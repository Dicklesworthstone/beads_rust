//! The bounded, row-preserving upgrade for the released br v0.1.27 schema4.
//!
//! This is deliberately independent of the historical catch-all migration
//! engine. That engine normalizes user values and deletes export bookkeeping;
//! this profile appends nullable/defaulted columns, creates new empty tables,
//! refreshes only a derived issue hash, and retains every existing record.

use super::{
    BeadsError, CLOSE_METADATA_INDEXES, Connection, GATE_RESULTS_INDEXES, ISSUES_RUNTIME_INDEXES,
    IssueType, Priority, Result, Row, SCHEMA_SQL, SqliteValue, Status,
    attest_reviewed_source_core_tables, canonical_index_creation_statements,
    connection_user_version, content_hash_from_parts, partial_index_predicate_matches,
    runtime_index_key_shape_canonical, runtime_index_row_canonical,
    semantic_partial_index_predicate_canonical, split_sql_statements, sql_contains_token_sequence,
};

pub const PROFILE: &str = "released-v0.1.27-schema4-preserve-records-v1";
pub const NEW_TABLES: &[&str] = &[
    "capacity_exemption_history",
    "capacity_exemptions",
    "capacity_occupancy",
    "close_metadata",
    "gate_result_history",
    "gate_results",
    "jsonl_row_receipts",
];
pub const NEW_SEQUENCES: &[&str] = &["capacity_exemption_history", "gate_result_history"];
pub const ISSUE_ADDITIONS: &[&str] = &["source_repo_path", "agent_context", "prerequisites"];
pub const EVENT_ADDITIONS: &[&str] = &["agent_name", "harness", "model"];

fn refused(detail: impl std::fmt::Display) -> BeadsError {
    BeadsError::Config(format!(
        "reviewed schema4 migration refused: {detail}; preserve the source database \
         and resolve this unsupported shape before planning again"
    ))
}

/// All admission is read-only and is repeated on the private candidate before
/// its first write. A version stamp alone cannot select this conversion.
pub fn attest_source(conn: &Connection) -> Result<()> {
    if connection_user_version(conn)? != 4 {
        return Err(refused("the released profile requires user_version=4"));
    }
    attest_reviewed_source_core_tables(conn, false)?;
    for row in conn.query("SELECT name FROM main.sqlite_schema")? {
        let name = text(row.get(0), "schema object name")?;
        if NEW_TABLES
            .iter()
            .chain(std::iter::once(&"dependencies_v19_stage"))
            .any(|reserved| name.eq_ignore_ascii_case(reserved))
        {
            return Err(refused(format!(
                "reserved destination name {name} already exists"
            )));
        }
    }
    for row in conn.query("SELECT name FROM main.sqlite_sequence")? {
        let name = text(row.get(0), "sequence name")?;
        if NEW_SEQUENCES
            .iter()
            .any(|reserved| name.eq_ignore_ascii_case(reserved))
        {
            return Err(refused(format!(
                "new history table {name} has an existing sequence reservation"
            )));
        }
    }
    // The dependency table is the only existing relation copied by this
    // profile. Its canonical FK must be satisfiable without dropping a row.
    if !conn
        .query(
            "SELECT 1 FROM main.dependencies AS d LEFT JOIN main.issues AS i \
             ON i.id = d.issue_id WHERE i.id IS NULL LIMIT 1",
        )?
        .is_empty()
    {
        return Err(refused("dependencies contains an orphan source issue"));
    }
    // Decode before ALTER/UPDATE, without silently coercing an opaque SQL
    // value into a hash input. The destination proof uses these same pure
    // derived values while separately preserving every other raw SQL cell.
    content_hashes(conn)?;
    Ok(())
}

pub(super) fn issue_indexes_canonical(conn: &Connection) -> bool {
    conn.query("PRAGMA main.index_list('issues')")
        .is_ok_and(|rows| {
            rows.iter().all(|row| {
                let Some(name) = row.get(1).and_then(SqliteValue::as_text) else {
                    return false;
                };
                if row.get(3).and_then(SqliteValue::as_text) == Some("pk") {
                    return true;
                }
                ISSUES_RUNTIME_INDEXES
                    .iter()
                    .find(|index| index.name == name)
                    .is_some_and(|index| runtime_index_row_canonical(conn, row, index))
                    || retired_index_row_canonical(conn, row, name)
            })
        })
}

