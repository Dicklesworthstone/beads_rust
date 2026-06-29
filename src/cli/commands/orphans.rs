//! orphans command implementation.
//!
//! Scans git commits for issue ID references and identifies issues
//! that are still `open/in_progress` but referenced in commits.

use crate::cli::OrphansArgs;
use crate::cli::commands::close::{self, CloseArgs};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::sanitize_terminal_inline;
use crate::model::{Issue, Status};
use crate::output::{IssueTable, IssueTableColumns, OutputContext, OutputMode};
use crate::storage::ListFilters;
use crate::util::{id::normalize_id, parse_id};
use regex::Regex;
use rich_rust::prelude::*;
use serde::Serialize;
use std::collections::{BTreeSet, HashMap};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OrphanRenderMode {
    Quiet,
    Json,
    Toon,
    Rich,
    Plain,
}

const fn resolve_render_mode(json: bool, output_mode: OutputMode) -> OrphanRenderMode {
    match (json, output_mode) {
        (true, _) | (false, OutputMode::Json) => OrphanRenderMode::Json,
        (false, OutputMode::Quiet) => OrphanRenderMode::Quiet,
        (false, OutputMode::Toon) => OrphanRenderMode::Toon,
        (false, OutputMode::Rich) => OrphanRenderMode::Rich,
        (false, OutputMode::Plain) => OrphanRenderMode::Plain,
    }
}

fn orphan_display_text(value: &str) -> String {
    sanitize_terminal_inline(value).into_owned()
}

fn validate_fix_render_mode(args: &OrphansArgs, render_mode: OrphanRenderMode) -> Result<()> {
    if args.fix
        && !matches!(
            render_mode,
            OrphanRenderMode::Rich | OrphanRenderMode::Plain
        )
    {
        return Err(BeadsError::validation(
            "fix",
            "--fix is interactive and requires human text output; omit --json/--robot/TOON/--quiet",
        ));
    }
    Ok(())
}

/// Execute the orphans command.
///
/// Scans git log for issue ID references and returns `open/in_progress`
/// issues that have been referenced in commits.
///
/// # Errors
///
/// Returns an error for invalid explicit targets or storage failures.
/// Returns an empty list when no workspace exists or when git metadata is
/// unavailable in the current repository.
#[allow(clippy::too_many_lines)]
pub fn execute(
    args: &OrphansArgs,
    json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let render_mode = resolve_render_mode(json, ctx.mode());
    validate_fix_render_mode(args, render_mode)?;

    let Some(beads_dir) = config::discover_optional_beads_dir_with_cli(cli)? else {
        output_empty(render_mode, ctx)?;
        return Ok(());
    };

    execute_inner(args, json, cli, ctx, &beads_dir, None)
}

/// Execute orphans using the caller's preopened storage context.
///
/// # Errors
///
/// Returns an error for invalid explicit targets or storage failures.
/// Returns an empty list when git metadata is unavailable in the current repository.
pub fn execute_with_storage_ctx(
    args: &OrphansArgs,
    json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    beads_dir: &Path,
    storage_ctx: &config::OpenStorageResult,
) -> Result<()> {
    execute_inner(args, json, cli, ctx, beads_dir, Some(storage_ctx))
}

