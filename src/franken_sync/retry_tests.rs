//! Exercise the production replay path with partial effects, not just flags.

use super::*;

fn row_values(conn: &Connection) -> Vec<Vec<SqliteValue>> {
    conn.query("SELECT value FROM t ORDER BY rowid")
        .unwrap()
        .iter()
        .map(|row| row.values().to_vec())
        .collect()
}

#[test]
fn partially_applied_batches_are_not_replayed_in_either_transaction_mode() {
    for explicit in [false, true] {
        for stale_schema in [false, true] {
            let conn = Connection::open(":memory:").unwrap();
            conn.execute("CREATE TABLE t (value INTEGER)").unwrap();
            if explicit {
                conn.execute("BEGIN IMMEDIATE").unwrap();
            }
            let sql = "INSERT INTO t VALUES (1); INSERT INTO t VALUES (2)";
            let mut calls = 0;
            // Model a batch that completed its first statement before the
            // engine reported recovery/schema failure on the second. Neither
            // an explicit transaction nor autocommit changes its boolean here.
            let result = with_engine_retries!(conn.inner, sql, {
                calls += 1;
                conn.execute("INSERT INTO t VALUES (1)").unwrap();
                if calls == 1 {
                    Err(if stale_schema {
                        FrankenError::SchemaChanged
                    } else {
                        FrankenError::BusyRecovery
                    })
                } else {
                    conn.execute("INSERT INTO t VALUES (2)")
                }
            });
            assert!(matches!(
                result,
                Err(FrankenError::SchemaChanged | FrankenError::BusyRecovery)
            ));
            assert_eq!(calls, 1, "the batch prefix must not execute twice");
            assert_eq!(row_values(&conn), vec![vec![SqliteValue::Integer(1)]]);
            assert_eq!(conn.as_async().in_transaction(), explicit);
            if explicit {
                conn.execute("ROLLBACK").unwrap();
                assert!(row_values(&conn).is_empty());
            }
        }
    }
}

#[test]
fn commit_then_begin_is_a_new_transaction_even_when_the_flag_is_unchanged() {
    let conn = Connection::open(":memory:").unwrap();
    conn.execute("CREATE TABLE t (value INTEGER)").unwrap();
    conn.execute("BEGIN IMMEDIATE").unwrap();
    conn.execute("INSERT INTO t VALUES (1)").unwrap();
    let sql = "COMMIT; BEGIN IMMEDIATE; INSERT INTO t VALUES (2)";
    let mut calls = 0;
    let result = with_engine_retries!(conn.inner, sql, {
        calls += 1;
        conn.execute("COMMIT").unwrap();
        conn.execute("BEGIN IMMEDIATE").unwrap();
        if calls == 1 {
            Err(FrankenError::BusyRecovery)
        } else {
            conn.execute("INSERT INTO t VALUES (2)")
        }
    });
    assert!(matches!(result, Err(FrankenError::BusyRecovery)));
    assert_eq!(calls, 1);
    assert!(conn.as_async().in_transaction());
    conn.execute("ROLLBACK").unwrap();
    // The intentional first COMMIT remains committed. The facade neither
    // duplicates the prefix nor invents a transaction rollback around it.
    assert_eq!(row_values(&conn), vec![vec![SqliteValue::Integer(1)]]);
}

#[test]
fn direct_maintenance_errors_neither_retry_nor_prepare_as_a_schema_refresh() {
    for stale_schema in [false, true] {
        let conn = Connection::open(":memory:").unwrap();
        conn.execute("PRAGMA user_version = 17").unwrap();
        let sql = "PRAGMA user_version = 42";
        let mut calls = 0;
        let result = with_engine_retries!(conn.inner, sql, {
            calls += 1;
            if calls == 1 {
                Err(if stale_schema {
                    FrankenError::SchemaChanged
                } else {
                    FrankenError::BusyRecovery
                })
            } else {
                Ok(0usize)
            }
        });
        assert!(matches!(
            result,
            Err(FrankenError::SchemaChanged | FrankenError::BusyRecovery)
        ));
        assert_eq!(calls, 1);
        assert_eq!(
            conn.query_row("PRAGMA user_version").unwrap().values(),
            &[SqliteValue::Integer(17)]
        );
    }
}

#[test]
fn ordinary_bound_sql_retains_recovery_and_one_schema_refresh() {
    let conn = Connection::open(":memory:").unwrap();
    let sql = "SELECT ?1, ?2";
    let params = [SqliteValue::from("keep; this binding"), SqliteValue::Integer(23)];
    let mut calls = 0;
    let result = with_engine_retries!(conn.inner, sql, {
        calls += 1;
        match calls {
            1 => Err(FrankenError::BusyRecovery),
            2 => Err(FrankenError::SchemaChanged),
            _ => drive(conn.inner.query_row_with_params(sql, &params)),
        }
    });
    assert_eq!(result.unwrap().values(), &params);
    assert_eq!(calls, 3);

    calls = 0;
    let result: Result<(), FrankenError> = with_engine_retries!(conn.inner, sql, {
        calls += 1;
        Err(FrankenError::SchemaChanged)
    });
    assert!(matches!(result, Err(FrankenError::SchemaChanged)));
    assert_eq!(calls, 2, "persistent schema errors must not recompile forever");
}

