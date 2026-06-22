use std::{
    fs::{self, File, OpenOptions},
    io::ErrorKind,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    time::{Instant, sleep},
};

use crate::{
    config::RuntimeConfig,
    ipc::{self, BrokerEndpoint, BrokerListener, BrokerStream},
    leases::BrowserToolError,
    protocol::{
        BROKER_PROTOCOL_VERSION, BrokerClient, BrokerRequest, BrokerResponse, BrokerStatus,
    },
};

const BROKER_START_TIMEOUT: Duration = Duration::from_secs(5);
const BROKER_CONNECT_RETRY: Duration = Duration::from_millis(50);

pub async fn run(config: RuntimeConfig) -> Result<()> {
    prepare_state(&config).await?;

    let endpoint = broker_endpoint(&config)?;
    let listener = endpoint.listen()?;
    write_pid_file(&config).await?;
    let _runtime_files = RuntimeFileGuard::new(
        config.pid_path.clone(),
        endpoint.stale_path().map(Path::to_path_buf),
    );

    tracing::info!(
        cdp_endpoint = %config.cdp_endpoint,
        ipc_endpoint = %endpoint.display(),
        state_dir = %config.state_dir.display(),
        "visible browser broker listening"
    );

    serve(config, listener).await
}

pub async fn ensure_running(config: &RuntimeConfig) -> Result<BrokerClient> {
    prepare_state(config).await?;

    if let Ok(client) = connect_and_ping(config).await {
        return Ok(client);
    }

    let deadline = Instant::now() + BROKER_START_TIMEOUT;

    loop {
        if let Some(_lock) = BrokerStartLock::try_acquire(&config.lock_path)? {
            if let Ok(client) = connect_and_ping(config).await {
                return Ok(client);
            }

            cleanup_stale_endpoint(config)?;
            spawn_broker(config)?;
            return wait_for_broker(config, BROKER_START_TIMEOUT).await;
        }

        if let Ok(client) = wait_for_broker(config, Duration::from_millis(250)).await {
            return Ok(client);
        }

        if Instant::now() >= deadline {
            bail!(
                "timed out waiting for broker startup lock `{}`",
                config.lock_path.display()
            );
        }

        sleep(BROKER_CONNECT_RETRY).await;
    }
}

pub async fn prepare_state(config: &RuntimeConfig) -> Result<()> {
    tokio::fs::create_dir_all(&config.state_dir).await?;
    tokio::fs::create_dir_all(&config.log_dir).await?;
    Ok(())
}

pub fn cleanup_stale_endpoint(config: &RuntimeConfig) -> Result<StaleEndpointCleanup> {
    let endpoint = broker_endpoint(config)?;
    let Some(stale_path) = endpoint.stale_path() else {
        return Ok(StaleEndpointCleanup::NoFilesystemEndpoint);
    };

    if !stale_path.exists() {
        return Ok(StaleEndpointCleanup::NoEndpoint);
    }

    match read_pid(&config.pid_path)? {
        Some(pid) if process_is_alive(pid) => bail!(
            "broker IPC `{}` is unavailable but pid `{pid}` is still alive",
            endpoint.display()
        ),
        Some(_) => {
            fs::remove_file(stale_path).with_context(|| {
                format!(
                    "failed to remove stale broker endpoint `{}`",
                    endpoint.display()
                )
            })?;
            let _ = fs::remove_file(&config.pid_path);
            Ok(StaleEndpointCleanup::RemovedDeadPid)
        }
        None => {
            fs::remove_file(stale_path).with_context(|| {
                format!(
                    "failed to remove stale broker endpoint `{}`",
                    endpoint.display()
                )
            })?;
            Ok(StaleEndpointCleanup::RemovedWithoutPid)
        }
    }
}

async fn connect_and_ping(config: &RuntimeConfig) -> Result<BrokerClient> {
    let endpoint = broker_endpoint(config)?;
    let mut client = BrokerClient::connect(&endpoint).await?;
    client.ping().await?;
    Ok(client)
}

async fn wait_for_broker(config: &RuntimeConfig, timeout: Duration) -> Result<BrokerClient> {
    let deadline = Instant::now() + timeout;

    loop {
        match connect_and_ping(config).await {
            Ok(client) => return Ok(client),
            Err(error) if Instant::now() >= deadline => {
                return Err(error).with_context(|| {
                    format!(
                        "timed out waiting for broker socket `{}`",
                        config.ipc_endpoint
                    )
                });
            }
            Err(_) => {}
        }

        sleep(BROKER_CONNECT_RETRY).await;
    }
}

