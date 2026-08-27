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
                    if let Ok(contents) = fs::read(&path) {
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
                        let mtime = meta
                            .modified()
                            .ok()
                            .and_then(|t| t.duration_since(std::time::SystemTime::UNIX_EPOCH).ok())
                            .map_or(0, |d| d.as_nanos());
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
// Portable source_repo_path migration (#402)
// ============================================================================

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
    let target = fs::canonicalize(&ws.root)
        .expect("canonical workspace")
        .to_string_lossy()
        .into_owned();
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
    for row in normalized_jsonl.values() {
        assert_eq!(row["source_repo_path"].as_str(), Some(target.as_str()));
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
        "drift update failed: {}",
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
