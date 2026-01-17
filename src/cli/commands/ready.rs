//! Ready command implementation.
//!
//! Shows issues ready to work on: unblocked, not deferred, not pinned, not ephemeral.

use crate::cli::{ReadyArgs, SortPolicy};
use crate::config;
use crate::error::Result;
use crate::format::IssueWithCounts;
use crate::model::{IssueType, Priority};
use crate::storage::{ReadyFilters, ReadySortPolicy};
use std::path::Path;
use std::str::FromStr;
use tracing::{debug, info, trace};

/// Execute the ready command.
///
/// # Errors
///
/// Returns an error if the database cannot be opened or the query fails.
pub fn execute(args: &ReadyArgs, json: bool, cli: &config::CliOverrides) -> Result<()> {
    // Open storage
    let beads_dir = config::discover_beads_dir(Some(Path::new(".")))?;
    let (storage, _paths) = config::open_storage(&beads_dir, cli.db.as_ref(), cli.lock_timeout)?;

    let filters = ReadyFilters {
        assignee: args.assignee.clone(),
        unassigned: args.unassigned,
        labels_and: args.label.clone(),
        labels_or: args.label_any.clone(),
        types: parse_types(&args.type_),
        priorities: parse_priorities(&args.priority)?,
        include_deferred: args.include_deferred,
        limit: Some(args.limit),
    };

    let sort_policy = match args.sort {
        SortPolicy::Hybrid => ReadySortPolicy::Hybrid,
        SortPolicy::Priority => ReadySortPolicy::Priority,
        SortPolicy::Oldest => ReadySortPolicy::Oldest,
    };

    info!("Fetching ready issues");
    debug!(filters = ?filters, sort = ?sort_policy, "Applied ready filters");

    // Get ready issues from storage (blocked cache only)
    let ready_issues = storage.get_ready_issues(&filters, sort_policy)?;

    // Convert to IssueWithCounts
    let issues_with_counts: Vec<IssueWithCounts> = ready_issues
        .into_iter()
        .map(|issue| {
            let dependency_count = storage.count_dependencies(&issue.id).unwrap_or(0);
            let dependent_count = storage.count_dependents(&issue.id).unwrap_or(0);
            IssueWithCounts {
                issue,
                dependency_count,
                dependent_count,
            }
        })
        .collect();

    info!(count = issues_with_counts.len(), "Found ready issues");
    for issue in issues_with_counts.iter().take(5) {
        trace!(id = %issue.issue.id, priority = issue.issue.priority.0, "Ready issue");
    }

    // Output
    let use_json = json || args.robot;
    if use_json {
        let json_output = serde_json::to_string_pretty(&issues_with_counts)?;
        println!("{json_output}");
    } else if issues_with_counts.is_empty() {
        println!("No issues ready to work on.");
    } else {
        println!(
            "Ready to work ({} issue{}):\n",
            issues_with_counts.len(),
            if issues_with_counts.len() == 1 {
                ""
            } else {
                "s"
            }
        );
        for (i, iwc) in issues_with_counts.iter().enumerate() {
            let assignee = iwc.issue.assignee.as_deref().unwrap_or("unassigned");
            println!(
                "{}. [P{}] {} {} ({assignee})",
                i + 1,
                iwc.issue.priority.0,
                iwc.issue.id,
                iwc.issue.title
            );
        }
    }

    Ok(())
}

/// Parse type filter strings to `IssueType` enums.
fn parse_types(types: &[String]) -> Option<Vec<IssueType>> {
    if types.is_empty() {
        return None;
    }
    let parsed: Vec<IssueType> = types.iter().filter_map(|t| t.parse().ok()).collect();
    if parsed.is_empty() {
        None
    } else {
        Some(parsed)
    }
}

/// Parse priority filter strings to Priority values.
fn parse_priorities(priorities: &[String]) -> Result<Option<Vec<Priority>>> {
    if priorities.is_empty() {
        return Ok(None);
    }

    let mut parsed = Vec::with_capacity(priorities.len());
    for p in priorities {
        parsed.push(Priority::from_str(p)?);
    }

    Ok(Some(parsed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_types() {
        let t = parse_types(&["bug".to_string(), "feature".to_string()]);
        assert!(t.is_some());
        let t = t.unwrap();
        assert_eq!(t.len(), 2);
    }

    #[test]
    fn test_parse_priorities() {
        let p = parse_priorities(&["0".to_string(), "P1".to_string(), "2".to_string()])
            .expect("parse priorities")
            .unwrap();
        assert_eq!(p.len(), 3);
        assert_eq!(p[0].0, 0);
        assert_eq!(p[1].0, 1);
        assert_eq!(p[2].0, 2);
    }
}
