//! Regression coverage for beads_rust-r9i9: FrankenSQLite must enforce every
//! `CHECK` constraint, including consecutive constraints in comment-prefixed
//! DDL.

use beads_rust::franken_sync::Connection;
use std::path::PathBuf;
use tempfile::TempDir;

fn make_conn(name: &str) -> (TempDir, PathBuf, Connection) {
    let temp = tempfile::tempdir().expect("create isolated database directory");
    let path = temp.path().join(name);
    let conn = Connection::open(path.to_string_lossy().into_owned())
        .expect("open isolated diagnostic database");
    (temp, path, conn)
}

#[test]
fn single_check_is_enforced() {
    let (_temp, _path, conn) = make_conn("single.db");
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
    let (_temp, _path, conn) = make_conn("two-columns.db");
    conn.execute(
        "CREATE TABLE t2 (
            id TEXT NOT NULL,
            title TEXT NOT NULL CHECK(length(title) <= 5),
            priority INTEGER NOT NULL CHECK(priority >= 0)
        )",
    )
    .unwrap();
    let first = conn.execute("INSERT INTO t2 (id, title, priority) VALUES ('a', 'toolong', 0)");
    let second = conn.execute("INSERT INTO t2 (id, title, priority) VALUES ('b', 'ok', -1)");
    println!("double: first={first:?} second={second:?}");
    assert!(first.is_err(), "title CHECK must reject overlong title");
    assert!(second.is_err(), "priority CHECK must reject negative value");
}

#[test]
fn consecutive_checks_in_comment_prefixed_ddl_survive_reopen() {
    let (_temp, path, conn) = make_conn("consecutive.db");
    conn.execute(
        "-- Schema statements include leading maintenance comments.
        CREATE TABLE issues (
            id TEXT NOT NULL,
            title TEXT NOT NULL CHECK(length(title) <= 5) CHECK(length(title) >= 1)
        )",
    )
    .unwrap();
    let fine = conn.execute("INSERT INTO issues (id, title) VALUES ('ok', 'fine')");
    let empty = conn.execute("INSERT INTO issues (id, title) VALUES ('x', '')");
    let overlong = conn.execute("INSERT INTO issues (id, title) VALUES ('y', 'toolong')");
    println!("consecutive live: fine={fine:?} empty={empty:?} overlong={overlong:?}");
    assert!(fine.is_ok());
    assert!(empty.is_err(), "lower-bound CHECK must reject empty title");
    assert!(
        overlong.is_err(),
        "upper-bound CHECK must reject overlong title"
    );

    drop(conn);
    let reopened =
        Connection::open(path.to_string_lossy().into_owned()).expect("reopen diagnostic database");
    let empty = reopened.execute("INSERT INTO issues (id, title) VALUES ('z', '')");
    let overlong = reopened.execute("INSERT INTO issues (id, title) VALUES ('w', 'toolong')");
    println!("consecutive reopened: empty={empty:?} overlong={overlong:?}");
    assert!(
        empty.is_err(),
        "reopened lower-bound CHECK must reject empty title"
    );
    assert!(
        overlong.is_err(),
        "reopened upper-bound CHECK must reject overlong title"
    );
}

#[test]
fn compound_checks_matching_the_issues_schema_are_enforced() {
    let (_temp, _path, conn) = make_conn("compound-checks.db");
    conn.execute(
        "CREATE TABLE issues (
            id TEXT NOT NULL,
            priority INTEGER NOT NULL DEFAULT 2 CHECK(priority >= 0 AND priority <= 4),
            status TEXT NOT NULL DEFAULT 'open',
            closed_at TEXT,
            CHECK (
                (status = 'closed' AND closed_at IS NOT NULL) OR
                (status = 'tombstone') OR
                (status NOT IN ('closed', 'tombstone') AND closed_at IS NULL)
            )
        )",
    )
    .unwrap();
    let fine = conn.execute(
        "INSERT INTO issues (id, priority, status, closed_at) VALUES ('ok', 2, 'open', NULL)",
    );
    let priority_low = conn.execute(
        "INSERT INTO issues (id, priority, status, closed_at) VALUES ('low', -1, 'open', NULL)",
    );
    let priority_high = conn.execute(
        "INSERT INTO issues (id, priority, status, closed_at) VALUES ('high', 5, 'open', NULL)",
    );
    let closed_without_time = conn.execute(
        "INSERT INTO issues (id, priority, status, closed_at) VALUES ('closed', 2, 'closed', NULL)",
    );
    let open_with_time = conn.execute(
        "INSERT INTO issues (id, priority, status, closed_at) \
         VALUES ('open-time', 2, 'open', '2026-08-26T00:00:00Z')",
    );
    println!(
        "compound issues checks: fine={fine:?} priority_low={priority_low:?} \
         priority_high={priority_high:?} closed_without_time={closed_without_time:?} \
         open_with_time={open_with_time:?}"
    );
    assert!(fine.is_ok(), "issues CHECKs must accept a valid open row");
    assert!(
        priority_low.is_err(),
        "priority CHECK must reject a negative priority"
    );
    assert!(
        priority_high.is_err(),
        "priority CHECK must reject a priority above four"
    );
    assert!(
        closed_without_time.is_err(),
        "closed-state CHECK must require closed_at"
    );
    assert!(
        open_with_time.is_err(),
        "closed-state CHECK must reject closed_at on an open issue"
    );
}
