use std::{env, path::PathBuf};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use url::Url;

pub const DEFAULT_CDP_PORT: &str = "9222";
pub const DEFAULT_CDP_ORIGIN: &str = "http://127.0.0.1";
pub const DEFAULT_STATE_DIR: &str = "/Users/wycats/.cache/visible-browser-lab";

const CDP_ENDPOINT_ENV: &str = "VISIBLE_BROWSER_CDP_ENDPOINT";
const CDP_PORT_ENV: &str = "VISIBLE_BROWSER_CDP_PORT";
const STATE_DIR_ENV: &str = "VISIBLE_BROWSER_LAB_STATE_DIR";

#[derive(Debug, Parser)]
#[command(
    name = "visible-browser-lab-mcp",
    version,
    about = "MCP facade for a shared visible Chrome browser",
    long_about = "Runs the Visible Browser Lab MCP facade or its local broker over a shared visible Chrome CDP endpoint."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    #[arg(long, global = true, value_name = "URL")]
    pub cdp_endpoint: Option<String>,

    #[arg(long, global = true, value_name = "DIR")]
    pub state_dir: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    #[command(about = "Run the tab-lease broker process")]
    Broker(BrokerArgs),
}

#[derive(Debug, Args)]
pub struct BrokerArgs {
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
}

impl BrokerArgs {
    pub fn apply(self, mut config: RuntimeConfig) -> RuntimeConfig {
        if let Some(socket_path) = self.socket {
            config.socket_path = socket_path;
        }

        config
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeOptions {
    pub cdp_endpoint: Option<String>,
    pub state_dir: Option<PathBuf>,
}

impl RuntimeOptions {
    pub fn into_config(self) -> Result<RuntimeConfig> {
        let env_endpoint = env::var(CDP_ENDPOINT_ENV).ok();
        let env_port = env::var(CDP_PORT_ENV).ok();
        let cdp_endpoint = resolve_cdp_endpoint(
            self.cdp_endpoint.as_deref(),
            env_endpoint.as_deref(),
            env_port.as_deref(),
        )?;

        let env_state_dir = env::var_os(STATE_DIR_ENV).map(PathBuf::from);
        let state_dir = resolve_state_dir(self.state_dir, env_state_dir);

        RuntimeConfig::from_parts(cdp_endpoint, state_dir)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub cdp_endpoint: String,
    pub state_dir: PathBuf,
    pub socket_path: PathBuf,
    pub lock_path: PathBuf,
    pub pid_path: PathBuf,
    pub log_dir: PathBuf,
}

impl RuntimeConfig {
    pub fn from_parts(cdp_endpoint: String, state_dir: PathBuf) -> Result<Self> {
        let cdp_endpoint = normalize_cdp_endpoint(&cdp_endpoint)?;

        Ok(Self {
            cdp_endpoint,
            socket_path: state_dir.join("broker.sock"),
            lock_path: state_dir.join("broker.lock"),
            pid_path: state_dir.join("broker.pid"),
            log_dir: state_dir.join("logs"),
            state_dir,
        })
    }
}

pub fn resolve_cdp_endpoint(
    cli_endpoint: Option<&str>,
    env_endpoint: Option<&str>,
    env_port: Option<&str>,
) -> Result<String> {
    if let Some(endpoint) = non_empty(cli_endpoint) {
        return normalize_cdp_endpoint(endpoint);
    }

    if let Some(endpoint) = non_empty(env_endpoint) {
        return normalize_cdp_endpoint(endpoint);
    }

    let port = non_empty(env_port).unwrap_or(DEFAULT_CDP_PORT);
    normalize_cdp_endpoint(&format!("{DEFAULT_CDP_ORIGIN}:{port}"))
}

pub fn resolve_state_dir(
    cli_state_dir: Option<PathBuf>,
    env_state_dir: Option<PathBuf>,
) -> PathBuf {
    cli_state_dir
        .or(env_state_dir)
        .unwrap_or_else(|| PathBuf::from(DEFAULT_STATE_DIR))
}

fn normalize_cdp_endpoint(endpoint: &str) -> Result<String> {
    let trimmed = endpoint.trim().trim_end_matches('/');
    let parsed =
        Url::parse(trimmed).with_context(|| format!("invalid CDP endpoint `{endpoint}`"))?;

    match parsed.scheme() {
        "http" | "https" => Ok(trimmed.to_string()),
        scheme => bail!("CDP endpoint must use http or https, not `{scheme}`"),
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn default_endpoint_targets_visible_browser_port() {
        let endpoint = resolve_cdp_endpoint(None, None, None).unwrap();

        assert_eq!(endpoint, "http://127.0.0.1:9222");
    }

    #[test]
    fn cli_endpoint_wins_over_environment_endpoint_and_port() {
        let endpoint = resolve_cdp_endpoint(
            Some("http://127.0.0.1:9333"),
            Some("http://127.0.0.1:9444"),
            Some("9555"),
        )
        .unwrap();

        assert_eq!(endpoint, "http://127.0.0.1:9333");
    }

    #[test]
    fn environment_endpoint_wins_over_port() {
        let endpoint =
            resolve_cdp_endpoint(None, Some("http://127.0.0.1:9444/"), Some("9555")).unwrap();

        assert_eq!(endpoint, "http://127.0.0.1:9444");
    }

    #[test]
    fn environment_port_builds_localhost_endpoint() {
        let endpoint = resolve_cdp_endpoint(None, None, Some("9333")).unwrap();

        assert_eq!(endpoint, "http://127.0.0.1:9333");
    }

    #[test]
    fn rejects_websocket_endpoints_at_runtime_boundary() {
        let err = resolve_cdp_endpoint(Some("ws://127.0.0.1:9222"), None, None).unwrap_err();

        assert!(err.to_string().contains("http or https"));
    }

    #[test]
    fn derives_runtime_paths_from_state_dir() {
        let config = RuntimeConfig::from_parts(
            "http://127.0.0.1:9222".to_string(),
            PathBuf::from("/tmp/visible-browser-lab-test"),
        )
        .unwrap();

        assert_eq!(
            config.socket_path,
            PathBuf::from("/tmp/visible-browser-lab-test/broker.sock")
        );
        assert_eq!(
            config.lock_path,
            PathBuf::from("/tmp/visible-browser-lab-test/broker.lock")
        );
        assert_eq!(
            config.pid_path,
            PathBuf::from("/tmp/visible-browser-lab-test/broker.pid")
        );
        assert_eq!(
            config.log_dir,
            PathBuf::from("/tmp/visible-browser-lab-test/logs")
        );
    }

    #[test]
    fn global_options_parse_after_broker_subcommand() {
        let cli = Cli::try_parse_from([
            "visible-browser-lab-mcp",
            "broker",
            "--socket",
            "/tmp/lab.sock",
            "--cdp-endpoint",
            "http://127.0.0.1:9333",
            "--state-dir",
            "/tmp/lab",
        ])
        .unwrap();

        assert_eq!(cli.cdp_endpoint.as_deref(), Some("http://127.0.0.1:9333"));
        assert_eq!(cli.state_dir, Some(PathBuf::from("/tmp/lab")));

        let Some(Command::Broker(args)) = cli.command else {
            panic!("expected broker subcommand");
        };

        assert_eq!(args.socket, Some(PathBuf::from("/tmp/lab.sock")));
    }
}
