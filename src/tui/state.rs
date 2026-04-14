//! In-memory dashboard state.
//!
//! Pure data — no rendering, no I/O. Owns the event-handling logic
//! (`TuiState::ingest`) and the `prune_resolved` housekeeping that's called
//! every tick by the render loop.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::types::{FillVerdict, PredictiveWarning};
use crate::ws::ClobFill;

use super::events::{ConnectionStatus, FeedKind, TuiEvent};

/// One line in the scrolling Live feed panel.
#[derive(Debug, Clone)]
pub struct FeedEntry {
    pub time: DateTime<Utc>,
    pub kind: FeedKind,
    pub tx_short: String,
    pub market: String,
    pub detail: String,
}

/// Aggregate counters shown in the Stats panel.
#[derive(Debug, Default, Clone)]
pub struct DashboardStats {
    pub total_verified: u64,
    pub total_ghost: u64,
    pub total_warnings: u64,
    /// Warnings that turned out to be ghosts (of warnings we could correlate).
    pub warning_correct: u64,
    pub warning_accuracy: f64,
    /// Cumulative latency for rolling average.
    pub latency_sum_ms: u128,
    pub latency_samples: u64,
    pub avg_latency_ms: f64,
    pub blacklisted_count: u64,
}

impl DashboardStats {
    pub fn ghost_pct(&self) -> f64 {
        if self.total_verified == 0 {
            0.0
        } else {
            (self.total_ghost as f64 / self.total_verified as f64) * 100.0
        }
    }
}

/// One row in the Markets panel — per-token rolling stats + cycle label.
#[derive(Debug, Clone, Default)]
pub struct MarketRow {
    pub slug: String,
    /// Human-readable cycle label, e.g. `"UP 7:05AM-7:10AM ET"`.
    /// Populated from NewMarket events; None until we know the cycle.
    pub label: Option<String>,
    pub mid_price: Option<f64>,
    pub total_size: f64,
    pub size_samples: u64,
    /// Sliding-window timestamps of fills for rate calculation.
    pub fill_times: VecDeque<Instant>,
    pub ghost_count: u64,
}

impl MarketRow {
    pub fn new(slug: String) -> Self {
        Self {
            slug,
            ..Default::default()
        }
    }

    pub fn avg_size(&self) -> f64 {
        if self.size_samples == 0 {
            0.0
        } else {
            self.total_size / self.size_samples as f64
        }
    }

    /// Fills in the last minute.
    pub fn fills_per_min(&self) -> u64 {
        self.fill_times.len() as u64
    }

    pub fn ghost_rate(&self) -> f64 {
        let n = self.size_samples;
        if n == 0 {
            0.0
        } else {
            (self.ghost_count as f64 / n as f64) * 100.0
        }
    }

    pub fn record_fill(&mut self, size: f64, now: Instant) {
        self.total_size += size;
        self.size_samples += 1;
        self.fill_times.push_back(now);
        // Evict anything older than 60s
        while let Some(&front) = self.fill_times.front() {
            if now.duration_since(front) > Duration::from_secs(60) {
                self.fill_times.pop_front();
            } else {
                break;
            }
        }
    }
}

/// Currently-active rotating market cycle.
#[derive(Debug, Clone)]
pub struct CycleInfo {
    pub slug: String,
    pub question: String,
    pub assets_ids: Vec<String>,
    pub started_at: Instant,
}

pub struct TuiState {
    pub feed: VecDeque<FeedEntry>,
    pub feed_cap: usize,
    pub stats: DashboardStats,
    pub markets: HashMap<String, MarketRow>,
    pub status: ConnectionStatus,
    pub paused: bool,
    pub start_time: Instant,
    pub markets_list: Vec<String>,
    pub rpc_url: String,
    /// Currently-active rotating market cycle (if rotation is enabled).
    pub current_cycle: Option<CycleInfo>,
    /// Markets that have resolved, with the time they resolved. Kept in the
    /// Markets panel for a grace period so the user can see the final state.
    pub resolved_at: HashMap<String, Instant>,
    /// How long to keep resolved markets visible.
    pub keep_resolved: Duration,
    /// Timestamp of the last NewMarket event — used to flash the header.
    pub last_new_market_at: Option<Instant>,
}

