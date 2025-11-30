use crate::args::{Cli, Command};
use anyhow::{Context, bail};
use clap::Parser;
use cpal::{
    Device, InputCallbackInfo, Sample, SampleRate, StreamConfig, SupportedStreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use futures_util::{pin_mut, stream::StreamExt};
use iroh::{
    Endpoint, EndpointAddr,
    discovery::{
        UserData,
        mdns::{DiscoveryEvent, MdnsDiscovery},
    },
    endpoint::Incoming,
};
use rb::{RB, RbConsumer, RbProducer};
use std::str::FromStr;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter};

mod args;

const USER_DATA_NAME_PREFIX: &str = "do/music/";
const ALPN: &[u8] = b"do/music";
const SERVICE_NAME: &str = "do-music";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Cli::parse();
    match args.cmd {
        Command::List => {
            let host = cpal::default_host();
            let inp = host
                .input_devices()
                .into_iter()
                .flat_map(|dev| dev.filter_map(|dev| dev.name().ok()))
                .collect::<Vec<_>>();
            let out = host
                .output_devices()
                .into_iter()
                .flat_map(|dev| dev.filter_map(|dev| dev.name().ok()))
                .collect::<Vec<_>>();
            if !inp.is_empty() {
                println!("Input devices:");
                inp.iter().for_each(|name| println!("  {name}"));
                if let Some(name) = host.default_input_device().and_then(|dev| dev.name().ok()) {
                    println!("Default input device:");
                    println!("  {name}");
                }
                if !out.is_empty() {
                    println!();
                }
            }
            if !out.is_empty() {
                println!("Output devices:");
                out.iter().for_each(|name| println!("  {name}"));
                if let Some(name) = host.default_output_device().and_then(|dev| dev.name().ok()) {
                    println!("Default output device:");
                    println!("  {name}");
                }
            }
            let mut nl = false;
            let endpoint = Endpoint::empty_builder(iroh::RelayMode::Disabled)
                .bind()
                .await?;
            let discovery = MdnsDiscovery::builder()
                .advertise(false)
                .service_name(SERVICE_NAME)
                .build(endpoint.id())?;
            let stream = discovery.subscribe().await;
            pin_mut!(stream);

            while let Some(discovered) = stream.next().await {
                let name = match discovered {
                    DiscoveryEvent::Discovered {
                        endpoint_info: info,
                        ..
                    } if info
                        .user_data()
                        .is_some_and(|ud| ud.to_string().starts_with(USER_DATA_NAME_PREFIX)) =>
                    {
                        &info.user_data().unwrap().to_string()[USER_DATA_NAME_PREFIX.len()..]
                    }

                    _ => continue,
                };
                if !nl {
                    nl = true;
                    println!();
                }
                println!("Found \"{name}\"");
            }

            Ok(())
        }
        Command::Client {
            name,
            input,
            bitrate,
        } => client(&name, input.as_deref(), bitrate).await,
        Command::Server { name, output } => server(&name, output).await,
    }
}

async fn client(name: &str, input: Option<&str>, bitrate: Option<u32>) -> anyhow::Result<()> {
    let name = format!("{}{}", USER_DATA_NAME_PREFIX, name);
    let endpoint = Endpoint::empty_builder(iroh::RelayMode::Disabled)
        .bind()
        .await?;
    let discovery = MdnsDiscovery::builder()
        .advertise(false)
        .service_name(SERVICE_NAME)
        .build(endpoint.id())?;

    loop {
        let stream = discovery.subscribe().await.filter_map(|event| {
            let res = match event {
                DiscoveryEvent::Discovered { endpoint_info, .. }
                    if endpoint_info
                        .user_data()
                        .is_some_and(|ud| ud.to_string() == name) =>
                {
                    Some(endpoint_info.into_endpoint_addr())
                }
                _ => None,
            };
            async move { res }
        });
        futures_util::pin_mut!(stream);
        if let Some(addr) = stream.next().await {
            match connect_to_server(&endpoint, addr, input, bitrate).await {
                Ok(()) => unreachable!(),
                Err(err) => eprintln!("{err}"),
            }
        }
    }
}

fn get_cpal_input_stuff(
    name: Option<&str>,
    bitrate: Option<u32>,
) -> anyhow::Result<(Device, SupportedStreamConfig)> {
    let host = cpal::default_host();
    let dev = match name {
        Some(name) => host
            .input_devices()?
            .find(|dev| dev.name().is_ok_and(|dev_name| dev_name == name)),
        None => host.default_input_device(),
    };
    let dev = match dev {
        None if name.is_some() => bail!("Input device {} not found", name.unwrap()),
        None => bail!("Default input device not found"),
        Some(dev) => dev,
    };
    let config = dev
        .supported_input_configs()?
        .filter_map(|conf| match bitrate {
            Some(bitrate) => conf.try_with_sample_rate(SampleRate(bitrate)),
            None => Some(conf.with_max_sample_rate()),
        })
        .next()
        .context("No valid config for input device")?;

    Ok((dev, config))
}

