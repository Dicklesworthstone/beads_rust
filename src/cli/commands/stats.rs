//! Stats command implementation.
//!
//! Shows project statistics including issue counts by status, type, priority,
//! assignee, and label. Also supports recent activity tracking via git.

use crate::cli::StatsArgs;
use crate::config;
use crate::error::Result;
use crate::format::{
    Breakdown, BreakdownEntry, RecentActivity, Statistics, StatsSummary, truncate_title,
};
use crate::model::{IssueType, Status};
use crate::output::{OutputContext, OutputMode};
use crate::storage::{ListFilters, SqliteStorage};
use chrono::Utc;
use rich_rust::prelude::*;
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use tracing::{debug, info};

/// Execute the stats command.
///
/// # Errors
///
/// Returns an error if the database cannot be opened or queries fail.
pub fn execute(
    args: &StatsArgs,
    _json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir(Some(Path::new(".")))?;
    let storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    let storage = &storage_ctx.storage;

    info!("Computing project statistics");

    // Get all issues including closed and tombstones for comprehensive stats
    let all_filters = ListFilters {
        include_closed: true,
        include_templates: true,
        ..Default::default()
    };
    let all_issues = storage.list_issues(&all_filters)?;

    debug!(total = all_issues.len(), "Loaded all issues for stats");

    // Compute summary counts
    let summary = compute_summary(storage, &all_issues)?;

    // Compute breakdowns if requested
    let mut breakdowns = Vec::new();

    if args.by_type {
        breakdowns.push(compute_type_breakdown(&all_issues));
    }
    if args.by_priority {
        breakdowns.push(compute_priority_breakdown(&all_issues));
    }
    if args.by_assignee {
        breakdowns.push(compute_assignee_breakdown(&all_issues));
    }
    if args.by_label {
        breakdowns.push(compute_label_breakdown(storage, &all_issues)?);
    }

    // Compute recent activity by default (matches bd behavior).
    // Use --no-activity to skip this (for performance).
    let recent_activity = if args.no_activity {
        None
    } else {
        compute_recent_activity(&beads_dir, args.activity_hours)
    };

    let output = Statistics {
        summary,
        breakdowns,
        recent_activity,
    };

    // Output based on mode
    if args.robot {
        // Robot mode: key=value format for script consumption
        print_robot_output(&output);
    } else if ctx.is_json() {
        ctx.json_pretty(&output);
    } else if matches!(ctx.mode(), OutputMode::Rich) {
        render_stats_rich(&output, ctx);
    } else {
        print_text_output(&output);
    }

    Ok(())
}

