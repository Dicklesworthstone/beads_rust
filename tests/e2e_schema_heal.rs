//! E2E tests for the stale-schema self-heal.
//!
//! Read commands must never migrate or replace an older database. Mutations
//! may run a reviewed lossless migration, but a missing or refused migration
//! is not permission to replace database authority with a JSONL projection.
//!
//! Fixture provenance: `schema4_v0127_*.db.gz` were written by the released
//! `br 0.1.27` binary (which stamps `user_version = 4`) with real `init` /
//! `create` / `label add` / `comments add` / `dep add` / `sync --flush-only`
//! commands. `schema4_v0127.issues.jsonl` is that binary's flush of the clean
//! database. The `db_only` variant then ran `--no-auto-flush create` (a new
//! issue absent from the JSONL) and `--no-auto-flush update --title` (an
//! unflushed, newer edit of `leg-n57`).

mod common;

use beads_rust::franken_sync::{Connection, SqliteValue};
use common::cli::{BrWorkspace, extract_json_payload, run_br, run_br_with_env};
use flate2::read::GzDecoder;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
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

/// Source payloads and retained recovery evidence. A refused mutation may
/// acquire coordination locks, but none of these bytes may change.
fn protected_files(workspace: &BrWorkspace) -> BTreeMap<PathBuf, Vec<u8>> {
    protected_files_at(&workspace.root.join(".beads"))
}

fn protected_files_at(beads_dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut files = BTreeMap::new();
    for name in [
        "beads.db",
        "beads.db-wal",
        "beads.db-journal",
        "issues.jsonl",
        "issues.history.jsonl",
        "metadata.json",
        "config.yaml",
    ] {
        let path = beads_dir.join(name);
        match fs::read(&path) {
            Ok(bytes) => {
                files.insert(PathBuf::from(name), bytes);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("cannot inspect {}: {error}", path.display()),
        }
    }
    for path in walk(&beads_dir.join(".br_recovery")) {
        files.insert(
            path.strip_prefix(beads_dir).unwrap().to_path_buf(),
            fs::read(path).expect("read retained recovery evidence"),
        );
    }
    files
}

/// An observational command must leave even reader coordination files alone.
/// Include every tracker file, including SHM, lock files and recovery evidence.
fn tracker_files(beads_dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    walk(beads_dir)
        .into_iter()
        .map(|path| {
            (
                path.strip_prefix(beads_dir).unwrap().to_path_buf(),
                fs::read(&path).expect("read tracker file"),
            )
        })
        .collect()
}

fn assert_tracker_unchanged(beads_dir: &Path, before: &BTreeMap<PathBuf, Vec<u8>>) {
    let after = tracker_files(beads_dir);
    let changed: Vec<_> = before
        .keys()
        .chain(after.keys())
        .filter(|path| before.get(*path) != after.get(*path))
        .collect();
    assert!(
        changed.is_empty(),
        "observational command changed tracker files: {changed:?}"
    );
}

fn edit_fixture(db: &Path, statements: &[&str]) {
    let connection = Connection::open(db.to_string_lossy().into_owned()).expect("open fixture");
    for sql in statements {
        connection.execute(sql).expect("edit fixture");
    }
    connection.close().expect("close fixture");
}

fn add_unreviewed_dependency_index(db: &Path) {
    // The released schema4 now has a lossless upgrade. Keep refusal controls
    // outside that exact reviewed profile without removing any original data.
    edit_fixture(
        db,
        &["CREATE INDEX operator_dependency_index ON dependencies(created_by)"],
    );
}

fn move_to_nested_directory(workspace: &mut BrWorkspace) {
    workspace.root = workspace.root.join("project/nested");
    fs::create_dir_all(&workspace.root).unwrap();
    assert!(!workspace.root.join(".beads").exists());
}

fn show(workspace: &BrWorkspace, id: &str) -> Value {
    let run = run_br(workspace, ["show", id, "--json"], &format!("show_{id}"));
    assert!(run.status.success(), "show {id}: {}", run.stderr);
    json_stdout(&run.stdout)[0].clone()
}

/// Synthetic variants of the released schema-16 tracker. A schema-17 source
/// receives the actual v17 occupancy addition; other version variants
/// deliberately exercise dispatch decisions. The frozen released fixtures in
/// e2e_schema_migration_upgrade independently qualify historical migrations.
fn synthetic_tracker(version: u32, events: usize, refused_index: bool) -> (BrWorkspace, PathBuf) {
    let workspace = BrWorkspace::new();
    let db = install(
        &workspace,
        "schema16_v0219_release.db.gz",
        "schema16_issues.jsonl",
        "v15ws",
    );
    fs::write(
        workspace.root.join(".beads/metadata.json"),
        "{\"database\":\"beads.db\",\"jsonl_export\":\"issues.jsonl\"}\n",
    )
    .unwrap();
    let connection = Connection::open(db.to_string_lossy().into_owned()).expect("open fixture");
    connection.execute("BEGIN IMMEDIATE").unwrap();
    // Replace only synthetic test records so the investigation's count is
    // exact, rather than counting the seed fixture's incidental audit rows.
    connection.execute("DELETE FROM events").unwrap();
    for index in 0..events {
        connection
            .execute_with_params(
                "INSERT INTO events \
                 (issue_id, event_type, actor, old_value, new_value, comment, created_at, \
                  agent_name, harness, model) \
                 VALUES ('v15ws-9nx', 'updated', 'gh-585', ?1, ?2, ?3, \
                         '2026-10-31T00:00:00Z', 'preserved-agent', 'preserved-harness', 'model')",
                &[
                    SqliteValue::from(format!("before-{index}")),
                    SqliteValue::from(format!("after-{index}")),
                    SqliteValue::from(format!("database-only audit event {index}")),
                ],
            )
            .unwrap();
    }
    for sql in [
        "INSERT INTO config (key, value) VALUES ('compact_batch_size', '37')",
        "INSERT INTO config (key, value) VALUES ('compact_model', 'local-model')",
        "INSERT INTO config (key, value) VALUES ('compaction_enabled', 'false')",
    ] {
        connection.execute(sql).unwrap();
    }
    if version == 17 && !refused_index {
        // Derive the genuine v17 shape from the released v16 fixture without
        // adding v18/v19 columns. This is a reviewed-upgrade control, not a
        // version stamp over an incompatible older table inventory.
        for sql in [
            "CREATE TABLE capacity_occupancy (issue_id TEXT PRIMARY KEY, actor TEXT, \
             agent_name TEXT, harness TEXT, session TEXT, \
             recorded_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP, \
             FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE)",
            "CREATE INDEX idx_capacity_occupancy_actor ON capacity_occupancy(actor) WHERE actor IS NOT NULL",
            "CREATE INDEX idx_capacity_occupancy_harness ON capacity_occupancy(harness) WHERE harness IS NOT NULL",
            "CREATE INDEX idx_capacity_occupancy_session ON capacity_occupancy(session) WHERE session IS NOT NULL",
        ] {
            connection.execute(sql).unwrap();
        }
    }
    if refused_index {
        // An operator's index remains outside the reviewed dependency-table
        // migration. The two historical accelerators from #585 are covered
        // separately by a successful lossless-upgrade regression below.
        connection
            .execute("CREATE INDEX operator_dependency_index ON dependencies (created_by)")
            .unwrap();
    }
    connection
        .execute(&format!("PRAGMA user_version = {version}"))
        .unwrap();
    connection.execute("COMMIT").unwrap();
    connection.close().expect("close synthetic fixture");
    (workspace, db)
}

/// Inspect a private copy: the assertion machinery must not itself checkpoint
/// or migrate the database whose preservation it is measuring.
fn query_copy(db: &Path, sql: &str) -> Vec<Vec<SqliteValue>> {
    let scratch = tempfile::tempdir().expect("scratch copy");
    let copy = scratch.path().join("copy.db");
    fs::copy(db, &copy).unwrap();
    for suffix in ["-wal", "-journal"] {
        let source = PathBuf::from(format!("{}{suffix}", db.display()));
        if source.is_file() {
            fs::copy(source, scratch.path().join(format!("copy.db{suffix}"))).unwrap();
        }
    }
    let connection = Connection::open(copy.to_string_lossy().into_owned()).expect("open copy");
    let rows = connection
        .query(sql)
        .expect("read preserved records")
        .iter()
        .map(|row| row.values().to_vec())
        .collect();
    connection.close().expect("close inspection copy");
    rows
}

fn event_count(db: &Path) -> usize {
    let rows = query_copy(db, "SELECT count(*) FROM events");
    match rows[0][0] {
        SqliteValue::Integer(count) => usize::try_from(count).expect("nonnegative event count"),
        ref other => panic!("invalid event count: {other:?}"),
    }
}

fn seed_migration_only_records(db: &Path) {
    edit_fixture(
        db,
        &[
            "INSERT INTO metadata (key, value) VALUES ('operator_receipt', 'preserve this receipt')",
            "INSERT INTO child_counters (parent_id, last_child) VALUES ('v15ws-4kf', 47)",
            "CREATE TABLE operator_notes (id INTEGER PRIMARY KEY AUTOINCREMENT, note TEXT NOT NULL, payload BLOB NOT NULL)",
            "INSERT INTO operator_notes VALUES (12, 'custom durable state', X'0001FF00')",
            "UPDATE sqlite_sequence SET seq = 91 WHERE name = 'operator_notes'",
        ],
    );
}

fn migration_only_records(db: &Path) -> BTreeMap<&'static str, Vec<Vec<SqliteValue>>> {
    [
        ("events", "SELECT * FROM events ORDER BY id"),
        ("config", "SELECT * FROM config ORDER BY key, value"),
        (
            "metadata",
            "SELECT key, value FROM metadata WHERE key = 'operator_receipt'",
        ),
        (
            "child_counters",
            "SELECT * FROM child_counters ORDER BY parent_id",
        ),
        ("operator_notes", "SELECT * FROM operator_notes ORDER BY id"),
        (
            "sqlite_sequence",
            "SELECT name, seq FROM sqlite_sequence WHERE name = 'operator_notes'",
        ),
    ]
    .into_iter()
    .map(|(name, query)| (name, query_copy(db, query)))
    .collect()
}

