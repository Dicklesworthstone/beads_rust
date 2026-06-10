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
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Config-table key that persists the user's collapsed-prefix set
/// across `bd dash --tui` invocations.
const COLLAPSED_KEY: &str = "dash_tui_collapsed";

/// Config-table key for the persisted sort mode.
const SORT_KEY: &str = "dash_tui_sort";

/// Config-table key for the persisted view mode.
const VIEW_KEY: &str = "dash_tui_view";

/// Which pane is on screen. Tab cycles forward; Shift-Tab cycles
/// backward; 1/2 jump directly. Each mode owns its own scroll state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ViewMode {
    Dashboard,
    Graph,
}

impl ViewMode {
    const fn label(self) -> &'static str {
        match self {
            Self::Dashboard => "dashboard",
            Self::Graph => "graph",
        }
    }

    const fn next(self) -> Self {
        match self {
            Self::Dashboard => Self::Graph,
            Self::Graph => Self::Dashboard,
        }
    }

    const fn prev(self) -> Self {
        // Two modes — same as next, but kept distinct so Shift-Tab
        // does the right thing once we add a third mode.
        self.next()
    }

    fn from_label(s: &str) -> Self {
        match s {
            "graph" => Self::Graph,
            _ => Self::Dashboard,
        }
    }
}

/// How the prefix list is ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SortMode {
    /// Alphabetical by prefix (the default).
    Name,
    /// Working agents first, then idle, then offline / no presence;
    /// alpha within each tier so the ordering is stable.
    Activity,
}

impl SortMode {
    const fn label(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Activity => "activity",
        }
    }

    const fn next(self) -> Self {
        match self {
            Self::Name => Self::Activity,
            Self::Activity => Self::Name,
        }
    }

    fn from_label(s: &str) -> Self {
        match s {
            "activity" => Self::Activity,
            _ => Self::Name,
        }
    }
}

/// Per-bead detail-pane state. Holds the pre-rendered Lines so we
/// build them once on Enter (and on 'r' inside detail), not every
/// frame. Scroll offset is line-indexed against `lines`.
struct DetailState {
    bead_id: String,
    title_for_pane: String,
    lines: Vec<Line<'static>>,
    scroll: u16,
}

/// Numeric tier for activity sort: lower = more active. Used as the
/// primary key when SortMode::Activity is active.
fn activity_tier(g: &OwnedGroup) -> u8 {
    match g.presence.as_ref().map(|p| p.state) {
        Some(crate::cli::commands::dash::PresenceKind::Working) => 0,
        Some(crate::cli::commands::dash::PresenceKind::Idle) => 1,
        _ => 2,
    }
}

/// Sort `groups` in place per the active mode. Always stable on the
/// prefix as a secondary key.
fn sort_groups(groups: &mut [OwnedGroup], mode: SortMode) {
    match mode {
        SortMode::Name => groups.sort_by(|a, b| a.prefix.cmp(&b.prefix)),
        SortMode::Activity => groups.sort_by(|a, b| {
            activity_tier(a)
                .cmp(&activity_tier(b))
                .then_with(|| a.prefix.cmp(&b.prefix))
        }),
    }
}

/// Max wait between event-loop wake-ups. Short enough for snappy key
/// response; the actual refetch cadence is `RefreshSchedule`.
const EVENT_POLL_MS: u64 = 100;

/// Default refresh interval when `--refresh` isn't passed. Same
/// default as the print-and-exit dash uses for its `--refresh` mode
/// when the user omits the explicit value.
const DEFAULT_REFRESH_SECS: u64 = 2;

/// One row of the flattened, currently-visible list. Rebuilt every
/// frame from the snapshot + collapse state. We carry `bead_id` on
/// child rows so the cursor can re-anchor on the exact same bead
/// across refreshes (not just the parent header).
enum VisibleRow {
    Header {
        prefix: String,
    },
    Bead {
        prefix: String,
        bead_id: String,
        line: Line<'static>,
    },
    Closure {
        prefix: String,
        bead_id: String,
        line: Line<'static>,
    },
}

impl VisibleRow {
    fn prefix(&self) -> &str {
        match self {
            Self::Header { prefix } | Self::Bead { prefix, .. } | Self::Closure { prefix, .. } => {
                prefix
            }
        }
    }
}

/// Stable identity of the currently-selected row, used to re-anchor
/// the cursor across snapshot refreshes. We track the row *kind*, not
/// just the prefix, so a cursor parked on a specific bead stays there
/// even when neighboring rows shift.
#[derive(Clone)]
enum CursorKey {
    Header(String),
    Bead { prefix: String, bead_id: String },
    Closure { prefix: String, bead_id: String },
}

impl CursorKey {
    fn prefix(&self) -> &str {
        match self {
            Self::Header(p) | Self::Bead { prefix: p, .. } | Self::Closure { prefix: p, .. } => p,
        }
    }
}

