//! `bd dash --tui` — interactive live dashboard backed by ratatui.
//!
//! The classic `bd dash` still prints-and-exits (script-friendly).
//! When `--tui` is set, we take over the alternate screen, enter raw
//! mode, and run an event loop that redraws on key input or on a
//! refresh timer. `ratatui::run` installs the panic hook and restores
//! the terminal even on unwind.
//!
//! This module owns rendering only — data fetch lives in
//! [`super::dash::snapshot`], shared with the print-and-exit path.

use crate::cli::DashArgs;
use crate::cli::commands::dash::{
    OwnedGroup, PresenceView, StatusKind, format_age_compact, snapshot,
};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::output::OutputContext;
use chrono::Utc;
use ratatui::Frame;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use std::collections::HashSet;
use std::io::IsTerminal;
use std::path::Path;
use std::time::{Duration, Instant};

/// Max wait between event-loop wake-ups. Short enough for snappy key
/// response; the actual refetch cadence is `RefreshSchedule`.
const EVENT_POLL_MS: u64 = 100;

/// Default refresh interval when `--refresh` isn't passed. Same
/// default as the print-and-exit dash uses for its `--refresh` mode
/// when the user omits the explicit value.
const DEFAULT_REFRESH_SECS: u64 = 2;

/// One row of the flattened, currently-visible list. Rebuilt every
/// frame from the snapshot + expansion state. The `prefix` slots on
/// Bead/Closure are used by .3's collapse navigation (h jumps cursor
/// to that prefix's header).
#[allow(dead_code)]
enum VisibleRow {
    /// A prefix header. Always present for each group; collapse state
    /// determines whether child rows follow.
    Header { prefix: String },
    /// One live bead under a prefix.
    Bead {
        prefix: String,
        line: Line<'static>,
    },
    /// A recently-closed bead under a prefix.
    Closure {
        prefix: String,
        line: Line<'static>,
    },
}

impl VisibleRow {
    #[allow(dead_code)]
    fn prefix(&self) -> &str {
        match self {
            Self::Header { prefix } | Self::Bead { prefix, .. } | Self::Closure { prefix, .. } => {
                prefix
            }
        }
    }
}

struct App {
    groups: Vec<OwnedGroup>,
    pending_asks: usize,
    /// Prefixes whose children are currently shown. Default = every
    /// prefix in `groups` (start fully expanded).
    expanded: HashSet<String>,
    rows: Vec<VisibleRow>,
    state: ListState,
    show_help: bool,
}

impl App {
    fn new(groups: Vec<OwnedGroup>, pending_asks: usize) -> Self {
        let expanded: HashSet<String> = groups.iter().map(|g| g.prefix.clone()).collect();
        let rows = build_rows(&groups, &expanded);
        let mut state = ListState::default();
        if !rows.is_empty() {
            state.select(Some(0));
        }
        Self {
            groups,
            pending_asks,
            expanded,
            rows,
            state,
            show_help: false,
        }
    }

    fn select_first(&mut self) {
        if !self.rows.is_empty() {
            self.state.select(Some(0));
        }
    }

    fn select_last(&mut self) {
        if !self.rows.is_empty() {
            self.state.select(Some(self.rows.len() - 1));
        }
    }

    fn select_next(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let i = self.state.selected().unwrap_or(0);
        self.state.select(Some((i + 1).min(self.rows.len() - 1)));
    }

    fn select_prev(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let i = self.state.selected().unwrap_or(0);
        self.state.select(Some(i.saturating_sub(1)));
    }

    /// Current row's prefix, if any.
    fn current_prefix(&self) -> Option<&str> {
        self.state
            .selected()
            .and_then(|i| self.rows.get(i))
            .map(VisibleRow::prefix)
    }

    /// Whether the current row is itself a Header.
    fn on_header(&self) -> bool {
        matches!(
            self.state.selected().and_then(|i| self.rows.get(i)),
            Some(VisibleRow::Header { .. })
        )
    }

    /// `h` / Left semantics:
    /// - on a child row → jump cursor to that prefix's header (don't collapse)
    /// - on an expanded header → collapse it (cursor stays)
    /// - on a collapsed header → no-op
    fn collapse_or_jump(&mut self) {
        let Some(prefix) = self.current_prefix().map(str::to_string) else {
            return;
        };
        if self.on_header() {
            self.expanded.remove(&prefix);
            self.rebuild_rows_preserving(&prefix);
        } else {
            self.jump_to_header(&prefix);
        }
    }

