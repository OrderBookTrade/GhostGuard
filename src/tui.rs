//! ratatui + crossterm dashboard for GhostGuard.
//!
//! Enabled via `Config::tui_mode = true` (or `--tui` on the CLI). When active,
//! `GhostGuard::start()` pipes live events into a `TuiEvent` channel that the
//! render loop consumes. No polling of external state — the dashboard is a
//! pure function of the event stream.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::time::{Duration, Instant};

use anyhow::Result;
use chrono::{DateTime, Utc};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{event, execute};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap};
use ratatui::{Frame, Terminal};
use tokio::sync::mpsc;

use crate::types::{FillVerdict, GhostFillEvent, PredictiveWarning};
use crate::ws::ClobFill;

/// Events accepted by the TUI render loop.
#[derive(Debug, Clone)]
pub enum TuiEvent {
    /// A trade arrived on the CLOB stream (before any verification).
    /// Emitted for every fill so the feed reflects live activity.
    Trade(ClobFill),
    /// A verdict came back from on-chain verification.
    Verdict {
        verdict: FillVerdict,
        fill: ClobFill,
        /// Time from CLOB receive to verdict.
        latency_ms: u64,
    },
    /// A predictive warning was emitted.
    Warning(PredictiveWarning),
    /// Mid-price update for a market.
    PriceUpdate {
        market: String,
        mid: f64,
    },
    /// WebSocket connection state changed.
    WsConnected,
    WsDisconnected,
    /// Free-form system status message (ws errors, reconnect notices).
    Status(String),
    /// Periodic metadata push from the main loop (e.g. markets list, rpc URL).
    Config {
        markets: Vec<String>,
        rpc_url: String,
        /// How long to keep resolved markets visible in the Markets panel.
        keep_resolved_secs: u64,
    },
    /// A new rotating market opened — TUI should show the new cycle and flash.
    NewMarket {
        market: String,
        slug: String,
        assets_ids: Vec<String>,
        question: String,
        /// Outcome labels in the same order as `assets_ids`, e.g. `["Up","Down"]`.
        /// Empty when the source event didn't include outcomes.
        outcomes: Vec<String>,
        /// Short time-window label derived from the event title, e.g.
        /// `"7:05AM-7:10AM ET"`. Empty when unavailable.
        time_window: String,
    },
    /// A market has been resolved — mark it [RESOLVED] and prune later.
    MarketResolved {
        market: String,
        assets_ids: Vec<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionStatus {
    Connecting,
    Connected,
    Disconnected,
}

impl ConnectionStatus {
    fn label(&self) -> &'static str {
        match self {
            ConnectionStatus::Connecting => "connecting",
            ConnectionStatus::Connected => "connected",
            ConnectionStatus::Disconnected => "disconnected",
        }
    }

    fn color(&self) -> Color {
        match self {
            ConnectionStatus::Connecting => Color::Yellow,
            ConnectionStatus::Connected => Color::Green,
            ConnectionStatus::Disconnected => Color::Red,
        }
    }
}

/// Kind of entry in the scrolling live feed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedKind {
    Trade,
    Real,
    Ghost,
    Warn,
    Auto,
    System,
}

impl FeedKind {
    fn tag(&self) -> &'static str {
        match self {
            FeedKind::Trade => "[TRADE]",
            FeedKind::Real => "[REAL]",
            FeedKind::Ghost => "[GHOST]",
            FeedKind::Warn => "[WARN]",
            FeedKind::Auto => "[AUTO]",
            FeedKind::System => "[SYS]",
        }
    }

    fn style(&self) -> Style {
        match self {
            FeedKind::Trade => Style::default().fg(Color::Blue),
            FeedKind::Real => Style::default().fg(Color::Green),
            FeedKind::Ghost => Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            FeedKind::Warn => Style::default().fg(Color::Yellow),
            FeedKind::Auto => Style::default().fg(Color::Cyan),
            FeedKind::System => Style::default().fg(Color::Magenta),
        }
    }
}

#[derive(Debug, Clone)]
pub struct FeedEntry {
    pub time: DateTime<Utc>,
    pub kind: FeedKind,
    pub tx_short: String,
    pub market: String,
    pub detail: String,
}

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

