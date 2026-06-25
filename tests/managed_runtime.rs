#[cfg(target_os = "macos")]
mod macos {
    use std::{
        fs,
        net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream},
        path::{Path, PathBuf},
        process::Command,
        thread,
        time::{Duration, Instant},
    };

    use anyhow::{Context, Result, bail};
    use chromiumoxide::Browser;
    use futures_util::StreamExt;
    use serde_json::{Value, json};
    use visible_browser_lab_test_support::{
        FixtureServer, McpClient, OpenTab, chrome_for_testing_executable, field_str,
    };

    #[test]
    #[ignore = "launches one visible managed Chrome window to verify macOS focus behavior"]
    fn managed_launch_preserves_frontmost_application_and_reuses_browser() -> Result<()> {
        let original_frontmost = frontmost_application()?;
        let state = tempfile::Builder::new()
            .prefix("vbl-managed-")
            .tempdir_in("/tmp")?;
        let cleanup = Cleanup {
            state_dir: state.path().to_path_buf(),
            original_frontmost: original_frontmost.clone(),
        };
        let fixture = FixtureServer::start()?;
        let chrome = chrome_for_testing_executable()?;
        let mut client =
            McpClient::spawn_managed(&test_binary(), state.path(), &repo_root(), &chrome)?;
        client.initialize("visible-browser-lab-managed-visible")?;

        let session = client.call_tool(
            "start_session",
            json!({
                "label": "managed-visible",
                "start_url": fixture.url("/managed"),
                "focus": false
            }),
            Duration::from_secs(45),
            false,
        )?;
        let session_id = field_str(&session, "agent_session_id")?;
        let tab = OpenTab::from_summary(
            &session_id,
            session.get("tab").context("start_session omitted tab")?,
        )?;
        let first_endpoint = read_active_endpoint(state.path())?;
        assert_frontmost(&original_frontmost, "managed Chrome launch")?;

        client.call_tool(
            "navigate",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab.tab_id,
                "url": fixture.url("/managed-navigation"),
                "wait_until": "load"
            }),
            Duration::from_secs(20),
            false,
        )?;
        client.call_tool(
            "screenshot",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab.tab_id,
                "full_page": false
            }),
            Duration::from_secs(20),
            false,
        )?;
        client.call_tool(
            "evaluate",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab.tab_id,
                "expression": "document.querySelector('#entry').focus()"
            }),
            Duration::from_secs(20),
            false,
        )?;
        client.call_tool(
            "type_text",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab.tab_id,
                "text": "managed background text"
            }),
            Duration::from_secs(20),
            false,
        )?;
        client.call_tool(
            "console_messages",
            json!({ "agent_session_id": session_id, "tab_id": tab.tab_id }),
            Duration::from_secs(20),
            false,
        )?;
        client.call_tool(
            "network_events",
            json!({ "agent_session_id": session_id, "tab_id": tab.tab_id }),
            Duration::from_secs(20),
            false,
        )?;
        let focus_state = client.call_tool(
            "evaluate",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab.tab_id,
                "expression": "document.hasFocus() && document.visibilityState === 'visible'"
            }),
            Duration::from_secs(20),
            false,
        )?;
        if focus_state.get("value").and_then(Value::as_bool) != Some(false) {
            bail!("background tab reported trusted-input focus: {focus_state}");
        }
        assert_frontmost(&original_frontmost, "background browser actions")?;

        for (tool, params) in [
            (
                "click",
                json!({
                    "agent_session_id": session_id,
                    "tab_id": tab.tab_id,
                    "target": { "css": "#clicker" },
                    "observe": "none"
                }),
            ),
            (
                "press_key",
                json!({
                    "agent_session_id": session_id,
                    "tab_id": tab.tab_id,
                    "key": "Enter"
                }),
            ),
        ] {
            let error = client.call_tool(tool, params, Duration::from_secs(20), true)?;
            if field_str(&error, "code")? != "focus_required" {
                bail!("{tool} returned the wrong background-focus error: {error}");
            }
        }

        client.call_tool(
            "focus_tab",
            json!({ "agent_session_id": session_id, "tab_id": tab.tab_id }),
            Duration::from_secs(20),
            false,
        )?;
        thread::sleep(Duration::from_millis(300));
        let chrome_frontmost = frontmost_application()?;
        if chrome_frontmost.bundle_id == original_frontmost.bundle_id
            || !chrome_frontmost
                .name
                .to_ascii_lowercase()
                .contains("chrome")
        {
            bail!("focus_tab did not activate managed Chrome: {chrome_frontmost:?}");
        }
        client.call_tool(
            "click",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab.tab_id,
                "target": { "css": "#clicker" },
                "observe": "none"
            }),
            Duration::from_secs(20),
            false,
        )?;
        client.call_tool(
            "press_key",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab.tab_id,
                "key": "Enter"
            }),
            Duration::from_secs(20),
            false,
        )?;
        restore_frontmost_application(&original_frontmost)?;

        let focused_creation = client.call_tool(
            "new_tab",
            json!({
                "agent_session_id": session_id,
                "url": fixture.url("/focused-creation"),
                "focus": true
            }),
            Duration::from_secs(20),
            false,
        )?;
        let focused_creation_tab = OpenTab::from_summary(
            &session_id,
            focused_creation
                .get("tab")
                .context("focused new_tab omitted tab")?,
        )?;
        thread::sleep(Duration::from_millis(300));
        let chrome_frontmost = frontmost_application()?;
        if chrome_frontmost.bundle_id == original_frontmost.bundle_id
            || !chrome_frontmost
                .name
                .to_ascii_lowercase()
                .contains("chrome")
        {
            bail!("new_tab with focus=true did not activate managed Chrome: {chrome_frontmost:?}");
        }
        client.call_tool(
            "close_tab",
            json!({
                "agent_session_id": session_id,
                "tab_id": focused_creation_tab.tab_id
            }),
            Duration::from_secs(20),
            false,
        )?;
        restore_frontmost_application(&original_frontmost)?;

        client.call_tool(
            "close_tab",
            json!({ "agent_session_id": session_id, "tab_id": tab.tab_id }),
            Duration::from_secs(20),
            false,
        )?;

        client.shutdown();
        terminate_broker(state.path())?;
        let mut restarted =
            McpClient::spawn_managed(&test_binary(), state.path(), &repo_root(), &chrome)?;
        restarted.initialize("visible-browser-lab-managed-restart")?;
        let restarted_session = restarted.call_tool(
            "start_session",
            json!({ "label": "managed-restart" }),
            Duration::from_secs(30),
            false,
        )?;
        let restarted_session_id = field_str(&restarted_session, "agent_session_id")?;
        assert_eq!(read_active_endpoint(state.path())?, first_endpoint);
        assert_frontmost(&original_frontmost, "broker restart and browser reuse")?;

        close_browser(&first_endpoint)?;
        wait_until_unhealthy(&first_endpoint)?;
        let replacement = restarted.call_tool(
            "new_tab",
            json!({
                "agent_session_id": restarted_session_id,
                "url": fixture.url("/replacement"),
                "focus": false
            }),
            Duration::from_secs(45),
            false,
        )?;
        let replacement_tab = OpenTab::from_summary(
            &restarted_session_id,
            replacement
                .get("tab")
                .context("new_tab omitted replacement tab")?,
        )?;
        let replacement_endpoint = read_active_endpoint(state.path())?;
        if replacement_endpoint == first_endpoint {
            bail!("replacement managed Chrome reused the closed CDP endpoint");
        }
        assert_frontmost(&original_frontmost, "managed Chrome replacement")?;
        restarted.call_tool(
            "close_tab",
            json!({
                "agent_session_id": restarted_session_id,
                "tab_id": replacement_tab.tab_id
            }),
            Duration::from_secs(20),
            false,
        )?;
        close_browser(&replacement_endpoint)?;
        restarted.shutdown();
        drop(cleanup);
        Ok(())
    }

    #[derive(Debug, Clone)]
    struct FrontmostApplication {
        pid: u32,
        name: String,
        bundle_id: String,
    }

    fn frontmost_application() -> Result<FrontmostApplication> {
        let output = Command::new("osascript")
            .args([
                "-e",
                "tell application \"System Events\" to tell first application process whose frontmost is true to return (unix id as string) & \"|\" & name & \"|\" & bundle identifier",
            ])
            .output()?;
        if !output.status.success() {
            bail!(
                "frontmost application query failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        let result = String::from_utf8(output.stdout)?;
        let mut fields = result.trim().splitn(3, '|');
        let pid = fields.next().context("frontmost application omitted pid")?;
        let name = fields
            .next()
            .context("frontmost application omitted name")?;
        let bundle_id = fields
            .next()
            .context("frontmost application omitted bundle identifier")?;
        Ok(FrontmostApplication {
            pid: pid.parse()?,
            name: name.to_string(),
            bundle_id: bundle_id.to_string(),
        })
    }

    fn assert_frontmost(expected: &FrontmostApplication, operation: &str) -> Result<()> {
        let actual = frontmost_application()?;
        let _observed_process_ids = (expected.pid, actual.pid);
        if actual.bundle_id != expected.bundle_id {
            bail!("{operation} changed the frontmost application from {expected:?} to {actual:?}");
        }
        Ok(())
    }

    fn restore_frontmost_application(application: &FrontmostApplication) -> Result<()> {
        let bundle_id = application.bundle_id.replace('"', "\\\"");
        let script = format!(
            "tell application \"System Events\" to set frontmost of first application process whose bundle identifier is \"{}\" to true",
            bundle_id
        );
        let status = Command::new("osascript").args(["-e", &script]).status()?;
        if !status.success() {
            bail!("failed to restore frontmost application {application:?}");
        }
        thread::sleep(Duration::from_millis(300));
        Ok(())
    }

    fn read_active_endpoint(state_dir: &Path) -> Result<String> {
        let active = fs::read_to_string(state_dir.join("chrome-profile/DevToolsActivePort"))?;
        let port = active
            .lines()
            .next()
            .context("DevToolsActivePort omitted port")?
            .parse::<u16>()?;
        Ok(format!("http://127.0.0.1:{port}"))
    }

    fn terminate_broker(state_dir: &Path) -> Result<()> {
        let pid = fs::read_to_string(state_dir.join("broker-v2.pid"))?
            .trim()
            .parse::<i32>()?;
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while unsafe { libc::kill(pid, 0) } == 0 {
            if Instant::now() >= deadline {
                bail!("broker pid {pid} did not terminate");
            }
            thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    }

    fn close_browser(endpoint: &str) -> Result<()> {
        tokio::runtime::Runtime::new()?.block_on(async {
            let (mut browser, mut handler) = Browser::connect(endpoint).await?;
            let handler_task = tokio::spawn(async move {
                while let Some(result) = handler.next().await {
                    if result.is_err() {
                        break;
                    }
                }
            });
            let _ = browser.close().await;
            let _ = tokio::time::timeout(Duration::from_secs(5), handler_task).await;
            Ok::<(), anyhow::Error>(())
        })
    }

    fn wait_until_unhealthy(endpoint: &str) -> Result<()> {
        let port = endpoint
            .rsplit(':')
            .next()
            .context("endpoint omitted port")?
            .parse()?;
        let address = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        let deadline = Instant::now() + Duration::from_secs(5);
        while TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_ok() {
            if Instant::now() >= deadline {
                bail!("Chrome endpoint `{endpoint}` remained reachable after close");
            }
            thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    }

    struct Cleanup {
        state_dir: PathBuf,
        original_frontmost: FrontmostApplication,
    }

    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = terminate_broker(&self.state_dir);
            let profile = format!(
                "user-data-dir={}",
                self.state_dir.join("chrome-profile").display()
            );
            let _ = Command::new("pkill").args(["-f", &profile]).status();
            let _ = restore_frontmost_application(&self.original_frontmost);
        }
    }

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    fn test_binary() -> PathBuf {
        PathBuf::from(env!("CARGO_BIN_EXE_visible-browser-lab-mcp"))
    }
}
