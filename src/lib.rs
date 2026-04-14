pub mod api;
pub mod callback;
pub mod config;
pub mod defense;
pub mod detection;
pub mod gamma;
pub mod logging;
pub mod predictive;
pub mod tui;
pub mod types;
pub mod ws;

pub use detection::verify_fill;
pub use predictive::Predictor;
pub use types::{Config, FillVerdict, GhostFillEvent, PredictiveWarning};

use anyhow::Result;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use crate::detection::{DetectionContext, GhostCallback, VerdictCallback};
use crate::logging::JsonlWriter;
use crate::tui::TuiEvent;
use crate::ws::ClobEvent;

type WarningCallback = Arc<dyn Fn(PredictiveWarning) + Send + Sync>;

/// Main GhostGuard SDK handle.
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

    pub fn on_real_fill<F>(&mut self, f: F)
    where
        F: Fn(FillVerdict) + Send + Sync + 'static,
    {
        self.on_real.push(Arc::new(f));
    }

    pub fn on_ghost_fill<F>(&mut self, f: F)
    where
        F: Fn(GhostFillEvent) + Send + Sync + 'static,
    {
        self.on_ghost.push(Arc::new(f));
    }

    pub fn on_predictive_warning<F>(&mut self, f: F)
    where
        F: Fn(PredictiveWarning) + Send + Sync + 'static,
    {
        self.on_warning.push(Arc::new(f));
    }

    /// Start the sidecar. Returns cleanly on SIGINT or (if `tui_mode`) when
    /// the user presses `q` in the dashboard.
    pub async fn start(self) -> Result<()> {
        let (event_tx, event_rx) = mpsc::channel::<ClobEvent>(256);

        let ws_url = self.config.clob_ws_url.clone();
        let markets = self.config.markets.clone();
        // Command channel: main loop → ws task (runtime subscribe/unsubscribe).
        let (cmd_tx, cmd_rx) = mpsc::channel::<ws::WsCommand>(32);
        let rotation_enabled = self.config.rotation_enabled;
        let rotation_pattern = self.config.rotation_pattern.clone();

        let ws_handle = tokio::spawn(async move {
            if let Err(e) =
                ws::listen_clob_events(&ws_url, &markets, event_tx, cmd_rx, rotation_enabled).await
            {
                error!(error = %e, "CLOB websocket listener failed");
            }
        });

        let config = Arc::new(self.config);

        // JSONL loggers (skipped if path is empty).
        let verdict_log = JsonlWriter::maybe_open(&config.verdict_log).await?;
        let predictive_log = JsonlWriter::maybe_open(&config.predictive_log).await?;

        if let Some(ref log) = verdict_log {
            info!(path = ?log.path(), "verdict log opened");
        }
        if let Some(ref log) = predictive_log {
            info!(path = ?log.path(), "predictive log opened");
        }

        // TUI channel (only created when tui_mode is on).
        let (tui_tx, tui_rx) = if config.tui_mode {
            let (tx, rx) = mpsc::unbounded_channel::<TuiEvent>();
            // Seed the TUI with initial config metadata.
            let _ = tx.send(TuiEvent::Config {
                markets: config.markets.clone(),
                rpc_url: config.rpc_url.clone(),
                keep_resolved_secs: config.keep_resolved_secs,
            });
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

        // Detection dispatch context.
        let det_ctx = DetectionContext {
            config: Arc::clone(&config),
            verdict_log,
            on_real: Arc::new(self.on_real),
            on_ghost: Arc::new(self.on_ghost),
            tui_tx: tui_tx.clone(),
            last_rpc_notice: Arc::new(AtomicU64::new(0)),
        };

        // Predictive scorer.
        let predictor = if config.predictive_enabled {
            let mut p = Predictor::new(
                config.predictive_threshold,
                config.avg_window,
                predictive_log,
            );
            if let Some(ref tx) = tui_tx {
                p = p.with_tui(tx.clone());
            }
            Some(Arc::new(p))
        } else {
            None
        };

        let on_warning = Arc::new(self.on_warning);

        // Rotation poller — queries Gamma every 30s as a fallback to WS
        // `new_market` events. On cycle change, pushes Subscribe/Unsubscribe
        // commands to the ws task so the subscription tracks reality even if
        // the server never emits `new_market`. Also pushes a NewMarket event
        // to the TUI so the header shows the current cycle.
        let rotation_handle = if rotation_enabled {
            let cmd_tx_clone = cmd_tx.clone();
            let tui_tx_clone = tui_tx.clone();
            let initial_markets = config.markets.clone();
            let pattern = rotation_pattern.clone();
            Some(tokio::spawn(async move {
                run_rotation_poller(
                    pattern,
                    std::time::Duration::from_secs(30),
                    cmd_tx_clone,
                    tui_tx_clone,
                    initial_markets,
                )
                .await;
            }))
        } else {
            None
        };

        if config.tui_mode {
            info!("GhostGuard started — TUI mode");
        } else if config.predictive_enabled {
            info!(
                threshold = config.predictive_threshold,
                window = config.avg_window,
                "GhostGuard started — predictive detection ENABLED"
            );
        } else {
            info!("GhostGuard started — listening for fills");
        }

        // Spawn TUI render task if in TUI mode.
        let tui_handle = tui_rx.map(|rx| tokio::spawn(async move { tui::run_tui(rx).await }));

        // Main dispatch loop.
        let event_loop_fut =
            run_event_loop(event_rx, det_ctx, predictor, on_warning, tui_tx, cmd_tx);

        // Race event loop against: TUI exit (user pressed q), ctrl-c.
        if let Some(handle) = tui_handle {
            tokio::select! {
                _ = event_loop_fut => {
                    info!("event loop exited (channel closed)");
                }
                res = handle => {
                    match res {
                        Ok(Ok(())) => info!("TUI exited cleanly"),
                        Ok(Err(e)) => error!(error = %e, "TUI exited with error"),
                        Err(e) => error!(error = %e, "TUI task panicked"),
                    }
                }
                _ = tokio::signal::ctrl_c() => {
                    info!("SIGINT received, shutting down...");
                }
            }
        } else {
            tokio::select! {
                _ = event_loop_fut => {
                    info!("event loop exited (channel closed)");
                }
                _ = tokio::signal::ctrl_c() => {
                    info!("SIGINT received, shutting down...");
                }
            }
        }

        ws_handle.abort();
        if let Some(h) = rotation_handle {
            h.abort();
        }
        Ok(())
    }
}

