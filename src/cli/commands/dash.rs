//! `bd dash` — grouped, refreshing situational-awareness view.
//!
//! Renders beads clustered by ID prefix with workable/blocked/in-progress
//! distinctions. Optionally redraws every N seconds for a `top`-style view.

use crate::cli::{DashArgs, OutputFormat, resolve_output_format_basic};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::model::{Issue, Priority, Status};
use crate::output::OutputContext;
use crate::storage::presence::{PresenceRow, PresenceState};
use crate::storage::{ListFilters, SqliteStorage};
use crate::util::id::split_prefix_remainder;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::thread;
use std::time::Duration;

const CLEAR_SCREEN: &str = "\x1b[2J\x1b[H";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
enum StatusKind {
    InProgress,
    Ready,
    Blocked,
    Deferred,
    Closed,
}

impl StatusKind {
    fn glyph(self) -> &'static str {
        match self {
            Self::InProgress => "▶",
            Self::Ready => "○",
            Self::Blocked => "⚠",
            Self::Deferred => "❄",
            Self::Closed => "✓",
        }
    }
}

#[derive(Serialize)]
struct DashBead<'a> {
    id: &'a str,
    kind: StatusKind,
    priority: i32,
    title: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    assignee: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sender: Option<&'a str>,
}

#[derive(Serialize)]
struct DashGroup<'a> {
    prefix: &'a str,
    in_progress: usize,
    ready: usize,
    blocked: usize,
    deferred: usize,
    closed: usize,
    closed_recently: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    presence: Option<PresenceJson<'a>>,
    beads: Vec<DashBead<'a>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    recently_closed: Vec<RecentClosure<'a>>,
}

#[derive(Serialize)]
struct PresenceJson<'a> {
    state: &'a str,
    age_secs: i64,
}

#[derive(Serialize)]
struct RecentClosure<'a> {
    id: &'a str,
    title: &'a str,
    closed_at: String,
    age_secs: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    assignee: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sender: Option<&'a str>,
}

#[derive(Serialize)]
struct DashOutput<'a> {
    ts: String,
    groups: Vec<DashGroup<'a>>,
}

/// Execute the dash command.
///
/// # Errors
///
/// Returns an error if storage open or queries fail.
pub fn execute(args: &DashArgs, cli: &config::CliOverrides, ctx: &OutputContext) -> Result<()> {
    let format = resolve_output_format_basic(args.format, ctx.is_json(), false);
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;

    match args.refresh {
        Some(secs) if secs > 0 && std::io::stdout().is_terminal() => {
            refresh_loop(args, cli, &beads_dir, format, Duration::from_secs(secs))?;
        }
        _ => {
            render_once(args, cli, &beads_dir, format)?;
        }
    }
    Ok(())
}

fn refresh_loop(
    args: &DashArgs,
    cli: &config::CliOverrides,
    beads_dir: &Path,
    format: OutputFormat,
    interval: Duration,
) -> Result<()> {
    loop {
        // Clear screen and home cursor.
        {
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            let _ = write!(out, "{CLEAR_SCREEN}");
            let _ = out.flush();
        }
        render_once(args, cli, beads_dir, format)?;
        thread::sleep(interval);
    }
}

fn render_once(
    args: &DashArgs,
    cli: &config::CliOverrides,
    beads_dir: &Path,
    format: OutputFormat,
) -> Result<()> {
    let (storage, _paths) = config::open_storage(beads_dir, cli.db.as_ref(), cli.lock_timeout)?;

    // Parse the recent-closures window. Empty / "0" disables.
    let window = parse_closed_within(&args.closed_within)?;
    let presence_ttl = parse_closed_within(&args.presence_ttl)?;

    // We need closed beads in the fetch when either:
    //  - the user asked for them via --show-closed (current behavior), or
    //  - the recently-closed footer is enabled (window > 0).
    let want_closed = args.show_closed || window.is_some();

    let filters = ListFilters {
        include_closed: want_closed,
        include_deferred: args.show_deferred,
        ..Default::default()
    };
    let issues = storage.list_issues(&filters)?;
    let blocked_ids = storage.get_blocked_ids()?;
    let parents = fetch_parent_map(&storage)?;

    // Pull presence as a per-prefix lookup. Empty when ttl is None.
    let presence: HashMap<String, PresenceRow> = if presence_ttl.is_some() {
        storage
            .all_presence()?
            .into_iter()
            .map(|p| (p.prefix.clone(), p))
            .collect()
    } else {
        HashMap::new()
    };

    let now = Utc::now();
    let groups = build_groups(
        &issues,
        &blocked_ids,
        &parents,
        args,
        now,
        window,
        &presence,
        presence_ttl,
    );

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    match format {
        OutputFormat::Json | OutputFormat::Toon => render_json(&mut out, &groups, now)?,
        _ => render_text(&mut out, &groups, terminal_width(), &args.closed_within, now)?,
    }
    out.flush().ok();
    Ok(())
}

