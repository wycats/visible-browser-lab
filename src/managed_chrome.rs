use std::{
    ffi::OsString,
    fs::{File, OpenOptions},
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::Deserialize;
use tokio::time::{Instant, sleep};

use crate::{
    chrome::{ChromeInstallation, discover_chrome},
    config::{RuntimeConfig, RuntimeMode},
};

const CHROME_START_TIMEOUT: Duration = Duration::from_secs(15);
const CHROME_START_RETRY: Duration = Duration::from_millis(50);
const CHROME_PROFILE_RELEASE_TIMEOUT: Duration = Duration::from_secs(5);
const STARTUP_PAGE: &str = "data:text/html,<title>Visible%20Browser%20Lab</title>";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserLaunchMode {
    Visible,
    Headless,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedChrome {
    pub cdp_endpoint: String,
    pub reused: bool,
    pub executable: PathBuf,
    pub pid: Option<u32>,
}

pub async fn ensure_managed_chrome(
    config: &RuntimeConfig,
    launch_mode: BrowserLaunchMode,
) -> Result<ManagedChrome> {
    if config.runtime_mode != RuntimeMode::Managed {
        bail!("managed Chrome was requested for an external CDP runtime");
    }

    tokio::fs::create_dir_all(&config.state_dir)
        .await
        .with_context(|| format!("failed to create `{}`", config.state_dir.display()))?;
    tokio::fs::create_dir_all(&config.chrome_profile_dir)
        .await
        .with_context(|| {
            format!(
                "failed to create managed Chrome profile `{}`",
                config.chrome_profile_dir.display()
            )
        })?;
    tokio::fs::create_dir_all(&config.log_dir)
        .await
        .with_context(|| format!("failed to create `{}`", config.log_dir.display()))?;

    let _lock = acquire_launch_lock(&config.chrome_lock_path).await?;
    if let Some(endpoint) = healthy_active_endpoint(config).await {
        return Ok(ManagedChrome {
            cdp_endpoint: endpoint,
            reused: true,
            executable: config.chrome_path.clone().unwrap_or_default(),
            pid: None,
        });
    }
    wait_for_profile_release(config).await?;

    remove_stale_active_port(&config.devtools_active_port_path).await?;
    let installation = discover_chrome(config.chrome_path.as_deref())?;
    let pid = launch_chrome(config, &installation, launch_mode)?;

    let deadline = Instant::now() + CHROME_START_TIMEOUT;
    loop {
        if let Some(endpoint) = healthy_active_endpoint(config).await {
            return Ok(ManagedChrome {
                cdp_endpoint: endpoint,
                reused: false,
                executable: installation.executable,
                pid,
            });
        }
        if Instant::now() >= deadline {
            let diagnostics = startup_diagnostics(&config.log_dir);
            bail!(
                "managed Chrome `{}` did not expose DevToolsActivePort within {} seconds. Diagnostics: {}",
                installation.executable.display(),
                CHROME_START_TIMEOUT.as_secs(),
                diagnostics.trim()
            );
        }
        sleep(CHROME_START_RETRY).await;
    }
}

pub fn activate_managed_chrome(config: &RuntimeConfig) -> Result<()> {
    let installation = discover_chrome(config.chrome_path.as_deref())?;

    #[cfg(target_os = "macos")]
    {
        let application_bundle = installation.application_bundle.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "managed Chrome focus on macOS requires an application bundle; `{}` is not inside a .app bundle",
                installation.executable.display()
            )
        })?;
        let status = Command::new("/usr/bin/open")
            .arg("-a")
            .arg(application_bundle)
            .status()
            .context("failed to invoke the macOS application activator")?;
        if !status.success() {
            bail!("failed to activate `{}`", application_bundle.display());
        }
    }

    #[cfg(not(target_os = "macos"))]
    let _ = installation;

    Ok(())
}

async fn acquire_launch_lock(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("failed to open Chrome launch lock `{}`", path.display()))?;
    let deadline = Instant::now() + CHROME_START_TIMEOUT;
    loop {
        match file.try_lock_exclusive() {
            Ok(()) => return Ok(file),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    bail!(
                        "timed out waiting for Chrome launch lock `{}`",
                        path.display()
                    );
                }
                sleep(CHROME_START_RETRY).await;
            }
            Err(error) => {
                return Err(error).with_context(|| format!("failed to lock `{}`", path.display()));
            }
        }
    }
}

