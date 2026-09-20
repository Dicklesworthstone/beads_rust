//! Synchronous facade over the async FrankenSQLite 0.3 engine API.
//!
//! fsqlite 0.2 made every engine entry point `async` with `!Send` futures
//! (the engine is `Rc<RefCell<..>>` internally; it was already `!Send` at
//! 0.1.x — only the call shape changed), and fsqlite 0.3 moved the runtime
//! family to asupersync 0.4.4. br's storage layer is fully synchronous, so
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

mod prepared;
mod retry;
pub(crate) mod wal_index;

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

/// Bounded retry for the engine's transient errors on one statement.
///
/// Replay requires one query/DML statement and unchanged autocommit state.
/// Batches can commit a prefix or change transaction identity without changing
/// that boolean; schema, transaction-control and maintenance SQL require caller-owned
/// recovery. The SQL proof is computed only after a retryable failure.
/// `BusyRecovery` is retried only while autocommit state is unchanged.
/// A failed statement that entered or left a transaction must reach its owner
/// rather than being replayed in a different transaction context.
/// `BusySnapshot` is first-committer-wins loss at commit; the engine
/// contract says "retry the whole transaction". When the connection was in
/// autocommit before the statement ran, the statement IS the whole
/// transaction, so retrying it here is exactly that contract. Inside an
/// explicit transaction the error is surfaced instead: only the caller can
/// re-run its transaction body.
fn retry_transient<T>(
    conn: &fsqlite::Connection,
    retry_safety: &retry::ReplaySafety<'_>,
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
                let is_autocommit = !conn.in_transaction();
                let retryable = was_autocommit == is_autocommit
                    && (matches!(error, FrankenError::BusyRecovery)
                        || (matches!(error, FrankenError::BusySnapshot { .. }) && was_autocommit));
                if !retryable || !retry_safety.allows_replay() || start.elapsed() >= RETRY_BUDGET {
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
        let retry_safety = retry::ReplaySafety::new($sql);
        let was_in_transaction = $conn.in_transaction();
        let first = retry_transient(&$conn, &retry_safety, || $attempt);
        let result = match first {
            Err(err)
                if schema_stale(&err)
                    && was_in_transaction == $conn.in_transaction()
                    && retry_safety.allows_replay() =>
            {
                // Recompilation is allowed only for one ordinary statement.
                // Never prepare a batch/PRAGMA as a "refresh": prepare itself
                // can execute maintenance, and a batch may have applied a prefix.
                match retry_transient(&$conn, &retry_safety, || drive($conn.prepare($sql))) {
                    Ok(refreshed) => {
                        drop(refreshed);
                        if was_in_transaction == $conn.in_transaction() {
                            retry_transient(&$conn, &retry_safety, || $attempt)
                        } else {
                            Err(err)
                        }
                    }
                    Err(refresh_error) => Err(refresh_error),
                }
            }
            other => other,
        };
        if matches!(&result, Err(FrankenError::BusyRecovery)) {
            wal_index::warn_if_poisoned($conn.path());
        }
        result
    }};
}

#[cfg(test)]
mod retry_tests;

/// Classify a standalone checkpoint, refusing mixed batches before execution.
/// The engine accepts multi-statement SQL on its execute path. Treating a batch
/// as an ordinary statement would bypass completion checking and could execute
/// later writes after an incomplete checkpoint. Query APIs retain raw status.
/// Ordinary statements avoid the extra parse; the engine parser distinguishes
/// real PRAGMAs from names occurring in comments, identifiers, or SQL values.
fn is_wal_checkpoint_pragma(sql: &str) -> Result<bool, FrankenError> {
    const NAME: &[u8] = b"wal_checkpoint";
    if !sql
        .as_bytes()
        .windows(NAME.len())
        .any(|window| window.eq_ignore_ascii_case(NAME))
    {
        return Ok(false);
    }
    let (statements, errors) = fsqlite_parser::Parser::from_sql(sql).parse_all();
    let has_checkpoint = statements.iter().any(|statement| {
        matches!(
            statement,
            fsqlite_ast::Statement::Pragma(pragma)
                if pragma.name.name.eq_ignore_ascii_case("wal_checkpoint")
        )
    });
    if has_checkpoint && (statements.len() != 1 || !errors.is_empty()) {
        return Err(FrankenError::Internal(
            "WAL checkpoint must be a standalone statement so completion can be verified; no statements were executed"
                .to_string(),
        ));
    }
    Ok(has_checkpoint)
}

/// A checkpoint issued through execute() must not silently discard an
/// incomplete result. Query APIs deliberately retain the raw SQLite status
/// tuple for callers that want to inspect partial progress themselves.
fn checkpoint_execute_result(
    result: Result<Vec<Row>, FrankenError>,
) -> Result<usize, FrankenError> {
    let rows = result?;
    completed_checkpoint_affected_rows(rows.iter().map(Row::values))
}

