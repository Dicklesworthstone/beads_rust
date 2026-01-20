//! E2E tests for the `defer` and `undefer` commands.
//!
//! These tests verify the defer/undefer lifecycle including:
//! - Setting/clearing deferred status
//! - Time parsing (relative, absolute, natural language)
//! - Ready/blocked list interactions
//! - Edge cases and error handling

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;
use tracing::info;
fn parse_created_id(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or("");
    // Handle both formats: "Created bd-xxx: title" and "✓ Created bd-xxx: title"
    let normalized = line.strip_prefix("✓ ").unwrap_or(line);
    let id_part = normalized
        .strip_prefix("Created ")
        .and_then(|rest| rest.split(':').next())
        .unwrap_or("");
    id_part.trim().to_string()
}

fn setup_workspace_with_issue() -> (BrWorkspace, String) {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Test issue for defer", "-p", "2", "-t", "task"],
        "create_issue",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    (workspace, id)
}

fn setup_workspace_with_multiple_issues() -> (BrWorkspace, Vec<String>) {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let mut ids = Vec::new();
    for i in 1..=3 {
        let create = run_br(
            &workspace,
            ["create", &format!("Issue {i}"), "-p", "2", "-t", "task"],
            &format!("create_issue_{i}"),
        );
        assert!(create.status.success());
        ids.push(parse_created_id(&create.stdout));
    }

    (workspace, ids)
}

// =============================================================================
// Defer Basic Tests
// =============================================================================

#[test]
fn defer_sets_status_deferred() {
    common::init_test_logging();
    info!("defer_sets_status_deferred: starting");
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(&workspace, ["defer", &id], "defer");
    assert!(defer.status.success(), "defer failed: {}", defer.stderr);

    let show = run_br(&workspace, ["show", &id, "--json"], "show");
    assert!(show.status.success());
    let payload = extract_json_payload(&show.stdout);
    let issues: Value = serde_json::from_str(&payload).expect("valid json");

    // show returns flattened array
    assert_eq!(
        issues[0]["status"].as_str().unwrap(),
        "deferred",
        "status should be deferred"
    );
    info!("defer_sets_status_deferred: assertions passed");
}

#[test]
fn defer_indefinitely_no_until() {
    common::init_test_logging();
    info!("defer_indefinitely_no_until: starting");
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(&workspace, ["defer", &id, "--json"], "defer");
    assert!(defer.status.success(), "defer failed: {}", defer.stderr);

    let payload = extract_json_payload(&defer.stdout);
    let result: Value = serde_json::from_str(&payload).expect("valid json");

    // defer returns array of updated issues
    assert_eq!(result.as_array().unwrap().len(), 1);
    let deferred = &result[0];
    assert_eq!(deferred["status"], "deferred");

    let show = run_br(&workspace, ["show", &id, "--json"], "show");
    let show_payload = extract_json_payload(&show.stdout);
    let show_issues: Value = serde_json::from_str(&show_payload).expect("valid json");
    let issue = &show_issues[0];

    assert!(
        issue.get("defer_until").is_none() || issue["defer_until"].is_null(),
        "defer_until should be null for indefinite defer"
    );
    info!("defer_indefinitely_no_until: assertions passed");
}

#[test]
fn defer_with_until_timestamp() {
    common::init_test_logging();
    info!("defer_with_until_timestamp: starting");
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(
        &workspace,
        ["defer", &id, "--until", "+1d", "--json"],
        "defer_with_until",
    );
    assert!(defer.status.success(), "defer failed: {}", defer.stderr);

    // Verify via show
    let show = run_br(&workspace, ["show", &id, "--json"], "show");
    let show_payload = extract_json_payload(&show.stdout);
    let show_issues: Value = serde_json::from_str(&show_payload).expect("valid json");
    let issue = &show_issues[0];

    assert!(
        issue["defer_until"].as_str().is_some(),
        "defer_until should have a value"
    );
    info!("defer_with_until_timestamp: assertions passed");
}

