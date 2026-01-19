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

// ============================================================================
// SUCCESS PATH TESTS
// ============================================================================

#[test]
fn e2e_epic_status_shows_progress() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create an epic
    let create_epic = run_br(
        &workspace,
        ["create", "Test Epic", "--type", "epic"],
        "create_epic",
    );
    assert!(
        create_epic.status.success(),
        "create epic failed: {}",
        create_epic.stderr
    );
    let epic_id = parse_created_id(&create_epic.stdout);

    // Create two child tasks
    let create_task1 = run_br(
        &workspace,
        ["create", "Task 1", "--type", "task"],
        "create_task1",
    );
    assert!(
        create_task1.status.success(),
        "create task1 failed: {}",
        create_task1.stderr
    );
    let task1_id = parse_created_id(&create_task1.stdout);

    let create_task2 = run_br(
        &workspace,
        ["create", "Task 2", "--type", "task"],
        "create_task2",
    );
    assert!(
        create_task2.status.success(),
        "create task2 failed: {}",
        create_task2.stderr
    );
    let task2_id = parse_created_id(&create_task2.stdout);

    // Add children to epic via parent-child dependencies
    let dep1 = run_br(
        &workspace,
        ["dep", "add", &task1_id, &epic_id, "--type", "parent-child"],
        "dep_add_1",
    );
    assert!(dep1.status.success(), "dep add 1 failed: {}", dep1.stderr);

    let dep2 = run_br(
        &workspace,
        ["dep", "add", &task2_id, &epic_id, "--type", "parent-child"],
        "dep_add_2",
    );
    assert!(dep2.status.success(), "dep add 2 failed: {}", dep2.stderr);

    // Check epic status - should show 0/2 closed
    let status = run_br(
        &workspace,
        ["epic", "status", "--json"],
        "epic_status_initial",
    );
    assert!(
        status.status.success(),
        "epic status failed: {}",
        status.stderr
    );
    let payload = extract_json_payload(&status.stdout);
    let status_json: Vec<Value> = serde_json::from_str(&payload).expect("parse epic status json");
    assert!(!status_json.is_empty(), "expected at least one epic");

    let epic_entry = status_json
        .iter()
        .find(|e| e["epic"]["id"] == epic_id)
        .expect("epic not found in status");
    assert_eq!(epic_entry["total_children"], 2);
    assert_eq!(epic_entry["closed_children"], 0);
    assert_eq!(epic_entry["eligible_for_close"], false);

    // Text output should show progress
    let status_text = run_br(&workspace, ["epic", "status"], "epic_status_text");
    assert!(
        status_text.status.success(),
        "epic status text failed: {}",
        status_text.stderr
    );
    assert!(
        status_text.stdout.contains("0/2 children closed"),
        "expected 0/2 progress in output: {}",
        status_text.stdout
    );
}

#[test]
fn e2e_epic_status_eligible_when_all_children_closed() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create epic with one child
    let create_epic = run_br(
        &workspace,
        ["create", "Closeable Epic", "--type", "epic"],
        "create_epic",
    );
    assert!(create_epic.status.success());
    let epic_id = parse_created_id(&create_epic.stdout);

    let create_task = run_br(
        &workspace,
        ["create", "Single Task", "--type", "task"],
        "create_task",
    );
    assert!(create_task.status.success());
    let task_id = parse_created_id(&create_task.stdout);

    let dep = run_br(
        &workspace,
        ["dep", "add", &task_id, &epic_id, "--type", "parent-child"],
        "dep_add",
    );
    assert!(dep.status.success());

    // Initially not eligible
    let status1 = run_br(
        &workspace,
        ["epic", "status", "--json"],
        "epic_status_before",
    );
    assert!(status1.status.success());
    let payload1 = extract_json_payload(&status1.stdout);
    let json1: Vec<Value> = serde_json::from_str(&payload1).unwrap();
    let epic1 = json1.iter().find(|e| e["epic"]["id"] == epic_id).unwrap();
    assert_eq!(epic1["eligible_for_close"], false);

    // Close the child task (use --force since parent-child deps are blocking)
    let close_task = run_br(&workspace, ["close", &task_id, "--force"], "close_task");
    assert!(
        close_task.status.success(),
        "close task failed: {}",
        close_task.stderr
    );

    // Now should be eligible
    let status2 = run_br(
        &workspace,
        ["epic", "status", "--json"],
        "epic_status_after",
    );
    assert!(status2.status.success());
    let payload2 = extract_json_payload(&status2.stdout);
    let json2: Vec<Value> = serde_json::from_str(&payload2).unwrap();
    let epic2 = json2.iter().find(|e| e["epic"]["id"] == epic_id).unwrap();
    assert_eq!(epic2["total_children"], 1);
    assert_eq!(epic2["closed_children"], 1);
    assert_eq!(epic2["eligible_for_close"], true);
}

