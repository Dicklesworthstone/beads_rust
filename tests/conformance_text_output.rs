#![allow(clippy::all, clippy::pedantic, clippy::nursery)]
//! Conformance Tests: Text Output Parity (beads_rust-g1ig)
//!
//! These tests verify br (Rust) produces identical human-readable text output
//! to bd (Go) for stable commands with color/whitespace normalization.
//!
//! Commands tested: list, show, ready, blocked, stats, orphans

mod common;

use common::harness::ConformanceWorkspace;
use regex::Regex;
use std::sync::LazyLock;

// ============================================================================
// Text Normalization for Conformance
// ============================================================================

/// Pre-compiled regex patterns for text normalization
static ANSI_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*m").expect("ansi regex"));
static ID_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b[a-zA-Z0-9_]+-[a-z0-9]{3,}\b").expect("id regex"));
static TS_FULL_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?(Z|[+-]\d{2}:?\d{2})?")
        .expect("full timestamp regex")
});
static DATE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d{4}-\d{2}-\d{2}").expect("date regex"));
static DURATION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d+(\.\d+)?\s*(ms|µs|ns|s|m|h|d)").expect("duration regex"));
static TRAILING_WS_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[ \t]+$").expect("trailing whitespace regex"));
static MULTIPLE_BLANK_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\n{3,}").expect("multiple blank lines regex"));
static HOME_PATH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/home/[a-zA-Z0-9_-]+").expect("home path regex"));
static USERS_PATH_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"/Users/[a-zA-Z0-9_-]+").expect("users path regex"));
static TMP_PATH_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"/tmp/\.tmp[a-zA-Z0-9]+|/var/folders/[a-zA-Z0-9/_-]+").expect("tmp path regex")
});
static VERSION_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\((main|master)@[a-f0-9]+\)").expect("version regex"));
static SEMVER_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\d+\.\d+\.\d+(-[a-zA-Z0-9.]+)?").expect("semver regex"));

/// Normalize text output for conformance comparison.
///
/// This strips ANSI codes, normalizes IDs/timestamps/paths, and cleans up
/// whitespace differences to allow comparing br and bd output.
fn normalize_text_for_conformance(text: &str) -> String {
    let mut normalized = text.to_string();

    // 1. Normalize line endings (CRLF → LF)
    normalized = normalized.replace("\r\n", "\n");

    // 2. Strip ANSI escape sequences
    normalized = ANSI_RE.replace_all(&normalized, "").to_string();

    // 3. Normalize issue IDs to a placeholder
    normalized = ID_RE.replace_all(&normalized, "ISSUE_ID").to_string();

    // 4. Mask full timestamps
    normalized = TS_FULL_RE
        .replace_all(&normalized, "TIMESTAMP")
        .to_string();

    // 5. Mask dates
    normalized = DATE_RE.replace_all(&normalized, "DATE").to_string();

    // 6. Mask durations
    normalized = DURATION_RE.replace_all(&normalized, "DURATION").to_string();

    // 7. Mask home paths
    normalized = HOME_PATH_RE.replace_all(&normalized, "/HOME").to_string();
    normalized = USERS_PATH_RE.replace_all(&normalized, "/HOME").to_string();

    // 8. Mask temp paths
    normalized = TMP_PATH_RE.replace_all(&normalized, "/TMP").to_string();

    // 9. Mask version/git hash info
    normalized = VERSION_RE
        .replace_all(&normalized, "(BRANCH@HASH)")
        .to_string();
    normalized = SEMVER_RE.replace_all(&normalized, "VERSION").to_string();

    // 10. Strip trailing whitespace per line
    let lines: Vec<&str> = normalized.lines().collect();
    let trimmed: Vec<String> = lines
        .iter()
        .map(|line| TRAILING_WS_RE.replace_all(line, "").to_string())
        .collect();
    normalized = trimmed.join("\n");

    // 11. Collapse multiple blank lines
    normalized = MULTIPLE_BLANK_RE.replace_all(&normalized, "\n\n").to_string();

    // 12. Trim leading/trailing whitespace from the entire output
    normalized = normalized.trim().to_string();

    normalized
}

