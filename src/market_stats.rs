use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Per-market rolling statistics used for predictive scoring.
#[derive(Debug, Clone)]
pub struct MarketSnapshot {
    /// Current mid price (average of best bid and best ask).
    pub mid_price: Option<f64>,
    /// Rolling window of recent fill sizes.
    fill_sizes: VecDeque<f64>,
    /// Maximum window size.
    window_size: usize,
    /// Running sum for incremental mean.
    running_sum: f64,
    /// Running sum of squared deviations (Welford's).
    running_m2: f64,
    /// Count of values seen (may exceed window_size; tracks total inserts).
    count: usize,
}

impl MarketSnapshot {
    pub fn new(window_size: usize) -> Self {
        Self {
            mid_price: None,
            fill_sizes: VecDeque::with_capacity(window_size),
            window_size,
            running_sum: 0.0,
            running_m2: 0.0,
            count: 0,
        }
    }

    /// Update mid price from a bid/ask spread.
    pub fn update_mid_price(&mut self, best_bid: f64, best_ask: f64) {
        if best_bid > 0.0 && best_ask > 0.0 {
            self.mid_price = Some((best_bid + best_ask) / 2.0);
        }
    }

    /// Record a new fill size, maintaining the rolling window.
    pub fn record_fill_size(&mut self, size: f64) {
        // Evict oldest if at capacity
        if self.fill_sizes.len() >= self.window_size {
            if let Some(old) = self.fill_sizes.pop_front() {
                self.running_sum -= old;
            }
        }
        self.fill_sizes.push_back(size);
        self.running_sum += size;
        self.count += 1;
    }

    /// Number of fills in the rolling window.
    pub fn window_len(&self) -> usize {
        self.fill_sizes.len()
    }

    /// Mean fill size over the rolling window. Returns None if empty.
    pub fn mean_fill_size(&self) -> Option<f64> {
        if self.fill_sizes.is_empty() {
            return None;
        }
        Some(self.running_sum / self.fill_sizes.len() as f64)
    }

    /// Standard deviation of fill sizes over the rolling window.
    /// Returns None if fewer than 2 samples.
    pub fn stddev_fill_size(&self) -> Option<f64> {
        let n = self.fill_sizes.len();
        if n < 2 {
            return None;
        }
        let mean = self.running_sum / n as f64;
        let variance = self
            .fill_sizes
            .iter()
            .map(|&x| (x - mean).powi(2))
            .sum::<f64>()
            / (n - 1) as f64;
        Some(variance.sqrt())
    }

    /// Compute z-score of a given size against the rolling window.
    /// Returns None if insufficient data (< 5 fills).
    pub fn size_zscore(&self, size: f64) -> Option<f64> {
        if self.fill_sizes.len() < 5 {
            return None;
        }
        let mean = self.mean_fill_size()?;
        let stddev = self.stddev_fill_size()?;
        if stddev < f64::EPSILON {
            // All values are identical — any deviation is infinite
            return if (size - mean).abs() < f64::EPSILON {
                Some(0.0)
            } else {
                Some(10.0) // cap at extreme
            };
        }
        Some((size - mean) / stddev)
    }

    /// Price deviation as a fraction of mid price.
    /// Returns None if mid price is not known.
    pub fn price_deviation_fraction(&self, fill_price: f64) -> Option<f64> {
        let mid = self.mid_price?;
        if mid < f64::EPSILON {
            return None;
        }
        Some((fill_price - mid).abs() / mid)
    }
}

/// Container for per-market statistics.
pub struct MarketStatsStore {
    stats: HashMap<String, MarketSnapshot>,
    window_size: usize,
}

impl MarketStatsStore {
    pub fn new(window_size: usize) -> Self {
        Self {
            stats: HashMap::new(),
            window_size,
        }
    }

    /// Get or create a MarketSnapshot for a market.
    pub fn get_or_create(&mut self, market: &str) -> &mut MarketSnapshot {
        self.stats
            .entry(market.to_string())
            .or_insert_with(|| MarketSnapshot::new(self.window_size))
    }

    /// Get snapshot for reading (immutable).
    pub fn get(&self, market: &str) -> Option<&MarketSnapshot> {
        self.stats.get(market)
    }

    /// Update mid price for a market.
    pub fn update_price(&mut self, market: &str, best_bid: f64, best_ask: f64) {
        self.get_or_create(market)
            .update_mid_price(best_bid, best_ask);
    }

    /// Record a fill and return a clone of the snapshot for scoring.
    pub fn record_fill(&mut self, market: &str, size: f64) {
        self.get_or_create(market).record_fill_size(size);
    }
}

/// Shared handle to MarketStatsStore, safe for concurrent access.
pub type SharedMarketStats = Arc<RwLock<MarketStatsStore>>;

pub fn new_shared_stats(window_size: usize) -> SharedMarketStats {
    Arc::new(RwLock::new(MarketStatsStore::new(window_size)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mid_price() {
        let mut snap = MarketSnapshot::new(10);
        assert_eq!(snap.mid_price, None);
        snap.update_mid_price(0.48, 0.52);
        assert!((snap.mid_price.unwrap() - 0.50).abs() < f64::EPSILON);
    }

    #[test]
    fn test_rolling_window() {
        let mut snap = MarketSnapshot::new(3);
        snap.record_fill_size(10.0);
        snap.record_fill_size(20.0);
        snap.record_fill_size(30.0);
        assert_eq!(snap.window_len(), 3);
        assert!((snap.mean_fill_size().unwrap() - 20.0).abs() < f64::EPSILON);

        // Adding 4th should evict the 10.0
        snap.record_fill_size(40.0);
        assert_eq!(snap.window_len(), 3);
        assert!((snap.mean_fill_size().unwrap() - 30.0).abs() < f64::EPSILON);
    }

    #[test]
    fn test_stddev() {
        let mut snap = MarketSnapshot::new(100);
        for v in &[10.0, 20.0, 30.0, 40.0, 50.0] {
            snap.record_fill_size(*v);
        }
        let stddev = snap.stddev_fill_size().unwrap();
        // stddev of [10,20,30,40,50] with sample stddev = sqrt(250/4) = ~15.81
        assert!((stddev - 15.811).abs() < 0.01);
    }

    #[test]
    fn test_zscore() {
        let mut snap = MarketSnapshot::new(100);
        for v in &[10.0, 20.0, 30.0, 40.0, 50.0] {
            snap.record_fill_size(*v);
        }
        let z = snap.size_zscore(100.0).unwrap();
        // mean=30, stddev≈15.81, z=(100-30)/15.81≈4.43
        assert!((z - 4.43).abs() < 0.1);
    }

    #[test]
    fn test_zscore_insufficient_data() {
        let mut snap = MarketSnapshot::new(100);
        snap.record_fill_size(10.0);
        snap.record_fill_size(20.0);
        assert!(snap.size_zscore(100.0).is_none());
    }

    #[test]
    fn test_price_deviation() {
        let mut snap = MarketSnapshot::new(10);
        snap.update_mid_price(0.48, 0.52);
        // mid = 0.50, fill at 0.95 → deviation = 0.45/0.50 = 0.90
        let dev = snap.price_deviation_fraction(0.95).unwrap();
        assert!((dev - 0.90).abs() < 0.01);
    }

    #[test]
    fn test_price_deviation_no_mid() {
        let snap = MarketSnapshot::new(10);
        assert!(snap.price_deviation_fraction(0.95).is_none());
    }
}
