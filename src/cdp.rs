use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::{Instant, timeout};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use url::Url;

use crate::leases::{BrowserToolError, TabSnapshot};

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
}