    /// `l` / Right semantics: expand current prefix's header.
    fn expand(&mut self) {
        let Some(prefix) = self.current_prefix().map(str::to_string) else {
            return;
        };
        if self.expanded.insert(prefix.clone()) {
            self.rebuild_rows_preserving(&prefix);
        }
    }

    /// Space toggles the prefix the cursor is currently within.
    fn toggle(&mut self) {
        let Some(prefix) = self.current_prefix().map(str::to_string) else {
            return;
        };
        if self.expanded.contains(&prefix) {
            self.expanded.remove(&prefix);
        } else {
            self.expanded.insert(prefix.clone());
        }
        self.rebuild_rows_preserving(&prefix);
    }

    /// Replace `rows` from current state. After collapses the previous
    /// selected index may point past the end (or to a now-gone child);
    /// move the cursor to `preserve_prefix`'s header so the user keeps
    /// their bearings.
    fn rebuild_rows_preserving(&mut self, preserve_prefix: &str) {
        self.rows = build_rows(&self.groups, &self.expanded);
        if self.rows.is_empty() {
            self.state.select(None);
            return;
        }
        let target = self
            .rows
            .iter()
            .position(|r| matches!(r, VisibleRow::Header { prefix } if prefix == preserve_prefix))
            .unwrap_or(0);
        self.state.select(Some(target.min(self.rows.len() - 1)));
    }

    fn jump_to_header(&mut self, prefix: &str) {
        if let Some(i) = self
            .rows
            .iter()
            .position(|r| matches!(r, VisibleRow::Header { prefix: p } if p == prefix))
        {
            self.state.select(Some(i));
        }
    }

    /// Replace `groups` + `pending_asks` from a fresh snapshot,
    /// preserving cursor position by remembering which prefix the
    /// selection lived under and re-selecting that prefix's header
    /// if it still exists.
    fn apply_snapshot(&mut self, groups: Vec<OwnedGroup>, pending_asks: usize) {
        let cursor_prefix = self.current_prefix().map(str::to_string);

        // Drop expanded entries for prefixes that no longer exist.
        let live: HashSet<String> = groups.iter().map(|g| g.prefix.clone()).collect();
        self.expanded.retain(|p| live.contains(p));
        // New prefixes default to expanded so users see them.
        for p in &live {
            self.expanded.insert(p.clone());
        }

        self.groups = groups;
        self.pending_asks = pending_asks;
        self.rows = build_rows(&self.groups, &self.expanded);

        if self.rows.is_empty() {
            self.state.select(None);
            return;
        }
        let target = cursor_prefix
            .as_deref()
            .and_then(|p| {
                self.rows.iter().position(
                    |r| matches!(r, VisibleRow::Header { prefix } if prefix == p),
                )
            })
            .unwrap_or(0);
        self.state.select(Some(target.min(self.rows.len() - 1)));
    }
}

/// Build the flattened visible-row vec from a snapshot. Children of
/// a prefix are only emitted when that prefix is in `expanded`.
fn build_rows(groups: &[OwnedGroup], expanded: &HashSet<String>) -> Vec<VisibleRow> {
    let mut rows: Vec<VisibleRow> = Vec::new();
    for g in groups {
        rows.push(VisibleRow::Header {
            prefix: g.prefix.clone(),
        });
        if !expanded.contains(&g.prefix) {
            continue;
        }
        for b in &g.beads {
            let line = bead_line(b);
            rows.push(VisibleRow::Bead {
                prefix: g.prefix.clone(),
                line,
            });
        }
        for c in &g.recently_closed {
            let line = closure_line(c);
            rows.push(VisibleRow::Closure {
                prefix: g.prefix.clone(),
                line,
            });
        }
    }
    rows
}

fn header_line(group: &OwnedGroup, expanded: bool) -> Line<'static> {
    let chevron = if expanded { "▾ " } else { "▸ " };
    let bold = Style::default().add_modifier(Modifier::BOLD);

    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(chevron, bold));
    spans.push(Span::styled("[", bold));

    if let Some(p) = &group.presence {
        let presence_style = presence_style(p).add_modifier(Modifier::BOLD);
        spans.push(Span::styled(p.state.glyph().to_string(), presence_style));
        spans.push(Span::styled(" ", bold));
    }

    spans.push(Span::styled(group.prefix.clone(), bold));

    if let Some(p) = &group.presence {
        spans.push(Span::styled(
            format!(" {}", format_age_compact(p.age_secs)),
            bold,
        ));
    }

    spans.push(Span::styled("]", bold));

    let counts = header_counts(group);
    if !counts.is_empty() {
        spans.push(Span::styled(format!(" {counts}"), bold));
    }

    Line::from(spans)
}

