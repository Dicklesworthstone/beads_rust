//! Comprehensive E2E tests for the `list` command.
//!
//! Tests cover:
//! - Basic listing (text, JSON, CSV formats)
//! - Status filtering (--status, --all)
//! - Type filtering (--type)
//! - Priority filtering (--priority, --priority-min, --priority-max)
//! - Label filtering (--label AND, --label-any OR)
//! - Assignee filtering (--assignee, --unassigned)
//! - Text search (--title-contains, --desc-contains)
//! - Sorting (--sort, --reverse)
//! - Limiting (--limit)
//! - Deferred and overdue filtering (--deferred, --overdue)
//! - Output format variations (--long, --pretty)

mod common;

use common::cli::{BrWorkspace, parse_list_issues, parse_list_page, run_br};

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

/// Setup a workspace with a variety of test issues for comprehensive filtering.
#[allow(clippy::too_many_lines)]
fn setup_diverse_workspace() -> (BrWorkspace, Vec<String>) {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let mut ids = Vec::new();

    // Issue 1: Open task, P1, labeled "core"
    let issue1 = run_br(
        &workspace,
        ["create", "Core task", "-t", "task", "-p", "1"],
        "create_task1",
    );
    assert!(issue1.status.success());
    let id1 = parse_created_id(&issue1.stdout);
    run_br(
        &workspace,
        ["update", &id1, "--add-label", "core"],
        "label_task1",
    );
    ids.push(id1);

    // Issue 2: Open bug, P0, labeled "urgent", assigned to "alice"
    let issue2 = run_br(
        &workspace,
        ["create", "Critical bug", "-t", "bug", "-p", "0"],
        "create_bug1",
    );
    assert!(issue2.status.success());
    let id2 = parse_created_id(&issue2.stdout);
    run_br(
        &workspace,
        [
            "update",
            &id2,
            "--add-label",
            "urgent",
            "--assignee",
            "alice",
        ],
        "update_bug1",
    );
    ids.push(id2);

    // Issue 3: Open feature, P2, labeled "core" and "frontend", assigned to "bob"
    let issue3 = run_br(
        &workspace,
        ["create", "New feature", "-t", "feature", "-p", "2"],
        "create_feature1",
    );
    assert!(issue3.status.success());
    let id3 = parse_created_id(&issue3.stdout);
    run_br(
        &workspace,
        [
            "update",
            &id3,
            "--add-label",
            "core",
            "--add-label",
            "frontend",
            "--assignee",
            "bob",
        ],
        "update_feature1",
    );
    ids.push(id3);

    // Issue 4: Closed task, P3
    let issue4 = run_br(
        &workspace,
        ["create", "Old task", "-t", "task", "-p", "3"],
        "create_task2",
    );
    assert!(issue4.status.success());
    let id4 = parse_created_id(&issue4.stdout);
    // beads_rust#301: terminal-state transitions go through `br close` so
    // close-policy fires uniformly. `br update --status closed` is rejected.
    run_br(
        &workspace,
        ["close", &id4, "--reason", "fixture: closed in setup"],
        "close_task2",
    );
    ids.push(id4);

    // Issue 5: Deferred epic, P2
    let issue5 = run_br(
        &workspace,
        ["create", "Deferred epic", "-t", "epic", "-p", "2"],
        "create_epic1",
    );
    assert!(issue5.status.success());
    let id5 = parse_created_id(&issue5.stdout);
    run_br(
        &workspace,
        [
            "update",
            &id5,
            "--status",
            "deferred",
            "--defer",
            "2100-01-01T00:00:00Z",
        ],
        "defer_epic1",
    );
    ids.push(id5);

    // Issue 6: Open task, P4 (backlog), with description containing "searchable"
    let issue6 = run_br(
        &workspace,
        [
            "create",
            "Backlog item",
            "-t",
            "task",
            "-p",
            "4",
            "-d",
            "This is a searchable description",
        ],
        "create_task3",
    );
    assert!(issue6.status.success());
    let id6 = parse_created_id(&issue6.stdout);
    ids.push(id6);

    (workspace, ids)
}

// =============================================================================
// BASIC LISTING TESTS
// =============================================================================

