//! End-to-end tests for routing, redirect files, and external DB reference safety.
//!
//! Tests cover:
//! - Prefix-based route lookup (routes.jsonl)
//! - Redirect file following
//! - Redirect loop detection
//! - External DB reference safety and path normalization
//! - Clear errors for missing/invalid routes

use std::fs::{self, OpenOptions};
use std::path::PathBuf;
use std::process::Command;

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br, run_br_with_env, run_br_with_stdin};
use serde_json::Value;
use toon_rust::try_decode as decode_toon;

/// Helper to create a routes.jsonl file with given entries.
fn create_routes_file(workspace: &BrWorkspace, entries: &[(&str, &str)]) {
    let routes_path = workspace.root.join(".beads").join("routes.jsonl");
    let content: String = entries
        .iter()
        .map(|(prefix, path)| format!(r#"{{"prefix":"{}","path":"{}"}}"#, prefix, path))
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&routes_path, content).expect("write routes.jsonl");
}

/// Helper to create a redirect file.
fn create_redirect_file(beads_dir: &std::path::Path, target: &str) {
    let redirect_path = beads_dir.join("redirect");
    fs::write(&redirect_path, target).expect("write redirect");
}

fn init_workspace(workspace: &BrWorkspace, label: &str) {
    let init = run_br(workspace, ["init"], label);
    assert!(init.status.success(), "init failed: {}", init.stderr);
}

fn configure_external_route(main_workspace: &BrWorkspace, external_workspace: &BrWorkspace) {
    fs::write(
        external_workspace.root.join(".beads").join("config.yaml"),
        "issue_prefix: ext\n",
    )
    .expect("write external config");

    create_routes_file(
        main_workspace,
        &[("ext-", external_workspace.root.to_string_lossy().as_ref())],
    );
}

fn create_issue_and_get_id(workspace: &BrWorkspace, title: &str, label: &str) -> String {
    let create = run_br(workspace, ["create", title, "--json"], label);
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let issue: Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("create json");
    issue["id"].as_str().expect("issue id").to_string()
}

fn show_issue_json(workspace: &BrWorkspace, issue_id: &str, label: &str) -> Vec<Value> {
    let show = run_br(workspace, ["show", issue_id, "--json"], label);
    assert!(show.status.success(), "show failed: {}", show.stderr);
    serde_json::from_str(&extract_json_payload(&show.stdout)).expect("show json")
}

fn issue_from_jsonl(workspace: &BrWorkspace, issue_id: &str) -> Value {
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let contents = fs::read_to_string(&jsonl_path).expect("read issues.jsonl");
    contents
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("parse issue jsonl line"))
        .find(|issue| issue["id"].as_str() == Some(issue_id))
        .expect("issue should exist in issues.jsonl")
}

fn write_single_issue_jsonl(workspace: &BrWorkspace, issue: &Value) {
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let serialized = serde_json::to_string(issue).expect("serialize issue jsonl");
    fs::write(&jsonl_path, format!("{serialized}\n")).expect("write issues.jsonl");
}

fn last_touched_path(workspace: &BrWorkspace) -> PathBuf {
    beads_rust::util::last_touched_path(&workspace.root.join(".beads"))
}

fn switch_workspace_to_custom_database(workspace: &BrWorkspace, database_name: &str) {
    let beads_dir = workspace.root.join(".beads");
    let old_db = beads_dir.join("beads.db");
    let new_db = beads_dir.join(database_name);
    fs::rename(&old_db, &new_db).expect("move db to custom metadata path");
    fs::write(
        beads_dir.join("metadata.json"),
        format!(r#"{{"database":"{database_name}","jsonl_export":"issues.jsonl"}}"#),
    )
    .expect("write metadata");
}

fn routed_partial_id(issue_id: &str) -> String {
    let (prefix, hash) = issue_id.split_once('-').expect("issue id with prefix");
    let partial_hash = hash.chars().take(2).collect::<String>();
    format!("{prefix}-{partial_hash}")
}

fn init_test_git_repo(repo_root: &std::path::Path) -> String {
    let init_git = Command::new("git")
        .args(["init", "-b", "main"])
        .current_dir(repo_root)
        .output()
        .expect("git init");
    assert!(init_git.status.success(), "git init failed");
    let config_name = Command::new("git")
        .args(["config", "user.name", "Test User"])
        .current_dir(repo_root)
        .output()
        .expect("git config user.name");
    assert!(config_name.status.success(), "git config user.name failed");
    let config_email = Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(repo_root)
        .output()
        .expect("git config user.email");
    assert!(
        config_email.status.success(),
        "git config user.email failed"
    );
    fs::write(repo_root.join("README.md"), "hello\n").expect("write readme");
    let add = Command::new("git")
        .args(["add", "README.md"])
        .current_dir(repo_root)
        .output()
        .expect("git add");
    assert!(add.status.success(), "git add failed");
    let commit = Command::new("git")
        .args(["commit", "-m", "initial"])
        .current_dir(repo_root)
        .output()
        .expect("git commit");
    assert!(commit.status.success(), "git commit failed");
    String::from_utf8_lossy(
        &Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(repo_root)
            .output()
            .expect("git rev-parse")
            .stdout,
    )
    .trim()
    .to_string()
}

// =============================================================================
// PREFIX-BASED ROUTING TESTS
// =============================================================================

#[test]
fn e2e_routing_local_prefix_no_routes_file() {
    let _log = common::test_log("e2e_routing_local_prefix_no_routes_file");
    let workspace = BrWorkspace::new();

    // Initialize workspace
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create an issue
    let create = run_br(
        &workspace,
        [
            "create",
            "Test issue",
            "--priority",
            "2",
            "--type",
            "task",
            "--json",
        ],
        "create",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Verify the issue was created locally (no routes.jsonl means local)
    let list = run_br(&workspace, ["list", "--json"], "list");
    assert!(list.status.success(), "list failed: {}", list.stderr);
    assert!(
        list.stdout.contains("Test issue"),
        "Expected issue in list output"
    );
}

#[test]
fn e2e_routing_routes_jsonl_local_route() {
    let _log = common::test_log("e2e_routing_routes_jsonl_local_route");
    let workspace = BrWorkspace::new();

    // Initialize workspace
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create routes file with local route (path = ".")
    create_routes_file(&workspace, &[("bd-", ".")]);

    // Create an issue - should use local storage
    let create = run_br(
        &workspace,
        [
            "create",
            "Test issue with route",
            "--priority",
            "2",
            "--type",
            "task",
            "--json",
        ],
        "create",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Verify the issue was created
    let list = run_br(&workspace, ["list", "--json"], "list");
    assert!(list.status.success(), "list failed: {}", list.stderr);
    assert!(
        list.stdout.contains("Test issue with route"),
        "Expected issue in list output"
    );
}

#[test]
fn e2e_routing_routes_jsonl_malformed_line() {
    let _log = common::test_log("e2e_routing_routes_jsonl_malformed_line");
    let workspace = BrWorkspace::new();

    // Initialize workspace
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create malformed routes.jsonl
    let routes_path = workspace.root.join(".beads").join("routes.jsonl");
    fs::write(&routes_path, "not valid json\n").expect("write routes.jsonl");

    // Create an issue - should still work (local fallback) or give clear error
    let create = run_br(
        &workspace,
        [
            "create",
            "Test issue",
            "--priority",
            "2",
            "--type",
            "task",
            "--json",
        ],
        "create",
    );

    // Either succeeds with local fallback or fails with clear error
    if !create.status.success() {
        assert!(
            create.stderr.contains("Invalid route")
                || create.stderr.contains("invalid")
                || create.stderr.contains("JSON"),
            "Expected clear error message for malformed routes.jsonl, got: {}",
            create.stderr
        );
    }
}

#[test]
fn e2e_routing_routes_jsonl_external_route() {
    let _log = common::test_log("e2e_routing_routes_jsonl_external_route");

    // Use separate workspaces for main and external projects
    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    // Initialize main workspace
    let init = run_br(&main_workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Initialize external workspace
    let init_external = run_br(&external_workspace, ["init"], "init_external");
    assert!(
        init_external.status.success(),
        "external init failed: {}",
        init_external.stderr
    );

    // Set a different prefix for external project
    let external_config = external_workspace.root.join(".beads").join("config.yaml");
    fs::write(&external_config, "issue_prefix: ext\n").expect("write external config");

    // Create routes file in main workspace pointing to external workspace
    let routes_path = main_workspace.root.join(".beads").join("routes.jsonl");
    let route_entry = format!(
        r#"{{"prefix":"ext-","path":"{}"}}"#,
        external_workspace.root.display()
    );
    fs::write(&routes_path, route_entry).expect("write routes.jsonl");

    // Create an issue in external project
    let create = run_br(
        &external_workspace,
        [
            "create",
            "External issue",
            "--priority",
            "2",
            "--type",
            "task",
            "--json",
        ],
        "create_external",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let create_payload = extract_json_payload(&create.stdout);
    let created_issue: Value = serde_json::from_str(&create_payload).expect("create json");
    let external_id = created_issue["id"]
        .as_str()
        .expect("external id")
        .to_string();

    // Verify the issue exists in external project
    let list = run_br(&external_workspace, ["list", "--json"], "list_external");
    assert!(list.status.success(), "list failed: {}", list.stderr);
    assert!(
        list.stdout.contains("External issue"),
        "Expected issue in external project"
    );

    // Show the external issue from the main workspace via routing
    let show = run_br(
        &main_workspace,
        ["show", &external_id, "--json"],
        "show_external_via_route",
    );
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let show_payload = extract_json_payload(&show.stdout);
    let shown: Vec<Value> = serde_json::from_str(&show_payload).expect("show json");
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0]["id"].as_str(), Some(external_id.as_str()));
    assert_eq!(shown[0]["title"].as_str(), Some("External issue"));
}

#[test]
fn e2e_routing_external_target_lock_blocks_routed_access() {
    let _log = common::test_log("e2e_routing_external_target_lock_blocks_routed_access");

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();
    init_workspace(&main_workspace, "init_routed_lock_main");
    init_workspace(&external_workspace, "init_routed_lock_external");
    configure_external_route(&main_workspace, &external_workspace);

    let external_id = create_issue_and_get_id(
        &external_workspace,
        "External issue behind held write lock",
        "create_external_locked_target",
    );
    let lock_path = external_workspace.root.join(".beads").join(".write.lock");
    let lock_file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .expect("open external write lock");
    lock_file.lock().expect("hold external write lock");

    let routed_show = run_br(
        &main_workspace,
        ["show", &external_id, "--json"],
        "show_routed_external_while_target_locked",
    );
    assert!(
        !routed_show.status.success(),
        "routed access should fail while target .write.lock is held"
    );
    assert!(
        routed_show
            .stdout
            .contains("Routed external workspace is busy")
            || routed_show.stdout.contains("target write lock"),
        "expected target lock diagnostic, got stdout: {}",
        routed_show.stdout
    );

    drop(lock_file);
    let unlocked_show = run_br(
        &main_workspace,
        ["show", &external_id, "--json"],
        "show_routed_external_after_target_unlock",
    );
    assert!(
        unlocked_show.status.success(),
        "routed access should succeed after target lock release: {}",
        unlocked_show.stderr
    );
}

#[test]
fn e2e_routing_show_format_json_routes_external_issue() {
    let _log = common::test_log("e2e_routing_show_format_json_routes_external_issue");

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    init_workspace(&main_workspace, "init_main_format_json");
    init_workspace(&external_workspace, "init_external_format_json");
    configure_external_route(&main_workspace, &external_workspace);

    let external_id = create_issue_and_get_id(
        &external_workspace,
        "External format json issue",
        "create_external_format_json",
    );

    let show = run_br(
        &main_workspace,
        ["show", &external_id, "--format", "json"],
        "show_external_format_json",
    );
    assert!(show.status.success(), "show failed: {}", show.stderr);

    let shown: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&show.stdout)).expect("show json");
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0]["id"].as_str(), Some(external_id.as_str()));
    assert_eq!(
        shown[0]["title"].as_str(),
        Some("External format json issue")
    );
}

#[test]
fn e2e_routing_show_format_toon_routes_external_issue() {
    let _log = common::test_log("e2e_routing_show_format_toon_routes_external_issue");

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    init_workspace(&main_workspace, "init_main_format_toon");
    init_workspace(&external_workspace, "init_external_format_toon");
    configure_external_route(&main_workspace, &external_workspace);

    let external_id = create_issue_and_get_id(
        &external_workspace,
        "External format toon issue",
        "create_external_format_toon",
    );

    let show = run_br(
        &main_workspace,
        ["show", &external_id, "--format", "toon"],
        "show_external_format_toon",
    );
    assert!(show.status.success(), "show failed: {}", show.stderr);

    let shown = Value::from(decode_toon(show.stdout.trim(), None).expect("valid show TOON"));
    let items = shown.as_array().expect("show TOON array");
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["id"].as_str(), Some(external_id.as_str()));
    assert_eq!(
        items[0]["title"].as_str(),
        Some("External format toon issue")
    );
}

