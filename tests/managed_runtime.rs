use std::{path::PathBuf, time::Duration};

use anyhow::Result;
use serde_json::json;
use visible_browser_lab_test_support::McpClient;

#[test]
fn identity_free_managed_call_requires_a_session_without_launching_chrome() -> Result<()> {
    let state = tempfile::tempdir()?;
    let missing_chrome = state.path().join("missing-chrome");
    let mut client =
        McpClient::spawn_managed(&test_binary(), state.path(), &repo_root(), &missing_chrome)?;
    client.initialize("visible-browser-lab-managed-fallback")?;

    let error = client.call_tool("list_tabs", json!({}), Duration::from_secs(20), true)?;

    assert_eq!(
        error.get("code").and_then(|value| value.as_str()),
        Some("session_required"),
        "identity-free managed call returned an unexpected error: {error}"
    );
    assert!(
        !state
            .path()
            .join("chrome-profile/DevToolsActivePort")
            .exists(),
        "identity fallback must not launch managed Chrome"
    );
    client.shutdown();
    Ok(())
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn test_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_visible-browser-lab-mcp"))
}

#[cfg(target_os = "macos")]
mod macos {
    use std::{
        fs,
        net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream},
        path::{Path, PathBuf},
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant},
    };

    use anyhow::{Context, Result, bail};
    use serde_json::json;
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
                "action": "url",
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
                "source": "document.querySelector('#entry').focus()"
            }),
            Duration::from_secs(20),
            false,
        )?;
        client.call_tool(
            "type_text",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab.tab_id,
                "target": { "css": "#entry" },
                "text": "managed background text"
            }),
            Duration::from_secs(20),
            false,
        )?;
        client.call_tool(
            "console",
            json!({ "agent_session_id": session_id, "tab_id": tab.tab_id, "operation": "list" }),
            Duration::from_secs(20),
            false,
        )?;
        client.call_tool(
            "network",
            json!({ "agent_session_id": session_id, "tab_id": tab.tab_id, "operation": "list" }),
            Duration::from_secs(20),
            false,
        )?;
        assert_frontmost(&original_frontmost, "background browser actions")?;

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
        assert_frontmost(&original_frontmost, "background click")?;
        client.call_tool(
            "press_key",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab.tab_id,
                "target": { "css": "#entry" },
                "key": "Enter"
            }),
            Duration::from_secs(20),
            false,
        )?;
        assert_frontmost(&original_frontmost, "background press_key")?;
        let background_actions = client.call_tool(
            "evaluate",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab.tab_id,
                "source": "({ clicked: document.body.dataset.clicked, key: document.body.dataset.key })"
            }),
            Duration::from_secs(20),
            false,
        )?;
        let background_value = background_actions
            .get("value")
            .context("background action verification omitted value")?;
        if background_value
            .get("clicked")
            .and_then(|value| value.as_str())
            != Some("yes")
        {
            bail!("background click did not update the fixture page: {background_actions}");
        }
        match background_value.get("key").and_then(|value| value.as_str()) {
            Some("Enter" | "Unidentified") => {}
            _ => {
                bail!("background press_key did not update the fixture page: {background_actions}");
            }
        }
        let snapshot = client.call_tool(
            "snapshot",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab.tab_id,
                "mode": "meaningful"
            }),
            Duration::from_secs(20),
            false,
        )?;
        let tree = field_str(&snapshot, "tree")?;
        let closed_overlay_ref = snapshot_element_ref(&tree, "button \"Closed overlay Apply\"")?;
        client
            .call_tool(
                "click",
                json!({
                    "agent_session_id": session_id,
                    "tab_id": tab.tab_id,
                    "target": { "ref": closed_overlay_ref },
                    "observe": "none"
                }),
                Duration::from_secs(20),
                false,
            )
            .context("closed-overlay background click")?;
        assert_frontmost(&original_frontmost, "closed-overlay background click")?;
        client
            .call_tool(
                "press_key",
                json!({
                    "agent_session_id": session_id,
                    "tab_id": tab.tab_id,
                    "target": { "ref": closed_overlay_ref },
                    "key": "Enter",
                    "observe": "none"
                }),
                Duration::from_secs(20),
                false,
            )
            .context("closed-overlay background press_key")?;
        assert_frontmost(&original_frontmost, "closed-overlay background press_key")?;
        let closed_overlay_actions = client.call_tool(
            "evaluate",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab.tab_id,
                "source": "({ clicked: document.body.dataset.closedOverlayClicked, clickTrusted: document.body.dataset.closedOverlayClickTrusted, key: document.body.dataset.closedOverlayKey, keyTrusted: document.body.dataset.closedOverlayKeyTrusted })"
            }),
            Duration::from_secs(20),
            false,
        )?;
        assert_eq!(closed_overlay_actions["value"]["clicked"], "yes");
        assert_eq!(closed_overlay_actions["value"]["clickTrusted"], "true");
        assert_eq!(closed_overlay_actions["value"]["key"], "Enter");
        assert_eq!(closed_overlay_actions["value"]["keyTrusted"], "true");
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
        wait_until_unhealthy(&first_endpoint)?;

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
        assert_frontmost(&original_frontmost, "broker restart and browser reuse")?;

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
        wait_until_unhealthy(&replacement_endpoint)?;
        restarted.shutdown();
        drop(cleanup);
        Ok(())
    }

    #[test]
    #[ignore = "launches visible managed Chrome to verify final-window cleanup"]
    fn closing_all_managed_tabs_does_not_leave_replacement_windows() -> Result<()> {
        let original_frontmost = frontmost_application()?;
        let state = tempfile::Builder::new()
            .prefix("vbl-managed-close-")
            .tempdir_in("/tmp")?;
        let cleanup = Cleanup {
            state_dir: state.path().to_path_buf(),
            original_frontmost,
        };
        let chrome = chrome_for_testing_executable()?;
        let mut client =
            McpClient::spawn_managed(&test_binary(), state.path(), &repo_root(), &chrome)?;
        client.initialize("visible-browser-lab-managed-close")?;

        let session = client.call_tool(
            "start_session",
            json!({
                "label": "managed-close",
                "start_url": "about:blank",
                "focus": false
            }),
            Duration::from_secs(45),
            false,
        )?;
        let session_id = field_str(&session, "agent_session_id")?;
        let first = OpenTab::from_summary(
            &session_id,
            session.get("tab").context("start_session omitted tab")?,
        )?;
        let second_result = client.call_tool(
            "new_tab",
            json!({
                "agent_session_id": session_id,
                "url": "about:blank",
                "focus": false
            }),
            Duration::from_secs(20),
            false,
        )?;
        let second = OpenTab::from_summary(
            &session_id,
            second_result.get("tab").context("new_tab omitted tab")?,
        )?;
        let endpoint = read_active_endpoint(state.path())?;
        let managed_pid = read_managed_pid(state.path())?;

        client.call_tool(
            "close_tab",
            json!({ "agent_session_id": session_id, "tab_id": first.tab_id }),
            Duration::from_secs(20),
            false,
        )?;
        client.call_tool(
            "close_tab",
            json!({ "agent_session_id": session_id, "tab_id": second.tab_id }),
            Duration::from_secs(20),
            false,
        )?;
        wait_until_unhealthy(&endpoint)?;
        wait_until_process_exited(managed_pid)?;

        client.shutdown();
        drop(cleanup);
        Ok(())
    }

    #[test]
    #[ignore = "launches visible managed Chrome to verify beforeunload recovery"]
    fn beforeunload_accept_keeps_the_owned_tab_usable() -> Result<()> {
        let original_frontmost = frontmost_application()?;
        let state = tempfile::Builder::new()
            .prefix("vbl-managed-beforeunload-")
            .tempdir_in("/tmp")?;
        let cleanup = Cleanup {
            state_dir: state.path().to_path_buf(),
            original_frontmost,
        };
        let fixture = FixtureServer::start()?;
        let chrome = chrome_for_testing_executable()?;
        let mut client =
            McpClient::spawn_managed(&test_binary(), state.path(), &repo_root(), &chrome)?;
        client.initialize("visible-browser-lab-managed-beforeunload")?;

        let session = client.call_tool(
            "start_session",
            json!({
                "label": "managed-beforeunload",
                "start_url": fixture.url("/beforeunload"),
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
        let sibling_result = client.call_tool(
            "new_tab",
            json!({
                "agent_session_id": session_id,
                "url": fixture.url("/sibling"),
                "focus": false
            }),
            Duration::from_secs(20),
            false,
        )?;
        let sibling = OpenTab::from_summary(
            &session_id,
            sibling_result.get("tab").context("new_tab omitted tab")?,
        )?;
        client.call_tool(
            "evaluate",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab.tab_id,
                "source": "history.pushState(null, '', '/chat/pending'); window.__vblBeforeUnloadSideEffects = 0; localStorage.setItem('vbl-beforeunload-side-effects', '0'); window.__vblGuard = event => { window.__vblBeforeUnloadSideEffects += 1; localStorage.setItem('vbl-beforeunload-side-effects', String(Number(localStorage.getItem('vbl-beforeunload-side-effects') || '0') + 1)); event.preventDefault(); event.returnValue = ''; }; window.addEventListener('beforeunload', window.__vblGuard); window.__pending = new Promise(() => {}); true"
            }),
            Duration::from_secs(20),
            false,
        )?;
        client.call_tool(
            "click",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab.tab_id,
                "target": { "css": "#entry" },
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
                "target": { "css": "#entry" },
                "key": "Enter",
                "observe": "none"
            }),
            Duration::from_secs(20),
            false,
        )?;
        client
            .call_tool(
                "navigate",
                json!({
                    "agent_session_id": session_id,
                    "tab_id": tab.tab_id,
                    "action": "url",
                    "url": fixture.url("/dismissed-beforeunload"),
                    "wait_until": "none",
                    "timeout_ms": 5000,
                    "before_unload": "dismiss",
                    "observe": "none"
                }),
                Duration::from_secs(15),
                false,
            )
            .context("dismiss beforeunload navigation")?;
        let dismissed = client.call_tool(
            "evaluate",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab.tab_id,
                "source": "location.pathname"
            }),
            Duration::from_secs(10),
            false,
        )?;
        assert_eq!(
            dismissed.get("value").and_then(|value| value.as_str()),
            Some("/chat/pending")
        );
        client.call_tool(
            "evaluate",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab.tab_id,
                "source": "window.removeEventListener('beforeunload', window.__vblGuard); true"
            }),
            Duration::from_secs(10),
            false,
        )?;
        client.call_tool(
            "evaluate",
            json!({
                "agent_session_id": session_id,
                "tab_id": sibling.tab_id,
                "source": "history.pushState(null, '', '/chat/pending'); window.__vblBeforeUnloadSideEffects = 0; localStorage.setItem('vbl-beforeunload-side-effects', '0'); window.__vblGuard = event => { window.__vblBeforeUnloadSideEffects += 1; localStorage.setItem('vbl-beforeunload-side-effects', String(Number(localStorage.getItem('vbl-beforeunload-side-effects') || '0') + 1)); event.preventDefault(); event.returnValue = ''; }; window.addEventListener('beforeunload', window.__vblGuard); window.__pending = new Promise(() => {}); true"
            }),
            Duration::from_secs(10),
            false,
        )?;
        client.call_tool(
            "click",
            json!({
                "agent_session_id": session_id,
                "tab_id": sibling.tab_id,
                "target": { "css": "#entry" },
                "observe": "none"
            }),
            Duration::from_secs(20),
            false,
        )?;
        client.call_tool(
            "press_key",
            json!({
                "agent_session_id": session_id,
                "tab_id": sibling.tab_id,
                "target": { "css": "#entry" },
                "key": "Enter",
                "observe": "none"
            }),
            Duration::from_secs(20),
            false,
        )?;
        for invalid_wait_request in [
            json!({
                "agent_session_id": session_id,
                "tab_id": sibling.tab_id,
                "action": "url",
                "url": fixture.url("/invalid-wait"),
                "wait_until": "loaded",
                "timeout_ms": 5000,
                "before_unload": "accept",
                "observe": "none"
            }),
            json!({
                "agent_session_id": session_id,
                "tab_id": sibling.tab_id,
                "action": "reload",
                "wait_until": "loaded",
                "timeout_ms": 5000,
                "before_unload": "accept",
                "observe": "none"
            }),
        ] {
            let invalid_wait = client.call_tool(
                "navigate",
                invalid_wait_request,
                Duration::from_secs(10),
                true,
            )?;
            assert_eq!(
                invalid_wait.get("code").and_then(|value| value.as_str()),
                Some("invalid_input")
            );
        }
        let invalid_forward = client.call_tool(
            "navigate",
            json!({
                "agent_session_id": session_id,
                "tab_id": sibling.tab_id,
                "action": "forward",
                "wait_until": "none",
                "timeout_ms": 5000,
                "before_unload": "accept",
                "observe": "none"
            }),
            Duration::from_secs(10),
            true,
        )?;
        assert_eq!(
            invalid_forward.get("code").and_then(|value| value.as_str()),
            Some("invalid_input")
        );
        let guard_still_active = client.call_tool(
            "evaluate",
            json!({
                "agent_session_id": session_id,
                "tab_id": sibling.tab_id,
                "source": "(() => { const event = new Event('beforeunload', { cancelable: true }); window.dispatchEvent(event); return event.defaultPrevented; })()"
            }),
            Duration::from_secs(10),
            false,
        )?;
        assert_eq!(
            guard_still_active
                .get("value")
                .and_then(|value| value.as_bool()),
            Some(true),
            "a no-op history request must preserve the page's unload guard"
        );
        client.call_tool(
            "navigate",
            json!({
                "agent_session_id": session_id,
                "tab_id": sibling.tab_id,
                "action": "url",
                "url": fixture.url("/chat/pending#accepted-fragment"),
                "wait_until": "none",
                "timeout_ms": 5000,
                "before_unload": "accept",
                "observe": "none"
            }),
            Duration::from_secs(10),
            false,
        )?;
        let same_document_guard = client.call_tool(
            "evaluate",
            json!({
                "agent_session_id": session_id,
                "tab_id": sibling.tab_id,
                "source": "(() => { const sideEffects = window.__vblBeforeUnloadSideEffects; const event = new Event('beforeunload', { cancelable: true }); window.dispatchEvent(event); return { prevented: event.defaultPrevented, href: location.href, sideEffects, stash: Boolean(window.__io_github_wycats_visible_browser_lab_beforeunload_stash_v1) }; })()"
            }),
            Duration::from_secs(10),
            false,
        )?;
        assert_eq!(
            same_document_guard
                .get("value")
                .and_then(|value| value.get("prevented"))
                .and_then(|value| value.as_bool()),
            Some(true),
            "a same-document accepted navigation must restore the page's unload guard: {same_document_guard}"
        );
        let side_effects_before_cross_document = same_document_guard
            .get("value")
            .and_then(|value| value.get("sideEffects"))
            .and_then(|value| value.as_u64())
            .context("same-document guard result omitted sideEffects")?;
        assert_eq!(
            side_effects_before_cross_document, 1,
            "a same-document accept must not duplicate Chrome's own beforeunload side effect: {same_document_guard}"
        );
        let rejected_navigation = client.call_tool(
            "navigate",
            json!({
                "agent_session_id": session_id,
                "tab_id": sibling.tab_id,
                "action": "url",
                "url": "http://[",
                "wait_until": "none",
                "timeout_ms": 5000,
                "before_unload": "accept",
                "observe": "none"
            }),
            Duration::from_secs(10),
            true,
        )?;
        assert!(
            matches!(
                rejected_navigation
                    .get("code")
                    .and_then(|value| value.as_str()),
                Some("invalid_input" | "chrome_unavailable" | "operation_timeout")
            ),
            "malformed navigation unexpectedly succeeded: {rejected_navigation}"
        );
        let guard_restored = client.call_tool(
            "evaluate",
            json!({
                "agent_session_id": session_id,
                "tab_id": sibling.tab_id,
                "source": "(() => { const event = new Event('beforeunload', { cancelable: true }); window.dispatchEvent(event); return event.defaultPrevented; })()"
            }),
            Duration::from_secs(10),
            false,
        )?;
        assert_eq!(
            guard_restored
                .get("value")
                .and_then(|value| value.as_bool()),
            Some(true),
            "a rejected navigation must restore the current document's unload guard"
        );
        let side_effects_before_cross_document = client.call_tool(
            "evaluate",
            json!({
                "agent_session_id": session_id,
                "tab_id": sibling.tab_id,
                "source": "Number(localStorage.getItem('vbl-beforeunload-side-effects') || '0')"
            }),
            Duration::from_secs(10),
            false,
        )?;
        let side_effects_before_cross_document = side_effects_before_cross_document
            .get("value")
            .and_then(|value| value.as_u64())
            .context("pre-navigation side-effect count was not numeric")?;
        client
            .call_tool(
                "navigate",
                json!({
                    "agent_session_id": session_id,
                    "tab_id": sibling.tab_id,
                    "action": "url",
                    "url": fixture.url("/after-beforeunload"),
                    "wait_until": "none",
                    "timeout_ms": 10000,
                    "before_unload": "accept",
                    "observe": "none"
                }),
                Duration::from_secs(15),
                false,
            )
            .context("accept beforeunload navigation")?;
        let title = client.call_tool(
            "evaluate",
            json!({
                "agent_session_id": session_id,
                "tab_id": sibling.tab_id,
                "source": "document.title"
            }),
            Duration::from_secs(10),
            false,
        )?;
        assert_eq!(
            title.get("value").and_then(|value| value.as_str()),
            Some("VBL Fixture")
        );
        let unload_side_effects = client.call_tool(
            "evaluate",
            json!({
                "agent_session_id": session_id,
                "tab_id": sibling.tab_id,
                "source": "Number(localStorage.getItem('vbl-beforeunload-side-effects') || '0')"
            }),
            Duration::from_secs(10),
            false,
        )?;
        assert_eq!(
            unload_side_effects
                .get("value")
                .and_then(|value| value.as_u64()),
            Some(side_effects_before_cross_document + 1),
            "accepting a cross-document beforeunload must add exactly one persisted side effect: {unload_side_effects}"
        );

        client.call_tool(
            "close_tab",
            json!({ "agent_session_id": session_id, "tab_id": tab.tab_id }),
            Duration::from_secs(15),
            false,
        )?;
        client.call_tool(
            "close_tab",
            json!({ "agent_session_id": session_id, "tab_id": sibling.tab_id }),
            Duration::from_secs(15),
            false,
        )?;
        client.shutdown();
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

    fn snapshot_element_ref(tree: &str, marker: &str) -> Result<String> {
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
            .context("snapshot node omitted an element reference")?;
        let end = line[start..]
            .find(']')
            .map(|index| start + index)
            .context("snapshot element reference omitted closing bracket")?;
        Ok(line[start..end].to_string())
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

    fn read_managed_pid(state_dir: &Path) -> Result<i32> {
        if let Ok(lock) = fs::read_link(state_dir.join("chrome-profile/SingletonLock"))
            && let Some(pid) = lock
                .to_string_lossy()
                .rsplit_once('-')
                .and_then(|(_, pid)| pid.parse::<i32>().ok())
        {
            return Ok(pid);
        }

        fs::read_to_string(state_dir.join("managed-chrome.pid"))?
            .trim()
            .parse()
            .context("managed Chrome pid file contained an invalid pid")
    }

    #[test]
    fn managed_pid_falls_back_to_the_runtime_pid_file() -> Result<()> {
        let state = tempfile::tempdir()?;
        fs::write(state.path().join("managed-chrome.pid"), "4242\n")?;
        assert_eq!(read_managed_pid(state.path())?, 4242);
        Ok(())
    }

    fn wait_until_process_exited(pid: i32) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while unsafe { libc::kill(pid, 0) } == 0 {
            if Instant::now() >= deadline {
                bail!("managed Chrome pid {pid} remained alive after final-target close");
            }
            thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    }

    fn terminate_broker(state_dir: &Path) -> Result<()> {
        let pid = fs::read_to_string(state_dir.join("broker-v4.pid"))?
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
            let _ = Command::new("pkill")
                .args(["-TERM", "-f", &profile])
                .status();
            let deadline = Instant::now() + Duration::from_secs(2);
            while Command::new("pgrep")
                .args(["-f", &profile])
                .stdout(Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
                && Instant::now() < deadline
            {
                thread::sleep(Duration::from_millis(50));
            }
            let _ = Command::new("pkill")
                .args(["-KILL", "-f", &profile])
                .status();
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
