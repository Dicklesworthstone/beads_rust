//! Minimal diagnostic for beads_rust-r9i9: does frankensqlite enforce
//! multiple table-level CHECK constraints, and in what grouping?

use beads_rust::franken_sync::Connection;

fn make_conn(path: &str) -> Connection {
    let _ = std::fs::remove_file(path);
    Connection::open(path.to_string_lossy().into_owned()).unwrap()
}

#[test]
fn single_check_is_enforced() {
    let conn = make_conn("/tmp/r9i9_single.db");
    conn.execute(
        "CREATE TABLE t1 (id TEXT NOT NULL, title TEXT NOT NULL CHECK(length(title) <= 5))",
    )
    .unwrap();
    let ok = conn.execute("INSERT INTO t1 VALUES ('a', 'ok')");
    let bad = conn.execute("INSERT INTO t1 VALUES ('b', 'way-too-long')");
    println!("single: ok={ok:?} bad={bad:?}");
    assert!(ok.is_ok());
    assert!(bad.is_err(), "single CHECK must reject");
}

#[test]
fn two_column_checks_both_enforced() {
    let conn = make_conn("/tmp/r9i9_double.db");
    conn.execute(
        "CREATE TABLE t2 (
            id TEXT NOT NULL,
            title TEXT NOT NULL CHECK(length(title) <= 5),
            prio INTEGER NOT NULL CHECK(priority >= 0)
        )",
    )
    .unwrap();
    let first = conn.execute("INSERT INTO t2 (id, title) VALUES ('a', 'toolong')");
    let second = conn.execute("INSERT INTO t2 (id, title, prio) VALUES ('a', 'ok', -1)");
    println!("double: first={first:?} second={second:?}");
    assert!(first.is_err(), "first CHECK must reject");
    assert!(second.is_err(), "second CHECK must reject");
}

#[test]
fn two_table_level_checks_on_same_column_enforced() {
    // Mirrors the failing fixture shape exactly: same column, two separate
    // table-level CHECK constraints back to back.
    let conn = make_conn("/tmp/r9i9_two_table.db");
    conn.execute(
        "CREATE TABLE issues (
            id TEXT NOT NULL,
            title TEXT NOT NULL CHECK(length(title) <= 500) CHECK(length(title) >= 1)
        )",
    )
    .unwrap();
    let empty = conn.execute("INSERT INTO issues (id, title) VALUES ('x', '')");
    let fine = conn.execute("INSERT INTO issues (id, title) VALUES ('y', 'fine')");
    println!("two_table_level: empty={empty:?} fine={fine:?}");
    assert!(fine.is_ok());
    assert!(empty.is_err(), "second table-level CHECK must reject empty title");
}

#[test]
fn check_with_and_grouping_enforced() {
    let conn = make_conn("/tmp/r9i9_and.db");
    conn.execute(
        "CREATE TABLE issues (
            id TEXT NOT NULL,
            title TEXT NOT NULL CHECK((length(title) >= 1) AND (length(title) <= 500))
        )",
    )
    .unwrap();
    let empty = conn.execute("INSERT INTO issues (id, title) VALUES ('x', '')");
    println!("and_grouping: empty={empty:?}");
    assert!(empty.is_err(), "combined AND CHECK must reject");
}
