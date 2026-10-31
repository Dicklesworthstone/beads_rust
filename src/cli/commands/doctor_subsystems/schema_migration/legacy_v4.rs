//! Complete, token-bound destination proof for the released schema4 upgrade.
//!
//! All source cells and hidden rowids participate. Only the named issue hash
//! derivation and additive default values are projected to their destination;
//! dirty markers, export receipts, events, configuration, opaque operator
//! tables, and sequence high-water marks remain exact.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use fsqlite_types::SqliteValue;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    BeadsError, Connection, LegacyTableProjection, Result, hash_len_prefixed, hex_digest,
    legacy_projection_rowid, legacy_rows_projection, preservation_text, query_user_version,
    quote_identifier, reviewed_schema_objects, with_preservation_snapshot,
};
use crate::storage::schema::{
    CURRENT_SCHEMA_VERSION, legacy_v4 as schema, runtime_schema_compatible,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct LegacyV4Projection {
    profile: String,
    issues_columns_added: Vec<String>,
    events_columns_added: Vec<String>,
    created_tables: Vec<String>,
    bookkeeping_preserved: bool,
    tables: Vec<LegacyTableProjection>,
    preserved_schema_sha256: String,
}

fn refused(detail: impl std::fmt::Display) -> BeadsError {
    BeadsError::Config(format!(
        "reviewed schema4 preservation refused: {detail}; the original database remains authoritative"
    ))
}

pub(super) fn projection(conn: &Connection, project_source: bool) -> Result<LegacyV4Projection> {
    let hashes = if project_source {
        schema::attest_source(conn)?;
        schema::content_hashes(conn)?.into_iter().collect()
    } else {
        if i64::from(query_user_version(conn)?) != i64::from(CURRENT_SCHEMA_VERSION)
            || !runtime_schema_compatible(conn)
        {
            return Err(refused(
                "the candidate does not have the current canonical schema",
            ));
        }
        BTreeMap::new()
    };
    let mut tables = Vec::new();
    for row in conn.query("SELECT name FROM main.sqlite_schema WHERE type='table' ORDER BY name")? {
        let name = preservation_text(row.get(0), "table name")?;
        tables.push(table_projection(conn, &name, project_source, &hashes)?);
    }
    if project_source {
        for name in schema::NEW_TABLES {
            tables.push(legacy_rows_projection(name, true, &[])?);
        }
    }
    tables.sort_by(|left, right| left.table.name.cmp(&right.table.name));
    Ok(LegacyV4Projection {
        profile: schema::PROFILE.to_owned(),
        issues_columns_added: schema::ISSUE_ADDITIONS
            .iter()
            .map(|name| (*name).to_owned())
            .collect(),
        events_columns_added: schema::EVENT_ADDITIONS
            .iter()
            .map(|name| (*name).to_owned())
            .collect(),
        created_tables: schema::NEW_TABLES
            .iter()
            .map(|name| (*name).to_owned())
            .collect(),
        bookkeeping_preserved: true,
        tables,
        preserved_schema_sha256: preserved_schema(conn)?,
    })
}

fn table_projection(
    conn: &Connection,
    table: &str,
    project_source: bool,
    hashes: &BTreeMap<String, String>,
) -> Result<LegacyTableProjection> {
    let rowid = legacy_projection_rowid(conn, table)?;
    let prefix = rowid.map_or_else(String::new, |alias| format!("{alias}, "));
    let mut rows = Vec::new();
    let mut empty_sequences = BTreeSet::new();
    for row in conn.query(&format!(
        "SELECT {prefix}* FROM main.{}",
        quote_identifier(table)
    ))? {
        let mut values = row.values().to_vec();
        if !project_source
            && table == "sqlite_sequence"
            && values
                .first()
                .and_then(SqliteValue::as_text)
                .is_some_and(|name| schema::NEW_SEQUENCES.contains(&name))
        {
            let name = preservation_text(values.first(), "new sequence name")?;
            if values.len() != 2
                || values.get(1).and_then(SqliteValue::as_integer) != Some(0)
                || !empty_sequences.insert(name)
            {
                return Err(refused(
                    "new history table has an unexpected sequence reservation",
                ));
            }
            // SQLite engines may reserve an empty AUTOINCREMENT table at 0
            // on CREATE or wait until its first INSERT. Neither consumes an
            // id, and every pre-existing reservation remains in the proof.
            continue;
        }
        if project_source {
            match table {
                "issues" => {
                    let offset = usize::from(rowid.is_some());
                    let id = preservation_text(values.get(offset), "issue id")?;
                    let hash = hashes.get(&id).ok_or_else(|| {
                        refused(format!("issue {id} has no derived destination hash"))
                    })?;
                    let cell = values
                        .get_mut(offset + 1)
                        .ok_or_else(|| refused("source issue omitted content_hash"))?;
                    *cell = SqliteValue::from(hash.as_str());
                    values.extend([SqliteValue::Null, SqliteValue::Null, SqliteValue::from("")]);
                }
                "events" => {
                    values.extend([SqliteValue::Null, SqliteValue::Null, SqliteValue::Null]);
                }
                _ => {}
            }
        }
        rows.push(values);
    }
    legacy_rows_projection(table, rowid.is_some(), &rows)
}

fn preserved_schema(conn: &Connection) -> Result<String> {
    let mut hasher = Sha256::new();
    for object in reviewed_schema_objects(conn, false)? {
        if schema::NEW_TABLES.contains(&object.table.as_str())
            || (object.table == "events" && object.object_type == "table")
            || (object.object_type == "index"
                && schema::is_retired_index(conn, &object.table, &object.name))
        {
            // Exact source declarations are attested before this projection;
            // exact destination declarations are independently attested above.
            // Only the named additive changes and released accelerators vary.
            continue;
        }
        for value in [
            object.object_type,
            object.name,
            object.table,
            object.fingerprint,
        ] {
            hash_len_prefixed(&mut hasher, value.as_bytes());
        }
    }
    Ok(hex_digest(hasher.finalize().as_slice()))
}

pub(super) fn verify(
    db_path: &Path,
    expected: &LegacyV4Projection,
    project_source: bool,
) -> Result<()> {
    with_preservation_snapshot(db_path, |conn| {
        let observed = projection(conn, project_source)?;
        if &observed != expected {
            return Err(refused(format!(
                "complete projected state differs at {}; refusing installation, resumption, or undo",
                db_path.display()
            )));
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use super::*;
    use crate::storage::schema::{
        execute_batch, run_reviewed_schema_migration_steps_in_transaction,
    };

    fn released_source() -> (tempfile::TempDir, Connection) {
        let temp = tempfile::tempdir().unwrap();
        let mut bytes = Vec::new();
        flate2::read::GzDecoder::new(
            &include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/schema_migration/schema4_v0127_clean.db.gz"
            ))[..],
        )
        .read_to_end(&mut bytes)
        .unwrap();
        let path = temp.path().join("schema4-projection.db");
        std::fs::write(&path, bytes).unwrap();
        let conn = Connection::open(path.to_string_lossy().into_owned()).unwrap();
        execute_batch(&conn, r"
            INSERT INTO config (rowid, key, value) VALUES (81, 'editor', 'vim'), (97, 'editor', 'nano');
            INSERT INTO metadata (rowid, key, value) VALUES (105, 'opaque', X'00FF');
            INSERT INTO dirty_issues (rowid, issue_id, marked_at) VALUES (33, 'leg-n57', 'retain-time');
            CREATE TABLE operator_notes (id INTEGER PRIMARY KEY AUTOINCREMENT, tag TEXT, payload BLOB);
            CREATE UNIQUE INDEX operator_tag ON operator_notes(tag COLLATE NOCASE) WHERE tag IS NOT NULL;
            INSERT INTO operator_notes (id, tag, payload) VALUES (171, 'retain', X'01FF');
            CREATE TABLE operator_without_rowid (key TEXT PRIMARY KEY, payload BLOB) WITHOUT ROWID;
            INSERT INTO operator_without_rowid VALUES ('keep', X'FF00');
            UPDATE sqlite_sequence SET seq=901 WHERE name='events';
            UPDATE sqlite_sequence SET seq=1001 WHERE name='operator_notes';
        ").unwrap();
        (temp, conn)
    }

    fn upgrade(conn: &Connection) {
        conn.execute("BEGIN IMMEDIATE").unwrap();
        run_reviewed_schema_migration_steps_in_transaction(
            conn,
            4,
            u32::try_from(CURRENT_SCHEMA_VERSION).unwrap(),
            "unused-for-schema4",
        )
        .unwrap();
        conn.execute("COMMIT").unwrap();
    }

    #[test]
    fn released_schema4_complete_destination_matches_before_and_after_upgrade() {
        let (_temp, conn) = released_source();
        let expected = projection(&conn, true).unwrap();
        upgrade(&conn);
        assert_eq!(projection(&conn, false).unwrap(), expected);
        assert!(
            expected
                .tables
                .iter()
                .any(|table| table.table.name == "events" && table.table.row_count == 5)
        );
        assert!(
            expected
                .tables
                .iter()
                .any(|table| table.table.name == "operator_notes" && table.rowid_included)
        );
        assert!(
            expected
                .tables
                .iter()
                .any(|table| table.table.name == "operator_without_rowid" && !table.rowid_included)
        );
    }

    #[test]
    fn schema4_projection_rejects_changed_records_defaults_hashes_and_operator_schema() {
        for sql in [
            "DELETE FROM events WHERE id=1",
            "UPDATE events SET old_value=X'00FF' WHERE id=1",
            "UPDATE config SET rowid=301 WHERE rowid=81",
            "DELETE FROM config WHERE rowid=81",
            "UPDATE metadata SET value='decoded' WHERE key='opaque'",
            "UPDATE dirty_issues SET marked_at='replaced-time'",
            "DELETE FROM export_hashes",
            "UPDATE issues SET content_hash='incorrect-derived-value' WHERE id='leg-n57'",
            "UPDATE issues SET source_repo_path='invented' WHERE id='leg-n57'",
            "UPDATE issues SET prerequisites='invented' WHERE id='leg-n57'",
            "UPDATE events SET agent_name='invented' WHERE id=1",
            "UPDATE operator_notes SET payload='decoded'",
            "UPDATE operator_without_rowid SET payload='decoded'",
            "UPDATE sqlite_sequence SET seq=5 WHERE name='events'",
            "UPDATE sqlite_sequence SET seq=171 WHERE name='operator_notes'",
            "DROP INDEX operator_tag; CREATE INDEX operator_tag ON operator_notes(tag COLLATE NOCASE) WHERE tag IS NOT NULL",
        ] {
            let (_temp, conn) = released_source();
            let expected = projection(&conn, true).unwrap();
            upgrade(&conn);
            execute_batch(&conn, sql).unwrap();
            let observed = projection(&conn, false);
            assert!(
                observed.is_err() || observed.unwrap() != expected,
                "accepted drift: {sql}"
            );
        }
    }

    #[test]
    fn schema4_plan_token_binds_duplicate_precedence_and_hidden_rowids() {
        use super::super::{
            MigrationForecast, compute_plan_token, logical_witness_from_connection,
        };

        let (_temp, conn) = released_source();
        let before = logical_witness_from_connection(&conn, "ok".to_owned()).unwrap();
        let mut forecast = MigrationForecast {
            from_version: 4,
            to_version: u32::try_from(CURRENT_SCHEMA_VERSION).unwrap(),
            content_hash_rows_rebuilt: 2,
            gate_result_history_created: true,
            prerequisites_column_added: true,
            dependency_type_key_added: true,
            legacy_v15_conversion: None,
            legacy_v4_conversion: Some(projection(&conn, true).unwrap()),
            post_migration_maintenance: true,
        };
        let token_before = compute_plan_token("/released-schema4.db", &before, &forecast).unwrap();
        conn.execute("UPDATE config SET rowid=301 WHERE rowid=81")
            .unwrap();
        let after = logical_witness_from_connection(&conn, "ok".to_owned()).unwrap();
        assert_eq!(
            before, after,
            "the old multiset witness does not include hidden rowids"
        );
        forecast.legacy_v4_conversion = Some(projection(&conn, true).unwrap());
        let token_after = compute_plan_token("/released-schema4.db", &after, &forecast).unwrap();
        assert_ne!(token_before, token_after);
    }
}
