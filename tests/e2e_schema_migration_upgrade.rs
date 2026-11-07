//! E2E regression tests for issue #398: `br doctor migrate-schema` must be
//! able to upgrade every schema actually shipped since v13 to the current
//! schema — in particular schema 15 (the #388 gate-history schema from the
//! v0.2.19-era line) and schema 16 (created by the released v0.2.19 binary),
//! both of which the reviewed migration used to reject with
//! "available only for 13->17 and 14->17".
//!
//! Fixture provenance (NOT synthesized `PRAGMA user_version` stamps):
//! - `tests/fixtures/schema_migration/schema15_pre384_era.db.gz` was created
//!   by a br binary built from commit `7c4af2a6~1` (`d1b90640`), the last
//!   commit with `CURRENT_SCHEMA_VERSION = 15`, by running real `init` /
//!   `create` / `dep add` / `label add` / `comment add` / `close` / `sync
//!   --flush-only` commands.
//! - `tests/fixtures/schema_migration/schema16_v0219_release.db.gz` was
//!   created the same way by the actual released `br 0.2.19` binary
//!   (linux_x86_64 GitHub release asset), which stamps schema 16.
//! - `schema19_v076_release.db.gz` was created by the official v0.7.6
//!   `br-0.7.6-linux_amd64.tar.gz` asset (SHA-256
//!   `56baae2df5e7cc6f6537e05fd5f560192a9d5ceb02659c61b10b9610b45eb3ef`),
//!   tag `0cef79b3b35479d9c2a7bf4c7b762b781344bddf`. Real init/create,
//!   typed dep add, label add, comments add, close/reopen, config set and
//!   sync --flush-only commands produced three issues, two differently typed
//!   edges on the same pair, audit history, and a policy-governed close.
//!   No main-binary schema or fabricated user_version was used. The frozen
//!   DB SHA-256 is `b670442544fd842878b54795e3ad1f8cb9cd7347056b74743f172729770d8c03`;
//!   v0.7.6 stats succeeds and SQLite quick_check is ok. It lacks both
//!   jsonl_row_receipts and close_metadata.closed_by despite declaring 19.
//! - `schema19_v076_workflows.db.gz` independently freezes a richer official
//!   v0.7.6 tracker: four issues including a DB-only ephemeral issue, real
//!   gate/capacity history, closure attribution, and parallel typed relations.
//!   The adjacent provenance document records its commands and checksums.
//!
//! Each test follows exactly the remediation the SCHEMA_MISMATCH error
//! prints: plan -> apply -> verify data -> reject stale receipt -> undo ->
//! re-apply.

mod common;
#[path = "schema_migration/runtime_safety.rs"]
mod runtime_safety;

use beads_rust::franken_sync::{Connection, SqliteValue};
use common::cli::{BrRun, BrWorkspace, extract_json_payload, run_br};
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

const WAL_ONLY_TITLE: &str = "committed sentinel present only in WAL";
const FRAMED_WAL_REFUSAL: &str = "engine recovery refused before opening: WAL contains frames \
                                  that may hold committed, uncheckpointed changes";

/// GH #556: a WAL that still carries committed frames is refused before
/// anything is opened, copied or rewritten. No index is created, nothing is
/// quarantined, and the main file and the WAL keep every byte.
fn assert_framed_wal_refusal(
    workspace: &BrWorkspace,
    run: &BrRun,
    main_before: &[u8],
    wal_before: &[u8],
) {
    assert!(!run.status.success(), "{} {}", run.stdout, run.stderr);
    let output = format!("{}{}", run.stdout, run.stderr);
    assert!(
        output.contains(FRAMED_WAL_REFUSAL),
        "unexpected refusal: {output}"
    );
    assert_source_family_preserved(workspace, main_before, wal_before);
}

fn assert_source_family_preserved(workspace: &BrWorkspace, main_before: &[u8], wal_before: &[u8]) {
    assert!(!workspace.root.join(".beads/beads.db-shm").exists());
    assert!(
        !workspace
            .root
            .join(".beads/.br_recovery/schema-migrations")
            .exists()
    );
    assert_eq!(
        fs::read(workspace.root.join(".beads/beads.db")).unwrap(),
        main_before
    );
    assert_eq!(
        fs::read(workspace.root.join(".beads/beads.db-wal")).unwrap(),
        wal_before
    );
}

fn current_workspace_without_wal_index(pending_merge: bool) -> BrWorkspace {
    let workspace = BrWorkspace::new();
    let init = run_br(
        &workspace,
        ["init", "--prefix", "wal", "--json"],
        "wal_init",
    );
    assert!(init.status.success(), "{} {}", init.stdout, init.stderr);
    let create = run_br(
        &workspace,
        ["create", "checkpointed title", "--json"],
        "wal_create",
    );
    assert!(
        create.status.success(),
        "{} {}",
        create.stdout,
        create.stderr
    );
    let db_path = workspace.root.join(".beads/beads.db");
    let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
    conn.execute("PRAGMA journal_mode=WAL").unwrap();
    conn.execute("PRAGMA wal_autocheckpoint=0").unwrap();
    conn.execute(&format!("UPDATE issues SET title='{WAL_ONLY_TITLE}'"))
        .unwrap();
    if pending_merge {
        conn.execute("INSERT INTO metadata (key, value) VALUES ('sync_merge_pending_v1', 'wal-only-pending-receipt')").unwrap();
    }
    // The facade's Drop closes without checkpointing, retaining committed
    // frames. Explicit close() would copy the sentinel into the main file.
    drop(conn);
    let main = fs::read(&db_path).unwrap();
    let wal = fs::read(workspace.root.join(".beads/beads.db-wal")).unwrap();
    for sentinel in
        std::iter::once(WAL_ONLY_TITLE).chain(pending_merge.then_some("wal-only-pending-receipt"))
    {
        assert!(
            !main
                .windows(sentinel.len())
                .any(|bytes| bytes == sentinel.as_bytes())
        );
        assert!(
            wal.windows(sentinel.len())
                .any(|bytes| bytes == sentinel.as_bytes())
        );
    }
    fs::rename(
        workspace.root.join(".beads/beads.db-shm"),
        workspace.root.join("retained-matching-shm"),
    )
    .unwrap();
    workspace
}

#[test]
fn missing_wal_index_reads_private_snapshot_but_mutations_refuse_framed_wal() {
    // The committed rows stay in the WAL; recovering them is the owning
    // engine's checkpoint, not an index-only recovery (#562).
    let workspace = current_workspace_without_wal_index(false);
    let main_before = fs::read(workspace.root.join(".beads/beads.db")).unwrap();
    let wal_before = fs::read(workspace.root.join(".beads/beads.db-wal")).unwrap();
    let list = run_br(&workspace, ["list", "--all", "--json"], "framed_list");
    assert!(list.status.success(), "{} {}", list.stdout, list.stderr);
    assert!(list.stdout.contains(WAL_ONLY_TITLE));
    assert_source_family_preserved(&workspace, &main_before, &wal_before);
    let create = run_br(
        &workspace,
        ["create", "must never be created", "--json"],
        "framed_create",
    );
    assert_framed_wal_refusal(&workspace, &create, &main_before, &wal_before);
}

#[test]
fn missing_wal_index_pending_merge_create_refuses_framed_wal() {
    // The frame refusal precedes the pending-receipt gate, so the receipt in
    // the WAL is never read and nothing is created.
    let workspace = current_workspace_without_wal_index(true);
    let main_before = fs::read(workspace.root.join(".beads/beads.db")).unwrap();
    let wal_before = fs::read(workspace.root.join(".beads/beads.db-wal")).unwrap();
    let create = run_br(
        &workspace,
        ["create", "must never be created", "--json"],
        "pending_refusal",
    );
    assert_framed_wal_refusal(&workspace, &create, &main_before, &wal_before);
}

#[test]
fn missing_wal_index_explicit_read_only_remains_nonmutating() {
    let workspace = current_workspace_without_wal_index(false);
    let main_path = workspace.root.join(".beads/beads.db");
    let wal_path = workspace.root.join(".beads/beads.db-wal");
    let main_before = fs::read(&main_path).unwrap();
    let wal_before = fs::read(&wal_path).unwrap();
    let list = run_br(
        &workspace,
        ["list", "--json", "--no-auto-import", "--no-auto-flush"],
        "readonly_snapshot",
    );
    assert!(list.status.success(), "{} {}", list.stdout, list.stderr);
    assert!(list.stdout.contains(WAL_ONLY_TITLE));
    assert!(!workspace.root.join(".beads/beads.db-shm").exists());
    assert_eq!(fs::read(main_path).unwrap(), main_before);
    assert_eq!(fs::read(wal_path).unwrap(), wal_before);
}

#[test]
fn missing_wal_index_observational_sync_remains_nonmutating() {
    for args in [
        vec!["sync", "--status", "--json"],
        vec!["sync", "--reconcile", "--dry-run", "--json"],
    ] {
        let workspace = current_workspace_without_wal_index(false);
        let db_path = workspace.root.join(".beads/beads.db");
        let wal_path = workspace.root.join(".beads/beads.db-wal");
        let main_before = fs::read(&db_path).unwrap();
        let wal_before = fs::read(&wal_path).unwrap();
        let result = run_br(&workspace, args, "observational_snapshot");
        assert!(
            result.status.success(),
            "{} {}",
            result.stdout,
            result.stderr
        );
        assert!(!workspace.root.join(".beads/beads.db-shm").exists());
        assert!(
            !workspace
                .root
                .join(".beads/.br_recovery/schema-migrations")
                .exists()
        );
        assert_eq!(fs::read(db_path).unwrap(), main_before);
        assert_eq!(fs::read(wal_path).unwrap(), wal_before);
    }
}

#[test]
fn missing_wal_index_doctor_reads_pending_receipt_without_live_repair() {
    let workspace = current_workspace_without_wal_index(true);
    let db_path = workspace.root.join(".beads/beads.db");
    let wal_path = workspace.root.join(".beads/beads.db-wal");
    let main_before = fs::read(&db_path).unwrap();
    let wal_before = fs::read(&wal_path).unwrap();
    let doctor = run_br(&workspace, ["doctor", "--json"], "doctor_snapshot");
    let report: Value = serde_json::from_str(&extract_json_payload(&doctor.stdout)).unwrap();
    let checks = report["checks"].as_array().unwrap();
    let pending = checks
        .iter()
        .find(|check| check["name"] == "sync.merge_pending")
        .unwrap();
    assert_eq!(pending["details"]["pending"], true);
    assert!(pending["message"].as_str().unwrap().contains("legacy"));
    let observational = checks
        .iter()
        .find(|check| check["name"] == "db.read_only_open_observational")
        .unwrap();
    assert_eq!(observational["status"], "ok");
    assert!(!workspace.root.join(".beads/beads.db-shm").exists());
    assert_eq!(fs::read(db_path).unwrap(), main_before);
    assert_eq!(fs::read(wal_path).unwrap(), wal_before);
}

