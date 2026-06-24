use std::{process::Command, time::Duration};

use anyhow::{Context, Result, bail};
use base64::Engine;
use chromiumoxide::{
    Browser, Command as CdpCommand, Method,
    cdp::{
        browser_protocol::{
            input::{
                DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams,
                DispatchMouseEventType, InsertTextParams, MouseButton,
            },
            network::EventRequestWillBeSent,
            page::{CaptureScreenshotFormat, CaptureScreenshotParams},
            target::{CreateTargetParams, CreateTargetReturns, TargetId},
        },
        js_protocol::runtime::EventConsoleApiCalled,
    },
    handler::HandlerConfig,
    page::Page,
};
use futures_util::StreamExt;
use serde::Serialize;
use visible_browser_lab_test_support::{BrowserMode, FixtureServer, RealBrowser};

const EVENT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CreateTargetWithoutApplicationFocus {
    url: String,
    background: bool,
    focus: bool,
}

impl Method for CreateTargetWithoutApplicationFocus {
    fn identifier(&self) -> chromiumoxide::types::MethodId {
        "Target.createTarget".into()
    }
}

impl CdpCommand for CreateTargetWithoutApplicationFocus {
    type Response = CreateTargetReturns;
}

#[tokio::main]
async fn main() -> Result<()> {
    let mode = BrowserMode::from_env()?;
    let original_frontmost = frontmost_application()?;
    let fixture = FixtureServer::start()?;
    let mut chrome = tokio::task::spawn_blocking(move || RealBrowser::launch(mode))
        .await
        .context("Chrome launch task failed")??;

    restore_frontmost_application(original_frontmost.as_deref())?;
    let frontmost_before_target = frontmost_application()?;
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
    eprintln!("spike: connected to {endpoint}");
    let handler_task = tokio::spawn(async move {
        while let Some(result) = handler.next().await {
            result.context("chromiumoxide handler failed")?;
        }
        Ok::<(), anyhow::Error>(())
    });

    let target_params = CreateTargetParams::builder()
        .url("about:blank")
        .background(true)
        .build()
        .map_err(anyhow::Error::msg)?;
    let created = tokio::time::timeout(EVENT_TIMEOUT, browser.execute(target_params))
        .await
        .context("timed out creating background target")?
        .context("failed to create background target")?;
    eprintln!(
        "spike: created target {}",
        created.result.target_id.as_ref()
    );
    let page = wait_for_page(&browser, created.result.target_id.clone()).await?;
    eprintln!("spike: acquired page handle");
    let frontmost_after_target = frontmost_application()?;

    let mut console_events = tokio::time::timeout(
        EVENT_TIMEOUT,
        page.event_listener::<EventConsoleApiCalled>(),
    )
    .await
    .context("timed out subscribing to console events")?
    .context("failed to subscribe to console events")?;
    let mut network_events = tokio::time::timeout(
        EVENT_TIMEOUT,
        page.event_listener::<EventRequestWillBeSent>(),
    )
    .await
    .context("timed out subscribing to network events")?
    .context("failed to subscribe to network events")?;
    eprintln!("spike: subscribed to page events");

    tokio::time::timeout(EVENT_TIMEOUT, page.goto(fixture.url("/")))
        .await
        .context("timed out navigating fixture page")?
        .context("failed to navigate fixture page")?;
    eprintln!("spike: navigated fixture page");
    let title: String =
        tokio::time::timeout(EVENT_TIMEOUT, page.evaluate_expression("document.title"))
            .await
            .context("timed out evaluating document.title")?
            .context("failed to evaluate document.title")?
            .into_value()
            .context("document.title was not a string")?;
    if title != "VBL Fixture" {
        bail!("unexpected fixture title `{title}`");
    }
    eprintln!("spike: evaluated title");

    click_selector(&page, "#clicker").await?;
    let clicked_by_input: String = tokio::time::timeout(
        EVENT_TIMEOUT,
        page.evaluate_expression("document.body.dataset.clicked || 'missing'"),
    )
    .await
    .context("timed out inspecting click result")?
    .context("failed to inspect click result")?
    .into_value()
    .context("click result was not text")?;
    let (clicked, click_status) = if clicked_by_input == "yes" {
        ("yes".to_string(), "input_dispatch")
    } else {
        tokio::time::timeout(
            EVENT_TIMEOUT,
            page.evaluate_expression("document.querySelector('#clicker').click()"),
        )
        .await
        .context("timed out using DOM click fallback")?
        .context("failed to use DOM click fallback")?;
        let clicked: String = page
            .evaluate_expression("document.body.dataset.clicked")
            .await?
            .into_value()
            .context("DOM click fallback did not update the fixture")?;
        (clicked, "dom_click_fallback")
    };
    eprintln!("spike: clicked fixture button");

    tokio::time::timeout(EVENT_TIMEOUT, async {
        page.evaluate_expression("document.querySelector('#entry').focus()")
            .await
            .context("failed to focus fixture input")?;
        page.execute(InsertTextParams::new("chromiumoxide"))
            .await
            .context("failed to insert fixture text")?;
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("timed out typing into fixture input")??;
    let typed_by_input: String = tokio::time::timeout(
        EVENT_TIMEOUT,
        page.evaluate_expression("document.querySelector('#entry').value"),
    )
    .await
    .context("timed out inspecting typed text")?
    .context("failed to inspect typed text")?
    .into_value()
    .context("typed value was not a string")?;
    let (typed, type_status) = if typed_by_input == "chromiumoxide" {
        (typed_by_input, "input_insert_text")
    } else {
        tokio::time::timeout(
            EVENT_TIMEOUT,
            page.evaluate_expression(
                "(() => { const input = document.querySelector('#entry'); const setter = Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set; setter.call(input, 'chromiumoxide'); input.dispatchEvent(new InputEvent('input', { bubbles: true, inputType: 'insertText', data: 'chromiumoxide' })); return input.value; })()",
            ),
        )
        .await
        .context("timed out using DOM text fallback")?
        .context("failed to use DOM text fallback")?;
        ("chromiumoxide".to_string(), "dom_input_fallback")
    };
    eprintln!("spike: typed into fixture input");

    for event_type in [DispatchKeyEventType::KeyDown, DispatchKeyEventType::KeyUp] {
        let params = DispatchKeyEventParams::builder()
            .r#type(event_type)
            .key("Enter")
            .code("Enter")
            .windows_virtual_key_code(13)
            .native_virtual_key_code(13)
            .build()
            .map_err(anyhow::Error::msg)?;
        tokio::time::timeout(EVENT_TIMEOUT, page.execute(params))
            .await
            .context("timed out dispatching key event")?
            .context("failed to dispatch key event")?;
    }
    let pressed_key: String = page
        .evaluate_expression("document.body.dataset.key || 'missing'")
        .await?
        .into_value()
        .context("key result was not text")?;
    let press_key_status = if pressed_key == "Enter" {
        "input_dispatch"
    } else {
        "not_delivered_to_background_target"
    };

    let data_url = fixture.url("/data.json");
    let fetch_expression = format!("fetch({})", serde_json::to_string(&data_url)?);
    tokio::time::timeout(EVENT_TIMEOUT, page.evaluate_expression(fetch_expression))
        .await
        .context("timed out starting fixture fetch")?
        .context("failed to start fixture fetch")?;

    let console_event = tokio::time::timeout(EVENT_TIMEOUT, console_events.next())
        .await
        .context("timed out waiting for console event")?
        .context("console event stream ended")?;
    let network_url = wait_for_network_url(&mut network_events, &data_url).await?;
    eprintln!("spike: received console and network events");
    let frontmost_after_actions = frontmost_application()?;

    let screenshot_params = CaptureScreenshotParams::builder()
        .format(CaptureScreenshotFormat::Png)
        .capture_beyond_viewport(true)
        .build();
    let background_screenshot =
        tokio::time::timeout(EVENT_TIMEOUT, page.execute(screenshot_params.clone())).await;
    let (screenshot_data, screenshot_status) = match background_screenshot {
        Ok(Ok(response)) => (Some(response.result.data), "background"),
        Ok(Err(error)) => {
            eprintln!("spike: background screenshot failed: {error:#}");
            (None, "background_error")
        }
        Err(_) => {
            tokio::time::timeout(EVENT_TIMEOUT, page.bring_to_front())
                .await
                .context("timed out activating page for screenshot")?
                .context("failed to activate page for screenshot")?;
            match tokio::time::timeout(EVENT_TIMEOUT, page.execute(screenshot_params)).await {
                Ok(Ok(response)) => (Some(response.result.data), "required_activation"),
                Ok(Err(error)) => {
                    eprintln!("spike: activated screenshot failed: {error:#}");
                    (None, "activated_error")
                }
                Err(_) => (None, "timed_out_before_and_after_activation"),
            }
        }
    };
    let screenshot = screenshot_data
        .map(|data| {
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .context("screenshot response was not valid base64")
        })
        .transpose()?
        .unwrap_or_default();
    if !screenshot.is_empty() && !screenshot.starts_with(b"\x89PNG\r\n\x1a\n") {
        bail!("screenshot did not contain a PNG signature");
    }
    let frontmost_after_screenshot = frontmost_application()?;

    let active_target = tokio::time::timeout(
        EVENT_TIMEOUT,
        browser.execute(CreateTargetWithoutApplicationFocus {
            url: "about:blank".to_string(),
            background: false,
            focus: false,
        }),
    )
    .await
    .context("timed out creating active non-focusing target")?
    .context("failed to create active non-focusing target")?;
    let active_page = wait_for_page(&browser, active_target.result.target_id).await?;
    active_page.goto(fixture.url("/")).await?;
    click_selector(&active_page, "#clicker").await?;
    let active_target_clicked: String = active_page
        .evaluate_expression("document.body.dataset.clicked || 'missing'")
        .await?
        .into_value()
        .context("active target click result was not text")?;
    let frontmost_after_active_target = frontmost_application()?;
    tokio::time::timeout(EVENT_TIMEOUT, active_page.close())
        .await
        .context("timed out closing active non-focusing target")?
        .context("failed to close active non-focusing target")?;

    let target_id = page.target_id().as_ref().to_string();
    tokio::time::timeout(EVENT_TIMEOUT, page.close())
        .await
        .context("timed out closing spike page")?
        .context("failed to close spike page")?;
    drop(browser);
    chrome.shutdown();
    let handler_exit = wait_for_handler(handler_task, "initial").await?;

    let mut restarted_chrome =
        tokio::task::spawn_blocking(|| RealBrowser::launch(BrowserMode::Headless))
            .await
            .context("restarted Chrome launch task failed")??;
    let restarted_endpoint = restarted_chrome.cdp_endpoint().to_string();
    let (reconnected, mut reconnected_handler) = Browser::connect(&restarted_endpoint)
        .await
        .with_context(|| format!("failed to reconnect to {restarted_endpoint}"))?;
    let reconnected_task = tokio::spawn(async move {
        while let Some(result) = reconnected_handler.next().await {
            result.context("reconnected chromiumoxide handler failed")?;
        }
        Ok::<(), anyhow::Error>(())
    });
    let high_level_page = tokio::time::timeout(
        EVENT_TIMEOUT,
        reconnected.new_page("data:text/html,<title>reconnected</title>"),
    )
    .await;
    let (reconnected_page, reconnected_new_page_status) = match high_level_page {
        Ok(Ok(page)) => (page, "high_level"),
        Ok(Err(error)) => {
            eprintln!("spike: reconnected Browser::new_page failed: {error:#}");
            let created = reconnected
                .execute(
                    CreateTargetParams::builder()
                        .url("data:text/html,<title>reconnected</title>")
                        .background(true)
                        .build()
                        .map_err(anyhow::Error::msg)?,
                )
                .await?;
            (
                wait_for_page(&reconnected, created.result.target_id).await?,
                "typed_fallback_after_error",
            )
        }
        Err(_) => {
            let created = reconnected
                .execute(
                    CreateTargetParams::builder()
                        .url("data:text/html,<title>reconnected</title>")
                        .background(true)
                        .build()
                        .map_err(anyhow::Error::msg)?,
                )
                .await?;
            (
                wait_for_page(&reconnected, created.result.target_id).await?,
                "typed_fallback_after_timeout",
            )
        }
    };
    let reconnected_title: String = tokio::time::timeout(
        EVENT_TIMEOUT,
        reconnected_page.evaluate_expression("document.title"),
    )
    .await
    .context("timed out evaluating after reconnect")?
    .context("failed to evaluate after reconnect")?
    .into_value()
    .context("reconnected title was not a string")?;
    reconnected_page.close().await?;
    drop(reconnected);
    restarted_chrome.shutdown();
    let reconnected_handler_exit = wait_for_handler(reconnected_task, "reconnected").await?;

    println!("endpoint={endpoint}");
    println!("target_id={target_id}");
    println!("title={title}");
    println!("clicked={clicked}");
    println!("click_status={click_status}");
    println!("typed={typed}");
    println!("type_status={type_status}");
    println!("pressed_key={pressed_key}");
    println!("press_key_status={press_key_status}");
    println!("console_event_type={:?}", console_event.r#type);
    println!("network_url={network_url}");
    println!("screenshot_bytes={}", screenshot.len());
    println!("screenshot_status={screenshot_status}");
    println!("active_non_focusing_click={active_target_clicked}");
    println!("handler_exit={handler_exit}");
    println!("reconnected_new_page_status={reconnected_new_page_status}");
    println!("reconnected_handler_exit={reconnected_handler_exit}");
    println!("reconnected_title={reconnected_title}");
    if let (Some(before), Some(after)) = (&frontmost_before_target, &frontmost_after_target) {
        println!("frontmost_before_target={before}");
        println!("frontmost_after_target={after}");
        println!("target_creation_preserved_frontmost={}", before == after);
    }
    if let (Some(before), Some(after)) = (&frontmost_after_target, &frontmost_after_actions) {
        println!("frontmost_after_actions={after}");
        println!("page_actions_preserved_frontmost={}", before == after);
    }
    if let (Some(before), Some(after)) = (&frontmost_after_actions, &frontmost_after_screenshot) {
        println!("frontmost_after_screenshot={after}");
        println!("screenshot_preserved_frontmost={}", before == after);
    }
    if let (Some(before), Some(after)) =
        (&frontmost_after_screenshot, &frontmost_after_active_target)
    {
        println!("frontmost_after_active_target={after}");
        println!("active_target_preserved_frontmost={}", before == after);
    }

    Ok(())
}

async fn wait_for_network_url(
    events: &mut chromiumoxide::listeners::EventStream<EventRequestWillBeSent>,
    expected: &str,
) -> Result<String> {
    tokio::time::timeout(EVENT_TIMEOUT, async {
        while let Some(event) = events.next().await {
            if event.request.url == expected {
                return Ok(event.request.url.clone());
            }
        }
        bail!("network event stream ended before `{expected}`")
    })
    .await
    .with_context(|| format!("timed out waiting for network event `{expected}`"))?
}

async fn click_selector(page: &Page, selector: &str) -> Result<()> {
    let expression = format!(
        "(() => {{ const element = document.querySelector({selector}); if (!element) throw new Error('selector not found'); element.scrollIntoView({{ block: 'center', inline: 'center' }}); const rect = element.getBoundingClientRect(); return {{ x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 }}; }})()",
        selector = serde_json::to_string(selector)?
    );
    let point: serde_json::Value =
        tokio::time::timeout(EVENT_TIMEOUT, page.evaluate_expression(expression))
            .await
            .context("timed out locating click target")?
            .context("failed to locate click target")?
            .into_value()
            .context("click target did not return a point")?;
    let x = point
        .get("x")
        .and_then(serde_json::Value::as_f64)
        .context("click target omitted x")?;
    let y = point
        .get("y")
        .and_then(serde_json::Value::as_f64)
        .context("click target omitted y")?;

    for event_type in [
        DispatchMouseEventType::MousePressed,
        DispatchMouseEventType::MouseReleased,
    ] {
        let params = DispatchMouseEventParams::builder()
            .r#type(event_type)
            .x(x)
            .y(y)
            .button(MouseButton::Left)
            .click_count(1)
            .build()
            .map_err(anyhow::Error::msg)?;
        tokio::time::timeout(EVENT_TIMEOUT, page.execute(params))
            .await
            .context("timed out dispatching mouse event")?
            .context("failed to dispatch mouse event")?;
    }
    Ok(())
}

async fn wait_for_page(browser: &Browser, target_id: TargetId) -> Result<Page> {
    tokio::time::timeout(EVENT_TIMEOUT, async {
        loop {
            if let Ok(page) = browser.get_page(target_id.clone()).await {
                return Ok(page);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .with_context(|| {
        format!(
            "timed out waiting for chromiumoxide page `{}`",
            target_id.as_ref()
        )
    })?
}

async fn wait_for_handler(
    task: tokio::task::JoinHandle<Result<()>>,
    label: &str,
) -> Result<String> {
    let joined = tokio::time::timeout(EVENT_TIMEOUT, task)
        .await
        .with_context(|| format!("{label} handler did not stop after Chrome exited"))?
        .with_context(|| format!("{label} handler task panicked"))?;
    Ok(match joined {
        Ok(()) => "clean".to_string(),
        Err(error) => format!("websocket_error:{error:#}"),
    })
}

#[cfg(target_os = "macos")]
fn frontmost_application() -> Result<Option<String>> {
    let output = Command::new("osascript")
        .args([
            "-e",
            "tell application \"System Events\" to get name of first application process whose frontmost is true",
        ])
        .output()
        .context("failed to query the frontmost macOS application")?;
    if !output.status.success() {
        bail!(
            "frontmost application query failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(Some(
        String::from_utf8(output.stdout)
            .context("frontmost application name was not UTF-8")?
            .trim()
            .to_string(),
    ))
}

#[cfg(not(target_os = "macos"))]
fn frontmost_application() -> Result<Option<String>> {
    Ok(None)
}

#[cfg(target_os = "macos")]
fn restore_frontmost_application(application: Option<&str>) -> Result<()> {
    let Some(application) = application else {
        return Ok(());
    };
    let escaped = application.replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!(
        "tell application \"System Events\" to set frontmost of first application process whose name is \"{escaped}\" to true"
    );
    let status = Command::new("osascript")
        .args(["-e", &script])
        .status()
        .context("failed to restore the frontmost macOS application")?;
    if !status.success() {
        bail!("failed to reactivate `{application}`");
    }
    std::thread::sleep(Duration::from_millis(500));
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn restore_frontmost_application(_application: Option<&str>) -> Result<()> {
    Ok(())
}