/// Parse a `--closed-within` duration string. `""`, `"0"`, `"0s"` etc. yield None.
fn parse_closed_within(raw: &str) -> Result<Option<chrono::Duration>> {
    let s = raw.trim();
    if s.is_empty() || s == "0" {
        return Ok(None);
    }
    // Split numeric prefix from unit suffix.
    let (num_part, unit) = s
        .find(|c: char| !c.is_ascii_digit())
        .map_or((s, ""), |i| (&s[..i], &s[i..]));
    let n: i64 = num_part.parse().map_err(|_| {
        BeadsError::validation(
            "closed-within",
            format!("not a valid duration: '{raw}' (expected e.g. 5m, 2h, 3d, 1w)"),
        )
    })?;
    if n == 0 {
        return Ok(None);
    }
    let dur = match unit {
        "" | "s" | "sec" | "secs" => chrono::Duration::seconds(n),
        "m" | "min" | "mins" => chrono::Duration::minutes(n),
        "h" | "hr" | "hrs" => chrono::Duration::hours(n),
        "d" | "day" | "days" => chrono::Duration::days(n),
        "w" | "wk" | "wks" | "week" | "weeks" => chrono::Duration::weeks(n),
        other => {
            return Err(BeadsError::validation(
                "closed-within",
                format!("unknown duration unit '{other}' (use s/m/h/d/w)"),
            ));
        }
    };
    Ok(Some(dur))
}

/// Render a relative age without an "ago" suffix — for the in-bracket
/// presence badge where the surrounding context implies "now."
fn format_age_compact(secs: i64) -> String {
    if secs < 0 {
        return "now".to_string();
    }
    if secs < 60 {
        return format!("{secs}s");
    }
    let m = secs / 60;
    if m < 60 {
        return format!("{m}m");
    }
    let h = m / 60;
    if h < 24 {
        return format!("{h}h");
    }
    let d = h / 24;
    if d < 7 {
        return format!("{d}d");
    }
    let w = d / 7;
    format!("{w}w")
}

/// Render a relative age as a terse string.
fn format_age(secs: i64) -> String {
    if secs < 0 {
        return "now".to_string();
    }
    if secs < 5 {
        return "just now".to_string();
    }
    if secs < 60 {
        return format!("{secs}s ago");
    }
    let m = secs / 60;
    if m < 60 {
        return format!("{m}m ago");
    }
    let h = m / 60;
    if h < 24 {
        return format!("{h}h ago");
    }
    let d = h / 24;
    if d < 7 {
        return format!("{d}d ago");
    }
    let w = d / 7;
    format!("{w}w ago")
}

fn fetch_parent_map(storage: &SqliteStorage) -> Result<HashMap<String, String>> {
    let issues = storage.list_issues(&ListFilters {
        include_closed: true,
        include_deferred: true,
        ..Default::default()
    })?;
    // For each issue, look up its dependencies and find the parent-child one.
    // For efficiency we'd love a single bulk query, but the existing storage API
    // doesn't expose typed deps cheaply; fall back to per-bead lookup using
    // get_dependencies_full which is a cached statement under the hood.
    let mut map = HashMap::new();
    for issue in issues {
        let deps = storage.get_dependencies_full(&issue.id)?;
        for dep in deps {
            if matches!(dep.dep_type, crate::model::DependencyType::ParentChild) {
                map.insert(issue.id.clone(), dep.depends_on_id);
                break;
            }
        }
    }
    Ok(map)
}