#[test]
fn missing_wal_index_read_with_a_live_peer_remains_observational() {
    // Shared readers may observe a verified private snapshot while a peer
    // holds its lease. A read must never request exclusive recovery authority.
    let workspace = current_workspace_without_wal_index(false);
    let db_path = workspace.root.join(".beads/beads.db");
    let main_before = fs::read(&db_path).unwrap();
    let wal_before = fs::read(workspace.root.join(".beads/beads.db-wal")).unwrap();
    let _peer = beads_rust::sync::DatabaseOpenerLease::register(&db_path).unwrap();
    let list = run_br(&workspace, ["list", "--json"], "peer_observation");
    assert!(list.status.success(), "{} {}", list.stdout, list.stderr);
    assert!(list.stdout.contains(WAL_ONLY_TITLE));
    assert_source_family_preserved(&workspace, &main_before, &wal_before);
}

#[test]
fn missing_wal_index_corrupt_wal_refuses_without_live_mutation() {
    // Private read-only snapshots validate committed WAL frames; they must
    // refuse a checksum failure without repairing or discarding the source.
    let workspace = current_workspace_without_wal_index(false);
    let wal_path = workspace.root.join(".beads/beads.db-wal");
    let main_before = fs::read(workspace.root.join(".beads/beads.db")).unwrap();
    let mut wal_before = fs::read(&wal_path).unwrap();
    wal_before[48] ^= 1; // First frame's checksum; a tolerant scan would ignore it.
    fs::write(&wal_path, &wal_before).unwrap();
    let list = run_br(&workspace, ["list", "--json"], "corrupt_refusal");
    assert!(!list.status.success(), "{} {}", list.stdout, list.stderr);
    let message = format!("{} {}", list.stdout, list.stderr).to_ascii_lowercase();
    assert!(message.contains("wal"), "{message}");
    assert_source_family_preserved(&workspace, &main_before, &wal_before);
}

fn install_fixture_workspace(workspace: &BrWorkspace, db_gz: &str, issues: &str, config: &str) {
    let beads_dir = workspace.root.join(".beads");
    fs::create_dir_all(&beads_dir).expect("create .beads");

    let mut decoder = GzDecoder::new(fs::File::open(fixture_dir().join(db_gz)).expect("open gz"));
    let mut db_bytes = Vec::new();
    decoder
        .read_to_end(&mut db_bytes)
        .expect("gunzip fixture db");
    fs::write(beads_dir.join("beads.db"), &db_bytes).expect("write beads.db");
    fs::copy(fixture_dir().join(issues), beads_dir.join("issues.jsonl")).expect("copy jsonl");
    fs::copy(fixture_dir().join(config), beads_dir.join("config.yaml")).expect("copy config");
}

fn header_user_version(db_path: &Path) -> u32 {
    let bytes = fs::read(db_path).expect("read db");
    u32::from_be_bytes(bytes[60..64].try_into().expect("db header"))
}

fn db_declares_table(db_path: &Path, table: &str) -> bool {
    // The sqlite_schema table stores the verbatim CREATE TABLE DDL (with or
    // without IF NOT EXISTS), so a raw byte scan is a connection-free
    // existence witness good enough for a test.
    let bytes = fs::read(db_path).expect("read db");
    [
        format!("CREATE TABLE {table}"),
        format!("CREATE TABLE IF NOT EXISTS {table}"),
    ]
    .iter()
    .any(|needle| {
        bytes
            .windows(needle.len())
            .any(|window| window == needle.as_bytes())
    })
}

type TableRows = std::collections::BTreeMap<String, (Vec<String>, Vec<Vec<String>>)>;