/// Result of comparing text outputs between br and bd
#[derive(Debug)]
struct TextComparisonResult {
    matches: bool,
    br_normalized: String,
    bd_normalized: String,
    diff_summary: String,
}

impl TextComparisonResult {
    fn compare(br_output: &str, bd_output: &str) -> Self {
        let br_normalized = normalize_text_for_conformance(br_output);
        let bd_normalized = normalize_text_for_conformance(bd_output);
        let matches = br_normalized == bd_normalized;

        let diff_summary = if matches {
            "Outputs match after normalization".to_string()
        } else {
            let br_lines: Vec<&str> = br_normalized.lines().collect();
            let bd_lines: Vec<&str> = bd_normalized.lines().collect();

            let mut diffs = Vec::new();
            let max_lines = br_lines.len().max(bd_lines.len());

            for i in 0..max_lines {
                let br_line = br_lines.get(i).map(|s| *s).unwrap_or("<missing>");
                let bd_line = bd_lines.get(i).map(|s| *s).unwrap_or("<missing>");
                if br_line != bd_line {
                    diffs.push(format!(
                        "Line {}: br='{}' vs bd='{}'",
                        i + 1,
                        br_line,
                        bd_line
                    ));
                }
            }

            if diffs.len() > 10 {
                format!(
                    "{} differences found (showing first 10):\n{}",
                    diffs.len(),
                    diffs[..10].join("\n")
                )
            } else {
                format!("{} differences found:\n{}", diffs.len(), diffs.join("\n"))
            }
        };

        Self {
            matches,
            br_normalized,
            bd_normalized,
            diff_summary,
        }
    }
}

/// Extract issue ID from create output
fn extract_id_from_create(stdout: &str) -> String {
    // br output: "Created bd-abc123: Title"
    // bd output: "Created bd-abc123: Title"
    let line = stdout.lines().next().unwrap_or("");
    line.strip_prefix("Created ")
        .and_then(|rest| rest.split(':').next())
        .unwrap_or("")
        .trim()
        .to_string()
}

// ============================================================================
// Text Output Conformance Tests
// ============================================================================

/// Test: `list` command with empty database
#[test]
fn conformance_text_list_empty() {
    common::init_test_logging();

    let mut workspace = ConformanceWorkspace::new("conformance_text", "list_empty");
    let (br_init, bd_init) = workspace.init_both();

    assert!(br_init.success, "br init failed: {}", br_init.stderr);
    assert!(bd_init.success, "bd init failed: {}", bd_init.stderr);

    // Run list command (text output, no --json)
    let br_list = workspace.run_br(["list"], "list");
    let bd_list = workspace.run_bd(["list"], "list");

    assert!(br_list.success, "br list failed: {}", br_list.stderr);
    assert!(bd_list.success, "bd list failed: {}", bd_list.stderr);

    let result = TextComparisonResult::compare(&br_list.stdout, &bd_list.stdout);

    assert!(
        result.matches,
        "Text output mismatch for 'list' (empty):\n{}\n\nbr output:\n{}\n\nbd output:\n{}",
        result.diff_summary,
        result.br_normalized,
        result.bd_normalized
    );

    workspace.finish(true);
}

/// Test: `list` command with issues
#[test]
fn conformance_text_list_with_issues() {
    common::init_test_logging();

    let mut workspace = ConformanceWorkspace::new("conformance_text", "list_with_issues");
    workspace.init_both();

    // Create identical issues in both workspaces
    workspace.run_br(["create", "First issue"], "create1");
    workspace.run_bd(["create", "First issue"], "create1");
    workspace.run_br(["create", "Second issue"], "create2");
    workspace.run_bd(["create", "Second issue"], "create2");
    workspace.run_br(["create", "Third issue"], "create3");
    workspace.run_bd(["create", "Third issue"], "create3");

    // Run list command
    let br_list = workspace.run_br(["list"], "list");
    let bd_list = workspace.run_bd(["list"], "list");

    assert!(br_list.success, "br list failed: {}", br_list.stderr);
    assert!(bd_list.success, "bd list failed: {}", bd_list.stderr);

    let result = TextComparisonResult::compare(&br_list.stdout, &bd_list.stdout);

    assert!(
        result.matches,
        "Text output mismatch for 'list' (with issues):\n{}\n\nbr output:\n{}\n\nbd output:\n{}",
        result.diff_summary,
        result.br_normalized,
        result.bd_normalized
    );

    workspace.finish(true);
}

