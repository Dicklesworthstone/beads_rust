use crate::cli::{CountArgs, CountBy};
use crate::config;
use crate::error::Result;
use crate::model::{IssueType, Priority, Status};
use crate::storage::{ListFilters, SqliteStorage};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Serialize)]
struct CountOutput {
    count: usize,
}

#[derive(Serialize)]
struct CountGroup {
    group: String,
    count: usize,
}

#[derive(Serialize)]
struct CountGroupedOutput {
    total: usize,
    groups: Vec<CountGroup>,
}

/// Execute the count command.
///
/// # Errors
///
/// Returns an error if filters are invalid or the database query fails.
pub fn execute(args: &CountArgs, json: bool, cli: &config::CliOverrides) -> Result<()> {
    let beads_dir = config::discover_beads_dir(None)?;
    let (storage, _paths) = config::open_storage(&beads_dir, cli.db.as_ref(), cli.lock_timeout)?;

    let mut filters = ListFilters::default();
    let statuses = parse_statuses(&args.status)?;
    let types = parse_types(&args.types)?;
    let priorities = parse_priorities(&args.priority)?;

    if !statuses.is_empty() {
        if statuses.iter().any(Status::is_terminal) {
            filters.include_closed = true;
        }
        filters.statuses = Some(statuses);
    }
    if !types.is_empty() {
        filters.types = Some(types);
    }
    if !priorities.is_empty() {
        filters.priorities = Some(priorities);
    }

    filters.assignee.clone_from(&args.assignee);
    filters.unassigned = args.unassigned;
    filters.include_closed = filters.include_closed || args.include_closed;
    filters.include_templates = args.include_templates;
    filters.title_contains.clone_from(&args.title_contains);

    let issues = storage.list_issues(&filters)?;
    let total = issues.len();

    match args.by {
        None => {
            if json {
                let payload = serde_json::to_string(&CountOutput { count: total })?;
                println!("{payload}");
            } else {
                println!("{total}");
            }
        }
        Some(by) => {
            let groups = group_counts(&storage, &issues, by)?;
            if json {
                let payload = serde_json::to_string(&CountGroupedOutput { total, groups })?;
                println!("{payload}");
            } else {
                println!("Total: {total}");
                for group in groups {
                    println!("{}: {}", group.group, group.count);
                }
            }
        }
    }

    Ok(())
}

fn parse_statuses(values: &[String]) -> Result<Vec<Status>> {
    values
        .iter()
        .map(|value| value.parse())
        .collect::<Result<Vec<Status>>>()
}

fn parse_types(values: &[String]) -> Result<Vec<IssueType>> {
    values
        .iter()
        .map(|value| value.parse())
        .collect::<Result<Vec<IssueType>>>()
}

fn parse_priorities(values: &[String]) -> Result<Vec<Priority>> {
    values
        .iter()
        .map(|value| value.parse())
        .collect::<Result<Vec<Priority>>>()
}

fn group_counts(
    storage: &SqliteStorage,
    issues: &[crate::model::Issue],
    by: CountBy,
) -> Result<Vec<CountGroup>> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();

    match by {
        CountBy::Status => {
            for issue in issues {
                let key = issue.status.as_str().to_string();
                *counts.entry(key).or_insert(0) += 1;
            }
        }
        CountBy::Priority => {
            for issue in issues {
                let key = issue.priority.to_string();
                *counts.entry(key).or_insert(0) += 1;
            }
        }
        CountBy::Type => {
            for issue in issues {
                let key = issue.issue_type.as_str().to_string();
                *counts.entry(key).or_insert(0) += 1;
            }
        }
        CountBy::Assignee => {
            for issue in issues {
                let key = issue
                    .assignee
                    .as_deref()
                    .unwrap_or("(unassigned)")
                    .to_string();
                *counts.entry(key).or_insert(0) += 1;
            }
        }
        CountBy::Label => {
            for issue in issues {
                let labels = storage.get_labels(&issue.id)?;
                if labels.is_empty() {
                    *counts.entry("(no labels)".to_string()).or_insert(0) += 1;
                } else {
                    for label in labels {
                        *counts.entry(label).or_insert(0) += 1;
                    }
                }
            }
        }
    }

    Ok(counts
        .into_iter()
        .map(|(group, count)| CountGroup { group, count })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Issue, IssueType, Priority, Status};
    use chrono::Utc;

    fn make_issue(id: &str, status: Status, priority: Priority, issue_type: IssueType) -> Issue {
        Issue {
            id: id.to_string(),
            title: format!("Issue {id}"),
            description: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            status,
            priority,
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
    fn test_group_counts_status() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue1 = make_issue("bd-1", Status::Open, Priority::MEDIUM, IssueType::Task);
        let issue2 = make_issue("bd-2", Status::InProgress, Priority::HIGH, IssueType::Bug);

        storage.create_issue(&issue1, "tester").unwrap();
        storage.create_issue(&issue2, "tester").unwrap();

        let filters = ListFilters {
            include_closed: true,
            include_templates: true,
            ..Default::default()
        };
        let listed_issues = storage.list_issues(&filters).unwrap();
        let groups = group_counts(&storage, &listed_issues, CountBy::Status).unwrap();

        let mut map = BTreeMap::new();
        for group in groups {
            map.insert(group.group, group.count);
        }

        assert_eq!(map.get("open"), Some(&1));
        assert_eq!(map.get("in_progress"), Some(&1));
    }

    #[test]
    fn test_group_counts_label_includes_unlabeled() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue1 = make_issue("bd-1", Status::Open, Priority::MEDIUM, IssueType::Task);
        let issue2 = make_issue("bd-2", Status::Open, Priority::LOW, IssueType::Task);

        storage.create_issue(&issue1, "tester").unwrap();
        storage.create_issue(&issue2, "tester").unwrap();
        storage.add_label("bd-1", "backend", "tester").unwrap();

        let filters = ListFilters {
            include_closed: true,
            include_templates: true,
            ..Default::default()
        };
        let listed_issues = storage.list_issues(&filters).unwrap();
        let groups = group_counts(&storage, &listed_issues, CountBy::Label).unwrap();

        let mut map = BTreeMap::new();
        for group in groups {
            map.insert(group.group, group.count);
        }

        assert_eq!(map.get("backend"), Some(&1));
        assert_eq!(map.get("(no labels)"), Some(&1));
    }
}
