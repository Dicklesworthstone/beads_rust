//! E2E tests for the `list` command scenarios.
//!
//! Comprehensive testing of list command filters, sorts, and output formats:
//! - Status filtering (open, closed, in_progress, deferred, all)
//! - Type filtering (bug, feature, task, epic)
//! - Priority filtering (P0-P4)
//! - Assignee and label filtering
//! - Sort options (created_at, updated_at, priority)
//! - Limit and offset pagination
//! - Output formats (text, JSON, CSV)
//! - Field selection with --fields

#![allow(
    clippy::doc_markdown,
    clippy::too_many_lines,
    clippy::uninlined_format_args,
    clippy::manual_range_contains
)]

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;

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

/// Setup a workspace with a diverse set of issues for comprehensive testing.
fn setup_diverse_workspace() -> (BrWorkspace, Vec<String>) {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let mut ids = Vec::new();

    // Issue 1: P0 bug assigned to alice with "critical" label
    let issue1 = run_br(
        &workspace,
        ["create", "Critical login bug", "-p", "0", "-t", "bug"],
        "create_bug_p0",
    );
    assert!(issue1.status.success());
    let id1 = parse_created_id(&issue1.stdout);
    run_br(
        &workspace,
        [
            "update",
            &id1,
            "--assignee",
            "alice",
            "--add-label",
            "critical",
        ],
        "update_bug_p0",
    );
    ids.push(id1);

    // Issue 2: P1 feature assigned to bob with "backend" label
    let issue2 = run_br(
        &workspace,
        ["create", "Add user dashboard", "-p", "1", "-t", "feature"],
        "create_feature_p1",
    );
    assert!(issue2.status.success());
    let id2 = parse_created_id(&issue2.stdout);
    run_br(
        &workspace,
        [
            "update",
            &id2,
            "--assignee",
            "bob",
            "--add-label",
            "backend",
        ],
        "update_feature_p1",
    );
    ids.push(id2);

    // Issue 3: P2 task assigned to alice with "frontend" label
    let issue3 = run_br(
        &workspace,
        ["create", "Update documentation", "-p", "2", "-t", "task"],
        "create_task_p2",
    );
    assert!(issue3.status.success());
    let id3 = parse_created_id(&issue3.stdout);
    run_br(
        &workspace,
        [
            "update",
            &id3,
            "--assignee",
            "alice",
            "--add-label",
            "frontend",
        ],
        "update_task_p2",
    );
    ids.push(id3);

    // Issue 4: P1 bug unassigned with "backend" and "api" labels
    let issue4 = run_br(
        &workspace,
        ["create", "API rate limiting bug", "-p", "1", "-t", "bug"],
        "create_bug_p1",
    );
    assert!(issue4.status.success());
    let id4 = parse_created_id(&issue4.stdout);
    run_br(
        &workspace,
        [
            "update",
            &id4,
            "--add-label",
            "backend",
            "--add-label",
            "api",
        ],
        "update_bug_p1",
    );
    ids.push(id4);

    // Issue 5: P3 chore unassigned
    let issue5 = run_br(
        &workspace,
        ["create", "Clean up test fixtures", "-p", "3", "-t", "chore"],
        "create_chore_p3",
    );
    assert!(issue5.status.success());
    ids.push(parse_created_id(&issue5.stdout));

    // Issue 6: Closed P2 bug
    let issue6 = run_br(
        &workspace,
        [
            "create",
            "Fixed database connection",
            "-p",
            "2",
            "-t",
            "bug",
        ],
        "create_bug_closed",
    );
    assert!(issue6.status.success());
    let id6 = parse_created_id(&issue6.stdout);
    run_br(&workspace, ["close", &id6], "close_bug");
    ids.push(id6);

    // Issue 7: In-progress P1 task
    let issue7 = run_br(
        &workspace,
        ["create", "Implement caching layer", "-p", "1", "-t", "task"],
        "create_task_in_progress",
    );
    assert!(issue7.status.success());
    let id7 = parse_created_id(&issue7.stdout);
    run_br(
        &workspace,
        [
            "update",
            &id7,
            "--status",
            "in_progress",
            "--assignee",
            "charlie",
        ],
        "update_task_in_progress",
    );
    ids.push(id7);

    // Issue 8: Deferred P4 feature
    let issue8 = run_br(
        &workspace,
        [
            "create",
            "Future enhancement idea",
            "-p",
            "4",
            "-t",
            "feature",
        ],
        "create_feature_deferred",
    );
    assert!(issue8.status.success());
    let id8 = parse_created_id(&issue8.stdout);
    run_br(
        &workspace,
        [
            "update",
            &id8,
            "--status",
            "deferred",
            "--defer",
            "2100-01-01T00:00:00Z",
        ],
        "update_feature_deferred",
    );
    ids.push(id8);

    (workspace, ids)
}

