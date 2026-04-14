//! Phase 2: predictive ghost fill detection.
//!
//! Scores fills at the instant of CLOB match, BEFORE on-chain confirmation.
//! Uses two anomaly factors:
//!
//!   1. Price deviation — `|trade_price - mid_price| / mid_price`
//!   2. Size anomaly    — `trade_size / avg_trade_size` over rolling window
//!
//! Combined score:
//!
//! ```text
//! score = clamp(price_deviation * 5.0 + log2(size_anomaly) * 0.3, 0.0, 1.0)
//! ```
//!
//! Cold-start handling: if mid price is unknown or fewer than 5 trades are
//! recorded for a market, no warning is emitted.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tracing::debug;

use crate::logging::JsonlWriter;
use crate::types::PredictiveWarning;
use crate::ws::ClobFill;

/// Minimum number of fills required before size_anomaly is meaningful.
const MIN_SAMPLES: usize = 5;

/// Per-market rolling state.
#[derive(Debug, Clone)]
pub struct MarketStats {
    pub mid_price: Option<f64>,
    fill_sizes: VecDeque<f64>,
    window_cap: usize,
    running_sum: f64,
}

impl MarketStats {
    pub fn new(window_cap: usize) -> Self {
        Self {
            mid_price: None,
            fill_sizes: VecDeque::with_capacity(window_cap.max(1)),
            window_cap: window_cap.max(1),
            running_sum: 0.0,
        }
    }

    /// Update mid price from best bid / best ask. Ignores non-positive values.
    pub fn update_mid(&mut self, best_bid: f64, best_ask: f64) {
        if best_bid > 0.0 && best_ask > 0.0 {
            self.mid_price = Some((best_bid + best_ask) / 2.0);
        }
    }

    /// Record a fill size into the rolling window.
    pub fn record_size(&mut self, size: f64) {
        if size <= 0.0 {
            return;
        }
        if self.fill_sizes.len() >= self.window_cap {
            if let Some(old) = self.fill_sizes.pop_front() {
                self.running_sum -= old;
            }
        }
        self.fill_sizes.push_back(size);
        self.running_sum += size;
    }

    /// Average fill size over the rolling window.
    /// Returns `None` if the window holds fewer than `MIN_SAMPLES` trades.
    pub fn avg_size(&self) -> Option<f64> {
        if self.fill_sizes.len() < MIN_SAMPLES {
            return None;
        }
        Some(self.running_sum / self.fill_sizes.len() as f64)
    }

    pub fn sample_count(&self) -> usize {
        self.fill_sizes.len()
    }
}

pub type SharedStats = Arc<RwLock<HashMap<String, MarketStats>>>;

/// Predictive scorer. Holds shared per-market state.
pub struct Predictor {
    stats: SharedStats,
    threshold: f64,
    window_cap: usize,
    warning_log: Option<Arc<JsonlWriter>>,
}

impl Predictor {
    pub fn new(threshold: f64, window_cap: usize, warning_log: Option<Arc<JsonlWriter>>) -> Self {
        Self {
            stats: Arc::new(RwLock::new(HashMap::new())),
            threshold,
            window_cap: window_cap.max(1),
            warning_log,
        }
    }

    pub fn stats(&self) -> SharedStats {
        self.stats.clone()
    }

    /// Update mid price for a market.
    pub async fn ingest_price(&self, market: &str, best_bid: f64, best_ask: f64) {
        let mut map = self.stats.write().await;
        let entry = map
            .entry(market.to_string())
            .or_insert_with(|| MarketStats::new(self.window_cap));
        entry.update_mid(best_bid, best_ask);
    }

