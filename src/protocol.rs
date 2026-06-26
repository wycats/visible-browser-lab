use std::{collections::BTreeMap, path::PathBuf};

use anyhow::{Context, Result, bail};
use rmcp::schemars::JsonSchema;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Map;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use uuid::Uuid;

use crate::{
    artifacts::ArtifactSummary,
    config::RuntimeMode,
    ipc::{BrokerEndpoint, BrokerStream},
    leases::{
        AgentSessionId, BrowserToolError, BrowserToolErrorCode, GlobalTabGroup, OwnedTabSummary,
        TabId,
    },
};

pub const BROKER_PROTOCOL_VERSION: u32 = 3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrokerStatus {
    pub protocol_version: u32,
    pub pid: u32,
    pub runtime_mode: RuntimeMode,
    pub cdp_endpoint: String,
    pub ipc_endpoint: String,
    pub socket_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StartSessionParams {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_url: Option<String>,
    #[serde(default)]
    pub focus: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_root: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartSessionResult {
    pub agent_session_id: AgentSessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab: Option<OwnedTabSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ListTabsScope {
    Owned,
    GlobalReadonly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ListTabsParams {
    pub agent_session_id: AgentSessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<ListTabsScope>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum ListTabsResult {
    Owned { tabs: Vec<OwnedTabSummary> },
    GlobalReadonly { groups: Vec<GlobalTabGroup> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NewTabParams {
    pub agent_session_id: AgentSessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default)]
    pub focus: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabResult {
    pub tab: OwnedTabSummary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ClaimTabParams {
    pub agent_session_id: AgentSessionId,
    pub target_id: String,
    #[serde(default)]
    pub takeover: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_instruction: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TabActionParams {
    pub agent_session_id: AgentSessionId,
    pub tab_id: TabId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NavigateParams {
    pub agent_session_id: AgentSessionId,
    pub tab_id: TabId,
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_until: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum NavigationAction {
    Url,
    Back,
    Forward,
    Reload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct V3NavigateParams {
    pub agent_session_id: AgentSessionId,
    pub tab_id: TabId,
    pub action: NavigationAction,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wait_until: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default)]
    pub ignore_cache: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub before_unload: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub init_script: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observe: Option<ObservationMode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ScreenshotParams {
    pub agent_session_id: AgentSessionId,
    pub tab_id: TabId,
    #[serde(default)]
    pub full_page: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<ElementTarget>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quality: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenshotResult {
    pub artifact: ArtifactSummary,
    pub image: ScreenshotImage,
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScreenshotImage {
    pub media_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EvaluateParams {
    pub agent_session_id: AgentSessionId,
    pub tab_id: TabId,
    pub expression: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct V3EvaluateParams {
    pub agent_session_id: AgentSessionId,
    pub tab_id: TabId,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<ElementTarget>,
    #[serde(default = "default_true")]
    pub await_promise: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvaluateResult {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ClickParams {
    pub agent_session_id: AgentSessionId,
    pub tab_id: TabId,
    pub target: ElementTarget,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub button: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<u8>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modifiers: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observe: Option<ObservationMode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum ElementTarget {
    Reference(ElementReferenceTarget),
    Css(CssElementTarget),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ElementReferenceTarget {
    #[serde(rename = "ref")]
    pub reference: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CssElementTarget {
    pub css: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_ref: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum ObservationMode {
    None,
    #[default]
    Diff,
    Snapshot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotMode {
    Interactive,
    #[default]
    Meaningful,
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SnapshotParams {
    pub agent_session_id: AgentSessionId,
    pub tab_id: TabId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<SnapshotMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depth: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_nodes: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotResult {
    pub snapshot_id: String,
    pub document_revision: String,
    pub url: String,
    pub title: String,
    pub tree: String,
    pub node_count: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotDiff {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_snapshot_id: Option<String>,
    pub snapshot_id: String,
    pub document_revision: String,
    pub changes: String,
    pub changed_node_count: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Observation {
    None,
    Diff { diff: SnapshotDiff },
    Snapshot { snapshot: SnapshotResult },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageActionResult {
    pub document_revision: String,
    pub observation: Observation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FillParams {
    pub agent_session_id: AgentSessionId,
    pub tab_id: TabId,
    pub target: ElementTarget,
    pub value: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observe: Option<ObservationMode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TypeTextParams {
    pub agent_session_id: AgentSessionId,
    pub tab_id: TabId,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct V3TypeTextParams {
    pub agent_session_id: AgentSessionId,
    pub tab_id: TabId,
    pub target: ElementTarget,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observe: Option<ObservationMode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypeTextResult {
    pub typed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PressKeyParams {
    pub agent_session_id: AgentSessionId,
    pub tab_id: TabId,
    pub key: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modifiers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct V3PressKeyParams {
    pub agent_session_id: AgentSessionId,
    pub tab_id: TabId,
    pub key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<ElementTarget>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modifiers: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observe: Option<ObservationMode>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WaitCondition {
    Delay {
        duration_ms: u64,
    },
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        state: Option<String>,
    },
    Element {
        target: ElementTarget,
        state: String,
    },
    Url {
        value: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        r#match: Option<String>,
    },
    Load {
        state: String,
    },
    Expression {
        expression: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct WaitForParams {
    pub agent_session_id: AgentSessionId,
    pub tab_id: TabId,
    pub condition: WaitCondition,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observe: Option<ObservationMode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitForResult {
    pub matched: bool,
    pub elapsed_ms: u64,
    pub document_revision: String,
    pub observation: Observation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FormField {
    Text {
        target: ElementTarget,
        value: String,
    },
    Select {
        target: ElementTarget,
        values: Vec<String>,
    },
    Checked {
        target: ElementTarget,
        checked: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FillFormParams {
    pub agent_session_id: AgentSessionId,
    pub tab_id: TabId,
    pub fields: Vec<FormField>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observe: Option<ObservationMode>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FillFormResult {
    pub completed_fields: usize,
    pub total_fields: usize,
    pub document_revision: String,
    pub observation: Observation,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DomainParams {
    pub agent_session_id: AgentSessionId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tab_id: Option<TabId>,
    pub operation: String,
    #[serde(flatten)]
    pub arguments: Map<String, Value>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PressKeyResult {
    pub pressed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DiagnosticsParams {
    pub agent_session_id: AgentSessionId,
    pub tab_id: TabId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsoleMessage {
    pub sequence: u64,
    pub level: String,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetworkEvent {
    pub sequence: u64,
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime_type: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsoleMessagesResult {
    pub messages: Vec<ConsoleMessage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NetworkEventsResult {
    pub events: Vec<NetworkEvent>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseTabResult {
    pub released: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseTabResult {
    pub closed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrokerRequest {
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrokerResponse {
    pub id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<BrowserToolError>,
}

impl BrokerResponse {
    pub fn success<T: Serialize>(id: String, result: T) -> Result<Self> {
        Ok(Self {
            id,
            ok: true,
            result: Some(serde_json::to_value(result)?),
            error: None,
        })
    }

    pub fn error(id: String, error: BrowserToolError) -> Self {
        Self {
            id,
            ok: false,
            result: None,
            error: Some(error),
        }
    }

    pub fn invalid_input(id: String, message: impl Into<String>) -> Self {
        Self::error(id, BrowserToolError::invalid_input(message))
    }
}

pub struct BrokerClient {
    stream: BufReader<BrokerStream>,
}

impl BrokerClient {
    pub async fn connect(endpoint: &BrokerEndpoint) -> Result<Self> {
        let stream = endpoint.connect().await?;
        Ok(Self::new(stream))
    }

    pub fn new(stream: BrokerStream) -> Self {
        Self {
            stream: BufReader::new(stream),
        }
    }

    pub async fn ping(&mut self) -> Result<BrokerStatus> {
        self.request("ping", Value::Null).await
    }

    pub async fn request_response<P>(&mut self, method: &str, params: P) -> Result<BrokerResponse>
    where
        P: Serialize,
    {
        let request_id = Uuid::new_v4().to_string();
        let request = BrokerRequest {
            id: request_id.clone(),
            method: method.to_string(),
            params: serde_json::to_value(params)?,
        };
        let encoded = serde_json::to_string(&request)?;

        self.stream.get_mut().write_all(encoded.as_bytes()).await?;
        self.stream.get_mut().write_all(b"\n").await?;
        self.stream.get_mut().flush().await?;

        let mut line = String::new();
        let bytes = self.stream.read_line(&mut line).await?;
        if bytes == 0 {
            bail!("broker closed the socket before responding to `{method}`");
        }

        let response: BrokerResponse =
            serde_json::from_str(&line).context("broker returned invalid JSON")?;

        if response.id != request_id {
            bail!(
                "broker response id mismatch: expected `{request_id}`, got `{}`",
                response.id
            );
        }

        Ok(response)
    }

    pub async fn request<P, R>(&mut self, method: &str, params: P) -> Result<R>
    where
        P: Serialize,
        R: DeserializeOwned,
    {
        let response = self.request_response(method, params).await?;

        if response.ok {
            let result = response
                .result
                .context("broker success response omitted result")?;
            return Ok(serde_json::from_value(result)?);
        }

        let error = response.error.unwrap_or_else(|| BrowserToolError {
            code: BrowserToolErrorCode::InvalidInput,
            message: "broker error response omitted error payload".to_string(),
            recovery: None,
        });
        bail!(
            "broker `{method}` failed with {:?}: {}",
            error.code,
            error.message
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn success_response_omits_error_payload() {
        let response =
            BrokerResponse::success("request-1".to_string(), json!({ "ok": true })).unwrap();
        let value = serde_json::to_value(response).unwrap();

        assert_eq!(value["ok"], true);
        assert!(value.get("error").is_none());
    }

    #[test]
    fn error_response_omits_result_payload() {
        let response = BrokerResponse::invalid_input("request-1".to_string(), "bad request");
        let value = serde_json::to_value(response).unwrap();

        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "invalid_input");
        assert!(value.get("result").is_none());
    }

    #[test]
    fn page_action_params_use_expected_wire_fields() {
        let params = ClickParams {
            agent_session_id: AgentSessionId("session_test".to_string()),
            tab_id: TabId("tab_test".to_string()),
            target: ElementTarget::Css(CssElementTarget {
                css: "#submit".to_string(),
                frame_ref: None,
            }),
            button: Some("right".to_string()),
            count: Some(2),
            modifiers: vec!["shift".to_string()],
            timeout_ms: Some(500),
            observe: Some(ObservationMode::Diff),
        };
        let value = serde_json::to_value(params).unwrap();

        assert_eq!(value["agent_session_id"], "session_test");
        assert_eq!(value["tab_id"], "tab_test");
        assert_eq!(value["target"]["css"], "#submit");
        assert_eq!(value["timeout_ms"], 500);
        assert_eq!(value["observe"], "diff");
    }

    #[test]
    fn element_target_rejects_mixed_reference_and_css_fields() {
        let error = serde_json::from_value::<ElementTarget>(json!({
            "ref": "e_1",
            "css": "#submit"
        }))
        .unwrap_err();

        assert!(error.to_string().contains("did not match any variant"));
    }

    #[test]
    fn diagnostics_results_include_sequence_numbers() {
        let result = ConsoleMessagesResult {
            messages: vec![ConsoleMessage {
                sequence: 7,
                level: "log".to_string(),
                text: "ready".to_string(),
                timestamp_ms: None,
            }],
        };
        let value = serde_json::to_value(result).unwrap();

        assert_eq!(value["messages"][0]["sequence"], 7);
        assert_eq!(value["messages"][0]["text"], "ready");
    }
}
