use std::{sync::Arc, time::Duration};

use chromiumoxide::{
    Browser,
    cdp::{
        browser_protocol::{
            input::{
                DispatchKeyEventParams, DispatchKeyEventType, DispatchMouseEventParams,
                DispatchMouseEventType, InsertTextParams, MouseButton,
            },
            log::{EnableParams as LogEnableParams, EventEntryAdded},
            network::{
                EnableParams as NetworkEnableParams, EventLoadingFailed, EventLoadingFinished,
                EventRequestWillBeSent, EventResponseReceived,
            },
            page::{
                CaptureScreenshotFormat, CaptureScreenshotParams, EnableParams as PageEnableParams,
                GetLayoutMetricsParams, Viewport,
            },
            target::{
                ActivateTargetParams, CloseTargetParams, CreateTargetParams, GetTargetsParams,
                TargetId,
            },
        },
        js_protocol::runtime::{EnableParams as RuntimeEnableParams, EventConsoleApiCalled},
    },
    error::CdpError,
    handler::HandlerConfig,
    page::Page,
};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::{
    sync::{Mutex, oneshot},
    task::JoinHandle,
    time::{Instant, sleep, timeout},
};
use url::Url;

use crate::leases::{BrowserToolError, TabSnapshot};
use crate::protocol::{EvaluateResult, NetworkEvent};

const PAGE_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);
const PAGE_DISCOVERY_RETRY: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CdpEndpoint {
    origin: Url,
}

impl CdpEndpoint {
    pub fn parse(endpoint: &str) -> anyhow::Result<Self> {
        let origin = Url::parse(endpoint)?;
        if !matches!(origin.scheme(), "http" | "https" | "ws" | "wss") {
            anyhow::bail!("unsupported CDP endpoint scheme `{}`", origin.scheme());
        }
        Ok(Self { origin })
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
    runtime: Arc<CdpRuntime>,
}

impl CdpClient {
    pub fn new(endpoint: &str) -> anyhow::Result<Self> {
        let endpoint = CdpEndpoint::parse(endpoint)?;
        Ok(Self {
            runtime: Arc::new(CdpRuntime::new(endpoint)),
        })
    }

    pub fn endpoint(&self) -> &Url {
        self.runtime.endpoint.origin()
    }

    #[cfg(test)]
    async fn disconnect_for_test(&self) {
        self.runtime.disconnect().await;
    }

    pub async fn page_targets(&self) -> Result<Vec<CdpTarget>, BrowserToolError> {
        let connection = self.runtime.connection().await?;
        let response = self
            .runtime
            .browser_command(
                &connection,
                connection.browser.execute(GetTargetsParams::default()),
                "list Chrome targets",
            )
            .await?;

        Ok(response
            .result
            .target_infos
            .into_iter()
            .filter(|target| target.r#type == "page")
            .map(|target| CdpTarget {
                id: target.target_id.as_ref().to_string(),
                target_type: target.r#type,
                title: target.title,
                url: target.url,
            })
            .collect())
    }

    pub async fn create_page(
        &self,
        url: Option<&str>,
        focus: bool,
    ) -> Result<CdpTarget, BrowserToolError> {
        let connection = self.runtime.connection().await?;
        let params = CreateTargetParams::builder()
            .url(url.unwrap_or("about:blank"))
            .background(!focus)
            .build()
            .map_err(BrowserToolError::invalid_input)?;
        let response = self
            .runtime
            .browser_command(
                &connection,
                connection.browser.execute(params),
                "create Chrome target",
            )
            .await?;
        let target_id = response.result.target_id.as_ref().to_string();

        if focus {
            self.activate_target(&target_id).await?;
        }

        self.page_target(&target_id).await
    }

    pub async fn page_target(&self, target_id: &str) -> Result<CdpTarget, BrowserToolError> {
        self.page_targets()
            .await?
            .into_iter()
            .find(|target| target.id == target_id)
            .ok_or_else(|| BrowserToolError::target_missing_for_target(target_id))
    }

    pub async fn activate_target(&self, target_id: &str) -> Result<(), BrowserToolError> {
        let connection = self.runtime.connection().await?;
        self.runtime
            .browser_command(
                &connection,
                connection
                    .browser
                    .execute(ActivateTargetParams::new(TargetId::new(target_id))),
                "activate Chrome target",
            )
            .await?;
        Ok(())
    }

    pub async fn close_target(&self, target_id: &str) -> Result<(), BrowserToolError> {
        let connection = self.runtime.connection().await?;
        self.runtime
            .browser_command(
                &connection,
                connection
                    .browser
                    .execute(CloseTargetParams::new(TargetId::new(target_id))),
                "close Chrome target",
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
        if wait_until.unwrap_or("load") != "load" {
            return Err(BrowserToolError::invalid_input(
                "navigate currently supports only wait_until `load`",
            ));
        }

        let (page, connection) = self.page(&target.id).await?;
        match timeout(Duration::from_millis(timeout_ms), page.goto(url)).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(error)) => Err(self
                .runtime
                .page_error(&connection, "navigate", error)
                .await),
            Err(_) => Err(BrowserToolError::operation_timeout(format!(
                "timed out waiting for load after navigating to `{url}`"
            ))),
        }
    }