/// Every table's columns and rows, read through the runtime engine from a
/// private copy of the family (the live one is never opened here). With
/// `columns` given, reads exactly those columns of those tables.
fn engine_rows(db_path: &Path, columns: Option<&TableRows>) -> TableRows {
    let scratch = tempfile::tempdir().expect("scratch dir");
    let copy = scratch.path().join("copy.db");
    fs::copy(db_path, &copy).expect("copy database");
    let wal = PathBuf::from(format!("{}-wal", db_path.display()));
    if wal.is_file() {
        fs::copy(&wal, scratch.path().join("copy.db-wal")).expect("copy WAL");
    }
    let conn = Connection::open(copy.to_string_lossy().into_owned()).expect("open copy");
    let text = |value: Option<&SqliteValue>| match value {
        None => "<missing>".to_owned(),
        Some(SqliteValue::Null) => "NULL".to_owned(),
        Some(SqliteValue::Integer(value)) => format!("integer {value}"),
        Some(SqliteValue::Float(value)) => format!("real {value:?}"),
        Some(SqliteValue::Text(text)) if !text.is_valid_utf8() => {
            format!("raw text {:?}", text.as_bytes_direct())
        }
        Some(SqliteValue::Text(_)) => format!("text {:?}", value.and_then(SqliteValue::as_text)),
        Some(SqliteValue::Blob(bytes)) => format!("blob {bytes:?}"),
    };
    let tables: Vec<(String, Vec<String>)> = match columns {
        Some(columns) => columns
            .iter()
            .map(|(table, (names, _))| (table.clone(), names.clone()))
            .collect(),
        None => conn
            .query(
                "SELECT name FROM sqlite_master WHERE type = 'table' \
                 AND name NOT LIKE 'sqlite_%' ORDER BY name",
            )
            .expect("list tables")
            .iter()
            .map(|row| {
                let table = row
                    .get(0)
                    .and_then(SqliteValue::as_text)
                    .expect("table name")
                    .to_owned();
                let names = conn
                    .query(&format!("PRAGMA table_info(\"{table}\")"))
                    .expect("table_info")
                    .iter()
                    .map(|info| {
                        info.get(1)
                            .and_then(SqliteValue::as_text)
                            .expect("column name")
                            .to_owned()
                    })
                    .collect();
                (table, names)
            })
            .collect(),
    };
    let mut result = TableRows::new();
    for (table, names) in tables {
        let list = names
            .iter()
            .map(|name| format!("\"{name}\""))
            .collect::<Vec<_>>()
            .join(", ");
        let order = (1..=names.len())
            .map(|index| index.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let rows = conn
            .query(&format!("SELECT {list} FROM \"{table}\" ORDER BY {order}"))
            .unwrap_or_else(|error| panic!("read {table}: {error}"))
            .iter()
            .map(|row| (0..names.len()).map(|index| text(row.get(index))).collect())
            .collect();
        result.insert(table, (names, rows));
    }
    conn.close().expect("close copy");
    result
}

/// Reconstruct persisted state after installation but before publication of
/// the applied receipt. This is recovery-state coverage, not a process-kill
/// test. Retain the completed receipt as evidence, leave the real commit-ready
/// intent in place, and require an explicit resume to finish it.
fn interrupted_apply_keeps_authority_until_explicit_resume(
    workspace: &BrWorkspace,
    db_path: &Path,
    run_id: &str,
    plan_token: &str,
    rows_before: &TableRows,
) {
    let run_dir = workspace
        .root
        .join(".beads/.br_recovery/schema-migrations")
        .join(run_id);
    let applied_path = run_dir.join("applied.json");
    fs::rename(
        &applied_path,
        run_dir.join("applied.interruption-test.json"),
    )
    .expect("retain receipt while simulating interrupted finalization");
    let original_backup = fs::read(run_dir.join("before/beads.db")).expect("original backup");
    let intent = fs::read(run_dir.join("commit-ready.json")).expect("commit-ready intent");
    let database = fs::read(db_path).expect("installed database");
    let wal_path = PathBuf::from(format!("{}-wal", db_path.display()));
    let wal = fs::read(&wal_path).ok();
    for (index, command) in ["info", "list", "stats"].into_iter().enumerate() {
        let read = run_br(
            workspace,
            [command, "--json", "--no-auto-import", "--no-auto-flush"],
            &format!("read_during_interrupted_migration_{index}"),
        );
        assert!(read.status.success(), "{} {}", read.stdout, read.stderr);
        assert!(!applied_path.exists(), "a read must not finalize recovery");
        assert_eq!(fs::read(db_path).unwrap(), database);
        assert_eq!(fs::read(&wal_path).ok(), wal);
        assert_eq!(fs::read(run_dir.join("commit-ready.json")).unwrap(), intent);
        assert_eq!(
            fs::read(run_dir.join("before/beads.db")).unwrap(),
            original_backup
        );
    }
    let resume = run_br(
        workspace,
        [
            "doctor",
            "migrate-schema",
            "apply",
            "--plan-token",
            plan_token,
            "--json",
        ],
        "resume_interrupted_migration",
    );
    assert!(
        resume.status.success(),
        "{} {}",
        resume.stdout,
        resume.stderr
    );
    let receipt: Value = serde_json::from_str(&extract_json_payload(&resume.stdout)).unwrap();
    assert_eq!(receipt["run_id"], run_id);
    assert!(applied_path.is_file());
    assert_eq!(engine_rows(db_path, Some(rows_before)), *rows_before);
    assert_eq!(
        fs::read(run_dir.join("before/beads.db")).unwrap(),
        original_backup
    );
}

#[allow(clippy::too_many_lines)]
fn upgrade_fixture_end_to_end(
    label: &str,
    db_gz: &str,
    issues: &str,
    config: &str,
    expected_from: u64,
    expected_issue_total: u64,
) {
    let target = u32::try_from(beads_rust::storage::schema::CURRENT_SCHEMA_VERSION).unwrap();
    let workspace = BrWorkspace::new();
    install_fixture_workspace(&workspace, db_gz, issues, config);
    let db_path = workspace.root.join(".beads").join("beads.db");
    // #585: preserving issue counts is insufficient. These records have no
    // representation in issues.jsonl and must remain in the active database,
    // byte-valued cells included, after migration and after undo/re-apply.
    let source = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
    for sql in [
        "INSERT INTO config (key, value) VALUES ('compact_batch_size', '37')",
        "INSERT INTO config (key, value) VALUES ('compact_model', 'preserved-model')",
        "INSERT INTO config (key, value) VALUES ('compaction_enabled', 'false')",
        "INSERT INTO metadata (key, value) VALUES ('operator_checkpoint', 'not a derived cache')",
        "CREATE TABLE extension_authority (key TEXT PRIMARY KEY, payload BLOB NOT NULL)",
        "INSERT INTO extension_authority VALUES ('private', X'0001FF007F')",
        "UPDATE dependencies SET metadata = '{\"reason\":\"database-only payload\"}', \
         thread_id = 'database-only-thread', created_by = 'database-only-agent'",
    ] {
        source.execute(sql).expect("seed database-only authority");
    }
    let owner = source
        .query("SELECT id FROM issues ORDER BY id LIMIT 1")
        .unwrap()[0]
        .get(0)
        .expect("fixture issue")
        .clone();
    source
        .execute_with_params(
            "INSERT INTO comments (issue_id, author, text, created_at) \
             VALUES (?1, 'local-agent', 'database-only comment', '2026-10-31T00:00:00Z')",
            &[owner],
        )
        .unwrap();
    source.close().expect("close seeded source");
    assert_eq!(
        u64::from(header_user_version(&db_path)),
        expected_from,
        "{label}: fixture must genuinely be at schema {expected_from}"
    );

    // 1. An explicitly read-only command never upgrades the database; it reads
    //    the JSONL instead and names the heal command on stderr.
    let stats = run_br(
        &workspace,
        ["stats", "--json", "--no-auto-flush", "--no-auto-import"],
        "stats_schema_mismatch",
    );
    assert!(
        stats.status.success(),
        "{label}: read-only stats must fall back to the JSONL on an old schema; stdout: {} stderr: {}",
        stats.stdout,
        stats.stderr
    );
    assert!(
        stats.stderr.contains("migrate-schema plan --json"),
        "{label}: the fallback warning must name `br doctor migrate-schema plan --json`; got: {}",
        stats.stderr
    );
    serde_json::from_str::<Value>(&extract_json_payload(&stats.stdout))
        .expect("stdout stays clean JSON");
    assert_eq!(
        u64::from(header_user_version(&db_path)),
        expected_from,
        "{label}: a read-only fallback must not touch the database"
    );

    // Rows as they stand on the old schema, before any column is added.
    let rows_before = engine_rows(&db_path, None);
    assert!(
        rows_before
            .get("issues")
            .is_some_and(|(names, rows)| !rows.is_empty()
                && !names.iter().any(|name| name == "prerequisites")),
        "{label}: fixture precondition: issues rows on a schema without prerequisites"
    );

    // 2. Follow the remediation: plan must accept the fixture.
    let plan = run_br(
        &workspace,
        [
            "doctor",
            "migrate-schema",
            "plan",
            "--json",
            "--no-auto-flush",
            "--no-auto-import",
        ],
        "migrate_plan",
    );
    assert!(
        plan.status.success(),
        "{label}: plan must accept schema {expected_from}; stdout: {} stderr: {}",
        plan.stdout,
        plan.stderr
    );
    let plan_json: Value =
        serde_json::from_str(&extract_json_payload(&plan.stdout)).expect("plan JSON");
    assert_eq!(
        plan_json["eligible"],
        Value::Bool(true),
        "{label}: plan not eligible"
    );
    assert_eq!(plan_json["from_version"].as_u64(), Some(expected_from));
    assert_eq!(plan_json["to_version"].as_u64(), Some(u64::from(target)));
    assert_eq!(plan_json["forecast"]["prerequisites_column_added"], true);
    assert_eq!(plan_json["forecast"]["dependency_type_key_added"], true);
    let plan_token = plan_json["plan_token"]
        .as_str()
        .expect("plan token")
        .to_string();

    // 3. Apply migrates atomically to the current schema.
    let apply = run_br(
        &workspace,
        [
            "doctor",
            "migrate-schema",
            "apply",
            "--plan-token",
            &plan_token,
            "--json",
            "--no-auto-flush",
            "--no-auto-import",
        ],
        "migrate_apply",
    );
    assert!(
        apply.status.success(),
        "{label}: apply failed; stdout: {} stderr: {}",
        apply.stdout,
        apply.stderr
    );
    let applied_json: Value =
        serde_json::from_str(&extract_json_payload(&apply.stdout)).expect("applied JSON");
    let run_id = applied_json["run_id"].as_str().expect("run id").to_string();
    assert_eq!(
        header_user_version(&db_path),
        target,
        "{label}: post-apply schema"
    );
    for table in [
        "gate_result_history",
        "capacity_exemptions",
        "capacity_exemption_history",
        "capacity_occupancy",
    ] {
        assert!(
            db_declares_table(&db_path, table),
            "{label}: migrated database must declare {table}"
        );
    }

    // 4. Tracker data survives and ordinary commands work again.
    let stats_after = run_br(
        &workspace,
        ["stats", "--json", "--no-auto-flush", "--no-auto-import"],
        "stats_after_apply",
    );
    assert!(
        stats_after.status.success(),
        "{label}: stats after apply failed: {}",
        stats_after.stderr
    );
    let stats_json: Value =
        serde_json::from_str(&extract_json_payload(&stats_after.stdout)).expect("stats JSON");
    assert_eq!(
        stats_json["summary"]["total_issues"].as_u64(),
        Some(expected_issue_total),
        "{label}: issue count must survive the migration"
    );

    let list = run_br(
        &workspace,
        [
            "list",
            "--all",
            "--json",
            "--no-auto-flush",
            "--no-auto-import",
        ],
        "list_after_apply",
    );
    assert!(
        list.status.success(),
        "{label}: list failed: {}",
        list.stderr
    );

    // The migration appends columns with ALTER TABLE ADD COLUMN, leaving
    // existing records shorter than the new table. Every pre-existing row
    // must read back through the engine exactly as before, and the appended
    // column must read as its default (not a neighbour's value): the shape
    // of mcp_agent_mail's br-2hpuk, where short records decoded shifted.
    let rows_after = engine_rows(&db_path, Some(&rows_before));
    for (table, (names, before)) in &rows_before {
        let (_, after) = &rows_after[table];
        assert_eq!(
            after, before,
            "{label}: {table} rows ({names:?}) changed across the migration"
        );
    }
    let prerequisites = engine_rows(&db_path, None)["issues"].clone();
    let column = prerequisites
        .0
        .iter()
        .position(|name| name == "prerequisites")
        .expect("migrated issues has prerequisites");
    assert_eq!(prerequisites.1.len(), rows_before["issues"].1.len());
    for row in &prerequisites.1 {
        assert_eq!(
            row[column],
            format!("text {:?}", Some("")),
            "{label}: pre-existing issue must read the prerequisites default: {row:?}"
        );
    }

    if expected_from == 16 {
        interrupted_apply_keeps_authority_until_explicit_resume(
            &workspace,
            &db_path,
            &run_id,
            &plan_token,
            &rows_before,
        );
    }

    // 5. The consumed receipt is stale: re-plan reports nothing to do, and
    //    re-applying the old token must be rejected without mutating.
    let replan = run_br(
        &workspace,
        [
            "doctor",
            "migrate-schema",
            "plan",
            "--json",
            "--no-auto-flush",
            "--no-auto-import",
        ],
        "replan_after_apply",
    );
    assert!(
        replan.status.success(),
        "{label}: re-plan failed: {}",
        replan.stderr
    );
    let replan_json: Value =
        serde_json::from_str(&extract_json_payload(&replan.stdout)).expect("replan JSON");
    assert_eq!(
        replan_json["eligible"],
        Value::Bool(false),
        "{label}: second plan must be a no-op"
    );

    let stale_apply = run_br(
        &workspace,
        [
            "doctor",
            "migrate-schema",
            "apply",
            "--plan-token",
            &plan_token,
            "--json",
            "--no-auto-flush",
            "--no-auto-import",
        ],
        "stale_apply",
    );
    assert!(
        !stale_apply.status.success(),
        "{label}: stale plan token must be rejected; stdout: {}",
        stale_apply.stdout
    );
    assert_eq!(
        header_user_version(&db_path),
        target,
        "{label}: rejected stale apply must not mutate the database"
    );

    // 6. Undo restores the exact pre-migration family, and the migration can
    //    be re-planned and re-applied afterwards.
    let undo = run_br(
        &workspace,
        [
            "doctor",
            "migrate-schema",
            "undo",
            &run_id,
            "--json",
            "--no-auto-flush",
            "--no-auto-import",
        ],
        "migrate_undo",
    );
    assert!(
        undo.status.success(),
        "{label}: undo failed; stdout: {} stderr: {}",
        undo.stdout,
        undo.stderr
    );
    assert_eq!(
        u64::from(header_user_version(&db_path)),
        expected_from,
        "{label}: undo must restore the pre-migration schema version"
    );

    let plan2 = run_br(
        &workspace,
        [
            "doctor",
            "migrate-schema",
            "plan",
            "--json",
            "--no-auto-flush",
            "--no-auto-import",
        ],
        "plan_after_undo",
    );
    assert!(
        plan2.status.success(),
        "{label}: plan after undo failed: {}",
        plan2.stderr
    );
    let plan2_json: Value =
        serde_json::from_str(&extract_json_payload(&plan2.stdout)).expect("plan2 JSON");
    assert_eq!(plan2_json["eligible"], Value::Bool(true));
    let token2 = plan2_json["plan_token"]
        .as_str()
        .expect("token2")
        .to_string();
    let apply2 = run_br(
        &workspace,
        [
            "doctor",
            "migrate-schema",
            "apply",
            "--plan-token",
            &token2,
            "--json",
            "--no-auto-flush",
            "--no-auto-import",
        ],
        "apply_after_undo",
    );
    assert!(
        apply2.status.success(),
        "{label}: apply after undo failed; stdout: {} stderr: {}",
        apply2.stdout,
        apply2.stderr
    );
    assert_eq!(header_user_version(&db_path), target);
    assert_eq!(
        engine_rows(&db_path, Some(&rows_before)),
        rows_before,
        "{label}: undo and re-apply must retain every original database value"
    );
}

/// Schema 15 (gate-history era, pre-#384) upgrades to the current schema.
#[test]
fn e2e_migrate_schema_upgrades_real_schema15_database() {
    let _log = common::test_log("e2e_migrate_schema_upgrades_real_schema15_database");
    upgrade_fixture_end_to_end(
        "schema15",
        "schema15_pre384_era.db.gz",
        "schema15_issues.jsonl",
        "schema15_config.yaml",
        15,
        2,
    );
}

/// Schema 16 (as created by the released v0.2.19 binary) upgrades to the
/// current schema.
#[test]
fn e2e_migrate_schema_upgrades_real_schema16_database() {
    let _log = common::test_log("e2e_migrate_schema_upgrades_real_schema16_database");
    upgrade_fixture_end_to_end(
        "schema16",
        "schema16_v0219_release.db.gz",
        "schema16_issues.jsonl",
        "schema16_config.yaml",
        16,
        3,
    );
}

fn released_schema19_workspace() -> BrWorkspace {
    let workspace = BrWorkspace::new();
    install_fixture_workspace(
        &workspace,
        "schema19_v076_release.db.gz",
        "schema19_v076_issues.jsonl",
        "schema19_v076_config.yaml",
    );
    workspace
}

fn edit_schema19_fixture(workspace: &BrWorkspace, statements: &[&str]) {
    let db = workspace.root.join(".beads/beads.db");
    let connection = Connection::open(db.to_string_lossy().into_owned()).expect("open fixture");
    for sql in statements {
        connection.execute(sql).expect("seed release fixture");
    }
    connection.close().expect("close release fixture");
}

/// Enrich the released layout with DB-only authority; the frozen fixture
/// itself remains a real release product. These records cannot be recovered
/// by importing its three-issue JSONL, even if issue counts happen to match.
fn seed_schema19_database_authority(workspace: &BrWorkspace) {
    edit_schema19_fixture(
        workspace,
        &[
            "INSERT INTO issues (id, title, created_at, updated_at) VALUES \
             ('rel19-local', 'Database-only issue', '2026-11-01T00:00:00Z', '2026-11-01T00:00:00Z')",
            "INSERT INTO dirty_issues (issue_id, marked_at) VALUES ('rel19-local', '2026-11-01T00:00:00Z')",
            "INSERT INTO events (id, issue_id, event_type, actor, old_value, new_value, comment, agent_name, harness, model) \
             VALUES (73, 'rel19-local', 'created', 'local-actor', NULL, 'database-only value', 'local audit', 'local-agent', 'local-harness', 'local-model')",
            "INSERT INTO comments (id, issue_id, author, text) VALUES (41, 'rel19-local', 'local-actor', 'Database-only comment')",
            "INSERT INTO config (key, value) VALUES ('compact_batch_size', '37'), ('compact_model', 'preserved-model'), ('compaction_enabled', 'false')",
            "INSERT INTO config (rowid, key, value) VALUES (101, 'operator_duplicate', 'older'), (207, 'operator_duplicate', 'newer')",
            "INSERT INTO metadata (rowid, key, value) VALUES (173, 'operator_checkpoint', 'not a derived cache')",
            "INSERT INTO child_counters (parent_id, last_child) VALUES ('rel19-local', 47)",
            "UPDATE dependencies SET thread_id = 'database-only-thread', created_by = 'database-only-agent'",
            "UPDATE close_metadata SET closed_by_agent_name = 'retained-agent', closed_by_harness = 'retained-harness', \
             closed_by_model = 'retained-model', bypass_reason = 'retained audit detail'",
            "INSERT INTO gate_results (issue_id, gate, provider, passed, note, recorded_by) \
             VALUES ('rel19-local', 'tests', 'ci', 1, 'database-only gate', 'local-actor')",
            "INSERT INTO gate_result_history (id, issue_id, from_status, to_status, status_revision, gate, provider, passed, note, recorded_by) \
             VALUES (29, 'rel19-local', 'open', 'closed', 0, 'tests', 'ci', 1, 'database-only history', 'local-actor')",
            "INSERT INTO capacity_exemptions (issue_id, capacity_kind, capacity_name, provider, reason, granted_by) \
             VALUES ('rel19-local', 'status', 'review', 'operator', 'preserve exemption', 'local-actor')",
            "INSERT INTO capacity_exemption_history (id, issue_id, capacity_kind, capacity_name, action, provider, reason, actor) \
             VALUES (31, 'rel19-local', 'status', 'review', 'grant', 'operator', 'preserve exemption history', 'local-actor')",
            "INSERT INTO capacity_occupancy (issue_id, actor, agent_name, harness, session) \
             VALUES ('rel19-local', 'local-actor', 'local-agent', 'local-harness', 'local-session')",
            "CREATE TABLE operator_notes (id INTEGER PRIMARY KEY AUTOINCREMENT, note TEXT NOT NULL, payload BLOB NOT NULL)",
            "CREATE INDEX operator_notes_lookup ON operator_notes(note)",
            "INSERT INTO operator_notes VALUES (12, 'custom durable state', X'0001FF007F')",
            "UPDATE sqlite_sequence SET seq = 91 WHERE name = 'operator_notes'",
        ],
    );
}

fn schema19_tracker_files(directory: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    let mut files = std::collections::BTreeMap::new();
    let mut directories = vec![directory.to_path_buf()];
    while let Some(current) = directories.pop() {
        for entry in fs::read_dir(current).expect("read tracker directory") {
            let path = entry.expect("tracker entry").path();
            if path.is_dir() {
                directories.push(path);
            } else {
                files.insert(
                    path.strip_prefix(directory).unwrap().to_path_buf(),
                    fs::read(path).expect("read tracker file"),
                );
            }
        }
    }
    files
}

/// Include hidden rowids and AUTOINCREMENT high-water marks, not just values
/// projected through br's model. Duplicate config precedence depends on rowid.
fn schema19_authority_rows(db: &Path, original: Option<&TableRows>) -> TableRows {
    if let Some(original) = original {
        return engine_rows(db, Some(original));
    }
    let mut columns = engine_rows(db, None);
    for (names, _) in columns.values_mut() {
        names.insert(0, "_rowid_".to_owned());
    }
    columns.insert(
        "sqlite_sequence".to_owned(),
        (vec!["name".to_owned(), "seq".to_owned()], Vec::new()),
    );
    engine_rows(db, Some(&columns))
}

/// VACUUM may normalize DDL whitespace, quoting and parentheses. Inspect the
/// operator's actual column/index constraints and exercise their behavior on
/// a disposable copy instead of treating SQL formatting as authoritative.
fn schema19_operator_shape(
    db: &Path,
) -> std::collections::BTreeMap<&'static str, Vec<Vec<SqliteValue>>> {
    let scratch = tempfile::tempdir().expect("operator schema scratch");
    let copy = scratch.path().join("copy.db");
    fs::copy(db, &copy).expect("copy database");
    let wal = PathBuf::from(format!("{}-wal", db.display()));
    if wal.is_file() {
        fs::copy(wal, scratch.path().join("copy.db-wal")).expect("copy WAL");
    }
    let connection = Connection::open(copy.to_string_lossy().into_owned()).expect("open copy");
    let shape = [
        ("columns", "PRAGMA table_info('operator_notes')"),
        ("foreign_keys", "PRAGMA foreign_key_list('operator_notes')"),
        ("indexes", "PRAGMA index_list('operator_notes')"),
        ("index_keys", "PRAGMA index_xinfo('operator_notes_lookup')"),
    ]
    .into_iter()
    .map(|(name, sql)| {
        let rows = connection
            .query(sql)
            .expect("operator schema query")
            .iter()
            .map(|row| row.values().to_vec())
            .collect();
        (name, rows)
    })
    .collect();
    connection
        .execute("INSERT INTO operator_notes (note, payload) VALUES ('next reservation', X'01')")
        .expect("operator table remains writable");
    let next_id = connection
        .query("SELECT id FROM operator_notes WHERE note = 'next reservation'")
        .expect("read next reserved id");
    assert_eq!(next_id[0].get(0), Some(&SqliteValue::Integer(92)));
    for sql in [
        "INSERT INTO operator_notes (note, payload) VALUES (NULL, X'01')",
        "INSERT INTO operator_notes (note, payload) VALUES ('missing payload', NULL)",
        "INSERT INTO operator_notes VALUES (12, 'duplicate primary key', X'01')",
    ] {
        assert!(
            connection.execute(sql).is_err(),
            "operator constraint lost: {sql}"
        );
    }
    connection.close().expect("close operator schema copy");
    shape
}

