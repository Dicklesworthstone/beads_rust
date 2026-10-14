//! Read admission and disposable write attestation by the linked FrankenSQLite.
//!
//! The parent recovery helper owns exclusion and only supplies private copies.
//! It independently checks main/WAL and index bytes after this process exits.
//! The supplied family remains read-only. Write/rollback/checkpoint operations
//! run only in a separately allocated, retained sandbox. No source index is
//! installed here, and neither path imports JSONL or opens ordinary br storage.

use std::collections::BTreeSet;
use std::fmt::Write as FmtWrite;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

use anyhow::{Context, Result, ensure};
use beads_rust::franken_sync::compat::{OpenFlags, open_with_flags};
use beads_rust::franken_sync::{Connection, Row, SqliteValue};
use serde::Deserialize;
use sha2::{Digest, Sha256};

pub const SCHEMA: &str = "br.orphaned_wal.native_probe.v1";
const RECEIPT_LIMIT: u64 = 4 * 1024 * 1024;
const WRITE_SCHEMA: &str = "br.orphaned_wal.native_write.v1";

/// The writable opener accepts only this sandbox, never a caller's DB path.
/// Construction allocates a fresh private directory with a fixed DB name.
struct WriteSandbox {
    directory: PathBuf,
    database: PathBuf,
}

impl WriteSandbox {
    fn allocate(parent: &Path) -> Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix("native-write-")
            .tempdir_in(parent)?
            .keep();
        #[cfg(unix)]
        {
            let parent_metadata = fs::symlink_metadata(parent)?;
            ensure!(parent_metadata.is_dir()
                    && parent_metadata.uid() == fs::metadata(&directory)?.uid()
                    && parent_metadata.mode() & 0o022 == 0,
                    "native write evidence parent must be owner-controlled and non-shared");
        }
        let database = directory.join("beads.db");
        Ok(Self { directory, database })
    }

    fn open_writer(&self) -> Result<Connection> {
        let path = self.database.to_str().context("non-UTF-8 sandbox path")?;
        // Never CREATE through the engine. A vanished copy is a refusal,
        // not permission to create an empty replacement database.
        Ok(open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?)
    }
}

fn component(db: &Path, suffix: &str) -> PathBuf {
    let mut value = db.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn regular_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let file = options.open(path)?;
    ensure!(file.metadata()?.is_file(), "probe input is not a regular file");
    #[cfg(unix)]
    ensure!(file.metadata()?.nlink() == 1, "probe input has extra hard links");
    Ok(file)
}

fn new_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    Ok(options.open(path)?)
}

fn file_digest(file: &mut File, max_bytes: u64) -> Result<(u64, String)> {
    file.seek(SeekFrom::Start(0))?;
    let mut hash = Sha256::new();
    let mut length = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        length = length.checked_add(u64::try_from(count)?).context("file length overflow")?;
        ensure!(length <= max_bytes, "native rehearsal exceeds --max-bytes");
        hash.update(&buffer[..count]);
    }
    Ok((length, lower_hex(&hash.finalize())))
}

fn copy_component(source: &Path, destination: &Path, max_bytes: u64) -> Result<(u64, String)> {
    let mut input = regular_file(source)?;
    let expected = file_digest(&mut input, max_bytes)?;
    input.seek(SeekFrom::Start(0))?;
    let mut output = new_file(destination)?;
    let copied = std::io::copy(&mut (&mut input).take(max_bytes.saturating_add(1)), &mut output)?;
    ensure!(copied == expected.0, "native rehearsal input changed during copy");
    output.sync_all()?;
    ensure!(file_digest(&mut input, max_bytes)? == expected, "native rehearsal input changed");
    let mut retained = regular_file(destination)?;
    ensure!(file_digest(&mut retained, max_bytes)? == expected, "native rehearsal copy differs");
    Ok(expected)
}

fn write_receipt(path: &Path, value: &serde_json::Value) -> Result<()> {
    let mut file = new_file(path)?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    File::open(path.parent().context("receipt has no parent")?)?.sync_all()?;
    Ok(())
}

fn truncate_completed(values: &[SqliteValue]) -> bool {
    matches!(values, [SqliteValue::Integer(0), SqliteValue::Integer(0), SqliteValue::Integer(0)])
}

