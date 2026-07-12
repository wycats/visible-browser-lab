use std::{
    env,
    ffi::OsString,
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use chrome_for_testing_manager::{ChromeBinary, ChromeForTestingManager, VersionRequest};
use chromiumoxide::Browser;
use futures_util::StreamExt;
use serde_json::{Value, json};
use tempfile::TempDir;

pub const BINARY_NAME: &str = "visible-browser-lab-mcp";
pub const BROWSER_MODE_ENV: &str = "VISIBLE_BROWSER_LAB_TEST_BROWSER_MODE";
pub const CFT_CACHE_DIR_ENV: &str = "VISIBLE_BROWSER_LAB_CFT_CACHE_DIR";

static REAL_BROWSER_IN_USE: AtomicBool = AtomicBool::new(false);

struct RealBrowserPermit;

impl RealBrowserPermit {
    fn acquire() -> Self {
        loop {
            if REAL_BROWSER_IN_USE
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return Self;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for RealBrowserPermit {
    fn drop(&mut self) {
        REAL_BROWSER_IN_USE.store(false, Ordering::Release);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserMode {
    Headless,
    Visible,
}

impl BrowserMode {
    pub fn from_env() -> Result<Self> {
        match env::var(BROWSER_MODE_ENV) {
            Ok(value) if value.trim().is_empty() => Ok(Self::Headless),
            Ok(value) => match value.trim() {
                "headless" => Ok(Self::Headless),
                "visible" => Ok(Self::Visible),
                mode => bail!("{BROWSER_MODE_ENV} must be `headless` or `visible`, got `{mode}`"),
            },
            Err(env::VarError::NotPresent) => Ok(Self::Headless),
            Err(error) => Err(error).context(format!("failed to read {BROWSER_MODE_ENV}")),
        }
    }
}

pub struct RealBrowser {
    _exclusive: RealBrowserPermit,
    child: Child,
    profile_dir: TempDir,
    cdp_endpoint: String,
}

impl RealBrowser {
    pub fn launch_from_env() -> Result<Self> {
        Self::launch(BrowserMode::from_env()?)
    }

    pub fn launch(mode: BrowserMode) -> Result<Self> {
        let exclusive = RealBrowserPermit::acquire();
        let chrome_executable = chrome_for_testing_executable()?;
        let profile_dir = tempfile::Builder::new()
            .prefix("visible-browser-lab-cft-profile-")
            .tempdir()
            .context("failed to create Chrome for Testing profile directory")?;

        let mut command = Command::new(&chrome_executable);
        command
            .args(chrome_for_testing_arguments(profile_dir.path(), mode))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());

        let child = command.spawn().with_context(|| {
            format!(
                "failed to launch Chrome for Testing `{}`",
                chrome_executable.display()
            )
        })?;
        let cdp_endpoint = wait_for_devtools_endpoint(profile_dir.path())?;

        Ok(Self {
            _exclusive: exclusive,
            child,
            profile_dir,
            cdp_endpoint,
        })
    }

    pub fn cdp_endpoint(&self) -> &str {
        &self.cdp_endpoint
    }

    pub fn profile_dir(&self) -> &Path {
        self.profile_dir.path()
    }

    pub fn shutdown(&mut self) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for RealBrowser {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub fn chrome_for_testing_executable() -> Result<PathBuf> {
    static CHROME_FOR_TESTING_LOCK: Mutex<()> = Mutex::new(());
    let _guard = CHROME_FOR_TESTING_LOCK
        .lock()
        .map_err(|_| anyhow!("Chrome for Testing lock was poisoned"))?;
    let cache_dir = chrome_for_testing_cache_dir();
    fs::create_dir_all(&cache_dir)
        .with_context(|| format!("failed to create `{}`", cache_dir.display()))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to create Chrome for Testing runtime")?;
    runtime.block_on(async move {
        let manager = ChromeForTestingManager::new_with_cache_dir(cache_dir)
            .map_err(|error| anyhow!("{error:#}"))?;
        let selected = manager
            .resolve_version(VersionRequest::stable())
            .await
            .map_err(|error| anyhow!("{error:#}"))?;
        let mut packages = manager
            .download(&selected, &[ChromeBinary::Chrome])
            .await
            .map_err(|error| anyhow!("{error:#}"))?;
        let package = packages
            .pop()
            .context("Chrome for Testing manager returned no browser package")?;
        let executable = package.browser_executable().to_path_buf();
        prepare_chrome_for_testing_executable(&executable)?;
        Ok(executable)
    })
}

fn chrome_for_testing_arguments(profile_dir: &Path, mode: BrowserMode) -> Vec<OsString> {
    let mut args = vec![
        "--remote-debugging-port=0".into(),
        format!("--user-data-dir={}", profile_dir.display()).into(),
        "--no-first-run".into(),
        "--no-default-browser-check".into(),
        "--disable-background-networking".into(),
        "--disable-component-update".into(),
        "--disable-sync".into(),
    ];

    #[cfg(target_os = "macos")]
    args.push("--use-mock-keychain".into());

    if mode == BrowserMode::Headless {
        args.push("--headless=new".into());
    }
    if cfg!(target_os = "linux") {
        args.push("--disable-dev-shm-usage".into());
        if env::var_os("CI").is_some() {
            args.push("--no-sandbox".into());
        }
    }

    args.push("about:blank".into());
    args
}

fn prepare_chrome_for_testing_executable(executable: &Path) -> Result<()> {
    remove_macos_quarantine(executable)
}

#[cfg(target_os = "macos")]
fn remove_macos_quarantine(executable: &Path) -> Result<()> {
    let Some(bundle) = executable.ancestors().find(|ancestor| {
        ancestor
            .extension()
            .and_then(|extension| extension.to_str())
            == Some("app")
    }) else {
        return Ok(());
    };
    let output = Command::new("/usr/bin/xattr")
        .args(["-d", "-r", "com.apple.quarantine"])
        .arg(bundle)
        .output()
        .with_context(|| format!("failed to inspect quarantine on `{}`", bundle.display()))?;
    if output.status.success() || String::from_utf8_lossy(&output.stderr).contains("No such xattr")
    {
        return Ok(());
    }
    bail!(
        "failed to remove quarantine from Chrome for Testing `{}`: {}",
        bundle.display(),
        String::from_utf8_lossy(&output.stderr).trim()
    )
}

#[cfg(not(target_os = "macos"))]
fn remove_macos_quarantine(_executable: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chrome_for_testing_arguments_include_stable_profile_and_startup_flags() {
        let args =
            chrome_for_testing_arguments(Path::new("/tmp/vbl-cft-profile"), BrowserMode::Headless);
        let args = args
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>();

        assert!(args.contains(&"--remote-debugging-port=0".into()));
        assert!(args.contains(&"--headless=new".into()));
        assert!(args.contains(&"about:blank".into()));
        assert!(args.iter().any(|arg| arg.starts_with("--user-data-dir=")));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn chrome_for_testing_uses_mock_keychain_on_macos() {
        let args =
            chrome_for_testing_arguments(Path::new("/tmp/vbl-cft-profile"), BrowserMode::Visible);
        let args = args
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>();

        assert!(args.contains(&"--use-mock-keychain".into()));
    }

    /// Chrome opens speculative preconnect sockets that never send a request.
    /// A silent connection must not wedge the accept loop or block a real
    /// request that arrives while the silent socket is still open.
    #[test]
    fn fixture_server_survives_silent_preconnect_sockets() {
        let server = FixtureServer::start().expect("fixture server starts");
        let address = server.base_url.trim_start_matches("http://").to_string();

        // Open silent connections that send nothing, like Chrome preconnects.
        let _silent: Vec<TcpStream> = (0..3)
            .map(|_| TcpStream::connect(&address).expect("silent connect"))
            .collect();
        // Give the accept loop time to pick the silent sockets up first.
        thread::sleep(Duration::from_millis(100));

        let mut stream = TcpStream::connect(&address).expect("real connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read timeout");
        stream
            .write_all(b"GET /data.json HTTP/1.1\r\nHost: fixture\r\n\r\n")
            .expect("write request");
        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read response");

        assert!(
            response.contains(r#"{"ok":true}"#),
            "real request must be served while silent sockets are open: {response}"
        );
    }

    #[test]
    fn fixture_server_stop_rejects_requests_from_preconnected_sockets() {
        let mut server = FixtureServer::start().expect("fixture server starts");
        let address = server.base_url.trim_start_matches("http://").to_string();
        let mut stream = TcpStream::connect(&address).expect("preconnect");
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("read timeout");
        thread::sleep(Duration::from_millis(100));

        server.stop();
        let accepts_closed_connection = |error: &std::io::Error| {
            matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionAborted | std::io::ErrorKind::ConnectionReset
            )
        };
        let write_result = stream.write_all(b"GET /data.json HTTP/1.1\r\nHost: fixture\r\n\r\n");
        let mut response = String::new();
        let read_result = match write_result {
            Ok(()) => Some(stream.read_to_string(&mut response)),
            Err(error) => {
                assert!(
                    accepts_closed_connection(&error),
                    "stopped preconnect write must close cleanly or reset: {error}"
                );
                None
            }
        };

        assert!(
            !response.contains("200 OK"),
            "a stopped fixture must not serve an accepted preconnect: {response}"
        );
        if let Some(Err(error)) = read_result {
            assert!(
                accepts_closed_connection(&error),
                "stopped preconnect must close cleanly or reset: {error}"
            );
        }
    }

    #[test]
    fn fixture_server_restart_rebinds_after_silent_preconnect() {
        let mut server = FixtureServer::start().expect("fixture server starts");
        let address = server.base_url.trim_start_matches("http://").to_string();
        let _silent = TcpStream::connect(&address).expect("silent preconnect");
        thread::sleep(Duration::from_millis(100));

        server
            .restart()
            .expect("fixture server rebinds its original address");
        assert_eq!(server.base_url, format!("http://{address}"));
    }

    /// Requests split across multiple TCP segments must still be served; a
    /// single read may return before the full request line has arrived. The
    /// split lands mid-path so a truncated read parses the wrong path.
    #[test]
    fn fixture_server_reads_slowly_delivered_requests() {
        let server = FixtureServer::start().expect("fixture server starts");
        let address = server.base_url.trim_start_matches("http://").to_string();

        let mut stream = TcpStream::connect(&address).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read timeout");
        stream.write_all(b"GET /data.js").expect("write start");
        stream.flush().expect("flush");
        thread::sleep(Duration::from_millis(150));
        stream
            .write_all(b"on HTTP/1.1\r\nHost: fixture\r\n\r\n")
            .expect("write rest");

        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read response");
        assert!(
            response.contains(r#"{"ok":true}"#),
            "split request must be served the JSON fixture: {response}"
        );
    }

    /// A connection whose request bytes arrive after accept must still be
    /// served. Accepted sockets can inherit the listener's non-blocking flag,
    /// and an eager read that treats `WouldBlock` as EOF drops the connection.
    #[test]
    fn fixture_server_waits_for_late_request_bytes() {
        let server = FixtureServer::start().expect("fixture server starts");
        let address = server.base_url.trim_start_matches("http://").to_string();

        let mut stream = TcpStream::connect(&address).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read timeout");
        // Let the accept loop pick this connection up before any bytes exist.
        thread::sleep(Duration::from_millis(200));
        stream
            .write_all(b"GET /data.json HTTP/1.1\r\nHost: fixture\r\n\r\n")
            .expect("write request");

        let mut response = String::new();
        stream.read_to_string(&mut response).expect("read response");
        assert!(
            response.contains(r#"{"ok":true}"#),
            "late-arriving request must be served: {response}"
        );
    }
}

fn chrome_for_testing_cache_dir() -> PathBuf {
    if let Some(path) = env::var_os(CFT_CACHE_DIR_ENV) {
        return PathBuf::from(path);
    }

    env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target"))
        .join("chrome-for-testing")
}

fn wait_for_devtools_endpoint(profile_dir: &Path) -> Result<String> {
    let active_port = profile_dir.join("DevToolsActivePort");
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut last_error = None;
    while Instant::now() < deadline {
        match fs::read_to_string(&active_port) {
            Ok(contents) => {
                let parsed = contents
                    .lines()
                    .next()
                    .context("DevToolsActivePort did not contain a port")
                    .and_then(|port| {
                        port.trim()
                            .parse::<u16>()
                            .context("DevToolsActivePort contained an invalid port")
                    });
                match parsed {
                    Ok(port) => return Ok(format!("http://127.0.0.1:{port}")),
                    Err(error) => last_error = Some(error),
                }
            }
            Err(error) => last_error = Some(error.into()),
        }
        thread::sleep(Duration::from_millis(100));
    }

    match last_error {
        Some(error) => {
            Err(error).with_context(|| format!("timed out waiting for `{}`", active_port.display()))
        }
        None => bail!("timed out waiting for `{}`", active_port.display()),
    }
}

pub fn run_live_smoke(
    client: &mut McpClient,
    open_tabs: &mut Vec<OpenTab>,
    cdp_endpoint: Option<&str>,
    state_dir: Option<&Path>,
    allow_focus: bool,
) -> Result<SmokeSummary> {
    let _init = client.request(
        "initialize",
        json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {},
            "clientInfo": {
                "name": "visible-browser-lab-live-smoke",
                "version": "0.0.0"
            }
        }),
        Duration::from_secs(20),
    )?;
    client.notify("notifications/initialized", Value::Null)?;

    let tools = client.request("tools/list", json!({}), Duration::from_secs(20))?;
    let tool_names = advertised_tool_names(&tools)?;
    for expected in EXPECTED_TOOLS {
        if !tool_names.contains(expected) {
            bail!("MCP tool `{expected}` was not advertised; got {tool_names:?}");
        }
    }

    let first = client.call_tool(
        "start_session",
        json!({
            "label": "smoke-first",
            "start_url": data_url("VBL Smoke One", "VBL Smoke One"),
            "focus": allow_focus
        }),
        Duration::from_secs(45),
        false,
    )?;
    let first_session = field_str(&first, "agent_session_id")?;
    let first_tab = first
        .get("tab")
        .context("start_session omitted first tab")?;
    let first_open_tab = OpenTab::from_summary(&first_session, first_tab)?;
    open_tabs.push(first_open_tab.clone());

    let second = client.call_tool(
        "start_session",
        json!({
            "label": "smoke-second",
            "start_url": data_url("VBL Smoke Two", "VBL Smoke Two"),
            "focus": allow_focus
        }),
        Duration::from_secs(45),
        false,
    )?;
    let second_session = field_str(&second, "agent_session_id")?;
    let second_tab = second
        .get("tab")
        .context("start_session omitted second tab")?;
    let second_open_tab = OpenTab::from_summary(&second_session, second_tab)?;
    open_tabs.push(second_open_tab.clone());

    let owned = client.call_tool(
        "list_tabs",
        json!({ "agent_session_id": first_session }),
        Duration::from_secs(20),
        false,
    )?;
    let owned_tabs = owned
        .get("tabs")
        .and_then(Value::as_array)
        .context("owned list_tabs omitted tabs array")?;
    if !tabs_include_id(owned_tabs, &first_open_tab.tab_id) {
        bail!("owned list did not include the caller's tab");
    }
    if tabs_include_id(owned_tabs, &second_open_tab.tab_id) {
        bail!("owned list exposed a foreign tab_id");
    }

    let global = client.call_tool(
        "list_tabs",
        json!({
            "agent_session_id": first_session,
            "scope": "global_readonly"
        }),
        Duration::from_secs(20),
        false,
    )?;
    let groups = global
        .get("groups")
        .and_then(Value::as_array)
        .context("global list_tabs omitted groups array")?;
    let mut caller_tabs = 0;
    let mut foreign_owned_tabs = 0;
    for tab in groups
        .iter()
        .filter_map(|group| group.get("tabs").and_then(Value::as_array))
        .flat_map(|tabs| tabs.iter())
    {
        if tab
            .get("owned_by_caller")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            caller_tabs += 1;
        } else if tab.get("owner_display_id").is_some() {
            foreign_owned_tabs += 1;
            if tab.get("caller_tab_id").is_some() || tab.get("tab_id").is_some() {
                bail!("global_readonly exposed a foreign action handle");
            }
        }
    }
    if caller_tabs == 0 || foreign_owned_tabs == 0 {
        bail!("global_readonly did not show both caller-owned and foreign-owned tabs");
    }

    let fixture = FixtureServer::start()?;

    let new_tab = client.call_tool(
        "new_tab",
        json!({
            "agent_session_id": first_session,
            "url": data_url("VBL Smoke Three", "VBL Smoke Three"),
            "focus": allow_focus
        }),
        Duration::from_secs(45),
        false,
    )?;
    let new_tab = new_tab.get("tab").context("new_tab omitted tab")?;
    let mut transferable_tab = OpenTab::from_summary(&first_session, new_tab)?;
    open_tabs.push(transferable_tab.clone());

    let navigated = client.call_tool(
        "navigate",
        json!({
            "agent_session_id": first_session,
            "tab_id": transferable_tab.tab_id,
            "action": "url",
            "url": data_url("VBL Smoke Nav", "VBL Smoke Nav"),
            "timeout_ms": 10000
        }),
        Duration::from_secs(30),
        false,
    )?;
    if navigated.get("document_revision").is_none() {
        bail!("navigate omitted the active document revision");
    }

    let screenshot = client.call_tool(
        "screenshot",
        json!({
            "agent_session_id": first_session,
            "tab_id": transferable_tab.tab_id,
            "full_page": false
        }),
        Duration::from_secs(30),
        false,
    )?;
    if screenshot
        .pointer("/image/media_type")
        .and_then(Value::as_str)
        != Some("image/png")
    {
        bail!("screenshot returned a non-PNG mime type");
    }
    let screenshot_bytes = screenshot
        .pointer("/artifact/size_bytes")
        .and_then(Value::as_u64)
        .context("screenshot omitted artifact size")? as usize;

    client.call_tool(
        "navigate",
        json!({
            "agent_session_id": first_session,
            "tab_id": transferable_tab.tab_id,
            "action": "url",
            "url": fixture.url("/page"),
            "timeout_ms": 10000
        }),
        Duration::from_secs(30),
        false,
    )?;

    let evaluated = client.call_tool(
        "evaluate",
        json!({
            "agent_session_id": first_session,
            "tab_id": transferable_tab.tab_id,
            "source": "(async () => { console.log('vbl-console-ready'); await fetch('/data.json'); return { title: document.title, ready: true }; })()"
        }),
        Duration::from_secs(30),
        false,
    )?;
    if evaluated
        .get("value")
        .and_then(|value| value.get("ready"))
        .and_then(Value::as_bool)
        != Some(true)
    {
        bail!("evaluate did not return the expected JSON value: {evaluated}");
    }

    let snapshot = client.call_tool(
        "snapshot",
        json!({
            "agent_session_id": first_session,
            "tab_id": transferable_tab.tab_id,
            "mode": "meaningful"
        }),
        Duration::from_secs(20),
        false,
    )?;
    let tree = field_str(&snapshot, "tree")?;
    let button_ref = snapshot_ref(&tree, "button \"Click\"")?;
    let textbox_ref = snapshot_ref(&tree, "textbox \"Entry\"")?;
    let frame_textbox_ref = snapshot_ref(&tree, "textbox \"Frame value\"")?;
    let frame_button_ref = snapshot_ref(&tree, "button \"Frame click\"")?;

    let fill_result = client.call_tool(
        "fill",
        json!({
            "agent_session_id": first_session,
            "tab_id": transferable_tab.tab_id,
            "target": { "ref": textbox_ref },
            "value": "semantic fill",
            "observe": "diff"
        }),
        Duration::from_secs(20),
        false,
    )?;
    if fill_result
        .get("observation")
        .and_then(|value| value.get("mode"))
        .and_then(Value::as_str)
        != Some("diff")
    {
        bail!("fill did not return the requested accessibility diff: {fill_result}");
    }
    let filled = client.call_tool(
        "evaluate",
        json!({
            "agent_session_id": first_session,
            "tab_id": transferable_tab.tab_id,
            "source": "document.querySelector('#entry').value"
        }),
        Duration::from_secs(20),
        false,
    )?;
    if filled.get("value").and_then(Value::as_str) != Some("semantic fill") {
        bail!("fill did not update the referenced input: {filled}");
    }

    client.call_tool(
        "fill",
        json!({
            "agent_session_id": first_session,
            "tab_id": transferable_tab.tab_id,
            "target": { "ref": frame_textbox_ref },
            "value": "inside frame",
            "observe": "none"
        }),
        Duration::from_secs(20),
        false,
    )?;
    let frame_value = client.call_tool(
        "evaluate",
        json!({
            "agent_session_id": first_session,
            "tab_id": transferable_tab.tab_id,
            "source": "document.querySelector('iframe').contentDocument.querySelector('#frame-entry').value"
        }),
        Duration::from_secs(20),
        false,
    )?;
    if frame_value.get("value").and_then(Value::as_str) != Some("inside frame") {
        bail!("fill did not resolve the iframe element reference: {frame_value}");
    }

    client.call_tool(
        "click",
        json!({
            "agent_session_id": first_session,
            "tab_id": transferable_tab.tab_id,
            "target": { "ref": frame_button_ref },
            "observe": "none",
            "timeout_ms": 5000
        }),
        Duration::from_secs(20),
        false,
    )?;
    let frame_clicked = client.call_tool(
        "evaluate",
        json!({
            "agent_session_id": first_session,
            "tab_id": transferable_tab.tab_id,
            "source": "document.querySelector('iframe').contentDocument.body.dataset.clicked"
        }),
        Duration::from_secs(20),
        false,
    )?;
    if frame_clicked.get("value").and_then(Value::as_str) != Some("yes") {
        bail!(
            "click did not use the iframe element's top-level viewport coordinates: {frame_clicked}"
        );
    }

    client.call_tool(
        "click",
        json!({
            "agent_session_id": first_session,
            "tab_id": transferable_tab.tab_id,
            "target": { "ref": button_ref },
            "observe": "diff",
            "timeout_ms": 5000
        }),
        Duration::from_secs(20),
        false,
    )?;
    let clicked = client.call_tool(
        "evaluate",
        json!({
            "agent_session_id": first_session,
            "tab_id": transferable_tab.tab_id,
            "source": "document.body.dataset.clicked"
        }),
        Duration::from_secs(20),
        false,
    )?;
    if clicked.get("value").and_then(Value::as_str) != Some("yes") {
        bail!("click did not update the fixture page: {clicked}");
    }

    client.call_tool(
        "type_text",
        json!({
            "agent_session_id": first_session,
            "tab_id": transferable_tab.tab_id,
            "target": { "css": "#prompt" },
            "text": "Create a tiny counter app with plus and minus buttons."
        }),
        Duration::from_secs(20),
        false,
    )?;
    let submitted = client.call_tool(
        "click",
        json!({
            "agent_session_id": first_session,
            "tab_id": transferable_tab.tab_id,
            "target": { "css": "#submit" },
            "observe": "diff",
            "timeout_ms": 5000
        }),
        Duration::from_secs(20),
        false,
    )?;
    let action = submitted
        .get("action")
        .context("submit click result omitted action evidence")?;
    let delivery_mode = action
        .get("delivery_mode")
        .and_then(Value::as_str)
        .context("submit click evidence omitted delivery_mode")?;
    if !matches!(
        delivery_mode,
        "browser_protocol_input" | "semantic_dom_activation"
    ) {
        bail!("submit click reported unknown delivery mode: {submitted}");
    }
    let effect = action
        .get("effect")
        .context("submit click evidence omitted effect")?;
    let url_changed = effect
        .get("url_changed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let network_event_count = effect
        .get("network_event_count")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    if !url_changed && network_event_count == 0 {
        bail!("submit click evidence did not record URL or network effect: {submitted}");
    }
    let submit_state = client.call_tool(
        "evaluate",
        json!({
            "agent_session_id": first_session,
            "tab_id": transferable_tab.tab_id,
            "source": "({ submitted: document.body.dataset.submitted, prompt: document.querySelector('#prompt').textContent, url: location.pathname })"
        }),
        Duration::from_secs(20),
        false,
    )?;
    let submit_value = submit_state
        .get("value")
        .context("submit state omitted value")?;
    if submit_value.get("submitted").and_then(Value::as_str) != Some("yes")
        || submit_value.get("prompt").and_then(Value::as_str) != Some("")
        || submit_value.get("url").and_then(Value::as_str) != Some("/submitted")
    {
        bail!(
            "submit click did not produce the fixture form effect: action={action}; state={submit_state}"
        );
    }

    client.call_tool(
        "evaluate",
        json!({
            "agent_session_id": first_session,
            "tab_id": transferable_tab.tab_id,
            "source": "document.querySelector('#clicker').remove()"
        }),
        Duration::from_secs(20),
        false,
    )?;
    let removed = client.call_tool(
        "click",
        json!({
            "agent_session_id": first_session,
            "tab_id": transferable_tab.tab_id,
            "target": { "ref": button_ref },
            "observe": "none"
        }),
        Duration::from_secs(20),
        true,
    )?;
    if field_str(&removed, "code")? != "element_stale" {
        bail!("removed DOM node did not invalidate its element reference: {removed}");
    }

    client.call_tool(
        "fill",
        json!({
            "agent_session_id": first_session,
            "tab_id": transferable_tab.tab_id,
            "target": { "ref": textbox_ref },
            "value": "",
            "observe": "none"
        }),
        Duration::from_secs(20),
        false,
    )?;
    client.call_tool(
        "type_text",
        json!({
            "agent_session_id": first_session,
            "tab_id": transferable_tab.tab_id,
            "target": { "ref": textbox_ref },
            "text": "typed"
        }),
        Duration::from_secs(20),
        false,
    )?;
    client.call_tool(
        "press_key",
        json!({
            "agent_session_id": first_session,
            "tab_id": transferable_tab.tab_id,
            "key": "Enter"
        }),
        Duration::from_secs(20),
        false,
    )?;
    let typed = client.call_tool(
        "evaluate",
        json!({
            "agent_session_id": first_session,
            "tab_id": transferable_tab.tab_id,
            "source": "({ value: document.querySelector('#entry').value, key: document.body.dataset.key })"
        }),
        Duration::from_secs(20),
        false,
    )?;
    let typed_value = typed
        .get("value")
        .and_then(|value| value.get("value"))
        .and_then(Value::as_str);
    let pressed_key = typed
        .get("value")
        .and_then(|value| value.get("key"))
        .and_then(Value::as_str);
    let key_matches = matches!(pressed_key, Some("Enter" | "Unidentified"));
    if typed_value != Some("typed") || !key_matches {
        bail!("type_text or press_key did not update the fixture page: {typed}");
    }

    wait_for_console_message(
        client,
        &first_session,
        &transferable_tab.tab_id,
        "vbl-console-ready",
    )?;
    wait_for_network_event(
        client,
        &first_session,
        &transferable_tab.tab_id,
        "/data.json",
    )?;

    client.call_tool(
        "navigate",
        json!({
            "agent_session_id": first_session,
            "tab_id": transferable_tab.tab_id,
            "action": "url",
            "url": fixture.url("/page?revision=2"),
            "timeout_ms": 10000
        }),
        Duration::from_secs(30),
        false,
    )?;
    let stale = client.call_tool(
        "fill",
        json!({
            "agent_session_id": first_session,
            "tab_id": transferable_tab.tab_id,
            "target": { "ref": textbox_ref },
            "value": "stale",
            "observe": "none"
        }),
        Duration::from_secs(20),
        true,
    )?;
    if field_str(&stale, "code")? != "element_stale" {
        bail!("navigation did not invalidate the prior element reference: {stale}");
    }

    for tool in [
        "snapshot",
        "evaluate",
        "click",
        "fill",
        "type_text",
        "press_key",
        "console",
        "network",
    ] {
        let arguments = match tool {
            "evaluate" => json!({
                "agent_session_id": first_session,
                "tab_id": second_open_tab.tab_id,
                "source": "1 + 1"
            }),
            "snapshot" => json!({
                "agent_session_id": first_session,
                "tab_id": second_open_tab.tab_id
            }),
            "click" => json!({
                "agent_session_id": first_session,
                "tab_id": second_open_tab.tab_id,
                "target": { "css": "body" },
                "observe": "none"
            }),
            "fill" => json!({
                "agent_session_id": first_session,
                "tab_id": second_open_tab.tab_id,
                "target": { "ref": textbox_ref },
                "value": "x",
                "observe": "none"
            }),
            "type_text" => json!({
                "agent_session_id": first_session,
                "tab_id": second_open_tab.tab_id,
                "target": { "ref": textbox_ref },
                "text": "x"
            }),
            "press_key" => json!({
                "agent_session_id": first_session,
                "tab_id": second_open_tab.tab_id,
                "key": "Enter"
            }),
            "console" | "network" => json!({
                "agent_session_id": first_session,
                "tab_id": second_open_tab.tab_id,
                "operation": "list"
            }),
            _ => unreachable!(),
        };
        let error = client.call_tool(tool, arguments, Duration::from_secs(20), true)?;
        if field_str(&error, "code")? != "tab_not_owned" {
            bail!("foreign tab `{tool}` returned the wrong error: {error}");
        }
    }

    let ownership_error = client.call_tool(
        "focus_tab",
        json!({
            "agent_session_id": first_session,
            "tab_id": second_open_tab.tab_id
        }),
        Duration::from_secs(20),
        true,
    )?;
    if field_str(&ownership_error, "code")? != "tab_not_owned" {
        bail!("foreign tab focus returned the wrong error: {ownership_error}");
    }

    let owned_claim_error = client.call_tool(
        "claim_tab",
        json!({
            "agent_session_id": first_session,
            "target_id": second_open_tab.target_id
        }),
        Duration::from_secs(20),
        true,
    )?;
    if field_str(&owned_claim_error, "code")? != "target_owned" {
        bail!("owned target claim returned the wrong error: {owned_claim_error}");
    }

    let takeover = client.call_tool(
        "claim_tab",
        json!({
            "agent_session_id": first_session,
            "target_id": second_open_tab.target_id,
            "takeover": true,
            "user_instruction": "transfer this tab for real-browser validation"
        }),
        Duration::from_secs(30),
        false,
    )?;
    let takeover_tab = takeover.get("tab").context("takeover omitted tab")?;
    let takeover_open_tab = OpenTab::from_summary(&first_session, takeover_tab)?;
    if takeover_open_tab.tab_id == second_open_tab.tab_id {
        bail!("takeover reused the previous tab_id");
    }
    open_tabs.push(takeover_open_tab.clone());
    let old_tab_error = client.call_tool(
        "focus_tab",
        json!({
            "agent_session_id": second_session,
            "tab_id": second_open_tab.tab_id
        }),
        Duration::from_secs(20),
        true,
    )?;
    if field_str(&old_tab_error, "code")? != "unknown_tab" {
        bail!("old takeover tab_id returned the wrong error: {old_tab_error}");
    }
    remove_open_tab(open_tabs, &second_open_tab.tab_id);

    let missing = client.call_tool(
        "new_tab",
        json!({
            "agent_session_id": first_session,
            "url": data_url("VBL Smoke Missing", "VBL Smoke Missing"),
            "focus": allow_focus
        }),
        Duration::from_secs(45),
        false,
    )?;
    let missing_tab = missing.get("tab").context("new_tab omitted missing tab")?;
    let missing_open_tab = OpenTab::from_summary(&first_session, missing_tab)?;
    open_tabs.push(missing_open_tab.clone());
    let missing_close_endpoint = match cdp_endpoint {
        Some(cdp_endpoint) => cdp_endpoint.to_owned(),
        None => {
            let state_dir = state_dir.context(
                "live smoke needs a CDP endpoint or managed state dir for external target close",
            )?;
            managed_endpoint(state_dir).with_context(|| {
                format!(
                    "failed to discover managed Chrome endpoint from `{}` for external target close",
                    state_dir.display()
                )
            })?
        }
    };
    close_target_via_cdp(&missing_close_endpoint, &missing_open_tab.target_id)?;

    let missing_error = client.call_tool(
        "focus_tab",
        json!({
            "agent_session_id": first_session,
            "tab_id": missing_open_tab.tab_id
        }),
        Duration::from_secs(20),
        true,
    )?;
    if field_str(&missing_error, "code")? != "target_missing" {
        bail!("external close returned the wrong recovery error: {missing_error}");
    }

    let owned_after_missing = client.call_tool(
        "list_tabs",
        json!({ "agent_session_id": first_session }),
        Duration::from_secs(20),
        false,
    )?;
    let missing_is_listed = owned_after_missing
        .get("tabs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|tab| {
            tab.get("tab_id").and_then(Value::as_str) == Some(&missing_open_tab.tab_id)
                && tab.get("state").and_then(Value::as_str) == Some("missing")
        });
    if !missing_is_listed {
        bail!("owned list did not keep the externally closed tab as missing");
    }

    let release = client.call_tool(
        "release_tab",
        json!({
            "agent_session_id": first_session,
            "tab_id": transferable_tab.tab_id
        }),
        Duration::from_secs(20),
        false,
    )?;
    if !release
        .get("released")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        bail!("release_tab did not report released=true");
    }
    if release.get("leave_visible").and_then(Value::as_bool) != Some(false) {
        bail!("release_tab did not report leave_visible=false");
    }
    remove_open_tab(open_tabs, &transferable_tab.tab_id);

    let claimed = client.call_tool(
        "claim_tab",
        json!({
            "agent_session_id": second_session,
            "target_id": transferable_tab.target_id
        }),
        Duration::from_secs(30),
        false,
    )?;
    let claimed_tab = claimed.get("tab").context("claim_tab omitted tab")?;
    if field_str(claimed_tab, "target_id")? != transferable_tab.target_id {
        bail!("claim_tab returned the wrong target_id");
    }
    transferable_tab = OpenTab::from_summary(&second_session, claimed_tab)?;
    open_tabs.push(transferable_tab.clone());

    let closed = client.call_tool(
        "close_tab",
        json!({
            "agent_session_id": transferable_tab.session_id,
            "tab_id": transferable_tab.tab_id
        }),
        Duration::from_secs(30),
        false,
    )?;
    if !closed
        .get("closed")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        bail!("close_tab did not report closed=true");
    }
    remove_open_tab(open_tabs, &transferable_tab.tab_id);

    Ok(SmokeSummary {
        tool_count: tool_names.len(),
        screenshot_bytes,
        global_groups: groups.len(),
    })
}

pub const EXPECTED_TOOLS: &[&str] = &[
    "start_session",
    "list_tabs",
    "new_tab",
    "claim_tab",
    "release_tab",
    "focus_tab",
    "close_tab",
    "snapshot",
    "navigate",
    "wait_for",
    "click",
    "fill",
    "fill_form",
    "type_text",
    "press_key",
    "screenshot",
    "evaluate",
    "interact",
    "console",
    "network",
    "emulation",
    "performance",
    "audit",
    "memory",
    "screencast",
    "artifacts",
    "help",
];

pub fn advertised_tool_names(tools_response: &Value) -> Result<Vec<&str>> {
    Ok(tools_response
        .get("tools")
        .and_then(Value::as_array)
        .context("tools/list omitted tools array")?
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect())
}

pub fn cleanup_open_tabs(client: &mut McpClient, open_tabs: &mut Vec<OpenTab>) {
    for tab in std::mem::take(open_tabs).into_iter().rev() {
        let _ = client.call_tool(
            "close_tab",
            json!({
                "agent_session_id": tab.session_id,
                "tab_id": tab.tab_id
            }),
            Duration::from_secs(10),
            false,
        );
    }
}

pub fn wait_for_console_message(
    client: &mut McpClient,
    session_id: &str,
    tab_id: &str,
    expected: &str,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let result = client.call_tool(
            "console",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab_id,
                "operation": "list"
            }),
            Duration::from_secs(10),
            false,
        )?;
        if result
            .get("messages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|message| {
                message
                    .get("text")
                    .and_then(Value::as_str)
                    .is_some_and(|text| text.contains(expected))
            })
        {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }

    bail!("console list did not include `{expected}`");
}

pub fn wait_for_network_event(
    client: &mut McpClient,
    session_id: &str,
    tab_id: &str,
    expected_url_fragment: &str,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let result = client.call_tool(
            "network",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab_id,
                "operation": "list"
            }),
            Duration::from_secs(10),
            false,
        )?;
        if result
            .get("requests")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|event| {
                event
                    .get("url")
                    .and_then(Value::as_str)
                    .is_some_and(|url| url.contains(expected_url_fragment))
            })
        {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }

    bail!("network list did not include `{expected_url_fragment}`");
}

