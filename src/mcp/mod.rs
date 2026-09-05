//! MCP (Model Context Protocol) server for beads_rust.
//!
//! Exposes the issue tracker as an MCP server so that AI agents can
//! query, create, and manage issues through the standard MCP protocol
//! instead of shelling out to the `br` CLI.
//!
//! This module is feature-gated behind `mcp` and is **not** included
//! in the default feature set.

mod prompts;
mod resources;
mod tools;

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

use fastmcp_rust::{McpError, McpErrorCode, McpResult, StdioTransport};
use serde_json::{Value, json};

use crate::error::StructuredError;
use crate::model::Issue;
use crate::storage::sqlite::PendingSyncMergeInspection;
use crate::storage::{ReadyFilters, ReadySortPolicy, SqliteStorage};
use crate::{BeadsError, config};

const MCP_READ_SNAPSHOT_ENV: &str = "BR_MCP_READ_SNAPSHOT";
const MCP_READ_SNAPSHOT_CACHE_LIMIT: usize = 64;

/// Map any `Display` error into a flat `McpError::tool_error`.
///
/// Used by resources and prompts for non-structured error mapping.
/// Tools use the richer `beads_to_mcp` in `tools.rs` instead.
pub(super) fn to_mcp(err: impl std::fmt::Display) -> McpError {
    McpError::tool_error(err.to_string())
}

fn shutdown_mcp_error() -> McpError {
    let err = BeadsError::ShuttingDown;
    let structured = StructuredError::from_error(&err);
    let message = structured.message.clone();
    let mut data = json!({
        "error_type": structured.code.as_str(),
        "recoverable": structured.retryable,
        "message": message,
    });
    if let Some(object) = data.as_object_mut() {
        if let Some(hint) = &structured.hint {
            object.insert("hint".to_string(), json!(hint));
        }
        if let Some(context) = &structured.context {
            object.insert("context".to_string(), context.clone());
        }
    }

    McpError::with_data(McpErrorCode::ToolExecutionError, structured.message, data)
}

fn ensure_not_shutting_down_with(is_requested: impl FnOnce() -> bool) -> McpResult<()> {
    if is_requested() {
        Err(shutdown_mcp_error())
    } else {
        Ok(())
    }
}

pub(super) fn ensure_not_shutting_down() -> McpResult<()> {
    ensure_not_shutting_down_with(crate::shutdown::is_requested)
}

pub(super) fn mcp_ready_issues(
    state: &BeadsState,
    storage: &SqliteStorage,
) -> fastmcp_rust::McpResult<Vec<Issue>> {
    let workflow = storage.workflow_policy();
    workflow.validate_ready_status_group().map_err(to_mcp)?;
    let filters = ReadyFilters {
        ready_statuses: workflow.ready_status_group(),
        ..ReadyFilters::default()
    };
    let mut ready = storage
        .get_ready_issues(&filters, ReadySortPolicy::Hybrid)
        .map_err(to_mcp)?;
    if ready.is_empty() || !storage.has_external_dependencies(true).map_err(to_mcp)? {
        return Ok(ready);
    }

    let config_layer = config::load_config(
        &state.beads_dir,
        Some(storage),
        &config::CliOverrides::default(),
    )
    .map_err(to_mcp)?;
    let external_db_paths = config::external_project_db_paths(&config_layer, &state.beads_dir);
    let external_statuses = storage
        .resolve_external_dependency_statuses(&external_db_paths, true)
        .map_err(to_mcp)?;
    let external_blockers = storage
        .external_blockers(&external_statuses)
        .map_err(to_mcp)?;
    if !external_blockers.is_empty() {
        ready.retain(|issue| !external_blockers.contains_key(&issue.id));
    }
    Ok(ready)
}

fn auto_flush_mcp_error(
    beads_dir: &Path,
    jsonl_path: &Path,
    err: impl std::fmt::Display,
) -> McpError {
    let message = "Automatic JSONL export failed";
    McpError::with_data(
        McpErrorCode::ToolExecutionError,
        message,
        json!({
            "error_type": "AUTO_FLUSH_FAILED",
            "recoverable": true,
            "sync_pending": true,
            "retry_mutation": false,
            "message": message,
            "beads_dir": beads_dir.display().to_string(),
            "jsonl_path": jsonl_path.display().to_string(),
            "error": err.to_string(),
            "recovery": "Run br sync --flush-only after fixing the export problem before committing .beads/issues.jsonl",
        }),
    )
}

fn sync_lock_mcp_error(
    beads_dir: &Path,
    jsonl_path: &Path,
    err: impl std::fmt::Display,
) -> McpError {
    let message = "Mutation was not attempted because the JSONL sync lock is unavailable";
    McpError::with_data(
        McpErrorCode::ToolExecutionError,
        message,
        json!({
            "error_type": "SYNC_LOCK_UNAVAILABLE",
            "recoverable": true,
            "message": message,
            "beads_dir": beads_dir.display().to_string(),
            "jsonl_path": jsonl_path.display().to_string(),
            "error": err.to_string(),
            "recovery": "Retry after the active sync finishes or fix the .beads/.sync.lock path.",
        }),
    )
}

fn sync_lock_busy_error(beads_dir: &Path) -> BeadsError {
    BeadsError::Config(format!(
        "Automatic JSONL export skipped because sync lock at {} is held by another process",
        beads_dir.join(".sync.lock").display()
    ))
}

fn pending_sync_merge_mcp_error(inspection: &PendingSyncMergeInspection) -> McpError {
    let (condition, metadata_key) = match inspection {
        PendingSyncMergeInspection::Absent => ("absent", None),
        PendingSyncMergeInspection::Valid(_) => (
            "valid",
            Some(crate::sync::METADATA_SYNC_MERGE_PENDING.to_string()),
        ),
        PendingSyncMergeInspection::Legacy { metadata_key, .. } => {
            ("legacy", Some(metadata_key.clone()))
        }
        PendingSyncMergeInspection::Malformed { metadata_key, .. } => {
            ("malformed", Some(metadata_key.clone()))
        }
    };
    let message =
        "MCP mutation refused because a pending sync merge requires explicit reconciliation";
    McpError::with_data(
        McpErrorCode::ToolExecutionError,
        message,
        json!({
            "error_type": "SYNC_MERGE_PENDING",
            "recoverable": true,
            "message": message,
            "condition": condition,
            "metadata_key": metadata_key,
            "diagnostic": inspection.diagnostic(),
            "recovery": "Run `br sync --merge`, verify that it clears the pending receipt, then retry the MCP operation.",
        }),
    )
}

fn pending_sync_merge_unknown_mcp_error(err: impl std::fmt::Display) -> McpError {
    let message =
        "MCP mutation refused because pending sync-merge state could not be proven absent";
    McpError::with_data(
        McpErrorCode::ToolExecutionError,
        message,
        json!({
            "error_type": "SYNC_MERGE_PENDING_UNKNOWN",
            "recoverable": false,
            "message": message,
            "inspection_error": err.to_string(),
            "recovery": "Restore current-schema read-only access to the database family, run `br doctor`, and reconcile with `br sync --merge` before retrying.",
        }),
    )
}

fn pending_sync_merge_read_fallback_error(inspection: &PendingSyncMergeInspection) -> BeadsError {
    BeadsError::SyncConflict {
        message: format!(
            "MCP writable read fallback refused because {}. Run `br sync --merge` before retrying",
            inspection.diagnostic()
        ),
    }
}

