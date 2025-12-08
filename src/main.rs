use std::{convert::Infallible, time::Duration};

use crate::args::{Cli, Command};
use anyhow::bail;
use clap::Parser;
use cpal::{
    SampleRate,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use rb::{RB, RbConsumer, RbProducer};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

mod args;

const PORT: u16 = 7350;
const SAMPLE_RATE: usize = 48_000;
const MAX_OPUS_PACKET_SIZE: usize = 1500;
const BITRATE: i32 = 128_000;

#[tokio::main]
async fn main() -> anyhow::Result<Infallible> {
    let args = Cli::parse();
    match args.cmd {
        Command::Client { address, input } => client_loop(&address, input.as_deref()).await,
        Command::Server { address, output } => {
            server(
                address
                    .as_deref()
                    .unwrap_or(const_format::formatcp!("0.0.0.0:{}", PORT)),
                output,
            )
            .await
        }
    }
}

async fn client_loop(address: &str, input: Option<&str>) -> ! {
    loop {
        let Err(err) = client(address, input).await;
        eprintln!("{:?}", err);
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

async fn client(address: &str, input: Option<&str>) -> anyhow::Result<Infallible> {
    let (dev, config) = {
        let host = cpal::default_host();
        if let Some(val) = match input {
            Some(name) => host
                .input_devices()
                .ok()
                .and_then(|mut devices| {
                    devices.find(|dev| dev.name().is_ok_and(|dev_name| dev_name == name))
                })
                .and_then(|dev| {
                    dev.supported_input_configs().ok().and_then(|configs| {
                        configs
                            .filter_map(|conf| {
                                conf.try_with_sample_rate(SampleRate(SAMPLE_RATE as u32))
                            })
                            .filter(|conf| matches!(conf.channels(), 1 | 2))
                            .next()
                            .map(|conf| (dev, conf))
                    })
                }),
            None => host.default_input_device().and_then(|dev| {
                dev.supported_input_configs().ok().and_then(|configs| {
                    configs
                        .filter_map(|conf| {
                            conf.try_with_sample_rate(SampleRate(SAMPLE_RATE as u32))
                        })
                        .filter(|conf| matches!(conf.channels(), 1 | 2))
                        .next()
                        .map(|conf| (dev, conf))
                })
            }),
        } {
            val
        } else {
            bail!("Could not open input device")
        }
    };
    let mut enc = opus::Encoder::new(
        SAMPLE_RATE as u32,
        if config.channels() == 1 {
            opus::Channels::Mono
        } else {
            opus::Channels::Stereo
        },
        opus::Application::Audio,
    )?;
    enc.set_inband_fec(false)?;
    enc.set_bitrate(opus::Bitrate::Bits(BITRATE))?;
    enc.set_vbr(true)?;
    let mut tcp = TcpStream::connect(address).await?;
    tcp.write_u16(config.channels()).await?;
    if tcp.read_u8().await? != 0 {
        bail!("Invalid server response");
    }

    let rb = rb::SpscRb::new(SAMPLE_RATE * config.channels() as usize);
    let (tx, rx) = (rb.producer(), rb.consumer());

    let stream = SendStream(dev.build_input_stream(
        &config.config(),
        move |mut data: &[f32], _| {
            while let Some(len) = tx.write_blocking(data) {
                data = &data[len..];
            }
        },
        |err| eprintln!("{:?}", err),
        None,
    )?);
    stream.0.play()?;

    //  2.5ms buffer
    let mut buffer = vec![0f32; SAMPLE_RATE * 25 / 10_000 * config.channels() as usize];
    let mut opus_buffer = [0u8; MAX_OPUS_PACKET_SIZE];
    loop {
        let mut buf: &mut [f32] = &mut buffer;
        while let Some(len) = rx.read_blocking(&mut buf) {
            buf = &mut buf[len..];
        }
        let len = enc.encode_float(&buffer, &mut opus_buffer)?;
        tcp.write_u32(len as u32).await?;
        tcp.write_all(&opus_buffer[..len]).await?;
        tcp.flush().await?;
    }
}

async fn server(address: &str, output: Option<String>) -> anyhow::Result<Infallible> {
    loop {
        let tcp = TcpListener::bind(address).await?;

        while let Ok((con, addr)) = tcp.accept().await {
            println!("Got connection: {}", addr);
            let output = output.clone();
            tokio::spawn(async move {
                let Err(err) = handle_client(con, output.as_deref()).await;
                eprintln!("Lost connection with {}", addr);
                eprintln!("{:?}", err);
            });
        }
    }
}

async fn handle_client(mut tcp: TcpStream, output: Option<&str>) -> anyhow::Result<Infallible> {
    let channels = tcp.read_u16().await?;
    if !matches!(channels, 1 | 2) {
        bail!("Invalid channel count");
    }

    let rb = rb::SpscRb::new(SAMPLE_RATE * channels as usize);
    let (tx, rx) = (rb.producer(), rb.consumer());

    let dev = {
        let host = cpal::default_host();
        if let Some(val) = match output {
            Some(name) => host.output_devices().ok().and_then(|mut devices| {
                devices.find(|dev| dev.name().is_ok_and(|dev_name| dev_name == name))
            }),
            None => host.default_output_device(),
        } {
            val
        } else {
            bail!("Could not open output device")
        }
    };
    let config = cpal::StreamConfig {
        buffer_size: cpal::BufferSize::Default,
        channels,
        sample_rate: SampleRate(SAMPLE_RATE as u32),
    };

    let stream = SendStream(dev.build_output_stream(
        &config,
        move |data: &mut [f32], _| {
            let len = rx.read(data).unwrap_or_default();
            data[len..].fill(0f32);
        },
        |err| eprintln!("{:?}", err),
        None,
    )?);
    stream.0.play()?;

    let mut dec = opus::Decoder::new(
        SAMPLE_RATE as u32,
        if channels == 1 {
            opus::Channels::Mono
        } else {
            opus::Channels::Stereo
        },
    )?;

    tcp.write_u8(0).await?;

    let mut opus_buffer = [0u8; MAX_OPUS_PACKET_SIZE];
    let mut sample_buffer = vec![0f32; SAMPLE_RATE * 25 / 10_000 * channels as usize];

    loop {
        let len = tcp.read_u32().await? as usize;
        tcp.read_exact(&mut opus_buffer[..len]).await?;
        dec.decode_float(&opus_buffer[..len], &mut sample_buffer, false)?;
        //  Skip null buffer to reduce delay on pause / mute
        if sample_buffer.iter().all(|sample| sample == &0f32) {
            continue;
        }
        let mut buf: &[f32] = &sample_buffer;
        while let Some(len) = tx.write_blocking(buf) {
            buf = &buf[len..];
        }
    }
}

struct SendStream(cpal::Stream);
// they see me Send-ing, they hatin'
unsafe impl Send for SendStream {}