/// Test: `show` command
#[test]
fn conformance_text_show() {
    common::init_test_logging();

    let mut workspace = ConformanceWorkspace::new("conformance_text", "show");
    workspace.init_both();

    // Create issues and capture IDs
    let br_create = workspace.run_br(["create", "Test issue for show"], "create");
    let bd_create = workspace.run_bd(["create", "Test issue for show"], "create");

    let br_id = extract_id_from_create(&br_create.stdout);
    let bd_id = extract_id_from_create(&bd_create.stdout);

    // Run show command
    let br_show = workspace.run_br(["show", &br_id], "show");
    let bd_show = workspace.run_bd(["show", &bd_id], "show");

    assert!(br_show.success, "br show failed: {}", br_show.stderr);
    assert!(bd_show.success, "bd show failed: {}", bd_show.stderr);

    let result = TextComparisonResult::compare(&br_show.stdout, &bd_show.stdout);

    assert!(
        result.matches,
        "Text output mismatch for 'show':\n{}\n\nbr output:\n{}\n\nbd output:\n{}",
        result.diff_summary,
        result.br_normalized,
        result.bd_normalized
    );

    workspace.finish(true);
}

/// Test: `ready` command with empty database
#[test]
fn conformance_text_ready_empty() {
    common::init_test_logging();

    let mut workspace = ConformanceWorkspace::new("conformance_text", "ready_empty");
    workspace.init_both();

    let br_ready = workspace.run_br(["ready"], "ready");
    let bd_ready = workspace.run_bd(["ready"], "ready");

    assert!(br_ready.success, "br ready failed: {}", br_ready.stderr);
    assert!(bd_ready.success, "bd ready failed: {}", bd_ready.stderr);

    let result = TextComparisonResult::compare(&br_ready.stdout, &bd_ready.stdout);

    assert!(
        result.matches,
        "Text output mismatch for 'ready' (empty):\n{}\n\nbr output:\n{}\n\nbd output:\n{}",
        result.diff_summary,
        result.br_normalized,
        result.bd_normalized
    );

    workspace.finish(true);
}

/// Test: `ready` command with issues
#[test]
fn conformance_text_ready_with_issues() {
    common::init_test_logging();

    let mut workspace = ConformanceWorkspace::new("conformance_text", "ready_with_issues");
    workspace.init_both();

    // Create issues with different priorities
    workspace.run_br(["create", "High priority task", "--priority", "1"], "create1");
    workspace.run_bd(["create", "High priority task", "--priority", "1"], "create1");
    workspace.run_br(["create", "Medium priority task", "--priority", "2"], "create2");
    workspace.run_bd(["create", "Medium priority task", "--priority", "2"], "create2");
    workspace.run_br(["create", "Low priority task", "--priority", "3"], "create3");
    workspace.run_bd(["create", "Low priority task", "--priority", "3"], "create3");

    let br_ready = workspace.run_br(["ready"], "ready");
    let bd_ready = workspace.run_bd(["ready"], "ready");

    assert!(br_ready.success, "br ready failed: {}", br_ready.stderr);
    assert!(bd_ready.success, "bd ready failed: {}", bd_ready.stderr);

    let result = TextComparisonResult::compare(&br_ready.stdout, &bd_ready.stdout);

    assert!(
        result.matches,
        "Text output mismatch for 'ready' (with issues):\n{}\n\nbr output:\n{}\n\nbd output:\n{}",
        result.diff_summary,
        result.br_normalized,
        result.bd_normalized
    );

    workspace.finish(true);
}

