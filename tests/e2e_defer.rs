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

fn parse_created_id(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or("");
    let id_part = line
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
// Defer Basic Tests (5 tests)
// =============================================================================

#[test]
fn defer_sets_status_deferred() {
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(&workspace, ["defer", &id], "defer");
    assert!(defer.status.success(), "defer failed: {}", defer.stderr);

    let show = run_br(&workspace, ["show", &id, "--json"], "show");
    assert!(show.status.success());
    let payload = extract_json_payload(&show.stdout);
    let issue: Value = serde_json::from_str(&payload).expect("valid json");

    assert_eq!(
        issue["status"].as_str().unwrap(),
        "deferred",
        "status should be deferred"
    );
}

#[test]
fn defer_indefinitely_no_until() {
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(&workspace, ["defer", &id, "--json"], "defer");
    assert!(defer.status.success(), "defer failed: {}", defer.stderr);

    let payload = extract_json_payload(&defer.stdout);
    let result: Value = serde_json::from_str(&payload).expect("valid json");

    assert_eq!(result["deferred"].as_array().unwrap().len(), 1);
    let deferred = &result["deferred"][0];
    assert!(
        deferred.get("defer_until").is_none() || deferred["defer_until"].is_null(),
        "defer_until should be null for indefinite defer"
    );
}

#[test]
fn defer_with_until_timestamp() {
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(
        &workspace,
        ["defer", &id, "--until", "+1d", "--json"],
        "defer_with_until",
    );
    assert!(defer.status.success(), "defer failed: {}", defer.stderr);

    let payload = extract_json_payload(&defer.stdout);
    let result: Value = serde_json::from_str(&payload).expect("valid json");

    assert_eq!(result["deferred"].as_array().unwrap().len(), 1);
    let deferred = &result["deferred"][0];
    assert!(
        deferred["defer_until"].as_str().is_some(),
        "defer_until should have a value"
    );
}

#[test]
fn defer_multiple_issues() {
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
        result["deferred"].as_array().unwrap().len(),
        3,
        "all 3 issues should be deferred"
    );

    for id in &ids {
        let show = run_br(&workspace, ["show", id, "--json"], &format!("show_{id}"));
        let show_payload = extract_json_payload(&show.stdout);
        let issue: Value = serde_json::from_str(&show_payload).expect("valid json");
        assert_eq!(issue["status"].as_str().unwrap(), "deferred");
    }
}

#[test]
fn defer_json_output() {
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(
        &workspace,
        ["defer", &id, "--until", "tomorrow", "--json"],
        "defer_json",
    );
    assert!(defer.status.success(), "defer failed: {}", defer.stderr);

    let payload = extract_json_payload(&defer.stdout);
    let result: Value = serde_json::from_str(&payload).expect("valid json");

    assert!(
        result.get("deferred").is_some(),
        "should have deferred field"
    );
    let deferred = result["deferred"].as_array().unwrap();
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
}

// =============================================================================
// Natural Time Parsing Tests (6 tests)
// =============================================================================

#[test]
fn defer_until_tomorrow() {
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(
        &workspace,
        ["defer", &id, "--until", "tomorrow", "--json"],
        "defer_tomorrow",
    );
    assert!(defer.status.success(), "defer failed: {}", defer.stderr);

    let payload = extract_json_payload(&defer.stdout);
    let result: Value = serde_json::from_str(&payload).expect("valid json");

    assert_eq!(result["deferred"].as_array().unwrap().len(), 1);
    let defer_until = result["deferred"][0]["defer_until"].as_str().unwrap();
    assert!(
        !defer_until.is_empty(),
        "defer_until should be set for tomorrow"
    );
}