#[test]
fn e2e_list_basic_text_output() {
    let _log = common::test_log("e2e_list_basic_text_output");
    let (workspace, ids) = setup_diverse_workspace();

    let list = run_br(&workspace, ["list"], "list_text");
    assert!(list.status.success(), "list failed: {}", list.stderr);

    // Default list excludes closed but includes deferred
    // Should contain: task1, bug1, feature1, epic1 (deferred), task3 = 5 issues
    for id in &ids[..3] {
        assert!(
            list.stdout.contains(id),
            "list should contain open issue {id}"
        );
    }
    // Backlog item should be included
    assert!(
        list.stdout.contains(&ids[5]),
        "list should contain backlog item"
    );

    // Deferred issue IS included by default
    assert!(
        list.stdout.contains(&ids[4]),
        "list should contain deferred issue by default"
    );

    // Closed issue should NOT be in default list
    assert!(
        !list.stdout.contains(&ids[3]),
        "list should not contain closed issue by default"
    );
}

#[test]
fn e2e_list_json_output() {
    let _log = common::test_log("e2e_list_json_output");
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(&workspace, ["list", "--json"], "list_json");
    assert!(list.status.success(), "list json failed: {}", list.stderr);

    let issues = parse_list_issues(&list.stdout);

    // Default list excludes closed but includes deferred
    // Should have 5 issues (4 open + 1 deferred)
    assert_eq!(issues.len(), 5, "expected 5 issues (4 open + 1 deferred)");

    // Verify JSON structure
    for issue in &issues {
        assert!(issue["id"].is_string(), "issue should have id");
        assert!(issue["title"].is_string(), "issue should have title");
        assert!(issue["status"].is_string(), "issue should have status");
        assert!(issue["priority"].is_number(), "issue should have priority");
    }
}

#[test]
fn e2e_list_csv_output() {
    let _log = common::test_log("e2e_list_csv_output");
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(&workspace, ["list", "--format", "csv"], "list_csv");
    assert!(list.status.success(), "list csv failed: {}", list.stderr);

    let lines: Vec<&str> = list.stdout.lines().collect();
    // Should have header + data rows
    assert!(lines.len() >= 2, "CSV should have header and data");

    // Check header
    let header = lines[0];
    assert!(header.contains("id"), "CSV header should have id");
    assert!(header.contains("title"), "CSV header should have title");
    assert!(header.contains("status"), "CSV header should have status");
}

// =============================================================================
// STATUS FILTERING TESTS
// =============================================================================

#[test]
fn e2e_list_status_filter_open() {
    let _log = common::test_log("e2e_list_status_filter_open");
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--status", "open", "--json"],
        "list_status_open",
    );
    assert!(list.status.success());

    let issues = parse_list_issues(&list.stdout);

    // All issues should be open
    for issue in &issues {
        assert_eq!(issue["status"], "open");
    }
}

#[test]
fn e2e_list_status_filter_closed() {
    let _log = common::test_log("e2e_list_status_filter_closed");
    let (workspace, ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--status", "closed", "--json"],
        "list_status_closed",
    );
    assert!(list.status.success());

    let issues = parse_list_issues(&list.stdout);

    // Should have exactly 1 closed issue
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0]["id"], ids[3]);
    assert_eq!(issues[0]["status"], "closed");
}

#[test]
fn e2e_list_status_filter_deferred() {
    let _log = common::test_log("e2e_list_status_filter_deferred");
    let (workspace, ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--status", "deferred", "--json"],
        "list_status_deferred",
    );
    assert!(list.status.success());

    let issues = parse_list_issues(&list.stdout);

    // Should have exactly 1 deferred issue
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0]["id"], ids[4]);
    assert_eq!(issues[0]["status"], "deferred");
}

#[test]
fn e2e_list_all_includes_closed() {
    let _log = common::test_log("e2e_list_all_includes_closed");
    let (workspace, ids) = setup_diverse_workspace();

    let list = run_br(&workspace, ["list", "--all", "--json"], "list_all");
    assert!(list.status.success());

    let issues = parse_list_issues(&list.stdout);

    // Should have all 6 issues
    assert_eq!(issues.len(), 6, "expected all 6 issues");

    // Verify closed issue is present
    assert!(
        issues.iter().any(|i| i["id"] == ids[3]),
        "closed issue should be included with --all"
    );
}

