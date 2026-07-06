use std::{env, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use directories::BaseDirs;
use serde::{Deserialize, Serialize};
use url::Url;

pub const DEFAULT_CDP_ORIGIN: &str = "http://127.0.0.1";
pub const RELEASE_VERSION: &str = match option_env!("VISIBLE_BROWSER_LAB_RELEASE_VERSION") {
    Some(version) => version,
    None => env!("CARGO_PKG_VERSION"),
};
const CDP_ENDPOINT_ENV: &str = "VISIBLE_BROWSER_CDP_ENDPOINT";
const CDP_PORT_ENV: &str = "VISIBLE_BROWSER_CDP_PORT";
const STATE_DIR_ENV: &str = "VISIBLE_BROWSER_LAB_STATE_DIR";
pub const CHROME_PATH_ENV: &str = "VISIBLE_BROWSER_LAB_CHROME_PATH";
pub const BROKER_IDLE_TIMEOUT_ENV: &str = "VISIBLE_BROWSER_LAB_BROKER_IDLE_TIMEOUT_SECS";
pub const DEFAULT_BROKER_IDLE_TIMEOUT: Duration = Duration::from_secs(15 * 60);

#[derive(Debug, Parser)]
#[command(
    name = "visible-browser-lab-mcp",
    version = RELEASE_VERSION,
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
    #[command(about = "Use the browser tool surface without the MCP transport")]
    Surface(SurfaceArgs),
}

#[derive(Debug, Args)]
pub struct BrokerArgs {
    #[arg(long, value_name = "ENDPOINT")]
    pub socket: Option<String>,

    #[arg(long, value_name = "SECS")]
    pub idle_timeout_secs: Option<u64>,
}

impl BrokerArgs {
    pub fn apply(self, mut config: RuntimeConfig) -> RuntimeConfig {
        if let Some(ipc_endpoint) = self.socket {
            config.ipc_endpoint = ipc_endpoint;
        }
        if let Some(secs) = self.idle_timeout_secs {
            config.idle_timeout = idle_timeout_from_secs(secs);
        }

        config
    }
}

#[derive(Debug, Args)]
pub struct SurfaceArgs {
    #[command(subcommand)]
    pub command: SurfaceCommand,
}

#[derive(Debug, Subcommand)]
pub enum SurfaceCommand {
    #[command(about = "Print the agent tool catalog as JSON")]
    Catalog,
    #[command(about = "Invoke one browser tool with JSON parameters from stdin")]
    Call(SurfaceCallArgs),
}

#[derive(Debug, Args)]
pub struct SurfaceCallArgs {
    #[arg(value_name = "TOOL")]
    pub tool: String,

    #[arg(long, value_name = "DIR")]
    pub workspace_root: Option<PathBuf>,
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
        let state_dir = resolve_state_dir(self.state_dir, env_state_dir)?;
        let chrome_path = env::var_os(CHROME_PATH_ENV).map(PathBuf::from);
        let idle_timeout = resolve_idle_timeout(env::var(BROKER_IDLE_TIMEOUT_ENV).ok().as_deref())?;

