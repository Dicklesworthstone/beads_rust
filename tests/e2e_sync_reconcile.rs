//! End-to-end tests for `br sync --reconcile` (additive JSONL reconciliation).
//!
//! Covers the beads_rust-3r45 acceptance bar: the false-equal cached-hash
//! state, the CASS-shaped recovery fixture (183 creates / 5 updates / all
//! events preserved), timestamp classification, tombstone protection,
//! relation preservation, orphan handling, malformed input, dry-run
//! zero-mutation, plan/apply witness rollback, lock contention, external
//! path policy, empty inputs, and a 2K+ issue bulk run.
//!
//! Reconcile must NEVER: delete issues, write events, write JSONL or base
//! snapshots, or reset tables. Several tests assert this byte-for-byte.

#![allow(
    clippy::too_many_lines,
    clippy::items_after_statements,
    clippy::format_push_string
)]

mod common;

use common::cli::{BrWorkspace, parse_json_value, run_br, run_br_with_env};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use beads_rust::model::{Issue, Status};
use beads_rust::storage::SqliteStorage;
use beads_rust::sync::{
    ImportConfig, METADATA_JSONL_CONTENT_HASH, METADATA_JSONL_MTIME, METADATA_JSONL_SIZE,
    METADATA_LAST_IMPORT_TIME, apply_sync_reconcile, compute_jsonl_hash, plan_sync_reconcile,
};

// ============================================================================
// Helpers
// ============================================================================

fn beads_dir(ws: &BrWorkspace) -> PathBuf {
    ws.root.join(".beads")
}

fn jsonl_path(ws: &BrWorkspace) -> PathBuf {
    beads_dir(ws).join("issues.jsonl")
}

fn db_path(ws: &BrWorkspace) -> PathBuf {
    beads_dir(ws).join("beads.db")
}

fn init_workspace(ws: &BrWorkspace, label: &str) {
    let run = run_br(ws, ["init"], &format!("{label}_init"));
    assert!(run.status.success(), "init failed: {}", run.stderr);
}

fn create_issue(ws: &BrWorkspace, title: &str, label: &str) -> String {
    let run = run_br(
        ws,
        [
            "create",
            title,
            "--type",
            "task",
            "--priority",
            "2",
            "--json",
        ],
        label,
    );
    assert!(run.status.success(), "create failed: {}", run.stderr);
    let json = parse_json_value(&run.stdout);
    json.get("id")
        .or_else(|| json.get(0).and_then(|v| v.get("id")))
        .and_then(Value::as_str)
        .expect("created issue id")
        .to_string()
}

/// Hash every file under `dir` recursively → map of rel-path to SHA-256.
fn hash_files_under(dir: &Path) -> BTreeMap<String, String> {
    fn visit(dir: &Path, base: &Path, map: &mut BTreeMap<String, String>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let rel = path
                    .strip_prefix(base)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();
                if path.is_file() {
                    if let Ok(mut contents) = fs::read(&path) {
                        // A WAL reader registers its snapshot in the -shm
                        // read-mark array (bytes 100..120) as part of the
                        // read-lock protocol, so a read-only open cannot
                        // leave those 20 bytes alone (GitHub #476,
                        // `storage::sqlite::SHM_READ_MARK_RANGE`). Every
                        // other byte of every file is under the contract.
                        if rel.ends_with("-shm") && contents.len() >= 120 {
                            contents[100..120].fill(0);
                        }
                        let mut digest = Sha256::new();
                        digest.update(&contents);
                        map.insert(rel, beads_rust::util::hex_encode(&digest.finalize()));
                    }
                } else if path.is_dir() {
                    visit(&path, base, map);
                }
            }
        }
    }
    let mut map = BTreeMap::new();
    if dir.exists() {
        visit(dir, dir, &mut map);
    }
    map
}

/// Stat witness (mtime nanos + len) for every file under `dir`.
fn stat_files_under(dir: &Path) -> BTreeMap<String, (u128, u64)> {
    fn visit(dir: &Path, base: &Path, map: &mut BTreeMap<String, (u128, u64)>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let rel = path
                    .strip_prefix(base)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();
                if path.is_file() {
                    if let Ok(meta) = fs::metadata(&path) {
                        // The -shm read-mark write of a read-only open
                        // (GitHub #476) refreshes that file's mtime; its
                        // size and every other byte stay under the contract.
                        let mtime = if rel.ends_with("-shm") {
                            0
                        } else {
                            meta.modified()
                                .ok()
                                .and_then(|t| {
                                    t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok()
                                })
                                .map_or(0, |d| d.as_nanos())
                        };
                        map.insert(rel, (mtime, meta.len()));
                    }
                } else if path.is_dir() {
                    visit(&path, base, map);
                }
            }
        }
    }
    let mut map = BTreeMap::new();
    if dir.exists() {
        visit(dir, dir, &mut map);
    }
    map
}

/// Simulate the false-equal generator: record the CURRENT file's content hash
/// and stat witness as the stored sync metadata, exactly as
/// `finalize_incremental_auto_flush` does after replacing dirty lines in a
/// JSONL that contains rows the DB never imported.
fn plant_false_equal_metadata(ws: &BrWorkspace) {
    let jsonl = jsonl_path(ws);
    let hash = compute_jsonl_hash(&jsonl).expect("hash jsonl");
    let meta = fs::metadata(&jsonl).expect("stat jsonl");
    let mtime = chrono::DateTime::<chrono::Utc>::from(meta.modified().expect("mtime")).to_rfc3339();
    let mut storage = SqliteStorage::open(&db_path(ws)).expect("open storage");
    storage
        .set_metadata(METADATA_JSONL_CONTENT_HASH, &hash)
        .expect("set hash");
    storage
        .set_metadata(METADATA_JSONL_MTIME, &mtime)
        .expect("set mtime");
    storage
        .set_metadata(METADATA_JSONL_SIZE, &meta.len().to_string())
        .expect("set size");
    storage
        .set_metadata(METADATA_LAST_IMPORT_TIME, &chrono::Utc::now().to_rfc3339())
        .expect("set import time");
}

fn read_jsonl_lines(ws: &BrWorkspace) -> Vec<String> {
    fs::read_to_string(jsonl_path(ws))
        .expect("read jsonl")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(String::from)
        .collect()
}

fn write_jsonl_lines(ws: &BrWorkspace, lines: &[String]) {
    let mut body = lines.join("\n");
    body.push('\n');
    fs::write(jsonl_path(ws), body).expect("write jsonl");
}

/// Clone a JSONL row into a new synthetic row with a unique id/title.
fn clone_row(template: &str, id: &str, title: &str, created_at: &str, updated_at: &str) -> String {
    let mut row: Value = serde_json::from_str(template).expect("template row");
    row["id"] = json!(id);
    row["title"] = json!(title);
    row["created_at"] = json!(created_at);
    row["updated_at"] = json!(updated_at);
    // Stale content_hash is fine: import normalization recomputes it.
    serde_json::to_string(&row).expect("serialize row")
}

fn set_row_field(line: &str, field: &str, value: Value) -> String {
    let mut row: Value = serde_json::from_str(line).expect("row");
    row[field] = value;
    serde_json::to_string(&row).expect("serialize row")
}

fn row_id(line: &str) -> String {
    let row: Value = serde_json::from_str(line).expect("row");
    row["id"].as_str().expect("row id").to_string()
}

fn reconcile_receipt(ws: &BrWorkspace, dry_run: bool, label: &str) -> Value {
    let mut args = vec!["sync", "--reconcile", "--json"];
    if dry_run {
        args.push("--dry-run");
    }
    let run = run_br(ws, args, label);
    assert!(
        run.status.success(),
        "reconcile ({}) failed: {}",
        if dry_run { "dry-run" } else { "apply" },
        run.stderr
    );
    parse_json_value(&run.stdout)
}

fn plan_count(receipt: &Value, field: &str) -> u64 {
    receipt["plan"][field]
        .as_u64()
        .unwrap_or_else(|| panic!("plan.{field} missing in receipt: {receipt}"))
}

fn events_witness(ws: &BrWorkspace) -> (u64, Option<i64>) {
    let storage = SqliteStorage::open(&db_path(ws)).expect("open storage");
    storage.events_table_witness().expect("events witness")
}

fn all_events_dump(ws: &BrWorkspace) -> String {
    let storage = SqliteStorage::open(&db_path(ws)).expect("open storage");
    let events = storage.get_all_events(100_000).expect("events");
    format!("{events:?}")
}

/// Total issue rows (including tombstones), read via the library so counts
/// are filter-independent and work while the CLI write lock is held.
fn issue_count(ws: &BrWorkspace, _label: &str) -> usize {
    let storage = SqliteStorage::open(&db_path(ws)).expect("open storage");
    storage.count_all_issues().expect("count issues")
}

fn get_needs_flush(ws: &BrWorkspace) -> Option<String> {
    let storage = SqliteStorage::open(&db_path(ws)).expect("open storage");
    storage.get_metadata("needs_flush").expect("metadata")
}

fn reconcile_import_config(ws: &BrWorkspace) -> ImportConfig {
    ImportConfig {
        skip_prefix_validation: true,
        rename_on_import: false,
        clear_duplicate_external_refs: false,
        force_upsert: false,
        beads_dir: Some(beads_dir(ws)),
        allow_external_jsonl: false,
        show_progress: false,
        ..ImportConfig::default()
    }
}

fn reconcile_persistent_file_stats(ws: &BrWorkspace) -> BTreeMap<String, (u128, u64)> {
    // Engine namespace admission refreshes these sidecars on read-only opens.
    // Their contents remain covered by hash_files_under; every other file's
    // stat retains the existing dry-run contract.
    stat_files_under(&beads_dir(ws))
        .into_iter()
        .filter(|(path, _)| !path.contains("-fsqlite-ns-"))
        .collect()
}

// ============================================================================
// Mode validation
// ============================================================================

#[test]
fn bare_sync_refused_and_reconcile_mode_exclusive() {
    let ws = BrWorkspace::new();
    init_workspace(&ws, "modes");

    let bare = run_br(&ws, ["sync"], "modes_bare");
    assert!(!bare.status.success(), "bare sync must be refused");
    assert!(
        bare.stderr.contains("--reconcile"),
        "mode error must list --reconcile: {}",
        bare.stderr
    );

    for (args, needle, label) in [
        (
            vec!["sync", "--reconcile", "--import-only"],
            "exactly one",
            "modes_two",
        ),
        (
            vec!["sync", "--reconcile", "--force"],
            "--force cannot be used with --reconcile",
            "modes_force",
        ),
        (
            vec!["sync", "--reconcile", "--rename-prefix"],
            "--rename-prefix cannot be used with --reconcile",
            "modes_rename",
        ),
        (
            vec!["sync", "--reconcile", "--orphans", "skip"],
            "--orphans cannot be used with --reconcile",
            "modes_orphans",
        ),
    ] {
        let run = run_br(&ws, args.clone(), label);
        assert!(!run.status.success(), "{args:?} must be rejected");
        assert!(
            run.stderr.contains(needle),
            "{args:?} error should mention '{needle}': {}",
            run.stderr
        );
    }

    // --dry-run without --reconcile is rejected at the clap layer.
    let dry = run_br(&ws, ["sync", "--flush-only", "--dry-run"], "modes_dry");
    assert!(
        !dry.status.success(),
        "--dry-run without --reconcile must be rejected"
    );
}

// ============================================================================
// Additive reconciliation cycle classification (GitHub #536)
// ============================================================================

#[test]
fn additive_sibling_dag_plans_read_only_and_applies_all_typed_edges() {
    let source = BrWorkspace::new();
    let target = BrWorkspace::new();
    for (workspace, label) in [(&source, "dag_source_init"), (&target, "dag_target_init")] {
        let init = run_br(workspace, ["init", "--prefix", "bd"], label);
        assert!(init.status.success(), "init failed: {init:?}");
    }

    let mut ids = Vec::<String>::new();
    for (title, kind) in [("Parent", "epic"), ("Child A", "task"), ("Child B", "task")] {
        let mut args = vec![
            "--no-auto-import",
            "--no-auto-flush",
            "create",
            title,
            "--type",
            kind,
            "--status",
            "draft",
            "--json",
        ];
        if let Some(parent) = ids.first() {
            args.extend(["--parent", parent]);
        }
        let created = run_br(&source, args, &format!("dag_create_{title}"));
        assert!(created.status.success(), "create failed: {created:?}");
        let row: Value = serde_json::from_str(&created.stdout).expect("whole create JSON");
        ids.push(row["id"].as_str().expect("created ID").to_string());
    }
    let dependency = run_br(
        &source,
        [
            "--no-auto-import",
            "--no-auto-flush",
            "dep",
            "add",
            &ids[1],
            &ids[2],
            "--type",
            "blocks",
            "--json",
        ],
        "dag_add_sibling_dependency",
    );
    assert!(
        dependency.status.success(),
        "dep add failed: {dependency:?}"
    );
    let flush = run_br(
        &source,
        [
            "--no-auto-import",
            "--no-auto-flush",
            "sync",
            "--flush-only",
            "--json",
        ],
        "dag_source_flush",
    );
    assert!(flush.status.success(), "flush failed: {flush:?}");
    let cycles = run_br(
        &source,
        [
            "--no-auto-import",
            "--no-auto-flush",
            "dep",
            "cycles",
            "--include-closed",
            "--blocking-only",
            "--json",
        ],
        "dag_native_cycles",
    );
    assert!(cycles.status.success(), "cycle check failed: {cycles:?}");
    let cycles: Value = serde_json::from_str(&cycles.stdout).expect("whole cycle-check JSON");
    assert_eq!(cycles["cycles"], json!([]));
    assert_eq!(cycles["total_count"], 0);

    assert_eq!(issue_count(&target, "dag_empty_target"), 0);
    let source_hashes = hash_files_under(&beads_dir(&source));
    let source_stats = reconcile_persistent_file_stats(&source);
    let target_hashes = hash_files_under(&beads_dir(&target));
    let target_stats = reconcile_persistent_file_stats(&target);
    let external_jsonl = jsonl_path(&source).to_string_lossy().to_string();
    let plan = run_br_with_env(
        &target,
        [
            "--no-auto-import",
            "--no-auto-flush",
            "sync",
            "--reconcile-additive",
            "--allow-external-jsonl",
            "--json",
        ],
        [("BEADS_JSONL", &external_jsonl)],
        "dag_additive_plan",
    );
    assert!(plan.status.success(), "DAG plan was refused: {plan:?}");
    let receipt: Value = serde_json::from_str(&plan.stdout).expect("whole additive plan JSON");
    let mut expected_ids = ids.clone();
    expected_ids.sort();
    assert_eq!(receipt["status"], "ready");
    assert_eq!(receipt["created"], 3);
    assert_eq!(receipt["created_issue_ids"], json!(expected_ids));
    assert_eq!(receipt["preexisting_blocking_cycles"], 0);
    assert_eq!(receipt["projected_blocking_cycles"], 0);
    assert_eq!(receipt["new_blocking_cycles"], 0);
    assert_eq!(receipt["conflict_issue_ids"], json!([]));
    assert_eq!(receipt["conflict_witnesses"], json!([]));
    assert_eq!(hash_files_under(&beads_dir(&source)), source_hashes);
    assert_eq!(reconcile_persistent_file_stats(&source), source_stats);
    assert_eq!(hash_files_under(&beads_dir(&target)), target_hashes);
    assert_eq!(reconcile_persistent_file_stats(&target), target_stats);

    let apply = run_br_with_env(
        &target,
        [
            "--no-auto-import",
            "--no-auto-flush",
            "sync",
            "--reconcile-additive",
            "--allow-external-jsonl",
            "--apply",
            "--expect-plan-sha256",
            receipt["plan_sha256"].as_str().expect("reviewed plan SHA"),
            "--json",
        ],
        [("BEADS_JSONL", &external_jsonl)],
        "dag_additive_apply",
    );
    assert!(apply.status.success(), "DAG apply failed: {apply:?}");
    let applied: Value = serde_json::from_str(&apply.stdout).expect("whole additive apply JSON");
    assert_eq!(applied["status"], "applied");
    assert_eq!(applied["created"], 3);
    assert_eq!(applied["events_before"], applied["events_after"]);
    assert_eq!(hash_files_under(&beads_dir(&source)), source_hashes);
    assert_eq!(reconcile_persistent_file_stats(&source), source_stats);
    let storage = SqliteStorage::open(&db_path(&target)).expect("open applied target");
    let stored = storage
        .get_issues_for_export(&ids)
        .expect("read applied graph");
    assert_eq!(stored.len(), 3);
    let mut actual_edges = stored
        .iter()
        .flat_map(|issue| &issue.dependencies)
        .map(|dependency| {
            (
                dependency.issue_id.clone(),
                dependency.depends_on_id.clone(),
                dependency.dep_type.as_str().to_string(),
            )
        })
        .collect::<Vec<_>>();
    actual_edges.sort();
    let mut expected_edges = vec![
        (ids[1].clone(), ids[0].clone(), "parent-child".to_string()),
        (ids[2].clone(), ids[0].clone(), "parent-child".to_string()),
        (ids[1].clone(), ids[2].clone(), "blocks".to_string()),
    ];
    expected_edges.sort();
    assert_eq!(
        actual_edges, expected_edges,
        "reconciliation must retain every typed edge"
    );
}