/// Compute summary statistics.
#[allow(clippy::cast_precision_loss)]
fn compute_summary(
    storage: &SqliteStorage,
    issues: &[crate::model::Issue],
) -> Result<StatsSummary> {
    let mut open = 0;
    let mut in_progress = 0;
    let mut closed = 0;
    let mut blocked_by_status = 0;
    let mut deferred = 0;
    let mut tombstone = 0;
    let mut pinned = 0;
    let mut epics = Vec::new();
    let mut lead_times = Vec::new();

    // Use only 'blocks' dependency type for stats blocked count (classic bd semantics).
    // This differs from the ready/blocked commands which use the full blocked cache.
    let blocked_by_blocks = storage.get_blocked_by_blocks_deps_only()?;

    // Get full blocked cache for accurate Ready count (must match `br ready` behavior)
    let all_blocked_ids = storage.get_blocked_ids()?;

    for issue in issues {
        match issue.status {
            Status::Open => open += 1,
            Status::InProgress => in_progress += 1,
            Status::Closed => {
                closed += 1;
                // Calculate lead time for closed issues
                if let Some(closed_at) = issue.closed_at {
                    let lead_time = closed_at.signed_duration_since(issue.created_at);
                    lead_times.push(lead_time.num_hours() as f64);
                }
            }
            Status::Blocked => blocked_by_status += 1,
            Status::Deferred => deferred += 1,
            Status::Tombstone => tombstone += 1,
            Status::Pinned | Status::Custom(_) => {}
        }
        if issue.pinned || issue.status == Status::Pinned {
            pinned += 1;
        }

        // Track epics for eligible-for-closure calculation
        if issue.issue_type == IssueType::Epic
            && !matches!(issue.status, Status::Closed | Status::Tombstone)
        {
            epics.push(issue.id.clone());
        }
    }

    // Ready count: status=open (not in_progress), no blockers (full definition).
    let now = Utc::now();
    let ready = issues
        .iter()
        .filter(|i| {
            i.status == Status::Open
                && !all_blocked_ids.contains(&i.id)
                && !i.ephemeral
                && !i.pinned
                && i.defer_until.is_none_or(|d| d <= now)
        })
        .count();

    // Blocked count based on 'blocks' deps only (classic bd semantics).
    let blocked = blocked_by_blocks.len();

    // Epics eligible for closure: all children closed
    let epics_eligible = count_epics_eligible_for_closure(storage, &epics)?;

    // Average lead time
    let avg_lead_time = if lead_times.is_empty() {
        None
    } else {
        let sum: f64 = lead_times.iter().sum();
        Some(sum / lead_times.len() as f64)
    };

    // Total excludes tombstones
    let total = issues
        .iter()
        .filter(|i| i.status != Status::Tombstone)
        .count();

    // blocked_by_status is unused but kept for potential future use
    let _ = blocked_by_status;

    Ok(StatsSummary {
        total_issues: total,
        open_issues: open,
        in_progress_issues: in_progress,
        closed_issues: closed,
        blocked_issues: blocked,
        deferred_issues: deferred,
        ready_issues: ready,
        tombstone_issues: tombstone,
        pinned_issues: pinned,
        epics_eligible_for_closure: epics_eligible,
        average_lead_time_hours: avg_lead_time,
    })
}

/// Count epics that have all children closed.
fn count_epics_eligible_for_closure(storage: &SqliteStorage, epic_ids: &[String]) -> Result<usize> {
    let mut eligible = 0;

    for epic_id in epic_ids {
        // Get children via parent-child dependencies
        let children = storage.get_dependents_with_metadata(epic_id)?;
        let parent_child_children: Vec<_> = children
            .iter()
            .filter(|c| c.dep_type == "parent-child")
            .collect();

        if parent_child_children.is_empty() {
            // No children means not eligible (nothing to close)
            continue;
        }

        // Check if all children are closed
        let all_closed = parent_child_children
            .iter()
            .all(|c| matches!(c.status, Status::Closed | Status::Tombstone));

        if all_closed {
            eligible += 1;
        }
    }

    Ok(eligible)
}

/// Compute breakdown by issue type.
fn compute_type_breakdown(issues: &[crate::model::Issue]) -> Breakdown {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();

    for issue in issues {
        if issue.status == Status::Tombstone {
            continue;
        }
        let key = issue.issue_type.as_str().to_string();
        *counts.entry(key).or_insert(0) += 1;
    }

    Breakdown {
        dimension: "type".to_string(),
        counts: counts
            .into_iter()
            .map(|(key, count)| BreakdownEntry { key, count })
            .collect(),
    }
}

/// Compute breakdown by priority.
fn compute_priority_breakdown(issues: &[crate::model::Issue]) -> Breakdown {
    let mut counts: BTreeMap<i32, usize> = BTreeMap::new();

    for issue in issues {
        if issue.status == Status::Tombstone {
            continue;
        }
        *counts.entry(issue.priority.0).or_insert(0) += 1;
    }

    Breakdown {
        dimension: "priority".to_string(),
        counts: counts
            .into_iter()
            .map(|(p, count)| BreakdownEntry {
                key: format!("P{p}"),
                count,
            })
            .collect(),
    }
}

/// Compute breakdown by assignee.
fn compute_assignee_breakdown(issues: &[crate::model::Issue]) -> Breakdown {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();

    for issue in issues {
        if issue.status == Status::Tombstone {
            continue;
        }
        let key = issue
            .assignee
            .as_deref()
            .unwrap_or("(unassigned)")
            .to_string();
        *counts.entry(key).or_insert(0) += 1;
    }

    Breakdown {
        dimension: "assignee".to_string(),
        counts: counts
            .into_iter()
            .map(|(key, count)| BreakdownEntry { key, count })
            .collect(),
    }
}