    /// Score a fill. Returns `Some(warning)` if `score >= threshold`.
    ///
    /// The fill's own size is recorded into stats AFTER scoring so it can't
    /// influence its own anomaly score. Returns `None` (and does not record)
    /// when the trade has invalid data.
    pub async fn score_fill(&self, fill: &ClobFill) -> Option<PredictiveWarning> {
        if fill.size <= 0.0 || fill.price <= 0.0 {
            return None;
        }

        // Read current stats under read lock, compute factors.
        let (mid, avg) = {
            let map = self.stats.read().await;
            let snap = map.get(&fill.market);
            let mid = snap.and_then(|s| s.mid_price);
            let avg = snap.and_then(|s| s.avg_size());
            (mid, avg)
        };

        // Record this fill's size for future scoring, regardless of whether
        // we have enough data to score it yet.
        {
            let mut map = self.stats.write().await;
            let entry = map
                .entry(fill.market.clone())
                .or_insert_with(|| MarketStats::new(self.window_cap));
            entry.record_size(fill.size);
        }

        // Cold start — no warning if we can't compute either factor.
        let mid_price = mid?;
        let avg_size = avg?;
        if mid_price <= 0.0 || avg_size <= 0.0 {
            return None;
        }

        let price_deviation = (fill.price - mid_price).abs() / mid_price;
        let size_anomaly = fill.size / avg_size;

        let raw = price_deviation * 5.0 + size_anomaly.log2() * 0.3;
        let score = raw.clamp(0.0, 1.0);

        debug!(
            tx = ?fill.tx_hash,
            market = %fill.market,
            score,
            price_deviation,
            size_anomaly,
            "predictive score computed"
        );

        if score < self.threshold {
            return None;
        }

        let warning = PredictiveWarning {
            tx_hash: fill.tx_hash,
            market: fill.market.clone(),
            score,
            price_deviation,
            size_anomaly,
            trade_price: fill.price,
            mid_price,
            trade_size: fill.size,
            avg_size,
            ts: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        };

        if let Some(ref log) = self.warning_log {
            let line = serde_json::json!({
                "ts": warning.ts,
                "kind": "predictive",
                "tx": format!("{:?}", warning.tx_hash),
                "market": warning.market,
                "score": warning.score,
                "price_dev": warning.price_deviation,
                "size_anom": warning.size_anomaly,
                "trade_price": warning.trade_price,
                "mid_price": warning.mid_price,
                "trade_size": warning.trade_size,
                "avg_size": warning.avg_size,
            });
            if let Err(e) = log.append(&line).await {
                tracing::warn!(error = %e, "failed to append predictive warning log");
            }
        }

        Some(warning)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ethers::types::H256;

    fn fill(market: &str, size: f64, price: f64) -> ClobFill {
        ClobFill {
            tx_hash: H256::zero(),
            market: market.into(),
            side: "BUY".into(),
            size,
            price,
        }
    }

    #[tokio::test]
    async fn test_cold_start_no_warning() {
        let p = Predictor::new(0.7, 50, None);
        // No price, no samples — nothing.
        let w = p.score_fill(&fill("m", 100.0, 0.5)).await;
        assert!(w.is_none());
    }

    #[tokio::test]
    async fn test_mid_price_zero_skipped() {
        let p = Predictor::new(0.7, 50, None);
        // update_mid ignores bid=0, so mid stays None → no warning.
        p.ingest_price("m", 0.0, 0.52).await;
        // seed enough samples for avg to exist
        for _ in 0..10 {
            p.score_fill(&fill("m", 10.0, 0.5)).await;
        }
        let w = p.score_fill(&fill("m", 1000.0, 0.95)).await;
        assert!(w.is_none(), "no mid price → no warning");
    }

    #[tokio::test]
    async fn test_normal_fill_no_warning() {
        let p = Predictor::new(0.7, 50, None);
        p.ingest_price("m", 0.48, 0.52).await; // mid = 0.50
                                               // Seed rolling window with normal-sized trades.
        for _ in 0..10 {
            p.score_fill(&fill("m", 15.0, 0.50)).await;
        }
        // Fill close to mid, size near average → score well below threshold.
        let w = p.score_fill(&fill("m", 15.0, 0.51)).await;
        assert!(w.is_none(), "normal fill should not warn, got {w:?}");
    }

    #[tokio::test]
    async fn test_suspicious_fill_triggers_warning() {
        let p = Predictor::new(0.7, 50, None);
        p.ingest_price("m", 0.48, 0.52).await; // mid = 0.50
        for _ in 0..10 {
            p.score_fill(&fill("m", 15.0, 0.50)).await;
        }
        // price_deviation = |0.95 - 0.50|/0.50 = 0.90 → contribution = 4.5
        // size_anomaly   = 1000/15 ≈ 66.7 → log2≈ 6.06 → contribution ≈ 1.82
        // raw = 4.5 + 1.82 = 6.32 → clamped to 1.0
        let w = p
            .score_fill(&fill("m", 1000.0, 0.95))
            .await
            .expect("expected warning");

        assert!((w.score - 1.0).abs() < 1e-9);
        assert!(w.price_deviation > 0.85);
        assert!(w.size_anomaly > 50.0);
    }

    #[tokio::test]
    async fn test_below_threshold_no_warning() {
        // High threshold → even suspicious fill doesn't warn.
        let p = Predictor::new(0.99, 50, None);
        p.ingest_price("m", 0.48, 0.52).await;
        for _ in 0..10 {
            p.score_fill(&fill("m", 15.0, 0.50)).await;
        }
        // Mildly anomalous: score ≈ 0.1 * 5 + log2(3)*0.3 ≈ 0.975.
        // At threshold 0.99 this may or may not trigger — use clearly sub-threshold.
        let w = p.score_fill(&fill("m", 16.0, 0.51)).await;
        assert!(w.is_none());
    }

    #[tokio::test]
    async fn test_small_trade_does_not_inflate_score() {
        let p = Predictor::new(0.7, 50, None);
        p.ingest_price("m", 0.48, 0.52).await;
        for _ in 0..10 {
            p.score_fill(&fill("m", 100.0, 0.50)).await;
        }
        // Small trade (below average): log2(0.1) ≈ -3.32 → contributes negatively.
        let w = p.score_fill(&fill("m", 10.0, 0.51)).await;
        assert!(w.is_none(), "small trade should not warn");
    }

    #[tokio::test]
    async fn test_zero_size_skipped() {
        let p = Predictor::new(0.7, 50, None);
        p.ingest_price("m", 0.48, 0.52).await;
        for _ in 0..10 {
            p.score_fill(&fill("m", 15.0, 0.50)).await;
        }
        let w = p.score_fill(&fill("m", 0.0, 0.95)).await;
        assert!(w.is_none(), "zero-size trade should be skipped");
    }

    #[test]
    fn test_market_stats_rolling_eviction() {
        let mut s = MarketStats::new(3);
        s.record_size(10.0);
        s.record_size(20.0);
        s.record_size(30.0);
        s.record_size(40.0);
        assert_eq!(s.sample_count(), 3);
        // 10 was evicted, avg of 20,30,40 = 30. But avg_size requires >=5 samples.
        assert!(s.avg_size().is_none());
    }

    #[test]
    fn test_market_stats_avg_requires_min_samples() {
        let mut s = MarketStats::new(50);
        for v in [1.0, 2.0, 3.0, 4.0] {
            s.record_size(v);
        }
        assert!(s.avg_size().is_none());
        s.record_size(5.0);
        let avg = s.avg_size().expect("5 samples enough");
        assert!((avg - 3.0).abs() < 1e-9);
    }
}
