//! E2E tests for the stale-schema self-heal.
//!
//! A tracker database left on an older schema used to refuse every command.
//! Ordinary commands now upgrade it automatically when an audit proves every
//! database row is represented in the JSONL, keep a backup, and print one
//! stderr notice; otherwise mutations refuse with the one command that
//! resolves it and read-only commands read the JSONL directly.
//!
//! Fixture provenance: `schema4_v0127_*.db.gz` were written by the released
//! `br 0.1.27` binary (which stamps `user_version = 4`) with real `init` /
//! `create` / `label add` / `comments add` / `dep add` / `sync --flush-only`
//! commands. `schema4_v0127.issues.jsonl` is that binary's flush of the clean
//! database. The `db_only` variant then ran `--no-auto-flush create` (a new
//! issue absent from the JSONL) and `--no-auto-flush update --title` (an
//! unflushed, newer edit of `leg-n57`).

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use flate2::read::GzDecoder;
use serde_json::Value;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("schema_migration")
}

fn install(workspace: &BrWorkspace, db_gz: &str, issues: &str, prefix: &str) -> PathBuf {
    let beads_dir = workspace.root.join(".beads");
    fs::create_dir_all(&beads_dir).expect("create .beads");
    let mut decoder = GzDecoder::new(fs::File::open(fixture_dir().join(db_gz)).expect("open gz"));
    let mut bytes = Vec::new();
    decoder.read_to_end(&mut bytes).expect("gunzip fixture");
    let db_path = beads_dir.join("beads.db");
    fs::write(&db_path, bytes).expect("write beads.db");
    fs::copy(fixture_dir().join(issues), beads_dir.join("issues.jsonl")).expect("copy jsonl");
    fs::write(
        beads_dir.join("config.yaml"),
        format!("issue_prefix: {prefix}\n"),
    )
    .expect("write config");
    db_path
}

fn header_user_version(db_path: &Path) -> u32 {
    let bytes = fs::read(db_path).expect("read db");
    u32::from_be_bytes(bytes[60..64].try_into().expect("db header"))
}

fn current_version() -> u32 {
    u32::try_from(beads_rust::storage::schema::CURRENT_SCHEMA_VERSION).unwrap()
}

fn json_stdout(stdout: &str) -> Value {
    serde_json::from_str(&extract_json_payload(stdout)).expect("stdout must be clean JSON")
}

fn show(workspace: &BrWorkspace, id: &str) -> Value {
    let run = run_br(workspace, ["show", id, "--json"], &format!("show_{id}"));
    assert!(run.status.success(), "show {id}: {}", run.stderr);
    json_stdout(&run.stdout)[0].clone()
}

#[test]
fn clean_legacy_database_heals_automatically_and_keeps_a_backup() {
    let workspace = BrWorkspace::new();
    let db_path = install(
        &workspace,
        "schema4_v0127_clean.db.gz",
        "schema4_v0127.issues.jsonl",
        "leg",
    );
    assert_eq!(header_user_version(&db_path), 4);

    let create = run_br(
        &workspace,
        ["create", "Created after heal", "--json"],
        "create_after_heal",
    );
    assert!(
        create.status.success(),
        "create must heal and proceed: {}",
        create.stderr
    );
    let created = json_stdout(&create.stdout);
    assert_eq!(created["title"], "Created after heal");
    let notices: Vec<&str> = create
        .stderr
        .lines()
        .filter(|line| line.starts_with("br: tracker database was on schema 4"))
        .collect();
    assert_eq!(notices.len(), 1, "one notice line: {}", create.stderr);
    assert!(
        notices[0].contains("rebuilt it from the JSONL") && notices[0].contains(".br_recovery")
    );

    // Current schema now, with every flushed row, label, comment and dependency.
    let listed = run_br(&workspace, ["list", "--json"], "list_after_heal");
    assert!(listed.status.success(), "{}", listed.stderr);
    assert!(
        !listed.stderr.contains("tracker database was on schema"),
        "heals once"
    );
    let issue = show(&workspace, "leg-n57");
    assert_eq!(issue["labels"], serde_json::json!(["legacy-label"]));
    assert_eq!(issue["comments"][0]["text"], "legacy comment text");
    let second = show(&workspace, "leg-znr");
    assert_eq!(second["dependencies"][0]["id"], "leg-n57", "{second}");

    // The old database is retained byte-for-byte in the recovery directory.
    let recovery = workspace.root.join(".beads/.br_recovery");
    let retained = walk(&recovery)
        .into_iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("beads.db") && !name.contains("-wal"))
        })
        .find(|path| {
            fs::read(path).is_ok_and(|bytes| bytes.len() > 100 && header_bytes(&bytes) == 4)
        });
    assert!(
        retained.is_some(),
        "schema-4 backup must be retained under {recovery:?}"
    );
}

