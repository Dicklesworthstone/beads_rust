//! Reopening and automatic import must retain the authority of local records.
//!
//! These tests exercise the production CLI after ordinary current-schema
//! initialization and after a reviewed upgrade of the released schema 16.
//! Raw cells and rowids are inspected on private copies, independently of
//! JSONL export and the migration preservation witness.

mod common;

use beads_rust::franken_sync::{Connection, SqliteValue};
use common::cli::{BrWorkspace, extract_json_payload, run_br, run_br_with_env};
use flate2::read::GzDecoder;
use serde_json::{Value, json};
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

fn raw_records(db: &Path) -> RawRows {
    let mut records = BTreeMap::new();
    records.insert(
        "schema".to_owned(),
        query_copy(
            db,
            "SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY type, name",
        ),
    );
    for row in query_copy(db, "SELECT name FROM sqlite_master WHERE type = 'table'") {
        let table = row[0].as_text().unwrap();
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

fn original_issue() -> Value {
    json!({
        "id": "dur-owner", "title": "Original issue", "status": "open",
        "priority": 2, "issue_type": "task", "created_by": "original-actor",
        "created_at": "2026-01-01T00:00:25Z", "updated_at": "2026-01-01T00:01:40Z"
    })
}

fn write_jsonl(path: &Path, issues: &[Value]) {
    let content = issues
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(path, format!("{content}\n")).unwrap();
}

fn current_tracker() -> (BrWorkspace, PathBuf) {
    let workspace = BrWorkspace::new();
    let init = run_br(
        &workspace,
        ["init", "--prefix", "dur", "--json"],
        "durable_init",
    );
    assert!(init.status.success(), "{} {}", init.stdout, init.stderr);
    let neighbor = json!({
        "id": "dur-neighbor", "title": "Other local issue", "status": "open",
        "priority": 2, "issue_type": "task", "created_by": "original-actor",
        "created_at": "2026-01-01T00:00:15Z", "updated_at": "2026-01-01T00:01:20Z"
    });
    write_jsonl(
        &workspace.root.join(".beads/issues.jsonl"),
        &[original_issue(), neighbor],
    );
    let imported = run_br(
        &workspace,
        ["sync", "--import-only", "--json"],
        "durable_initial_import",
    );
    assert!(
        imported.status.success(),
        "{} {}",
        imported.stdout,
        imported.stderr
    );
    let db = workspace.root.join(".beads/beads.db");
    (workspace, db)
}

fn released_tracker() -> (BrWorkspace, PathBuf) {
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
    (workspace, db)
}

fn seed_owned_records(db: &Path, owner: &str) {
    edit(
        db,
        &[
            &format!("UPDATE issues SET source_repo_path = '/local/original' WHERE id = '{owner}'"),
            &format!(
                "INSERT INTO events (id, issue_id, event_type, actor, new_value, agent_name, harness, model) VALUES (811, '{owner}', 'updated', 'original-actor', 'preserved audit evidence', 'original-agent', 'original-harness', 'original-model')"
            ),
            &format!(
                "INSERT INTO gate_results (issue_id, gate, provider, passed, recorded_by) VALUES ('{owner}', 'review', 'operator', 1, 'original-reviewer')"
            ),
            &format!(
                "INSERT INTO child_counters (rowid, parent_id, last_child) VALUES (401, '{owner}', 47)"
            ),
            "INSERT INTO config (rowid, key, value) VALUES (1001, 'compact_model', 'original-model')",
            "INSERT INTO metadata (rowid, key, value) VALUES (9001, 'operator_receipt', 'original receipt')",
            "CREATE TABLE operator_owned (id INTEGER PRIMARY KEY AUTOINCREMENT, issue_id TEXT REFERENCES issues(id), payload BLOB NOT NULL)",
            &format!("INSERT INTO operator_owned VALUES (17, '{owner}', X'0001FF00')"),
            "UPDATE sqlite_sequence SET seq = 91 WHERE name = 'operator_owned'",
        ],
    );
}

fn heal_released_tracker(workspace: &BrWorkspace, db: &Path) {
    let source = fs::read(db).unwrap();
    let healed = run_br(
        workspace,
        [
            "doctor",
            "migrate-schema",
            "heal",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "durable_reviewed_heal",
    );
    assert!(
        healed.status.success(),
        "{} {}",
        healed.stdout,
        healed.stderr
    );
    assert_eq!(
        query_copy(db, "PRAGMA user_version"),
        vec![vec![SqliteValue::Integer(i64::from(
            beads_rust::storage::schema::CURRENT_SCHEMA_VERSION
        ))]],
    );
    assert!(
        files_in(&db.parent().unwrap().join(".br_recovery/schema-migrations"))
            .values()
            .any(|bytes| bytes == &source)
    );
}

fn assert_original_rows_remain(db: &Path, before: &RawRows) {
    let after = raw_records(db);
    assert_eq!(after["schema"], before["schema"]);
    for table in [
        "issues",
        "events",
        "comments",
        "labels",
        "dependencies",
        "gate_results",
        "dirty_issues",
        "export_hashes",
    ] {
        assert!(
            before[table].iter().all(|row| after[table].contains(row)),
            "lost original {table} records"
        );
    }
    for table in ["config", "child_counters", "operator_owned"] {
        assert_eq!(after[table], before[table], "changed original {table}");
    }
    let original_receipts: Vec<_> = before["metadata"]
        .iter()
        .filter(|row| row[1].as_text() == Some("operator_receipt"))
        .cloned()
        .collect();
    assert_eq!(
        query_copy(
            db,
            "SELECT rowid, * FROM metadata WHERE key = 'operator_receipt' ORDER BY rowid"
        ),
        original_receipts
    );
    assert_eq!(
        query_copy(
            db,
            "SELECT seq FROM sqlite_sequence WHERE name = 'operator_owned'"
        ),
        vec![vec![SqliteValue::Integer(91)]]
    );
    assert!(
        after["events"].len() > before["events"].len(),
        "successful create must append its own event"
    );
}

fn assert_doctor_still_reports_duplicate(workspace: &BrWorkspace, beads: &Path, table: &str) {
    let before = files_in(beads);
    let doctor = run_br(
        workspace,
        ["doctor", "--json", "--no-auto-import", "--no-auto-flush"],
        "durable_duplicate_diagnostic",
    );
    assert!(!doctor.status.success());
    let report = payload(&doctor.stdout);
    assert!(
        report["checks"].as_array().unwrap().iter().any(|check| {
            check["name"] == "db.recoverable_anomalies"
                && check["details"]["findings"]
                    .as_array()
                    .is_some_and(|findings| {
                        findings.iter().any(|finding| {
                            finding.as_str().is_some_and(|message| {
                                message.contains(&format!("{table} contains duplicate rows"))
                            })
                        })
                    })
        }),
        "{report}"
    );
    assert_eq!(
        files_in(beads),
        before,
        "Doctor diagnostic changed the tracker"
    );
}

#[test]
fn writable_reopen_keeps_duplicate_keys_and_every_original_owned_record() {
    for upgraded in [false, true] {
        for table in ["config", "metadata"] {
            let (mut workspace, db) = if upgraded {
                released_tracker()
            } else {
                current_tracker()
            };
            let owner = if upgraded { "v15ws-9nx" } else { "dur-owner" };
            seed_owned_records(&db, owner);
            let duplicate = if table == "config" {
                "INSERT INTO config (rowid, key, value) VALUES (1009, 'compact_model', 'newer preserved model')"
            } else {
                "INSERT INTO metadata (rowid, key, value) VALUES (9017, 'operator_receipt', 'newer preserved receipt')"
            };
            edit(&db, &[duplicate]);
            if upgraded {
                heal_released_tracker(&workspace, &db);
            }
            let beads = workspace.root.join(".beads");
            let before = raw_records(&db);
            let recovery = files_in(&beads.join(".br_recovery"));
            let source_jsonl = fs::read(beads.join("issues.jsonl")).unwrap();
            move_to_nested(&mut workspace);
            let created = run_br(
                &workspace,
                [
                    "create",
                    "Writable reopen preserves original authority",
                    "--json",
                    "--no-auto-import",
                    "--no-auto-flush",
                ],
                &format!("durable_reopen_{upgraded}_{table}"),
            );
            assert!(
                created.status.success(),
                "{} {}",
                created.stdout,
                created.stderr
            );
            assert!(!created.stderr.contains("rebuilt"), "{}", created.stderr);
            assert!(payload(&created.stdout)["id"].is_string());
            assert_original_rows_remain(&db, &before);
            assert_eq!(fs::read(beads.join("issues.jsonl")).unwrap(), source_jsonl);
            assert_eq!(
                files_in(&beads.join(".br_recovery")),
                recovery,
                "writable reopen reconstructed the database"
            );
            assert_doctor_still_reports_duplicate(&workspace, &beads, table);
        }
    }
}

fn seed_identity_owned_records(db: &Path) {
    seed_owned_records(db, "dur-owner");
    edit(
        db,
        &[
            "INSERT INTO gate_result_history (issue_id, from_status, to_status, status_revision, gate, provider, passed) VALUES ('dur-owner', 'open', 'closed', 0, 'review', 'operator', 1)",
            "INSERT INTO close_metadata (issue_id, bypassed_policy, bypass_reason) VALUES ('dur-owner', 1, 'original closure evidence')",
            "INSERT INTO capacity_exemptions (issue_id, capacity_kind, capacity_name, provider, reason, granted_by) VALUES ('dur-owner', 'status', 'in_progress', 'operator', 'original grant', 'original-actor')",
            "INSERT INTO capacity_exemption_history (issue_id, capacity_kind, capacity_name, action, provider, actor) VALUES ('dur-owner', 'status', 'in_progress', 'grant', 'operator', 'original-actor')",
            "INSERT INTO capacity_occupancy (issue_id, actor, session) VALUES ('dur-owner', 'original-actor', 'original-session')",
            "INSERT INTO dependencies (issue_id, depends_on_id, type) VALUES ('dur-owner', 'dur-neighbor', 'related'), ('dur-neighbor', 'dur-owner', 'related')",
            "INSERT INTO labels (issue_id, label) VALUES ('dur-owner', 'original-label')",
        ],
    );
}

fn write_displaced_source(jsonl: &Path, relocated_first: bool) {
    let mut relocated = original_issue();
    relocated["id"] = Value::from("dur-relocated");
    let replacement = json!({
        "id": "dur-owner", "title": "Different issue", "status": "open",
        "priority": 2, "issue_type": "task", "created_by": "different-actor",
        "created_at": "2026-01-01T00:00:20Z", "updated_at": "2026-01-01T00:00:20Z"
    });
    let fresh = json!({
        "id": "dur-new", "title": "Must not be inserted", "status": "open",
        "priority": 2, "issue_type": "task", "created_by": "external-actor",
        "created_at": "2026-01-01T00:02:30Z", "updated_at": "2026-01-01T00:03:20Z"
    });
    let issues = if relocated_first {
        [fresh, relocated, replacement]
    } else {
        [fresh, replacement, relocated]
    };
    write_jsonl(jsonl, &issues);
}

fn assert_original_identity_read_only(workspace: &BrWorkspace, beads: &Path) {
    let before = files_in(beads);
    let shown = run_br_with_env(
        workspace,
        [
            "show",
            "dur-owner",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        [("BR_DISABLE_READ_ONLY_FAST_OPEN", "1")],
        "durable_original_identity_read",
    );
    assert!(shown.status.success(), "{} {}", shown.stdout, shown.stderr);
    let issue = payload(&shown.stdout)[0].clone();
    assert_eq!(issue["id"], "dur-owner");
    assert_eq!(issue["title"], "Original issue");
    assert_eq!(issue["source_repo_path"], "/local/original");
    assert_eq!(files_in(beads), before, "explicit read changed the tracker");
}

#[test]
fn automatic_import_never_transfers_owned_records_to_a_different_issue_identity() {
    for relocated_first in [false, true] {
        let (mut workspace, db) = current_tracker();
        let beads = workspace.root.join(".beads");
        seed_identity_owned_records(&db);
        let jsonl = beads.join("issues.jsonl");
        write_displaced_source(&jsonl, relocated_first);
        let source = fs::read(&jsonl).unwrap();
        let before = raw_records(&db);
        assert!(
            before["dirty_issues"].is_empty(),
            "must exercise automatic import"
        );
        let recovery = files_in(&beads.join(".br_recovery"));
        move_to_nested(&mut workspace);
        assert_original_identity_read_only(&workspace, &beads);
        let imported = run_br(
            &workspace,
            ["list", "--json"],
            "durable_identity_automatic_import",
        );
        assert!(
            !imported.status.success(),
            "{} {}",
            imported.stdout,
            imported.stderr
        );
        let error = payload(&imported.stdout);
        let message = error["error"]["message"].as_str().unwrap();
        for expected in [
            "Automatic import refused",
            "issue identity",
            "dur-owner",
            "database-only",
            "no import was applied",
        ] {
            assert!(message.contains(expected), "missing {expected}: {message}");
        }
        assert_eq!(
            raw_records(&db),
            before,
            "refusal changed original rows, rowids, or schema"
        );
        assert_eq!(fs::read(&jsonl).unwrap(), source);
        assert_eq!(files_in(&beads.join(".br_recovery")), recovery);
        assert_original_identity_read_only(&workspace, &beads);
        assert!(
            query_copy(
                &db,
                "SELECT id FROM issues WHERE id IN ('dur-relocated', 'dur-new')"
            )
            .is_empty()
        );
    }
}

fn damaged_rebuild_source(db: &Path) -> Vec<u8> {
    // Isolate replacement/rollback from orphan-WAL recovery. The source
    // remains retained test data, including its deliberately damaged header.
    edit(
        db,
        &[
            "PRAGMA wal_checkpoint(TRUNCATE)",
            "PRAGMA journal_mode = DELETE",
        ],
    );
    assert!(!PathBuf::from(format!("{}-wal", db.display())).exists());
    assert!(!PathBuf::from(format!("{}-journal", db.display())).exists());
    let mut source = fs::read(db).unwrap();
    source[..16].copy_from_slice(&[0xff; 16]);
    fs::write(db, &source).unwrap();
    source
}

#[test]
fn explicit_rebuild_retains_damaged_source_and_restores_it_if_import_fails() {
    for valid_jsonl in [true, false] {
        let (mut workspace, db) = current_tracker();
        let beads = workspace.root.join(".beads");
        seed_owned_records(&db, "dur-owner");
        fs::write(beads.join("config.yaml"), "issue_prefix: dur\n").unwrap();
        let prior = beads.join(".br_recovery/operator-evidence/retained.bin");
        fs::create_dir_all(prior.parent().unwrap()).unwrap();
        fs::write(&prior, b"original operator recovery evidence").unwrap();
        let source = damaged_rebuild_source(&db);
        let jsonl = beads.join("issues.jsonl");
        if !valid_jsonl {
            fs::write(&jsonl, b"{ malformed recovery source\n").unwrap();
        }
        let source_jsonl = fs::read(&jsonl).unwrap();
        move_to_nested(&mut workspace);
        let rebuilt = run_br(
            &workspace,
            ["sync", "--import-only", "--rebuild", "--json"],
            &format!("durable_explicit_rebuild_{valid_jsonl}"),
        );
        assert_eq!(
            rebuilt.status.success(),
            valid_jsonl,
            "{} {}",
            rebuilt.stdout,
            rebuilt.stderr
        );
        assert_eq!(fs::read(&jsonl).unwrap(), source_jsonl);
        assert_eq!(
            fs::read(&prior).unwrap(),
            b"original operator recovery evidence"
        );
        if valid_jsonl {
            assert!(
                files_in(&beads.join(".br_recovery"))
                    .values()
                    .any(|bytes| bytes == &source),
                "explicit reconstruction lost the original source"
            );
            assert_eq!(
                query_copy(&db, "SELECT id, title FROM issues ORDER BY id"),
                vec![
                    vec![
                        SqliteValue::from("dur-neighbor"),
                        SqliteValue::from("Other local issue")
                    ],
                    vec![
                        SqliteValue::from("dur-owner"),
                        SqliteValue::from("Original issue")
                    ],
                ]
            );
            let read = run_br(
                &workspace,
                ["list", "--json", "--no-auto-import", "--no-auto-flush"],
                "durable_explicit_rebuild_read",
            );
            assert!(read.status.success(), "{} {}", read.stdout, read.stderr);
            assert_eq!(payload(&read.stdout)["issues"].as_array().unwrap().len(), 2);
        } else {
            assert!(payload(&rebuilt.stdout)["error"].is_object());
            assert_eq!(
                fs::read(&db).unwrap(),
                source,
                "failed explicit import restored a placeholder instead of the original database"
            );
            assert!(!PathBuf::from(format!("{}-wal", db.display())).exists());
            assert!(!PathBuf::from(format!("{}-journal", db.display())).exists());
        }
    }
}