#[test]
fn e2e_list_multiple_status_filter() {
    let _log = common::test_log("e2e_list_multiple_status_filter");
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--status", "open", "--status", "closed", "--json"],
        "list_multi_status",
    );
    assert!(list.status.success());

    let issues = parse_list_issues(&list.stdout);

    // Should have open and closed issues (5 total: 4 open + 1 closed)
    assert_eq!(issues.len(), 5);

    // Verify statuses
    for issue in &issues {
        let status = issue["status"].as_str().unwrap();
        assert!(
            status == "open" || status == "closed",
            "unexpected status: {status}"
        );
    }
}

// =============================================================================
// TYPE FILTERING TESTS
// =============================================================================

#[test]
fn e2e_list_type_filter_task() {
    let _log = common::test_log("e2e_list_type_filter_task");
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--type", "task", "--json"],
        "list_type_task",
    );
    assert!(list.status.success());

    let issues = parse_list_issues(&list.stdout);

    // Should have 2 open tasks (task1 and task3; task2 is closed)
    assert_eq!(issues.len(), 2);
    for issue in &issues {
        assert_eq!(issue["issue_type"], "task");
    }
}

#[test]
fn e2e_list_type_filter_bug() {
    let _log = common::test_log("e2e_list_type_filter_bug");
    let (workspace, ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--type", "bug", "--json"],
        "list_type_bug",
    );
    assert!(list.status.success());

    let issues = parse_list_issues(&list.stdout);

    // Should have exactly 1 bug
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0]["id"], ids[1]);
    assert_eq!(issues[0]["issue_type"], "bug");
}

#[test]
fn e2e_list_multiple_type_filter() {
    let _log = common::test_log("e2e_list_multiple_type_filter");
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--type", "bug", "--type", "feature", "--json"],
        "list_multi_type",
    );
    assert!(list.status.success());

    let issues = parse_list_issues(&list.stdout);

    // Should have 1 bug + 1 feature = 2 issues
    assert_eq!(issues.len(), 2);
    for issue in &issues {
        let issue_type = issue["issue_type"].as_str().unwrap();
        assert!(
            issue_type == "bug" || issue_type == "feature",
            "unexpected type: {issue_type}"
        );
    }
}

// =============================================================================
// PRIORITY FILTERING TESTS
// =============================================================================

#[test]
fn e2e_list_priority_filter_exact() {
    let _log = common::test_log("e2e_list_priority_filter_exact");
    let (workspace, ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--priority", "0", "--json"],
        "list_priority_0",
    );
    assert!(list.status.success());

    let issues = parse_list_issues(&list.stdout);

    // Should have exactly 1 P0 issue (the critical bug)
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0]["id"], ids[1]);
    assert_eq!(issues[0]["priority"], 0);
}

#[test]
fn e2e_list_priority_min() {
    let _log = common::test_log("e2e_list_priority_min");
    let (workspace, _ids) = setup_diverse_workspace();

    // priority-min=2 means priority >= 2 (P2, P3, P4 = lower priority)
    let list = run_br(
        &workspace,
        ["list", "--priority-min", "2", "--json"],
        "list_priority_min",
    );
    assert!(list.status.success());

    let issues = parse_list_issues(&list.stdout);

    // Should have issues with priority >= 2 (feature1 P2, epic1 P2 deferred, task3 P4)
    // Note: deferred issues ARE included by default
    assert_eq!(issues.len(), 3);
    for issue in &issues {
        let priority = issue["priority"].as_u64().unwrap();
        assert!(priority >= 2, "priority should be >= 2, got {priority}");
    }
}

#[test]
fn e2e_list_priority_max() {
    let _log = common::test_log("e2e_list_priority_max");
    let (workspace, _ids) = setup_diverse_workspace();

    // priority-max=1 means priority <= 1 (P0, P1 = high priority)
    let list = run_br(
        &workspace,
        ["list", "--priority-max", "1", "--json"],
        "list_priority_max",
    );
    assert!(list.status.success());

    let issues = parse_list_issues(&list.stdout);

    // Should have issues with priority <= 1 (task1 P1, bug1 P0)
    assert_eq!(issues.len(), 2);
    for issue in &issues {
        let priority = issue["priority"].as_u64().unwrap();
        assert!(priority <= 1, "priority should be <= 1, got {priority}");
    }
}