#[test]
fn e2e_epic_close_eligible_closes_epics() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    // Create epic with one child
    let create_epic = run_br(
        &workspace,
        ["create", "Auto-closeable Epic", "--type", "epic"],
        "create_epic",
    );
    assert!(create_epic.status.success());
    let epic_id = parse_created_id(&create_epic.stdout);

    let create_task = run_br(
        &workspace,
        ["create", "Task for epic", "--type", "task"],
        "create_task",
    );
    assert!(create_task.status.success());
    let task_id = parse_created_id(&create_task.stdout);

    let dep = run_br(
        &workspace,
        ["dep", "add", &task_id, &epic_id, "--type", "parent-child"],
        "dep_add",
    );
    assert!(dep.status.success());

    // Close the child (use --force since parent-child deps are blocking)
    let close_task = run_br(&workspace, ["close", &task_id, "--force"], "close_task");
    assert!(close_task.status.success());

    // Verify epic is open before close-eligible
    let show_before = run_br(&workspace, ["show", &epic_id, "--json"], "show_before");
    assert!(show_before.status.success());
    let payload_before = extract_json_payload(&show_before.stdout);
    let show_json_before: Vec<Value> = serde_json::from_str(&payload_before).unwrap();
    assert_eq!(show_json_before[0]["status"], "open");

    // Run close-eligible
    let close_eligible = run_br(
        &workspace,
        ["epic", "close-eligible", "--json"],
        "close_eligible",
    );
    assert!(
        close_eligible.status.success(),
        "close-eligible failed: {}",
        close_eligible.stderr
    );
    let payload = extract_json_payload(&close_eligible.stdout);
    let result: Value = serde_json::from_str(&payload).unwrap();
    assert_eq!(result["count"], 1);
    assert!(
        result["closed"]
            .as_array()
            .unwrap()
            .iter()
            .any(|id| id == &epic_id)
    );

    // Verify epic is now closed
    let show_after = run_br(&workspace, ["show", &epic_id, "--json"], "show_after");
    assert!(show_after.status.success());
    let payload_after = extract_json_payload(&show_after.stdout);
    let show_json_after: Vec<Value> = serde_json::from_str(&payload_after).unwrap();
    assert_eq!(show_json_after[0]["status"], "closed");
    assert!(
        show_json_after[0]["close_reason"]
            .as_str()
            .unwrap_or("")
            .contains("children completed"),
        "close reason should mention children completed"
    );
}

#[test]
fn e2e_epic_close_eligible_dry_run() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    // Create epic with one closed child
    let create_epic = run_br(
        &workspace,
        ["create", "Dry Run Epic", "--type", "epic"],
        "create_epic",
    );
    assert!(create_epic.status.success());
    let epic_id = parse_created_id(&create_epic.stdout);

    let create_task = run_br(
        &workspace,
        ["create", "Dry run task", "--type", "task"],
        "create_task",
    );
    assert!(create_task.status.success());
    let task_id = parse_created_id(&create_task.stdout);

    let dep = run_br(
        &workspace,
        ["dep", "add", &task_id, &epic_id, "--type", "parent-child"],
        "dep_add",
    );
    assert!(dep.status.success());

    let close_task = run_br(&workspace, ["close", &task_id, "--force"], "close_task");
    assert!(close_task.status.success());

    // Run dry-run - should show what would be closed
    let dry_run = run_br(
        &workspace,
        ["epic", "close-eligible", "--dry-run"],
        "dry_run",
    );
    assert!(
        dry_run.status.success(),
        "dry-run failed: {}",
        dry_run.stderr
    );
    assert!(
        dry_run.stdout.contains("Would close"),
        "dry-run output should mention 'Would close': {}",
        dry_run.stdout
    );

    // Epic should still be open
    let show = run_br(
        &workspace,
        ["show", &epic_id, "--json"],
        "show_after_dry_run",
    );
    assert!(show.status.success());
    let payload = extract_json_payload(&show.stdout);
    let show_json: Vec<Value> = serde_json::from_str(&payload).unwrap();
    assert_eq!(
        show_json[0]["status"], "open",
        "epic should remain open after dry-run"
    );
}

