//! ratatui + crossterm dashboard for GhostGuard.
//!
//! Enabled via `Config::tui_mode = true` (or `--tui` on the CLI). When active,
//! `GhostGuard::start()` pipes live events into a `TuiEvent` channel that the
//! render loop consumes. No polling of external state — the dashboard is a
//! pure function of the event stream.
//!
//! ## Module layout
//!
//! - [`events`] — `TuiEvent`, `FeedKind`, `ConnectionStatus` (the public API
//!   surface for callers in `lib.rs`).
//! - [`state`]  — `TuiState`, `MarketRow`, `CycleInfo`, `DashboardStats`,
//!   `FeedEntry`. Pure data + the `ingest` dispatcher.
//! - [`render`] — pure presentation; turns `TuiState` into ratatui widgets.
//! - this `mod.rs` — terminal lifecycle, async event loop, key handling.

mod events;
mod render;
mod state;

use std::io;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::{DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::{event, execute};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use tokio::sync::mpsc;

pub use events::{ConnectionStatus, FeedKind, TuiEvent};
pub use state::{CycleInfo, DashboardStats, FeedEntry, MarketRow, TuiState};

use crate::types::GhostFillEvent;
use crate::ws::ClobFill;

/// Run the TUI until the user quits or the event channel closes.
///
/// The caller must feed `TuiEvent`s into `rx` from their real event sources
/// (detection / predictive / ws). This function owns the terminal while
/// running and restores it cleanly on exit.
pub async fn run_tui(mut rx: mpsc::UnboundedReceiver<TuiEvent>) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut state = TuiState::new();
    let result = event_loop(&mut terminal, &mut state, &mut rx).await;

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
        // Keeps CPU low and rendering smooth under bursts.
        loop {
            match rx.try_recv() {
                Ok(ev) => state.ingest(ev),
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => return Ok(()),
            }
        }

        // Periodic maintenance: drop resolved markets past their grace window
        // AND any phantom rows that aren't part of the current cycle.
        state.prune_resolved();

        terminal.draw(|f| render::render(f, state))?;

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
