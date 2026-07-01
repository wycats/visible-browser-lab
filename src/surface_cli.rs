use agent_surface_contract::SERVER_INSTRUCTIONS;
use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use tokio::io::AsyncReadExt;

use crate::{
    config::{RuntimeConfig, SurfaceArgs, SurfaceCallArgs, SurfaceCommand},
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
    let arguments = read_arguments().await?;
    let workspace_root = args
        .workspace_root
        .map(|path| path.to_string_lossy().into_owned());
    let result = surface
        .call_tool(&args.tool, arguments, workspace_root)
        .await;
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

async fn read_arguments() -> Result<Map<String, Value>> {
    let mut input = String::new();
    tokio::io::stdin()
        .read_to_string(&mut input)
        .await
        .context("failed to read tool arguments from stdin")?;
    if input.trim().is_empty() {
        return Ok(Map::new());
    }

    let value: Value = serde_json::from_str(&input).context("invalid tool argument JSON")?;
    match value {
        Value::Object(arguments) => Ok(arguments),
        _ => bail!("tool arguments must be a JSON object"),
    }
}

fn write_json(value: &Value) -> Result<()> {
    serde_json::to_writer_pretty(std::io::stdout(), value)?;
    println!();
    Ok(())
}