struct App {
    groups: Vec<OwnedGroup>,
    pending_asks: usize,
    /// Prefixes the user has explicitly collapsed. Empty = everything
    /// expanded (the default). Tracking *collapses* rather than
    /// expansions means refreshes don't accidentally re-expand a
    /// prefix the user just folded.
    collapsed: HashSet<String>,
    sort: SortMode,
    rows: Vec<VisibleRow>,
    state: ListState,
    show_help: bool,
    /// Active view. Each mode keeps its own scroll/cursor state.
    view: ViewMode,
    /// Cached graph rendering. Refreshed on the same cadence as the
    /// dashboard snapshot. Stored as styled ratatui Lines so the
    /// graph view shows color (status, priority, headers).
    graph_lines: Vec<Line<'static>>,
    graph_scroll: u16,
    /// Active detail-view state, if any. `Some(...)` means the user
    /// pressed Enter on a bead and is currently looking at its
    /// details; key handling switches to detail bindings until they
    /// press q / Backspace to return.
    detail: Option<DetailState>,
    /// Held for self-persistence — the App writes its collapsed-set,
    /// sort mode, and view mode back to the config table on every
    /// user change so they survive across `bd dash --tui` invocations.
    beads_dir: PathBuf,
    cli: config::CliOverrides,
}

impl App {
    fn new(
        mut groups: Vec<OwnedGroup>,
        pending_asks: usize,
        beads_dir: PathBuf,
        cli: config::CliOverrides,
    ) -> Self {
        let collapsed = load_collapsed(&beads_dir, &cli).unwrap_or_default();
        let sort = load_sort(&beads_dir, &cli).unwrap_or(SortMode::Name);
        let view = load_view(&beads_dir, &cli).unwrap_or(ViewMode::Dashboard);
        sort_groups(&mut groups, sort);
        let rows = build_rows(&groups, &collapsed);
        let mut state = ListState::default();
        if !rows.is_empty() {
            state.select(Some(0));
        }
        Self {
            groups,
            pending_asks,
            collapsed,
            sort,
            rows,
            state,
            show_help: false,
            view,
            graph_lines: Vec::new(),
            graph_scroll: 0,
            detail: None,
            beads_dir,
            cli,
        }
    }

    /// Best-effort write of `collapsed` back to the config table.
    /// A failed write shouldn't disrupt the TUI; the state simply
    /// doesn't persist for this session.
    fn save_collapsed(&self) {
        if let Ok((mut storage, _paths)) =
            config::open_storage(&self.beads_dir, self.cli.db.as_ref(), self.cli.lock_timeout)
        {
            let mut list: Vec<&str> = self.collapsed.iter().map(String::as_str).collect();
            list.sort_unstable();
            if let Ok(json) = serde_json::to_string(&list) {
                let _ = storage.set_config(COLLAPSED_KEY, &json);
            }
        }
    }

    /// Same best-effort write for the sort mode.
    fn save_sort(&self) {
        if let Ok((mut storage, _paths)) =
            config::open_storage(&self.beads_dir, self.cli.db.as_ref(), self.cli.lock_timeout)
        {
            let _ = storage.set_config(SORT_KEY, self.sort.label());
        }
    }

    /// Cycle to the next sort mode and persist the choice.
    /// Cursor stays on the same row (we re-anchor by CursorKey, which
    /// is identity-based, not position-based).
    fn cycle_sort(&mut self) {
        let key = self.cursor_key();
        self.sort = self.sort.next();
        sort_groups(&mut self.groups, self.sort);
        self.rows = build_rows(&self.groups, &self.collapsed);
        self.select_by_key(key);
        self.save_sort();
    }

    fn save_view(&self) {
        if let Ok((mut storage, _paths)) =
            config::open_storage(&self.beads_dir, self.cli.db.as_ref(), self.cli.lock_timeout)
        {
            let _ = storage.set_config(VIEW_KEY, self.view.label());
        }
    }

    fn set_view(&mut self, v: ViewMode) {
        if self.view == v {
            return;
        }
        self.view = v;
        self.save_view();
    }

    fn cycle_view_forward(&mut self) {
        self.set_view(self.view.next());
    }

    fn cycle_view_back(&mut self) {
        self.set_view(self.view.prev());
    }

    /// Re-fetch the graph rendering. Best-effort — keeps the previous
    /// text on failure rather than blanking the view.
    fn refresh_graph(&mut self) {
        let args = crate::cli::GraphArgs {
            all: true,
            ..Default::default()
        };
        if let Ok(lines) = crate::cli::commands::graph::render_all_lines(&self.cli, &args) {
            self.graph_lines = lines;
            // Clamp scroll if the new content is shorter.
            let max = self.graph_lines.len().saturating_sub(1) as u16;
            if self.graph_scroll > max {
                self.graph_scroll = max;
            }
        }
    }

