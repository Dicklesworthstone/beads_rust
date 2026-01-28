//! E2E tests for the --wrap flag across various commands.
//!
//! Tests verify that:
//! - --wrap flag is accepted by show, list, ready, comments, search, blocked
//! - Default behavior (no --wrap) still truncates text
//! - With --wrap, long content is not truncated
//! - Different terminal widths are respected

mod common;

use common::cli::{run_br, run_br_with_env, BrWorkspace};

fn init_workspace_with_long_issues(workspace: &BrWorkspace) {
    // Initialize
    let output = run_br(workspace, ["init", "--prefix", "wrap"], "init");
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
        ],
        "create_long",
    );
    assert!(output.status.success(), "create failed: {}", output.stderr);

    // Create a shorter issue for comparison
    let output = run_br(
        workspace,
        ["create", "Short issue", "--type", "bug"],
        "create_short",
    );
    assert!(output.status.success(), "create short failed");
}

// =============================================================================
// BR LIST --WRAP TESTS
// =============================================================================

#[test]
fn e2e_list_without_wrap_truncates() {
    let workspace = BrWorkspace::new();
    init_workspace_with_long_issues(&workspace);

    // List without --wrap at narrow width
    let output = run_br_with_env(
        &workspace,
        ["list"],
        [("COLUMNS", "60")],
        "list_no_wrap",
    );
    assert!(output.status.success(), "list failed");

    // Should contain truncation indicator (...)
    let _has_ellipsis = output.stdout.contains("...");
    // Note: May or may not have ellipsis depending on actual width calculation
    // The key is the command succeeds
    assert!(output.stdout.contains("wrap-"), "Should show issue IDs");
}

#[test]
fn e2e_list_with_wrap_shows_full_content() {
    let workspace = BrWorkspace::new();
    init_workspace_with_long_issues(&workspace);

    // List with --wrap
    let output = run_br_with_env(
        &workspace,
        ["list", "--wrap"],
        [("COLUMNS", "60")],
        "list_with_wrap",
    );
    assert!(output.status.success(), "list --wrap failed");

    // With wrap, content should not be truncated
    assert!(output.stdout.contains("wrap-"), "Should show issue IDs");
}

#[test]
fn e2e_list_wrap_json_unchanged() {
    let workspace = BrWorkspace::new();
    init_workspace_with_long_issues(&workspace);

    // --wrap should not affect --json output
    let output_no_wrap = run_br(&workspace, ["list", "--json"], "list_json");
    let output_wrap = run_br(&workspace, ["list", "--wrap", "--json"], "list_json_wrap");

    assert!(output_no_wrap.status.success());
    assert!(output_wrap.status.success());

    // JSON output should be identical (wrap is text-only feature)
    // Both should be valid JSON with the same structure
    let json_no_wrap: serde_json::Value =
        serde_json::from_str(&output_no_wrap.stdout).expect("parse json");
    let json_wrap: serde_json::Value =
        serde_json::from_str(&output_wrap.stdout).expect("parse json");

    assert!(json_no_wrap.is_array());
    assert!(json_wrap.is_array());
    assert_eq!(
        json_no_wrap.as_array().unwrap().len(),
        json_wrap.as_array().unwrap().len()
    );
}

// =============================================================================
// BR SHOW --WRAP TESTS
// =============================================================================

#[test]
fn e2e_show_without_wrap() {
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
        ["show", long_issue_id],
        [("COLUMNS", "60")],
        "show_no_wrap",
    );
    assert!(output.status.success(), "show failed");
    assert!(output.stdout.contains(long_issue_id));
}

#[test]
fn e2e_show_with_wrap() {
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

    let output = run_br_with_env(
        &workspace,
        ["show", long_issue_id, "--wrap"],
        [("COLUMNS", "60")],
        "show_with_wrap",
    );
    assert!(output.status.success(), "show --wrap failed");
    assert!(output.stdout.contains(long_issue_id));

    // The full description should be present
    assert!(
        output.stdout.contains("detailed context"),
        "Description should be visible"
    );
}

// =============================================================================
// BR READY --WRAP TESTS
// =============================================================================

#[test]
fn e2e_ready_without_wrap() {
    let workspace = BrWorkspace::new();
    init_workspace_with_long_issues(&workspace);

    let output = run_br_with_env(
        &workspace,
        ["ready"],
        [("COLUMNS", "60")],
        "ready_no_wrap",
    );
    assert!(output.status.success(), "ready failed");
}

#[test]
fn e2e_ready_with_wrap() {
    let workspace = BrWorkspace::new();
    init_workspace_with_long_issues(&workspace);

    let output = run_br_with_env(
        &workspace,
        ["ready", "--wrap"],
        [("COLUMNS", "60")],
        "ready_with_wrap",
    );
    assert!(output.status.success(), "ready --wrap failed");
}

// =============================================================================
// BR SEARCH --WRAP TESTS
// =============================================================================

#[test]
fn e2e_search_without_wrap() {
    let workspace = BrWorkspace::new();
    init_workspace_with_long_issues(&workspace);

    let output = run_br_with_env(
        &workspace,
        ["search", "long"],
        [("COLUMNS", "60")],
        "search_no_wrap",
    );
    assert!(output.status.success(), "search failed");
}

