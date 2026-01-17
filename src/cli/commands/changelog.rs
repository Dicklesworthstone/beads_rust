//! Changelog command implementation.
//!
//! Generates release notes from closed issues since a given date or git reference.
//! Groups issues by type and sorts by priority within each group.

use crate::cli::ChangelogArgs;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::model::{Issue, Status};
use crate::storage::ListFilters;
use crate::util::time::{parse_flexible_timestamp, parse_relative_time};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::BTreeMap;
use std::process::Command;

/// Changelog output structure.
#[derive(Serialize, Debug)]
pub struct ChangelogOutput {
    /// Start date for the changelog period.
    pub since: String,
    /// End date for the changelog period (now).
    pub until: String,
    /// Total number of closed issues in the period.
    pub total_closed: usize,
    /// Issues grouped by type.
    pub groups: Vec<ChangelogGroup>,
}

/// A group of issues by type.
#[derive(Serialize, Debug)]
pub struct ChangelogGroup {
    /// Issue type (feature, bug, task, etc.).
    pub issue_type: String,
    /// Human-readable label for the type.
    pub label: String,
    /// Issues in this group, sorted by priority.
    pub issues: Vec<ChangelogEntry>,
}

/// A single changelog entry.
#[derive(Serialize, Debug)]
pub struct ChangelogEntry {
    pub id: String,
    pub title: String,
    pub priority: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<String>,
}

/// Execute changelog generation.
///
/// # Errors
///
/// Returns an error if config loading, git lookup, or storage access fails.
pub fn execute(args: &ChangelogArgs, json: bool, cli: &config::CliOverrides) -> Result<()> {
    let beads_dir = config::discover_beads_dir(None)?;
    let config::OpenStorageResult { storage, .. } = config::open_storage_with_cli(&beads_dir, cli)?;

    let (since_dt, since_label) = resolve_since(args)?;
    let until = Utc::now();

    let filters = ListFilters {
        statuses: Some(vec![Status::Closed]),
        include_closed: true,
        ..Default::default()
    };
    let issues = storage.list_issues(&filters)?;

    let mut grouped: BTreeMap<String, Vec<Issue>> = BTreeMap::new();
    for issue in issues {
        if let Some(since_dt) = since_dt {
            let Some(closed_at) = issue.closed_at else {
                continue;
            };
            if closed_at < since_dt {
                continue;
            }
        }
        grouped
            .entry(issue.issue_type.as_str().to_string())
            .or_default()
            .push(issue);
    }

    let mut groups = Vec::new();
    for (issue_type, mut items) in grouped {
        items.sort_by_key(|issue| issue.priority);
        let issues = items
            .into_iter()
            .map(|issue| ChangelogEntry {
                id: issue.id,
                title: issue.title,
                priority: issue.priority.to_string(),
                closed_at: issue.closed_at.map(|dt| dt.to_rfc3339()),
            })
            .collect();

        groups.push(ChangelogGroup {
            issue_type: issue_type.clone(),
            label: issue_type,
            issues,
        });
    }

    let total_closed = groups.iter().map(|g| g.issues.len()).sum();
    let output = ChangelogOutput {
        since: since_label,
        until: until.to_rfc3339(),
        total_closed,
        groups,
    };

    if json || args.robot {
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    println!(
        "Changelog since {} ({} closed issues):",
        output.since, total_closed
    );
    for group in &output.groups {
        println!();
        println!("{}:", group.label);
        for entry in &group.issues {
            println!("- [{}] {} {}", entry.priority, entry.id, entry.title);
        }
    }

    Ok(())
}

fn resolve_since(args: &ChangelogArgs) -> Result<(Option<DateTime<Utc>>, String)> {
    if let Some(tag) = args.since_tag.as_deref() {
        let dt = git_ref_date(tag)?;
        return Ok((Some(dt), dt.to_rfc3339()));
    }
    if let Some(commit) = args.since_commit.as_deref() {
        let dt = git_ref_date(commit)?;
        return Ok((Some(dt), dt.to_rfc3339()));
    }
    if let Some(since) = args.since.as_deref() {
        if let Some(dt) = parse_relative_time(since) {
            return Ok((Some(dt), dt.to_rfc3339()));
        }
        let dt = parse_flexible_timestamp(since, "since")?;
        return Ok((Some(dt), dt.to_rfc3339()));
    }
    Ok((None, "all".to_string()))
}

fn git_ref_date(reference: &str) -> Result<DateTime<Utc>> {
    let output = Command::new("git")
        .args(["show", "-s", "--format=%cI", reference])
        .output()
        .map_err(|e| BeadsError::Config(format!("Failed to run git: {e}")))?;

    if !output.status.success() {
        return Err(BeadsError::Config(format!(
            "Failed to resolve git reference: {reference}"
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stamp = stdout.trim();
    let dt = DateTime::parse_from_rfc3339(stamp)
        .map_err(|e| BeadsError::Config(format!("Invalid git date: {e}")))?
        .with_timezone(&Utc);
    Ok(dt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    #[test]
    fn test_resolve_since_rfc3339() {
        let args = ChangelogArgs {
            since: Some("2023-01-01T00:00:00Z".to_string()),
            ..Default::default()
        };
        let (dt, label) = resolve_since(&args).unwrap();
        assert_eq!(dt.unwrap(), Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap());
        assert_eq!(label, "2023-01-01T00:00:00+00:00");
    }

    #[test]
    fn test_resolve_since_relative() {
        let args = ChangelogArgs {
            since: Some("-1d".to_string()),
            ..Default::default()
        };
        let (dt, _) = resolve_since(&args).unwrap();
        let expected = Utc::now() - Duration::days(1);
        let actual = dt.unwrap();
        // Allow small delta
        assert!(actual < Utc::now());
        assert!(actual > expected - Duration::seconds(5));
    }

    #[test]
    fn test_resolve_since_none() {
        let args = ChangelogArgs::default();
        let (dt, label) = resolve_since(&args).unwrap();
        assert!(dt.is_none());
        assert_eq!(label, "all");
    }
}
