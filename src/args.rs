use crate::DEFAULT_PORT;
use std::net::{SocketAddr, ToSocketAddrs};

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
        #[arg(short = 'l', long, default_value_t = false)]
        low_latency: bool,
        #[arg(long, default_value_t = 10., value_parser = parse_opus_ms)]
        opus_ms: f32,
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

fn parse_opus_ms(s: &str) -> Result<f32, String> {
    let opus_ms: f32 = s.parse().map_err(|err| format!("{:?}", err))?;
    if !matches!(opus_ms, 2.5 | 5. | 10. | 20. | 40. | 60.) {
        return Err(format!(
            "invalid opus_ms value: {}\nHas to be one of [2.5, 5, 10, 20, 40, 60]",
            opus_ms
        ));
    }
    Ok(opus_ms)
}
