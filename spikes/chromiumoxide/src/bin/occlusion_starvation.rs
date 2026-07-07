//! Discriminating experiment for screencast starvation under macOS window
//! occlusion.
//!
//! PR #49 shipped `--disable-backgrounding-occluded-windows`,
//! `--disable-renderer-backgrounding`, and
//! `--disable-background-timer-throttling`. Timers now survive occlusion, but
//! screencasts still collapse to a single frame when the Chrome window is
//! fully covered. This spike separates the layers so we know which knob (if
//! any) restores frame production:
//!
//!   - timer ticks — does the renderer keep running JS? (the flags' job)
//!   - rAF frames  — does the compositor keep issuing BeginFrames?
//!   - screencast  — does Page.startScreencast keep delivering frames?
//!
//! Phases (Enter-gated so a human can cover the window with a NON-Chrome
//! window — on macOS 26 Chrome's manual occlusion detection is disabled, so
//! Chrome-over-Chrome may not trigger occlusion at all):
//!
//!   A  visible baseline
//!   B  occluded, no intervention
//!   C1 occluded + Emulation.setFocusEmulationEnabled(true), reset afterwards
//!   C2 occluded + Page.setWebLifecycleState(active), measured in isolation
//!   D  Target.activateTarget (fronting fallback — expected to un-occlude)
//!
//! Usage: occlusion_starvation [occlude|minimize|tab|window] [--bare]
//!
//!   occlude   (default) human covers the window with a non-Chrome window
//!   minimize  human minimizes the window to the Dock
//!   tab       spike opens+activates a second tab, screencasted tab becomes a
//!             background tab (no human action needed)
//!   window    spike opens+activates a second target with newWindow: true —
//!             tests whether own-window isolation protects the screencast
//!   --bare    drop the three production anti-throttling flags (control run)

