mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;
use std::fs;

fn parse_created_id(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or("");
    let id_part = line
        .strip_prefix("Created ")
        .and_then(|rest| rest.split(':').next())
        .unwrap_or("");
    id_part.trim().to_string()
}

fn setup_workspace_with_issues() -> (BrWorkspace, Vec<String>) {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let mut ids = Vec::new();

    // Issue 1: High priority task assigned to alice with "backend" label
    let issue1 = run_br(
        &workspace,
        ["create", "Backend API", "-p", "1", "-t", "task"],
        "create_issue1",
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
            "backend",
        ],
        "update_issue1",
    );
    ids.push(id1);

    // Issue 2: Medium priority bug assigned to bob with "frontend" label
    let issue2 = run_br(
        &workspace,
        ["create", "Frontend Bug", "-p", "2", "-t", "bug"],
        "create_issue2",
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
            "frontend",
        ],
        "update_issue2",
    );
    ids.push(id2);

    // Issue 3: Low priority feature unassigned with "backend" and "api" labels
    let issue3 = run_br(
        &workspace,
        ["create", "New Feature", "-p", "3", "-t", "feature"],
        "create_issue3",
    );
    assert!(issue3.status.success());
    let id3 = parse_created_id(&issue3.stdout);
    run_br(
        &workspace,
        [
            "update",
            &id3,
            "--add-label",
            "backend",
            "--add-label",
            "api",
        ],
        "update_issue3",
    );
    ids.push(id3);

    // Issue 4: Critical task unassigned with "urgent" label
    let issue4 = run_br(
        &workspace,
        ["create", "Critical Fix", "-p", "0", "-t", "task"],
        "create_issue4",
    );
    assert!(issue4.status.success());
    let id4 = parse_created_id(&issue4.stdout);
    run_br(
        &workspace,
        ["update", &id4, "--add-label", "urgent"],
        "update_issue4",
    );
    ids.push(id4);

    // Issue 5: Backlog task assigned to alice
    let issue5 = run_br(
        &workspace,
        ["create", "Backlog Item", "-p", "4", "-t", "task"],
        "create_issue5",
    );
    assert!(issue5.status.success());
    let id5 = parse_created_id(&issue5.stdout);
    run_br(
        &workspace,
        ["update", &id5, "--assignee", "alice"],
        "update_issue5",
    );
    ids.push(id5);

    (workspace, ids)
}

#[test]
fn ready_cli_filters_by_assignee() {
    let (workspace, ids) = setup_workspace_with_issues();

    let result = run_br(
        &workspace,
        ["ready", "--assignee", "alice", "--json"],
        "ready_assignee",
    );
    assert!(result.status.success(), "ready failed: {}", result.stderr);

    let payload = extract_json_payload(&result.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).expect("valid json");

    // Should have alice's issues: issue 1 and issue 5
    assert_eq!(issues.len(), 2);
    assert!(
        issues
            .iter()
            .map(|i| i["id"].as_str().unwrap())
            .any(|id| id == ids[0].as_str())
    ); // Backend API
    assert!(
        issues
            .iter()
            .map(|i| i["id"].as_str().unwrap())
            .any(|id| id == ids[4].as_str())
    ); // Backlog Item
}