#[test]
fn additive_real_cycles_refuse_planning_and_apply_without_mutation() {
    for (label, ids, edges, reason, expected_cycles) in [
        (
            "two_node_cycle",
            &["bd-cycle-a", "bd-cycle-b"][..],
            &[(0usize, 1usize), (1, 0)][..],
            "projected_blocking_cycle",
            1,
        ),
        (
            "self_loop",
            &["bd-self-loop"][..],
            &[(0, 0)][..],
            "self_dependency",
            0,
        ),
    ] {
        let target = BrWorkspace::new();
        let init = run_br(
            &target,
            ["init", "--prefix", "bd"],
            &format!("{label}_init"),
        );
        assert!(init.status.success(), "init failed: {init:?}");
        let seed_id = create_issue(
            &target,
            "Keep existing database state",
            &format!("{label}_seed"),
        );
        let events_before = all_events_dump(&target);
        let source_path = target.root.join("cyclic-issues.jsonl");
        let source = ids
            .iter()
            .enumerate()
            .map(|(index, id)| {
                let dependencies = edges
                    .iter()
                    .filter(|(from, _)| *from == index)
                    .map(|(_, to)| {
                        json!({
                            "issue_id": id,
                            "depends_on_id": ids[*to],
                            "type": "blocks",
                            "created_at": "2026-01-01T00:00:00Z"
                        })
                    })
                    .collect::<Vec<_>>();
                json!({
                    "id": id,
                    "title": format!("Cyclic source issue {index}"),
                    "status": "draft",
                    "priority": 2,
                    "issue_type": "task",
                    "created_at": "2026-01-01T00:00:00Z",
                    "updated_at": "2026-01-01T00:00:00Z",
                    "dependencies": dependencies
                })
                .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(&source_path, source.as_bytes()).expect("write cyclic source fixture");
        let source_stat = fs::metadata(&source_path).expect("cyclic source metadata");
        let target_hashes = hash_files_under(&beads_dir(&target));
        let target_stats = reconcile_persistent_file_stats(&target);
        let source_override = source_path.to_string_lossy().to_string();
        let mut args = vec![
            "--no-auto-import",
            "--no-auto-flush",
            "sync",
            "--reconcile-additive",
            "--allow-external-jsonl",
            "--json",
        ];
        let plan = run_br_with_env(
            &target,
            args.clone(),
            [("BEADS_JSONL", &source_override)],
            &format!("{label}_plan"),
        );
        assert_eq!(
            plan.status.code(),
            Some(6),
            "cycle plan must conflict: {plan:?}"
        );
        let receipt: Value =
            serde_json::from_str(&plan.stdout).expect("whole conflicted plan JSON");
        assert_eq!(receipt["status"], "conflicted");
        assert_eq!(receipt["preexisting_blocking_cycles"], 0);
        assert_eq!(receipt["projected_blocking_cycles"], expected_cycles);
        assert_eq!(receipt["new_blocking_cycles"], expected_cycles);
        assert_eq!(receipt["conflict_issue_ids"], json!(ids));
        assert_eq!(receipt["conflict_reasons"][reason], json!(ids.len()));
        let witnesses = receipt["conflict_witnesses"]
            .as_array()
            .expect("conflict witnesses");
        assert_eq!(witnesses.len(), ids.len());
        let mut expected_member_hashes = ids
            .iter()
            .map(|id| beads_rust::util::hex_encode(&Sha256::digest(id.as_bytes())))
            .collect::<Vec<_>>();
        expected_member_hashes.sort();
        for (witness, id) in witnesses.iter().zip(ids) {
            assert_eq!(witness["issue_id"], *id);
            assert_eq!(witness["reasons"], json!([reason]));
            let details = witness["details"]
                .as_array()
                .expect("cycle witness details");
            assert_eq!(details.len(), 1);
            assert_eq!(details[0]["reason"], reason);
            assert_eq!(
                details[0]["related_value_sha256"],
                json!(expected_member_hashes)
            );
        }
        assert_eq!(hash_files_under(&beads_dir(&target)), target_hashes);
        assert_eq!(reconcile_persistent_file_stats(&target), target_stats);

        args.extend([
            "--apply",
            "--expect-plan-sha256",
            receipt["plan_sha256"]
                .as_str()
                .expect("conflicted plan SHA"),
        ]);
        let apply = run_br_with_env(
            &target,
            args,
            [("BEADS_JSONL", &source_override)],
            &format!("{label}_apply_refused"),
        );
        assert_eq!(
            apply.status.code(),
            Some(6),
            "cyclic apply must refuse: {apply:?}"
        );
        assert_eq!(hash_files_under(&beads_dir(&target)), target_hashes);
        assert_eq!(reconcile_persistent_file_stats(&target), target_stats);
        assert_eq!(
            fs::read(&source_path).expect("source after refusal"),
            source.as_bytes()
        );
        let source_after = fs::metadata(&source_path).expect("source metadata after refusal");
        assert_eq!(source_after.len(), source_stat.len());
        assert_eq!(
            source_after.modified().unwrap(),
            source_stat.modified().unwrap()
        );
        assert_eq!(all_events_dump(&target), events_before);
        let storage = SqliteStorage::open(&db_path(&target)).expect("open unchanged target");
        assert_eq!(
            storage.count_all_issues().expect("unchanged issue count"),
            1
        );
        assert!(
            storage
                .get_issue(&seed_id)
                .expect("read existing seed")
                .is_some()
        );
        for id in ids {
            assert!(
                storage
                    .get_issue(id)
                    .expect("read rejected issue")
                    .is_none()
            );
        }
    }
}

// ============================================================================
// Portable source_repo_path migration (#402)
// ============================================================================

#[test]
fn prerequisite_only_reconcile_preserves_acceptance_and_audit() {
    let ws = BrWorkspace::new();
    init_workspace(&ws, "prerequisite");
    let id = create_issue(&ws, "Prepare before implementation", "prerequisite_create");
    let criteria = "- [ ] Implement the real API\r\n";
    let original = "- [ ] Review the API\n";
    let completed = "## Preparation\n- [X] Review the API\n";
    let set = run_br(
        &ws,
        [
            "update",
            &id,
            "--acceptance-criteria",
            criteria,
            "--prerequisites",
            original,
            "--json",
        ],
        "prerequisite_set",
    );
    assert!(set.status.success(), "{set:?}");
    let before = SqliteStorage::open(&db_path(&ws)).unwrap();
    let events = before.get_all_events(0).unwrap();
    let original_issue = before.get_issue(&id).unwrap().unwrap();
    let newer = original_issue.updated_at + chrono::Duration::seconds(1);
    let hash = original_issue.content_hash;
    drop(before);
    let mut lines = read_jsonl_lines(&ws);
    assert_eq!(lines.len(), 1);
    lines[0] = set_row_field(&lines[0], "prerequisites", json!(completed));
    lines[0] = set_row_field(&lines[0], "updated_at", json!(newer));
    write_jsonl_lines(&ws, &lines);
    let source = fs::read(jsonl_path(&ws)).unwrap();
    let dry = run_br(
        &ws,
        ["sync", "--reconcile", "--dry-run", "--json"],
        "prerequisite_reconcile_dry",
    );
    assert!(dry.status.success(), "{dry:?}");
    let dry_receipt: Value = serde_json::from_str(&dry.stdout).unwrap();
    assert_eq!(plan_count(&dry_receipt, "updated"), 1);
    let unchanged = SqliteStorage::open(&db_path(&ws)).unwrap();
    assert_eq!(
        unchanged
            .get_issue(&id)
            .unwrap()
            .unwrap()
            .prerequisites
            .as_deref(),
        Some(original)
    );
    assert_eq!(unchanged.get_all_events(0).unwrap(), events);
    drop(unchanged);
    let apply = run_br(
        &ws,
        ["sync", "--reconcile", "--json"],
        "prerequisite_reconcile_apply",
    );
    assert!(apply.status.success(), "{apply:?}");
    assert_eq!(fs::read(jsonl_path(&ws)).unwrap(), source);
    let after = SqliteStorage::open(&db_path(&ws)).unwrap();
    let issue = after.get_issue(&id).unwrap().unwrap();
    assert_eq!(issue.prerequisites.as_deref(), Some(completed));
    assert_eq!(issue.acceptance_criteria.as_deref(), Some(criteria));
    assert_ne!(issue.content_hash, hash);
    assert_eq!(after.get_all_events(0).unwrap(), events);
    drop(after);
    let repeated = run_br(
        &ws,
        ["sync", "--reconcile", "--json"],
        "prerequisite_reconcile_noop",
    );
    assert!(repeated.status.success(), "{repeated:?}");
    let repeated_receipt: Value = serde_json::from_str(&repeated.stdout).unwrap();
    assert_eq!(plan_count(&repeated_receipt, "updated"), 0);
    assert_eq!(plan_count(&repeated_receipt, "skipped_equal"), 1);
    assert_eq!(fs::read(jsonl_path(&ws)).unwrap(), source);
    let repeated_storage = SqliteStorage::open(&db_path(&ws)).unwrap();
    assert_eq!(repeated_storage.get_all_events(0).unwrap(), events);
}

#[test]
fn source_repo_path_migration_reconciles_and_is_idempotent() {
    let ws = BrWorkspace::new();
    init_workspace(&ws, "pathmig");
    let database_newer_id = create_issue(&ws, "Database-newer row", "pathmig_db");
    let source_newer_id = create_issue(&ws, "Source-newer row", "pathmig_source");
    let flush = run_br(&ws, ["sync", "--flush-only", "--json"], "pathmig_flush");
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);

    let database_update = run_br(
        &ws,
        [
            "update",
            &database_newer_id,
            "--source-repo",
            "portable-db-name",
            "--source-repo-path",
            "/home/foreign-checkout/database-copy",
            "--no-auto-import",
            "--no-auto-flush",
            "--json",
        ],
        "pathmig_db_update",
    );
    assert!(
        database_update.status.success(),
        "database update failed: {}",
        database_update.stderr
    );

    let mut lines = read_jsonl_lines(&ws);
    let template = lines
        .iter()
        .find(|line| row_id(line) == source_newer_id)
        .expect("source-newer template")
        .clone();
    let source_index = lines
        .iter()
        .position(|line| row_id(line) == source_newer_id)
        .expect("source-newer row");
    lines[source_index] = set_row_field(
        &lines[source_index],
        "description",
        json!("newer source payload"),
    );
    lines[source_index] = set_row_field(
        &lines[source_index],
        "updated_at",
        json!("2030-01-01T00:00:00Z"),
    );
    lines[source_index] = set_row_field(
        &lines[source_index],
        "source_repo",
        json!("portable-jsonl-name"),
    );
    lines[source_index] = set_row_field(
        &lines[source_index],
        "source_repo_path",
        json!("/tmp/foreign-checkout/source-copy"),
    );

    let mut source_only: Value = serde_json::from_str(&clone_row(
        &template,
        "br-pathnew1",
        "Source-only row",
        "2030-01-02T00:00:00Z",
        "2030-01-02T00:00:00Z",
    ))
    .expect("source-only row");
    source_only["source_repo"] = json!("portable-source-only-name");
    source_only["source_repo_path"] = json!("/var/tmp/foreign-checkout/source-only-copy");
    lines.push(serde_json::to_string(&source_only).expect("serialize source-only row"));
    write_jsonl_lines(&ws, &lines);

    let jsonl_before_plan = fs::read(jsonl_path(&ws)).expect("JSONL before dry-run");
    let database_before_plan = SqliteStorage::open(&db_path(&ws))
        .expect("open DB before dry-run")
        .get_issue(&database_newer_id)
        .expect("read database-newer row")
        .expect("database-newer row exists");
    let plan_run = run_br(
        &ws,
        [
            "sync",
            "--migrate-source-repo-path",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "pathmig_plan",
    );
    assert!(
        plan_run.status.success(),
        "migration plan failed: {}",
        plan_run.stderr
    );
    let plan = parse_json_value(&plan_run.stdout);
    assert_eq!(
        plan["schema"].as_str(),
        Some("br.sync.source-repo-path-migration.v1")
    );
    assert_eq!(plan["mode"].as_str(), Some("dry_run"));
    assert_eq!(plan["applied"].as_bool(), Some(false));
    assert_eq!(plan["no_op"].as_bool(), Some(false));
    assert_eq!(plan["source_only_created"].as_u64(), Some(1));
    assert_eq!(plan["source_newer_updated"].as_u64(), Some(1));
    assert_eq!(plan["database_newer_preserved"].as_u64(), Some(1));
    assert_eq!(plan["jsonl_rewrite_required"].as_bool(), Some(true));
    assert_eq!(plan["source_repo_preserved"].as_bool(), Some(true));
    assert_eq!(plan["vcs_status"].as_str(), Some("not_probed"));
    let target = canonical_root(&ws);
    assert_eq!(plan["target_path"].as_str(), Some(target.as_str()));
    let token = plan["plan_sha256"]
        .as_str()
        .expect("plan token")
        .to_string();
    assert_eq!(token.len(), 64);
    assert_eq!(
        fs::read(jsonl_path(&ws)).expect("JSONL after dry-run"),
        jsonl_before_plan,
        "dry-run must not rewrite JSONL"
    );
    let database_after_plan = SqliteStorage::open(&db_path(&ws))
        .expect("open DB after dry-run")
        .get_issue(&database_newer_id)
        .expect("read database-newer row")
        .expect("database-newer row exists");
    assert_eq!(
        database_after_plan.source_repo_path, database_before_plan.source_repo_path,
        "dry-run must not normalize the database"
    );

    let apply_run = run_br(
        &ws,
        [
            "sync",
            "--migrate-source-repo-path",
            "--apply",
            "--expect-plan-sha256",
            &token,
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "pathmig_apply",
    );
    assert!(
        apply_run.status.success(),
        "migration apply failed: stdout={} stderr={}",
        apply_run.stdout,
        apply_run.stderr
    );
    let applied = parse_json_value(&apply_run.stdout);
    assert_eq!(applied["mode"].as_str(), Some("apply"));
    assert_eq!(applied["applied"].as_bool(), Some(true));
    assert_eq!(applied["no_op"].as_bool(), Some(false));
    assert_eq!(applied["plan_sha256"].as_str(), Some(token.as_str()));
    assert!(applied["receipt_id"].as_str().is_some());

    let storage = SqliteStorage::open(&db_path(&ws)).expect("open normalized DB");
    let database_newer = storage
        .get_issue(&database_newer_id)
        .expect("read database-newer")
        .expect("database-newer exists");
    let source_newer = storage
        .get_issue(&source_newer_id)
        .expect("read source-newer")
        .expect("source-newer exists");
    let imported = storage
        .get_issue("br-pathnew1")
        .expect("read source-only")
        .expect("source-only exists");
    drop(storage);
    for issue in [&database_newer, &source_newer, &imported] {
        assert_eq!(
            issue.source_repo_path.as_deref(),
            Some(target.as_str()),
            "{} retained a foreign source_repo_path",
            issue.id
        );
    }
    assert_eq!(
        database_newer.source_repo.as_deref(),
        Some("portable-db-name")
    );
    assert_eq!(
        source_newer.source_repo.as_deref(),
        Some("portable-jsonl-name")
    );
    assert_eq!(
        source_newer.description.as_deref(),
        Some("newer source payload")
    );
    assert_eq!(
        imported.source_repo.as_deref(),
        Some("portable-source-only-name")
    );

    let normalized_jsonl = read_jsonl_lines(&ws)
        .into_iter()
        .map(|line| {
            let row: Value = serde_json::from_str(&line).expect("normalized JSONL row");
            (row["id"].as_str().expect("row id").to_string(), row)
        })
        .collect::<BTreeMap<_, _>>();
    assert_eq!(
        normalized_jsonl.len(),
        3,
        "migration lost or duplicated rows"
    );
    // GitHub #528: the database adopts this machine's path, but the shared
    // JSONL never carries it; the migration strips legacy values.
    for row in normalized_jsonl.values() {
        assert!(
            row.get("source_repo_path").is_none(),
            "migrated JSONL still carries source_repo_path: {row}"
        );
    }
    assert_eq!(
        normalized_jsonl[&database_newer_id]["source_repo"].as_str(),
        Some("portable-db-name")
    );
    assert_eq!(
        normalized_jsonl[&source_newer_id]["source_repo"].as_str(),
        Some("portable-jsonl-name")
    );
    assert_eq!(
        normalized_jsonl["br-pathnew1"]["source_repo"].as_str(),
        Some("portable-source-only-name")
    );

    let idempotent_plan = run_br(
        &ws,
        [
            "sync",
            "--migrate-source-repo-path",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "pathmig_idempotent_plan",
    );
    assert!(idempotent_plan.status.success());
    let idempotent = parse_json_value(&idempotent_plan.stdout);
    assert_eq!(idempotent["no_op"].as_bool(), Some(true));
    let idempotent_token = idempotent["plan_sha256"]
        .as_str()
        .expect("idempotent token");
    let normalized_bytes = fs::read(jsonl_path(&ws)).expect("normalized JSONL bytes");
    let idempotent_apply = run_br(
        &ws,
        [
            "sync",
            "--migrate-source-repo-path",
            "--apply",
            "--expect-plan-sha256",
            idempotent_token,
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "pathmig_idempotent_apply",
    );
    assert!(idempotent_apply.status.success());
    let idempotent_applied = parse_json_value(&idempotent_apply.stdout);
    assert_eq!(idempotent_applied["applied"].as_bool(), Some(true));
    assert_eq!(idempotent_applied["no_op"].as_bool(), Some(true));
    assert!(idempotent_applied.get("receipt_id").is_none());
    assert_eq!(
        fs::read(jsonl_path(&ws)).expect("JSONL after idempotent apply"),
        normalized_bytes,
        "idempotent apply must not rewrite JSONL"
    );

    let stale_plan_token = idempotent_token.to_string();
    let drift = run_br(
        &ws,
        [
            "update",
            &database_newer_id,
            "--source-repo-path",
            "/tmp/newer-foreign-checkout",
            "--no-auto-import",
            "--no-auto-flush",
            "--json",
        ],
        "pathmig_stale_drift",
    );
    assert!(
        drift.status.success(),
        "drift update failed: stdout={} stderr={}",
        drift.stdout,
        drift.stderr
    );
    let stale_apply = run_br(
        &ws,
        [
            "sync",
            "--migrate-source-repo-path",
            "--apply",
            "--expect-plan-sha256",
            &stale_plan_token,
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "pathmig_stale_apply",
    );
    assert!(!stale_apply.status.success(), "stale plan token must fail");
    assert!(format!("{}{}", stale_apply.stdout, stale_apply.stderr).contains("plan changed"));
    let drifted = SqliteStorage::open(&db_path(&ws))
        .expect("open drifted DB")
        .get_issue(&database_newer_id)
        .expect("read drifted row")
        .expect("drifted row exists");
    assert_eq!(
        drifted.source_repo_path.as_deref(),
        Some("/tmp/newer-foreign-checkout"),
        "stale apply must not partially normalize the database"
    );
    assert_eq!(
        fs::read(jsonl_path(&ws)).expect("JSONL after stale apply"),
        normalized_bytes,
        "stale apply must not rewrite JSONL"
    );
}

#[test]
fn source_repo_path_migration_rejects_equal_timestamp_payload_conflict() {
    let ws = BrWorkspace::new();
    init_workspace(&ws, "pathconflict");
    let id = create_issue(&ws, "Equal timestamp row", "pathconflict_create");
    let flush = run_br(
        &ws,
        ["sync", "--flush-only", "--json"],
        "pathconflict_flush",
    );
    assert!(flush.status.success());
    let mut lines = read_jsonl_lines(&ws);
    lines[0] = set_row_field(&lines[0], "description", json!("divergent source payload"));
    lines[0] = set_row_field(
        &lines[0],
        "source_repo_path",
        json!("/tmp/equal-timestamp-foreign-copy"),
    );
    write_jsonl_lines(&ws, &lines);
    let jsonl_before = fs::read(jsonl_path(&ws)).expect("JSONL before conflict");

    let run = run_br(
        &ws,
        [
            "sync",
            "--migrate-source-repo-path",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "pathconflict_plan",
    );
    assert!(!run.status.success(), "equal-timestamp drift must fail");
    assert!(
        format!("{}{}", run.stdout, run.stderr)
            .contains("equal timestamps but divergent DB/JSONL payloads")
    );
    assert_eq!(
        fs::read(jsonl_path(&ws)).expect("JSONL after conflict"),
        jsonl_before
    );
    let issue = SqliteStorage::open(&db_path(&ws))
        .expect("open conflict DB")
        .get_issue(&id)
        .expect("read conflict row")
        .expect("conflict row exists");
    assert_ne!(
        issue.description.as_deref(),
        Some("divergent source payload"),
        "failed plan must not import the source payload"
    );
}

// ============================================================================
// source_repo_path stays machine-local (GitHub #528)
// ============================================================================

fn canonical_root(ws: &BrWorkspace) -> String {
    dunce::canonicalize(&ws.root)
        .expect("canonical workspace")
        .to_string_lossy()
        .into_owned()
}

fn local_source_repo_path(ws: &BrWorkspace, id: &str) -> Option<String> {
    SqliteStorage::open(&db_path(ws))
        .expect("open DB")
        .get_issue(id)
        .expect("read issue")
        .expect("issue exists")
        .source_repo_path
}

fn assert_jsonl_has_no_source_repo_path(ws: &BrWorkspace) {
    for line in read_jsonl_lines(ws) {
        let row: Value = serde_json::from_str(&line).expect("JSONL row");
        assert!(
            row.get("source_repo_path").is_none(),
            "JSONL leaks a machine-local source_repo_path: {line}"
        );
    }
}

#[test]
fn create_keeps_source_repo_path_out_of_jsonl() {
    let ws = BrWorkspace::new();
    init_workspace(&ws, "localpath");
    let id = create_issue(&ws, "Local path stays local", "localpath_create");
    let flush = run_br(&ws, ["sync", "--flush-only", "--json"], "localpath_flush");
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);

    assert_jsonl_has_no_source_repo_path(&ws);
    let row: Value = serde_json::from_str(&read_jsonl_lines(&ws)[0]).expect("row");
    assert!(
        row["source_repo"].as_str().is_some(),
        "the portable source_repo name is still exported: {row}"
    );

    let root = canonical_root(&ws);
    assert_eq!(
        local_source_repo_path(&ws, &id).as_deref(),
        Some(root.as_str())
    );
    let show = run_br(&ws, ["show", &id, "--json"], "localpath_show");
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let shown = parse_json_value(&show.stdout);
    let shown = shown.get(0).unwrap_or(&shown);
    assert_eq!(shown["source_repo_path"].as_str(), Some(root.as_str()));

    // A later edit re-exports the row without the field as well.
    let update = run_br(
        &ws,
        ["update", &id, "--title", "Edited locally", "--json"],
        "localpath_update",
    );
    assert!(update.status.success(), "update failed: {}", update.stderr);
    let flush = run_br(&ws, ["sync", "--flush-only", "--json"], "localpath_reflush");
    assert!(flush.status.success(), "reflush failed: {}", flush.stderr);
    assert_jsonl_has_no_source_repo_path(&ws);
}

#[test]
fn pulled_path_free_rows_keep_the_local_source_repo_path() {
    for mode in ["import-only", "reconcile"] {
        let ws = BrWorkspace::new();
        init_workspace(&ws, "pulledpath");
        let id = create_issue(&ws, "Edited on another machine", "pulledpath_create");
        let flush = run_br(&ws, ["sync", "--flush-only", "--json"], "pulledpath_flush");
        assert!(flush.status.success(), "flush failed: {}", flush.stderr);
        let root = canonical_root(&ws);

        // A peer edits the row; its export has no source_repo_path.
        let mut lines = read_jsonl_lines(&ws);
        lines[0] = set_row_field(&lines[0], "title", json!("Edited by a peer"));
        lines[0] = set_row_field(&lines[0], "updated_at", json!("2030-01-01T00:00:00Z"));
        write_jsonl_lines(&ws, &lines);

        let run = run_br(
            &ws,
            ["sync", &format!("--{mode}"), "--json"],
            "pulledpath_sync",
        );
        assert!(
            run.status.success(),
            "{mode} failed: stdout={} stderr={}",
            run.stdout,
            run.stderr
        );
        let issue = SqliteStorage::open(&db_path(&ws))
            .expect("open DB")
            .get_issue(&id)
            .expect("read issue")
            .expect("issue exists");
        assert_eq!(
            issue.title, "Edited by a peer",
            "{mode} did not apply the peer edit"
        );
        assert_eq!(
            issue.source_repo_path.as_deref(),
            Some(root.as_str()),
            "{mode} erased the local source_repo_path"
        );
    }
}

#[test]
fn legacy_peer_edit_does_not_replace_the_local_source_repo_path() {
    // An older br on another machine still exports its own absolute path.
    // Importing that peer's edit applies the edit but keeps this machine's
    // path; the foreign path never names this workspace.
    for mode in ["import-only", "reconcile", "reconcile-additive"] {
        let ws = BrWorkspace::new();
        init_workspace(&ws, "legacyedit");
        let id = create_issue(&ws, "Edited by an older br", "legacyedit_create");
        let flush = run_br(&ws, ["sync", "--flush-only", "--json"], "legacyedit_flush");
        assert!(flush.status.success(), "flush failed: {}", flush.stderr);
        let root = canonical_root(&ws);

        let mut lines = read_jsonl_lines(&ws);
        lines[0] = set_row_field(&lines[0], "title", json!("Edited by an older peer"));
        lines[0] = set_row_field(&lines[0], "updated_at", json!("2030-01-01T00:00:00Z"));
        lines[0] = set_row_field(
            &lines[0],
            "source_repo_path",
            json!("/home/someone-else/checkout"),
        );
        write_jsonl_lines(&ws, &lines);

        let mut args = vec![
            "sync".to_string(),
            format!("--{mode}"),
            "--json".to_string(),
        ];
        if mode == "reconcile-additive" {
            // A source-newer scalar edit needs explicit source authority.
            args.extend(["--resolve-source-id".to_string(), id.clone()]);
            let plan = run_br(&ws, args.clone(), "legacyedit_plan");
            assert!(
                plan.status.success(),
                "additive plan failed: {}",
                plan.stderr
            );
            let plan = parse_json_value(&plan.stdout);
            assert_eq!(plan["status"], "ready", "{plan}");
            args.extend([
                "--apply".to_string(),
                "--expect-plan-sha256".to_string(),
                plan["plan_sha256"].as_str().expect("plan sha").to_string(),
            ]);
        }
        let run = run_br(&ws, args, "legacyedit_sync");
        assert!(
            run.status.success(),
            "{mode} failed: stdout={} stderr={}",
            run.stdout,
            run.stderr
        );
        let issue = SqliteStorage::open(&db_path(&ws))
            .expect("open DB")
            .get_issue(&id)
            .expect("read issue")
            .expect("issue exists");
        assert_eq!(
            issue.title, "Edited by an older peer",
            "{mode} did not apply the peer edit"
        );
        assert_eq!(
            issue.source_repo_path.as_deref(),
            Some(root.as_str()),
            "{mode} replaced the local source_repo_path with a peer's"
        );
    }
}

#[test]
fn legacy_jsonl_with_absolute_source_repo_path_still_imports() {
    let ws = BrWorkspace::new();
    init_workspace(&ws, "legacypath");
    let id = create_issue(&ws, "Exported by an older br", "legacypath_create");
    let flush = run_br(&ws, ["sync", "--flush-only", "--json"], "legacypath_flush");
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);

    // Older br versions exported the creating machine's absolute path.
    let mut lines = read_jsonl_lines(&ws);
    let legacy = clone_row(
        &lines[0],
        "br-legacy1",
        "Legacy row from another machine",
        "2026-01-01T00:00:00Z",
        "2026-01-01T00:00:00Z",
    );
    lines.push(set_row_field(
        &legacy,
        "source_repo_path",
        json!("/home/someone-else/checkout"),
    ));
    lines[0] = set_row_field(
        &lines[0],
        "source_repo_path",
        json!("/home/someone-else/checkout"),
    );
    write_jsonl_lines(&ws, &lines);

    let import = run_br(
        &ws,
        ["sync", "--import-only", "--json"],
        "legacypath_import",
    );
    assert!(
        import.status.success(),
        "legacy import failed: stdout={} stderr={}",
        import.stdout,
        import.stderr
    );
    assert_eq!(
        local_source_repo_path(&ws, "br-legacy1").as_deref(),
        Some("/home/someone-else/checkout")
    );
    // The shared row's payload is unchanged, so its local path is untouched.
    let root = canonical_root(&ws);
    assert_eq!(
        local_source_repo_path(&ws, &id).as_deref(),
        Some(root.as_str())
    );

    // Rows written from now on drop the legacy field.
    let update = run_br(
        &ws,
        [
            "update",
            "br-legacy1",
            "--title",
            "Touched locally",
            "--json",
        ],
        "legacypath_update",
    );
    assert!(update.status.success(), "update failed: {}", update.stderr);
    let flush = run_br(
        &ws,
        ["sync", "--flush-only", "--json"],
        "legacypath_reflush",
    );
    assert!(flush.status.success(), "reflush failed: {}", flush.stderr);
    let rewritten = read_jsonl_lines(&ws)
        .into_iter()
        .find(|line| row_id(line) == "br-legacy1")
        .expect("legacy row");
    let rewritten: Value = serde_json::from_str(&rewritten).expect("row");
    assert!(rewritten.get("source_repo_path").is_none(), "{rewritten}");
}

// ============================================================================
// The false-equal state (the bug this mode exists to repair)
// ============================================================================

/// Build a workspace in the false-equal state:
/// - DB holds 3 issues,
/// - JSONL holds those 3 plus 2 JSONL-only rows, with 1 shared row newer,
/// - stored metadata hash matches the JSONL byte-for-byte.
///
/// Returns (`jsonl_only_ids`, `newer_shared_id`).
fn build_false_equal_workspace(ws: &BrWorkspace, label: &str) -> (Vec<String>, String) {
    init_workspace(ws, label);
    let _a = create_issue(ws, "Shared issue alpha", &format!("{label}_a"));
    let _b = create_issue(ws, "Shared issue beta", &format!("{label}_b"));
    let _c = create_issue(ws, "Shared issue gamma", &format!("{label}_c"));
    let flush = run_br(
        ws,
        ["sync", "--flush-only", "--json"],
        &format!("{label}_flush"),
    );
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);

    let mut lines = read_jsonl_lines(ws);
    assert_eq!(lines.len(), 3, "expected 3 exported rows");
    let template = lines[0].clone();

    // One shared row becomes strictly newer in JSONL (with real drift).
    let newer_shared_id = row_id(&lines[2]);
    lines[2] = set_row_field(&lines[2], "updated_at", json!("2030-01-01T00:00:00Z"));
    lines[2] = set_row_field(
        &lines[2],
        "description",
        json!("newer JSONL-side description"),
    );

    // Two JSONL-only rows the DB has never imported.
    let extra_a = clone_row(
        &template,
        "br-reconx1",
        "JSONL-only recovery row one",
        "2029-01-01T00:00:00Z",
        "2029-01-01T00:00:00Z",
    );
    let extra_b = clone_row(
        &template,
        "br-reconx2",
        "JSONL-only recovery row two",
        "2029-01-02T00:00:00Z",
        "2029-01-02T00:00:00Z",
    );
    lines.push(extra_a);
    lines.push(extra_b);
    write_jsonl_lines(ws, &lines);

    // Record the stored hash AS IF the auto-flush had just certified this
    // exact file — the finalize_incremental_auto_flush false-equal.
    plant_false_equal_metadata(ws);

    (
        vec!["br-reconx1".to_string(), "br-reconx2".to_string()],
        newer_shared_id,
    )
}

#[test]
fn import_only_heals_false_equal_and_dry_run_sees_it() {
    let ws = BrWorkspace::new();
    let (jsonl_only_ids, newer_shared_id) = build_false_equal_workspace(&ws, "blind");

    // Dry-run reconcile sees through the false-equal state.
    let receipt = reconcile_receipt(&ws, true, "blind_dry");
    assert_eq!(
        receipt["schema_version"].as_str(),
        Some("br.sync.reconcile.v1")
    );
    assert_eq!(receipt["mode"].as_str(), Some("dry_run"));
    assert_eq!(receipt["applied"].as_bool(), Some(false));
    assert_eq!(plan_count(&receipt, "created"), 2);
    assert_eq!(plan_count(&receipt, "updated"), 1);
    assert_eq!(plan_count(&receipt, "deleted"), 0);
    assert_eq!(
        receipt["target"]["stored_hash_matches_jsonl"].as_bool(),
        Some(true),
        "fixture must be in the false-equal state: {receipt}"
    );
    let created_ids: Vec<&str> = receipt["previews"]["created_ids"]
        .as_array()
        .expect("created_ids")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(created_ids, jsonl_only_ids, "created preview ids");
    let updated_ids: Vec<&str> = receipt["previews"]["updated_ids"]
        .as_array()
        .expect("updated_ids")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(
        updated_ids,
        vec![newer_shared_id.as_str()],
        "updated preview ids"
    );

    // `beads_rust-jdmh`: the stored-hash shortcut is no longer blind — the
    // coverage invariant rejects the uncovered hash match and the plain
    // import falls through and heals the divergence additively.
    let import = run_br(&ws, ["sync", "--import-only", "--json"], "blind_import");
    assert!(import.status.success(), "import failed: {}", import.stderr);
    let import_json = parse_json_value(&import.stdout);
    assert_eq!(
        import_json["created"].as_u64(),
        Some(2),
        "import must heal the false-equal state: {import_json}"
    );
    assert_eq!(
        issue_count(&ws, "blind_count"),
        5,
        "import must recover the JSONL-only rows"
    );
}

#[test]
fn force_import_repairs_exact_duplicate_comments_and_reports_the_repair() {
    let ws = BrWorkspace::new();
    init_workspace(&ws, "duplicate_comment");
    let issue_id = create_issue(
        &ws,
        "Duplicate comment recovery",
        "duplicate_comment_create",
    );
    let flush = run_br(
        &ws,
        ["sync", "--flush-only", "--json"],
        "duplicate_comment_flush",
    );
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);

    let mut lines = read_jsonl_lines(&ws);
    assert_eq!(lines.len(), 1, "fixture should contain one issue row");
    let mut row: Value = serde_json::from_str(&lines[0]).expect("issue row");
    let duplicate = json!({
        "id": 7,
        "issue_id": issue_id,
        "author": "recovery-probe",
        "text": "preserve one exact copy",
        "created_at": "2026-08-27T12:00:00Z"
    });
    row["comments"] = json!([duplicate.clone(), duplicate]);
    lines[0] = serde_json::to_string(&row).expect("serialize duplicated comment row");
    write_jsonl_lines(&ws, &lines);

    let import = run_br(
        &ws,
        [
            "--no-auto-import",
            "sync",
            "--import-only",
            "--force",
            "--json",
        ],
        "duplicate_comment_import",
    );
    assert!(
        import.status.success(),
        "force import failed: stdout={} stderr={}",
        import.stdout,
        import.stderr
    );
    let receipt = parse_json_value(&import.stdout);
    assert_eq!(
        receipt["exact_duplicate_comments_deduplicated"].as_u64(),
        Some(1),
        "repair count must be explicit in robot output: {receipt}"
    );

    let show = run_br(&ws, ["show", &issue_id, "--json"], "duplicate_comment_show");
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let shown = parse_json_value(&show.stdout);
    let issue = shown.get(0).unwrap_or(&shown);
    let comments = issue["comments"].as_array().expect("comments array");
    assert_eq!(comments.len(), 1, "only one exact comment copy may remain");
    assert_eq!(comments[0]["text"], "preserve one exact copy");
}

#[test]
fn show_fails_loudly_when_jsonl_id_is_missing_from_database() {
    let ws = BrWorkspace::new();
    init_workspace(&ws, "show_divergence");
    let issue_id = create_issue(&ws, "JSONL remains authoritative", "show_divergence_create");
    let flush = run_br(
        &ws,
        ["sync", "--flush-only", "--json"],
        "show_divergence_flush",
    );
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);

    let mut storage = SqliteStorage::open(&db_path(&ws)).expect("open storage");
    storage
        .purge_issue(&issue_id, "show-divergence-test")
        .expect("remove only the database row");
    drop(storage);

    let show = run_br(
        &ws,
        ["--no-auto-import", "show", &issue_id, "--json"],
        "show_divergence_probe",
    );
    assert!(!show.status.success(), "divergent show must fail closed");
    let output = format!("{}{}", show.stdout, show.stderr);
    assert!(
        output.contains("exists in JSONL but is not addressable in SQLite")
            && output.contains("refusing a false missing-issue result"),
        "show must distinguish database loss from a genuinely absent id: {output}"
    );

    let hash_suffix = issue_id.rsplit('-').next().expect("generated hash suffix");
    let partial_show = run_br(
        &ws,
        ["--no-auto-import", "show", hash_suffix, "--json"],
        "show_divergence_partial_probe",
    );
    assert!(
        !partial_show.status.success(),
        "divergent partial-id show must fail closed"
    );
    let partial_output = format!("{}{}", partial_show.stdout, partial_show.stderr);
    assert!(
        partial_output.contains("exists in JSONL but is not addressable in SQLite")
            && partial_output.contains("refusing a false missing-issue result"),
        "partial-id resolution must also distinguish database loss: {partial_output}"
    );
}

#[test]
fn dry_run_mutates_no_files_and_is_deterministic() {
    let ws = BrWorkspace::new();
    build_false_equal_workspace(&ws, "nomut");

    let before_hashes = hash_files_under(&beads_dir(&ws));
    let before_stats = stat_files_under(&beads_dir(&ws));

    // The read-only fast open engages with the explicit no-auto opt-outs;
    // the dry-run contract is zero mutation including the -wal/-shm family.
    let first = run_br(
        &ws,
        [
            "sync",
            "--reconcile",
            "--dry-run",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "nomut_dry1",
    );
    assert!(first.status.success(), "dry-run failed: {}", first.stderr);

    let after_hashes = hash_files_under(&beads_dir(&ws));
    let after_stats = stat_files_under(&beads_dir(&ws));
    assert_eq!(
        before_hashes, after_hashes,
        "dry-run must not change any .beads file contents (incl. -wal/-shm)"
    );
    // Stat comparison: the fsqlite namespace-admission sidecars
    // (`*-fsqlite-ns-use` / `*-fsqlite-ns-gate`) get their mtime refreshed by
    // the engine on EVERY database open, read-only included. Their contents
    // are covered by the hash assertion above; exempt only their stats.
    let strip_ns_sidecars = |m: &BTreeMap<String, (u128, u64)>| -> BTreeMap<String, (u128, u64)> {
        m.iter()
            .filter(|(k, _)| !k.contains("-fsqlite-ns-"))
            .map(|(k, v)| (k.clone(), *v))
            .collect()
    };
    assert_eq!(
        strip_ns_sidecars(&before_stats),
        strip_ns_sidecars(&after_stats),
        "dry-run must not touch any .beads file stat (mtime/size)"
    );

    let second = run_br(
        &ws,
        [
            "sync",
            "--reconcile",
            "--dry-run",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "nomut_dry2",
    );
    assert!(
        second.status.success(),
        "dry-run 2 failed: {}",
        second.stderr
    );
    assert_eq!(
        parse_json_value(&first.stdout),
        parse_json_value(&second.stdout),
        "identical state must produce an identical receipt"
    );
}

#[test]
fn apply_recovers_false_equal_state_without_touching_jsonl_or_events() {
    let ws = BrWorkspace::new();
    let (_, newer_shared_id) = build_false_equal_workspace(&ws, "recover");

    let jsonl_bytes_before = fs::read(jsonl_path(&ws)).expect("jsonl before");
    let events_before = events_witness(&ws);
    let events_dump_before = all_events_dump(&ws);

    let receipt = reconcile_receipt(&ws, false, "recover_apply");
    assert_eq!(receipt["mode"].as_str(), Some("apply"));
    assert_eq!(receipt["applied"].as_bool(), Some(true));
    assert_eq!(plan_count(&receipt, "created"), 2);
    assert_eq!(plan_count(&receipt, "updated"), 1);
    assert_eq!(
        receipt["events_before"], receipt["events_after"],
        "events must be preserved exactly: {receipt}"
    );
    assert_eq!(
        receipt["apply"]["metadata_repaired"].as_bool(),
        Some(true),
        "apply must repair the sync metadata: {receipt}"
    );

    // DB recovered: all five issues present, the shared row took the newer
    // JSONL description.
    assert_eq!(issue_count(&ws, "recover_count"), 5);
    let show = run_br(&ws, ["show", &newer_shared_id, "--json"], "recover_show");
    assert!(show.status.success(), "show failed: {}", show.stderr);
    assert!(
        show.stdout.contains("newer JSONL-side description"),
        "updated row must carry the newer JSONL content: {}",
        show.stdout
    );

    // JSONL bytes untouched; events byte-identical.
    let jsonl_bytes_after = fs::read(jsonl_path(&ws)).expect("jsonl after");
    assert_eq!(
        jsonl_bytes_before, jsonl_bytes_after,
        "apply must never write the JSONL"
    );
    assert_eq!(events_before, events_witness(&ws), "events witness drifted");
    assert_eq!(
        events_dump_before,
        all_events_dump(&ws),
        "event rows must be preserved byte-for-byte"
    );

    // A second dry-run is a zero-change no-op.
    let second = reconcile_receipt(&ws, true, "recover_dry2");
    assert_eq!(plan_count(&second, "created"), 0);
    assert_eq!(plan_count(&second, "updated"), 0);

    // And the metadata repair means plain import agrees the file is current.
    let import = run_br(&ws, ["sync", "--import-only", "--json"], "recover_import");
    assert!(import.status.success());
    let import_json = parse_json_value(&import.stdout);
    assert_eq!(import_json["created"].as_u64(), Some(0));
    assert_eq!(import_json["updated"].as_u64(), Some(0));
}

// ============================================================================
// Timestamp classification and drift
// ============================================================================

#[test]
fn timestamp_newer_equal_older_classification() {
    let ws = BrWorkspace::new();
    init_workspace(&ws, "ts");
    let _ = create_issue(&ws, "Row newer in JSONL", "ts_a");
    let _ = create_issue(&ws, "Row equal timestamps", "ts_b");
    let _ = create_issue(&ws, "Row older in JSONL", "ts_c");
    let flush = run_br(&ws, ["sync", "--flush-only", "--json"], "ts_flush");
    assert!(flush.status.success());

    let mut lines = read_jsonl_lines(&ws);
    assert_eq!(lines.len(), 3);
    // Row 0: JSONL strictly newer → update.
    lines[0] = set_row_field(&lines[0], "updated_at", json!("2031-01-01T00:00:00Z"));
    lines[0] = set_row_field(&lines[0], "description", json!("newer body"));
    // Row 1: untouched → equal timestamps → skip, certified equal.
    // Row 2: JSONL strictly older → skip, DB copy is newer. Back-dating the
    // JSONL row below its created_at would be repaired by import
    // normalization, so instead make the DB copy genuinely newer while the
    // JSONL keeps the pre-update export.
    let older_id = row_id(&lines[2]);
    write_jsonl_lines(&ws, &lines);
    // --no-auto-import: the modified JSONL would otherwise be imported at
    // this command's own startup, consuming the classifications this test
    // exists to observe. --no-auto-flush keeps the JSONL byte-stable.
    let bump = run_br(
        &ws,
        [
            "update",
            &older_id,
            "--priority",
            "0",
            "--no-auto-import",
            "--no-auto-flush",
            "--json",
        ],
        "ts_bump_db",
    );
    assert!(bump.status.success(), "bump failed: {}", bump.stderr);

    let receipt = reconcile_receipt(&ws, false, "ts_apply");
    assert_eq!(plan_count(&receipt, "created"), 0);
    assert_eq!(plan_count(&receipt, "updated"), 1);
    assert_eq!(plan_count(&receipt, "skipped_equal"), 1);
    assert_eq!(plan_count(&receipt, "skipped_older"), 1);

    // The older JSONL copy must NOT clobber the newer DB row: the local
    // priority-0 edit survives.
    let show = run_br(&ws, ["show", &older_id, "--json"], "ts_show_older");
    assert!(show.status.success());
    let shown = parse_json_value(&show.stdout);
    let priority = shown
        .get(0)
        .and_then(|v| v.get("priority"))
        .or_else(|| shown.get("priority"))
        .and_then(Value::as_u64);
    assert_eq!(
        priority,
        Some(0),
        "older JSONL copy must not overwrite the newer DB row: {}",
        show.stdout
    );

    // A local-newer row means the JSONL is behind: apply must mark the DB
    // for flush so the divergence is exported later.
    assert_eq!(
        receipt["apply"]["needs_flush_set"].as_bool(),
        Some(true),
        "skip-older must set needs_flush: {receipt}"
    );
    assert_eq!(get_needs_flush(&ws).as_deref(), Some("true"));
}

#[test]
fn content_hash_only_drift_is_uncertified_local_win() {
    let ws = BrWorkspace::new();
    init_workspace(&ws, "drift");
    let _ = create_issue(&ws, "Row with content drift", "drift_a");
    let flush = run_br(&ws, ["sync", "--flush-only", "--json"], "drift_flush");
    assert!(flush.status.success());

    let mut lines = read_jsonl_lines(&ws);
    // Same updated_at, different content: equal timestamps skip, but the DB
    // copy no longer matches the JSONL → uncertified local win.
    lines[0] = set_row_field(
        &lines[0],
        "description",
        json!("drifted body, same timestamp"),
    );
    write_jsonl_lines(&ws, &lines);

    let receipt = reconcile_receipt(&ws, false, "drift_apply");
    assert_eq!(plan_count(&receipt, "updated"), 0);
    assert_eq!(plan_count(&receipt, "skipped_equal"), 1);
    assert_eq!(
        receipt["apply"]["uncertified_local_wins"].as_u64(),
        Some(1),
        "content drift at equal timestamps must be uncertified: {receipt}"
    );
    assert_eq!(
        receipt["apply"]["needs_flush_set"].as_bool(),
        Some(true),
        "uncertified local wins must set needs_flush"
    );

    // DB keeps its own copy.
    let list = run_br(&ws, ["list", "--status", "all", "--json"], "drift_list");
    assert!(
        !list.stdout.contains("drifted body"),
        "equal-timestamp drift must not overwrite the DB row"
    );
}

#[test]
fn tombstone_protection_wins_over_live_jsonl_row() {
    let ws = BrWorkspace::new();
    init_workspace(&ws, "tomb");
    let id = create_issue(&ws, "Doomed issue", "tomb_a");
    let flush = run_br(&ws, ["sync", "--flush-only", "--json"], "tomb_flush");
    assert!(flush.status.success());
    let live_lines = read_jsonl_lines(&ws);

    // Tombstone the issue in the DB, then present the old live row again
    // (strictly newer timestamp so only tombstone protection can skip it).
    let delete = run_br(&ws, ["delete", &id, "--force"], "tomb_delete");
    assert!(delete.status.success(), "delete failed: {}", delete.stderr);
    let mut lines = live_lines;
    lines[0] = set_row_field(&lines[0], "updated_at", json!("2032-01-01T00:00:00Z"));
    write_jsonl_lines(&ws, &lines);

    let receipt = reconcile_receipt(&ws, false, "tomb_apply");
    assert_eq!(plan_count(&receipt, "created"), 0);
    assert_eq!(plan_count(&receipt, "updated"), 0);
    assert_eq!(plan_count(&receipt, "skipped_tombstone"), 1);

    let show = run_br(&ws, ["show", &id, "--json"], "tomb_show");
    assert!(
        show.stdout.contains("tombstone") || !show.status.success(),
        "tombstoned issue must stay tombstoned: {}",
        show.stdout
    );
}

// ============================================================================
// Relations, orphans, caches
// ============================================================================

#[test]
fn created_rows_carry_relations_and_unsuperseded_rows_survive() {
    let ws = BrWorkspace::new();
    init_workspace(&ws, "rel");
    let keeper = create_issue(&ws, "DB-only issue with relations", "rel_keeper");
    let comment = run_br(
        &ws,
        ["comments", "add", &keeper, "--message", "keeper comment"],
        "rel_comment",
    );
    assert!(
        comment.status.success(),
        "comment failed: {}",
        comment.stderr
    );
    let label_add = run_br(&ws, ["label", "add", &keeper, "keeplabel"], "rel_label");
    assert!(label_add.status.success());
    let anchor = create_issue(&ws, "Anchor issue", "rel_anchor");
    let flush = run_br(&ws, ["sync", "--flush-only", "--json"], "rel_flush");
    assert!(flush.status.success());

    // JSONL: only the anchor row plus one new row that depends on the anchor
    // and carries a label + comment. The keeper is db-only.
    let lines = read_jsonl_lines(&ws);
    let anchor_line = lines
        .iter()
        .find(|l| row_id(l) == anchor)
        .expect("anchor line")
        .clone();
    let mut new_row: Value = serde_json::from_str(&anchor_line).expect("row");
    new_row["id"] = json!("br-relnew1");
    new_row["title"] = json!("New row with relations");
    new_row["created_at"] = json!("2026-02-01T00:00:00Z");
    new_row["updated_at"] = json!("2026-02-01T00:00:00Z");
    new_row["labels"] = json!(["fromjsonl"]);
    new_row["dependencies"] = json!([{
        "issue_id": "br-relnew1",
        "depends_on_id": anchor,
        "type": "blocks",
        "created_at": "2026-02-01T00:00:00Z"
    }]);
    new_row["comments"] = json!([{
        "id": 1,
        "issue_id": "br-relnew1",
        "author": "jsonl",
        "text": "imported comment",
        "created_at": "2026-02-01T00:00:00Z"
    }]);
    let new_line = serde_json::to_string(&new_row).expect("serialize");
    write_jsonl_lines(&ws, &[anchor_line, new_line]);

    let receipt = reconcile_receipt(&ws, false, "rel_apply");
    assert_eq!(plan_count(&receipt, "created"), 1);
    assert_eq!(
        plan_count(&receipt, "db_only"),
        1,
        "keeper is db-only: {receipt}"
    );
    assert_eq!(receipt["relations"]["labels"].as_u64(), Some(1));
    assert_eq!(receipt["relations"]["dependencies"].as_u64(), Some(1));
    assert_eq!(receipt["relations"]["comments"].as_u64(), Some(1));

    // New row landed with relations; blocked cache sees the dependency.
    let show_new = run_br(&ws, ["show", "br-relnew1", "--json"], "rel_show_new");
    assert!(show_new.status.success(), "new row must exist");
    assert!(show_new.stdout.contains("fromjsonl"), "label imported");
    assert!(
        show_new.stdout.contains("imported comment"),
        "comment imported"
    );
    let blocked = run_br(&ws, ["blocked", "--json"], "rel_blocked");
    assert!(
        blocked.stdout.contains("br-relnew1"),
        "new row should be blocked by the anchor dependency: {}",
        blocked.stdout
    );

    // The db-only keeper kept every unsuperseded relation.
    let show_keeper = run_br(&ws, ["show", &keeper, "--json"], "rel_show_keeper");
    assert!(show_keeper.status.success(), "keeper must survive");
    assert!(
        show_keeper.stdout.contains("keeper comment"),
        "keeper comment kept"
    );
    assert!(
        show_keeper.stdout.contains("keeplabel"),
        "keeper label kept"
    );
    assert!(
        receipt["apply"]["needs_flush_set"].as_bool() == Some(true),
        "db-only rows must mark the DB for flush: {receipt}"
    );
}

#[test]
fn dangling_dependency_on_created_row_is_cleaned_scoped() {
    let ws = BrWorkspace::new();
    init_workspace(&ws, "orph");
    let anchor = create_issue(&ws, "Anchor for orphan test", "orph_anchor");
    let flush = run_br(&ws, ["sync", "--flush-only", "--json"], "orph_flush");
    assert!(flush.status.success());

    let lines = read_jsonl_lines(&ws);
    let template = lines[0].clone();
    let mut new_row: Value = serde_json::from_str(&clone_row(
        &template,
        "br-orphn1",
        "Row with dangling dep",
        "2026-02-01T00:00:00Z",
        "2026-02-01T00:00:00Z",
    ))
    .expect("row");
    new_row["dependencies"] = json!([
        {
            "issue_id": "br-orphn1",
            "depends_on_id": "br-doesnotexist",
            "type": "blocks",
            "created_at": "2026-02-01T00:00:00Z"
        },
        {
            "issue_id": "br-orphn1",
            "depends_on_id": anchor,
            "type": "blocks",
            "created_at": "2026-02-01T00:00:00Z"
        }
    ]);
    let mut all = lines;
    all.push(serde_json::to_string(&new_row).expect("serialize"));
    write_jsonl_lines(&ws, &all);

    let receipt = reconcile_receipt(&ws, false, "orph_apply");
    assert_eq!(plan_count(&receipt, "created"), 1);
    assert_eq!(
        receipt["apply"]["orphan_dependencies_cleaned"].as_u64(),
        Some(1),
        "exactly the dangling edge must be cleaned: {receipt}"
    );

    // The valid edge survived, the dangling one is gone.
    let deps = run_br(&ws, ["dep", "list", "br-orphn1", "--json"], "orph_deps");
    assert!(deps.stdout.contains(&anchor), "valid dependency kept");
    assert!(
        !deps.stdout.contains("br-doesnotexist"),
        "dangling dependency must be cleaned: {}",
        deps.stdout
    );

    // Doctor-grade integrity: a follow-up mutating command works fine.
    let touch = run_br(
        &ws,
        ["update", "br-orphn1", "--priority", "1", "--json"],
        "orph_touch",
    );
    assert!(
        touch.status.success(),
        "post-reconcile mutation failed: {}",
        touch.stderr
    );
}

#[test]
fn parent_child_rows_import_and_counters_rebuild() {
    let ws = BrWorkspace::new();
    init_workspace(&ws, "pc");
    let parent = create_issue(&ws, "Parent epic row", "pc_parent");
    let flush = run_br(&ws, ["sync", "--flush-only", "--json"], "pc_flush");
    assert!(flush.status.success());

    let lines = read_jsonl_lines(&ws);
    let template = lines[0].clone();
    let mut child: Value = serde_json::from_str(&clone_row(
        &template,
        "br-pcchild1",
        "Child under parent",
        "2026-02-01T00:00:00Z",
        "2026-02-01T00:00:00Z",
    ))
    .expect("row");
    child["dependencies"] = json!([{
        "issue_id": "br-pcchild1",
        "depends_on_id": parent,
        "type": "parent-child",
        "created_at": "2026-02-01T00:00:00Z"
    }]);
    let mut all = lines;
    all.push(serde_json::to_string(&child).expect("serialize"));
    write_jsonl_lines(&ws, &all);

    let receipt = reconcile_receipt(&ws, false, "pc_apply");
    assert_eq!(plan_count(&receipt, "created"), 1);
    assert!(
        receipt["apply"]["child_counter_entries"].as_u64().is_some(),
        "child counters must be rebuilt: {receipt}"
    );

    // `dep tree`/`dep list` walk what an issue depends ON, so inspect the
    // child (which carries the parent-child edge to its parent).
    let deps = run_br(&ws, ["dep", "list", "br-pcchild1", "--json"], "pc_deps");
    assert!(
        deps.stdout.contains(&parent) && deps.stdout.contains("parent-child"),
        "parent-child edge must be visible from the child: {}",
        deps.stdout
    );
}

// ============================================================================
// Malformed input
// ============================================================================

#[test]
fn malformed_jsonl_conflict_markers_and_duplicates_reject_cleanly() {
    let ws = BrWorkspace::new();
    init_workspace(&ws, "bad");
    let _ = create_issue(&ws, "Healthy issue", "bad_a");
    let flush = run_br(&ws, ["sync", "--flush-only", "--json"], "bad_flush");
    assert!(flush.status.success());
    let good_lines = read_jsonl_lines(&ws);
    let before = hash_files_under(&beads_dir(&ws));

    // Malformed JSON.
    let mut broken = good_lines.clone();
    broken.push("{not valid json".to_string());
    write_jsonl_lines(&ws, &broken);
    for dry in [true, false] {
        let mut args = vec!["sync", "--reconcile", "--json"];
        if dry {
            args.push("--dry-run");
        }
        let run = run_br(&ws, args, &format!("bad_json_dry_{dry}"));
        assert!(
            !run.status.success(),
            "malformed JSONL must fail (dry={dry})"
        );
        let combined = format!("{}{}", run.stdout, run.stderr);
        assert!(
            combined.contains("Invalid JSON"),
            "error should name the parse failure: {combined}"
        );
    }

    // Conflict markers.
    let mut conflicted = good_lines.clone();
    conflicted.push("<<<<<<< HEAD".to_string());
    write_jsonl_lines(&ws, &conflicted);
    let run = run_br(&ws, ["sync", "--reconcile", "--json"], "bad_conflict");
    assert!(!run.status.success(), "conflict markers must fail");
    assert!(
        format!("{}{}", run.stdout, run.stderr)
            .to_lowercase()
            .contains("conflict"),
        "error should name the conflict markers: {}",
        run.stderr
    );

    // Duplicate ids.
    let mut duplicated = good_lines.clone();
    duplicated.push(good_lines[0].clone());
    write_jsonl_lines(&ws, &duplicated);
    let run = run_br(&ws, ["sync", "--reconcile", "--json"], "bad_dupe");
    assert!(!run.status.success(), "duplicate ids must fail");
    assert!(
        format!("{}{}", run.stdout, run.stderr).contains("Duplicate issue id"),
        "error should name the duplicate: {}{}",
        run.stdout,
        run.stderr
    );

    // The issue data never changed across any of the failures. Workspace
    // bookkeeping (last-touched, lock files, fsqlite namespace sidecars) is
    // touched by every storage open and is not issue data.
    write_jsonl_lines(&ws, &good_lines);
    let after = hash_files_under(&beads_dir(&ws));
    let changed: Vec<&String> = before
        .iter()
        .filter(|(k, v)| after.get(*k) != Some(*v))
        .map(|(k, _)| k)
        .filter(|k| {
            !k.ends_with("issues.jsonl")
                && !k.ends_with("last-touched")
                && Path::new(k.as_str()).extension() != Some(std::ffi::OsStr::new("lock"))
                && !k.contains("-fsqlite-ns-")
        })
        .collect();
    assert!(
        changed.is_empty(),
        "failed reconciles must leave the DB family byte-identical; changed: {changed:?}"
    );
}

#[test]
fn missing_base_snapshot_is_irrelevant() {
    let ws = BrWorkspace::new();
    build_false_equal_workspace(&ws, "nobase");
    let base = beads_dir(&ws).join("beads.base.jsonl");
    if base.exists() {
        fs::remove_file(&base).expect("remove base snapshot");
    }

    let receipt = reconcile_receipt(&ws, false, "nobase_apply");
    assert_eq!(plan_count(&receipt, "created"), 2);
    assert_eq!(issue_count(&ws, "nobase_count"), 5);
    assert!(!base.exists(), "reconcile must not create a base snapshot");
}

// ============================================================================
// Path policy and file-safety
// ============================================================================

#[test]
fn external_jsonl_requires_explicit_opt_in() {
    let ws = BrWorkspace::new();
    init_workspace(&ws, "ext");
    let _ = create_issue(&ws, "External path issue", "ext_a");
    let flush = run_br(&ws, ["sync", "--flush-only", "--json"], "ext_flush");
    assert!(flush.status.success());

    let external = ws.root.join("external_issues.jsonl");
    fs::copy(jsonl_path(&ws), &external).expect("copy jsonl");

    let denied = run_br_with_env(
        &ws,
        ["sync", "--reconcile", "--dry-run", "--json"],
        [("BEADS_JSONL", external.to_string_lossy().to_string())],
        "ext_denied",
    );
    assert!(
        !denied.status.success(),
        "external JSONL without opt-in must be rejected: {}",
        denied.stdout
    );

    let allowed = run_br_with_env(
        &ws,
        [
            "sync",
            "--reconcile",
            "--dry-run",
            "--json",
            "--allow-external-jsonl",
        ],
        [("BEADS_JSONL", external.to_string_lossy().to_string())],
        "ext_allowed",
    );
    assert!(
        allowed.status.success(),
        "external JSONL with opt-in must plan: {}",
        allowed.stderr
    );
}

#[test]
// Restoring the fixture file's original writable bits after the read-only
// probe is exactly the case this lint warns about; the file is a temp
// fixture, not a shared resource.
#[allow(clippy::permissions_set_readonly_false)]
fn read_only_jsonl_applies_fine_because_reconcile_never_writes_it() {
    let ws = BrWorkspace::new();
    build_false_equal_workspace(&ws, "rojsonl");
    let jsonl = jsonl_path(&ws);
    let mut perms = fs::metadata(&jsonl).expect("stat").permissions();
    perms.set_readonly(true);
    fs::set_permissions(&jsonl, perms).expect("chmod");

    let receipt = reconcile_receipt(&ws, false, "rojsonl_apply");
    assert_eq!(plan_count(&receipt, "created"), 2);
    assert_eq!(issue_count(&ws, "rojsonl_count"), 5);

    let mut restore = fs::metadata(&jsonl).expect("stat").permissions();
    restore.set_readonly(false);
    let _ = fs::set_permissions(&jsonl, restore);
}

#[test]
fn apply_touches_only_the_db_family() {
    let ws = BrWorkspace::new();
    build_false_equal_workspace(&ws, "allow");
    let before = hash_files_under(&ws.root);

    let _ = reconcile_receipt(&ws, false, "allow_apply");

    let after = hash_files_under(&ws.root);
    let mut touched: Vec<String> = Vec::new();
    for (path, hash) in &after {
        if before.get(path) != Some(hash) {
            touched.push(path.clone());
        }
    }
    for path in before.keys() {
        if !after.contains_key(path) {
            touched.push(format!("(deleted) {path}"));
        }
    }
    let disallowed: Vec<&String> = touched
        .iter()
        .filter(|p| {
            let name = Path::new(p.as_str())
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            // Only the SQLite DB family may change; the write lock and
            // last-touched marker are workspace bookkeeping shared by every
            // storage-opening command, and logs are harness-owned.
            let db_family = name == "beads.db"
                || name.starts_with("beads.db-")
                || name.ends_with("-wal")
                || name.ends_with("-shm")
                || name.ends_with("-fsqlite-ns-use")
                || name.ends_with("-fsqlite-ns-gate");
            let bookkeeping = name == ".write.lock" || name == "last-touched";
            let harness_log = p.starts_with("logs/");
            !(db_family || bookkeeping || harness_log)
        })
        .collect();
    assert!(
        disallowed.is_empty(),
        "reconcile apply touched files outside the DB family: {disallowed:?}"
    );
}

// ============================================================================
// Concurrency: witnesses, rollback, locks
// ============================================================================

#[test]
fn lib_apply_rolls_back_when_db_changed_after_planning() {
    let ws = BrWorkspace::new();
    let (jsonl_only_ids, newer_shared_id) = build_false_equal_workspace(&ws, "racedb");
    let config = reconcile_import_config(&ws);

    let plan = {
        let storage = SqliteStorage::open(&db_path(&ws)).expect("open");
        plan_sync_reconcile(&storage, &jsonl_path(&ws), &config).expect("plan")
    };
    assert_eq!(plan.actions.len(), 5);

    // Concurrent DB change on a row the plan classified SkipEqual: the local
    // edit makes the DB copy strictly newer, so the same row now classifies
    // SkipOlder and the apply-time re-classification must diverge. (Racing
    // the Update row would NOT diverge: its 2030 JSONL timestamp still wins,
    // which is the same outcome a fresh plan would produce.)
    let equal_shared_id = read_jsonl_lines(&ws)
        .iter()
        .map(|l| row_id(l))
        .find(|id| *id != newer_shared_id && !jsonl_only_ids.contains(id))
        .expect("an equal-classified shared row");
    let touch = run_br(
        &ws,
        [
            "update",
            &equal_shared_id,
            "--priority",
            "0",
            "--no-auto-import",
            "--no-auto-flush",
            "--json",
        ],
        "racedb_touch",
    );
    assert!(
        touch.status.success(),
        "racing update failed: {}",
        touch.stderr
    );

    let issues_before = issue_count(&ws, "racedb_before");
    let events_before = events_witness(&ws);

    let mut storage = SqliteStorage::open(&db_path(&ws)).expect("open");
    let err = apply_sync_reconcile(&mut storage, &jsonl_path(&ws), &config, &plan)
        .expect_err("apply must refuse a stale plan");
    let msg = err.to_string();
    assert!(
        msg.contains("changed") || msg.contains("events changed"),
        "error should describe the divergence: {msg}"
    );
    drop(storage);

    assert_eq!(
        issue_count(&ws, "racedb_after"),
        issues_before,
        "rolled-back apply must not add issues"
    );
    assert_eq!(events_before, events_witness(&ws), "events unchanged");
}

#[test]
fn lib_apply_rolls_back_when_jsonl_changed_after_planning() {
    let ws = BrWorkspace::new();
    build_false_equal_workspace(&ws, "racejsonl");
    let config = reconcile_import_config(&ws);

    let plan = {
        let storage = SqliteStorage::open(&db_path(&ws)).expect("open");
        plan_sync_reconcile(&storage, &jsonl_path(&ws), &config).expect("plan")
    };

    // Concurrent JSONL change after planning.
    let mut lines = read_jsonl_lines(&ws);
    let template = lines[0].clone();
    lines.push(clone_row(
        &template,
        "br-racenew",
        "Row added after planning",
        "2033-01-01T00:00:00Z",
        "2033-01-01T00:00:00Z",
    ));
    write_jsonl_lines(&ws, &lines);

    let issues_before = issue_count(&ws, "racejsonl_before");
    let mut storage = SqliteStorage::open(&db_path(&ws)).expect("open");
    let err = apply_sync_reconcile(&mut storage, &jsonl_path(&ws), &config, &plan)
        .expect_err("apply must refuse a changed JSONL");
    assert!(
        err.to_string().contains("changed since the reconcile plan"),
        "error should describe the JSONL drift: {err}"
    );
    drop(storage);
    assert_eq!(issue_count(&ws, "racejsonl_after"), issues_before);
}

#[test]
fn apply_fails_under_lock_contention_but_fast_dry_run_proceeds() {
    let ws = BrWorkspace::new();
    build_false_equal_workspace(&ws, "lock");

    let lock_path = beads_dir(&ws).join(".write.lock");
    let lock_file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .expect("open .write.lock");
    lock_file.lock().expect("hold .write.lock");

    // Apply needs the write lock: with a short timeout it must fail cleanly.
    let blocked = run_br(
        &ws,
        ["--lock-timeout", "300", "sync", "--reconcile", "--json"],
        "lock_blocked",
    );
    assert!(
        !blocked.status.success(),
        "apply must fail while the write lock is held: {}",
        blocked.stdout
    );
    assert_eq!(
        issue_count_unlocked(&ws),
        3,
        "no rows applied under contention"
    );

    // Dry-run through the read-only fast path takes no lock and succeeds.
    let dry = run_br(
        &ws,
        [
            "sync",
            "--reconcile",
            "--dry-run",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "lock_dry",
    );
    assert!(
        dry.status.success(),
        "read-only dry-run must not need the write lock: {}",
        dry.stderr
    );
    let receipt = parse_json_value(&dry.stdout);
    assert_eq!(plan_count(&receipt, "created"), 2);

    drop(lock_file);

    // After release, apply succeeds.
    let receipt = reconcile_receipt(&ws, false, "lock_apply");
    assert_eq!(plan_count(&receipt, "created"), 2);
}

/// Count issues via the lib (no CLI invocation) so lock-holding tests can
/// assert row counts without deadlocking on their own lock.
fn issue_count_unlocked(ws: &BrWorkspace) -> usize {
    let storage = SqliteStorage::open(&db_path(ws)).expect("open");
    storage.count_all_issues().expect("count")
}

// ============================================================================
// Empty inputs
// ============================================================================

#[test]
fn empty_jsonl_and_empty_db_edge_cases() {
    // Both empty: zero-change plan, apply succeeds and repairs metadata.
    let ws = BrWorkspace::new();
    init_workspace(&ws, "empty");
    fs::write(jsonl_path(&ws), "").expect("write empty jsonl");

    let receipt = reconcile_receipt(&ws, true, "empty_dry");
    assert_eq!(plan_count(&receipt, "created"), 0);
    assert_eq!(plan_count(&receipt, "db_only"), 0);
    let receipt = reconcile_receipt(&ws, false, "empty_apply");
    assert_eq!(receipt["apply"]["metadata_repaired"].as_bool(), Some(true));

    // Empty JSONL + populated DB: nothing created, everything db-only, and
    // the needs_flush repair lets a follow-up flush restore the JSONL.
    let ws2 = BrWorkspace::new();
    init_workspace(&ws2, "empty2");
    let _ = create_issue(&ws2, "Survivor row", "empty2_a");
    let flush = run_br(&ws2, ["sync", "--flush-only", "--json"], "empty2_flush");
    assert!(flush.status.success());
    fs::write(jsonl_path(&ws2), "").expect("truncate jsonl");

    let receipt = reconcile_receipt(&ws2, false, "empty2_apply");
    assert_eq!(plan_count(&receipt, "created"), 0);
    assert_eq!(plan_count(&receipt, "deleted"), 0);
    assert_eq!(plan_count(&receipt, "db_only"), 1);
    assert_eq!(receipt["apply"]["needs_flush_set"].as_bool(), Some(true));
    assert_eq!(
        issue_count(&ws2, "empty2_count"),
        1,
        "reconcile never deletes"
    );

    // The flush marker means the next explicit flush restores the export.
    let reflush = run_br(&ws2, ["sync", "--flush-only", "--json"], "empty2_reflush");
    assert!(
        reflush.status.success(),
        "reflush failed: {}",
        reflush.stderr
    );
    let restored = read_jsonl_lines(&ws2);
    assert_eq!(restored.len(), 1, "flush must restore the db-only row");
}

// ============================================================================
// Schema registration
// ============================================================================

#[test]
fn reconcile_receipt_schema_is_registered() {
    let ws = BrWorkspace::new();
    let run = run_br(&ws, ["schema", "all", "--format", "json"], "schema_all");
    assert!(run.status.success(), "schema all failed: {}", run.stderr);
    let json = parse_json_value(&run.stdout);
    assert!(
        json["schemas"]["SyncReconcileReceipt"].is_object(),
        "SyncReconcileReceipt must be in the schema catalog"
    );
}

// ============================================================================
// Scale: the CASS-shaped fixture and a 2K+ bulk run
// ============================================================================

/// Deterministic synthetic issue row for bulk fixtures.
fn synthetic_row(template: &str, index: usize) -> String {
    let ts = format!(
        "2026-01-01T{:02}:{:02}:{:02}Z",
        index / 3600 % 24,
        index / 60 % 60,
        index % 60
    );
    clone_row(
        template,
        &format!("br-syn{index:05}"),
        &format!("Synthetic issue {index:05}"),
        &ts,
        &ts,
    )
}

/// The exact CASS-tracker shape from beads_rust-3r45: DB holds 1,732 issues
/// and 315 audit events; the canonical JSONL holds 1,915 issues — 183
/// JSONL-only rows plus 5 shared rows that are strictly newer — and the
/// stored content hash matches the file byte-for-byte (false-equal).
///
/// Dry-run must report created=183, updated=5, events 315→315 and touch
/// nothing; apply must produce 1,915 issues, keep all 315 events exactly,
/// write no JSONL, and a second dry-run must be a zero-change no-op.
#[test]
fn cass_shaped_fixture_recovers_exactly() {
    const SHARED: usize = 1_732;
    const JSONL_ONLY: usize = 183;
    const NEWER_SHARED: usize = 5;
    const TARGET_EVENTS: u64 = 315;

    let ws = BrWorkspace::new();
    init_workspace(&ws, "cass");
    let seed = create_issue(&ws, "Template seed", "cass_seed");
    let flush = run_br(&ws, ["sync", "--flush-only", "--json"], "cass_flush0");
    assert!(flush.status.success());
    let template = read_jsonl_lines(&ws)[0].clone();

    // Import the 1,732 shared rows (minus the seed, which is row zero).
    let mut shared_rows: Vec<String> = vec![template.clone()];
    for i in 1..SHARED {
        shared_rows.push(synthetic_row(&template, i));
    }
    write_jsonl_lines(&ws, &shared_rows);
    let import = run_br(&ws, ["sync", "--import-only", "--json"], "cass_import");
    assert!(
        import.status.success(),
        "bulk import failed: {}",
        import.stderr
    );
    assert_eq!(issue_count(&ws, "cass_count0"), SHARED);

    // Plant exactly 315 audit events via real mutations (priority toggles on
    // the seed row; each writes at least one event, so top off one op at a
    // time until the witness hits the target exactly).
    let mut planted = events_witness(&ws).0;
    let mut toggle = 1u8;
    let mut guard = 0usize;
    while planted < TARGET_EVENTS {
        guard += 1;
        assert!(guard <= 2_000, "event planting did not converge");
        let run = run_br(
            &ws,
            [
                "update",
                &seed,
                "--priority",
                if toggle == 1 { "1" } else { "2" },
                "--no-auto-flush",
                "--json",
            ],
            &format!("cass_event_{guard}"),
        );
        assert!(
            run.status.success(),
            "event mutation failed: {}",
            run.stderr
        );
        toggle ^= 3; // 1 <-> 2
        planted = events_witness(&ws).0;
    }
    assert_eq!(
        planted, TARGET_EVENTS,
        "fixture must hold exactly 315 events"
    );

    // Flush so the JSONL reflects the DB, then build the canonical 1,915-row
    // file: all shared rows (5 of them strictly newer) + 183 JSONL-only rows.
    let flush = run_br(&ws, ["sync", "--flush-only", "--json"], "cass_flush1");
    assert!(flush.status.success());
    let mut lines = read_jsonl_lines(&ws);
    assert_eq!(lines.len(), SHARED);
    for line in lines.iter_mut().take(NEWER_SHARED) {
        *line = set_row_field(line, "updated_at", json!("2034-01-01T00:00:00Z"));
        *line = set_row_field(line, "description", json!("recovered newer body"));
    }
    for i in 0..JSONL_ONLY {
        lines.push(clone_row(
            &template,
            &format!("br-cassx{i:04}"),
            &format!("CASS jsonl-only row {i:04}"),
            "2034-02-01T00:00:00Z",
            "2034-02-01T00:00:00Z",
        ));
    }
    write_jsonl_lines(&ws, &lines);
    plant_false_equal_metadata(&ws);

    // Note: a plain `--import-only` would no longer be blind here — the
    // `beads_rust-jdmh` coverage invariant rejects the uncovered hash match
    // and heals the state — so this fixture goes straight to reconcile to
    // keep the divergence intact for the receipt assertions.

    // Dry-run: exact counts, zero mutation.
    let before_hashes = hash_files_under(&beads_dir(&ws));
    let receipt = reconcile_receipt(&ws, true, "cass_dry");
    assert_eq!(plan_count(&receipt, "created"), JSONL_ONLY as u64);
    assert_eq!(plan_count(&receipt, "updated"), NEWER_SHARED as u64);
    assert_eq!(plan_count(&receipt, "deleted"), 0);
    assert_eq!(receipt["events_before"].as_u64(), Some(TARGET_EVENTS));
    assert_eq!(receipt["events_after"].as_u64(), Some(TARGET_EVENTS));
    assert_eq!(
        receipt["target"]["stored_hash_matches_jsonl"].as_bool(),
        Some(true)
    );
    assert_eq!(
        before_hashes,
        hash_files_under(&beads_dir(&ws)),
        "dry-run must leave every .beads file byte-identical"
    );

    // Apply: full recovery, events preserved exactly, JSONL untouched.
    let jsonl_before = fs::read(jsonl_path(&ws)).expect("jsonl bytes");
    let events_dump_before = all_events_dump(&ws);
    let receipt = reconcile_receipt(&ws, false, "cass_apply");
    assert_eq!(plan_count(&receipt, "created"), JSONL_ONLY as u64);
    assert_eq!(plan_count(&receipt, "updated"), NEWER_SHARED as u64);
    assert_eq!(receipt["events_after"].as_u64(), Some(TARGET_EVENTS));

    assert_eq!(issue_count(&ws, "cass_final"), SHARED + JSONL_ONLY);
    assert_eq!(events_witness(&ws).0, TARGET_EVENTS);
    assert_eq!(
        events_dump_before,
        all_events_dump(&ws),
        "all 315 events must survive apply byte-for-byte"
    );
    assert_eq!(
        jsonl_before,
        fs::read(jsonl_path(&ws)).expect("jsonl bytes"),
        "apply must not write the JSONL"
    );

    // Second dry-run: zero-change no-op.
    let second = reconcile_receipt(&ws, true, "cass_dry2");
    assert_eq!(plan_count(&second, "created"), 0);
    assert_eq!(plan_count(&second, "updated"), 0);
}

#[test]
fn bulk_two_thousand_issue_input() {
    const TOTAL: usize = 2_200;
    const SECOND_PASS_NEWER: usize = 50;

    let ws = BrWorkspace::new();
    init_workspace(&ws, "bulk");
    let _ = create_issue(&ws, "Bulk template seed", "bulk_seed");
    let flush = run_br(&ws, ["sync", "--flush-only", "--json"], "bulk_flush");
    assert!(flush.status.success());
    let template = read_jsonl_lines(&ws)[0].clone();

    let mut lines: Vec<String> = vec![template.clone()];
    for i in 1..TOTAL {
        lines.push(synthetic_row(&template, i));
    }
    write_jsonl_lines(&ws, &lines);

    let receipt = reconcile_receipt(&ws, false, "bulk_apply");
    assert_eq!(plan_count(&receipt, "created"), (TOTAL - 1) as u64);
    assert_eq!(issue_count(&ws, "bulk_count"), TOTAL);

    // Second pass: bump a slice to newer timestamps → pure updates.
    let mut lines = read_jsonl_lines(&ws);
    for line in lines.iter_mut().skip(1).take(SECOND_PASS_NEWER) {
        *line = set_row_field(line, "updated_at", json!("2035-01-01T00:00:00Z"));
    }
    write_jsonl_lines(&ws, &lines);
    let receipt = reconcile_receipt(&ws, false, "bulk_apply2");
    assert_eq!(plan_count(&receipt, "created"), 0);
    assert_eq!(plan_count(&receipt, "updated"), SECOND_PASS_NEWER as u64);
    assert_eq!(issue_count(&ws, "bulk_count2"), TOTAL);
}

// ============================================================================
// Ordinary import preserves exported close-policy audit evidence
// ============================================================================

fn init_import_audit_workspace(ws: &BrWorkspace) {
    let init = run_br(ws, ["init", "--prefix", "audit"], "audit_init");
    assert!(init.status.success(), "{init:?}");
}

fn import_audit_rows(ws: &BrWorkspace) -> BTreeMap<String, Value> {
    read_jsonl_lines(ws)
        .iter()
        .map(|line| {
            let row: Value = serde_json::from_str(line).expect("audit source row");
            (row["id"].as_str().expect("audit issue ID").to_string(), row)
        })
        .collect()
}

fn write_import_audit_rows(ws: &BrWorkspace, rows: &BTreeMap<String, Value>) {
    write_jsonl_lines(
        ws,
        &rows
            .values()
            .map(|row| serde_json::to_string(row).expect("serialize audit source"))
            .collect::<Vec<_>>(),
    );
}

fn import_audit_issue(ws: &BrWorkspace, id: &str) -> Issue {
    SqliteStorage::open(&db_path(ws))
        .expect("open audit database")
        .get_issues_for_export(&[id.to_string()])
        .expect("hydrate issue and exported audit")
        .pop()
        .expect("audit issue exists")
}

fn ordinary_audit_import(ws: &BrWorkspace, label: &str) -> Value {
    let import = run_br(
        ws,
        ["--no-auto-flush", "sync", "--import-only", "--json"],
        label,
    );
    assert!(
        import.status.success(),
        "ordinary import failed: {import:?}"
    );
    parse_json_value(&import.stdout)
}

fn assert_import_audit_export(ws: &BrWorkspace, expected: &Issue) {
    let mut expected = expected.clone();
    expected.source_repo_path = None;
    assert_eq!(
        import_audit_rows(ws).get(&expected.id),
        Some(&serde_json::to_value(&expected).expect("expected exported issue")),
        "the complete issue and close audit must survive ordinary export"
    );
}

fn add_source_bypass_audit(row: &mut Value) {
    row["bypassed_policy"] = json!(true);
    row["bypass_reason"] = json!("Peer's reviewed waiver");
    row["policy_gates_fired"] = json!(["acceptance_criteria", "security_review"]);
}

#[test]
fn ordinary_import_persists_new_close_audit_through_later_mutation_and_export() {
    let producer = BrWorkspace::new();
    init_import_audit_workspace(&producer);
    let audited = create_issue(&producer, "Imported audited closure", "audit_create");
    let minimal = create_issue(
        &producer,
        "Audit with optional fields absent",
        "audit_minimal",
    );
    let sibling = create_issue(&producer, "Unaudited sibling", "audit_sibling");
    for id in [&audited, &minimal] {
        let closed = run_br(&producer, ["close", id], &format!("audit_close_{id}"));
        assert!(closed.status.success(), "{closed:?}");
    }
    let mut rows = import_audit_rows(&producer);
    add_source_bypass_audit(rows.get_mut(&audited).expect("audited source"));
    rows.get_mut(&minimal).expect("minimal source")["bypassed_policy"] = json!(true);

    let consumer = BrWorkspace::new();
    init_import_audit_workspace(&consumer);
    assert_eq!(issue_count(&consumer, "audit_empty"), 0);
    let events = all_events_dump(&consumer);
    write_import_audit_rows(&consumer, &rows);
    let source = fs::read(jsonl_path(&consumer)).expect("audit source bytes");
    let receipt = ordinary_audit_import(&consumer, "audit_import_fresh");
    assert_eq!(receipt["created"], 3);
    assert_eq!(receipt["updated"], 0);
    assert_eq!(
        all_events_dump(&consumer),
        events,
        "import invents no events"
    );
    assert_eq!(fs::read(jsonl_path(&consumer)).unwrap(), source);

    let storage = SqliteStorage::open(&db_path(&consumer)).unwrap();
    let audit = storage.get_close_metadata(&audited).unwrap().unwrap();
    assert!(audit.bypassed_policy);
    assert_eq!(
        audit.bypass_reason.as_deref(),
        Some("Peer's reviewed waiver")
    );
    assert_eq!(
        audit.policy_gates_fired,
        vec![
            "acceptance_criteria".to_string(),
            "security_review".to_string()
        ]
    );
    assert!(storage.get_close_metadata(&minimal).unwrap().is_some());
    assert!(storage.get_close_metadata(&sibling).unwrap().is_none());
    assert_eq!(storage.get_dirty_issue_count().unwrap(), 0);
    assert_eq!(
        storage.get_metadata("needs_flush").unwrap().as_deref(),
        Some("false")
    );
    for id in [&audited, &minimal, &sibling] {
        assert!(storage.get_export_hash(id).unwrap().is_some());
    }
    drop(storage);
    let minimal_before = import_audit_issue(&consumer, &minimal);
    assert_eq!(minimal_before.bypassed_policy, Some(true));
    assert_eq!(minimal_before.bypass_reason, None);
    assert_eq!(minimal_before.policy_gates_fired, None);

    let updated = run_br(
        &consumer,
        ["update", &audited, "--priority", "1"],
        "audit_update_after_import",
    );
    assert!(updated.status.success(), "{updated:?}");
    let storage = SqliteStorage::open(&db_path(&consumer)).unwrap();
    assert_eq!(storage.get_close_metadata(&audited).unwrap(), Some(audit));
    assert!(storage.get_close_metadata(&sibling).unwrap().is_none());
    assert_eq!(storage.get_dirty_issue_count().unwrap(), 0);
    assert_eq!(
        storage.get_metadata("needs_flush").unwrap().as_deref(),
        Some("false")
    );
    drop(storage);
    assert_import_audit_export(&consumer, &import_audit_issue(&consumer, &audited));
    assert_import_audit_export(&consumer, &minimal_before);
    assert_import_audit_export(&consumer, &import_audit_issue(&consumer, &sibling));
}

#[test]
fn ordinary_import_adopts_matching_skipped_audit_without_churning_noops() {
    let ws = BrWorkspace::new();
    init_import_audit_workspace(&ws);
    let id = create_issue(&ws, "Matching imported audit", "audit_skip_create");
    let comment = run_br(
        &ws,
        ["comments", "add", &id, "Preserve discussion"],
        "audit_skip_comment",
    );
    assert!(comment.status.success(), "{comment:?}");
    let closed = run_br(&ws, ["close", &id], "audit_skip_close");
    assert!(closed.status.success(), "{closed:?}");
    let before = import_audit_issue(&ws, &id);
    assert!(
        SqliteStorage::open(&db_path(&ws))
            .unwrap()
            .get_close_metadata(&id)
            .unwrap()
            .is_none()
    );
    let events = all_events_dump(&ws);
    let mut rows = import_audit_rows(&ws);
    add_source_bypass_audit(rows.get_mut(&id).unwrap());
    write_import_audit_rows(&ws, &rows);
    let source = fs::read(jsonl_path(&ws)).unwrap();
    let receipt = ordinary_audit_import(&ws, "audit_skip_adopt");
    assert_eq!(receipt["created"], 0);
    assert_eq!(receipt["updated"], 0);
    assert_eq!(receipt["skipped"], 1);
    assert_eq!(fs::read(jsonl_path(&ws)).unwrap(), source);
    let mut expected = before;
    expected.bypassed_policy = Some(true);
    expected.bypass_reason = Some("Peer's reviewed waiver".to_string());
    expected.policy_gates_fired = Some(vec![
        "acceptance_criteria".to_string(),
        "security_review".to_string(),
    ]);
    assert_eq!(import_audit_issue(&ws, &id), expected);
    assert_eq!(all_events_dump(&ws), events);

    let storage = SqliteStorage::open(&db_path(&ws)).unwrap();
    let audit = storage
        .get_close_metadata(&id)
        .unwrap()
        .expect("adopted audit row");
    let certificate = storage
        .get_export_hash(&id)
        .unwrap()
        .expect("certified imported audit");
    assert_eq!(storage.get_dirty_issue_count().unwrap(), 0);
    assert_eq!(
        storage.get_metadata("needs_flush").unwrap().as_deref(),
        Some("false")
    );
    drop(storage);
    for pass in 0..2 {
        // Force the full ordinary-import skip path on both repeats, rather
        // than proving only the file-hash shortcut is a no-op.
        let lines = read_jsonl_lines(&ws)
            .iter()
            .map(|line| line.replacen('{', "{ ", 1))
            .collect::<Vec<_>>();
        write_jsonl_lines(&ws, &lines);
        let bytes = fs::read(jsonl_path(&ws)).unwrap();
        let receipt = ordinary_audit_import(&ws, &format!("audit_skip_repeat_{pass}"));
        assert_eq!(receipt["created"], 0);
        assert_eq!(receipt["updated"], 0);
        assert_eq!(receipt["skipped"], 1);
        let storage = SqliteStorage::open(&db_path(&ws)).unwrap();
        assert_eq!(
            storage.get_close_metadata(&id).unwrap(),
            Some(audit.clone())
        );
        assert_eq!(
            storage.get_export_hash(&id).unwrap(),
            Some(certificate.clone()),
            "no-op certification must retain exported_at"
        );
        assert_eq!(storage.get_dirty_issue_count().unwrap(), 0);
        assert_eq!(
            storage.get_metadata("needs_flush").unwrap().as_deref(),
            Some("false")
        );
        drop(storage);
        assert_eq!(import_audit_issue(&ws, &id), expected);
        assert_eq!(all_events_dump(&ws), events);
        assert_eq!(fs::read(jsonl_path(&ws)).unwrap(), bytes);
    }
}

#[test]
fn ordinary_import_preserves_authoritative_local_close_audit_and_republishes_it() {
    for source_case in [
        "missing",
        "different_reason",
        "different_gates",
        "local_false",
    ] {
        let ws = BrWorkspace::new();
        init_import_audit_workspace(&ws);
        let id = create_issue(&ws, "Authoritative local closure", "audit_local_create");
        let sibling = create_issue(&ws, "Trigger ordinary export", "audit_local_sibling");
        fs::write(
            beads_dir(&ws).join("policy.yaml"),
            "close_policy:\n  require_close_reason:\n    enabled: true\n    min_length: 40\n",
        )
        .unwrap();
        let close = if source_case == "local_false" {
            run_br(
                &ws,
                [
                    "close",
                    &id,
                    "--reason",
                    "Completed with the required local policy evidence and review",
                ],
                "audit_local_compliant",
            )
        } else {
            run_br(
                &ws,
                [
                    "close",
                    &id,
                    "--reason",
                    "done",
                    "--bypass-policy",
                    "--bypass-reason",
                    "Local reviewed exception",
                ],
                "audit_local_bypass",
            )
        };
        assert!(close.status.success(), "{close:?}");
        let storage = SqliteStorage::open(&db_path(&ws)).unwrap();
        let audit = storage
            .get_close_metadata(&id)
            .unwrap()
            .expect("real CLI close metadata");
        assert_eq!(audit.bypassed_policy, source_case != "local_false");
        assert_eq!(storage.get_dirty_issue_count().unwrap(), 0);
        assert!(storage.get_export_hash(&id).unwrap().is_some());
        drop(storage);
        let local = import_audit_issue(&ws, &id);
        let events = all_events_dump(&ws);
        let mut rows = import_audit_rows(&ws);
        let row = rows.get_mut(&id).unwrap();
        match source_case {
            "missing" => {
                for field in ["bypassed_policy", "bypass_reason", "policy_gates_fired"] {
                    row.as_object_mut().unwrap().remove(field);
                }
            }
            "different_reason" => row["bypass_reason"] = json!("Other machine's waiver"),
            "different_gates" => row["policy_gates_fired"] = json!([]),
            "local_false" => add_source_bypass_audit(row),
            _ => unreachable!(),
        }
        write_import_audit_rows(&ws, &rows);
        let source = fs::read(jsonl_path(&ws)).unwrap();
        let receipt = ordinary_audit_import(&ws, &format!("audit_local_import_{source_case}"));
        assert_eq!(receipt["created"], 0);
        assert_eq!(receipt["updated"], 0);
        assert_eq!(receipt["skipped"], 2);
        assert_eq!(import_audit_issue(&ws, &id), local);
        assert_eq!(all_events_dump(&ws), events);
        assert_eq!(fs::read(jsonl_path(&ws)).unwrap(), source);
        let storage = SqliteStorage::open(&db_path(&ws)).unwrap();
        assert_eq!(
            storage.get_close_metadata(&id).unwrap(),
            Some(audit.clone())
        );
        assert_eq!(storage.get_dirty_issue_count().unwrap(), 0);
        assert_eq!(
            storage.get_metadata("needs_flush").unwrap().as_deref(),
            Some("true")
        );
        assert!(
            storage.get_export_hash(&id).unwrap().is_none(),
            "audit mismatch must revoke certification"
        );
        assert!(storage.get_export_hash(&sibling).unwrap().is_some());
        drop(storage);

        let update = run_br(
            &ws,
            ["update", &sibling, "--priority", "1"],
            "audit_local_republish",
        );
        assert!(update.status.success(), "{update:?}");
        assert_import_audit_export(&ws, &local);
        let storage = SqliteStorage::open(&db_path(&ws)).unwrap();
        assert_eq!(storage.get_close_metadata(&id).unwrap(), Some(audit));
        assert_eq!(storage.get_dirty_issue_count().unwrap(), 0);
        assert_eq!(
            storage.get_metadata("needs_flush").unwrap().as_deref(),
            Some("false")
        );
        assert!(storage.get_export_hash(&id).unwrap().is_some());
    }
}

#[test]
fn ordinary_import_never_attaches_stale_close_audit_to_newer_local_state() {
    for local_change in ["reopened", "title", "comment"] {
        let ws = BrWorkspace::new();
        init_import_audit_workspace(&ws);
        let id = create_issue(&ws, "Closure before local changes", "audit_newer_create");
        let sibling = create_issue(&ws, "Publish preserved local state", "audit_newer_sibling");
        let closed = run_br(&ws, ["close", &id], "audit_newer_close");
        assert!(closed.status.success(), "{closed:?}");
        let mut restored = import_audit_rows(&ws);
        add_source_bypass_audit(restored.get_mut(&id).unwrap());
        let changed = match local_change {
            "reopened" => run_br(&ws, ["reopen", &id], "audit_newer_reopen"),
            "title" => run_br(
                &ws,
                ["update", &id, "--title", "Newer authoritative local title"],
                "audit_newer_title",
            ),
            "comment" => run_br(
                &ws,
                ["comments", "add", &id, "Newer local review discussion"],
                "audit_newer_comment",
            ),
            _ => unreachable!(),
        };
        assert!(changed.status.success(), "{changed:?}");
        let local = import_audit_issue(&ws, &id);
        let events = all_events_dump(&ws);
        write_import_audit_rows(&ws, &restored);
        let source = fs::read(jsonl_path(&ws)).unwrap();
        let receipt = ordinary_audit_import(&ws, &format!("audit_newer_import_{local_change}"));
        assert_eq!(receipt["created"], 0);
        assert_eq!(receipt["updated"], 0);
        assert_eq!(receipt["skipped"], 2);
        assert_eq!(import_audit_issue(&ws, &id), local);
        assert_eq!(all_events_dump(&ws), events);
        assert_eq!(fs::read(jsonl_path(&ws)).unwrap(), source);
        let storage = SqliteStorage::open(&db_path(&ws)).unwrap();
        assert!(storage.get_close_metadata(&id).unwrap().is_none());
        assert_eq!(storage.get_dirty_issue_count().unwrap(), 0);
        assert_eq!(
            storage.get_metadata("needs_flush").unwrap().as_deref(),
            Some("true")
        );
        assert!(storage.get_export_hash(&id).unwrap().is_none());
        drop(storage);
        let update = run_br(
            &ws,
            ["update", &sibling, "--priority", "1"],
            "audit_newer_republish",
        );
        assert!(update.status.success(), "{update:?}");
        assert_import_audit_export(&ws, &local);
        let storage = SqliteStorage::open(&db_path(&ws)).unwrap();
        assert!(storage.get_close_metadata(&id).unwrap().is_none());
        assert_eq!(storage.get_dirty_issue_count().unwrap(), 0);
        assert_eq!(
            storage.get_metadata("needs_flush").unwrap().as_deref(),
            Some("false")
        );
        assert!(storage.get_export_hash(&id).unwrap().is_some());
    }
}

// ============================================================================
// Retention-aware import certification and automatic export
// ============================================================================

fn set_workspace_retention_days(ws: &BrWorkspace, days: u64) {
    let path = beads_dir(ws).join("metadata.json");
    let mut metadata: Value =
        serde_json::from_slice(&fs::read(&path).expect("read workspace metadata"))
            .expect("parse workspace metadata");
    metadata["deletions_retention_days"] = json!(days);
    fs::write(
        path,
        serde_json::to_vec_pretty(&metadata).expect("serialize workspace metadata"),
    )
    .expect("set workspace retention policy");
}

fn stored_retention_issue(ws: &BrWorkspace, id: &str) -> Issue {
    SqliteStorage::open(&db_path(ws))
        .expect("open retention database")
        .get_issue_for_export(id)
        .expect("hydrate retention issue")
        .expect("retention issue exists")
}

fn delete_retention_fixture(
    ws: &BrWorkspace,
    id: &str,
    deleted_at: Option<chrono::DateTime<chrono::Utc>>,
    label: &str,
) -> Issue {
    // Close through the CLI first so a later no-op import has no unrelated
    // closed_at normalization to perform. Leave both transitions unflushed.
    let close = run_br(
        ws,
        [
            "--no-auto-import",
            "--no-auto-flush",
            "close",
            id,
            "--reason",
            "Retention fixture closure",
        ],
        &format!("{label}_close"),
    );
    assert!(close.status.success(), "fixture close failed: {close:?}");
    let delete = run_br(
        ws,
        [
            "--no-auto-import",
            "--no-auto-flush",
            "delete",
            id,
            "--force",
            "--reason",
            "Retention fixture deletion",
        ],
        &format!("{label}_delete"),
    );
    assert!(delete.status.success(), "fixture delete failed: {delete:?}");

    // Deletion dates have no CLI editing surface. Adjust only that persisted
    // field, preserving the real CLI mutation's dirty marker and event trail.
    let mut issue = stored_retention_issue(ws, id);
    assert_eq!(issue.status, Status::Tombstone);
    issue.deleted_at = deleted_at;
    SqliteStorage::open(&db_path(ws))
        .expect("open deletion-date fixture")
        .upsert_issue_for_import(&issue)
        .expect("set fixture deletion date");
    stored_retention_issue(ws, id)
}

fn assert_retention_export(ws: &BrWorkspace, expected_ids: &[&str], expired_id: &str) {
    let exported = read_jsonl_lines(ws)
        .iter()
        .map(|line| serde_json::from_str::<Issue>(line).expect("exported retention issue"))
        .collect::<Vec<_>>();
    let mut actual_ids = exported
        .iter()
        .map(|issue| issue.id.as_str())
        .collect::<Vec<_>>();
    actual_ids.sort_unstable();
    let mut expected_ids = expected_ids.to_vec();
    expected_ids.sort_unstable();
    assert_eq!(actual_ids, expected_ids, "exact retained JSONL population");

    let storage = SqliteStorage::open(&db_path(ws)).expect("open export certificate witness");
    assert_eq!(storage.get_dirty_issue_count().expect("dirty count"), 0);
    assert_eq!(
        storage
            .get_metadata("needs_flush")
            .expect("flush flag")
            .as_deref(),
        Some("false"),
        "intentional retention omissions must not leave export pending"
    );
    for issue in exported {
        let (hash, _) = storage
            .get_export_hash(&issue.id)
            .expect("read retained issue certificate")
            .expect("published issue must be certified");
        assert_eq!(hash, issue.compute_content_hash());
    }
    assert!(
        storage
            .get_export_hash(expired_id)
            .expect("read expired issue certificate")
            .is_none(),
        "an omitted expired tombstone must never be certified as published"
    );
}

fn assert_restored_tombstone_payload(ws: &BrWorkspace, expected: &Issue) {
    let published = read_jsonl_lines(ws)
        .iter()
        .map(|line| serde_json::from_str::<Value>(line).expect("published issue"))
        .find(|issue| issue["id"].as_str() == Some(expected.id.as_str()))
        .expect("retained tombstone restored to export");
    let mut expected_json = serde_json::to_value(expected).unwrap();
    expected_json
        .as_object_mut()
        .unwrap()
        .remove("source_repo_path");
    assert_eq!(published, expected_json, "complete tombstone payload");
    assert_eq!(stored_retention_issue(ws, &expected.id), *expected);
    let storage = SqliteStorage::open(&db_path(ws)).unwrap();
    assert_eq!(storage.get_dirty_issue_count().unwrap(), 0);
    assert_eq!(
        storage.get_metadata("needs_flush").unwrap().as_deref(),
        Some("false")
    );
    assert_eq!(
        storage.get_export_hash(&expected.id).unwrap().unwrap().0,
        expected.compute_content_hash()
    );
}

#[test]
fn clean_explicit_flush_applies_retention_contraction_and_expansion() {
    for expanded_days in [0, 90] {
        let ws = BrWorkspace::new();
        init_workspace(&ws, "clean_policy");
        let live_id = create_issue(&ws, "Live issue", "clean_policy_live");
        let closed = run_br(&ws, ["close", &live_id], "clean_policy_close");
        assert!(closed.status.success(), "{closed:?}");
        let tombstone_id = create_issue(&ws, "Retention history", "clean_policy_deleted");
        let comment = run_br(
            &ws,
            ["comments", "add", &tombstone_id, "Keep this history"],
            "clean_policy_comment",
        );
        assert!(comment.status.success(), "{comment:?}");
        let tombstone = delete_retention_fixture(
            &ws,
            &tombstone_id,
            Some(chrono::Utc::now() - chrono::Duration::days(60)),
            "clean_policy",
        );
        SqliteStorage::open(&db_path(&ws))
            .unwrap()
            .record_close_metadata(
                &live_id,
                &beads_rust::close_policy::AttributionValues::default(),
                true,
                Some("Retained audit evidence"),
                &["retention-proof".to_string()],
            )
            .unwrap();
        let initial = run_br(
            &ws,
            ["sync", "--flush-only", "--json"],
            "clean_policy_initial",
        );
        assert!(initial.status.success(), "{initial:?}");
        assert_restored_tombstone_payload(&ws, &tombstone);
        let audited = read_jsonl_lines(&ws)
            .iter()
            .map(|line| serde_json::from_str::<Issue>(line).unwrap())
            .find(|issue| issue.id == live_id)
            .unwrap();
        assert_eq!(audited.bypassed_policy, Some(true));
        assert_eq!(
            audited.bypass_reason.as_deref(),
            Some("Retained audit evidence")
        );
        assert_eq!(
            audited.policy_gates_fired,
            Some(vec!["retention-proof".to_string()])
        );
        let events = SqliteStorage::open(&db_path(&ws))
            .unwrap()
            .get_events(&tombstone_id, 100)
            .unwrap();

        set_workspace_retention_days(&ws, 30);
        let contracted = run_br(
            &ws,
            ["sync", "--flush-only", "--json"],
            "clean_policy_contract",
        );
        assert!(contracted.status.success(), "{contracted:?}");
        assert_eq!(parse_json_value(&contracted.stdout)["exported_issues"], 1);
        assert_retention_export(&ws, &[&live_id], &tombstone_id);

        set_workspace_retention_days(&ws, expanded_days);
        let expanded = run_br(
            &ws,
            ["sync", "--flush-only", "--json"],
            "clean_policy_expand",
        );
        assert!(expanded.status.success(), "{expanded:?}");
        assert_eq!(parse_json_value(&expanded.stdout)["exported_issues"], 2);
        assert_restored_tombstone_payload(&ws, &tombstone);
        assert_eq!(
            SqliteStorage::open(&db_path(&ws))
                .unwrap()
                .get_events(&tombstone_id, 100)
                .unwrap(),
            events
        );

        let bytes = fs::read(jsonl_path(&ws)).unwrap();
        let repeated = run_br(
            &ws,
            ["sync", "--flush-only", "--json"],
            "clean_policy_repeat",
        );
        assert!(repeated.status.success(), "{repeated:?}");
        assert_eq!(parse_json_value(&repeated.stdout)["exported_issues"], 0);
        assert_eq!(fs::read(jsonl_path(&ws)).unwrap(), bytes);
    }
}

#[test]
fn incremental_retention_expansion_restores_clean_tombstones_and_preserves_source() {
    for expanded_days in [0, 90] {
        let ws = BrWorkspace::new();
        init_workspace(&ws, "expand_policy");
        let dirty_id = create_issue(&ws, "Local edit", "expand_dirty");
        let remote_id = create_issue(&ws, "Remote history", "expand_remote");
        let tombstone_id = create_issue(&ws, "Retained history", "expand_deleted");
        let comment = run_br(
            &ws,
            ["comments", "add", &tombstone_id, "Retained comment"],
            "expand_comment",
        );
        assert!(comment.status.success(), "{comment:?}");
        let tombstone = delete_retention_fixture(
            &ws,
            &tombstone_id,
            Some(chrono::Utc::now() - chrono::Duration::days(60)),
            "expand_policy",
        );
        set_workspace_retention_days(&ws, 30);
        let initial = run_br(&ws, ["sync", "--flush-only", "--json"], "expand_initial");
        assert!(initial.status.success(), "{initial:?}");
        assert_retention_export(&ws, &[&dirty_id, &remote_id], &tombstone_id);
        let local_remote = stored_retention_issue(&ws, &remote_id);
        set_workspace_retention_days(&ws, expanded_days);
        let before = fs::read(jsonl_path(&ws)).unwrap();
        let read = run_br(&ws, ["count", "--json"], "expand_no_pending");
        assert!(read.status.success(), "{read:?}");
        assert_eq!(
            fs::read(jsonl_path(&ws)).unwrap(),
            before,
            "no pending automatic export stays a no-op"
        );

        let pending = run_br(
            &ws,
            [
                "--no-auto-flush",
                "update",
                &dirty_id,
                "--title",
                "Pending local edit",
            ],
            "expand_pending",
        );
        assert!(pending.status.success(), "{pending:?}");
        let mut lines = read_jsonl_lines(&ws);
        let remote_line = lines
            .iter_mut()
            .find(|line| serde_json::from_str::<Value>(line).unwrap()["id"] == remote_id)
            .unwrap();
        *remote_line = set_row_field(remote_line, "due_at", json!("2035-01-02T03:04:05Z"));
        *remote_line = set_row_field(remote_line, "updated_at", json!("2035-01-01T00:00:00Z"));
        let remote_line = remote_line.clone();
        write_jsonl_lines(&ws, &lines);
        assert_eq!(
            serde_json::from_str::<Issue>(&remote_line)
                .unwrap()
                .compute_content_hash(),
            local_remote.compute_content_hash(),
            "narrow hashes do not certify this source difference"
        );

        let updated = run_br(
            &ws,
            ["update", &dirty_id, "--title", "Published local edit"],
            "expand_flush",
        );
        assert!(updated.status.success(), "{updated:?}");
        let published = read_jsonl_lines(&ws);
        assert_eq!(published.len(), 3);
        assert!(
            published.contains(&remote_line),
            "untouched newer source payload must survive verbatim"
        );
        assert_restored_tombstone_payload(&ws, &tombstone);
        assert_eq!(stored_retention_issue(&ws, &remote_id), local_remote);
        assert_eq!(
            stored_retention_issue(&ws, &dirty_id).title,
            "Published local edit"
        );
    }
}

#[test]
fn clean_retention_flush_refuses_source_payload_preserved_by_incremental_export() {
    for (field, value) in [
        ("due_at", json!("2035-01-02T03:04:05Z")),
        // Import repairs this to None. A clean exporter must still certify
        // the captured source, rather than compare a repaired replacement.
        ("external_ref", json!("")),
        ("created_at", json!("2020-01-01T00:00:00Z")),
        ("updated_at", json!("2035-01-01T00:00:00Z")),
        ("bypassed_policy", json!(true)),
        ("bypass_reason", json!("Peer's retained audit reason")),
        ("policy_gates_fired", json!(["peer-gate"])),
    ] {
        let ws = BrWorkspace::new();
        init_workspace(&ws, "retention_guard");
        let dirty_id = create_issue(&ws, "Local edit", "guard_dirty");
        let remote_id = create_issue(&ws, "Remote history", "guard_remote");
        let tombstone_id = create_issue(&ws, "Expiring history", "guard_deleted");
        let tombstone = delete_retention_fixture(
            &ws,
            &tombstone_id,
            Some(chrono::Utc::now() - chrono::Duration::days(60)),
            "retention_guard",
        );
        let initial = run_br(&ws, ["sync", "--flush-only", "--json"], "guard_initial");
        assert!(initial.status.success(), "{initial:?}");
        let local_remote = stored_retention_issue(&ws, &remote_id);
        let pending = run_br(
            &ws,
            [
                "--no-auto-flush",
                "update",
                &dirty_id,
                "--title",
                "Pending local edit",
            ],
            "guard_pending",
        );
        assert!(pending.status.success(), "{pending:?}");
        let mut lines = read_jsonl_lines(&ws);
        let remote_line = lines
            .iter_mut()
            .find(|line| serde_json::from_str::<Value>(line).unwrap()["id"] == remote_id)
            .unwrap();
        *remote_line = set_row_field(remote_line, field, value);
        let remote_line = remote_line.clone();
        write_jsonl_lines(&ws, &lines);
        let updated = run_br(
            &ws,
            [
                "--no-auto-import",
                "update",
                &dirty_id,
                "--title",
                "Published local edit",
            ],
            "guard_incremental",
        );
        assert!(updated.status.success(), "{updated:?}");
        assert!(read_jsonl_lines(&ws).contains(&remote_line));
        {
            let storage = SqliteStorage::open(&db_path(&ws)).unwrap();
            assert_eq!(storage.get_dirty_issue_count().unwrap(), 0);
            assert_eq!(
                storage.get_metadata("needs_flush").unwrap().as_deref(),
                Some("false")
            );
            assert_eq!(
                storage.get_metadata(METADATA_JSONL_CONTENT_HASH).unwrap(),
                Some(compute_jsonl_hash(&jsonl_path(&ws)).unwrap()),
                "the incremental exporter recorded this exact source hash"
            );
            assert_eq!(
                storage.get_export_hash(&remote_id).unwrap().unwrap().0,
                serde_json::from_str::<Issue>(&remote_line)
                    .unwrap()
                    .compute_content_hash(),
                "even the narrow certificate still matches"
            );
        }
        set_workspace_retention_days(&ws, 30);
        let before = fs::read(jsonl_path(&ws)).unwrap();
        let anchor_before = fs::read(beads_dir(&ws).join("beads.base.jsonl")).unwrap();
        let refused = run_br(&ws, ["sync", "--flush-only", "--json"], "guard_refused");
        assert!(
            !refused.status.success(),
            "full export would overwrite newer source: {refused:?}"
        );
        assert!(
            format!("{} {}", refused.stdout, refused.stderr)
                .contains("Cannot apply retention during a clean flush")
        );
        assert_eq!(fs::read(jsonl_path(&ws)).unwrap(), before);
        assert_eq!(
            fs::read(beads_dir(&ws).join("beads.base.jsonl")).unwrap(),
            anchor_before
        );
        assert_eq!(stored_retention_issue(&ws, &remote_id), local_remote);
        assert_eq!(stored_retention_issue(&ws, &tombstone_id), tombstone);
        assert_eq!(
            SqliteStorage::open(&db_path(&ws))
                .unwrap()
                .get_dirty_issue_count()
                .unwrap(),
            0
        );
    }
}

#[test]
fn clean_retention_flush_accepts_direct_tombstone_without_closed_at() {
    let ws = BrWorkspace::new();
    init_workspace(&ws, "direct_tombstone");
    let created = run_br(
        &ws,
        [
            "--no-auto-flush",
            "create",
            "Direct tombstone",
            "--status",
            "tombstone",
            "--json",
        ],
        "direct_create",
    );
    assert!(created.status.success(), "{created:?}");
    let id = parse_json_value(&created.stdout)["id"]
        .as_str()
        .unwrap()
        .to_string();
    let mut tombstone = stored_retention_issue(&ws, &id);
    assert_eq!(tombstone.status, Status::Tombstone);
    assert!(
        tombstone.closed_at.is_none(),
        "direct CLI creation does not fabricate a close timestamp"
    );
    tombstone.deleted_at = Some(chrono::Utc::now() - chrono::Duration::days(60));
    SqliteStorage::open(&db_path(&ws))
        .unwrap()
        .upsert_issue_for_import(&tombstone)
        .unwrap();
    let initial = run_br(&ws, ["sync", "--flush-only", "--json"], "direct_initial");
    assert!(initial.status.success(), "{initial:?}");
    assert_restored_tombstone_payload(&ws, &tombstone);

    set_workspace_retention_days(&ws, 30);
    let contracted = run_br(&ws, ["sync", "--flush-only", "--json"], "direct_contract");
    assert!(contracted.status.success(), "{contracted:?}");
    assert_retention_export(&ws, &[], &id);
    assert_eq!(stored_retention_issue(&ws, &id), tombstone);
    set_workspace_retention_days(&ws, 0);
    let expanded = run_br(&ws, ["sync", "--flush-only", "--json"], "direct_expand");
    assert!(expanded.status.success(), "{expanded:?}");
    assert_restored_tombstone_payload(&ws, &tombstone);
}

#[test]
fn retention_export_followed_by_noop_import_stays_clean() {
    let ws = BrWorkspace::new();
    init_workspace(&ws, "retention_noop");
    set_workspace_retention_days(&ws, 30);
    let live_id = create_issue(&ws, "Retained live issue", "retention_live");
    let expired_id = create_issue(&ws, "Expired local tombstone", "retention_expired");
    let comment = run_br(
        &ws,
        [
            "comments",
            "add",
            &expired_id,
            "Preserve deleted issue history",
        ],
        "retention_comment",
    );
    assert!(
        comment.status.success(),
        "fixture comment failed: {comment:?}"
    );
    let expired = delete_retention_fixture(
        &ws,
        &expired_id,
        Some(chrono::Utc::now() - chrono::Duration::days(365)),
        "retention_expired",
    );
    assert!(expired.is_expired_tombstone(Some(30)));
    assert_eq!(expired.comments.len(), 1);
    let live = stored_retention_issue(&ws, &live_id);
    let events = all_events_dump(&ws);

    let flush = run_br(
        &ws,
        ["--no-auto-import", "sync", "--flush-only", "--json"],
        "retention_explicit_flush",
    );
    assert!(flush.status.success(), "retention export failed: {flush:?}");
    assert_retention_export(&ws, &[&live_id], &expired_id);
    assert_eq!(stored_retention_issue(&ws, &expired_id), expired);
    assert_eq!(stored_retention_issue(&ws, &live_id), live);
    assert_eq!(all_events_dump(&ws), events);

    // Change bytes inside the JSON object, preserving every issue field but
    // bypassing the stored-file-hash shortcut for the first import.
    let before_hash = compute_jsonl_hash(&jsonl_path(&ws)).expect("export hash");
    let reformatted = read_jsonl_lines(&ws)
        .iter()
        .map(|line| line.replacen('{', "{ ", 1))
        .collect::<Vec<_>>();
    write_jsonl_lines(&ws, &reformatted);
    assert_ne!(
        compute_jsonl_hash(&jsonl_path(&ws)).expect("reformatted hash"),
        before_hash
    );
    let source_bytes = fs::read(jsonl_path(&ws)).expect("reformatted source bytes");

    for (pass, label) in ["retention_import", "retention_repeat"]
        .into_iter()
        .enumerate()
    {
        let import = run_br(
            &ws,
            ["--no-auto-flush", "sync", "--import-only", "--json"],
            label,
        );
        assert!(import.status.success(), "no-op import failed: {import:?}");
        let receipt = parse_json_value(&import.stdout);
        assert_eq!(receipt["created"], 0);
        assert_eq!(receipt["updated"], 0);
        if pass == 0 {
            assert_eq!(receipt["skipped"], 1, "must execute skip certification");
        }
        assert_retention_export(&ws, &[&live_id], &expired_id);
        assert_eq!(stored_retention_issue(&ws, &expired_id), expired);
        assert_eq!(stored_retention_issue(&ws, &live_id), live);
        assert_eq!(issue_count(&ws, label), 2);
        assert_eq!(all_events_dump(&ws), events);
        assert_eq!(
            fs::read(jsonl_path(&ws)).expect("source bytes"),
            source_bytes
        );
    }
}

#[test]
fn retention_incremental_auto_flush_omits_dirty_and_clean_expired_tombstones() {
    for (label, clean_tombstone) in [("dirty_expired", false), ("clean_expired", true)] {
        let ws = BrWorkspace::new();
        init_workspace(&ws, label);
        let live_id = create_issue(&ws, "Unrelated live issue", "incremental_live");
        let expired_id = create_issue(&ws, "Pending expired tombstone", "incremental_expired");
        let expired = delete_retention_fixture(
            &ws,
            &expired_id,
            Some(chrono::Utc::now() - chrono::Duration::days(365)),
            label,
        );

        if clean_tombstone {
            // Export with the default keep-forever policy before enabling
            // retention. The later live edit must also remove this clean row.
            let flush = run_br(
                &ws,
                ["--no-auto-import", "sync", "--flush-only", "--json"],
                "incremental_keep_forever_flush",
            );
            assert!(flush.status.success(), "fixture export failed: {flush:?}");
        }
        assert!(
            read_jsonl_lines(&ws)
                .iter()
                .any(|line| row_id(line) == expired_id)
        );
        set_workspace_retention_days(&ws, 30);
        let tombstone_events = {
            let storage = SqliteStorage::open(&db_path(&ws)).expect("open incremental fixture");
            let expected_dirty = usize::from(!clean_tombstone);
            assert_eq!(
                storage.get_dirty_issue_count().expect("dirty count"),
                expected_dirty
            );
            assert_eq!(
                storage
                    .get_metadata("needs_flush")
                    .expect("flush flag")
                    .as_deref(),
                Some("false"),
                "an existing source with no full-flush request must use the incremental path"
            );
            assert!(
                storage
                    .get_export_hash(&expired_id)
                    .expect("prior certificate")
                    .is_some()
            );
            storage
                .get_events(&expired_id, 0)
                .expect("tombstone events")
        };

        let update = run_br(
            &ws,
            [
                "update",
                &live_id,
                "--title",
                "Live edit triggers retention",
                "--json",
            ],
            "incremental_ordinary_update",
        );
        assert!(
            update.status.success(),
            "ordinary update failed: {update:?}"
        );
        assert_retention_export(&ws, &[&live_id], &expired_id);
        assert_eq!(stored_retention_issue(&ws, &expired_id), expired);
        assert_eq!(
            stored_retention_issue(&ws, &live_id).title,
            "Live edit triggers retention"
        );
        let storage = SqliteStorage::open(&db_path(&ws)).expect("open incremental result");
        assert_eq!(storage.count_all_issues().expect("issue count"), 2);
        assert_eq!(
            storage
                .get_events(&expired_id, 0)
                .expect("tombstone events"),
            tombstone_events
        );
    }
}

#[test]
fn retention_restored_omissions_require_full_auto_flush_for_protected_local_state() {
    for (label, tombstone, deleted_at) in [
        ("live", false, None),
        (
            "recent_tombstone",
            true,
            Some(chrono::Utc::now() - chrono::Duration::days(1)),
        ),
        ("unknown_date_tombstone", true, None),
    ] {
        let ws = BrWorkspace::new();
        init_workspace(&ws, label);
        set_workspace_retention_days(&ws, 30);
        let anchor_id = create_issue(&ws, "Shared source row", "retention_anchor");
        let protected_id = create_issue(&ws, "Preserve omitted local state", "retention_protected");
        let comment = run_br(
            &ws,
            [
                "comments",
                "add",
                &protected_id,
                "Local relation survives restored omission",
            ],
            "retention_protected_comment",
        );
        assert!(
            comment.status.success(),
            "fixture comment failed: {comment:?}"
        );
        let expired_id = create_issue(&ws, "Intentionally omitted expired state", "retention_old");
        let expired = delete_retention_fixture(
            &ws,
            &expired_id,
            Some(chrono::Utc::now() - chrono::Duration::days(365)),
            "retention_old",
        );
        let protected = if tombstone {
            delete_retention_fixture(&ws, &protected_id, deleted_at, label)
        } else {
            stored_retention_issue(&ws, &protected_id)
        };
        assert!(!protected.is_expired_tombstone(Some(30)));
        assert_eq!(protected.comments.len(), 1);
        let flush = run_br(
            &ws,
            ["--no-auto-import", "sync", "--flush-only", "--json"],
            "retention_before_restore",
        );
        assert!(flush.status.success(), "fixture export failed: {flush:?}");
        assert_retention_export(&ws, &[&anchor_id, &protected_id], &expired_id);

        let restored = read_jsonl_lines(&ws)
            .into_iter()
            .filter(|line| row_id(line) != protected_id)
            .collect::<Vec<_>>();
        write_jsonl_lines(&ws, &restored);
        let source_bytes = fs::read(jsonl_path(&ws)).expect("restored source bytes");
        let events = all_events_dump(&ws);
        let import = run_br(
            &ws,
            ["--no-auto-flush", "sync", "--import-only", "--json"],
            "retention_import_restored",
        );
        assert!(
            import.status.success(),
            "restored import failed: {import:?}"
        );
        let receipt = parse_json_value(&import.stdout);
        assert_eq!(receipt["created"], 0);
        assert_eq!(receipt["updated"], 0);
        assert_eq!(receipt["skipped"], 1);
        assert_eq!(get_needs_flush(&ws).as_deref(), Some("true"));
        assert_eq!(stored_retention_issue(&ws, &protected_id), protected);
        assert_eq!(stored_retention_issue(&ws, &expired_id), expired);
        assert_eq!(all_events_dump(&ws), events);
        assert_eq!(
            fs::read(jsonl_path(&ws)).expect("source bytes"),
            source_bytes
        );
        {
            let storage = SqliteStorage::open(&db_path(&ws)).expect("open pending export witness");
            assert_eq!(storage.get_dirty_issue_count().expect("dirty count"), 0);
            assert!(
                storage
                    .get_export_hash(&protected_id)
                    .expect("omitted certificate")
                    .is_none()
            );
        }

        // needs_flush, not a dirty protected issue, must select the full
        // automatic exporter after this ordinary mutation of the shared row.
        let update = run_br(
            &ws,
            [
                "update",
                &anchor_id,
                "--title",
                "Edited after restored export",
                "--json",
            ],
            "retention_ordinary_full_flush",
        );
        assert!(
            update.status.success(),
            "ordinary update failed: {update:?}"
        );
        assert_retention_export(&ws, &[&anchor_id, &protected_id], &expired_id);
        assert_eq!(stored_retention_issue(&ws, &protected_id), protected);
        assert_eq!(stored_retention_issue(&ws, &expired_id), expired);
        assert_eq!(issue_count(&ws, label), 3);

        let published = read_jsonl_lines(&ws)
            .iter()
            .map(|line| serde_json::from_str::<Value>(line).expect("published issue"))
            .find(|issue| issue["id"].as_str() == Some(protected_id.as_str()))
            .expect("omitted issue restored to JSONL");
        let mut expected = serde_json::to_value(&protected).expect("protected issue JSON");
        expected
            .as_object_mut()
            .expect("issue object")
            .remove("source_repo_path");
        assert_eq!(
            published, expected,
            "full auto-flush must preserve the complete local payload"
        );
    }
}

#[test]
fn restored_export_no_op_import_does_not_starve_concurrent_commands() {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    let ws = BrWorkspace::new();
    init_workspace(&ws, "skip_contention");
    let seed = create_issue(&ws, "Local state must survive", "skip_seed");
    let template = read_jsonl_lines(&ws)[0].clone();
    let mut lines = vec![template];
    for index in 1..2435 {
        lines.push(synthetic_row(&lines[0], index));
    }
    write_jsonl_lines(&ws, &lines);
    let imported = run_br(&ws, ["sync", "--import-only", "--json"], "skip_import");
    assert!(imported.status.success(), "{}", imported.stderr);
    let old_export = fs::read(jsonl_path(&ws)).unwrap();
    let local = run_br(
        &ws,
        ["comments", "add", &seed, "local before restore"],
        "skip_local",
    );
    assert!(local.status.success(), "{}", local.stderr);
    fs::write(jsonl_path(&ws), &old_export).unwrap();

    let log_path = ws.root.join("skip_import.log");
    let log = fs::File::create(&log_path).unwrap();
    let started = Instant::now();
    let mut command = Command::new(assert_cmd::cargo::cargo_bin!("br"));
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if name.starts_with("BD_")
            || name.starts_with("BEADS_")
            || matches!(
                name.as_ref(),
                "BR_DISABLE_READ_ONLY_FAST_OPEN"
                    | "BR_OUTPUT_FORMAT"
                    | "TOON_DEFAULT_FORMAT"
                    | "TOON_STATS"
            )
        {
            command.env_remove(key);
        }
    }
    let mut importer = command
        .current_dir(&ws.root)
        .env("HOME", &ws.root)
        .env("RUST_LOG", "error,beads_rust::sync=debug")
        .env("NO_COLOR", "1")
        .env("PATH", common::cli::deduplicated_br_path())
        .env("BR_HISTORY_MIN_INTERVAL_SECS", "0")
        .args(["--no-auto-flush", "-v", "show", &seed, "--json"])
        .stdout(Stdio::null())
        .stderr(Stdio::from(log))
        .spawn()
        .unwrap();
    // Wait for the actual import apply phase, rather than merely racing
    // process startup. A fast importer may finish before this poll observes it.
    loop {
        let log = fs::read_to_string(&log_path).unwrap();
        if log.contains("collision plan ready; applying rows") {
            break;
        }
        if let Some(status) = importer.try_wait().unwrap() {
            assert!(status.success(), "{log}");
            panic!("auto-import apply phase was not observed: {log}");
        }
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "import did not reach apply: {log}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    std::thread::scope(|scope| {
        let reader = scope.spawn(|| {
            run_br(
                &ws,
                [
                    "--lock-timeout",
                    "10000",
                    "--no-auto-flush",
                    "show",
                    &seed,
                    "--json",
                ],
                "skip_reader",
            )
        });
        let writer = scope.spawn(|| {
            run_br(
                &ws,
                [
                    "--lock-timeout",
                    "10000",
                    "--no-auto-flush",
                    "comments",
                    "add",
                    &seed,
                    "concurrent writer",
                ],
                "skip_writer",
            )
        });
        let reader = reader.join().unwrap();
        let writer = writer.join().unwrap();
        assert!(reader.status.success(), "reader starved: {}", reader.stderr);
        assert!(writer.status.success(), "writer starved: {}", writer.stderr);
    });
    assert!(
        importer.wait().unwrap().success(),
        "{}",
        fs::read_to_string(&log_path).unwrap()
    );
    let log = fs::read_to_string(&log_path).unwrap();
    assert!(
        log.contains("imported_count=0"),
        "expected a no-op import: {log}"
    );
    let apply_line = log
        .lines()
        .find(|line| line.contains("issue rows, relations, and export hashes written"))
        .expect("import must report its apply duration");
    let apply_ms = apply_line
        .split("elapsed_ms=")
        .nth(1)
        .and_then(|suffix| suffix.split_whitespace().next())
        .and_then(|value| value.parse::<u64>().ok())
        .expect("numeric apply duration");
    assert!(
        apply_ms < 10_000,
        "skip certification held the lock too long: {apply_line}"
    );
    eprintln!("2435-row restored export: {apply_line}");
    let comments = run_br(
        &ws,
        ["--no-auto-flush", "comments", "list", &seed, "--json"],
        "skip_comments",
    );
    assert!(comments.status.success(), "{}", comments.stderr);
    assert!(comments.stdout.contains("local before restore"));
    assert!(comments.stdout.contains("concurrent writer"));
}
