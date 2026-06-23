use std::{
    env,
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use zip::{
    CompressionMethod, ZipArchive, ZipWriter,
    write::{ExtendedFileOptions, FileOptions},
};

const BINARY_NAME: &str = "visible-browser-lab-mcp";
const DEFAULT_OUT_DIR: &str = "out/packages";
const SUPPORTED_TARGETS: &[&str] = &[
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "x86_64-pc-windows-msvc",
    "aarch64-pc-windows-msvc",
];

#[derive(Clone, Copy)]
struct AgentHost {
    id: &'static str,
    display_name: &'static str,
    manifest_path: &'static str,
}

const AGENT_HOSTS: &[AgentHost] = &[
    AgentHost {
        id: "codex",
        display_name: "Codex",
        manifest_path: ".codex-plugin/plugin.json",
    },
    AgentHost {
        id: "claude-code",
        display_name: "Claude Code",
        manifest_path: ".claude-plugin/plugin.json",
    },
    AgentHost {
        id: "vscode",
        display_name: "VS Code",
        manifest_path: ".vscode-plugin/plugin.json",
    },
];

fn main() -> Result<()> {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        print_usage();
        return Ok(());
    };

    match command.as_str() {
        "validate" => validate(),
        "package" => package(PackageArgs::parse(args.collect())?),
        "checksums" => checksums(ChecksumsArgs::parse(args.collect())?),
        "live-smoke" => live_smoke(LiveSmokeArgs::parse(args.collect())?),
        "-h" | "--help" | "help" => {
            print_usage();
            Ok(())
        }
        command => bail!("unknown xtask command `{command}`"),
    }
}

fn print_usage() {
    eprintln!(
        "\
usage:
  cargo xtask validate
  cargo xtask package [--target <target>] [--binary <path>] [--out-dir <dir>]
  cargo xtask checksums [--dir <dir>]
  cargo xtask live-smoke [--cdp-endpoint <url>] [--binary <path>] [--state-dir <dir>]
"
    );
}

#[derive(Debug)]
struct PackageArgs {
    target: String,
    binary: Option<PathBuf>,
    out_dir: PathBuf,
}

impl PackageArgs {
    fn parse(args: Vec<String>) -> Result<Self> {
        let mut target = None;
        let mut binary = None;
        let mut out_dir = PathBuf::from(DEFAULT_OUT_DIR);
        let mut index = 0;

        while index < args.len() {
            match args[index].as_str() {
                "--target" => {
                    index += 1;
                    target = Some(
                        args.get(index)
                            .context("missing value after --target")?
                            .to_string(),
                    );
                }
                "--binary" => {
                    index += 1;
                    binary = Some(PathBuf::from(
                        args.get(index).context("missing value after --binary")?,
                    ));
                }
                "--out-dir" => {
                    index += 1;
                    out_dir =
                        PathBuf::from(args.get(index).context("missing value after --out-dir")?);
                }
                arg => bail!("unknown package argument `{arg}`"),
            }

            index += 1;
        }

        let target = target.unwrap_or(host_target()?);
        ensure_supported_target(&target)?;

        Ok(Self {
            target,
            binary,
            out_dir,
        })
    }
}

#[derive(Debug)]
struct ChecksumsArgs {
    dir: PathBuf,
}

impl ChecksumsArgs {
    fn parse(args: Vec<String>) -> Result<Self> {
        let mut dir = PathBuf::from(DEFAULT_OUT_DIR);
        let mut index = 0;

        while index < args.len() {
            match args[index].as_str() {
                "--dir" => {
                    index += 1;
                    dir = PathBuf::from(args.get(index).context("missing value after --dir")?);
                }
                arg => bail!("unknown checksums argument `{arg}`"),
            }

            index += 1;
        }

        Ok(Self { dir })
    }
}

#[derive(Debug)]
struct LiveSmokeArgs {
    cdp_endpoint: String,
    binary: Option<PathBuf>,
    state_dir: Option<PathBuf>,
}