fn schema19_json(run: &BrRun) -> Value {
    assert!(run.status.success(), "{} {}", run.stdout, run.stderr);
    serde_json::from_str(&run.stdout).expect("complete stdout must be JSON")
}

#[test]
#[allow(clippy::too_many_lines)]
fn released_schema19_plan_apply_undo_preserves_every_original_value() {
    let workspace = released_schema19_workspace();
    let beads = workspace.root.join(".beads");
    let db = beads.join("beads.db");
    let released = engine_rows(&db, None);
    assert_eq!(header_user_version(&db), 19);
    assert_eq!(released["issues"].1.len(), 3);
    assert_eq!(released["dependencies"].1.len(), 2);
    assert_eq!(released["close_metadata"].1.len(), 1);
    assert!(!released.contains_key("jsonl_row_receipts"));
    assert!(
        !released["close_metadata"]
            .0
            .iter()
            .any(|name| name == "closed_by")
    );
    seed_schema19_database_authority(&workspace);
    let before = schema19_authority_rows(&db, None);
    let operator_shape = schema19_operator_shape(&db);
    let source_files = schema19_tracker_files(&beads);
    let catalog_columns = TableRows::from([(
        "sqlite_master".to_owned(),
        (
            vec!["type".to_owned(), "name".to_owned(), "tbl_name".to_owned()],
            Vec::new(),
        ),
    )]);
    let catalog_before = engine_rows(&db, Some(&catalog_columns));

    let stats = run_br(&workspace, ["stats", "--json"], "release19_stats_before");
    assert!(!stats.status.success(), "{}", stats.stdout);
    assert!(format!("{}{}", stats.stdout, stats.stderr).contains("migrate-schema"));
    assert_eq!(schema19_tracker_files(&beads), source_files);
    let additions = serde_json::json!({
        "jsonl_row_receipts_created": true,
        "closed_by_column_added": true,
    });
    let doctor = run_br(&workspace, ["doctor", "--json"], "release19_doctor");
    assert!(!doctor.status.success(), "{}", doctor.stdout);
    let inspection: Value = serde_json::from_str(&doctor.stdout).expect("doctor JSON");
    let runtime = inspection["checks"]
        .as_array()
        .and_then(|checks| {
            checks
                .iter()
                .find(|check| check["name"] == "schema.runtime")
        })
        .expect("doctor must diagnose the current-version layout");
    assert_eq!(runtime["status"], "error");
    assert_eq!(runtime["details"]["schema19_runtime_upgrade"], additions);
    assert!(
        runtime["details"]["downgrade"]
            .as_str()
            .is_some_and(|note| note.contains("v0.7.6"))
    );
    assert_eq!(schema19_tracker_files(&beads), source_files);

    let heal = schema19_json(&run_br(
        &workspace,
        ["doctor", "migrate-schema", "heal", "--dry-run", "--json"],
        "release19_heal_preview",
    ));
    assert_eq!(heal["heal_needed"], true);
    assert_eq!(heal["migration_eligible"], true);
    assert_eq!(heal["from_version"], 19);
    assert_eq!(heal["to_version"], 19);
    assert!(
        heal["note"]
            .as_str()
            .is_some_and(|note| note.contains("not a safe downgrade"))
    );
    assert_eq!(schema19_tracker_files(&beads), source_files);

    let plan = schema19_json(&run_br(
        &workspace,
        ["doctor", "migrate-schema", "plan", "--json"],
        "release19_plan",
    ));
    assert_eq!(plan["eligible"], true);
    assert_eq!(plan["from_version"], 19);
    assert_eq!(plan["to_version"], 19);
    assert!(
        plan["note"]
            .as_str()
            .is_some_and(|note| note.contains("v0.7.6") && note.contains("not a safe downgrade"))
    );
    assert_eq!(plan["forecast"]["schema19_runtime_upgrade"], additions);
    assert_eq!(heal["schema19_runtime_upgrade"], additions);
    assert_eq!(heal["plan_token"], plan["plan_token"]);
    assert_eq!(heal["migration_plan"], plan);
    assert_schema19_preservation_profile(&plan);
    assert_eq!(schema19_tracker_files(&beads), source_files);
    let token = plan["plan_token"].as_str().expect("upgrade token");
    let applied = schema19_json(&run_br(
        &workspace,
        [
            "doctor",
            "migrate-schema",
            "apply",
            "--plan-token",
            token,
            "--json",
        ],
        "release19_apply",
    ));
    assert_eq!(applied["effects"]["schema19_runtime_upgrade"], additions);
    assert_eq!(header_user_version(&db), 19);
    assert_eq!(schema19_authority_rows(&db, Some(&before)), before);
    assert_eq!(schema19_operator_shape(&db), operator_shape);
    let catalog_after = engine_rows(&db, Some(&catalog_columns));
    for row in &catalog_before["sqlite_master"].1 {
        // Existing schema object names, types and ownership must survive.
        // Engine-normalized SQL spelling is checked semantically above.
        assert!(
            catalog_after["sqlite_master"].1.contains(row),
            "schema object changed: {row:?}"
        );
    }
    for name in ["issues.jsonl", "config.yaml"] {
        assert_eq!(
            fs::read(beads.join(name)).unwrap(),
            source_files[Path::new(name)]
        );
    }
    let after = engine_rows(&db, None);
    assert!(after["jsonl_row_receipts"].1.is_empty());
    let closed_by = after["close_metadata"]
        .0
        .iter()
        .position(|name| name == "closed_by")
        .unwrap();
    assert!(
        after["close_metadata"]
            .1
            .iter()
            .all(|row| row[closed_by] == "NULL")
    );

    let after_files = schema19_tracker_files(&beads);
    let stats = schema19_json(&run_br(
        &workspace,
        ["stats", "--json"],
        "release19_stats_after",
    ));
    assert_eq!(
        stats["summary"]["total_issues"], 4,
        "DB-only issue must remain visible"
    );
    let current_plan = schema19_json(&run_br(
        &workspace,
        ["doctor", "migrate-schema", "plan", "--json"],
        "release19_current_plan",
    ));
    assert_eq!(current_plan["eligible"], false);
    assert!(current_plan["forecast"].is_null());
    let current_heal = schema19_json(&run_br(
        &workspace,
        ["doctor", "migrate-schema", "heal", "--dry-run", "--json"],
        "release19_current_heal",
    ));
    assert_eq!(current_heal["heal_needed"], false);
    assert_eq!(schema19_tracker_files(&beads), after_files);
    let stale = run_br(
        &workspace,
        [
            "doctor",
            "migrate-schema",
            "apply",
            "--plan-token",
            token,
            "--json",
        ],
        "release19_stale_apply",
    );
    assert!(!stale.status.success(), "{}", stale.stdout);
    assert_eq!(schema19_authority_rows(&db, Some(&before)), before);

    let run_id = applied["run_id"].as_str().expect("migration run id");
    // The same-version profile must survive the existing crash/resume
    // protocol, including its retained-source and new-column proofs.
    interrupted_apply_keeps_authority_until_explicit_resume(
        &workspace, &db, run_id, token, &before,
    );
    schema19_json(&run_br(
        &workspace,
        ["doctor", "migrate-schema", "undo", run_id, "--json"],
        "release19_undo",
    ));
    let restored = schema19_tracker_files(&beads);
    for name in [
        "beads.db",
        "beads.db-wal",
        "beads.db-shm",
        "beads.db-journal",
        "issues.jsonl",
        "config.yaml",
    ] {
        assert_eq!(
            restored.get(Path::new(name)),
            source_files.get(Path::new(name)),
            "undo changed {name}"
        );
    }
    assert_eq!(schema19_authority_rows(&db, None), before);
    let again = schema19_json(&run_br(
        &workspace,
        ["doctor", "migrate-schema", "plan", "--json"],
        "release19_plan_after_undo",
    ));
    assert_eq!(again["eligible"], true);
    assert_eq!(again["forecast"]["schema19_runtime_upgrade"], additions);
}