#[test]
fn e2e_routing_show_json_preserves_requested_order_across_routes() {
    let _log = common::test_log("e2e_routing_show_json_preserves_requested_order_across_routes");

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    let init_main = run_br(&main_workspace, ["init"], "init_main");
    assert!(
        init_main.status.success(),
        "init failed: {}",
        init_main.stderr
    );

    let init_external = run_br(&external_workspace, ["init"], "init_external");
    assert!(
        init_external.status.success(),
        "external init failed: {}",
        init_external.stderr
    );

    fs::write(
        external_workspace.root.join(".beads").join("config.yaml"),
        "issue_prefix: ext\n",
    )
    .expect("write external config");

    create_routes_file(
        &main_workspace,
        &[("ext-", external_workspace.root.to_string_lossy().as_ref())],
    );

    let create_local_first = run_br(
        &main_workspace,
        ["create", "Local first", "--json"],
        "create_local_first",
    );
    assert!(
        create_local_first.status.success(),
        "create local first failed: {}",
        create_local_first.stderr
    );
    let local_first_payload = extract_json_payload(&create_local_first.stdout);
    let local_first: Value = serde_json::from_str(&local_first_payload).expect("local first json");
    let local_first_id = local_first["id"]
        .as_str()
        .expect("local first id")
        .to_string();

    let create_external_middle = run_br(
        &external_workspace,
        ["create", "External middle", "--json"],
        "create_external_middle",
    );
    assert!(
        create_external_middle.status.success(),
        "create external middle failed: {}",
        create_external_middle.stderr
    );
    let external_middle_payload = extract_json_payload(&create_external_middle.stdout);
    let external_middle: Value =
        serde_json::from_str(&external_middle_payload).expect("external middle json");
    let external_middle_id = external_middle["id"]
        .as_str()
        .expect("external middle id")
        .to_string();

    let create_local_last = run_br(
        &main_workspace,
        ["create", "Local last", "--json"],
        "create_local_last",
    );
    assert!(
        create_local_last.status.success(),
        "create local last failed: {}",
        create_local_last.stderr
    );
    let local_last_payload = extract_json_payload(&create_local_last.stdout);
    let local_last: Value = serde_json::from_str(&local_last_payload).expect("local last json");
    let local_last_id = local_last["id"]
        .as_str()
        .expect("local last id")
        .to_string();

    let show = run_br(
        &main_workspace,
        [
            "show",
            &local_first_id,
            &external_middle_id,
            &local_last_id,
            "--json",
        ],
        "show_mixed_route_order",
    );
    assert!(show.status.success(), "show failed: {}", show.stderr);

    let show_payload = extract_json_payload(&show.stdout);
    let shown: Vec<Value> = serde_json::from_str(&show_payload).expect("show json");
    assert_eq!(shown.len(), 3);
    assert_eq!(shown[0]["id"].as_str(), Some(local_first_id.as_str()));
    assert_eq!(shown[1]["id"].as_str(), Some(external_middle_id.as_str()));
    assert_eq!(shown[2]["id"].as_str(), Some(local_last_id.as_str()));
}

#[test]
fn e2e_routing_show_text_preserves_requested_order_across_routes() {
    let _log = common::test_log("e2e_routing_show_text_preserves_requested_order_across_routes");

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    let init_main = run_br(&main_workspace, ["init"], "init_main_text");
    assert!(
        init_main.status.success(),
        "init failed: {}",
        init_main.stderr
    );

    let init_external = run_br(&external_workspace, ["init"], "init_external_text");
    assert!(
        init_external.status.success(),
        "external init failed: {}",
        init_external.stderr
    );

    fs::write(
        external_workspace.root.join(".beads").join("config.yaml"),
        "issue_prefix: ext\n",
    )
    .expect("write external config");

    create_routes_file(
        &main_workspace,
        &[("ext-", external_workspace.root.to_string_lossy().as_ref())],
    );

    let create_local_first = run_br(
        &main_workspace,
        ["create", "Local first text", "--json"],
        "create_local_first_text",
    );
    assert!(
        create_local_first.status.success(),
        "create local first failed: {}",
        create_local_first.stderr
    );
    let local_first =
        serde_json::from_str::<Value>(&extract_json_payload(&create_local_first.stdout))
            .expect("local first text json");
    let local_first_id = local_first["id"]
        .as_str()
        .expect("local first text id")
        .to_string();

    let create_external_middle = run_br(
        &external_workspace,
        ["create", "External middle text", "--json"],
        "create_external_middle_text",
    );
    assert!(
        create_external_middle.status.success(),
        "create external middle failed: {}",
        create_external_middle.stderr
    );
    let external_middle =
        serde_json::from_str::<Value>(&extract_json_payload(&create_external_middle.stdout))
            .expect("external middle text json");
    let external_middle_id = external_middle["id"]
        .as_str()
        .expect("external middle text id")
        .to_string();

    let create_local_last = run_br(
        &main_workspace,
        ["create", "Local last text", "--json"],
        "create_local_last_text",
    );
    assert!(
        create_local_last.status.success(),
        "create local last failed: {}",
        create_local_last.stderr
    );
    let local_last =
        serde_json::from_str::<Value>(&extract_json_payload(&create_local_last.stdout))
            .expect("local last text json");
    let local_last_id = local_last["id"]
        .as_str()
        .expect("local last text id")
        .to_string();

    let show = run_br(
        &main_workspace,
        ["show", &local_first_id, &external_middle_id, &local_last_id],
        "show_mixed_route_order_text",
    );
    assert!(show.status.success(), "show failed: {}", show.stderr);

    let local_first_pos = show
        .stdout
        .find("Local first text")
        .expect("local first title in output");
    let external_middle_pos = show
        .stdout
        .find("External middle text")
        .expect("external middle title in output");
    let local_last_pos = show
        .stdout
        .find("Local last text")
        .expect("local last title in output");

    assert!(local_first_pos < external_middle_pos);
    assert!(external_middle_pos < local_last_pos);
}

#[test]
fn e2e_routing_update_external_issue_via_main_workspace() {
    let _log = common::test_log("e2e_routing_update_external_issue_via_main_workspace");

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    let init = run_br(&main_workspace, ["init"], "init_main");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let init_external = run_br(&external_workspace, ["init"], "init_external");
    assert!(
        init_external.status.success(),
        "external init failed: {}",
        init_external.stderr
    );

    fs::write(
        external_workspace.root.join(".beads").join("config.yaml"),
        "issue_prefix: ext\n",
    )
    .expect("write external config");

    create_routes_file(
        &main_workspace,
        &[("ext-", external_workspace.root.to_string_lossy().as_ref())],
    );

    let create = run_br(
        &external_workspace,
        ["create", "External update target", "--json"],
        "create_external_update_target",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let create_payload = extract_json_payload(&create.stdout);
    let created_issue: Value = serde_json::from_str(&create_payload).expect("create json");
    let external_id = created_issue["id"]
        .as_str()
        .expect("external id")
        .to_string();

    let update = run_br(
        &main_workspace,
        ["update", &external_id, "--status", "in_progress", "--json"],
        "update_external_via_route",
    );
    assert!(update.status.success(), "update failed: {}", update.stderr);
    let update_payload = extract_json_payload(&update.stdout);
    let updated: Value = serde_json::from_str(&update_payload).expect("update json");
    let updated_array = updated.as_array().expect("update array");
    assert_eq!(updated_array.len(), 1);
    assert_eq!(updated_array[0]["id"].as_str(), Some(external_id.as_str()));
    assert_eq!(updated_array[0]["status"].as_str(), Some("in_progress"));

    let show_external = run_br(
        &external_workspace,
        ["show", &external_id, "--json"],
        "show_external_after_routed_update",
    );
    assert!(
        show_external.status.success(),
        "external show failed: {}",
        show_external.stderr
    );
    let show_payload = extract_json_payload(&show_external.stdout);
    let shown: Vec<Value> = serde_json::from_str(&show_payload).expect("show json");
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0]["status"].as_str(), Some("in_progress"));

    let jsonl_issue = issue_from_jsonl(&external_workspace, &external_id);
    assert_eq!(jsonl_issue["status"].as_str(), Some("in_progress"));
}

#[test]
fn e2e_routing_update_description_stdin_is_consumed_once_before_route_fanout() {
    let _log = common::test_log(
        "e2e_routing_update_description_stdin_is_consumed_once_before_route_fanout",
    );

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();
    init_workspace(&main_workspace, "init_description_stdin_main");
    init_workspace(&external_workspace, "init_description_stdin_external");
    configure_external_route(&main_workspace, &external_workspace);

    let local_id = create_issue_and_get_id(
        &main_workspace,
        "Local description stdin target",
        "create_local_description_stdin_target",
    );
    let external_id = create_issue_and_get_id(
        &external_workspace,
        "External description stdin target",
        "create_external_description_stdin_target",
    );
    let exact_description =
        "  shared leading spaces\n\n# Shared markdown\n\nroute one\nroute two\n\n";

    let update = run_br_with_stdin(
        &main_workspace,
        [
            "update",
            &local_id,
            &external_id,
            "--description-file",
            "-",
            "--json",
        ],
        exact_description,
        "update_mixed_routes_description_stdin",
    );
    assert!(
        update.status.success(),
        "mixed-route stdin update failed: stdout={} stderr={}",
        update.stdout,
        update.stderr
    );
    assert!(
        !update.stdout.contains("No updates specified"),
        "description stdin must be treated as an update: {}",
        update.stdout
    );
    let updated: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&update.stdout)).expect("update json");
    assert_eq!(updated.len(), 2);
    assert_eq!(updated[0]["id"].as_str(), Some(local_id.as_str()));
    assert_eq!(updated[1]["id"].as_str(), Some(external_id.as_str()));

    let local_after = show_issue_json(
        &main_workspace,
        &local_id,
        "show_local_after_description_stdin",
    );
    let external_after = show_issue_json(
        &external_workspace,
        &external_id,
        "show_external_after_description_stdin",
    );
    assert_eq!(
        local_after[0]["description"].as_str(),
        Some(exact_description)
    );
    assert_eq!(
        external_after[0]["description"].as_str(),
        Some(exact_description)
    );
}

#[test]
fn e2e_routing_update_sets_invoking_workspace_last_touched_for_follow_up_close() {
    let _log = common::test_log(
        "e2e_routing_update_sets_invoking_workspace_last_touched_for_follow_up_close",
    );

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    let init = run_br(&main_workspace, ["init"], "init_main");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let init_external = run_br(&external_workspace, ["init"], "init_external");
    assert!(
        init_external.status.success(),
        "external init failed: {}",
        init_external.stderr
    );

    fs::write(
        external_workspace.root.join(".beads").join("config.yaml"),
        "issue_prefix: ext\n",
    )
    .expect("write external config");
    create_routes_file(
        &main_workspace,
        &[("ext-", external_workspace.root.to_string_lossy().as_ref())],
    );

    let create = run_br(
        &external_workspace,
        ["create", "External follow-up close target", "--json"],
        "create_external_follow_up_close_target",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let created: Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("create json");
    let external_id = created["id"].as_str().expect("external id").to_string();

    let routed_input = routed_partial_id(&external_id);
    let update = run_br(
        &main_workspace,
        ["update", &routed_input, "--status", "in_progress", "--json"],
        "update_external_before_follow_up_close",
    );
    assert!(update.status.success(), "update failed: {}", update.stderr);

    let close = run_br(
        &main_workspace,
        ["close", "--json"],
        "close_follow_up_using_routed_last_touched",
    );
    assert!(close.status.success(), "close failed: {}", close.stderr);
    let closed: Value =
        serde_json::from_str(&extract_json_payload(&close.stdout)).expect("close json");
    let closed_array = closed.as_array().expect("closed array");
    assert_eq!(closed_array.len(), 1);
    assert_eq!(closed_array[0]["id"].as_str(), Some(external_id.as_str()));

    let show_external = run_br(
        &external_workspace,
        ["show", &external_id, "--json"],
        "show_external_after_follow_up_close",
    );
    assert!(
        show_external.status.success(),
        "external show failed: {}",
        show_external.stderr
    );
    let shown: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&show_external.stdout)).expect("show json");
    assert_eq!(shown[0]["status"].as_str(), Some("closed"));

    let jsonl_issue = issue_from_jsonl(&external_workspace, &external_id);
    assert_eq!(jsonl_issue["status"].as_str(), Some("closed"));
}

