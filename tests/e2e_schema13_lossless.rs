//! CLI qualification of the record-preserving schema-13 hash refresh.
//!
//! The source is a synthetic historical shape: the frozen released schema-16
//! tracker with the three later workflow/capacity tables removed. Its core
//! columns are the schema-13 columns. It deliberately retains old derived
//! hashes, dirty markers, export receipts, and opaque database-only records.

mod common;

use beads_rust::franken_sync::{Connection, SqliteValue};
use common::cli::{BrWorkspace, extract_json_payload, run_br, run_br_with_env};
use flate2::read::GzDecoder;
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

type RawRows = BTreeMap<String, Vec<Vec<SqliteValue>>>;

fn payload(stdout: &str) -> Value {
    serde_json::from_str(&extract_json_payload(stdout)).expect("complete JSON stdout")
}

fn query_copy(db: &Path, sql: &str) -> Vec<Vec<SqliteValue>> {
    let scratch = tempfile::tempdir().unwrap();
    let copy = scratch.path().join("copy.db");
    fs::copy(db, &copy).unwrap();
    for suffix in ["-wal", "-journal"] {
        let source = PathBuf::from(format!("{}{suffix}", db.display()));
        if source.exists() {
            fs::copy(source, scratch.path().join(format!("copy.db{suffix}"))).unwrap();
        }
    }
    let connection = Connection::open(copy.to_string_lossy().into_owned()).unwrap();
    let rows = connection
        .query(sql)
        .unwrap()
        .iter()
        .map(|row| row.values().to_vec())
        .collect();
    connection.close().unwrap();
    rows
}

fn edit(db: &Path, statements: &[&str]) {
    let connection = Connection::open(db.to_string_lossy().into_owned()).unwrap();
    for statement in statements {
        connection.execute(statement).unwrap();
    }
    connection.close().unwrap();
}

fn install() -> (BrWorkspace, PathBuf) {
    let workspace = BrWorkspace::new();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/schema_migration");
    let beads = workspace.root.join(".beads");
    fs::create_dir_all(&beads).unwrap();
    let mut bytes = Vec::new();
    GzDecoder::new(fs::File::open(fixtures.join("schema16_v0219_release.db.gz")).unwrap())
        .read_to_end(&mut bytes)
        .unwrap();
    let db = beads.join("beads.db");
    fs::write(&db, bytes).unwrap();
    fs::copy(
        fixtures.join("schema16_issues.jsonl"),
        beads.join("issues.jsonl"),
    )
    .unwrap();
    fs::write(beads.join("config.yaml"), "issue_prefix: v15ws\n").unwrap();
    edit(
        &db,
        &[
            "PRAGMA foreign_keys = OFF",
            "DROP TABLE gate_result_history",
            "DROP TABLE capacity_exemption_history",
            "DROP TABLE capacity_exemptions",
            "PRAGMA user_version = 13",
            "UPDATE issues SET content_hash = 'legacy-hash-' || id",
            "INSERT INTO dirty_issues (rowid, issue_id, marked_at) VALUES (311, 'v15ws-4kf', '2021-02-03T04:05:06Z'), (719, 'v15ws-9nx', '2022-03-04T05:06:07Z'), (907, 'v15ws-orphan', X'00FF')",
            "UPDATE export_hashes SET content_hash = 'original-export-' || issue_id, exported_at = '2020-01-02T03:04:05Z'",
            "INSERT INTO export_hashes (rowid, issue_id, content_hash, exported_at) VALUES (1207, 'v15ws-orphan', X'FF0001', 'original orphan receipt')",
            "INSERT INTO events (id, issue_id, event_type, actor, old_value, new_value, comment, agent_name, harness, model) VALUES (811, 'v15ws-9nx', 'updated', 'original-actor', 'before', 'after', 'database-only history', 'original-agent', 'original-harness', 'original-model')",
            "INSERT INTO config (rowid, key, value) VALUES (1001, 'compact_model', 'local-model')",
            "INSERT INTO metadata (rowid, key, value) VALUES (9001, 'operator_receipt', X'0001FF00')",
            "INSERT INTO child_counters (rowid, parent_id, last_child) VALUES (401, 'v15ws-9nx', 47)",
            "INSERT INTO gate_results (issue_id, gate, provider, passed, recorded_by) VALUES ('v15ws-9nx', 'review', 'operator', 1, 'original-reviewer')",
            "CREATE TABLE operator_notes (id INTEGER PRIMARY KEY AUTOINCREMENT, note TEXT NOT NULL, payload BLOB NOT NULL)",
            "INSERT INTO operator_notes VALUES (17, 'original operator evidence', X'0001FF00')",
            "CREATE INDEX idx_operator_payload ON operator_notes(payload)",
            "UPDATE sqlite_sequence SET seq = 91 WHERE name = 'operator_notes'",
        ],
    );
    (workspace, db)
}