async fn healthy_active_endpoint(config: &RuntimeConfig) -> Option<String> {
    let active_port = tokio::fs::read_to_string(&config.devtools_active_port_path)
        .await
        .ok()?;
    let endpoint = parse_active_port(&active_port).ok()?;
    validate_endpoint(&endpoint).await.then_some(endpoint)
}

async fn wait_for_profile_release(config: &RuntimeConfig) -> Result<()> {
    let profile_lock = config.chrome_profile_dir.join("SingletonLock");
    if !path_entry_exists(&profile_lock).await? {
        return Ok(());
    }

    let deadline = Instant::now() + CHROME_PROFILE_RELEASE_TIMEOUT;
    loop {
        if !path_entry_exists(&profile_lock).await? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            tracing::warn!(
                path = %profile_lock.display(),
                "managed Chrome profile remained locked after its CDP endpoint stopped responding"
            );
            return Ok(());
        }
        sleep(CHROME_START_RETRY).await;
    }
}

async fn path_entry_exists(path: &Path) -> Result<bool> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error)
            .with_context(|| format!("failed to inspect Chrome profile lock `{}`", path.display())),
    }
}

fn parse_active_port(contents: &str) -> Result<String> {
    let port = contents
        .lines()
        .next()
        .context("DevToolsActivePort omitted the port")?
        .trim()
        .parse::<u16>()
        .context("DevToolsActivePort contained an invalid port")?;
    Ok(format!("http://127.0.0.1:{port}"))
}

#[derive(Debug, Deserialize)]
struct BrowserVersion {
    #[serde(rename = "Browser")]
    browser: String,
    #[serde(rename = "webSocketDebuggerUrl")]
    websocket_url: String,
}

async fn validate_endpoint(endpoint: &str) -> bool {
    let url = format!("{}/json/version", endpoint.trim_end_matches('/'));
    let Ok(response) = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .expect("static HTTP client configuration must be valid")
        .get(url)
        .send()
        .await
    else {
        return false;
    };
    if !response.status().is_success() {
        return false;
    }
    let Ok(version) = response.json::<BrowserVersion>().await else {
        return false;
    };
    !version.browser.is_empty() && !version.websocket_url.is_empty()
}

async fn remove_stale_active_port(path: &Path) -> Result<()> {
    match tokio::fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("failed to remove stale `{}`", path.display()))
        }
    }
}

fn launch_chrome(
    config: &RuntimeConfig,
    installation: &ChromeInstallation,
    launch_mode: BrowserLaunchMode,
) -> Result<Option<u32>> {
    let args = chrome_arguments(config, launch_mode);

    #[cfg(target_os = "macos")]
    if launch_mode == BrowserLaunchMode::Visible {
        launch_macos_background(config, installation, &args)?;
        return Ok(None);
    }

    launch_direct(config, installation, &args, launch_mode)
}

fn chrome_arguments(config: &RuntimeConfig, launch_mode: BrowserLaunchMode) -> Vec<OsString> {
    let mut args = vec![
        "--remote-debugging-port=0".into(),
        format!("--user-data-dir={}", config.chrome_profile_dir.display()).into(),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--disable-background-networking".into(),
        "--disable-component-update".into(),
        "--disable-sync".into(),
        "--enable-logging".into(),
        format!("--log-file={}", config.log_dir.join("chrome.log").display()).into(),
    ];
    if launch_mode == BrowserLaunchMode::Headless {
        args.push("--headless=new".into());
    }
    args.push(STARTUP_PAGE.into());
    args
}

#[cfg(target_os = "macos")]
fn launch_macos_background(
    config: &RuntimeConfig,
    installation: &ChromeInstallation,
    args: &[OsString],
) -> Result<()> {
    let application_bundle = installation.application_bundle.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "managed visible Chrome on macOS requires an application bundle; `{}` is not inside a .app bundle",
            installation.executable.display()
        )
    })?;
    let status = Command::new("/usr/bin/open")
        .arg("-g")
        .arg("-n")
        .arg("-a")
        .arg(application_bundle)
        .arg("--stdout")
        .arg(config.log_dir.join("chrome.stdout.log"))
        .arg("--stderr")
        .arg(config.log_dir.join("chrome.stderr.log"))
        .arg("--args")
        .args(args)
        .status()
        .context("failed to invoke the macOS background application launcher")?;
    if !status.success() {
        bail!(
            "macOS failed to launch `{}` in the background",
            application_bundle.display()
        );
    }
    Ok(())
}