fn pending_sync_merge_read_fallback_unknown(err: impl std::fmt::Display) -> BeadsError {
    BeadsError::SyncConflict {
        message: format!(
            "MCP writable read fallback refused because pending sync-merge state could not be proven absent: {err}. Restore current-schema read-only database access, run `br doctor`, and reconcile with `br sync --merge` before retrying"
        ),
    }
}

fn dirty_auto_flush_incomplete_error(remaining_dirty: usize, needs_flush: bool) -> BeadsError {
    BeadsError::Config(format!(
        "Automatic JSONL export remains pending: {remaining_dirty} dirty issue(s), forced flush: {needs_flush}"
    ))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct McpReadSnapshotWitness {
    files: Vec<McpReadSnapshotFile>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct McpReadSnapshotFile {
    path: PathBuf,
    metadata: Option<McpReadSnapshotFileMetadata>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct McpReadSnapshotFileMetadata {
    len: u64,
    modified_ns: Option<u128>,
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(unix)]
    ctime_sec: i64,
    #[cfg(unix)]
    ctime_nsec: i64,
}

#[derive(Debug, Default)]
pub(super) struct McpReadSnapshotCache {
    entries: Vec<McpReadSnapshotEntry>,
}

#[derive(Debug)]
struct McpReadSnapshotEntry {
    key: String,
    witness: McpReadSnapshotWitness,
    value: Value,
}

impl McpReadSnapshotCache {
    fn get(&self, key: &str, witness: &McpReadSnapshotWitness) -> Option<Value> {
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.key == key && entry.witness == *witness)
            .map(|entry| entry.value.clone())
    }

    fn insert(&mut self, key: String, witness: McpReadSnapshotWitness, value: Value) {
        if let Some(index) = self.entries.iter().position(|entry| entry.key == key) {
            self.entries.remove(index);
        }

        self.entries.push(McpReadSnapshotEntry {
            key,
            witness,
            value,
        });

        if self.entries.len() > MCP_READ_SNAPSHOT_CACHE_LIMIT {
            self.entries.remove(0);
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
    }
}

fn mcp_read_snapshot_cache_from_env() -> Option<Mutex<McpReadSnapshotCache>> {
    std::env::var(MCP_READ_SNAPSHOT_ENV)
        .ok()
        .filter(|value| env_value_is_truthy(value))
        .map(|_| Mutex::new(McpReadSnapshotCache::default()))
}

fn env_value_is_truthy(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn snapshot_sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut raw = path.as_os_str().to_os_string();
    raw.push(suffix);
    PathBuf::from(raw)
}

fn system_time_ns(time: std::time::SystemTime) -> Option<u128> {
    time.duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_nanos())
}

fn snapshot_file(path: &Path) -> Option<McpReadSnapshotFile> {
    match fs::metadata(path) {
        Ok(metadata) => Some(McpReadSnapshotFile {
            path: path.to_path_buf(),
            metadata: Some(McpReadSnapshotFileMetadata {
                len: metadata.len(),
                modified_ns: metadata.modified().ok().and_then(system_time_ns),
                #[cfg(unix)]
                dev: metadata.dev(),
                #[cfg(unix)]
                ino: metadata.ino(),
                #[cfg(unix)]
                ctime_sec: metadata.ctime(),
                #[cfg(unix)]
                ctime_nsec: metadata.ctime_nsec(),
            }),
        }),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Some(McpReadSnapshotFile {
            path: path.to_path_buf(),
            metadata: None,
        }),
        Err(err) => {
            tracing::debug!(
                error = %err,
                path = %path.display(),
                "MCP read snapshot witness capture failed"
            );
            None
        }
    }
}

/// Shared configuration available to every MCP handler.
///
/// Storage is intentionally **not** held open: `fsqlite::Connection` uses
/// `Rc` internally and therefore cannot satisfy `Send + Sync`.  Each
/// handler call opens a fresh connection via [`open_read_storage`] or
/// [`open_storage`] depending on whether the operation may mutate state.
pub struct BeadsState {
    pub db_path: PathBuf,
    pub beads_dir: PathBuf,
    pub jsonl_path: PathBuf,
    pub write_lock_timeout_ms: Option<u64>,
    pub allow_external_jsonl: bool,
    pub actor: String,
    pub issue_prefix: Option<String>,
    /// `.br_history` policy resolved once at server start from the merged
    /// config layer and the environment (GitHub #484); every MCP auto-flush
    /// exports with exactly this configuration.
    pub history: crate::sync::history::HistoryConfig,
    pub(super) read_snapshot_cache: Option<Mutex<McpReadSnapshotCache>>,
}

impl BeadsState {
    pub(super) fn cached_read_json(&self, key: &str) -> Option<Value> {
        let cache = self.read_snapshot_cache.as_ref()?;
        let before = self.capture_read_snapshot_witness()?;
        let value = {
            let guard = cache.lock().ok()?;
            guard.get(key, &before)
        };
        let after = self.capture_read_snapshot_witness()?;

        if before == after { value } else { None }
    }

    pub(super) fn capture_read_snapshot_witness(&self) -> Option<McpReadSnapshotWitness> {
        self.read_snapshot_cache.as_ref()?;

        let paths = [
            self.db_path.clone(),
            snapshot_sidecar_path(&self.db_path, "-wal"),
            snapshot_sidecar_path(&self.db_path, "-shm"),
            self.jsonl_path.clone(),
            self.beads_dir.join("policy.yaml"),
        ];

        paths
            .iter()
            .map(|path| snapshot_file(path))
            .collect::<Option<Vec<_>>>()
            .map(|files| McpReadSnapshotWitness { files })
    }

    pub(super) fn store_read_json_snapshot(
        &self,
        key: String,
        before: Option<McpReadSnapshotWitness>,
        value: &Value,
    ) {
        let Some(cache) = self.read_snapshot_cache.as_ref() else {
            return;
        };
        let Some(before) = before else {
            return;
        };
        let Some(after) = self.capture_read_snapshot_witness() else {
            self.clear_read_snapshot_cache();
            return;
        };

        if before != after {
            return;
        }

        if let Ok(mut guard) = cache.lock() {
            guard.insert(key, after, value.clone());
        }
    }

    pub(super) fn clear_read_snapshot_cache(&self) {
        if let Some(cache) = &self.read_snapshot_cache
            && let Ok(mut guard) = cache.lock()
        {
            guard.clear();
        }
    }

    /// Open a fresh writable `SqliteStorage` connection under an inode-bound
    /// database-family authority retained by the returned storage handle.
    ///
    /// # Errors
    ///
    /// Returns an error if the database file cannot be opened.
    fn open_storage_under_write_authority(
        &self,
        write_authority: &Arc<crate::sync::DatabaseFamilyWriteLock>,
    ) -> crate::Result<SqliteStorage> {
        if write_authority.bind_database_inode_for_mutation()? {
            write_authority.install_empty_database_replacement_and_bind()?;
        }
        write_authority.verify_database_authority()?;
        let mut storage = SqliteStorage::open(&self.db_path)?;
        write_authority.verify_database_authority()?;
        storage.attach_write_authority(Arc::clone(write_authority));
        Ok(storage)
    }

    fn open_storage_with_fresh_write_authority(&self) -> crate::Result<SqliteStorage> {
        let write_authority = Arc::new(
            crate::sync::blocking_database_family_write_lock_with_timeout(
                &self.beads_dir,
                &self.db_path,
                self.write_lock_timeout_ms,
            )?,
        );
        let _sync_lock = crate::sync::try_sync_lock(&self.beads_dir)?
            .ok_or_else(|| sync_lock_busy_error(&self.beads_dir))?;

        match SqliteStorage::inspect_pending_sync_merge_under_authority(
            &self.db_path,
            &write_authority,
        ) {
            Ok(PendingSyncMergeInspection::Absent) => {}
            Ok(inspection) => {
                return Err(pending_sync_merge_read_fallback_error(&inspection));
            }
            Err(err) => {
                return Err(pending_sync_merge_read_fallback_unknown(err));
            }
        }

        let storage = self.open_storage_under_write_authority(&write_authority)?;
        match storage.inspect_pending_sync_merge() {
            Ok(PendingSyncMergeInspection::Absent) => Ok(storage),
            Ok(inspection) => Err(pending_sync_merge_read_fallback_error(&inspection)),
            Err(err) => Err(pending_sync_merge_read_fallback_unknown(err)),
        }
    }

    /// Open a fresh read-oriented storage connection.
    ///
    /// Current-schema databases open read-only to avoid schema, recovery, or
    /// metadata writes for MCP resources, prompts, and read-only tools. If the
    /// read-only fast path is unavailable, fall back to normal storage open
    /// while holding the workspace write lock because that path may repair or
    /// initialize database state.
    ///
    /// # Errors
    ///
    /// Returns an error if storage cannot be opened.
    pub fn open_read_storage(&self) -> crate::Result<SqliteStorage> {
        let policy = crate::close_policy::load_for_beads_dir(&self.beads_dir)?;
        let mut storage = match SqliteStorage::open_current_read_only(&self.db_path) {
            Ok(Some(storage)) => Ok(storage),
            Ok(None) => self.open_storage_with_fresh_write_authority(),
            Err(err) => {
                tracing::debug!(
                    error = %err,
                    db_path = %self.db_path.display(),
                    "MCP read-only storage open failed; falling back to locked writable open"
                );
                self.open_storage_with_fresh_write_authority()
            }
        }?;
        storage.set_workflow_policy(policy.workflow);
        Ok(storage)
    }

    /// Execute a mutating closure against the storage, acquiring the cross-process
    /// write lock and triggering an auto-flush upon success.
    pub fn with_mutation<F, R>(&self, mut f: F) -> fastmcp_rust::McpResult<R>
    where
        R: serde::Serialize,
        F: FnMut(
            &mut SqliteStorage,
            &crate::close_policy::PolicyDocument,
        ) -> fastmcp_rust::McpResult<R>,
    {
        // 1. Acquire the cross-process write lock.
        let write_authority = Arc::new(
            crate::sync::blocking_database_family_write_lock_with_timeout(
                &self.beads_dir,
                &self.db_path,
                self.write_lock_timeout_ms,
            )
            .map_err(to_mcp)?,
        );

        // 2. Acquire the sync lock before committing a mutation. MCP writes
        // should not report success when JSONL export is known to be unguarded
        // or impossible.
        let _sync_lock = match crate::sync::try_sync_lock(&self.beads_dir) {
            Ok(Some(lock)) => lock,
            Ok(None) => {
                return Err(sync_lock_mcp_error(
                    &self.beads_dir,
                    &self.jsonl_path,
                    sync_lock_busy_error(&self.beads_dir),
                ));
            }
            Err(err) => {
                return Err(sync_lock_mcp_error(&self.beads_dir, &self.jsonl_path, err));
            }
        };

        // The server can remain alive across an independently committed merge
        // receipt. Decide under freshly acquired DB + sync authority before a
        // writable open, then recheck on the opened connection immediately
        // before invoking the caller's closure.
        match SqliteStorage::inspect_pending_sync_merge_under_authority(
            &self.db_path,
            &write_authority,
        ) {
            Ok(PendingSyncMergeInspection::Absent) => {}
            Ok(inspection) => return Err(pending_sync_merge_mcp_error(&inspection)),
            Err(err) => return Err(pending_sync_merge_unknown_mcp_error(err)),
        }

        self.clear_read_snapshot_cache();

        // Refresh policy once per request under write authority. The same
        // document drives storage admission and close-time policy evaluation.
        let policy = crate::close_policy::load_for_beads_dir(&self.beads_dir).map_err(to_mcp)?;
        // 3. Open storage.
        let mut storage = self
            .open_storage_under_write_authority(&write_authority)
            .map_err(to_mcp)?;
        storage.set_workflow_policy(policy.workflow.clone());
        match storage.inspect_pending_sync_merge() {
            Ok(PendingSyncMergeInspection::Absent) => {}
            Ok(inspection) => return Err(pending_sync_merge_mcp_error(&inspection)),
            Err(err) => return Err(pending_sync_merge_unknown_mcp_error(err)),
        }
        let dirty_before_mutation = storage.get_dirty_issue_metadata().map_err(to_mcp)?;
        let (_, _, previous_sync_pending) =
            crate::sync::pending_export_state(&storage, self.jsonl_path.exists())
                .map_err(to_mcp)?;

        let result = f(&mut storage, &policy);
        self.finish_mutation(
            &mut storage,
            &dirty_before_mutation,
            previous_sync_pending,
            result,
        )
    }

    fn finish_mutation<R: serde::Serialize>(
        &self,
        storage: &mut SqliteStorage,
        before: &[(String, String)],
        previous_sync_pending: bool,
        result: McpResult<R>,
    ) -> McpResult<R> {
        let changed = match storage.get_dirty_issue_metadata() {
            Ok(after) => after != before,
            Err(witness_error) => {
                let mut error = match result {
                    Ok(value) => {
                        let mut error = to_mcp(&witness_error);
                        retain_request_result(&mut error, &value);
                        error
                    }
                    Err(error) => error,
                };
                mark_unknown_outcome(&mut error, &witness_error);
                return Err(error);
            }
        };
        match result {
            Ok(value) => {
                if let Err(mut error) = self.flush_dirty_storage(storage) {
                    let data = error_data(&mut error);
                    data["mutation_committed"] = json!(changed);
                    data["previous_sync_pending"] = json!(previous_sync_pending);
                    data["sync_pending"] = json!(true);
                    data["retry_mutation"] = json!(false);
                    retain_request_result(&mut error, &value);
                    return Err(error);
                }
                Ok(value)
            }
            Err(mut error) => {
                if changed {
                    mark_committed_error(&mut error);
                    if let Err(mut flush_error) = self.flush_dirty_storage(storage) {
                        let data = error_data(&mut flush_error);
                        data["mutation_committed"] = json!(true);
                        data["previous_sync_pending"] = json!(previous_sync_pending);
                        data["sync_pending"] = json!(true);
                        data["retry_mutation"] = json!(false);
                        data["mutation_error"] = json!({
                            "code": i32::from(error.code), "message": error.message, "data": error.data,
                        });
                        return Err(flush_error);
                    }
                    error_data(&mut error)["sync_pending"] = json!(false);
                }
                Err(error)
            }
        }
    }

    fn flush_dirty_storage(&self, storage: &mut SqliteStorage) -> fastmcp_rust::McpResult<()> {
        let flush_result = crate::sync::auto_flush(
            storage,
            &self.beads_dir,
            &self.jsonl_path,
            self.allow_external_jsonl,
            self.history.clone(),
        )
        .map_err(|err| auto_flush_mcp_error(&self.beads_dir, &self.jsonl_path, err))?;

        if !flush_result.flushed {
            let (remaining_dirty, needs_flush, pending) =
                crate::sync::pending_export_state(storage, self.jsonl_path.exists())
                    .map_err(|err| auto_flush_mcp_error(&self.beads_dir, &self.jsonl_path, err))?;
            if pending {
                return Err(auto_flush_mcp_error(
                    &self.beads_dir,
                    &self.jsonl_path,
                    dirty_auto_flush_incomplete_error(remaining_dirty, needs_flush),
                ));
            }
        }

        Ok(())
    }
}

/// Preserve the distinction between an unchanged refusal and an operation that
/// committed before a later step failed, including individual batch items.
fn with_item_outcome<T>(
    storage: &mut SqliteStorage,
    operation: impl FnOnce(&mut SqliteStorage) -> McpResult<T>,
) -> McpResult<T> {
    let before = storage.get_dirty_issue_metadata().map_err(to_mcp)?;
    operation(storage).map_err(|mut error| {
        match storage.get_dirty_issue_metadata() {
            Ok(after) if after != before => mark_committed_error(&mut error),
            Ok(_) => {}
            Err(witness_error) => mark_unknown_outcome(&mut error, &witness_error),
        }
        error
    })
}

fn error_data(error: &mut McpError) -> &mut Value {
    let data = error.data.get_or_insert_with(|| json!({}));
    if !data.is_object() {
        *data = json!({"original_error": data.clone()});
    }
    data
}

fn retain_request_result(error: &mut McpError, value: &impl serde::Serialize) {
    let data = error_data(error);
    match serde_json::to_value(value) {
        Ok(result) => data["request_result"] = result,
        Err(error) => data["request_result_error"] = json!(error.to_string()),
    }
}

fn mark_unknown_outcome(error: &mut McpError, witness_error: &impl std::fmt::Display) {
    let data = error_data(error);
    data["mutation_committed"] = Value::Null;
    data["retry_mutation"] = json!(false);
    data["outcome_error"] = json!(witness_error.to_string());
    data["recovery"] = json!(
        "Inspect the issue and audit events before retrying; mutation outcome could not be determined."
    );
}

fn mark_committed_error(error: &mut McpError) {
    let data = error_data(error);
    data["mutation_committed"] = json!(true);
    data["retry_mutation"] = json!(false);
    data["publication"] = json!("see_request_outcome");
    data["recovery"] = json!(
        "Inspect the committed issue and audit events; retry only the unfinished operation, not the whole mutation."
    );
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::fs;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::Mutex;

    use chrono::Utc;
    use serde_json::json;
    use tempfile::TempDir;

    use super::*;
    use crate::model::Issue;

    fn test_issue(id: &str, title: &str) -> Issue {
        let now = Utc::now();
        Issue {
            id: id.to_string(),
            title: title.to_string(),
            created_at: now,
            updated_at: now,
            created_by: Some("mcp-test".to_string()),
            ..Issue::default()
        }
    }

    fn test_state(temp: &TempDir, jsonl_path: PathBuf) -> BeadsState {
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");
        SqliteStorage::open(&db_path).unwrap();

        BeadsState {
            db_path,
            beads_dir,
            jsonl_path,
            // Robust under heavy parallel-test load (a concurrent auto-flush can
            // hold .write.lock for >25ms); no test asserts the timeout path.
            write_lock_timeout_ms: Some(5_000),
            allow_external_jsonl: false,
            actor: "mcp-test".to_string(),
            issue_prefix: Some("br".to_string()),
            history: crate::sync::history::HistoryConfig::default(),
            read_snapshot_cache: None,
        }
    }

    fn test_state_with_read_snapshot(temp: &TempDir, jsonl_path: PathBuf) -> BeadsState {
        let mut state = test_state(temp, jsonl_path);
        state.read_snapshot_cache = Some(Mutex::new(McpReadSnapshotCache::default()));
        state
    }

    #[test]
    fn policy_refresh_invalidates_read_cache_and_refuses_before_mutation() {
        let temp = TempDir::new().unwrap();
        let state = test_state_with_read_snapshot(&temp, temp.path().join(".beads/issues.jsonl"));
        let policy_path = state.beads_dir.join("policy.yaml");
        fs::write(
            &policy_path,
            "workflow:\n  status_groups:\n    ready: [open]\n",
        )
        .unwrap();
        let witness = state.capture_read_snapshot_witness();
        state.store_read_json_snapshot("policy-read".to_string(), witness, &json!({"ready": 0}));
        assert!(state.cached_read_json("policy-read").is_some());
        fs::write(&policy_path, "workflow: [malformed").unwrap();
        assert!(state.cached_read_json("policy-read").is_none());
        let before = fs::read(&state.db_path).unwrap();
        let called = Cell::new(false);
        let error = state
            .with_mutation(|_, _| {
                called.set(true);
                Ok(())
            })
            .unwrap_err();
        assert!(!called.get());
        assert!(error.message.contains("policy"), "{error}");
        assert_eq!(fs::read(&state.db_path).unwrap(), before);
    }

    #[test]
    fn request_policy_installs_capacity_before_create() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp, temp.path().join(".beads/issues.jsonl"));
        let path = state.beads_dir.join("policy.yaml");
        fs::write(
            &path,
            "workflow:\n  statuses: [open]\n  capacity:\n    statuses:\n      open:\n        hard: 1\n",
        )
        .unwrap();
        state
            .with_mutation(|storage, _| {
                storage
                    .create_issue(&test_issue("br-policy-a", "first"), &state.actor)
                    .map_err(to_mcp)
            })
            .unwrap();
        let before = fs::read(&state.jsonl_path).unwrap();
        let error = state
            .with_mutation(|storage, _| {
                storage
                    .create_issue(&test_issue("br-policy-b", "second"), &state.actor)
                    .map_err(to_mcp)
            })
            .unwrap_err();
        assert!(error.message.contains("capacity"), "{error}");
        assert_eq!(fs::read(&state.jsonl_path).unwrap(), before);
        let storage = state.open_read_storage().unwrap();
        assert_eq!(storage.count_issues().unwrap(), 1);
        assert_eq!(storage.get_dirty_issue_count().unwrap(), 0);
        drop(storage);
        fs::write(
            &path,
            "workflow:\n  statuses: [open]\n  capacity:\n    statuses:\n      open:\n        hard: 2\n",
        )
        .unwrap();
        state
            .with_mutation(|storage, _| {
                storage
                    .create_issue(&test_issue("br-policy-b", "second"), &state.actor)
                    .map_err(to_mcp)
            })
            .unwrap();
        assert_eq!(
            state.open_read_storage().unwrap().count_issues().unwrap(),
            2
        );
    }

    fn install_valid_pending_merge_receipt(
        state: &BeadsState,
    ) -> crate::sync::SyncMergePendingReceipt {
        let mut storage = SqliteStorage::open(&state.db_path).unwrap();
        let database_before = crate::sync::capture_sync_database_witness(&storage).unwrap();
        let intent = crate::sync::SyncMergeIntent {
            schema_version: 2,
            database_authority_sha256: "1".repeat(64),
            jsonl_authority_sha256: "2".repeat(64),
            jsonl_path_sha256: "3".repeat(64),
            jsonl_before: crate::sync::JsonlSourceStateWitness::Missing,
            jsonl_before_content_sha256: None,
            base_authority_sha256: "4".repeat(64),
            base_before: crate::sync::JsonlSourceStateWitness::Missing,
            base_before_content_sha256: None,
            resolution: "manual".to_string(),
            actor: "mcp-test".to_string(),
            event_attribution: crate::storage::EventAttribution::default(),
            capacity_policy: crate::close_policy::CapacityPolicy::default(),
            retention_days: None,
            export_as_of: chrono::DateTime::parse_from_rfc3339("2026-07-27T00:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            changed_kept_issue_ids: Vec::new(),
            kept_issue_witnesses: Vec::new(),
            deleted_issue_ids: Vec::new(),
            note_witnesses: Vec::new(),
            database_before,
        };
        let database_after = crate::sync::capture_sync_merge_core_witness(&storage).unwrap();
        let receipt = crate::sync::SyncMergePendingReceipt::new(
            intent,
            "2026-07-27T00:00:00Z".to_string(),
            database_after,
            "5".repeat(64),
            0,
            &[],
            Vec::new(),
        )
        .unwrap();
        receipt.validate().unwrap();
        storage
            .set_metadata(
                crate::sync::METADATA_SYNC_MERGE_PENDING,
                &serde_json::to_string(&receipt).unwrap(),
            )
            .unwrap();
        receipt
    }

    #[test]
    fn shutdown_guard_allows_handlers_when_no_signal_is_pending() {
        ensure_not_shutting_down_with(|| false).expect("unsignalled MCP handler should proceed");
    }

    #[test]
    fn shutdown_guard_returns_structured_mcp_error() {
        let err = ensure_not_shutting_down_with(|| true).unwrap_err();

        assert_eq!(err.code, McpErrorCode::ToolExecutionError);
        assert_eq!(err.message, "Shutdown requested");
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|data| data.get("error_type"))
                .and_then(serde_json::Value::as_str),
            Some("SHUTTING_DOWN")
        );
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|data| data.get("context"))
                .and_then(|context| context.get("shutdown_requested"))
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
    }

    #[test]
    fn open_read_storage_uses_read_only_fast_path_without_write_lock() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let jsonl_path = beads_dir.join("issues.jsonl");
        let state = test_state(&temp, jsonl_path);
        let _held_lock =
            crate::sync::blocking_write_lock(&state.beads_dir).expect("hold write lock");

        let storage = state
            .open_read_storage()
            .expect("current schema read storage should not wait for write lock");

        assert_eq!(storage.count_all_issues().unwrap(), 0);
    }

    #[test]
    fn writable_read_fallback_refuses_stale_schema_without_repairing_it() {
        let temp = TempDir::new().unwrap();
        let jsonl_path = temp.path().join(".beads").join("issues.jsonl");
        let state = test_state(&temp, jsonl_path.clone());
        let storage = SqliteStorage::open(&state.db_path).unwrap();
        storage.execute_test_sql("PRAGMA user_version = 1").unwrap();
        drop(storage);
        let database_before = fs::read(&state.db_path).unwrap();

        let err = state.open_read_storage().unwrap_err();

        assert!(
            err.to_string().contains("could not be proven absent")
                && err.to_string().contains("br doctor"),
            "stale-schema fallback must fail closed with remediation: {err}"
        );
        assert_eq!(
            fs::read(&state.db_path).unwrap(),
            database_before,
            "writable read fallback must not migrate or repair a stale database"
        );
        assert!(
            SqliteStorage::open_current_read_only(&state.db_path)
                .unwrap()
                .is_none(),
            "failed fallback must leave the stale schema version unchanged"
        );
        assert!(
            !jsonl_path.exists(),
            "failed read fallback must not create or rewrite JSONL"
        );

        let called = Rc::new(Cell::new(false));
        let called_for_closure = Rc::clone(&called);
        let mutation_err = state
            .with_mutation(|_, _| {
                called_for_closure.set(true);
                Ok(())
            })
            .unwrap_err();
        assert!(
            !called.get(),
            "unknown pending state must refuse before the mutation closure"
        );
        assert_eq!(
            mutation_err
                .data
                .as_ref()
                .and_then(|data| data.get("error_type"))
                .and_then(serde_json::Value::as_str),
            Some("SYNC_MERGE_PENDING_UNKNOWN")
        );
        assert_eq!(
            fs::read(&state.db_path).unwrap(),
            database_before,
            "unknown-state mutation refusal must not repair the stale database"
        );
    }

    #[test]
    fn read_snapshot_cache_returns_value_when_witness_is_stable() {
        let temp = TempDir::new().unwrap();
        let jsonl_path = temp.path().join(".beads").join("issues.jsonl");
        let state = test_state_with_read_snapshot(&temp, jsonl_path);
        let cached = json!({"count": 1});

        let witness = state.capture_read_snapshot_witness();
        state.store_read_json_snapshot("test".to_string(), witness, &cached);

        assert_eq!(state.cached_read_json("test"), Some(cached));
    }

    #[test]
    fn read_snapshot_cache_rejects_jsonl_witness_mismatch() {
        let temp = TempDir::new().unwrap();
        let jsonl_path = temp.path().join(".beads").join("issues.jsonl");
        let state = test_state_with_read_snapshot(&temp, jsonl_path.clone());
        let cached = json!({"count": 1});

        let witness = state.capture_read_snapshot_witness();
        state.store_read_json_snapshot("test".to_string(), witness, &cached);
        fs::write(jsonl_path, "{\"id\":\"br-new\"}\n").unwrap();

        assert_eq!(state.cached_read_json("test"), None);
    }

    #[test]
    fn with_mutation_clears_read_snapshot_cache_before_writing() {
        let temp = TempDir::new().unwrap();
        let jsonl_path = temp.path().join(".beads").join("issues.jsonl");
        let state = test_state_with_read_snapshot(&temp, jsonl_path);
        let cached = json!({"count": 1});
        let witness = state.capture_read_snapshot_witness();
        state.store_read_json_snapshot("test".to_string(), witness, &cached);

        state
            .with_mutation(|storage, _| {
                assert!(
                    storage.attached_write_authority().is_some(),
                    "MCP mutation storage must retain database-family authority"
                );
                storage
                    .create_issue(
                        &test_issue("br-mcp-cache-clear", "clear stale read cache"),
                        "mcp-test",
                    )
                    .map_err(to_mcp)?;
                Ok(())
            })
            .unwrap();

        assert_eq!(state.cached_read_json("test"), None);
    }

    /// GitHub #484: the MCP auto-flush must export with the resolved history
    /// policy, not `HistoryConfig::default()`.
    #[test]
    fn with_mutation_auto_flush_honors_resolved_history_config() {
        fn run(history: crate::sync::history::HistoryConfig) -> (TempDir, PathBuf) {
            let temp = TempDir::new().unwrap();
            let jsonl_path = temp.path().join(".beads").join("issues.jsonl");
            let mut state = test_state(&temp, jsonl_path.clone());
            state.history = history;

            // First flush publishes issues.jsonl (nothing to back up yet).
            state
                .with_mutation(|storage, _| {
                    storage
                        .create_issue(&test_issue("br-mcp-hist", "history knob"), "mcp-test")
                        .map_err(to_mcp)?;
                    Ok(())
                })
                .unwrap();
            assert!(jsonl_path.exists(), "first mutation must publish JSONL");

            // Second flush replaces an existing JSONL: the only point where a
            // `.br_history` snapshot can be taken.
            state
                .with_mutation(|storage, _| {
                    storage
                        .update_issue(
                            "br-mcp-hist",
                            &crate::storage::IssueUpdate {
                                title: Some("history knob changed".to_string()),
                                ..Default::default()
                            },
                            "mcp-test",
                        )
                        .map_err(to_mcp)?;
                    Ok(())
                })
                .unwrap();

            let history_dir = state.beads_dir.join(".br_history");
            (temp, history_dir)
        }

        let (_disabled_temp, disabled) = run(crate::sync::history::HistoryConfig {
            enabled: false,
            ..Default::default()
        });
        assert!(
            !disabled.exists(),
            "history disabled: MCP auto-flush must not create {}",
            disabled.display()
        );

        let (_enabled_temp, enabled) = run(crate::sync::history::HistoryConfig {
            enabled: true,
            min_interval_secs: 0,
            ..Default::default()
        });
        let snapshots = fs::read_dir(&enabled)
            .expect("history dir exists when enabled")
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .path()
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
            })
            .count();
        assert_eq!(snapshots, 1, "control: enabled history snapshots once");
    }

    #[test]
    fn with_mutation_requires_openable_sync_lock_before_mutating() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let jsonl_path = beads_dir.join("issues.jsonl");
        let state = test_state(&temp, jsonl_path);
        fs::create_dir(state.beads_dir.join(".sync.lock")).unwrap();
        let database_before = fs::read(&state.db_path).unwrap();
        let called = Rc::new(Cell::new(false));
        let called_for_closure = Rc::clone(&called);

        let err = state
            .with_mutation(|storage, _| {
                called_for_closure.set(true);
                storage
                    .create_issue(
                        &test_issue("br-mcp-lock", "should not be created"),
                        "mcp-test",
                    )
                    .map_err(to_mcp)?;
                Ok(())
            })
            .unwrap_err();

        assert!(
            !called.get(),
            "mutation closure must not run without sync lock"
        );
        assert_eq!(err.code, McpErrorCode::ToolExecutionError);
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|data| data.get("error_type"))
                .and_then(serde_json::Value::as_str),
            Some("SYNC_LOCK_UNAVAILABLE")
        );
        assert_eq!(
            fs::read(&state.db_path).unwrap(),
            database_before,
            "sync-lock refusal must occur before writable storage open"
        );
        let storage = SqliteStorage::open(&state.db_path).unwrap();
        assert!(!storage.id_exists("br-mcp-lock").unwrap());
    }

    #[test]
    fn with_mutation_refuses_malformed_pending_state_before_invoking_closure() {
        let temp = TempDir::new().unwrap();
        let jsonl_path = temp.path().join(".beads").join("issues.jsonl");
        let state = test_state(&temp, jsonl_path.clone());
        let mut storage = SqliteStorage::open(&state.db_path).unwrap();
        storage
            .set_metadata(crate::sync::METADATA_SYNC_MERGE_PENDING, "{")
            .unwrap();
        drop(storage);
        let database_before = fs::read(&state.db_path).unwrap();
        let called = Rc::new(Cell::new(false));
        let called_for_closure = Rc::clone(&called);

        let err = state
            .with_mutation(|_, _| {
                called_for_closure.set(true);
                Ok(())
            })
            .unwrap_err();

        assert!(!called.get(), "pending gate must run before the closure");
        assert_eq!(err.code, McpErrorCode::ToolExecutionError);
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|data| data.get("error_type"))
                .and_then(serde_json::Value::as_str),
            Some("SYNC_MERGE_PENDING")
        );
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|data| data.get("condition"))
                .and_then(serde_json::Value::as_str),
            Some("malformed")
        );
        assert_eq!(
            fs::read(&state.db_path).unwrap(),
            database_before,
            "refused MCP mutation must not change database core bytes"
        );
        assert!(
            !jsonl_path.exists(),
            "refused MCP mutation must not create or rewrite JSONL"
        );
        let storage = SqliteStorage::open_current_read_only(&state.db_path)
            .unwrap()
            .expect("fixture remains current schema");
        assert_eq!(
            storage
                .get_metadata(crate::sync::METADATA_SYNC_MERGE_PENDING)
                .unwrap()
                .as_deref(),
            Some("{"),
            "refused MCP mutation must preserve pending metadata exactly"
        );
    }

    #[test]
    fn with_mutation_returns_structured_legacy_pending_refusal() {
        let temp = TempDir::new().unwrap();
        let jsonl_path = temp.path().join(".beads").join("issues.jsonl");
        let state = test_state(&temp, jsonl_path.clone());
        let mut storage = SqliteStorage::open(&state.db_path).unwrap();
        storage
            .set_metadata(
                crate::sync::METADATA_SYNC_MERGE_PENDING_LEGACY,
                "legacy-receipt",
            )
            .unwrap();
        drop(storage);
        let database_before = fs::read(&state.db_path).unwrap();
        let called = Rc::new(Cell::new(false));
        let called_for_closure = Rc::clone(&called);

        let err = state
            .with_mutation(|_, _| {
                called_for_closure.set(true);
                Ok(())
            })
            .unwrap_err();

        assert!(!called.get(), "legacy gate must precede the closure");
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|data| data.get("error_type"))
                .and_then(serde_json::Value::as_str),
            Some("SYNC_MERGE_PENDING")
        );
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|data| data.get("condition"))
                .and_then(serde_json::Value::as_str),
            Some("legacy")
        );
        assert!(
            err.data
                .as_ref()
                .and_then(|data| data.get("recovery"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|recovery| recovery.contains("br sync --merge")),
            "legacy refusal must include explicit recovery"
        );
        assert_eq!(
            fs::read(&state.db_path).unwrap(),
            database_before,
            "legacy refusal must not change database core bytes"
        );
        assert!(!jsonl_path.exists());
    }

    #[test]
    fn long_lived_server_refuses_receipt_committed_after_start_before_invoking_closure() {
        let temp = TempDir::new().unwrap();
        let jsonl_path = temp.path().join(".beads").join("issues.jsonl");
        let state = test_state(&temp, jsonl_path.clone());
        fs::write(&jsonl_path, b"{\"id\":\"br-existing\"}\n").unwrap();

        // `state` represents an already-running server. Commit the receipt
        // through a separate connection only after that long-lived state exists.
        let receipt = install_valid_pending_merge_receipt(&state);
        let database_before = fs::read(&state.db_path).unwrap();
        let jsonl_before = fs::read(&jsonl_path).unwrap();
        let called = Rc::new(Cell::new(false));
        let called_for_closure = Rc::clone(&called);

        let err = state
            .with_mutation(|_, _| {
                called_for_closure.set(true);
                Ok(())
            })
            .unwrap_err();

        assert!(
            !called.get(),
            "live receipt inspection must precede the mutation closure"
        );
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|data| data.get("error_type"))
                .and_then(serde_json::Value::as_str),
            Some("SYNC_MERGE_PENDING")
        );
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|data| data.get("condition"))
                .and_then(serde_json::Value::as_str),
            Some("valid")
        );
        assert_eq!(
            fs::read(&state.db_path).unwrap(),
            database_before,
            "refused live MCP mutation must not change database core bytes"
        );
        assert_eq!(
            fs::read(&jsonl_path).unwrap(),
            jsonl_before,
            "refused live MCP mutation must not change JSONL bytes"
        );
        let storage = SqliteStorage::open_current_read_only(&state.db_path)
            .unwrap()
            .expect("fixture remains current schema");
        assert_eq!(
            storage.pending_sync_merge_receipt().unwrap(),
            Some(receipt),
            "refused live MCP mutation must preserve the exact receipt"
        );
    }

    #[test]
    fn with_mutation_reports_auto_flush_failure_and_preserves_dirty_state() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let jsonl_path = beads_dir.join("issues.jsonl");
        let state = test_state(&temp, jsonl_path.clone());
        fs::write(
            &jsonl_path,
            "<<<<<<< HEAD\n{}\n=======\n{}\n>>>>>>> branch\n",
        )
        .unwrap();

        let err = state
            .with_mutation(|storage, _| {
                storage
                    .create_issue(&test_issue("br-mcp-dirty", "dirty issue"), "mcp-test")
                    .map_err(to_mcp)?;
                Ok(())
            })
            .unwrap_err();

        assert_eq!(err.code, McpErrorCode::ToolExecutionError);
        assert_eq!(
            err.data
                .as_ref()
                .and_then(|data| data.get("error_type"))
                .and_then(serde_json::Value::as_str),
            Some("AUTO_FLUSH_FAILED")
        );

        let storage = SqliteStorage::open(&state.db_path).unwrap();
        assert!(storage.id_exists("br-mcp-dirty").unwrap());
        assert_eq!(storage.get_dirty_issue_count().unwrap(), 1);
        let jsonl = fs::read_to_string(jsonl_path).unwrap();
        assert!(jsonl.contains("<<<<<<<"));
    }

    #[test]
    fn no_op_reports_forced_export_pending_after_a_real_purge() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp, temp.path().join(".beads/issues.jsonl"));
        state
            .with_mutation(|storage, _| {
                storage
                    .create_issue(&test_issue("br-purge", "Pending purge"), &state.actor)
                    .map_err(to_mcp)
            })
            .unwrap();
        {
            let mut storage = state.open_storage_with_fresh_write_authority().unwrap();
            storage.purge_issue("br-purge", &state.actor).unwrap();
            assert_eq!(storage.get_dirty_issue_count().unwrap(), 0);
            assert_eq!(storage.count_issues().unwrap(), 0);
            assert_eq!(
                storage.get_metadata("needs_flush").unwrap().as_deref(),
                Some("true")
            );
        }
        let conflict = "<<<<<<< ours\n{}\n=======\n{}\n>>>>>>> theirs\n";
        fs::write(&state.jsonl_path, conflict).unwrap();
        let expected = json!({"count": 0, "ok_count": 0, "error_count": 0});
        let error = state
            .with_mutation(|_, _| Ok(expected.clone()))
            .unwrap_err();
        let data = error.data.unwrap();
        assert_eq!(data["error_type"], "AUTO_FLUSH_FAILED");
        assert_eq!(data["mutation_committed"], false);
        assert_eq!(data["previous_sync_pending"], true);
        assert_eq!(data["sync_pending"], true);
        assert_eq!(data["retry_mutation"], false);
        assert_eq!(data["request_result"], expected);
        assert_eq!(fs::read_to_string(&state.jsonl_path).unwrap(), conflict);

        fs::rename(
            &state.jsonl_path,
            state.beads_dir.join("preserved-conflict.jsonl"),
        )
        .unwrap();
        assert_eq!(
            state.with_mutation(|_, _| Ok(expected.clone())).unwrap(),
            expected
        );
        assert!(fs::read(&state.jsonl_path).unwrap().is_empty());
        let storage = state.open_read_storage().unwrap();
        assert_eq!(
            crate::sync::pending_export_state(&storage, true).unwrap(),
            (0, false, false)
        );
    }

    #[test]
    fn with_mutation_preserves_original_error_when_commit_witness_is_unreadable() {
        let temp = TempDir::new().unwrap();
        let state = test_state(&temp, temp.path().join(".beads/issues.jsonl"));
        let error = state
            .with_mutation(|storage, _| -> McpResult<()> {
                storage
                    .create_issue(
                        &test_issue("br-unreadable-witness", "Committed issue"),
                        &state.actor,
                    )
                    .map_err(to_mcp)?;
                // A deliberately malformed witness exercises the unavailable
                // outcome path using the real database, without a production hook.
                storage
                    .execute_test_sql("UPDATE dirty_issues SET marked_at = X'FF'")
                    .map_err(to_mcp)?;
                Err(McpError::with_data(
                    McpErrorCode::InvalidParams,
                    "Original late failure",
                    json!({"error_type": "ORIGINAL_FAILURE", "operation": "append_comment"}),
                ))
            })
            .unwrap_err();
        assert_eq!(error.code, McpErrorCode::InvalidParams);
        assert_eq!(error.message, "Original late failure");
        let data = error.data.expect("outcome context");
        assert_eq!(data["error_type"], "ORIGINAL_FAILURE");
        assert_eq!(data["operation"], "append_comment");
        assert!(data["mutation_committed"].is_null());
        assert_eq!(data["retry_mutation"], false);
        assert!(
            data["outcome_error"]
                .as_str()
                .unwrap()
                .contains("marked_at was not text")
        );
    }

    #[test]
    fn with_mutation_flushes_committed_changes_before_returning_late_error() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let jsonl_path = beads_dir.join("issues.jsonl");
        let state = test_state(&temp, jsonl_path.clone());

        let err = state
            .with_mutation(|storage, _| -> fastmcp_rust::McpResult<()> {
                storage
                    .create_issue(
                        &test_issue("br-mcp-partial", "partial mutation"),
                        "mcp-test",
                    )
                    .map_err(to_mcp)?;
                Err(fastmcp_rust::McpError::invalid_params(
                    "simulated side-effect failure",
                ))
            })
            .unwrap_err();

        assert_eq!(err.code, McpErrorCode::InvalidParams);
        let detail = err.data.as_ref().expect("partial mutation detail");
        assert_eq!(detail["mutation_committed"], true);
        assert_eq!(detail["retry_mutation"], false);
        assert_eq!(detail["sync_pending"], false);

        let storage = SqliteStorage::open(&state.db_path).unwrap();
        assert!(storage.id_exists("br-mcp-partial").unwrap());
        assert_eq!(storage.get_dirty_issue_count().unwrap(), 0);

        let jsonl = fs::read_to_string(jsonl_path).unwrap();
        assert!(
            jsonl.contains("\"id\":\"br-mcp-partial\""),
            "late-error committed mutation must still reach JSONL"
        );
    }
}

