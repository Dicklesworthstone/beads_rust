//! `br blocked` command implementation.
//!
//! Lists blocked issues from the `blocked_issues_cache`.

use crate::cli::BlockedArgs;
use crate::config::{
    CliOverrides, discover_beads_dir, external_project_db_paths, load_config, open_storage_with_cli,
};
use crate::error::{BeadsError, Result};
use crate::format::BlockedIssue;

/// Execute the blocked command.
///
/// # Errors
///
/// Returns an error if:
/// - The beads directory cannot be found
/// - The database cannot be opened
/// - Querying blocked issues fails
pub fn execute(args: &BlockedArgs, json: bool, overrides: &CliOverrides) -> Result<()> {
    tracing::info!("Fetching blocked issues from cache");

    let beads_dir = discover_beads_dir(None)?;
    let storage_ctx = open_storage_with_cli(&beads_dir, overrides)?;
    let storage = &storage_ctx.storage;

    let config_layer = load_config(&beads_dir, Some(storage), overrides)?;
    let external_db_paths = external_project_db_paths(&config_layer, &beads_dir);

    // Get blocked issues from cache
    let blocked_raw = storage.get_blocked_issues()?;

    tracing::debug!(
        count = blocked_raw.len(),
        "Found {} blocked issues",
        blocked_raw.len()
    );

    // Convert to BlockedIssue format
    let mut blocked_issues: Vec<BlockedIssue> = blocked_raw
        .into_iter()
        .map(|(issue, blockers)| BlockedIssue {
            blocked_by_count: blockers.len(),
            blocked_by: blockers,
            issue,
        })
        .collect();

    let external_statuses =
        storage.resolve_external_dependency_statuses(&external_db_paths, true)?;
    let external_blockers = storage.external_blockers(&external_statuses)?;

    if !external_blockers.is_empty() {
        let mut by_id: std::collections::HashMap<String, usize> = blocked_issues
            .iter()
            .enumerate()
            .map(|(idx, bi)| (bi.issue.id.clone(), idx))
            .collect();

        for (issue_id, blockers) in external_blockers {
            if let Some(idx) = by_id.get(&issue_id).copied() {
                let entry = &mut blocked_issues[idx];
                entry.blocked_by.extend(blockers);
                entry.blocked_by.sort();
                entry.blocked_by.dedup();
                entry.blocked_by_count = entry.blocked_by.len();
                continue;
            }

            if let Ok(Some(issue)) = storage.get_issue(&issue_id) {
                if issue.status.is_terminal() {
                    continue;
                }
                let blocked_by_count = blockers.len();
                blocked_issues.push(BlockedIssue {
                    blocked_by_count,
                    blocked_by: blockers,
                    issue,
                });
                by_id.insert(issue_id, blocked_issues.len() - 1);
            }
        }
    }

    // Apply filters
    filter_by_type(&mut blocked_issues, &args.type_);
    filter_by_priority(&mut blocked_issues, &args.priority);

    // Filter by labels (AND logic) - need to fetch labels from storage
    if !args.label.is_empty() {
        blocked_issues.retain(|bi| {
            let issue_labels = storage.get_labels(&bi.issue.id).unwrap_or_default();
            args.label.iter().all(|l| issue_labels.contains(l))
        });
    }

    // Sort by priority (ascending), then by blocker count (descending)
    sort_blocked_issues(&mut blocked_issues);

    // Apply limit
    if args.limit > 0 && blocked_issues.len() > args.limit {
        blocked_issues.truncate(args.limit);
    }

    for bi in &blocked_issues {
        tracing::trace!(
            id = %bi.issue.id,
            blockers = ?bi.blocked_by,
            "Blocked issue: {} blocked by {:?}",
            bi.issue.id,
            bi.blocked_by
        );
    }

    // Output
    if json {
        let output = serde_json::to_string_pretty(&blocked_issues).map_err(BeadsError::Json)?;
        println!("{output}");
    } else {
        print_text_output(&blocked_issues, args.detailed, storage);
    }

    Ok(())
}

/// Sort blocked issues by priority (ascending), then by blocker count (descending).
fn sort_blocked_issues(issues: &mut [BlockedIssue]) {
    issues.sort_by(|a, b| {
        let pa = a.issue.priority.0;
        let pb = b.issue.priority.0;
        pa.cmp(&pb)
            .then_with(|| b.blocked_by_count.cmp(&a.blocked_by_count))
    });
}

/// Filter blocked issues by issue type (case-insensitive).
fn filter_by_type(issues: &mut Vec<BlockedIssue>, types: &[String]) {
    if types.is_empty() {
        return;
    }
    issues.retain(|bi| {
        let issue_type_str = bi.issue.issue_type.to_string().to_lowercase();
        types.iter().any(|t| t.to_lowercase() == issue_type_str)
    });
}