#[test]
fn ready_respects_external_dependencies() {
    let workspace = BrWorkspace::new();
    let external = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_main");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    let init_ext = run_br(&external, ["init"], "init_external");
    assert!(
        init_ext.status.success(),
        "external init failed: {}",
        init_ext.stderr
    );

    let config_path = workspace.root.join(".beads/config.yaml");
    let external_path = external.root.display();
    let config = format!("issue_prefix: bd\nexternal_projects:\n  extproj: \"{external_path}\"\n");
    fs::write(&config_path, config).expect("write config");
    let external_config_path = external.root.join(".beads/config.yaml");
    fs::write(&external_config_path, "issue_prefix: bd\n").expect("write ext config");

    let issue = run_br(&workspace, ["create", "Main issue"], "create_main_issue");
    assert!(issue.status.success(), "create failed: {}", issue.stderr);
    let issue_id = parse_created_id(&issue.stdout);

    let dep_add = run_br(
        &workspace,
        ["dep", "add", &issue_id, "external:extproj:auth"],
        "dep_add_external",
    );
    assert!(
        dep_add.status.success(),
        "dep add failed: {}",
        dep_add.stderr
    );

    let ready_before = run_br(&workspace, ["ready", "--json"], "ready_before");
    assert!(
        ready_before.status.success(),
        "ready before failed: {}",
        ready_before.stderr
    );
    let ready_payload = extract_json_payload(&ready_before.stdout);
    let ready_json: Vec<Value> = serde_json::from_str(&ready_payload).expect("ready json");
    assert!(
        !ready_json.iter().any(|item| item["id"] == issue_id),
        "issue should be blocked by external dependency"
    );

    let blocked_before = run_br(&workspace, ["blocked", "--json"], "blocked_before");
    assert!(
        blocked_before.status.success(),
        "blocked before failed: {}",
        blocked_before.stderr
    );
    let blocked_payload = extract_json_payload(&blocked_before.stdout);
    let blocked_json: Vec<Value> = serde_json::from_str(&blocked_payload).expect("blocked json");
    assert!(
        blocked_json.iter().any(|item| item["id"] == issue_id),
        "blocked list should include external-blocked issue"
    );

    let provider = run_br(&external, ["create", "Provide auth"], "ext_create");
    assert!(
        provider.status.success(),
        "external create failed: {}",
        provider.stderr
    );
    let provider_id = parse_created_id(&provider.stdout);

    let label = run_br(
        &external,
        ["update", &provider_id, "--add-label", "provides:auth"],
        "ext_label",
    );
    assert!(
        label.status.success(),
        "external label failed: {}",
        label.stderr
    );

    let close = run_br(&external, ["close", &provider_id], "ext_close");
    assert!(
        close.status.success(),
        "external close failed: {}",
        close.stderr
    );

    let ready_after = run_br(&workspace, ["ready", "--json"], "ready_after");
    assert!(
        ready_after.status.success(),
        "ready after failed: {}",
        ready_after.stderr
    );
    let ready_payload = extract_json_payload(&ready_after.stdout);
    let ready_json: Vec<Value> = serde_json::from_str(&ready_payload).expect("ready json");
    assert!(
        ready_json.iter().any(|item| item["id"] == issue_id),
        "issue should be ready once external dependency is satisfied"
    );

    let blocked_after = run_br(&workspace, ["blocked", "--json"], "blocked_after");
    assert!(
        blocked_after.status.success(),
        "blocked after failed: {}",
        blocked_after.stderr
    );
    let blocked_payload = extract_json_payload(&blocked_after.stdout);
    let blocked_json: Vec<Value> = serde_json::from_str(&blocked_payload).expect("blocked json");
    assert!(
        !blocked_json.iter().any(|item| item["id"] == issue_id),
        "blocked list should clear after external dependency is satisfied"
    );
}

#[test]
fn ready_cli_filters_unassigned_only() {
    let (workspace, ids) = setup_workspace_with_issues();

    let result = run_br(
        &workspace,
        ["ready", "--unassigned", "--json"],
        "ready_unassigned",
    );
    assert!(result.status.success(), "ready failed: {}", result.stderr);

    let payload = extract_json_payload(&result.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).expect("valid json");

    // Should have unassigned issues: issue 3 and issue 4
    assert_eq!(issues.len(), 2);
    assert!(
        issues
            .iter()
            .map(|i| i["id"].as_str().unwrap())
            .any(|id| id == ids[2].as_str())
    ); // New Feature
    assert!(
        issues
            .iter()
            .map(|i| i["id"].as_str().unwrap())
            .any(|id| id == ids[3].as_str())
    ); // Critical Fix
}

#[test]
fn ready_cli_filters_by_type() {
    let (workspace, _ids) = setup_workspace_with_issues();

    // Filter by task type
    let result = run_br(
        &workspace,
        ["ready", "--type", "task", "--json"],
        "ready_type_task",
    );
    assert!(result.status.success(), "ready failed: {}", result.stderr);

    let payload = extract_json_payload(&result.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).expect("valid json");

    // Should have tasks: issue 1, 4, and 5
    assert_eq!(issues.len(), 3);
    for issue in &issues {
        assert_eq!(issue["issue_type"], "task");
    }
}

#[test]
fn ready_cli_filters_by_multiple_types() {
    let (workspace, _ids) = setup_workspace_with_issues();

    // Filter by task and bug types
    let result = run_br(
        &workspace,
        ["ready", "--type", "task", "--type", "bug", "--json"],
        "ready_type_multi",
    );
    assert!(result.status.success(), "ready failed: {}", result.stderr);

    let payload = extract_json_payload(&result.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).expect("valid json");

    // Should have tasks and bugs: issue 1, 2, 4, and 5
    assert_eq!(issues.len(), 4);
    for issue in &issues {
        let issue_type = issue["issue_type"].as_str().unwrap();
        assert!(issue_type == "task" || issue_type == "bug");
    }
}

#[test]
fn ready_cli_filters_by_priority() {
    let (workspace, ids) = setup_workspace_with_issues();

    // Filter by priority 0 (critical)
    let result = run_br(
        &workspace,
        ["ready", "--priority", "0", "--json"],
        "ready_priority",
    );
    assert!(result.status.success(), "ready failed: {}", result.stderr);

    let payload = extract_json_payload(&result.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).expect("valid json");

    // Should have only issue 4
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0]["id"].as_str().unwrap(), ids[3]);
}

