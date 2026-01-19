//! E2E tests for the `label` command.
//!
//! Tests label management: add, remove, list, list-all, and rename.

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

// =============================================================================
// Success Path Tests (1-5)
// =============================================================================

/// Test 1: Add single label, verify via show
#[test]
fn e2e_label_add_single_verify_show() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Test issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);
    assert!(!id.is_empty(), "missing created id");

    // Add label
    let add = run_br(&workspace, ["label", "add", &id, "bug"], "label_add");
    assert!(add.status.success(), "label add failed: {}", add.stderr);
    assert!(
        add.stdout.contains("Added label") || add.stdout.contains("bug"),
        "unexpected output: {}",
        add.stdout
    );

    // Verify via show --json
    let show = run_br(&workspace, ["show", &id, "--json"], "show");
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let show_payload = extract_json_payload(&show.stdout);
    let show_json: Vec<Value> = serde_json::from_str(&show_payload).expect("show json");
    assert_eq!(show_json.len(), 1);
    let labels = &show_json[0]["labels"];
    assert!(labels.is_array(), "labels should be array");
    let label_arr: Vec<String> = serde_json::from_value(labels.clone()).unwrap();
    assert!(
        label_arr.contains(&"bug".to_string()),
        "label not found in show"
    );

    // Verify via label list
    let list = run_br(&workspace, ["label", "list", &id], "label_list");
    assert!(list.status.success(), "label list failed: {}", list.stderr);
    assert!(list.stdout.contains("bug"), "label not in list output");
}

/// Test 2: Add multiple labels to same issue
#[test]
fn e2e_label_add_multiple_to_same_issue() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Multi-label issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // Add first label
    let add1 = run_br(&workspace, ["label", "add", &id, "bug"], "add1");
    assert!(add1.status.success(), "label add 1 failed: {}", add1.stderr);

    // Add second label
    let add2 = run_br(&workspace, ["label", "add", &id, "urgent"], "add2");
    assert!(add2.status.success(), "label add 2 failed: {}", add2.stderr);

    // Add third label
    let add3 = run_br(&workspace, ["label", "add", &id, "frontend"], "add3");
    assert!(add3.status.success(), "label add 3 failed: {}", add3.stderr);

    // Verify all labels present
    let list = run_br(&workspace, ["label", "list", &id, "--json"], "list_labels");
    assert!(list.status.success(), "label list failed: {}", list.stderr);
    let labels_payload = extract_json_payload(&list.stdout);
    let labels: Vec<String> = serde_json::from_str(&labels_payload).expect("labels json");
    assert!(labels.contains(&"bug".to_string()), "missing bug label");
    assert!(
        labels.contains(&"urgent".to_string()),
        "missing urgent label"
    );
    assert!(
        labels.contains(&"frontend".to_string()),
        "missing frontend label"
    );
}

/// Test 3: Remove label, verify removed
#[test]
fn e2e_label_remove_verify() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Label remove test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // Add labels
    let add1 = run_br(&workspace, ["label", "add", &id, "bug"], "add1");
    assert!(add1.status.success(), "add failed: {}", add1.stderr);
    let add2 = run_br(&workspace, ["label", "add", &id, "urgent"], "add2");
    assert!(add2.status.success(), "add failed: {}", add2.stderr);

    // Remove one label
    let remove = run_br(&workspace, ["label", "remove", &id, "bug"], "remove");
    assert!(remove.status.success(), "remove failed: {}", remove.stderr);
    assert!(
        remove.stdout.contains("Removed") || remove.stdout.contains("removed"),
        "unexpected remove output: {}",
        remove.stdout
    );

    // Verify removed
    let list = run_br(
        &workspace,
        ["label", "list", &id, "--json"],
        "list_after_remove",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);
    let labels_payload = extract_json_payload(&list.stdout);
    let labels: Vec<String> = serde_json::from_str(&labels_payload).expect("labels json");
    assert!(
        !labels.contains(&"bug".to_string()),
        "bug label should be removed"
    );
    assert!(
        labels.contains(&"urgent".to_string()),
        "urgent label should remain"
    );
}