#[test]
fn failed_direct_recompilation_does_not_execute_a_stale_statement_again() {
    let conn = Connection::open(":memory:").unwrap();
    let sql = "SELECT value FROM missing_table";
    let mut calls = 0;
    let result: Result<(), FrankenError> = with_engine_retries!(conn.inner, sql, {
        calls += 1;
        Err(FrankenError::SchemaChanged)
    });
    assert!(matches!(result, Err(FrankenError::NoSuchTable { .. })));
    assert_eq!(calls, 1, "failed preparation must be surfaced, not ignored");
}

#[test]
fn direct_retries_still_stop_when_one_statement_changes_transaction_state() {
    for explicit in [false, true] {
        for stale_schema in [false, true] {
            let conn = Connection::open(":memory:").unwrap();
            if explicit {
                conn.execute("BEGIN IMMEDIATE").unwrap();
            }
            let sql = "SELECT 1";
            let mut calls = 0;
            let result: Result<(), FrankenError> = with_engine_retries!(conn.inner, sql, {
                calls += 1;
                if calls == 1 {
                    conn.execute(if explicit { "ROLLBACK" } else { "BEGIN IMMEDIATE" })
                        .unwrap();
                    Err(if stale_schema {
                        FrankenError::SchemaChanged
                    } else {
                        FrankenError::BusyRecovery
                    })
                } else {
                    Ok(())
                }
            });
            assert!(matches!(
                result,
                Err(FrankenError::SchemaChanged | FrankenError::BusyRecovery)
            ));
            assert_eq!(calls, 1);
            assert_eq!(conn.as_async().in_transaction(), !explicit);
            if !explicit {
                conn.execute("ROLLBACK").unwrap();
            }
        }
    }
}

fn execute_raw_path(conn: &Connection, sql: &str, call: usize) -> Result<(), FrankenError> {
    let engine = conn.as_async();
    match call {
        0 => drive(engine.execute(sql)).map(|_| ()),
        1 => drive(engine.execute_with_params(sql, &[])).map(|_| ()),
        2 => drive(engine.query(sql)).map(|_| ()),
        3 => drive(engine.query_with_params(sql, &[])).map(|_| ()),
        4 => drive(engine.query_row(sql)).map(|_| ()),
        _ => drive(engine.query_row_with_params(sql, &[])).map(|_| ()),
    }
}

fn execute_facade_path(conn: &Connection, sql: &str, call: usize) -> Result<(), FrankenError> {
    match call {
        0 => conn.execute(sql).map(|_| ()),
        1 => conn.execute_with_params(sql, &[]).map(|_| ()),
        2 => conn.query(sql).map(|_| ()),
        3 => conn.query_with_params(sql, &[]).map(|_| ()),
        4 => conn.query_row(sql).map(|_| ()),
        _ => conn.query_row_with_params(sql, &[]).map(|_| ()),
    }
}

#[test]
fn public_batch_paths_match_one_raw_engine_dispatch_on_success_and_failure() {
    // Different engine entry points may accept a batch or reject it. Compare
    // with that entry point's real single-dispatch semantics, not an assumed
    // row count, and require the facade to preserve both the error and effects.
    for sql in [
        "INSERT INTO t VALUES (1); INSERT INTO t VALUES (2)",
        "INSERT INTO t VALUES (1); INSERT INTO missing_table VALUES (2)",
        "INSERT INTO t VALUES (1); SELECT value FROM missing_table",
    ] {
        for call in 0..6 {
            let raw = Connection::open(":memory:").unwrap();
            let wrapped = Connection::open(":memory:").unwrap();
            raw.execute("CREATE TABLE t (value INTEGER)").unwrap();
            wrapped.execute("CREATE TABLE t (value INTEGER)").unwrap();
            let expected = execute_raw_path(&raw, sql, call).map_err(|error| format!("{error:?}"));
            let actual =
                execute_facade_path(&wrapped, sql, call).map_err(|error| format!("{error:?}"));
            assert_eq!(actual, expected, "entry point {call}: {sql}");
            assert_eq!(
                row_values(&wrapped),
                row_values(&raw),
                "entry point {call} must not duplicate a completed batch prefix: {sql}"
            );
            assert_eq!(
                wrapped.as_async().in_transaction(),
                raw.as_async().in_transaction()
            );
        }
    }
}