#[test]
fn e2e_epic_status_eligible_only_filter() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    // Create two epics - one eligible, one not
    let create_epic1 = run_br(
        &workspace,
        ["create", "Eligible Epic", "--type", "epic"],
        "create_epic1",
    );
    assert!(create_epic1.status.success());
    let epic1_id = parse_created_id(&create_epic1.stdout);

    let create_epic2 = run_br(
        &workspace,
        ["create", "Not Eligible Epic", "--type", "epic"],
        "create_epic2",
    );
    assert!(create_epic2.status.success());
    let epic2_id = parse_created_id(&create_epic2.stdout);

    // Add closed child to epic1
    let create_task1 = run_br(
        &workspace,
        ["create", "Task 1", "--type", "task"],
        "create_task1",
    );
    assert!(create_task1.status.success());
    let task1_id = parse_created_id(&create_task1.stdout);

    let dep1 = run_br(
        &workspace,
        ["dep", "add", &task1_id, &epic1_id, "--type", "parent-child"],
        "dep_add_1",
    );
    assert!(dep1.status.success());

    let close_task1 = run_br(&workspace, ["close", &task1_id, "--force"], "close_task1");
    assert!(close_task1.status.success());

    // Add open child to epic2
    let create_task2 = run_br(
        &workspace,
        ["create", "Task 2", "--type", "task"],
        "create_task2",
    );
    assert!(create_task2.status.success());
    let task2_id = parse_created_id(&create_task2.stdout);

    let dep2 = run_br(
        &workspace,
        ["dep", "add", &task2_id, &epic2_id, "--type", "parent-child"],
        "dep_add_2",
    );
    assert!(dep2.status.success());

    // Without filter - should see both epics
    let status_all = run_br(&workspace, ["epic", "status", "--json"], "status_all");
    assert!(status_all.status.success());
    let payload_all = extract_json_payload(&status_all.stdout);
    let json_all: Vec<Value> = serde_json::from_str(&payload_all).unwrap();
    assert_eq!(json_all.len(), 2, "should have 2 epics");

    // With --eligible-only - should see only epic1
    let status_eligible = run_br(
        &workspace,
        ["epic", "status", "--eligible-only", "--json"],
        "status_eligible",
    );
    assert!(status_eligible.status.success());
    let payload_eligible = extract_json_payload(&status_eligible.stdout);
    let json_eligible: Vec<Value> = serde_json::from_str(&payload_eligible).unwrap();
    assert_eq!(json_eligible.len(), 1, "should have only 1 eligible epic");
    assert_eq!(json_eligible[0]["epic"]["id"], epic1_id);
}

// ============================================================================
// EDGE CASE TESTS
// ============================================================================

