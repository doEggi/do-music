use crate::DEFAULT_PORT;
use clap::ValueEnum;
use std::{
    fmt::Display,
    net::{SocketAddr, ToSocketAddrs},
};

#[derive(clap::Parser)]
#[command(version, about = "A simple opus music stream between two devices", long_about = None)]
pub struct Args {
    #[command(subcommand)]
    pub cmd: Command,
}

#[derive(clap::Subcommand)]
pub enum Command {
    Client {
        #[arg(short, long)]
        input: Option<String>,
        #[arg(short, long, value_parser = parse_to_socket_addr)]
        address: SocketAddr,
        #[arg(short, long, default_value = "audio")]
        opus_mode: Application,
        #[arg(short, long, value_parser = clap::value_parser!(u16).range(1..=2))]
        channels: Option<u16>,
    },
    Server {
        #[arg(short, long)]
        output: Option<String>,
        #[arg(short, long, value_parser = parse_to_socket_addr)]
        bind: Option<SocketAddr>,
    },
}

fn parse_to_socket_addr(s: &str) -> Result<SocketAddr, String> {
    let addr = if let Some((host, port_str)) = s.rsplit_once(':') {
        let port = port_str.parse().map_err(|_| "Invalid port")?;
        (host, port)
    } else {
        (s, DEFAULT_PORT)
    };
    format!("{}:{}", addr.0, addr.1)
        .to_socket_addrs()
        .map_err(|e| e.to_string())?
        .filter(|a| a.is_ipv4())
        .next()
        .ok_or_else(|| "No address resolved".into())
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Application {
    Voip,
    Audio,
    LowDelay,
}

impl From<Application> for opus::Application {
    fn from(value: Application) -> Self {
        match value {
            Application::Voip => Self::Voip,
            Application::Audio => Self::Audio,
            Application::LowDelay => Self::LowDelay,
        }
    }
}

impl Display for Application {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Voip => f.write_str("VoIp"),
            Self::Audio => f.write_str("Audio"),
            Self::LowDelay => f.write_str("LowDelay"),
        }
    }
}
