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
        #[arg(short, long)]
        name: String,
        #[arg(short, long)]
        input: Option<String>,
    },
    Server {
        #[arg(short, long)]
        name: String,
        #[arg(short, long)]
        output: Option<String>,
    },
    List,
}
