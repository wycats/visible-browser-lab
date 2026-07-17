use std::{
    env,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use proptest::{
    prelude::*,
    sample::select,
    strategy::{BoxedStrategy, Union},
    test_runner::{Config, FileFailurePersistence},
};
use proptest_state_machine::{ReferenceStateMachine, StateMachineTest};
use serde_json::{Value, json};
use tempfile::TempDir;
use visible_browser_lab_test_support::{
    EXPECTED_TOOLS, FixtureServer, McpClient, OpenTab, RealBrowser, cleanup_open_tabs,
    close_target_via_cdp, data_url, field_str, run_live_smoke, stop_broker, tabs_include_id,
};

const PROPERTY_BROWSER_TOOL_TIMEOUT: Duration = Duration::from_secs(60);
const PROPERTY_NAVIGATION_TIMEOUT_MS: u64 = 30_000;
const PROPERTY_NAVIGATION_ATTEMPTS: usize = 3;

#[test]
fn deterministic_real_browser_facade() -> Result<()> {
    let mut harness = BrowserMcpHarness::start("visible-browser-lab-deterministic", false)?;
    let mut open_tabs = Vec::new();
    let cdp_endpoint = harness.cdp_endpoint().to_string();
    let summary = run_live_smoke(
        harness.client_mut(),
        &mut open_tabs,
        Some(&cdp_endpoint),
        None,
        true,
    );
    cleanup_open_tabs(harness.client_mut(), &mut open_tabs);
    let summary = summary?;
    assert!(summary.tool_count >= EXPECTED_TOOLS.len());
    assert!(summary.screenshot_bytes > 1000);
    assert!(summary.global_groups > 0);
    Ok(())
}

#[test]
fn complete_v03_domain_surface() -> Result<()> {
    let mut harness = BrowserMcpHarness::start("visible-browser-lab-v03-domains", true)?;
    let workspace = harness.state_dir.path().to_path_buf();
    let start_url = harness.fixture.url("/page");
    let init_script_url = harness.fixture.url("/page?init-script=1");
    std::fs::write(workspace.join("upload.txt"), b"visible browser lab upload")?;
    let session = harness.client_mut().call_tool(
        "start_session",
        json!({
            "label":"v03-domain-surface",
            "start_url":"about:blank",
            "focus":true,
            "workspace_root":workspace
        }),
        Duration::from_secs(45),
        false,
    )?;
    let session_id = field_str(&session, "agent_session_id")?;
    let tab = OpenTab::from_summary(
        &session_id,
        session.get("tab").context("start_session omitted tab")?,
    )?;
    harness.client_mut().call_tool(
        "navigate",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"action":"url","url":start_url,"wait_until":"load"}),
        Duration::from_secs(20),
        false,
    )?;
    let snapshot = harness.client_mut().call_tool(
        "snapshot",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"mode":"meaningful"}),
        Duration::from_secs(20),
        false,
    )?;
    let tree = field_str(&snapshot, "tree")?;
    let select_ref = snapshot_element_ref(&tree, "combobox \"Choice\"")?;
    let click_ref = snapshot_element_ref(&tree, "button \"Click\"")?;
    let checkbox_ref = snapshot_element_ref(&tree, "checkbox \"Enabled\"")?;
    let hover_ref = snapshot_element_ref(&tree, "button \"Hover target\"")?;
    let drag_ref = snapshot_element_ref(&tree, "button \"Drag source\"")?;
    let drop_ref = snapshot_element_ref(&tree, "button \"Drop target\"")?;
    let file_drop_ref = snapshot_element_ref(&tree, "button \"File drop\"")?;
    let dialog_ref = snapshot_element_ref(&tree, "button \"Dialog\"")?;
    let iframe_ref = snapshot_element_ref(&tree, "Iframe \"Embedded fixture\"")?;
    let rooted_snapshot = harness.client_mut().call_tool(
        "snapshot",
        json!({
            "agent_session_id":session_id,
            "tab_id":tab.tab_id,
            "mode":"full",
            "root":{"ref":hover_ref},
            "include_bounds":true
        }),
        Duration::from_secs(20),
        false,
    )?;
    let rooted_tree = field_str(&rooted_snapshot, "tree")?;
    assert!(rooted_tree.contains("Hover target"));
    assert!(rooted_tree.contains("[bounds="));
    assert!(!rooted_tree.contains("combobox \"Choice\""));

    let targeted = harness.client_mut().call_tool(
        "evaluate",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"target":{"ref":hover_ref},"source":"this.id","mode":"expression"}),
        Duration::from_secs(20),
        false,
    )?;
    assert_eq!(targeted["value"], "hover");
    let targeted_function = harness.client_mut().call_tool(
        "evaluate",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"target":{"css":"#hover"},"source":"function(suffix){ return this.id + suffix; }","mode":"function","args":["-target"]}),
        Duration::from_secs(20),
        false,
    )?;
    assert_eq!(targeted_function["value"], "hover-target");
    harness.client_mut().call_tool(
        "fill",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"target":{"css":"#frame-entry","frame_ref":iframe_ref},"value":"framed CSS","observe":"none"}),
        Duration::from_secs(20),
        false,
    )?;
    let framed_value = harness.client_mut().call_tool(
        "evaluate",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"target":{"css":"#frame-entry","frame_ref":iframe_ref},"source":"this.value"}),
        Duration::from_secs(20),
        false,
    )?;
    assert_eq!(framed_value["value"], "framed CSS");
    harness.client_mut().call_tool(
        "click",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"target":{"ref":click_ref},"button":"left","count":2,"modifiers":["shift"],"observe":"none"}),
        Duration::from_secs(20),
        false,
    )?;
    let click_state = harness.client_mut().call_tool(
        "evaluate",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"source":"({button:document.body.dataset.clickButton,shift:document.body.dataset.clickShift,double:document.body.dataset.doubleClicked})"}),
        Duration::from_secs(20),
        false,
    )?;
    assert_eq!(click_state["value"]["button"], "0");
    assert_eq!(click_state["value"]["shift"], "true");
    assert_eq!(click_state["value"]["double"], "yes");

    for format in ["jpeg", "webp"] {
        let screenshot = harness.client_mut().call_tool(
            "screenshot",
            json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"target":{"ref":hover_ref},"format":format,"quality":70}),
            Duration::from_secs(20),
            false,
        )?;
        assert!(screenshot["width"].as_u64().unwrap_or(0) > 0);
        assert!(screenshot["height"].as_u64().unwrap_or(0) > 0);
    }

    for arguments in [
        json!({"operation":"select_options","target":{"ref":select_ref},"values":["two"],"observe":"none"}),
        json!({"operation":"set_checked","target":{"ref":checkbox_ref},"checked":true,"observe":"none"}),
        json!({"operation":"hover","target":{"css":"#hover"},"observe":"none"}),
        json!({"operation":"drag","source":{"ref":drag_ref},"destination":{"ref":drop_ref},"observe":"none"}),
        json!({"operation":"drop","target":{"ref":file_drop_ref},"paths":["upload.txt"],"data":{"text/plain":"fixture"},"observe":"none"}),
        json!({"operation":"upload_files","target":{"css":"#upload"},"paths":["upload.txt"],"observe":"none"}),
        json!({"operation":"scroll","target":{"css":"#scroll-box"},"delta_y":120,"observe":"none"}),
    ] {
        let mut arguments = arguments.as_object().cloned().unwrap();
        arguments.insert("agent_session_id".to_string(), json!(session_id));
        arguments.insert("tab_id".to_string(), json!(tab.tab_id));
        harness.client_mut().call_tool(
            "interact",
            Value::Object(arguments),
            Duration::from_secs(20),
            false,
        )?;
    }
    harness.client_mut().call_tool(
        "focus_tab",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id}),
        Duration::from_secs(20),
        false,
    )?;
    harness.client_mut().call_tool(
        "interact",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"operation":"click_at","x":2,"y":2,"button":"left","count":1,"observe":"none"}),
        Duration::from_secs(20),
        false,
    )?;
    harness.client_mut().call_tool(
        "fill_form",
        json!({
            "agent_session_id":session_id,
            "tab_id":tab.tab_id,
            "fields":[
                {"kind":"select","target":{"css":"#choice"},"values":["one"]},
                {"kind":"checked","target":{"css":"#checked"},"checked":false}
            ],
            "observe":"none"
        }),
        Duration::from_secs(20),
        false,
    )?;
    harness.client_mut().call_tool(
        "click",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"target":{"ref":dialog_ref},"observe":"none"}),
        Duration::from_secs(20),
        false,
    )?;
    harness.client_mut().call_tool(
        "interact",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"operation":"handle_dialog","action":"accept","observe":"none"}),
        Duration::from_secs(20),
        false,
    )?;

    harness.client_mut().call_tool(
        "evaluate",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"source":"(async()=>{console.error('v03-console');await fetch('/data.json');return true})()"}),
        Duration::from_secs(20),
        false,
    )?;
    let console = harness.client_mut().call_tool(
        "console",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"operation":"list","levels":["error"]}),
        Duration::from_secs(20),
        false,
    )?;
    let message_id = console["messages"][0]["message_id"]
        .as_str()
        .context("console list omitted message id")?;
    harness.client_mut().call_tool(
        "console",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"operation":"get","message_id":message_id}),
        Duration::from_secs(20),
        false,
    )?;
    harness.client_mut().call_tool(
        "console",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"operation":"clear"}),
        Duration::from_secs(20),
        false,
    )?;
    let network = harness.client_mut().call_tool(
        "network",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"operation":"list","url_pattern":"data\\.json"}),
        Duration::from_secs(20),
        false,
    )?;
    let request_id = network["requests"][0]["request_id"]
        .as_str()
        .context("network list omitted request id")?;
    harness.client_mut().call_tool(
        "network",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"operation":"get","request_id":request_id,"include_response_body":true}),
        Duration::from_secs(20),
        false,
    )?;
    harness.client_mut().call_tool(
        "network",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"operation":"clear"}),
        Duration::from_secs(20),
        false,
    )?;

    let baseline_user_agent = harness.client_mut().call_tool(
        "evaluate",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"source":"navigator.userAgent"}),
        Duration::from_secs(20),
        false,
    )?["value"]
        .clone();
    for arguments in [
        json!({"operation":"set_viewport","width":800,"height":600,"device_scale_factor":1,"mobile":false,"touch":false}),
        json!({"operation":"set_network","preset":"offline"}),
        json!({"operation":"set_cpu","slowdown":2}),
        json!({"operation":"set_geolocation","latitude":37.77,"longitude":-122.42,"accuracy_meters":10}),
        json!({"operation":"set_media","media":"screen","color_scheme":"dark","reduced_motion":"reduce"}),
        json!({"operation":"set_user_agent","user_agent":"VisibleBrowserLab/0.3","platform":"test"}),
        json!({"operation":"set_headers","headers":{"x-visible-browser-lab":"true"}}),
        json!({"operation":"reset"}),
    ] {
        let mut arguments = arguments.as_object().cloned().unwrap();
        arguments.insert("agent_session_id".to_string(), json!(session_id));
        arguments.insert("tab_id".to_string(), json!(tab.tab_id));
        harness.client_mut().call_tool(
            "emulation",
            Value::Object(arguments),
            Duration::from_secs(20),
            false,
        )?;
    }
    let reset_user_agent = harness.client_mut().call_tool(
        "evaluate",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"source":"navigator.userAgent"}),
        Duration::from_secs(20),
        false,
    )?;
    assert_eq!(reset_user_agent["value"], baseline_user_agent);

    harness.client_mut().call_tool(
        "performance",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"operation":"start_trace"}),
        Duration::from_secs(20),
        false,
    )?;
    harness.client_mut().call_tool(
        "evaluate",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"source":"(()=>{const end=performance.now()+60;while(performance.now()<end){};return true})()"}),
        Duration::from_secs(20),
        false,
    )?;
    let stopped = harness.client_mut().call_tool(
        "performance",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"operation":"stop_trace"}),
        Duration::from_secs(30),
        false,
    )?;
    let trace_id = field_str(&stopped["artifact"], "artifact_id")?;
    harness.client_mut().call_tool(
        "performance",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"operation":"vitals"}),
        Duration::from_secs(20),
        false,
    )?;
    harness.client_mut().call_tool(
        "performance",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"operation":"analyze","artifact_id":trace_id,"insight":"long_tasks"}),
        Duration::from_secs(20),
        false,
    )?;

    harness.client_mut().call_tool(
        "emulation",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"operation":"set_viewport","width":777,"height":555,"device_scale_factor":1,"mobile":false,"touch":false}),
        Duration::from_secs(20),
        false,
    )?;
    let audit = harness.client_mut().call_tool(
        "audit",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"operation":"run","categories":["accessibility","seo","best_practices","agentic_browsing"],"mode":"navigation","device":"mobile"}),
        Duration::from_secs(30),
        false,
    )?;
    assert!(audit["findings"].as_array().is_some_and(|findings| {
        findings.iter().any(|finding| {
            finding["refs"]
                .as_array()
                .is_some_and(|refs| !refs.is_empty())
        })
    }));
    let restored_viewport = harness.client_mut().call_tool(
        "evaluate",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"source":"({width:innerWidth,height:innerHeight})"}),
        Duration::from_secs(20),
        false,
    )?;
    assert_eq!(restored_viewport["value"]["width"], 777);
    assert_eq!(restored_viewport["value"]["height"], 555);
    harness.client_mut().call_tool(
        "emulation",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"operation":"reset"}),
        Duration::from_secs(20),
        false,
    )?;
    let audit_id = field_str(&audit["reports"][0], "artifact_id")?;

    let capture = harness.client_mut().call_tool(
        "memory",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"operation":"capture"}),
        Duration::from_secs(60),
        false,
    )?;
    let heap_id = field_str(&capture["artifact"], "artifact_id")?;
    let summary = harness.client_mut().call_tool(
        "memory",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"operation":"summary","artifact_id":heap_id}),
        Duration::from_secs(60),
        false,
    )?;
    let root_node = field_str(&summary["data"]["root"], "node_id")?;
    for arguments in [
        json!({"operation":"classes","artifact_id":heap_id,"limit":5}),
        json!({"operation":"node","artifact_id":heap_id,"node_id":root_node}),
        json!({"operation":"dominators","artifact_id":heap_id,"node_id":root_node,"limit":5}),
        json!({"operation":"retainers","artifact_id":heap_id,"node_id":root_node,"limit":5}),
        json!({"operation":"retaining_paths","artifact_id":heap_id,"node_id":root_node,"max_depth":4,"limit":5}),
        json!({"operation":"edges","artifact_id":heap_id,"node_id":root_node,"direction":"outgoing","limit":5}),
    ] {
        let mut arguments = arguments.as_object().cloned().unwrap();
        arguments.insert("agent_session_id".to_string(), json!(session_id));
        arguments.insert("tab_id".to_string(), json!(tab.tab_id));
        harness.client_mut().call_tool(
            "memory",
            Value::Object(arguments),
            Duration::from_secs(60),
            false,
        )?;
    }
    harness.client_mut().call_tool(
        "memory",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"operation":"close","artifact_id":heap_id}),
        Duration::from_secs(20),
        false,
    )?;

    harness.client_mut().call_tool(
        "screencast",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"operation":"start","fps":5,"quality":50,"max_duration_ms":3000}),
        Duration::from_secs(20),
        false,
    )?;
    harness.client_mut().call_tool(
        "screencast",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"operation":"status"}),
        Duration::from_secs(20),
        false,
    )?;
    harness.client_mut().call_tool(
        "navigate",
        json!({
            "agent_session_id":session_id,
            "tab_id":tab.tab_id,
            "action":"url",
            "url":data_url("Screencast Navigation", "Screencast Navigation"),
            "wait_until":"load",
            "timeout_ms":10_000
        }),
        Duration::from_secs(20),
        false,
    )?;
    harness.client_mut().call_tool(
        "evaluate",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"source":"document.body.style.background='rgb(10,20,30)';true"}),
        Duration::from_secs(20),
        false,
    )?;
    std::thread::sleep(Duration::from_millis(800));
    let mut stopped = harness.client_mut().call_tool(
        "screencast",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"operation":"stop"}),
        Duration::from_secs(60),
        false,
    )?;
    let completion_deadline = Instant::now() + Duration::from_secs(35);
    while stopped["state"] == "finalizing" && Instant::now() < completion_deadline {
        std::thread::sleep(Duration::from_millis(50));
        stopped = harness.client_mut().call_tool(
            "screencast",
            json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"operation":"status"}),
            Duration::from_secs(10),
            false,
        )?;
    }
    assert_eq!(stopped["state"], "ready");
    assert_eq!(stopped["artifact"]["media_type"], "video/webm");
    assert!(stopped["metrics"]["encoded_frames"].as_u64().unwrap() > 0);

    let listed = harness.client_mut().call_tool(
        "artifacts",
        json!({"agent_session_id":session_id,"operation":"list","limit":100}),
        Duration::from_secs(20),
        false,
    )?;
    assert!(!listed["artifacts"].as_array().unwrap().is_empty());
    harness.client_mut().call_tool(
        "artifacts",
        json!({"agent_session_id":session_id,"operation":"metadata","artifact_id":audit_id}),
        Duration::from_secs(20),
        false,
    )?;
    harness.client_mut().call_tool(
        "artifacts",
        json!({"agent_session_id":session_id,"operation":"read","artifact_id":audit_id,"offset":0,"length":1024}),
        Duration::from_secs(20),
        false,
    )?;
    harness.client_mut().call_tool(
        "artifacts",
        json!({"agent_session_id":session_id,"operation":"export","artifact_id":audit_id,"path":"audit-export.json"}),
        Duration::from_secs(20),
        false,
    )?;
    assert!(workspace.join("audit-export.json").is_file());
    harness.client_mut().call_tool(
        "artifacts",
        json!({"agent_session_id":session_id,"operation":"delete","artifact_id":audit_id}),
        Duration::from_secs(20),
        false,
    )?;

    harness.client_mut().call_tool(
        "evaluate",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"source":"window.onbeforeunload = () => 'leave'; true"}),
        Duration::from_secs(20),
        false,
    )?;
    harness.client_mut().call_tool(
        "navigate",
        json!({
            "agent_session_id":session_id,
            "tab_id":tab.tab_id,
            "action":"url",
            "url":init_script_url,
            "wait_until":"network_idle",
            "before_unload":"accept",
            "init_script":"window.__visibleBrowserLabInit = 'before-page-script';",
            "observe":"none"
        }),
        Duration::from_secs(30),
        false,
    )?;
    let init_script = harness.client_mut().call_tool(
        "evaluate",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"source":"window.__visibleBrowserLabInit"}),
        Duration::from_secs(20),
        false,
    )?;
    assert_eq!(init_script["value"], "before-page-script");
    harness.client_mut().call_tool(
        "wait_for",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id,"condition":{"kind":"load","state":"network_idle"},"timeout_ms":5000,"observe":"none"}),
        Duration::from_secs(20),
        false,
    )?;
    for (action, url, wait_until) in [
        (
            "url",
            Some(harness.fixture.url("/page?history=one")),
            "dom_content_loaded",
        ),
        (
            "url",
            Some(harness.fixture.url("/page?history=two")),
            "load",
        ),
        ("back", None, "load"),
        ("forward", None, "dom_content_loaded"),
        ("reload", None, "load"),
    ] {
        harness
            .client_mut()
            .call_tool(
                "navigate",
                json!({
                    "agent_session_id":session_id,
                    "tab_id":tab.tab_id,
                    "action":action,
                    "url":url,
                    "wait_until":wait_until,
                    "observe":"none"
                }),
                Duration::from_secs(20),
                false,
            )
            .with_context(|| format!("navigate {action} with wait_until={wait_until}"))?;
    }

    harness.client_mut().call_tool(
        "close_tab",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id}),
        Duration::from_secs(20),
        false,
    )?;
    Ok(())
}

