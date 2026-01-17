//! List command implementation.
//!
//! Primary discovery interface with classic filter semantics and
//! `IssueWithCounts` JSON output.

use crate::cli::ListArgs;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::{IssueWithCounts, TextFormatOptions, format_issue_line_with, terminal_width};
use crate::model::{Issue, IssueType, Priority, Status};
use crate::storage::{ListFilters, SqliteStorage};
use chrono::Utc;
use std::collections::HashSet;
use std::io::IsTerminal;
use std::path::Path;

/// Execute the list command.
///
/// # Errors
///
/// Returns an error if the database cannot be opened or the query fails.
pub fn execute(args: &ListArgs, json: bool, cli: &config::CliOverrides) -> Result<()> {
    validate_priority_range(&args.priority)?;

    // Open storage
    let beads_dir = config::discover_beads_dir(Some(Path::new(".")))?;
    let (storage, _paths) = config::open_storage(&beads_dir, cli.db.as_ref(), cli.lock_timeout)?;
    let config_layer = config::load_config(&beads_dir, Some(&storage), cli)?;
    let use_color = config::should_use_color(&config_layer);
    let max_width = if std::io::stdout().is_terminal() {
        Some(terminal_width())
    } else {
        None
    };
    let format_options = TextFormatOptions {
        use_color,
        max_width,
    };

    // Build filter from args
    let mut filters = build_filters(args)?;
    let client_filters = needs_client_filters(args);
    let limit = if client_filters {
        filters.limit.take()
    } else {
        None
    };

    // Query issues
    let issues = storage.list_issues(&filters)?;
    let mut issues = if client_filters {
        apply_client_filters(&storage, issues, args)?
    } else {
        issues
    };

    // Sort and limit on Issue (before expensive counts)
    apply_sort(&mut issues, args.sort.as_deref())?;
    if args.reverse {
        issues.reverse();
    }
    if let Some(limit) = limit {
        if limit > 0 && issues.len() > limit {
            issues.truncate(limit);
        }
    }

    // Output
    if json {
        // Convert to IssueWithCounts only for JSON
        let issues_with_counts: Vec<IssueWithCounts> = issues
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
        let json_output = serde_json::to_string_pretty(&issues_with_counts)?;
        println!("{json_output}");
    } else if issues.is_empty() {
        println!("No issues found.");
    } else {
        for issue in &issues {
            let line = format_issue_line_with(issue, format_options);
            println!("{line}");
        }
        println!("\n{} issue(s)", issues.len());
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

/// Convert CLI args to storage filter.
fn build_filters(args: &ListArgs) -> Result<ListFilters> {
    // Parse status strings to Status enums
    let statuses = if args.status.is_empty() {
        None
    } else {
        Some(
            args.status
                .iter()
                .map(|s| s.parse())
                .collect::<Result<Vec<Status>>>()?,
        )
    };

    // Parse type strings to IssueType enums
    let types = if args.type_.is_empty() {
        None
    } else {
        Some(
            args.type_
                .iter()
                .map(|t| t.parse())
                .collect::<Result<Vec<IssueType>>>()?,
        )
    };

    // Parse priority values
    let priorities = if args.priority.is_empty() {
        None
    } else {
        let parsed: Vec<Priority> = args
            .priority
            .iter()
            .map(|&p| Priority(i32::from(p)))
            .collect();
        Some(parsed)
    };

    let include_closed = args.all
        || statuses
            .as_ref()
            .is_some_and(|parsed| parsed.iter().any(Status::is_terminal));

    Ok(ListFilters {
        statuses,
        types,
        priorities,
        assignee: args.assignee.clone(),
        unassigned: args.unassigned,
        include_closed,
        include_templates: false,
        title_contains: args.title_contains.clone(),
        limit: args.limit,
    })
}

fn needs_client_filters(args: &ListArgs) -> bool {
    !args.id.is_empty()
        || !args.label.is_empty()
        || !args.label_any.is_empty()
        || args.priority_min.is_some()
        || args.priority_max.is_some()
        || args.desc_contains.is_some()
        || args.notes_contains.is_some()
        || args.sort.is_some()
        || args.reverse
        || args.deferred
        || args.overdue
}

fn apply_client_filters(
    storage: &SqliteStorage,
    issues: Vec<crate::model::Issue>,
    args: &ListArgs,
) -> Result<Vec<crate::model::Issue>> {
    let id_filter: Option<HashSet<&str>> = if args.id.is_empty() {
        None
    } else {
        Some(args.id.iter().map(String::as_str).collect())
    };

    let label_filters = !args.label.is_empty() || !args.label_any.is_empty();

    // Pre-fetch labels if needed to avoid N+1
    let labels_map = if label_filters {
        let issue_ids: Vec<String> = issues.iter().map(|i| i.id.clone()).collect();
        storage.get_labels_for_issues(&issue_ids)?
    } else {
        std::collections::HashMap::new()
    };

    let mut filtered = Vec::new();
    let now = Utc::now();
    let min_priority = args.priority_min.map(i32::from);
    let max_priority = args.priority_max.map(i32::from);
    let desc_needle = args.desc_contains.as_deref().map(str::to_lowercase);
    let notes_needle = args.notes_contains.as_deref().map(str::to_lowercase);
    let include_deferred = args.deferred
        || args
            .status
            .iter()
            .any(|status| status.eq_ignore_ascii_case("deferred"));

    if let Some(min) = min_priority {
        if !(0..=4).contains(&min) {
            return Err(BeadsError::InvalidPriority { priority: min });
        }
    }
    if let Some(max) = max_priority {
        if !(0..=4).contains(&max) {
            return Err(BeadsError::InvalidPriority { priority: max });
        }
    }

    for issue in issues {
        if let Some(ids) = &id_filter {
            if !ids.contains(issue.id.as_str()) {
                continue;
            }
        }

        if let Some(min) = min_priority {
            if issue.priority.0 < min {
                continue;
            }
        }
        if let Some(max) = max_priority {
            if issue.priority.0 > max {
                continue;
            }
        }

        if let Some(ref needle) = desc_needle {
            let haystack = issue.description.as_deref().unwrap_or("").to_lowercase();
            if !haystack.contains(needle) {
                continue;
            }
        }

        if let Some(ref needle) = notes_needle {
            let haystack = issue.notes.as_deref().unwrap_or("").to_lowercase();
            if !haystack.contains(needle) {
                continue;
            }
        }

        if !include_deferred && matches!(issue.status, Status::Deferred) {
            continue;
        }

        if args.overdue {
            let overdue = issue.due_at.is_some_and(|due| due < now) && !issue.status.is_terminal();
            if !overdue {
                continue;
            }
        }

        if label_filters {
            let default_labels = Vec::new();
            let labels = labels_map.get(&issue.id).unwrap_or(&default_labels);
            if !args.label.is_empty() && !args.label.iter().all(|label| labels.contains(label)) {
                continue;
            }
            if !args.label_any.is_empty()
                && !args.label_any.iter().any(|label| labels.contains(label))
            {
                continue;
            }
        }

        filtered.push(issue);
    }

    Ok(filtered)
}

fn apply_sort(issues: &mut [Issue], sort: Option<&str>) -> Result<()> {
    let Some(sort_key) = sort else {
        return Ok(());
    };

    match sort_key {
        "priority" => issues.sort_by_key(|issue| issue.priority),
        "created_at" => issues.sort_by_key(|issue| issue.created_at),
        "updated_at" => issues.sort_by_key(|issue| issue.updated_at),
        "title" => issues.sort_by(|a, b| a.title.cmp(&b.title)),
        _ => {
            return Err(BeadsError::Validation {
                field: "sort".to_string(),
                reason: format!("invalid sort field '{sort_key}'"),
            });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli;

    #[test]
    fn test_build_filters_includes_closed_for_terminal_status() {
        let args = cli::ListArgs {
            status: vec!["closed".to_string()],
            ..Default::default()
        };

        let filters = build_filters(&args).expect("build filters");
        assert!(filters.include_closed);
        assert!(
            filters
                .statuses
                .as_ref()
                .expect("statuses")
                .contains(&Status::Closed)
        );
    }

    #[test]
    fn test_build_filters_parses_priorities() {
        let args = cli::ListArgs {
            priority: vec![0, 2],
            ..Default::default()
        };

        let filters = build_filters(&args).expect("build filters");
        let priorities = filters.priorities.expect("priorities");
        let values: Vec<i32> = priorities.iter().map(|p| p.0).collect();
        assert_eq!(values, vec![0, 2]);
    }

    #[test]
    fn test_needs_client_filters_detects_fields() {
        let args = ListArgs::default();
        assert!(!needs_client_filters(&args));

        let args = cli::ListArgs {
            label: vec!["backend".to_string()],
            ..Default::default()
        };
        assert!(needs_client_filters(&args));

        let args = cli::ListArgs {
            desc_contains: Some("needle".to_string()),
            ..Default::default()
        };
        assert!(needs_client_filters(&args));
    }

    #[test]
    fn test_validate_priority_range_rejects_out_of_range() {
        let err = validate_priority_range(&[9]).unwrap_err();
        match err {
            BeadsError::InvalidPriority { priority } => assert_eq!(priority, 9),
            other => panic!("unexpected error: {other:?}"),
        }
    }
}
