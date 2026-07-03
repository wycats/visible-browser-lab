use std::{
    collections::{HashMap, HashSet, VecDeque},
    fs::{self, File, OpenOptions},
    future::Future,
    io::ErrorKind,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use fs2::FileExt;
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::Mutex as AsyncMutex,
    time::{Instant, sleep},
};
use url::Url;

use crate::{
    artifacts::ArtifactRegistry,
    cdp::{
        CapturedScreencastFrame, CdpClient, CdpDiagnosticEvent, CdpDiagnosticsMonitor,
        CdpScreencastCapture, CdpTarget, CdpTraceCapture, ElementEvaluation,
    },
    config::{CHROME_PATH_ENV, RuntimeConfig, RuntimeMode},
    ipc::{self, BrokerEndpoint, BrokerListener, BrokerStream},
    leases::{
        AgentSessionId, BrowserToolError, BrowserToolErrorCode, LeaseRegistry, LeaseState,
        OwnedTabSummary, TabId, TabLease, TabSnapshot,
    },
    managed_chrome::{BrowserLaunchMode, activate_managed_chrome, ensure_managed_chrome},
    protocol::{
        BROKER_PROTOCOL_VERSION, BrokerClient, BrokerRequest, BrokerResponse, BrokerStatus,
        ClaimTabParams, ClickParams, CloseTabResult, ConsoleMessage, DomainParams,
        ElementReferenceTarget, ElementTarget, EvaluateResult, FillFormParams, FillFormResult,
        FillParams, FormField, ListTabsParams, ListTabsResult, ListTabsScope, NavigationAction,
        NetworkEvent, NewTabParams, Observation, ObservationMode, PageActionEffect,
        PageActionEvidence, PageActionResult, ReleaseTabResult, ScreenshotImage, ScreenshotParams,
        ScreenshotResult, SnapshotMode, SnapshotParams, SnapshotResult, StartSessionParams,
        StartSessionResult, TabActionParams, TabResult, V3EvaluateParams, V3NavigateParams,
        V3PressKeyParams, V3TypeTextParams, WaitCondition, WaitForParams, WaitForResult,
    },
    semantic::{ElementReference, ElementReferenceRegistry, RawAxSnapshot, SnapshotBuildContext},
};

#[cfg(test)]
use crate::protocol::{
    ConsoleMessagesResult, DiagnosticsParams, EvaluateParams, NavigateParams, NetworkEventsResult,
    PressKeyParams, PressKeyResult, TypeTextParams, TypeTextResult,
};
#[cfg(test)]
use crate::semantic::{RawAxFrame, RawAxNode};

const BROKER_START_TIMEOUT: Duration = Duration::from_secs(5);
const BROKER_CONNECT_RETRY: Duration = Duration::from_millis(50);
const DEFAULT_NAVIGATION_TIMEOUT_MS: u64 = 15_000;
const DEFAULT_CLICK_TIMEOUT_MS: u64 = 5_000;
const DEFAULT_ELEMENT_TIMEOUT_MS: u64 = 5_000;
const DIAGNOSTICS_BUFFER_LIMIT: usize = 200;
const ACTION_EVIDENCE_NETWORK_EVENT_LIMIT: usize = 20;
const MAX_ANALYZABLE_TRACE_BYTES: u64 = 128 * 1024 * 1024;

#[derive(Clone)]
struct BrokerState {
    registry: Arc<Mutex<LeaseRegistry>>,
    diagnostics: Arc<Mutex<DiagnosticsRegistry>>,
    references: Arc<Mutex<ElementReferenceRegistry>>,
    artifacts: Arc<Mutex<ArtifactRegistry>>,
    traces: Arc<AsyncMutex<HashMap<String, TraceCapture>>>,
    screencasts: Arc<AsyncMutex<HashMap<String, ActiveScreencast>>>,
    viewport_overrides: Arc<Mutex<HashMap<String, serde_json::Map<String, Value>>>>,
    focused_target_id: Arc<Mutex<Option<String>>>,
    browser: BrowserBackend,
}

impl BrokerState {
    fn new(config: &RuntimeConfig) -> Result<Self> {
        Ok(Self {
            registry: Arc::new(Mutex::new(LeaseRegistry::new())),
            diagnostics: Arc::new(Mutex::new(DiagnosticsRegistry::new())),
            references: Arc::new(Mutex::new(ElementReferenceRegistry::new())),
            artifacts: Arc::new(Mutex::new(
                ArtifactRegistry::new(&config.state_dir)
                    .map_err(|error| anyhow::anyhow!(error.message))?,
            )),
            traces: Arc::new(AsyncMutex::new(HashMap::new())),
            screencasts: Arc::new(AsyncMutex::new(HashMap::new())),
            viewport_overrides: Arc::new(Mutex::new(HashMap::new())),
            focused_target_id: Arc::new(Mutex::new(None)),
            browser: BrowserBackend::new(config)?,
        })
    }

    #[cfg(test)]
    fn with_browser(browser: BrowserBackend) -> Self {
        let state_dir = std::env::temp_dir().join(format!("vbl-test-{}", uuid::Uuid::new_v4()));
        Self {
            registry: Arc::new(Mutex::new(LeaseRegistry::new())),
            diagnostics: Arc::new(Mutex::new(DiagnosticsRegistry::new())),
            references: Arc::new(Mutex::new(ElementReferenceRegistry::new())),
            artifacts: Arc::new(Mutex::new(ArtifactRegistry::new(&state_dir).unwrap())),
            traces: Arc::new(AsyncMutex::new(HashMap::new())),
            screencasts: Arc::new(AsyncMutex::new(HashMap::new())),
            viewport_overrides: Arc::new(Mutex::new(HashMap::new())),
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

    fn references(&self) -> &Mutex<ElementReferenceRegistry> {
        &self.references
    }

    fn artifacts(&self) -> &Mutex<ArtifactRegistry> {
        &self.artifacts
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
        self.monitors
            .get(target_id)
            .map(|monitor| !monitor.is_finished())
            .unwrap_or_else(|| self.monitored_targets.contains(target_id))
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
        target.last_network_event_at = Some(Instant::now());
        target.network.push_back(event);
        truncate_front(&mut target.network);
    }

    fn network_is_idle(&self, target_id: &str, quiet_for: Duration) -> bool {
        let Some(target) = self.targets.get(target_id) else {
            return false;
        };
        let records = network_records(target.network.iter().cloned().collect());
        let active = records
            .iter()
            .any(|record| !record.failed && record.finished_at_ms.is_none());
        !active
            && target
                .last_network_event_at
                .is_some_and(|last| last.elapsed() >= quiet_for)
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

    fn clear_console(&mut self, target_id: &str) {
        if let Some(target) = self.targets.get_mut(target_id) {
            target.console.clear();
        }
    }

    fn clear_network(&mut self, target_id: &str) {
        if let Some(target) = self.targets.get_mut(target_id) {
            target.network.clear();
        }
    }
}

#[derive(Default)]
struct TargetDiagnostics {
    console: VecDeque<ConsoleMessage>,
    network: VecDeque<NetworkEvent>,
    last_network_event_at: Option<Instant>,
}

enum TraceCapture {
    Real(CdpTraceCapture),
    #[cfg(test)]
    Fake(Vec<Value>),
}

enum ScreencastCapture {
    Real(CdpScreencastCapture),
    #[cfg(test)]
    Fake(Vec<CapturedScreencastFrame>),
}

struct ActiveScreencast {
    capture: ScreencastCapture,
    started_at_ms: u64,
    fps: u32,
    quality: u8,
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
    prepared_targets: Vec<String>,
    closed_targets: Vec<String>,
    clicked_selectors: Vec<String>,
    clicked_backend_nodes: Vec<i64>,
    semantic_activated_backend_nodes: Vec<i64>,
    filled_backend_nodes: Vec<(i64, String)>,
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
            prepared_targets: Vec::new(),
            closed_targets: Vec::new(),
            clicked_selectors: Vec::new(),
            clicked_backend_nodes: Vec::new(),
            semantic_activated_backend_nodes: Vec::new(),
            filled_backend_nodes: Vec::new(),
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

    fn has_focus(&self, target_id: &str) -> Result<bool, BrowserToolError> {
        if !self.targets.iter().any(|target| target.id == target_id) {
            return Err(BrowserToolError::target_missing_for_target(target_id));
        }

        Ok(self.focused_target_id.as_deref() == Some(target_id))
    }

    fn prepare_target_for_action(&mut self, target_id: &str) -> Result<(), BrowserToolError> {
        if self.targets.iter().any(|target| target.id == target_id) {
            self.prepared_targets.push(target_id.to_string());
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

    fn click(&mut self, target: &CdpTarget, selector: &str) -> Result<Value, BrowserToolError> {
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
        Ok(json!({
            "state":"ready",
            "selector":selector,
            "resolved_element":{"selector":selector},
            "center_hit_test":{"topmost":"fake","target_contains_topmost":true},
            "submit_candidate":selector == "#submit",
            "dispatch":{"release_delivery":"chrome_ack","delivery_uncertain":false}
        }))
    }

    fn document_revision(&self, target: &CdpTarget) -> Result<String, BrowserToolError> {
        self.targets
            .iter()
            .find(|candidate| candidate.id == target.id)
            .map(|target| format!("loader:{}", target.url))
            .ok_or_else(|| BrowserToolError::target_missing_for_target(&target.id))
    }

    fn accessibility_snapshot(
        &self,
        target: &CdpTarget,
    ) -> Result<RawAxSnapshot, BrowserToolError> {
        let target = self
            .targets
            .iter()
            .find(|candidate| candidate.id == target.id)
            .ok_or_else(|| BrowserToolError::target_missing_for_target(&target.id))?;
        Ok(RawAxSnapshot {
            title: target.title.clone(),
            url: target.url.clone(),
            frames: vec![RawAxFrame {
                frame_id: "frame-main".to_string(),
                parent_frame_id: None,
                loader_id: format!("loader:{}", target.url),
                url: target.url.clone(),
                nodes: vec![
                    RawAxNode {
                        node_id: "root".to_string(),
                        parent_id: None,
                        child_ids: vec!["submit".to_string(), "email".to_string()],
                        backend_node_id: Some(1),
                        frame_id: "frame-main".to_string(),
                        role: "WebArea".to_string(),
                        name: target.title.clone(),
                        value: None,
                        properties: Vec::new(),
                        ignored: false,
                        bounds: Some("0.0,0.0,800.0,600.0".to_string()),
                    },
                    RawAxNode {
                        node_id: "submit".to_string(),
                        parent_id: Some("root".to_string()),
                        child_ids: Vec::new(),
                        backend_node_id: Some(2),
                        frame_id: "frame-main".to_string(),
                        role: "button".to_string(),
                        name: "Submit".to_string(),
                        value: None,
                        properties: Vec::new(),
                        ignored: false,
                        bounds: Some("20.0,20.0,80.0,30.0".to_string()),
                    },
                    RawAxNode {
                        node_id: "email".to_string(),
                        parent_id: Some("root".to_string()),
                        child_ids: Vec::new(),
                        backend_node_id: Some(3),
                        frame_id: "frame-main".to_string(),
                        role: "textbox".to_string(),
                        name: "Email".to_string(),
                        value: None,
                        properties: Vec::new(),
                        ignored: false,
                        bounds: Some("20.0,60.0,200.0,30.0".to_string()),
                    },
                ],
            }],
        })
    }

    fn click_backend_node(
        &mut self,
        target: &CdpTarget,
        backend_node_id: i64,
    ) -> Result<Value, BrowserToolError> {
        if !self
            .targets
            .iter()
            .any(|candidate| candidate.id == target.id)
        {
            return Err(BrowserToolError::target_missing_for_target(&target.id));
        }
        self.clicked_backend_nodes.push(backend_node_id);
        let submit_candidate = backend_node_id == 2;
        Ok(json!({
            "state":"ready",
            "resolved_element":{"backend_node_id":backend_node_id,"role": if backend_node_id == 2 { "button" } else { "unknown" }},
            "center_hit_test":{"topmost":"fake","target_contains_topmost":true},
            "submit_candidate":submit_candidate,
            "dispatch":{"release_delivery":"chrome_ack","delivery_uncertain":false}
        }))
    }

    fn semantic_activate_backend_node(
        &mut self,
        target: &CdpTarget,
        backend_node_id: i64,
    ) -> Result<Value, BrowserToolError> {
        let Some(existing) = self
            .targets
            .iter_mut()
            .find(|candidate| candidate.id == target.id)
        else {
            return Err(BrowserToolError::target_missing_for_target(&target.id));
        };
        self.semantic_activated_backend_nodes.push(backend_node_id);
        existing.url = "fake://semantic-submit".to_string();
        existing.title = "fake://semantic-submit".to_string();
        Ok(json!({
            "state":"ready",
            "semantic_activation":"form_request_submit",
            "submit_candidate":true,
            "resolved_element":{"backend_node_id":backend_node_id}
        }))
    }

    fn fill_backend_node(
        &mut self,
        target: &CdpTarget,
        backend_node_id: i64,
        value: &str,
    ) -> Result<(), BrowserToolError> {
        if !self
            .targets
            .iter()
            .any(|candidate| candidate.id == target.id)
        {
            return Err(BrowserToolError::target_missing_for_target(&target.id));
        }
        self.filled_backend_nodes
            .push((backend_node_id, value.to_string()));
        Ok(())
    }

    fn fill_css(
        &mut self,
        target: &CdpTarget,
        selector: &str,
        value: &str,
    ) -> Result<(), BrowserToolError> {
        if !self
            .targets
            .iter()
            .any(|candidate| candidate.id == target.id)
        {
            return Err(BrowserToolError::target_missing_for_target(&target.id));
        }
        if selector == "#missing" {
            return Err(BrowserToolError::element_not_found(selector));
        }
        self.filled_backend_nodes.push((0, value.to_string()));
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

    fn typed_text(&self) -> &[String] {
        &self.typed_text
    }

    fn pressed_keys(&self) -> &[String] {
        &self.pressed_keys
    }
}

#[derive(Clone)]
enum BrowserBackend {
    External(CdpClient),
    Managed(Arc<ManagedBrowserBackend>),
    #[cfg(test)]
    Fake(Arc<Mutex<FakeBrowser>>),
}

#[derive(Clone)]
struct ManagedBrowserBackend {
    config: RuntimeConfig,
    client: Arc<AsyncMutex<Option<(String, CdpClient)>>>,
}

impl ManagedBrowserBackend {
    fn new(config: RuntimeConfig) -> Self {
        Self {
            config,
            client: Arc::new(AsyncMutex::new(None)),
        }
    }

    async fn client(&self) -> Result<CdpClient, BrowserToolError> {
        let managed = ensure_managed_chrome(&self.config, BrowserLaunchMode::Visible)
            .await
            .map_err(|error| BrowserToolError::chrome_unavailable(error.to_string()))?;
        let mut client = self.client.lock().await;
        if client
            .as_ref()
            .is_none_or(|(endpoint, _)| endpoint != &managed.cdp_endpoint)
        {
            let cdp = CdpClient::new(&managed.cdp_endpoint)
                .map_err(|error| BrowserToolError::chrome_unavailable(error.to_string()))?;
            *client = Some((managed.cdp_endpoint, cdp));
        }
        Ok(client
            .as_ref()
            .expect("managed CDP client was initialized")
            .1
            .clone())
    }
}

impl BrowserBackend {
    fn new(config: &RuntimeConfig) -> Result<Self> {
        match config.runtime_mode {
            RuntimeMode::External => {
                let endpoint = config
                    .cdp_endpoint
                    .as_deref()
                    .context("external runtime omitted its CDP endpoint")?;
                Ok(Self::External(CdpClient::new(endpoint)?))
            }
            RuntimeMode::Managed => Ok(Self::Managed(Arc::new(ManagedBrowserBackend::new(
                config.clone(),
            )))),
        }
    }

    async fn cdp_client(&self) -> Result<CdpClient, BrowserToolError> {
        match self {
            Self::External(client) => Ok(client.clone()),
            Self::Managed(browser) => browser.client().await,
            #[cfg(test)]
            Self::Fake(_) => unreachable!("fake browser does not expose a CDP client"),
        }
    }

    async fn resolved_endpoint(&self) -> Result<String, BrowserToolError> {
        match self {
            Self::External(_) => Ok(normalized_endpoint(&self.cdp_client().await?)),
            Self::Managed(browser) => browser
                .client()
                .await
                .map(|client| normalized_endpoint(&client)),
            #[cfg(test)]
            Self::Fake(_) => Ok("fake://browser".to_string()),
        }
    }

    async fn page_targets(&self) -> Result<Vec<CdpTarget>, BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(browser) => Ok(browser.lock().unwrap().page_targets()),
            _ => self.cdp_client().await?.page_targets().await,
        }
    }

    async fn create_page(
        &self,
        url: Option<&str>,
        focus: bool,
    ) -> Result<CdpTarget, BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(browser) => Ok(browser.lock().unwrap().create_page(url, focus)),
            Self::External(client) => client.create_page(url, focus).await,
            Self::Managed(browser) => {
                let client = browser.client().await?;
                let target = client.create_page(url, false).await?;
                if focus {
                    client.activate_target(&target.id).await?;
                    activate_managed_chrome_if_available(&browser.config);
                }
                Ok(target)
            }
        }
    }

    async fn activate_target(&self, target_id: &str) -> Result<(), BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(browser) => browser.lock().unwrap().activate_target(target_id),
            Self::External(client) => client.activate_target(target_id).await,
            Self::Managed(browser) => {
                browser.client().await?.activate_target(target_id).await?;
                activate_managed_chrome_if_available(&browser.config);
                Ok(())
            }
        }
    }

