//! Reproduction of 3-way merge data loss bug.
//!
//! This test demonstrates that changes to labels, dependencies, or comments
//! must participate in merge-change detection even when scalar issue content
//! is unchanged.
//! It also covers GitHub #541: a local export is not proof that an incoming
//! JSONL contains that export, especially after switching branches.

mod common;

use beads_rust::storage::SqliteStorage;
use common::cli::{BrRun, BrWorkspace, run_br};
use serde_json::{Value, json};
use std::fs;
use std::path::Path;

fn create_issue_id(workspace: &BrWorkspace, title: &str, label: &str) -> String {
    let create = run_br(workspace, ["--json", "create", title, "-t", "task"], label);
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let created_issue: serde_json::Value =
        serde_json::from_str(&create.stdout).expect("parse create json");
    created_issue["id"]
        .as_str()
        .expect("create json should include issue id")
        .to_string()
}

#[test]
fn repro_3way_merge_data_loss() {
    let workspace = BrWorkspace::new();

    // 1. Initialize beads
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed");

    // 2. Create an issue
    let issue_id = create_issue_id(&workspace, "Test issue", "create");

    // 3. Sync to JSONL (creates base snapshot)
    let sync1 = run_br(&workspace, ["sync", "--flush-only"], "sync1");
    assert!(sync1.status.success(), "sync1 failed");
    let beads_dir = workspace.root.join(".beads");
    let jsonl_path = beads_dir.join("issues.jsonl");
    let base_snapshot_path = beads_dir.join("beads.base.jsonl");
    let base_jsonl = fs::read_to_string(&jsonl_path).expect("read base jsonl");
    fs::write(&base_snapshot_path, &base_jsonl).expect("seed base snapshot");

    // 4. Modify labels LOCALLY (Left side of merge), keeping JSONL at the base
    // snapshot so this is a true local-only relation change.
    let label_local = run_br(
        &workspace,
        ["--no-auto-flush", "label", "add", &issue_id, "local-tag"],
        "label_local",
    );
    assert!(
        label_local.status.success(),
        "label_local failed: {}",
        label_local.stderr
    );

    // 5. Modify description EXTERNALLY (Right side of merge)
    // We simulate this by directly editing the JSONL.
    let jsonl_content = fs::read_to_string(&jsonl_path).expect("read jsonl");
    let mut issue: serde_json::Value =
        serde_json::from_str(jsonl_content.trim()).expect("parse jsonl");

    // Change description - this WILL change the content_hash
    issue["description"] = serde_json::Value::String("External description".to_string());
    issue["updated_at"] = serde_json::Value::String("2999-01-01T00:00:00Z".to_string());

    let modified_jsonl = serde_json::to_string(&issue).expect("serialize modified issue");
    fs::write(&jsonl_path, format!("{}\n", modified_jsonl)).expect("write modified jsonl");

    // 6. Run 3-way merge
    // At this point:
    // Base: labels=[], desc=""
    // Left (DB): labels=["local-tag"], desc=""
    // Right (JSONL): labels=[], desc="External description"

    let merge = run_br(&workspace, ["sync", "--merge"], "merge");
    assert!(
        !merge.status.success(),
        "manual merge should stop instead of choosing a lossy side: stdout={} stderr={}",
        merge.stdout,
        merge.stderr
    );
    assert!(
        merge.stderr.contains("BothModified"),
        "manual merge should report a both-modified conflict: {}",
        merge.stderr
    );

    // 7. Verify the failed merge preserved both sides for explicit operator
    // resolution: DB still has the local tag, JSONL still has the external
    // description.
    let show = run_br(
        &workspace,
        ["--allow-stale", "show", &issue_id, "--json"],
        "show_after_conflict",
    );
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let final_issue_list: serde_json::Value =
        serde_json::from_str(&show.stdout).expect("parse final issue list");
    let final_issue = &final_issue_list[0];

    let labels = final_issue["labels"]
        .as_array()
        .expect("labels should be array");
    let has_local_tag = labels.iter().any(|v| v.as_str() == Some("local-tag"));

    assert!(
        has_local_tag,
        "DATA LOSS: Local tag 'local-tag' was lost during failed manual merge!\n\
         Final labels: {:?}",
        labels
    );

    let preserved_jsonl = fs::read_to_string(&jsonl_path).expect("read preserved jsonl");
    assert!(
        preserved_jsonl.contains("External description"),
        "DATA LOSS: External description was lost during failed manual merge!\n\
         Final JSONL: {preserved_jsonl}"
    );
}