/// Current rotating market cycle.
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

    /// Prune markets whose resolved grace period has elapsed. Call from the
    /// render loop on every tick.
    pub fn prune_resolved(&mut self) {
        let now = Instant::now();
        let keep = self.keep_resolved;
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

fn short_hex(h: &str) -> String {
    // "0x9e3230ab..." -> "9e3230ab"
    let stripped = h.strip_prefix("0x").unwrap_or(h);
    stripped.chars().take(8).collect()
}

// ---------------------------------------------------------------------------
// Run loop
// ---------------------------------------------------------------------------

/// Run the TUI until the user quits or the event channel closes.
///
/// The caller must feed `TuiEvent`s into `rx` from their real event sources
/// (detection/predictive/ws). This function owns the terminal while running.
pub async fn run_tui(mut rx: mpsc::UnboundedReceiver<TuiEvent>) -> Result<()> {
    // Set up terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut state = TuiState::new();
    let result = event_loop(&mut terminal, &mut state, &mut rx).await;

    // Restore terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    result
}

async fn event_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    state: &mut TuiState,
    rx: &mut mpsc::UnboundedReceiver<TuiEvent>,
) -> Result<()> {
    let tick = Duration::from_millis(250);
    let mut ticker = tokio::time::interval(tick);

    loop {
        // Drain whatever events are immediately available, then render once.
        // This keeps CPU low and rendering smooth under bursts.
        loop {
            match rx.try_recv() {
                Ok(ev) => state.ingest(ev),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => return Ok(()),
            }
        }

        // Periodic maintenance: drop resolved markets past their grace window.
        state.prune_resolved();

        terminal.draw(|f| render(f, state))?;

        tokio::select! {
            _ = ticker.tick() => {}
            ev = rx.recv() => {
                match ev {
                    Some(e) => state.ingest(e),
                    None => return Ok(()),
                }
            }
            key = poll_key() => {
                match key {
                    Ok(Some(KeyCode::Char('q'))) | Ok(Some(KeyCode::Esc)) => return Ok(()),
                    Ok(Some(KeyCode::Char('p'))) => state.paused = !state.paused,
                    Ok(_) => {}
                    Err(_) => {}
                }
            }
        }
    }
}

async fn poll_key() -> Result<Option<KeyCode>> {
    // crossterm poll is sync; run on blocking pool with a short deadline so
    // we don't starve other branches of the select.
    tokio::task::spawn_blocking(|| {
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    return Ok::<_, anyhow::Error>(Some(key.code));
                }
            }
        }
        Ok(None)
    })
    .await
    .unwrap_or(Ok(None))
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn render(f: &mut Frame, state: &TuiState) {
    let size = f.area();

    // Header grows by 1 line when a rotating cycle is active (extra row for
    // cycle name + countdown).
    let header_height = if state.current_cycle.is_some() { 4 } else { 3 };

    // Vertical split: header | body (flex) | stats | markets (flex) | footer
    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(header_height),
            Constraint::Min(5),
            Constraint::Length(5),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(size);

    render_header(f, outer[0], state);
    render_feed(f, outer[1], state);
    render_stats(f, outer[2], state);
    render_markets(f, outer[3], state);
    render_footer(f, outer[4], state);
}

