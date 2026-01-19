#![allow(clippy::similar_names)]

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

#[test]
fn e2e_relations_labels_comments() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let parent = run_br(&workspace, ["create", "Parent issue"], "create_parent");
    assert!(
        parent.status.success(),
        "parent create failed: {}",
        parent.stderr
    );
    let parent_id = parse_created_id(&parent.stdout);

    let child = run_br(&workspace, ["create", "Child issue"], "create_child");
    assert!(
        child.status.success(),
        "child create failed: {}",
        child.stderr
    );
    let child_id = parse_created_id(&child.stdout);

    let parent_args = vec![
        "update".to_string(),
        child_id.clone(),
        "--parent".to_string(),
        parent_id,
    ];
    let parent_update = run_br(&workspace, parent_args, "set_parent");
    assert!(
        parent_update.status.success(),
        "parent update failed: {}",
        parent_update.stderr
    );

    let label_args = vec![
        "update".to_string(),
        child_id.clone(),
        "--add-label".to_string(),
        "backend".to_string(),
    ];
    let label_update = run_br(&workspace, label_args, "add_label");
    assert!(
        label_update.status.success(),
        "label update failed: {}",
        label_update.stderr
    );

    let list = run_br(
        &workspace,
        ["list", "--label", "backend", "--json"],
        "list_label",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);
    let list_payload = extract_json_payload(&list.stdout);
    let list_json: Vec<Value> = serde_json::from_str(&list_payload).expect("list json");
    assert!(
        list_json.iter().any(|item| item["id"] == child_id),
        "labeled issue missing in list"
    );

    let comment_args = vec![
        "comments".to_string(),
        "add".to_string(),
        child_id.clone(),
        "First comment".to_string(),
    ];
    let comment = run_br(&workspace, comment_args, "add_comment");
    assert!(
        comment.status.success(),
        "comment add failed: {}",
        comment.stderr
    );

    let list_comments = run_br(
        &workspace,
        ["comments", "list", &child_id, "--json"],
        "list_comments",
    );
    assert!(
        list_comments.status.success(),
        "comment list failed: {}",
        list_comments.stderr
    );
    let comments_payload = extract_json_payload(&list_comments.stdout);
    let comments_json: Vec<Value> = serde_json::from_str(&comments_payload).expect("comments json");
    assert_eq!(comments_json.len(), 1);
    assert_eq!(comments_json[0]["text"], "First comment");
}

#[test]
fn e2e_dep_add_list_blocked_remove() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let blocking_issue = run_br(&workspace, ["create", "Blocker issue"], "create_blocker");
    assert!(
        blocking_issue.status.success(),
        "blocker create failed: {}",
        blocking_issue.stderr
    );
    let blocking_id = parse_created_id(&blocking_issue.stdout);

    let blocked_issue = run_br(&workspace, ["create", "Blocked issue"], "create_blocked");
    assert!(
        blocked_issue.status.success(),
        "blocked create failed: {}",
        blocked_issue.stderr
    );
    let blocked_id = parse_created_id(&blocked_issue.stdout);

    let dep_add = run_br(
        &workspace,
        ["dep", "add", &blocked_id, &blocking_id, "--json"],
        "dep_add",
    );
    assert!(
        dep_add.status.success(),
        "dep add failed: {}",
        dep_add.stderr
    );

    let list = run_br(
        &workspace,
        ["dep", "list", &blocked_id, "--json"],
        "dep_list",
    );
    assert!(list.status.success(), "dep list failed: {}", list.stderr);
    let list_payload = extract_json_payload(&list.stdout);
    let list_json: Vec<Value> = serde_json::from_str(&list_payload).expect("dep list json");
    assert!(
        list_json
            .iter()
            .any(|item| item["issue_id"] == blocked_id && item["depends_on_id"] == blocking_id),
        "dependency not listed"
    );

    let blocked_view = run_br(&workspace, ["blocked", "--json"], "blocked");
    assert!(
        blocked_view.status.success(),
        "blocked failed: {}",
        blocked_view.stderr
    );
    let blocked_payload = extract_json_payload(&blocked_view.stdout);
    let blocked_json: Vec<Value> = serde_json::from_str(&blocked_payload).expect("blocked json");
    assert!(
        blocked_json.iter().any(|item| item["id"] == blocked_id),
        "blocked issue missing from blocked list"
    );

    let dep_remove = run_br(
        &workspace,
        ["dep", "remove", &blocked_id, &blocking_id, "--json"],
        "dep_remove",
    );
    assert!(
        dep_remove.status.success(),
        "dep remove failed: {}",
        dep_remove.stderr
    );

    let blocked_view = run_br(&workspace, ["blocked", "--json"], "blocked_after");
    assert!(
        blocked_view.status.success(),
        "blocked after remove failed: {}",
        blocked_view.stderr
    );
    let blocked_payload = extract_json_payload(&blocked_view.stdout);
    let blocked_json: Vec<Value> = serde_json::from_str(&blocked_payload).expect("blocked json");
    assert!(
        !blocked_json.iter().any(|item| item["id"] == blocked_id),
        "blocked issue still present after dep remove"
    );
}

