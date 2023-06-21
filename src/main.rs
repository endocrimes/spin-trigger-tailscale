use anyhow::{Error, Result};
use clap::Parser;
use is_terminal::IsTerminal;
use spin_trigger::{cli::{TriggerExecutorCommand}};
use spin_trigger_http_tsnet::{HttpTrigger};

type Command = TriggerExecutorCommand<HttpTrigger>;

#[tokio::main]
async fn main() -> Result<(), Error> {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_ansi(std::io::stderr().is_terminal())
        .init();

    let t = Command::parse();
    t.run().await
}

