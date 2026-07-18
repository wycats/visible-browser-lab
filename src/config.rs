use std::{env, path::PathBuf, time::Duration};

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use directories::BaseDirs;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::{
    conversation_identity::ConversationIdentityCompatibility, protocol::BROKER_PROTOCOL_VERSION,
};

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
pub const SESSION_TTL_ENV: &str = "VISIBLE_BROWSER_LAB_SESSION_TTL_SECS";
/// Four times the broker's idle window, mirroring the staleness bound it
/// replaces: long enough that no plausible pause in active work hits it,
/// short enough that the tab pool recovers from a crashed client within
/// the hour.
pub const DEFAULT_SESSION_TTL: Duration =
    Duration::from_secs(DEFAULT_BROKER_IDLE_TIMEOUT.as_secs() * 4);

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

    #[arg(
        long,
        global = true,
        value_name = "URL",
        help = "Connect to an external Chrome CDP endpoint; without --state-dir, uses a stable endpoint-specific broker state directory"
    )]
    pub cdp_endpoint: Option<String>,

    #[arg(
        long,
        global = true,
        value_name = "DIR",
        help = "Override broker state; a directory already used by another runtime or CDP endpoint is preserved and rejected"
    )]
    pub state_dir: Option<PathBuf>,

    #[arg(
        long,
        global = true,
        value_enum,
        default_value_t = ConversationIdentityCompatibility::Disabled
    )]
    pub conversation_identity_compatibility: ConversationIdentityCompatibility,
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

    #[arg(long, value_name = "SECS")]
    pub session_ttl_secs: Option<u64>,
}

