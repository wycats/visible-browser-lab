use anyhow::{Result, bail};

use crate::{broker, config::RuntimeConfig};

pub async fn run(config: RuntimeConfig) -> Result<()> {
    let mut client = broker::ensure_running(&config).await?;
    let status = client.ping().await?;

    tracing::info!(
        broker_pid = status.pid,
        cdp_endpoint = %status.cdp_endpoint,
        socket = %status.socket_path.display(),
        "visible browser MCP facade connected to broker"
    );

    bail!(
        "MCP facade tools are not implemented yet; continue with the session-and-tab-lease-registry task"
    )
}