/// Compute breakdown by label.
fn compute_label_breakdown(
    storage: &SqliteStorage,
    issues: &[crate::model::Issue],
) -> Result<Breakdown> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();

    for issue in issues {
        if issue.status == Status::Tombstone {
            continue;
        }
        let labels = storage.get_labels(&issue.id)?;
        if labels.is_empty() {
            *counts.entry("(no labels)".to_string()).or_insert(0) += 1;
        } else {
            for label in labels {
                *counts.entry(label).or_insert(0) += 1;
            }
        }
    }

    Ok(Breakdown {
        dimension: "label".to_string(),
        counts: counts
            .into_iter()
            .map(|(key, count)| BreakdownEntry { key, count })
            .collect(),
    })
}

/// Compute recent activity from git log on issues.jsonl.
fn compute_recent_activity(beads_dir: &Path, hours: u32) -> Option<RecentActivity> {
    let jsonl_path = beads_dir.join("issues.jsonl");
    if !jsonl_path.exists() {
        debug!("No issues.jsonl found for activity tracking");
        return None;
    }

    let since = format!("{hours} hours ago");

    // Get the git repo root (parent of .beads)
    let repo_root = beads_dir.parent().unwrap_or(beads_dir);

    // Get commit count using relative path from repo root
    let mut child = Command::new("git")
        .args([
            "log",
            "--oneline",
            "--since",
            &since,
            "--",
            ".beads/issues.jsonl",
        ])
        .current_dir(repo_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;

    let stdout = child.stdout.take()?;
    let reader = BufReader::new(stdout);
    let commit_count = reader.lines().count();

    let status = child.wait().ok()?;
    if !status.success() {
        // Log stderr if available
        if let Some(stderr) = child.stderr {
            use std::io::Read;
            let mut err_msg = String::new();
            if std::io::BufReader::new(stderr)
                .read_to_string(&mut err_msg)
                .is_ok()
            {
                debug!(stderr = %err_msg, "Git log failed");
            }
        }
        return None;
    }

    Some(RecentActivity {
        hours_tracked: hours,
        commit_count,
        issues_created: 0,
        issues_closed: 0,
        issues_updated: 0,
        issues_reopened: 0,
        total_changes: 0,
    })
}

/// Print robot-friendly key=value output for script consumption.
fn print_robot_output(output: &Statistics) {
    let s = &output.summary;
    println!(
        "total={} open={} in_progress={} closed={} ready={} blocked={} deferred={} tombstone={} pinned={}",
        s.total_issues,
        s.open_issues,
        s.in_progress_issues,
        s.closed_issues,
        s.ready_issues,
        s.blocked_issues,
        s.deferred_issues,
        s.tombstone_issues,
        s.pinned_issues,
    );

    if s.epics_eligible_for_closure > 0 {
        println!("epics_closeable={}", s.epics_eligible_for_closure);
    }

    if let Some(avg) = s.average_lead_time_hours {
        println!("avg_lead_time_hours={:.1}", avg);
    }

    for breakdown in &output.breakdowns {
        for entry in &breakdown.counts {
            // Normalize key for shell-friendly output
            let key = entry
                .key
                .to_lowercase()
                .replace(' ', "_")
                .replace(['(', ')'], "");
            println!("{}_{key}={}", breakdown.dimension, entry.count);
        }
    }

    if let Some(activity) = &output.recent_activity {
        println!(
            "activity_hours={} commits={} changes={}",
            activity.hours_tracked, activity.commit_count, activity.total_changes
        );
    }
}

/// Print text output for stats.
fn print_text_output(output: &Statistics) {
    // Match bd format: 📊 Issue Database Status
    println!("📊 Issue Database Status\n");

    let s = &output.summary;
    println!("Summary:");
    // Match bd alignment (right-aligned numbers, 18-char label width)
    println!("  Total Issues:           {}", s.total_issues);
    println!("  Open:                   {}", s.open_issues);
    println!("  In Progress:            {}", s.in_progress_issues);
    println!("  Blocked:                {}", s.blocked_issues);
    println!("  Closed:                 {}", s.closed_issues);
    println!("  Ready to Work:          {}", s.ready_issues);

    // Optional fields (only show if non-zero)
    if s.deferred_issues > 0 {
        println!("  Deferred:               {}", s.deferred_issues);
    }
    if s.tombstone_issues > 0 {
        println!("  Tombstones:             {}", s.tombstone_issues);
    }
    if s.pinned_issues > 0 {
        println!("  Pinned:                 {}", s.pinned_issues);
    }
    if s.epics_eligible_for_closure > 0 {
        println!("  Epics ready to close:   {}", s.epics_eligible_for_closure);
    }

    // Extended section (matches bd format)
    if s.average_lead_time_hours.is_some() || s.tombstone_issues > 0 {
        println!("\nExtended:");
        if let Some(avg_hours) = s.average_lead_time_hours {
            // Format like bd: "N.N hours" or "N days" for large values
            let formatted = if avg_hours >= 24.0 {
                let avg_days = avg_hours / 24.0;
                format!("{avg_days:.1} days")
            } else {
                format!("{avg_hours:.1} hours")
            };
            println!("  Avg Lead Time:          {formatted}");
        }
        if s.tombstone_issues > 0 {
            println!(
                "  Deleted:                {} (tombstones)",
                s.tombstone_issues
            );
        }
    }

    for breakdown in &output.breakdowns {
        println!("\nBy {}:", breakdown.dimension);
        for entry in &breakdown.counts {
            println!("  {}: {}", entry.key, entry.count);
        }
    }

    if let Some(activity) = &output.recent_activity {
        println!("\nRecent Activity (last {} hours):", activity.hours_tracked);
        println!("  Commits:                {}", activity.commit_count);
        println!("  Total Changes:          {}", activity.total_changes);
        println!("  Issues Created:         {}", activity.issues_created);
        println!("  Issues Closed:          {}", activity.issues_closed);
        println!("  Issues Reopened:        {}", activity.issues_reopened);
        println!("  Issues Updated:         {}", activity.issues_updated);
    }

    // Match bd footer
    println!("\nFor more details, use 'bd list' to see individual issues.");
}

/// Render stats with rich formatting.
#[allow(clippy::cast_precision_loss)]
fn render_stats_rich(output: &Statistics, ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    // Build content as Text with multiple sections
    let mut content = Text::new("");

    // === Overview Section ===
    content.append_styled("\u{1f4ca} Overview\n", theme.section.clone());

    let s = &output.summary;

    // Main stats row
    content.append_styled("   Total: ", theme.dimmed.clone());
    content.append_styled(&s.total_issues.to_string(), theme.emphasis.clone());

    content.append_styled("    Ready: ", theme.dimmed.clone());
    content.append_styled(&s.ready_issues.to_string(), theme.success.clone());
    content.append_styled(" \u{2713}", theme.success.clone());

    content.append_styled("    Blocked: ", theme.dimmed.clone());
    content.append_styled(&s.blocked_issues.to_string(), theme.warning.clone());
    if s.blocked_issues > 0 {
        content.append_styled(" \u{26a0}", theme.warning.clone());
    }
    content.append("\n\n");

    // === Status Breakdown ===
    content.append_styled("\u{1f4c8} By Status\n", theme.section.clone());
    render_status_bars(&mut content, s, theme);
    content.append("\n");

    // === Optional Breakdowns ===
    for breakdown in &output.breakdowns {
        content.append_styled(
            &format!("\u{1f4c8} By {}\n", capitalize(&breakdown.dimension)),
            theme.section.clone(),
        );
        render_breakdown_bars(&mut content, breakdown, s.total_issues, theme);
        content.append("\n");
    }

    // === Recent Activity ===
    if let Some(activity) = &output.recent_activity {
        content.append_styled(
            &format!(
                "\u{1f4c5} Activity (last {} hours)\n",
                activity.hours_tracked
            ),
            theme.section.clone(),
        );
        content.append_styled("   Commits: ", theme.dimmed.clone());
        content.append(&activity.commit_count.to_string());
        if activity.total_changes > 0 {
            content.append_styled("    Changes: ", theme.dimmed.clone());
            content.append(&activity.total_changes.to_string());
        }
        content.append("\n\n");
    }

    // === Health Warnings ===
    let mut warnings = Vec::new();
    if s.blocked_issues > 5 {
        warnings.push(format!("{} issues blocked", s.blocked_issues));
    }
    if s.epics_eligible_for_closure > 0 {
        warnings.push(format!(
            "{} epic{} ready to close",
            s.epics_eligible_for_closure,
            if s.epics_eligible_for_closure == 1 {
                ""
            } else {
                "s"
            }
        ));
    }
    if s.deferred_issues > 10 {
        warnings.push(format!("{} issues deferred", s.deferred_issues));
    }

    if !warnings.is_empty() {
        content.append_styled("\u{26a0} Health Warnings\n", theme.warning.clone());
        for warning in &warnings {
            content.append_styled("   \u{2022} ", theme.warning.clone());
            content.append(warning);
            content.append("\n");
        }
    }

    // Wrap in panel
    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled("Project Health", theme.panel_title.clone()))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

/// Render status distribution as progress bars.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn render_status_bars(content: &mut Text, summary: &StatsSummary, theme: &crate::output::Theme) {
    let total = summary.total_issues.max(1);
    let bar_width: usize = 24;

    let statuses = [
        ("Open", summary.open_issues, &theme.status_open),
        (
            "In Progress",
            summary.in_progress_issues,
            &theme.status_in_progress,
        ),
        ("Blocked", summary.blocked_issues, &theme.status_blocked),
        ("Closed", summary.closed_issues, &theme.status_closed),
    ];

    for (label, count, style) in statuses {
        if count == 0 {
            continue;
        }
        let pct = (count as f64 / total as f64) * 100.0;
        let filled = ((count as f64 / total as f64) * bar_width as f64).round() as usize;
        let empty = bar_width.saturating_sub(filled);

        content.append_styled(&format!("   {:<12}", label), style.clone());
        content.append_styled(&"\u{2588}".repeat(filled), style.clone());
        content.append_styled(&"\u{2591}".repeat(empty), theme.dimmed.clone());
        content.append_styled(
            &format!(" {:>3} ({:.0}%)", count, pct),
            theme.dimmed.clone(),
        );
        content.append("\n");
    }
}

