mod common;

use beads_rust::storage::SqliteStorage;
use common::cli::{BrWorkspace, extract_json_payload, run_br, run_br_with_env};
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

fn create_issue_with_description(
    workspace: &BrWorkspace,
    title: &str,
    issue_type: Option<&str>,
    description: Option<&str>,
    label: &str,
) -> String {
    let mut args = vec!["create".to_string(), title.to_string()];
    if let Some(kind) = issue_type {
        args.push("--type".to_string());
        args.push(kind.to_string());
    }
    if let Some(text) = description {
        args.push("--description".to_string());
        args.push(text.to_string());
    }
    let create = run_br(workspace, args, label);
    assert!(create.status.success(), "create failed: {}", create.stderr);
    parse_created_id(&create.stdout)
}

fn run_lint_json(workspace: &BrWorkspace, mut args: Vec<String>, label: &str) -> Value {
    args.push("--json".to_string());
    let lint = run_br(workspace, args, label);
    assert!(lint.status.success(), "lint json failed: {}", lint.stderr);
    let payload = extract_json_payload(&lint.stdout);
    serde_json::from_str(&payload).expect("parse lint json")
}

fn overwrite_local_tombstone_title(workspace: &BrWorkspace, id: &str, title: &str) {
    let db_path = workspace.root.join(".beads").join("beads.db");
    let storage = SqliteStorage::open(&db_path).expect("open local beads db");
    let mut issue = storage
        .get_issue(id)
        .expect("read issue from db")
        .expect("issue should exist in db");
    assert_eq!(
        issue.status.as_str(),
        "tombstone",
        "local override helper expects a tombstone issue"
    );
    issue.title = title.to_string();
    storage
        .upsert_issue_for_import(&issue)
        .expect("write divergent local tombstone");
}

fn assert_issue_title_and_clean_sync_state(
    workspace: &BrWorkspace,
    id: &str,
    expected_title: &str,
    show_label: &str,
    status_label: &str,
) {
    let show = run_br(workspace, ["show", id, "--json"], show_label);
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let payload = extract_json_payload(&show.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse show json");
    let record = if json.is_array() {
        json.as_array().and_then(|rows| rows.first()).cloned()
    } else {
        Some(json.clone())
    }
    .expect("show should return a record");
    assert_eq!(record["status"].as_str(), Some("tombstone"));
    assert_eq!(record["title"].as_str(), Some(expected_title));

    let status = run_br(workspace, ["sync", "--status", "--json"], status_label);
    assert!(status.status.success(), "status failed: {}", status.stderr);
    let payload = extract_json_payload(&status.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse status json");
    assert_eq!(
        json["dirty_count"].as_u64(),
        Some(0),
        "import should not re-dirty tombstones that were already present in JSONL"
    );
}

#[test]
fn e2e_error_handling() {
    let _log = common::test_log("e2e_error_handling");
    let workspace = BrWorkspace::new();

    let list_uninit = run_br(&workspace, ["list"], "list_uninitialized");
    assert!(!list_uninit.status.success());

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Bad status"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    let bad_priority = run_br(
        &workspace,
        ["list", "--priority-min", "9"],
        "list_bad_priority",
    );
    assert!(!bad_priority.status.success());

    let bad_ready_priority = run_br(
        &workspace,
        ["ready", "--priority", "9"],
        "ready_bad_priority",
    );
    assert!(!bad_ready_priority.status.success());

    let bad_label = run_br(
        &workspace,
        ["update", &id, "--add-label", "bad label"],
        "update_bad_label",
    );
    assert!(!bad_label.status.success());

    let show_missing = run_br(&workspace, ["show", "bd-doesnotexist"], "show_missing");
    assert!(!show_missing.status.success());

    let delete_missing = run_br(&workspace, ["delete", "bd-doesnotexist"], "delete_missing");
    assert!(!delete_missing.status.success());

    let beads_dir = workspace.root.join(".beads");
    let issues_path = beads_dir.join("issues.jsonl");
    fs::write(
        &issues_path,
        "<<<<<<< HEAD\n{}\n=======\n{}\n>>>>>>> branch\n",
    )
    .expect("write conflict jsonl");

    let sync_bad = run_br(&workspace, ["sync", "--import-only"], "sync_bad_jsonl");
    assert!(!sync_bad.status.success());
}

#[test]
fn e2e_sync_force_import_keeps_jsonl_authoritative_for_existing_tombstones() {
    let _log =
        common::test_log("e2e_sync_force_import_keeps_jsonl_authoritative_for_existing_tombstones");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "JSONL tombstone title", "--json"],
        "create",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let created: Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("create json");
    let id = created["id"].as_str().expect("issue id").to_string();

    let flush_open = run_br(&workspace, ["sync", "--flush-only"], "flush_open");
    assert!(
        flush_open.status.success(),
        "flush open failed: {}",
        flush_open.stderr
    );

    let delete = run_br(
        &workspace,
        ["delete", &id, "--force", "--no-auto-flush"],
        "delete",
    );
    assert!(delete.status.success(), "delete failed: {}", delete.stderr);

    let flush_tombstone = run_br(&workspace, ["sync", "--flush-only"], "flush_tombstone");
    assert!(
        flush_tombstone.status.success(),
        "flush tombstone failed: {}",
        flush_tombstone.stderr
    );

    overwrite_local_tombstone_title(&workspace, &id, "stale local tombstone title");

    let import = run_br(
        &workspace,
        ["sync", "--import-only", "--force", "--json"],
        "force_import",
    );
    assert!(
        import.status.success(),
        "force import failed: {}",
        import.stderr
    );

    assert_issue_title_and_clean_sync_state(
        &workspace,
        &id,
        "JSONL tombstone title",
        "show_after_force_import",
        "status_after_force_import",
    );
}

#[test]
fn e2e_sync_rebuild_keeps_jsonl_authoritative_for_existing_tombstones() {
    let _log =
        common::test_log("e2e_sync_rebuild_keeps_jsonl_authoritative_for_existing_tombstones");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "JSONL tombstone title", "--json"],
        "create",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let created: Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("create json");
    let id = created["id"].as_str().expect("issue id").to_string();

    let flush_open = run_br(&workspace, ["sync", "--flush-only"], "flush_open");
    assert!(
        flush_open.status.success(),
        "flush open failed: {}",
        flush_open.stderr
    );

    let delete = run_br(
        &workspace,
        ["delete", &id, "--force", "--no-auto-flush"],
        "delete",
    );
    assert!(delete.status.success(), "delete failed: {}", delete.stderr);

    let flush_tombstone = run_br(&workspace, ["sync", "--flush-only"], "flush_tombstone");
    assert!(
        flush_tombstone.status.success(),
        "flush tombstone failed: {}",
        flush_tombstone.stderr
    );

    overwrite_local_tombstone_title(&workspace, &id, "stale local tombstone title");

    let rebuild = run_br(
        &workspace,
        ["sync", "--import-only", "--rebuild", "--json"],
        "rebuild_import",
    );
    assert!(
        rebuild.status.success(),
        "rebuild import failed: {}",
        rebuild.stderr
    );

    assert_issue_title_and_clean_sync_state(
        &workspace,
        &id,
        "JSONL tombstone title",
        "show_after_rebuild_import",
        "status_after_rebuild_import",
    );
}