    pub async fn screenshot(
        &self,
        target: &CdpTarget,
        full_page: bool,
    ) -> Result<String, BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        self.runtime
            .page_command(
                &connection,
                page.execute(PageEnableParams::default()),
                "enable page for screenshot",
            )
            .await?;
        let mut builder = CaptureScreenshotParams::builder()
            .format(CaptureScreenshotFormat::Png)
            .capture_beyond_viewport(full_page);

        if full_page {
            let metrics = self
                .runtime
                .page_command(
                    &connection,
                    page.execute(GetLayoutMetricsParams::default()),
                    "read page layout metrics",
                )
                .await?;
            let size = metrics.result.css_content_size;
            let clip = Viewport::builder()
                .x(size.x)
                .y(size.y)
                .width(size.width.max(1.0))
                .height(size.height.max(1.0))
                .scale(1.0)
                .build()
                .map_err(BrowserToolError::invalid_input)?;
            builder = builder.clip(clip);
        }

        let response = self
            .runtime
            .page_command(
                &connection,
                page.execute(builder.build()),
                "capture page screenshot",
            )
            .await?;
        Ok(response.result.data.into())
    }

    pub async fn evaluate(
        &self,
        target: &CdpTarget,
        expression: &str,
    ) -> Result<EvaluateResult, BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        let result = match page.evaluate_expression(expression).await {
            Ok(result) => result,
            Err(error) => {
                return Err(self
                    .runtime
                    .page_error(&connection, "evaluate", error)
                    .await);
            }
        };
        let remote = result.object();
        Ok(EvaluateResult {
            value: remote.value.clone(),
            preview: remote
                .description
                .clone()
                .or_else(|| Some(remote.r#type.as_ref().to_string())),
        })
    }

    pub async fn has_focus(&self, target: &CdpTarget) -> Result<bool, BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        match page
            .evaluate_expression("document.hasFocus() && document.visibilityState === 'visible'")
            .await
        {
            Ok(result) => result.into_value::<bool>().map_err(|error| {
                BrowserToolError::chrome_unavailable(format!(
                    "Chrome returned an invalid focus result: {error}"
                ))
            }),
            Err(error) => Err(self
                .runtime
                .page_error(&connection, "check page focus", error)
                .await),
        }
    }

    pub async fn click(
        &self,
        target: &CdpTarget,
        selector: &str,
        timeout_ms: u64,
    ) -> Result<(), BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        let point = self
            .selector_point(
                &page,
                &connection,
                selector,
                Duration::from_millis(timeout_ms),
            )
            .await?;

        for event in [
            DispatchMouseEventParams::builder()
                .r#type(DispatchMouseEventType::MouseMoved)
                .x(point.x)
                .y(point.y)
                .button(MouseButton::None)
                .build(),
            DispatchMouseEventParams::builder()
                .r#type(DispatchMouseEventType::MousePressed)
                .x(point.x)
                .y(point.y)
                .button(MouseButton::Left)
                .click_count(1)
                .build(),
            DispatchMouseEventParams::builder()
                .r#type(DispatchMouseEventType::MouseReleased)
                .x(point.x)
                .y(point.y)
                .button(MouseButton::Left)
                .click_count(1)
                .build(),
        ] {
            let event = event.map_err(BrowserToolError::invalid_input)?;
            self.runtime
                .page_command(&connection, page.execute(event), "dispatch mouse input")
                .await?;
        }