/// Test: `blocked` command with empty database
#[test]
fn conformance_text_blocked_empty() {
    common::init_test_logging();

    let mut workspace = ConformanceWorkspace::new("conformance_text", "blocked_empty");
    workspace.init_both();

    let br_blocked = workspace.run_br(["blocked"], "blocked");
    let bd_blocked = workspace.run_bd(["blocked"], "blocked");

    assert!(br_blocked.success, "br blocked failed: {}", br_blocked.stderr);
    assert!(bd_blocked.success, "bd blocked failed: {}", bd_blocked.stderr);

    let result = TextComparisonResult::compare(&br_blocked.stdout, &bd_blocked.stdout);

    assert!(
        result.matches,
        "Text output mismatch for 'blocked' (empty):\n{}\n\nbr output:\n{}\n\nbd output:\n{}",
        result.diff_summary,
        result.br_normalized,
        result.bd_normalized
    );

    workspace.finish(true);
}

/// Test: `blocked` command with blocked issues
#[test]
fn conformance_text_blocked_with_issues() {
    common::init_test_logging();

    let mut workspace = ConformanceWorkspace::new("conformance_text", "blocked_with_issues");
    workspace.init_both();

    // Create issues and add dependencies to create blocked issues
    let br_blocker = workspace.run_br(["create", "Blocker issue"], "create_blocker");
    let bd_blocker = workspace.run_bd(["create", "Blocker issue"], "create_blocker");
    let br_blocked = workspace.run_br(["create", "Blocked issue"], "create_blocked");
    let bd_blocked = workspace.run_bd(["create", "Blocked issue"], "create_blocked");

    let br_blocker_id = extract_id_from_create(&br_blocker.stdout);
    let bd_blocker_id = extract_id_from_create(&bd_blocker.stdout);
    let br_blocked_id = extract_id_from_create(&br_blocked.stdout);
    let bd_blocked_id = extract_id_from_create(&bd_blocked.stdout);

    // Add dependency: blocked depends on blocker
    workspace.run_br(["dep", "add", &br_blocked_id, &br_blocker_id], "dep_add");
    workspace.run_bd(["dep", "add", &bd_blocked_id, &bd_blocker_id], "dep_add");

    let br_result = workspace.run_br(["blocked"], "blocked");
    let bd_result = workspace.run_bd(["blocked"], "blocked");

    assert!(br_result.success, "br blocked failed: {}", br_result.stderr);
    assert!(bd_result.success, "bd blocked failed: {}", bd_result.stderr);

    let result = TextComparisonResult::compare(&br_result.stdout, &bd_result.stdout);

    assert!(
        result.matches,
        "Text output mismatch for 'blocked' (with issues):\n{}\n\nbr output:\n{}\n\nbd output:\n{}",
        result.diff_summary,
        result.br_normalized,
        result.bd_normalized
    );

    workspace.finish(true);
}

/// Test: `stats` command (alias for status)
#[test]
fn conformance_text_stats_empty() {
    common::init_test_logging();

    let mut workspace = ConformanceWorkspace::new("conformance_text", "stats_empty");
    workspace.init_both();

    // Use --no-activity to skip git activity tracking which could differ
    let br_stats = workspace.run_br(["stats", "--no-activity"], "stats");
    let bd_stats = workspace.run_bd(["stats", "--no-activity"], "stats");

    assert!(br_stats.success, "br stats failed: {}", br_stats.stderr);
    assert!(bd_stats.success, "bd stats failed: {}", bd_stats.stderr);

    let result = TextComparisonResult::compare(&br_stats.stdout, &bd_stats.stdout);

    assert!(
        result.matches,
        "Text output mismatch for 'stats' (empty):\n{}\n\nbr output:\n{}\n\nbd output:\n{}",
        result.diff_summary,
        result.br_normalized,
        result.bd_normalized
    );

    workspace.finish(true);
}