        let mut config = match cdp_endpoint {
            Some(cdp_endpoint) => {
                RuntimeConfig::external_with_chrome(cdp_endpoint, state_dir, chrome_path)?
            }
            None => RuntimeConfig::managed(state_dir, chrome_path),
        };
        config.idle_timeout = idle_timeout;
        Ok(config)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeMode {
    Managed,
    External,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeConfig {
    pub runtime_mode: RuntimeMode,
    pub cdp_endpoint: Option<String>,
    pub state_dir: PathBuf,
    pub ipc_endpoint: String,
    pub socket_path: PathBuf,
    pub lock_path: PathBuf,
    pub pid_path: PathBuf,
    pub log_dir: PathBuf,
    pub chrome_profile_dir: PathBuf,
    pub devtools_active_port_path: PathBuf,
    pub chrome_lock_path: PathBuf,
    pub chrome_path: Option<PathBuf>,
    /// How long the broker may sit with no connections and no sessions before
    /// exiting on its own. `None` disables idle exit.
    pub idle_timeout: Option<Duration>,
}

impl RuntimeConfig {
    pub fn from_parts(cdp_endpoint: String, state_dir: PathBuf) -> Result<Self> {
        Self::external_with_chrome(cdp_endpoint, state_dir, None)
    }

    pub fn external_with_chrome(
        cdp_endpoint: String,
        state_dir: PathBuf,
        chrome_path: Option<PathBuf>,
    ) -> Result<Self> {
        let cdp_endpoint = normalize_cdp_endpoint(&cdp_endpoint)?;
        let chrome_profile_dir = state_dir.join("chrome-profile");

        Ok(Self {
            runtime_mode: RuntimeMode::External,
            cdp_endpoint: Some(cdp_endpoint),
            ipc_endpoint: derive_ipc_endpoint(&state_dir),
            socket_path: state_dir.join("broker-v3.sock"),
            lock_path: state_dir.join("broker-v3.lock"),
            pid_path: state_dir.join("broker-v3.pid"),
            log_dir: state_dir.join("logs"),
            devtools_active_port_path: chrome_profile_dir.join("DevToolsActivePort"),
            chrome_lock_path: state_dir.join("chrome-launch.lock"),
            chrome_profile_dir,
            chrome_path,
            state_dir,
            idle_timeout: Some(DEFAULT_BROKER_IDLE_TIMEOUT),
        })
    }

    pub fn managed(state_dir: PathBuf, chrome_path: Option<PathBuf>) -> Self {
        let chrome_profile_dir = state_dir.join("chrome-profile");
        Self {
            runtime_mode: RuntimeMode::Managed,
            cdp_endpoint: None,
            ipc_endpoint: derive_ipc_endpoint(&state_dir),
            socket_path: state_dir.join("broker-v3.sock"),
            lock_path: state_dir.join("broker-v3.lock"),
            pid_path: state_dir.join("broker-v3.pid"),
            log_dir: state_dir.join("logs"),
            devtools_active_port_path: chrome_profile_dir.join("DevToolsActivePort"),
            chrome_lock_path: state_dir.join("chrome-launch.lock"),
            chrome_profile_dir,
            chrome_path,
            state_dir,
            idle_timeout: Some(DEFAULT_BROKER_IDLE_TIMEOUT),
        }
    }
}

/// Resolve the idle window from the environment. Zero disables idle exit,
/// matching the `SCCACHE_IDLE_TIMEOUT=0` convention.
pub fn resolve_idle_timeout(env_secs: Option<&str>) -> Result<Option<Duration>> {
    let Some(raw) = non_empty(env_secs) else {
        return Ok(Some(DEFAULT_BROKER_IDLE_TIMEOUT));
    };

    let secs: u64 = raw
        .parse()
        .with_context(|| format!("invalid {BROKER_IDLE_TIMEOUT_ENV} value `{raw}`"))?;
    Ok(idle_timeout_from_secs(secs))
}

fn idle_timeout_from_secs(secs: u64) -> Option<Duration> {
    if secs == 0 {
        None
    } else {
        Some(Duration::from_secs(secs))
    }
}

pub fn derive_ipc_endpoint(state_dir: &std::path::Path) -> String {
    crate::ipc::default_endpoint_display(state_dir)
}

pub fn resolve_cdp_endpoint(
    cli_endpoint: Option<&str>,
    env_endpoint: Option<&str>,
    env_port: Option<&str>,
) -> Result<Option<String>> {
    if let Some(endpoint) = non_empty(cli_endpoint) {
        return normalize_cdp_endpoint(endpoint).map(Some);
    }

    if let Some(endpoint) = non_empty(env_endpoint) {
        return normalize_cdp_endpoint(endpoint).map(Some);
    }

    if let Some(port) = non_empty(env_port) {
        return normalize_cdp_endpoint(&format!("{DEFAULT_CDP_ORIGIN}:{port}")).map(Some);
    }

    Ok(None)
}

pub fn resolve_state_dir(
    cli_state_dir: Option<PathBuf>,
    env_state_dir: Option<PathBuf>,
) -> Result<PathBuf> {
    if let Some(state_dir) = cli_state_dir.or(env_state_dir) {
        return Ok(state_dir);
    }

    let base_dirs =
        BaseDirs::new().context("could not resolve the operating-system cache directory")?;
    Ok(base_dirs.cache_dir().join("visible-browser-lab"))
}

fn normalize_cdp_endpoint(endpoint: &str) -> Result<String> {
    let trimmed = endpoint.trim().trim_end_matches('/');
    let parsed =
        Url::parse(trimmed).with_context(|| format!("invalid CDP endpoint `{endpoint}`"))?;

    match parsed.scheme() {
        "http" => Ok(trimmed.to_string()),
        scheme => bail!("CDP endpoint must use http, not `{scheme}`"),
    }
}

fn non_empty(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|value| !value.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::{CommandFactory, Parser};

    #[test]
    fn cli_reports_the_compiled_release_version() {
        assert_eq!(Cli::command().get_version(), Some(RELEASE_VERSION));
    }

    #[test]
    fn absent_endpoint_selects_managed_runtime() {
        let endpoint = resolve_cdp_endpoint(None, None, None).unwrap();

        assert_eq!(endpoint, None);
    }

    #[test]
    fn cli_endpoint_wins_over_environment_endpoint_and_port() {
        let endpoint = resolve_cdp_endpoint(
            Some("http://127.0.0.1:9333"),
            Some("http://127.0.0.1:9444"),
            Some("9555"),
        )
        .unwrap();

        assert_eq!(endpoint.as_deref(), Some("http://127.0.0.1:9333"));
    }

    #[test]
    fn environment_endpoint_wins_over_port() {
        let endpoint =
            resolve_cdp_endpoint(None, Some("http://127.0.0.1:9444/"), Some("9555")).unwrap();

        assert_eq!(endpoint.as_deref(), Some("http://127.0.0.1:9444"));
    }

    #[test]
    fn environment_port_builds_localhost_endpoint() {
        let endpoint = resolve_cdp_endpoint(None, None, Some("9333")).unwrap();

        assert_eq!(endpoint.as_deref(), Some("http://127.0.0.1:9333"));
    }

    #[test]
    fn rejects_websocket_endpoints_at_runtime_boundary() {
        let err = resolve_cdp_endpoint(Some("ws://127.0.0.1:9222"), None, None).unwrap_err();

        assert!(err.to_string().contains("must use http"));
    }

    #[test]
    fn derives_runtime_paths_from_state_dir() {
        let config = RuntimeConfig::from_parts(
            "http://127.0.0.1:9222".to_string(),
            PathBuf::from("/tmp/visible-browser-lab-test"),
        )
        .unwrap();

        assert_eq!(config.runtime_mode, RuntimeMode::External);
        assert_eq!(
            config.cdp_endpoint.as_deref(),
            Some("http://127.0.0.1:9222")
        );

        assert_eq!(
            config.socket_path,
            PathBuf::from("/tmp/visible-browser-lab-test/broker-v3.sock")
        );
        if cfg!(windows) {
            assert!(config.ipc_endpoint.starts_with("visible-browser-lab-"));
            assert!(!config.ipc_endpoint.contains('/'));
        } else {
            assert_eq!(
                config.ipc_endpoint,
                "/tmp/visible-browser-lab-test/broker-v3.sock"
            );
        }
        assert_eq!(
            config.lock_path,
            PathBuf::from("/tmp/visible-browser-lab-test/broker-v3.lock")
        );
        assert_eq!(
            config.pid_path,
            PathBuf::from("/tmp/visible-browser-lab-test/broker-v3.pid")
        );
        assert_eq!(
            config.log_dir,
            PathBuf::from("/tmp/visible-browser-lab-test/logs")
        );
        assert_eq!(
            config.chrome_profile_dir,
            PathBuf::from("/tmp/visible-browser-lab-test/chrome-profile")
        );
        assert_eq!(
            config.devtools_active_port_path,
            PathBuf::from("/tmp/visible-browser-lab-test/chrome-profile/DevToolsActivePort")
        );
    }

    #[test]
    fn default_state_dir_uses_the_platform_cache_directory() {
        let state_dir = resolve_state_dir(None, None).unwrap();

        assert_eq!(
            state_dir,
            BaseDirs::new()
                .unwrap()
                .cache_dir()
                .join("visible-browser-lab")
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

        assert_eq!(args.socket.as_deref(), Some("/tmp/lab.sock"));
    }

    #[test]
    fn idle_timeout_defaults_to_fifteen_minutes() {
        assert_eq!(
            resolve_idle_timeout(None).unwrap(),
            Some(DEFAULT_BROKER_IDLE_TIMEOUT)
        );
        assert_eq!(
            resolve_idle_timeout(Some("  ")).unwrap(),
            Some(DEFAULT_BROKER_IDLE_TIMEOUT)
        );
    }

    #[test]
    fn idle_timeout_env_parses_seconds_and_zero_disables() {
        assert_eq!(
            resolve_idle_timeout(Some("120")).unwrap(),
            Some(Duration::from_secs(120))
        );
        assert_eq!(resolve_idle_timeout(Some("0")).unwrap(), None);
    }

    #[test]
    fn idle_timeout_env_rejects_garbage() {
        let err = resolve_idle_timeout(Some("soon")).unwrap_err();

        assert!(err.to_string().contains("soon"));
    }

    #[test]
    fn broker_flag_overrides_the_configured_idle_timeout() {
        let cli = Cli::try_parse_from([
            "visible-browser-lab-mcp",
            "broker",
            "--idle-timeout-secs",
            "2",
        ])
        .unwrap();

        let Some(Command::Broker(args)) = cli.command else {
            panic!("expected broker subcommand");
        };

        let config = args.apply(RuntimeConfig::managed(PathBuf::from("/tmp/lab"), None));
        assert_eq!(config.idle_timeout, Some(Duration::from_secs(2)));
    }

    #[test]
    fn broker_flag_zero_disables_idle_exit() {
        let cli = Cli::try_parse_from([
            "visible-browser-lab-mcp",
            "broker",
            "--idle-timeout-secs",
            "0",
        ])
        .unwrap();

        let Some(Command::Broker(args)) = cli.command else {
            panic!("expected broker subcommand");
        };

        let config = args.apply(RuntimeConfig::managed(PathBuf::from("/tmp/lab"), None));
        assert_eq!(config.idle_timeout, None);
    }

    #[test]
    fn broker_without_flag_keeps_the_configured_idle_timeout() {
        let cli = Cli::try_parse_from(["visible-browser-lab-mcp", "broker"]).unwrap();

        let Some(Command::Broker(args)) = cli.command else {
            panic!("expected broker subcommand");
        };

        let mut base = RuntimeConfig::managed(PathBuf::from("/tmp/lab"), None);
        base.idle_timeout = Some(Duration::from_secs(7));
        let config = args.apply(base);
        assert_eq!(config.idle_timeout, Some(Duration::from_secs(7)));
    }
}
