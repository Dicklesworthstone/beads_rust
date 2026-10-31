//! Regressions for local-replay risks in a schema-heal reconstruction.
//! Valid JSONL and a replayable issue subset do not prove preservation of the
//! complete database, nor a crash-safe installation. Until a reviewed path
//! exists, neither heal nor --discard-db-only may replace the old database.

mod common;

use beads_rust::franken_sync::{Connection, SqliteValue};
use common::cli::{BrWorkspace, run_br};
use flate2::read::GzDecoder;
use serde_json::Value;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

const LOCAL_CONTEXT: &str = "{\"constraints\":[\"keep governing instructions\"]}";
const LOCAL_PATH: &str = "/original/legacy/workspace";
const LOCAL_PREREQUISITES: &str = "- [ ] Review before implementation";

fn fixture(workspace: &BrWorkspace) -> PathBuf {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/schema_migration");
    let beads = workspace.root.join(".beads");
    fs::create_dir_all(&beads).unwrap();
    let mut bytes = Vec::new();
    GzDecoder::new(fs::File::open(fixtures.join("schema4_v0127_db_only.db.gz")).unwrap())
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
    let conn = Connection::open(db.to_string_lossy().into_owned()).unwrap();
    // Extend the released fixture with later legacy payload columns while
    // retaining the pre-reviewed schema marker. The schema-agnostic rebuild
    // must preserve these values rather than silently projecting them away.
    let columns = conn.query("PRAGMA table_info(issues)").unwrap();
    for column in ["agent_context", "source_repo_path", "prerequisites"] {
        if !columns
            .iter()
            .any(|row| row.get(1).and_then(SqliteValue::as_text) == Some(column))
        {
            conn.execute(&format!("ALTER TABLE issues ADD COLUMN {column} TEXT"))
                .unwrap();
        }
    }
    conn.execute(&format!(
        "UPDATE issues SET agent_context = '{LOCAL_CONTEXT}', \
         source_repo_path = '{LOCAL_PATH}', prerequisites = '{LOCAL_PREREQUISITES}' \
         WHERE id = 'leg-u4i'"
    ))
    .unwrap();
    for sql in [
        "UPDATE issues SET external_ref = 'replay-conflict' WHERE id = 'leg-u4i'",
        "INSERT INTO labels (issue_id, label) VALUES ('leg-u4i', 'preserve-local')",
        "INSERT INTO comments (issue_id, author, text, created_at) \
         VALUES ('leg-u4i', 'agent', 'preserve this comment', '2026-01-01 00:00:00')",
        "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by) \
         VALUES ('leg-u4i', 'leg-n57', 'blocks', '2026-01-01 00:00:00', 'agent')",
    ] {
        conn.execute(sql).unwrap();
    }
    conn.close().unwrap();
    // The DB and JSONL each have a unique external_ref internally. Only
    // combining the JSONL with the absent, unflushed leg-u4i causes collision.
    set_upstream_reference(workspace, "replay-conflict");
    db
}

fn json(text: &str) -> Value {
    serde_json::from_str(text).expect("complete stdout must be one JSON document")
}

fn set_upstream_reference(workspace: &BrWorkspace, external_ref: &str) {
    let path = workspace.root.join(".beads/issues.jsonl");
    let lines: Vec<String> = fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| {
            let mut row = json(line);
            if row["id"] == "leg-n57" {
                row["external_ref"] = Value::String(external_ref.to_owned());
                row["title"] = serde_json::json!("Superseding upstream title");
                row["updated_at"] = serde_json::json!("2999-01-01T00:00:00Z");
            }
            row.to_string()
        })
        .collect();
    fs::write(path, format!("{}\n", lines.join("\n"))).unwrap();
}

fn header_version(db: &Path) -> u32 {
    let bytes = fs::read(db).unwrap();
    u32::from_be_bytes(bytes[60..64].try_into().unwrap())
}

fn optional_bytes(path: &Path) -> Option<Vec<u8>> {
    match fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => panic!("cannot inspect {}: {error}", path.display()),
    }
}