#[test]
fn e2e_list_priority_range() {
    let _log = common::test_log("e2e_list_priority_range");
    let (workspace, _ids) = setup_diverse_workspace();

    // Priority range 1-2
    let list = run_br(
        &workspace,
        [
            "list",
            "--priority-min",
            "1",
            "--priority-max",
            "2",
            "--json",
        ],
        "list_priority_range",
    );
    assert!(list.status.success());

    let issues = parse_list_issues(&list.stdout);

    // Should have P1 and P2 issues (task1 P1, feature1 P2, epic1 P2 deferred)
    // Note: deferred issues ARE included by default
    assert_eq!(issues.len(), 3);
    for issue in &issues {
        let priority = issue["priority"].as_u64().unwrap();
        assert!(
            (1..=2).contains(&priority),
            "priority should be 1-2, got {priority}"
        );
    }
}

// =============================================================================
// LABEL FILTERING TESTS
// =============================================================================

#[test]
fn e2e_list_label_filter_and() {
    let _log = common::test_log("e2e_list_label_filter_and");
    let (workspace, ids) = setup_diverse_workspace();

    // Filter by label "core" (AND logic)
    let list = run_br(
        &workspace,
        ["list", "--label", "core", "--json"],
        "list_label_core",
    );
    assert!(list.status.success());

    let issues = parse_list_issues(&list.stdout);

    // Should have 2 issues with "core" label (task1, feature1)
    assert_eq!(issues.len(), 2);
    assert!(issues.iter().any(|i| i["id"] == ids[0]));
    assert!(issues.iter().any(|i| i["id"] == ids[2]));
}

#[test]
fn e2e_list_label_filter_multiple_and() {
    let _log = common::test_log("e2e_list_label_filter_multiple_and");
    let (workspace, ids) = setup_diverse_workspace();

    // Filter by labels "core" AND "frontend" (must have both)
    let list = run_br(
        &workspace,
        ["list", "--label", "core", "--label", "frontend", "--json"],
        "list_label_and",
    );
    assert!(list.status.success());

    let issues = parse_list_issues(&list.stdout);

    // Should have only feature1 (has both core and frontend)
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0]["id"], ids[2]);
}

#[test]
fn e2e_list_label_filter_or() {
    let _log = common::test_log("e2e_list_label_filter_or");
    let (workspace, ids) = setup_diverse_workspace();

    // Filter by labels "urgent" OR "frontend" (any match)
    let list = run_br(
        &workspace,
        [
            "list",
            "--label-any",
            "urgent",
            "--label-any",
            "frontend",
            "--json",
        ],
        "list_label_or",
    );
    assert!(list.status.success());

    let issues = parse_list_issues(&list.stdout);

    // Should have bug1 (urgent) and feature1 (frontend) = 2 issues
    assert_eq!(issues.len(), 2);
    assert!(issues.iter().any(|i| i["id"] == ids[1])); // bug1 with urgent
    assert!(issues.iter().any(|i| i["id"] == ids[2])); // feature1 with frontend
}

// =============================================================================
// ASSIGNEE FILTERING TESTS
// =============================================================================

#[test]
fn e2e_list_assignee_filter() {
    let _log = common::test_log("e2e_list_assignee_filter");
    let (workspace, ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--assignee", "alice", "--json"],
        "list_assignee_alice",
    );
    assert!(list.status.success());

    let issues = parse_list_issues(&list.stdout);

    // Should have exactly 1 issue assigned to alice (bug1)
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0]["id"], ids[1]);
    assert_eq!(issues[0]["assignee"], "alice");
}

#[test]
fn e2e_list_unassigned_filter() {
    let _log = common::test_log("e2e_list_unassigned_filter");
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--unassigned", "--json"],
        "list_unassigned",
    );
    assert!(list.status.success());

    let issues = parse_list_issues(&list.stdout);

    // Should have 3 unassigned non-closed issues: task1, epic1 (deferred), task3
    // bug1 assigned to alice, feature1 assigned to bob, task2 is closed
    assert_eq!(issues.len(), 3);
    for issue in &issues {
        assert!(
            issue["assignee"].is_null() || issue["assignee"] == "",
            "issue should be unassigned"
        );
    }
}