#[test]
fn defer_until_relative() {
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(
        &workspace,
        ["defer", &id, "--until", "+2h", "--json"],
        "defer_relative",
    );
    assert!(defer.status.success(), "defer failed: {}", defer.stderr);

    let payload = extract_json_payload(&defer.stdout);
    let result: Value = serde_json::from_str(&payload).expect("valid json");

    assert_eq!(result["deferred"].as_array().unwrap().len(), 1);
    let defer_until = result["deferred"][0]["defer_until"].as_str().unwrap();
    assert!(!defer_until.is_empty(), "defer_until should be set for +2h");
}

#[test]
fn defer_until_specific_date() {
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(
        &workspace,
        ["defer", &id, "--until", "2099-12-31", "--json"],
        "defer_specific_date",
    );
    assert!(defer.status.success(), "defer failed: {}", defer.stderr);

    let payload = extract_json_payload(&defer.stdout);
    let result: Value = serde_json::from_str(&payload).expect("valid json");

    assert_eq!(result["deferred"].as_array().unwrap().len(), 1);
    let defer_until = result["deferred"][0]["defer_until"].as_str().unwrap();
    assert!(
        defer_until.contains("2099-12-31"),
        "defer_until should contain the specified date"
    );
}

#[test]
fn defer_until_datetime() {
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(
        &workspace,
        ["defer", &id, "--until", "2099-02-01T09:00:00Z", "--json"],
        "defer_datetime",
    );
    assert!(defer.status.success(), "defer failed: {}", defer.stderr);

    let payload = extract_json_payload(&defer.stdout);
    let result: Value = serde_json::from_str(&payload).expect("valid json");

    assert_eq!(result["deferred"].as_array().unwrap().len(), 1);
    let defer_until = result["deferred"][0]["defer_until"].as_str().unwrap();
    assert!(
        defer_until.contains("2099-02-01"),
        "defer_until should contain the specified date"
    );
}

#[test]
fn defer_until_past_allows() {
    let (workspace, id) = setup_workspace_with_issue();

    // Past dates should be allowed (might warn but not fail)
    let defer = run_br(
        &workspace,
        ["defer", &id, "--until", "-1d", "--json"],
        "defer_past",
    );
    assert!(
        defer.status.success(),
        "defer with past date should succeed: {}",
        defer.stderr
    );

    let payload = extract_json_payload(&defer.stdout);
    let result: Value = serde_json::from_str(&payload).expect("valid json");
    assert_eq!(result["deferred"].as_array().unwrap().len(), 1);
}

#[test]
fn defer_until_invalid_error() {
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
}

// =============================================================================
// Undefer Tests (4 tests)
// =============================================================================

#[test]
fn undefer_sets_status_open() {
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
    let issue: Value = serde_json::from_str(&payload).expect("valid json");

    assert_eq!(
        issue["status"].as_str().unwrap(),
        "open",
        "status should be open after undefer"
    );
}

#[test]
fn undefer_clears_defer_until() {
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(&workspace, ["defer", &id, "--until", "+1d"], "defer_first");
    assert!(defer.status.success());

    let undefer = run_br(&workspace, ["undefer", &id, "--json"], "undefer");
    assert!(undefer.status.success());

    let show = run_br(&workspace, ["show", &id, "--json"], "show");
    let payload = extract_json_payload(&show.stdout);
    let issue: Value = serde_json::from_str(&payload).expect("valid json");

    assert!(
        issue.get("defer_until").is_none() || issue["defer_until"].is_null(),
        "defer_until should be cleared after undefer"
    );
}

#[test]
fn undefer_multiple_issues() {
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
        result["undeferred"].as_array().unwrap().len(),
        3,
        "all 3 issues should be undeferred"
    );

    for id in &ids {
        let show = run_br(&workspace, ["show", id, "--json"], &format!("show_{id}"));
        let show_payload = extract_json_payload(&show.stdout);
        let issue: Value = serde_json::from_str(&show_payload).expect("valid json");
        assert_eq!(issue["status"].as_str().unwrap(), "open");
    }
}

