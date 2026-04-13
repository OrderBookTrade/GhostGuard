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

    // --- Predictive detection ---
    /// Enable predictive ghost fill scoring.
    pub predictive_enabled: bool,
    /// Combined GhostRisk threshold for triggering on_high_risk callback.
    pub risk_threshold: f64,
    /// Weight for price deviation factor.
    pub weight_price: f64,
    /// Weight for size anomaly factor.
    pub weight_size: f64,
    /// Weight for wallet history factor.
    pub weight_wallet: f64,
    /// Enable async wallet history lookups (adds latency).
    pub wallet_lookup_enabled: bool,
    /// Polymarket API base URL for wallet lookups.
    pub polymarket_api_url: String,
    /// Timeout for wallet history API call.
    pub wallet_lookup_timeout: Duration,
    /// Rolling window size for market stats (number of fills).
    pub stats_window_size: usize,
    /// Price deviation fraction considered extreme (e.g. 0.15 = 15%).
    pub price_deviation_extreme: f64,
    /// Size z-score considered extreme.
    pub size_zscore_extreme: f64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            rpc_url: "https://polygon-rpc.com".into(),
            clob_ws_url: "wss://ws-subscriptions-clob.polymarket.com/ws/market".into(),
            verify_timeout: Duration::from_secs(10),
            poll_interval: Duration::from_millis(500),
            webhook_url: None,
            // predictive defaults
            predictive_enabled: false,
            risk_threshold: 0.7,
            weight_price: 0.4,
            weight_size: 0.4,
            weight_wallet: 0.2,
            wallet_lookup_enabled: false,
            polymarket_api_url: "https://clob.polymarket.com".into(),
            wallet_lookup_timeout: Duration::from_secs(2),
            stats_window_size: 100,
            price_deviation_extreme: 0.15,
            size_zscore_extreme: 3.0,
        }
    }
}

/// Result of verifying a fill transaction on-chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "verdict")]
pub enum FillVerdict {
    /// Transaction confirmed on-chain with status=1.
    Real {
        tx_hash: H256,
        block: u64,
    },
    /// Transaction reverted on-chain (status=0). This is a ghost fill.
    Ghost {
        tx_hash: H256,
        reason: String,
        counterparty: Option<Address>,
    },
    /// No receipt received within verify_timeout. Treat as ghost.
    Timeout {
        tx_hash: H256,
    },
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
        matches!(self, FillVerdict::Ghost { .. } | FillVerdict::Timeout { .. })
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

// ---------------------------------------------------------------------------
// Predictive detection types
// ---------------------------------------------------------------------------

/// Predictive risk score produced BEFORE chain confirmation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GhostRisk {
    /// Combined risk score: 0.0 (safe) to 1.0 (almost certainly ghost).
    pub score: f64,
    /// Per-factor breakdown.
    pub factors: RiskFactors,
    /// Market this fill belongs to.
    pub market: String,
    /// Transaction hash of the scored fill.
    pub tx_hash: H256,
    /// Unix timestamp when scoring completed.
    pub scored_at: u64,
}

impl GhostRisk {
    pub fn is_high_risk(&self, threshold: f64) -> bool {
        self.score >= threshold
    }
}

/// Individual factor scores that compose a GhostRisk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskFactors {
    /// Price deviation from rolling mid price. 0.0 = at mid, 1.0 = extreme.
    pub price_deviation: f64,
    /// Size anomaly vs recent fill sizes. 0.0 = normal, 1.0 = extreme outlier.
    pub size_anomaly: f64,
    /// Wallet history suspicion. 0.0 = established, 1.0 = new/no history.
    /// None if taker address unavailable or wallet lookup disabled/timed out.
    pub wallet_risk: Option<f64>,
}