fn render_header(f: &mut Frame, area: Rect, state: &TuiState) {
    // Flash green for 2 seconds after a new market event lands.
    let flash = state
        .last_new_market_at
        .map(|t| t.elapsed() < Duration::from_secs(2))
        .unwrap_or(false);

    let title_style = if flash {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let title_block = Block::default()
        .borders(Borders::ALL)
        .title(Span::styled(" GhostGuard ", title_style));

    let inner = title_block.inner(area);
    f.render_widget(title_block, area);

    // Build line constraints: 3 lines base, +1 if cycle is active.
    let constraints: Vec<Constraint> = if state.current_cycle.is_some() {
        vec![
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ]
    } else {
        vec![
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ]
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    // Line 1: version + uptime
    let line1 = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[0]);
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("GhostGuard ", Style::default().add_modifier(Modifier::BOLD)),
            Span::styled("v0.2.0", Style::default().fg(Color::Cyan)),
        ])),
        line1[0],
    );
    f.render_widget(
        Paragraph::new(format!("uptime {}", state.uptime_hhmmss())).alignment(Alignment::Right),
        line1[1],
    );

    // Line 2: status | markets | ws
    let status_text = if state.paused { "PAUSED" } else { "RUNNING" };
    let status_style = if state.paused {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::Green)
    };
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::raw("Status: "),
            Span::styled(status_text, status_style),
            Span::raw(format!("  |  Markets: {}", state.markets_list.len())),
            Span::raw("  |  WS: "),
            Span::styled(
                state.status.label(),
                Style::default().fg(state.status.color()),
            ),
        ])),
        rows[1],
    );

    // Line 3: rpc | verified
    let rpc_short = if state.rpc_url.len() > 40 {
        format!("{}...", &state.rpc_url[..40])
    } else {
        state.rpc_url.clone()
    };
    f.render_widget(
        Paragraph::new(format!(
            "RPC: {}  |  Fills verified: {}",
            rpc_short, state.stats.total_verified
        )),
        rows[2],
    );

    // Line 4 (only when a cycle is active): Cycle: {slug} ({countdown})
    if let Some(ref cycle) = state.current_cycle {
        let remaining = seconds_until_next_5min_boundary();
        let m = remaining / 60;
        let s = remaining % 60;
        let cycle_style = if flash {
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::Cyan)
        };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::raw("Cycle: "),
                Span::styled(cycle.slug.clone(), cycle_style),
                Span::raw(format!("  ({m}:{s:02} remaining)")),
            ])),
            rows[3],
        );
    }
}

/// Seconds until the next UTC 5-minute boundary (00:00, 00:05, 00:10, ...).
fn seconds_until_next_5min_boundary() -> u64 {
    let now = Utc::now();
    let sec_in_bucket = (now.timestamp() as u64) % 300;
    300 - sec_in_bucket
}

