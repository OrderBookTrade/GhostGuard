use anyhow::Result;
use ethers::types::Address;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, warn};

/// Cached wallet profile from Polymarket API.
#[derive(Debug, Clone)]
pub struct WalletProfile {
    /// Total historical volume in USDC.
    pub total_volume: f64,
    /// Number of historical trades.
    pub trade_count: u64,
    /// When this cache entry was fetched.
    pub fetched_at: Instant,
}

/// Async LRU-style cache for wallet history lookups.
pub struct WalletCache {
    cache: HashMap<Address, WalletProfile>,
    api_base_url: String,
    timeout: Duration,
    /// How long a cached entry remains valid.
    ttl: Duration,
    http_client: reqwest::Client,
}

impl WalletCache {
    pub fn new(api_base_url: String, timeout: Duration) -> Self {
        Self {
            cache: HashMap::new(),
            api_base_url,
            timeout,
            ttl: Duration::from_secs(30 * 60), // 30 minutes
            http_client: reqwest::Client::new(),
        }
    }

    /// Look up wallet history. Returns cached if fresh, otherwise queries API.
    /// Returns None on API error or timeout (caller should skip wallet factor).
    pub async fn lookup(&mut self, address: Address) -> Option<WalletProfile> {
        // Check cache first
        if let Some(cached) = self.cache.get(&address) {
            if cached.fetched_at.elapsed() < self.ttl {
                debug!(?address, "wallet cache hit");
                return Some(cached.clone());
            }
            // Expired — will re-fetch below
        }

        // Query Polymarket API
        match self.fetch_from_api(address).await {
            Ok(profile) => {
                self.cache.insert(address, profile.clone());
                Some(profile)
            }
            Err(e) => {
                warn!(?address, error = %e, "wallet history lookup failed");
                None
            }
        }
    }

    /// Query the Polymarket CLOB API for wallet trade history.
    async fn fetch_from_api(&self, address: Address) -> Result<WalletProfile> {
        let url = format!(
            "{}/data/trade-history?maker=0x{:x}&limit=1",
            self.api_base_url, address
        );

        let resp = self
            .http_client
            .get(&url)
            .timeout(self.timeout)
            .send()
            .await?;

        // The API returns trade history; we extract volume/count summary.
        // Actual response schema may vary — this handles common shapes.
        let body: serde_json::Value = resp.json().await?;

        let (total_volume, trade_count) = parse_trade_history(&body);

        Ok(WalletProfile {
            total_volume,
            trade_count,
            fetched_at: Instant::now(),
        })
    }

    /// Compute wallet risk factor from a WalletProfile.
    /// 0.0 = very established (high volume, many trades)
    /// 1.0 = brand new wallet (no history)
    pub fn compute_wallet_risk(profile: &WalletProfile) -> f64 {
        // Decay curve: risk = 1 / (1 + trade_count / 50)
        // 0 trades → 1.0, 50 trades → 0.5, 200 trades → ~0.2
        let trade_factor = 1.0 / (1.0 + profile.trade_count as f64 / 50.0);

        // Also factor in volume: < $50 total is suspicious
        let volume_factor = 1.0 / (1.0 + profile.total_volume / 500.0);

        // Average the two signals
        (trade_factor + volume_factor) / 2.0
    }
}

/// Parse trade history response to extract volume and count.
fn parse_trade_history(body: &serde_json::Value) -> (f64, u64) {
    // Handle array of trades
    if let Some(trades) = body.as_array() {
        let count = trades.len() as u64;
        let volume: f64 = trades
            .iter()
            .filter_map(|t| {
                t.get("size")
                    .and_then(|s| s.as_str().and_then(|s| s.parse::<f64>().ok()))
                    .or_else(|| t.get("size").and_then(|s| s.as_f64()))
            })
            .sum();
        return (volume, count);
    }

    // Handle { data: [...] } wrapper
    if let Some(data) = body.get("data").and_then(|d| d.as_array()) {
        let count = data.len() as u64;
        let volume: f64 = data
            .iter()
            .filter_map(|t| {
                t.get("size")
                    .and_then(|s| s.as_str().and_then(|s| s.parse::<f64>().ok()))
                    .or_else(|| t.get("size").and_then(|s| s.as_f64()))
            })
            .sum();
        return (volume, count);
    }

    // Handle { total_volume, trade_count } summary
    let volume = body
        .get("total_volume")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let count = body
        .get("trade_count")
        .and_then(|v| v.as_u64())
        .unwrap_or(0);

    (volume, count)
}

/// Shared handle to WalletCache.
pub type SharedWalletCache = Arc<RwLock<WalletCache>>;

pub fn new_shared_wallet_cache(api_base_url: String, timeout: Duration) -> SharedWalletCache {
    Arc::new(RwLock::new(WalletCache::new(api_base_url, timeout)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_wallet_risk_no_history() {
        let profile = WalletProfile {
            total_volume: 0.0,
            trade_count: 0,
            fetched_at: Instant::now(),
        };
        let risk = WalletCache::compute_wallet_risk(&profile);
        assert!((risk - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_wallet_risk_established() {
        let profile = WalletProfile {
            total_volume: 50000.0,
            trade_count: 500,
            fetched_at: Instant::now(),
        };
        let risk = WalletCache::compute_wallet_risk(&profile);
        // trade_factor ≈ 0.091, volume_factor ≈ 0.0099 → avg ≈ 0.05
        assert!(risk < 0.1);
    }

    #[test]
    fn test_wallet_risk_moderate() {
        let profile = WalletProfile {
            total_volume: 200.0,
            trade_count: 50,
            fetched_at: Instant::now(),
        };
        let risk = WalletCache::compute_wallet_risk(&profile);
        // trade_factor = 0.5, volume_factor ≈ 0.714 → avg ≈ 0.607
        assert!(risk > 0.4 && risk < 0.8);
    }

    #[test]
    fn test_parse_trade_history_array() {
        let body: serde_json::Value = serde_json::json!([
            {"size": "100.0", "price": "0.5"},
            {"size": "200.0", "price": "0.6"},
        ]);
        let (volume, count) = parse_trade_history(&body);
        assert_eq!(count, 2);
        assert!((volume - 300.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_parse_trade_history_empty() {
        let body: serde_json::Value = serde_json::json!([]);
        let (volume, count) = parse_trade_history(&body);
        assert_eq!(count, 0);
        assert!((volume - 0.0).abs() < f64::EPSILON);
    }
}