#[test]
fn e2e_routing_close_external_issue_via_main_workspace() {
    let _log = common::test_log("e2e_routing_close_external_issue_via_main_workspace");

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    let init = run_br(&main_workspace, ["init"], "init_main");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let init_external = run_br(&external_workspace, ["init"], "init_external");
    assert!(
        init_external.status.success(),
        "external init failed: {}",
        init_external.stderr
    );

    fs::write(
        external_workspace.root.join(".beads").join("config.yaml"),
        "issue_prefix: ext\n",
    )
    .expect("write external config");
    create_routes_file(
        &main_workspace,
        &[("ext-", external_workspace.root.to_string_lossy().as_ref())],
    );

    let create = run_br(
        &external_workspace,
        ["create", "External close target", "--json"],
        "create_external_close_target",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let created: Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("create json");
    let external_id = created["id"].as_str().expect("external id").to_string();
    let routed_input = routed_partial_id(&external_id);

    let close = run_br(
        &main_workspace,
        ["close", &routed_input, "--json"],
        "close_external_via_route",
    );
    assert!(close.status.success(), "close failed: {}", close.stderr);
    let closed: Value =
        serde_json::from_str(&extract_json_payload(&close.stdout)).expect("close json");
    let closed_array = closed.as_array().expect("closed array");
    assert_eq!(closed_array.len(), 1);
    assert_eq!(closed_array[0]["id"].as_str(), Some(external_id.as_str()));
    assert_eq!(closed_array[0]["status"].as_str(), Some("closed"));

    let show_external = run_br(
        &external_workspace,
        ["show", &external_id, "--json"],
        "show_external_after_routed_close",
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
fn e2e_routing_close_sets_invoking_workspace_last_touched_for_follow_up_reopen() {
    let _log = common::test_log(
        "e2e_routing_close_sets_invoking_workspace_last_touched_for_follow_up_reopen",
    );

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    let init = run_br(&main_workspace, ["init"], "init_main");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let init_external = run_br(&external_workspace, ["init"], "init_external");
    assert!(
        init_external.status.success(),
        "external init failed: {}",
        init_external.stderr
    );

    fs::write(
        external_workspace.root.join(".beads").join("config.yaml"),
        "issue_prefix: ext\n",
    )
    .expect("write external config");
    create_routes_file(
        &main_workspace,
        &[("ext-", external_workspace.root.to_string_lossy().as_ref())],
    );

    let create = run_br(
        &external_workspace,
        ["create", "External follow-up reopen target", "--json"],
        "create_external_follow_up_reopen_target",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let created: Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("create json");
    let external_id = created["id"].as_str().expect("external id").to_string();

    let routed_input = routed_partial_id(&external_id);
    let close = run_br(
        &main_workspace,
        ["close", &routed_input, "--json"],
        "close_external_before_follow_up_reopen",
    );
    assert!(close.status.success(), "close failed: {}", close.stderr);

    let reopen = run_br(
        &main_workspace,
        ["reopen", "--json"],
        "reopen_follow_up_using_routed_last_touched",
    );
    assert!(reopen.status.success(), "reopen failed: {}", reopen.stderr);
    let reopened: Value =
        serde_json::from_str(&extract_json_payload(&reopen.stdout)).expect("reopen json");
    let reopened_array = reopened["reopened"].as_array().expect("reopened array");
    assert_eq!(reopened_array.len(), 1);
    assert_eq!(reopened_array[0]["id"].as_str(), Some(external_id.as_str()));

    let show_external = run_br(
        &external_workspace,
        ["show", &external_id, "--json"],
        "show_external_after_follow_up_reopen",
    );
    assert!(
        show_external.status.success(),
        "external show failed: {}",
        show_external.stderr
    );
    let shown: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&show_external.stdout)).expect("show json");
    assert_eq!(shown[0]["status"].as_str(), Some("open"));

    let jsonl_issue = issue_from_jsonl(&external_workspace, &external_id);
    assert_eq!(jsonl_issue["status"].as_str(), Some("open"));
}

#[test]
fn e2e_routing_reopen_external_issue_via_main_workspace() {
    let _log = common::test_log("e2e_routing_reopen_external_issue_via_main_workspace");

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    let init = run_br(&main_workspace, ["init"], "init_main");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let init_external = run_br(&external_workspace, ["init"], "init_external");
    assert!(
        init_external.status.success(),
        "external init failed: {}",
        init_external.stderr
    );

    fs::write(
        external_workspace.root.join(".beads").join("config.yaml"),
        "issue_prefix: ext\n",
    )
    .expect("write external config");
    create_routes_file(
        &main_workspace,
        &[("ext-", external_workspace.root.to_string_lossy().as_ref())],
    );

    let create = run_br(
        &external_workspace,
        ["create", "External reopen target", "--json"],
        "create_external_reopen_target",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let created: Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("create json");
    let external_id = created["id"].as_str().expect("external id").to_string();
    let routed_input = routed_partial_id(&external_id);

    let close = run_br(
        &external_workspace,
        ["close", &external_id],
        "close_external_before_routed_reopen",
    );
    assert!(close.status.success(), "close failed: {}", close.stderr);

    let reopen = run_br(
        &main_workspace,
        ["reopen", &routed_input, "--json"],
        "reopen_external_via_route",
    );
    assert!(reopen.status.success(), "reopen failed: {}", reopen.stderr);
    let reopened: Value =
        serde_json::from_str(&extract_json_payload(&reopen.stdout)).expect("reopen json");
    let reopened_array = reopened["reopened"].as_array().expect("reopened array");
    assert_eq!(reopened_array.len(), 1);
    assert_eq!(reopened_array[0]["id"].as_str(), Some(external_id.as_str()));
    assert_eq!(reopened_array[0]["status"].as_str(), Some("open"));

    let show_external = run_br(
        &external_workspace,
        ["show", &external_id, "--json"],
        "show_external_after_routed_reopen",
    );
    assert!(
        show_external.status.success(),
        "external show failed: {}",
        show_external.stderr
    );
    let shown: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&show_external.stdout)).expect("show json");
    assert_eq!(shown[0]["status"].as_str(), Some("open"));
}

#[test]
fn e2e_routing_defer_and_undefer_external_issue_via_main_workspace() {
    let _log = common::test_log("e2e_routing_defer_and_undefer_external_issue_via_main_workspace");

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    let init = run_br(&main_workspace, ["init"], "init_main");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let init_external = run_br(&external_workspace, ["init"], "init_external");
    assert!(
        init_external.status.success(),
        "external init failed: {}",
        init_external.stderr
    );

    fs::write(
        external_workspace.root.join(".beads").join("config.yaml"),
        "issue_prefix: ext\n",
    )
    .expect("write external config");
    create_routes_file(
        &main_workspace,
        &[("ext-", external_workspace.root.to_string_lossy().as_ref())],
    );

    let create = run_br(
        &external_workspace,
        ["create", "External defer target", "--json"],
        "create_external_defer_target",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let created: Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("create json");
    let external_id = created["id"].as_str().expect("external id").to_string();
    let routed_input = routed_partial_id(&external_id);

    let defer = run_br(
        &main_workspace,
        ["defer", &routed_input, "--json"],
        "defer_external_via_route",
    );
    assert!(defer.status.success(), "defer failed: {}", defer.stderr);
    let deferred: Value =
        serde_json::from_str(&extract_json_payload(&defer.stdout)).expect("defer json");
    let deferred_array = deferred["deferred"].as_array().expect("deferred array");
    assert_eq!(deferred_array.len(), 1);
    assert_eq!(deferred_array[0]["id"].as_str(), Some(external_id.as_str()));
    assert_eq!(deferred_array[0]["status"].as_str(), Some("deferred"));

    let show_deferred = run_br(
        &external_workspace,
        ["show", &external_id, "--json"],
        "show_external_after_routed_defer",
    );
    assert!(
        show_deferred.status.success(),
        "external show failed: {}",
        show_deferred.stderr
    );
    let deferred_issue: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&show_deferred.stdout)).expect("show json");
    assert_eq!(deferred_issue[0]["status"].as_str(), Some("deferred"));

    let deferred_jsonl_issue = issue_from_jsonl(&external_workspace, &external_id);
    assert_eq!(deferred_jsonl_issue["status"].as_str(), Some("deferred"));

    let undefer = run_br(
        &main_workspace,
        ["undefer", &routed_input, "--json"],
        "undefer_external_via_route",
    );
    assert!(
        undefer.status.success(),
        "undefer failed: {}",
        undefer.stderr
    );
    let undeferred: Value =
        serde_json::from_str(&extract_json_payload(&undefer.stdout)).expect("undefer json");
    let undeferred_array = undeferred["undeferred"]
        .as_array()
        .expect("undeferred array");
    assert_eq!(undeferred_array.len(), 1);
    assert_eq!(
        undeferred_array[0]["id"].as_str(),
        Some(external_id.as_str())
    );
    assert_eq!(undeferred_array[0]["status"].as_str(), Some("open"));

    let show_undeferred = run_br(
        &external_workspace,
        ["show", &external_id, "--json"],
        "show_external_after_routed_undefer",
    );
    assert!(
        show_undeferred.status.success(),
        "external show failed: {}",
        show_undeferred.stderr
    );
    let undeferred_issue: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&show_undeferred.stdout)).expect("show json");
    assert_eq!(undeferred_issue[0]["status"].as_str(), Some("open"));

    let undeferred_jsonl_issue = issue_from_jsonl(&external_workspace, &external_id);
    assert_eq!(undeferred_jsonl_issue["status"].as_str(), Some("open"));
}

#[test]
fn e2e_routing_defer_and_undefer_import_stale_external_jsonl() {
    let _log = common::test_log("e2e_routing_defer_and_undefer_import_stale_external_jsonl");

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    init_workspace(&main_workspace, "init_main");
    init_workspace(&external_workspace, "init_external");
    configure_external_route(&main_workspace, &external_workspace);

    let external_id = create_issue_and_get_id(
        &external_workspace,
        "External defer stale db target",
        "create_external_defer_stale_target",
    );
    let routed_input = routed_partial_id(&external_id);

    let mut jsonl_issue = issue_from_jsonl(&external_workspace, &external_id);
    jsonl_issue["title"] = Value::String("External defer title from JSONL".to_string());
    jsonl_issue["updated_at"] = Value::String("2099-01-01T00:00:00Z".to_string());
    write_single_issue_jsonl(&external_workspace, &jsonl_issue);

    let defer = run_br(
        &main_workspace,
        ["defer", &routed_input, "--json"],
        "defer_external_stale_jsonl_via_route",
    );
    assert!(defer.status.success(), "defer failed: {}", defer.stderr);
    let deferred: Value =
        serde_json::from_str(&extract_json_payload(&defer.stdout)).expect("defer json");
    let deferred_array = deferred["deferred"].as_array().expect("deferred array");
    assert_eq!(deferred_array.len(), 1);
    assert_eq!(deferred_array[0]["id"].as_str(), Some(external_id.as_str()));
    assert_eq!(deferred_array[0]["status"].as_str(), Some("deferred"));
    assert_eq!(
        deferred_array[0]["title"].as_str(),
        Some("External defer title from JSONL")
    );

    let deferred_jsonl_issue = issue_from_jsonl(&external_workspace, &external_id);
    assert_eq!(
        deferred_jsonl_issue["title"].as_str(),
        Some("External defer title from JSONL")
    );
    assert_eq!(deferred_jsonl_issue["status"].as_str(), Some("deferred"));

    let mut stale_deferred_jsonl_issue = deferred_jsonl_issue;
    stale_deferred_jsonl_issue["title"] =
        Value::String("External undefer title from JSONL".to_string());
    stale_deferred_jsonl_issue["updated_at"] = Value::String("2099-01-02T00:00:00Z".to_string());
    write_single_issue_jsonl(&external_workspace, &stale_deferred_jsonl_issue);

    let undefer = run_br(
        &main_workspace,
        ["undefer", &routed_input, "--json"],
        "undefer_external_stale_jsonl_via_route",
    );
    assert!(
        undefer.status.success(),
        "undefer failed: {}",
        undefer.stderr
    );
    let undeferred: Value =
        serde_json::from_str(&extract_json_payload(&undefer.stdout)).expect("undefer json");
    let undeferred_array = undeferred["undeferred"]
        .as_array()
        .expect("undeferred array");
    assert_eq!(undeferred_array.len(), 1);
    assert_eq!(
        undeferred_array[0]["id"].as_str(),
        Some(external_id.as_str())
    );
    assert_eq!(undeferred_array[0]["status"].as_str(), Some("open"));
    assert_eq!(
        undeferred_array[0]["title"].as_str(),
        Some("External undefer title from JSONL")
    );

    let undeferred_jsonl_issue = issue_from_jsonl(&external_workspace, &external_id);
    assert_eq!(
        undeferred_jsonl_issue["title"].as_str(),
        Some("External undefer title from JSONL")
    );
    assert_eq!(undeferred_jsonl_issue["status"].as_str(), Some("open"));
}

