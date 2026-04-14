use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use ethers::types::Address;
use tracing::debug;

use crate::market_stats::{MarketSnapshot, SharedMarketStats};
use crate::types::{Config, GhostRisk, RiskFactors};
use crate::wallet_cache::SharedWalletCache;
use crate::ws::ClobFill;

/// Predictive ghost fill detector.
///
/// Scores fills at the instant of CLOB match, BEFORE chain confirmation.
/// Uses a three-factor model:
/// 1. Price deviation from rolling mid price
/// 2. Size anomaly vs recent fill sizes
/// 3. Wallet history suspicion (optional async lookup)
///
/// This is a heuristic early-warning system; `verify_fill()` remains the
/// definitive check.
pub struct PredictiveDetector {
    config: Arc<Config>,
    stats: SharedMarketStats,
    wallet_cache: Option<SharedWalletCache>,
}

impl PredictiveDetector {
    pub fn new(
        config: Arc<Config>,
        stats: SharedMarketStats,
        wallet_cache: Option<SharedWalletCache>,
    ) -> Self {
        Self {
            config,
            stats,
            wallet_cache,
        }
    }

    /// Score a fill. Main entry point.
    ///
    /// Reads market stats (RwLock read lock -- fast, non-blocking with other reads).
    /// Optionally queries wallet cache (may do HTTP if cache miss).
    /// Returns GhostRisk with per-factor breakdown.
    pub async fn score(&self, fill: &ClobFill) -> GhostRisk {
        // Read market stats snapshot under read lock
        let (price_factor, size_factor) = {
            let store = self.stats.read().await;
            let price_factor = store
                .get(&fill.market)
                .and_then(|snap| self.compute_price_factor(snap, fill.price))
                .unwrap_or(0.0);

            let size_factor = store
                .get(&fill.market)
                .and_then(|snap| self.compute_size_factor(snap, fill.size))
                .unwrap_or(0.0);

            (price_factor, size_factor)
        }; // read lock released here

        // Wallet factor (async, optional)
        let wallet_factor = self.compute_wallet_factor(fill.taker).await;

        let factors = RiskFactors {
            price_deviation: price_factor,
            size_anomaly: size_factor,
            wallet_risk: wallet_factor,
        };

        let score = self.combine_factors(&factors);

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        debug!(
            ?fill.tx_hash,
            score,
            price_factor,
            size_factor,
            ?wallet_factor,
            "predictive score computed"
        );

        GhostRisk {
            score,
            factors,
            market: fill.market.clone(),
            tx_hash: fill.tx_hash,
            scored_at: now,
        }
    }

    /// Price deviation factor.
    /// 0.0 if no mid price data available (insufficient data = not suspicious).
    fn compute_price_factor(&self, snapshot: &MarketSnapshot, fill_price: f64) -> Option<f64> {
        let deviation = snapshot.price_deviation_fraction(fill_price)?;
        // Scale: 0 at mid, 1.0 at price_deviation_extreme or beyond
        Some((deviation / self.config.price_deviation_extreme).min(1.0))
    }

    /// Size anomaly factor.
    /// 0.0 if insufficient historical data.
    fn compute_size_factor(&self, snapshot: &MarketSnapshot, fill_size: f64) -> Option<f64> {
        let z = snapshot.size_zscore(fill_size)?;
        // Only positive z-scores matter (large fills are suspicious, not small ones)
        if z <= 0.0 {
            return Some(0.0);
        }
        Some((z / self.config.size_zscore_extreme).min(1.0))
    }

    /// Wallet history factor. Returns None if wallet lookup is disabled
    /// or taker address is unavailable.
    async fn compute_wallet_factor(&self, taker: Option<Address>) -> Option<f64> {
        if !self.config.wallet_lookup_enabled {
            return None;
        }
        let address = taker?;
        let cache = self.wallet_cache.as_ref()?;

        let mut cache_guard = cache.write().await;
        let profile = cache_guard.lookup(address).await?;
        Some(crate::wallet_cache::WalletCache::compute_wallet_risk(&profile))
    }