    async fn prepare_target_for_action(&self, target: &CdpTarget) -> Result<(), BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(browser) => browser
                .lock()
                .unwrap()
                .prepare_target_for_action(&target.id),
            Self::External(client) => client.prepare_target_for_action(target).await,
            Self::Managed(browser) => {
                browser
                    .client()
                    .await?
                    .prepare_target_for_action(target)
                    .await
            }
        }
    }

    async fn close_target(&self, target_id: &str) -> Result<(), BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(browser) => browser.lock().unwrap().close_target(target_id),
            _ => self.cdp_client().await?.close_target(target_id).await,
        }
    }

    async fn has_focus(&self, target: &CdpTarget) -> Result<bool, BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(browser) => browser.lock().unwrap().has_focus(&target.id),
            _ => self.cdp_client().await?.has_focus(target).await,
        }
    }

    async fn navigate(
        &self,
        target: &CdpTarget,
        url: &str,
        wait_until: Option<&str>,
        timeout_ms: u64,
        before_unload: Option<&str>,
    ) -> Result<CdpTarget, BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(browser) => browser.lock().unwrap().navigate(target, url),
            _ => {
                let client = self.cdp_client().await?;
                client
                    .navigate(target, url, wait_until, timeout_ms, before_unload)
                    .await?;
                client.page_target(&target.id).await
            }
        }
    }

    async fn add_init_script(
        &self,
        target: &CdpTarget,
        source: &str,
    ) -> Result<Option<String>, BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(_) => Ok(Some("fake-init-script".to_string())),
            _ => self
                .cdp_client()
                .await?
                .add_init_script(target, source)
                .await
                .map(Some),
        }
    }

    async fn remove_init_script(
        &self,
        target: &CdpTarget,
        identifier: Option<String>,
    ) -> Result<(), BrowserToolError> {
        let Some(identifier) = identifier else {
            return Ok(());
        };
        match self {
            #[cfg(test)]
            Self::Fake(_) => Ok(()),
            _ => {
                self.cdp_client()
                    .await?
                    .remove_init_script(target, identifier)
                    .await
            }
        }
    }

    async fn navigate_history(
        &self,
        target: &CdpTarget,
        direction: i64,
        wait_until: Option<&str>,
        timeout_ms: u64,
        before_unload: Option<&str>,
    ) -> Result<CdpTarget, BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(browser) => browser.lock().unwrap().navigate(target, &target.url),
            _ => {
                let client = self.cdp_client().await?;
                client
                    .navigate_history(target, direction, wait_until, timeout_ms, before_unload)
                    .await?;
                client.page_target(&target.id).await
            }
        }
    }

    async fn reload(
        &self,
        target: &CdpTarget,
        ignore_cache: bool,
        wait_until: Option<&str>,
        timeout_ms: u64,
        before_unload: Option<&str>,
    ) -> Result<CdpTarget, BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(browser) => browser.lock().unwrap().navigate(target, &target.url),
            _ => {
                let client = self.cdp_client().await?;
                client
                    .reload(target, ignore_cache, wait_until, timeout_ms, before_unload)
                    .await?;
                client.page_target(&target.id).await
            }
        }
    }

    async fn screenshot(
        &self,
        target: &CdpTarget,
        full_page: bool,
        format: &str,
        quality: Option<u8>,
    ) -> Result<String, BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(browser) => browser.lock().unwrap().screenshot(target, full_page),
            _ => {
                self.cdp_client()
                    .await?
                    .screenshot(target, full_page, format, quality, None)
                    .await
            }
        }
    }

    async fn screenshot_element(
        &self,
        target: &CdpTarget,
        element: ResolvedElementTarget,
        format: &str,
        quality: Option<u8>,
    ) -> Result<String, BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(browser) => browser.lock().unwrap().screenshot(target, false),
            _ => {
                let client = self.cdp_client().await?;
                match element {
                    ResolvedElementTarget::Reference(element) => {
                        client
                            .screenshot_backend_node(
                                target,
                                element.backend_node_id,
                                format,
                                quality,
                            )
                            .await
                    }
                    ResolvedElementTarget::Css(selector) => {
                        client
                            .screenshot_css(target, &selector, format, quality)
                            .await
                    }
                }
            }
        }
    }

    async fn evaluate(
        &self,
        target: &CdpTarget,
        expression: &str,
    ) -> Result<EvaluateResult, BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(browser) => browser.lock().unwrap().evaluate(target, expression),
            _ => self.cdp_client().await?.evaluate(target, expression).await,
        }
    }

    async fn evaluate_on_target(
        &self,
        target: &CdpTarget,
        element: ResolvedElementTarget,
        source: &str,
        mode: &str,
        args: &[Value],
        await_promise: bool,
    ) -> Result<EvaluateResult, BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(browser) => browser.lock().unwrap().evaluate(target, source),
            _ => {
                let client = self.cdp_client().await?;
                match element {
                    ResolvedElementTarget::Reference(element) => {
                        client
                            .evaluate_on_backend_node(
                                target,
                                element.backend_node_id,
                                ElementEvaluation {
                                    source,
                                    mode,
                                    args,
                                    await_promise,
                                },
                            )
                            .await
                    }
                    ResolvedElementTarget::Css(selector) => {
                        client
                            .evaluate_on_css(
                                target,
                                &selector,
                                ElementEvaluation {
                                    source,
                                    mode,
                                    args,
                                    await_promise,
                                },
                            )
                            .await
                    }
                }
            }
        }
    }

    async fn resolve_frame_css_backend_node(
        &self,
        target: &CdpTarget,
        frame_backend_node_id: i64,
        selector: &str,
    ) -> Result<i64, BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(_) => Ok(frame_backend_node_id),
            _ => {
                self.cdp_client()
                    .await?
                    .resolve_frame_css_backend_node(target, frame_backend_node_id, selector)
                    .await
            }
        }
    }

    async fn resolve_css_backend_node(
        &self,
        target: &CdpTarget,
        selector: &str,
    ) -> Result<i64, BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(_) => match selector {
                "#submit" => Ok(2),
                "#email" => Ok(3),
                _ => Err(BrowserToolError::element_not_found(selector)),
            },
            _ => {
                self.cdp_client()
                    .await?
                    .resolve_css_backend_node(target, selector)
                    .await
            }
        }
    }

    async fn document_revision(&self, target: &CdpTarget) -> Result<String, BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(browser) => browser.lock().unwrap().document_revision(target),
            _ => self.cdp_client().await?.document_revision(target).await,
        }
    }

    async fn accessibility_snapshot(
        &self,
        target: &CdpTarget,
        depth: Option<usize>,
        include_bounds: bool,
    ) -> Result<RawAxSnapshot, BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(browser) => browser.lock().unwrap().accessibility_snapshot(target),
            _ => {
                self.cdp_client()
                    .await?
                    .accessibility_snapshot(target, depth, include_bounds)
                    .await
            }
        }
    }

    async fn click(
        &self,
        target: &CdpTarget,
        selector: &str,
        timeout_ms: u64,
        button: &str,
        count: u8,
        modifiers: &[String],
    ) -> Result<Value, BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(browser) => browser.lock().unwrap().click(target, selector),
            _ => {
                self.cdp_client()
                    .await?
                    .click(target, selector, timeout_ms, button, count, modifiers)
                    .await
            }
        }
    }

    async fn click_backend_node(
        &self,
        target: &CdpTarget,
        backend_node_id: i64,
        button: &str,
        count: u8,
        modifiers: &[String],
    ) -> Result<Value, BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(browser) => browser
                .lock()
                .unwrap()
                .click_backend_node(target, backend_node_id),
            _ => {
                self.cdp_client()
                    .await?
                    .click_backend_node(target, backend_node_id, button, count, modifiers)
                    .await
            }
        }
    }

    async fn semantic_activate_backend_node(
        &self,
        target: &CdpTarget,
        backend_node_id: i64,
    ) -> Result<Value, BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(browser) => browser
                .lock()
                .unwrap()
                .semantic_activate_backend_node(target, backend_node_id),
            _ => {
                self.cdp_client()
                    .await?
                    .semantic_activate_backend_node(target, backend_node_id)
                    .await
            }
        }
    }

    async fn fill_backend_node(
        &self,
        target: &CdpTarget,
        backend_node_id: i64,
        value: &str,
    ) -> Result<(), BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(browser) => {
                browser
                    .lock()
                    .unwrap()
                    .fill_backend_node(target, backend_node_id, value)
            }
            _ => {
                self.cdp_client()
                    .await?
                    .fill_backend_node(target, backend_node_id, value)
                    .await
            }
        }
    }

    async fn fill_css(
        &self,
        target: &CdpTarget,
        selector: &str,
        value: &str,
    ) -> Result<(), BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(browser) => browser.lock().unwrap().fill_css(target, selector, value),
            _ => {
                self.cdp_client()
                    .await?
                    .fill_css(target, selector, value)
                    .await
            }
        }
    }

    async fn type_text_backend_node(
        &self,
        target: &CdpTarget,
        backend_node_id: i64,
        text: &str,
    ) -> Result<(), BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(browser) => browser.lock().unwrap().type_text(target, text),
            _ => {
                self.cdp_client()
                    .await?
                    .type_text_backend_node(target, backend_node_id, text)
                    .await
            }
        }
    }

    async fn type_text_css(
        &self,
        target: &CdpTarget,
        selector: &str,
        text: &str,
    ) -> Result<(), BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(browser) => browser.lock().unwrap().type_text(target, text),
            _ => {
                self.cdp_client()
                    .await?
                    .type_text_css(target, selector, text)
                    .await
            }
        }
    }

    async fn select_backend_node(
        &self,
        target: &CdpTarget,
        backend_node_id: i64,
        values: &[String],
    ) -> Result<(), BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(_) => Ok(()),
            _ => {
                self.cdp_client()
                    .await?
                    .select_backend_node(target, backend_node_id, values)
                    .await
            }
        }
    }

    async fn select_css(
        &self,
        target: &CdpTarget,
        selector: &str,
        values: &[String],
    ) -> Result<(), BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(_) => Ok(()),
            _ => {
                self.cdp_client()
                    .await?
                    .select_css(target, selector, values)
                    .await
            }
        }
    }

    async fn set_checked_backend_node(
        &self,
        target: &CdpTarget,
        backend_node_id: i64,
        checked: bool,
    ) -> Result<(), BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(_) => Ok(()),
            _ => {
                self.cdp_client()
                    .await?
                    .set_checked_backend_node(target, backend_node_id, checked)
                    .await
            }
        }
    }

    async fn set_checked_css(
        &self,
        target: &CdpTarget,
        selector: &str,
        checked: bool,
    ) -> Result<(), BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(_) => Ok(()),
            _ => {
                self.cdp_client()
                    .await?
                    .set_checked_css(target, selector, checked)
                    .await
            }
        }
    }

    async fn element_state_backend_node(
        &self,
        target: &CdpTarget,
        backend_node_id: i64,
    ) -> Result<serde_json::Value, BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(_) => Ok(serde_json::json!({
                "attached": true,
                "visible": true,
                "enabled": true,
                "editable": true,
                "checked": false
            })),
            _ => {
                self.cdp_client()
                    .await?
                    .element_state_backend_node(target, backend_node_id)
                    .await
            }
        }
    }

    async fn hover_backend_node(
        &self,
        target: &CdpTarget,
        backend_node_id: i64,
    ) -> Result<(), BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(_) => Ok(()),
            _ => {
                self.cdp_client()
                    .await?
                    .hover_backend_node(target, backend_node_id)
                    .await
            }
        }
    }

    async fn drag_backend_nodes(
        &self,
        target: &CdpTarget,
        source_backend_node_id: i64,
        destination_backend_node_id: i64,
    ) -> Result<(), BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(_) => Ok(()),
            _ => {
                self.cdp_client()
                    .await?
                    .drag_backend_nodes(target, source_backend_node_id, destination_backend_node_id)
                    .await
            }
        }
    }

    async fn click_at(
        &self,
        target: &CdpTarget,
        x: f64,
        y: f64,
        button: &str,
        count: i64,
        modifiers: &[String],
    ) -> Result<(), BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(_) => Ok(()),
            _ => {
                self.cdp_client()
                    .await?
                    .click_at(target, x, y, button, count, modifiers)
                    .await
            }
        }
    }

    async fn scroll_backend_node(
        &self,
        target: &CdpTarget,
        backend_node_id: i64,
        delta_x: f64,
        delta_y: f64,
    ) -> Result<(), BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(_) => Ok(()),
            _ => {
                self.cdp_client()
                    .await?
                    .scroll_backend_node(target, backend_node_id, delta_x, delta_y)
                    .await
            }
        }
    }

    async fn upload_files_backend_node(
        &self,
        target: &CdpTarget,
        backend_node_id: i64,
        paths: &[String],
    ) -> Result<(), BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(_) => Ok(()),
            _ => {
                self.cdp_client()
                    .await?
                    .upload_files_backend_node(target, backend_node_id, paths)
                    .await
            }
        }
    }

    async fn drop_data_backend_node(
        &self,
        target: &CdpTarget,
        backend_node_id: i64,
        files: &Value,
        data: &Value,
    ) -> Result<(), BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(_) => Ok(()),
            _ => {
                self.cdp_client()
                    .await?
                    .drop_data_backend_node(target, backend_node_id, files, data)
                    .await
            }
        }
    }

    async fn handle_dialog(
        &self,
        target: &CdpTarget,
        accept: bool,
        prompt_text: Option<&str>,
    ) -> Result<(), BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(_) => Ok(()),
            _ => {
                self.cdp_client()
                    .await?
                    .handle_dialog(target, accept, prompt_text)
                    .await
            }
        }
    }

    async fn type_text(&self, target: &CdpTarget, text: &str) -> Result<(), BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(browser) => browser.lock().unwrap().type_text(target, text),
            _ => self.cdp_client().await?.type_text(target, text).await,
        }
    }

    async fn press_key(
        &self,
        target: &CdpTarget,
        key: &str,
        modifiers: &[String],
    ) -> Result<(), BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(browser) => browser.lock().unwrap().press_key(target, key, modifiers),
            _ => {
                self.cdp_client()
                    .await?
                    .press_key(target, key, modifiers)
                    .await
            }
        }
    }

    async fn diagnostics_monitor(
        &self,
        target: &CdpTarget,
        sink: Arc<dyn Fn(CdpDiagnosticEvent) + Send + Sync>,
    ) -> Result<Option<CdpDiagnosticsMonitor>, BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(_) => Ok(None),
            _ => Ok(Some(
                self.cdp_client()
                    .await?
                    .diagnostics_monitor(target, sink)
                    .await?,
            )),
        }
    }

    async fn network_response_body(
        &self,
        target: &CdpTarget,
        request_id: &str,
    ) -> Result<Vec<u8>, BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(_) => Ok(b"fake response body".to_vec()),
            _ => {
                self.cdp_client()
                    .await?
                    .network_response_body(target, request_id)
                    .await
            }
        }
    }

    async fn network_request_body(
        &self,
        target: &CdpTarget,
        request_id: &str,
    ) -> Result<Option<String>, BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(_) => Ok(None),
            _ => {
                self.cdp_client()
                    .await?
                    .network_request_body(target, request_id)
                    .await
            }
        }
    }

    async fn emulate(
        &self,
        target: &CdpTarget,
        operation: &str,
        arguments: &serde_json::Map<String, Value>,
    ) -> Result<Value, BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(_) => Ok(Value::Object(arguments.clone())),
            _ => {
                self.cdp_client()
                    .await?
                    .emulate(target, operation, arguments)
                    .await
            }
        }
    }

    async fn start_trace(
        &self,
        categories: Vec<String>,
        screenshots: bool,
    ) -> Result<TraceCapture, BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(_) => Ok(TraceCapture::Fake(vec![json!({
                "name":"RunTask","cat":"devtools.timeline","ph":"X","ts":1000,"dur":60000
            })])),
            _ => Ok(TraceCapture::Real(
                self.cdp_client()
                    .await?
                    .start_trace(categories, screenshots)
                    .await?,
            )),
        }
    }

    async fn stop_trace(capture: TraceCapture) -> Result<Vec<Value>, BrowserToolError> {
        match capture {
            TraceCapture::Real(capture) => CdpClient::stop_trace(capture).await,
            #[cfg(test)]
            TraceCapture::Fake(events) => Ok(events),
        }
    }

    async fn heap_snapshot(&self, target: &CdpTarget) -> Result<Vec<u8>, BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(_) => Ok(br#"{"snapshot":{"meta":{"node_fields":["type","name","id","self_size","edge_count","trace_node_id","detachedness"],"node_types":[["hidden","array","string","object","code","closure","regexp","number","native","synthetic","concatenated string","sliced string","symbol","bigint"],"string","number","number","number","number","number"],"edge_fields":["type","name_or_index","to_node"],"edge_types":[["context","element","property","internal","hidden","shortcut","weak"],"string_or_number","node"]},"node_count":1,"edge_count":0},"nodes":[9,0,1,0,0,0,0],"edges":[],"strings":["(root)"]}"#.to_vec()),
            _ => self.cdp_client().await?.heap_snapshot(target).await,
        }
    }

    async fn start_screencast(
        &self,
        target: &CdpTarget,
        fps: u32,
        quality: u8,
        max_duration: Duration,
    ) -> Result<ScreencastCapture, BrowserToolError> {
        match self {
            #[cfg(test)]
            Self::Fake(_) => Ok(ScreencastCapture::Fake(Vec::new())),
            _ => Ok(ScreencastCapture::Real(
                self.cdp_client()
                    .await?
                    .start_screencast(target, fps, quality, max_duration)
                    .await?,
            )),
        }
    }

    async fn stop_screencast(
        capture: ScreencastCapture,
    ) -> Result<Vec<CapturedScreencastFrame>, BrowserToolError> {
        match capture {
            ScreencastCapture::Real(capture) => CdpClient::stop_screencast(capture).await,
            #[cfg(test)]
            ScreencastCapture::Fake(frames) => Ok(frames),
        }
    }
}

fn normalized_endpoint(client: &CdpClient) -> String {
    client.endpoint().as_str().trim_end_matches('/').to_string()
}

fn activate_managed_chrome_if_available(config: &RuntimeConfig) {
    if let Err(error) = activate_managed_chrome(config) {
        tracing::warn!(
            error = %error,
            "managed Chrome window activation did not complete"
        );
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
        runtime_mode = ?config.runtime_mode,
        cdp_endpoint = ?config.cdp_endpoint,
        ipc_endpoint = %endpoint.display(),
        state_dir = %config.state_dir.display(),
        "visible browser broker listening"
    );

    serve(config, listener).await
}