#[test]
fn repro_merge_base_snapshot_matches_finalized_export_with_notes() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_base_finalized");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let issue_id = create_issue_id(
        &workspace,
        "Base snapshot finality",
        "create_base_finalized",
    );

    let flush = run_br(&workspace, ["sync", "--flush-only"], "flush_base_finalized");
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);

    let beads_dir = workspace.root.join(".beads");
    let jsonl_path = beads_dir.join("issues.jsonl");
    let base_snapshot_path = beads_dir.join("beads.base.jsonl");
    let original_jsonl = fs::read_to_string(&jsonl_path).expect("read original jsonl");
    fs::write(&base_snapshot_path, &original_jsonl).expect("seed base snapshot");

    let local_update = run_br(
        &workspace,
        [
            "update",
            &issue_id,
            "--description",
            "Local description",
            "--no-auto-flush",
        ],
        "local_update_base_finalized",
    );
    assert!(
        local_update.status.success(),
        "local update failed: {}",
        local_update.stderr
    );

    let mut external_issue: serde_json::Value =
        serde_json::from_str(original_jsonl.trim()).expect("parse original issue");
    external_issue["description"] = serde_json::Value::String("External description".to_string());
    external_issue["updated_at"] = serde_json::Value::String("2999-01-01T00:00:00Z".to_string());
    fs::write(
        &jsonl_path,
        format!("{}\n", serde_json::to_string(&external_issue).unwrap()),
    )
    .expect("write external jsonl");

    let merge = run_br(
        &workspace,
        ["sync", "--merge", "--force"],
        "merge_base_finalized",
    );
    assert!(merge.status.success(), "merge failed: {}", merge.stderr);

    let merged_jsonl = fs::read_to_string(&jsonl_path).expect("read merged jsonl");
    let base_jsonl = fs::read_to_string(&base_snapshot_path).expect("read base snapshot");
    assert_eq!(
        base_jsonl, merged_jsonl,
        "base snapshot should be copied from the finalized exported JSONL"
    );

    let base_issue: serde_json::Value =
        serde_json::from_str(base_jsonl.trim()).expect("parse base issue");
    let comments = base_issue["comments"]
        .as_array()
        .expect("base snapshot comments should be an array");
    assert!(
        comments.iter().any(|comment| {
            comment["author"].as_str() == Some("br-sync")
                && comment["text"]
                    .as_str()
                    .is_some_and(|body| body.contains("Both modified"))
        }),
        "base snapshot should include merge note comment from finalized export: {base_issue}"
    );
}

#[test]
fn repro_merge_tolerates_base_only_deleted_issue_absent_from_db() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let issue_id = create_issue_id(&workspace, "Current issue", "create");

    let flush = run_br(&workspace, ["sync", "--flush-only"], "flush");
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);

    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let issue_json = fs::read_to_string(&jsonl_path).expect("read issues jsonl");
    let mut phantom_issue: serde_json::Value =
        serde_json::from_str(issue_json.trim()).expect("parse current issue json");
    phantom_issue["id"] = serde_json::Value::String("second-9u2".to_string());
    phantom_issue["external_ref"] = serde_json::Value::Null;

    let base_snapshot_path = workspace.root.join(".beads").join("beads.base.jsonl");
    fs::write(
        &base_snapshot_path,
        format!(
            "{}\n",
            serde_json::to_string(&phantom_issue).expect("serialize phantom issue")
        ),
    )
    .expect("write base snapshot");

    let merge = run_br(&workspace, ["sync", "--merge"], "merge");
    assert!(merge.status.success(), "merge failed: {}", merge.stderr);

    let show = run_br(&workspace, ["show", &issue_id, "--json"], "show");
    assert!(show.status.success(), "show failed: {}", show.stderr);

    let final_issue_list: serde_json::Value =
        serde_json::from_str(&show.stdout).expect("parse final issue list");
    let final_issue = &final_issue_list[0];
    assert_eq!(final_issue["id"].as_str(), Some(issue_id.as_str()));

    let merged_jsonl = fs::read_to_string(&jsonl_path).expect("read merged jsonl");
    assert!(
        !merged_jsonl.contains("second-9u2"),
        "phantom base-only issue should not survive merge"
    );
}

