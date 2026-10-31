//! GH #550: authorizing backup-only retention must not discard recoverable work.
//!
//! The old discard flag cannot authorize an unreviewed rebuild that moves
//! audit/configuration records out of the active database. These released
//! schema-4 fixtures keep their original events and metadata throughout.

mod common;

use beads_rust::franken_sync::Connection;
use common::cli::{BrWorkspace, run_br};
use flate2::read::GzDecoder;
use serde_json::Value;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

fn install_fixture(workspace: &BrWorkspace) -> PathBuf {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/schema_migration");
    let beads_dir = workspace.root.join(".beads");
    fs::create_dir_all(&beads_dir).expect("create .beads");
    let input = fs::File::open(fixtures.join("schema4_v0127_db_only.db.gz")).expect("fixture");
    let mut bytes = Vec::new();
    GzDecoder::new(input)
        .read_to_end(&mut bytes)
        .expect("decompress fixture");
    assert_eq!(u32::from_be_bytes(bytes[60..64].try_into().unwrap()), 4);
    let db_path = beads_dir.join("beads.db");
    fs::write(&db_path, bytes).expect("install database");
    fs::copy(
        fixtures.join("schema4_v0127.issues.jsonl"),
        beads_dir.join("issues.jsonl"),
    )
    .expect("install export");
    fs::write(beads_dir.join("config.yaml"), "issue_prefix: leg\n").expect("config");
    db_path
}

fn seed_relations(db_path: &Path, orphan: bool, unreadable: bool) {
    let conn = Connection::open(db_path.to_string_lossy().into_owned()).expect("open fixture");
    for sql in [
        "PRAGMA foreign_keys = OFF",
        "INSERT INTO labels (issue_id, label) VALUES ('leg-u4i', 'unflushed-label')",
        "INSERT INTO comments (issue_id, author, text, created_at) \
         VALUES ('leg-u4i', 'agent', 'unflushed comment', '2026-01-01 00:00:00')",
        "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by) \
         VALUES ('leg-u4i', 'leg-n57', 'blocks', '2026-01-01 00:00:00', 'agent')",
    ] {
        conn.execute(sql).expect("seed recoverable relations");
    }
    if orphan {
        conn.execute("INSERT INTO labels (issue_id, label) VALUES ('leg-missing', 'orphan')")
            .expect("seed orphan");
    }
    if unreadable {
        conn.execute("UPDATE issues SET updated_at = 'not a timestamp' WHERE id = 'leg-n57'")
            .expect("make one payload unreadable to the legacy decoder");
    }
    conn.close().expect("close fixture");
}

fn json(stdout: &str) -> Value {
    serde_json::from_str(stdout).expect("the whole stdout must be one JSON document")
}

fn flagged_heal(workspace: &BrWorkspace, dry_run: bool) -> Value {
    let mut args = vec![
        "doctor",
        "migrate-schema",
        "heal",
        "--discard-db-only",
        "--json",
    ];
    if dry_run {
        args.push("--dry-run");
    }
    let result = run_br(workspace, args, if dry_run { "preview" } else { "heal" });
    assert_eq!(
        result.status.success(),
        dry_run,
        "stdout={} stderr={}",
        result.stdout,
        result.stderr
    );
    json(&result.stdout)
}

fn assert_recoverable_records_still_active(db_path: &Path) {
    let scratch = tempfile::tempdir().unwrap();
    let copy = scratch.path().join("copy.db");
    fs::copy(db_path, &copy).unwrap();
    let wal = PathBuf::from(format!("{}-wal", db_path.display()));
    if wal.exists() {
        fs::copy(wal, scratch.path().join("copy.db-wal")).unwrap();
    }
    let conn = Connection::open(copy.to_string_lossy().into_owned()).expect("open inspection copy");
    for (sql, expected) in [
        (
            "SELECT title FROM issues WHERE id = 'leg-u4i'",
            "Unflushed legacy issue",
        ),
        (
            "SELECT label FROM labels WHERE issue_id = 'leg-u4i'",
            "unflushed-label",
        ),
        (
            "SELECT text FROM comments WHERE issue_id = 'leg-u4i'",
            "unflushed comment",
        ),
        (
            "SELECT depends_on_id FROM dependencies WHERE issue_id = 'leg-u4i'",
            "leg-n57",
        ),
    ] {
        let rows = conn.query(sql).unwrap();
        assert_eq!(rows.len(), 1, "{sql}");
        assert_eq!(
            rows[0]
                .get(0)
                .and_then(beads_rust::franken_sync::SqliteValue::as_text),
            Some(expected)
        );
    }
    let dirty = conn
        .query("SELECT issue_id FROM dirty_issues WHERE issue_id = 'leg-u4i'")
        .expect("read dirty markers");
    assert_eq!(
        dirty.len(),
        1,
        "the original issue must remain exportable by its owning release"
    );
    let events = conn.query("SELECT count(*) FROM events").unwrap();
    assert_eq!(
        events[0]
            .get(0)
            .and_then(beads_rust::franken_sync::SqliteValue::as_integer),
        Some(7)
    );
    conn.close().expect("close inspection copy");
}