pub async fn ensure_running(config: &RuntimeConfig) -> Result<BrokerClient> {
    prepare_state(config).await?;

    if let Ok(BrokerProbe::Compatible(client)) = probe_broker(config).await {
        return Ok(client);
    }

    let deadline = Instant::now() + BROKER_START_TIMEOUT;

    loop {
        if let Some(_lock) = BrokerStartLock::try_acquire(&config.lock_path)? {
            match probe_broker(config).await {
                Ok(BrokerProbe::Compatible(client)) => return Ok(client),
                Ok(BrokerProbe::Incompatible { status, message }) => {
                    restart_incompatible_broker(config, &status, &message).await?;
                }
                Err(_) => {}
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
    match probe_broker(config).await? {
        BrokerProbe::Compatible(client) => Ok(client),
        BrokerProbe::Incompatible { message, .. } => bail!("{message}"),
    }
}

enum BrokerProbe {
    Compatible(BrokerClient),
    Incompatible {
        status: BrokerStatus,
        message: String,
    },
}

async fn probe_broker(config: &RuntimeConfig) -> Result<BrokerProbe> {
    let endpoint = broker_endpoint(config)?;
    let mut client = BrokerClient::connect(&endpoint).await?;
    let status = client.ping().await?;
    if let Some(message) = broker_status_mismatch(config, &status)? {
        return Ok(BrokerProbe::Incompatible { status, message });
    }
    Ok(BrokerProbe::Compatible(client))
}

fn broker_status_mismatch(config: &RuntimeConfig, status: &BrokerStatus) -> Result<Option<String>> {
    if status.protocol_version != BROKER_PROTOCOL_VERSION {
        return Ok(Some(format!(
            "broker protocol mismatch: expected {}, got {}",
            BROKER_PROTOCOL_VERSION, status.protocol_version
        )));
    }
    if status.runtime_mode != config.runtime_mode {
        return Ok(Some(format!(
            "broker runtime mismatch: requested {:?}, existing broker is {:?} at {}",
            config.runtime_mode, status.runtime_mode, status.ipc_endpoint
        )));
    }
    if config.runtime_mode == RuntimeMode::External {
        let expected = config
            .cdp_endpoint
            .as_deref()
            .context("external runtime omitted its CDP endpoint")?;
        if status.cdp_endpoint != expected {
            return Ok(Some(format!(
                "broker CDP endpoint mismatch: requested {expected}, existing broker uses {} at {}",
                status.cdp_endpoint, status.ipc_endpoint
            )));
        }
    }
    Ok(None)
}

async fn restart_incompatible_broker(
    config: &RuntimeConfig,
    status: &BrokerStatus,
    message: &str,
) -> Result<()> {
    tracing::warn!(
        pid = status.pid,
        runtime_mode = ?status.runtime_mode,
        cdp_endpoint = %status.cdp_endpoint,
        ipc_endpoint = %status.ipc_endpoint,
        reason = %message,
        "restarting incompatible visible browser broker"
    );
    terminate_process(status.pid).await?;
    let endpoint = broker_endpoint(config)?;
    if let Some(stale_path) = endpoint.stale_path() {
        let _ = fs::remove_file(stale_path);
    }
    let _ = fs::remove_file(&config.pid_path);
    Ok(())
}

async fn wait_for_broker(config: &RuntimeConfig, timeout: Duration) -> Result<BrokerClient> {
    let deadline = Instant::now() + timeout;

    loop {
        match connect_and_ping(config).await {
            Ok(client) => return Ok(client),
            Err(error) if Instant::now() >= deadline => {
                let diagnostics = fs::read_to_string(config.log_dir.join("broker.stderr.log"))
                    .unwrap_or_else(|read_error| {
                        format!("failed to read broker diagnostics: {read_error}")
                    });
                let connection_error = format!("{error:#}");
                return Err(error).with_context(|| {
                    format!(
                        "timed out waiting for broker socket `{}`; last connection error: {connection_error}; broker diagnostics: {}",
                        config.ipc_endpoint,
                        diagnostics.trim()
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

    let mut command = Command::new(current_exe);
    command
        .arg("broker")
        .arg("--socket")
        .arg(&config.ipc_endpoint)
        .arg("--state-dir")
        .arg(&config.state_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    if let Some(cdp_endpoint) = &config.cdp_endpoint {
        command.arg("--cdp-endpoint").arg(cdp_endpoint);
    }
    if let Some(chrome_path) = &config.chrome_path {
        command.env(CHROME_PATH_ENV, chrome_path);
    }
    let child = command
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
        "ping" => broker_response(request.id, broker_status(config, state).await),
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
            broker_navigate_v3(state, parse_params(request.params)).await,
        ),
        "wait_for" => broker_response(
            request.id,
            broker_wait_for(state, parse_params(request.params)).await,
        ),
        "screenshot" => broker_response(
            request.id,
            broker_screenshot(state, parse_params(request.params)).await,
        ),
        "evaluate" => broker_response(
            request.id,
            broker_evaluate_v3(state, parse_params(request.params)).await,
        ),
        "snapshot" => broker_response(
            request.id,
            broker_snapshot(state, parse_params(request.params)).await,
        ),
        "click" => broker_response(
            request.id,
            broker_click(state, parse_params(request.params)).await,
        ),
        "fill" => broker_response(
            request.id,
            broker_fill(state, parse_params(request.params)).await,
        ),
        "fill_form" => broker_response(
            request.id,
            broker_fill_form(state, parse_params(request.params)).await,
        ),
        "type_text" => broker_response(
            request.id,
            broker_type_text_v3(state, parse_params(request.params)).await,
        ),
        "press_key" => broker_response(
            request.id,
            broker_press_key_v3(state, parse_params(request.params)).await,
        ),
        "interact" => broker_response(
            request.id,
            broker_interact(state, parse_params(request.params)).await,
        ),
        "console" => broker_response(
            request.id,
            broker_console(state, parse_params(request.params)).await,
        ),
        "network" => broker_response(
            request.id,
            broker_network(state, parse_params(request.params)).await,
        ),
        "emulation" => broker_response(
            request.id,
            broker_emulation(state, parse_params(request.params)).await,
        ),
        "performance" => broker_response(
            request.id,
            broker_performance(state, parse_params(request.params)).await,
        ),
        "audit" => broker_response(
            request.id,
            broker_audit(state, parse_params(request.params)).await,
        ),
        "memory" => broker_response(
            request.id,
            broker_memory(state, parse_params(request.params)).await,
        ),
        "screencast" => broker_response(
            request.id,
            broker_screencast(state, parse_params(request.params)).await,
        ),
        "artifacts" => broker_response(
            request.id,
            broker_artifacts(state, parse_params(request.params)).await,
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
        Ok(result) => match serde_json::to_value(result) {
            Ok(mut result) => {
                strip_null_fields(&mut result);
                BrokerResponse::success(id.clone(), result).unwrap_or_else(|error| {
                    BrokerResponse::error(
                        id,
                        BrowserToolError::invalid_input(format!(
                            "failed to serialize broker response: {error}"
                        )),
                    )
                })
            }
            Err(error) => BrokerResponse::error(
                id,
                BrowserToolError::invalid_input(format!(
                    "failed to serialize broker response: {error}"
                )),
            ),
        },
        Err(error) => BrokerResponse::error(id, error),
    }
}

fn strip_null_fields(value: &mut Value) {
    match value {
        Value::Object(object) => {
            object.retain(|_, value| !value.is_null());
            for value in object.values_mut() {
                strip_null_fields(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                strip_null_fields(value);
            }
        }
        _ => {}
    }
}

async fn broker_start_session(
    state: &BrokerState,
    params: Result<StartSessionParams, BrowserToolError>,
) -> Result<StartSessionResult, BrowserToolError> {
    let params = params?;
    let workspace_root = params
        .workspace_root
        .map(canonical_workspace_root)
        .transpose()?;
    let session = {
        let mut registry = state.registry().lock().unwrap();
        registry.start_session_with_workspace(params.label, workspace_root)
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

fn canonical_workspace_root(path: PathBuf) -> Result<PathBuf, BrowserToolError> {
    let original = path.display().to_string();
    let path = local_workspace_path(path)?;
    path.canonicalize().map_err(|error| {
        BrowserToolError::workspace_unavailable(format!(
            "workspace root `{original}` is unavailable: {error}"
        ))
    })
}

fn local_workspace_path(path: PathBuf) -> Result<PathBuf, BrowserToolError> {
    let raw = path.as_os_str().to_string_lossy();
    if !raw.starts_with("file:") {
        return Ok(path);
    }

    let url = Url::parse(raw.as_ref()).map_err(|error| {
        BrowserToolError::workspace_unavailable(format!(
            "workspace root `{raw}` is not a valid file URL: {error}"
        ))
    })?;
    url.to_file_path().map_err(|()| {
        BrowserToolError::workspace_unavailable(format!(
            "workspace root `{raw}` is not a local file URL"
        ))
    })
}

async fn broker_list_tabs(
    state: &BrokerState,
    params: Result<ListTabsParams, BrowserToolError>,
) -> Result<ListTabsResult, BrowserToolError> {
    let params = params?;
    let targets = state.browser.page_targets().await?;
    reconcile_missing_targets(state, &targets).await;
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
    if params.takeover {
        if let Some(capture) = state.traces.lock().await.remove(&target.id) {
            let _ = BrowserBackend::stop_trace(capture).await;
        }
        if let Some(active) = state.screencasts.lock().await.remove(&target.id) {
            let _ = BrowserBackend::stop_screencast(active.capture).await;
        }
        state
            .browser
            .emulate(&target, "reset", &serde_json::Map::new())
            .await?;
        state.viewport_overrides.lock().unwrap().remove(&target.id);
    }
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
    state.references().lock().unwrap().reset_target(&target.id);
    ensure_diagnostics_monitor(state, &target).await?;

    Ok(TabResult { tab })
}

async fn broker_release_tab(
    state: &BrokerState,
    params: Result<TabActionParams, BrowserToolError>,
) -> Result<ReleaseTabResult, BrowserToolError> {
    let params = params?;
    let active_target_id = state
        .registry()
        .lock()
        .unwrap()
        .owned_lease(&params.agent_session_id, &params.tab_id)?
        .target_id;
    if state
        .screencasts
        .lock()
        .await
        .contains_key(&active_target_id)
    {
        return Err(BrowserToolError::invalid_input(
            "stop the active screencast before releasing its tab",
        ));
    }
    if state.traces.lock().await.contains_key(&active_target_id) {
        return Err(BrowserToolError::invalid_input(
            "stop the active performance trace before releasing its tab",
        ));
    }
    let lease = state
        .registry()
        .lock()
        .unwrap()
        .release_tab(&params.agent_session_id, &params.tab_id)?;
    if let Ok(target) = target_by_id(state, &lease.target_id).await {
        state
            .browser
            .emulate(&target, "reset", &serde_json::Map::new())
            .await?;
        state.viewport_overrides.lock().unwrap().remove(&target.id);
    }
    state
        .diagnostics()
        .lock()
        .unwrap()
        .reset_target(&lease.target_id);
    state.references().lock().unwrap().reset_tab(&params.tab_id);
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
    let focus_deadline = Instant::now() + Duration::from_secs(2);
    while !state.browser.has_focus(&target).await? {
        if Instant::now() >= focus_deadline {
            return Err(BrowserToolError::focus_required(&params.tab_id));
        }
        sleep(Duration::from_millis(25)).await;
    }
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

#[cfg(test)]
async fn broker_navigate(
    state: &BrokerState,
    params: Result<NavigateParams, BrowserToolError>,
) -> Result<TabResult, BrowserToolError> {
    let params = params?;
    let target = active_owned_target(state, &params.agent_session_id, &params.tab_id).await?;
    ensure_diagnostics_monitor(state, &target).await?;
    let target = state
        .browser
        .navigate(
            &target,
            &params.url,
            params.wait_until.as_deref(),
            params.timeout_ms.unwrap_or(DEFAULT_NAVIGATION_TIMEOUT_MS),
            None,
        )
        .await?;
    state.references().lock().unwrap().reset_tab(&params.tab_id);
    let focused = state.is_focused_target(&target.id);
    let lease = state.registry().lock().unwrap().update_tab_snapshot(
        &params.tab_id,
        tab_snapshot(&target, focused.then_some(target.id.as_str())),
    )?;

    Ok(TabResult {
        tab: owned_summary(&lease, focused),
    })
}

async fn broker_navigate_v3(
    state: &BrokerState,
    params: Result<V3NavigateParams, BrowserToolError>,
) -> Result<PageActionResult, BrowserToolError> {
    let params = params?;
    let target = active_owned_target(state, &params.agent_session_id, &params.tab_id).await?;
    ensure_diagnostics_monitor(state, &target).await?;
    if matches!(params.action, NavigationAction::Url) && params.url.is_none() {
        return Err(BrowserToolError::invalid_input(
            "navigate action `url` requires `url`",
        ));
    }
    let init_script_id = if let Some(init_script) = params.init_script.as_deref() {
        state.browser.add_init_script(&target, init_script).await?
    } else {
        None
    };
    let timeout_ms = params.timeout_ms.unwrap_or(DEFAULT_NAVIGATION_TIMEOUT_MS);
    let navigation = match params.action {
        NavigationAction::Url => {
            let url = params.url.as_deref().expect("URL action validated above");
            state
                .browser
                .navigate(
                    &target,
                    url,
                    params.wait_until.as_deref(),
                    timeout_ms,
                    params.before_unload.as_deref(),
                )
                .await
        }
        NavigationAction::Back => {
            state
                .browser
                .navigate_history(
                    &target,
                    -1,
                    params.wait_until.as_deref(),
                    timeout_ms,
                    params.before_unload.as_deref(),
                )
                .await
        }
        NavigationAction::Forward => {
            state
                .browser
                .navigate_history(
                    &target,
                    1,
                    params.wait_until.as_deref(),
                    timeout_ms,
                    params.before_unload.as_deref(),
                )
                .await
        }
        NavigationAction::Reload => {
            state
                .browser
                .reload(
                    &target,
                    params.ignore_cache,
                    params.wait_until.as_deref(),
                    timeout_ms,
                    params.before_unload.as_deref(),
                )
                .await
        }
    };
    let remove_result = state
        .browser
        .remove_init_script(&target, init_script_id)
        .await;
    let target = navigation?;
    remove_result?;
    if params.wait_until.as_deref() == Some("network_idle") {
        wait_for_network_idle(state, &target, timeout_ms).await?;
    }
    state.references().lock().unwrap().reset_tab(&params.tab_id);
    update_owned_target_snapshot(state, &params.tab_id, &target)?;
    // Navigation establishes a new document and invalidates prior element
    // references, so the default observation is a full snapshot rather than a
    // diff against the previous document's tree.
    post_action_observation(
        state,
        &params.agent_session_id,
        &params.tab_id,
        &target,
        params.observe.unwrap_or(ObservationMode::Snapshot),
    )
    .await
}

async fn broker_wait_for(
    state: &BrokerState,
    params: Result<WaitForParams, BrowserToolError>,
) -> Result<WaitForResult, BrowserToolError> {
    let params = params?;
    let target = active_owned_target(state, &params.agent_session_id, &params.tab_id).await?;
    ensure_diagnostics_monitor(state, &target).await?;
    let started = Instant::now();
    let timeout_ms = params.timeout_ms.unwrap_or(5_000).clamp(1, 120_000);
    if let WaitCondition::Delay { duration_ms } = &params.condition {
        if *duration_ms > timeout_ms {
            return Err(BrowserToolError::operation_timeout(format!(
                "delay {duration_ms}ms exceeds timeout {timeout_ms}ms"
            )));
        }
        sleep(Duration::from_millis(*duration_ms)).await;
    } else {
        loop {
            if wait_condition_matches(
                state,
                &params.agent_session_id,
                &params.tab_id,
                &target,
                &params.condition,
            )
            .await?
            {
                break;
            }
            if started.elapsed() >= Duration::from_millis(timeout_ms) {
                return Err(BrowserToolError::operation_timeout(
                    "wait_for condition did not match before the timeout",
                ));
            }
            sleep(Duration::from_millis(50)).await;
        }
    }
    let target = target_by_id(state, &target.id).await?;
    let action = post_action_observation(
        state,
        &params.agent_session_id,
        &params.tab_id,
        &target,
        params.observe.unwrap_or_default(),
    )
    .await?;
    Ok(WaitForResult {
        matched: true,
        elapsed_ms: started.elapsed().as_millis() as u64,
        document_revision: action.document_revision,
        observation: action.observation,
    })
}

async fn wait_condition_matches(
    state: &BrokerState,
    agent_session_id: &AgentSessionId,
    tab_id: &TabId,
    target: &CdpTarget,
    condition: &WaitCondition,
) -> Result<bool, BrowserToolError> {
    match condition {
        WaitCondition::Delay { .. } => Ok(true),
        WaitCondition::Text {
            text,
            state: expected,
        } => {
            let text = serde_json::to_string(text)
                .map_err(|error| BrowserToolError::invalid_input(error.to_string()))?;
            let expression = format!(
                "(() => {{ const wanted = {text}; const walker = document.createTreeWalker(document.body || document.documentElement, NodeFilter.SHOW_TEXT); while (walker.nextNode()) {{ const node = walker.currentNode; if (!node.textContent.includes(wanted)) continue; const element = node.parentElement; if (!element) continue; const r = element.getBoundingClientRect(); const s = getComputedStyle(element); if (r.width > 0 && r.height > 0 && s.visibility !== 'hidden' && s.display !== 'none') return true; }} return false; }})()"
            );
            let visible = evaluate_truthy(state, target, &expression).await?;
            Ok(if expected.as_deref() == Some("hidden") {
                !visible
            } else {
                visible
            })
        }
        WaitCondition::Element {
            target: element_target,
            state: expected,
        } => {
            let state_value = match element_target {
                ElementTarget::Reference(reference) => {
                    let document_revision = state.browser.document_revision(target).await?;
                    let element = state.references().lock().unwrap().resolve(
                        agent_session_id,
                        tab_id,
                        &reference.reference,
                        &document_revision,
                    );
                    match element {
                        Ok(element) => {
                            state
                                .browser
                                .element_state_backend_node(target, element.backend_node_id)
                                .await?
                        }
                        Err(error)
                            if matches!(
                                error.code,
                                crate::leases::BrowserToolErrorCode::ElementStale
                            ) =>
                        {
                            json!({"attached": false, "visible": false})
                        }
                        Err(error) => return Err(error),
                    }
                }
                ElementTarget::Css(css) => {
                    let selector = serde_json::to_string(&css.css)
                        .map_err(|error| BrowserToolError::invalid_input(error.to_string()))?;
                    state
                        .browser
                        .evaluate(
                            target,
                            &format!(
                                "(() => {{ const e = document.querySelector({selector}); if (!e) return {{attached:false,visible:false}}; const r=e.getBoundingClientRect(),s=getComputedStyle(e),d=e.matches(':disabled')||e.getAttribute('aria-disabled')==='true'; return {{attached:e.isConnected,visible:r.width>0&&r.height>0&&s.visibility!=='hidden'&&s.display!=='none',enabled:!d,editable:!d&&(e instanceof HTMLInputElement||e instanceof HTMLTextAreaElement||e.isContentEditable),checked:Boolean(e.checked||e.getAttribute('aria-checked')==='true')}}; }})()"
                            ),
                        )
                        .await?
                        .value
                        .unwrap_or(Value::Null)
                }
            };
            Ok(match expected.as_str() {
                "attached" => state_value["attached"].as_bool() == Some(true),
                "detached" => state_value["attached"].as_bool() != Some(true),
                "visible" => state_value["visible"].as_bool() == Some(true),
                "hidden" => state_value["visible"].as_bool() != Some(true),
                "enabled" => state_value["enabled"].as_bool() == Some(true),
                "disabled" => state_value["enabled"].as_bool() == Some(false),
                "editable" => state_value["editable"].as_bool() == Some(true),
                "checked" => state_value["checked"].as_bool() == Some(true),
                "unchecked" => state_value["checked"].as_bool() == Some(false),
                _ => {
                    return Err(BrowserToolError::invalid_input(format!(
                        "unknown element wait state `{expected}`"
                    )));
                }
            })
        }
        WaitCondition::Url { value, r#match } => {
            let current = target_by_id(state, &target.id).await?.url;
            match r#match.as_deref().unwrap_or("substring") {
                "exact" => Ok(current == *value),
                "substring" => Ok(current.contains(value)),
                "regex" => regex::Regex::new(value)
                    .map(|pattern| pattern.is_match(&current))
                    .map_err(|error| BrowserToolError::invalid_input(error.to_string())),
                other => Err(BrowserToolError::invalid_input(format!(
                    "unknown URL match mode `{other}`"
                ))),
            }
        }
        WaitCondition::Load { state: expected } => {
            if expected == "network_idle" {
                return Ok(state
                    .diagnostics()
                    .lock()
                    .unwrap()
                    .network_is_idle(&target.id, Duration::from_millis(500)));
            }
            let expression = match expected.as_str() {
                "dom_content_loaded" => {
                    "document.readyState === 'interactive' || document.readyState === 'complete'"
                }
                "load" => "document.readyState === 'complete'",
                other => {
                    return Err(BrowserToolError::invalid_input(format!(
                        "unknown load state `{other}`"
                    )));
                }
            };
            evaluate_truthy(state, target, expression).await
        }
        WaitCondition::Expression { expression } => {
            evaluate_truthy(state, target, expression).await
        }
    }
}

async fn wait_for_network_idle(
    state: &BrokerState,
    target: &CdpTarget,
    timeout_ms: u64,
) -> Result<(), BrowserToolError> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if state
            .diagnostics()
            .lock()
            .unwrap()
            .network_is_idle(&target.id, Duration::from_millis(500))
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(BrowserToolError::operation_timeout(
                "network did not remain idle for 500ms before the timeout",
            ));
        }
        sleep(Duration::from_millis(50)).await;
    }
}

async fn evaluate_truthy(
    state: &BrokerState,
    target: &CdpTarget,
    expression: &str,
) -> Result<bool, BrowserToolError> {
    Ok(state
        .browser
        .evaluate(target, expression)
        .await?
        .value
        .is_some_and(|value| match value {
            Value::Bool(value) => value,
            Value::Null => false,
            Value::Number(ref number) => number.as_f64().is_some_and(|value| value != 0.0),
            Value::String(ref value) => !value.is_empty(),
            _ => true,
        }))
}

async fn broker_screenshot(
    state: &BrokerState,
    params: Result<ScreenshotParams, BrowserToolError>,
) -> Result<ScreenshotResult, BrowserToolError> {
    let params = params?;
    let target = active_owned_target(state, &params.agent_session_id, &params.tab_id).await?;
    ensure_diagnostics_monitor(state, &target).await?;
    if params.target.is_some() && params.full_page {
        return Err(BrowserToolError::invalid_input(
            "screenshot cannot combine `target` with `full_page`",
        ));
    }
    let format = params.format.as_deref().unwrap_or("png");
    let data_base64 = if let Some(element_target) = &params.target {
        let element = resolve_element_target(
            state,
            &params.agent_session_id,
            &params.tab_id,
            &target,
            element_target,
        )
        .await?;
        state
            .browser
            .screenshot_element(&target, element, format, params.quality)
            .await?
    } else {
        state
            .browser
            .screenshot(&target, params.full_page, format, params.quality)
            .await?
    };
    let bytes = BASE64.decode(&data_base64).map_err(|error| {
        BrowserToolError::artifact_error(format!(
            "Chrome returned invalid screenshot data: {error}"
        ))
    })?;
    let (media_type, extension) = match format {
        "png" => ("image/png", "png"),
        "jpeg" => ("image/jpeg", "jpg"),
        "webp" => ("image/webp", "webp"),
        other => {
            return Err(BrowserToolError::invalid_input(format!(
                "unsupported screenshot format `{other}`"
            )));
        }
    };
    let (width, height) = image::load_from_memory(&bytes)
        .map(|image| (image.width(), image.height()))
        .unwrap_or((0, 0));
    let artifact = state.artifacts().lock().unwrap().insert_bytes(
        &params.agent_session_id,
        Some(&params.tab_id),
        "screenshot",
        media_type,
        extension,
        &bytes,
    )?;

    Ok(ScreenshotResult {
        artifact,
        image: ScreenshotImage {
            media_type: media_type.to_string(),
        },
        width,
        height,
    })
}

#[cfg(test)]
async fn broker_evaluate(
    state: &BrokerState,
    params: Result<EvaluateParams, BrowserToolError>,
) -> Result<EvaluateResult, BrowserToolError> {
    let params = params?;
    let target = active_owned_target(state, &params.agent_session_id, &params.tab_id).await?;
    ensure_diagnostics_monitor(state, &target).await?;
    state.browser.evaluate(&target, &params.expression).await
}

async fn broker_evaluate_v3(
    state: &BrokerState,
    params: Result<V3EvaluateParams, BrowserToolError>,
) -> Result<EvaluateResult, BrowserToolError> {
    let params = params?;
    let target = active_owned_target(state, &params.agent_session_id, &params.tab_id).await?;
    ensure_diagnostics_monitor(state, &target).await?;
    let mode = params.mode.as_deref().unwrap_or("expression");
    if let Some(element_target) = &params.target {
        let element = resolve_element_target(
            state,
            &params.agent_session_id,
            &params.tab_id,
            &target,
            element_target,
        )
        .await?;
        return state
            .browser
            .evaluate_on_target(
                &target,
                element,
                &params.source,
                mode,
                &params.args,
                params.await_promise,
            )
            .await;
    }
    let expression = match mode {
        "expression" if params.args.is_empty() => params.source,
        "expression" => {
            return Err(BrowserToolError::invalid_input(
                "evaluate arguments require mode `function`",
            ));
        }
        "function" => {
            let args = serde_json::to_string(&params.args)
                .map_err(|error| BrowserToolError::invalid_input(error.to_string()))?;
            format!("(async () => await ({}) (...{args}))()", params.source)
        }
        mode => {
            return Err(BrowserToolError::invalid_input(format!(
                "unknown evaluation mode `{mode}`"
            )));
        }
    };
    state.browser.evaluate(&target, &expression).await
}

async fn broker_snapshot(
    state: &BrokerState,
    params: Result<SnapshotParams, BrowserToolError>,
) -> Result<SnapshotResult, BrowserToolError> {
    let params = params?;
    let target = active_owned_target(state, &params.agent_session_id, &params.tab_id).await?;
    let root_backend_node_id = match params.root.as_ref() {
        Some(element_target) => Some(
            match resolve_element_target(
                state,
                &params.agent_session_id,
                &params.tab_id,
                &target,
                element_target,
            )
            .await?
            {
                ResolvedElementTarget::Reference(element) => element.backend_node_id,
                ResolvedElementTarget::Css(selector) => {
                    state
                        .browser
                        .resolve_css_backend_node(&target, &selector)
                        .await?
                }
            },
        ),
        None => None,
    };
    snapshot_for_target(
        state,
        &params.agent_session_id,
        &params.tab_id,
        &target,
        SnapshotRequest {
            mode: params.mode.unwrap_or_default(),
            root_backend_node_id,
            depth: params.depth.unwrap_or(8).clamp(1, 64),
            max_nodes: params.max_nodes.unwrap_or(500).clamp(1, 5_000),
            include_hidden: params.include_hidden,
            include_bounds: params.include_bounds,
        },
    )
    .await
    .map(|(snapshot, _)| snapshot)
}

async fn broker_click(
    state: &BrokerState,
    params: Result<ClickParams, BrowserToolError>,
) -> Result<PageActionResult, BrowserToolError> {
    let params = params?;
    let target = active_owned_target(state, &params.agent_session_id, &params.tab_id).await?;
    ensure_diagnostics_monitor(state, &target).await?;
    state.browser.prepare_target_for_action(&target).await?;
    let button = params.button.as_deref().unwrap_or("left");
    let count = params.count.unwrap_or(1);
    let baseline = click_effect_baseline(state, &target).await?;
    let mut delivery_mode = "browser_protocol_input".to_string();
    let mut delivery = match resolve_element_target(
        state,
        &params.agent_session_id,
        &params.tab_id,
        &target,
        &params.target,
    )
    .await?
    {
        ResolvedElementTarget::Reference(element) => {
            retry_element_action(params.timeout_ms, || {
                state.browser.click_backend_node(
                    &target,
                    element.backend_node_id,
                    button,
                    count,
                    &params.modifiers,
                )
            })
            .await?
        }
        ResolvedElementTarget::Css(selector) => {
            retry_element_action(params.timeout_ms, || {
                state.browser.click(
                    &target,
                    &selector,
                    params.timeout_ms.unwrap_or(DEFAULT_CLICK_TIMEOUT_MS),
                    button,
                    count,
                    &params.modifiers,
                )
            })
            .await?
        }
    };

    let submit_candidate = click_submit_candidate(&delivery);
    let mut effect = if submit_candidate {
        wait_for_submit_effect(state, &target, &baseline).await?
    } else {
        wait_for_click_effect(state, &target, &baseline).await?
    };
    if !(if submit_candidate {
        effect_has_submit_signal(&effect)
    } else {
        effect_has_action_signal(&effect)
    }) && submit_candidate
        && button == "left"
        && count == 1
        && let Some(backend_node_id) = click_backend_node_id(&delivery)
    {
        let semantic = state
            .browser
            .semantic_activate_backend_node(&target, backend_node_id)
            .await?;
        delivery_mode = "semantic_dom_activation".to_string();
        attach_delivery_detail(&mut delivery, "semantic_activation", semantic);
        effect = wait_for_submit_effect(state, &target, &baseline).await?;
    }

    let mut result = post_action_observation(
        state,
        &params.agent_session_id,
        &params.tab_id,
        &target,
        params.observe.unwrap_or_default(),
    )
    .await?;
    let (accessibility_changed, accessibility_changed_node_count) =
        observation_change_summary(&result.observation);
    effect.accessibility_changed = accessibility_changed;
    effect.accessibility_changed_node_count = accessibility_changed_node_count;
    result.action = Some(PageActionEvidence {
        delivery_mode,
        release_delivery: click_release_delivery(&delivery),
        delivery_uncertain: click_delivery_uncertain(&delivery),
        resolved_element: delivery.get("resolved_element").cloned(),
        center_hit_test: delivery.get("center_hit_test").cloned(),
        effect,
    });
    Ok(result)
}

struct ClickEffectBaseline {
    url: String,
    network_since: u64,
}

async fn click_effect_baseline(
    state: &BrokerState,
    target: &CdpTarget,
) -> Result<ClickEffectBaseline, BrowserToolError> {
    let current = target_by_id(state, &target.id)
        .await
        .unwrap_or_else(|_| target.clone());
    let network_since = state
        .diagnostics()
        .lock()
        .unwrap()
        .network_events(&target.id, None)
        .into_iter()
        .map(|event| event.sequence)
        .max()
        .unwrap_or(0);
    Ok(ClickEffectBaseline {
        url: current.url,
        network_since,
    })
}

async fn wait_for_click_effect(
    state: &BrokerState,
    target: &CdpTarget,
    baseline: &ClickEffectBaseline,
) -> Result<PageActionEffect, BrowserToolError> {
    let deadline = Instant::now() + Duration::from_millis(750);
    loop {
        let effect = click_effect(state, target, baseline).await?;
        if effect_has_action_signal(&effect) || Instant::now() >= deadline {
            return Ok(effect);
        }
        sleep(Duration::from_millis(50)).await;
    }
}

async fn wait_for_submit_effect(
    state: &BrokerState,
    target: &CdpTarget,
    baseline: &ClickEffectBaseline,
) -> Result<PageActionEffect, BrowserToolError> {
    let deadline = Instant::now() + Duration::from_millis(2_000);
    loop {
        let effect = click_effect(state, target, baseline).await?;
        if effect.url_changed {
            return Ok(effect);
        }
        if effect_has_completed_submit_request(&effect) {
            sleep(Duration::from_millis(100)).await;
            return click_effect(state, target, baseline).await;
        }
        if Instant::now() >= deadline {
            return Ok(effect);
        }
        sleep(Duration::from_millis(50)).await;
    }
}

async fn click_effect(
    state: &BrokerState,
    target: &CdpTarget,
    baseline: &ClickEffectBaseline,
) -> Result<PageActionEffect, BrowserToolError> {
    let current = target_by_id(state, &target.id)
        .await
        .unwrap_or_else(|_| target.clone());
    let network_events = state
        .diagnostics()
        .lock()
        .unwrap()
        .network_events(&target.id, Some(baseline.network_since));
    let network_records = network_records(network_events);
    let network_event_count = network_records.len();
    let network_events = network_records
        .iter()
        .take(ACTION_EVIDENCE_NETWORK_EVENT_LIMIT)
        .map(network_record_value)
        .collect::<Vec<_>>();
    let post_url = current.url;
    Ok(PageActionEffect {
        pre_url: baseline.url.clone(),
        url_changed: post_url != baseline.url,
        post_url,
        network_event_count,
        network_events,
        accessibility_changed: None,
        accessibility_changed_node_count: None,
    })
}

fn effect_has_action_signal(effect: &PageActionEffect) -> bool {
    effect.url_changed || effect.network_event_count > 0
}

fn effect_has_submit_signal(effect: &PageActionEffect) -> bool {
    effect.url_changed || effect_has_completed_submit_request(effect)
}

fn effect_has_completed_submit_request(effect: &PageActionEffect) -> bool {
    effect.network_events.iter().any(|event| {
        event
            .get("method")
            .and_then(Value::as_str)
            .is_some_and(|method| method != "GET")
            && (event.get("status").is_some_and(|status| !status.is_null())
                || event
                    .get("failed")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                || event
                    .get("duration_ms")
                    .is_some_and(|duration| !duration.is_null()))
    })
}

fn click_submit_candidate(delivery: &Value) -> bool {
    delivery
        .get("submit_candidate")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn click_backend_node_id(delivery: &Value) -> Option<i64> {
    delivery
        .get("resolved_element")
        .and_then(|element| element.get("backend_node_id"))
        .and_then(Value::as_i64)
}

fn click_release_delivery(delivery: &Value) -> String {
    delivery
        .get("dispatch")
        .and_then(|dispatch| dispatch.get("release_delivery"))
        .and_then(Value::as_str)
        .unwrap_or("delivery_uncertain")
        .to_string()
}

fn click_delivery_uncertain(delivery: &Value) -> bool {
    delivery
        .get("dispatch")
        .and_then(|dispatch| dispatch.get("delivery_uncertain"))
        .and_then(Value::as_bool)
        .unwrap_or_else(|| click_release_delivery(delivery) == "delivery_uncertain")
}

fn attach_delivery_detail(delivery: &mut Value, key: &str, detail: Value) {
    if let Some(object) = delivery.as_object_mut() {
        object.insert(key.to_string(), detail);
    }
}

fn observation_change_summary(observation: &Observation) -> (Option<bool>, Option<usize>) {
    match observation {
        Observation::Diff { diff } => (
            Some(diff.changed_node_count > 0),
            Some(diff.changed_node_count),
        ),
        Observation::Snapshot { .. } | Observation::None => (None, None),
    }
}

async fn broker_fill(
    state: &BrokerState,
    params: Result<FillParams, BrowserToolError>,
) -> Result<PageActionResult, BrowserToolError> {
    let params = params?;
    let target = active_owned_target(state, &params.agent_session_id, &params.tab_id).await?;
    match resolve_element_target(
        state,
        &params.agent_session_id,
        &params.tab_id,
        &target,
        &params.target,
    )
    .await?
    {
        ResolvedElementTarget::Reference(element) => {
            retry_element_action(params.timeout_ms, || {
                state
                    .browser
                    .fill_backend_node(&target, element.backend_node_id, &params.value)
            })
            .await?;
        }
        ResolvedElementTarget::Css(selector) => {
            retry_element_action(params.timeout_ms, || {
                state.browser.fill_css(&target, &selector, &params.value)
            })
            .await?;
        }
    }
    post_action_observation(
        state,
        &params.agent_session_id,
        &params.tab_id,
        &target,
        params.observe.unwrap_or_default(),
    )
    .await
}

async fn broker_fill_form(
    state: &BrokerState,
    params: Result<FillFormParams, BrowserToolError>,
) -> Result<FillFormResult, BrowserToolError> {
    let params = params?;
    if params.fields.len() < 2 {
        return Err(BrowserToolError::invalid_input(
            "fill_form requires at least two fields",
        ));
    }
    let target = active_owned_target(state, &params.agent_session_id, &params.tab_id).await?;
    let total_fields = params.fields.len();
    let timeout_ms = params.timeout_ms;
    let mut completed_fields = 0;
    for field in params.fields {
        if let Err(mut error) = apply_form_field(
            state,
            &params.agent_session_id,
            &params.tab_id,
            &target,
            field,
            timeout_ms,
        )
        .await
        {
            error.message = format!(
                "fill_form completed {completed_fields} of {total_fields} fields: {}",
                error.message
            );
            return Err(error);
        }
        completed_fields += 1;
    }
    let action = post_action_observation(
        state,
        &params.agent_session_id,
        &params.tab_id,
        &target,
        params.observe.unwrap_or_default(),
    )
    .await?;
    Ok(FillFormResult {
        completed_fields,
        total_fields,
        document_revision: action.document_revision,
        observation: action.observation,
    })
}

async fn apply_form_field(
    state: &BrokerState,
    agent_session_id: &AgentSessionId,
    tab_id: &TabId,
    target: &CdpTarget,
    field: FormField,
    timeout_ms: Option<u64>,
) -> Result<(), BrowserToolError> {
    match field {
        FormField::Text {
            target: element_target,
            value,
        } => match resolve_element_target(state, agent_session_id, tab_id, target, &element_target)
            .await?
        {
            ResolvedElementTarget::Reference(element) => {
                retry_element_action(timeout_ms, || {
                    state
                        .browser
                        .fill_backend_node(target, element.backend_node_id, &value)
                })
                .await
            }
            ResolvedElementTarget::Css(selector) => {
                retry_element_action(timeout_ms, || {
                    state.browser.fill_css(target, &selector, &value)
                })
                .await
            }
        },
        FormField::Select {
            target: element_target,
            values,
        } => match resolve_element_target(state, agent_session_id, tab_id, target, &element_target)
            .await?
        {
            ResolvedElementTarget::Reference(element) => {
                retry_element_action(timeout_ms, || {
                    state
                        .browser
                        .select_backend_node(target, element.backend_node_id, &values)
                })
                .await
            }
            ResolvedElementTarget::Css(selector) => {
                retry_element_action(timeout_ms, || {
                    state.browser.select_css(target, &selector, &values)
                })
                .await
            }
        },
        FormField::Checked {
            target: element_target,
            checked,
        } => match resolve_element_target(state, agent_session_id, tab_id, target, &element_target)
            .await?
        {
            ResolvedElementTarget::Reference(element) => {
                retry_element_action(timeout_ms, || {
                    state
                        .browser
                        .set_checked_backend_node(target, element.backend_node_id, checked)
                })
                .await
            }
            ResolvedElementTarget::Css(selector) => {
                retry_element_action(timeout_ms, || {
                    state.browser.set_checked_css(target, &selector, checked)
                })
                .await
            }
        },
    }
}

enum ResolvedElementTarget {
    Reference(ElementReference),
    Css(String),
}

async fn retry_element_action<T, F, Fut>(
    timeout_ms: Option<u64>,
    mut action: F,
) -> Result<T, BrowserToolError>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, BrowserToolError>>,
{
    let timeout_ms = timeout_ms
        .unwrap_or(DEFAULT_ELEMENT_TIMEOUT_MS)
        .clamp(1, 60_000);
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        match action().await {
            Ok(value) => return Ok(value),
            Err(error)
                if matches!(
                    error.code,
                    BrowserToolErrorCode::ElementNotFound
                        | BrowserToolErrorCode::ElementNotActionable
                ) && Instant::now() < deadline =>
            {
                sleep(Duration::from_millis(50)).await;
            }
            Err(error)
                if matches!(
                    error.code,
                    BrowserToolErrorCode::ElementNotFound
                        | BrowserToolErrorCode::ElementNotActionable
                ) =>
            {
                return Err(BrowserToolError::operation_timeout(format!(
                    "element did not become actionable within {timeout_ms} ms: {}",
                    error.message
                )));
            }
            Err(error) => return Err(error),
        }
    }
}

async fn resolve_element_target(
    state: &BrokerState,
    agent_session_id: &AgentSessionId,
    tab_id: &TabId,
    target: &CdpTarget,
    element_target: &ElementTarget,
) -> Result<ResolvedElementTarget, BrowserToolError> {
    match element_target {
        ElementTarget::Reference(ElementReferenceTarget { reference }) => {
            let document_revision = state.browser.document_revision(target).await?;
            let element = state.references().lock().unwrap().resolve(
                agent_session_id,
                tab_id,
                reference,
                &document_revision,
            )?;
            if element.target_id != target.id {
                return Err(BrowserToolError::element_stale(reference));
            }
            Ok(ResolvedElementTarget::Reference(element))
        }
        ElementTarget::Css(css_target) => {
            let css = &css_target.css;
            if let Some(frame_ref) = &css_target.frame_ref {
                let document_revision = state.browser.document_revision(target).await?;
                let frame = state.references().lock().unwrap().resolve(
                    agent_session_id,
                    tab_id,
                    frame_ref,
                    &document_revision,
                )?;
                if frame.target_id != target.id {
                    return Err(BrowserToolError::element_stale(frame_ref));
                }
                let backend_node_id = state
                    .browser
                    .resolve_frame_css_backend_node(target, frame.backend_node_id, css)
                    .await?;
                return Ok(ResolvedElementTarget::Reference(ElementReference {
                    agent_session_id: agent_session_id.clone(),
                    tab_id: tab_id.clone(),
                    target_id: target.id.clone(),
                    frame_id: frame.frame_id,
                    document_revision,
                    backend_node_id,
                    role: "css".to_string(),
                    name: css.clone(),
                }));
            }
            Ok(ResolvedElementTarget::Css(css.clone()))
        }
    }
}

struct SnapshotRequest {
    mode: SnapshotMode,
    root_backend_node_id: Option<i64>,
    depth: usize,
    max_nodes: usize,
    include_hidden: bool,
    include_bounds: bool,
}

async fn snapshot_for_target(
    state: &BrokerState,
    agent_session_id: &AgentSessionId,
    tab_id: &TabId,
    target: &CdpTarget,
    options: SnapshotRequest,
) -> Result<(SnapshotResult, crate::protocol::SnapshotDiff), BrowserToolError> {
    let SnapshotRequest {
        mode,
        root_backend_node_id,
        depth,
        max_nodes,
        include_hidden,
        include_bounds,
    } = options;
    let raw = state
        .browser
        .accessibility_snapshot(
            target,
            root_backend_node_id.is_none().then_some(depth),
            include_bounds,
        )
        .await?;
    state.references().lock().unwrap().build_snapshot(
        SnapshotBuildContext {
            agent_session_id,
            tab_id,
            target_id: &target.id,
            mode,
            root_backend_node_id,
            depth,
            max_nodes,
            include_hidden,
            include_bounds,
        },
        raw,
    )
}

async fn post_action_observation(
    state: &BrokerState,
    agent_session_id: &AgentSessionId,
    tab_id: &TabId,
    target: &CdpTarget,
    mode: ObservationMode,
) -> Result<PageActionResult, BrowserToolError> {
    match mode {
        ObservationMode::None => Ok(PageActionResult {
            document_revision: state
                .references()
                .lock()
                .unwrap()
                .document_revision(tab_id)
                .unwrap_or_else(|| format!("target:{}", target.id)),
            observation: Observation::None,
            action: None,
        }),
        ObservationMode::Diff => {
            let (snapshot, diff) = snapshot_for_target(
                state,
                agent_session_id,
                tab_id,
                target,
                SnapshotRequest {
                    mode: SnapshotMode::Meaningful,
                    root_backend_node_id: None,
                    depth: 8,
                    max_nodes: 500,
                    include_hidden: false,
                    include_bounds: false,
                },
            )
            .await?;
            Ok(PageActionResult {
                document_revision: snapshot.document_revision,
                observation: Observation::Diff { diff },
                action: None,
            })
        }
        ObservationMode::Snapshot => {
            let (snapshot, _) = snapshot_for_target(
                state,
                agent_session_id,
                tab_id,
                target,
                SnapshotRequest {
                    mode: SnapshotMode::Meaningful,
                    root_backend_node_id: None,
                    depth: 8,
                    max_nodes: 500,
                    include_hidden: false,
                    include_bounds: false,
                },
            )
            .await?;
            Ok(PageActionResult {
                document_revision: snapshot.document_revision.clone(),
                observation: Observation::Snapshot { snapshot },
                action: None,
            })
        }
    }
}

#[cfg(test)]
async fn broker_type_text(
    state: &BrokerState,
    params: Result<TypeTextParams, BrowserToolError>,
) -> Result<TypeTextResult, BrowserToolError> {
    let params = params?;
    let target = active_owned_target(state, &params.agent_session_id, &params.tab_id).await?;
    ensure_diagnostics_monitor(state, &target).await?;
    state.browser.type_text(&target, &params.text).await?;
    Ok(TypeTextResult { typed: true })
}

async fn broker_type_text_v3(
    state: &BrokerState,
    params: Result<V3TypeTextParams, BrowserToolError>,
) -> Result<PageActionResult, BrowserToolError> {
    let params = params?;
    let target = active_owned_target(state, &params.agent_session_id, &params.tab_id).await?;
    ensure_diagnostics_monitor(state, &target).await?;
    let resolved = resolve_element_target(
        state,
        &params.agent_session_id,
        &params.tab_id,
        &target,
        &params.target,
    )
    .await?;
    let delay_ms = params.delay_ms.unwrap_or(0).min(1_000);
    let initial_text = if delay_ms == 0 {
        params.text.as_str()
    } else {
        ""
    };
    match resolved {
        ResolvedElementTarget::Reference(element) => {
            retry_element_action(params.timeout_ms, || {
                state
                    .browser
                    .type_text_backend_node(&target, element.backend_node_id, initial_text)
            })
            .await?;
        }
        ResolvedElementTarget::Css(selector) => {
            retry_element_action(params.timeout_ms, || {
                state
                    .browser
                    .type_text_css(&target, &selector, initial_text)
            })
            .await?;
        }
    }
    if delay_ms > 0 {
        for character in params.text.chars() {
            state
                .browser
                .type_text(&target, &character.to_string())
                .await?;
            sleep(Duration::from_millis(delay_ms)).await;
        }
    }
    post_action_observation(
        state,
        &params.agent_session_id,
        &params.tab_id,
        &target,
        params.observe.unwrap_or_default(),
    )
    .await
}

#[cfg(test)]
async fn broker_press_key(
    state: &BrokerState,
    params: Result<PressKeyParams, BrowserToolError>,
) -> Result<PressKeyResult, BrowserToolError> {
    let params = params?;
    let target = active_owned_target(state, &params.agent_session_id, &params.tab_id).await?;
    ensure_diagnostics_monitor(state, &target).await?;
    state.browser.prepare_target_for_action(&target).await?;
    state
        .browser
        .press_key(&target, &params.key, &params.modifiers)
        .await?;
    Ok(PressKeyResult { pressed: true })
}

async fn broker_press_key_v3(
    state: &BrokerState,
    params: Result<V3PressKeyParams, BrowserToolError>,
) -> Result<PageActionResult, BrowserToolError> {
    let params = params?;
    let target = active_owned_target(state, &params.agent_session_id, &params.tab_id).await?;
    ensure_diagnostics_monitor(state, &target).await?;
    state.browser.prepare_target_for_action(&target).await?;
    if let Some(element_target) = params.target.as_ref() {
        match resolve_element_target(
            state,
            &params.agent_session_id,
            &params.tab_id,
            &target,
            element_target,
        )
        .await?
        {
            ResolvedElementTarget::Reference(element) => {
                retry_element_action(params.timeout_ms, || {
                    state
                        .browser
                        .type_text_backend_node(&target, element.backend_node_id, "")
                })
                .await?;
            }
            ResolvedElementTarget::Css(selector) => {
                retry_element_action(params.timeout_ms, || {
                    state.browser.type_text_css(&target, &selector, "")
                })
                .await?;
            }
        }
    } else {
        ensure_focused_document_for_raw_input(state, &target, &params.tab_id).await?;
    }
    state
        .browser
        .press_key(&target, &params.key, &params.modifiers)
        .await?;
    post_action_observation(
        state,
        &params.agent_session_id,
        &params.tab_id,
        &target,
        params.observe.unwrap_or_default(),
    )
    .await
}

async fn broker_interact(
    state: &BrokerState,
    params: Result<DomainParams, BrowserToolError>,
) -> Result<Value, BrowserToolError> {
    let params = params?;
    let tab_id = params
        .tab_id
        .as_ref()
        .ok_or_else(|| BrowserToolError::invalid_input("interact requires tab_id"))?;
    let target = active_owned_target(state, &params.agent_session_id, tab_id).await?;
    let observe = params
        .arguments
        .get("observe")
        .cloned()
        .map(serde_json::from_value)
        .transpose()
        .map_err(|error| BrowserToolError::invalid_input(error.to_string()))?
        .unwrap_or_default();
    let timeout_ms = params.arguments.get("timeout_ms").and_then(Value::as_u64);

    match params.operation.as_str() {
        "select_options" => {
            let values = string_array_argument(&params.arguments, "values")?;
            match resolve_domain_element_target(state, &params, tab_id, &target, "target").await? {
                ResolvedElementTarget::Reference(element) => {
                    retry_element_action(timeout_ms, || {
                        state
                            .browser
                            .select_backend_node(&target, element.backend_node_id, &values)
                    })
                    .await?;
                }
                ResolvedElementTarget::Css(selector) => {
                    retry_element_action(timeout_ms, || {
                        state.browser.select_css(&target, &selector, &values)
                    })
                    .await?;
                }
            }
        }
        "set_checked" => {
            let checked = bool_argument(&params.arguments, "checked")?;
            match resolve_domain_element_target(state, &params, tab_id, &target, "target").await? {
                ResolvedElementTarget::Reference(element) => {
                    retry_element_action(timeout_ms, || {
                        state.browser.set_checked_backend_node(
                            &target,
                            element.backend_node_id,
                            checked,
                        )
                    })
                    .await?;
                }
                ResolvedElementTarget::Css(selector) => {
                    retry_element_action(timeout_ms, || {
                        state.browser.set_checked_css(&target, &selector, checked)
                    })
                    .await?;
                }
            }
        }
        "hover" => {
            state.browser.prepare_target_for_action(&target).await?;
            let element =
                resolve_domain_backend_element(state, &params, tab_id, &target, "target").await?;
            retry_element_action(timeout_ms, || {
                state
                    .browser
                    .hover_backend_node(&target, element.backend_node_id)
            })
            .await?;
        }
        "drag" => {
            state.browser.prepare_target_for_action(&target).await?;
            let source =
                resolve_domain_backend_element(state, &params, tab_id, &target, "source").await?;
            let destination =
                resolve_domain_backend_element(state, &params, tab_id, &target, "destination")
                    .await?;
            retry_element_action(timeout_ms, || {
                state.browser.drag_backend_nodes(
                    &target,
                    source.backend_node_id,
                    destination.backend_node_id,
                )
            })
            .await?;
        }
        "drop" => {
            let element =
                resolve_domain_backend_element(state, &params, tab_id, &target, "target").await?;
            let paths = optional_string_array_argument(&params.arguments, "paths")?;
            let files = read_workspace_files(state, &params.agent_session_id, &paths)?;
            let data = params
                .arguments
                .get("data")
                .cloned()
                .unwrap_or_else(|| json!({}));
            retry_element_action(timeout_ms, || {
                state.browser.drop_data_backend_node(
                    &target,
                    element.backend_node_id,
                    &files,
                    &data,
                )
            })
            .await?;
        }
        "upload_files" => {
            let element =
                resolve_domain_backend_element(state, &params, tab_id, &target, "target").await?;
            let paths = string_array_argument(&params.arguments, "paths")?;
            let paths = resolve_workspace_paths(state, &params.agent_session_id, &paths)?;
            retry_element_action(timeout_ms, || {
                state
                    .browser
                    .upload_files_backend_node(&target, element.backend_node_id, &paths)
            })
            .await?;
        }
        "handle_dialog" => {
            let action = string_argument(&params.arguments, "action")?;
            state
                .browser
                .handle_dialog(
                    &target,
                    action == "accept",
                    params.arguments.get("prompt_text").and_then(Value::as_str),
                )
                .await?;
        }
        "scroll" => {
            let delta_x = number_argument_or(&params.arguments, "delta_x", 0.0)?;
            let delta_y = number_argument_or(&params.arguments, "delta_y", 0.0)?;
            if params.arguments.contains_key("target") {
                let element =
                    resolve_domain_backend_element(state, &params, tab_id, &target, "target")
                        .await?;
                state
                    .browser
                    .scroll_backend_node(&target, element.backend_node_id, delta_x, delta_y)
                    .await?;
            } else {
                state
                    .browser
                    .evaluate(
                        &target,
                        &format!("window.scrollBy({delta_x}, {delta_y}); true"),
                    )
                    .await?;
            }
        }
        "click_at" => {
            state.browser.prepare_target_for_action(&target).await?;
            ensure_focused_document_for_raw_input(state, &target, tab_id).await?;
            let modifiers = optional_string_array_argument(&params.arguments, "modifiers")?;
            state
                .browser
                .click_at(
                    &target,
                    number_argument_or(&params.arguments, "x", 0.0)?,
                    number_argument_or(&params.arguments, "y", 0.0)?,
                    params
                        .arguments
                        .get("button")
                        .and_then(Value::as_str)
                        .unwrap_or("left"),
                    params
                        .arguments
                        .get("count")
                        .and_then(Value::as_i64)
                        .unwrap_or(1)
                        .clamp(1, 2),
                    &modifiers,
                )
                .await?;
        }
        operation => {
            return Err(BrowserToolError::invalid_input(format!(
                "unknown interact operation `{operation}`"
            )));
        }
    }

    let action =
        post_action_observation(state, &params.agent_session_id, tab_id, &target, observe).await?;
    Ok(json!({
        "operation": params.operation,
        "document_revision": action.document_revision,
        "observation": action.observation,
    }))
}

async fn ensure_focused_document_for_raw_input(
    state: &BrokerState,
    target: &CdpTarget,
    tab_id: &TabId,
) -> Result<(), BrowserToolError> {
    if state.browser.has_focus(target).await? {
        return Ok(());
    }

    Err(BrowserToolError::focus_required(tab_id))
}

async fn resolve_domain_backend_element(
    state: &BrokerState,
    params: &DomainParams,
    tab_id: &TabId,
    target: &CdpTarget,
    field: &str,
) -> Result<ElementReference, BrowserToolError> {
    let element_target: ElementTarget = serde_json::from_value(
        params
            .arguments
            .get(field)
            .cloned()
            .ok_or_else(|| BrowserToolError::invalid_input(format!("missing `{field}`")))?,
    )
    .map_err(|error| BrowserToolError::invalid_input(error.to_string()))?;
    match resolve_element_target(
        state,
        &params.agent_session_id,
        tab_id,
        target,
        &element_target,
    )
    .await?
    {
        ResolvedElementTarget::Reference(element) => Ok(element),
        ResolvedElementTarget::Css(selector) => {
            let document_revision = state.browser.document_revision(target).await?;
            let backend_node_id = state
                .browser
                .resolve_css_backend_node(target, &selector)
                .await?;
            Ok(ElementReference {
                agent_session_id: params.agent_session_id.clone(),
                tab_id: tab_id.clone(),
                target_id: target.id.clone(),
                frame_id: "main".to_string(),
                document_revision,
                backend_node_id,
                role: "css".to_string(),
                name: selector,
            })
        }
    }
}

async fn resolve_domain_element_target(
    state: &BrokerState,
    params: &DomainParams,
    tab_id: &TabId,
    target: &CdpTarget,
    field: &str,
) -> Result<ResolvedElementTarget, BrowserToolError> {
    let element_target: ElementTarget = serde_json::from_value(
        params
            .arguments
            .get(field)
            .cloned()
            .ok_or_else(|| BrowserToolError::invalid_input(format!("missing `{field}`")))?,
    )
    .map_err(|error| BrowserToolError::invalid_input(error.to_string()))?;
    resolve_element_target(
        state,
        &params.agent_session_id,
        tab_id,
        target,
        &element_target,
    )
    .await
}

fn string_argument<'a>(
    arguments: &'a serde_json::Map<String, Value>,
    name: &str,
) -> Result<&'a str, BrowserToolError> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| BrowserToolError::invalid_input(format!("missing `{name}`")))
}

fn bool_argument(
    arguments: &serde_json::Map<String, Value>,
    name: &str,
) -> Result<bool, BrowserToolError> {
    arguments
        .get(name)
        .and_then(Value::as_bool)
        .ok_or_else(|| BrowserToolError::invalid_input(format!("missing `{name}`")))
}

fn number_argument_or(
    arguments: &serde_json::Map<String, Value>,
    name: &str,
    default: f64,
) -> Result<f64, BrowserToolError> {
    arguments.get(name).map_or(Ok(default), |value| {
        value
            .as_f64()
            .ok_or_else(|| BrowserToolError::invalid_input(format!("`{name}` must be a number")))
    })
}

fn string_array_argument(
    arguments: &serde_json::Map<String, Value>,
    name: &str,
) -> Result<Vec<String>, BrowserToolError> {
    let values = optional_string_array_argument(arguments, name)?;
    if values.is_empty() {
        return Err(BrowserToolError::invalid_input(format!(
            "`{name}` must contain at least one value"
        )));
    }
    Ok(values)
}

fn optional_string_array_argument(
    arguments: &serde_json::Map<String, Value>,
    name: &str,
) -> Result<Vec<String>, BrowserToolError> {
    arguments
        .get(name)
        .map(|value| {
            value
                .as_array()
                .ok_or_else(|| {
                    BrowserToolError::invalid_input(format!("`{name}` must be an array"))
                })?
                .iter()
                .map(|value| {
                    value.as_str().map(str::to_string).ok_or_else(|| {
                        BrowserToolError::invalid_input(format!(
                            "`{name}` must contain only strings"
                        ))
                    })
                })
                .collect()
        })
        .transpose()
        .map(Option::unwrap_or_default)
}

fn resolve_workspace_paths(
    state: &BrokerState,
    session_id: &AgentSessionId,
    paths: &[String],
) -> Result<Vec<String>, BrowserToolError> {
    let workspace_root = state
        .registry()
        .lock()
        .unwrap()
        .session(session_id)
        .and_then(|session| session.workspace_root.clone())
        .ok_or_else(|| {
            BrowserToolError::workspace_unavailable(
                "this browser session has no host workspace root",
            )
        })?;
    let workspace_root = workspace_root
        .canonicalize()
        .map_err(|error| BrowserToolError::workspace_unavailable(error.to_string()))?;
    paths
        .iter()
        .map(|path| {
            let path = Path::new(path);
            let candidate = if path.is_absolute() {
                path.to_path_buf()
            } else {
                workspace_root.join(path)
            };
            let canonical = candidate
                .canonicalize()
                .map_err(|error| BrowserToolError::workspace_unavailable(error.to_string()))?;
            if !canonical.starts_with(&workspace_root) || !canonical.is_file() {
                return Err(BrowserToolError::path_outside_workspace(path));
            }
            Ok(canonical.to_string_lossy().into_owned())
        })
        .collect()
}

fn read_workspace_files(
    state: &BrokerState,
    session_id: &AgentSessionId,
    paths: &[String],
) -> Result<Value, BrowserToolError> {
    let paths = resolve_workspace_paths(state, session_id, paths)?;
    let files = paths
        .into_iter()
        .map(|path| {
            let bytes = fs::read(&path)
                .map_err(|error| BrowserToolError::workspace_unavailable(error.to_string()))?;
            if bytes.len() > 16 * 1024 * 1024 {
                return Err(BrowserToolError::invalid_input(
                    "drop files are limited to 16 MiB each",
                ));
            }
            Ok(json!({
                "name": Path::new(&path).file_name().and_then(|name| name.to_str()).unwrap_or("file"),
                "media_type": "application/octet-stream",
                "base64": BASE64.encode(bytes),
            }))
        })
        .collect::<Result<Vec<_>, BrowserToolError>>()?;
    Ok(Value::Array(files))
}

async fn broker_console(
    state: &BrokerState,
    params: Result<DomainParams, BrowserToolError>,
) -> Result<Value, BrowserToolError> {
    let params = params?;
    let tab_id = params
        .tab_id
        .as_ref()
        .ok_or_else(|| BrowserToolError::invalid_input("console requires tab_id"))?;
    let target = active_owned_target(state, &params.agent_session_id, tab_id).await?;
    ensure_diagnostics_monitor(state, &target).await?;
    match params.operation.as_str() {
        "list" => {
            let since = params.arguments.get("since").and_then(Value::as_u64);
            let limit = params
                .arguments
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(100)
                .clamp(1, 500) as usize;
            let levels = optional_string_array_argument(&params.arguments, "levels")?;
            let pattern = params
                .arguments
                .get("text_pattern")
                .and_then(Value::as_str)
                .map(regex::Regex::new)
                .transpose()
                .map_err(|error| BrowserToolError::invalid_input(error.to_string()))?;
            let all = state
                .diagnostics()
                .lock()
                .unwrap()
                .console_messages(&target.id, since)
                .into_iter()
                .filter(|message| {
                    levels.is_empty() || levels.contains(&normalize_console_level(&message.level))
                })
                .filter(|message| {
                    pattern
                        .as_ref()
                        .is_none_or(|pattern| pattern.is_match(&message.text))
                })
                .collect::<Vec<_>>();
            let truncated = all.len() > limit;
            let messages = all
                .iter()
                .take(limit)
                .map(console_message_value)
                .collect::<Vec<_>>();
            let next_since = all
                .last()
                .map(|message| message.sequence)
                .unwrap_or(since.unwrap_or(0));
            let mut result = json!({
                "operation":"list",
                "messages":messages,
                "next_since":next_since,
                "truncated":truncated
            });
            if truncated {
                let bytes = serde_json::to_vec(&all)
                    .map_err(|error| BrowserToolError::artifact_error(error.to_string()))?;
                let artifact = state.artifacts().lock().unwrap().insert_bytes(
                    &params.agent_session_id,
                    Some(tab_id),
                    "console",
                    "application/json",
                    "json",
                    &bytes,
                )?;
                result["artifact"] = serde_json::to_value(artifact)
                    .map_err(|error| BrowserToolError::artifact_error(error.to_string()))?;
            }
            Ok(result)
        }
        "get" => {
            let sequence = parse_scoped_sequence(
                string_argument(&params.arguments, "message_id")?,
                "console_",
            )?;
            let message = state
                .diagnostics()
                .lock()
                .unwrap()
                .console_messages(&target.id, None)
                .into_iter()
                .find(|message| message.sequence == sequence)
                .ok_or_else(|| BrowserToolError::invalid_input("unknown console message_id"))?;
            Ok(json!({"operation":"get", "message":console_message_value(&message)}))
        }
        "clear" => {
            state
                .diagnostics()
                .lock()
                .unwrap()
                .clear_console(&target.id);
            Ok(json!({"operation":"clear", "cleared":true}))
        }
        operation => Err(BrowserToolError::invalid_input(format!(
            "unknown console operation `{operation}`"
        ))),
    }
}

fn normalize_console_level(level: &str) -> String {
    match level {
        "warn" | "warning" => "warning",
        "log" => "info",
        "verbose" | "debug" | "info" | "error" => level,
        _ => "info",
    }
    .to_string()
}

fn console_message_value(message: &ConsoleMessage) -> Value {
    json!({
        "message_id":format!("console_{}", message.sequence),
        "sequence":message.sequence,
        "level":normalize_console_level(&message.level),
        "text":message.text,
        "timestamp_ms":message.timestamp_ms,
        "source":{},
        "stack":[],
        "arguments":[]
    })
}

#[derive(Debug, Clone)]
struct NetworkRecord {
    public_id: String,
    raw_id: String,
    sequence: u64,
    url: String,
    method: String,
    resource_type: Option<String>,
    status: Option<u16>,
    mime_type: Option<String>,
    failed: bool,
    error_text: Option<String>,
    started_at_ms: Option<u64>,
    finished_at_ms: Option<u64>,
    request_headers: std::collections::BTreeMap<String, String>,
    response_headers: std::collections::BTreeMap<String, String>,
}

fn network_records(events: Vec<NetworkEvent>) -> Vec<NetworkRecord> {
    let mut records = std::collections::BTreeMap::<String, NetworkRecord>::new();
    for event in events {
        let Some(raw_id) = event.request_id.clone() else {
            continue;
        };
        let entry = records
            .entry(raw_id.clone())
            .or_insert_with(|| NetworkRecord {
                public_id: format!("request_{}", event.sequence),
                raw_id,
                sequence: event.sequence,
                url: event.url.clone().unwrap_or_default(),
                method: event.method.clone().unwrap_or_else(|| "GET".to_string()),
                resource_type: event.resource_type.clone(),
                status: None,
                mime_type: None,
                failed: false,
                error_text: None,
                started_at_ms: event.timestamp_ms,
                finished_at_ms: None,
                request_headers: std::collections::BTreeMap::new(),
                response_headers: std::collections::BTreeMap::new(),
            });
        match event.kind.as_str() {
            "request" => {
                entry.sequence = event.sequence;
                entry.public_id = format!("request_{}", event.sequence);
                entry.url = event.url.unwrap_or_default();
                entry.method = event.method.unwrap_or_else(|| "GET".to_string());
                entry.resource_type = event.resource_type;
                entry.started_at_ms = event.timestamp_ms;
                entry.request_headers = event.headers;
            }
            "response" => {
                entry.status = event.status;
                entry.mime_type = event.mime_type;
                entry.response_headers = event.headers;
                if !event.url.as_deref().unwrap_or_default().is_empty() {
                    entry.url = event.url.unwrap_or_default();
                }
            }
            "failed" => {
                entry.failed = true;
                entry.error_text = event.error_text;
                entry.finished_at_ms = event.timestamp_ms;
            }
            "finished" => entry.finished_at_ms = event.timestamp_ms,
            _ => {}
        }
    }
    let mut records = records.into_values().collect::<Vec<_>>();
    records.sort_by_key(|record| record.sequence);
    records
}

fn network_record_value(record: &NetworkRecord) -> Value {
    json!({
        "request_id":record.public_id,
        "sequence":record.sequence,
        "url":record.url,
        "method":record.method,
        "resource_type":record.resource_type,
        "status":record.status,
        "mime_type":record.mime_type,
        "failed":record.failed,
        "error_text":record.error_text,
        "started_at_ms":record.started_at_ms,
        "duration_ms":record.started_at_ms.zip(record.finished_at_ms).map(|(start,end)| end.saturating_sub(start) as f64)
    })
}

async fn broker_network(
    state: &BrokerState,
    params: Result<DomainParams, BrowserToolError>,
) -> Result<Value, BrowserToolError> {
    let params = params?;
    let tab_id = params
        .tab_id
        .as_ref()
        .ok_or_else(|| BrowserToolError::invalid_input("network requires tab_id"))?;
    let target = active_owned_target(state, &params.agent_session_id, tab_id).await?;
    ensure_diagnostics_monitor(state, &target).await?;
    match params.operation.as_str() {
        "list" => {
            let since = params.arguments.get("since").and_then(Value::as_u64);
            let limit = params
                .arguments
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(100)
                .clamp(1, 500) as usize;
            let url_pattern = params
                .arguments
                .get("url_pattern")
                .and_then(Value::as_str)
                .map(regex::Regex::new)
                .transpose()
                .map_err(|error| BrowserToolError::invalid_input(error.to_string()))?;
            let resource_types =
                optional_string_array_argument(&params.arguments, "resource_types")?;
            let status_min = params.arguments.get("status_min").and_then(Value::as_u64);
            let status_max = params.arguments.get("status_max").and_then(Value::as_u64);
            let include_static = params
                .arguments
                .get("include_static")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let records = network_records(
                state
                    .diagnostics()
                    .lock()
                    .unwrap()
                    .network_events(&target.id, since),
            )
            .into_iter()
            .filter(|record| {
                url_pattern
                    .as_ref()
                    .is_none_or(|pattern| pattern.is_match(&record.url))
            })
            .filter(|record| {
                resource_types.is_empty()
                    || record
                        .resource_type
                        .as_ref()
                        .is_some_and(|kind| resource_types.contains(kind))
            })
            .filter(|record| {
                include_static
                    || !record.resource_type.as_deref().is_some_and(|kind| {
                        matches!(
                            kind.to_ascii_lowercase().as_str(),
                            "font" | "image" | "manifest" | "media" | "stylesheet"
                        )
                    })
            })
            .filter(|record| {
                status_min.is_none_or(|minimum| {
                    record
                        .status
                        .is_some_and(|status| u64::from(status) >= minimum)
                })
            })
            .filter(|record| {
                status_max.is_none_or(|maximum| {
                    record
                        .status
                        .is_some_and(|status| u64::from(status) <= maximum)
                })
            })
            .collect::<Vec<_>>();
            let truncated = records.len() > limit;
            let next_since = records
                .last()
                .map(|record| record.sequence)
                .unwrap_or(since.unwrap_or(0));
            Ok(json!({
                "operation":"list",
                "requests":records.iter().take(limit).map(network_record_value).collect::<Vec<_>>(),
                "next_since":next_since,
                "truncated":truncated
            }))
        }
        "get" => {
            let request_id = string_argument(&params.arguments, "request_id")?;
            let include_response_body = params
                .arguments
                .get("include_response_body")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let deadline = Instant::now() + Duration::from_secs(2);
            let record = loop {
                let record = network_records(
                    state
                        .diagnostics()
                        .lock()
                        .unwrap()
                        .network_events(&target.id, None),
                )
                .into_iter()
                .find(|record| record.public_id == request_id)
                .ok_or_else(|| BrowserToolError::invalid_input("unknown network request_id"))?;
                if !include_response_body
                    || record.finished_at_ms.is_some()
                    || record.failed
                    || Instant::now() >= deadline
                {
                    break record;
                }
                sleep(Duration::from_millis(20)).await;
            };
            let mut result = json!({
                "operation":"get",
                "request":network_record_value(&record),
                "request_headers":record.request_headers,
                "response_headers":record.response_headers,
                "timing":{},
                "initiator":{}
            });
            if params
                .arguments
                .get("include_request_body")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                && let Some(body) = state
                    .browser
                    .network_request_body(&target, &record.raw_id)
                    .await?
            {
                result["request_body"] = Value::String(body);
            }
            if include_response_body {
                let body = state
                    .browser
                    .network_response_body(&target, &record.raw_id)
                    .await?;
                let limit = params
                    .arguments
                    .get("body_limit_bytes")
                    .and_then(Value::as_u64)
                    .unwrap_or(65_536)
                    .clamp(1, 1_048_576) as usize;
                if body.len() <= limit {
                    result["response_body"] = Value::String(String::from_utf8_lossy(&body).into());
                } else {
                    let artifact = state.artifacts().lock().unwrap().insert_bytes(
                        &params.agent_session_id,
                        Some(tab_id),
                        "network",
                        record
                            .mime_type
                            .as_deref()
                            .unwrap_or("application/octet-stream"),
                        "bin",
                        &body,
                    )?;
                    result["body_artifact"] = serde_json::to_value(artifact)
                        .map_err(|error| BrowserToolError::artifact_error(error.to_string()))?;
                }
            }
            Ok(result)
        }
        "clear" => {
            state
                .diagnostics()
                .lock()
                .unwrap()
                .clear_network(&target.id);
            Ok(json!({"operation":"clear", "cleared":true}))
        }
        operation => Err(BrowserToolError::invalid_input(format!(
            "unknown network operation `{operation}`"
        ))),
    }
}

async fn broker_emulation(
    state: &BrokerState,
    params: Result<DomainParams, BrowserToolError>,
) -> Result<Value, BrowserToolError> {
    let params = params?;
    let tab_id = params
        .tab_id
        .as_ref()
        .ok_or_else(|| BrowserToolError::invalid_input("emulation requires tab_id"))?;
    let target = active_owned_target(state, &params.agent_session_id, tab_id).await?;
    let effective = state
        .browser
        .emulate(&target, &params.operation, &params.arguments)
        .await?;
    match params.operation.as_str() {
        "set_viewport" => {
            state
                .viewport_overrides
                .lock()
                .unwrap()
                .insert(target.id.clone(), params.arguments.clone());
        }
        "reset" => {
            state.viewport_overrides.lock().unwrap().remove(&target.id);
        }
        _ => {}
    }
    Ok(json!({"operation":params.operation, "effective":effective}))
}

async fn broker_artifacts(
    state: &BrokerState,
    params: Result<DomainParams, BrowserToolError>,
) -> Result<Value, BrowserToolError> {
    let params = params?;
    state
        .registry()
        .lock()
        .unwrap()
        .ensure_session(&params.agent_session_id)?;
    match params.operation.as_str() {
        "list" => {
            let tab_id = params
                .arguments
                .get("tab_id")
                .and_then(Value::as_str)
                .map(|tab_id| TabId(tab_id.to_string()));
            let kinds = optional_string_array_argument(&params.arguments, "kinds")?;
            let offset = params
                .arguments
                .get("cursor")
                .and_then(Value::as_str)
                .map(|cursor| parse_cursor(cursor, "artifacts_"))
                .transpose()?
                .unwrap_or(0);
            let limit = params
                .arguments
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(100)
                .clamp(1, 500) as usize;
            let artifacts = state.artifacts().lock().unwrap().list(
                &params.agent_session_id,
                tab_id.as_ref(),
                &kinds,
            );
            let next =
                (offset + limit < artifacts.len()).then(|| format!("artifacts_{}", offset + limit));
            Ok(json!({
                "operation":"list",
                "artifacts":artifacts.into_iter().skip(offset).take(limit).collect::<Vec<_>>(),
                "next_cursor":next
            }))
        }
        "metadata" => {
            let artifact = state.artifacts().lock().unwrap().metadata(
                &params.agent_session_id,
                string_argument(&params.arguments, "artifact_id")?,
            )?;
            Ok(json!({"operation":"metadata", "artifact":artifact}))
        }
        "read" => {
            let artifact_id = string_argument(&params.arguments, "artifact_id")?;
            let offset = params
                .arguments
                .get("offset")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            let length = params
                .arguments
                .get("length")
                .and_then(Value::as_u64)
                .unwrap_or(65_536) as usize;
            let artifacts = state.artifacts().lock().unwrap();
            let artifact = artifacts.metadata(&params.agent_session_id, artifact_id)?;
            let (bytes, more) =
                artifacts.read(&params.agent_session_id, artifact_id, offset, length)?;
            Ok(json!({
                "operation":"read",
                "artifact":artifact,
                "offset":offset,
                "data_base64":BASE64.encode(bytes),
                "eof":!more
            }))
        }
        "export" => {
            let artifact_id = string_argument(&params.arguments, "artifact_id")?;
            let requested = Path::new(string_argument(&params.arguments, "path")?);
            let workspace_root = state
                .registry()
                .lock()
                .unwrap()
                .session(&params.agent_session_id)
                .and_then(|session| session.workspace_root.clone())
                .ok_or_else(|| {
                    BrowserToolError::workspace_unavailable(
                        "this browser session has no host workspace root",
                    )
                })?;
            let artifacts = state.artifacts().lock().unwrap();
            let artifact = artifacts.metadata(&params.agent_session_id, artifact_id)?;
            let path = artifacts.export(
                &params.agent_session_id,
                artifact_id,
                &workspace_root,
                requested,
                params
                    .arguments
                    .get("overwrite")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            )?;
            Ok(json!({"operation":"export", "artifact":artifact, "path":path}))
        }
        "delete" => {
            let artifact_id = string_argument(&params.arguments, "artifact_id")?;
            state
                .artifacts()
                .lock()
                .unwrap()
                .delete(&params.agent_session_id, artifact_id)?;
            Ok(json!({"operation":"delete", "deleted":true}))
        }
        operation => Err(BrowserToolError::invalid_input(format!(
            "unknown artifacts operation `{operation}`"
        ))),
    }
}

async fn broker_performance(
    state: &BrokerState,
    params: Result<DomainParams, BrowserToolError>,
) -> Result<Value, BrowserToolError> {
    let params = params?;
    let tab_id = params
        .tab_id
        .as_ref()
        .ok_or_else(|| BrowserToolError::invalid_input("performance requires tab_id"))?;
    let target = active_owned_target(state, &params.agent_session_id, tab_id).await?;
    match params.operation.as_str() {
        "start_trace" => {
            let mut traces = state.traces.lock().await;
            if !traces.is_empty() {
                return Err(BrowserToolError::invalid_input(
                    "a browser-wide performance trace is already recording",
                ));
            }
            let categories = optional_string_array_argument(&params.arguments, "categories")?;
            let capture = state
                .browser
                .start_trace(
                    categories,
                    params
                        .arguments
                        .get("screenshots")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                )
                .await?;
            traces.insert(target.id.clone(), capture);
            drop(traces);
            if params
                .arguments
                .get("reload")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                state
                    .browser
                    .reload(
                        &target,
                        false,
                        Some("load"),
                        DEFAULT_NAVIGATION_TIMEOUT_MS,
                        None,
                    )
                    .await?;
            }
            Ok(json!({"operation":"start_trace", "recording":true}))
        }
        "stop_trace" => {
            let capture = state
                .traces
                .lock()
                .await
                .remove(&target.id)
                .ok_or_else(|| {
                    BrowserToolError::invalid_input("no performance trace is recording")
                })?;
            let events = BrowserBackend::stop_trace(capture).await?;
            let bytes = serde_json::to_vec(&json!({"traceEvents":events}))
                .map_err(|error| BrowserToolError::artifact_error(error.to_string()))?;
            let artifact = state.artifacts().lock().unwrap().insert_bytes(
                &params.agent_session_id,
                Some(tab_id),
                "trace",
                "application/json",
                "trace.json",
                &bytes,
            )?;
            Ok(json!({"operation":"stop_trace", "recording":false, "artifact":artifact}))
        }
        "vitals" => {
            let result = state.browser.evaluate(&target, r#"(() => {
  const entries = performance.getEntries();
  const navigation = performance.getEntriesByType('navigation')[0];
  const paint = Object.fromEntries(performance.getEntriesByType('paint').map(e => [e.name, e.startTime]));
  const lcp = performance.getEntriesByType('largest-contentful-paint').at(-1);
  const shifts = performance.getEntriesByType('layout-shift').filter(e => !e.hadRecentInput);
  return {
    FCP: paint['first-contentful-paint'] ?? null,
    LCP: lcp?.startTime ?? null,
    CLS: shifts.reduce((sum, e) => sum + e.value, 0),
    TTFB: navigation ? navigation.responseStart : null,
    DOMContentLoaded: navigation ? navigation.domContentLoadedEventEnd : null,
    Load: navigation ? navigation.loadEventEnd : null,
    resource_count: entries.filter(e => e.entryType === 'resource').length
  };
})()"#).await?;
            Ok(json!({"operation":"vitals", "metrics":result.value.unwrap_or_else(|| json!({}))}))
        }
        "analyze" => {
            let artifact_id = string_argument(&params.arguments, "artifact_id")?;
            let artifact = state
                .artifacts()
                .lock()
                .unwrap()
                .metadata(&params.agent_session_id, artifact_id)?;
            if artifact.kind != "trace" {
                return Err(BrowserToolError::invalid_input(
                    "performance analyze requires a trace artifact",
                ));
            }
            if artifact.size_bytes > MAX_ANALYZABLE_TRACE_BYTES {
                return Err(BrowserToolError::artifact_error(format!(
                    "trace artifact is {} bytes; in-process analysis accepts at most {} bytes",
                    artifact.size_bytes, MAX_ANALYZABLE_TRACE_BYTES
                )));
            }
            let bytes = state
                .artifacts()
                .lock()
                .unwrap()
                .bytes(&params.agent_session_id, artifact_id)?;
            let trace: Value = serde_json::from_slice(&bytes)
                .map_err(|error| BrowserToolError::artifact_error(error.to_string()))?;
            let insight = params
                .arguments
                .get("insight")
                .and_then(Value::as_str)
                .unwrap_or("overview");
            let limit = params
                .arguments
                .get("max_findings")
                .and_then(Value::as_u64)
                .unwrap_or(20)
                .clamp(1, 100) as usize;
            let findings = analyze_trace(&trace, insight, limit)?;
            Ok(json!({"operation":"analyze", "artifact":artifact, "findings":findings}))
        }
        operation => Err(BrowserToolError::invalid_input(format!(
            "unknown performance operation `{operation}`"
        ))),
    }
}

fn analyze_trace(
    trace: &Value,
    insight: &str,
    limit: usize,
) -> Result<Vec<Value>, BrowserToolError> {
    let allowed = [
        "overview",
        "long_tasks",
        "script_execution",
        "style_layout",
        "paint",
        "network",
        "dominant_slices",
    ];
    if !allowed.contains(&insight) {
        return Err(BrowserToolError::invalid_input(format!(
            "unknown performance insight `{insight}`"
        )));
    }
    let events = trace
        .get("traceEvents")
        .and_then(Value::as_array)
        .ok_or_else(|| BrowserToolError::invalid_input("trace artifact omits traceEvents"))?;
    let mut slices = events
        .iter()
        .filter(|event| event.get("ph").and_then(Value::as_str) == Some("X"))
        .filter_map(|event| {
            let name = event.get("name")?.as_str()?.to_string();
            let category = event
                .get("cat")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let duration_us = event.get("dur").and_then(Value::as_f64).unwrap_or(0.0);
            Some((name, category, duration_us))
        })
        .filter(|(name, category, duration)| match insight {
            "long_tasks" => *duration >= 50_000.0,
            "script_execution" => {
                category.contains("v8") || name.contains("Script") || name.contains("Function")
            }
            "style_layout" => matches!(
                name.as_str(),
                "Layout" | "RecalculateStyles" | "UpdateLayoutTree"
            ),
            "paint" => name.contains("Paint") || name.contains("Composite"),
            "network" => category.contains("loading") || name.contains("Resource"),
            _ => true,
        })
        .collect::<Vec<_>>();
    slices.sort_by(|left, right| right.2.total_cmp(&left.2));
    Ok(slices
        .into_iter()
        .take(limit)
        .map(|(name, category, duration_us)| {
            let duration_ms = duration_us / 1_000.0;
            json!({
                "name":name,
                "severity":if duration_ms >= 100.0 {"warning"} else {"info"},
                "summary":format!("{name} occupied {duration_ms:.2} ms"),
                "evidence":{"category":category,"duration_ms":duration_ms,"insight":insight}
            })
        })
        .collect())
}

async fn broker_audit(
    state: &BrokerState,
    params: Result<DomainParams, BrowserToolError>,
) -> Result<Value, BrowserToolError> {
    let params = params?;
    if params.operation != "run" {
        return Err(BrowserToolError::invalid_input(format!(
            "unknown audit operation `{}`",
            params.operation
        )));
    }
    let tab_id = params
        .tab_id
        .as_ref()
        .ok_or_else(|| BrowserToolError::invalid_input("audit requires tab_id"))?;
    let target = active_owned_target(state, &params.agent_session_id, tab_id).await?;
    let mode = params
        .arguments
        .get("mode")
        .and_then(Value::as_str)
        .unwrap_or("snapshot");
    if !matches!(mode, "navigation" | "snapshot") {
        return Err(BrowserToolError::invalid_input(format!(
            "unknown audit mode `{mode}`"
        )));
    }
    let device = params
        .arguments
        .get("device")
        .and_then(Value::as_str)
        .unwrap_or("desktop");
    if !matches!(device, "desktop" | "mobile") {
        return Err(BrowserToolError::invalid_input(format!(
            "unknown audit device `{device}`"
        )));
    }
    let categories = {
        let requested = optional_string_array_argument(&params.arguments, "categories")?;
        if requested.is_empty() {
            vec![
                "accessibility".to_string(),
                "seo".to_string(),
                "best_practices".to_string(),
                "agentic_browsing".to_string(),
            ]
        } else {
            requested
        }
    };
    let requested = serde_json::to_string(&categories)
        .map_err(|error| BrowserToolError::invalid_input(error.to_string()))?;
    let expression = format!(
        r#"(() => {{
  const requested = new Set({requested});
  const findings = [];
  const selector = e => {{
    if (!e) return null;
    if (e.id) return `#${{CSS.escape(e.id)}}`;
    const parts = [];
    for (let node = e; node && node !== document.documentElement; node = node.parentElement) {{
      let part = node.localName;
      const siblings = node.parentElement ? [...node.parentElement.children].filter(child => child.localName === node.localName) : [];
      if (siblings.length > 1) part += `:nth-of-type(${{siblings.indexOf(node) + 1}})`;
      parts.unshift(part);
    }}
    return parts.join(" > ");
  }};
  const add = (id, category, title, description, element = null) => findings.push({{id, category, title, description, refs:[], selector:selector(element)}});
  const visible = e => {{ const r=e.getBoundingClientRect(),s=getComputedStyle(e); return r.width>0&&r.height>0&&s.visibility!=="hidden"&&s.display!=="none"; }};
  const name = e => (e.getAttribute("aria-label") || e.getAttribute("aria-labelledby") || e.alt || e.title || e.textContent || "").trim();
  if (requested.has("accessibility")) {{
    document.querySelectorAll("img:not([alt])").forEach(e => add("image-alt", "accessibility", "Image has no alt attribute", "Provide alt text or an empty alt attribute for decorative images.", e));
    document.querySelectorAll("button,input,select,textarea,a[href],[role=button]").forEach(e => {{ if (visible(e) && !name(e)) add("control-name", "accessibility", "Interactive control has no accessible name", "Give the control a label or accessible name.", e); }});
    const ids = [...document.querySelectorAll("[id]")].map(e => e.id); if (new Set(ids).size !== ids.length) add("duplicate-id", "accessibility", "Document contains duplicate ids", "Use unique id values for label and accessibility relationships.");
  }}
  if (requested.has("seo")) {{
    if (!document.title.trim()) add("document-title", "seo", "Document title is empty", "Provide a concise page title.");
    if (!document.querySelector('meta[name="description"]')?.content?.trim()) add("meta-description", "seo", "Meta description is missing", "Provide a page description for search results.");
    if (document.querySelectorAll("h1").length !== 1) add("primary-heading", "seo", "Page should have one primary heading", "Provide exactly one h1 that names the page.");
    if (!document.documentElement.lang) add("document-language", "seo", "Document language is missing", "Set the html lang attribute.");
  }}
  if (requested.has("best_practices")) {{
    if (location.protocol === "https:" && [...document.querySelectorAll("img,script,link")].some(e => (e.src || e.href || "").startsWith("http:"))) add("mixed-content", "best_practices", "Page requests insecure content", "Serve subresources over HTTPS.");
    document.querySelectorAll('input[type="password"]:not([autocomplete])').forEach(e => add("password-autocomplete", "best_practices", "Password field omits autocomplete", "Declare the appropriate current-password or new-password value.", e));
  }}
  if (requested.has("agentic_browsing")) {{
    document.querySelectorAll("button,input,select,textarea,a[href],[role=button],[contenteditable=true]").forEach(e => {{ if (visible(e) && !name(e)) add("unnamed-action", "agentic_browsing", "Action cannot be selected semantically", "Give the action a stable accessible name.", e); }});
    [...document.querySelectorAll("a[href]")].filter(e => /^(click here|more|learn more)$/i.test(e.textContent.trim())).forEach(e => add("generic-link", "agentic_browsing", "Link text does not identify its destination", "Use link text that names the destination or action.", e));
  }}
  return {{findings}};
}})()"#
    );
    let prior_viewport = (device == "mobile")
        .then(|| {
            state
                .viewport_overrides
                .lock()
                .unwrap()
                .get(&target.id)
                .cloned()
        })
        .flatten();
    if device == "mobile" {
        state
            .browser
            .emulate(
                &target,
                "set_viewport",
                &serde_json::Map::from_iter([
                    ("width".to_string(), json!(390)),
                    ("height".to_string(), json!(844)),
                    ("device_scale_factor".to_string(), json!(3.0)),
                    ("mobile".to_string(), json!(true)),
                    ("touch".to_string(), json!(true)),
                    ("orientation".to_string(), json!("portrait")),
                ]),
            )
            .await?;
    }
    let audit_result = async {
        if mode == "navigation" {
            state
                .browser
                .reload(
                    &target,
                    false,
                    Some("load"),
                    DEFAULT_NAVIGATION_TIMEOUT_MS,
                    None,
                )
                .await?;
        }
        let (snapshot, _) = snapshot_for_target(
            state,
            &params.agent_session_id,
            tab_id,
            &target,
            SnapshotRequest {
                mode: SnapshotMode::Full,
                root_backend_node_id: None,
                depth: 64,
                max_nodes: 5_000,
                include_hidden: false,
                include_bounds: false,
            },
        )
        .await?;
        let result = state.browser.evaluate(&target, &expression).await?;
        Ok::<_, BrowserToolError>((snapshot.document_revision, result))
    }
    .await;
    let reset_result = if let Some(prior_viewport) = prior_viewport {
        state
            .browser
            .emulate(&target, "set_viewport", &prior_viewport)
            .await
            .map(|_| ())
    } else if device == "mobile" {
        state
            .browser
            .emulate(&target, "reset_viewport", &serde_json::Map::new())
            .await
            .map(|_| ())
    } else {
        Ok(())
    };
    let (document_revision, result) = audit_result?;
    let result = result.value.unwrap_or_else(|| json!({"findings":[]}));
    reset_result?;
    let mut findings = result
        .get("findings")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for finding in &mut findings {
        let reference = if let Some(selector) = finding.get("selector").and_then(Value::as_str) {
            match state
                .browser
                .resolve_css_backend_node(&target, selector)
                .await
            {
                Ok(backend_node_id) => state
                    .references()
                    .lock()
                    .unwrap()
                    .reference_for_backend_node(
                        &params.agent_session_id,
                        tab_id,
                        backend_node_id,
                        &document_revision,
                    ),
                Err(_) => None,
            }
        } else {
            None
        };
        if let Some(object) = finding.as_object_mut() {
            object.remove("selector");
            object.insert(
                "refs".to_string(),
                reference.map_or_else(|| json!([]), |reference| json!([reference])),
            );
        }
    }
    let mut scores = serde_json::Map::new();
    for category in &categories {
        let count = findings
            .iter()
            .filter(|finding| finding.get("category").and_then(Value::as_str) == Some(category))
            .count();
        scores.insert(category.clone(), json!((1.0 - count as f64 * 0.1).max(0.0)));
    }
    let report = json!({"scores":scores,"findings":findings,"url":target.url});
    let bytes = serde_json::to_vec_pretty(&report)
        .map_err(|error| BrowserToolError::artifact_error(error.to_string()))?;
    let artifact = state.artifacts().lock().unwrap().insert_bytes(
        &params.agent_session_id,
        Some(tab_id),
        "audit",
        "application/json",
        "json",
        &bytes,
    )?;
    Ok(json!({
        "operation":"run",
        "scores":report["scores"],
        "findings":report["findings"],
        "reports":[artifact]
    }))
}

async fn broker_memory(
    state: &BrokerState,
    params: Result<DomainParams, BrowserToolError>,
) -> Result<Value, BrowserToolError> {
    let params = params?;
    let tab_id = params
        .tab_id
        .as_ref()
        .ok_or_else(|| BrowserToolError::invalid_input("memory requires tab_id"))?;
    let target = active_owned_target(state, &params.agent_session_id, tab_id).await?;
    if params.operation == "capture" {
        let bytes = state.browser.heap_snapshot(&target).await?;
        let artifact = state.artifacts().lock().unwrap().insert_bytes(
            &params.agent_session_id,
            Some(tab_id),
            "heap_snapshot",
            "application/json",
            "heapsnapshot",
            &bytes,
        )?;
        return Ok(json!({"operation":"capture", "artifact":artifact}));
    }
    let artifact_id = string_argument(&params.arguments, "artifact_id")?;
    let artifact = state
        .artifacts()
        .lock()
        .unwrap()
        .metadata(&params.agent_session_id, artifact_id)?;
    if artifact.kind != "heap_snapshot" {
        return Err(BrowserToolError::invalid_input(
            "memory operations require a heap_snapshot artifact",
        ));
    }
    if params.operation == "close" {
        state
            .artifacts()
            .lock()
            .unwrap()
            .delete(&params.agent_session_id, artifact_id)?;
        return Ok(json!({"operation":"close", "closed":true}));
    }
    let bytes = state
        .artifacts()
        .lock()
        .unwrap()
        .bytes(&params.agent_session_id, artifact_id)?;
    let graph = crate::heap::HeapGraph::parse(&bytes)?;
    let cursor = params
        .arguments
        .get("cursor")
        .and_then(Value::as_str)
        .map(|cursor| parse_cursor(cursor, "memory_"))
        .transpose()?
        .unwrap_or(0);
    let limit = params
        .arguments
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, 500) as usize;
    let data = match params.operation.as_str() {
        "summary" => graph.summary(),
        "classes" => Value::Array(
            graph.classes(
                params.arguments.get("class_name").and_then(Value::as_str),
                params
                    .arguments
                    .get("min_retained_bytes")
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            ),
        ),
        "node" => graph.node(string_argument(&params.arguments, "node_id")?)?,
        "dominators" => {
            Value::Array(graph.dominators(params.arguments.get("node_id").and_then(Value::as_str))?)
        }
        "retainers" => {
            Value::Array(graph.retainers(string_argument(&params.arguments, "node_id")?)?)
        }
        "retaining_paths" => Value::Array(
            graph.retaining_paths(
                string_argument(&params.arguments, "node_id")?,
                params
                    .arguments
                    .get("max_depth")
                    .and_then(Value::as_u64)
                    .unwrap_or(12)
                    .clamp(1, 64) as usize,
                limit,
            )?,
        ),
        "edges" => Value::Array(graph.edges(
            string_argument(&params.arguments, "node_id")?,
            params.arguments.get("direction").and_then(Value::as_str) == Some("incoming"),
        )?),
        operation => {
            return Err(BrowserToolError::invalid_input(format!(
                "unknown memory operation `{operation}`"
            )));
        }
    };
    let (data, truncated, next_cursor) = paginate_value(data, cursor, limit);
    Ok(json!({
        "operation":params.operation,
        "artifact":artifact,
        "data":data,
        "truncated":truncated,
        "next_cursor":next_cursor
    }))
}

async fn broker_screencast(
    state: &BrokerState,
    params: Result<DomainParams, BrowserToolError>,
) -> Result<Value, BrowserToolError> {
    let params = params?;
    let tab_id = params
        .tab_id
        .as_ref()
        .ok_or_else(|| BrowserToolError::invalid_input("screencast requires tab_id"))?;
    let target = active_owned_target(state, &params.agent_session_id, tab_id).await?;
    match params.operation.as_str() {
        "start" => {
            let fps = params
                .arguments
                .get("fps")
                .and_then(Value::as_u64)
                .unwrap_or(10)
                .clamp(1, 30) as u32;
            let quality = params
                .arguments
                .get("quality")
                .and_then(Value::as_u64)
                .unwrap_or(70)
                .clamp(1, 100) as u8;
            let max_duration_ms = params
                .arguments
                .get("max_duration_ms")
                .and_then(Value::as_u64)
                .unwrap_or(30_000)
                .clamp(1_000, 300_000);
            let mut captures = state.screencasts.lock().await;
            if captures.contains_key(&target.id) {
                return Err(BrowserToolError::invalid_input(
                    "a screencast is already recording for this tab",
                ));
            }
            let started_at_ms = current_time_ms();
            let capture = state
                .browser
                .start_screencast(
                    &target,
                    fps,
                    quality,
                    Duration::from_millis(max_duration_ms),
                )
                .await?;
            captures.insert(
                target.id,
                ActiveScreencast {
                    capture,
                    started_at_ms,
                    fps,
                    quality,
                },
            );
            Ok(json!({"operation":"start", "recording":true, "started_at_ms":started_at_ms}))
        }
        "status" => {
            let captures = state.screencasts.lock().await;
            let active = captures.get(&target.id);
            Ok(json!({
                "operation":"status",
                "recording":active.is_some(),
                "started_at_ms":active.map(|capture| capture.started_at_ms)
            }))
        }
        "stop" => {
            let active = state
                .screencasts
                .lock()
                .await
                .remove(&target.id)
                .ok_or_else(|| BrowserToolError::invalid_input("no screencast is recording"))?;
            let frames = BrowserBackend::stop_screencast(active.capture).await?;
            let fps = active.fps;
            let quality = active.quality;
            let bytes = tokio::task::spawn_blocking(move || {
                crate::video::encode_silent_webm(&frames, fps, quality)
            })
            .await
            .map_err(|error| {
                BrowserToolError::artifact_error(format!("screencast encoder failed: {error}"))
            })??;
            let artifact = state.artifacts().lock().unwrap().insert_bytes(
                &params.agent_session_id,
                Some(tab_id),
                "screencast",
                "video/webm",
                "webm",
                &bytes,
            )?;
            Ok(json!({"operation":"stop", "recording":false, "artifact":artifact}))
        }
        operation => Err(BrowserToolError::invalid_input(format!(
            "unknown screencast operation `{operation}`"
        ))),
    }
}

fn current_time_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn paginate_value(value: Value, cursor: usize, limit: usize) -> (Value, bool, Option<String>) {
    let Value::Array(values) = value else {
        return (value, false, None);
    };
    let truncated = cursor.saturating_add(limit) < values.len();
    let next_cursor = truncated.then(|| format!("memory_{}", cursor + limit));
    (
        Value::Array(values.into_iter().skip(cursor).take(limit).collect()),
        truncated,
        next_cursor,
    )
}

fn parse_cursor(cursor: &str, prefix: &str) -> Result<usize, BrowserToolError> {
    cursor
        .strip_prefix(prefix)
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| BrowserToolError::invalid_input(format!("invalid cursor `{cursor}`")))
}