fn completed_checkpoint_affected_rows<'a>(
    rows: impl IntoIterator<Item = &'a [SqliteValue]>,
) -> Result<usize, FrankenError> {
    let mut rows = rows.into_iter();
    let Some(row) = rows.next() else {
        return Err(FrankenError::Internal(
            "WAL checkpoint returned no completion result".to_string(),
        ));
    };
    if rows.next().is_some() {
        return Err(FrankenError::Internal(
            "WAL checkpoint returned more than one completion result".to_string(),
        ));
    }
    let [
        SqliteValue::Integer(busy),
        SqliteValue::Integer(frames),
        SqliteValue::Integer(backfilled),
    ] = row
    else {
        return Err(FrankenError::Internal(
            "WAL checkpoint completion result must contain three integers".to_string(),
        ));
    };
    // (0, -1, -1) is the non-WAL sentinel. All other successful results
    // require equal nonnegative log/backfill counts and no busy indication.
    if *busy == 0 && *frames >= -1 && frames == backfilled {
        return Ok(0);
    }
    if (0..=1).contains(busy) && *frames >= 0 && (0..=*frames).contains(backfilled) {
        tracing::debug!(busy, frames, backfilled, "WAL checkpoint did not complete");
        // Do not turn contention or partial backfill into a corruption error:
        // callers must not respond by rebuilding from potentially stale JSONL.
        return Err(FrankenError::Busy);
    }
    Err(FrankenError::Internal(format!(
        "WAL checkpoint returned invalid completion values: busy={busy}, log={frames}, checkpointed={backfilled}"
    )))
}

// ---------------------------------------------------------------------------
// Connection
// ---------------------------------------------------------------------------

/// Synchronous wrapper over [`fsqlite::Connection`] with the pre-0.2
/// blocking method signatures.
pub struct Connection {
    inner: fsqlite::Connection,
    checkpoint_on_close: bool,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection")
            .field("path", &self.inner.path())
            .finish_non_exhaustive()
    }
}

impl Connection {
    /// Open an existing database only when its VFS handle matches the retained identity.
    ///
    /// The explicit recovery workflow rehearses this open on a private copy
    /// before using it on the live family. Before admitting #507's exact
    /// poisoned-index signature to the engine, quarantine only the derived
    /// index under independent engine and SQLite exclusion. Ordinary and
    /// read-only opens never take this filesystem-recovery path.
    ///
    /// Every handle returned here closes without checkpointing, including
    /// retries after quarantine or missing-index reconstruction. Recovery may
    /// rebuild caches, but it must not rewrite the protected main/WAL payload.
    pub fn open_existing_with_expected_identity(
        path: impl Into<String>,
        identity: fsqlite_vfs::FileIdentity,
    ) -> Result<Self, FrankenError> {
        let path = path.into();
        wal_index::quarantine_poisoned_index(&path, identity)?;
        let inner = drive(fsqlite::Connection::open_existing_with_expected_identity(
            path, identity,
        ))?;
        // #507: after an interruption the index may already be absent or
        // healthy, so quarantine is not a reliable signal for close policy.
        // Set the policy at construction, before any initialization SQL.
        Self::from_inner(inner, true, false)
    }

    /// Open (or create) a database at `path`.
    pub fn open(path: impl Into<String>) -> Result<Self, FrankenError> {
        let path = path.into();
        let inner = drive(fsqlite::Connection::open(path.clone())).inspect_err(|error| {
            if matches!(error, FrankenError::BusyRecovery) {
                wal_index::warn_if_poisoned(&path);
            }
        })?;
        Self::from_inner(inner, true, true)
    }