use std::{
    fs,
    path::Path,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use chromiumoxide::{
    Browser,
    cdp::{
        browser_protocol::{
            emulation::SetFocusEmulationEnabledParams,
            page::{
                EnableParams as PageEnableParams, EventScreencastFrame,
                EventScreencastVisibilityChanged, ScreencastFrameAckParams,
                SetWebLifecycleStateParams, SetWebLifecycleStateState, StartScreencastFormat,
                StartScreencastParams,
            },
            target::{ActivateTargetParams, CreateTargetParams, TargetId},
        },
        js_protocol::runtime::EvaluateParams as RuntimeEvaluateParams,
    },
    handler::HandlerConfig,
    page::Page,
};
use futures_util::StreamExt;
use tempfile::TempDir;
use visible_browser_lab_test_support::chrome_for_testing_executable;

#[derive(Debug)]
struct PhaseReport {
    label: &'static str,
    seconds: u64,
    screencast_frames: u64,
    raf_delta: f64,
    ticks_delta: f64,
    visibility_end: String,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Occlude,
    Minimize,
    Tab,
    Window,
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut mode = Mode::Occlude;
    let mut bare = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "occlude" => mode = Mode::Occlude,
            "minimize" => mode = Mode::Minimize,
            "tab" => mode = Mode::Tab,
            "window" => mode = Mode::Window,
            "--bare" => bare = true,
            other => bail!("unknown argument `{other}`"),
        }
    }
    eprintln!("mode: {mode:?}, production flags: {}", !bare);
    let mut chrome = tokio::task::spawn_blocking(move || launch_visible_chrome(!bare))
        .await
        .context("Chrome launch task failed")??;
    let endpoint = chrome.endpoint.clone();
    let (browser, mut handler) = Browser::connect_with_config(
        endpoint.clone(),
        HandlerConfig {
            viewport: None,
            ..HandlerConfig::default()
        },
    )
    .await
    .with_context(|| format!("failed to connect chromiumoxide to {endpoint}"))?;
    let handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let created = browser
        .execute(
            CreateTargetParams::builder()
                .url("about:blank")
                .build()
                .map_err(anyhow::Error::msg)?,
        )
        .await
        .context("failed to create target")?;
    let page = wait_for_page(&browser, created.result.target_id.clone()).await?;
    page.execute(PageEnableParams::default())
        .await
        .context("Page.enable failed")?;
    install_counters(&page).await?;

    // Count screencast frames, acking each so Chrome keeps sending.
    let frames = Arc::new(AtomicU64::new(0));
    let frame_counter = Arc::clone(&frames);
    let ack_page = page.clone();
    let mut frame_events = page
        .event_listener::<EventScreencastFrame>()
        .await
        .context("failed to listen for screencast frames")?;
    let frame_task = tokio::spawn(async move {
        while let Some(frame) = frame_events.next().await {
            frame_counter.fetch_add(1, Ordering::Relaxed);
            let _ = ack_page
                .execute(ScreencastFrameAckParams::new(frame.session_id))
                .await;
        }
    });

    // Record screencastVisibilityChanged events with elapsed timestamps.
    let started_at = Instant::now();
    let mut visibility_events = page
        .event_listener::<EventScreencastVisibilityChanged>()
        .await
        .context("failed to listen for screencast visibility")?;
    let visibility_task = tokio::spawn(async move {
        while let Some(event) = visibility_events.next().await {
            eprintln!(
                "[{:>6.1}s] screencastVisibilityChanged visible={}",
                started_at.elapsed().as_secs_f64(),
                event.visible
            );
        }
    });

    page.execute(
        StartScreencastParams::builder()
            .format(StartScreencastFormat::Jpeg)
            .quality(60)
            .max_width(800)
            .max_height(600)
            .every_nth_frame(1)
            .build(),
    )
    .await
    .context("Page.startScreencast failed")?;

    let mut reports = Vec::new();

    reports.push(measure_phase(&page, &frames, "A visible baseline", 10).await?);

    match mode {
        Mode::Occlude => {
            pause_for_user(
                "Cover the Chrome window COMPLETELY with a non-Chrome window (e.g. this \
                 editor), keep it covered for the next three phases, then press Enter",
            )
            .await?;
        }
        Mode::Minimize => {
            pause_for_user(
                "MINIMIZE the Chrome window to the Dock, leave it minimized for the next \
                 three phases, then press Enter",
            )
            .await?;
        }
        Mode::Tab => {
            let second = browser
                .execute(
                    CreateTargetParams::builder()
                        .url("about:blank")
                        .build()
                        .map_err(anyhow::Error::msg)?,
                )
                .await
                .context("failed to create second target")?;
            browser
                .execute(ActivateTargetParams::new(second.result.target_id.clone()))
                .await
                .context("failed to activate second target")?;
            eprintln!("opened + activated a second tab; screencasted tab is now backgrounded");
        }
        Mode::Window => {
            // Same position and size as the casting window (launch args below)
            // so the second window covers it completely, not just overlaps.
            let second = browser
                .execute(
                    CreateTargetParams::builder()
                        .url("about:blank")
                        .new_window(true)
                        .left(80)
                        .top(80)
                        .width(800)
                        .height(600)
                        .build()
                        .map_err(anyhow::Error::msg)?,
                )
                .await
                .context("failed to create second window")?;
            browser
                .execute(ActivateTargetParams::new(second.result.target_id.clone()))
                .await
                .context("failed to activate second window")?;
            eprintln!(
                "opened + activated a second WINDOW directly over the casting window; \
                 screencasted tab keeps its own window"
            );
        }
    }

    reports.push(measure_phase(&page, &frames, "B occluded", 15).await?);

    page.execute(SetFocusEmulationEnabledParams::new(true))
        .await
        .context("Emulation.setFocusEmulationEnabled failed")?;
    reports.push(measure_phase(&page, &frames, "C1 + focus emulation", 15).await?);

    // Reset C1's intervention so C2 measures the lifecycle knob in isolation
    // rather than on top of still-engaged focus emulation.
    page.execute(SetFocusEmulationEnabledParams::new(false))
        .await
        .context("Emulation.setFocusEmulationEnabled(false) failed")?;
    reports.push(measure_phase(&page, &frames, "C1r emulation reset", 10).await?);

    page.execute(SetWebLifecycleStateParams::new(
        SetWebLifecycleStateState::Active,
    ))
    .await
    .context("Page.setWebLifecycleState failed")?;
    reports.push(measure_phase(&page, &frames, "C2 + lifecycle active", 15).await?);

    browser
        .execute(ActivateTargetParams::new(created.result.target_id.clone()))
        .await
        .context("Target.activateTarget failed")?;
    reports.push(measure_phase(&page, &frames, "D activateTarget", 10).await?);

    println!();
    println!(
        "phase                  secs  cast_frames  cast_fps  raf_delta  ticks_delta  visibility"
    );
    for report in &reports {
        println!(
            "{:<21}  {:>4}  {:>11}  {:>8.1}  {:>9.0}  {:>11.0}  {}",
            report.label,
            report.seconds,
            report.screencast_frames,
            report.screencast_frames as f64 / report.seconds as f64,
            report.raf_delta,
            report.ticks_delta,
            report.visibility_end,
        );
    }

    frame_task.abort();
    visibility_task.abort();
    drop(browser);
    handler_task.abort();
    chrome.shutdown();
    Ok(())
}

/// Install per-layer counters plus a 100ms repaint so the compositor always
/// has damage: if screencast frames stop, it is frame production that
/// starved, not a lack of changes to capture.
async fn install_counters(page: &Page) -> Result<()> {
    eval(
        page,
        r#"(() => {
            window.__frames = 0;
            window.__ticks = 0;
            const raf = () => { window.__frames++; requestAnimationFrame(raf); };
            requestAnimationFrame(raf);
            setInterval(() => { window.__ticks++; }, 100);
            setInterval(() => {
                document.body.textContent =
                    `raf=${window.__frames} ticks=${window.__ticks} ${Date.now()}`;
            }, 100);
            return true;
        })()"#,
    )
    .await?;
    Ok(())
}

