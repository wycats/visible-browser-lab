use std::{collections::BTreeMap, sync::Arc};

use crate::{
    config::RuntimeConfig,
    conversation_identity::{ConversationIdentityCompatibility, normalize_metadata},
    leases::BrowserToolError,
    protocol::BrokerRequestContext,
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
    conversation_identity_compatibility: ConversationIdentityCompatibility,
}

impl VisibleBrowserLab {
    fn new(
        config: RuntimeConfig,
        conversation_identity_compatibility: ConversationIdentityCompatibility,
    ) -> Result<Self> {
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
            conversation_identity_compatibility,
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
        let request_context = match broker_request_context(
            call_tool_meta(request.meta.as_ref(), &context.meta),
            self.conversation_identity_compatibility,
        ) {
            Ok(context) => context,
            Err(error) => {
                return Ok(call_tool_result(SurfaceToolResult::structured_error(
                    serde_json::to_value(error).unwrap_or_else(|_| {
                        serde_json::json!({
                            "code":"invalid_request_context",
                            "message":"invalid conversation identity metadata"
                        })
                    }),
                )));
            }
        };
        let result = self
            .surface
            .call_tool(name, arguments, request_context)
            .await;

        Ok(call_tool_result(result))
    }
}

pub async fn run(
    config: RuntimeConfig,
    conversation_identity_compatibility: ConversationIdentityCompatibility,
) -> Result<()> {
    let server = VisibleBrowserLab::new(config, conversation_identity_compatibility)?;
    server.serve(stdio()).await?.waiting().await?;
    Ok(())
}

fn call_tool_result(result: SurfaceToolResult) -> CallToolResult {
    match result {
        SurfaceToolResult::Structured(value) => CallToolResult::structured(value),
        SurfaceToolResult::StructuredError(value) => CallToolResult::structured_error(value),
    }
}

fn workspace_root(meta: &Meta) -> Option<std::path::PathBuf> {
    codex_workspace_cwd(meta)
        .map(std::path::PathBuf::from)
        .or_else(|| non_empty_env("CLAUDE_PROJECT_DIR").map(std::path::PathBuf::from))
        .or_else(|| {
            non_empty_env("VISIBLE_BROWSER_LAB_WORKSPACE_ROOT").map(std::path::PathBuf::from)
        })
}

fn broker_request_context(
    meta: &Meta,
    compatibility: ConversationIdentityCompatibility,
) -> Result<BrokerRequestContext, BrowserToolError> {
    let conversation_identity = normalize_metadata(&meta.0, compatibility)
        .map_err(|error| BrowserToolError::invalid_request_context(error.to_string()))?;
    Ok(BrokerRequestContext {
        conversation_identity,
        workspace_root: workspace_root(meta),
    })
}

fn call_tool_meta<'a>(request_meta: Option<&'a Meta>, context_meta: &'a Meta) -> &'a Meta {
    request_meta.unwrap_or(context_meta)
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
        let server = VisibleBrowserLab::new(
            config,
            ConversationIdentityCompatibility::TrustedCodexThreadId,
        )
        .unwrap();
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

    #[test]
    fn extracts_canonical_and_guarded_codex_conversation_identity() {
        let canonical = Meta(serde_json::Map::from_iter([(
            crate::conversation_identity::CONVERSATION_IDENTITY_META_KEY.to_string(),
            serde_json::json!({"version":1,"issuer":"com.example.host","id":"conversation"}),
        )]));
        let context =
            broker_request_context(&canonical, ConversationIdentityCompatibility::Disabled)
                .unwrap();
        assert_eq!(
            context.conversation_identity.as_ref().unwrap().issuer(),
            "com.example.host"
        );

        let codex = Meta(serde_json::Map::from_iter([(
            "threadId".to_string(),
            serde_json::json!("thread"),
        )]));
        assert!(
            broker_request_context(&codex, ConversationIdentityCompatibility::Disabled)
                .unwrap()
                .conversation_identity
                .is_none()
        );
        assert_eq!(
            broker_request_context(
                &codex,
                ConversationIdentityCompatibility::TrustedCodexThreadId,
            )
            .unwrap()
            .conversation_identity
            .unwrap()
            .issuer(),
            "com.openai.codex"
        );
    }

    #[test]
    fn reads_protocol_metadata_from_the_call_tool_request() {
        let context_meta = Meta(serde_json::Map::from_iter([(
            "threadId".to_string(),
            serde_json::json!("context-thread"),
        )]));
        let mut request: CallToolRequestParams = serde_json::from_value(serde_json::json!({
            "name": "list_tabs",
            "_meta": { "threadId": "request-thread" }
        }))
        .unwrap();

        let selected = call_tool_meta(request.meta.as_ref(), &context_meta);
        assert_eq!(
            selected.0.get("threadId"),
            Some(&serde_json::json!("request-thread"))
        );
        assert_eq!(
            broker_request_context(
                selected,
                ConversationIdentityCompatibility::TrustedCodexThreadId,
            )
            .unwrap()
            .conversation_identity
            .unwrap()
            .issuer(),
            "com.openai.codex"
        );

        request.meta = None;
        assert!(std::ptr::eq(
            call_tool_meta(request.meta.as_ref(), &context_meta),
            &context_meta
        ));
    }
}
