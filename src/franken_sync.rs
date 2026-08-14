//! Synchronous facade over the async FrankenSQLite 0.3 engine API.
//!
//! fsqlite 0.2 made every engine entry point `async` with `!Send` futures
//! (the engine is `Rc<RefCell<..>>` internally; it was already `!Send` at
//! 0.1.x — only the call shape changed), and fsqlite 0.3 moved the runtime
//! family to asupersync 0.4.3. br's storage layer is fully synchronous, so
//! this module preserves the pre-0.2 blocking call shape by driving each
//! engine future to completion on the calling thread with a private
//! current-thread `asupersync` runtime (the proven sqlmodel/cass
//! `block_on` bridge pattern; see coding_agent_session_search
//! `src/franken_sync.rs`).
//!
//! Every future is created, polled, and dropped entirely within one bridge
//! call, so the engine's `Rc<RefCell<..>>` state never crosses a thread
//! boundary between poll steps. `Runtime::block_on` has no `Send` bound and
//! saves/restores the ambient runtime handle, so nesting inside a consumer's
//! own `block_on` is safe.
//!
//! The runtime lives in a thread-local slot and is *taken out* while a
//! future is being driven: a reentrant bridge call (e.g. SQL issued from
//! inside a row-mapping closure) finds the slot empty and builds a fresh
//! runtime instead of re-entering `block_on` on the same runtime instance.
//!
//! Everything outside this module refers to the engine through
//! `crate::franken_sync::` (or `beads_rust::franken_sync::` from integration
//! tests); only this module names the `fsqlite` dependency directly for
//! connection/statement driving.

use std::cell::RefCell;
use std::future::Future;

use asupersync::runtime::{Runtime, RuntimeBuilder};

pub use fsqlite::{FrankenError, Row, SqliteValue};

// ---------------------------------------------------------------------------
// Bridge driver
// ---------------------------------------------------------------------------

thread_local! {
    static DRIVER: RefCell<Option<Runtime>> = const { RefCell::new(None) };
}

/// Drive a `!Send` fsqlite future to completion on the calling thread.
fn drive<T>(future: impl Future<Output = T>) -> T {
    let runtime = DRIVER
        .with(|slot| slot.borrow_mut().take())
        .unwrap_or_else(|| {
            RuntimeBuilder::current_thread()
                .build()
                .expect("failed to build FrankenSQLite sync-bridge runtime")
        });
    let output = runtime.block_on(future);
    DRIVER.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.is_none() {
            *slot = Some(runtime);
        }
    });
    output
}

/// True when `err` can mean the connection's schema image predates another
/// connection's DDL commit.
///
/// fsqlite 0.2.1+ behavior (verified by standalone probe, absent at 0.1.x):
/// a connection opened before another connection CREATEs a table may not see
/// that table through the plain `query`/`execute` paths — but `prepare()`
/// refreshes the shared schema publication before resolving, after which the
/// same SQL succeeds. The facade therefore treats these errors as
/// possibly-stale-schema, drives a `prepare()` of the same SQL to force the
/// refresh, and retries once. Plan-time resolution failures have no side
/// effects, so the retry is safe.
fn schema_stale(err: &FrankenError) -> bool {
    matches!(
        err,
        FrankenError::NoSuchTable { .. }
            | FrankenError::NoSuchColumn { .. }
            | FrankenError::NoSuchIndex { .. }
    )
}

/// Bounded retry for `FrankenError::BusyRecovery`.
///
/// fsqlite 0.2+ ns-lifecycle opens can put a database into a short
/// "recovery in progress" window; statements admitted during that window
/// fail with `BusyRecovery` immediately instead of waiting out the
/// connection's busy timeout. C SQLite's busy handler covers
/// `SQLITE_BUSY_RECOVERY`, and the 0.1.x line had no recovery windows at
/// all, so a bounded caller-side retry restores the pre-0.2 observable
/// behavior. Plain `Busy` is deliberately NOT retried here: br classifies
/// ordinary lock contention itself and the engine owns that timeout.
fn retry_busy_recovery<T>(
    mut attempt: impl FnMut() -> Result<T, FrankenError>,
) -> Result<T, FrankenError> {
    const RETRY_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);
    const BACKOFF_CAP: std::time::Duration = std::time::Duration::from_millis(250);
    let start = std::time::Instant::now();
    let mut backoff = std::time::Duration::from_millis(5);
    loop {
        match attempt() {
            Err(FrankenError::BusyRecovery) if start.elapsed() < RETRY_BUDGET => {
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(BACKOFF_CAP);
            }
            other => return other,
        }
    }
}