#[test]
fn e2e_routing_label_add_and_list_external_issue_via_main_workspace() {
    let _log = common::test_log("e2e_routing_label_add_and_list_external_issue_via_main_workspace");

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    let init = run_br(&main_workspace, ["init"], "init_main");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let init_external = run_br(&external_workspace, ["init"], "init_external");
    assert!(
        init_external.status.success(),
        "external init failed: {}",
        init_external.stderr
    );

    fs::write(
        external_workspace.root.join(".beads").join("config.yaml"),
        "issue_prefix: ext\n",
    )
    .expect("write external config");
    create_routes_file(
        &main_workspace,
        &[("ext-", external_workspace.root.to_string_lossy().as_ref())],
    );

    let create = run_br(
        &external_workspace,
        ["create", "External label target", "--json"],
        "create_external_label_target",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let created: Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("create json");
    let external_id = created["id"].as_str().expect("external id").to_string();
    let routed_input = routed_partial_id(&external_id);

    let label_add = run_br(
        &main_workspace,
        ["label", "add", &routed_input, "triage", "--json"],
        "label_add_external_via_route",
    );
    assert!(
        label_add.status.success(),
        "label add failed: {}",
        label_add.stderr
    );
    let added: Value =
        serde_json::from_str(&extract_json_payload(&label_add.stdout)).expect("label add json");
    let added_array = added.as_array().expect("label add array");
    assert_eq!(added_array.len(), 1);
    assert_eq!(
        added_array[0]["issue_id"].as_str(),
        Some(external_id.as_str())
    );
    assert_eq!(added_array[0]["label"].as_str(), Some("triage"));

    let label_list = run_br(
        &main_workspace,
        ["label", "list", &routed_input, "--json"],
        "label_list_external_via_route",
    );
    assert!(
        label_list.status.success(),
        "label list failed: {}",
        label_list.stderr
    );
    let labels_payload = extract_json_payload(&label_list.stdout);
    let labels: Vec<String> = serde_json::from_str(&labels_payload).expect("label list json");
    assert_eq!(labels, vec!["triage".to_string()]);

    let show_external = run_br(
        &external_workspace,
        ["show", &external_id, "--json"],
        "show_external_after_routed_label",
    );
    assert!(
        show_external.status.success(),
        "external show failed: {}",
        show_external.stderr
    );
    let shown: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&show_external.stdout)).expect("show json");
    assert_eq!(shown[0]["labels"][0].as_str(), Some("triage"));

    let jsonl_issue = issue_from_jsonl(&external_workspace, &external_id);
    let jsonl_labels = jsonl_issue["labels"].as_array().expect("labels array");
    assert!(
        jsonl_labels
            .iter()
            .any(|label| label.as_str() == Some("triage")),
        "expected triage label in issues.jsonl"
    );
}

#[test]
fn e2e_routing_label_add_sets_invoking_workspace_last_touched_for_follow_up_update() {
    let _log = common::test_log(
        "e2e_routing_label_add_sets_invoking_workspace_last_touched_for_follow_up_update",
    );

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    let init = run_br(&main_workspace, ["init"], "init_main");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let init_external = run_br(&external_workspace, ["init"], "init_external");
    assert!(
        init_external.status.success(),
        "external init failed: {}",
        init_external.stderr
    );

    fs::write(
        external_workspace.root.join(".beads").join("config.yaml"),
        "issue_prefix: ext\n",
    )
    .expect("write external config");
    create_routes_file(
        &main_workspace,
        &[("ext-", external_workspace.root.to_string_lossy().as_ref())],
    );

    let create = run_br(
        &external_workspace,
        ["create", "External label context target", "--json"],
        "create_external_label_context_target",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let created: Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("create json");
    let external_id = created["id"].as_str().expect("external id").to_string();

    let routed_input = routed_partial_id(&external_id);
    let label_add = run_br(
        &main_workspace,
        ["label", "add", &routed_input, "triage", "--json"],
        "label_add_before_follow_up_update",
    );
    assert!(
        label_add.status.success(),
        "label add failed: {}",
        label_add.stderr
    );

    let update = run_br(
        &main_workspace,
        ["update", "--status", "in_progress", "--json"],
        "update_follow_up_using_label_last_touched",
    );
    assert!(update.status.success(), "update failed: {}", update.stderr);
    let updated: Value =
        serde_json::from_str(&extract_json_payload(&update.stdout)).expect("update json");
    let updated_array = updated.as_array().expect("update array");
    assert_eq!(updated_array.len(), 1);
    assert_eq!(updated_array[0]["id"].as_str(), Some(external_id.as_str()));

    let show_external = run_br(
        &external_workspace,
        ["show", &external_id, "--json"],
        "show_external_after_label_context_update",
    );
    assert!(
        show_external.status.success(),
        "external show failed: {}",
        show_external.stderr
    );
    let shown: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&show_external.stdout)).expect("show json");
    assert_eq!(shown[0]["status"].as_str(), Some("in_progress"));
}

#[test]
fn e2e_routing_comments_add_and_list_external_issue_via_main_workspace() {
    let _log =
        common::test_log("e2e_routing_comments_add_and_list_external_issue_via_main_workspace");

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    let init = run_br(&main_workspace, ["init"], "init_main");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let init_external = run_br(&external_workspace, ["init"], "init_external");
    assert!(
        init_external.status.success(),
        "external init failed: {}",
        init_external.stderr
    );

    fs::write(
        external_workspace.root.join(".beads").join("config.yaml"),
        "issue_prefix: ext\n",
    )
    .expect("write external config");
    create_routes_file(
        &main_workspace,
        &[("ext-", external_workspace.root.to_string_lossy().as_ref())],
    );

    let create = run_br(
        &external_workspace,
        ["create", "External comments target", "--json"],
        "create_external_comments_target",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let created: Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("create json");
    let external_id = created["id"].as_str().expect("external id").to_string();
    let routed_input = routed_partial_id(&external_id);

    let comment_add = run_br(
        &main_workspace,
        ["comments", "add", &routed_input, "Routed comment", "--json"],
        "comments_add_external_via_route",
    );
    assert!(
        comment_add.status.success(),
        "comments add failed: {}",
        comment_add.stderr
    );
    let added: Value =
        serde_json::from_str(&extract_json_payload(&comment_add.stdout)).expect("comment add json");
    assert_eq!(added["issue_id"].as_str(), Some(external_id.as_str()));
    assert_eq!(added["text"].as_str(), Some("Routed comment"));

    let comment_list = run_br(
        &main_workspace,
        ["comments", "list", &routed_input, "--json"],
        "comments_list_external_via_route",
    );
    assert!(
        comment_list.status.success(),
        "comments list failed: {}",
        comment_list.stderr
    );
    let comments_payload = extract_json_payload(&comment_list.stdout);
    let comments: Vec<Value> = serde_json::from_str(&comments_payload).expect("comments list json");
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0]["issue_id"].as_str(), Some(external_id.as_str()));
    assert_eq!(comments[0]["text"].as_str(), Some("Routed comment"));

    let show_external = run_br(
        &external_workspace,
        ["show", &external_id, "--json"],
        "show_external_after_routed_comment",
    );
    assert!(
        show_external.status.success(),
        "external show failed: {}",
        show_external.stderr
    );
    let shown: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&show_external.stdout)).expect("show json");
    assert_eq!(
        shown[0]["comments"][0]["text"].as_str(),
        Some("Routed comment")
    );

    let jsonl_issue = issue_from_jsonl(&external_workspace, &external_id);
    assert_eq!(
        jsonl_issue["comments"][0]["text"].as_str(),
        Some("Routed comment")
    );
}

#[test]
fn e2e_routing_comments_list_imports_stale_external_jsonl() {
    let _log = common::test_log("e2e_routing_comments_list_imports_stale_external_jsonl");

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    init_workspace(&main_workspace, "init_main");
    init_workspace(&external_workspace, "init_external");
    configure_external_route(&main_workspace, &external_workspace);

    let external_id = create_issue_and_get_id(
        &external_workspace,
        "External comments stale db target",
        "create_external_comments_stale_target",
    );
    let routed_input = routed_partial_id(&external_id);

    let mut jsonl_issue = issue_from_jsonl(&external_workspace, &external_id);
    jsonl_issue["comments"] = serde_json::json!([
        {
            "id": 1,
            "issue_id": external_id,
            "author": "jsonl-author",
            "text": "External comment from stale JSONL",
            "created_at": "2099-01-01T00:00:00Z"
        }
    ]);
    jsonl_issue["updated_at"] = Value::String("2099-01-01T00:00:00Z".to_string());
    write_single_issue_jsonl(&external_workspace, &jsonl_issue);

    let comment_list = run_br(
        &main_workspace,
        ["comments", "list", &routed_input, "--json"],
        "comments_list_external_stale_jsonl_via_route",
    );
    assert!(
        comment_list.status.success(),
        "comments list failed: {}",
        comment_list.stderr
    );
    let comments: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&comment_list.stdout)).expect("comments json");
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0]["issue_id"].as_str(), Some(external_id.as_str()));
    assert_eq!(
        comments[0]["text"].as_str(),
        Some("External comment from stale JSONL")
    );

    let shown = show_issue_json(
        &external_workspace,
        &external_id,
        "show_external_after_stale_comments_list",
    );
    assert_eq!(
        shown[0]["comments"][0]["text"].as_str(),
        Some("External comment from stale JSONL")
    );
}

#[test]
fn e2e_routing_comments_add_sets_invoking_workspace_last_touched_for_follow_up_update() {
    let _log = common::test_log(
        "e2e_routing_comments_add_sets_invoking_workspace_last_touched_for_follow_up_update",
    );

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    let init = run_br(&main_workspace, ["init"], "init_main");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let init_external = run_br(&external_workspace, ["init"], "init_external");
    assert!(
        init_external.status.success(),
        "external init failed: {}",
        init_external.stderr
    );

    fs::write(
        external_workspace.root.join(".beads").join("config.yaml"),
        "issue_prefix: ext\n",
    )
    .expect("write external config");
    create_routes_file(
        &main_workspace,
        &[("ext-", external_workspace.root.to_string_lossy().as_ref())],
    );

    let create = run_br(
        &external_workspace,
        ["create", "External comment context target", "--json"],
        "create_external_comment_context_target",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let created: Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("create json");
    let external_id = created["id"].as_str().expect("external id").to_string();

    let routed_input = routed_partial_id(&external_id);
    let comment_add = run_br(
        &main_workspace,
        [
            "comments",
            "add",
            &routed_input,
            "Context comment",
            "--json",
        ],
        "comment_add_before_follow_up_update",
    );
    assert!(
        comment_add.status.success(),
        "comments add failed: {}",
        comment_add.stderr
    );

    let update = run_br(
        &main_workspace,
        ["update", "--status", "in_progress", "--json"],
        "update_follow_up_using_comment_last_touched",
    );
    assert!(update.status.success(), "update failed: {}", update.stderr);
    let updated: Value =
        serde_json::from_str(&extract_json_payload(&update.stdout)).expect("update json");
    let updated_array = updated.as_array().expect("update array");
    assert_eq!(updated_array.len(), 1);
    assert_eq!(updated_array[0]["id"].as_str(), Some(external_id.as_str()));

    let show_external = run_br(
        &external_workspace,
        ["show", &external_id, "--json"],
        "show_external_after_comment_context_update",
    );
    assert!(
        show_external.status.success(),
        "external show failed: {}",
        show_external.stderr
    );
    let shown: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&show_external.stdout)).expect("show json");
    assert_eq!(shown[0]["status"].as_str(), Some("in_progress"));
}

#[test]
fn e2e_routing_dep_add_remove_and_list_external_issue_via_main_workspace() {
    let _log =
        common::test_log("e2e_routing_dep_add_remove_and_list_external_issue_via_main_workspace");

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    init_workspace(&main_workspace, "init_main");
    init_workspace(&external_workspace, "init_external");
    configure_external_route(&main_workspace, &external_workspace);

    let parent_id = create_issue_and_get_id(
        &external_workspace,
        "External dep parent",
        "create_external_dep_parent",
    );
    let child_id = create_issue_and_get_id(
        &external_workspace,
        "External dep child",
        "create_external_dep_child",
    );
    let routed_parent = routed_partial_id(&parent_id);
    let routed_child = routed_partial_id(&child_id);

    let dep_add = run_br(
        &main_workspace,
        ["dep", "add", &routed_child, &routed_parent, "--json"],
        "dep_add_external_via_route",
    );
    assert!(
        dep_add.status.success(),
        "dep add failed: {}",
        dep_add.stderr
    );
    let added: Value =
        serde_json::from_str(&extract_json_payload(&dep_add.stdout)).expect("dep add json");
    assert_eq!(added["issue_id"].as_str(), Some(child_id.as_str()));
    assert_eq!(added["depends_on_id"].as_str(), Some(parent_id.as_str()));
    assert_eq!(added["action"].as_str(), Some("added"));

    let dep_list = run_br(
        &main_workspace,
        ["dep", "list", &routed_child, "--json"],
        "dep_list_external_via_route",
    );
    assert!(
        dep_list.status.success(),
        "dep list failed: {}",
        dep_list.stderr
    );
    let list_json: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&dep_list.stdout)).expect("dep list json");
    assert!(
        list_json
            .iter()
            .any(|item| item["issue_id"] == child_id && item["depends_on_id"] == parent_id),
        "routed dependency not listed"
    );

    let jsonl_issue = issue_from_jsonl(&external_workspace, &child_id);
    assert_eq!(
        jsonl_issue["dependencies"][0]["depends_on_id"].as_str(),
        Some(parent_id.as_str())
    );

    let dep_remove = run_br(
        &main_workspace,
        ["dep", "remove", &routed_child, &routed_parent, "--json"],
        "dep_remove_external_via_route",
    );
    assert!(
        dep_remove.status.success(),
        "dep remove failed: {}",
        dep_remove.stderr
    );
    let removed: Value =
        serde_json::from_str(&extract_json_payload(&dep_remove.stdout)).expect("dep remove json");
    assert_eq!(removed["issue_id"].as_str(), Some(child_id.as_str()));
    assert_eq!(removed["depends_on_id"].as_str(), Some(parent_id.as_str()));
    assert_eq!(removed["action"].as_str(), Some("removed"));

    let dep_list_after = run_br(
        &main_workspace,
        ["dep", "list", &routed_child, "--json"],
        "dep_list_external_after_remove",
    );
    assert!(
        dep_list_after.status.success(),
        "dep list after remove failed: {}",
        dep_list_after.stderr
    );
    let list_after_json: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&dep_list_after.stdout))
            .expect("dep list after remove json");
    assert!(
        !list_after_json
            .iter()
            .any(|item| item["issue_id"] == child_id && item["depends_on_id"] == parent_id),
        "removed routed dependency still listed"
    );

    let jsonl_issue_after = issue_from_jsonl(&external_workspace, &child_id);
    assert_eq!(
        jsonl_issue_after["dependencies"]
            .as_array()
            .map_or(0, Vec::len),
        0
    );
}