fn success(run: BrRun) -> BrRun {
    assert!(
        run.status.success(),
        "command failed: stdout={} stderr={}",
        run.stdout,
        run.stderr
    );
    run
}

fn fixed_issue(id: &str, title: &str, status: &str, updated_at: &str) -> Value {
    let mut issue = json!({
        "id": id,
        "title": title,
        "status": status,
        "priority": 2,
        "issue_type": "task",
        "created_at": "2000-01-01T00:00:00Z",
        "updated_at": updated_at,
    });
    if status == "closed" {
        issue["closed_at"] = json!(updated_at);
        issue["close_reason"] = json!("Previously completed");
    } else if status == "tombstone" {
        issue["deleted_at"] = json!(updated_at);
        issue["deleted_by"] = json!("fixture");
        issue["delete_reason"] = json!("Explicit deletion");
        issue["original_type"] = json!("task");
    }
    issue
}

fn write_issue_rows(path: &Path, issues: &[Value]) {
    let mut jsonl = issues
        .iter()
        .map(|issue| serde_json::to_string(issue).expect("serialize issue"))
        .collect::<Vec<_>>()
        .join("\n");
    jsonl.push('\n');
    fs::write(path, jsonl).expect("write JSONL fixture");
}

fn read_issue_rows(path: &Path) -> Vec<Value> {
    fs::read_to_string(path)
        .expect("read JSONL fixture")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("parse JSONL issue"))
        .collect()
}

fn workspace_with_exported_issues(issues: &[Value]) -> BrWorkspace {
    let workspace = BrWorkspace::new();
    success(run_br(&workspace, ["init", "--prefix", "rp"], "init"));
    write_issue_rows(&workspace.root.join(".beads/issues.jsonl"), issues);
    success(run_br(
        &workspace,
        ["sync", "--import-only"],
        "import_fixed_issues",
    ));
    // Publish the database's persisted defaults so the base represents the
    // local export used by these merge fixtures, even after a clean import.
    success(run_br(
        &workspace,
        ["sync", "--flush-only", "--force"],
        "export_fixed_issues",
    ));
    assert_eq!(
        fs::read(workspace.root.join(".beads/beads.base.jsonl")).expect("read exported base"),
        fs::read(workspace.root.join(".beads/issues.jsonl")).expect("read exported issues"),
        "merge fixture must start with the published local export as its base"
    );
    workspace
}

fn show_stored_issue(workspace: &BrWorkspace, id: &str) -> Value {
    let show = success(run_br(
        workspace,
        ["--no-auto-import", "--allow-stale", "show", id, "--json"],
        &format!("show_{id}"),
    ));
    let issues: Value = serde_json::from_str(&show.stdout).expect("parse show JSON");
    issues[0].clone()
}

fn stored_merge_state(workspace: &BrWorkspace) -> (Vec<u8>, Value) {
    let storage = SqliteStorage::open(&workspace.root.join(".beads/beads.db"))
        .expect("open merge fixture database");
    let mut issues = Vec::new();
    beads_rust::sync::export_to_writer(&storage, &mut issues).expect("snapshot all stored issues");
    let events = serde_json::to_value(storage.get_all_events(1000).expect("read fixture events"))
        .expect("serialize fixture events");
    (issues, events)
}

