//! Demo test using the new E2E harness foundation
//!
//! This test validates that the harness:
//! - Creates isolated workspaces
//! - Captures command output and timing
//! - Logs JSONL events
//! - Takes file tree snapshots

mod common;

use common::harness::{TestWorkspace, extract_json_payload, parse_created_id};

#[test]
fn harness_full_workflow() {
    let mut ws = TestWorkspace::new("e2e_harness_demo", "full_workflow");

    // Initialize workspace
    let init = ws.run_br(["init"], "init");
    init.assert_success();
    assert!(init.duration.as_secs() < 10, "init too slow");

    // Create an issue
    let create = ws.run_br(
        [
            "create",
            "Test harness issue",
            "--type",
            "task",
            "--priority",
            "2",
        ],
        "create_issue",
    );
    create.assert_success();

    // Parse created ID
    let id = parse_created_id(&create.stdout);
    assert!(!id.is_empty(), "missing created id");

    // List issues in JSON mode
    let list = ws.run_br(["list", "--json"], "list_json");
    list.assert_success();

    let payload = extract_json_payload(&list.stdout);
    let issues: Vec<serde_json::Value> = serde_json::from_str(&payload).expect("parse list json");
    assert!(!issues.is_empty(), "no issues found");
    assert!(
        issues.iter().any(|i| i["id"].as_str() == Some(&id)),
        "created issue not in list"
    );

    // Update the issue
    let update = ws.run_br(["update", &id, "--status", "in_progress"], "update_status");
    update.assert_success();

    // Show the issue
    let show = ws.run_br(["show", &id, "--json"], "show_json");
    show.assert_success();

    // Close the issue
    let close = ws.run_br(["close", &id, "--reason", "Test complete"], "close");
    close.assert_success();

    // Finalize
    ws.finish(true);
}

#[test]
fn harness_captures_failure() {
    let mut ws = TestWorkspace::new("e2e_harness_demo", "captures_failure");

    // Initialize
    let init = ws.run_br(["init"], "init");
    init.assert_success();

    // Try an invalid command (show nonexistent issue)
    let show = ws.run_br(["show", "nonexistent-id"], "show_invalid");
    show.assert_failure();

    // Verify error was captured
    assert!(
        !show.stderr.is_empty() || !show.stdout.is_empty(),
        "expected some output on error"
    );

    ws.finish(true);
}

#[test]
fn harness_env_isolation() {
    let mut ws = TestWorkspace::new("e2e_harness_demo", "env_isolation");

    // Initialize
    let init = ws.run_br(["init"], "init");
    init.assert_success();

    // Run with custom env var
    let result = ws.run_br_env(
        ["info", "--json"],
        [("BR_TEST_VAR", "test_value")],
        "info_with_env",
    );
    result.assert_success();

    ws.finish(true);
}

#[test]
fn harness_stdin_input() {
    let mut ws = TestWorkspace::new("e2e_harness_demo", "stdin_input");

    // Initialize
    let init = ws.run_br(["init"], "init");
    init.assert_success();

    // Create an issue first
    let create = ws.run_br(["create", "Issue for comment"], "create");
    create.assert_success();

    let id = parse_created_id(&create.stdout);
    assert!(!id.is_empty(), "missing created id");

    // Add comment via stdin
    let comment = ws.run_br_stdin(
        ["comments", "add", &id, "-"],
        "This is a comment from stdin",
        "add_comment_stdin",
    );
    comment.assert_success();

    ws.finish(true);
}