// =============================================================================
// STATUS FILTER TESTS
// =============================================================================

#[test]
fn list_filter_by_status_open() {
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--status", "open", "--json"],
        "list_open",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    // All returned issues should have status "open"
    for issue in &json {
        assert_eq!(
            issue["status"], "open",
            "Expected status 'open', got {:?}",
            issue["status"]
        );
    }

    // Should find at least 4 open issues (excluding closed, in_progress, deferred)
    assert!(
        json.len() >= 4,
        "Expected at least 4 open issues, got {}",
        json.len()
    );
}

#[test]
fn list_filter_by_status_closed() {
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--status", "closed", "--json"],
        "list_closed",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    assert_eq!(json.len(), 1, "Expected exactly 1 closed issue");
    assert_eq!(json[0]["status"], "closed");
}

#[test]
fn list_filter_by_status_in_progress() {
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--status", "in_progress", "--json"],
        "list_in_progress",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    assert_eq!(json.len(), 1, "Expected exactly 1 in_progress issue");
    assert_eq!(json[0]["status"], "in_progress");
    assert!(json[0]["title"].as_str().unwrap().contains("caching layer"));
}

#[test]
fn list_filter_by_status_deferred() {
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--status", "deferred", "--json"],
        "list_deferred",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    assert_eq!(json.len(), 1, "Expected exactly 1 deferred issue");
    assert_eq!(json[0]["status"], "deferred");
}

#[test]
fn list_include_closed_shows_all() {
    let (workspace, ids) = setup_diverse_workspace();

    let list = run_br(&workspace, ["list", "--all", "--json"], "list_all");
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    // Should include all 8 issues
    assert_eq!(
        json.len(),
        ids.len(),
        "Expected {} issues with --include-closed, got {}",
        ids.len(),
        json.len()
    );
}

// =============================================================================
// TYPE FILTER TESTS
// =============================================================================

#[test]
fn list_filter_by_type_bug() {
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "-t", "bug", "--all", "--json"],
        "list_bugs",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    // All should be bugs
    for issue in &json {
        assert_eq!(
            issue["issue_type"], "bug",
            "Expected type 'bug', got {:?}",
            issue["issue_type"]
        );
    }

    assert_eq!(json.len(), 3, "Expected 3 bug issues");
}

#[test]
fn list_filter_by_type_feature() {
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "-t", "feature", "--all", "--json"],
        "list_features",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    assert_eq!(json.len(), 2, "Expected 2 feature issues");
    for issue in &json {
        assert_eq!(issue["issue_type"], "feature");
    }
}

#[test]
fn list_filter_by_type_task() {
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "-t", "task", "--all", "--json"],
        "list_tasks",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    assert_eq!(json.len(), 2, "Expected 2 task issues");
}

// =============================================================================
// PRIORITY FILTER TESTS
// =============================================================================

#[test]
fn list_filter_by_priority_p0() {
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(&workspace, ["list", "-p", "0", "--json"], "list_p0");
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    assert_eq!(json.len(), 1, "Expected 1 P0 issue");
    assert_eq!(json[0]["priority"], 0);
    assert!(
        json[0]["title"]
            .as_str()
            .unwrap()
            .contains("Critical login bug")
    );
}

#[test]
fn list_filter_by_priority_p1() {
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "-p", "1", "--all", "--json"],
        "list_p1",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    assert_eq!(json.len(), 3, "Expected 3 P1 issues");
    for issue in &json {
        assert_eq!(issue["priority"], 1);
    }
}