async fn run_event_loop(
    mut event_rx: mpsc::Receiver<ClobEvent>,
    det_ctx: DetectionContext,
    predictor: Option<Arc<Predictor>>,
    on_warning: Arc<Vec<WarningCallback>>,
    tui_tx: Option<mpsc::UnboundedSender<TuiEvent>>,
    cmd_tx: mpsc::Sender<ws::WsCommand>,
) {
    let rotation_pattern = det_ctx.config.rotation_pattern.clone();
    let rotation_enabled = det_ctx.config.rotation_enabled;
    while let Some(event) = event_rx.recv().await {
        match event {
            ClobEvent::Connected => {
                if let Some(ref tx) = tui_tx {
                    let _ = tx.send(TuiEvent::WsConnected);
                }
            }
            ClobEvent::Disconnected => {
                if let Some(ref tx) = tui_tx {
                    let _ = tx.send(TuiEvent::WsDisconnected);
                }
            }
            ClobEvent::Status(msg) => {
                if let Some(ref tx) = tui_tx {
                    let _ = tx.send(TuiEvent::Status(msg));
                }
            }
            ClobEvent::PriceUpdate(p) => {
                if let Some(ref predictor) = predictor {
                    predictor
                        .ingest_price(&p.market, p.best_bid, p.best_ask)
                        .await;
                }
                if let Some(ref tx) = tui_tx {
                    let mid = (p.best_bid + p.best_ask) / 2.0;
                    let _ = tx.send(TuiEvent::PriceUpdate {
                        market: p.market,
                        mid,
                    });
                }
            }
            ClobEvent::Fill(fill) => {
                // Always surface the trade to the TUI immediately.
                if let Some(ref tx) = tui_tx {
                    let _ = tx.send(TuiEvent::Trade(fill.clone()));
                }

                // Phase 2: predictive scoring (fast, non-blocking).
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

                // Phase 1: on-chain verification (slow — 500ms to 10s).
                // Skip when tx_hash is zero (Polymarket's market channel does
                // not deliver settlement tx hashes; that path comes in Phase 3
                // via trade-history polling).
                if !fill.tx_hash.is_zero() {
                    let det_ctx = det_ctx.clone();
                    tokio::spawn(async move {
                        detection::handle_fill(det_ctx, fill).await;
                    });
                }
            }
            ClobEvent::NewMarket {
                market,
                assets_ids,
                question,
                slug,
            } => {
                // Only act if rotation is on AND the slug matches our pattern.
                // Otherwise we'd end up subscribed to every new market that
                // ever launches.
                let matches = rotation_enabled
                    && (rotation_pattern.is_empty() || slug.starts_with(&rotation_pattern));
                if matches && !assets_ids.is_empty() {
                    info!(slug = %slug, tokens = assets_ids.len(), "auto-subscribing to new cycle");
                    if cmd_tx
                        .send(ws::WsCommand::Subscribe(assets_ids.clone()))
                        .await
                        .is_err()
                    {
                        warn!("ws command channel closed — cannot subscribe");
                    }
                }
                if let Some(ref tx) = tui_tx {
                    // The WS `new_market` event doesn't carry outcomes or a
                    // title time window. The Gamma poller will emit a richer
                    // NewMarket shortly after with label info and overwrite.
                    let _ = tx.send(TuiEvent::NewMarket {
                        market,
                        slug,
                        assets_ids,
                        question,
                        outcomes: Vec::new(),
                        time_window: String::new(),
                    });
                }
            }
            ClobEvent::MarketResolved { market, assets_ids } => {
                if rotation_enabled
                    && !assets_ids.is_empty()
                    && cmd_tx
                        .send(ws::WsCommand::Unsubscribe(assets_ids.clone()))
                        .await
                        .is_err()
                {
                    warn!("ws command channel closed — cannot unsubscribe");
                }
                if let Some(ref tx) = tui_tx {
                    let _ = tx.send(TuiEvent::MarketResolved { market, assets_ids });
                }
            }
        }
    }

    warn!("event channel closed");
}