impl BrokerArgs {
    pub fn apply(self, mut config: RuntimeConfig) -> RuntimeConfig {
        if let Some(ipc_endpoint) = self.socket {
            #[cfg(not(windows))]
            {
                config.socket_path = PathBuf::from(&ipc_endpoint);
            }
            config.ipc_endpoint = ipc_endpoint;
            config.implicit_external_socket_parent = None;
        }
        if let Some(secs) = self.idle_timeout_secs {
            config.idle_timeout = idle_timeout_from_secs(secs);
        }
        if let Some(secs) = self.session_ttl_secs {
            config.session_ttl = idle_timeout_from_secs(secs);
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

    #[arg(long, value_name = "VERSION")]
    pub request_envelope_version: Option<u32>,
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
        let state_dir_is_explicit = self.state_dir.is_some() || env_state_dir.is_some();
        let state_dir = resolve_state_dir(self.state_dir, env_state_dir)?;
        let chrome_path = env::var_os(CHROME_PATH_ENV).map(PathBuf::from);
        let idle_timeout = resolve_idle_timeout(env::var(BROKER_IDLE_TIMEOUT_ENV).ok().as_deref())?;
        let session_ttl = resolve_session_ttl(env::var(SESSION_TTL_ENV).ok().as_deref())?;

        let mut config = match cdp_endpoint {
            Some(cdp_endpoint) if !state_dir_is_explicit => {
                RuntimeConfig::implicit_external(cdp_endpoint, state_dir, chrome_path)?
            }
            Some(cdp_endpoint) => {
                RuntimeConfig::external_with_chrome(cdp_endpoint, state_dir, chrome_path)?
            }
            None => RuntimeConfig::managed(state_dir, chrome_path),
        };
        config.idle_timeout = idle_timeout;
        config.session_ttl = session_ttl;
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
    /// How long a session may go untouched before the expiry sweep releases
    /// its tabs and removes it. `None` disables expiry.
    pub session_ttl: Option<Duration>,
    pub(crate) implicit_external_fallback_state_dir: Option<PathBuf>,
    /// The private parent created for VBL's generated short Unix socket.
    /// Custom `--socket` paths retain migration provenance but clear this
    /// marker so their parent directory is never chmodded by VBL.
    pub(crate) implicit_external_socket_parent: Option<PathBuf>,
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
        let cdp_endpoint = canonical_cdp_endpoint(&cdp_endpoint)?;
        let chrome_profile_dir = state_dir.join("chrome-profile");

        Ok(Self {
            runtime_mode: RuntimeMode::External,
            cdp_endpoint: Some(cdp_endpoint),
            ipc_endpoint: derive_ipc_endpoint(&state_dir),
            socket_path: state_dir.join("broker-v4.sock"),
            lock_path: state_dir.join("broker-v4.lock"),
            pid_path: state_dir.join("broker-v4.pid"),
            log_dir: state_dir.join("logs"),
            devtools_active_port_path: chrome_profile_dir.join("DevToolsActivePort"),
            chrome_lock_path: state_dir.join("chrome-launch.lock"),
            chrome_profile_dir,
            chrome_path,
            state_dir,
            idle_timeout: Some(DEFAULT_BROKER_IDLE_TIMEOUT),
            session_ttl: Some(DEFAULT_SESSION_TTL),
            implicit_external_fallback_state_dir: None,
            implicit_external_socket_parent: None,
        })
    }

    pub fn managed(state_dir: PathBuf, chrome_path: Option<PathBuf>) -> Self {
        let chrome_profile_dir = state_dir.join("chrome-profile");
        Self {
            runtime_mode: RuntimeMode::Managed,
            cdp_endpoint: None,
            ipc_endpoint: derive_ipc_endpoint(&state_dir),
            socket_path: state_dir.join("broker-v4.sock"),
            lock_path: state_dir.join("broker-v4.lock"),
            pid_path: state_dir.join("broker-v4.pid"),
            log_dir: state_dir.join("logs"),
            devtools_active_port_path: chrome_profile_dir.join("DevToolsActivePort"),
            chrome_lock_path: state_dir.join("chrome-launch.lock"),
            chrome_profile_dir,
            chrome_path,
            state_dir,
            idle_timeout: Some(DEFAULT_BROKER_IDLE_TIMEOUT),
            session_ttl: Some(DEFAULT_SESSION_TTL),
            implicit_external_fallback_state_dir: None,
            implicit_external_socket_parent: None,
        }
    }

    pub(crate) fn implicit_external(
        cdp_endpoint: String,
        default_state_dir: PathBuf,
        chrome_path: Option<PathBuf>,
    ) -> Result<Self> {
        let cdp_endpoint = canonical_cdp_endpoint(&cdp_endpoint)?;
        let state_dir = external_state_dir(&default_state_dir, &cdp_endpoint);
        let mut config = Self::external_with_chrome(cdp_endpoint, state_dir, chrome_path)?;
        config.implicit_external_fallback_state_dir = Some(default_state_dir.clone());
        config.ipc_endpoint =
            implicit_external_ipc_endpoint(&default_state_dir, &config, BROKER_PROTOCOL_VERSION)?;
        #[cfg(not(windows))]
        {
            config.socket_path = PathBuf::from(&config.ipc_endpoint);
            config.implicit_external_socket_parent = config
                .socket_path
                .parent()
                .map(std::path::Path::to_path_buf);
        }
        Ok(config)
    }

    pub(crate) fn implicit_external_ipc_endpoint_for_protocol(
        &self,
        protocol_version: u32,
    ) -> Result<Option<String>> {
        let Some(default_state_dir) = self.implicit_external_fallback_state_dir.as_deref() else {
            return Ok(None);
        };
        implicit_external_ipc_endpoint(default_state_dir, self, protocol_version).map(Some)
    }

    pub(crate) fn implicit_external_fallback_config(&self) -> Result<Option<Self>> {
        // Transitional lookup for brokers created before implicit external
        // endpoints received their own state namespace. The broker layer
        // reuses only a matching protocol/runtime/endpoint and never moves or
        // terminates the old registry while it may still own leases.
        let Some(default_state_dir) = self.implicit_external_fallback_state_dir.as_deref() else {
            return Ok(None);
        };
        self.implicit_external_fallback_config_for(default_state_dir)
    }

    fn implicit_external_fallback_config_for(
        &self,
        default_state_dir: &std::path::Path,
    ) -> Result<Option<Self>> {
        let Some(cdp_endpoint) = self.cdp_endpoint.as_deref() else {
            return Ok(None);
        };
        if self.runtime_mode != RuntimeMode::External
            || self.implicit_external_fallback_state_dir.as_deref() != Some(default_state_dir)
            || self.state_dir != external_state_dir(default_state_dir, cdp_endpoint)
        {
            return Ok(None);
        }

        let mut fallback = Self::external_with_chrome(
            cdp_endpoint.to_string(),
            default_state_dir.to_path_buf(),
            self.chrome_path.clone(),
        )?;
        fallback.idle_timeout = self.idle_timeout;
        fallback.session_ttl = self.session_ttl;
        Ok(Some(fallback))
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

/// Resolve the session TTL from the environment. Zero disables expiry,
/// the same convention as the broker's idle window.
pub fn resolve_session_ttl(env_secs: Option<&str>) -> Result<Option<Duration>> {
    let Some(raw) = non_empty(env_secs) else {
        return Ok(Some(DEFAULT_SESSION_TTL));
    };

    let secs: u64 = raw
        .parse()
        .with_context(|| format!("invalid {SESSION_TTL_ENV} value `{raw}`"))?;
    Ok(idle_timeout_from_secs(secs))
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
        return canonical_cdp_endpoint(endpoint).map(Some);
    }

    if let Some(endpoint) = non_empty(env_endpoint) {
        return canonical_cdp_endpoint(endpoint).map(Some);
    }

    if let Some(port) = non_empty(env_port) {
        return canonical_cdp_endpoint(&format!("{DEFAULT_CDP_ORIGIN}:{port}")).map(Some);
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

fn external_state_dir(default_state_dir: &std::path::Path, cdp_endpoint: &str) -> PathBuf {
    let digest = format!("{:x}", Sha256::digest(cdp_endpoint.as_bytes()));
    default_state_dir.join("external").join(&digest[..32])
}

fn implicit_external_ipc_endpoint(
    default_state_dir: &std::path::Path,
    config: &RuntimeConfig,
    protocol_version: u32,
) -> Result<String> {
    #[cfg(windows)]
    {
        let _ = default_state_dir;
        Ok(crate::ipc::endpoint_display_for_protocol(
            &config.state_dir,
            protocol_version,
        ))
    }

    #[cfg(not(windows))]
    {
        let endpoint = config
            .cdp_endpoint
            .as_deref()
            .context("implicit external runtime omitted its CDP endpoint")?;
        let user_digest = format!(
            "{:x}",
            Sha256::digest(default_state_dir.to_string_lossy().as_bytes())
        );
        let endpoint_digest = format!("{:x}", Sha256::digest(endpoint.as_bytes()));
        Ok(format!(
            "/tmp/vbl-{}/v{protocol_version}-{}.sock",
            &user_digest[..16],
            &endpoint_digest[..32]
        ))
    }
}

pub(crate) fn canonical_cdp_endpoint(endpoint: &str) -> Result<String> {
    let trimmed = endpoint.trim();
    let parsed =
        Url::parse(trimmed).with_context(|| format!("invalid CDP endpoint `{endpoint}`"))?;

    match parsed.scheme() {
        "http" => {
            if !parsed.username().is_empty() || parsed.password().is_some() {
                bail!("CDP endpoint must not contain credentials");
            }
            if parsed.host_str().is_none() {
                bail!("CDP endpoint must include a host");
            }
            Ok(parsed.origin().ascii_serialization())
        }
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
    fn endpoint_paths_queries_and_fragments_share_one_canonical_origin() {
        let root = resolve_cdp_endpoint(Some("http://127.0.0.1:9222"), None, None).unwrap();
        let version = resolve_cdp_endpoint(
            Some("http://127.0.0.1:9222/json/version?fresh=1#ignored"),
            None,
            None,
        )
        .unwrap();

        assert_eq!(version, root);
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
            PathBuf::from("/tmp/visible-browser-lab-test/broker-v4.sock")
        );
        if cfg!(windows) {
            assert!(config.ipc_endpoint.starts_with("visible-browser-lab-"));
            assert!(!config.ipc_endpoint.contains('/'));
        } else {
            assert_eq!(
                config.ipc_endpoint,
                "/tmp/visible-browser-lab-test/broker-v4.sock"
            );
        }
        assert_eq!(
            config.lock_path,
            PathBuf::from("/tmp/visible-browser-lab-test/broker-v4.lock")
        );
        assert_eq!(
            config.pid_path,
            PathBuf::from("/tmp/visible-browser-lab-test/broker-v4.pid")
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
    fn implicit_external_runtime_uses_a_deterministic_isolated_state_dir() {
        let default_state_dir = PathBuf::from("/cache/visible-browser-lab");
        let first = RuntimeConfig::implicit_external(
            "http://127.0.0.1:9222".to_string(),
            default_state_dir.clone(),
            None,
        )
        .unwrap();
        let repeated = RuntimeConfig::implicit_external(
            "http://127.0.0.1:9222".to_string(),
            default_state_dir.clone(),
            None,
        )
        .unwrap();
        let other = RuntimeConfig::implicit_external(
            "http://127.0.0.1:9333".to_string(),
            default_state_dir.clone(),
            None,
        )
        .unwrap();

        assert_eq!(first.state_dir, repeated.state_dir);
        assert_eq!(first.ipc_endpoint, repeated.ipc_endpoint);
        assert!(
            first
                .state_dir
                .starts_with(default_state_dir.join("external"))
        );
        assert_eq!(
            first
                .state_dir
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap()
                .len(),
            32
        );
        assert_ne!(first.state_dir, other.state_dir);
        assert_ne!(first.ipc_endpoint, other.ipc_endpoint);
        #[cfg(not(windows))]
        {
            assert!(first.ipc_endpoint.starts_with("/tmp/vbl-"));
            assert!(first.ipc_endpoint.contains("/v4-"));
            assert!(
                first
                    .implicit_external_ipc_endpoint_for_protocol(3)
                    .unwrap()
                    .unwrap()
                    .contains("/v3-")
            );
            assert!(
                first.ipc_endpoint.len() < derive_ipc_endpoint(&first.state_dir).len(),
                "implicit external socket should be shorter than a socket nested under its state"
            );
        }
    }

    #[test]
    fn implicit_external_runtime_hashes_the_canonical_cdp_origin() {
        let default_state_dir = PathBuf::from("/cache/visible-browser-lab");
        let root = RuntimeConfig::implicit_external(
            "http://127.0.0.1:9222".to_string(),
            default_state_dir.clone(),
            None,
        )
        .unwrap();
        let version = RuntimeConfig::implicit_external(
            "http://127.0.0.1:9222/json/version?fresh=1".to_string(),
            default_state_dir,
            None,
        )
        .unwrap();

        assert_eq!(version.cdp_endpoint, root.cdp_endpoint);
        assert_eq!(version.state_dir, root.state_dir);
        assert_eq!(version.ipc_endpoint, root.ipc_endpoint);
    }

    #[test]
    fn explicit_state_dir_wins_for_external_runtime() {
        let explicit = PathBuf::from("/tmp/vbl-external");

        let config =
            RuntimeConfig::from_parts("http://127.0.0.1:9222".to_string(), explicit.clone())
                .unwrap();

        assert_eq!(config.state_dir, explicit);
        assert!(config.implicit_external_fallback_state_dir.is_none());
    }

    #[test]
    fn implicit_external_runtime_retains_the_previous_default_state_as_a_fallback() {
        let default_state_dir = PathBuf::from("/cache/visible-browser-lab");
        let cdp_endpoint = "http://127.0.0.1:9222";
        let config = RuntimeConfig::implicit_external(
            cdp_endpoint.to_string(),
            default_state_dir.clone(),
            None,
        )
        .unwrap();

        let fallback = config
            .implicit_external_fallback_config_for(&default_state_dir)
            .unwrap()
            .expect("implicit external config should retain its prior state location");

        assert_eq!(fallback.state_dir, default_state_dir);
        assert_eq!(fallback.runtime_mode, RuntimeMode::External);
        assert_eq!(fallback.cdp_endpoint, config.cdp_endpoint);
    }

    #[test]
    fn explicit_external_state_has_no_implicit_fallback() {
        let default_state_dir = PathBuf::from("/cache/visible-browser-lab");
        let config = RuntimeConfig::from_parts(
            "http://127.0.0.1:9222".to_string(),
            PathBuf::from("/tmp/vbl-external"),
        )
        .unwrap();

        assert!(
            config
                .implicit_external_fallback_config_for(&default_state_dir)
                .unwrap()
                .is_none()
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
        let applied = args.apply(
            RuntimeConfig::from_parts(
                "http://127.0.0.1:9333".to_string(),
                PathBuf::from("/tmp/lab"),
            )
            .unwrap(),
        );
        assert_eq!(applied.ipc_endpoint, "/tmp/lab.sock");
        #[cfg(not(windows))]
        assert_eq!(applied.socket_path, PathBuf::from("/tmp/lab.sock"));
    }

    #[test]
    fn custom_socket_keeps_migration_provenance_without_generated_socket_hardening() {
        let default_state_dir = PathBuf::from("/cache/visible-browser-lab");
        let config = RuntimeConfig::implicit_external(
            "http://127.0.0.1:9222".to_string(),
            default_state_dir.clone(),
            None,
        )
        .unwrap();
        let applied = BrokerArgs {
            socket: Some("/tmp/lab.sock".to_string()),
            idle_timeout_secs: None,
            session_ttl_secs: None,
        }
        .apply(config);

        assert_eq!(
            applied.implicit_external_fallback_state_dir.as_deref(),
            Some(default_state_dir.as_path())
        );
        assert!(applied.implicit_external_socket_parent.is_none());
    }

    #[test]
    fn trusted_codex_compatibility_is_an_explicit_global_option() {
        let default = Cli::try_parse_from(["visible-browser-lab-mcp"]).unwrap();
        assert_eq!(
            default.conversation_identity_compatibility,
            ConversationIdentityCompatibility::Disabled
        );

        let trusted = Cli::try_parse_from([
            "visible-browser-lab-mcp",
            "--conversation-identity-compatibility",
            "trusted-codex-thread-id",
        ])
        .unwrap();
        assert_eq!(
            trusted.conversation_identity_compatibility,
            ConversationIdentityCompatibility::TrustedCodexThreadId
        );
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
