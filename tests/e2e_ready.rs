mod common;

use common::cli::{BrWorkspace, extract_issues_array, extract_json_payload, run_br};
use serde_json::Value;
use std::fs;

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

fn issue_from_jsonl(workspace: &BrWorkspace, issue_id: &str) -> Value {
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let contents = fs::read_to_string(&jsonl_path).expect("read issues.jsonl");
    contents
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("parse issue jsonl line"))
        .find(|issue| {
            issue
                .get("id")
                .and_then(Value::as_str)
                .is_some_and(|id| id.eq(issue_id))
        })
        .expect("issue should exist in issues.jsonl")
}

fn write_single_issue_jsonl(workspace: &BrWorkspace, issue: &Value) {
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let serialized = serde_json::to_string(issue).expect("serialize issue jsonl");
    fs::write(&jsonl_path, format!("{serialized}\n")).expect("write issues.jsonl");
}

fn issue_list_contains_id(issues: &[Value], issue_id: &str) -> bool {
    issues.iter().any(|issue| {
        issue
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| id.eq(issue_id))
    })
}

fn set_issue_jsonl_string(issue: &mut Value, field: &str, value: &str) {
    let object = issue
        .as_object_mut()
        .expect("issue jsonl entry should be an object");
    object.insert(field.to_string(), Value::String(value.to_string()));
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
fn ready_cli_excludes_in_progress_issues() {
    let _log = common::test_log("ready_cli_excludes_in_progress_issues");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let open_issue = run_br(&workspace, ["create", "Open issue"], "create_open_issue");
    assert!(
        open_issue.status.success(),
        "create open failed: {}",
        open_issue.stderr
    );
    let open_id = parse_created_id(&open_issue.stdout);

    let claimed_issue = run_br(
        &workspace,
        ["create", "Claimed issue"],
        "create_claimed_issue",
    );
    assert!(
        claimed_issue.status.success(),
        "create claimed failed: {}",
        claimed_issue.stderr
    );
    let claimed_id = parse_created_id(&claimed_issue.stdout);

    let claim = run_br(
        &workspace,
        ["update", &claimed_id, "--status", "in_progress"],
        "claim_issue",
    );
    assert!(claim.status.success(), "claim failed: {}", claim.stderr);

    let result = run_br(
        &workspace,
        ["ready", "--json"],
        "ready_excludes_in_progress",
    );
    assert!(result.status.success(), "ready failed: {}", result.stderr);

    let payload = extract_json_payload(&result.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).expect("valid json");

    assert!(
        issues
            .iter()
            .map(|issue| issue["id"].as_str().unwrap())
            .any(|id| id == open_id.as_str()),
        "open issue should still appear in ready output"
    );
    assert!(
        !issues
            .iter()
            .map(|issue| issue["id"].as_str().unwrap())
            .any(|id| id == claimed_id.as_str()),
        "in-progress issue should not appear in ready output"
    );
}

#[test]
fn ready_cli_text_reports_no_ready_issues_when_work_exists() {
    let _log = common::test_log("ready_cli_text_reports_no_ready_issues_when_work_exists");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let claimed_issue = run_br(
        &workspace,
        ["create", "Claimed issue"],
        "create_claimed_issue_text",
    );
    assert!(
        claimed_issue.status.success(),
        "create claimed failed: {}",
        claimed_issue.stderr
    );
    let claimed_id = parse_created_id(&claimed_issue.stdout);

    let claim = run_br(
        &workspace,
        ["update", &claimed_id, "--status", "in_progress"],
        "claim_issue_text",
    );
    assert!(claim.status.success(), "claim failed: {}", claim.stderr);

    let result = run_br(&workspace, ["ready"], "ready_empty_text");
    assert!(result.status.success(), "ready failed: {}", result.stderr);
    assert!(
        result.stdout.contains("No ready issues"),
        "ready text should explain that work exists but none is ready: {}",
        result.stdout
    );
    assert!(
        !result.stdout.contains("No open issues"),
        "ready text should not claim there are no open issues when work is in progress: {}",
        result.stdout
    );
}

#[test]
fn ready_cli_text_explains_when_filters_hide_ready_work() {
    let _log = common::test_log("ready_cli_text_explains_when_filters_hide_ready_work");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let completed = run_br(
        &workspace,
        ["ready", "--assignee", "nobody"],
        "ready_filtered_complete",
    );
    assert!(
        completed.stdout.contains("All work complete"),
        "restrictive filters must not hide a genuinely completed workspace: {}",
        completed.stdout
    );

    let created = run_br(
        &workspace,
        ["create", "Unassigned ready work"],
        "create_ready",
    );
    assert!(
        created.status.success(),
        "create failed: {}",
        created.stderr
    );

    let result = run_br(
        &workspace,
        ["ready", "--assignee", "nobody"],
        "ready_filtered_empty",
    );
    assert!(result.status.success(), "ready failed: {}", result.stderr);
    assert!(
        result
            .stdout
            .contains("No ready issues match the requested filters"),
        "filtered empty output must identify the filter mismatch: {}",
        result.stdout
    );
    assert!(
        !result.stdout.contains("all remaining work is blocked"),
        "filtered empty output must not misdiagnose visible ready work: {}",
        result.stdout
    );
}

#[test]
fn ready_cli_text_does_not_overstate_why_active_work_is_not_ready() {
    let _log = common::test_log("ready_cli_text_does_not_overstate_why_active_work_is_not_ready");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    let created = run_br(&workspace, ["create", "Rework queue item"], "create_rework");
    let issue_id = parse_created_id(&created.stdout);
    let update = run_br(
        &workspace,
        ["update", &issue_id, "--status", "rework"],
        "mark_rework",
    );
    assert!(update.status.success(), "update failed: {}", update.stderr);

    let result = run_br(&workspace, ["ready"], "ready_non_actionable");
    assert!(result.status.success(), "ready failed: {}", result.stderr);
    assert!(
        result.stdout.contains("not currently actionable"),
        "active work outside the ready group needs a truthful generic explanation: {}",
        result.stdout
    );
    assert!(
        !result.stdout.contains("all remaining work is blocked"),
        "ready output must not invent an exhaustive cause list: {}",
        result.stdout
    );
}

#[test]
fn ready_cli_filters_by_assignee() {
    let _log = common::test_log("ready_cli_filters_by_assignee");
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
fn ready_cli_assignee_flag_without_value_uses_actor() {
    let _log = common::test_log("ready_cli_assignee_flag_without_value_uses_actor");
    let (workspace, ids) = setup_workspace_with_issues();

    let result = run_br(
        &workspace,
        ["--actor", "alice", "ready", "--assignee", "--json"],
        "ready_assignee_actor_default",
    );
    assert!(result.status.success(), "ready failed: {}", result.stderr);

    let payload = extract_json_payload(&result.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).expect("valid json");

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
#[allow(clippy::too_many_lines)]
fn ready_respects_external_dependencies() {
    let _log = common::test_log("ready_respects_external_dependencies");
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

    let issue = run_br(
        &workspace,
        ["create", "Main issue", "--priority", "0"],
        "create_main_issue",
    );
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

    let unblocked = run_br(
        &workspace,
        ["create", "Independent work", "--priority", "2"],
        "create_unblocked_issue",
    );
    assert!(unblocked.status.success(), "{}", unblocked.stderr);
    let unblocked_id = parse_created_id(&unblocked.stdout);
    let limited = run_br(
        &workspace,
        ["ready", "--sort", "priority", "--limit", "1", "--json"],
        "ready_limited_external_blocker",
    );
    assert!(limited.status.success(), "{}", limited.stderr);
    let limited_issues = extract_issues_array(&limited.stdout);
    assert_eq!(limited_issues.len(), 1);
    assert_eq!(
        limited_issues[0]["id"], unblocked_id,
        "filter the higher-priority external blocker before applying the limit"
    );

    let blocked_before = run_br(&workspace, ["blocked", "--json"], "blocked_before");
    assert!(
        blocked_before.status.success(),
        "blocked before failed: {}",
        blocked_before.stderr
    );
    let blocked_json = extract_issues_array(&blocked_before.stdout);
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
    let blocked_json = extract_issues_array(&blocked_after.stdout);
    assert!(
        !blocked_json.iter().any(|item| item["id"] == issue_id),
        "blocked list should clear after external dependency is satisfied"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn ready_imports_stale_external_jsonl_before_status_probe() {
    let _log = common::test_log("ready_imports_stale_external_jsonl_before_status_probe");
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
    fs::write(
        external.root.join(".beads/config.yaml"),
        "issue_prefix: bd\n",
    )
    .expect("write ext config");

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

    let ready_before = run_br(&workspace, ["ready", "--json"], "ready_before");
    assert!(
        ready_before.status.success(),
        "ready before failed: {}",
        ready_before.stderr
    );
    let ready_payload = extract_json_payload(&ready_before.stdout);
    let ready_json: Vec<Value> = serde_json::from_str(&ready_payload).expect("ready json");
    assert!(
        !issue_list_contains_id(&ready_json, &issue_id),
        "issue should be blocked while external provider is open in the DB"
    );

    let mut provider_jsonl = issue_from_jsonl(&external, &provider_id);
    set_issue_jsonl_string(&mut provider_jsonl, "status", "closed");
    set_issue_jsonl_string(&mut provider_jsonl, "updated_at", "2099-01-01T00:00:00Z");
    set_issue_jsonl_string(&mut provider_jsonl, "closed_at", "2099-01-01T00:00:00Z");
    set_issue_jsonl_string(&mut provider_jsonl, "close_reason", "stale JSONL closure");
    write_single_issue_jsonl(&external, &provider_jsonl);

    let ready_after = run_br(
        &workspace,
        ["ready", "--json"],
        "ready_after_stale_external_jsonl",
    );
    assert!(
        ready_after.status.success(),
        "ready after failed: {}",
        ready_after.stderr
    );
    let ready_payload = extract_json_payload(&ready_after.stdout);
    let ready_json: Vec<Value> = serde_json::from_str(&ready_payload).expect("ready json");
    assert!(
        issue_list_contains_id(&ready_json, &issue_id),
        "ready should import the external JSONL closure before probing dependency status"
    );

    let show_external = run_br(
        &external,
        ["show", &provider_id, "--json"],
        "show_external_after_ready_import",
    );
    assert!(
        show_external.status.success(),
        "external show failed: {}",
        show_external.stderr
    );
    let shown: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&show_external.stdout)).expect("show json");
    assert_eq!(shown[0]["status"].as_str(), Some("closed"));
}

#[test]
fn ready_cli_filters_unassigned_only() {
    let _log = common::test_log("ready_cli_filters_unassigned_only");
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
    let _log = common::test_log("ready_cli_filters_by_type");
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
    let _log = common::test_log("ready_cli_filters_by_multiple_types");
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
    let _log = common::test_log("ready_cli_filters_by_priority");
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
    let _log = common::test_log("ready_cli_filters_by_multiple_priorities");
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
    let _log = common::test_log("ready_cli_filters_by_label_and");
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
    let _log = common::test_log("ready_cli_filters_by_multiple_labels_and");
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
    let _log = common::test_log("ready_cli_filters_by_label_or");
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
    let _log = common::test_log("ready_cli_respects_limit");
    let (workspace, _ids) = setup_workspace_with_issues();

    for sort in ["hybrid", "priority", "oldest"] {
        let all = run_br(
            &workspace,
            ["ready", "--sort", sort, "--limit", "0", "--json"],
            &format!("ready_unlimited_{sort}"),
        );
        assert!(all.status.success(), "{}", all.stderr);
        let all_issues = extract_issues_array(&all.stdout);
        assert_eq!(all_issues.len(), 5);

        // Cover a full high-priority bucket, crossing into the lower bucket,
        // and limits that cover the entire result. Compare every output field,
        // including hydrated labels, rather than just the number of rows.
        for limit in [1_usize, 2, 3, 5, 10] {
            let limit_arg = limit.to_string();
            let result = run_br(
                &workspace,
                ["ready", "--sort", sort, "--limit", &limit_arg, "--json"],
                &format!("ready_{sort}_limit_{limit}"),
            );
            assert!(result.status.success(), "ready failed: {}", result.stderr);
            let issues = extract_issues_array(&result.stdout);
            assert_eq!(issues, all_issues[..limit.min(all_issues.len())]);
        }
    }
}

#[test]
fn ready_cli_limit_zero_returns_all() {
    let _log = common::test_log("ready_cli_limit_zero_returns_all");
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
    let _log = common::test_log("ready_cli_sort_priority");
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
    let _log = common::test_log("ready_cli_combined_filters");
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
    let _log = common::test_log("ready_cli_excludes_blocked_issues");
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
    let _log = common::test_log("ready_cli_excludes_deferred_by_default");
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
    let _log = common::test_log("ready_cli_includes_deferred_with_flag");
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
    let _log = common::test_log("ready_cli_text_output_format");
    let (workspace, _ids) = setup_workspace_with_issues();

    let result = run_br(&workspace, ["ready"], "ready_text");
    assert!(result.status.success(), "ready failed: {}", result.stderr);

    // Should have the header (matches bd format)
    assert!(result.stdout.contains("Ready work"));
    // Should show priority badge (matches bd format: [● P2])
    assert!(result.stdout.contains("[●"));
}

#[test]
fn ready_cli_empty_result_message() {
    let _log = common::test_log("ready_cli_empty_result_message");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let result = run_br(&workspace, ["ready"], "ready_empty");
    assert!(result.status.success(), "ready failed: {}", result.stderr);

    // Empty workspace shows completion message
    assert!(
        result.stdout.contains("No open issues")
            || result.stdout.contains("no issues to work on")
            || result.stdout.contains("All work complete"),
        "expected empty-ready message, got: {}",
        result.stdout
    );
}

#[test]
fn ready_cli_priority_p_format() {
    let _log = common::test_log("ready_cli_priority_p_format");
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

// ============================================================================
// beads_rust-jsgu: invariant-based e2e ordering tests (added 2026-05-09)
// Pairs with the unit-level rewrite in tests/storage_ready.rs::ready_sort_*
// (those exercise the storage API directly; these exercise the full CLI).
// ============================================================================

/// jsgu AC: full CLI round-trip for the priority-ordering contract. Creates
/// issues at P0/P1/P2/P3 in REVERSE priority order, runs `br ready --json`,
/// asserts hybrid ordering invariant (high-tier P0/P1 before low-tier P2+).
#[test]
fn e2e_ready_with_mixed_priority_high_tier_first() {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    eprintln!("[jsgu TEST] e2e_ready_with_mixed_priority_high_tier_first");

    // Create issues in REVERSE priority order to guard against accidental
    // creation-order pass-through.
    for (title, prio) in [
        ("Low", "3"),
        ("Critical", "0"),
        ("Medium", "2"),
        ("High", "1"),
    ] {
        let create = run_br(
            &workspace,
            ["create", title, "-t", "task", "-p", prio, "--no-auto-flush"],
            &format!("create_{title}"),
        );
        assert!(
            create.status.success(),
            "create {title} failed: {}",
            create.stderr
        );
    }

    let out = run_br(&workspace, ["ready", "--json"], "ready");
    assert!(out.status.success(), "br ready failed: {}", out.stderr);
    let issues: Vec<Value> =
        serde_json::from_str(out.stdout.trim()).expect("ready json must parse");

    eprintln!(
        "  ready order: {:?}",
        issues
            .iter()
            .map(|i| (
                i["title"].as_str().unwrap_or(""),
                i["priority"].as_u64().unwrap_or(99)
            ))
            .collect::<Vec<_>>()
    );

    assert_eq!(issues.len(), 4, "expected 4 ready issues");

    // Hybrid invariant: no high-tier (priority ≤ 1) issue appears AFTER any
    // low-tier (priority > 1) issue.
    let mut low_seen = false;
    for issue in &issues {
        let prio = issue["priority"].as_u64().unwrap_or(99);
        let title = issue["title"].as_str().unwrap_or("");
        if prio <= 1 {
            assert!(
                !low_seen,
                "P{prio} issue '{title}' appears after low-tier; full order: {:?}",
                issues
                    .iter()
                    .map(|i| (
                        i["title"].as_str().unwrap_or(""),
                        i["priority"].as_u64().unwrap_or(99)
                    ))
                    .collect::<Vec<_>>()
            );
        } else {
            low_seen = true;
        }
    }

    eprintln!("  [PASS] hybrid ordering invariant holds via CLI");
}

/// jsgu AC: invariant — `br ready --json` MUST NEVER return duplicate IDs,
/// regardless of how many fixtures share priority/created_at.
#[test]
fn e2e_ready_returns_no_duplicate_ids() {
    let workspace = BrWorkspace::new();
    run_br(&workspace, ["init"], "init");

    eprintln!("[jsgu TEST] e2e_ready_returns_no_duplicate_ids");

    // Create 6 issues all at the same priority — id-tiebreak path is exercised
    for i in 0..6 {
        let title = format!("issue {i}");
        let create = run_br(
            &workspace,
            ["create", &title, "-t", "task", "-p", "2", "--no-auto-flush"],
            &format!("c_{i}"),
        );
        assert!(create.status.success(), "create failed");
    }

    let out = run_br(&workspace, ["ready", "--json"], "ready");
    assert!(out.status.success(), "br ready failed");
    let issues: Vec<Value> = serde_json::from_str(out.stdout.trim()).expect("must parse");
    assert_eq!(issues.len(), 6, "all 6 should be ready");

    let mut seen = std::collections::HashSet::new();
    for issue in &issues {
        let id = issue["id"].as_str().expect("id field present");
        assert!(
            seen.insert(id.to_string()),
            "duplicate ID {id} in ready output; got: {:?}",
            issues.iter().map(|i| i["id"].as_str()).collect::<Vec<_>>()
        );
    }

    eprintln!("  [PASS] all 6 IDs unique");
}

/// #354: write a `.beads/policy.yaml` ready group and assert `br ready --json`
/// honors it end-to-end (config surface → CLI → query → JSON parity).
fn write_policy(workspace: &BrWorkspace, yaml: &str) {
    let policy_path = workspace.root.join(".beads").join("policy.yaml");
    fs::write(&policy_path, yaml).expect("write policy.yaml");
}

#[test]
fn ready_default_group_is_open_only_e2e() {
    let _log = common::test_log("ready_default_group_is_open_only_e2e");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let open = run_br(&workspace, ["create", "Open work", "-t", "task"], "c_open");
    let open_id = parse_created_id(&open.stdout);
    let rework = run_br(
        &workspace,
        ["create", "Rework work", "-t", "task"],
        "c_rework",
    );
    let rework_id = parse_created_id(&rework.stdout);
    let set = run_br(
        &workspace,
        ["update", &rework_id, "--status", "rework"],
        "to_rework",
    );
    assert!(
        set.status.success(),
        "update to rework failed: {}",
        set.stderr
    );

    // No policy configured → default ready group is [open].
    let result = run_br(&workspace, ["ready", "--json"], "ready_default");
    assert!(result.status.success(), "ready failed: {}", result.stderr);
    let payload = extract_json_payload(&result.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).expect("valid json");
    assert!(
        issue_list_contains_id(&issues, &open_id),
        "open issue must be ready by default"
    );
    assert!(
        !issue_list_contains_id(&issues, &rework_id),
        "rework issue must NOT be ready under the default [open] group"
    );
}

#[test]
fn ready_configured_group_surfaces_rework_e2e() {
    let _log = common::test_log("ready_configured_group_surfaces_rework_e2e");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let open = run_br(&workspace, ["create", "Open work", "-t", "task"], "c_open");
    let open_id = parse_created_id(&open.stdout);
    let rework = run_br(
        &workspace,
        ["create", "Rework work", "-t", "task"],
        "c_rework",
    );
    let rework_id = parse_created_id(&rework.stdout);
    run_br(
        &workspace,
        ["update", &rework_id, "--status", "rework"],
        "to_rework",
    );

    write_policy(
        &workspace,
        "workflow:\n  status_groups:\n    ready: [open, rework]\n",
    );

    let result = run_br(&workspace, ["ready", "--json"], "ready_configured");
    assert!(result.status.success(), "ready failed: {}", result.stderr);
    let payload = extract_json_payload(&result.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).expect("valid json");
    assert!(
        issue_list_contains_id(&issues, &open_id),
        "open issue must still be ready"
    );
    assert!(
        issue_list_contains_id(&issues, &rework_id),
        "rework issue must surface under the configured [open, rework] group"
    );
    // Status parity: the rework issue keeps its real status in JSON output.
    let rework_issue = issues
        .iter()
        .find(|i| i["id"].as_str() == Some(rework_id.as_str()))
        .expect("rework issue present");
    assert_eq!(
        rework_issue["status"].as_str(),
        Some("rework"),
        "returned issue must preserve its real status"
    );
}

#[test]
fn ready_custom_only_group_surfaces_rework_e2e() {
    let _log = common::test_log("ready_custom_only_group_surfaces_rework_e2e");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let rework = run_br(
        &workspace,
        ["create", "Only rework candidate", "-t", "task"],
        "c_rework_only",
    );
    let rework_id = parse_created_id(&rework.stdout);
    let update = run_br(
        &workspace,
        ["update", &rework_id, "--status", "rework"],
        "to_rework_only",
    );
    assert!(
        update.status.success(),
        "update to rework failed: {}",
        update.stderr
    );
    write_policy(
        &workspace,
        "workflow:\n  status_groups:\n    ready: [rework]\n",
    );

    let result = run_br(&workspace, ["ready", "--json"], "ready_custom_only");
    assert!(result.status.success(), "ready failed: {}", result.stderr);
    let payload = extract_json_payload(&result.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).expect("valid json");
    assert_eq!(issues.len(), 1, "custom-only ready group lost its sole row");
    assert_eq!(issues[0]["id"], rework_id);
    assert_eq!(issues[0]["status"], "rework");
}

#[test]
fn ready_strict_rejects_out_of_vocab_group_e2e() {
    let _log = common::test_log("ready_strict_rejects_out_of_vocab_group_e2e");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    run_br(&workspace, ["create", "Open work", "-t", "task"], "c_open");

    // strict statuses do NOT include `rework`, but the ready group lists it.
    write_policy(
        &workspace,
        "workflow:\n  strict: true\n  statuses: [open, in_progress, closed]\n  status_groups:\n    ready: [open, rework]\n",
    );

    let result = run_br(&workspace, ["ready", "--json"], "ready_strict_reject");
    assert!(
        !result.status.success(),
        "strict out-of-vocab ready group must be rejected; stdout: {} stderr: {}",
        result.stdout,
        result.stderr
    );
    // In --json mode the structured error envelope is emitted on stdout; in
    // human mode it goes to stderr. Accept either so the assertion is robust.
    let combined = format!("{}{}", result.stdout, result.stderr);
    assert!(
        combined.contains("rework") && combined.contains("workflow.status_groups.ready"),
        "error must name the offending status and config key; stdout: {} stderr: {}",
        result.stdout,
        result.stderr
    );
}

#[test]
fn ready_cli_text_truncation_emits_showing_note() {
    // #356: when `br ready --limit N` hides ready rows, the text surface must
    // print a "Showing N of M" note (on stderr) instead of silently truncating
    // — mirroring `br list` and the MCP ready surface (issue #91).
    let _log = common::test_log("ready_cli_text_truncation_emits_showing_note");
    let (workspace, _ids) = setup_workspace_with_issues(); // 5 ready issues

    let result = run_br(&workspace, ["ready", "--limit", "2"], "ready_limit_note");
    assert!(result.status.success(), "ready failed: {}", result.stderr);

    // Exactly 2 rows shown on stdout (lines starting with "1." and "2.").
    let shown = result
        .stdout
        .lines()
        .filter(|l| l.trim_start().starts_with("1. ") || l.trim_start().starts_with("2. "))
        .count();
    assert_eq!(shown, 2, "expected 2 ready rows; stdout: {}", result.stdout);

    assert!(
        result
            .stderr
            .contains("Showing 2 of 5 ready issues. Use --limit 0 for all results."),
        "expected truncation note on stderr; stderr: {}",
        result.stderr
    );
}

#[test]
fn ready_cli_no_note_when_limit_covers_all() {
    // No "Showing N of M" note when the limit does not actually truncate.
    let _log = common::test_log("ready_cli_no_note_when_limit_covers_all");
    let (workspace, _ids) = setup_workspace_with_issues(); // 5 ready issues

    let result = run_br(
        &workspace,
        ["ready", "--limit", "10"],
        "ready_limit_no_note",
    );
    assert!(result.status.success(), "ready failed: {}", result.stderr);
    assert!(
        !result.stderr.contains("Showing") && !result.stderr.contains("Use --limit 0"),
        "no truncation note expected when limit >= total; stderr: {}",
        result.stderr
    );
}

#[test]
fn ready_cli_quiet_suppresses_truncation_note() {
    // `--quiet` must suppress the truncation note, matching `br list`.
    let _log = common::test_log("ready_cli_quiet_suppresses_truncation_note");
    let (workspace, _ids) = setup_workspace_with_issues(); // 5 ready issues

    let result = run_br(
        &workspace,
        ["--quiet", "ready", "--limit", "2"],
        "ready_limit_quiet",
    );
    assert!(result.status.success(), "ready failed: {}", result.stderr);
    assert!(
        !result.stderr.contains("Showing 2 of"),
        "quiet ready should not emit truncation note; stderr: {}",
        result.stderr
    );
}

/// GH #595: status recovery must recover ready work without interpreting a
/// satisfied dependency graph as permission to remove a manual block.
mod dependency_unblock {
    use super::*;
    use common::cli::{BrRun, run_br_with_env};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn command(workspace: &BrWorkspace, args: &[&str], label: &str) -> BrRun {
        let mut argv = vec!["--actor", "repairer"];
        argv.extend_from_slice(args);
        run_br(workspace, argv, label)
    }

    fn success(workspace: &BrWorkspace, args: &[&str], label: &str) -> BrRun {
        let result = command(workspace, args, label);
        assert!(result.status.success(), "{label}: {result:?}");
        result
    }

    fn json(workspace: &BrWorkspace, args: &[&str], label: &str) -> Value {
        let result = success(workspace, args, label);
        serde_json::from_str(&extract_json_payload(&result.stdout))
            .unwrap_or_else(|error| panic!("{label}: {error}: {}", result.stdout))
    }

    fn attributed_command(
        workspace: &BrWorkspace,
        args: &[&str],
        attribution: &[(&str, &str)],
        label: &str,
    ) -> BrRun {
        let mut argv = vec!["--actor", "repairer"];
        argv.extend_from_slice(args);
        run_br_with_env(workspace, argv, attribution.iter().copied(), label)
    }

    fn attributed_json(
        workspace: &BrWorkspace,
        args: &[&str],
        attribution: &[(&str, &str)],
        label: &str,
    ) -> Value {
        let result = attributed_command(workspace, args, attribution, label);
        assert!(result.status.success(), "{label}: {result:?}");
        serde_json::from_str(&extract_json_payload(&result.stdout))
            .unwrap_or_else(|error| panic!("{label}: {error}: {}", result.stdout))
    }

    fn workspace() -> BrWorkspace {
        let workspace = BrWorkspace::new();
        success(&workspace, &["init"], "unblock_init");
        workspace
    }

    fn create(workspace: &BrWorkspace, title: &str) -> String {
        let result = success(workspace, &["create", title], "unblock_create");
        let id = parse_created_id(&result.stdout);
        assert!(!id.is_empty(), "created ID missing: {result:?}");
        id
    }

    fn dependency(workspace: &BrWorkspace, dependent: &str, blocker: &str) {
        success(
            workspace,
            &["dep", "add", dependent, blocker],
            "unblock_add_dependency",
        );
    }

    fn block(workspace: &BrWorkspace, id: &str, dependency_only: bool) {
        let mut args = vec!["update", id, "--status", "blocked"];
        if dependency_only {
            args.push("--blocked-by-dependencies");
        }
        success(workspace, &args, "unblock_set_blocked");
    }

    fn close(workspace: &BrWorkspace, id: &str) {
        success(
            workspace,
            &["close", id, "--reason", "verified prerequisite completed"],
            "unblock_close",
        );
    }

    fn pair(workspace: &BrWorkspace, dependency_only: bool) -> (String, String) {
        let blocker = create(workspace, "prerequisite A");
        let dependent = create(workspace, "dependent B");
        dependency(workspace, &dependent, &blocker);
        block(workspace, &dependent, dependency_only);
        (blocker, dependent)
    }

    fn show(workspace: &BrWorkspace, id: &str) -> Value {
        json(workspace, &["show", id, "--json"], "unblock_show")[0].clone()
    }

    fn ready_contains(workspace: &BrWorkspace, id: &str) -> bool {
        let result = json(
            workspace,
            &["ready", "--limit", "0", "--json"],
            "unblock_ready",
        );
        issue_list_contains_id(result.as_array().expect("ready array"), id)
    }

    fn preview(workspace: &BrWorkspace, options: &[&str]) -> Value {
        let mut args = vec!["doctor", "unblock", "--json"];
        args.extend_from_slice(options);
        json(workspace, &args, "unblock_preview")
    }

    fn apply(workspace: &BrWorkspace, options: &[&str], token: &str) -> BrRun {
        let mut args = vec!["doctor", "unblock", "--apply", "--expect", token, "--json"];
        args.extend_from_slice(options);
        command(workspace, &args, "unblock_apply")
    }

    fn repair(workspace: &BrWorkspace, options: &[&str]) -> Value {
        let plan = preview(workspace, options);
        let token = plan["plan_token"].as_str().expect("review token");
        let applied = apply(workspace, options, token);
        assert!(applied.status.success(), "repair: {applied:?}");
        let result: Value =
            serde_json::from_str(&extract_json_payload(&applied.stdout)).expect("repair report");
        assert_eq!(result["plan_token"], token);
        result
    }

    fn candidate<'a>(plan: &'a Value, id: &str) -> Option<&'a Value> {
        plan["candidates"]
            .as_array()
            .expect("candidate array")
            .iter()
            .find(|candidate| candidate["id"].as_str() == Some(id))
    }

    fn eligible(plan: &Value, id: &str) -> bool {
        candidate(plan, id).is_some_and(|candidate| candidate["eligible"] == true)
    }

    fn persisted_files(workspace: &BrWorkspace) -> BTreeMap<PathBuf, Vec<u8>> {
        let beads = workspace.root.join(".beads");
        walkdir::WalkDir::new(&beads)
            .into_iter()
            .map(|entry| entry.expect("read tracker inventory"))
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| {
                let path = entry.path();
                let mut bytes = fs::read(path).expect("read tracker file");
                // A read-only WAL opener may register its snapshot in this
                // read-mark range. Every other persisted byte is witnessed,
                // including audit tables, JSONL, cache and history files.
                if path.to_string_lossy().ends_with("-shm") && bytes.len() >= 120 {
                    bytes[100..120].fill(0);
                }
                (path.strip_prefix(&beads).unwrap().to_path_buf(), bytes)
            })
            .collect()
    }

    #[test]
    fn unblock_595_reporter_reproduction_requires_reviewed_authorization() {
        let workspace = workspace();
        let (blocker, dependent) = pair(&workspace, false);
        close(&workspace, &blocker);
        assert_eq!(show(&workspace, &dependent)["status"], "blocked");
        assert!(!ready_contains(&workspace, &dependent));

        let before = persisted_files(&workspace);
        let plan = preview(&workspace, &[]);
        assert_eq!(
            persisted_files(&workspace),
            before,
            "preview wrote tracker data"
        );
        assert_eq!(preview(&workspace, &["--dry-run"]), plan);
        assert_eq!(
            json(
                &workspace,
                &["doctor", "--dry-run", "unblock", "--json"],
                "unblock_parent_dry_run",
            ),
            plan
        );
        assert_eq!(persisted_files(&workspace), before);
        let stale = candidate(&plan, &dependent).expect("graph-stale dependent is reported");
        assert_eq!(
            stale["eligible"], false,
            "a manual status change is not proof"
        );
        assert!(
            stale["reason"]
                .as_str()
                .is_some_and(|reason| !reason.is_empty())
        );
        assert_eq!(plan["planned_ids"], serde_json::json!([]));
        let no_permission = apply(&workspace, &[], plan["plan_token"].as_str().unwrap());
        assert!(
            no_permission.status.success(),
            "safe empty apply: {no_permission:?}"
        );
        assert_eq!(show(&workspace, &dependent)["status"], "blocked");

        let result = repair(&workspace, &["--authorize", &dependent]);
        assert_eq!(result["changed"], serde_json::json!([dependent]));
        let repaired = show(&workspace, &dependent);
        assert_eq!(repaired["status"], "open");
        assert!(ready_contains(&workspace, &dependent));
        assert!(
            repaired["comments"]
                .as_array()
                .unwrap()
                .iter()
                .any(|comment| {
                    comment["author"] == "repairer"
                        && comment["text"]
                            .as_str()
                            .is_some_and(|text| text.contains(&blocker))
                }),
            "repair must explain the satisfied blocker in an attributed audit comment: {repaired}"
        );
        let exported = issue_from_jsonl(&workspace, &dependent);
        assert_eq!(exported["status"], "open");
        assert!(
            exported["dependencies"]
                .as_array()
                .unwrap()
                .iter()
                .any(|edge| { edge["depends_on_id"] == blocker }),
            "repair must preserve satisfied dependency history"
        );
        let no_op = repair(&workspace, &[]);
        assert_eq!(no_op["changed"], serde_json::json!([]));
        assert_eq!(
            show(&workspace, &dependent),
            repaired,
            "retry added duplicate audit data"
        );
    }

    #[test]
    fn unblock_595_dependency_provenance_waits_for_every_blocker() {
        let workspace = workspace();
        let blocker_a = create(&workspace, "first prerequisite");
        let blocker_c = create(&workspace, "second prerequisite");
        let dependent = create(&workspace, "requires both prerequisites");
        dependency(&workspace, &dependent, &blocker_a);
        dependency(&workspace, &dependent, &blocker_c);
        block(&workspace, &dependent, true);
        close(&workspace, &blocker_a);

        assert!(!eligible(&preview(&workspace, &[]), &dependent));
        let forced = repair(&workspace, &["--authorize", &dependent]);
        assert_eq!(forced["changed"], serde_json::json!([]));
        assert_eq!(show(&workspace, &dependent)["status"], "blocked");
        close(&workspace, &blocker_c);
        assert!(eligible(&preview(&workspace, &[]), &dependent));
        let result = repair(&workspace, &[]);
        assert_eq!(result["changed"], serde_json::json!([dependent]));
        assert!(ready_contains(&workspace, &dependent));
    }

    #[test]
    fn unblock_595_chained_dependents_wait_for_completion_not_reopening() {
        let workspace = workspace();
        let (first, middle) = pair(&workspace, true);
        let last = create(&workspace, "requires middle to complete");
        dependency(&workspace, &last, &middle);
        block(&workspace, &last, true);
        close(&workspace, &first);

        let result = repair(&workspace, &[]);
        assert_eq!(result["changed"], serde_json::json!([middle]));
        assert!(ready_contains(&workspace, &middle));
        assert_eq!(show(&workspace, &last)["status"], "blocked");
        assert!(!ready_contains(&workspace, &last));
        close(&workspace, &middle);
        assert_eq!(
            repair(&workspace, &[])["changed"],
            serde_json::json!([last])
        );
        assert!(ready_contains(&workspace, &last));
    }

    #[test]
    fn unblock_595_manual_and_external_holds_are_preserved() {
        let workspace = workspace();
        let (blocker, manual) = pair(&workspace, false);
        success(
            &workspace,
            &[
                "update",
                &manual,
                "--notes",
                "Waiting for the customer's approval",
            ],
            "manual_external_hold",
        );
        let unlinked = create(&workspace, "Waiting for an off-tracker deployment");
        block(&workspace, &unlinked, false);
        let external = create(&workspace, "Requires unavailable external provider");
        dependency(&workspace, &external, &blocker);
        dependency(&workspace, &external, "external:unavailable:approval");
        block(&workspace, &external, false);
        close(&workspace, &blocker);

        let report = repair(&workspace, &[]);
        assert_eq!(report["changed"], serde_json::json!([]));
        let authorized = repair(&workspace, &["--authorize", &external]);
        assert_eq!(authorized["changed"], serde_json::json!([]));
        for id in [&manual, &unlinked, &external] {
            assert_eq!(show(&workspace, id)["status"], "blocked");
            assert!(!ready_contains(&workspace, id));
        }
        assert_eq!(
            show(&workspace, &manual)["notes"],
            "Waiting for the customer's approval"
        );
    }

    #[test]
    fn unblock_595_parent_and_epic_gates_survive_authorization() {
        let workspace = workspace();
        let (closed, child) = pair(&workspace, false);
        let parent = create(&workspace, "parent with a live prerequisite");
        let parent_blocker = create(&workspace, "parent prerequisite");
        dependency(&workspace, &parent, &parent_blocker);
        success(
            &workspace,
            &["update", &child, "--parent", &parent],
            "set_parent",
        );
        close(&workspace, &closed);
        assert_eq!(
            repair(&workspace, &["--authorize", &child])["changed"],
            serde_json::json!([])
        );
        assert!(!ready_contains(&workspace, &child));
        close(&workspace, &parent_blocker);
        assert_eq!(
            repair(&workspace, &["--authorize", &child])["changed"],
            serde_json::json!([child])
        );
        assert!(ready_contains(&workspace, &child));

        let epic = create(&workspace, "epic waiting for its child");
        success(&workspace, &["update", &epic, "--type", "epic"], "set_epic");
        dependency(&workspace, &epic, &closed);
        block(&workspace, &epic, false);
        success(
            &workspace,
            &["update", &child, "--parent", &epic],
            "set_epic_parent",
        );
        assert_eq!(
            repair(&workspace, &["--authorize", &epic])["changed"],
            serde_json::json!([])
        );
        assert!(!ready_contains(&workspace, &epic));
        close(&workspace, &child);
        assert_eq!(
            repair(&workspace, &["--authorize", &epic])["changed"],
            serde_json::json!([epic])
        );
        assert!(ready_contains(&workspace, &epic));
    }

    #[test]
    fn unblock_595_custom_workflow_target_and_transition_are_enforced() {
        let workspace = workspace();
        let (blocker, dependent) = pair(&workspace, true);
        close(&workspace, &blocker);
        write_policy(
            &workspace,
            "workflow:\n  strict: true\n  statuses: [open, blocked, rework, closed]\n  transitions:\n    blocked: [rework]\n  status_groups:\n    ready: [open, rework]\n  required_fields:\n    rework: [transition_comment]\n",
        );
        assert!(!eligible(
            &preview(&workspace, &["--to-status", "open"]),
            &dependent
        ));
        let denied = repair(
            &workspace,
            &["--to-status", "open", "--authorize", &dependent],
        );
        assert_eq!(denied["changed"], serde_json::json!([]));
        assert_eq!(show(&workspace, &dependent)["status"], "blocked");

        write_policy(
            &workspace,
            "workflow:\n  strict: true\n  statuses: [open, blocked, rework, closed]\n  transitions:\n    blocked: [rework]\n  status_groups:\n    ready: [rework]\n  required_fields:\n    rework: [transition_comment]\n",
        );
        let non_ready_target = command(
            &workspace,
            &["doctor", "unblock", "--to-status", "open", "--json"],
            "unblock_non_ready_target",
        );
        assert!(
            !non_ready_target.status.success(),
            "repair target must belong to ready group"
        );
        assert_eq!(show(&workspace, &dependent)["status"], "blocked");
        let result = repair(&workspace, &["--to-status", "rework"]);
        assert_eq!(result["changed"], serde_json::json!([dependent]));
        assert_eq!(show(&workspace, &dependent)["status"], "rework");
        assert!(ready_contains(&workspace, &dependent));
    }

    #[test]
    fn unblock_595_workflow_required_fields_and_defer_time_are_not_bypassed() {
        let workspace = workspace();
        let (blocker, dependent) = pair(&workspace, false);
        close(&workspace, &blocker);
        write_policy(
            &workspace,
            "workflow:\n  required_fields:\n    open: [acceptance_criteria_present]\n",
        );
        let options = ["--authorize", dependent.as_str()];
        assert!(!eligible(&preview(&workspace, &options), &dependent));
        assert_eq!(
            repair(&workspace, &options)["changed"],
            serde_json::json!([])
        );
        success(
            &workspace,
            &[
                "update",
                &dependent,
                "--acceptance-criteria",
                "- [ ] verified delivery",
                "--defer",
                "2999-01-01T00:00:00Z",
            ],
            "unblock_required_fields_and_defer",
        );
        assert!(!eligible(&preview(&workspace, &options), &dependent));
        assert_eq!(
            repair(&workspace, &options)["changed"],
            serde_json::json!([])
        );
        assert_eq!(show(&workspace, &dependent)["status"], "blocked");
        success(
            &workspace,
            &["update", &dependent, "--defer", ""],
            "clear_future_defer",
        );
        assert_eq!(
            repair(&workspace, &options)["changed"],
            serde_json::json!([dependent])
        );
        assert!(ready_contains(&workspace, &dependent));
    }

    #[test]
    fn unblock_595_authorization_does_not_take_someone_elses_ownership() {
        let workspace = workspace();
        let (blocker, dependent) = pair(&workspace, false);
        success(
            &workspace,
            &[
                "update",
                &dependent,
                "--assignee",
                "alice",
                "--owner",
                "alice",
            ],
            "assign_to_alice",
        );
        close(&workspace, &blocker);
        let before = show(&workspace, &dependent);
        let report = repair(&workspace, &["--authorize", &dependent]);
        assert_eq!(report["changed"], serde_json::json!([]));
        assert_eq!(show(&workspace, &dependent), before);

        let plan = run_br(
            &workspace,
            [
                "--actor",
                "alice",
                "doctor",
                "unblock",
                "--authorize",
                &dependent,
                "--json",
            ],
            "owner_unblock_preview",
        );
        assert!(plan.status.success(), "owner preview: {plan:?}");
        let plan: Value = serde_json::from_str(&extract_json_payload(&plan.stdout)).unwrap();
        assert!(eligible(&plan, &dependent));
        let applied = run_br(
            &workspace,
            [
                "--actor",
                "alice",
                "doctor",
                "unblock",
                "--authorize",
                &dependent,
                "--apply",
                "--expect",
                plan["plan_token"].as_str().unwrap(),
                "--json",
            ],
            "owner_unblock_apply",
        );
        assert!(applied.status.success(), "owner repair: {applied:?}");
        let after = show(&workspace, &dependent);
        assert_eq!(after["status"], "open");
        assert_eq!(after["assignee"], "alice");
        assert_eq!(after["owner"], "alice");
        assert!(ready_contains(&workspace, &dependent));
    }

    #[test]
    fn unblock_595_scoped_capacity_enforces_and_records_reviewed_attribution() {
        for scope in ["harness", "session"] {
            let workspace = workspace();
            let (blocker, dependent) = pair(&workspace, true);
            close(&workspace, &blocker);
            let occupied = create(&workspace, "already admitted work");
            success(
                &workspace,
                &["update", &occupied, "--status", "in_progress"],
                "leave_ready_before_scoped_admission",
            );
            write_policy(
                &workspace,
                &format!(
                    "workflow:\n  statuses: [open, in_progress, blocked, closed]\n  capacity:\n    scopes:\n      {scope}:\n        statuses:\n          open:\n            hard: 1\n"
                ),
            );
            let attribution = [
                (
                    "BR_HARNESS",
                    if scope == "harness" {
                        "repair-scope"
                    } else {
                        ""
                    },
                ),
                (
                    "BR_SESSION",
                    if scope == "session" {
                        "repair-scope"
                    } else {
                        ""
                    },
                ),
            ];
            let admission = attributed_command(
                &workspace,
                &["update", &occupied, "--status", "open"],
                &attribution,
                "occupy_scoped_ready_capacity",
            );
            assert!(admission.status.success(), "{scope}: {admission:?}");
            let args = [
                "doctor",
                "unblock",
                "--authorize",
                &dependent,
                "--agent-name",
                "recovery-worker",
                "--model",
                "reviewed-model",
                "--json",
            ];
            let before = persisted_files(&workspace);
            let full = attributed_json(&workspace, &args, &attribution, "scoped_full_preview");
            assert_eq!(persisted_files(&workspace), before);
            let skipped = candidate(&full, &dependent).expect("blocked candidate");
            assert_eq!(skipped["eligible"], false, "{scope}: {full}");
            assert_eq!(skipped["reason"], "capacity_policy", "{scope}: {full}");
            let mut apply_args = args.to_vec();
            apply_args.extend_from_slice(&[
                "--apply",
                "--expect",
                full["plan_token"].as_str().unwrap(),
            ]);
            let no_change =
                attributed_json(&workspace, &apply_args, &attribution, "scoped_full_apply");
            assert_eq!(no_change["changed"], serde_json::json!([]));
            assert_eq!(show(&workspace, &dependent)["status"], "blocked");

            close(&workspace, &occupied);
            let allowed =
                attributed_json(&workspace, &args, &attribution, "scoped_available_preview");
            assert!(eligible(&allowed, &dependent), "{scope}: {allowed}");
            let mut apply_args = args.to_vec();
            apply_args.extend_from_slice(&[
                "--apply",
                "--expect",
                allowed["plan_token"].as_str().unwrap(),
            ]);
            for changed in ["scope", "agent", "model"] {
                let mut changed_args = apply_args.clone();
                let mut changed_attribution = attribution;
                match changed {
                    "scope" => {
                        changed_attribution[usize::from(scope == "session")].1 = "different-scope"
                    }
                    "agent" => changed_args[5] = "different-worker",
                    "model" => changed_args[7] = "different-model",
                    _ => unreachable!(),
                }
                let before = persisted_files(&workspace);
                let rejected = attributed_command(
                    &workspace,
                    &changed_args,
                    &changed_attribution,
                    "scoped_changed_attribution",
                );
                assert!(
                    !rejected.status.success(),
                    "{scope}/{changed} must invalidate the reviewed token: {rejected:?}"
                );
                assert_eq!(persisted_files(&workspace), before);
            }
            let applied = attributed_json(
                &workspace,
                &apply_args,
                &attribution,
                "scoped_allowed_apply",
            );
            assert_eq!(applied["changed"], serde_json::json!([dependent]));
            assert!(ready_contains(&workspace, &dependent));
            let stats = json(&workspace, &["stats", "--json"], "scoped_occupancy_stats");
            let occupancy = stats["capacity"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| {
                    row["scope"] == scope
                        && row["scope_key"] == "repair-scope"
                        && row["name"] == "open"
                })
                .unwrap_or_else(|| panic!("{scope} occupancy missing: {stats}"));
            assert_eq!(occupancy["counted"], 1, "{scope}: {occupancy}");

            let storage =
                beads_rust::storage::SqliteStorage::open(&workspace.root.join(".beads/beads.db"))
                    .unwrap();
            let events = storage.get_events(&dependent, 0).unwrap();
            let event = events
                .iter()
                .find(|event| {
                    event.event_type == beads_rust::model::EventType::StatusChanged
                        && event.old_value.as_deref() == Some("blocked")
                        && event.new_value.as_deref() == Some("open")
                })
                .expect("repair status event");
            assert_eq!(event.actor, "repairer");
            assert_eq!(event.agent_name.as_deref(), Some("recovery-worker"));
            assert_eq!(event.model.as_deref(), Some("reviewed-model"));
            assert_eq!(
                event.harness.as_deref(),
                (scope == "harness").then_some("repair-scope")
            );
        }
    }

    #[test]
    fn unblock_595_capacity_warnings_are_preserved_in_preview_and_apply() {
        let workspace = workspace();
        let (blocker, dependent) = pair(&workspace, true);
        close(&workspace, &blocker);
        write_policy(
            &workspace,
            "workflow:\n  statuses: [open, blocked, closed]\n  capacity:\n    scopes:\n      harness:\n        statuses:\n          open:\n            soft: 1\n",
        );
        let args = ["doctor", "unblock", "--harness", "warning-scope", "--json"];
        let before = persisted_files(&workspace);
        let plan = json(&workspace, &args, "capacity_warning_preview");
        assert_eq!(persisted_files(&workspace), before);
        assert!(eligible(&plan, &dependent));
        let warnings = plan["capacity_warnings"]
            .as_array()
            .expect("preview warnings");
        assert_eq!(warnings.len(), 1, "{plan}");
        assert_eq!(warnings[0]["issue_id"], dependent);
        assert_eq!(warnings[0]["scope"], "harness");
        assert_eq!(warnings[0]["scope_key"], "warning-scope");
        assert_eq!(warnings[0]["prospective"], 1);
        assert_eq!(
            warnings[0]["policy_path"],
            "workflow.capacity.scopes.harness.statuses.open"
        );
        let mut apply_args = args.to_vec();
        apply_args.extend_from_slice(&[
            "--apply",
            "--expect",
            plan["plan_token"].as_str().unwrap(),
        ]);
        let applied = json(&workspace, &apply_args, "capacity_warning_apply");
        assert_eq!(applied["changed"], serde_json::json!([dependent]));
        assert_eq!(applied["capacity_warnings"], plan["capacity_warnings"]);
        assert!(ready_contains(&workspace, &dependent));
    }

    #[test]
    fn unblock_595_later_edits_invalidate_dependency_only_intent() {
        for edit in ["status", "notes", "label", "comment"] {
            let workspace = workspace();
            let (blocker, dependent) = pair(&workspace, true);
            match edit {
                "status" => block(&workspace, &dependent, false),
                "notes" => {
                    success(
                        &workspace,
                        &[
                            "update",
                            &dependent,
                            "--notes",
                            "Now also waiting for external approval",
                        ],
                        "record_new_external_hold",
                    );
                }
                "label" => {
                    success(
                        &workspace,
                        &["label", "add", &dependent, "external-approval"],
                        "label_new_external_hold",
                    );
                }
                "comment" => {
                    success(
                        &workspace,
                        &[
                            "comments",
                            "add",
                            &dependent,
                            "--author",
                            "approval-bot",
                            "--message",
                            "External approval is now required before this work can resume",
                        ],
                        "comment_new_external_hold",
                    );
                }
                _ => unreachable!(),
            }
            close(&workspace, &blocker);
            assert!(
                !eligible(&preview(&workspace, &[]), &dependent),
                "{edit} must invalidate the earlier dependency-only declaration"
            );
            let before = show(&workspace, &dependent);
            assert_eq!(repair(&workspace, &[])["changed"], serde_json::json!([]));
            assert_eq!(show(&workspace, &dependent), before);
            assert_eq!(before["status"], "blocked");
        }
    }

    #[test]
    fn unblock_595_supersedes_relationship_and_custom_status_keep_blocker_live() {
        let workspace = workspace();
        let (blocker, dependent) = pair(&workspace, true);
        let replacement = create(&workspace, "replacement for an unfinished prerequisite");
        success(
            &workspace,
            &["dep", "add", &replacement, &blocker, "--type", "supersedes"],
            "record_supersedes_relationship",
        );
        success(
            &workspace,
            &["update", &blocker, "--status", "superseded"],
            "record_custom_superseded_status",
        );
        assert_eq!(show(&workspace, &blocker)["status"], "superseded");
        assert!(!eligible(&preview(&workspace, &[]), &dependent));
        assert_eq!(
            repair(&workspace, &["--authorize", &dependent])["changed"],
            serde_json::json!([])
        );
        assert_eq!(show(&workspace, &dependent)["status"], "blocked");
        assert!(!ready_contains(&workspace, &dependent));

        // Only the actual terminal transition satisfies B's blocking edge;
        // neither an informational edge nor a similarly named custom status
        // provides evidence that the prerequisite completed.
        close(&workspace, &blocker);
        assert!(eligible(&preview(&workspace, &[]), &dependent));
        assert_eq!(
            repair(&workspace, &[])["changed"],
            serde_json::json!([dependent])
        );
        assert!(ready_contains(&workspace, &dependent));
    }

    #[test]
    fn unblock_595_soft_delete_preserves_orphaned_manual_status() {
        let workspace = workspace();
        let (blocker, dependent) = pair(&workspace, true);
        success(
            &workspace,
            &[
                "delete",
                &blocker,
                "--force",
                "--reason",
                "prerequisite withdrawn without completing the work",
            ],
            "soft_delete_prerequisite",
        );
        assert_eq!(
            issue_from_jsonl(&workspace, &blocker)["status"],
            "tombstone"
        );
        let before = show(&workspace, &dependent);
        assert_eq!(before["status"], "blocked");
        assert!(
            issue_from_jsonl(&workspace, &dependent)["dependencies"]
                .as_array()
                .is_none_or(Vec::is_empty),
            "delete --force removes the forward dependency, including on soft delete"
        );

        // The stale-blocked detector requires retained dependency history.
        // Deletion orphans this record, so authorization must not manufacture
        // the missing evidence or treat disappearance as completion.
        let options = ["--authorize", dependent.as_str()];
        let plan = preview(&workspace, &options);
        let orphan = candidate(&plan, &dependent).expect("orphan is reported as skipped");
        assert_eq!(orphan["eligible"], false);
        assert_eq!(orphan["reason"], "no_dependency_history");
        assert_eq!(orphan["blockers"], serde_json::json!([]));
        assert_eq!(
            repair(&workspace, &options)["changed"],
            serde_json::json!([])
        );
        assert_eq!(show(&workspace, &dependent), before);
        assert!(!ready_contains(&workspace, &dependent));
    }

    #[test]
    fn unblock_595_dependency_intent_cannot_be_invented_for_an_existing_manual_hold() {
        let workspace = workspace();
        let dependent = create(&workspace, "manual approval required");
        let before = show(&workspace, &dependent);
        let declare = [
            "update",
            dependent.as_str(),
            "--status",
            "blocked",
            "--blocked-by-dependencies",
            "--json",
        ];
        let no_blocker = command(&workspace, &declare, "dependency_intent_without_blocker");
        assert!(
            !no_blocker.status.success(),
            "no live dependency: {no_blocker:?}"
        );
        assert_eq!(show(&workspace, &dependent), before);

        let blocker = create(&workspace, "added prerequisite");
        dependency(&workspace, &dependent, &blocker);
        block(&workspace, &dependent, false);
        let manual = show(&workspace, &dependent);
        let already_blocked = command(&workspace, &declare, "dependency_intent_after_manual_block");
        assert!(
            !already_blocked.status.success(),
            "existing manual block: {already_blocked:?}"
        );
        assert_eq!(show(&workspace, &dependent), manual);
        close(&workspace, &blocker);
        assert!(!eligible(&preview(&workspace, &[]), &dependent));
    }

    #[test]
    fn unblock_595_stale_review_refuses_the_whole_batch() {
        for change in ["notes", "assignee", "blocker", "policy"] {
            let workspace = workspace();
            let (blocker, first) = pair(&workspace, false);
            let second = create(&workspace, "second reviewed dependent");
            dependency(&workspace, &second, &blocker);
            block(&workspace, &second, false);
            let competing = create(&workspace, "new concurrent prerequisite");
            close(&workspace, &blocker);
            let options = [
                "--authorize",
                first.as_str(),
                "--authorize",
                second.as_str(),
            ];
            let plan = preview(&workspace, &options);
            assert_eq!(plan["planned_ids"].as_array().unwrap().len(), 2);
            match change {
                "notes" => {
                    success(
                        &workspace,
                        &["update", &second, "--notes", "competing edit"],
                        "competing_notes",
                    );
                }
                "assignee" => {
                    success(
                        &workspace,
                        &["update", &second, "--assignee", "alice"],
                        "competing_assignee",
                    );
                }
                "blocker" => dependency(&workspace, &second, &competing),
                "policy" => write_policy(
                    &workspace,
                    "workflow:\n  strict: true\n  statuses: [open, blocked, closed]\n  transitions:\n    blocked: [closed]\n",
                ),
                _ => unreachable!(),
            }
            let before_first = show(&workspace, &first);
            let before_second = show(&workspace, &second);
            let before = persisted_files(&workspace);
            let result = apply(&workspace, &options, plan["plan_token"].as_str().unwrap());
            assert!(
                !result.status.success(),
                "{change} must invalidate review: {result:?}"
            );
            assert_eq!(
                persisted_files(&workspace),
                before,
                "{change}: refused repair wrote state"
            );
            assert_eq!(
                show(&workspace, &first),
                before_first,
                "{change}: first row partially repaired"
            );
            assert_eq!(
                show(&workspace, &second),
                before_second,
                "{change}: competing edit overwritten"
            );
        }
    }

    #[test]
    fn unblock_595_review_binds_authorization_and_requires_explicit_apply_token() {
        let workspace = workspace();
        let (blocker, dependent) = pair(&workspace, false);
        close(&workspace, &blocker);
        let plan = preview(&workspace, &[]);
        let unauthorized_token = apply(
            &workspace,
            &["--authorize", &dependent],
            plan["plan_token"].as_str().unwrap(),
        );
        assert!(
            !unauthorized_token.status.success(),
            "authorization changed the reviewed plan"
        );
        let missing = command(
            &workspace,
            &[
                "doctor",
                "unblock",
                "--authorize",
                &dependent,
                "--apply",
                "--json",
            ],
            "unblock_missing_token",
        );
        assert!(
            !missing.status.success(),
            "apply without reviewed token must fail"
        );
        assert_eq!(show(&workspace, &dependent)["status"], "blocked");
    }

    #[test]
    fn unblock_595_review_token_cannot_cross_identical_workspaces() {
        let source = workspace();
        let (blocker, dependent) = pair(&source, false);
        close(&source, &blocker);
        let options = ["--authorize", dependent.as_str()];
        let source_plan = preview(&source, &options);

        // Clone the closed fixture only after every CLI process has exited.
        // The issue IDs, graph, audit and settings are identical, so this
        // refusal specifically tests workspace binding rather than missing IDs.
        let clone = BrWorkspace::new();
        let source_beads = source.root.join(".beads");
        let clone_beads = clone.root.join(".beads");
        for entry in walkdir::WalkDir::new(&source_beads) {
            let entry = entry.expect("read fixture tree");
            let target = clone_beads.join(entry.path().strip_prefix(&source_beads).unwrap());
            if entry.file_type().is_dir() {
                fs::create_dir_all(&target).expect("clone fixture directory");
            } else {
                assert!(
                    entry.file_type().is_file(),
                    "fixture contains a special file"
                );
                fs::copy(entry.path(), &target).expect("clone tracker data");
            }
        }
        let clone_plan = preview(&clone, &options);
        assert_eq!(clone_plan["planned_ids"], source_plan["planned_ids"]);
        assert_eq!(clone_plan["planned_ids"], serde_json::json!([dependent]));
        assert_ne!(clone_plan["plan_token"], source_plan["plan_token"]);
        let before = persisted_files(&clone);
        let wrong_workspace = apply(
            &clone,
            &options,
            source_plan["plan_token"].as_str().unwrap(),
        );
        assert!(
            !wrong_workspace.status.success(),
            "a token from another workspace must be rejected: {wrong_workspace:?}"
        );
        assert_eq!(persisted_files(&clone), before);
        assert_eq!(show(&clone, &dependent)["status"], "blocked");

        let applied = apply(&clone, &options, clone_plan["plan_token"].as_str().unwrap());
        assert!(
            applied.status.success(),
            "local reviewed repair: {applied:?}"
        );
        assert!(ready_contains(&clone, &dependent));
        assert_eq!(show(&source, &dependent)["status"], "blocked");
        assert!(!ready_contains(&source, &dependent));
    }

    #[test]
    fn unblock_595_flat_doctor_modes_cannot_override_reviewed_repair() {
        let workspace = workspace();
        let (blocker, dependent) = pair(&workspace, false);
        close(&workspace, &blocker);
        let options = ["--authorize", dependent.as_str()];
        let plan = preview(&workspace, &options);
        let bundle = workspace.root.join("unexpected-doctor-bundle.tar.gz");
        let cases: &[&[&str]] = &[
            &["--repair"],
            &["--fix"],
            &["--repair-indexes"],
            &["--robot-triage"],
            &["--quick"],
            &["--selftest"],
            &["--bundle", bundle.to_str().unwrap()],
            &["--only=fm-deps-stale-blocked"],
            &["--skip=fm-deps-stale-blocked"],
            &["--allow-repeated-repair"],
            &["--unsafe-auto-fix"],
            &["--git-runtime-files"],
            &["--dry-run"],
        ];
        for flags in cases {
            let before = persisted_files(&workspace);
            let mut args = vec!["doctor"];
            args.extend_from_slice(flags);
            args.extend_from_slice(&[
                "unblock",
                "--authorize",
                &dependent,
                "--apply",
                "--expect",
                plan["plan_token"].as_str().unwrap(),
                "--json",
            ]);
            let result = command(&workspace, &args, "unblock_conflicting_doctor_mode");
            assert!(
                !result.status.success(),
                "{flags:?} must not dispatch another doctor mode: {result:?}"
            );
            assert_eq!(
                persisted_files(&workspace),
                before,
                "{flags:?} changed tracker data before refusing the mixed modes"
            );
            assert!(
                !bundle.exists(),
                "bundle mode ran before unblock validation"
            );
        }
        assert_eq!(show(&workspace, &dependent)["status"], "blocked");
        assert!(!ready_contains(&workspace, &dependent));
    }
}
