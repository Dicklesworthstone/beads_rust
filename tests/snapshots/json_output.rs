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
fn snapshot_ready_json() {
    let workspace = init_workspace();
    create_issue(&workspace, "Ready issue", "create_ready");

    let output = run_br(&workspace, ["ready", "--json"], "ready_json");
    assert!(
        output.status.success(),
        "ready json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("ready_json_output", normalize_json(&json));
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

    let output = run_br(
        &workspace,
        ["stale", "--days", "0", "--json"],
        "stale_json",
    );
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
    assert_json_snapshot!(
        "label_list_all_json_output",
        normalize_json(&list_all_json)
    );
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