#[test]
fn e2e_epic_childless_epic_not_eligible() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    // Create epic with no children
    let create_epic = run_br(
        &workspace,
        ["create", "Childless Epic", "--type", "epic"],
        "create_epic",
    );
    assert!(create_epic.status.success());
    let epic_id = parse_created_id(&create_epic.stdout);

    // Check status
    let status = run_br(&workspace, ["epic", "status", "--json"], "epic_status");
    assert!(status.status.success());
    let payload = extract_json_payload(&status.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).unwrap();
    let epic = json.iter().find(|e| e["epic"]["id"] == epic_id).unwrap();

    assert_eq!(epic["total_children"], 0);
    assert_eq!(epic["closed_children"], 0);
    assert_eq!(
        epic["eligible_for_close"], false,
        "childless epic should not be eligible for close"
    );

    // close-eligible should not close it
    let close_eligible = run_br(
        &workspace,
        ["epic", "close-eligible", "--json"],
        "close_eligible",
    );
    assert!(close_eligible.status.success());
    let payload_close = extract_json_payload(&close_eligible.stdout);
    // Could be empty array or object with count=0
    let result: Value = serde_json::from_str(&payload_close).unwrap_or(Value::Array(vec![]));
    if result.is_array() {
        assert!(result.as_array().unwrap().is_empty());
    } else {
        assert_eq!(result["count"], 0);
    }
}

#[test]
fn e2e_epic_nested_epics() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    // Create parent epic
    let create_parent = run_br(
        &workspace,
        ["create", "Parent Epic", "--type", "epic"],
        "create_parent",
    );
    assert!(create_parent.status.success());
    let parent_id = parse_created_id(&create_parent.stdout);

    // Create child epic
    let create_child = run_br(
        &workspace,
        ["create", "Child Epic", "--type", "epic"],
        "create_child",
    );
    assert!(create_child.status.success());
    let child_id = parse_created_id(&create_child.stdout);

    // Create a task under the child epic
    let create_task = run_br(
        &workspace,
        ["create", "Nested Task", "--type", "task"],
        "create_task",
    );
    assert!(create_task.status.success());
    let task_id = parse_created_id(&create_task.stdout);

    // Set up relationships: task -> child epic, child epic -> parent epic
    let dep1 = run_br(
        &workspace,
        ["dep", "add", &task_id, &child_id, "--type", "parent-child"],
        "dep_task_to_child",
    );
    assert!(dep1.status.success());

    let dep2 = run_br(
        &workspace,
        [
            "dep",
            "add",
            &child_id,
            &parent_id,
            "--type",
            "parent-child",
        ],
        "dep_child_to_parent",
    );
    assert!(dep2.status.success());

    // Check status - parent should have child epic as child
    let status = run_br(&workspace, ["epic", "status", "--json"], "epic_status");
    assert!(status.status.success());
    let payload = extract_json_payload(&status.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).unwrap();

    let parent_epic = json.iter().find(|e| e["epic"]["id"] == parent_id).unwrap();
    assert_eq!(
        parent_epic["total_children"], 1,
        "parent should have 1 child (the child epic)"
    );

    let child_epic = json.iter().find(|e| e["epic"]["id"] == child_id).unwrap();
    assert_eq!(
        child_epic["total_children"], 1,
        "child epic should have 1 child (the task)"
    );

    // Close the task (use --force since parent-child deps are blocking)
    let close_task = run_br(&workspace, ["close", &task_id, "--force"], "close_task");
    assert!(close_task.status.success());

    // Child epic should now be eligible
    let status2 = run_br(
        &workspace,
        ["epic", "status", "--json"],
        "epic_status_after_task_close",
    );
    assert!(status2.status.success());
    let payload2 = extract_json_payload(&status2.stdout);
    let json2: Vec<Value> = serde_json::from_str(&payload2).unwrap();

    let child_epic2 = json2.iter().find(|e| e["epic"]["id"] == child_id).unwrap();
    assert_eq!(
        child_epic2["eligible_for_close"], true,
        "child epic should be eligible"
    );

    // Parent epic is not eligible yet (child epic is open)
    let parent_epic2 = json2.iter().find(|e| e["epic"]["id"] == parent_id).unwrap();
    assert_eq!(
        parent_epic2["eligible_for_close"], false,
        "parent epic not eligible until child epic closed"
    );
}