    fn graph_scroll_down(&mut self) {
        let max = (self.graph_lines.len().saturating_sub(1)) as u16;
        if self.graph_scroll < max {
            self.graph_scroll += 1;
        }
    }

    fn graph_scroll_up(&mut self) {
        self.graph_scroll = self.graph_scroll.saturating_sub(1);
    }

    fn graph_scroll_first(&mut self) {
        self.graph_scroll = 0;
    }

    fn graph_scroll_last(&mut self) {
        self.graph_scroll = self.graph_lines.len().saturating_sub(1) as u16;
    }

    /// Get the bead-id under the cursor, if any.
    fn cursor_bead_id(&self) -> Option<&str> {
        match self.state.selected().and_then(|i| self.rows.get(i)) {
            Some(VisibleRow::Bead { bead_id, .. } | VisibleRow::Closure { bead_id, .. }) => {
                Some(bead_id.as_str())
            }
            _ => None,
        }
    }

    /// Open the detail pane for the currently-selected bead. Best-
    /// effort — if the fetch fails (DB lock, deleted between snapshot
    /// and now), silently no-op so the UI stays responsive.
    fn open_detail(&mut self) {
        let Some(bead_id) = self.cursor_bead_id().map(str::to_string) else {
            return;
        };
        if let Some(state) = load_detail(&self.beads_dir, &self.cli, &bead_id) {
            self.detail = Some(state);
        }
    }

    /// Re-fetch the current detail (used by 'r' inside the pane).
    fn refresh_detail(&mut self) {
        let Some(bead_id) = self.detail.as_ref().map(|d| d.bead_id.clone()) else {
            return;
        };
        if let Some(state) = load_detail(&self.beads_dir, &self.cli, &bead_id) {
            self.detail = Some(state);
        }
    }

    fn close_detail(&mut self) {
        self.detail = None;
    }

    fn detail_scroll_down(&mut self) {
        if let Some(d) = self.detail.as_mut() {
            let max = d.lines.len().saturating_sub(1) as u16;
            if d.scroll < max {
                d.scroll += 1;
            }
        }
    }

    fn detail_scroll_up(&mut self) {
        if let Some(d) = self.detail.as_mut() {
            d.scroll = d.scroll.saturating_sub(1);
        }
    }

    fn detail_scroll_first(&mut self) {
        if let Some(d) = self.detail.as_mut() {
            d.scroll = 0;
        }
    }

    fn detail_scroll_last(&mut self) {
        if let Some(d) = self.detail.as_mut() {
            d.scroll = d.lines.len().saturating_sub(1) as u16;
        }
    }

    fn is_expanded(&self, prefix: &str) -> bool {
        !self.collapsed.contains(prefix)
    }