#[test]
fn undefer_json_output() {
    let (workspace, id) = setup_workspace_with_issue();

    let defer = run_br(&workspace, ["defer", &id], "defer_first");
    assert!(defer.status.success());

    let undefer = run_br(&workspace, ["undefer", &id, "--json"], "undefer");
    assert!(undefer.status.success());

    let payload = extract_json_payload(&undefer.stdout);
    let result: Value = serde_json::from_str(&payload).expect("valid json");

    assert!(result.get("undeferred").is_some());
    let undeferred = result["undeferred"].as_array().unwrap();
    assert_eq!(undeferred.len(), 1);

    let first = &undeferred[0];
    assert!(first.get("id").is_some());
    assert!(first.get("title").is_some());
    assert!(first.get("status").is_some());
    assert_eq!(first["status"].as_str().unwrap(), "open");
}

// =============================================================================
// Edge Cases (4 tests)
// =============================================================================

#[test]
fn defer_already_deferred_updates_time() {
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

    // Should have deferred with new time, or skipped as already deferred
    let deferred_count = result["deferred"].as_array().map_or(0, Vec::len);
    let skipped_count = result
        .get("skipped")
        .and_then(|s| s.as_array())
        .map_or(0, Vec::len);

    assert!(
        deferred_count > 0 || skipped_count > 0,
        "should either update defer time or skip as already deferred"
    );
}

#[test]
fn undefer_already_open_skips() {
    let (workspace, id) = setup_workspace_with_issue();

    // Issue starts as open - undefer should skip it
    let undefer = run_br(&workspace, ["undefer", &id, "--json"], "undefer_open");
    assert!(undefer.status.success());

    let payload = extract_json_payload(&undefer.stdout);
    let result: Value = serde_json::from_str(&payload).expect("valid json");

    let skipped = result.get("skipped").and_then(|s| s.as_array());
    assert!(
        skipped.is_some() && !skipped.unwrap().is_empty(),
        "should skip already-open issue"
    );
}

#[test]
fn defer_closed_issue_error() {
    let (workspace, id) = setup_workspace_with_issue();

    let close = run_br(&workspace, ["close", &id], "close_first");
    assert!(close.status.success());

    let defer = run_br(&workspace, ["defer", &id, "--json"], "defer_closed");
    assert!(defer.status.success()); // Command succeeds but issue is skipped

    let payload = extract_json_payload(&defer.stdout);
    let result: Value = serde_json::from_str(&payload).expect("valid json");

    let skipped = result.get("skipped").and_then(|s| s.as_array());
    assert!(
        skipped.is_some() && !skipped.unwrap().is_empty(),
        "should skip closed issue"
    );
    let reason = skipped.unwrap()[0]["reason"].as_str().unwrap();
    assert!(
        reason.to_lowercase().contains("closed"),
        "skip reason should mention closed status"
    );
}

#[test]
fn defer_nonexistent_error() {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let defer = run_br(
        &workspace,
        ["defer", "bd-nonexistent", "--json"],
        "defer_nonexistent",
    );

    // May fail or skip - depends on implementation
    let payload = extract_json_payload(&defer.stdout);
    let result: Value = serde_json::from_str(&payload).unwrap_or_default();

    if defer.status.success() {
        let skipped = result.get("skipped").and_then(|s| s.as_array());
        assert!(
            skipped.is_some() && !skipped.unwrap().is_empty(),
            "should skip nonexistent issue"
        );
    } else {
        assert!(
            defer.stderr.to_lowercase().contains("not found")
                || defer.stderr.to_lowercase().contains("no matching")
                || defer.stderr.to_lowercase().contains("unknown"),
            "error should indicate issue not found"
        );
    }
}

// =============================================================================
// Ready/Blocked Interaction Tests (3 tests)
// =============================================================================

#[test]
fn deferred_not_in_ready() {
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
}

#[test]
fn deferred_not_blocked() {
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
}

#[test]
fn undefer_appears_in_ready() {
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
}