fn spawn_broker(config: &RuntimeConfig) -> Result<()> {
    let current_exe = std::env::current_exe().context("failed to locate current executable")?;
    let stdout = append_log_file(&config.log_dir.join("broker.stdout.log"))?;
    let stderr = append_log_file(&config.log_dir.join("broker.stderr.log"))?;

    let child = Command::new(current_exe)
        .arg("broker")
        .arg("--socket")
        .arg(&config.ipc_endpoint)
        .arg("--cdp-endpoint")
        .arg(&config.cdp_endpoint)
        .arg("--state-dir")
        .arg(&config.state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .context("failed to spawn visible browser broker")?;

    tracing::info!(
        pid = child.id(),
        ipc_endpoint = %config.ipc_endpoint,
        "spawned visible browser broker"
    );

    Ok(())
}

fn append_log_file(path: &Path) -> Result<File> {
    Ok(OpenOptions::new().create(true).append(true).open(path)?)
}

async fn write_pid_file(config: &RuntimeConfig) -> Result<()> {
    tokio::fs::write(&config.pid_path, std::process::id().to_string()).await?;
    Ok(())
}

async fn serve(config: RuntimeConfig, listener: BrokerListener) -> Result<()> {
    loop {
        let stream = ipc::accept(&listener).await?;
        let connection_config = config.clone();

        tokio::spawn(async move {
            if let Err(error) = handle_connection(connection_config, stream).await {
                tracing::warn!(error = %error, "broker connection failed");
            }
        });
    }
}

async fn handle_connection(config: RuntimeConfig, stream: BrokerStream) -> Result<()> {
    let mut stream = BufReader::new(stream);

    let mut line = String::new();
    loop {
        line.clear();
        let bytes = stream.read_line(&mut line).await?;
        if bytes == 0 {
            break;
        }

        let response = match serde_json::from_str::<BrokerRequest>(&line) {
            Ok(request) => dispatch_request(&config, request),
            Err(error) => BrokerResponse::invalid_input(
                String::new(),
                format!("invalid broker request JSON: {error}"),
            ),
        };
        let encoded = serde_json::to_string(&response)?;

        stream.get_mut().write_all(encoded.as_bytes()).await?;
        stream.get_mut().write_all(b"\n").await?;
        stream.get_mut().flush().await?;
    }

    Ok(())
}

fn dispatch_request(config: &RuntimeConfig, request: BrokerRequest) -> BrokerResponse {
    match request.method.as_str() {
        "ping" => {
            BrokerResponse::success(request.id, broker_status(config)).unwrap_or_else(|error| {
                BrokerResponse::error(
                    String::new(),
                    BrowserToolError::invalid_input(format!(
                        "failed to serialize broker status: {error}"
                    )),
                )
            })
        }
        method => {
            BrokerResponse::invalid_input(request.id, format!("unknown broker method `{method}`"))
        }
    }
}

fn broker_status(config: &RuntimeConfig) -> BrokerStatus {
    BrokerStatus {
        protocol_version: BROKER_PROTOCOL_VERSION,
        pid: std::process::id(),
        cdp_endpoint: config.cdp_endpoint.clone(),
        ipc_endpoint: config.ipc_endpoint.clone(),
        socket_path: config.socket_path.clone(),
    }
}

fn broker_endpoint(config: &RuntimeConfig) -> Result<BrokerEndpoint> {
    BrokerEndpoint::from_state(&config.state_dir, Some(&config.ipc_endpoint))
}

fn read_pid(path: &Path) -> Result<Option<u32>> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(contents.trim().parse::<u32>().ok()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read `{}`", path.display())),
    }
}