#[allow(clippy::too_many_arguments)]
fn build_groups<'a>(
    issues: &'a [Issue],
    blocked_ids: &HashSet<String>,
    parents: &'a HashMap<String, String>,
    args: &DashArgs,
    now: DateTime<Utc>,
    window: Option<chrono::Duration>,
    presence: &HashMap<String, PresenceRow>,
    presence_ttl: Option<chrono::Duration>,
) -> Vec<OwnedGroup> {
    let mut by_prefix: BTreeMap<String, Vec<&Issue>> = BTreeMap::new();
    for issue in issues {
        let prefix = match split_prefix_remainder(&issue.id) {
            Some((p, _)) => p.to_string(),
            None => "(no-prefix)".to_string(),
        };
        if let Some(filter) = &args.prefix {
            if filter != &prefix {
                continue;
            }
        }
        by_prefix.entry(prefix).or_default().push(issue);
    }

    let cutoff = window.map(|w| now - w);
    let limit = if args.closed_limit == 0 { usize::MAX } else { args.closed_limit };

    let mut groups = Vec::with_capacity(by_prefix.len());
    for (prefix, beads) in by_prefix {
        // Split into live (non-closed) and closed within window.
        let (live, closed_pool): (Vec<&Issue>, Vec<&Issue>) = beads
            .into_iter()
            .partition(|i| !matches!(i.status, Status::Closed | Status::Tombstone));

        // Recently-closed pool, sorted newest-first, truncated to limit.
        let mut recent: Vec<OwnedClosure> = closed_pool
            .into_iter()
            .filter_map(|i| {
                let closed_at = i.closed_at?;
                if let Some(c) = cutoff {
                    if closed_at < c {
                        return None;
                    }
                }
                let age = (now - closed_at).num_seconds().max(0);
                Some(OwnedClosure {
                    id: i.id.clone(),
                    title: i.title.clone(),
                    closed_at,
                    age_secs: age,
                    assignee: i.assignee.clone(),
                    sender: i.sender.clone(),
                })
            })
            .collect();
        recent.sort_by_key(|c| std::cmp::Reverse(c.closed_at));
        if recent.len() > limit {
            recent.truncate(limit);
        }

        // Sort live beads.
        let mut live = live;
        live.sort_by(|a, b| {
            let ka = kind_of(a, blocked_ids);
            let kb = kind_of(b, blocked_ids);
            ka.cmp(&kb)
                .then(a.priority.0.cmp(&b.priority.0))
                .then(a.created_at.cmp(&b.created_at))
        });

        let mut in_progress = 0usize;
        let mut ready = 0usize;
        let mut blocked = 0usize;
        let mut deferred = 0usize;
        let mut closed = 0usize;

        let rows: Vec<OwnedBead> = live
            .iter()
            .map(|i| {
                let kind = kind_of(i, blocked_ids);
                match kind {
                    StatusKind::InProgress => in_progress += 1,
                    StatusKind::Ready => ready += 1,
                    StatusKind::Blocked => blocked += 1,
                    StatusKind::Deferred => deferred += 1,
                    StatusKind::Closed => closed += 1,
                }
                OwnedBead {
                    id: i.id.clone(),
                    kind,
                    priority: i.priority,
                    title: i.title.clone(),
                    assignee: i.assignee.clone(),
                    parent: parents.get(&i.id).cloned(),
                    sender: i.sender.clone(),
                }
            })
            .collect();

        let presence_view = presence.get(&prefix).map(|p| {
            let age = (now - p.last_changed).num_seconds().max(0);
            let label = match (p.state, presence_ttl) {
                (_, None) => None, // shouldn't reach here since presence map is empty
                (PresenceState::Working, Some(ttl)) if age <= ttl.num_seconds() => Some(
                    PresenceView { state: PresenceKind::Working, age_secs: age },
                ),
                (PresenceState::Idle, Some(ttl)) if age <= ttl.num_seconds() => Some(
                    PresenceView { state: PresenceKind::Idle, age_secs: age },
                ),
                _ => Some(PresenceView {
                    state: PresenceKind::Offline,
                    age_secs: age,
                }),
            };
            label
        }).flatten();

        // Skip prefixes that have nothing to show at all.
        if rows.is_empty() && recent.is_empty() && presence_view.is_none() {
            continue;
        }

        groups.push(OwnedGroup {
            prefix,
            in_progress,
            ready,
            blocked,
            deferred,
            closed,
            closed_recently: recent.len(),
            presence: presence_view,
            beads: rows,
            recently_closed: recent,
        });
    }

    // Also surface prefixes that have ONLY presence (no beads or closures
    // in the current window). Iterate presence map for prefixes we haven't
    // emitted yet.
    if presence_ttl.is_some() {
        let emitted: HashSet<String> = groups.iter().map(|g| g.prefix.clone()).collect();
        for (prefix, row) in presence {
            if emitted.contains(prefix) {
                continue;
            }
            if let Some(filter) = &args.prefix {
                if filter != prefix {
                    continue;
                }
            }
            let age = (now - row.last_changed).num_seconds().max(0);
            let view = match (row.state, presence_ttl) {
                (PresenceState::Working, Some(ttl)) if age <= ttl.num_seconds() => PresenceView {
                    state: PresenceKind::Working,
                    age_secs: age,
                },
                (PresenceState::Idle, Some(ttl)) if age <= ttl.num_seconds() => PresenceView {
                    state: PresenceKind::Idle,
                    age_secs: age,
                },
                _ => PresenceView {
                    state: PresenceKind::Offline,
                    age_secs: age,
                },
            };
            // Skip offline-only prefixes — too noisy if they're stale forever.
            if matches!(view.state, PresenceKind::Offline) {
                continue;
            }
            groups.push(OwnedGroup {
                prefix: prefix.clone(),
                in_progress: 0,
                ready: 0,
                blocked: 0,
                deferred: 0,
                closed: 0,
                closed_recently: 0,
                presence: Some(view),
                beads: vec![],
                recently_closed: vec![],
            });
        }
        groups.sort_by(|a, b| a.prefix.cmp(&b.prefix));
    }

    groups
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PresenceKind {
    Working,
    Idle,
    Offline,
}

impl PresenceKind {
    fn glyph(self) -> &'static str {
        match self {
            Self::Working => "⚡",
            Self::Idle => "⏸",
            Self::Offline => "○",
        }
    }
    fn label(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Idle => "idle",
            Self::Offline => "offline",
        }
    }
}