#[test]
fn released_schema19_explicit_heal_preserves_database_only_authority() {
    let workspace = released_schema19_workspace();
    seed_schema19_database_authority(&workspace);
    let db = workspace.root.join(".beads/beads.db");
    let before = schema19_authority_rows(&db, None);
    let jsonl = fs::read(workspace.root.join(".beads/issues.jsonl")).unwrap();
    let healed = schema19_json(&run_br(
        &workspace,
        ["doctor", "migrate-schema", "heal", "--json"],
        "release19_heal",
    ));
    assert_eq!(healed["action"], "migrated");
    assert_eq!(schema19_authority_rows(&db, Some(&before)), before);
    assert_eq!(
        fs::read(workspace.root.join(".beads/issues.jsonl")).unwrap(),
        jsonl
    );
    let stats = schema19_json(&run_br(
        &workspace,
        ["stats", "--json"],
        "release19_healed_stats",
    ));
    assert_eq!(stats["summary"]["total_issues"], 4);
}

#[test]
fn released_schema19_partial_upgrades_only_add_the_missing_structure() {
    for (name, statements, receipts_needed, actor_needed) in [
        (
            "actor_present",
            vec![
                "ALTER TABLE close_metadata ADD COLUMN closed_by TEXT",
                "UPDATE close_metadata SET closed_by = 'existing close actor'",
            ],
            true,
            false,
        ),
        (
            "receipts_present",
            vec![
                "CREATE TABLE jsonl_row_receipts (issue_id TEXT PRIMARY KEY, row_hash TEXT NOT NULL)",
                "INSERT INTO jsonl_row_receipts VALUES ('rel19-jzj', 'retained row receipt')",
            ],
            false,
            true,
        ),
    ] {
        let workspace = released_schema19_workspace();
        edit_schema19_fixture(&workspace, &statements);
        let db = workspace.root.join(".beads/beads.db");
        let before = schema19_authority_rows(&db, None);
        let plan = schema19_json(&run_br(
            &workspace,
            ["doctor", "migrate-schema", "plan", "--json"],
            &format!("release19_{name}_plan"),
        ));
        assert_eq!(
            plan["forecast"]["schema19_runtime_upgrade"],
            serde_json::json!({
                "jsonl_row_receipts_created": receipts_needed,
                "closed_by_column_added": actor_needed,
            })
        );
        assert_schema19_preservation_profile(&plan);
        schema19_json(&run_br(
            &workspace,
            [
                "doctor",
                "migrate-schema",
                "apply",
                "--plan-token",
                plan["plan_token"].as_str().unwrap(),
                "--json",
            ],
            &format!("release19_{name}_apply"),
        ));
        assert_eq!(schema19_authority_rows(&db, Some(&before)), before);
    }
}

#[test]
fn released_schema19_unknown_layouts_refuse_without_a_noop_or_source_mutation() {
    for (name, sql) in [
        (
            "unknown_close_column",
            "ALTER TABLE close_metadata ADD COLUMN operator_extra TEXT",
        ),
        (
            "malformed_receipts",
            "CREATE TABLE jsonl_row_receipts (issue_id TEXT PRIMARY KEY, row_hash BLOB NOT NULL)",
        ),
        (
            "unknown_core_column",
            "ALTER TABLE issues ADD COLUMN operator_extra TEXT",
        ),
    ] {
        let workspace = released_schema19_workspace();
        edit_schema19_fixture(&workspace, &[sql]);
        let beads = workspace.root.join(".beads");
        let before = schema19_tracker_files(&beads);
        for args in [
            vec!["doctor", "migrate-schema", "plan", "--json"],
            vec!["doctor", "migrate-schema", "heal", "--dry-run", "--json"],
        ] {
            let run = run_br(&workspace, args, &format!("release19_{name}_refusal"));
            let output: Value = serde_json::from_str(&run.stdout).expect("structured refusal");
            assert!(!run.status.success(), "unknown layout accepted: {output}");
            assert_eq!(output["error"]["code"], "CONFIG_ERROR");
            assert!(
                !output
                    .get("heal_needed")
                    .is_some_and(|needed| needed == false)
            );
            assert!(
                !output
                    .get("migration_eligible")
                    .is_some_and(|eligible| eligible == true)
            );
            assert!(output.get("plan_token").is_none());
            assert_eq!(schema19_tracker_files(&beads), before);
        }
    }
}

fn assert_schema19_preservation_profile(plan: &Value) {
    let preservation = &plan["forecast"]["schema19_preservation"];
    assert_eq!(preservation["profile"], "schema19-layout-preservation-v2");
    let digest = preservation["source_sha256"].as_str().unwrap();
    assert_eq!(digest.len(), 64);
    assert!(digest.bytes().all(|byte| byte.is_ascii_hexdigit()));
}

fn workflow_schema19_workspace() -> BrWorkspace {
    let workspace = BrWorkspace::new();
    install_fixture_workspace(
        &workspace,
        "schema19_v076_workflows.db.gz",
        "schema19_v076_workflows_issues.jsonl",
        "schema19_v076_workflows_config.yaml",
    );
    fs::copy(
        fixture_dir().join("schema19_v076_workflows_policy.yaml"),
        workspace.root.join(".beads/policy.yaml"),
    )
    .unwrap();
    assert_eq!(
        header_user_version(&workspace.root.join(".beads/beads.db")),
        19
    );
    workspace
}

/// The schema and workflow rows come from the release. These supplementary
/// operator values exercise opaque payloads, duplicate row identity, and
/// reservations that exceed the largest surviving AUTOINCREMENT row.
fn seed_workflow_schema19_operator_state(workspace: &BrWorkspace) {
    edit_schema19_fixture(
        workspace,
        &[
            "INSERT INTO config (key, value) VALUES ('compact_model', 'private-database-model')",
            "INSERT INTO config (key, value) VALUES ('compact_batch_size', '37')",
            "INSERT INTO metadata (key, value) VALUES ('operator_checkpoint', 'not in JSONL')",
            "CREATE TABLE release_operator_records (payload BLOB, amount REAL, note TEXT)",
            "CREATE INDEX release_operator_payload ON release_operator_records(payload)",
            "CREATE VIEW release_operator_view AS SELECT payload, note FROM release_operator_records",
            "INSERT INTO release_operator_records (rowid, payload, amount, note) VALUES (17, X'0001FF007F', 1.25, NULL)",
            "INSERT INTO release_operator_records (rowid, payload, amount, note) VALUES (91, X'0001FF007F', 1.25, NULL)",
            "UPDATE dependencies SET thread_id = 'private-thread', created_by = 'private-agent'",
            "UPDATE child_counters SET last_child = 777",
            "UPDATE sqlite_sequence SET seq = seq + 10000",
        ],
    );
}

fn workflow_schema19_authority(db: &Path) -> TableRows {
    let mut rows = schema19_authority_rows(db, None);
    if rows.contains_key("release_operator_records") {
        rows.insert(
            "release_operator_view".to_owned(),
            (vec!["payload".into(), "note".into()], Vec::new()),
        );
        rows = engine_rows(db, Some(&rows));
    }
    rows
}

/// Refused mutations may create coordination files, but must preserve every
/// durable database component and source artifact, including committed WAL
/// records that comparing only beads.db would miss.
fn workflow_schema19_durable_files(
    workspace: &BrWorkspace,
) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    let mut files = schema19_tracker_files(&workspace.root.join(".beads"));
    files.retain(|path, _| {
        [
            "beads.db",
            "beads.db-wal",
            "beads.db-journal",
            "issues.jsonl",
            "issues.history.jsonl",
            "metadata.json",
            "config.yaml",
            "policy.yaml",
        ]
        .iter()
        .any(|name| path == Path::new(name))
    });
    files
}

fn workflow_schema19_plan(workspace: &BrWorkspace) -> Value {
    schema19_json(&run_br(
        workspace,
        ["doctor", "migrate-schema", "plan", "--json"],
        "workflow19_plan",
    ))
}

fn workflow_schema19_apply(workspace: &BrWorkspace, token: &str) -> Value {
    schema19_json(&run_br(
        workspace,
        [
            "doctor",
            "migrate-schema",
            "apply",
            "--plan-token",
            token,
            "--json",
        ],
        "workflow19_apply",
    ))
}

