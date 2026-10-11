//! GH #550: authorizing backup-only retention must not discard recoverable work.
//!
//! Exercise the actual heal and flush commands using the released schema-4
//! fixture, not just the audit or its human-readable plan.

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
    assert!(
        result.status.success(),
        "stdout={} stderr={}",
        result.stdout,
        result.stderr
    );
    json(&result.stdout)
}

fn show_without_sync(workspace: &BrWorkspace, id: &str) -> Value {
    let result = run_br(
        workspace,
        ["show", id, "--json", "--no-auto-import", "--no-auto-flush"],
        &format!("show_{id}"),
    );
    assert!(result.status.success(), "{} {}", result.stdout, result.stderr);
    json(&result.stdout)[0].clone()
}

fn assert_recoverable_issue_and_flush(workspace: &BrWorkspace, db_path: &Path) -> Vec<Value> {
    let issue = show_without_sync(workspace, "leg-u4i");
    assert_eq!(issue["title"], "Unflushed legacy issue");
    assert_eq!(issue["labels"], serde_json::json!(["unflushed-label"]));
    assert_eq!(issue["comments"][0]["text"], "unflushed comment");
    assert_eq!(issue["dependencies"][0]["id"], "leg-n57");

    let conn = Connection::open(db_path.to_string_lossy().into_owned()).expect("open healed DB");
    let dirty = conn
        .query("SELECT issue_id FROM dirty_issues WHERE issue_id = 'leg-u4i'")
        .expect("read dirty markers");
    assert_eq!(dirty.len(), 1, "the restored issue must remain exportable");
    conn.close().expect("close healed DB");

    let flush = run_br(workspace, ["sync", "--flush-only", "--json"], "flush");
    assert!(flush.status.success(), "{} {}", flush.stdout, flush.stderr);
    let exported = fs::read_to_string(workspace.root.join(".beads/issues.jsonl")).expect("export");
    let rows: Vec<Value> = exported.lines().map(json).collect();
    let restored = rows
        .iter()
        .find(|row| row["id"] == "leg-u4i")
        .expect("restored export");
    assert_eq!(restored["title"], "Unflushed legacy issue");
    assert_eq!(restored["labels"], serde_json::json!(["unflushed-label"]));
    assert_eq!(restored["comments"][0]["text"], "unflushed comment");
    assert_eq!(restored["dependencies"][0]["depends_on_id"], "leg-n57");
    rows
}

#[test]
fn discard_for_orphans_restores_unflushed_edits_and_their_relations() {
    // The flag must preserve recoverable work even when it was unnecessary.
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
            assert!(error.contains("without an owning issue"), "{error}");
            assert!(
                error.contains("2 recoverable database-only issue(s)"),
                "{error}"
            );
            assert!(error.contains("leg-n57, leg-u4i"), "{error}");
            assert_eq!(fs::read(&db_path).unwrap(), before_db);
            assert_eq!(fs::read(&jsonl_path).unwrap(), before_jsonl);
        }

        let preview = flagged_heal(&workspace, true);
        assert!(
            preview["planned_action"]
                .as_str()
                .unwrap()
                .contains("re-add 2 database-only issue(s)"),
            "{preview}"
        );
        assert_eq!(preview["automatic_rebuild_allowed"], false);
        assert_eq!(
            fs::read(&db_path).unwrap(),
            before_db,
            "dry run must not rebuild"
        );
        assert_eq!(fs::read(&jsonl_path).unwrap(), before_jsonl);

        let outcome = flagged_heal(&workspace, false);
        assert_eq!(outcome["action"], "rebuilt");
        let mut restored: Vec<&str> = outcome["restored_db_only"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id.as_str().unwrap())
            .collect();
        restored.sort_unstable();
        assert_eq!(restored, ["leg-n57", "leg-u4i"]);
        assert_eq!(outcome["backup_only_db_only"], serde_json::json!([]));
        if orphan {
            assert_eq!(
                outcome["backup_only_orphan_relations"],
                serde_json::json!([{"table": "labels", "rows": 1}])
            );
        }
        assert!(Path::new(outcome["backup"].as_str().unwrap()).is_dir());
        assert_eq!(
            show_without_sync(&workspace, "leg-n57")["title"],
            "Renamed locally, never flushed"
        );
        let rows = assert_recoverable_issue_and_flush(&workspace, &db_path);
        assert_eq!(
            rows.iter().find(|row| row["id"] == "leg-n57").unwrap()["title"],
            "Renamed locally, never flushed"
        );
    }
}

#[test]
fn discard_unreadable_payload_does_not_omit_a_different_recoverable_issue() {
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
    assert!(error.contains("unreadable"), "{error}");
    assert!(
        error.contains("1 recoverable database-only issue(s) (leg-u4i)"),
        "{error}"
    );
    assert_eq!(fs::read(&db_path).unwrap(), before);

    let preview = flagged_heal(&workspace, true);
    let action = preview["planned_action"].as_str().unwrap();
    assert!(action.contains("re-add 1 database-only issue(s)"), "{action}");
    assert!(
        action.contains("1 unreadable or superseded database-only issue(s)"),
        "{action}"
    );
    assert_eq!(fs::read(&db_path).unwrap(), before);

    let outcome = flagged_heal(&workspace, false);
    assert_eq!(outcome["restored_db_only"], serde_json::json!(["leg-u4i"]));
    assert_eq!(outcome["backup_only_db_only"], serde_json::json!(["leg-n57"]));
    assert!(Path::new(outcome["backup"].as_str().unwrap()).is_dir());
    let _ = assert_recoverable_issue_and_flush(&workspace, &db_path);
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

    let outcome = flagged_heal(&workspace, false);
    assert_eq!(outcome["restored_db_only"], serde_json::json!(["leg-u4i"]));
    assert_eq!(outcome["backup_only_db_only"], serde_json::json!(["leg-n57"]));
    assert_eq!(
        show_without_sync(&workspace, "leg-n57")["title"],
        "Newer upstream edit"
    );
    let rows = assert_recoverable_issue_and_flush(&workspace, &db_path);
    assert_eq!(
        rows.iter().find(|row| row["id"] == "leg-n57").unwrap()["title"],
        "Newer upstream edit"
    );
}