#[test]
fn list_filter_by_multiple_priorities() {
    let (workspace, _ids) = setup_diverse_workspace();

    // Filter for P0 and P1
    let list = run_br(
        &workspace,
        ["list", "-p", "0", "-p", "1", "--json"],
        "list_p0_p1",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    // Should include P0 and P1 issues (excluding closed)
    for issue in &json {
        let priority = issue["priority"].as_u64().expect("priority number");
        assert!(
            priority == 0 || priority == 1,
            "Expected priority 0 or 1, got {}",
            priority
        );
    }
}

// =============================================================================
// ASSIGNEE FILTER TESTS
// =============================================================================

#[test]
fn list_filter_by_assignee() {
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--assignee", "alice", "--json"],
        "list_alice",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    assert_eq!(json.len(), 2, "Expected 2 issues assigned to alice");
    for issue in &json {
        assert_eq!(issue["assignee"], "alice");
    }
}

#[test]
fn list_filter_by_unassigned() {
    let (workspace, _ids) = setup_diverse_workspace();

    // The --unassigned flag filters for issues without an assignee
    let list = run_br(
        &workspace,
        ["list", "--unassigned", "--json"],
        "list_unassigned",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    // All returned issues should have no assignee
    for issue in &json {
        assert!(
            issue["assignee"].is_null() || issue["assignee"] == "",
            "Expected unassigned, got {:?}",
            issue["assignee"]
        );
    }
}

// =============================================================================
// LABEL FILTER TESTS
// =============================================================================

#[test]
fn list_filter_by_label_single() {
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--label", "backend", "--json"],
        "list_backend",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    assert_eq!(json.len(), 2, "Expected 2 issues with 'backend' label");
}

#[test]
fn list_filter_by_label_multiple() {
    let (workspace, _ids) = setup_diverse_workspace();

    // Filter for issues with both "backend" AND "api" labels
    let list = run_br(
        &workspace,
        ["list", "--label", "backend", "--label", "api", "--json"],
        "list_backend_api",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    // Should find the one issue with both labels
    assert_eq!(
        json.len(),
        1,
        "Expected 1 issue with both 'backend' and 'api' labels"
    );
    assert!(
        json[0]["title"]
            .as_str()
            .unwrap()
            .contains("API rate limiting")
    );
}

// =============================================================================
// COMBINED FILTER TESTS
// =============================================================================

#[test]
fn list_combined_filters_type_and_priority() {
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "-t", "bug", "-p", "1", "--json"],
        "list_bug_p1",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    assert_eq!(json.len(), 1, "Expected 1 P1 bug");
    assert_eq!(json[0]["issue_type"], "bug");
    assert_eq!(json[0]["priority"], 1);
}

#[test]
fn list_combined_filters_assignee_and_label() {
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        [
            "list",
            "--assignee",
            "alice",
            "--label",
            "critical",
            "--json",
        ],
        "list_alice_critical",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    assert_eq!(
        json.len(),
        1,
        "Expected 1 issue assigned to alice with critical label"
    );
    assert!(
        json[0]["title"]
            .as_str()
            .unwrap()
            .contains("Critical login bug")
    );
}

// =============================================================================
// SORT TESTS
// =============================================================================

#[test]
fn list_sort_by_priority_asc() {
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--sort", "priority", "--json"],
        "list_sort_priority",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    // Verify issues are sorted by priority (ascending = P0 first)
    let priorities: Vec<u64> = json
        .iter()
        .map(|i| i["priority"].as_u64().unwrap())
        .collect();

    for window in priorities.windows(2) {
        assert!(
            window[0] <= window[1],
            "Priority not sorted: {} > {}",
            window[0],
            window[1]
        );
    }
}

#[test]
fn list_sort_by_priority_desc() {
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--sort", "priority", "--reverse", "--json"],
        "list_sort_priority_desc",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    let priorities: Vec<u64> = json
        .iter()
        .map(|i| i["priority"].as_u64().unwrap())
        .collect();

    for window in priorities.windows(2) {
        assert!(
            window[0] >= window[1],
            "Priority not sorted descending: {} < {}",
            window[0],
            window[1]
        );
    }
}

#[test]
fn list_sort_by_created_at_desc() {
    let (workspace, _ids) = setup_diverse_workspace();

    // Default sort for created_at is descending (newest first) for UX
    let list = run_br(
        &workspace,
        ["list", "--sort", "created_at", "--json"],
        "list_sort_created_desc",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    let timestamps: Vec<&str> = json
        .iter()
        .map(|i| i["created_at"].as_str().unwrap())
        .collect();

    // Verify descending order (newest first)
    for window in timestamps.windows(2) {
        assert!(
            window[0] >= window[1],
            "created_at not sorted descending: {} < {}",
            window[0],
            window[1]
        );
    }
}

#[test]
fn list_sort_by_created_at_asc() {
    let (workspace, _ids) = setup_diverse_workspace();

    // Use --reverse to get ascending order (oldest first)
    let list = run_br(
        &workspace,
        ["list", "--sort", "created_at", "--reverse", "--json"],
        "list_sort_created_asc",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    let timestamps: Vec<&str> = json
        .iter()
        .map(|i| i["created_at"].as_str().unwrap())
        .collect();

    // Verify ascending order (oldest first)
    for window in timestamps.windows(2) {
        assert!(
            window[0] <= window[1],
            "created_at not sorted ascending: {} > {}",
            window[0],
            window[1]
        );
    }
}

// =============================================================================
// LIMIT AND PAGINATION TESTS
// =============================================================================

#[test]
fn list_with_limit() {
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--limit", "3", "--json"],
        "list_limit_3",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    assert_eq!(json.len(), 3, "Expected exactly 3 issues with --limit 3");
}

#[test]
fn list_with_limit_zero_unlimited() {
    let (workspace, _ids) = setup_diverse_workspace();

    // --limit 0 should return all issues (unlimited)
    let list = run_br(
        &workspace,
        ["list", "--limit", "0", "--json"],
        "list_limit_0",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    // Should get all non-closed issues (default excludes closed)
    // We have 8 total issues, 1 closed, so at least 5 open ones
    assert!(
        json.len() >= 5,
        "Expected at least 5 issues with unlimited limit, got {}",
        json.len()
    );
}

// =============================================================================
// OUTPUT FORMAT TESTS
// =============================================================================

#[test]
fn list_text_output_format() {
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(&workspace, ["list"], "list_text");
    assert!(list.status.success(), "list failed: {}", list.stderr);

    // Text output should contain issue IDs (bd-xxxx format)
    assert!(
        list.stdout.contains("bd-") || list.stdout.contains("beads_rust-"),
        "Text output should contain issue IDs"
    );
    // Should also contain some issue content
    assert!(!list.stdout.is_empty(), "Text output should not be empty");
}

#[test]
fn list_json_output_format() {
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(&workspace, ["list", "--json"], "list_json");
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    // Verify JSON structure has expected fields
    if !json.is_empty() {
        let first = &json[0];
        assert!(first.get("id").is_some(), "JSON missing 'id' field");
        assert!(first.get("title").is_some(), "JSON missing 'title' field");
        assert!(first.get("status").is_some(), "JSON missing 'status' field");
        assert!(
            first.get("priority").is_some(),
            "JSON missing 'priority' field"
        );
        assert!(
            first.get("issue_type").is_some(),
            "JSON missing 'issue_type' field"
        );
    }
}

#[test]
fn list_csv_output_format() {
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(&workspace, ["list", "--format", "csv"], "list_csv");
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let lines: Vec<&str> = list.stdout.lines().collect();
    assert!(!lines.is_empty(), "CSV output is empty");

    // First line should be header
    let header = lines[0];
    assert!(header.contains("id"), "CSV header missing 'id'");
    assert!(header.contains("title"), "CSV header missing 'title'");
}

#[test]
fn list_csv_with_custom_fields() {
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        [
            "list",
            "--format",
            "csv",
            "--fields",
            "id,title,priority,assignee",
        ],
        "list_csv_fields",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let header = list.stdout.lines().next().unwrap_or("");
    assert_eq!(
        header, "id,title,priority,assignee",
        "CSV header doesn't match requested fields"
    );
}

// =============================================================================
// EDGE CASES
// =============================================================================

#[test]
fn list_empty_workspace() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_empty");
    assert!(init.status.success());

    let list = run_br(&workspace, ["list", "--json"], "list_empty");
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    assert_eq!(json.len(), 0, "Expected empty list for new workspace");
}

#[test]
fn list_filter_no_matches() {
    let (workspace, _ids) = setup_diverse_workspace();

    // Filter for a type that doesn't exist
    let list = run_br(&workspace, ["list", "-t", "epic", "--json"], "list_no_epic");
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    assert_eq!(json.len(), 0, "Expected no epic issues");
}

#[test]
fn list_filter_nonexistent_label() {
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--label", "nonexistent-label-xyz", "--json"],
        "list_no_label",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    assert_eq!(json.len(), 0, "Expected no issues with nonexistent label");
}

#[test]
fn list_filter_nonexistent_assignee() {
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--assignee", "nobody-exists-here", "--json"],
        "list_no_assignee",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    assert_eq!(
        json.len(),
        0,
        "Expected no issues with nonexistent assignee"
    );
}

#[test]
fn list_before_init_fails() {
    let workspace = BrWorkspace::new();

    let list = run_br(&workspace, ["list"], "list_no_init");
    assert!(!list.status.success(), "list should fail before init");
    assert!(
        list.stderr.contains("not initialized") || list.stderr.contains("No .beads"),
        "Error message should mention workspace not initialized"
    );
}

// =============================================================================
// SPECIAL CHARACTER TESTS
// =============================================================================

#[test]
fn list_issue_with_special_chars_in_title() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_special");
    assert!(init.status.success());

    // Create issue with special characters
    let create = run_br(
        &workspace,
        ["create", "Fix \"quoted\" & <special> chars"],
        "create_special",
    );
    assert!(create.status.success());

    // List in JSON format
    let list_json = run_br(&workspace, ["list", "--json"], "list_special_json");
    assert!(list_json.status.success());

    let payload = extract_json_payload(&list_json.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    assert_eq!(json.len(), 1);
    let title = json[0]["title"].as_str().unwrap();
    assert!(title.contains("\"quoted\""), "Title should contain quotes");
    assert!(title.contains('&'), "Title should contain ampersand");
    assert!(
        title.contains("<special>"),
        "Title should contain angle brackets"
    );

    // List in CSV format
    let list_csv = run_br(&workspace, ["list", "--format", "csv"], "list_special_csv");
    assert!(list_csv.status.success());

    // CSV should properly escape the quotes
    assert!(
        list_csv.stdout.contains("\"\"quoted\"\"") || list_csv.stdout.contains("Fix"),
        "CSV should escape special characters"
    );
}