fn assert_workflow_schema19_read(workspace: &BrWorkspace, command: &str, label: &str) {
    let result = run_br(
        workspace,
        [command, "--json", "--no-auto-import", "--no-auto-flush"],
        label,
    );
    if command == "info" {
        // Generic diagnostics intentionally inspect legacy tables directly.
        assert_eq!(schema19_json(&result)["issue_count"], 4);
    } else {
        assert!(format!("{}{}", result.stdout, result.stderr).contains("migrate-schema"));
    }
}

#[test]
fn schema19_workflows_reads_and_implicit_upgrades_preserve_source() {
    let workspace = workflow_schema19_workspace();
    let beads = workspace.root.join(".beads");
    let source = schema19_tracker_files(&beads);
    for command in ["stats", "info", "list"] {
        assert_workflow_schema19_read(&workspace, command, &format!("workflow19_read_{command}"));
        assert_eq!(schema19_tracker_files(&beads), source);
    }
    // A refused writer may initialize a frame-free WAL before the runtime
    // layout gate. Establish that engine-created sidecar before the writer
    // assertions, then compare every WAL byte without normalizing frames.
    let baseline = Connection::open(beads.join("beads.db").to_string_lossy().into_owned()).unwrap();
    baseline.query("SELECT 1").unwrap();
    baseline.close().unwrap();
    assert_eq!(
        fs::read(beads.join("beads.db")).unwrap(),
        source[Path::new("beads.db")]
    );
    let wal = fs::read(beads.join("beads.db-wal")).expect("engine-created writer baseline WAL");
    assert!(
        matches!(wal.len(), 0 | 32),
        "writer baseline must have no WAL frames"
    );
    let durable = workflow_schema19_durable_files(&workspace);
    for args in [
        vec!["sync", "--import-only", "--json"],
        vec!["create", "must not implicitly upgrade", "--json"],
    ] {
        let refused = run_br(&workspace, args, "workflow19_implicit_refusal");
        assert!(
            !refused.status.success(),
            "{} {}",
            refused.stdout,
            refused.stderr
        );
        assert!(format!("{}{}", refused.stdout, refused.stderr).contains("migrate-schema"));
        assert_eq!(workflow_schema19_durable_files(&workspace), durable);
    }
    assert!(!beads.join(".br_recovery/schema-migrations").exists());
}

#[test]
#[allow(clippy::too_many_lines)]
fn schema19_workflows_unreviewed_core_layouts_refuse_without_losing_columns() {
    // Both reviewed additions are already present. Each core-table extension
    // must independently refuse ordinary startup instead of reaching the
    // generic rebuild, which cannot preserve unknown columns or rowids.
    for table in ["issues", "config", "metadata"] {
        let workspace = workflow_schema19_workspace();
        seed_workflow_schema19_operator_state(&workspace);
        edit_schema19_fixture(
            &workspace,
            &[
                "ALTER TABLE close_metadata ADD COLUMN closed_by TEXT",
                "UPDATE close_metadata SET closed_by = 'retained close actor'",
                "CREATE TABLE jsonl_row_receipts (issue_id TEXT PRIMARY KEY, row_hash TEXT NOT NULL)",
                "INSERT INTO jsonl_row_receipts VALUES ('release-66o', 'retained receipt')",
                "INSERT INTO config (rowid, key, value) VALUES (101, 'operator_duplicate', 'older'), (207, 'operator_duplicate', 'newer')",
                "INSERT INTO metadata (rowid, key, value) VALUES (173, 'operator_duplicate', 'older'), (277, 'operator_duplicate', 'newer')",
                "UPDATE issues SET rowid = 4096 WHERE id = 'release-9sp'",
                &format!("ALTER TABLE {table} ADD COLUMN operator_payload BLOB"),
                &format!("UPDATE {table} SET operator_payload = X'0001FF007F'"),
            ],
        );
        let beads = workspace.root.join(".beads");
        let db = beads.join("beads.db");
        let before = workflow_schema19_authority(&db);
        let (columns, rows) = &before[table];
        let payload = columns
            .iter()
            .position(|column| column == "operator_payload")
            .expect("populated opaque core-table column");
        assert!(!rows.is_empty());
        assert!(
            rows.iter()
                .all(|row| row[payload] == "blob [0, 1, 255, 0, 127]")
        );
        assert!(
            before["issues"]
                .1
                .iter()
                .any(|row| row[0] == "integer 4096")
        );
        assert!(before["config"].1.iter().any(|row| row[0] == "integer 207"));
        assert!(
            before["metadata"]
                .1
                .iter()
                .any(|row| row[0] == "integer 277")
        );
        assert_eq!(header_user_version(&db), 19);

        let source = schema19_tracker_files(&beads);
        for args in [
            vec!["doctor", "migrate-schema", "plan", "--json"],
            vec!["doctor", "migrate-schema", "heal", "--dry-run", "--json"],
        ] {
            let refused = run_br(&workspace, args, &format!("workflow19_{table}_inspect"));
            assert!(
                !refused.status.success(),
                "{} {}",
                refused.stdout,
                refused.stderr
            );
            let output: Value = serde_json::from_str(&refused.stdout).expect("structured refusal");
            assert_eq!(output["error"]["code"], "CONFIG_ERROR");
            assert_ne!(output.get("heal_needed"), Some(&Value::Bool(false)));
            assert!(output.get("plan_token").is_none());
            assert_eq!(schema19_tracker_files(&beads), source);
            assert_eq!(workflow_schema19_authority(&db), before);
        }

        // Establish the engine's frame-free WAL before testing refused
        // writers; all subsequent durable bytes, including this WAL, must
        // remain exact. Coordination files are not authoritative records.
        let baseline = Connection::open(db.to_string_lossy().into_owned()).unwrap();
        baseline.query("SELECT 1").unwrap();
        baseline.close().unwrap();
        assert_eq!(fs::read(&db).unwrap(), source[Path::new("beads.db")]);
        let wal = fs::read(beads.join("beads.db-wal")).expect("writer baseline WAL");
        assert!(matches!(wal.len(), 0 | 32), "baseline WAL contains frames");
        let durable = workflow_schema19_durable_files(&workspace);
        for args in [
            vec!["sync", "--import-only", "--json"],
            vec!["create", "must preserve unknown core columns", "--json"],
        ] {
            let refused = run_br(&workspace, args, &format!("workflow19_{table}_writer"));
            assert!(
                !refused.status.success(),
                "{} {}",
                refused.stdout,
                refused.stderr
            );
            let output: Value = serde_json::from_str(&refused.stdout).expect("structured refusal");
            assert!(matches!(
                output["error"]["code"].as_str(),
                Some("CONFIG_ERROR" | "SCHEMA_MISMATCH")
            ));
            assert!(
                output["error"]["message"]
                    .as_str()
                    .unwrap()
                    .to_ascii_lowercase()
                    .contains("schema"),
                "unexpected refusal: {output}"
            );
            assert_eq!(workflow_schema19_durable_files(&workspace), durable);
            assert_eq!(workflow_schema19_authority(&db), before);
        }
        let error = beads_rust::storage::SqliteStorage::open(&db)
            .expect_err("ordinary storage open must refuse an unreviewed current layout");
        assert!(
            matches!(&error, beads_rust::BeadsError::Config(_)),
            "{error}"
        );
        assert!(
            error.to_string().to_ascii_lowercase().contains("schema"),
            "{error}"
        );
        assert_eq!(workflow_schema19_durable_files(&workspace), durable);
        assert_eq!(workflow_schema19_authority(&db), before);
        assert!(!beads.join(".br_recovery/schema-migrations").exists());
    }
}

#[test]
fn schema19_workflows_heal_without_jsonl_preserves_history_and_repeats() {
    let workspace = workflow_schema19_workspace();
    seed_workflow_schema19_operator_state(&workspace);
    let db = workspace.root.join(".beads/beads.db");
    let before = workflow_schema19_authority(&db);
    for (table, count) in [
        ("issues", 4),
        ("events", 23),
        ("dependencies", 3),
        ("close_metadata", 1),
        ("gate_results", 1),
        ("gate_result_history", 2),
        ("capacity_exemptions", 1),
        ("capacity_exemption_history", 2),
        ("capacity_occupancy", 4),
    ] {
        assert_eq!(before[table].1.len(), count, "authentic fixture {table}");
    }
    let source = fs::read(&db).unwrap();
    let jsonl = workspace.root.join(".beads/issues.jsonl");
    let retained_jsonl = workspace.root.join("retained-release-issues.jsonl");
    fs::rename(&jsonl, &retained_jsonl).unwrap();
    let retained_bytes = fs::read(&retained_jsonl).unwrap();
    assert_eq!(String::from_utf8_lossy(&retained_bytes).lines().count(), 3);
    let files = schema19_tracker_files(&workspace.root.join(".beads"));
    let preview = schema19_json(&run_br(
        &workspace,
        ["doctor", "migrate-schema", "heal", "--dry-run", "--json"],
        "workflow19_no_jsonl_preview",
    ));
    let plan = workflow_schema19_plan(&workspace);
    assert_eq!(preview["heal_needed"], true);
    assert_eq!(preview["migration_eligible"], true);
    assert_eq!(preview["automatic_heal_allowed"], false);
    assert_eq!(preview["from_version"], 19);
    assert_eq!(preview["to_version"], 19);
    assert_eq!(preview["plan_token"], plan["plan_token"]);
    assert_eq!(preview["migration_plan"], plan);
    assert_schema19_preservation_profile(&plan);
    assert_eq!(
        schema19_tracker_files(&workspace.root.join(".beads")),
        files
    );
    let healed = schema19_json(&run_br(
        &workspace,
        ["doctor", "migrate-schema", "heal", "--json"],
        "workflow19_no_jsonl_heal",
    ));
    assert_eq!(healed["action"], "migrated");
    assert_eq!(healed["from_version"], 19);
    assert_eq!(healed["to_version"], 19);
    assert_eq!(header_user_version(&db), 19);
    assert_eq!(engine_rows(&db, Some(&before)), before);
    let backup = Path::new(healed["backup"].as_str().unwrap());
    assert_eq!(fs::read(backup.join("beads.db")).unwrap(), source);
    assert!(
        !jsonl.exists(),
        "heal must not require or create a JSONL export"
    );
    assert_eq!(fs::read(&retained_jsonl).unwrap(), retained_bytes);
    let migrated = workflow_schema19_durable_files(&workspace);
    let repeated = schema19_json(&run_br(
        &workspace,
        ["doctor", "migrate-schema", "heal", "--json"],
        "workflow19_repeat_heal",
    ));
    assert_eq!(repeated["heal_needed"], false);
    assert_eq!(workflow_schema19_durable_files(&workspace), migrated);
    assert_eq!(fs::read(backup.join("beads.db")).unwrap(), source);
}

