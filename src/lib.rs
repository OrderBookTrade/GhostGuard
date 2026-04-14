pub mod api;
pub mod callback;
pub mod config;
pub mod defense;
pub mod detection;
pub mod logging;
pub mod predictive;
pub mod types;
pub mod ws;

pub use detection::verify_fill;
pub use predictive::Predictor;
pub use types::{Config, FillVerdict, GhostFillEvent, PredictiveWarning};

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::detection::{DetectionContext, GhostCallback, VerdictCallback};
use crate::logging::JsonlWriter;
use crate::ws::ClobEvent;

type WarningCallback = Arc<dyn Fn(PredictiveWarning) + Send + Sync>;

/// Main GhostGuard SDK handle.
///
/// # Example
///
/// ```no_run
/// use ghostguard::{Config, GhostGuard};
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
/// guard.on_predictive_warning(|w| {
///     eprintln!("[WARN {:.2}] {:?}", w.score, w.tx_hash);
/// });
///
/// guard.start().await?;
/// # Ok(())
/// # }
/// ```
pub struct GhostGuard {
    config: Config,
    on_real: Vec<VerdictCallback>,
    on_ghost: Vec<GhostCallback>,
    on_warning: Vec<WarningCallback>,
}

impl GhostGuard {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            on_real: Vec::new(),
            on_ghost: Vec::new(),
            on_warning: Vec::new(),
        }
    }

    /// Register a callback for confirmed real fills.
    pub fn on_real_fill<F>(&mut self, f: F)
    where
        F: Fn(FillVerdict) + Send + Sync + 'static,
    {
        self.on_real.push(Arc::new(f));
    }

    /// Register a callback for detected ghost fills.
    pub fn on_ghost_fill<F>(&mut self, f: F)
    where
        F: Fn(GhostFillEvent) + Send + Sync + 'static,
    {
        self.on_ghost.push(Arc::new(f));
    }

    /// Register a callback for predictive warnings (fired BEFORE chain confirmation).
    pub fn on_predictive_warning<F>(&mut self, f: F)
    where
        F: Fn(PredictiveWarning) + Send + Sync + 'static,
    {
        self.on_warning.push(Arc::new(f));
    }

    /// Start listening, verifying fills on-chain, and dispatching callbacks.
    /// Returns cleanly on SIGINT (Ctrl-C).
    pub async fn start(self) -> Result<()> {
        let (event_tx, event_rx) = mpsc::channel::<ClobEvent>(256);

        let ws_url = self.config.clob_ws_url.clone();
        let markets = self.config.markets.clone();
        let ws_handle = tokio::spawn(async move {
            if let Err(e) = ws::listen_clob_events(&ws_url, &markets, event_tx).await {
                error!(error = %e, "CLOB websocket listener failed");
            }
        });

        let config = Arc::new(self.config);

        // Open JSONL loggers (skipped if the path is empty).
        let verdict_log = JsonlWriter::maybe_open(&config.verdict_log).await?;
        let predictive_log = JsonlWriter::maybe_open(&config.predictive_log).await?;

        if let Some(ref log) = verdict_log {
            info!(path = ?log.path(), "verdict log opened");
        }
        if let Some(ref log) = predictive_log {
            info!(path = ?log.path(), "predictive log opened");
        }

        // Detection dispatch context (shared by all verification tasks).
        let det_ctx = DetectionContext {
            config: Arc::clone(&config),
            verdict_log,
            on_real: Arc::new(self.on_real),
            on_ghost: Arc::new(self.on_ghost),
        };

        // Predictive scorer (only used if enabled).
        let predictor = if config.predictive_enabled {
            Some(Arc::new(Predictor::new(
                config.predictive_threshold,
                config.avg_window,
                predictive_log,
            )))
        } else {
            None
        };

        let on_warning = Arc::new(self.on_warning);

        if config.predictive_enabled {
            info!(
                threshold = config.predictive_threshold,
                window = config.avg_window,
                "GhostGuard started — predictive detection ENABLED"
            );
        } else {
            info!("GhostGuard started — listening for fills");
        }

        // Main dispatch loop, racing Ctrl-C.
        tokio::select! {
            _ = run_event_loop(event_rx, det_ctx, predictor, on_warning) => {
                info!("event loop exited (channel closed)");
            }
            _ = tokio::signal::ctrl_c() => {
                info!("SIGINT received, shutting down...");
            }
        }

        ws_handle.abort();
        Ok(())
    }
}

async fn run_event_loop(
    mut event_rx: mpsc::Receiver<ClobEvent>,
    det_ctx: DetectionContext,
    predictor: Option<Arc<Predictor>>,
    on_warning: Arc<Vec<WarningCallback>>,
) {
    while let Some(event) = event_rx.recv().await {
        match event {
            ClobEvent::PriceUpdate(p) => {
                if let Some(ref predictor) = predictor {
                    predictor
                        .ingest_price(&p.market, p.best_bid, p.best_ask)
                        .await;
                }
            }
            ClobEvent::Fill(fill) => {
                // Phase 2: predictive scoring (fast, non-blocking)
                if let Some(ref predictor) = predictor {
                    let predictor = Arc::clone(predictor);
                    let on_warning = Arc::clone(&on_warning);
                    let fill_p = fill.clone();
                    tokio::spawn(async move {
                        if let Some(warning) = predictor.score_fill(&fill_p).await {
                            for cb in on_warning.iter() {
                                cb(warning.clone());
                            }
                        }
                    });
                }

                // Phase 1: on-chain verification (slow — 500ms to 10s)
                let det_ctx = det_ctx.clone();
                tokio::spawn(async move {
                    detection::handle_fill(det_ctx, fill).await;
                });
            }
        }
    }

    warn!("event channel closed");
}
