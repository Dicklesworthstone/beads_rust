use crate::cli::{CountArgs, CountBy};
use crate::config;
use crate::error::Result;
use crate::model::{IssueType, Priority, Status};
use crate::output::{OutputContext, OutputMode};
use crate::storage::{ListFilters, SqliteStorage};
use rich_rust::prelude::*;
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
pub fn execute(
    args: &CountArgs,
    _json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir(None)?;
    let storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    let storage = &storage_ctx.storage;

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

    let by = args.by.or(if args.by_status {
        Some(CountBy::Status)
    } else if args.by_priority {
        Some(CountBy::Priority)
    } else if args.by_type {
        Some(CountBy::Type)
    } else if args.by_assignee {
        Some(CountBy::Assignee)
    } else if args.by_label {
        Some(CountBy::Label)
    } else {
        None
    });

    match by {
        None => {
            if ctx.is_json() {
                ctx.json_pretty(&CountOutput { count: total });
            } else if matches!(ctx.mode(), OutputMode::Rich) {
                render_count_simple_rich(total, ctx);
            } else {
                println!("{total}");
            }
        }
        Some(by) => {
            let groups = group_counts(storage, &issues, by)?;
            if ctx.is_json() {
                ctx.json_pretty(&CountGroupedOutput { total, groups });
            } else if matches!(ctx.mode(), OutputMode::Rich) {
                render_count_grouped_rich(total, &groups, by, ctx);
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

// ─────────────────────────────────────────────────────────────
// Rich Output Rendering
// ─────────────────────────────────────────────────────────────

/// Render simple count with rich formatting.
fn render_count_simple_rich(total: usize, ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");
    content.append_styled("Total: ", theme.dimmed.clone());
    content.append_styled(&total.to_string(), theme.emphasis.clone());
    content.append("\n");

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled("Issue Count", theme.panel_title.clone()))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

/// Render grouped count with rich formatting.
fn render_count_grouped_rich(
    total: usize,
    groups: &[CountGroup],
    by: CountBy,
    ctx: &OutputContext,
) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");
    content.append_styled("Total: ", theme.dimmed.clone());
    content.append_styled(&total.to_string(), theme.emphasis.clone());
    content.append("\n\n");

    // Find longest group name for alignment
    let max_len = groups.iter().map(|g| g.group.len()).max().unwrap_or(0);

    for group in groups {
        let padded = format!("{:<width$}", group.group, width = max_len);
        content.append_styled(&padded, theme.accent.clone());
        content.append("  ");
        content.append_styled(&group.count.to_string(), theme.emphasis.clone());
        content.append("\n");
    }

    let title = match by {
        CountBy::Status => "Issue Counts by Status",
        CountBy::Priority => "Issue Counts by Priority",
        CountBy::Type => "Issue Counts by Type",
        CountBy::Assignee => "Issue Counts by Assignee",
        CountBy::Label => "Issue Counts by Label",
    };

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled(title, theme.panel_title.clone()))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
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
            let issue_ids: Vec<String> = issues.iter().map(|i| i.id.clone()).collect();
            let labels_map = storage.get_labels_for_issues(&issue_ids)?;

            for issue in issues {
                if let Some(labels) = labels_map.get(&issue.id) {
                    for label in labels {
                        *counts.entry(label.clone()).or_insert(0) += 1;
                    }
                } else {
                    *counts.entry("(no labels)".to_string()).or_insert(0) += 1;
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
    use tracing::info;

    fn init_logging() {
        crate::logging::init_test_logging();
    }

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
        init_logging();
        info!("test_group_counts_status: starting");
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
        info!("test_group_counts_status: assertions passed");
    }

    #[test]
    fn test_group_counts_label_includes_unlabeled() {
        init_logging();
        info!("test_group_counts_label_includes_unlabeled: starting");
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
        info!("test_group_counts_label_includes_unlabeled: assertions passed");
    }
}
