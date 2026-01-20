//! Changelog command implementation.
//!
//! Generates release notes from closed issues since a given date or git reference.
//! Groups issues by type and sorts by priority within each group.

use crate::cli::ChangelogArgs;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::model::{Issue, Status};
use crate::output::{OutputContext, OutputMode};
use crate::storage::ListFilters;
use crate::util::time::{parse_flexible_timestamp, parse_relative_time};
use chrono::{DateTime, Utc};
use rich_rust::prelude::*;
use serde::Serialize;
use std::collections::BTreeMap;
use std::process::Command;
use tracing::debug;

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
pub fn execute(
    args: &ChangelogArgs,
    _json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir(None)?;
    let config::OpenStorageResult { storage, .. } = config::open_storage_with_cli(&beads_dir, cli)?;

    let (since_dt, since_label) = resolve_since(args)?;
    let until = Utc::now();

    debug!(since = %since_label, "Filtering closed issues for changelog");

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
        let label = type_to_header(&issue_type);
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
            label,
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

    debug!(
        total_closed = output.total_closed,
        groups = output.groups.len(),
        "Generated changelog"
    );

    if ctx.is_json() {
        ctx.json_pretty(&output);
        return Ok(());
    }

    if matches!(ctx.mode(), OutputMode::Rich) {
        render_changelog_rich(&output, ctx);
    } else {
        print_text_output(&output);
    }

    Ok(())
}

/// Convert issue type to human-readable changelog header.
fn type_to_header(issue_type: &str) -> String {
    match issue_type {
        "bug" => "Bug Fixes".to_string(),
        "feature" => "Features".to_string(),
        "task" => "Tasks".to_string(),
        "epic" => "Epics".to_string(),
        "chore" => "Maintenance".to_string(),
        "docs" => "Documentation".to_string(),
        "question" => "Questions Resolved".to_string(),
        other => {
            // Capitalize first letter for custom types
            let mut chars = other.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect()
            })
        }
    }
}

/// Print plain text output for changelog.
fn print_text_output(output: &ChangelogOutput) {
    println!(
        "Changelog since {} ({} closed issues):",
        output.since, output.total_closed
    );
    for group in &output.groups {
        println!();
        println!("{}:", group.label);
        for entry in &group.issues {
            println!("- [{}] {} {}", entry.priority, entry.id, entry.title);
        }
    }
}

/// Render changelog with rich formatting.
fn render_changelog_rich(output: &ChangelogOutput, ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");

    // Date range header
    let date_header = format_date_range(&output.since, &output.until);
    content.append_styled(&format!("{date_header}\n\n"), theme.section.clone());

    if output.groups.is_empty() {
        content.append_styled("No closed issues in this period.\n", theme.dimmed.clone());
    } else {
        // Render each group
        for group in &output.groups {
            // Group header with icon
            let icon = type_icon(&group.issue_type);
            content.append_styled(&format!("{icon} {}\n", group.label), theme.emphasis.clone());

            // Issue entries
            for entry in &group.issues {
                content.append_styled("  • ", theme.dimmed.clone());
                content.append(&entry.title);
                content.append_styled(&format!(" ({})", entry.id), theme.issue_id.clone());
                content.append("\n");
            }
            content.append("\n");
        }
    }

    // Footer with total count
    content.append_styled(
        &format!(
            "Closed: {} issue{}",
            output.total_closed,
            if output.total_closed == 1 { "" } else { "s" }
        ),
        theme.success.clone(),
    );

    // Wrap in panel
    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled("Changelog", theme.panel_title.clone()))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

/// Format the date range header.
fn format_date_range(since: &str, until: &str) -> String {
    // Try to parse and format nicely, fall back to raw strings
    let since_fmt = format_date_brief(since);
    let until_fmt = format_date_brief(until);
    format!("{since_fmt} → {until_fmt}")
}

/// Format a date string briefly (YYYY-MM-DD or original if parse fails).
fn format_date_brief(date_str: &str) -> String {
    if date_str == "all" {
        return "all time".to_string();
    }
    // Try to parse RFC3339 and extract just the date portion
    if let Ok(dt) = DateTime::parse_from_rfc3339(date_str) {
        return dt.format("%Y-%m-%d").to_string();
    }
    date_str.to_string()
}