fn probe_values() -> Vec<SqliteValue> {
    vec![SqliteValue::Integer(1), SqliteValue::Null, SqliteValue::Float(0.5),
        SqliteValue::Text("native WAL commit ' λ".into()),
        SqliteValue::Blob([0_u8, 255].repeat(2048).into())]
}

fn verify_probe_row(conn: &Connection, table: &str, max_bytes: u64) -> Result<()> {
    let rows = conn.query(&format!("SELECT id,missing,ratio,note,payload FROM {table} LIMIT 2"))?;
    ensure!(rows.len() == 1 && encoded_row(rows[0].values(), max_bytes)? ==
            encoded_row(&probe_values(), max_bytes)?, "native committed probe row was lost or changed");
    Ok(())
}

fn commit_probe(sandbox: &WriteSandbox, table: &str, max_bytes: u64) -> Result<()> {
    let mut conn = sandbox.open_writer()?;
    let operation = (|| -> Result<()> {
        conn.execute("PRAGMA wal_autocheckpoint=0")?;
        conn.execute("BEGIN IMMEDIATE")?;
        conn.execute(&format!("CREATE TABLE {table} (id INTEGER PRIMARY KEY, missing, ratio REAL, note TEXT, payload BLOB)"))?;
        conn.execute_with_params(&format!("INSERT INTO {table} VALUES (?1,?2,?3,?4,?5)"), &probe_values())?;
        conn.execute("COMMIT")?;
        verify_probe_row(&conn, table, max_bytes)
    })();
    let closed = conn.close_without_checkpoint_in_place();
    operation?;
    closed?;
    Ok(())
}

fn rollback_and_checkpoint(sandbox: &WriteSandbox, table: &str,
                           manifest: &Manifest, max_bytes: u64) -> Result<()> {
    let mut conn = sandbox.open_writer()?;
    let operation = (|| -> Result<()> {
        conn.execute("PRAGMA wal_autocheckpoint=0")?;
        // A fresh connection, not a retained pager cache, must see the commit.
        verify_probe_row(&conn, table, max_bytes)?;
        conn.execute("BEGIN IMMEDIATE")?;
        conn.execute(&format!("UPDATE {table} SET note='must roll back', payload=X'00' WHERE id=1"))?;
        conn.execute("ROLLBACK")?;
        verify_probe_row(&conn, table, max_bytes)?;
        conn.execute("BEGIN IMMEDIATE")?;
        conn.execute(&format!("DROP TABLE {table}"))?;
        conn.execute("COMMIT")?;
        // Compare to stock SQLite's BEFORE witness, not to the engine's own
        // earlier observation. All original rows and schema must survive.
        verify(&conn, manifest, max_bytes)?;
        conn.execute("PRAGMA query_only=OFF")?;
        let checkpoint = conn.query("PRAGMA wal_checkpoint(TRUNCATE)")?;
        ensure!(checkpoint.len() == 1 && truncate_completed(checkpoint[0].values()),
                "private native TRUNCATE did not return exactly (0,0,0)");
        Ok(())
    })();
    let closed = conn.close_without_checkpoint_in_place();
    operation?;
    closed?;
    Ok(())
}