impl TuiState {
    pub fn new() -> Self {
        Self {
            feed: VecDeque::with_capacity(200),
            feed_cap: 200,
            stats: DashboardStats::default(),
            markets: HashMap::new(),
            status: ConnectionStatus::Connecting,
            paused: false,
            start_time: Instant::now(),
            markets_list: Vec::new(),
            rpc_url: String::new(),
            current_cycle: None,
            resolved_at: HashMap::new(),
            keep_resolved: Duration::from_secs(30),
            last_new_market_at: None,
        }
    }

    fn push_feed(&mut self, entry: FeedEntry) {
        if self.feed.len() >= self.feed_cap {
            self.feed.pop_front();
        }
        self.feed.push_back(entry);
    }

    /// Explicitly register a market row — only callable from Config /
    /// NewMarket paths. Inserts if missing, returns a mutable handle.
    fn track_market(&mut self, slug: &str) -> &mut MarketRow {
        self.markets
            .entry(slug.to_string())
            .or_insert_with(|| MarketRow::new(slug.to_string()))
    }

    /// Return a mutable handle ONLY if the market is already tracked.
    /// Used by PriceUpdate / Trade / Verdict handlers so late ws events
    /// for tokens we've unsubscribed from (or never subscribed to)
    /// don't resurrect phantom rows in the Markets panel.
    fn market_if_tracked(&mut self, slug: &str) -> Option<&mut MarketRow> {
        self.markets.get_mut(slug)
    }

    /// Apply an incoming event to state.
    pub fn ingest(&mut self, ev: TuiEvent) {
        if self.paused {
            return;
        }
        match ev {
            TuiEvent::Trade(fill) => self.on_trade(fill),
            TuiEvent::Verdict {
                verdict,
                fill,
                latency_ms,
            } => self.on_verdict(verdict, fill, latency_ms),
            TuiEvent::Warning(w) => self.on_warning(w),
            TuiEvent::PriceUpdate { market, mid } => {
                // Only update rows we're actively tracking. Ignore late ws
                // ticks for already-retired cycles.
                if let Some(row) = self.market_if_tracked(&market) {
                    row.mid_price = Some(mid);
                }
            }
            TuiEvent::WsConnected => self.status = ConnectionStatus::Connected,
            TuiEvent::WsDisconnected => self.status = ConnectionStatus::Disconnected,
            TuiEvent::Status(msg) => {
                self.push_feed(FeedEntry {
                    time: Utc::now(),
                    kind: FeedKind::System,
                    tx_short: String::new(),
                    market: String::new(),
                    detail: msg,
                });
            }
            TuiEvent::Config {
                markets,
                rpc_url,
                keep_resolved_secs,
            } => {
                self.markets_list = markets.clone();
                self.rpc_url = rpc_url;
                self.keep_resolved = Duration::from_secs(keep_resolved_secs);
                for m in &markets {
                    self.track_market(m);
                }
            }
            TuiEvent::NewMarket {
                market,
                slug,
                assets_ids,
                question,
                outcomes,
                time_window,
            } => {
                self.last_new_market_at = Some(Instant::now());
                self.current_cycle = Some(CycleInfo {
                    slug: slug.clone(),
                    question: question.clone(),
                    assets_ids: assets_ids.clone(),
                    started_at: Instant::now(),
                });
                // Add new markets to the panel, attaching the per-token label
                // (outcome + time window) so the user can tell UP from DOWN.
                for (i, aid) in assets_ids.iter().enumerate() {
                    let label = outcomes.get(i).map(|outcome| {
                        if time_window.is_empty() {
                            outcome.to_uppercase()
                        } else {
                            format!("{} {}", outcome.to_uppercase(), time_window)
                        }
                    });
                    let row = self.track_market(aid);
                    if label.is_some() {
                        row.label = label;
                    }
                }
                // Make sure the markets list reflects the new subscription
                self.markets_list.retain(|m| !m.is_empty());
                for aid in &assets_ids {
                    if !self.markets_list.contains(aid) {
                        self.markets_list.push(aid.clone());
                    }
                }
                // Clear the resolved flag if this market is coming back
                self.resolved_at.remove(&market);
                for aid in &assets_ids {
                    self.resolved_at.remove(aid);
                }
                self.push_feed(FeedEntry {
                    time: Utc::now(),
                    kind: FeedKind::System,
                    tx_short: String::new(),
                    market: slug.clone(),
                    detail: format!("[NEW] {question} (tokens={})", assets_ids.len()),
                });
            }
            TuiEvent::MarketResolved { market, assets_ids } => {
                let now = Instant::now();
                self.resolved_at.insert(market.clone(), now);
                for aid in &assets_ids {
                    self.resolved_at.insert(aid.clone(), now);
                }
                self.push_feed(FeedEntry {
                    time: Utc::now(),
                    kind: FeedKind::System,
                    tx_short: String::new(),
                    market: market.clone(),
                    detail: format!("[RESOLVED] tokens={}", assets_ids.len()),
                });
            }
        }
    }