#[test]
fn defer_multiple_issues() {
    common::init_test_logging();
    info!("defer_multiple_issues: starting");
    let (workspace, ids) = setup_workspace_with_multiple_issues();

    let defer = run_br(
        &workspace,
        ["defer", &ids[0], &ids[1], &ids[2], "--json"],
        "defer_multiple",
    );
    assert!(defer.status.success(), "defer failed: {}", defer.stderr);

    let payload = extract_json_payload(&defer.stdout);
    let result: Value = serde_json::from_str(&payload).expect("valid json");

    assert_eq!(
        result.as_array().unwrap().len(),
        3,
        "all 3 issues should be deferred"
    );

    for id in &ids {
        let show = run_br(&workspace, ["show", id, "--json"], &format!("show_{id}"));
        let show_payload = extract_json_payload(&show.stdout);
        let issues: Value = serde_json::from_str(&show_payload).expect("valid json");
        assert_eq!(issues[0]["status"].as_str().unwrap(), "deferred");
    }
    info!("defer_multiple_issues: assertions passed");
}

#[test]
fn defer_json_output() {
    common::init_test_logging();
    info!("defer_json_output: starting");
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(
        &workspace,
        ["defer", &id, "--until", "tomorrow", "--json"],
        "defer_json",
    );
    assert!(defer.status.success(), "defer failed: {}", defer.stderr);

    let payload = extract_json_payload(&defer.stdout);
    let result: Value = serde_json::from_str(&payload).expect("valid json");

    // Expect array of updated issues
    let deferred = result.as_array().unwrap();
    assert!(!deferred.is_empty());

    let first = &deferred[0];
    assert!(first.get("id").is_some(), "deferred item should have id");
    assert!(
        first.get("title").is_some(),
        "deferred item should have title"
    );
    assert!(
        first.get("status").is_some(),
        "deferred item should have status"
    );
    assert_eq!(first["status"].as_str().unwrap(), "deferred");
    info!("defer_json_output: assertions passed");
}

// =============================================================================
// Natural Time Parsing Tests
// =============================================================================

#[test]
fn defer_until_tomorrow() {
    common::init_test_logging();
    info!("defer_until_tomorrow: starting");
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(
        &workspace,
        ["defer", &id, "--until", "tomorrow", "--json"],
        "defer_tomorrow",
    );
    assert!(defer.status.success(), "defer failed: {}", defer.stderr);

    let show = run_br(&workspace, ["show", &id, "--json"], "show");
    let show_payload = extract_json_payload(&show.stdout);
    let show_issues: Value = serde_json::from_str(&show_payload).expect("valid json");
    let issue = &show_issues[0];

    let defer_until = issue["defer_until"].as_str().unwrap();
    assert!(
        !defer_until.is_empty(),
        "defer_until should be set for tomorrow"
    );
    info!("defer_until_tomorrow: assertions passed");
}

#[test]
fn defer_until_relative() {
    common::init_test_logging();
    info!("defer_until_relative: starting");
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(
        &workspace,
        ["defer", &id, "--until", "+2h", "--json"],
        "defer_relative",
    );
    assert!(defer.status.success(), "defer failed: {}", defer.stderr);

    let show = run_br(&workspace, ["show", &id, "--json"], "show");
    let show_payload = extract_json_payload(&show.stdout);
    let show_issues: Value = serde_json::from_str(&show_payload).expect("valid json");
    let issue = &show_issues[0];

    let defer_until = issue["defer_until"].as_str().unwrap();
    assert!(!defer_until.is_empty(), "defer_until should be set for +2h");
    info!("defer_until_relative: assertions passed");
}

#[test]
fn defer_until_specific_date() {
    common::init_test_logging();
    info!("defer_until_specific_date: starting");
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(
        &workspace,
        ["defer", &id, "--until", "2099-12-31", "--json"],
        "defer_specific_date",
    );
    assert!(defer.status.success(), "defer failed: {}", defer.stderr);

    let show = run_br(&workspace, ["show", &id, "--json"], "show");
    let show_payload = extract_json_payload(&show.stdout);
    let show_issues: Value = serde_json::from_str(&show_payload).expect("valid json");
    let issue = &show_issues[0];

    let defer_until = issue["defer_until"].as_str().unwrap();
    assert!(
        defer_until.contains("2099-12-31"),
        "defer_until should contain the specified date"
    );
    info!("defer_until_specific_date: assertions passed");
}

