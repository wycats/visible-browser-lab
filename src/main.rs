use anyhow::Result;
use clap::Parser;
use visible_browser_lab::{
    broker,
    config::{Cli, Command, RuntimeOptions},
    mcp, surface_cli,
};

#[tokio::main]
async fn main() -> Result<()> {
    install_tracing();

    let Cli {
        command,
        cdp_endpoint,
        state_dir,
    } = Cli::parse();

    let config = RuntimeOptions {
        cdp_endpoint,
        state_dir,
    }
    .into_config()?;

    match command {
        Some(Command::Broker(args)) => broker::run(args.apply(config)).await,
        Some(Command::Surface(args)) => surface_cli::run(config, args).await,
        None => mcp::run(config).await,
    }
}

fn install_tracing() {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "visible_browser_lab=info,warn".into());

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .init();
}