#[test]
fn ready_cli_filters_by_multiple_priorities() {
    let (workspace, _ids) = setup_workspace_with_issues();

    // Filter by priority 0 and 1
    let result = run_br(
        &workspace,
        ["ready", "--priority", "0", "--priority", "1", "--json"],
        "ready_priority_multi",
    );
    assert!(result.status.success(), "ready failed: {}", result.stderr);

    let payload = extract_json_payload(&result.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).expect("valid json");

    // Should have issue 1 (P1) and issue 4 (P0)
    assert_eq!(issues.len(), 2);
    for issue in &issues {
        let priority = issue["priority"].as_u64().unwrap();
        assert!(priority == 0 || priority == 1);
    }
}

#[test]
fn ready_cli_filters_by_label_and() {
    let (workspace, ids) = setup_workspace_with_issues();

    // Filter by "backend" label
    let result = run_br(
        &workspace,
        ["ready", "--label", "backend", "--json"],
        "ready_label_and",
    );
    assert!(result.status.success(), "ready failed: {}", result.stderr);

    let payload = extract_json_payload(&result.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).expect("valid json");

    // Should have issue 1 and issue 3
    assert_eq!(issues.len(), 2);
    assert!(
        issues
            .iter()
            .map(|i| i["id"].as_str().unwrap())
            .any(|id| id == ids[0].as_str())
    );
    assert!(
        issues
            .iter()
            .map(|i| i["id"].as_str().unwrap())
            .any(|id| id == ids[2].as_str())
    );
}

#[test]
fn ready_cli_filters_by_multiple_labels_and() {
    let (workspace, ids) = setup_workspace_with_issues();

    // Filter by both "backend" AND "api" labels
    let result = run_br(
        &workspace,
        ["ready", "--label", "backend", "--label", "api", "--json"],
        "ready_label_and_multi",
    );
    assert!(result.status.success(), "ready failed: {}", result.stderr);

    let payload = extract_json_payload(&result.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).expect("valid json");

    // Should only have issue 3 (both labels)
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0]["id"].as_str().unwrap(), ids[2]);
}

#[test]
fn ready_cli_filters_by_label_or() {
    let (workspace, _ids) = setup_workspace_with_issues();

    // Filter by "backend" OR "frontend" labels
    let result = run_br(
        &workspace,
        [
            "ready",
            "--label-any",
            "backend",
            "--label-any",
            "frontend",
            "--json",
        ],
        "ready_label_or",
    );
    assert!(result.status.success(), "ready failed: {}", result.stderr);

    let payload = extract_json_payload(&result.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).expect("valid json");

    // Should have issues 1, 2, and 3
    assert_eq!(issues.len(), 3);
}

#[test]
fn ready_cli_respects_limit() {
    let (workspace, _ids) = setup_workspace_with_issues();

    let result = run_br(
        &workspace,
        ["ready", "--limit", "2", "--json"],
        "ready_limit",
    );
    assert!(result.status.success(), "ready failed: {}", result.stderr);

    let payload = extract_json_payload(&result.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).expect("valid json");

    assert_eq!(issues.len(), 2);
}

#[test]
fn ready_cli_limit_zero_returns_all() {
    let (workspace, _ids) = setup_workspace_with_issues();

    let result = run_br(
        &workspace,
        ["ready", "--limit", "0", "--json"],
        "ready_limit_zero",
    );
    assert!(result.status.success(), "ready failed: {}", result.stderr);

    let payload = extract_json_payload(&result.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).expect("valid json");

    // All 5 issues
    assert_eq!(issues.len(), 5);
}

#[test]
fn ready_cli_sort_priority() {
    let (workspace, ids) = setup_workspace_with_issues();

    let result = run_br(
        &workspace,
        ["ready", "--sort", "priority", "--limit", "0", "--json"],
        "ready_sort_priority",
    );
    assert!(result.status.success(), "ready failed: {}", result.stderr);

    let payload = extract_json_payload(&result.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).expect("valid json");

    // First should be P0 (Critical Fix - ids[3])
    assert_eq!(issues[0]["id"].as_str().unwrap(), ids[3]);
    // Second should be P1 (Backend API - ids[0])
    assert_eq!(issues[1]["id"].as_str().unwrap(), ids[0]);
}

