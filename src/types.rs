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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            rpc_url: "https://polygon-rpc.com".into(),
            clob_ws_url: "wss://ws-subscriptions-clob.polymarket.com/ws/market".into(),
            verify_timeout: Duration::from_secs(10),
            poll_interval: Duration::from_millis(500),
            webhook_url: None,
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