    /// Combine factors into a single score using configured weights.
    /// If wallet factor is None, redistribute its weight proportionally.
    fn combine_factors(&self, factors: &RiskFactors) -> f64 {
        match factors.wallet_risk {
            Some(wallet) => {
                factors.price_deviation * self.config.weight_price
                    + factors.size_anomaly * self.config.weight_size
                    + wallet * self.config.weight_wallet
            }
            None => {
                // Redistribute wallet weight to other two factors
                let total = self.config.weight_price + self.config.weight_size;
                if total < f64::EPSILON {
                    return 0.0;
                }
                let w_price = self.config.weight_price / total;
                let w_size = self.config.weight_size / total;
                factors.price_deviation * w_price + factors.size_anomaly * w_size
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market_stats::new_shared_stats;
    use crate::types::Config;
    use ethers::types::H256;


    fn test_config() -> Config {
        Config {
            predictive_enabled: true,
            risk_threshold: 0.7,
            weight_price: 0.4,
            weight_size: 0.4,
            weight_wallet: 0.2,
            price_deviation_extreme: 0.15,
            size_zscore_extreme: 3.0,
            stats_window_size: 100,
            ..Default::default()
        }
    }

    fn test_fill(price: f64, size: f64) -> ClobFill {
        ClobFill {
            tx_hash: H256::zero(),
            market: "test-market".into(),
            side: "BUY".into(),
            size,
            price,
            taker: None,
        }
    }

    #[tokio::test]
    async fn test_score_no_data() {
        let config = Arc::new(test_config());
        let stats = new_shared_stats(100);
        let detector = PredictiveDetector::new(config, stats, None);

        let fill = test_fill(0.95, 1000.0);
        let risk = detector.score(&fill).await;

        // No market data → all factors 0 → score 0
        assert!((risk.score - 0.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn test_score_normal_fill() {
        let config = Arc::new(test_config());
        let stats = new_shared_stats(100);

        // Seed market data
        {
            let mut store = stats.write().await;
            store.update_price("test-market", 0.48, 0.52); // mid = 0.50
            for size in &[10.0, 15.0, 12.0, 20.0, 18.0, 11.0, 14.0] {
                store.record_fill("test-market", *size);
            }
        }

        let detector = PredictiveDetector::new(config, stats, None);

        // Normal fill: price near mid, normal size
        let fill = test_fill(0.51, 15.0);
        let risk = detector.score(&fill).await;

        // Should be low risk
        assert!(risk.score < 0.3, "expected low risk, got {}", risk.score);
    }

    #[tokio::test]
    async fn test_score_suspicious_fill() {
        let config = Arc::new(test_config());
        let stats = new_shared_stats(100);

        // Seed market data
        {
            let mut store = stats.write().await;
            store.update_price("test-market", 0.48, 0.52); // mid = 0.50
            for size in &[10.0, 15.0, 12.0, 20.0, 18.0, 11.0, 14.0] {
                store.record_fill("test-market", *size);
            }
        }

        let detector = PredictiveDetector::new(config, stats, None);

        // Suspicious fill: price far from mid + huge size
        let fill = test_fill(0.95, 500.0);
        let risk = detector.score(&fill).await;

        // Should be high risk
        assert!(risk.score > 0.7, "expected high risk, got {}", risk.score);
        assert!(risk.factors.price_deviation > 0.9);
        assert!(risk.factors.size_anomaly > 0.9);
    }

    #[test]
    fn test_combine_factors_with_wallet() {
        let config = Arc::new(test_config());
        let stats = new_shared_stats(100);
        let detector = PredictiveDetector::new(config, stats, None);

        let factors = RiskFactors {
            price_deviation: 1.0,
            size_anomaly: 1.0,
            wallet_risk: Some(1.0),
        };
        let score = detector.combine_factors(&factors);
        assert!((score - 1.0).abs() < f64::EPSILON);

        let factors = RiskFactors {
            price_deviation: 0.5,
            size_anomaly: 0.5,
            wallet_risk: Some(0.5),
        };
        let score = detector.combine_factors(&factors);
        assert!((score - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn test_combine_factors_without_wallet() {
        let config = Arc::new(test_config());
        let stats = new_shared_stats(100);
        let detector = PredictiveDetector::new(config, stats, None);

        // Without wallet, weights redistribute: price=0.5, size=0.5
        let factors = RiskFactors {
            price_deviation: 1.0,
            size_anomaly: 0.0,
            wallet_risk: None,
        };
        let score = detector.combine_factors(&factors);
        assert!((score - 0.5).abs() < f64::EPSILON);
    }
}