    /// Prune the Markets panel down to a hard invariant:
    ///
    ///   panel = current_cycle.assets_ids ∪ resolved_at(within grace window)
    ///
    /// Two passes:
    ///
    /// 1. Time-based: drop `resolved_at` entries past the grace window AND
    ///    their corresponding rows.
    /// 2. Phantom sweep (only when a current_cycle is known): drop any row
    ///    that is neither current nor resolving. This catches stragglers
    ///    that slipped in via stale ws events, cross-rotation races, or
    ///    initial bootstrap markets that were superseded.
    ///
    /// Called from the render loop on every tick (~250 ms).
    pub fn prune_resolved(&mut self) {
        let now = Instant::now();
        let keep = self.keep_resolved;

        // Pass 1: expire grace-window resolutions.
        let expired: Vec<String> = self
            .resolved_at
            .iter()
            .filter(|(_, at)| now.duration_since(**at) > keep)
            .map(|(k, _)| k.clone())
            .collect();
        for key in expired {
            self.resolved_at.remove(&key);
            self.markets.remove(&key);
            self.markets_list.retain(|m| m != &key);
        }

        // Pass 2: phantom sweep (only safe once we know the current cycle —
        // otherwise we'd nuke bootstrap-only markets in non-rotation mode).
        if let Some(cycle) = self.current_cycle.clone() {
            let alive: HashSet<&String> = cycle.assets_ids.iter().collect();
            let phantoms: Vec<String> = self
                .markets
                .keys()
                .filter(|k| !alive.contains(*k) && !self.resolved_at.contains_key(*k))
                .cloned()
                .collect();
            for key in phantoms {
                self.markets.remove(&key);
                self.markets_list.retain(|m| m != &key);
            }
        }
    }

    /// Called for every incoming trade (before any verification).
    /// Updates the market row's rolling stats (only if tracked) and emits a
    /// [TRADE] feed entry. Late trades for retired markets still show in the
    /// feed but don't resurrect a panel row.
    fn on_trade(&mut self, fill: ClobFill) {
        let now = Instant::now();
        if let Some(row) = self.market_if_tracked(&fill.market) {
            row.record_fill(fill.size, now);
        }

        let detail = format!(
            "side={} size={:.2} price={:.4}",
            fill.side, fill.size, fill.price
        );
        self.push_feed(FeedEntry {
            time: Utc::now(),
            kind: FeedKind::Trade,
            tx_short: if fill.tx_hash.is_zero() {
                String::new()
            } else {
                short_hex(&format!("{:?}", fill.tx_hash))
            },
            market: fill.market,
            detail,
        });
    }

