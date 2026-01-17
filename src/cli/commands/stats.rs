//! Stats command implementation.
//!
//! Shows project statistics including issue counts by status, type, priority,
//! assignee, and label. Also supports recent activity tracking via git.

use crate::cli::StatsArgs;
use crate::config;
use crate::error::Result;
use crate::model::{IssueType, Status};
use crate::storage::{ListFilters, SqliteStorage};
use chrono::Utc;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;
use tracing::{debug, info, warn};

/// Summary statistics for the project.
#[derive(Serialize, Debug)]
pub struct StatsSummary {
    pub total_issues: usize,
    pub open_issues: usize,
    pub in_progress_issues: usize,
    pub closed_issues: usize,
    pub blocked_issues: usize,
    pub deferred_issues: usize,
    pub ready_issues: usize,
    pub tombstone_issues: usize,
    pub pinned_issues: usize,
    pub epics_eligible_for_closure: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub average_lead_time_hours: Option<f64>,
}

/// Breakdown statistics by a dimension.
#[derive(Serialize, Debug)]
pub struct Breakdown {
    pub dimension: String,
    pub counts: Vec<BreakdownEntry>,
}

/// A single entry in a breakdown.
#[derive(Serialize, Debug)]
pub struct BreakdownEntry {
    pub key: String,
    pub count: usize,
}

/// Recent activity statistics from git history.
#[derive(Serialize, Debug)]
pub struct RecentActivity {
    pub hours_tracked: u32,
    pub commit_count: usize,
    pub issues_created: usize,
    pub issues_closed: usize,
    pub issues_updated: usize,
}

/// Complete stats output structure.
#[derive(Serialize, Debug)]
pub struct StatsOutput {
    pub summary: StatsSummary,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub breakdowns: Vec<Breakdown>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recent_activity: Option<RecentActivity>,
}

/// Execute the stats command.
///
/// # Errors
///
/// Returns an error if the database cannot be opened or queries fail.
pub fn execute(args: &StatsArgs, json: bool, cli: &config::CliOverrides) -> Result<()> {
    let beads_dir = config::discover_beads_dir(Some(Path::new(".")))?;
    let (storage, _paths) = config::open_storage(&beads_dir, cli.db.as_ref(), cli.lock_timeout)?;

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
    let summary = compute_summary(&storage, &all_issues)?;

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
        breakdowns.push(compute_label_breakdown(&storage, &all_issues)?);
    }

    // Compute recent activity if requested
    let recent_activity = if args.activity {
        compute_recent_activity(&beads_dir, args.activity_hours)
    } else {
        None
    };

    let output = StatsOutput {
        summary,
        breakdowns,
        recent_activity,
    };

    // Output
    let use_json = json || args.robot;
    if use_json {
        let json_str = serde_json::to_string_pretty(&output)?;
        println!("{json_str}");
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

    // Use blocked cache to align with ready/blocked command semantics.
    let blocked_ids = storage.get_blocked_ids()?;

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

    // Ready count: open status, not blocked, not deferred, not pinned, not ephemeral
    let now = Utc::now();
    let ready = issues
        .iter()
        .filter(|i| {
            matches!(i.status, Status::Open | Status::InProgress)
                && !blocked_ids.contains(&i.id)
                && !i.ephemeral
                && !i.pinned
                && i.defer_until.is_none_or(|d| d <= now)
        })
        .count();

    // Blocked count based on blocked cache.
    let blocked = blocked_ids.len();

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

    // Get commit count
    let commit_output = Command::new("git")
        .args(["log", "--oneline", "--since", &since, "--", "issues.jsonl"])
        .current_dir(beads_dir.parent().unwrap_or(beads_dir))
        .output();

    let commit_count = match commit_output {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            stdout.lines().count()
        }
        Ok(output) => {
            debug!(
                stderr = %String::from_utf8_lossy(&output.stderr),
                "Git log failed"
            );
            0
        }
        Err(e) => {
            warn!("Failed to run git: {e}");
            return None;
        }
    };

    Some(RecentActivity {
        hours_tracked: hours,
        commit_count,
        issues_created: 0,
        issues_closed: 0,
        issues_updated: 0,
    })
}

/// Print text output for stats.
fn print_text_output(output: &StatsOutput) {
    println!("Project Statistics");
    println!("==================\n");

    let s = &output.summary;
    println!("Summary:");
    println!("  Total issues:     {}", s.total_issues);
    println!("  Open:             {}", s.open_issues);
    println!("  In Progress:      {}", s.in_progress_issues);
    println!("  Closed:           {}", s.closed_issues);
    println!("  Blocked:          {}", s.blocked_issues);
    println!("  Deferred:         {}", s.deferred_issues);
    println!("  Ready:            {}", s.ready_issues);
    if s.tombstone_issues > 0 {
        println!("  Tombstones:       {}", s.tombstone_issues);
    }
    if s.pinned_issues > 0 {
        println!("  Pinned:           {}", s.pinned_issues);
    }
    if s.epics_eligible_for_closure > 0 {
        println!("  Epics ready to close: {}", s.epics_eligible_for_closure);
    }
    if let Some(avg) = s.average_lead_time_hours {
        println!("  Avg lead time:    {avg:.1}h");
    }

    for breakdown in &output.breakdowns {
        println!("\nBy {}:", breakdown.dimension);
        for entry in &breakdown.counts {
            println!("  {}: {}", entry.key, entry.count);
        }
    }

    if let Some(activity) = &output.recent_activity {
        println!("\nRecent Activity (last {}h):", activity.hours_tracked);
        println!("  Commits: {}", activity.commit_count);
        if activity.issues_created > 0 {
            println!("  Created: {}", activity.issues_created);
        }
        if activity.issues_closed > 0 {
            println!("  Closed: {}", activity.issues_closed);
        }
        if activity.issues_updated > 0 {
            println!("  Updated: {}", activity.issues_updated);
        }
    }
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
}
