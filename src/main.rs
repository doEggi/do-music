use crate::{cert::SkipServerVerification, stream::MyStream};
use anyhow::{Context, bail};
use clap::Parser;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use quinn::{
    ClientConfig, Connection, Endpoint, Incoming, ServerConfig, TransportConfig,
    crypto::rustls::QuicClientConfig,
    rustls::{
        self,
        pki_types::{CertificateDer, PrivatePkcs8KeyDer},
    },
};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use std::{
    convert::Infallible,
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
};

mod args;
mod cert;
mod stream;

const DEFAULT_PORT: u16 = 24853;
const SAMPLE_RATE: u32 = 48000;
const CLIENT_WAIT_MS: u64 = 5000;
const MAX_OPUS_PACKAGE_SIZE: usize = 1500;

fn opus_sample_buffer_size(opus_ms: f32, channels: u16) -> usize {
    match opus_ms {
        2.5 => channels as usize * SAMPLE_RATE as usize * 25 / 10000,
        5. => channels as usize * SAMPLE_RATE as usize * 5 / 1000,
        10. => channels as usize * SAMPLE_RATE as usize / 100,
        20. => channels as usize * SAMPLE_RATE as usize / 50,
        40. => channels as usize * SAMPLE_RATE as usize / 25,
        60. => channels as usize * SAMPLE_RATE as usize * 6 / 100,
        _ => panic!("Invalid OPUS_MS value: {}", opus_ms),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<Infallible> {
    match args::Args::parse().cmd {
        args::Command::Client {
            input,
            address,
            low_latency,
            opus_ms,
        } => client(input, address, low_latency, opus_ms).await,
        args::Command::Server { output, bind } => server(output, bind).await,
    }
}

async fn client(
    input: Option<String>,
    address: SocketAddr,
    low_latency: bool,
    opus_ms: f32,
) -> anyhow::Result<Infallible> {
    let mut endpoint = Endpoint::client(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))?;

    let mut client_config = ClientConfig::new(Arc::new(QuicClientConfig::try_from(
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(SkipServerVerification::new())
            .with_no_client_auth(),
    )?));
    let mut transport_config = TransportConfig::default();
    transport_config
        .max_idle_timeout(Some(Duration::from_secs(5).try_into().unwrap())) // Max. 30s ohne Aktivität
        .keep_alive_interval(Some(Duration::from_secs(3)));
    client_config.transport_config(Arc::new(transport_config));
    endpoint.set_default_client_config(client_config);

    loop {
        let connection = endpoint.connect(address, "do-music")?.await?;
        tokio::select! {
            res = connection.read_datagram() => match res {
                Ok(_) => eprintln!("Got datagram from server (this is bad)"),
                Err(err) => eprintln!("{:?}", err),
            },
            Err(err) = client_loop(&input, &connection, low_latency, opus_ms) => eprintln!("{:?}", err),
        };
        tokio::time::sleep(Duration::from_millis(CLIENT_WAIT_MS)).await;
    }
}

async fn client_loop(
    input: &Option<String>,
    connection: &Connection,
    low_latency: bool,
    opus_ms: f32,
) -> anyhow::Result<Infallible> {
    let (signal_tx, mut signal) = mpsc::channel(1);
    let (stream, channels): (MyStream, _) = {
        let host = cpal::default_host();
        let device = match input {
            Some(name) => host
                .input_devices()?
                .map(|dev| {
                    _ = dbg!(dev.id());
                    dev
                })
                .find(|dev| {
                    dev.id().is_ok_and(|id| &id.1 == name)
                        || dev.description().is_ok_and(|desc| desc.name() == name)
                }),
            None => host.default_input_device(),
        };
        if device.is_none() {
            bail!("Input device not found");
        }
        let device = device.unwrap();
        println!("Using input device \"{}\"", device.description()?.name());
        let config = device
            .supported_input_configs()?
            .filter(|config| config.sample_format() == cpal::SampleFormat::F32)
            .filter_map(|config| config.try_with_sample_rate(SAMPLE_RATE))
            .next()
            .context("Input device does not support config")?
            .config();
        let channels = config.channels;
        if !matches!(channels, 1 | 2) {
            bail!("Invalid channel count");
        }

        let mut rb = ringbuf::LocalRb::new(opus_sample_buffer_size(opus_ms, channels));

        (
            device
                .build_input_stream(
                    &config,
                    move |mut data: &[f32], _| {
                        while !data.is_empty() {
                            data = &data[rb.push_slice(&data)..];
                            if rb.is_full() {
                                let mut buffer =
                                    vec![0f32; opus_sample_buffer_size(opus_ms, channels)];
                                rb.pop_slice(&mut buffer);
                                signal_tx.blocking_send(buffer).unwrap();
                            }
                        }
                    },
                    |err| eprintln!("cpal error: {:?}", err),
                    None,
                )?
                .into(),
            channels,
        )
    };
    stream.play()?;

    {
        let mut tx = connection.open_uni().await?;
        tx.write_u16(channels).await?;
        tx.finish()?;
    }

    let mut encoder = opus::Encoder::new(
        SAMPLE_RATE,
        match channels {
            1 => opus::Channels::Mono,
            2 => opus::Channels::Stereo,
            _ => unreachable!(),
        },
        match low_latency {
            true => opus::Application::LowDelay,
            false => opus::Application::Audio,
        },
    )?;
    encoder.set_bitrate(opus::Bitrate::Max)?;
    encoder.set_inband_fec(false)?;
    encoder.set_packet_loss_perc(10)?;
    encoder.set_complexity(10)?;
    encoder.set_dtx(false)?;

    let mut opus_buffer = [0u8; MAX_OPUS_PACKAGE_SIZE + 8];
    let mut seq = 0u64;

    loop {
        let sample_buffer = signal.recv().await.context("got no sample buffer")?;
        opus_buffer[0..8].copy_from_slice(&seq.to_be_bytes());
        let len = encoder.encode_float(&sample_buffer, &mut opus_buffer[8..])?;
        connection.send_datagram(opus_buffer[0..len + 8].to_vec().into())?;
        seq += 1;
    }
}

async fn server(output: Option<String>, bind: Option<SocketAddr>) -> anyhow::Result<Infallible> {
    let endpoint = {
        let cert = rcgen::generate_simple_self_signed(vec!["do-music".into()]).unwrap();
        let cert_der = CertificateDer::from(cert.cert);
        let priv_key = PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der());

        let mut server_config =
            ServerConfig::with_single_cert(vec![cert_der.clone()], priv_key.into())?;
        let transport_config = Arc::get_mut(&mut server_config.transport).unwrap();
        transport_config.max_concurrent_bidi_streams(0_u8.into());
        transport_config.max_concurrent_uni_streams(1_u8.into());
        transport_config
            .max_idle_timeout(Some(Duration::from_secs(5).try_into().unwrap())) // Max. 30s ohne Aktivität
            .keep_alive_interval(Some(Duration::from_secs(3)));

        Endpoint::server(
            server_config,
            bind.unwrap_or(SocketAddr::V4(SocketAddrV4::new(
                Ipv4Addr::UNSPECIFIED,
                DEFAULT_PORT,
            ))),
        )?
    };

    let output = Arc::new(output);

    loop {
        if let Some(incoming) = endpoint.accept().await {
            let output = output.clone();
            tokio::spawn(async move {
                let Err(err) = handle_connection(incoming, &output).await;
                eprintln!("{:?}", err);
            });
        }
    }
}

