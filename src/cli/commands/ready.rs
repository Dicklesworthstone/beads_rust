//! Ready command implementation.
//!
//! Shows issues ready to work on: unblocked, not deferred, not pinned, not ephemeral.

use crate::cli::{ReadyArgs, SortPolicy};
use crate::config;
use crate::error::Result;
use crate::format::{ReadyIssue, format_priority_badge, terminal_width, truncate_title};
use crate::model::{IssueType, Priority};
use crate::storage::{ReadyFilters, ReadySortPolicy};
use std::io::IsTerminal;
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
    let storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    let storage = &storage_ctx.storage;

    let config_layer = config::load_config(&beads_dir, Some(storage), cli)?;
    let external_db_paths = config::external_project_db_paths(&config_layer, &beads_dir);
    let use_color = config::should_use_color(&config_layer);
    let max_width = if std::io::stdout().is_terminal() {
        Some(terminal_width())
    } else {
        None
    };

    let filters = ReadyFilters {
        assignee: args.assignee.clone(),
        unassigned: args.unassigned,
        labels_and: args.label.clone(),
        labels_or: args.label_any.clone(),
        types: parse_types(&args.type_),
        priorities: parse_priorities(&args.priority)?,
        include_deferred: args.include_deferred,
        // Fetch all candidates to allow post-filtering of external blockers
        limit: None,
    };

    let sort_policy = match args.sort {
        SortPolicy::Hybrid => ReadySortPolicy::Hybrid,
        SortPolicy::Priority => ReadySortPolicy::Priority,
        SortPolicy::Oldest => ReadySortPolicy::Oldest,
    };

    info!("Fetching ready issues");
    debug!(filters = ?filters, sort = ?sort_policy, "Applied ready filters");

    // Get ready issues from storage (blocked cache only)
    let mut ready_issues = storage.get_ready_issues(&filters, sort_policy)?;

    let external_statuses =
        storage.resolve_external_dependency_statuses(&external_db_paths, true)?;
    let external_blockers = storage.external_blockers(&external_statuses)?;
    if !external_blockers.is_empty() {
        ready_issues.retain(|issue| !external_blockers.contains_key(&issue.id));
    }

    // Apply limit after external filtering
    if args.limit > 0 && ready_issues.len() > args.limit {
        ready_issues.truncate(args.limit);
    }

    info!(count = ready_issues.len(), "Found ready issues");
    for issue in ready_issues.iter().take(5) {
        trace!(id = %issue.id, priority = issue.priority.0, "Ready issue");
    }

    // Output
    let use_json = json || args.robot;
    if use_json {
        // Use ReadyIssue for bd parity (excludes compaction_level, original_size, dependency_count, dependent_count)
        let ready_output: Vec<ReadyIssue> = ready_issues.iter().map(ReadyIssue::from).collect();
        let json_output = serde_json::to_string_pretty(&ready_output)?;
        println!("{json_output}");
    } else if ready_issues.is_empty() {
        println!("No issues ready to work on.");
    } else {
        println!(
            "Ready to work ({} issue{}):\n",
            ready_issues.len(),
            if ready_issues.len() == 1 { "" } else { "s" }
        );
        for (i, issue) in ready_issues.iter().enumerate() {
            let assignee = issue.assignee.as_deref().unwrap_or("unassigned");
            let line = format_ready_line(i + 1, issue, assignee, use_color, max_width);
            println!("{line}");
        }
    }

    Ok(())
}

fn format_ready_line(
    index: usize,
    issue: &crate::model::Issue,
    assignee: &str,
    use_color: bool,
    max_width: Option<usize>,
) -> String {
    let priority_badge_plain = format!("[{}]", crate::format::format_priority(&issue.priority));
    let prefix_plain = format!("{index}. {priority_badge_plain} {} ", issue.id);
    let suffix_plain = format!(" ({assignee})");
    let title = max_width.map_or_else(
        || issue.title.clone(),
        |width| {
            let max_title =
                width.saturating_sub(prefix_plain.chars().count() + suffix_plain.chars().count());
            truncate_title(&issue.title, max_title)
        },
    );

    let priority_badge = format_priority_badge(&issue.priority, use_color);
    format!(
        "{index}. {priority_badge} {} {title}{suffix_plain}",
        issue.id
    )
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
    use tracing::info;

    fn init_logging() {
        crate::logging::init_test_logging();
    }

    #[test]
    fn test_parse_types() {
        init_logging();
        info!("test_parse_types: starting");
        let t = parse_types(&["bug".to_string(), "feature".to_string()]);
        assert!(t.is_some());
        let t = t.unwrap();
        assert_eq!(t.len(), 2);
        info!("test_parse_types: assertions passed");
    }

    #[test]
    fn test_parse_priorities() {
        init_logging();
        info!("test_parse_priorities: starting");
        let p = parse_priorities(&["0".to_string(), "P1".to_string(), "2".to_string()])
            .expect("parse priorities")
            .unwrap();
        assert_eq!(p.len(), 3);
        assert_eq!(p[0].0, 0);
        assert_eq!(p[1].0, 1);
        assert_eq!(p[2].0, 2);
        info!("test_parse_priorities: assertions passed");
    }
}