#[test]
fn e2e_dep_tree_external_nodes() {
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
    let external_config_path = external.root.join(".beads/config.yaml");
    fs::write(&external_config_path, "issue_prefix: bd\n").expect("write ext config");

    let config_path = workspace.root.join(".beads/config.yaml");
    let external_path = external.root.display();
    let config = format!("issue_prefix: bd\nexternal_projects:\n  extproj: \"{external_path}\"\n");
    fs::write(&config_path, config).expect("write config");

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

    let tree_before = run_br(
        &workspace,
        ["dep", "tree", &issue_id, "--json"],
        "dep_tree_before",
    );
    assert!(
        tree_before.status.success(),
        "dep tree before failed: {}",
        tree_before.stderr
    );
    let tree_payload = extract_json_payload(&tree_before.stdout);
    let nodes: Vec<Value> = serde_json::from_str(&tree_payload).expect("tree json");
    let external_node = nodes
        .iter()
        .find(|node| node["id"] == "external:extproj:auth")
        .expect("external node");
    assert_eq!(external_node["status"], "blocked");
    assert!(
        external_node["title"]
            .as_str()
            .unwrap_or("")
            .starts_with('⏳'),
        "external node should show pending marker"
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

    let tree_after = run_br(
        &workspace,
        ["dep", "tree", &issue_id, "--json"],
        "dep_tree_after",
    );
    assert!(
        tree_after.status.success(),
        "dep tree after failed: {}",
        tree_after.stderr
    );
    let tree_payload = extract_json_payload(&tree_after.stdout);
    let nodes: Vec<Value> = serde_json::from_str(&tree_payload).expect("tree json");
    let external_node = nodes
        .iter()
        .find(|node| node["id"] == "external:extproj:auth")
        .expect("external node");
    assert_eq!(external_node["status"], "closed");
    assert!(
        external_node["title"]
            .as_str()
            .unwrap_or("")
            .starts_with('✓'),
        "external node should show satisfied marker"
    );
}

#[test]
fn e2e_dep_list_external_nodes() {
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
    let external_config_path = external.root.join(".beads/config.yaml");
    fs::write(&external_config_path, "issue_prefix: bd\n").expect("write ext config");

    let config_path = workspace.root.join(".beads/config.yaml");
    let external_path = external.root.display();
    let config = format!("issue_prefix: bd\nexternal_projects:\n  extproj: \"{external_path}\"\n");
    fs::write(&config_path, config).expect("write config");

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

    let list_before = run_br(
        &workspace,
        ["dep", "list", &issue_id, "--json"],
        "dep_list_before",
    );
    assert!(
        list_before.status.success(),
        "dep list before failed: {}",
        list_before.stderr
    );
    let list_payload = extract_json_payload(&list_before.stdout);
    let list_json: Vec<Value> = serde_json::from_str(&list_payload).expect("dep list json");
    let external_entry = list_json
        .iter()
        .find(|item| item["depends_on_id"] == "external:extproj:auth")
        .expect("external dep entry");
    assert_eq!(external_entry["status"], "blocked");
    assert!(
        external_entry["title"]
            .as_str()
            .unwrap_or("")
            .starts_with('⏳'),
        "external dep should show pending marker"
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

    let list_after = run_br(
        &workspace,
        ["dep", "list", &issue_id, "--json"],
        "dep_list_after",
    );
    assert!(
        list_after.status.success(),
        "dep list after failed: {}",
        list_after.stderr
    );
    let list_payload = extract_json_payload(&list_after.stdout);
    let list_json: Vec<Value> = serde_json::from_str(&list_payload).expect("dep list json");
    let external_entry = list_json
        .iter()
        .find(|item| item["depends_on_id"] == "external:extproj:auth")
        .expect("external dep entry");
    assert_eq!(external_entry["status"], "closed");
    assert!(
        external_entry["title"]
            .as_str()
            .unwrap_or("")
            .starts_with('✓'),
        "external dep should show satisfied marker"
    );
}

#[test]
fn e2e_close_suggest_next_unblocks() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let blocker = run_br(&workspace, ["create", "Blocker issue"], "create_blocker");
    assert!(
        blocker.status.success(),
        "blocker create failed: {}",
        blocker.stderr
    );
    let blocker_id = parse_created_id(&blocker.stdout);

    let blocked = run_br(&workspace, ["create", "Blocked issue"], "create_blocked");
    assert!(
        blocked.status.success(),
        "blocked create failed: {}",
        blocked.stderr
    );
    let blocked_id = parse_created_id(&blocked.stdout);

    let dep_add = run_br(
        &workspace,
        ["dep", "add", &blocked_id, &blocker_id],
        "dep_add",
    );
    assert!(
        dep_add.status.success(),
        "dep add failed: {}",
        dep_add.stderr
    );

    let close = run_br(
        &workspace,
        ["close", &blocker_id, "--suggest-next", "--json"],
        "close_suggest_next",
    );
    assert!(close.status.success(), "close failed: {}", close.stderr);

    let payload = extract_json_payload(&close.stdout);
    let close_json: serde_json::Value = serde_json::from_str(&payload).expect("close json");
    let unblocked = close_json["unblocked"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        unblocked.iter().any(|item| item["id"] == blocked_id),
        "blocked issue not reported as unblocked"
    );
}

#[test]
fn e2e_close_blocked_requires_force() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let blocker = run_br(&workspace, ["create", "Blocker issue"], "create_blocker");
    assert!(
        blocker.status.success(),
        "blocker create failed: {}",
        blocker.stderr
    );
    let blocker_id = parse_created_id(&blocker.stdout);

    let blocked = run_br(&workspace, ["create", "Blocked issue"], "create_blocked");
    assert!(
        blocked.status.success(),
        "blocked create failed: {}",
        blocked.stderr
    );
    let blocked_id = parse_created_id(&blocked.stdout);

    let dep_add = run_br(
        &workspace,
        ["dep", "add", &blocked_id, &blocker_id],
        "dep_add",
    );
    assert!(
        dep_add.status.success(),
        "dep add failed: {}",
        dep_add.stderr
    );

    let close_skip = run_br(
        &workspace,
        ["close", &blocked_id, "--json"],
        "close_blocked_skip",
    );
    assert!(
        close_skip.status.success(),
        "close blocked failed: {}",
        close_skip.stderr
    );
    let payload = extract_json_payload(&close_skip.stdout);
    let close_json: Value = serde_json::from_str(&payload).expect("close json");
    let skipped = close_json["skipped"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        skipped.iter().any(|item| item["id"] == blocked_id),
        "blocked issue not skipped"
    );

    let close_force = run_br(
        &workspace,
        ["close", &blocked_id, "--force", "--json"],
        "close_blocked_force",
    );
    assert!(
        close_force.status.success(),
        "close force failed: {}",
        close_force.stderr
    );
    let payload = extract_json_payload(&close_force.stdout);
    let close_json: Value = serde_json::from_str(&payload).expect("close json");
    let closed = close_json["closed"].as_array().cloned().unwrap_or_default();
    assert!(
        closed.iter().any(|item| item["id"] == blocked_id),
        "blocked issue not closed with --force"
    );
}
