use crate::cli::StaleArgs;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::{StaleIssue, format_status_label, sanitize_terminal_inline};
use crate::model::{Issue, Status};
use crate::output::{OutputContext, OutputMode};
use crate::storage::{ListFilters, SqliteStorage};
use chrono::{DateTime, Duration, Utc};

/// Execute the stale command.
///
/// # Errors
///
/// Returns an error if filters are invalid or the database query fails.
pub fn execute(args: &StaleArgs, cli: &config::CliOverrides, ctx: &OutputContext) -> Result<()> {
    if !(0..=36500).contains(&args.days) {
        return Err(BeadsError::validation(
            "days",
            "must be between 0 and 36500",
        ));
    }

    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    execute_inner(args, ctx, &storage_ctx.storage)
}

/// Execute the stale command using storage that was already opened by the caller.
///
/// # Errors
///
/// Returns an error if filters are invalid or the database query fails.
pub fn execute_with_storage(
    args: &StaleArgs,
    ctx: &OutputContext,
    storage: &SqliteStorage,
) -> Result<()> {
    if !(0..=36500).contains(&args.days) {
        return Err(BeadsError::validation(
            "days",
            "must be between 0 and 36500",
        ));
    }

    execute_inner(args, ctx, storage)
}

fn execute_inner(args: &StaleArgs, ctx: &OutputContext, storage: &SqliteStorage) -> Result<()> {
    let statuses = if args.status.is_empty() {
        Vec::new()
    } else {
        parse_statuses(&args.status)?
    };

    let mut filters = ListFilters::default();
    if statuses.is_empty() {
        filters.include_deferred = true;
    } else {
        if statuses.iter().any(Status::is_terminal) {
            filters.include_closed = true;
        }
        if statuses.contains(&Status::Deferred) {
            filters.include_deferred = true;
        }
        filters.statuses = Some(statuses);
    }

    let now = Utc::now();
    let threshold = now - Duration::days(args.days);
    filters.updated_before = Some(threshold);
    // Sort by updated_at ASC (oldest first) to show most stale items first
    filters.sort = Some("updated_at".to_string());
    filters.reverse = true; // updated_at default is DESC, so reverse gets ASC

    let stale = storage.list_stale_issues_for_command_output(&filters)?;

    // Output based on mode
    if matches!(ctx.mode(), OutputMode::Quiet) {
        return Ok(());
    }

    if matches!(ctx.mode(), OutputMode::Rich) {
        render_stale_rich(&stale, now, args.days, ctx);
    } else if ctx.is_toon() {
        let stale_output = stale_issue_outputs(&stale);
        ctx.toon(&stale_output);
    } else if ctx.is_json() {
        ctx.json_array(stale.iter().map(stale_issue_output));
    } else {
        println!(
            "Stale issues ({} not updated in {}+ days):",
            stale.len(),
            args.days
        );
        for (idx, issue) in stale.iter().enumerate() {
            let days_stale = (now - issue.updated_at).num_days().max(0);
            let status = format_status_label(&issue.status, false);
            let issue_id = stale_display_text(&issue.id);
            if let Some(assignee) = issue.assignee.as_deref() {
                println!(
                    "{}. [{}] {}d {} {} ({assignee})",
                    idx + 1,
                    status,
                    days_stale,
                    issue_id,
                    sanitize_terminal_inline(&issue.title),
                    assignee = sanitize_terminal_inline(assignee)
                );
            } else {
                println!(
                    "{}. [{}] {}d {} {}",
                    idx + 1,
                    status,
                    days_stale,
                    issue_id,
                    sanitize_terminal_inline(&issue.title)
                );
            }
        }
    }

    Ok(())
}

fn stale_issue_outputs(stale: &[Issue]) -> Vec<StaleIssue> {
    stale.iter().map(stale_issue_output).collect()
}

fn stale_issue_output(issue: &Issue) -> StaleIssue {
    StaleIssue {
        created_at: issue.created_at,
        id: issue.id.clone(),
        issue_type: issue.issue_type.clone(),
        priority: issue.priority,
        status: issue.status.clone(),
        title: issue.title.clone(),
        updated_at: issue.updated_at,
        assignee: issue.assignee.clone(),
    }
}

fn parse_statuses(values: &[String]) -> Result<Vec<Status>> {
    values
        .iter()
        .map(|value| value.parse())
        .collect::<Result<Vec<Status>>>()
}

