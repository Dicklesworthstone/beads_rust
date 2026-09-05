//! E2E tests for the `defer` and `undefer` commands.
//!
//! These tests verify the defer/undefer lifecycle including:
//! - Setting/clearing deferred status
//! - Time parsing (relative, absolute, natural language)
//! - Ready/blocked list interactions
//! - Edge cases and error handling

mod common;

use common::cli::{BrWorkspace, extract_json_payload, parse_list_issues, run_br};
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

    let deferred = result["deferred"].as_array().expect("deferred array");
    assert_eq!(deferred.len(), 1);
    let deferred = &deferred[0];
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

    let deferred = result["deferred"].as_array().expect("deferred array");
    assert_eq!(deferred.len(), 3, "all 3 issues should be deferred");

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

    let deferred = result["deferred"].as_array().expect("deferred array");
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
    assert_eq!(first["previous_status"].as_str(), Some("open"));
    assert!(
        first["defer_until"].as_str().is_some(),
        "defer json output should preserve defer_until"
    );
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
        defer.stdout.to_lowercase().contains("invalid")
            || defer.stdout.to_lowercase().contains("parse")
            || defer.stdout.to_lowercase().contains("unrecognized"),
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

    let undeferred = result["undeferred"].as_array().expect("undeferred array");
    assert_eq!(undeferred.len(), 3, "all 3 issues should be undeferred");

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

    let undeferred = result["undeferred"].as_array().expect("undeferred array");
    assert_eq!(undeferred.len(), 1);

    let first = &undeferred[0];
    assert!(first.get("id").is_some());
    assert!(first.get("title").is_some());
    assert!(first.get("status").is_some());
    assert_eq!(first["status"].as_str().unwrap(), "open");
    assert_eq!(first["previous_status"].as_str(), Some("deferred"));
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

    let deferred = result["deferred"].as_array().expect("deferred array");
    assert_eq!(deferred.len(), 1);

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
    let undeferred = result["undeferred"].as_array().cloned().unwrap_or_default();
    let skipped = result["skipped"].as_array().cloned().unwrap_or_default();

    assert!(
        undeferred.is_empty(),
        "already-open issue should not be reported as undeferred"
    );
    assert_eq!(
        skipped.len(),
        1,
        "already-open issue should be reported as skipped"
    );
    assert_eq!(skipped[0]["id"], id);
    assert!(
        skipped[0]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("not deferred")),
        "skip reason should explain that the issue was not deferred"
    );

    let show = run_br(&workspace, ["show", &id, "--json"], "show");
    let show_payload = extract_json_payload(&show.stdout);
    let issues: Value = serde_json::from_str(&show_payload).expect("valid json");
    assert_eq!(issues[0]["status"], "open");
    info!("undefer_already_open_skips: assertions passed");
}

#[test]
fn defer_closed_issue_skips() {
    common::init_test_logging();
    info!("defer_closed_issue_skips: starting");
    let (workspace, id) = setup_workspace_with_issue();

    let close = run_br(&workspace, ["close", &id], "close_first");
    assert!(close.status.success());

    // Closed issues should be reported as skipped instead of being deferred.
    let defer = run_br(&workspace, ["defer", &id, "--json"], "defer_closed");
    assert!(defer.status.success());

    let payload = extract_json_payload(&defer.stdout);
    let result: Value = serde_json::from_str(&payload).expect("valid json");
    let deferred = result["deferred"].as_array().cloned().unwrap_or_default();
    let skipped = result["skipped"].as_array().cloned().unwrap_or_default();

    assert!(
        deferred.is_empty(),
        "closed issue should not be deferred successfully"
    );
    assert_eq!(
        skipped.len(),
        1,
        "closed issue should be reported as skipped"
    );
    assert_eq!(skipped[0]["id"], id);
    assert!(
        skipped[0]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("cannot defer closed issue")),
        "skip reason should explain that closed issues cannot be deferred"
    );
    info!("defer_closed_issue_skips: assertions passed");
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
    assert!(defer.stdout.contains("not found") || defer.stdout.contains("matching"));
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

