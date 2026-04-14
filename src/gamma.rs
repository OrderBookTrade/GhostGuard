//! Polymarket Gamma API client.
//!
//! Used to bootstrap the current active market on startup AND as a fallback
//! rotation mechanism (every 30s we re-check — if the server didn't push a
//! `new_market` WebSocket event we still pick up the rollover).
//!
//! ## How we pick "the current cycle"
//!
//! Polymarket's 5-minute rotating markets follow a strict slug convention:
//!
//! ```text
//! {pattern}-{start_unix}
//! ```
//!
//! where `start_unix` is the Unix timestamp of the 5-minute UTC boundary at
//! which the cycle opened. So at 10:57 UTC the current cycle's slug is
//! `btc-updown-5m-1776164100` (1776164100 → 2026-04-14T10:55:00Z).
//!
//! We exploit this to do a direct `/events?slug=...` lookup — one network
//! call, no pagination, no sorting. If that ever fails (e.g. the pattern
//! isn't actually 5-minute aligned) we fall back to fetching a batch of
//! events and filtering client-side by time window.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use std::time::Duration;
use tracing::debug;

/// Information about the currently-active rotating cycle.
#[derive(Debug, Clone)]
pub struct CurrentCycle {
    /// Event slug, e.g. `btc-updown-5m-1776163800`.
    pub slug: String,
    /// Underlying market condition ID (0x...).
    pub condition_id: String,
    pub question: String,
    /// Human-readable event title, e.g.
    /// `"Bitcoin Up or Down - April 14, 7:05AM-7:10AM ET"`.
    pub title: String,
    /// Outcome labels in the same order as `assets_ids` (e.g. `["Up", "Down"]`
    /// or `["Yes", "No"]`).
    pub outcomes: Vec<String>,
    /// CLOB token IDs (YES + NO).
    pub assets_ids: Vec<String>,
    pub start_time: DateTime<Utc>,
    pub end_date: DateTime<Utc>,
}

impl CurrentCycle {
    /// Extract the time-window substring from the title, e.g.
    /// `"7:05AM-7:10AM ET"` from `"Bitcoin Up or Down - April 14, 7:05AM-7:10AM ET"`.
    /// Falls back to an empty string when the title has no `, ` separator.
    pub fn time_window(&self) -> String {
        self.title
            .rsplit_once(", ")
            .map(|(_, rhs)| rhs.trim().to_string())
            .unwrap_or_default()
    }
}

const GAMMA_BASE: &str = "https://gamma-api.polymarket.com";
const CYCLE_PERIOD_SECS: i64 = 300; // 5 minutes

/// Fetch the currently-active cycle whose slug starts with `pattern`.
///
/// Returns `Ok(Some(cycle))` when a matching active cycle is found. Returns
/// `Ok(None)` when nothing matches (e.g. pattern typo, market retired).
pub async fn fetch_current_cycle(pattern: &str) -> Result<Option<CurrentCycle>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("build reqwest client")?;

    // 1. Fast path — directly guess the slug from the current 5-min boundary.
    let now = Utc::now().timestamp();
    let slug_ts = (now / CYCLE_PERIOD_SECS) * CYCLE_PERIOD_SECS;
    let candidate_slug = format!("{pattern}-{slug_ts}");
    debug!(candidate_slug = %candidate_slug, "gamma direct slug lookup");

    if let Some(cycle) = fetch_by_slug(&client, &candidate_slug).await? {
        return Ok(Some(cycle));
    }

    // 2. Fallback — broader search and filter by start/end window. Handles
    //    non-5-minute cycles or slug-scheme drift.
    debug!(pattern = %pattern, "gamma fast-path empty, falling back to list+filter");
    fetch_by_time_window(&client, pattern).await
}

async fn fetch_by_slug(client: &reqwest::Client, slug: &str) -> Result<Option<CurrentCycle>> {
    let url = format!("{GAMMA_BASE}/events?slug={slug}");
    let resp = client
        .get(&url)
        .send()
        .await
        .context("gamma slug request")?
        .error_for_status()
        .context("gamma slug bad status")?;
    let events: serde_json::Value = resp.json().await.context("gamma slug parse")?;
    let Some(arr) = events.as_array() else {
        return Ok(None);
    };
    for ev in arr {
        if let Some(cycle) = parse_event_to_cycle(ev) {
            return Ok(Some(cycle));
        }
    }
    Ok(None)
}

async fn fetch_by_time_window(
    client: &reqwest::Client,
    pattern: &str,
) -> Result<Option<CurrentCycle>> {
    // Pull a healthy batch. Gamma's `slug=` only accepts exact matches so we
    // filter prefixes client-side.
    let url = format!(
        "{GAMMA_BASE}/events?tag_slug=5M&closed=false&limit=500&order=startDate&ascending=true"
    );
    let resp = client
        .get(&url)
        .send()
        .await
        .context("gamma list request")?
        .error_for_status()
        .context("gamma list bad status")?;
    let events: serde_json::Value = resp.json().await.context("gamma list parse")?;

    let arr = events.as_array().cloned().unwrap_or_default();
    let now = Utc::now();

    for ev in arr {
        let slug = ev.get("slug").and_then(|s| s.as_str()).unwrap_or("");
        if !slug.starts_with(pattern) {
            continue;
        }
        if let Some(cycle) = parse_event_to_cycle(&ev) {
            if cycle.start_time <= now && now < cycle.end_date {
                return Ok(Some(cycle));
            }
        }
    }

    Ok(None)
}

