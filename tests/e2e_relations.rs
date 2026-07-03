#![allow(clippy::similar_names)]

mod common;

use common::cli::{BrWorkspace, extract_issues_array, extract_json_payload, run_br};
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
fn e2e_dep_cycles_default_hides_closed_archive_and_include_closed_exposes_it() {
    common::init_test_logging();
    info!("e2e_dep_cycles_default_hides_closed_archive_and_include_closed_exposes_it: starting");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_closed_archive_cycles");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let issue_a = run_br(&workspace, ["create", "Archived cycle A"], "create_a");
    assert!(
        issue_a.status.success(),
        "create A failed: {}",
        issue_a.stderr
    );
    let issue_a_id = parse_created_id(&issue_a.stdout);

    let issue_b = run_br(&workspace, ["create", "Archived cycle B"], "create_b");
    assert!(
        issue_b.status.success(),
        "create B failed: {}",
        issue_b.stderr
    );
    let issue_b_id = parse_created_id(&issue_b.stdout);

    let add_a_b = run_br(
        &workspace,
        ["dep", "add", &issue_a_id, &issue_b_id, "-t", "related"],
        "add_a_b_related",
    );
    assert!(
        add_a_b.status.success(),
        "add A->B failed: {}",
        add_a_b.stderr
    );
    let add_b_a = run_br(
        &workspace,
        ["dep", "add", &issue_b_id, &issue_a_id, "-t", "related"],
        "add_b_a_related",
    );
    assert!(
        add_b_a.status.success(),
        "add B->A failed: {}",
        add_b_a.stderr
    );

    let close_a = run_br(&workspace, ["close", &issue_a_id], "close_a");
    assert!(
        close_a.status.success(),
        "close A failed: {}",
        close_a.stderr
    );
    let close_b = run_br(&workspace, ["close", &issue_b_id], "close_b");
    assert!(
        close_b.status.success(),
        "close B failed: {}",
        close_b.stderr
    );

    let active_only = run_br(
        &workspace,
        ["dep", "cycles", "--json"],
        "cycles_active_only",
    );
    assert!(
        active_only.status.success(),
        "dep cycles active-only failed: {}",
        active_only.stderr
    );
    let active_payload: Value = serde_json::from_str(&extract_json_payload(&active_only.stdout))
        .expect("active cycles json");
    assert_eq!(active_payload["scope"], "active");
    assert_eq!(active_payload["include_closed"], false);
    assert_eq!(active_payload["count"], 0);
    assert_eq!(active_payload["active_count"], 0);
    assert_eq!(active_payload["archived_closed_count"], 1);
    assert_eq!(active_payload["total_count"], 1);
    assert_eq!(active_payload["cycles"].as_array().unwrap().len(), 0);
    assert!(active_payload.get("archived_closed_cycles").is_none());

    let with_archive = run_br(
        &workspace,
        ["dep", "cycles", "--json", "--include-closed"],
        "cycles_include_closed",
    );
    assert!(
        with_archive.status.success(),
        "dep cycles --include-closed failed: {}",
        with_archive.stderr
    );
    let archive_payload: Value = serde_json::from_str(&extract_json_payload(&with_archive.stdout))
        .expect("include-closed cycles json");
    assert_eq!(archive_payload["scope"], "active_and_archived");
    assert_eq!(archive_payload["include_closed"], true);
    assert_eq!(archive_payload["count"], 1);
    assert_eq!(archive_payload["active_count"], 0);
    assert_eq!(archive_payload["archived_closed_count"], 1);
    assert_eq!(archive_payload["total_count"], 1);
    assert_eq!(archive_payload["cycles"].as_array().unwrap().len(), 1);
    assert_eq!(
        archive_payload["archived_closed_cycles"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    info!(
        "e2e_dep_cycles_default_hides_closed_archive_and_include_closed_exposes_it: assertions passed"
    );
}

/// #368: an active dependency cycle must surface to scripted/robot callers via
/// a non-zero exit code (5 = CycleDetected) on both the text and JSON surfaces,
/// while the cycle data itself is still emitted on stdout.
#[test]
fn e2e_dep_cycles_active_cycle_exits_nonzero() {
    common::init_test_logging();
    info!("e2e_dep_cycles_active_cycle_exits_nonzero: starting");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_active_cycle");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let issue_a = run_br(&workspace, ["create", "Active cycle A"], "create_a");
    assert!(
        issue_a.status.success(),
        "create A failed: {}",
        issue_a.stderr
    );
    let issue_a_id = parse_created_id(&issue_a.stdout);

    let issue_b = run_br(&workspace, ["create", "Active cycle B"], "create_b");
    assert!(
        issue_b.status.success(),
        "create B failed: {}",
        issue_b.stderr
    );
    let issue_b_id = parse_created_id(&issue_b.stdout);

    // `related` edges can close a cycle without br refusing the edge (only
    // `blocks` cycles are rejected at insert time), leaving an active cycle the
    // detector reports.
    let add_a_b = run_br(
        &workspace,
        ["dep", "add", &issue_a_id, &issue_b_id, "-t", "related"],
        "add_a_b_related",
    );
    assert!(
        add_a_b.status.success(),
        "add A->B failed: {}",
        add_a_b.stderr
    );
    let add_b_a = run_br(
        &workspace,
        ["dep", "add", &issue_b_id, &issue_a_id, "-t", "related"],
        "add_b_a_related",
    );
    assert!(
        add_b_a.status.success(),
        "add B->A failed: {}",
        add_b_a.stderr
    );

    // JSON surface: cycle data preserved AND non-zero exit.
    let json = run_br(&workspace, ["dep", "cycles", "--json"], "cycles_json");
    assert_eq!(
        json.status.code(),
        Some(5),
        "dep cycles --json should exit 5 with an active cycle; stderr: {}",
        json.stderr
    );
    let payload: Value =
        serde_json::from_str(&extract_json_payload(&json.stdout)).expect("cycles json");
    assert_eq!(payload["count"], 1, "expected one reported cycle");
    assert_eq!(payload["active_count"], 1, "expected one active cycle");

    // Text surface: same non-zero exit.
    let text = run_br(&workspace, ["dep", "cycles"], "cycles_text");
    assert_eq!(
        text.status.code(),
        Some(5),
        "dep cycles should exit 5 with an active cycle; stderr: {}",
        text.stderr
    );

    info!("e2e_dep_cycles_active_cycle_exits_nonzero: assertions passed");
}

/// #368: when a bulk import's per-edge fallback drops a cycle-closing edge, the
/// issues are still created but the command exits non-zero so scripted callers
/// learn the imported graph differs from the declared spec.
#[test]
fn e2e_create_bulk_dropped_cycle_edge_exits_nonzero() {
    common::init_test_logging();
    info!("e2e_create_bulk_dropped_cycle_edge_exits_nonzero: starting");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_bulk_cycle");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let bulk = "## Task A\n\n### ID\na1\n\n### Type\ntask\n\n### Description\nFirst task.\n\n\
                ### Dependencies\nblocks: b1\n\n\
                ## Task B\n\n### ID\nb1\n\n### Type\ntask\n\n### Description\nSecond task.\n\n\
                ### Dependencies\nblocks: a1\n";
    let bulk_path = workspace.root.join("bulk.md");
    fs::write(&bulk_path, bulk).expect("write bulk.md");

    let created = run_br(
        &workspace,
        ["create", "-f", bulk_path.to_str().unwrap()],
        "bulk_create_cycle",
    );
    assert_eq!(
        created.status.code(),
        Some(5),
        "bulk create that dropped a cycle edge should exit 5; stderr: {}",
        created.stderr
    );

    // The issues themselves are still created despite the dropped edge.
    let list = run_br(&workspace, ["list", "--json"], "list_after_bulk");
    assert!(list.status.success(), "list failed: {}", list.stderr);
    let issues = extract_issues_array(&list.stdout);
    assert_eq!(
        issues.len(),
        2,
        "both issues should be created from the bulk file"
    );

    info!("e2e_create_bulk_dropped_cycle_edge_exits_nonzero: assertions passed");
}

#[test]
fn e2e_relations_labels_comments() {
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
    let list_json = extract_issues_array(&list.stdout);
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
    info!("e2e_relations_labels_comments: assertions passed");
}

#[test]
fn e2e_label_add_updates_last_touched_context() {
    common::init_test_logging();
    info!("e2e_label_add_updates_last_touched_context: starting");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_label_last_touched");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let target = run_br(
        &workspace,
        ["create", "Label target"],
        "create_label_target",
    );
    assert!(target.status.success(), "create failed: {}", target.stderr);
    let target_id = parse_created_id(&target.stdout);

    let other = run_br(&workspace, ["create", "Other issue"], "create_label_other");
    assert!(other.status.success(), "create failed: {}", other.stderr);
    let other_id = parse_created_id(&other.stdout);

    let label_add = run_br(
        &workspace,
        ["label", "add", &target_id, "triage", "--json"],
        "label_add_last_touched",
    );
    assert!(
        label_add.status.success(),
        "label add failed: {}",
        label_add.stderr
    );

    let update = run_br(
        &workspace,
        ["update", "--title", "Label-touched target", "--json"],
        "update_after_label_add_last_touched",
    );
    assert!(
        update.status.success(),
        "update without explicit id failed after label add: {}",
        update.stderr
    );

    let show_target = run_br(
        &workspace,
        ["show", &target_id, "--json"],
        "show_label_target",
    );
    assert!(
        show_target.status.success(),
        "show target failed: {}",
        show_target.stderr
    );
    let shown_target: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&show_target.stdout)).expect("target json");
    assert_eq!(shown_target[0]["title"], "Label-touched target");

    let show_other = run_br(
        &workspace,
        ["show", &other_id, "--json"],
        "show_label_other",
    );
    assert!(
        show_other.status.success(),
        "show other failed: {}",
        show_other.stderr
    );
    let shown_other: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&show_other.stdout)).expect("other json");
    assert_eq!(shown_other[0]["title"], "Other issue");
    info!("e2e_label_add_updates_last_touched_context: assertions passed");
}

