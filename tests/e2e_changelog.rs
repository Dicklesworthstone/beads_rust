//! E2E tests for the changelog command.
//!
//! Tests cover:
//! - Success paths: generate changelog with closed issues
//! - Date filtering with --since
//! - Grouping by issue type
//! - Error cases: before init, no closed issues
//! - Edge cases: many issues, same-second closes, reopen-then-close

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;
use std::thread::sleep;
use std::time::Duration;
use tracing::info;

/// Parse issue ID from create output.
fn parse_created_id(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or("");
    // Handle both "✓ Created ID: ..." and "Created ID: ..." formats
    let id_part = line
        .strip_prefix("✓ Created ")
        .or_else(|| line.strip_prefix("Created "))
        .and_then(|rest| rest.split(':').next())
        .unwrap_or("");
    id_part.trim().to_string()
}

// ============================================================
// SUCCESS PATH TESTS
// ============================================================

#[test]
fn changelog_with_closed_issues() {
    common::init_test_logging();
    info!("Starting changelog_with_closed_issues test");

    let workspace = BrWorkspace::new();
    run_br(&workspace, ["init"], "init");

    // Create and close some issues
    let create1 = run_br(
        &workspace,
        ["create", "Fix login bug", "--type", "bug"],
        "create1",
    );
    let id1 = parse_created_id(&create1.stdout);
    run_br(&workspace, ["close", &id1], "close1");

    let create2 = run_br(
        &workspace,
        ["create", "Add dark mode", "--type", "feature"],
        "create2",
    );
    let id2 = parse_created_id(&create2.stdout);
    run_br(&workspace, ["close", &id2], "close2");

    // Generate changelog
    let changelog = run_br(&workspace, ["changelog", "--json"], "changelog");
    assert!(
        changelog.status.success(),
        "changelog failed: {}",
        changelog.stderr
    );

    let payload = extract_json_payload(&changelog.stdout);
    eprintln!("DEBUG stdout: {}", changelog.stdout);
    eprintln!("DEBUG stderr: {}", changelog.stderr);
    eprintln!("DEBUG payload: {payload}");
    let json: Value = serde_json::from_str(&payload).expect("parse json");
    eprintln!("DEBUG json: {json:?}");

    assert_eq!(json["total_closed"], 2, "should have 2 closed issues");
    assert!(json["since"].is_string(), "should have since field");
    assert!(json["until"].is_string(), "should have until field");
    assert!(json["groups"].is_array(), "should have groups array");

    info!("changelog_with_closed_issues passed");
}

