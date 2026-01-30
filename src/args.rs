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
        #[arg(short = 'i', long)]
        input: Option<String>,
        #[arg(
            short = 'l',
            long,
            long_help = "Low latency mode. Might affect quality",
            default_value = "false"
        )]
        low_latency: bool,
    },
    Server {
        #[arg(short = 'n', long)]
        name: String,
        #[arg(short = 'o', long)]
        output: Option<String>,
    },
}
