//! Native witness transaction regressions. Every database is a new retained
//! fixture. The manifest generated here tests phase handling, not independent
//! engine agreement; the Python e2e suite supplies the independent manifests.

use super::*;

const LIMIT: u64 = 4 * 1024 * 1024;

fn fixture() -> (PathBuf, Manifest) {
    let directory = tempfile::Builder::new()
        .prefix("native-freelist-test-")
        .tempdir()
        .unwrap()
        .keep();
    let db = directory.join("beads.db");
    let mut conn = Connection::open(db.to_str().unwrap()).unwrap();
    conn.execute("PRAGMA wal_autocheckpoint=0").unwrap();
    conn.execute("CREATE TABLE items(id INTEGER PRIMARY KEY, note TEXT)")
        .unwrap();
    conn.execute("INSERT INTO items VALUES(1, 'preserved')")
        .unwrap();
    conn.execute("CREATE TABLE discarded(payload BLOB)")
        .unwrap();
    conn.execute("BEGIN IMMEDIATE").unwrap();
    for _ in 0..16 {
        conn.execute_with_params(
            "INSERT INTO discarded VALUES(?1)",
            &[SqliteValue::Blob(vec![0_u8; 8192].into())],
        )
        .unwrap();
    }
    conn.execute("COMMIT").unwrap();
    conn.execute("DROP TABLE discarded").unwrap();
    let checkpoint = conn.query("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
    assert_eq!(checkpoint.len(), 1);
    assert!(truncate_completed(checkpoint[0].values()));
    assert!(integer_pragma(&conn, "PRAGMA freelist_count").unwrap() > 0);
    verify_integrity(&conn).unwrap();
    let schema = conn
        .query("SELECT type,name,tbl_name,rootpage,sql FROM sqlite_schema")
        .unwrap();
    let rows = conn.query("SELECT rowid,id,note FROM items").unwrap();
    let manifest = Manifest {
        schema: SCHEMA.to_string(),
        schema_rows: RowWitness {
            rows: u64::try_from(schema.len()).unwrap(),
            sha256: rows_digest(&schema, LIMIT).unwrap(),
        },
        tables: vec![TableWitness {
            name: "items".to_string(),
            columns: vec!["id".to_string(), "note".to_string()],
            rowid_alias: Some("rowid".to_string()),
            rows: 1,
            sha256: rows_digest(&rows, LIMIT).unwrap(),
        }],
        skipped_virtual: Vec::new(),
        user_version: integer_pragma(&conn, "PRAGMA user_version").unwrap(),
        application_id: integer_pragma(&conn, "PRAGMA application_id").unwrap(),
    };
    conn.close_without_checkpoint_in_place().unwrap();
    (db, manifest)
}

#[test]
fn committed_freelist_is_not_misclassified_by_the_native_witness() {
    let (db, manifest) = fixture();
    let before = file_digest(&mut regular_file(&db).unwrap(), LIMIT).unwrap();
    let mut conn = open_with_flags(db.to_str().unwrap(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    assert!(integer_pragma(&conn, "PRAGMA freelist_count").unwrap() > 0);
    verify(&conn, &manifest, LIMIT).unwrap();
    assert!(!conn.as_async().in_transaction());
    verify(&conn, &manifest, LIMIT).unwrap();
    conn.close_without_checkpoint_in_place().unwrap();
    assert_eq!(
        file_digest(&mut regular_file(&db).unwrap(), LIMIT).unwrap(),
        before
    );
}

#[test]
fn failed_snapshot_ends_its_transaction_and_keeps_the_mismatch() {
    let (db, mut manifest) = fixture();
    let mut conn = open_with_flags(db.to_str().unwrap(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let correct_hash = manifest.schema_rows.sha256.clone();
    manifest.schema_rows.sha256 = "0".repeat(64);
    let error = verify(&conn, &manifest, LIMIT).unwrap_err();
    assert!(error.to_string().contains("native schema differs"));
    assert!(
        !conn.as_async().in_transaction(),
        "failed witness left a transaction open"
    );
    manifest.schema_rows.sha256 = correct_hash;
    verify(&conn, &manifest, LIMIT).unwrap();
    conn.close_without_checkpoint_in_place().unwrap();
}

#[test]
fn caller_transaction_is_not_rolled_back_or_reconfigured() {
    let (db, manifest) = fixture();
    let mut conn =
        open_with_flags(db.to_str().unwrap(), OpenFlags::SQLITE_OPEN_READ_WRITE).unwrap();
    assert_eq!(integer_pragma(&conn, "PRAGMA query_only").unwrap(), 0);
    conn.execute("BEGIN IMMEDIATE").unwrap();
    conn.execute("UPDATE items SET note='caller-owned' WHERE id=1")
        .unwrap();
    let error = verify(&conn, &manifest, LIMIT).unwrap_err();
    assert!(error.to_string().contains("autocommit connection"));
    assert!(conn.as_async().in_transaction());
    assert_eq!(integer_pragma(&conn, "PRAGMA query_only").unwrap(), 0);
    let rows = conn.query("SELECT note FROM items WHERE id=1").unwrap();
    assert!(matches!(rows[0].values(), [SqliteValue::Text(text)]
        if text.as_bytes() == b"caller-owned"));
    conn.execute("ROLLBACK").unwrap();
    verify(&conn, &manifest, LIMIT).unwrap();
    conn.close_without_checkpoint_in_place().unwrap();
}

#[test]
fn integrity_helper_refuses_transaction_local_freelist_projection() {
    let (db, _) = fixture();
    let mut conn = open_with_flags(db.to_str().unwrap(), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    conn.execute("BEGIN").unwrap();
    let error = verify_integrity(&conn).unwrap_err();
    assert!(error.to_string().contains("autocommit state"));
    assert!(conn.as_async().in_transaction());
    conn.execute("ROLLBACK").unwrap();
    verify_integrity(&conn).unwrap();
    conn.close_without_checkpoint_in_place().unwrap();
}