// =============================================================================
// TEXT SEARCH TESTS
// =============================================================================

#[test]
fn e2e_list_title_contains() {
    let _log = common::test_log("e2e_list_title_contains");
    let (workspace, ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--title-contains", "Critical", "--json"],
        "list_title_contains",
    );
    assert!(list.status.success());

    let issues = parse_list_issues(&list.stdout);

    // Should match "Critical bug"
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0]["id"], ids[1]);
    assert!(issues[0]["title"].as_str().unwrap().contains("Critical"));
}

#[test]
fn e2e_list_desc_contains() {
    let _log = common::test_log("e2e_list_desc_contains");
    let (workspace, ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--desc-contains", "searchable", "--json"],
        "list_desc_contains",
    );
    assert!(list.status.success());

    let issues = parse_list_issues(&list.stdout);

    // Should match the backlog item with "searchable" in description
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0]["id"], ids[5]);
}

// =============================================================================
// SORTING TESTS
// =============================================================================

#[test]
fn e2e_list_sort_by_priority() {
    let _log = common::test_log("e2e_list_sort_by_priority");
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--sort", "priority", "--json"],
        "list_sort_priority",
    );
    assert!(list.status.success());

    let issues = parse_list_issues(&list.stdout);

    // Verify sorted by priority ascending (P0 first)
    let priorities: Vec<u64> = issues
        .iter()
        .map(|i| i["priority"].as_u64().unwrap())
        .collect();
    let mut sorted = priorities.clone();
    sorted.sort_unstable();
    assert_eq!(
        priorities, sorted,
        "issues should be sorted by priority ascending"
    );
}

#[test]
fn e2e_list_sort_by_priority_reverse() {
    let _log = common::test_log("e2e_list_sort_by_priority_reverse");
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--sort", "priority", "--reverse", "--json"],
        "list_sort_priority_rev",
    );
    assert!(list.status.success());

    let issues = parse_list_issues(&list.stdout);

    // Verify sorted by priority descending (P4 first)
    let priorities: Vec<u64> = issues
        .iter()
        .map(|i| i["priority"].as_u64().unwrap())
        .collect();
    let mut sorted = priorities.clone();
    sorted.sort_unstable();
    sorted.reverse();
    assert_eq!(
        priorities, sorted,
        "issues should be sorted by priority descending"
    );
}

#[test]
fn e2e_list_sort_by_title() {
    let _log = common::test_log("e2e_list_sort_by_title");
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--sort", "title", "--json"],
        "list_sort_title",
    );
    assert!(list.status.success());

    let issues = parse_list_issues(&list.stdout);

    // Verify sorted by title alphabetically
    let titles: Vec<&str> = issues
        .iter()
        .map(|i| i["title"].as_str().unwrap())
        .collect();
    let mut sorted = titles.clone();
    sorted.sort_unstable();
    assert_eq!(titles, sorted, "issues should be sorted by title");
}

// =============================================================================
// LIMIT TESTS
// =============================================================================

#[test]
fn e2e_list_limit() {
    let _log = common::test_log("e2e_list_limit");
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(&workspace, ["list", "--limit", "2", "--json"], "list_limit");
    assert!(list.status.success());

    let issues = parse_list_issues(&list.stdout);

    // Should have exactly 2 issues
    assert_eq!(issues.len(), 2);
}

#[test]
fn e2e_list_limit_with_label_filter() {
    let _log = common::test_log("e2e_list_limit_with_label_filter");
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--label", "core", "--limit", "1", "--json"],
        "list_limit_label",
    );
    assert!(list.status.success());

    let issues = parse_list_issues(&list.stdout);

    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0]["title"], "Core task");
}