#[test]
fn schema19_workflows_plan_rejects_rowid_drift_and_pending_merge() {
    let workspace = workflow_schema19_workspace();
    seed_workflow_schema19_operator_state(&workspace);
    let plan = workflow_schema19_plan(&workspace);
    edit_schema19_fixture(
        &workspace,
        &["UPDATE release_operator_records SET rowid = 92 WHERE rowid = 91"],
    );
    let drifted = workflow_schema19_durable_files(&workspace);
    let refused = run_br(
        &workspace,
        [
            "doctor",
            "migrate-schema",
            "apply",
            "--plan-token",
            plan["plan_token"].as_str().unwrap(),
            "--json",
        ],
        "workflow19_rowid_drift",
    );
    assert!(
        !refused.status.success(),
        "{} {}",
        refused.stdout,
        refused.stderr
    );
    assert_eq!(workflow_schema19_durable_files(&workspace), drifted);
    let next = workflow_schema19_plan(&workspace);
    assert_ne!(next["plan_token"], plan["plan_token"]);
    assert_ne!(
        next["forecast"]["schema19_preservation"]["source_sha256"],
        plan["forecast"]["schema19_preservation"]["source_sha256"]
    );
    // Even an empty, malformed pending marker freezes the original catalog.
    edit_schema19_fixture(
        &workspace,
        &["INSERT INTO metadata (key, value) VALUES ('sync_merge_pending_v1', '')"],
    );
    let pending = workflow_schema19_durable_files(&workspace);
    for args in [
        vec!["doctor", "migrate-schema", "plan", "--json"],
        vec!["doctor", "migrate-schema", "heal", "--dry-run", "--json"],
        vec!["doctor", "migrate-schema", "heal", "--json"],
    ] {
        let refused = run_br(&workspace, args, "workflow19_pending_merge");
        assert!(
            !refused.status.success(),
            "{} {}",
            refused.stdout,
            refused.stderr
        );
        assert!(format!("{}{}", refused.stdout, refused.stderr).contains("pending"));
        assert_eq!(workflow_schema19_durable_files(&workspace), pending);
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn schema19_raw_text_changes_stale_plans_and_refuse_undo_without_losing_bytes() {
    for (table, column, predicate) in [
        ("config", "value", "key='operator_raw_text'"),
        ("release_operator_records", "note", "rowid=17"),
    ] {
        let workspace = workflow_schema19_workspace();
        seed_workflow_schema19_operator_state(&workspace);
        edit_schema19_fixture(
            &workspace,
            &[
                "INSERT INTO config (key, value) VALUES ('operator_raw_text', CAST(X'80' AS TEXT))",
                "UPDATE release_operator_records SET note=CAST(X'80' AS TEXT) WHERE rowid=17",
            ],
        );
        let db = workspace.root.join(".beads/beads.db");
        let source_rows = workflow_schema19_authority(&db);
        let plan = workflow_schema19_plan(&workspace);
        assert_schema19_preservation_profile(&plan);
        let update =
            |hex| format!("UPDATE {table} SET {column}=CAST(X'{hex}' AS TEXT) WHERE {predicate}");
        edit_schema19_fixture(&workspace, &[&update("81")]);
        let drifted_rows = workflow_schema19_authority(&db);
        assert_ne!(
            drifted_rows, source_rows,
            "the SQLite TEXT bytes really changed"
        );
        let next = workflow_schema19_plan(&workspace);
        assert_eq!(
            next["logical_witness"], plan["logical_witness"],
            "negative control: both distinct payloads have the same lossy v1 witness"
        );
        assert_ne!(next["plan_token"], plan["plan_token"]);
        assert_ne!(
            next["forecast"]["schema19_preservation"]["source_sha256"],
            plan["forecast"]["schema19_preservation"]["source_sha256"]
        );
        let drifted_files = workflow_schema19_durable_files(&workspace);
        let refused = run_br(
            &workspace,
            [
                "doctor",
                "migrate-schema",
                "apply",
                "--plan-token",
                plan["plan_token"].as_str().unwrap(),
                "--json",
            ],
            "raw_text_stale_plan",
        );
        assert!(
            !refused.status.success(),
            "{} {}",
            refused.stdout,
            refused.stderr
        );
        assert_eq!(workflow_schema19_durable_files(&workspace), drifted_files);
        assert_eq!(workflow_schema19_authority(&db), drifted_rows);

        let applied = workflow_schema19_apply(&workspace, next["plan_token"].as_str().unwrap());
        assert_eq!(engine_rows(&db, Some(&drifted_rows)), drifted_rows);
        let run_id = applied["run_id"].as_str().unwrap();
        edit_schema19_fixture(&workspace, &[&update("82")]);
        let newer_rows = workflow_schema19_authority(&db);
        let newer_files = workflow_schema19_durable_files(&workspace);
        for dry_run in [true, false] {
            let mut args = vec!["doctor", "migrate-schema", "undo", run_id, "--json"];
            if dry_run {
                args.push("--dry-run");
            }
            let refused = run_br(&workspace, args, "raw_text_newer_work_undo");
            assert!(
                !refused.status.success(),
                "{} {}",
                refused.stdout,
                refused.stderr
            );
            assert!(format!("{}{}", refused.stdout, refused.stderr).contains("changed"));
            assert_eq!(workflow_schema19_durable_files(&workspace), newer_files);
            assert_eq!(workflow_schema19_authority(&db), newer_rows);
        }
        let run_dir = workspace
            .root
            .join(".beads/.br_recovery/schema-migrations")
            .join(run_id);
        assert!(!run_dir.join("undone.json").exists());

        // Restore the exact applied payload, not its replacement-character
        // string. The unchanged control must still restore the source family.
        edit_schema19_fixture(&workspace, &[&update("81")]);
        schema19_json(&run_br(
            &workspace,
            ["doctor", "migrate-schema", "undo", run_id, "--json"],
            "raw_text_unchanged_undo",
        ));
        assert!(run_dir.join("undone.json").is_file());
        assert_eq!(workflow_schema19_durable_files(&workspace), drifted_files);
        assert_eq!(workflow_schema19_authority(&db), drifted_rows);
    }
}

/// Reconstruct the pre-install boundary from genuine migration receipts and
/// retained original files. This does not simulate a process or power failure.
#[test]
#[allow(clippy::too_many_lines)]
fn schema19_workflows_preinstall_recovery_requires_explicit_retry() {
    let workspace = workflow_schema19_workspace();
    seed_workflow_schema19_operator_state(&workspace);
    let db = workspace.root.join(".beads/beads.db");
    let before = workflow_schema19_authority(&db);
    let source = fs::read(&db).unwrap();
    let plan = workflow_schema19_plan(&workspace);
    let token = plan["plan_token"].as_str().unwrap();
    let applied = workflow_schema19_apply(&workspace, token);
    let run_id = applied["run_id"].as_str().unwrap();
    let run_dir = workspace
        .root
        .join(".beads/.br_recovery/schema-migrations")
        .join(run_id);
    let before_dir = run_dir.join("before");
    let original = fs::read(before_dir.join("beads.db")).unwrap();
    assert_eq!(original, source);
    let intent = fs::read(run_dir.join("commit-ready.json")).unwrap();
    fs::rename(
        run_dir.join("applied.json"),
        run_dir.join("applied.preinstall-test.json"),
    )
    .unwrap();
    let retained = run_dir.join("preinstall-test-retained-install");
    fs::create_dir(&retained).unwrap();
    for suffix in ["", "-wal", "-shm", "-journal"] {
        let live = PathBuf::from(format!("{}{suffix}", db.display()));
        if live.exists() {
            fs::rename(&live, retained.join(format!("beads.db{suffix}"))).unwrap();
        }
    }
    fs::rename(run_dir.join("maintenance-displaced/beads.db"), &db).unwrap();
    assert_eq!(fs::read(&db).unwrap(), source);
    let interrupted = schema19_tracker_files(&workspace.root.join(".beads"));
    for command in ["stats", "info", "list"] {
        assert_workflow_schema19_read(
            &workspace,
            command,
            &format!("workflow19_preinstall_{command}"),
        );
        assert_eq!(
            schema19_tracker_files(&workspace.root.join(".beads")),
            interrupted
        );
        assert!(!run_dir.join("failed.json").exists());
        assert!(!run_dir.join("applied.json").exists());
    }
    let retried = workflow_schema19_apply(&workspace, token);
    assert_ne!(retried["run_id"], run_id);
    let classified: Value =
        serde_json::from_slice(&fs::read(run_dir.join("failed.json")).unwrap()).unwrap();
    assert!(
        classified["error"]
            .as_str()
            .unwrap()
            .contains("before the replacement became live")
    );
    assert!(!run_dir.join("applied.json").exists());
    assert_eq!(header_user_version(&db), 19);
    assert_eq!(engine_rows(&db, Some(&before)), before);
    assert_eq!(fs::read(run_dir.join("commit-ready.json")).unwrap(), intent);
    assert_eq!(fs::read(before_dir.join("beads.db")).unwrap(), original);
}

#[test]
fn schema19_workflows_postinstall_resume_repeat_and_guarded_undo() {
    let workspace = workflow_schema19_workspace();
    seed_workflow_schema19_operator_state(&workspace);
    let db = workspace.root.join(".beads/beads.db");
    let before = workflow_schema19_authority(&db);
    let source = workflow_schema19_durable_files(&workspace);
    let plan = workflow_schema19_plan(&workspace);
    let token = plan["plan_token"].as_str().unwrap();
    let applied = workflow_schema19_apply(&workspace, token);
    let run_id = applied["run_id"].as_str().unwrap();
    interrupted_apply_keeps_authority_until_explicit_resume(
        &workspace, &db, run_id, token, &before,
    );
    assert_eq!(workflow_schema19_plan(&workspace)["eligible"], false);
    schema19_json(&run_br(
        &workspace,
        ["doctor", "migrate-schema", "undo", run_id, "--json"],
        "workflow19_undo",
    ));
    assert_eq!(workflow_schema19_durable_files(&workspace), source);
    assert_eq!(engine_rows(&db, Some(&before)), before);
    let again = workflow_schema19_plan(&workspace);
    let reapplied = workflow_schema19_apply(&workspace, again["plan_token"].as_str().unwrap());
    assert_eq!(engine_rows(&db, Some(&before)), before);
    schema19_json(&run_br(
        &workspace,
        [
            "update",
            "release-66o",
            "--title",
            "Work performed after the reviewed upgrade",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "workflow19_later_write",
    ));
    let changed = workflow_schema19_authority(&db);
    let changed_files = workflow_schema19_durable_files(&workspace);
    let refused = run_br(
        &workspace,
        [
            "doctor",
            "migrate-schema",
            "undo",
            reapplied["run_id"].as_str().unwrap(),
            "--json",
        ],
        "workflow19_refuse_destructive_undo",
    );
    assert!(
        !refused.status.success(),
        "{} {}",
        refused.stdout,
        refused.stderr
    );
    assert_eq!(workflow_schema19_durable_files(&workspace), changed_files);
    assert_eq!(workflow_schema19_authority(&db), changed);
}

#[test]
fn schema19_workflows_future_version_heal_refuses_without_mutation() {
    let workspace = workflow_schema19_workspace();
    edit_schema19_fixture(&workspace, &["PRAGMA user_version = 20"]);
    let source = workflow_schema19_durable_files(&workspace);
    for args in [
        vec!["doctor", "migrate-schema", "plan", "--json"],
        vec!["doctor", "migrate-schema", "heal", "--dry-run", "--json"],
        vec!["doctor", "migrate-schema", "heal", "--json"],
    ] {
        let refused = run_br(&workspace, args, "workflow19_future_refusal");
        assert!(
            !refused.status.success(),
            "{} {}",
            refused.stdout,
            refused.stderr
        );
        let output: Value = serde_json::from_str(&refused.stdout).unwrap();
        assert_ne!(output.get("heal_needed"), Some(&Value::Bool(false)));
        assert!(output.get("plan_token").is_none());
        assert_eq!(workflow_schema19_durable_files(&workspace), source);
    }
}

/// Qualification against the real old executable; no network fetch occurs in
/// the test. Both database layouts declare 19, so refusal must concern actual
/// closure-table incompatibility, not a fabricated future-version header.
#[test]
#[ignore = "requires BR_V076_BINARY pointing to the checksum-verified official Linux amd64 v0.7.6 release"]
#[allow(clippy::too_many_lines)]
fn official_v076_refuses_upgraded_schema19_layout_and_reads_guarded_undo() {
    use sha2::Digest;
    use std::fmt::Write as _;

    let binary = std::env::var_os("BR_V076_BINARY").expect("set BR_V076_BINARY");
    let digest = sha2::Sha256::digest(fs::read(&binary).unwrap());
    let mut checksum = String::with_capacity(64);
    for byte in digest {
        write!(&mut checksum, "{byte:02x}").unwrap();
    }
    assert_eq!(
        checksum,
        "73cbef7f11680b8349ef5b465d002e404272cc6c06b0f95a86035b488b1844b9"
    );
    let version = std::process::Command::new(&binary)
        .args(["version", "--json"])
        .output()
        .unwrap();
    assert!(version.status.success());
    let identity: Value = serde_json::from_slice(&version.stdout).unwrap();
    assert_eq!(identity["version"], "0.7.6");
    assert_eq!(
        identity["commit"],
        "ad4212f562ca59864946b8620c1361f0933ee4a8"
    );
    let workspace = workflow_schema19_workspace();
    let beads = workspace.root.join(".beads");
    let db = workspace.root.join(".beads/beads.db");
    let run_release_stats = || {
        // Old executables may create or change sidecars before reporting a
        // layout refusal. Run them only against disposable family copies;
        // keep the actual migrated family eligible for guarded undo.
        let probe = BrWorkspace::new();
        let probe_beads = probe.root.join(".beads");
        fs::create_dir(&probe_beads).unwrap();
        for name in [
            "beads.db",
            "beads.db-wal",
            "beads.db-shm",
            "beads.db-journal",
            "issues.jsonl",
            "config.yaml",
            "policy.yaml",
        ] {
            let original = beads.join(name);
            if original.is_file() {
                fs::copy(original, probe_beads.join(name)).unwrap();
            }
        }
        let mut command = std::process::Command::new(&binary);
        for (key, _) in std::env::vars_os() {
            let name = key.to_string_lossy();
            if name.starts_with("BEADS_") || name.starts_with("BD_") || name.starts_with("BR_") {
                command.env_remove(key);
            }
        }
        command
            .args(["stats", "--json", "--no-auto-import", "--no-auto-flush"])
            .current_dir(&probe.root)
            .env("HOME", &probe.root)
            .env("BEADS_DIR", &probe_beads)
            .env("BEADS_DB", probe_beads.join("beads.db"))
            .env("RUST_LOG", "error")
            .output()
            .unwrap()
    };
    let original_stats = run_release_stats();
    assert!(
        original_stats.status.success(),
        "{}",
        String::from_utf8_lossy(&original_stats.stderr)
    );
    let source = fs::read(&db).unwrap();
    let plan = workflow_schema19_plan(&workspace);
    let applied = workflow_schema19_apply(&workspace, plan["plan_token"].as_str().unwrap());
    assert_eq!(header_user_version(&db), 19);
    let migrated = schema19_tracker_files(&beads);
    let migrated_records = workflow_schema19_authority(&db);
    let old = run_release_stats();
    assert!(
        !old.status.success(),
        "{}",
        String::from_utf8_lossy(&old.stdout)
    );
    let refusal: Value = serde_json::from_slice(&old.stdout).unwrap();
    assert_eq!(refusal["error"]["code"], "CONFIG_ERROR");
    assert!(
        refusal["error"]["message"]
            .as_str()
            .unwrap()
            .contains("close_metadata")
    );
    assert_eq!(schema19_tracker_files(&beads), migrated);
    assert_eq!(workflow_schema19_authority(&db), migrated_records);
    schema19_json(&run_br(
        &workspace,
        [
            "doctor",
            "migrate-schema",
            "undo",
            applied["run_id"].as_str().unwrap(),
            "--json",
        ],
        "workflow19_downgrade_undo",
    ));
    assert_eq!(header_user_version(&db), 19);
    assert_eq!(fs::read(&db).unwrap(), source);
    let restored = run_release_stats();
    assert!(
        restored.status.success(),
        "{} {}",
        String::from_utf8_lossy(&restored.stdout),
        String::from_utf8_lossy(&restored.stderr)
    );
    let stats: Value = serde_json::from_slice(&restored.stdout).unwrap();
    assert_eq!(stats["summary"]["total_issues"], 4);
}

#[test]
fn e2e_migrate_schema_refuses_unsupported_core_shapes_before_issuing_a_token() {
    let cases: &[(&str, &[&str])] = &[
        (
            "dirty_issues",
            &["ALTER TABLE dirty_issues ADD COLUMN content_hash TEXT"],
        ),
        (
            "issues",
            &["CREATE INDEX operator_title_lookup ON issues(title)"],
        ),
        (
            "dependencies",
            &[
                "ALTER TABLE dependencies RENAME TO legacy_edges",
                "CREATE TABLE dependencies (
                    issue_id TEXT NOT NULL, depends_on_id TEXT NOT NULL,
                    type TEXT NOT NULL DEFAULT 'blocks',
                    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                    created_by TEXT NOT NULL DEFAULT '', metadata TEXT DEFAULT '{}',
                    thread_id TEXT DEFAULT '', PRIMARY KEY (issue_id, depends_on_id, type),
                    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
                )",
                "INSERT INTO dependencies SELECT * FROM legacy_edges",
                "INSERT INTO dependencies
                 SELECT issue_id, depends_on_id, 'parent-child', created_at,
                        created_by, metadata, thread_id FROM legacy_edges LIMIT 1",
                "DROP TABLE legacy_edges",
            ],
        ),
    ];
    for (table, statements) in cases {
        let workspace = BrWorkspace::new();
        install_fixture_workspace(
            &workspace,
            "schema15_pre384_era.db.gz",
            "schema15_issues.jsonl",
            "schema15_config.yaml",
        );
        let db_path = workspace.root.join(".beads/beads.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).expect("open fixture");
        for statement in *statements {
            conn.execute(statement).expect("prepare unsupported source");
        }
        conn.close().expect("close source fixture");
        let source = fs::read(&db_path).expect("capture source bytes");
        let plan = run_br(
            &workspace,
            ["doctor", "migrate-schema", "plan", "--json"],
            &format!("refuse_{table}"),
        );
        assert!(!plan.status.success(), "unexpected plan: {}", plan.stdout);
        let error: Value = serde_json::from_str(&plan.stdout).expect("structured refusal");
        assert_eq!(error["error"]["code"], "CONFIG_ERROR");
        assert!(error["error"]["message"].as_str().is_some_and(|message| {
            message.contains(&format!(
                "unsupported historical shape for core table {table}"
            )) && message.contains("no migration token was issued")
        }));
        assert!(error.get("plan_token").is_none());
        assert_eq!(fs::read(&db_path).expect("source after refusal"), source);
        assert_eq!(header_user_version(&db_path), 15);
        assert!(
            !workspace
                .root
                .join(".beads/.br_recovery/schema-migrations")
                .exists(),
            "planning must refuse before allocating migration recovery work"
        );
    }
}

/// mcp_agent_mail's br-2hpuk: after `ALTER TABLE ADD COLUMN` on a table whose
/// first column aliases the rowid, FrankenSQLite decoded the shorter
/// pre-existing records shifted by one column. br's migrations append
/// columns the same way, so pin the engine's reading of such rows.
#[test]
fn engine_reads_rows_written_before_add_column() {
    let scratch = tempfile::tempdir().expect("scratch dir");
    let path = scratch.path().join("add_column.db");
    let conn = Connection::open(path.to_string_lossy().into_owned()).expect("open");
    conn.execute("PRAGMA journal_mode=WAL").expect("WAL");
    conn.execute(
        "CREATE TABLE messages (id INTEGER PRIMARY KEY, project_id INTEGER NOT NULL, \
         sender TEXT NOT NULL, subject TEXT, body TEXT, importance TEXT DEFAULT 'normal', \
         ack INTEGER DEFAULT 0, created_ts INTEGER, topic TEXT, thread TEXT, \
         attachments TEXT DEFAULT '[]', extra BLOB)",
    )
    .expect("create");
    for id in 1..=40 {
        conn.execute(&format!(
            "INSERT INTO messages VALUES ({id}, {}, 'agent{id}', 'subject {id}', 'body {id}', \
             'high', {}, {}, NULL, 'thread-{id}', '[]', x'0102')",
            id % 3 + 7,
            id % 2,
            1_000_000 + id
        ))
        .expect("insert");
    }
    conn.execute("ALTER TABLE messages ADD COLUMN archive_metadata_json TEXT")
        .expect("add nullable column");
    conn.execute("ALTER TABLE messages ADD COLUMN prerequisites TEXT NOT NULL DEFAULT ''")
        .expect("add defaulted column");
    let check = |conn: &Connection, label: &str| {
        let rows = conn
            .query(
                "SELECT id, project_id, sender, subject, topic, thread, archive_metadata_json, \
                 prerequisites FROM messages ORDER BY id",
            )
            .expect("select");
        assert_eq!(rows.len(), 40, "{label}");
        for (index, row) in rows.iter().enumerate() {
            let id = i64::try_from(index + 1).unwrap();
            let int = |column: usize| row.get(column).and_then(SqliteValue::as_integer);
            let text = |column: usize| row.get(column).and_then(SqliteValue::as_text);
            assert_eq!(int(0), Some(id), "{label} row {id}");
            assert_eq!(int(1), Some(id % 3 + 7), "{label} row {id}");
            assert_eq!(
                text(2),
                Some(format!("agent{id}").as_str()),
                "{label} row {id}"
            );
            assert_eq!(
                text(3),
                Some(format!("subject {id}").as_str()),
                "{label} row {id}"
            );
            assert!(
                matches!(row.get(4), Some(SqliteValue::Null)),
                "{label} row {id}"
            );
            assert_eq!(
                text(5),
                Some(format!("thread-{id}").as_str()),
                "{label} row {id}"
            );
            assert!(
                matches!(row.get(6), Some(SqliteValue::Null)),
                "{label} row {id}"
            );
            assert_eq!(text(7), Some(""), "{label} row {id}");
        }
        let filtered = conn
            .query("SELECT count(*) FROM messages WHERE project_id = 7")
            .expect("filter");
        assert_eq!(
            filtered[0].get(0).and_then(SqliteValue::as_integer),
            Some(13),
            "{label}"
        );
    };
    check(&conn, "same connection");
    conn.close().expect("close");
    let reopened = Connection::open(path.to_string_lossy().into_owned()).expect("reopen");
    check(&reopened, "reopened");
    reopened.close().expect("close");
}
