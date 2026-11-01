//! Strict, record-preserving content-hash refresh for reviewed upgrades.
//!
//! Hashes are derived issue identities and are absent from JSONL. Refreshing
//! them does not require replacing durable dirty markers or export receipts:
//! an old export hash can only request conservative work after a dirty edit.

use super::{
    BeadsError, Connection, IssueType, Priority, Result, SqliteValue, Status, column_exists,
    content_hash_from_parts,
};

/// New v13 plans bind this behavior; absent profiles identify old receipts,
/// whose already-installed generations may still be recovered or undone.
pub const PROFILE: &str = "v13-preserve-bookkeeping-v1";

fn refused(detail: impl std::fmt::Display) -> BeadsError {
    BeadsError::Config(format!(
        "reviewed content hash refresh refused: {detail}; preserve the source database \
         and resolve this unsupported value before planning again"
    ))
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

/// Derive every destination hash before any write. Unsupported SQL values
/// must not be silently converted to defaults while computing an identity.
pub fn content_hashes(conn: &Connection) -> Result<Vec<(String, String)>> {
    let prerequisites = if column_exists(conn, "issues", "prerequisites") {
        "prerequisites"
    } else {
        "NULL"
    };
    let mut hashes = Vec::new();
    for row in conn.query(&format!(
        "SELECT id, title, description, design, acceptance_criteria, notes, \
         status, priority, issue_type, assignee, owner, created_by, external_ref, \
         source_system, pinned, is_template, {prerequisites} FROM main.issues ORDER BY id"
    ))? {
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
            optional_text(row.get(16), "issues.prerequisites")?,
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

pub(super) fn write_hashes(conn: &Connection, hashes: &[(String, String)]) -> Result<usize> {
    for (id, hash) in hashes {
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

/// The caller owns the transaction and source-shape admission. Decode all
/// hashes first, then update them in place without touching any other table.
pub(super) fn apply_in_transaction(conn: &Connection) -> Result<usize> {
    write_hashes(conn, &content_hashes(conn)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::schema::{apply_schema, execute_batch};

    fn source() -> Connection {
        let conn = Connection::open(":memory:").unwrap();
        apply_schema(&conn).unwrap();
        execute_batch(
            &conn,
            r"
            PRAGMA foreign_keys=OFF;
            INSERT INTO issues (id, content_hash, title) VALUES
                ('bd-first', 'original-first-hash', 'First'),
                ('bd-last', 'original-last-hash', 'Last');
            INSERT INTO dirty_issues (rowid, issue_id, marked_at) VALUES
                (71, 'bd-first', 'original-mark'), (103, 'bd-orphan', X'00FF');
            INSERT INTO export_hashes (rowid, issue_id, content_hash, exported_at) VALUES
                (83, 'bd-first', 'exported-first-hash', 'original-export'),
                (109, 'bd-orphan', X'7FFF', 'orphan-export');
        ",
        )
        .unwrap();
        conn
    }

    fn rows(conn: &Connection, sql: &str) -> Vec<Vec<SqliteValue>> {
        conn.query(sql)
            .unwrap()
            .into_iter()
            .map(|row| row.values().to_vec())
            .collect()
    }

    #[test]
    fn refresh_preserves_existing_and_orphan_bookkeeping_verbatim() {
        let conn = source();
        let dirty = rows(&conn, "SELECT rowid, * FROM dirty_issues ORDER BY rowid");
        let exports = rows(&conn, "SELECT rowid, * FROM export_hashes ORDER BY rowid");
        conn.execute("BEGIN IMMEDIATE").unwrap();
        assert_eq!(apply_in_transaction(&conn).unwrap(), 2);
        conn.execute("COMMIT").unwrap();
        assert_eq!(
            rows(&conn, "SELECT rowid, * FROM dirty_issues ORDER BY rowid"),
            dirty
        );
        assert_eq!(
            rows(&conn, "SELECT rowid, * FROM export_hashes ORDER BY rowid"),
            exports
        );
        assert_eq!(
            rows(
                &conn,
                "SELECT COUNT(*) FROM dirty_issues WHERE issue_id='bd-last'"
            ),
            vec![vec![SqliteValue::Integer(0)]],
            "refreshing a hash does not create a dirty marker for an unchanged issue"
        );
    }

    #[test]
    fn every_hash_input_is_decoded_before_the_first_update() {
        for malformed in [
            "UPDATE issues SET assignee=X'00FF' WHERE id='bd-last'",
            "UPDATE issues SET pinned=2 WHERE id='bd-last'",
            "UPDATE issues SET prerequisites=X'00FF' WHERE id='bd-last'",
        ] {
            let conn = source();
            conn.execute(malformed).unwrap();
            let before = rows(&conn, "SELECT rowid, * FROM issues ORDER BY rowid");
            let dirty = rows(&conn, "SELECT rowid, * FROM dirty_issues ORDER BY rowid");
            let exports = rows(&conn, "SELECT rowid, * FROM export_hashes ORDER BY rowid");
            conn.execute("BEGIN IMMEDIATE").unwrap();
            let error = apply_in_transaction(&conn).unwrap_err();
            assert!(error.to_string().contains("content hash refresh refused"));
            assert_eq!(
                rows(&conn, "SELECT rowid, * FROM issues ORDER BY rowid"),
                before
            );
            assert_eq!(
                rows(&conn, "SELECT rowid, * FROM dirty_issues ORDER BY rowid"),
                dirty
            );
            assert_eq!(
                rows(&conn, "SELECT rowid, * FROM export_hashes ORDER BY rowid"),
                exports
            );
            conn.execute("ROLLBACK").unwrap();
        }
    }
}
