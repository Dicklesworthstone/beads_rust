//! GH #551: JSONL representability must not gate a lossless raw-row migration.
//! Use the released schema-16 fixture used by e2e_schema_heal, not a current
//! schema with only its version header changed. No test bypasses admission.

mod common;

use beads_rust::franken_sync::{Connection, SqliteValue};
use common::cli::{BrWorkspace, extract_json_payload, run_br};
use flate2::read::GzDecoder;
use serde_json::Value;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

const GHOST: &str = "v15ws-551-ghost";

fn workspace() -> (BrWorkspace, PathBuf) {
    let workspace = BrWorkspace::new();
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/schema_migration");
    let beads = workspace.root.join(".beads");
    fs::create_dir_all(&beads).unwrap();
    let mut decoder =
        GzDecoder::new(fs::File::open(fixtures.join("schema16_v0219_release.db.gz")).unwrap());
    let mut bytes = Vec::new();
    decoder.read_to_end(&mut bytes).unwrap();
    assert_eq!(u32::from_be_bytes(bytes[60..64].try_into().unwrap()), 16);
    let db = beads.join("beads.db");
    fs::write(&db, bytes).unwrap();
    fs::copy(
        fixtures.join("schema16_issues.jsonl"),
        beads.join("issues.jsonl"),
    )
    .unwrap();
    fs::copy(
        fixtures.join("schema16_config.yaml"),
        beads.join("config.yaml"),
    )
    .unwrap();
    (workspace, db)
}

fn edit(db: &Path, sql: &str) {
    let conn = Connection::open(db.to_string_lossy().into_owned()).unwrap();
    conn.execute("PRAGMA foreign_keys = OFF").unwrap();
    conn.execute(sql).unwrap();
    conn.close().unwrap();
}

fn add_label(db: &Path, whitespace: bool) {
    let label = if whitespace { "   " } else { "retained-orphan" };
    edit(
        db,
        &format!("INSERT INTO labels (issue_id, label) VALUES ('{GHOST}', '{label}')"),
    );
}

fn json(stdout: &str) -> Value {
    serde_json::from_str(&extract_json_payload(stdout)).unwrap()
}

fn protected_bytes(db: &Path) -> [Option<Vec<u8>>; 2] {
    ["", "-wal"].map(|suffix| {
        let mut name = db.as_os_str().to_os_string();
        name.push(suffix);
        match fs::read(PathBuf::from(name)) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => panic!("reading protected family: {error}"),
        }
    })
}

/// Observe through a private copy; the verification must not repair/checkpoint
/// the tested family or erase the evidence of a failed attempt.
fn query_copy(db: &Path, sql: &str) -> Vec<Vec<SqliteValue>> {
    let scratch = tempfile::tempdir().unwrap();
    let copy = scratch.path().join("copy.db");
    for (suffix, bytes) in ["", "-wal"].into_iter().zip(protected_bytes(db)) {
        if let Some(bytes) = bytes {
            let mut name = copy.as_os_str().to_os_string();
            name.push(suffix);
            fs::write(PathBuf::from(name), bytes).unwrap();
        }
    }
    let conn = Connection::open(copy.to_string_lossy().into_owned()).unwrap();
    let rows = conn
        .query(sql)
        .unwrap()
        .into_iter()
        .map(|row| row.values().to_vec())
        .collect();
    conn.close().unwrap();
    rows
}

fn labels(db: &Path) -> Vec<Vec<SqliteValue>> {
    query_copy(
        db,
        &format!("SELECT issue_id, label FROM labels WHERE issue_id = '{GHOST}'"),
    )
}

fn assert_current(db: &Path) {
    assert_eq!(
        query_copy(db, "PRAGMA user_version")[0][0].as_integer(),
        Some(i64::from(
            beads_rust::storage::schema::CURRENT_SCHEMA_VERSION
        ))
    );
}

#[test]
fn automatic_migration_keeps_rows_rejected_by_the_jsonl_audit() {
    let (workspace, db) = workspace();
    add_label(&db, true);
    let before = labels(&db);
    assert_eq!(before.len(), 1);
    let jsonl = workspace.root.join(".beads/issues.jsonl");
    let exported = fs::read(&jsonl).unwrap();

    let create = run_br(
        &workspace,
        [
            "create",
            "Created after raw-row migration",
            "--no-auto-flush",
            "--json",
        ],
        "551_automatic",
    );
    assert!(
        create.status.success(),
        "{} {}",
        create.stdout,
        create.stderr
    );
    assert!(
        create.stderr.contains("migrated it in place"),
        "{}",
        create.stderr
    );
    assert_eq!(
        json(&create.stdout)["title"],
        "Created after raw-row migration"
    );
    assert_current(&db);
    assert_eq!(
        labels(&db),
        before,
        "do not normalize or discard raw labels"
    );
    assert_eq!(fs::read(jsonl).unwrap(), exported);
}