    /// Capture a stable key for the current selection so it can be
    /// re-located after a `build_rows` rebuild.
    fn cursor_key(&self) -> Option<CursorKey> {
        let row = self.state.selected().and_then(|i| self.rows.get(i))?;
        Some(match row {
            VisibleRow::Header { prefix } => CursorKey::Header(prefix.clone()),
            VisibleRow::Bead { prefix, bead_id, .. } => CursorKey::Bead {
                prefix: prefix.clone(),
                bead_id: bead_id.clone(),
            },
            VisibleRow::Closure { prefix, bead_id, .. } => CursorKey::Closure {
                prefix: prefix.clone(),
                bead_id: bead_id.clone(),
            },
        })
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

    /// True if the prefix at the cursor has any beads or closures.
    /// Folding empty groups is a visual no-op, so skip the work.
    fn cursor_has_children(&self) -> bool {
        let Some(prefix) = self.current_prefix() else {
            return false;
        };
        self.groups
            .iter()
            .find(|g| g.prefix == prefix)
            .is_some_and(group_has_children)
    }

    /// `h` / Left semantics:
    /// - on a child row → jump cursor to that prefix's header (don't collapse)
    /// - on an expanded header with children → collapse it (cursor stays)
    /// - on a collapsed or empty header → no-op
    fn collapse_or_jump(&mut self) {
        let Some(prefix) = self.current_prefix().map(str::to_string) else {
            return;
        };
        if !self.on_header() {
            self.jump_to_header(&prefix);
            return;
        }
        if !self.cursor_has_children() {
            return;
        }
        self.collapsed.insert(prefix.clone());
        self.rebuild_rows_preserving(CursorKey::Header(prefix));
        self.save_collapsed();
    }

    /// `l` / Right semantics: expand current prefix's header (no-op
    /// for empty groups).
    fn expand(&mut self) {
        let Some(prefix) = self.current_prefix().map(str::to_string) else {
            return;
        };
        if !self.cursor_has_children() {
            return;
        }
        if self.collapsed.remove(&prefix) {
            self.rebuild_rows_preserving(CursorKey::Header(prefix));
            self.save_collapsed();
        }
    }

    /// Space toggles the prefix the cursor is currently within
    /// (no-op for empty groups).
    fn toggle(&mut self) {
        let Some(prefix) = self.current_prefix().map(str::to_string) else {
            return;
        };
        if !self.cursor_has_children() {
            return;
        }
        if self.collapsed.contains(&prefix) {
            self.collapsed.remove(&prefix);
        } else {
            self.collapsed.insert(prefix.clone());
        }
        self.rebuild_rows_preserving(CursorKey::Header(prefix));
        self.save_collapsed();
    }

    /// Replace `rows` from current state. The caller supplies the
    /// `CursorKey` they want re-anchored — usually the header of the
    /// prefix whose fold-state just changed.
    fn rebuild_rows_preserving(&mut self, target: CursorKey) {
        self.rows = build_rows(&self.groups, &self.collapsed);
        self.select_by_key(Some(target));
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

    /// Replace `groups` + `pending_asks` from a fresh snapshot.
    /// Preserves fold-state (via `self.collapsed`, untouched except
    /// for prefixes that disappeared), sort mode, and cursor row
    /// (via CursorKey lookup).
    fn apply_snapshot(&mut self, mut groups: Vec<OwnedGroup>, pending_asks: usize) {
        let key = self.cursor_key();

        // Drop collapsed entries for prefixes that no longer exist —
        // but never *add* prefixes here. New prefixes are implicitly
        // expanded because absence from `collapsed` means "shown".
        let live: HashSet<String> = groups.iter().map(|g| g.prefix.clone()).collect();
        self.collapsed.retain(|p| live.contains(p));

        sort_groups(&mut groups, self.sort);
        self.groups = groups;
        self.pending_asks = pending_asks;
        self.rows = build_rows(&self.groups, &self.collapsed);
        self.select_by_key(key);
    }

    /// Re-select the row matching `key`. Falls back to: same prefix's
    /// header if the specific bead/closure is gone; row 0 if nothing
    /// matches at all.
    fn select_by_key(&mut self, key: Option<CursorKey>) {
        if self.rows.is_empty() {
            self.state.select(None);
            return;
        }
        let target = key
            .as_ref()
            .and_then(|k| self.find_row(k))
            .or_else(|| {
                key.as_ref().and_then(|k| {
                    let p = k.prefix();
                    self.rows.iter().position(
                        |r| matches!(r, VisibleRow::Header { prefix } if prefix == p),
                    )
                })
            })
            .unwrap_or(0);
        self.state.select(Some(target.min(self.rows.len() - 1)));
    }

    /// Locate a row matching this CursorKey in the freshly-built rows.
    fn find_row(&self, key: &CursorKey) -> Option<usize> {
        self.rows.iter().position(|r| match (key, r) {
            (CursorKey::Header(p), VisibleRow::Header { prefix }) => prefix == p,
            (
                CursorKey::Bead { prefix, bead_id },
                VisibleRow::Bead { prefix: rp, bead_id: rb, .. },
            ) => rp == prefix && rb == bead_id,
            (
                CursorKey::Closure { prefix, bead_id },
                VisibleRow::Closure { prefix: rp, bead_id: rb, .. },
            ) => rp == prefix && rb == bead_id,
            _ => false,
        })
    }
}

/// Build the flattened visible-row vec from a snapshot. Children of
/// a prefix are only emitted when that prefix is NOT in `collapsed`.
fn build_rows(groups: &[OwnedGroup], collapsed: &HashSet<String>) -> Vec<VisibleRow> {
    let mut rows: Vec<VisibleRow> = Vec::new();
    for g in groups {
        rows.push(VisibleRow::Header {
            prefix: g.prefix.clone(),
        });
        if collapsed.contains(&g.prefix) {
            continue;
        }
        for b in &g.beads {
            rows.push(VisibleRow::Bead {
                prefix: g.prefix.clone(),
                bead_id: b.id.clone(),
                line: bead_line(b),
            });
        }
        for c in &g.recently_closed {
            rows.push(VisibleRow::Closure {
                prefix: g.prefix.clone(),
                bead_id: c.id.clone(),
                line: closure_line(c),
            });
        }
    }
    rows
}

/// Whether this group has any visible children at all. Empty groups
/// shouldn't display a fold chevron — there's nothing to toggle.
fn group_has_children(g: &OwnedGroup) -> bool {
    !g.beads.is_empty() || !g.recently_closed.is_empty()
}

fn header_line(group: &OwnedGroup, expanded: bool) -> Line<'static> {
    // Empty groups get a two-space pad instead of a chevron — there's
    // nothing to expand or collapse, and the missing arrow makes that
    // obvious at a glance.
    let chevron = if !group_has_children(group) {
        "  "
    } else if expanded {
        "▾ "
    } else {
        "▸ "
    };
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
        // Yellow for the lightning bolt — it's lightning, not growth.
        crate::cli::commands::dash::PresenceKind::Working => Style::default().fg(Color::Yellow),
        crate::cli::commands::dash::PresenceKind::Idle => Style::default().add_modifier(Modifier::DIM),
        crate::cli::commands::dash::PresenceKind::Offline => Style::default().add_modifier(Modifier::DIM),
    }
}