impl LiveSmokeArgs {
    fn parse(args: Vec<String>) -> Result<Self> {
        let mut cdp_endpoint = "http://127.0.0.1:9222".to_string();
        let mut binary = None;
        let mut state_dir = None;
        let mut index = 0;

        while index < args.len() {
            match args[index].as_str() {
                "--cdp-endpoint" => {
                    index += 1;
                    cdp_endpoint = args
                        .get(index)
                        .context("missing value after --cdp-endpoint")?
                        .to_string();
                }
                "--binary" => {
                    index += 1;
                    binary = Some(PathBuf::from(
                        args.get(index).context("missing value after --binary")?,
                    ));
                }
                "--state-dir" => {
                    index += 1;
                    state_dir = Some(PathBuf::from(
                        args.get(index).context("missing value after --state-dir")?,
                    ));
                }
                arg => bail!("unknown live-smoke argument `{arg}`"),
            }

            index += 1;
        }

        Ok(Self {
            cdp_endpoint,
            binary,
            state_dir,
        })
    }
}

fn validate() -> Result<()> {
    let root = repo_root()?;

    for path in [
        ".codex-plugin/plugin.json",
        "skills/visible-browser-lab/SKILL.md",
        "Cargo.toml",
        "Cargo.lock",
    ] {
        let path = root.join(path);
        if !path.is_file() {
            bail!("required source file is missing: {}", path.display());
        }
    }

    for forbidden in [
        "package.json",
        "pnpm-lock.yaml",
        "yarn.lock",
        "package-lock.json",
    ] {
        let path = root.join(forbidden);
        if path.exists() {
            bail!("Node packaging file is not allowed for trusted binary releases: {forbidden}");
        }
    }

    let gitignore =
        fs::read_to_string(root.join(".gitignore")).context("failed to read .gitignore")?;
    for required in [".DS_Store", ".exo/runtime/", "target/", "out/"] {
        if !gitignore.lines().any(|line| line.trim() == required) {
            bail!(".gitignore must ignore `{required}`");
        }
    }

    let package_dir = root.join(DEFAULT_OUT_DIR);
    if package_dir.is_dir() {
        validate_archives(&package_dir)?;
    }

    println!("validated visible-browser-lab release inputs");
    Ok(())
}

fn package(args: PackageArgs) -> Result<()> {
    let root = repo_root()?;
    let binary = binary_path(&root, &args.target, args.binary.as_deref())?;
    let out_dir = root.join(args.out_dir);
    fs::create_dir_all(&out_dir)
        .with_context(|| format!("failed to create output directory `{}`", out_dir.display()))?;

    let mut archives = Vec::new();
    for host in AGENT_HOSTS {
        let archive = out_dir.join(format!(
            "visible-browser-lab-{}-{}.zip",
            host.id, args.target
        ));
        write_plugin_archive(&root, host, &args.target, &binary, &archive)?;
        archives.push(archive);
    }

    let binary_archive = out_dir.join(format!("visible-browser-lab-mcp-{}.zip", args.target));
    write_binary_archive(&args.target, &binary, &binary_archive)?;
    archives.push(binary_archive);

    for archive in &archives {
        println!("wrote {}", archive.display());
    }

    validate_archives(&out_dir)?;
    Ok(())
}

fn checksums(args: ChecksumsArgs) -> Result<()> {
    let root = repo_root()?;
    let dir = root.join(args.dir);
    let mut files = archive_files(&dir)?;

    if files.is_empty() {
        bail!("no release assets found in `{}`", dir.display());
    }

    files.sort();
    let mut output = String::new();

    for path in &files {
        if path.file_name().is_some_and(|name| name == "SHA256SUMS") {
            continue;
        }

        let digest = sha256_file(path)?;
        let relative = path
            .strip_prefix(&dir)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        output.push_str(&format!("{digest}  {relative}\n"));
    }

    let sums_path = dir.join("SHA256SUMS");
    fs::write(&sums_path, output)
        .with_context(|| format!("failed to write `{}`", sums_path.display()))?;
    println!("wrote {}", sums_path.display());
    Ok(())
}

