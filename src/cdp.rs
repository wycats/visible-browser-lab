use std::{sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tokio::time::{Instant, timeout};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use url::Url;

use crate::leases::{BrowserToolError, TabSnapshot};
use crate::protocol::{EvaluateResult, NetworkEvent};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CdpEndpoint {
    origin: Url,
}

impl CdpEndpoint {
    pub fn parse(endpoint: &str) -> anyhow::Result<Self> {
        Ok(Self {
            origin: Url::parse(endpoint)?,
        })
    }

    pub fn version_url(&self) -> Url {
        self.join("/json/version")
    }

    pub fn targets_url(&self) -> Url {
        self.join("/json/list")
    }

    pub fn origin(&self) -> &Url {
        &self.origin
    }

    fn join(&self, path: &str) -> Url {
        self.origin
            .join(path)
            .expect("static CDP paths must be valid URLs")
    }
}

#[derive(Debug, Clone)]
pub struct CdpClient {
    endpoint: CdpEndpoint,
    http: reqwest::Client,
}

impl CdpClient {
    pub fn new(endpoint: &str) -> anyhow::Result<Self> {
        Ok(Self {
            endpoint: CdpEndpoint::parse(endpoint)?,
            http: reqwest::Client::new(),
        })
    }

    pub async fn browser_version(&self) -> Result<BrowserVersion, BrowserToolError> {
        self.get_json(self.endpoint.version_url()).await
    }

    pub async fn browser_websocket_url(&self) -> Result<String, BrowserToolError> {
        let version = self.browser_version().await?;
        let browser = version.browser.unwrap_or_default();
        if !browser.contains("Chrome") && !browser.contains("Chromium") {
            return Err(BrowserToolError::chrome_unavailable(format!(
                "CDP endpoint `{}` did not report a Chrome-compatible browser",
                self.endpoint.origin()
            )));
        }

        version.web_socket_debugger_url.ok_or_else(|| {
            BrowserToolError::chrome_unavailable(format!(
                "CDP endpoint `{}` did not expose a browser websocket URL",
                self.endpoint.origin()
            ))
        })
    }

    pub async fn page_targets(&self) -> Result<Vec<CdpTarget>, BrowserToolError> {
        let targets: Vec<CdpTarget> = self.get_json(self.endpoint.targets_url()).await?;
        Ok(targets
            .into_iter()
            .filter(|target| target.target_type == "page")
            .collect())
    }

    pub async fn create_page(
        &self,
        url: Option<&str>,
        focus: bool,
    ) -> Result<CdpTarget, BrowserToolError> {
        let browser_ws = self.browser_websocket_url().await?;
        let result = cdp_call(
            &browser_ws,
            "Target.createTarget",
            json!({ "url": url.unwrap_or("about:blank") }),
        )
        .await?;
        let target_id = result
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                BrowserToolError::chrome_unavailable("Chrome omitted targetId for new page")
            })?;

        if focus {
            self.activate_target(target_id).await?;
        }

        self.page_target(target_id).await
    }

    pub async fn page_target(&self, target_id: &str) -> Result<CdpTarget, BrowserToolError> {
        self.page_targets()
            .await?
            .into_iter()
            .find(|target| target.id == target_id)
            .ok_or_else(|| BrowserToolError::target_missing_for_target(target_id))
    }

    pub async fn activate_target(&self, target_id: &str) -> Result<(), BrowserToolError> {
        let browser_ws = self.browser_websocket_url().await?;
        cdp_call(
            &browser_ws,
            "Target.activateTarget",
            json!({ "targetId": target_id }),
        )
        .await?;
        Ok(())
    }

    pub async fn close_target(&self, target_id: &str) -> Result<(), BrowserToolError> {
        let browser_ws = self.browser_websocket_url().await?;
        cdp_call(
            &browser_ws,
            "Target.closeTarget",
            json!({ "targetId": target_id }),
        )
        .await?;
        Ok(())
    }

    pub async fn navigate(
        &self,
        target: &CdpTarget,
        url: &str,
        wait_until: Option<&str>,
        timeout_ms: u64,
    ) -> Result<(), BrowserToolError> {
        let wait_until = wait_until.unwrap_or("load");
        if wait_until != "load" {
            return Err(BrowserToolError::invalid_input(
                "navigate currently supports only wait_until `load`",
            ));
        }

        let ws_url = target.web_socket_debugger_url.as_ref().ok_or_else(|| {
            BrowserToolError::chrome_unavailable(format!(
                "Chrome target `{}` does not expose a websocket URL",
                target.id
            ))
        })?;
        let deadline = Duration::from_millis(timeout_ms);

        cdp_page_navigate(ws_url, url, deadline).await
    }

    pub async fn screenshot(
        &self,
        target: &CdpTarget,
        full_page: bool,
    ) -> Result<String, BrowserToolError> {
        let ws_url = target.web_socket_debugger_url.as_ref().ok_or_else(|| {
            BrowserToolError::chrome_unavailable(format!(
                "Chrome target `{}` does not expose a websocket URL",
                target.id
            ))
        })?;

        cdp_page_screenshot(ws_url, full_page).await
    }

    pub async fn evaluate(
        &self,
        target: &CdpTarget,
        expression: &str,
    ) -> Result<EvaluateResult, BrowserToolError> {
        let ws_url = target.web_socket_debugger_url.as_ref().ok_or_else(|| {
            BrowserToolError::chrome_unavailable(format!(
                "Chrome target `{}` does not expose a websocket URL",
                target.id
            ))
        })?;

        cdp_runtime_evaluate(ws_url, expression).await
    }

    pub async fn click(
        &self,
        target: &CdpTarget,
        selector: &str,
        timeout_ms: u64,
    ) -> Result<(), BrowserToolError> {
        let ws_url = target.web_socket_debugger_url.as_ref().ok_or_else(|| {
            BrowserToolError::chrome_unavailable(format!(
                "Chrome target `{}` does not expose a websocket URL",
                target.id
            ))
        })?;

        cdp_click_selector(ws_url, selector, Duration::from_millis(timeout_ms)).await
    }

    pub async fn type_text(&self, target: &CdpTarget, text: &str) -> Result<(), BrowserToolError> {
        let ws_url = target.web_socket_debugger_url.as_ref().ok_or_else(|| {
            BrowserToolError::chrome_unavailable(format!(
                "Chrome target `{}` does not expose a websocket URL",
                target.id
            ))
        })?;

        cdp_insert_text(ws_url, text).await
    }

    pub async fn press_key(
        &self,
        target: &CdpTarget,
        key: &str,
        modifiers: &[String],
    ) -> Result<(), BrowserToolError> {
        let ws_url = target.web_socket_debugger_url.as_ref().ok_or_else(|| {
            BrowserToolError::chrome_unavailable(format!(
                "Chrome target `{}` does not expose a websocket URL",
                target.id
            ))
        })?;

        cdp_press_key(ws_url, key, modifiers).await
    }

    pub async fn diagnostics_monitor(
        &self,
        target: &CdpTarget,
        sink: Arc<dyn Fn(CdpDiagnosticEvent) + Send + Sync>,
    ) -> Result<CdpDiagnosticsMonitor, BrowserToolError> {
        let ws_url = target.web_socket_debugger_url.as_ref().ok_or_else(|| {
            BrowserToolError::chrome_unavailable(format!(
                "Chrome target `{}` does not expose a websocket URL",
                target.id
            ))
        })?;

        start_diagnostics_monitor(ws_url, sink).await
    }

    async fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        url: Url,
    ) -> Result<T, BrowserToolError> {
        let response = self.http.get(url.clone()).send().await.map_err(|error| {
            BrowserToolError::chrome_unavailable(format!("failed to reach `{url}`: {error}"))
        })?;

        if !response.status().is_success() {
            return Err(BrowserToolError::chrome_unavailable(format!(
                "`{url}` returned HTTP {}",
                response.status()
            )));
        }

        response.json::<T>().await.map_err(|error| {
            BrowserToolError::chrome_unavailable(format!("`{url}` returned invalid JSON: {error}"))
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CdpDiagnosticEvent {
    Console {
        level: String,
        text: String,
        timestamp_ms: Option<u64>,
    },
    Network(NetworkEvent),
}

pub struct CdpDiagnosticsMonitor {
    stop: Option<oneshot::Sender<()>>,
}

impl Drop for CdpDiagnosticsMonitor {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrowserVersion {
    #[serde(rename = "Browser")]
    pub browser: Option<String>,

    #[serde(rename = "Protocol-Version")]
    pub protocol_version: Option<String>,

    #[serde(rename = "webSocketDebuggerUrl")]
    pub web_socket_debugger_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CdpTarget {
    pub id: String,

    #[serde(rename = "type")]
    pub target_type: String,

    pub title: String,
    pub url: String,

    #[serde(rename = "webSocketDebuggerUrl")]
    pub web_socket_debugger_url: Option<String>,
}

impl From<&CdpTarget> for TabSnapshot {
    fn from(target: &CdpTarget) -> Self {
        TabSnapshot::new(&target.id, &target.title, &target.url, false)
    }
}

async fn cdp_call(
    websocket_url: &str,
    method: &str,
    params: Value,
) -> Result<Value, BrowserToolError> {
    let (mut socket, _) = connect_async(websocket_url).await.map_err(|error| {
        BrowserToolError::chrome_unavailable(format!(
            "failed to connect to Chrome websocket `{websocket_url}`: {error}"
        ))
    })?;

    send_cdp_command(&mut socket, 1, method, params).await?;

    loop {
        let message = socket.next().await.ok_or_else(|| {
            BrowserToolError::chrome_unavailable("Chrome websocket closed before responding")
        })?;
        let message = message.map_err(|error| {
            BrowserToolError::chrome_unavailable(format!("Chrome websocket read failed: {error}"))
        })?;
        let Message::Text(text) = message else {
            continue;
        };
        let value: Value = serde_json::from_str(&text).map_err(|error| {
            BrowserToolError::chrome_unavailable(format!(
                "Chrome websocket returned invalid JSON: {error}"
            ))
        })?;

        if value.get("id").and_then(Value::as_u64) == Some(1) {
            return parse_cdp_response(value);
        }
    }
}

async fn cdp_page_navigate(
    websocket_url: &str,
    url: &str,
    deadline: Duration,
) -> Result<(), BrowserToolError> {
    let (mut socket, _) = connect_async(websocket_url).await.map_err(|error| {
        BrowserToolError::chrome_unavailable(format!(
            "failed to connect to Chrome websocket `{websocket_url}`: {error}"
        ))
    })?;

    send_cdp_command(&mut socket, 1, "Page.enable", json!({})).await?;
    wait_for_cdp_response(&mut socket, 1).await?;
    send_cdp_command(&mut socket, 2, "Page.navigate", json!({ "url": url })).await?;
    wait_for_cdp_response(&mut socket, 2).await?;

    let end = Instant::now() + deadline;
    loop {
        let remaining = end.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(BrowserToolError::operation_timeout(format!(
                "timed out waiting for load after navigating to `{url}`"
            )));
        }

        let message = timeout(remaining, socket.next()).await.map_err(|_| {
            BrowserToolError::operation_timeout(format!(
                "timed out waiting for load after navigating to `{url}`"
            ))
        })?;
        let message = message.ok_or_else(|| {
            BrowserToolError::chrome_unavailable("Chrome websocket closed before load event")
        })?;
        let message = message.map_err(|error| {
            BrowserToolError::chrome_unavailable(format!("Chrome websocket read failed: {error}"))
        })?;
        let Message::Text(text) = message else {
            continue;
        };
        let value: Value = serde_json::from_str(&text).map_err(|error| {
            BrowserToolError::chrome_unavailable(format!(
                "Chrome websocket returned invalid JSON: {error}"
            ))
        })?;

        if value.get("method").and_then(Value::as_str) == Some("Page.loadEventFired") {
            return Ok(());
        }
    }
}

async fn cdp_page_screenshot(
    websocket_url: &str,
    full_page: bool,
) -> Result<String, BrowserToolError> {
    let (mut socket, _) = connect_async(websocket_url).await.map_err(|error| {
        BrowserToolError::chrome_unavailable(format!(
            "failed to connect to Chrome websocket `{websocket_url}`: {error}"
        ))
    })?;

    send_cdp_command(&mut socket, 1, "Page.enable", json!({})).await?;
    wait_for_cdp_response(&mut socket, 1).await?;

    let params = if full_page {
        send_cdp_command(&mut socket, 2, "Page.getLayoutMetrics", json!({})).await?;
        let metrics = wait_for_cdp_response(&mut socket, 2).await?;
        let content_size = metrics.get("contentSize").ok_or_else(|| {
            BrowserToolError::chrome_unavailable("Chrome omitted contentSize for full-page capture")
        })?;
        json!({
            "format": "png",
            "fromSurface": true,
            "captureBeyondViewport": true,
            "clip": {
                "x": content_size.get("x").and_then(Value::as_f64).unwrap_or(0.0),
                "y": content_size.get("y").and_then(Value::as_f64).unwrap_or(0.0),
                "width": content_size.get("width").and_then(Value::as_f64).unwrap_or(1.0),
                "height": content_size.get("height").and_then(Value::as_f64).unwrap_or(1.0),
                "scale": 1
            }
        })
    } else {
        json!({
            "format": "png",
            "fromSurface": true
        })
    };

    send_cdp_command(&mut socket, 3, "Page.captureScreenshot", params).await?;
    let result = wait_for_cdp_response(&mut socket, 3).await?;
    result
        .get("data")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| BrowserToolError::chrome_unavailable("Chrome omitted screenshot data"))
}

async fn cdp_runtime_evaluate(
    websocket_url: &str,
    expression: &str,
) -> Result<EvaluateResult, BrowserToolError> {
    let (mut socket, _) = connect_async(websocket_url).await.map_err(|error| {
        BrowserToolError::chrome_unavailable(format!(
            "failed to connect to Chrome websocket `{websocket_url}`: {error}"
        ))
    })?;

    send_cdp_command(&mut socket, 1, "Runtime.enable", json!({})).await?;
    wait_for_cdp_response(&mut socket, 1).await?;
    send_cdp_command(
        &mut socket,
        2,
        "Runtime.evaluate",
        json!({
            "expression": expression,
            "returnByValue": true,
            "awaitPromise": true,
            "userGesture": true
        }),
    )
    .await?;
    let result = wait_for_cdp_response(&mut socket, 2).await?;

    parse_evaluate_result(result)
}

fn parse_evaluate_result(result: Value) -> Result<EvaluateResult, BrowserToolError> {
    if let Some(exception) = result.get("exceptionDetails") {
        return Err(BrowserToolError::invalid_input(format!(
            "evaluation failed: {}",
            exception_text(exception)
        )));
    }

    let remote = result.get("result").ok_or_else(|| {
        BrowserToolError::chrome_unavailable("Chrome omitted Runtime.evaluate result")
    })?;
    let value = remote.get("value").cloned();
    let preview = remote
        .get("description")
        .and_then(Value::as_str)
        .or_else(|| remote.get("type").and_then(Value::as_str))
        .map(str::to_string);

    Ok(EvaluateResult { value, preview })
}

fn exception_text(exception: &Value) -> String {
    exception
        .get("exception")
        .and_then(|value| value.get("description").or_else(|| value.get("value")))
        .and_then(Value::as_str)
        .or_else(|| exception.get("text").and_then(Value::as_str))
        .unwrap_or("JavaScript exception")
        .to_string()
}

async fn cdp_click_selector(
    websocket_url: &str,
    selector: &str,
    deadline: Duration,
) -> Result<(), BrowserToolError> {
    let (mut socket, _) = connect_async(websocket_url).await.map_err(|error| {
        BrowserToolError::chrome_unavailable(format!(
            "failed to connect to Chrome websocket `{websocket_url}`: {error}"
        ))
    })?;

    send_cdp_command(&mut socket, 1, "Runtime.enable", json!({})).await?;
    wait_for_cdp_response(&mut socket, 1).await?;

    let end = Instant::now() + deadline;
    let mut next_id = 2;
    let selector_json = serde_json::to_string(selector)
        .map_err(|error| BrowserToolError::invalid_input(format!("invalid selector: {error}")))?;
    let expression = format!(
        r#"
(() => {{
  const selector = {selector_json};
  const element = document.querySelector(selector);
  if (!element) return {{ found: false, visible: false }};
  element.scrollIntoView({{ block: "center", inline: "center" }});
  const rect = element.getBoundingClientRect();
  const style = window.getComputedStyle(element);
  const visible = rect.width > 0 && rect.height > 0 && style.visibility !== "hidden" && style.display !== "none" && Number(style.opacity || "1") !== 0;
  if (!visible) return {{ found: true, visible: false }};
  return {{
    found: true,
    visible: true,
    x: rect.left + rect.width / 2,
    y: rect.top + rect.height / 2
  }};
}})()
"#
    );

    loop {
        let remaining = end.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(BrowserToolError::operation_timeout(format!(
                "timed out waiting for visible selector `{selector}`"
            )));
        }

        send_cdp_command(
            &mut socket,
            next_id,
            "Runtime.evaluate",
            json!({
                "expression": expression,
                "returnByValue": true,
                "awaitPromise": true,
                "userGesture": true
            }),
        )
        .await?;
        let response = wait_for_cdp_response(&mut socket, next_id).await?;
        next_id += 1;

        if let Some(point) = click_point(response)? {
            send_mouse_click(&mut socket, next_id, point.x, point.y).await?;
            return Ok(());
        }

        tokio::time::sleep(remaining.min(Duration::from_millis(100))).await;
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ClickPoint {
    x: f64,
    y: f64,
}

fn click_point(result: Value) -> Result<Option<ClickPoint>, BrowserToolError> {
    if let Some(exception) = result.get("exceptionDetails") {
        return Err(BrowserToolError::invalid_input(format!(
            "selector evaluation failed: {}",
            exception_text(exception)
        )));
    }

    let Some(value) = result.get("result").and_then(|remote| remote.get("value")) else {
        return Ok(None);
    };

    if !value.get("found").and_then(Value::as_bool).unwrap_or(false)
        || !value
            .get("visible")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    {
        return Ok(None);
    }

    let x = value
        .get("x")
        .and_then(Value::as_f64)
        .ok_or_else(|| BrowserToolError::chrome_unavailable("selector result omitted x"))?;
    let y = value
        .get("y")
        .and_then(Value::as_f64)
        .ok_or_else(|| BrowserToolError::chrome_unavailable("selector result omitted y"))?;

    Ok(Some(ClickPoint { x, y }))
}

async fn send_mouse_click<S>(
    socket: &mut S,
    start_id: u64,
    x: f64,
    y: f64,
) -> Result<(), BrowserToolError>
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error>
        + futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    send_cdp_command(
        socket,
        start_id,
        "Input.dispatchMouseEvent",
        json!({
            "type": "mouseMoved",
            "x": x,
            "y": y,
            "button": "none"
        }),
    )
    .await?;
    wait_for_cdp_response(socket, start_id).await?;
    send_cdp_command(
        socket,
        start_id + 1,
        "Input.dispatchMouseEvent",
        json!({
            "type": "mousePressed",
            "x": x,
            "y": y,
            "button": "left",
            "clickCount": 1
        }),
    )
    .await?;
    wait_for_cdp_response(socket, start_id + 1).await?;
    send_cdp_command(
        socket,
        start_id + 2,
        "Input.dispatchMouseEvent",
        json!({
            "type": "mouseReleased",
            "x": x,
            "y": y,
            "button": "left",
            "clickCount": 1
        }),
    )
    .await?;
    wait_for_cdp_response(socket, start_id + 2).await?;
    Ok(())
}

async fn cdp_insert_text(websocket_url: &str, text: &str) -> Result<(), BrowserToolError> {
    let (mut socket, _) = connect_async(websocket_url).await.map_err(|error| {
        BrowserToolError::chrome_unavailable(format!(
            "failed to connect to Chrome websocket `{websocket_url}`: {error}"
        ))
    })?;

    send_cdp_command(&mut socket, 1, "Input.insertText", json!({ "text": text })).await?;
    wait_for_cdp_response(&mut socket, 1).await?;
    Ok(())
}

async fn cdp_press_key(
    websocket_url: &str,
    key: &str,
    modifiers: &[String],
) -> Result<(), BrowserToolError> {
    let key_event = key_event_for(key, modifiers)?;
    let (mut socket, _) = connect_async(websocket_url).await.map_err(|error| {
        BrowserToolError::chrome_unavailable(format!(
            "failed to connect to Chrome websocket `{websocket_url}`: {error}"
        ))
    })?;

    send_cdp_command(
        &mut socket,
        1,
        "Input.dispatchKeyEvent",
        key_event.params("keyDown"),
    )
    .await?;
    wait_for_cdp_response(&mut socket, 1).await?;
    send_cdp_command(
        &mut socket,
        2,
        "Input.dispatchKeyEvent",
        key_event.params("keyUp"),
    )
    .await?;
    wait_for_cdp_response(&mut socket, 2).await?;
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct KeyEvent {
    key: String,
    code: String,
    text: Option<String>,
    modifiers: u8,
    windows_virtual_key_code: Option<u16>,
}

impl KeyEvent {
    fn params(&self, event_type: &str) -> Value {
        let mut params = json!({
            "type": event_type,
            "key": self.key,
            "code": self.code,
            "modifiers": self.modifiers
        });

        if event_type == "keyDown" {
            if let Some(text) = &self.text {
                params["text"] = Value::String(text.clone());
                params["unmodifiedText"] = Value::String(text.clone());
            }
        }

        if let Some(code) = self.windows_virtual_key_code {
            params["windowsVirtualKeyCode"] = Value::from(code);
            params["nativeVirtualKeyCode"] = Value::from(code);
        }

        params
    }
}

fn key_event_for(key: &str, modifiers: &[String]) -> Result<KeyEvent, BrowserToolError> {
    let modifiers = modifier_bits(modifiers)?;

    let named = match key {
        "Enter" => Some(("Enter", "Enter", 13)),
        "Tab" => Some(("Tab", "Tab", 9)),
        "Escape" | "Esc" => Some(("Escape", "Escape", 27)),
        "Backspace" => Some(("Backspace", "Backspace", 8)),
        "Delete" => Some(("Delete", "Delete", 46)),
        "ArrowLeft" => Some(("ArrowLeft", "ArrowLeft", 37)),
        "ArrowRight" => Some(("ArrowRight", "ArrowRight", 39)),
        "ArrowUp" => Some(("ArrowUp", "ArrowUp", 38)),
        "ArrowDown" => Some(("ArrowDown", "ArrowDown", 40)),
        "Home" => Some(("Home", "Home", 36)),
        "End" => Some(("End", "End", 35)),
        "PageUp" => Some(("PageUp", "PageUp", 33)),
        "PageDown" => Some(("PageDown", "PageDown", 34)),
        "Space" => Some((" ", "Space", 32)),
        _ => None,
    };

    if let Some((mapped_key, code, virtual_key)) = named {
        return Ok(KeyEvent {
            key: mapped_key.to_string(),
            code: code.to_string(),
            text: if mapped_key == " " {
                Some(" ".to_string())
            } else {
                None
            },
            modifiers,
            windows_virtual_key_code: Some(virtual_key),
        });
    }

    let mut chars = key.chars();
    let Some(ch) = chars.next() else {
        return Err(BrowserToolError::invalid_input("key must not be empty"));
    };
    if chars.next().is_some() {
        return Err(BrowserToolError::invalid_input(format!(
            "unsupported key `{key}`; use a single printable character or common named key"
        )));
    }
    if ch.is_control() {
        return Err(BrowserToolError::invalid_input(format!(
            "unsupported control key `{key}`"
        )));
    }

    Ok(KeyEvent {
        key: key.to_string(),
        code: printable_code(ch),
        text: Some(key.to_string()),
        modifiers,
        windows_virtual_key_code: Some(ch.to_ascii_uppercase() as u16),
    })
}

fn modifier_bits(modifiers: &[String]) -> Result<u8, BrowserToolError> {
    let mut bits = 0;
    for modifier in modifiers {
        match modifier.as_str() {
            "Alt" => bits |= 1,
            "Control" | "Ctrl" => bits |= 2,
            "Meta" | "Command" => bits |= 4,
            "Shift" => bits |= 8,
            other => {
                return Err(BrowserToolError::invalid_input(format!(
                    "unsupported key modifier `{other}`"
                )));
            }
        }
    }
    Ok(bits)
}

fn printable_code(ch: char) -> String {
    if ch.is_ascii_alphabetic() {
        return format!("Key{}", ch.to_ascii_uppercase());
    }

    if ch.is_ascii_digit() {
        return format!("Digit{ch}");
    }

    match ch {
        ' ' => "Space".to_string(),
        '-' => "Minus".to_string(),
        '=' => "Equal".to_string(),
        '[' => "BracketLeft".to_string(),
        ']' => "BracketRight".to_string(),
        '\\' => "Backslash".to_string(),
        ';' => "Semicolon".to_string(),
        '\'' => "Quote".to_string(),
        ',' => "Comma".to_string(),
        '.' => "Period".to_string(),
        '/' => "Slash".to_string(),
        '`' => "Backquote".to_string(),
        _ => String::new(),
    }
}

async fn start_diagnostics_monitor(
    websocket_url: &str,
    sink: Arc<dyn Fn(CdpDiagnosticEvent) + Send + Sync>,
) -> Result<CdpDiagnosticsMonitor, BrowserToolError> {
    let (mut socket, _) = connect_async(websocket_url).await.map_err(|error| {
        BrowserToolError::chrome_unavailable(format!(
            "failed to connect to Chrome websocket `{websocket_url}`: {error}"
        ))
    })?;

    send_cdp_command(&mut socket, 1, "Runtime.enable", json!({})).await?;
    wait_for_cdp_response(&mut socket, 1).await?;
    send_cdp_command(&mut socket, 2, "Log.enable", json!({})).await?;
    wait_for_cdp_response(&mut socket, 2).await?;
    send_cdp_command(&mut socket, 3, "Network.enable", json!({})).await?;
    wait_for_cdp_response(&mut socket, 3).await?;

    let (stop_tx, mut stop_rx) = oneshot::channel();

    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut stop_rx => break,
                message = socket.next() => {
                    let Some(Ok(Message::Text(text))) = message else {
                        if message.is_none() {
                            break;
                        }
                        continue;
                    };

                    let Ok(value) = serde_json::from_str::<Value>(&text) else {
                        continue;
                    };

                    if let Some(event) = diagnostic_event(&value) {
                        sink(event);
                    }
                }
            }
        }
    });

    Ok(CdpDiagnosticsMonitor {
        stop: Some(stop_tx),
    })
}

