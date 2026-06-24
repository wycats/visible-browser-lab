use anyhow::{Context, Result};
use chromiumoxide::{
    Browser, cdp::browser_protocol::target::CreateTargetParams, handler::HandlerConfig,
};
use futures_util::StreamExt;

async fn exercise_core_api(endpoint: &str) -> Result<()> {
    let (browser, mut handler) = Browser::connect_with_config(
        endpoint,
        HandlerConfig {
            viewport: None,
            ..HandlerConfig::default()
        },
    )
    .await
    .context("connect failed")?;
    let handler_task = tokio::spawn(async move {
        while let Some(result) = handler.next().await {
            result?;
        }
        Ok::<(), chromiumoxide::error::CdpError>(())
    });
    let params = CreateTargetParams::builder()
        .url("about:blank")
        .background(true)
        .build()
        .map_err(anyhow::Error::msg)?;
    let target = browser.execute(params).await?.result.target_id;
    let _ = browser.get_page(target).await?;
    drop(browser);
    handler_task.abort();
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    if let Some(endpoint) = std::env::args().nth(1) {
        exercise_core_api(&endpoint).await?;
    }
    Ok(())
}