/// CLI arguments for `br serve`.
#[derive(clap::Args, Debug, Clone)]
pub struct ServeArgs {
    /// Actor name for mutations (defaults to "mcp")
    #[arg(long, default_value = "mcp")]
    pub actor: String,
}

/// Entry point: build and run the MCP server on stdio.
///
/// # Errors
///
/// Returns an error if the beads workspace is not initialised or storage
/// cannot be opened.
/// Build the runtime-backed serve context.
///
/// asupersync 0.4.8 gates `Cx::for_request()` behind `test-internals`; the
/// production ambient-free entry is a runtime-minted request Cx. The returned
/// runtime object must outlive the serve loop, so the caller keeps it alive.
fn build_serve_cx() -> crate::Result<(asupersync::runtime::Runtime, fastmcp_rust::Cx)> {
    let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
        .build()
        .map_err(|e| {
            BeadsError::Config(format!("failed to build asupersync runtime for serve: {e}"))
        })?;
    let cx = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    Ok((runtime, cx))
}

/// Open the workspace once under the write lock to read the configured
/// prefix, resolve the paths and the history config every auto-flush must
/// use, then release everything before the server starts.
///
/// Everything the bootstrap open owns — the connection *and* the
/// database-family write authority the open result keeps a clone of — must
/// drop here. Keeping the result alive held `.beads/.write.lock` for the
/// whole serve session: every mutating tool then timed out waiting for a
/// lock it could never get, and CLI writes in other processes hung.
fn bootstrap_serve_paths(
    beads_dir: &Path,
    startup: config::StartupConfig,
    overrides: &config::CliOverrides,
    lock_timeout: Option<u64>,
) -> crate::Result<(
    Option<String>,
    PathBuf,
    PathBuf,
    crate::sync::history::HistoryConfig,
)> {
    let write_lock = Arc::new(
        crate::sync::blocking_database_family_write_lock_with_timeout(
            beads_dir,
            &startup.paths.db_path,
            lock_timeout,
        )?,
    );
    let res = config::open_storage_with_startup_config_under_write_lock(
        startup,
        overrides,
        false,
        &write_lock,
    )?;
    let prefix = res
        .storage
        .get_config("issue_prefix")?
        .map(|prefix| crate::util::id::normalize_configured_prefix(&prefix))
        .transpose()?;
    let history = res.resolved_history_config();
    Ok((
        prefix,
        res.paths.db_path.clone(),
        res.paths.jsonl_path.clone(),
        history,
    ))
}