/// Get an icon for issue type.
fn type_icon(issue_type: &str) -> &'static str {
    match issue_type {
        "bug" => "\u{1f41b}",     // 🐛
        "feature" => "\u{2728}",  // ✨
        "task" => "\u{2705}",     // ✅
        "epic" => "\u{1f3c6}",    // 🏆
        "chore" => "\u{1f9f9}",   // 🧹
        "docs" => "\u{1f4da}",    // 📚
        "question" => "\u{2753}", // ❓
        _ => "\u{1f4cb}",         // 📋
    }
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
        assert_eq!(
            dt.unwrap(),
            Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap()
        );
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

    #[test]
    fn test_type_to_header() {
        assert_eq!(type_to_header("bug"), "Bug Fixes");
        assert_eq!(type_to_header("feature"), "Features");
        assert_eq!(type_to_header("task"), "Tasks");
        assert_eq!(type_to_header("epic"), "Epics");
        assert_eq!(type_to_header("chore"), "Maintenance");
        assert_eq!(type_to_header("docs"), "Documentation");
        assert_eq!(type_to_header("question"), "Questions Resolved");
        // Custom types get capitalized
        assert_eq!(type_to_header("custom"), "Custom");
        assert_eq!(type_to_header("refactor"), "Refactor");
    }

    #[test]
    fn test_type_icon() {
        assert_eq!(type_icon("bug"), "\u{1f41b}");
        assert_eq!(type_icon("feature"), "\u{2728}");
        assert_eq!(type_icon("task"), "\u{2705}");
        assert_eq!(type_icon("epic"), "\u{1f3c6}");
        assert_eq!(type_icon("chore"), "\u{1f9f9}");
        assert_eq!(type_icon("docs"), "\u{1f4da}");
        assert_eq!(type_icon("question"), "\u{2753}");
        // Unknown types get clipboard
        assert_eq!(type_icon("custom"), "\u{1f4cb}");
    }

    #[test]
    fn test_format_date_brief() {
        assert_eq!(format_date_brief("all"), "all time");
        assert_eq!(format_date_brief("2024-01-15T10:30:00+00:00"), "2024-01-15");
        assert_eq!(format_date_brief("2024-01-15T10:30:00Z"), "2024-01-15");
        // Invalid format returns original
        assert_eq!(format_date_brief("invalid"), "invalid");
    }

    #[test]
    fn test_format_date_range() {
        let result = format_date_range("all", "2024-01-22T00:00:00Z");
        assert!(result.contains("all time"));
        assert!(result.contains("2024-01-22"));
        assert!(result.contains("→"));
    }

    #[test]
    fn test_changelog_grouping() {
        // Test that ChangelogOutput can be constructed properly
        let output = ChangelogOutput {
            since: "2024-01-01T00:00:00Z".to_string(),
            until: "2024-01-22T00:00:00Z".to_string(),
            total_closed: 3,
            groups: vec![
                ChangelogGroup {
                    issue_type: "bug".to_string(),
                    label: "Bug Fixes".to_string(),
                    issues: vec![ChangelogEntry {
                        id: "bd-abc1".to_string(),
                        title: "Fix auth timeout".to_string(),
                        priority: "P1".to_string(),
                        closed_at: Some("2024-01-15T00:00:00Z".to_string()),
                    }],
                },
                ChangelogGroup {
                    issue_type: "feature".to_string(),
                    label: "Features".to_string(),
                    issues: vec![
                        ChangelogEntry {
                            id: "bd-def2".to_string(),
                            title: "Add dark mode".to_string(),
                            priority: "P2".to_string(),
                            closed_at: Some("2024-01-16T00:00:00Z".to_string()),
                        },
                        ChangelogEntry {
                            id: "bd-ghi3".to_string(),
                            title: "User preferences".to_string(),
                            priority: "P2".to_string(),
                            closed_at: Some("2024-01-17T00:00:00Z".to_string()),
                        },
                    ],
                },
            ],
        };

        assert_eq!(output.groups.len(), 2);
        assert_eq!(output.groups[0].issues.len(), 1);
        assert_eq!(output.groups[1].issues.len(), 2);
        assert_eq!(output.total_closed, 3);
    }

    #[test]
    fn test_json_serialization() {
        let output = ChangelogOutput {
            since: "all".to_string(),
            until: "2024-01-22T00:00:00Z".to_string(),
            total_closed: 1,
            groups: vec![ChangelogGroup {
                issue_type: "bug".to_string(),
                label: "Bug Fixes".to_string(),
                issues: vec![ChangelogEntry {
                    id: "bd-test".to_string(),
                    title: "Test issue".to_string(),
                    priority: "P1".to_string(),
                    closed_at: None,
                }],
            }],
        };

        let json_str = serde_json::to_string_pretty(&output).unwrap();
        assert!(json_str.contains("\"since\": \"all\""));
        assert!(json_str.contains("\"total_closed\": 1"));
        assert!(json_str.contains("Bug Fixes"));
        assert!(json_str.contains("bd-test"));
        // closed_at should be omitted when None
        assert!(!json_str.contains("closed_at"));
    }

    #[test]
    fn test_empty_changelog() {
        let output = ChangelogOutput {
            since: "all".to_string(),
            until: "2024-01-22T00:00:00Z".to_string(),
            total_closed: 0,
            groups: vec![],
        };

        assert!(output.groups.is_empty());
        assert_eq!(output.total_closed, 0);
    }
}
