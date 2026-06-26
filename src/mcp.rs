use std::{collections::BTreeMap, sync::Arc};

use agent_surface_contract::{SERVER_INSTRUCTIONS, hybrid_catalog};
use anyhow::{Context, Result};
use rmcp::{
    RoleServer, ServerHandler, ServiceExt,
    model::{
        CallToolRequestParams, CallToolResult, JsonObject, ListToolsResult, Meta,
        PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
    },
    service::RequestContext,
    transport::stdio,
};
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::{broker, config::RuntimeConfig, leases::BrowserToolError, protocol::BrokerResponse};

const CODEX_SANDBOX_STATE_META_CAPABILITY: &str = "codex/sandbox-state-meta";
const PRODUCTION_TOOLS: &[&str] = &[
    "start_session",
    "list_tabs",
    "new_tab",
    "claim_tab",
    "release_tab",
    "focus_tab",
    "close_tab",
    "snapshot",
    "navigate",
    "wait_for",
    "click",
    "fill",
    "fill_form",
    "type_text",
    "press_key",
    "screenshot",
    "evaluate",
    "interact",
    "console",
    "network",
    "emulation",
    "performance",
    "audit",
    "memory",
    "screencast",
    "artifacts",
    "help",
];

#[derive(Clone)]
struct VisibleBrowserLab {
    config: RuntimeConfig,
    tools: Arc<Vec<Tool>>,
}

impl VisibleBrowserLab {
    fn new(config: RuntimeConfig) -> Result<Self> {
        let tools = hybrid_catalog()
            .into_iter()
            .filter(|definition| PRODUCTION_TOOLS.contains(&definition.name.as_str()))
            .map(|definition| {
                serde_json::from_value(serde_json::to_value(definition)?)
                    .context("agent surface definition is not a valid MCP tool")
            })
            .collect::<Result<Vec<Tool>>>()?;
        Ok(Self {
            config,
            tools: Arc::new(tools),
        })
    }

    async fn call_broker<P>(&self, method: &str, params: P) -> CallToolResult
    where
        P: Serialize,
    {
        let response = match self.call_broker_response(method, params).await {
            Ok(response) => response,
            Err(error) => return structured_browser_error(error),
        };

        if response.ok {
            return CallToolResult::structured(response.result.unwrap_or(Value::Null));
        }

        structured_browser_error(response.error.unwrap_or_else(|| {
            BrowserToolError::invalid_input("broker error response omitted error payload")
        }))
    }

    async fn call_broker_response<P>(
        &self,
        method: &str,
        params: P,
    ) -> Result<BrokerResponse, BrowserToolError>
    where
        P: Serialize,
    {
        let mut client = broker::ensure_running(&self.config)
            .await
            .map_err(|error| {
                BrowserToolError::chrome_unavailable(format!(
                    "visible-browser-lab broker is unavailable: {error}"
                ))
            })?;

        client
            .request_response(method, params)
            .await
            .map_err(|error| {
                BrowserToolError::chrome_unavailable(format!(
                    "visible-browser-lab broker request `{method}` failed: {error}"
                ))
            })
    }

    fn help(arguments: &Map<String, Value>) -> CallToolResult {
        let topic = arguments
            .get("topic")
            .and_then(Value::as_str)
            .unwrap_or("workflow");
        let operation = arguments.get("operation").and_then(Value::as_str);
        let (preferred_tools, guidance) = help_content(topic, operation);
        let preferred_tool = preferred_tools.first().copied().unwrap_or("help");
        let result_schema = hybrid_catalog()
            .into_iter()
            .find(|tool| tool.name == preferred_tool)
            .map(|tool| tool.output_schema)
            .unwrap_or_else(|| json!({}));
        let preferred = operation.map_or_else(
            || json!({"tool": preferred_tool, "reason": guidance}),
            |operation| json!({"tool": preferred_tool, "operation": operation, "reason": guidance}),
        );
        let neighbors = preferred_tools
            .iter()
            .skip(1)
            .map(|tool| json!({"tool": tool, "use_when": format!("Use `{tool}` when it is the narrowest operation for the task.")}))
            .collect::<Vec<_>>();
        let mut response = json!({
            "topic": topic,
            "task": guidance,
            "preferred": preferred,
            "neighbors": neighbors,
            "example": {"tool": preferred_tool, "arguments": {}},
            "result_schema": result_schema,
            "errors": [
                {"code":"focus_required", "recovery":"Call focus_tab for the owned tab and retry native input."},
                {"code":"element_stale", "recovery":"Call snapshot and use a reference from the active document."}
            ]
        });
        if let Some(operation) = operation {
            response["operation"] = Value::String(operation.to_string());
        }
        CallToolResult::structured(response)
    }
}

