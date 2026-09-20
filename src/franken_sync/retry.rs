//! Prove that automatic replay is confined to one ordinary query or DML statement.
//!
//! An unchanged autocommit flag is not an atomicity receipt: an earlier
//! statement in a batch can have committed, or COMMIT followed by BEGIN can
//! leave that flag unchanged while replacing the transaction. Maintenance and
//! transaction control therefore stay with their caller, even on transient or
//! schema errors. This guard does not reject SQL or change its first execution.

use std::cell::Cell;

use fsqlite_ast::Statement;

/// Cache a statement-boundary proof only when an error would cause replay.
/// Successful calls incur no additional SQL parse, including prepared calls.
pub(super) struct ReplaySafety<'sql> {
    sql: &'sql str,
    replayable: Cell<Option<bool>>,
}

impl<'sql> ReplaySafety<'sql> {
    pub(super) const fn new(sql: &'sql str) -> Self {
        Self {
            sql,
            replayable: Cell::new(None),
        }
    }

    pub(super) fn allows_replay(&self) -> bool {
        if let Some(replayable) = self.replayable.get() {
            return replayable;
        }
        // Use the engine grammar, not semicolon counting: strings, comments,
        // quoted identifiers and trigger bodies may contain semicolons. Parser
        // recovery must not turn a malformed suffix into a single-statement proof.
        let (statements, errors) = fsqlite_parser::Parser::from_sql(self.sql).parse_all();
        let replayable = errors.is_empty()
            && matches!(
                statements.as_slice(),
                [Statement::Select(_)
                    | Statement::Insert(_)
                    | Statement::Update(_)
                    | Statement::Delete(_)]
            );
        // Deliberately fail closed for transaction control, PRAGMAs, VACUUM,
        // ATTACH/DETACH, DDL and new AST variants. Their preparation or
        // execution is not covered by the ordinary query/DML replay contract.
        self.replayable.set(Some(replayable));
        replayable
    }
}

#[cfg(test)]
mod tests {
    use super::ReplaySafety;
    use crate::franken_sync::{Connection, retry_transient};

    #[test]
    fn replay_proof_understands_sql_boundaries_not_semicolon_counts() {
        for sql in [
            "SELECT ';COMMIT;' AS value",
            "/* ; BEGIN */ SELECT 1; -- ; ROLLBACK",
            ";; SELECT 1;;",
            "INSERT INTO t VALUES ('café; it''s one value')",
            "UPDATE t SET value = ';' WHERE value = 'old'",
            "DELETE FROM t WHERE value = ';'",
            "WITH c(value) AS (SELECT ';') SELECT value FROM c",
        ] {
            assert!(ReplaySafety::new(sql).allows_replay(), "{sql}");
        }
    }

    #[test]
    fn replay_proof_refuses_batches_partial_parses_and_nontransactional_work() {
        for sql in [
            "",
            ";; -- comments only",
            "INSERT INTO t VALUES (1); INSERT INTO t VALUES (2)",
            "SELECT 1; SELECT 2",
            "BEGIN; INSERT INTO t VALUES (1); COMMIT",
            "COMMIT; BEGIN IMMEDIATE",
            "SELECT 1; SELECT (",
            "SELECT (; SELECT 1",
            "BEGIN IMMEDIATE",
            "COMMIT",
            "END TRANSACTION",
            "ROLLBACK",
            "SAVEPOINT retry_owner",
            "ROLLBACK TO retry_owner",
            "RELEASE retry_owner",
            "PRAGMA user_version = 23",
            "PRAGMA wal_checkpoint(TRUNCATE)",
            "VACUUM",
            "VACUUM INTO 'copy.db'",
            "ATTACH DATABASE ':memory:' AS other",
            "DETACH DATABASE other",
            "REINDEX",
            "ANALYZE",
            "CREATE TABLE \"t;one\" (value TEXT DEFAULT ';')",
            "CREATE INDEX ix ON t(value)",
            "CREATE VIEW v AS SELECT ';' AS value",
            "CREATE TRIGGER tr AFTER INSERT ON t BEGIN \
             INSERT INTO audit VALUES ('one;two'); UPDATE audit SET value = ';'; END;",
            "DROP TABLE t",
            "ALTER TABLE t ADD COLUMN extra INTEGER",
            "CREATE VIRTUAL TABLE v USING fts5(value)",
            "EXPLAIN PRAGMA user_version = 23",
        ] {
            assert!(!ReplaySafety::new(sql).allows_replay(), "{sql}");
        }
    }

    #[test]
    fn successful_execution_does_not_parse_or_reject_the_sql() {
        let conn = Connection::open(":memory:").unwrap();
        let safety = ReplaySafety::new("SELECT 1; SELECT 2");
        let mut calls = 0;
        let result = retry_transient(conn.as_async(), &safety, || {
            calls += 1;
            Ok(23)
        });
        assert_eq!(result.unwrap(), 23);
        assert_eq!(calls, 1);
        assert_eq!(safety.replayable.get(), None);
        assert!(!safety.allows_replay());
        assert_eq!(safety.replayable.get(), Some(false));
    }
}