#[derive(Debug, Clone)]
struct PresenceView {
    state: PresenceKind,
    age_secs: i64,
}

struct OwnedBead {
    id: String,
    kind: StatusKind,
    priority: Priority,
    title: String,
    assignee: Option<String>,
    parent: Option<String>,
    sender: Option<String>,
}

struct OwnedGroup {
    prefix: String,
    in_progress: usize,
    ready: usize,
    blocked: usize,
    deferred: usize,
    closed: usize,
    closed_recently: usize,
    presence: Option<PresenceView>,
    beads: Vec<OwnedBead>,
    recently_closed: Vec<OwnedClosure>,
}

struct OwnedClosure {
    id: String,
    title: String,
    closed_at: DateTime<Utc>,
    age_secs: i64,
    assignee: Option<String>,
    sender: Option<String>,
}

fn kind_of(issue: &Issue, blocked_ids: &HashSet<String>) -> StatusKind {
    match &issue.status {
        Status::InProgress => StatusKind::InProgress,
        Status::Closed | Status::Tombstone => StatusKind::Closed,
        Status::Deferred => StatusKind::Deferred,
        Status::Blocked => StatusKind::Blocked,
        Status::Open | Status::Pinned | Status::Custom(_) => {
            if blocked_ids.contains(&issue.id) {
                StatusKind::Blocked
            } else {
                StatusKind::Ready
            }
        }
    }
}