async fn connect_to_server(
    endpoint: &Endpoint,
    server: EndpointAddr,
    input: Option<&str>,
    bitrate: Option<u32>,
) -> anyhow::Result<()> {
    let (device, config) = get_cpal_input_stuff(input, bitrate)?;
    let connection = endpoint.connect(server, ALPN).await?;
    let mut send = connection.open_uni().await?;
    send.write_u32(config.sample_rate().0).await?;
    send.write_u16(config.channels()).await?;
    let mut send = BufWriter::new(send);
    println!(
        "Connected to server:\n  Sample rate: {}\n  Channels: {}",
        config.sample_rate().0,
        config.channels()
    );
    let rb = rb::SpscRb::new(config.sample_rate().0 as usize);
    let (tx, rx) = (rb.producer(), rb.consumer());
    let stream = device.build_input_stream(
        &config.config(),
        move |data: &[f32], _info: &InputCallbackInfo| {
            _ = tx.write(data);
        },
        move |err| eprintln!("{err}"),
        None,
    )?;
    stream.play()?;

    let mut buffer = vec![0f32; config.sample_rate().0 as usize];
    let mut counter: u32 = 0;
    loop {
        let len = rx.read_blocking(&mut buffer).unwrap();
        let buffer = &buffer[..len];
        if buffer.iter().all(|v| v == &f32::EQUILIBRIUM) {
            counter += len as u32;
            //  Allow 1s silence, then stop sending stream
            if counter >= config.sample_rate().0 {
                send.write_f32(f32::EQUILIBRIUM).await?;
                continue;
            }
        } else {
            counter = 0;
        }
        for val in buffer {
            send.write_f32(*val).await?;
        }
    }
}

async fn server(name: &str, output: Option<String>) -> anyhow::Result<()> {
    let endpoint = Endpoint::empty_builder(iroh::RelayMode::Disabled)
        .alpns(vec![ALPN.to_vec()])
        .user_data_for_discovery(
            UserData::from_str(&format!("{USER_DATA_NAME_PREFIX}{name}")).unwrap(),
        )
        .bind()
        .await?;
    let discovery = MdnsDiscovery::builder()
        .advertise(true)
        .service_name(SERVICE_NAME)
        .build(endpoint.id())?;
    endpoint.discovery().add(discovery);

    println!("Ready");

    while let Some(accept) = endpoint.accept().await {
        let output = output.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_connection(accept, output.as_deref()).await {
                eprintln!("{}", err);
            }
        });
    }

    Ok(())
}

fn get_output_device(
    rx: rb::Consumer<f32>,
    name: Option<&str>,
    sample_rate: SampleRate,
    channels: u16,
) -> anyhow::Result<SendStream> {
    let host = cpal::default_host();
    let dev = match name {
        Some(name) => host
            .output_devices()?
            .find(|dev| dev.name().is_ok_and(|dev_name| dev_name == name)),
        None => host.default_output_device(),
    };
    let dev = match dev {
        None if name.is_some() => bail!("Output device {} not found", name.unwrap()),
        None => bail!("Default output device not found"),
        Some(dev) => dev,
    };
    let config = StreamConfig {
        buffer_size: cpal::BufferSize::Default,
        channels,
        sample_rate,
    };
    Ok(SendStream(dev.build_output_stream(
        &config,
        move |data: &mut [f32], _| {
            rx.read_blocking(data);
        },
        |err| {
            eprintln!("{err}");
        },
        None,
    )?))
}

async fn handle_connection(accept: Incoming, output: Option<&str>) -> anyhow::Result<()> {
    let conn = accept.accept()?.await?;
    let mut read = conn.accept_uni().await?;
    let sample_rate = read.read_u32().await?;
    let channels = read.read_u16().await?;
    let mut read = BufReader::new(read);
    println!(
        "Got client:\n  Sample Rate: {}\n  Channels: {}",
        sample_rate, channels
    );

    let rb = rb::SpscRb::new(sample_rate as usize);
    let (tx, rx) = (rb.producer(), rb.consumer());

    let stream = get_output_device(rx, output, SampleRate(sample_rate), channels)?;
    stream.0.play()?;

    loop {
        let val = read.read_f32().await?;
        _ = tx.write(&[val]);
    }
}

struct SendStream(cpal::Stream);
// they see me Send-ing, they hatin'
unsafe impl Send for SendStream {}