fn status_glyph_style(kind: StatusKind) -> Style {
    match kind {
        StatusKind::InProgress => Style::default().fg(Color::Green),
        // Ready stays uncolored — the agent-presence "offline" glyph
        // also uses ○ (dim), and tinting ready's circle blue made
        // the two look distinct in a way that didn't match meaning.
        // A plain ○ for "available work" reads cleanly against the
        // dim ○ for "no presence record."
        StatusKind::Ready => Style::default(),
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
    let assignee = b
        .assignee
        .as_deref()
        .map(|a| format!(" [{a}]"))
        .unwrap_or_default();

    let dim = Style::default().add_modifier(Modifier::DIM);
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::raw("  "));
    spans.push(Span::styled(glyph.to_string(), status_glyph_style(b.kind)));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(b.id.clone(), Style::default().fg(Color::Cyan)));
    spans.push(Span::raw("  "));
    spans.push(Span::styled(pri, Style::default().fg(Color::Yellow)));
    spans.push(Span::raw("  "));
    spans.push(Span::raw(b.title.clone()));
    if !parent.is_empty() {
        spans.push(Span::styled(parent, dim));
    }
    if !sender.is_empty() {
        spans.push(Span::styled(sender, dim));
    }
    if !assignee.is_empty() {
        spans.push(Span::styled(assignee, dim));
    }
    Line::from(spans)
}