#[test]
fn e2e_epic_no_epics_message() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    // Don't create any epics, just tasks
    let create_task = run_br(
        &workspace,
        ["create", "Just a task", "--type", "task"],
        "create_task",
    );
    assert!(create_task.status.success());

    // Epic status should say no epics
    let status = run_br(&workspace, ["epic", "status"], "epic_status_none");
    assert!(status.status.success());
    assert!(
        status.stdout.contains("No open epics found"),
        "should show no epics message: {}",
        status.stdout
    );

    // JSON output should be empty array
    let status_json = run_br(
        &workspace,
        ["epic", "status", "--json"],
        "epic_status_json_none",
    );
    assert!(status_json.status.success());
    let payload = extract_json_payload(&status_json.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).unwrap();
    assert!(json.is_empty(), "JSON should be empty array when no epics");
}

#[test]
fn e2e_epic_close_eligible_no_eligible_message() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    // Create epic with open child
    let create_epic = run_br(
        &workspace,
        ["create", "Open Children Epic", "--type", "epic"],
        "create_epic",
    );
    assert!(create_epic.status.success());
    let epic_id = parse_created_id(&create_epic.stdout);

    let create_task = run_br(
        &workspace,
        ["create", "Open Task", "--type", "task"],
        "create_task",
    );
    assert!(create_task.status.success());
    let task_id = parse_created_id(&create_task.stdout);

    let dep = run_br(
        &workspace,
        ["dep", "add", &task_id, &epic_id, "--type", "parent-child"],
        "dep_add",
    );
    assert!(dep.status.success());

    // close-eligible should say no epics eligible
    let close_eligible = run_br(
        &workspace,
        ["epic", "close-eligible"],
        "close_eligible_none",
    );
    assert!(close_eligible.status.success());
    assert!(
        close_eligible.stdout.contains("No epics eligible"),
        "should say no epics eligible: {}",
        close_eligible.stdout
    );
}

#[test]
fn e2e_epic_multiple_children_partial_progress() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    // Create epic with 5 children
    let create_epic = run_br(
        &workspace,
        ["create", "Large Epic", "--type", "epic"],
        "create_epic",
    );
    assert!(create_epic.status.success());
    let epic_id = parse_created_id(&create_epic.stdout);

    let mut task_ids = Vec::new();
    for i in 1..=5 {
        let create_task = run_br(
            &workspace,
            ["create", &format!("Task {i}"), "--type", "task"],
            &format!("create_task_{i}"),
        );
        assert!(create_task.status.success());
        let task_id = parse_created_id(&create_task.stdout);

        let dep = run_br(
            &workspace,
            ["dep", "add", &task_id, &epic_id, "--type", "parent-child"],
            &format!("dep_add_{i}"),
        );
        assert!(dep.status.success());

        task_ids.push(task_id);
    }

    // Check status - 0/5 closed
    let status1 = run_br(&workspace, ["epic", "status", "--json"], "status_0_of_5");
    assert!(status1.status.success());
    let payload1 = extract_json_payload(&status1.stdout);
    let json1: Vec<Value> = serde_json::from_str(&payload1).unwrap();
    let epic1 = json1.iter().find(|e| e["epic"]["id"] == epic_id).unwrap();
    assert_eq!(epic1["total_children"], 5);
    assert_eq!(epic1["closed_children"], 0);

    // Close 3 tasks (use --force since parent-child deps are blocking)
    for task_id in task_ids.iter().take(3) {
        let close = run_br(&workspace, ["close", task_id, "--force"], "close_task");
        assert!(close.status.success());
    }

    // Check status - 3/5 closed, not eligible
    let status2 = run_br(&workspace, ["epic", "status", "--json"], "status_3_of_5");
    assert!(status2.status.success());
    let payload2 = extract_json_payload(&status2.stdout);
    let json2: Vec<Value> = serde_json::from_str(&payload2).unwrap();
    let epic2 = json2.iter().find(|e| e["epic"]["id"] == epic_id).unwrap();
    assert_eq!(epic2["total_children"], 5);
    assert_eq!(epic2["closed_children"], 3);
    assert_eq!(epic2["eligible_for_close"], false);

    // Text output should show 60% progress
    let status_text = run_br(&workspace, ["epic", "status"], "status_text_partial");
    assert!(status_text.status.success());
    assert!(
        status_text.stdout.contains("3/5 children closed"),
        "should show 3/5 progress: {}",
        status_text.stdout
    );
    assert!(
        status_text.stdout.contains("60%"),
        "should show 60% progress: {}",
        status_text.stdout
    );

    // Close remaining tasks (use --force since parent-child deps are blocking)
    for task_id in task_ids.iter().skip(3) {
        let close = run_br(&workspace, ["close", task_id, "--force"], "close_remaining");
        assert!(close.status.success());
    }

    // Now should be eligible
    let status3 = run_br(&workspace, ["epic", "status", "--json"], "status_5_of_5");
    assert!(status3.status.success());
    let payload3 = extract_json_payload(&status3.stdout);
    let json3: Vec<Value> = serde_json::from_str(&payload3).unwrap();
    let epic3 = json3.iter().find(|e| e["epic"]["id"] == epic_id).unwrap();
    assert_eq!(epic3["closed_children"], 5);
    assert_eq!(epic3["eligible_for_close"], true);
}

