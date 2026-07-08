use agent_surface_contract::SERVER_INSTRUCTIONS;
use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::io::AsyncReadExt;

use crate::{
    config::{RuntimeConfig, SurfaceArgs, SurfaceCallArgs, SurfaceCommand},
    protocol::BrokerRequestContext,
    surface::{SurfaceToolResult, VisibleBrowserLabSurface},
};

pub async fn run(config: RuntimeConfig, args: SurfaceArgs) -> Result<()> {
    let surface = VisibleBrowserLabSurface::new(config)?;

    match args.command {
        SurfaceCommand::Catalog => write_json(&json!({
            "server_instructions": SERVER_INSTRUCTIONS,
            "tools": surface.tools(),
        })),
        SurfaceCommand::Call(args) => call_tool(&surface, args).await,
    }
}

async fn call_tool(surface: &VisibleBrowserLabSurface, args: SurfaceCallArgs) -> Result<()> {
    let (arguments, context) = read_request(args.request_envelope_version).await?;
    if args.request_envelope_version.is_some() && args.workspace_root.is_some() {
        bail!("--workspace-root cannot be combined with --request-envelope-version");
    }
    let context = match args.workspace_root {
        Some(workspace_root) => BrokerRequestContext {
            conversation_identity: context.conversation_identity,
            workspace_root: Some(workspace_root),
        },
        None => context,
    };
    let result = surface.call_tool(&args.tool, arguments, context).await;
    let output = match result {
        SurfaceToolResult::Structured(result) => json!({
            "ok": true,
            "result": result,
        }),
        SurfaceToolResult::StructuredError(error) => json!({
            "ok": false,
            "error": error,
        }),
    };

    write_json(&output)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SurfaceRequestEnvelope {
    arguments: Map<String, Value>,
    #[serde(default)]
    context: BrokerRequestContext,
}

async fn read_request(
    envelope_version: Option<u32>,
) -> Result<(Map<String, Value>, BrokerRequestContext)> {
    let mut input = String::new();
    tokio::io::stdin()
        .read_to_string(&mut input)
        .await
        .context("failed to read tool arguments from stdin")?;
    parse_request(&input, envelope_version)
}

fn parse_request(
    input: &str,
    envelope_version: Option<u32>,
) -> Result<(Map<String, Value>, BrokerRequestContext)> {
    if input.trim().is_empty() {
        return match envelope_version {
            None => Ok((Map::new(), BrokerRequestContext::default())),
            Some(_) => bail!("request envelope JSON is required"),
        };
    }

    match envelope_version {
        None => {
            let value: Value = serde_json::from_str(input).context("invalid tool argument JSON")?;
            match value {
                Value::Object(arguments) => Ok((arguments, BrokerRequestContext::default())),
                _ => bail!("tool arguments must be a JSON object"),
            }
        }
        Some(1) => {
            let envelope: SurfaceRequestEnvelope =
                serde_json::from_str(input).context("invalid request envelope JSON")?;
            Ok((envelope.arguments, envelope.context))
        }
        Some(version) => bail!("unsupported request envelope version `{version}`"),
    }
}

fn write_json(value: &Value) -> Result<()> {
    serde_json::to_writer_pretty(std::io::stdout(), value)?;
    println!();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_surface_calls_still_accept_raw_argument_objects() {
        let (arguments, context) = parse_request(r#"{"topic":"workflow"}"#, None).unwrap();
        assert_eq!(arguments["topic"], "workflow");
        assert_eq!(context, BrokerRequestContext::default());
    }

    #[test]
    fn versioned_envelope_keeps_arguments_and_private_context_separate() {
        let (arguments, context) = parse_request(
            r#"{
                "arguments":{"tab_id":"tab"},
                "context":{
                    "conversation_identity":{
                        "version":1,
                        "issuer":"com.microsoft.vscode",
                        "id":"vscode-chat-session://session"
                    },
                    "workspace_root":"/workspace/project"
                }
            }"#,
            Some(1),
        )
        .unwrap();
        assert_eq!(arguments.len(), 1);
        assert!(arguments.get("conversation_identity").is_none());
        let identity = context.conversation_identity.unwrap();
        assert_eq!(identity.issuer(), "com.microsoft.vscode");
        assert_eq!(identity.id(), "vscode-chat-session://session");
        assert_eq!(
            context.workspace_root.unwrap(),
            std::path::PathBuf::from("/workspace/project")
        );
    }

    #[test]
    fn unknown_envelope_versions_fail_closed() {
        assert!(parse_request("{}", Some(2)).is_err());
    }
}
