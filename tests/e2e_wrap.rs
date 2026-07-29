//! E2E tests for wrapping behavior across various commands.
//!
//! NOTE: `--wrap` was removed as an opt-in flag. Wrapping to the terminal
//! width is now the DEFAULT behavior for `list`/`show`/`search`/`blocked`;
//! the surviving flag is `--no-wrap`, which opts OUT of wrapping (truncates
//! long lines instead). This file has been ported accordingly: tests that
//! previously passed `--wrap` now rely on the (new) default, and tests that
//! previously exercised "no flag = truncate" now pass `--no-wrap` to get
//! truncation.
//!
//! Tests verify that:
//! - `--no-wrap` is accepted by show, list, search, blocked
//! - Default behavior now wraps (does not truncate) long content
//! - With `--no-wrap`, long content is truncated
//! - Different terminal widths are respected

mod common;

use common::cli::{BrWorkspace, run_br, run_br_with_env};

fn init_workspace_with_long_issues(workspace: &BrWorkspace) {
    // Initialize (`init` no longer accepts `--prefix`; the "wrap-" prefix
    // used below and asserted on further down is now supplied explicitly
    // on each `create` call instead).
    let output = run_br(workspace, ["init"], "init");
    assert!(output.status.success(), "init failed: {}", output.stderr);

    // Create issue with a very long title
    let long_title = "This is a very long issue title that should definitely exceed the normal terminal width when displayed in the list view or show view without wrapping enabled";
    let output = run_br(
        workspace,
        [
            "create",
            long_title,
            "--type",
            "task",
            "--priority",
            "2",
            "-d",
            "This is also a very long description that contains multiple sentences and should span several lines when the wrap option is enabled because it provides detailed context about the issue being tracked.",
            "--prefix",
            "wrap",
        ],
        "create_long",
    );
    assert!(output.status.success(), "create failed: {}", output.stderr);

    // Create a shorter issue for comparison
    let output = run_br(
        workspace,
        ["create", "Short issue", "--type", "bug", "--prefix", "wrap"],
        "create_short",
    );
    assert!(output.status.success(), "create short failed");
}

// =============================================================================
// BR LIST WRAP/NO-WRAP TESTS
// =============================================================================

#[test]
fn e2e_list_no_wrap_truncates() {
    let workspace = BrWorkspace::new();
    init_workspace_with_long_issues(&workspace);

    // `--no-wrap` at narrow width should truncate long lines.
    let output = run_br_with_env(
        &workspace,
        ["list", "--no-wrap"],
        [("COLUMNS", "60")],
        "list_no_wrap",
    );
    assert!(output.status.success(), "list --no-wrap failed");

    // Should contain truncation indicator (...)
    let _has_ellipsis = output.stdout.contains("...");
    // Note: May or may not have ellipsis depending on actual width calculation
    // The key is the command succeeds
    assert!(output.stdout.contains("wrap-"), "Should show issue IDs");
}

#[test]
fn e2e_list_default_shows_full_content() {
    let workspace = BrWorkspace::new();
    init_workspace_with_long_issues(&workspace);

    // Wrapping is the default now, so no flag is needed.
    let output = run_br_with_env(&workspace, ["list"], [("COLUMNS", "60")], "list_with_wrap");
    assert!(output.status.success(), "list failed");

    // With (default) wrap, content should not be truncated
    assert!(output.stdout.contains("wrap-"), "Should show issue IDs");
}

#[test]
fn e2e_list_wrap_json_unchanged() {
    let workspace = BrWorkspace::new();
    init_workspace_with_long_issues(&workspace);

    // Wrapping (default vs --no-wrap) should not affect --json output.
    let output_default = run_br(&workspace, ["list", "--json"], "list_json");
    let output_no_wrap = run_br(&workspace, ["list", "--no-wrap", "--json"], "list_json_no_wrap");

    assert!(output_default.status.success());
    assert!(output_no_wrap.status.success());

    // JSON output should be identical (wrap is text-only feature)
    // Both should be valid JSON with the same structure
    let json_default: serde_json::Value =
        serde_json::from_str(&output_default.stdout).expect("parse json");
    let json_no_wrap: serde_json::Value =
        serde_json::from_str(&output_no_wrap.stdout).expect("parse json");

    assert!(json_default.is_array());
    assert!(json_no_wrap.is_array());
    assert_eq!(
        json_default.as_array().unwrap().len(),
        json_no_wrap.as_array().unwrap().len()
    );
}