fn assert_merge_conflict_preserves_inputs(workspace: &BrWorkspace, expected_conflict: &str) {
    let jsonl_path = workspace.root.join(".beads/issues.jsonl");
    let base_path = workspace.root.join(".beads/beads.base.jsonl");
    let jsonl_before = fs::read(&jsonl_path).expect("read external generation before merge");
    let base_before = fs::read(&base_path).expect("read local export before merge");
    let database_before = stored_merge_state(workspace);

    let merge = run_br(workspace, ["sync", "--merge", "--json"], "ambiguous_merge");
    assert!(
        !merge.status.success(),
        "a conflicted merge must not silently choose a version: {merge:?}"
    );
    assert!(
        format!("{}{}", merge.stdout, merge.stderr).contains(expected_conflict),
        "expected {expected_conflict}, distinguishing ambiguous ancestry from concurrent edits: {merge:?}"
    );
    assert_eq!(
        stored_merge_state(workspace),
        database_before,
        "refused merge changed stored issues, relations, or audit events"
    );
    assert_eq!(fs::read(&jsonl_path).unwrap(), jsonl_before);
    assert_eq!(fs::read(&base_path).unwrap(), base_before);
}

fn switched_branch_workspace() -> (BrWorkspace, String) {
    let workspace = workspace_with_exported_issues(&[
        fixed_issue(
            "rp-close",
            "Close on branch",
            "open",
            "2020-01-01T00:00:00Z",
        ),
        fixed_issue(
            "rp-reopen",
            "Reopen on branch",
            "closed",
            "2020-01-01T00:00:00Z",
        ),
        fixed_issue("rp-main", "Teammate issue", "open", "2020-01-01T00:00:00Z"),
    ]);
    let jsonl_path = workspace.root.join(".beads/issues.jsonl");
    let mut incoming_main = read_issue_rows(&jsonl_path);

    // Real CLI transitions and export on a feature branch. The initial
    // timestamps are fixed, so this needs no sleep or filesystem-mtime race.
    success(run_br(
        &workspace,
        [
            "--no-auto-flush",
            "close",
            "rp-close",
            "--reason",
            "Completed on branch",
        ],
        "close_on_branch",
    ));
    success(run_br(
        &workspace,
        ["--no-auto-flush", "reopen", "rp-reopen"],
        "reopen_on_branch",
    ));
    let create = success(run_br(
        &workspace,
        ["--no-auto-flush", "create", "Created on branch", "--json"],
        "create_on_branch",
    ));
    let created: Value = serde_json::from_str(&create.stdout).expect("created issue JSON");
    let created_id = created["id"].as_str().expect("created id").to_string();
    success(run_br(
        &workspace,
        ["sync", "--flush-only"],
        "export_branch",
    ));

    // A checkout replaces only the tracked JSONL. Main never saw the branch
    // export, but it contains a teammate's genuinely newer, unrelated edit.
    let teammate = incoming_main
        .iter_mut()
        .find(|issue| issue["id"] == "rp-main")
        .unwrap();
    teammate["description"] = json!("Teammate update on main");
    teammate["updated_at"] = json!("2999-01-01T00:00:00Z");
    write_issue_rows(&jsonl_path, &incoming_main);
    (workspace, created_id)
}