/// Periodic Gamma-API poller that drives market rotation.
///
/// Fires immediately on startup so the TUI header shows the current cycle
/// without waiting 30s, then runs on a fixed interval. On each fetch:
///
/// - Skips if Gamma returned `None` or the fetch failed (next tick retries).
/// - If the current cycle's asset IDs differ from what we last subscribed to,
///   emits `Unsubscribe(old) → Subscribe(new)` WS commands AND a `NewMarket`
///   event to the TUI so the header + feed reflect the rotation.
///
/// Serves as a safety net for when Polymarket's server doesn't honour
/// `custom_feature_enabled: true` (or simply misses a `new_market` push
/// during reconnect).
async fn run_rotation_poller(
    pattern: String,
    interval: std::time::Duration,
    cmd_tx: mpsc::Sender<ws::WsCommand>,
    tui_tx: Option<mpsc::UnboundedSender<TuiEvent>>,
    initial_assets: Vec<String>,
) {
    use tokio::time::MissedTickBehavior;

    let mut current_assets: Vec<String> = initial_assets;
    let mut current_slug = String::new();

    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;

        let cycle = match gamma::fetch_current_cycle(&pattern).await {
            Ok(Some(c)) => c,
            Ok(None) => {
                tracing::debug!(pattern = %pattern, "no active cycle from gamma");
                continue;
            }
            Err(e) => {
                tracing::warn!(error = %e, "gamma fetch failed");
                continue;
            }
        };

        let changed = cycle.slug != current_slug || cycle.assets_ids != current_assets;
        if !changed {
            continue;
        }

        info!(
            new_slug = %cycle.slug,
            old_slug = %current_slug,
            "rotation detected via gamma poller"
        );

        // Unsubscribe from old tokens (if any) first, then subscribe to new.
        let to_unsub: Vec<String> = current_assets
            .iter()
            .filter(|a| !cycle.assets_ids.contains(a))
            .cloned()
            .collect();
        let to_sub: Vec<String> = cycle
            .assets_ids
            .iter()
            .filter(|a| !current_assets.contains(a))
            .cloned()
            .collect();

        // Tell the TUI to mark the old tokens as resolved BEFORE announcing
        // the new cycle. Without this the Markets panel would keep
        // accumulating rows across every rotation.
        if !to_unsub.is_empty() {
            if let Some(ref tx) = tui_tx {
                let _ = tx.send(TuiEvent::MarketResolved {
                    market: current_slug.clone(),
                    assets_ids: to_unsub.clone(),
                });
            }
            let _ = cmd_tx.send(ws::WsCommand::Unsubscribe(to_unsub)).await;
        }
        if !to_sub.is_empty() {
            let _ = cmd_tx.send(ws::WsCommand::Subscribe(to_sub)).await;
        }

        // Tell the TUI about the new cycle — this path carries the full
        // outcome + time-window info so the Markets panel can label each
        // token (e.g. "UP 7:05AM-7:10AM ET").
        if let Some(ref tx) = tui_tx {
            let _ = tx.send(TuiEvent::NewMarket {
                market: cycle.condition_id.clone(),
                slug: cycle.slug.clone(),
                assets_ids: cycle.assets_ids.clone(),
                question: cycle.question.clone(),
                outcomes: cycle.outcomes.clone(),
                time_window: cycle.time_window(),
            });
        }

        current_assets = cycle.assets_ids;
        current_slug = cycle.slug;
    }
}