pub fn run_serve(args: &ServeArgs, overrides: &config::CliOverrides) -> crate::Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(overrides)?;
    let startup = config::load_startup_config_with_paths(&beads_dir, overrides.db.as_ref())?;
    let mut startup_layers = startup.layers.clone();
    startup_layers.push(overrides.as_layer());
    let merged_layer = config::ConfigLayer::merge_layers(&startup_layers);
    let lock_timeout = overrides
        .lock_timeout
        .or_else(|| config::lock_timeout_from_layer(&merged_layer))
        .or(Some(crate::sync::default_write_lock_timeout_ms()));
    let (prefix, db_path, jsonl_path, history) =
        bootstrap_serve_paths(&beads_dir, startup, overrides, lock_timeout)?;
    let allow_external_jsonl =
        config::implicit_external_jsonl_allowed(&beads_dir, &db_path, &jsonl_path);
    let state = std::sync::Arc::new(BeadsState {
        db_path,
        beads_dir,
        jsonl_path,
        write_lock_timeout_ms: lock_timeout,
        allow_external_jsonl,
        actor: args.actor.clone(),
        issue_prefix: prefix,
        history,
        read_snapshot_cache: mcp_read_snapshot_cache_from_env(),
    });

    let server = fastmcp_rust::modern::ServerBuilder::new("br", env!("CARGO_PKG_VERSION"))
        .instructions(
            "beads_rust (br) issue tracker MCP server.\n\n\
             Use tools to query, create, and manage issues. All mutations are \
             recorded with full audit trails.\n\n\
             Getting started:\n\
             1. Call project_overview to understand the project state\n\
             2. Read beads://schema for valid field values and bead anatomy guidance\n\
             3. Read beads://labels to discover existing labels\n\
             4. Use list_issues to find specific issues\n\n\
             Discovery resources: beads://project/info, beads://schema, \
             beads://labels, beads://issues/ready, beads://issues/blocked, \
             beads://issues/in_progress, beads://coordination/status, \
             beads://issues/deferred, beads://issues/bottlenecks, \
             beads://graph/health, beads://events/recent\n\n\
             Guided workflows:\n\
             - 'triage' — backlog triage (blocked, unassigned, deferred)\n\
             - 'status_report' — project status report generation\n\
             - 'plan_next_work' — graph-aware work planning (bottlenecks, quick wins)\n\
             - 'polish_backlog' — review issue quality and dependency health",
        )
        // Tools (7 — at the ≤7 cluster ceiling)
        .tool(tools::ListIssuesTool::new(state.clone()))
        .tool(tools::ShowIssueTool::new(state.clone()))
        .tool(tools::CreateIssueTool::new(state.clone()))
        .tool(tools::UpdateIssueTool::new(state.clone()))
        .tool(tools::CloseIssueTool::new(state.clone()))
        .tool(tools::ManageDependenciesTool::new(state.clone()))
        .tool(tools::ProjectOverviewTool::new(state.clone()))
        // Resources (12)
        .resource(resources::ProjectInfoResource::new(state.clone()))
        .resource(resources::SchemaResource)
        .resource(resources::LabelsResource::new(state.clone()))
        .resource(resources::ReadyIssuesResource::new(state.clone()))
        .resource(resources::BlockedIssuesResource::new(state.clone()))
        .resource(resources::InProgressResource::new(state.clone()))
        .resource(resources::CoordinationStatusResource::new(state.clone()))
        .resource(resources::EventsResource::new(state.clone()))
        .resource(resources::DeferredIssuesResource::new(state.clone()))
        .resource(resources::GraphHealthResource::new(state.clone()))
        .resource(resources::BottlenecksResource::new(state.clone()))
        // fastmcp rejects overlapping exact/template registrations. Individual
        // issues use the singular namespace; collections retain issues/.
        .resource(resources::IssueResource::new(state.clone()))
        // Prompts (4)
        .prompt(prompts::TriagePrompt::new(state.clone()))
        .prompt(prompts::StatusReportPrompt::new(state.clone()))
        .prompt(prompts::PlanNextWorkPrompt::new(state.clone()))
        .prompt(prompts::PolishBacklogPrompt::new(state))
        .build();

    // The stdio transport observes `cx.is_cancel_requested()` between its
    // read polls, so translating br's cooperative shutdown flag
    // (SIGINT/SIGTERM/SIGHUP; see `crate::shutdown`) into a Cx cancellation
    // lets `br serve` return through `main` and run every destructor (WAL
    // flush on drop, #270) instead of waiting on transport EOF detection.
    let (_serve_runtime, serve_cx) = build_serve_cx()?;
    let watcher_cx = serve_cx.clone();
    std::thread::spawn(move || {
        while !crate::shutdown::is_requested() {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        watcher_cx.set_cancel_requested(true);
    });
    server
        .run_transport_returning_with_cx(&serve_cx, StdioTransport::stdio())
        .map_err(|e| BeadsError::Config(format!("MCP serve transport failed: {e}")))?;
    Ok(())
}