#[allow(clippy::too_many_lines)]
fn execute_inner(
    args: &OrphansArgs,
    json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
    beads_dir: &Path,
    preloaded_storage_ctx: Option<&config::OpenStorageResult>,
) -> Result<()> {
    // beads_rust-750p: orphans must auto-import a newer JSONL before
    // scanning issue state, so JSONL-only closures (e.g. from `git pull`)
    // are reflected in the orphan list. We only do this on the
    // non-preloaded path; the preloaded path was opened by main.rs
    // (potentially in read_only_fast_open mode) and routes through its
    // own auto-import probe in the startup phase.
    //
    // CONCURRENCY NOTE: We do NOT acquire the project's `.write.lock`
    // explicitly here. This deviates from the audit/close/etc. flows in
    // sibling commands which acquire the lock before opening writable.
    // Rationale: orphans is on the read-only-fast-open list; main.rs's
    // startup path (which DOES acquire the lock) is the canonical
    // serialization point. The non-preloaded path is exercised in narrow
    // cases (e.g. dispatch fallback, certain test harnesses) where the
    // race is bounded by SQLite's per-connection locking. If two orphans
    // invocations race the auto-import here, SQLite's WAL serializes the
    // import write; the worst observable outcome is one waiting briefly
    // for the other's transaction. This is acceptable for a read command
    // and is documented at SYNC_SAFETY_INVARIANTS.md.
    let owned_storage_ctx = if preloaded_storage_ctx.is_some() {
        None
    } else {
        // Open with a writable cli (clear read_only_fast_open) so the
        // auto-import probe + import can mutate the DB. Without this,
        // the read-only fast-path connection raises "Bad file descriptor"
        // when imports try to write.
        let mut writable_cli = cli.clone();
        writable_cli.read_only_fast_open = false;
        let mut ctx_owned = config::open_storage_with_cli(beads_dir, &writable_cli)?;
        crate::cli::commands::auto_import_storage_ctx_if_stale(&mut ctx_owned, &writable_cli)?;
        Some(ctx_owned)
    };
    let storage_ctx = preloaded_storage_ctx
        .or(owned_storage_ctx.as_ref())
        .expect("orphans should have an open storage context");
    let storage = &storage_ctx.storage;

    // Get issue prefix from config
    let config_layer = storage_ctx.load_config(cli)?;
    let prefix = config::id_config_from_layer(&config_layer).prefix;
    let render_mode = resolve_render_mode(json, ctx.mode());
    validate_fix_render_mode(args, render_mode)?;

    // Get all open and in_progress issues first. If there are no candidate
    // issues, no commit reference can become an orphan, so skip the expensive
    // git history scan entirely.
    let filters = ListFilters {
        statuses: Some(vec![Status::Open, Status::InProgress]),
        ..Default::default()
    };
    let issues = storage.list_orphan_candidate_issues_for_command_output(&filters)?;
    if issues.is_empty() {
        output_empty(render_mode, ctx)?;
        return Ok(());
    }
    debug!(total_issues = issues.len(), "Scanning for orphaned issues");

    let Some(repo_root) = git_repo_root_for_path(&storage_ctx.paths.jsonl_path)
        .or_else(|| git_repo_root_for_path(beads_dir))
    else {
        output_empty(render_mode, ctx)?;
        return Ok(());
    };

    // Get git log and extract issue references. Use the candidate issues'
    // prefixes instead of only the configured default so mixed-prefix imports
    // and slugged IDs are still discoverable.
    let scan_prefixes = issue_prefixes_for_orphan_scan(&issues, &prefix);
    let commit_refs = get_git_commit_refs_for_prefixes(&scan_prefixes, &repo_root)?;

    trace!(
        commit_refs = commit_refs.len(),
        "Retrieved commit references"
    );

    if commit_refs.is_empty() {
        output_empty(render_mode, ctx)?;
        return Ok(());
    }

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

    if orphans.is_empty() {
        output_empty(render_mode, ctx)?;
        return Ok(());
    }

    match render_mode {
        OrphanRenderMode::Quiet => return Ok(()),
        OrphanRenderMode::Json => {
            if ctx.is_json() {
                ctx.json_pretty(&orphans);
            } else {
                // Robot mode requests JSON even though the shared output context only
                // sees global flags.
                println!("{}", serde_json::to_string_pretty(&orphans)?);
            }
        }
        OrphanRenderMode::Toon => {
            ctx.toon(&orphans);
        }
        OrphanRenderMode::Rich => {
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
        }
        OrphanRenderMode::Plain => {
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
                    orphan_display_text(&orphan.issue_id),
                    sanitize_terminal_inline(&orphan.title)
                );
                if args.details {
                    println!(
                        "   Commit: {} {}",
                        sanitize_terminal_inline(&orphan.latest_commit),
                        sanitize_terminal_inline(&orphan.latest_commit_message)
                    );
                }
            }
        }
    }

    if args.fix {
        println!();
        println!("Interactive close mode:");
        for orphan in &orphans {
            let issue_id = orphan_display_text(&orphan.issue_id);
            print!(
                "Close {} ({})? [y/N] ",
                issue_id,
                sanitize_terminal_inline(&orphan.title)
            );
            io::stdout().flush()?;

            let mut input = String::new();
            if io::stdin().read_line(&mut input).is_ok() {
                let input = input.trim().to_lowercase();
                if input == "y" || input == "yes" {
                    // Close the issue directly using internal API
                    let close_args = CloseArgs {
                        ids: vec![orphan.issue_id.clone()],
                        reason: Some("Implemented (detected by orphans scan)".to_string()),
                        ..CloseArgs::default()
                    };

                    if let Err(e) = close::execute_with_args(&close_args, false, cli, ctx) {
                        eprintln!(
                            "  Failed to close {}: {}",
                            issue_id,
                            sanitize_terminal_inline(&e.to_string())
                        );
                    }
                } else {
                    println!("  Skipped {issue_id}");
                }
            }
        }
    }

    Ok(())
}