fn header_bytes(bytes: &[u8]) -> u32 {
    u32::from_be_bytes(bytes[60..64].try_into().expect("header"))
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                out.extend(walk(&path));
            } else {
                out.push(path);
            }
        }
    }
    out
}

#[test]
fn database_only_data_refuses_mutation_reads_jsonl_and_heals_explicitly() {
    let workspace = BrWorkspace::new();
    let db_path = install(
        &workspace,
        "schema4_v0127_db_only.db.gz",
        "schema4_v0127.issues.jsonl",
        "leg",
    );
    let source = fs::read(&db_path).expect("source bytes");

    // Mutations refuse, naming the command and the database-only issues.
    let create = run_br(
        &workspace,
        ["create", "blocked", "--json"],
        "create_refused",
    );
    assert!(!create.status.success(), "must refuse: {}", create.stdout);
    let error = json_stdout(&create.stdout);
    let message = error["error"]["message"].as_str().expect("message");
    assert!(
        message.contains("br doctor migrate-schema heal")
            && message.contains("leg-u4i (absent from JSONL)")
            && message.contains("leg-n57 (unflushed edit)"),
        "{message}"
    );
    assert_eq!(fs::read(&db_path).expect("after refusal"), source);

    // Read-only commands read the JSONL (as --no-db) with clean JSON stdout.
    let list = run_br(&workspace, ["list", "--json"], "list_fallback");
    assert!(list.status.success(), "{}", list.stderr);
    let rows = json_stdout(&list.stdout);
    let ids: Vec<&str> = rows["issues"]
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|row| row["id"].as_str())
        .collect();
    assert!(
        ids.contains(&"leg-n57") && !ids.contains(&"leg-u4i"),
        "{ids:?}"
    );
    assert!(
        list.stderr.contains("reads issues.jsonl directly"),
        "{}",
        list.stderr
    );
    assert_eq!(header_user_version(&db_path), 4);

    // The dry run reports the audit without changing anything.
    let dry = run_br(
        &workspace,
        ["doctor", "migrate-schema", "heal", "--dry-run", "--json"],
        "heal_dry_run",
    );
    assert!(dry.status.success(), "{}", dry.stderr);
    let dry = json_stdout(&dry.stdout);
    assert_eq!(dry["automatic_heal_allowed"], false);
    assert_eq!(dry["audit"]["from_version"], 4);
    assert_eq!(
        dry["audit"]["db_only"].as_array().expect("db_only").len(),
        2
    );
    assert_eq!(fs::read(&db_path).expect("after dry run"), source);

    // The explicit heal rebuilds and re-adds both database-only issues.
    let heal = run_br(
        &workspace,
        ["doctor", "migrate-schema", "heal", "--json"],
        "heal_explicit",
    );
    assert!(heal.status.success(), "{} {}", heal.stdout, heal.stderr);
    let outcome = json_stdout(&heal.stdout);
    assert_eq!(outcome["action"], "rebuilt");
    let mut restored: Vec<&str> = outcome["restored_db_only"]
        .as_array()
        .expect("restored")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    restored.sort_unstable();
    assert_eq!(restored, vec!["leg-n57", "leg-u4i"]);
    assert_eq!(header_user_version(&db_path), current_version());

    assert_eq!(
        show(&workspace, "leg-n57")["title"],
        "Renamed locally, never flushed"
    );
    assert_eq!(
        show(&workspace, "leg-u4i")["title"],
        "Unflushed legacy issue"
    );
    let flush = run_br(&workspace, ["sync", "--flush-only", "--json"], "flush");
    assert!(flush.status.success(), "{}", flush.stderr);
    let jsonl = fs::read_to_string(workspace.root.join(".beads/issues.jsonl")).expect("jsonl");
    assert!(jsonl.contains("\"leg-u4i\"") && jsonl.contains("Renamed locally, never flushed"));
}

#[test]
fn explicitly_read_only_invocation_never_heals() {
    let workspace = BrWorkspace::new();
    let db_path = install(
        &workspace,
        "schema4_v0127_clean.db.gz",
        "schema4_v0127.issues.jsonl",
        "leg",
    );
    let source = fs::read(&db_path).expect("source bytes");
    let list = run_br(
        &workspace,
        ["list", "--json", "--no-auto-import", "--no-auto-flush"],
        "observational_list",
    );
    assert!(list.status.success(), "{}", list.stderr);
    assert_eq!(
        json_stdout(&list.stdout)["issues"]
            .as_array()
            .expect("rows")
            .len(),
        2
    );
    assert!(
        list.stderr.contains("migrate-schema heal"),
        "{}",
        list.stderr
    );
    assert_eq!(fs::read(&db_path).expect("unchanged"), source);
}

