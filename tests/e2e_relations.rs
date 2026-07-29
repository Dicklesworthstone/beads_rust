#![allow(clippy::similar_names)]

mod common;

use common::cli::{BrWorkspace, create_via_markdown, extract_json_payload, run_br};
use serde_json::Value;
use std::fs;
use tracing::info;

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

#[test]
fn e2e_relations_labels_comments() {
    // NOTE: the `comments` CLI subcommand was removed entirely (no
    // replacement surface), so the comments half of this test has been
    // dropped. The labels half survives (labels are still real,
    // filterable, exported data — only the mutation CLI moved to
    // markdown bulk-import), so this test is ported to exercise that.
    common::init_test_logging();
    info!("e2e_relations_labels_comments: starting");
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

    // Child issue is created with the "backend" label via markdown
    // bulk-import, since `update --add-label` no longer exists.
    let child_id = create_via_markdown(
        &workspace,
        "create_child",
        "Child issue",
        None,
        None,
        None,
        &["backend"],
    );

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
    info!("e2e_relations_labels_comments: assertions passed");
}

#[test]
fn e2e_dep_add_list_blocked_remove() {
    common::init_test_logging();
    info!("e2e_dep_add_list_blocked_remove: starting");
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
    info!("e2e_dep_add_list_blocked_remove: assertions passed");
}

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_dep_tree_external_nodes() {
    common::init_test_logging();
    info!("e2e_dep_tree_external_nodes: starting");
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
    fs::write(&external_config_path, "").expect("write ext config");

    let config_path = workspace.root.join(".beads/config.yaml");
    let external_path = external.root.display();
    let config = format!("external_projects:\n  extproj: \"{external_path}\"\n");
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

    // NOTE: labels can no longer be attached via `update --add-label` (the
    // CLI flag was removed; only markdown bulk-import can set labels at
    // creation time now). The "provides:auth" label is load-bearing here
    // (external dep resolution in src/storage/sqlite.rs matches on it), so
    // it must actually be set, not just decorative test setup.
    let provider_id = create_via_markdown(
        &external,
        "ext_create",
        "Provide auth",
        None,
        None,
        None,
        &["provides:auth"],
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
    info!("e2e_dep_tree_external_nodes: assertions passed");
}

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_dep_list_external_nodes() {
    common::init_test_logging();
    info!("e2e_dep_list_external_nodes: starting");
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
    fs::write(&external_config_path, "").expect("write ext config");

    let config_path = workspace.root.join(".beads/config.yaml");
    let external_path = external.root.display();
    let config = format!("external_projects:\n  extproj: \"{external_path}\"\n");
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

    // NOTE: labels can no longer be attached via `update --add-label` (the
    // CLI flag was removed; only markdown bulk-import can set labels at
    // creation time now). The "provides:auth" label is load-bearing here
    // (external dep resolution in src/storage/sqlite.rs matches on it), so
    // it must actually be set, not just decorative test setup.
    let provider_id = create_via_markdown(
        &external,
        "ext_create",
        "Provide auth",
        None,
        None,
        None,
        &["provides:auth"],
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
    info!("e2e_dep_list_external_nodes: assertions passed");
}

#[test]
fn e2e_close_suggest_next_unblocks() {
    common::init_test_logging();
    info!("e2e_close_suggest_next_unblocks: starting");
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
    info!("e2e_close_suggest_next_unblocks: assertions passed");
}

#[test]
fn e2e_close_blocked_requires_force() {
    common::init_test_logging();
    info!("e2e_close_blocked_requires_force: starting");
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
    // NOTE: `close` intentionally returns a non-zero exit code (with the
    // `NOTHING_TO_DO` error envelope on stderr) when every requested issue
    // was skipped, while still printing the (empty) closed-issues array on
    // stdout (see src/cli/commands/close.rs, commit e727f6c "add
    // NothingToDo exit code"). This is deliberate design, not a
    // regression, so the test asserts on that behavior instead of a
    // successful exit code.
    assert!(
        !close_skip.status.success(),
        "close of a blocked issue (no --force) should now fail with NOTHING_TO_DO"
    );
    assert!(
        close_skip.stderr.contains("NOTHING_TO_DO"),
        "expected NOTHING_TO_DO error on stderr, got: {}",
        close_skip.stderr
    );
    let payload = extract_json_payload(&close_skip.stdout);
    let close_json: Value = serde_json::from_str(&payload).expect("close json");
    let closed = close_json.as_array().cloned().unwrap_or_default();
    assert!(
        closed.is_empty(),
        "blocked issue should not close without --force"
    );

    let show = run_br(
        &workspace,
        ["show", &blocked_id, "--json"],
        "show_blocked_after_skip",
    );
    let payload = extract_json_payload(&show.stdout);
    let issues: Value = serde_json::from_str(&payload).expect("show json");
    assert_eq!(issues[0]["status"].as_str().unwrap(), "open");

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
    let closed = close_json.as_array().cloned().unwrap_or_default();
    assert!(
        closed.iter().any(|item| item["id"] == blocked_id),
        "blocked issue not closed with --force"
    );
    info!("e2e_close_blocked_requires_force: assertions passed");
}
