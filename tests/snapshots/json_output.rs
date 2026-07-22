use super::common::cli::run_br;
use super::{create_issue, init_workspace, normalize_json};
use insta::assert_json_snapshot;
use serde_json::Value;

#[test]
fn snapshot_list_json() {
    let workspace = init_workspace();
    create_issue(&workspace, "Issue one", "create_one");
    create_issue(&workspace, "Issue two", "create_two");

    let output = run_br(&workspace, ["list", "--json"], "list_json");
    assert!(
        output.status.success(),
        "list json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("list_json_output", normalize_json(&json));
}

#[test]
fn snapshot_show_json() {
    let workspace = init_workspace();
    let id = create_issue(&workspace, "Detailed issue", "create_detail");

    let output = run_br(&workspace, ["show", &id, "--json"], "show_json");
    assert!(
        output.status.success(),
        "show json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("show_json_output", normalize_json(&json));
}

#[test]
#[allow(clippy::similar_names)]
fn snapshot_blocked_json() {
    let workspace = init_workspace();

    // Create a dependency chain
    let blocker = create_issue(&workspace, "Blocker issue", "create_blocker_json");
    let blocked = create_issue(&workspace, "Blocked issue", "create_blocked_json");

    let _ = run_br(
        &workspace,
        ["dep", "add", &blocked, &blocker],
        "dep_add_json",
    );

    let output = run_br(&workspace, ["blocked", "--json"], "blocked_json");
    assert!(
        output.status.success(),
        "blocked json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("blocked_json_output", normalize_json(&json));
}

#[test]
fn snapshot_list_with_filters_json() {
    let workspace = init_workspace();
    let id1 = create_issue(&workspace, "Bug: Fix login", "create_bug_json");
    let id2 = create_issue(&workspace, "Feature: Add theme", "create_feature_json");

    // Update types
    let _ = run_br(
        &workspace,
        ["update", &id1, "--type", "bug"],
        "update_bug_json",
    );
    let _ = run_br(
        &workspace,
        ["update", &id2, "--type", "feature"],
        "update_feature_json",
    );

    // List only bugs
    let output = run_br(
        &workspace,
        ["list", "--type", "bug", "--json"],
        "list_bugs_json",
    );
    assert!(
        output.status.success(),
        "list bugs json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("list_filtered_json_output", normalize_json(&json));
}

#[test]
fn snapshot_stats_json() {
    let workspace = init_workspace();
    create_issue(&workspace, "Stats Issue", "create_stats");

    let output = run_br(&workspace, ["stats", "--json"], "stats_json");
    assert!(output.status.success());
    // Parse the JSON string into Value before passing to normalize_json
    let json: serde_json::Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("stats_json_output", normalize_json(&json));
}

#[test]
fn snapshot_create_json() {
    let workspace = init_workspace();

    let output = run_br(
        &workspace,
        [
            "create",
            "New feature request",
            "--type",
            "feature",
            "--priority",
            "1",
            "--json",
        ],
        "create_json",
    );
    assert!(
        output.status.success(),
        "create json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("create_json_output", normalize_json(&json));
}

#[test]
fn snapshot_update_json() {
    let workspace = init_workspace();
    let id = create_issue(&workspace, "Issue to update", "create_update");

    let output = run_br(
        &workspace,
        ["update", &id, "--status", "in_progress", "--json"],
        "update_json",
    );
    assert!(
        output.status.success(),
        "update json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("update_json_output", normalize_json(&json));
}

#[test]
fn snapshot_close_json() {
    let workspace = init_workspace();
    let id = create_issue(&workspace, "Issue to close", "create_close_json");

    let output = run_br(
        &workspace,
        ["close", &id, "--reason", "Done", "--json"],
        "close_json",
    );
    assert!(
        output.status.success(),
        "close json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("close_json_output", normalize_json(&json));
}

#[test]
fn snapshot_dep_list_json() {
    let workspace = init_workspace();
    let id1 = create_issue(&workspace, "Parent issue", "create_parent");
    let id2 = create_issue(&workspace, "Child issue", "create_child");

    // Add dependency
    let add = run_br(&workspace, ["dep", "add", &id2, &id1], "dep_add");
    assert!(add.status.success(), "dep add failed: {}", add.stderr);

    let output = run_br(&workspace, ["dep", "list", &id2, "--json"], "dep_list_json");
    assert!(
        output.status.success(),
        "dep list json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("dep_list_json_output", normalize_json(&json));
}

#[test]
fn snapshot_search_json() {
    let workspace = init_workspace();
    create_issue(&workspace, "Search target", "create_search_target");
    create_issue(&workspace, "Other issue", "create_search_other");

    let output = run_br(&workspace, ["search", "target", "--json"], "search_json");
    assert!(
        output.status.success(),
        "search json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("search_json_output", normalize_json(&json));
}

#[test]
fn snapshot_count_json() {
    let workspace = init_workspace();
    create_issue(&workspace, "Count one", "create_count_one");
    create_issue(&workspace, "Count two", "create_count_two");

    let output = run_br(&workspace, ["count", "--json"], "count_json");
    assert!(
        output.status.success(),
        "count json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("count_json_output", normalize_json(&json));
}

#[test]
fn snapshot_count_grouped_json() {
    let workspace = init_workspace();
    let id = create_issue(&workspace, "Grouped one", "create_grouped_one");
    let _ = run_br(
        &workspace,
        ["update", &id, "--status", "in_progress"],
        "update_grouped_one",
    );
    create_issue(&workspace, "Grouped two", "create_grouped_two");

    let output = run_br(
        &workspace,
        ["count", "--by", "status", "--json"],
        "count_grouped_json",
    );
    assert!(
        output.status.success(),
        "count grouped json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("count_grouped_json_output", normalize_json(&json));
}

#[test]
fn snapshot_stale_json() {
    let workspace = init_workspace();
    create_issue(&workspace, "Stale issue", "create_stale");

    let output = run_br(&workspace, ["stale", "--days", "0", "--json"], "stale_json");
    assert!(
        output.status.success(),
        "stale json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("stale_json_output", normalize_json(&json));
}

#[test]
fn snapshot_comments_json() {
    let workspace = init_workspace();
    let id = create_issue(&workspace, "Commented issue", "create_commented");

    let add = run_br(
        &workspace,
        ["comments", "add", &id, "First comment", "--json"],
        "comments_add_json",
    );
    assert!(
        add.status.success(),
        "comments add json failed: {}",
        add.stderr
    );

    let add_json: Value = serde_json::from_str(&add.stdout).expect("parse json");
    assert_json_snapshot!("comments_add_json_output", normalize_json(&add_json));

    let list = run_br(
        &workspace,
        ["comments", "list", &id, "--json"],
        "comments_list_json",
    );
    assert!(
        list.status.success(),
        "comments list json failed: {}",
        list.stderr
    );

    let list_json: Value = serde_json::from_str(&list.stdout).expect("parse json");
    assert_json_snapshot!("comments_list_json_output", normalize_json(&list_json));
}

#[test]
fn snapshot_label_json() {
    let workspace = init_workspace();
    let id = create_issue(&workspace, "Labeled issue", "create_labeled");

    let add = run_br(
        &workspace,
        ["label", "add", &id, "backend", "--json"],
        "label_add_json",
    );
    assert!(
        add.status.success(),
        "label add json failed: {}",
        add.stderr
    );

    let add_json: Value = serde_json::from_str(&add.stdout).expect("parse json");
    assert_json_snapshot!("label_add_json_output", normalize_json(&add_json));

    let list = run_br(
        &workspace,
        ["label", "list", &id, "--json"],
        "label_list_json",
    );
    assert!(
        list.status.success(),
        "label list json failed: {}",
        list.stderr
    );

    let list_json: Value = serde_json::from_str(&list.stdout).expect("parse json");
    assert_json_snapshot!("label_list_json_output", normalize_json(&list_json));

    let list_all = run_br(
        &workspace,
        ["label", "list-all", "--json"],
        "label_list_all_json",
    );
    assert!(
        list_all.status.success(),
        "label list-all json failed: {}",
        list_all.stderr
    );

    let list_all_json: Value = serde_json::from_str(&list_all.stdout).expect("parse json");
    assert_json_snapshot!("label_list_all_json_output", normalize_json(&list_all_json));
}

#[test]
fn snapshot_orphans_json() {
    let workspace = init_workspace();

    let output = run_br(&workspace, ["orphans", "--json"], "orphans_json");
    assert!(
        output.status.success(),
        "orphans json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("orphans_json_output", normalize_json(&json));
}

#[test]
fn snapshot_graph_json() {
    let workspace = init_workspace();
    let root = create_issue(&workspace, "Graph root", "create_graph_root");
    let child = create_issue(&workspace, "Graph child", "create_graph_child");

    let _ = run_br(
        &workspace,
        ["dep", "add", &child, &root],
        "graph_dep_add_json",
    );

    let output = run_br(&workspace, ["graph", &root, "--json"], "graph_json");
    assert!(
        output.status.success(),
        "graph json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("graph_json_output", normalize_json(&json));
}

// ============================================================================
// Edge Cases: Empty Results
// ============================================================================

#[test]
fn snapshot_list_empty_json() {
    let workspace = init_workspace();

    let output = run_br(&workspace, ["list", "--json"], "list_empty_json");
    assert!(
        output.status.success(),
        "list empty json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("list_empty_json_output", normalize_json(&json));
}

#[test]
fn snapshot_blocked_empty_json() {
    let workspace = init_workspace();

    let output = run_br(&workspace, ["blocked", "--json"], "blocked_empty_json");
    assert!(
        output.status.success(),
        "blocked empty json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("blocked_empty_json_output", normalize_json(&json));
}

#[test]
fn snapshot_search_no_match_json() {
    let workspace = init_workspace();
    create_issue(&workspace, "Existing issue", "create_for_search_miss");

    let output = run_br(
        &workspace,
        ["search", "nonexistent_xyz", "--json"],
        "search_no_match_json",
    );
    assert!(
        output.status.success(),
        "search no match json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("search_no_match_json_output", normalize_json(&json));
}

#[test]
fn snapshot_stale_empty_json() {
    let workspace = init_workspace();

    let output = run_br(
        &workspace,
        ["stale", "--days", "0", "--json"],
        "stale_empty_json",
    );
    assert!(
        output.status.success(),
        "stale empty json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("stale_empty_json_output", normalize_json(&json));
}

#[test]
fn snapshot_count_empty_json() {
    let workspace = init_workspace();

    let output = run_br(&workspace, ["count", "--json"], "count_empty_json");
    assert!(
        output.status.success(),
        "count empty json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("count_empty_json_output", normalize_json(&json));
}

// ============================================================================
// Ordering Guarantees
// ============================================================================

#[test]
fn snapshot_list_priority_ordering_json() {
    let workspace = init_workspace();

    // Create issues with different priorities (lower number = higher priority)
    let id_low = create_issue(&workspace, "Low priority task", "create_low_prio");
    let id_high = create_issue(&workspace, "High priority task", "create_high_prio");
    let id_crit = create_issue(&workspace, "Critical task", "create_crit_prio");

    let _ = run_br(
        &workspace,
        ["update", &id_low, "--priority", "3"],
        "set_low_prio",
    );
    let _ = run_br(
        &workspace,
        ["update", &id_high, "--priority", "1"],
        "set_high_prio",
    );
    let _ = run_br(
        &workspace,
        ["update", &id_crit, "--priority", "0"],
        "set_crit_prio",
    );

    let output = run_br(&workspace, ["list", "--json"], "list_priority_order_json");
    assert!(
        output.status.success(),
        "list priority ordering json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    let normalized = normalize_json(&json);
    assert_json_snapshot!("list_priority_ordering_json_output", normalized);

    // Also verify ordering programmatically: priorities should be ascending
    if let Value::Array(items) = &json {
        let priorities: Vec<i64> = items
            .iter()
            .filter_map(|item| item.get("priority").and_then(Value::as_i64))
            .collect();
        for window in priorities.windows(2) {
            assert!(
                window[0] <= window[1],
                "list ordering violated: P{} should come before P{}",
                window[0],
                window[1]
            );
        }
    }
}

// ============================================================================
// Multiple IDs / Complex Scenarios
// ============================================================================

#[test]
fn snapshot_show_multiple_ids_json() {
    let workspace = init_workspace();
    let id1 = create_issue(&workspace, "First detailed issue", "create_multi_1");
    let id2 = create_issue(&workspace, "Second detailed issue", "create_multi_2");

    let output = run_br(
        &workspace,
        ["show", &id1, &id2, "--json"],
        "show_multi_json",
    );
    assert!(
        output.status.success(),
        "show multiple ids json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    let normalized = normalize_json(&json);
    assert_json_snapshot!("show_multiple_ids_json_output", normalized);

    // Verify we got exactly 2 results
    if let Value::Array(items) = &json {
        assert_eq!(items.len(), 2, "show with 2 IDs should return 2 results");
    }
}

#[test]
fn snapshot_count_grouped_by_type_json() {
    let workspace = init_workspace();
    let id1 = create_issue(&workspace, "Bug to fix", "create_typed_bug");
    let id2 = create_issue(&workspace, "Feature to add", "create_typed_feature");
    create_issue(&workspace, "Plain task", "create_typed_task");

    let _ = run_br(
        &workspace,
        ["update", &id1, "--type", "bug"],
        "set_type_bug",
    );
    let _ = run_br(
        &workspace,
        ["update", &id2, "--type", "feature"],
        "set_type_feature",
    );

    let output = run_br(
        &workspace,
        ["count", "--by", "type", "--json"],
        "count_by_type_json",
    );
    assert!(
        output.status.success(),
        "count grouped by type json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("count_grouped_by_type_json_output", normalize_json(&json));
}

#[test]
fn snapshot_count_grouped_by_priority_json() {
    let workspace = init_workspace();
    let id1 = create_issue(&workspace, "Critical item", "create_prio_p0");
    let id2 = create_issue(&workspace, "Normal item", "create_prio_p2");
    create_issue(&workspace, "Default item", "create_prio_default");

    let _ = run_br(
        &workspace,
        ["update", &id1, "--priority", "0"],
        "set_prio_p0",
    );
    let _ = run_br(
        &workspace,
        ["update", &id2, "--priority", "3"],
        "set_prio_p3",
    );

    let output = run_br(
        &workspace,
        ["count", "--by", "priority", "--json"],
        "count_by_priority_json",
    );
    assert!(
        output.status.success(),
        "count grouped by priority json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!(
        "count_grouped_by_priority_json_output",
        normalize_json(&json)
    );
}

#[test]
fn snapshot_graph_all_json() {
    let workspace = init_workspace();
    let root1 = create_issue(&workspace, "Graph root A", "create_graph_root_a");
    let child1 = create_issue(&workspace, "Graph child of A", "create_graph_child_a");
    let root2 = create_issue(&workspace, "Graph root B", "create_graph_root_b");

    let _ = run_br(
        &workspace,
        ["dep", "add", &child1, &root1],
        "graph_all_dep_add",
    );

    // graph --all shows all roots
    let output = run_br(&workspace, ["graph", "--all", "--json"], "graph_all_json");
    assert!(
        output.status.success(),
        "graph all json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("graph_all_json_output", normalize_json(&json));

    // Suppress unused variable warning
    let _ = root2;
}