fn version(db: &Path) -> u32 {
    u32::from_be_bytes(fs::read(db).unwrap()[60..64].try_into().unwrap())
}

fn current_version() -> u32 {
    u32::try_from(beads_rust::storage::schema::CURRENT_SCHEMA_VERSION).unwrap()
}

fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Select every original cell and rowid by name across appended columns.
/// Only the derived issue content_hash is expected to change. Sequence
/// reservations are keyed by table name; new empty history tables may add
/// their own zero reservation without changing any original high-water mark.
fn preservation_queries(db: &Path) -> BTreeMap<String, String> {
    let mut queries = BTreeMap::new();
    for row in query_copy(
        db,
        "SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name",
    ) {
        let table = row[0].as_text().unwrap();
        let name = quoted(table);
        let sql = if table == "sqlite_sequence" {
            let names = query_copy(db, "SELECT name FROM sqlite_sequence ORDER BY name")
                .iter()
                .map(|row| format!("'{}'", row[0].as_text().unwrap().replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(",");
            format!("SELECT name, seq FROM sqlite_sequence WHERE name IN ({names}) ORDER BY name")
        } else {
            let columns = query_copy(db, &format!("PRAGMA table_info({name})"))
                .iter()
                .map(|column| column[1].as_text().unwrap())
                .filter(|column| table != "issues" || *column != "content_hash")
                .map(quoted)
                .collect::<Vec<_>>()
                .join(", ");
            format!("SELECT rowid, {columns} FROM {name} ORDER BY rowid")
        };
        queries.insert(table.to_owned(), sql);
    }
    queries
}

fn preserved_rows(db: &Path, queries: &BTreeMap<String, String>) -> RawRows {
    queries
        .iter()
        .map(|(table, sql)| (table.clone(), query_copy(db, sql)))
        .collect()
}

fn files_in(directory: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, directory: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(root, &path, files);
            } else {
                files.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut files = BTreeMap::new();
    if directory.exists() {
        visit(directory, directory, &mut files);
    }
    files
}

fn move_to_nested(workspace: &mut BrWorkspace) {
    workspace.root = workspace.root.join("project/nested");
    fs::create_dir_all(&workspace.root).unwrap();
}

fn plan(workspace: &BrWorkspace) -> Value {
    let planned = run_br_with_env(
        workspace,
        [
            "doctor",
            "migrate-schema",
            "plan",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        [("BR_DISABLE_READ_ONLY_FAST_OPEN", "1")],
        "schema13_plan",
    );
    assert!(
        planned.status.success(),
        "{} {}",
        planned.stdout,
        planned.stderr
    );
    let receipt = payload(&planned.stdout);
    assert_eq!(receipt["eligible"], true, "{receipt}");
    assert_eq!(receipt["from_version"], 13);
    assert_eq!(
        receipt["forecast"]["content_hash_refresh_profile"],
        "v13-preserve-bookkeeping-v1"
    );
    assert_eq!(receipt["forecast"]["content_hash_rows_rebuilt"], 3);
    receipt
}

fn apply(workspace: &BrWorkspace, token: &str) -> Value {
    let applied = run_br(
        workspace,
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
        "schema13_apply",
    );
    assert!(
        applied.status.success(),
        "{} {}",
        applied.stdout,
        applied.stderr
    );
    payload(&applied.stdout)
}

fn assert_refreshed_hashes(workspace: &BrWorkspace, db: &Path) {
    for row in query_copy(db, "SELECT id, content_hash FROM issues ORDER BY id") {
        let id = row[0].as_text().unwrap();
        let shown = run_br(
            workspace,
            ["show", id, "--json", "--no-auto-import", "--no-auto-flush"],
            &format!("schema13_hash_{id}"),
        );
        assert!(shown.status.success(), "{} {}", shown.stdout, shown.stderr);
        let details: beads_rust::format::IssueDetails =
            serde_json::from_value(payload(&shown.stdout)[0].clone()).unwrap();
        let expected = details.issue.compute_content_hash();
        assert_eq!(
            row[1].as_text(),
            Some(expected.as_str()),
            "incorrect derived hash for {id}"
        );
        assert_ne!(row[1].as_text(), Some(format!("legacy-hash-{id}").as_str()));
    }
    assert!(
        query_copy(
            db,
            "SELECT id FROM issues WHERE prerequisites != '' OR prerequisites IS NULL"
        )
        .is_empty()
    );
}

fn assert_undo_restores_source(
    workspace: &BrWorkspace,
    beads: &Path,
    db: &Path,
    run_id: &str,
    source: &[u8],
) {
    let run_dir = beads.join(".br_recovery/schema-migrations").join(run_id);
    assert!(run_dir.join("applied.json").is_file());
    assert!(
        files_in(&run_dir.join("before"))
            .values()
            .any(|bytes| bytes == source)
    );
    let before = files_in(beads);
    let preview = run_br_with_env(
        workspace,
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
        "schema13_undo_preview",
    );
    assert!(
        preview.status.success(),
        "{} {}",
        preview.stdout,
        preview.stderr
    );
    assert_eq!(payload(&preview.stdout)["dry_run"], true);
    assert_eq!(files_in(beads), before, "undo preview modified the tracker");
    let undone = run_br(
        workspace,
        [
            "doctor",
            "migrate-schema",
            "undo",
            run_id,
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "schema13_undo",
    );
    assert!(
        undone.status.success(),
        "{} {}",
        undone.stdout,
        undone.stderr
    );
    assert_eq!(
        fs::read(db).unwrap(),
        source,
        "undo did not restore exact source bytes"
    );
    assert_eq!(version(db), 13);
    assert!(run_dir.join("undone.json").is_file());
}

#[test]
fn schema13_plan_apply_and_undo_keep_original_bookkeeping_and_raw_records() {
    let (mut workspace, db) = install();
    let beads = workspace.root.join(".beads");
    let source = fs::read(&db).unwrap();
    let source_jsonl = fs::read(beads.join("issues.jsonl")).unwrap();
    let queries = preservation_queries(&db);
    let records = preserved_rows(&db, &queries);
    assert_eq!(records["dirty_issues"].len(), 3);
    assert_eq!(records["export_hashes"].len(), 4);
    move_to_nested(&mut workspace);
    let before = files_in(&beads);
    let planned = plan(&workspace);
    assert_eq!(files_in(&beads), before, "plan modified the source tracker");
    assert_eq!(
        Path::new(planned["database_path"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        db.canonicalize().unwrap()
    );
    let applied = apply(&workspace, planned["plan_token"].as_str().unwrap());
    assert_eq!(version(&db), current_version());
    assert_eq!(
        preserved_rows(&db, &queries),
        records,
        "migration lost original cells or rowids"
    );
    assert_refreshed_hashes(&workspace, &db);
    assert_eq!(fs::read(beads.join("issues.jsonl")).unwrap(), source_jsonl);
    assert_undo_restores_source(
        &workspace,
        &beads,
        &db,
        applied["run_id"].as_str().unwrap(),
        &source,
    );
    assert_eq!(preserved_rows(&db, &queries), records);
    assert_eq!(fs::read(beads.join("issues.jsonl")).unwrap(), source_jsonl);
}

#[test]
fn schema13_automatic_migration_retains_dirty_markers_and_export_receipts() {
    let (mut workspace, db) = install();
    let beads = workspace.root.join(".beads");
    let source = fs::read(&db).unwrap();
    let source_jsonl = fs::read(beads.join("issues.jsonl")).unwrap();
    let queries = preservation_queries(&db);
    let before = preserved_rows(&db, &queries);
    move_to_nested(&mut workspace);
    let created = run_br(
        &workspace,
        [
            "create",
            "Created after preserved schema13 bookkeeping",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "schema13_automatic_create",
    );
    assert!(
        created.status.success(),
        "{} {}",
        created.stdout,
        created.stderr
    );
    assert!(
        created.stderr.contains("migrated it in place"),
        "{}",
        created.stderr
    );
    assert!(!created.stderr.contains("rebuilt it from the JSONL"));
    assert_eq!(version(&db), current_version());
    let after = preserved_rows(&db, &queries);
    for table in [
        "issues",
        "events",
        "comments",
        "labels",
        "dependencies",
        "dirty_issues",
    ] {
        assert!(
            before[table].iter().all(|row| after[table].contains(row)),
            "lost original {table}"
        );
    }
    for table in [
        "config",
        "export_hashes",
        "child_counters",
        "operator_notes",
        "gate_results",
    ] {
        assert_eq!(after[table], before[table], "changed original {table}");
    }
    assert_eq!(
        after["dirty_issues"].len(),
        before["dirty_issues"].len() + 1,
        "hash refresh must not mark unchanged issues dirty"
    );
    assert!(after["events"].len() > before["events"].len());
    assert_eq!(
        query_copy(
            &db,
            "SELECT rowid, key, value FROM metadata WHERE key = 'operator_receipt'"
        ),
        vec![vec![
            SqliteValue::Integer(9001),
            SqliteValue::from("operator_receipt"),
            SqliteValue::Blob(vec![0, 1, 255, 0].into())
        ]]
    );
    assert_eq!(
        query_copy(
            &db,
            "SELECT seq FROM sqlite_sequence WHERE name = 'operator_notes'"
        ),
        vec![vec![SqliteValue::Integer(91)]]
    );
    assert_refreshed_hashes(&workspace, &db);
    assert_eq!(fs::read(beads.join("issues.jsonl")).unwrap(), source_jsonl);
    assert!(
        files_in(&beads.join(".br_recovery/schema-migrations"))
            .values()
            .any(|bytes| bytes == &source)
    );
}

#[test]
fn schema13_malformed_hash_input_refuses_before_any_source_or_recovery_change() {
    let (mut workspace, db) = install();
    edit(
        &db,
        &["UPDATE issues SET assignee = X'00FF' WHERE id = 'v15ws-9nx'"],
    );
    let beads = workspace.root.join(".beads");
    let source = fs::read(&db).unwrap();
    let source_jsonl = fs::read(beads.join("issues.jsonl")).unwrap();
    move_to_nested(&mut workspace);
    let before = files_in(&beads);
    let planned = run_br_with_env(
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
        "schema13_malformed_plan",
    );
    assert!(
        !planned.status.success(),
        "{} {}",
        planned.stdout,
        planned.stderr
    );
    let output = payload(&planned.stdout);
    let message = output["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("assignee") && message.contains("hash refresh refused"),
        "{message}"
    );
    assert_eq!(
        files_in(&beads),
        before,
        "refused plan changed tracker files"
    );
    for command in [
        vec![
            "create",
            "Must not run",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        vec![
            "doctor",
            "migrate-schema",
            "heal",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
    ] {
        let refused = run_br(&workspace, command, "schema13_malformed_mutation");
        assert!(
            !refused.status.success(),
            "{} {}",
            refused.stdout,
            refused.stderr
        );
        assert!(payload(&refused.stdout)["error"].is_object());
        assert_eq!(fs::read(&db).unwrap(), source);
        assert_eq!(fs::read(beads.join("issues.jsonl")).unwrap(), source_jsonl);
        assert_eq!(version(&db), 13);
        assert!(!beads.join(".br_recovery").exists());
    }
}

#[test]
fn schema13_undo_refuses_changes_to_bookkeeping_cells_or_rowids() {
    for (label, change) in [
        (
            "dirty_rowid",
            "UPDATE dirty_issues SET rowid = 1881 WHERE rowid = 311",
        ),
        (
            "export_cell",
            "UPDATE export_hashes SET content_hash = X'000102' WHERE issue_id = 'v15ws-9nx'",
        ),
    ] {
        let (mut workspace, db) = install();
        let beads = workspace.root.join(".beads");
        move_to_nested(&mut workspace);
        let planned = plan(&workspace);
        let applied = apply(&workspace, planned["plan_token"].as_str().unwrap());
        edit(&db, &[change]);
        let before = files_in(&beads);
        let source = fs::read(&db).unwrap();
        let source_jsonl = fs::read(beads.join("issues.jsonl")).unwrap();
        let recovery = files_in(&beads.join(".br_recovery"));
        let run_id = applied["run_id"].as_str().unwrap();
        let preview = run_br(
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
            &format!("schema13_changed_{label}_preview"),
        );
        assert!(
            !preview.status.success(),
            "{} {}",
            preview.stdout,
            preview.stderr
        );
        assert_eq!(files_in(&beads), before, "refused preview changed {label}");
        let undone = run_br(
            &workspace,
            [
                "doctor",
                "migrate-schema",
                "undo",
                run_id,
                "--json",
                "--no-auto-import",
                "--no-auto-flush",
            ],
            &format!("schema13_changed_{label}_undo"),
        );
        assert!(
            !undone.status.success(),
            "{} {}",
            undone.stdout,
            undone.stderr
        );
        assert!(payload(&undone.stdout)["error"].is_object());
        assert_eq!(
            fs::read(&db).unwrap(),
            source,
            "refused undo rewrote {label}"
        );
        assert_eq!(fs::read(beads.join("issues.jsonl")).unwrap(), source_jsonl);
        assert_eq!(
            files_in(&beads.join(".br_recovery")),
            recovery,
            "refused undo changed recovery evidence"
        );
        assert_eq!(version(&db), current_version());
    }
}