fn render_stale_rich(
    stale: &[Issue],
    now: DateTime<Utc>,
    threshold_days: i64,
    ctx: &OutputContext,
) {
    use rich_rust::Text;

    let theme = ctx.theme();

    if stale.is_empty() {
        let mut text = Text::new("");
        text.append_styled("\u{2728} ", theme.success.clone());
        text.append_styled(
            &format!("No stale issues (threshold: {}+ days)", threshold_days),
            theme.success.clone().bold(),
        );
        ctx.render(&text);
        return;
    }

    // Header
    let mut header = Text::new("");
    header.append_styled("\u{23f3} ", theme.warning.clone());
    header.append_styled("Stale issues", theme.warning.clone().bold());
    header.append_styled(
        &format!(" ({} not updated in {}+ days)", stale.len(), threshold_days),
        theme.dimmed.clone(),
    );
    ctx.render(&header);
    ctx.newline();

    for issue in stale {
        let days_stale = (now - issue.updated_at).num_days().max(0);

        // Staleness coloring: red (>30d), orange (14-30d), yellow (7-14d), dim (<7d)
        // Using theme colors where possible, or falling back to specific logic
        // We can use priority styles as proxies for urgency or define direct colors if needed
        let staleness_style = if days_stale > 30 {
            theme.error.clone().bold()
        } else if days_stale > 14 {
            theme.warning.clone().bold() // Bright yellow/orange
        } else if days_stale > 7 {
            theme.warning.clone()
        } else {
            theme.dimmed.clone()
        };

        // Status style
        let status_style = theme.status_style(&issue.status);

        let mut line = Text::new("");

        // Days stale badge
        line.append_styled(&format!("{:>3}d ", days_stale), staleness_style);

        // Status badge
        line.append_styled(
            &format!("[{}] ", format_status_label(&issue.status, false)),
            status_style,
        );

        // Issue ID
        line.append_styled(&stale_display_text(&issue.id), theme.issue_id.clone());
        line.append(" ");

        // Title
        line.append_styled(
            sanitize_terminal_inline(&issue.title).as_ref(),
            theme.issue_title.clone(),
        );

        // Assignee if present
        if let Some(ref assignee) = issue.assignee {
            line.append_styled(
                &format!(" (@{})", sanitize_terminal_inline(assignee)),
                theme.dimmed.clone(),
            );
        }

        ctx.render(&line);
    }
}

fn stale_display_text(value: &str) -> String {
    sanitize_terminal_inline(value).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IssueType, Priority};
    use tracing::info;

    fn init_logging() {
        crate::logging::init_test_logging();
    }

    fn make_issue(id: &str, updated_at: DateTime<Utc>) -> Issue {
        Issue {
            id: id.to_string(),
            title: format!("Issue {id}"),
            description: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_at: updated_at,
            created_by: None,
            updated_at,
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
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

    #[test]
    fn stale_display_text_escapes_terminal_controls() {
        let rendered = stale_display_text("bd-1\x1b]52;c;bad\x07");

        assert!(!rendered.chars().any(char::is_control));
        assert_eq!(rendered, "bd-1\\u{1b}]52;c;bad\\u{7}");
    }

    #[test]
    fn test_stale_issue_output_iterator_matches_materialized_outputs() {
        let now = Utc::now();
        let issues = vec![
            make_issue("bd-1", now - Duration::days(10)),
            make_issue("bd-2", now - Duration::days(40)),
        ];

        let streamed: Vec<StaleIssue> = issues.iter().map(stale_issue_output).collect();
        let materialized: Vec<StaleIssue> = issues.iter().cloned().map(StaleIssue::from).collect();

        let streamed_json = serde_json::to_vec(&streamed).expect("serialize streamed stale output");
        let materialized_json =
            serde_json::to_vec(&materialized).expect("serialize materialized stale output");
        assert_eq!(streamed_json, materialized_json);

        let helper_json = serde_json::to_vec(&stale_issue_outputs(&issues))
            .expect("serialize helper stale output");
        assert_eq!(helper_json, materialized_json);
    }

    /// Filter and sort stale issues for testing purposes.
    /// Note: In production, this filtering is done by the storage layer via SQL.
    fn filter_stale_issues(
        issues: Vec<Issue>,
        now: DateTime<Utc>,
        threshold_days: i64,
    ) -> Vec<Issue> {
        let threshold = now - Duration::days(threshold_days);
        let mut stale: Vec<Issue> = issues
            .into_iter()
            .filter(|i| i.updated_at <= threshold)
            .collect();
        // Sort by updated_at ascending (oldest first)
        stale.sort_by_key(|a| a.updated_at);
        stale
    }

    #[test]
    fn test_filter_stale_issues_orders_oldest_first() {
        init_logging();
        info!("test_filter_stale_issues_orders_oldest_first: starting");
        let now = Utc::now();
        let issues = vec![
            make_issue("bd-1", now - Duration::days(10)),
            make_issue("bd-2", now - Duration::days(40)),
            make_issue("bd-3", now - Duration::days(60)),
            make_issue("bd-4", now - Duration::days(30)),
        ];

        let stale = filter_stale_issues(issues, now, 30);
        assert_eq!(stale.len(), 3);
        assert_eq!(stale[0].id, "bd-3");
        assert_eq!(stale[1].id, "bd-2");
        assert_eq!(stale[2].id, "bd-4");
        info!("test_filter_stale_issues_orders_oldest_first: assertions passed");
    }
}