        Ok(())
    }

    pub async fn type_text(&self, target: &CdpTarget, text: &str) -> Result<(), BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;
        self.runtime
            .page_command(
                &connection,
                page.execute(InsertTextParams::new(text)),
                "insert text",
            )
            .await?;
        Ok(())
    }

    pub async fn press_key(
        &self,
        target: &CdpTarget,
        key: &str,
        modifiers: &[String],
    ) -> Result<(), BrowserToolError> {
        let key_event = key_event_for(key, modifiers)?;
        let (page, connection) = self.page(&target.id).await?;

        for event_type in [DispatchKeyEventType::KeyDown, DispatchKeyEventType::KeyUp] {
            let params = key_event.params(event_type)?;
            self.runtime
                .page_command(&connection, page.execute(params), "dispatch keyboard input")
                .await?;
        }
        Ok(())
    }

    pub async fn diagnostics_monitor(
        &self,
        target: &CdpTarget,
        sink: Arc<dyn Fn(CdpDiagnosticEvent) + Send + Sync>,
    ) -> Result<CdpDiagnosticsMonitor, BrowserToolError> {
        let (page, connection) = self.page(&target.id).await?;

        self.runtime
            .page_command(
                &connection,
                page.execute(RuntimeEnableParams::default()),
                "enable runtime diagnostics",
            )
            .await?;
        self.runtime
            .page_command(
                &connection,
                page.execute(LogEnableParams::default()),
                "enable log diagnostics",
            )
            .await?;
        self.runtime
            .page_command(
                &connection,
                page.execute(NetworkEnableParams::default()),
                "enable network diagnostics",
            )
            .await?;

        let mut console = self
            .event_listener::<EventConsoleApiCalled>(&page, &connection)
            .await?;
        let mut log = self
            .event_listener::<EventEntryAdded>(&page, &connection)
            .await?;
        let mut request = self
            .event_listener::<EventRequestWillBeSent>(&page, &connection)
            .await?;
        let mut response = self
            .event_listener::<EventResponseReceived>(&page, &connection)
            .await?;
        let mut failed = self
            .event_listener::<EventLoadingFailed>(&page, &connection)
            .await?;
        let mut finished = self
            .event_listener::<EventLoadingFinished>(&page, &connection)
            .await?;
        let (stop_tx, mut stop_rx) = oneshot::channel();

        let task = tokio::spawn(async move {
            loop {
                let event = tokio::select! {
                    _ = &mut stop_rx => break,
                    event = console.next() => typed_event("Runtime.consoleAPICalled", event),
                    event = log.next() => typed_event("Log.entryAdded", event),
                    event = request.next() => typed_event("Network.requestWillBeSent", event),
                    event = response.next() => typed_event("Network.responseReceived", event),
                    event = failed.next() => typed_event("Network.loadingFailed", event),
                    event = finished.next() => typed_event("Network.loadingFinished", event),
                };

                match event {
                    Some(event) => sink(event),
                    None => break,
                }
            }
        });

        Ok(CdpDiagnosticsMonitor {
            stop: Some(stop_tx),
            task,
        })
    }

    async fn page(&self, target_id: &str) -> Result<(Page, RuntimeConnection), BrowserToolError> {
        let connection = self.runtime.connection().await?;
        let deadline = Instant::now() + PAGE_DISCOVERY_TIMEOUT;
        loop {
            match connection.browser.get_page(TargetId::new(target_id)).await {
                Ok(page) => return Ok((page, connection)),
                Err(CdpError::NotFound) if Instant::now() < deadline => {
                    sleep(PAGE_DISCOVERY_RETRY).await;
                }
                Err(CdpError::NotFound) => {
                    return Err(BrowserToolError::target_missing_for_target(target_id));
                }
                Err(error) => {
                    return Err(self
                        .runtime
                        .page_error(&connection, "open Chrome target session", error)
                        .await);
                }
            }
        }
    }

    async fn event_listener<T>(
        &self,
        page: &Page,
        connection: &RuntimeConnection,
    ) -> Result<chromiumoxide::listeners::EventStream<T>, BrowserToolError>
    where
        T: chromiumoxide::cdp::IntoEventKind + Unpin,
    {
        match page.event_listener::<T>().await {
            Ok(stream) => Ok(stream),
            Err(error) => {
                let operation = format!("subscribe to {}", std::any::type_name::<T>());
                if invalidates_connection(&error) {
                    self.runtime.invalidate(connection.generation).await;
                }
                Err(map_cdp_error(&operation, &error))
            }
        }
    }

    async fn selector_point(
        &self,
        page: &Page,
        connection: &RuntimeConnection,
        selector: &str,
        deadline: Duration,
    ) -> Result<ClickPoint, BrowserToolError> {
        let selector_json = serde_json::to_string(selector).map_err(|error| {
            BrowserToolError::invalid_input(format!("invalid selector: {error}"))
        })?;
        let expression = format!(
            r#"(() => {{
  const selector = {selector_json};
  const element = document.querySelector(selector);
  if (!element) return {{ found: false, visible: false }};
  element.scrollIntoView({{ block: "center", inline: "center" }});
  const rect = element.getBoundingClientRect();
  const style = window.getComputedStyle(element);
  const visible = rect.width > 0 && rect.height > 0 && style.visibility !== "hidden" && style.display !== "none" && Number(style.opacity || "1") !== 0;
  if (!visible) return {{ found: true, visible: false }};
  return {{ found: true, visible: true, x: rect.left + rect.width / 2, y: rect.top + rect.height / 2 }};
}})()"#
        );
        let end = Instant::now() + deadline;

        loop {
            let remaining = end.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(BrowserToolError::operation_timeout(format!(
                    "timed out waiting for visible selector `{selector}`"
                )));
            }

            let result = match timeout(remaining, page.evaluate_expression(&expression)).await {
                Ok(Ok(result)) => result,
                Ok(Err(error)) => {
                    return Err(self
                        .runtime
                        .page_error(connection, "evaluate click selector", error)
                        .await);
                }
                Err(_) => {
                    return Err(BrowserToolError::operation_timeout(format!(
                        "timed out waiting for visible selector `{selector}`"
                    )));
                }
            };

            if let Some(point) = click_point(result.value().cloned())? {
                return Ok(point);
            }
            sleep(remaining.min(Duration::from_millis(100))).await;
        }
    }
}

