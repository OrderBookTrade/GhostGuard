use anyhow::Result;
use tracing::{info, warn};

use crate::types::{FillVerdict, GhostFillEvent};

/// Dispatch a fill verdict to a webhook URL via HTTP POST.
pub async fn send_webhook(url: &str, verdict: &FillVerdict) -> Result<()> {
    let client = reqwest::Client::new();
    let resp = client.post(url).json(verdict).send().await?;

    if resp.status().is_success() {
        info!(url, "webhook delivered successfully");
    } else {
        warn!(url, status = %resp.status(), "webhook returned non-2xx");
    }

    Ok(())
}

/// Dispatch a ghost fill event to a webhook URL.
pub async fn send_ghost_event_webhook(url: &str, event: &GhostFillEvent) -> Result<()> {
    let client = reqwest::Client::new();
    let resp = client.post(url).json(event).send().await?;

    if resp.status().is_success() {
        info!(url, "ghost event webhook delivered");
    } else {
        warn!(url, status = %resp.status(), "ghost event webhook non-2xx");
    }

    Ok(())
}