fn render_text<W: Write>(
    out: &mut W,
    groups: &[OwnedGroup],
    width: u16,
    _closed_within_label: &str,
    _now: DateTime<Utc>,
) -> Result<()> {
    if groups.is_empty() {
        writeln!(out, "(no beads)")?;
        return Ok(());
    }

    for (i, group) in groups.iter().enumerate() {
        if i > 0 {
            writeln!(out)?;
        }
        let header_label = match &group.presence {
            Some(p) => format!(
                "[{} {} {}]",
                p.state.glyph(),
                group.prefix,
                format_age_compact(p.age_secs)
            ),
            None => format!("[{}]", group.prefix),
        };
        let counts = header_counts(group);
        if counts.is_empty() {
            writeln!(out, "{header_label}")?;
        } else {
            writeln!(out, "{header_label} {counts}")?;
        }

        // Compute ID column width considering both live and recently-closed rows.
        let id_w = group
            .beads
            .iter()
            .map(|b| b.id.len())
            .chain(group.recently_closed.iter().map(|c| c.id.len()))
            .max()
            .unwrap_or(8);
        let pri_w = 3; // "P0".."P4"

        for bead in &group.beads {
            let glyph = bead.kind.glyph();
            let pri = format!("P{}", bead.priority.0);
            let row_prefix = format!(
                "  {glyph} {id:<id_w$}  {pri:>pri_w$}  ",
                id = bead.id,
                pri = pri,
                id_w = id_w,
                pri_w = pri_w,
            );

            // Suffixes that should never get truncated off the end.
            let assignee_suffix = bead
                .assignee
                .as_deref()
                .map(|a| format!(" [{a}]"))
                .unwrap_or_default();
            let parent_suffix = bead
                .parent
                .as_deref()
                .map(|p| format!(" ← {p}"))
                .unwrap_or_default();
            let sender_suffix = bead
                .sender
                .as_deref()
                .map(|s| format!(" (from: {s})"))
                .unwrap_or_default();
            let trailing = format!("{parent_suffix}{sender_suffix}{assignee_suffix}");

            let used = row_prefix.chars().count() + trailing.chars().count();
            let title_cap = (width as usize).saturating_sub(used).max(20);
            let title = truncate_with_ellipsis(&bead.title, title_cap);

            writeln!(out, "{row_prefix}{title}{trailing}")?;
        }

        if !group.recently_closed.is_empty() {
            if !group.beads.is_empty() {
                writeln!(out)?;
            }
            // Align the age column for compact reading.
            let age_w = group
                .recently_closed
                .iter()
                .map(|c| format_age(c.age_secs).len())
                .max()
                .unwrap_or(7);
            for c in &group.recently_closed {
                let age = format_age(c.age_secs);
                let assignee_suffix = c
                    .assignee
                    .as_deref()
                    .map(|a| format!(" [{a}]"))
                    .unwrap_or_default();
                let sender_suffix = c
                    .sender
                    .as_deref()
                    .map(|s| format!(" (from: {s})"))
                    .unwrap_or_default();
                let trailing = format!("{sender_suffix}{assignee_suffix}");
                let row_prefix = format!(
                    "  ✓ {id:<id_w$}  {age:>age_w$}  ",
                    id = c.id,
                    age = age,
                    id_w = id_w,
                    age_w = age_w,
                );
                let used = row_prefix.chars().count() + trailing.chars().count();
                let title_cap = (width as usize).saturating_sub(used).max(20);
                let title = truncate_with_ellipsis(&c.title, title_cap);
                writeln!(out, "{row_prefix}{title}{trailing}")?;
            }
        }
    }
    Ok(())
}

fn header_counts(g: &OwnedGroup) -> String {
    let mut parts = Vec::new();
    if g.ready > 0 {
        parts.push(format!("{} ready", g.ready));
    }
    if g.blocked > 0 {
        parts.push(format!("{} blocked", g.blocked));
    }
    if g.in_progress > 0 {
        parts.push(format!("{} in progress", g.in_progress));
    }
    if g.deferred > 0 {
        parts.push(format!("{} deferred", g.deferred));
    }
    if g.closed > 0 {
        parts.push(format!("{} closed", g.closed));
    }
    if g.closed_recently > 0 {
        parts.push(format!("{} closed recently", g.closed_recently));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("({})", parts.join(", "))
    }
}