fn process_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }

    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if result == 0 {
            return true;
        }

        return std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    }

    #[cfg(windows)]
    {
        let filter = format!("PID eq {pid}");
        let Ok(output) = Command::new("tasklist")
            .args(["/FI", &filter, "/FO", "CSV", "/NH"])
            .output()
        else {
            return false;
        };

        if !output.status.success() {
            return false;
        }

        String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleEndpointCleanup {
    NoEndpoint,
    NoFilesystemEndpoint,
    RemovedWithoutPid,
    RemovedDeadPid,
}

struct BrokerStartLock {
    _file: File,
}

impl BrokerStartLock {
    fn try_acquire(lock_path: &Path) -> Result<Option<Self>> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(lock_path)
            .with_context(|| format!("failed to open broker lock `{}`", lock_path.display()))?;

        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(None),
            Err(error) => {
                Err(error).with_context(|| format!("failed to lock `{}`", lock_path.display()))
            }
        }
    }
}

impl Drop for BrokerStartLock {
    fn drop(&mut self) {
        let _ = self._file.unlock();
    }
}

struct RuntimeFileGuard {
    pid_path: PathBuf,
    stale_path: Option<PathBuf>,
}

impl RuntimeFileGuard {
    fn new(pid_path: PathBuf, stale_path: Option<PathBuf>) -> Self {
        Self {
            pid_path,
            stale_path,
        }
    }
}

impl Drop for RuntimeFileGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.pid_path);
        if let Some(stale_path) = &self.stale_path {
            let _ = fs::remove_file(stale_path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RuntimeConfig;

    fn test_config(state_dir: PathBuf) -> RuntimeConfig {
        RuntimeConfig::from_parts("http://127.0.0.1:9222".to_string(), state_dir).unwrap()
    }

    #[tokio::test]
    async fn prepare_state_creates_state_and_log_directories() {
        let tempdir = tempfile::tempdir().unwrap();
        let state_dir = tempdir.path().join("state");
        let config = test_config(state_dir.clone());

        prepare_state(&config).await.unwrap();

        assert!(state_dir.is_dir());
        assert!(state_dir.join("logs").is_dir());
    }

    #[tokio::test]
    async fn broker_protocol_responds_to_ping() {
        let tempdir = tempfile::tempdir().unwrap();
        let config = test_config(tempdir.path().join("state"));
        prepare_state(&config).await.unwrap();
        let endpoint = broker_endpoint(&config).unwrap();
        let listener = endpoint.listen().unwrap();
        let server = tokio::spawn(serve(config.clone(), listener));

        let mut client = BrokerClient::connect(&endpoint).await.unwrap();
        let status = client.ping().await.unwrap();

        assert_eq!(status.protocol_version, BROKER_PROTOCOL_VERSION);
        assert_eq!(status.cdp_endpoint, "http://127.0.0.1:9222");
        assert_eq!(status.ipc_endpoint, config.ipc_endpoint);

        server.abort();
    }

    #[tokio::test]
    async fn ensure_running_uses_existing_broker_socket() {
        let tempdir = tempfile::tempdir().unwrap();
        let config = test_config(tempdir.path().join("state"));
        prepare_state(&config).await.unwrap();
        let endpoint = broker_endpoint(&config).unwrap();
        let listener = endpoint.listen().unwrap();
        let server = tokio::spawn(serve(config.clone(), listener));

        let mut client = ensure_running(&config).await.unwrap();
        let status = client.ping().await.unwrap();

        assert_eq!(status.ipc_endpoint, config.ipc_endpoint);

        server.abort();
    }

    #[test]
    fn stale_socket_cleanup_removes_socket_when_pid_is_missing() {
        if cfg!(windows) {
            return;
        }

        let tempdir = tempfile::tempdir().unwrap();
        let config = test_config(tempdir.path().join("state"));
        fs::create_dir_all(&config.state_dir).unwrap();
        File::create(&config.socket_path).unwrap();

        let result = cleanup_stale_endpoint(&config).unwrap();

        assert_eq!(result, StaleEndpointCleanup::RemovedWithoutPid);
        assert!(!config.socket_path.exists());
    }

    #[test]
    fn stale_socket_cleanup_preserves_socket_when_pid_is_alive() {
        if cfg!(windows) {
            return;
        }

        let tempdir = tempfile::tempdir().unwrap();
        let config = test_config(tempdir.path().join("state"));
        fs::create_dir_all(&config.state_dir).unwrap();
        File::create(&config.socket_path).unwrap();
        fs::write(&config.pid_path, std::process::id().to_string()).unwrap();

        let error = cleanup_stale_endpoint(&config).unwrap_err();

        assert!(error.to_string().contains("still alive"));
        assert!(config.socket_path.exists());
    }
}
