//! Real-CLI qualification of the bounded upgrade for released br 0.1.27.
//!
//! These frozen schema-4 files were produced by the historical CLI. They are
//! not current databases with an older version stamp. The assertions inspect
//! original SQL columns and hidden rowids independently of the migrator's
//! destination projection, and verify the retained and restored source bytes.

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

fn install(db_only: bool) -> (BrWorkspace, PathBuf) {
    let workspace = BrWorkspace::new();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/schema_migration");
    let beads = workspace.root.join(".beads");
    fs::create_dir_all(&beads).unwrap();
    let fixture = if db_only {
        "schema4_v0127_db_only.db.gz"
    } else {
        "schema4_v0127_clean.db.gz"
    };
    let mut bytes = Vec::new();
    GzDecoder::new(fs::File::open(fixtures.join(fixture)).unwrap())
        .read_to_end(&mut bytes)
        .unwrap();
    let db = beads.join("beads.db");
    fs::write(&db, bytes).unwrap();
    fs::copy(
        fixtures.join("schema4_v0127.issues.jsonl"),
        beads.join("issues.jsonl"),
    )
    .unwrap();
    fs::write(beads.join("config.yaml"), "issue_prefix: leg\n").unwrap();
    (workspace, db)
}

fn version(db: &Path) -> u32 {
    u32::from_be_bytes(fs::read(db).unwrap()[60..64].try_into().unwrap())
}