#[test]
fn repro_branch_switch_merge_preserves_exported_close_reopen_and_creation() {
    let (workspace, created_id) = switched_branch_workspace();
    let jsonl_path = workspace.root.join(".beads/issues.jsonl");
    assert_merge_conflict_preserves_inputs(&workspace, "AmbiguousBase");
    assert_eq!(
        show_stored_issue(&workspace, "rp-close")["status"],
        "closed"
    );
    assert_eq!(show_stored_issue(&workspace, "rp-reopen")["status"], "open");
    assert_eq!(show_stored_issue(&workspace, &created_id)["status"], "open");

    // The operator explicitly accepts timestamp ordering. Only the
    // ambiguous rows use that policy; main's independent update still lands.
    let merge = success(run_br(
        &workspace,
        ["sync", "--merge", "--force", "--json"],
        "merge_with_explicit_timestamp_policy",
    ));
    let report: Value = serde_json::from_str(&merge.stdout).expect("merge report");
    assert_eq!(report["deleted_issues"], 0);
    assert_eq!(report["conflicts"], 0);
    assert_eq!(
        show_stored_issue(&workspace, "rp-close")["status"],
        "closed"
    );
    let reopened = show_stored_issue(&workspace, "rp-reopen");
    assert_eq!(reopened["status"], "open");
    assert!(reopened["closed_at"].is_null());
    assert_eq!(show_stored_issue(&workspace, &created_id)["status"], "open");
    assert_eq!(
        show_stored_issue(&workspace, "rp-main")["description"],
        "Teammate update on main"
    );

    let published = fs::read(&jsonl_path).expect("read resolved export");
    assert_eq!(
        fs::read(workspace.root.join(".beads/beads.base.jsonl")).unwrap(),
        published
    );
    success(run_br(&workspace, ["sync", "--merge"], "merge_again"));
    assert_eq!(
        fs::read(&jsonl_path).unwrap(),
        published,
        "resolved merge must be idempotent"
    );
}

#[test]
fn repro_equal_and_slow_external_clocks_require_an_explicit_merge_winner() {
    for external_time in ["2020-01-01T00:00:00Z", "2019-01-01T00:00:00Z"] {
        let workspace = workspace_with_exported_issues(&[fixed_issue(
            "rp-clock",
            "Locally exported title",
            "open",
            "2020-01-01T00:00:00Z",
        )]);
        let jsonl_path = workspace.root.join(".beads/issues.jsonl");
        let mut external = read_issue_rows(&jsonl_path);
        external[0]["title"] = json!("Genuine edit from another machine");
        external[0]["updated_at"] = json!(external_time);
        write_issue_rows(&jsonl_path, &external);

        // Equal or older clocks prove neither ancestry nor staleness. Keep
        // all evidence until the caller selects a side explicitly.
        assert_merge_conflict_preserves_inputs(&workspace, "AmbiguousBase");
        success(run_br(
            &workspace,
            ["sync", "--merge", "--force-db"],
            "explicit_local_winner",
        ));
        assert_eq!(
            show_stored_issue(&workspace, "rp-clock")["title"],
            "Locally exported title"
        );

        write_issue_rows(&jsonl_path, &external);
        success(run_br(
            &workspace,
            ["sync", "--merge", "--force-jsonl"],
            "explicit_external_winner",
        ));
        assert_eq!(
            show_stored_issue(&workspace, "rp-clock")["title"],
            "Genuine edit from another machine",
            "a slow clock must not defeat --force-jsonl"
        );
    }
}

#[test]
fn repro_missing_live_issues_survive_even_when_tombstones_can_expire() {
    for retention in [None, Some(0_u64), Some(30)] {
        let workspace = workspace_with_exported_issues(&[
            fixed_issue(
                "rp-created",
                "Exported branch creation",
                "open",
                "2020-01-01T00:00:00Z",
            ),
            fixed_issue(
                "rp-edited",
                "Locally edited after export",
                "open",
                "2020-01-01T00:00:00Z",
            ),
            fixed_issue("rp-shared", "Shared issue", "open", "2020-01-01T00:00:00Z"),
            fixed_issue(
                "rp-deleted",
                "Known deletion",
                "tombstone",
                "2020-01-01T00:00:00Z",
            ),
        ]);
        if let Some(days) = retention {
            let metadata_path = workspace.root.join(".beads/metadata.json");
            let mut metadata: Value =
                serde_json::from_slice(&fs::read(&metadata_path).unwrap()).unwrap();
            metadata["deletions_retention_days"] = json!(days);
            fs::write(metadata_path, serde_json::to_vec(&metadata).unwrap()).unwrap();
        }
        success(run_br(
            &workspace,
            [
                "--no-auto-flush",
                "update",
                "rp-edited",
                "--description",
                "Unexported local work",
            ],
            "edit_after_export",
        ));
        let jsonl_path = workspace.root.join(".beads/issues.jsonl");
        let shared = read_issue_rows(&jsonl_path)
            .into_iter()
            .find(|issue| issue["id"] == "rp-shared")
            .unwrap();
        write_issue_rows(&jsonl_path, &[shared]);

        let merge = success(run_br(
            &workspace,
            ["sync", "--merge", "--json"],
            "merge_unseen_issues",
        ));
        let report: Value = serde_json::from_str(&merge.stdout).expect("merge report");
        assert_eq!(report["deleted_issues"], 0, "retention={retention:?}");
        assert_eq!(
            show_stored_issue(&workspace, "rp-created")["status"],
            "open"
        );
        assert_eq!(
            show_stored_issue(&workspace, "rp-edited")["description"],
            "Unexported local work"
        );
        assert_eq!(
            show_stored_issue(&workspace, "rp-deleted")["status"],
            "tombstone"
        );
        let exported = read_issue_rows(&jsonl_path);
        assert!(exported.iter().any(|issue| issue["id"] == "rp-created"));
        assert!(exported.iter().any(|issue| issue["id"] == "rp-edited"));
        assert_eq!(
            exported.iter().any(|issue| issue["id"] == "rp-deleted"),
            matches!(retention, None | Some(0)),
            "retention only omits known expired tombstones"
        );
    }
}