pub struct FixtureServer {
    address: std::net::SocketAddr,
    base_url: String,
    accepting: Arc<AtomicBool>,
    stop: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl FixtureServer {
    pub fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").context("failed to bind fixture server")?;
        Self::from_listener(listener)
    }

    fn from_listener(listener: TcpListener) -> Result<Self> {
        listener
            .set_nonblocking(true)
            .context("failed to configure fixture server")?;
        let address = listener
            .local_addr()
            .context("failed to read fixture server address")?;
        let accepting = Arc::new(AtomicBool::new(true));
        let accepting_connections = Arc::clone(&accepting);
        let (stop_tx, stop_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            let mut connection_threads = Vec::new();
            loop {
                if stop_rx.try_recv().is_ok() {
                    break;
                }

                match listener.accept() {
                    Ok((stream, _)) => {
                        // Handle each connection on its own thread. Chrome opens
                        // speculative preconnect sockets that never send a request;
                        // handling connections serially lets one silent socket wedge
                        // the accept loop and time out every navigation.
                        let accepting = Arc::clone(&accepting_connections);
                        connection_threads.push(thread::spawn(move || {
                            handle_fixture_connection(stream, accepting)
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(25));
                    }
                    Err(_) => break,
                }
            }

            // A stopped listener is not enough to make its address reusable on
            // every platform: accepted speculative connections can still own
            // the local port until their handlers exit. Join them before
            // restart() attempts to bind the same address.
            for connection_thread in connection_threads {
                let _ = connection_thread.join();
            }
        });

        Ok(Self {
            address,
            base_url: format!("http://{address}"),
            accepting,
            stop: Some(stop_tx),
            thread: Some(thread),
        })
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    pub fn stop(&mut self) {
        self.accepting.store(false, Ordering::Release);
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }

    pub fn restart(&mut self) -> Result<()> {
        self.stop();
        let replacement = Self::from_listener(
            TcpListener::bind(self.address).context("failed to rebind fixture server")?,
        )?;
        *self = replacement;
        Ok(())
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn handle_fixture_connection(mut stream: TcpStream, accepting: Arc<AtomicBool>) {
    // Accepted sockets inherit the listener's non-blocking flag on some
    // platforms; switch to blocking reads with a timeout so speculative
    // preconnect sockets that never send bytes release the thread.
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    if stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .is_err()
    {
        return;
    }

    let mut buffer = Vec::with_capacity(2048);
    let mut chunk = [0; 2048];
    let request = loop {
        match stream.read(&mut chunk) {
            Ok(0) => return,
            Ok(bytes) => {
                buffer.extend_from_slice(&chunk[..bytes]);
                if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                    break String::from_utf8_lossy(&buffer).into_owned();
                }
                if buffer.len() > 16 * 1024 {
                    return;
                }
            }
            Err(_) => return,
        }
    };
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");

    let (content_type, body) = match path {
        "/data.json" => ("application/json", r#"{"ok":true}"#.to_string()),
        "/frame" => (
            "text/html; charset=utf-8",
            r#"<!doctype html>
<title>Frame Fixture</title>
<label>Frame value <input id="frame-entry" /></label>
<button id="frame-click" onclick="document.body.dataset.clicked='yes'">Frame click</button>"#
                .to_string(),
        ),
        _ => (
            "text/html; charset=utf-8",
            r#"<!doctype html>
<title>VBL Fixture</title>
<button id="clicker" onclick="document.body.dataset.clicked='yes'; document.body.dataset.clickButton=event.button; document.body.dataset.clickShift=event.shiftKey; console.log('vbl-clicked')" ondblclick="document.body.dataset.doubleClicked='yes'">Click</button>
<label>Entry <input id="entry" /></label>
<label>Choice <select id="choice"><option value="one">One</option><option value="two">Two</option></select></label>
<label><input id="checked" type="checkbox" /> Enabled</label>
<div id="editable" contenteditable="true" aria-label="Editable value"></div>
<form id="prompt-form">
  <div id="prompt" contenteditable="true" role="textbox" aria-label="Prompt"></div>
  <button id="submit" type="submit" data-testid="prompt-form-send-button" data-prompt-primary-action="send-prompt">Send</button>
</form>
<button id="hover" onmouseenter="document.body.dataset.hovered='yes'">Hover target</button>
<img id="missing-alt" src="data:image/gif;base64,R0lGODlhAQABAIAAAAAAAP///ywAAAAAAQABAAACAUwAOw==" />
<div id="drag-source" role="button" tabindex="0" draggable="true">Drag source</div>
<div id="drop-target" role="button" tabindex="0" ondragover="event.preventDefault()" ondrop="event.preventDefault(); document.body.dataset.dropped='yes'">Drop target</div>
<label>Upload <input id="upload" type="file" /></label>
<div id="file-drop" role="button" tabindex="0" ondragover="event.preventDefault()" ondrop="event.preventDefault(); document.body.dataset.files=event.dataTransfer.files.length">File drop</div>
<div id="scroll-box" tabindex="0" style="height:40px;overflow:auto"><div style="height:200px">Scrollable content</div></div>
<button id="dialog" onclick="document.body.dataset.dialog=confirm('Continue?')">Dialog</button>
<iframe src="/frame" title="Embedded fixture"></iframe>
<script>
document.querySelector('#entry').addEventListener('keydown', (event) => {
  document.body.dataset.key = event.key;
});
document.querySelector('#drop-target').addEventListener('drop', () => document.body.dataset.dropped='yes');
document.querySelector('#prompt-form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const editor = document.querySelector('#prompt');
  document.body.dataset.submitStarted = 'yes';
  await fetch('/chat/api/chat', { method: 'POST', body: editor.textContent || '' });
  editor.textContent = '';
  document.body.dataset.submitted = 'yes';
  history.pushState({}, '', '/submitted');
});
</script>"#
                .to_string(),
        ),
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    if !accepting.load(Ordering::Acquire) {
        return;
    }
    let _ = stream.write_all(response.as_bytes());
}

#[derive(Debug, Clone)]
pub struct OpenTab {
    pub session_id: String,
    pub tab_id: String,
    pub target_id: String,
}

impl OpenTab {
    pub fn from_summary(session_id: &str, value: &Value) -> Result<Self> {
        Ok(Self {
            session_id: session_id.to_string(),
            tab_id: field_str(value, "tab_id")?,
            target_id: field_str(value, "target_id")?,
        })
    }
}

#[derive(Debug)]
pub struct SmokeSummary {
    pub tool_count: usize,
    pub screenshot_bytes: usize,
    pub global_groups: usize,
}

pub struct McpClient {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Receiver<String>,
    stderr: Receiver<String>,
    next_id: u64,
    state_dir: Option<PathBuf>,
}

/// Idle window for brokers spawned by test harnesses. Short enough that even
/// a SIGKILLed harness strands a broker for seconds rather than forever.
pub const TEST_BROKER_IDLE_TIMEOUT_SECS: &str = "2";

impl McpClient {
    pub fn spawn(binary: &Path, cdp_endpoint: &str, state_dir: &Path, root: &Path) -> Result<Self> {
        let mut command = Command::new(binary);
        command
            .arg("--cdp-endpoint")
            .arg(cdp_endpoint)
            .arg("--state-dir")
            .arg(state_dir)
            .current_dir(root);
        Self::spawn_command(command, binary, Some(state_dir))
    }

    pub fn spawn_with_state(binary: &Path, state_dir: &Path, root: &Path) -> Result<Self> {
        let mut command = Command::new(binary);
        command.arg("--state-dir").arg(state_dir).current_dir(root);
        Self::spawn_command(command, binary, Some(state_dir))
    }

    pub fn spawn_managed(
        binary: &Path,
        state_dir: &Path,
        root: &Path,
        chrome_path: &Path,
    ) -> Result<Self> {
        let mut command = Command::new(binary);
        command
            .arg("--state-dir")
            .arg(state_dir)
            .current_dir(root)
            .env("VISIBLE_BROWSER_LAB_CHROME_PATH", chrome_path);
        Self::spawn_command(command, binary, Some(state_dir))
    }

    pub fn spawn_managed_from_environment(
        binary: &Path,
        state_dir: &Path,
        root: &Path,
        chrome_path: &Path,
    ) -> Result<Self> {
        let mut command = Command::new(binary);
        command
            .current_dir(root)
            .env("VISIBLE_BROWSER_LAB_STATE_DIR", state_dir)
            .env("VISIBLE_BROWSER_LAB_CHROME_PATH", chrome_path);
        Self::spawn_command(command, binary, Some(state_dir))
    }

    fn spawn_command(
        mut command: Command,
        binary: &Path,
        state_dir: Option<&Path>,
    ) -> Result<Self> {
        // Test brokers expire on their own shortly after the harness stops
        // using them, even when no cleanup code runs at all.
        command.env(
            "VISIBLE_BROWSER_LAB_BROKER_IDLE_TIMEOUT_SECS",
            TEST_BROKER_IDLE_TIMEOUT_SECS,
        );
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to spawn `{}`", binary.display()))?;
        let stdin = child.stdin.take().context("failed to capture MCP stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("failed to capture MCP stdout")?;
        let stderr = child
            .stderr
            .take()
            .context("failed to capture MCP stderr")?;

        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout: read_lines(stdout),
            stderr: read_lines(stderr),
            next_id: 1,
            state_dir: state_dir.map(Path::to_path_buf),
        })
    }

    pub fn initialize(&mut self, client_name: &str) -> Result<()> {
        let _init = self.request(
            "initialize",
            json!({
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": {
                    "name": client_name,
                    "version": "0.0.0"
                }
            }),
            Duration::from_secs(20),
        )?;
        self.notify("notifications/initialized", Value::Null)
    }

    pub fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let mut message = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method
        });
        if !params.is_null() {
            message["params"] = params;
        }
        self.write_message(&message)?;

        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.stdout.recv_timeout(Duration::from_millis(100)) {
                Ok(line) => {
                    let value: Value = serde_json::from_str(&line)
                        .with_context(|| format!("MCP server wrote invalid JSON line `{line}`"))?;
                    if value.get("id").and_then(Value::as_u64) == Some(id) {
                        if let Some(error) = value.get("error") {
                            bail!(
                                "MCP request `{method}` failed: {error}; stderr: {}",
                                self.stderr_tail()
                            );
                        }
                        return Ok(value.get("result").cloned().unwrap_or(Value::Null));
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if let Some(status) = self.child.try_wait()? {
                        bail!(
                            "MCP server exited with {status}; stderr: {}",
                            self.stderr_tail()
                        );
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("MCP stdout closed; stderr: {}", self.stderr_tail());
                }
            }
        }

        bail!(
            "timed out waiting for MCP request `{method}`; stderr: {}",
            self.stderr_tail()
        )
    }