macro_rules! with_engine_retries {
    ($conn:expr, $sql:expr, $attempt:expr) => {{
        let first = retry_busy_recovery(|| $attempt);
        match first {
            Err(ref err) if schema_stale(err) => {
                // `prepare` refreshes the schema image from the shared
                // publication plane even when it ultimately fails to resolve.
                let _ = drive($conn.prepare($sql));
                retry_busy_recovery(|| $attempt)
            }
            other => other,
        }
    }};
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

/// Synchronous wrapper over [`fsqlite::Connection`] with the pre-0.2
/// blocking method signatures.
pub struct Connection {
    inner: fsqlite::Connection,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("path", &self.inner.path())
            .finish_non_exhaustive()
    }
}

impl Connection {
    /// Open (or create) a database at `path`.
    pub fn open(path: impl Into<String>) -> Result<Self, FrankenError> {
        Ok(Self {
            inner: drive(fsqlite::Connection::open(path))?,
        })
    }

    /// Access the wrapped async connection (escape hatch for callers that
    /// drive engine APIs this facade does not wrap).
    #[must_use]
    pub const fn as_async(&self) -> &fsqlite::Connection {
        &self.inner
    }

    /// Execute a single SQL statement, returning the affected row count.
    pub fn execute(&self, sql: &str) -> Result<usize, FrankenError> {
        with_engine_retries!(self.inner, sql, drive(self.inner.execute(sql)))
    }

    /// Execute a single SQL statement with positional parameters.
    pub fn execute_with_params(
        &self,
        sql: &str,
        params: &[SqliteValue],
    ) -> Result<usize, FrankenError> {
        with_engine_retries!(
            self.inner,
            sql,
            drive(self.inner.execute_with_params(sql, params))
        )
    }

    /// Query, returning all rows.
    pub fn query(&self, sql: &str) -> Result<Vec<Row>, FrankenError> {
        with_engine_retries!(self.inner, sql, drive(self.inner.query(sql)))
    }

    /// Query with positional parameters, returning all rows.
    pub fn query_with_params(
        &self,
        sql: &str,
        params: &[SqliteValue],
    ) -> Result<Vec<Row>, FrankenError> {
        with_engine_retries!(
            self.inner,
            sql,
            drive(self.inner.query_with_params(sql, params))
        )
    }

    /// Query, returning exactly one row.
    pub fn query_row(&self, sql: &str) -> Result<Row, FrankenError> {
        with_engine_retries!(self.inner, sql, drive(self.inner.query_row(sql)))
    }

    /// Query with positional parameters, returning exactly one row.
    pub fn query_row_with_params(
        &self,
        sql: &str,
        params: &[SqliteValue],
    ) -> Result<Row, FrankenError> {
        with_engine_retries!(
            self.inner,
            sql,
            drive(self.inner.query_row_with_params(sql, params))
        )
    }

    /// Prepare a statement for repeated execution.
    pub fn prepare(&self, sql: &str) -> Result<PreparedStatement<'_>, FrankenError> {
        Ok(PreparedStatement {
            inner: retry_busy_recovery(|| drive(self.inner.prepare(sql)))?,
        })
    }

    /// Last-inserted rowid on this connection.
    #[must_use]
    pub fn last_insert_rowid(&self) -> i64 {
        self.inner.last_insert_rowid()
    }

    /// Close the connection (rolls back any active transaction, then runs the
    /// final passive WAL checkpoint).
    pub fn close(mut self) -> Result<(), FrankenError> {
        drive(self.inner.close_in_place())
    }

    /// Close in place, retaining the handle on error so callers can retry.
    pub fn close_in_place(&mut self) -> Result<(), FrankenError> {
        drive(self.inner.close_in_place())
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        // fsqlite 0.1.x closed on drop (best-effort, no checkpoint); 0.2+'s
        // `Drop` cannot await and so skips that teardown. Driving the same
        // best-effort close here restores the 0.1.x observable contract that
        // writes made through a dropped connection are visible to any later
        // open (br #270 relies on Drop flushing the WAL). This is a no-op if
        // the connection was already explicitly closed.
        drive(self.inner.close_best_effort_in_place());
    }
}