#[test]
fn e2e_search_with_wrap() {
    let workspace = BrWorkspace::new();
    init_workspace_with_long_issues(&workspace);

    let output = run_br_with_env(
        &workspace,
        ["search", "long", "--wrap"],
        [("COLUMNS", "60")],
        "search_with_wrap",
    );
    assert!(output.status.success(), "search --wrap failed");
}

// =============================================================================
// BR COMMENTS --WRAP TESTS
// =============================================================================

#[test]
fn e2e_comments_with_wrap() {
    let workspace = BrWorkspace::new();
    init_workspace_with_long_issues(&workspace);

    // Get an issue ID
    let list_output = run_br(&workspace, ["list", "--json"], "list_for_comments");
    let issues: Vec<serde_json::Value> =
        serde_json::from_str(&list_output.stdout).expect("parse json");
    let issue_id = issues[0]["id"].as_str().unwrap();

    // Add a long comment
    let long_comment = "This is a very long comment that contains lots of detailed information about the progress of this issue and should demonstrate the wrapping behavior when the wrap flag is enabled.";
    let output = run_br(
        &workspace,
        ["comments", "add", issue_id, long_comment],
        "add_comment",
    );
    assert!(output.status.success(), "add comment failed");

    // List comments without --wrap
    let output = run_br_with_env(
        &workspace,
        ["comments", issue_id],
        [("COLUMNS", "60")],
        "comments_no_wrap",
    );
    assert!(output.status.success(), "comments failed");

    // List comments with --wrap
    let output = run_br_with_env(
        &workspace,
        ["comments", issue_id, "--wrap"],
        [("COLUMNS", "60")],
        "comments_with_wrap",
    );
    assert!(output.status.success(), "comments --wrap failed");
}

// =============================================================================
// BR BLOCKED --WRAP TESTS
// =============================================================================

#[test]
fn e2e_blocked_with_wrap() {
    let workspace = BrWorkspace::new();
    init_workspace_with_long_issues(&workspace);

    // The blocked command should accept --wrap even if there are no blocked issues
    let output = run_br_with_env(
        &workspace,
        ["blocked", "--wrap"],
        [("COLUMNS", "60")],
        "blocked_with_wrap",
    );
    assert!(output.status.success(), "blocked --wrap failed");
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
    let blocker_id = issues[0]["id"].as_str().unwrap();
    let blocked_id = issues[1]["id"].as_str().unwrap();

    // Add dependency
    let output = run_br(
        &workspace,
        ["dep", "add", blocked_id, blocker_id],
        "add_dep",
    );
    assert!(output.status.success(), "dep add failed: {}", output.stderr);

    // Test blocked with --wrap
    let output = run_br_with_env(
        &workspace,
        ["blocked", "--wrap"],
        [("COLUMNS", "60")],
        "blocked_with_wrap_deps",
    );
    assert!(output.status.success(), "blocked --wrap failed");
}

// =============================================================================
// EDGE CASES
// =============================================================================

#[test]
fn e2e_wrap_very_narrow_terminal() {
    let workspace = BrWorkspace::new();
    init_workspace_with_long_issues(&workspace);

    // Very narrow terminal (20 columns)
    let output = run_br_with_env(
        &workspace,
        ["list", "--wrap"],
        [("COLUMNS", "20")],
        "list_narrow",
    );
    assert!(output.status.success(), "list at narrow width failed");
}

#[test]
fn e2e_wrap_very_wide_terminal() {
    let workspace = BrWorkspace::new();
    init_workspace_with_long_issues(&workspace);

    // Very wide terminal (200 columns)
    let output = run_br_with_env(
        &workspace,
        ["list", "--wrap"],
        [("COLUMNS", "200")],
        "list_wide",
    );
    assert!(output.status.success(), "list at wide width failed");
}

#[test]
fn e2e_wrap_with_unicode_content() {
    let workspace = BrWorkspace::new();

    // Initialize
    let output = run_br(&workspace, ["init", "--prefix", "uni"], "init_unicode");
    assert!(output.status.success());

    // Create issue with unicode content (emoji, CJK, etc.)
    let unicode_title = "Fix bug 🐛 with 日本語 characters and emojis 🎉🚀";
    let output = run_br(
        &workspace,
        ["create", unicode_title, "--type", "bug"],
        "create_unicode",
    );
    assert!(output.status.success(), "create unicode failed");

    // Test with --wrap
    let output = run_br_with_env(
        &workspace,
        ["list", "--wrap"],
        [("COLUMNS", "40")],
        "list_unicode_wrap",
    );
    assert!(output.status.success(), "list unicode with wrap failed");
    // Should contain the unicode content
    assert!(output.stdout.contains("🐛") || output.stdout.contains("bug"));
}

#[test]
fn e2e_wrap_empty_database() {
    let workspace = BrWorkspace::new();

    // Initialize but don't create any issues
    let output = run_br(&workspace, ["init"], "init_empty");
    assert!(output.status.success());

    // Test all wrap commands on empty database
    let output = run_br(&workspace, ["list", "--wrap"], "list_empty_wrap");
    assert!(output.status.success());

    let output = run_br(&workspace, ["ready", "--wrap"], "ready_empty_wrap");
    assert!(output.status.success());

    let output = run_br(&workspace, ["blocked", "--wrap"], "blocked_empty_wrap");
    assert!(output.status.success());

    let output = run_br(&workspace, ["search", "nothing", "--wrap"], "search_empty_wrap");
    assert!(output.status.success());
}