    pub fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let mut message = json!({
            "jsonrpc": "2.0",
            "method": method
        });
        if !params.is_null() {
            message["params"] = params;
        }
        self.write_message(&message)
    }

    pub fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
        timeout: Duration,
        expect_error: bool,
    ) -> Result<Value> {
        let result = self
            .request(
                "tools/call",
                json!({
                    "name": name,
                    "arguments": arguments
                }),
                timeout,
            )
            .with_context(|| format!("tool `{name}` request failed"))?;
        let is_error = result
            .get("isError")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if is_error != expect_error {
            bail!("tool `{name}` returned isError={is_error}, expected {expect_error}: {result}");
        }

        result
            .get("structuredContent")
            .or_else(|| result.get("structured_content"))
            .cloned()
            .with_context(|| format!("tool `{name}` omitted structured content: {result}"))
    }

    fn write_message(&mut self, message: &Value) -> Result<()> {
        let stdin = self.stdin.as_mut().context("MCP stdin is closed")?;
        serde_json::to_writer(&mut *stdin, message)?;
        stdin.write_all(b"\n")?;
        stdin.flush()?;
        Ok(())
    }

    fn stderr_tail(&self) -> String {
        let mut lines = Vec::new();
        while let Ok(line) = self.stderr.try_recv() {
            lines.push(line);
        }
        lines
            .into_iter()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn shutdown(&mut self) {
        let _ = self.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut exited = false;
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                exited = true;
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        if !exited {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        // Also stop the broker this client spawned. The broker's own idle
        // exit covers the SIGKILL case; this covers the ordinary case
        // without waiting out the idle window.
        if let Some(state_dir) = self.state_dir.take() {
            stop_broker(&state_dir);
        }
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn read_lines<R>(reader: R) -> Receiver<String>
where
    R: Read + Send + 'static,
{
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let reader = BufReader::new(reader);
        for line in reader.lines() {
            let Ok(line) = line else {
                break;
            };
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    receiver
}

pub fn stop_broker(state_dir: &Path) {
    let pid = [
        "broker-v4.pid",
        "broker-v3.pid",
        "broker-v2.pid",
        "broker.pid",
    ]
    .into_iter()
    .find_map(|name| {
        fs::read_to_string(state_dir.join(name))
            .ok()
            .and_then(|pid| pid.trim().parse::<u32>().ok())
    });
    let Some(pid) = pid else {
        return;
    };

    if cfg!(windows) {
        let _ = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output();
    } else {
        let _ = Command::new("kill").arg(pid.to_string()).output();
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let alive = if cfg!(windows) {
            Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}")])
                .output()
                .is_ok_and(|output| {
                    String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
                })
        } else {
            Command::new("kill")
                .args(["-0", &pid.to_string()])
                .output()
                .is_ok_and(|output| output.status.success())
        };
        if !alive {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

pub fn close_browser_via_cdp(endpoint: &str) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to create browser cleanup runtime")?;
    runtime.block_on(async {
        let (mut browser, mut handler) = Browser::connect(endpoint)
            .await
            .with_context(|| format!("failed to connect to managed Chrome at `{endpoint}`"))?;
        let handler_task = tokio::spawn(async move {
            while let Some(result) = handler.next().await {
                if result.is_err() {
                    break;
                }
            }
        });
        browser
            .close()
            .await
            .context("failed to close managed Chrome")?;
        let _ = tokio::time::timeout(Duration::from_secs(5), handler_task).await;
        Ok::<(), anyhow::Error>(())
    })?;

    let version_url = format!("{}/json/version", endpoint.trim_end_matches('/'));
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if reqwest::blocking::get(&version_url).is_err() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    bail!("managed Chrome endpoint `{endpoint}` remained reachable after Browser.close")
}

pub fn managed_endpoint(state_dir: &Path) -> Result<String> {
    let active_port = fs::read_to_string(state_dir.join("chrome-profile/DevToolsActivePort"))?;
    let port = active_port
        .lines()
        .next()
        .context("DevToolsActivePort omitted port")?
        .trim()
        .parse::<u16>()?;
    Ok(format!("http://127.0.0.1:{port}"))
}

pub fn close_target_via_cdp(cdp_endpoint: &str, target_id: &str) -> Result<()> {
    let close_url = format!(
        "{}/json/close/{target_id}",
        cdp_endpoint.trim_end_matches('/')
    );
    let response = reqwest::blocking::get(&close_url)
        .with_context(|| format!("failed to call Chrome close endpoint `{close_url}`"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        bail!("Chrome close endpoint failed for target `{target_id}` with status {status}: {body}");
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !target_is_listed(cdp_endpoint, target_id)? {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }

    bail!("Chrome target `{target_id}` stayed listed after close request");
}

pub fn target_is_listed(cdp_endpoint: &str, target_id: &str) -> Result<bool> {
    let list_url = format!("{}/json/list", cdp_endpoint.trim_end_matches('/'));
    let response = reqwest::blocking::get(&list_url)
        .with_context(|| format!("failed to call Chrome list endpoint `{list_url}`"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().unwrap_or_default();
        bail!("Chrome list endpoint failed with status {status}: {body}",);
    }

    let body = response
        .bytes()
        .with_context(|| format!("failed to read Chrome list endpoint body `{list_url}`"))?;
    let targets: Value = serde_json::from_slice(&body)
        .with_context(|| format!("Chrome list endpoint returned invalid JSON from `{list_url}`"))?;
    let targets = targets
        .as_array()
        .context("Chrome list endpoint did not return an array")?;
    Ok(targets
        .iter()
        .any(|target| target.get("id").and_then(Value::as_str) == Some(target_id)))
}

pub fn tabs_include_id(tabs: &[Value], tab_id: &str) -> bool {
    tabs.iter()
        .any(|tab| tab.get("tab_id").and_then(Value::as_str) == Some(tab_id))
}

pub fn remove_open_tab(open_tabs: &mut Vec<OpenTab>, tab_id: &str) {
    open_tabs.retain(|tab| tab.tab_id != tab_id);
}

pub fn field_str(value: &Value, field: &str) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .with_context(|| format!("missing string field `{field}` in {value}"))
}

fn snapshot_ref(tree: &str, marker: &str) -> Result<String> {
    let mut matches = tree.lines().filter(|line| line.contains(marker));
    let line = matches
        .next()
        .with_context(|| format!("snapshot omitted `{marker}`:\n{tree}"))?;
    if matches.next().is_some() {
        bail!("snapshot marker `{marker}` matched more than one node:\n{tree}");
    }
    let start = line
        .find("[ref=")
        .map(|index| index + 5)
        .with_context(|| format!("snapshot node `{marker}` omitted an element reference"))?;
    let end = line[start..]
        .find(']')
        .map(|index| start + index)
        .context("snapshot element reference omitted closing bracket")?;
    Ok(line[start..end].to_string())
}

pub fn data_url(title: &str, body: &str) -> String {
    let html = format!("<!doctype html><title>{title}</title><main>{body}</main>");
    format!("data:text/html,{}", percent_encode(&html))
}

fn percent_encode(input: &str) -> String {
    let mut output = String::new();
    for byte in input.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            output.push(byte as char);
        } else {
            output.push_str(&format!("%{byte:02X}"));
        }
    }
    output
}