#[test]
fn e2e_list_limit_zero_unlimited() {
    let _log = common::test_log("e2e_list_limit_zero_unlimited");
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--limit", "0", "--json"],
        "list_limit_unlimited",
    );
    assert!(list.status.success());

    let issues = parse_list_issues(&list.stdout);

    // Should have all 5 non-closed issues (limit=0 means unlimited)
    // Default list excludes closed but includes deferred
    assert_eq!(issues.len(), 5);
}

#[test]
fn e2e_list_limit_zero_with_offset_reports_unpaginated_total() {
    let _log = common::test_log("e2e_list_limit_zero_with_offset_reports_unpaginated_total");
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--limit", "0", "--offset", "2", "--json"],
        "list_limit_zero_offset",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let page = parse_list_page(&list.stdout);
    let issues = page["issues"].as_array().expect("issues array");

    assert_eq!(issues.len(), 3);
    assert_eq!(page["total"].as_u64(), Some(5));
    assert_eq!(page["limit"].as_u64(), Some(0));
    assert_eq!(page["offset"].as_u64(), Some(2));
    assert_eq!(page["has_more"].as_bool(), Some(false));
}

// =============================================================================
// DEFERRED FILTER TESTS
// =============================================================================

#[test]
fn e2e_list_deferred_flag() {
    let _log = common::test_log("e2e_list_deferred_flag");
    let (workspace, ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--deferred", "--json"],
        "list_deferred",
    );
    assert!(list.status.success());

    let issues = parse_list_issues(&list.stdout);

    // Should include all open + deferred issues (5 total)
    assert_eq!(issues.len(), 5);

    // Verify deferred issue is present
    assert!(
        issues.iter().any(|i| i["id"] == ids[4]),
        "deferred issue should be included with --deferred flag"
    );
}

// =============================================================================
// OUTPUT FORMAT TESTS
// =============================================================================

#[test]
fn e2e_list_long_format() {
    let _log = common::test_log("e2e_list_long_format");
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(&workspace, ["list", "--long"], "list_long");
    assert!(list.status.success());

    assert!(
        list.stdout.contains("Status: "),
        "long format should emit explicit status lines: {}",
        list.stdout
    );
    assert!(
        list.stdout.contains("Created: "),
        "long format should emit created timestamps: {}",
        list.stdout
    );
}

#[test]
fn e2e_list_pretty_format() {
    let _log = common::test_log("e2e_list_pretty_format");
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(&workspace, ["list", "--pretty"], "list_pretty");
    assert!(list.status.success());

    assert!(
        list.stdout.contains("├── ") || list.stdout.contains("└── "),
        "pretty format should emit tree connectors: {}",
        list.stdout
    );
}

#[test]
fn e2e_list_default_and_pretty_outputs_differ() {
    let _log = common::test_log("e2e_list_default_and_pretty_outputs_differ");
    let (workspace, _ids) = setup_diverse_workspace();

    let normal = run_br(&workspace, ["list"], "list_default_plain");
    let pretty = run_br(&workspace, ["list", "--pretty"], "list_pretty_plain");

    assert!(normal.status.success());
    assert!(pretty.status.success());
    assert_ne!(
        normal.stdout, pretty.stdout,
        "pretty flag should change plain-text output"
    );
}

#[test]
fn e2e_list_csv_custom_fields() {
    let _log = common::test_log("e2e_list_csv_custom_fields");
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
    assert!(list.status.success());

    let lines: Vec<&str> = list.stdout.lines().collect();
    assert!(lines.len() >= 2);

    // Check header has only requested fields
    let header = lines[0];
    assert_eq!(header, "id,title,priority,assignee");
}