/// Filter blocked issues by priority.
fn filter_by_priority(issues: &mut Vec<BlockedIssue>, priorities: &[String]) {
    if priorities.is_empty() {
        return;
    }
    let parsed: Vec<crate::model::Priority> = priorities
        .iter()
        .filter_map(|p| std::str::FromStr::from_str(p).ok())
        .collect();

    if parsed.is_empty() {
        return;
    }

    issues.retain(|bi| {
        parsed
            .iter()
            .any(|&p| p == bi.issue.priority)
    });
}

fn print_text_output(
    blocked_issues: &[BlockedIssue],
    verbose: bool,
    storage: &crate::storage::SqliteStorage,
) {
    if blocked_issues.is_empty() {
        println!("No blocked issues.");
        return;
    }

    println!("Blocked Issues ({} total):\n", blocked_issues.len());

    for (i, bi) in blocked_issues.iter().enumerate() {
        let priority = bi.issue.priority.0;
        println!(
            "{}. [{}] P{} {}",
            i + 1,
            bi.issue.id,
            priority,
            bi.issue.title
        );

        if verbose {
            println!("   Blocked by:");
            for blocker_ref in &bi.blocked_by {
                // blocker_ref format is "id:status", extract just the id for lookup
                let blocker_id = blocker_id_from_ref(blocker_ref);
                if let Ok(Some(blocker)) = storage.get_issue(blocker_id) {
                    println!(
                        "     \u{2022} {}: {} [P{}] [{}]",
                        blocker_id, blocker.title, blocker.priority.0, blocker.status
                    );
                } else {
                    println!("     \u{2022} {blocker_ref} (not found)");
                }
            }
        } else {
            let count = bi.blocked_by.len();
            let issue_word = if count == 1 { "issue" } else { "issues" };
            println!(
                "   Blocked by: {} ({} {})",
                bi.blocked_by.join(", "),
                count,
                issue_word
            );
        }
        println!();
    }
}

