//! orphans command implementation.
//!
//! Scans git commits for issue ID references and identifies issues
//! that are still `open/in_progress` but referenced in commits.

use crate::cli::OrphansArgs;
use crate::cli::commands::close::{self, CloseArgs};
use crate::config;
use crate::error::Result;
use crate::model::{Issue, Status};
use crate::output::{IssueTable, IssueTableColumns, OutputContext};
use crate::storage::ListFilters;
use regex::Regex;
use rich_rust::prelude::*;
use serde::Serialize;
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use tracing::{debug, trace};

/// Output format for orphan issues.
#[derive(Debug, Clone, Serialize)]
pub struct OrphanIssue {
    pub issue_id: String,
    pub title: String,
    pub status: String,
    pub latest_commit: String,
    pub latest_commit_message: String,
}

/// Execute the orphans command.
///
/// Scans git log for issue ID references and returns `open/in_progress`
/// issues that have been referenced in commits.
///
/// # Errors
///
/// Returns an error only for unexpected failures. Returns empty list
/// (not error) when git/DB is unavailable.
#[allow(clippy::too_many_lines)]
pub fn execute(
    args: &OrphansArgs,
    json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    // Try to discover beads directory - return empty if not found
    let Ok(beads_dir) = config::discover_beads_dir(None) else {
        output_empty(json || args.robot, ctx);
        return Ok(());
    };

    // Try to open storage - return empty if not found
    let Ok(storage_ctx) = config::open_storage_with_cli(&beads_dir, cli) else {
        output_empty(json || args.robot, ctx);
        return Ok(());
    };
    let storage = &storage_ctx.storage;

    // Get issue prefix from config
    let config_layer = config::load_config(&beads_dir, Some(storage), cli)?;
    let prefix = config::id_config_from_layer(&config_layer).prefix;

    // Check if we're in a git repo by running git rev-parse
    if !is_git_repo() {
        output_empty(json || args.robot, ctx);
        return Ok(());
    }

    // Get git log and extract issue references
    let Ok(commit_refs) = get_git_commit_refs(&prefix) else {
        output_empty(json || args.robot, ctx);
        return Ok(());
    };

    trace!(
        commit_refs = commit_refs.len(),
        "Retrieved commit references"
    );

    if commit_refs.is_empty() {
        output_empty(json || args.robot, ctx);
        return Ok(());
    }

    // Get all open and in_progress issues
    let filters = ListFilters {
        statuses: Some(vec![Status::Open, Status::InProgress]),
        ..Default::default()
    };
    let issues = storage.list_issues(&filters)?;
    debug!(total_issues = issues.len(), "Scanning for orphaned issues");

    // Build a map of issue_id -> (commit_hash, commit_message)
    // We already have latest-first from git log, so first occurrence wins
    let mut issue_commits: HashMap<String, (String, String)> = HashMap::new();
    for (commit_hash, commit_msg, issue_id) in &commit_refs {
        issue_commits
            .entry(issue_id.clone())
            .or_insert_with(|| (commit_hash.clone(), commit_msg.clone()));
    }

    // Find orphans: issues that are referenced in commits but still open
    let mut orphans: Vec<OrphanIssue> = Vec::new();
    let mut orphan_issues: Vec<Issue> = Vec::new();
    let mut context_snippets: HashMap<String, String> = HashMap::new();

    for issue in issues {
        if let Some((commit_hash, commit_msg)) = issue_commits.get(&issue.id) {
            let issue_id = issue.id.clone();
            let title = issue.title.clone();
            let status = issue.status.as_str().to_string();

            if args.details {
                context_snippets.insert(issue_id.clone(), format!("{commit_hash} {commit_msg}"));
            }

            orphans.push(OrphanIssue {
                issue_id,
                title,
                status,
                latest_commit: commit_hash.clone(),
                latest_commit_message: commit_msg.clone(),
            });
            orphan_issues.push(issue);
        }
    }

    // Sort by issue_id for consistent output
    orphans.sort_by(|a, b| a.issue_id.cmp(&b.issue_id));
    orphan_issues.sort_by(|a, b| a.id.cmp(&b.id));
    debug!(orphan_count = orphans.len(), "Scanning for orphaned issues");

    if json || args.robot {
        println!("{}", serde_json::to_string_pretty(&orphans)?);
        return Ok(());
    }

    if orphans.is_empty() {
        output_empty(false, ctx);
        return Ok(());
    }

    if ctx.is_rich() {
        let columns = IssueTableColumns {
            id: true,
            priority: true,
            status: false,
            issue_type: false,
            title: true,
            assignee: false,
            labels: false,
            created: false,
            updated: false,
            context: args.details,
        };

        let mut table = IssueTable::new(&orphan_issues, ctx.theme())
            .columns(columns)
            .title(format!("Orphan Issues ({})", orphan_issues.len()));

        if args.details {
            table = table.context_snippets(context_snippets);
        }

        let table = table.build();
        ctx.render(&table);
        ctx.print(
            "\nSuggestion: Assign these to an epic or set a parent with br update <ID> --parent <EPIC_ID>\n",
        );
    } else {
        println!(
            "Orphan issues ({} open/in_progress referenced in commits):",
            orphans.len()
        );
        println!();

        for (idx, orphan) in orphans.iter().enumerate() {
            println!(
                "{}. [{}] {} {}",
                idx + 1,
                orphan.status,
                orphan.issue_id,
                orphan.title
            );
            if args.details {
                println!(
                    "   Commit: {} {}",
                    orphan.latest_commit, orphan.latest_commit_message
                );
            }
        }
    }

    if args.fix {
        println!();
        println!("Interactive close mode:");
        for orphan in &orphans {
            print!("Close {} ({})? [y/N] ", orphan.issue_id, orphan.title);
            io::stdout().flush()?;

            let mut input = String::new();
            if io::stdin().read_line(&mut input).is_ok() {
                let input = input.trim().to_lowercase();
                if input == "y" || input == "yes" {
                    // Close the issue directly using internal API
                    let close_args = CloseArgs {
                        ids: vec![orphan.issue_id.clone()],
                        reason: Some("Implemented (detected by orphans scan)".to_string()),
                        force: false,
                        session: None,
                        suggest_next: false,
                    };

                    if let Err(e) = close::execute_with_args(&close_args, false, cli, ctx) {
                        eprintln!("  Failed to close {}: {}", orphan.issue_id, e);
                    }
                } else {
                    println!("  Skipped {}", orphan.issue_id);
                }
            }
        }
    }

    Ok(())
}