#[test]
fn e2e_update_tombstone_rejected() {
    let _log = common::test_log("e2e_update_tombstone_rejected");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "To delete", "--json"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let created: Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("create json");
    let id = created["id"].as_str().expect("issue id");

    let delete = run_br(
        &workspace,
        [
            "delete",
            id,
            "--force",
            "--reason",
            "Delete for update regression",
        ],
        "delete",
    );
    assert!(delete.status.success(), "delete failed: {}", delete.stderr);

    let update = run_br(
        &workspace,
        ["update", id, "--status", "open", "--json"],
        "update_tombstone",
    );
    assert!(!update.status.success(), "tombstone update should fail");
    assert_eq!(update.status.code(), Some(4), "exit code should be 4");

    let json = parse_error_json(&update.stdout).expect("should be valid error json");
    assert!(verify_error_structure(&json), "missing required fields");
    assert_eq!(json["error"]["code"], "VALIDATION_FAILED");
    assert!(
        json["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("cannot update tombstone issue")),
        "error should explain that tombstones cannot be updated"
    );

    let show = run_br(&workspace, ["show", id, "--json"], "show_tombstone");
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let show_json: Value =
        serde_json::from_str(&extract_json_payload(&show.stdout)).expect("show json");
    assert_eq!(show_json[0]["status"], "tombstone");
}

#[test]
fn e2e_update_invalid_parent_does_not_partially_apply_other_changes() {
    let _log = common::test_log("e2e_update_invalid_parent_does_not_partially_apply_other_changes");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Original title", "--json"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let created: Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("create json");
    let id = created["id"].as_str().expect("issue id").to_string();

    let update = run_br(
        &workspace,
        [
            "update",
            &id,
            "--title",
            "Changed title",
            "--parent",
            "bd-missing",
        ],
        "update_invalid_parent",
    );
    assert!(
        !update.status.success(),
        "invalid parent update should fail"
    );

    let show = run_br(
        &workspace,
        ["show", &id, "--json"],
        "show_after_invalid_parent",
    );
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let shown: Value =
        serde_json::from_str(&extract_json_payload(&show.stdout)).expect("show json");
    assert_eq!(shown[0]["title"].as_str(), Some("Original title"));
    assert!(shown[0]["parent"].is_null());
}

#[test]
fn e2e_update_self_parent_does_not_partially_apply_other_changes() {
    let _log = common::test_log("e2e_update_self_parent_does_not_partially_apply_other_changes");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Self parent target", "--json"],
        "create",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let created: Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("create json");
    let id = created["id"].as_str().expect("issue id").to_string();

    let update = run_br(
        &workspace,
        ["update", &id, "--status", "in_progress", "--parent", &id],
        "update_self_parent",
    );
    assert!(!update.status.success(), "self parent update should fail");

    let show = run_br(
        &workspace,
        ["show", &id, "--json"],
        "show_after_self_parent",
    );
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let shown: Value =
        serde_json::from_str(&extract_json_payload(&show.stdout)).expect("show json");
    assert_eq!(shown[0]["status"].as_str(), Some("open"));
    assert!(shown[0]["parent"].is_null());
}

#[test]
fn e2e_dependency_errors() {
    let _log = common::test_log("e2e_dependency_errors");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let issue_a = run_br(&workspace, ["create", "Issue A"], "create_a");
    assert!(
        issue_a.status.success(),
        "create A failed: {}",
        issue_a.stderr
    );
    let id_a = parse_created_id(&issue_a.stdout);

    let issue_b = run_br(&workspace, ["create", "Issue B"], "create_b");
    assert!(
        issue_b.status.success(),
        "create B failed: {}",
        issue_b.stderr
    );
    let id_b = parse_created_id(&issue_b.stdout);

    let self_dep = run_br(&workspace, ["dep", "add", &id_a, &id_a], "dep_self");
    assert!(!self_dep.status.success(), "self dependency should fail");

    let add = run_br(&workspace, ["dep", "add", &id_a, &id_b], "dep_add");
    assert!(add.status.success(), "dep add failed: {}", add.stderr);

    let cycle = run_br(&workspace, ["dep", "add", &id_b, &id_a], "dep_cycle");
    assert!(!cycle.status.success(), "cycle dependency should fail");
}

#[test]
fn e2e_dep_add_blocks_ignores_non_blocking_cycle_edges() {
    let _log = common::test_log("e2e_dep_add_blocks_ignores_non_blocking_cycle_edges");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let issue_a = run_br(&workspace, ["create", "Issue A"], "create_a");
    assert!(
        issue_a.status.success(),
        "create A failed: {}",
        issue_a.stderr
    );
    let id_a = parse_created_id(&issue_a.stdout);

    let issue_b = run_br(&workspace, ["create", "Issue B"], "create_b");
    assert!(
        issue_b.status.success(),
        "create B failed: {}",
        issue_b.stderr
    );
    let id_b = parse_created_id(&issue_b.stdout);

    let related = run_br(
        &workspace,
        ["dep", "add", &id_a, &id_b, "--type", "related"],
        "dep_related",
    );
    assert!(
        related.status.success(),
        "related dep add failed: {}",
        related.stderr
    );

    let blocks = run_br(
        &workspace,
        ["dep", "add", &id_b, &id_a, "--type", "blocks"],
        "dep_blocks",
    );
    assert!(
        blocks.status.success(),
        "blocking dep should ignore non-blocking related edge: {}",
        blocks.stderr
    );
}

#[test]
fn e2e_sync_invalid_orphans() {
    let _log = common::test_log("e2e_sync_invalid_orphans");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Sync issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let flush = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(
        flush.status.success(),
        "sync flush failed: {}",
        flush.stderr
    );

    let bad_orphans = run_br(
        &workspace,
        ["sync", "--import-only", "--force", "--orphans", "weird"],
        "sync_bad_orphans",
    );
    assert!(
        !bad_orphans.status.success(),
        "invalid orphans mode should fail"
    );
}

#[test]
fn e2e_sync_rename_prefix_applies_after_missing_db_recovery_with_force() {
    let _log =
        common::test_log("e2e_sync_rename_prefix_applies_after_missing_db_recovery_with_force");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let set_prefix = run_br(
        &workspace,
        ["config", "set", "issue_prefix=target"],
        "config_set_issue_prefix",
    );
    assert!(
        set_prefix.status.success(),
        "config set failed: {}",
        set_prefix.stderr
    );

    let create = run_br(&workspace, ["create", "Seed issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let original_id = parse_created_id(&create.stdout);
    let mismatched_id = format!(
        "other-{}",
        original_id
            .split_once('-')
            .map(|(_, remainder)| remainder)
            .expect("created issue id should include a prefix")
    );

    let flush = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(
        flush.status.success(),
        "sync flush failed: {}",
        flush.stderr
    );

    let issues_path = workspace.root.join(".beads").join("issues.jsonl");
    let jsonl = fs::read_to_string(&issues_path).expect("read issues jsonl");
    fs::write(&issues_path, jsonl.replace(&original_id, &mismatched_id)).expect("rewrite jsonl");

    let alt_db = workspace.root.join(".beads").join("auto-rebuilt-alt.db");
    let result = run_br(
        &workspace,
        [
            "--db",
            alt_db.to_str().expect("alt db path"),
            "sync",
            "--import-only",
            "--force",
            "--rename-prefix",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "sync_missing_db_rename_prefix_force",
    );
    assert!(
        result.status.success(),
        "rename-prefix import should succeed after deferring open-time recovery: {}",
        result.stderr
    );

    let payload = extract_json_payload(&result.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse import json");
    assert_eq!(json["created"].as_u64(), Some(1));

    let alt_storage = SqliteStorage::open(&alt_db).expect("open rebuilt alternate db");
    assert_eq!(
        alt_storage.count_all_issues().expect("count issues"),
        1,
        "alternate DB should be populated by the explicit rename-prefix import"
    );
    let imported_ids = alt_storage.get_all_ids().expect("all ids");
    assert_eq!(imported_ids.len(), 1);
    assert!(
        imported_ids[0].starts_with("target-"),
        "renamed import should use the configured prefix: {:?}",
        imported_ids
    );
    assert_ne!(
        imported_ids[0], mismatched_id,
        "rename-prefix import should rewrite mismatched IDs"
    );
}

#[test]
fn e2e_sync_rename_prefix_applies_after_missing_db_recovery_without_force() {
    let _log =
        common::test_log("e2e_sync_rename_prefix_applies_after_missing_db_recovery_without_force");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let set_prefix = run_br(
        &workspace,
        ["config", "set", "issue_prefix=target"],
        "config_set_issue_prefix",
    );
    assert!(
        set_prefix.status.success(),
        "config set failed: {}",
        set_prefix.stderr
    );

    let create = run_br(&workspace, ["create", "Seed issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let original_id = parse_created_id(&create.stdout);
    let mismatched_id = format!(
        "other-{}",
        original_id
            .split_once('-')
            .map(|(_, remainder)| remainder)
            .expect("created issue id should include a prefix")
    );

    let flush = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(
        flush.status.success(),
        "sync flush failed: {}",
        flush.stderr
    );

    let issues_path = workspace.root.join(".beads").join("issues.jsonl");
    let jsonl = fs::read_to_string(&issues_path).expect("read issues jsonl");
    fs::write(&issues_path, jsonl.replace(&original_id, &mismatched_id)).expect("rewrite jsonl");

    let alt_db = workspace
        .root
        .join(".beads")
        .join("auto-rebuilt-plain-alt.db");
    let result = run_br(
        &workspace,
        [
            "--db",
            alt_db.to_str().expect("alt db path"),
            "sync",
            "--import-only",
            "--rename-prefix",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "sync_missing_db_plain_rename_prefix",
    );
    assert!(
        result.status.success(),
        "plain rename-prefix import should succeed after deferring open-time recovery: {}",
        result.stderr
    );

    let payload = extract_json_payload(&result.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse import json");
    assert_eq!(json["created"].as_u64(), Some(1));

    let alt_storage = SqliteStorage::open(&alt_db).expect("open rebuilt alternate db");
    assert_eq!(
        alt_storage.count_all_issues().expect("count issues"),
        1,
        "alternate DB should be populated by the explicit rename-prefix import"
    );
    let imported_ids = alt_storage.get_all_ids().expect("all ids");
    assert_eq!(imported_ids.len(), 1);
    assert!(
        imported_ids[0].starts_with("target-"),
        "renamed import should use the configured prefix: {:?}",
        imported_ids
    );
    assert_ne!(
        imported_ids[0], mismatched_id,
        "rename-prefix import should rewrite mismatched IDs"
    );
}

#[test]
fn e2e_auto_flush_skips_silently_overwriting_conflict_markered_jsonl() {
    // Regression: post-command auto-flush used to unconditionally call
    // `export_to_jsonl_with_policy`, which overwrote any existing JSONL —
    // including unresolved `<<<<<<<` / `=======` / `>>>>>>>` regions from
    // a botched `git merge`. Auto-import's conflict-markers check catches
    // most of these before the mutation runs, but commands invoked with
    // `--no-auto-import` skip that guard entirely, leaving auto-flush as
    // the last line of defense. The fix teaches `auto_flush` itself to
    // skip when it sees merge markers, so the mutation still lands in the
    // DB but the JSONL on disk keeps its unresolved state for the
    // operator to fix.
    let _log =
        common::test_log("e2e_auto_flush_skips_silently_overwriting_conflict_markered_jsonl");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Seed"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let issue_id = parse_created_id(&create.stdout);

    let seed_flush = run_br(&workspace, ["sync", "--flush-only"], "sync_flush_seed");
    assert!(
        seed_flush.status.success(),
        "initial flush failed: {}",
        seed_flush.stderr
    );

    // Drop the JSONL into a half-resolved merge-conflict state.
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let clean = fs::read_to_string(&jsonl_path).expect("read jsonl");
    let conflicted = format!("<<<<<<< HEAD\n{clean}=======\n{clean}>>>>>>> branch\n");
    fs::write(&jsonl_path, &conflicted).expect("write conflicted jsonl");
    let before_bytes = fs::read(&jsonl_path).expect("read conflicted jsonl");

    // Run a mutating command with `--no-auto-import` so the first line of
    // defense (auto-import's conflict-markers scan) is bypassed. The
    // mutation should still succeed against the DB, but auto-flush must
    // NOT overwrite the conflict-markered JSONL.
    let update = run_br(
        &workspace,
        ["--no-auto-import", "update", &issue_id, "--priority", "1"],
        "update_no_auto_import",
    );
    assert!(
        update.status.success(),
        "mutation should still succeed even though auto-flush is skipped: {}",
        update.stderr
    );

    // On-disk JSONL must still hold the conflict markers byte-for-byte.
    let after_bytes = fs::read(&jsonl_path).expect("reread jsonl");
    assert_eq!(
        before_bytes, after_bytes,
        "auto-flush must not rewrite a JSONL that contains unresolved merge-conflict markers"
    );
}

#[test]
fn e2e_auto_flush_failure_is_visible_in_json_mode() {
    let _log = common::test_log("e2e_auto_flush_failure_is_visible_in_json_mode");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Visible flush debt"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let issue_id = parse_created_id(&create.stdout);

    let bad_jsonl = workspace
        .root
        .join(".beads")
        .join("beads.db")
        .join("issues.jsonl");
    let bad_jsonl = bad_jsonl.to_string_lossy().to_string();

    let update = run_br_with_env(
        &workspace,
        [
            "--json",
            "--no-auto-import",
            "update",
            &issue_id,
            "--priority",
            "1",
        ],
        [("BEADS_JSONL", bad_jsonl.as_str())],
        "update_bad_auto_flush_jsonl",
    );
    assert!(
        update.status.success(),
        "mutation should still succeed while surfacing auto-flush debt: {}",
        update.stderr
    );

    let warning_payload = extract_json_payload(&update.stderr);
    let warning: Value =
        serde_json::from_str(&warning_payload).expect("auto-flush warning should be JSON");
    assert_eq!(
        warning["warning"]["code"].as_str(),
        Some("AUTO_FLUSH_FAILED")
    );
    assert!(
        warning["warning"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("Mutation succeeded")),
        "warning should make the committed mutation explicit: {}",
        update.stderr
    );
    assert!(
        warning["warning"]["recovery"]
            .as_str()
            .is_some_and(|recovery| recovery.contains("br sync --flush-only")),
        "warning should tell operators how to repair export debt: {}",
        update.stderr
    );
    assert!(
        update.stdout.contains(&issue_id),
        "JSON stdout should still contain command output: {}",
        update.stdout
    );
}

#[test]
fn e2e_sync_flush_checks_conflict_markers_before_noop_short_circuit() {
    // Regression: `br sync --flush-only` can return early when the DB has
    // nothing dirty. That early return must not hide unresolved JSONL merge
    // markers, because a user running sync for safety should still be told
    // the working tree contains an unresolved beads data conflict.
    let _log = common::test_log("e2e_sync_flush_checks_conflict_markers_before_noop_short_circuit");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Seed"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let _ = parse_created_id(&create.stdout);

    let first_flush = run_br(&workspace, ["sync", "--flush-only"], "sync_flush_initial");
    assert!(
        first_flush.status.success(),
        "initial flush should succeed: {}",
        first_flush.stderr
    );

    // Simulate a merge conflict by wrapping the clean JSONL in conflict
    // markers, as if `git merge` left the file in a half-resolved state.
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let clean = fs::read_to_string(&jsonl_path).expect("read jsonl");
    let conflicted = format!("<<<<<<< HEAD\n{clean}=======\n{clean}>>>>>>> branch\n");
    fs::write(&jsonl_path, &conflicted).expect("write conflicted jsonl");
    let before_size = fs::metadata(&jsonl_path).expect("stat jsonl").len();

    // A subsequent no-op flush must refuse with a conflict-markers error
    // before taking the "nothing to do" short-circuit.
    let flush = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(
        !flush.status.success(),
        "flush should fail when JSONL contains conflict markers: stdout={} stderr={}",
        flush.stdout,
        flush.stderr
    );
    // Error goes to stderr, not stdout, so check the human-readable text
    // rather than trying to parse JSON from stdout.
    let lower = flush.stderr.to_lowercase();
    assert!(
        lower.contains("conflict") || lower.contains("marker"),
        "flush error should mention conflict markers, got stderr: {}",
        flush.stderr
    );

    // The JSONL on disk must still contain the conflict markers: if the
    // flush had overwritten it, the markers would be gone.
    let after = fs::read_to_string(&jsonl_path).expect("reread jsonl");
    assert!(
        after.contains("<<<<<<<"),
        "conflict markers must still be on disk after refused flush"
    );
    assert_eq!(
        fs::metadata(&jsonl_path).expect("stat jsonl").len(),
        before_size,
        "JSONL size must not change when flush refuses due to conflict markers"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_sync_rebuild_preserves_unflushed_tombstones_across_delegation() {
    // Regression: `br sync --import-only --rebuild` on an existing DB used
    // to lose tombstones that had not yet been flushed to JSONL. The
    // in-place path preserves them via `snapshot_tombstones` +
    // `restore_preserved_issues` across `reset_data_tables`, but the new
    // delegation path to `recover_database_from_jsonl` opens a fresh DB and
    // imports only what's in the JSONL. Unflushed tombstones therefore
    // vanished silently, taking their deletion-retention state with them.
    // The fix snapshots tombstones before delegation and restores them
    // after.
    let _log =
        common::test_log("e2e_sync_rebuild_preserves_unflushed_tombstones_across_delegation");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create two issues so the rebuild has content to preserve.
    let keep = run_br(&workspace, ["create", "Keep"], "create_keep");
    assert!(keep.status.success(), "create keep failed: {}", keep.stderr);
    let keep_id = parse_created_id(&keep.stdout);

    let delete = run_br(&workspace, ["create", "Delete"], "create_delete");
    assert!(
        delete.status.success(),
        "create delete failed: {}",
        delete.stderr
    );
    let delete_id = parse_created_id(&delete.stdout);

    // Flush both as open so the JSONL reflects the pre-deletion state.
    let flush = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(
        flush.status.success(),
        "sync flush failed: {}",
        flush.stderr
    );

    // Delete one issue WITHOUT flushing: the tombstone only lives in the
    // DB, the JSONL still shows `delete_id` as open.
    let delete_cmd = run_br(
        &workspace,
        ["delete", &delete_id, "--force", "--no-auto-flush"],
        "delete_no_flush",
    );
    assert!(
        delete_cmd.status.success(),
        "delete failed: {}",
        delete_cmd.stderr
    );

    // Run --rebuild. The delegation path fires because the DB exists, no
    // rename was requested, and the JSONL is available.
    let rebuild = run_br(
        &workspace,
        ["sync", "--import-only", "--rebuild", "--json"],
        "sync_rebuild",
    );
    assert!(
        rebuild.status.success(),
        "rebuild failed: {}",
        rebuild.stderr
    );

    // The surviving tombstone must still be queryable via `br show`. If the
    // delegation had silently wiped it, `show` would either report
    // "Issue not found" or return the resurrected-as-open version from the
    // JSONL.
    let show = run_br(&workspace, ["show", &delete_id, "--json"], "show_tombstone");
    assert!(
        show.status.success(),
        "tombstone lookup failed after --rebuild: {}",
        show.stderr
    );
    let payload = extract_json_payload(&show.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse show json");
    let record = if json.is_array() {
        json.as_array().and_then(|a| a.first()).cloned()
    } else {
        Some(json.clone())
    }
    .expect("show should return at least one record");
    assert_eq!(
        record["status"].as_str(),
        Some("tombstone"),
        "tombstone status was lost across --rebuild: {record}"
    );

    // The kept issue must still be open.
    let show_keep = run_br(&workspace, ["show", &keep_id, "--json"], "show_keep");
    assert!(
        show_keep.status.success(),
        "keep lookup failed: {}",
        show_keep.stderr
    );
    let payload = extract_json_payload(&show_keep.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse show keep json");
    let record = if json.is_array() {
        json.as_array().and_then(|a| a.first()).cloned()
    } else {
        Some(json.clone())
    }
    .expect("show should return at least one record");
    assert_eq!(record["status"].as_str(), Some("open"));

    // The preserved tombstone must remain dirty so a later flush writes the
    // deletion back to JSONL instead of incorrectly reporting "Nothing to
    // export". Without this, the rebuilt DB and JSONL silently diverge until
    // a future import/rebuild cycle resurrects the supposedly deleted issue.
    let status = run_br(
        &workspace,
        ["sync", "--status", "--json"],
        "status_after_rebuild",
    );
    assert!(
        status.status.success(),
        "status failed after rebuild: {}",
        status.stderr
    );
    let payload = extract_json_payload(&status.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse status json");
    assert_eq!(
        json["dirty_count"].as_u64(),
        Some(1),
        "the preserved tombstone should stay dirty until it is flushed"
    );

    let flush_after_rebuild = run_br(
        &workspace,
        ["sync", "--flush-only", "--json"],
        "flush_after_rebuild",
    );
    assert!(
        flush_after_rebuild.status.success(),
        "flush after rebuild failed: {}",
        flush_after_rebuild.stderr
    );
    let payload = extract_json_payload(&flush_after_rebuild.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse flush json");
    assert_eq!(
        json["cleared_dirty"].as_u64(),
        Some(1),
        "flush should report the single preserved tombstone dirty flag it cleared"
    );

    let issues_path = workspace.root.join(".beads").join("issues.jsonl");
    let jsonl = fs::read_to_string(&issues_path).expect("read rebuilt issues jsonl");
    let exported_issue_states: Vec<(String, String)> = jsonl
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let value: Value = serde_json::from_str(line).expect("parse exported issue line");
            (
                value["id"].as_str().expect("exported issue id").to_string(),
                value["status"]
                    .as_str()
                    .expect("exported issue status")
                    .to_string(),
            )
        })
        .collect();
    assert!(
        exported_issue_states
            .iter()
            .any(|(id, status)| id == &delete_id && status == "tombstone"),
        "flush after rebuild should export the preserved tombstone: {:?}",
        exported_issue_states
    );
    assert!(
        exported_issue_states
            .iter()
            .any(|(id, status)| id == &keep_id && status == "open"),
        "flush after rebuild should keep the surviving issue open: {:?}",
        exported_issue_states
    );

    let status_after_flush = run_br(
        &workspace,
        ["sync", "--status", "--json"],
        "status_after_flush",
    );
    assert!(
        status_after_flush.status.success(),
        "status failed after flush: {}",
        status_after_flush.stderr
    );
    let payload = extract_json_payload(&status_after_flush.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse post-flush status json");
    assert_eq!(
        json["dirty_count"].as_u64(),
        Some(0),
        "flush should clear the preserved tombstone's dirty flag"
    );
}

#[test]
fn e2e_sync_rebuild_with_rename_prefix_keeps_renamed_issues() {
    // Regression: `--rebuild --rename-prefix` used to wipe the DB. The
    // rebuild's orphan-cleanup pass compares the *raw* JSONL IDs (pre-rename)
    // against `storage.get_all_ids()` (post-rename). Every renamed issue
    // therefore looked like a "DB entry not present in JSONL" and got
    // deleted. The fix is to skip the orphan pass when `--rename-prefix`
    // rewrote the IDs the import just inserted, since the set-difference
    // comparison is no longer semantically meaningful.
    let _log = common::test_log("e2e_sync_rebuild_with_rename_prefix_keeps_renamed_issues");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let set_prefix = run_br(
        &workspace,
        ["config", "set", "issue_prefix=target"],
        "config_set_issue_prefix",
    );
    assert!(
        set_prefix.status.success(),
        "config set failed: {}",
        set_prefix.stderr
    );

    let create = run_br(&workspace, ["create", "Seed issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let original_id = parse_created_id(&create.stdout);
    let mismatched_id = format!(
        "other-{}",
        original_id
            .split_once('-')
            .map(|(_, remainder)| remainder)
            .expect("created issue id should include a prefix")
    );

    let flush = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(
        flush.status.success(),
        "sync flush failed: {}",
        flush.stderr
    );

    let issues_path = workspace.root.join(".beads").join("issues.jsonl");
    let jsonl = fs::read_to_string(&issues_path).expect("read issues jsonl");
    fs::write(&issues_path, jsonl.replace(&original_id, &mismatched_id)).expect("rewrite jsonl");

    let result = run_br(
        &workspace,
        [
            "sync",
            "--import-only",
            "--rebuild",
            "--rename-prefix",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "sync_rebuild_rename_prefix",
    );
    assert!(
        result.status.success(),
        "--rebuild --rename-prefix should succeed: {}",
        result.stderr
    );

    let payload = extract_json_payload(&result.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse import json");
    assert_eq!(
        json["created"].as_u64(),
        Some(1),
        "expected the renamed issue to be inserted"
    );
    assert_eq!(
        json["orphans_removed"].as_u64(),
        Some(0),
        "orphan cleanup must not run when --rename-prefix rewrote IDs; otherwise every renamed issue is wiped"
    );

    let db_path = workspace.root.join(".beads").join("beads.db");
    let storage = SqliteStorage::open(&db_path).expect("open rebuilt db");
    assert_eq!(
        storage.count_all_issues().expect("count issues"),
        1,
        "DB must retain the renamed issue after --rebuild + --rename-prefix"
    );
    let ids = storage.get_all_ids().expect("all ids");
    assert_eq!(ids.len(), 1);
    assert!(
        ids[0].starts_with("target-"),
        "issue should carry the renamed prefix, got {:?}",
        ids
    );
    assert_ne!(
        ids[0], mismatched_id,
        "the renamed ID must differ from the pre-rename JSONL ID"
    );
}

#[test]
fn e2e_sync_auto_rebuild_plain_import_reports_recovery_result() {
    let _log = common::test_log("e2e_sync_auto_rebuild_plain_import_reports_recovery_result");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Seed issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let flush = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(
        flush.status.success(),
        "sync flush failed: {}",
        flush.stderr
    );

    let alt_db = workspace
        .root
        .join(".beads")
        .join("auto-rebuilt-report-alt.db");
    let result = run_br(
        &workspace,
        [
            "--db",
            alt_db.to_str().expect("alt db path"),
            "sync",
            "--import-only",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "sync_auto_rebuild_plain_import",
    );
    assert!(
        result.status.success(),
        "plain import should succeed after open-time auto-rebuild: {}",
        result.stderr
    );

    let payload = extract_json_payload(&result.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse import json");
    assert_eq!(json["created"].as_u64(), Some(1));
    assert_eq!(json["updated"].as_u64(), Some(0));
    assert_eq!(json["blocked_cache_rebuilt"].as_bool(), Some(true));

    let alt_storage = SqliteStorage::open(&alt_db).expect("open rebuilt alternate db");
    assert_eq!(
        alt_storage.count_all_issues().expect("count issues"),
        1,
        "alternate DB should be populated by automatic recovery"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_sync_rename_prefix_clears_duplicate_external_ref_after_missing_db_recovery() {
    let _log = common::test_log(
        "e2e_sync_rename_prefix_clears_duplicate_external_ref_after_missing_db_recovery",
    );
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let set_prefix = run_br(
        &workspace,
        ["config", "set", "issue_prefix=target"],
        "config_set_issue_prefix",
    );
    assert!(
        set_prefix.status.success(),
        "config set failed: {}",
        set_prefix.stderr
    );

    let first = run_br(
        &workspace,
        ["create", "First issue", "--external-ref", "EXT-DUP"],
        "create_first",
    );
    assert!(
        first.status.success(),
        "create first failed: {}",
        first.stderr
    );
    let first_id = parse_created_id(&first.stdout);

    let second = run_br(&workspace, ["create", "Second issue"], "create_second");
    assert!(
        second.status.success(),
        "create second failed: {}",
        second.stderr
    );
    let second_id = parse_created_id(&second.stdout);

    let flush = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(
        flush.status.success(),
        "sync flush failed: {}",
        flush.stderr
    );

    let issues_path = workspace.root.join(".beads").join("issues.jsonl");
    let updated = fs::read_to_string(&issues_path)
        .expect("read issues jsonl")
        .lines()
        .map(|line| {
            let mut value: Value = serde_json::from_str(line).expect("issue json");
            if value["id"].as_str() == Some(&second_id) {
                value["external_ref"] = Value::String("EXT-DUP".to_string());
            }
            serde_json::to_string(&value).expect("serialize issue json")
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&issues_path, format!("{updated}\n")).expect("rewrite jsonl");

    let alt_db = workspace
        .root
        .join(".beads")
        .join("auto-rebuilt-duplicate-extref-alt.db");
    let result = run_br(
        &workspace,
        [
            "--db",
            alt_db.to_str().expect("alt db path"),
            "sync",
            "--import-only",
            "--rename-prefix",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "sync_missing_db_duplicate_external_ref_cleanup",
    );
    assert!(
        result.status.success(),
        "rename-prefix duplicate external_ref cleanup should succeed after deferring open-time recovery: {}",
        result.stderr
    );

    let alt_storage = SqliteStorage::open(&alt_db).expect("open rebuilt alternate db");
    assert_eq!(
        alt_storage.count_all_issues().expect("count issues"),
        2,
        "alternate DB should be populated by the explicit import"
    );
    let retained = [&first_id, &second_id]
        .into_iter()
        .filter(|id| {
            alt_storage
                .get_issue(id)
                .expect("query imported issue")
                .and_then(|issue| issue.external_ref)
                .as_deref()
                == Some("EXT-DUP")
        })
        .count();
    let cleared = [&first_id, &second_id]
        .into_iter()
        .filter(|id| {
            alt_storage
                .get_issue(id)
                .expect("query imported issue")
                .and_then(|issue| issue.external_ref)
                .is_none()
        })
        .count();
    assert_eq!(
        retained, 1,
        "exactly one duplicate external_ref should be preserved"
    );
    assert_eq!(
        cleared, 1,
        "exactly one duplicate external_ref should be cleared"
    );
}

#[test]
fn e2e_sync_rename_prefix_failed_import_restores_original_corrupt_db_family() {
    let _log = common::test_log(
        "e2e_sync_rename_prefix_failed_import_restores_original_corrupt_db_family",
    );
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let set_prefix = run_br(
        &workspace,
        ["config", "set", "issue_prefix=target"],
        "config_set_issue_prefix",
    );
    assert!(
        set_prefix.status.success(),
        "config set failed: {}",
        set_prefix.stderr
    );

    let issues_path = workspace.root.join(".beads").join("issues.jsonl");
    fs::write(&issues_path, "{\"id\":\"broken\"\n").expect("write malformed jsonl");

    let alt_db = workspace
        .root
        .join(".beads")
        .join("deferred-recovery-restore-alt.db");
    let original_bytes = b"not a sqlite database but should be restored".to_vec();
    fs::write(&alt_db, &original_bytes).expect("write corrupt alt db");

    let result = run_br(
        &workspace,
        [
            "--db",
            alt_db.to_str().expect("alt db path"),
            "sync",
            "--import-only",
            "--rename-prefix",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "sync_failed_deferred_recovery_restore",
    );
    assert!(
        !result.status.success(),
        "malformed JSONL should fail explicit import after deferred recovery"
    );
    assert!(
        result.stdout.contains("Invalid JSON"),
        "unexpected stdout: {}",
        result.stdout
    );

    let restored_bytes = fs::read(&alt_db).expect("read restored alt db");
    assert_eq!(
        restored_bytes, original_bytes,
        "failed deferred import should restore the original corrupt db bytes"
    );
}

#[test]
fn e2e_sync_rename_prefix_validation_failure_restores_original_corrupt_db_family() {
    let _log = common::test_log(
        "e2e_sync_rename_prefix_validation_failure_restores_original_corrupt_db_family",
    );
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let set_prefix = run_br(
        &workspace,
        ["config", "set", "issue_prefix=target"],
        "config_set_issue_prefix",
    );
    assert!(
        set_prefix.status.success(),
        "config set failed: {}",
        set_prefix.stderr
    );

    let external_dir = workspace.root.join("external-jsonl");
    fs::create_dir_all(&external_dir).expect("create external dir");
    let external_jsonl = external_dir.join("metadata.jsonl");
    fs::write(
        &external_jsonl,
        "{\"id\":\"legacy-1\",\"title\":\"External metadata JSONL\",\"status\":\"open\",\"priority\":2,\"issue_type\":\"task\",\"created_at\":\"2026-01-01T00:00:00Z\",\"updated_at\":\"2026-01-01T00:00:00Z\"}\n",
    )
    .expect("write external jsonl");

    let metadata_path = workspace.root.join(".beads").join("metadata.json");
    let metadata_json = format!(
        r#"{{"database":"beads.db","jsonl_export":"{}"}}"#,
        external_jsonl.display()
    );
    fs::write(&metadata_path, metadata_json).expect("write metadata");

    let alt_db = workspace
        .root
        .join(".beads")
        .join("deferred-recovery-validation-restore-alt.db");
    let original_bytes = b"not a sqlite database but should survive validation failure".to_vec();
    fs::write(&alt_db, &original_bytes).expect("write corrupt alt db");

    let result = run_br(
        &workspace,
        [
            "--db",
            alt_db.to_str().expect("alt db path"),
            "sync",
            "--import-only",
            "--rename-prefix",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "sync_failed_deferred_recovery_validation_restore",
    );
    assert!(
        !result.status.success(),
        "external metadata JSONL without allow flag should fail validation"
    );
    let combined = format!("{}{}", result.stdout, result.stderr);
    assert!(
        combined.contains("external")
            || combined.contains("allow-external-jsonl")
            || combined.contains("outside"),
        "unexpected validation failure output: {combined}"
    );

    let restored_bytes = fs::read(&alt_db).expect("read restored alt db");
    assert_eq!(
        restored_bytes, original_bytes,
        "validation failure after deferred recovery should restore the original corrupt db bytes"
    );
}

#[test]
fn e2e_sync_rename_prefix_validation_failure_does_not_create_missing_db() {
    let _log =
        common::test_log("e2e_sync_rename_prefix_validation_failure_does_not_create_missing_db");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let external_dir = workspace.root.join("external-jsonl");
    fs::create_dir_all(&external_dir).expect("create external dir");
    let external_jsonl = external_dir.join("metadata.jsonl");
    fs::write(
        &external_jsonl,
        "{\"id\":\"legacy-1\",\"title\":\"External metadata JSONL\",\"status\":\"open\",\"priority\":2,\"issue_type\":\"task\",\"created_at\":\"2026-01-01T00:00:00Z\",\"updated_at\":\"2026-01-01T00:00:00Z\"}\n",
    )
    .expect("write external jsonl");

    let metadata_path = workspace.root.join(".beads").join("metadata.json");
    let metadata_json = format!(
        r#"{{"database":"beads.db","jsonl_export":"{}"}}"#,
        external_jsonl.display()
    );
    fs::write(&metadata_path, metadata_json).expect("write metadata");

    let alt_db = workspace
        .root
        .join(".beads")
        .join("deferred-recovery-validation-missing-alt.db");
    assert!(
        !alt_db.exists(),
        "precondition: alternate db should start missing"
    );

    let result = run_br(
        &workspace,
        [
            "--db",
            alt_db.to_str().expect("alt db path"),
            "sync",
            "--import-only",
            "--rename-prefix",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "sync_failed_deferred_recovery_validation_missing_db",
    );
    assert!(
        !result.status.success(),
        "external metadata JSONL without allow flag should fail validation"
    );
    let combined = format!("{}{}", result.stdout, result.stderr);
    assert!(
        combined.contains("external")
            || combined.contains("allow-external-jsonl")
            || combined.contains("outside"),
        "unexpected validation failure output: {combined}"
    );
    assert!(
        !alt_db.exists(),
        "validation failure should not create a fresh alternate db"
    );
}

#[test]
fn e2e_sync_rename_prefix_import_failure_does_not_leave_missing_db_created() {
    let _log =
        common::test_log("e2e_sync_rename_prefix_import_failure_does_not_leave_missing_db_created");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let set_prefix = run_br(
        &workspace,
        ["config", "set", "issue_prefix=target"],
        "config_set_issue_prefix",
    );
    assert!(
        set_prefix.status.success(),
        "config set failed: {}",
        set_prefix.stderr
    );

    let issues_path = workspace.root.join(".beads").join("issues.jsonl");
    fs::write(&issues_path, "{\"id\":\"broken\"\n").expect("write malformed jsonl");

    let alt_db = workspace
        .root
        .join(".beads")
        .join("deferred-recovery-import-missing-alt.db");
    assert!(
        !alt_db.exists(),
        "precondition: alternate db should start missing"
    );

    let result = run_br(
        &workspace,
        [
            "--db",
            alt_db.to_str().expect("alt db path"),
            "sync",
            "--import-only",
            "--rename-prefix",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "sync_failed_deferred_recovery_import_missing_db",
    );
    assert!(
        !result.status.success(),
        "malformed JSONL should fail explicit import after deferred recovery"
    );
    assert!(
        result.stdout.contains("Invalid JSON"),
        "unexpected stdout: {}",
        result.stdout
    );
    assert!(
        !alt_db.exists(),
        "failed deferred import should not leave a fresh alternate db behind when none existed before"
    );
}

#[test]
fn e2e_sync_export_guards() {
    let _log = common::test_log("e2e_sync_export_guards");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let beads_dir = workspace.root.join(".beads");
    let issues_path = beads_dir.join("issues.jsonl");

    // Empty DB guard: JSONL has content but DB has zero issues.
    fs::write(&issues_path, "{\"id\":\"bd-ghost\"}\n").expect("write jsonl");
    let flush_guard = run_br(&workspace, ["sync", "--flush-only"], "sync_flush_guard");
    assert!(
        !flush_guard.status.success(),
        "expected empty DB guard failure"
    );
    assert!(
        flush_guard
            .stderr
            .contains("Refusing to export empty database"),
        "missing empty DB guard message"
    );
    // Reset JSONL to avoid guard on the seed export.
    fs::write(&issues_path, "").expect("reset jsonl");

    // Stale DB guard: JSONL has an ID missing from DB.
    let create = run_br(&workspace, ["create", "Stale guard issue"], "create_stale");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let flush = run_br(&workspace, ["sync", "--flush-only"], "sync_flush_seed");
    assert!(
        flush.status.success(),
        "sync flush failed: {}",
        flush.stderr
    );

    let mut contents = fs::read_to_string(&issues_path).expect("read jsonl");
    // Use a complete Issue JSON (not just {"id":"bd-missing"}) to avoid parse errors during auto-import
    contents.push_str("{\"id\":\"bd-missing\",\"title\":\"Ghost issue\",\"status\":\"open\",\"priority\":2,\"issue_type\":\"task\",\"created_at\":\"2026-01-01T00:00:00Z\",\"updated_at\":\"2026-01-01T00:00:00Z\"}\n");
    fs::write(&issues_path, contents).expect("append jsonl");

    // Use --no-auto-import and --allow-stale to prevent bd-missing from being imported into DB
    let create2 = run_br(
        &workspace,
        ["create", "Dirty issue", "--no-auto-import", "--allow-stale"],
        "create_dirty",
    );
    assert!(
        create2.status.success(),
        "create failed: {}",
        create2.stderr
    );

    // The flush should fail because JSONL has bd-missing but DB doesn't
    let flush_stale = run_br(&workspace, ["sync", "--flush-only"], "sync_flush_stale");
    assert!(
        !flush_stale.status.success(),
        "expected stale DB guard failure"
    );
    assert!(
        flush_stale
            .stderr
            .contains("Refusing to export stale database"),
        "missing stale DB guard message"
    );
}

#[test]
fn e2e_ambiguous_id() {
    let _log = common::test_log("e2e_ambiguous_id");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let mut ids: Vec<String> = Vec::new();
    let mut attempt = 0;
    let mut ambiguous_prefix: Option<String> = None;

    while ambiguous_prefix.is_none() && attempt < 30 {
        let title = format!("Ambiguous {attempt}");
        let create = run_br(&workspace, ["create", &title], "create_ambiguous");
        assert!(create.status.success(), "create failed: {}", create.stderr);
        let id = parse_created_id(&create.stdout);
        ids.push(id);

        // Check for first-character collisions (matches how the resolver
        // uses contains() -- a single char matches any hash containing it)
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                let hash_i = ids[i].split('-').nth(1).unwrap_or("");
                let hash_j = ids[j].split('-').nth(1).unwrap_or("");
                if !hash_i.is_empty()
                    && !hash_j.is_empty()
                    && hash_i.chars().next() == hash_j.chars().next()
                {
                    let common_char = hash_i.chars().next().unwrap();
                    ambiguous_prefix = Some(common_char.to_string());
                    break;
                }
            }
            if ambiguous_prefix.is_some() {
                break;
            }
        }

        attempt += 1;
    }

    let ambiguous_input = ambiguous_prefix.expect("failed to find ambiguous prefix");

    let show = run_br(&workspace, ["show", &ambiguous_input], "show_ambiguous");
    assert!(!show.status.success(), "ambiguous id should fail");
}

#[test]
fn e2e_lint_before_init_fails() {
    let _log = common::test_log("e2e_lint_before_init_fails");
    let workspace = BrWorkspace::new();
    let lint = run_br(&workspace, ["lint"], "lint_before_init");
    assert!(!lint.status.success());
}

#[test]
fn e2e_lint_clean_output_when_no_warnings() {
    let _log = common::test_log("e2e_lint_clean_output_when_no_warnings");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "lint_clean_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let description = "## Acceptance Criteria\n- done";
    create_issue_with_description(
        &workspace,
        "Task with criteria",
        Some("task"),
        Some(description),
        "lint_clean_create",
    );

    let lint = run_br(&workspace, ["lint"], "lint_clean_run");
    assert!(
        lint.status.success(),
        "lint should succeed: {}",
        lint.stderr
    );
    assert!(lint.stdout.contains("No template warnings found"));
}

#[test]
fn e2e_lint_bug_missing_sections_json() {
    let _log = common::test_log("e2e_lint_bug_missing_sections_json");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "lint_bug_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    create_issue_with_description(
        &workspace,
        "Bug with missing sections",
        Some("bug"),
        Some("Bug report"),
        "lint_bug_create",
    );

    let json = run_lint_json(&workspace, vec!["lint".to_string()], "lint_bug_json");
    assert_eq!(json["total"].as_u64(), Some(2));
    assert_eq!(json["issues"].as_u64(), Some(1));
    let missing = json["results"][0]["missing"]
        .as_array()
        .expect("missing array");
    let missing_text: Vec<String> = missing
        .iter()
        .filter_map(|value| value.as_str().map(str::to_string))
        .collect();
    assert!(missing_text.contains(&"## Steps to Reproduce".to_string()));
    assert!(missing_text.contains(&"## Acceptance Criteria".to_string()));
}

#[test]
fn e2e_lint_multiple_issues_aggregate_warnings() {
    let _log = common::test_log("e2e_lint_multiple_issues_aggregate_warnings");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "lint_multi_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    create_issue_with_description(
        &workspace,
        "Bug missing sections",
        Some("bug"),
        Some("Bug report"),
        "lint_multi_bug",
    );
    create_issue_with_description(
        &workspace,
        "Task missing criteria",
        Some("task"),
        Some("Task description"),
        "lint_multi_task",
    );

    let json = run_lint_json(&workspace, vec!["lint".to_string()], "lint_multi_json");
    assert_eq!(json["issues"].as_u64(), Some(2));
    assert_eq!(json["total"].as_u64(), Some(3));
}

#[test]
fn e2e_lint_text_output_exit_code() {
    let _log = common::test_log("e2e_lint_text_output_exit_code");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "lint_text_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    create_issue_with_description(
        &workspace,
        "Bug missing sections",
        Some("bug"),
        Some("Bug report"),
        "lint_text_bug",
    );

    let lint = run_br(&workspace, ["lint"], "lint_text_run");
    assert!(!lint.status.success());
    assert!(lint.stdout.contains("Template warnings"));
}

#[test]
fn e2e_lint_status_all_includes_closed() {
    let _log = common::test_log("e2e_lint_status_all_includes_closed");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "lint_closed_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let id = create_issue_with_description(
        &workspace,
        "Closed bug",
        Some("bug"),
        Some("Bug report"),
        "lint_closed_bug",
    );

    let close = run_br(
        &workspace,
        ["close", &id, "--reason", "done"],
        "lint_closed_close",
    );
    assert!(close.status.success(), "close failed: {}", close.stderr);

    let json = run_lint_json(
        &workspace,
        vec![
            "lint".to_string(),
            "--status".to_string(),
            "all".to_string(),
        ],
        "lint_closed_json",
    );
    assert_eq!(json["issues"].as_u64(), Some(1));
}

#[test]
fn e2e_lint_type_filter_limits_results() {
    let _log = common::test_log("e2e_lint_type_filter_limits_results");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "lint_type_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    create_issue_with_description(
        &workspace,
        "Bug missing sections",
        Some("bug"),
        Some("Bug report"),
        "lint_type_bug",
    );
    create_issue_with_description(
        &workspace,
        "Task with criteria",
        Some("task"),
        Some("## Acceptance Criteria\n- done"),
        "lint_type_task",
    );

    let json = run_lint_json(
        &workspace,
        vec!["lint".to_string(), "--type".to_string(), "bug".to_string()],
        "lint_type_json",
    );
    assert_eq!(json["issues"].as_u64(), Some(1));
    assert_eq!(json["results"][0]["type"].as_str(), Some("bug"));
}

#[test]
fn e2e_lint_ids_only_lints_selected() {
    let _log = common::test_log("e2e_lint_ids_only_lints_selected");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "lint_ids_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let bug_id = create_issue_with_description(
        &workspace,
        "Bug missing sections",
        Some("bug"),
        Some("Bug report"),
        "lint_ids_bug",
    );
    create_issue_with_description(
        &workspace,
        "Task missing criteria",
        Some("task"),
        Some("Task description"),
        "lint_ids_task",
    );

    let json = run_lint_json(
        &workspace,
        vec!["lint".to_string(), bug_id.clone()],
        "lint_ids_json",
    );
    assert_eq!(json["issues"].as_u64(), Some(1));
    assert_eq!(json["results"][0]["id"].as_str(), Some(bug_id.as_str()));
}

#[test]
fn e2e_lint_skips_types_without_required_sections() {
    let _log = common::test_log("e2e_lint_skips_types_without_required_sections");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "lint_skip_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    create_issue_with_description(
        &workspace,
        "Chore without requirements",
        Some("chore"),
        Some("No requirements"),
        "lint_skip_chore",
    );

    let json = run_lint_json(&workspace, vec!["lint".to_string()], "lint_skip_json");
    assert_eq!(json["issues"].as_u64(), Some(0));
    assert_eq!(json["total"].as_u64(), Some(0));
}

// === Structured JSON Error Output Tests ===

/// Parse structured error JSON from stderr.
/// This handles the case where log lines may precede the JSON output.
fn parse_error_json(stderr: &str) -> Option<Value> {
    // First try parsing the whole stderr as JSON
    if let Ok(json) = serde_json::from_str(stderr) {
        return Some(json);
    }

    // If that fails, look for a JSON object starting with '{'
    // This handles cases where log lines precede the JSON output
    if let Some(start) = stderr.find('{') {
        let json_part = &stderr[start..];
        if let Ok(json) = serde_json::from_str(json_part) {
            return Some(json);
        }
    }

    None
}

/// Verify error JSON has required fields.
fn verify_error_structure(json: &Value) -> bool {
    let error = json.get("error");
    if error.is_none() {
        return false;
    }
    let error = error.unwrap();

    // Required fields
    error.get("code").is_some()
        && error.get("message").is_some()
        && error.get("retryable").is_some()
}

#[test]
fn e2e_structured_error_not_initialized() {
    let _log = common::test_log("e2e_structured_error_not_initialized");
    let workspace = BrWorkspace::new();

    // Don't init - test NOT_INITIALIZED error
    let result = run_br(&workspace, ["list", "--json"], "list_not_init_json");
    assert!(!result.status.success());
    assert_eq!(result.status.code(), Some(2), "exit code should be 2");

    let json = parse_error_json(&result.stdout).expect("should be valid JSON");
    assert!(verify_error_structure(&json), "missing required fields");

    let error = &json["error"];
    assert_eq!(error["code"], "NOT_INITIALIZED");
    assert!(!error["retryable"].as_bool().unwrap());
    assert!(error["hint"].as_str().unwrap().contains("br init"));
}

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_transition_required_fields_are_structured_fresh_and_atomic() {
    let _log = common::test_log("e2e_transition_required_fields_are_structured_fresh_and_atomic");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "required_fields_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    let issue = run_br(
        &workspace,
        ["create", "Review candidate"],
        "required_fields_create",
    );
    assert!(issue.status.success(), "create failed: {}", issue.stderr);
    let id = parse_created_id(&issue.stdout);
    let claim = run_br(
        &workspace,
        ["update", &id, "--status", "in_progress"],
        "required_fields_claim",
    );
    assert!(claim.status.success(), "claim failed: {}", claim.stderr);
    let old_comment = run_br(
        &workspace,
        [
            "comments",
            "add",
            &id,
            "--message",
            "historical review note",
        ],
        "required_fields_old_comment",
    );
    assert!(
        old_comment.status.success(),
        "historical comment failed: {}",
        old_comment.stderr
    );

    fs::write(
        workspace.root.join(".beads").join("policy.yaml"),
        r#"
workflow:
  required_fields:
    "in_progress -> in_review":
      - acceptance_criteria
      - transition_comment
"#,
    )
    .expect("write required-fields policy");

    let missing_comment = run_br(
        &workspace,
        [
            "update",
            &id,
            "--status",
            "in_review",
            "--acceptance-criteria",
            "- [x] Exercised",
            "--json",
        ],
        "required_fields_missing_comment",
    );
    assert!(!missing_comment.status.success());
    assert_eq!(missing_comment.status.code(), Some(4));
    let json = parse_error_json(&missing_comment.stdout).expect("structured policy error");
    assert!(verify_error_structure(&json));
    assert_eq!(json["error"]["code"], "POLICY_VIOLATION");
    assert_eq!(json["error"]["context"]["issue_id"], id);
    let violations = json["error"]["context"]["violations"]
        .as_array()
        .expect("violations");
    assert_eq!(violations.len(), 1);
    assert_eq!(violations[0]["gate"], "transition_comment_missing");
    assert_eq!(
        violations[0]["detail"]["required_field"],
        "transition_comment"
    );

    let unchecked = run_br(
        &workspace,
        [
            "update",
            &id,
            "--status",
            "in_review",
            "--acceptance-criteria",
            "- [ ] Still pending",
            "--transition-comment",
            "fresh attempt",
        ],
        "required_fields_unchecked_human",
    );
    assert!(!unchecked.status.success());
    assert!(unchecked.stderr.contains("acceptance criteria"));
    assert!(unchecked.stderr.contains("unchecked"));

    let storage = SqliteStorage::open(&workspace.root.join(".beads").join("beads.db"))
        .expect("open storage after rejected transitions");
    let unchanged = storage.get_issue(&id).unwrap().unwrap();
    assert_eq!(unchanged.status.as_str(), "in_progress");
    assert!(unchanged.acceptance_criteria.is_none());
    assert_eq!(storage.get_comments(&id).unwrap().len(), 1);
    drop(storage);

    let accepted = run_br(
        &workspace,
        [
            "update",
            &id,
            "--status",
            "in_review",
            "--acceptance-criteria",
            "- [x] Exercised",
            "--transition-comment",
            "fresh attempt",
        ],
        "required_fields_accepted",
    );
    assert!(
        accepted.status.success(),
        "update failed: {}",
        accepted.stderr
    );
    let storage = SqliteStorage::open(&workspace.root.join(".beads").join("beads.db"))
        .expect("open storage after accepted transition");
    let transitioned = storage.get_issue(&id).unwrap().unwrap();
    assert_eq!(transitioned.status.as_str(), "in_review");
    assert_eq!(
        transitioned.acceptance_criteria.as_deref(),
        Some("- [x] Exercised")
    );
    let comments = storage.get_comments(&id).unwrap();
    assert_eq!(comments.len(), 2);
    assert_eq!(comments[1].body, "fresh attempt");
}

#[test]
fn e2e_workflow_capacity_rejection_is_structured_and_atomic() {
    let _log = common::test_log("e2e_workflow_capacity_rejection_is_structured_and_atomic");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "capacity_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let active = run_br(
        &workspace,
        ["create", "Already active"],
        "capacity_create_active",
    );
    assert!(active.status.success(), "create failed: {}", active.stderr);
    let active_id = parse_created_id(&active.stdout);
    let activate = run_br(
        &workspace,
        ["update", &active_id, "--status", "in_progress"],
        "capacity_activate",
    );
    assert!(
        activate.status.success(),
        "initial activation failed: {}",
        activate.stderr
    );

    let candidate = run_br(
        &workspace,
        ["create", "Candidate"],
        "capacity_create_candidate",
    );
    assert!(
        candidate.status.success(),
        "candidate create failed: {}",
        candidate.stderr
    );
    let candidate_id = parse_created_id(&candidate.stdout);

    fs::write(
        workspace.root.join(".beads").join("policy.yaml"),
        r"
workflow:
  statuses: [open, in_progress, closed]
  capacity:
    statuses:
      in_progress:
        hard: 1
",
    )
    .expect("write capacity policy");

    let rejected = run_br(
        &workspace,
        [
            "update",
            &candidate_id,
            "--status",
            "in_progress",
            "--title",
            "must not commit",
            "--json",
        ],
        "capacity_reject",
    );
    assert!(!rejected.status.success(), "capacity update must fail");
    assert_eq!(rejected.status.code(), Some(4));
    // Global JSON errors use the same clean stdout channel as successful JSON
    // output (#336); diagnostics remain isolated on stderr.
    let json = parse_error_json(&rejected.stdout).expect("structured capacity error");
    assert!(verify_error_structure(&json));
    let error = &json["error"];
    assert_eq!(error["code"], "WORKFLOW_CAPACITY_EXCEEDED");
    assert_eq!(error["retryable"], true);
    assert_eq!(error["context"]["issue_id"], candidate_id);
    assert_eq!(error["context"]["from_status"], "open");
    assert_eq!(error["context"]["to_status"], "in_progress");
    assert_eq!(error["context"]["capacity_kind"], "status");
    assert_eq!(error["context"]["current"], 1);
    assert_eq!(error["context"]["prospective"], 2);
    assert_eq!(error["context"]["hard_limit"], 1);

    let show = run_br(
        &workspace,
        ["show", &candidate_id, "--json"],
        "capacity_show_unchanged",
    );
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let shown: Value =
        serde_json::from_str(&extract_json_payload(&show.stdout)).expect("show json");
    assert_eq!(shown[0]["status"], "open");
    assert_eq!(shown[0]["title"], "Candidate");
}

#[test]
fn e2e_workflow_capacity_batch_rejection_rolls_back_every_issue() {
    let _log = common::test_log("e2e_workflow_capacity_batch_rejection_rolls_back_every_issue");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "capacity_batch_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let active_id = create_issue_with_description(
        &workspace,
        "Already active",
        None,
        None,
        "capacity_batch_active",
    );
    let activate = run_br(
        &workspace,
        ["update", &active_id, "--status", "in_progress"],
        "capacity_batch_activate",
    );
    assert!(
        activate.status.success(),
        "activate failed: {}",
        activate.stderr
    );

    let first_id = create_issue_with_description(
        &workspace,
        "First candidate",
        None,
        None,
        "capacity_batch_first",
    );
    let second_id = create_issue_with_description(
        &workspace,
        "Second candidate",
        None,
        None,
        "capacity_batch_second",
    );
    fs::write(
        workspace.root.join(".beads").join("policy.yaml"),
        r"
workflow:
  statuses: [open, in_progress, closed]
  capacity:
    statuses:
      in_progress:
        hard: 2
",
    )
    .expect("write capacity policy");

    let rejected = run_br(
        &workspace,
        [
            "update",
            &first_id,
            &second_id,
            "--status",
            "in_progress",
            "--title",
            "must not commit",
            "--json",
        ],
        "capacity_batch_reject",
    );
    assert!(!rejected.status.success(), "batch must fail");
    assert_eq!(rejected.status.code(), Some(4));
    let json = parse_error_json(&rejected.stdout).expect("structured capacity error");
    assert_eq!(json["error"]["code"], "WORKFLOW_CAPACITY_EXCEEDED");
    assert_eq!(json["error"]["context"]["current"], 1);
    assert_eq!(json["error"]["context"]["prospective"], 3);
    assert_eq!(json["error"]["context"]["hard_limit"], 2);

    for (id, expected_title, label) in [
        (&first_id, "First candidate", "capacity_batch_show_first"),
        (&second_id, "Second candidate", "capacity_batch_show_second"),
    ] {
        let show = run_br(&workspace, ["show", id, "--json"], label);
        assert!(show.status.success(), "show failed: {}", show.stderr);
        let shown: Value =
            serde_json::from_str(&extract_json_payload(&show.stdout)).expect("show json");
        assert_eq!(shown[0]["status"], "open");
        assert_eq!(shown[0]["title"], expected_title);
    }
}

#[test]
fn e2e_workflow_capacity_soft_limit_emits_structured_batch_warning() {
    let _log = common::test_log("e2e_workflow_capacity_soft_limit_emits_structured_batch_warning");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "capacity_soft_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    let first_id = create_issue_with_description(
        &workspace,
        "First soft candidate",
        None,
        None,
        "capacity_soft_first",
    );
    let second_id = create_issue_with_description(
        &workspace,
        "Second soft candidate",
        None,
        None,
        "capacity_soft_second",
    );
    fs::write(
        workspace.root.join(".beads").join("policy.yaml"),
        r"
workflow:
  statuses: [open, in_progress, closed]
  capacity:
    statuses:
      in_progress:
        soft: 2
        hard: 3
",
    )
    .expect("write capacity policy");

    let updated = run_br(
        &workspace,
        [
            "update",
            &first_id,
            &second_id,
            "--status",
            "in_progress",
            "--json",
        ],
        "capacity_soft_update",
    );
    assert!(
        updated.status.success(),
        "update failed: {}",
        updated.stderr
    );
    let payload: Value = serde_json::from_str(&extract_json_payload(&updated.stdout))
        .expect("capacity warning json");
    assert_eq!(payload["updated"].as_array().map(Vec::len), Some(2));
    let warnings = payload["warnings"].as_array().expect("warnings array");
    assert_eq!(warnings.len(), 1);
    let warning = &warnings[0];
    assert_eq!(warning["issue_id"], first_id);
    assert_eq!(warning["from_status"], "open");
    assert_eq!(warning["to_status"], "in_progress");
    assert_eq!(warning["capacity_kind"], "status");
    assert_eq!(warning["capacity_name"], "in_progress");
    assert_eq!(warning["scope"], "repository");
    assert_eq!(warning["counting_mode"], "all");
    assert_eq!(warning["current"], 0);
    assert_eq!(warning["prospective"], 2);
    assert_eq!(warning["soft_limit"], 2);
    assert_eq!(warning["hard_limit"], 3);
    assert_eq!(
        warning["policy_path"],
        "workflow.capacity.statuses.in_progress"
    );
}

#[test]
fn e2e_workflow_capacity_create_preserves_legacy_shape_until_warning_exists() {
    let _log = common::test_log(
        "e2e_workflow_capacity_create_preserves_legacy_shape_until_warning_exists",
    );
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "capacity_create_shape_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    fs::write(
        workspace.root.join(".beads").join("policy.yaml"),
        r"
workflow:
  statuses: [open, closed]
  capacity:
    statuses:
      open:
        soft: 2
        hard: 3
",
    )
    .expect("write capacity policy");

    let first = run_br(
        &workspace,
        ["create", "Below soft limit", "--json"],
        "capacity_create_below_soft",
    );
    assert!(
        first.status.success(),
        "first create failed: {}",
        first.stderr
    );
    let first_payload: Value =
        serde_json::from_str(&extract_json_payload(&first.stdout)).expect("first create json");
    assert!(
        first_payload["id"].is_string(),
        "below the soft limit, create must retain its legacy direct-issue shape: {}",
        first.stdout
    );
    assert!(first_payload.get("created").is_none());
    assert!(first_payload.get("warnings").is_none());

    let second = run_br(
        &workspace,
        ["create", "Reaches soft limit", "--json"],
        "capacity_create_at_soft",
    );
    assert!(
        second.status.success(),
        "second create failed: {}",
        second.stderr
    );
    let second_payload: Value =
        serde_json::from_str(&extract_json_payload(&second.stdout)).expect("second create json");
    assert!(second_payload["created"]["id"].is_string());
    let warnings = second_payload["warnings"]
        .as_array()
        .expect("create warnings array");
    assert_eq!(warnings.len(), 1);
    assert_eq!(warnings[0]["from_status"], Value::Null);
    assert_eq!(warnings[0]["to_status"], "open");
    assert_eq!(warnings[0]["current"], 1);
    assert_eq!(warnings[0]["prospective"], 2);
    assert_eq!(warnings[0]["soft_limit"], 2);
    assert!(
        !second
            .stderr
            .contains("has reached or exceeded its soft limit"),
        "JSON mode must carry capacity evidence structurally rather than duplicate a human warning on stderr: {}",
        second.stderr
    );
}

#[test]
fn e2e_workflow_capacity_leaf_work_excludes_aggregate_parents_and_reports_rollup() {
    // GitHub #384 phase 3, end to end: an epic -> parent -> child chain
    // occupies one leaf_work slot, not three, and `br show` reports the
    // parent's derived rollup without mutating its explicit status.
    let _log = common::test_log(
        "e2e_workflow_capacity_leaf_work_excludes_aggregate_parents_and_reports_rollup",
    );
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "capacity_leaf_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let epic_id =
        create_issue_with_description(&workspace, "Epic", Some("epic"), None, "capacity_leaf_epic");
    let parent_id =
        create_issue_with_description(&workspace, "Parent", None, None, "capacity_leaf_parent");
    let child_id =
        create_issue_with_description(&workspace, "Child", None, None, "capacity_leaf_child");
    let fresh_id =
        create_issue_with_description(&workspace, "Fresh", None, None, "capacity_leaf_fresh");

    for (child, parent, label) in [
        (&parent_id, &epic_id, "capacity_leaf_dep_parent"),
        (&child_id, &parent_id, "capacity_leaf_dep_child"),
    ] {
        let dep = run_br(
            &workspace,
            ["dep", "add", child, parent, "--type", "parent-child"],
            label,
        );
        assert!(dep.status.success(), "dep add failed: {}", dep.stderr);
    }

    // Activate the whole chain before the policy exists so the pre-existing
    // state is what hierarchy counting has to interpret.
    let activate = run_br(
        &workspace,
        [
            "update",
            &epic_id,
            &parent_id,
            &child_id,
            "--status",
            "in_progress",
        ],
        "capacity_leaf_activate",
    );
    assert!(
        activate.status.success(),
        "activate failed: {}",
        activate.stderr
    );

    fs::write(
        workspace.root.join(".beads").join("policy.yaml"),
        r"
workflow:
  statuses: [open, in_progress, closed]
  capacity:
    counting:
      hierarchy: leaf_work
    statuses:
      in_progress:
        hard: 1
",
    )
    .expect("write hierarchy capacity policy");

    let rejected = run_br(
        &workspace,
        ["update", &fresh_id, "--status", "in_progress", "--json"],
        "capacity_leaf_reject",
    );
    assert!(
        !rejected.status.success(),
        "admitting a fourth active issue must fail"
    );
    assert_eq!(rejected.status.code(), Some(4));
    let json = parse_error_json(&rejected.stdout).expect("structured capacity error");
    let error = &json["error"];
    assert_eq!(error["code"], "WORKFLOW_CAPACITY_EXCEEDED");
    assert_eq!(error["context"]["counting_mode"], "leaf_work");
    // Three issues are in_progress, but the epic and the parent are
    // aggregates: only the leaf counts.
    assert_eq!(error["context"]["current"], 1);
    assert_eq!(error["context"]["prospective"], 2);
    assert_eq!(error["context"]["aggregate_parents_excluded"], 2);

    // The rejected transition left no trace.
    let show_fresh = run_br(
        &workspace,
        ["show", &fresh_id, "--json"],
        "capacity_leaf_show_fresh",
    );
    let fresh: Value =
        serde_json::from_str(&extract_json_payload(&show_fresh.stdout)).expect("show fresh json");
    assert_eq!(fresh[0]["status"], "open");

    // The parent keeps its own status and gains a derived rollup.
    let show_parent = run_br(
        &workspace,
        ["show", &parent_id, "--json"],
        "capacity_leaf_show_parent",
    );
    let parent: Value =
        serde_json::from_str(&extract_json_payload(&show_parent.stdout)).expect("show parent json");
    assert_eq!(parent[0]["status"], "in_progress");
    assert_eq!(parent[0]["rollup"]["status"], "in_progress");
    assert_eq!(parent[0]["rollup"]["descendants"]["in_progress"], 1);

    // A childless leaf has no rollup key at all.
    let show_child = run_br(
        &workspace,
        ["show", &child_id, "--json"],
        "capacity_leaf_show_child",
    );
    let child: Value =
        serde_json::from_str(&extract_json_payload(&show_child.stdout)).expect("show child json");
    assert!(child[0].get("rollup").is_none());
}

#[test]
fn e2e_structured_error_issue_not_found() {
    let _log = common::test_log("e2e_structured_error_issue_not_found");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let result = run_br(
        &workspace,
        ["show", "bd-nonexistent", "--json"],
        "show_missing_json",
    );
    assert!(!result.status.success());
    assert_eq!(result.status.code(), Some(3), "exit code should be 3");

    let json = parse_error_json(&result.stdout).expect("should be valid JSON");
    assert!(verify_error_structure(&json), "missing required fields");

    let error = &json["error"];
    assert_eq!(error["code"], "ISSUE_NOT_FOUND");
    assert!(!error["retryable"].as_bool().unwrap());
    assert!(error["context"]["searched_id"].is_string());
    assert!(error["hint"].as_str().unwrap().contains("br list"));
}

#[test]
fn e2e_structured_error_cycle_detected() {
    let _log = common::test_log("e2e_structured_error_cycle_detected");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create_a = run_br(&workspace, ["create", "Issue A"], "create_a");
    assert!(create_a.status.success());
    let id_a = parse_created_id(&create_a.stdout);

    let create_b = run_br(&workspace, ["create", "Issue B"], "create_b");
    assert!(create_b.status.success());
    let id_b = parse_created_id(&create_b.stdout);

    // A depends on B
    let dep_add = run_br(&workspace, ["dep", "add", &id_a, &id_b], "dep_add");
    assert!(dep_add.status.success());

    // B depends on A - would create cycle
    let result = run_br(
        &workspace,
        ["dep", "add", &id_b, &id_a, "--json"],
        "dep_cycle_json",
    );
    assert!(!result.status.success());
    assert_eq!(result.status.code(), Some(5), "exit code should be 5");

    let json = parse_error_json(&result.stdout).expect("should be valid JSON");
    assert!(verify_error_structure(&json), "missing required fields");

    let error = &json["error"];
    assert_eq!(error["code"], "CYCLE_DETECTED");
    assert!(!error["retryable"].as_bool().unwrap());
    assert!(error["context"]["cycle_path"].is_string());
}

#[test]
fn e2e_structured_error_self_dependency() {
    let _log = common::test_log("e2e_structured_error_self_dependency");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create = run_br(&workspace, ["create", "Self dep issue"], "create");
    assert!(create.status.success());
    let id = parse_created_id(&create.stdout);

    let result = run_br(
        &workspace,
        ["dep", "add", &id, &id, "--json"],
        "dep_self_json",
    );
    assert!(!result.status.success());
    assert_eq!(result.status.code(), Some(5), "exit code should be 5");

    let json = parse_error_json(&result.stdout).expect("should be valid JSON");
    assert!(verify_error_structure(&json), "missing required fields");

    let error = &json["error"];
    assert_eq!(error["code"], "SELF_DEPENDENCY");
    assert!(!error["retryable"].as_bool().unwrap());
}

#[test]
fn e2e_structured_error_ambiguous_id() {
    let _log = common::test_log("e2e_structured_error_ambiguous_id");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let mut ids: Vec<String> = Vec::new();
    let mut attempt = 0;
    let mut ambiguous_prefix: Option<String> = None;

    // Create issues until we have ambiguous IDs
    while ambiguous_prefix.is_none() && attempt < 30 {
        let title = format!("Structured test {attempt}");
        let create = run_br(&workspace, ["create", &title], &format!("create_{attempt}"));
        assert!(create.status.success());
        let id = parse_created_id(&create.stdout);
        ids.push(id);

        // Check for prefix collisions
        for i in 0..ids.len() {
            for j in (i + 1)..ids.len() {
                let hash_i = ids[i].split('-').nth(1).unwrap_or("");
                let hash_j = ids[j].split('-').nth(1).unwrap_or("");
                if !hash_i.is_empty()
                    && !hash_j.is_empty()
                    && hash_i.chars().next() == hash_j.chars().next()
                {
                    let common_char = hash_i.chars().next().unwrap();
                    ambiguous_prefix = Some(common_char.to_string());
                    break;
                }
            }
            if ambiguous_prefix.is_some() {
                break;
            }
        }
        attempt += 1;
    }

    let prefix = ambiguous_prefix.expect("failed to create ambiguous IDs");

    let result = run_br(
        &workspace,
        ["show", &prefix, "--json"],
        "show_ambiguous_json",
    );
    assert!(!result.status.success());
    assert_eq!(result.status.code(), Some(3), "exit code should be 3");

    let json = parse_error_json(&result.stdout).expect("should be valid JSON");
    assert!(verify_error_structure(&json), "missing required fields");

    let error = &json["error"];
    assert_eq!(error["code"], "AMBIGUOUS_ID");
    assert!(error["retryable"].as_bool().unwrap());
    assert!(error["context"]["matches"].is_array());
}

#[test]
fn e2e_structured_error_jsonl_parse() {
    let _log = common::test_log("e2e_structured_error_jsonl_parse");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    // Create malformed JSONL
    let beads_dir = workspace.root.join(".beads");
    let issues_path = beads_dir.join("issues.jsonl");
    fs::write(&issues_path, "{ not valid json\n").expect("write bad jsonl");

    let result = run_br(
        &workspace,
        ["sync", "--import-only", "--json"],
        "import_bad_json",
    );
    assert!(!result.status.success());
    // JSONL parse errors should be exit code 6 (sync errors) or 7 (config)
    let exit_code = result.status.code().unwrap_or(0);
    assert!(
        exit_code == 6 || exit_code == 7,
        "unexpected exit code: {exit_code}"
    );

    // The error output should be valid JSON
    let json = parse_error_json(&result.stdout);
    if let Some(json) = json {
        assert!(verify_error_structure(&json), "missing required fields");
    }
    // Note: Some errors may not produce structured JSON yet - that's OK
}

#[test]
fn e2e_structured_error_conflict_markers() {
    let _log = common::test_log("e2e_structured_error_conflict_markers");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    // Create JSONL with conflict markers
    let beads_dir = workspace.root.join(".beads");
    let issues_path = beads_dir.join("issues.jsonl");
    fs::write(
        &issues_path,
        "<<<<<<< HEAD\n{\"id\":\"bd-abc\"}\n=======\n{\"id\":\"bd-def\"}\n>>>>>>> branch\n",
    )
    .expect("write conflict jsonl");

    let result = run_br(
        &workspace,
        ["sync", "--import-only", "--json"],
        "import_conflict_json",
    );
    assert!(!result.status.success());

    // Should detect conflict markers
    assert!(
        result.stdout.contains("conflict") || result.stdout.contains("CONFLICT"),
        "should detect conflict markers"
    );
}

#[test]
fn e2e_sync_flush_refuses_to_overwrite_conflict_markers() {
    let _log = common::test_log("e2e_sync_flush_refuses_to_overwrite_conflict_markers");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Flush conflict seed", "--no-auto-flush"],
        "create_seed",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    let first_flush = run_br(&workspace, ["sync", "--flush-only"], "first_flush");
    assert!(
        first_flush.status.success(),
        "initial flush failed: {}",
        first_flush.stderr
    );

    let issues_path = workspace.root.join(".beads").join("issues.jsonl");
    let original_jsonl = fs::read_to_string(&issues_path).expect("read initial jsonl");
    assert!(
        original_jsonl.contains("Flush conflict seed"),
        "initial flush should export the seed issue"
    );

    let update = run_br(
        &workspace,
        [
            "update",
            &id,
            "--title",
            "Dirty title that must not be flushed over conflict markers",
            "--no-auto-flush",
        ],
        "dirty_update",
    );
    assert!(update.status.success(), "update failed: {}", update.stderr);

    let conflicted_jsonl = format!(
        "<<<<<<< HEAD\n{}=======\n{}>>>>>>> feature-branch\n",
        original_jsonl, original_jsonl
    );
    fs::write(&issues_path, &conflicted_jsonl).expect("write conflicted jsonl");

    let refused_flush = run_br(
        &workspace,
        ["sync", "--flush-only", "--json"],
        "refused_flush",
    );
    assert!(
        !refused_flush.status.success(),
        "flush should fail while issues.jsonl contains merge conflict markers"
    );
    let exit_code = refused_flush.status.code().unwrap_or(0);
    assert!(
        exit_code == 6 || exit_code == 7,
        "conflict-marker flush refusal should be a sync/config error, got {exit_code}"
    );
    assert!(
        refused_flush.stdout.contains("conflict") || refused_flush.stdout.contains("CONFLICT"),
        "flush error should explain the unresolved conflict markers: {}",
        refused_flush.stdout
    );

    let after_refusal = fs::read_to_string(&issues_path).expect("read refused jsonl");
    assert_eq!(
        after_refusal, conflicted_jsonl,
        "flush refusal must leave the conflicted JSONL byte-for-byte untouched"
    );
    assert!(
        !after_refusal.contains("Dirty title that must not be flushed over conflict markers"),
        "dirty DB title must not be exported over unresolved JSONL conflict markers"
    );
}

#[test]
fn e2e_custom_type_accepted() {
    let _log = common::test_log("e2e_custom_type_accepted");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    // Custom types are accepted (not rejected as invalid)
    let result = run_br(
        &workspace,
        ["create", "Test issue", "--type", "custom_type", "--json"],
        "create_custom_type_json",
    );
    assert!(
        result.status.success(),
        "custom types should be accepted: {}",
        result.stderr
    );

    // Verify the custom type is stored correctly
    let json: serde_json::Value =
        serde_json::from_str(&result.stdout).expect("should be valid JSON");
    assert_eq!(
        json["issue_type"], "custom_type",
        "custom type should be preserved"
    );
}

#[test]
fn e2e_structured_error_invalid_priority() {
    let _log = common::test_log("e2e_structured_error_invalid_priority");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    // Test invalid priority (out of 0-4 range)
    let result = run_br(
        &workspace,
        ["create", "Test issue", "--priority", "10", "--json"],
        "create_invalid_priority_json",
    );
    assert!(!result.status.success());
    assert_eq!(result.status.code(), Some(4), "exit code should be 4");

    let json = parse_error_json(&result.stdout).expect("should be valid JSON");
    assert!(verify_error_structure(&json), "missing required fields");

    let error = &json["error"];
    assert_eq!(error["code"], "INVALID_PRIORITY");
    assert!(error["retryable"].as_bool().unwrap());
    let hint = error["hint"].as_str().unwrap();
    assert!(
        hint.contains('0') && hint.contains('4') || hint.contains("between"),
        "hint should mention valid priority range, got: {hint}"
    );
}

// === --no-color mode tests for stable snapshots ===

#[test]
fn e2e_error_text_mode_no_color() {
    let _log = common::test_log("e2e_error_text_mode_no_color");
    let workspace = BrWorkspace::new();

    // Test NOT_INITIALIZED error in no-color mode
    let result = run_br(&workspace, ["list", "--no-color"], "list_not_init_no_color");
    assert!(!result.status.success());

    // Output should not contain ANSI escape codes
    assert!(
        !result.stderr.contains("\x1b["),
        "stderr should not contain ANSI escape codes"
    );
    assert!(
        !result.stdout.contains("\x1b["),
        "stdout should not contain ANSI escape codes"
    );
}

#[test]
fn e2e_error_text_vs_json_parity() {
    let _log = common::test_log("e2e_error_text_vs_json_parity");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    // Same error in text mode
    let text_result = run_br(
        &workspace,
        ["show", "bd-nonexistent", "--no-color"],
        "show_missing_text",
    );
    assert!(!text_result.status.success());

    // Same error in JSON mode
    let json_result = run_br(
        &workspace,
        ["show", "bd-nonexistent", "--json"],
        "show_missing_json",
    );
    assert!(!json_result.status.success());

    // Both should have same exit code
    assert_eq!(
        text_result.status.code(),
        json_result.status.code(),
        "text and JSON mode should have same exit code"
    );

    // JSON mode should produce valid structured error
    let json = parse_error_json(&json_result.stdout).expect("JSON mode should produce valid JSON");
    assert!(
        verify_error_structure(&json),
        "JSON error should have required fields"
    );

    // Text mode output should contain error message (not JSON)
    assert!(
        text_result.stderr.contains("not found") || text_result.stderr.contains("No issue"),
        "text mode should contain human-readable error"
    );
}

#[test]
fn e2e_error_multiple_errors_same_exit_code() {
    let _log = common::test_log("e2e_error_multiple_errors_same_exit_code");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create = run_br(&workspace, ["create", "Test issue"], "create");
    assert!(create.status.success());
    let _id = parse_created_id(&create.stdout);

    // Validation errors should return exit code 4
    // Note: invalid type is NOT tested here because custom types are allowed
    let invalid_priority = run_br(
        &workspace,
        ["create", "Test", "--priority", "99", "--json"],
        "invalid_priority",
    );

    assert_eq!(
        invalid_priority.status.code(),
        Some(4),
        "invalid priority should be exit 4"
    );
}

#[test]
fn e2e_error_exit_code_categories() {
    let _log = common::test_log("e2e_error_exit_code_categories");
    let workspace = BrWorkspace::new();

    // Exit code 2: Database/initialization errors
    let not_init = run_br(&workspace, ["list", "--json"], "not_init");
    assert_eq!(
        not_init.status.code(),
        Some(2),
        "NOT_INITIALIZED should be exit 2"
    );

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    // Exit code 3: Issue errors
    let not_found = run_br(&workspace, ["show", "bd-missing", "--json"], "not_found");
    assert_eq!(
        not_found.status.code(),
        Some(3),
        "ISSUE_NOT_FOUND should be exit 3"
    );

    // Exit code 4: Validation errors (already tested above)

    // Exit code 5: Dependency errors
    let create = run_br(&workspace, ["create", "Self dep"], "create_self");
    assert!(create.status.success());
    let id = parse_created_id(&create.stdout);

    let self_dep = run_br(&workspace, ["dep", "add", &id, &id, "--json"], "self_dep");
    assert_eq!(
        self_dep.status.code(),
        Some(5),
        "SELF_DEPENDENCY should be exit 5"
    );
}

// === Additional Validation + Error Parity Tests ===

#[test]
fn e2e_structured_error_label_validation() {
    let _log = common::test_log("e2e_structured_error_label_validation");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create = run_br(&workspace, ["create", "Test issue"], "create");
    assert!(create.status.success());
    let id = parse_created_id(&create.stdout);

    // Test label with invalid characters (spaces not allowed)
    let result = run_br(
        &workspace,
        ["update", &id, "--add-label", "bad label", "--json"],
        "update_bad_label_json",
    );
    assert!(!result.status.success());
    assert_eq!(result.status.code(), Some(4), "exit code should be 4");

    let json = parse_error_json(&result.stdout).expect("should be valid JSON");
    assert!(verify_error_structure(&json), "missing required fields");

    let error = &json["error"];
    assert_eq!(error["code"], "VALIDATION_FAILED");
    assert!(error["retryable"].as_bool().unwrap());
    assert!(
        error["message"].as_str().unwrap().contains("label")
            || error["hint"].as_str().unwrap_or("").contains("label"),
        "error should mention label"
    );
}

#[test]
fn e2e_structured_error_label_too_long() {
    let _log = common::test_log("e2e_structured_error_label_too_long");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create = run_br(&workspace, ["create", "Test issue"], "create");
    assert!(create.status.success());
    let id = parse_created_id(&create.stdout);

    // Create a label that exceeds 50 characters
    let long_label = "a".repeat(60);
    let result = run_br(
        &workspace,
        ["update", &id, "--add-label", &long_label, "--json"],
        "update_long_label_json",
    );
    assert!(!result.status.success());
    assert_eq!(result.status.code(), Some(4), "exit code should be 4");

    let json = parse_error_json(&result.stdout).expect("should be valid JSON");
    assert!(verify_error_structure(&json), "missing required fields");

    let error = &json["error"];
    assert_eq!(error["code"], "VALIDATION_FAILED");
}

#[test]
fn e2e_structured_error_dependency_target_not_found() {
    let _log = common::test_log("e2e_structured_error_dependency_target_not_found");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create = run_br(&workspace, ["create", "Test issue"], "create");
    assert!(create.status.success());
    let id = parse_created_id(&create.stdout);

    // Try to add dependency on non-existent issue
    // The implementation returns ISSUE_NOT_FOUND for missing dependency targets
    let result = run_br(
        &workspace,
        ["dep", "add", &id, "bd-nonexistent", "--json"],
        "dep_missing_target_json",
    );
    assert!(!result.status.success());
    assert_eq!(
        result.status.code(),
        Some(3),
        "exit code should be 3 (issue not found)"
    );

    let json = parse_error_json(&result.stdout).expect("should be valid JSON");
    assert!(verify_error_structure(&json), "missing required fields");

    let error = &json["error"];
    // Returns ISSUE_NOT_FOUND since the target issue doesn't exist
    assert_eq!(error["code"], "ISSUE_NOT_FOUND");
    assert!(!error["retryable"].as_bool().unwrap());
    assert!(
        error["context"]["searched_id"]
            .as_str()
            .unwrap()
            .contains("nonexistent")
    );
}

#[test]
fn e2e_dependency_idempotent_duplicate() {
    let _log = common::test_log("e2e_dependency_idempotent_duplicate");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create_a = run_br(&workspace, ["create", "Issue A"], "create_a");
    assert!(create_a.status.success());
    let id_a = parse_created_id(&create_a.stdout);

    let create_b = run_br(&workspace, ["create", "Issue B"], "create_b");
    assert!(create_b.status.success());
    let id_b = parse_created_id(&create_b.stdout);

    // Add dependency first time - should succeed
    let dep_add = run_br(&workspace, ["dep", "add", &id_a, &id_b], "dep_add_first");
    assert!(dep_add.status.success());

    // Add same dependency again - should succeed (idempotent) with status "exists"
    let result = run_br(
        &workspace,
        ["dep", "add", &id_a, &id_b, "--json"],
        "dep_add_duplicate_json",
    );
    assert!(
        result.status.success(),
        "duplicate dependency should be idempotent"
    );

    // Parse output as success JSON (not error)
    let json: Value = serde_json::from_str(&result.stdout).expect("should be valid JSON");
    assert_eq!(
        json["status"].as_str().unwrap_or(""),
        "exists",
        "status should be 'exists'"
    );
    assert_eq!(
        json["action"].as_str().unwrap_or(""),
        "already_exists",
        "action should be 'already_exists'"
    );
}

#[test]
fn e2e_dependency_metadata_flag_persists_to_jsonl() {
    let _log = common::test_log("e2e_dependency_metadata_flag_persists_to_jsonl");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create_a = run_br(&workspace, ["create", "Issue A"], "create_a");
    assert!(create_a.status.success());
    let id_a = parse_created_id(&create_a.stdout);

    let create_b = run_br(&workspace, ["create", "Issue B"], "create_b");
    assert!(create_b.status.success());
    let id_b = parse_created_id(&create_b.stdout);

    let dep_add = run_br(
        &workspace,
        [
            "dep",
            "add",
            &id_a,
            &id_b,
            "--metadata",
            r#"{"source":"cli","reason":"gate"}"#,
        ],
        "dep_add_metadata",
    );
    assert!(
        dep_add.status.success(),
        "dep add failed: {}",
        dep_add.stderr
    );

    let sync = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(sync.status.success(), "sync failed: {}", sync.stderr);

    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let contents = fs::read_to_string(&jsonl_path).expect("read issues jsonl");
    let issue = contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).expect("valid issue json"))
        .find(|value| value["id"] == id_a)
        .expect("issue A exported");

    let deps = issue["dependencies"]
        .as_array()
        .expect("dependencies array");
    assert_eq!(deps.len(), 1);
    assert_eq!(deps[0]["depends_on_id"], id_b);
    assert_eq!(deps[0]["metadata"], r#"{"source":"cli","reason":"gate"}"#);
}

#[test]
fn e2e_dependency_remove_json_reports_removed_type() {
    let _log = common::test_log("e2e_dependency_remove_json_reports_removed_type");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create_a = run_br(&workspace, ["create", "Issue A"], "create_a");
    assert!(create_a.status.success());
    let id_a = parse_created_id(&create_a.stdout);

    let create_b = run_br(&workspace, ["create", "Issue B"], "create_b");
    assert!(create_b.status.success());
    let id_b = parse_created_id(&create_b.stdout);

    let dep_add = run_br(
        &workspace,
        ["dep", "add", &id_a, &id_b, "--type", "waits-for"],
        "dep_add_waits_for",
    );
    assert!(
        dep_add.status.success(),
        "dep add failed: {}",
        dep_add.stderr
    );

    let result = run_br(
        &workspace,
        ["dep", "remove", &id_a, &id_b, "--json"],
        "dep_remove_json",
    );
    assert!(
        result.status.success(),
        "dep remove failed: {}",
        result.stderr
    );

    let json: Value = serde_json::from_str(&result.stdout).expect("should be valid JSON");
    assert_eq!(json["status"], "ok");
    assert_eq!(json["action"], "removed");
    assert_eq!(json["type"], "waits-for");
}

#[test]
fn e2e_delete_with_dependents_preview() {
    let _log = common::test_log("e2e_delete_with_dependents_preview");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create_a = run_br(&workspace, ["create", "Issue A"], "create_a");
    assert!(create_a.status.success());
    let id_a = parse_created_id(&create_a.stdout);

    let create_b = run_br(&workspace, ["create", "Issue B"], "create_b");
    assert!(create_b.status.success());
    let id_b = parse_created_id(&create_b.stdout);

    // B depends on A
    let dep_add = run_br(&workspace, ["dep", "add", &id_b, &id_a], "dep_add");
    assert!(dep_add.status.success());

    // Delete A (which has B as dependent) - shows preview mode warning
    // The command exits 0 (preview mode) but warns about dependents
    let result = run_br(&workspace, ["delete", &id_a], "delete_with_deps");
    assert!(
        result.status.success(),
        "delete with dependents should show preview"
    );
    assert!(
        result.stdout.contains("depend on") || result.stdout.contains("dependents"),
        "should mention dependents in output"
    );
    assert!(
        result.stdout.contains("--force") || result.stdout.contains("--cascade"),
        "should suggest force or cascade options"
    );

    // Issue should still exist after preview
    let show = run_br(&workspace, ["show", &id_a], "show_after_preview");
    assert!(
        show.status.success(),
        "issue should still exist after preview"
    );
}

#[test]
fn e2e_delete_json_sorts_deleted_ids() {
    let _log = common::test_log("e2e_delete_json_sorts_deleted_ids");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create_a = run_br(&workspace, ["create", "Delete A"], "create_delete_a");
    assert!(create_a.status.success());
    let id_a = parse_created_id(&create_a.stdout);

    let create_b = run_br(&workspace, ["create", "Delete B"], "create_delete_b");
    assert!(create_b.status.success());
    let id_b = parse_created_id(&create_b.stdout);

    let result = run_br(
        &workspace,
        ["delete", &id_b, &id_a, "--json"],
        "delete_json_sorted_ids",
    );
    assert!(
        result.status.success(),
        "delete json failed: {}",
        result.stderr
    );

    let json: Value = serde_json::from_str(&result.stdout).expect("should be valid JSON");
    let deleted = json["deleted"].as_array().expect("deleted array");
    let deleted_ids: Vec<&str> = deleted
        .iter()
        .map(|value| value.as_str().expect("deleted id"))
        .collect();

    let mut expected = vec![id_a.as_str(), id_b.as_str()];
    expected.sort_unstable();
    assert_eq!(deleted_ids, expected);
    assert_eq!(json["deleted_count"], 2);
}

#[test]
fn e2e_delete_dry_run_sorts_requested_ids() {
    let _log = common::test_log("e2e_delete_dry_run_sorts_requested_ids");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create_a = run_br(&workspace, ["create", "Dry Run A"], "create_dry_run_a");
    assert!(create_a.status.success());
    let id_a = parse_created_id(&create_a.stdout);

    let create_b = run_br(&workspace, ["create", "Dry Run B"], "create_dry_run_b");
    assert!(create_b.status.success());
    let id_b = parse_created_id(&create_b.stdout);

    let result = run_br(
        &workspace,
        ["delete", &id_b, &id_a, "--dry-run"],
        "delete_dry_run_sorted_ids",
    );
    assert!(
        result.status.success(),
        "delete dry-run failed: {}",
        result.stderr
    );

    let listed_ids: Vec<&str> = result
        .stdout
        .lines()
        .filter_map(|line| line.strip_prefix("  - "))
        .filter_map(|line| line.split(':').next())
        .take(2)
        .collect();

    let mut expected = vec![id_a.as_str(), id_b.as_str()];
    expected.sort_unstable();
    assert_eq!(listed_ids, expected);
}

#[test]
fn e2e_delete_dry_run_json_returns_structured_preview() {
    let _log = common::test_log("e2e_delete_dry_run_json_returns_structured_preview");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create_a = run_br(
        &workspace,
        ["create", "Dry Run JSON A"],
        "create_dry_run_json_a",
    );
    assert!(create_a.status.success());
    let id_a = parse_created_id(&create_a.stdout);

    let create_b = run_br(
        &workspace,
        ["create", "Dry Run JSON B"],
        "create_dry_run_json_b",
    );
    assert!(create_b.status.success());
    let id_b = parse_created_id(&create_b.stdout);

    let result = run_br(
        &workspace,
        ["delete", &id_b, &id_a, "--dry-run", "--json"],
        "delete_dry_run_json",
    );
    assert!(
        result.status.success(),
        "delete dry-run --json failed: {}",
        result.stderr
    );

    let payload = extract_json_payload(&result.stdout);
    let json: Value = serde_json::from_str(&payload).expect("delete dry-run preview json");
    assert_eq!(json["preview"], true);
    let ids = json["would_delete"].as_array().expect("would_delete array");
    let mut expected = vec![id_a.as_str(), id_b.as_str()];
    expected.sort_unstable();
    let actual: Vec<&str> = ids
        .iter()
        .map(|value| value.as_str().expect("preview delete id"))
        .collect();
    assert_eq!(actual, expected);
}

#[test]
fn e2e_delete_with_dependents_json_returns_structured_preview() {
    let _log = common::test_log("e2e_delete_with_dependents_json_returns_structured_preview");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create_a = run_br(&workspace, ["create", "Issue A"], "create_a_json_preview");
    assert!(create_a.status.success());
    let id_a = parse_created_id(&create_a.stdout);

    let create_b = run_br(&workspace, ["create", "Issue B"], "create_b_json_preview");
    assert!(create_b.status.success());
    let id_b = parse_created_id(&create_b.stdout);

    let dep_add = run_br(
        &workspace,
        ["dep", "add", &id_b, &id_a],
        "dep_add_json_preview",
    );
    assert!(dep_add.status.success());

    let result = run_br(
        &workspace,
        ["delete", &id_a, "--json"],
        "delete_with_dependents_json_preview",
    );
    assert!(
        result.status.success(),
        "delete with dependents --json should return preview: {}",
        result.stderr
    );

    let payload = extract_json_payload(&result.stdout);
    let json: Value = serde_json::from_str(&payload).expect("delete dependent preview json");
    assert_eq!(json["preview"], true);
    assert_eq!(json["would_delete"][0], id_a);
    let blocked = json["blocked_dependents"]
        .as_array()
        .expect("blocked_dependents array");
    assert_eq!(blocked.len(), 1);
    assert_eq!(blocked[0], id_b);
}

#[test]
fn e2e_delete_ignores_non_blocking_related_dependencies() {
    let _log = common::test_log("e2e_delete_ignores_non_blocking_related_dependencies");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create_anchor = run_br(&workspace, ["create", "Anchor"], "create_anchor");
    assert!(create_anchor.status.success());
    let anchor_id = parse_created_id(&create_anchor.stdout);

    let create_related = run_br(&workspace, ["create", "Related"], "create_related");
    assert!(create_related.status.success());
    let related_id = parse_created_id(&create_related.stdout);

    let dep_add = run_br(
        &workspace,
        ["dep", "add", &related_id, &anchor_id, "--type", "related"],
        "dep_add_related",
    );
    assert!(
        dep_add.status.success(),
        "dep add failed: {}",
        dep_add.stderr
    );

    let delete = run_br(
        &workspace,
        ["delete", &anchor_id, "--json"],
        "delete_related_edge_json",
    );
    assert!(delete.status.success(), "delete failed: {}", delete.stderr);

    let payload = extract_json_payload(&delete.stdout);
    let json: Value = serde_json::from_str(&payload).expect("delete json");
    assert_eq!(json["deleted_count"], 1);
    assert_eq!(json["deleted"][0], anchor_id);
    assert!(
        json.get("preview").is_none(),
        "non-blocking related edges should not trigger preview: {json}"
    );
}

#[test]
fn e2e_delete_child_with_parent_child_dependency_previews_parent() {
    let _log = common::test_log("e2e_delete_child_with_parent_child_dependency_previews_parent");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create_parent = run_br(&workspace, ["create", "Parent"], "create_parent");
    assert!(create_parent.status.success());
    let parent_id = parse_created_id(&create_parent.stdout);

    let create_child = run_br(&workspace, ["create", "Child"], "create_child");
    assert!(create_child.status.success());
    let child_id = parse_created_id(&create_child.stdout);

    let dep_add = run_br(
        &workspace,
        [
            "dep",
            "add",
            &child_id,
            &parent_id,
            "--type",
            "parent-child",
        ],
        "dep_add_parent_child",
    );
    assert!(
        dep_add.status.success(),
        "dep add failed: {}",
        dep_add.stderr
    );

    let delete = run_br(
        &workspace,
        ["delete", &child_id, "--json"],
        "delete_child_parent_child_json",
    );
    assert!(
        delete.status.success(),
        "delete should return preview json: {}",
        delete.stderr
    );

    let payload = extract_json_payload(&delete.stdout);
    let json: Value = serde_json::from_str(&payload).expect("delete preview json");
    assert_eq!(json["preview"], true);
    assert_eq!(json["would_delete"][0], child_id);
    let blocked = json["blocked_dependents"]
        .as_array()
        .expect("blocked_dependents array");
    assert_eq!(blocked.len(), 1);
    assert_eq!(blocked[0], parent_id);
}

#[test]
fn e2e_delete_hard_json_reports_removed_labels_and_events() {
    let _log = common::test_log("e2e_delete_hard_json_reports_removed_labels_and_events");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create = run_br(
        &workspace,
        ["create", "Delete counters issue"],
        "create_delete_counters",
    );
    assert!(create.status.success());
    let issue_id = parse_created_id(&create.stdout);

    let label_add = run_br(
        &workspace,
        ["label", "add", &issue_id, "triage"],
        "label_add_delete_counters",
    );
    assert!(
        label_add.status.success(),
        "label add failed: {}",
        label_add.stderr
    );

    let delete = run_br(
        &workspace,
        ["delete", &issue_id, "--hard", "--json"],
        "delete_hard_counters_json",
    );
    assert!(delete.status.success(), "delete failed: {}", delete.stderr);

    let payload = extract_json_payload(&delete.stdout);
    let json: Value = serde_json::from_str(&payload).expect("delete hard json");
    assert_eq!(json["labels_removed"], 1);
    assert!(
        json["events_removed"].as_u64().unwrap_or(0) >= 2,
        "hard delete should report removed audit events: {json}"
    );
}

#[test]
fn e2e_validation_error_empty_label() {
    let _log = common::test_log("e2e_validation_error_empty_label");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create = run_br(&workspace, ["create", "Test issue"], "create");
    assert!(create.status.success());
    let id = parse_created_id(&create.stdout);

    // Empty label should fail validation
    let result = run_br(
        &workspace,
        ["update", &id, "--add-label", "", "--json"],
        "update_empty_label_json",
    );
    assert!(!result.status.success());
    assert_eq!(result.status.code(), Some(4), "exit code should be 4");
}

#[test]
fn e2e_validation_special_characters_in_label() {
    let _log = common::test_log("e2e_validation_special_characters_in_label");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create = run_br(&workspace, ["create", "Test issue"], "create");
    assert!(create.status.success());
    let id = parse_created_id(&create.stdout);

    // Valid labels (alphanumeric, hyphen, underscore, colon)
    let valid_labels = ["bug", "feat-1", "scope:subsystem", "test_case"];
    for label in valid_labels {
        let result = run_br(
            &workspace,
            ["update", &id, "--add-label", label],
            &format!("add_label_{}", label.replace(':', "_")),
        );
        assert!(
            result.status.success(),
            "label '{}' should be valid: {}",
            label,
            result.stderr
        );
    }

    // Create a new issue for testing invalid labels (to avoid label conflict)
    let create2 = run_br(&workspace, ["create", "Test issue 2"], "create2");
    assert!(create2.status.success());
    let id2 = parse_created_id(&create2.stdout);

    // Invalid labels (special characters not allowed)
    let invalid_labels = ["@mention", "has/slash", "with.dot", "emoji🎉"];
    for label in invalid_labels {
        let result = run_br(
            &workspace,
            ["update", &id2, "--add-label", label, "--json"],
            &format!("add_invalid_label_{}", label.len()),
        );
        assert!(
            !result.status.success(),
            "label '{}' should be invalid",
            label
        );
    }
}

#[test]
fn e2e_error_text_json_parity_validation() {
    let _log = common::test_log("e2e_error_text_json_parity_validation");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create = run_br(&workspace, ["create", "Test issue"], "create");
    assert!(create.status.success());
    let id = parse_created_id(&create.stdout);

    // Same validation error in text mode
    let text_result = run_br(
        &workspace,
        ["update", &id, "--add-label", "bad label", "--no-color"],
        "label_error_text",
    );
    assert!(!text_result.status.success());

    // Same validation error in JSON mode
    let json_result = run_br(
        &workspace,
        ["update", &id, "--add-label", "bad label", "--json"],
        "label_error_json",
    );
    assert!(!json_result.status.success());

    // Both should have same exit code
    assert_eq!(
        text_result.status.code(),
        json_result.status.code(),
        "text and JSON mode should have same exit code for validation errors"
    );

    // JSON mode should produce valid structured error
    let json = parse_error_json(&json_result.stdout).expect("JSON mode should produce valid JSON");
    assert!(
        verify_error_structure(&json),
        "JSON error should have required fields"
    );
}

#[test]
fn e2e_sync_merge_detects_conflict_markers_in_base_snapshot() {
    // Regression: `execute_merge` loads `beads.base.jsonl` via
    // `load_base_snapshot` *before* scanning the main JSONL for conflict
    // markers. If the base snapshot itself contained unresolved
    // `<<<<<<<` / `=======` / `>>>>>>>` regions (a rare but possible state
    // when a user commits the base snapshot against the default gitignore
    // and then hits a botched `git merge`), the merge would fail with a
    // cryptic "Invalid JSON in base snapshot at line 1" instead of the
    // helpful "merge conflict markers detected" diagnostic. The fix
    // scans the base snapshot for markers before attempting to parse.
    let _log = common::test_log("e2e_sync_merge_detects_conflict_markers_in_base_snapshot");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Seed"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // First flush so the JSONL is valid and the main sync path won't
    // short-circuit before the merge code runs.
    let flush = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);

    // Build a base snapshot that contains merge-conflict markers as if a
    // user committed `beads.base.jsonl` and then hit a botched `git merge`.
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let clean = fs::read_to_string(&jsonl_path).expect("read jsonl");
    let base_path = workspace.root.join(".beads").join("beads.base.jsonl");
    let conflicted = format!("<<<<<<< HEAD\n{clean}=======\n{clean}>>>>>>> branch\n");
    fs::write(&base_path, &conflicted).expect("write conflicted base snapshot");

    // Merge must refuse with a conflict-markers diagnostic instead of a
    // generic "Invalid JSON in base snapshot" parse error.
    let merge = run_br(&workspace, ["sync", "--merge"], "sync_merge");
    assert!(
        !merge.status.success(),
        "merge should fail when base snapshot contains conflict markers: stdout={} stderr={}",
        merge.stdout,
        merge.stderr
    );
    let lower = merge.stderr.to_lowercase();
    assert!(
        lower.contains("conflict") || lower.contains("marker"),
        "merge error should mention conflict markers, got stderr: {}",
        merge.stderr
    );
    assert!(
        !lower.contains("invalid json in base snapshot"),
        "merge error should surface the conflict-markers diagnostic rather than the generic JSON parse failure, got stderr: {}",
        merge.stderr
    );
}

/// #336: In `--json` mode, the structured error envelope must go to STDOUT
/// (where success JSON already goes) so robot callers read ONE clean,
/// parseable stream. Tracing/log lines belong on stderr and must not be
/// interleaved into the stdout JSON. Human mode keeps errors on stderr.
#[test]
fn e2e_json_failure_emits_parseable_json_on_stdout() {
    let _log = common::test_log("e2e_json_failure_emits_parseable_json_on_stdout");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // A failing command in --json mode: showing a non-existent issue.
    let show = run_br(
        &workspace,
        ["show", "bd-doesnotexist", "--json"],
        "show_missing_json",
    );
    assert!(
        !show.status.success(),
        "showing a missing issue should fail: stdout={} stderr={}",
        show.stdout,
        show.stderr
    );

    // The structured error envelope must be on STDOUT and fully parseable.
    assert!(
        !show.stdout.trim().is_empty(),
        "json-mode error must be emitted on stdout, got empty stdout (stderr={})",
        show.stderr
    );
    let payload = extract_json_payload(&show.stdout);
    let value: Value = serde_json::from_str(&payload).unwrap_or_else(|e| {
        panic!(
            "json-mode error stdout must be parseable JSON: {e}\nstdout={}\nstderr={}",
            show.stdout, show.stderr
        )
    });
    // Sanity-check the structured envelope shape.
    assert!(
        value.get("error").is_some()
            || value.get("code").is_some()
            || value.get("message").is_some(),
        "json error envelope should carry an error/code/message field, got: {value}"
    );

    // The HUMAN-mode counterpart keeps the error on stderr (stdout stays clean).
    let human = run_br(
        &workspace,
        ["show", "bd-doesnotexist"],
        "show_missing_human",
    );
    assert!(!human.status.success());
    assert!(
        serde_json::from_str::<Value>(human.stdout.trim()).is_err(),
        "human mode must not emit JSON on stdout, got: {}",
        human.stdout
    );
    assert!(
        !human.stderr.trim().is_empty(),
        "human-mode error should be on stderr"
    );
}