fn git_repo_root_for_path(path: &Path) -> Option<PathBuf> {
    let start = if path.is_dir() { path } else { path.parent()? };
    let output = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(start)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let root = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if root.is_empty() {
        None
    } else {
        Some(PathBuf::from(root))
    }
}

fn issue_prefixes_for_orphan_scan(issues: &[Issue], default_prefix: &str) -> Vec<String> {
    let mut prefixes = BTreeSet::from([default_prefix.to_string()]);
    prefixes.extend(
        issues
            .iter()
            .filter_map(|issue| parse_id(&issue.id).ok().map(|parsed| parsed.prefix)),
    );

    let mut prefixes: Vec<_> = prefixes.into_iter().collect();
    prefixes.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
    prefixes
}

#[cfg(test)]
fn get_git_commit_refs(prefix: &str, repo_root: &Path) -> Result<Vec<(String, String, String)>> {
    get_git_commit_refs_for_prefixes(&[prefix.to_string()], repo_root)
}

fn get_git_commit_refs_for_prefixes(
    prefixes: &[String],
    repo_root: &Path,
) -> Result<Vec<(String, String, String)>> {
    let mut child = Command::new("git")
        .args(["log", "--oneline", "--all"])
        .current_dir(repo_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;

    let stdout = child.stdout.take().expect("Failed to open git log stdout");

    // Parse the stream as it comes in
    let refs_result = parse_git_log_for_prefixes(BufReader::new(stdout), prefixes);

    // Wait for the process to finish and check status
    let status = child.wait()?;
    if !status.success() {
        let mut stderr = String::new();
        if let Some(mut err_stream) = child.stderr.take() {
            let _ = err_stream.read_to_string(&mut stderr);
        }
        let stderr = stderr.trim();
        let detail = if stderr.is_empty() {
            "git log failed".to_string()
        } else {
            format!("git log failed: {stderr}")
        };
        return Err(crate::error::BeadsError::external_command("git", detail));
    }

    refs_result
}

/// Parse git log output and extract issue ID references.
///
/// Looks for patterns like `(bd-abc123)` or `bd-abc123` in commit messages.
#[cfg(test)]
fn parse_git_log<R: BufRead>(reader: R, prefix: &str) -> Result<Vec<(String, String, String)>> {
    parse_git_log_for_prefixes(reader, &[prefix.to_string()])
}

fn parse_git_log_for_prefixes<R: BufRead>(
    reader: R,
    prefixes: &[String],
) -> Result<Vec<(String, String, String)>> {
    if prefixes.is_empty() {
        return Ok(Vec::new());
    }

    let mut prefixes = prefixes
        .iter()
        .filter(|prefix| !prefix.is_empty())
        .cloned()
        .collect::<Vec<_>>();
    prefixes.sort_by(|left, right| right.len().cmp(&left.len()).then_with(|| left.cmp(right)));
    prefixes.dedup();

    let prefix_pattern = prefixes
        .iter()
        .map(|prefix| regex::escape(prefix))
        .collect::<Vec<_>>()
        .join("|");
    if prefix_pattern.is_empty() {
        return Ok(Vec::new());
    }

    // Pattern matches prefix-id including hierarchical IDs like bd-abc.1.2
    // We use word boundaries \b to avoid matching suffix/prefix (e.g. abd-123 or bd-123a)
    // although matching bd-123a is technically valid if 123a is the hash.
    // The previous regex forced parens: r"\(({}-[a-zA-Z0-9]+(?:\.[0-9]+)?)\)"
    // Use (?i) for case-insensitive matching (user input in commits varies)
    let pattern = format!(r"(?i)\b((?:{prefix_pattern})-[a-z0-9]+(?:\.[0-9]+)*)\b");
    let re = Regex::new(&pattern)
        .map_err(|e| crate::error::BeadsError::internal(format!("Invalid regex pattern: {e}")))?;

    let mut results = Vec::new();

    for line in reader.lines() {
        let line = line.map_err(crate::error::BeadsError::Io)?;

        // Each line is: <short_hash> <message>
        let Some((commit_hash, commit_msg)) = line.split_once(' ') else {
            continue;
        };
        let commit_hash = commit_hash.to_string();
        let commit_msg = commit_msg.to_string();

        // Find all issue references in this commit message
        for cap in re.captures_iter(&commit_msg) {
            if let Some(issue_id) = cap.get(1) {
                results.push((
                    commit_hash.clone(),
                    commit_msg.clone(),
                    normalize_id(issue_id.as_str()),
                ));
            }
        }
    }

    Ok(results)
}

/// Output empty result in appropriate format.
fn output_empty(render_mode: OrphanRenderMode, ctx: &OutputContext) -> Result<()> {
    let empty: Vec<OrphanIssue> = Vec::new();
    match render_mode {
        OrphanRenderMode::Quiet => {}
        OrphanRenderMode::Json => {
            if ctx.is_json() {
                ctx.json_pretty(&empty);
            } else {
                println!("{}", serde_json::to_string_pretty(&empty)?);
            }
        }
        OrphanRenderMode::Toon => {
            ctx.toon(&empty);
        }
        OrphanRenderMode::Rich => {
            let theme = ctx.theme();
            let panel = Panel::from_text("No orphaned issues found.")
                .title(Text::styled("Orphans", theme.panel_title.clone()))
                .box_style(theme.box_style)
                .border_style(theme.panel_border.clone());
            ctx.render(&panel);
        }
        OrphanRenderMode::Plain => {
            // Match bd format
            println!("✓ No orphaned issues found");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Cursor;
    use tempfile::TempDir;

    #[test]
    fn orphan_display_text_sanitizes_terminal_controls() {
        let rendered = orphan_display_text("bd-1\x1b[2J\rreset\x08\nnext\x07\u{9b}");

        assert!(!rendered.chars().any(char::is_control));
        assert!(rendered.contains("\\u{1b}[2J"));
        assert!(rendered.contains("\\r"));
        assert!(rendered.contains("\\u{8}"));
        assert!(rendered.contains("\\n"));
        assert!(rendered.contains("\\u{7}"));
        assert!(rendered.contains("\\u{9b}"));
    }

    #[test]
    fn fix_mode_requires_human_output() {
        let args = OrphansArgs {
            fix: true,
            ..Default::default()
        };

        assert!(validate_fix_render_mode(&args, OrphanRenderMode::Plain).is_ok());
        assert!(validate_fix_render_mode(&args, OrphanRenderMode::Rich).is_ok());

        for mode in [
            OrphanRenderMode::Json,
            OrphanRenderMode::Toon,
            OrphanRenderMode::Quiet,
        ] {
            let err = validate_fix_render_mode(&args, mode).unwrap_err();
            assert!(
                err.to_string().contains("--fix is interactive"),
                "unexpected error for {mode:?}: {err}"
            );
        }
    }

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
        let log = "abc1234 Fix child (bd-parent.1.2)";
        let refs = parse_git_log(Cursor::new(log), "bd").unwrap();

        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].2, "bd-parent.1.2");
    }

    #[test]
    fn test_parse_git_log_custom_prefix() {
        let log = "abc1234 Fix issue (proj-xyz)";
        let refs = parse_git_log(Cursor::new(log), "proj").unwrap();

        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].2, "proj-xyz");
    }

    #[test]
    fn test_parse_git_log_multiple_prefixes() {
        let log = "abc1234 Fix imported issue ext-xyz and local bd-abc";
        let prefixes = vec!["bd".to_string(), "ext".to_string()];
        let refs = parse_git_log_for_prefixes(Cursor::new(log), &prefixes).unwrap();

        assert_eq!(refs.len(), 2);
        assert_eq!(refs[0].2, "ext-xyz");
        assert_eq!(refs[1].2, "bd-abc");
    }

    #[test]
    fn test_parse_git_log_prefers_longest_slugged_prefix() {
        let log = "abc1234 Fix slugged issue bd-survey-abc";
        let prefixes = vec!["bd".to_string(), "bd-survey".to_string()];
        let refs = parse_git_log_for_prefixes(Cursor::new(log), &prefixes).unwrap();

        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].2, "bd-survey-abc");
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

    #[test]
    fn test_parse_git_log_normalizes_case() {
        let log = "abc1234 Fix bug (BD-ABC)";
        let refs = parse_git_log(Cursor::new(log), "bd").unwrap();
        assert_eq!(refs[0].2, "bd-abc");
    }

    fn git(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .status()
            .expect("run git");
        assert!(status.success(), "git {:?} failed", args);
    }

    #[test]
    fn test_get_git_commit_refs_empty_repo_returns_empty() {
        let temp = TempDir::new().expect("tempdir");
        git(temp.path(), &["init", "-q"]);

        let refs = get_git_commit_refs("bd", temp.path()).expect("empty repo refs");
        assert!(refs.is_empty());
    }

    #[test]
    fn test_get_git_commit_refs_uses_target_repo_root() {
        let temp = TempDir::new().expect("tempdir");
        git(temp.path(), &["init", "-q"]);
        git(temp.path(), &["config", "user.email", "tester@example.com"]);
        git(temp.path(), &["config", "user.name", "Tester"]);

        fs::write(temp.path().join("README.md"), "hello\n").expect("write readme");
        git(temp.path(), &["add", "README.md"]);
        git(temp.path(), &["commit", "-q", "-m", "Implement bd-xyz123"]);

        let refs = get_git_commit_refs("bd", temp.path()).expect("refs");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].2, "bd-xyz123");
    }

    #[test]
    fn test_resolve_render_mode_robot_overrides_inherited_toon() {
        assert_eq!(
            resolve_render_mode(true, OutputMode::Toon),
            OrphanRenderMode::Json
        );
    }

    #[test]
    fn test_resolve_render_mode_robot_overrides_rich() {
        assert_eq!(
            resolve_render_mode(true, OutputMode::Rich),
            OrphanRenderMode::Json
        );
    }

    #[test]
    fn test_resolve_render_mode_robot_overrides_plain() {
        assert_eq!(
            resolve_render_mode(true, OutputMode::Plain),
            OrphanRenderMode::Json
        );
    }

    #[test]
    fn test_resolve_render_mode_robot_overrides_quiet() {
        assert_eq!(
            resolve_render_mode(true, OutputMode::Quiet),
            OrphanRenderMode::Json
        );
    }

    #[test]
    fn test_resolve_render_mode_preserves_toon_without_robot() {
        assert_eq!(
            resolve_render_mode(false, OutputMode::Toon),
            OrphanRenderMode::Toon
        );
    }

    #[test]
    fn test_resolve_render_mode_preserves_json_context() {
        assert_eq!(
            resolve_render_mode(false, OutputMode::Json),
            OrphanRenderMode::Json
        );
    }

    #[test]
    fn test_resolve_render_mode_respects_quiet() {
        assert_eq!(
            resolve_render_mode(false, OutputMode::Quiet),
            OrphanRenderMode::Quiet
        );
    }
}