fn truncate_with_ellipsis(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    if max_chars == 0 {
        return String::new();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn render_json<W: Write>(out: &mut W, groups: &[OwnedGroup], now: DateTime<Utc>) -> Result<()> {
    let view: Vec<DashGroup> = groups
        .iter()
        .map(|g| DashGroup {
            prefix: &g.prefix,
            in_progress: g.in_progress,
            ready: g.ready,
            blocked: g.blocked,
            deferred: g.deferred,
            closed: g.closed,
            closed_recently: g.closed_recently,
            presence: g.presence.as_ref().map(|p| PresenceJson {
                state: p.state.label(),
                age_secs: p.age_secs,
            }),
            beads: g
                .beads
                .iter()
                .map(|b| DashBead {
                    id: &b.id,
                    kind: b.kind,
                    priority: b.priority.0,
                    title: &b.title,
                    assignee: b.assignee.as_deref(),
                    parent: b.parent.as_deref(),
                    sender: b.sender.as_deref(),
                })
                .collect(),
            recently_closed: g
                .recently_closed
                .iter()
                .map(|c| RecentClosure {
                    id: &c.id,
                    title: &c.title,
                    closed_at: c.closed_at.to_rfc3339(),
                    age_secs: c.age_secs,
                    assignee: c.assignee.as_deref(),
                    sender: c.sender.as_deref(),
                })
                .collect(),
        })
        .collect();
    let payload = DashOutput {
        ts: now.to_rfc3339(),
        groups: view,
    };
    writeln!(out, "{}", serde_json::to_string(&payload)?)?;
    Ok(())
}

fn terminal_width() -> u16 {
    match crossterm::terminal::size() {
        Ok((cols, _)) if cols > 0 => cols,
        _ => 100,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn issue(id: &str, status: Status, priority: i32, title: &str) -> Issue {
        let mut i = Issue {
            id: id.to_string(),
            title: title.to_string(),
            description: None,
            status,
            priority: Priority(priority),
            issue_type: crate::model::IssueType::Task,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            assignee: None,
            owner: None,
            estimated_minutes: None,
            due_at: None,
            defer_until: None,
            external_ref: None,
            ephemeral: false,
            content_hash: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            created_by: None,
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            source_system: None,
            source_repo: None,
            deleted_at: None,
            deleted_by: None,
            delete_reason: None,
            original_type: None,
            compaction_level: None,
            compacted_at: None,
            compacted_at_commit: None,
            original_size: None,
            sender: None,
            pinned: false,
            is_template: false,
            labels: vec![],
            dependencies: vec![],
            comments: vec![],
        };
        if matches!(i.status, Status::Closed | Status::Tombstone) {
            i.closed_at = Some(Utc::now());
        }
        i
    }

    #[test]
    fn kind_of_classifies_known_states() {
        let blocked_ids: HashSet<String> = ["arc1-c".to_string()].into_iter().collect();
        let a = issue("arc1-a", Status::Open, 1, "ready one");
        let b = issue("arc1-b", Status::InProgress, 1, "running");
        let c = issue("arc1-c", Status::Open, 1, "blocked one");
        let d = issue("arc1-d", Status::Deferred, 1, "later");
        let e = issue("arc1-e", Status::Closed, 1, "done");
        assert_eq!(kind_of(&a, &blocked_ids), StatusKind::Ready);
        assert_eq!(kind_of(&b, &blocked_ids), StatusKind::InProgress);
        assert_eq!(kind_of(&c, &blocked_ids), StatusKind::Blocked);
        assert_eq!(kind_of(&d, &blocked_ids), StatusKind::Deferred);
        assert_eq!(kind_of(&e, &blocked_ids), StatusKind::Closed);
    }

    fn default_args() -> DashArgs {
        DashArgs {
            closed_within: "24h".to_string(),
            closed_limit: 5,
            ..Default::default()
        }
    }

    #[test]
    fn group_sort_orders_in_progress_first() {
        let issues = vec![
            issue("arc1-c", Status::Open, 2, "ready"),
            issue("arc1-a", Status::Open, 0, "blocked"),
            issue("arc1-b", Status::InProgress, 3, "running"),
        ];
        let blocked: HashSet<String> = ["arc1-a".to_string()].into_iter().collect();
        let parents = HashMap::new();
        let groups = build_groups(&issues, &blocked, &parents, &default_args(), Utc::now(), None, &HashMap::new(), None);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].beads[0].id, "arc1-b");
        assert_eq!(groups[0].beads[0].kind, StatusKind::InProgress);
        assert_eq!(groups[0].beads[1].id, "arc1-c");
        assert_eq!(groups[0].beads[1].kind, StatusKind::Ready);
        assert_eq!(groups[0].beads[2].id, "arc1-a");
        assert_eq!(groups[0].beads[2].kind, StatusKind::Blocked);
    }

    #[test]
    fn separate_prefixes_get_separate_groups() {
        let issues = vec![
            issue("arc1-a", Status::Open, 1, "arc1 one"),
            issue("arc2-x", Status::Open, 1, "arc2 one"),
            issue("arc1-b", Status::Open, 1, "arc1 two"),
        ];
        let blocked = HashSet::new();
        let parents = HashMap::new();
        let groups = build_groups(&issues, &blocked, &parents, &default_args(), Utc::now(), None, &HashMap::new(), None);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].prefix, "arc1");
        assert_eq!(groups[0].beads.len(), 2);
        assert_eq!(groups[1].prefix, "arc2");
        assert_eq!(groups[1].beads.len(), 1);
    }

    #[test]
    fn header_counts_drops_zero_categories() {
        let g = OwnedGroup {
            prefix: "arc1".into(),
            in_progress: 0,
            ready: 3,
            blocked: 0,
            deferred: 0,
            closed: 0,
            closed_recently: 0,
            presence: None,
            beads: vec![],
            recently_closed: vec![],
        };
        assert_eq!(header_counts(&g), "(3 ready)");

        let g2 = OwnedGroup {
            prefix: "arc1".into(),
            in_progress: 1,
            ready: 2,
            blocked: 1,
            deferred: 0,
            closed: 0,
            closed_recently: 0,
            presence: None,
            beads: vec![],
            recently_closed: vec![],
        };
        assert_eq!(header_counts(&g2), "(2 ready, 1 blocked, 1 in progress)");

        let g3 = OwnedGroup {
            prefix: "arc1".into(),
            in_progress: 0,
            ready: 1,
            blocked: 0,
            deferred: 0,
            closed: 0,
            closed_recently: 2,
            presence: None,
            beads: vec![],
            recently_closed: vec![],
        };
        assert_eq!(header_counts(&g3), "(1 ready, 2 closed recently)");
    }

    #[test]
    fn presence_within_ttl_renders_working_or_idle() {
        let now = Utc::now();
        let issues = vec![issue("arc1-a", Status::Open, 1, "live")];
        let blocked = HashSet::new();
        let parents = HashMap::new();
        let mut presence = HashMap::new();
        presence.insert(
            "arc1".to_string(),
            PresenceRow {
                prefix: "arc1".into(),
                state: PresenceState::Working,
                last_changed: now - chrono::Duration::seconds(30),
            },
        );
        let groups = build_groups(
            &issues,
            &blocked,
            &parents,
            &default_args(),
            now,
            None,
            &presence,
            Some(chrono::Duration::minutes(30)),
        );
        assert_eq!(groups.len(), 1);
        let p = groups[0].presence.as_ref().unwrap();
        assert_eq!(p.state, PresenceKind::Working);
    }

    #[test]
    fn presence_past_ttl_becomes_offline() {
        let now = Utc::now();
        let issues = vec![issue("arc1-a", Status::Open, 1, "live")];
        let blocked = HashSet::new();
        let parents = HashMap::new();
        let mut presence = HashMap::new();
        presence.insert(
            "arc1".to_string(),
            PresenceRow {
                prefix: "arc1".into(),
                state: PresenceState::Working,
                last_changed: now - chrono::Duration::hours(2),
            },
        );
        let groups = build_groups(
            &issues,
            &blocked,
            &parents,
            &default_args(),
            now,
            None,
            &presence,
            Some(chrono::Duration::minutes(30)),
        );
        let p = groups[0].presence.as_ref().unwrap();
        assert_eq!(p.state, PresenceKind::Offline);
    }

    #[test]
    fn presence_only_prefix_surfaces_when_live() {
        let now = Utc::now();
        let issues: Vec<Issue> = vec![];
        let blocked = HashSet::new();
        let parents = HashMap::new();
        let mut presence = HashMap::new();
        presence.insert(
            "ghost".to_string(),
            PresenceRow {
                prefix: "ghost".into(),
                state: PresenceState::Idle,
                last_changed: now - chrono::Duration::seconds(10),
            },
        );
        let groups = build_groups(
            &issues,
            &blocked,
            &parents,
            &default_args(),
            now,
            None,
            &presence,
            Some(chrono::Duration::minutes(30)),
        );
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].prefix, "ghost");
        assert!(groups[0].beads.is_empty());
    }

    #[test]
    fn presence_only_offline_is_omitted() {
        let now = Utc::now();
        let issues: Vec<Issue> = vec![];
        let blocked = HashSet::new();
        let parents = HashMap::new();
        let mut presence = HashMap::new();
        presence.insert(
            "stale".to_string(),
            PresenceRow {
                prefix: "stale".into(),
                state: PresenceState::Idle,
                last_changed: now - chrono::Duration::hours(5),
            },
        );
        let groups = build_groups(
            &issues,
            &blocked,
            &parents,
            &default_args(),
            now,
            None,
            &presence,
            Some(chrono::Duration::minutes(30)),
        );
        assert!(groups.is_empty());
    }

    #[test]
    fn truncate_with_ellipsis_preserves_short_strings() {
        assert_eq!(truncate_with_ellipsis("hi", 10), "hi");
        assert_eq!(truncate_with_ellipsis("hello world!", 8), "hello w…");
        assert_eq!(truncate_with_ellipsis("", 5), "");
    }

    #[test]
    fn prefix_filter_keeps_only_matching_group() {
        let issues = vec![
            issue("arc1-a", Status::Open, 1, "arc1"),
            issue("arc2-x", Status::Open, 1, "arc2"),
        ];
        let blocked = HashSet::new();
        let parents = HashMap::new();
        let mut args = default_args();
        args.prefix = Some("arc2".into());
        let groups = build_groups(&issues, &blocked, &parents, &args, Utc::now(), None, &HashMap::new(), None);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].prefix, "arc2");
    }

    #[test]
    fn empty_workspace_renders_no_beads() {
        let mut buf = Vec::new();
        render_text(&mut buf, &[], 80, "24h", Utc::now()).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(s.trim(), "(no beads)");
    }

    #[test]
    fn parse_closed_within_handles_units() {
        assert!(parse_closed_within("").unwrap().is_none());
        assert!(parse_closed_within("0").unwrap().is_none());
        assert!(parse_closed_within("0h").unwrap().is_none());
        assert_eq!(
            parse_closed_within("30s").unwrap(),
            Some(chrono::Duration::seconds(30))
        );
        assert_eq!(
            parse_closed_within("5m").unwrap(),
            Some(chrono::Duration::minutes(5))
        );
        assert_eq!(
            parse_closed_within("2h").unwrap(),
            Some(chrono::Duration::hours(2))
        );
        assert_eq!(
            parse_closed_within("3d").unwrap(),
            Some(chrono::Duration::days(3))
        );
        assert_eq!(
            parse_closed_within("1w").unwrap(),
            Some(chrono::Duration::weeks(1))
        );
        assert!(parse_closed_within("foo").is_err());
        assert!(parse_closed_within("5x").is_err());
    }

    #[test]
    fn format_age_buckets() {
        assert_eq!(format_age(2), "just now");
        assert_eq!(format_age(30), "30s ago");
        assert_eq!(format_age(125), "2m ago");
        assert_eq!(format_age(60 * 90), "1h ago");
        assert_eq!(format_age(60 * 60 * 26), "1d ago");
        assert_eq!(format_age(60 * 60 * 24 * 10), "1w ago");
    }

    #[test]
    fn recently_closed_filters_by_window_and_limit() {
        let now = Utc::now();
        let mk_closed = |id: &str, closed_at: DateTime<Utc>| {
            let mut i = issue(id, Status::Closed, 2, id);
            i.closed_at = Some(closed_at);
            i
        };
        let issues = vec![
            mk_closed("arc1-a", now - chrono::Duration::minutes(5)),  // 5m ago — in window
            mk_closed("arc1-b", now - chrono::Duration::hours(2)),    // 2h — in window
            mk_closed("arc1-c", now - chrono::Duration::hours(30)),   // 30h — out
            issue("arc1-live", Status::Open, 2, "live one"),
        ];
        let blocked = HashSet::new();
        let parents = HashMap::new();
        let mut args = default_args();
        args.closed_limit = 5;
        let groups = build_groups(
            &issues,
            &blocked,
            &parents,
            &args,
            now,
            Some(chrono::Duration::hours(24)),
            &HashMap::new(),
            None,
        );
        assert_eq!(groups.len(), 1);
        let g = &groups[0];
        assert_eq!(g.closed_recently, 2);
        // Sorted newest-first.
        assert_eq!(g.recently_closed[0].id, "arc1-a");
        assert_eq!(g.recently_closed[1].id, "arc1-b");
        // Live still present.
        assert_eq!(g.beads.len(), 1);
        assert_eq!(g.beads[0].id, "arc1-live");
    }

    #[test]
    fn prefix_with_only_closures_still_renders_when_window_set() {
        let now = Utc::now();
        let mut closed = issue("arc1-x", Status::Closed, 2, "done thing");
        closed.closed_at = Some(now - chrono::Duration::minutes(10));
        let issues = vec![closed];
        let blocked = HashSet::new();
        let parents = HashMap::new();
        let groups = build_groups(
            &issues,
            &blocked,
            &parents,
            &default_args(),
            now,
            Some(chrono::Duration::hours(24)),
            &HashMap::new(),
            None,
        );
        assert_eq!(groups.len(), 1);
        assert!(groups[0].beads.is_empty());
        assert_eq!(groups[0].recently_closed.len(), 1);
    }

    #[test]
    fn prefix_with_only_old_closures_is_omitted() {
        let now = Utc::now();
        let mut closed = issue("arc1-x", Status::Closed, 2, "long ago");
        closed.closed_at = Some(now - chrono::Duration::days(5));
        let issues = vec![closed];
        let blocked = HashSet::new();
        let parents = HashMap::new();
        let groups = build_groups(
            &issues,
            &blocked,
            &parents,
            &default_args(),
            now,
            Some(chrono::Duration::hours(24)),
            &HashMap::new(),
            None,
        );
        assert!(groups.is_empty());
    }
}
