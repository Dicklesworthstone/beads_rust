//! `br blocked` command implementation.
//!
//! Lists blocked issues from the `blocked_issues_cache`.

use crate::cli::BlockedArgs;
use crate::config::{CliOverrides, discover_beads_dir, open_storage};
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

    validate_priority_range(&args.priority)?;

    let beads_dir = discover_beads_dir(None)?;
    let (storage, _paths) =
        open_storage(&beads_dir, overrides.db.as_ref(), overrides.lock_timeout)?;

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
        print_text_output(&blocked_issues, args.detailed, &storage);
    }

    Ok(())
}

fn validate_priority_range(priorities: &[u8]) -> Result<()> {
    for &priority in priorities {
        if priority > 4 {
            return Err(BeadsError::InvalidPriority {
                priority: i32::from(priority),
            });
        }
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
fn filter_by_priority(issues: &mut Vec<BlockedIssue>, priorities: &[u8]) {
    if priorities.is_empty() {
        return;
    }
    issues.retain(|bi| {
        priorities
            .iter()
            .any(|&p| i32::from(p) == bi.issue.priority.0)
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
                let blocker_id = blocker_ref.split(':').next().unwrap_or(blocker_ref);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::BlockedArgs;
    use crate::model::{Issue, IssueType, Priority, Status};
    use chrono::{TimeZone, Utc};

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
        // Note: Default::default() gives 0 for limit; clap sets 50 at parse time
        let args = BlockedArgs::default();
        assert_eq!(args.limit, 0); // Rust Default, not clap default
        assert!(!args.detailed);
        assert!(args.type_.is_empty());
        assert!(args.priority.is_empty());
        assert!(args.label.is_empty());
        assert!(!args.robot);
    }

    #[test]
    fn test_sort_by_priority_then_blocker_count() {
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
    }

    #[test]
    fn test_filter_by_type_empty_keeps_all() {
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
    }

    #[test]
    fn test_filter_by_type_filters_correctly() {
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
    }

    #[test]
    fn test_filter_by_type_case_insensitive() {
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
    }

    #[test]
    fn test_filter_by_type_multiple_types() {
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
    }

    #[test]
    fn test_filter_by_priority_empty_keeps_all() {
        let mut issues = vec![
            make_blocked_issue("a", "P0", 0, 1),
            make_blocked_issue("b", "P2", 2, 1),
            make_blocked_issue("c", "P4", 4, 1),
        ];

        filter_by_priority(&mut issues, &[]);
        assert_eq!(issues.len(), 3);
    }

    #[test]
    fn test_filter_by_priority_single() {
        let mut issues = vec![
            make_blocked_issue("a", "P0", 0, 1),
            make_blocked_issue("b", "P2", 2, 1),
            make_blocked_issue("c", "P4", 4, 1),
        ];

        filter_by_priority(&mut issues, &[2]);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].issue.id, "b");
    }

    #[test]
    fn test_filter_by_priority_multiple() {
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
    }
}