    fn on_verdict(&mut self, verdict: FillVerdict, fill: ClobFill, latency_ms: u64) {
        self.stats.total_verified += 1;
        self.stats.latency_sum_ms += latency_ms as u128;
        self.stats.latency_samples += 1;
        if self.stats.latency_samples > 0 {
            self.stats.avg_latency_ms =
                self.stats.latency_sum_ms as f64 / self.stats.latency_samples as f64;
        }

        // Note: fill size / rolling window is already tracked by `on_trade`;
        // don't double-count here.

        // For ghosts, render the FULL tx hash into the detail string so the
        // user can copy it straight to Polygonscan without digging into the
        // JSONL log.
        let tx_full = format!("{:?}", verdict.tx_hash());
        let (kind, detail) = match &verdict {
            FillVerdict::Real { block, .. } => (FeedKind::Real, format!("block={block}")),
            FillVerdict::Ghost { reason, .. } => {
                self.stats.total_ghost += 1;
                if let Some(row) = self.market_if_tracked(&fill.market) {
                    row.ghost_count += 1;
                }
                (FeedKind::Ghost, format!("tx={tx_full} reason={reason}"))
            }
            FillVerdict::Timeout { .. } => {
                self.stats.total_ghost += 1;
                if let Some(row) = self.market_if_tracked(&fill.market) {
                    row.ghost_count += 1;
                }
                (FeedKind::Ghost, format!("tx={tx_full} reason=timeout"))
            }
        };

        // Refresh accuracy metric based on correlation.
        if self.stats.total_warnings > 0 {
            self.stats.warning_accuracy =
                (self.stats.warning_correct as f64 / self.stats.total_warnings as f64) * 100.0;
        }

        self.push_feed(FeedEntry {
            time: Utc::now(),
            kind,
            tx_short: short_hex(&format!("{:?}", verdict.tx_hash())),
            market: fill.market,
            detail,
        });
    }

    fn on_warning(&mut self, w: PredictiveWarning) {
        self.stats.total_warnings += 1;
        self.push_feed(FeedEntry {
            time: Utc::now(),
            kind: FeedKind::Warn,
            tx_short: short_hex(&format!("{:?}", w.tx_hash)),
            market: w.market,
            detail: format!(
                "score={:.2} price_dev={:.3} size_anom={:.2}",
                w.score, w.price_deviation, w.size_anomaly
            ),
        });
    }

    pub fn uptime_hhmmss(&self) -> String {
        let secs = self.start_time.elapsed().as_secs();
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        let s = secs % 60;
        format!("{h:02}:{m:02}:{s:02}")
    }
}

impl Default for TuiState {
    fn default() -> Self {
        Self::new()
    }
}