#[test]
fn e2e_epic_closed_epic_not_shown() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    // Create and close an epic
    let create_epic = run_br(
        &workspace,
        ["create", "Already Closed Epic", "--type", "epic"],
        "create_epic",
    );
    assert!(create_epic.status.success());
    let epic_id = parse_created_id(&create_epic.stdout);

    let close_epic = run_br(&workspace, ["close", &epic_id], "close_epic");
    assert!(close_epic.status.success());

    // Epic status should not show closed epics
    let status = run_br(&workspace, ["epic", "status", "--json"], "epic_status");
    assert!(status.status.success());
    let payload = extract_json_payload(&status.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).unwrap();
    assert!(
        !json.iter().any(|e| e["epic"]["id"] == epic_id),
        "closed epic should not appear in status"
    );
}

#[test]
fn e2e_epic_deleted_child_removes_dependency() {
    // When a child task is deleted, its dependencies are also removed.
    // This means the epic loses that child entirely (not just counts it as closed).
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    // Create epic with one child
    let create_epic = run_br(
        &workspace,
        ["create", "Tombstone Test Epic", "--type", "epic"],
        "create_epic",
    );
    assert!(create_epic.status.success());
    let epic_id = parse_created_id(&create_epic.stdout);

    let create_task = run_br(
        &workspace,
        ["create", "Task to delete", "--type", "task"],
        "create_task",
    );
    assert!(create_task.status.success());
    let task_id = parse_created_id(&create_task.stdout);

    let dep = run_br(
        &workspace,
        ["dep", "add", &task_id, &epic_id, "--type", "parent-child"],
        "dep_add",
    );
    assert!(dep.status.success());

    // Verify initial state: epic has 1 child
    let status_before = run_br(
        &workspace,
        ["epic", "status", "--json"],
        "epic_status_before",
    );
    assert!(status_before.status.success());
    let payload_before = extract_json_payload(&status_before.stdout);
    let json_before: Vec<Value> = serde_json::from_str(&payload_before).unwrap();
    let epic_before = json_before
        .iter()
        .find(|e| e["epic"]["id"] == epic_id)
        .unwrap();
    assert_eq!(epic_before["total_children"], 1);

    // Delete the task (creates tombstone and removes dependencies)
    let delete_task = run_br(
        &workspace,
        [
            "delete",
            &task_id,
            "--force",
            "--reason",
            "Testing tombstone",
        ],
        "delete_task",
    );
    assert!(delete_task.status.success());

    // After deletion, epic should have 0 children (dependency was removed)
    let status = run_br(
        &workspace,
        ["epic", "status", "--json"],
        "epic_status_after_delete",
    );
    assert!(status.status.success());
    let payload = extract_json_payload(&status.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).unwrap();
    let epic = json.iter().find(|e| e["epic"]["id"] == epic_id).unwrap();
    assert_eq!(
        epic["total_children"], 0,
        "deleted child's dependency should be removed"
    );
    assert_eq!(epic["closed_children"], 0);
    // Childless epic is not eligible for auto-close
    assert_eq!(
        epic["eligible_for_close"], false,
        "childless epic should not be eligible"
    );
}
