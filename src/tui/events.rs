//! TUI-facing event types.
//!
//! `TuiEvent` is the only way data gets into the dashboard from the rest of
//! the system (ws listener, detection, predictive, gamma poller). Splitting
//! these out from state/render keeps the API surface minimal and makes
//! cross-module imports obvious.

use ratatui::style::{Color, Modifier, Style};

use crate::types::{FillVerdict, PredictiveWarning};
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
    pub(super) fn label(&self) -> &'static str {
        match self {
            ConnectionStatus::Connecting => "connecting",
            ConnectionStatus::Connected => "connected",
            ConnectionStatus::Disconnected => "disconnected",
        }
    }

    pub(super) fn color(&self) -> Color {
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
    pub(super) fn tag(&self) -> &'static str {
        match self {
            FeedKind::Trade => "[TRADE]",
            FeedKind::Real => "[REAL]",
            FeedKind::Ghost => "[GHOST]",
            FeedKind::Warn => "[WARN]",
            FeedKind::Auto => "[AUTO]",
            FeedKind::System => "[SYS]",
        }
    }

    pub(super) fn style(&self) -> Style {
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
