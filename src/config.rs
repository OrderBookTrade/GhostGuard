//! TOML configuration loader for GhostGuard.
//!
//! Precedence (highest first):
//!   1. Explicit CLI flags
//!   2. TOML file (if `--config` was passed)
//!   3. `Config::default()`
//!
//! The TOML shape is a hierarchical superset of the flat runtime `Config`.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::Path;
use std::time::Duration;

use crate::types::Config;

/// TOML-facing configuration shape. All fields optional so partial files work.
#[derive(Debug, Default, Deserialize)]
pub struct FileConfig {
    #[serde(default)]
    pub rpc: Option<RpcSection>,
    #[serde(default)]
    pub clob: Option<ClobSection>,
    #[serde(default)]
    pub detection: Option<DetectionSection>,
    #[serde(default)]
    pub alerts: Option<AlertsSection>,
    // Phase 3 — parsed but unused in this session.
    #[serde(default)]
    pub defense: Option<DefenseSection>,
    // Phase 4 — parsed but unused in this session.
    #[serde(default)]
    pub api: Option<ApiSection>,
}

#[derive(Debug, Default, Deserialize)]
pub struct RpcSection {
    pub url: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ClobSection {
    pub ws_url: Option<String>,
    #[serde(default)]
    pub asset_ids: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct DetectionSection {
    pub verify_timeout_secs: Option<u64>,
    pub poll_interval_ms: Option<u64>,
    pub predictive_enabled: Option<bool>,
    pub predictive_threshold: Option<f64>,
    pub avg_window: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
pub struct AlertsSection {
    pub webhook_url: Option<String>,
    pub verdict_log: Option<String>,
    pub predictive_log: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct DefenseSection {
    pub auto_cancel: Option<bool>,
    pub auto_blacklist: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ApiSection {
    pub listen: Option<String>,
}

impl FileConfig {
    /// Load and parse a TOML config file.
    pub async fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let contents = tokio::fs::read_to_string(path)
            .await
            .with_context(|| format!("read {path:?}"))?;
        let parsed: FileConfig =
            toml::from_str(&contents).with_context(|| format!("parse TOML in {path:?}"))?;
        Ok(parsed)
    }

    /// Merge this file config onto a base `Config`. File values override the base.
    /// The caller should then apply CLI overrides on top of the result.
    pub fn apply_to(self, mut base: Config) -> Config {
        if let Some(rpc) = self.rpc {
            if let Some(url) = rpc.url {
                base.rpc_url = url;
            }
        }
        if let Some(clob) = self.clob {
            if let Some(ws) = clob.ws_url {
                base.clob_ws_url = ws;
            }
            if !clob.asset_ids.is_empty() {
                base.markets = clob.asset_ids;
            }
        }
        if let Some(det) = self.detection {
            if let Some(t) = det.verify_timeout_secs {
                base.verify_timeout = Duration::from_secs(t);
            }
            if let Some(ms) = det.poll_interval_ms {
                base.poll_interval = Duration::from_millis(ms);
            }
            if let Some(b) = det.predictive_enabled {
                base.predictive_enabled = b;
            }
            if let Some(t) = det.predictive_threshold {
                base.predictive_threshold = t;
            }
            if let Some(w) = det.avg_window {
                base.avg_window = w;
            }
        }
        if let Some(alerts) = self.alerts {
            if let Some(w) = alerts.webhook_url {
                base.webhook_url = Some(w);
            }
            if let Some(p) = alerts.verdict_log {
                base.verdict_log = p;
            }
            if let Some(p) = alerts.predictive_log {
                base.predictive_log = p;
            }
        }
        // defense and api sections are parsed but not wired into runtime yet.
        let _ = self.defense;
        let _ = self.api;
        base
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_full_toml() {
        let toml_src = r#"
[rpc]
url = "https://alchemy"

[clob]
ws_url = "wss://custom"
asset_ids = ["aaa", "bbb"]

[detection]
verify_timeout_secs = 15
poll_interval_ms = 250
predictive_enabled = true
predictive_threshold = 0.5
avg_window = 100

[alerts]
webhook_url = "http://hook"
verdict_log = "/tmp/v.jsonl"
predictive_log = "/tmp/p.jsonl"

[defense]
auto_cancel = true
auto_blacklist = false

[api]
listen = "0.0.0.0:9000"
"#;
        let parsed: FileConfig = toml::from_str(toml_src).unwrap();
        let merged = parsed.apply_to(Config::default());

        assert_eq!(merged.rpc_url, "https://alchemy");
        assert_eq!(merged.clob_ws_url, "wss://custom");
        assert_eq!(merged.markets, vec!["aaa".to_string(), "bbb".to_string()]);
        assert_eq!(merged.verify_timeout, Duration::from_secs(15));
        assert_eq!(merged.poll_interval, Duration::from_millis(250));
        assert!(merged.predictive_enabled);
        assert_eq!(merged.predictive_threshold, 0.5);
        assert_eq!(merged.avg_window, 100);
        assert_eq!(merged.webhook_url.as_deref(), Some("http://hook"));
        assert_eq!(merged.verdict_log, "/tmp/v.jsonl");
        assert_eq!(merged.predictive_log, "/tmp/p.jsonl");
    }

    #[test]
    fn test_partial_toml_keeps_defaults() {
        let toml_src = r#"
[detection]
predictive_threshold = 0.9
"#;
        let parsed: FileConfig = toml::from_str(toml_src).unwrap();
        let merged = parsed.apply_to(Config::default());

        assert_eq!(merged.predictive_threshold, 0.9);
        // Unspecified fields unchanged:
        assert_eq!(merged.rpc_url, "https://polygon-rpc.com");
        assert!(!merged.predictive_enabled);
    }

    #[test]
    fn test_empty_toml() {
        let parsed: FileConfig = toml::from_str("").unwrap();
        let merged = parsed.apply_to(Config::default());
        assert_eq!(merged.rpc_url, Config::default().rpc_url);
    }
}
