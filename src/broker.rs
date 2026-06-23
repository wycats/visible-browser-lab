use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{self, File, OpenOptions},
    io::ErrorKind,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Serialize, de::DeserializeOwned};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    time::{Instant, sleep},
};

use crate::{
    cdp::{CdpClient, CdpDiagnosticEvent, CdpDiagnosticsMonitor, CdpTarget},
    config::RuntimeConfig,
    ipc::{self, BrokerEndpoint, BrokerListener, BrokerStream},
    leases::{
        AgentSessionId, BrowserToolError, LeaseRegistry, LeaseState, OwnedTabSummary, TabId,
        TabLease, TabSnapshot,
    },
    protocol::{
        BROKER_PROTOCOL_VERSION, BrokerClient, BrokerRequest, BrokerResponse, BrokerStatus,
        ClaimTabParams, ClickParams, ClickResult, CloseTabResult, ConsoleMessage,
        ConsoleMessagesResult, DiagnosticsParams, EvaluateParams, EvaluateResult, ListTabsParams,
        ListTabsResult, ListTabsScope, NavigateParams, NetworkEvent, NetworkEventsResult,
        NewTabParams, PressKeyParams, PressKeyResult, ReleaseTabResult, ScreenshotParams,
        ScreenshotResult, StartSessionParams, StartSessionResult, TabActionParams, TabResult,
        TypeTextParams, TypeTextResult,
    },
};

const BROKER_START_TIMEOUT: Duration = Duration::from_secs(5);
const BROKER_CONNECT_RETRY: Duration = Duration::from_millis(50);
const DEFAULT_NAVIGATION_TIMEOUT_MS: u64 = 15_000;
const DEFAULT_CLICK_TIMEOUT_MS: u64 = 5_000;
const DIAGNOSTICS_BUFFER_LIMIT: usize = 200;

#[derive(Clone)]
struct BrokerState {
    registry: Arc<Mutex<LeaseRegistry>>,
    diagnostics: Arc<Mutex<DiagnosticsRegistry>>,
    focused_target_id: Arc<Mutex<Option<String>>>,
    browser: BrowserBackend,
}

impl BrokerState {
    fn new(config: &RuntimeConfig) -> Result<Self> {
        Ok(Self {
            registry: Arc::new(Mutex::new(LeaseRegistry::new())),
            diagnostics: Arc::new(Mutex::new(DiagnosticsRegistry::new())),
            focused_target_id: Arc::new(Mutex::new(None)),
            browser: BrowserBackend::new(&config.cdp_endpoint)?,
        })
    }

    #[cfg(test)]
    fn with_browser(browser: BrowserBackend) -> Self {
        Self {
            registry: Arc::new(Mutex::new(LeaseRegistry::new())),
            diagnostics: Arc::new(Mutex::new(DiagnosticsRegistry::new())),
            focused_target_id: Arc::new(Mutex::new(None)),
            browser,
        }
    }

    fn registry(&self) -> &Mutex<LeaseRegistry> {
        &self.registry
    }

    fn diagnostics(&self) -> &Mutex<DiagnosticsRegistry> {
        &self.diagnostics
    }

    fn mark_focused_target(&self, target_id: &str) {
        *self.focused_target_id.lock().unwrap() = Some(target_id.to_string());
    }

    fn clear_focused_target(&self, target_id: &str) {
        let mut focused_target_id = self.focused_target_id.lock().unwrap();
        if focused_target_id.as_deref() == Some(target_id) {
            *focused_target_id = None;
        }
    }

    fn is_focused_target(&self, target_id: &str) -> bool {
        self.focused_target_id.lock().unwrap().as_deref() == Some(target_id)
    }

    fn focused_target_id_for_targets(&self, targets: &[CdpTarget]) -> Option<String> {
        let mut focused_target_id = self.focused_target_id.lock().unwrap();
        let focused = focused_target_id.clone()?;
        if targets.iter().any(|target| target.id == focused) {
            Some(focused)
        } else {
            *focused_target_id = None;
            None
        }
    }
}

struct DiagnosticsRegistry {
    targets: HashMap<String, TargetDiagnostics>,
    monitored_targets: HashSet<String>,
    monitors: HashMap<String, CdpDiagnosticsMonitor>,
    next_sequence: u64,
}

impl DiagnosticsRegistry {
    fn new() -> Self {
        Self {
            targets: HashMap::new(),
            monitored_targets: HashSet::new(),
            monitors: HashMap::new(),
            next_sequence: 1,
        }
    }

    fn ensure_target(&mut self, target_id: &str) {
        self.targets.entry(target_id.to_string()).or_default();
    }

    fn is_monitored(&self, target_id: &str) -> bool {
        self.monitored_targets.contains(target_id)
    }

    fn mark_monitored(&mut self, target_id: &str, monitor: Option<CdpDiagnosticsMonitor>) {
        self.monitored_targets.insert(target_id.to_string());
        if let Some(monitor) = monitor {
            self.monitors.insert(target_id.to_string(), monitor);
        }
    }

    fn reset_target(&mut self, target_id: &str) {
        self.targets.remove(target_id);
        self.monitored_targets.remove(target_id);
        self.monitors.remove(target_id);
    }

    fn push_event(&mut self, target_id: &str, event: CdpDiagnosticEvent) {
        let sequence = self.next_sequence;
        self.next_sequence += 1;
        match event {
            CdpDiagnosticEvent::Console {
                level,
                text,
                timestamp_ms,
            } => {
                self.push_console(
                    target_id,
                    ConsoleMessage {
                        sequence,
                        level,
                        text,
                        timestamp_ms,
                    },
                );
            }
            CdpDiagnosticEvent::Network(mut event) => {
                event.sequence = sequence;
                self.push_network(target_id, event);
            }
        }
    }

    fn push_console(&mut self, target_id: &str, message: ConsoleMessage) {
        let target = self.targets.entry(target_id.to_string()).or_default();
        target.console.push_back(message);
        truncate_front(&mut target.console);
    }

    fn push_network(&mut self, target_id: &str, event: NetworkEvent) {
        let target = self.targets.entry(target_id.to_string()).or_default();
        target.network.push_back(event);
        truncate_front(&mut target.network);
    }

