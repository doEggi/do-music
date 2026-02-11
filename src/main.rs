use crate::args::{Cli, Command};
use anyhow::Context;
use clap::Parser;
use cpal::{
    SampleFormat,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use iroh::{Endpoint, EndpointAddr, Watcher, endpoint::Connection};
use rb::{RB, RbConsumer, RbProducer};
use std::{
    convert::Infallible,
    net::{Ipv4Addr, SocketAddrV4},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UdpSocket,
    sync::mpsc::unbounded_channel,
};

mod args;

const ALPN: &[u8] = b"do/music";
const SAMPLE_RATE: u32 = 48_000;
const MAX_OPUS_PACKET_SIZE: usize = 1500;
const OPUS_MS: usize = 5;
const MDNS_PORT: u16 = 5369;
const MDNS_MESSAGE: &[u8] = b"where speaker?";

#[tokio::main]
async fn main() -> anyhow::Result<Infallible> {
    let args = Cli::parse();
    match args.cmd {
        Command::Client {
            name,
            input,
            low_latency,
        } => client(name, input, low_latency).await,
        Command::Server { name, output } => server(name, output).await,
    }
}

async fn server(name: String, device: Option<String>) -> anyhow::Result<Infallible> {
    let endpoint: Endpoint = Endpoint::empty_builder(iroh::RelayMode::Disabled)
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await?;
    let addr = endpoint.watch_addr();
    tokio::spawn(async move {
        let Err(err) = publish_name(&name, addr).await;
        eprintln!("Error publishing device {:?}", err);
    });
    println!("Server ready: {}", endpoint.id().fmt_short());
    loop {
        let connection = match endpoint.accept().await {
            Some(val) => val,
            None => continue,
        }
        .await?;
        let device = device.clone();
        tokio::spawn(async move {
            let Err(err) = handle_connection(&connection, device).await;
            eprintln!(
                "No connection to client {}:\n  {}",
                connection.remote_id().fmt_short(),
                err
            );
        });
    }
}

async fn publish_name(
    name: &str,
    mut addr: impl Watcher<Value = EndpointAddr>,
) -> anyhow::Result<Infallible> {
    let udp = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, MDNS_PORT)).await?;
    let mut buf = Vec::with_capacity(4096);
    loop {
        buf.clear();
        let (_, client_addr) = udp.recv_buf_from(&mut buf).await?;
        println!("Got udp pkg from {}", client_addr.ip());
        if &buf == MDNS_MESSAGE {
            let value = (addr.get(), name.to_string());
            udp.send_to(&postcard::to_stdvec(&value)?, client_addr)
                .await?;
            println!("Sent addr info to {}", client_addr.ip());
        }
    }
}

async fn handle_connection(
    connection: &Connection,
    device: Option<String>,
) -> anyhow::Result<Infallible> {
    println!("Got connection from {}", connection.remote_id().fmt_short());
    let mut rx = connection.accept_uni().await?;
    let channels = match rx.read_u8().await? {
        1 => 1u16,
        2 => 2u16,
        channels => {
            return Err(anyhow::Error::msg(format!(
                "Invalid channel count: {}",
                channels
            )));
        }
    };
    drop(rx);
    let device = {
        let host = cpal::default_host();
        match device {
            Some(name) => host
                .output_devices()?
                .find(|dev| dev.name().is_ok_and(|name_| name_ == name)),
            None => host.default_output_device(),
        }
        .context("Output device not found")?
    };
    let config = device
        .supported_output_configs()?
        .filter(|conf| conf.channels() == channels && conf.sample_format() == SampleFormat::F32)
        .filter_map(|conf| conf.try_with_sample_rate(cpal::SampleRate(SAMPLE_RATE)))
        .next()
        .context("Output device does not support config")?;
    //  Buffer for 100ms
    let rb = Arc::new(rb::SpscRb::new(
        SAMPLE_RATE as usize * channels as usize * 100 / 1_000,
    ));
    let rb_ = rb.clone();
    let (rtx, rrx) = (rb.producer(), rb.consumer());
    let stream = SendStream(device.build_output_stream(
        &config.config(),
        move |mut data: &mut [f32], _| {
            data.fill(0.);
            while data.len() > 0 {
                let len = rrx.read_blocking(&mut data).unwrap_or_default();
                data = &mut data[len..];
            }
        },
        move |err| {
            eprintln!("Error writing to output device:\n  {}", err);
            rb_.clear();
        },
        None,
    )?);
    stream.0.play()?;
    let mut dec = opus::Decoder::new(
        SAMPLE_RATE,
        match channels {
            1 => opus::Channels::Mono,
            2 => opus::Channels::Stereo,
            _ => unreachable!(),
        },
    )?;

    //let mut recv_buffer = [0u8; MAX_OPUS_PACKET_SIZE];
    let mut sample_buffer = vec![0f32; SAMPLE_RATE as usize * channels as usize * OPUS_MS / 1000];
    let mut seq: Option<u64> = None;

    loop {
        let recv_buffer = connection.read_datagram().await?;
        let new_seq = u64::from_be_bytes(recv_buffer[0..8].try_into().unwrap());
        if seq.is_some_and(|seq| seq >= new_seq) {
            //  We alraydy skipped this package
            continue;
        }
        while seq.is_none_or(|seq| seq + 1 < new_seq) {
            dec.decode_float(&[], &mut sample_buffer, true)?;
            _ = rtx.write(&sample_buffer);
            seq = Some(seq.unwrap_or_default() + 1);
        }
        dec.decode_float(&recv_buffer[8..], &mut sample_buffer, false)?;
        let len = rtx.write(&sample_buffer).unwrap_or_default();
        if len != sample_buffer.len() {
            rb.clear();
        }
        seq = Some(new_seq);
    }
}