fn live_smoke(args: LiveSmokeArgs) -> Result<()> {
    let root = repo_root()?;
    if args.binary.is_none() {
        build_live_smoke_binary(&root)?;
    }
    let binary = live_smoke_binary(&root, args.binary.as_deref())?;
    let (state_dir, remove_state_dir) = match args.state_dir {
        Some(state_dir) => (state_dir, false),
        None => {
            // Unix socket paths are length-limited; keep the smoke state path short.
            let state_root = if cfg!(windows) {
                env::temp_dir()
            } else {
                PathBuf::from("/tmp")
            };
            let state_dir = state_root.join(format!(
                "visible-browser-lab-live-smoke-{}-{}",
                std::process::id(),
                unix_millis()?
            ));
            fs::create_dir_all(&state_dir)
                .with_context(|| format!("failed to create `{}`", state_dir.display()))?;
            (state_dir, true)
        }
    };

    let mut client = McpClient::spawn(&binary, &args.cdp_endpoint, &state_dir, &root)?;
    let mut open_tabs = Vec::new();
    let smoke_result = run_live_smoke(&mut client, &mut open_tabs, &args.cdp_endpoint);
    cleanup_open_tabs(&mut client, &mut open_tabs);
    client.shutdown();
    stop_broker(&state_dir);

    if remove_state_dir {
        let _ = fs::remove_dir_all(&state_dir);
    }

    let summary = smoke_result?;
    println!(
        "live smoke passed: tools={}, screenshot_bytes={}, global_groups={}",
        summary.tool_count, summary.screenshot_bytes, summary.global_groups
    );
    Ok(())
}

