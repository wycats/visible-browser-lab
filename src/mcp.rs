use std::collections::BTreeMap;

use anyhow::Result;
use rmcp::{
    RoleServer, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, JsonObject, Meta, ServerCapabilities, ServerInfo},
    service::RequestContext,
    tool, tool_handler, tool_router,
    transport::stdio,
};

const CODEX_SANDBOX_STATE_META_CAPABILITY: &str = "codex/sandbox-state-meta";
use serde::Serialize;
use serde_json::Value;

use crate::{
    broker,
    config::RuntimeConfig,
    leases::BrowserToolError,
    protocol::{
        BrokerResponse, ClaimTabParams, ClickParams, DiagnosticsParams, EvaluateParams, FillParams,
        ListTabsParams, NavigateParams, NewTabParams, PressKeyParams, ScreenshotParams,
        SnapshotParams, StartSessionParams, TabActionParams, TypeTextParams,
    },
};

#[derive(Debug, Clone)]
struct VisibleBrowserLab {
    config: RuntimeConfig,
    tool_router: ToolRouter<Self>,
}

impl VisibleBrowserLab {
    fn new(config: RuntimeConfig) -> Self {
        Self {
            config,
            tool_router: Self::tool_router(),
        }
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
}

#[tool_router]
impl VisibleBrowserLab {
    #[tool(
        name = "start_session",
        description = "Start a visible-browser session and optionally create the first leased tab."
    )]
    async fn start_session(
        &self,
        context: RequestContext<RoleServer>,
        params: Parameters<StartSessionParams>,
    ) -> CallToolResult {
        if let Some(workspace) = codex_workspace_cwd(&context.meta) {
            tracing::debug!(workspace, "received Codex workspace context");
        }
        self.call_broker("start_session", params.0).await
    }

    #[tool(
        name = "list_tabs",
        description = "List tabs owned by this session by default, or request a read-only global inventory."
    )]
    async fn list_tabs(&self, params: Parameters<ListTabsParams>) -> CallToolResult {
        self.call_broker("list_tabs", params.0).await
    }

    #[tool(
        name = "new_tab",
        description = "Create a new visible Chrome tab and lease it to this session."
    )]
    async fn new_tab(&self, params: Parameters<NewTabParams>) -> CallToolResult {
        self.call_broker("new_tab", params.0).await
    }

    #[tool(
        name = "claim_tab",
        description = "Claim an unowned visible Chrome target. Set takeover only with explicit user instruction."
    )]
    async fn claim_tab(&self, params: Parameters<ClaimTabParams>) -> CallToolResult {
        self.call_broker("claim_tab", params.0).await
    }

    #[tool(
        name = "release_tab",
        description = "Release an owned tab lease while leaving the Chrome tab open and claimable."
    )]
    async fn release_tab(&self, params: Parameters<TabActionParams>) -> CallToolResult {
        self.call_broker("release_tab", params.0).await
    }

    #[tool(
        name = "focus_tab",
        description = "Focus an active tab owned by this session."
    )]
    async fn focus_tab(&self, params: Parameters<TabActionParams>) -> CallToolResult {
        self.call_broker("focus_tab", params.0).await
    }

    #[tool(
        name = "navigate",
        description = "Navigate an active tab owned by this session and wait for page load."
    )]
    async fn navigate(&self, params: Parameters<NavigateParams>) -> CallToolResult {
        self.call_broker("navigate", params.0).await
    }

    #[tool(
        name = "screenshot",
        description = "Capture a PNG screenshot from an active tab owned by this session."
    )]
    async fn screenshot(&self, params: Parameters<ScreenshotParams>) -> CallToolResult {
        self.call_broker("screenshot", params.0).await
    }

    #[tool(
        name = "evaluate",
        description = "Evaluate JavaScript in an active tab owned by this session."
    )]
    async fn evaluate(&self, params: Parameters<EvaluateParams>) -> CallToolResult {
        self.call_broker("evaluate", params.0).await
    }

    #[tool(
        name = "snapshot",
        description = "Inspect user-perceivable page structure and obtain lease-scoped element references."
    )]
    async fn snapshot(&self, params: Parameters<SnapshotParams>) -> CallToolResult {
        self.call_broker("snapshot", params.0).await
    }

    #[tool(
        name = "click",
        description = "Click one referenced element after ownership and actionability checks, or one explicit CSS selector after strict single-match and visibility checks."
    )]
    async fn click(&self, params: Parameters<ClickParams>) -> CallToolResult {
        self.call_broker("click", params.0).await
    }

    #[tool(
        name = "fill",
        description = "Replace the value of one referenced editable control without activating Chrome."
    )]
    async fn fill(&self, params: Parameters<FillParams>) -> CallToolResult {
        self.call_broker("fill", params.0).await
    }

    #[tool(
        name = "type_text",
        description = "Insert text into the focused element in an active tab owned by this session."
    )]
    async fn type_text(&self, params: Parameters<TypeTextParams>) -> CallToolResult {
        self.call_broker("type_text", params.0).await
    }

    #[tool(
        name = "press_key",
        description = "Dispatch one printable or common named key in an active tab owned by this session."
    )]
    async fn press_key(&self, params: Parameters<PressKeyParams>) -> CallToolResult {
        self.call_broker("press_key", params.0).await
    }

    #[tool(
        name = "console_messages",
        description = "Read buffered console messages for an active tab owned by this session."
    )]
    async fn console_messages(&self, params: Parameters<DiagnosticsParams>) -> CallToolResult {
        self.call_broker("console_messages", params.0).await
    }

    #[tool(
        name = "network_events",
        description = "Read buffered network events for an active tab owned by this session."
    )]
    async fn network_events(&self, params: Parameters<DiagnosticsParams>) -> CallToolResult {
        self.call_broker("network_events", params.0).await
    }

    #[tool(
        name = "close_tab",
        description = "Close an owned Chrome tab and mark its lease closed."
    )]
    async fn close_tab(&self, params: Parameters<TabActionParams>) -> CallToolResult {
        self.call_broker("close_tab", params.0).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for VisibleBrowserLab {
    fn get_info(&self) -> ServerInfo {
        let mut capabilities = ServerCapabilities::builder().enable_tools().build();
        capabilities.experimental = Some(BTreeMap::from([(
            CODEX_SANDBOX_STATE_META_CAPABILITY.to_string(),
            JsonObject::new(),
        )]));
        ServerInfo::new(capabilities)
            .with_instructions("Use start_session first and retain its agent_session_id. Act only through tab_id values owned by that session. Inspect unfamiliar pages with snapshot, then pass its element references to click or fill. Use focus_tab before trusted pointer or keyboard input when an action returns focus_required.")
    }
}

pub async fn run(config: RuntimeConfig) -> Result<()> {
    let server = VisibleBrowserLab::new(config);
    server.serve(stdio()).await?.waiting().await?;
    Ok(())
}

fn structured_browser_error(error: BrowserToolError) -> CallToolResult {
    let value = serde_json::to_value(error).unwrap_or_else(|serialization_error| {
        serde_json::json!({
            "code": "invalid_input",
            "message": format!("failed to serialize browser error: {serialization_error}")
        })
    });
    CallToolResult::structured_error(value)
}

fn codex_workspace_cwd(meta: &Meta) -> Option<&str> {
    meta.0
        .get(CODEX_SANDBOX_STATE_META_CAPABILITY)?
        .get("sandboxCwd")?
        .as_str()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advertises_and_reads_codex_workspace_metadata() {
        let config = RuntimeConfig::managed(std::path::PathBuf::from("/tmp/vbl-mcp"), None);
        let info = VisibleBrowserLab::new(config).get_info();
        assert!(
            info.capabilities
                .experimental
                .as_ref()
                .is_some_and(|capabilities| {
                    capabilities.contains_key(CODEX_SANDBOX_STATE_META_CAPABILITY)
                })
        );

        let meta = Meta(serde_json::Map::from_iter([(
            CODEX_SANDBOX_STATE_META_CAPABILITY.to_string(),
            serde_json::json!({ "sandboxCwd": "/workspace/project" }),
        )]));
        assert_eq!(codex_workspace_cwd(&meta), Some("/workspace/project"));
    }
}
