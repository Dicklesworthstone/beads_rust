//! A pending merge owns the exact schema catalog, including physical roots.
//! Neither a version upgrade nor equal-version maintenance may replace it.

use super::*;
use crate::sync::{METADATA_SYNC_MERGE_PENDING, METADATA_SYNC_MERGE_PENDING_LEGACY};
use tempfile::TempDir;

fn current_context() -> (TempDir, MigrationContext) {
    let temp = TempDir::new().unwrap();
    let beads_dir = temp.path().join(".beads");
    fs::create_dir(&beads_dir).unwrap();
    let db_path = beads_dir.join("beads.db");
    let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
    crate::storage::schema::apply_schema(&conn).unwrap();
    conn.execute("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    conn.execute("PRAGMA journal_mode = DELETE").unwrap();
    close_connection(conn).unwrap();
    let write_authority = Arc::new(
        crate::sync::blocking_database_family_write_lock_with_timeout(
            &beads_dir,
            &db_path,
            Some(1000),
        )
        .unwrap(),
    );
    (
        temp,
        MigrationContext {
            beads_dir,
            db_path,
            write_authority,
        },
    )
}

fn seed_pending(db: &Path, key: &str, value_sql: &str) {
    let conn = Connection::open(db.to_string_lossy().into_owned()).unwrap();
    if value_sql == "NULL" {
        // Model malformed historical metadata. The pending key still freezes
        // the catalog even when no valid current receipt can be decoded.
        conn.execute("DROP TABLE metadata").unwrap();
        conn.execute("CREATE TABLE metadata (key TEXT NOT NULL, value TEXT)")
            .unwrap();
        conn.execute("CREATE INDEX idx_metadata_key ON metadata(key)")
            .unwrap();
    }
    conn.execute_with_params(
        &format!("INSERT INTO metadata (key, value) VALUES (?, {value_sql})"),
        &[SqliteValue::from(key)],
    )
    .unwrap();
    close_connection(conn).unwrap();
}

fn assert_pending_refusal(error: &BeadsError) {
    assert!(matches!(error, BeadsError::SyncConflict { .. }), "{error}");
    assert!(error.to_string().contains("resume `br sync --merge`"));
}

#[test]
fn pending_merge_blocks_historical_plan_before_a_token_or_recovery_run() {
    for key in [
        METADATA_SYNC_MERGE_PENDING,
        METADATA_SYNC_MERGE_PENDING_LEGACY,
    ] {
        for value in ["''", "'not-json'", "X'00'"] {
            let (_temp, context) = super::tests::reviewed_source_migration_context("beads.db", 18);
            assert!(build_observational_plan(&context.db_path).unwrap().eligible);
            seed_pending(&context.db_path, key, value);
            let before = raw_family_witness(&context.db_path).unwrap();

            let error = build_observational_plan(&context.db_path).unwrap_err();
            assert_pending_refusal(&error);
            assert_eq!(raw_family_witness(&context.db_path).unwrap(), before);
            assert!(!migration_runs_root(&context.beads_dir).exists());
        }
    }
}

#[test]
fn current_pending_merge_retains_observational_noop_planning() {
    for key in [
        METADATA_SYNC_MERGE_PENDING,
        METADATA_SYNC_MERGE_PENDING_LEGACY,
    ] {
        let (_temp, context) = current_context();
        seed_pending(&context.db_path, key, "''");
        let before = raw_family_witness(&context.db_path).unwrap();
        let plan = build_observational_plan(&context.db_path).unwrap();
        assert_eq!(plan.from_version, 19);
        assert_eq!(plan.to_version, 19);
        assert!(!plan.eligible);
        assert!(plan.forecast.is_none());
        assert!(plan.plan_token.is_none());
        assert_eq!(raw_family_witness(&context.db_path).unwrap(), before);
    }
}

#[test]
fn pending_merge_blocks_private_vacuum_even_without_schema_ddl() {
    for key in [
        METADATA_SYNC_MERGE_PENDING,
        METADATA_SYNC_MERGE_PENDING_LEGACY,
    ] {
        for value in ["''", "'not-json'", "X'00'", "NULL"] {
            let (_temp, context) = current_context();
            seed_pending(&context.db_path, key, value);
            let before = raw_family_witness(&context.db_path).unwrap();
            let run_dir = context.beads_dir.join("pending-maintenance");
            let backup = run_dir.join("before");
            ensure_new_directory(&run_dir).unwrap();
            ensure_new_directory(&backup).unwrap();
            copy_family_to_backup(&context.db_path, &backup, &before).unwrap();
            let candidate = maintenance_candidate_path(&context.db_path, &run_dir).unwrap();

            // Old prepared maintenance receipts can reach this path without
            // passing through today's planner or schema-migration DDL guard.
            let error = vacuum_candidate_from_private_source(
                &context.db_path,
                &candidate,
                &run_dir,
                &before,
            )
            .unwrap_err();
            assert_pending_refusal(&error);
            assert!(!candidate.exists());
            assert!(!run_dir.join("commit-ready.json").exists());
            assert_eq!(raw_family_witness(&context.db_path).unwrap(), before);
            verify_backup_family(&context.db_path, &backup, &before).unwrap();
        }
    }
}

/// Append one in-range page belonging to neither a b-tree nor the freelist.
/// Unlike harmless trailing bytes, the header includes this page, producing
/// the real repairable "page N is never used" diagnostic.
fn add_unowned_page(db: &Path) {
    // A retained WAL page-one frame could mask the edited main-file header.
    // Finish all fixture writes before injecting physical page damage.
    let conn = Connection::open(db.to_string_lossy().into_owned()).unwrap();
    conn.execute("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    conn.execute("PRAGMA journal_mode = DELETE").unwrap();
    close_connection(conn).unwrap();
    let mut bytes = fs::read(db).unwrap();
    let encoded = u16::from_be_bytes(bytes[16..18].try_into().unwrap());
    let page_size = if encoded == 1 {
        65_536
    } else {
        usize::from(encoded)
    };
    assert_eq!(bytes.len() % page_size, 0);
    let pages = u32::try_from(bytes.len() / page_size).unwrap();
    assert_eq!(u32::from_be_bytes(bytes[28..32].try_into().unwrap()), pages);
    assert_eq!(&bytes[24..28], &bytes[92..96]);
    bytes.resize(bytes.len() + page_size, 0);
    bytes[28..32].copy_from_slice(&(pages + 1).to_be_bytes());
    fs::write(db, bytes).unwrap();
}

#[test]
fn pending_merge_blocks_current_schema_page_maintenance_plan() {
    for pending in [
        None,
        Some(METADATA_SYNC_MERGE_PENDING),
        Some(METADATA_SYNC_MERGE_PENDING_LEGACY),
    ] {
        let (_temp, context) = current_context();
        if let Some(key) = pending {
            seed_pending(&context.db_path, key, "'pending-catalog-witness'");
        }
        add_unowned_page(&context.db_path);
        let before = raw_family_witness(&context.db_path).unwrap();
        let observed =
            with_migration_read_snapshot(&context.db_path, |snapshot, _| logical_witness(snapshot))
                .unwrap();
        assert!(
            integrity_check_is_repairable(&observed.integrity_check),
            "fixture must exercise real page maintenance: {}",
            observed.integrity_check
        );

        let planned = build_observational_plan(&context.db_path);
        if pending.is_some() {
            assert_pending_refusal(&planned.unwrap_err());
        } else {
            let plan = planned.unwrap();
            assert!(plan.eligible, "the no-pending control must be repairable");
            let forecast = plan.forecast.unwrap();
            assert_eq!(forecast.from_version, forecast.to_version);
            assert!(forecast.schema19_runtime_upgrade.is_none());
            assert!(forecast.post_migration_maintenance);
        }
        assert_eq!(raw_family_witness(&context.db_path).unwrap(), before);
        assert!(!migration_runs_root(&context.beads_dir).exists());
    }
}