async fn client(
    name: String,
    device: Option<String>,
    low_latency: bool,
) -> anyhow::Result<Infallible> {
    let endpoint = Endpoint::empty_builder(iroh::RelayMode::Disabled)
        .bind()
        .await?;
    println!("Client ready: {}", endpoint.id().fmt_short());
    loop {
        let Err(err) = client_loop(&name, device.as_deref(), &endpoint, low_latency).await;
        eprintln!("Connection error:\n  {}", err);
    }
}

async fn search_device(name: &str) -> Option<EndpointAddr> {
    tokio::time::timeout(Duration::from_secs(10), async {
        let udp = UdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, MDNS_PORT)).await?;
        udp.set_broadcast(true)?;
        let mut buf = Vec::with_capacity(4096);
        udp.send_to(
            MDNS_MESSAGE,
            SocketAddrV4::new(Ipv4Addr::BROADCAST, MDNS_PORT),
        )
        .await?;
        println!("Sent broadcast for server");
        loop {
            buf.clear();
            udp.recv_buf(&mut buf).await?;
            match postcard::from_bytes::<(EndpointAddr, String)>(&buf) {
                Ok((addr, name_)) => {
                    if name == name_ {
                        return Ok(addr);
                    } else {
                        println!("Found another server: {}", name_);
                    }
                }
                Err(err) => eprintln!("Got malformed response: {:?}", err),
            }
        }
    })
    .await
    .ok()
    .map(|res: anyhow::Result<EndpointAddr>| res.ok())
    .flatten()
}

async fn client_loop(
    name: &str,
    device: Option<&str>,
    endpoint: &Endpoint,
    low_latency: bool,
) -> anyhow::Result<Infallible> {
    let server_id = loop {
        if let Some(server_id) = search_device(name).await {
            break server_id;
        }
    };
    println!("Got server at {}", server_id.ip_addrs().next().unwrap());
    let connection = endpoint.connect(server_id, ALPN).await?;
    println!("Connected to server {}", connection.remote_id().fmt_short());
    let device = {
        let host = cpal::default_host();
        match device {
            Some(name) => host
                .input_devices()?
                .find(|dev| dev.name().is_ok_and(|name_| name_ == name)),
            None => host.default_input_device(),
        }
        .context("Input device not found")?
    };
    let config = device
        .supported_input_configs()?
        .filter(|conf| {
            matches!(conf.channels(), 1 | 2) && conf.sample_format() == SampleFormat::F32
        })
        .filter_map(|conf| conf.try_with_sample_rate(cpal::SampleRate(SAMPLE_RATE)))
        .max_by(|a, b| a.channels().cmp(&b.channels()))
        .context("Input device does not support config")?;
    let mut tx = connection.open_uni().await?;
    tx.write_u8(config.channels() as u8).await?;
    tx.finish()?;
    let rb = rb::SpscRb::new(SAMPLE_RATE as usize * config.channels() as usize * OPUS_MS / 1000);
    let (rtx, rrx) = (rb.producer(), rb.consumer());
    let mut rbuf = vec![0f32; SAMPLE_RATE as usize * config.channels() as usize * OPUS_MS / 1000]
        .into_boxed_slice();
    let (tx, mut rx) = unbounded_channel();
    let stream = SendStream(device.build_input_stream(
        &config.config(),
        move |mut data: &[f32], _| {
            while data.len() > 0 {
                match rtx.write(&data) {
                    Ok(len) => data = &data[len..],
                    Err(rb::RbError::Full) => {
                        assert_eq!(
                            rrx.read(&mut rbuf).unwrap(),
                            SAMPLE_RATE as usize * config.channels() as usize * OPUS_MS / 1000
                        );
                        _ = tx.send(rbuf.clone());
                    }
                    _ => unreachable!(),
                }
            }
        },
        move |err| {
            eprintln!("Error reading input device:\n  {}", err);
        },
        None,
    )?);
    stream.0.play()?;
    let mut enc = opus::Encoder::new(
        SAMPLE_RATE,
        opus::Channels::Stereo,
        if low_latency {
            opus::Application::LowDelay
        } else {
            opus::Application::Audio
        },
    )?;
    enc.set_bitrate(opus::Bitrate::Max)?;
    enc.set_inband_fec(true)?;
    enc.set_packet_loss_perc(10)?;
    enc.set_complexity(10)?;
    enc.set_dtx(false)?;

    let mut opus_buffer: [u8; MAX_OPUS_PACKET_SIZE + size_of::<u64>()] =
        [0u8; MAX_OPUS_PACKET_SIZE + size_of::<u64>()];
    let mut seq = 1u64;

    loop {
        let buf = rx.recv().await.context("Stream crashed")?;
        seq.to_be_bytes()
            .into_iter()
            .enumerate()
            .for_each(|(i, b)| opus_buffer[i] = b);

        let len = enc.encode_float(&buf, &mut opus_buffer[size_of::<u64>()..])?;
        connection.send_datagram(opus_buffer[..len + size_of::<u64>()].to_vec().into())?;
        seq += 1;
    }
}

struct SendStream(cpal::Stream);
// they see me Send-ing, they hatin'
unsafe impl Send for SendStream {}