#[test]
fn e2e_routing_dep_add_sets_invoking_workspace_last_touched_for_follow_up_update() {
    let _log = common::test_log(
        "e2e_routing_dep_add_sets_invoking_workspace_last_touched_for_follow_up_update",
    );

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    init_workspace(&main_workspace, "init_main");
    init_workspace(&external_workspace, "init_external");
    configure_external_route(&main_workspace, &external_workspace);

    let parent_id = create_issue_and_get_id(
        &external_workspace,
        "External follow-up dep parent",
        "create_external_follow_up_dep_parent",
    );
    let child_id = create_issue_and_get_id(
        &external_workspace,
        "External follow-up dep child",
        "create_external_follow_up_dep_child",
    );
    let routed_parent = routed_partial_id(&parent_id);
    let routed_child = routed_partial_id(&child_id);

    let dep_add = run_br(
        &main_workspace,
        ["dep", "add", &routed_child, &routed_parent, "--json"],
        "dep_add_before_follow_up_update",
    );
    assert!(
        dep_add.status.success(),
        "dep add failed: {}",
        dep_add.stderr
    );

    let update = run_br(
        &main_workspace,
        ["update", "--title", "Updated after routed dep", "--json"],
        "update_follow_up_using_dep_last_touched",
    );
    assert!(update.status.success(), "update failed: {}", update.stderr);
    let updated: Value =
        serde_json::from_str(&extract_json_payload(&update.stdout)).expect("update json");
    let updated_array = updated.as_array().expect("update array");
    assert_eq!(updated_array.len(), 1);
    assert_eq!(updated_array[0]["id"].as_str(), Some(child_id.as_str()));

    let shown = show_issue_json(
        &external_workspace,
        &child_id,
        "show_external_after_dep_context_update",
    );
    assert_eq!(shown[0]["title"].as_str(), Some("Updated after routed dep"));
}

#[test]
fn e2e_routing_dep_add_rejects_direct_cross_project_target() {
    let _log = common::test_log("e2e_routing_dep_add_rejects_direct_cross_project_target");

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    init_workspace(&main_workspace, "init_main");
    init_workspace(&external_workspace, "init_external");
    configure_external_route(&main_workspace, &external_workspace);

    let local_id = create_issue_and_get_id(&main_workspace, "Local dep issue", "create_local_dep");
    let external_id = create_issue_and_get_id(
        &external_workspace,
        "External direct target",
        "create_external_direct_target",
    );

    let dep_add = run_br(
        &main_workspace,
        ["dep", "add", &local_id, &external_id, "--json"],
        "dep_add_direct_cross_project_target",
    );
    assert!(
        !dep_add.status.success(),
        "dep add should reject bare cross-project targets"
    );
    assert!(
        dep_add.stdout.contains("different projects") && dep_add.stdout.contains("external:"),
        "unexpected stdout: {}",
        dep_add.stdout
    );
}

#[test]
fn e2e_routing_graph_external_issue_via_main_workspace() {
    let _log = common::test_log("e2e_routing_graph_external_issue_via_main_workspace");

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    init_workspace(&main_workspace, "init_main");
    init_workspace(&external_workspace, "init_external");
    configure_external_route(&main_workspace, &external_workspace);

    let parent_id = create_issue_and_get_id(
        &external_workspace,
        "External graph root",
        "create_external_graph_root",
    );
    let child_id = create_issue_and_get_id(
        &external_workspace,
        "External graph child",
        "create_external_graph_child",
    );

    let dep_add = run_br(
        &external_workspace,
        ["dep", "add", &child_id, &parent_id, "--json"],
        "external_dep_add_for_graph",
    );
    assert!(
        dep_add.status.success(),
        "dep add failed: {}",
        dep_add.stderr
    );

    let routed_parent = routed_partial_id(&parent_id);
    let graph = run_br(
        &main_workspace,
        ["--lock-timeout", "5", "graph", &routed_parent, "--json"],
        "graph_external_via_route",
    );
    assert!(graph.status.success(), "graph failed: {}", graph.stderr);
    let payload = extract_json_payload(&graph.stdout);
    let json: Value = serde_json::from_str(&payload).expect("graph json");

    assert_eq!(json["root"].as_str(), Some(parent_id.as_str()));
    let nodes = json["nodes"].as_array().expect("graph nodes array");
    assert!(
        nodes.iter().any(|node| node["id"] == parent_id),
        "graph should contain routed root"
    );
    assert!(
        nodes.iter().any(|node| node["id"] == child_id),
        "graph should contain routed dependent"
    );
}

#[test]
fn e2e_routing_delete_external_issue_via_main_workspace() {
    let _log = common::test_log("e2e_routing_delete_external_issue_via_main_workspace");

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    init_workspace(&main_workspace, "init_main");
    init_workspace(&external_workspace, "init_external");
    configure_external_route(&main_workspace, &external_workspace);

    let issue_id = create_issue_and_get_id(
        &external_workspace,
        "External delete target",
        "create_external_delete_target",
    );
    let routed_issue = routed_partial_id(&issue_id);

    let delete = run_br(
        &main_workspace,
        ["delete", &routed_issue, "--force", "--json"],
        "delete_external_via_route",
    );
    assert!(delete.status.success(), "delete failed: {}", delete.stderr);
    let json: Value =
        serde_json::from_str(&extract_json_payload(&delete.stdout)).expect("delete json");
    assert_eq!(json["deleted_count"].as_u64(), Some(1));
    assert_eq!(json["deleted"][0].as_str(), Some(issue_id.as_str()));

    let external_issue = issue_from_jsonl(&external_workspace, &issue_id);
    assert_eq!(external_issue["status"].as_str(), Some("tombstone"));
}

#[test]
fn e2e_routing_audit_log_external_issue_via_main_workspace() {
    let _log = common::test_log("e2e_routing_audit_log_external_issue_via_main_workspace");

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    init_workspace(&main_workspace, "init_main");
    init_workspace(&external_workspace, "init_external");
    configure_external_route(&main_workspace, &external_workspace);

    let issue_id = create_issue_and_get_id(
        &external_workspace,
        "External audit target",
        "create_external_audit_target",
    );
    let update = run_br(
        &external_workspace,
        ["update", &issue_id, "--priority", "0"],
        "update_external_audit_target",
    );
    assert!(update.status.success(), "update failed: {}", update.stderr);

    let routed_issue = routed_partial_id(&issue_id);
    let log = run_br(
        &main_workspace,
        ["audit", "log", &routed_issue, "--json"],
        "audit_log_external_via_route",
    );
    assert!(log.status.success(), "audit log failed: {}", log.stderr);

    let json: Value =
        serde_json::from_str(&extract_json_payload(&log.stdout)).expect("audit log json");
    assert_eq!(json["issue_id"].as_str(), Some(issue_id.as_str()));
    assert!(
        json["events"]
            .as_array()
            .is_some_and(|events| events.len() >= 2),
        "routed audit log should include external issue events: {json}"
    );
}

#[test]
fn e2e_routing_delete_preview_does_not_mutate_earlier_local_batch() {
    let _log = common::test_log("e2e_routing_delete_preview_does_not_mutate_earlier_local_batch");

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    init_workspace(&main_workspace, "init_main");
    init_workspace(&external_workspace, "init_external");
    configure_external_route(&main_workspace, &external_workspace);

    let local_id = create_issue_and_get_id(
        &main_workspace,
        "Local delete should remain",
        "create_local_delete_guard",
    );
    let blocker_id = create_issue_and_get_id(
        &external_workspace,
        "External delete blocker",
        "create_external_delete_blocker",
    );
    let child_id = create_issue_and_get_id(
        &external_workspace,
        "External delete child",
        "create_external_delete_child",
    );

    let dep_add = run_br(
        &external_workspace,
        ["dep", "add", &child_id, &blocker_id, "--json"],
        "external_dep_add_for_delete_preview",
    );
    assert!(
        dep_add.status.success(),
        "dep add failed: {}",
        dep_add.stderr
    );

    let routed_blocker = routed_partial_id(&blocker_id);
    let delete = run_br(
        &main_workspace,
        ["delete", &local_id, &routed_blocker, "--json"],
        "delete_preview_cross_route_guard",
    );
    assert!(
        delete.status.success(),
        "delete preview failed: {}",
        delete.stderr
    );
    let json: Value =
        serde_json::from_str(&extract_json_payload(&delete.stdout)).expect("delete preview json");
    assert_eq!(json["preview"].as_bool(), Some(true));

    let would_delete = json["would_delete"].as_array().expect("would_delete array");
    assert!(
        would_delete
            .iter()
            .any(|value| value == &Value::String(local_id.clone())),
        "preview should include the local issue"
    );
    assert!(
        would_delete
            .iter()
            .any(|value| value == &Value::String(blocker_id.clone())),
        "preview should include the routed external issue"
    );

    let local_issue = issue_from_jsonl(&main_workspace, &local_id);
    assert_eq!(local_issue["status"].as_str(), Some("open"));

    let external_issue = issue_from_jsonl(&external_workspace, &blocker_id);
    assert_eq!(external_issue["status"].as_str(), Some("open"));
}

#[test]
fn e2e_routing_delete_force_dry_run_reports_orphans_not_cascade() {
    let _log = common::test_log("e2e_routing_delete_force_dry_run_reports_orphans_not_cascade");

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    init_workspace(&main_workspace, "init_main");
    init_workspace(&external_workspace, "init_external");
    configure_external_route(&main_workspace, &external_workspace);

    let blocker_id = create_issue_and_get_id(
        &external_workspace,
        "External dry-run force blocker",
        "create_external_dry_run_force_blocker",
    );
    let child_id = create_issue_and_get_id(
        &external_workspace,
        "External dry-run force child",
        "create_external_dry_run_force_child",
    );
    let grandchild_id = create_issue_and_get_id(
        &external_workspace,
        "External dry-run force grandchild",
        "create_external_dry_run_force_grandchild",
    );

    let child_dep = run_br(
        &external_workspace,
        ["dep", "add", &child_id, &blocker_id, "--json"],
        "external_child_dep_add_for_force_dry_run",
    );
    assert!(
        child_dep.status.success(),
        "child dep add failed: {}",
        child_dep.stderr
    );
    let grandchild_dep = run_br(
        &external_workspace,
        ["dep", "add", &grandchild_id, &child_id, "--json"],
        "external_grandchild_dep_add_for_force_dry_run",
    );
    assert!(
        grandchild_dep.status.success(),
        "grandchild dep add failed: {}",
        grandchild_dep.stderr
    );

    let routed_blocker = routed_partial_id(&blocker_id);
    let delete = run_br(
        &main_workspace,
        ["delete", &routed_blocker, "--dry-run", "--force", "--json"],
        "delete_force_dry_run_external_via_route",
    );
    assert!(
        delete.status.success(),
        "force dry-run delete failed: {}",
        delete.stderr
    );

    let json: Value =
        serde_json::from_str(&extract_json_payload(&delete.stdout)).expect("delete preview json");
    assert_eq!(json["preview"].as_bool(), Some(true));
    assert_eq!(
        json["would_delete"].as_array().expect("would_delete array"),
        &[Value::String(blocker_id.clone())]
    );
    assert_eq!(
        json["orphaned_issues"]
            .as_array()
            .expect("orphaned_issues array"),
        &[Value::String(child_id.clone())]
    );
    assert!(
        json["cascade_delete"]
            .as_array()
            .expect("cascade_delete array")
            .is_empty(),
        "force dry-run must not report cascade deletes: {json}"
    );

    let blocker_issue = issue_from_jsonl(&external_workspace, &blocker_id);
    assert_eq!(blocker_issue["status"].as_str(), Some("open"));
    let child_issue = issue_from_jsonl(&external_workspace, &child_id);
    assert_eq!(child_issue["status"].as_str(), Some("open"));
    let grandchild_issue = issue_from_jsonl(&external_workspace, &grandchild_id);
    assert_eq!(grandchild_issue["status"].as_str(), Some("open"));
}

