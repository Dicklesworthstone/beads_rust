use crate::cli::StaleArgs;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::model::{Issue, Status};
use crate::storage::ListFilters;
use chrono::{DateTime, Duration, Utc};

/// Execute the stale command.
///
/// # Errors
///
/// Returns an error if filters are invalid or the database query fails.
pub fn execute(args: &StaleArgs, json: bool, cli: &config::CliOverrides) -> Result<()> {
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

    if json {
        let payload = serde_json::to_string(&stale)?;
        println!("{payload}");
        return Ok(());
    }

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IssueType, Priority};

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
    }
}