fn diagnostic_event(value: &Value) -> Option<CdpDiagnosticEvent> {
    match value.get("method").and_then(Value::as_str)? {
        "Runtime.consoleAPICalled" => runtime_console_event(value),
        "Log.entryAdded" => log_entry_event(value),
        "Network.requestWillBeSent" => Some(CdpDiagnosticEvent::Network(NetworkEvent {
            sequence: 0,
            kind: "request".to_string(),
            url: value
                .pointer("/params/request/url")
                .and_then(Value::as_str)
                .map(str::to_string),
            method: value
                .pointer("/params/request/method")
                .and_then(Value::as_str)
                .map(str::to_string),
            status: None,
            error_text: None,
            timestamp_ms: monotonic_timestamp_ms(value.pointer("/params/timestamp")),
        })),
        "Network.responseReceived" => Some(CdpDiagnosticEvent::Network(NetworkEvent {
            sequence: 0,
            kind: "response".to_string(),
            url: value
                .pointer("/params/response/url")
                .and_then(Value::as_str)
                .map(str::to_string),
            method: None,
            status: value
                .pointer("/params/response/status")
                .and_then(Value::as_u64)
                .and_then(|status| u16::try_from(status).ok()),
            error_text: None,
            timestamp_ms: monotonic_timestamp_ms(value.pointer("/params/timestamp")),
        })),
        "Network.loadingFailed" => Some(CdpDiagnosticEvent::Network(NetworkEvent {
            sequence: 0,
            kind: "failed".to_string(),
            url: None,
            method: None,
            status: None,
            error_text: value
                .pointer("/params/errorText")
                .and_then(Value::as_str)
                .map(str::to_string),
            timestamp_ms: monotonic_timestamp_ms(value.pointer("/params/timestamp")),
        })),
        "Network.loadingFinished" => Some(CdpDiagnosticEvent::Network(NetworkEvent {
            sequence: 0,
            kind: "finished".to_string(),
            url: None,
            method: None,
            status: None,
            error_text: None,
            timestamp_ms: monotonic_timestamp_ms(value.pointer("/params/timestamp")),
        })),
        _ => None,
    }
}