/// These exact released accelerators carry no independent record or operator
/// constraint. Same-name UNIQUE, partial, expression, collation, or key changes
/// remain part of the source schema and cannot be exempted from its proof.
pub fn is_retired_index(conn: &Connection, table: &str, name: &str) -> bool {
    table == "issues"
        && conn
            .query("PRAGMA main.index_list('issues')")
            .is_ok_and(|rows| {
                rows.iter().any(|row| {
                    row.get(1).and_then(SqliteValue::as_text) == Some(name)
                        && retired_index_row_canonical(conn, row, name)
                })
            })
}

fn retired_index_row_canonical(conn: &Connection, row: &Row, name: &str) -> bool {
    if row.get(2).and_then(SqliteValue::as_integer) != Some(0)
        || row.get(3).and_then(SqliteValue::as_text) != Some("c")
        || row.get(4).and_then(SqliteValue::as_integer) != Some(1)
    {
        return false;
    }
    match name {
        "idx_issues_external_ref" => {
            runtime_index_key_shape_canonical(conn, name, &["external_ref"])
                && partial_index_predicate_matches(conn, name, "external_ref IS NOT NULL")
        }
        "idx_issues_list_active_order" => {
            descending_active_key_canonical(conn)
                && semantic_partial_index_predicate_canonical(conn, name)
        }
        _ => false,
    }
}

fn descending_active_key_canonical(conn: &Connection) -> bool {
    let Ok(rows) = conn.query("PRAGMA main.index_xinfo('idx_issues_list_active_order')") else {
        return false;
    };
    if rows
        .iter()
        .any(|row| row.get(5).and_then(SqliteValue::as_integer).is_none())
    {
        return false;
    }
    let keys = rows
        .iter()
        .filter(|row| row.get(5).and_then(SqliteValue::as_integer) == Some(1))
        .collect::<Vec<_>>();
    keys.len() == 2
        && keys
            .iter()
            .zip([("priority", 0), ("created_at", 1)])
            .enumerate()
            .all(|(position, (row, (column, descending)))| {
                row.get(0).and_then(SqliteValue::as_integer) == i64::try_from(position).ok()
                    && row
                        .get(1)
                        .and_then(SqliteValue::as_integer)
                        .is_some_and(|cid| cid >= 0)
                    && row.get(2).and_then(SqliteValue::as_text) == Some(column)
                    && row.get(3).and_then(SqliteValue::as_integer) == Some(descending)
                    && row
                        .get(4)
                        .and_then(SqliteValue::as_text)
                        .is_some_and(|collation| collation.eq_ignore_ascii_case("BINARY"))
            })
}

fn text<'a>(value: Option<&'a SqliteValue>, field: &str) -> Result<&'a str> {
    value
        .and_then(SqliteValue::as_text)
        .ok_or_else(|| refused(format!("{field} is not stored as TEXT")))
}

fn optional_text<'a>(value: Option<&'a SqliteValue>, field: &str) -> Result<Option<&'a str>> {
    match value {
        Some(SqliteValue::Null) => Ok(None),
        Some(SqliteValue::Text(value)) => Ok(Some(value.as_ref())),
        _ => Err(refused(format!("{field} is neither TEXT nor NULL"))),
    }
}

fn flag(value: Option<&SqliteValue>, field: &str) -> Result<bool> {
    match value.and_then(SqliteValue::as_integer) {
        Some(0) => Ok(false),
        Some(1) => Ok(true),
        _ => Err(refused(format!("{field} is not an integer boolean"))),
    }
}