fn presence_style(p: &PresenceView) -> Style {
    match p.state {
        crate::cli::commands::dash::PresenceKind::Working => Style::default().fg(Color::Green),
        crate::cli::commands::dash::PresenceKind::Idle => Style::default().add_modifier(Modifier::DIM),
        crate::cli::commands::dash::PresenceKind::Offline => Style::default().add_modifier(Modifier::DIM),
    }
}

fn status_glyph_style(kind: StatusKind) -> Style {
    match kind {
        StatusKind::InProgress => Style::default().fg(Color::Green),
        StatusKind::Ready => Style::default().fg(Color::Blue),
        StatusKind::Blocked => Style::default().fg(Color::Red),
        StatusKind::Deferred | StatusKind::Closed => Style::default().add_modifier(Modifier::DIM),
    }
}

fn header_counts(g: &OwnedGroup) -> String {
    let mut parts: Vec<String> = Vec::new();
    if g.in_progress > 0 {
        parts.push(format!("{} working", g.in_progress));
    }
    if g.ready > 0 {
        parts.push(format!("{} ready", g.ready));
    }
    if g.blocked > 0 {
        parts.push(format!("{} blocked", g.blocked));
    }
    if g.deferred > 0 {
        parts.push(format!("{} deferred", g.deferred));
    }
    if g.closed_recently > 0 {
        parts.push(format!("{} closed", g.closed_recently));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("({})", parts.join(", "))
    }
}

fn bead_line(b: &crate::cli::commands::dash::OwnedBead) -> Line<'static> {
    let glyph = StatusKind::glyph(b.kind);
    let pri = format!("P{}", b.priority.0);
    let assignee = b
        .assignee
        .as_deref()
        .map(|a| format!(" [{a}]"))
        .unwrap_or_default();
    let parent = b
        .parent
        .as_deref()
        .map(|p| format!(" ← {p}"))
        .unwrap_or_default();
    let sender = b
        .sender
        .as_deref()
        .map(|s| format!(" (from: {s})"))
        .unwrap_or_default();
    let tail = format!(
        " {id}  {pri}  {title}{parent}{sender}{assignee}",
        id = b.id,
        title = b.title,
    );
    Line::from(vec![
        Span::raw("  "),
        Span::styled(glyph.to_string(), status_glyph_style(b.kind)),
        Span::raw(tail),
    ])
}

fn closure_line(c: &crate::cli::commands::dash::OwnedClosure) -> Line<'static> {
    let assignee = c
        .assignee
        .as_deref()
        .map(|a| format!(" [{a}]"))
        .unwrap_or_default();
    let sender = c
        .sender
        .as_deref()
        .map(|s| format!(" (from: {s})"))
        .unwrap_or_default();
    let text = format!(
        "  ✓ {id}  {title}  ({age} ago){sender}{assignee}",
        id = c.id,
        title = c.title,
        age = format_age_compact(c.age_secs),
    );
    Line::from(Span::styled(
        text,
        Style::default().add_modifier(Modifier::DIM),
    ))
}

/// Execute `bd dash --tui`.
///
/// # Errors
///
/// Returns an error if storage open or terminal init fails.
pub fn execute(args: &DashArgs, cli: &config::CliOverrides, _ctx: &OutputContext) -> Result<()> {
    if !std::io::stdout().is_terminal() {
        return Err(BeadsError::validation(
            "tui",
            "`bd dash --tui` needs an interactive terminal; \
             omit --tui to get the plain text dash.",
        ));
    }

    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let initial = snapshot(args, cli, &beads_dir, Utc::now())?;
    let mut app = App::new(initial.0, initial.1);

    let refresh_every =
        Duration::from_secs(args.refresh.unwrap_or(DEFAULT_REFRESH_SECS).max(1));
    let mut next_refresh = Instant::now() + refresh_every;

    ratatui::run(|terminal| -> std::io::Result<()> {
        loop {
            terminal.draw(|f| draw(f, &mut app))?;

            // Wake on either: a key event arrives, or the refresh
            // timer fires, whichever is sooner.
            let timeout = next_refresh
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(EVENT_POLL_MS));

            if event::poll(timeout)? {
                if let Event::Key(key) = event::read()?
                    && key.kind == KeyEventKind::Press
                {
                    // Esc / q / ? close the help overlay first if open;
                    // other keys are ignored until it's dismissed.
                    if app.show_help {
                        if matches!(
                            key.code,
                            KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Esc
                        ) {
                            app.show_help = false;
                        }
                    } else {
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                return Ok(());
                            }
                            KeyCode::Char('j') | KeyCode::Down => app.select_next(),
                            KeyCode::Char('k') | KeyCode::Up => app.select_prev(),
                            KeyCode::Char('h') | KeyCode::Left => app.collapse_or_jump(),
                            KeyCode::Char('l') | KeyCode::Right => app.expand(),
                            KeyCode::Char(' ') => app.toggle(),
                            KeyCode::Char('g') | KeyCode::Home => app.select_first(),
                            KeyCode::Char('G') | KeyCode::End => app.select_last(),
                            KeyCode::Char('?') => app.show_help = true,
                            KeyCode::Char('r') => {
                                // Force refresh immediately; reset the next-refresh deadline.
                                refresh(&mut app, args, cli, &beads_dir);
                                next_refresh = Instant::now() + refresh_every;
                            }
                            _ => {}
                        }
                    }
                }
                // Other events (Resize, Mouse, etc.) just trigger a redraw.
            }

            if Instant::now() >= next_refresh {
                refresh(&mut app, args, cli, &beads_dir);
                next_refresh = Instant::now() + refresh_every;
            }
        }
    })
    .map_err(BeadsError::from)
}