/// Check if the current directory is inside a git repository.
fn is_git_repo() -> bool {
    Command::new("git")
        .args(["rev-parse", "--git-dir"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Get git commit references containing issue IDs.
///
/// Returns Vec of (`commit_hash`, `commit_message`, `issue_id`) tuples.
/// The list is ordered from most recent to oldest commit.
fn get_git_commit_refs(prefix: &str) -> Result<Vec<(String, String, String)>> {
    let mut child = Command::new("git")
        .args(["log", "--oneline", "HEAD"])
        .stdout(Stdio::piped())
        .spawn()?;

    let stdout = child.stdout.take().ok_or_else(|| {
        crate::error::BeadsError::Config("Failed to capture git stdout".to_string())
    })?;

    let reader = BufReader::new(stdout);
    let refs = parse_git_log(reader, prefix)?;

    let status = child.wait()?;
    if !status.success() {
        return Ok(Vec::new());
    }

    Ok(refs)
}

/// Parse git log output and extract issue ID references.
///
/// Looks for patterns like `(bd-abc123)` or `bd-abc123` in commit messages.
fn parse_git_log<R: BufRead>(reader: R, prefix: &str) -> Result<Vec<(String, String, String)>> {
    // Pattern matches prefix-id including hierarchical IDs like bd-abc.1
    // We use word boundaries \b to avoid matching suffix/prefix (e.g. abd-123 or bd-123a)
    // although matching bd-123a is technically valid if 123a is the hash.
    // The previous regex forced parens: r"\(({}-[a-zA-Z0-9]+(?:\.[0-9]+)?)\)"
    let pattern = format!(r"\b({}-[a-zA-Z0-9]+(?:\.[0-9]+)?)\b", regex::escape(prefix));
    let re = Regex::new(&pattern)
        .map_err(|e| crate::error::BeadsError::Config(format!("Invalid regex pattern: {e}")))?;

    let mut results = Vec::new();

    for line in reader.lines() {
        let line = line.map_err(|e| crate::error::BeadsError::Config(format!("IO error: {e}")))?;

        // Each line is: <short_hash> <message>
        let parts: Vec<&str> = line.splitn(2, ' ').collect();
        if parts.len() < 2 {
            continue;
        }

        let commit_hash = parts[0].to_string();
        let commit_msg = parts[1].to_string();

        // Find all issue references in this commit message
        for cap in re.captures_iter(&commit_msg) {
            if let Some(issue_id) = cap.get(1) {
                results.push((
                    commit_hash.clone(),
                    commit_msg.clone(),
                    issue_id.as_str().to_string(),
                ));
            }
        }
    }

    Ok(results)
}

/// Output empty result in appropriate format.
fn output_empty(json: bool, ctx: &OutputContext) {
    if json || ctx.is_json() {
        println!("[]");
        return;
    }
    if ctx.is_quiet() {
        return;
    }
    if ctx.is_rich() {
        let theme = ctx.theme();
        let panel = Panel::from_text("No orphaned issues found.")
            .title(Text::styled("Orphans", theme.panel_title.clone()))
            .box_style(theme.box_style)
            .border_style(theme.panel_border.clone());
        ctx.render(&panel);
        return;
    }

    // Match bd format
    println!("✓ No orphaned issues found");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn test_parse_git_log_extracts_issue_ids() {
        let log = r"abc1234 Fix bug (bd-abc)
def5678 Another commit
ghi9012 Implement feature bd-xyz123
jkl3456 Multi-ref (bd-foo) and bd-bar";

        let refs = parse_git_log(Cursor::new(log), "bd").unwrap();

        assert_eq!(refs.len(), 4);
        assert_eq!(refs[0].2, "bd-abc");
        assert_eq!(refs[1].2, "bd-xyz123");
        assert_eq!(refs[2].2, "bd-foo");
        assert_eq!(refs[3].2, "bd-bar");
    }

    #[test]
    fn test_parse_git_log_hierarchical_ids() {
        let log = "abc1234 Fix child (bd-parent.1)";
        let refs = parse_git_log(Cursor::new(log), "bd").unwrap();

        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].2, "bd-parent.1");
    }

    #[test]
    fn test_parse_git_log_custom_prefix() {
        let log = "abc1234 Fix issue (proj-xyz)";
        let refs = parse_git_log(Cursor::new(log), "proj").unwrap();

        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].2, "proj-xyz");
    }

    #[test]
    fn test_parse_git_log_no_matches() {
        let log = "abc1234 Regular commit without issue refs";
        let refs = parse_git_log(Cursor::new(log), "bd").unwrap();

        assert!(refs.is_empty());
    }

    #[test]
    fn test_parse_git_log_preserves_order() {
        let log = r"aaa Latest (bd-1)
bbb Middle (bd-2)
ccc Oldest (bd-1)";

        let refs = parse_git_log(Cursor::new(log), "bd").unwrap();

        // First occurrence of bd-1 should be from the latest commit
        assert_eq!(refs[0].0, "aaa");
        assert_eq!(refs[0].2, "bd-1");

        // bd-2 is in the middle
        assert_eq!(refs[1].0, "bbb");
        assert_eq!(refs[1].2, "bd-2");

        // Second occurrence of bd-1 is from oldest
        assert_eq!(refs[2].0, "ccc");
        assert_eq!(refs[2].2, "bd-1");
    }
}