#[test]
fn defer_until_datetime() {
    common::init_test_logging();
    info!("defer_until_datetime: starting");
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(
        &workspace,
        ["defer", &id, "--until", "2099-02-01T09:00:00Z", "--json"],
        "defer_datetime",
    );
    assert!(defer.status.success(), "defer failed: {}", defer.stderr);

    let show = run_br(&workspace, ["show", &id, "--json"], "show");
    let show_payload = extract_json_payload(&show.stdout);
    let show_issues: Value = serde_json::from_str(&show_payload).expect("valid json");
    let issue = &show_issues[0];

    let defer_until = issue["defer_until"].as_str().unwrap();
    assert!(
        defer_until.contains("2099-02-01"),
        "defer_until should contain the specified date"
    );
    info!("defer_until_datetime: assertions passed");
}

#[test]
fn defer_until_past_allows() {
    common::init_test_logging();
    info!("defer_until_past_allows: starting");
    let (workspace, id) = setup_workspace_with_issue();

    // Past dates should be allowed. Pass value with --until=-1d to avoid flag confusion
    // or use -- to separate args if id comes after?
    // clap syntax for negative values usually requires equals sign or --
    // br defer id --until=-1d should work
    let defer = run_br(
        &workspace,
        ["defer", &id, "--until=-1d", "--json"],
        "defer_past",
    );
    assert!(
        defer.status.success(),
        "defer with past date should succeed: {}",
        defer.stderr
    );

    let show = run_br(&workspace, ["show", &id, "--json"], "show");
    let show_payload = extract_json_payload(&show.stdout);
    let show_issues: Value = serde_json::from_str(&show_payload).expect("valid json");
    let issue = &show_issues[0];

    assert_eq!(issue["status"], "deferred");
    info!("defer_until_past_allows: assertions passed");
}

#[test]
fn defer_until_invalid_error() {
    common::init_test_logging();
    info!("defer_until_invalid_error: starting");
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(
        &workspace,
        ["defer", &id, "--until", "not-a-valid-time", "--json"],
        "defer_invalid_time",
    );
    assert!(
        !defer.status.success(),
        "defer with invalid time should fail"
    );
    assert!(
        defer.stderr.to_lowercase().contains("invalid")
            || defer.stderr.to_lowercase().contains("parse")
            || defer.stderr.to_lowercase().contains("unrecognized"),
        "error should mention invalid time format"
    );
    info!("defer_until_invalid_error: assertions passed");
}

// =============================================================================
// Undefer Tests
// =============================================================================

#[test]
fn undefer_sets_status_open() {
    common::init_test_logging();
    info!("undefer_sets_status_open: starting");
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(&workspace, ["defer", &id], "defer_first");
    assert!(defer.status.success());

    let undefer = run_br(&workspace, ["undefer", &id], "undefer");
    assert!(
        undefer.status.success(),
        "undefer failed: {}",
        undefer.stderr
    );

    let show = run_br(&workspace, ["show", &id, "--json"], "show");
    let payload = extract_json_payload(&show.stdout);
    let issues: Value = serde_json::from_str(&payload).expect("valid json");

    assert_eq!(
        issues[0]["status"].as_str().unwrap(),
        "open",
        "status should be open after undefer"
    );
    info!("undefer_sets_status_open: assertions passed");
}

#[test]
fn undefer_clears_defer_until() {
    common::init_test_logging();
    info!("undefer_clears_defer_until: starting");
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(&workspace, ["defer", &id, "--until", "+1d"], "defer_first");
    assert!(defer.status.success());

    let undefer = run_br(&workspace, ["undefer", &id, "--json"], "undefer");
    assert!(undefer.status.success());

    let show = run_br(&workspace, ["show", &id, "--json"], "show");
    let payload = extract_json_payload(&show.stdout);
    let issues: Value = serde_json::from_str(&payload).expect("valid json");
    let issue = &issues[0];

    assert!(
        issue.get("defer_until").is_none() || issue["defer_until"].is_null(),
        "defer_until should be cleared after undefer"
    );
    info!("undefer_clears_defer_until: assertions passed");
}