/// Test 4: List all labels across issues
#[test]
fn e2e_label_list_all() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create multiple issues with different labels
    let create1 = run_br(&workspace, ["create", "Issue 1"], "create1");
    assert!(
        create1.status.success(),
        "create1 failed: {}",
        create1.stderr
    );
    let id1 = parse_created_id(&create1.stdout);

    let create2 = run_br(&workspace, ["create", "Issue 2"], "create2");
    assert!(
        create2.status.success(),
        "create2 failed: {}",
        create2.stderr
    );
    let id2 = parse_created_id(&create2.stdout);

    // Add labels
    run_br(&workspace, ["label", "add", &id1, "bug"], "add_bug1");
    run_br(&workspace, ["label", "add", &id1, "urgent"], "add_urgent1");
    run_br(
        &workspace,
        ["label", "add", &id2, "feature"],
        "add_feature2",
    );
    run_br(&workspace, ["label", "add", &id2, "urgent"], "add_urgent2");

    // List all unique labels
    let list_all = run_br(&workspace, ["label", "list-all", "--json"], "list_all");
    assert!(
        list_all.status.success(),
        "list-all failed: {}",
        list_all.stderr
    );
    let all_payload = extract_json_payload(&list_all.stdout);
    let label_counts: Vec<Value> = serde_json::from_str(&all_payload).expect("list-all json");

    // Should have 3 unique labels
    assert_eq!(label_counts.len(), 3, "expected 3 unique labels");

    // urgent should have count 2
    let urgent_count = label_counts
        .iter()
        .find(|lc| lc["label"] == "urgent")
        .map_or(0, |lc| lc["count"].as_u64().unwrap_or(0));
    assert_eq!(urgent_count, 2, "urgent label should have count 2");
}

/// Test 5: Add same label to multiple issues
#[test]
fn e2e_label_add_same_to_multiple_issues() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create issues
    let create1 = run_br(&workspace, ["create", "Issue A"], "create1");
    let create2 = run_br(&workspace, ["create", "Issue B"], "create2");
    let create3 = run_br(&workspace, ["create", "Issue C"], "create3");
    let id1 = parse_created_id(&create1.stdout);
    let id2 = parse_created_id(&create2.stdout);
    let id3 = parse_created_id(&create3.stdout);

    // Add same label to all
    let add1 = run_br(&workspace, ["label", "add", &id1, "shared-label"], "add1");
    let add2 = run_br(&workspace, ["label", "add", &id2, "shared-label"], "add2");
    let add3 = run_br(&workspace, ["label", "add", &id3, "shared-label"], "add3");
    assert!(add1.status.success(), "add1 failed: {}", add1.stderr);
    assert!(add2.status.success(), "add2 failed: {}", add2.stderr);
    assert!(add3.status.success(), "add3 failed: {}", add3.stderr);

    // Verify via list-all
    let list_all = run_br(&workspace, ["label", "list-all", "--json"], "list_all");
    assert!(
        list_all.status.success(),
        "list-all failed: {}",
        list_all.stderr
    );
    let all_payload = extract_json_payload(&list_all.stdout);
    let label_counts: Vec<Value> = serde_json::from_str(&all_payload).expect("list-all json");

    let shared_count = label_counts
        .iter()
        .find(|lc| lc["label"] == "shared-label")
        .map_or(0, |lc| lc["count"].as_u64().unwrap_or(0));
    assert_eq!(shared_count, 3, "shared-label should have count 3");
}

// =============================================================================
// Error Case Tests (6-8)
// =============================================================================

/// Test 6: Add label to non-existent issue → error
#[test]
fn e2e_label_add_nonexistent_issue_error() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Try to add label to non-existent issue
    let add = run_br(
        &workspace,
        ["label", "add", "nonexistent-id", "bug"],
        "add_nonexistent",
    );
    assert!(
        !add.status.success(),
        "should fail for nonexistent issue, stdout: {}, stderr: {}",
        add.stdout,
        add.stderr
    );
}

/// Test 7: Remove non-existent label → no-op (not error)
#[test]
fn e2e_label_remove_nonexistent_noop() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Test issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // Remove label that doesn't exist - should succeed (no-op)
    let remove = run_br(
        &workspace,
        ["label", "remove", &id, "nonexistent-label"],
        "remove_nonexistent",
    );
    assert!(
        remove.status.success(),
        "remove of nonexistent label should succeed as no-op: {}",
        remove.stderr
    );
    assert!(
        remove.stdout.contains("not found") || remove.stdout.contains("no-op"),
        "should indicate label not found: {}",
        remove.stdout
    );
}