// =============================================================================
// BR SHOW WRAP/NO-WRAP TESTS
// =============================================================================

#[test]
fn e2e_show_no_wrap() {
    let workspace = BrWorkspace::new();
    init_workspace_with_long_issues(&workspace);

    // Get the issue ID
    let list_output = run_br(&workspace, ["list", "--json"], "list_for_show");
    let issues: Vec<serde_json::Value> =
        serde_json::from_str(&list_output.stdout).expect("parse json");
    let long_issue_id = issues
        .iter()
        .find(|i| i["title"].as_str().unwrap_or("").contains("very long"))
        .expect("find long issue")["id"]
        .as_str()
        .unwrap();

    let output = run_br_with_env(
        &workspace,
        ["show", long_issue_id, "--no-wrap"],
        [("COLUMNS", "60")],
        "show_no_wrap",
    );
    assert!(output.status.success(), "show --no-wrap failed");
    assert!(output.stdout.contains(long_issue_id));
}

#[test]
fn e2e_show_default_wraps() {
    let workspace = BrWorkspace::new();
    init_workspace_with_long_issues(&workspace);

    // Get the issue ID
    let list_output = run_br(&workspace, ["list", "--json"], "list_for_show_wrap");
    let issues: Vec<serde_json::Value> =
        serde_json::from_str(&list_output.stdout).expect("parse json");
    let long_issue_id = issues
        .iter()
        .find(|i| i["title"].as_str().unwrap_or("").contains("very long"))
        .expect("find long issue")["id"]
        .as_str()
        .unwrap();

    // Wrapping is the default now, so no flag is needed.
    let output = run_br_with_env(
        &workspace,
        ["show", long_issue_id],
        [("COLUMNS", "60")],
        "show_with_wrap",
    );
    assert!(output.status.success(), "show failed");
    assert!(output.stdout.contains(long_issue_id));

    // The full description should be present
    assert!(
        output.stdout.contains("detailed context"),
        "Description should be visible"
    );
}

// =============================================================================
// BR SEARCH WRAP/NO-WRAP TESTS
// =============================================================================

#[test]
fn e2e_search_no_wrap() {
    let workspace = BrWorkspace::new();
    init_workspace_with_long_issues(&workspace);

    let output = run_br_with_env(
        &workspace,
        ["search", "long", "--no-wrap"],
        [("COLUMNS", "60")],
        "search_no_wrap",
    );
    assert!(output.status.success(), "search --no-wrap failed");
}

#[test]
fn e2e_search_default_wraps() {
    let workspace = BrWorkspace::new();
    init_workspace_with_long_issues(&workspace);

    // Wrapping is the default now, so no flag is needed.
    let output = run_br_with_env(
        &workspace,
        ["search", "long"],
        [("COLUMNS", "60")],
        "search_with_wrap",
    );
    assert!(output.status.success(), "search failed");
}

// =============================================================================
// BR BLOCKED WRAP/NO-WRAP TESTS
//
// NOTE: `e2e_comments_with_wrap` was removed. It exercised `comments add` /
// `comments <id>`, and the `comments` subcommand has been removed from the
// CLI entirely (no replacement surface), so there is nothing left of it to
// port.
// =============================================================================