/// Convert a Gamma `/events` entry into our `CurrentCycle`. Returns None if
/// any required field is missing.
fn parse_event_to_cycle(ev: &serde_json::Value) -> Option<CurrentCycle> {
    let slug = ev.get("slug").and_then(|s| s.as_str())?.to_string();

    let start_str = ev
        .get("startTime")
        .and_then(|s| s.as_str())
        .or_else(|| ev.get("start_time").and_then(|s| s.as_str()))?;
    let end_str = ev
        .get("endDate")
        .and_then(|s| s.as_str())
        .or_else(|| ev.get("end_date").and_then(|s| s.as_str()))?;

    let start_time = parse_iso(start_str)?;
    let end_date = parse_iso(end_str)?;

    let markets = ev.get("markets").and_then(|m| m.as_array())?;
    let first = markets.first()?;

    let condition_id = first
        .get("conditionId")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    let question = first
        .get("question")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .to_string();
    // Title lives at the top level of the event (not inside markets[0]).
    // Fall back to question when missing.
    let title = ev
        .get("title")
        .and_then(|s| s.as_str())
        .unwrap_or(&question)
        .to_string();

    // clobTokenIds comes as a JSON-encoded STRING holding the array.
    let raw = first
        .get("clobTokenIds")
        .and_then(|s| s.as_str())
        .unwrap_or("[]");
    let assets_ids: Vec<String> = serde_json::from_str(raw).unwrap_or_default();
    if assets_ids.is_empty() {
        return None;
    }

    // Outcomes are also a JSON-encoded STRING (e.g. "[\"Up\",\"Down\"]").
    let outcomes_raw = first
        .get("outcomes")
        .and_then(|s| s.as_str())
        .unwrap_or("[]");
    let outcomes: Vec<String> = serde_json::from_str(outcomes_raw).unwrap_or_default();

    Some(CurrentCycle {
        slug,
        condition_id,
        question,
        title,
        outcomes,
        assets_ids,
        start_time,
        end_date,
    })
}

fn parse_iso(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_iso() {
        let t = parse_iso("2026-04-14T10:50:00Z").unwrap();
        assert_eq!(t.to_rfc3339(), "2026-04-14T10:50:00+00:00");
    }

    #[test]
    fn test_parse_iso_invalid() {
        assert!(parse_iso("not a date").is_none());
        assert!(parse_iso("").is_none());
    }

    #[test]
    fn test_parse_event_to_cycle() {
        let ev = serde_json::json!({
            "slug": "btc-updown-5m-1776164100",
            "title": "Bitcoin Up or Down - April 14, 7:05AM-7:10AM ET",
            "startTime": "2026-04-14T11:05:00Z",
            "endDate": "2026-04-14T11:10:00Z",
            "markets": [{
                "conditionId": "0xabc",
                "question": "Bitcoin Up or Down - April 14, 7:05AM-7:10AM ET",
                "clobTokenIds": "[\"111\",\"222\"]",
                "outcomes": "[\"Up\",\"Down\"]"
            }]
        });
        let cycle = parse_event_to_cycle(&ev).expect("should parse");
        assert_eq!(cycle.slug, "btc-updown-5m-1776164100");
        assert_eq!(cycle.condition_id, "0xabc");
        assert_eq!(cycle.assets_ids, vec!["111".to_string(), "222".to_string()]);
        assert_eq!(cycle.outcomes, vec!["Up".to_string(), "Down".to_string()]);
        assert_eq!(cycle.time_window(), "7:05AM-7:10AM ET");
    }

    #[test]
    fn test_time_window_fallback() {
        let c = CurrentCycle {
            slug: String::new(),
            condition_id: String::new(),
            question: String::new(),
            title: "NoCommaHere".into(),
            outcomes: vec![],
            assets_ids: vec![],
            start_time: Utc::now(),
            end_date: Utc::now(),
        };
        assert_eq!(c.time_window(), "");
    }

    #[test]
    fn test_parse_event_missing_fields() {
        // No markets array → None
        let ev = serde_json::json!({
            "slug": "x",
            "startTime": "2026-01-01T00:00:00Z",
            "endDate": "2026-01-01T00:05:00Z"
        });
        assert!(parse_event_to_cycle(&ev).is_none());
    }

    /// Double-check the 5-min-floor arithmetic: at 10:59:40 the current cycle
    /// slug should be pinned to 10:55:00 (start of the 5-min bucket).
    #[test]
    fn test_slug_floor_math() {
        let now: i64 = 1776164380; // 2026-04-14T10:59:40Z
        let slug_ts = (now / CYCLE_PERIOD_SECS) * CYCLE_PERIOD_SECS;
        assert_eq!(slug_ts, 1776164100); // 10:55:00 UTC
    }
}
