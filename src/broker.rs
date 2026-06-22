use anyhow::{Result, bail};

use crate::config::RuntimeConfig;

pub async fn run(config: RuntimeConfig) -> Result<()> {
    prepare_state(&config).await?;

    tracing::info!(
        cdp_endpoint = %config.cdp_endpoint,
        socket = %config.socket_path.display(),
        state_dir = %config.state_dir.display(),
        "visible browser broker scaffold initialized"
    );

    bail!(
        "broker socket protocol is not implemented yet; continue with the broker-lifecycle-and-socket-protocol task"
    )
}

pub async fn prepare_state(config: &RuntimeConfig) -> Result<()> {
    tokio::fs::create_dir_all(&config.state_dir).await?;
    tokio::fs::create_dir_all(&config.log_dir).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RuntimeConfig;

    #[tokio::test]
    async fn prepare_state_creates_state_and_log_directories() {
        let tempdir = tempfile::tempdir().unwrap();
        let state_dir = tempdir.path().join("state");
        let config =
            RuntimeConfig::from_parts("http://127.0.0.1:9222".to_string(), state_dir.clone())
                .unwrap();

        prepare_state(&config).await.unwrap();

        assert!(state_dir.is_dir());
        assert!(state_dir.join("logs").is_dir());
    }
}
