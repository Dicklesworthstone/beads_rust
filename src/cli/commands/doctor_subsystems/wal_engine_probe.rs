//! Read-only attestation by the exact FrankenSQLite linked into the companion.
//!
//! The parent recovery helper owns exclusion and only supplies private copies.
//! It independently checks main/WAL and index bytes after this process exits.
//! This module cannot checkpoint, repair, import JSONL, or install an index.

use std::collections::BTreeSet;
use std::fs::File;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use beads_rust::franken_sync::compat::{OpenFlags, open_with_flags};
use beads_rust::franken_sync::{Connection, Row, SqliteValue};
use serde::Deserialize;
use sha2::{Digest, Sha256};

pub(super) const SCHEMA: &str = "br.orphaned_wal.native_probe.v1";
const RECEIPT_LIMIT: u64 = 4 * 1024 * 1024;

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
    Ok(format!("{:x}", hash.finalize()))
}

fn integer_pragma(conn: &Connection, sql: &str) -> Result<i64> {
    let row = conn.query_row(sql)?;
    let [SqliteValue::Integer(value)] = row.values() else {
        anyhow::bail!("unexpected integer PRAGMA result");
    };
    Ok(*value)
}

fn verify(conn: &Connection, manifest: &Manifest, max_bytes: u64) -> Result<()> {
    conn.execute("PRAGMA query_only=ON")?;
    conn.execute("BEGIN")?;
    let integrity = conn.query("PRAGMA integrity_check")?;
    ensure!(integrity.len() == 1, "native integrity check did not return one row");
    ensure!(matches!(integrity[0].values(), [SqliteValue::Text(text)] if text.as_bytes() == b"ok"),
            "native integrity check failed");
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
    conn.execute("ROLLBACK")?;
    Ok(())
}

pub(super) fn execute(db: &Path, expected: &Path, max_bytes: u64) -> Result<serde_json::Value> {
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
    Ok(serde_json::json!({
        "schema": SCHEMA,
        "engine": "frankensqlite",
        "engine_version": option_env!("BR_FSQLITE_VERSION").unwrap_or("unknown"),
        "verified": true,
        "manifest_sha256": format!("{:x}", Sha256::digest(&bytes)),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_encoding_preserves_types_boundaries_and_float_bits() {
        let row = [SqliteValue::Null, SqliteValue::Integer(-2), SqliteValue::Float(-0.0),
            SqliteValue::Text("λ".into()), SqliteValue::Blob(vec![0, 255].into())];
        let expected = "00000000000000056e69fffffffffffffffe728000000000000000740000000000000002cebb62000000000000000200ff";
        let actual = encoded_row(&row, 1024).unwrap();
        assert_eq!(actual.iter().map(|byte| format!("{byte:02x}")).collect::<String>(), expected);
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