#[test]
fn changelog_groups_by_type() {
    common::init_test_logging();
    info!("Starting changelog_groups_by_type test");

    let workspace = BrWorkspace::new();
    run_br(&workspace, ["init"], "init");

    // Create issues of different types
    let create1 = run_br(&workspace, ["create", "Bug fix 1", "--type", "bug"], "c1");
    let id1 = parse_created_id(&create1.stdout);
    run_br(&workspace, ["close", &id1], "close1");

    let create2 = run_br(&workspace, ["create", "Bug fix 2", "--type", "bug"], "c2");
    let id2 = parse_created_id(&create2.stdout);
    run_br(&workspace, ["close", &id2], "close2");

    let create3 = run_br(
        &workspace,
        ["create", "New feature", "--type", "feature"],
        "c3",
    );
    let id3 = parse_created_id(&create3.stdout);
    run_br(&workspace, ["close", &id3], "close3");

    let changelog = run_br(&workspace, ["changelog", "--json"], "changelog");
    assert!(changelog.status.success());

    let payload = extract_json_payload(&changelog.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse json");

    let groups = json["groups"].as_array().expect("groups array");

    // Should have bug and feature groups
    let bug_group = groups.iter().find(|g| g["issue_type"] == "bug");
    let feature_group = groups.iter().find(|g| g["issue_type"] == "feature");

    assert!(bug_group.is_some(), "should have bug group");
    assert!(feature_group.is_some(), "should have feature group");

    let bug_issues = bug_group.unwrap()["issues"].as_array().expect("issues");
    assert_eq!(bug_issues.len(), 2, "bug group should have 2 issues");

    let feature_issues = feature_group.unwrap()["issues"].as_array().expect("issues");
    assert_eq!(feature_issues.len(), 1, "feature group should have 1 issue");

    info!("changelog_groups_by_type passed");
}

#[test]
fn changelog_sorts_by_priority() {
    common::init_test_logging();
    info!("Starting changelog_sorts_by_priority test");

    let workspace = BrWorkspace::new();
    run_br(&workspace, ["init"], "init");

    // Create bugs with different priorities
    let create1 = run_br(
        &workspace,
        [
            "create",
            "Low priority bug",
            "--type",
            "bug",
            "--priority",
            "3",
        ],
        "c1",
    );
    let id1 = parse_created_id(&create1.stdout);
    run_br(&workspace, ["close", &id1], "close1");

    let create2 = run_br(
        &workspace,
        [
            "create",
            "High priority bug",
            "--type",
            "bug",
            "--priority",
            "1",
        ],
        "c2",
    );
    let id2 = parse_created_id(&create2.stdout);
    run_br(&workspace, ["close", &id2], "close2");

    let create3 = run_br(
        &workspace,
        ["create", "Critical bug", "--type", "bug", "--priority", "0"],
        "c3",
    );
    let id3 = parse_created_id(&create3.stdout);
    run_br(&workspace, ["close", &id3], "close3");

    let changelog = run_br(&workspace, ["changelog", "--json"], "changelog");
    assert!(changelog.status.success());

    let payload = extract_json_payload(&changelog.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse json");

    let groups = json["groups"].as_array().expect("groups");
    let bug_group = groups.iter().find(|g| g["issue_type"] == "bug").unwrap();
    let issues = bug_group["issues"].as_array().expect("issues");

    // Should be sorted by priority (0, 1, 3)
    assert_eq!(issues[0]["priority"], "P0", "first should be P0");
    assert_eq!(issues[1]["priority"], "P1", "second should be P1");
    assert_eq!(issues[2]["priority"], "P3", "third should be P3");

    info!("changelog_sorts_by_priority passed");
}

#[test]
fn changelog_text_output() {
    common::init_test_logging();
    info!("Starting changelog_text_output test");

    let workspace = BrWorkspace::new();
    run_br(&workspace, ["init"], "init");

    let create = run_br(&workspace, ["create", "Test issue"], "create");
    let id = parse_created_id(&create.stdout);
    run_br(&workspace, ["close", &id], "close");

    let changelog = run_br(&workspace, ["changelog"], "changelog");
    assert!(
        changelog.status.success(),
        "changelog failed: {}",
        changelog.stderr
    );

    // Text output should contain header and issue
    assert!(
        changelog.stdout.contains("Changelog since"),
        "should have header"
    );
    assert!(
        changelog.stdout.contains("closed issue"),
        "should mention closed issues"
    );

    info!("changelog_text_output passed");
}

#[test]
fn changelog_robot_mode() {
    common::init_test_logging();
    info!("Starting changelog_robot_mode test");

    let workspace = BrWorkspace::new();
    run_br(&workspace, ["init"], "init");

    let create = run_br(&workspace, ["create", "Test issue"], "create");
    let id = parse_created_id(&create.stdout);
    run_br(&workspace, ["close", &id], "close");

    // --robot should produce JSON output
    let changelog = run_br(&workspace, ["changelog", "--robot"], "changelog");
    assert!(changelog.status.success());

    let payload = extract_json_payload(&changelog.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse json");
    assert!(json["groups"].is_array());

    info!("changelog_robot_mode passed");
}

// ============================================================
// DATE FILTERING TESTS
// ============================================================

#[test]
fn changelog_since_date() {
    common::init_test_logging();
    info!("Starting changelog_since_date test");

    let workspace = BrWorkspace::new();
    run_br(&workspace, ["init"], "init");

    // Create and close an issue
    let create = run_br(&workspace, ["create", "Recent issue"], "create");
    let id = parse_created_id(&create.stdout);
    run_br(&workspace, ["close", &id], "close");

    // Use --since with yesterday to include the issue
    // Note: Use --since=-1d format to avoid clap treating -1d as a flag
    let changelog = run_br(
        &workspace,
        ["changelog", "--since=-1d", "--json"],
        "changelog",
    );
    assert!(
        changelog.status.success(),
        "changelog failed: {}",
        changelog.stderr
    );

    let payload = extract_json_payload(&changelog.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse json");
    assert_eq!(json["total_closed"], 1, "should find 1 issue");

    info!("changelog_since_date passed");
}

#[test]
fn changelog_since_future_date() {
    common::init_test_logging();
    info!("Starting changelog_since_future_date test");

    let workspace = BrWorkspace::new();
    run_br(&workspace, ["init"], "init");

    let create = run_br(&workspace, ["create", "Old issue"], "create");
    let id = parse_created_id(&create.stdout);
    run_br(&workspace, ["close", &id], "close");

    // Use --since with a future date - should find no issues
    let changelog = run_br(
        &workspace,
        ["changelog", "--since", "2099-01-01T00:00:00Z", "--json"],
        "changelog",
    );
    assert!(changelog.status.success());

    let payload = extract_json_payload(&changelog.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse json");
    assert_eq!(json["total_closed"], 0, "should find no issues in future");

    info!("changelog_since_future_date passed");
}

#[test]
fn changelog_all_time() {
    common::init_test_logging();
    info!("Starting changelog_all_time test");

    let workspace = BrWorkspace::new();
    run_br(&workspace, ["init"], "init");

    let create = run_br(&workspace, ["create", "Test issue"], "create");
    let id = parse_created_id(&create.stdout);
    run_br(&workspace, ["close", &id], "close");

    // No --since should return all closed issues
    let changelog = run_br(&workspace, ["changelog", "--json"], "changelog");
    assert!(changelog.status.success());

    let payload = extract_json_payload(&changelog.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse json");
    assert_eq!(json["since"], "all", "since should be 'all'");
    assert_eq!(json["total_closed"], 1);

    info!("changelog_all_time passed");
}

// ============================================================
// ERROR CASE TESTS
// ============================================================

#[test]
fn changelog_before_init_fails() {
    common::init_test_logging();
    info!("Starting changelog_before_init_fails test");

    let workspace = BrWorkspace::new();
    // Don't run init

    let changelog = run_br(&workspace, ["changelog"], "changelog");
    assert!(
        !changelog.status.success(),
        "changelog should fail before init"
    );
    assert!(
        changelog.stderr.contains("not initialized")
            || changelog.stderr.contains("NotInitialized")
            || changelog.stderr.contains("No beads"),
        "error should mention not initialized: {}",
        changelog.stderr
    );

    info!("changelog_before_init_fails passed");
}

#[test]
fn changelog_no_closed_issues() {
    common::init_test_logging();
    info!("Starting changelog_no_closed_issues test");

    let workspace = BrWorkspace::new();
    run_br(&workspace, ["init"], "init");

    // Create issue but don't close it
    run_br(&workspace, ["create", "Open issue"], "create");

    let changelog = run_br(&workspace, ["changelog", "--json"], "changelog");
    assert!(
        changelog.status.success(),
        "changelog should succeed with no closed issues"
    );

    let payload = extract_json_payload(&changelog.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse json");
    assert_eq!(json["total_closed"], 0, "should have 0 closed issues");
    let groups = json["groups"].as_array().expect("groups");
    assert!(groups.is_empty(), "groups should be empty");

    info!("changelog_no_closed_issues passed");
}

// ============================================================
// EDGE CASE TESTS
// ============================================================

#[test]
fn changelog_many_closed_issues() {
    common::init_test_logging();
    info!("Starting changelog_many_closed_issues test");

    let workspace = BrWorkspace::new();
    run_br(&workspace, ["init"], "init");

    // Create and close 20 issues
    for i in 0..20 {
        let title = format!("Issue number {i}");
        let create = run_br(&workspace, ["create", &title], &format!("create_{i}"));
        let id = parse_created_id(&create.stdout);
        run_br(&workspace, ["close", &id], &format!("close_{i}"));
    }

    let changelog = run_br(&workspace, ["changelog", "--json"], "changelog");
    assert!(
        changelog.status.success(),
        "changelog failed: {}",
        changelog.stderr
    );

    let payload = extract_json_payload(&changelog.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse json");
    assert_eq!(json["total_closed"], 20, "should have 20 closed issues");

    info!("changelog_many_closed_issues passed");
}

#[test]
fn changelog_with_close_reasons() {
    common::init_test_logging();
    info!("Starting changelog_with_close_reasons test");

    let workspace = BrWorkspace::new();
    run_br(&workspace, ["init"], "init");

    let create = run_br(&workspace, ["create", "Bug with reason"], "create");
    let id = parse_created_id(&create.stdout);
    run_br(
        &workspace,
        ["close", &id, "--reason", "Fixed in v1.2.3"],
        "close",
    );

    let changelog = run_br(&workspace, ["changelog", "--json"], "changelog");
    assert!(changelog.status.success());

    let payload = extract_json_payload(&changelog.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse json");
    assert_eq!(json["total_closed"], 1);

    // Verify the issue appears in the changelog
    let groups = json["groups"].as_array().expect("groups");
    assert!(!groups.is_empty(), "should have at least one group");

    info!("changelog_with_close_reasons passed");
}

#[test]
fn changelog_reopen_then_close() {
    common::init_test_logging();
    info!("Starting changelog_reopen_then_close test");

    let workspace = BrWorkspace::new();
    run_br(&workspace, ["init"], "init");

    // Create, close, reopen, close again
    let create = run_br(&workspace, ["create", "Reopened issue"], "create");
    let id = parse_created_id(&create.stdout);

    run_br(&workspace, ["close", &id], "close1");

    // Small delay to ensure different timestamp
    sleep(Duration::from_millis(100));

    run_br(&workspace, ["reopen", &id], "reopen");
    run_br(&workspace, ["close", &id], "close2");

    let changelog = run_br(&workspace, ["changelog", "--json"], "changelog");
    assert!(
        changelog.status.success(),
        "changelog failed: {}",
        changelog.stderr
    );

    let payload = extract_json_payload(&changelog.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse json");

    // Issue should appear only once in changelog
    assert_eq!(json["total_closed"], 1, "reopened issue should appear once");

    info!("changelog_reopen_then_close passed");
}

#[test]
fn changelog_mixed_statuses() {
    common::init_test_logging();
    info!("Starting changelog_mixed_statuses test");

    let workspace = BrWorkspace::new();
    run_br(&workspace, ["init"], "init");

    // Create issues in different states
    let c1 = run_br(&workspace, ["create", "Closed issue"], "c1");
    let id1 = parse_created_id(&c1.stdout);
    run_br(&workspace, ["close", &id1], "close1");

    let c2 = run_br(&workspace, ["create", "Open issue"], "c2");
    let _id2 = parse_created_id(&c2.stdout);
    // Don't close this one

    let c3 = run_br(&workspace, ["create", "In progress issue"], "c3");
    let id3 = parse_created_id(&c3.stdout);
    run_br(
        &workspace,
        ["update", &id3, "--status", "in_progress"],
        "update3",
    );

    let changelog = run_br(&workspace, ["changelog", "--json"], "changelog");
    assert!(changelog.status.success());

    let payload = extract_json_payload(&changelog.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse json");

    // Only the closed issue should appear
    assert_eq!(json["total_closed"], 1, "only closed issues in changelog");

    info!("changelog_mixed_statuses passed");
}

#[test]
fn changelog_since_relative_time() {
    common::init_test_logging();
    info!("Starting changelog_since_relative_time test");

    let workspace = BrWorkspace::new();
    run_br(&workspace, ["init"], "init");

    let create = run_br(&workspace, ["create", "Recent fix"], "create");
    let id = parse_created_id(&create.stdout);
    run_br(&workspace, ["close", &id], "close");

    // Test various relative time formats
    // Note: Use --since=VALUE format to avoid clap treating -Xd as a flag
    let formats = ["-1h", "-24h", "-7d"];
    for format in formats {
        let since_arg = format!("--since={format}");
        let changelog = run_br(
            &workspace,
            ["changelog", &since_arg, "--json"],
            &format!("changelog_{format}"),
        );
        assert!(
            changelog.status.success(),
            "changelog with {} failed: {}",
            format,
            changelog.stderr
        );

        let payload = extract_json_payload(&changelog.stdout);
        let json: Value = serde_json::from_str(&payload).expect("parse json");
        assert!(
            json["since"].is_string(),
            "{format} should produce valid since"
        );
    }

    info!("changelog_since_relative_time passed");
}