/// Truncate a `0x`-prefixed hex string to its first 8 hex chars (without prefix).
pub(super) fn short_hex(h: &str) -> String {
    let stripped = h.strip_prefix("0x").unwrap_or(h);
    stripped.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ethers::types::H256;

    fn fill(market: &str, size: f64) -> ClobFill {
        ClobFill {
            tx_hash: H256::zero(),
            market: market.into(),
            side: "BUY".into(),
            size,
            price: 0.5,
        }
    }

    #[test]
    fn test_feed_cap_bounded() {
        let mut s = TuiState::new();
        s.feed_cap = 5;
        for i in 0..20 {
            s.ingest(TuiEvent::Verdict {
                verdict: FillVerdict::Real {
                    tx_hash: H256::zero(),
                    block: i,
                },
                fill: fill("m", 10.0),
                latency_ms: 100,
            });
        }
        assert_eq!(s.feed.len(), 5);
    }

    #[test]
    fn test_stats_counters() {
        let mut s = TuiState::new();
        // One real, one ghost
        s.ingest(TuiEvent::Verdict {
            verdict: FillVerdict::Real {
                tx_hash: H256::zero(),
                block: 1,
            },
            fill: fill("m1", 10.0),
            latency_ms: 200,
        });
        s.ingest(TuiEvent::Verdict {
            verdict: FillVerdict::Ghost {
                tx_hash: H256::zero(),
                reason: "TRANSFER_FROM_FAILED".into(),
                counterparty: None,
            },
            fill: fill("m1", 20.0),
            latency_ms: 600,
        });
        assert_eq!(s.stats.total_verified, 2);
        assert_eq!(s.stats.total_ghost, 1);
        assert!((s.stats.ghost_pct() - 50.0).abs() < 1e-9);
        assert!((s.stats.avg_latency_ms - 400.0).abs() < 1e-9);
    }

    #[test]
    fn test_market_row_fills_per_min_window() {
        let mut m = MarketRow::new("x".into());
        let t0 = Instant::now();
        m.record_fill(10.0, t0);
        m.record_fill(20.0, t0);
        assert_eq!(m.fills_per_min(), 2);
        assert_eq!(m.size_samples, 2);
        assert!((m.avg_size() - 15.0).abs() < 1e-9);
    }

    #[test]
    fn test_pause_blocks_ingest() {
        let mut s = TuiState::new();
        s.paused = true;
        s.ingest(TuiEvent::Verdict {
            verdict: FillVerdict::Real {
                tx_hash: H256::zero(),
                block: 1,
            },
            fill: fill("m", 10.0),
            latency_ms: 100,
        });
        assert_eq!(s.stats.total_verified, 0);
        assert!(s.feed.is_empty());
    }

    #[test]
    fn test_ws_status_updates() {
        let mut s = TuiState::new();
        assert_eq!(s.status, ConnectionStatus::Connecting);
        s.ingest(TuiEvent::WsConnected);
        assert_eq!(s.status, ConnectionStatus::Connected);
        s.ingest(TuiEvent::WsDisconnected);
        assert_eq!(s.status, ConnectionStatus::Disconnected);
    }

    #[test]
    fn test_price_update_for_untracked_market_is_dropped() {
        let mut s = TuiState::new();
        s.ingest(TuiEvent::PriceUpdate {
            market: "ghost-market".into(),
            mid: 0.75,
        });
        assert!(!s.markets.contains_key("ghost-market"));
    }

    #[test]
    fn test_price_update_for_tracked_market_updates_mid() {
        let mut s = TuiState::new();
        s.ingest(TuiEvent::NewMarket {
            market: "0xevent".into(),
            slug: "test-cycle".into(),
            assets_ids: vec!["live-token".into()],
            question: "?".into(),
            outcomes: vec!["Up".into()],
            time_window: "now".into(),
        });
        s.ingest(TuiEvent::PriceUpdate {
            market: "live-token".into(),
            mid: 0.75,
        });
        let m = s.markets.get("live-token").expect("tracked market exists");
        assert_eq!(m.mid_price, Some(0.75));
    }

    #[test]
    fn test_trade_for_untracked_market_does_not_create_row() {
        let mut s = TuiState::new();
        s.ingest(TuiEvent::Trade(fill("ghost-token", 100.0)));
        assert!(!s.markets.contains_key("ghost-token"));
        assert_eq!(s.feed.len(), 1);
    }

    #[test]
    fn test_prune_sweeps_phantoms_outside_current_cycle() {
        let mut s = TuiState::new();
        s.keep_resolved = Duration::from_secs(10);

        s.ingest(TuiEvent::NewMarket {
            market: "0xcurrent".into(),
            slug: "btc-updown-5m-9999".into(),
            assets_ids: vec!["live-1".into(), "live-2".into()],
            question: "current".into(),
            outcomes: vec!["Up".into(), "Down".into()],
            time_window: "now".into(),
        });

        s.markets
            .insert("phantom".into(), MarketRow::new("phantom".into()));
        assert!(s.markets.contains_key("phantom"));

        s.prune_resolved();

        assert!(s.markets.contains_key("live-1"));
        assert!(s.markets.contains_key("live-2"));
        assert!(!s.markets.contains_key("phantom"));
    }

    #[test]
    fn test_prune_keeps_resolved_within_grace_window() {
        let mut s = TuiState::new();
        s.keep_resolved = Duration::from_secs(60);

        s.ingest(TuiEvent::NewMarket {
            market: "0xcurrent".into(),
            slug: "cycle-2".into(),
            assets_ids: vec!["live-1".into()],
            question: "now".into(),
            outcomes: vec!["Up".into()],
            time_window: "now".into(),
        });
        s.ingest(TuiEvent::MarketResolved {
            market: "0xprev".into(),
            assets_ids: vec!["resolving-1".into()],
        });
        s.markets
            .insert("resolving-1".into(), MarketRow::new("resolving-1".into()));

        s.prune_resolved();

        assert!(s.markets.contains_key("live-1"));
        assert!(s.markets.contains_key("resolving-1"));
    }

    #[test]
    fn test_short_hex() {
        assert_eq!(short_hex("0x9e3230abcdef"), "9e3230ab");
        assert_eq!(short_hex("9e3230abcdef"), "9e3230ab");
        assert_eq!(short_hex("0xabc"), "abc");
    }

    #[test]
    fn test_ghost_rate_flag_threshold() {
        let mut m = MarketRow::new("m".into());
        let now = Instant::now();
        for _ in 0..100 {
            m.record_fill(1.0, now);
        }
        m.ghost_count = 10;
        assert!((m.ghost_rate() - 10.0).abs() < 1e-9);
    }
}