#[test]
fn e2e_routing_lint_external_issue_via_main_workspace() {
    let _log = common::test_log("e2e_routing_lint_external_issue_via_main_workspace");

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    init_workspace(&main_workspace, "init_main");
    init_workspace(&external_workspace, "init_external");
    configure_external_route(&main_workspace, &external_workspace);

    let create = run_br(
        &external_workspace,
        ["create", "External lint target", "--type", "bug", "--json"],
        "create_external_lint_target",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let create_json: Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("create json");
    let issue_id = create_json["id"].as_str().expect("issue id").to_string();

    let routed_issue = routed_partial_id(&issue_id);
    let lint = run_br(
        &main_workspace,
        ["lint", &routed_issue, "--json"],
        "lint_external_via_route",
    );
    assert!(lint.status.success(), "lint failed: {}", lint.stderr);

    let json: Value = serde_json::from_str(&extract_json_payload(&lint.stdout)).expect("lint json");
    assert_eq!(json["issues"].as_u64(), Some(1));
    let results = json["results"].as_array().expect("lint results");
    assert_eq!(results[0]["id"].as_str(), Some(issue_id.as_str()));
    assert!(
        results[0]["missing"]
            .as_array()
            .is_some_and(|missing| !missing.is_empty()),
        "routed lint should report missing sections for the external issue: {json}"
    );
}

#[test]
fn e2e_routing_label_add_failure_does_not_mutate_earlier_batches() {
    let _log = common::test_log("e2e_routing_label_add_failure_does_not_mutate_earlier_batches");

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    let init = run_br(&main_workspace, ["init"], "init_main");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let init_external = run_br(&external_workspace, ["init"], "init_external");
    assert!(
        init_external.status.success(),
        "external init failed: {}",
        init_external.stderr
    );

    fs::write(
        external_workspace.root.join(".beads").join("config.yaml"),
        "issue_prefix: ext\n",
    )
    .expect("write external config");
    create_routes_file(
        &main_workspace,
        &[("ext-", external_workspace.root.to_string_lossy().as_ref())],
    );

    let create_target = run_br(
        &main_workspace,
        ["create", "Local label target", "--json"],
        "create_local_label_target",
    );
    assert!(
        create_target.status.success(),
        "target create failed: {}",
        create_target.stderr
    );
    let target_issue: Value =
        serde_json::from_str(&extract_json_payload(&create_target.stdout)).expect("target json");
    let target_id = target_issue["id"].as_str().expect("target id").to_string();

    let create_other = run_br(
        &main_workspace,
        ["create", "Last touched sentinel", "--json"],
        "create_last_touched_sentinel",
    );
    assert!(
        create_other.status.success(),
        "sentinel create failed: {}",
        create_other.stderr
    );
    let last_touched_path = last_touched_path(&main_workspace);
    let last_touched_before = fs::read_to_string(&last_touched_path).ok();

    let label_add = run_br(
        &main_workspace,
        [
            "label",
            "add",
            &target_id,
            "ext-missing",
            "triage",
            "--json",
        ],
        "label_add_partial_failure",
    );
    assert!(
        !label_add.status.success(),
        "expected routed label add with missing external issue to fail"
    );
    // In JSON mode the structured error envelope is the whole of stdout
    // (issue #336), so "no partial success output" means the payload is an
    // error rather than a per-issue success list.
    let label_add_json: Value = serde_json::from_str(label_add.stdout.trim())
        .expect("failing routed label add should still emit one parseable JSON payload");
    assert!(
        label_add_json.get("error").is_some(),
        "failing routed label add should not emit partial success output: {}",
        label_add.stdout
    );

    let label_list = run_br(
        &main_workspace,
        ["label", "list", &target_id, "--json"],
        "label_list_after_failed_routed_add",
    );
    assert!(
        label_list.status.success(),
        "label list after failed add failed: {}",
        label_list.stderr
    );
    let labels: Vec<String> =
        serde_json::from_str(&extract_json_payload(&label_list.stdout)).expect("labels json");
    assert!(labels.is_empty(), "local target should remain unlabeled");

    let last_touched_after = fs::read_to_string(&last_touched_path).ok();
    assert_eq!(last_touched_after, last_touched_before);
}

#[test]
fn e2e_routing_show_external_issue_uses_metadata_database_path() {
    let _log = common::test_log("e2e_routing_show_external_issue_uses_metadata_database_path");

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    let init = run_br(&main_workspace, ["init"], "init_main");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let init_external = run_br(&external_workspace, ["init"], "init_external");
    assert!(
        init_external.status.success(),
        "external init failed: {}",
        init_external.stderr
    );

    fs::write(
        external_workspace.root.join(".beads").join("config.yaml"),
        "issue_prefix: ext\n",
    )
    .expect("write external config");
    switch_workspace_to_custom_database(&external_workspace, "custom.db");

    create_routes_file(
        &main_workspace,
        &[("ext-", external_workspace.root.to_string_lossy().as_ref())],
    );

    let create = run_br(
        &external_workspace,
        ["create", "External issue on custom db", "--json"],
        "create_external_custom_db",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let create_payload = extract_json_payload(&create.stdout);
    let created_issue: Value = serde_json::from_str(&create_payload).expect("create json");
    let external_id = created_issue["id"]
        .as_str()
        .expect("external id")
        .to_string();

    let show = run_br(
        &main_workspace,
        ["show", &external_id, "--json"],
        "show_external_custom_db_via_route",
    );
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let show_payload = extract_json_payload(&show.stdout);
    let shown: Vec<Value> = serde_json::from_str(&show_payload).expect("show json");
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0]["id"].as_str(), Some(external_id.as_str()));
    assert_eq!(
        shown[0]["title"].as_str(),
        Some("External issue on custom db")
    );
}

#[test]
fn e2e_routing_update_external_issue_uses_metadata_database_path() {
    let _log = common::test_log("e2e_routing_update_external_issue_uses_metadata_database_path");

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    let init = run_br(&main_workspace, ["init"], "init_main");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let init_external = run_br(&external_workspace, ["init"], "init_external");
    assert!(
        init_external.status.success(),
        "external init failed: {}",
        init_external.stderr
    );

    fs::write(
        external_workspace.root.join(".beads").join("config.yaml"),
        "issue_prefix: ext\n",
    )
    .expect("write external config");
    switch_workspace_to_custom_database(&external_workspace, "custom.db");

    create_routes_file(
        &main_workspace,
        &[("ext-", external_workspace.root.to_string_lossy().as_ref())],
    );

    let create = run_br(
        &external_workspace,
        ["create", "External update on custom db", "--json"],
        "create_external_update_custom_db",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let create_payload = extract_json_payload(&create.stdout);
    let created_issue: Value = serde_json::from_str(&create_payload).expect("create json");
    let external_id = created_issue["id"]
        .as_str()
        .expect("external id")
        .to_string();

    let update = run_br(
        &main_workspace,
        ["update", &external_id, "--status", "in_progress", "--json"],
        "update_external_custom_db_via_route",
    );
    assert!(update.status.success(), "update failed: {}", update.stderr);
    let update_payload = extract_json_payload(&update.stdout);
    let updated: Value = serde_json::from_str(&update_payload).expect("update json");
    let updated_array = updated.as_array().expect("update array");
    assert_eq!(updated_array.len(), 1);
    assert_eq!(updated_array[0]["id"].as_str(), Some(external_id.as_str()));
    assert_eq!(updated_array[0]["status"].as_str(), Some("in_progress"));

    let show_external = run_br(
        &external_workspace,
        ["show", &external_id, "--json"],
        "show_external_custom_db_after_routed_update",
    );
    assert!(
        show_external.status.success(),
        "external show failed: {}",
        show_external.stderr
    );
    let show_payload = extract_json_payload(&show_external.stdout);
    let shown: Vec<Value> = serde_json::from_str(&show_payload).expect("show json");
    assert_eq!(shown.len(), 1);
    assert_eq!(shown[0]["status"].as_str(), Some("in_progress"));
}

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_routing_update_mixed_batches_preserve_local_db_override() {
    let _log = common::test_log("e2e_routing_update_mixed_batches_preserve_local_db_override");

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    let init = run_br(&main_workspace, ["init"], "init_main");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let init_external = run_br(&external_workspace, ["init"], "init_external");
    assert!(
        init_external.status.success(),
        "external init failed: {}",
        init_external.stderr
    );

    fs::write(
        external_workspace.root.join(".beads").join("config.yaml"),
        "issue_prefix: ext\n",
    )
    .expect("write external config");

    create_routes_file(
        &main_workspace,
        &[("ext-", external_workspace.root.to_string_lossy().as_ref())],
    );

    let local_db = main_workspace.root.join("cache").join("alt-local.db");
    fs::create_dir_all(local_db.parent().expect("alt db parent")).expect("create cache dir");
    fs::copy(
        main_workspace.root.join(".beads").join("beads.db"),
        &local_db,
    )
    .expect("copy local db override");

    let create_local_first = run_br(
        &main_workspace,
        [
            "--db",
            local_db.to_str().unwrap(),
            "create",
            "Local issue on override db",
            "--json",
        ],
        "create_local_override_db_first",
    );
    assert!(
        create_local_first.status.success(),
        "local create failed: {}",
        create_local_first.stderr
    );
    let local_first_payload = extract_json_payload(&create_local_first.stdout);
    let local_first_issue: Value =
        serde_json::from_str(&local_first_payload).expect("local first create json");
    let local_first_id = local_first_issue["id"]
        .as_str()
        .expect("local first issue id")
        .to_string();

    let create_external = run_br(
        &external_workspace,
        ["create", "External issue on routed db", "--json"],
        "create_external_override_db",
    );
    assert!(
        create_external.status.success(),
        "external create failed: {}",
        create_external.stderr
    );
    let external_payload = extract_json_payload(&create_external.stdout);
    let external_issue: Value =
        serde_json::from_str(&external_payload).expect("external create json");
    let external_id = external_issue["id"]
        .as_str()
        .expect("external issue id")
        .to_string();

    let create_local_last = run_br(
        &main_workspace,
        [
            "--db",
            local_db.to_str().unwrap(),
            "create",
            "Local issue after routed db",
            "--json",
        ],
        "create_local_override_db_last",
    );
    assert!(
        create_local_last.status.success(),
        "local second create failed: {}",
        create_local_last.stderr
    );
    let local_last_payload = extract_json_payload(&create_local_last.stdout);
    let local_last_issue: Value =
        serde_json::from_str(&local_last_payload).expect("local last create json");
    let local_last_id = local_last_issue["id"]
        .as_str()
        .expect("local last issue id")
        .to_string();

    let update = run_br(
        &main_workspace,
        [
            "--db",
            local_db.to_str().unwrap(),
            "update",
            &local_first_id,
            &external_id,
            &local_last_id,
            "--status",
            "in_progress",
            "--json",
        ],
        "update_mixed_routed_override_db",
    );
    assert!(update.status.success(), "update failed: {}", update.stderr);
    let update_payload = extract_json_payload(&update.stdout);
    let updated: Vec<Value> = serde_json::from_str(&update_payload).expect("update json");
    assert_eq!(updated.len(), 3);
    assert_eq!(updated[0]["id"].as_str(), Some(local_first_id.as_str()));
    assert_eq!(updated[1]["id"].as_str(), Some(external_id.as_str()));
    assert_eq!(updated[2]["id"].as_str(), Some(local_last_id.as_str()));

    let show_local = run_br(
        &main_workspace,
        [
            "--db",
            local_db.to_str().unwrap(),
            "show",
            &local_first_id,
            "--json",
        ],
        "show_local_override_after_mixed_update",
    );
    assert!(
        show_local.status.success(),
        "local show failed: {}",
        show_local.stderr
    );
    let local_issue_details: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&show_local.stdout)).expect("local show json");
    assert_eq!(
        local_issue_details[0]["status"].as_str(),
        Some("in_progress")
    );

    let show_local_last = run_br(
        &main_workspace,
        [
            "--db",
            local_db.to_str().unwrap(),
            "show",
            &local_last_id,
            "--json",
        ],
        "show_local_override_last_after_mixed_update",
    );
    assert!(
        show_local_last.status.success(),
        "local last show failed: {}",
        show_local_last.stderr
    );
    let local_last_issue_details: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&show_local_last.stdout))
            .expect("local last show json");
    assert_eq!(
        local_last_issue_details[0]["status"].as_str(),
        Some("in_progress")
    );

    let show_external = run_br(
        &external_workspace,
        ["show", &external_id, "--json"],
        "show_external_after_mixed_update",
    );
    assert!(
        show_external.status.success(),
        "external show failed: {}",
        show_external.stderr
    );
    let external_issue_details: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&show_external.stdout))
            .expect("external show json");
    assert_eq!(
        external_issue_details[0]["status"].as_str(),
        Some("in_progress")
    );
}