#[derive(Debug)]
struct CdpRuntime {
    endpoint: CdpEndpoint,
    state: Mutex<RuntimeState>,
}

#[derive(Debug, Default)]
struct RuntimeState {
    connection: Option<ConnectedBrowser>,
    generation: u64,
}

#[derive(Debug)]
struct ConnectedBrowser {
    generation: u64,
    browser: Arc<Browser>,
    handler: JoinHandle<Result<(), String>>,
}

#[derive(Debug, Clone)]
struct RuntimeConnection {
    generation: u64,
    browser: Arc<Browser>,
}

impl CdpRuntime {
    fn new(endpoint: CdpEndpoint) -> Self {
        Self {
            endpoint,
            state: Mutex::new(RuntimeState::default()),
        }
    }

    async fn connection(&self) -> Result<RuntimeConnection, BrowserToolError> {
        let mut state = self.state.lock().await;
        if let Some(connection) = &state.connection
            && !connection.handler.is_finished()
        {
            return Ok(RuntimeConnection {
                generation: connection.generation,
                browser: connection.browser.clone(),
            });
        }

        if let Some(connection) = state.connection.take() {
            connection.handler.abort();
        }

        let endpoint = self.endpoint.origin().as_str().to_string();
        let (mut browser, mut handler) = Browser::connect_with_config(
            endpoint.clone(),
            HandlerConfig {
                viewport: None,
                ..HandlerConfig::default()
            },
        )
        .await
        .map_err(|error| {
            BrowserToolError::chrome_unavailable(format!(
                "failed to connect Chromiumoxide to `{endpoint}`: {error}"
            ))
        })?;
        let handler_task = tokio::spawn(async move {
            while let Some(result) = handler.next().await {
                result.map_err(|error| error.to_string())?;
            }
            Err("Chromiumoxide handler ended".to_string())
        });
        if let Err(error) = browser.fetch_targets().await {
            handler_task.abort();
            return Err(map_cdp_error("register existing Chrome targets", &error));
        }
        let browser = Arc::new(browser);
        state.generation += 1;
        let generation = state.generation;
        state.connection = Some(ConnectedBrowser {
            generation,
            browser: browser.clone(),
            handler: handler_task,
        });

        Ok(RuntimeConnection {
            generation,
            browser,
        })
    }