/// Recompute the current issue identity without normalizing or discarding any
/// source field. Returning values before mutation also lets the plan bind the
/// exact destination hashes instead of excluding hashes from preservation.
pub fn content_hashes(conn: &Connection) -> Result<Vec<(String, String)>> {
    let mut hashes = Vec::new();
    for row in conn.query(
        "SELECT id, title, description, design, acceptance_criteria, notes, \
         status, priority, issue_type, assignee, owner, created_by, external_ref, \
         source_system, pinned, is_template FROM main.issues ORDER BY id",
    )? {
        let id = text(row.get(0), "issues.id")?;
        let status_text = text(row.get(6), "issues.status")?;
        let status = status_text
            .parse::<Status>()
            .unwrap_or_else(|_| Status::Custom(status_text.to_owned()));
        let type_text = text(row.get(8), "issues.issue_type")?;
        let issue_type = type_text
            .parse::<IssueType>()
            .unwrap_or_else(|_| IssueType::Custom(type_text.to_owned()));
        let priority = row
            .get(7)
            .and_then(SqliteValue::as_integer)
            .filter(|value| (0..=4).contains(value))
            .and_then(|value| i32::try_from(value).ok())
            .ok_or_else(|| refused(format!("issue {id} has an unsupported priority value")))?;
        let hash = content_hash_from_parts(
            text(row.get(1), "issues.title")?,
            Some(text(row.get(2), "issues.description")?),
            Some(text(row.get(3), "issues.design")?),
            Some(text(row.get(4), "issues.acceptance_criteria")?),
            None,
            Some(text(row.get(5), "issues.notes")?),
            &status,
            &Priority(priority),
            &issue_type,
            optional_text(row.get(9), "issues.assignee")?,
            optional_text(row.get(10), "issues.owner")?,
            optional_text(row.get(11), "issues.created_by")?,
            optional_text(row.get(12), "issues.external_ref")?,
            optional_text(row.get(13), "issues.source_system")?,
            flag(row.get(14), "issues.pinned")?,
            flag(row.get(15), "issues.is_template")?,
        );
        hashes.push((id.to_owned(), hash));
    }
    Ok(hashes)
}

fn create_canonical_table(conn: &Connection, table: &str) -> Result<()> {
    let declaration = split_sql_statements(SCHEMA_SQL)
        .into_iter()
        .find(|statement| {
            sql_contains_token_sequence(statement, &format!("CREATE TABLE IF NOT EXISTS {table}"))
        })
        .ok_or_else(|| refused(format!("canonical declaration for {table} is missing")))?;
    conn.execute(declaration)?;
    Ok(())
}

