//! Recovery for reusable engine statements, without losing their SQL or bindings.
//!
//! Checkpoints do not enter this module: the facade executes those afresh and
//! validates each completion receipt. Ordinary prepared statements need the same
//! transient-error policy as direct SQL, plus replacement of a stale program.

use std::cell::RefCell;

use super::{FrankenError, Row, SqliteValue, drive, retry_busy_recovery, retry_transient};

pub(super) struct EngineStatement<'conn> {
    connection: &'conn fsqlite::Connection,
    sql: String,
    statement: RefCell<fsqlite::PreparedStatement<'conn>>,
}

impl<'conn> EngineStatement<'conn> {
    pub(super) fn new(
        connection: &'conn fsqlite::Connection,
        sql: &str,
    ) -> Result<Self, FrankenError> {
        let statement = retry_busy_recovery(|| drive(connection.prepare(sql)))?;
        Ok(Self {
            connection,
            sql: sql.to_string(),
            statement: RefCell::new(statement),
        })
    }

    /// Retry only failures already classified by the facade as safe to retry.
    /// In particular, an explicit transaction's snapshot conflict must return
    /// to its owner, not replay just this statement. Recompilation is bounded
    /// to once per invocation and replaces the cached program, not just the
    /// connection's schema image. Parameters stay in the invocation closure.
    fn run<T>(
        &self,
        mut action: impl FnMut(&fsqlite::PreparedStatement<'conn>) -> Result<T, FrankenError>,
    ) -> Result<T, FrankenError> {
        let was_in_transaction = self.connection.in_transaction();
        let result = retry_transient(self.connection, || action(&self.statement.borrow()))
            .or_else(|error| {
                if !super::schema_stale(&error)
                    || was_in_transaction != self.connection.in_transaction()
                {
                    return Err(error);
                }
                let refreshed = retry_busy_recovery(|| drive(self.connection.prepare(&self.sql)))?;
                // No statement borrow or engine future survives an attempt.
                // Drop the stale program before driving the replacement.
                drop(self.statement.replace(refreshed));
                retry_transient(self.connection, || action(&self.statement.borrow()))
            });
        if matches!(&result, Err(FrankenError::BusyRecovery)) {
            super::wal_index::warn_if_poisoned(self.connection.path());
        }
        result
    }

    pub(super) fn explain(&self) -> String {
        self.statement.borrow().explain()
    }

    pub(super) fn query(&self) -> Result<Vec<Row>, FrankenError> {
        self.run(|statement| drive(statement.query()))
    }

    pub(super) fn query_with_params(
        &self,
        params: &[SqliteValue],
    ) -> Result<Vec<Row>, FrankenError> {
        self.run(|statement| drive(statement.query_with_params(params)))
    }

    pub(super) fn query_row(&self) -> Result<Row, FrankenError> {
        self.run(|statement| drive(statement.query_row()))
    }

    pub(super) fn query_row_with_params(
        &self,
        params: &[SqliteValue],
    ) -> Result<Row, FrankenError> {
        self.run(|statement| drive(statement.query_row_with_params(params)))
    }

    pub(super) fn execute(&self) -> Result<usize, FrankenError> {
        self.run(|statement| drive(statement.execute()))
    }

    pub(super) fn execute_with_params(
        &self,
        params: &[SqliteValue],
    ) -> Result<usize, FrankenError> {
        self.run(|statement| drive(statement.execute_with_params(params)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::franken_sync::Connection;

    #[test]
    fn prepared_recovery_retries_without_duplicating_parameterized_writes() {
        let conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (value TEXT)").unwrap();
        let statement = EngineStatement::new(conn.as_async(), "INSERT INTO t VALUES (?1)").unwrap();
        let params = [SqliteValue::from("write exactly once")];
        let mut attempts = 0;
        let affected = statement
            .run(|engine| {
                attempts += 1;
                if attempts == 1 {
                    return Err(FrankenError::BusyRecovery);
                }
                drive(engine.execute_with_params(&params))
            })
            .unwrap();
        assert_eq!(attempts, 2);
        assert_eq!(affected, 1);
        let rows = conn.query("SELECT value FROM t").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values(), &params);
    }

    #[test]
    fn prepared_recovery_and_schema_refresh_keep_current_bindings() {
        let conn = Connection::open(":memory:").unwrap();
        let statement = EngineStatement::new(conn.as_async(), "SELECT ?1, ?2").unwrap();
        let params = [SqliteValue::from("current binding"), SqliteValue::Integer(23)];
        let mut attempts = 0;
        let row = statement
            .run(|engine| {
                attempts += 1;
                match attempts {
                    1 => Err(FrankenError::BusyRecovery),
                    2 => Err(FrankenError::SchemaChanged),
                    _ => drive(engine.query_row_with_params(&params)),
                }
            })
            .unwrap();
        assert_eq!(attempts, 3);
        assert_eq!(row.values(), &params);
        let next = [SqliteValue::from("next invocation"), SqliteValue::Integer(42)];
        assert_eq!(statement.query_row_with_params(&next).unwrap().values(), &next);
    }

    #[test]
    fn prepared_schema_refresh_replaces_the_compiled_projection() {
        let conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (value TEXT)").unwrap();
        conn.execute("INSERT INTO t VALUES ('old')").unwrap();
        let statement = EngineStatement::new(conn.as_async(), "SELECT * FROM t").unwrap();
        assert_eq!(statement.query_row().unwrap().values().len(), 1);
        conn.execute("DROP TABLE t").unwrap();
        conn.execute("CREATE TABLE t (value TEXT, extra INTEGER)").unwrap();
        conn.execute("INSERT INTO t VALUES ('new', 7)").unwrap();
        let mut attempts = 0;
        let row = statement
            .run(|engine| {
                attempts += 1;
                if attempts == 1 {
                    return Err(FrankenError::SchemaChanged);
                }
                drive(engine.query_row())
            })
            .unwrap();
        assert_eq!(attempts, 2);
        assert_eq!(row.values(), &[SqliteValue::from("new"), SqliteValue::Integer(7)]);
        assert_eq!(statement.query_row().unwrap().values(), row.values());
    }

    #[test]
    fn prepared_schema_refresh_is_bounded_and_preserves_real_resolution_errors() {
        let conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (value TEXT)").unwrap();
        let statement = EngineStatement::new(conn.as_async(), "SELECT value FROM t").unwrap();
        let mut attempts = 0;
        let result: Result<(), FrankenError> = statement.run(|_| {
            attempts += 1;
            Err(FrankenError::SchemaChanged)
        });
        assert!(matches!(result, Err(FrankenError::SchemaChanged)));
        assert_eq!(attempts, 2, "a persistent schema error must not spin");

        conn.execute("DROP TABLE t").unwrap();
        attempts = 0;
        let result: Result<(), FrankenError> = statement.run(|_| {
            attempts += 1;
            Err(FrankenError::SchemaChanged)
        });
        assert!(matches!(result, Err(FrankenError::NoSuchTable { .. })));
        assert_eq!(attempts, 1, "failed recompilation must not execute a stale plan");
    }

    #[test]
    fn prepared_recovery_does_not_retry_lock_contention_or_corruption() {
        let conn = Connection::open(":memory:").unwrap();
        let statement = EngineStatement::new(conn.as_async(), "SELECT 1").unwrap();
        for corruption in [false, true] {
            let mut attempts = 0;
            let result: Result<(), FrankenError> = statement.run(|_| {
                attempts += 1;
                if corruption {
                    Err(FrankenError::WalCorrupt {
                        detail: "retain the original corruption diagnostic".to_string(),
                    })
                } else {
                    Err(FrankenError::Busy)
                }
            });
            assert_eq!(attempts, 1);
            if corruption {
                assert!(matches!(
                    result,
                    Err(FrankenError::WalCorrupt { detail })
                        if detail == "retain the original corruption diagnostic"
                ));
            } else {
                assert!(matches!(result, Err(FrankenError::Busy)));
            }
        }
    }

    #[test]
    fn prepared_recovery_never_replays_across_a_transaction_boundary() {
        for explicit in [false, true] {
            for stale_schema in [false, true] {
                let conn = Connection::open(":memory:").unwrap();
                conn.execute("CREATE TABLE t (value INTEGER)").unwrap();
                let statement =
                    EngineStatement::new(conn.as_async(), "INSERT INTO t VALUES (1)").unwrap();
                if explicit {
                    conn.execute("BEGIN IMMEDIATE").unwrap();
                }
                let mut attempts = 0;
                let result = statement.run(|engine| {
                    attempts += 1;
                    if attempts == 1 {
                        // Model an engine failure that changed transaction state.
                        // Replaying after ROLLBACK would commit outside the
                        // caller's transaction; after BEGIN it would join a new one.
                        conn.execute(if explicit { "ROLLBACK" } else { "BEGIN IMMEDIATE" })?;
                        return Err(if stale_schema {
                            FrankenError::SchemaChanged
                        } else {
                            FrankenError::BusyRecovery
                        });
                    }
                    drive(engine.execute())
                });
                assert!(matches!(
                    result,
                    Err(FrankenError::SchemaChanged | FrankenError::BusyRecovery)
                ));
                assert_eq!(attempts, 1, "transaction-changing failures must reach the owner");
                assert!(conn.query("SELECT value FROM t").unwrap().is_empty());
                assert_eq!(conn.as_async().in_transaction(), !explicit);
                if !explicit {
                    conn.execute("ROLLBACK").unwrap();
                }
            }
        }
    }

    #[test]
    fn prepared_public_paths_keep_bindings_and_explicit_transactions() {
        let conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE t (value INTEGER)").unwrap();
        conn.execute("BEGIN IMMEDIATE").unwrap();
        let first = conn.prepare("INSERT INTO t VALUES (1)").unwrap();
        assert_eq!(first.execute().unwrap(), 1);
        let second = conn.prepare("INSERT INTO t VALUES (?1)").unwrap();
        assert_eq!(second.execute_with_params(&[SqliteValue::Integer(2)]).unwrap(), 1);
        let query = conn.prepare("SELECT value FROM t ORDER BY value").unwrap();
        assert_eq!(query.query().unwrap().len(), 2);
        let bound = conn.prepare("SELECT value FROM t WHERE value = ?1").unwrap();
        let params = [SqliteValue::Integer(2)];
        assert_eq!(bound.query_with_params(&params).unwrap()[0].values(), &params);
        assert_eq!(bound.query_row_with_params(&params).unwrap().values(), &params);
        let single = conn.prepare("SELECT value FROM t WHERE value = 1").unwrap();
        assert_eq!(single.query_row().unwrap().values(), &[SqliteValue::Integer(1)]);
        assert!(conn.as_async().in_transaction());
        conn.execute("ROLLBACK").unwrap();
        assert!(query.query().unwrap().is_empty());
    }
}
