pub mod callback;
pub mod market_stats;
pub mod predictor;
pub mod types;
pub mod verifier;
pub mod wallet_cache;
pub mod ws;

pub use types::{Config, FillVerdict, GhostFillEvent, GhostRisk, RiskFactors};
pub use verifier::verify_fill;
pub use predictor::PredictiveDetector;
pub use market_stats::{new_shared_stats, SharedMarketStats};

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::wallet_cache::new_shared_wallet_cache;
use crate::ws::ClobEvent;

type VerdictCallback = Box<dyn Fn(FillVerdict) + Send + Sync>;
type GhostCallback = Box<dyn Fn(GhostFillEvent) + Send + Sync>;
type HighRiskCallback = Box<dyn Fn(GhostRisk) + Send + Sync>;

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
    on_high_risk: Vec<Arc<HighRiskCallback>>,
}

impl GhostGuard {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            on_real: Vec::new(),
            on_ghost: Vec::new(),
            on_high_risk: Vec::new(),
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

    /// Register a callback for predictive high-risk early warnings.
    /// Called BEFORE chain verification completes.
    pub fn on_high_risk<F>(&mut self, f: F)
    where
        F: Fn(GhostRisk) + Send + Sync + 'static,
    {
        self.on_high_risk.push(Arc::new(Box::new(f)));
    }

    /// Start listening to the CLOB websocket, verifying each fill on-chain,
    /// and dispatching to registered callbacks.
    pub async fn start(self) -> Result<()> {
        let (event_tx, mut event_rx) = mpsc::channel::<ClobEvent>(256);

        let ws_url = self.config.clob_ws_url.clone();
        let ws_handle = tokio::spawn(async move {
            if let Err(e) = ws::listen_clob_events(&ws_url, event_tx).await {
                error!(error = %e, "CLOB websocket listener failed");
            }
        });

        let config = Arc::new(self.config);
        let on_real = Arc::new(self.on_real);
        let on_ghost = Arc::new(self.on_ghost);
        let on_high_risk = Arc::new(self.on_high_risk);

        // Predictive detection setup
        let stats = new_shared_stats(config.stats_window_size);
        let wallet_cache = if config.wallet_lookup_enabled {
            Some(new_shared_wallet_cache(
                config.polymarket_api_url.clone(),
                config.wallet_lookup_timeout,
            ))
        } else {
            None
        };
        let detector = Arc::new(PredictiveDetector::new(
            Arc::clone(&config),
            stats.clone(),
            wallet_cache,
        ));

        if config.predictive_enabled {
            info!("GhostGuard started — predictive detection ENABLED (threshold={:.2})", config.risk_threshold);
        } else {
            info!("GhostGuard started — listening for fills");
        }

        while let Some(event) = event_rx.recv().await {
            match event {
                ClobEvent::PriceUpdate(update) => {
                    // Update market stats inline (fast, sub-microsecond write lock)
                    let mut store = stats.write().await;
                    store.update_price(&update.market, update.best_bid, update.best_ask);
                }
                ClobEvent::Fill(fill) => {
                    // Task 1: Predictive scoring (if enabled)
                    if config.predictive_enabled {
                        let detector = Arc::clone(&detector);
                        let stats = stats.clone();
                        let config_p = Arc::clone(&config);
                        let on_high_risk = Arc::clone(&on_high_risk);
                        let fill_clone = fill.clone();

                        tokio::spawn(async move {
                            let risk = detector.score(&fill_clone).await;

                            if risk.is_high_risk(config_p.risk_threshold) {
                                for cb in on_high_risk.iter() {
                                    cb(risk.clone());
                                }
                            }

                            // Record fill size AFTER scoring (so it doesn't influence its own score)
                            let mut store = stats.write().await;
                            store.record_fill(&fill_clone.market, fill_clone.size);
                        });
                    } else {
                        // Still record fills for stats even if predictive is off
                        let mut store = stats.write().await;
                        store.record_fill(&fill.market, fill.size);
                    }

                    // Task 2: On-chain verification (always runs)
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
                                                callback::send_ghost_event_webhook(url, &event)
                                                    .await
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
            }
        }

        ws_handle.abort();
        Ok(())
    }
}