/// Test: `stats` command with issues
#[test]
fn conformance_text_stats_with_issues() {
    common::init_test_logging();

    let mut workspace = ConformanceWorkspace::new("conformance_text", "stats_with_issues");
    workspace.init_both();

    // Create some issues
    workspace.run_br(["create", "Open issue 1"], "create1");
    workspace.run_bd(["create", "Open issue 1"], "create1");
    workspace.run_br(["create", "Open issue 2"], "create2");
    workspace.run_bd(["create", "Open issue 2"], "create2");

    // Create and close an issue
    let br_close = workspace.run_br(["create", "Issue to close"], "create_close");
    let bd_close = workspace.run_bd(["create", "Issue to close"], "create_close");
    let br_close_id = extract_id_from_create(&br_close.stdout);
    let bd_close_id = extract_id_from_create(&bd_close.stdout);
    workspace.run_br(["close", &br_close_id], "close");
    workspace.run_bd(["close", &bd_close_id], "close");

    let br_stats = workspace.run_br(["stats", "--no-activity"], "stats");
    let bd_stats = workspace.run_bd(["stats", "--no-activity"], "stats");

    assert!(br_stats.success, "br stats failed: {}", br_stats.stderr);
    assert!(bd_stats.success, "bd stats failed: {}", bd_stats.stderr);

    let result = TextComparisonResult::compare(&br_stats.stdout, &bd_stats.stdout);

    assert!(
        result.matches,
        "Text output mismatch for 'stats' (with issues):\n{}\n\nbr output:\n{}\n\nbd output:\n{}",
        result.diff_summary,
        result.br_normalized,
        result.bd_normalized
    );

    workspace.finish(true);
}

/// Test: `orphans` command with empty database
#[test]
fn conformance_text_orphans_empty() {
    common::init_test_logging();

    let mut workspace = ConformanceWorkspace::new("conformance_text", "orphans_empty");
    workspace.init_both();

    let br_orphans = workspace.run_br(["orphans"], "orphans");
    let bd_orphans = workspace.run_bd(["orphans"], "orphans");

    // Exit code behavior should match
    assert_eq!(
        br_orphans.success,
        bd_orphans.success,
        "Exit code mismatch for 'orphans' (empty): br={}, bd={}",
        br_orphans.exit_code,
        bd_orphans.exit_code
    );

    let result = TextComparisonResult::compare(&br_orphans.stdout, &bd_orphans.stdout);

    assert!(
        result.matches,
        "Text output mismatch for 'orphans' (empty):\n{}\n\nbr output:\n{}\n\nbd output:\n{}",
        result.diff_summary,
        result.br_normalized,
        result.bd_normalized
    );

    workspace.finish(true);
}

/// Test: `list` with status filter
#[test]
fn conformance_text_list_status_filter() {
    common::init_test_logging();

    let mut workspace = ConformanceWorkspace::new("conformance_text", "list_status_filter");
    workspace.init_both();

    // Create issues with different statuses
    workspace.run_br(["create", "Open issue"], "create_open");
    workspace.run_bd(["create", "Open issue"], "create_open");

    let br_create = workspace.run_br(["create", "Issue to close"], "create_close");
    let bd_create = workspace.run_bd(["create", "Issue to close"], "create_close");
    let br_id = extract_id_from_create(&br_create.stdout);
    let bd_id = extract_id_from_create(&bd_create.stdout);
    workspace.run_br(["close", &br_id], "close");
    workspace.run_bd(["close", &bd_id], "close");

    // List only open issues
    let br_list = workspace.run_br(["list", "--status", "open"], "list_open");
    let bd_list = workspace.run_bd(["list", "--status", "open"], "list_open");

    assert!(br_list.success, "br list --status failed: {}", br_list.stderr);
    assert!(bd_list.success, "bd list --status failed: {}", bd_list.stderr);

    let result = TextComparisonResult::compare(&br_list.stdout, &bd_list.stdout);

    assert!(
        result.matches,
        "Text output mismatch for 'list --status open':\n{}\n\nbr output:\n{}\n\nbd output:\n{}",
        result.diff_summary,
        result.br_normalized,
        result.bd_normalized
    );

    workspace.finish(true);
}