    async fn browser_command<T, F>(
        &self,
        connection: &RuntimeConnection,
        future: F,
        operation: &str,
    ) -> Result<T, BrowserToolError>
    where
        F: Future<Output = Result<T, CdpError>>,
    {
        match future.await {
            Ok(value) => Ok(value),
            Err(error) => {
                if invalidates_connection(&error) {
                    self.invalidate(connection.generation).await;
                }
                Err(map_cdp_error(operation, &error))
            }
        }
    }

    async fn page_command<T, F>(
        &self,
        connection: &RuntimeConnection,
        future: F,
        operation: &str,
    ) -> Result<T, BrowserToolError>
    where
        F: Future<Output = Result<T, CdpError>>,
    {
        self.browser_command(connection, future, operation).await
    }

    async fn page_error(
        &self,
        connection: &RuntimeConnection,
        operation: &str,
        error: CdpError,
    ) -> BrowserToolError {
        if invalidates_connection(&error) {
            self.invalidate(connection.generation).await;
        }
        map_cdp_error(operation, &error)
    }

    async fn invalidate(&self, generation: u64) {
        let mut state = self.state.lock().await;
        if state
            .connection
            .as_ref()
            .is_some_and(|connection| connection.generation == generation)
            && let Some(connection) = state.connection.take()
        {
            connection.handler.abort();
        }
    }

    #[cfg(test)]
    async fn disconnect(&self) {
        let mut state = self.state.lock().await;
        if let Some(connection) = state.connection.take() {
            connection.handler.abort();
        }
    }
}

impl Drop for CdpRuntime {
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.try_lock()
            && let Some(connection) = state.connection.take()
        {
            connection.handler.abort();
        }
    }
}

fn invalidates_connection(error: &CdpError) -> bool {
    matches!(
        error,
        CdpError::Ws(_)
            | CdpError::Io(_)
            | CdpError::NoResponse
            | CdpError::ChannelSendError(_)
            | CdpError::UnexpectedWsMessage(_)
    )
}

