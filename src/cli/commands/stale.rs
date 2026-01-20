use crate::cli::StaleArgs;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::StaleIssue;
use crate::model::{Issue, Status};
use crate::output::{OutputContext, OutputMode};
use crate::storage::ListFilters;
use chrono::{DateTime, Duration, Utc};

/// Execute the stale command.
///
/// # Errors
///
/// Returns an error if filters are invalid or the database query fails.
pub fn execute(args: &StaleArgs, cli: &config::CliOverrides, ctx: &OutputContext) -> Result<()> {
    if args.days < 0 {
        return Err(BeadsError::validation("days", "must be >= 0"));
    }

    let beads_dir = config::discover_beads_dir(None)?;
    let storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    let storage = &storage_ctx.storage;

    let statuses = if args.status.is_empty() {
        vec![Status::Open, Status::InProgress]
    } else {
        parse_statuses(&args.status)?
    };

    let mut filters = ListFilters::default();
    if statuses.iter().any(Status::is_terminal) {
        filters.include_closed = true;
    }
    filters.statuses = Some(statuses);

    let now = Utc::now();
    let threshold = now - Duration::days(args.days);
    filters.updated_before = Some(threshold);
    // Sort by updated_at ASC (oldest first) to show most stale items first
    filters.sort = Some("updated_at".to_string());
    filters.reverse = true; // updated_at default is DESC, so reverse gets ASC

    let stale = storage.list_issues(&filters)?;

    // Output based on mode
    if matches!(ctx.mode(), OutputMode::Rich) {
        render_stale_rich(&stale, now, args.days, ctx);
    } else if ctx.is_json() {
        // Convert to StaleIssue for bd-compatible JSON output
        let stale_output: Vec<StaleIssue> = stale.iter().map(StaleIssue::from).collect();
        ctx.json(&stale_output);
    } else {
        println!(
            "Stale issues ({} not updated in {}+ days):",
            stale.len(),
            args.days
        );
        for (idx, issue) in stale.iter().enumerate() {
            let days_stale = (now - issue.updated_at).num_days().max(0);
            let status = issue.status.as_str();
            if let Some(assignee) = issue.assignee.as_deref() {
                println!(
                    "{}. [{}] {}d {} {} ({assignee})",
                    idx + 1,
                    status,
                    days_stale,
                    issue.id,
                    issue.title
                );
            } else {
                println!(
                    "{}. [{}] {}d {} {}",
                    idx + 1,
                    status,
                    days_stale,
                    issue.id,
                    issue.title
                );
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

fn render_stale_rich(
    stale: &[Issue],
    now: DateTime<Utc>,
    threshold_days: i64,
    ctx: &OutputContext,
) {
    use rich_rust::Text;
    use rich_rust::prelude::*;

    let console = Console::default();
    let theme = ctx.theme();

    if stale.is_empty() {
        let mut text = Text::new("");
        text.append_styled("\u{2728} ", theme.success.clone());
        text.append_styled(
            &format!("No stale issues (threshold: {}+ days)", threshold_days),
            theme.success.clone().bold(),
        );
        console.print_renderable(&text);
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
    console.print_renderable(&header);
    console.print("");

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
        line.append_styled(&format!("[{}] ", issue.status.as_str()), status_style);

        // Issue ID
        line.append_styled(&issue.id, theme.issue_id.clone());
        line.append(" ");

        // Title
        line.append_styled(&issue.title, theme.issue_title.clone());

        // Assignee if present
        if let Some(ref assignee) = issue.assignee {
            line.append_styled(&format!(" (@{})", assignee), theme.dimmed.clone());
        }

        console.print_renderable(&line);
    }
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
            .filter(|i| i.updated_at < threshold)
            .collect();
        // Sort by updated_at ascending (oldest first)
        stale.sort_by(|a, b| a.updated_at.cmp(&b.updated_at));
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
        ];

        let stale = filter_stale_issues(issues, now, 30);
        assert_eq!(stale.len(), 2);
        assert_eq!(stale[0].id, "bd-3");
        assert_eq!(stale[1].id, "bd-2");
        info!("test_filter_stale_issues_orders_oldest_first: assertions passed");
    }
}
