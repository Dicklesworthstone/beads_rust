//! Regressions for failures after a valid JSONL import but during local replay.
//! Neither an explicit heal nor --discard-db-only may first replace the live
//! database and only then discover that readable local work cannot be restored.

mod common;

use beads_rust::franken_sync::Connection;
use common::cli::{BrWorkspace, run_br};
use flate2::read::GzDecoder;
use serde_json::Value;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

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
    assert!(message.contains("restore rehearsal failed"), "{message}");
    assert!(message.contains("live database rebuild was not started"), "{message}");
    assert_eq!(header_version(db), 4, "failure must not mark the tracker current");
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

fn show(workspace: &BrWorkspace, id: &str) -> Value {
    let result = run_br(
        workspace,
        ["show", id, "--json", "--no-auto-import", "--no-auto-flush"],
        &format!("show_{id}"),
    );
    assert!(result.status.success(), "{} {}", result.stdout, result.stderr);
    json(&result.stdout)[0].clone()
}

#[test]
fn resolving_the_conflict_allows_a_lossless_retry_and_export() {
    let workspace = BrWorkspace::new();
    let db = fixture(&workspace);
    assert_refused_before_replacement(&workspace, &db, false);
    set_upstream_reference(&workspace, "different-upstream-reference");

    let result = run_br(
        &workspace,
        ["doctor", "migrate-schema", "heal", "--json"],
        "heal_after_resolving_conflict",
    );
    assert!(result.status.success(), "{} {}", result.stdout, result.stderr);
    let receipt = json(&result.stdout);
    assert_eq!(receipt["action"], "rebuilt");
    assert_eq!(receipt["restored_db_only"], serde_json::json!(["leg-u4i"]));
    assert_eq!(receipt["backup_only_db_only"], serde_json::json!(["leg-n57"]));
    assert!(Path::new(receipt["backup"].as_str().unwrap()).is_dir());
    assert_eq!(
        header_version(&db),
        u32::try_from(beads_rust::storage::schema::CURRENT_SCHEMA_VERSION).unwrap()
    );
    let local = show(&workspace, "leg-u4i");
    assert_eq!(local["title"], "Unflushed legacy issue");
    assert_eq!(local["external_ref"], "replay-conflict");
    assert_eq!(local["labels"], serde_json::json!(["preserve-local"]));
    assert_eq!(local["comments"][0]["text"], "preserve this comment");
    assert_eq!(local["dependencies"][0]["id"], "leg-n57");
    let upstream = show(&workspace, "leg-n57");
    assert_eq!(upstream["title"], "Superseding upstream title");
    assert_eq!(upstream["external_ref"], "different-upstream-reference");

    let flush = run_br(&workspace, ["sync", "--flush-only", "--json"], "flush_retry");
    assert!(flush.status.success(), "{} {}", flush.stdout, flush.stderr);
    let rows: Vec<Value> = fs::read_to_string(workspace.root.join(".beads/issues.jsonl"))
        .unwrap()
        .lines()
        .map(json)
        .collect();
    let local = rows.iter().find(|row| row["id"] == "leg-u4i").unwrap();
    assert_eq!(local["external_ref"], "replay-conflict");
    assert_eq!(local["labels"], serde_json::json!(["preserve-local"]));
    assert_eq!(local["comments"][0]["text"], "preserve this comment");
    assert_eq!(local["dependencies"][0]["depends_on_id"], "leg-n57");
    let upstream = rows.iter().find(|row| row["id"] == "leg-n57").unwrap();
    assert_eq!(upstream["title"], "Superseding upstream title");
}