#[test]
fn reviewed_range_database_heals_through_the_reviewed_migration() {
    let workspace = BrWorkspace::new();
    let beads_dir = workspace.root.join(".beads");
    fs::create_dir_all(&beads_dir).expect("create .beads");
    let mut decoder = GzDecoder::new(
        fs::File::open(fixture_dir().join("schema16_v0219_release.db.gz")).expect("open gz"),
    );
    let mut bytes = Vec::new();
    decoder.read_to_end(&mut bytes).expect("gunzip");
    let db_path = beads_dir.join("beads.db");
    fs::write(&db_path, bytes).expect("write db");
    fs::copy(
        fixture_dir().join("schema16_issues.jsonl"),
        beads_dir.join("issues.jsonl"),
    )
    .expect("copy jsonl");
    fs::copy(
        fixture_dir().join("schema16_config.yaml"),
        beads_dir.join("config.yaml"),
    )
    .expect("copy config");
    assert_eq!(header_user_version(&db_path), 16);

    let dry = run_br(
        &workspace,
        ["doctor", "migrate-schema", "heal", "--dry-run", "--json"],
        "heal16_dry_run",
    );
    assert!(dry.status.success(), "{}", dry.stderr);
    let dry = json_stdout(&dry.stdout);
    let planned = dry["planned_action"].as_str().expect("planned action");
    assert!(
        planned.contains("reviewed in-place migration 16 ->"),
        "{planned}"
    );

    let heal = run_br(
        &workspace,
        ["doctor", "migrate-schema", "heal", "--json"],
        "heal16",
    );
    assert!(heal.status.success(), "{} {}", heal.stdout, heal.stderr);
    let outcome = json_stdout(&heal.stdout);
    assert_eq!(outcome["action"], "migrated", "{outcome}");
    assert!(
        outcome["backup"]
            .as_str()
            .is_some_and(|backup| backup.contains("schema-migrations")),
        "{outcome}"
    );
    let list = run_br(&workspace, ["list", "--json", "--all"], "list16");
    assert!(list.status.success(), "{}", list.stderr);
    assert!(
        !json_stdout(&list.stdout)["issues"]
            .as_array()
            .expect("rows")
            .is_empty()
    );
}

/// A JSONL dependency edge whose target issue exists nowhere is dropped by the
/// import's orphan cleanup. The post-import verifiers used to compare against
/// the raw payload, so every import (and every rebuild) of such a JSONL failed
/// with "does not match its normalized JSONL payload (differing fields:
/// dependencies)"; five real trackers carried such edges.
#[test]
fn dangling_dependency_edge_does_not_block_import_or_heal() {
    let workspace = BrWorkspace::new();
    let db_path = install(
        &workspace,
        "schema4_v0127_clean.db.gz",
        "schema4_v0127.issues.jsonl",
        "leg",
    );
    let jsonl_path = workspace.root.join(".beads/issues.jsonl");
    let original = fs::read_to_string(&jsonl_path).expect("jsonl");
    let mut rows: Vec<Value> = original
        .lines()
        .map(|line| serde_json::from_str(line).expect("row"))
        .collect();
    let first = rows
        .iter_mut()
        .find(|row| row["id"] == "leg-n57")
        .expect("leg-n57");
    first["dependencies"] = serde_json::json!([{
        "issue_id": "leg-n57",
        "depends_on_id": "leg-purged",
        "type": "discovered-from",
        "created_at": "2026-09-25T18:26:08Z",
        "created_by": "tester"
    }]);
    let rewritten = rows
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(&jsonl_path, format!("{rewritten}\n")).expect("rewrite jsonl");

    // The automatic heal rebuilds from this JSONL.
    let list = run_br(
        &workspace,
        ["list", "--json"],
        "list_heals_with_dangling_edge",
    );
    assert!(list.status.success(), "{}", list.stderr);
    assert!(
        list.stderr.contains("rebuilt it from the JSONL"),
        "{}",
        list.stderr
    );
    assert_eq!(header_user_version(&db_path), current_version());

    // A plain re-import of the same JSONL succeeds too.
    let import = run_br(
        &workspace,
        ["sync", "--import-only", "--rebuild", "--json"],
        "reimport_with_dangling_edge",
    );
    assert!(
        import.status.success(),
        "{} {}",
        import.stdout,
        import.stderr
    );
    assert!(
        show(&workspace, "leg-n57")["dependencies"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "the dangling edge is dropped locally"
    );
}