fn closure_line(c: &crate::cli::commands::dash::OwnedClosure) -> Line<'static> {
    // Mirror bead_line's column layout so the eye can scan a single
    // column for either "P?" (open) or "age" (closed) without having
    // to hunt past the title.
    let sender = c
        .sender
        .as_deref()
        .map(|s| format!(" (from: {s})"))
        .unwrap_or_default();
    let assignee = c
        .assignee
        .as_deref()
        .map(|a| format!(" [{a}]"))
        .unwrap_or_default();
    let dim = Style::default().add_modifier(Modifier::DIM);

    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::raw("  "));
    spans.push(Span::styled("✓".to_string(), dim));
    spans.push(Span::raw(" "));
    // Keep the id cyan-ish but dimmed so it still parses as an id
    // without competing visually with live beads above.
    spans.push(Span::styled(
        c.id.clone(),
        Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
    ));
    spans.push(Span::raw("  "));
    spans.push(Span::styled(format_age_compact(c.age_secs), dim));
    spans.push(Span::raw("  "));
    spans.push(Span::styled(c.title.clone(), dim));
    if !sender.is_empty() {
        spans.push(Span::styled(sender, dim));
    }
    if !assignee.is_empty() {
        spans.push(Span::styled(assignee, dim));
    }
    Line::from(spans)
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
    let mut app = App::new(initial.0, initial.1, beads_dir.clone(), cli.clone());
    // Prime the graph view so switching to it doesn't show empty
    // contents until the first refresh tick.
    app.refresh_graph();

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
                    } else if app.detail.is_some() {
                        // Detail pane keys. q / Backspace go back to
                        // dashboard; Esc and Ctrl-C exit entirely.
                        match key.code {
                            KeyCode::Esc => return Ok(()),
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                return Ok(());
                            }
                            KeyCode::Char('q') | KeyCode::Backspace => app.close_detail(),
                            KeyCode::Char('j') | KeyCode::Down => app.detail_scroll_down(),
                            KeyCode::Char('k') | KeyCode::Up => app.detail_scroll_up(),
                            KeyCode::Char('g') | KeyCode::Home => app.detail_scroll_first(),
                            KeyCode::Char('G') | KeyCode::End => app.detail_scroll_last(),
                            KeyCode::Char('r') => app.refresh_detail(),
                            KeyCode::Char('?') => app.show_help = true,
                            _ => {}
                        }
                    } else {
                        // Keys common to all views.
                        match key.code {
                            KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                                return Ok(());
                            }
                            KeyCode::Char('?') => app.show_help = true,
                            KeyCode::Tab => app.cycle_view_forward(),
                            KeyCode::BackTab => app.cycle_view_back(),
                            KeyCode::Char('1') => app.set_view(ViewMode::Dashboard),
                            KeyCode::Char('2') => app.set_view(ViewMode::Graph),
                            KeyCode::Char('r') => {
                                refresh(&mut app, args, cli, &beads_dir);
                                next_refresh = Instant::now() + refresh_every;
                            }
                            // View-specific bindings.
                            _ => match app.view {
                                ViewMode::Dashboard => match key.code {
                                    KeyCode::Char('j') | KeyCode::Down => app.select_next(),
                                    KeyCode::Char('k') | KeyCode::Up => app.select_prev(),
                                    KeyCode::Char('h') | KeyCode::Left => app.collapse_or_jump(),
                                    KeyCode::Char('l') | KeyCode::Right => app.expand(),
                                    KeyCode::Char(' ') => app.toggle(),
                                    KeyCode::Char('g') | KeyCode::Home => app.select_first(),
                                    KeyCode::Char('G') | KeyCode::End => app.select_last(),
                                    KeyCode::Char('s') => app.cycle_sort(),
                                    KeyCode::Enter => app.open_detail(),
                                    _ => {}
                                },
                                ViewMode::Graph => match key.code {
                                    KeyCode::Char('j') | KeyCode::Down => app.graph_scroll_down(),
                                    KeyCode::Char('k') | KeyCode::Up => app.graph_scroll_up(),
                                    KeyCode::Char('g') | KeyCode::Home => app.graph_scroll_first(),
                                    KeyCode::Char('G') | KeyCode::End => app.graph_scroll_last(),
                                    _ => {}
                                },
                            },
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
/// continues to render. Refreshes both the dashboard snapshot and
/// the graph rendering on every tick so view switches feel snappy.
fn refresh(app: &mut App, args: &DashArgs, cli: &config::CliOverrides, beads_dir: &Path) {
    if let Ok((groups, pending)) = snapshot(args, cli, beads_dir, Utc::now()) {
        app.apply_snapshot(groups, pending);
    }
    app.refresh_graph();
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

    // Top status line — shared across views.
    let status: Paragraph<'_> = if app.pending_asks > 0 {
        let noun = if app.pending_asks == 1 { "ask" } else { "asks" };
        Paragraph::new(format!(
            "operator: {} {} pending — run `bd admin operator attend`",
            app.pending_asks, noun
        ))
        .style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD))
    } else {
        Paragraph::new(view_tab_strip(app.view))
            .style(Style::default().add_modifier(Modifier::DIM))
    };
    frame.render_widget(status, chunks[0]);

    if app.detail.is_some() {
        draw_detail(frame, chunks[1], app);
    } else {
        match app.view {
            ViewMode::Dashboard => draw_dashboard(frame, chunks[1], app),
            ViewMode::Graph => draw_graph(frame, chunks[1], app),
        }
    }

    // Footer help.
    let footer_text = if app.detail.is_some() {
        "j/k scroll  g/G top/bot  r refresh  q back  Esc exit"
    } else {
        footer_help(app.view)
    };
    let footer = Paragraph::new(footer_text)
        .style(Style::default().add_modifier(Modifier::DIM));
    frame.render_widget(footer, chunks[2]);

    if app.show_help {
        draw_help_overlay(frame, frame.area());
    }
}

fn view_tab_strip(active: ViewMode) -> String {
    // Tiny tab-strip so the user always sees which view is active.
    let mut s = String::new();
    for v in [ViewMode::Dashboard, ViewMode::Graph] {
        if !s.is_empty() {
            s.push_str("  ");
        }
        if v == active {
            s.push_str(&format!("[{}]", v.label()));
        } else {
            s.push_str(&format!(" {} ", v.label()));
        }
    }
    s
}

fn footer_help(view: ViewMode) -> &'static str {
    match view {
        ViewMode::Dashboard => {
            "j/k nav  h/l fold  ↵ detail  s sort  Tab view  r refresh  ? help  q quit"
        }
        ViewMode::Graph => "j/k scroll  g/G first/last  Tab view  r refresh  ? help  q quit",
    }
}

fn draw_dashboard(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let items: Vec<ListItem<'_>> = app
        .rows
        .iter()
        .map(|row| match row {
            VisibleRow::Header { prefix } => {
                let group = app.groups.iter().find(|g| g.prefix == *prefix);
                let expanded = app.is_expanded(prefix);
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

    let title = format!("dashboard — sort: {}", app.sort.label());
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));
    frame.render_stateful_widget(list, area, &mut app.state);
}

fn draw_detail(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let Some(d) = app.detail.as_ref() else {
        return;
    };
    let para = Paragraph::new(d.lines.clone())
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(d.title_for_pane.clone()),
        )
        .wrap(Wrap { trim: false })
        .scroll((d.scroll, 0));
    frame.render_widget(para, area);
}