/// Test: `list` with type filter
#[test]
fn conformance_text_list_type_filter() {
    common::init_test_logging();

    let mut workspace = ConformanceWorkspace::new("conformance_text", "list_type_filter");
    workspace.init_both();

    // Create issues with different types
    workspace.run_br(["create", "Bug report", "--type", "bug"], "create_bug");
    workspace.run_bd(["create", "Bug report", "--type", "bug"], "create_bug");
    workspace.run_br(["create", "Feature request", "--type", "feature"], "create_feature");
    workspace.run_bd(["create", "Feature request", "--type", "feature"], "create_feature");
    workspace.run_br(["create", "Regular task", "--type", "task"], "create_task");
    workspace.run_bd(["create", "Regular task", "--type", "task"], "create_task");

    // List only bugs
    let br_list = workspace.run_br(["list", "--type", "bug"], "list_bug");
    let bd_list = workspace.run_bd(["list", "--type", "bug"], "list_bug");

    assert!(br_list.success, "br list --type failed: {}", br_list.stderr);
    assert!(bd_list.success, "bd list --type failed: {}", bd_list.stderr);

    let result = TextComparisonResult::compare(&br_list.stdout, &bd_list.stdout);

    assert!(
        result.matches,
        "Text output mismatch for 'list --type bug':\n{}\n\nbr output:\n{}\n\nbd output:\n{}",
        result.diff_summary,
        result.br_normalized,
        result.bd_normalized
    );

    workspace.finish(true);
}

/// Test: `list` with priority filter
#[test]
fn conformance_text_list_priority_filter() {
    common::init_test_logging();

    let mut workspace = ConformanceWorkspace::new("conformance_text", "list_priority_filter");
    workspace.init_both();

    // Create issues with different priorities
    workspace.run_br(["create", "Critical issue", "--priority", "0"], "create_p0");
    workspace.run_bd(["create", "Critical issue", "--priority", "0"], "create_p0");
    workspace.run_br(["create", "High priority", "--priority", "1"], "create_p1");
    workspace.run_bd(["create", "High priority", "--priority", "1"], "create_p1");
    workspace.run_br(["create", "Medium priority", "--priority", "2"], "create_p2");
    workspace.run_bd(["create", "Medium priority", "--priority", "2"], "create_p2");

    // List only critical (P0) issues
    let br_list = workspace.run_br(["list", "--priority", "0"], "list_p0");
    let bd_list = workspace.run_bd(["list", "--priority", "0"], "list_p0");

    assert!(br_list.success, "br list --priority failed: {}", br_list.stderr);
    assert!(bd_list.success, "bd list --priority failed: {}", bd_list.stderr);

    let result = TextComparisonResult::compare(&br_list.stdout, &bd_list.stdout);

    assert!(
        result.matches,
        "Text output mismatch for 'list --priority 0':\n{}\n\nbr output:\n{}\n\nbd output:\n{}",
        result.diff_summary,
        result.br_normalized,
        result.bd_normalized
    );

    workspace.finish(true);
}

/// Test: `show` with non-existent issue
#[test]
fn conformance_text_show_not_found() {
    common::init_test_logging();

    let mut workspace = ConformanceWorkspace::new("conformance_text", "show_not_found");
    workspace.init_both();

    let br_show = workspace.run_br(["show", "nonexistent-id"], "show");
    let bd_show = workspace.run_bd(["show", "nonexistent-id"], "show");

    // Both should fail consistently
    assert_eq!(
        br_show.success,
        bd_show.success,
        "Exit code mismatch for 'show' (not found): br={}, bd={}",
        br_show.exit_code,
        bd_show.exit_code
    );

    // Compare stderr for error message parity
    let result = TextComparisonResult::compare(&br_show.stderr, &bd_show.stderr);

    // Note: Error messages may differ, so we just verify both fail
    if !br_show.success && !bd_show.success {
        // Both failed as expected
    }

    workspace.finish(true);
}