fn run_live_smoke(
    client: &mut McpClient,
    open_tabs: &mut Vec<OpenTab>,
    cdp_endpoint: &str,
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
    let tool_names = tools
        .get("tools")
        .and_then(Value::as_array)
        .context("tools/list omitted tools array")?
        .iter()
        .filter_map(|tool| tool.get("name").and_then(Value::as_str))
        .collect::<Vec<_>>();
    for expected in [
        "start_session",
        "list_tabs",
        "new_tab",
        "claim_tab",
        "release_tab",
        "focus_tab",
        "navigate",
        "screenshot",
        "evaluate",
        "click",
        "type_text",
        "press_key",
        "console_messages",
        "network_events",
        "close_tab",
    ] {
        if !tool_names.contains(&expected) {
            bail!("MCP tool `{expected}` was not advertised; got {tool_names:?}");
        }
    }

    let first = client.call_tool(
        "start_session",
        json!({
            "label": "smoke-first",
            "start_url": data_url("VBL Smoke One", "VBL Smoke One"),
            "focus": true
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
            "focus": true
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
            "focus": true
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
            "url": data_url("VBL Smoke Nav", "VBL Smoke Nav"),
            "timeout_ms": 10000
        }),
        Duration::from_secs(30),
        false,
    )?;
    let navigated_tab = navigated.get("tab").context("navigate omitted tab")?;
    if field_str(navigated_tab, "tab_id")? != transferable_tab.tab_id {
        bail!("navigate returned a different tab_id");
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
    if field_str(&screenshot, "mime_type")? != "image/png" {
        bail!("screenshot returned a non-PNG mime type");
    }
    let screenshot_data = field_str(&screenshot, "data_base64")?;
    if !screenshot_data.starts_with("iVBOR") || screenshot_data.len() < 1000 {
        bail!("screenshot payload does not look like a PNG");
    }

    client.call_tool(
        "navigate",
        json!({
            "agent_session_id": first_session,
            "tab_id": transferable_tab.tab_id,
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
            "expression": "(async () => { console.log('vbl-console-ready'); await fetch('/data.json'); return { title: document.title, ready: true }; })()"
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

    client.call_tool(
        "click",
        json!({
            "agent_session_id": first_session,
            "tab_id": transferable_tab.tab_id,
            "selector": "#clicker",
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
            "expression": "document.body.dataset.clicked"
        }),
        Duration::from_secs(20),
        false,
    )?;
    if clicked.get("value").and_then(Value::as_str) != Some("yes") {
        bail!("click did not update the fixture page: {clicked}");
    }

    client.call_tool(
        "click",
        json!({
            "agent_session_id": first_session,
            "tab_id": transferable_tab.tab_id,
            "selector": "#entry",
            "timeout_ms": 5000
        }),
        Duration::from_secs(20),
        false,
    )?;
    client.call_tool(
        "type_text",
        json!({
            "agent_session_id": first_session,
            "tab_id": transferable_tab.tab_id,
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
            "expression": "({ value: document.querySelector('#entry').value, key: document.body.dataset.key })"
        }),
        Duration::from_secs(20),
        false,
    )?;
    if typed
        .get("value")
        .and_then(|value| value.get("value"))
        .and_then(Value::as_str)
        != Some("typed")
        || typed
            .get("value")
            .and_then(|value| value.get("key"))
            .and_then(Value::as_str)
            != Some("Enter")
    {
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

    for tool in [
        "evaluate",
        "click",
        "type_text",
        "press_key",
        "console_messages",
        "network_events",
    ] {
        let arguments = match tool {
            "evaluate" => json!({
                "agent_session_id": first_session,
                "tab_id": second_open_tab.tab_id,
                "expression": "1 + 1"
            }),
            "click" => json!({
                "agent_session_id": first_session,
                "tab_id": second_open_tab.tab_id,
                "selector": "body"
            }),
            "type_text" => json!({
                "agent_session_id": first_session,
                "tab_id": second_open_tab.tab_id,
                "text": "x"
            }),
            "press_key" => json!({
                "agent_session_id": first_session,
                "tab_id": second_open_tab.tab_id,
                "key": "Enter"
            }),
            "console_messages" | "network_events" => json!({
                "agent_session_id": first_session,
                "tab_id": second_open_tab.tab_id
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

    let missing = client.call_tool(
        "new_tab",
        json!({
            "agent_session_id": first_session,
            "url": data_url("VBL Smoke Missing", "VBL Smoke Missing"),
            "focus": true
        }),
        Duration::from_secs(45),
        false,
    )?;
    let missing_tab = missing.get("tab").context("new_tab omitted missing tab")?;
    let missing_open_tab = OpenTab::from_summary(&first_session, missing_tab)?;
    open_tabs.push(missing_open_tab.clone());
    close_target_via_cdp(cdp_endpoint, &missing_open_tab.target_id)?;

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
        screenshot_bytes: screenshot_data.len() * 3 / 4,
        global_groups: groups.len(),
    })
}

fn cleanup_open_tabs(client: &mut McpClient, open_tabs: &mut Vec<OpenTab>) {
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

fn wait_for_console_message(
    client: &mut McpClient,
    session_id: &str,
    tab_id: &str,
    expected: &str,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let result = client.call_tool(
            "console_messages",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab_id
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

    bail!("console_messages did not include `{expected}`");
}

fn wait_for_network_event(
    client: &mut McpClient,
    session_id: &str,
    tab_id: &str,
    expected_url_fragment: &str,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let result = client.call_tool(
            "network_events",
            json!({
                "agent_session_id": session_id,
                "tab_id": tab_id
            }),
            Duration::from_secs(10),
            false,
        )?;
        if result
            .get("events")
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

    bail!("network_events did not include `{expected_url_fragment}`");
}

struct FixtureServer {
    base_url: String,
    stop: Option<mpsc::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl FixtureServer {
    fn start() -> Result<Self> {
        let listener =
            TcpListener::bind("127.0.0.1:0").context("failed to bind live-smoke fixture server")?;
        listener
            .set_nonblocking(true)
            .context("failed to configure live-smoke fixture server")?;
        let address = listener
            .local_addr()
            .context("failed to read fixture server address")?;
        let (stop_tx, stop_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            loop {
                if stop_rx.try_recv().is_ok() {
                    break;
                }

                match listener.accept() {
                    Ok((stream, _)) => handle_fixture_connection(stream),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(25));
                    }
                    Err(_) => break,
                }
            }
        });

        Ok(Self {
            base_url: format!("http://{address}"),
            stop: Some(stop_tx),
            thread: Some(thread),
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn handle_fixture_connection(mut stream: TcpStream) {
    let mut buffer = [0; 2048];
    let Ok(bytes) = stream.read(&mut buffer) else {
        return;
    };
    let request = String::from_utf8_lossy(&buffer[..bytes]);
    let path = request
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/");

    let (content_type, body) = match path {
        "/data.json" => ("application/json", r#"{"ok":true}"#.to_string()),
        _ => (
            "text/html; charset=utf-8",
            r#"<!doctype html>
<title>VBL Fixture</title>
<button id="clicker" onclick="document.body.dataset.clicked='yes'; console.log('vbl-clicked')">Click</button>
<input id="entry" />
<script>
document.querySelector('#entry').addEventListener('keydown', (event) => {
  document.body.dataset.key = event.key;
});
</script>"#
                .to_string(),
        ),
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

#[derive(Debug, Clone)]
struct OpenTab {
    session_id: String,
    tab_id: String,
    target_id: String,
}

impl OpenTab {
    fn from_summary(session_id: &str, value: &Value) -> Result<Self> {
        Ok(Self {
            session_id: session_id.to_string(),
            tab_id: field_str(value, "tab_id")?,
            target_id: field_str(value, "target_id")?,
        })
    }
}

#[derive(Debug)]
struct SmokeSummary {
    tool_count: usize,
    screenshot_bytes: usize,
    global_groups: usize,
}

struct McpClient {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Receiver<String>,
    stderr: Receiver<String>,
    next_id: u64,
}

impl McpClient {
    fn spawn(binary: &Path, cdp_endpoint: &str, state_dir: &Path, root: &Path) -> Result<Self> {
        let mut child = Command::new(binary)
            .arg("--cdp-endpoint")
            .arg(cdp_endpoint)
            .arg("--state-dir")
            .arg(state_dir)
            .current_dir(root)
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
        })
    }

    fn request(&mut self, method: &str, params: Value, timeout: Duration) -> Result<Value> {
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

    fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let mut message = json!({
            "jsonrpc": "2.0",
            "method": method
        });
        if !params.is_null() {
            message["params"] = params;
        }
        self.write_message(&message)
    }

    fn call_tool(
        &mut self,
        name: &str,
        arguments: Value,
        timeout: Duration,
        expect_error: bool,
    ) -> Result<Value> {
        let result = self.request(
            "tools/call",
            json!({
                "name": name,
                "arguments": arguments
            }),
            timeout,
        )?;
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

    fn shutdown(&mut self) {
        let _ = self.stdin.take();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if matches!(self.child.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
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

fn live_smoke_binary(root: &Path, override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
        bail!(
            "live-smoke binary override does not exist: {}",
            path.display()
        );
    }

    let binary = root
        .join("target")
        .join("debug")
        .join(binary_file_name(&host_target()?));
    if binary.is_file() {
        return Ok(binary);
    }

    bail!(
        "debug binary not found at `{}`. Run `cargo build --bin visible-browser-lab-mcp` first.",
        binary.display()
    )
}

fn build_live_smoke_binary(root: &Path) -> Result<()> {
    let status = Command::new("cargo")
        .args(["build", "--bin", BINARY_NAME])
        .current_dir(root)
        .status()
        .context("failed to run cargo build for live smoke binary")?;
    if !status.success() {
        bail!("cargo build --bin {BINARY_NAME} failed");
    }
    Ok(())
}

fn stop_broker(state_dir: &Path) {
    let pid = fs::read_to_string(state_dir.join("broker.pid"))
        .ok()
        .and_then(|pid| pid.trim().parse::<u32>().ok());
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
}

fn close_target_via_cdp(cdp_endpoint: &str, target_id: &str) -> Result<()> {
    let close_url = format!(
        "{}/json/close/{target_id}",
        cdp_endpoint.trim_end_matches('/')
    );
    let output = Command::new("curl")
        .args(["-fsS", &close_url])
        .output()
        .with_context(|| format!("failed to call Chrome close endpoint `{close_url}`"))?;
    if !output.status.success() {
        bail!(
            "Chrome close endpoint failed for target `{target_id}`: {}",
            String::from_utf8_lossy(&output.stderr)
        );
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

fn target_is_listed(cdp_endpoint: &str, target_id: &str) -> Result<bool> {
    let list_url = format!("{}/json/list", cdp_endpoint.trim_end_matches('/'));
    let output = Command::new("curl")
        .args(["-fsS", &list_url])
        .output()
        .with_context(|| format!("failed to call Chrome list endpoint `{list_url}`"))?;
    if !output.status.success() {
        bail!(
            "Chrome list endpoint failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let targets: Value = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("Chrome list endpoint returned invalid JSON from `{list_url}`"))?;
    let targets = targets
        .as_array()
        .context("Chrome list endpoint did not return an array")?;
    Ok(targets
        .iter()
        .any(|target| target.get("id").and_then(Value::as_str) == Some(target_id)))
}

fn tabs_include_id(tabs: &[Value], tab_id: &str) -> bool {
    tabs.iter()
        .any(|tab| tab.get("tab_id").and_then(Value::as_str) == Some(tab_id))
}

fn remove_open_tab(open_tabs: &mut Vec<OpenTab>, tab_id: &str) {
    open_tabs.retain(|tab| tab.tab_id != tab_id);
}

fn field_str(value: &Value, field: &str) -> Result<String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .with_context(|| format!("missing string field `{field}` in {value}"))
}

fn data_url(title: &str, body: &str) -> String {
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

fn unix_millis() -> Result<u128> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before UNIX_EPOCH")?
        .as_millis())
}

fn write_plugin_archive(
    root: &Path,
    host: &AgentHost,
    target: &str,
    binary: &Path,
    archive: &Path,
) -> Result<()> {
    let file = File::create(archive)
        .with_context(|| format!("failed to create archive `{}`", archive.display()))?;
    let mut zip = ZipWriter::new(file);
    let binary_name = binary_file_name(target);
    let mcp_config = mcp_config_bytes(&binary_name)?;
    let manifest = host_manifest_bytes(root, host, target, &binary_name)?;
    let package_manifest = package_manifest_bytes(host, target, &binary_name)?;

    add_bytes(&mut zip, host.manifest_path, &manifest, 0o644)?;
    add_bytes(&mut zip, ".mcp.json", &mcp_config, 0o644)?;
    add_file(
        &mut zip,
        "skills/visible-browser-lab/SKILL.md",
        &root.join("skills/visible-browser-lab/SKILL.md"),
        0o644,
    )?;
    add_file(
        &mut zip,
        &format!("bin/{binary_name}"),
        binary,
        executable_mode(target),
    )?;
    add_bytes(&mut zip, "package-manifest.json", &package_manifest, 0o644)?;

    zip.finish()
        .with_context(|| format!("failed to finish archive `{}`", archive.display()))?;
    validate_plugin_archive(archive)?;
    Ok(())
}

fn write_binary_archive(target: &str, binary: &Path, archive: &Path) -> Result<()> {
    let file = File::create(archive)
        .with_context(|| format!("failed to create archive `{}`", archive.display()))?;
    let mut zip = ZipWriter::new(file);
    let binary_name = binary_file_name(target);
    let readme = format!(
        "\
visible-browser-lab MCP broker

target: {target}
binary: {binary_name}

This archive is for debugging or manual installation. Plugin hosts should use
the host-specific visible-browser-lab package archives from the same release.
"
    );

    add_file(&mut zip, &binary_name, binary, executable_mode(target))?;
    add_bytes(&mut zip, "README.txt", readme.as_bytes(), 0o644)?;
    zip.finish()
        .with_context(|| format!("failed to finish archive `{}`", archive.display()))?;
    validate_binary_archive(archive)?;
    Ok(())
}

fn add_file<W: Write + Seek>(
    zip: &mut ZipWriter<W>,
    name: &str,
    source: &Path,
    mode: u32,
) -> Result<()> {
    let bytes =
        fs::read(source).with_context(|| format!("failed to read `{}`", source.display()))?;
    add_bytes(zip, name, &bytes, mode)
}

fn add_bytes<W: Write + Seek>(
    zip: &mut ZipWriter<W>,
    name: &str,
    bytes: &[u8],
    mode: u32,
) -> Result<()> {
    if name.starts_with('/')
        || name.contains("..")
        || name.contains('\\')
        || forbidden_archive_path(name)
    {
        bail!("refusing to add unsafe archive path `{name}`");
    }

    let options: FileOptions<'_, ExtendedFileOptions> = FileOptions::default()
        .compression_method(CompressionMethod::Deflated)
        .unix_permissions(mode);
    zip.start_file(name, options)
        .with_context(|| format!("failed to start archive entry `{name}`"))?;
    zip.write_all(bytes)
        .with_context(|| format!("failed to write archive entry `{name}`"))?;
    Ok(())
}

fn host_manifest_bytes(
    root: &Path,
    host: &AgentHost,
    target: &str,
    binary_name: &str,
) -> Result<Vec<u8>> {
    if host.id == "codex" {
        let source = fs::read_to_string(root.join(".codex-plugin/plugin.json"))
            .context("failed to read Codex plugin manifest")?;
        let mut manifest: Value =
            serde_json::from_str(&source).context("invalid Codex manifest JSON")?;
        manifest["mcpServers"] = Value::String("./.mcp.json".to_string());
        manifest["packaging"] = json!({
            "kind": "trusted-binary-release",
            "target": target,
            "binary": format!("bin/{binary_name}"),
        });
        return Ok(serde_json::to_vec_pretty(&manifest)?);
    }

    let manifest = json!({
        "name": "visible-browser-lab",
        "version": plugin_version(root)?,
        "displayName": format!("Visible Browser Lab ({})", host.display_name),
        "description": "Visible Chrome automation through a shared CDP endpoint",
        "skills": "./skills/",
        "mcpServers": "./.mcp.json",
        "packaging": {
            "kind": "trusted-binary-release",
            "target": target,
            "binary": format!("bin/{binary_name}"),
        },
    });
    Ok(serde_json::to_vec_pretty(&manifest)?)
}

fn mcp_config_bytes(binary_name: &str) -> Result<Vec<u8>> {
    let config = json!({
        "mcpServers": {
            "visible-browser-lab": {
                "command": format!("./bin/{binary_name}"),
                "args": []
            }
        }
    });
    Ok(serde_json::to_vec_pretty(&config)?)
}

fn package_manifest_bytes(host: &AgentHost, target: &str, binary_name: &str) -> Result<Vec<u8>> {
    let manifest = json!({
        "name": "visible-browser-lab",
        "host": host.id,
        "target": target,
        "binary": format!("bin/{binary_name}"),
        "mcp_server": "visible-browser-lab",
        "source_commit": git_head().ok(),
    });
    Ok(serde_json::to_vec_pretty(&manifest)?)
}

fn plugin_version(root: &Path) -> Result<String> {
    let source = fs::read_to_string(root.join(".codex-plugin/plugin.json"))
        .context("failed to read Codex plugin manifest")?;
    let manifest: Value = serde_json::from_str(&source).context("invalid Codex manifest JSON")?;
    manifest
        .get("version")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .context("Codex plugin manifest is missing string `version`")
}

fn validate_archives(dir: &Path) -> Result<()> {
    for path in archive_files(dir)? {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        if name == "SHA256SUMS" {
            continue;
        }

        if name.starts_with("visible-browser-lab-mcp-") {
            validate_binary_archive(&path)?;
        } else if name.starts_with("visible-browser-lab-") {
            validate_plugin_archive(&path)?;
        }
    }

    Ok(())
}

fn validate_plugin_archive(path: &Path) -> Result<()> {
    let mut archive = open_zip(path)?;
    let mut names = Vec::new();
    let mut mcp_config = None;

    for index in 0..archive.len() {
        let mut file = archive.by_index(index)?;
        let name = file.name().to_string();
        if forbidden_archive_path(&name) {
            bail!(
                "archive `{}` contains forbidden path `{name}`",
                path.display()
            );
        }

        if name == ".mcp.json" {
            let mut contents = String::new();
            file.read_to_string(&mut contents)?;
            mcp_config = Some(contents);
        }

        names.push(name);
    }

    let binary_count = names
        .iter()
        .filter(|name| name.starts_with("bin/visible-browser-lab-mcp"))
        .count();
    if binary_count != 1 {
        bail!(
            "archive `{}` must contain exactly one packaged binary, found {binary_count}",
            path.display()
        );
    }

    for required in [
        ".mcp.json",
        "skills/visible-browser-lab/SKILL.md",
        "package-manifest.json",
    ] {
        if !names.iter().any(|name| name == required) {
            bail!("archive `{}` is missing `{required}`", path.display());
        }
    }

    let has_host_manifest = AGENT_HOSTS
        .iter()
        .any(|host| names.iter().any(|name| name == host.manifest_path));
    if !has_host_manifest {
        bail!(
            "archive `{}` is missing a host plugin manifest",
            path.display()
        );
    }

    let mcp_config =
        mcp_config.with_context(|| format!("archive `{}` is missing .mcp.json", path.display()))?;
    if mcp_config.contains("npx")
        || mcp_config.contains("node")
        || mcp_config.contains("cargo")
        || !mcp_config.contains("./bin/visible-browser-lab-mcp")
    {
        bail!(
            "archive `{}` has an invalid generated MCP config",
            path.display()
        );
    }

    Ok(())
}

fn validate_binary_archive(path: &Path) -> Result<()> {
    let mut archive = open_zip(path)?;
    let mut binary_count = 0;

    for index in 0..archive.len() {
        let file = archive.by_index(index)?;
        let name = file.name();
        if forbidden_archive_path(name) {
            bail!(
                "archive `{}` contains forbidden path `{name}`",
                path.display()
            );
        }

        if name == BINARY_NAME || name == format!("{BINARY_NAME}.exe") {
            binary_count += 1;
        }
    }

    if binary_count != 1 {
        bail!(
            "archive `{}` must contain exactly one binary, found {binary_count}",
            path.display()
        );
    }

    Ok(())
}

fn open_zip(path: &Path) -> Result<ZipArchive<File>> {
    let file =
        File::open(path).with_context(|| format!("failed to open archive `{}`", path.display()))?;
    ZipArchive::new(file).with_context(|| format!("invalid zip archive `{}`", path.display()))
}

fn archive_files(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut files = Vec::new();
    collect_files(dir, &mut files)?;
    files.retain(|path| {
        path.extension().and_then(|ext| ext.to_str()) == Some("zip")
            || path.file_name().is_some_and(|name| name == "SHA256SUMS")
    });
    Ok(files)
}

fn collect_files(dir: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("failed to read `{}`", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, files)?;
        } else if path.is_file() {
            files.push(path);
        }
    }

    Ok(())
}

fn forbidden_archive_path(name: &str) -> bool {
    name.starts_with(".git/")
        || name.starts_with(".exo/runtime/")
        || name.starts_with("target/")
        || name.contains("/.git/")
        || name.contains("/.exo/runtime/")
        || name.contains("/target/")
        || name.contains(".DS_Store")
        || name.contains("/logs/")
        || name.starts_with("logs/")
        || name.contains("/cache/")
        || name.starts_with("cache/")
}

fn binary_path(root: &Path, target: &str, override_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        if path.is_file() {
            return Ok(path.to_path_buf());
        }

        bail!("binary override does not exist: {}", path.display());
    }

    let binary_name = binary_file_name(target);
    let mut candidates = vec![
        root.join("target")
            .join(target)
            .join("release")
            .join(&binary_name),
    ];
    if host_target()? == target {
        candidates.push(root.join("target").join("release").join(&binary_name));
    }

    for candidate in &candidates {
        if candidate.is_file() {
            return Ok(candidate.clone());
        }
    }

    let candidates = candidates
        .iter()
        .map(|path| format!("  - {}", path.display()))
        .collect::<Vec<_>>()
        .join("\n");
    bail!("release binary for `{target}` not found. Checked:\n{candidates}");
}

fn binary_file_name(target: &str) -> String {
    if target.contains("windows") {
        format!("{BINARY_NAME}.exe")
    } else {
        BINARY_NAME.to_string()
    }
}

fn executable_mode(target: &str) -> u32 {
    if target.contains("windows") {
        0o644
    } else {
        0o755
    }
}

fn ensure_supported_target(target: &str) -> Result<()> {
    if SUPPORTED_TARGETS.contains(&target) {
        Ok(())
    } else {
        bail!(
            "unsupported target `{target}`. Supported targets: {}",
            SUPPORTED_TARGETS.join(", ")
        )
    }
}

fn host_target() -> Result<String> {
    let output = Command::new("rustc")
        .arg("-vV")
        .output()
        .context("failed to run rustc -vV")?;
    if !output.status.success() {
        bail!("rustc -vV failed");
    }

    let stdout = String::from_utf8(output.stdout).context("rustc -vV output was not UTF-8")?;
    for line in stdout.lines() {
        if let Some(host) = line.strip_prefix("host: ") {
            return Ok(host.to_string());
        }
    }

    bail!("rustc -vV output did not include host target")
}

fn repo_root() -> Result<PathBuf> {
    let mut dir = env::current_dir().context("failed to read current directory")?;

    loop {
        if dir.join(".codex-plugin/plugin.json").is_file() && dir.join("Cargo.toml").is_file() {
            return Ok(dir);
        }

        if !dir.pop() {
            bail!("could not find visible-browser-lab repository root");
        }
    }
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        File::open(path).with_context(|| format!("failed to open `{}`", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 64 * 1024];

    loop {
        let bytes = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read `{}`", path.display()))?;
        if bytes == 0 {
            break;
        }
        hasher.update(&buffer[..bytes]);
    }

    let digest = hasher.finalize();
    Ok(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn git_head() -> Result<String> {
    let output = Command::new("git")
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .context("failed to run git rev-parse")?;
    if !output.status.success() {
        bail!("git rev-parse failed");
    }

    Ok(String::from_utf8(output.stdout)
        .context("git rev-parse output was not UTF-8")?
        .trim()
        .to_string())
}
