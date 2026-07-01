use std::{collections::BTreeMap, sync::Arc};

use crate::{
    config::RuntimeConfig,
    surface::{SurfaceToolResult, VisibleBrowserLabSurface},
};
use agent_surface_contract::SERVER_INSTRUCTIONS;
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

const CODEX_SANDBOX_STATE_META_CAPABILITY: &str = "codex/sandbox-state-meta";

#[derive(Clone)]
struct VisibleBrowserLab {
    surface: VisibleBrowserLabSurface,
    tools: Arc<Vec<Tool>>,
}

impl VisibleBrowserLab {
    fn new(config: RuntimeConfig) -> Result<Self> {
        let surface = VisibleBrowserLabSurface::new(config)?;
        let tools = surface
            .tools()
            .iter()
            .map(|definition| {
                serde_json::from_value(serde_json::to_value(definition)?)
                    .context("agent surface definition is not a valid MCP tool")
            })
            .collect::<Result<Vec<Tool>>>()?;
        Ok(Self {
            surface,
            tools: Arc::new(tools),
        })
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
        let arguments = request.arguments.unwrap_or_default();
        let result = self
            .surface
            .call_tool(name, arguments, workspace_root(&context.meta))
            .await;

        Ok(call_tool_result(result))
    }
}

pub async fn run(config: RuntimeConfig) -> Result<()> {
    let server = VisibleBrowserLab::new(config)?;
    server.serve(stdio()).await?.waiting().await?;
    Ok(())
}

fn call_tool_result(result: SurfaceToolResult) -> CallToolResult {
    match result {
        SurfaceToolResult::Structured(value) => CallToolResult::structured(value),
        SurfaceToolResult::StructuredError(value) => CallToolResult::structured_error(value),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface::PRODUCTION_TOOLS;

    #[test]
    fn advertises_core_contract_and_reads_codex_workspace_metadata() {
        let config = RuntimeConfig::managed(std::path::PathBuf::from("/tmp/vbl-mcp"), None);
        let server = VisibleBrowserLab::new(config).unwrap();
        let info = server.get_info();
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
            serde_json::json!({ "sandboxCwd": "/workspace/project" }),
        )]));
        assert_eq!(codex_workspace_cwd(&meta), Some("/workspace/project"));
    }
}
