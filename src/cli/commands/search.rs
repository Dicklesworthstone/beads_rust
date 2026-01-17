//! Search command implementation.
//!
//! Classic bd-style LIKE search across title/description/id with list-like filters.

use crate::cli::{ListArgs, SearchArgs};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::{IssueWithCounts, format_issue_line};
use crate::model::{IssueType, Priority, Status};
use crate::storage::{ListFilters, SqliteStorage};
use chrono::Utc;
use std::collections::HashSet;
use std::path::Path;

/// Execute the search command.
///
/// # Errors
///
/// Returns an error if the query is empty, the database cannot be opened,
/// or the query fails.
pub fn execute(args: &SearchArgs, json: bool, cli: &config::CliOverrides) -> Result<()> {
    let query = args.query.trim();
    if query.is_empty() {
        return Err(BeadsError::Validation {
            field: "query".to_string(),
            reason: "search query cannot be empty".to_string(),
        });
    }

    validate_priority_range(&args.filters.priority)?;

    let beads_dir = config::discover_beads_dir(Some(Path::new(".")))?;
    let (storage, _paths) = config::open_storage(&beads_dir, cli.db.as_ref(), cli.lock_timeout)?;

    let mut filters = build_filters(&args.filters)?;
    let client_filters = needs_client_filters(&args.filters);
    let limit = if client_filters {
        filters.limit.take()
    } else {
        None
    };

    let issues = storage.search_issues(query, &filters)?;
    let issues = if client_filters {
        apply_client_filters(&storage, issues, &args.filters)?
    } else {
        issues
    };

    let mut issues_with_counts: Vec<IssueWithCounts> = issues
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

    apply_sort(&mut issues_with_counts, args.filters.sort.as_deref())?;
    if args.filters.reverse {
        issues_with_counts.reverse();
    }
    if let Some(limit) = limit {
        if limit > 0 && issues_with_counts.len() > limit {
            issues_with_counts.truncate(limit);
        }
    }

    if json {
        let json_output = serde_json::to_string_pretty(&issues_with_counts)?;
        println!("{json_output}");
    } else {
        println!(
            "Found {} issue(s) matching '{}'",
            issues_with_counts.len(),
            query
        );
        for iwc in &issues_with_counts {
            let line = format_issue_line(&iwc.issue);
            println!("{line}");
        }
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

fn build_filters(args: &ListArgs) -> Result<ListFilters> {
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

    // Pre-fetch labels if needed to avoid N+1 query
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
            let empty_labels = Vec::new();
            let labels = labels_map.get(&issue.id).unwrap_or(&empty_labels);
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

fn apply_sort(issues: &mut [IssueWithCounts], sort: Option<&str>) -> Result<()> {
    let Some(sort_key) = sort else {
        return Ok(());
    };

    match sort_key {
        "priority" => issues.sort_by_key(|iwc| iwc.issue.priority),
        "created_at" => issues.sort_by_key(|iwc| iwc.issue.created_at),
        "updated_at" => issues.sort_by_key(|iwc| iwc.issue.updated_at),
        "title" => issues.sort_by(|a, b| a.issue.title.cmp(&b.issue.title)),
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
    use crate::model::{Issue, IssueType, Priority, Status};
    use chrono::{DateTime, TimeZone, Utc};

    fn make_issue(
        id: &str,
        title: &str,
        description: Option<&str>,
        created_at: DateTime<Utc>,
    ) -> Issue {
        Issue {
            id: id.to_string(),
            content_hash: None,
            title: title.to_string(),
            description: description.map(str::to_string),
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_at,
            created_by: None,
            updated_at: created_at,
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

    #[test]
    fn test_search_matches_title_description_id() {
        let mut storage = SqliteStorage::open_memory().expect("db");
        let t1 = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let t2 = Utc.with_ymd_and_hms(2025, 1, 2, 0, 0, 0).unwrap();
        let t3 = Utc.with_ymd_and_hms(2025, 1, 3, 0, 0, 0).unwrap();

        let issue1 = make_issue("bd-001", "Alpha title", None, t1);
        let issue2 = make_issue("bd-002", "Other", Some("alpha desc"), t2);
        let issue3 = make_issue("bd-xyz", "Other", None, t3);

        storage.create_issue(&issue1, "tester").expect("create");
        storage.create_issue(&issue2, "tester").expect("create");
        storage.create_issue(&issue3, "tester").expect("create");

        let filters = ListFilters::default();
        let results = storage.search_issues("alpha", &filters).expect("search");
        let ids: Vec<String> = results.into_iter().map(|issue| issue.id).collect();
        assert!(ids.contains(&"bd-001".to_string()));
        assert!(ids.contains(&"bd-002".to_string()));
        assert!(!ids.contains(&"bd-xyz".to_string()));

        let results = storage.search_issues("xyz", &filters).expect("search");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "bd-xyz");
    }

    #[test]
    fn test_sort_by_title_and_reverse() {
        let t1 = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();
        let t2 = Utc.with_ymd_and_hms(2025, 1, 2, 0, 0, 0).unwrap();

        let issue_a = make_issue("bd-a", "Alpha", None, t1);
        let issue_b = make_issue("bd-b", "Beta", None, t2);

        let mut items = vec![
            IssueWithCounts {
                issue: issue_b,
                dependency_count: 0,
                dependent_count: 0,
            },
            IssueWithCounts {
                issue: issue_a,
                dependency_count: 0,
                dependent_count: 0,
            },
        ];

        apply_sort(&mut items, Some("title")).expect("sort");
        assert_eq!(items[0].issue.title, "Alpha");
        items.reverse();
        assert_eq!(items[0].issue.title, "Beta");
    }
}