impl ServerHandler for VisibleBrowserLab {
    fn get_info(&self) -> ServerInfo {
        let mut capabilities = ServerCapabilities::builder().enable_tools().build();
        capabilities.experimental = Some(BTreeMap::from([(
            CODEX_SANDBOX_STATE_META_CAPABILITY.to_string(),
            JsonObject::new(),
        )]));
        ServerInfo::new(capabilities).with_instructions(SERVER_INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, rmcp::ErrorData> {
        Ok(ListToolsResult::with_all_items(self.tools.as_ref().clone()))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tools.iter().find(|tool| tool.name == name).cloned()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        let name = request.name.as_ref();
        if !self.tools.iter().any(|tool| tool.name == request.name) {
            return Ok(CallToolResult::structured_error(json!({
                "code": "unsupported_operation",
                "message": format!("unknown tool `{name}`"),
                "recovery": "help",
            })));
        }

        let mut arguments = request.arguments.unwrap_or_default();
        if name == "help" {
            return Ok(Self::help(&arguments));
        }
        if name == "start_session"
            && let Some(workspace_root) = workspace_root(&context.meta)
        {
            arguments.insert("workspace_root".to_string(), Value::String(workspace_root));
        }

        Ok(self.call_broker(name, Value::Object(arguments)).await)
    }
}

pub async fn run(config: RuntimeConfig) -> Result<()> {
    let server = VisibleBrowserLab::new(config)?;
    server.serve(stdio()).await?.waiting().await?;
    Ok(())
}

fn structured_browser_error(error: BrowserToolError) -> CallToolResult {
    let value = serde_json::to_value(error).unwrap_or_else(|serialization_error| {
        json!({
            "code": "invalid_input",
            "message": format!("failed to serialize browser error: {serialization_error}")
        })
    });
    CallToolResult::structured_error(value)
}

fn workspace_root(meta: &Meta) -> Option<String> {
    codex_workspace_cwd(meta)
        .map(ToOwned::to_owned)
        .or_else(|| non_empty_env("CLAUDE_PROJECT_DIR"))
        .or_else(|| non_empty_env("VISIBLE_BROWSER_LAB_WORKSPACE_ROOT"))
}

fn codex_workspace_cwd(meta: &Meta) -> Option<&str> {
    meta.0
        .get(CODEX_SANDBOX_STATE_META_CAPABILITY)?
        .get("sandboxCwd")?
        .as_str()
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn help_content(topic: &str, operation: Option<&str>) -> (Vec<&'static str>, String) {
    let (tools, guidance) = match topic {
        "tabs" => (
            vec!["start_session", "list_tabs", "new_tab", "claim_tab"],
            "Start one session, use its owned tab handles, and inspect global_readonly only when choosing a target to claim.",
        ),
        "snapshot" => (
            vec!["snapshot"],
            "Inspect the accessibility tree and retain its short element references for semantic actions.",
        ),
        "interaction" => (
            vec!["click", "fill", "fill_form", "type_text", "press_key"],
            "Use snapshot references for interaction. Focus the tab only when native pointer or keyboard input reports focus_required.",
        ),
        "navigation" => (
            vec!["navigate", "wait_for"],
            "Navigate by URL, history, or reload, then wait for the semantic state that completes the task.",
        ),
        "diagnostics" => (
            vec!["console", "network"],
            "Inspect structured console and network records by stable lease-scoped identifiers.",
        ),
        "emulation" => (
            vec!["emulation"],
            "Apply target-scoped emulation and use reset to restore normal browser behavior.",
        ),
        "performance" => (
            vec!["performance"],
            "Capture a trace or read web vitals, then analyze the resulting trace artifact.",
        ),
        "audit" => (
            vec!["audit"],
            "Run the requested accessibility, SEO, best-practices, or agentic-browsing checks.",
        ),
        "memory" => (
            vec!["memory"],
            "Capture a heap snapshot, query its bounded graph views, and close it when analysis is complete.",
        ),
        "screencast" => (
            vec!["screencast"],
            "Start and explicitly stop a silent WebM recording while the tab remains owned.",
        ),
        "artifacts" => (
            vec!["artifacts"],
            "Inspect session-owned artifact metadata, read bounded ranges, or export within the session workspace.",
        ),
        "errors" => (
            vec!["help", "snapshot", "list_tabs"],
            "Follow the structured recovery field, refreshing snapshots after stale references and listings after lease changes.",
        ),
        _ => (
            vec!["start_session", "snapshot", "help"],
            "Start a session, inspect the page semantically, choose the narrowest action tool, and close or release each owned tab.",
        ),
    };
    let suffix = operation
        .map(|operation| format!(" Requested operation: `{operation}`."))
        .unwrap_or_default();
    (tools, format!("{guidance}{suffix}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertises_core_contract_and_reads_codex_workspace_metadata() {
        let config = RuntimeConfig::managed(std::path::PathBuf::from("/tmp/vbl-mcp"), None);
        let server = VisibleBrowserLab::new(config).unwrap();
        let info = server.get_info();
        assert_eq!(server.tools.len(), 27);
        assert_eq!(server.tools.len(), PRODUCTION_TOOLS.len());
        assert!(
            PRODUCTION_TOOLS
                .iter()
                .all(|name| server.get_tool(name).is_some())
        );
        assert!(
            info.capabilities.experimental.as_ref().is_some_and(
                |capabilities| capabilities.contains_key(CODEX_SANDBOX_STATE_META_CAPABILITY)
            )
        );

        let meta = Meta(serde_json::Map::from_iter([(
            CODEX_SANDBOX_STATE_META_CAPABILITY.to_string(),
            json!({ "sandboxCwd": "/workspace/project" }),
        )]));
        assert_eq!(codex_workspace_cwd(&meta), Some("/workspace/project"));
    }

    #[test]
    fn help_routes_specialized_topics() {
        let result = VisibleBrowserLab::help(&Map::from_iter([(
            "topic".to_string(),
            Value::String("performance".to_string()),
        )]));
        assert!(!result.is_error.unwrap_or(false));
    }
}