#[test]
fn ready_cli_combined_filters() {
    let (workspace, ids) = setup_workspace_with_issues();

    // Filter by assignee "alice" AND type "task"
    let result = run_br(
        &workspace,
        ["ready", "--assignee", "alice", "--type", "task", "--json"],
        "ready_combined",
    );
    assert!(result.status.success(), "ready failed: {}", result.stderr);

    let payload = extract_json_payload(&result.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).expect("valid json");

    // Should have issue 1 and issue 5 (both alice's tasks)
    assert_eq!(issues.len(), 2);
    assert!(
        issues
            .iter()
            .map(|i| i["id"].as_str().unwrap())
            .any(|id| id == ids[0].as_str())
    );
    assert!(
        issues
            .iter()
            .map(|i| i["id"].as_str().unwrap())
            .any(|id| id == ids[4].as_str())
    );
}

#[test]
fn ready_cli_excludes_blocked_issues() {
    let (workspace, ids) = setup_workspace_with_issues();

    // Create a dependency: issue 3 is blocked by issue 1
    let dep = run_br(&workspace, ["dep", "add", &ids[2], &ids[0]], "add_dep");
    assert!(dep.status.success(), "dep add failed: {}", dep.stderr);

    // Ready should NOT include the blocked issue
    let result = run_br(
        &workspace,
        ["ready", "--limit", "0", "--json"],
        "ready_with_blocked",
    );
    assert!(result.status.success(), "ready failed: {}", result.stderr);

    let payload = extract_json_payload(&result.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).expect("valid json");

    // Should have 4 issues (issue 3 is blocked)
    assert_eq!(issues.len(), 4);
    assert!(
        !issues
            .iter()
            .map(|i| i["id"].as_str().unwrap())
            .any(|id| id == ids[2].as_str())
    ); // New Feature is blocked
}

#[test]
fn ready_cli_excludes_deferred_by_default() {
    let (workspace, ids) = setup_workspace_with_issues();

    // Defer issue 3
    let defer = run_br(
        &workspace,
        [
            "update",
            &ids[2],
            "--status",
            "deferred",
            "--defer",
            "2100-01-01T00:00:00Z",
        ],
        "defer_issue",
    );
    assert!(defer.status.success(), "defer failed: {}", defer.stderr);

    // Ready should NOT include deferred by default
    let result = run_br(
        &workspace,
        ["ready", "--limit", "0", "--json"],
        "ready_no_deferred",
    );
    assert!(result.status.success(), "ready failed: {}", result.stderr);

    let payload = extract_json_payload(&result.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).expect("valid json");

    assert_eq!(issues.len(), 4);
    assert!(
        !issues
            .iter()
            .map(|i| i["id"].as_str().unwrap())
            .any(|id| id == ids[2].as_str())
    );
}

#[test]
fn ready_cli_includes_deferred_with_flag() {
    let (workspace, ids) = setup_workspace_with_issues();

    // Defer issue 3
    let defer = run_br(
        &workspace,
        [
            "update",
            &ids[2],
            "--status",
            "deferred",
            "--defer",
            "2100-01-01T00:00:00Z",
        ],
        "defer_issue",
    );
    assert!(defer.status.success(), "defer failed: {}", defer.stderr);

    // Ready with --include-deferred should include it
    let result = run_br(
        &workspace,
        ["ready", "--limit", "0", "--include-deferred", "--json"],
        "ready_with_deferred",
    );
    assert!(result.status.success(), "ready failed: {}", result.stderr);

    let payload = extract_json_payload(&result.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).expect("valid json");

    assert_eq!(issues.len(), 5);
    assert!(
        issues
            .iter()
            .map(|i| i["id"].as_str().unwrap())
            .any(|id| id == ids[2].as_str())
    );
}

#[test]
fn ready_cli_text_output_format() {
    let (workspace, _ids) = setup_workspace_with_issues();

    let result = run_br(&workspace, ["ready"], "ready_text");
    assert!(result.status.success(), "ready failed: {}", result.stderr);

    // Should have the header
    assert!(result.stdout.contains("Ready to work"));
    // Should show priority prefix
    assert!(result.stdout.contains("[P"));
}

#[test]
fn ready_cli_empty_result_message() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let result = run_br(&workspace, ["ready"], "ready_empty");
    assert!(result.status.success(), "ready failed: {}", result.stderr);

    assert!(result.stdout.contains("No issues ready to work on"));
}

#[test]
fn ready_cli_priority_p_format() {
    let (workspace, _ids) = setup_workspace_with_issues();

    // Priority can be specified as P0, P1, etc.
    let result = run_br(
        &workspace,
        ["ready", "--priority", "P0", "--json"],
        "ready_priority_p_format",
    );
    assert!(result.status.success(), "ready failed: {}", result.stderr);

    let payload = extract_json_payload(&result.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).expect("valid json");

    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0]["priority"].as_u64().unwrap(), 0);
}