/// Best-effort snapshot refresh — a single failed query shouldn't
/// kill the TUI loop. If the refresh fails, the previous snapshot
/// continues to render.
fn refresh(app: &mut App, args: &DashArgs, cli: &config::CliOverrides, beads_dir: &Path) {
    if let Ok((groups, pending)) = snapshot(args, cli, beads_dir, Utc::now()) {
        app.apply_snapshot(groups, pending);
    }
}

fn draw(frame: &mut Frame<'_>, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(frame.area());

    // Top status line.
    let status: Paragraph<'_> = if app.pending_asks > 0 {
        let noun = if app.pending_asks == 1 { "ask" } else { "asks" };
        Paragraph::new(format!(
            "operator: {} {} pending — run `bd admin operator attend`",
            app.pending_asks, noun
        ))
        .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
    } else {
        Paragraph::new("")
    };
    frame.render_widget(status, chunks[0]);

    // Scrollable list. We build ListItems on the fly, looking up
    // headers by prefix from the snapshot rather than caching them
    // alongside VisibleRow — keeps build_rows cheap.
    let items: Vec<ListItem<'_>> = app
        .rows
        .iter()
        .map(|row| match row {
            VisibleRow::Header { prefix } => {
                let group = app.groups.iter().find(|g| g.prefix == *prefix);
                let expanded = app.expanded.contains(prefix);
                let line = group
                    .map(|g| header_line(g, expanded))
                    .unwrap_or_else(|| Line::from(prefix.clone()));
                ListItem::new(line)
            }
            VisibleRow::Bead { line, .. } | VisibleRow::Closure { line, .. } => {
                ListItem::new(line.clone())
            }
        })
        .collect();

    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title("dashboard"))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, chunks[1], &mut app.state);

    // Footer help.
    let footer =
        Paragraph::new("j/k nav  h/l fold  space toggle  g/G first/last  r refresh  ? help  q quit")
            .style(Style::default().add_modifier(Modifier::DIM));
    frame.render_widget(footer, chunks[2]);

    if app.show_help {
        draw_help_overlay(frame, frame.area());
    }
}

fn draw_help_overlay(frame: &mut Frame<'_>, area: Rect) {
    let lines: Vec<Line<'_>> = vec![
        Line::from(Span::styled(
            "bd dash — keys",
            Style::default().add_modifier(Modifier::BOLD),
        )),
        Line::raw(""),
        Line::raw("j / ↓        next row"),
        Line::raw("k / ↑        previous row"),
        Line::raw("h / ←        collapse current prefix (or jump to its header)"),
        Line::raw("l / →        expand current prefix"),
        Line::raw("space        toggle current prefix"),
        Line::raw("g / Home     first row"),
        Line::raw("G / End      last row"),
        Line::raw("r            refresh now"),
        Line::raw("?            toggle this help"),
        Line::raw("q / Esc      quit"),
        Line::raw("Ctrl-C       quit"),
        Line::raw(""),
        Line::from(Span::styled(
            "press ? or Esc to close",
            Style::default().add_modifier(Modifier::DIM),
        )),
    ];

    let rect = centered_rect(60, 60, area);
    frame.render_widget(Clear, rect);
    let block = Block::default().borders(Borders::ALL).title("help");
    let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: false });
    frame.render_widget(para, rect);
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let popup_height = area.height * percent_y / 100;
    let popup_width = area.width * percent_x / 100;
    let x = area.x + (area.width - popup_width) / 2;
    let y = area.y + (area.height - popup_height) / 2;
    Rect {
        x,
        y,
        width: popup_width,
        height: popup_height,
    }
}