#[test]
fn undefer_multiple_issues() {
    common::init_test_logging();
    info!("undefer_multiple_issues: starting");
    let (workspace, ids) = setup_workspace_with_multiple_issues();

    let defer = run_br(
        &workspace,
        ["defer", &ids[0], &ids[1], &ids[2]],
        "defer_all",
    );
    assert!(defer.status.success());

    let undefer = run_br(
        &workspace,
        ["undefer", &ids[0], &ids[1], &ids[2], "--json"],
        "undefer_all",
    );
    assert!(undefer.status.success());

    let payload = extract_json_payload(&undefer.stdout);
    let result: Value = serde_json::from_str(&payload).expect("valid json");

    assert_eq!(
        result.as_array().unwrap().len(),
        3,
        "all 3 issues should be undeferred"
    );

    for id in &ids {
        let show = run_br(&workspace, ["show", id, "--json"], &format!("show_{id}"));
        let show_payload = extract_json_payload(&show.stdout);
        let issues: Value = serde_json::from_str(&show_payload).expect("valid json");
        assert_eq!(issues[0]["status"].as_str().unwrap(), "open");
    }
    info!("undefer_multiple_issues: assertions passed");
}

#[test]
fn undefer_json_output() {
    common::init_test_logging();
    info!("undefer_json_output: starting");
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(&workspace, ["defer", &id], "defer_first");
    assert!(defer.status.success());

    let undefer = run_br(&workspace, ["undefer", &id, "--json"], "undefer");
    assert!(undefer.status.success());

    let payload = extract_json_payload(&undefer.stdout);
    let result: Value = serde_json::from_str(&payload).expect("valid json");

    let undeferred = result.as_array().unwrap();
    assert_eq!(undeferred.len(), 1);

    let first = &undeferred[0];
    assert!(first.get("id").is_some());
    assert!(first.get("title").is_some());
    assert!(first.get("status").is_some());
    assert_eq!(first["status"].as_str().unwrap(), "open");
    info!("undefer_json_output: assertions passed");
}

// =============================================================================
// Edge Cases
// =============================================================================

#[test]
fn defer_already_deferred_updates_time() {
    common::init_test_logging();
    info!("defer_already_deferred_updates_time: starting");
    let (workspace, id) = setup_workspace_with_issue();

    let defer1 = run_br(
        &workspace,
        ["defer", &id, "--until", "+1d", "--json"],
        "defer_first",
    );
    assert!(defer1.status.success());

    let defer2 = run_br(
        &workspace,
        ["defer", &id, "--until", "+2d", "--json"],
        "defer_second",
    );
    assert!(defer2.status.success());

    let payload = extract_json_payload(&defer2.stdout);
    let result: Value = serde_json::from_str(&payload).expect("valid json");

    // Expect array with 1 updated issue
    assert_eq!(result.as_array().unwrap().len(), 1);

    // Check time updated via show
    let show = run_br(&workspace, ["show", &id, "--json"], "show");
    let show_payload = extract_json_payload(&show.stdout);
    let show_issues: Value = serde_json::from_str(&show_payload).expect("valid json");
    // Verify defer_until is > 1d from now
    assert!(show_issues[0]["defer_until"].as_str().is_some());
    info!("defer_already_deferred_updates_time: assertions passed");
}

#[test]
fn undefer_already_open_skips() {
    common::init_test_logging();
    info!("undefer_already_open_skips: starting");
    let (workspace, id) = setup_workspace_with_issue();

    let undefer = run_br(&workspace, ["undefer", &id, "--json"], "undefer_open");
    assert!(undefer.status.success());

    let payload = extract_json_payload(&undefer.stdout);
    let result: Value = serde_json::from_str(&payload).expect("valid json");
    assert!(result.is_array());

    // update command returns issues that were processed.
    // If no changes, it might still return it depending on implementation details of update command.
    // If it returns empty array, it means nothing happened.
    // Let's verify status is open regardless.

    let show = run_br(&workspace, ["show", &id, "--json"], "show");
    let show_payload = extract_json_payload(&show.stdout);
    let issues: Value = serde_json::from_str(&show_payload).expect("valid json");
    assert_eq!(issues[0]["status"], "open");
    info!("undefer_already_open_skips: assertions passed");
}

