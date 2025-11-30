use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(version, about, long_about = None, author = "doEggi")]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Command,
}

#[derive(Subcommand)]
pub enum Command {
    Client {
        #[arg(short = 'n', long)]
        name: String,
        #[arg(short = 'd', long)]
        input: Option<String>,
        #[arg(short = 'b', long)]
        bitrate: Option<u32>,
    },
    Server {
        #[arg(short = 'n', long)]
        name: String,
        #[arg(short = 'd', long)]
        output: Option<String>,
    },
    List,
}