fn parse_scoped_sequence(value: &str, prefix: &str) -> Result<u64, BrowserToolError> {
    value
        .strip_prefix(prefix)
        .and_then(|sequence| sequence.parse().ok())
        .ok_or_else(|| BrowserToolError::invalid_input(format!("invalid identifier `{value}`")))
}

#[cfg(test)]
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

#[cfg(test)]
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
    if state
        .screencasts
        .lock()
        .await
        .contains_key(&lease.target_id)
    {
        return Err(BrowserToolError::invalid_input(
            "stop the active screencast before closing its tab",
        ));
    }
    if state.traces.lock().await.contains_key(&lease.target_id) {
        return Err(BrowserToolError::invalid_input(
            "stop the active performance trace before closing its tab",
        ));
    }

    if matches!(lease.state, LeaseState::Active) {
        match target_by_id(state, &lease.target_id).await {
            Ok(target) => {
                state
                    .browser
                    .emulate(&target, "reset", &serde_json::Map::new())
                    .await?;
                state.browser.close_target(&lease.target_id).await?;
            }
            Err(error) if error.code == crate::leases::BrowserToolErrorCode::TargetMissing => {}
            Err(error) => return Err(error),
        }
    }
    state
        .viewport_overrides
        .lock()
        .unwrap()
        .remove(&lease.target_id);

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
    state.references().lock().unwrap().reset_tab(&params.tab_id);

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
    reconcile_missing_targets(state, &targets).await;
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

