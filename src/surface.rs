use std::sync::Arc;

pub use agent_surface_contract::PRODUCTION_TOOLS;
use agent_surface_contract::{ToolDefinition, hybrid_catalog};
use anyhow::Result;
use serde::Serialize;
use serde_json::{Map, Value, json};

use crate::{
    broker,
    config::RuntimeConfig,
    leases::BrowserToolError,
    protocol::{BrokerRequestContext, BrokerResponse},
};

#[derive(Clone)]
pub struct VisibleBrowserLabSurface {
    config: RuntimeConfig,
    tools: Arc<Vec<ToolDefinition>>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SurfaceToolResult {
    Structured(Value),
    StructuredError(Value),
}

impl SurfaceToolResult {
    pub fn structured(value: Value) -> Self {
        Self::Structured(value)
    }

    pub fn structured_error(value: Value) -> Self {
        Self::StructuredError(value)
    }

    pub fn is_error(&self) -> bool {
        matches!(self, Self::StructuredError(_))
    }

    pub fn into_value(self) -> Value {
        match self {
            Self::Structured(value) | Self::StructuredError(value) => value,
        }
    }
}

impl VisibleBrowserLabSurface {
    pub fn new(config: RuntimeConfig) -> Result<Self> {
        let tools = hybrid_catalog()
            .into_iter()
            .filter(|definition| PRODUCTION_TOOLS.contains(&definition.name.as_str()))
            .collect::<Vec<_>>();

        if tools.len() != PRODUCTION_TOOLS.len() {
            anyhow::bail!(
                "agent surface catalog mismatch: expected {} production tools, found {}",
                PRODUCTION_TOOLS.len(),
                tools.len()
            );
        }

        Ok(Self {
            config,
            tools: Arc::new(tools),
        })
    }

    pub fn tools(&self) -> &[ToolDefinition] {
        &self.tools
    }

    pub fn get_tool(&self, name: &str) -> Option<&ToolDefinition> {
        self.tools.iter().find(|tool| tool.name == name)
    }

    pub async fn call_tool(
        &self,
        name: &str,
        arguments: Map<String, Value>,
        context: BrokerRequestContext,
    ) -> SurfaceToolResult {
        if self.get_tool(name).is_none() {
            return SurfaceToolResult::structured_error(json!({
                "code": "unsupported_operation",
                "message": format!("unknown tool `{name}`"),
                "recovery": "help",
            }));
        }

        if name == "help" {
            return Self::help(&arguments);
        }

        self.call_broker(name, Value::Object(arguments), context)
            .await
    }

    pub fn help(arguments: &Map<String, Value>) -> SurfaceToolResult {
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
                {"code":"focus_required", "recovery":"The requested handoff did not make the owned document focused; keep the same tab_id, verify Chrome is available, and retry focus_tab when the user is ready."},
                {"code":"element_stale", "recovery":"Call snapshot and use a reference from the active document."}
            ]
        });
        if let Some(operation) = operation {
            response["operation"] = Value::String(operation.to_string());
        }
        SurfaceToolResult::structured(response)
    }

    async fn call_broker<P>(
        &self,
        method: &str,
        params: P,
        context: BrokerRequestContext,
    ) -> SurfaceToolResult
    where
        P: Serialize,
    {
        let response = match self.call_broker_response(method, params, context).await {
            Ok(response) => response,
            Err(error) => return structured_browser_error(error),
        };

        if response.ok {
            return SurfaceToolResult::structured(response.result.unwrap_or(Value::Null));
        }

        structured_browser_error(response.error.unwrap_or_else(|| {
            BrowserToolError::invalid_input("broker error response omitted error payload")
        }))
    }

    async fn call_broker_response<P>(
        &self,
        method: &str,
        params: P,
        context: BrokerRequestContext,
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
            .request_response_with_context(method, params, Some(context))
            .await
            .map_err(|error| {
                BrowserToolError::chrome_unavailable(format!(
                    "visible-browser-lab broker request `{method}` failed: {error}"
                ))
            })
    }
}

fn structured_browser_error(error: BrowserToolError) -> SurfaceToolResult {
    let value = serde_json::to_value(error).unwrap_or_else(|serialization_error| {
        json!({
            "code": "invalid_input",
            "message": format!("failed to serialize browser error: {serialization_error}")
        })
    });
    SurfaceToolResult::structured_error(value)
}

fn help_content(topic: &str, operation: Option<&str>) -> (Vec<&'static str>, String) {
    let (tools, guidance) = match topic {
        "tabs" => (
            vec!["list_tabs", "new_tab", "claim_tab", "start_session"],
            "Use the conversation-selected session and its owned tab handles. Call start_session only after session_required, and inspect global_readonly only when choosing a target to claim.",
        ),
        "snapshot" => (
            vec!["snapshot"],
            "Inspect the accessibility tree and retain its short element references for semantic actions.",
        ),
        "interaction" => (
            vec!["click", "fill", "fill_form", "type_text", "press_key"],
            "Use snapshot references for interaction. Routine click, targeted key, and referenced pointer actions attach to the owned target, prepare the resolved element, and preserve the user's active application. Click results include delivery and effect evidence for URL, network, and quiet submit verification. Targetless press_key and interact click_at require focus_tab first.",
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
            vec!["snapshot", "help", "start_session"],
            "Call browser operations directly, inspect the page semantically, choose the narrowest action tool, and close or release each owned tab. Call start_session only after session_required.",
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
    use crate::config::RuntimeConfig;

    #[test]
    fn production_catalog_matches_declared_tool_set() {
        let config = RuntimeConfig::managed(std::path::PathBuf::from("/tmp/vbl-surface"), None);
        let surface = VisibleBrowserLabSurface::new(config).unwrap();

        assert_eq!(surface.tools().len(), 27);
        assert_eq!(surface.tools().len(), PRODUCTION_TOOLS.len());
        assert!(
            PRODUCTION_TOOLS
                .iter()
                .all(|name| surface.get_tool(name).is_some())
        );
    }

    #[test]
    fn help_routes_specialized_topics() {
        let result = VisibleBrowserLabSurface::help(&Map::from_iter([(
            "topic".to_string(),
            Value::String("performance".to_string()),
        )]));

        assert!(!result.is_error());
    }
}
