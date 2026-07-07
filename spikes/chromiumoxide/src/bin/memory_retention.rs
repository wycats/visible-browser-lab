//! Discriminating experiment for CDP remote-object retention.
//!
//! Hypothesis: `Runtime.evaluate` with `returnByValue: false` and no
//! `Runtime.releaseObjectGroup` pins detached DOM trees in the renderer,
//! which is how a long agent session drives Chrome to multi-GB RSS.
//!
//! Two conditions, fresh browser each:
//!   retain  — resolve an element handle each iteration, never release
//!   release — same workload, but release the object group each iteration
//!
//! Each iteration builds a ~5k-node DOM tree, resolves its root as a remote
//! object, then replaces the body (detaching the previous tree). If handles
//! pin detached trees, the retain condition's renderer RSS grows linearly
//! and the release condition's stays flat.

use std::{collections::HashMap, process::Command, time::Duration};

use anyhow::{Context, Result, bail};
use chromiumoxide::{
    Browser,
    cdp::{
        browser_protocol::{
            performance::{EnableParams as PerformanceEnableParams, GetMetricsParams},
            target::{CreateTargetParams, TargetId},
        },
        js_protocol::{
            heap_profiler::CollectGarbageParams,
            runtime::{EvaluateParams as RuntimeEvaluateParams, ReleaseObjectGroupParams},
        },
    },
    handler::HandlerConfig,
    page::Page,
};
use futures_util::StreamExt;
use visible_browser_lab_test_support::{BrowserMode, RealBrowser};

const ITERATIONS: usize = 40;
const NODES_PER_TREE: usize = 5_000;
const OBJECT_GROUP: &str = "vbl-spike";

#[derive(Debug, Clone, Copy)]
struct Sample {
    rss_mb: f64,
    dom_nodes: f64,
    js_heap_mb: f64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let retain = run_condition(false).await?;
    let release = run_condition(true).await?;

    println!();
    println!("condition  nodes_start  nodes_end  js_heap_start_mb  js_heap_end_mb  rss_end_mb");
    for (label, (start, end)) in [("retain", retain), ("release", release)] {
        println!(
            "{label:<9}  {:>11.0}  {:>9.0}  {:>16.1}  {:>14.1}  {:>10.1}",
            start.dom_nodes, end.dom_nodes, start.js_heap_mb, end.js_heap_mb, end.rss_mb
        );
    }
    Ok(())
}

async fn run_condition(release_group: bool) -> Result<(Sample, Sample)> {
    let label = if release_group { "release" } else { "retain" };
    let mut chrome = tokio::task::spawn_blocking(|| RealBrowser::launch(BrowserMode::Headless))
        .await
        .context("Chrome launch task failed")??;
    let profile_marker = chrome.profile_dir().to_string_lossy().into_owned();
    let endpoint = chrome.cdp_endpoint().to_string();
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
    page.execute(PerformanceEnableParams::default())
        .await
        .context("Performance.enable failed")?;

    // Warm up and take the baseline after the first tree exists.
    build_tree_and_resolve(&page, release_group).await?;
    force_gc(&page).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let start = sample(&page, &profile_marker).await?;
    eprintln!("{label}: baseline {start:?}");

    for iteration in 0..ITERATIONS {
        build_tree_and_resolve(&page, release_group).await?;
        if iteration % 10 == 9 {
            force_gc(&page).await?;
            let current = sample(&page, &profile_marker).await?;
            eprintln!("{label}: after {:>2} iterations {current:?}", iteration + 1);
        }
    }

    force_gc(&page).await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let end = sample(&page, &profile_marker).await?;
    eprintln!("{label}: final {end:?}");

    drop(browser);
    handler_task.abort();
    chrome.shutdown();
    Ok((start, end))
}

async fn sample(page: &Page, profile_marker: &str) -> Result<Sample> {
    let metrics = page
        .execute(GetMetricsParams::default())
        .await
        .context("Performance.getMetrics failed")?;
    let metric = |name: &str| {
        metrics
            .result
            .metrics
            .iter()
            .find(|m| m.name == name)
            .map(|m| m.value)
            .unwrap_or(f64::NAN)
    };
    Ok(Sample {
        rss_mb: renderer_rss_mb(profile_marker)?,
        dom_nodes: metric("Nodes"),
        js_heap_mb: metric("JSHeapUsedSize") / (1024.0 * 1024.0),
    })
}

/// Build a large DOM tree, resolve its root as a remote object (like the
/// broker's `evaluate_on_css` / `resolve_backend_node` paths), and replace
/// the body so the previous tree is detached.
async fn build_tree_and_resolve(page: &Page, release_group: bool) -> Result<()> {
    let expression = format!(
        r#"(() => {{
            const root = document.createElement('div');
            for (let i = 0; i < {NODES_PER_TREE}; i++) {{
                const el = document.createElement('div');
                el.textContent = 'node ' + i + ' ' + 'x'.repeat(200);
                root.appendChild(el);
            }}
            document.body.replaceChildren(root);
            return root;
        }})()"#
    );
    let result = page
        .execute(
            RuntimeEvaluateParams::builder()
                .expression(expression)
                .return_by_value(false)
                .object_group(OBJECT_GROUP)
                .build()
                .map_err(anyhow::Error::msg)?,
        )
        .await
        .context("evaluate failed")?;
    if result.result.result.object_id.is_none() {
        bail!("expected a remote object handle");
    }
    if release_group {
        page.execute(ReleaseObjectGroupParams::new(OBJECT_GROUP))
            .await
            .context("releaseObjectGroup failed")?;
    }
    Ok(())
}

async fn force_gc(page: &Page) -> Result<()> {
    page.execute(CollectGarbageParams::default())
        .await
        .context("collectGarbage failed")?;
    Ok(())
}

/// Sum RSS (in MB) of every descendant of the Chrome main process, which we
/// find by its unique --user-data-dir profile path.
fn renderer_rss_mb(profile_marker: &str) -> Result<f64> {
    let output = Command::new("ps")
        .args(["-axo", "pid=,ppid=,rss=,command="])
        .output()
        .context("ps failed")?;
    if !output.status.success() {
        bail!("ps exited with {}", output.status);
    }
    let listing = String::from_utf8_lossy(&output.stdout);
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut rss_kb: HashMap<u32, u64> = HashMap::new();
    let mut main_pid = None;
    for line in listing.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(ppid), Some(rss)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let command = fields.collect::<Vec<_>>().join(" ");
        let (Ok(pid), Ok(ppid), Ok(rss)) = (pid.parse(), ppid.parse::<u32>(), rss.parse()) else {
            continue;
        };
        children.entry(ppid).or_default().push(pid);
        rss_kb.insert(pid, rss);
        if command.contains(profile_marker) && command.contains("--user-data-dir") {
            main_pid = Some(pid);
        }
    }
    let main_pid = main_pid.context("Chrome main process not found")?;
    let mut total_kb = 0u64;
    let mut stack = vec![main_pid];
    while let Some(pid) = stack.pop() {
        total_kb += rss_kb.get(&pid).copied().unwrap_or(0);
        if let Some(kids) = children.get(&pid) {
            stack.extend(kids);
        }
    }
    Ok(total_kb as f64 / 1024.0)
}

async fn wait_for_page(browser: &Browser, target_id: TargetId) -> Result<Page> {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        match browser.get_page(target_id.clone()).await {
            Ok(page) => return Ok(page),
            Err(_) if std::time::Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(error) => return Err(error).context("failed to acquire page"),
        }
    }
}