fn render_feed(f: &mut Frame, area: Rect, state: &TuiState) {
    let block = Block::default().borders(Borders::ALL).title(" Live feed ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    // Show as many as fit, newest at bottom
    let visible = inner.height as usize;
    let start = state.feed.len().saturating_sub(visible);
    let lines: Vec<Line> = state
        .feed
        .iter()
        .skip(start)
        .map(|e| {
            let time = e.time.format("%H:%M:%S").to_string();
            let tail = if matches!(e.kind, FeedKind::System) {
                format!(" {}", e.detail)
            } else {
                format!(" tx=0x{}.. market={} {}", e.tx_short, e.market, e.detail)
            };
            Line::from(vec![
                Span::raw(format!("{time} ")),
                Span::styled(e.kind.tag(), e.kind.style()),
                Span::raw(tail),
            ])
        })
        .collect();

    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn render_stats(f: &mut Frame, area: Rect, state: &TuiState) {
    let block = Block::default().borders(Borders::ALL).title(" Stats ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let s = &state.stats;
    let lines = vec![
        Line::from(format!(
            "Verified: {}  Ghost: {} ({:.1}%)  Avg latency: {:.2}s",
            s.total_verified,
            s.total_ghost,
            s.ghost_pct(),
            s.avg_latency_ms / 1000.0,
        )),
        Line::from(format!(
            "Predictive warnings: {}  Accuracy: {:.1}%",
            s.total_warnings, s.warning_accuracy,
        )),
        Line::from(format!("Blacklisted addresses: {}", s.blacklisted_count)),
    ];
    f.render_widget(Paragraph::new(lines), inner);
}

fn render_markets(f: &mut Frame, area: Rect, state: &TuiState) {
    let block = Block::default().borders(Borders::ALL).title(" Markets ");
    let inner = block.inner(area);
    f.render_widget(block, area);

    let header = Row::new(vec![
        Cell::from("market"),
        Cell::from("cycle"),
        Cell::from("mid"),
        Cell::from("avg size"),
        Cell::from("fills/min"),
        Cell::from("ghost %"),
    ])
    .style(Style::default().add_modifier(Modifier::BOLD));

    let mut rows: Vec<&MarketRow> = state.markets.values().collect();
    // Sort by label first so UP / DOWN of the same cycle sit next to each
    // other, then by slug as tiebreaker for deterministic order.
    rows.sort_by(|a, b| {
        a.label
            .as_deref()
            .unwrap_or("")
            .cmp(b.label.as_deref().unwrap_or(""))
            .then_with(|| a.slug.cmp(&b.slug))
    });

    let body: Vec<Row> = rows
        .iter()
        .map(|m| {
            let ghost = m.ghost_rate();
            let resolved = state.resolved_at.contains_key(&m.slug);
            let base_slug = if m.slug.len() > 14 {
                format!("{}..", &m.slug[..12])
            } else {
                m.slug.clone()
            };
            let slug_cell = if resolved {
                format!("{base_slug} [RESOLVED]")
            } else {
                base_slug
            };
            let cycle_label = m.label.clone().unwrap_or_else(|| "-".into());
            let mid = m
                .mid_price
                .map(|p| format!("{p:.4}"))
                .unwrap_or_else(|| "-".into());
            let mut row = Row::new(vec![
                Cell::from(slug_cell),
                Cell::from(cycle_label),
                Cell::from(mid),
                Cell::from(format!("{:.2}", m.avg_size())),
                Cell::from(format!("{}", m.fills_per_min())),
                Cell::from(format!("{ghost:.1}")),
            ]);
            if resolved {
                row = row.style(Style::default().fg(Color::DarkGray));
            } else if ghost > 5.0 {
                row = row.style(Style::default().fg(Color::Red).add_modifier(Modifier::BOLD));
            }
            row
        })
        .collect();

    let widths = [
        Constraint::Percentage(18),
        Constraint::Percentage(32),
        Constraint::Percentage(10),
        Constraint::Percentage(14),
        Constraint::Percentage(13),
        Constraint::Percentage(13),
    ];
    let table = Table::new(body, widths).header(header);
    f.render_widget(table, inner);
}

fn render_footer(f: &mut Frame, area: Rect, _state: &TuiState) {
    let help = Line::from(vec![
        Span::styled("[q]", Style::default().fg(Color::Cyan)),
        Span::raw("uit  "),
        Span::styled("[p]", Style::default().fg(Color::Cyan)),
        Span::raw("ause  "),
        Span::styled("[b]", Style::default().fg(Color::DarkGray)),
        Span::styled("lacklist (Phase 3)  ", Style::default().fg(Color::DarkGray)),
        Span::styled("[m]", Style::default().fg(Color::DarkGray)),
        Span::styled("arkets (Phase 3)", Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(Paragraph::new(help), area);
}

// ---------------------------------------------------------------------------
// Helper: convert GhostFillEvent into a ClobFill for feed purposes
// ---------------------------------------------------------------------------

/// Convenience for upstream dispatchers that only have a `GhostFillEvent`.
pub fn ghost_event_to_fill(ev: &GhostFillEvent) -> ClobFill {
    ClobFill {
        tx_hash: ev.tx_hash,
        market: ev.market.clone(),
        side: ev.side.clone(),
        size: ev.size,
        price: ev.price,
    }
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
        // Late ws ticks for tokens we never subscribed to (or already retired)
        // must NOT resurrect a phantom row in the Markets panel.
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
        // Track the market explicitly via NewMarket.
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
        // Feed should still record the trade for forensic visibility.
        assert_eq!(s.feed.len(), 1);
    }

    #[test]
    fn test_short_hex() {
        assert_eq!(short_hex("0x9e3230abcdef"), "9e3230ab");
        assert_eq!(short_hex("9e3230abcdef"), "9e3230ab");
        assert_eq!(short_hex("0xabc"), "abc");
    }

    #[test]
    fn test_ghost_rate_flag_threshold() {
        // Sanity: >5% should be flagged by render (can only check the rate value)
        let mut m = MarketRow::new("m".into());
        let now = Instant::now();
        for _ in 0..100 {
            m.record_fill(1.0, now);
        }
        m.ghost_count = 10;
        assert!((m.ghost_rate() - 10.0).abs() < 1e-9);
    }
}
