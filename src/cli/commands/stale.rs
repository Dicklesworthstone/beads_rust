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
pub fn execute(
    args: &StaleArgs,
    json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
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
    let issues = storage.list_issues(&filters)?;
    let stale = filter_stale_issues(issues, now, args.days);

    // Output based on mode
    if matches!(ctx.mode(), OutputMode::Rich) {
        render_stale_rich(&stale, now, args.days);
    } else if json {
        // Convert to StaleIssue for bd-compatible JSON output
        let stale_output: Vec<StaleIssue> = stale.iter().map(StaleIssue::from).collect();
        let payload = serde_json::to_string(&stale_output)?;
        println!("{payload}");
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

fn filter_stale_issues(mut issues: Vec<Issue>, now: DateTime<Utc>, days: i64) -> Vec<Issue> {
    let threshold = now - Duration::days(days);
    issues.retain(|issue| issue.updated_at <= threshold);
    issues.sort_by_key(|issue| issue.updated_at);
    issues
}

fn render_stale_rich(stale: &[Issue], now: DateTime<Utc>, threshold_days: i64) {
    use rich_rust::Text;
    use rich_rust::prelude::*;

    fn color(name: &str) -> Color {
        Color::parse(name).unwrap_or_default()
    }

    let console = Console::default();

    if stale.is_empty() {
        let mut text = Text::new("");
        text.append_styled("\u{2728} ", Style::new().color(color("green")));
        text.append_styled(
            &format!("No stale issues (threshold: {}+ days)", threshold_days),
            Style::new().bold().color(color("green")),
        );
        console.print_renderable(&text);
        return;
    }

    // Header
    let mut header = Text::new("");
    header.append_styled("\u{23f3} ", Style::new().color(color("yellow")));
    header.append_styled("Stale issues", Style::new().bold().color(color("yellow")));
    header.append_styled(
        &format!(" ({} not updated in {}+ days)", stale.len(), threshold_days),
        Style::new().dim(),
    );
    console.print_renderable(&header);
    console.print("");

    for issue in stale {
        let days_stale = (now - issue.updated_at).num_days().max(0);

        // Staleness coloring: red (>30d), orange (14-30d), yellow (7-14d), dim (<7d)
        let staleness_style = if days_stale > 30 {
            Style::new().bold().color(color("red"))
        } else if days_stale > 14 {
            Style::new().color(color("bright_yellow"))
        } else if days_stale > 7 {
            Style::new().color(color("yellow"))
        } else {
            Style::new().dim()
        };

        // Status style
        let status_style = match issue.status {
            Status::Open => Style::new().color(color("blue")),
            Status::InProgress => Style::new().color(color("yellow")),
            _ => Style::new().dim(),
        };

        let mut line = Text::new("");

        // Days stale badge
        line.append_styled(&format!("{:>3}d ", days_stale), staleness_style.clone());

        // Status badge
        line.append_styled(&format!("[{}] ", issue.status.as_str()), status_style);

        // Issue ID
        line.append_styled(&issue.id, Style::new().bold().color(color("cyan")));
        line.append(" ");

        // Title
        line.append(&issue.title);

        // Assignee if present
        if let Some(ref assignee) = issue.assignee {
            line.append_styled(&format!(" (@{})", assignee), Style::new().dim());
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