    fn from_inner(
        inner: fsqlite::Connection,
        serialized: bool,
        checkpoint_on_close: bool,
    ) -> Result<Self, FrankenError> {
        let connection = Self {
            inner,
            checkpoint_on_close,
        };
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
    /// Checkpoint PRAGMAs return zero only after their status proves completion;
    /// use query() to observe a partial checkpoint without treating it as failure.
    pub fn execute(&self, sql: &str) -> Result<usize, FrankenError> {
        if is_wal_checkpoint_pragma(sql)? {
            return checkpoint_execute_result(self.query(sql));
        }
        with_engine_retries!(self.inner, sql, drive(self.inner.execute(sql)))
    }

    /// Execute a single SQL statement with positional parameters.
    pub fn execute_with_params(
        &self,
        sql: &str,
        params: &[SqliteValue],
    ) -> Result<usize, FrankenError> {
        if is_wal_checkpoint_pragma(sql)? {
            return checkpoint_execute_result(self.query_with_params(sql, params));
        }
        with_engine_retries!(
            self.inner,
            sql,
            drive(self.inner.execute_with_params(sql, params))
        )
    }

    /// Query, returning all rows.
    pub fn query(&self, sql: &str) -> Result<Vec<Row>, FrankenError> {
        #[cfg(test)]
        if let Some(status) = checkpoint_fault::take(self.inner.path(), sql) {
            return self.query_with_params("SELECT ?1, ?2, ?3", &status.map(SqliteValue::Integer));
        }
        with_engine_retries!(self.inner, sql, drive(self.inner.query(sql)))
    }

    /// Query with positional parameters, returning all rows.
    pub fn query_with_params(
        &self,
        sql: &str,
        params: &[SqliteValue],
    ) -> Result<Vec<Row>, FrankenError> {
        #[cfg(test)]
        if let Some(status) = checkpoint_fault::take(self.inner.path(), sql) {
            return self.query_with_params("SELECT ?1, ?2, ?3", &status.map(SqliteValue::Integer));
        }
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
        // A checkpoint row is a receipt for one execution, not reusable data.
        // Do not hand it to engine prepare, which may process the PRAGMA here.
        // Retain validated SQL and issue it afresh on every subsequent call.
        let inner = if is_wal_checkpoint_pragma(sql)? {
            PreparedStatementInner::Checkpoint {
                connection: self,
                sql: sql.to_string(),
            }
        } else {
            PreparedStatementInner::Engine(prepared::EngineStatement::new(&self.inner, sql)?)
        };
        Ok(PreparedStatement { inner })
    }

    /// Last-inserted rowid on this connection.
    #[must_use]
    pub fn last_insert_rowid(&self) -> i64 {
        self.inner.last_insert_rowid()
    }

    /// Close the connection (rolls back any active transaction, then runs the
    /// final passive WAL checkpoint, except for cache-recovery handles).
    pub fn close(mut self) -> Result<(), FrankenError> {
        self.close_in_place()
    }

    /// Close in place, retaining the handle on error so callers can retry.
    pub fn close_in_place(&mut self) -> Result<(), FrankenError> {
        #[cfg(test)]
        if tests::FAIL_NEXT_CLOSE.with(|fault| fault.replace(false)) {
            return Err(FrankenError::BusyRecovery);
        }
        if self.checkpoint_on_close {
            drive(self.inner.close_in_place())
        } else {
            drive(self.inner.close_without_checkpoint_in_place())
        }
    }

    /// Close without checkpointing; the caller controls checkpoint admission.
    /// This decision is sticky even when close fails: a later close retry must
    /// not silently regain permission to checkpoint a live peer's WAL.
    pub fn close_without_checkpoint_in_place(&mut self) -> Result<(), FrankenError> {
        self.checkpoint_on_close = false;
        self.close_in_place()
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

/// Synchronous prepared statement. Checkpoints retain SQL rather than a
/// compiled result so every execution observes the current WAL generation.
pub struct PreparedStatement<'conn> {
    inner: PreparedStatementInner<'conn>,
}

// `clippy::large_enum_variant` wants the big variant boxed. Do not: the big
// variant is `Engine`, which carries the engine's own prepared statement and is
// what every ordinary `prepare()` in the storage layer produces. The small
// variant is `Checkpoint`, added by 57104d61 for the rare deferred
// `PRAGMA wal_checkpoint` path — a pointer plus a String. Boxing `Engine` would
// put a heap allocation on every prepared statement across the whole hot path
// so that a rarely-constructed variant looks tidier, which is a pessimisation,
// not a fix. This annotation changes no codegen. If the engine owner would
// rather box it, that is their call to make deliberately.
#[allow(clippy::large_enum_variant)]
enum PreparedStatementInner<'conn> {
    Engine(prepared::EngineStatement<'conn>),
    Checkpoint {
        connection: &'conn Connection,
        sql: String,
    },
}

impl PreparedStatement<'_> {
    /// Render the compiled program, or describe a deferred checkpoint without
    /// running it. Checkpoint diagnostics are not an engine completion receipt.
    #[must_use]
    pub fn explain(&self) -> String {
        match &self.inner {
            PreparedStatementInner::Engine(statement) => statement.explain(),
            PreparedStatementInner::Checkpoint { sql, .. } => {
                format!("Deferred WAL checkpoint (fresh execution on each call): {sql}")
            }
        }
    }

    /// Query, returning all rows. Checkpoints expose fresh raw status rows.
    pub fn query(&self) -> Result<Vec<Row>, FrankenError> {
        match &self.inner {
            PreparedStatementInner::Engine(statement) => statement.query(),
            PreparedStatementInner::Checkpoint { connection, sql } => connection.query(sql),
        }
    }

    /// Query with positional parameters, returning all rows.
    pub fn query_with_params(&self, params: &[SqliteValue]) -> Result<Vec<Row>, FrankenError> {
        match &self.inner {
            PreparedStatementInner::Engine(statement) => statement.query_with_params(params),
            PreparedStatementInner::Checkpoint { connection, sql } => {
                connection.query_with_params(sql, params)
            }
        }
    }

    /// Query, returning exactly one row.
    pub fn query_row(&self) -> Result<Row, FrankenError> {
        match &self.inner {
            PreparedStatementInner::Engine(statement) => statement.query_row(),
            PreparedStatementInner::Checkpoint { connection, sql } => connection.query_row(sql),
        }
    }

    /// Query with positional parameters, returning exactly one row.
    pub fn query_row_with_params(&self, params: &[SqliteValue]) -> Result<Row, FrankenError> {
        match &self.inner {
            PreparedStatementInner::Engine(statement) => statement.query_row_with_params(params),
            PreparedStatementInner::Checkpoint { connection, sql } => {
                connection.query_row_with_params(sql, params)
            }
        }
    }

    /// Execute, returning the affected row count. A checkpoint succeeds only
    /// when this invocation's fresh status proves completion.
    pub fn execute(&self) -> Result<usize, FrankenError> {
        match &self.inner {
            PreparedStatementInner::Engine(statement) => statement.execute(),
            PreparedStatementInner::Checkpoint { connection, sql } => {
                checkpoint_execute_result(connection.query(sql))
            }
        }
    }

    /// Execute with positional parameters, returning the affected row count.
    pub fn execute_with_params(&self, params: &[SqliteValue]) -> Result<usize, FrankenError> {
        match &self.inner {
            PreparedStatementInner::Engine(statement) => statement.execute_with_params(params),
            PreparedStatementInner::Checkpoint { connection, sql } => {
                checkpoint_execute_result(connection.query_with_params(sql, params))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// compat: rusqlite-style open flags, synchronous form
// ---------------------------------------------------------------------------

pub mod compat {
    use super::{Connection, FrankenError, drive, wal_index};

    pub use fsqlite::compat::OpenFlags;

    /// Open a database with rusqlite-style open flags (synchronous form of
    /// [`fsqlite::compat::open_with_flags`]).
    pub fn open_with_flags(path: &str, flags: OpenFlags) -> Result<Connection, FrankenError> {
        let serialized = flags.contains(OpenFlags::SQLITE_OPEN_READ_WRITE);
        let inner = drive(fsqlite::compat::open_with_flags(path, flags)).inspect_err(|error| {
            if matches!(error, FrankenError::BusyRecovery) {
                wal_index::warn_if_poisoned(path);
            }
        })?;
        Connection::from_inner(inner, serialized, true)
    }
}

/// Query-result faults exercise the real execute/checkpoint/compaction chain.
/// They are thread-local, path-bound, ordered, and absent from production.
#[cfg(test)]
pub(crate) mod checkpoint_fault {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    struct State {
        path: String,
        results: VecDeque<(&'static str, [i64; 3])>,
    }

    thread_local! {
        static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    }

    struct ClearOnDrop;

    impl Drop for ClearOnDrop {
        fn drop(&mut self) {
            STATE.with(|slot| {
                slot.borrow_mut().take();
            });
        }
    }

    pub(super) fn take(path: &str, sql: &str) -> Option<[i64; 3]> {
        STATE.with(|slot| {
            let mut slot = slot.borrow_mut();
            let state = slot.as_mut()?;
            let (expected_sql, status) = state.results.front()?;
            if state.path != path || *expected_sql != sql {
                return None;
            }
            let status = *status;
            state.results.pop_front();
            Some(status)
        })
    }

    // `pub`, not `pub(crate)`: the enclosing `checkpoint_fault` module is
    // `#[cfg(test)] pub(crate) mod` (line 552), so this stays crate-visible
    // through that module and does not exist at all in a non-test build.
    // `pub(crate)` here is what `clippy::redundant_pub_crate` rejects.
    pub fn with_results<T>(
        path: &str,
        results: Vec<(&'static str, [i64; 3])>,
        action: impl FnOnce() -> T,
    ) -> T {
        STATE.with(|slot| {
            let mut slot = slot.borrow_mut();
            assert!(slot.is_none(), "checkpoint fault scopes must not nest");
            *slot = Some(State {
                path: path.to_string(),
                results: results.into(),
            });
        });
        let clear = ClearOnDrop;
        let result = action();
        STATE.with(|slot| {
            let slot = slot.borrow();
            let remaining = &slot
                .as_ref()
                .expect("active checkpoint fault scope")
                .results;
            assert!(
                remaining.is_empty(),
                "checkpoint path skipped results: {remaining:?}"
            );
        });
        drop(clear);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        pub(super) static FAIL_NEXT_CLOSE: std::cell::Cell<bool> = const {
            std::cell::Cell::new(false)
        };
    }

    #[test]
    fn failed_no_checkpoint_close_keeps_its_policy_on_retry() {
        if run_recovery_test_in_subprocess(
            "franken_sync::tests::failed_no_checkpoint_close_keeps_its_policy_on_retry",
        ) {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("close-retry.db");
        let mut connection = Connection::open(path.to_string_lossy().into_owned()).unwrap();
        connection.execute("PRAGMA journal_mode = WAL").unwrap();
        connection.execute("PRAGMA wal_autocheckpoint = 0").unwrap();
        connection.execute("CREATE TABLE t (value TEXT)").unwrap();
        connection
            .execute("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        connection
            .execute("INSERT INTO t VALUES ('WAL-only close retry')")
            .unwrap();
        let wal_path = temp.path().join("close-retry.db-wal");
        let main_before = std::fs::read(&path).unwrap();
        let wal_before = std::fs::read(&wal_path).unwrap();
        assert!(wal_before.len() > 32);

        FAIL_NEXT_CLOSE.with(|fault| fault.set(true));
        assert!(matches!(
            connection.close_without_checkpoint_in_place(),
            Err(FrankenError::BusyRecovery)
        ));
        assert!(!connection.checkpoint_on_close);
        // A caller may use ordinary close() to retry. It must not recover the
        // checkpoint authority explicitly declined by the first close call.
        connection.close().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), main_before);
        assert_eq!(std::fs::read(&wal_path).unwrap(), wal_before);
    }

    pub(super) fn run_recovery_test_in_subprocess(name: &str) -> bool {
        // As in #506, do not create a fixture whose leases can be inherited
        // by unrelated tests' children in the parallel parent process.
        const CHILD_ENV: &str = "BR_TEST_ISOLATED_RECOVERY_CLOSE";
        if std::env::var(CHILD_ENV).as_deref() == Ok(name) {
            return false;
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                name,
                "--test-threads=1",
                "--format=pretty",
                "--color=never",
            ])
            .env(CHILD_ENV, name)
            .env_remove("RUST_TEST_NOCAPTURE")
            .output()
            .expect("run isolated recovery close test");
        let stdout = String::from_utf8_lossy(&output.stdout);
        let passed = format!("test {name} ... ok");
        assert!(
            output.status.success() && stdout.lines().any(|line| line == passed),
            "isolated recovery test failed or did not run: {}\n{stdout}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        true
    }

    #[test]
    fn engine_recovery_preserves_wal_across_repeated_closes() {
        if run_recovery_test_in_subprocess(
            "franken_sync::tests::engine_recovery_preserves_wal_across_repeated_closes",
        ) {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("recovery.db");
        let path = db.to_string_lossy().into_owned();
        let wal = temp.path().join("recovery.db-wal");
        let shm = temp.path().join("recovery.db-shm");
        let sentinel = "recovery must preserve this committed WAL-only row";
        let mut writer = Connection::open(path.clone()).unwrap();
        assert!(
            writer.checkpoint_on_close,
            "ordinary open keeps its close policy"
        );
        writer.execute("PRAGMA journal_mode = WAL").unwrap();
        writer.execute("PRAGMA wal_autocheckpoint = 0").unwrap();
        writer.execute("CREATE TABLE t (v TEXT)").unwrap();
        writer.execute("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
        writer
            .execute_with_params("INSERT INTO t VALUES (?1)", &[SqliteValue::from(sentinel)])
            .unwrap();
        writer.close_without_checkpoint_in_place().unwrap();
        drop(writer);
        let main_before = std::fs::read(&db).unwrap();
        let wal_before = std::fs::read(&wal).unwrap();
        assert!(wal_before.len() > 32);
        assert!(
            !main_before
                .windows(sentinel.len())
                .any(|b| b == sentinel.as_bytes())
        );
        assert!(
            wal_before
                .windows(sentinel.len())
                .any(|b| b == sentinel.as_bytes())
        );
        let retained = std::fs::File::open(&db).unwrap();
        let identity = fsqlite_vfs::FileIdentity::from_file(&retained)
            .unwrap()
            .unwrap();

        // Both states skip quarantine: an already rebuilt index and the
        // missing index left by interruption immediately after quarantine.
        // Repeat against the same committed WAL, not an empty no-op fixture.
        for close_mode in 0..3 {
            for missing_index in [false, true] {
                if missing_index {
                    std::fs::rename(
                        &shm,
                        temp.path().join(format!("retained-index-{close_mode}")),
                    )
                    .unwrap();
                }
                let mut recovered =
                    Connection::open_existing_with_expected_identity(path.clone(), identity)
                        .unwrap();
                let row = recovered.query_row("SELECT v FROM t").unwrap();
                assert_eq!(row.get(0).and_then(SqliteValue::as_text), Some(sentinel));
                assert_eq!(std::fs::read(&db).unwrap(), main_before);
                assert_eq!(std::fs::read(&wal).unwrap(), wal_before);
                match close_mode {
                    0 => recovered.close().unwrap(),
                    1 => {
                        recovered.close_in_place().unwrap();
                        drop(recovered);
                    }
                    _ => drop(recovered),
                }
                assert_eq!(std::fs::read(&db).unwrap(), main_before);
                assert_eq!(std::fs::read(&wal).unwrap(), wal_before);
            }
        }
    }

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
    fn engine_recovery_refuses_replaced_database_identity() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source.db");
        let conn = Connection::open(path.to_string_lossy().into_owned()).unwrap();
        conn.execute("CREATE TABLE original (value TEXT)").unwrap();
        conn.close().unwrap();
        let retained = std::fs::File::open(&path).unwrap();
        let identity = fsqlite_vfs::FileIdentity::from_file(&retained)
            .unwrap()
            .unwrap();
        std::fs::rename(&path, temp.path().join("retained-original.db")).unwrap();
        std::fs::write(&path, b"replacement must not be opened").unwrap();
        assert!(
            Connection::open_existing_with_expected_identity(
                path.to_string_lossy().into_owned(),
                identity,
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"replacement must not be opened"
        );
        assert!(retained.metadata().unwrap().len() > 0);
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

    #[test]
    fn checkpoint_detection_uses_sql_structure() {
        for sql in [
            "PRAGMA wal_checkpoint(TRUNCATE)",
            "pragma main.wal_checkpoint = PASSIVE;",
            "/* maintenance */ PRAGMA \"main\".\"wal_checkpoint\"(FULL); -- done",
            "-- maintenance\nPRAGMA WAL_CHECKPOINT(RESTART)",
            ";; PRAGMA [wal_checkpoint];",
        ] {
            assert!(is_wal_checkpoint_pragma(sql).unwrap(), "{sql}");
        }
        for sql in [
            "SELECT 'PRAGMA wal_checkpoint(TRUNCATE)'",
            "SELECT 1 /* wal_checkpoint */",
            "PRAGMA wal_autocheckpoint = 0",
            "PRAGMA wal_checkpoint_extra",
            "PRAGMA wal_checkpoint(",
        ] {
            assert!(!is_wal_checkpoint_pragma(sql).unwrap(), "{sql}");
        }
    }

    #[test]
    fn checkpoint_batches_fail_before_any_statement_or_prepare_side_effect() {
        let conn = Connection::open(":memory:").unwrap();
        conn.execute("CREATE TABLE guarded (value TEXT)").unwrap();
        for sql in [
            "INSERT INTO guarded VALUES ('before'); PRAGMA wal_checkpoint(TRUNCATE)",
            "PRAGMA wal_checkpoint(PASSIVE); INSERT INTO guarded VALUES ('after')",
            "PRAGMA wal_checkpoint; PRAGMA wal_checkpoint",
            "SELECT 1; /* boundary */ PRAGMA main.\"WAL_CHECKPOINT\"(FULL)",
        ] {
            for error in [
                conn.execute(sql).expect_err("mixed execute must fail"),
                conn.execute_with_params(sql, &[])
                    .expect_err("mixed parameterized execute must fail"),
            ] {
                assert!(
                    matches!(error, FrankenError::Internal(ref detail) if detail.contains("standalone")),
                    "{sql}: {error:?}"
                );
            }
            assert!(conn.prepare(sql).is_err(), "mixed prepare must fail: {sql}");
            assert!(conn.query("SELECT value FROM guarded").unwrap().is_empty());
        }
        // Merely mentioning the PRAGMA in data must not prevent a real write.
        conn.execute("INSERT INTO guarded VALUES ('PRAGMA wal_checkpoint; SELECT 1')")
            .unwrap();
        assert_eq!(conn.query("SELECT value FROM guarded").unwrap().len(), 1);
    }

    #[test]
    fn checkpoint_execute_rejects_partial_query_rows_but_queries_keep_them() {
        let conn = Connection::open(":memory:").unwrap();
        let sql = "PRAGMA wal_checkpoint(TRUNCATE)";
        for status in [[1, 23, 0], [0, 23, 7], [1, 23, 23]] {
            checkpoint_fault::with_results(conn.inner.path(), vec![(sql, status); 3], || {
                assert!(matches!(conn.execute(sql), Err(FrankenError::Busy)));
                assert!(matches!(
                    conn.execute_with_params(sql, &[]),
                    Err(FrankenError::Busy)
                ));
                assert_eq!(
                    conn.query(sql).unwrap()[0].values(),
                    &status.map(SqliteValue::Integer)
                );
            });
        }
    }

    #[test]
    fn checkpoint_completion_requires_complete_wal_or_non_wal_status() {
        for busy in -1..=2 {
            for frames in -2..=7 {
                for backfilled in -2..=8 {
                    let values = [busy, frames, backfilled].map(SqliteValue::Integer);
                    let result = completed_checkpoint_affected_rows([values.as_slice()]);
                    let complete = busy == 0 && frames >= -1 && frames == backfilled;
                    assert_eq!(result.is_ok(), complete, "{values:?}: {result:?}");
                    if complete {
                        assert_eq!(result.unwrap(), 0, "a PRAGMA affects no table rows");
                    }
                }
            }
        }
    }

    #[test]
    fn incomplete_checkpoint_is_contention_not_corruption() {
        for values in [[1, 23, 0], [1, 23, 23], [0, 23, 7]] {
            let values = values.map(SqliteValue::Integer);
            assert!(matches!(
                completed_checkpoint_affected_rows([values.as_slice()]),
                Err(FrankenError::Busy)
            ));
        }
    }

    #[test]
    fn checkpoint_completion_rejects_malformed_rows() {
        let malformed = [
            Vec::new(),
            vec![vec![SqliteValue::Integer(0); 3]; 2],
            vec![Vec::new()],
            vec![vec![SqliteValue::Integer(0); 2]],
            vec![vec![SqliteValue::Integer(0); 4]],
            vec![vec![SqliteValue::Null; 3]],
            vec![vec![
                SqliteValue::Integer(0),
                SqliteValue::from("0"),
                SqliteValue::Integer(0),
            ]],
        ];
        for rows in malformed {
            let result = completed_checkpoint_affected_rows(rows.iter().map(Vec::as_slice));
            assert!(matches!(result, Err(FrankenError::Internal(_))), "{rows:?}");
        }
    }

    #[test]
    fn checkpoint_execution_preserves_engine_certificate_error() {
        let detail = "parallel WAL certificate suffix does not start at a record boundary";
        let error = checkpoint_execute_result(Err(FrankenError::WalCorrupt {
            detail: detail.to_string(),
        }))
        .expect_err("the engine error must not become a successful checkpoint");
        assert!(matches!(
            error,
            FrankenError::WalCorrupt { detail: actual } if actual == detail
        ));
    }

    #[test]
    fn checkpoint_execute_and_prepared_paths_keep_query_status_available() {
        let conn = Connection::open(":memory:").expect("open non-WAL database");
        let sql = "PRAGMA wal_checkpoint(TRUNCATE)";
        assert_eq!(conn.execute(sql).unwrap(), 0);
        assert_eq!(conn.execute_with_params(sql, &[]).unwrap(), 0);
        let rows = conn.query(sql).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].values(), &[0, -1, -1].map(SqliteValue::Integer));
        let statement = conn.prepare(sql).unwrap();
        assert!(matches!(
            &statement.inner,
            PreparedStatementInner::Checkpoint { .. }
        ));
        assert_eq!(statement.execute().unwrap(), 0);
        assert_eq!(statement.execute_with_params(&[]).unwrap(), 0);
        assert_eq!(statement.query().unwrap()[0].values(), rows[0].values());
        assert_eq!(statement.query_row().unwrap().values(), rows[0].values());
        assert_eq!(
            statement.query_row_with_params(&[]).unwrap().values(),
            rows[0].values()
        );
        assert!(matches!(
            conn.prepare("SELECT 1").unwrap().inner,
            PreparedStatementInner::Engine(_)
        ));
    }

    #[test]
    fn prepared_checkpoint_completion_is_checked_on_every_execution() {
        let conn = Connection::open(":memory:").unwrap();
        let sql = "PRAGMA wal_checkpoint(TRUNCATE)";
        checkpoint_fault::with_results(
            conn.inner.path(),
            vec![
                (sql, [0, 9, 9]),
                (sql, [0, 10, 9]),
                (sql, [0, 10, 10]),
                (sql, [1, 10, 10]),
                (sql, [0, 11, 12]),
                (sql, [0, -1, -1]),
            ],
            || {
                let statement = conn.prepare(sql).unwrap();
                assert!(statement.explain().contains("Deferred WAL checkpoint"));
                assert_eq!(statement.execute().unwrap(), 0);
                assert!(matches!(
                    statement.execute_with_params(&[]),
                    Err(FrankenError::Busy)
                ));
                assert_eq!(statement.execute_with_params(&[]).unwrap(), 0);
                assert!(matches!(statement.execute(), Err(FrankenError::Busy)));
                assert!(matches!(
                    statement.execute_with_params(&[]),
                    Err(FrankenError::Internal(_))
                ));
                assert_eq!(statement.execute().unwrap(), 0);
            },
        );
    }

    #[test]
    fn prepared_checkpoint_queries_keep_fresh_partial_status() {
        let conn = Connection::open(":memory:").unwrap();
        let sql = "PRAGMA wal_checkpoint(PASSIVE)";
        let statement = conn.prepare(sql).unwrap();
        for status in [[0, 23, 7], [1, 23, 0], [1, 23, 23]] {
            checkpoint_fault::with_results(conn.inner.path(), vec![(sql, status); 4], || {
                assert_eq!(
                    statement.query().unwrap()[0].values(),
                    &status.map(SqliteValue::Integer)
                );
                assert_eq!(
                    statement.query_with_params(&[]).unwrap()[0].values(),
                    &status.map(SqliteValue::Integer)
                );
                assert!(matches!(statement.execute(), Err(FrankenError::Busy)));
                assert!(matches!(
                    statement.execute_with_params(&[]),
                    Err(FrankenError::Busy)
                ));
            });
        }
    }

    #[test]
    fn prepared_checkpoint_is_inert_until_executed_and_rechecks_new_writes() {
        if run_recovery_test_in_subprocess(
            "franken_sync::tests::prepared_checkpoint_is_inert_until_executed_and_rechecks_new_writes",
        ) {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        let db = temp.path().join("prepared.db");
        let wal = temp.path().join("prepared.db-wal");
        let conn = Connection::open(db.to_string_lossy().into_owned()).unwrap();
        conn.execute("PRAGMA journal_mode = WAL").unwrap();
        conn.execute("PRAGMA wal_autocheckpoint = 0").unwrap();
        conn.execute("CREATE TABLE t (value TEXT)").unwrap();
        conn.execute("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
        conn.execute("INSERT INTO t VALUES ('first uncheckpointed write')")
            .unwrap();
        let main_before = std::fs::read(&db).unwrap();
        let wal_before = std::fs::read(&wal).unwrap();
        assert!(
            wal_before.len() > 32,
            "the fixture must contain real WAL frames"
        );
        let statement = conn.prepare("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
        assert!(statement.explain().contains("Deferred WAL checkpoint"));
        assert_eq!(std::fs::read(&db).unwrap(), main_before);
        assert_eq!(std::fs::read(&wal).unwrap(), wal_before);
        assert_eq!(statement.execute().unwrap(), 0);
        let first_checkpoint = std::fs::read(&db).unwrap();
        assert_ne!(first_checkpoint, main_before);
        conn.execute("INSERT INTO t VALUES ('second uncheckpointed write')")
            .unwrap();
        assert_eq!(std::fs::read(&db).unwrap(), first_checkpoint);
        assert_eq!(statement.execute_with_params(&[]).unwrap(), 0);
        assert_ne!(std::fs::read(&db).unwrap(), first_checkpoint);
        assert_eq!(conn.query("SELECT value FROM t").unwrap().len(), 2);
        drop(statement);
        conn.close().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn prepared_checkpoint_rejects_new_certificate_corruption_on_every_call_path() {
        use std::os::unix::fs::MetadataExt;

        if run_recovery_test_in_subprocess(
            "franken_sync::tests::prepared_checkpoint_rejects_new_certificate_corruption_on_every_call_path",
        ) {
            return;
        }
        let temp = tempfile::tempdir().unwrap();
        for call in 0..6 {
            let db = temp.path().join(format!("prepared-cert-{call}.db"));
            let mut conn = Connection::open(db.to_string_lossy().into_owned()).unwrap();
            conn.execute("PRAGMA journal_mode = WAL").unwrap();
            conn.execute("PRAGMA wal_autocheckpoint = 0").unwrap();
            conn.execute("CREATE TABLE t (value TEXT)").unwrap();
            let statement = conn.prepare("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
            assert_eq!(statement.execute().unwrap(), 0);
            let certificate = db.with_file_name(format!("prepared-cert-{call}.db-wal-cert"));
            std::fs::write(&certificate, b"live-sidecar-must-not-change").unwrap();
            let witness = || {
                [
                    "",
                    "-wal",
                    "-wal-cert",
                    "-wal-cert-head",
                    ".fsqlite-migration-state",
                ]
                .map(|suffix| {
                    let mut path = db.as_os_str().to_os_string();
                    path.push(suffix);
                    match std::fs::symlink_metadata(&path) {
                        Ok(metadata) => {
                            assert!(metadata.is_file());
                            Some((
                                metadata.dev(),
                                metadata.ino(),
                                std::fs::read(&path).unwrap(),
                            ))
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                        Err(error) => panic!("inspect checkpoint payload: {error}"),
                    }
                })
            };
            let before = witness();
            let result = match call {
                0 => statement.execute().map(|_| ()),
                1 => statement.execute_with_params(&[]).map(|_| ()),
                2 => statement.query().map(|_| ()),
                3 => statement.query_with_params(&[]).map(|_| ()),
                4 => statement.query_row().map(|_| ()),
                _ => statement.query_row_with_params(&[]).map(|_| ()),
            };
            assert!(
                matches!(result, Err(FrankenError::WalCorrupt { ref detail })
                    if detail.contains("parallel WAL certificate suffix does not start at a record boundary")),
                "prepared checkpoint path {call} reused success or changed the error: {result:?}"
            );
            assert_eq!(witness(), before, "refusal must preserve payload evidence");
            drop(statement);
            conn.close_without_checkpoint_in_place().unwrap();
            drop(conn);
            assert_eq!(witness(), before, "close must preserve payload evidence");
        }
    }
}
