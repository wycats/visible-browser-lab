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
        conversation_identity_compatibility,
    } = Cli::parse();

    let config = RuntimeOptions {
        cdp_endpoint,
        state_dir,
    }
    .into_config()?;

    match command {
        Some(Command::Broker(args)) => broker::run(args.apply(config)).await,
        Some(Command::Surface(args)) => surface_cli::run(config, args).await,
        None => mcp::run(config, conversation_identity_compatibility).await,
    }
}

fn install_tracing() {
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        // Chromiumoxide logs every forward-compatible CDP event it cannot
        // decode at WARN. Modern Chrome can emit hundreds per minute, hiding
        // the actionable VBL recovery diagnostics and growing broker logs
        // without bound. Keep transport failures at ERROR by default; an
        // explicit RUST_LOG can still opt into the raw protocol warnings.
        .unwrap_or_else(|_| "visible_browser_lab=info,chromiumoxide::handler=error,warn".into());

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .init();
}