fn blocker_id_from_ref(blocker_ref: &str) -> &str {
    // Split from the right to preserve external IDs containing ':'
    blocker_ref
        .rsplit_once(':')
        .map_or(blocker_ref, |(prefix, _)| prefix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::BlockedArgs;
    use crate::logging::init_test_logging;
    use crate::model::{Issue, IssueType, Priority, Status};
    use chrono::{TimeZone, Utc};
    use tracing::info;

    fn make_issue(id: &str, title: &str, priority: i32, issue_type: IssueType) -> Issue {
        Issue {
            id: id.to_string(),
            content_hash: None,
            title: title.to_string(),
            description: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: Status::Open,
            priority: Priority(priority),
            issue_type,
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_at: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
            created_by: None,
            updated_at: Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap(),
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
        }
    }

    fn make_blocked_issue(
        id: &str,
        title: &str,
        priority: i32,
        blocker_count: usize,
    ) -> BlockedIssue {
        BlockedIssue {
            issue: make_issue(id, title, priority, IssueType::Task),
            blocked_by_count: blocker_count,
            blocked_by: (0..blocker_count).map(|i| format!("blocker-{i}")).collect(),
        }
    }

    #[test]
    fn test_blocked_args_defaults() {
        init_test_logging();
        info!("test_blocked_args_defaults: starting");
        // Note: Default::default() gives 0 for limit; clap sets 50 at parse time
        let args = BlockedArgs::default();
        assert_eq!(args.limit, 0); // Rust Default, not clap default
        assert!(!args.detailed);
        assert!(args.type_.is_empty());
        assert!(args.priority.is_empty());
        assert!(args.label.is_empty());
        assert!(!args.robot);
        info!("test_blocked_args_defaults: assertions passed");
    }

    #[test]
    fn test_sort_by_priority_then_blocker_count() {
        init_test_logging();
        info!("test_sort_by_priority_then_blocker_count: starting");
        let mut issues = vec![
            make_blocked_issue("a", "P2 few blockers", 2, 1),
            make_blocked_issue("b", "P1 few blockers", 1, 1),
            make_blocked_issue("c", "P1 many blockers", 1, 5),
            make_blocked_issue("d", "P0 critical", 0, 2),
        ];

        sort_blocked_issues(&mut issues);

        // Should be sorted: P0 first, then P1 (more blockers first), then P2
        assert_eq!(issues[0].issue.id, "d"); // P0
        assert_eq!(issues[1].issue.id, "c"); // P1, 5 blockers
        assert_eq!(issues[2].issue.id, "b"); // P1, 1 blocker
        assert_eq!(issues[3].issue.id, "a"); // P2
        info!("test_sort_by_priority_then_blocker_count: assertions passed");
    }

    #[test]
    fn test_filter_by_type_empty_keeps_all() {
        init_test_logging();
        info!("test_filter_by_type_empty_keeps_all: starting");
        let mut issues = vec![
            BlockedIssue {
                issue: make_issue("a", "Bug", 2, IssueType::Bug),
                blocked_by_count: 1,
                blocked_by: vec!["x".to_string()],
            },
            BlockedIssue {
                issue: make_issue("b", "Task", 2, IssueType::Task),
                blocked_by_count: 1,
                blocked_by: vec!["y".to_string()],
            },
        ];

        filter_by_type(&mut issues, &[]);
        assert_eq!(issues.len(), 2);
        info!("test_filter_by_type_empty_keeps_all: assertions passed");
    }

    #[test]
    fn test_filter_by_type_filters_correctly() {
        init_test_logging();
        info!("test_filter_by_type_filters_correctly: starting");
        let mut issues = vec![
            BlockedIssue {
                issue: make_issue("a", "Bug", 2, IssueType::Bug),
                blocked_by_count: 1,
                blocked_by: vec!["x".to_string()],
            },
            BlockedIssue {
                issue: make_issue("b", "Task", 2, IssueType::Task),
                blocked_by_count: 1,
                blocked_by: vec!["y".to_string()],
            },
            BlockedIssue {
                issue: make_issue("c", "Feature", 2, IssueType::Feature),
                blocked_by_count: 1,
                blocked_by: vec!["z".to_string()],
            },
        ];

        filter_by_type(&mut issues, &["bug".to_string()]);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].issue.id, "a");
        info!("test_filter_by_type_filters_correctly: assertions passed");
    }

    #[test]
    fn test_filter_by_type_case_insensitive() {
        init_test_logging();
        info!("test_filter_by_type_case_insensitive: starting");
        let mut issues = vec![BlockedIssue {
            issue: make_issue("a", "Bug", 2, IssueType::Bug),
            blocked_by_count: 1,
            blocked_by: vec!["x".to_string()],
        }];

        filter_by_type(&mut issues, &["BUG".to_string()]);
        assert_eq!(issues.len(), 1);

        let mut issues2 = vec![BlockedIssue {
            issue: make_issue("a", "Bug", 2, IssueType::Bug),
            blocked_by_count: 1,
            blocked_by: vec!["x".to_string()],
        }];

        filter_by_type(&mut issues2, &["Bug".to_string()]);
        assert_eq!(issues2.len(), 1);
        info!("test_filter_by_type_case_insensitive: assertions passed");
    }

    #[test]
    fn test_filter_by_type_multiple_types() {
        init_test_logging();
        info!("test_filter_by_type_multiple_types: starting");
        let mut issues = vec![
            BlockedIssue {
                issue: make_issue("a", "Bug", 2, IssueType::Bug),
                blocked_by_count: 1,
                blocked_by: vec!["x".to_string()],
            },
            BlockedIssue {
                issue: make_issue("b", "Task", 2, IssueType::Task),
                blocked_by_count: 1,
                blocked_by: vec!["y".to_string()],
            },
            BlockedIssue {
                issue: make_issue("c", "Feature", 2, IssueType::Feature),
                blocked_by_count: 1,
                blocked_by: vec!["z".to_string()],
            },
        ];

        filter_by_type(&mut issues, &["bug".to_string(), "feature".to_string()]);
        assert_eq!(issues.len(), 2);
        let ids: Vec<_> = issues.iter().map(|i| i.issue.id.as_str()).collect();
        assert!(ids.contains(&"a"));
        assert!(ids.contains(&"c"));
        info!("test_filter_by_type_multiple_types: assertions passed");
    }

    #[test]
    fn test_filter_by_priority_empty_keeps_all() {
        init_test_logging();
        info!("test_filter_by_priority_empty_keeps_all: starting");
        let mut issues = vec![
            make_blocked_issue("a", "P0", 0, 1),
            make_blocked_issue("b", "P2", 2, 1),
            make_blocked_issue("c", "P4", 4, 1),
        ];

        filter_by_priority(&mut issues, &[]);
        assert_eq!(issues.len(), 3);
        info!("test_filter_by_priority_empty_keeps_all: assertions passed");
    }

    #[test]
    fn test_filter_by_priority_single() {
        init_test_logging();
        info!("test_filter_by_priority_single: starting");
        let mut issues = vec![
            make_blocked_issue("a", "P0", 0, 1),
            make_blocked_issue("b", "P2", 2, 1),
            make_blocked_issue("c", "P4", 4, 1),
        ];

        filter_by_priority(&mut issues, &[2]);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].issue.id, "b");
        info!("test_filter_by_priority_single: assertions passed");
    }

    #[test]
    fn test_filter_by_priority_multiple() {
        init_test_logging();
        info!("test_filter_by_priority_multiple: starting");
        let mut issues = vec![
            make_blocked_issue("a", "P0", 0, 1),
            make_blocked_issue("b", "P2", 2, 1),
            make_blocked_issue("c", "P4", 4, 1),
        ];

        filter_by_priority(&mut issues, &[0, 4]);
        assert_eq!(issues.len(), 2);
        let ids: Vec<_> = issues.iter().map(|i| i.issue.id.as_str()).collect();
        assert!(ids.contains(&"a"));
        assert!(ids.contains(&"c"));
        info!("test_filter_by_priority_multiple: assertions passed");
    }
}