fn assert_migration_kept_records(
    db: &Path,
    before: &BTreeMap<&'static str, Vec<Vec<SqliteValue>>>,
) {
    let after = migration_only_records(db);
    for (table, rows) in before {
        if *table == "events" {
            assert!(
                rows.iter().all(|row| after[*table].contains(row)),
                "migration removed or altered original event records"
            );
            assert!(
                after[*table].len() > rows.len(),
                "the successful mutation must record its own new event"
            );
        } else {
            assert_eq!(after[*table], *rows, "migration changed durable {table}");
        }
    }
}

#[test]
fn legacy_database_with_an_unreviewed_index_refuses_automatic_rebuild() {
    let workspace = BrWorkspace::new();
    let db_path = install(
        &workspace,
        "schema4_v0127_clean.db.gz",
        "schema4_v0127.issues.jsonl",
        "leg",
    );
    add_unreviewed_dependency_index(&db_path);
    assert_eq!(header_user_version(&db_path), 4);
    let before = protected_files(&workspace);

    let create = run_br(
        &workspace,
        ["create", "Created after heal", "--json"],
        "create_after_heal",
    );
    assert!(
        !create.status.success(),
        "an unreviewed schema must not be rebuilt automatically: {} {}",
        create.stdout,
        create.stderr,
    );
    let message = json_stdout(&create.stdout)["error"]["message"]
        .as_str()
        .expect("actionable refusal")
        .to_owned();
    assert!(
        message.contains("migration") && message.contains("doctor"),
        "{message}"
    );
    assert_eq!(protected_files(&workspace), before);
    assert_eq!(header_user_version(&db_path), 4);

    // The JSONL issue projection is still readable, without claiming that its
    // missing audit history is the complete database or replacing the source.
    let listed = run_br(&workspace, ["list", "--json"], "list_without_heal");
    assert!(listed.status.success(), "{}", listed.stderr);
    assert!(
        listed.stderr.contains("reads issues.jsonl directly"),
        "{}",
        listed.stderr
    );
    let issue = show(&workspace, "leg-n57");
    assert_eq!(issue["labels"], serde_json::json!(["legacy-label"]));
    assert_eq!(issue["comments"][0]["text"], "legacy comment text");
    let second = show(&workspace, "leg-znr");
    assert_eq!(second["dependencies"][0]["id"], "leg-n57", "{second}");

    assert_eq!(protected_files(&workspace), before);
    assert!(!workspace.root.join(".beads/.br_recovery").exists());
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
fn database_only_data_refuses_an_unreviewed_shape_and_keeps_read_only_jsonl_available() {
    let workspace = BrWorkspace::new();
    let db_path = install(
        &workspace,
        "schema4_v0127_db_only.db.gz",
        "schema4_v0127.issues.jsonl",
        "leg",
    );
    add_unreviewed_dependency_index(&db_path);
    let source = fs::read(&db_path).expect("source bytes");
    let protected = protected_files(&workspace);

    // Without a reviewed migration the source must remain authoritative.
    // Automatic startup cannot treat issue equivalence as permission to
    // discard events, configuration, or a partially restorable database.
    let create = run_br(
        &workspace,
        ["create", "blocked", "--json"],
        "create_refused",
    );
    assert!(!create.status.success(), "must refuse: {}", create.stdout);
    let error = json_stdout(&create.stdout);
    let message = error["error"]["message"].as_str().expect("message");
    assert!(
        message.contains("br doctor migrate-schema heal") && message.contains("migration"),
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

    // Neither an explicit heal nor its discard flag establishes a crash-safe
    // migration. All local records must stay active, not merely in a backup.
    for discard in [false, true] {
        let mut args = vec!["doctor", "migrate-schema", "heal", "--json"];
        if discard {
            args.push("--discard-db-only");
        }
        let heal = run_br(&workspace, args, "heal_refused");
        assert!(!heal.status.success(), "{} {}", heal.stdout, heal.stderr);
        let message = json_stdout(&heal.stdout)["error"]["message"]
            .as_str()
            .expect("refusal")
            .to_owned();
        assert!(
            message.contains("migration") && message.contains("preserv"),
            "{message}"
        );
        assert_eq!(protected_files(&workspace), protected);
        assert_eq!(header_user_version(&db_path), 4);
        assert_eq!(event_count(&db_path), 7);
        let rows = query_copy(&db_path, "SELECT id, title FROM issues ORDER BY id");
        assert!(rows.iter().any(|row| row[0].as_text() == Some("leg-u4i")
            && row[1].as_text() == Some("Unflushed legacy issue")));
    }
    assert!(!workspace.root.join(".beads/.br_recovery").exists());
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
        list.stderr.contains("migrate-schema plan --json"),
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
fn dangling_dependency_edge_import_does_not_authorize_rebuilding_an_old_database() {
    let workspace = BrWorkspace::new();
    let db_path = install(
        &workspace,
        "schema4_v0127_clean.db.gz",
        "schema4_v0127.issues.jsonl",
        "leg",
    );
    add_unreviewed_dependency_index(&db_path);
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
    let before = protected_files(&workspace);

    // Reads may use JSONL; neither usable JSONL nor a dangling edge licenses
    // replacing this old tracker and dropping its database-only audit trail.
    let list = run_br(&workspace, ["list", "--json"], "list_with_dangling_edge");
    assert!(list.status.success(), "{}", list.stderr);
    assert!(
        list.stderr.contains("reads issues.jsonl directly"),
        "{}",
        list.stderr
    );
    assert_eq!(header_user_version(&db_path), 4);
    let heal = run_br(
        &workspace,
        ["doctor", "migrate-schema", "heal", "--json"],
        "heal_refuses_despite_readable_jsonl",
    );
    assert!(!heal.status.success(), "{} {}", heal.stdout, heal.stderr);
    assert_eq!(header_user_version(&db_path), 4);
    assert_eq!(protected_files(&workspace), before);
    assert_eq!(event_count(&db_path), 5);

    // Preserve the original orphan-import regression on a current tracker,
    // where the user is explicitly importing this interchange projection.
    let current = BrWorkspace::new();
    let init = run_br(
        &current,
        ["init", "--prefix", "leg", "--json"],
        "init_import_target",
    );
    assert!(init.status.success(), "{} {}", init.stdout, init.stderr);
    fs::copy(&jsonl_path, current.root.join(".beads/issues.jsonl")).unwrap();
    let import = run_br(
        &current,
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
        show(&current, "leg-n57")["dependencies"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "the dangling edge is dropped locally"
    );
}

/// Delete an issue the way pre-0.6.0 releases could: with foreign-key
/// enforcement off, so its label and comment rows outlive it.
fn add_orphan_relations(db_path: &Path, ghost: &str) {
    let conn = Connection::open(db_path.to_string_lossy().into_owned()).expect("open fixture");
    for statement in [
        "PRAGMA foreign_keys = OFF".to_owned(),
        format!("INSERT INTO labels (issue_id, label) VALUES ('{ghost}', 'orphan-label')"),
        format!(
            "INSERT INTO comments (issue_id, author, text, created_at) \
             VALUES ('{ghost}', 'agent', 'orphan comment', '2026-01-01 00:00:00')"
        ),
    ] {
        conn.execute(&statement).expect("add orphan relation row");
    }
    conn.close().expect("close fixture");
}

/// `(labels, comments)` rows owned by `issue_id`, read from a scratch copy so
/// the inspected database family is never touched.
fn relation_rows(db_path: &Path, issue_id: &str) -> (i64, i64) {
    let scratch = tempfile::tempdir().expect("scratch dir");
    let copy = scratch.path().join("copy.db");
    fs::copy(db_path, &copy).expect("copy database");
    let wal = PathBuf::from(format!("{}-wal", db_path.display()));
    if wal.is_file() {
        fs::copy(&wal, scratch.path().join("copy.db-wal")).expect("copy WAL");
    }
    let conn = Connection::open(copy.to_string_lossy().into_owned()).expect("open copy");
    let count = |table: &str| match conn
        .query(&format!(
            "SELECT count(*) FROM {table} WHERE issue_id = '{issue_id}'"
        ))
        .expect("count rows")
        .first()
        .and_then(|row| row.get(0).cloned())
    {
        Some(SqliteValue::Integer(rows)) => rows,
        other => panic!("count({table}) returned {other:?}"),
    };
    let rows = (count("labels"), count("comments"));
    let _ = conn.close();
    rows
}

/// Include every table, value, duplicate and hidden rowid. Comparing the raw
/// records also catches import-side changes to receipts and sequence counters
/// that issue-model equality would miss.
fn raw_database_records(db: &Path) -> BTreeMap<String, Vec<Vec<SqliteValue>>> {
    let mut records = BTreeMap::new();
    records.insert(
        "schema".to_owned(),
        query_copy(
            db,
            "SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY type, name",
        ),
    );
    for row in query_copy(db, "SELECT name FROM sqlite_master WHERE type = 'table'") {
        let table = row[0].as_text().expect("table name");
        let quoted = table.replace('"', "\"\"");
        records.insert(
            table.to_owned(),
            query_copy(
                db,
                &format!("SELECT rowid, * FROM \"{quoted}\" ORDER BY rowid"),
            ),
        );
    }
    records
}

fn seed_durable_import_orphans(db: &Path, ghost: &str) {
    add_orphan_relations(db, ghost);
    edit_fixture(
        db,
        &[
            "PRAGMA foreign_keys = OFF",
            &format!(
                "INSERT INTO events (issue_id, event_type, actor, old_value, new_value, comment, created_at) \
                 VALUES ('{ghost}', 'updated', 'original-operator', 'before', 'after', \
                         'durable audit evidence', '2026-01-01T00:00:00Z')"
            ),
            &format!("INSERT INTO child_counters (parent_id, last_child) VALUES ('{ghost}', 83)"),
        ],
    );
}

fn seed_orphan_dependency_endpoints(db: &Path, live_id: &str, ghost: &str) {
    edit_fixture(
        db,
        &[
            "PRAGMA foreign_keys = OFF",
            &format!(
                "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by) \
                 VALUES ('{ghost}', '{live_id}', 'blocks', '2026-01-01T00:00:00Z', 'original-operator')"
            ),
            &format!(
                "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by) \
                 VALUES ('{live_id}', '{ghost}', 'discovered-from', '2026-01-01T00:00:00Z', 'original-operator')"
            ),
        ],
    );
}

fn advance_jsonl_issue(jsonl: &Path, id: &str) {
    let original = fs::read_to_string(jsonl).expect("read JSONL");
    let mut rows: Vec<Value> = original
        .lines()
        .map(|line| serde_json::from_str(line).expect("valid JSONL row"))
        .collect();
    let issue = rows
        .iter_mut()
        .find(|row| row["id"] == id)
        .expect("issue in JSONL");
    issue["title"] = Value::from("External edit that must not erase local audit history");
    issue["updated_at"] = Value::from("2099-01-01T00:00:00Z");
    let rewritten = rows
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(jsonl, format!("{rewritten}\n")).expect("publish external JSONL edit");
}

fn assert_automatic_import_keeps_orphans(workspace: &mut BrWorkspace, db: &Path, live_id: &str) {
    let beads_dir = workspace.root.join(".beads");
    let jsonl = beads_dir.join("issues.jsonl");
    advance_jsonl_issue(&jsonl, live_id);
    let source = fs::read(&jsonl).unwrap();
    let before = raw_database_records(db);
    assert!(
        before["dirty_issues"].is_empty(),
        "must exercise automatic import"
    );
    move_to_nested_directory(workspace);

    // The explicitly observational escape hatch must continue to work even
    // while this tracker requires an operator to reconcile its orphan rows.
    let files = tracker_files(&beads_dir);
    let read = run_br_with_env(
        workspace,
        ["list", "--json", "--no-auto-import", "--no-auto-flush"],
        [("BR_DISABLE_READ_ONLY_FAST_OPEN", "1")],
        "orphan_noauto_read",
    );
    assert!(read.status.success(), "{} {}", read.stdout, read.stderr);
    assert_tracker_unchanged(&beads_dir, &files);

    let automatic = run_br(workspace, ["list", "--json"], "orphan_automatic_import");
    assert!(
        !automatic.status.success(),
        "automatic import must refuse before cleanup: {} {}",
        automatic.stdout,
        automatic.stderr
    );
    let output = json_stdout(&automatic.stdout);
    let message = output["error"]["message"]
        .as_str()
        .expect("actionable refusal");
    for expected in [
        "Automatic import refused",
        "events",
        "comments",
        "labels",
        "dependencies",
        "child_counters",
    ] {
        assert!(message.contains(expected), "missing {expected}: {message}");
    }
    assert!(message.contains("permanently delete"), "{message}");
    assert_eq!(
        raw_database_records(db),
        before,
        "refused import changed raw database state"
    );
    assert_eq!(
        fs::read(jsonl).unwrap(),
        source,
        "refused import rewrote JSONL"
    );
}

#[test]
fn current_database_automatic_import_refuses_preexisting_durable_orphans() {
    let mut workspace = BrWorkspace::new();
    let init = run_br(
        &workspace,
        ["init", "--prefix", "orph", "--json"],
        "orphan_current_init",
    );
    assert!(init.status.success(), "{} {}", init.stdout, init.stderr);
    let created = run_br(
        &workspace,
        ["create", "Original issue", "--json"],
        "orphan_current_create",
    );
    assert!(
        created.status.success(),
        "{} {}",
        created.stdout,
        created.stderr
    );
    let created = json_stdout(&created.stdout);
    let live_id = created["id"].as_str().expect("created issue id");
    let db = workspace.root.join(".beads/beads.db");
    seed_durable_import_orphans(&db, "orph-deleted");
    seed_orphan_dependency_endpoints(&db, live_id, "orph-deleted");
    assert_automatic_import_keeps_orphans(&mut workspace, &db, live_id);
}

#[test]
fn reviewed_migration_then_ordinary_import_keeps_preexisting_durable_orphans() {
    let (mut workspace, db) = synthetic_tracker(17, 5, false);
    seed_durable_import_orphans(&db, "v15ws-deleted");
    let source = raw_database_records(&db);
    let source_bytes = fs::read(&db).unwrap();
    let healed = run_br(
        &workspace,
        [
            "doctor",
            "migrate-schema",
            "heal",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "orphan_reviewed_heal",
    );
    assert!(
        healed.status.success(),
        "{} {}",
        healed.stdout,
        healed.stderr
    );
    assert_eq!(header_user_version(&db), current_version());
    let migrated = raw_database_records(&db);
    for table in [
        "events",
        "comments",
        "labels",
        "dependencies",
        "child_counters",
    ] {
        assert_eq!(migrated[table], source[table], "migration changed {table}");
    }
    let backup = walk(&workspace.root.join(".beads/.br_recovery/schema-migrations"))
        .into_iter()
        .find(|path| fs::read(path).is_ok_and(|bytes| bytes == source_bytes))
        .expect("migration retains exact original database backup");
    assert_eq!(raw_database_records(&backup), source);
    // The reviewed typed-key conversion correctly enforces the owner's FK.
    // Add damaged dependency endpoints only after proving that migration kept
    // all supported legacy orphan records and every original dependency row.
    seed_orphan_dependency_endpoints(&db, "v15ws-9nx", "v15ws-deleted");
    assert_automatic_import_keeps_orphans(&mut workspace, &db, "v15ws-9nx");
}

/// A pre-0.6.0 database holding the label and comment of a deleted issue used
/// to fail the stale-schema audit outright ("contains rows without an owning
/// issue"), so every mutating command refused and even
/// `heal --discard-db-only` failed. The reviewed in-place migration keeps those
/// rows, so the ordinary automatic heal now proceeds without dropping them.
#[test]
fn orphan_relation_rows_heal_through_the_reviewed_migration_and_are_kept() {
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
    add_orphan_relations(&db_path, "v15ws-deleted");
    assert_eq!(header_user_version(&db_path), 16);

    let dry = run_br(
        &workspace,
        ["doctor", "migrate-schema", "heal", "--dry-run", "--json"],
        "orphans16_dry_run",
    );
    assert!(dry.status.success(), "{}", dry.stderr);
    let dry = json_stdout(&dry.stdout);
    assert_eq!(
        dry["audit"]["orphan_relations"],
        serde_json::json!([
            {"table": "labels", "rows": 1},
            {"table": "comments", "rows": 1}
        ]),
        "{dry}"
    );
    assert_eq!(dry["automatic_heal_allowed"], true, "{dry}");

    let create = run_br(
        &workspace,
        [
            "create",
            "Created after orphan heal",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "orphans16_create",
    );
    assert!(
        create.status.success(),
        "create must heal and proceed: {}",
        create.stderr
    );
    assert!(
        create.stderr.contains("migrated it in place"),
        "{}",
        create.stderr
    );
    assert_eq!(header_user_version(&db_path), current_version());
    assert_eq!(
        relation_rows(&db_path, "v15ws-deleted"),
        (1, 1),
        "the in-place migration keeps every row"
    );
}

/// Orphans do not justify replacing the rest of an undamaged tracker.
/// Neither mode nor the old discard flag supplies a reviewed migration.
#[test]
fn orphan_relation_rows_never_authorize_an_unreviewed_rebuild() {
    let workspace = BrWorkspace::new();
    let db_path = install(
        &workspace,
        "schema4_v0127_clean.db.gz",
        "schema4_v0127.issues.jsonl",
        "leg",
    );
    add_orphan_relations(&db_path, "leg-deleted");
    add_unreviewed_dependency_index(&db_path);
    assert_eq!(header_user_version(&db_path), 4);
    let before = protected_files(&workspace);

    let create = run_br(
        &workspace,
        ["create", "Must not run", "--json"],
        "orphans4_create",
    );
    assert!(!create.status.success(), "create must refuse");
    // With --json the refusal is a JSON error on stdout.
    let refusal = format!("{}{}", create.stdout, create.stderr);
    assert!(
        refusal.contains("migration") && refusal.contains("doctor"),
        "{refusal}"
    );
    assert_eq!(header_user_version(&db_path), 4, "nothing changed");

    let heal = run_br(
        &workspace,
        ["doctor", "migrate-schema", "heal", "--json"],
        "orphans4_heal",
    );
    assert!(!heal.status.success(), "plain heal must refuse");
    let refusal = format!("{}{}", heal.stdout, heal.stderr);
    assert!(
        refusal.contains("refused") && refusal.contains("migration"),
        "{refusal}"
    );
    assert_eq!(header_user_version(&db_path), 4, "nothing changed");
    assert_eq!(relation_rows(&db_path, "leg-deleted"), (1, 1));

    let heal = run_br(
        &workspace,
        [
            "doctor",
            "migrate-schema",
            "heal",
            "--discard-db-only",
            "--json",
        ],
        "orphans4_heal_discard",
    );
    assert!(!heal.status.success(), "{} {}", heal.stdout, heal.stderr);
    assert_eq!(header_user_version(&db_path), 4);
    assert_eq!(relation_rows(&db_path, "leg-deleted"), (1, 1));
    assert_eq!(event_count(&db_path), 5);
    assert_eq!(protected_files(&workspace), before);
    let issue = show(&workspace, "leg-n57");
    assert_eq!(issue["labels"], serde_json::json!(["legacy-label"]));
    assert_eq!(protected_files(&workspace), before);
    assert!(!workspace.root.join(".beads/.br_recovery").exists());
}

/// #585's five ordinary reads, expanded to the flagged info regression and
/// relation commands. Each runs below the tracker, not with a BEADS_DIR hint.
#[test]
fn gh585_nested_read_matrix_never_upgrades_or_rebuilds_old_databases() {
    let commands = [
        vec!["info", "--json"],
        vec!["list", "--json"],
        vec!["ready", "--json"],
        vec!["stats", "--json"],
        vec!["show", "v15ws-9nx", "--json"],
        vec!["blocked", "--json"],
        vec!["dep", "list", "v15ws-z1u", "--json"],
        vec!["dep", "tree", "v15ws-z1u", "--direction=down", "--json"],
    ];
    for (version, refused_index) in [(0, false), (4, false), (7, false), (16, true), (17, false)] {
        let (mut workspace, db) = synthetic_tracker(version, 5, refused_index);
        let beads_dir = workspace.root.join(".beads");
        let before = tracker_files(&beads_dir);
        let config = query_copy(&db, "SELECT key, value FROM config ORDER BY key");
        assert_eq!(config.len(), 4);
        assert_eq!(event_count(&db), 5);
        let nested = workspace.root.join("project/nested");
        fs::create_dir_all(&nested).unwrap();
        assert!(!nested.join(".beads").exists());
        workspace.root = nested;

        // The optimization switch must not decide whether explicitly
        // observational commands have permission to rewrite user data.
        for mode in 0..3 {
            for (index, command) in commands.iter().enumerate() {
                let mut args = command.clone();
                if mode != 0 {
                    args.extend(["--no-auto-import", "--no-auto-flush"]);
                }
                let label = format!("gh585_read_v{version}_mode{mode}_{index}");
                let run = if mode == 2 {
                    run_br_with_env(
                        &workspace,
                        args,
                        [("BR_DISABLE_READ_ONLY_FAST_OPEN", "1")],
                        &label,
                    )
                } else {
                    run_br(&workspace, args, &label)
                };
                assert!(
                    run.status.success(),
                    "{label}: the supported JSONL read fallback must stay usable: {} {}",
                    run.stdout,
                    run.stderr
                );
                let _ = json_stdout(&run.stdout);
                if command[0] != "info" {
                    assert!(
                        run.stderr.contains("schema") && !run.stderr.contains("rebuilt it"),
                        "{label}: explain the stale source without claiming migration: {}",
                        run.stderr
                    );
                }
                assert_tracker_unchanged(&beads_dir, &before);
                assert_eq!(header_user_version(&db), version);
                assert!(!beads_dir.join(".br_recovery").exists());
            }
        }
        assert_eq!(event_count(&db), 5);
        assert_eq!(
            query_copy(&db, "SELECT key, value FROM config ORDER BY key"),
            config
        );
    }
}

#[test]
fn gh585_database_authority_reads_never_invent_empty_history_or_default_config() {
    let (workspace, db) = synthetic_tracker(16, 5, true);
    let connection = Connection::open(db.to_string_lossy().into_owned()).unwrap();
    for sql in [
        "INSERT INTO config (key, value) VALUES ('saved_query:local', \
         '{\"name\":\"local\",\"created_at\":\"2026-10-31T00:00:00Z\",\"filters\":{\"status\":[\"open\"]}}')",
        "INSERT OR REPLACE INTO dirty_issues (issue_id, marked_at) VALUES ('v15ws-9nx', '2026-10-31T00:00:00Z')",
        "UPDATE metadata SET value = '1' WHERE key = 'needs_flush'",
    ] {
        connection.execute(sql).unwrap();
    }
    connection.close().unwrap();
    let before = protected_files(&workspace);
    for (index, mut args) in [
        vec!["audit", "log", "v15ws-9nx", "--json"],
        vec!["audit", "summary", "--json"],
        vec!["query", "list", "--json"],
        vec!["query", "run", "local", "--json"],
        vec!["sync", "--status", "--json"],
        vec!["sync", "--reconcile", "--dry-run", "--json"],
        vec!["config", "get", "compact_model", "--json"],
        vec!["config", "list", "--json"],
    ]
    .into_iter()
    .enumerate()
    {
        args.extend(["--no-auto-import", "--no-auto-flush"]);
        let run = run_br(&workspace, args, &format!("gh585_db_authority_{index}"));
        if index < 6 {
            assert!(
                !run.status.success(),
                "must refuse an incomplete view of database authority: {}",
                run.stdout
            );
            let error = json_stdout(&run.stdout)["error"]["message"]
                .as_str()
                .expect("structured refusal")
                .to_owned();
            assert!(
                error.contains("schema") && error.contains("doctor"),
                "{error}"
            );
        } else {
            assert!(run.status.success(), "{} {}", run.stdout, run.stderr);
            let config = json_stdout(&run.stdout);
            if index == 6 {
                assert_eq!(config["value"], "local-model");
            } else {
                assert_eq!(config["compact_model"], "local-model");
                assert_eq!(config["compact_batch_size"], "37");
                assert_eq!(config["compaction_enabled"], "false");
                assert!(
                    config["saved_query:local"]
                        .as_str()
                        .is_some_and(|query| query.contains("local"))
                );
            }
        }
        assert_eq!(protected_files(&workspace), before);
        assert_eq!(event_count(&db), 5);
    }
}

#[test]
fn gh585_explicit_previews_never_upgrade_older_databases() {
    // The released schema16 has a reviewed migration; stamping its newer
    // layout as schema4 does not match the separately reviewed release4 shape.
    // The preview contract forbids either kind of upgrade before dispatch.
    for version in [4, 16] {
        let (workspace, db) = synthetic_tracker(version, 5, false);
        let before = protected_files(&workspace);
        for (index, command) in [
            vec!["create", "Proposed issue", "--dry-run", "--json"],
            vec!["delete", "v15ws-9nx", "--dry-run", "--json"],
            vec!["delete", "--hard", "--dry-run", "--json"],
            vec!["epic", "close-eligible", "--dry-run", "--json"],
        ]
        .into_iter()
        .enumerate()
        {
            for explicit_flags in [false, true] {
                let mut args = command.clone();
                if explicit_flags {
                    args.extend(["--no-auto-import", "--no-auto-flush"]);
                }
                let run = run_br(
                    &workspace,
                    args,
                    &format!("gh585_preview_v{version}_{index}_{explicit_flags}"),
                );
                let output = json_stdout(&run.stdout);
                if !run.status.success() {
                    let message = output["error"]["message"]
                        .as_str()
                        .expect("actionable preview refusal");
                    assert!(
                        message.contains("schema") && message.contains("doctor"),
                        "{message}"
                    );
                }
                assert_eq!(
                    protected_files(&workspace),
                    before,
                    "preview changed schema {version}: {run:?}"
                );
                assert_eq!(header_user_version(&db), version);
                assert!(!workspace.root.join(".beads/.br_recovery").exists());
            }
        }
        assert_eq!(event_count(&db), 5);
    }
}

#[test]
fn gh585_failed_or_missing_migration_preserves_every_database_only_record() {
    for (version, refused_index) in [(0, false), (4, false), (7, false), (16, true)] {
        let (workspace, db) = synthetic_tracker(version, 5, refused_index);
        let connection = Connection::open(db.to_string_lossy().into_owned()).unwrap();
        for sql in [
            "INSERT INTO comments (issue_id, author, text, created_at) \
             VALUES ('v15ws-z1u', 'local-agent', 'comment absent from JSONL', '2026-10-31T00:00:00Z')",
            "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by, metadata, thread_id) \
             VALUES ('v15ws-9nx', 'v15ws-4kf', 'related', '2026-10-31T00:00:00Z', \
                     'local-agent', '{\"why\":\"database-only edge\"}', 'unexported-thread')",
            "INSERT INTO labels (issue_id, label) VALUES ('v15ws-z1u', 'unexported-label')",
            "INSERT INTO metadata (key, value) VALUES ('operator_state', 'never exported')",
            "CREATE TABLE extension_authority (key TEXT PRIMARY KEY, payload BLOB NOT NULL)",
            "INSERT INTO extension_authority VALUES ('private', X'0001FF00')",
        ] {
            connection.execute(sql).unwrap();
        }
        connection.close().unwrap();
        let before = protected_files(&workspace);
        let tables = [
            "events",
            "config",
            "comments",
            "dependencies",
            "labels",
            "metadata",
            "extension_authority",
        ];
        let expected: Vec<_> = tables
            .iter()
            .map(|table| query_copy(&db, &format!("SELECT * FROM {table} ORDER BY rowid")))
            .collect();

        for (index, args) in [
            vec!["create", "must not run", "--json"],
            vec!["doctor", "migrate-schema", "heal", "--json"],
            vec![
                "doctor",
                "migrate-schema",
                "heal",
                "--discard-db-only",
                "--json",
            ],
        ]
        .into_iter()
        .enumerate()
        {
            let run = run_br(
                &workspace,
                args,
                &format!("gh585_refused_v{version}_{index}"),
            );
            assert!(!run.status.success(), "{} {}", run.stdout, run.stderr);
            let message = json_stdout(&run.stdout)["error"]["message"]
                .as_str()
                .expect("actionable refusal")
                .to_owned();
            assert!(
                message.contains("preserv") || message.contains("migration"),
                "{message}"
            );
            assert_eq!(header_user_version(&db), version);
            // A reviewed attempt may retain its private diagnostics, but
            // never changes any existing source or recovery artifact.
            let after = protected_files(&workspace);
            for (path, bytes) in &before {
                assert_eq!(after.get(path), Some(bytes), "changed {path:?}: {message}");
            }
            for (table, rows) in tables.iter().zip(&expected) {
                assert_eq!(
                    query_copy(&db, &format!("SELECT * FROM {table} ORDER BY rowid")),
                    *rows,
                    "lost {table} on schema {version}"
                );
            }
        }
    }
}

#[test]
fn gh585_old_schema_readers_do_not_take_writer_or_replacement_authority() {
    let (workspace, db) = synthetic_tracker(16, 5, true);
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(workspace.root.join(".beads/.write.lock"))
        .unwrap();
    lock.lock().expect("hold writer lock");
    let _peer = beads_rust::sync::DatabaseOpenerLease::register(&db).unwrap();
    let before = protected_files(&workspace);
    let before_reads = tracker_files(&workspace.root.join(".beads"));
    std::thread::scope(|scope| {
        for (index, command) in ["list", "ready", "stats"].into_iter().enumerate() {
            let workspace = &workspace;
            scope.spawn(move || {
                let run = run_br(
                    workspace,
                    [command, "--json", "--lock-timeout", "50"],
                    &format!("gh585_concurrent_read_{index}"),
                );
                assert!(
                    run.status.success(),
                    "{command}: {} {}",
                    run.stdout,
                    run.stderr
                );
                let _ = json_stdout(&run.stdout);
            });
        }
    });
    // A stable source can be inspected through a verified private snapshot
    // even while a writer holds its advisory lock. The read must not acquire
    // writer or replacement authority merely to inspect historical tables.
    let info = run_br(
        &workspace,
        ["info", "--json", "--lock-timeout", "50"],
        "gh585_info_under_writer",
    );
    assert!(info.status.success(), "{} {}", info.stdout, info.stderr);
    assert_eq!(
        json_stdout(&info.stdout)["config"]["compact_model"],
        "local-model"
    );
    assert_tracker_unchanged(&workspace.root.join(".beads"), &before_reads);
    let mutation = run_br(
        &workspace,
        [
            "create",
            "Writer lock is still authoritative",
            "--json",
            "--lock-timeout",
            "50",
        ],
        "gh585_mutation_under_writer",
    );
    assert!(
        !mutation.status.success(),
        "mutation must still honor the writer lock: {} {}",
        mutation.stdout,
        mutation.stderr
    );
    assert!(
        format!("{} {}", mutation.stdout, mutation.stderr)
            .to_lowercase()
            .contains("lock"),
        "{} {}",
        mutation.stdout,
        mutation.stderr
    );
    assert_eq!(protected_files(&workspace), before);
    assert!(!workspace.root.join(".beads/.br_recovery").exists());
    drop(lock);
    let info = run_br(&workspace, ["info", "--json"], "gh585_info_after_writer");
    assert!(info.status.success(), "{} {}", info.stdout, info.stderr);
    assert_eq!(
        json_stdout(&info.stdout)["config"]["compact_model"],
        "local-model"
    );
    assert_eq!(protected_files(&workspace), before);
    assert_eq!(event_count(&db), 5);
}

#[test]
fn gh585_reviewed_migration_waits_for_live_opener_without_reconstructing() {
    let (workspace, db) = synthetic_tracker(16, 5, false);
    let before = protected_files(&workspace);
    let original_events = query_copy(&db, "SELECT * FROM events ORDER BY id");
    let original_config = query_copy(&db, "SELECT key, value FROM config ORDER BY key");
    let peer = beads_rust::sync::DatabaseOpenerLease::register(&db).unwrap();
    let refused = run_br(
        &workspace,
        [
            "create",
            "Wait for the existing reader",
            "--json",
            "--lock-timeout",
            "50",
        ],
        "gh585_migration_live_opener",
    );
    assert!(
        !refused.status.success(),
        "{} {}",
        refused.stdout,
        refused.stderr
    );
    let refusal = json_stdout(&refused.stdout)["error"]["message"]
        .as_str()
        .expect("migration contention diagnosis")
        .to_owned();
    assert!(
        refusal.contains("opener") || refusal.contains("lease"),
        "{refusal}"
    );
    assert_eq!(header_user_version(&db), 16);
    let after = protected_files(&workspace);
    for (path, bytes) in &before {
        assert_eq!(after.get(path), Some(bytes), "changed {path:?}");
    }
    assert_eq!(
        query_copy(&db, "SELECT * FROM events ORDER BY id"),
        original_events
    );
    assert_eq!(
        query_copy(&db, "SELECT key, value FROM config ORDER BY key"),
        original_config
    );

    drop(peer);
    let created = run_br(
        &workspace,
        [
            "create",
            "Created after the reader released its lease",
            "--json",
        ],
        "gh585_migration_after_opener",
    );
    assert!(
        created.status.success(),
        "{} {}",
        created.stdout,
        created.stderr
    );
    assert_eq!(header_user_version(&db), current_version());
    let migrated_events = query_copy(&db, "SELECT * FROM events ORDER BY id");
    assert!(
        original_events
            .iter()
            .all(|event| migrated_events.contains(event))
    );
    assert_eq!(
        query_copy(&db, "SELECT key, value FROM config ORDER BY key"),
        original_config
    );
    assert!(
        workspace
            .root
            .join(".beads/.br_recovery/schema-migrations")
            .is_dir()
    );
}

#[test]
fn historical_dependency_indexes_migrate_without_losing_database_only_records() {
    let (mut workspace, db) = synthetic_tracker(16, 9, false);
    seed_migration_only_records(&db);
    edit_fixture(
        &db,
        &[
            "CREATE INDEX idx_dependencies_depends_on_type_issue ON dependencies(depends_on_id, type, issue_id)",
            "CREATE INDEX idx_dependencies_issue_type ON dependencies(issue_id, type)",
        ],
    );
    let original_db = fs::read(&db).unwrap();
    let records = migration_only_records(&db);
    assert_eq!(records["events"].len(), 9);
    assert_eq!(records["config"].len(), 4);
    let beads_dir = workspace.root.join(".beads");
    let jsonl = fs::read(beads_dir.join("issues.jsonl")).unwrap();
    move_to_nested_directory(&mut workspace);

    // A real mutation triggers the reviewed upgrade. Disable synchronization
    // so these checks distinguish migration preservation from JSONL import.
    let result = run_br(
        &workspace,
        [
            "create",
            "Created after the historical-index upgrade",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "historical_indexes_create",
    );
    assert!(
        result.status.success(),
        "{} {}",
        result.stdout,
        result.stderr
    );
    assert!(
        result.stderr.contains("migrated it in place"),
        "{}",
        result.stderr
    );
    assert!(!result.stderr.contains("rebuilt it from the JSONL"));
    assert_eq!(header_user_version(&db), current_version());
    assert_migration_kept_records(&db, &records);
    assert_eq!(fs::read(beads_dir.join("issues.jsonl")).unwrap(), jsonl);
    assert!(
        query_copy(
            &db,
            "SELECT name FROM sqlite_master WHERE type = 'index' AND name IN \
             ('idx_dependencies_depends_on_type_issue', 'idx_dependencies_issue_type')"
        )
        .is_empty(),
        "the reviewed pair-key accelerators are retired during typed-key migration"
    );

    let backup = walk(&beads_dir.join(".br_recovery/schema-migrations"))
        .into_iter()
        .find(|path| fs::read(path).is_ok_and(|bytes| bytes == original_db))
        .expect("successful migration retains a byte-identical source backup");
    assert_eq!(header_user_version(&backup), 16);
    assert_eq!(migration_only_records(&backup), records);
}

#[test]
fn schema17_migration_preserves_capacity_occupancy_and_history() {
    // synthetic_tracker adds schema 17's real occupancy table and indexes to
    // the released v16 fixture, not only a different user_version stamp.
    let (workspace, db) = synthetic_tracker(17, 9, false);
    seed_migration_only_records(&db);
    edit_fixture(
        &db,
        &[
            "INSERT INTO capacity_occupancy (issue_id, actor, agent_name, harness, session) \
             VALUES ('v15ws-9nx', 'operator', 'migration-agent', 'cli', 'retained-session')",
        ],
    );
    let records = migration_only_records(&db);
    let occupancy = query_copy(&db, "SELECT * FROM capacity_occupancy ORDER BY issue_id");
    assert_eq!(occupancy.len(), 1);
    let beads_dir = workspace.root.join(".beads");
    let jsonl = fs::read(beads_dir.join("issues.jsonl")).unwrap();
    let result = run_br(
        &workspace,
        [
            "create",
            "Created after the schema-17 upgrade",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "schema17_create",
    );
    assert!(
        result.status.success(),
        "{} {}",
        result.stdout,
        result.stderr
    );
    assert!(
        result.stderr.contains("migrated it in place"),
        "{}",
        result.stderr
    );
    assert_eq!(header_user_version(&db), current_version());
    assert_migration_kept_records(&db, &records);
    assert_eq!(
        query_copy(&db, "SELECT * FROM capacity_occupancy ORDER BY issue_id"),
        occupancy
    );
    assert_eq!(fs::read(beads_dir.join("issues.jsonl")).unwrap(), jsonl);
    assert!(beads_dir.join(".br_recovery/schema-migrations").is_dir());
}

#[test]
fn schema_migration_previews_preserve_nested_tracker_and_keep_plan_token_applicable() {
    let (mut workspace, db) = synthetic_tracker(16, 5, false);
    seed_migration_only_records(&db);
    let beads_dir = workspace.root.join(".beads");
    let records = migration_only_records(&db);
    move_to_nested_directory(&mut workspace);
    let before = tracker_files(&beads_dir);
    let plan = run_br_with_env(
        &workspace,
        [
            "doctor",
            "migrate-schema",
            "plan",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        [("BR_DISABLE_READ_ONLY_FAST_OPEN", "1")],
        "observational_migration_plan",
    );
    assert!(plan.status.success(), "{} {}", plan.stdout, plan.stderr);
    let plan = json_stdout(&plan.stdout);
    assert_eq!(plan["eligible"], true, "{plan}");
    assert_eq!(Path::new(plan["database_path"].as_str().unwrap()), db);
    let token = plan["plan_token"].as_str().expect("applicable plan token");
    assert_tracker_unchanged(&beads_dir, &before);

    let heal = run_br_with_env(
        &workspace,
        [
            "doctor",
            "migrate-schema",
            "heal",
            "--dry-run",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        [("BR_DISABLE_READ_ONLY_FAST_OPEN", "1")],
        "observational_heal_plan",
    );
    assert!(heal.status.success(), "{} {}", heal.stdout, heal.stderr);
    let _ = json_stdout(&heal.stdout);
    assert_tracker_unchanged(&beads_dir, &before);
    assert_eq!(header_user_version(&db), 16);
    assert!(!beads_dir.join(".br_recovery").exists());

    let applied = run_br(
        &workspace,
        [
            "doctor",
            "migrate-schema",
            "apply",
            "--plan-token",
            token,
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "apply_observational_plan",
    );
    assert!(
        applied.status.success(),
        "{} {}",
        applied.stdout,
        applied.stderr
    );
    let applied = json_stdout(&applied.stdout);
    let run_id = applied["run_id"].as_str().unwrap();
    assert_eq!(header_user_version(&db), current_version());
    assert_eq!(migration_only_records(&db), records);
    let after_apply = tracker_files(&beads_dir);
    let undo = run_br_with_env(
        &workspace,
        [
            "doctor",
            "migrate-schema",
            "undo",
            run_id,
            "--dry-run",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        [("BR_DISABLE_READ_ONLY_FAST_OPEN", "1")],
        "observational_undo_plan",
    );
    assert!(undo.status.success(), "{} {}", undo.stdout, undo.stderr);
    assert_eq!(json_stdout(&undo.stdout)["dry_run"], true);
    assert_tracker_unchanged(&beads_dir, &after_apply);
    assert_eq!(header_user_version(&db), current_version());
}

#[test]
fn refused_schema_migration_previews_keep_the_complete_legacy_tracker() {
    let mut workspace = BrWorkspace::new();
    let db = install(
        &workspace,
        "schema4_v0127_clean.db.gz",
        "schema4_v0127.issues.jsonl",
        "leg",
    );
    add_unreviewed_dependency_index(&db);
    let beads_dir = workspace.root.join(".beads");
    let before = tracker_files(&beads_dir);
    move_to_nested_directory(&mut workspace);
    for mut command in [
        vec!["doctor", "migrate-schema", "plan", "--json"],
        vec!["doctor", "migrate-schema", "heal", "--dry-run", "--json"],
    ] {
        command.extend(["--no-auto-import", "--no-auto-flush"]);
        let preview = run_br_with_env(
            &workspace,
            command,
            [("BR_DISABLE_READ_ONLY_FAST_OPEN", "1")],
            "refused_observational_migration_plan",
        );
        let report = json_stdout(&preview.stdout);
        assert!(
            report["eligible"] == false
                || report["automatic_heal_allowed"] == false
                || report["error"].is_object(),
            "an unsupported schema must remain refused: {report}"
        );
        assert_tracker_unchanged(&beads_dir, &before);
        assert_eq!(header_user_version(&db), 4);
        assert!(!beads_dir.join(".br_recovery").exists());
    }
}

#[test]
fn gh585_unreadable_schema_probe_cannot_fall_through_to_startup_reconstruction() {
    let (mut workspace, db) = synthetic_tracker(16, 5, false);
    let beads_dir = workspace.root.join(".beads");
    let mut damaged = fs::read(&db).unwrap();
    // Keep the valid SQLite header and stale user_version but make the first
    // b-tree page invalid. Schema detection now fails during the engine probe
    // instead of returning a usable version, exercising the other startup
    // branch. Recovery evidence still contains the complete source.
    let evidence = beads_dir.join(".br_recovery/gh585-original");
    fs::create_dir_all(&evidence).unwrap();
    fs::write(evidence.join("beads.db"), &damaged).unwrap();
    let wal = beads_dir.join("beads.db-wal");
    if wal.is_file() {
        fs::rename(wal, evidence.join("beads.db-wal")).unwrap();
    }
    damaged[100] = 0xff;
    fs::write(&db, damaged).unwrap();
    let before = tracker_files(&beads_dir);
    move_to_nested_directory(&mut workspace);

    for (index, args) in [
        vec!["list", "--json"],
        vec!["list", "--json", "--no-auto-import", "--no-auto-flush"],
        vec!["info", "--json", "--no-auto-import", "--no-auto-flush"],
    ]
    .into_iter()
    .enumerate()
    {
        let run = run_br_with_env(
            &workspace,
            args,
            [("BR_DISABLE_READ_ONLY_FAST_OPEN", "1")],
            &format!("gh585_unreadable_source_{index}"),
        );
        if index < 2 {
            assert!(
                !run.status.success(),
                "unreadable authority must refuse: {}",
                run.stdout
            );
            assert!(json_stdout(&run.stdout)["error"].is_object());
        }
        assert_tracker_unchanged(&beads_dir, &before);
        assert_eq!(header_user_version(&db), 16);
    }
    assert_eq!(event_count(&evidence.join("beads.db")), 5);
}

#[test]
fn released_legacy_info_keeps_the_complete_tracker_unchanged_with_fast_open_disabled() {
    for (version, db_gz, issues, prefix, issue_count) in [
        (
            4,
            "schema4_v0127_clean.db.gz",
            "schema4_v0127.issues.jsonl",
            "leg",
            2,
        ),
        (
            16,
            "schema16_v0219_release.db.gz",
            "schema16_issues.jsonl",
            "v15ws",
            3,
        ),
    ] {
        let mut workspace = BrWorkspace::new();
        let db = install(&workspace, db_gz, issues, prefix);
        let beads_dir = workspace.root.join(".beads");
        let before = tracker_files(&beads_dir);
        for nested in [false, true] {
            if nested {
                move_to_nested_directory(&mut workspace);
            }
            let result = run_br_with_env(
                &workspace,
                [
                    "info",
                    "--schema",
                    "--json",
                    "--no-auto-import",
                    "--no-auto-flush",
                ],
                [("BR_DISABLE_READ_ONLY_FAST_OPEN", "1")],
                &format!("released_info_{version}_{nested}"),
            );
            assert!(
                result.status.success(),
                "{} {}",
                result.stdout,
                result.stderr
            );
            let info = json_stdout(&result.stdout);
            assert_eq!(info["issue_count"], issue_count);
            assert_eq!(info["schema"]["schema_version"], version.to_string());
            assert_eq!(header_user_version(&db), version);
            assert_tracker_unchanged(&beads_dir, &before);
            assert!(!beads_dir.join(".br_recovery").exists());
        }
    }
}

#[test]
fn explicit_reads_do_not_repair_a_current_database_with_missing_runtime_state() {
    for disable_fast_open in [false, true] {
        let mut workspace = BrWorkspace::new();
        let init = run_br(&workspace, ["init", "--prefix", "current"], "current_init");
        assert!(init.status.success(), "{} {}", init.stdout, init.stderr);
        let create = run_br(
            &workspace,
            ["create", "Current fixture", "--json"],
            "current_create",
        );
        assert!(
            create.status.success(),
            "{} {}",
            create.stdout,
            create.stderr
        );
        let db = workspace.root.join(".beads/beads.db");
        edit_fixture(&db, &["DROP TABLE blocked_issues_cache"]);
        let beads_dir = workspace.root.join(".beads");
        let before = tracker_files(&beads_dir);
        move_to_nested_directory(&mut workspace);
        for command in ["info", "list", "ready"] {
            let result = run_br_with_env(
                &workspace,
                [command, "--json", "--no-auto-import", "--no-auto-flush"],
                [(
                    "BR_DISABLE_READ_ONLY_FAST_OPEN",
                    if disable_fast_open { "1" } else { "0" },
                )],
                &format!("current_observational_{command}"),
            );
            // Generic diagnostics can inspect this source. A query requiring
            // the absent runtime table can refuse, but cannot repair it.
            if command == "info" {
                assert!(
                    result.status.success(),
                    "{} {}",
                    result.stdout,
                    result.stderr
                );
            }
            let _ = json_stdout(&result.stdout);
            assert_tracker_unchanged(&beads_dir, &before);
            assert_eq!(header_user_version(&db), current_version());
            assert!(!beads_dir.join(".br_recovery").exists());
        }
    }
}

#[test]
fn explicit_reads_never_reconstruct_a_missing_or_unreadable_database() {
    for corrupt in [false, true] {
        let mut workspace = BrWorkspace::new();
        let db = install(
            &workspace,
            "schema4_v0127_clean.db.gz",
            "schema4_v0127.issues.jsonl",
            "leg",
        );
        let retained = workspace.root.join("retained-original.db");
        fs::rename(&db, &retained).expect("retain original fixture");
        if corrupt {
            fs::write(&db, b"unreadable database must remain untouched").unwrap();
        }
        let beads_dir = workspace.root.join(".beads");
        let before = tracker_files(&beads_dir);
        move_to_nested_directory(&mut workspace);
        for disable_fast_open in [false, true] {
            for command in ["info", "list"] {
                let result = run_br_with_env(
                    &workspace,
                    [command, "--json", "--no-auto-import", "--no-auto-flush"],
                    [(
                        "BR_DISABLE_READ_ONLY_FAST_OPEN",
                        if disable_fast_open { "1" } else { "0" },
                    )],
                    &format!("unavailable_db_{command}_{disable_fast_open}"),
                );
                assert!(
                    !result.stderr.contains("rebuilt it from the JSONL"),
                    "{} {}",
                    result.stdout,
                    result.stderr
                );
                let _ = json_stdout(&result.stdout);
                assert_tracker_unchanged(&beads_dir, &before);
                assert!(!beads_dir.join(".br_recovery").exists());
            }
        }
        assert_eq!(header_user_version(&retained), 4);
    }
}

/// Reproduce the investigation's scale and documented version distribution
/// using generated data, without claiming access to the reporter's trackers.
#[test]
fn gh585_fifty_tracker_cohort_keeps_all_5251_events_in_active_databases() {
    let versions: Vec<u32> = std::iter::repeat_n(0, 19)
        .chain([7, 7, 4])
        .chain(std::iter::repeat_n(17, 10))
        .chain([16])
        // The report did not classify these seventeen sources. Represent
        // them as refusal controls with unmistakable DB-only issue state.
        .chain([1, 2, 3, 5, 6, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 0])
        .collect();
    assert_eq!(versions.len(), 50);
    let mut events_before = 0;
    let mut events_after = 0;
    for (index, version) in versions.into_iter().enumerate() {
        let events = match index {
            0..=4 => 875,
            5 => 876,
            _ => 0,
        };
        let (mut workspace, db) = synthetic_tracker(version, events, index >= 33);
        if index >= 33 {
            let connection = Connection::open(db.to_string_lossy().into_owned()).unwrap();
            connection
                .execute(
                    "UPDATE issues SET title = 'Unflushed negative-control edit', \
                 updated_at = '2999-01-01T00:00:00Z' WHERE id = 'v15ws-9nx'",
                )
                .unwrap();
            connection
                .execute(
                    "INSERT OR REPLACE INTO dirty_issues (issue_id, marked_at) \
                 VALUES ('v15ws-9nx', '2999-01-01T00:00:00Z')",
                )
                .unwrap();
            connection.close().unwrap();
        }
        let beads_dir = workspace.root.join(".beads");
        let before = protected_files(&workspace);
        events_before += event_count(&db);
        let config = query_copy(&db, "SELECT key, value FROM config ORDER BY key");
        assert_eq!(config.len(), 4);
        let nested = workspace.root.join("nested/project");
        fs::create_dir_all(&nested).unwrap();
        workspace.root = nested;
        let run = run_br(
            &workspace,
            ["info", "--json", "--no-auto-import", "--no-auto-flush"],
            &format!("gh585_cohort_{index}"),
        );
        assert!(
            run.status.success(),
            "tracker {index}: {} {}",
            run.stdout,
            run.stderr
        );
        let info = json_stdout(&run.stdout);
        assert_eq!(info["issue_count"], 3, "tracker {index}: {info}");
        assert_eq!(info["config"]["compact_model"], "local-model");
        assert_eq!(info["config"]["compact_batch_size"], "37");
        assert_eq!(info["config"]["compaction_enabled"], "false");
        assert_eq!(protected_files_at(&beads_dir), before, "tracker {index}");
        assert_eq!(header_user_version(&db), version);
        assert!(!beads_dir.join(".br_recovery").exists());
        events_after += event_count(&db);
        assert_eq!(
            query_copy(&db, "SELECT key, value FROM config ORDER BY key"),
            config
        );
    }
    assert_eq!(
        events_before, 5251,
        "six populated sources in the 22 formerly rebuilt trackers"
    );
    assert_eq!(
        events_after, events_before,
        "all events must remain in the active databases"
    );
}