fn verify_main_only(sandbox: &WriteSandbox, manifest: &Manifest, max_bytes: u64) -> Result<(u64, String)> {
    let directory = sandbox.directory.join("main-only");
    fs::create_dir(&directory)?;
    let db = directory.join("beads.db");
    let witness = copy_component(&sandbox.database, &db, max_bytes)?;
    let mut conn = open_with_flags(db.to_str().context("non-UTF-8 main-only path")?,
                                   OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let verified = verify(&conn, manifest, max_bytes);
    let closed = conn.close_without_checkpoint_in_place();
    verified?;
    closed?;
    ensure!(file_digest(&mut regular_file(&db)?, max_bytes)? == witness,
            "main-only native read changed checkpointed data");
    Ok(witness)
}

fn rehearse_writes(input: &Path, expected: &Path, manifest: &Manifest,
                   manifest_sha256: &str, max_bytes: u64) -> Result<()> {
    let evidence = expected.parent().context("native manifest has no parent")?;
    let sandbox = WriteSandbox::allocate(evidence)?;
    let mut stage = "capture";
    let operation = (|| -> Result<serde_json::Value> {
        let mut inputs = Vec::new();
        for suffix in ["", "-wal", "-shm"] {
            let source = component(input, suffix);
            let witness = copy_component(&source, &component(&sandbox.database, suffix), max_bytes)?;
            inputs.push((source, witness));
        }
        write_receipt(&sandbox.directory.join("prepared.json"), &serde_json::json!({
            "schema": WRITE_SCHEMA, "manifest_sha256": manifest_sha256,
            "input_family": input.to_str().context("non-UTF-8 input path")?,
            "sql_write_target": sandbox.database.to_str().context("non-UTF-8 sandbox path")?,
        }))?;
        // A random sandbox-bound identifier avoids user objects. CREATE has
        // no IF NOT EXISTS: even a coincident name must fail, not reuse data.
        let relative = sandbox.directory.file_name().and_then(|name| name.to_str())
            .context("invalid sandbox name")?;
        let name = format!("__br_wal_probe_{relative}");
        let table = quoted(&name)?;
        stage = "native-commit";
        commit_probe(&sandbox, &table, max_bytes)?;
        stage = "native-reopen-rollback-checkpoint";
        rollback_and_checkpoint(&sandbox, &table, manifest, max_bytes)?;
        stage = "main-only-verification";
        let (bytes, sha256) = verify_main_only(&sandbox, manifest, max_bytes)?;
        stage = "input-preservation";
        for (path, witness) in inputs {
            ensure!(file_digest(&mut regular_file(&path)?, max_bytes)? == witness,
                    "native write rehearsal changed its read-only input");
        }
        Ok(serde_json::json!({
            "schema": WRITE_SCHEMA, "verified": true,
            "scope": "fresh-private-copy", "source_checkpoint_performed": false,
            "manifest_sha256": manifest_sha256, "sandbox": relative,
            "probe_table": name, "input_family_unchanged": true,
            "committed_write_reopened": true, "rollback_verified": true,
            "checkpoint": [0, 0, 0], "main_only_verified": true,
            "main_only_bytes": bytes, "main_only_sha256": sha256,
        }))
    })();
    match operation {
        Ok(receipt) => {
            write_receipt(&sandbox.directory.join("complete.json"), &receipt)?;
            // Completion must be durable before native verified:true reaches
            // the parent. A competing old receipt is never overwritten.
            write_receipt(&evidence.join("native-write.json"), &receipt)
        }
        Err(error) => {
            let _ = write_receipt(&sandbox.directory.join("failed.json"), &serde_json::json!({
                "schema": WRITE_SCHEMA, "verified": false, "stage": stage,
                "manifest_sha256": manifest_sha256, "error": format!("{error:#}"),
            }));
            Err(error.context(format!("native write rehearsal failed at {stage}; retained {}",
                                       sandbox.directory.display())))
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RowWitness {
    rows: u64,
    sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TableWitness {
    name: String,
    columns: Vec<String>,
    rowid_alias: Option<String>,
    rows: u64,
    sha256: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    schema: String,
    schema_rows: RowWitness,
    tables: Vec<TableWitness>,
    skipped_virtual: Vec<String>,
    user_version: i64,
    application_id: i64,
}

fn quoted(name: &str) -> Result<String> {
    ensure!(!name.is_empty() && !name.contains('\0'), "invalid SQL identifier");
    Ok(format!("\"{}\"", name.replace('"', "\"\"")))
}

fn table_query(table: &TableWitness) -> Result<String> {
    ensure!(!table.columns.is_empty(), "empty column projection");
    let mut projection = Vec::new();
    if let Some(alias) = &table.rowid_alias {
        ensure!(matches!(alias.as_str(), "rowid" | "_rowid_" | "oid"), "invalid rowid alias");
        projection.push(alias.clone());
    }
    for column in &table.columns {
        projection.push(quoted(column)?);
    }
    let limit = table.rows.checked_add(1).context("row count overflow")?;
    Ok(format!("SELECT {} FROM {} LIMIT {limit}", projection.join(","), quoted(&table.name)?))
}

fn encoded_row(values: &[SqliteValue], max_bytes: u64) -> Result<Vec<u8>> {
    let mut encoded = Vec::new();
    encoded.extend_from_slice(&u64::try_from(values.len())?.to_be_bytes());
    for value in values {
        match value {
            SqliteValue::Null => encoded.push(b'n'),
            SqliteValue::Integer(number) => {
                encoded.push(b'i');
                encoded.extend_from_slice(&number.to_be_bytes());
            }
            SqliteValue::Float(number) => {
                encoded.push(b'r');
                encoded.extend_from_slice(&number.to_bits().to_be_bytes());
            }
            SqliteValue::Text(text) => {
                encoded.push(b't');
                append_bytes(&mut encoded, text.as_bytes(), max_bytes)?;
            }
            SqliteValue::Blob(blob) => {
                encoded.push(b'b');
                append_bytes(&mut encoded, blob, max_bytes)?;
            }
        }
        ensure!(u64::try_from(encoded.len())? <= max_bytes, "row exceeds witness byte limit");
    }
    Ok(encoded)
}

fn append_bytes(encoded: &mut Vec<u8>, bytes: &[u8], max_bytes: u64) -> Result<()> {
    let length = u64::try_from(bytes.len())?;
    let required = u64::try_from(encoded.len())?.checked_add(8)
        .and_then(|size| size.checked_add(length)).context("row length overflow")?;
    ensure!(required <= max_bytes, "value exceeds witness byte limit");
    encoded.extend_from_slice(&length.to_be_bytes());
    encoded.extend_from_slice(bytes);
    Ok(())
}

fn rows_digest(rows: &[Row], max_bytes: u64) -> Result<String> {
    let mut encoded = Vec::new();
    let mut total = 0_u64;
    for row in rows {
        let data = encoded_row(row.values(), max_bytes)?;
        total = total.checked_add(u64::try_from(data.len())?).and_then(|size| size.checked_add(8))
            .context("row witness length overflow")?;
        ensure!(total <= max_bytes, "row witness exceeds byte limit");
        encoded.push(data);
    }
    encoded.sort_unstable();
    let mut hash = Sha256::new();
    for row in encoded {
        hash.update(u64::try_from(row.len())?.to_be_bytes());
        hash.update(row);
    }
    Ok(lower_hex(&hash.finalize()))
}

/// sha2 0.11 digests are `hybrid_array::Array`s, which do not implement `LowerHex`.
fn lower_hex(bytes: &[u8]) -> String {
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut hex, "{byte:02x}").expect("writing to a String cannot fail");
    }
    hex
}

fn integer_pragma(conn: &Connection, sql: &str) -> Result<i64> {
    let row = conn.query_row(sql)?;
    let [SqliteValue::Integer(value)] = row.values() else {
        anyhow::bail!("unexpected integer PRAGMA result");
    };
    Ok(*value)
}

fn verify_integrity(conn: &Connection) -> Result<()> {
    ensure!(
        !conn.as_async().in_transaction(),
        "native integrity check requires autocommit state"
    );
    let integrity = conn.query("PRAGMA integrity_check")?;
    ensure!(integrity.len() == 1, "native integrity check did not return one row");
    ensure!(matches!(integrity[0].values(), [SqliteValue::Text(text)] if text.as_bytes() == b"ok"),
            "native integrity check failed");
    Ok(())
}

fn verify(conn: &Connection, manifest: &Manifest, max_bytes: u64) -> Result<()> {
    // Never roll back or change query_only inside a transaction owned by a
    // caller. All three probe callers arrive after committing/closing theirs.
    ensure!(
        !conn.as_async().in_transaction(),
        "native witness requires an autocommit connection"
    );
    conn.execute("PRAGMA query_only=ON")?;
    // Match schema_migration::logical_witness: FrankenSQLite's integrity
    // walker uses a transaction-local freelist inside BEGIN, which can report
    // healthy committed free pages as orphans. DROP of our disposable probe
    // table itself creates free pages. Check integrity in autocommit BEFORE
    // and AFTER the stable row/schema snapshot, never suppress an error.
    // The supplied family is excluded by the parent; writable/main-only
    // rehearsals are private owned copies, not concurrently writable sources.
    verify_integrity(conn)?;
    conn.execute("BEGIN")?;
    let verified = verify_snapshot(conn, manifest, max_bytes);
    // Evaluate rollback on failure too; a mismatch must not leak our read
    // transaction into the caller's close or next verification attempt.
    let rolled_back = conn.execute("ROLLBACK");
    verified?;
    rolled_back.context("could not end native witness snapshot")?;
    verify_integrity(conn)
}

fn verify_snapshot(conn: &Connection, manifest: &Manifest, max_bytes: u64) -> Result<()> {
    ensure!(manifest.schema_rows.rows <= max_bytes, "implausible expected schema row count");
    let schema_limit = manifest.schema_rows.rows.checked_add(1).context("schema row count overflow")?;
    let schema = conn.query(&format!(
        "SELECT type,name,tbl_name,rootpage,sql FROM sqlite_schema LIMIT {schema_limit}"
    ))?;
    ensure!(u64::try_from(schema.len())? == manifest.schema_rows.rows
            && rows_digest(&schema, max_bytes)? == manifest.schema_rows.sha256,
            "native schema differs from the independent SQLite witness");
    let mut actual_names = BTreeSet::new();
    for row in &schema {
        if let [SqliteValue::Text(kind), SqliteValue::Text(name), ..] = row.values()
            && kind.as_bytes() == b"table"
        {
            actual_names.insert(std::str::from_utf8(name.as_bytes())?.to_owned());
        }
    }
    let mut expected_names = BTreeSet::new();
    for name in manifest.tables.iter().map(|table| &table.name).chain(&manifest.skipped_virtual) {
        ensure!(expected_names.insert(name.clone()), "duplicate table in native manifest");
    }
    ensure!(actual_names == expected_names, "native manifest omitted or added a table");
    for table in &manifest.tables {
        ensure!(table.rows <= max_bytes, "implausible expected row count");
        let rows = conn.query(&table_query(table)?)?;
        ensure!(u64::try_from(rows.len())? == table.rows
                && rows_digest(&rows, max_bytes)? == table.sha256,
                "native table differs from the independent witness: {}", table.name);
    }
    ensure!(integer_pragma(conn, "PRAGMA user_version")? == manifest.user_version
            && integer_pragma(conn, "PRAGMA application_id")? == manifest.application_id,
            "native database metadata differs from the independent witness");
    Ok(())
}

pub fn execute(db: &Path, expected: &Path, max_bytes: u64) -> Result<serde_json::Value> {
    let path = db.to_str().context("native probe refuses a non-UTF-8 path instead of retargeting it")?;
    let mut bytes = Vec::new();
    File::open(expected)?.take(RECEIPT_LIMIT + 1).read_to_end(&mut bytes)?;
    ensure!(u64::try_from(bytes.len())? <= RECEIPT_LIMIT, "oversized native manifest");
    let manifest: Manifest = serde_json::from_slice(&bytes)?;
    ensure!(manifest.schema == SCHEMA, "unsupported native manifest schema");
    let mut conn = open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let verified = verify(&conn, &manifest, max_bytes);
    // Close on BOTH paths. No admitted native read can silently authorize a
    // close-time checkpoint, including after a verification failure.
    let closed = conn.close_without_checkpoint_in_place();
    verified?;
    closed?;
    // #562's original family was readable but unwritable. Reading the
    // proposed index alone therefore cannot be the companion's success gate.
    // Never write to db: the rehearsal allocates its OWN database first.
    let manifest_sha256 = lower_hex(&Sha256::digest(&bytes));
    rehearse_writes(db, expected, &manifest, &manifest_sha256, max_bytes)?;
    Ok(serde_json::json!({
        "schema": SCHEMA,
        "engine": "frankensqlite",
        "engine_version": option_env!("BR_FSQLITE_VERSION").unwrap_or("unknown"),
        "verified": true,
        "manifest_sha256": manifest_sha256,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_exact_truncate_completion_is_accepted() {
        let success = [SqliteValue::Integer(0), SqliteValue::Integer(0), SqliteValue::Integer(0)];
        assert!(truncate_completed(&success));
        for tuple in [[1, 0, 0], [0, 2, 2], [0, -1, -1], [1, -1, -1], [0, 2, 1]] {
            assert!(!truncate_completed(&tuple.map(SqliteValue::Integer)));
        }
        assert!(!truncate_completed(&[]));
        assert!(!truncate_completed(&success[..2]));
        assert!(!truncate_completed(&[SqliteValue::Float(0.0), success[1].clone(), success[2].clone()]));
    }

    #[test]
    fn sandbox_copy_is_exact_bounded_and_never_clobbers() {
        let root = tempfile::Builder::new().prefix("native-copy-test-").tempdir().unwrap().keep();
        let source = root.join("source");
        let target = root.join("target");
        fs::write(&source, b"database bytes").unwrap();
        let witness = copy_component(&source, &target, 1024).unwrap();
        assert_eq!(witness.0, 14);
        assert_eq!(fs::read(&source).unwrap(), fs::read(&target).unwrap());
        assert!(copy_component(&source, &target, 1024).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"database bytes");
        let bounded = root.join("bounded");
        assert!(copy_component(&source, &bounded, 3).is_err());
        assert!(!bounded.exists());
    }

    #[cfg(unix)]
    #[test]
    fn sandbox_capture_refuses_symlinks_and_extra_links() {
        let root = tempfile::Builder::new().prefix("native-links-test-").tempdir().unwrap().keep();
        let source = root.join("source");
        fs::write(&source, b"keep this input").unwrap();
        let symlink = root.join("symlink");
        std::os::unix::fs::symlink(&source, &symlink).unwrap();
        assert!(copy_component(&symlink, &root.join("copy"), 1024).is_err());
        fs::hard_link(&source, root.join("extra-link")).unwrap();
        assert!(regular_file(&source).is_err());
        assert_eq!(fs::read(&source).unwrap(), b"keep this input");
    }

    #[test]
    fn allocated_writer_cannot_fall_back_to_an_input_path() {
        let root = tempfile::Builder::new().prefix("native-owned-test-").tempdir().unwrap().keep();
        let sandbox = WriteSandbox::allocate(&root).unwrap();
        assert_eq!(sandbox.database.parent(), Some(sandbox.directory.as_path()));
        assert_eq!(sandbox.directory.parent(), Some(root.as_path()));
        assert!(!sandbox.database.exists());
        assert!(sandbox.open_writer().is_err());
        assert!(!sandbox.database.exists(), "read-write-only opener must not create a DB");
    }

    #[test]
    fn wire_encoding_preserves_types_boundaries_and_float_bits() {
        let row = [SqliteValue::Null, SqliteValue::Integer(-2), SqliteValue::Float(-0.0),
            SqliteValue::Text("λ".into()), SqliteValue::Blob(vec![0, 255].into())];
        let expected = "00000000000000056e69fffffffffffffffe728000000000000000740000000000000002cebb62000000000000000200ff";
        let actual = encoded_row(&row, 1024).unwrap();
        assert_eq!(lower_hex(&actual), expected);
        assert!(encoded_row(&row, 8).is_err());
        assert_ne!(encoded_row(&[SqliteValue::Integer(1)], 1024).unwrap(),
                   encoded_row(&[SqliteValue::Float(1.0)], 1024).unwrap());
    }

    #[test]
    fn table_identifiers_are_data_not_executable_sql() {
        let mut table = TableWitness { name: "a\"; DROP TABLE issues;--".into(),
            columns: vec!["b\"c".into()], rowid_alias: Some("rowid".into()), rows: 0,
            sha256: String::new() };
        assert_eq!(table_query(&table).unwrap(),
            "SELECT rowid,\"b\"\"c\" FROM \"a\"\"; DROP TABLE issues;--\" LIMIT 1");
        table.rowid_alias = Some("rowid FROM issues; DELETE".into());
        assert!(table_query(&table).is_err());
        table.rowid_alias = None;
        table.name = "bad\0name".into();
        assert!(table_query(&table).is_err());
    }

    #[test]
    fn malformed_duplicate_or_unknown_manifest_fields_are_refused() {
        for value in [r#"{"schema":"a","schema":"b"}"#,
                      r#"{"schema":"a","unexpected":true}"#] {
            assert!(serde_json::from_str::<Manifest>(value).is_err());
        }
    }
}

#[cfg(test)]
#[path = "wal_engine_probe_tests.rs"]
mod witness_tests;