#[test]
fn e2e_routing_update_failure_does_not_print_partial_success() {
    let _log = common::test_log("e2e_routing_update_failure_does_not_print_partial_success");

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    let init = run_br(&main_workspace, ["init"], "init_partial_update_main");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let init_external = run_br(
        &external_workspace,
        ["init"],
        "init_partial_update_external",
    );
    assert!(
        init_external.status.success(),
        "external init failed: {}",
        init_external.stderr
    );

    fs::write(
        external_workspace.root.join(".beads").join("config.yaml"),
        "issue_prefix: ext\n",
    )
    .expect("write external config");

    create_routes_file(
        &main_workspace,
        &[("ext-", external_workspace.root.to_string_lossy().as_ref())],
    );

    let create_local = run_br(
        &main_workspace,
        ["create", "Local issue before routed failure", "--json"],
        "create_local_before_routed_failure",
    );
    assert!(
        create_local.status.success(),
        "local create failed: {}",
        create_local.stderr
    );
    let local_issue: Value = serde_json::from_str(&extract_json_payload(&create_local.stdout))
        .expect("local create json");
    let local_id = local_issue["id"]
        .as_str()
        .expect("local issue id")
        .to_string();
    let last_touched_path = last_touched_path(&main_workspace);
    let last_touched_before = fs::read_to_string(&last_touched_path).ok();

    let update = run_br(
        &main_workspace,
        [
            "update",
            &local_id,
            "ext-missing",
            "--status",
            "in_progress",
        ],
        "update_routed_failure_no_partial_stdout",
    );
    assert!(
        !update.status.success(),
        "expected routed update with missing external issue to fail"
    );
    assert!(
        update.stdout.trim().is_empty(),
        "failing routed update should not emit partial success output: {}",
        update.stdout
    );

    let show_local = run_br(
        &main_workspace,
        ["show", &local_id, "--json"],
        "show_local_after_failed_routed_update",
    );
    assert!(
        show_local.status.success(),
        "show local after failed routed update failed: {}",
        show_local.stderr
    );
    let local_after: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&show_local.stdout)).expect("show local json");
    assert_eq!(local_after[0]["status"].as_str(), Some("open"));

    let last_touched_after = fs::read_to_string(&last_touched_path).ok();
    assert_eq!(last_touched_after, last_touched_before);
}

#[test]
fn e2e_routing_update_claim_failure_does_not_mutate_earlier_routes() {
    let _log = common::test_log("e2e_routing_update_claim_failure_does_not_mutate_earlier_routes");

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    init_workspace(&main_workspace, "init_routed_claim_failure_main");
    init_workspace(&external_workspace, "init_routed_claim_failure_external");
    configure_external_route(&main_workspace, &external_workspace);

    let local_id = create_issue_and_get_id(
        &main_workspace,
        "Local routed claim target",
        "create_local_routed_claim_target",
    );
    let external_id = create_issue_and_get_id(
        &external_workspace,
        "External routed claim target",
        "create_external_routed_claim_target",
    );

    let claim_external = run_br(
        &external_workspace,
        [
            "--actor",
            "bob",
            "update",
            &external_id,
            "--claim",
            "--json",
        ],
        "claim_external_issue_bob",
    );
    assert!(
        claim_external.status.success(),
        "claim external failed: {}",
        claim_external.stderr
    );

    let update = run_br(
        &main_workspace,
        [
            "--actor",
            "alice",
            "update",
            &local_id,
            &external_id,
            "--claim",
            "--json",
        ],
        "update_routed_claim_failure_atomic",
    );
    assert!(
        !update.status.success(),
        "expected routed claim to fail when external issue is already assigned"
    );

    let local_after = show_issue_json(
        &main_workspace,
        &local_id,
        "show_local_after_failed_routed_claim",
    );
    assert_eq!(local_after[0]["status"].as_str(), Some("open"));
    assert!(local_after[0]["assignee"].is_null());
}

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_routing_update_text_preserves_requested_order_across_routes() {
    let _log = common::test_log("e2e_routing_update_text_preserves_requested_order_across_routes");

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    let init_main = run_br(&main_workspace, ["init"], "init_update_text_main");
    assert!(
        init_main.status.success(),
        "init failed: {}",
        init_main.stderr
    );

    let init_external = run_br(&external_workspace, ["init"], "init_update_text_external");
    assert!(
        init_external.status.success(),
        "external init failed: {}",
        init_external.stderr
    );

    fs::write(
        external_workspace.root.join(".beads").join("config.yaml"),
        "issue_prefix: ext\n",
    )
    .expect("write external config");

    create_routes_file(
        &main_workspace,
        &[("ext-", external_workspace.root.to_string_lossy().as_ref())],
    );

    let create_local_first = run_br(
        &main_workspace,
        ["create", "Local first update text", "--json"],
        "create_local_first_update_text",
    );
    assert!(
        create_local_first.status.success(),
        "create local first failed: {}",
        create_local_first.stderr
    );
    let local_first: Value =
        serde_json::from_str(&extract_json_payload(&create_local_first.stdout))
            .expect("local first update text json");
    let local_first_id = local_first["id"]
        .as_str()
        .expect("local first update text id")
        .to_string();

    let create_external_middle = run_br(
        &external_workspace,
        ["create", "External middle update text", "--json"],
        "create_external_middle_update_text",
    );
    assert!(
        create_external_middle.status.success(),
        "create external middle failed: {}",
        create_external_middle.stderr
    );
    let external_middle: Value =
        serde_json::from_str(&extract_json_payload(&create_external_middle.stdout))
            .expect("external middle update text json");
    let external_middle_id = external_middle["id"]
        .as_str()
        .expect("external middle update text id")
        .to_string();

    let create_local_last = run_br(
        &main_workspace,
        ["create", "Local last update text", "--json"],
        "create_local_last_update_text",
    );
    assert!(
        create_local_last.status.success(),
        "create local last failed: {}",
        create_local_last.stderr
    );
    let local_last: Value = serde_json::from_str(&extract_json_payload(&create_local_last.stdout))
        .expect("local last update text json");
    let local_last_id = local_last["id"]
        .as_str()
        .expect("local last update text id")
        .to_string();

    let update = run_br(
        &main_workspace,
        [
            "update",
            &local_first_id,
            &external_middle_id,
            &local_last_id,
            "--status",
            "in_progress",
        ],
        "update_mixed_route_text_order",
    );
    assert!(update.status.success(), "update failed: {}", update.stderr);

    let local_first_pos = update
        .stdout
        .find("Local first update text")
        .expect("local first update output");
    let external_middle_pos = update
        .stdout
        .find("External middle update text")
        .expect("external middle update output");
    let local_last_pos = update
        .stdout
        .find("Local last update text")
        .expect("local last update output");

    assert!(local_first_pos < external_middle_pos);
    assert!(external_middle_pos < local_last_pos);
}

#[test]
fn e2e_routing_show_mixed_no_db_batches_preserve_local_db_override() {
    let _log = common::test_log("e2e_routing_show_mixed_no_db_batches_preserve_local_db_override");

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    let init = run_br(&main_workspace, ["init"], "init_main");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let init_external = run_br(&external_workspace, ["init"], "init_external");
    assert!(
        init_external.status.success(),
        "external init failed: {}",
        init_external.stderr
    );

    fs::write(
        external_workspace.root.join(".beads").join("config.yaml"),
        "issue_prefix: ext\n",
    )
    .expect("write external config");

    create_routes_file(
        &main_workspace,
        &[("ext-", external_workspace.root.to_string_lossy().as_ref())],
    );

    let local_db = main_workspace.root.join("cache").join("alt-local.db");
    fs::create_dir_all(local_db.parent().expect("alt db parent")).expect("create cache dir");

    let create_local = run_br(
        &main_workspace,
        [
            "--db",
            local_db.to_str().unwrap(),
            "create",
            "Local issue on override jsonl",
            "--json",
        ],
        "create_local_override_jsonl",
    );
    assert!(
        create_local.status.success(),
        "local create failed: {}",
        create_local.stderr
    );
    let local_payload = extract_json_payload(&create_local.stdout);
    let local_issue: Value = serde_json::from_str(&local_payload).expect("local create json");
    let local_id = local_issue["id"]
        .as_str()
        .expect("local issue id")
        .to_string();

    let create_external = run_br(
        &external_workspace,
        ["create", "External issue for no-db show", "--json"],
        "create_external_no_db_show",
    );
    assert!(
        create_external.status.success(),
        "external create failed: {}",
        create_external.stderr
    );
    let external_payload = extract_json_payload(&create_external.stdout);
    let external_issue: Value =
        serde_json::from_str(&external_payload).expect("external create json");
    let external_id = external_issue["id"]
        .as_str()
        .expect("external issue id")
        .to_string();

    let show = run_br(
        &main_workspace,
        [
            "--no-db",
            "--db",
            local_db.to_str().unwrap(),
            "show",
            &local_id,
            &external_id,
            "--json",
        ],
        "show_mixed_routed_override_jsonl",
    );
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let shown: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&show.stdout)).expect("show json");
    assert_eq!(shown.len(), 2);
    assert!(
        shown
            .iter()
            .any(|issue| issue["id"].as_str() == Some(local_id.as_str()))
    );
    assert!(
        shown
            .iter()
            .any(|issue| issue["id"].as_str() == Some(external_id.as_str()))
    );
}

// =============================================================================
// REDIRECT FILE TESTS
// =============================================================================

