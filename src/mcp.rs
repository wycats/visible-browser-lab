use anyhow::{Result, bail};

use crate::config::RuntimeConfig;

pub async fn run(config: RuntimeConfig) -> Result<()> {
    tracing::info!(
        cdp_endpoint = %config.cdp_endpoint,
        socket = %config.socket_path.display(),
        state_dir = %config.state_dir.display(),
        "visible browser MCP facade scaffold initialized"
    );

    bail!(
        "MCP facade tools are not implemented yet; continue with the broker-lifecycle-and-socket-protocol task"
    )
}
