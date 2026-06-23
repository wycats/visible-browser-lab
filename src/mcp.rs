use anyhow::Result;
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::Serialize;
use serde_json::Value;

use crate::{
    broker,
    config::RuntimeConfig,
    leases::BrowserToolError,
    protocol::{
        BrokerResponse, ClaimTabParams, ListTabsParams, NavigateParams, NewTabParams,
        ScreenshotParams, StartSessionParams, TabActionParams,
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
    async fn start_session(&self, params: Parameters<StartSessionParams>) -> CallToolResult {
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
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Use start_session first. Reuse the returned agent_session_id and act only through owned tab_id values.")
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