fn assert_refused_before_replacement(workspace: &BrWorkspace, db: &Path, discard: bool) {
    let before = fs::read(db).unwrap();
    let wal = PathBuf::from(format!("{}-wal", db.display()));
    let before_wal = optional_bytes(&wal);
    let export = workspace.root.join(".beads/issues.jsonl");
    let before_export = fs::read(&export).unwrap();
    let mut args = vec!["doctor", "migrate-schema", "heal", "--json"];
    if discard {
        args.push("--discard-db-only");
    }
    let result = run_br(workspace, args, "refuse_replay_conflict");
    assert!(!result.status.success(), "{}", result.stdout);
    let message = json(&result.stdout)["error"]["message"]
        .as_str()
        .expect("structured error")
        .to_owned();
    assert!(
        message.contains("migration") && message.contains("refused"),
        "{message}"
    );
    assert!(
        message.contains("original database") && message.contains("preserv"),
        "{message}"
    );
    assert_eq!(
        header_version(db),
        4,
        "failure must not mark the tracker current"
    );
    assert_eq!(fs::read(db).unwrap(), before, "original database bytes");
    assert_eq!(optional_bytes(&wal), before_wal, "original committed WAL");
    assert_eq!(fs::read(export).unwrap(), before_export, "original JSONL");
}

#[test]
fn conflicting_local_replay_leaves_the_old_family_intact_on_every_retry() {
    let workspace = BrWorkspace::new();
    let db = fixture(&workspace);
    for discard in [false, true, false] {
        assert_refused_before_replacement(&workspace, &db, discard);
    }
}

fn assert_stored_governance(db: &Path) {
    let scratch = tempfile::tempdir().unwrap();
    let copy = scratch.path().join("copy.db");
    fs::copy(db, &copy).unwrap();
    let wal = PathBuf::from(format!("{}-wal", db.display()));
    if wal.exists() {
        fs::copy(wal, scratch.path().join("copy.db-wal")).unwrap();
    }
    let conn = Connection::open(copy.to_string_lossy().into_owned()).unwrap();
    let rows = conn
        .query(
            "SELECT agent_context, source_repo_path, prerequisites \
             FROM issues WHERE id = 'leg-u4i'",
        )
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].get(0).and_then(SqliteValue::as_text),
        Some(LOCAL_CONTEXT)
    );
    assert_eq!(
        rows[0].get(1).and_then(SqliteValue::as_text),
        Some(LOCAL_PATH)
    );
    assert_eq!(
        rows[0].get(2).and_then(SqliteValue::as_text),
        Some(LOCAL_PREREQUISITES)
    );
    conn.close().unwrap();
}

#[test]
fn resolving_one_replay_conflict_does_not_prove_complete_database_preservation() {
    let workspace = BrWorkspace::new();
    let db = fixture(&workspace);
    assert_refused_before_replacement(&workspace, &db, false);
    set_upstream_reference(&workspace, "different-upstream-reference");

    // The old code only rehearsed issue replay. Even after this collision is
    // resolved, the events/configuration still have no lossless migration and
    // replay-after-install cannot survive interruption safely.
    assert_refused_before_replacement(&workspace, &db, false);
    assert_refused_before_replacement(&workspace, &db, true);
    assert_stored_governance(&db);
    let rows: Vec<Value> = fs::read_to_string(workspace.root.join(".beads/issues.jsonl"))
        .unwrap()
        .lines()
        .map(json)
        .collect();
    assert!(
        rows.iter().all(|row| row["id"] != "leg-u4i"),
        "unreviewed reconstruction must not publish local work into JSONL"
    );
    let upstream = rows.iter().find(|row| row["id"] == "leg-n57").unwrap();
    assert_eq!(upstream["title"], "Superseding upstream title");
    assert_eq!(upstream["external_ref"], "different-upstream-reference");
    assert!(!workspace.root.join(".beads/.br_recovery").exists());
}
