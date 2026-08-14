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
    // `SchemaChanged` is the engine's explicit stale-schema-cookie signal
    // (a plan compiled against a schema image another connection has since
    // replaced); the upstream error-taxonomy recipe for it is exactly the
    // re-prepare + single retry this facade already performs for the
    // name-resolution staleness shapes below.
    matches!(
        err,
        FrankenError::SchemaChanged
            | FrankenError::NoSuchTable { .. }
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
    // ubs:ignore — this monotonic clock bounds retries; it generates no token or randomness.
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

/// Bounded retry for the engine's transient errors on one statement.
///
/// `BusyRecovery` is always retried (see [`retry_busy_recovery`]).
/// `BusySnapshot` is first-committer-wins loss at commit; the engine
/// contract says "retry the whole transaction". When the connection was in
/// autocommit before the statement ran, the statement IS the whole
/// transaction, so retrying it here is exactly that contract. Inside an
/// explicit transaction the error is surfaced instead: only the caller can
/// re-run its transaction body.
fn retry_transient<T>(
    conn: &fsqlite::Connection,
    mut attempt: impl FnMut() -> Result<T, FrankenError>,
) -> Result<T, FrankenError> {
    const RETRY_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);
    const BACKOFF_CAP: std::time::Duration = std::time::Duration::from_millis(250);
    let was_autocommit = !conn.in_transaction();
    let start = std::time::Instant::now();
    let mut backoff = std::time::Duration::from_millis(5);
    loop {
        match attempt() {
            Err(error) => {
                let retryable = matches!(error, FrankenError::BusyRecovery)
                    || (matches!(error, FrankenError::BusySnapshot { .. })
                        && was_autocommit
                        && !conn.in_transaction());
                if !retryable || start.elapsed() >= RETRY_BUDGET {
                    return Err(error);
                }
                std::thread::sleep(backoff);
                backoff = (backoff * 2).min(BACKOFF_CAP);
            }
            ok => return ok,
        }
    }
}

macro_rules! with_engine_retries {
    ($conn:expr, $sql:expr, $attempt:expr) => {{
        let first = retry_transient(&$conn, || $attempt);
        match first {
            Err(ref err) if schema_stale(err) => {
                // `prepare` refreshes the schema image from the shared
                // publication plane even when it ultimately fails to resolve.
                let _ = drive($conn.prepare($sql));
                retry_transient(&$conn, || $attempt)
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
        let inner = drive(fsqlite::Connection::open(path))?;
        Self::from_inner(inner, true)
    }

    fn from_inner(inner: fsqlite::Connection, serialized: bool) -> Result<Self, FrankenError> {
        let connection = Self { inner };
        if !serialized {
            return Ok(connection);
        }
        // br already serializes mutations through its workspace write lock
        // and owns whole-transaction retries. Keep the engine on SQLite's
        // single-writer semantics so a schema rebuild cannot be rejected by
        // MVCC validation against its own INSERT..SELECT + DROP write set.
        connection.execute("PRAGMA fsqlite.concurrent_mode = OFF")?;
        Ok(connection)
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
        let serialized = flags.contains(OpenFlags::SQLITE_OPEN_READ_WRITE);
        let inner = drive(fsqlite::compat::open_with_flags(path, flags))?;
        Connection::from_inner(inner, serialized)
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
    fn string_in_list_predicates_match_equality_forms() {
        let conn = Connection::open(":memory:").expect("open in-memory database");
        conn.execute("CREATE TABLE dependencies (issue_id TEXT, depends_on_id TEXT, type TEXT)")
            .expect("create");
        for (a, b, t) in [
            ("i1", "i2", "blocks"),
            ("i2", "i1", "blocks"),
            ("i3", "i1", "related"),
            ("i4", "i1", "waits-for"),
        ] {
            conn.execute_with_params(
                "INSERT INTO dependencies (issue_id, depends_on_id, type) VALUES (?1, ?2, ?3)",
                &[
                    SqliteValue::from(a),
                    SqliteValue::from(b),
                    SqliteValue::from(t),
                ],
            )
            .expect("insert");
        }
        // The dependency-cycle graph loader depends on bare full-scan
        // string IN-list predicates returning exactly the equality union.
        let in_list = conn
            .query(
                "SELECT issue_id, depends_on_id FROM dependencies \
                 WHERE type IN ('blocks', 'conditional-blocks', 'waits-for')",
            )
            .expect("in-list query");
        assert_eq!(in_list.len(), 3, "IN-list must match blocks + waits-for");
        let eq = conn
            .query("SELECT issue_id FROM dependencies WHERE type = 'blocks'")
            .expect("equality query");
        assert_eq!(eq.len(), 2, "equality predicate must see both blocks rows");
        let or_form = conn
            .query(
                "SELECT issue_id FROM dependencies \
                 WHERE type = 'blocks' OR type = 'conditional-blocks' OR type = 'waits-for'",
            )
            .expect("or query");
        assert_eq!(or_form.len(), 3, "OR form must agree with the IN form");
    }

    #[test]
    fn connections_default_to_serialized_engine_mode() {
        let conn = Connection::open(":memory:").expect("open in-memory database");
        let row = conn
            .query_row("PRAGMA fsqlite.concurrent_mode")
            .expect("query engine mode");
        assert_eq!(row.get(0).and_then(SqliteValue::as_integer), Some(0));
    }

    #[test]
    fn writable_compat_connections_use_serialized_engine_mode() {
        let dir = tempfile::tempdir().expect("tempdir");
        let db = dir.path().join("compat.db");
        let path = db.to_string_lossy().into_owned();
        let initial = Connection::open(path.clone()).expect("create compat database");
        initial.close().expect("close initial connection");

        let conn = compat::open_with_flags(&path, compat::OpenFlags::SQLITE_OPEN_READ_WRITE)
            .expect("open writable compat connection");
        let row = conn
            .query_row("PRAGMA fsqlite.concurrent_mode")
            .expect("query compat engine mode");
        assert_eq!(row.get(0).and_then(SqliteValue::as_integer), Some(0));
    }

    #[test]
    fn schema_changed_enters_the_stale_schema_retry_path() {
        assert!(schema_stale(&FrankenError::SchemaChanged));
    }

    #[test]
    fn reentrant_bridge_calls_build_fresh_runtime() {
        // A bridge call issued while another bridge call's runtime is checked
        // out must not panic or deadlock (the thread-local slot is empty, so
        // a fresh runtime is built).
        let row_count = drive(async {
            let conn = Connection::open(":memory:").expect("nested open");
            conn.execute("CREATE TABLE t (k INTEGER)")
                .expect("nested create");
            conn.execute("INSERT INTO t (k) VALUES (1)")
                .expect("nested insert");
            conn.query("SELECT k FROM t").expect("nested query").len()
        });

        assert_eq!(row_count, 1);
    }
}