fn launch_direct(
    config: &RuntimeConfig,
    installation: &ChromeInstallation,
    args: &[OsString],
    launch_mode: BrowserLaunchMode,
) -> Result<Option<u32>> {
    #[cfg(target_os = "windows")]
    if launch_mode == BrowserLaunchMode::Visible {
        return launch_windows_background(installation, args);
    }

    let stdout = append_log_file(&config.log_dir.join("chrome.stdout.log"))?;
    let stderr = append_log_file(&config.log_dir.join("chrome.stderr.log"))?;
    let mut command = Command::new(&installation.executable);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));

    #[cfg(target_os = "linux")]
    if std::env::var_os("CI").is_some() {
        command.arg("--disable-dev-shm-usage").arg("--no-sandbox");
    }

    let child = command.spawn().with_context(|| {
        format!(
            "failed to launch managed Chrome `{}`",
            installation.executable.display()
        )
    })?;
    tracing::info!(
        pid = child.id(),
        executable = %installation.executable.display(),
        mode = ?launch_mode,
        "launched managed Chrome"
    );
    Ok(Some(child.id()))
}

#[cfg(target_os = "windows")]
fn launch_windows_background(
    installation: &ChromeInstallation,
    args: &[OsString],
) -> Result<Option<u32>> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{
            CREATE_NEW_PROCESS_GROUP, CreateProcessW, PROCESS_INFORMATION, STARTF_USESHOWWINDOW,
            STARTUPINFOW,
        },
        UI::WindowsAndMessaging::SW_SHOWNOACTIVATE,
    };

    let application = installation
        .executable
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut command_line = std::iter::once(installation.executable.as_os_str())
        .chain(args.iter().map(OsString::as_os_str))
        .map(quote_windows_argument)
        .collect::<Vec<_>>()
        .join(" ")
        .encode_utf16()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        dwFlags: STARTF_USESHOWWINDOW,
        wShowWindow: SW_SHOWNOACTIVATE as u16,
        ..Default::default()
    };
    let mut process = PROCESS_INFORMATION::default();
    let created = unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            0,
            CREATE_NEW_PROCESS_GROUP,
            std::ptr::null(),
            std::ptr::null(),
            &mut startup,
            &mut process,
        )
    };
    if created == 0 {
        return Err(std::io::Error::last_os_error()).with_context(|| {
            format!(
                "failed to launch managed Chrome `{}` without activation",
                installation.executable.display()
            )
        });
    }
    let pid = process.dwProcessId;
    unsafe {
        CloseHandle(process.hThread);
        CloseHandle(process.hProcess);
    }
    Ok(Some(pid))
}

#[cfg(target_os = "windows")]
fn quote_windows_argument(argument: &std::ffi::OsStr) -> String {
    let argument = argument.to_string_lossy();
    if !argument.is_empty()
        && !argument
            .chars()
            .any(|character| character.is_whitespace() || character == '"')
    {
        return argument.into_owned();
    }

    let mut quoted = String::from("\"");
    let mut backslashes = 0;
    for character in argument.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                quoted.push_str(&"\\".repeat(backslashes));
                backslashes = 0;
                quoted.push(character);
            }
        }
    }
    quoted.push_str(&"\\".repeat(backslashes * 2));
    quoted.push('"');
    quoted
}

fn append_log_file(path: &Path) -> Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("failed to open `{}`", path.display()))
}

fn read_log_tail(path: &Path) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open `{}`", path.display()))?;
    let length = file.metadata()?.len();
    file.seek(SeekFrom::Start(length.saturating_sub(8192)))?;
    let mut tail = String::new();
    file.read_to_string(&mut tail)?;
    Ok(tail)
}