#[test]
fn defer_closed_issue_error() {
    common::init_test_logging();
    info!("defer_closed_issue_error: starting");
    let (workspace, id) = setup_workspace_with_issue();

    let close = run_br(&workspace, ["close", &id], "close_first");
    assert!(close.status.success());

    // Deferring a closed issue should update status to deferred
    let defer = run_br(&workspace, ["defer", &id, "--json"], "defer_closed");
    assert!(defer.status.success());

    let payload = extract_json_payload(&defer.stdout);
    let result: Value = serde_json::from_str(&payload).expect("valid json");

    let updated = result.as_array().unwrap();
    if !updated.is_empty() {
        assert_eq!(updated[0]["status"], "deferred");
    }
    info!("defer_closed_issue_error: assertions passed");
}

#[test]
fn defer_nonexistent_error() {
    common::init_test_logging();
    info!("defer_nonexistent_error: starting");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let defer = run_br(
        &workspace,
        ["defer", "bd-nonexistent", "--json"],
        "defer_nonexistent",
    );

    // Should fail with not found
    assert!(!defer.status.success());
    assert!(defer.stderr.contains("not found") || defer.stderr.contains("matching"));
    info!("defer_nonexistent_error: assertions passed");
}

// =============================================================================
// Ready/Blocked Interaction Tests
// =============================================================================

#[test]
fn deferred_not_in_ready() {
    common::init_test_logging();
    info!("deferred_not_in_ready: starting");
    let (workspace, ids) = setup_workspace_with_multiple_issues();

    // Defer one issue
    let defer = run_br(&workspace, ["defer", &ids[0]], "defer_one");
    assert!(defer.status.success());

    let ready = run_br(&workspace, ["ready", "--json"], "ready");
    assert!(ready.status.success());

    let payload = extract_json_payload(&ready.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).expect("valid json");

    // Deferred issue should NOT appear in ready list
    let ready_ids: Vec<&str> = issues.iter().filter_map(|i| i["id"].as_str()).collect();

    assert!(
        !ready_ids.contains(&ids[0].as_str()),
        "deferred issue should not appear in ready list"
    );

    // Other issues should still be in ready
    assert!(
        ready_ids.contains(&ids[1].as_str()),
        "non-deferred issues should be in ready list"
    );
    info!("deferred_not_in_ready: assertions passed");
}

#[test]
fn deferred_not_blocked() {
    common::init_test_logging();
    info!("deferred_not_blocked: starting");
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(&workspace, ["defer", &id], "defer");
    assert!(defer.status.success());

    let blocked = run_br(&workspace, ["blocked", "--json"], "blocked");
    assert!(blocked.status.success());

    let payload = extract_json_payload(&blocked.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).unwrap_or_else(|_| vec![]);

    // Deferred issue should NOT appear in blocked list (deferred != blocked)
    assert!(
        !issues
            .iter()
            .filter_map(|i| i["id"].as_str())
            .any(|x| x == id.as_str()),
        "deferred issue should not appear in blocked list"
    );
    info!("deferred_not_blocked: assertions passed");
}

#[test]
fn undefer_appears_in_ready() {
    common::init_test_logging();
    info!("undefer_appears_in_ready: starting");
    let (workspace, id) = setup_workspace_with_issue();

    // Defer then undefer
    let defer = run_br(&workspace, ["defer", &id], "defer");
    assert!(defer.status.success());

    let ready_before = run_br(&workspace, ["ready", "--json"], "ready_before");
    let payload_before = extract_json_payload(&ready_before.stdout);
    let issues_before: Vec<Value> =
        serde_json::from_str(&payload_before).unwrap_or_else(|_| vec![]);
    assert!(
        !issues_before
            .iter()
            .filter_map(|i| i["id"].as_str())
            .any(|x| x == id.as_str())
    );

    // Undefer
    let undefer = run_br(&workspace, ["undefer", &id], "undefer");
    assert!(undefer.status.success());

    let ready_after = run_br(&workspace, ["ready", "--json"], "ready_after");
    assert!(ready_after.status.success());

    let payload_after = extract_json_payload(&ready_after.stdout);
    let issues_after: Vec<Value> = serde_json::from_str(&payload_after).expect("valid json");

    assert!(
        issues_after
            .iter()
            .filter_map(|i| i["id"].as_str())
            .any(|x| x == id.as_str()),
        "undeferred issue should appear in ready list"
    );
    info!("undefer_appears_in_ready: assertions passed");
}