fn map_cdp_error(operation: &str, error: &CdpError) -> BrowserToolError {
    match error {
        CdpError::JavascriptException(details) => {
            BrowserToolError::invalid_input(format!("{operation} failed: {}", details.text))
        }
        CdpError::Timeout => BrowserToolError::operation_timeout(format!("{operation} timed out")),
        CdpError::NotFound => BrowserToolError::chrome_unavailable(format!(
            "{operation} failed because Chrome no longer exposes the target"
        )),
        _ => BrowserToolError::chrome_unavailable(format!("{operation} failed: {error}")),
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
    task: JoinHandle<()>,
}

impl CdpDiagnosticsMonitor {
    pub fn is_finished(&self) -> bool {
        self.task.is_finished()
    }
}

impl Drop for CdpDiagnosticsMonitor {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        self.task.abort();
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CdpTarget {
    pub id: String,

    #[serde(rename = "type")]
    pub target_type: String,

    pub title: String,
    pub url: String,
}

impl From<&CdpTarget> for TabSnapshot {
    fn from(target: &CdpTarget) -> Self {
        TabSnapshot::new(&target.id, &target.title, &target.url, false)
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct ClickPoint {
    x: f64,
    y: f64,
}

fn click_point(value: Option<Value>) -> Result<Option<ClickPoint>, BrowserToolError> {
    let Some(value) = value else {
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct KeyEvent {
    key: String,
    code: String,
    text: Option<String>,
    modifiers: u8,
    windows_virtual_key_code: Option<u16>,
}

impl KeyEvent {
    fn params(
        &self,
        event_type: DispatchKeyEventType,
    ) -> Result<DispatchKeyEventParams, BrowserToolError> {
        let is_key_down = event_type == DispatchKeyEventType::KeyDown;
        let mut builder = DispatchKeyEventParams::builder()
            .r#type(event_type)
            .key(&self.key)
            .code(&self.code)
            .modifiers(i64::from(self.modifiers));
        if is_key_down && let Some(text) = &self.text {
            builder = builder.text(text).unmodified_text(text);
        }
        if let Some(code) = self.windows_virtual_key_code {
            builder = builder
                .windows_virtual_key_code(i64::from(code))
                .native_virtual_key_code(i64::from(code));
        }
        builder.build().map_err(BrowserToolError::invalid_input)
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
            text: (mapped_key == " ").then(|| " ".to_string()),
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

fn typed_event<T: Serialize>(method: &str, event: Option<Arc<T>>) -> Option<CdpDiagnosticEvent> {
    let event = event?;
    let params = serde_json::to_value(&*event).ok()?;
    diagnostic_event(&json!({ "method": method, "params": params }))
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
                .and_then(Value::as_f64)
                .and_then(|status| u16::try_from(status as u64).ok()),
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
        timestamp_ms: wall_timestamp_ms(params.get("timestamp")),
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
        timestamp_ms: wall_timestamp_ms(entry.get("timestamp")),
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

fn wall_timestamp_ms(value: Option<&Value>) -> Option<u64> {
    value.and_then(Value::as_f64).map(|timestamp| {
        if timestamp > 1_000_000_000_000.0 {
            timestamp as u64
        } else {
            (timestamp * 1000.0) as u64
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use visible_browser_lab_test_support::{BrowserMode, RealBrowser};

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
        let point = click_point(Some(json!({
            "found": true,
            "visible": true,
            "x": 12.5,
            "y": 30.0
        })))
        .unwrap()
        .unwrap();
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
                timestamp_ms: Some(1_234_000)
            }
        );
    }

    #[test]
    fn parses_log_entry_timestamp_as_milliseconds() {
        let event = diagnostic_event(&json!({
            "method": "Log.entryAdded",
            "params": { "entry": { "level": "warning", "text": "careful", "timestamp": 1234.5 } }
        }))
        .unwrap();
        assert_eq!(
            event,
            CdpDiagnosticEvent::Console {
                level: "warning".to_string(),
                text: "careful".to_string(),
                timestamp_ms: Some(1_234_500)
            }
        );
    }

    #[test]
    fn parses_network_response_event() {
        let event = diagnostic_event(&json!({
            "method": "Network.responseReceived",
            "params": {
                "timestamp": 12.5,
                "response": { "url": "https://example.com/data.json", "status": 201 }
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

    #[tokio::test(flavor = "multi_thread")]
    async fn reconnects_after_handler_shutdown_and_reports_browser_disappearance() {
        let mut chrome = tokio::task::spawn_blocking(|| RealBrowser::launch(BrowserMode::Headless))
            .await
            .unwrap()
            .unwrap();
        let client = CdpClient::new(chrome.cdp_endpoint()).unwrap();

        client.page_targets().await.unwrap();
        client.disconnect_for_test().await;
        client.page_targets().await.unwrap();

        chrome.shutdown();
        let error = timeout(Duration::from_secs(5), client.page_targets())
            .await
            .expect("Chrome disappearance should be observed without a request timeout")
            .unwrap_err();
        assert_eq!(
            error.code,
            crate::leases::BrowserToolErrorCode::ChromeUnavailable
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn registers_targets_that_predate_the_runtime_connection() {
        let mut chrome = tokio::task::spawn_blocking(|| RealBrowser::launch(BrowserMode::Headless))
            .await
            .unwrap()
            .unwrap();
        let client = CdpClient::new(chrome.cdp_endpoint()).unwrap();

        let target = client
            .page_targets()
            .await
            .unwrap()
            .into_iter()
            .next()
            .expect("Chrome for Testing should expose its initial page");
        let result = client
            .evaluate(&target, "document.location.href")
            .await
            .unwrap();

        assert!(result.value.is_some());
        chrome.shutdown();
    }
}
