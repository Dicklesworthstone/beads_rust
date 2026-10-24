//! Changelog command implementation.
//!
//! Generates release notes from closed issues since a given date or git reference.
//! Groups issues by type and sorts by priority within each group.

use crate::cli::ChangelogArgs;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::sanitize_terminal_inline;
use crate::output::{OutputContext, OutputMode, rich_text_panel};
use crate::storage::ChangelogIssueRow;
use crate::util::time::{parse_flexible_timestamp, parse_relative_time};
use chrono::{DateTime, Utc};
use rich_rust::prelude::*;
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChangelogRenderMode {
    Quiet,
    Json,
    Toon,
    Rich,
    Plain,
}

/// Execute changelog generation.
///
/// # Errors
///
/// Returns an error if config loading, git lookup, storage access, or output
/// serialization fails.
pub fn execute(
    args: &ChangelogArgs,
    json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    execute_with_storage_ctx(args, json, ctx, &beads_dir, &storage_ctx)
}

/// Execute changelog generation using storage that was already opened by the caller.
///
/// # Errors
///
/// Returns an error if git lookup, storage access, or output serialization
/// fails.
pub fn execute_with_storage_ctx(
    args: &ChangelogArgs,
    json: bool,
    ctx: &OutputContext,
    beads_dir: &Path,
    storage_ctx: &config::OpenStorageResult,
) -> Result<()> {
    let storage = &storage_ctx.storage;
    let repo_root = git_repo_root_for_path(&storage_ctx.paths.jsonl_path)
        .or_else(|| git_repo_root_for_path(beads_dir));

    let (since_dt, since_label) = resolve_since(args, repo_root.as_deref())?;
    let until = Utc::now();

    debug!(since = %since_label, "Filtering closed issues for changelog");

    let issues = storage.list_changelog_issues()?;

    let mut grouped: BTreeMap<String, Vec<ChangelogIssueRow>> = BTreeMap::new();
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

    match resolve_render_mode(json, ctx.mode()) {
        ChangelogRenderMode::Quiet => {}
        ChangelogRenderMode::Json => {
            if ctx.is_json() {
                ctx.json_pretty(&output);
            } else {
                // Robot mode requests JSON even though the shared output context only
                // sees global flags.
                println!("{}", serde_json::to_string_pretty(&output)?);
            }
        }
        ChangelogRenderMode::Toon => {
            ctx.toon(&output);
        }
        ChangelogRenderMode::Rich => {
            render_changelog_rich(&output, ctx);
        }
        ChangelogRenderMode::Plain => {
            print_text_output(&output);
        }
    }

    Ok(())
}

const fn resolve_render_mode(json: bool, output_mode: OutputMode) -> ChangelogRenderMode {
    match (json, output_mode) {
        (true, _) | (false, OutputMode::Json) => ChangelogRenderMode::Json,
        (false, OutputMode::Quiet) => ChangelogRenderMode::Quiet,
        (false, OutputMode::Toon) => ChangelogRenderMode::Toon,
        (false, OutputMode::Rich) => ChangelogRenderMode::Rich,
        (false, OutputMode::Plain) => ChangelogRenderMode::Plain,
    }
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
            let sanitized = sanitize_terminal_inline(other);
            let mut chars = sanitized.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect()
            })
        }
    }
}

fn changelog_display_text(value: &str) -> String {
    sanitize_terminal_inline(value).into_owned()
}

fn short_git_reference_label(reference: &str) -> String {
    reference.chars().take(7).collect()
}

/// Print plain text output for changelog.
fn print_text_output(output: &ChangelogOutput) {
    println!(
        "Changelog since {} ({} closed issues):",
        changelog_display_text(&output.since),
        output.total_closed
    );
    for group in &output.groups {
        println!();
        println!("{}:", changelog_display_text(&group.label));
        for entry in &group.issues {
            println!(
                "- [{}] {} {}",
                changelog_display_text(&entry.priority),
                changelog_display_text(&entry.id),
                sanitize_terminal_inline(&entry.title)
            );
        }
    }
}