#[test]
fn explicit_migration_and_dry_run_do_not_require_a_successful_jsonl_audit() {
    let (workspace, db) = workspace();
    add_label(&db, true);
    let before = protected_bytes(&db);
    let raw_labels = labels(&db);
    let dry = run_br(
        &workspace,
        ["doctor", "migrate-schema", "heal", "--dry-run", "--json"],
        "551_dry_run",
    );
    assert!(dry.status.success(), "{} {}", dry.stdout, dry.stderr);
    let report = json(&dry.stdout);
    assert!(
        report["audit"].is_null(),
        "a failed audit must not look clean: {report}"
    );
    assert!(
        report["audit_error"]
            .as_str()
            .unwrap()
            .contains("labels.label")
    );
    assert_eq!(report["automatic_rebuild_allowed"], false);
    assert!(
        report["planned_action"]
            .as_str()
            .unwrap()
            .contains("reviewed in-place migration")
    );
    assert_eq!(protected_bytes(&db), before, "dry run must not migrate");

    let heal = run_br(
        &workspace,
        ["doctor", "migrate-schema", "heal", "--json"],
        "551_explicit",
    );
    assert!(heal.status.success(), "{} {}", heal.stdout, heal.stderr);
    let outcome = json(&heal.stdout);
    assert_eq!(outcome["action"], "migrated");
    assert!(Path::new(outcome["backup"].as_str().unwrap()).exists());
    assert_current(&db);
    assert_eq!(labels(&db), raw_labels);
}

#[test]
fn reviewed_migration_does_not_depend_on_a_present_or_parseable_export() {
    for missing in [true, false] {
        let (workspace, db) = workspace();
        let jsonl = workspace.root.join(".beads/issues.jsonl");
        // Retain the source export rather than deleting test evidence.
        fs::rename(&jsonl, jsonl.with_extension("retained.jsonl")).unwrap();
        if !missing {
            fs::write(&jsonl, "{not valid JSONL\n").unwrap();
        }
        let before = fs::read(&jsonl).ok();
        let heal = run_br(
            &workspace,
            ["doctor", "migrate-schema", "heal", "--json"],
            "551_without_export",
        );
        assert!(
            heal.status.success(),
            "missing={missing}: {} {}",
            heal.stdout,
            heal.stderr
        );
        assert_eq!(json(&heal.stdout)["action"], "migrated");
        assert_current(&db);
        assert_eq!(
            fs::read(&jsonl).ok(),
            before,
            "migration must not touch JSONL"
        );
    }
}

#[test]
fn failed_reviewed_migration_cannot_bypass_the_strict_fallback_audit() {
    let (workspace, db) = workspace();
    add_label(&db, true);
    // The reviewed migrator requires this source table. Removing it makes
    // migration ineligible before commit, independently of JSONL decoding.
    edit(&db, "DROP TABLE dirty_issues");
    let before = protected_bytes(&db);
    let exported = fs::read(workspace.root.join(".beads/issues.jsonl")).unwrap();
    let commands = [
        vec!["create", "Must stay refused", "--json"],
        vec!["doctor", "migrate-schema", "heal", "--json"],
        vec![
            "doctor",
            "migrate-schema",
            "heal",
            "--discard-db-only",
            "--json",
        ],
    ];
    for args in commands {
        let run = run_br(&workspace, args, "551_failed_migration");
        assert!(!run.status.success(), "{} {}", run.stdout, run.stderr);
        let output = format!("{}{}", run.stdout, run.stderr);
        // Neither mode may turn a refused reviewed migration into a JSONL
        // replacement that projects away the original malformed values.
        assert!(
            output.contains("migration") && output.contains("dirty_issues"),
            "{output}"
        );
        assert_eq!(protected_bytes(&db), before);
        assert_eq!(
            fs::read(workspace.root.join(".beads/issues.jsonl")).unwrap(),
            exported
        );
    }
}

#[test]
fn failed_reviewed_migration_cannot_automatically_drop_well_formed_orphans() {
    let (workspace, db) = workspace();
    add_label(&db, false);
    edit(&db, "DROP TABLE dirty_issues");
    let before = protected_bytes(&db);
    let run = run_br(
        &workspace,
        ["create", "Must stay refused", "--json"],
        "551_orphans",
    );
    assert!(!run.status.success(), "{} {}", run.stdout, run.stderr);
    let output = format!("{}{}", run.stdout, run.stderr);
    assert!(
        output.contains("migration") && output.contains("dirty_issues"),
        "{output}"
    );
    assert_eq!(protected_bytes(&db), before);
    assert_eq!(labels(&db).len(), 1);
}