#[test]
fn repro_newer_external_reopen_and_explicit_deletion_are_merged() {
    let workspace = workspace_with_exported_issues(&[
        fixed_issue(
            "rp-reopen",
            "External reopen",
            "closed",
            "2020-01-01T00:00:00Z",
        ),
        fixed_issue(
            "rp-delete",
            "External deletion",
            "open",
            "2020-01-01T00:00:00Z",
        ),
        fixed_issue(
            "rp-protected",
            "Local tombstone",
            "tombstone",
            "2020-01-01T00:00:00Z",
        ),
    ]);
    let jsonl_path = workspace.root.join(".beads/issues.jsonl");
    let external = [
        fixed_issue(
            "rp-reopen",
            "External reopen",
            "open",
            "2021-01-01T00:00:00Z",
        ),
        fixed_issue(
            "rp-delete",
            "External deletion",
            "tombstone",
            "2021-01-01T00:00:00Z",
        ),
        fixed_issue(
            "rp-protected",
            "Attempted resurrection",
            "open",
            "2021-01-01T00:00:00Z",
        ),
    ];
    for flags in [
        vec!["sync", "--merge"],
        vec!["sync", "--merge", "--force-jsonl"],
    ] {
        write_issue_rows(&jsonl_path, &external);
        success(run_br(&workspace, flags, "merge_explicit_transitions"));
        assert_eq!(show_stored_issue(&workspace, "rp-reopen")["status"], "open");
        let deleted = show_stored_issue(&workspace, "rp-delete");
        assert_eq!(deleted["status"], "tombstone");
        assert_eq!(deleted["delete_reason"], "Explicit deletion");
        assert_eq!(
            show_stored_issue(&workspace, "rp-protected")["status"],
            "tombstone"
        );
    }
}

#[test]
fn repro_force_jsonl_can_explicitly_delete_an_absent_exported_issue() {
    let workspace = workspace_with_exported_issues(&[
        fixed_issue(
            "rp-victim",
            "Explicitly discarded issue",
            "open",
            "2020-01-01T00:00:00Z",
        ),
        fixed_issue("rp-shared", "Shared issue", "open", "2020-01-01T00:00:00Z"),
    ]);
    let jsonl_path = workspace.root.join(".beads/issues.jsonl");
    let shared = read_issue_rows(&jsonl_path)
        .into_iter()
        .find(|issue| issue["id"] == "rp-shared")
        .unwrap();
    write_issue_rows(&jsonl_path, &[shared]);

    let merge = success(run_br(
        &workspace,
        ["sync", "--merge", "--force-jsonl", "--json"],
        "explicit_missing_deletion",
    ));
    let report: Value = serde_json::from_str(&merge.stdout).expect("merge report");
    assert_eq!(report["deleted_issues"], 1);
    assert_eq!(
        show_stored_issue(&workspace, "rp-victim")["status"],
        "tombstone"
    );
    assert_eq!(show_stored_issue(&workspace, "rp-shared")["status"], "open");
}