#[test]
fn e2e_comments_add_updates_last_touched_context() {
    common::init_test_logging();
    info!("e2e_comments_add_updates_last_touched_context: starting");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_comments_last_touched");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let target = run_br(
        &workspace,
        ["create", "Comment target"],
        "create_comments_target",
    );
    assert!(target.status.success(), "create failed: {}", target.stderr);
    let target_id = parse_created_id(&target.stdout);

    let other = run_br(
        &workspace,
        ["create", "Other comments issue"],
        "create_comments_other",
    );
    assert!(other.status.success(), "create failed: {}", other.stderr);
    let other_id = parse_created_id(&other.stdout);

    let comment_add = run_br(
        &workspace,
        ["comments", "add", &target_id, "Context anchor", "--json"],
        "comments_add_last_touched",
    );
    assert!(
        comment_add.status.success(),
        "comments add failed: {}",
        comment_add.stderr
    );

    let update = run_br(
        &workspace,
        ["update", "--title", "Comment-touched target", "--json"],
        "update_after_comments_add_last_touched",
    );
    assert!(
        update.status.success(),
        "update without explicit id failed after comments add: {}",
        update.stderr
    );

    let show_target = run_br(
        &workspace,
        ["show", &target_id, "--json"],
        "show_comments_target",
    );
    assert!(
        show_target.status.success(),
        "show target failed: {}",
        show_target.stderr
    );
    let shown_target: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&show_target.stdout)).expect("target json");
    assert_eq!(shown_target[0]["title"], "Comment-touched target");

    let show_other = run_br(
        &workspace,
        ["show", &other_id, "--json"],
        "show_comments_other",
    );
    assert!(
        show_other.status.success(),
        "show other failed: {}",
        show_other.stderr
    );
    let shown_other: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&show_other.stdout)).expect("other json");
    assert_eq!(shown_other[0]["title"], "Other comments issue");
    info!("e2e_comments_add_updates_last_touched_context: assertions passed");
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
fn e2e_dep_add_updates_last_touched_context() {
    common::init_test_logging();
    info!("e2e_dep_add_updates_last_touched_context: starting");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let blocker = run_br(&workspace, ["create", "Blocker"], "create_blocker");
    assert!(
        blocker.status.success(),
        "create blocker failed: {}",
        blocker.stderr
    );
    let blocker_id = parse_created_id(&blocker.stdout);

    let blocked = run_br(&workspace, ["create", "Blocked"], "create_blocked");
    assert!(
        blocked.status.success(),
        "create blocked failed: {}",
        blocked.stderr
    );
    let blocked_id = parse_created_id(&blocked.stdout);

    let dep_add = run_br(
        &workspace,
        ["dep", "add", &blocked_id, &blocker_id, "--json"],
        "dep_add_last_touched",
    );
    assert!(
        dep_add.status.success(),
        "dep add failed: {}",
        dep_add.stderr
    );

    let update = run_br(
        &workspace,
        [
            "update",
            "--title",
            "Blocked renamed via last touched",
            "--json",
        ],
        "update_last_touched_after_dep_add",
    );
    assert!(
        update.status.success(),
        "update without explicit id failed after dep add: {}",
        update.stderr
    );

    let show = run_br(
        &workspace,
        ["show", &blocked_id, "--json"],
        "show_blocked_after_update",
    );
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let payload = extract_json_payload(&show.stdout);
    let json: Value = serde_json::from_str(&payload).expect("show json");
    assert_eq!(json[0]["title"], "Blocked renamed via last touched");
    info!("e2e_dep_add_updates_last_touched_context: assertions passed");
}