/// Render changelog with rich formatting.
fn render_changelog_rich(output: &ChangelogOutput, ctx: &OutputContext) {
    let _console = Console::default();
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
            content.append_styled(
                &format!("{icon} {}\n", changelog_display_text(&group.label)),
                theme.emphasis.clone(),
            );

            // Issue entries
            for entry in &group.issues {
                content.append_styled("  • ", theme.dimmed.clone());
                content.append(sanitize_terminal_inline(&entry.title).as_ref());
                content.append_styled(
                    &format!(" ({})", changelog_display_text(&entry.id)),
                    theme.issue_id.clone(),
                );
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
    let panel = rich_text_panel(&content, width)
        .title(Text::styled("Changelog", theme.panel_title.clone()))
        .box_style(theme.box_style);

    ctx.render(&panel);
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
    changelog_display_text(date_str)
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

fn resolve_since(
    args: &ChangelogArgs,
    repo_root: Option<&Path>,
) -> Result<(Option<DateTime<Utc>>, String)> {
    if let Some(tag) = args.since_tag.as_deref() {
        let dt = git_tag_date(tag, repo_root)?;
        return Ok((Some(dt), tag.to_string()));
    }
    if let Some(commit) = args.since_commit.as_deref() {
        let dt = git_commit_date(commit, repo_root)?;
        // Show short hash if possible
        return Ok((Some(dt), short_git_reference_label(commit)));
    }
    if let Some(since) = args.since.as_deref() {
        if let Some(dt) = parse_relative_time(since) {
            return Ok((Some(dt), since.to_string()));
        }
        let dt = parse_flexible_timestamp(since, "since")?;
        return Ok((Some(dt), since.to_string()));
    }
    Ok((None, "all".to_string()))
}

fn git_commit_date(reference: &str, repo_root: Option<&Path>) -> Result<DateTime<Utc>> {
    if reference.starts_with('-') {
        return Err(BeadsError::validation(
            "git reference",
            "cannot start with '-'",
        ));
    }
    let display_reference = changelog_display_text(reference);

    let repo_root = repo_root.ok_or_else(|| {
        BeadsError::external_command(
            "git",
            format!(
                "Cannot resolve git reference '{display_reference}' without a git repository for the targeted project"
            ),
        )
    })?;
    let output = Command::new("git")
        .args(["show", "-s", "--format=%cI", reference])
        .current_dir(repo_root)
        .output()
        .map_err(|e| BeadsError::external_command("git", format!("Failed to run git: {e}")))?;

    if !output.status.success() {
        return Err(BeadsError::external_command(
            "git",
            format!("Failed to resolve git reference: {display_reference}"),
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stamp = stdout.trim();
    let dt = DateTime::parse_from_rfc3339(stamp)
        .map_err(|e| BeadsError::external_command("git", format!("Invalid git date: {e}")))?
        .with_timezone(&Utc);
    Ok(dt)
}

fn git_tag_date(reference: &str, repo_root: Option<&Path>) -> Result<DateTime<Utc>> {
    if reference.starts_with('-') {
        return Err(BeadsError::validation(
            "git tag reference",
            "cannot start with '-'",
        ));
    }
    let display_reference = changelog_display_text(reference);

    let repo_root = repo_root.ok_or_else(|| {
        BeadsError::external_command(
            "git",
            format!(
                "Cannot resolve git tag '{display_reference}' without a git repository for the targeted project"
            ),
        )
    })?;
    let tag_ref = format!("refs/tags/{reference}");

    let verify = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", &tag_ref])
        .current_dir(repo_root)
        .output()
        .map_err(|e| BeadsError::external_command("git", format!("Failed to run git: {e}")))?;

    if !verify.status.success() {
        return Err(BeadsError::external_command(
            "git",
            format!("Failed to resolve git tag: {display_reference}"),
        ));
    }

    // Annotated tags carry their own timestamp, which is what --since-tag promises.
    // Lightweight tags have no tagger date, so we fall back to the referenced commit.
    let output = Command::new("git")
        .args([
            "for-each-ref",
            "--format=%(taggerdate:iso-strict)",
            &tag_ref,
        ])
        .current_dir(repo_root)
        .output()
        .map_err(|e| BeadsError::external_command("git", format!("Failed to run git: {e}")))?;

    if !output.status.success() {
        return Err(BeadsError::external_command(
            "git",
            format!("Failed to resolve git tag: {display_reference}"),
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stamp = stdout.trim();
    if stamp.is_empty() {
        return git_commit_date(&format!("{tag_ref}^{{commit}}"), Some(repo_root));
    }

    DateTime::parse_from_rfc3339(stamp)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| BeadsError::external_command("git", format!("Invalid git tag date: {e}")))
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
        let (dt, label) = resolve_since(&args, None).unwrap();
        assert_eq!(
            dt.unwrap(),
            Utc.with_ymd_and_hms(2023, 1, 1, 0, 0, 0).unwrap()
        );
        assert_eq!(label, "2023-01-01T00:00:00Z");
    }

    #[test]
    fn test_resolve_since_relative() {
        let args = ChangelogArgs {
            since: Some("-1d".to_string()),
            ..Default::default()
        };
        let (dt, _) = resolve_since(&args, None).unwrap();
        let expected = Utc::now() - Duration::days(1);
        let actual = dt.unwrap();
        // Allow small delta
        assert!(actual < Utc::now());
        assert!(actual > expected - Duration::seconds(5));
    }

    #[test]
    fn test_resolve_since_none() {
        let args = ChangelogArgs::default();
        let (dt, label) = resolve_since(&args, None).unwrap();
        assert!(dt.is_none());
        assert_eq!(label, "all");
    }

    #[test]
    fn test_resolve_render_mode_prefers_context_structured_modes() {
        assert_eq!(
            resolve_render_mode(false, OutputMode::Json),
            ChangelogRenderMode::Json
        );
        assert_eq!(
            resolve_render_mode(false, OutputMode::Toon),
            ChangelogRenderMode::Toon
        );
    }

    #[test]
    fn test_resolve_render_mode_respects_robot_json_requests() {
        assert_eq!(
            resolve_render_mode(true, OutputMode::Plain),
            ChangelogRenderMode::Json
        );
        assert_eq!(
            resolve_render_mode(true, OutputMode::Rich),
            ChangelogRenderMode::Json
        );
    }

    #[test]
    fn test_resolve_render_mode_robot_overrides_inherited_toon() {
        assert_eq!(
            resolve_render_mode(true, OutputMode::Toon),
            ChangelogRenderMode::Json
        );
    }

    #[test]
    fn test_resolve_render_mode_respects_quiet_mode() {
        assert_eq!(
            resolve_render_mode(false, OutputMode::Quiet),
            ChangelogRenderMode::Quiet
        );
    }

    fn init_git_repo(repo_root: &Path) {
        let init = Command::new("git")
            .args(["init", "-b", "main"])
            .current_dir(repo_root)
            .output()
            .unwrap();
        assert!(init.status.success(), "git init failed");

        let config_name = Command::new("git")
            .args(["config", "user.name", "Test User"])
            .current_dir(repo_root)
            .status()
            .unwrap();
        assert!(config_name.success(), "git config user.name failed");
        let config_email = Command::new("git")
            .args(["config", "user.email", "test@example.com"])
            .current_dir(repo_root)
            .status()
            .unwrap();
        assert!(config_email.success(), "git config user.email failed");
    }

    fn commit_file(repo_root: &Path, message: &str, stamp: &str) {
        use std::fs;

        fs::write(repo_root.join("README.md"), format!("{message}\n")).unwrap();
        let add = Command::new("git")
            .args(["add", "README.md"])
            .current_dir(repo_root)
            .output()
            .unwrap();
        assert!(add.status.success(), "git add failed");
        let commit = Command::new("git")
            .args(["commit", "-m", message])
            .env("GIT_AUTHOR_DATE", stamp)
            .env("GIT_COMMITTER_DATE", stamp)
            .current_dir(repo_root)
            .output()
            .unwrap();
        assert!(commit.status.success(), "git commit failed");
    }

    #[test]
    fn test_git_commit_date_uses_target_repo_root() {
        use std::fs;

        let temp = tempfile::TempDir::new().unwrap();
        let repo_root = temp.path().join("target-repo");
        fs::create_dir_all(&repo_root).unwrap();

        init_git_repo(&repo_root);
        commit_file(&repo_root, "initial", "2024-01-01T00:00:00Z");

        let head = String::from_utf8_lossy(
            &Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&repo_root)
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();

        let dt = git_commit_date(&head, Some(&repo_root)).unwrap();
        assert!(dt <= Utc::now());
    }

    #[test]
    fn test_git_tag_date_prefers_annotated_tag_timestamp() {
        use std::fs;

        let temp = tempfile::TempDir::new().unwrap();
        let repo_root = temp.path().join("annotated-tag-repo");
        fs::create_dir_all(&repo_root).unwrap();

        init_git_repo(&repo_root);
        commit_file(&repo_root, "initial", "2024-01-01T00:00:00Z");

        let tag = Command::new("git")
            .args(["tag", "-a", "v1", "-m", "release"])
            .env("GIT_COMMITTER_DATE", "2024-02-01T00:00:00Z")
            .current_dir(&repo_root)
            .output()
            .unwrap();
        assert!(tag.status.success(), "git tag failed");

        let dt = git_tag_date("v1", Some(&repo_root)).unwrap();
        assert_eq!(dt, Utc.with_ymd_and_hms(2024, 2, 1, 0, 0, 0).unwrap());
    }

    #[test]
    fn test_git_tag_date_falls_back_for_lightweight_tags() {
        use std::fs;

        let temp = tempfile::TempDir::new().unwrap();
        let repo_root = temp.path().join("lightweight-tag-repo");
        fs::create_dir_all(&repo_root).unwrap();

        init_git_repo(&repo_root);
        commit_file(&repo_root, "initial", "2024-03-01T00:00:00Z");

        let tag = Command::new("git")
            .args(["tag", "v1"])
            .current_dir(&repo_root)
            .output()
            .unwrap();
        assert!(tag.status.success(), "git tag failed");

        let dt = git_tag_date("v1", Some(&repo_root)).unwrap();
        assert_eq!(dt, Utc.with_ymd_and_hms(2024, 3, 1, 0, 0, 0).unwrap());
    }

    #[test]
    fn test_git_tag_date_errors_for_missing_tag_even_if_branch_exists() {
        use std::fs;

        let temp = tempfile::TempDir::new().unwrap();
        let repo_root = temp.path().join("missing-tag-repo");
        fs::create_dir_all(&repo_root).unwrap();

        init_git_repo(&repo_root);
        commit_file(&repo_root, "initial", "2024-04-01T00:00:00Z");

        let branch = Command::new("git")
            .args(["checkout", "-b", "release"])
            .current_dir(&repo_root)
            .output()
            .unwrap();
        assert!(branch.status.success(), "git branch create failed");

        let err = git_tag_date("release", Some(&repo_root)).unwrap_err();
        assert!(
            err.to_string()
                .contains("Failed to resolve git tag: release"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_git_reference_errors_sanitize_terminal_controls() {
        let err = git_commit_date("bad\x1b[2J\rreset", None).unwrap_err();
        let message = err.to_string();

        assert!(!message.chars().any(char::is_control));
        assert!(message.contains("\\u{1b}[2J"));
        assert!(message.contains("\\r"));
    }

    #[test]
    fn test_git_tag_errors_sanitize_terminal_controls() {
        let err = git_tag_date("bad\x1b[2J\rreset", None).unwrap_err();
        let message = err.to_string();

        assert!(!message.chars().any(char::is_control));
        assert!(message.contains("\\u{1b}[2J"));
        assert!(message.contains("\\r"));
    }

    #[test]
    fn test_resolve_render_mode_prefers_robot_json_over_quiet() {
        assert_eq!(
            resolve_render_mode(true, OutputMode::Quiet),
            ChangelogRenderMode::Json
        );
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
    fn test_type_to_header_sanitizes_custom_type() {
        let header = type_to_header("qa\x1b[2J\rreset");

        assert!(!header.chars().any(char::is_control));
        assert!(header.contains("\\u{1b}[2J"));
        assert!(header.contains("\\r"));
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
    fn test_format_date_brief_sanitizes_unparseable_label() {
        let label = format_date_brief("release\x1b]52;c;bad\x07");

        assert!(!label.chars().any(char::is_control));
        assert!(label.contains("\\u{1b}]52;c;bad\\u{7}"));
    }

    #[test]
    fn test_changelog_display_text_sanitizes_terminal_controls() {
        let rendered = changelog_display_text("bd-1\x1b[2J\rreset\x08\nnext\x07\u{9b}");

        assert!(!rendered.chars().any(char::is_control));
        assert!(rendered.contains("\\u{1b}[2J"));
        assert!(rendered.contains("\\r"));
        assert!(rendered.contains("\\u{8}"));
        assert!(rendered.contains("\\n"));
        assert!(rendered.contains("\\u{7}"));
        assert!(rendered.contains("\\u{9b}"));
    }

    #[test]
    fn test_short_git_reference_label_is_character_boundary_safe() {
        assert_eq!(short_git_reference_label("abcdefghi"), "abcdefg");
        assert_eq!(short_git_reference_label("éééééééé"), "ééééééé");
        assert_eq!(short_git_reference_label("abc"), "abc");
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
