use anyhow::{Context, Result};
use rmcp::{ServiceExt, transport::stdio};

#[tokio::main]
async fn main() -> Result<()> {
    let fixture_id = std::env::var("VISIBLE_BROWSER_LAB_EVAL_FIXTURE")
        .context("VISIBLE_BROWSER_LAB_EVAL_FIXTURE is required")?;
    let log_path = std::env::var("VISIBLE_BROWSER_LAB_EVAL_LOG")
        .context("VISIBLE_BROWSER_LAB_EVAL_LOG is required")?;
    let server = agent_surface_eval::server::EvaluationServer::new(&fixture_id, log_path.into())?;
    server.serve(stdio()).await?.waiting().await?;
    Ok(())
}
