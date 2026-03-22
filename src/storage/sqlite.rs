//! `SQLite` storage implementation.

use crate::error::{BeadsError, Result};
use crate::format::{IssueDetails, IssueWithDependencyMetadata};
use crate::model::{Comment, DependencyType, Event, EventType, Issue, IssueType, Priority, Status};
use crate::storage::events::get_events;
use crate::storage::schema::CURRENT_SCHEMA_VERSION;
use crate::storage::schema::{
    apply_runtime_compatible_schema, apply_schema, runtime_schema_compatible,
};
use crate::util::id::parse_id;
use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use fsqlite::Connection;
use fsqlite_error::FrankenError;
use fsqlite_types::SqliteValue;
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Number of mutations between WAL checkpoint attempts.
const WAL_CHECKPOINT_INTERVAL: u32 = 50;
const DEFAULT_BUSY_TIMEOUT_MS: u64 = 30_000;
// `fsqlite` starts returning false PRIMARY KEY conflicts when we rewrite
// existing `export_hashes` rows with a single multi-values INSERT. Batch the
// DELETE side for efficiency, but re-insert one row at a time for correctness.
const EXPORT_HASH_CHUNK_SIZE: usize = 32;
// `fsqlite` can surface the same false primary-key conflict when an existing
// blocked-cache population is rewritten via a large multi-values INSERT. Keep
// the delete batched/full-table, but re-insert rows individually.
const BLOCKED_CACHE_DELETE_CHUNK_SIZE: usize = 400;
const DIRTY_ISSUE_CHUNK_SIZE: usize = 900;
const IMPORT_LABEL_CHUNK_SIZE: usize = 400;
const IMPORT_DEPENDENCY_CHUNK_SIZE: usize = 140;
const DEPENDENCY_TRAVERSAL_MAX_DEPTH: usize = 500;
const BLOCKED_CACHE_STATE_KEY: &str = "blocked_cache_state";
const BLOCKED_CACHE_STATE_STALE: &str = "stale";

/// SQLite-based storage backend.
#[derive(Debug)]
pub struct SqliteStorage {
    conn: Connection,
    /// Track mutations to trigger periodic WAL checkpoints.
    mutation_count: u32,
}

/// Context for a mutation operation, tracking side effects.
pub struct MutationContext {
    pub op_name: String,
    pub actor: String,
    pub events: Vec<Event>,
    pub dirty_ids: HashSet<String>,
    pub invalidate_blocked_cache: bool,
    /// When set, only these issue IDs (and their transitive parent-child
    /// descendants) need their blocked-cache entries recomputed.  If `None`
    /// while `invalidate_blocked_cache` is true, the entire cache is rebuilt.
    pub cache_affected_ids: Option<HashSet<String>>,
    /// When true, skip the eager post-commit cache refresh and rely on the
    /// lazy `ensure_blocked_cache_fresh` rebuild triggered by the next read.
    /// This reduces DB write-lock contention for high-frequency mutations like
    /// dep add/remove where the caller does not immediately read blocked state.
    pub defer_cache_refresh: bool,
    pub force_flush: bool,
}

#[derive(Debug, Clone)]
enum BlockedCacheRefreshPlan {
    Full,
    Incremental(HashSet<String>),
    /// The stale marker has been set inside the transaction; skip the eager
    /// post-commit rebuild and let `ensure_blocked_cache_fresh` handle it
    /// lazily on the next read.  Eliminates a second write transaction per
    /// dep add/remove, reducing DB lock contention under concurrent agents.
    Deferred,
}

impl BlockedCacheRefreshPlan {
    fn from_context(ctx: &MutationContext) -> Option<Self> {
        if !ctx.invalidate_blocked_cache {
            return None;
        }

        if ctx.defer_cache_refresh {
            return Some(Self::Deferred);
        }

        match &ctx.cache_affected_ids {
            Some(ids) if !ids.is_empty() => Some(Self::Incremental(ids.clone())),
            _ => Some(Self::Full),
        }
    }
}

#[derive(Clone, Copy)]
enum ReadyIssueProjection {
    Full,
    Command,
}

impl ReadyIssueProjection {
    const fn select_clause(self) -> &'static str {
        match self {
            Self::Full => {
                r"SELECT id, content_hash, title, description, design, acceptance_criteria, notes,
                         status, priority, issue_type, assignee, owner, estimated_minutes,
                         created_at, created_by, updated_at, closed_at, close_reason, closed_by_session,
                         due_at, defer_until, external_ref, source_system, source_repo,
                         deleted_at, deleted_by, delete_reason, original_type,
                         compaction_level, compacted_at, compacted_at_commit, original_size,
                         sender, ephemeral, pinned, is_template"
            }
            Self::Command => {
                r"SELECT id, title, description, acceptance_criteria, notes, status, priority,
                         issue_type, assignee, owner, estimated_minutes, created_at, created_by,
                         updated_at"
            }
        }
    }

    fn parse_row(self, row: &fsqlite::Row) -> Result<Issue> {
        match self {
            Self::Full => SqliteStorage::issue_from_row(row),
            Self::Command => SqliteStorage::ready_issue_from_row(row),
        }
    }
}

impl MutationContext {
    #[must_use]
    pub fn new(op_name: &str, actor: &str) -> Self {
        Self {
            op_name: op_name.to_string(),
            actor: actor.to_string(),
            events: Vec::new(),
            dirty_ids: HashSet::new(),
            invalidate_blocked_cache: false,
            cache_affected_ids: None,
            defer_cache_refresh: false,
            force_flush: false,
        }
    }

    pub fn record_event(&mut self, event_type: EventType, issue_id: &str, details: Option<String>) {
        self.events.push(Event {
            id: 0, // Placeholder, DB assigns auto-inc ID
            issue_id: issue_id.to_string(),
            event_type,
            actor: self.actor.clone(),
            old_value: None,
            new_value: None,
            comment: details,
            created_at: Utc::now(),
        });
    }

    /// Record a field change event with old and new values.
    pub fn record_field_change(
        &mut self,
        event_type: EventType,
        issue_id: &str,
        old_value: Option<String>,
        new_value: Option<String>,
        comment: Option<String>,
    ) {
        self.events.push(Event {
            id: 0,
            issue_id: issue_id.to_string(),
            event_type,
            actor: self.actor.clone(),
            old_value,
            new_value,
            comment,
            created_at: Utc::now(),
        });
    }

    pub fn mark_dirty(&mut self, issue_id: &str) {
        self.dirty_ids.insert(issue_id.to_string());
    }

    pub fn invalidate_cache(&mut self) {
        self.invalidate_blocked_cache = true;
        // Force full rebuild by clearing any incremental affected set.
        self.cache_affected_ids = None;
    }

    /// Signal that only specific issues need their blocked-cache entries
    /// recomputed (incremental update).  Merges with any previously recorded
    /// affected IDs.  If `invalidate_cache()` was already called (which sets
    /// `cache_affected_ids = None`), the full rebuild path takes precedence.
    pub fn invalidate_cache_for(&mut self, ids: &[&str]) {
        self.invalidate_blocked_cache = true;
        let set = self.cache_affected_ids.get_or_insert_with(HashSet::new);
        for id in ids {
            set.insert((*id).to_string());
        }
    }

    /// Mark the blocked-cache as needing invalidation but defer the actual
    /// rebuild to the next read that calls `ensure_blocked_cache_fresh`.
    ///
    /// Use this for high-frequency write operations (dep add/remove) where the
    /// caller does not immediately read blocked state and where eliminating the
    /// second post-commit write transaction reduces DB lock contention under
    /// concurrent agents.
    pub fn invalidate_cache_deferred(&mut self) {
        self.invalidate_blocked_cache = true;
        self.defer_cache_refresh = true;
    }
}

impl SqliteStorage {
    fn with_connection_write_transaction<F, R>(conn: &Connection, mut f: F) -> Result<R>
    where
        F: FnMut(&Connection) -> Result<R>,
    {
        const MAX_RETRIES: u32 = 5;
        let base_backoff_ms: u64 = 10;

        for attempt in 0..MAX_RETRIES {
            match conn.execute("BEGIN IMMEDIATE") {
                Ok(_) => {}
                Err(e) if e.is_transient() && attempt < MAX_RETRIES - 1 => {
                    let backoff = base_backoff_ms * 2u64.pow(attempt);
                    std::thread::sleep(Duration::from_millis(backoff));
                    continue;
                }
                Err(e) => return Err(e.into()),
            }

            match f(conn) {
                Ok(result) => match conn.execute("COMMIT") {
                    Ok(_) => return Ok(result),
                    Err(e) if e.is_transient() && attempt < MAX_RETRIES - 1 => {
                        if let Err(rb_err) = conn.execute("ROLLBACK") {
                            tracing::warn!(
                                error = %rb_err,
                                "ROLLBACK failed after transient COMMIT error"
                            );
                        }
                        let backoff = base_backoff_ms * 2u64.pow(attempt);
                        std::thread::sleep(Duration::from_millis(backoff));
                    }
                    Err(e) => {
                        if let Err(rb_err) = conn.execute("ROLLBACK") {
                            tracing::warn!(error = %rb_err, "ROLLBACK failed after COMMIT error");
                        }
                        return Err(e.into());
                    }
                },
                Err(e) => {
                    if let Err(rb_err) = conn.execute("ROLLBACK") {
                        tracing::warn!(error = %rb_err, "ROLLBACK failed after transaction error");
                    }
                    if e.is_transient() && attempt < MAX_RETRIES - 1 {
                        let backoff = base_backoff_ms * 2u64.pow(attempt);
                        std::thread::sleep(Duration::from_millis(backoff));
                    } else {
                        return Err(e);
                    }
                }
            }
        }

        unreachable!("Retry loop exited without returning")
    }

    fn delete_metadata_key_in_tx(conn: &Connection, key: &str) -> Result<()> {
        conn.execute_with_params(
            "DELETE FROM metadata WHERE key = ?",
            &[SqliteValue::from(key)],
        )?;
        Ok(())
    }

    fn metadata_equals(conn: &Connection, key: &str, expected: &str) -> Result<bool> {
        match conn.query_row_with_params(
            "SELECT value FROM metadata WHERE key = ?",
            &[SqliteValue::from(key)],
        ) {
            Ok(row) => Ok(row.get(0).and_then(SqliteValue::as_text) == Some(expected)),
            Err(fsqlite_error::FrankenError::QueryReturnedNoRows) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn apply_blocked_cache_refresh_plan(
        conn: &Connection,
        plan: &BlockedCacheRefreshPlan,
    ) -> Result<usize> {
        match plan {
            BlockedCacheRefreshPlan::Full => Self::rebuild_blocked_cache_impl(conn),
            BlockedCacheRefreshPlan::Incremental(ids) => {
                Self::incremental_blocked_cache_update(conn, ids)
            }
            // Deferred plan is never applied eagerly; the stale marker already
            // set inside the write transaction will trigger a lazy rebuild.
            BlockedCacheRefreshPlan::Deferred => Ok(0),
        }
    }

    fn refresh_blocked_cache_after_commit(
        &self,
        op: &str,
        plan: &BlockedCacheRefreshPlan,
    ) -> Result<()> {
        Self::with_connection_write_transaction(&self.conn, |conn| {
            let refreshed = Self::apply_blocked_cache_refresh_plan(conn, plan)?;
            Self::delete_metadata_key_in_tx(conn, BLOCKED_CACHE_STATE_KEY)?;
            tracing::debug!(operation = op, refreshed, "Refreshed blocked issues cache");
            Ok(())
        })
    }

    pub(crate) fn blocked_cache_marked_stale(&self) -> Result<bool> {
        Self::metadata_equals(
            &self.conn,
            BLOCKED_CACHE_STATE_KEY,
            BLOCKED_CACHE_STATE_STALE,
        )
    }

    /// Mark the blocked-cache as stale so a future read can rebuild it on demand.
    ///
    /// # Errors
    ///
    /// Returns an error if the metadata update fails.
    pub(crate) fn mark_blocked_cache_stale(&mut self) -> Result<()> {
        self.set_metadata(BLOCKED_CACHE_STATE_KEY, BLOCKED_CACHE_STATE_STALE)
    }

    pub(crate) fn ensure_blocked_cache_fresh(&self) -> Result<bool> {
        if !self.blocked_cache_marked_stale()? {
            return Ok(false);
        }

        Self::with_connection_write_transaction(&self.conn, |conn| {
            if !Self::metadata_equals(conn, BLOCKED_CACHE_STATE_KEY, BLOCKED_CACHE_STATE_STALE)? {
                return Ok(false);
            }

            let refreshed = Self::rebuild_blocked_cache_impl(conn)?;
            Self::delete_metadata_key_in_tx(conn, BLOCKED_CACHE_STATE_KEY)?;
            tracing::debug!(refreshed, "Rebuilt stale blocked issues cache on demand");
            Ok(true)
        })
    }

    /// Open a new connection to the database at the given path.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection cannot be established or schema application fails.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_timeout(path, Some(DEFAULT_BUSY_TIMEOUT_MS))
    }

    /// Open a new connection with an optional busy timeout (ms).
    ///
    /// # Errors
    ///
    /// Returns an error if the connection cannot be established or schema application fails.
    pub fn open_with_timeout(path: &Path, lock_timeout_ms: Option<u64>) -> Result<Self> {
        let conn = Connection::open(path.to_string_lossy().into_owned())?;

        // Configure busy_timeout so that BEGIN IMMEDIATE retries instead of
        // failing instantly when another writer holds the lock (issue #109).
        if let Some(timeout_ms) = lock_timeout_ms {
            conn.execute(&format!("PRAGMA busy_timeout={timeout_ms}"))?;
        }

        if database_header_user_version(path)
            .is_some_and(|version| version >= u32::try_from(CURRENT_SCHEMA_VERSION).unwrap_or(0))
        {
            crate::storage::schema::apply_runtime_pragmas(&conn)?;
        } else if runtime_schema_compatible(&conn) {
            apply_runtime_compatible_schema(&conn)?;
        } else {
            apply_schema(&conn)?;
        }
        Ok(Self {
            conn,
            mutation_count: 0,
        })
    }

    /// Open an in-memory database for testing.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection cannot be established.
    pub fn open_memory() -> Result<Self> {
        static MEM_COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let count = MEM_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("beads_mem_{}_{}.db", std::process::id(), count));
        let conn = Connection::open(path.to_string_lossy().into_owned())?;

        conn.execute(&format!("PRAGMA busy_timeout={DEFAULT_BUSY_TIMEOUT_MS}"))?;
        if let Err(e) = apply_schema(&conn) {
            eprintln!("apply_schema failed: {:?}", e);
            return Err(e);
        }
        Ok(Self {
            conn,
            mutation_count: 0,
        })
    }

    /// Drop and recreate all data tables, preserving `config` and `metadata`.
    ///
    /// Used before force imports to avoid fsqlite btree cursor bugs on DELETE
    /// operations in large tables. By starting with empty tables, the import
    /// only performs INSERTs.
    ///
    /// # Errors
    ///
    /// Returns an error if any DROP/CREATE statement fails.
    pub fn reset_data_tables(&mut self) -> Result<()> {
        use crate::storage::schema::execute_batch;
        execute_batch(
            &self.conn,
            r"
            DROP TABLE IF EXISTS blocked_issues_cache;
            DROP TABLE IF EXISTS export_hashes;
            DROP TABLE IF EXISTS dirty_issues;
            DROP TABLE IF EXISTS child_counters;
            DROP TABLE IF EXISTS events;
            DROP TABLE IF EXISTS comments;
            DROP TABLE IF EXISTS labels;
            DROP TABLE IF EXISTS dependencies;
            DROP TABLE IF EXISTS issues;
            ",
        )?;
        // Recreate with full schema (config/metadata already exist, IF NOT EXISTS is safe).
        // Use apply_runtime_compatible_schema rather than apply_schema because we are
        // mid-session: the connection is already open with correct pragmas and we only
        // need to restore the DDL without re-running heavier first-open migrations.
        apply_runtime_compatible_schema(&self.conn)?;
        Ok(())
    }

    /// Detect recoverable on-disk anomalies that should trigger JSONL rebuild.
    ///
    /// These checks run after the database opens successfully because some
    /// malformed states remain queryable enough to reach startup, then fail on
    /// the next single-row lookup.
    ///
    /// # Errors
    ///
    /// Returns an error if probing the database fails.
    pub(crate) fn detect_recoverable_open_anomaly(&self) -> Result<Option<String>> {
        let duplicate_schema_rows = self.conn.query(
            "SELECT type, name, COUNT(*) AS row_count
             FROM sqlite_master
             WHERE name IN ('blocked_issues_cache', 'idx_blocked_cache_blocked_at')
             GROUP BY type, name
             HAVING COUNT(*) > 1
             ORDER BY row_count DESC, name ASC
             LIMIT 1",
        )?;
        if let Some(row) = duplicate_schema_rows.first() {
            let object_type = row
                .get(0)
                .and_then(SqliteValue::as_text)
                .unwrap_or("object");
            let name = row
                .get(1)
                .and_then(SqliteValue::as_text)
                .unwrap_or("unknown");
            let row_count = row.get(2).and_then(SqliteValue::as_integer).unwrap_or(2);
            return Ok(Some(format!(
                "sqlite_master contains duplicate {object_type} entries for '{name}' ({row_count} rows)"
            )));
        }

        if let Some((key, row_count)) = self.first_duplicate_kv_key("config")? {
            return Ok(Some(format!(
                "config contains duplicate rows for key '{key}' ({row_count} rows)"
            )));
        }

        if let Some((key, row_count)) = self.first_duplicate_kv_key("metadata")? {
            return Ok(Some(format!(
                "metadata contains duplicate rows for key '{key}' ({row_count} rows)"
            )));
        }

        Ok(None)
    }

    fn first_duplicate_kv_key(&self, table: &str) -> Result<Option<(String, i64)>> {
        let sql = format!(
            "SELECT key, COUNT(*) AS row_count
             FROM {table}
             GROUP BY key
             HAVING COUNT(*) > 1
             ORDER BY row_count DESC, key ASC
             LIMIT 1"
        );
        let rows = self.conn.query(&sql)?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };

        let key = row
            .get(0)
            .and_then(SqliteValue::as_text)
            .unwrap_or("")
            .to_string();
        let row_count = row.get(1).and_then(SqliteValue::as_integer).unwrap_or(2);
        Ok(Some((key, row_count)))
    }

    /// Execute a raw SQL statement (no parameters, no result).
    ///
    /// Useful for PRAGMAs and DDL that don't fit the normal mutation flow.
    ///
    /// # Errors
    ///
    /// Returns an error if the statement fails.
    pub(crate) fn execute_raw(&self, sql: &str) -> Result<()> {
        self.conn.execute(sql)?;
        Ok(())
    }

    /// Execute a raw SQL query and return all result rows.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub(crate) fn execute_raw_query(&self, sql: &str) -> Result<Vec<Vec<SqliteValue>>> {
        let rows = self.conn.query(sql)?;
        Ok(rows.iter().map(|r| r.values().to_vec()).collect())
    }

    /// Check whether a foreign-key-backed table contains rows whose reference
    /// column points at a missing issue.
    ///
    /// Only whitelisted table/column pairs are accepted to prevent SQL injection
    /// through the string-interpolated query.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails or the table/column pair is not
    /// whitelisted.
    pub(crate) fn has_missing_issue_reference(&self, table: &str, column: &str) -> Result<bool> {
        const ALLOWED_PAIRS: &[(&str, &str)] = &[
            ("dependencies", "issue_id"),
            ("labels", "issue_id"),
            ("comments", "issue_id"),
            ("events", "issue_id"),
            ("dirty_issues", "issue_id"),
            ("export_hashes", "issue_id"),
            ("blocked_issues_cache", "issue_id"),
            ("child_counters", "parent_id"),
        ];
        if !ALLOWED_PAIRS.contains(&(table, column)) {
            return Err(crate::error::BeadsError::Config(format!(
                "has_missing_issue_reference: disallowed table/column pair ({table}, {column})"
            )));
        }
        let row = self.conn.query_row(&format!(
            "SELECT COUNT(*) FROM {table} WHERE {column} NOT IN (SELECT id FROM issues)"
        ))?;
        Ok(row.get(0).and_then(SqliteValue::as_integer).unwrap_or(0) > 0)
    }

    /// Execute a raw SQL statement and return the number of affected rows.
    ///
    /// # Errors
    ///
    /// Returns an error if the statement fails.
    pub(crate) fn execute_raw_count(&self, sql: &str) -> Result<usize> {
        let rows = self.conn.execute(sql)?;
        Ok(rows)
    }

    /// Probe whether a rollback-only write against an issue can safely touch
    /// the scheduling/status indexes used by update-style mutations.
    ///
    /// This is used to distinguish a genuine on-disk corruption problem from a
    /// higher-level application error after a write fails.
    ///
    /// # Errors
    ///
    /// Returns any database error raised while executing the probe.
    pub(crate) fn probe_issue_mutation_write_path(&self, issue_id: &str) -> Result<()> {
        self.conn.execute("BEGIN IMMEDIATE")?;

        let probe_result = self.conn.execute_with_params(
            "UPDATE issues SET priority = priority, status = status WHERE id = ?",
            &[SqliteValue::from(issue_id)],
        );
        let rollback_result = self.conn.execute("ROLLBACK");

        finish_issue_mutation_write_probe(probe_result, rollback_result)
    }

    /// Execute a closure inside a write transaction with robust retry logic
    /// for lock contention.
    ///
    /// Retries on all transient BUSY errors (from BEGIN, DML, or COMMIT) with
    /// exponential backoff.
    ///
    /// # Errors
    ///
    /// Returns an error if any step fails (e.g. database error, logic error).
    /// The transaction is rolled back on error.
    pub(crate) fn with_write_transaction<F, R>(&mut self, mut f: F) -> Result<R>
    where
        F: FnMut(&mut Self) -> Result<R>,
    {
        const MAX_RETRIES: u32 = 5;
        let base_backoff_ms: u64 = 10;

        for attempt in 0..MAX_RETRIES {
            match self.conn.execute("BEGIN IMMEDIATE") {
                Ok(_) => {}
                Err(e) if e.is_transient() && attempt < MAX_RETRIES - 1 => {
                    let backoff = base_backoff_ms * 2u64.pow(attempt);
                    std::thread::sleep(Duration::from_millis(backoff));
                    continue;
                }
                Err(e) => return Err(e.into()),
            }

            match f(self) {
                Ok(result) => {
                    match self.conn.execute("COMMIT") {
                        Ok(_) => {
                            // Periodic WAL checkpoint to prevent unbounded WAL growth
                            self.mutation_count += 1;
                            if self.mutation_count >= WAL_CHECKPOINT_INTERVAL {
                                self.mutation_count = 0;
                                self.try_wal_checkpoint();
                            }
                            return Ok(result);
                        }
                        Err(e) if e.is_transient() && attempt < MAX_RETRIES - 1 => {
                            if let Err(rb_err) = self.conn.execute("ROLLBACK") {
                                tracing::warn!(error = %rb_err, "ROLLBACK failed after transient COMMIT error");
                            }
                            let backoff = base_backoff_ms * 2u64.pow(attempt);
                            std::thread::sleep(Duration::from_millis(backoff));
                            // retry
                        }
                        Err(e) => {
                            if let Err(rb_err) = self.conn.execute("ROLLBACK") {
                                tracing::warn!(error = %rb_err, "ROLLBACK failed after COMMIT error");
                            }
                            return Err(e.into());
                        }
                    }
                }
                Err(e) => {
                    if let Err(rb_err) = self.conn.execute("ROLLBACK") {
                        tracing::warn!(error = %rb_err, "ROLLBACK failed after transaction error");
                    }
                    if e.is_transient() && attempt < MAX_RETRIES - 1 {
                        let backoff = base_backoff_ms * 2u64.pow(attempt);
                        std::thread::sleep(Duration::from_millis(backoff));
                        // retry
                    } else {
                        return Err(e);
                    }
                }
            }
        }
        unreachable!("Retry loop exited without returning")
    }

    /// Set export hashes using the caller's active transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub(crate) fn set_export_hashes_in_tx(&self, exports: &[(String, String)]) -> Result<usize> {
        let unique_exports = Self::dedupe_export_hash_batch(exports);
        if unique_exports.is_empty() {
            return Ok(0);
        }

        let now = Utc::now().to_rfc3339();
        let mut count = 0;

        for chunk in unique_exports.chunks(EXPORT_HASH_CHUNK_SIZE) {
            // Bulk delete existing hashes for this chunk
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            let sql = format!(
                "DELETE FROM export_hashes WHERE issue_id IN ({})",
                placeholders.join(",")
            );
            let params: Vec<SqliteValue> = chunk
                .iter()
                .map(|(id, _)| SqliteValue::from(id.as_str()))
                .collect();
            self.conn.execute_with_params(&sql, &params)?;

            // Plain INSERT after chunk DELETE — fsqlite does not reliably
            // enforce UNIQUE/PRIMARY KEY for OR REPLACE.
            for (issue_id, content_hash) in chunk {
                self.conn.execute_with_params(
                    "INSERT INTO export_hashes (issue_id, content_hash, exported_at) VALUES (?, ?, ?)",
                    &[
                        SqliteValue::from(issue_id.as_str()),
                        SqliteValue::from(content_hash.as_str()),
                        SqliteValue::from(now.as_str()),
                    ],
                )?;
                count += 1;
            }
        }

        Ok(count)
    }

    fn dedupe_export_hash_batch(exports: &[(String, String)]) -> Vec<(String, String)> {
        let mut deduped: Vec<(String, String)> = Vec::with_capacity(exports.len());
        let mut positions: HashMap<String, usize> = HashMap::with_capacity(exports.len());

        for (issue_id, content_hash) in exports {
            if let Some(position) = positions.get(issue_id).copied() {
                deduped[position].1.clone_from(content_hash);
            } else {
                positions.insert(issue_id.clone(), deduped.len());
                deduped.push((issue_id.clone(), content_hash.clone()));
            }
        }

        deduped
    }

    /// Clear export hashes using the caller's active transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub(crate) fn clear_export_hashes_in_tx(&self, issue_ids: &[String]) -> Result<usize> {
        if issue_ids.is_empty() {
            return Ok(0);
        }

        let mut total_deleted = 0;
        for chunk in issue_ids.chunks(EXPORT_HASH_CHUNK_SIZE) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            let sql = format!(
                "DELETE FROM export_hashes WHERE issue_id IN ({})",
                placeholders.join(",")
            );
            let params: Vec<SqliteValue> = chunk
                .iter()
                .map(|issue_id| SqliteValue::from(issue_id.as_str()))
                .collect();
            total_deleted += self.conn.execute_with_params(&sql, &params)?;
        }

        Ok(total_deleted)
    }

    /// Attempt a WAL checkpoint (TRUNCATE mode) to flush WAL back to the main
    /// database file. Errors are logged but do not propagate — checkpoint
    /// failure is non-fatal and will be retried on the next interval.
    fn try_wal_checkpoint(&self) {
        if let Err(e) = self.conn.execute("PRAGMA wal_checkpoint(TRUNCATE)") {
            tracing::debug!(error = %e, "WAL checkpoint failed (non-fatal, will retry later)");
        }
    }

    /// Get audit events for a specific issue.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_events(&self, issue_id: &str, limit: usize) -> Result<Vec<Event>> {
        crate::storage::events::get_events(&self.conn, issue_id, limit)
    }

    /// Get all audit events (for summary).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_all_events(&self, limit: usize) -> Result<Vec<Event>> {
        crate::storage::events::get_all_events(&self.conn, limit)
    }

    /// Execute a mutation with the 4-step transaction protocol.
    ///
    /// Retries on all transient BUSY errors (from BEGIN, DML, or COMMIT) with
    /// exponential backoff.  This is the fix for issue #109 — previously only
    /// `BusySnapshot` at COMMIT time was retried, while `Busy` from
    /// `BEGIN IMMEDIATE` (lock contention) would propagate immediately,
    /// causing concurrent close/update operations to silently lose data.
    ///
    /// # Errors
    ///
    /// Returns an error if any step fails (e.g. database error, logic error).
    /// The transaction is rolled back on error.
    #[allow(clippy::too_many_lines)]
    pub fn mutate<F, R>(&mut self, op: &str, actor: &str, mut f: F) -> Result<R>
    where
        F: FnMut(&Connection, &mut MutationContext) -> Result<R>,
    {
        let (result, blocked_cache_plan) = self.with_write_transaction(|storage| {
            let mut ctx = MutationContext::new(op, actor);
            let result = f(&storage.conn, &mut ctx)?;

            // Write events
            if !ctx.events.is_empty() {
                let sql = "INSERT INTO events (issue_id, event_type, actor, old_value, new_value, comment, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)";
                for event in &ctx.events {
                    let params = vec![
                        SqliteValue::from(event.issue_id.as_str()),
                        SqliteValue::from(event.event_type.as_str()),
                        SqliteValue::from(event.actor.as_str()),
                        event
                            .old_value
                            .as_deref()
                            .map_or(SqliteValue::Null, SqliteValue::from),
                        event
                            .new_value
                            .as_deref()
                            .map_or(SqliteValue::Null, SqliteValue::from),
                        event
                            .comment
                            .as_deref()
                            .map_or(SqliteValue::Null, SqliteValue::from),
                        SqliteValue::from(event.created_at.to_rfc3339()),
                    ];
                    storage.conn.execute_with_params(sql, &params)?;
                }
            }

            // Mark dirty
            if !ctx.dirty_ids.is_empty() {
                let now_str = Utc::now().to_rfc3339();
                // Collect IDs into a Vec for chunked processing
                let dirty_vec: Vec<_> = ctx.dirty_ids.iter().collect();

                for chunk in dirty_vec.chunks(DIRTY_ISSUE_CHUNK_SIZE) {
                    // DELETE + INSERT to simulate UPSERT (fsqlite does not
                    // reliably enforce UNIQUE/PRIMARY KEY for OR REPLACE).
                    for insert_chunk in chunk.chunks(450) {
                        // Delete existing rows first
                        let del_placeholders: Vec<&str> =
                            insert_chunk.iter().map(|_| "?").collect();
                        let delete_sql = format!(
                            "DELETE FROM dirty_issues WHERE issue_id IN ({})",
                            del_placeholders.join(", ")
                        );
                        let delete_params: Vec<SqliteValue> = insert_chunk
                            .iter()
                            .map(|id| SqliteValue::from(id.as_str()))
                            .collect();
                        storage.conn.execute_with_params(&delete_sql, &delete_params)?;

                        // Insert fresh rows
                        let placeholders: Vec<&str> =
                            insert_chunk.iter().map(|_| "(?, ?)").collect();
                        let insert_sql = format!(
                            "INSERT INTO dirty_issues (issue_id, marked_at) VALUES {}",
                            placeholders.join(", ")
                        );
                        let mut insert_params = Vec::with_capacity(insert_chunk.len() * 2);
                        for id in insert_chunk {
                            insert_params.push(SqliteValue::from(id.as_str()));
                            insert_params.push(SqliteValue::from(now_str.as_str()));
                        }
                        storage.conn.execute_with_params(&insert_sql, &insert_params)?;
                    }
                }
            }

            let blocked_cache_plan = BlockedCacheRefreshPlan::from_context(&ctx);
            if blocked_cache_plan.is_some() {
                storage.set_metadata_in_tx(BLOCKED_CACHE_STATE_KEY, BLOCKED_CACHE_STATE_STALE)?;
            }

            if ctx.force_flush {
                storage.conn.execute_with_params(
                    "DELETE FROM metadata WHERE key = 'needs_flush'",
                    &[],
                )?;
                storage.conn.execute_with_params(
                    "INSERT INTO metadata (key, value) VALUES ('needs_flush', 'true')",
                    &[],
                )?;
            }

            Ok((result, blocked_cache_plan))
        })?;

        match blocked_cache_plan {
            Some(BlockedCacheRefreshPlan::Deferred) => {
                // Stale marker already set inside the transaction.  The next
                // read that calls ensure_blocked_cache_fresh will rebuild.
                // Skipping the eager second write transaction here eliminates
                // the main source of DB lock contention for dep add/remove.
                tracing::debug!(
                    operation = op,
                    "Blocked cache refresh deferred; will rebuild lazily on next read"
                );
            }
            Some(ref plan) => {
                if let Err(error) = self.refresh_blocked_cache_after_commit(op, plan) {
                    tracing::warn!(
                        operation = op,
                        error = %error,
                        "Blocked cache refresh deferred after commit; cache remains marked stale"
                    );
                }
            }
            None => {}
        }

        Ok(result)
    }

    /// Create a new issue.
    ///
    /// # Errors
    ///
    /// Returns an error if the issue cannot be inserted (e.g. ID collision).
    #[allow(clippy::too_many_lines)]
    pub fn create_issue(&mut self, issue: &Issue, actor: &str) -> Result<()> {
        self.mutate("create_issue", actor, |conn, ctx| {
            // Explicit duplicate check since fsqlite does not enforce
            // UNIQUE constraints on non-rowid columns.
            let existing = conn.query_with_params(
                "SELECT 1 FROM issues WHERE id = ? LIMIT 1",
                &[SqliteValue::from(issue.id.as_str())],
            )?;
            if !existing.is_empty() {
                return Err(BeadsError::IdCollision {
                    id: issue.id.clone(),
                });
            }

            // Check for external_ref collision
            if let Some(ref ext_ref) = issue.external_ref {
                let existing_ext = conn.query_with_params(
                    "SELECT id FROM issues WHERE external_ref = ? LIMIT 1",
                    &[SqliteValue::from(ext_ref.as_str())],
                )?;
                if !existing_ext.is_empty() {
                    let other_id = existing_ext[0]
                        .get(0)
                        .and_then(SqliteValue::as_text)
                        .unwrap_or_default()
                        .to_string();
                    return Err(BeadsError::Config(format!(
                        "External reference '{ext_ref}' already exists on issue {other_id}"
                    )));
                }
            }

            let status_str = issue.status.as_str();
            let issue_type_str = issue.issue_type.as_str();
            let created_at_str = issue.created_at.to_rfc3339();
            let updated_at_str = issue.updated_at.to_rfc3339();
            let closed_at_str = issue.closed_at.map(|dt| dt.to_rfc3339());
            let due_at_str = issue.due_at.map(|dt| dt.to_rfc3339());
            let defer_until_str = issue.defer_until.map(|dt| dt.to_rfc3339());
            let deleted_at_str = issue.deleted_at.map(|dt| dt.to_rfc3339());
            let compacted_at_str = issue.compacted_at.map(|dt| dt.to_rfc3339());

            conn.execute_with_params(
                "INSERT INTO issues (
                    id, content_hash, title, description, design, acceptance_criteria, notes,
                    status, priority, issue_type, assignee, owner, estimated_minutes,
                    created_at, created_by, updated_at, closed_at, close_reason,
                    closed_by_session, due_at, defer_until, external_ref, source_system,
                    source_repo, deleted_at, deleted_by, delete_reason, original_type,
                    compaction_level, compacted_at, compacted_at_commit, original_size,
                    sender, ephemeral, pinned, is_template
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                &[
                    SqliteValue::from(issue.id.as_str()),
                    issue.content_hash.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                    SqliteValue::from(issue.title.as_str()),
                    SqliteValue::from(issue.description.as_deref().unwrap_or("")),
                    SqliteValue::from(issue.design.as_deref().unwrap_or("")),
                    SqliteValue::from(issue.acceptance_criteria.as_deref().unwrap_or("")),
                    SqliteValue::from(issue.notes.as_deref().unwrap_or("")),
                    SqliteValue::from(status_str),
                    SqliteValue::from(issue.priority.0),
                    SqliteValue::from(issue_type_str),
                    issue.assignee.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                    SqliteValue::from(issue.owner.as_deref().unwrap_or("")),
                    issue.estimated_minutes.map_or(SqliteValue::Null, SqliteValue::from),
                    SqliteValue::from(created_at_str.as_str()),
                    SqliteValue::from(issue.created_by.as_deref().unwrap_or("")),
                    SqliteValue::from(updated_at_str.as_str()),
                    closed_at_str.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                    SqliteValue::from(issue.close_reason.as_deref().unwrap_or("")),
                    SqliteValue::from(issue.closed_by_session.as_deref().unwrap_or("")),
                    due_at_str.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                    defer_until_str.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                    issue.external_ref.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                    SqliteValue::from(issue.source_system.as_deref().unwrap_or("")),
                    SqliteValue::from(issue.source_repo.as_deref().unwrap_or(".")),
                    deleted_at_str.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                    SqliteValue::from(issue.deleted_by.as_deref().unwrap_or("")),
                    SqliteValue::from(issue.delete_reason.as_deref().unwrap_or("")),
                    SqliteValue::from(issue.original_type.as_deref().unwrap_or("")),
                    SqliteValue::from(i64::from(issue.compaction_level.unwrap_or(0))),
                    compacted_at_str.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                    issue.compacted_at_commit.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                    SqliteValue::from(i64::from(issue.original_size.unwrap_or(0))),
                    SqliteValue::from(issue.sender.as_deref().unwrap_or("")),
                    SqliteValue::from(i64::from(i32::from(issue.ephemeral))),
                    SqliteValue::from(i64::from(i32::from(issue.pinned))),
                    SqliteValue::from(i64::from(i32::from(issue.is_template))),
                ],
            )?;

            // Update child counter if this is a hierarchical ID
            if let Ok(parsed) = parse_id(&issue.id)
                && !parsed.is_root()
                && let Some(parent) = parsed.parent()
                && let Some(&child_num) = parsed.child_path.last()
            {
                Self::update_child_counter_in_tx(conn, &parent, child_num)?;
            }

            // Insert Labels
            let mut seen_labels = HashSet::new();
            for label in &issue.labels {
                if !seen_labels.insert(label.as_str()) {
                    continue;
                }
                conn.execute_with_params(
                    "INSERT INTO labels (issue_id, label) VALUES (?, ?)",
                    &[SqliteValue::from(issue.id.as_str()), SqliteValue::from(label.as_str())],
                )?;
                ctx.record_event(
                    EventType::LabelAdded,
                    &issue.id,
                    Some(format!("Added label {label}")),
                );
            }

            // Insert Dependencies
            let mut seen_deps = HashSet::new();
            for dep in &issue.dependencies {
                if dep.depends_on_id == issue.id {
                    return Err(BeadsError::SelfDependency {
                        id: issue.id.clone(),
                    });
                }

                if !seen_deps.insert(dep.depends_on_id.as_str()) {
                    continue;
                }
                Self::ensure_dependency_target_exists_in_tx(conn, &dep.depends_on_id)?;
                // Check cycle if blocking
                if dep.dep_type.is_blocking()
                    && Self::check_cycle(conn, &issue.id, &dep.depends_on_id, true)?
                {
                    return Err(BeadsError::DependencyCycle {
                        path: format!(
                            "Adding dependency {} -> {} would create a cycle",
                            issue.id, dep.depends_on_id
                        ),
                    });
                }

                conn.execute_with_params(
                    "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by)
                     VALUES (?, ?, ?, ?, ?)",
                    &[
                        SqliteValue::from(issue.id.as_str()),
                        SqliteValue::from(dep.depends_on_id.as_str()),
                        SqliteValue::from(dep.dep_type.as_str()),
                        SqliteValue::from(dep.created_at.to_rfc3339()),
                        SqliteValue::from(dep.created_by.as_deref().unwrap_or(actor)),
                    ],
                )?;

                ctx.record_event(
                    EventType::DependencyAdded,
                    &issue.id,
                    Some(format!(
                        "Added dependency on {} ({})",
                        dep.depends_on_id, dep.dep_type
                    )),
                );
                ctx.invalidate_cache_for(&[issue.id.as_str(), dep.depends_on_id.as_str()]);
            }

            // Insert Comments
            for comment in &issue.comments {
                conn.execute_with_params(
                    "INSERT INTO comments (issue_id, author, text, created_at) VALUES (?, ?, ?, ?)",
                    &[
                        SqliteValue::from(issue.id.as_str()),
                        SqliteValue::from(comment.author.as_str()),
                        SqliteValue::from(comment.body.as_str()),
                        SqliteValue::from(comment.created_at.to_rfc3339()),
                    ],
                )?;
                ctx.record_event(
                    EventType::Commented,
                    &issue.id,
                    Some(comment.body.clone()),
                );
            }

            ctx.record_event(
                EventType::Created,
                &issue.id,
                Some(format!("Created issue: {}", issue.title)),
            );

            ctx.mark_dirty(&issue.id);

            Ok(())
        })
    }

    /// Iterative BFS cycle detection (replaces recursive CTE).
    ///
    /// Checks whether adding an edge `issue_id -> depends_on_id` would create
    /// a cycle.  This works by starting from `depends_on_id` and walking its
    /// transitive forward dependencies; if any reachable node equals `issue_id`,
    /// a cycle would be formed.
    ///
    /// Uses lazy per-node BFS: instead of bulk-loading the entire dependency
    /// graph into memory, each BFS step queries only the immediate neighbors
    /// of the current frontier node.  On sparse graphs this visits a tiny
    /// fraction of total edges, giving dramatic speedups for `dep add` on
    /// large repositories (e.g. 813-issue graphs).
    ///
    /// Two kinds of edges are followed in the blocker graph:
    /// 1. Standard deps (`issue_id -> depends_on_id`), filtered by type.
    /// 2. Parent-child edges reversed (`depends_on_id -> issue_id`), since a
    ///    parent finishing requires its children to finish first.
    ///
    /// A depth cap prevents pathological traversal on corrupted graphs.
    fn check_cycle(
        conn: &Connection,
        issue_id: &str,
        depends_on_id: &str,
        blocking_only: bool,
    ) -> Result<bool> {
        // Build the per-node neighbor query.  We UNION two directions:
        //   (a) standard deps: given node as issue_id, follow to depends_on_id
        //   (b) parent-child reversed: given node as depends_on_id (parent),
        //       follow to issue_id (child) -- because parent is blocked by child
        let neighbor_sql = if blocking_only {
            "SELECT depends_on_id FROM dependencies \
             WHERE issue_id = ? AND type IN ('blocks', 'conditional-blocks', 'waits-for') \
             UNION \
             SELECT issue_id FROM dependencies \
             WHERE depends_on_id = ? AND type = 'parent-child'"
                .to_string()
        } else {
            "SELECT depends_on_id FROM dependencies \
             WHERE issue_id = ? AND type != 'parent-child' \
             UNION \
             SELECT issue_id FROM dependencies \
             WHERE depends_on_id = ? AND type = 'parent-child'"
                .to_string()
        };

        let stmt = conn.prepare(&neighbor_sql)?;

        let mut visited = HashSet::new();
        // Level-synchronous BFS: process all nodes at one depth before moving
        // to the next, so we can enforce a depth cap cleanly.
        let mut frontier: Vec<String> = vec![depends_on_id.to_string()];
        visited.insert(depends_on_id.to_string());

        for _depth in 0..DEPENDENCY_TRAVERSAL_MAX_DEPTH {
            if frontier.is_empty() {
                break;
            }

            let mut next_frontier = Vec::new();

            for node in &frontier {
                let rows = stmt.query_with_params(&[
                    SqliteValue::from(node.as_str()),
                    SqliteValue::from(node.as_str()),
                ])?;

                for row in &rows {
                    if let Some(neighbor) = row.get(0).and_then(SqliteValue::as_text) {
                        if neighbor == issue_id {
                            return Ok(true); // Cycle detected -- early exit
                        }
                        if visited.insert(neighbor.to_string()) {
                            next_frontier.push(neighbor.to_string());
                        }
                    }
                }
            }

            frontier = next_frontier;
        }

        Ok(false)
    }

    /// Update an issue's fields.
    ///
    /// # Errors
    ///
    /// Returns an error if the issue doesn't exist or the update fails.
    #[allow(clippy::too_many_lines)]
    pub fn update_issue(&mut self, id: &str, updates: &IssueUpdate, actor: &str) -> Result<Issue> {
        if updates.is_empty() {
            return self
                .get_issue(id)?
                .ok_or_else(|| BeadsError::IssueNotFound { id: id.to_string() });
        }

        self.mutate("update_issue", actor, |conn, ctx| {
            let mut issue = Self::get_issue_from_conn(conn, id)?
                .ok_or_else(|| BeadsError::IssueNotFound { id: id.to_string() })?;

            // Atomic claim guard: check assignee INSIDE the CONCURRENT transaction
            // to prevent TOCTOU races where two agents both see "unassigned".
            if updates.expect_unassigned {
                let rows = conn.query_with_params(
                    "SELECT assignee FROM issues WHERE id = ?",
                    &[SqliteValue::from(id)],
                )?;
                let current_assignee: Option<String> = rows
                    .first()
                    .and_then(|row| row.get(0).and_then(SqliteValue::as_text).map(String::from));
                let trimmed = current_assignee
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let claim_actor = updates.claim_actor.as_deref().unwrap_or("");

                match trimmed {
                    None => { /* unassigned, proceed with claim */ }
                    Some(current) if !updates.claim_exclusive && current == claim_actor => {
                        /* same actor re-claim, idempotent */
                    }
                    Some(current) => {
                        return Err(BeadsError::validation(
                            "claim",
                            format!("issue {id} already assigned to {current}"),
                        ));
                    }
                }
            }

            let mut set_clauses: Vec<String> = vec![];
            let mut params: Vec<SqliteValue> = vec![];

            // Helper to add update
            let mut add_update = |field: &str, val: SqliteValue| {
                set_clauses.push(format!("{field} = ?"));
                params.push(val);
            };

            // Title
            if let Some(ref title) = updates.title {
                let old_title = issue.title.clone();
                issue.title.clone_from(title);
                add_update("title", SqliteValue::from(title.as_str()));
                ctx.record_field_change(
                    EventType::Updated,
                    id,
                    Some(old_title),
                    Some(title.clone()),
                    Some("Title changed".to_string()),
                );
            }

            // Simple text fields - use empty string instead of NULL for bd compatibility
            if let Some(ref val) = updates.description {
                issue.description.clone_from(val);
                add_update(
                    "description",
                    SqliteValue::from(val.as_deref().unwrap_or("")),
                );
            }
            if let Some(ref val) = updates.design {
                issue.design.clone_from(val);
                add_update("design", SqliteValue::from(val.as_deref().unwrap_or("")));
            }
            if let Some(ref val) = updates.acceptance_criteria {
                issue.acceptance_criteria.clone_from(val);
                add_update(
                    "acceptance_criteria",
                    SqliteValue::from(val.as_deref().unwrap_or("")),
                );
            }
            if let Some(ref val) = updates.notes {
                issue.notes.clone_from(val);
                add_update("notes", SqliteValue::from(val.as_deref().unwrap_or("")));
            }

            // Status
            if let Some(ref status) = updates.status {
                let old_status_obj = issue.status.clone();
                let old_status = old_status_obj.as_str().to_string();
                let was_terminal = old_status_obj.is_terminal();
                issue.status.clone_from(status);
                add_update("status", SqliteValue::from(status.as_str()));

                if status.as_str() != old_status {
                    ctx.record_field_change(
                        EventType::StatusChanged,
                        id,
                        Some(old_status),
                        Some(status.as_str().to_string()),
                        None,
                    );
                }

                // Record Closed event if status is now Closed and wasn't before
                if *status == Status::Closed {
                    if !was_terminal {
                        let reason = updates.close_reason.as_ref().and_then(Clone::clone);
                        ctx.record_event(EventType::Closed, id, reason);
                    }

                    // Auto-set closed_at if not provided
                    if updates.closed_at.is_none() && issue.closed_at.is_none() {
                        let now = Utc::now();
                        issue.closed_at = Some(now);
                        add_update("closed_at", SqliteValue::from(now.to_rfc3339()));
                    }
                } else if *status == Status::Tombstone {
                    let reason = updates.close_reason.as_ref().and_then(Clone::clone);

                    if !was_terminal {
                        ctx.record_event(EventType::Deleted, id, reason.clone());
                    }

                    let now = Utc::now();
                    issue.deleted_at = Some(now);
                    issue.deleted_by = Some(actor.to_string());
                    issue.delete_reason.clone_from(&reason);
                    add_update("deleted_at", SqliteValue::from(now.to_rfc3339()));
                    add_update("deleted_by", SqliteValue::from(actor));
                    // Always update delete_reason if we are setting to Tombstone,
                    // using close_reason as fallback if provided.
                    add_update(
                        "delete_reason",
                        SqliteValue::from(reason.as_deref().unwrap_or("")),
                    );
                } else {
                    if was_terminal && !status.is_terminal() {
                        ctx.record_event(EventType::Reopened, id, None);
                    }
                    if issue.closed_at.is_some() && updates.closed_at.is_none() {
                        // Reopening (or fixing state): Clear closed_at if it was set
                        issue.closed_at = None;
                        add_update("closed_at", SqliteValue::Null);
                    }
                    if issue.deleted_at.is_some() {
                        issue.deleted_at = None;
                        issue.deleted_by = None;
                        issue.delete_reason = None;
                        add_update("deleted_at", SqliteValue::Null);
                        add_update("deleted_by", SqliteValue::Null);
                        add_update("delete_reason", SqliteValue::Null);
                    }
                }

                if updates.skip_cache_rebuild {
                    ctx.invalidate_cache_deferred();
                } else {
                    ctx.invalidate_cache();
                }
            }

            // Priority
            if let Some(priority) = updates.priority {
                let old_priority = issue.priority.0;
                issue.priority = priority;
                add_update("priority", SqliteValue::from(i64::from(priority.0)));
                if priority.0 != old_priority {
                    ctx.record_field_change(
                        EventType::PriorityChanged,
                        id,
                        Some(old_priority.to_string()),
                        Some(priority.0.to_string()),
                        None,
                    );
                }
            }

            // Issue type
            if let Some(ref issue_type) = updates.issue_type {
                issue.issue_type.clone_from(issue_type);
                add_update("issue_type", SqliteValue::from(issue_type.as_str()));
            }

            // Assignee
            if let Some(ref assignee_opt) = updates.assignee {
                let old_assignee = issue.assignee.clone();
                issue.assignee.clone_from(assignee_opt);
                add_update(
                    "assignee",
                    assignee_opt
                        .as_deref()
                        .map_or(SqliteValue::Null, SqliteValue::from),
                );
                if old_assignee != *assignee_opt {
                    ctx.record_field_change(
                        EventType::AssigneeChanged,
                        id,
                        old_assignee,
                        assignee_opt.clone(),
                        None,
                    );
                }
            }

            // Simple Option fields - use empty string instead of NULL for bd compatibility
            if let Some(ref val) = updates.owner {
                issue.owner.clone_from(val);
                add_update("owner", SqliteValue::from(val.as_deref().unwrap_or("")));
            }
            if let Some(ref val) = updates.estimated_minutes {
                issue.estimated_minutes = *val;
                add_update(
                    "estimated_minutes",
                    val.map_or(SqliteValue::Null, |v| SqliteValue::from(i64::from(v))),
                );
            }
            if let Some(ref val) = updates.external_ref {
                // Explicit uniqueness check for fsqlite
                if let Some(ext_ref) = val {
                    let existing_ext = conn.query_with_params(
                        "SELECT id FROM issues WHERE external_ref = ? AND id != ? LIMIT 1",
                        &[SqliteValue::from(ext_ref.as_str()), SqliteValue::from(id)],
                    )?;
                    if !existing_ext.is_empty() {
                        let other_id = existing_ext[0]
                            .get(0)
                            .and_then(SqliteValue::as_text)
                            .unwrap_or_default()
                            .to_string();
                        return Err(BeadsError::Config(format!(
                            "External reference '{ext_ref}' already exists on issue {other_id}"
                        )));
                    }
                }

                issue.external_ref.clone_from(val);
                add_update(
                    "external_ref",
                    val.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                );
            }
            // Use empty string instead of NULL for bd compatibility
            if let Some(ref val) = updates.close_reason {
                issue.close_reason.clone_from(val);
                add_update(
                    "close_reason",
                    SqliteValue::from(val.as_deref().unwrap_or("")),
                );
            }
            if let Some(ref val) = updates.closed_by_session {
                issue.closed_by_session.clone_from(val);
                add_update(
                    "closed_by_session",
                    SqliteValue::from(val.as_deref().unwrap_or("")),
                );
            }

            // Tombstone fields
            if let Some(ref val) = updates.deleted_at {
                issue.deleted_at = *val;
                add_update(
                    "deleted_at",
                    val.map_or(SqliteValue::Null, |d| SqliteValue::from(d.to_rfc3339())),
                );
            }
            // Use empty string instead of NULL for bd compatibility
            if let Some(ref val) = updates.deleted_by {
                issue.deleted_by.clone_from(val);
                add_update(
                    "deleted_by",
                    SqliteValue::from(val.as_deref().unwrap_or("")),
                );
            }
            if let Some(ref val) = updates.delete_reason {
                issue.delete_reason.clone_from(val);
                add_update(
                    "delete_reason",
                    SqliteValue::from(val.as_deref().unwrap_or("")),
                );
            }

            // Date fields
            if let Some(ref val) = updates.due_at {
                issue.due_at = *val;
                add_update(
                    "due_at",
                    val.map_or(SqliteValue::Null, |d| SqliteValue::from(d.to_rfc3339())),
                );
            }
            if let Some(ref val) = updates.defer_until {
                issue.defer_until = *val;
                add_update(
                    "defer_until",
                    val.map_or(SqliteValue::Null, |d| SqliteValue::from(d.to_rfc3339())),
                );
            }
            if let Some(ref val) = updates.closed_at {
                issue.closed_at = *val;
                add_update(
                    "closed_at",
                    val.map_or(SqliteValue::Null, |d| SqliteValue::from(d.to_rfc3339())),
                );
            }

            // Always update updated_at
            set_clauses.push("updated_at = ?".to_string());
            params.push(SqliteValue::from(Utc::now().to_rfc3339()));

            // Update content hash
            let new_hash = issue.compute_content_hash();
            set_clauses.push("content_hash = ?".to_string());
            params.push(SqliteValue::from(new_hash));

            // Build and execute SQL. Claim operations use an additional
            // compare-and-set predicate so exactly one contender can win even
            // if two writers both observed the row as unassigned earlier.
            let mut where_clause = "id = ?".to_string();
            params.push(SqliteValue::from(id));
            if updates.expect_unassigned {
                where_clause.push_str(" AND (assignee IS NULL OR TRIM(assignee) = ''");
                if !updates.claim_exclusive
                    && let Some(claim_actor) = updates
                        .claim_actor
                        .as_deref()
                        .filter(|actor| !actor.is_empty())
                {
                    where_clause.push_str(" OR assignee = ?");
                    params.push(SqliteValue::from(claim_actor));
                }
                where_clause.push(')');
            }

            let sql = format!(
                "UPDATE issues SET {} WHERE {where_clause}",
                set_clauses.join(", ")
            );
            let updated_rows = conn.execute_with_params(&sql, &params)?;
            if updated_rows == 0 {
                if updates.expect_unassigned {
                    let rows = conn.query_with_params(
                        "SELECT assignee FROM issues WHERE id = ?",
                        &[SqliteValue::from(id)],
                    )?;
                    let current_assignee = rows
                        .first()
                        .and_then(|row| row.get(0).and_then(SqliteValue::as_text))
                        .map(str::trim)
                        .filter(|assignee| !assignee.is_empty())
                        .unwrap_or("<unknown>");
                    return Err(BeadsError::validation(
                        "claim",
                        format!("issue {id} already assigned to {current_assignee}"),
                    ));
                }

                return Err(BeadsError::IssueNotFound { id: id.to_string() });
            }

            ctx.mark_dirty(id);

            Ok(())
        })?;

        // Return updated issue
        self.get_issue(id)?
            .ok_or_else(|| BeadsError::IssueNotFound { id: id.to_string() })
    }

    /// Delete an issue by creating a tombstone.
    ///
    /// # Errors
    ///
    /// Returns an error if the issue doesn't exist or the update fails.
    pub fn delete_issue(
        &mut self,
        id: &str,
        actor: &str,
        reason: &str,
        deleted_at: Option<DateTime<Utc>>,
    ) -> Result<Issue> {
        let issue = self
            .get_issue(id)?
            .ok_or_else(|| BeadsError::IssueNotFound { id: id.to_string() })?;

        let was_terminal = issue.status.is_terminal();
        let original_type = issue.issue_type.as_str().to_string();
        let timestamp = deleted_at.unwrap_or_else(Utc::now);
        let mut tombstone_issue = issue;
        tombstone_issue.status = Status::Tombstone;
        let tombstone_hash = crate::util::content_hash(&tombstone_issue);

        self.mutate("delete_issue", actor, |conn, ctx| {
            conn.execute_with_params(
                "UPDATE issues SET
                    content_hash = ?,
                    status = 'tombstone',
                    deleted_at = ?,
                    deleted_by = ?,
                    delete_reason = ?,
                    original_type = ?,
                    updated_at = ?
                 WHERE id = ?",
                &[
                    SqliteValue::from(tombstone_hash.as_str()),
                    SqliteValue::from(timestamp.to_rfc3339()),
                    SqliteValue::from(actor),
                    SqliteValue::from(reason),
                    SqliteValue::from(original_type.as_str()),
                    SqliteValue::from(Utc::now().to_rfc3339()),
                    SqliteValue::from(id),
                ],
            )?;

            if !was_terminal {
                ctx.record_event(
                    EventType::Deleted,
                    id,
                    Some(format!("Deleted issue: {reason}")),
                );
            }
            ctx.mark_dirty(id);
            ctx.invalidate_cache();

            Ok(())
        })?;

        self.get_issue(id)?
            .ok_or_else(|| BeadsError::IssueNotFound { id: id.to_string() })
    }

    /// Physically remove an issue and all related data from the database.
    ///
    /// Unlike `delete_issue` (which creates a tombstone), this permanently
    /// removes the issue row plus its labels, dependencies, comments, and
    /// events so it will not appear in subsequent JSONL exports.
    ///
    /// # Errors
    ///
    /// Returns an error if the issue doesn't exist or a database operation fails.
    pub fn purge_issue(&mut self, id: &str, actor: &str) -> Result<()> {
        if self.get_issue(id)?.is_none() {
            return Err(BeadsError::IssueNotFound { id: id.to_string() });
        }

        self.mutate("purge_issue", actor, |conn, ctx| {
            conn.execute_with_params(
                "DELETE FROM comments WHERE issue_id = ?",
                &[SqliteValue::from(id)],
            )?;
            conn.execute_with_params(
                "DELETE FROM labels WHERE issue_id = ?",
                &[SqliteValue::from(id)],
            )?;
            conn.execute_with_params(
                "DELETE FROM dependencies WHERE issue_id = ?",
                &[SqliteValue::from(id)],
            )?;
            conn.execute_with_params(
                "DELETE FROM dependencies WHERE depends_on_id = ?",
                &[SqliteValue::from(id)],
            )?;
            conn.execute_with_params(
                "DELETE FROM events WHERE issue_id = ?",
                &[SqliteValue::from(id)],
            )?;
            conn.execute_with_params(
                "DELETE FROM dirty_issues WHERE issue_id = ?",
                &[SqliteValue::from(id)],
            )?;
            conn.execute_with_params(
                "DELETE FROM export_hashes WHERE issue_id = ?",
                &[SqliteValue::from(id)],
            )?;
            conn.execute_with_params(
                "DELETE FROM blocked_issues_cache WHERE issue_id = ?",
                &[SqliteValue::from(id)],
            )?;
            conn.execute_with_params(
                "DELETE FROM child_counters WHERE parent_id = ?",
                &[SqliteValue::from(id)],
            )?;
            conn.execute_with_params("DELETE FROM issues WHERE id = ?", &[SqliteValue::from(id)])?;

            ctx.invalidate_cache();
            ctx.force_flush = true;

            Ok(())
        })
    }

    /// Get an issue by ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_issue(&self, id: &str) -> Result<Option<Issue>> {
        Self::get_issue_from_conn(&self.conn, id)
    }

    /// Get metadata for all issues to optimize import collision detection.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_all_issues_metadata(&self) -> Result<Vec<IssueMetadata>> {
        let sql = "SELECT id, external_ref, content_hash, updated_at, status FROM issues";
        let rows = self.conn.query(sql)?;
        let mut metas = Vec::with_capacity(rows.len());
        for row in &rows {
            let id = row
                .get(0)
                .and_then(SqliteValue::as_text)
                .unwrap_or_default()
                .to_string();
            let external_ref = row
                .get(1)
                .and_then(SqliteValue::as_text)
                .map(str::to_string);
            let content_hash = row
                .get(2)
                .and_then(SqliteValue::as_text)
                .map(str::to_string);
            let updated_at = parse_datetime(
                row.get(3)
                    .and_then(SqliteValue::as_text)
                    .unwrap_or_default(),
            )?;
            let status = parse_status(row.get(4).and_then(SqliteValue::as_text));

            metas.push(IssueMetadata {
                id,
                external_ref,
                content_hash,
                updated_at,
                status,
            });
        }
        Ok(metas)
    }

    fn get_issue_from_conn(conn: &Connection, id: &str) -> Result<Option<Issue>> {
        let sql = r"
            SELECT id, content_hash, title, description, design, acceptance_criteria, notes,
                   status, priority, issue_type, assignee, owner, estimated_minutes,
                   created_at, created_by, updated_at, closed_at, close_reason, closed_by_session,
                   due_at, defer_until, external_ref, source_system, source_repo,
                   deleted_at, deleted_by, delete_reason, original_type,
                   compaction_level, compacted_at, compacted_at_commit, original_size,
                   sender, ephemeral, pinned, is_template
            FROM issues WHERE id = ?
        ";

        match conn.query_row_with_params(sql, &[SqliteValue::from(id)]) {
            Ok(row) => Ok(Some(Self::issue_from_row(&row)?)),
            Err(fsqlite_error::FrankenError::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get multiple issues by ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_issues_by_ids(&self, ids: &[String]) -> Result<Vec<Issue>> {
        const SQLITE_VAR_LIMIT: usize = 900;

        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut issues = Vec::new();

        for chunk in ids.chunks(SQLITE_VAR_LIMIT) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            let sql = format!(
                r"SELECT id, content_hash, title, description, design, acceptance_criteria, notes,
                         status, priority, issue_type, assignee, owner, estimated_minutes,
                         created_at, created_by, updated_at, closed_at, close_reason, closed_by_session,
                         due_at, defer_until, external_ref, source_system, source_repo,
                         deleted_at, deleted_by, delete_reason, original_type,
                         compaction_level, compacted_at, compacted_at_commit, original_size,
                         sender, ephemeral, pinned, is_template
                  FROM issues WHERE id IN ({})",
                placeholders.join(",")
            );

            let params: Vec<SqliteValue> = chunk
                .iter()
                .map(|s| SqliteValue::from(s.as_str()))
                .collect();

            let rows = self.conn.query_with_params(&sql, &params)?;
            for row in &rows {
                issues.push(Self::issue_from_row(row)?);
            }
        }

        Ok(issues)
    }

    /// List issues with optional filters.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    #[allow(clippy::too_many_lines)]
    pub fn list_issues(&self, filters: &ListFilters) -> Result<Vec<Issue>> {
        let mut sql = String::from(
            r"SELECT id, content_hash, title, description, design, acceptance_criteria, notes,
                     status, priority, issue_type, assignee, owner, estimated_minutes,
                     created_at, created_by, updated_at, closed_at, close_reason, closed_by_session,
                     due_at, defer_until, external_ref, source_system, source_repo,
                     deleted_at, deleted_by, delete_reason, original_type,
                     compaction_level, compacted_at, compacted_at_commit, original_size,
                     sender, ephemeral, pinned, is_template
              FROM issues
              WHERE 1=1",
        );

        let mut params: Vec<SqliteValue> = Vec::new();

        if let Some(ref labels) = filters.labels {
            for label in labels {
                sql.push_str(" AND EXISTS (SELECT 1 FROM labels WHERE labels.issue_id = issues.id AND labels.label = ?)");
                params.push(SqliteValue::from(label.as_str()));
            }
        }

        if let Some(ref labels_or) = filters.labels_or
            && !labels_or.is_empty()
        {
            let placeholders: Vec<String> = labels_or.iter().map(|_| "?".to_string()).collect();
            let _ = write!(
                sql,
                " AND EXISTS (SELECT 1 FROM labels WHERE labels.issue_id = issues.id AND labels.label IN ({}))",
                placeholders.join(",")
            );
            for label in labels_or {
                params.push(SqliteValue::from(label.as_str()));
            }
        }

        if let Some(ref statuses) = filters.statuses
            && !statuses.is_empty()
        {
            let placeholders: Vec<String> = statuses.iter().map(|_| "?".to_string()).collect();
            let _ = write!(sql, " AND status IN ({}) ", placeholders.join(","));
            for s in statuses {
                params.push(SqliteValue::from(s.as_str()));
            }
        }

        if let Some(ref types) = filters.types
            && !types.is_empty()
        {
            let placeholders: Vec<String> = types.iter().map(|_| "?".to_string()).collect();
            let _ = write!(sql, " AND issue_type IN ({}) ", placeholders.join(","));
            for t in types {
                params.push(SqliteValue::from(t.as_str()));
            }
        }

        if let Some(ref priorities) = filters.priorities
            && !priorities.is_empty()
        {
            let placeholders: Vec<String> = priorities.iter().map(|_| "?".to_string()).collect();
            let _ = write!(sql, " AND priority IN ({}) ", placeholders.join(","));
            for p in priorities {
                params.push(SqliteValue::from(i64::from(p.0)));
            }
        }

        if let Some(ref assignee) = filters.assignee {
            sql.push_str(" AND assignee = ?");
            params.push(SqliteValue::from(assignee.as_str()));
        }

        if filters.unassigned {
            sql.push_str(" AND (assignee IS NULL OR assignee = '')");
        }

        if !filters.include_closed {
            if filters.include_deferred {
                sql.push_str(" AND status NOT IN ('closed', 'tombstone')");
            } else {
                sql.push_str(" AND status NOT IN ('closed', 'tombstone', 'deferred')");
            }
        } else if filters.statuses.as_ref().is_none_or(Vec::is_empty) {
            // When including closed issues, still exclude tombstones (deleted issues) by default
            // unless specific statuses were requested.
            sql.push_str(" AND status != 'tombstone'");
        }

        if !filters.include_templates {
            sql.push_str(" AND (is_template = 0 OR is_template IS NULL)");
        }

        if let Some(ref title_contains) = filters.title_contains {
            sql.push_str(" AND title LIKE ? ESCAPE '\\'");
            let escaped = escape_like_pattern(title_contains);
            params.push(SqliteValue::from(format!("%{escaped}%")));
        }

        if let Some(ts) = filters.updated_before {
            sql.push_str(" AND updated_at <= ?");
            params.push(SqliteValue::from(ts.to_rfc3339()));
        }

        if let Some(ts) = filters.updated_after {
            sql.push_str(" AND updated_at >= ?");
            params.push(SqliteValue::from(ts.to_rfc3339()));
        }

        // Apply custom sort if provided
        if let Some(ref sort_field) = filters.sort {
            let order = if filters.reverse { "DESC" } else { "ASC" };
            // Simple validation to prevent injection (though params should handle it,
            // column names can't be parameterized)
            match sort_field.as_str() {
                "priority" => {
                    let secondary_order = if filters.reverse { "ASC" } else { "DESC" };
                    let _ = write!(
                        sql,
                        " ORDER BY priority {order}, created_at {secondary_order}"
                    );
                }
                "created_at" | "created" => {
                    let order = if filters.reverse { "ASC" } else { "DESC" };
                    let _ = write!(sql, " ORDER BY created_at {order}");
                }
                "updated_at" | "updated" => {
                    let order = if filters.reverse { "ASC" } else { "DESC" };
                    let _ = write!(sql, " ORDER BY updated_at {order}");
                }
                "title" => {
                    // Case-insensitive sort for title
                    let _ = write!(sql, " ORDER BY title COLLATE NOCASE {order}");
                }
                _ => {
                    // Default fallback
                    sql.push_str(" ORDER BY priority ASC, created_at DESC");
                }
            }
        } else if filters.reverse {
            sql.push_str(" ORDER BY priority DESC, created_at ASC");
        } else {
            sql.push_str(" ORDER BY priority ASC, created_at DESC");
        }

        if let Some(limit) = filters.limit
            && limit > 0
        {
            let _ = write!(sql, " LIMIT {limit}");
            if let Some(offset) = filters.offset
                && offset > 0
            {
                let _ = write!(sql, " OFFSET {offset}");
            }
        }

        let rows = self.conn.query_with_params(&sql, &params)?;
        let mut issues = Vec::with_capacity(rows.len());
        for row in &rows {
            issues.push(Self::issue_from_row(row)?);
        }

        Ok(issues)
    }

    /// Get lean issue rows for stats computation without hydrating large text fields.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn list_stats_issues(&self) -> Result<Vec<StatsIssueRow>> {
        let rows = self.conn.query(
            r"SELECT id, status, priority, issue_type, assignee, created_at, closed_at,
                     defer_until, ephemeral, pinned, is_template
              FROM issues",
        )?;
        let mut issues = Vec::with_capacity(rows.len());
        for row in &rows {
            issues.push(Self::stats_issue_from_row(row)?);
        }
        Ok(issues)
    }

    /// Count issues matching the given filters (no LIMIT/OFFSET applied).
    ///
    /// Runs a `SELECT COUNT(*)` using the same WHERE conditions as [`list_issues`],
    /// without ORDER BY, LIMIT, or OFFSET clauses. Used to compute pagination metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    #[allow(clippy::too_many_lines)]
    pub fn count_issues_with_filters(&self, filters: &ListFilters) -> Result<usize> {
        let mut sql = String::from("SELECT COUNT(*) FROM issues WHERE 1=1");

        let mut params: Vec<SqliteValue> = Vec::new();

        if let Some(ref labels) = filters.labels {
            for label in labels {
                sql.push_str(" AND EXISTS (SELECT 1 FROM labels WHERE labels.issue_id = issues.id AND labels.label = ?)");
                params.push(SqliteValue::from(label.as_str()));
            }
        }

        if let Some(ref labels_or) = filters.labels_or
            && !labels_or.is_empty()
        {
            let placeholders: Vec<String> = labels_or.iter().map(|_| "?".to_string()).collect();
            let _ = write!(
                sql,
                " AND EXISTS (SELECT 1 FROM labels WHERE labels.issue_id = issues.id AND labels.label IN ({}))",
                placeholders.join(",")
            );
            for label in labels_or {
                params.push(SqliteValue::from(label.as_str()));
            }
        }

        if let Some(ref statuses) = filters.statuses
            && !statuses.is_empty()
        {
            let placeholders: Vec<String> = statuses.iter().map(|_| "?".to_string()).collect();
            let _ = write!(sql, " AND status IN ({}) ", placeholders.join(","));
            for s in statuses {
                params.push(SqliteValue::from(s.as_str()));
            }
        }

        if let Some(ref types) = filters.types
            && !types.is_empty()
        {
            let placeholders: Vec<String> = types.iter().map(|_| "?".to_string()).collect();
            let _ = write!(sql, " AND issue_type IN ({}) ", placeholders.join(","));
            for t in types {
                params.push(SqliteValue::from(t.as_str()));
            }
        }

        if let Some(ref priorities) = filters.priorities
            && !priorities.is_empty()
        {
            let placeholders: Vec<String> = priorities.iter().map(|_| "?".to_string()).collect();
            let _ = write!(sql, " AND priority IN ({}) ", placeholders.join(","));
            for p in priorities {
                params.push(SqliteValue::from(i64::from(p.0)));
            }
        }

        if let Some(ref assignee) = filters.assignee {
            sql.push_str(" AND assignee = ?");
            params.push(SqliteValue::from(assignee.as_str()));
        }

        if filters.unassigned {
            sql.push_str(" AND (assignee IS NULL OR assignee = '')");
        }

        if !filters.include_closed {
            if filters.include_deferred {
                sql.push_str(" AND status NOT IN ('closed', 'tombstone')");
            } else {
                sql.push_str(" AND status NOT IN ('closed', 'tombstone', 'deferred')");
            }
        } else if filters.statuses.as_ref().is_none_or(Vec::is_empty) {
            sql.push_str(" AND status != 'tombstone'");
        }

        if !filters.include_templates {
            sql.push_str(" AND (is_template = 0 OR is_template IS NULL)");
        }

        if let Some(ref title_contains) = filters.title_contains {
            sql.push_str(" AND title LIKE ? ESCAPE '\\'");
            let escaped = escape_like_pattern(title_contains);
            params.push(SqliteValue::from(format!("%{escaped}%")));
        }

        if let Some(ts) = filters.updated_before {
            sql.push_str(" AND updated_at <= ?");
            params.push(SqliteValue::from(ts.to_rfc3339()));
        }

        if let Some(ts) = filters.updated_after {
            sql.push_str(" AND updated_at >= ?");
            params.push(SqliteValue::from(ts.to_rfc3339()));
        }

        let row = self.conn.query_row_with_params(&sql, &params)?;
        let count = row.get(0).and_then(SqliteValue::as_integer).unwrap_or(0);
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// Search issues by query with optional filters.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    #[allow(clippy::too_many_lines)]
    pub fn search_issues(&self, query: &str, filters: &ListFilters) -> Result<Vec<Issue>> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }

        let mut sql = String::from(
            r"SELECT id, content_hash, title, description, design, acceptance_criteria, notes,
                     status, priority, issue_type, assignee, owner, estimated_minutes,
                     created_at, created_by, updated_at, closed_at, close_reason, closed_by_session,
                     due_at, defer_until, external_ref, source_system, source_repo,
                     deleted_at, deleted_by, delete_reason, original_type,
                     compaction_level, compacted_at, compacted_at_commit, original_size,
                     sender, ephemeral, pinned, is_template
              FROM issues
              WHERE 1=1",
        );

        let mut params: Vec<SqliteValue> = Vec::new();

        sql.push_str(
            " AND (title LIKE ? ESCAPE '\\' OR description LIKE ? ESCAPE '\\' OR id LIKE ? ESCAPE '\\')",
        );
        let escaped = escape_like_pattern(trimmed);
        let pattern = format!("%{escaped}%");
        params.push(SqliteValue::from(pattern.as_str()));
        params.push(SqliteValue::from(pattern.as_str()));
        params.push(SqliteValue::from(pattern));

        if let Some(ref labels) = filters.labels {
            for label in labels {
                sql.push_str(" AND EXISTS (SELECT 1 FROM labels WHERE labels.issue_id = issues.id AND labels.label = ?)");
                params.push(SqliteValue::from(label.as_str()));
            }
        }

        if let Some(ref labels_or) = filters.labels_or
            && !labels_or.is_empty()
        {
            let placeholders: Vec<String> = labels_or.iter().map(|_| "?".to_string()).collect();
            let _ = write!(
                sql,
                " AND EXISTS (SELECT 1 FROM labels WHERE labels.issue_id = issues.id AND labels.label IN ({}))",
                placeholders.join(",")
            );
            for label in labels_or {
                params.push(SqliteValue::from(label.as_str()));
            }
        }

        if let Some(ref statuses) = filters.statuses
            && !statuses.is_empty()
        {
            let placeholders: Vec<String> = statuses.iter().map(|_| "?".to_string()).collect();
            let _ = write!(sql, " AND status IN ({})", placeholders.join(","));
            for s in statuses {
                params.push(SqliteValue::from(s.as_str()));
            }
        }

        if let Some(ref types) = filters.types
            && !types.is_empty()
        {
            let placeholders: Vec<String> = types.iter().map(|_| "?".to_string()).collect();
            let _ = write!(sql, " AND issue_type IN ({})", placeholders.join(","));
            for t in types {
                params.push(SqliteValue::from(t.as_str()));
            }
        }

        if let Some(ref priorities) = filters.priorities
            && !priorities.is_empty()
        {
            let placeholders: Vec<String> = priorities.iter().map(|_| "?".to_string()).collect();
            let _ = write!(sql, " AND priority IN ({})", placeholders.join(","));
            for p in priorities {
                params.push(SqliteValue::from(i64::from(p.0)));
            }
        }

        if let Some(ref assignee) = filters.assignee {
            sql.push_str(" AND assignee = ?");
            params.push(SqliteValue::from(assignee.as_str()));
        }

        if filters.unassigned {
            sql.push_str(" AND (assignee IS NULL OR assignee = '')");
        }

        if !filters.include_closed {
            if filters.include_deferred {
                sql.push_str(" AND status NOT IN ('closed', 'tombstone')");
            } else {
                sql.push_str(" AND status NOT IN ('closed', 'tombstone', 'deferred')");
            }
        } else if filters.statuses.as_ref().is_none_or(Vec::is_empty) {
            // When including closed issues, still exclude tombstones (deleted issues) by default
            // unless specific statuses were requested.
            sql.push_str(" AND status != 'tombstone'");
        }

        if !filters.include_templates {
            sql.push_str(" AND (is_template = 0 OR is_template IS NULL)");
        }

        if let Some(ref title_contains) = filters.title_contains {
            sql.push_str(" AND title LIKE ? ESCAPE '\\'");
            let escaped = escape_like_pattern(title_contains);
            params.push(SqliteValue::from(format!("%{escaped}%")));
        }

        if let Some(ts) = filters.updated_before {
            sql.push_str(" AND updated_at <= ?");
            params.push(SqliteValue::from(ts.to_rfc3339()));
        }

        if let Some(ts) = filters.updated_after {
            sql.push_str(" AND updated_at >= ?");
            params.push(SqliteValue::from(ts.to_rfc3339()));
        }

        if let Some(ref sort_field) = filters.sort {
            let order = if filters.reverse { "DESC" } else { "ASC" };
            match sort_field.as_str() {
                "priority" => {
                    let secondary_order = if filters.reverse { "ASC" } else { "DESC" };
                    let _ = write!(
                        sql,
                        " ORDER BY priority {order}, created_at {secondary_order}"
                    );
                }
                "created_at" | "created" => {
                    let order = if filters.reverse { "ASC" } else { "DESC" };
                    let _ = write!(sql, " ORDER BY created_at {order}");
                }
                "updated_at" | "updated" => {
                    let order = if filters.reverse { "ASC" } else { "DESC" };
                    let _ = write!(sql, " ORDER BY updated_at {order}");
                }
                "title" => {
                    let _ = write!(sql, " ORDER BY title COLLATE NOCASE {order}");
                }
                _ => {
                    sql.push_str(" ORDER BY priority ASC, created_at DESC");
                }
            }
        } else if filters.reverse {
            sql.push_str(" ORDER BY priority DESC, created_at ASC");
        } else {
            sql.push_str(" ORDER BY priority ASC, created_at DESC");
        }

        if let Some(limit) = filters.limit
            && limit > 0
        {
            let _ = write!(sql, " LIMIT {limit}");
        }

        let rows = self.conn.query_with_params(&sql, &params)?;
        let mut issues = Vec::with_capacity(rows.len());
        for row in &rows {
            issues.push(Self::issue_from_row(row)?);
        }

        Ok(issues)
    }

    /// Get ready issues (unblocked, not deferred, not pinned, not ephemeral).
    ///
    /// Ready definition:
    /// 1. Status is `open` by default, or `deferred` when `include_deferred` is set
    /// 2. NOT in `blocked_issues_cache`
    /// 3. `defer_until` is NULL or <= now (unless `include_deferred`)
    /// 4. `pinned = 0` (not pinned)
    /// 5. `ephemeral = 0` AND ID does not contain `-wisp-`
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    #[allow(clippy::too_many_lines)]
    pub fn get_ready_issues(
        &self,
        filters: &ReadyFilters,
        sort: ReadySortPolicy,
    ) -> Result<Vec<Issue>> {
        self.get_ready_issues_with_projection(filters, sort, ReadyIssueProjection::Full)
    }

    /// Get ready issues optimized for `ready` command rendering.
    ///
    /// Hydrates only the columns consumed by the command's JSON/TOON/text
    /// output, avoiding large overflow-page reads for fields that never reach
    /// the user-facing result.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_ready_issues_for_command_output(
        &self,
        filters: &ReadyFilters,
        sort: ReadySortPolicy,
    ) -> Result<Vec<Issue>> {
        self.get_ready_issues_with_projection(filters, sort, ReadyIssueProjection::Command)
    }

    fn get_ready_issues_with_projection(
        &self,
        filters: &ReadyFilters,
        sort: ReadySortPolicy,
        projection: ReadyIssueProjection,
    ) -> Result<Vec<Issue>> {
        match self.ensure_blocked_cache_fresh() {
            Ok(_) => match self
                .query_ready_issue_candidates_with_projection(filters, sort, true, true, projection)
            {
                Ok(issues) => Ok(issues),
                Err(error) => {
                    let blocked_ids = self.recover_blocked_ids("ready_issues_query", &error)?;
                    self.query_ready_issues_without_cache_with_projection(
                        filters,
                        sort,
                        &blocked_ids,
                        projection,
                    )
                }
            },
            Err(error) => {
                let blocked_ids = self.recover_blocked_ids("ready_issues_refresh", &error)?;
                self.query_ready_issues_without_cache_with_projection(
                    filters,
                    sort,
                    &blocked_ids,
                    projection,
                )
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn build_ready_issue_candidates_query(
        &self,
        filters: &ReadyFilters,
        sort: ReadySortPolicy,
        exclude_blocked_in_sql: bool,
        apply_limit: bool,
        projection: ReadyIssueProjection,
    ) -> Result<(String, Vec<SqliteValue>)> {
        let mut sql = String::from(projection.select_clause());
        sql.push_str(" FROM issues WHERE 1=1");
        let mut params: Vec<SqliteValue> = Vec::new();

        if !filters.labels_and.is_empty() {
            for label in &filters.labels_and {
                sql.push_str(" AND EXISTS (SELECT 1 FROM labels WHERE labels.issue_id = issues.id AND labels.label = ?)");
                params.push(SqliteValue::from(label.as_str()));
            }
        }

        if !filters.labels_or.is_empty() {
            let placeholders: Vec<String> =
                filters.labels_or.iter().map(|_| "?".to_string()).collect();
            let _ = write!(
                sql,
                " AND EXISTS (SELECT 1 FROM labels WHERE labels.issue_id = issues.id AND labels.label IN ({}))",
                placeholders.join(",")
            );
            for label in &filters.labels_or {
                params.push(SqliteValue::from(label.as_str()));
            }
        }

        // Ready condition 1: only `open` issues are "ready" (in_progress means
        // already claimed). Optionally include deferred issues when requested.
        if filters.include_deferred {
            sql.push_str(" AND status IN ('open', 'deferred')");
        } else {
            sql.push_str(" AND status = 'open'");
        }

        // Ready condition 2: blocked issues are filtered in SQL when the cache
        // is healthy; fallback callers filter them in Rust after directly
        // recomputing the blocker graph from dependencies.
        if exclude_blocked_in_sql {
            sql.push_str(" AND issues.id NOT IN (SELECT issue_id FROM blocked_issues_cache)");
        }

        // Ready condition 3: `defer_until` is NULL or <= now (unless `include_deferred`)
        if !filters.include_deferred {
            sql.push_str(" AND (defer_until IS NULL OR datetime(defer_until) <= datetime('now'))");
        }

        // Ready condition 4: not pinned. Legacy rows may still store NULL,
        // which the rest of the storage layer treats as false.
        sql.push_str(" AND (pinned = 0 OR pinned IS NULL)");

        // Ready condition 5: not ephemeral and not wisp. Legacy rows may
        // still store NULL, which should behave the same as false.
        sql.push_str(" AND (ephemeral = 0 OR ephemeral IS NULL)");
        sql.push_str(" AND id NOT LIKE '%-wisp-%'");

        // Exclude templates
        sql.push_str(" AND (is_template = 0 OR is_template IS NULL)");

        // Filter by types
        if let Some(ref types) = filters.types
            && !types.is_empty()
        {
            let placeholders: Vec<String> = types.iter().map(|_| "?".to_string()).collect();
            let _ = write!(sql, " AND issue_type IN ({}) ", placeholders.join(","));
            for t in types {
                params.push(SqliteValue::from(t.as_str()));
            }
        }

        // Filter by priorities
        if let Some(ref priorities) = filters.priorities
            && !priorities.is_empty()
        {
            let placeholders: Vec<String> = priorities.iter().map(|_| "?".to_string()).collect();
            let _ = write!(sql, " AND priority IN ({})", placeholders.join(","));
            for p in priorities {
                params.push(SqliteValue::from(i64::from(p.0)));
            }
        }

        // Filter by assignee
        if let Some(ref assignee) = filters.assignee {
            sql.push_str(" AND assignee = ?");
            params.push(SqliteValue::from(assignee.as_str()));
        }

        // Filter for unassigned
        if filters.unassigned {
            sql.push_str(" AND (assignee IS NULL OR assignee = '')");
        }

        // Filter by parent (--parent flag)
        if let Some(ref parent_id) = filters.parent {
            if filters.recursive {
                // Collect all descendants via Rust-side BFS instead of
                // WITH RECURSIVE (not yet supported in fsqlite subqueries).
                let descendant_ids = self.collect_descendant_ids(parent_id)?;
                if descendant_ids.is_empty() {
                    // No descendants — short-circuit to empty result.
                    sql.push_str(" AND 1 = 0");
                } else {
                    let mut chunks_sql = Vec::new();
                    for chunk in descendant_ids.chunks(900) {
                        let placeholders: Vec<String> =
                            chunk.iter().map(|_| "?".to_string()).collect();
                        chunks_sql.push(format!("id IN ({})", placeholders.join(",")));
                        for id in chunk {
                            params.push(SqliteValue::from(id.as_str()));
                        }
                    }
                    let _ = write!(sql, " AND ({})", chunks_sql.join(" OR "));
                }
            } else {
                sql.push_str(
                    " AND id IN (
                        SELECT issue_id FROM dependencies
                        WHERE depends_on_id = ? AND type = 'parent-child'
                    )",
                );
                params.push(SqliteValue::from(parent_id.as_str()));
            }
        }

        // Sorting
        match sort {
            ReadySortPolicy::Hybrid => {
                sql.push_str(" ORDER BY CASE WHEN priority <= 1 THEN 0 ELSE 1 END, created_at ASC");
            }
            ReadySortPolicy::Priority => {
                sql.push_str(" ORDER BY priority ASC, created_at ASC");
            }
            ReadySortPolicy::Oldest => {
                sql.push_str(" ORDER BY created_at ASC");
            }
        }

        // Apply limit in SQL only when the blocked filter also happens in SQL.
        // Fallback callers must filter blocked IDs in Rust first.
        if apply_limit
            && let Some(limit) = filters.limit
            && limit > 0
        {
            let _ = write!(sql, " LIMIT {limit}");
        }

        Ok((sql, params))
    }

    fn query_ready_issue_candidates_with_projection(
        &self,
        filters: &ReadyFilters,
        sort: ReadySortPolicy,
        exclude_blocked_in_sql: bool,
        apply_limit: bool,
        projection: ReadyIssueProjection,
    ) -> Result<Vec<Issue>> {
        let (sql, params) = self.build_ready_issue_candidates_query(
            filters,
            sort,
            exclude_blocked_in_sql,
            apply_limit,
            projection,
        )?;
        let rows = self.conn.query_with_params(&sql, &params)?;
        let mut issues = Vec::with_capacity(rows.len());
        for row in &rows {
            issues.push(projection.parse_row(row)?);
        }

        Ok(issues)
    }

    fn query_ready_issues_without_cache_with_projection(
        &self,
        filters: &ReadyFilters,
        sort: ReadySortPolicy,
        blocked_ids: &HashSet<String>,
        projection: ReadyIssueProjection,
    ) -> Result<Vec<Issue>> {
        let mut issues = self.query_ready_issue_candidates_with_projection(
            filters, sort, false, false, projection,
        )?;
        issues.retain(|issue| !blocked_ids.contains(issue.id.as_str()));
        if let Some(limit) = filters.limit
            && limit > 0
            && issues.len() > limit
        {
            issues.truncate(limit);
        }
        Ok(issues)
    }

    fn recover_blocked_issues_map(
        &self,
        stage: &'static str,
        error: &dyn std::fmt::Display,
    ) -> Result<HashMap<String, Vec<String>>> {
        tracing::warn!(
            stage,
            %error,
            "Blocked cache unavailable; computing blocker graph directly"
        );
        Self::compute_blocked_issues_map_impl(&self.conn)
    }

    fn recover_blocked_ids(
        &self,
        stage: &'static str,
        error: &dyn std::fmt::Display,
    ) -> Result<HashSet<String>> {
        Ok(self
            .recover_blocked_issues_map(stage, error)?
            .into_keys()
            .collect())
    }

    /// Get IDs of blocked issues from cache.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_blocked_ids(&self) -> Result<HashSet<String>> {
        if let Err(error) = self.ensure_blocked_cache_fresh() {
            return self.recover_blocked_ids("get_blocked_ids_refresh", &error);
        }
        let rows = match self.conn.query("SELECT issue_id FROM blocked_issues_cache") {
            Ok(rows) => rows,
            Err(error) => return self.recover_blocked_ids("get_blocked_ids_query", &error),
        };
        let mut ids = HashSet::new();
        for row in &rows {
            if let Some(id) = row.get(0).and_then(SqliteValue::as_text) {
                ids.insert(id.to_string());
            }
        }
        Ok(ids)
    }

    /// Get issue IDs blocked by `blocks` dependency type only (not full cache).
    ///
    /// This is used for stats computation where blocked count should be based
    /// only on `blocks` deps per classic bd semantics.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_blocked_by_blocks_deps_only(&self) -> Result<HashSet<String>> {
        // Returns issues that:
        // 1. Have a 'blocks' type dependency
        // 2. Where the blocker is not closed/tombstone
        // 3. AND the blocked issue itself is not closed/tombstone
        let rows = self.conn.query(
            r"SELECT DISTINCT d.issue_id
              FROM dependencies d
              LEFT JOIN issues blocker ON d.depends_on_id = blocker.id
              LEFT JOIN issues blocked ON d.issue_id = blocked.id
              WHERE d.type = 'blocks'
                AND blocker.status NOT IN ('closed', 'tombstone')
                AND blocked.status NOT IN ('closed', 'tombstone')",
        )?;
        let mut ids = HashSet::new();
        for row in &rows {
            if let Some(id) = row.get(0).and_then(SqliteValue::as_text) {
                ids.insert(id.to_string());
            }
        }
        Ok(ids)
    }

    /// Get raw `blocks` dependency edges as (issue_id, depends_on_id) pairs.
    ///
    /// This is a lightweight single-table query (no JOINs) suitable for
    /// callers that already have issues loaded in memory and can filter
    /// by status themselves.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_blocks_dep_edges(&self) -> Result<Vec<(String, String)>> {
        let rows = self
            .conn
            .query("SELECT issue_id, depends_on_id FROM dependencies WHERE type = 'blocks'")?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                let issue_id = row.get(0).and_then(SqliteValue::as_text)?.to_string();
                let depends_on = row.get(1).and_then(SqliteValue::as_text)?.to_string();
                Some((issue_id, depends_on))
            })
            .collect())
    }

    /// Check if an issue is blocked (in the blocked cache).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn is_blocked(&self, issue_id: &str) -> Result<bool> {
        if let Err(error) = self.ensure_blocked_cache_fresh() {
            let blocked_ids = self.recover_blocked_ids("is_blocked_refresh", &error)?;
            return Ok(blocked_ids.contains(issue_id));
        }
        let rows = match self.conn.query_with_params(
            "SELECT 1 FROM blocked_issues_cache WHERE issue_id = ? LIMIT 1",
            &[SqliteValue::from(issue_id)],
        ) {
            Ok(rows) => rows,
            Err(error) => {
                let blocked_ids = self.recover_blocked_ids("is_blocked_query", &error)?;
                return Ok(blocked_ids.contains(issue_id));
            }
        };
        Ok(!rows.is_empty())
    }

    /// Get the actual blockers for an issue from the blocked issues cache.
    ///
    /// Returns the issue IDs that are blocking this issue. The format includes
    /// status annotations like "bd-123:open" or "bd-456:parent-blocked".
    /// Returns an empty vec if the issue is not blocked.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_blockers(&self, issue_id: &str) -> Result<Vec<String>> {
        if let Err(error) = self.ensure_blocked_cache_fresh() {
            let blocked_issues_map =
                self.recover_blocked_issues_map("get_blockers_refresh", &error)?;
            return Ok(Self::blocker_refs_to_issue_ids(
                blocked_issues_map
                    .get(issue_id)
                    .map_or(&[][..], Vec::as_slice),
            ));
        }
        let rows = match self.conn.query_with_params(
            "SELECT blocked_by FROM blocked_issues_cache WHERE issue_id = ?",
            &[SqliteValue::from(issue_id)],
        ) {
            Ok(rows) => rows,
            Err(error) => {
                let blocked_issues_map =
                    self.recover_blocked_issues_map("get_blockers_query", &error)?;
                return Ok(Self::blocker_refs_to_issue_ids(
                    blocked_issues_map
                        .get(issue_id)
                        .map_or(&[][..], Vec::as_slice),
                ));
            }
        };
        let Some(row) = rows.first() else {
            return Ok(Vec::new());
        };

        match parse_blocked_by_json(issue_id, row.get(0).and_then(SqliteValue::as_text)) {
            Ok(blockers) => Ok(Self::blocker_refs_to_issue_ids(&blockers)),
            Err(error) => {
                let blocked_issues_map =
                    self.recover_blocked_issues_map("get_blockers_parse", &error)?;
                Ok(Self::blocker_refs_to_issue_ids(
                    blocked_issues_map
                        .get(issue_id)
                        .map_or(&[][..], Vec::as_slice),
                ))
            }
        }
    }

    /// Rebuild the blocked issues cache from scratch.
    ///
    /// This computes which issues are blocked based on their dependencies
    /// and the status of their blockers. Standard blocking edges (`blocks`,
    /// `conditional-blocks`, `waits-for`) block directly. `parent-child`
    /// does not make an open parent block a child; instead it propagates an
    /// already-blocked parent down to its descendants.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    #[allow(clippy::too_many_lines)]
    pub fn rebuild_blocked_cache(&mut self, force_rebuild: bool) -> Result<usize> {
        if !force_rebuild {
            return Ok(0);
        }
        self.with_write_transaction(|storage| {
            let rebuilt = Self::rebuild_blocked_cache_impl(&storage.conn)?;
            Self::delete_metadata_key_in_tx(&storage.conn, BLOCKED_CACHE_STATE_KEY)?;
            Ok(rebuilt)
        })
    }

    /// Rebuild the blocked cache using the caller's active transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if the rebuild fails.
    pub(crate) fn rebuild_blocked_cache_in_tx(&self) -> Result<usize> {
        let rebuilt = Self::rebuild_blocked_cache_impl(&self.conn)?;
        Self::delete_metadata_key_in_tx(&self.conn, BLOCKED_CACHE_STATE_KEY)?;
        Ok(rebuilt)
    }

    /// Rebuild the child counters table from all existing issues.
    ///
    /// Useful after a full import or manual database manipulation.
    ///
    /// # Errors
    ///
    /// Returns an error if the rebuild fails.
    pub(crate) fn rebuild_child_counters_in_tx(&self) -> Result<usize> {
        Self::rebuild_child_counters_impl(&self.conn)
    }

    fn rebuild_child_counters_impl(conn: &Connection) -> Result<usize> {
        // Clear existing counters
        conn.execute("DELETE FROM child_counters")?;

        // Build counters only for parents that still exist. Recovered imports can
        // contain hierarchical IDs whose root parent was deleted long ago.
        let rows = conn.query("SELECT id FROM issues")?;
        let issue_ids: HashSet<String> = rows
            .iter()
            .filter_map(|row| row.get(0).and_then(SqliteValue::as_text).map(String::from))
            .collect();
        let mut max_children: HashMap<String, u32> = HashMap::new();
        let mut skipped_missing_parents = 0usize;

        for id in &issue_ids {
            let Ok(parsed) = parse_id(id) else {
                continue;
            };
            if parsed.is_root() {
                continue;
            }

            let Some(parent) = parsed.parent() else {
                skipped_missing_parents += 1;
                continue;
            };
            if !issue_ids.contains(&parent) {
                skipped_missing_parents += 1;
                continue;
            }

            let Some(&child_num) = parsed.child_path.last() else {
                skipped_missing_parents += 1;
                continue;
            };

            let entry = max_children.entry(parent).or_insert(0);
            if child_num > *entry {
                *entry = child_num;
            }
        }

        if skipped_missing_parents > 0 {
            tracing::debug!(
                skipped_missing_parents,
                "Skipped child counter rebuild for hierarchical issues whose parent ID is missing"
            );
        }

        let mut count = 0;
        for (parent_id, last_child) in max_children {
            conn.execute_with_params(
                "INSERT INTO child_counters (parent_id, last_child) VALUES (?, ?)",
                &[
                    SqliteValue::from(parent_id.as_str()),
                    SqliteValue::from(i64::from(last_child)),
                ],
            )?;
            count += 1;
        }

        Ok(count)
    }

    fn compute_blocked_issues_map_impl(conn: &Connection) -> Result<HashMap<String, Vec<String>>> {
        let mut blocked_issues_map = Self::load_direct_blockers_impl(conn)?;
        let children_by_parent = Self::load_local_parent_child_edges_impl(conn)?;

        // 1. Propagate standard blockers (blocks, conditional-blocks, waits-for)
        // from parent to children.
        Self::propagate_blocked_parents(&mut blocked_issues_map, &children_by_parent);

        // 2. Add blockers for parents with open children.
        // We do this AFTER propagation so that a parent blocked only by its children
        // does not transitively block those same children (avoiding logic cycle).
        let child_blockers = Self::load_local_open_child_blockers_impl(conn)?;
        for (parent_id, mut blockers) in child_blockers {
            blocked_issues_map
                .entry(parent_id)
                .or_default()
                .append(&mut blockers);
        }

        blocked_issues_map.retain(|_, blockers| {
            blockers.sort();
            blockers.dedup();
            !blockers.is_empty()
        });

        Ok(blocked_issues_map)
    }

    fn blocker_refs_to_issue_ids(blockers: &[String]) -> Vec<String> {
        blockers
            .iter()
            .map(|blocker| {
                blocker
                    .split(':')
                    .next()
                    .unwrap_or(blocker.as_str())
                    .to_string()
            })
            .collect()
    }

    fn load_blocked_issues_from_map(
        &self,
        blocked_issues_map: &HashMap<String, Vec<String>>,
    ) -> Result<Vec<(Issue, Vec<String>)>> {
        if blocked_issues_map.is_empty() {
            return Ok(Vec::new());
        }

        let rows = self.conn.query(
            r"SELECT id, content_hash, title, description, design, acceptance_criteria, notes,
                     status, priority, issue_type, assignee, owner, estimated_minutes,
                     created_at, created_by, updated_at, closed_at, close_reason, closed_by_session,
                     due_at, defer_until, external_ref, source_system, source_repo,
                     deleted_at, deleted_by, delete_reason, original_type, compaction_level,
                     compacted_at, compacted_at_commit, original_size, sender, ephemeral,
                     pinned, is_template
              FROM issues
              WHERE status NOT IN ('closed', 'tombstone')
              ORDER BY priority ASC, created_at DESC",
        )?;

        let mut blocked_issues = Vec::new();
        for row in &rows {
            let issue = Self::issue_from_row(row)?;
            if let Some(blockers) = blocked_issues_map.get(issue.id.as_str()) {
                blocked_issues.push((issue, blockers.clone()));
            }
        }

        Ok(blocked_issues)
    }

    fn rebuild_blocked_cache_impl(conn: &Connection) -> Result<usize> {
        let blocked_issues_map = Self::compute_blocked_issues_map_impl(conn)?;

        // Atomic update: Clear and Re-insert
        // Since we are within a BEGIN IMMEDIATE transaction, this is safe and efficient.
        conn.execute("DELETE FROM blocked_issues_cache")?;

        let mut entries = Vec::with_capacity(blocked_issues_map.len());

        for (issue_id, blockers) in blocked_issues_map {
            let blockers_json = match serde_json::to_string(&blockers) {
                Ok(blockers_json) => blockers_json,
                Err(error) => {
                    tracing::warn!(
                        issue_id = %issue_id,
                        %error,
                        "Failed to serialize blocker list; treating issue as unblocked"
                    );
                    continue;
                }
            };
            entries.push((issue_id, blockers_json));
        }

        let count = Self::insert_blocked_cache_entries(conn, &entries)?;
        tracing::debug!(blocked_count = count, "Rebuilt blocked issues cache");
        Ok(count)
    }

    /// Incremental blocked-cache update: recompute only the entries for the
    /// given seed issue IDs and their transitive parent-child descendants.
    ///
    /// This avoids the full DELETE + INSERT cycle of `rebuild_blocked_cache_impl`
    /// when only a small number of dependency edges changed.
    fn incremental_blocked_cache_update(
        conn: &Connection,
        seed_ids: &HashSet<String>,
    ) -> Result<usize> {
        let children_by_parent = Self::load_local_parent_child_edges_impl(conn)?;
        let parents_by_child = Self::build_parents_by_child(&children_by_parent);
        let affected =
            Self::expand_blocked_cache_component(seed_ids, &children_by_parent, &parents_by_child);
        let affected_children_by_parent =
            Self::filter_parent_child_edges_for_ids(&children_by_parent, &affected);

        // Recompute only the affected component instead of rebuilding the full
        // blocker graph inside the active write transaction.
        let mut blocked_issues_map = Self::load_direct_blockers_for_ids_impl(conn, &affected)?;
        Self::propagate_blocked_parents(&mut blocked_issues_map, &affected_children_by_parent);
        let child_blockers = Self::load_local_open_child_blockers_for_ids_impl(conn, &affected)?;
        for (parent_id, mut blockers) in child_blockers {
            blocked_issues_map
                .entry(parent_id)
                .or_default()
                .append(&mut blockers);
        }

        // 3. Delete only affected rows from the cache.
        for chunk in affected
            .iter()
            .collect::<Vec<_>>()
            .chunks(BLOCKED_CACHE_DELETE_CHUNK_SIZE)
        {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            let sql = format!(
                "DELETE FROM blocked_issues_cache WHERE issue_id IN ({})",
                placeholders.join(", ")
            );
            let params: Vec<SqliteValue> = chunk
                .iter()
                .map(|id| SqliteValue::from(id.as_str()))
                .collect();
            conn.execute_with_params(&sql, &params)?;
        }

        // 4. Re-insert only affected rows that have blockers.
        let mut entries = Vec::new();
        for id in &affected {
            if let Some(mut blockers) = blocked_issues_map.remove(id.as_str()) {
                if blockers.is_empty() {
                    continue;
                }
                blockers.sort();
                blockers.dedup();
                let blockers_json = match serde_json::to_string(&blockers) {
                    Ok(json) => json,
                    Err(error) => {
                        tracing::warn!(
                            issue_id = %id,
                            %error,
                            "Failed to serialize blocker list; treating issue as unblocked"
                        );
                        continue;
                    }
                };
                entries.push((id.clone(), blockers_json));
            }
        }

        let count = Self::insert_blocked_cache_entries(conn, &entries)?;
        tracing::debug!(
            affected_count = affected.len(),
            blocked_count = count,
            "Incremental blocked cache update"
        );
        Ok(count)
    }

    fn insert_blocked_cache_entries(
        conn: &Connection,
        entries: &[(String, String)],
    ) -> Result<usize> {
        let mut count = 0;
        for (issue_id, blockers_json) in entries {
            conn.execute_with_params(
                // DELETE + INSERT to simulate UPSERT (fsqlite does not
                // reliably enforce UNIQUE/PRIMARY KEY for OR REPLACE).
                "DELETE FROM blocked_issues_cache WHERE issue_id = ?",
                &[SqliteValue::from(issue_id.as_str())],
            )?;
            conn.execute_with_params(
                "INSERT INTO blocked_issues_cache (issue_id, blocked_by, blocked_at) VALUES (?, ?, CURRENT_TIMESTAMP)",
                &[
                    SqliteValue::from(issue_id.as_str()),
                    SqliteValue::from(blockers_json.as_str()),
                ],
            )?;
            count += 1;
        }
        Ok(count)
    }

    fn load_direct_blockers_impl(conn: &Connection) -> Result<HashMap<String, Vec<String>>> {
        // Exclude external dependencies from the persisted cache because their
        // status is not locally known and must be resolved at query time.
        let rows = conn.query(
            "SELECT DISTINCT d.issue_id, d.depends_on_id || ':' || COALESCE(i.status, 'unknown')
             FROM dependencies d
             LEFT JOIN issues i ON d.depends_on_id = i.id
             WHERE d.type IN ('blocks', 'conditional-blocks', 'waits-for')
               AND d.depends_on_id NOT LIKE 'external:%'
               AND (
                 i.status NOT IN ('closed', 'tombstone')
                 OR i.id IS NULL
               )
               AND (i.is_template = 0 OR i.is_template IS NULL OR i.id IS NULL)",
        )?;
        let mut blocked_issues_map: HashMap<String, Vec<String>> = HashMap::new();

        for row in &rows {
            let Some(issue_id) = row.get(0).and_then(SqliteValue::as_text) else {
                continue;
            };
            let Some(blocker_ref) = row.get(1).and_then(SqliteValue::as_text) else {
                continue;
            };
            if issue_id.is_empty() || blocker_ref.is_empty() {
                continue;
            }
            blocked_issues_map
                .entry(issue_id.to_string())
                .or_default()
                .push(blocker_ref.to_string());
        }

        Ok(blocked_issues_map)
    }

    fn load_direct_blockers_for_ids_impl(
        conn: &Connection,
        issue_ids: &HashSet<String>,
    ) -> Result<HashMap<String, Vec<String>>> {
        if issue_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let mut blocked_issues_map: HashMap<String, Vec<String>> = HashMap::new();
        let issue_ids: Vec<_> = issue_ids.iter().collect();

        for chunk in issue_ids.chunks(400) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            let sql = format!(
                "SELECT DISTINCT d.issue_id, d.depends_on_id || ':' || COALESCE(i.status, 'unknown')
                 FROM dependencies d
                 LEFT JOIN issues i ON d.depends_on_id = i.id
                 WHERE d.issue_id IN ({})
                   AND d.type IN ('blocks', 'conditional-blocks', 'waits-for')
                   AND d.depends_on_id NOT LIKE 'external:%'
                   AND (
                     i.status NOT IN ('closed', 'tombstone')
                     OR i.id IS NULL
                   )
                   AND (i.is_template = 0 OR i.is_template IS NULL OR i.id IS NULL)",
                placeholders.join(", ")
            );
            let params: Vec<SqliteValue> = chunk
                .iter()
                .map(|issue_id| SqliteValue::from(issue_id.as_str()))
                .collect();
            let rows = conn.query_with_params(&sql, &params)?;

            for row in &rows {
                let Some(issue_id) = row.get(0).and_then(SqliteValue::as_text) else {
                    continue;
                };
                let Some(blocker_ref) = row.get(1).and_then(SqliteValue::as_text) else {
                    continue;
                };
                if issue_id.is_empty() || blocker_ref.is_empty() {
                    continue;
                }
                blocked_issues_map
                    .entry(issue_id.to_string())
                    .or_default()
                    .push(blocker_ref.to_string());
            }
        }

        Ok(blocked_issues_map)
    }

    fn load_local_parent_child_edges_impl(
        conn: &Connection,
    ) -> Result<HashMap<String, Vec<String>>> {
        let edge_rows = conn.query(
            "SELECT issue_id, depends_on_id
             FROM dependencies
             WHERE type = 'parent-child'
               AND issue_id NOT LIKE 'external:%'
               AND depends_on_id NOT LIKE 'external:%'",
        )?;
        let mut children_by_parent: HashMap<String, Vec<String>> = HashMap::new();

        for row in &edge_rows {
            let Some(child_id) = row.get(0).and_then(SqliteValue::as_text) else {
                continue;
            };
            let Some(parent_id) = row.get(1).and_then(SqliteValue::as_text) else {
                continue;
            };
            children_by_parent
                .entry(parent_id.to_string())
                .or_default()
                .push(child_id.to_string());
        }

        Ok(children_by_parent)
    }

    fn build_parents_by_child(
        children_by_parent: &HashMap<String, Vec<String>>,
    ) -> HashMap<String, Vec<String>> {
        let mut parents_by_child: HashMap<String, Vec<String>> = HashMap::new();
        for (parent_id, children) in children_by_parent {
            for child_id in children {
                parents_by_child
                    .entry(child_id.clone())
                    .or_default()
                    .push(parent_id.clone());
            }
        }
        parents_by_child
    }

    fn expand_blocked_cache_component(
        seed_ids: &HashSet<String>,
        children_by_parent: &HashMap<String, Vec<String>>,
        parents_by_child: &HashMap<String, Vec<String>>,
    ) -> HashSet<String> {
        let mut affected = seed_ids.clone();
        let mut queue: Vec<String> = seed_ids.iter().cloned().collect();

        while let Some(id) = queue.pop() {
            if let Some(children) = children_by_parent.get(&id) {
                for child_id in children {
                    if affected.insert(child_id.clone()) {
                        queue.push(child_id.clone());
                    }
                }
            }

            if let Some(parents) = parents_by_child.get(&id) {
                for parent_id in parents {
                    if affected.insert(parent_id.clone()) {
                        queue.push(parent_id.clone());
                    }
                }
            }
        }

        affected
    }

    fn filter_parent_child_edges_for_ids(
        children_by_parent: &HashMap<String, Vec<String>>,
        issue_ids: &HashSet<String>,
    ) -> HashMap<String, Vec<String>> {
        let mut filtered = HashMap::new();
        for (parent_id, children) in children_by_parent {
            if !issue_ids.contains(parent_id) {
                continue;
            }
            let affected_children: Vec<String> = children
                .iter()
                .filter(|child_id| issue_ids.contains(child_id.as_str()))
                .cloned()
                .collect();
            if !affected_children.is_empty() {
                filtered.insert(parent_id.clone(), affected_children);
            }
        }
        filtered
    }

    fn load_local_open_child_blockers_impl(
        conn: &Connection,
    ) -> Result<HashMap<String, Vec<String>>> {
        let rows = conn.query(
            "SELECT DISTINCT d.depends_on_id as parent_id, d.issue_id || ':child-open' as blocker
             FROM dependencies d
             JOIN issues i ON d.issue_id = i.id
             WHERE d.type = 'parent-child'
               AND i.status NOT IN ('closed', 'tombstone')
               AND (i.is_template = 0 OR i.is_template IS NULL)
               AND d.depends_on_id NOT LIKE 'external:%'
               AND d.issue_id NOT LIKE 'external:%'",
        )?;
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for row in &rows {
            let Some(parent_id) = row.get(0).and_then(SqliteValue::as_text) else {
                continue;
            };
            let Some(blocker) = row.get(1).and_then(SqliteValue::as_text) else {
                continue;
            };
            if parent_id.is_empty() || blocker.is_empty() {
                continue;
            }
            map.entry(parent_id.to_string())
                .or_default()
                .push(blocker.to_string());
        }
        Ok(map)
    }

    fn load_local_open_child_blockers_for_ids_impl(
        conn: &Connection,
        parent_ids: &HashSet<String>,
    ) -> Result<HashMap<String, Vec<String>>> {
        if parent_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        let parent_ids: Vec<_> = parent_ids.iter().collect();

        for chunk in parent_ids.chunks(400) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            let sql = format!(
                "SELECT DISTINCT d.depends_on_id as parent_id, d.issue_id || ':child-open' as blocker
                 FROM dependencies d
                 JOIN issues i ON d.issue_id = i.id
                 WHERE d.depends_on_id IN ({})
                   AND d.type = 'parent-child'
                   AND i.status NOT IN ('closed', 'tombstone')
                   AND (i.is_template = 0 OR i.is_template IS NULL)
                   AND d.depends_on_id NOT LIKE 'external:%'
                   AND d.issue_id NOT LIKE 'external:%'",
                placeholders.join(", ")
            );
            let params: Vec<SqliteValue> = chunk
                .iter()
                .map(|parent_id| SqliteValue::from(parent_id.as_str()))
                .collect();
            let rows = conn.query_with_params(&sql, &params)?;

            for row in &rows {
                let Some(parent_id) = row.get(0).and_then(SqliteValue::as_text) else {
                    continue;
                };
                let Some(blocker) = row.get(1).and_then(SqliteValue::as_text) else {
                    continue;
                };
                if parent_id.is_empty() || blocker.is_empty() {
                    continue;
                }
                map.entry(parent_id.to_string())
                    .or_default()
                    .push(blocker.to_string());
            }
        }

        Ok(map)
    }

    fn propagate_blocked_parents(
        blocked_issues_map: &mut HashMap<String, Vec<String>>,
        children_by_parent: &HashMap<String, Vec<String>>,
    ) {
        if children_by_parent.is_empty() || blocked_issues_map.is_empty() {
            return;
        }

        let mut queue: Vec<String> = blocked_issues_map.keys().cloned().collect();
        let mut seen: HashSet<String> = HashSet::new();

        while let Some(parent_id) = queue.pop() {
            if !seen.insert(parent_id.clone()) {
                continue;
            }
            if let Some(children) = children_by_parent.get(&parent_id) {
                for child_id in children {
                    let marker = format!("{parent_id}:parent-blocked");
                    let entry = blocked_issues_map.entry(child_id.clone()).or_default();
                    if entry.contains(&marker) {
                        continue;
                    }
                    entry.push(marker);
                    queue.push(child_id.clone());
                }
            }
        }
    }

    /// Get issues that are blocked, along with what's blocking them.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_blocked_issues(&self) -> Result<Vec<(Issue, Vec<String>)>> {
        if let Err(error) = self.ensure_blocked_cache_fresh() {
            let blocked_issues_map =
                self.recover_blocked_issues_map("get_blocked_issues_refresh", &error)?;
            return self.load_blocked_issues_from_map(&blocked_issues_map);
        }
        let rows = match self.conn.query(
            r"SELECT i.id, i.content_hash, i.title, i.description, i.design, i.acceptance_criteria, i.notes,
                     i.status, i.priority, i.issue_type, i.assignee, i.owner, i.estimated_minutes,
                     i.created_at, i.created_by, i.updated_at, i.closed_at, i.close_reason, i.closed_by_session,
                     i.due_at, i.defer_until, i.external_ref, i.source_system, i.source_repo,
                     i.deleted_at, i.deleted_by, i.delete_reason, i.original_type, i.compaction_level,
                     i.compacted_at, i.compacted_at_commit, i.original_size, i.sender, i.ephemeral,
                     i.pinned, i.is_template,
                     bc.blocked_by
              FROM issues i
              INNER JOIN blocked_issues_cache bc ON i.id = bc.issue_id
              WHERE i.status NOT IN ('closed', 'tombstone')
              ORDER BY i.priority ASC, i.created_at DESC",
        ) {
            Ok(rows) => rows,
            Err(error) => {
                let blocked_issues_map =
                    self.recover_blocked_issues_map("get_blocked_issues_query", &error)?;
                return self.load_blocked_issues_from_map(&blocked_issues_map);
            }
        };

        let mut blocked_issues = Vec::new();
        for row in &rows {
            let issue = Self::issue_from_row(row)?;
            let blockers = match parse_blocked_by_json(
                &issue.id,
                row.get(36).and_then(SqliteValue::as_text),
            ) {
                Ok(blockers) => blockers,
                Err(error) => {
                    let blocked_issues_map =
                        self.recover_blocked_issues_map("get_blocked_issues_parse", &error)?;
                    return self.load_blocked_issues_from_map(&blocked_issues_map);
                }
            };
            blocked_issues.push((issue, blockers));
        }

        Ok(blocked_issues)
    }

    /// Check if the project has any external dependencies.
    ///
    /// # Errors
    ///
    /// Returns an error if the dependency probe query fails.
    pub fn has_external_dependencies(&self, blocking_only: bool) -> Result<bool> {
        let sql = if blocking_only {
            "SELECT 1
             FROM dependencies
             WHERE (depends_on_id LIKE 'external:%'
                    AND type IN ('blocks', 'conditional-blocks', 'waits-for'))
                OR (issue_id LIKE 'external:%' AND type = 'parent-child')
             LIMIT 1"
        } else {
            "SELECT 1
             FROM dependencies
             WHERE depends_on_id LIKE 'external:%'
                OR (issue_id LIKE 'external:%' AND type = 'parent-child')
             LIMIT 1"
        };
        let rows = self.conn.query(sql)?;
        Ok(!rows.is_empty())
    }

    /// Resolve external dependency satisfaction for dependencies of this project.
    ///
    /// Returns a map of external dependency IDs to whether they are satisfied.
    /// Missing projects or query failures are treated as unsatisfied.
    ///
    /// # Errors
    ///
    /// Returns an error if querying local dependencies fails.
    pub fn resolve_external_dependency_statuses(
        &self,
        external_db_paths: &HashMap<String, PathBuf>,
        blocking_only: bool,
    ) -> Result<HashMap<String, bool>> {
        let external_ids = self.list_external_dependency_ids(blocking_only)?;
        Ok(Self::resolve_external_dependency_statuses_for_ids(
            &external_ids,
            external_db_paths,
        ))
    }

    pub(crate) fn resolve_external_dependency_statuses_for_ids(
        external_ids: &HashSet<String>,
        external_db_paths: &HashMap<String, PathBuf>,
    ) -> HashMap<String, bool> {
        if external_ids.is_empty() {
            return HashMap::new();
        }

        let mut project_caps: HashMap<String, HashSet<String>> = HashMap::new();
        let mut parsed: HashMap<String, (String, String)> = HashMap::new();
        for dep_id in external_ids {
            if let Some((project, capability)) = parse_external_dependency(dep_id) {
                project_caps
                    .entry(project.clone())
                    .or_default()
                    .insert(capability.clone());
                parsed.insert(dep_id.clone(), (project, capability));
            }
        }

        // Query each external project's database to find satisfied capabilities
        let mut satisfied: HashMap<String, HashSet<String>> = HashMap::new();
        for (project, caps) in &project_caps {
            let Some(db_path) = external_db_paths.get(project) else {
                tracing::warn!(
                    project = %project,
                    "External project not configured; treating dependencies as unsatisfied"
                );
                continue;
            };

            match query_external_project_capabilities(db_path, caps) {
                Ok(found) => {
                    satisfied.insert(project.clone(), found);
                }
                Err(err) => {
                    tracing::warn!(
                        project = %project,
                        path = %db_path.display(),
                        error = %err,
                        "Failed to query external project; treating dependencies as unsatisfied"
                    );
                }
            }
        }

        let mut statuses = HashMap::new();
        for dep_id in external_ids {
            let is_satisfied = parsed
                .get(dep_id.as_str())
                .is_some_and(|(project, capability)| {
                    satisfied
                        .get(project)
                        .is_some_and(|caps| caps.contains(capability))
                });
            statuses.insert(dep_id.clone(), is_satisfied);
        }

        statuses
    }

    /// Compute blockers caused by unsatisfied external dependencies.
    ///
    /// This excludes external dependencies from the blocked cache and evaluates
    /// them at query time, including parent-child propagation.
    ///
    /// # Errors
    ///
    /// Returns an error if dependency queries fail.
    pub fn external_blockers(
        &self,
        external_statuses: &HashMap<String, bool>,
    ) -> Result<HashMap<String, Vec<String>>> {
        let mut blockers: HashMap<String, Vec<String>> = HashMap::new();

        // Direct external blockers.
        // 1. Local issues blocked by external targets (standard blocking types)
        let rows = self.conn.query(
            "SELECT issue_id, depends_on_id
             FROM dependencies
             WHERE depends_on_id LIKE 'external:%'
               AND type IN ('blocks', 'conditional-blocks', 'waits-for')",
        )?;

        for row in &rows {
            let issue_id = row.get(0).and_then(SqliteValue::as_text).unwrap_or("");
            let depends_on_id = row.get(1).and_then(SqliteValue::as_text).unwrap_or("");
            let satisfied = external_statuses
                .get(depends_on_id)
                .copied()
                .unwrap_or(false);
            if !satisfied {
                blockers
                    .entry(issue_id.to_string())
                    .or_default()
                    .push(format!("{depends_on_id}:blocked"));
            }
        }

        // 2. Local parents blocked by external children
        let rows = self.conn.query(
            "SELECT depends_on_id, issue_id
             FROM dependencies
             WHERE issue_id LIKE 'external:%'
               AND type = 'parent-child'",
        )?;

        for row in &rows {
            let parent_id = row.get(0).and_then(SqliteValue::as_text).unwrap_or("");
            let child_id = row.get(1).and_then(SqliteValue::as_text).unwrap_or("");
            let satisfied = external_statuses.get(child_id).copied().unwrap_or(false);
            if !satisfied {
                blockers
                    .entry(parent_id.to_string())
                    .or_default()
                    .push(format!("{child_id}:child-blocked"));
            }
        }

        // Propagate externally blocked parents down through local parent-child relationships.
        let edge_rows = self.conn.query(
            "SELECT issue_id, depends_on_id
             FROM dependencies
             WHERE type = 'parent-child'
               AND issue_id NOT LIKE 'external:%'
               AND depends_on_id NOT LIKE 'external:%'",
        )?;
        let mut children_by_parent: HashMap<String, Vec<String>> = HashMap::new();
        for row in &edge_rows {
            let Some(child_id) = row.get(0).and_then(SqliteValue::as_text) else {
                continue;
            };
            let Some(parent_id) = row.get(1).and_then(SqliteValue::as_text) else {
                continue;
            };
            children_by_parent
                .entry(parent_id.to_string())
                .or_default()
                .push(child_id.to_string());
        }

        if !children_by_parent.is_empty() && !blockers.is_empty() {
            let mut queue: Vec<String> = blockers.keys().cloned().collect();
            let mut seen: HashSet<String> = HashSet::new();

            while let Some(parent_id) = queue.pop() {
                if !seen.insert(parent_id.clone()) {
                    continue;
                }
                if let Some(children) = children_by_parent.get(&parent_id) {
                    for child_id in children {
                        let entry = blockers.entry(child_id.clone()).or_default();
                        let marker = format!("{parent_id}:parent-blocked");
                        if entry.contains(&marker) {
                            continue;
                        }
                        entry.push(marker);
                        queue.push(child_id.clone());
                    }
                }
            }
        }

        Ok(blockers
            .into_iter()
            .map(|(k, v)| (k, v.into_iter().collect()))
            .collect())
    }

    fn list_external_dependency_ids(&self, blocking_only: bool) -> Result<HashSet<String>> {
        let mut ids = HashSet::new();
        let sql = if blocking_only {
            "SELECT DISTINCT depends_on_id
             FROM dependencies
             WHERE depends_on_id LIKE 'external:%'
               AND type IN ('blocks', 'conditional-blocks', 'waits-for')
             UNION
             SELECT DISTINCT issue_id
             FROM dependencies
             WHERE issue_id LIKE 'external:%'
               AND type = 'parent-child'"
        } else {
            "SELECT DISTINCT depends_on_id
             FROM dependencies
             WHERE depends_on_id LIKE 'external:%'
             UNION
             SELECT DISTINCT issue_id
             FROM dependencies
             WHERE issue_id LIKE 'external:%'"
        };

        let rows = self.conn.query(sql)?;
        for row in &rows {
            if let Some(id) = row.get(0).and_then(SqliteValue::as_text) {
                ids.insert(id.to_string());
            }
        }
        Ok(ids)
    }

    /// Check if an issue ID already exists.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn id_exists(&self, id: &str) -> Result<bool> {
        let rows = self.conn.query_with_params(
            "SELECT 1 FROM issues WHERE id = ? LIMIT 1",
            &[SqliteValue::from(id)],
        )?;
        Ok(!rows.is_empty())
    }

    fn issue_status_in_tx(conn: &Connection, id: &str) -> Result<Option<Status>> {
        let rows = conn.query_with_params(
            "SELECT status FROM issues WHERE id = ?",
            &[SqliteValue::from(id)],
        )?;
        if let Some(row) = rows.first() {
            let status_str = row.get(0).and_then(SqliteValue::as_text).unwrap_or("");
            Ok(Some(status_str.parse()?))
        } else {
            Ok(None)
        }
    }

    fn ensure_dependency_target_exists_in_tx(conn: &Connection, depends_on_id: &str) -> Result<()> {
        if depends_on_id.starts_with("external:") {
            return Ok(());
        }

        match Self::issue_status_in_tx(conn, depends_on_id)? {
            Some(Status::Tombstone) => Err(BeadsError::Validation {
                field: "depends_on_id".to_string(),
                reason: format!("cannot depend on tombstone issue: {depends_on_id}"),
            }),
            Some(_) => Ok(()),
            None => Err(BeadsError::IssueNotFound {
                id: depends_on_id.to_string(),
            }),
        }
    }

    fn validate_parent_child_endpoints(
        issue_id: &str,
        depends_on_id: &str,
        dep_type: &str,
    ) -> Result<()> {
        if dep_type.eq_ignore_ascii_case("parent-child")
            && (issue_id.starts_with("external:") || depends_on_id.starts_with("external:"))
        {
            let (field, endpoint) = if issue_id.starts_with("external:") {
                ("issue_id", issue_id)
            } else {
                ("depends_on_id", depends_on_id)
            };
            return Err(BeadsError::Validation {
                field: field.to_string(),
                reason: format!("parent-child dependencies must link local issues: {endpoint}"),
            });
        }

        Ok(())
    }

    /// Find issue IDs that end with the given hash substring.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn find_ids_by_hash(&self, hash_suffix: &str) -> Result<Vec<String>> {
        let all_ids = self.get_all_ids()?;
        Ok(crate::util::id::find_matching_ids(&all_ids, hash_suffix))
    }

    /// Count total issues in the database.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn count_issues(&self) -> Result<usize> {
        let row = self.conn.query_row("SELECT count(*) FROM issues")?;
        let count = row.get(0).and_then(SqliteValue::as_integer).unwrap_or(0);
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// Get all issue IDs in the database.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_all_ids(&self) -> Result<Vec<String>> {
        let rows = self.conn.query("SELECT id FROM issues ORDER BY id")?;
        Ok(rows
            .iter()
            .filter_map(|r| r.get(0).and_then(SqliteValue::as_text).map(String::from))
            .collect())
    }

    /// Get epic counts (total children, closed children) for all epics.
    ///
    /// Returns a map from epic ID to (`total_children`, `closed_children`).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn get_epic_counts(&self) -> Result<std::collections::HashMap<String, (usize, usize)>> {
        // Fetch raw rows and aggregate in Rust to avoid SUM(CASE WHEN ... THEN 1 ELSE 0 END)
        // which crashes fsqlite (it doesn't support non-column arguments in aggregate functions).
        let rows = self.conn.query(
            "SELECT
                d.depends_on_id AS epic_id,
                i.status
             FROM dependencies d
             JOIN issues i ON d.issue_id = i.id
             WHERE d.type = 'parent-child'
               AND (i.is_template = 0 OR i.is_template IS NULL)",
        )?;
        let mut counts: std::collections::HashMap<String, (usize, usize)> =
            std::collections::HashMap::new();
        for row in &rows {
            let epic_id = row
                .get(0)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            let status = row.get(1).and_then(SqliteValue::as_text).unwrap_or("");
            let entry = counts.entry(epic_id).or_insert((0, 0));
            entry.0 += 1; // total
            if status == "closed" || status == "tombstone" {
                entry.1 += 1; // closed
            }
        }
        Ok(counts)
    }

    /// Add a dependency between issues.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn add_dependency(
        &mut self,
        issue_id: &str,
        depends_on_id: &str,
        dep_type: &str,
        actor: &str,
    ) -> Result<bool> {
        self.add_dependency_with_metadata(issue_id, depends_on_id, dep_type, actor, None)
    }

    /// Add a dependency link with optional JSON metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if the dependency is invalid, the metadata is not valid JSON,
    /// or the database update fails.
    pub fn add_dependency_with_metadata(
        &mut self,
        issue_id: &str,
        depends_on_id: &str,
        dep_type: &str,
        actor: &str,
        metadata: Option<&str>,
    ) -> Result<bool> {
        if issue_id == depends_on_id {
            return Err(BeadsError::SelfDependency {
                id: issue_id.to_string(),
            });
        }

        let metadata = if let Some(metadata) = metadata {
            serde_json::from_str::<serde_json::Value>(metadata).map_err(|err| {
                BeadsError::Validation {
                    field: "metadata".to_string(),
                    reason: format!("dependency metadata must be valid JSON: {err}"),
                }
            })?;
            metadata
        } else {
            "{}"
        };

        Self::validate_parent_child_endpoints(issue_id, depends_on_id, dep_type)?;

        self.mutate("add_dependency", actor, |conn, ctx| {
            match Self::issue_status_in_tx(conn, issue_id)? {
                Some(Status::Tombstone) => {
                    return Err(BeadsError::Validation {
                        field: "issue_id".to_string(),
                        reason: format!("cannot add dependency from tombstone issue: {issue_id}"),
                    });
                }
                Some(_) => {}
                None => {
                    return Err(BeadsError::IssueNotFound {
                        id: issue_id.to_string(),
                    });
                }
            }
            Self::ensure_dependency_target_exists_in_tx(conn, depends_on_id)?;

            // Cycle check runs INSIDE the transaction (BEGIN IMMEDIATE) to
            // prevent TOCTOU races where a concurrent writer could insert an
            // edge between our check and our INSERT.
            if let Ok(dt) = dep_type.parse::<DependencyType>()
                && dt.is_blocking()
                && Self::check_cycle(conn, issue_id, depends_on_id, true)?
            {
                return Err(BeadsError::DependencyCycle {
                    path: format!(
                        "Adding dependency {issue_id} -> {depends_on_id} would create a cycle"
                    ),
                });
            }

            let existing = conn.query_with_params(
                "SELECT 1 FROM dependencies WHERE issue_id = ? AND depends_on_id = ? LIMIT 1",
                &[
                    SqliteValue::from(issue_id),
                    SqliteValue::from(depends_on_id),
                ],
            )?;
            if !existing.is_empty() {
                return Ok(false);
            }

            let inserted = conn.execute_with_params(
                "INSERT OR IGNORE INTO dependencies (issue_id, depends_on_id, type, created_at, created_by, metadata)
                 VALUES (?, ?, ?, ?, ?, ?)",
                &[
                    SqliteValue::from(issue_id),
                    SqliteValue::from(depends_on_id),
                    SqliteValue::from(dep_type),
                    SqliteValue::from(Utc::now().to_rfc3339()),
                    SqliteValue::from(actor),
                    SqliteValue::from(metadata),
                ],
            )?;

            if inserted == 0 {
                return Ok(false);
            }

            conn.execute_with_params(
                "UPDATE issues SET updated_at = ? WHERE id = ?",
                &[
                    SqliteValue::from(Utc::now().to_rfc3339()),
                    SqliteValue::from(issue_id),
                ],
            )?;

            ctx.record_event(
                EventType::DependencyAdded,
                issue_id,
                Some(format!("Added dependency on {depends_on_id} ({dep_type})")),
            );
            ctx.mark_dirty(issue_id);
            // Defer the blocked-cache rebuild to the next read rather than
            // doing it eagerly in a second write transaction.  This eliminates
            // the main DB lock contention source under concurrent agents:
            // previously even the incremental path held a second write lock
            // while traversing all parent-child edges in the graph.
            ctx.invalidate_cache_deferred();

            Ok(true)
        })
    }

    /// Remove a dependency link.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn remove_dependency(
        &mut self,
        issue_id: &str,
        depends_on_id: &str,
        actor: &str,
    ) -> Result<bool> {
        self.mutate("remove_dependency", actor, |conn, ctx| {
            let rows = conn.execute_with_params(
                "DELETE FROM dependencies WHERE issue_id = ? AND depends_on_id = ?",
                &[
                    SqliteValue::from(issue_id),
                    SqliteValue::from(depends_on_id),
                ],
            )?;

            if rows > 0 {
                conn.execute_with_params(
                    "UPDATE issues SET updated_at = ? WHERE id = ?",
                    &[
                        SqliteValue::from(Utc::now().to_rfc3339()),
                        SqliteValue::from(issue_id),
                    ],
                )?;

                ctx.record_event(
                    EventType::DependencyRemoved,
                    issue_id,
                    Some(format!("Removed dependency on {depends_on_id}")),
                );
                ctx.mark_dirty(issue_id);
                // Defer rebuild for the same reason as add_dependency_with_metadata.
                ctx.invalidate_cache_deferred();
            }

            Ok(rows > 0)
        })
    }

    /// Remove all dependencies for an issue.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn remove_all_dependencies(&mut self, issue_id: &str, actor: &str) -> Result<usize> {
        self.mutate("remove_all_dependencies", actor, |conn, ctx| {
            let affected_rows = conn.query_with_params(
                "SELECT DISTINCT issue_id FROM dependencies WHERE depends_on_id = ?",
                &[SqliteValue::from(issue_id)],
            )?;
            let affected: Vec<String> = affected_rows
                .iter()
                .filter_map(|r| r.get(0).and_then(SqliteValue::as_text).map(String::from))
                .collect();

            let outgoing = conn.execute_with_params(
                "DELETE FROM dependencies WHERE issue_id = ?",
                &[SqliteValue::from(issue_id)],
            )?;
            let incoming = conn.execute_with_params(
                "DELETE FROM dependencies WHERE depends_on_id = ?",
                &[SqliteValue::from(issue_id)],
            )?;
            let total = outgoing + incoming;

            if total > 0 {
                let now = Utc::now().to_rfc3339();

                conn.execute_with_params(
                    "UPDATE issues SET updated_at = ? WHERE id = ?",
                    &[SqliteValue::from(now.as_str()), SqliteValue::from(issue_id)],
                )?;

                for chunk in affected.chunks(400) {
                    let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
                    let sql = format!(
                        "UPDATE issues SET updated_at = ? WHERE id IN ({})",
                        placeholders.join(", ")
                    );
                    let mut params = Vec::with_capacity(chunk.len() + 1);
                    params.push(SqliteValue::from(now.as_str()));
                    for id in chunk {
                        params.push(SqliteValue::from(id.as_str()));
                    }
                    conn.execute_with_params(&sql, &params)?;
                }

                ctx.record_event(
                    EventType::DependencyRemoved,
                    issue_id,
                    Some(format!("Removed {total} dependency links")),
                );
                ctx.mark_dirty(issue_id);
                for affected_id in &affected {
                    ctx.mark_dirty(affected_id);
                }
                let mut cache_ids: Vec<&str> = Vec::with_capacity(affected.len() + 1);
                cache_ids.push(issue_id);
                cache_ids.extend(affected.iter().map(String::as_str));
                ctx.invalidate_cache_for(&cache_ids);
            }

            Ok(total)
        })
    }

    /// Remove parent-child dependency for an issue.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn remove_parent(&mut self, issue_id: &str, actor: &str) -> Result<bool> {
        self.mutate("remove_parent", actor, |conn, ctx| {
            let previous_parent = conn
                .query_with_params(
                    "SELECT depends_on_id FROM dependencies WHERE issue_id = ? AND type = 'parent-child' LIMIT 1",
                    &[SqliteValue::from(issue_id)],
                )?
                .first()
                .and_then(|row| row.get(0).and_then(SqliteValue::as_text))
                .map(str::to_string);
            let rows = conn.execute_with_params(
                "DELETE FROM dependencies WHERE issue_id = ? AND type = 'parent-child'",
                &[SqliteValue::from(issue_id)],
            )?;

            if rows > 0 {
                conn.execute_with_params(
                    "UPDATE issues SET updated_at = ? WHERE id = ?",
                    &[
                        SqliteValue::from(Utc::now().to_rfc3339()),
                        SqliteValue::from(issue_id),
                    ],
                )?;

                ctx.record_event(
                    EventType::DependencyRemoved,
                    issue_id,
                    Some("Removed parent".to_string()),
                );
                ctx.mark_dirty(issue_id);
                let mut cache_ids = vec![issue_id];
                if let Some(parent_id) = previous_parent.as_deref() {
                    cache_ids.push(parent_id);
                }
                ctx.invalidate_cache_for(&cache_ids);
            }

            Ok(rows > 0)
        })
    }

    /// Set parent for an issue (replace existing).
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails or cycle detected.
    pub fn set_parent(
        &mut self,
        issue_id: &str,
        parent_id: Option<&str>,
        actor: &str,
    ) -> Result<()> {
        self.set_parent_with_options(issue_id, parent_id, actor, false)
    }

    /// Set parent for an issue (replace existing) with optional deferred cache rebuild.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails or cycle detected.
    pub fn set_parent_with_options(
        &mut self,
        issue_id: &str,
        parent_id: Option<&str>,
        actor: &str,
        skip_cache_rebuild: bool,
    ) -> Result<()> {
        self.mutate("set_parent", actor, |conn, ctx| {
            let previous_parent = conn
                .query_with_params(
                    "SELECT depends_on_id FROM dependencies WHERE issue_id = ? AND type = 'parent-child' LIMIT 1",
                    &[SqliteValue::from(issue_id)],
                )?
                .first()
                .and_then(|row| row.get(0).and_then(SqliteValue::as_text))
                .map(str::to_string);
            // Remove existing parent
            conn.execute_with_params(
                "DELETE FROM dependencies WHERE issue_id = ? AND type = 'parent-child'",
                &[SqliteValue::from(issue_id)],
            )?;

            if let Some(pid) = parent_id {
                if pid == issue_id {
                    return Err(BeadsError::SelfDependency {
                        id: issue_id.to_string(),
                    });
                }

                Self::validate_parent_child_endpoints(issue_id, pid, "parent-child")?;
                Self::ensure_dependency_target_exists_in_tx(conn, pid)?;

                if Self::check_cycle(conn, issue_id, pid, true)? {
                    return Err(BeadsError::DependencyCycle {
                        path: format!("Setting parent of {issue_id} to {pid} would create a cycle"),
                    });
                }

                conn.execute_with_params(
                    "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by)
                     VALUES (?, ?, 'parent-child', ?, ?)",
                    &[
                        SqliteValue::from(issue_id),
                        SqliteValue::from(pid),
                        SqliteValue::from(Utc::now().to_rfc3339()),
                        SqliteValue::from(actor),
                    ],
                )?;

                ctx.record_event(
                    EventType::DependencyAdded,
                    issue_id,
                    Some(format!("Set parent to {pid}")),
                );
            } else {
                ctx.record_event(
                    EventType::DependencyRemoved,
                    issue_id,
                    Some("Removed parent".to_string()),
                );
            }

            conn.execute_with_params(
                "UPDATE issues SET updated_at = ? WHERE id = ?",
                &[
                    SqliteValue::from(Utc::now().to_rfc3339()),
                    SqliteValue::from(issue_id),
                ],
            )?;

            ctx.mark_dirty(issue_id);
            if skip_cache_rebuild {
                ctx.invalidate_cache_deferred();
            } else {
                let mut cache_ids = vec![issue_id];
                if let Some(parent_id) = previous_parent.as_deref() {
                    cache_ids.push(parent_id);
                }
                if let Some(parent_id) = parent_id {
                    cache_ids.push(parent_id);
                }
                ctx.invalidate_cache_for(&cache_ids);
            }
            Ok(())
        })
    }

    /// Add a label to an issue.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn add_label(&mut self, issue_id: &str, label: &str, actor: &str) -> Result<bool> {
        self.mutate("add_label", actor, |conn, ctx| {
            match Self::issue_status_in_tx(conn, issue_id)? {
                Some(Status::Tombstone) => {
                    return Err(BeadsError::Validation {
                        field: "issue_id".to_string(),
                        reason: format!("cannot add label to tombstone issue: {issue_id}"),
                    });
                }
                Some(_) => {}
                None => {
                    return Err(BeadsError::IssueNotFound {
                        id: issue_id.to_string(),
                    });
                }
            }

            let row = conn.query_row_with_params(
                "SELECT count(*) FROM labels WHERE issue_id = ? AND label = ?",
                &[SqliteValue::from(issue_id), SqliteValue::from(label)],
            )?;
            let exists = row.get(0).and_then(SqliteValue::as_integer).unwrap_or(0);

            if exists > 0 {
                return Ok(false);
            }

            conn.execute_with_params(
                "INSERT INTO labels (issue_id, label) VALUES (?, ?)",
                &[SqliteValue::from(issue_id), SqliteValue::from(label)],
            )?;

            ctx.record_event(
                EventType::LabelAdded,
                issue_id,
                Some(format!("Added label {label}")),
            );
            ctx.mark_dirty(issue_id);

            conn.execute_with_params(
                "UPDATE issues SET updated_at = ? WHERE id = ?",
                &[
                    SqliteValue::from(Utc::now().to_rfc3339()),
                    SqliteValue::from(issue_id),
                ],
            )?;

            Ok(true)
        })
    }

    /// Remove a label from an issue.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn remove_label(&mut self, issue_id: &str, label: &str, actor: &str) -> Result<bool> {
        self.mutate("remove_label", actor, |conn, ctx| {
            match Self::issue_status_in_tx(conn, issue_id)? {
                Some(Status::Tombstone) => {
                    return Err(BeadsError::Validation {
                        field: "issue_id".to_string(),
                        reason: format!("cannot remove label from tombstone issue: {issue_id}"),
                    });
                }
                Some(_) => {}
                None => {
                    return Err(BeadsError::IssueNotFound {
                        id: issue_id.to_string(),
                    });
                }
            }

            let rows = conn.execute_with_params(
                "DELETE FROM labels WHERE issue_id = ? AND label = ?",
                &[SqliteValue::from(issue_id), SqliteValue::from(label)],
            )?;

            if rows > 0 {
                conn.execute_with_params(
                    "UPDATE issues SET updated_at = ? WHERE id = ?",
                    &[
                        SqliteValue::from(Utc::now().to_rfc3339()),
                        SqliteValue::from(issue_id),
                    ],
                )?;

                ctx.record_event(
                    EventType::LabelRemoved,
                    issue_id,
                    Some(format!("Removed label {label}")),
                );
                ctx.mark_dirty(issue_id);
            }

            Ok(rows > 0)
        })
    }

    /// Remove all labels from an issue.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn remove_all_labels(&mut self, issue_id: &str, actor: &str) -> Result<usize> {
        self.mutate("remove_all_labels", actor, |conn, ctx| {
            let rows = conn.execute_with_params(
                "DELETE FROM labels WHERE issue_id = ?",
                &[SqliteValue::from(issue_id)],
            )?;

            if rows > 0 {
                conn.execute_with_params(
                    "UPDATE issues SET updated_at = ? WHERE id = ?",
                    &[
                        SqliteValue::from(Utc::now().to_rfc3339()),
                        SqliteValue::from(issue_id),
                    ],
                )?;

                ctx.record_event(
                    EventType::LabelRemoved,
                    issue_id,
                    Some(format!("Removed {rows} labels")),
                );
                ctx.mark_dirty(issue_id);
            }

            Ok(rows)
        })
    }

    /// Set all labels for an issue (replace existing).
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn set_labels(&mut self, issue_id: &str, labels: &[String], actor: &str) -> Result<()> {
        self.mutate("set_labels", actor, |conn, ctx| {
            let old_rows = conn.query_with_params(
                "SELECT label FROM labels WHERE issue_id = ?",
                &[SqliteValue::from(issue_id)],
            )?;
            let old_labels_raw: Vec<String> = old_rows
                .iter()
                .filter_map(|r| r.get(0).and_then(SqliteValue::as_text).map(String::from))
                .collect();
            let old_labels = dedupe_preserving_order(&old_labels_raw);
            let desired_labels = dedupe_preserving_order(labels);

            let old_matches_desired = old_labels.len() == desired_labels.len()
                && old_labels
                    .iter()
                    .all(|label| desired_labels.contains(label));
            let db_has_duplicate_labels = old_labels_raw.len() != old_labels.len();

            if old_matches_desired && !db_has_duplicate_labels {
                return Ok(());
            }

            conn.execute_with_params(
                "DELETE FROM labels WHERE issue_id = ?",
                &[SqliteValue::from(issue_id)],
            )?;

            let mut seen_labels = HashSet::new();
            for label in &desired_labels {
                if !seen_labels.insert(label.as_str()) {
                    continue;
                }
                conn.execute_with_params(
                    "INSERT INTO labels (issue_id, label) VALUES (?, ?)",
                    &[
                        SqliteValue::from(issue_id),
                        SqliteValue::from(label.as_str()),
                    ],
                )?;
            }

            // Record changes
            let removed: Vec<_> = old_labels
                .iter()
                .filter(|label| !desired_labels.contains(label))
                .collect();
            let added: Vec<_> = desired_labels
                .iter()
                .filter(|label| !old_labels.contains(label))
                .collect();

            if !removed.is_empty() || !added.is_empty() || db_has_duplicate_labels {
                let mut details = Vec::new();
                if !removed.is_empty() {
                    details.push(format!(
                        "removed: {}",
                        removed
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                if !added.is_empty() {
                    details.push(format!(
                        "added: {}",
                        added
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                if db_has_duplicate_labels && removed.is_empty() && added.is_empty() {
                    details.push("normalized duplicate labels".to_string());
                }
                ctx.record_event(
                    EventType::Updated,
                    issue_id,
                    Some(format!("Labels {}", details.join("; "))),
                );
                ctx.mark_dirty(issue_id);

                conn.execute_with_params(
                    "UPDATE issues SET updated_at = ? WHERE id = ?",
                    &[
                        SqliteValue::from(Utc::now().to_rfc3339()),
                        SqliteValue::from(issue_id),
                    ],
                )?;
            }

            Ok(())
        })
    }

    /// Get labels for an issue.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_labels(&self, issue_id: &str) -> Result<Vec<String>> {
        let rows = self.conn.query_with_params(
            "SELECT label FROM labels WHERE issue_id = ? ORDER BY label",
            &[SqliteValue::from(issue_id)],
        )?;
        Ok(rows
            .iter()
            .filter_map(|r| r.get(0).and_then(SqliteValue::as_text).map(String::from))
            .collect())
    }

    /// Get labels for multiple issues efficiently.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_labels_for_issues(
        &self,
        issue_ids: &[String],
    ) -> Result<HashMap<String, Vec<String>>> {
        const SQLITE_VAR_LIMIT: usize = 900;

        if issue_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let mut map: HashMap<String, Vec<String>> = HashMap::new();

        // SQLite has a finite variable limit (default 999). Chunk to avoid query failures.
        for chunk in issue_ids.chunks(SQLITE_VAR_LIMIT) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            let sql = format!(
                "SELECT issue_id, label FROM labels WHERE issue_id IN ({}) ORDER BY issue_id, label",
                placeholders.join(",")
            );

            let params: Vec<SqliteValue> = chunk
                .iter()
                .map(|s| SqliteValue::from(s.as_str()))
                .collect();

            let rows = self.conn.query_with_params(&sql, &params)?;
            for row in &rows {
                let issue_id = row
                    .get(0)
                    .and_then(SqliteValue::as_text)
                    .unwrap_or("")
                    .to_string();
                let label = row
                    .get(1)
                    .and_then(SqliteValue::as_text)
                    .unwrap_or("")
                    .to_string();
                map.entry(issue_id).or_default().push(label);
            }
        }

        Ok(map)
    }

    /// Get all labels for all issues as a map of issue_id -> labels.
    ///
    /// Used for export and sync operations that need complete label state.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_all_labels(&self) -> Result<HashMap<String, Vec<String>>> {
        let rows = self
            .conn
            .query("SELECT issue_id, label FROM labels ORDER BY issue_id, label")?;

        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for row in &rows {
            let issue_id = row
                .get(0)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            let label = row
                .get(1)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            map.entry(issue_id).or_default().push(label);
        }
        Ok(map)
    }

    /// Get all unique labels with their issue counts.
    ///
    /// Returns a vector of (label, count) pairs sorted alphabetically by label.
    /// Excludes labels on tombstoned (deleted) issues.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_unique_labels_with_counts(&self) -> Result<Vec<(String, i64)>> {
        let rows = self.conn.query(
            r"SELECT l.label, COUNT(*) as count
              FROM labels l
              JOIN issues i ON l.issue_id = i.id
              WHERE i.status != 'tombstone'
              GROUP BY l.label
              ORDER BY l.label",
        )?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let label = r.get(0).and_then(SqliteValue::as_text)?.to_string();
                let count = r.get(1).and_then(SqliteValue::as_integer)?;
                Some((label, count))
            })
            .collect())
    }

    /// Rename a label across all issues.
    ///
    /// Returns the number of issues affected.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn rename_label(&mut self, old_name: &str, new_name: &str, actor: &str) -> Result<usize> {
        if old_name == new_name {
            return Ok(0);
        }

        self.mutate("rename_label", actor, |conn, ctx| {
            let id_rows = conn.query_with_params(
                "SELECT issue_id FROM labels WHERE label = ?",
                &[SqliteValue::from(old_name)],
            )?;
            let issue_ids: Vec<String> = id_rows
                .iter()
                .filter_map(|r| r.get(0).and_then(SqliteValue::as_text).map(String::from))
                .collect();

            let conflict_rows = conn.query_with_params(
                "SELECT issue_id FROM labels WHERE label = ? AND issue_id IN (SELECT issue_id FROM labels WHERE label = ?)",
                &[SqliteValue::from(new_name), SqliteValue::from(old_name)],
            )?;
            let conflicts: Vec<String> = conflict_rows
                .iter()
                .filter_map(|r| r.get(0).and_then(SqliteValue::as_text).map(String::from))
                .collect();

            for conflict_id in &conflicts {
                conn.execute_with_params(
                    "DELETE FROM labels WHERE issue_id = ? AND label = ?",
                    &[SqliteValue::from(conflict_id.as_str()), SqliteValue::from(old_name)],
                )?;
                ctx.mark_dirty(conflict_id);
            }

            let renamed = conn.execute_with_params(
                "UPDATE labels SET label = ? WHERE label = ?",
                &[SqliteValue::from(new_name), SqliteValue::from(old_name)],
            )?;

            let now = Utc::now().to_rfc3339();
            for issue_id in &issue_ids {
                ctx.record_event(
                    EventType::LabelRemoved,
                    issue_id,
                    Some(format!("Renamed label {old_name} to {new_name}")),
                );
                ctx.mark_dirty(issue_id);

                conn.execute_with_params(
                    "UPDATE issues SET updated_at = ? WHERE id = ?",
                    &[SqliteValue::from(now.as_str()), SqliteValue::from(issue_id.as_str())],
                )?;
            }

            Ok(renamed + conflicts.len())
        })
    }

    /// Get comments for an issue.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_comments(&self, issue_id: &str) -> Result<Vec<Comment>> {
        let rows = self.conn.query_with_params(
            "SELECT id, issue_id, author, text, created_at
             FROM comments
             WHERE issue_id = ?
             ORDER BY created_at ASC",
            &[SqliteValue::from(issue_id)],
        )?;

        rows.iter().map(comment_from_row).collect()
    }

    /// Get comments for multiple issues in batch.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_comments_for_issues(
        &self,
        issue_ids: &[String],
    ) -> Result<std::collections::HashMap<String, Vec<Comment>>> {
        const SQLITE_VAR_LIMIT: usize = 900;
        let mut map: std::collections::HashMap<String, Vec<Comment>> =
            std::collections::HashMap::new();

        if issue_ids.is_empty() {
            return Ok(map);
        }

        for chunk in issue_ids.chunks(SQLITE_VAR_LIMIT) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            let sql = format!(
                "SELECT id, issue_id, author, text, created_at
                 FROM comments
                 WHERE issue_id IN ({})
                 ORDER BY created_at ASC",
                placeholders.join(",")
            );

            let params: Vec<SqliteValue> = chunk
                .iter()
                .map(|id| SqliteValue::from(id.as_str()))
                .collect();

            let rows = self.conn.query_with_params(&sql, &params)?;

            for row in &rows {
                let comment = comment_from_row(row)?;
                map.entry(comment.issue_id.clone())
                    .or_default()
                    .push(comment);
            }
        }

        Ok(map)
    }

    /// Count how many audit events belong to an issue.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn count_issue_events(&self, issue_id: &str) -> Result<usize> {
        let count = self
            .conn
            .query_row_with_params(
                "SELECT count(*) FROM events WHERE issue_id = ?",
                &[SqliteValue::from(issue_id)],
            )?
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap_or(0);
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// Add a comment to an issue.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn add_comment(&mut self, issue_id: &str, author: &str, text: &str) -> Result<Comment> {
        if text.trim().is_empty() {
            return Err(crate::error::BeadsError::validation(
                "comment",
                "comment text cannot be empty",
            ));
        }
        self.mutate("add_comment", author, |conn, ctx| {
            let comment_id = insert_comment_row(conn, issue_id, author, text)?;

            conn.execute_with_params(
                "UPDATE issues SET updated_at = ? WHERE id = ?",
                &[
                    SqliteValue::from(Utc::now().to_rfc3339()),
                    SqliteValue::from(issue_id),
                ],
            )?;

            ctx.record_event(EventType::Commented, issue_id, Some(text.to_string()));
            ctx.mark_dirty(issue_id);

            fetch_comment(conn, comment_id)
        })
    }

    /// Get dependencies with metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_dependencies_with_metadata(
        &self,
        issue_id: &str,
    ) -> Result<Vec<IssueWithDependencyMetadata>> {
        let rows = self.conn.query_with_params(
            "SELECT d.depends_on_id, i.title, i.status, i.priority, d.type, i.created_at
             FROM dependencies d
             LEFT JOIN issues i ON d.depends_on_id = i.id
             WHERE d.issue_id = ?
            ORDER BY i.priority ASC, i.created_at DESC",
            &[SqliteValue::from(issue_id)],
        )?;

        rows.iter()
            .map(|row| dependency_metadata_from_row(row, "dependency target", true))
            .collect()
    }

    /// Get dependents with metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_dependents_with_metadata(
        &self,
        issue_id: &str,
    ) -> Result<Vec<IssueWithDependencyMetadata>> {
        let rows = self.conn.query_with_params(
            "SELECT d.issue_id, i.title, i.status, i.priority, d.type, i.created_at
             FROM dependencies d
             LEFT JOIN issues i ON d.issue_id = i.id
             WHERE d.depends_on_id = ?
            ORDER BY i.priority ASC, i.created_at DESC",
            &[SqliteValue::from(issue_id)],
        )?;

        rows.iter()
            .map(|row| dependency_metadata_from_row(row, "dependent issue", false))
            .collect()
    }

    /// Get parent issue ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_parent_id(&self, issue_id: &str) -> Result<Option<String>> {
        match self.conn.query_row_with_params(
            "SELECT depends_on_id FROM dependencies WHERE issue_id = ? AND type = 'parent-child' ORDER BY rowid DESC LIMIT 1",
            &[SqliteValue::from(issue_id)],
        ) {
            Ok(row) => Ok(row.get(0).and_then(SqliteValue::as_text).map(String::from)),
            Err(fsqlite_error::FrankenError::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Collect all descendant issue IDs via BFS through parent-child edges.
    ///
    /// # Errors
    ///
    /// Returns an error if a database query fails.
    fn collect_descendant_ids(&self, parent_id: &str) -> Result<Vec<String>> {
        let mut result = Vec::new();
        // Use a HashSet for O(1) visited-set lookups instead of the
        // previous Vec::contains() which was O(n) per check (O(n^2) total).
        let mut visited = std::collections::HashSet::new();
        let mut queue = std::collections::VecDeque::new();
        queue.push_back(parent_id.to_string());
        while let Some(pid) = queue.pop_front() {
            let rows = self.conn.query_with_params(
                "SELECT issue_id FROM dependencies WHERE depends_on_id = ? AND type = 'parent-child'",
                &[SqliteValue::from(pid.as_str())],
            )?;
            for row in &rows {
                if let Some(id) = row.get(0).and_then(SqliteValue::as_text) {
                    let id = id.to_string();
                    if visited.insert(id.clone()) {
                        queue.push_back(id.clone());
                        result.push(id);
                    }
                }
            }
        }
        Ok(result)
    }

    /// Get IDs of issues that depend on this one.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_dependents(&self, issue_id: &str) -> Result<Vec<String>> {
        let rows = self.conn.query_with_params(
            "SELECT issue_id FROM dependencies WHERE depends_on_id = ?",
            &[SqliteValue::from(issue_id)],
        )?;
        Ok(rows
            .iter()
            .filter_map(|r| r.get(0).and_then(SqliteValue::as_text).map(String::from))
            .collect())
    }

    /// Get IDs of issues that block this one (respects parent-child direction).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_blocker_ids(&self, issue_id: &str) -> Result<Vec<String>> {
        let rows = self.conn.query_with_params(
            r"
            SELECT depends_on_id
            FROM dependencies
            WHERE issue_id = ?
              AND type IN ('blocks', 'conditional-blocks', 'waits-for')
            UNION
            SELECT issue_id FROM dependencies WHERE depends_on_id = ? AND type = 'parent-child'
            ",
            &[SqliteValue::from(issue_id), SqliteValue::from(issue_id)],
        )?;
        Ok(rows
            .iter()
            .filter_map(|r| r.get(0).and_then(SqliteValue::as_text).map(String::from))
            .collect())
    }

    /// Get IDs of issues that are blocked by this one (respects parent-child direction).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_blocked_issue_ids(&self, issue_id: &str) -> Result<Vec<String>> {
        let rows = self.conn.query_with_params(
            r"
            SELECT issue_id
            FROM dependencies
            WHERE depends_on_id = ?
              AND type IN ('blocks', 'conditional-blocks', 'waits-for')
            UNION
            SELECT depends_on_id FROM dependencies WHERE issue_id = ? AND type = 'parent-child'
            ",
            &[SqliteValue::from(issue_id), SqliteValue::from(issue_id)],
        )?;
        Ok(rows
            .iter()
            .filter_map(|r| r.get(0).and_then(SqliteValue::as_text).map(String::from))
            .collect())
    }

    /// Get IDs of issues that this one depends on.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_dependencies(&self, issue_id: &str) -> Result<Vec<String>> {
        let rows = self.conn.query_with_params(
            "SELECT depends_on_id FROM dependencies WHERE issue_id = ?",
            &[SqliteValue::from(issue_id)],
        )?;
        Ok(rows
            .iter()
            .filter_map(|r| r.get(0).and_then(SqliteValue::as_text).map(String::from))
            .collect())
    }

    /// Count how many dependencies an issue has.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    pub fn count_dependencies(&self, issue_id: &str) -> Result<usize> {
        let row = self.conn.query_row_with_params(
            "SELECT count(*) FROM dependencies WHERE issue_id = ?",
            &[SqliteValue::from(issue_id)],
        )?;
        let count = row.get(0).and_then(SqliteValue::as_integer).unwrap_or(0);
        Ok(count as usize)
    }

    /// Count how many issues depend on this one.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    pub fn count_dependents(&self, issue_id: &str) -> Result<usize> {
        let row = self.conn.query_row_with_params(
            "SELECT count(*) FROM dependencies WHERE depends_on_id = ?",
            &[SqliteValue::from(issue_id)],
        )?;
        let count = row.get(0).and_then(SqliteValue::as_integer).unwrap_or(0);
        Ok(count as usize)
    }

    /// Find the next available child number for a parent issue.
    ///
    /// Looks for existing issues with IDs like `{parent_id}.N` and returns the next
    /// available number. For example, if `bd-abc.1` and `bd-abc.2` exist, returns 3.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn next_child_number(&self, parent_id: &str) -> Result<u32> {
        // First, check the child_counters table (source of truth)
        match self.conn.query_row_with_params(
            "SELECT last_child FROM child_counters WHERE parent_id = ?",
            &[SqliteValue::from(parent_id)],
        ) {
            Ok(row) => {
                if let Some(last_child) = row.get(0).and_then(SqliteValue::as_integer) {
                    return Ok(u32::try_from(last_child).unwrap_or(0).saturating_add(1));
                }
            }
            Err(fsqlite_error::FrankenError::QueryReturnedNoRows) => {}
            Err(e) => return Err(e.into()),
        }

        // Fallback: Scan issues table for legacy data or missing counter
        // Find all existing child IDs matching the pattern {parent_id}.N
        // Escape LIKE wildcards in parent_id to prevent injection
        let escaped_parent = escape_like_pattern(parent_id);
        let pattern = format!("{escaped_parent}.%");
        let ids_rows = self.conn.query_with_params(
            "SELECT id FROM issues WHERE id LIKE ? ESCAPE '\\'",
            &[SqliteValue::from(pattern.as_str())],
        )?;
        let ids: Vec<String> = ids_rows
            .iter()
            .filter_map(|r| r.get(0).and_then(SqliteValue::as_text).map(String::from))
            .collect();

        // Extract child numbers and find the maximum
        let prefix_with_dot = format!("{parent_id}.");
        let max_child = ids
            .iter()
            .filter_map(|id| {
                id.strip_prefix(&prefix_with_dot)
                    .and_then(|suffix| {
                        // Handle both simple children (parent.1) and nested (parent.1.2)
                        // We only care about direct children, so take the first segment
                        suffix.split('.').next()
                    })
                    .and_then(|num_str| num_str.parse::<u32>().ok())
            })
            .max()
            .unwrap_or(0);

        // Use saturating_add to prevent overflow (extremely unlikely but safe)
        Ok(max_child.saturating_add(1))
    }

    /// Internal helper to update a child counter within a transaction.
    fn update_child_counter_in_tx(
        conn: &Connection,
        parent_id: &str,
        child_number: u32,
    ) -> Result<()> {
        // Check current value
        let current_max = match conn.query_row_with_params(
            "SELECT last_child FROM child_counters WHERE parent_id = ?",
            &[SqliteValue::from(parent_id)],
        ) {
            Ok(row) => row.get(0).and_then(SqliteValue::as_integer).unwrap_or(0),
            Err(fsqlite_error::FrankenError::QueryReturnedNoRows) => 0,
            Err(e) => return Err(e.into()),
        };

        if i64::from(child_number) > current_max {
            // DELETE + INSERT to simulate UPSERT (fsqlite limitation)
            conn.execute_with_params(
                "DELETE FROM child_counters WHERE parent_id = ?",
                &[SqliteValue::from(parent_id)],
            )?;
            conn.execute_with_params(
                "INSERT INTO child_counters (parent_id, last_child) VALUES (?, ?)",
                &[
                    SqliteValue::from(parent_id),
                    SqliteValue::from(i64::from(child_number)),
                ],
            )?;
        }

        Ok(())
    }

    /// Count dependencies for multiple issues efficiently.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn count_dependencies_for_issues(
        &self,
        issue_ids: &[String],
    ) -> Result<HashMap<String, usize>> {
        const SQLITE_VAR_LIMIT: usize = 900;

        if issue_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let mut map: HashMap<String, usize> = HashMap::new();

        for chunk in issue_ids.chunks(SQLITE_VAR_LIMIT) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            let sql = format!(
                "SELECT issue_id, COUNT(*) FROM dependencies WHERE issue_id IN ({}) GROUP BY issue_id",
                placeholders.join(",")
            );

            let params: Vec<SqliteValue> = chunk
                .iter()
                .map(|s| SqliteValue::from(s.as_str()))
                .collect();

            let rows = self.conn.query_with_params(&sql, &params)?;
            for row in &rows {
                let issue_id = row
                    .get(0)
                    .and_then(SqliteValue::as_text)
                    .unwrap_or("")
                    .to_string();
                let count = row.get(1).and_then(SqliteValue::as_integer).unwrap_or(0);
                map.insert(issue_id, usize::try_from(count).unwrap_or(0));
            }
        }

        Ok(map)
    }

    /// Count dependencies and dependents for multiple issues with one round-trip per chunk.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn count_relation_counts_for_issues(
        &self,
        issue_ids: &[String],
    ) -> Result<(HashMap<String, usize>, HashMap<String, usize>)> {
        const SQLITE_VAR_LIMIT: usize = 900;

        if issue_ids.is_empty() {
            return Ok((HashMap::new(), HashMap::new()));
        }

        let mut dependency_counts: HashMap<String, usize> = HashMap::new();
        let mut dependent_counts: HashMap<String, usize> = HashMap::new();

        for chunk in issue_ids.chunks(SQLITE_VAR_LIMIT) {
            let value_rows: Vec<&str> = chunk.iter().map(|_| "(?)").collect();
            let sql = format!(
                r"WITH target_ids(id) AS (VALUES {}),
                  counts AS (
                      SELECT issue_id AS id, COUNT(*) AS dependency_count, 0 AS dependent_count
                      FROM dependencies
                      WHERE issue_id IN (SELECT id FROM target_ids)
                      GROUP BY issue_id
                      UNION ALL
                      SELECT depends_on_id AS id, 0 AS dependency_count, COUNT(*) AS dependent_count
                      FROM dependencies
                      WHERE depends_on_id IN (SELECT id FROM target_ids)
                      GROUP BY depends_on_id
                  )
                  SELECT target_ids.id,
                         COALESCE(SUM(counts.dependency_count), 0),
                         COALESCE(SUM(counts.dependent_count), 0)
                  FROM target_ids
                  LEFT JOIN counts ON counts.id = target_ids.id
                  GROUP BY target_ids.id",
                value_rows.join(",")
            );

            let params: Vec<SqliteValue> = chunk
                .iter()
                .map(|issue_id| SqliteValue::from(issue_id.as_str()))
                .collect();

            let rows = self.conn.query_with_params(&sql, &params)?;
            for row in &rows {
                let issue_id = row
                    .get(0)
                    .and_then(SqliteValue::as_text)
                    .unwrap_or("")
                    .to_string();
                let dependency_count = row.get(1).and_then(SqliteValue::as_integer).unwrap_or(0);
                let dependent_count = row.get(2).and_then(SqliteValue::as_integer).unwrap_or(0);

                if dependency_count > 0 {
                    dependency_counts.insert(
                        issue_id.clone(),
                        usize::try_from(dependency_count).unwrap_or(0),
                    );
                }
                if dependent_count > 0 {
                    dependent_counts
                        .insert(issue_id, usize::try_from(dependent_count).unwrap_or(0));
                }
            }
        }

        Ok((dependency_counts, dependent_counts))
    }

    /// Count dependents for multiple issues efficiently.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn count_dependents_for_issues(
        &self,
        issue_ids: &[String],
    ) -> Result<HashMap<String, usize>> {
        const SQLITE_VAR_LIMIT: usize = 900;

        if issue_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let mut map: HashMap<String, usize> = HashMap::new();

        for chunk in issue_ids.chunks(SQLITE_VAR_LIMIT) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            let sql = format!(
                "SELECT depends_on_id, COUNT(*) FROM dependencies WHERE depends_on_id IN ({}) GROUP BY depends_on_id",
                placeholders.join(",")
            );

            let params: Vec<SqliteValue> = chunk
                .iter()
                .map(|s| SqliteValue::from(s.as_str()))
                .collect();

            let rows = self.conn.query_with_params(&sql, &params)?;
            for row in &rows {
                let issue_id = row
                    .get(0)
                    .and_then(SqliteValue::as_text)
                    .unwrap_or("")
                    .to_string();
                let count = row.get(1).and_then(SqliteValue::as_integer).unwrap_or(0);
                map.insert(issue_id, usize::try_from(count).unwrap_or(0));
            }
        }

        Ok(map)
    }

    /// Fetch a config value.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_config(&self, key: &str) -> Result<Option<String>> {
        match self.conn.query_row_with_params(
            "SELECT value FROM config WHERE key = ?",
            &[SqliteValue::from(key)],
        ) {
            Ok(row) => Ok(row.get(0).and_then(SqliteValue::as_text).map(String::from)),
            Err(fsqlite_error::FrankenError::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Fetch all config values from the config table.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_all_config(&self) -> Result<HashMap<String, String>> {
        let rows = self.conn.query("SELECT key, value FROM config")?;

        let mut map = HashMap::new();
        for row in &rows {
            let key = row
                .get(0)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            let value = row
                .get(1)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            map.insert(key, value);
        }
        Ok(map)
    }

    /// Set a config value.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn set_config(&mut self, key: &str, value: &str) -> Result<()> {
        self.with_write_transaction(|storage| {
            storage.conn.execute_with_params(
                "DELETE FROM config WHERE key = ?",
                &[SqliteValue::from(key)],
            )?;
            storage.conn.execute_with_params(
                "INSERT INTO config (key, value) VALUES (?, ?)",
                &[SqliteValue::from(key), SqliteValue::from(value)],
            )?;
            Ok(())
        })
    }

    /// Delete a config value.
    ///
    /// Returns `true` if a value was deleted, `false` if the key didn't exist.
    ///
    /// # Errors
    ///
    /// Returns an error if the database delete fails.
    pub fn delete_config(&mut self, key: &str) -> Result<bool> {
        let deleted = self.conn.execute_with_params(
            "DELETE FROM config WHERE key = ?",
            &[SqliteValue::from(key)],
        )?;
        Ok(deleted > 0)
    }

    // ========================================================================
    // Export-related methods
    // ========================================================================

    /// Get all issues for JSONL export.
    ///
    /// Includes tombstones (for sync propagation), excludes ephemerals and wisps.
    /// Returns issues sorted by ID for deterministic output.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_all_issues_for_export(&self) -> Result<Vec<Issue>> {
        let sql = r"SELECT id, content_hash, title, description, design, acceptance_criteria, notes,
                           status, priority, issue_type, assignee, owner, estimated_minutes,
                           created_at, created_by, updated_at, closed_at, close_reason, closed_by_session,
                           due_at, defer_until, external_ref, source_system, source_repo,
                           deleted_at, deleted_by, delete_reason, original_type, compaction_level,
                           compacted_at, compacted_at_commit, original_size, sender, ephemeral,
                           pinned, is_template
                    FROM issues
                    WHERE (ephemeral = 0 OR ephemeral IS NULL)
                      AND id NOT LIKE '%-wisp-%'
                    ORDER BY id ASC";

        let rows = self.conn.query(sql)?;
        let mut issues = Vec::with_capacity(rows.len());
        for row in &rows {
            issues.push(Self::issue_from_row(row)?);
        }

        Ok(issues)
    }

    /// Get all dependency records for all issues.
    ///
    /// Returns a map from `issue_id` to its list of Dependency records.
    /// This avoids N+1 queries when populating issues for export.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_all_dependency_records(
        &self,
    ) -> Result<HashMap<String, Vec<crate::model::Dependency>>> {
        use crate::model::{Dependency, DependencyType};

        let rows = self.conn.query(
            "SELECT issue_id, depends_on_id, type, created_at, created_by, metadata, thread_id
             FROM dependencies
             ORDER BY issue_id, depends_on_id",
        )?;

        let mut map: HashMap<String, Vec<Dependency>> = HashMap::new();
        for row in &rows {
            let issue_id = row
                .get(0)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            let dep = Dependency {
                issue_id: issue_id.clone(),
                depends_on_id: row
                    .get(1)
                    .and_then(SqliteValue::as_text)
                    .unwrap_or("")
                    .to_string(),
                dep_type: row
                    .get(2)
                    .and_then(SqliteValue::as_text)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(DependencyType::Blocks),
                created_at: parse_datetime(
                    row.get(3).and_then(SqliteValue::as_text).unwrap_or(""),
                )?,
                created_by: row.get(4).and_then(SqliteValue::as_text).map(String::from),
                metadata: row.get(5).and_then(SqliteValue::as_text).map(String::from),
                thread_id: row.get(6).and_then(SqliteValue::as_text).map(String::from),
            };
            map.entry(issue_id).or_default().push(dep);
        }
        Ok(map)
    }

    /// Get all comments for all issues.
    ///
    /// Returns a map from `issue_id` to its list of comments.
    /// This avoids N+1 queries when populating issues for export.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_all_comments(&self) -> Result<HashMap<String, Vec<Comment>>> {
        let rows = self.conn.query(
            "SELECT id, issue_id, author, text, created_at
             FROM comments
             ORDER BY issue_id, created_at ASC",
        )?;

        let mut map: HashMap<String, Vec<Comment>> = HashMap::new();
        for row in &rows {
            let comment = comment_from_row(row)?;
            map.entry(comment.issue_id.clone())
                .or_default()
                .push(comment);
        }
        Ok(map)
    }

    /// Get the count of dirty issues (issues modified since last export).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_dirty_issue_count(&self) -> Result<usize> {
        let row = self.conn.query_row("SELECT COUNT(*) FROM dirty_issues")?;
        let count = row.get(0).and_then(SqliteValue::as_integer).unwrap_or(0);
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// Get the IDs and timestamps of dirty issues.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_dirty_issue_metadata(&self) -> Result<Vec<(String, String)>> {
        let rows = self
            .conn
            .query("SELECT issue_id, marked_at FROM dirty_issues ORDER BY marked_at")?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let id = r.get(0).and_then(SqliteValue::as_text).map(String::from)?;
                let marked_at = r.get(1).and_then(SqliteValue::as_text).map(String::from)?;
                Some((id, marked_at))
            })
            .collect())
    }

    /// Get IDs of all dirty issues (issues modified since last export).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_dirty_issue_ids(&self) -> Result<Vec<String>> {
        let rows = self
            .conn
            .query("SELECT issue_id FROM dirty_issues ORDER BY marked_at")?;
        Ok(rows
            .iter()
            .filter_map(|r| r.get(0).and_then(SqliteValue::as_text).map(String::from))
            .collect())
    }

    /// Clear dirty flags for the given issue IDs and timestamps.
    ///
    /// This is a safe version that only deletes if the timestamp matches,
    /// preventing a race condition where a concurrent update during export
    /// would otherwise have its dirty flag cleared incorrectly.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn clear_dirty_issues(&self, metadata: &[(String, String)]) -> Result<usize> {
        if metadata.is_empty() {
            return Ok(0);
        }

        let mut total_deleted = 0;
        for (id, marked_at) in metadata {
            let count = self.conn.execute_with_params(
                "DELETE FROM dirty_issues WHERE issue_id = ? AND marked_at = ?",
                &[
                    SqliteValue::from(id.as_str()),
                    SqliteValue::from(marked_at.as_str()),
                ],
            )?;
            total_deleted += count;
        }

        Ok(total_deleted)
    }

    /// Clear dirty flags for the given issue IDs WITHOUT timestamp validation (Legacy).
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn clear_dirty_issues_legacy(&mut self, issue_ids: &[String]) -> Result<usize> {
        const SQLITE_VAR_LIMIT: usize = 900;
        if issue_ids.is_empty() {
            return Ok(0);
        }

        let mut total_deleted = 0;
        for chunk in issue_ids.chunks(SQLITE_VAR_LIMIT) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            let sql = format!(
                "DELETE FROM dirty_issues WHERE issue_id IN ({})",
                placeholders.join(",")
            );

            let params: Vec<SqliteValue> = chunk
                .iter()
                .map(|s| SqliteValue::from(s.as_str()))
                .collect();

            let count = self.conn.execute_with_params(&sql, &params)?;
            total_deleted += count;
        }

        Ok(total_deleted)
    }

    /// Clear all dirty flags.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn clear_all_dirty_issues(&mut self) -> Result<usize> {
        let count = self.conn.execute("DELETE FROM dirty_issues")?;
        Ok(count)
    }

    // =========================================================================
    // Export Hashes (for incremental export)
    // =========================================================================

    /// Get the stored export hash for an issue.
    ///
    /// Returns the content hash and exported timestamp if the issue has been exported.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_export_hash(&self, issue_id: &str) -> Result<Option<(String, String)>> {
        match self.conn.query_row_with_params(
            "SELECT content_hash, exported_at FROM export_hashes WHERE issue_id = ?",
            &[SqliteValue::from(issue_id)],
        ) {
            Ok(row) => {
                let hash = row
                    .get(0)
                    .and_then(SqliteValue::as_text)
                    .unwrap_or("")
                    .to_string();
                let exported = row
                    .get(1)
                    .and_then(SqliteValue::as_text)
                    .unwrap_or("")
                    .to_string();
                Ok(Some((hash, exported)))
            }
            Err(fsqlite_error::FrankenError::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(BeadsError::Database(e)),
        }
    }

    /// Set the export hash for an issue after successful export.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn set_export_hash(&mut self, issue_id: &str, content_hash: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.with_write_transaction(|storage| {
            storage.conn.execute_with_params(
                "DELETE FROM export_hashes WHERE issue_id = ?",
                &[SqliteValue::from(issue_id)],
            )?;
            storage.conn.execute_with_params(
                "INSERT INTO export_hashes (issue_id, content_hash, exported_at) VALUES (?, ?, ?)",
                &[
                    SqliteValue::from(issue_id),
                    SqliteValue::from(content_hash),
                    SqliteValue::from(now.as_str()),
                ],
            )?;
            Ok(())
        })
    }

    /// Batch set export hashes for multiple issues after successful export.
    ///
    /// More efficient than calling `set_export_hash` in a loop.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub fn set_export_hashes(&mut self, exports: &[(String, String)]) -> Result<usize> {
        if exports.is_empty() {
            return Ok(0);
        }

        self.with_write_transaction(|storage| storage.set_export_hashes_in_tx(exports))
    }

    /// Clear all export hashes.
    ///
    /// Call this before import to ensure fresh state.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn clear_all_export_hashes(&mut self) -> Result<usize> {
        let count = self.conn.execute("DELETE FROM export_hashes")?;
        Ok(count)
    }

    /// Get issues that need to be exported (dirty issues whose content hash differs from stored export hash).
    ///
    /// This enables incremental export by filtering out issues that haven't actually changed
    /// since the last export, even if they were marked dirty.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_issues_needing_export(&self, dirty_ids: &[String]) -> Result<Vec<String>> {
        const SQLITE_VAR_LIMIT: usize = 900;
        if dirty_ids.is_empty() {
            return Ok(vec![]);
        }

        let mut results = Vec::new();
        for chunk in dirty_ids.chunks(SQLITE_VAR_LIMIT) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            let sql = format!(
                "SELECT i.id FROM issues i
                 WHERE i.id IN ({})
                   AND i.deleted_at IS NULL
                   AND (
                     i.id NOT IN (SELECT issue_id FROM export_hashes)
                     OR i.content_hash != (SELECT e.content_hash FROM export_hashes e WHERE e.issue_id = i.id)
                   )
                 ORDER BY i.id",
                placeholders.join(",")
            );

            let params: Vec<SqliteValue> = chunk
                .iter()
                .map(|s| SqliteValue::from(s.as_str()))
                .collect();

            let rows = self.conn.query_with_params(&sql, &params)?;
            results.extend(
                rows.iter()
                    .filter_map(|r| r.get(0).and_then(SqliteValue::as_text).map(String::from)),
            );
        }

        results.sort();
        Ok(results)
    }

    /// Get a metadata value by key.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_metadata(&self, key: &str) -> Result<Option<String>> {
        match self.conn.query_row_with_params(
            "SELECT value FROM metadata WHERE key = ?",
            &[SqliteValue::from(key)],
        ) {
            Ok(row) => Ok(row.get(0).and_then(SqliteValue::as_text).map(String::from)),
            Err(fsqlite_error::FrankenError::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(BeadsError::Database(e)),
        }
    }

    /// Set a metadata value.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn set_metadata(&mut self, key: &str, value: &str) -> Result<()> {
        self.with_write_transaction(|storage| {
            storage.conn.execute_with_params(
                "DELETE FROM metadata WHERE key = ?",
                &[SqliteValue::from(key)],
            )?;
            storage.conn.execute_with_params(
                "INSERT INTO metadata (key, value) VALUES (?, ?)",
                &[SqliteValue::from(key), SqliteValue::from(value)],
            )?;
            Ok(())
        })
    }

    /// Set a metadata value using an internal write transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub(crate) fn set_metadata_shared(&self, key: &str, value: &str) -> Result<()> {
        Self::with_connection_write_transaction(&self.conn, |conn| {
            Self::delete_metadata_key_in_tx(conn, key)?;
            conn.execute_with_params(
                "INSERT INTO metadata (key, value) VALUES (?, ?)",
                &[SqliteValue::from(key), SqliteValue::from(value)],
            )?;
            Ok(())
        })
    }

    /// Delete a metadata key.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn delete_metadata(&mut self, key: &str) -> Result<bool> {
        let count = self.conn.execute_with_params(
            "DELETE FROM metadata WHERE key = ?",
            &[SqliteValue::from(key)],
        )?;
        Ok(count > 0)
    }

    /// Count issues in the database.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn count_all_issues(&self) -> Result<usize> {
        let count = self
            .conn
            .query_row("SELECT count(*) FROM issues")?
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap_or(0);
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// Count active project issues using default user-facing visibility.
    ///
    /// Active issues are non-closed issues, including deferred issues, while
    /// excluding template issues like the default command/query surfaces.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn count_active_issues(&self) -> Result<usize> {
        self.count_issues_with_filters(&ListFilters {
            include_deferred: true,
            ..ListFilters::default()
        })
    }

    /// Get full issue details.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_issue_details(
        &self,
        id: &str,
        include_comments: bool,
        include_events: bool,
        event_limit: usize,
    ) -> Result<Option<IssueDetails>> {
        let Some(issue) = self.get_issue(id)? else {
            return Ok(None);
        };

        let labels = self.get_labels(id)?;
        let dependencies = self.get_dependencies_with_metadata(id)?;
        let dependents = self.get_dependents_with_metadata(id)?;
        let comments = if include_comments {
            self.get_comments(id)?
        } else {
            vec![]
        };
        let events = if include_events {
            get_events(&self.conn, id, event_limit)?
        } else {
            vec![]
        };
        let parent = self.get_parent_id(id)?;

        Ok(Some(IssueDetails {
            issue,
            labels,
            dependencies,
            dependents,
            comments,
            events,
            parent,
        }))
    }

    fn issue_from_row(row: &fsqlite::Row) -> Result<Issue> {
        let get_str = |idx: usize| -> String {
            row.get(idx)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string()
        };
        let get_str_ref =
            |idx: usize| -> &str { row.get(idx).and_then(SqliteValue::as_text).unwrap_or("") };
        let get_opt_str = |idx: usize| -> Option<String> {
            row.get(idx)
                .and_then(SqliteValue::as_text)
                .map(str::to_string)
        };
        let get_non_empty_str = |idx: usize| -> Option<String> {
            row.get(idx)
                .and_then(SqliteValue::as_text)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        #[allow(clippy::cast_possible_truncation)]
        let get_opt_i32 = |idx: usize| -> Option<i32> {
            row.get(idx)
                .and_then(SqliteValue::as_integer)
                .map(|v| v as i32)
        };
        let get_bool = |idx: usize| -> bool {
            row.get(idx).and_then(SqliteValue::as_integer).unwrap_or(0) != 0
        };
        let get_opt_datetime = |idx: usize| -> Result<Option<chrono::DateTime<chrono::Utc>>> {
            row.get(idx)
                .and_then(SqliteValue::as_text)
                .filter(|s| !s.is_empty())
                .map(parse_datetime)
                .transpose()
        };

        Ok(Issue {
            id: get_str(0),
            content_hash: get_opt_str(1),
            title: get_str(2),
            description: get_non_empty_str(3),
            design: get_non_empty_str(4),
            acceptance_criteria: get_non_empty_str(5),
            notes: get_non_empty_str(6),
            status: parse_status(row.get(7).and_then(SqliteValue::as_text)),
            priority: Priority(get_opt_i32(8).unwrap_or_else(|| Priority::default().0)),
            issue_type: parse_issue_type(row.get(9).and_then(SqliteValue::as_text)),
            assignee: get_non_empty_str(10),
            owner: get_non_empty_str(11),
            estimated_minutes: get_opt_i32(12),
            created_at: parse_datetime(get_str_ref(13))?,
            created_by: get_non_empty_str(14),
            updated_at: parse_datetime(get_str_ref(15))?,
            closed_at: get_opt_datetime(16)?,
            close_reason: get_non_empty_str(17),
            closed_by_session: get_non_empty_str(18),
            due_at: get_opt_datetime(19)?,
            defer_until: get_opt_datetime(20)?,
            external_ref: get_opt_str(21),
            source_system: get_non_empty_str(22),
            source_repo: get_non_empty_str(23),
            deleted_at: get_opt_datetime(24)?,
            deleted_by: get_non_empty_str(25),
            delete_reason: get_non_empty_str(26),
            original_type: get_non_empty_str(27),
            compaction_level: get_opt_i32(28),
            compacted_at: get_opt_datetime(29)?,
            compacted_at_commit: get_opt_str(30),
            original_size: get_opt_i32(31),
            sender: get_non_empty_str(32),
            ephemeral: get_bool(33),
            pinned: get_bool(34),
            is_template: get_bool(35),
            labels: vec![],
            dependencies: vec![],
            comments: vec![],
        })
    }

    fn ready_issue_from_row(row: &fsqlite::Row) -> Result<Issue> {
        let get_str = |idx: usize| -> String {
            row.get(idx)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string()
        };
        let get_str_ref =
            |idx: usize| -> &str { row.get(idx).and_then(SqliteValue::as_text).unwrap_or("") };
        let get_non_empty_str = |idx: usize| -> Option<String> {
            row.get(idx)
                .and_then(SqliteValue::as_text)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        #[allow(clippy::cast_possible_truncation)]
        let get_opt_i32 = |idx: usize| -> Option<i32> {
            row.get(idx)
                .and_then(SqliteValue::as_integer)
                .map(|value| value as i32)
        };

        Ok(Issue {
            id: get_str(0),
            content_hash: None,
            title: get_str(1),
            description: get_non_empty_str(2),
            design: None,
            acceptance_criteria: get_non_empty_str(3),
            notes: get_non_empty_str(4),
            status: parse_status(row.get(5).and_then(SqliteValue::as_text)),
            priority: Priority(get_opt_i32(6).unwrap_or_else(|| Priority::default().0)),
            issue_type: parse_issue_type(row.get(7).and_then(SqliteValue::as_text)),
            assignee: get_non_empty_str(8),
            owner: get_non_empty_str(9),
            estimated_minutes: get_opt_i32(10),
            created_at: parse_datetime(get_str_ref(11))?,
            created_by: get_non_empty_str(12),
            updated_at: parse_datetime(get_str_ref(13))?,
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            due_at: None,
            defer_until: None,
            external_ref: None,
            source_system: None,
            source_repo: None,
            deleted_at: None,
            deleted_by: None,
            delete_reason: None,
            original_type: None,
            compaction_level: None,
            compacted_at: None,
            compacted_at_commit: None,
            original_size: None,
            sender: None,
            ephemeral: false,
            pinned: false,
            is_template: false,
            labels: vec![],
            dependencies: vec![],
            comments: vec![],
        })
    }

    fn stats_issue_from_row(row: &fsqlite::Row) -> Result<StatsIssueRow> {
        let get_str = |idx: usize| -> String {
            row.get(idx)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string()
        };
        let get_non_empty_str = |idx: usize| -> Option<String> {
            row.get(idx)
                .and_then(SqliteValue::as_text)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        #[allow(clippy::cast_possible_truncation)]
        let get_opt_i32 = |idx: usize| -> Option<i32> {
            row.get(idx)
                .and_then(SqliteValue::as_integer)
                .map(|value| value as i32)
        };
        let get_bool = |idx: usize| -> bool {
            row.get(idx).and_then(SqliteValue::as_integer).unwrap_or(0) != 0
        };
        let get_opt_datetime = |idx: usize| -> Result<Option<DateTime<Utc>>> {
            row.get(idx)
                .and_then(SqliteValue::as_text)
                .filter(|s| !s.is_empty())
                .map(parse_datetime)
                .transpose()
        };

        Ok(StatsIssueRow {
            id: get_str(0),
            status: parse_status(row.get(1).and_then(SqliteValue::as_text)),
            priority: Priority(get_opt_i32(2).unwrap_or_else(|| Priority::default().0)),
            issue_type: parse_issue_type(row.get(3).and_then(SqliteValue::as_text)),
            assignee: get_non_empty_str(4),
            created_at: parse_datetime(row.get(5).and_then(SqliteValue::as_text).unwrap_or(""))?,
            closed_at: get_opt_datetime(6)?,
            defer_until: get_opt_datetime(7)?,
            ephemeral: get_bool(8),
            pinned: get_bool(9),
            is_template: get_bool(10),
        })
    }

    /// Get metadata for all active issues.
    ///
    /// This is used to pre-populate caches for graph traversals, avoiding N+1 queries.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_active_issues_metadata(
        &self,
    ) -> Result<std::collections::HashMap<String, (String, i32, String)>> {
        let sql = "SELECT id, title, priority, status FROM issues WHERE status != 'tombstone'";
        let rows = self.conn.query(sql)?;

        let mut map = std::collections::HashMap::with_capacity(rows.len());
        for row in &rows {
            let id = row
                .get(0)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            let title = row
                .get(1)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            let priority = row.get(2).and_then(SqliteValue::as_integer).map_or(2, |v| {
                i32::try_from(v).unwrap_or(if v < 0 { i32::MIN } else { i32::MAX })
            });
            let status = row
                .get(3)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            map.insert(id, (title, priority, status));
        }
        Ok(map)
    }

    /// Set metadata (in tx).
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub(crate) fn set_metadata_in_tx(&self, key: &str, value: &str) -> Result<()> {
        // Explicit DELETE + INSERT instead of INSERT OR REPLACE because
        // fsqlite does not enforce UNIQUE constraints on non-rowid columns.
        self.conn.execute_with_params(
            "DELETE FROM metadata WHERE key = ?",
            &[SqliteValue::from(key)],
        )?;
        self.conn.execute_with_params(
            "INSERT INTO metadata (key, value) VALUES (?, ?)",
            &[SqliteValue::from(key), SqliteValue::from(value)],
        )?;
        Ok(())
    }

    /// Clear all export hashes (in tx).
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub(crate) fn clear_all_export_hashes_in_tx(&self) -> Result<usize> {
        let count = self.conn.execute("DELETE FROM export_hashes")?;
        Ok(count)
    }
}

fn finish_issue_mutation_write_probe(
    probe_result: std::result::Result<usize, FrankenError>,
    rollback_result: std::result::Result<usize, FrankenError>,
) -> Result<()> {
    match (probe_result, rollback_result) {
        (Ok(_), Ok(_)) => Ok(()),
        (Ok(_), Err(rollback_err)) => Err(BeadsError::Database(rollback_err)),
        (Err(probe_err), Ok(_)) => Err(BeadsError::Database(probe_err)),
        (Err(probe_err), Err(rollback_err)) => {
            tracing::warn!(
                error = %rollback_err,
                "ROLLBACK failed after issue write probe"
            );
            Err(BeadsError::Database(probe_err))
        }
    }
}

fn database_header_user_version(path: &Path) -> Option<u32> {
    if path == Path::new(":memory:") || !path.is_file() {
        return None;
    }

    let mut file = std::fs::File::open(path).ok()?;
    let mut header = [0_u8; 100];
    file.read_exact(&mut header).ok()?;
    if &header[..16] != b"SQLite format 3\0" {
        return None;
    }

    Some(u32::from_be_bytes([
        header[60], header[61], header[62], header[63],
    ]))
}

/// Filter options for listing issues.
#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct ListFilters {
    pub statuses: Option<Vec<Status>>,
    pub types: Option<Vec<IssueType>>,
    pub priorities: Option<Vec<Priority>>,
    pub assignee: Option<String>,
    pub unassigned: bool,
    pub include_closed: bool,
    pub include_deferred: bool,
    pub include_templates: bool,
    pub title_contains: Option<String>,
    pub limit: Option<usize>,
    /// Offset for pagination (number of rows to skip before applying LIMIT)
    pub offset: Option<usize>,
    /// Sort field (priority, `created_at`, `updated_at`, title)
    pub sort: Option<String>,
    /// Reverse sort order
    pub reverse: bool,
    /// Filter by labels (all specified labels must match)
    pub labels: Option<Vec<String>>,
    /// Filter by labels (OR logic)
    pub labels_or: Option<Vec<String>>,
    /// Filter by `updated_at` <= timestamp
    pub updated_before: Option<DateTime<Utc>>,
    /// Filter by `updated_at` >= timestamp
    pub updated_after: Option<DateTime<Utc>>,
}

/// Lean issue row used by the stats command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatsIssueRow {
    pub id: String,
    pub status: Status,
    pub priority: Priority,
    pub issue_type: IssueType,
    pub assignee: Option<String>,
    pub created_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    pub defer_until: Option<DateTime<Utc>>,
    pub ephemeral: bool,
    pub pinned: bool,
    pub is_template: bool,
}

/// Fields to update on an issue.
#[derive(Debug, Clone, Default)]
pub struct IssueUpdate {
    pub title: Option<String>,
    pub description: Option<Option<String>>,
    pub design: Option<Option<String>>,
    pub acceptance_criteria: Option<Option<String>>,
    pub notes: Option<Option<String>>,
    pub status: Option<Status>,
    pub priority: Option<Priority>,
    pub issue_type: Option<IssueType>,
    pub assignee: Option<Option<String>>,
    pub owner: Option<Option<String>>,
    pub estimated_minutes: Option<Option<i32>>,
    pub due_at: Option<Option<DateTime<Utc>>>,
    pub defer_until: Option<Option<DateTime<Utc>>>,
    pub external_ref: Option<Option<String>>,
    pub closed_at: Option<Option<DateTime<Utc>>>,
    pub close_reason: Option<Option<String>>,
    pub closed_by_session: Option<Option<String>>,
    pub deleted_at: Option<Option<DateTime<Utc>>>,
    pub deleted_by: Option<Option<String>>,
    pub delete_reason: Option<Option<String>>,
    /// If true, do not rebuild the blocked cache after update.
    /// Caller is responsible for rebuilding cache if needed.
    pub skip_cache_rebuild: bool,
    /// If true, verify the issue is unassigned (or assigned to `claim_actor`)
    /// inside the IMMEDIATE transaction to prevent TOCTOU races.
    pub expect_unassigned: bool,
    /// If true, reject re-claims even by the same actor.
    pub claim_exclusive: bool,
    /// The actor performing the claim (used for idempotent same-actor check).
    pub claim_actor: Option<String>,
}

impl IssueUpdate {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.description.is_none()
            && self.design.is_none()
            && self.acceptance_criteria.is_none()
            && self.notes.is_none()
            && self.status.is_none()
            && self.priority.is_none()
            && self.issue_type.is_none()
            && self.assignee.is_none()
            && self.owner.is_none()
            && self.estimated_minutes.is_none()
            && self.due_at.is_none()
            && self.defer_until.is_none()
            && self.external_ref.is_none()
            && self.closed_at.is_none()
            && self.close_reason.is_none()
            && self.closed_by_session.is_none()
            && self.deleted_at.is_none()
            && self.deleted_by.is_none()
            && self.delete_reason.is_none()
            && !self.expect_unassigned
    }
}

/// Filter options for ready issues.
#[derive(Debug, Clone, Default)]
pub struct ReadyFilters {
    pub assignee: Option<String>,
    pub unassigned: bool,
    pub labels_and: Vec<String>,
    pub labels_or: Vec<String>,
    pub types: Option<Vec<IssueType>>,
    pub priorities: Option<Vec<Priority>>,
    pub include_deferred: bool,
    pub limit: Option<usize>,
    /// Filter to children of this parent issue ID.
    pub parent: Option<String>,
    /// Include all descendants (grandchildren, etc.) not just direct children.
    pub recursive: bool,
}

/// Minimal metadata needed for fast collision detection during sync.
#[derive(Debug, Clone)]
pub struct IssueMetadata {
    pub id: String,
    pub external_ref: Option<String>,
    pub content_hash: Option<String>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub status: crate::model::Status,
}

/// Sort policy for ready issues.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub enum ReadySortPolicy {
    /// P0/P1 first by `created_at` ASC, then others by `created_at` ASC
    #[default]
    Hybrid,
    /// Sort by priority ASC, then `created_at` ASC
    Priority,
    /// Sort by `created_at` ASC only
    Oldest,
}

fn parse_status(s: Option<&str>) -> Status {
    s.map_or_else(Status::default, |val| {
        val.parse()
            .unwrap_or_else(|_| Status::Custom(val.to_string()))
    })
}

fn parse_issue_type(s: Option<&str>) -> IssueType {
    s.and_then(|s| s.parse().ok()).unwrap_or_default()
}

fn dependency_metadata_from_row(
    row: &fsqlite::Row,
    row_role: &str,
    allow_external_placeholder: bool,
) -> Result<IssueWithDependencyMetadata> {
    let id = row
        .get(0)
        .and_then(SqliteValue::as_text)
        .ok_or_else(|| BeadsError::Config(format!("{row_role} row missing id")))?;
    let dep_type = row
        .get(4)
        .and_then(SqliteValue::as_text)
        .ok_or_else(|| {
            BeadsError::Config(format!("{row_role} row missing dependency type for {id}"))
        })?
        .to_string();

    let title = row.get(1).and_then(SqliteValue::as_text);
    let status = row.get(2).and_then(SqliteValue::as_text);
    let priority = row.get(3).and_then(SqliteValue::as_integer);

    let (title, status, priority) = match (title, status, priority) {
        (Some(title), Some(status), Some(priority)) => (title, status, priority),
        _ if allow_external_placeholder && id.starts_with("external:") => {
            return Ok(IssueWithDependencyMetadata {
                id: id.to_string(),
                title: id.strip_prefix("external:").unwrap_or(id).to_string(),
                status: Status::Blocked,
                priority: Priority::MEDIUM,
                dep_type,
            });
        }
        _ => {
            // Graceful fallback for missing dependencies (e.g. deleted/not synced yet)
            // instead of crashing the query with a Config error.
            return Ok(IssueWithDependencyMetadata {
                id: id.to_string(),
                title: format!("[missing issue: {}]", id),
                status: Status::Tombstone,
                priority: Priority::MEDIUM,
                dep_type,
            });
        }
    };

    let priority = i32::try_from(priority).map_err(|_| {
        BeadsError::Config(format!("{row_role} row priority out of range for {id}"))
    })?;

    Ok(IssueWithDependencyMetadata {
        id: id.to_string(),
        title: title.to_string(),
        status: parse_status(Some(status)),
        priority: Priority(priority),
        dep_type,
    })
}

fn parse_blocked_by_json(issue_id: &str, blockers_json: Option<&str>) -> Result<Vec<String>> {
    let blockers_json = blockers_json.ok_or_else(|| {
        BeadsError::Config(format!(
            "blocked_issues_cache missing blocked_by payload for {issue_id}"
        ))
    })?;

    serde_json::from_str(blockers_json).map_err(|err| {
        BeadsError::Config(format!("Malformed blocked_by JSON for {issue_id}: {err}"))
    })
}

fn parse_external_dependency(dep_id: &str) -> Option<(String, String)> {
    let mut parts = dep_id.splitn(3, ':');
    let prefix = parts.next()?;
    if prefix != "external" {
        return None;
    }
    let project = parts.next()?.to_string();
    let capability = parts.next()?.to_string();
    if project.is_empty() || capability.is_empty() {
        return None;
    }
    Some((project, capability))
}

fn query_external_project_capabilities(
    db_path: &Path,
    capabilities: &HashSet<String>,
) -> Result<HashSet<String>> {
    const SQLITE_VAR_LIMIT: usize = 900;

    if capabilities.is_empty() {
        return Ok(HashSet::new());
    }

    let conn = Connection::open(db_path.to_string_lossy().into_owned())?;
    let labels: Vec<String> = capabilities
        .iter()
        .map(|cap| format!("provides:{cap}"))
        .collect();

    let mut satisfied = HashSet::new();

    for chunk in labels.chunks(SQLITE_VAR_LIMIT) {
        let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
        let label_sql = format!(
            "SELECT label, issue_id
             FROM labels
             WHERE label IN ({})",
            placeholders.join(",")
        );
        let label_params: Vec<SqliteValue> = chunk
            .iter()
            .map(|label| SqliteValue::from(label.as_str()))
            .collect();
        let rows = conn.query_with_params(&label_sql, &label_params)?;

        let mut issue_ids_by_capability: HashMap<String, HashSet<String>> = HashMap::new();
        for row in &rows {
            let Some(label) = row.get(0).and_then(SqliteValue::as_text) else {
                continue;
            };
            let Some(issue_id) = row.get(1).and_then(SqliteValue::as_text) else {
                continue;
            };
            let Some(capability) = label.strip_prefix("provides:") else {
                continue;
            };
            issue_ids_by_capability
                .entry(capability.to_string())
                .or_default()
                .insert(issue_id.to_string());
        }

        if issue_ids_by_capability.is_empty() {
            continue;
        }

        let candidate_issue_ids: Vec<String> = issue_ids_by_capability
            .values()
            .flat_map(|issue_ids| issue_ids.iter().cloned())
            .collect();
        let mut closed_issue_ids = HashSet::new();

        for issue_chunk in candidate_issue_ids.chunks(SQLITE_VAR_LIMIT) {
            let issue_placeholders: Vec<&str> = issue_chunk.iter().map(|_| "?").collect();
            let issue_sql = format!(
                "SELECT id
                 FROM issues
                 WHERE status = 'closed' AND id IN ({})",
                issue_placeholders.join(",")
            );
            let issue_params: Vec<SqliteValue> = issue_chunk
                .iter()
                .map(|issue_id| SqliteValue::from(issue_id.as_str()))
                .collect();
            let issue_rows = conn.query_with_params(&issue_sql, &issue_params)?;

            for row in &issue_rows {
                if let Some(issue_id) = row.get(0).and_then(SqliteValue::as_text) {
                    closed_issue_ids.insert(issue_id.to_string());
                }
            }
        }

        for (capability, issue_ids) in issue_ids_by_capability {
            if issue_ids
                .iter()
                .any(|issue_id| closed_issue_ids.contains(issue_id))
            {
                satisfied.insert(capability);
            }
        }
    }

    // Explicitly close the connection to avoid fsqlite drop_close warnings.
    let _ = conn.close();
    Ok(satisfied)
}

fn parse_datetime(s: &str) -> Result<DateTime<Utc>> {
    if s.is_empty() {
        // NULL/empty datetime columns (common when migrating from bd/Go beads)
        // default to epoch rather than crashing the import.
        return Ok(DateTime::<Utc>::UNIX_EPOCH);
    }

    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }

    if let Ok(naive) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Ok(Utc.from_utc_datetime(&naive));
    }

    Err(BeadsError::Config(format!("unparseable datetime: {s}")))
}

/// Escape special LIKE pattern characters (%, _, \) for literal matching.
///
/// Use with `LIKE ? ESCAPE '\\'` in SQL queries.
fn escape_like_pattern(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

// ============================================================================
// EXPORT/SYNC METHODS
// ============================================================================

impl SqliteStorage {
    /// Get issue with all relations populated for export.
    ///
    /// Includes labels, dependencies, and comments.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_issue_for_export(&self, id: &str) -> Result<Option<Issue>> {
        let Some(mut issue) = self.get_issue(id)? else {
            return Ok(None);
        };

        // Populate relations
        issue.labels = self.get_labels(id)?;
        issue.dependencies = self.get_dependencies_full(id)?;
        issue.comments = self.get_comments(id)?;

        Ok(Some(issue))
    }

    /// Get multiple issues with all relations populated for export.
    ///
    /// Includes labels, dependencies, and comments. This fetches in batch to avoid N+1 queries.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_issues_for_export(&self, ids: &[String]) -> Result<Vec<Issue>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut issues = self.get_issues_by_ids(ids)?;

        // Fetch relations in batch
        let labels_map = self.get_labels_for_issues(ids)?;
        let deps_map = self.get_dependencies_full_for_issues(ids)?;
        let comments_map = self.get_comments_for_issues(ids)?;

        for issue in &mut issues {
            if let Some(labels) = labels_map.get(&issue.id) {
                issue.labels = labels.clone();
                issue.labels.sort();
                issue.labels.dedup();
            }
            if let Some(deps) = deps_map.get(&issue.id) {
                issue.dependencies = deps.clone();
            }
            if let Some(comments) = comments_map.get(&issue.id) {
                issue.comments = comments.clone();
            }
        }

        Ok(issues)
    }

    /// Get dependencies as full Dependency structs for export.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_dependencies_full(&self, issue_id: &str) -> Result<Vec<crate::model::Dependency>> {
        let stmt = self.conn.prepare(
            "SELECT issue_id, depends_on_id, type, created_at, created_by, metadata, thread_id
             FROM dependencies
             WHERE issue_id = ?
             ORDER BY depends_on_id",
        )?;

        let rows = stmt.query_with_params(&[SqliteValue::from(issue_id)])?;

        let mut deps = Vec::with_capacity(rows.len());
        for row in &rows {
            let created_at_str = row
                .get(3)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            deps.push(crate::model::Dependency {
                issue_id: row
                    .get(0)
                    .and_then(SqliteValue::as_text)
                    .unwrap_or("")
                    .to_string(),
                depends_on_id: row
                    .get(1)
                    .and_then(SqliteValue::as_text)
                    .unwrap_or("")
                    .to_string(),
                dep_type: row
                    .get(2)
                    .and_then(SqliteValue::as_text)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(crate::model::DependencyType::Blocks),
                created_at: parse_datetime(&created_at_str)?,
                created_by: row
                    .get(4)
                    .and_then(SqliteValue::as_text)
                    .map(str::to_string),
                metadata: row
                    .get(5)
                    .and_then(SqliteValue::as_text)
                    .map(str::to_string),
                thread_id: row
                    .get(6)
                    .and_then(SqliteValue::as_text)
                    .map(str::to_string),
            });
        }

        Ok(deps)
    }

    /// Get dependencies as full Dependency structs for multiple issues in batch.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_dependencies_full_for_issues(
        &self,
        issue_ids: &[String],
    ) -> Result<std::collections::HashMap<String, Vec<crate::model::Dependency>>> {
        const SQLITE_VAR_LIMIT: usize = 900;
        let mut map: std::collections::HashMap<String, Vec<crate::model::Dependency>> =
            std::collections::HashMap::new();

        if issue_ids.is_empty() {
            return Ok(map);
        }

        for chunk in issue_ids.chunks(SQLITE_VAR_LIMIT) {
            let placeholders = vec!["?"; chunk.len()].join(", ");
            let sql = format!(
                "SELECT issue_id, depends_on_id, type, created_at, created_by, metadata, thread_id
                 FROM dependencies
                 WHERE issue_id IN ({})
                 ORDER BY depends_on_id",
                placeholders
            );

            let params: Vec<SqliteValue> = chunk
                .iter()
                .map(|id| SqliteValue::from(id.as_str()))
                .collect();

            let rows = self.conn.query_with_params(&sql, &params)?;

            for row in &rows {
                let created_at_str = row
                    .get(3)
                    .and_then(SqliteValue::as_text)
                    .unwrap_or("")
                    .to_string();
                let dep = crate::model::Dependency {
                    issue_id: row
                        .get(0)
                        .and_then(SqliteValue::as_text)
                        .unwrap_or("")
                        .to_string(),
                    depends_on_id: row
                        .get(1)
                        .and_then(SqliteValue::as_text)
                        .unwrap_or("")
                        .to_string(),
                    dep_type: row
                        .get(2)
                        .and_then(SqliteValue::as_text)
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(crate::model::DependencyType::Blocks),
                    created_at: parse_datetime(&created_at_str)?,
                    created_by: row
                        .get(4)
                        .and_then(SqliteValue::as_text)
                        .map(str::to_string),
                    metadata: row
                        .get(5)
                        .and_then(SqliteValue::as_text)
                        .map(str::to_string),
                    thread_id: row
                        .get(6)
                        .and_then(SqliteValue::as_text)
                        .map(str::to_string),
                };
                map.entry(dep.issue_id.clone()).or_default().push(dep);
            }
        }

        Ok(map)
    }

    /// Clear dirty flags for the given issue IDs.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub fn clear_dirty_flags(&mut self, ids: &[String]) -> Result<usize> {
        const SQLITE_VAR_LIMIT: usize = 900;
        if ids.is_empty() {
            return Ok(0);
        }

        let mut total_deleted = 0;
        for chunk in ids.chunks(SQLITE_VAR_LIMIT) {
            let placeholders = vec!["?"; chunk.len()].join(", ");
            let sql = format!("DELETE FROM dirty_issues WHERE issue_id IN ({placeholders})");

            let params: Vec<SqliteValue> = chunk
                .iter()
                .map(|s| SqliteValue::from(s.as_str()))
                .collect();
            let deleted = self.conn.execute_with_params(&sql, &params)?;
            total_deleted += deleted;
        }

        Ok(total_deleted)
    }

    /// Clear all dirty flags.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub fn clear_all_dirty_flags(&mut self) -> Result<usize> {
        let deleted = self.conn.execute("DELETE FROM dirty_issues")?;
        Ok(deleted)
    }

    /// Get the count of issues (for safety guard).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn count_exportable_issues(&self) -> Result<usize> {
        let count = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM issues WHERE ephemeral = 0 AND id NOT LIKE '%-wisp-%'",
            )?
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap_or(0);
        // count is always non-negative from COUNT(*), safe to cast
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        Ok(count as usize)
    }

    /// Check if a dependency exists between two issues.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn dependency_exists_between(&self, issue_id: &str, depends_on_id: &str) -> Result<bool> {
        let count = self
            .conn
            .query_row_with_params(
                "SELECT COUNT(*) FROM dependencies WHERE issue_id = ? AND depends_on_id = ?",
                &[
                    SqliteValue::from(issue_id),
                    SqliteValue::from(depends_on_id),
                ],
            )?
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap_or(0);
        Ok(count > 0)
    }

    /// Check if adding a dependency would create a cycle.
    ///
    /// If `blocking_only` is true, only considers blocking dependency types
    /// ('blocks', 'parent-child', 'conditional-blocks') for cycle detection.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn would_create_cycle(
        &self,
        issue_id: &str,
        depends_on_id: &str,
        blocking_only: bool,
    ) -> Result<bool> {
        Self::check_cycle(&self.conn, issue_id, depends_on_id, blocking_only)
    }

    /// Detect all cycles in the dependency graph.
    ///
    /// Returns a list of cycles, where each cycle is a vector of issue IDs.
    /// Uses an iterative DFS to avoid stack overflow on deep graphs.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn detect_all_cycles(&self) -> Result<Vec<Vec<String>>> {
        use std::collections::{HashMap, HashSet};

        // Get all dependencies, respecting parent-child direction (parent depends on child)
        let mut graph: HashMap<String, Vec<String>> = HashMap::new();
        let stmt = self.conn.prepare(
            r"
                SELECT issue_id, depends_on_id FROM dependencies WHERE type != 'parent-child'
                UNION
                SELECT depends_on_id, issue_id FROM dependencies WHERE type = 'parent-child'
            ",
        )?;

        let rows = stmt.query()?;

        for row in &rows {
            let from = row
                .get(0)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            let to = row
                .get(1)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            graph.entry(from).or_default().push(to);
        }

        let mut cycles = Vec::new();
        let mut visited = HashSet::new();
        let mut rec_stack = HashSet::new();
        let mut path = Vec::new();

        // Stack stores (node_id, neighbor_index)
        let mut stack: Vec<(String, usize)> = Vec::new();

        // Sort keys for deterministic output
        let mut keys: Vec<_> = graph.keys().cloned().collect();
        keys.sort();

        for node in keys {
            if visited.contains(&node) {
                continue;
            }

            stack.push((node.clone(), 0));
            visited.insert(node.clone());
            rec_stack.insert(node.clone());
            path.push(node.clone());

            while let Some((u, idx)) = stack.last_mut() {
                let neighbors = graph.get(u);

                if let Some(neighbors) = neighbors
                    && *idx < neighbors.len()
                {
                    let v = &neighbors[*idx];
                    *idx += 1;

                    if rec_stack.contains(v) {
                        // Found a cycle: reconstruct it from the current path
                        if let Some(start_pos) = path.iter().position(|x| x == v) {
                            let mut cycle = path[start_pos..].to_vec();
                            cycle.push(v.clone()); // Close the loop
                            cycles.push(cycle);
                        }
                    } else if !visited.contains(v) {
                        visited.insert(v.clone());
                        rec_stack.insert(v.clone());
                        path.push(v.clone());
                        stack.push((v.clone(), 0));
                    }
                    continue;
                }

                // Finished processing all neighbors of u
                rec_stack.remove(u);
                path.pop();
                stack.pop();
            }
        }

        Ok(cycles)
    }

    // ===== Import Helper Methods =====

    /// Find an issue by external reference.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn find_by_external_ref(&self, external_ref: &str) -> Result<Option<Issue>> {
        match self.conn.query_row_with_params(
            r"SELECT id, content_hash, title, description, design, acceptance_criteria, notes,
                     status, priority, issue_type, assignee, owner, estimated_minutes,
                     created_at, created_by, updated_at, closed_at, close_reason, closed_by_session,
                     due_at, defer_until, external_ref, source_system, source_repo,
                     deleted_at, deleted_by, delete_reason, original_type, compaction_level,
                     compacted_at, compacted_at_commit, original_size, sender, ephemeral,
                     pinned, is_template
               FROM issues WHERE external_ref = ?",
            &[SqliteValue::from(external_ref)],
        ) {
            Ok(row) => Ok(Some(Self::issue_from_row(&row)?)),
            Err(fsqlite_error::FrankenError::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(BeadsError::Database(e)),
        }
    }

    /// Find an issue by content hash.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn find_by_content_hash(&self, content_hash: &str) -> Result<Option<Issue>> {
        match self.conn.query_row_with_params(
            r"SELECT id, content_hash, title, description, design, acceptance_criteria, notes,
                     status, priority, issue_type, assignee, owner, estimated_minutes,
                     created_at, created_by, updated_at, closed_at, close_reason, closed_by_session,
                     due_at, defer_until, external_ref, source_system, source_repo,
                     deleted_at, deleted_by, delete_reason, original_type, compaction_level,
                     compacted_at, compacted_at_commit, original_size, sender, ephemeral,
                     pinned, is_template
               FROM issues WHERE content_hash = ?",
            &[SqliteValue::from(content_hash)],
        ) {
            Ok(row) => Ok(Some(Self::issue_from_row(&row)?)),
            Err(fsqlite_error::FrankenError::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(BeadsError::Database(e)),
        }
    }

    /// Check if an issue is a tombstone (deleted).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn is_tombstone(&self, id: &str) -> Result<bool> {
        match self.conn.query_row_with_params(
            "SELECT status FROM issues WHERE id = ?",
            &[SqliteValue::from(id)],
        ) {
            Ok(row) => {
                let status = row.get(0).and_then(SqliteValue::as_text).unwrap_or("");
                Ok(status == "tombstone")
            }
            Err(fsqlite_error::FrankenError::QueryReturnedNoRows) => Ok(false),
            Err(e) => Err(BeadsError::Database(e)),
        }
    }

    /// Upsert an issue (create or update) for import operations.
    ///
    /// Uses DELETE + INSERT to atomically handle both cases.
    /// This does NOT trigger dirty tracking or events.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    #[allow(clippy::too_many_lines)]
    pub fn upsert_issue_for_import(&self, issue: &Issue) -> Result<bool> {
        let status_str = issue.status.as_str();
        let issue_type_str = issue.issue_type.as_str();
        let created_at_str = issue.created_at.to_rfc3339();
        let updated_at_str = issue.updated_at.to_rfc3339();
        let closed_at_str = issue.closed_at.map(|dt| dt.to_rfc3339());
        let due_at_str = issue.due_at.map(|dt| dt.to_rfc3339());
        let defer_until_str = issue.defer_until.map(|dt| dt.to_rfc3339());
        let deleted_at_str = issue.deleted_at.map(|dt| dt.to_rfc3339());
        let compacted_at_str = issue.compacted_at.map(|dt| dt.to_rfc3339());

        // Explicit DELETE + INSERT instead of INSERT OR REPLACE because
        // fsqlite does not enforce UNIQUE constraints on non-rowid columns.
        self.conn.execute_with_params(
            "DELETE FROM issues WHERE id = ?",
            &[SqliteValue::from(issue.id.as_str())],
        )?;

        let rows = self.conn.execute_with_params(
            r"INSERT INTO issues (
                id, content_hash, title, description, design, acceptance_criteria, notes,
                status, priority, issue_type, assignee, owner, estimated_minutes,
                created_at, created_by, updated_at, closed_at, close_reason, closed_by_session,
                due_at, defer_until, external_ref, source_system, source_repo,
                deleted_at, deleted_by, delete_reason, original_type, compaction_level,
                compacted_at, compacted_at_commit, original_size, sender, ephemeral,
                pinned, is_template
            ) VALUES (
                ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
            )",
            &[
                SqliteValue::from(issue.id.as_str()),
                issue.content_hash.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                SqliteValue::from(issue.title.as_str()),
                SqliteValue::from(issue.description.as_deref().unwrap_or("")),
                SqliteValue::from(issue.design.as_deref().unwrap_or("")),
                SqliteValue::from(issue.acceptance_criteria.as_deref().unwrap_or("")),
                SqliteValue::from(issue.notes.as_deref().unwrap_or("")),
                SqliteValue::from(status_str),
                SqliteValue::from(i64::from(issue.priority.0)),
                SqliteValue::from(issue_type_str),
                issue.assignee.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                SqliteValue::from(issue.owner.as_deref().unwrap_or("")),
                issue.estimated_minutes.map_or(SqliteValue::Null, |v| SqliteValue::from(i64::from(v))),
                SqliteValue::from(created_at_str.as_str()),
                SqliteValue::from(issue.created_by.as_deref().unwrap_or("")),
                SqliteValue::from(updated_at_str.as_str()),
                closed_at_str.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                SqliteValue::from(issue.close_reason.as_deref().unwrap_or("")),
                SqliteValue::from(issue.closed_by_session.as_deref().unwrap_or("")),
                due_at_str.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                defer_until_str.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                issue.external_ref.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                SqliteValue::from(issue.source_system.as_deref().unwrap_or("")),
                SqliteValue::from(issue.source_repo.as_deref().unwrap_or(".")),
                deleted_at_str.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                SqliteValue::from(issue.deleted_by.as_deref().unwrap_or("")),
                SqliteValue::from(issue.delete_reason.as_deref().unwrap_or("")),
                SqliteValue::from(issue.original_type.as_deref().unwrap_or("")),
                SqliteValue::from(i64::from(issue.compaction_level.unwrap_or(0))),
                compacted_at_str.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                issue.compacted_at_commit.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                SqliteValue::from(i64::from(issue.original_size.unwrap_or(0))),
                SqliteValue::from(issue.sender.as_deref().unwrap_or("")),
                SqliteValue::from(i64::from(i32::from(issue.ephemeral))),
                SqliteValue::from(i64::from(i32::from(issue.pinned))),
                SqliteValue::from(i64::from(i32::from(issue.is_template))),
            ],
        )?;

        Ok(rows > 0)
    }

    /// Sync labels for an issue (remove existing, add new).
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub fn sync_labels_for_import(&self, issue_id: &str, labels: &[String]) -> Result<()> {
        // Remove existing labels
        self.conn.execute_with_params(
            "DELETE FROM labels WHERE issue_id = ?",
            &[SqliteValue::from(issue_id)],
        )?;

        if labels.is_empty() {
            return Ok(());
        }

        // Add new labels
        let mut seen_labels = HashSet::new();
        let mut unique_labels = Vec::new();
        for label in labels {
            if seen_labels.insert(label.as_str()) {
                unique_labels.push(label);
            }
        }

        if unique_labels.is_empty() {
            return Ok(());
        }

        for chunk in unique_labels.chunks(IMPORT_LABEL_CHUNK_SIZE) {
            let placeholders: Vec<String> = chunk.iter().map(|_| "(?, ?)".to_string()).collect();
            let sql = format!(
                "INSERT INTO labels (issue_id, label) VALUES {}",
                placeholders.join(", ")
            );

            let mut params = Vec::with_capacity(chunk.len() * 2);
            for label in chunk {
                params.push(SqliteValue::from(issue_id));
                params.push(SqliteValue::from(label.as_str()));
            }

            self.conn.execute_with_params(&sql, &params)?;
        }

        Ok(())
    }

    /// Sync dependencies for an issue (remove existing, add new).
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub fn sync_dependencies_for_import(
        &self,
        issue_id: &str,
        dependencies: &[crate::model::Dependency],
    ) -> Result<()> {
        // Remove existing dependencies where this issue is the dependent
        self.conn.execute_with_params(
            "DELETE FROM dependencies WHERE issue_id = ?",
            &[SqliteValue::from(issue_id)],
        )?;

        if dependencies.is_empty() {
            return Ok(());
        }

        // Add new dependencies
        let mut seen_deps = HashSet::new();
        let mut unique_deps = Vec::new();
        for dep in dependencies {
            Self::validate_parent_child_endpoints(
                issue_id,
                &dep.depends_on_id,
                dep.dep_type.as_str(),
            )?;
            // Deduplicate by (target, type) to allow multiple relationship types
            // between the same issues while preventing identical duplicates.
            if seen_deps.insert((dep.depends_on_id.as_str(), dep.dep_type.as_str())) {
                unique_deps.push(dep);
            }
        }

        if unique_deps.is_empty() {
            return Ok(());
        }

        for chunk in unique_deps.chunks(IMPORT_DEPENDENCY_CHUNK_SIZE) {
            let placeholders: Vec<String> = chunk
                .iter()
                .map(|_| "(?, ?, ?, ?, ?, ?, ?)".to_string())
                .collect();
            let sql = format!(
                "INSERT OR IGNORE INTO dependencies (issue_id, depends_on_id, type, created_at, created_by, metadata, thread_id) VALUES {}",
                placeholders.join(", ")
            );

            let mut params = Vec::with_capacity(chunk.len() * 7);
            for dep in chunk {
                params.push(SqliteValue::from(issue_id));
                params.push(SqliteValue::from(dep.depends_on_id.as_str()));
                params.push(SqliteValue::from(dep.dep_type.as_str()));
                params.push(SqliteValue::from(dep.created_at.to_rfc3339().as_str()));
                params.push(SqliteValue::from(
                    dep.created_by.as_deref().unwrap_or("import"),
                ));
                params.push(SqliteValue::from(dep.metadata.as_deref().unwrap_or("{}")));
                params.push(SqliteValue::from(dep.thread_id.as_deref().unwrap_or("")));
            }

            self.conn.execute_with_params(&sql, &params)?;
        }

        Ok(())
    }

    /// Sync comments for an issue (remove existing, add new).
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub fn sync_comments_for_import(
        &self,
        issue_id: &str,
        comments: &[crate::model::Comment],
    ) -> Result<()> {
        // Remove existing comments
        self.conn.execute_with_params(
            "DELETE FROM comments WHERE issue_id = ?",
            &[SqliteValue::from(issue_id)],
        )?;

        if comments.is_empty() {
            return Ok(());
        }

        for comment in comments {
            let created_at = comment.created_at.to_rfc3339();
            let colliding_issue_id = if comment.id > 0 {
                self.conn
                    .query_with_params(
                        "SELECT issue_id FROM comments WHERE id = ? LIMIT 1",
                        &[SqliteValue::from(comment.id)],
                    )?
                    .into_iter()
                    .next()
                    .and_then(|row| {
                        row.get(0)
                            .and_then(SqliteValue::as_text)
                            .map(str::to_string)
                    })
            } else {
                None
            };

            if colliding_issue_id
                .as_deref()
                .is_some_and(|existing_issue_id| existing_issue_id != issue_id)
                || comment.id <= 0
            {
                self.conn.execute_with_params(
                    "INSERT INTO comments (issue_id, author, text, created_at) VALUES (?, ?, ?, ?)",
                    &[
                        SqliteValue::from(issue_id),
                        SqliteValue::from(comment.author.as_str()),
                        SqliteValue::from(comment.body.as_str()),
                        SqliteValue::from(created_at.as_str()),
                    ],
                )?;
            } else {
                self.conn.execute_with_params(
                    "INSERT INTO comments (id, issue_id, author, text, created_at) VALUES (?, ?, ?, ?, ?)",
                    &[
                        SqliteValue::from(comment.id),
                        SqliteValue::from(issue_id),
                        SqliteValue::from(comment.author.as_str()),
                        SqliteValue::from(comment.body.as_str()),
                        SqliteValue::from(created_at.as_str()),
                    ],
                )?;
            }
        }

        Ok(())
    }
}

/// Implement the `DependencyStore` trait for `SqliteStorage`.
impl crate::validation::DependencyStore for SqliteStorage {
    fn issue_exists(&self, id: &str) -> std::result::Result<bool, crate::error::BeadsError> {
        self.id_exists(id)
    }

    fn dependency_exists(
        &self,
        issue_id: &str,
        depends_on_id: &str,
    ) -> std::result::Result<bool, crate::error::BeadsError> {
        self.dependency_exists_between(issue_id, depends_on_id)
    }

    fn would_create_cycle(
        &self,
        issue_id: &str,
        depends_on_id: &str,
    ) -> std::result::Result<bool, crate::error::BeadsError> {
        Self::check_cycle(&self.conn, issue_id, depends_on_id, true)
    }
}

fn insert_comment_row(conn: &Connection, issue_id: &str, author: &str, text: &str) -> Result<i64> {
    conn.execute_with_params(
        "INSERT INTO comments (issue_id, author, text, created_at)
         VALUES (?, ?, ?, CURRENT_TIMESTAMP)",
        &[
            SqliteValue::from(issue_id),
            SqliteValue::from(author),
            SqliteValue::from(text),
        ],
    )?;
    let row = conn.query_row("SELECT last_insert_rowid()")?;
    let comment_id = row
        .get(0)
        .and_then(SqliteValue::as_integer)
        .ok_or_else(|| {
            BeadsError::Config("comments insert did not return last_insert_rowid".to_string())
        })?;
    if comment_id <= 0 {
        return Err(BeadsError::Config(format!(
            "comments insert returned invalid last_insert_rowid: {comment_id}"
        )));
    }
    Ok(comment_id)
}

fn fetch_comment(conn: &Connection, comment_id: i64) -> Result<Comment> {
    let row = conn.query_row_with_params(
        "SELECT id, issue_id, author, text, created_at FROM comments WHERE id = ?",
        &[SqliteValue::from(comment_id)],
    )?;
    comment_from_row(&row)
}

fn comment_from_row(row: &fsqlite::Row) -> Result<Comment> {
    let id = row
        .get(0)
        .and_then(SqliteValue::as_integer)
        .ok_or_else(|| BeadsError::Config("comments row missing id".to_string()))?;
    let issue_id = row
        .get(1)
        .and_then(SqliteValue::as_text)
        .ok_or_else(|| BeadsError::Config(format!("comments row missing issue_id for {id}")))?
        .to_string();
    let author = row
        .get(2)
        .and_then(SqliteValue::as_text)
        .ok_or_else(|| BeadsError::Config(format!("comments row missing author for {id}")))?
        .to_string();
    let body = row
        .get(3)
        .and_then(SqliteValue::as_text)
        .ok_or_else(|| BeadsError::Config(format!("comments row missing body for {id}")))?
        .to_string();
    let created_at_str = row
        .get(4)
        .and_then(SqliteValue::as_text)
        .ok_or_else(|| BeadsError::Config(format!("comments row missing created_at for {id}")))?;
    let created_at = parse_datetime(created_at_str).map_err(|err| match err {
        BeadsError::Config(msg) => {
            BeadsError::Config(format!("invalid comment timestamp for {id}: {msg}"))
        }
        other => other,
    })?;

    Ok(Comment {
        id,
        issue_id,
        author,
        body,
        created_at,
    })
}

fn dedupe_preserving_order(values: &[String]) -> Vec<String> {
    let mut seen = HashSet::<&str>::new();
    let mut deduped = Vec::with_capacity(values.len());
    for value in values {
        if seen.insert(value) {
            deduped.push(value.clone());
        }
    }
    deduped
}

impl Drop for SqliteStorage {
    fn drop(&mut self) {
        // Do not checkpoint on drop. Read-only CLI commands were turning
        // teardown into a write-like operation, which created spurious busy
        // failures under parallel read traffic.
        // Explicitly close the connection to avoid fsqlite drop_close warnings.
        let _ = self.conn.close_in_place();
    }
}

#[cfg(test)]
impl SqliteStorage {
    /// Execute raw SQL for tests.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQL execution fails.
    pub fn execute_test_sql(&self, sql: &str) -> Result<()> {
        crate::storage::schema::execute_batch(&self.conn, sql)?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::similar_names)]
mod tests {
    use super::*;
    use crate::format::ReadyIssue;
    use crate::model::{Issue, IssueType, Priority, Status};
    use chrono::{DateTime, Datelike, TimeZone, Utc};
    use std::fs;
    use tempfile::TempDir;

    fn make_issue(
        id: &str,
        title: &str,
        status: Status,
        priority: i32,
        assignee: Option<&str>,
        created_at: DateTime<Utc>,
        defer_until: Option<DateTime<Utc>>,
    ) -> Issue {
        Issue {
            id: id.to_string(),
            title: title.to_string(),
            status,
            priority: Priority(priority),
            issue_type: IssueType::Task,
            created_at,
            updated_at: created_at,
            defer_until,
            content_hash: None,
            description: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            assignee: assignee.map(str::to_string),
            owner: None,
            estimated_minutes: None,
            created_by: None,
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            due_at: None,
            external_ref: None,
            source_system: None,
            source_repo: None,
            deleted_at: None,
            deleted_by: None,
            delete_reason: None,
            original_type: None,
            compaction_level: None,
            compacted_at: None,
            compacted_at_commit: None,
            original_size: None,
            sender: None,
            ephemeral: false,
            pinned: false,
            is_template: false,
            labels: vec![],
            dependencies: vec![],
            comments: vec![],
        }
    }

    #[test]
    fn test_open_memory() {
        let storage = SqliteStorage::open_memory();
        if let Err(error) = storage {
            panic!("open_memory failed: {error:?}");
        }
    }

    #[test]
    fn test_create_issue() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue = Issue {
            id: "bd-1".to_string(),
            title: "Test Issue".to_string(),
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            content_hash: None,
            description: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_by: None,
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            defer_until: None,
            due_at: None,
            external_ref: None,
            source_system: None,
            source_repo: None,
            deleted_at: None,
            deleted_by: None,
            delete_reason: None,
            original_type: None,
            compaction_level: None,
            compacted_at: None,
            compacted_at_commit: None,
            original_size: None,
            sender: None,
            ephemeral: false,
            pinned: false,
            is_template: false,
            labels: vec![],
            dependencies: vec![],
            comments: vec![],
        };

        storage.create_issue(&issue, "tester").unwrap();

        // Verify it exists (raw query since get_issue not impl yet)
        let count = storage
            .conn
            .query_row_with_params(
                "SELECT count(*) FROM issues WHERE id = ?",
                &[SqliteValue::from("bd-1")],
            )
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap_or(0);
        assert_eq!(count, 1);

        // Verify event
        let event_count = storage
            .conn
            .query_row_with_params(
                "SELECT count(*) FROM events WHERE issue_id = ?",
                &[SqliteValue::from("bd-1")],
            )
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap_or(0);
        assert_eq!(event_count, 1);

        // Verify dirty
        let dirty_count = storage
            .conn
            .query_row_with_params(
                "SELECT count(*) FROM dirty_issues WHERE issue_id = ?",
                &[SqliteValue::from("bd-1")],
            )
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap_or(0);
        assert_eq!(dirty_count, 1);
    }

    #[test]
    fn test_get_all_issues_metadata_preserves_custom_status() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue = make_issue(
            "bd-custom",
            "Custom status",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        storage.create_issue(&issue, "tester").unwrap();
        storage
            .execute_test_sql("UPDATE issues SET status = 'mystery-state' WHERE id = 'bd-custom'")
            .unwrap();

        let metadata = storage.get_all_issues_metadata().unwrap();
        let issue_meta = metadata
            .iter()
            .find(|meta| meta.id == "bd-custom")
            .expect("metadata for bd-custom");

        assert_eq!(
            issue_meta.status,
            Status::Custom("mystery-state".to_string())
        );
    }

    #[test]
    fn test_get_all_issues_metadata_errors_on_invalid_updated_at() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue = make_issue(
            "bd-bad-time",
            "Bad timestamp",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        storage.create_issue(&issue, "tester").unwrap();
        storage
            .execute_test_sql(
                "UPDATE issues SET updated_at = 'not-a-timestamp' WHERE id = 'bd-bad-time'",
            )
            .unwrap();

        let err = storage.get_all_issues_metadata().unwrap_err();
        match err {
            BeadsError::Config(message) => {
                assert!(
                    message.contains("unparseable datetime"),
                    "unexpected error: {message}"
                );
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn test_transaction_rollback_on_error() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue = make_issue("bd-tx1", "Tx Test", Status::Open, 2, None, Utc::now(), None);
        storage.create_issue(&issue, "tester").unwrap();

        // Attempt a mutation that fails
        let result: Result<()> = storage.mutate("fail_op", "tester", |_tx, ctx| {
            // Do something valid first (record an event)
            ctx.record_event(
                EventType::Updated,
                "bd-tx1",
                Some("Should be rolled back".to_string()),
            );

            // Return error to trigger rollback
            Err(BeadsError::Config("Planned failure".to_string()))
        });

        assert!(result.is_err());

        // Verify side effects (event) are gone
        let events = storage.get_events("bd-tx1", 100).unwrap();
        // Should only have the creation event
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::Created);
    }

    #[test]
    fn test_transaction_rolls_back_post_body_side_effect_failures() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue = make_issue(
            "bd-tx-side-effect",
            "Tx Side Effect Test",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        storage.create_issue(&issue, "tester").unwrap();

        let result: Result<()> = storage.mutate("fail_event_insert", "tester", |_tx, ctx| {
            ctx.record_event(
                EventType::Updated,
                "bd-missing",
                Some("Should fail after closure succeeds".to_string()),
            );
            Ok(())
        });

        assert!(
            result.is_err(),
            "event insert should fail on missing issue FK"
        );

        let follow_up = make_issue(
            "bd-tx-side-effect-2",
            "Follow Up",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        storage.create_issue(&follow_up, "tester").unwrap();
        assert!(
            storage.get_issue("bd-tx-side-effect-2").unwrap().is_some(),
            "subsequent writes should succeed after rollback"
        );
    }

    #[test]
    fn test_external_dependency_blocks_and_propagates_to_children() {
        let temp = TempDir::new().unwrap();
        let external_root = temp.path().join("extproj");
        let beads_dir = external_root.join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let db_path = beads_dir.join("beads.db");
        let _external_storage = SqliteStorage::open(&db_path).unwrap();

        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 3, 3, 0, 0, 0).unwrap();
        let parent = make_issue("bd-p1", "Parent", Status::Open, 2, None, t1, None);
        let child = make_issue("bd-c1", "Child", Status::Open, 2, None, t1, None);
        storage.create_issue(&parent, "tester").unwrap();
        storage.create_issue(&child, "tester").unwrap();

        // Parent (bd-p1) depends on external capability
        storage
            .add_dependency("bd-p1", "external:extproj:capability", "blocks", "tester")
            .unwrap();

        // Child (bd-c1) depends on Parent (bd-p1) via parent-child
        storage
            .add_dependency("bd-c1", "bd-p1", "parent-child", "tester")
            .unwrap();

        let mut external_db_paths = HashMap::new();
        external_db_paths.insert("extproj".to_string(), db_path);

        let statuses = storage
            .resolve_external_dependency_statuses(&external_db_paths, true)
            .unwrap();
        assert_eq!(statuses.get("external:extproj:capability"), Some(&false));

        let blockers = storage.external_blockers(&statuses).unwrap();
        let parent_blockers = blockers.get("bd-p1").expect("parent blockers");
        assert!(
            parent_blockers
                .iter()
                .any(|b| b.starts_with("external:extproj:capability"))
        );
        let child_blockers = blockers.get("bd-c1").expect("child blockers");
        assert!(child_blockers.iter().any(|b| b == "bd-p1:parent-blocked"));
    }

    #[test]
    fn test_add_dependency_rejects_external_parent_child_target() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 2, 0, 0, 0).unwrap();

        let issue_a = make_issue("bd-a1", "A", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue_a, "tester").unwrap();

        let err = storage
            .add_dependency(
                "bd-a1",
                "external:proj:capability",
                "parent-child",
                "tester",
            )
            .unwrap_err();

        assert!(matches!(err, BeadsError::Validation { field, .. } if field == "depends_on_id"));
    }

    #[test]
    fn test_has_external_dependencies_detects_external_parent_child_children() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 3, 3, 0, 0, 0).unwrap();
        let parent = make_issue("bd-p1", "Parent", Status::Open, 2, None, t1, None);
        storage.create_issue(&parent, "tester").unwrap();

        // Temporarily disable FK checks for the raw INSERT since
        // external:extproj:child doesn't exist in the issues table.
        storage.conn.execute("PRAGMA foreign_keys = OFF").unwrap();
        storage
            .conn
            .execute_with_params(
                "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by)
                 VALUES (?, ?, 'parent-child', ?, ?)",
                &[
                    SqliteValue::from("external:extproj:child"),
                    SqliteValue::from("bd-p1"),
                    SqliteValue::from(t1.to_rfc3339()),
                    SqliteValue::from("tester"),
                ],
            )
            .unwrap();
        storage.conn.execute("PRAGMA foreign_keys = ON").unwrap();

        assert!(storage.has_external_dependencies(true).unwrap());
        assert!(storage.has_external_dependencies(false).unwrap());
    }

    #[test]
    fn test_update_issue_changes_fields() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 5, 1, 0, 0, 0).unwrap();

        let issue = make_issue("bd-u1", "Update me", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue, "tester").unwrap();

        let updates = IssueUpdate {
            title: Some("Updated title".to_string()),
            description: Some(Some("New description".to_string())),
            status: Some(Status::InProgress),
            priority: Some(Priority::HIGH),
            assignee: Some(Some("alice".to_string())),
            ..IssueUpdate::default()
        };

        let updated = storage.update_issue("bd-u1", &updates, "tester").unwrap();
        assert_eq!(updated.title, "Updated title");
        assert_eq!(updated.status, Status::InProgress);
        assert_eq!(updated.priority, Priority::HIGH);
        assert_eq!(updated.assignee.as_deref(), Some("alice"));
        assert_eq!(updated.description.as_deref(), Some("New description"));
    }

    /*
    #[test]
    fn test_update_issue_recomputes_hash_from_fresh_transaction_state() {
        use std::sync::mpsc;
        use std::time::Duration;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("beads.db");

        let mut setup = SqliteStorage::open(&db_path).unwrap();
        let issue = make_issue(
            "bd-race1",
            "Original title",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        setup.create_issue(&issue, "tester").unwrap();
        drop(setup);

        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let writer_db_path = db_path.clone();

        let writer = std::thread::spawn(move || {
            let storage = SqliteStorage::open(&writer_db_path).unwrap();
            storage.conn.execute("BEGIN IMMEDIATE").unwrap();
            storage
                .conn
                .execute_with_params(
                    "UPDATE issues SET description = ?, updated_at = ? WHERE id = ?",
                    &[
                        SqliteValue::from("Thread description"),
                        SqliteValue::from(Utc::now().to_rfc3339()),
                        SqliteValue::from("bd-race1"),
                    ],
                )
                .unwrap();
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            storage.conn.execute("COMMIT").unwrap();
        });

        ready_rx.recv().unwrap();

        let updater_db_path = db_path;
        let updater = std::thread::spawn(move || {
            let mut storage = SqliteStorage::open(&updater_db_path).unwrap();
            let updates = IssueUpdate {
                title: Some("Updated title".to_string()),
                ..IssueUpdate::default()
            };
            storage
                .update_issue("bd-race1", &updates, "tester")
                .unwrap();
            storage.get_issue("bd-race1").unwrap().unwrap()
        });

        std::thread::sleep(Duration::from_millis(50));
        release_tx.send(()).unwrap();

        writer.join().unwrap();
        let updated = updater.join().unwrap();

        assert_eq!(updated.description.as_deref(), Some("Thread description"));
        assert_eq!(
            updated.content_hash.as_deref(),
            Some(crate::util::content_hash(&updated).as_str())
        );
    }
    */

    #[test]
    fn test_delete_issue_sets_tombstone() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap();

        let issue = make_issue("bd-d1", "Delete me", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue, "tester").unwrap();

        let deleted = storage
            .delete_issue("bd-d1", "tester", "cleanup", None)
            .unwrap();
        assert_eq!(deleted.status, Status::Tombstone);
        assert_eq!(deleted.delete_reason.as_deref(), Some("cleanup"));

        let is_tombstone = storage.is_tombstone("bd-d1").unwrap();
        assert!(is_tombstone);
    }

    #[test]
    fn test_reopen_records_reopened_event() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap();

        let issue = make_issue("bd-r1", "Reopen me", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue, "tester").unwrap();

        let close_update = IssueUpdate {
            status: Some(Status::Closed),
            ..IssueUpdate::default()
        };
        storage
            .update_issue("bd-r1", &close_update, "tester")
            .unwrap();

        let reopen_update = IssueUpdate {
            status: Some(Status::Open),
            closed_at: Some(None),
            close_reason: Some(None),
            closed_by_session: Some(None),
            ..IssueUpdate::default()
        };
        storage
            .update_issue("bd-r1", &reopen_update, "tester")
            .unwrap();

        let events = storage.get_events("bd-r1", 10).unwrap();
        println!("Events: {:#?}", events);
        assert!(
            events
                .iter()
                .any(|event| event.event_type == EventType::Reopened)
        );
    }

    #[test]
    fn test_delete_issue_recomputes_content_hash_for_tombstone() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap();

        let issue = make_issue("bd-d2", "Delete me too", Status::Open, 2, None, t1, None);
        let original_hash = issue.content_hash.clone();
        storage.create_issue(&issue, "tester").unwrap();

        let deleted = storage
            .delete_issue("bd-d2", "tester", "cleanup", None)
            .unwrap();

        assert_eq!(deleted.status, Status::Tombstone);
        assert_ne!(deleted.content_hash, original_hash);
        assert_eq!(
            deleted.content_hash.as_deref(),
            Some(crate::util::content_hash(&deleted).as_str())
        );
    }

    #[test]
    fn test_purge_issue_succeeds_without_fk_side_effects() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap();

        let issue = make_issue("bd-p1", "Purge me", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue, "tester").unwrap();

        storage.purge_issue("bd-p1", "tester").unwrap();

        assert!(storage.get_issue("bd-p1").unwrap().is_none());

        let dirty_count = storage
            .conn
            .query_row("SELECT COUNT(*) FROM dirty_issues WHERE issue_id = 'bd-p1'")
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap_or(0);
        assert_eq!(dirty_count, 0);

        let event_count = storage
            .conn
            .query_row("SELECT COUNT(*) FROM events WHERE issue_id = 'bd-p1'")
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap_or(0);
        assert_eq!(event_count, 0);
    }

    #[test]
    fn test_get_blocked_issues_lists_blockers() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 4, 1, 0, 0, 0).unwrap();

        let blocker = make_issue("bd-b1", "Blocker", Status::Open, 1, None, t1, None);
        let blocked = make_issue("bd-b2", "Blocked", Status::Open, 2, None, t1, None);
        storage.create_issue(&blocker, "tester").unwrap();
        storage.create_issue(&blocked, "tester").unwrap();

        storage
            .add_dependency("bd-b2", "bd-b1", "blocks", "tester")
            .unwrap();

        let blocked_issues = storage.get_blocked_issues().unwrap();
        assert_eq!(blocked_issues.len(), 1);
        assert_eq!(blocked_issues[0].0.id, "bd-b2");
        assert_eq!(blocked_issues[0].1.len(), 1);
    }

    #[test]
    fn test_get_blocked_issues_includes_nonterminal_statuses() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 4, 1, 0, 0, 0).unwrap();

        let blocker = make_issue("bd-b1", "Blocker", Status::Open, 1, None, t1, None);
        let mut deferred = make_issue(
            "bd-b2",
            "Deferred blocked",
            Status::Deferred,
            2,
            None,
            t1,
            None,
        );
        deferred.defer_until = Some(t1 + chrono::Duration::days(1));
        let custom = make_issue(
            "bd-b3",
            "Custom blocked",
            Status::Custom("review".to_string()),
            2,
            None,
            t1,
            None,
        );

        storage.create_issue(&blocker, "tester").unwrap();
        storage.create_issue(&deferred, "tester").unwrap();
        storage.create_issue(&custom, "tester").unwrap();

        storage
            .add_dependency("bd-b2", "bd-b1", "blocks", "tester")
            .unwrap();
        storage
            .add_dependency("bd-b3", "bd-b1", "blocks", "tester")
            .unwrap();

        let blocked_issues = storage.get_blocked_issues().unwrap();
        let ids: HashSet<_> = blocked_issues
            .iter()
            .map(|(issue, _)| issue.id.as_str())
            .collect();
        assert!(ids.contains("bd-b2"));
        assert!(ids.contains("bd-b3"));
    }

    #[test]
    fn test_add_and_remove_labels_sorted() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 1, 0, 0, 0).unwrap();

        let issue = make_issue("bd-l1", "Label me", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue, "tester").unwrap();

        let added = storage.add_label("bd-l1", "backend", "tester").unwrap();
        assert!(added);
        let added = storage.add_label("bd-l1", "api", "tester").unwrap();
        assert!(added);

        let labels = storage.get_labels("bd-l1").unwrap();
        assert_eq!(labels, vec!["api".to_string(), "backend".to_string()]);

        let removed = storage.remove_label("bd-l1", "api", "tester").unwrap();
        assert!(removed);
        let labels = storage.get_labels("bd-l1").unwrap();
        assert_eq!(labels, vec!["backend".to_string()]);
    }

    #[test]
    fn test_set_labels_deduplicates_input() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 1, 0, 0, 0).unwrap();

        let issue = make_issue("bd-l2", "Dedup labels", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue, "tester").unwrap();

        storage
            .set_labels(
                "bd-l2",
                &[
                    "backend".to_string(),
                    "backend".to_string(),
                    "api".to_string(),
                ],
                "tester",
            )
            .unwrap();

        let labels = storage.get_labels("bd-l2").unwrap();
        assert_eq!(labels, vec!["api".to_string(), "backend".to_string()]);
    }

    #[test]
    fn test_rename_label_same_name_is_noop() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 1, 0, 0, 0).unwrap();

        let issue = make_issue("bd-l3", "Rename label", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue, "tester").unwrap();
        storage.add_label("bd-l3", "backend", "tester").unwrap();
        let event_count_before = storage.get_events("bd-l3", 100).unwrap().len();

        let affected = storage
            .rename_label("backend", "backend", "tester")
            .unwrap();

        assert_eq!(affected, 0);
        assert_eq!(
            storage.get_labels("bd-l3").unwrap(),
            vec!["backend".to_string()]
        );
        assert_eq!(
            storage.get_events("bd-l3", 100).unwrap().len(),
            event_count_before
        );
    }

    #[test]
    fn test_add_dependency_and_remove() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 2, 0, 0, 0).unwrap();

        let issue_a = make_issue("bd-a1", "A", Status::Open, 2, None, t1, None);
        let issue_b = make_issue("bd-b1", "B", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue_a, "tester").unwrap();
        storage.create_issue(&issue_b, "tester").unwrap();

        let added = storage
            .add_dependency("bd-a1", "bd-b1", "blocks", "tester")
            .unwrap();
        assert!(added);

        let added = storage
            .add_dependency("bd-a1", "bd-b1", "blocks", "tester")
            .unwrap();
        assert!(!added);

        let deps = storage.get_dependencies("bd-a1").unwrap();
        assert_eq!(deps, vec!["bd-b1".to_string()]);

        let removed = storage
            .remove_dependency("bd-a1", "bd-b1", "tester")
            .unwrap();
        assert!(removed);
        let deps = storage.get_dependencies("bd-a1").unwrap();
        assert!(deps.is_empty());
    }

    #[test]
    fn test_add_dependency_rejects_missing_target() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 2, 0, 0, 0).unwrap();

        let issue_a = make_issue("bd-a1", "A", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue_a, "tester").unwrap();

        let err = storage
            .add_dependency("bd-a1", "bd-missing", "blocks", "tester")
            .unwrap_err();

        assert!(matches!(err, BeadsError::IssueNotFound { id } if id == "bd-missing"));
    }

    #[test]
    fn test_add_dependency_with_metadata_persists_json() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 2, 0, 0, 0).unwrap();

        let issue_a = make_issue("bd-a1", "A", Status::Open, 2, None, t1, None);
        let issue_b = make_issue("bd-b1", "B", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue_a, "tester").unwrap();
        storage.create_issue(&issue_b, "tester").unwrap();

        storage
            .add_dependency_with_metadata(
                "bd-a1",
                "bd-b1",
                "blocks",
                "tester",
                Some(r#"{"source":"cli","reason":"gate"}"#),
            )
            .unwrap();

        let deps = storage.get_dependencies_full("bd-a1").unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(
            deps[0].metadata.as_deref(),
            Some(r#"{"source":"cli","reason":"gate"}"#)
        );
    }

    #[test]
    fn test_add_dependency_with_metadata_rejects_invalid_json() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 2, 0, 0, 0).unwrap();

        let issue_a = make_issue("bd-a1", "A", Status::Open, 2, None, t1, None);
        let issue_b = make_issue("bd-b1", "B", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue_a, "tester").unwrap();
        storage.create_issue(&issue_b, "tester").unwrap();

        let err = storage
            .add_dependency_with_metadata("bd-a1", "bd-b1", "blocks", "tester", Some("{not-json"))
            .unwrap_err();

        assert!(matches!(err, BeadsError::Validation { field, .. } if field == "metadata"));
    }

    #[test]
    fn test_find_ids_by_hash_only_matches_hash_portion() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 2, 0, 0, 0).unwrap();

        let issue_a = make_issue("my-proj-abc123", "Alpha", Status::Open, 2, None, t1, None);
        let issue_b = make_issue("other-proj-xyz789", "Beta", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue_a, "tester").unwrap();
        storage.create_issue(&issue_b, "tester").unwrap();

        let matches = storage.find_ids_by_hash("proj").unwrap();
        assert!(
            matches.is_empty(),
            "prefix fragments must not match hash lookup"
        );
        assert_eq!(
            storage.find_ids_by_hash("abc").unwrap(),
            vec!["my-proj-abc123".to_string()]
        );
    }

    #[test]
    fn test_get_dependencies_with_metadata_external_placeholder() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 2, 0, 0, 0).unwrap();

        let issue_a = make_issue("bd-a1", "A", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue_a, "tester").unwrap();
        storage
            .add_dependency("bd-a1", "external:proj:capability", "blocks", "tester")
            .unwrap();

        let deps = storage.get_dependencies_with_metadata("bd-a1").unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].id, "external:proj:capability");
        assert_eq!(deps[0].title, "proj:capability");
        assert_eq!(deps[0].status, Status::Blocked);
        assert_eq!(deps[0].priority, Priority::MEDIUM);
        assert_eq!(deps[0].dep_type, "blocks");
    }

    #[test]
    fn test_get_dependencies_with_metadata_errors_on_missing_internal_target() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 2, 0, 0, 0).unwrap();

        let issue_a = make_issue("bd-a1", "A", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue_a, "tester").unwrap();

        let created_at = Utc::now().to_rfc3339();
        storage
            .execute_test_sql(&format!(
                "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by)
                 VALUES ('bd-a1', 'bd-missing', 'blocks', '{created_at}', 'tester')"
            ))
            .unwrap();

        let deps = storage
            .get_dependencies_with_metadata("bd-a1")
            .expect("should return placeholder for missing dependency");

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].id, "bd-missing");
        assert_eq!(deps[0].title, "[missing issue: bd-missing]");
        assert_eq!(deps[0].status, Status::Tombstone);
    }

    #[test]
    fn test_get_dependents_with_metadata_errors_on_missing_dependent_issue() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 2, 0, 0, 0).unwrap();

        let issue_b = make_issue("bd-b1", "B", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue_b, "tester").unwrap();

        let created_at = Utc::now().to_rfc3339();
        storage
            .execute_test_sql(&format!(
                "PRAGMA foreign_keys = OFF;
                 INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by)
                 VALUES ('bd-missing', 'bd-b1', 'blocks', '{created_at}', 'tester');
                 PRAGMA foreign_keys = ON;"
            ))
            .unwrap();

        let deps = storage
            .get_dependents_with_metadata("bd-b1")
            .expect("should return placeholder for missing dependent");

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].id, "bd-missing");
        assert_eq!(deps[0].title, "[missing issue: bd-missing]");
        assert_eq!(deps[0].status, Status::Tombstone);
    }

    #[test]
    fn test_would_create_cycle_detects_cycle() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 3, 0, 0, 0).unwrap();

        let issue_a = make_issue("bd-cy1", "A", Status::Open, 2, None, t1, None);
        let issue_b = make_issue("bd-cy2", "B", Status::Open, 2, None, t1, None);
        let issue_c = make_issue("bd-cy3", "C", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue_a, "tester").unwrap();
        storage.create_issue(&issue_b, "tester").unwrap();
        storage.create_issue(&issue_c, "tester").unwrap();

        storage
            .add_dependency("bd-cy1", "bd-cy2", "blocks", "tester")
            .unwrap();
        storage
            .add_dependency("bd-cy2", "bd-cy3", "blocks", "tester")
            .unwrap();

        let creates_cycle = storage
            .would_create_cycle("bd-cy3", "bd-cy1", true)
            .unwrap();
        assert!(creates_cycle);
    }

    #[test]
    fn test_get_comments_orders_by_created_at() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 4, 0, 0, 0).unwrap();

        let issue = Issue {
            id: "bd-c1".to_string(),
            content_hash: None,
            title: "Comment issue".to_string(),
            description: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_at: t1,
            created_by: None,
            updated_at: t1,
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            defer_until: None,
            due_at: None,
            external_ref: None,
            source_system: None,
            source_repo: None,
            deleted_at: None,
            deleted_by: None,
            delete_reason: None,
            original_type: None,
            compaction_level: None,
            compacted_at: None,
            compacted_at_commit: None,
            original_size: None,
            sender: None,
            ephemeral: false,
            pinned: false,
            is_template: false,
            labels: vec![],
            dependencies: vec![],
            comments: vec![],
        };
        storage.create_issue(&issue, "tester").unwrap();

        storage
            .conn
            .execute_with_params(
                "INSERT INTO comments (issue_id, author, text, created_at) VALUES (?, ?, ?, ?)",
                &[
                    SqliteValue::from("bd-c1"),
                    SqliteValue::from("alice"),
                    SqliteValue::from("first"),
                    SqliteValue::from("2025-07-01T00:00:00Z"),
                ],
            )
            .unwrap();
        storage
            .conn
            .execute_with_params(
                "INSERT INTO comments (issue_id, author, text, created_at) VALUES (?, ?, ?, ?)",
                &[
                    SqliteValue::from("bd-c1"),
                    SqliteValue::from("bob"),
                    SqliteValue::from("second"),
                    SqliteValue::from("2025-07-02T00:00:00Z"),
                ],
            )
            .unwrap();

        let comments = storage.get_comments("bd-c1").unwrap();
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].author, "alice");
        assert_eq!(comments[1].author, "bob");
    }

    #[test]
    fn test_get_comments_errors_on_invalid_timestamp() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 4, 0, 0, 0).unwrap();

        let issue = Issue {
            id: "bd-c-invalid".to_string(),
            content_hash: None,
            title: "Comment issue".to_string(),
            description: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_at: t1,
            created_by: None,
            updated_at: t1,
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            defer_until: None,
            due_at: None,
            external_ref: None,
            source_system: None,
            source_repo: None,
            deleted_at: None,
            deleted_by: None,
            delete_reason: None,
            original_type: None,
            compaction_level: None,
            compacted_at: None,
            compacted_at_commit: None,
            original_size: None,
            sender: None,
            ephemeral: false,
            pinned: false,
            is_template: false,
            labels: vec![],
            dependencies: vec![],
            comments: vec![],
        };
        storage.create_issue(&issue, "tester").unwrap();

        storage
            .conn
            .execute_with_params(
                "INSERT INTO comments (issue_id, author, text, created_at) VALUES (?, ?, ?, ?)",
                &[
                    SqliteValue::from("bd-c-invalid"),
                    SqliteValue::from("alice"),
                    SqliteValue::from("first"),
                    SqliteValue::from("not-a-real-timestamp"),
                ],
            )
            .unwrap();

        let err = storage.get_comments("bd-c-invalid").unwrap_err();
        match err {
            BeadsError::Config(msg) => {
                assert!(msg.contains("invalid comment timestamp"));
                assert!(msg.contains("unparseable datetime"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[test]
    fn test_add_comment_round_trip() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 4, 0, 0, 0).unwrap();

        let issue = Issue {
            id: "bd-c2".to_string(),
            content_hash: None,
            title: "Comment issue".to_string(),
            description: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_at: t1,
            created_by: None,
            updated_at: t1,
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            defer_until: None,
            due_at: None,
            external_ref: None,
            source_system: None,
            source_repo: None,
            deleted_at: None,
            deleted_by: None,
            delete_reason: None,
            original_type: None,
            compaction_level: None,
            compacted_at: None,
            compacted_at_commit: None,
            original_size: None,
            sender: None,
            ephemeral: false,
            pinned: false,
            is_template: false,
            labels: vec![],
            dependencies: vec![],
            comments: vec![],
        };
        storage.create_issue(&issue, "tester").unwrap();

        let comment = storage
            .add_comment("bd-c2", "alice", "Hello there")
            .unwrap();
        assert_eq!(comment.issue_id, "bd-c2");
        assert_eq!(comment.author, "alice");
        assert_eq!(comment.body, "Hello there");
        assert!(comment.id > 0);

        let comments = storage.get_comments("bd-c2").unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0], comment);
    }

    #[test]
    fn test_sync_comments_for_import_preserves_comments_on_other_issues() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 4, 0, 0, 0).unwrap();

        let issue_a = make_issue(
            "bd-c-import-a",
            "Import target",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        let issue_b = make_issue(
            "bd-c-import-b",
            "Existing comment owner",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&issue_a, "tester").unwrap();
        storage.create_issue(&issue_b, "tester").unwrap();

        let existing_comment = storage
            .add_comment("bd-c-import-b", "bob", "Existing comment")
            .unwrap();

        let imported_comment = crate::model::Comment {
            id: existing_comment.id,
            issue_id: "bd-c-import-a".to_string(),
            author: "alice".to_string(),
            body: "Imported comment".to_string(),
            created_at: t1 + chrono::Duration::minutes(5),
        };
        storage
            .sync_comments_for_import("bd-c-import-a", &[imported_comment])
            .unwrap();

        let comments_a = storage.get_comments("bd-c-import-a").unwrap();
        assert_eq!(comments_a.len(), 1);
        assert_eq!(comments_a[0].issue_id, "bd-c-import-a");
        assert_eq!(comments_a[0].body, "Imported comment");
        assert_ne!(comments_a[0].id, existing_comment.id);

        let comments_b = storage.get_comments("bd-c-import-b").unwrap();
        assert_eq!(comments_b, vec![existing_comment]);
    }

    #[test]
    fn test_external_project_capabilities_ignore_tombstones() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("external.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 4, 0, 0, 0).unwrap();

        let mut closed_issue = make_issue(
            "bd-cap-closed",
            "Closed provider",
            Status::Closed,
            2,
            None,
            t1,
            None,
        );
        closed_issue.closed_at = Some(t1);
        let mut tombstone_issue = make_issue(
            "bd-cap-tombstone",
            "Deleted provider",
            Status::Tombstone,
            2,
            None,
            t1,
            None,
        );
        tombstone_issue.deleted_at = Some(t1);
        tombstone_issue.delete_reason = Some("deleted".to_string());

        storage.create_issue(&closed_issue, "tester").unwrap();
        storage.create_issue(&tombstone_issue, "tester").unwrap();
        storage
            .add_label("bd-cap-closed", "provides:closed-cap", "tester")
            .unwrap();
        storage
            .conn
            .execute_with_params(
                "INSERT INTO labels (issue_id, label) VALUES (?, ?)",
                &[
                    SqliteValue::from("bd-cap-tombstone"),
                    SqliteValue::from("provides:deleted-cap"),
                ],
            )
            .unwrap();
        drop(storage);

        let capabilities = HashSet::from(["closed-cap".to_string(), "deleted-cap".to_string()]);
        let satisfied = query_external_project_capabilities(&db_path, &capabilities).unwrap();

        assert!(satisfied.contains("closed-cap"));
        assert!(!satisfied.contains("deleted-cap"));
    }

    #[test]
    fn test_add_comment_marks_dirty() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 4, 0, 0, 0).unwrap();

        let issue = Issue {
            id: "bd-c3".to_string(),
            content_hash: None,
            title: "Comment issue".to_string(),
            description: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_at: t1,
            created_by: None,
            updated_at: t1,
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            defer_until: None,
            due_at: None,
            external_ref: None,
            source_system: None,
            source_repo: None,
            deleted_at: None,
            deleted_by: None,
            delete_reason: None,
            original_type: None,
            compaction_level: None,
            compacted_at: None,
            compacted_at_commit: None,
            original_size: None,
            sender: None,
            ephemeral: false,
            pinned: false,
            is_template: false,
            labels: vec![],
            dependencies: vec![],
            comments: vec![],
        };
        storage.create_issue(&issue, "tester").unwrap();

        storage
            .add_comment("bd-c3", "alice", "Dirty comment")
            .unwrap();

        let dirty_count = storage
            .conn
            .query_row_with_params(
                "SELECT count(*) FROM dirty_issues WHERE issue_id = ?",
                &[SqliteValue::from("bd-c3")],
            )
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap_or(0);
        assert_eq!(dirty_count, 1);
    }

    #[test]
    fn test_events_have_timestamps() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue = make_issue(
            "bd-e1",
            "Event Test",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        storage.create_issue(&issue, "tester").unwrap();

        // Verify event has timestamp
        let created_at: String = storage
            .conn
            .query_row_with_params(
                "SELECT created_at FROM events WHERE issue_id = ?",
                &[SqliteValue::from("bd-e1")],
            )
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_text)
            .unwrap_or("")
            .to_string();

        // Should be a valid RFC3339 timestamp
        assert!(
            chrono::DateTime::parse_from_rfc3339(&created_at).is_ok(),
            "Event timestamp should be valid RFC3339"
        );
    }

    #[test]
    fn test_blocked_cache_invalidation() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        // Create issues first (required for FK constraints on events table)
        let issue1 = make_issue(
            "bd-c1",
            "Cached issue",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        storage.create_issue(&issue1, "tester").unwrap();

        let issue2 = make_issue(
            "bd-b1",
            "Blocker issue",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        storage.create_issue(&issue2, "tester").unwrap();

        // Manually insert some cache data (DELETE + INSERT for fsqlite safety)
        storage
            .conn
            .execute_with_params(
                "DELETE FROM blocked_issues_cache WHERE issue_id = ?",
                &[SqliteValue::from("bd-c1")],
            )
            .unwrap();
        storage
            .conn
            .execute_with_params(
                "INSERT INTO blocked_issues_cache (issue_id, blocked_by) VALUES (?, ?)",
                &[
                    SqliteValue::from("bd-c1"),
                    SqliteValue::from(r#"["bd-b1"]"#),
                ],
            )
            .unwrap();

        // Verify cache has data
        let count = storage
            .conn
            .query_row_with_params(
                "SELECT count(*) FROM blocked_issues_cache WHERE issue_id = ?",
                &[SqliteValue::from("bd-c1")],
            )
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap_or(0);
        assert_eq!(count, 1);

        // Now add a non-blocking dependency type ("related" doesn't block)
        storage
            .add_dependency("bd-c1", "bd-b1", "related", "tester")
            .unwrap();

        // With deferred cache refresh, add_dependency now marks the cache stale
        // rather than rebuilding immediately.  The stale entry persists until a
        // read that calls ensure_blocked_cache_fresh triggers the lazy rebuild.
        // Verify the stale marker was set.
        assert!(
            storage.blocked_cache_marked_stale().unwrap(),
            "cache should be marked stale after add_dependency"
        );

        // Trigger a read — is_blocked calls ensure_blocked_cache_fresh internally.
        let blocked = storage.is_blocked("bd-c1").unwrap();
        assert!(
            !blocked,
            "bd-c1 should not be blocked after adding only a 'related' dep"
        );

        // After the lazy rebuild the stale marker should be cleared.
        assert!(
            !storage.blocked_cache_marked_stale().unwrap(),
            "cache should no longer be stale after lazy rebuild"
        );

        // And the old stale entry should be gone.
        let count = storage
            .conn
            .query_row_with_params(
                "SELECT count(*) FROM blocked_issues_cache WHERE issue_id = ?",
                &[SqliteValue::from("bd-c1")],
            )
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap_or(0);
        assert_eq!(count, 0);
    }

    #[test]
    fn test_update_issue_skip_cache_rebuild_marks_cache_stale_for_lazy_refresh() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let blocker = make_issue(
            "bd-blocker",
            "Blocker",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        let blocked = make_issue(
            "bd-blocked",
            "Blocked",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        storage.create_issue(&blocker, "tester").unwrap();
        storage.create_issue(&blocked, "tester").unwrap();
        storage
            .add_dependency(
                &blocked.id,
                &blocker.id,
                DependencyType::Blocks.as_str(),
                "tester",
            )
            .unwrap();

        assert!(storage.is_blocked(&blocked.id).unwrap());
        assert!(!storage.blocked_cache_marked_stale().unwrap());

        let close_update = IssueUpdate {
            status: Some(Status::Closed),
            skip_cache_rebuild: true,
            ..IssueUpdate::default()
        };
        storage
            .update_issue(&blocker.id, &close_update, "tester")
            .unwrap();

        assert!(
            storage.blocked_cache_marked_stale().unwrap(),
            "status updates with skip_cache_rebuild should leave a stale marker behind"
        );
        assert!(
            !storage.is_blocked(&blocked.id).unwrap(),
            "lazy refresh should rebuild the cache before blocked lookups"
        );
        assert!(
            !storage.blocked_cache_marked_stale().unwrap(),
            "lazy refresh should clear the stale marker after rebuilding"
        );
    }

    #[test]
    fn test_set_parent_skip_cache_rebuild_marks_cache_stale_for_lazy_refresh() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let blocker = make_issue(
            "bd-parent-blocker",
            "Parent blocker",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        let parent = make_issue(
            "bd-parent",
            "Parent",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        let child = make_issue("bd-child", "Child", Status::Open, 2, None, Utc::now(), None);
        storage.create_issue(&blocker, "tester").unwrap();
        storage.create_issue(&parent, "tester").unwrap();
        storage.create_issue(&child, "tester").unwrap();
        storage
            .add_dependency(
                &parent.id,
                &blocker.id,
                DependencyType::Blocks.as_str(),
                "tester",
            )
            .unwrap();

        assert!(storage.is_blocked(&parent.id).unwrap());
        assert!(!storage.blocked_cache_marked_stale().unwrap());

        storage
            .set_parent_with_options(&child.id, Some(&parent.id), "tester", true)
            .unwrap();

        assert!(
            storage.blocked_cache_marked_stale().unwrap(),
            "parent changes with skip_cache_rebuild should leave a stale marker behind"
        );
        assert!(
            storage.is_blocked(&child.id).unwrap(),
            "lazy refresh should propagate parent blockers to the child"
        );
        assert!(
            !storage.blocked_cache_marked_stale().unwrap(),
            "lazy refresh should clear the stale marker after rebuilding"
        );
    }

    #[test]
    fn test_expand_blocked_cache_component_includes_parent_and_siblings() {
        let children_by_parent = HashMap::from([
            (
                "bd-root".to_string(),
                vec!["bd-parent".to_string(), "bd-aunt".to_string()],
            ),
            (
                "bd-parent".to_string(),
                vec!["bd-parent.1".to_string(), "bd-parent.2".to_string()],
            ),
        ]);
        let parents_by_child = SqliteStorage::build_parents_by_child(&children_by_parent);
        let seed_ids = HashSet::from(["bd-parent.1".to_string()]);

        let affected = SqliteStorage::expand_blocked_cache_component(
            &seed_ids,
            &children_by_parent,
            &parents_by_child,
        );

        assert!(affected.contains("bd-parent.1"));
        assert!(affected.contains("bd-parent"));
        assert!(affected.contains("bd-parent.2"));
        assert!(affected.contains("bd-root"));
        assert!(affected.contains("bd-aunt"));
    }

    #[test]
    fn test_incremental_blocked_cache_update_recomputes_entire_parent_child_component() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let now = Utc::now();

        for issue in [
            make_issue("bd-parent", "Parent", Status::Open, 2, None, now, None),
            make_issue("bd-parent.1", "Child 1", Status::Open, 2, None, now, None),
            make_issue("bd-parent.2", "Child 2", Status::Open, 2, None, now, None),
            make_issue("bd-blocker", "Blocker", Status::Open, 2, None, now, None),
            make_issue(
                "bd-unrelated",
                "Unrelated",
                Status::Open,
                2,
                None,
                now,
                None,
            ),
            make_issue(
                "bd-unrelated-blocker",
                "Unrelated blocker",
                Status::Open,
                2,
                None,
                now,
                None,
            ),
        ] {
            storage.create_issue(&issue, "tester").unwrap();
        }

        storage
            .add_dependency("bd-parent.1", "bd-parent", "parent-child", "tester")
            .unwrap();
        storage
            .add_dependency("bd-parent.2", "bd-parent", "parent-child", "tester")
            .unwrap();
        storage
            .add_dependency("bd-parent", "bd-blocker", "blocks", "tester")
            .unwrap();
        storage
            .add_dependency("bd-unrelated", "bd-unrelated-blocker", "blocks", "tester")
            .unwrap();

        assert!(storage.is_blocked("bd-parent").unwrap());
        assert!(storage.is_blocked("bd-parent.1").unwrap());
        assert!(storage.is_blocked("bd-parent.2").unwrap());
        assert!(storage.is_blocked("bd-unrelated").unwrap());

        storage
            .conn
            .execute_with_params(
                "DELETE FROM dependencies WHERE issue_id = ? AND depends_on_id = ?",
                &[
                    SqliteValue::from("bd-parent"),
                    SqliteValue::from("bd-blocker"),
                ],
            )
            .unwrap();

        let seed_ids = HashSet::from(["bd-parent.1".to_string()]);
        SqliteStorage::incremental_blocked_cache_update(&storage.conn, &seed_ids).unwrap();

        let parent_blockers = storage.get_blockers("bd-parent").unwrap();
        assert_eq!(
            parent_blockers,
            vec!["bd-parent.1".to_string(), "bd-parent.2".to_string()]
        );
        assert!(storage.get_blockers("bd-parent.1").unwrap().is_empty());
        assert!(storage.get_blockers("bd-parent.2").unwrap().is_empty());
        assert_eq!(
            storage.get_blockers("bd-unrelated").unwrap(),
            vec!["bd-unrelated-blocker".to_string()]
        );
    }

    #[test]
    fn test_get_blocked_ids_and_is_blocked_fall_back_when_cache_table_missing() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let blocker = make_issue("bd-b1", "Blocker", Status::Open, 2, None, Utc::now(), None);
        let blocked = make_issue(
            "bd-c1",
            "Blocked issue",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        storage.create_issue(&blocker, "tester").unwrap();
        storage.create_issue(&blocked, "tester").unwrap();
        storage
            .add_dependency("bd-c1", "bd-b1", "blocks", "tester")
            .unwrap();

        storage
            .conn
            .execute("DROP TABLE blocked_issues_cache")
            .unwrap();

        let blocked_ids = storage.get_blocked_ids().unwrap();
        assert!(blocked_ids.contains("bd-c1"));
        assert!(storage.is_blocked("bd-c1").unwrap());
    }

    #[test]
    fn test_get_ready_issues_fall_back_when_cache_table_missing() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let blocker = make_issue("bd-b1", "Blocker", Status::Open, 1, None, Utc::now(), None);
        let blocked = make_issue(
            "bd-c1",
            "Blocked issue",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        let ready = make_issue(
            "bd-r1",
            "Ready issue",
            Status::Open,
            3,
            None,
            Utc::now(),
            None,
        );
        storage.create_issue(&blocker, "tester").unwrap();
        storage.create_issue(&blocked, "tester").unwrap();
        storage.create_issue(&ready, "tester").unwrap();
        storage
            .add_dependency("bd-c1", "bd-b1", "blocks", "tester")
            .unwrap();

        storage
            .conn
            .execute("DROP TABLE blocked_issues_cache")
            .unwrap();

        let ready_ids: HashSet<_> = storage
            .get_ready_issues(&ReadyFilters::default(), ReadySortPolicy::Priority)
            .unwrap()
            .into_iter()
            .map(|issue| issue.id)
            .collect();
        assert!(!ready_ids.contains("bd-c1"));
        assert!(ready_ids.contains("bd-b1"));
        assert!(ready_ids.contains("bd-r1"));
    }

    #[test]
    fn test_get_ready_issues_for_command_output_matches_full_ready_output() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let created_at = Utc.with_ymd_and_hms(2026, 3, 20, 12, 0, 0).unwrap();

        let mut ready = make_issue(
            "bd-ready-detailed",
            "Detailed ready issue",
            Status::Open,
            1,
            Some("alice"),
            created_at,
            None,
        );
        ready.description = Some("Description".to_string());
        ready.design = Some("Should not be loaded for ready output".to_string());
        ready.acceptance_criteria = Some("AC".to_string());
        ready.notes = Some("Notes".to_string());
        ready.owner = Some("product".to_string());
        ready.estimated_minutes = Some(45);
        ready.created_by = Some("agent".to_string());
        ready.updated_at = created_at + chrono::Duration::minutes(5);
        ready.external_ref = Some("jira-123".to_string());
        ready.source_system = Some("jira".to_string());
        ready.source_repo = Some("proj".to_string());
        ready.sender = Some("cli".to_string());

        let blocker = make_issue(
            "bd-blocker-detailed",
            "Blocker",
            Status::Open,
            0,
            None,
            created_at + chrono::Duration::minutes(1),
            None,
        );
        let blocked = make_issue(
            "bd-blocked-detailed",
            "Blocked",
            Status::Open,
            2,
            None,
            created_at + chrono::Duration::minutes(2),
            None,
        );

        storage.create_issue(&ready, "tester").unwrap();
        storage.create_issue(&blocker, "tester").unwrap();
        storage.create_issue(&blocked, "tester").unwrap();
        storage
            .add_dependency(
                "bd-blocked-detailed",
                "bd-blocker-detailed",
                "blocks",
                "tester",
            )
            .unwrap();

        let filters = ReadyFilters::default();
        let full: Vec<_> = storage
            .get_ready_issues(&filters, ReadySortPolicy::Priority)
            .unwrap()
            .into_iter()
            .map(ReadyIssue::from)
            .collect();
        let projected: Vec<_> = storage
            .get_ready_issues_for_command_output(&filters, ReadySortPolicy::Priority)
            .unwrap()
            .into_iter()
            .map(ReadyIssue::from)
            .collect();

        assert_eq!(projected, full);
    }

    #[test]
    fn test_get_ready_issues_for_command_output_falls_back_when_cache_table_missing() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let blocker = make_issue("bd-b1", "Blocker", Status::Open, 1, None, Utc::now(), None);
        let blocked = make_issue(
            "bd-c1",
            "Blocked issue",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        let ready = make_issue(
            "bd-r1",
            "Ready issue",
            Status::Open,
            3,
            None,
            Utc::now(),
            None,
        );
        storage.create_issue(&blocker, "tester").unwrap();
        storage.create_issue(&blocked, "tester").unwrap();
        storage.create_issue(&ready, "tester").unwrap();
        storage
            .add_dependency("bd-c1", "bd-b1", "blocks", "tester")
            .unwrap();

        storage
            .conn
            .execute("DROP TABLE blocked_issues_cache")
            .unwrap();

        let full: Vec<_> = storage
            .get_ready_issues(&ReadyFilters::default(), ReadySortPolicy::Priority)
            .unwrap()
            .into_iter()
            .map(ReadyIssue::from)
            .collect();
        let projected: Vec<_> = storage
            .get_ready_issues_for_command_output(
                &ReadyFilters::default(),
                ReadySortPolicy::Priority,
            )
            .unwrap()
            .into_iter()
            .map(ReadyIssue::from)
            .collect();

        assert_eq!(projected, full);
    }

    #[test]
    fn test_get_blockers_fall_back_on_malformed_cache_json() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let blocker = make_issue("bd-b1", "Blocker", Status::Open, 2, None, Utc::now(), None);
        let issue = make_issue(
            "bd-c1",
            "Blocked issue",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        storage.create_issue(&blocker, "tester").unwrap();
        storage.create_issue(&issue, "tester").unwrap();
        storage
            .add_dependency("bd-c1", "bd-b1", "blocks", "tester")
            .unwrap();

        storage
            .conn
            .execute_with_params(
                "UPDATE blocked_issues_cache SET blocked_by = ? WHERE issue_id = ?",
                &[SqliteValue::from("not-json"), SqliteValue::from("bd-c1")],
            )
            .unwrap();

        assert_eq!(
            storage.get_blockers("bd-c1").unwrap(),
            vec!["bd-b1".to_string()]
        );
    }

    #[test]
    fn test_get_blocked_issues_fall_back_on_malformed_cache_json() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let blocker = make_issue("bd-b1", "Blocker", Status::Open, 1, None, Utc::now(), None);
        let blocked = make_issue(
            "bd-c1",
            "Blocked issue",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        storage.create_issue(&blocker, "tester").unwrap();
        storage.create_issue(&blocked, "tester").unwrap();
        storage
            .add_dependency("bd-c1", "bd-b1", "blocks", "tester")
            .unwrap();

        storage
            .conn
            .execute_with_params(
                "UPDATE blocked_issues_cache SET blocked_by = ? WHERE issue_id = ?",
                &[SqliteValue::from("not-json"), SqliteValue::from("bd-c1")],
            )
            .unwrap();

        let blocked_issues = storage.get_blocked_issues().unwrap();
        assert_eq!(blocked_issues.len(), 1);
        assert_eq!(blocked_issues[0].0.id, "bd-c1");
        assert!(
            blocked_issues[0]
                .1
                .iter()
                .any(|blocker_ref| blocker_ref.starts_with("bd-b1:")),
            "fallback should preserve blocker metadata, got: {:?}",
            blocked_issues[0].1
        );
    }

    #[test]
    fn test_blocker_helpers_ignore_non_blocking_related_edges() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 3, 3, 0, 0, 0).unwrap();

        let blocker = make_issue("bd-b1", "Blocker", Status::Open, 2, None, t1, None);
        let blocked = make_issue("bd-c1", "Blocked", Status::Open, 2, None, t1, None);
        let parent = make_issue("bd-p1", "Parent", Status::Open, 2, None, t1, None);
        let child = make_issue("bd-p1.1", "Child", Status::Open, 2, None, t1, None);
        let related = make_issue("bd-r1", "Related", Status::Open, 2, None, t1, None);

        storage.create_issue(&blocker, "tester").unwrap();
        storage.create_issue(&blocked, "tester").unwrap();
        storage.create_issue(&parent, "tester").unwrap();
        storage.create_issue(&child, "tester").unwrap();
        storage.create_issue(&related, "tester").unwrap();

        storage
            .add_dependency("bd-c1", "bd-b1", "blocks", "tester")
            .unwrap();
        storage
            .add_dependency("bd-p1.1", "bd-p1", "parent-child", "tester")
            .unwrap();
        storage
            .add_dependency("bd-r1", "bd-b1", "related", "tester")
            .unwrap();

        let blocker_ids = storage.get_blocker_ids("bd-c1").unwrap();
        assert_eq!(blocker_ids, vec!["bd-b1"]);

        let parent_blockers = storage.get_blocker_ids("bd-p1").unwrap();
        assert_eq!(parent_blockers, vec!["bd-p1.1"]);

        let related_blockers = storage.get_blocker_ids("bd-r1").unwrap();
        assert!(
            related_blockers.is_empty(),
            "non-blocking related edges should not be reported as blockers"
        );

        let blocked_issue_ids = storage.get_blocked_issue_ids("bd-b1").unwrap();
        assert_eq!(blocked_issue_ids, vec!["bd-c1"]);

        let child_blocked_issue_ids = storage.get_blocked_issue_ids("bd-p1.1").unwrap();
        assert_eq!(child_blocked_issue_ids, vec!["bd-p1"]);
    }

    #[test]
    fn test_update_issue_recomputes_hash() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let mut issue = make_issue(
            "bd-h1",
            "Old Title",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        issue.content_hash = Some(issue.compute_content_hash());
        storage.create_issue(&issue, "tester").unwrap();

        // Get initial hash
        let initial = storage.get_issue("bd-h1").unwrap().unwrap();
        let initial_hash = initial.content_hash.unwrap();

        // Update title
        let update = IssueUpdate {
            title: Some("New Title".to_string()),
            ..IssueUpdate::default()
        };
        storage.update_issue("bd-h1", &update, "tester").unwrap();

        // Check new hash
        let updated = storage.get_issue("bd-h1").unwrap().unwrap();
        let updated_hash = updated.content_hash.unwrap();

        assert_ne!(
            initial_hash, updated_hash,
            "Hash should change when title changes"
        );
    }

    #[test]
    fn test_delete_config() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        // Set a config value
        storage.set_config("test_key", "test_value").unwrap();
        assert_eq!(
            storage.get_config("test_key").unwrap(),
            Some("test_value".to_string())
        );

        // Delete it
        let deleted = storage.delete_config("test_key").unwrap();
        assert!(deleted, "Should return true when key existed");
        assert_eq!(storage.get_config("test_key").unwrap(), None);

        // Delete non-existent key
        let deleted_again = storage.delete_config("nonexistent").unwrap();
        assert!(!deleted_again, "Should return false when key doesn't exist");
    }

    #[test]
    fn test_open_creates_database() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("new_db.db");

        assert!(!db_path.exists(), "Database should not exist yet");

        let _storage = SqliteStorage::open(&db_path).unwrap();

        assert!(db_path.exists(), "Database file should be created");
    }

    #[test]
    fn test_database_header_user_version_reads_file_header_value() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("header_user_version.db");

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        conn.execute(&format!("PRAGMA user_version = {CURRENT_SCHEMA_VERSION}"))
            .unwrap();
        drop(conn);

        assert_eq!(
            database_header_user_version(&db_path),
            Some(u32::try_from(CURRENT_SCHEMA_VERSION).unwrap())
        );
    }

    #[test]
    fn test_open_with_timeout_does_not_require_write_lock_when_schema_current() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("lock_read_open.db");

        let _ = SqliteStorage::open(&db_path).unwrap();

        let lock_conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        lock_conn.execute("BEGIN IMMEDIATE").unwrap();

        let opened = SqliteStorage::open_with_timeout(&db_path, Some(50));
        assert!(
            opened.is_ok(),
            "opening an existing DB should succeed for read paths under a concurrent write lock"
        );

        lock_conn.execute("COMMIT").unwrap();
    }

    #[test]
    fn test_open_uses_default_busy_timeout() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("lock_read_open_default.db");

        let _ = SqliteStorage::open(&db_path).unwrap();

        let lock_conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        lock_conn.execute("BEGIN IMMEDIATE").unwrap();

        let opened = SqliteStorage::open(&db_path);
        assert!(
            opened.is_ok(),
            "default open() should use the standard busy timeout under a concurrent write lock"
        );

        lock_conn.execute("COMMIT").unwrap();
    }

    #[test]
    fn test_open_repairs_runtime_compatible_legacy_db_indexes() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("legacy_runtime_compatible.db");

        {
            let storage = SqliteStorage::open(&db_path).unwrap();
            storage
                .conn
                .execute("DROP INDEX IF EXISTS idx_issues_external_ref_unique")
                .unwrap();
            storage.conn.execute("PRAGMA user_version = 0").unwrap();
        }

        let reopened = SqliteStorage::open(&db_path).unwrap();
        let user_version = reopened
            .conn
            .query_row("PRAGMA user_version")
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap();
        assert_eq!(
            user_version,
            i64::from(CURRENT_SCHEMA_VERSION),
            "runtime-compatible legacy DBs should be repaired and marked current on open"
        );

        let indexes: HashSet<String> = reopened
            .conn
            .query("SELECT name FROM sqlite_master WHERE type='index'")
            .unwrap()
            .iter()
            .filter_map(|row| row.get(0).and_then(SqliteValue::as_text).map(str::to_owned))
            .collect();
        assert!(
            indexes.contains("idx_issues_external_ref_unique"),
            "runtime-compatible repair path should restore missing canonical indexes"
        );
    }

    #[test]
    fn test_open_repairs_missing_canonical_indexes_even_when_user_version_is_current() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("current_version_missing_index.db");

        {
            let storage = SqliteStorage::open(&db_path).unwrap();
            storage
                .conn
                .execute("DROP INDEX IF EXISTS idx_issues_external_ref_unique")
                .unwrap();
            // Reset user_version so the reopen takes the full schema path
            // (the fast path only applies runtime pragmas and does not
            // recreate missing indexes).
            storage.conn.execute("PRAGMA user_version = 0").unwrap();
        }

        let reopened = SqliteStorage::open(&db_path).unwrap();
        let user_version = reopened
            .conn
            .query_row("PRAGMA user_version")
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap();
        assert_eq!(
            user_version,
            i64::from(CURRENT_SCHEMA_VERSION),
            "reopen should restore the current schema version"
        );

        // Use PRAGMA index_list instead of sqlite_master (more reliable in fsqlite)
        let index_rows = reopened.conn.query("PRAGMA index_list('issues')").unwrap();
        let index_names: HashSet<String> = index_rows
            .iter()
            .filter_map(|row| row.get(1).and_then(SqliteValue::as_text).map(str::to_owned))
            .collect();
        assert!(
            index_names.contains("idx_issues_external_ref_unique"),
            "reopen should recreate missing canonical indexes, got: {index_names:?}"
        );
    }

    #[test]
    fn test_open_repairs_legacy_kv_primary_key_tables() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("legacy_kv_primary_keys.db");

        {
            let mut storage = SqliteStorage::open(&db_path).unwrap();
            storage.set_config("issue_prefix", "legacy").unwrap();
            storage.set_metadata("project", "legacy-project").unwrap();

            storage
                .conn
                .execute("DROP INDEX IF EXISTS idx_config_key")
                .unwrap();
            storage.conn.execute("DROP TABLE config").unwrap();
            storage
                .conn
                .execute("CREATE TABLE config (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
                .unwrap();
            storage
                .conn
                .execute("INSERT INTO config (key, value) VALUES ('issue_prefix', 'legacy')")
                .unwrap();

            storage
                .conn
                .execute("DROP INDEX IF EXISTS idx_metadata_key")
                .unwrap();
            storage.conn.execute("DROP TABLE metadata").unwrap();
            storage
                .conn
                .execute("CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
                .unwrap();
            storage
                .conn
                .execute("INSERT INTO metadata (key, value) VALUES ('project', 'legacy-project')")
                .unwrap();

            storage.conn.execute("PRAGMA user_version = 0").unwrap();
        }

        let reopened = SqliteStorage::open(&db_path).unwrap();
        assert_eq!(
            reopened.get_config("issue_prefix").unwrap(),
            Some("legacy".to_string())
        );
        assert_eq!(
            reopened.get_metadata("project").unwrap(),
            Some("legacy-project".to_string())
        );

        // Use PRAGMA table_info to verify the repair (sqlite_master can
        // return inconsistent results in fsqlite).
        // Check that the `key` column no longer has pk flag set.
        let config_has_pk = reopened
            .conn
            .query("PRAGMA table_info('config')")
            .unwrap()
            .iter()
            .any(|row| {
                let col_name = row.get(1).and_then(SqliteValue::as_text);
                let pk_flag = row.get(5).and_then(SqliteValue::as_integer).unwrap_or(0);
                col_name == Some("key") && pk_flag > 0
            });
        let metadata_has_pk = reopened
            .conn
            .query("PRAGMA table_info('metadata')")
            .unwrap()
            .iter()
            .any(|row| {
                let col_name = row.get(1).and_then(SqliteValue::as_text);
                let pk_flag = row.get(5).and_then(SqliteValue::as_integer).unwrap_or(0);
                col_name == Some("key") && pk_flag > 0
            });

        assert!(
            !config_has_pk,
            "legacy config primary key should be rebuilt to the canonical shape"
        );
        assert!(
            !metadata_has_pk,
            "legacy metadata primary key should be rebuilt to the canonical shape"
        );
    }

    #[test]
    fn test_upsert_issue_for_import_coalesces_optional_text_fields_to_empty_strings() {
        let storage = SqliteStorage::open_memory().unwrap();
        let issue = Issue {
            id: "bd-import-null-optional-text".to_string(),
            title: "Import null optional text".to_string(),
            ..Issue::default()
        };

        storage.upsert_issue_for_import(&issue).unwrap();

        let row = storage
            .conn
            .query_row_with_params(
                "SELECT
                    typeof(description), typeof(design), typeof(acceptance_criteria), typeof(notes),
                    typeof(owner), typeof(created_by), typeof(close_reason), typeof(closed_by_session),
                    typeof(source_system), typeof(source_repo), typeof(deleted_by), typeof(delete_reason),
                    typeof(original_type), typeof(sender),
                    description, design, acceptance_criteria, notes, owner, created_by, close_reason,
                    closed_by_session, source_system, source_repo, deleted_by, delete_reason,
                    original_type, sender
                 FROM issues WHERE id = ?",
                &[SqliteValue::from(issue.id.as_str())],
            )
            .unwrap();

        for index in 0..14 {
            assert_eq!(
                row.get(index).and_then(SqliteValue::as_text),
                Some("text"),
                "column {index} should store an empty string, not NULL"
            );
        }

        for index in 14..23 {
            assert_eq!(
                row.get(index).and_then(SqliteValue::as_text),
                Some(""),
                "column {index} should coalesce missing optional text to ''"
            );
        }

        assert_eq!(
            row.get(23).and_then(SqliteValue::as_text),
            Some("."),
            "source_repo should coalesce missing values to '.'"
        );

        for index in 24..28 {
            assert_eq!(
                row.get(index).and_then(SqliteValue::as_text),
                Some(""),
                "column {index} should coalesce missing optional text to ''"
            );
        }
    }

    #[test]
    fn test_pragmas_are_set_correctly() {
        let storage = SqliteStorage::open_memory().unwrap();

        // Check foreign keys are enabled
        #[allow(clippy::cast_possible_truncation)]
        let fk = storage
            .conn
            .query_row("PRAGMA foreign_keys")
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap_or(0) as i32;
        assert_eq!(fk, 1, "Foreign keys should be enabled");

        // Check journal mode (memory DBs use 'memory' mode)
        let mode = storage
            .conn
            .query_row("PRAGMA journal_mode")
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_text)
            .unwrap_or("")
            .to_string();
        assert!(
            mode.to_lowercase() == "wal" || mode.to_lowercase() == "memory",
            "Journal mode should be WAL or memory"
        );
    }

    #[test]
    fn test_create_duplicate_id_fails() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap();

        let issue = make_issue("bd-dup-1", "First issue", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue, "tester").unwrap();

        // Try to create another issue with the same ID
        let dup = make_issue("bd-dup-1", "Duplicate", Status::Open, 2, None, t1, None);
        let result = storage.create_issue(&dup, "tester");

        assert!(result.is_err(), "Creating duplicate ID should fail");
    }

    #[test]
    fn test_set_export_hashes_deduplicates_duplicate_issue_ids_last_value_wins() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap();
        let issue = make_issue("bd-hash-1", "Hash target", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue, "tester").unwrap();

        let inserted = storage
            .set_export_hashes(&[
                ("bd-hash-1".to_string(), "hash-old".to_string()),
                ("bd-hash-1".to_string(), "hash-new".to_string()),
            ])
            .unwrap();

        assert_eq!(
            inserted, 1,
            "duplicate issue IDs should collapse to one row"
        );

        let (content_hash, _) = storage.get_export_hash("bd-hash-1").unwrap().unwrap();
        assert_eq!(content_hash, "hash-new");
    }

    #[test]
    fn test_set_export_hashes_updates_large_existing_batch() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let created_at = Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap();

        let initial_hashes: Vec<(String, String)> = (0..40)
            .map(|idx| {
                let issue_id = format!("bd-hash-{idx:02}");
                let issue = make_issue(
                    &issue_id,
                    &format!("Hash target {idx}"),
                    Status::Open,
                    2,
                    None,
                    created_at,
                    None,
                );
                storage.create_issue(&issue, "tester").unwrap();
                (issue_id, format!("hash-a-{idx:02}"))
            })
            .collect();

        storage.set_export_hashes(&initial_hashes).unwrap();

        let updated_hashes: Vec<(String, String)> = initial_hashes
            .iter()
            .map(|(issue_id, _)| (issue_id.clone(), format!("hash-b-{issue_id}")))
            .collect();

        let updated = storage.set_export_hashes(&updated_hashes).unwrap();
        assert_eq!(updated, updated_hashes.len());

        let (content_hash, _) = storage
            .get_export_hash("bd-hash-39")
            .unwrap()
            .expect("updated export hash");
        assert_eq!(content_hash, "hash-b-bd-hash-39");
    }

    #[test]
    fn test_set_export_hashes_rewrites_large_mixed_prefix_batch_on_file_db() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let created_at = Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap();

        let issue_ids: Vec<String> = (0..160)
            .map(|idx| {
                let prefix = if idx % 2 == 0 { "bd" } else { "br" };
                format!("{prefix}-hash-{idx:03}")
            })
            .collect();

        let initial_hashes: Vec<(String, String)> = issue_ids
            .iter()
            .map(|issue_id| {
                let issue = make_issue(
                    issue_id,
                    &format!("Hash target {issue_id}"),
                    Status::Open,
                    2,
                    None,
                    created_at,
                    None,
                );
                storage.create_issue(&issue, "tester").unwrap();
                (issue_id.clone(), format!("hash-a-{issue_id}"))
            })
            .collect();

        storage.set_export_hashes(&initial_hashes).unwrap();

        let mut rewritten_hashes: Vec<(String, String)> = issue_ids
            .iter()
            .rev()
            .map(|issue_id| (issue_id.clone(), format!("hash-b-{issue_id}")))
            .collect();
        rewritten_hashes.push(("bd-hash-000".to_string(), "hash-c-bd-hash-000".to_string()));
        rewritten_hashes.push(("br-hash-001".to_string(), "hash-c-br-hash-001".to_string()));

        let updated = storage.set_export_hashes(&rewritten_hashes).unwrap();
        assert_eq!(updated, issue_ids.len());

        let (first_hash, _) = storage
            .get_export_hash("bd-hash-000")
            .unwrap()
            .expect("updated export hash for bd-hash-000");
        assert_eq!(first_hash, "hash-c-bd-hash-000");

        let (second_hash, _) = storage
            .get_export_hash("br-hash-001")
            .unwrap()
            .expect("updated export hash for br-hash-001");
        assert_eq!(second_hash, "hash-c-br-hash-001");

        let row_count = storage
            .execute_raw_query("SELECT COUNT(*) FROM export_hashes")
            .unwrap()
            .first()
            .and_then(|row| row.first())
            .and_then(SqliteValue::as_integer)
            .unwrap_or(-1);
        assert_eq!(row_count, i64::try_from(issue_ids.len()).unwrap_or(-1));
    }

    #[test]
    fn test_rebuild_blocked_cache_rewrites_large_existing_batch_on_file_db() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let created_at = Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap();

        let issue_pairs: Vec<(String, String)> = (0..160)
            .map(|idx| {
                let prefix = if idx % 2 == 0 { "bd" } else { "br" };
                (
                    format!("{prefix}-blocked-{idx:03}"),
                    format!("{prefix}-blocker-{idx:03}"),
                )
            })
            .collect();

        for (blocked_id, blocker_id) in &issue_pairs {
            let blocked = make_issue(
                blocked_id,
                &format!("Blocked target {blocked_id}"),
                Status::Open,
                2,
                None,
                created_at,
                None,
            );
            let blocker = make_issue(
                blocker_id,
                &format!("Blocking source {blocker_id}"),
                Status::Open,
                2,
                None,
                created_at,
                None,
            );
            storage.create_issue(&blocked, "tester").unwrap();
            storage.create_issue(&blocker, "tester").unwrap();
            storage
                .add_dependency(
                    blocked_id,
                    blocker_id,
                    DependencyType::Blocks.as_str(),
                    "tester",
                )
                .unwrap();
        }

        let rebuilt = storage.rebuild_blocked_cache(true).unwrap();
        assert_eq!(rebuilt, issue_pairs.len());

        for _ in 0..4 {
            let rewritten = storage.rebuild_blocked_cache(true).unwrap();
            assert_eq!(rewritten, issue_pairs.len());
        }

        let row_count = storage
            .execute_raw_query("SELECT COUNT(*) FROM blocked_issues_cache")
            .unwrap()
            .first()
            .and_then(|row| row.first())
            .and_then(SqliteValue::as_integer)
            .unwrap_or(-1);
        assert_eq!(row_count, i64::try_from(issue_pairs.len()).unwrap_or(-1));
    }

    #[test]
    fn test_diag_data_visibility() {
        use fsqlite_types::value::SqliteValue;
        // Simplest possible reproduction
        let conn = fsqlite::Connection::open(":memory:".to_string()).unwrap();
        conn.execute("CREATE TABLE t (k TEXT, v TEXT)").unwrap();
        conn.execute_with_params(
            "INSERT INTO t VALUES (?, ?)",
            &[SqliteValue::from("a"), SqliteValue::from("b")],
        )
        .unwrap();

        // 1: count without WHERE
        let r1 = conn
            .query_with_params("SELECT count(*) FROM t", &[])
            .unwrap();
        eprintln!(
            "[DIAG] 1. count(*) no WHERE: {:?}",
            r1.first().map(fsqlite::Row::values)
        );

        // 2: count with literal WHERE
        let r2 = conn
            .query_with_params("SELECT count(*) FROM t WHERE k = 'a'", &[])
            .unwrap();
        eprintln!(
            "[DIAG] 2. count(*) literal WHERE: {:?}",
            r2.first().map(fsqlite::Row::values)
        );

        // 3: count with bind WHERE
        let explain3 = conn
            .prepare("SELECT count(*) FROM t WHERE k = ?")
            .map_or_else(|e| format!("PREPARE ERROR: {e}"), |s| s.explain());
        for line in explain3.lines() {
            eprintln!("[DIAG] 3.E| {line}");
        }
        if explain3.is_empty() {
            eprintln!("[DIAG] 3.E| (empty)");
        }
        let r3 = conn
            .query_with_params(
                "SELECT count(*) FROM t WHERE k = ?",
                &[SqliteValue::from("a")],
            )
            .unwrap();
        eprintln!(
            "[DIAG] 3. count(*) bind WHERE: {:?}",
            r3.first().map(fsqlite::Row::values)
        );

        // Also get EXPLAIN for the working non-aggregate version
        let explain4 = conn
            .prepare("SELECT k FROM t WHERE k = ?")
            .map_or_else(|e| format!("PREPARE ERROR: {e}"), |s| s.explain());
        for line in explain4.lines() {
            eprintln!("[DIAG] 4.E| {line}");
        }
        if explain4.is_empty() {
            eprintln!("[DIAG] 4.E| (empty)");
        }

        // 4: select with bind WHERE (no aggregate)
        let r4 = conn
            .query_with_params("SELECT k FROM t WHERE k = ?", &[SqliteValue::from("a")])
            .unwrap();
        eprintln!(
            "[DIAG] 4. select k bind WHERE: {:?}",
            r4.first().map(fsqlite::Row::values)
        );

        // 5: count(k) with bind WHERE
        let r5 = conn
            .query_with_params(
                "SELECT count(k) FROM t WHERE k = ?",
                &[SqliteValue::from("a")],
            )
            .unwrap();
        eprintln!(
            "[DIAG] 5. count(k) bind WHERE: {:?}",
            r5.first().map(fsqlite::Row::values)
        );

        // 6: count with bind WHERE but no match
        let r6 = conn
            .query_with_params(
                "SELECT count(*) FROM t WHERE k = ?",
                &[SqliteValue::from("nonexistent")],
            )
            .unwrap();
        eprintln!(
            "[DIAG] 6. count(*) bind WHERE no match: {:?}",
            r6.first().map(fsqlite::Row::values)
        );

        let c = r3
            .first()
            .and_then(|r| r.values().first())
            .and_then(SqliteValue::as_integer)
            .unwrap_or(-99);
        assert_eq!(c, 1, "count(*) with bind param WHERE should return 1");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn test_diag_root_page_visibility() {
        use fsqlite_types::value::SqliteValue;
        // Create full beads schema and check which root pages are accessible
        let conn = fsqlite::Connection::open(":memory:".to_string()).unwrap();

        // Apply schema step by step, checking after each table
        let tables = vec![(
            "issues",
            r"CREATE TABLE IF NOT EXISTS issues (
                id TEXT PRIMARY KEY,
                content_hash TEXT,
                title TEXT NOT NULL CHECK(length(title) <= 500),
                description TEXT NOT NULL DEFAULT '',
                design TEXT NOT NULL DEFAULT '',
                acceptance_criteria TEXT NOT NULL DEFAULT '',
                notes TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL DEFAULT 'open',
                priority INTEGER NOT NULL DEFAULT 2 CHECK(priority >= 0 AND priority <= 4),
                issue_type TEXT NOT NULL DEFAULT 'task',
                assignee TEXT,
                owner TEXT DEFAULT '',
                estimated_minutes INTEGER,
                created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                created_by TEXT DEFAULT '',
                updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                closed_at DATETIME,
                close_reason TEXT DEFAULT '',
                closed_by_session TEXT DEFAULT '',
                due_at DATETIME,
                defer_until DATETIME,
                external_ref TEXT,
                source_system TEXT DEFAULT '',
                source_repo TEXT NOT NULL DEFAULT '.',
                deleted_at DATETIME,
                deleted_by TEXT DEFAULT '',
                delete_reason TEXT DEFAULT '',
                original_type TEXT DEFAULT '',
                compaction_level INTEGER DEFAULT 0,
                compacted_at DATETIME,
                compacted_at_commit TEXT,
                original_size INTEGER,
                sender TEXT DEFAULT '',
                ephemeral INTEGER DEFAULT 0,
                pinned INTEGER DEFAULT 0,
                is_template INTEGER DEFAULT 0,
                CHECK (
                    (status = 'closed' AND closed_at IS NOT NULL) OR
                    (status = 'tombstone') OR
                    (status NOT IN ('closed', 'tombstone') AND closed_at IS NULL)
                )
            )",
        )];
        for (name, sql) in &tables {
            match conn.execute(sql) {
                Ok(_) => eprintln!("[ROOT-DIAG] Created table {name} OK"),
                Err(e) => eprintln!("[ROOT-DIAG] Failed to create table {name}: {e}"),
            }
        }

        // Create first few indexes
        let indexes = vec![
            "CREATE INDEX IF NOT EXISTS idx_issues_status ON issues(status)",
            "CREATE INDEX IF NOT EXISTS idx_issues_priority ON issues(priority)",
            "CREATE INDEX IF NOT EXISTS idx_issues_issue_type ON issues(issue_type)",
            "CREATE INDEX IF NOT EXISTS idx_issues_assignee ON issues(assignee) WHERE assignee IS NOT NULL",
            "CREATE INDEX IF NOT EXISTS idx_issues_created_at ON issues(created_at)",
            "CREATE INDEX IF NOT EXISTS idx_issues_updated_at ON issues(updated_at)",
            "CREATE INDEX IF NOT EXISTS idx_issues_content_hash ON issues(content_hash)",
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_issues_external_ref_unique ON issues(external_ref) WHERE external_ref IS NOT NULL",
            "CREATE INDEX IF NOT EXISTS idx_issues_ephemeral ON issues(ephemeral) WHERE ephemeral = 1",
            "CREATE INDEX IF NOT EXISTS idx_issues_pinned ON issues(pinned) WHERE pinned = 1",
            "CREATE INDEX IF NOT EXISTS idx_issues_tombstone ON issues(status) WHERE status = 'tombstone'",
            "CREATE INDEX IF NOT EXISTS idx_issues_due_at ON issues(due_at) WHERE due_at IS NOT NULL",
            "CREATE INDEX IF NOT EXISTS idx_issues_defer_until ON issues(defer_until) WHERE defer_until IS NOT NULL",
            "CREATE INDEX IF NOT EXISTS idx_issues_ready ON issues(status, priority, created_at) WHERE status = 'open' AND ephemeral = 0 AND pinned = 0 AND (is_template = 0 OR is_template IS NULL)",
        ];
        for (i, sql) in indexes.iter().enumerate() {
            match conn.execute(sql) {
                Ok(_) => eprintln!("[ROOT-DIAG] Created index {} OK", i + 1),
                Err(e) => eprintln!("[ROOT-DIAG] Failed to create index {}: {e}", i + 1),
            }
        }

        // Try count(*) first (simplest possible query)
        match conn.query_with_params("SELECT count(*) FROM sqlite_master", &[]) {
            Ok(rows) => {
                let count = rows
                    .first()
                    .and_then(|r| r.values().first())
                    .and_then(SqliteValue::as_integer)
                    .unwrap_or(-99);
                eprintln!("[ROOT-DIAG] count(*) from sqlite_master: {count}");
            }
            Err(e) => eprintln!("[ROOT-DIAG] count(*) FAILED: {e}"),
        }

        // Try SELECT without ORDER BY
        match conn.query_with_params("SELECT type, name, rootpage FROM sqlite_master", &[]) {
            Ok(rows) => {
                eprintln!("[ROOT-DIAG] sqlite_master entries (no ORDER BY):");
                for row in &rows {
                    let vals = row.values();
                    let typ = vals.first().map(|v| format!("{v:?}")).unwrap_or_default();
                    let name = vals.get(1).map(|v| format!("{v:?}")).unwrap_or_default();
                    let rootpage = vals.get(2).and_then(SqliteValue::as_integer).unwrap_or(0);
                    eprintln!("[ROOT-DIAG]   type={typ} name={name} rootpage={rootpage}");
                }
            }
            Err(e) => eprintln!("[ROOT-DIAG] SELECT (no ORDER BY) FAILED: {e}"),
        }

        // Try SELECT with ORDER BY
        match conn.query_with_params(
            "SELECT type, name, rootpage FROM sqlite_master ORDER BY rootpage",
            &[],
        ) {
            Ok(rows) => {
                eprintln!("[ROOT-DIAG] sqlite_master entries (ORDER BY):");
                for row in &rows {
                    let vals = row.values();
                    let rootpage = vals.get(2).and_then(SqliteValue::as_integer).unwrap_or(0);
                    eprintln!("[ROOT-DIAG]   rootpage={rootpage}");
                }
            }
            Err(e) => eprintln!("[ROOT-DIAG] SELECT (ORDER BY) FAILED: {e}"),
        }

        // Try simple SELECT from issues table
        match conn.query_with_params("SELECT count(*) FROM issues", &[]) {
            Ok(rows) => {
                let count = rows
                    .first()
                    .and_then(|r| r.values().first())
                    .and_then(SqliteValue::as_integer)
                    .unwrap_or(-99);
                eprintln!("[ROOT-DIAG] count(*) from issues: {count}");
            }
            Err(e) => eprintln!("[ROOT-DIAG] count(*) from issues FAILED: {e}"),
        }

        let max_rootpage = 0i64;

        // Also try: incrementally create indexes and check count(*) after each
        eprintln!("[ROOT-DIAG] --- Incremental index creation with count check ---");
        let conn2 = fsqlite::Connection::open(":memory:".to_string()).unwrap();
        conn2
            .execute("CREATE TABLE t (a TEXT, b TEXT, c TEXT, d TEXT, e TEXT)")
            .unwrap();
        for i in 1..=20 {
            let col = ['a', 'b', 'c', 'd', 'e'][i % 5];
            let sql = format!("CREATE INDEX IF NOT EXISTS idx_{i} ON t({col})");
            match conn2.execute(&sql) {
                Ok(_) => {}
                Err(e) => {
                    eprintln!("[ROOT-DIAG] Index {i} creation FAILED: {e}");
                    break;
                }
            }
            match conn2.query_with_params("SELECT count(*) FROM sqlite_master", &[]) {
                Ok(rows) => {
                    let count = rows
                        .first()
                        .and_then(|r| r.values().first())
                        .and_then(SqliteValue::as_integer)
                        .unwrap_or(-99);
                    eprintln!("[ROOT-DIAG] After {i} indexes: count(*)={count}");
                }
                Err(e) => {
                    eprintln!("[ROOT-DIAG] After {i} indexes: count(*) FAILED: {e}");
                    break;
                }
            }
        }

        // Test multi-insert with explicit transactions
        eprintln!("[ROOT-DIAG] --- Multi-insert test ---");
        let conn3 = fsqlite::Connection::open(":memory:".to_string()).unwrap();
        conn3
            .execute("CREATE TABLE ev (id INTEGER PRIMARY KEY AUTOINCREMENT, msg TEXT)")
            .unwrap();
        for i in 0..5 {
            conn3.execute("BEGIN IMMEDIATE").unwrap();
            conn3
                .execute_with_params(
                    "INSERT INTO ev (msg) VALUES (?)",
                    &[SqliteValue::from(format!("msg{i}"))],
                )
                .unwrap();
            conn3.execute("COMMIT").unwrap();
        }
        let rows3 = conn3
            .query_with_params("SELECT count(*) FROM ev", &[])
            .unwrap();
        let count3 = rows3
            .first()
            .and_then(|r| r.values().first())
            .and_then(SqliteValue::as_integer)
            .unwrap_or(-99);
        eprintln!("[ROOT-DIAG] Multi-insert count: {count3} (expected 5)");

        let all3 = conn3
            .query_with_params("SELECT id, msg FROM ev", &[])
            .unwrap();
        for row in &all3 {
            let id = row
                .values()
                .first()
                .and_then(SqliteValue::as_integer)
                .unwrap_or(-1);
            let msg = row
                .values()
                .get(1)
                .map(|v| format!("{v:?}"))
                .unwrap_or_default();
            eprintln!("[ROOT-DIAG]   id={id} msg={msg}");
        }

        // Also test without explicit transactions (autocommit)
        let conn4 = fsqlite::Connection::open(":memory:".to_string()).unwrap();
        conn4
            .execute("CREATE TABLE ev2 (id INTEGER PRIMARY KEY AUTOINCREMENT, msg TEXT)")
            .unwrap();
        for i in 0..5 {
            conn4
                .execute_with_params(
                    "INSERT INTO ev2 (msg) VALUES (?)",
                    &[SqliteValue::from(format!("msg{i}"))],
                )
                .unwrap();
        }
        let rows4 = conn4
            .query_with_params("SELECT count(*) FROM ev2", &[])
            .unwrap();
        let count4 = rows4
            .first()
            .and_then(|r| r.values().first())
            .and_then(SqliteValue::as_integer)
            .unwrap_or(-99);
        eprintln!("[ROOT-DIAG] Multi-insert (autocommit) count: {count4} (expected 5)");

        let all4 = conn4
            .query_with_params("SELECT id, msg FROM ev2", &[])
            .unwrap();
        for row in &all4 {
            let id = row
                .values()
                .first()
                .and_then(SqliteValue::as_integer)
                .unwrap_or(-1);
            let msg = row
                .values()
                .get(1)
                .map(|v| format!("{v:?}"))
                .unwrap_or_default();
            eprintln!("[ROOT-DIAG]   id={id} msg={msg}");
        }

        // Test events-like table with indexes and WHERE+ORDER BY
        eprintln!("[ROOT-DIAG] --- Events-like test ---");
        let conn5 = fsqlite::Connection::open(":memory:".to_string()).unwrap();
        conn5
            .execute("CREATE TABLE issues2 (id TEXT PRIMARY KEY, title TEXT)")
            .unwrap();
        conn5.execute("CREATE TABLE ev3 (id INTEGER PRIMARY KEY AUTOINCREMENT, issue_id TEXT NOT NULL, msg TEXT, created_at TEXT, FOREIGN KEY (issue_id) REFERENCES issues2(id))").unwrap();
        conn5
            .execute("CREATE INDEX idx_ev3_issue ON ev3(issue_id)")
            .unwrap();
        conn5
            .execute("CREATE INDEX idx_ev3_created ON ev3(created_at)")
            .unwrap();
        conn5
            .execute("INSERT INTO issues2 (id, title) VALUES ('test-001', 'Test')")
            .unwrap();

        for i in 0..5 {
            conn5.execute("BEGIN IMMEDIATE").unwrap();
            conn5
                .execute_with_params(
                    "INSERT INTO ev3 (issue_id, msg, created_at) VALUES (?1, ?2, ?3)",
                    &[
                        SqliteValue::from("test-001"),
                        SqliteValue::from(format!("msg{i}")),
                        SqliteValue::from(format!("2024-01-0{} 00:00:00", i + 1)),
                    ],
                )
                .unwrap();
            conn5.execute("COMMIT").unwrap();
        }

        // Test count
        let ev_count = conn5
            .query_with_params("SELECT count(*) FROM ev3", &[])
            .unwrap();
        let c = ev_count
            .first()
            .and_then(|r| r.values().first())
            .and_then(SqliteValue::as_integer)
            .unwrap_or(-99);
        eprintln!("[ROOT-DIAG] ev3 count: {c}");

        // Test WHERE with bind (no order) - uses index_eq path
        let ev_where = conn5
            .query_with_params(
                "SELECT id, msg FROM ev3 WHERE issue_id = ?1",
                &[SqliteValue::from("test-001")],
            )
            .unwrap();
        eprintln!("[ROOT-DIAG] ev3 WHERE bind: {} rows", ev_where.len());

        // Test WHERE with literal (no bind) - uses full scan
        let ev_literal = conn5
            .query_with_params("SELECT id, msg FROM ev3 WHERE issue_id = 'test-001'", &[])
            .unwrap();
        eprintln!("[ROOT-DIAG] ev3 WHERE literal: {} rows", ev_literal.len());

        // Test full scan (no WHERE)
        let ev_all = conn5
            .query_with_params("SELECT id, msg FROM ev3", &[])
            .unwrap();
        eprintln!("[ROOT-DIAG] ev3 ALL (no where): {} rows", ev_all.len());

        // Test WHERE with ORDER BY
        let ev_ordered = conn5
            .query_with_params(
                "SELECT id, msg FROM ev3 WHERE issue_id = ?1 ORDER BY created_at DESC, id DESC",
                &[SqliteValue::from("test-001")],
            )
            .unwrap();
        eprintln!("[ROOT-DIAG] ev3 WHERE+ORDER: {} rows", ev_ordered.len());
        for row in &ev_ordered {
            let id = row
                .values()
                .first()
                .and_then(SqliteValue::as_integer)
                .unwrap_or(-1);
            let msg = row
                .values()
                .get(1)
                .map(|v| format!("{v:?}"))
                .unwrap_or_default();
            eprintln!("[ROOT-DIAG]   id={id} msg={msg}");
        }

        assert!(max_rootpage >= 0, "diagnostic test completed");
    }

    #[test]
    fn test_get_issue_not_found_returns_none() {
        let storage = SqliteStorage::open_memory().unwrap();

        let result = storage.get_issue("nonexistent-id").unwrap();

        assert!(
            result.is_none(),
            "Getting non-existent issue should return None"
        );
    }

    #[test]
    fn test_open_nonexistent_parent_fails() {
        let result = SqliteStorage::open(Path::new("/nonexistent/path/to/db.db"));

        assert!(
            result.is_err(),
            "Opening DB in non-existent directory should fail"
        );
    }

    #[test]
    fn test_list_issues_empty_db() {
        let storage = SqliteStorage::open_memory().unwrap();
        let filters = ListFilters::default();

        let issues = storage.list_issues(&filters).unwrap();

        assert!(issues.is_empty(), "Empty DB should return no issues");
    }

    #[test]
    fn test_update_issue_not_found_fails() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        let update = IssueUpdate {
            title: Some("Updated title".to_string()),
            ..IssueUpdate::default()
        };

        let result = storage.update_issue("nonexistent-id", &update, "tester");

        assert!(result.is_err(), "Updating non-existent issue should fail");
    }

    #[test]
    fn test_list_issues_filter_by_title() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 8, 1, 0, 0, 0).unwrap();

        // Create issues with different titles
        let issue1 = make_issue(
            "bd-s1",
            "Fix authentication bug",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        let issue2 = make_issue(
            "bd-s2",
            "Add user registration",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        let issue3 = make_issue(
            "bd-s3",
            "Update documentation",
            Status::Open,
            2,
            None,
            t1,
            None,
        );

        storage.create_issue(&issue1, "tester").unwrap();
        storage.create_issue(&issue2, "tester").unwrap();
        storage.create_issue(&issue3, "tester").unwrap();

        // Filter by title containing "bug"
        let filters = ListFilters {
            title_contains: Some("bug".to_string()),
            ..ListFilters::default()
        };

        let issues = storage.list_issues(&filters).unwrap();

        assert_eq!(
            issues.len(),
            1,
            "Should find one issue matching 'bug' in title"
        );
        assert_eq!(issues[0].id, "bd-s1");
    }

    #[test]
    fn test_list_issues_reverse_default_sort() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 8, 1, 0, 0, 0).unwrap();
        let t2 = Utc.with_ymd_and_hms(2025, 8, 2, 0, 0, 0).unwrap();

        let issue_a = make_issue("bd-a", "A", Status::Open, 1, None, t1, None);
        let issue_b = make_issue("bd-b", "B", Status::Open, 1, None, t2, None);
        let issue_c = make_issue("bd-c", "C", Status::Open, 2, None, t1, None);

        storage.create_issue(&issue_a, "tester").unwrap();
        storage.create_issue(&issue_b, "tester").unwrap();
        storage.create_issue(&issue_c, "tester").unwrap();

        let filters = ListFilters {
            reverse: true,
            ..ListFilters::default()
        };

        let issues = storage.list_issues(&filters).unwrap();
        let ids: Vec<_> = issues.iter().map(|i| i.id.as_str()).collect();

        assert_eq!(ids, vec!["bd-c", "bd-a", "bd-b"]);
    }

    #[test]
    fn test_search_issues_full_text() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 9, 1, 0, 0, 0).unwrap();

        let issue1 = make_issue(
            "bd-s1",
            "Fix authentication bug",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        let issue2 = make_issue(
            "bd-s2",
            "Add user registration",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        let issue3 = make_issue(
            "bd-s3",
            "Update documentation",
            Status::Open,
            2,
            None,
            t1,
            None,
        );

        storage.create_issue(&issue1, "tester").unwrap();
        storage.create_issue(&issue2, "tester").unwrap();
        storage.create_issue(&issue3, "tester").unwrap();

        let filters = ListFilters::default();
        let results = storage.search_issues("authentication", &filters).unwrap();

        assert_eq!(
            results.len(),
            1,
            "Should find one issue matching 'authentication'"
        );
        assert_eq!(results[0].id, "bd-s1");
    }

    #[test]
    fn test_search_issues_respects_include_deferred_flag() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 9, 1, 0, 0, 0).unwrap();

        let open_issue = make_issue(
            "bd-s-open",
            "authentication flow update",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        let deferred_issue = make_issue(
            "bd-s-deferred",
            "authentication flow deferred follow-up",
            Status::Deferred,
            2,
            None,
            t1,
            None,
        );

        storage.create_issue(&open_issue, "tester").unwrap();
        storage.create_issue(&deferred_issue, "tester").unwrap();

        let filters = ListFilters {
            include_deferred: false,
            ..ListFilters::default()
        };
        let results = storage.search_issues("authentication", &filters).unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "bd-s-open");
    }

    #[test]
    fn test_search_issues_orders_by_updated() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 9, 1, 0, 0, 0).unwrap();
        let t3 = Utc.with_ymd_and_hms(2025, 9, 3, 0, 0, 0).unwrap();

        let older_updated = make_issue(
            "bd-s-sort-a",
            "authentication alpha",
            Status::Open,
            2,
            None,
            t3,
            None,
        );
        let newer_updated = make_issue(
            "bd-s-sort-b",
            "authentication beta",
            Status::Open,
            2,
            None,
            t1,
            None,
        );

        storage.create_issue(&older_updated, "tester").unwrap();
        storage.create_issue(&newer_updated, "tester").unwrap();
        storage
            .execute_test_sql(&format!(
                "UPDATE issues SET updated_at = '{}' WHERE id = 'bd-s-sort-a';\n\
                 UPDATE issues SET updated_at = '{}' WHERE id = 'bd-s-sort-b';",
                t1.to_rfc3339(),
                t3.to_rfc3339()
            ))
            .unwrap();

        let results = storage
            .search_issues(
                "authentication",
                &ListFilters {
                    sort: Some("updated".to_string()),
                    ..ListFilters::default()
                },
            )
            .unwrap();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, "bd-s-sort-b");
    }

    #[test]
    fn test_search_issues_filter_by_updated_date() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let now = Utc::now();
        let old = now - chrono::Duration::days(10);
        let older = now - chrono::Duration::days(20);

        let issue1 = make_issue(
            "bd-search-old",
            "authentication old",
            Status::Open,
            2,
            None,
            old,
            None,
        );
        let issue2 = make_issue(
            "bd-search-older",
            "authentication older",
            Status::Open,
            2,
            None,
            older,
            None,
        );
        let issue3 = make_issue(
            "bd-search-new",
            "authentication new",
            Status::Open,
            2,
            None,
            now,
            None,
        );

        storage.create_issue(&issue1, "tester").unwrap();
        storage.create_issue(&issue2, "tester").unwrap();
        storage.create_issue(&issue3, "tester").unwrap();

        let mut filters = ListFilters {
            updated_before: Some(old),
            ..Default::default()
        };

        let issues = storage.search_issues("authentication", &filters).unwrap();
        let ids: Vec<_> = issues.iter().map(|issue| issue.id.as_str()).collect();
        assert_eq!(issues.len(), 2);
        assert!(ids.contains(&"bd-search-old"));
        assert!(ids.contains(&"bd-search-older"));
        assert!(!ids.contains(&"bd-search-new"));

        filters.updated_before = None;
        filters.updated_after = Some(old);

        let issues = storage.search_issues("authentication", &filters).unwrap();
        let ids: Vec<_> = issues.iter().map(|issue| issue.id.as_str()).collect();
        assert_eq!(issues.len(), 2);
        assert!(ids.contains(&"bd-search-old"));
        assert!(ids.contains(&"bd-search-new"));
        assert!(!ids.contains(&"bd-search-older"));
    }

    #[test]
    fn test_list_issues_filter_by_updated_date() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let now = Utc::now();
        let old = now - chrono::Duration::days(10);
        let older = now - chrono::Duration::days(20);

        let issue1 = make_issue("bd-old", "Old issue", Status::Open, 2, None, old, None);
        let issue2 = make_issue(
            "bd-older",
            "Older issue",
            Status::Open,
            2,
            None,
            older,
            None,
        );
        let issue3 = make_issue("bd-new", "New issue", Status::Open, 2, None, now, None);

        storage.create_issue(&issue1, "tester").unwrap();
        storage.create_issue(&issue2, "tester").unwrap();
        storage.create_issue(&issue3, "tester").unwrap();

        // Filter updated_before 'old' (inclusive? SQL uses <=)
        // If we use 'old', issue1 matches. issue2 matches. issue3 does not.
        let mut filters = ListFilters {
            updated_before: Some(old),
            ..Default::default()
        };

        let issues = storage.list_issues(&filters).unwrap();
        // Should contain bd-old and bd-older
        assert_eq!(issues.len(), 2);
        let ids: Vec<_> = issues.iter().map(|i| i.id.as_str()).collect();
        assert!(ids.contains(&"bd-old"));
        assert!(ids.contains(&"bd-older"));
        assert!(!ids.contains(&"bd-new"));

        // Filter updated_after 'old'
        filters.updated_before = None;
        filters.updated_after = Some(old);
        let issues = storage.list_issues(&filters).unwrap();
        // Should contain bd-old and bd-new
        assert_eq!(issues.len(), 2);
        let ids: Vec<_> = issues.iter().map(|i| i.id.as_str()).collect();
        assert!(ids.contains(&"bd-old"));
        assert!(ids.contains(&"bd-new"));
        assert!(!ids.contains(&"bd-older"));
    }

    #[test]
    fn test_list_issues_filter_by_labels() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc::now();

        let issue1 = make_issue("bd-l1", "Issue with label", Status::Open, 2, None, t1, None);
        let issue2 = make_issue(
            "bd-l2",
            "Issue without label",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&issue1, "tester").unwrap();
        storage.create_issue(&issue2, "tester").unwrap();

        // Add label to issue1
        storage.add_label("bd-l1", "test-label", "tester").unwrap();

        // Filter by label
        let filters = ListFilters {
            labels: Some(vec!["test-label".to_string()]),
            ..Default::default()
        };

        let issues = storage.list_issues(&filters).unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].id, "bd-l1");
    }

    #[test]
    fn test_list_issues_filter_by_multiple_labels_uses_and_logic() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc::now();

        let issue1 = make_issue("bd-l3", "Core only", Status::Open, 2, None, t1, None);
        let issue2 = make_issue(
            "bd-l4",
            "Core and frontend",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&issue1, "tester").unwrap();
        storage.create_issue(&issue2, "tester").unwrap();

        storage.add_label("bd-l3", "core", "tester").unwrap();
        storage.add_label("bd-l4", "core", "tester").unwrap();
        storage.add_label("bd-l4", "frontend", "tester").unwrap();

        let filters = ListFilters {
            labels: Some(vec!["core".to_string(), "frontend".to_string()]),
            ..Default::default()
        };

        let issues = storage.list_issues(&filters).unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].id, "bd-l4");
    }

    #[test]
    fn test_list_issues_combined_type_and_label_filters() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc::now();

        let task_issue = make_issue("bd-l5", "Core task", Status::Open, 1, None, t1, None);
        let mut feature_issue =
            make_issue("bd-l6", "Core feature", Status::Open, 1, None, t1, None);
        feature_issue.issue_type = IssueType::Feature;

        storage.create_issue(&task_issue, "tester").unwrap();
        storage.create_issue(&feature_issue, "tester").unwrap();

        storage.add_label("bd-l5", "core", "tester").unwrap();
        storage.add_label("bd-l6", "core", "tester").unwrap();

        let filters = ListFilters {
            types: Some(vec![IssueType::Task]),
            labels: Some(vec!["core".to_string()]),
            ..Default::default()
        };

        let issues = storage.list_issues(&filters).unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].id, "bd-l5");
    }

    #[test]
    fn test_blocked_cache_handles_quotes_in_ids() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc::now();

        let issue = make_issue("bd-x1", "Blocked", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue, "tester").unwrap();

        // Add a dependency on an ID containing a quote (e.g. from bad import)
        // This is valid in DB but tricky for manual JSON building.
        // Create a real issue with a tricky ID so it passes existence checks.
        let tricky_id = "bd-q\"ote";
        let tricky_issue = make_issue(tricky_id, "Tricky", Status::Open, 2, None, t1, None);
        storage.create_issue(&tricky_issue, "tester").unwrap();
        storage
            .add_dependency("bd-x1", tricky_id, "blocks", "tester")
            .unwrap();

        // Cache should be rebuilt and handle the quote correctly
        // (rebuild happens automatically on add_dependency via mutation context)

        // Verify we can read it back without error
        let blocked = storage.get_blocked_issues().unwrap();
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].0.id, "bd-x1");

        let blockers = &blocked[0].1;
        assert_eq!(blockers.len(), 1);
        // ID + ":open" (since the tricky issue is open)
        assert_eq!(blockers[0], "bd-q\"ote:open");
    }

    #[test]
    fn test_get_ready_issues_filters_by_labels() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc::now();

        let i1 = make_issue("bd-1", "A", Status::Open, 2, None, t1, None);
        let i2 = make_issue("bd-2", "B", Status::Open, 2, None, t1, None);
        let i3 = make_issue("bd-3", "C", Status::Open, 2, None, t1, None);

        storage.create_issue(&i1, "tester").unwrap();
        storage.create_issue(&i2, "tester").unwrap();
        storage.create_issue(&i3, "tester").unwrap();

        storage.add_label("bd-1", "backend", "tester").unwrap();
        storage.add_label("bd-1", "urgent", "tester").unwrap();
        storage.add_label("bd-2", "backend", "tester").unwrap();
        // bd-3 has no labels

        // Filter AND: backend + urgent
        let filters_and = ReadyFilters {
            labels_and: vec!["backend".to_string(), "urgent".to_string()],
            ..Default::default()
        };
        let res = storage
            .get_ready_issues(&filters_and, ReadySortPolicy::Oldest)
            .unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].id, "bd-1");

        // Filter OR: urgent
        let filters_or = ReadyFilters {
            labels_or: vec!["urgent".to_string()],
            ..Default::default()
        };
        let res = storage
            .get_ready_issues(&filters_or, ReadySortPolicy::Oldest)
            .unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].id, "bd-1");

        // Filter OR: backend (should get 1 and 2)
        let filters_or_backend = ReadyFilters {
            labels_or: vec!["backend".to_string()],
            ..Default::default()
        };
        let res = storage
            .get_ready_issues(&filters_or_backend, ReadySortPolicy::Oldest)
            .unwrap();
        assert_eq!(res.len(), 2);
    }

    #[test]
    fn test_get_ready_issues_filters_by_parent() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc::now();

        // Create parent epic
        let parent = make_issue("bd-epic", "Parent Epic", Status::Open, 1, None, t1, None);
        storage.create_issue(&parent, "tester").unwrap();

        // Create direct children of the epic
        let child1 = make_issue("bd-epic.1", "Child 1", Status::Open, 2, None, t1, None);
        let child2 = make_issue("bd-epic.2", "Child 2", Status::Open, 2, None, t1, None);
        storage.create_issue(&child1, "tester").unwrap();
        storage.create_issue(&child2, "tester").unwrap();

        // Create unrelated issue (not a child of the epic)
        let unrelated = make_issue("bd-other", "Unrelated", Status::Open, 2, None, t1, None);
        storage.create_issue(&unrelated, "tester").unwrap();

        // Add parent-child dependencies (no grandchild yet)
        storage
            .add_dependency("bd-epic.1", "bd-epic", "parent-child", "tester")
            .unwrap();
        storage
            .add_dependency("bd-epic.2", "bd-epic", "parent-child", "tester")
            .unwrap();

        // Test: --parent bd-epic (non-recursive) should return only direct children
        let filters_direct = ReadyFilters {
            parent: Some("bd-epic".to_string()),
            recursive: false,
            ..Default::default()
        };
        let res = storage
            .get_ready_issues(&filters_direct, ReadySortPolicy::Oldest)
            .unwrap();
        assert_eq!(
            res.len(),
            2,
            "Non-recursive should return only direct children"
        );
        let ids: Vec<&str> = res.iter().map(|i| i.id.as_str()).collect();
        assert!(ids.contains(&"bd-epic.1"), "Should contain child1");
        assert!(ids.contains(&"bd-epic.2"), "Should contain child2");

        // Now create grandchild and its dependency for recursive test
        let grandchild = make_issue("bd-epic.1.1", "Grandchild", Status::Open, 2, None, t1, None);
        storage.create_issue(&grandchild, "tester").unwrap();
        storage
            .add_dependency("bd-epic.1.1", "bd-epic.1", "parent-child", "tester")
            .unwrap();

        // Test: --parent bd-epic --recursive should return all non-blocked descendants
        // Note: bd-epic.1 is now blocked by its open child bd-epic.1.1 (blocked-cache
        // semantics), so only bd-epic.2 and bd-epic.1.1 are "ready".
        let filters_recursive = ReadyFilters {
            parent: Some("bd-epic".to_string()),
            recursive: true,
            ..Default::default()
        };
        let res = storage
            .get_ready_issues(&filters_recursive, ReadySortPolicy::Oldest)
            .unwrap();
        assert_eq!(
            res.len(),
            2,
            "Recursive should return non-blocked descendants"
        );
        let ids: Vec<&str> = res.iter().map(|i| i.id.as_str()).collect();
        assert!(ids.contains(&"bd-epic.2"), "Should contain child2");
        assert!(ids.contains(&"bd-epic.1.1"), "Should contain grandchild");
        assert!(
            !ids.contains(&"bd-epic"),
            "Should NOT contain the parent itself"
        );
        assert!(
            !ids.contains(&"bd-other"),
            "Should NOT contain unrelated issue"
        );

        // Test: --parent with non-existent parent should return empty
        let filters_nonexistent = ReadyFilters {
            parent: Some("bd-nonexistent".to_string()),
            recursive: false,
            ..Default::default()
        };
        let res = storage
            .get_ready_issues(&filters_nonexistent, ReadySortPolicy::Oldest)
            .unwrap();
        assert_eq!(res.len(), 0, "Non-existent parent should return empty");
    }

    #[test]
    fn test_get_ready_issues_treats_null_legacy_flags_as_false() {
        let conn = Connection::open(":memory:").unwrap();
        crate::storage::schema::execute_batch(
            &conn,
            r"
            CREATE TABLE issues (
                id TEXT PRIMARY KEY,
                content_hash TEXT,
                title TEXT NOT NULL,
                description TEXT,
                design TEXT,
                acceptance_criteria TEXT,
                notes TEXT,
                status TEXT NOT NULL,
                priority INTEGER NOT NULL,
                issue_type TEXT NOT NULL,
                assignee TEXT,
                owner TEXT,
                estimated_minutes INTEGER,
                created_at DATETIME NOT NULL,
                created_by TEXT,
                updated_at DATETIME NOT NULL,
                closed_at DATETIME,
                close_reason TEXT,
                closed_by_session TEXT,
                due_at DATETIME,
                defer_until DATETIME,
                external_ref TEXT,
                source_system TEXT,
                source_repo TEXT,
                deleted_at DATETIME,
                deleted_by TEXT,
                delete_reason TEXT,
                original_type TEXT,
                compaction_level INTEGER,
                compacted_at DATETIME,
                compacted_at_commit TEXT,
                original_size INTEGER,
                sender TEXT,
                ephemeral INTEGER,
                pinned INTEGER,
                is_template INTEGER
            );
            CREATE TABLE blocked_issues_cache (
                issue_id TEXT PRIMARY KEY,
                blocked_by TEXT NOT NULL,
                blocked_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE metadata (
                key TEXT NOT NULL,
                value TEXT NOT NULL
            );
            ",
        )
        .unwrap();

        let storage = SqliteStorage {
            conn,
            mutation_count: 0,
        };
        let timestamp = Utc.with_ymd_and_hms(2026, 3, 11, 0, 0, 0).unwrap();
        let stamp = timestamp.to_rfc3339();

        storage
            .conn
            .execute_with_params(
                r"
                INSERT INTO issues (
                    id, title, status, priority, issue_type, created_at, updated_at,
                    ephemeral, pinned, is_template
                ) VALUES (?, ?, ?, ?, ?, ?, ?, NULL, NULL, NULL)
                ",
                &[
                    SqliteValue::from("bd-legacy-ready"),
                    SqliteValue::from("Legacy ready issue"),
                    SqliteValue::from("open"),
                    SqliteValue::from(2_i64),
                    SqliteValue::from("task"),
                    SqliteValue::from(stamp.as_str()),
                    SqliteValue::from(stamp.as_str()),
                ],
            )
            .unwrap();

        let ready = storage
            .get_ready_issues(&ReadyFilters::default(), ReadySortPolicy::Priority)
            .unwrap();

        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "bd-legacy-ready");
    }

    #[test]
    fn test_get_ready_issues_excludes_in_progress() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc::now();

        // Create an open issue (should appear in ready)
        let open_issue = make_issue("bd-open", "Open Issue", Status::Open, 2, None, t1, None);
        storage.create_issue(&open_issue, "tester").unwrap();

        // Create an in_progress issue (should NOT appear in ready)
        let ip_issue = make_issue(
            "bd-inprogress",
            "In Progress Issue",
            Status::InProgress,
            1,
            None,
            t1,
            None,
        );
        storage.create_issue(&ip_issue, "tester").unwrap();

        let ready = storage
            .get_ready_issues(&ReadyFilters::default(), ReadySortPolicy::Priority)
            .unwrap();

        // Only the open issue should be ready; in_progress is already claimed
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "bd-open");
    }

    #[test]
    fn test_next_child_number() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();

        // Create parent issue
        let parent = make_issue("bd-parent", "Parent Epic", Status::Open, 2, None, t1, None);
        storage.create_issue(&parent, "tester").unwrap();

        // No children yet - should return 1
        let next = storage.next_child_number("bd-parent").unwrap();
        assert_eq!(next, 1, "First child should be .1");

        // Create first child
        let child1 = make_issue("bd-parent.1", "Child 1", Status::Open, 2, None, t1, None);
        storage.create_issue(&child1, "tester").unwrap();

        // Should now return 2
        let next = storage.next_child_number("bd-parent").unwrap();
        assert_eq!(next, 2, "After .1 exists, next should be .2");

        // Create child with .3 (skip .2)
        let child3 = make_issue("bd-parent.3", "Child 3", Status::Open, 2, None, t1, None);
        storage.create_issue(&child3, "tester").unwrap();

        // Should return 4 (max is 3, so next is 4)
        let next = storage.next_child_number("bd-parent").unwrap();
        assert_eq!(next, 4, "After .3 exists (skipping .2), next should be .4");

        // Create grandchild - should not affect parent's next child number
        let grandchild = make_issue(
            "bd-parent.1.1",
            "Grandchild",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&grandchild, "tester").unwrap();

        // Parent's next child should still be 4
        let next = storage.next_child_number("bd-parent").unwrap();
        assert_eq!(
            next, 4,
            "Grandchild should not affect parent's next child number"
        );

        // Check grandchild's parent (bd-parent.1) next child number
        let next_for_child1 = storage.next_child_number("bd-parent.1").unwrap();
        assert_eq!(
            next_for_child1, 2,
            "After bd-parent.1.1 exists, next for bd-parent.1 should be .2"
        );
    }

    #[test]
    fn test_rebuild_child_counters_skips_missing_parents() {
        let storage = SqliteStorage::open_memory().unwrap();
        let timestamp = Utc.with_ymd_and_hms(2026, 3, 12, 0, 0, 0).unwrap();
        let stamp = timestamp.to_rfc3339();

        storage
            .conn
            .execute_with_params(
                r"
                INSERT INTO issues (
                    id, title, status, priority, issue_type, created_at, updated_at,
                    ephemeral, pinned, is_template
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                ",
                &[
                    SqliteValue::from("bd-orphan.6"),
                    SqliteValue::from("Recovered orphan child"),
                    SqliteValue::from("open"),
                    SqliteValue::from(2_i64),
                    SqliteValue::from("task"),
                    SqliteValue::from(stamp.as_str()),
                    SqliteValue::from(stamp.as_str()),
                    SqliteValue::from(0_i64),
                    SqliteValue::from(0_i64),
                    SqliteValue::from(0_i64),
                ],
            )
            .unwrap();

        let rebuilt = storage.rebuild_child_counters_in_tx().unwrap();
        assert_eq!(
            rebuilt, 0,
            "missing parents should not get counters rebuilt"
        );
        assert!(
            !storage
                .has_missing_issue_reference("child_counters", "parent_id")
                .unwrap(),
            "rebuild should not create orphan child counter rows"
        );
    }

    #[test]
    fn test_finish_issue_mutation_write_probe_returns_rollback_error_when_cleanup_fails() {
        let result = finish_issue_mutation_write_probe(
            Ok(1),
            Err(FrankenError::Internal("rollback failed".to_string())),
        );

        let err = result.expect_err("rollback failure should surface");
        assert!(
            err.to_string().contains("rollback failed"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_finish_issue_mutation_write_probe_prefers_write_error() {
        let result = finish_issue_mutation_write_probe(
            Err(FrankenError::Internal("write failed".to_string())),
            Err(FrankenError::Internal("rollback failed".to_string())),
        );

        let err = result.expect_err("write failure should surface");
        assert!(
            err.to_string().contains("write failed"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_parse_datetime_empty_string_returns_epoch() {
        let result = parse_datetime("").unwrap();
        assert_eq!(result, DateTime::<Utc>::UNIX_EPOCH);
    }

    #[test]
    fn test_parse_datetime_rfc3339_with_z() {
        let result = parse_datetime("2026-02-26T19:54:42.715824474Z").unwrap();
        assert_eq!(result.year(), 2026);
        assert_eq!(result.month(), 2);
    }

    #[test]
    fn test_parse_datetime_rfc3339_with_offset() {
        let result = parse_datetime("2026-02-26T19:54:42+00:00").unwrap();
        assert_eq!(result.year(), 2026);
    }

    #[test]
    fn test_parse_datetime_naive_format() {
        let result = parse_datetime("2026-02-26 19:54:42").unwrap();
        assert_eq!(result.year(), 2026);
        assert_eq!(result.month(), 2);
    }

    #[test]
    fn test_parse_datetime_garbage_returns_error() {
        let result = parse_datetime("not-a-date");
        assert!(result.is_err());
    }

    #[test]
    fn test_reset_data_tables_preserves_config() {
        // Use a real file to avoid fsqlite in-memory BUSY contention under parallel tests.
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();

        // Set config and create some issues
        storage.set_config("issue_prefix", "test").unwrap();
        storage.set_metadata("last_import", "2025-01-01").unwrap();
        let issue = make_issue("test-1", "Issue 1", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue, "tester").unwrap();

        // Verify issue exists
        assert!(storage.get_issue("test-1").unwrap().is_some());

        // Reset data tables
        storage.reset_data_tables().unwrap();

        // Config and metadata should be preserved
        assert_eq!(
            storage.get_config("issue_prefix").unwrap(),
            Some("test".to_string()),
        );
        assert_eq!(
            storage.get_metadata("last_import").unwrap(),
            Some("2025-01-01".to_string()),
        );

        // Issue data should be gone
        assert!(storage.get_issue("test-1").unwrap().is_none());

        // Should be able to insert new issues (schema intact)
        let issue2 = make_issue("test-2", "Issue 2", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue2, "tester").unwrap();
        assert!(storage.get_issue("test-2").unwrap().is_some());
    }
}
