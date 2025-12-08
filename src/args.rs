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
        #[arg(short = 'a', long)]
        address: String,
        #[arg(short = 'i', long)]
        input: Option<String>,
    },
    Server {
        #[arg(short = 'a', long)]
        address: Option<String>,
        #[arg(short = 'o', long)]
        output: Option<String>,
    },
}
