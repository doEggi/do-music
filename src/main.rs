use crate::args::{Cli, Command};
use anyhow::Context;
use clap::Parser;
use cpal::{
    SampleFormat, SampleRate,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use futures_util::StreamExt;
use iroh::{
    Endpoint, EndpointAddr,
    discovery::{
        EndpointInfo, UserData,
        mdns::{DiscoveryEvent, MdnsDiscovery},
    },
    endpoint::Connection,
};
use rb::{RB, RbConsumer, RbProducer};
use std::{convert::Infallible, str::FromStr, sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

mod args;

const ALPN: &[u8] = b"do/music";
const SAMPLE_RATE: u32 = 48_000;
const MAX_OPUS_PACKET_SIZE: usize = 1500;
const OPUS_MS: usize = 5;

#[tokio::main]
async fn main() -> anyhow::Result<Infallible> {
    let args = Cli::parse();
    match args.cmd {
        Command::Client { name, input } => client(name, input).await,
        Command::Server { name, output } => server(name, output).await,
    }
}

async fn server(name: String, device: Option<String>) -> anyhow::Result<Infallible> {
    let endpoint = Endpoint::empty_builder(iroh::RelayMode::Disabled)
        .alpns(vec![ALPN.to_vec()])
        .user_data_for_discovery(UserData::from_str(&name)?)
        .bind()
        .await?;
    endpoint.discovery().add(
        MdnsDiscovery::builder()
            .advertise(true)
            .build(endpoint.id())?,
    );
    loop {
        let connection = match endpoint.accept().await {
            Some(val) => val,
            None => continue,
        }
        .await?;
        let device = device.clone();
        tokio::spawn(async move {
            let Err(err) = handle_connection(connection, device).await;
            eprintln!("Error connecting to client:\n  {}", err);
        });
    }
}

async fn handle_connection(
    connection: Connection,
    device: Option<String>,
) -> anyhow::Result<Infallible> {
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
            Some(dev_name) => host
                .output_devices()?
                .find(|dev| dev.name().is_ok_and(|name| name == dev_name)),
            None => host.default_output_device(),
        }
        .context("Output device not found")?
    };
    let config = device
        .supported_output_configs()?
        .filter(|conf| conf.channels() == channels && conf.sample_format() == SampleFormat::F32)
        .filter_map(|conf| conf.try_with_sample_rate(SampleRate(SAMPLE_RATE)))
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

async fn client(name: String, device: Option<String>) -> anyhow::Result<Infallible> {
    let endpoint = Endpoint::empty_builder(iroh::RelayMode::Disabled)
        .bind()
        .await?;

    loop {
        let Err(err) = client_loop(&name, device.as_deref(), &endpoint).await;
        eprintln!("Error connecting to server:\n  {}", err);
    }
}

async fn search_device(name: &str, endpoint: &Endpoint) -> Option<EndpointAddr> {
    let discovery = MdnsDiscovery::builder()
        .advertise(false)
        .build(endpoint.id())
        .ok()?;
    let mut stream = discovery.subscribe().await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match stream.next().await {
                Some(DiscoveryEvent::Discovered {
                    endpoint_info: EndpointInfo { data, endpoint_id },
                    ..
                }) if data
                    .user_data()
                    .is_some_and(|data| data.to_string() == name) =>
                {
                    return EndpointAddr::from_parts(endpoint_id, data.addrs().cloned());
                }
                _ => continue,
            }
        }
    })
    .await
    .ok()
}

async fn client_loop(
    name: &str,
    device: Option<&str>,
    endpoint: &Endpoint,
) -> anyhow::Result<Infallible> {
    let server_id = loop {
        if let Some(server_id) = search_device(name, endpoint).await {
            break server_id;
        }
    };
    let connection = endpoint.connect(server_id, ALPN).await?;
    let device = {
        let host = cpal::default_host();
        match device {
            Some(dev_name) => host
                .input_devices()?
                .find(|dev| dev.name().is_ok_and(|name| &name == dev_name)),
            None => host.default_input_device(),
        }
        .context("Input device not found")?
    };
    let config = device
        .supported_input_configs()?
        .filter(|conf| {
            matches!(conf.channels(), 1 | 2) && conf.sample_format() == SampleFormat::F32
        })
        .filter_map(|conf| conf.try_with_sample_rate(SampleRate(SAMPLE_RATE)))
        .max_by(|a, b| a.channels().cmp(&b.channels()))
        .context("Input device does not support config")?;
    let mut tx = connection.open_uni().await?;
    tx.write_u8(config.channels() as u8).await?;
    tx.finish()?;
    //  Buffer for 100ms
    let rb = Arc::new(rb::SpscRb::new(
        SAMPLE_RATE as usize * config.channels() as usize * 100 / 1_000,
    ));
    let rb_ = rb.clone();
    let (rtx, rrx) = (rb.producer(), rb.consumer());
    let stream = SendStream(device.build_input_stream(
        &config.config(),
        move |mut data: &[f32], _| {
            while data.len() > 0 {
                let len = rtx.write_blocking(data).unwrap_or_default();
                data = &data[len..];
            }
        },
        move |err| {
            eprintln!("Error reading input device:\n  {}", err);
            rb_.clear();
        },
        None,
    )?);
    stream.0.play()?;
    let mut enc = opus::Encoder::new(
        SAMPLE_RATE,
        opus::Channels::Stereo,
        opus::Application::Audio,
    )?;
    enc.set_bitrate(opus::Bitrate::Max)?;
    enc.set_inband_fec(true)?;
    enc.set_packet_loss_perc(10)?;
    enc.set_complexity(10)?;
    enc.set_dtx(false)?;

    let mut sample_buffer =
        vec![0f32; SAMPLE_RATE as usize * config.channels() as usize * OPUS_MS / 1000];
    let mut seq = 1u64;

    loop {
        let mut buffer = &mut sample_buffer[..];
        while buffer.len() > 0 {
            let len = rrx.read_blocking(&mut buffer).unwrap_or_default();
            buffer = &mut buffer[len..];
        }
        let mut samples_buffer = enc.encode_vec_float(&sample_buffer, MAX_OPUS_PACKET_SIZE)?;
        let mut buffer = seq.to_be_bytes().to_vec();
        buffer.append(&mut samples_buffer);
        connection.send_datagram(buffer.into())?;
        seq += 1;
    }
}

struct SendStream(cpal::Stream);
// they see me Send-ing, they hatin'
unsafe impl Send for SendStream {}