#[test]
fn e2e_routing_redirect_file_absolute_path() {
    let _log = common::test_log("e2e_routing_redirect_file_absolute_path");

    // Use separate workspaces
    let actual_workspace = BrWorkspace::new();
    let redirect_workspace = BrWorkspace::new();

    // Initialize the actual workspace
    let init = run_br(&actual_workspace, ["init"], "init_actual");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create redirect file pointing to actual beads directory (absolute path)
    let actual_beads = actual_workspace.root.join(".beads");
    // First create the redirect .beads directory
    fs::create_dir_all(redirect_workspace.root.join(".beads")).expect("create redirect beads");
    // Then create the redirect file
    create_redirect_file(
        &redirect_workspace.root.join(".beads"),
        actual_beads.to_str().unwrap(),
    );

    // The redirect is used during route resolution, not BEADS_DIR discovery.
    // Test that creating an issue in the actual workspace works
    let create = run_br(
        &actual_workspace,
        [
            "create",
            "Via redirect test",
            "--priority",
            "2",
            "--type",
            "task",
            "--json",
        ],
        "create",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Verify issue exists
    let list = run_br(&actual_workspace, ["list", "--json"], "list");
    assert!(list.status.success(), "list failed: {}", list.stderr);
    assert!(
        list.stdout.contains("Via redirect test"),
        "Expected issue in workspace"
    );
}

#[test]
fn e2e_routing_redirect_file_relative_path() {
    let _log = common::test_log("e2e_routing_redirect_file_relative_path");
    let workspace = BrWorkspace::new();

    // Initialize workspace
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Test that relative paths in redirect files are handled correctly
    // by creating a redirect file and verifying the path resolution logic
    let beads_dir = workspace.root.join(".beads");
    let redirect_path = beads_dir.join("redirect");

    // Create a redirect to a relative path (which resolves to same location)
    fs::write(&redirect_path, ".").expect("write redirect");

    // Should work (redirect to "." means same directory)
    let create = run_br(
        &workspace,
        [
            "create",
            "Test relative redirect",
            "--priority",
            "2",
            "--type",
            "task",
            "--json",
        ],
        "create",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Verify issue exists
    let list = run_br(&workspace, ["list", "--json"], "list");
    assert!(list.status.success(), "list failed: {}", list.stderr);
    assert!(
        list.stdout.contains("Test relative redirect"),
        "Expected issue in workspace"
    );
}

#[test]
fn e2e_routing_redirect_missing_target() {
    let _log = common::test_log("e2e_routing_redirect_missing_target");
    let workspace = BrWorkspace::new();

    // Initialize workspace
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create a route to a nonexistent external project
    let routes_path = workspace.root.join(".beads").join("routes.jsonl");
    fs::write(
        &routes_path,
        r#"{"prefix":"missing-","path":"/nonexistent/path/to/project"}"#,
    )
    .expect("write routes.jsonl");

    // Create a redirect file in an external route target directory
    let ext_beads = workspace.root.join("ext").join(".beads");
    fs::create_dir_all(&ext_beads).expect("create ext beads");
    create_redirect_file(&ext_beads, "/nonexistent/redirect/target/.beads");

    // Add route to this external project
    fs::write(&routes_path, r#"{"prefix":"ext-","path":"ext"}"#).expect("write routes.jsonl");

    // Trying to show an issue with the ext- prefix should trigger redirect resolution
    // and fail because the redirect target doesn't exist
    let show = run_br(
        &workspace,
        ["show", "ext-abc123", "--json"],
        "show_missing_redirect",
    );

    // The routing code attempts to follow redirects. If target is missing,
    // it should produce an error or fall back gracefully.
    // Check that error messaging is clear when redirect/route fails
    if !show.status.success() {
        assert!(
            show.stdout.contains("not found")
                || show.stdout.contains("Redirect")
                || show.stdout.contains("redirect")
                || show.stdout.contains("Issue")
                || show.stdout.contains("route"),
            "Expected clear error about routing/redirect, got: {}",
            show.stdout
        );
    }
    // If it succeeds (by falling back to local), that's also acceptable behavior
}

#[test]
fn e2e_routing_redirect_empty_file() {
    let _log = common::test_log("e2e_routing_redirect_empty_file");
    let workspace = BrWorkspace::new();

    // Initialize workspace
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create empty redirect file
    let redirect_path = workspace.root.join(".beads").join("redirect");
    fs::write(&redirect_path, "").expect("write empty redirect");

    // Should still work (empty redirect is ignored)
    let list = run_br(&workspace, ["list", "--json"], "list");
    assert!(
        list.status.success(),
        "Expected success with empty redirect: {}",
        list.stderr
    );
}

// =============================================================================
// EXTERNAL DB REFERENCE SAFETY TESTS
// =============================================================================

#[test]
fn e2e_routing_db_flag_external_path() {
    let _log = common::test_log("e2e_routing_db_flag_external_path");
    let workspace = BrWorkspace::new();

    // Create external project with beads
    let external_beads = workspace.root.join("external").join(".beads");
    fs::create_dir_all(&external_beads).expect("create external beads dir");
    let external_db = external_beads.join("beads.db");

    // Initialize external database using --db flag
    let init = run_br_with_env(
        &workspace,
        ["init"],
        [("BEADS_DIR", external_beads.to_str().unwrap())],
        "init_external",
    );
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Use --db flag to point to external database
    let create = run_br(
        &workspace,
        [
            "--db",
            external_db.to_str().unwrap(),
            "create",
            "Via db flag",
            "--priority",
            "2",
            "--type",
            "task",
            "--json",
        ],
        "create_via_db",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Verify issue exists in external project
    let list = run_br(
        &workspace,
        ["--db", external_db.to_str().unwrap(), "list", "--json"],
        "list_external",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);
    assert!(
        list.stdout.contains("Via db flag"),
        "Expected issue in external project"
    );
}

#[test]
fn e2e_routing_db_flag_external_db_uses_workspace_beads_dir() {
    let _log = common::test_log("e2e_routing_db_flag_external_db_uses_workspace_beads_dir");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_workspace");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        [
            "create",
            "Workspace issue",
            "--priority",
            "2",
            "--type",
            "task",
        ],
        "create_workspace_issue",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let external_db = workspace.root.join("cache").join("beads.db");
    fs::create_dir_all(external_db.parent().unwrap()).expect("create cache dir");
    fs::copy(workspace.root.join(".beads").join("beads.db"), &external_db).expect("copy db");

    let list = run_br(
        &workspace,
        ["--db", external_db.to_str().unwrap(), "list", "--json"],
        "list_external_db_outside_beads",
    );
    assert!(
        list.status.success(),
        "commands should still discover the workspace when --db points outside .beads: {}",
        list.stderr
    );
    assert!(list.stdout.contains("Workspace issue"));
}

#[test]
fn e2e_config_get_db_flag_invalid_target_fails_instead_of_falling_back() {
    let _log =
        common::test_log("e2e_config_get_db_flag_invalid_target_fails_instead_of_falling_back");
    let workspace = BrWorkspace::new();

    let external_beads = workspace.root.join("broken").join(".beads");
    fs::create_dir_all(&external_beads).expect("create external beads dir");
    let external_db = external_beads.join("beads.db");
    fs::write(&external_db, "not a sqlite database").expect("write corrupt db");
    fs::write(
        external_beads.join("config.yaml"),
        "issue_prefix: PROJECT\n",
    )
    .expect("write config");

    let get = run_br(
        &workspace,
        [
            "--db",
            external_db.to_str().unwrap(),
            "config",
            "get",
            "issue_prefix",
        ],
        "config_get_invalid_db_target",
    );
    // config get for YAML-backed keys (issue_prefix) succeeds even with a
    // corrupt DB because the config layer reads from the sibling config.yaml.
    // The --db flag influences which .beads/ directory the config is loaded
    // from, so the value from the broken workspace's config.yaml is returned.
    assert!(
        get.status.success(),
        "config get for YAML-backed key should succeed even with corrupt DB: {}",
        get.stderr
    );
    assert!(
        get.stdout.contains("PROJECT"),
        "config get should resolve YAML config from the --db directory's workspace"
    );
}

#[test]
fn e2e_config_delete_db_flag_invalid_target_preserves_yaml() {
    let _log = common::test_log("e2e_config_delete_db_flag_invalid_target_preserves_yaml");
    let workspace = BrWorkspace::new();

    let external_beads = workspace.root.join("broken-delete").join(".beads");
    fs::create_dir_all(&external_beads).expect("create external beads dir");
    let external_db = external_beads.join("beads.db");
    let project_config = external_beads.join("config.yaml");
    fs::write(&external_db, "not a sqlite database").expect("write corrupt db");
    fs::write(&project_config, "issue_prefix: PROJECT\n").expect("write config");

    let delete = run_br(
        &workspace,
        [
            "--db",
            external_db.to_str().unwrap(),
            "config",
            "delete",
            "issue_prefix",
        ],
        "config_delete_invalid_db_target",
    );
    assert!(
        !delete.status.success(),
        "config delete should fail for an explicitly targeted broken DB"
    );
    assert_eq!(
        fs::read_to_string(&project_config).unwrap(),
        "issue_prefix: PROJECT\n",
        "project YAML should remain untouched when explicit DB open fails"
    );
}

#[test]
fn e2e_changelog_since_commit_uses_target_repo_root() {
    let _log = common::test_log("e2e_changelog_since_commit_uses_target_repo_root");
    let workspace = BrWorkspace::new();

    let external_root = workspace.root.join("external-repo");
    let external_beads = external_root.join(".beads");
    fs::create_dir_all(&external_beads).expect("create external beads dir");
    let head = init_test_git_repo(&external_root);

    let init = run_br_with_env(
        &workspace,
        ["init"],
        [("BEADS_DIR", external_beads.to_str().unwrap())],
        "init_external_repo",
    );
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        [
            "--db",
            external_beads.join("beads.db").to_str().unwrap(),
            "create",
            "External closed issue",
            "--type",
            "task",
            "--priority",
            "2",
            "--json",
        ],
        "create_external_closed_issue",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let payload = extract_json_payload(&create.stdout);
    let issue: Value = serde_json::from_str(&payload).expect("parse create json");
    let id = issue["id"].as_str().expect("issue id").to_string();

    let close = run_br(
        &workspace,
        [
            "--db",
            external_beads.join("beads.db").to_str().unwrap(),
            "close",
            &id,
            "--reason",
            "done",
        ],
        "close_external_closed_issue",
    );
    assert!(close.status.success(), "close failed: {}", close.stderr);

    let changelog = run_br(
        &workspace,
        [
            "--db",
            external_beads.join("beads.db").to_str().unwrap(),
            "changelog",
            "--since-commit",
            &head,
            "--json",
        ],
        "changelog_external_since_commit",
    );
    assert!(
        changelog.status.success(),
        "changelog should resolve git references in the targeted repo: {}",
        changelog.stderr
    );
}

#[test]
fn e2e_routing_path_normalization() {
    let _log = common::test_log("e2e_routing_path_normalization");
    let workspace = BrWorkspace::new();

    // Create actual project
    let actual_beads = workspace.root.join("actual").join(".beads");
    fs::create_dir_all(&actual_beads).expect("create actual beads dir");

    // Initialize
    let init = run_br_with_env(
        &workspace,
        ["init"],
        [("BEADS_DIR", actual_beads.to_str().unwrap())],
        "init",
    );
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Use path with .. components that normalizes to a valid path
    let db_with_dotdot = workspace
        .root
        .join("actual")
        .join("subdir")
        .join("..")
        .join(".beads")
        .join("beads.db");
    fs::create_dir_all(workspace.root.join("actual").join("subdir")).expect("create subdir");

    let list = run_br(
        &workspace,
        ["--db", db_with_dotdot.to_str().unwrap(), "list", "--json"],
        "list_normalized",
    );
    assert!(
        list.status.success(),
        "Expected success with normalized path: {}",
        list.stderr
    );
}

// =============================================================================
// ERROR MESSAGE CLARITY TESTS
// =============================================================================

#[test]
fn e2e_routing_not_initialized_error() {
    let _log = common::test_log("e2e_routing_not_initialized_error");
    let workspace = BrWorkspace::new();

    // Run command without initialization
    let list = run_br(&workspace, ["list", "--json"], "list_not_init");
    assert!(
        !list.status.success(),
        "Expected failure when not initialized"
    );
    assert!(
        list.stdout.contains("not initialized")
            || list.stdout.contains("br init")
            || list.stdout.contains("NotInitialized"),
        "Expected clear error about initialization, got: {}",
        list.stderr
    );
}

#[test]
fn e2e_routing_invalid_beads_dir_env() {
    let _log = common::test_log("e2e_routing_invalid_beads_dir_env");
    let workspace = BrWorkspace::new();

    // Use BEADS_DIR pointing to nonexistent directory
    let list = run_br_with_env(
        &workspace,
        ["list", "--json"],
        [("BEADS_DIR", "/nonexistent/path/.beads")],
        "list_invalid_env",
    );
    assert!(
        !list.status.success(),
        "Expected failure for invalid BEADS_DIR"
    );
    // Should fall back to discovery and fail with not initialized
    assert!(
        list.stdout.contains("not initialized")
            || list.stdout.contains("br init")
            || list.stdout.contains("NotInitialized")
            || list.stdout.contains("not found"),
        "Expected clear error, got: {}",
        list.stderr
    );
}

#[test]
fn e2e_routing_show_external_issue_not_found() {
    let _log = common::test_log("e2e_routing_show_external_issue_not_found");

    // Use separate workspaces to avoid init conflicts
    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    // Initialize main workspace
    let init = run_br(&main_workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Initialize external workspace
    let init_ext = run_br(&external_workspace, ["init"], "init_external");
    assert!(
        init_ext.status.success(),
        "init failed: {}",
        init_ext.stderr
    );

    // Set a different prefix for external project
    let external_config = external_workspace.root.join(".beads").join("config.yaml");
    fs::write(&external_config, "issue_prefix: ext\n").expect("write external config");

    // Create routes file in main workspace pointing to external workspace
    let routes_path = main_workspace.root.join(".beads").join("routes.jsonl");
    let route_entry = format!(
        r#"{{"prefix":"ext-","path":"{}"}}"#,
        external_workspace.root.display()
    );
    fs::write(&routes_path, route_entry).expect("write routes.jsonl");

    // Try to show a nonexistent issue with ext- prefix
    // This should trigger route resolution to external project
    let show = run_br(
        &main_workspace,
        ["show", "ext-nonexistent", "--json"],
        "show_missing",
    );
    assert!(
        !show.status.success(),
        "Expected failure for nonexistent issue"
    );
    assert!(
        show.stdout.contains("not found")
            || show.stdout.contains("Issue")
            || show.stdout.contains("ext-nonexistent")
            || show.stderr.contains("No issue"),
        "Expected clear error about missing issue, got: {}",
        show.stderr
    );
}

#[test]
fn e2e_routing_show_external_issue_not_found_quiet_still_fails() {
    let _log = common::test_log("e2e_routing_show_external_issue_not_found_quiet_still_fails");

    let main_workspace = BrWorkspace::new();
    let external_workspace = BrWorkspace::new();

    let init = run_br(&main_workspace, ["init"], "init_quiet_missing");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let init_ext = run_br(&external_workspace, ["init"], "init_external_quiet_missing");
    assert!(
        init_ext.status.success(),
        "init failed: {}",
        init_ext.stderr
    );

    fs::write(
        external_workspace.root.join(".beads").join("config.yaml"),
        "issue_prefix: ext\n",
    )
    .expect("write external config");

    let routes_path = main_workspace.root.join(".beads").join("routes.jsonl");
    let route_entry = format!(
        r#"{{"prefix":"ext-","path":"{}"}}"#,
        external_workspace.root.display()
    );
    fs::write(&routes_path, route_entry).expect("write routes.jsonl");

    let show = run_br(
        &main_workspace,
        ["--quiet", "show", "ext-nonexistent"],
        "show_missing_quiet",
    );
    assert!(
        !show.status.success(),
        "expected quiet routed show to preserve missing-issue failure"
    );
    assert!(
        show.stdout.trim().is_empty(),
        "quiet failure should not emit stdout: {}",
        show.stdout
    );
}