fn draw_graph(frame: &mut Frame<'_>, area: Rect, app: &mut App) {
    let lines: Vec<Line<'static>> = if app.graph_lines.is_empty() {
        vec![Line::from(Span::styled(
            "(no graph data yet — refreshing)",
            Style::default().add_modifier(Modifier::DIM),
        ))]
    } else {
        app.graph_lines.clone()
    };

    let title = format!("graph — {} lines", app.graph_lines.len());
    let para = Paragraph::new(lines)
        .block(Block::default().borders(Borders::ALL).title(title))
        .wrap(Wrap { trim: false })
        .scroll((app.graph_scroll, 0));
    frame.render_widget(para, area);
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
        Line::raw("g / Home     first row / top"),
        Line::raw("G / End      last row / bottom"),
        Line::raw("s            cycle sort: name → activity (dashboard view)"),
        Line::raw("Enter        open detail pane for the selected bead"),
        Line::raw("              (q / Backspace closes; Esc exits entirely)"),
        Line::raw("Tab          next view (dashboard ⇄ graph)"),
        Line::raw("Shift-Tab    previous view"),
        Line::raw("1 / 2        jump to dashboard / graph view"),
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

/// Open the issue details for `bead_id` and prerender them into the
/// styled Lines that the detail pane displays. None on any fetch
/// failure or unknown id.
fn load_detail(
    beads_dir: &Path,
    cli: &config::CliOverrides,
    bead_id: &str,
) -> Option<DetailState> {
    let (storage, _paths) =
        config::open_storage(beads_dir, cli.db.as_ref(), cli.lock_timeout).ok()?;
    let details = storage.get_issue_details(bead_id, false, false, 0).ok()??;
    let title_for_pane = format!("detail: {bead_id}");
    let lines = render_issue_detail(&details);
    Some(DetailState {
        bead_id: bead_id.to_string(),
        title_for_pane,
        lines,
        scroll: 0,
    })
}

fn render_issue_detail(
    details: &crate::format::IssueDetails,
) -> Vec<Line<'static>> {
    let issue = &details.issue;
    let cyan = Style::default().fg(Color::Cyan);
    let yellow = Style::default().fg(Color::Yellow);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let dim = Style::default().add_modifier(Modifier::DIM);

    let mut out: Vec<Line<'static>> = Vec::new();

    // Header row: <id>  ·  <status>  ·  <P?>  ·  <type>
    let status_text = issue.status.as_str().to_uppercase();
    let status_style = match issue.status.as_str() {
        "in_progress" => Style::default().fg(Color::Green),
        "blocked" => Style::default().fg(Color::Red),
        "deferred" | "closed" | "tombstone" => dim,
        _ => Style::default(),
    };
    out.push(Line::from(vec![
        Span::styled(issue.id.clone(), cyan.add_modifier(Modifier::BOLD)),
        Span::raw("  ·  "),
        Span::styled(status_text, status_style.add_modifier(Modifier::BOLD)),
        Span::raw("  ·  "),
        Span::styled(format!("P{}", issue.priority.0), yellow),
        Span::raw("  ·  "),
        Span::raw(issue.issue_type.as_str().to_string()),
    ]));

    // Title row.
    out.push(Line::from(Span::styled(issue.title.clone(), bold)));
    out.push(Line::raw(""));

    // Owner / assignee.
    if let Some(owner) = issue.owner.as_deref().filter(|s| !s.is_empty()) {
        out.push(label_value("Owner", owner));
    }
    if let Some(assignee) = issue.assignee.as_deref().filter(|s| !s.is_empty()) {
        out.push(label_value("Assignee", assignee));
    }

    // Dates.
    out.push(label_value(
        "Created",
        &issue.created_at.format("%Y-%m-%d %H:%M UTC").to_string(),
    ));
    out.push(label_value(
        "Updated",
        &issue.updated_at.format("%Y-%m-%d %H:%M UTC").to_string(),
    ));
    if let Some(closed) = issue.closed_at.as_ref() {
        let reason = issue.close_reason.as_deref().unwrap_or("closed");
        out.push(label_value(
            "Closed",
            &format!("{} ({})", closed.format("%Y-%m-%d %H:%M UTC"), reason),
        ));
    }
    if let Some(due) = issue.due_at.as_ref() {
        out.push(label_value("Due", &due.format("%Y-%m-%d").to_string()));
    }
    if let Some(defer) = issue.defer_until.as_ref() {
        out.push(label_value(
            "Deferred until",
            &defer.format("%Y-%m-%d").to_string(),
        ));
    }

    // Estimate.
    if let Some(min) = issue.estimated_minutes
        && min > 0
    {
        let h = min / 60;
        let r = min % 60;
        let txt = if h > 0 && r > 0 {
            format!("{h}h {r}m")
        } else if h > 0 {
            format!("{h}h")
        } else {
            format!("{r}m")
        };
        out.push(label_value("Estimate", &txt));
    }

    // External ref / source / sender.
    if let Some(s) = issue.external_ref.as_deref().filter(|s| !s.is_empty()) {
        out.push(label_value("External ref", s));
    }
    if let Some(s) = issue.sender.as_deref().filter(|s| !s.is_empty()) {
        out.push(label_value("Sender", s));
    }
    if let Some(p) = details.parent.as_deref().filter(|s| !s.is_empty()) {
        out.push(Line::from(vec![
            Span::styled("Parent: ", dim),
            Span::styled(p.to_string(), cyan),
        ]));
    }

    // Free-form sections.
    if let Some(text) = issue.description.as_deref().filter(|s| !s.trim().is_empty()) {
        out.push(Line::raw(""));
        out.push(Line::from(Span::styled("Description", bold)));
        push_wrapped(&mut out, text);
    }
    if let Some(text) = issue.design.as_deref().filter(|s| !s.trim().is_empty()) {
        out.push(Line::raw(""));
        out.push(Line::from(Span::styled("Design", bold)));
        push_wrapped(&mut out, text);
    }
    if let Some(text) = issue
        .acceptance_criteria
        .as_deref()
        .filter(|s| !s.trim().is_empty())
    {
        out.push(Line::raw(""));
        out.push(Line::from(Span::styled("Acceptance Criteria", bold)));
        push_wrapped(&mut out, text);
    }
    if let Some(text) = issue.notes.as_deref().filter(|s| !s.trim().is_empty()) {
        out.push(Line::raw(""));
        out.push(Line::from(Span::styled("Notes", bold)));
        push_wrapped(&mut out, text);
    }

    // Dependencies / dependents.
    if !details.dependencies.is_empty() {
        out.push(Line::raw(""));
        out.push(Line::from(Span::styled("Dependencies", bold)));
        for dep in &details.dependencies {
            out.push(Line::from(vec![
                Span::raw("  → "),
                Span::styled(dep.id.clone(), cyan),
                Span::styled(format!(" ({}) ", dep.dep_type), dim),
                Span::raw(dep.title.clone()),
            ]));
        }
    }
    if !details.dependents.is_empty() {
        out.push(Line::raw(""));
        out.push(Line::from(Span::styled("Dependents", bold)));
        for dep in &details.dependents {
            out.push(Line::from(vec![
                Span::raw("  ← "),
                Span::styled(dep.id.clone(), cyan),
                Span::styled(format!(" ({}) ", dep.dep_type), dim),
                Span::raw(dep.title.clone()),
            ]));
        }
    }

    out
}