fn startup_diagnostics(log_dir: &Path) -> String {
    let mut diagnostics = Vec::new();
    for name in ["chrome.stderr.log", "chrome.log"] {
        let path = log_dir.join(name);
        if let Ok(tail) = read_log_tail(&path)
            && !tail.trim().is_empty()
        {
            diagnostics.push(format!("{name}:\n{}", tail.trim()));
        }
    }
    if diagnostics.is_empty() {
        "Chrome produced no startup diagnostics".to_string()
    } else {
        diagnostics.join("\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use visible_browser_lab_test_support::chrome_for_testing_executable;

    #[test]
    fn parses_dynamic_devtools_port() {
        assert_eq!(
            parse_active_port("49321\n/devtools/browser/test\n").unwrap(),
            "http://127.0.0.1:49321"
        );
    }

    #[test]
    fn rejects_invalid_devtools_port() {
        assert!(parse_active_port("not-a-port\n").is_err());
        assert!(parse_active_port("").is_err());
    }

    #[test]
    fn chrome_arguments_use_dynamic_port_and_managed_profile() {
        let config = RuntimeConfig::managed(PathBuf::from("/tmp/vbl"), None);
        let args = chrome_arguments(&config, BrowserLaunchMode::Visible);
        let args = args
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>();

        assert!(args.contains(&"--remote-debugging-port=0".into()));
        assert!(
            args.contains(
                &format!("--user-data-dir={}", config.chrome_profile_dir.display()).into()
            )
        );
        assert!(!args.contains(&"--headless=new".into()));
    }

    #[tokio::test]
    async fn waits_for_chrome_profile_lock_release_before_launch() {
        let state = tempfile::tempdir().unwrap();
        let config = RuntimeConfig::managed(state.path().to_path_buf(), None);
        tokio::fs::create_dir_all(&config.chrome_profile_dir)
            .await
            .unwrap();
        let profile_lock = config.chrome_profile_dir.join("SingletonLock");
        tokio::fs::write(&profile_lock, b"active").await.unwrap();

        let lock_to_release = profile_lock.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(100)).await;
            tokio::fs::remove_file(lock_to_release).await.unwrap();
        });

        wait_for_profile_release(&config).await.unwrap();
        assert!(!path_entry_exists(&profile_lock).await.unwrap());
    }

    #[test]
    fn startup_diagnostics_falls_back_to_chrome_log() {
        let logs = tempfile::tempdir().unwrap();
        std::fs::write(logs.path().join("chrome.log"), "chrome startup failure").unwrap();

        assert_eq!(
            startup_diagnostics(logs.path()),
            "chrome.log:\nchrome startup failure"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn headless_managed_lifecycle_launches_reuses_and_relaunches() {
        let state = tempfile::tempdir().unwrap();
        let executable = tokio::task::spawn_blocking(chrome_for_testing_executable)
            .await
            .unwrap()
            .unwrap();
        let config = RuntimeConfig::managed(state.path().to_path_buf(), Some(executable));

        let first = ensure_managed_chrome(&config, BrowserLaunchMode::Headless)
            .await
            .unwrap();
        assert!(!first.reused);
        let reused = ensure_managed_chrome(&config, BrowserLaunchMode::Headless)
            .await
            .unwrap();
        assert!(reused.reused);
        assert_eq!(reused.cdp_endpoint, first.cdp_endpoint);

        terminate_process(first.pid.expect("direct launch should return a pid"));
        wait_until_unhealthy(&first.cdp_endpoint).await;
        let replacement = ensure_managed_chrome(&config, BrowserLaunchMode::Headless)
            .await
            .unwrap();
        assert!(!replacement.reused);
        assert!(validate_endpoint(&replacement.cdp_endpoint).await);

        terminate_process(replacement.pid.expect("direct launch should return a pid"));
        wait_until_unhealthy(&replacement.cdp_endpoint).await;
    }

    #[cfg(unix)]
    fn terminate_process(pid: u32) {
        let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        assert_eq!(result, 0, "failed to terminate managed Chrome pid {pid}");
    }

    #[cfg(windows)]
    fn terminate_process(pid: u32) {
        let status = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status()
            .unwrap();
        assert!(
            status.success(),
            "failed to terminate managed Chrome pid {pid}"
        );
    }

    async fn wait_until_unhealthy(endpoint: &str) {
        let deadline = Instant::now() + CHROME_START_TIMEOUT;
        while validate_endpoint(endpoint).await {
            assert!(Instant::now() < deadline, "managed Chrome did not stop");
            sleep(Duration::from_millis(50)).await;
        }
    }
}
