use crate::cli::{CountArgs, CountBy};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::sanitize_terminal_inline;
use crate::model::Status;
use crate::output::OutputContext;
use crate::storage::{ListFilters, SqliteStorage, StatsIssueRow};
use rich_rust::prelude::*;
use serde::Serialize;
use std::collections::BTreeMap;

const COUNT_SMALL_RESULT_THRESHOLD: usize = 64;

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
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    execute_inner(args, ctx, &storage_ctx.storage)
}

/// Execute the count command using storage that was already opened by the caller.
///
/// # Errors
///
/// Returns an error if filters are invalid or the database query fails.
pub fn execute_with_storage(
    args: &CountArgs,
    ctx: &OutputContext,
    storage: &SqliteStorage,
) -> Result<()> {
    execute_inner(args, ctx, storage)
}

fn execute_inner(args: &CountArgs, ctx: &OutputContext, storage: &SqliteStorage) -> Result<()> {
    let mut filters = ListFilters::default();
    let statuses = parse_trimmed_values(&args.status)?;
    let types = parse_trimmed_values(&args.types)?;
    let priorities = parse_trimmed_values(&args.priority)?;

    // `--status all` is the same meta-value `br lint` accepts: no status
    // filter, every status included (beads_rust-6ilv).
    if super::status_filter_requests_all(&args.status) {
        filters.include_closed = true;
        filters.include_deferred = true;
    } else if !statuses.is_empty() {
        if statuses.iter().any(Status::is_terminal) {
            filters.include_closed = true;
        }
        if statuses.contains(&Status::Deferred) {
            filters.include_deferred = true;
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

    let by = resolve_count_grouping(args)?;

    match by {
        None => {
            let total = storage.count_issues_with_filters(&filters)?;
            if ctx.is_quiet() {
                return Ok(());
            }

            if ctx.is_toon() {
                ctx.toon(&CountOutput { count: total });
            } else if ctx.is_json() {
                ctx.json_pretty(&CountOutput { count: total });
            } else if ctx.is_rich() {
                render_count_simple_rich(total, ctx);
            } else {
                println!("{total}");
            }
        }
        Some(by) => {
            let (total, groups) = group_counts_for_filters(storage, &filters, by)?;
            if ctx.is_quiet() {
                return Ok(());
            }

            if ctx.is_toon() {
                ctx.toon(&CountGroupedOutput { total, groups });
            } else if ctx.is_json() {
                ctx.json_pretty(&CountGroupedOutput { total, groups });
            } else if ctx.is_rich() {
                render_count_grouped_rich(total, &groups, by, ctx);
            } else {
                println!("Total: {total}");
                for group in groups {
                    println!(
                        "{}: {}",
                        sanitize_terminal_inline(&group.group),
                        group.count
                    );
                }
            }
        }
    }

    Ok(())
}

fn resolve_count_grouping(args: &CountArgs) -> Result<Option<CountBy>> {
    let mut selected = Vec::with_capacity(6);

    if let Some(by) = args.by {
        selected.push(("--by", by));
    }
    if args.by_status {
        selected.push(("--by-status", CountBy::Status));
    }
    if args.by_priority {
        selected.push(("--by-priority", CountBy::Priority));
    }
    if args.by_type {
        selected.push(("--by-type", CountBy::Type));
    }
    if args.by_assignee {
        selected.push(("--by-assignee", CountBy::Assignee));
    }
    if args.by_label {
        selected.push(("--by-label", CountBy::Label));
    }

    if selected.len() > 1 {
        let mut flags = String::new();
        for (index, (flag, _)) in selected.iter().enumerate() {
            if index > 0 {
                flags.push_str(", ");
            }
            flags.push_str(flag);
        }
        return Err(BeadsError::Validation {
            field: "by".to_string(),
            reason: format!("only one count grouping selector can be specified; got {flags}"),
        });
    }

    Ok(selected.first().map(|(_, by)| *by))
}

// ─────────────────────────────────────────────────────────────
// Rich Output Rendering
// ─────────────────────────────────────────────────────────────

/// Render simple count with rich formatting.
fn render_count_simple_rich(total: usize, ctx: &OutputContext) {
    let _console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");
    content.append_styled("Total: ", theme.dimmed.clone());
    content.append_styled(&total.to_string(), theme.emphasis.clone());
    content.append("\n");

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled("Issue Count", theme.panel_title.clone()))
        .box_style(theme.box_style);

    ctx.render(&panel);
}

/// Render grouped count with rich formatting.
fn render_count_grouped_rich(
    total: usize,
    groups: &[CountGroup],
    by: CountBy,
    ctx: &OutputContext,
) {
    let _console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");
    content.append_styled("Total: ", theme.dimmed.clone());
    content.append_styled(&total.to_string(), theme.emphasis.clone());
    content.append("\n\n");

    // Find longest group name for alignment
    let sanitized_groups = groups
        .iter()
        .map(|group| {
            (
                sanitize_terminal_inline(&group.group).into_owned(),
                group.count,
            )
        })
        .collect::<Vec<_>>();
    let max_len = sanitized_groups
        .iter()
        .map(|(group, _)| group.len())
        .max()
        .unwrap_or(0);

    for (group, count) in sanitized_groups {
        let padded = format!("{group:<width$}", width = max_len);
        content.append_styled(&padded, theme.accent.clone());
        content.append("  ");
        content.append_styled(&count.to_string(), theme.emphasis.clone());
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

    ctx.render(&panel);
}

fn parse_trimmed_values<T>(values: &[String]) -> Result<Vec<T>>
where
    T: std::str::FromStr<Err = crate::error::BeadsError>,
{
    values
        .iter()
        .map(|value| value.trim().parse())
        .collect::<Result<Vec<T>>>()
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
            let mut labels_map = storage.get_labels_for_issues(&issue_ids)?;

            for issue in issues {
                if let Some(labels) = labels_map.remove(&issue.id) {
                    if labels.is_empty() {
                        *counts.entry("(no labels)".to_string()).or_insert(0) += 1;
                    } else {
                        for label in labels {
                            *counts.entry(label).or_insert(0) += 1;
                        }
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

fn group_counts_for_filters(
    storage: &SqliteStorage,
    filters: &ListFilters,
    by: CountBy,
) -> Result<(usize, Vec<CountGroup>)> {
    if let Some(result) = default_visible_group_counts(storage, filters, by)? {
        return Ok(result);
    }

    if by == CountBy::Label {
        let total = storage.count_issues_with_filters(filters)?;
        if total <= COUNT_SMALL_RESULT_THRESHOLD {
            let issues = storage.list_issues(filters)?;
            let groups = group_counts(storage, &issues, by)?;
            return Ok((total, groups));
        }
        let groups = storage
            .count_labels_with_filters(filters)?
            .into_iter()
            .map(|(group, count)| CountGroup { group, count })
            .collect();
        return Ok((total, groups));
    }

    if should_use_full_issue_rows_for_count(filters) {
        let issues = storage.list_issues(filters)?;
        let groups = group_counts(storage, &issues, by)?;
        return Ok((issues.len(), groups));
    }

    if !filters.include_closed {
        let total = storage.count_issues_with_filters(filters)?;
        if total <= COUNT_SMALL_RESULT_THRESHOLD {
            let issues = storage.list_issues(filters)?;
            let groups = group_counts(storage, &issues, by)?;
            return Ok((total, groups));
        }
    }

    let rows = storage.list_stats_issues()?;
    let rows = rows
        .into_iter()
        .filter(|issue| stats_row_matches_count_filters(issue, filters))
        .collect::<Vec<_>>();
    let groups = group_stats_counts(storage, &rows, by)?;
    Ok((rows.len(), groups))
}

fn default_visible_group_counts(
    storage: &SqliteStorage,
    filters: &ListFilters,
    by: CountBy,
) -> Result<Option<(usize, Vec<CountGroup>)>> {
    if !is_default_visible_group_count(filters) {
        return Ok(None);
    }

    if by == CountBy::Label {
        let (total, rows) = storage.count_default_visible_labels()?;
        let groups = rows
            .into_iter()
            .map(|(group, count)| CountGroup { group, count })
            .collect();
        return Ok(Some((total, groups)));
    }

    let rows = storage.list_stats_issues()?;
    let rows = rows
        .into_iter()
        .filter(|issue| stats_row_matches_count_filters(issue, filters))
        .collect::<Vec<_>>();
    let groups = group_stats_counts(storage, &rows, by)?;
    Ok(Some((rows.len(), groups)))
}

fn is_default_visible_group_count(filters: &ListFilters) -> bool {
    filters.statuses.as_ref().is_none_or(Vec::is_empty)
        && filters.types.as_ref().is_none_or(Vec::is_empty)
        && filters.priorities.as_ref().is_none_or(Vec::is_empty)
        && filters.assignee.is_none()
        && !filters.unassigned
        && !filters.include_closed
        && !filters.include_deferred
        && !filters.include_templates
        && filters.title_contains.is_none()
        && filters.limit.is_none()
        && filters.offset.is_none()
        && filters.sort.is_none()
        && !filters.reverse
        && filters.labels.as_ref().is_none_or(Vec::is_empty)
        && filters.labels_or.as_ref().is_none_or(Vec::is_empty)
        && filters.updated_before.is_none()
        && filters.updated_after.is_none()
}

fn should_use_full_issue_rows_for_count(filters: &ListFilters) -> bool {
    filters.title_contains.is_some() || filters.assignee.as_deref() == Some("")
}

fn stats_row_matches_count_filters(issue: &StatsIssueRow, filters: &ListFilters) -> bool {
    if let Some(statuses) = &filters.statuses
        && !statuses.is_empty()
        && !statuses.contains(&issue.status)
    {
        return false;
    }

    if let Some(types) = &filters.types
        && !types.is_empty()
        && !types.contains(&issue.issue_type)
    {
        return false;
    }

    if let Some(priorities) = &filters.priorities
        && !priorities.is_empty()
        && !priorities.contains(&issue.priority)
    {
        return false;
    }

    if let Some(assignee) = &filters.assignee
        && issue.assignee.as_deref() != Some(assignee.as_str())
    {
        return false;
    }

    if filters.unassigned && issue.assignee.is_some() {
        return false;
    }

    if !filters.include_closed {
        if matches!(issue.status, Status::Closed | Status::Tombstone) {
            return false;
        }
        if !filters.include_deferred && issue.status == Status::Deferred {
            return false;
        }
    } else if filters.statuses.as_ref().is_none_or(Vec::is_empty)
        && issue.status == Status::Tombstone
    {
        return false;
    }

    filters.include_templates || !issue.is_template
}

fn group_stats_counts(
    storage: &SqliteStorage,
    issues: &[StatsIssueRow],
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
            let mut labels_map = storage.get_labels_for_issues(&issue_ids)?;

            for issue in issues {
                if let Some(labels) = labels_map.remove(&issue.id) {
                    if labels.is_empty() {
                        *counts.entry("(no labels)".to_string()).or_insert(0) += 1;
                    } else {
                        for label in labels {
                            *counts.entry(label).or_insert(0) += 1;
                        }
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

    fn count_args() -> CountArgs {
        CountArgs {
            by: None,
            by_status: false,
            by_priority: false,
            by_type: false,
            by_assignee: false,
            by_label: false,
            status: vec![],
            types: vec![],
            priority: vec![],
            assignee: None,
            unassigned: false,
            include_closed: false,
            include_templates: false,
            title_contains: None,
        }
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
            bypassed_policy: None,
            bypass_reason: None,
            policy_gates_fired: None,
            due_at: None,
            defer_until: None,
            external_ref: None,
            source_system: None,
            source_repo: None,
            source_repo_path: None,
            agent_context: None,
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

    fn groups_to_map(groups: Vec<CountGroup>) -> BTreeMap<String, usize> {
        groups
            .into_iter()
            .map(|group| (group.group, group.count))
            .collect()
    }

    #[test]
    fn test_group_counts_status() -> Result<()> {
        init_logging();
        info!("test_group_counts_status: starting");
        let mut storage = SqliteStorage::open_memory()?;
        let issue1 = make_issue("bd-1", Status::Open, Priority::MEDIUM, IssueType::Task);
        let issue2 = make_issue("bd-2", Status::InProgress, Priority::HIGH, IssueType::Bug);

        storage.create_issue(&issue1, "tester")?;
        storage.create_issue(&issue2, "tester")?;

        let filters = ListFilters {
            include_closed: true,
            include_templates: true,
            ..Default::default()
        };
        let listed_issues = storage.list_issues(&filters)?;
        let groups = group_counts(&storage, &listed_issues, CountBy::Status)?;

        let map = groups_to_map(groups);

        assert_eq!(map.get("open"), Some(&1));
        assert_eq!(map.get("in_progress"), Some(&1));
        info!("test_group_counts_status: assertions passed");

        Ok(())
    }

    #[test]
    fn test_group_counts_label_includes_unlabeled() -> Result<()> {
        init_logging();
        info!("test_group_counts_label_includes_unlabeled: starting");
        let mut storage = SqliteStorage::open_memory()?;
        let issue1 = make_issue("bd-1", Status::Open, Priority::MEDIUM, IssueType::Task);
        let issue2 = make_issue("bd-2", Status::Open, Priority::LOW, IssueType::Task);

        storage.create_issue(&issue1, "tester")?;
        storage.create_issue(&issue2, "tester")?;
        storage.add_label("bd-1", "backend", "tester")?;

        let filters = ListFilters {
            include_closed: true,
            include_templates: true,
            ..Default::default()
        };
        let listed_issues = storage.list_issues(&filters)?;
        let groups = group_counts(&storage, &listed_issues, CountBy::Label)?;

        let map = groups_to_map(groups);

        assert_eq!(map.get("backend"), Some(&1));
        assert_eq!(map.get("(no labels)"), Some(&1));

        let (total, aggregate_groups) =
            group_counts_for_filters(&storage, &filters, CountBy::Label)?;
        let aggregate_map = groups_to_map(aggregate_groups);
        assert_eq!(total, 2);
        assert_eq!(aggregate_map.get("backend"), Some(&1));
        assert_eq!(aggregate_map.get("(no labels)"), Some(&1));
        info!("test_group_counts_label_includes_unlabeled: assertions passed");

        Ok(())
    }

    #[test]
    fn test_lean_group_counts_match_full_issue_filtering() -> Result<()> {
        init_logging();
        info!("test_lean_group_counts_match_full_issue_filtering: starting");
        let mut storage = SqliteStorage::open_memory()?;

        let task = make_issue("bd-1", Status::Open, Priority::HIGH, IssueType::Task);
        let bug = make_issue("bd-2", Status::Open, Priority::HIGH, IssueType::Bug);
        let mut closed = make_issue("bd-3", Status::Closed, Priority::HIGH, IssueType::Task);
        closed.closed_at = Some(Utc::now());
        let deferred = make_issue("bd-4", Status::Deferred, Priority::HIGH, IssueType::Task);
        let mut template = make_issue("bd-5", Status::Open, Priority::HIGH, IssueType::Task);
        template.is_template = true;

        storage.create_issue(&task, "tester")?;
        storage.create_issue(&bug, "tester")?;
        storage.create_issue(&closed, "tester")?;
        storage.create_issue(&deferred, "tester")?;
        storage.create_issue(&template, "tester")?;

        let filters = ListFilters {
            types: Some(vec![IssueType::Task]),
            priorities: Some(vec![Priority::HIGH]),
            ..Default::default()
        };
        let full_issues = storage.list_issues(&filters)?;
        let expected = groups_to_map(group_counts(&storage, &full_issues, CountBy::Status)?);
        let (total, actual_groups) = group_counts_for_filters(&storage, &filters, CountBy::Status)?;

        assert_eq!(total, full_issues.len());
        assert_eq!(groups_to_map(actual_groups), expected);
        info!("test_lean_group_counts_match_full_issue_filtering: assertions passed");

        Ok(())
    }

    #[test]
    fn test_default_visible_group_counts_match_default_filtering() -> Result<()> {
        init_logging();
        info!("test_default_visible_group_counts_match_default_filtering: starting");
        let mut storage = SqliteStorage::open_memory()?;

        let task = make_issue("bd-1", Status::Open, Priority::HIGH, IssueType::Task);
        let bug = make_issue("bd-2", Status::InProgress, Priority::MEDIUM, IssueType::Bug);
        let mut closed = make_issue("bd-3", Status::Closed, Priority::LOW, IssueType::Task);
        closed.closed_at = Some(Utc::now());
        let deferred = make_issue(
            "bd-4",
            Status::Deferred,
            Priority::CRITICAL,
            IssueType::Feature,
        );
        let mut template = make_issue("bd-5", Status::Open, Priority::HIGH, IssueType::Task);
        template.is_template = true;

        storage.create_issue(&task, "tester")?;
        storage.create_issue(&bug, "tester")?;
        storage.create_issue(&closed, "tester")?;
        storage.create_issue(&deferred, "tester")?;
        storage.create_issue(&template, "tester")?;

        let filters = ListFilters::default();
        let full_issues = storage.list_issues(&filters)?;
        let expected = groups_to_map(group_counts(&storage, &full_issues, CountBy::Status)?);
        let (total, actual_groups) = group_counts_for_filters(&storage, &filters, CountBy::Status)?;

        assert_eq!(total, full_issues.len());
        assert_eq!(groups_to_map(actual_groups), expected);
        info!("test_default_visible_group_counts_match_default_filtering: assertions passed");

        Ok(())
    }

    #[test]
    fn test_default_visible_label_counts_match_default_filtering() -> Result<()> {
        init_logging();
        info!("test_default_visible_label_counts_match_default_filtering: starting");
        let mut storage = SqliteStorage::open_memory()?;

        let labeled = make_issue("bd-1", Status::Open, Priority::HIGH, IssueType::Task);
        let unlabeled = make_issue("bd-2", Status::InProgress, Priority::MEDIUM, IssueType::Bug);
        let mut closed = make_issue("bd-3", Status::Closed, Priority::LOW, IssueType::Task);
        closed.closed_at = Some(Utc::now());
        let deferred = make_issue(
            "bd-4",
            Status::Deferred,
            Priority::CRITICAL,
            IssueType::Feature,
        );
        let mut template = make_issue("bd-5", Status::Open, Priority::HIGH, IssueType::Task);
        template.is_template = true;

        storage.create_issue(&labeled, "tester")?;
        storage.create_issue(&unlabeled, "tester")?;
        storage.create_issue(&closed, "tester")?;
        storage.create_issue(&deferred, "tester")?;
        storage.create_issue(&template, "tester")?;
        storage.add_label("bd-1", "backend", "tester")?;
        storage.add_label("bd-3", "closed-only", "tester")?;
        storage.add_label("bd-4", "deferred-only", "tester")?;
        storage.add_label("bd-5", "template-only", "tester")?;

        let filters = ListFilters::default();
        let full_issues = storage.list_issues(&filters)?;
        let expected = groups_to_map(group_counts(&storage, &full_issues, CountBy::Label)?);
        let (total, actual_groups) = group_counts_for_filters(&storage, &filters, CountBy::Label)?;

        assert_eq!(total, full_issues.len());
        assert_eq!(groups_to_map(actual_groups), expected);
        assert_eq!(expected.get("backend"), Some(&1));
        assert_eq!(expected.get("(no labels)"), Some(&1));
        assert!(!expected.contains_key("closed-only"));
        assert!(!expected.contains_key("deferred-only"));
        assert!(!expected.contains_key("template-only"));
        info!("test_default_visible_label_counts_match_default_filtering: assertions passed");

        Ok(())
    }

    #[test]
    fn test_lean_group_counts_include_labels_for_filtered_rows() -> Result<()> {
        init_logging();
        info!("test_lean_group_counts_include_labels_for_filtered_rows: starting");
        let mut storage = SqliteStorage::open_memory()?;
        let mut assigned = make_issue("bd-1", Status::Open, Priority::MEDIUM, IssueType::Task);
        assigned.assignee = Some("agent@example.com".to_string());
        let unassigned = make_issue("bd-2", Status::Open, Priority::MEDIUM, IssueType::Task);

        storage.create_issue(&assigned, "tester")?;
        storage.create_issue(&unassigned, "tester")?;
        storage.add_label("bd-1", "backend", "tester")?;
        storage.add_label("bd-2", "docs", "tester")?;

        let filters = ListFilters {
            assignee: Some("agent@example.com".to_string()),
            ..Default::default()
        };
        let full_issues = storage.list_issues(&filters)?;
        let expected = groups_to_map(group_counts(&storage, &full_issues, CountBy::Label)?);
        let (total, actual_groups) = group_counts_for_filters(&storage, &filters, CountBy::Label)?;

        assert_eq!(total, full_issues.len());
        assert_eq!(groups_to_map(actual_groups), expected);
        info!("test_lean_group_counts_include_labels_for_filtered_rows: assertions passed");

        Ok(())
    }

    #[test]
    fn test_parse_count_filters_trim_delimited_whitespace() -> Result<()> {
        init_logging();
        info!("test_parse_count_filters_trim_delimited_whitespace: starting");

        let statuses =
            parse_trimmed_values::<Status>(&["open".to_string(), " closed ".to_string()])?;
        let types =
            parse_trimmed_values::<IssueType>(&["bug".to_string(), " feature ".to_string()])?;
        let priorities = parse_trimmed_values::<Priority>(&["P0".to_string(), " 1 ".to_string()])?;

        assert_eq!(statuses, vec![Status::Open, Status::Closed]);
        assert_eq!(types, vec![IssueType::Bug, IssueType::Feature]);
        assert_eq!(priorities, vec![Priority::CRITICAL, Priority::HIGH]);

        let empty: &[String] = &[];
        assert!(parse_trimmed_values::<Status>(empty)?.is_empty());
        assert!(matches!(
            parse_trimmed_values::<Priority>(&[" nope ".to_string()]),
            Err(crate::error::BeadsError::InvalidPriority { priority }) if priority == "NOPE"
        ));

        info!("test_parse_count_filters_trim_delimited_whitespace: assertions passed");
        Ok(())
    }

    #[test]
    fn test_resolve_count_grouping_selects_single_selector() -> Result<()> {
        init_logging();
        info!("test_resolve_count_grouping_selects_single_selector: starting");

        let args = count_args();
        assert_eq!(resolve_count_grouping(&args)?, None);

        let mut args = count_args();
        args.by = Some(CountBy::Priority);
        assert_eq!(resolve_count_grouping(&args)?, Some(CountBy::Priority));

        let mut args = count_args();
        args.by_assignee = true;
        assert_eq!(resolve_count_grouping(&args)?, Some(CountBy::Assignee));

        info!("test_resolve_count_grouping_selects_single_selector: assertions passed");
        Ok(())
    }

    #[test]
    fn test_resolve_count_grouping_rejects_conflicting_selectors() {
        init_logging();
        info!("test_resolve_count_grouping_rejects_conflicting_selectors: starting");

        let mut args = count_args();
        args.by = Some(CountBy::Status);
        args.by_label = true;

        let err = resolve_count_grouping(&args).expect_err("conflicting selectors should fail");
        assert!(matches!(
            err,
            BeadsError::Validation { ref field, ref reason }
                if field == "by"
                    && reason.contains("--by")
                    && reason.contains("--by-label")
        ));

        info!("test_resolve_count_grouping_rejects_conflicting_selectors: assertions passed");
    }
}