/// `Label: value` line — label dim, value default.
fn label_value(label: &str, value: &str) -> Line<'static> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    Line::from(vec![
        Span::styled(format!("{label}: "), dim),
        Span::raw(value.to_string()),
    ])
}

/// Append free-form text as plain Lines, preserving the user's
/// original line breaks. Paragraph::wrap handles any further wrapping
/// at render time when the terminal is narrower than the content.
fn push_wrapped(out: &mut Vec<Line<'static>>, text: &str) {
    for line in text.lines() {
        out.push(Line::raw(line.to_string()));
    }
}

/// Pull the persisted collapsed-prefix set out of the config table.
/// Treats every failure mode (missing row, bad JSON, DB lock) as
/// "no persisted state" and returns an empty set so the TUI just
/// starts fully-expanded.
fn load_collapsed(beads_dir: &Path, cli: &config::CliOverrides) -> Option<HashSet<String>> {
    let (storage, _paths) =
        config::open_storage(beads_dir, cli.db.as_ref(), cli.lock_timeout).ok()?;
    let raw = storage.get_config(COLLAPSED_KEY).ok().flatten()?;
    let list: Vec<String> = serde_json::from_str(&raw).ok()?;
    Some(list.into_iter().collect())
}

/// Pull the persisted sort mode out of the config table; default
/// (SortMode::Name) is returned via the caller's unwrap_or.
fn load_sort(beads_dir: &Path, cli: &config::CliOverrides) -> Option<SortMode> {
    let (storage, _paths) =
        config::open_storage(beads_dir, cli.db.as_ref(), cli.lock_timeout).ok()?;
    let raw = storage.get_config(SORT_KEY).ok().flatten()?;
    Some(SortMode::from_label(&raw))
}

/// Same pattern for view mode.
fn load_view(beads_dir: &Path, cli: &config::CliOverrides) -> Option<ViewMode> {
    let (storage, _paths) =
        config::open_storage(beads_dir, cli.db.as_ref(), cli.lock_timeout).ok()?;
    let raw = storage.get_config(VIEW_KEY).ok().flatten()?;
    Some(ViewMode::from_label(&raw))
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