#[test]
fn e2e_dep_remove_updates_last_touched_context() {
    common::init_test_logging();
    info!("e2e_dep_remove_updates_last_touched_context: starting");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let blocker = run_br(&workspace, ["create", "Blocker"], "create_blocker");
    assert!(
        blocker.status.success(),
        "create blocker failed: {}",
        blocker.stderr
    );
    let blocker_id = parse_created_id(&blocker.stdout);

    let blocked = run_br(&workspace, ["create", "Blocked"], "create_blocked");
    assert!(
        blocked.status.success(),
        "create blocked failed: {}",
        blocked.stderr
    );
    let blocked_id = parse_created_id(&blocked.stdout);

    let dep_add = run_br(
        &workspace,
        ["dep", "add", &blocked_id, &blocker_id, "--json"],
        "dep_add_before_remove",
    );
    assert!(
        dep_add.status.success(),
        "dep add failed: {}",
        dep_add.stderr
    );

    let update_blocker = run_br(
        &workspace,
        [
            "update",
            &blocker_id,
            "--title",
            "Blocker renamed first",
            "--json",
        ],
        "update_blocker_first",
    );
    assert!(
        update_blocker.status.success(),
        "update blocker failed: {}",
        update_blocker.stderr
    );

    let dep_remove = run_br(
        &workspace,
        ["dep", "remove", &blocked_id, &blocker_id, "--json"],
        "dep_remove_last_touched",
    );
    assert!(
        dep_remove.status.success(),
        "dep remove failed: {}",
        dep_remove.stderr
    );

    let update = run_br(
        &workspace,
        [
            "update",
            "--title",
            "Blocked renamed after dep remove",
            "--json",
        ],
        "update_last_touched_after_dep_remove",
    );
    assert!(
        update.status.success(),
        "update without explicit id failed after dep remove: {}",
        update.stderr
    );

    let show = run_br(
        &workspace,
        ["show", &blocked_id, "--json"],
        "show_blocked_after_remove",
    );
    assert!(
        show.status.success(),
        "show blocked failed: {}",
        show.stderr
    );
    let blocked_payload = extract_json_payload(&show.stdout);
    let blocked_json: Value = serde_json::from_str(&blocked_payload).expect("show blocked json");
    assert_eq!(blocked_json[0]["title"], "Blocked renamed after dep remove");

    let show_blocker = run_br(
        &workspace,
        ["show", &blocker_id, "--json"],
        "show_blocker_final",
    );
    assert!(
        show_blocker.status.success(),
        "show blocker failed: {}",
        show_blocker.stderr
    );
    let blocker_payload = extract_json_payload(&show_blocker.stdout);
    let blocker_json: Value = serde_json::from_str(&blocker_payload).expect("show blocker json");
    assert_eq!(blocker_json[0]["title"], "Blocker renamed first");
    info!("e2e_dep_remove_updates_last_touched_context: assertions passed");
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
    assert!(
        !close_skip.status.success(),
        "close blocked should fail with nothing-to-do: {}",
        close_skip.stdout
    );
    let payload = extract_json_payload(&close_skip.stdout);
    let close_json: Value = serde_json::from_str(&payload).expect("close json");
    let closed = close_json["closed"].as_array().cloned().unwrap_or_default();
    let skipped = close_json["skipped"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        closed.is_empty(),
        "blocked issue should not close without --force"
    );
    assert_eq!(
        skipped.len(),
        1,
        "blocked close should report one skipped issue"
    );
    assert_eq!(skipped[0]["id"], blocked_id);
    assert!(
        skipped[0]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("blocked by:")),
        "blocked close should explain the dependency blocker"
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

#[test]
fn e2e_close_json_reports_closed_and_skipped_in_partial_batch() {
    common::init_test_logging();
    info!("e2e_close_json_reports_closed_and_skipped_in_partial_batch: starting");
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

    let independent = run_br(
        &workspace,
        ["create", "Independent issue"],
        "create_independent",
    );
    assert!(
        independent.status.success(),
        "independent create failed: {}",
        independent.stderr
    );
    let independent_id = parse_created_id(&independent.stdout);

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
        ["close", &blocked_id, &independent_id, "--json"],
        "close_partial_batch",
    );
    assert!(
        close.status.success(),
        "partial close should succeed: {}",
        close.stderr
    );

    let payload = extract_json_payload(&close.stdout);
    let close_json: Value = serde_json::from_str(&payload).expect("close json");
    let closed = close_json["closed"].as_array().cloned().unwrap_or_default();
    let skipped = close_json["skipped"]
        .as_array()
        .cloned()
        .unwrap_or_default();

    assert_eq!(
        closed.len(),
        1,
        "partial close should report one closed issue"
    );
    assert_eq!(closed[0]["id"], independent_id);
    assert_eq!(
        skipped.len(),
        1,
        "partial close should report one skipped issue"
    );
    assert_eq!(skipped[0]["id"], blocked_id);
    assert!(
        skipped[0]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("blocked by:")),
        "partial close should preserve skipped blocker details"
    );
    info!("e2e_close_json_reports_closed_and_skipped_in_partial_batch: assertions passed");
}

#[test]
fn e2e_close_honors_env_json_mode() {
    common::init_test_logging();
    info!("e2e_close_honors_env_json_mode: starting");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Close via env json", "--json"],
        "create_env_json_close",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let created: Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("create json");
    let issue_id = created["id"].as_str().expect("issue id");

    let close = common::cli::run_br_with_env(
        &workspace,
        ["close", issue_id],
        [("BR_OUTPUT_FORMAT", "json")],
        "close_env_json",
    );
    assert!(
        close.status.success(),
        "close with env json failed: {}",
        close.stderr
    );

    let payload = extract_json_payload(&close.stdout);
    let close_json: Value = serde_json::from_str(&payload).expect("close json");
    let closed = close_json.as_array().cloned().unwrap_or_default();
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0]["id"], issue_id);
    assert_eq!(closed[0]["status"], "closed");
    info!("e2e_close_honors_env_json_mode: assertions passed");
}