#[test]
fn discard_for_orphans_cannot_remove_unflushed_edits_or_audit_records() {
    // The flag never establishes a lossless migration, with or without orphans.
    for orphan in [false, true] {
        let workspace = BrWorkspace::new();
        let db_path = install_fixture(&workspace);
        seed_relations(&db_path, orphan, false);
        let jsonl_path = workspace.root.join(".beads/issues.jsonl");
        let before_jsonl = fs::read(&jsonl_path).expect("original export");
        let before_db = fs::read(&db_path).expect("original database");
        if orphan {
            let refused = run_br(
                &workspace,
                ["doctor", "migrate-schema", "heal", "--json"],
                "refused_without_authorization",
            );
            assert!(!refused.status.success());
            let error = json(&refused.stdout).to_string();
            assert!(
                error.contains("migration") && error.contains("preserv"),
                "{error}"
            );
            assert_eq!(fs::read(&db_path).unwrap(), before_db);
            assert_eq!(fs::read(&jsonl_path).unwrap(), before_jsonl);
        }

        let preview = flagged_heal(&workspace, true);
        assert!(
            preview["planned_action"]
                .as_str()
                .unwrap()
                .contains("refus"),
            "{preview}"
        );
        assert_eq!(preview["automatic_rebuild_allowed"], false);
        assert_eq!(preview["audit"]["db_only"].as_array().unwrap().len(), 2);
        if orphan {
            assert_eq!(
                preview["audit"]["orphan_relations"],
                serde_json::json!([{"table": "labels", "rows": 1}])
            );
        }
        assert_eq!(
            fs::read(&db_path).unwrap(),
            before_db,
            "dry run must not rebuild"
        );
        assert_eq!(fs::read(&jsonl_path).unwrap(), before_jsonl);

        let outcome = flagged_heal(&workspace, false);
        assert!(outcome["error"].is_object());
        assert_eq!(fs::read(&db_path).unwrap(), before_db);
        assert_eq!(fs::read(&jsonl_path).unwrap(), before_jsonl);
        assert!(!workspace.root.join(".beads/.br_recovery").exists());
        assert_recoverable_records_still_active(&db_path);
    }
}

#[test]
fn discard_unreadable_payload_cannot_omit_a_different_recoverable_issue() {
    let workspace = BrWorkspace::new();
    let db_path = install_fixture(&workspace);
    seed_relations(&db_path, true, true);
    let before = fs::read(&db_path).expect("original database");
    let refused = run_br(
        &workspace,
        ["doctor", "migrate-schema", "heal", "--json"],
        "unreadable_refused",
    );
    assert!(!refused.status.success());
    let error = json(&refused.stdout).to_string();
    assert!(
        error.contains("migration") && error.contains("preserv"),
        "{error}"
    );
    assert_eq!(fs::read(&db_path).unwrap(), before);

    let preview = flagged_heal(&workspace, true);
    let action = preview["planned_action"].as_str().unwrap();
    assert!(action.contains("refus"), "{action}");
    assert!(
        preview["audit"]["db_only"]
            .as_array()
            .unwrap()
            .iter()
            .any(|issue| issue["id"] == "leg-n57" && issue["reason"] == "unreadable")
    );
    assert_eq!(fs::read(&db_path).unwrap(), before);

    let outcome = flagged_heal(&workspace, false);
    assert!(outcome["error"].is_object());
    assert_eq!(fs::read(&db_path).unwrap(), before);
    assert!(!workspace.root.join(".beads/.br_recovery").exists());
    assert_recoverable_records_still_active(&db_path);
}

#[test]
fn discard_preservation_does_not_overwrite_a_superseding_jsonl_edit() {
    let workspace = BrWorkspace::new();
    let db_path = install_fixture(&workspace);
    seed_relations(&db_path, false, false);
    let jsonl_path = workspace.root.join(".beads/issues.jsonl");
    let exported = fs::read_to_string(&jsonl_path).expect("original JSONL");
    let rewritten: Vec<String> = exported
        .lines()
        .map(|line| {
            let mut row = json(line);
            if row["id"] == "leg-n57" {
                row["title"] = serde_json::json!("Newer upstream edit");
                row["updated_at"] = serde_json::json!("2999-01-01T00:00:00Z");
            }
            row.to_string()
        })
        .collect();
    fs::write(&jsonl_path, format!("{}\n", rewritten.join("\n"))).expect("newer export");
    let before_db = fs::read(&db_path).unwrap();
    let before_jsonl = fs::read(&jsonl_path).unwrap();

    let outcome = flagged_heal(&workspace, false);
    assert!(outcome["error"].is_object());
    assert_eq!(fs::read(&db_path).unwrap(), before_db);
    assert_eq!(fs::read(&jsonl_path).unwrap(), before_jsonl);
    assert!(!workspace.root.join(".beads/.br_recovery").exists());
    assert_recoverable_records_still_active(&db_path);
}