/// The caller owns the transaction. All source validation and hash decoding
/// precede the first write; only the verified private candidate reaches here
/// through the receipt lifecycle. Existing dirty/export records stay exact:
/// hashes are not JSONL fields, and an old export hash mismatch only causes
/// conservative export work after a future dirty update.
pub(super) fn apply_in_transaction(conn: &Connection) -> Result<usize> {
    attest_source(conn)?;
    let hashes = content_hashes(conn)?;
    conn.execute("ALTER TABLE main.issues ADD COLUMN source_repo_path TEXT")?;
    conn.execute("ALTER TABLE main.issues ADD COLUMN agent_context TEXT")?;
    for column in EVENT_ADDITIONS {
        conn.execute(&format!("ALTER TABLE main.events ADD COLUMN {column} TEXT"))?;
    }
    for table in ["close_metadata", "gate_results"] {
        create_canonical_table(conn, table)?;
    }
    for name in ["idx_issues_external_ref", "idx_issues_list_active_order"] {
        if is_retired_index(conn, "issues", name) {
            conn.execute(&format!("DROP INDEX main.{name}"))?;
        }
    }
    let declarations = canonical_index_creation_statements();
    for index in ISSUES_RUNTIME_INDEXES
        .iter()
        .chain(CLOSE_METADATA_INDEXES)
        .chain(GATE_RESULTS_INDEXES)
    {
        let declaration = declarations
            .iter()
            .find(|statement| {
                sql_contains_token_sequence(
                    statement,
                    &format!("INDEX IF NOT EXISTS {}", index.name),
                )
            })
            .ok_or_else(|| refused(format!("canonical index {} is missing", index.name)))?;
        conn.execute(declaration)?;
    }
    for (id, hash) in &hashes {
        conn.execute_with_params(
            "UPDATE main.issues SET content_hash = ? WHERE id = ?",
            &[
                SqliteValue::from(hash.as_str()),
                SqliteValue::from(id.as_str()),
            ],
        )?;
    }
    Ok(hashes.len())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::io::Read;

    use super::*;
    use crate::storage::schema::{
        CURRENT_SCHEMA_VERSION, execute_batch, run_reviewed_schema_migration_steps_in_transaction,
        runtime_schema_compatible,
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
        let path = temp.path().join("released-schema4.db");
        std::fs::write(&path, bytes).unwrap();
        let conn = Connection::open(path.to_string_lossy().into_owned()).unwrap();
        (temp, conn)
    }

    fn raw_rows(conn: &Connection, query: &str) -> Vec<Vec<SqliteValue>> {
        conn.query(query)
            .unwrap()
            .into_iter()
            .map(|row| row.values().to_vec())
            .collect()
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn released_schema4_upgrade_keeps_records_rowids_and_sequence_reservations() {
        let (_temp, conn) = released_source();
        execute_batch(&conn, r"
            INSERT INTO config (rowid, key, value) VALUES (41, 'editor', 'vim'), (72, 'editor', 'nano');
            INSERT INTO metadata (rowid, key, value) VALUES (81, 'operator', X'00FF');
            INSERT INTO dirty_issues (rowid, issue_id, marked_at) VALUES (37, 'leg-n57', 'original-mark');
            CREATE TABLE operator_notes (id INTEGER PRIMARY KEY AUTOINCREMENT, payload BLOB, tag TEXT);
            CREATE UNIQUE INDEX operator_tag ON operator_notes(tag COLLATE NOCASE) WHERE tag IS NOT NULL;
            INSERT INTO operator_notes (id, payload, tag) VALUES (205, X'007FFF', 'retain');
            UPDATE sqlite_sequence SET seq=700 WHERE name='events';
            UPDATE sqlite_sequence SET seq=900 WHERE name='operator_notes';
        ").unwrap();
        let mut expected = BTreeMap::new();
        for table in [
            "issues",
            "events",
            "comments",
            "dependencies",
            "labels",
            "config",
            "metadata",
            "dirty_issues",
            "export_hashes",
            "blocked_issues_cache",
            "child_counters",
            "operator_notes",
        ] {
            let columns = conn
                .query(&format!("PRAGMA table_xinfo('{table}')"))
                .unwrap()
                .iter()
                .map(|row| {
                    row.get(1)
                        .and_then(SqliteValue::as_text)
                        .unwrap()
                        .to_owned()
                })
                .filter(|column| table != "issues" || column != "content_hash")
                .collect::<Vec<_>>()
                .join(", ");
            let query = format!("SELECT rowid, {columns} FROM {table} ORDER BY rowid");
            expected.insert(query.clone(), raw_rows(&conn, &query));
        }
        let sequences = raw_rows(&conn, "SELECT name, seq FROM sqlite_sequence ORDER BY name");
        let expected_hashes = content_hashes(&conn).unwrap();
        assert_eq!(expected_hashes.len(), 2);
        attest_source(&conn).unwrap();
        conn.execute("BEGIN IMMEDIATE").unwrap();
        let effects = run_reviewed_schema_migration_steps_in_transaction(
            &conn,
            4,
            u32::try_from(CURRENT_SCHEMA_VERSION).unwrap(),
            "unused-for-schema4",
        )
        .unwrap();
        conn.execute("COMMIT").unwrap();
        assert_eq!(effects.content_hash_rows_rebuilt, 2);
        assert!(runtime_schema_compatible(&conn));
        for (query, rows) in expected {
            assert_eq!(
                raw_rows(&conn, &query),
                rows,
                "changed source rows: {query}"
            );
        }
        assert_eq!(
            raw_rows(
                &conn,
                "SELECT name, seq FROM sqlite_sequence WHERE name IN ('comments','events','operator_notes') ORDER BY name"
            ),
            sequences,
        );
        for (id, hash) in expected_hashes {
            let row = conn.query_row_with_params("SELECT content_hash, source_repo_path, agent_context, prerequisites FROM issues WHERE id=?", &[id.into()]).unwrap();
            assert_eq!(
                row.get(0).and_then(SqliteValue::as_text),
                Some(hash.as_str())
            );
            assert_eq!(row.get(1), Some(&SqliteValue::Null));
            assert_eq!(row.get(2), Some(&SqliteValue::Null));
            assert_eq!(row.get(3).and_then(SqliteValue::as_text), Some(""));
        }
        assert_eq!(
            raw_rows(&conn, "SELECT agent_name, harness, model FROM events"),
            vec![vec![SqliteValue::Null; 3]; 5]
        );
        conn.execute(
            "INSERT INTO events (issue_id, event_type) VALUES ('leg-n57', 'sequence-proof')",
        )
        .unwrap();
        assert_eq!(
            conn.query_row("SELECT MAX(id) FROM events")
                .unwrap()
                .get(0)
                .and_then(SqliteValue::as_integer),
            Some(701)
        );
    }

    #[test]
    fn schema4_unsupported_shapes_refuse_before_any_migration_statement() {
        for sql in [
            "ALTER TABLE issues ADD COLUMN operator_payload BLOB",
            "ALTER TABLE events ADD COLUMN operator_payload TEXT",
            "CREATE TABLE close_metadata (operator_payload TEXT)",
            "DROP INDEX idx_issues_external_ref; CREATE UNIQUE INDEX idx_issues_external_ref ON issues(external_ref) WHERE external_ref IS NOT NULL",
            "DROP INDEX idx_issues_list_active_order; CREATE INDEX idx_issues_list_active_order ON issues(priority DESC, created_at DESC) WHERE status NOT IN ('closed','tombstone') AND (is_template=0 OR is_template IS NULL)",
            "DROP INDEX idx_issues_list_active_order; CREATE INDEX idx_issues_list_active_order ON issues(priority, created_at COLLATE NOCASE DESC) WHERE status NOT IN ('closed','tombstone') AND (is_template=0 OR is_template IS NULL)",
            "UPDATE issues SET priority=1.5 WHERE id='leg-n57'",
            "UPDATE issues SET notes=X'FF00' WHERE id='leg-n57'",
        ] {
            let (_temp, conn) = released_source();
            execute_batch(&conn, sql).unwrap();
            let schema_before = raw_rows(
                &conn,
                "SELECT type, name, tbl_name, sql FROM sqlite_schema ORDER BY type, name",
            );
            let rows_before = raw_rows(&conn, "SELECT rowid, * FROM issues ORDER BY rowid");
            conn.execute("BEGIN IMMEDIATE").unwrap();
            let result = run_reviewed_schema_migration_steps_in_transaction(
                &conn,
                4,
                u32::try_from(CURRENT_SCHEMA_VERSION).unwrap(),
                "not-written",
            );
            assert!(result.is_err(), "accepted {sql}");
            assert_eq!(
                raw_rows(
                    &conn,
                    "SELECT type, name, tbl_name, sql FROM sqlite_schema ORDER BY type, name"
                ),
                schema_before,
                "wrote schema before refusing {sql}"
            );
            assert_eq!(
                raw_rows(&conn, "SELECT rowid, * FROM issues ORDER BY rowid"),
                rows_before,
                "changed issue values before refusing {sql}"
            );
            assert_eq!(connection_user_version(&conn).unwrap(), 4);
            conn.execute("ROLLBACK").unwrap();
        }
    }

    #[test]
    fn released_schema4_shape_does_not_authorize_other_version_stamps() {
        for version in [0, 7] {
            let (_temp, conn) = released_source();
            conn.execute(&format!("PRAGMA user_version={version}"))
                .unwrap();
            assert!(attest_source(&conn).is_err());
            assert!(
                run_reviewed_schema_migration_steps_in_transaction(
                    &conn,
                    version,
                    u32::try_from(CURRENT_SCHEMA_VERSION).unwrap(),
                    "unused",
                )
                .is_err()
            );
        }
    }
}