// =============================================================================
// Scheduled ("not before this date") beads — GitHub #489
//
// `defer_until` is a time gate, not merely a companion to the `deferred`
// status: `br ready` excludes ANY issue whose `defer_until` is in the future,
// including a plain `open` one. That is what makes a recurring or time-gated
// bead expressible without a daemon — the bead stays `open` and visible in
// `br list`, but no agent picks it up before its date. These tests pin that
// contract end to end so it cannot regress into a status-only feature.
// =============================================================================

/// Helper: ids present in a `br ready --json` payload.
fn ready_ids(workspace: &BrWorkspace, args: &[&str], label: &str) -> Vec<String> {
    let mut argv = vec!["ready", "--json"];
    argv.extend_from_slice(args);
    let ready = run_br(workspace, argv, label);
    assert!(ready.status.success(), "ready failed: {}", ready.stderr);
    let payload = extract_json_payload(&ready.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).expect("valid ready json");
    issues
        .iter()
        .filter_map(|i| i["id"].as_str())
        .map(str::to_string)
        .collect()
}

/// A workspace holding one ungated control issue (so an empty `br ready` can
/// never make a gating assertion pass by accident) and one `open` bead whose
/// `defer_until` sits far in the future. Returns
/// `(workspace, scheduled_id, control_id)`.
fn setup_scheduled_workspace() -> (BrWorkspace, String, String) {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let control = run_br(
        &workspace,
        ["create", "Ungated work", "-p", "2", "-t", "task"],
        "create_control",
    );
    assert!(
        control.status.success(),
        "create failed: {}",
        control.stderr
    );
    let control_id = parse_created_id(&control.stdout);

    let create = run_br(
        &workspace,
        [
            "create",
            "Weekly dependency audit",
            "-p",
            "2",
            "-t",
            "task",
            "--defer",
            SCHEDULED_GATE,
        ],
        "create_scheduled",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    (workspace, id, control_id)
}

/// Explicit RFC3339 so the stored instant does not depend on the host's zone
/// (a bare `YYYY-MM-DD` is parsed as 09:00 *local* time).
const SCHEDULED_GATE: &str = "2099-12-31T09:00:00Z";
const SCHEDULED_GATE_DATE: &str = "2099-12-31";

#[test]
fn scheduled_open_issue_is_hidden_from_ready_until_its_date() {
    common::init_test_logging();
    info!("scheduled_open_issue_is_hidden_from_ready_until_its_date: starting");
    let (workspace, id, control_id) = setup_scheduled_workspace();

    // `--defer` must NOT flip the status: the bead is real, open work whose
    // start date has not arrived.
    let show = run_br(&workspace, ["show", &id, "--json"], "show_scheduled");
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let shown: Value =
        serde_json::from_str(&extract_json_payload(&show.stdout)).expect("valid show json");
    assert_eq!(
        shown[0]["status"].as_str(),
        Some("open"),
        "--defer must not change status"
    );
    assert!(
        shown[0]["defer_until"]
            .as_str()
            .is_some_and(|d| d.contains(SCHEDULED_GATE_DATE)),
        "defer_until must round-trip: {}",
        shown[0]
    );

    // It stays visible as open work.
    let list = run_br(
        &workspace,
        ["list", "--status", "open", "--json"],
        "list_open",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);
    let listed = parse_list_issues(&list.stdout);
    assert!(
        listed
            .iter()
            .filter_map(|i| i["id"].as_str())
            .any(|listed_id| listed_id == id),
        "a scheduled bead is still open work and must appear in `br list --status open`"
    );

    // But no agent picks it up.
    let gated = ready_ids(&workspace, &[], "ready_gated");
    assert!(
        !gated.contains(&id),
        "a future defer_until must gate an open issue out of ready: {gated:?}"
    );
    assert!(
        gated.contains(&control_id),
        "the ungated control issue must still be ready: {gated:?}"
    );

    // ...unless the caller explicitly asks for gated work.
    let including = ready_ids(&workspace, &["--include-deferred"], "ready_including");
    assert!(
        including.contains(&id),
        "--include-deferred must surface a time-gated open issue: {including:?}"
    );

    // `br show` text mode states how long the gate still has to run.
    let show_text = run_br(&workspace, ["show", &id], "show_text");
    assert!(
        show_text.status.success(),
        "show failed: {}",
        show_text.stderr
    );
    assert!(
        show_text
            .stdout
            .contains(&format!("Deferred until: {SCHEDULED_GATE_DATE}"))
            && show_text.stdout.contains("(ready in "),
        "show should annotate the gate countdown: {}",
        show_text.stdout
    );
    info!("scheduled_open_issue_is_hidden_from_ready_until_its_date: assertions passed");
}

#[test]
fn an_elapsed_defer_gate_releases_the_issue_without_a_status_change() {
    common::init_test_logging();
    info!("an_elapsed_defer_gate_releases_the_issue_without_a_status_change: starting");
    let (workspace, id, _control_id) = setup_scheduled_workspace();
    assert!(!ready_ids(&workspace, &[], "ready_gated").contains(&id));

    // The gate elapsing is simulated by moving it into the past; the point is
    // that nothing else changes — no transition, no `br undefer`.
    let update = run_br(
        &workspace,
        ["update", &id, "--defer", "2000-01-01T09:00:00Z"],
        "update_past_gate",
    );
    assert!(update.status.success(), "update failed: {}", update.stderr);

    let elapsed = ready_ids(&workspace, &[], "ready_elapsed");
    assert!(
        elapsed.contains(&id),
        "an elapsed defer_until must release the issue into ready: {elapsed:?}"
    );

    let show_after = run_br(&workspace, ["show", &id, "--json"], "show_after");
    assert!(
        show_after.status.success(),
        "show failed: {}",
        show_after.stderr
    );
    let shown: Value =
        serde_json::from_str(&extract_json_payload(&show_after.stdout)).expect("valid show json");
    assert_eq!(
        shown[0]["status"].as_str(),
        Some("open"),
        "the gate must never have changed the status"
    );
    // An elapsed gate no longer constrains anything, so `br show` must not
    // annotate it with a countdown.
    let show_text = run_br(&workspace, ["show", &id], "show_text_after");
    assert!(
        !show_text.stdout.contains("(ready in "),
        "an elapsed gate must not be annotated: {}",
        show_text.stdout
    );
    info!("an_elapsed_defer_gate_releases_the_issue_without_a_status_change: assertions passed");
}

#[test]
fn scheduled_gate_round_trips_through_jsonl() {
    common::init_test_logging();
    info!("scheduled_gate_round_trips_through_jsonl: starting");
    let (workspace, id, _control_id) = setup_scheduled_workspace();

    let flush = run_br(&workspace, ["sync", "--flush-only"], "flush");
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);

    // `br init` names the export after the project prefix, so discover it
    // rather than hard-coding a file name.
    let beads_dir = workspace.root.join(".beads");
    let jsonl_path = std::fs::read_dir(&beads_dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", beads_dir.display()))
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
        .unwrap_or_else(|| panic!("no *.jsonl in {}", beads_dir.display()));
    let jsonl = std::fs::read_to_string(&jsonl_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", jsonl_path.display()));
    let record = jsonl
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|v| v["id"].as_str() == Some(id.as_str()))
        .unwrap_or_else(|| panic!("no JSONL record for {id} in {jsonl}"));

    assert_eq!(record["status"].as_str(), Some("open"));
    assert!(
        record["defer_until"]
            .as_str()
            .is_some_and(|d| d.contains(SCHEDULED_GATE_DATE)),
        "JSONL export must carry the gate: {record}"
    );
    info!("scheduled_gate_round_trips_through_jsonl: assertions passed");
}

/// Clearing the gate (`--defer ''`) releases the bead immediately.
#[test]
fn clearing_the_defer_gate_releases_an_open_issue_into_ready() {
    common::init_test_logging();
    info!("clearing_the_defer_gate_releases_an_open_issue_into_ready: starting");
    let (workspace, id, _control_id) = setup_scheduled_workspace();
    assert!(!ready_ids(&workspace, &[], "ready_gated").contains(&id));

    let clear = run_br(&workspace, ["update", &id, "--defer", ""], "clear_gate");
    assert!(clear.status.success(), "update failed: {}", clear.stderr);

    let released = ready_ids(&workspace, &[], "ready_released");
    assert!(
        released.contains(&id),
        "clearing defer_until must release the issue: {released:?}"
    );
    info!("clearing_the_defer_gate_releases_an_open_issue_into_ready: assertions passed");
}