#[test]
fn e2e_list_csv_escaping() {
    let _log = common::test_log("e2e_list_csv_escaping");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    // Create issues with CSV-problematic characters
    let create_comma = run_br(
        &workspace,
        ["create", "Fix login, signup flow"],
        "create_comma",
    );
    assert!(create_comma.status.success());

    let create_quote = run_br(
        &workspace,
        ["create", "Handle \"double quotes\" properly"],
        "create_quote",
    );
    assert!(create_quote.status.success());

    let list = run_br(&workspace, ["list", "--format", "csv"], "list_csv_escape");
    assert!(list.status.success(), "list csv failed: {}", list.stderr);

    let lines: Vec<&str> = list.stdout.lines().collect();
    assert!(lines.len() >= 3, "should have header + 2 data rows");

    // Verify the title with comma is quoted
    let csv_text = &list.stdout;
    assert!(
        csv_text.contains("\"Fix login, signup flow\"")
            || csv_text.contains("Fix login, signup flow"),
        "comma in title should be properly handled in CSV"
    );

    // Verify each data row has the same number of commas as the header
    // (field count consistency)
    let header_fields = lines[0].matches(',').count();
    for (i, line) in lines.iter().enumerate().skip(1) {
        if !line.is_empty() {
            // For quoted fields, count commas outside quotes
            let unquoted_comma_count = count_csv_field_separators(line);
            assert_eq!(
                unquoted_comma_count, header_fields,
                "row {i} has {unquoted_comma_count} separators, expected {header_fields}: {line}"
            );
        }
    }
}

/// Count field separator commas in a CSV line (ignoring commas inside quotes).
fn count_csv_field_separators(line: &str) -> usize {
    let mut in_quotes = false;
    let mut count = 0;
    let mut prev = '\0';
    for ch in line.chars() {
        match ch {
            '"' if prev != '\\' => in_quotes = !in_quotes,
            ',' if !in_quotes => count += 1,
            _ => {}
        }
        prev = ch;
    }
    count
}

// =============================================================================
// COMBINED FILTER TESTS
// =============================================================================

#[test]
fn e2e_list_combined_filters() {
    let _log = common::test_log("e2e_list_combined_filters");
    let (workspace, ids) = setup_diverse_workspace();

    // Combine type, priority, and label filters
    let list = run_br(
        &workspace,
        [
            "list",
            "--type",
            "task",
            "--priority-max",
            "2",
            "--label",
            "core",
            "--json",
        ],
        "list_combined",
    );
    assert!(list.status.success());

    let issues = parse_list_issues(&list.stdout);

    // Should match only task1 (task, P1, has core label)
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0]["id"], ids[0]);
}

#[test]
fn e2e_list_empty_result() {
    let _log = common::test_log("e2e_list_empty_result");
    let (workspace, _ids) = setup_diverse_workspace();

    // Filter that matches nothing
    let list = run_br(
        &workspace,
        [
            "list",
            "--type",
            "bug",
            "--assignee",
            "nonexistent",
            "--json",
        ],
        "list_empty",
    );
    assert!(list.status.success());

    let issues = parse_list_issues(&list.stdout);

    // Should be empty
    assert!(issues.is_empty(), "expected no matching issues");
}

// =============================================================================
// ERROR CASE TESTS
// =============================================================================

#[test]
fn e2e_list_before_init_fails() {
    let _log = common::test_log("e2e_list_before_init_fails");
    let workspace = BrWorkspace::new();

    let list = run_br(&workspace, ["list"], "list_no_init");
    assert!(!list.status.success(), "list should fail before init");
    assert!(
        list.stderr.contains("not initialized")
            || list.stderr.contains("NotInitialized")
            || list.stderr.contains("not found")
            || list.stderr.contains(".beads"),
        "error should mention initialization: {}",
        list.stderr
    );
}

#[test]
fn e2e_list_custom_status() {
    let _log = common::test_log("e2e_list_custom_status");
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--status", "invalid_status"],
        "list_custom_status",
    );
    assert!(
        list.status.success(),
        "list with custom status should succeed (custom statuses are allowed)"
    );
    assert!(
        list.stdout.trim().is_empty(),
        "list with unknown custom status should return an empty plain-text result: {}",
        list.stdout
    );
}

#[test]
fn e2e_list_custom_type() {
    // Custom types are allowed (see IssueType::from_str which accepts any string as Custom variant)
    let _log = common::test_log("e2e_list_custom_type");
    let (workspace, _ids) = setup_diverse_workspace();

    let list = run_br(
        &workspace,
        ["list", "--type", "custom_type", "--json"],
        "list_custom_type",
    );
    assert!(
        list.status.success(),
        "list with custom type should succeed (custom types are allowed)"
    );

    // Since no issues have type "custom_type", result should be empty
    let issues = parse_list_issues(&list.stdout);
    assert!(
        issues.is_empty(),
        "no issues should match custom type filter"
    );
}
