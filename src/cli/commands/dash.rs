//! `bd dash` — grouped, refreshing situational-awareness view.
//!
//! Renders beads clustered by ID prefix with workable/blocked/in-progress
//! distinctions. Optionally redraws every N seconds for a `top`-style view.

use crate::cli::{DashArgs, OutputFormat, resolve_output_format_basic};
use crate::config;
use crate::error::Result;
use crate::model::{Issue, Priority, Status};
use crate::output::OutputContext;
use crate::storage::{ListFilters, SqliteStorage};
use crate::util::id::split_prefix_remainder;
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
            Self::Deferred => "⏸",
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
    beads: Vec<DashBead<'a>>,
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

    let filters = ListFilters {
        include_closed: args.show_closed,
        include_deferred: args.show_deferred,
        ..Default::default()
    };
    let issues = storage.list_issues(&filters)?;
    let blocked_ids = storage.get_blocked_ids()?;
    let parents = fetch_parent_map(&storage)?;

    let groups = build_groups(&issues, &blocked_ids, &parents, args);

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    match format {
        OutputFormat::Json | OutputFormat::Toon => render_json(&mut out, &groups)?,
        _ => render_text(&mut out, &groups, terminal_width())?,
    }
    out.flush().ok();
    Ok(())
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

fn build_groups<'a>(
    issues: &'a [Issue],
    blocked_ids: &HashSet<String>,
    parents: &'a HashMap<String, String>,
    args: &DashArgs,
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

    let mut groups = Vec::with_capacity(by_prefix.len());
    for (prefix, mut beads) in by_prefix {
        beads.sort_by(|a, b| {
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

        let rows: Vec<OwnedBead> = beads
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

        groups.push(OwnedGroup {
            prefix,
            in_progress,
            ready,
            blocked,
            deferred,
            closed,
            beads: rows,
        });
    }
    groups
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
    beads: Vec<OwnedBead>,
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

fn render_text<W: Write>(out: &mut W, groups: &[OwnedGroup], width: u16) -> Result<()> {
    if groups.is_empty() || groups.iter().all(|g| g.beads.is_empty()) {
        writeln!(out, "(no beads)")?;
        return Ok(());
    }

    // Pre-compute max ID width so columns align inside each cluster.
    for group in groups {
        writeln!(out, "=== {} {} ===", group.prefix, header_counts(group))?;
        if group.beads.is_empty() {
            writeln!(out, "  (no beads in this group)")?;
            continue;
        }

        let id_w = group.beads.iter().map(|b| b.id.len()).max().unwrap_or(8);
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
    if parts.is_empty() {
        "(empty)".to_string()
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

fn render_json<W: Write>(out: &mut W, groups: &[OwnedGroup]) -> Result<()> {
    let view: Vec<DashGroup> = groups
        .iter()
        .map(|g| DashGroup {
            prefix: &g.prefix,
            in_progress: g.in_progress,
            ready: g.ready,
            blocked: g.blocked,
            deferred: g.deferred,
            closed: g.closed,
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
        })
        .collect();
    let payload = DashOutput {
        ts: chrono::Utc::now().to_rfc3339(),
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

    #[test]
    fn group_sort_orders_in_progress_first() {
        let issues = vec![
            issue("arc1-c", Status::Open, 2, "ready"),
            issue("arc1-a", Status::Open, 0, "blocked"),
            issue("arc1-b", Status::InProgress, 3, "running"),
        ];
        let blocked: HashSet<String> = ["arc1-a".to_string()].into_iter().collect();
        let parents = HashMap::new();
        let args = DashArgs::default();
        let groups = build_groups(&issues, &blocked, &parents, &args);
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
        let args = DashArgs::default();
        let groups = build_groups(&issues, &blocked, &parents, &args);
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
            beads: vec![],
        };
        assert_eq!(header_counts(&g), "(3 ready)");

        let g2 = OwnedGroup {
            prefix: "arc1".into(),
            in_progress: 1,
            ready: 2,
            blocked: 1,
            deferred: 0,
            closed: 0,
            beads: vec![],
        };
        assert_eq!(header_counts(&g2), "(2 ready, 1 blocked, 1 in progress)");
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
        let mut args = DashArgs::default();
        args.prefix = Some("arc2".into());
        let groups = build_groups(&issues, &blocked, &parents, &args);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].prefix, "arc2");
    }

    #[test]
    fn empty_workspace_renders_no_beads() {
        let mut buf = Vec::new();
        render_text(&mut buf, &[], 80).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert_eq!(s.trim(), "(no beads)");
    }
}
