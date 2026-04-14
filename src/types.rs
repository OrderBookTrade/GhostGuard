use ethers::types::{Address, H256};
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// Configuration for GhostGuard.
#[derive(Debug, Clone)]
pub struct Config {
    /// Polygon JSON-RPC endpoint (HTTP or WebSocket).
    pub rpc_url: String,
    /// Polymarket CLOB WebSocket URL.
    pub clob_ws_url: String,
    /// How long to wait for a receipt before declaring Timeout.
    pub verify_timeout: Duration,
    /// How often to poll eth_getTransactionReceipt.
    pub poll_interval: Duration,
    /// Optional webhook URL for sidecar mode.
    pub webhook_url: Option<String>,
    /// List of market IDs to monitor. If empty, subscribes to user channel.
    pub markets: Vec<String>,

    // ---- Phase 2: predictive detection ----
    /// Enable predictive ghost fill scoring.
    pub predictive_enabled: bool,
    /// Risk threshold above which a warning is emitted.
    pub predictive_threshold: f64,
    /// Rolling window size for per-market average trade size.
    pub avg_window: usize,

    // ---- Phase 1 refinements: logging ----
    /// Path to JSONL verdict log. Empty = disabled.
    pub verdict_log: String,
    /// Path to JSONL predictive warning log. Empty = disabled.
    pub predictive_log: String,

    // ---- UI ----
    /// Launch the ratatui dashboard instead of plain stdout output.
    pub tui_mode: bool,

    // ---- Auto market rotation ----
    /// Auto-follow rotating short-cycle markets (e.g. `btc-updown-5m-*`).
    pub rotation_enabled: bool,
    /// Market slug prefix to follow (e.g. `btc-updown-5m`).
    pub rotation_pattern: String,
    /// How long to keep a resolved market visible in the TUI before pruning.
    pub keep_resolved_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            rpc_url: "https://polygon-rpc.com".into(),
            clob_ws_url: "wss://ws-subscriptions-clob.polymarket.com/ws/market".into(),
            verify_timeout: Duration::from_secs(10),
            poll_interval: Duration::from_millis(500),
            webhook_url: None,
            markets: vec![],
            predictive_enabled: false,
            predictive_threshold: 0.7,
            avg_window: 50,
            verdict_log: "data/verdicts.jsonl".into(),
            predictive_log: "data/predictive_warnings.jsonl".into(),
            tui_mode: false,
            rotation_enabled: false,
            rotation_pattern: "btc-updown-5m".into(),
            keep_resolved_secs: 10,
        }
    }
}

/// Result of verifying a fill transaction on-chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "verdict")]
pub enum FillVerdict {
    /// Transaction confirmed on-chain with status=1.
    Real { tx_hash: H256, block: u64 },
    /// Transaction reverted on-chain (status=0). This is a ghost fill.
    Ghost {
        tx_hash: H256,
        reason: String,
        counterparty: Option<Address>,
    },
    /// No receipt received within verify_timeout. Treat as ghost.
    Timeout { tx_hash: H256 },
}

impl FillVerdict {
    pub fn tx_hash(&self) -> H256 {
        match self {
            FillVerdict::Real { tx_hash, .. } => *tx_hash,
            FillVerdict::Ghost { tx_hash, .. } => *tx_hash,
            FillVerdict::Timeout { tx_hash } => *tx_hash,
        }
    }

    pub fn is_ghost(&self) -> bool {
        matches!(
            self,
            FillVerdict::Ghost { .. } | FillVerdict::Timeout { .. }
        )
    }
}

/// Structured event emitted when a ghost fill is detected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GhostFillEvent {
    pub tx_hash: H256,
    pub market: String,
    pub side: String,
    pub size: f64,
    pub price: f64,
    pub counterparty: Option<Address>,
    pub reason: String,
    pub timestamp: u64,
}

/// Predictive warning emitted BEFORE chain confirmation.
///
/// Produced by `predictive::Predictor::score_fill()` when the computed
/// risk score exceeds `Config::predictive_threshold`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PredictiveWarning {
    pub tx_hash: H256,
    pub market: String,
    pub score: f64,
    pub price_deviation: f64,
    pub size_anomaly: f64,
    pub trade_price: f64,
    pub mid_price: f64,
    pub trade_size: f64,
    pub avg_size: f64,
    pub ts: u64,
}

/// Known Polymarket contract addresses on Polygon.
pub mod contracts {
    use ethers::types::Address;

    /// CTF Exchange
    pub fn ctf_exchange() -> Address {
        "0x4bfb41d5b3570defd03c39a9a4d8de6bd8b8982e"
            .parse()
            .unwrap()
    }

    /// NegRisk CTF Exchange
    pub fn neg_risk_ctf_exchange() -> Address {
        "0xC5d563A36AE78145C45a50134d48A1215220f80a"
            .parse()
            .unwrap()
    }

    /// USDC.e on Polygon
    pub fn usdc() -> Address {
        "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174"
            .parse()
            .unwrap()
    }
}

/// TRANSFER_FROM_FAILED selector — the 4-byte keccak of the revert reason
/// commonly seen in wallet-drain ghost fills.
pub const TRANSFER_FROM_FAILED_SELECTOR: &str = "TRANSFER_FROM_FAILED";