/// Render a breakdown as progress bars.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]
fn render_breakdown_bars(
    content: &mut Text,
    breakdown: &Breakdown,
    total: usize,
    theme: &crate::output::Theme,
) {
    let total = total.max(1);
    let bar_width: usize = 24;

    for entry in &breakdown.counts {
        let pct = (entry.count as f64 / total as f64) * 100.0;
        let filled = ((entry.count as f64 / total as f64) * bar_width as f64).round() as usize;
        let empty = bar_width.saturating_sub(filled);

        // Choose style based on key
        let style = match breakdown.dimension.as_str() {
            "priority" => match entry.key.as_str() {
                "P0" => theme.priority_critical.clone(),
                "P1" => theme.priority_high.clone(),
                "P2" => theme.priority_medium.clone(),
                "P3" => theme.priority_low.clone(),
                _ => theme.priority_backlog.clone(),
            },
            "type" => match entry.key.as_str() {
                "task" => theme.type_task.clone(),
                "bug" => theme.type_bug.clone(),
                "feature" => theme.type_feature.clone(),
                "epic" => theme.type_epic.clone(),
                "chore" => theme.type_chore.clone(),
                "docs" => theme.type_docs.clone(),
                "question" => theme.type_question.clone(),
                _ => theme.dimmed.clone(),
            },
            _ => theme.accent.clone(),
        };

        content.append_styled(
            &format!("   {:<12}", truncate_title(&entry.key, 12)),
            style.clone(),
        );
        content.append_styled(&"\u{2588}".repeat(filled), style.clone());
        content.append_styled(&"\u{2591}".repeat(empty), theme.dimmed.clone());
        content.append_styled(
            &format!(" {:>3} ({:.0}%)", entry.count, pct),
            theme.dimmed.clone(),
        );
        content.append("\n");
    }
}