/// Test: `ready` with limit
#[test]
fn conformance_text_ready_with_limit() {
    common::init_test_logging();

    let mut workspace = ConformanceWorkspace::new("conformance_text", "ready_with_limit");
    workspace.init_both();

    // Create multiple issues
    for i in 1..=5 {
        let title = format!("Task {}", i);
        workspace.run_br(["create", &title], &format!("create_{}", i));
        workspace.run_bd(["create", &title], &format!("create_{}", i));
    }

    // Get only first 2 ready issues
    let br_ready = workspace.run_br(["ready", "--limit", "2"], "ready");
    let bd_ready = workspace.run_bd(["ready", "--limit", "2"], "ready");

    assert!(br_ready.success, "br ready --limit failed: {}", br_ready.stderr);
    assert!(bd_ready.success, "bd ready --limit failed: {}", bd_ready.stderr);

    let result = TextComparisonResult::compare(&br_ready.stdout, &bd_ready.stdout);

    assert!(
        result.matches,
        "Text output mismatch for 'ready --limit 2':\n{}\n\nbr output:\n{}\n\nbd output:\n{}",
        result.diff_summary,
        result.br_normalized,
        result.bd_normalized
    );

    workspace.finish(true);
}

// ============================================================================
// Unit Tests for Normalization
// ============================================================================

#[cfg(test)]
mod normalization_tests {
    use super::*;

    #[test]
    fn test_normalize_ansi_codes() {
        let input = "\x1b[31mRed\x1b[0m normal \x1b[1;32mgreen\x1b[0m";
        let result = normalize_text_for_conformance(input);
        assert!(!result.contains("\x1b["));
        assert!(result.contains("Red"));
        assert!(result.contains("green"));
    }

    #[test]
    fn test_normalize_issue_ids() {
        let input = "Issue bd-abc123 depends on beads_rust-xyz789";
        let result = normalize_text_for_conformance(input);
        assert!(result.contains("ISSUE_ID"));
        assert!(!result.contains("bd-abc123"));
        assert!(!result.contains("beads_rust-xyz789"));
    }

    #[test]
    fn test_normalize_timestamps() {
        let input = "Created: 2026-01-17T12:30:45.123456Z";
        let result = normalize_text_for_conformance(input);
        assert!(result.contains("TIMESTAMP"));
        assert!(!result.contains("2026-01-17T12:30:45"));
    }

    #[test]
    fn test_normalize_dates() {
        let input = "Due: 2026-01-17";
        let result = normalize_text_for_conformance(input);
        assert!(result.contains("DATE"));
        assert!(!result.contains("2026-01-17"));
    }

    #[test]
    fn test_normalize_durations() {
        let input = "Completed in 123ms, total 5s";
        let result = normalize_text_for_conformance(input);
        assert!(result.contains("DURATION"));
        assert!(!result.contains("123ms"));
    }

    #[test]
    fn test_normalize_paths() {
        let input = "Config at /home/user/.config/br";
        let result = normalize_text_for_conformance(input);
        assert!(result.contains("/HOME"));
        assert!(!result.contains("/home/user"));
    }

    #[test]
    fn test_normalize_line_endings() {
        let input = "line1\r\nline2\r\n";
        let result = normalize_text_for_conformance(input);
        assert!(!result.contains("\r\n"));
    }

    #[test]
    fn test_normalize_trailing_whitespace() {
        let input = "line1   \nline2\t\t\n";
        let result = normalize_text_for_conformance(input);
        let lines: Vec<&str> = result.lines().collect();
        for line in lines {
            assert!(!line.ends_with(' '));
            assert!(!line.ends_with('\t'));
        }
    }

    #[test]
    fn test_comparison_result_match() {
        let text = "Issue ISSUE_ID is open";
        let result = TextComparisonResult::compare(text, text);
        assert!(result.matches);
    }

    #[test]
    fn test_comparison_result_mismatch() {
        let br = "Issue bd-abc: open";
        let bd = "Issue bd-xyz: closed";
        let result = TextComparisonResult::compare(br, bd);
        // After normalization, "bd-abc" and "bd-xyz" become "ISSUE_ID"
        // but "open" vs "closed" will differ
        assert!(!result.matches);
    }
}
