pub mod callback;
pub mod types;
pub mod verifier;
pub mod ws;

pub use types::{Config, FillVerdict, GhostFillEvent};
pub use verifier::verify_fill;

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::ws::ClobFill;

type VerdictCallback = Box<dyn Fn(FillVerdict) + Send + Sync>;
type GhostCallback = Box<dyn Fn(GhostFillEvent) + Send + Sync>;

/// Main GhostGuard SDK handle.
///
/// # Example
///
/// ```no_run
/// use ghostguard::{GhostGuard, Config};
///
/// # async fn run() -> anyhow::Result<()> {
/// let config = Config {
///     rpc_url: "https://polygon-rpc.com".into(),
///     clob_ws_url: "wss://ws-subscriptions-clob.polymarket.com/ws/market".into(),
///     ..Default::default()
/// };
///
/// let mut guard = GhostGuard::new(config);
///
/// guard.on_real_fill(|verdict| {
///     println!("Real fill: {:?}", verdict.tx_hash());
/// });
///
/// guard.on_ghost_fill(|event| {
///     eprintln!("GHOST: {} — {}", event.tx_hash, event.reason);
/// });
///
/// guard.start().await?;
/// # Ok(())
/// # }
/// ```
pub struct GhostGuard {
    config: Config,
    on_real: Vec<Arc<VerdictCallback>>,
    on_ghost: Vec<Arc<GhostCallback>>,
}

impl GhostGuard {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            on_real: Vec::new(),
            on_ghost: Vec::new(),
        }
    }

    /// Register a callback for confirmed real fills.
    pub fn on_real_fill<F>(&mut self, f: F)
    where
        F: Fn(FillVerdict) + Send + Sync + 'static,
    {
        self.on_real.push(Arc::new(Box::new(f)));
    }

    /// Register a callback for detected ghost fills.
    pub fn on_ghost_fill<F>(&mut self, f: F)
    where
        F: Fn(GhostFillEvent) + Send + Sync + 'static,
    {
        self.on_ghost.push(Arc::new(Box::new(f)));
    }

    /// Start listening to the CLOB websocket, verifying each fill on-chain,
    /// and dispatching to registered callbacks.
    pub async fn start(self) -> Result<()> {
        let (fill_tx, mut fill_rx) = mpsc::channel::<ClobFill>(256);

        let ws_url = self.config.clob_ws_url.clone();
        let ws_handle = tokio::spawn(async move {
            if let Err(e) = ws::listen_clob_fills(&ws_url, fill_tx).await {
                error!(error = %e, "CLOB websocket listener failed");
            }
        });

        let config = Arc::new(self.config);
        let on_real = Arc::new(self.on_real);
        let on_ghost = Arc::new(self.on_ghost);

        info!("GhostGuard started — listening for fills");

        while let Some(fill) = fill_rx.recv().await {
            let config = Arc::clone(&config);
            let on_real = Arc::clone(&on_real);
            let on_ghost = Arc::clone(&on_ghost);

            tokio::spawn(async move {
                match verify_fill(&config.rpc_url, fill.tx_hash, &config).await {
                    Ok(verdict) => {
                        // Dispatch webhook if configured
                        if let Some(ref url) = config.webhook_url {
                            if let Err(e) = callback::send_webhook(url, &verdict).await {
                                warn!(error = %e, "webhook dispatch failed");
                            }
                        }

                        match &verdict {
                            FillVerdict::Real { .. } => {
                                for cb in on_real.iter() {
                                    cb(verdict.clone());
                                }
                            }
                            FillVerdict::Ghost {
                                tx_hash,
                                reason,
                                counterparty,
                            } => {
                                let event = GhostFillEvent {
                                    tx_hash: *tx_hash,
                                    market: fill.market.clone(),
                                    side: fill.side.clone(),
                                    size: fill.size,
                                    price: fill.price,
                                    counterparty: *counterparty,
                                    reason: reason.clone(),
                                    timestamp: std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_secs(),
                                };

                                if let Some(ref url) = config.webhook_url {
                                    if let Err(e) =
                                        callback::send_ghost_event_webhook(url, &event).await
                                    {
                                        warn!(error = %e, "ghost event webhook failed");
                                    }
                                }

                                for cb in on_ghost.iter() {
                                    cb(event.clone());
                                }
                            }
                            FillVerdict::Timeout { tx_hash } => {
                                let event = GhostFillEvent {
                                    tx_hash: *tx_hash,
                                    market: fill.market.clone(),
                                    side: fill.side.clone(),
                                    size: fill.size,
                                    price: fill.price,
                                    counterparty: None,
                                    reason: "timeout — no receipt".into(),
                                    timestamp: std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_secs(),
                                };

                                for cb in on_ghost.iter() {
                                    cb(event.clone());
                                }
                            }
                        }
                    }
                    Err(e) => {
                        error!(tx_hash = ?fill.tx_hash, error = %e, "verification failed");
                    }
                }
            });
        }

        ws_handle.abort();
        Ok(())
    }
}