/// Capitalize the first letter of a string.
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Issue, IssueType, Priority, Status};
    use crate::storage::SqliteStorage;
    use chrono::Utc;

    fn make_issue(id: &str, status: Status, issue_type: IssueType) -> Issue {
        Issue {
            id: id.to_string(),
            title: format!("Issue {id}"),
            description: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            status,
            priority: Priority::MEDIUM,
            issue_type,
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_at: Utc::now(),
            created_by: None,
            updated_at: Utc::now(),
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            due_at: None,
            defer_until: None,
            external_ref: None,
            source_system: None,
            deleted_at: None,
            deleted_by: None,
            delete_reason: None,
            original_type: None,
            compaction_level: None,
            compacted_at: None,
            compacted_at_commit: None,
            original_size: None,
            sender: None,
            ephemeral: false,
            pinned: false,
            is_template: false,
            labels: vec![],
            dependencies: vec![],
            comments: vec![],
            content_hash: None,
        }
    }

    #[test]
    fn test_compute_type_breakdown() {
        let test_issues = vec![
            make_issue("t-1", Status::Open, IssueType::Task),
            make_issue("t-2", Status::Open, IssueType::Task),
            make_issue("t-3", Status::Open, IssueType::Bug),
            make_issue("t-4", Status::Tombstone, IssueType::Feature), // Excluded
        ];

        let breakdown = compute_type_breakdown(&test_issues);
        assert_eq!(breakdown.dimension, "type");

        let mut map: BTreeMap<String, usize> = BTreeMap::new();
        for entry in &breakdown.counts {
            map.insert(entry.key.clone(), entry.count);
        }

        assert_eq!(map.get("task"), Some(&2));
        assert_eq!(map.get("bug"), Some(&1));
        assert_eq!(map.get("feature"), None); // Tombstone excluded
    }

    #[test]
    fn test_compute_priority_breakdown() {
        let mut test_issues = vec![
            make_issue("t-1", Status::Open, IssueType::Task),
            make_issue("t-2", Status::Open, IssueType::Task),
            make_issue("t-3", Status::Open, IssueType::Bug),
        ];
        test_issues[0].priority = Priority::CRITICAL;
        test_issues[1].priority = Priority::CRITICAL;
        test_issues[2].priority = Priority::LOW;

        let breakdown = compute_priority_breakdown(&test_issues);
        assert_eq!(breakdown.dimension, "priority");

        let mut map: BTreeMap<String, usize> = BTreeMap::new();
        for entry in &breakdown.counts {
            map.insert(entry.key.clone(), entry.count);
        }

        assert_eq!(map.get("P0"), Some(&2));
        assert_eq!(map.get("P3"), Some(&1));
    }

    #[test]
    fn test_compute_assignee_breakdown() {
        let mut test_issues = vec![
            make_issue("t-1", Status::Open, IssueType::Task),
            make_issue("t-2", Status::Open, IssueType::Task),
            make_issue("t-3", Status::Open, IssueType::Bug),
        ];
        test_issues[0].assignee = Some("alice".to_string());
        test_issues[1].assignee = Some("alice".to_string());

        let breakdown = compute_assignee_breakdown(&test_issues);
        assert_eq!(breakdown.dimension, "assignee");

        let mut map: BTreeMap<String, usize> = BTreeMap::new();
        for entry in &breakdown.counts {
            map.insert(entry.key.clone(), entry.count);
        }

        assert_eq!(map.get("alice"), Some(&2));
        assert_eq!(map.get("(unassigned)"), Some(&1));
    }

    #[test]
    fn test_compute_summary_basic() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        let first_issue = make_issue("t-1", Status::Open, IssueType::Task);
        let second_issue = make_issue("t-2", Status::InProgress, IssueType::Task);
        let mut third_issue = make_issue("t-3", Status::Closed, IssueType::Bug);
        third_issue.closed_at = Some(Utc::now());

        storage.create_issue(&first_issue, "tester").unwrap();
        storage.create_issue(&second_issue, "tester").unwrap();
        storage.create_issue(&third_issue, "tester").unwrap();

        let all_issues = vec![first_issue, second_issue, third_issue];
        let summary = compute_summary(&storage, &all_issues).unwrap();

        assert_eq!(summary.total_issues, 3);
        assert_eq!(summary.open_issues, 1);
        assert_eq!(summary.in_progress_issues, 1);
        assert_eq!(summary.closed_issues, 1);
    }

    #[test]
    fn test_blocked_by_blocks_deps() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        let blocking_issue = make_issue("t-1", Status::Open, IssueType::Task);
        let dependent_issue = make_issue("t-2", Status::Open, IssueType::Task);

        storage.create_issue(&blocking_issue, "tester").unwrap();
        storage.create_issue(&dependent_issue, "tester").unwrap();
        storage
            .add_dependency("t-2", "t-1", "blocks", "tester")
            .unwrap();

        let blocked_ids = storage.get_blocked_by_blocks_deps_only().unwrap();
        assert!(blocked_ids.contains("t-2"));
        assert!(!blocked_ids.contains("t-1"));
    }

    #[test]
    fn test_blocked_cleared_when_blocker_closed() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        let mut blocking_issue = make_issue("t-1", Status::Closed, IssueType::Task);
        blocking_issue.closed_at = Some(Utc::now());
        let dependent_issue = make_issue("t-2", Status::Open, IssueType::Task);

        storage.create_issue(&blocking_issue, "tester").unwrap();
        storage.create_issue(&dependent_issue, "tester").unwrap();
        storage
            .add_dependency("t-2", "t-1", "blocks", "tester")
            .unwrap();

        let blocked_ids = storage.get_blocked_by_blocks_deps_only().unwrap();
        // t-2 should NOT be blocked because t-1 is closed
        assert!(!blocked_ids.contains("t-2"));
    }

    #[test]
    fn test_label_breakdown() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        let first_issue = make_issue("t-1", Status::Open, IssueType::Task);
        let second_issue = make_issue("t-2", Status::Open, IssueType::Task);
        let third_issue = make_issue("t-3", Status::Open, IssueType::Task);

        storage.create_issue(&first_issue, "tester").unwrap();
        storage.create_issue(&second_issue, "tester").unwrap();
        storage.create_issue(&third_issue, "tester").unwrap();

        storage.add_label("t-1", "backend", "tester").unwrap();
        storage.add_label("t-1", "urgent", "tester").unwrap();
        storage.add_label("t-2", "backend", "tester").unwrap();

        let test_issues = vec![first_issue, second_issue, third_issue];
        let breakdown = compute_label_breakdown(&storage, &test_issues).unwrap();

        let mut map: BTreeMap<String, usize> = BTreeMap::new();
        for entry in &breakdown.counts {
            map.insert(entry.key.clone(), entry.count);
        }

        assert_eq!(map.get("backend"), Some(&2));
        assert_eq!(map.get("urgent"), Some(&1));
        assert_eq!(map.get("(no labels)"), Some(&1));
    }

    #[test]
    fn test_truncate_title_ascii() {
        assert_eq!(truncate_title("short", 12), "short");
        assert_eq!(truncate_title("exactly_twelve", 14), "exactly_twelve");
        assert_eq!(
            truncate_title("this_is_too_long_for_column", 12),
            "this_is_t..."
        );
    }

    #[test]
    fn test_truncate_title_multibyte() {
        // Multi-byte characters should not cause panics
        let emoji = "😊".repeat(10); // 10 chars, 20 visual width
        // truncate to 5 visual width
        let result = truncate_title(&emoji, 5);
        assert!(result.ends_with("..."));
        // 5 visual width: "😊" (2) + "..." (3) = 5
        assert_eq!(result, "😊...");

        // Mixed ASCII and emoji
        let mixed = "abc😊def";
        // "abc" (3) + "😊" (2) + "def" (3) = 8 width
        assert_eq!(truncate_title(mixed, 8), "abc😊def");
        // "abc" (3) + "..." (3) = 6
        assert_eq!(truncate_title(mixed, 6), "abc...");
    }

    #[test]
    fn test_capitalize() {
        assert_eq!(capitalize("type"), "Type");
        assert_eq!(capitalize("priority"), "Priority");
        assert_eq!(capitalize(""), "");
        assert_eq!(capitalize("ALREADY"), "ALREADY");
    }
}