async fn reconcile_missing_targets(state: &BrokerState, targets: &[CdpTarget]) {
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
        for lease in missing {
            if let Some(capture) = state.traces.lock().await.remove(&lease.target_id) {
                let _ = BrowserBackend::stop_trace(capture).await;
            }
            if let Some(active) = state.screencasts.lock().await.remove(&lease.target_id) {
                drop(active);
            }
            state.clear_focused_target(&lease.target_id);
            state
                .viewport_overrides
                .lock()
                .unwrap()
                .remove(&lease.target_id);
            state
                .diagnostics()
                .lock()
                .unwrap()
                .reset_target(&lease.target_id);
            state.references().lock().unwrap().reset_tab(&lease.tab_id);
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

fn update_owned_target_snapshot(
    state: &BrokerState,
    tab_id: &TabId,
    target: &CdpTarget,
) -> Result<(), BrowserToolError> {
    let focused = state.is_focused_target(&target.id);
    state.registry().lock().unwrap().update_tab_snapshot(
        tab_id,
        tab_snapshot(target, focused.then_some(target.id.as_str())),
    )?;
    Ok(())
}

fn tab_snapshot(target: &CdpTarget, focused_target_id: Option<&str>) -> TabSnapshot {
    let mut snapshot = TabSnapshot::from(target);
    snapshot.focused = focused_target_id == Some(target.id.as_str());
    snapshot
}

async fn broker_status(
    config: &RuntimeConfig,
    state: &BrokerState,
) -> Result<BrokerStatus, BrowserToolError> {
    Ok(BrokerStatus {
        protocol_version: BROKER_PROTOCOL_VERSION,
        pid: std::process::id(),
        runtime_mode: config.runtime_mode,
        cdp_endpoint: state.browser.resolved_endpoint().await?,
        ipc_endpoint: config.ipc_endpoint.clone(),
        socket_path: config.socket_path.clone(),
    })
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

        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
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

async fn terminate_process(pid: u32) -> Result<()> {
    if pid == 0 {
        return Ok(());
    }
    if pid == std::process::id() {
        bail!("refusing to terminate the current process as an incompatible broker");
    }

    #[cfg(unix)]
    {
        let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
        if result != 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error).with_context(|| format!("failed to terminate broker pid {pid}"));
            }
        }
        wait_for_process_exit(pid, Duration::from_secs(2)).await;
        if process_is_alive(pid) {
            let result = unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) };
            if result != 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error).with_context(|| format!("failed to kill broker pid {pid}"));
                }
            }
            wait_for_process_exit(pid, Duration::from_secs(2)).await;
        }
    }

    #[cfg(windows)]
    {
        let output = Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .output()
            .with_context(|| format!("failed to invoke taskkill for broker pid {pid}"))?;
        if !output.status.success() && process_is_alive(pid) {
            bail!(
                "failed to terminate broker pid {pid}: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        wait_for_process_exit(pid, Duration::from_secs(2)).await;
    }

    if process_is_alive(pid) {
        bail!("broker pid {pid} did not exit after termination");
    }
    Ok(())
}

async fn wait_for_process_exit(pid: u32, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while process_is_alive(pid) && Instant::now() < deadline {
        sleep(Duration::from_millis(50)).await;
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
            .truncate(false)
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
        }
    }

    fn fake_state(targets: Vec<CdpTarget>) -> BrokerState {
        BrokerState::with_browser(BrowserBackend::Fake(Arc::new(Mutex::new(
            FakeBrowser::with_targets(targets),
        ))))
    }

    #[test]
    fn normalized_endpoint_omits_the_url_root_slash() {
        let client = CdpClient::new("http://127.0.0.1:9222/").unwrap();

        assert_eq!(normalized_endpoint(&client), "http://127.0.0.1:9222");
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
        assert_eq!(status.runtime_mode, RuntimeMode::External);
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
                workspace_root: None,
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
    async fn start_session_accepts_file_url_workspace_root() {
        let state = fake_state(Vec::new());
        let workspace = tempfile::Builder::new()
            .prefix("workspace root ")
            .tempdir()
            .unwrap();
        let canonical_workspace = workspace.path().canonicalize().unwrap();
        let workspace_url = Url::from_directory_path(workspace.path()).unwrap();

        let result = broker_start_session(
            &state,
            Ok(StartSessionParams {
                label: Some("agent".to_string()),
                start_url: None,
                focus: false,
                workspace_root: Some(PathBuf::from(workspace_url.as_str())),
            }),
        )
        .await
        .unwrap();

        let registry = state.registry.lock().unwrap();
        let session = registry.session(&result.agent_session_id).unwrap();
        assert_eq!(
            session.workspace_root.as_deref(),
            Some(canonical_workspace.as_path())
        );
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
                workspace_root: None,
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
                workspace_root: None,
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
                workspace_root: None,
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
                workspace_root: None,
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
                target: None,
                format: None,
                quality: None,
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
                workspace_root: None,
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
                workspace_root: None,
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
                workspace_root: None,
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

        let snapshot = broker_snapshot(
            &state,
            Ok(SnapshotParams {
                agent_session_id: owner.agent_session_id.clone(),
                tab_id: tab.tab_id.clone(),
                mode: Some(SnapshotMode::Meaningful),
                root: None,
                depth: None,
                max_nodes: None,
                include_hidden: false,
                include_bounds: false,
            }),
        )
        .await
        .unwrap();
        assert!(snapshot.tree.contains("button \"Submit\" [ref=e_2]"));
        assert!(snapshot.tree.contains("textbox \"Email\" [ref=e_3]"));

        broker_fill(
            &state,
            Ok(FillParams {
                agent_session_id: owner.agent_session_id.clone(),
                tab_id: tab.tab_id.clone(),
                target: ElementTarget::Reference(ElementReferenceTarget {
                    reference: "e_3".to_string(),
                }),
                value: "person@example.test".to_string(),
                timeout_ms: None,
                observe: Some(ObservationMode::None),
            }),
        )
        .await
        .unwrap();

        let clicked = broker_click(
            &state,
            Ok(ClickParams {
                agent_session_id: owner.agent_session_id.clone(),
                tab_id: tab.tab_id.clone(),
                target: ElementTarget::Reference(ElementReferenceTarget {
                    reference: "e_2".to_string(),
                }),
                button: None,
                count: None,
                modifiers: Vec::new(),
                timeout_ms: None,
                observe: Some(ObservationMode::None),
            }),
        )
        .await
        .unwrap();
        assert!(matches!(clicked.observation, Observation::None));
        let action = clicked.action.as_ref().unwrap();
        assert_eq!(action.delivery_mode, "semantic_dom_activation");
        assert_eq!(action.release_delivery, "chrome_ack");
        assert!(action.effect.url_changed);
        assert_eq!(action.effect.post_url, "fake://semantic-submit");
        assert_eq!(
            action
                .resolved_element
                .as_ref()
                .and_then(|element| element.get("backend_node_id"))
                .and_then(Value::as_i64),
            Some(2)
        );

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

        let raw_key_error = broker_press_key_v3(
            &state,
            Ok(V3PressKeyParams {
                agent_session_id: owner.agent_session_id.clone(),
                tab_id: tab.tab_id.clone(),
                target: None,
                key: "Enter".to_string(),
                modifiers: Vec::new(),
                timeout_ms: None,
                observe: Some(ObservationMode::None),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(
            raw_key_error.code,
            crate::leases::BrowserToolErrorCode::FocusRequired
        );

        let raw_click_error = broker_interact(
            &state,
            Ok(serde_json::from_value(json!({
                "agent_session_id": owner.agent_session_id,
                "tab_id": tab.tab_id.clone(),
                "operation": "click_at",
                "x": 2,
                "y": 2,
                "button": "left",
                "count": 1,
                "observe": "none"
            }))
            .unwrap()),
        )
        .await
        .unwrap_err();
        assert_eq!(
            raw_click_error.code,
            crate::leases::BrowserToolErrorCode::FocusRequired
        );

        {
            let fake = fake.lock().unwrap();
            assert_eq!(fake.clicked_backend_nodes, vec![2]);
            assert_eq!(fake.semantic_activated_backend_nodes, vec![2]);
            assert_eq!(
                fake.filled_backend_nodes,
                vec![(3, "person@example.test".to_string())]
            );
            assert_eq!(fake.typed_text(), &["hello".to_string()]);
            assert_eq!(fake.pressed_keys(), &["Enter".to_string()]);
            assert_eq!(
                fake.prepared_targets,
                vec![
                    "target-a".to_string(),
                    "target-a".to_string(),
                    "target-a".to_string(),
                    "target-a".to_string()
                ]
            );
            assert_eq!(
                fake.focused_target_id, None,
                "routine click and key actions must not activate the target"
            );
        }

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
                workspace_root: None,
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
                    request_id: Some("request-1".to_string()),
                    url: Some("https://example.com/data.json".to_string()),
                    method: Some("GET".to_string()),
                    resource_type: Some("Fetch".to_string()),
                    mime_type: None,
                    headers: std::collections::BTreeMap::new(),
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
    fn broker_status_must_match_requested_runtime() {
        let tempdir = tempfile::tempdir().unwrap();
        let managed_config = RuntimeConfig::managed(tempdir.path().join("state"), None);
        let external_config = RuntimeConfig::from_parts(
            "http://127.0.0.1:9222".to_string(),
            managed_config.state_dir.clone(),
        )
        .unwrap();
        let status = BrokerStatus {
            protocol_version: BROKER_PROTOCOL_VERSION,
            pid: 123,
            runtime_mode: RuntimeMode::External,
            cdp_endpoint: "http://127.0.0.1:9222".to_string(),
            ipc_endpoint: managed_config.ipc_endpoint.clone(),
            socket_path: managed_config.socket_path.clone(),
        };

        let message = broker_status_mismatch(&managed_config, &status)
            .unwrap()
            .expect("managed startup should reject an external broker");
        assert!(message.contains("broker runtime mismatch"));

        assert!(
            broker_status_mismatch(&external_config, &status)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn external_broker_status_must_match_requested_cdp_endpoint() {
        let tempdir = tempfile::tempdir().unwrap();
        let config = RuntimeConfig::from_parts(
            "http://127.0.0.1:9222".to_string(),
            tempdir.path().join("state"),
        )
        .unwrap();
        let status = BrokerStatus {
            protocol_version: BROKER_PROTOCOL_VERSION,
            pid: 123,
            runtime_mode: RuntimeMode::External,
            cdp_endpoint: "http://127.0.0.1:9223".to_string(),
            ipc_endpoint: config.ipc_endpoint.clone(),
            socket_path: config.socket_path.clone(),
        };

        let message = broker_status_mismatch(&config, &status)
            .unwrap()
            .expect("external startup should reject a different endpoint");
        assert!(message.contains("broker CDP endpoint mismatch"));
    }

    #[tokio::test]
    async fn element_action_retry_waits_for_actionability_and_times_out() {
        let mut attempts = 0;
        let value = retry_element_action(Some(250), || {
            attempts += 1;
            std::future::ready(if attempts < 3 {
                Err(BrowserToolError::element_not_actionable("moving"))
            } else {
                Ok("ready")
            })
        })
        .await
        .unwrap();
        assert_eq!(value, "ready");
        assert_eq!(attempts, 3);

        let error = retry_element_action(Some(1), || {
            std::future::ready::<Result<(), _>>(Err(BrowserToolError::element_not_actionable(
                "covered",
            )))
        })
        .await
        .unwrap_err();
        assert_eq!(error.code, BrowserToolErrorCode::OperationTimeout);
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