async fn handle_connection(
    incoming: Incoming,
    output: &Option<String>,
) -> anyhow::Result<Infallible> {
    let connection = incoming.await?;
    println!("Got connection from {}", connection.remote_address());
    let channels = {
        let mut rx = connection.accept_uni().await?;
        let channels = rx.read_u16().await?;
        if !matches!(channels, 1 | 2) {
            rx.stop(1u8.into())?;
            bail!("Invalid channel count");
        }
        rx.stop(0u8.into())?;
        channels
    };
    println!("Channels: {}", channels);

    //  100ms buffer
    let rb = ringbuf::SharedRb::<ringbuf::storage::Heap<f32>>::new(
        channels as usize * SAMPLE_RATE as usize / 10,
    );
    let (mut rtx, mut rrx) = rb.split();

    let stream: MyStream = {
        let host = cpal::default_host();
        let device = match output {
            Some(name) => host.output_devices()?.find(|dev| {
                dev.id().is_ok_and(|id| &id.1 == name)
                    || dev.description().is_ok_and(|desc| desc.name() == name)
            }),
            None => host.default_output_device(),
        };
        if device.is_none() {
            eprintln!("Output device not found");
        }
        let device = device.unwrap();
        println!("Using output device \"{}\"", device.description()?.name());
        let config = device
            .supported_output_configs()?
            .filter(|config| {
                config.channels() == channels && config.sample_format() == cpal::SampleFormat::F32
            })
            .filter_map(|config| config.try_with_sample_rate(SAMPLE_RATE))
            .next()
            .context("Output device does not support config")?
            .config();
        device
            .build_output_stream(
                &config,
                move |data: &mut [f32], _| {
                    let len = rrx.pop_slice(data);
                    //println!("{len}/{}", data.len());
                    data[len..].fill(cpal::Sample::EQUILIBRIUM);
                },
                |err| eprintln!("cpal error: {:?}", err),
                None,
            )?
            .into()
    };
    stream.play()?;
    let mut decoder = opus::Decoder::new(
        SAMPLE_RATE,
        match channels {
            1 => opus::Channels::Mono,
            2 => opus::Channels::Stereo,
            _ => unreachable!(),
        },
    )?;

    let mut sample_buffer = vec![0f32; opus_sample_buffer_size(60., channels)];
    let mut seq: Option<u64> = None;

    loop {
        let datagram = connection.read_datagram().await?;
        if datagram.len() <= 8 {
            bail!("Invalid package received");
        }
        let new_seq = u64::from_be_bytes(datagram[0..8].try_into().unwrap());
        if let Some(diff) = seq.as_ref().map(|seq| new_seq - seq)
            && diff > 1
        {
            for i in 1..diff {
                println!("lost package {}", seq.unwrap() + i);
                let len = decoder.decode_float(&[], &mut sample_buffer, false)?;
                rtx.push_slice(&sample_buffer[..len * channels as usize]);
            }
        }
        let len = decoder.decode_float(&datagram[8..], &mut sample_buffer, false)?;
        rtx.push_slice(&sample_buffer[..len * channels as usize]);
        seq = Some(new_seq);
    }
}