fn json(stdout: &str) -> Value {
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

fn seed_database_only_records(db: &Path) {
    edit(
        db,
        &[
            "INSERT INTO config (rowid, key, value) VALUES (1001, 'compact_model', 'older-model')",
            "INSERT INTO config (rowid, key, value) VALUES (1009, 'compact_model', 'preserved-model')",
            "INSERT INTO config (key, value) VALUES ('compact_batch_size', '37')",
            "INSERT INTO metadata (rowid, key, value) VALUES (9001, 'operator_receipt', 'older receipt')",
            "INSERT INTO metadata (rowid, key, value) VALUES (9017, 'operator_receipt', 'preserved receipt')",
            "INSERT INTO child_counters (parent_id, last_child) VALUES ('leg-n57', 47)",
            "CREATE TABLE operator_notes (id INTEGER PRIMARY KEY AUTOINCREMENT, note TEXT NOT NULL, payload BLOB NOT NULL)",
            "INSERT INTO operator_notes VALUES (12, 'database-only operator state', X'0001FF00')",
            "UPDATE sqlite_sequence SET seq = 91 WHERE name = 'operator_notes'",
        ],
    );
}

fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Select the original cells by name after ALTER TABLE appends new fields.
/// Only the explicitly derived content_hash may change. Sequence identity is
/// its name, not sqlite_sequence's incidental hidden rowid; new empty history
/// tables may acquire their own zero-valued reservations.
fn preservation_queries(db: &Path) -> BTreeMap<String, String> {
    let mut queries = BTreeMap::new();
    for row in query_copy(
        db,
        "SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name",
    ) {
        let table = row[0].as_text().unwrap();
        let quoted = quote_identifier(table);
        let query = if table == "sqlite_sequence" {
            let names = query_copy(db, "SELECT name FROM sqlite_sequence ORDER BY name")
                .iter()
                .map(|row| format!("'{}'", row[0].as_text().unwrap().replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(",");
            format!("SELECT name, seq FROM sqlite_sequence WHERE name IN ({names}) ORDER BY name")
        } else {
            let columns = query_copy(db, &format!("PRAGMA table_info({quoted})"))
                .iter()
                .map(|column| column[1].as_text().unwrap())
                .filter(|name| table != "issues" || *name != "content_hash")
                .map(quote_identifier)
                .collect::<Vec<_>>()
                .join(", ");
            format!("SELECT rowid, {columns} FROM {quoted} ORDER BY rowid")
        };
        queries.insert(table.to_owned(), query);
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
    visit(directory, directory, &mut files);
    files
}

fn nested(workspace: &mut BrWorkspace) {
    workspace.root = workspace.root.join("project/nested");
    fs::create_dir_all(&workspace.root).unwrap();
}

fn assert_added_defaults(db: &Path) {
    assert!(query_copy(db, "SELECT id FROM issues WHERE source_repo_path IS NOT NULL OR agent_context IS NOT NULL OR prerequisites != '' OR prerequisites IS NULL").is_empty());
    assert!(query_copy(db, "SELECT id FROM events WHERE agent_name IS NOT NULL OR harness IS NOT NULL OR model IS NOT NULL").is_empty());
}

fn assert_hashes_match_current_cli_issues(workspace: &BrWorkspace, db: &Path) {
    for row in query_copy(db, "SELECT id, content_hash FROM issues ORDER BY id") {
        let id = row[0].as_text().unwrap();
        let shown = run_br(
            workspace,
            ["show", id, "--json", "--no-auto-import", "--no-auto-flush"],
            &format!("schema4_verify_hash_{id}"),
        );
        assert!(shown.status.success(), "{} {}", shown.stdout, shown.stderr);
        let details: beads_rust::format::IssueDetails =
            serde_json::from_value(json(&shown.stdout)[0].clone()).unwrap();
        let expected = details.issue.compute_content_hash();
        assert_eq!(
            row[1].as_text(),
            Some(expected.as_str()),
            "derived hash for {id}"
        );
    }
}

fn apply_reviewed_plan(workspace: &BrWorkspace, token: &str) -> Value {
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
        "schema4_apply",
    );
    assert!(
        applied.status.success(),
        "{} {}",
        applied.stdout,
        applied.stderr
    );
    json(&applied.stdout)
}

fn assert_next_child_keeps_reserved_numbers(workspace: &BrWorkspace) {
    let child = run_br(
        workspace,
        [
            "create",
            "First visible child after migration",
            "--parent",
            "leg-n57",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "schema4_reserved_child_number",
    );
    assert!(child.status.success(), "{} {}", child.stdout, child.stderr);
    assert_eq!(
        json(&child.stdout)["id"],
        "leg-n57.48",
        "automatic import must not make reserved child IDs reusable"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn released_schema4_plan_apply_and_undo_preserve_complete_original_records() {
    for db_only in [false, true] {
        let (mut workspace, db) = install(db_only);
        seed_database_only_records(&db);
        let beads = workspace.root.join(".beads");
        let original = fs::read(&db).unwrap();
        let source_jsonl = fs::read(beads.join("issues.jsonl")).unwrap();
        let queries = preservation_queries(&db);
        let records = preserved_rows(&db, &queries);
        let issue_count = records["issues"].len();
        let dirty_count = records["dirty_issues"].len();
        assert_eq!(issue_count, if db_only { 3 } else { 2 });
        assert_eq!(dirty_count, if db_only { 2 } else { 0 });
        nested(&mut workspace);
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
            "schema4_plan",
        );
        assert!(
            planned.status.success(),
            "{} {}",
            planned.stdout,
            planned.stderr
        );
        let plan = json(&planned.stdout);
        assert_eq!(plan["eligible"], true, "{plan}");
        assert_eq!(plan["from_version"], 4);
        assert_eq!(
            Path::new(plan["database_path"].as_str().unwrap())
                .canonicalize()
                .unwrap(),
            db.canonicalize().unwrap()
        );
        let forecast = &plan["forecast"];
        assert_eq!(forecast["content_hash_rows_rebuilt"], issue_count);
        let profile = &forecast["legacy_v4_conversion"];
        assert_eq!(profile["bookkeeping_preserved"], true);
        assert_eq!(
            profile["issues_columns_added"],
            serde_json::json!(["source_repo_path", "agent_context", "prerequisites"])
        );
        assert_eq!(
            profile["events_columns_added"],
            serde_json::json!(["agent_name", "harness", "model"])
        );
        assert_eq!(files_in(&beads), before, "plan wrote tracker files");
        let preview = run_br_with_env(
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
            "schema4_heal_preview",
        );
        assert!(
            preview.status.success(),
            "{} {}",
            preview.stdout,
            preview.stderr
        );
        assert_eq!(json(&preview.stdout)["automatic_heal_allowed"], true);
        assert_eq!(files_in(&beads), before, "heal preview wrote tracker files");
        assert_eq!(version(&db), 4);

        let applied = apply_reviewed_plan(&workspace, plan["plan_token"].as_str().unwrap());
        assert_eq!(
            i64::from(version(&db)),
            i64::from(beads_rust::storage::schema::CURRENT_SCHEMA_VERSION)
        );
        assert_eq!(
            preserved_rows(&db, &queries),
            records,
            "migration changed original durable records"
        );
        assert_added_defaults(&db);
        assert_hashes_match_current_cli_issues(&workspace, &db);
        assert_eq!(fs::read(beads.join("issues.jsonl")).unwrap(), source_jsonl);
        let run_id = applied["run_id"].as_str().unwrap();
        let run_dir = beads.join(".br_recovery/schema-migrations").join(run_id);
        assert!(run_dir.join("applied.json").is_file());
        assert!(
            files_in(&run_dir.join("before"))
                .values()
                .any(|bytes| bytes == &original),
            "recovery bundle omitted byte-identical source"
        );
        let before_undo = files_in(&beads);
        let preview = run_br_with_env(
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
            "schema4_undo_preview",
        );
        assert!(
            preview.status.success(),
            "{} {}",
            preview.stdout,
            preview.stderr
        );
        assert_eq!(json(&preview.stdout)["dry_run"], true);
        assert_eq!(
            files_in(&beads),
            before_undo,
            "undo preview wrote tracker files"
        );
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
            "schema4_undo",
        );
        assert!(
            undone.status.success(),
            "{} {}",
            undone.stdout,
            undone.stderr
        );
        assert_eq!(
            fs::read(&db).unwrap(),
            original,
            "undo must restore exact historical bytes"
        );
        assert_eq!(preserved_rows(&db, &queries), records);
        assert_eq!(fs::read(beads.join("issues.jsonl")).unwrap(), source_jsonl);
        assert_eq!(version(&db), 4);
        assert!(run_dir.join("undone.json").is_file());
    }
}

#[test]
fn released_schema4_automatic_mutation_keeps_history_and_local_state() {
    for db_only in [false, true] {
        let (workspace, db) = install(db_only);
        seed_database_only_records(&db);
        let source = fs::read(&db).unwrap();
        let queries = preservation_queries(&db);
        let before = preserved_rows(&db, &queries);
        let mut command = vec![
            "create",
            "Created after a lossless schema4 upgrade",
            "--json",
        ];
        if db_only {
            command.extend(["--no-auto-import", "--no-auto-flush"]);
        }
        let created = run_br(&workspace, command, "schema4_automatic_create");
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
        assert_eq!(
            i64::from(version(&db)),
            i64::from(beads_rust::storage::schema::CURRENT_SCHEMA_VERSION)
        );
        let after = preserved_rows(&db, &queries);
        for table in ["events", "config", "operator_notes", "child_counters"] {
            assert!(
                before[table].iter().all(|row| after[table].contains(row)),
                "lost original {table} records"
            );
        }
        assert_eq!(after["config"], before["config"]);
        assert_eq!(after["operator_notes"], before["operator_notes"]);
        assert!(
            after["events"].len() > before["events"].len(),
            "create omitted its own new event"
        );
        if db_only {
            for table in [
                "issues",
                "comments",
                "labels",
                "dependencies",
                "dirty_issues",
                "export_hashes",
            ] {
                assert!(
                    before[table].iter().all(|row| after[table].contains(row)),
                    "lost unflushed {table}"
                );
            }
        }
        assert_eq!(
            query_copy(
                &db,
                "SELECT seq FROM sqlite_sequence WHERE name = 'operator_notes'"
            ),
            vec![vec![SqliteValue::Integer(91)]]
        );
        assert_eq!(
            query_copy(
                &db,
                "SELECT value FROM metadata WHERE key = 'operator_receipt' ORDER BY rowid"
            ),
            vec![
                vec![SqliteValue::from("older receipt")],
                vec![SqliteValue::from("preserved receipt")]
            ]
        );
        let recovery = workspace.root.join(".beads/.br_recovery/schema-migrations");
        assert!(files_in(&recovery).values().any(|bytes| bytes == &source));
        assert_next_child_keeps_reserved_numbers(&workspace);
    }
}

#[test]
fn schema4_unreviewed_shapes_and_versions_refuse_without_reconstruction() {
    for (label, statement, expected_version) in [
        (
            "ordinary_column",
            "ALTER TABLE issues ADD COLUMN operator_payload BLOB",
            4,
        ),
        (
            "generated_column",
            "ALTER TABLE issues ADD COLUMN operator_length INTEGER GENERATED ALWAYS AS (length(title)) VIRTUAL",
            4,
        ),
        (
            "operator_unique_index",
            "CREATE UNIQUE INDEX operator_dependency_constraint ON dependencies(created_by)",
            4,
        ),
        (
            "missing_core_table",
            "ALTER TABLE comments RENAME TO operator_comments",
            4,
        ),
        ("unstamped", "PRAGMA user_version = 0", 0),
        ("unsupported_seven", "PRAGMA user_version = 7", 7),
    ] {
        let (mut workspace, db) = install(true);
        seed_database_only_records(&db);
        edit(&db, &[statement]);
        if label == "ordinary_column" {
            edit(
                &db,
                &["UPDATE issues SET operator_payload = X'0001FF00' WHERE id = 'leg-u4i'"],
            );
        }
        let beads = workspace.root.join(".beads");
        let source = fs::read(&db).unwrap();
        let source_jsonl = fs::read(beads.join("issues.jsonl")).unwrap();
        nested(&mut workspace);
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
            &format!("schema4_{label}_plan"),
        );
        let plan = json(&planned.stdout);
        assert!(
            plan["eligible"] == false || plan["error"].is_object(),
            "unreviewed {label} admitted: {plan}"
        );
        assert_eq!(files_in(&beads), before, "refused plan modified {label}");
        for mut command in [
            vec!["create", "Must not run", "--json"],
            vec!["doctor", "migrate-schema", "heal", "--json"],
            vec![
                "doctor",
                "migrate-schema",
                "heal",
                "--discard-db-only",
                "--json",
            ],
        ] {
            command.extend(["--no-auto-import", "--no-auto-flush"]);
            let refused = run_br(&workspace, command, &format!("schema4_{label}_refuse"));
            assert!(
                !refused.status.success(),
                "{} {}",
                refused.stdout,
                refused.stderr
            );
            let output = json(&refused.stdout);
            let message = output["error"]["message"]
                .as_str()
                .expect("migration refusal");
            assert!(message.contains("migration"), "{message}");
            assert_eq!(
                fs::read(&db).unwrap(),
                source,
                "refusal changed {label} database bytes"
            );
            assert_eq!(fs::read(beads.join("issues.jsonl")).unwrap(), source_jsonl);
            assert_eq!(version(&db), expected_version);
            assert!(
                !beads.join(".br_recovery").exists(),
                "unsafe reconstruction created recovery artifacts"
            );
        }
    }
}