/// Test 8: Invalid label format → error
#[test]
fn e2e_label_invalid_format_error() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Test issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // Try to add label with spaces (invalid)
    let add_space = run_br(&workspace, ["label", "add", &id, "has space"], "add_space");
    assert!(
        !add_space.status.success(),
        "label with space should fail: {}",
        add_space.stderr
    );

    // Try to add label with @ (invalid)
    let add_at = run_br(&workspace, ["label", "add", &id, "invalid@char"], "add_at");
    assert!(
        !add_at.status.success(),
        "label with @ should fail: {}",
        add_at.stderr
    );

    // Try to add empty label
    let add_empty = run_br(&workspace, ["label", "add", &id, ""], "add_empty");
    assert!(
        !add_empty.status.success(),
        "empty label should fail: {}",
        add_empty.stderr
    );
}

// =============================================================================
// Edge Case Tests (9-12)
// =============================================================================

/// Test 9: Label with special characters (allowed: dash, underscore, colon)
#[test]
fn e2e_label_special_characters() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Special char test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // Labels with allowed special characters
    let add_dash = run_br(
        &workspace,
        ["label", "add", &id, "high-priority"],
        "add_dash",
    );
    assert!(
        add_dash.status.success(),
        "dash label failed: {}",
        add_dash.stderr
    );

    let add_underscore = run_br(
        &workspace,
        ["label", "add", &id, "needs_review"],
        "add_underscore",
    );
    assert!(
        add_underscore.status.success(),
        "underscore label failed: {}",
        add_underscore.stderr
    );

    let add_colon = run_br(
        &workspace,
        ["label", "add", &id, "team:backend"],
        "add_colon",
    );
    assert!(
        add_colon.status.success(),
        "colon label failed: {}",
        add_colon.stderr
    );

    // Verify all present
    let list = run_br(&workspace, ["label", "list", &id, "--json"], "list");
    assert!(list.status.success(), "list failed: {}", list.stderr);
    let labels_payload = extract_json_payload(&list.stdout);
    let labels: Vec<String> = serde_json::from_str(&labels_payload).expect("labels json");
    assert!(labels.contains(&"high-priority".to_string()));
    assert!(labels.contains(&"needs_review".to_string()));
    assert!(labels.contains(&"team:backend".to_string()));
}

/// Test 10: Very long label name
#[test]
fn e2e_label_very_long_name() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Long label test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // Create a very long label (100 characters)
    let long_label = "a".repeat(100);
    let add = run_br(&workspace, ["label", "add", &id, &long_label], "add_long");
    assert!(add.status.success(), "long label failed: {}", add.stderr);

    // Verify it's stored
    let list = run_br(&workspace, ["label", "list", &id, "--json"], "list");
    assert!(list.status.success(), "list failed: {}", list.stderr);
    let labels_payload = extract_json_payload(&list.stdout);
    let labels: Vec<String> = serde_json::from_str(&labels_payload).expect("labels json");
    assert!(labels.contains(&long_label), "long label not found");
}

/// Test 11: Case sensitivity (bug vs BUG are different labels)
#[test]
fn e2e_label_case_sensitivity() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Case test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // Add both lowercase and uppercase versions
    let add_lower = run_br(&workspace, ["label", "add", &id, "bug"], "add_lower");
    assert!(
        add_lower.status.success(),
        "add lowercase failed: {}",
        add_lower.stderr
    );

    let add_upper = run_br(&workspace, ["label", "add", &id, "BUG"], "add_upper");
    assert!(
        add_upper.status.success(),
        "add uppercase failed: {}",
        add_upper.stderr
    );

    // Both should exist (case-sensitive)
    let list = run_br(&workspace, ["label", "list", &id, "--json"], "list");
    assert!(list.status.success(), "list failed: {}", list.stderr);
    let labels_payload = extract_json_payload(&list.stdout);
    let labels: Vec<String> = serde_json::from_str(&labels_payload).expect("labels json");
    assert!(
        labels.contains(&"bug".to_string()),
        "lowercase bug not found"
    );
    assert!(
        labels.contains(&"BUG".to_string()),
        "uppercase BUG not found"
    );
    assert_eq!(labels.len(), 2, "should have exactly 2 labels");
}

/// Test 12: Label on closed issue
#[test]
fn e2e_label_on_closed_issue() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Closeable issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // Close the issue
    let close = run_br(&workspace, ["close", &id], "close");
    assert!(close.status.success(), "close failed: {}", close.stderr);

    // Add label to closed issue - should work
    let add = run_br(
        &workspace,
        ["label", "add", &id, "archived"],
        "add_to_closed",
    );
    assert!(
        add.status.success(),
        "adding label to closed issue should work: {}",
        add.stderr
    );

    // Verify
    let list = run_br(&workspace, ["label", "list", &id, "--json"], "list_closed");
    assert!(list.status.success(), "list failed: {}", list.stderr);
    let labels_payload = extract_json_payload(&list.stdout);
    let labels: Vec<String> = serde_json::from_str(&labels_payload).expect("labels json");
    assert!(
        labels.contains(&"archived".to_string()),
        "label not added to closed issue"
    );
}