#[test]
fn closed_shadow_controls_accept_trusted_background_input() -> Result<()> {
    let mut harness = BrowserMcpHarness::start("visible-browser-lab-closed-shadow", true)?;
    let start_url = harness.fixture.url("/closed-shadow");
    let session = harness.client_mut().call_tool(
        "start_session",
        json!({
            "label": "closed-shadow-background-input",
            "start_url": start_url,
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
    let snapshot = harness.client_mut().call_tool(
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
    let apply_ref = snapshot_element_ref(&tree, "button \"Closed overlay Apply\"")?;

    harness.client_mut().call_tool(
        "click",
        json!({
            "agent_session_id": session_id,
            "tab_id": tab.tab_id,
            "target": { "ref": apply_ref },
            "observe": "none"
        }),
        Duration::from_secs(20),
        false,
    )?;
    harness.client_mut().call_tool(
        "press_key",
        json!({
            "agent_session_id": session_id,
            "tab_id": tab.tab_id,
            "target": { "ref": apply_ref },
            "key": "Enter",
            "observe": "none"
        }),
        Duration::from_secs(20),
        false,
    )?;
    let result = harness.client_mut().call_tool(
        "evaluate",
        json!({
            "agent_session_id": session_id,
            "tab_id": tab.tab_id,
            "source": "({ clicked: document.body.dataset.closedOverlayClicked, clickTrusted: document.body.dataset.closedOverlayClickTrusted, key: document.body.dataset.closedOverlayKey, keyTrusted: document.body.dataset.closedOverlayKeyTrusted })"
        }),
        Duration::from_secs(20),
        false,
    )?;
    assert_eq!(result["value"]["clicked"], "yes");
    assert_eq!(result["value"]["clickTrusted"], "true");
    assert_eq!(result["value"]["key"], "Enter");
    assert_eq!(result["value"]["keyTrusted"], "true");

    harness.client_mut().call_tool(
        "close_tab",
        json!({"agent_session_id":session_id,"tab_id":tab.tab_id}),
        Duration::from_secs(20),
        false,
    )?;
    Ok(())
}

#[test]
fn explicit_focus_contract() -> Result<()> {
    let mut harness = BrowserMcpHarness::start("visible-browser-lab-focus-contract", true)?;
    let url = harness.fixture.url("/page");
    let session = harness.client_mut().call_tool(
        "start_session",
        json!({
            "label": "focus-contract",
            "start_url": url,
            "focus": false
        }),
        Duration::from_secs(45),
        false,
    )?;
    let session_id = field_str(&session, "agent_session_id")?;
    let tab = session.get("tab").context("start_session omitted tab")?;
    let open_tab = OpenTab::from_summary(&session_id, tab)?;
    let active = harness.client_mut().call_tool(
        "new_tab",
        json!({
            "agent_session_id": session_id,
            "url": "about:blank",
            "focus": true
        }),
        Duration::from_secs(20),
        false,
    )?;
    let active_tab = OpenTab::from_summary(
        &session_id,
        active.get("tab").context("new_tab omitted tab")?,
    )?;

    let navigation_url = harness.fixture.url("/page?background-navigation=1");
    harness.client_mut().call_tool(
        "navigate",
        json!({
            "agent_session_id": session_id,
            "tab_id": open_tab.tab_id,
            "action": "url",
            "url": navigation_url,
            "wait_until": "load"
        }),
        Duration::from_secs(20),
        false,
    )?;

    let title = harness.client_mut().call_tool(
        "evaluate",
        json!({
            "agent_session_id": session_id,
            "tab_id": open_tab.tab_id,
            "source": "document.title"
        }),
        Duration::from_secs(20),
        false,
    )?;
    if title.get("value").and_then(Value::as_str) != Some("VBL Fixture") {
        bail!("background evaluation returned an unexpected title: {title}");
    }

    harness.client_mut().call_tool(
        "screenshot",
        json!({
            "agent_session_id": session_id,
            "tab_id": open_tab.tab_id,
            "full_page": false
        }),
        Duration::from_secs(20),
        false,
    )?;
    harness.client_mut().call_tool(
        "evaluate",
        json!({
            "agent_session_id": session_id,
            "tab_id": open_tab.tab_id,
            "source": "document.querySelector('#entry').focus()"
        }),
        Duration::from_secs(20),
        false,
    )?;
    harness.client_mut().call_tool(
        "type_text",
        json!({
            "agent_session_id": session_id,
            "tab_id": open_tab.tab_id,
            "target": { "css": "#entry" },
            "text": "background text"
        }),
        Duration::from_secs(20),
        false,
    )?;
    harness.client_mut().call_tool(
        "console",
        json!({
            "agent_session_id": session_id,
            "tab_id": open_tab.tab_id,
            "operation": "list"
        }),
        Duration::from_secs(20),
        false,
    )?;
    harness.client_mut().call_tool(
        "network",
        json!({
            "agent_session_id": session_id,
            "tab_id": open_tab.tab_id,
            "operation": "list"
        }),
        Duration::from_secs(20),
        false,
    )?;

    harness.client_mut().call_tool(
        "click",
        json!({
            "agent_session_id": session_id,
            "tab_id": open_tab.tab_id,
            "target": { "css": "#clicker" },
            "observe": "none"
        }),
        Duration::from_secs(20),
        false,
    )?;
    harness.client_mut().call_tool(
        "press_key",
        json!({
            "agent_session_id": session_id,
            "tab_id": open_tab.tab_id,
            "target": { "css": "#entry" },
            "key": "Enter"
        }),
        Duration::from_secs(20),
        false,
    )?;
    let action_result = harness.client_mut().call_tool(
        "evaluate",
        json!({
            "agent_session_id": session_id,
            "tab_id": open_tab.tab_id,
            "source": "({ clicked: document.body.dataset.clicked, key: document.body.dataset.key })"
        }),
        Duration::from_secs(20),
        false,
    )?;
    let action_value = action_result
        .get("value")
        .context("background action result omitted value")?;
    if action_value.get("clicked").and_then(Value::as_str) != Some("yes") {
        bail!("background click did not update the fixture page: {action_result}");
    }
    match action_value.get("key").and_then(Value::as_str) {
        Some("Enter" | "Unidentified") => {}
        _ => {
            bail!("background press_key did not update the fixture page: {action_result}");
        }
    }
    harness.client_mut().call_tool(
        "close_tab",
        json!({
            "agent_session_id": session_id,
            "tab_id": open_tab.tab_id
        }),
        Duration::from_secs(20),
        false,
    )?;
    harness.client_mut().call_tool(
        "close_tab",
        json!({
            "agent_session_id": session_id,
            "tab_id": active_tab.tab_id
        }),
        Duration::from_secs(20),
        false,
    )?;
    Ok(())
}

proptest_state_machine::prop_state_machine! {
    #![proptest_config(property_config())]

    #[test]
    fn tab_ownership_state_machine(sequential 3..8 => OwnershipStateMachine);
}

fn property_config() -> Config {
    let cases = env::var("PROPTEST_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(8);
    Config {
        cases,
        failure_persistence: Some(Box::new(FileFailurePersistence::Direct(
            "proptest-regressions/headless_mcp.txt",
        ))),
        ..Config::default()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Actor {
    First,
    Second,
}

impl Actor {
    const ALL: [Self; 2] = [Self::First, Self::Second];

    fn index(self) -> usize {
        match self {
            Self::First => 0,
            Self::Second => 1,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::First => "property-first",
            Self::Second => "property-second",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ModelTabState {
    Active,
    Missing,
    Closed,
}

#[derive(Clone, Debug)]
struct ModelTab {
    owner: Option<Actor>,
    state: ModelTabState,
}

#[derive(Clone, Debug, Default)]
struct ModelState {
    sessions: [bool; 2],
    tabs: Vec<ModelTab>,
}

#[derive(Clone, Debug)]
enum Transition {
    StartSession(Actor),
    NewTab(Actor),
    ListOwned(Actor),
    ForeignFocus { caller: Actor, tab: usize },
    Release { owner: Actor, tab: usize },
    Claim { caller: Actor, tab: usize },
    Takeover { caller: Actor, tab: usize },
    Close { owner: Actor, tab: usize },
    ExternalCloseThenFocus { owner: Actor, tab: usize },
    Navigate { owner: Actor, tab: usize },
    Evaluate { owner: Actor, tab: usize },
    SemanticFill { owner: Actor, tab: usize },
}

struct OwnershipModel;

impl ReferenceStateMachine for OwnershipModel {
    type State = ModelState;
    type Transition = Transition;

    fn init_state() -> BoxedStrategy<Self::State> {
        Just(ModelState::default()).boxed()
    }

    fn transitions(state: &Self::State) -> BoxedStrategy<Self::Transition> {
        let mut choices: Vec<BoxedStrategy<Transition>> = Vec::new();

        for actor in Actor::ALL {
            if !state.sessions[actor.index()] {
                choices.push(Just(Transition::StartSession(actor)).boxed());
            } else {
                choices.push(Just(Transition::ListOwned(actor)).boxed());
                if state.tabs.len() < 6 {
                    choices.push(Just(Transition::NewTab(actor)).boxed());
                }
            }
        }

        for (tab, model_tab) in state.tabs.iter().enumerate() {
            match (model_tab.owner, model_tab.state) {
                (Some(owner), ModelTabState::Active) => {
                    choices.push(Just(Transition::Navigate { owner, tab }).boxed());
                    choices.push(Just(Transition::Evaluate { owner, tab }).boxed());
                    choices.push(Just(Transition::SemanticFill { owner, tab }).boxed());
                    choices.push(Just(Transition::Release { owner, tab }).boxed());
                    choices.push(Just(Transition::Close { owner, tab }).boxed());
                    choices.push(Just(Transition::ExternalCloseThenFocus { owner, tab }).boxed());

                    for caller in Actor::ALL {
                        if state.sessions[caller.index()] && caller != owner {
                            choices.push(Just(Transition::ForeignFocus { caller, tab }).boxed());
                            choices.push(Just(Transition::Takeover { caller, tab }).boxed());
                        }
                    }
                }
                (None, ModelTabState::Active) => {
                    let callers = Actor::ALL
                        .into_iter()
                        .filter(|actor| state.sessions[actor.index()])
                        .collect::<Vec<_>>();
                    if !callers.is_empty() {
                        choices.push(
                            select(callers)
                                .prop_map(move |caller| Transition::Claim { caller, tab })
                                .boxed(),
                        );
                    }
                }
                _ => {}
            }
        }

        Union::new(choices).boxed()
    }

    fn apply(mut state: Self::State, transition: &Self::Transition) -> Self::State {
        match *transition {
            Transition::StartSession(actor) => {
                state.sessions[actor.index()] = true;
                state.tabs.push(ModelTab {
                    owner: Some(actor),
                    state: ModelTabState::Active,
                });
            }
            Transition::NewTab(actor) => {
                state.tabs.push(ModelTab {
                    owner: Some(actor),
                    state: ModelTabState::Active,
                });
            }
            Transition::ListOwned(_) | Transition::ForeignFocus { .. } => {}
            Transition::Release { tab, .. } => state.tabs[tab].owner = None,
            Transition::Claim { caller, tab } | Transition::Takeover { caller, tab } => {
                state.tabs[tab].owner = Some(caller);
            }
            Transition::Close { tab, .. } => state.tabs[tab].state = ModelTabState::Closed,
            Transition::ExternalCloseThenFocus { tab, .. } => {
                state.tabs[tab].state = ModelTabState::Missing;
            }
            Transition::Navigate { .. }
            | Transition::Evaluate { .. }
            | Transition::SemanticFill { .. } => {}
        }
        state
    }

    fn preconditions(state: &Self::State, transition: &Self::Transition) -> bool {
        match *transition {
            Transition::StartSession(actor) => !state.sessions[actor.index()],
            Transition::NewTab(actor) | Transition::ListOwned(actor) => {
                state.sessions[actor.index()]
            }
            Transition::ForeignFocus { caller, tab } => {
                state.sessions[caller.index()]
                    && state.tabs.get(tab).is_some_and(|tab| {
                        tab.state == ModelTabState::Active
                            && tab.owner.is_some()
                            && tab.owner != Some(caller)
                    })
            }
            Transition::Release { owner, tab }
            | Transition::Close { owner, tab }
            | Transition::ExternalCloseThenFocus { owner, tab }
            | Transition::Navigate { owner, tab }
            | Transition::Evaluate { owner, tab }
            | Transition::SemanticFill { owner, tab } => state
                .tabs
                .get(tab)
                .is_some_and(|tab| tab.state == ModelTabState::Active && tab.owner == Some(owner)),
            Transition::Claim { caller, tab } => {
                state.sessions[caller.index()]
                    && state.tabs.get(tab).is_some_and(|tab| {
                        tab.state == ModelTabState::Active && tab.owner.is_none()
                    })
            }
            Transition::Takeover { caller, tab } => {
                state.sessions[caller.index()]
                    && state.tabs.get(tab).is_some_and(|tab| {
                        tab.state == ModelTabState::Active
                            && tab.owner.is_some()
                            && tab.owner != Some(caller)
                    })
            }
        }
    }
}

struct OwnershipStateMachine;

impl StateMachineTest for OwnershipStateMachine {
    type SystemUnderTest = BrowserMcpHarness;
    type Reference = OwnershipModel;

    fn init_test(_ref_state: &ModelState) -> Self::SystemUnderTest {
        BrowserMcpHarness::start("visible-browser-lab-property", true)
            .unwrap_or_else(|error| panic!("{error:#}"))
    }

    fn apply(
        mut state: Self::SystemUnderTest,
        ref_state: &ModelState,
        transition: Transition,
    ) -> Self::SystemUnderTest {
        state
            .apply_transition(transition)
            .unwrap_or_else(|error| panic!("{error:#}"));
        state
            .check_model(ref_state)
            .unwrap_or_else(|error| panic!("{error:#}"));
        state
    }

    fn teardown(mut state: Self::SystemUnderTest, _ref_state: ModelState) {
        state.shutdown();
    }
}

struct ConcreteTab {
    owner: Option<Actor>,
    state: ModelTabState,
    tab_id: String,
    target_id: String,
}

struct BrowserMcpHarness {
    browser: RealBrowser,
    state_dir: TempDir,
    client: Option<McpClient>,
    fixture: FixtureServer,
    sessions: [Option<String>; 2],
    tabs: Vec<ConcreteTab>,
}

impl BrowserMcpHarness {
    fn start(client_name: &str, initialize: bool) -> Result<Self> {
        let browser = RealBrowser::launch_from_env()?;
        let state_dir = tempfile::Builder::new()
            .prefix("visible-browser-lab-headless-mcp-")
            .tempdir()
            .context("failed to create broker state directory")?;
        let root = repo_root();
        let mut client = McpClient::spawn(
            &test_binary(),
            browser.cdp_endpoint(),
            state_dir.path(),
            &root,
        )?;
        if initialize {
            client.initialize(client_name)?;
        }

        Ok(Self {
            browser,
            state_dir,
            client: Some(client),
            fixture: FixtureServer::start()?,
            sessions: [None, None],
            tabs: Vec::new(),
        })
    }

    fn client_mut(&mut self) -> &mut McpClient {
        self.client.as_mut().expect("MCP client is still running")
    }

    fn cdp_endpoint(&self) -> &str {
        self.browser.cdp_endpoint()
    }

    fn session(&self, actor: Actor) -> Result<String> {
        self.sessions[actor.index()]
            .clone()
            .with_context(|| format!("session for {actor:?} has not started"))
    }

    fn apply_transition(&mut self, transition: Transition) -> Result<()> {
        match transition {
            Transition::StartSession(actor) => self.start_session(actor),
            Transition::NewTab(actor) => self.new_tab(actor),
            Transition::ListOwned(actor) => self.list_owned(actor).map(|_| ()),
            Transition::ForeignFocus { caller, tab } => self.foreign_focus(caller, tab),
            Transition::Release { owner, tab } => self.release(owner, tab),
            Transition::Claim { caller, tab } => self.claim(caller, tab),
            Transition::Takeover { caller, tab } => self.takeover(caller, tab),
            Transition::Close { owner, tab } => self.close(owner, tab),
            Transition::ExternalCloseThenFocus { owner, tab } => {
                self.external_close_then_focus(owner, tab)
            }
            Transition::Navigate { owner, tab } => self.navigate(owner, tab),
            Transition::Evaluate { owner, tab } => self.evaluate(owner, tab),
            Transition::SemanticFill { owner, tab } => self.semantic_fill(owner, tab),
        }
    }

    fn start_session(&mut self, actor: Actor) -> Result<()> {
        let url = self.fixture.url("/page");
        let result = self.client_mut().call_tool(
            "start_session",
            json!({
                "label": actor.label(),
                "start_url": "about:blank",
                "focus": true
            }),
            Duration::from_secs(45),
            false,
        )?;
        let session_id = field_str(&result, "agent_session_id")?;
        let tab = result.get("tab").context("start_session omitted tab")?;
        let open_tab = OpenTab::from_summary(&session_id, tab)?;
        self.navigate_to_url(&session_id, &open_tab.tab_id, &url)?;
        self.sessions[actor.index()] = Some(session_id);
        self.tabs.push(ConcreteTab {
            owner: Some(actor),
            state: ModelTabState::Active,
            tab_id: open_tab.tab_id,
            target_id: open_tab.target_id,
        });
        Ok(())
    }

    fn new_tab(&mut self, actor: Actor) -> Result<()> {
        let session_id = self.session(actor)?;
        let url = self.fixture.url("/page");
        let result = self.client_mut().call_tool(
            "new_tab",
            json!({
                "agent_session_id": session_id,
                "focus": true
            }),
            Duration::from_secs(45),
            false,
        )?;
        let tab = result.get("tab").context("new_tab omitted tab")?;
        let open_tab = OpenTab::from_summary(&session_id, tab)?;
        self.navigate_to_url(&session_id, &open_tab.tab_id, &url)?;
        self.tabs.push(ConcreteTab {
            owner: Some(actor),
            state: ModelTabState::Active,
            tab_id: open_tab.tab_id,
            target_id: open_tab.target_id,
        });
        Ok(())
    }

    fn navigate_to_url(&mut self, session_id: &str, tab_id: &str, url: &str) -> Result<()> {
        let arguments = || {
            json!({
                "agent_session_id": session_id,
                "tab_id": tab_id,
                "action": "url",
                "url": url,
                "wait_until": "dom_content_loaded",
                "timeout_ms": PROPERTY_NAVIGATION_TIMEOUT_MS
            })
        };
        for attempt in 1..=PROPERTY_NAVIGATION_ATTEMPTS {
            let result = self.client_mut().request(
                "tools/call",
                json!({
                    "name": "navigate",
                    "arguments": arguments()
                }),
                PROPERTY_BROWSER_TOOL_TIMEOUT,
            )?;
            let is_error = result
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let structured = result
                .get("structuredContent")
                .or_else(|| result.get("structured_content"))
                .cloned()
                .context("tool `navigate` omitted structured content")?;
            if !is_error {
                if structured.get("document_revision").is_none() {
                    bail!("navigate omitted document_revision");
                }
                return Ok(());
            }
            if attempt < PROPERTY_NAVIGATION_ATTEMPTS
                && is_retryable_fixture_navigation_error(&structured)
            {
                thread::sleep(Duration::from_millis(250));
                continue;
            }
            bail!("tool `navigate` returned isError=true, expected false: {result}");
        }
        unreachable!("navigation attempts loop always returns or bails")
    }

    fn list_owned(&mut self, actor: Actor) -> Result<Value> {
        let session_id = self.session(actor)?;
        self.client_mut().call_tool(
            "list_tabs",
            json!({ "agent_session_id": session_id }),
            Duration::from_secs(20),
            false,
        )
    }

    fn foreign_focus(&mut self, caller: Actor, tab: usize) -> Result<()> {
        let session_id = self.session(caller)?;
        let tab_id = self.tabs[tab].tab_id.clone();
        let error = self.client_mut().call_tool(
            "focus_tab",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab_id
            }),
            Duration::from_secs(20),
            true,
        )?;
        assert_tool_error(&error, "tab_not_owned")
    }

    fn release(&mut self, owner: Actor, tab: usize) -> Result<()> {
        let session_id = self.session(owner)?;
        let tab_id = self.tabs[tab].tab_id.clone();
        let result = self.client_mut().call_tool(
            "release_tab",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab_id
            }),
            Duration::from_secs(20),
            false,
        )?;
        if result.get("released").and_then(Value::as_bool) != Some(true) {
            bail!("release_tab did not report released=true: {result}");
        }
        if result.get("leave_visible").and_then(Value::as_bool) != Some(false) {
            bail!("release_tab did not report leave_visible=false: {result}");
        }
        self.tabs[tab].owner = None;
        Ok(())
    }

    fn claim(&mut self, caller: Actor, tab: usize) -> Result<()> {
        let session_id = self.session(caller)?;
        let target_id = self.tabs[tab].target_id.clone();
        let result = self.client_mut().call_tool(
            "claim_tab",
            json!({
                "agent_session_id": session_id,
                "target_id": target_id
            }),
            Duration::from_secs(30),
            false,
        )?;
        let tab_summary = result.get("tab").context("claim_tab omitted tab")?;
        self.tabs[tab].tab_id = field_str(tab_summary, "tab_id")?;
        self.tabs[tab].owner = Some(caller);
        Ok(())
    }

    fn takeover(&mut self, caller: Actor, tab: usize) -> Result<()> {
        let session_id = self.session(caller)?;
        let old_owner = self.tabs[tab].owner.context("takeover tab has no owner")?;
        let old_session = self.session(old_owner)?;
        let old_tab_id = self.tabs[tab].tab_id.clone();
        let target_id = self.tabs[tab].target_id.clone();
        let result = self.client_mut().call_tool(
            "claim_tab",
            json!({
                "agent_session_id": session_id,
                "target_id": target_id,
                "takeover": true,
                "user_instruction": "transfer this tab for property validation"
            }),
            Duration::from_secs(30),
            false,
        )?;
        let tab_summary = result.get("tab").context("takeover omitted tab")?;
        let new_tab_id = field_str(tab_summary, "tab_id")?;
        if new_tab_id == old_tab_id {
            bail!("takeover reused the old tab_id");
        }
        let old_error = self.client_mut().call_tool(
            "focus_tab",
            json!({
                "agent_session_id": old_session.clone(),
                "tab_id": old_tab_id
            }),
            Duration::from_secs(20),
            true,
        )?;
        assert_tool_error(&old_error, "unknown_tab")?;
        let old_owner_tabs = self.client_mut().call_tool(
            "list_tabs",
            json!({ "agent_session_id": old_session }),
            Duration::from_secs(20),
            false,
        )?;
        let old_owner_tabs = old_owner_tabs
            .get("tabs")
            .and_then(Value::as_array)
            .context("old owner list_tabs omitted tabs array")?;
        if tabs_include_id(old_owner_tabs, &old_tab_id) {
            bail!("old owner list_tabs exposed stale takeover tab_id `{old_tab_id}`");
        }
        self.tabs[tab].tab_id = new_tab_id;
        self.tabs[tab].owner = Some(caller);
        Ok(())
    }

    fn close(&mut self, owner: Actor, tab: usize) -> Result<()> {
        let session_id = self.session(owner)?;
        let tab_id = self.tabs[tab].tab_id.clone();
        let result = self.client_mut().call_tool(
            "close_tab",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab_id
            }),
            Duration::from_secs(30),
            false,
        )?;
        if result.get("closed").and_then(Value::as_bool) != Some(true) {
            bail!("close_tab did not report closed=true: {result}");
        }
        self.tabs[tab].state = ModelTabState::Closed;
        Ok(())
    }

    fn external_close_then_focus(&mut self, owner: Actor, tab: usize) -> Result<()> {
        let session_id = self.session(owner)?;
        let tab_id = self.tabs[tab].tab_id.clone();
        let target_id = self.tabs[tab].target_id.clone();
        close_target_via_cdp(self.cdp_endpoint(), &target_id)?;
        let error = self.client_mut().call_tool(
            "focus_tab",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab_id
            }),
            Duration::from_secs(20),
            true,
        )?;
        assert_tool_error(&error, "target_missing")?;
        self.tabs[tab].state = ModelTabState::Missing;
        Ok(())
    }

    fn navigate(&mut self, owner: Actor, tab: usize) -> Result<()> {
        let session_id = self.session(owner)?;
        let tab_id = self.tabs[tab].tab_id.clone();
        let url = self.fixture.url("/page?property-navigation=1");
        let result = self.client_mut().call_tool(
            "navigate",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab_id,
                "action": "url",
                "url": url,
                "wait_until": "dom_content_loaded",
                "timeout_ms": PROPERTY_NAVIGATION_TIMEOUT_MS
            }),
            PROPERTY_BROWSER_TOOL_TIMEOUT,
            false,
        )?;
        if result.get("document_revision").is_none() {
            bail!("navigate omitted document_revision");
        }
        Ok(())
    }

    fn evaluate(&mut self, owner: Actor, tab: usize) -> Result<()> {
        let session_id = self.session(owner)?;
        let tab_id = self.tabs[tab].tab_id.clone();
        let result = self.client_mut().call_tool(
            "evaluate",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab_id,
                "source": "1 + 1"
            }),
            Duration::from_secs(20),
            false,
        )?;
        if result.get("value").and_then(Value::as_i64) != Some(2) {
            bail!("evaluate returned an unexpected value: {result}");
        }
        Ok(())
    }

    fn semantic_fill(&mut self, owner: Actor, tab: usize) -> Result<()> {
        let session_id = self.session(owner)?;
        let tab_id = self.tabs[tab].tab_id.clone();
        self.client_mut().call_tool(
            "wait_for",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab_id,
                "condition": {
                    "kind": "element",
                    "target": { "css": "#entry" },
                    "state": "visible"
                },
                "timeout_ms": PROPERTY_NAVIGATION_TIMEOUT_MS,
                "observe": "none"
            }),
            PROPERTY_BROWSER_TOOL_TIMEOUT,
            false,
        )?;
        let snapshot = self.client_mut().call_tool(
            "snapshot",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab_id,
                "mode": "meaningful"
            }),
            Duration::from_secs(20),
            false,
        )?;
        let tree = field_str(&snapshot, "tree")?;
        let reference = snapshot_element_ref(&tree, "textbox \"Entry\"")?;
        self.client_mut().call_tool(
            "fill",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab_id,
                "target": { "ref": reference },
                "value": "property value",
                "observe": "diff"
            }),
            Duration::from_secs(20),
            false,
        )?;
        let result = self.client_mut().call_tool(
            "evaluate",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab_id,
                "source": "document.querySelector('#entry').value"
            }),
            Duration::from_secs(20),
            false,
        )?;
        if result.get("value").and_then(Value::as_str) != Some("property value") {
            bail!("semantic fill did not update the property fixture: {result}");
        }
        Ok(())
    }

    fn check_model(&mut self, ref_state: &ModelState) -> Result<()> {
        for actor in Actor::ALL {
            if !ref_state.sessions[actor.index()] {
                continue;
            }
            self.check_owned_listing(actor, ref_state)?;
            self.check_global_inventory(actor)?;
        }
        Ok(())
    }

    fn check_owned_listing(&mut self, actor: Actor, ref_state: &ModelState) -> Result<()> {
        let session_id = self.session(actor)?;
        let owned = self.client_mut().call_tool(
            "list_tabs",
            json!({ "agent_session_id": session_id }),
            Duration::from_secs(20),
            false,
        )?;
        let tabs = owned
            .get("tabs")
            .and_then(Value::as_array)
            .context("owned list_tabs omitted tabs array")?;
        for (index, model_tab) in ref_state.tabs.iter().enumerate() {
            let concrete = self
                .tabs
                .get(index)
                .with_context(|| format!("missing concrete tab {index}"))?;
            let listed = tabs_include_id(tabs, &concrete.tab_id);
            let should_be_listed =
                model_tab.owner == Some(actor) && model_tab.state != ModelTabState::Closed;
            if listed != should_be_listed {
                bail!(
                    "owned list mismatch for {actor:?} tab {index}: listed={listed}, expected={should_be_listed}, response={owned}"
                );
            }
        }
        Ok(())
    }

    fn check_global_inventory(&mut self, actor: Actor) -> Result<()> {
        let session_id = self.session(actor)?;
        let global = self.client_mut().call_tool(
            "list_tabs",
            json!({
                "agent_session_id": session_id,
                "scope": "global_readonly"
            }),
            Duration::from_secs(20),
            false,
        )?;
        let groups = global
            .get("groups")
            .and_then(Value::as_array)
            .context("global list_tabs omitted groups array")?;
        for tab in groups
            .iter()
            .filter_map(|group| group.get("tabs").and_then(Value::as_array))
            .flat_map(|tabs| tabs.iter())
        {
            let owned_by_caller = tab
                .get("owned_by_caller")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if !owned_by_caller
                && tab.get("owner_display_id").is_some()
                && (tab.get("caller_tab_id").is_some() || tab.get("tab_id").is_some())
            {
                bail!("global_readonly exposed a foreign action handle: {tab}");
            }
        }
        Ok(())
    }

    fn shutdown(&mut self) {
        if let Some(client) = self.client.as_mut() {
            let mut open_tabs = self
                .tabs
                .iter()
                .filter_map(|tab| {
                    let owner = tab.owner?;
                    if tab.state != ModelTabState::Active {
                        return None;
                    }
                    Some(OpenTab {
                        session_id: self.sessions[owner.index()].clone()?,
                        tab_id: tab.tab_id.clone(),
                        target_id: tab.target_id.clone(),
                    })
                })
                .collect::<Vec<_>>();
            cleanup_open_tabs(client, &mut open_tabs);
        }

        for tab in &self.tabs {
            if tab.owner.is_none() && tab.state == ModelTabState::Active {
                let _ = close_target_via_cdp(self.cdp_endpoint(), &tab.target_id);
            }
        }

        if let Some(client) = self.client.as_mut() {
            client.shutdown();
        }
        stop_broker(self.state_dir.path());
        self.browser.shutdown();
        self.client = None;
    }
}

impl Drop for BrowserMcpHarness {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn is_retryable_fixture_navigation_error(error: &Value) -> bool {
    let code = error.get("code").and_then(Value::as_str);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    (matches!(code, Some("operation_timeout"))
        && message.contains("start page navigation timed out"))
        || (matches!(code, Some("chrome_unavailable"))
            && message.contains("net::ERR_CONNECTION_RESET"))
}

fn assert_tool_error(value: &Value, expected: &str) -> Result<()> {
    let code = field_str(value, "code")?;
    if code != expected {
        bail!("expected tool error `{expected}`, got `{code}` in {value}");
    }
    Ok(())
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

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn test_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_visible-browser-lab-mcp"))
}