async fn measure_phase(
    page: &Page,
    frames: &AtomicU64,
    label: &'static str,
    seconds: u64,
) -> Result<PhaseReport> {
    let (raf_start, ticks_start, _) = read_counters(page).await?;
    let frames_start = frames.load(Ordering::Relaxed);
    eprintln!("phase {label}: running for {seconds}s…");
    tokio::time::sleep(Duration::from_secs(seconds)).await;
    let (raf_end, ticks_end, visibility) = read_counters(page).await?;
    let frames_end = frames.load(Ordering::Relaxed);
    let report = PhaseReport {
        label,
        seconds,
        screencast_frames: frames_end - frames_start,
        raf_delta: raf_end - raf_start,
        ticks_delta: ticks_end - ticks_start,
        visibility_end: visibility,
    };
    eprintln!("phase {label}: {report:?}");
    Ok(report)
}

async fn read_counters(page: &Page) -> Result<(f64, f64, String)> {
    let value = eval(
        page,
        "[window.__frames, window.__ticks, document.visibilityState]",
    )
    .await?;
    let items = value
        .as_array()
        .context("counter sample was not an array")?;
    let number = |index: usize| {
        items
            .get(index)
            .and_then(|v| v.as_f64())
            .unwrap_or(f64::NAN)
    };
    let visibility = items
        .get(2)
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string();
    Ok((number(0), number(1), visibility))
}

async fn eval(page: &Page, expression: &str) -> Result<serde_json::Value> {
    let result = page
        .execute(
            RuntimeEvaluateParams::builder()
                .expression(expression)
                .return_by_value(true)
                .build()
                .map_err(anyhow::Error::msg)?,
        )
        .await
        .context("evaluate failed")?;
    Ok(result
        .result
        .result
        .value
        .clone()
        .unwrap_or(serde_json::Value::Null))
}

async fn pause_for_user(message: &str) -> Result<()> {
    eprintln!();
    eprintln!(">>> {message} <<<");
    tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        std::io::stdin().read_line(&mut line).map(|_| ())
    })
    .await
    .context("stdin task failed")?
    .context("failed to read stdin")?;
    Ok(())
}

struct SpikeChrome {
    child: Child,
    _profile_dir: TempDir,
    endpoint: String,
}

impl SpikeChrome {
    fn shutdown(&mut self) {
        if matches!(self.child.try_wait(), Ok(Some(_))) {
            return;
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for SpikeChrome {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Launch a visible Chrome for Testing, optionally with the same occlusion
/// flags the broker's managed Chrome uses (managed_chrome.rs) so the baseline
/// phase reproduces production behavior. `production_flags: false` is the
/// control run.
fn launch_visible_chrome(production_flags: bool) -> Result<SpikeChrome> {
    let executable = chrome_for_testing_executable()?;
    let profile_dir = tempfile::Builder::new()
        .prefix("vbl-occlusion-spike-")
        .tempdir()
        .context("failed to create profile directory")?;
    let mut command = Command::new(&executable);
    command
        .arg("--remote-debugging-port=0")
        .arg(format!("--user-data-dir={}", profile_dir.path().display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-background-networking")
        .arg("--disable-component-update")
        .arg("--disable-sync")
        .arg("--use-mock-keychain")
        .arg("--window-size=800,600")
        .arg("--window-position=80,80")
        .arg("about:blank")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if production_flags {
        command
            .arg("--disable-backgrounding-occluded-windows")
            .arg("--disable-renderer-backgrounding")
            .arg("--disable-background-timer-throttling");
    }
    let child = command.spawn().with_context(|| {
        format!(
            "failed to launch Chrome for Testing `{}`",
            executable.display()
        )
    })?;
    // Hold the child in the Drop guard before endpoint discovery: if Chrome
    // never writes DevToolsActivePort, the error path kills it instead of
    // leaking a headed Chrome and its profile.
    let mut chrome = SpikeChrome {
        child,
        _profile_dir: profile_dir,
        endpoint: String::new(),
    };
    chrome.endpoint = wait_for_devtools_endpoint(chrome._profile_dir.path())?;
    Ok(chrome)
}

fn wait_for_devtools_endpoint(profile_dir: &Path) -> Result<String> {
    let active_port = profile_dir.join("DevToolsActivePort");
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if let Ok(contents) = fs::read_to_string(&active_port)
            && let Some(port) = contents.lines().next()
            && let Ok(port) = port.trim().parse::<u16>()
        {
            return Ok(format!("http://127.0.0.1:{port}"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    bail!("timed out waiting for `{}`", active_port.display())
}

async fn wait_for_page(browser: &Browser, target_id: TargetId) -> Result<Page> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match browser.get_page(target_id.clone()).await {
            Ok(page) => return Ok(page),
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(error) => return Err(error).context("failed to acquire page"),
        }
    }
}