#[test]
fn repro_explicit_external_tombstone_resolves_a_conflicting_local_edit() {
    let workspace = workspace_with_exported_issues(&[fixed_issue(
        "rp-conflict",
        "Conflicting deletion",
        "open",
        "2020-01-01T00:00:00Z",
    )]);
    success(run_br(
        &workspace,
        [
            "--no-auto-flush",
            "update",
            "rp-conflict",
            "--description",
            "Local edit before external deletion",
        ],
        "local_conflicting_edit",
    ));
    let jsonl_path = workspace.root.join(".beads/issues.jsonl");
    write_issue_rows(
        &jsonl_path,
        &[fixed_issue(
            "rp-conflict",
            "Conflicting deletion",
            "tombstone",
            "2999-01-01T00:00:00Z",
        )],
    );

    assert_merge_conflict_preserves_inputs(&workspace, "BothModified");
    success(run_br(
        &workspace,
        ["sync", "--merge", "--force-jsonl"],
        "accept_conflicting_external_deletion",
    ));
    let deleted = show_stored_issue(&workspace, "rp-conflict");
    assert_eq!(deleted["status"], "tombstone");
    assert_eq!(deleted["deleted_at"], "2999-01-01T00:00:00Z");
    assert_eq!(deleted["deleted_by"], "fixture");
    assert_eq!(deleted["delete_reason"], "Explicit deletion");
    assert!(deleted["comments"].as_array().is_none_or(Vec::is_empty));
    let published = read_issue_rows(&jsonl_path);
    assert_eq!(published.len(), 1);
    assert_eq!(published[0]["status"], "tombstone");
    assert_eq!(published[0]["deleted_at"], "2999-01-01T00:00:00Z");
}

#[test]
fn repro_merge_note_cannot_regress_a_preserved_revision_timestamp() {
    let workspace = workspace_with_exported_issues(&[
        fixed_issue(
            "rp-future",
            "Export from a faster local clock",
            "open",
            "2999-01-01T00:00:00Z",
        ),
        fixed_issue("rp-shared", "Shared issue", "open", "2020-01-01T00:00:00Z"),
    ]);
    let jsonl_path = workspace.root.join(".beads/issues.jsonl");
    let mut external = read_issue_rows(&jsonl_path);
    let older = external
        .iter_mut()
        .find(|issue| issue["id"] == "rp-future")
        .unwrap();
    older["title"] = json!("Older external copy");
    older["updated_at"] = json!("2998-01-01T00:00:00Z");
    write_issue_rows(&jsonl_path, &external);

    assert_merge_conflict_preserves_inputs(&workspace, "AmbiguousBase");
    success(run_br(
        &workspace,
        ["sync", "--merge", "--force-db"],
        "preserve_fast_clock_revision",
    ));
    let preserved = show_stored_issue(&workspace, "rp-future");
    assert_eq!(preserved["title"], "Export from a faster local clock");
    assert_eq!(preserved["updated_at"], "2999-01-01T00:00:00Z");

    // A merge note's real creation time must not make a previously rejected
    // external revision appear newer on the next merge.
    write_issue_rows(&jsonl_path, &external);
    assert_merge_conflict_preserves_inputs(&workspace, "AmbiguousBase");

    // An unseen live row is kept exactly, with no note or audit mutation.
    let shared = external
        .into_iter()
        .find(|issue| issue["id"] == "rp-shared")
        .unwrap();
    write_issue_rows(&jsonl_path, &[shared]);
    let stored_before = stored_merge_state(&workspace);
    success(run_br(
        &workspace,
        ["sync", "--merge"],
        "preserve_unseen_fast_clock_revision",
    ));
    assert_eq!(stored_merge_state(&workspace), stored_before);
    assert_eq!(show_stored_issue(&workspace, "rp-future"), preserved);
}