    fn console_messages(&self, target_id: &str, since: Option<u64>) -> Vec<ConsoleMessage> {
        self.targets
            .get(target_id)
            .map(|target| {
                target
                    .console
                    .iter()
                    .filter(|message| since.is_none_or(|since| message.sequence > since))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    fn network_events(&self, target_id: &str, since: Option<u64>) -> Vec<NetworkEvent> {
        self.targets
            .get(target_id)
            .map(|target| {
                target
                    .network
                    .iter()
                    .filter(|event| since.is_none_or(|since| event.sequence > since))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[derive(Default)]
struct TargetDiagnostics {
    console: VecDeque<ConsoleMessage>,
    network: VecDeque<NetworkEvent>,
}

fn truncate_front<T>(buffer: &mut VecDeque<T>) {
    while buffer.len() > DIAGNOSTICS_BUFFER_LIMIT {
        buffer.pop_front();
    }
}

#[cfg(test)]
#[derive(Debug, Default)]
struct FakeBrowser {
    targets: Vec<CdpTarget>,
    next_target: u64,
    focused_target_id: Option<String>,
    closed_targets: Vec<String>,
    clicked_selectors: Vec<String>,
    typed_text: Vec<String>,
    pressed_keys: Vec<String>,
}

#[cfg(test)]
impl FakeBrowser {
    fn with_targets(targets: Vec<CdpTarget>) -> Self {
        Self {
            targets,
            next_target: 1,
            focused_target_id: None,
            closed_targets: Vec::new(),
            clicked_selectors: Vec::new(),
            typed_text: Vec::new(),
            pressed_keys: Vec::new(),
        }
    }

    fn page_targets(&self) -> Vec<CdpTarget> {
        self.targets.clone()
    }

    fn create_page(&mut self, url: Option<&str>, focus: bool) -> CdpTarget {
        let id = format!("target-new-{}", self.next_target);
        self.next_target += 1;
        let target = CdpTarget {
            id: id.clone(),
            target_type: "page".to_string(),
            title: url.unwrap_or("about:blank").to_string(),
            url: url.unwrap_or("about:blank").to_string(),
            web_socket_debugger_url: Some(format!("ws://fake/{id}")),
        };
        self.targets.push(target.clone());
        if focus {
            self.focused_target_id = Some(id);
        }
        target
    }

    fn activate_target(&mut self, target_id: &str) -> Result<(), BrowserToolError> {
        if self.targets.iter().any(|target| target.id == target_id) {
            self.focused_target_id = Some(target_id.to_string());
            return Ok(());
        }

        Err(BrowserToolError::target_missing_for_target(target_id))
    }

    fn close_target(&mut self, target_id: &str) -> Result<(), BrowserToolError> {
        let original_len = self.targets.len();
        self.targets.retain(|target| target.id != target_id);
        if self.targets.len() == original_len {
            return Err(BrowserToolError::target_missing_for_target(target_id));
        }

        self.closed_targets.push(target_id.to_string());
        Ok(())
    }

    fn navigate(&mut self, target: &CdpTarget, url: &str) -> Result<CdpTarget, BrowserToolError> {
        let target = self
            .targets
            .iter_mut()
            .find(|candidate| candidate.id == target.id)
            .ok_or_else(|| BrowserToolError::target_missing_for_target(&target.id))?;
        target.url = url.to_string();
        target.title = url.to_string();
        Ok(target.clone())
    }

    fn screenshot(&self, target: &CdpTarget, _full_page: bool) -> Result<String, BrowserToolError> {
        if self
            .targets
            .iter()
            .any(|candidate| candidate.id == target.id)
        {
            return Ok("ZmFrZS1wbmc=".to_string());
        }

        Err(BrowserToolError::target_missing_for_target(&target.id))
    }

    fn evaluate(
        &self,
        target: &CdpTarget,
        expression: &str,
    ) -> Result<EvaluateResult, BrowserToolError> {
        if !self
            .targets
            .iter()
            .any(|candidate| candidate.id == target.id)
        {
            return Err(BrowserToolError::target_missing_for_target(&target.id));
        }

        let value = match expression {
            "1 + 1" => Some(serde_json::json!(2)),
            "document.title" => Some(serde_json::json!(target.title)),
            _ => None,
        };
        Ok(EvaluateResult {
            value,
            preview: Some(expression.to_string()),
        })
    }

    fn click(&mut self, target: &CdpTarget, selector: &str) -> Result<(), BrowserToolError> {
        if !self
            .targets
            .iter()
            .any(|candidate| candidate.id == target.id)
        {
            return Err(BrowserToolError::target_missing_for_target(&target.id));
        }

        if selector == "#missing" {
            return Err(BrowserToolError::operation_timeout(
                "timed out waiting for visible selector `#missing`",
            ));
        }

        self.clicked_selectors.push(selector.to_string());
        Ok(())
    }

    fn type_text(&mut self, target: &CdpTarget, text: &str) -> Result<(), BrowserToolError> {
        if !self
            .targets
            .iter()
            .any(|candidate| candidate.id == target.id)
        {
            return Err(BrowserToolError::target_missing_for_target(&target.id));
        }

        self.typed_text.push(text.to_string());
        Ok(())
    }

    fn press_key(
        &mut self,
        target: &CdpTarget,
        key: &str,
        _modifiers: &[String],
    ) -> Result<(), BrowserToolError> {
        if !self
            .targets
            .iter()
            .any(|candidate| candidate.id == target.id)
        {
            return Err(BrowserToolError::target_missing_for_target(&target.id));
        }

        self.pressed_keys.push(key.to_string());
        Ok(())
    }

    fn remove_target(&mut self, target_id: &str) {
        self.targets.retain(|target| target.id != target_id);
    }

    fn was_closed(&self, target_id: &str) -> bool {
        self.closed_targets.iter().any(|closed| closed == target_id)
    }

    fn was_clicked(&self, selector: &str) -> bool {
        self.clicked_selectors
            .iter()
            .any(|clicked| clicked == selector)
    }

    fn typed_text(&self) -> &[String] {
        &self.typed_text
    }

    fn pressed_keys(&self) -> &[String] {
        &self.pressed_keys
    }
}

#[derive(Clone)]
enum BrowserBackend {
    Cdp(CdpClient),
    #[cfg(test)]
    Fake(Arc<Mutex<FakeBrowser>>),
}

impl BrowserBackend {
    fn new(cdp_endpoint: &str) -> Result<Self> {
        Ok(Self::Cdp(CdpClient::new(cdp_endpoint)?))
    }

    async fn page_targets(&self) -> Result<Vec<CdpTarget>, BrowserToolError> {
        match self {
            Self::Cdp(client) => client.page_targets().await,
            #[cfg(test)]
            Self::Fake(browser) => Ok(browser.lock().unwrap().page_targets()),
        }
    }

    async fn create_page(
        &self,
        url: Option<&str>,
        focus: bool,
    ) -> Result<CdpTarget, BrowserToolError> {
        match self {
            Self::Cdp(client) => client.create_page(url, focus).await,
            #[cfg(test)]
            Self::Fake(browser) => Ok(browser.lock().unwrap().create_page(url, focus)),
        }
    }

    async fn activate_target(&self, target_id: &str) -> Result<(), BrowserToolError> {
        match self {
            Self::Cdp(client) => client.activate_target(target_id).await,
            #[cfg(test)]
            Self::Fake(browser) => browser.lock().unwrap().activate_target(target_id),
        }
    }

    async fn close_target(&self, target_id: &str) -> Result<(), BrowserToolError> {
        match self {
            Self::Cdp(client) => client.close_target(target_id).await,
            #[cfg(test)]
            Self::Fake(browser) => browser.lock().unwrap().close_target(target_id),
        }
    }

    async fn navigate(
        &self,
        target: &CdpTarget,
        url: &str,
        wait_until: Option<&str>,
        timeout_ms: u64,
    ) -> Result<CdpTarget, BrowserToolError> {
        match self {
            Self::Cdp(client) => {
                client.navigate(target, url, wait_until, timeout_ms).await?;
                client.page_target(&target.id).await
            }
            #[cfg(test)]
            Self::Fake(browser) => browser.lock().unwrap().navigate(target, url),
        }
    }

    async fn screenshot(
        &self,
        target: &CdpTarget,
        full_page: bool,
    ) -> Result<String, BrowserToolError> {
        match self {
            Self::Cdp(client) => client.screenshot(target, full_page).await,
            #[cfg(test)]
            Self::Fake(browser) => browser.lock().unwrap().screenshot(target, full_page),
        }
    }

    async fn evaluate(
        &self,
        target: &CdpTarget,
        expression: &str,
    ) -> Result<EvaluateResult, BrowserToolError> {
        match self {
            Self::Cdp(client) => client.evaluate(target, expression).await,
            #[cfg(test)]
            Self::Fake(browser) => browser.lock().unwrap().evaluate(target, expression),
        }
    }

    async fn click(
        &self,
        target: &CdpTarget,
        selector: &str,
        timeout_ms: u64,
    ) -> Result<(), BrowserToolError> {
        match self {
            Self::Cdp(client) => client.click(target, selector, timeout_ms).await,
            #[cfg(test)]
            Self::Fake(browser) => browser.lock().unwrap().click(target, selector),
        }
    }

    async fn type_text(&self, target: &CdpTarget, text: &str) -> Result<(), BrowserToolError> {
        match self {
            Self::Cdp(client) => client.type_text(target, text).await,
            #[cfg(test)]
            Self::Fake(browser) => browser.lock().unwrap().type_text(target, text),
        }
    }

    async fn press_key(
        &self,
        target: &CdpTarget,
        key: &str,
        modifiers: &[String],
    ) -> Result<(), BrowserToolError> {
        match self {
            Self::Cdp(client) => client.press_key(target, key, modifiers).await,
            #[cfg(test)]
            Self::Fake(browser) => browser.lock().unwrap().press_key(target, key, modifiers),
        }
    }

    async fn diagnostics_monitor(
        &self,
        target: &CdpTarget,
        sink: Arc<dyn Fn(CdpDiagnosticEvent) + Send + Sync>,
    ) -> Result<Option<CdpDiagnosticsMonitor>, BrowserToolError> {
        match self {
            Self::Cdp(client) => Ok(Some(client.diagnostics_monitor(target, sink).await?)),
            #[cfg(test)]
            Self::Fake(_) => Ok(None),
        }
    }
}

pub async fn run(config: RuntimeConfig) -> Result<()> {
    prepare_state(&config).await?;

    let endpoint = broker_endpoint(&config)?;
    let listener = endpoint.listen()?;
    write_pid_file(&config).await?;
    let _runtime_files = RuntimeFileGuard::new(
        config.pid_path.clone(),
        endpoint.stale_path().map(Path::to_path_buf),
    );

    tracing::info!(
        cdp_endpoint = %config.cdp_endpoint,
        ipc_endpoint = %endpoint.display(),
        state_dir = %config.state_dir.display(),
        "visible browser broker listening"
    );

    serve(config, listener).await
}

pub async fn ensure_running(config: &RuntimeConfig) -> Result<BrokerClient> {
    prepare_state(config).await?;

    if let Ok(client) = connect_and_ping(config).await {
        return Ok(client);
    }

    let deadline = Instant::now() + BROKER_START_TIMEOUT;

    loop {
        if let Some(_lock) = BrokerStartLock::try_acquire(&config.lock_path)? {
            if let Ok(client) = connect_and_ping(config).await {
                return Ok(client);
            }

            cleanup_stale_endpoint(config)?;
            spawn_broker(config)?;
            return wait_for_broker(config, BROKER_START_TIMEOUT).await;
        }

        if let Ok(client) = wait_for_broker(config, Duration::from_millis(250)).await {
            return Ok(client);
        }

        if Instant::now() >= deadline {
            bail!(
                "timed out waiting for broker startup lock `{}`",
                config.lock_path.display()
            );
        }

        sleep(BROKER_CONNECT_RETRY).await;
    }
}

pub async fn prepare_state(config: &RuntimeConfig) -> Result<()> {
    tokio::fs::create_dir_all(&config.state_dir).await?;
    tokio::fs::create_dir_all(&config.log_dir).await?;
    Ok(())
}

pub fn cleanup_stale_endpoint(config: &RuntimeConfig) -> Result<StaleEndpointCleanup> {
    let endpoint = broker_endpoint(config)?;
    let Some(stale_path) = endpoint.stale_path() else {
        return Ok(StaleEndpointCleanup::NoFilesystemEndpoint);
    };

    if !stale_path.exists() {
        return Ok(StaleEndpointCleanup::NoEndpoint);
    }

    match read_pid(&config.pid_path)? {
        Some(pid) if process_is_alive(pid) => bail!(
            "broker IPC `{}` is unavailable but pid `{pid}` is still alive",
            endpoint.display()
        ),
        Some(_) => {
            fs::remove_file(stale_path).with_context(|| {
                format!(
                    "failed to remove stale broker endpoint `{}`",
                    endpoint.display()
                )
            })?;
            let _ = fs::remove_file(&config.pid_path);
            Ok(StaleEndpointCleanup::RemovedDeadPid)
        }
        None => {
            fs::remove_file(stale_path).with_context(|| {
                format!(
                    "failed to remove stale broker endpoint `{}`",
                    endpoint.display()
                )
            })?;
            Ok(StaleEndpointCleanup::RemovedWithoutPid)
        }
    }
}

async fn connect_and_ping(config: &RuntimeConfig) -> Result<BrokerClient> {
    let endpoint = broker_endpoint(config)?;
    let mut client = BrokerClient::connect(&endpoint).await?;
    client.ping().await?;
    Ok(client)
}

async fn wait_for_broker(config: &RuntimeConfig, timeout: Duration) -> Result<BrokerClient> {
    let deadline = Instant::now() + timeout;

    loop {
        match connect_and_ping(config).await {
            Ok(client) => return Ok(client),
            Err(error) if Instant::now() >= deadline => {
                return Err(error).with_context(|| {
                    format!(
                        "timed out waiting for broker socket `{}`",
                        config.ipc_endpoint
                    )
                });
            }
            Err(_) => {}
        }

        sleep(BROKER_CONNECT_RETRY).await;
    }
}

fn spawn_broker(config: &RuntimeConfig) -> Result<()> {
    let current_exe = std::env::current_exe().context("failed to locate current executable")?;
    let stdout = append_log_file(&config.log_dir.join("broker.stdout.log"))?;
    let stderr = append_log_file(&config.log_dir.join("broker.stderr.log"))?;

    let child = Command::new(current_exe)
        .arg("broker")
        .arg("--socket")
        .arg(&config.ipc_endpoint)
        .arg("--cdp-endpoint")
        .arg(&config.cdp_endpoint)
        .arg("--state-dir")
        .arg(&config.state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .context("failed to spawn visible browser broker")?;

    tracing::info!(
        pid = child.id(),
        ipc_endpoint = %config.ipc_endpoint,
        "spawned visible browser broker"
    );

    Ok(())
}

fn append_log_file(path: &Path) -> Result<File> {
    Ok(OpenOptions::new().create(true).append(true).open(path)?)
}

async fn write_pid_file(config: &RuntimeConfig) -> Result<()> {
    tokio::fs::write(&config.pid_path, std::process::id().to_string()).await?;
    Ok(())
}

async fn serve(config: RuntimeConfig, listener: BrokerListener) -> Result<()> {
    let state = BrokerState::new(&config)?;

    loop {
        let stream = ipc::accept(&listener).await?;
        let connection_config = config.clone();
        let connection_state = state.clone();

        tokio::spawn(async move {
            if let Err(error) = handle_connection(connection_config, connection_state, stream).await
            {
                tracing::warn!(error = %error, "broker connection failed");
            }
        });
    }
}

async fn handle_connection(
    config: RuntimeConfig,
    state: BrokerState,
    stream: BrokerStream,
) -> Result<()> {
    let mut stream = BufReader::new(stream);

    let mut line = String::new();
    loop {
        line.clear();
        let bytes = stream.read_line(&mut line).await?;
        if bytes == 0 {
            break;
        }

        let response = match serde_json::from_str::<BrokerRequest>(&line) {
            Ok(request) => dispatch_request(&config, &state, request).await,
            Err(error) => BrokerResponse::invalid_input(
                String::new(),
                format!("invalid broker request JSON: {error}"),
            ),
        };
        let encoded = serde_json::to_string(&response)?;

        stream.get_mut().write_all(encoded.as_bytes()).await?;
        stream.get_mut().write_all(b"\n").await?;
        stream.get_mut().flush().await?;
    }

    Ok(())
}

async fn dispatch_request(
    config: &RuntimeConfig,
    state: &BrokerState,
    request: BrokerRequest,
) -> BrokerResponse {
    match request.method.as_str() {
        "ping" => BrokerResponse::success(request.id.clone(), broker_status(config))
            .unwrap_or_else(|error| {
                BrokerResponse::error(
                    request.id,
                    BrowserToolError::invalid_input(format!(
                        "failed to serialize broker status: {error}"
                    )),
                )
            }),
        "start_session" => broker_response(
            request.id,
            broker_start_session(state, parse_params(request.params)).await,
        ),
        "list_tabs" => broker_response(
            request.id,
            broker_list_tabs(state, parse_params(request.params)).await,
        ),
        "new_tab" => broker_response(
            request.id,
            broker_new_tab(state, parse_params(request.params)).await,
        ),
        "claim_tab" => broker_response(
            request.id,
            broker_claim_tab(state, parse_params(request.params)).await,
        ),
        "release_tab" => broker_response(
            request.id,
            broker_release_tab(state, parse_params(request.params)).await,
        ),
        "focus_tab" => broker_response(
            request.id,
            broker_focus_tab(state, parse_params(request.params)).await,
        ),
        "navigate" => broker_response(
            request.id,
            broker_navigate(state, parse_params(request.params)).await,
        ),
        "screenshot" => broker_response(
            request.id,
            broker_screenshot(state, parse_params(request.params)).await,
        ),
        "evaluate" => broker_response(
            request.id,
            broker_evaluate(state, parse_params(request.params)).await,
        ),
        "click" => broker_response(
            request.id,
            broker_click(state, parse_params(request.params)).await,
        ),
        "type_text" => broker_response(
            request.id,
            broker_type_text(state, parse_params(request.params)).await,
        ),
        "press_key" => broker_response(
            request.id,
            broker_press_key(state, parse_params(request.params)).await,
        ),
        "console_messages" => broker_response(
            request.id,
            broker_console_messages(state, parse_params(request.params)).await,
        ),
        "network_events" => broker_response(
            request.id,
            broker_network_events(state, parse_params(request.params)).await,
        ),
        "close_tab" => broker_response(
            request.id,
            broker_close_tab(state, parse_params(request.params)).await,
        ),
        method => {
            BrokerResponse::invalid_input(request.id, format!("unknown broker method `{method}`"))
        }
    }
}

fn parse_params<T>(params: serde_json::Value) -> Result<T, BrowserToolError>
where
    T: DeserializeOwned,
{
    serde_json::from_value(params)
        .map_err(|error| BrowserToolError::invalid_input(format!("invalid parameters: {error}")))
}

fn broker_response<T>(id: String, result: Result<T, BrowserToolError>) -> BrokerResponse
where
    T: Serialize,
{
    match result {
        Ok(result) => BrokerResponse::success(id.clone(), result).unwrap_or_else(|error| {
            BrokerResponse::error(
                id,
                BrowserToolError::invalid_input(format!(
                    "failed to serialize broker response: {error}"
                )),
            )
        }),
        Err(error) => BrokerResponse::error(id, error),
    }
}

async fn broker_start_session(
    state: &BrokerState,
    params: Result<StartSessionParams, BrowserToolError>,
) -> Result<StartSessionResult, BrowserToolError> {
    let params = params?;
    let session = {
        let mut registry = state.registry().lock().unwrap();
        registry.start_session(params.label)
    };

    let tab = match params.start_url {
        Some(url) => Some(
            create_and_lease_tab(state, &session.agent_session_id, Some(url), params.focus).await?,
        ),
        None => None,
    };

    Ok(StartSessionResult {
        agent_session_id: session.agent_session_id,
        tab,
    })
}

async fn broker_list_tabs(
    state: &BrokerState,
    params: Result<ListTabsParams, BrowserToolError>,
) -> Result<ListTabsResult, BrowserToolError> {
    let params = params?;
    let targets = state.browser.page_targets().await?;
    reconcile_missing_targets(state, &targets);
    let focused_target_id = state.focused_target_id_for_targets(&targets);

    match params.scope.unwrap_or(ListTabsScope::Owned) {
        ListTabsScope::Owned => {
            let tabs = state
                .registry()
                .lock()
                .unwrap()
                .list_owned_tabs(&params.agent_session_id, focused_target_id.as_deref())?;
            Ok(ListTabsResult::Owned { tabs })
        }
        ListTabsScope::GlobalReadonly => {
            let snapshots = targets
                .iter()
                .map(|target| tab_snapshot(target, focused_target_id.as_deref()))
                .collect::<Vec<_>>();
            let inventory = state
                .registry()
                .lock()
                .unwrap()
                .global_inventory(&params.agent_session_id, snapshots)?;
            Ok(ListTabsResult::GlobalReadonly {
                groups: inventory.groups,
            })
        }
    }
}

async fn broker_new_tab(
    state: &BrokerState,
    params: Result<NewTabParams, BrowserToolError>,
) -> Result<TabResult, BrowserToolError> {
    let params = params?;
    let tab =
        create_and_lease_tab(state, &params.agent_session_id, params.url, params.focus).await?;
    Ok(TabResult { tab })
}

async fn broker_claim_tab(
    state: &BrokerState,
    params: Result<ClaimTabParams, BrowserToolError>,
) -> Result<TabResult, BrowserToolError> {
    let params = params?;
    let target = target_by_id(state, &params.target_id).await?;
    let tab = state.registry().lock().unwrap().claim_tab(
        &params.agent_session_id,
        tab_snapshot(
            &target,
            state
                .is_focused_target(&target.id)
                .then_some(target.id.as_str()),
        ),
        params.takeover,
        params.user_instruction.as_deref(),
    )?;
    state.diagnostics().lock().unwrap().reset_target(&target.id);
    ensure_diagnostics_monitor(state, &target).await?;

    Ok(TabResult { tab })
}

async fn broker_release_tab(
    state: &BrokerState,
    params: Result<TabActionParams, BrowserToolError>,
) -> Result<ReleaseTabResult, BrowserToolError> {
    let params = params?;
    let lease = state
        .registry()
        .lock()
        .unwrap()
        .release_tab(&params.agent_session_id, &params.tab_id)?;
    state
        .diagnostics()
        .lock()
        .unwrap()
        .reset_target(&lease.target_id);
    Ok(ReleaseTabResult { released: true })
}

async fn broker_focus_tab(
    state: &BrokerState,
    params: Result<TabActionParams, BrowserToolError>,
) -> Result<TabResult, BrowserToolError> {
    let params = params?;
    let target = active_owned_target(state, &params.agent_session_id, &params.tab_id).await?;
    ensure_diagnostics_monitor(state, &target).await?;
    state.browser.activate_target(&target.id).await?;
    state.mark_focused_target(&target.id);
    let lease = state
        .registry()
        .lock()
        .unwrap()
        .update_tab_snapshot(&params.tab_id, tab_snapshot(&target, Some(&target.id)))?;

    Ok(TabResult {
        tab: owned_summary(&lease, true),
    })
}

async fn broker_navigate(
    state: &BrokerState,
    params: Result<NavigateParams, BrowserToolError>,
) -> Result<TabResult, BrowserToolError> {
    let params = params?;
    let target = active_owned_target(state, &params.agent_session_id, &params.tab_id).await?;
    ensure_diagnostics_monitor(state, &target).await?;
    state.browser.activate_target(&target.id).await?;
    state.mark_focused_target(&target.id);
    let target = state
        .browser
        .navigate(
            &target,
            &params.url,
            params.wait_until.as_deref(),
            params.timeout_ms.unwrap_or(DEFAULT_NAVIGATION_TIMEOUT_MS),
        )
        .await?;
    let lease = state
        .registry()
        .lock()
        .unwrap()
        .update_tab_snapshot(&params.tab_id, tab_snapshot(&target, Some(&target.id)))?;

    Ok(TabResult {
        tab: owned_summary(&lease, true),
    })
}

async fn broker_screenshot(
    state: &BrokerState,
    params: Result<ScreenshotParams, BrowserToolError>,
) -> Result<ScreenshotResult, BrowserToolError> {
    let params = params?;
    let target = active_owned_target(state, &params.agent_session_id, &params.tab_id).await?;
    ensure_diagnostics_monitor(state, &target).await?;
    state.browser.activate_target(&target.id).await?;
    state.mark_focused_target(&target.id);
    let data_base64 = state.browser.screenshot(&target, params.full_page).await?;

    Ok(ScreenshotResult {
        mime_type: "image/png".to_string(),
        data_base64,
    })
}

async fn broker_evaluate(
    state: &BrokerState,
    params: Result<EvaluateParams, BrowserToolError>,
) -> Result<EvaluateResult, BrowserToolError> {
    let params = params?;
    let target = active_owned_target(state, &params.agent_session_id, &params.tab_id).await?;
    ensure_diagnostics_monitor(state, &target).await?;
    state.browser.evaluate(&target, &params.expression).await
}

async fn broker_click(
    state: &BrokerState,
    params: Result<ClickParams, BrowserToolError>,
) -> Result<ClickResult, BrowserToolError> {
    let params = params?;
    let target = active_owned_target(state, &params.agent_session_id, &params.tab_id).await?;
    ensure_diagnostics_monitor(state, &target).await?;
    state.browser.activate_target(&target.id).await?;
    state.mark_focused_target(&target.id);
    state
        .browser
        .click(
            &target,
            &params.selector,
            params.timeout_ms.unwrap_or(DEFAULT_CLICK_TIMEOUT_MS),
        )
        .await?;
    Ok(ClickResult { clicked: true })
}

async fn broker_type_text(
    state: &BrokerState,
    params: Result<TypeTextParams, BrowserToolError>,
) -> Result<TypeTextResult, BrowserToolError> {
    let params = params?;
    let target = active_owned_target(state, &params.agent_session_id, &params.tab_id).await?;
    ensure_diagnostics_monitor(state, &target).await?;
    state.browser.activate_target(&target.id).await?;
    state.mark_focused_target(&target.id);
    state.browser.type_text(&target, &params.text).await?;
    Ok(TypeTextResult { typed: true })
}

async fn broker_press_key(
    state: &BrokerState,
    params: Result<PressKeyParams, BrowserToolError>,
) -> Result<PressKeyResult, BrowserToolError> {
    let params = params?;
    let target = active_owned_target(state, &params.agent_session_id, &params.tab_id).await?;
    ensure_diagnostics_monitor(state, &target).await?;
    state.browser.activate_target(&target.id).await?;
    state.mark_focused_target(&target.id);
    state
        .browser
        .press_key(&target, &params.key, &params.modifiers)
        .await?;
    Ok(PressKeyResult { pressed: true })
}

async fn broker_console_messages(
    state: &BrokerState,
    params: Result<DiagnosticsParams, BrowserToolError>,
) -> Result<ConsoleMessagesResult, BrowserToolError> {
    let params = params?;
    let target = active_owned_target(state, &params.agent_session_id, &params.tab_id).await?;
    ensure_diagnostics_monitor(state, &target).await?;
    let messages = state
        .diagnostics()
        .lock()
        .unwrap()
        .console_messages(&target.id, params.since);
    Ok(ConsoleMessagesResult { messages })
}

async fn broker_network_events(
    state: &BrokerState,
    params: Result<DiagnosticsParams, BrowserToolError>,
) -> Result<NetworkEventsResult, BrowserToolError> {
    let params = params?;
    let target = active_owned_target(state, &params.agent_session_id, &params.tab_id).await?;
    ensure_diagnostics_monitor(state, &target).await?;
    let events = state
        .diagnostics()
        .lock()
        .unwrap()
        .network_events(&target.id, params.since);
    Ok(NetworkEventsResult { events })
}

async fn broker_close_tab(
    state: &BrokerState,
    params: Result<TabActionParams, BrowserToolError>,
) -> Result<CloseTabResult, BrowserToolError> {
    let params = params?;
    let lease = state
        .registry()
        .lock()
        .unwrap()
        .owned_lease(&params.agent_session_id, &params.tab_id)?;

    if matches!(lease.state, LeaseState::Active) {
        match target_by_id(state, &lease.target_id).await {
            Ok(_) => state.browser.close_target(&lease.target_id).await?,
            Err(error) if error.code == crate::leases::BrowserToolErrorCode::TargetMissing => {}
            Err(error) => return Err(error),
        }
    }

    let closed = state
        .registry()
        .lock()
        .unwrap()
        .close_tab_mark(&params.agent_session_id, &params.tab_id)?;
    state.clear_focused_target(&closed.target_id);
    state
        .diagnostics()
        .lock()
        .unwrap()
        .reset_target(&closed.target_id);

    Ok(CloseTabResult { closed: true })
}

async fn create_and_lease_tab(
    state: &BrokerState,
    session_id: &AgentSessionId,
    url: Option<String>,
    focus: bool,
) -> Result<OwnedTabSummary, BrowserToolError> {
    {
        let registry = state.registry().lock().unwrap();
        registry.ensure_session(session_id)?;
    }

    let target = state.browser.create_page(url.as_deref(), focus).await?;
    if focus {
        state.mark_focused_target(&target.id);
    }
    let snapshot = tab_snapshot(&target, focus.then_some(target.id.as_str()));
    let summary = state
        .registry()
        .lock()
        .unwrap()
        .lease_tab(session_id, snapshot)?;
    state.diagnostics().lock().unwrap().reset_target(&target.id);
    ensure_diagnostics_monitor(state, &target).await?;
    Ok(summary)
}

async fn active_owned_target(
    state: &BrokerState,
    session_id: &AgentSessionId,
    tab_id: &TabId,
) -> Result<CdpTarget, BrowserToolError> {
    let targets = state.browser.page_targets().await?;
    reconcile_missing_targets(state, &targets);
    let lease = {
        let target_exists = |target_id: &str| targets.iter().any(|target| target.id == target_id);
        let mut registry = state.registry().lock().unwrap();
        let lease = registry.owned_lease(session_id, tab_id)?;
        match registry.require_active_owned(session_id, tab_id, target_exists(&lease.target_id)) {
            Ok(lease) => lease,
            Err(error) => {
                if matches!(
                    &error.code,
                    crate::leases::BrowserToolErrorCode::TargetMissing
                ) {
                    state
                        .diagnostics()
                        .lock()
                        .unwrap()
                        .reset_target(&lease.target_id);
                }
                return Err(error);
            }
        }
    };

    targets
        .into_iter()
        .find(|target| target.id == lease.target_id)
        .ok_or_else(|| BrowserToolError::target_missing(tab_id))
}

async fn target_by_id(state: &BrokerState, target_id: &str) -> Result<CdpTarget, BrowserToolError> {
    state
        .browser
        .page_targets()
        .await?
        .into_iter()
        .find(|target| target.id == target_id)
        .ok_or_else(|| BrowserToolError::target_missing_for_target(target_id))
}

fn reconcile_missing_targets(state: &BrokerState, targets: &[CdpTarget]) {
    let visible_ids = targets
        .iter()
        .map(|target| target.id.clone())
        .collect::<Vec<_>>();
    let missing = state
        .registry()
        .lock()
        .unwrap()
        .mark_missing_targets_not_in(visible_ids);
    if !missing.is_empty() {
        let mut diagnostics = state.diagnostics().lock().unwrap();
        for lease in missing {
            state.clear_focused_target(&lease.target_id);
            diagnostics.reset_target(&lease.target_id);
        }
    }
}

async fn ensure_diagnostics_monitor(
    state: &BrokerState,
    target: &CdpTarget,
) -> Result<(), BrowserToolError> {
    let should_start = {
        let mut diagnostics = state.diagnostics().lock().unwrap();
        diagnostics.ensure_target(&target.id);
        !diagnostics.is_monitored(&target.id)
    };

    if !should_start {
        return Ok(());
    }

    let target_id = target.id.clone();
    let diagnostics = state.diagnostics.clone();
    let sink = Arc::new(move |event| {
        diagnostics.lock().unwrap().push_event(&target_id, event);
    });
    let monitor = state.browser.diagnostics_monitor(target, sink).await?;
    state
        .diagnostics()
        .lock()
        .unwrap()
        .mark_monitored(&target.id, monitor);
    Ok(())
}

fn owned_summary(lease: &TabLease, focused: bool) -> OwnedTabSummary {
    OwnedTabSummary {
        tab_id: lease.tab_id.clone(),
        target_id: lease.target_id.clone(),
        title: lease.title.clone().unwrap_or_default(),
        url: lease.url.clone().unwrap_or_default(),
        state: lease.state.clone(),
        focused,
        created_at_ms: lease.created_at_ms,
        updated_at_ms: lease.updated_at_ms,
    }
}

fn tab_snapshot(target: &CdpTarget, focused_target_id: Option<&str>) -> TabSnapshot {
    let mut snapshot = TabSnapshot::from(target);
    snapshot.focused = focused_target_id == Some(target.id.as_str());
    snapshot
}

fn broker_status(config: &RuntimeConfig) -> BrokerStatus {
    BrokerStatus {
        protocol_version: BROKER_PROTOCOL_VERSION,
        pid: std::process::id(),
        cdp_endpoint: config.cdp_endpoint.clone(),
        ipc_endpoint: config.ipc_endpoint.clone(),
        socket_path: config.socket_path.clone(),
    }
}

fn broker_endpoint(config: &RuntimeConfig) -> Result<BrokerEndpoint> {
    BrokerEndpoint::from_state(&config.state_dir, Some(&config.ipc_endpoint))
}

fn read_pid(path: &Path) -> Result<Option<u32>> {
    match fs::read_to_string(path) {
        Ok(contents) => Ok(contents.trim().parse::<u32>().ok()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("failed to read `{}`", path.display())),
    }
}

fn process_is_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }

    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if result == 0 {
            return true;
        }

        return std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    }

    #[cfg(windows)]
    {
        let filter = format!("PID eq {pid}");
        let Ok(output) = Command::new("tasklist")
            .args(["/FI", &filter, "/FO", "CSV", "/NH"])
            .output()
        else {
            return false;
        };

        if !output.status.success() {
            return false;
        }

        String::from_utf8_lossy(&output.stdout).contains(&pid.to_string())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaleEndpointCleanup {
    NoEndpoint,
    NoFilesystemEndpoint,
    RemovedWithoutPid,
    RemovedDeadPid,
}

struct BrokerStartLock {
    _file: File,
}

impl BrokerStartLock {
    fn try_acquire(lock_path: &Path) -> Result<Option<Self>> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(lock_path)
            .with_context(|| format!("failed to open broker lock `{}`", lock_path.display()))?;

        match file.try_lock_exclusive() {
            Ok(()) => Ok(Some(Self { _file: file })),
            Err(error) if error.kind() == ErrorKind::WouldBlock => Ok(None),
            Err(error) => {
                Err(error).with_context(|| format!("failed to lock `{}`", lock_path.display()))
            }
        }
    }
}

impl Drop for BrokerStartLock {
    fn drop(&mut self) {
        let _ = self._file.unlock();
    }
}

struct RuntimeFileGuard {
    pid_path: PathBuf,
    stale_path: Option<PathBuf>,
}

impl RuntimeFileGuard {
    fn new(pid_path: PathBuf, stale_path: Option<PathBuf>) -> Self {
        Self {
            pid_path,
            stale_path,
        }
    }
}

impl Drop for RuntimeFileGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.pid_path);
        if let Some(stale_path) = &self.stale_path {
            let _ = fs::remove_file(stale_path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{config::RuntimeConfig, leases::TabSnapshot};
    use serde::ser::Error as _;
    use serde_json::json;

    fn test_config(state_dir: PathBuf) -> RuntimeConfig {
        RuntimeConfig::from_parts("http://127.0.0.1:9222".to_string(), state_dir).unwrap()
    }

    fn fake_target(id: &str) -> CdpTarget {
        CdpTarget {
            id: id.to_string(),
            target_type: "page".to_string(),
            title: format!("Title {id}"),
            url: format!("https://example.com/{id}"),
            web_socket_debugger_url: Some(format!("ws://fake/{id}")),
        }
    }

    fn fake_state(targets: Vec<CdpTarget>) -> BrokerState {
        BrokerState::with_browser(BrowserBackend::Fake(Arc::new(Mutex::new(
            FakeBrowser::with_targets(targets),
        ))))
    }

    #[tokio::test]
    async fn prepare_state_creates_state_and_log_directories() {
        let tempdir = tempfile::tempdir().unwrap();
        let state_dir = tempdir.path().join("state");
        let config = test_config(state_dir.clone());

        prepare_state(&config).await.unwrap();

        assert!(state_dir.is_dir());
        assert!(state_dir.join("logs").is_dir());
    }

    #[tokio::test]
    async fn broker_protocol_responds_to_ping() {
        let tempdir = tempfile::tempdir().unwrap();
        let config = test_config(tempdir.path().join("state"));
        prepare_state(&config).await.unwrap();
        let endpoint = broker_endpoint(&config).unwrap();
        let listener = endpoint.listen().unwrap();
        let server = tokio::spawn(serve(config.clone(), listener));

        let mut client = BrokerClient::connect(&endpoint).await.unwrap();
        let status = client.ping().await.unwrap();

        assert_eq!(status.protocol_version, BROKER_PROTOCOL_VERSION);
        assert_eq!(status.cdp_endpoint, "http://127.0.0.1:9222");
        assert_eq!(status.ipc_endpoint, config.ipc_endpoint);

        server.abort();
    }

    #[test]
    fn serialization_fallback_preserves_request_id() {
        struct FailsSerialize;

        impl Serialize for FailsSerialize {
            fn serialize<S>(&self, _serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                Err(S::Error::custom("intentional serialization failure"))
            }
        }

        let response = broker_response("request-1".to_string(), Ok(FailsSerialize));

        assert_eq!(response.id, "request-1");
        assert!(!response.ok);
        assert!(response.result.is_none());
        assert_eq!(
            response.error.unwrap().message,
            "failed to serialize broker response: intentional serialization failure"
        );
    }

    #[test]
    fn broker_state_carries_shared_lease_registry() {
        let state = fake_state(Vec::new());
        let mut registry = state.registry.lock().unwrap();

        let session = registry.start_session(Some("agent".to_string()));
        let leased = registry
            .lease_tab(
                &session.agent_session_id,
                TabSnapshot::new("target-1", "Target", "https://example.com", false),
            )
            .unwrap();

        assert!(leased.tab_id.0.starts_with("tab_"));
    }

    #[tokio::test]
    async fn start_session_can_create_initial_leased_tab() {
        let state = fake_state(Vec::new());

        let result = broker_start_session(
            &state,
            Ok(StartSessionParams {
                label: Some("agent".to_string()),
                start_url: Some("https://example.com/start".to_string()),
                focus: true,
            }),
        )
        .await
        .unwrap();

        let tab = result.tab.unwrap();
        assert!(result.agent_session_id.0.starts_with("session_"));
        assert_eq!(tab.url, "https://example.com/start");
        assert!(tab.focused);
    }

    #[tokio::test]
    async fn list_tabs_defaults_to_owned_and_global_readonly_withholds_foreign_handles() {
        let state = fake_state(vec![fake_target("target-a"), fake_target("target-b")]);
        let first = broker_start_session(
            &state,
            Ok(StartSessionParams {
                label: Some("first".to_string()),
                start_url: None,
                focus: false,
            }),
        )
        .await
        .unwrap();
        let second = broker_start_session(
            &state,
            Ok(StartSessionParams {
                label: Some("second".to_string()),
                start_url: None,
                focus: false,
            }),
        )
        .await
        .unwrap();
        let first_tab = broker_claim_tab(
            &state,
            Ok(ClaimTabParams {
                agent_session_id: first.agent_session_id.clone(),
                target_id: "target-a".to_string(),
                takeover: false,
                user_instruction: None,
            }),
        )
        .await
        .unwrap()
        .tab;
        broker_claim_tab(
            &state,
            Ok(ClaimTabParams {
                agent_session_id: second.agent_session_id.clone(),
                target_id: "target-b".to_string(),
                takeover: false,
                user_instruction: None,
            }),
        )
        .await
        .unwrap();
        broker_focus_tab(
            &state,
            Ok(TabActionParams {
                agent_session_id: first.agent_session_id.clone(),
                tab_id: first_tab.tab_id.clone(),
            }),
        )
        .await
        .unwrap();

        let owned = broker_list_tabs(
            &state,
            Ok(ListTabsParams {
                agent_session_id: first.agent_session_id.clone(),
                scope: None,
            }),
        )
        .await
        .unwrap();
        let global = broker_list_tabs(
            &state,
            Ok(ListTabsParams {
                agent_session_id: first.agent_session_id,
                scope: Some(ListTabsScope::GlobalReadonly),
            }),
        )
        .await
        .unwrap();

        match owned {
            ListTabsResult::Owned { tabs } => {
                assert_eq!(tabs.len(), 1);
                assert_eq!(tabs[0].tab_id, first_tab.tab_id);
                assert!(tabs[0].focused);
            }
            ListTabsResult::GlobalReadonly { .. } => panic!("expected owned tab listing"),
        }

        match global {
            ListTabsResult::GlobalReadonly { groups } => {
                let tabs = groups
                    .iter()
                    .flat_map(|group| group.tabs.iter())
                    .collect::<Vec<_>>();
                let first_summary = tabs
                    .iter()
                    .find(|summary| summary.target_id == "target-a")
                    .unwrap();
                let second_summary = tabs
                    .iter()
                    .find(|summary| summary.target_id == "target-b")
                    .unwrap();

                assert_eq!(first_summary.caller_tab_id, Some(first_tab.tab_id));
                assert!(first_summary.owned_by_caller);
                assert!(first_summary.focused);
                assert_eq!(second_summary.caller_tab_id, None);
                assert!(!second_summary.owned_by_caller);
                assert!(!second_summary.focused);
            }
            ListTabsResult::Owned { .. } => panic!("expected global tab listing"),
        }
    }

    #[tokio::test]
    async fn ownership_is_enforced_before_core_tab_actions() {
        let state = fake_state(vec![fake_target("target-a")]);
        let owner = broker_start_session(
            &state,
            Ok(StartSessionParams {
                label: Some("owner".to_string()),
                start_url: None,
                focus: false,
            }),
        )
        .await
        .unwrap();
        let foreign = broker_start_session(
            &state,
            Ok(StartSessionParams {
                label: Some("foreign".to_string()),
                start_url: None,
                focus: false,
            }),
        )
        .await
        .unwrap();
        let tab = broker_claim_tab(
            &state,
            Ok(ClaimTabParams {
                agent_session_id: owner.agent_session_id.clone(),
                target_id: "target-a".to_string(),
                takeover: false,
                user_instruction: None,
            }),
        )
        .await
        .unwrap()
        .tab;

        let focus_error = broker_focus_tab(
            &state,
            Ok(TabActionParams {
                agent_session_id: foreign.agent_session_id.clone(),
                tab_id: tab.tab_id.clone(),
            }),
        )
        .await
        .unwrap_err();
        let navigate_error = broker_navigate(
            &state,
            Ok(NavigateParams {
                agent_session_id: foreign.agent_session_id.clone(),
                tab_id: tab.tab_id.clone(),
                url: "https://example.com/new".to_string(),
                wait_until: None,
                timeout_ms: None,
            }),
        )
        .await
        .unwrap_err();
        let screenshot_error = broker_screenshot(
            &state,
            Ok(ScreenshotParams {
                agent_session_id: foreign.agent_session_id,
                tab_id: tab.tab_id,
                full_page: false,
            }),
        )
        .await
        .unwrap_err();

        assert_eq!(
            focus_error.code,
            crate::leases::BrowserToolErrorCode::TabNotOwned
        );
        assert_eq!(
            navigate_error.code,
            crate::leases::BrowserToolErrorCode::TabNotOwned
        );
        assert_eq!(
            screenshot_error.code,
            crate::leases::BrowserToolErrorCode::TabNotOwned
        );
    }

    #[tokio::test]
    async fn navigate_release_close_and_missing_target_paths_update_leases() {
        let fake = Arc::new(Mutex::new(FakeBrowser::with_targets(vec![fake_target(
            "target-a",
        )])));
        let state = BrokerState::with_browser(BrowserBackend::Fake(fake.clone()));
        let session = broker_start_session(
            &state,
            Ok(StartSessionParams {
                label: Some("owner".to_string()),
                start_url: None,
                focus: false,
            }),
        )
        .await
        .unwrap();
        let tab = broker_claim_tab(
            &state,
            Ok(ClaimTabParams {
                agent_session_id: session.agent_session_id.clone(),
                target_id: "target-a".to_string(),
                takeover: false,
                user_instruction: None,
            }),
        )
        .await
        .unwrap()
        .tab;

        let navigated = broker_navigate(
            &state,
            Ok(NavigateParams {
                agent_session_id: session.agent_session_id.clone(),
                tab_id: tab.tab_id.clone(),
                url: "https://example.com/after".to_string(),
                wait_until: None,
                timeout_ms: None,
            }),
        )
        .await
        .unwrap()
        .tab;
        assert_eq!(navigated.url, "https://example.com/after");

        broker_release_tab(
            &state,
            Ok(TabActionParams {
                agent_session_id: session.agent_session_id.clone(),
                tab_id: tab.tab_id.clone(),
            }),
        )
        .await
        .unwrap();
        let released_owned = broker_list_tabs(
            &state,
            Ok(ListTabsParams {
                agent_session_id: session.agent_session_id.clone(),
                scope: None,
            }),
        )
        .await
        .unwrap();
        assert!(matches!(released_owned, ListTabsResult::Owned { tabs } if tabs.is_empty()));

        let reclaimed = broker_claim_tab(
            &state,
            Ok(ClaimTabParams {
                agent_session_id: session.agent_session_id.clone(),
                target_id: "target-a".to_string(),
                takeover: false,
                user_instruction: None,
            }),
        )
        .await
        .unwrap()
        .tab;
        broker_close_tab(
            &state,
            Ok(TabActionParams {
                agent_session_id: session.agent_session_id.clone(),
                tab_id: reclaimed.tab_id.clone(),
            }),
        )
        .await
        .unwrap();
        assert!(fake.lock().unwrap().was_closed("target-a"));

        let missing = broker_new_tab(
            &state,
            Ok(NewTabParams {
                agent_session_id: session.agent_session_id.clone(),
                url: Some("https://example.com/missing".to_string()),
                focus: false,
            }),
        )
        .await
        .unwrap()
        .tab;
        fake.lock().unwrap().remove_target(&missing.target_id);
        let missing_error = broker_focus_tab(
            &state,
            Ok(TabActionParams {
                agent_session_id: session.agent_session_id,
                tab_id: missing.tab_id,
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(
            missing_error.code,
            crate::leases::BrowserToolErrorCode::TargetMissing
        );
    }

    #[tokio::test]
    async fn page_actions_require_owned_tabs_and_route_to_browser_backend() {
        let fake = Arc::new(Mutex::new(FakeBrowser::with_targets(vec![fake_target(
            "target-a",
        )])));
        let state = BrokerState::with_browser(BrowserBackend::Fake(fake.clone()));
        let owner = broker_start_session(
            &state,
            Ok(StartSessionParams {
                label: Some("owner".to_string()),
                start_url: None,
                focus: false,
            }),
        )
        .await
        .unwrap();
        let foreign = broker_start_session(
            &state,
            Ok(StartSessionParams {
                label: Some("foreign".to_string()),
                start_url: None,
                focus: false,
            }),
        )
        .await
        .unwrap();
        let tab = broker_claim_tab(
            &state,
            Ok(ClaimTabParams {
                agent_session_id: owner.agent_session_id.clone(),
                target_id: "target-a".to_string(),
                takeover: false,
                user_instruction: None,
            }),
        )
        .await
        .unwrap()
        .tab;

        let evaluated = broker_evaluate(
            &state,
            Ok(EvaluateParams {
                agent_session_id: owner.agent_session_id.clone(),
                tab_id: tab.tab_id.clone(),
                expression: "1 + 1".to_string(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(evaluated.value, Some(json!(2)));

        let clicked = broker_click(
            &state,
            Ok(ClickParams {
                agent_session_id: owner.agent_session_id.clone(),
                tab_id: tab.tab_id.clone(),
                selector: "#submit".to_string(),
                timeout_ms: None,
            }),
        )
        .await
        .unwrap();
        assert!(clicked.clicked);

        broker_type_text(
            &state,
            Ok(TypeTextParams {
                agent_session_id: owner.agent_session_id.clone(),
                tab_id: tab.tab_id.clone(),
                text: "hello".to_string(),
            }),
        )
        .await
        .unwrap();
        broker_press_key(
            &state,
            Ok(PressKeyParams {
                agent_session_id: owner.agent_session_id.clone(),
                tab_id: tab.tab_id.clone(),
                key: "Enter".to_string(),
                modifiers: Vec::new(),
            }),
        )
        .await
        .unwrap();

        let fake = fake.lock().unwrap();
        assert!(fake.was_clicked("#submit"));
        assert_eq!(fake.typed_text(), &["hello".to_string()]);
        assert_eq!(fake.pressed_keys(), &["Enter".to_string()]);
        drop(fake);

        let foreign_error = broker_evaluate(
            &state,
            Ok(EvaluateParams {
                agent_session_id: foreign.agent_session_id,
                tab_id: tab.tab_id,
                expression: "1 + 1".to_string(),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(
            foreign_error.code,
            crate::leases::BrowserToolErrorCode::TabNotOwned
        );
    }

    #[tokio::test]
    async fn diagnostics_buffers_support_since_and_reset_on_release() {
        let state = fake_state(vec![fake_target("target-a")]);
        let session = broker_start_session(
            &state,
            Ok(StartSessionParams {
                label: Some("owner".to_string()),
                start_url: None,
                focus: false,
            }),
        )
        .await
        .unwrap();
        let tab = broker_claim_tab(
            &state,
            Ok(ClaimTabParams {
                agent_session_id: session.agent_session_id.clone(),
                target_id: "target-a".to_string(),
                takeover: false,
                user_instruction: None,
            }),
        )
        .await
        .unwrap()
        .tab;

        {
            let mut diagnostics = state.diagnostics().lock().unwrap();
            diagnostics.push_event(
                "target-a",
                CdpDiagnosticEvent::Console {
                    level: "log".to_string(),
                    text: "first".to_string(),
                    timestamp_ms: Some(1),
                },
            );
            diagnostics.push_event(
                "target-a",
                CdpDiagnosticEvent::Console {
                    level: "log".to_string(),
                    text: "second".to_string(),
                    timestamp_ms: Some(2),
                },
            );
            diagnostics.push_event(
                "target-a",
                CdpDiagnosticEvent::Network(NetworkEvent {
                    sequence: 0,
                    kind: "request".to_string(),
                    url: Some("https://example.com/data.json".to_string()),
                    method: Some("GET".to_string()),
                    status: None,
                    error_text: None,
                    timestamp_ms: Some(3),
                }),
            );
        }

        let messages = broker_console_messages(
            &state,
            Ok(DiagnosticsParams {
                agent_session_id: session.agent_session_id.clone(),
                tab_id: tab.tab_id.clone(),
                since: None,
            }),
        )
        .await
        .unwrap()
        .messages;
        assert_eq!(messages.len(), 2);
        let since = messages[0].sequence;
        let filtered = broker_console_messages(
            &state,
            Ok(DiagnosticsParams {
                agent_session_id: session.agent_session_id.clone(),
                tab_id: tab.tab_id.clone(),
                since: Some(since),
            }),
        )
        .await
        .unwrap()
        .messages;
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].text, "second");

        let network = broker_network_events(
            &state,
            Ok(DiagnosticsParams {
                agent_session_id: session.agent_session_id.clone(),
                tab_id: tab.tab_id.clone(),
                since: None,
            }),
        )
        .await
        .unwrap()
        .events;
        assert_eq!(network.len(), 1);
        assert_eq!(
            network[0].url.as_deref(),
            Some("https://example.com/data.json")
        );

        broker_release_tab(
            &state,
            Ok(TabActionParams {
                agent_session_id: session.agent_session_id.clone(),
                tab_id: tab.tab_id.clone(),
            }),
        )
        .await
        .unwrap();

        let reclaimed = broker_claim_tab(
            &state,
            Ok(ClaimTabParams {
                agent_session_id: session.agent_session_id.clone(),
                target_id: "target-a".to_string(),
                takeover: false,
                user_instruction: None,
            }),
        )
        .await
        .unwrap()
        .tab;
        let after_reset = broker_console_messages(
            &state,
            Ok(DiagnosticsParams {
                agent_session_id: session.agent_session_id,
                tab_id: reclaimed.tab_id,
                since: None,
            }),
        )
        .await
        .unwrap()
        .messages;
        assert!(after_reset.is_empty());
    }

    #[tokio::test]
    async fn ensure_running_uses_existing_broker_socket() {
        let tempdir = tempfile::tempdir().unwrap();
        let config = test_config(tempdir.path().join("state"));
        prepare_state(&config).await.unwrap();
        let endpoint = broker_endpoint(&config).unwrap();
        let listener = endpoint.listen().unwrap();
        let server = tokio::spawn(serve(config.clone(), listener));

        let mut client = ensure_running(&config).await.unwrap();
        let status = client.ping().await.unwrap();

        assert_eq!(status.ipc_endpoint, config.ipc_endpoint);

        server.abort();
    }

    #[test]
    fn stale_socket_cleanup_removes_socket_when_pid_is_missing() {
        if cfg!(windows) {
            return;
        }

        let tempdir = tempfile::tempdir().unwrap();
        let config = test_config(tempdir.path().join("state"));
        fs::create_dir_all(&config.state_dir).unwrap();
        File::create(&config.socket_path).unwrap();

        let result = cleanup_stale_endpoint(&config).unwrap();

        assert_eq!(result, StaleEndpointCleanup::RemovedWithoutPid);
        assert!(!config.socket_path.exists());
    }

    #[test]
    fn stale_socket_cleanup_preserves_socket_when_pid_is_alive() {
        if cfg!(windows) {
            return;
        }

        let tempdir = tempfile::tempdir().unwrap();
        let config = test_config(tempdir.path().join("state"));
        fs::create_dir_all(&config.state_dir).unwrap();
        File::create(&config.socket_path).unwrap();
        fs::write(&config.pid_path, std::process::id().to_string()).unwrap();

        let error = cleanup_stale_endpoint(&config).unwrap_err();

        assert!(error.to_string().contains("still alive"));
        assert!(config.socket_path.exists());
    }
}
