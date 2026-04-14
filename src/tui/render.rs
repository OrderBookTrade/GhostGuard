//! Rendering — translates `TuiState` into ratatui widgets.
//!
//! Pure presentation. No state mutation, no event handling. Only the top-level
//! `render` function is exposed to the surrounding module; all the per-panel
//! helpers are private.

use std::time::Duration;

use chrono::Utc;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap};
use ratatui::Frame;

use super::events::FeedKind;
use super::state::{MarketRow, TuiState};

pub(super) fn render(f: &mut Frame, state: &TuiState) {
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
