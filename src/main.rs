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
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

mod args;
mod cert;
mod stream;

const DEFAULT_PORT: u16 = 24853;
const SAMPLE_RATE: u32 = 48000;
const CLIENT_WAIT_MS: u64 = 5000;
const MAX_OPUS_PACKAGE_SIZE: usize = 1500;

/// This returns the maximum possible sample size an opus packet can encode / decode
const fn opus_sample_buffer_size(channels: u16) -> usize {
    assert!(matches!(channels, 1 | 2));
    SAMPLE_RATE as usize * channels as usize * 6 / 100
}

/// This returns the next lower opus buffer size to encode, or none if to less samples are present
fn next_lower_opus_sample_count(len: usize, channels: u16) -> Option<usize> {
    const MAGIC_OPUS_VALUES: &[usize] = &[25, 50, 100, 200, 400, 600];
    MAGIC_OPUS_VALUES
        .into_iter()
        .map(|val| SAMPLE_RATE as usize * channels as usize * val / 10000)
        .filter(|val| val <= &len)
        .max()
}

#[tokio::main]
async fn main() -> anyhow::Result<Infallible> {
    match args::Args::parse().cmd {
        args::Command::Client {
            input,
            address,
            opus_mode,
            channels,
        } => client(input, address, opus_mode.into(), channels).await,
        args::Command::Server { output, bind } => server(output, bind).await,
    }
}

async fn client(
    input: Option<String>,
    address: SocketAddr,
    opus_mode: opus::Application,
    channels: Option<u16>,
) -> anyhow::Result<Infallible> {
    assert!(matches!(channels, None | Some(1 | 2)));

    let mut endpoint = Endpoint::client(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0))?;

    let mut client_config = ClientConfig::new(Arc::new(QuicClientConfig::try_from(
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(SkipServerVerification::new())
            .with_no_client_auth(),
    )?));
    let mut transport_config = TransportConfig::default();
    transport_config
        .max_idle_timeout(Some(Duration::from_secs(5).try_into().unwrap()))
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
            Err(err) = client_loop(&input, &connection, opus_mode, channels) => eprintln!("{:?}", err),
        };
        tokio::time::sleep(Duration::from_millis(CLIENT_WAIT_MS)).await;
    }
}

async fn client_loop(
    input: &Option<String>,
    connection: &Connection,
    opus_mode: opus::Application,
    channels: Option<u16>,
) -> anyhow::Result<Infallible> {
    let host = cpal::default_host();
    let device = match input {
        Some(name) => host.input_devices()?.map(|dev| dev).find(|dev| {
            dev.id().is_ok_and(|id| &id.1 == name)
                || dev.description().is_ok_and(|desc| desc.name() == name)
        }),
        None => host.default_input_device(),
    };
    let device = if let Some(device) = device {
        device
    } else {
        bail!("Input device not found");
    };
    println!("Using input device \"{}\"", device.description()?.name());
    let config = device
        .supported_input_configs()?
        .filter(|config| config.sample_format() == cpal::SampleFormat::F32)
        .filter_map(|config| config.try_with_sample_rate(SAMPLE_RATE))
        .filter(|config| matches!(config.channels(), 1 | 2))
        .filter(|config| channels.is_none_or(|ch| ch == config.channels()))
        .max_by(|a, b| a.channels().cmp(&b.channels()))
        .context("Input device does not support config")?
        .config();
    let channels = config.channels;

    let mut rb = ringbuf::LocalRb::new(opus_sample_buffer_size(channels));
    let (tx, mut rx) = tokio::sync::mpsc::channel(1);

    let stream: MyStream = device
        .build_input_stream(
            &config,
            move |mut data: &[f32], _| {
                while !data.is_empty() {
                    data = &data[rb.push_slice(&data)..];
                    if let Some(len) = next_lower_opus_sample_count(rb.occupied_len(), channels) {
                        let mut buffer = vec![0f32; len];
                        assert_eq!(rb.pop_slice(&mut buffer), len);
                        tx.blocking_send(buffer).unwrap();
                    }
                }
            },
            |err| eprintln!("cpal error: {:?}", err),
            None,
        )?
        .into();
    stream.play()?;

    let mut tx = connection.open_uni().await?;
    //  Todo: Write sample rate
    tx.write_i32(opus_mode as i32).await?;
    tx.write_u16(channels).await?;

    let mut encoder = opus::Encoder::new(
        SAMPLE_RATE,
        match channels {
            1 => opus::Channels::Mono,
            2 => opus::Channels::Stereo,
            _ => unreachable!(),
        },
        opus_mode,
    )?;
    encoder.set_bitrate(opus::Bitrate::Max)?;
    encoder.set_inband_fec(false)?;
    encoder.set_packet_loss_perc(10)?;
    encoder.set_complexity(10)?;
    encoder.set_vbr(true)?;
    encoder.set_dtx(false)?;

    let mut opus_buffer = [0u8; MAX_OPUS_PACKAGE_SIZE];

    loop {
        let sample_buffer = rx.recv().await.context("got no sample buffer")?;
        let len = encoder.encode_float(&sample_buffer, &mut opus_buffer)?;
        tx.write_u32(len as u32).await?;
        tx.write_all(&opus_buffer[..len]).await?;
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
            bind.unwrap_or(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::UNSPECIFIED),
                DEFAULT_PORT,
            )),
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
    let mut rx = connection.accept_uni().await?;

    //  TODO: Read bitrate

    let opus_mode = match rx.read_i32().await? {
        i if i == opus::Application::Voip as i32 => opus::Application::Voip,
        i if i == opus::Application::Audio as i32 => opus::Application::Audio,
        i if i == opus::Application::LowDelay as i32 => opus::Application::LowDelay,
        i => {
            rx.stop(1u8.into())?;
            bail!("Invalid opus mode: {}", i)
        }
    };
    println!("Opus Mode: {:?}", opus_mode);

    let channels = rx.read_u16().await?;
    if !matches!(channels, 1 | 2) {
        rx.stop(1u8.into())?;
        bail!("Invalid channel count: {}", channels);
    }
    println!("Channels: {}", channels);

    //  100ms buffer
    let rb = ringbuf::SharedRb::<ringbuf::storage::Heap<f32>>::new(
        SAMPLE_RATE as usize * channels as usize / 10,
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
                    if len >= 2 {
                        //  Fill rest of the buffer with last known value
                        let last_samples: [f32; 2] = data[len - 2..len].try_into().unwrap();
                        data[len..]
                            .chunks_exact_mut(2)
                            .for_each(|ch| ch.copy_from_slice(&last_samples));
                    }
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

    let mut sample_buffer = vec![0f32; opus_sample_buffer_size(channels)];
    let mut opus_buffer = [0u8; MAX_OPUS_PACKAGE_SIZE];

    loop {
        let len = rx.read_u32().await? as usize;
        if len > MAX_OPUS_PACKAGE_SIZE {
            rx.stop(1u8.into())?;
            bail!("Got invalid frame size: {}", len);
        }
        rx.read_exact(&mut opus_buffer[..len]).await?;
        let len = decoder.decode_float(&opus_buffer[..len], &mut sample_buffer, false)?;
        rtx.push_slice(&sample_buffer[..len * channels as usize]);
        //println!("Pushed {} samples", len * channels as usize);
    }
}