// =============================================================================
// Additional Tests
// =============================================================================

/// Test JSON output mode for label add
#[test]
fn e2e_label_add_json_output() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "JSON output test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // Add with JSON output
    let add = run_br(
        &workspace,
        ["label", "add", &id, "json-test", "--json"],
        "add_json",
    );
    assert!(add.status.success(), "add failed: {}", add.stderr);

    let payload = extract_json_payload(&add.stdout);
    let results: Vec<Value> = serde_json::from_str(&payload).expect("add json output");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["status"], "added");
    assert_eq!(results[0]["label"], "json-test");
}

/// Test adding duplicate label (should report "exists")
#[test]
fn e2e_label_add_duplicate() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Duplicate test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // Add label first time
    let add1 = run_br(&workspace, ["label", "add", &id, "dup", "--json"], "add1");
    assert!(add1.status.success(), "add1 failed: {}", add1.stderr);
    let payload1 = extract_json_payload(&add1.stdout);
    let results1: Vec<Value> = serde_json::from_str(&payload1).expect("add1 json");
    assert_eq!(results1[0]["status"], "added");

    // Add same label again
    let add2 = run_br(&workspace, ["label", "add", &id, "dup", "--json"], "add2");
    assert!(add2.status.success(), "add2 failed: {}", add2.stderr);
    let payload2 = extract_json_payload(&add2.stdout);
    let results2: Vec<Value> = serde_json::from_str(&payload2).expect("add2 json");
    assert_eq!(results2[0]["status"], "exists");
}

/// Test label rename across multiple issues
#[test]
fn e2e_label_rename() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create issues with same label
    let create1 = run_br(&workspace, ["create", "Issue 1"], "create1");
    let create2 = run_br(&workspace, ["create", "Issue 2"], "create2");
    let id1 = parse_created_id(&create1.stdout);
    let id2 = parse_created_id(&create2.stdout);

    run_br(&workspace, ["label", "add", &id1, "old-name"], "add1");
    run_br(&workspace, ["label", "add", &id2, "old-name"], "add2");

    // Rename label
    let rename = run_br(
        &workspace,
        ["label", "rename", "old-name", "new-name", "--json"],
        "rename",
    );
    assert!(rename.status.success(), "rename failed: {}", rename.stderr);
    let rename_payload = extract_json_payload(&rename.stdout);
    let rename_result: Value = serde_json::from_str(&rename_payload).expect("rename json");
    assert_eq!(rename_result["old_name"], "old-name");
    assert_eq!(rename_result["new_name"], "new-name");
    assert_eq!(rename_result["affected_issues"], 2);

    // Verify old label gone, new label present
    let list1 = run_br(&workspace, ["label", "list", &id1, "--json"], "list1");
    let labels1_payload = extract_json_payload(&list1.stdout);
    let labels1: Vec<String> = serde_json::from_str(&labels1_payload).expect("labels1 json");
    assert!(
        !labels1.contains(&"old-name".to_string()),
        "old-name should be gone"
    );
    assert!(
        labels1.contains(&"new-name".to_string()),
        "new-name should exist"
    );
}

/// Test label persistence in JSONL export
#[test]
fn e2e_label_persistence_jsonl() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Persistence test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // Add labels
    run_br(&workspace, ["label", "add", &id, "persisted"], "add");

    // Export to JSONL
    let export = run_br(&workspace, ["sync", "--flush-only"], "export");
    assert!(export.status.success(), "export failed: {}", export.stderr);

    // Read JSONL and verify labels
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let jsonl_content = std::fs::read_to_string(&jsonl_path).expect("read jsonl");

    // Find the line for our issue
    let issue_line = jsonl_content
        .lines()
        .find(|line| line.contains(&id))
        .expect("issue not found in jsonl");

    let issue_json: Value = serde_json::from_str(issue_line).expect("parse issue json");
    let labels = &issue_json["labels"];
    assert!(labels.is_array(), "labels should be array in jsonl");
    let label_arr: Vec<String> = serde_json::from_value(labels.clone()).unwrap();
    assert!(
        label_arr.contains(&"persisted".to_string()),
        "label not persisted in jsonl"
    );
}