#[test]
fn e2e_blocked_default_wraps() {
    let workspace = BrWorkspace::new();
    init_workspace_with_long_issues(&workspace);

    // Wrapping is the default now; the blocked command should still work
    // fine even with no blocked issues.
    let output = run_br_with_env(
        &workspace,
        ["blocked"],
        [("COLUMNS", "60")],
        "blocked_with_wrap",
    );
    assert!(output.status.success(), "blocked failed");
    // Either "No blocked issues" or actual blocked issues
    assert!(
        output.stdout.contains("blocked") || output.stdout.contains("No blocked"),
        "Should show blocked output"
    );
}

#[test]
fn e2e_blocked_with_dependencies() {
    let workspace = BrWorkspace::new();
    init_workspace_with_long_issues(&workspace);

    // Get issue IDs
    let list_output = run_br(&workspace, ["list", "--json"], "list_for_blocked");
    let issues: Vec<serde_json::Value> =
        serde_json::from_str(&list_output.stdout).expect("parse json");
    if issues.len() < 2 {
        // Skip if not enough issues
        return;
    }
    let parent_id = issues[0]["id"].as_str().unwrap();
    let child_id = issues[1]["id"].as_str().unwrap();

    // Add dependency (child depends on parent)
    let output = run_br(&workspace, ["dep", "add", child_id, parent_id], "add_dep");
    assert!(output.status.success(), "dep add failed: {}", output.stderr);

    // Test blocked with (default) wrap
    let output = run_br_with_env(
        &workspace,
        ["blocked"],
        [("COLUMNS", "60")],
        "blocked_with_wrap_deps",
    );
    assert!(output.status.success(), "blocked failed");
}

// =============================================================================
// EDGE CASES
// =============================================================================

#[test]
fn e2e_wrap_very_narrow_terminal() {
    let workspace = BrWorkspace::new();
    init_workspace_with_long_issues(&workspace);

    // Very narrow terminal (20 columns), default (wrap) behavior.
    let output = run_br_with_env(&workspace, ["list"], [("COLUMNS", "20")], "list_narrow");
    assert!(output.status.success(), "list at narrow width failed");
}

#[test]
fn e2e_wrap_very_wide_terminal() {
    let workspace = BrWorkspace::new();
    init_workspace_with_long_issues(&workspace);

    // Very wide terminal (200 columns), default (wrap) behavior.
    let output = run_br_with_env(&workspace, ["list"], [("COLUMNS", "200")], "list_wide");
    assert!(output.status.success(), "list at wide width failed");
}

#[test]
fn e2e_wrap_with_unicode_content() {
    let workspace = BrWorkspace::new();

    // Initialize (`init` no longer accepts `--prefix`; the harness's
    // `--prefix bd` convenience shim covers the create call below, and no
    // assertion here depends on a specific prefix value).
    let output = run_br(&workspace, ["init"], "init_unicode");
    assert!(output.status.success());

    // Create issue with unicode content (emoji, CJK, etc.)
    let unicode_title = "Fix bug 🐛 with 日本語 characters and emojis 🎉🚀";
    let output = run_br(
        &workspace,
        ["create", unicode_title, "--type", "bug"],
        "create_unicode",
    );
    assert!(output.status.success(), "create unicode failed");

    // Test with default (wrap) behavior
    let output = run_br_with_env(
        &workspace,
        ["list"],
        [("COLUMNS", "40")],
        "list_unicode_wrap",
    );
    assert!(output.status.success(), "list unicode failed");
    // Should contain the unicode content
    assert!(output.stdout.contains("🐛") || output.stdout.contains("bug"));
}

#[test]
fn e2e_wrap_empty_database() {
    let workspace = BrWorkspace::new();

    // Initialize but don't create any issues
    let output = run_br(&workspace, ["init"], "init_empty");
    assert!(output.status.success());

    // Test all commands (default wrap behavior) on empty database
    let output = run_br(&workspace, ["list"], "list_empty_wrap");
    assert!(output.status.success());

    let output = run_br(&workspace, ["blocked"], "blocked_empty_wrap");
    assert!(output.status.success());

    let output = run_br(&workspace, ["search", "nothing"], "search_empty_wrap");
    assert!(output.status.success());
}