// ---------------------------------------------------------------------------
// Prepared statements
// ---------------------------------------------------------------------------

/// Synchronous wrapper over [`fsqlite::PreparedStatement`].
pub struct PreparedStatement<'conn> {
    inner: fsqlite::PreparedStatement<'conn>,
}

impl PreparedStatement<'_> {
    /// Render the compiled program for diagnostics (sync in fsqlite).
    #[must_use]
    pub fn explain(&self) -> String {
        self.inner.explain()
    }

    /// Query, returning all rows.
    pub fn query(&self) -> Result<Vec<Row>, FrankenError> {
        drive(self.inner.query())
    }

    /// Query with positional parameters, returning all rows.
    pub fn query_with_params(&self, params: &[SqliteValue]) -> Result<Vec<Row>, FrankenError> {
        drive(self.inner.query_with_params(params))
    }

    /// Query, returning exactly one row.
    pub fn query_row(&self) -> Result<Row, FrankenError> {
        drive(self.inner.query_row())
    }

    /// Query with positional parameters, returning exactly one row.
    pub fn query_row_with_params(&self, params: &[SqliteValue]) -> Result<Row, FrankenError> {
        drive(self.inner.query_row_with_params(params))
    }

    /// Execute, returning the affected row count.
    pub fn execute(&self) -> Result<usize, FrankenError> {
        drive(self.inner.execute())
    }

    /// Execute with positional parameters, returning the affected row count.
    pub fn execute_with_params(&self, params: &[SqliteValue]) -> Result<usize, FrankenError> {
        drive(self.inner.execute_with_params(params))
    }
}

// ---------------------------------------------------------------------------
// compat: rusqlite-style open flags, synchronous form
// ---------------------------------------------------------------------------

pub mod compat {
    use super::{Connection, FrankenError, drive};

    pub use fsqlite::compat::OpenFlags;

    /// Open a database with rusqlite-style open flags (synchronous form of
    /// [`fsqlite::compat::open_with_flags`]).
    pub fn open_with_flags(path: &str, flags: OpenFlags) -> Result<Connection, FrankenError> {
        Ok(Connection {
            inner: drive(fsqlite::compat::open_with_flags(path, flags))?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_execute_query_roundtrip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("bridge.db");
        let conn =
            Connection::open(db.to_string_lossy().into_owned()).expect("open bridge database");
        conn.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT)")
            .expect("create table");
        let inserted = conn
            .execute_with_params(
                "INSERT INTO t (v) VALUES (?1)",
                &[SqliteValue::from("hello")],
            )
            .expect("insert row");
        assert_eq!(inserted, 1);
        let rows = conn.query("SELECT v FROM t").expect("query rows");
        assert_eq!(rows.len(), 1);
        let row = conn
            .query_row_with_params("SELECT v FROM t WHERE id = ?1", &[SqliteValue::from(1i64)])
            .expect("query row");
        assert_eq!(row.get(0).and_then(SqliteValue::as_text), Some("hello"));
        conn.close().expect("close");
    }

    #[test]
    fn prepared_statement_roundtrip() {
        let conn = Connection::open(":memory:").expect("open in-memory database");
        conn.execute("CREATE TABLE t (k TEXT)").expect("create");
        conn.execute_with_params("INSERT INTO t (k) VALUES (?1)", &[SqliteValue::from("a")])
            .expect("insert");
        let stmt = conn
            .prepare("SELECT count(*) FROM t WHERE k = ?1")
            .expect("prepare");
        let row = stmt
            .query_row_with_params(&[SqliteValue::from("a")])
            .expect("query");
        assert_eq!(row.get(0).and_then(SqliteValue::as_integer), Some(1));
    }

    #[test]
    fn reentrant_bridge_calls_build_fresh_runtime() {
        // A bridge call issued while another bridge call's runtime is checked
        // out must not panic or deadlock (the thread-local slot is empty, so
        // a fresh runtime is built).
        let conn = Connection::open(":memory:").expect("open in-memory database");
        conn.execute("CREATE TABLE t (k INTEGER)").expect("create");
        let nested = Connection::open(":memory:").expect("nested open");
        nested.execute("CREATE TABLE u (k INTEGER)").expect("nested create");
        drop(nested);
        conn.execute("INSERT INTO t (k) VALUES (1)").expect("insert");
        assert_eq!(conn.query("SELECT k FROM t").expect("query").len(), 1);
    }
}