fn runtime_console_event(value: &Value) -> Option<CdpDiagnosticEvent> {
    let params = value.get("params")?;
    let level = params
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("log")
        .to_string();
    let text = params
        .get("args")
        .and_then(Value::as_array)
        .map(|args| {
            args.iter()
                .map(remote_object_text)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();

    Some(CdpDiagnosticEvent::Console {
        level,
        text,
        timestamp_ms: params
            .get("timestamp")
            .and_then(Value::as_f64)
            .map(|ms| ms as u64),
    })
}

fn log_entry_event(value: &Value) -> Option<CdpDiagnosticEvent> {
    let entry = value.pointer("/params/entry")?;
    Some(CdpDiagnosticEvent::Console {
        level: entry
            .get("level")
            .and_then(Value::as_str)
            .unwrap_or("log")
            .to_string(),
        text: entry
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        timestamp_ms: entry
            .get("timestamp")
            .and_then(Value::as_f64)
            .map(|ms| ms as u64),
    })
}

fn remote_object_text(value: &Value) -> String {
    value
        .get("value")
        .map(|value| match value {
            Value::String(text) => text.clone(),
            other => other.to_string(),
        })
        .or_else(|| {
            value
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .or_else(|| {
            value
                .get("type")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_default()
}

fn monotonic_timestamp_ms(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_f64).map(|seconds| {
        if seconds > 10_000_000.0 {
            seconds as u64
        } else {
            (seconds * 1000.0) as u64
        }
    })
}

async fn send_cdp_command<S>(
    socket: &mut S,
    id: u64,
    method: &str,
    params: Value,
) -> Result<(), BrowserToolError>
where
    S: futures_util::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    let request = json!({
        "id": id,
        "method": method,
        "params": params,
    });
    socket
        .send(Message::Text(request.to_string().into()))
        .await
        .map_err(|error| {
            BrowserToolError::chrome_unavailable(format!("Chrome websocket write failed: {error}"))
        })
}

async fn wait_for_cdp_response<S>(socket: &mut S, id: u64) -> Result<Value, BrowserToolError>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let message = socket.next().await.ok_or_else(|| {
            BrowserToolError::chrome_unavailable("Chrome websocket closed before responding")
        })?;
        let message = message.map_err(|error| {
            BrowserToolError::chrome_unavailable(format!("Chrome websocket read failed: {error}"))
        })?;
        let Message::Text(text) = message else {
            continue;
        };
        let value: Value = serde_json::from_str(&text).map_err(|error| {
            BrowserToolError::chrome_unavailable(format!(
                "Chrome websocket returned invalid JSON: {error}"
            ))
        })?;

        if value.get("id").and_then(Value::as_u64) == Some(id) {
            return parse_cdp_response(value);
        }
    }
}

fn parse_cdp_response(value: Value) -> Result<Value, BrowserToolError> {
    if let Some(error) = value.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("Chrome returned a CDP error");
        return Err(BrowserToolError::chrome_unavailable(message));
    }

    Ok(value.get("result").cloned().unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_cdp_discovery_urls() {
        let endpoint = CdpEndpoint::parse("http://127.0.0.1:9222").unwrap();

        assert_eq!(
            endpoint.version_url().as_str(),
            "http://127.0.0.1:9222/json/version"
        );
        assert_eq!(
            endpoint.targets_url().as_str(),
            "http://127.0.0.1:9222/json/list"
        );
    }

    #[test]
    fn cdp_target_converts_to_tab_snapshot() {
        let target = CdpTarget {
            id: "target-1".to_string(),
            target_type: "page".to_string(),
            title: "Example".to_string(),
            url: "https://example.com".to_string(),
            web_socket_debugger_url: Some("ws://127.0.0.1/devtools/page/target-1".to_string()),
        };

        let snapshot = TabSnapshot::from(&target);

        assert_eq!(snapshot.target_id, "target-1");
        assert_eq!(snapshot.title, "Example");
        assert_eq!(snapshot.url, "https://example.com");
        assert!(!snapshot.focused);
    }

    #[test]
    fn maps_printable_and_named_keys() {
        let printable = key_event_for("a", &["Shift".to_string()]).unwrap();
        assert_eq!(printable.key, "a");
        assert_eq!(printable.code, "KeyA");
        assert_eq!(printable.text, Some("a".to_string()));
        assert_eq!(printable.modifiers, 8);

        let enter = key_event_for("Enter", &["Control".to_string()]).unwrap();
        assert_eq!(enter.key, "Enter");
        assert_eq!(enter.code, "Enter");
        assert_eq!(enter.text, None);
        assert_eq!(enter.modifiers, 2);
        assert_eq!(enter.windows_virtual_key_code, Some(13));
    }

    #[test]
    fn parses_selector_click_point() {
        let result = json!({
            "result": {
                "value": {
                    "found": true,
                    "visible": true,
                    "x": 12.5,
                    "y": 30.0
                }
            }
        });

        let point = click_point(result).unwrap().unwrap();

        assert_eq!(point, ClickPoint { x: 12.5, y: 30.0 });
    }

    #[test]
    fn parses_runtime_console_event() {
        let event = diagnostic_event(&json!({
            "method": "Runtime.consoleAPICalled",
            "params": {
                "type": "log",
                "timestamp": 1234.0,
                "args": [
                    { "type": "string", "value": "hello" },
                    { "type": "number", "value": 42 }
                ]
            }
        }))
        .unwrap();

        assert_eq!(
            event,
            CdpDiagnosticEvent::Console {
                level: "log".to_string(),
                text: "hello 42".to_string(),
                timestamp_ms: Some(1234)
            }
        );
    }

    #[test]
    fn parses_network_response_event() {
        let event = diagnostic_event(&json!({
            "method": "Network.responseReceived",
            "params": {
                "timestamp": 12.5,
                "response": {
                    "url": "https://example.com/data.json",
                    "status": 201
                }
            }
        }))
        .unwrap();

        assert_eq!(
            event,
            CdpDiagnosticEvent::Network(NetworkEvent {
                sequence: 0,
                kind: "response".to_string(),
                url: Some("https://example.com/data.json".to_string()),
                method: None,
                status: Some(201),
                error_text: None,
                timestamp_ms: Some(12_500)
            })
        );
    }
}
