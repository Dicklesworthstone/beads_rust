//! JSONL import/export for `beads_rust`.
//!
//! This module handles:
//! - Export: `SQLite` -> JSONL (for git tracking)
//! - Import: JSONL -> `SQLite` (for git clone/pull)
//! - Dirty tracking for incremental exports
//! - Collision detection during imports
//! - Path validation and allowlist enforcement

pub mod history;
pub mod path;
pub mod witness;

pub use path::{
    ALLOWED_EXACT_NAMES, ALLOWED_EXTENSIONS, PathValidation, is_sync_path_allowed,
    require_safe_sync_overwrite_path, require_valid_sync_path, validate_no_git_path,
    validate_sync_path, validate_sync_path_with_external, validate_temp_file_path,
};

use crate::error::{BeadsError, Result};
use crate::model::{Comment, Dependency, Issue};
use crate::storage::SqliteStorage;
use crate::sync::history::HistoryConfig;
use crate::util::id::{IdConfig, IdGenerator, parse_id};
use crate::util::progress::{create_progress_bar, create_spinner};
use crate::validation::IssueValidator;
use chrono::{DateTime, Utc};
use fsqlite_types::SqliteValue;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::util::hex_encode;
use indicatif::ProgressBar;
use std::collections::{BTreeMap, HashMap, HashSet, hash_map::RandomState};
use std::fmt::Write as FmtWrite;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_WRITE_LOCK_TIMEOUT_MS: u64 = 30_000;
const WRITE_LOCK_POLL_INTERVAL: Duration = Duration::from_millis(25);
const EXPORT_ISSUE_BATCH_SIZE: usize = 1024;
const EXPORT_FULL_SCAN_ISSUE_THRESHOLD: usize = 20_000;
const EXPORT_PARALLEL_PREPARE_MIN_ISSUES: usize = 256;
const DEFAULT_JSONL_EXPORT_PARALLELISM: usize = 64;
const IMPORT_EXPORT_HASH_BATCH_SIZE: usize = 512;
const MAX_JSONL_TEMP_PATH_ATTEMPTS: u32 = 64;

/// Acquire a blocking exclusive lock on `.beads/.write.lock`.
///
/// This serializes all mutating operations across processes, preventing
/// concurrent-write deadlocks in the underlying SQLite engine. Uses a fast-path
/// `try_lock()` for the uncontended case, then polls with a bounded timeout for
/// contended locks. The lock is held until the returned `File` drops.
#[allow(clippy::incompatible_msrv)]
pub fn blocking_write_lock(beads_dir: &Path) -> Result<File> {
    blocking_write_lock_with_timeout(beads_dir, None)
}

/// Acquire a bounded exclusive lock on `.beads/.write.lock`.
///
/// `lock_timeout_ms` uses the same millisecond setting as `--lock-timeout`.
/// When unset, a 30s default prevents a stuck writer from parking every
/// subsequent mutating command indefinitely.
#[allow(clippy::incompatible_msrv)]
pub fn blocking_write_lock_with_timeout(
    beads_dir: &Path,
    lock_timeout_ms: Option<u64>,
) -> Result<File> {
    let lock_path = beads_dir.join(".write.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|err| {
            BeadsError::Config(format!(
                "Failed to open write lock at {}: {err}",
                lock_path.display()
            ))
        })?;

    // Fast path: non-blocking try for the common uncontended case.
    match file.try_lock() {
        Ok(()) => return Ok(file),
        Err(TryLockError::WouldBlock) => {}
        Err(TryLockError::Error(err)) => {
            return Err(BeadsError::Config(format!(
                "Failed to acquire write lock at {}: {err}",
                lock_path.display()
            )));
        }
    }

    let timeout_ms = lock_timeout_ms.unwrap_or(DEFAULT_WRITE_LOCK_TIMEOUT_MS);
    let timeout = Duration::from_millis(timeout_ms);
    let start = Instant::now();
    tracing::debug!(
        timeout_ms,
        lock_path = %lock_path.display(),
        ".write.lock is held by another process; waiting with timeout"
    );

    loop {
        if start.elapsed() >= timeout {
            return Err(write_lock_timeout_error(&lock_path, timeout_ms));
        }

        let remaining = timeout.saturating_sub(start.elapsed());
        thread::sleep(remaining.min(WRITE_LOCK_POLL_INTERVAL));

        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) => {}
            Err(TryLockError::Error(err)) => {
                tracing::debug!("failed to acquire .write.lock: {err}");
                return Err(BeadsError::Config(format!(
                    "Failed to acquire write lock at {}: {err}",
                    lock_path.display()
                )));
            }
        }
    }
}

fn write_lock_timeout_error(lock_path: &Path, timeout_ms: u64) -> BeadsError {
    BeadsError::Config(format!(
        "Timed out after {timeout_ms}ms waiting for write lock at {}. \
         Another br process may be holding .write.lock; retry after it exits or investigate a stuck process.",
        lock_path.display()
    ))
}

#[must_use]
pub const fn default_write_lock_timeout_ms() -> u64 {
    DEFAULT_WRITE_LOCK_TIMEOUT_MS
}

/// Try to acquire an exclusive advisory lock on `.beads/.sync.lock`.
///
/// Returns the lock file on success. The lock is held until the returned
/// `File` is dropped. If another process already holds the lock, returns
/// `Ok(None)` (non-blocking). Lock-file open or OS lock errors are returned
/// separately so callers do not confuse a broken lock path with contention.
#[allow(clippy::incompatible_msrv)]
pub fn try_sync_lock(beads_dir: &Path) -> Result<Option<File>> {
    let lock_path = beads_dir.join(".sync.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|err| {
            BeadsError::Config(format!(
                "Failed to open sync lock at {}: {err}",
                lock_path.display()
            ))
        })?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(err)) => Err(BeadsError::Config(format!(
            "Failed to acquire sync lock at {}: {err}",
            lock_path.display()
        ))),
    }
}

struct TempFileGuard {
    path: PathBuf,
    persist: bool,
}

impl TempFileGuard {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            persist: false,
        }
    }

    fn persist(&mut self) {
        self.persist = true;
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if !self.persist {
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(crate) fn export_temp_path(output_path: &Path) -> PathBuf {
    export_temp_path_for_attempt(output_path, 0)
}

fn export_temp_path_for_attempt(output_path: &Path, attempt: u32) -> PathBuf {
    let pid = std::process::id();
    if attempt == 0 {
        return output_path.with_extension(format!("jsonl.{pid}.tmp"));
    }

    let retry_suffix = u64::from(pid)
        .saturating_mul(100)
        .saturating_add(u64::from(attempt));
    output_path.with_extension(format!("jsonl.{retry_suffix}.tmp"))
}

fn create_jsonl_temp_file(output_path: &Path, config: &ExportConfig) -> Result<(PathBuf, File)> {
    for attempt in 0..MAX_JSONL_TEMP_PATH_ATTEMPTS {
        let temp_path = export_temp_path_for_attempt(output_path, attempt);

        if let Some(ref beads_dir) = config.beads_dir {
            validate_temp_file_path(
                &temp_path,
                output_path,
                beads_dir,
                config.allow_external_jsonl,
            )?;
            tracing::debug!(
                temp_path = %temp_path.display(),
                target_path = %output_path.display(),
                "Temp file path validated"
            );
        }

        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(temp_file) => return Ok((temp_path, temp_file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if fs::symlink_metadata(&temp_path)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
                {
                    return Err(BeadsError::Config(format!(
                        "Temporary export file already exists: {}",
                        temp_path.display()
                    )));
                }
            }
            Err(error) => return Err(error.into()),
        }
    }

    Err(BeadsError::Config(format!(
        "Failed to allocate temporary export file for {}",
        output_path.display()
    )))
}

fn create_base_snapshot_temp_file(
    snapshot_path: &Path,
    jsonl_dir: &Path,
) -> Result<(PathBuf, File)> {
    for attempt in 0..MAX_JSONL_TEMP_PATH_ATTEMPTS {
        let temp_path = export_temp_path_for_attempt(snapshot_path, attempt);
        validate_temp_file_path(&temp_path, snapshot_path, jsonl_dir, false)?;

        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(temp_file) => return Ok((temp_path, temp_file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if fs::symlink_metadata(&temp_path)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
                {
                    return Err(BeadsError::Config(format!(
                        "Temporary base snapshot file already exists: {}",
                        temp_path.display()
                    )));
                }
            }
            Err(error) => return Err(error.into()),
        }
    }

    Err(BeadsError::Config(format!(
        "Failed to allocate temporary base snapshot file for {}",
        snapshot_path.display()
    )))
}

#[cfg(unix)]
fn set_restrictive_jsonl_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    if let Err(error) = fs::set_permissions(path, perms) {
        tracing::warn!(
            path = %path.display(),
            error = %error,
            "Failed to set restrictive permissions on JSONL file"
        );
    }
}

#[cfg(not(unix))]
fn set_restrictive_jsonl_permissions(_path: &Path) {}

/// Configuration for JSONL export.
#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct ExportConfig {
    /// Force export even if database is empty and JSONL has issues.
    pub force: bool,
    /// Whether this is an export to the default JSONL path (affects dirty flag clearing).
    pub is_default_path: bool,
    /// Error handling policy for export.
    pub error_policy: ExportErrorPolicy,
    /// Retention period for tombstones in days (None = keep forever).
    pub retention_days: Option<u64>,
    /// The `.beads` directory path for path validation.
    /// If None, path validation is skipped (for backwards compatibility).
    pub beads_dir: Option<PathBuf>,
    /// Allow JSONL path outside `.beads/` directory (requires explicit opt-in).
    /// Even with this flag, git paths are ALWAYS rejected.
    pub allow_external_jsonl: bool,
    /// Show progress indicators for long-running operations.
    pub show_progress: bool,
    /// Configuration for history backups.
    pub history: HistoryConfig,
    /// Worker cap for parallel JSONL line preparation during file exports.
    ///
    /// `0` means "auto": use up to 64 workers, capped by host parallelism.
    /// `1` is the deterministic serial fallback.
    pub max_parallel_workers: usize,
}

/// Export error handling policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ExportErrorPolicy {
    /// Abort export on any error (default).
    #[default]
    Strict,
    /// Skip problematic records, export what we can.
    BestEffort,
    /// Export valid records, report failures.
    Partial,
    /// Only export core issues; non-core errors are tolerated.
    RequiredCore,
}

impl std::fmt::Display for ExportErrorPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Strict => "strict",
            Self::BestEffort => "best-effort",
            Self::Partial => "partial",
            Self::RequiredCore => "required-core",
        };
        write!(f, "{value}")
    }
}

impl std::str::FromStr for ExportErrorPolicy {
    type Err = String;

    fn from_str(input: &str) -> std::result::Result<Self, Self::Err> {
        match input.to_ascii_lowercase().as_str() {
            "strict" => Ok(Self::Strict),
            "best-effort" | "best_effort" | "best" => Ok(Self::BestEffort),
            "partial" => Ok(Self::Partial),
            "required-core" | "required_core" | "core" => Ok(Self::RequiredCore),
            other => Err(format!(
                "Invalid error policy: {other}. Must be one of: strict, best-effort, partial, required-core"
            )),
        }
    }
}

/// Export entity types for error reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExportEntityType {
    Issue,
    Dependency,
    Label,
    Comment,
}

/// Export error record.
#[derive(Debug, Clone, Serialize)]
pub struct ExportError {
    pub entity_type: ExportEntityType,
    pub entity_id: String,
    pub message: String,
}

impl ExportError {
    fn new(
        entity_type: ExportEntityType,
        entity_id: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            entity_type,
            entity_id: entity_id.into(),
            message: message.into(),
        }
    }

    #[must_use]
    pub fn summary(&self) -> String {
        let id = if self.entity_id.is_empty() {
            "<unknown>"
        } else {
            self.entity_id.as_str()
        };
        format!("{:?} {id}: {}", self.entity_type, self.message)
    }
}

/// Export report with error details and counts.
#[derive(Debug, Clone, Serialize)]
pub struct ExportReport {
    pub issues_exported: usize,
    pub dependencies_exported: usize,
    pub labels_exported: usize,
    pub comments_exported: usize,
    pub errors: Vec<ExportError>,
    pub policy_used: ExportErrorPolicy,
}

impl ExportReport {
    const fn new(policy: ExportErrorPolicy) -> Self {
        Self {
            issues_exported: 0,
            dependencies_exported: 0,
            labels_exported: 0,
            comments_exported: 0,
            errors: Vec::new(),
            policy_used: policy,
        }
    }

    /// True if any errors were recorded.
    #[must_use]
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    /// Success rate for exported entities.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn success_rate(&self) -> f64 {
        let total = self.issues_exported
            + self.dependencies_exported
            + self.labels_exported
            + self.comments_exported;
        let failed = self.errors.len();
        if total + failed == 0 {
            1.0
        } else {
            total as f64 / (total + failed) as f64
        }
    }
}

struct ExportContext {
    policy: ExportErrorPolicy,
    errors: Vec<ExportError>,
}

impl ExportContext {
    const fn new(policy: ExportErrorPolicy) -> Self {
        Self {
            policy,
            errors: Vec::new(),
        }
    }

    fn handle_error(&mut self, err: ExportError) -> Result<()> {
        match self.policy {
            ExportErrorPolicy::Strict => Err(BeadsError::Config(format!(
                "Export error: {}",
                err.summary()
            ))),
            ExportErrorPolicy::BestEffort | ExportErrorPolicy::Partial => {
                self.errors.push(err);
                Ok(())
            }
            ExportErrorPolicy::RequiredCore => {
                if err.entity_type == ExportEntityType::Issue {
                    Err(BeadsError::Config(format!(
                        "Export error: {}",
                        err.summary()
                    )))
                } else {
                    self.errors.push(err);
                    Ok(())
                }
            }
        }
    }
}

/// Result of a JSONL export operation.
#[derive(Debug, Clone, Default)]
pub struct ExportResult {
    /// Number of issues exported.
    pub exported_count: usize,
    /// IDs of exported issues.
    pub exported_ids: Vec<String>,
    /// IDs and timestamps of dirty issues that were cleared.
    pub exported_marked_at: Vec<(String, String)>,
    /// IDs skipped due to expired tombstone retention (still clear dirty flags).
    pub skipped_tombstone_ids: Vec<String>,
    /// SHA256 hash of the exported JSONL content.
    pub content_hash: String,
    /// Output file path (None if stdout).
    pub output_path: Option<String>,
    /// Per-issue content hashes (`issue_id`, `content_hash`) for incremental export tracking.
    pub issue_hashes: Vec<(String, String)>,
}

/// Configuration for JSONL import.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct ImportConfig {
    /// Skip prefix validation when importing.
    pub skip_prefix_validation: bool,
    /// Rewrite IDs and references on prefix mismatch.
    pub rename_on_import: bool,
    /// Clear duplicate external refs instead of erroring.
    pub clear_duplicate_external_refs: bool,
    /// How to handle orphaned issues during import.
    pub orphan_mode: OrphanMode,
    /// Force upsert even if timestamps are equal or older.
    pub force_upsert: bool,
    /// The `.beads` directory path for path validation.
    /// If None, path validation is skipped (for backwards compatibility).
    pub beads_dir: Option<PathBuf>,
    /// Allow JSONL path outside `.beads/` directory (requires explicit opt-in).
    /// Even with this flag, git paths are ALWAYS rejected.
    pub allow_external_jsonl: bool,
    /// Show progress indicators for long-running operations.
    pub show_progress: bool,
}

impl Default for ImportConfig {
    fn default() -> Self {
        Self {
            skip_prefix_validation: false,
            rename_on_import: false,
            clear_duplicate_external_refs: false,
            orphan_mode: OrphanMode::Strict,
            force_upsert: false,
            beads_dir: None,
            allow_external_jsonl: false,
            show_progress: false,
        }
    }
}

/// Orphan handling behavior for import.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrphanMode {
    /// Fail if any issue references a missing parent.
    Strict,
    /// Attempt to resurrect missing parents if found.
    Resurrect,
    /// Skip orphaned issues.
    Skip,
    /// Allow orphans (no parent validation).
    Allow,
}

/// Result of a JSONL import.
#[derive(Debug, Clone, Default)]
pub struct ImportResult {
    /// Number of issues imported (created or updated).
    pub imported_count: usize,
    /// Number of issues created during import.
    pub created_count: usize,
    /// Number of issues updated during import.
    pub updated_count: usize,
    /// Number of issues skipped.
    pub skipped_count: usize,
    /// Number of tombstones skipped.
    pub tombstone_skipped: usize,
    /// Conflict markers detected (if any).
    pub conflict_markers: Vec<ConflictMarker>,
    /// Number of orphaned DB entries removed during --rebuild.
    pub orphans_removed: usize,
    /// Number of orphaned FK rows cleaned after deferred-FK import.
    pub orphan_cleaned_count: usize,
    /// Number of label rows imported from JSONL for applied issue records.
    pub labels_imported: usize,
    /// Number of dependency rows imported from JSONL for applied issue records.
    pub dependencies_imported: usize,
    /// Number of comment rows imported from JSONL for applied issue records.
    pub comments_imported: usize,
    /// Number of export-hash rows recorded for the imported JSONL snapshot.
    pub export_hashes_recorded: usize,
    /// Number of blocked-cache rows rebuilt after import.
    pub blocked_cache_entries: usize,
    /// Number of child-counter rows rebuilt after import.
    pub child_counter_entries: usize,
}

// ============================================================================
// PREFLIGHT CHECKS (beads_rust-0v1.2.7)
// ============================================================================

/// Status of a preflight check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreflightCheckStatus {
    /// Check passed.
    Pass,
    /// Check passed with warnings.
    Warn,
    /// Check failed.
    Fail,
}

/// A single preflight check result.
#[derive(Debug, Clone)]
pub struct PreflightCheck {
    /// Name of the check (e.g., "`path_validation`").
    pub name: String,
    /// Human-readable description of what was checked.
    pub description: String,
    /// Status of the check.
    pub status: PreflightCheckStatus,
    /// Detailed message (error/warning reason, or success confirmation).
    pub message: String,
    /// Actionable remediation hint (if status is Fail or Warn).
    pub remediation: Option<String>,
}

impl PreflightCheck {
    fn pass(
        name: impl Into<String>,
        description: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            status: PreflightCheckStatus::Pass,
            message: message.into(),
            remediation: None,
        }
    }

    fn warn(
        name: impl Into<String>,
        description: impl Into<String>,
        message: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            status: PreflightCheckStatus::Warn,
            message: message.into(),
            remediation: Some(remediation.into()),
        }
    }

    fn fail(
        name: impl Into<String>,
        description: impl Into<String>,
        message: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            status: PreflightCheckStatus::Fail,
            message: message.into(),
            remediation: Some(remediation.into()),
        }
    }
}

/// Result of running all preflight checks.
#[derive(Debug, Clone)]
pub struct PreflightResult {
    /// All checks that were run.
    pub checks: Vec<PreflightCheck>,
    /// Overall status (Fail if any check failed, Warn if any warned, Pass otherwise).
    pub overall_status: PreflightCheckStatus,
}

impl PreflightResult {
    const fn new() -> Self {
        Self {
            checks: Vec::new(),
            overall_status: PreflightCheckStatus::Pass,
        }
    }

    fn add(&mut self, check: PreflightCheck) {
        // Update overall status (Fail > Warn > Pass)
        match check.status {
            PreflightCheckStatus::Fail => self.overall_status = PreflightCheckStatus::Fail,
            PreflightCheckStatus::Warn if self.overall_status != PreflightCheckStatus::Fail => {
                self.overall_status = PreflightCheckStatus::Warn;
            }
            _ => {}
        }
        self.checks.push(check);
    }

    /// Returns true if all checks passed (no failures or warnings).
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.overall_status == PreflightCheckStatus::Pass
    }

    /// Returns true if there are no failures (warnings are acceptable).
    #[must_use]
    pub fn has_no_failures(&self) -> bool {
        self.overall_status != PreflightCheckStatus::Fail
    }

    /// Get all failed checks.
    #[must_use]
    pub fn failures(&self) -> Vec<&PreflightCheck> {
        self.checks
            .iter()
            .filter(|c| c.status == PreflightCheckStatus::Fail)
            .collect()
    }

    /// Get all warnings.
    #[must_use]
    pub fn warnings(&self) -> Vec<&PreflightCheck> {
        self.checks
            .iter()
            .filter(|c| c.status == PreflightCheckStatus::Warn)
            .collect()
    }

    /// Convert to an error if there are failures.
    ///
    /// # Errors
    ///
    /// Returns an error if there are failed checks.
    pub fn into_result(self) -> Result<Self> {
        if self.overall_status == PreflightCheckStatus::Fail {
            let mut msg = String::from("Preflight checks failed:\n");
            for check in self.failures() {
                use std::fmt::Write;
                let _ = writeln!(msg, "  - {}: {}", check.name, check.message);
                if let Some(ref rem) = check.remediation {
                    let _ = writeln!(msg, "    Hint: {rem}");
                }
            }
            Err(BeadsError::Config(msg))
        } else {
            Ok(self)
        }
    }
}

const JSONL_VALIDATION_PREVIEW_LIMIT: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JsonlIssueValidationFailure {
    pub line: usize,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct JsonlIssueValidationSummary {
    pub record_count: usize,
    pub invalid_count: usize,
    pub failures: Vec<JsonlIssueValidationFailure>,
}

impl JsonlIssueValidationSummary {
    fn push_failure(&mut self, line: usize, message: impl Into<String>) {
        self.invalid_count += 1;
        if self.failures.len() < JSONL_VALIDATION_PREVIEW_LIMIT {
            self.failures.push(JsonlIssueValidationFailure {
                line,
                message: message.into(),
            });
        }
    }

    pub(crate) fn preview_messages(&self) -> Vec<String> {
        self.failures
            .iter()
            .map(|failure| format!("line {}: {}", failure.line, failure.message))
            .collect()
    }
}

pub(crate) fn validate_jsonl_issue_records(path: &Path) -> Result<JsonlIssueValidationSummary> {
    let file = File::open(path)?;
    let reader = BufReader::with_capacity(2 * 1024 * 1024, file);
    let mut summary = JsonlIssueValidationSummary::default();
    let mut seen_ids = HashSet::new();

    for (line_num, line_result) in reader.lines().enumerate() {
        let line = line_result?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        summary.record_count += 1;
        match serde_json::from_str::<Issue>(trimmed) {
            Ok(mut issue) => {
                normalize_issue(&mut issue);
                if !seen_ids.insert(issue.id.clone()) {
                    summary
                        .push_failure(line_num + 1, format!("Duplicate issue id '{}'", issue.id));
                    continue;
                }
                if let Err(errors) = IssueValidator::validate(&issue) {
                    summary.push_failure(
                        line_num + 1,
                        errors
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(", "),
                    );
                }
            }
            Err(err) => summary.push_failure(line_num + 1, err.to_string()),
        }
    }

    Ok(summary)
}

/// Run preflight checks for export operation.
///
/// This function is read-only and validates:
/// - Beads directory exists
/// - Output path is within allowlist (not in .git, within `beads_dir`)
/// - Database is accessible
/// - Export won't cause data loss (empty db over non-empty JSONL, stale db)
///
/// # Arguments
///
/// * `storage` - Database connection for validation
/// * `output_path` - Target JSONL path
/// * `config` - Export configuration
///
/// # Returns
///
/// `PreflightResult` with all check results. Use `.into_result()` to convert
/// failures to an error.
///
/// # Errors
///
/// Returns an error if the preflight checks fail.
#[allow(clippy::too_many_lines)]
pub fn preflight_export(
    storage: &SqliteStorage,
    output_path: &Path,
    config: &ExportConfig,
) -> Result<PreflightResult> {
    let mut result = PreflightResult::new();

    tracing::debug!(
        output_path = %output_path.display(),
        beads_dir = ?config.beads_dir,
        "Running export preflight checks"
    );

    // Check 1: Beads directory exists
    if let Some(ref beads_dir) = config.beads_dir {
        if beads_dir.is_dir() {
            result.add(PreflightCheck::pass(
                "beads_dir_exists",
                "Beads directory exists",
                format!("Found: {}", beads_dir.display()),
            ));
            tracing::debug!(beads_dir = %beads_dir.display(), "Beads directory check: PASS");
        } else {
            result.add(PreflightCheck::fail(
                "beads_dir_exists",
                "Beads directory exists",
                format!("Not found: {}", beads_dir.display()),
                "Run 'br init' to initialize the beads directory.",
            ));
            tracing::debug!(beads_dir = %beads_dir.display(), "Beads directory check: FAIL");
        }
    }

    // Check 2: Output path validation (PC-1, PC-2, PC-3, NGI-3)
    if let Some(ref beads_dir) = config.beads_dir {
        // Determine if the path is external (outside .beads/)
        let canonical_beads = dunce::canonicalize(beads_dir).unwrap_or_else(|_| beads_dir.clone());
        let is_external =
            !output_path.starts_with(beads_dir) && !output_path.starts_with(&canonical_beads);

        match validate_sync_path_with_external(output_path, beads_dir, config.allow_external_jsonl)
        {
            Ok(()) => {
                let msg = format!(
                    "Path {} validated (external={})",
                    output_path.display(),
                    is_external
                );
                if is_external && config.allow_external_jsonl {
                    result.add(PreflightCheck::warn(
                        "path_validation",
                        "Output path is within allowlist",
                        msg,
                        "Consider moving JSONL to .beads/ directory for better safety.",
                    ));
                } else {
                    result.add(PreflightCheck::pass(
                        "path_validation",
                        "Output path is within allowlist",
                        msg,
                    ));
                }
                tracing::debug!(path = %output_path.display(), is_external = is_external, "Path validation: PASS");
            }
            Err(e) => {
                result.add(PreflightCheck::fail(
                    "path_validation",
                    "Output path is within allowlist",
                    format!("Path rejected: {e}"),
                    "Use a path within .beads/ directory or set --allow-external-jsonl.",
                ));
                tracing::debug!(path = %output_path.display(), error = %e, "Path validation: FAIL");
            }
        }
    }

    // Check 3: Database is accessible
    match storage.count_issues() {
        Ok(count) => {
            result.add(PreflightCheck::pass(
                "database_accessible",
                "Database is accessible",
                format!("Database contains {count} issue(s)"),
            ));
            tracing::debug!(issue_count = count, "Database access check: PASS");

            // Check 4: Empty database safety (would overwrite non-empty JSONL)
            if count == 0 && !config.force && output_path.exists() {
                match count_issues_in_jsonl(output_path) {
                    Ok(jsonl_count) if jsonl_count > 0 => {
                        result.add(PreflightCheck::fail(
                            "empty_database_safety",
                            "Export won't cause data loss",
                            format!(
                                "Database has 0 issues, JSONL has {jsonl_count} issues. Export would cause data loss.",
                            ),
                            "Import the JSONL first, or use --force to override.",
                        ));
                        tracing::debug!(
                            db_count = 0,
                            jsonl_count = jsonl_count,
                            "Empty database safety check: FAIL"
                        );
                    }
                    Ok(_) => {
                        result.add(PreflightCheck::pass(
                            "empty_database_safety",
                            "Export won't cause data loss",
                            "Database is empty, no existing JSONL to overwrite.",
                        ));
                    }
                    Err(e) => {
                        result.add(PreflightCheck::warn(
                            "empty_database_safety",
                            "Export won't cause data loss",
                            format!("Could not read existing JSONL: {e}"),
                            "Verify JSONL file is readable.",
                        ));
                    }
                }
            } else if count == 0 && !config.force {
                result.add(PreflightCheck::pass(
                    "empty_database_safety",
                    "Export won't cause data loss",
                    "Database is empty, no existing JSONL to overwrite.",
                ));
            }

            // Check 5: Stale database safety (would lose issues from JSONL)
            if count > 0 && !config.force && output_path.exists() {
                match get_issue_ids_from_jsonl(output_path) {
                    Ok(jsonl_ids) if !jsonl_ids.is_empty() => {
                        let db_ids: HashSet<String> = storage.get_all_ids()?.into_iter().collect();
                        let missing: Vec<_> = jsonl_ids.difference(&db_ids).take(5).collect();
                        if missing.is_empty() {
                            result.add(PreflightCheck::pass(
                                "stale_database_safety",
                                "Export won't lose JSONL issues",
                                "All JSONL issues are present in database.",
                            ));
                        } else {
                            let total_missing = jsonl_ids.difference(&db_ids).count();
                            result.add(PreflightCheck::fail(
                                "stale_database_safety",
                                "Export won't lose JSONL issues",
                                format!(
                                    "Database is missing {total_missing} issue(s) from JSONL: {}{}",
                                    missing
                                        .iter()
                                        .map(|s| s.as_str())
                                        .collect::<Vec<_>>()
                                        .join(", "),
                                    if total_missing > 5 { " ..." } else { "" }
                                ),
                                "Import the JSONL first to sync, or use --force to override.",
                            ));
                            tracing::debug!(
                                missing_count = total_missing,
                                sample = ?missing,
                                "Stale database safety check: FAIL"
                            );
                        }
                    }
                    Ok(_) => {
                        result.add(PreflightCheck::pass(
                            "stale_database_safety",
                            "Export won't lose JSONL issues",
                            "JSONL is empty or doesn't exist.",
                        ));
                    }
                    Err(e) => {
                        result.add(PreflightCheck::warn(
                            "stale_database_safety",
                            "Export won't lose JSONL issues",
                            format!("Could not read existing JSONL: {e}"),
                            "Verify JSONL file is readable.",
                        ));
                    }
                }
            }
        }
        Err(e) => {
            result.add(PreflightCheck::fail(
                "database_accessible",
                "Database is accessible",
                format!("Database error: {e}"),
                "Check database file permissions and integrity.",
            ));
            tracing::debug!(error = %e, "Database access check: FAIL");
        }
    }

    tracing::debug!(
        overall_status = ?result.overall_status,
        check_count = result.checks.len(),
        failure_count = result.failures().len(),
        "Export preflight complete"
    );

    Ok(result)
}

/// Run preflight checks for import operation.
///
/// This function is read-only and validates:
/// - Beads directory exists
/// - Input path is within allowlist (not in .git, within `beads_dir`)
/// - Input file exists and is readable
/// - No merge conflict markers in input file
/// - JSONL is parseable (basic syntax check)
/// - Issue ID prefixes match expected prefix (unless explicitly skipped)
///
/// # Arguments
///
/// * `input_path` - Source JSONL path
/// * `config` - Import configuration
/// * `expected_prefix` - Expected issue ID prefix (e.g., "bd") for mismatch guardrails
///
/// # Returns
///
/// `PreflightResult` with all check results. Use `.into_result()` to convert
/// failures to an error.
///
/// # Errors
///
/// Returns an error if the preflight checks fail.
#[allow(clippy::too_many_lines)]
pub fn preflight_import(
    input_path: &Path,
    config: &ImportConfig,
    expected_prefix: Option<&str>,
) -> Result<PreflightResult> {
    let mut result = PreflightResult::new();

    tracing::debug!(
        input_path = %input_path.display(),
        beads_dir = ?config.beads_dir,
        "Running import preflight checks"
    );

    // Check 1: Beads directory exists
    if let Some(ref beads_dir) = config.beads_dir {
        if beads_dir.is_dir() {
            result.add(PreflightCheck::pass(
                "beads_dir_exists",
                "Beads directory exists",
                format!("Found: {}", beads_dir.display()),
            ));
            tracing::debug!(beads_dir = %beads_dir.display(), "Beads directory check: PASS");
        } else {
            result.add(PreflightCheck::fail(
                "beads_dir_exists",
                "Beads directory exists",
                format!("Not found: {}", beads_dir.display()),
                "Run 'br init' to initialize the beads directory.",
            ));
            tracing::debug!(beads_dir = %beads_dir.display(), "Beads directory check: FAIL");
        }
    }

    // Check 2: Input path validation (PC-1, PC-2, PC-3, NGI-3)
    if let Some(ref beads_dir) = config.beads_dir {
        // Determine if the path is external (outside .beads/)
        let canonical_beads = dunce::canonicalize(beads_dir).unwrap_or_else(|_| beads_dir.clone());
        let is_external =
            !input_path.starts_with(beads_dir) && !input_path.starts_with(&canonical_beads);

        match validate_sync_path_with_external(input_path, beads_dir, config.allow_external_jsonl) {
            Ok(()) => {
                let msg = format!(
                    "Path {} validated (external={})",
                    input_path.display(),
                    is_external
                );
                if is_external && config.allow_external_jsonl {
                    result.add(PreflightCheck::warn(
                        "path_validation",
                        "Input path is within allowlist",
                        msg,
                        "Consider using JSONL from .beads/ directory for better safety.",
                    ));
                } else {
                    result.add(PreflightCheck::pass(
                        "path_validation",
                        "Input path is within allowlist",
                        msg,
                    ));
                }
                tracing::debug!(path = %input_path.display(), is_external = is_external, "Path validation: PASS");
            }
            Err(e) => {
                result.add(PreflightCheck::fail(
                    "path_validation",
                    "Input path is within allowlist",
                    format!("Path rejected: {e}"),
                    "Use a path within .beads/ directory or set --allow-external-jsonl.",
                ));
                tracing::debug!(path = %input_path.display(), error = %e, "Path validation: FAIL");
                return Ok(result);
            }
        }
    }

    // Check 3: Input file exists and is readable
    if input_path.exists() {
        match File::open(input_path) {
            Ok(_) => {
                result.add(PreflightCheck::pass(
                    "file_readable",
                    "Input file exists and is readable",
                    format!("File accessible: {}", input_path.display()),
                ));
                tracing::debug!(path = %input_path.display(), "File readable check: PASS");
            }
            Err(e) => {
                result.add(PreflightCheck::fail(
                    "file_readable",
                    "Input file exists and is readable",
                    format!("Cannot read file: {e}"),
                    "Check file permissions.",
                ));
                tracing::debug!(path = %input_path.display(), error = %e, "File readable check: FAIL");
            }
        }
    } else {
        result.add(PreflightCheck::fail(
            "file_readable",
            "Input file exists and is readable",
            format!("File not found: {}", input_path.display()),
            "Verify the path is correct or run export first.",
        ));
        tracing::debug!(path = %input_path.display(), "File readable check: FAIL (not found)");
        // Return early since we can't do further checks without the file
        return Ok(result);
    }

    // Check 4: No merge conflict markers
    match scan_conflict_markers(input_path) {
        Ok(markers) if markers.is_empty() => {
            result.add(PreflightCheck::pass(
                "no_conflict_markers",
                "No merge conflict markers",
                "File is clean of conflict markers.",
            ));
            tracing::debug!(path = %input_path.display(), "Conflict marker check: PASS");
        }
        Ok(markers) => {
            let preview: Vec<String> = markers
                .iter()
                .take(3)
                .map(|m| {
                    format!(
                        "line {}: {:?}{}",
                        m.line,
                        m.marker_type,
                        m.branch
                            .as_ref()
                            .map_or(String::new(), |b| format!(" ({b})"))
                    )
                })
                .collect();
            result.add(PreflightCheck::fail(
                "no_conflict_markers",
                "No merge conflict markers",
                format!(
                    "Found {} conflict marker(s): {}{}",
                    markers.len(),
                    preview.join("; "),
                    if markers.len() > 3 { " ..." } else { "" }
                ),
                "Resolve git merge conflicts before importing.",
            ));
            tracing::debug!(
                path = %input_path.display(),
                marker_count = markers.len(),
                "Conflict marker check: FAIL"
            );
        }
        Err(e) => {
            result.add(PreflightCheck::warn(
                "no_conflict_markers",
                "No merge conflict markers",
                format!("Could not scan for markers: {e}"),
                "Verify file is readable and not corrupted.",
            ));
            tracing::debug!(path = %input_path.display(), error = %e, "Conflict marker check: WARN");
        }
    }

    // Check 5: Per-line issue-record validation
    match validate_jsonl_issue_records(input_path) {
        Ok(summary) if summary.invalid_count == 0 => {
            result.add(PreflightCheck::pass(
                "json_valid",
                "All JSONL lines are valid issue records",
                format!("Validated {} issue record(s).", summary.record_count),
            ));
            tracing::debug!(path = %input_path.display(), record_count = summary.record_count, "JSONL issue validation check: PASS");
        }
        Ok(summary) => {
            let preview = summary.preview_messages();
            result.add(PreflightCheck::fail(
                "json_valid",
                "All JSONL lines are valid issue records",
                format!(
                    "Found {} invalid issue record(s): {}{}",
                    summary.invalid_count,
                    preview.join("; "),
                    if summary.invalid_count > preview.len() {
                        " ..."
                    } else {
                        ""
                    }
                ),
                "Fix or remove malformed issue records before importing.",
            ));
            tracing::debug!(
                path = %input_path.display(),
                invalid_count = summary.invalid_count,
                "JSONL issue validation check: FAIL"
            );
        }
        Err(err) => {
            result.add(PreflightCheck::warn(
                "json_valid",
                "All JSONL lines are valid issue records",
                format!("Could not open file for JSONL validation: {err}"),
                "Verify file is readable.",
            ));
        }
    }

    // Check 6: Prefix mismatch guard
    if !config.skip_prefix_validation
        && let Some(prefix) = expected_prefix
    {
        let file = File::open(input_path);
        match file {
            Ok(f) => {
                let reader = BufReader::new(f);
                let mut mismatched_ids: Vec<String> = Vec::new();
                for line_result in reader.lines() {
                    let Ok(line) = line_result else { continue };
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Ok(partial) = serde_json::from_str::<PartialId>(trimmed) {
                        // Skip tombstones — they may retain a foreign prefix legitimately
                        #[derive(Deserialize)]
                        struct StatusProbe {
                            status: Option<String>,
                        }
                        let is_tombstone = serde_json::from_str::<StatusProbe>(trimmed)
                            .ok()
                            .and_then(|p| p.status)
                            .is_some_and(|s| s == "tombstone");
                        if is_tombstone {
                            continue;
                        }
                        if !id_matches_expected_prefix(&partial.id, prefix) {
                            mismatched_ids.push(partial.id);
                        }
                    }
                }
                if mismatched_ids.is_empty() {
                    result.add(PreflightCheck::pass(
                        "prefix_match",
                        "Issue IDs match expected prefix",
                        format!("All issue IDs start with '{prefix}'."),
                    ));
                    tracing::debug!(prefix = prefix, "Prefix match check: PASS");
                } else {
                    let preview: Vec<String> = mismatched_ids.iter().take(5).cloned().collect();
                    result.add(PreflightCheck::fail(
                        "prefix_match",
                        "Issue IDs match expected prefix",
                        format!(
                            "Expected prefix '{}', found {} mismatched ID(s): {}{}",
                            prefix,
                            mismatched_ids.len(),
                            preview.join(", "),
                            if mismatched_ids.len() > 5 { " ..." } else { "" }
                        ),
                        "Use --force to skip prefix validation or --rename-prefix to remap IDs.",
                    ));
                    tracing::debug!(
                        prefix = prefix,
                        mismatch_count = mismatched_ids.len(),
                        "Prefix match check: FAIL"
                    );
                }
            }
            Err(e) => {
                result.add(PreflightCheck::warn(
                    "prefix_match",
                    "Issue IDs match expected prefix",
                    format!("Could not open file for prefix validation: {e}"),
                    "Verify file is readable.",
                ));
            }
        }
    }

    tracing::debug!(
        overall_status = ?result.overall_status,
        check_count = result.checks.len(),
        failure_count = result.failures().len(),
        "Import preflight complete"
    );

    Ok(result)
}

/// Conflict marker kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictMarkerType {
    Start,
    Separator,
    End,
}

/// A detected merge conflict marker within an import file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictMarker {
    pub path: PathBuf,
    pub line: usize,
    pub marker_type: ConflictMarkerType,
    pub branch: Option<String>,
}

const CONFLICT_START: &str = "<<<<<<<";
const CONFLICT_SEPARATOR: &str = "=======";
const CONFLICT_END: &str = ">>>>>>>";

/// Scan a file for merge conflict markers.
///
/// # Errors
///
/// Returns an error if the file cannot be read.
pub fn scan_conflict_markers(path: &Path) -> Result<Vec<ConflictMarker>> {
    let file = File::open(path)?;
    path::validate_jsonl_fd_metadata(&file, path)?;
    let reader = BufReader::with_capacity(2 * 1024 * 1024, file);
    let mut markers = Vec::new();

    for (line_num, line) in reader.lines().enumerate() {
        let line = line?;
        if let Some((marker_type, branch)) = detect_conflict_marker(&line) {
            markers.push(ConflictMarker {
                path: path.to_path_buf(),
                line: line_num + 1,
                marker_type,
                branch,
            });
        }
    }

    Ok(markers)
}

fn detect_conflict_marker(line: &str) -> Option<(ConflictMarkerType, Option<String>)> {
    if let Some(branch) = line.strip_prefix(CONFLICT_START) {
        return Some((ConflictMarkerType::Start, Some(branch.trim().to_string())));
    }
    if line.starts_with(CONFLICT_SEPARATOR) {
        return Some((ConflictMarkerType::Separator, None));
    }
    if let Some(branch) = line.strip_prefix(CONFLICT_END) {
        return Some((ConflictMarkerType::End, Some(branch.trim().to_string())));
    }
    None
}

/// Fail if a file contains merge conflict markers.
///
/// # Errors
///
/// Returns a config error describing the first few markers found.
pub fn ensure_no_conflict_markers(path: &Path) -> Result<()> {
    let markers = scan_conflict_markers(path)?;
    if markers.is_empty() {
        return Ok(());
    }

    let mut preview = String::new();
    for marker in markers.iter().take(5) {
        let _ = writeln!(
            preview,
            "{}:{} {:?}{}",
            marker.path.display(),
            marker.line,
            marker.marker_type,
            marker
                .branch
                .as_ref()
                .map_or(String::new(), |b| format!(" ({b})"))
        );
    }

    Err(BeadsError::Config(format!(
        "Merge conflict markers detected in {}.\n{}Resolve conflicts before importing.",
        path.display(),
        preview
    )))
}

#[derive(Deserialize)]
struct PartialId {
    id: String,
}

/// Analyze JSONL to get line count and unique issue IDs efficiently.
///
/// # Errors
///
/// Returns an error if the file cannot be read or contains invalid JSON.
pub fn analyze_jsonl(path: &Path) -> Result<(usize, HashSet<String>)> {
    let file = match File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((0, HashSet::new())),
        Err(e) => return Err(BeadsError::Io(e)),
    };
    path::validate_jsonl_fd_metadata(&file, path)?;

    let mut reader = BufReader::new(file);
    let mut count = 0;
    let mut ids = HashSet::new();
    let mut line_buf = String::new();
    let mut line_num = 0;

    loop {
        line_buf.clear();
        let bytes = reader.read_line(&mut line_buf)?;
        if bytes == 0 {
            break;
        }

        line_num += 1;
        let trimmed = line_buf.trim_end_matches(['\n', '\r']);
        if trimmed.trim().is_empty() {
            continue;
        }

        let partial: PartialId = serde_json::from_str(trimmed)
            .map_err(|e| BeadsError::Config(format!("Invalid JSON at line {}: {}", line_num, e)))?;

        if !ids.insert(partial.id.clone()) {
            return Err(BeadsError::Config(format!(
                "Duplicate issue id '{}' in {} at line {}",
                partial.id,
                path.display(),
                line_num
            )));
        }
        count += 1;
    }

    Ok((count, ids))
}

/// Count issues in an existing JSONL file.
///
/// # Errors
///
/// Returns an error if the file cannot be read or contains invalid JSON.
pub fn count_issues_in_jsonl(path: &Path) -> Result<usize> {
    Ok(analyze_jsonl(path)?.0)
}

fn verify_exported_jsonl_integrity(path: &Path, expected_ids: &[String]) -> Result<()> {
    let file = File::open(path)?;
    path::validate_jsonl_fd_metadata(&file, path)?;

    let expected: HashSet<&str> = expected_ids.iter().map(String::as_str).collect();
    let mut observed = HashSet::with_capacity(expected_ids.len());
    let mut reader = BufReader::new(file);
    let mut line_buf = String::new();
    let mut line_num = 0;
    let mut issue_count = 0;

    loop {
        line_buf.clear();
        let bytes = reader.read_line(&mut line_buf)?;
        if bytes == 0 {
            break;
        }

        line_num += 1;
        let trimmed = line_buf.trim_end_matches(['\n', '\r']);
        if trimmed.trim().is_empty() {
            continue;
        }

        let issue: Issue = serde_json::from_str(trimmed).map_err(|err| {
            BeadsError::Config(format!(
                "Export verification failed: invalid exported JSON at line {line_num}: {err}"
            ))
        })?;

        if issue.id.trim().is_empty() {
            return Err(BeadsError::Config(format!(
                "Export verification failed: empty issue id at line {line_num}"
            )));
        }

        if !expected.contains(issue.id.as_str()) {
            return Err(BeadsError::Config(format!(
                "Export verification failed: unexpected issue id '{}' at line {line_num}",
                issue.id
            )));
        }

        if !observed.insert(issue.id.clone()) {
            return Err(BeadsError::Config(format!(
                "Export verification failed: duplicate issue id '{}' at line {line_num}",
                issue.id
            )));
        }

        issue_count += 1;
    }

    if issue_count != expected_ids.len() {
        return Err(BeadsError::Config(format!(
            "Export verification failed: expected {} issues, JSONL has {} valid issue lines",
            expected_ids.len(),
            issue_count
        )));
    }

    if observed.len() != expected.len() {
        let mut missing = expected_ids
            .iter()
            .filter(|id| !observed.contains(id.as_str()))
            .cloned()
            .collect::<Vec<_>>();
        missing.sort();
        let preview = missing
            .iter()
            .take(10)
            .map(String::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        let more = if missing.len() > 10 {
            format!(" ... and {} more", missing.len() - 10)
        } else {
            String::new()
        };
        return Err(BeadsError::Config(format!(
            "Export verification failed: JSONL is missing expected issue id(s): {preview}{more}"
        )));
    }

    Ok(())
}

/// Get issue IDs from an existing JSONL file.
///
/// # Errors
///
/// Returns an error if the file cannot be read or contains invalid JSON.
pub fn get_issue_ids_from_jsonl(path: &Path) -> Result<HashSet<String>> {
    Ok(analyze_jsonl(path)?.1)
}

fn read_jsonl_lines_by_id(path: &Path) -> Result<BTreeMap<String, String>> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut lines_by_id = BTreeMap::new();
    let mut line_buf = String::new();
    let mut line_num = 0;

    loop {
        line_buf.clear();
        let bytes = reader.read_line(&mut line_buf)?;
        if bytes == 0 {
            break;
        }

        line_num += 1;
        let trimmed = line_buf.trim_end_matches(['\n', '\r']);
        if trimmed.trim().is_empty() {
            continue;
        }

        let partial: PartialId = serde_json::from_str(trimmed)
            .map_err(|e| BeadsError::Config(format!("Invalid JSON at line {}: {}", line_num, e)))?;

        if lines_by_id
            .insert(partial.id.clone(), trimmed.to_string())
            .is_some()
        {
            return Err(BeadsError::Config(format!(
                "Duplicate issue id '{}' in {} at line {}",
                partial.id,
                path.display(),
                line_num
            )));
        }
    }

    Ok(lines_by_id)
}

fn export_issue_ids(storage: &SqliteStorage) -> Result<Vec<String>> {
    let rows = storage.execute_raw_query(
        r"SELECT id
          FROM issues
          WHERE (ephemeral = 0 OR ephemeral IS NULL)
            AND id NOT LIKE '%-wisp-%'
          ORDER BY id ASC",
    )?;

    Ok(rows
        .iter()
        .filter_map(|row| row.first().and_then(SqliteValue::as_text).map(String::from))
        .collect())
}

fn hydrate_export_issue_batch(
    storage: &SqliteStorage,
    ids: &[String],
    ctx: &mut ExportContext,
) -> Result<Vec<Issue>> {
    let mut issues = storage.get_issues_by_ids(ids)?;
    issues.sort_unstable_by(|left, right| left.id.cmp(&right.id));

    let deps_map = match storage.get_dependencies_full_for_issues(ids) {
        Ok(map) => Some(map),
        Err(err) => {
            ctx.handle_error(ExportError::new(
                ExportEntityType::Dependency,
                "batch",
                err.to_string(),
            ))?;
            None
        }
    };
    let labels_map = match storage.get_labels_for_issues(ids) {
        Ok(map) => Some(map),
        Err(err) => {
            ctx.handle_error(ExportError::new(
                ExportEntityType::Label,
                "batch",
                err.to_string(),
            ))?;
            None
        }
    };
    let comments_map = match storage.get_comments_for_issues(ids) {
        Ok(map) => Some(map),
        Err(err) => {
            ctx.handle_error(ExportError::new(
                ExportEntityType::Comment,
                "batch",
                err.to_string(),
            ))?;
            None
        }
    };

    populate_export_issue_relations(
        storage,
        &mut issues,
        deps_map.as_ref(),
        labels_map.as_ref(),
        comments_map.as_ref(),
        ctx,
    );

    Ok(issues)
}

fn hydrate_export_issues_full_scan(
    storage: &SqliteStorage,
    ctx: &mut ExportContext,
) -> Result<Vec<Issue>> {
    let mut issues = storage.get_all_issues_for_export()?;

    let deps_map = match storage.get_dependency_records_for_export() {
        Ok(map) => Some(map),
        Err(err) => {
            ctx.handle_error(ExportError::new(
                ExportEntityType::Dependency,
                "export",
                err.to_string(),
            ))?;
            None
        }
    };
    let labels_map = match storage.get_labels_for_export() {
        Ok(map) => Some(map),
        Err(err) => {
            ctx.handle_error(ExportError::new(
                ExportEntityType::Label,
                "export",
                err.to_string(),
            ))?;
            None
        }
    };
    let comments_map = match storage.get_comments_for_export() {
        Ok(map) => Some(map),
        Err(err) => {
            ctx.handle_error(ExportError::new(
                ExportEntityType::Comment,
                "export",
                err.to_string(),
            ))?;
            None
        }
    };

    populate_export_issue_relations(
        storage,
        &mut issues,
        deps_map.as_ref(),
        labels_map.as_ref(),
        comments_map.as_ref(),
        ctx,
    );

    Ok(issues)
}

fn populate_export_issue_relations(
    storage: &SqliteStorage,
    issues: &mut [Issue],
    deps_map: Option<&HashMap<String, Vec<Dependency>>>,
    labels_map: Option<&HashMap<String, Vec<String>>>,
    comments_map: Option<&HashMap<String, Vec<Comment>>>,
    ctx: &ExportContext,
) {
    for issue in issues {
        if let Some(map) = deps_map {
            if let Some(deps) = map.get(&issue.id) {
                issue.dependencies.clone_from(deps);
            }
        } else if ctx.policy != ExportErrorPolicy::RequiredCore
            && let Ok(deps) = storage.get_dependencies_full(&issue.id)
        {
            issue.dependencies = deps;
        }

        if let Some(map) = labels_map {
            if let Some(labels) = map.get(&issue.id) {
                issue.labels.clone_from(labels);
            }
        } else if ctx.policy != ExportErrorPolicy::RequiredCore
            && let Ok(labels) = storage.get_labels(&issue.id)
        {
            issue.labels = labels;
        }

        if let Some(map) = comments_map {
            if let Some(comments) = map.get(&issue.id) {
                issue.comments.clone_from(comments);
            }
        } else if ctx.policy != ExportErrorPolicy::RequiredCore
            && let Ok(comments) = storage.get_comments(&issue.id)
        {
            issue.comments = comments;
        }

        normalize_issue_for_export(issue);
    }
}

fn write_export_issue_jsonl<W: Write>(
    writer: &mut W,
    issue: &Issue,
    hasher: &mut Sha256,
    buffer: &mut Vec<u8>,
    ctx: &mut ExportContext,
) -> Result<bool> {
    buffer.clear();
    if let Err(err) = serde_json::to_writer(&mut *buffer, issue) {
        ctx.handle_error(ExportError::new(
            ExportEntityType::Issue,
            issue.id.clone(),
            err.to_string(),
        ))?;
        return Ok(false);
    }

    if let Err(err) = writer
        .write_all(buffer)
        .and_then(|()| writer.write_all(b"\n"))
    {
        ctx.handle_error(ExportError::new(
            ExportEntityType::Issue,
            issue.id.clone(),
            err.to_string(),
        ))?;
        return Ok(false);
    }

    hasher.update(&*buffer);
    hasher.update(b"\n");

    Ok(true)
}

struct PreparedExportIssue {
    id: String,
    jsonl_line: Vec<u8>,
    content_hash: String,
    dependency_count: usize,
    label_count: usize,
    comment_count: usize,
}

enum PreparedExportEntry {
    Issue(PreparedExportIssue),
    SkippedTombstone(String),
    Error(ExportError),
}

fn effective_export_parallelism(config: &ExportConfig) -> usize {
    if config.max_parallel_workers == 1 || export_parallelism_disabled_by_env() {
        return 1;
    }

    let host_parallelism = thread::available_parallelism()
        .map_or(DEFAULT_JSONL_EXPORT_PARALLELISM, std::num::NonZero::get);
    let cap = if config.max_parallel_workers == 0 {
        DEFAULT_JSONL_EXPORT_PARALLELISM
    } else {
        config.max_parallel_workers
    };

    cap.min(host_parallelism).max(1)
}

fn export_parallelism_disabled_by_env() -> bool {
    std::env::var_os("BR_DISABLE_PARALLEL_JSONL_EXPORT").is_some_and(|value| {
        let value = value.to_string_lossy();
        value != "0" && !value.eq_ignore_ascii_case("false")
    })
}

const fn should_prepare_export_issues_parallel(issue_count: usize, max_parallelism: usize) -> bool {
    max_parallelism > 1 && issue_count >= EXPORT_PARALLEL_PREPARE_MIN_ISSUES
}

fn prepare_export_issue_jsonl(issue: &Issue, retention_days: Option<u64>) -> PreparedExportEntry {
    if issue.is_expired_tombstone(retention_days) {
        return PreparedExportEntry::SkippedTombstone(issue.id.clone());
    }

    let mut jsonl_line = Vec::with_capacity(1024);
    if let Err(err) = serde_json::to_writer(&mut jsonl_line, issue) {
        return PreparedExportEntry::Error(ExportError::new(
            ExportEntityType::Issue,
            issue.id.clone(),
            err.to_string(),
        ));
    }
    jsonl_line.push(b'\n');

    PreparedExportEntry::Issue(PreparedExportIssue {
        id: issue.id.clone(),
        jsonl_line,
        content_hash: issue
            .content_hash
            .clone()
            .unwrap_or_else(|| crate::util::content_hash(issue)),
        dependency_count: issue.dependencies.len(),
        label_count: issue.labels.len(),
        comment_count: issue.comments.len(),
    })
}

fn prepare_export_issue_chunk(
    issues: &[Issue],
    retention_days: Option<u64>,
) -> Vec<PreparedExportEntry> {
    issues
        .iter()
        .map(|issue| prepare_export_issue_jsonl(issue, retention_days))
        .collect()
}

fn prepare_export_issues_jsonl_parallel(
    issues: &[Issue],
    retention_days: Option<u64>,
    max_parallelism: usize,
) -> Result<Vec<PreparedExportEntry>> {
    if !should_prepare_export_issues_parallel(issues.len(), max_parallelism) {
        return Ok(prepare_export_issue_chunk(issues, retention_days));
    }

    let worker_count = max_parallelism.min(issues.len());
    let chunk_size = issues.len().div_ceil(worker_count);

    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(worker_count);
        for (chunk_index, chunk) in issues.chunks(chunk_size).enumerate() {
            let start_index = chunk_index * chunk_size;
            handles.push(scope.spawn(move || {
                (
                    start_index,
                    prepare_export_issue_chunk(chunk, retention_days),
                )
            }));
        }

        let mut chunks = Vec::with_capacity(handles.len());
        for handle in handles {
            let chunk = handle.join().map_err(|_| {
                BeadsError::Config("Parallel JSONL export worker panicked".to_string())
            })?;
            chunks.push(chunk);
        }

        chunks.sort_unstable_by_key(|(start_index, _)| *start_index);
        let total_entries = chunks.iter().map(|(_, entries)| entries.len()).sum();
        let mut entries = Vec::with_capacity(total_entries);
        for (_, chunk_entries) in chunks {
            entries.extend(chunk_entries);
        }

        Ok(entries)
    })
}

#[allow(clippy::too_many_arguments)]
fn write_prepared_export_entries<W: Write>(
    writer: &mut W,
    prepared_entries: Vec<PreparedExportEntry>,
    hasher: &mut Sha256,
    ctx: &mut ExportContext,
    report: &mut ExportReport,
    exported_ids: &mut Vec<String>,
    skipped_tombstone_ids: &mut Vec<String>,
    issue_hashes: &mut Vec<(String, String)>,
    progress: Option<&ProgressBar>,
) -> Result<()> {
    for entry in prepared_entries {
        match entry {
            PreparedExportEntry::Issue(prepared) => {
                if let Err(err) = writer.write_all(&prepared.jsonl_line) {
                    ctx.handle_error(ExportError::new(
                        ExportEntityType::Issue,
                        prepared.id,
                        err.to_string(),
                    ))?;
                    increment_progress(progress);
                    continue;
                }

                hasher.update(&prepared.jsonl_line);
                exported_ids.push(prepared.id.clone());
                issue_hashes.push((prepared.id, prepared.content_hash));
                report.issues_exported += 1;
                report.dependencies_exported += prepared.dependency_count;
                report.labels_exported += prepared.label_count;
                report.comments_exported += prepared.comment_count;
            }
            PreparedExportEntry::SkippedTombstone(id) => {
                skipped_tombstone_ids.push(id);
            }
            PreparedExportEntry::Error(err) => {
                ctx.handle_error(err)?;
            }
        }
        increment_progress(progress);
    }

    Ok(())
}

fn increment_progress(progress: Option<&ProgressBar>) {
    if let Some(progress) = progress {
        progress.inc(1);
    }
}

/// Export issues from `SQLite` to JSONL format.
///
/// This implements the classic beads export semantics:
/// - Include tombstones (for sync propagation)
/// - Exclude ephemerals/wisps
/// - Sort by ID for deterministic output
/// - Populate dependencies and labels for each issue
/// - Atomic write (temp file -> rename)
/// - Safety guard against empty DB overwriting non-empty JSONL
///
/// # Errors
///
/// Returns an error if:
/// - Database read fails
/// - Safety guard is violated (empty DB, non-empty JSONL, no force)
/// - File write fails
#[allow(clippy::too_many_lines)]
pub fn export_to_jsonl(
    storage: &SqliteStorage,
    output_path: &Path,
    config: &ExportConfig,
) -> Result<ExportResult> {
    let (result, _report) = export_to_jsonl_with_policy(storage, output_path, config)?;
    Ok(result)
}

/// Export issues with configurable error policy, returning a report.
///
/// # Errors
///
/// Returns an error if:
/// - Path validation fails (git path, outside `beads_dir` without opt-in)
/// - Database queries fail and the policy requires strict handling
/// - Safety guards are violated (empty/stale export without `force`)
/// - File I/O fails
#[allow(clippy::too_many_lines)]
pub fn export_to_jsonl_with_policy(
    storage: &SqliteStorage,
    output_path: &Path,
    config: &ExportConfig,
) -> Result<(ExportResult, ExportReport)> {
    // Path validation (PC-1, PC-2, PC-3, NGI-3)
    if let Some(ref beads_dir) = config.beads_dir {
        validate_sync_path_with_external(output_path, beads_dir, config.allow_external_jsonl)?;
        tracing::debug!(
            output_path = %output_path.display(),
            beads_dir = %beads_dir.display(),
            allow_external = config.allow_external_jsonl,
            "Export path validated"
        );

        // Perform backup before overwriting (if enabled and we have a beads_dir).
        // We backup any JSONL file that has been validated as safe for sync,
        // even if it's outside the .beads/ directory (e.g., in repo root).
        let output_abs = if output_path.is_absolute() {
            output_path.to_path_buf()
        } else if let Ok(cwd) = std::env::current_dir() {
            cwd.join(output_path)
        } else {
            output_path.to_path_buf()
        };

        history::backup_before_export(beads_dir, &config.history, &output_abs)?;
    }

    // Get sorted export IDs up front for safety checks and bounded batch hydration.
    let export_ids = export_issue_ids(storage)?;

    // Fetch dirty metadata for safe clearing later
    let dirty_metadata = storage.get_dirty_issue_metadata()?;

    // Safety checks
    if !config.force && output_path.exists() {
        let (jsonl_count, jsonl_ids) = analyze_jsonl(output_path)?;

        // Check 1: prevent exporting empty database over non-empty JSONL
        if export_ids.is_empty() && jsonl_count > 0 {
            return Err(BeadsError::Config(format!(
                "Refusing to export empty database over non-empty JSONL file.\n\
                 Database has 0 issues, JSONL has {jsonl_count} lines.\n\
                 This would result in data loss!\n\
                 Hint: Use --force to override this safety check."
            )));
        }

        // Check 2: prevent exporting stale database that would lose issues
        if !jsonl_ids.is_empty() {
            let db_ids: HashSet<String> = export_ids.iter().cloned().collect();
            let missing: Vec<_> = jsonl_ids.difference(&db_ids).collect();

            if !missing.is_empty() {
                let mut missing_list = missing.into_iter().cloned().collect::<Vec<_>>();
                missing_list.sort();
                let display_count = missing_list.len().min(10);
                let preview: Vec<_> = missing_list.iter().take(display_count).collect();
                let more = if missing_list.len() > 10 {
                    format!(" ... and {} more", missing_list.len() - 10)
                } else {
                    String::new()
                };

                return Err(BeadsError::Config(format!(
                    "Refusing to export stale database that would lose issues.\n\
                     Database has {} issues, JSONL has {} unique issues.\n\
                     Export would lose {} issue(s): {}{}\n\
                     Hint: Run import first, or use --force to override.",
                    export_ids.len(),
                    jsonl_ids.len(),
                    missing_list.len(),
                    preview
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    more
                )));
            }
        }
    }

    let mut ctx = ExportContext::new(config.error_policy);
    let mut report = ExportReport::new(config.error_policy);

    let progress = create_progress_bar(
        export_ids.len() as u64,
        "Exporting issues",
        config.show_progress,
    );

    // Write to temp file for atomic rename
    let parent_dir = output_path.parent().ok_or_else(|| {
        BeadsError::Config(format!("Invalid output path: {}", output_path.display()))
    })?;

    // Ensure parent directory exists
    fs::create_dir_all(parent_dir)?;

    let (temp_path, temp_file) = create_jsonl_temp_file(output_path, config)?;
    let mut temp_guard = TempFileGuard::new(temp_path.clone());
    set_restrictive_jsonl_permissions(&temp_path);
    let mut writer = BufWriter::new(temp_file);

    // Write JSONL and compute hash
    let mut hasher = Sha256::new();
    let mut exported_ids = Vec::with_capacity(export_ids.len());
    let mut skipped_tombstone_ids = Vec::new(); // Usually small
    let mut issue_hashes = Vec::with_capacity(export_ids.len());
    let mut buffer = Vec::with_capacity(1024);
    let max_parallelism = effective_export_parallelism(config);

    if export_ids.len() <= EXPORT_FULL_SCAN_ISSUE_THRESHOLD {
        let issues = hydrate_export_issues_full_scan(storage, &mut ctx)?;
        if should_prepare_export_issues_parallel(issues.len(), max_parallelism) {
            let prepared = prepare_export_issues_jsonl_parallel(
                &issues,
                config.retention_days,
                max_parallelism,
            )?;
            write_prepared_export_entries(
                &mut writer,
                prepared,
                &mut hasher,
                &mut ctx,
                &mut report,
                &mut exported_ids,
                &mut skipped_tombstone_ids,
                &mut issue_hashes,
                Some(&progress),
            )?;
        } else {
            for issue in &issues {
                // Skip expired tombstones
                if issue.is_expired_tombstone(config.retention_days) {
                    skipped_tombstone_ids.push(issue.id.clone());
                    progress.inc(1);
                    continue;
                }

                if !write_export_issue_jsonl(
                    &mut writer,
                    issue,
                    &mut hasher,
                    &mut buffer,
                    &mut ctx,
                )? {
                    progress.inc(1);
                    continue;
                }

                exported_ids.push(issue.id.clone());
                issue_hashes.push((
                    issue.id.clone(),
                    issue
                        .content_hash
                        .clone()
                        .unwrap_or_else(|| crate::util::content_hash(issue)),
                ));
                report.issues_exported += 1;
                report.dependencies_exported += issue.dependencies.len();
                report.labels_exported += issue.labels.len();
                report.comments_exported += issue.comments.len();
                progress.inc(1);
            }
        }
    } else {
        for id_batch in export_ids.chunks(EXPORT_ISSUE_BATCH_SIZE) {
            let issues = hydrate_export_issue_batch(storage, id_batch, &mut ctx)?;
            if should_prepare_export_issues_parallel(issues.len(), max_parallelism) {
                let prepared = prepare_export_issues_jsonl_parallel(
                    &issues,
                    config.retention_days,
                    max_parallelism,
                )?;
                write_prepared_export_entries(
                    &mut writer,
                    prepared,
                    &mut hasher,
                    &mut ctx,
                    &mut report,
                    &mut exported_ids,
                    &mut skipped_tombstone_ids,
                    &mut issue_hashes,
                    Some(&progress),
                )?;
            } else {
                for issue in &issues {
                    // Skip expired tombstones
                    if issue.is_expired_tombstone(config.retention_days) {
                        skipped_tombstone_ids.push(issue.id.clone());
                        progress.inc(1);
                        continue;
                    }

                    if !write_export_issue_jsonl(
                        &mut writer,
                        issue,
                        &mut hasher,
                        &mut buffer,
                        &mut ctx,
                    )? {
                        progress.inc(1);
                        continue;
                    }

                    exported_ids.push(issue.id.clone());
                    issue_hashes.push((
                        issue.id.clone(),
                        issue
                            .content_hash
                            .clone()
                            .unwrap_or_else(|| crate::util::content_hash(issue)),
                    ));
                    report.issues_exported += 1;
                    report.dependencies_exported += issue.dependencies.len();
                    report.labels_exported += issue.labels.len();
                    report.comments_exported += issue.comments.len();
                    progress.inc(1);
                }
            }
        }
    }

    progress.finish_with_message("Export complete");

    // Flush and sync
    writer.flush()?;
    writer
        .into_inner()
        .map_err(|e| BeadsError::Io(e.into_error()))?
        .sync_all()?;

    // Compute final hash
    let content_hash = hex_encode(&hasher.finalize());

    // Verify staged export integrity before replacing the live JSONL.
    verify_exported_jsonl_integrity(&temp_path, &exported_ids)?;

    if let Some(ref beads_dir) = config.beads_dir {
        require_safe_sync_overwrite_path(
            &temp_path,
            beads_dir,
            config.allow_external_jsonl,
            "rename temp file",
        )?;
        require_safe_sync_overwrite_path(
            output_path,
            beads_dir,
            config.allow_external_jsonl,
            "overwrite JSONL output",
        )?;
    }

    // Atomic rename plus parent-directory fsync for power-loss durability.
    crate::util::durable_rename(&temp_path, output_path)?;
    temp_guard.persist();

    let result = ExportResult {
        exported_count: exported_ids.len(),
        exported_marked_at: filter_dirty_metadata_for_export(
            &dirty_metadata,
            &exported_ids,
            &skipped_tombstone_ids,
        ),
        exported_ids,
        skipped_tombstone_ids,
        content_hash,
        output_path: Some(output_path.to_string_lossy().to_string()),
        issue_hashes,
    };

    report.errors = ctx.errors;

    Ok((result, report))
}

/// Export issues to a writer (e.g., stdout).
///
/// # Errors
///
/// Returns an error if serialization or writing fails.
pub fn export_to_writer<W: Write>(storage: &SqliteStorage, writer: &mut W) -> Result<ExportResult> {
    let (result, _report) =
        export_to_writer_with_policy(storage, writer, ExportErrorPolicy::Strict)?;
    Ok(result)
}

/// Export issues to a writer with configurable error policy.
///
/// # Errors
///
/// Returns an error if serialization or writing fails under a strict policy.
#[allow(clippy::too_many_lines)]
pub fn export_to_writer_with_policy<W: Write>(
    storage: &SqliteStorage,
    writer: &mut W,
    policy: ExportErrorPolicy,
) -> Result<(ExportResult, ExportReport)> {
    let export_ids = export_issue_ids(storage)?;

    let mut ctx = ExportContext::new(policy);
    let mut report = ExportReport::new(policy);

    let mut hasher = Sha256::new();
    let mut exported_ids = Vec::with_capacity(export_ids.len());
    let skipped_tombstone_ids = Vec::new();
    let mut issue_hashes = Vec::with_capacity(export_ids.len());
    let mut buffer = Vec::with_capacity(1024);

    if export_ids.len() <= EXPORT_FULL_SCAN_ISSUE_THRESHOLD {
        let issues = hydrate_export_issues_full_scan(storage, &mut ctx)?;
        for issue in &issues {
            if !write_export_issue_jsonl(writer, issue, &mut hasher, &mut buffer, &mut ctx)? {
                continue;
            }

            exported_ids.push(issue.id.clone());
            issue_hashes.push((
                issue.id.clone(),
                issue
                    .content_hash
                    .clone()
                    .unwrap_or_else(|| crate::util::content_hash(issue)),
            ));
            report.issues_exported += 1;
            report.dependencies_exported += issue.dependencies.len();
            report.labels_exported += issue.labels.len();
            report.comments_exported += issue.comments.len();
        }
    } else {
        for id_batch in export_ids.chunks(EXPORT_ISSUE_BATCH_SIZE) {
            let issues = hydrate_export_issue_batch(storage, id_batch, &mut ctx)?;
            for issue in &issues {
                if !write_export_issue_jsonl(writer, issue, &mut hasher, &mut buffer, &mut ctx)? {
                    continue;
                }

                exported_ids.push(issue.id.clone());
                issue_hashes.push((
                    issue.id.clone(),
                    issue
                        .content_hash
                        .clone()
                        .unwrap_or_else(|| crate::util::content_hash(issue)),
                ));
                report.issues_exported += 1;
                report.dependencies_exported += issue.dependencies.len();
                report.labels_exported += issue.labels.len();
                report.comments_exported += issue.comments.len();
            }
        }
    }

    let content_hash = hex_encode(&hasher.finalize());

    let result = ExportResult {
        exported_count: exported_ids.len(),
        exported_ids,
        exported_marked_at: Vec::new(),
        skipped_tombstone_ids,
        content_hash,
        output_path: None,
        issue_hashes,
    };

    report.errors = ctx.errors;

    Ok((result, report))
}

/// Metadata key for the JSONL content hash.
pub const METADATA_JSONL_CONTENT_HASH: &str = "jsonl_content_hash";
/// Metadata key for the exact observed JSONL mtime at the last successful sync.
pub const METADATA_JSONL_MTIME: &str = "jsonl_mtime";
/// Metadata key for the exact observed JSONL size at the last successful sync.
pub const METADATA_JSONL_SIZE: &str = "jsonl_size";
/// Metadata key for the last export time.
pub const METADATA_LAST_EXPORT_TIME: &str = "last_export_time";
/// Metadata key for the last import time.
pub const METADATA_LAST_IMPORT_TIME: &str = "last_import_time";

#[derive(Debug, Clone)]
struct JsonlWitness {
    mtime: std::time::SystemTime,
    mtime_witness: String,
    size: u64,
}

/// Result of a staleness check between JSONL and DB.
#[derive(Debug, Clone, Copy)]
pub struct StalenessCheck {
    pub dirty_count: usize,
    pub jsonl_exists: bool,
    pub jsonl_mtime: Option<std::time::SystemTime>,
    pub jsonl_newer: bool,
    pub db_newer: bool,
}

fn pending_export_state(
    storage: &SqliteStorage,
    jsonl_exists: bool,
) -> Result<(usize, bool, bool)> {
    let dirty_count = storage.get_dirty_issue_count()?;
    let needs_flush = storage.get_metadata("needs_flush")?.as_deref() == Some("true");
    let missing_jsonl_with_data = !jsonl_exists && storage.count_issues()? > 0;
    Ok((
        dirty_count,
        needs_flush,
        dirty_count > 0 || needs_flush || missing_jsonl_with_data,
    ))
}

/// Compute staleness based on JSONL mtime + content hash and DB dirty state.
///
/// Uses Lstat (`symlink_metadata`) for JSONL mtime to match classic bd behavior.
///
/// # Errors
///
/// Returns an error if reading dirty state, metadata, JSONL mtime, or hashing fails.
pub fn compute_staleness(storage: &SqliteStorage, jsonl_path: &Path) -> Result<StalenessCheck> {
    let (staleness, _) = compute_staleness_impl(storage, jsonl_path)?;
    Ok(staleness)
}

/// Compute staleness and opportunistically persist refreshed JSONL witnesses.
///
/// When the stored content hash still matches but the cached mtime/size witness
/// is stale or incomplete, this updates the metadata so later commands can skip
/// re-hashing an unchanged JSONL file.
///
/// # Errors
///
/// Returns an error if reading dirty state, metadata, JSONL metadata, or
/// hashing fails. Opportunistic witness refresh failures are logged and
/// ignored so startup freshness probes do not fail on metadata backfill races.
pub fn compute_staleness_refreshing_witnesses(
    storage: &mut SqliteStorage,
    jsonl_path: &Path,
) -> Result<StalenessCheck> {
    let (staleness, refresh_witness) = compute_staleness_impl(storage, jsonl_path)?;
    if let Some(observed) = refresh_witness {
        refresh_jsonl_witness_best_effort(storage, jsonl_path, &observed);
    }
    Ok(staleness)
}

/// Check whether auto-import needs to inspect the JSONL contents.
///
/// This is the read-command startup fast path: when JSONL is not newer, callers
/// do not need dirty-count or pending-flush state because no import can happen.
/// If JSONL may be newer, `auto_import_if_stale` recomputes the full staleness
/// record before deciding whether a local dirty DB should block import.
///
/// # Errors
///
/// Returns an error if reading JSONL metadata, stored witnesses, or hashing
/// fails. Opportunistic witness refresh failures are logged and ignored.
pub fn auto_import_probe_refreshing_witnesses(
    storage: &mut SqliteStorage,
    beads_dir: &Path,
    jsonl_path: &Path,
    allow_external_jsonl: bool,
) -> Result<bool> {
    if jsonl_path.exists() {
        validate_sync_path_with_external(jsonl_path, beads_dir, allow_external_jsonl)?;
    }
    let probe = compute_jsonl_newer_impl(storage, jsonl_path)?;
    if let Some(observed) = probe.refresh_witness {
        refresh_jsonl_witness_best_effort(storage, jsonl_path, &observed);
    }
    Ok(probe.jsonl_newer)
}

/// Check whether auto-import needs to inspect JSONL contents without mutating metadata.
///
/// This variant is intended for read-only startup probes. It deliberately skips
/// the opportunistic JSONL witness refresh that
/// [`auto_import_probe_refreshing_witnesses`] performs, so callers can use a
/// read-only SQLite handle and reopen writable storage only if import work is
/// actually needed.
///
/// # Errors
///
/// Returns an error if reading JSONL metadata, stored witnesses, or hashing
/// fails.
pub fn auto_import_probe(
    storage: &SqliteStorage,
    beads_dir: &Path,
    jsonl_path: &Path,
    allow_external_jsonl: bool,
) -> Result<bool> {
    if jsonl_path.exists() {
        validate_sync_path_with_external(jsonl_path, beads_dir, allow_external_jsonl)?;
    }
    compute_jsonl_newer_impl(storage, jsonl_path).map(|probe| probe.jsonl_newer)
}

fn compute_staleness_impl(
    storage: &SqliteStorage,
    jsonl_path: &Path,
) -> Result<(StalenessCheck, Option<JsonlWitness>)> {
    let jsonl_exists = jsonl_path.exists();
    let (dirty_count, _needs_flush, db_newer) = pending_export_state(storage, jsonl_exists)?;
    let probe = compute_jsonl_newer_impl(storage, jsonl_path)?;

    Ok((
        StalenessCheck {
            dirty_count,
            jsonl_exists: probe.jsonl_exists,
            jsonl_mtime: probe.jsonl_mtime,
            jsonl_newer: probe.jsonl_newer,
            db_newer,
        },
        probe.refresh_witness,
    ))
}

struct JsonlNewerProbe {
    jsonl_exists: bool,
    jsonl_mtime: Option<std::time::SystemTime>,
    jsonl_newer: bool,
    refresh_witness: Option<JsonlWitness>,
}

fn compute_jsonl_newer_impl(storage: &SqliteStorage, jsonl_path: &Path) -> Result<JsonlNewerProbe> {
    if !jsonl_path.exists() {
        return Ok(JsonlNewerProbe {
            jsonl_exists: false,
            jsonl_mtime: None,
            jsonl_newer: false,
            refresh_witness: None,
        });
    }

    let observed = observed_jsonl_witness(jsonl_path)?;
    let stored_mtime = storage.get_metadata(METADATA_JSONL_MTIME)?;
    let stored_size = storage.get_metadata(METADATA_JSONL_SIZE)?;
    let stored_hash = storage.get_metadata(METADATA_JSONL_CONTENT_HASH)?;
    let mut refresh_witness = None;

    if stored_mtime.as_deref() == Some(observed.mtime_witness.as_str()) {
        let stored_size_matches =
            stored_size.as_deref().and_then(parse_jsonl_size_witness) == Some(observed.size);
        let jsonl_newer = if stored_size_matches {
            stored_hash.is_none()
        } else {
            stored_hash.as_ref().is_none_or(|hash| {
                compute_jsonl_hash(jsonl_path).map_or(true, |current_hash| &current_hash != hash)
            })
        };

        if !jsonl_newer && stored_hash.is_some() && !stored_size_matches {
            refresh_witness = Some(observed.clone());
        }

        return Ok(JsonlNewerProbe {
            jsonl_exists: true,
            jsonl_mtime: Some(observed.mtime),
            jsonl_newer,
            refresh_witness,
        });
    }

    let last_import_time = storage.get_metadata(METADATA_LAST_IMPORT_TIME)?;
    let last_export_time = storage.get_metadata(METADATA_LAST_EXPORT_TIME)?;

    // Get the latest known sync time (either import or export)
    let mut latest_sync_ts: Option<chrono::DateTime<Utc>> = None;

    if let Some(import_time) = &last_import_time
        && let Ok(ts) = chrono::DateTime::parse_from_rfc3339(import_time)
    {
        latest_sync_ts = Some(ts.with_timezone(&Utc));
    }

    if let Some(export_time) = &last_export_time
        && let Ok(ts) = chrono::DateTime::parse_from_rfc3339(export_time)
    {
        let ts_utc = ts.with_timezone(&Utc);
        if latest_sync_ts.is_none_or(|latest| ts_utc > latest) {
            latest_sync_ts = Some(ts_utc);
        }
    }

    // JSONL is newer if it was modified after the latest sync.
    // If metadata is missing or invalid, assume JSONL is newer (safe default).
    let mtime_newer = latest_sync_ts.is_none_or(|sync_ts| {
        let sync_sys_time = std::time::SystemTime::from(sync_ts);
        observed.mtime > sync_sys_time
    });

    let jsonl_newer = if mtime_newer {
        stored_hash.as_ref().is_none_or(|stored_hash| {
            compute_jsonl_hash(jsonl_path).map_or(true, |current_hash| &current_hash != stored_hash)
        })
    } else {
        false
    };

    if !jsonl_newer && stored_hash.is_some() {
        let stored_size_matches =
            stored_size.as_deref().and_then(parse_jsonl_size_witness) == Some(observed.size);
        if stored_mtime.as_deref() != Some(observed.mtime_witness.as_str()) || !stored_size_matches
        {
            refresh_witness = Some(observed.clone());
        }
    }

    Ok(JsonlNewerProbe {
        jsonl_exists: true,
        jsonl_mtime: Some(observed.mtime),
        jsonl_newer,
        refresh_witness,
    })
}

#[cfg(test)]
fn observed_jsonl_mtime(jsonl_path: &Path) -> Result<(std::time::SystemTime, String)> {
    let observed = observed_jsonl_witness(jsonl_path)?;
    Ok((observed.mtime, observed.mtime_witness))
}

fn observed_jsonl_witness(jsonl_path: &Path) -> Result<JsonlWitness> {
    let metadata = fs::symlink_metadata(jsonl_path)?;
    let jsonl_mtime = metadata.modified()?;
    Ok(JsonlWitness {
        mtime: jsonl_mtime,
        mtime_witness: chrono::DateTime::<Utc>::from(jsonl_mtime).to_rfc3339(),
        size: metadata.len(),
    })
}

fn parse_jsonl_size_witness(value: &str) -> Option<u64> {
    value.parse().ok()
}

fn record_observed_jsonl_witness_in_tx(
    storage: &SqliteStorage,
    observed: &JsonlWitness,
) -> Result<()> {
    storage.set_metadata_in_tx(METADATA_JSONL_MTIME, &observed.mtime_witness)?;
    storage.set_metadata_in_tx(METADATA_JSONL_SIZE, &observed.size.to_string())
}

fn maybe_refresh_jsonl_witness(
    storage: &mut SqliteStorage,
    jsonl_path: &Path,
    observed: &JsonlWitness,
) -> Result<()> {
    let current = observed_jsonl_witness(jsonl_path)?;
    if current.mtime != observed.mtime || current.size != observed.size {
        return Ok(());
    }

    storage.with_write_transaction(|storage| record_observed_jsonl_witness_in_tx(storage, &current))
}

fn refresh_jsonl_witness_best_effort(
    storage: &mut SqliteStorage,
    jsonl_path: &Path,
    observed: &JsonlWitness,
) {
    if let Err(error) = maybe_refresh_jsonl_witness(storage, jsonl_path, observed) {
        tracing::debug!(
            path = %jsonl_path.display(),
            error = %error,
            "Skipping opportunistic JSONL witness refresh"
        );
    }
}

/// Result of an auto-import attempt.
#[derive(Debug, Default)]
pub struct AutoImportResult {
    /// Whether an import was attempted.
    pub attempted: bool,
    /// Number of issues imported (created or updated).
    pub imported_count: usize,
}

/// Auto-import JSONL if it is newer than the DB.
///
/// Honors `--no-auto-import` and `--allow-stale` behavior.
/// Both flags short-circuit before any staleness probe so startup can skip the
/// JSONL stat/hash path entirely when the caller explicitly opted out.
///
/// # Errors
///
/// Returns an error if staleness checks, metadata reads, or import steps fail.
pub fn auto_import_if_stale(
    storage: &mut SqliteStorage,
    beads_dir: &Path,
    jsonl_path: &Path,
    expected_prefix: Option<&str>,
    allow_external_jsonl: bool,
    allow_stale: bool,
    no_auto_import: bool,
) -> Result<AutoImportResult> {
    if allow_stale || no_auto_import {
        tracing::debug!(
            allow_stale,
            no_auto_import,
            "Skipping auto-import staleness probe due to startup override"
        );
        return Ok(AutoImportResult::default());
    }

    if jsonl_path.exists() {
        validate_sync_path_with_external(jsonl_path, beads_dir, allow_external_jsonl)?;
    }
    let staleness = compute_staleness_refreshing_witnesses(storage, jsonl_path)?;
    if !staleness.jsonl_newer {
        return Ok(AutoImportResult::default());
    }

    // When both JSONL and DB have changed, skip the auto-import with a
    // warning instead of failing the command.  This prevents spurious
    // SyncConflict errors when ≥3 concurrent `br` processes race: one
    // process flushes JSONL while another has pending local writes,
    // causing both `jsonl_newer` and `db_newer` to be true.
    //
    // Explicit `br sync` still detects this as a hard conflict so the
    // user can reconcile manually.
    if staleness.db_newer && !allow_stale {
        tracing::warn!(
            dirty_count = staleness.dirty_count,
            jsonl_mtime = ?staleness.jsonl_mtime,
            "Skipping auto-import: JSONL changed externally while {} local change(s) are pending. \
             Run `br sync` to reconcile.",
            staleness.dirty_count,
        );
        return Ok(AutoImportResult::default());
    }

    let import_config = ImportConfig {
        // The configured prefix is the default for new IDs, not a project-wide
        // invariant. Auto-import should preserve mixed-prefix workspaces.
        skip_prefix_validation: true,
        beads_dir: Some(beads_dir.to_path_buf()),
        allow_external_jsonl,
        show_progress: false,
        ..Default::default()
    };

    let result = import_from_jsonl(storage, jsonl_path, &import_config, expected_prefix)?;

    tracing::debug!(
        imported_count = result.imported_count,
        jsonl_path = %jsonl_path.display(),
        "Auto-import completed"
    );

    Ok(AutoImportResult {
        attempted: true,
        imported_count: result.imported_count,
    })
}

/// Finalize an export by updating metadata, clearing dirty flags, and recording export hashes.
///
/// This should be called after a successful export to the default JSONL path.
/// It performs the following updates:
/// - Clears dirty flags for the exported issue IDs
/// - Records export hashes for each exported issue (for incremental export)
/// - Updates `jsonl_content_hash` metadata with the export hash
/// - Updates `last_export_time` metadata with the current timestamp
///
/// # Errors
///
/// Returns an error if database updates fail.
pub fn finalize_export(
    storage: &mut SqliteStorage,
    result: &ExportResult,
    issue_hashes: Option<&[(String, String)]>,
    jsonl_path: &Path,
) -> Result<()> {
    use chrono::Utc;
    let observed_jsonl = observed_jsonl_witness(jsonl_path)?;
    let prior_content_hash = storage.get_metadata(METADATA_JSONL_CONTENT_HASH)?;
    let export_hashes_current =
        export_hashes_certified_current(result, prior_content_hash.as_deref());

    storage.with_write_transaction(|storage| -> Result<()> {
        // Clear dirty flags for exported issues (safe version with timestamp validation)
        if !result.exported_marked_at.is_empty() {
            storage.clear_dirty_issues(&result.exported_marked_at)?;
        }

        // Record export hashes for each exported issue. Keep unchanged rows
        // stable so full flushes do not rewrite the export_hashes table.
        if !export_hashes_current && let Some(hashes) = issue_hashes {
            storage.set_changed_export_hashes_in_tx(hashes)?;
        }

        // Update metadata
        storage.set_metadata_in_tx(METADATA_JSONL_CONTENT_HASH, &result.content_hash)?;
        storage.set_metadata_in_tx(METADATA_LAST_EXPORT_TIME, &Utc::now().to_rfc3339())?;
        record_observed_jsonl_witness_in_tx(storage, &observed_jsonl)?;

        // Keep the row stable and clear the flag in place so ordinary export
        // cycles avoid delete+insert churn on the metadata B-tree.
        storage.set_metadata_in_tx("needs_flush", "false")?;

        Ok(())
    })?;

    Ok(())
}

fn export_hashes_certified_current(
    result: &ExportResult,
    prior_content_hash: Option<&str>,
) -> bool {
    result.exported_marked_at.is_empty() && prior_content_hash == Some(result.content_hash.as_str())
}

fn normalize_issue_for_export(issue: &mut Issue) {
    if !issue.labels.is_empty() {
        issue.labels.sort_unstable();
        issue.labels.dedup();
    }

    if !issue.dependencies.is_empty() {
        issue.dependencies.sort_by(|left, right| {
            left.issue_id
                .cmp(&right.issue_id)
                .then_with(|| left.depends_on_id.cmp(&right.depends_on_id))
                .then_with(|| left.dep_type.as_str().cmp(right.dep_type.as_str()))
                .then_with(|| left.created_at.cmp(&right.created_at))
                .then_with(|| left.created_by.cmp(&right.created_by))
                .then_with(|| left.metadata.cmp(&right.metadata))
                .then_with(|| left.thread_id.cmp(&right.thread_id))
        });
    }

    if !issue.comments.is_empty() {
        issue.comments.sort_by(|left, right| {
            left.issue_id
                .cmp(&right.issue_id)
                .then_with(|| left.created_at.cmp(&right.created_at))
                .then_with(|| left.author.cmp(&right.author))
                .then_with(|| left.body.cmp(&right.body))
                .then_with(|| left.id.cmp(&right.id))
        });
    }
}

fn filter_dirty_metadata_for_export(
    dirty_metadata: &[(String, String)],
    exported_ids: &[String],
    skipped_tombstone_ids: &[String],
) -> Vec<(String, String)> {
    let dirty_by_id: HashMap<&str, &str> = dirty_metadata
        .iter()
        .map(|(issue_id, marked_at)| (issue_id.as_str(), marked_at.as_str()))
        .collect();

    exported_ids
        .iter()
        .chain(skipped_tombstone_ids.iter())
        .filter_map(|issue_id| {
            dirty_by_id
                .get(issue_id.as_str())
                .map(|marked_at| (issue_id.clone(), (*marked_at).to_string()))
        })
        .collect()
}

fn restore_foreign_keys_after_import(
    storage: &SqliteStorage,
    validate_integrity: bool,
) -> Result<()> {
    storage
        .execute_raw("PRAGMA foreign_keys = ON")
        .map_err(|source| BeadsError::WithContext {
            context: "Failed to re-enable foreign key enforcement after import".to_string(),
            source: Box::new(source),
        })?;

    let foreign_keys_enabled = storage
        .execute_raw_query("PRAGMA foreign_keys")
        .map_err(|source| BeadsError::WithContext {
            context: "Failed to verify foreign key enforcement state after import".to_string(),
            source: Box::new(source),
        })?
        .first()
        .and_then(|row| row.first())
        .and_then(SqliteValue::as_integer)
        .unwrap_or(0);

    if foreign_keys_enabled != 1 {
        return Err(BeadsError::internal(
            "Import completed with foreign key enforcement still disabled",
        ));
    }

    if !validate_integrity {
        return Ok(());
    }

    if let Some((table, column)) = find_post_import_fk_violation(storage)? {
        return Err(BeadsError::validation(
            "jsonl import",
            format!("orphaned rows in {table}.{column}"),
        ));
    }

    Ok(())
}

fn finish_import_after_foreign_key_restore(
    apply_result: Result<ImportResult>,
    fk_restore_result: Result<()>,
) -> Result<ImportResult> {
    match (apply_result, fk_restore_result) {
        (Ok(import_result), Ok(())) => Ok(import_result),
        (Ok(_), Err(fk_err)) => Err(fk_err),
        (Err(import_err), Ok(())) => Err(import_err),
        (Err(import_err), Err(fk_err)) => {
            tracing::error!(
                error = %fk_err,
                "Failed to restore foreign key enforcement after failed import"
            );
            Err(BeadsError::WithContext {
                context: format!(
                    "jsonl import failed, and SQLite foreign key enforcement could not be re-enabled: {fk_err}"
                ),
                source: Box::new(import_err),
            })
        }
    }
}

fn find_post_import_fk_violation(storage: &SqliteStorage) -> Result<Option<(String, String)>> {
    let fk_backed_tables = [
        ("dependencies", "issue_id"),
        ("labels", "issue_id"),
        ("comments", "issue_id"),
        ("events", "issue_id"),
        ("dirty_issues", "issue_id"),
        ("export_hashes", "issue_id"),
        ("blocked_issues_cache", "issue_id"),
        ("child_counters", "parent_id"),
    ];

    for (table, column) in fk_backed_tables {
        let has_orphan = storage
            .has_missing_issue_reference(table, column)
            .map_err(|source| BeadsError::WithContext {
                context: format!(
                    "Failed to verify import integrity for foreign-key-backed table {table}.{column}"
                ),
                source: Box::new(source),
            })?;

        if has_orphan {
            return Ok(Some((table.to_string(), column.to_string())));
        }
    }

    Ok(None)
}

fn is_issue_exportable(issue: &Issue, retention_days: Option<u64>) -> bool {
    !issue.ephemeral && !issue.id.contains("-wisp-") && !issue.is_expired_tombstone(retention_days)
}

fn finalize_incremental_auto_flush(
    storage: &mut SqliteStorage,
    clear_dirty_metadata: &[(String, String)],
    removed_hash_ids: &[String],
    issue_hashes: &[(String, String)],
    content_hash: Option<&str>,
    jsonl_path: Option<&Path>,
) -> Result<()> {
    use chrono::Utc;
    let export_metadata = match content_hash {
        Some(content_hash) => {
            let jsonl_path = jsonl_path.ok_or_else(|| {
                BeadsError::Config(
                    "incremental auto-flush metadata update requires a JSONL path".to_string(),
                )
            })?;
            Some((content_hash, observed_jsonl_witness(jsonl_path)?))
        }
        None => None,
    };

    storage.with_write_transaction(|storage| -> Result<()> {
        if !clear_dirty_metadata.is_empty() {
            storage.clear_dirty_issues(clear_dirty_metadata)?;
        }
        if !removed_hash_ids.is_empty() {
            storage.clear_export_hashes_in_tx(removed_hash_ids)?;
        }
        if !issue_hashes.is_empty() {
            storage.set_changed_export_hashes_in_tx(issue_hashes)?;
        }
        if let Some((content_hash, observed_jsonl)) = &export_metadata {
            storage.set_metadata_in_tx(METADATA_JSONL_CONTENT_HASH, content_hash)?;
            storage.set_metadata_in_tx(METADATA_LAST_EXPORT_TIME, &Utc::now().to_rfc3339())?;
            record_observed_jsonl_witness_in_tx(storage, observed_jsonl)?;
        }
        storage.set_metadata_in_tx("needs_flush", "false")?;
        Ok(())
    })?;

    Ok(())
}

struct ExistingJsonlReplacementScan {
    exported_count: usize,
    changed: bool,
    all_replacements_seen: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum ExistingJsonlReplacementWrite {
    Unchanged {
        exported_count: usize,
    },
    Written {
        content_hash: String,
        exported_count: usize,
    },
}

struct JsonlTempOutput {
    temp_path: PathBuf,
    temp_guard: TempFileGuard,
    writer: BufWriter<File>,
}

fn scan_existing_jsonl_replacements(
    path: &Path,
    replacement_lines: &HashMap<String, String>,
) -> Result<ExistingJsonlReplacementScan> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut seen_ids = HashSet::new();
    let mut seen_replacements = HashSet::with_capacity(replacement_lines.len());
    let mut line_buf = String::new();
    let mut line_num = 0;
    let mut exported_count = 0;
    let mut changed = false;

    loop {
        line_buf.clear();
        let bytes = reader.read_line(&mut line_buf)?;
        if bytes == 0 {
            break;
        }

        line_num += 1;
        let trimmed = line_buf.trim_end_matches(['\n', '\r']);
        if trimmed.trim().is_empty() {
            continue;
        }

        let partial: PartialId = serde_json::from_str(trimmed)
            .map_err(|e| BeadsError::Config(format!("Invalid JSON at line {}: {}", line_num, e)))?;

        if !seen_ids.insert(partial.id.clone()) {
            return Err(BeadsError::Config(format!(
                "Duplicate issue id '{}' in {} at line {}",
                partial.id,
                path.display(),
                line_num
            )));
        }

        if let Some(replacement) = replacement_lines.get(&partial.id) {
            seen_replacements.insert(partial.id);
            changed |= replacement != trimmed;
        }

        exported_count += 1;
    }

    Ok(ExistingJsonlReplacementScan {
        exported_count,
        changed,
        all_replacements_seen: seen_replacements.len() == replacement_lines.len(),
    })
}

fn prepare_jsonl_temp_output(output_path: &Path, config: &ExportConfig) -> Result<JsonlTempOutput> {
    if let Some(ref beads_dir) = config.beads_dir {
        validate_sync_path_with_external(output_path, beads_dir, config.allow_external_jsonl)?;
        let output_abs = absolute_or_current_dir_join(output_path);
        history::backup_before_export(beads_dir, &config.history, &output_abs)?;
    }

    let parent_dir = output_path.parent().ok_or_else(|| {
        BeadsError::Config(format!("Invalid output path: {}", output_path.display()))
    })?;
    fs::create_dir_all(parent_dir)?;

    let (temp_path, temp_file) = create_jsonl_temp_file(output_path, config)?;
    let temp_guard = TempFileGuard::new(temp_path.clone());
    set_restrictive_jsonl_permissions(&temp_path);

    Ok(JsonlTempOutput {
        temp_path,
        temp_guard,
        writer: BufWriter::new(temp_file),
    })
}

fn absolute_or_current_dir_join(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.join(path)
    } else {
        path.to_path_buf()
    }
}

fn rename_jsonl_temp_output(
    temp_path: &Path,
    mut temp_guard: TempFileGuard,
    output_path: &Path,
    config: &ExportConfig,
) -> Result<()> {
    if let Some(ref beads_dir) = config.beads_dir {
        require_safe_sync_overwrite_path(
            temp_path,
            beads_dir,
            config.allow_external_jsonl,
            "rename temp file",
        )?;
        require_safe_sync_overwrite_path(
            output_path,
            beads_dir,
            config.allow_external_jsonl,
            "overwrite JSONL output",
        )?;
    }

    crate::util::durable_rename(temp_path, output_path)?;
    temp_guard.persist();
    Ok(())
}

fn sync_jsonl_writer(mut writer: BufWriter<File>) -> Result<()> {
    writer.flush()?;
    writer
        .into_inner()
        .map_err(|e| BeadsError::Io(e.into_error()))?
        .sync_all()?;
    Ok(())
}

fn try_write_existing_jsonl_replacements_atomically(
    replacement_lines: &HashMap<String, String>,
    output_path: &Path,
    config: &ExportConfig,
) -> Result<ExistingJsonlReplacementWrite> {
    let scan = scan_existing_jsonl_replacements(output_path, replacement_lines)?;

    if !scan.changed && scan.all_replacements_seen {
        return Ok(ExistingJsonlReplacementWrite::Unchanged {
            exported_count: scan.exported_count,
        });
    }

    let (content_hash, exported_count) =
        write_existing_jsonl_replacements_atomically(replacement_lines, output_path, config)?;
    Ok(ExistingJsonlReplacementWrite::Written {
        content_hash,
        exported_count,
    })
}

fn write_existing_jsonl_replacements_atomically(
    replacement_lines: &HashMap<String, String>,
    output_path: &Path,
    config: &ExportConfig,
) -> Result<(String, usize)> {
    let input_file = File::open(output_path)?;
    let mut reader = BufReader::new(input_file);
    let mut temp_output = prepare_jsonl_temp_output(output_path, config)?;
    let mut hasher = Sha256::new();
    let mut seen_ids = HashSet::new();
    let mut replaced_ids = HashSet::with_capacity(replacement_lines.len());
    let mut expected_ids = Vec::new();
    let mut line_buf = String::new();
    let mut line_num = 0;
    let mut exported_count = 0;

    loop {
        line_buf.clear();
        let bytes = reader.read_line(&mut line_buf)?;
        if bytes == 0 {
            break;
        }

        line_num += 1;
        let trimmed = line_buf.trim_end_matches(['\n', '\r']);
        if trimmed.trim().is_empty() {
            continue;
        }

        let partial: PartialId = serde_json::from_str(trimmed)
            .map_err(|e| BeadsError::Config(format!("Invalid JSON at line {}: {}", line_num, e)))?;

        if !seen_ids.insert(partial.id.clone()) {
            return Err(BeadsError::Config(format!(
                "Duplicate issue id '{}' in {} at line {}",
                partial.id,
                output_path.display(),
                line_num
            )));
        }

        let output_line = if let Some(replacement) = replacement_lines.get(&partial.id) {
            replaced_ids.insert(partial.id);
            replacement.as_str()
        } else {
            trimmed
        };

        writeln!(temp_output.writer, "{output_line}")?;
        hasher.update(output_line.as_bytes());
        hasher.update(b"\n");
        expected_ids.push(
            serde_json::from_str::<PartialId>(output_line)
                .map_err(|e| {
                    BeadsError::Config(format!(
                        "Invalid replacement JSON while preparing incremental auto-flush: {e}"
                    ))
                })?
                .id,
        );
        exported_count += 1;
    }

    let mut appended_ids = replacement_lines
        .keys()
        .filter(|id| !replaced_ids.contains(*id))
        .collect::<Vec<_>>();
    appended_ids.sort();

    for issue_id in appended_ids {
        let output_line = replacement_lines.get(issue_id).ok_or_else(|| {
            BeadsError::Config(format!(
                "Missing replacement JSON while preparing incremental auto-flush for {issue_id}"
            ))
        })?;
        writeln!(temp_output.writer, "{output_line}")?;
        hasher.update(output_line.as_bytes());
        hasher.update(b"\n");
        expected_ids.push(issue_id.clone());
        exported_count += 1;
    }

    let JsonlTempOutput {
        temp_path,
        temp_guard,
        writer,
    } = temp_output;

    sync_jsonl_writer(writer)?;
    verify_exported_jsonl_integrity(&temp_path, &expected_ids)?;
    rename_jsonl_temp_output(&temp_path, temp_guard, output_path, config)?;

    Ok((hex_encode(&hasher.finalize()), exported_count))
}

fn write_jsonl_lines_atomically(
    lines_by_id: &BTreeMap<String, String>,
    output_path: &Path,
    config: &ExportConfig,
) -> Result<String> {
    let mut temp_output = prepare_jsonl_temp_output(output_path, config)?;
    let mut hasher = Sha256::new();

    for line in lines_by_id.values() {
        writeln!(temp_output.writer, "{line}")?;
        hasher.update(line.as_bytes());
        hasher.update(b"\n");
    }

    let JsonlTempOutput {
        temp_path,
        temp_guard,
        writer,
    } = temp_output;

    sync_jsonl_writer(writer)?;
    let expected_ids = lines_by_id.keys().cloned().collect::<Vec<_>>();
    verify_exported_jsonl_integrity(&temp_path, &expected_ids)?;

    rename_jsonl_temp_output(&temp_path, temp_guard, output_path, config)?;

    Ok(hex_encode(&hasher.finalize()))
}

struct IncrementalAutoFlushChanges {
    dirty_metadata: Vec<(String, String)>,
    removed_hash_ids: Vec<String>,
    issue_hashes: Vec<(String, String)>,
    replacement_lines: HashMap<String, String>,
}

fn collect_incremental_auto_flush_changes(
    storage: &SqliteStorage,
    dirty_metadata: Vec<(String, String)>,
) -> Result<IncrementalAutoFlushChanges> {
    let dirty_len = dirty_metadata.len();
    let mut removed_hash_ids = Vec::with_capacity(dirty_len);
    let mut issue_hashes = Vec::with_capacity(dirty_len);
    let mut replacement_lines = HashMap::with_capacity(dirty_len);

    let dirty_ids: Vec<String> = dirty_metadata.iter().map(|(id, _)| id.clone()).collect();
    let batch_issues = storage.get_issues_for_export(&dirty_ids)?;
    let mut issues_by_id: HashMap<String, crate::model::Issue> = batch_issues
        .into_iter()
        .map(|issue| (issue.id.clone(), issue))
        .collect();

    for (issue_id, _) in &dirty_metadata {
        let maybe_issue = issues_by_id.remove(issue_id);
        match maybe_issue {
            Some(mut issue) if is_issue_exportable(&issue, None) => {
                normalize_issue_for_export(&mut issue);
                let json = serde_json::to_string(&issue).map_err(|err| {
                    BeadsError::Config(format!(
                        "Failed to serialize issue '{}' during auto-flush: {err}",
                        issue.id
                    ))
                })?;

                issue_hashes.push((
                    issue_id.clone(),
                    issue
                        .content_hash
                        .clone()
                        .unwrap_or_else(|| issue.compute_content_hash()),
                ));
                replacement_lines.insert(issue_id.clone(), json);
            }
            Some(_) | None => removed_hash_ids.push(issue_id.clone()),
        }
    }

    Ok(IncrementalAutoFlushChanges {
        dirty_metadata,
        removed_hash_ids,
        issue_hashes,
        replacement_lines,
    })
}

fn try_existing_line_auto_flush(
    storage: &mut SqliteStorage,
    jsonl_path: &Path,
    export_config: &ExportConfig,
    changes: &IncrementalAutoFlushChanges,
) -> Result<Option<AutoFlushResult>> {
    if !changes.removed_hash_ids.is_empty() || changes.replacement_lines.is_empty() {
        return Ok(None);
    }

    let result = try_write_existing_jsonl_replacements_atomically(
        &changes.replacement_lines,
        jsonl_path,
        export_config,
    )?;

    match result {
        ExistingJsonlReplacementWrite::Unchanged { .. } => {
            finalize_incremental_auto_flush(
                storage,
                &changes.dirty_metadata,
                &changes.removed_hash_ids,
                &changes.issue_hashes,
                None,
                None,
            )?;
            Ok(Some(AutoFlushResult::default()))
        }
        ExistingJsonlReplacementWrite::Written {
            content_hash,
            exported_count,
        } => {
            finalize_incremental_auto_flush(
                storage,
                &changes.dirty_metadata,
                &changes.removed_hash_ids,
                &changes.issue_hashes,
                Some(&content_hash),
                Some(jsonl_path),
            )?;
            Ok(Some(AutoFlushResult {
                flushed: true,
                exported_count,
                content_hash,
            }))
        }
    }
}

fn apply_incremental_auto_flush_changes(
    lines_by_id: &mut BTreeMap<String, String>,
    changes: &IncrementalAutoFlushChanges,
) -> bool {
    let mut changed = false;
    for (issue_id, json) in &changes.replacement_lines {
        if lines_by_id.get(issue_id) != Some(json) {
            lines_by_id.insert(issue_id.clone(), json.clone());
            changed = true;
        }
    }
    for issue_id in &changes.removed_hash_ids {
        changed |= lines_by_id.remove(issue_id).is_some();
    }
    changed
}

fn try_incremental_auto_flush(
    storage: &mut SqliteStorage,
    beads_dir: &Path,
    jsonl_path: &Path,
    allow_external_jsonl: bool,
) -> Result<Option<AutoFlushResult>> {
    if !jsonl_path.exists() {
        return Ok(None);
    }

    let dirty_metadata = storage.get_dirty_issue_metadata()?;
    if dirty_metadata.is_empty() {
        return Ok(Some(AutoFlushResult::default()));
    }

    let changes = collect_incremental_auto_flush_changes(storage, dirty_metadata)?;
    let export_config = ExportConfig {
        force: false,
        beads_dir: Some(beads_dir.to_path_buf()),
        allow_external_jsonl,
        ..Default::default()
    };

    if let Some(result) =
        try_existing_line_auto_flush(storage, jsonl_path, &export_config, &changes)?
    {
        return Ok(Some(result));
    }

    let mut lines_by_id = read_jsonl_lines_by_id(jsonl_path)?;
    let changed = apply_incremental_auto_flush_changes(&mut lines_by_id, &changes);

    if !changed {
        finalize_incremental_auto_flush(
            storage,
            &changes.dirty_metadata,
            &changes.removed_hash_ids,
            &changes.issue_hashes,
            None,
            None,
        )?;
        return Ok(Some(AutoFlushResult::default()));
    }

    let content_hash = write_jsonl_lines_atomically(&lines_by_id, jsonl_path, &export_config)?;
    finalize_incremental_auto_flush(
        storage,
        &changes.dirty_metadata,
        &changes.removed_hash_ids,
        &changes.issue_hashes,
        Some(&content_hash),
        Some(jsonl_path),
    )?;

    Ok(Some(AutoFlushResult {
        flushed: true,
        exported_count: lines_by_id.len(),
        content_hash,
    }))
}

/// Result of an auto-flush operation.
#[derive(Debug, Default)]
pub struct AutoFlushResult {
    /// Whether the flush was performed (false if skipped due to no dirty issues).
    pub flushed: bool,
    /// Number of issues exported (0 if not flushed).
    pub exported_count: usize,
    /// Content hash of the exported JSONL (empty if not flushed).
    pub content_hash: String,
}

/// Perform an automatic flush of dirty issues to JSONL.
///
/// This is the auto-flush operation that runs at the end of mutating commands
/// (unless `--no-auto-flush` is set). It:
/// 1. Checks for dirty issues
/// 2. If any exist, exports them to the resolved JSONL path
/// 3. Clears dirty flags and updates metadata
///
/// Returns early (no-op) if there are no dirty issues.
///
/// # Arguments
///
/// * `storage` - Mutable reference to the `SQLite` storage
/// * `beads_dir` - Path to the .beads directory
/// * `jsonl_path` - Resolved JSONL export target for this workspace
///
/// # Errors
///
/// Returns an error if the export fails.
pub fn auto_flush(
    storage: &mut SqliteStorage,
    beads_dir: &Path,
    jsonl_path: &Path,
    allow_external_jsonl: bool,
) -> Result<AutoFlushResult> {
    // Check for dirty issues or forced flush first
    let jsonl_exists = jsonl_path.exists();
    let (dirty_count, needs_flush, db_newer) = pending_export_state(storage, jsonl_exists)?;

    if !db_newer {
        tracing::debug!("Auto-flush: no dirty issues, skipping");
        return Ok(AutoFlushResult::default());
    }

    validate_sync_path_with_external(jsonl_path, beads_dir, allow_external_jsonl)?;

    // Refuse to auto-flush over a JSONL that still holds unresolved
    // merge-conflict markers. The downstream export path would otherwise
    // silently overwrite the `<<<<<<<` / `=======` / `>>>>>>>` regions
    // (along with the remote side of the merge the operator hadn't yet
    // looked at) every time a mutating CLI command returns. Explicit
    // `br sync --flush-only` already has a `--force` escape hatch for this
    // case; auto-flush has no such surface, so the only safe default is to
    // stop, log clearly, and let the next explicit sync surface the error.
    if jsonl_exists {
        let conflict_markers = scan_conflict_markers(jsonl_path)?;
        if !conflict_markers.is_empty() {
            tracing::warn!(
                jsonl_path = %jsonl_path.display(),
                marker_count = conflict_markers.len(),
                "Skipping auto-flush: JSONL contains merge-conflict markers. Resolve them (or run `br sync --flush-only --force` to override) before the next write.",
            );
            return Ok(AutoFlushResult::default());
        }
    }

    tracing::debug!(
        dirty_count,
        needs_flush,
        "Auto-flush: exporting dirty issues"
    );

    if !needs_flush {
        match try_incremental_auto_flush(storage, beads_dir, jsonl_path, allow_external_jsonl) {
            Ok(Some(result)) => {
                tracing::info!(
                    flushed = result.flushed,
                    exported = result.exported_count,
                    "Auto-flush complete"
                );
                return Ok(result);
            }
            Ok(None) => {}
            Err(err) => {
                tracing::debug!(
                    ?err,
                    "Incremental auto-flush unavailable; falling back to full export"
                );
            }
        }
    }

    // Configure export with defaults, including beads_dir for path validation.
    // When needs_flush is set (e.g. after purge_issue), force must be true
    // even if there are also dirty issues from related mutations (like
    // dependency removal), so that the safety guard does not block export
    // of a DB that intentionally has fewer issues than the on-disk JSONL.
    let export_config = ExportConfig {
        force: needs_flush,
        beads_dir: Some(beads_dir.to_path_buf()),
        allow_external_jsonl,
        ..Default::default()
    };

    // Perform export
    let (export_result, _report) =
        export_to_jsonl_with_policy(storage, jsonl_path, &export_config)?;

    // Finalize export (clear dirty flags, update metadata)
    finalize_export(
        storage,
        &export_result,
        Some(&export_result.issue_hashes),
        jsonl_path,
    )?;

    tracing::info!(
        exported = export_result.exported_count,
        "Auto-flush complete"
    );

    Ok(AutoFlushResult {
        flushed: true,
        exported_count: export_result.exported_count,
        content_hash: export_result.content_hash,
    })
}

/// Read all issues from a JSONL file.
///
/// # Errors
///
/// Returns an error if the file cannot be read or contains invalid JSON.
pub fn read_issues_from_jsonl(path: &Path) -> Result<Vec<Issue>> {
    let file = File::open(path)?;
    path::validate_jsonl_fd_metadata(&file, path)?;
    let file_size = file.metadata().map_or(0, |m| m.len());
    let estimated_count = (file_size / 500) as usize;
    let mut reader = BufReader::new(file);
    let mut issues = Vec::with_capacity(estimated_count);
    let mut seen_ids = HashSet::with_capacity(estimated_count);
    let mut line = String::new();
    let mut line_num = 0;

    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line)?;
        if bytes_read == 0 {
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            line_num += 1;
            continue;
        }

        let issue: Issue = serde_json::from_str(trimmed).map_err(|e| {
            BeadsError::Config(format!("Invalid JSON at line {}: {}", line_num + 1, e))
        })?;
        if !seen_ids.insert(issue.id.clone()) {
            return Err(BeadsError::Config(format!(
                "Duplicate issue id '{}' in {} at line {}",
                issue.id,
                path.display(),
                line_num + 1
            )));
        }
        issues.push(issue);
        line_num += 1;
    }

    Ok(issues)
}

// ===== 4-Phase Collision Detection =====

/// Match type from collision detection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchType {
    /// Matched by external reference (e.g., JIRA-123).
    ExternalRef,
    /// Matched by content hash (deduplication).
    ContentHash,
    /// Matched by ID.
    Id,
}

/// Result of collision detection.
#[derive(Debug, Clone)]
pub enum CollisionResult {
    /// No match found - issue is new.
    NewIssue,
    /// Matched an existing issue.
    Match {
        /// The existing issue ID.
        existing_id: String,
        /// How the match was determined.
        match_type: MatchType,
        /// Which phase found the match (1-3).
        phase: u8,
    },
}

/// Action to take after collision detection.
#[derive(Debug, Clone)]
pub enum CollisionAction {
    /// Insert as a new issue.
    Insert,
    /// Update the existing issue.
    Update { existing_id: String },
    /// Skip this issue (existing is newer or it's a tombstone).
    Skip { reason: String },
}

/// Detect collision for an incoming issue using the 4-phase algorithm with preloaded metadata maps.
fn detect_collision(
    incoming: &Issue,
    id_by_ext_ref: &std::collections::HashMap<String, String>,
    id_by_hash: &std::collections::HashMap<String, String>,
    meta_by_id: &std::collections::HashMap<String, crate::storage::sqlite::IssueMetadata>,
    computed_hash: &str,
) -> CollisionResult {
    // Phase 1: External reference match
    if let Some(ref external_ref) = incoming.external_ref
        && let Some(existing_id) = id_by_ext_ref.get(external_ref)
    {
        return CollisionResult::Match {
            existing_id: existing_id.clone(),
            match_type: MatchType::ExternalRef,
            phase: 1,
        };
    }

    // Phase 2: ID match
    if meta_by_id.contains_key(&incoming.id) {
        return CollisionResult::Match {
            existing_id: incoming.id.clone(),
            match_type: MatchType::Id,
            phase: 2,
        };
    }

    // Phase 3: Content hash match
    if let Some(existing_id) = id_by_hash.get(computed_hash) {
        return CollisionResult::Match {
            existing_id: existing_id.clone(),
            match_type: MatchType::ContentHash,
            phase: 3,
        };
    }

    // Phase 4: No match
    CollisionResult::NewIssue
}

/// Determine the action to take based on collision result.
fn determine_action(
    collision: &CollisionResult,
    incoming: &Issue,
    meta_by_id: &std::collections::HashMap<String, crate::storage::sqlite::IssueMetadata>,
    force_upsert: bool,
) -> Result<CollisionAction> {
    match collision {
        CollisionResult::NewIssue => Ok(CollisionAction::Insert),
        CollisionResult::Match { existing_id, .. } => {
            let existing_meta =
                meta_by_id
                    .get(existing_id)
                    .ok_or_else(|| BeadsError::IssueNotFound {
                        id: existing_id.clone(),
                    })?;

            // Check for tombstone protection (even force doesn't override this)
            if existing_meta.status == crate::model::Status::Tombstone {
                return Ok(CollisionAction::Skip {
                    reason: format!("Tombstone protection: {existing_id}"),
                });
            }

            // If force_upsert is enabled, always update (skip timestamp comparison)
            if force_upsert {
                return Ok(CollisionAction::Update {
                    existing_id: existing_id.clone(),
                });
            }

            // Last-write-wins: compare updated_at
            match incoming.updated_at.cmp(&existing_meta.updated_at) {
                std::cmp::Ordering::Greater => Ok(CollisionAction::Update {
                    existing_id: existing_id.clone(),
                }),
                std::cmp::Ordering::Equal => Ok(CollisionAction::Skip {
                    reason: format!("Equal timestamps: {existing_id}"),
                }),
                std::cmp::Ordering::Less => Ok(CollisionAction::Skip {
                    reason: format!("Existing is newer: {existing_id}"),
                }),
            }
        }
    }
}

/// Normalize an issue for import.
///
/// - Recomputes `content_hash`
/// - Sets ephemeral=true if ID contains "-wisp-"
/// - Applies defaults and repairs `closed_at` invariant
fn normalize_issue(issue: &mut Issue) {
    use crate::util::content_hash;

    // Deduplicate labels
    if !issue.labels.is_empty() {
        issue.labels.sort();
        issue.labels.dedup();
    }

    // Normalize dependency types (fix legacy underscores)
    for dep in &mut issue.dependencies {
        if let crate::model::DependencyType::Custom(custom) = &dep.dep_type {
            let candidate = custom.replace('_', "-");
            if let Ok(normalized) = candidate.parse::<crate::model::DependencyType>()
                && !matches!(normalized, crate::model::DependencyType::Custom(_))
            {
                dep.dep_type = normalized;
            }
        }
    }

    // Deduplicate dependencies: for each (issue_id, depends_on_id, dep_type) triple,
    // keep only the most recent entry by created_at. This handles duplicate parent-child
    // entries from reparenting or migration artifacts (see issue #159).
    if issue.dependencies.len() > 1 {
        use std::collections::HashMap;
        // Build a map keyed by (issue_id, depends_on_id, dep_type), keeping the entry
        // with the latest created_at for each triple.
        let mut best: HashMap<(String, String, String), usize> = HashMap::new();
        for (i, dep) in issue.dependencies.iter().enumerate() {
            let key = (
                dep.issue_id.clone(),
                dep.depends_on_id.clone(),
                dep.dep_type.as_str().to_string(),
            );
            match best.get(&key) {
                Some(&prev_idx) if issue.dependencies[prev_idx].created_at >= dep.created_at => {
                    // existing entry is newer or equal, skip
                }
                _ => {
                    best.insert(key, i);
                }
            }
        }
        if best.len() < issue.dependencies.len() {
            let mut keep_indices: Vec<usize> = best.into_values().collect();
            keep_indices.sort_unstable();
            issue.dependencies = keep_indices
                .into_iter()
                .map(|i| issue.dependencies[i].clone())
                .collect();
        }
    }

    // Normalize legacy Go-beads (bd) terminal status aliases that survived
    // JSONL import as `Status::Custom(_)`. Leaving them unmapped is
    // corruptive: our own `is_terminal()` returns false for Custom, so the
    // closed_at repair below skips them and the CHECK constraint later
    // rejects the row. Downstream consumers (bv, bd-style readers) also
    // reject unknown statuses outright.
    if let crate::model::Status::Custom(raw) = &issue.status {
        let key = raw.trim().to_ascii_lowercase();
        if matches!(
            key.as_str(),
            "done" | "complete" | "completed" | "finished" | "resolved"
        ) {
            issue.status = crate::model::Status::Closed;
        }
    }

    // Wisp detection: if ID contains "-wisp-", mark as ephemeral
    if issue.id.contains("-wisp-") {
        issue.ephemeral = true;
    }

    // Repair closed_at invariant: if status is terminal (closed/tombstone), ensure closed_at is set
    if issue.status.is_terminal() && issue.closed_at.is_none() {
        issue.closed_at = Some(issue.updated_at);
    }

    // If status is not terminal, clear closed_at
    if !issue.status.is_terminal() {
        issue.closed_at = None;
    }

    // Normalize external_ref: empty string should be None to prevent UNIQUE constraint violations
    if let Some(ext_ref) = &issue.external_ref {
        if ext_ref.trim().is_empty() {
            issue.external_ref = None;
        } else {
            // Re-assign trimmed version just in case
            issue.external_ref = Some(ext_ref.trim().to_string());
        }
    }

    // Repair timestamps invariant: updated_at cannot be before created_at.
    // In distributed systems, clocks can be out of sync; we enforce the invariant
    // locally to keep the database consistent.
    if issue.updated_at < issue.created_at {
        issue.updated_at = issue.created_at;
    }

    // Recompute after all import repairs so the stored row hash matches the
    // canonical issue state used by collision detection and export hashes.
    issue.content_hash = Some(content_hash(issue));
}

#[derive(Debug)]
struct PrefixRenameSeed {
    old_id: String,
    title: String,
    description: Option<String>,
    created_by: Option<String>,
    created_at: DateTime<Utc>,
}

#[derive(Debug, Default)]
struct ImportValidationPlan {
    record_count: usize,
    prefix_mismatches: Vec<PrefixRenameSeed>,
    occupied_ids: HashSet<String>,
}

struct ImportMetadataMaps {
    meta_by_id: HashMap<String, crate::storage::sqlite::IssueMetadata>,
    id_by_ext_ref: HashMap<String, String>,
    id_by_hash: HashMap<String, String>,
}

fn parse_normalized_import_issue(trimmed: &str, line_num: usize) -> Result<Issue> {
    let mut issue: Issue = serde_json::from_str(trimmed)
        .map_err(|e| BeadsError::Config(format!("Invalid JSON at line {line_num}: {e}")))?;

    normalize_issue(&mut issue);

    if let Err(errors) = IssueValidator::validate(&issue) {
        let details = errors
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(BeadsError::Config(format!(
            "Validation failed for issue {} at line {}: {}",
            issue.id, line_num, details
        )));
    }

    Ok(issue)
}

fn for_each_jsonl_import_issue(
    input_path: &Path,
    mut handle_issue: impl FnMut(usize, Issue) -> Result<()>,
) -> Result<()> {
    let file = File::open(input_path)?;
    path::validate_jsonl_fd_metadata(&file, input_path)?;
    let mut reader = BufReader::with_capacity(2 * 1024 * 1024, file);
    let mut line = String::new();
    let mut line_num = 0usize;

    while reader.read_line(&mut line)? > 0 {
        line_num += 1;
        let trimmed = line.trim();
        if !trimmed.is_empty() {
            let issue = parse_normalized_import_issue(trimmed, line_num)?;
            handle_issue(line_num, issue)?;
        }
        line.clear();
    }

    Ok(())
}

fn collect_import_validation_plan(
    input_path: &Path,
    config: &ImportConfig,
    expected_prefix: Option<&str>,
) -> Result<ImportValidationPlan> {
    let mut plan = ImportValidationPlan::default();
    let mut seen_ids = HashSet::new();

    for_each_jsonl_import_issue(input_path, |line_num, issue| {
        let prefix_mismatch = !config.skip_prefix_validation
            && expected_prefix.is_some_and(|prefix| {
                !id_matches_expected_prefix(&issue.id, prefix)
                    && issue.status != crate::model::Status::Tombstone
            });

        if prefix_mismatch && !config.rename_on_import {
            return Err(BeadsError::Config(format!(
                "Prefix mismatch at line {}: expected '{}', found issue '{}'",
                line_num,
                expected_prefix.unwrap_or_default(),
                issue.id
            )));
        }

        if !seen_ids.insert(issue.id.clone()) {
            return Err(BeadsError::Config(format!(
                "Duplicate issue id '{}' in {} at line {}",
                issue.id,
                input_path.display(),
                line_num
            )));
        }

        if prefix_mismatch {
            plan.prefix_mismatches.push(PrefixRenameSeed {
                old_id: issue.id,
                title: issue.title,
                description: issue.description,
                created_by: issue.created_by,
                created_at: issue.created_at,
            });
        } else {
            plan.occupied_ids.insert(issue.id);
        }
        plan.record_count += 1;

        Ok(())
    })?;

    Ok(plan)
}

fn build_prefix_renames(
    storage: &SqliteStorage,
    plan: &ImportValidationPlan,
    expected_prefix: Option<&str>,
) -> Result<HashMap<String, String>> {
    if plan.prefix_mismatches.is_empty() {
        return Ok(HashMap::new());
    }

    let Some(prefix) = expected_prefix else {
        return Ok(HashMap::new());
    };

    let generator = IdGenerator::new(IdConfig::with_prefix(prefix));
    let mut occupied_ids = plan.occupied_ids.clone();
    occupied_ids.extend(storage.get_all_ids()?);

    let mut generated_ids = HashSet::new();
    let mut renames = HashMap::with_capacity(plan.prefix_mismatches.len());

    for seed in &plan.prefix_mismatches {
        let new_id = generator.generate(
            &seed.title,
            seed.description.as_deref(),
            seed.created_by.as_deref(),
            seed.created_at,
            plan.record_count,
            |candidate| occupied_ids.contains(candidate) || generated_ids.contains(candidate),
        );
        generated_ids.insert(new_id.clone());
        renames.insert(seed.old_id.clone(), new_id);
    }

    Ok(renames)
}

fn apply_prefix_renames(issue: &mut Issue, renames: &HashMap<String, String>) {
    use crate::util::content_hash;

    if let Some(new_id) = renames.get(&issue.id) {
        if issue.external_ref.is_none() {
            issue.external_ref = Some(issue.id.clone());
        }
        issue.id.clone_from(new_id);
        issue.content_hash = Some(content_hash(issue));
    }

    for dep in &mut issue.dependencies {
        if let Some(new_target) = renames.get(&dep.depends_on_id) {
            dep.depends_on_id.clone_from(new_target);
        }
        if let Some(new_source) = renames.get(&dep.issue_id) {
            dep.issue_id.clone_from(new_source);
        }
    }

    for comment in &mut issue.comments {
        if let Some(new_source) = renames.get(&comment.issue_id) {
            comment.issue_id.clone_from(new_source);
        }
    }
}

fn load_import_metadata_maps(storage: &SqliteStorage) -> Result<ImportMetadataMaps> {
    let all_meta = storage.get_all_issues_metadata()?;
    let meta_len = all_meta.len();
    let mut meta_by_id = HashMap::with_capacity(meta_len);
    let mut id_by_ext_ref = HashMap::with_capacity(meta_len);
    let mut id_by_hash = HashMap::with_capacity(meta_len);

    for metadata in all_meta {
        let issue_id = metadata.id.clone();
        if let Some(ext) = metadata.external_ref.as_ref() {
            id_by_ext_ref
                .entry(ext.clone())
                .or_insert_with(|| issue_id.clone());
        }
        if let Some(hash) = metadata.content_hash.as_ref() {
            // Preserve the first matching issue to mirror the old query_row
            // collision path when multiple issues share the same content hash.
            id_by_hash
                .entry(hash.clone())
                .or_insert_with(|| issue_id.clone());
        }
        meta_by_id.insert(issue_id, metadata);
    }

    Ok(ImportMetadataMaps {
        meta_by_id,
        id_by_ext_ref,
        id_by_hash,
    })
}

fn handle_duplicate_external_ref(
    issue: &mut Issue,
    seen_external_refs: &mut HashSet<String>,
    config: &ImportConfig,
) -> Result<()> {
    let Some(ext_ref) = issue.external_ref.clone() else {
        return Ok(());
    };

    if seen_external_refs.contains(&ext_ref) {
        if config.clear_duplicate_external_refs {
            issue.external_ref = None;
            issue.content_hash = Some(crate::util::content_hash(issue));
            Ok(())
        } else {
            Err(BeadsError::Config(format!(
                "Duplicate external_ref: {ext_ref}"
            )))
        }
    } else {
        seen_external_refs.insert(ext_ref);
        Ok(())
    }
}

fn scan_import_collision_renames(
    input_path: &Path,
    config: &ImportConfig,
    prefix_renames: &HashMap<String, String>,
    metadata: &ImportMetadataMaps,
    result: &mut ImportResult,
    record_count: usize,
) -> Result<HashMap<String, String>> {
    let mut seen_external_refs = HashSet::new();
    let mut renames = HashMap::new();
    let progress =
        create_progress_bar(record_count as u64, "Scanning issues", config.show_progress);

    for_each_jsonl_import_issue(input_path, |_line_num, mut issue| {
        apply_prefix_renames(&mut issue, prefix_renames);

        if issue.ephemeral {
            result.skipped_count += 1;
            progress.inc(1);
            return Ok(());
        }

        handle_duplicate_external_ref(&mut issue, &mut seen_external_refs, config)?;

        let computed_hash = crate::util::content_hash(&issue);
        let collision = detect_collision(
            &issue,
            &metadata.id_by_ext_ref,
            &metadata.id_by_hash,
            &metadata.meta_by_id,
            &computed_hash,
        );
        let _action = determine_action(
            &collision,
            &issue,
            &metadata.meta_by_id,
            config.force_upsert,
        )?;
        let target_id = match &collision {
            CollisionResult::Match { existing_id, .. } => existing_id.clone(),
            CollisionResult::NewIssue => issue.id.clone(),
        };

        if target_id != issue.id {
            renames.insert(issue.id.clone(), target_id);
        }

        progress.inc(1);
        Ok(())
    })?;

    progress.finish_with_message("Scan complete");
    Ok(renames)
}

fn apply_collision_renames(issue: &mut Issue, renames: &HashMap<String, String>) {
    if let Some(new_id) = renames.get(&issue.id) {
        issue.id.clone_from(new_id);
    }

    for dep in &mut issue.dependencies {
        if let Some(new_target) = renames.get(&dep.depends_on_id) {
            dep.depends_on_id.clone_from(new_target);
        }
        if let Some(new_source) = renames.get(&dep.issue_id) {
            dep.issue_id.clone_from(new_source);
        }
    }

    for comment in &mut issue.comments {
        if let Some(new_source) = renames.get(&comment.issue_id) {
            comment.issue_id.clone_from(new_source);
        }
    }
}

fn cleanup_import_orphans_in_tx(storage: &SqliteStorage) -> Result<usize> {
    let orphan_tables = &[
        ("dependencies", "issue_id"),
        ("dependencies", "depends_on_id"),
        ("labels", "issue_id"),
        ("comments", "issue_id"),
        ("events", "issue_id"),
        ("dirty_issues", "issue_id"),
        ("blocked_issues_cache", "issue_id"),
        ("child_counters", "parent_id"),
    ];
    let mut orphans_cleaned = 0usize;

    for (table, col) in orphan_tables {
        let external_dependency_filter = match (*table, *col) {
            ("dependencies", "issue_id") => " AND issue_id NOT LIKE 'external:%'",
            ("dependencies", "depends_on_id") => " AND depends_on_id NOT LIKE 'external:%'",
            _ => "",
        };
        let sql = format!(
            "DELETE FROM {table} WHERE {col} NOT IN (SELECT id FROM issues){external_dependency_filter}"
        );
        orphans_cleaned += storage.execute_raw_count(&sql)?;
    }

    Ok(orphans_cleaned)
}

fn skipped_import_matches_stored_issue(
    storage: &SqliteStorage,
    target_id: &str,
    incoming: &Issue,
) -> Result<bool> {
    let Some(mut stored) = storage.get_issue_for_export(target_id)? else {
        return Ok(false);
    };
    let mut expected = incoming.clone();
    if expected.id != target_id {
        expected.id = target_id.to_string();
    }

    normalize_issue_for_export(&mut stored);
    normalize_issue_for_export(&mut expected);
    Ok(stored.sync_equals(&expected))
}

fn export_hash_entry_for_import_action(
    storage: &SqliteStorage,
    action: &CollisionAction,
    target_id: &str,
    issue: &Issue,
    computed_hash: &str,
) -> Result<Option<(String, String)>> {
    match action {
        CollisionAction::Insert | CollisionAction::Update { .. } => {
            Ok(Some((target_id.to_string(), computed_hash.to_string())))
        }
        CollisionAction::Skip { .. } => {
            if skipped_import_matches_stored_issue(storage, target_id, issue)? {
                Ok(Some((target_id.to_string(), computed_hash.to_string())))
            } else {
                Ok(None)
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn stream_import_actions_in_tx(
    storage: &SqliteStorage,
    input_path: &Path,
    config: &ImportConfig,
    prefix_renames: &HashMap<String, String>,
    collision_renames: &HashMap<String, String>,
    metadata: &ImportMetadataMaps,
    base_result: &ImportResult,
    progress: &indicatif::ProgressBar,
) -> Result<ImportResult> {
    let mut tx_result = base_result.clone();
    let mut seen_external_refs = HashSet::new();
    let mut export_hash_batch = Vec::with_capacity(IMPORT_EXPORT_HASH_BATCH_SIZE);
    let mut export_hash_ids = HashSet::new();
    let mut uncertified_local_wins = 0usize;

    progress.set_position(0);
    storage.clear_all_export_hashes_in_tx()?;

    for_each_jsonl_import_issue(input_path, |_line_num, mut issue| {
        apply_prefix_renames(&mut issue, prefix_renames);

        if issue.ephemeral {
            progress.inc(1);
            return Ok(());
        }

        handle_duplicate_external_ref(&mut issue, &mut seen_external_refs, config)?;

        let computed_hash = crate::util::content_hash(&issue);
        let collision = detect_collision(
            &issue,
            &metadata.id_by_ext_ref,
            &metadata.id_by_hash,
            &metadata.meta_by_id,
            &computed_hash,
        );
        let action = determine_action(
            &collision,
            &issue,
            &metadata.meta_by_id,
            config.force_upsert,
        )?;
        let target_id = match &collision {
            CollisionResult::Match { existing_id, .. } => existing_id.clone(),
            CollisionResult::NewIssue => issue.id.clone(),
        };

        apply_collision_renames(&mut issue, collision_renames);
        process_import_action(storage, &action, &issue, &mut tx_result)?;

        if let Some((export_id, export_hash)) = export_hash_entry_for_import_action(
            storage,
            &action,
            &target_id,
            &issue,
            &computed_hash,
        )? {
            export_hash_ids.insert(export_id.clone());
            export_hash_batch.push((export_id, export_hash));
            if export_hash_batch.len() >= IMPORT_EXPORT_HASH_BATCH_SIZE {
                storage.insert_export_hashes_after_clear_in_tx(&export_hash_batch)?;
                export_hash_batch.clear();
            }
        } else {
            uncertified_local_wins += 1;
        }

        progress.inc(1);
        Ok(())
    })?;

    if !export_hash_batch.is_empty() {
        storage.insert_export_hashes_after_clear_in_tx(&export_hash_batch)?;
    }
    tx_result.export_hashes_recorded = export_hash_ids.len();
    if uncertified_local_wins > 0 {
        tracing::debug!(
            count = uncertified_local_wins,
            "Import preserved local records that differ from JSONL; marking database for flush"
        );
        storage.set_metadata_in_tx("needs_flush", "true")?;
    }

    let orphans_cleaned = cleanup_import_orphans_in_tx(storage)?;
    if orphans_cleaned > 0 {
        tracing::info!(
            count = orphans_cleaned,
            "Cleaned orphaned FK rows after import"
        );
        tx_result.orphan_cleaned_count = orphans_cleaned;
    }

    tx_result.blocked_cache_entries = storage.rebuild_blocked_cache_in_tx()?;
    tx_result.child_counter_entries = storage.rebuild_child_counters_in_tx()?;

    Ok(tx_result)
}

/// Import issues from a JSONL file.
///
/// Implements classic bd import semantics:
/// 0. Path validation - reject git paths and outside-beads paths without opt-in
/// 1. Conflict marker scan - abort if found
/// 2. Parse JSONL with 2MB buffer
/// 3. Normalize issues (recompute `content_hash`, set defaults)
/// 4. Prefix validation (optional)
/// 5. 4-phase collision detection
/// 6. Tombstone protection
/// 7. Orphan handling
/// 8. Create/update issues
/// 9. Sync deps/labels/comments
/// 10. Refresh blocked cache
/// 11. Update metadata
///
/// # Errors
///
/// Returns an error if:
/// - Path validation fails (git path, outside `beads_dir` without opt-in)
/// - Conflict markers are detected
/// - File cannot be read
/// - Prefix validation fails
/// - Database operations fail
#[allow(clippy::too_many_lines)]
pub fn import_from_jsonl(
    storage: &mut SqliteStorage,
    input_path: &Path,
    config: &ImportConfig,
    expected_prefix: Option<&str>,
) -> Result<ImportResult> {
    // Step 0: Path validation (PC-1, PC-2, PC-3, NGI-3) - BEFORE any file operations
    if let Some(ref beads_dir) = config.beads_dir {
        validate_sync_path_with_external(input_path, beads_dir, config.allow_external_jsonl)?;
        tracing::debug!(
            input_path = %input_path.display(),
            beads_dir = %beads_dir.display(),
            allow_external = config.allow_external_jsonl,
            "Import path validated"
        );
    }

    // Step 1: Conflict marker scan
    ensure_no_conflict_markers(input_path)?;

    // Step 2: Parse, Normalize, Validate, and collect minimal rename state.
    let spinner = create_spinner("Parsing and validating issues", config.show_progress);
    let validation_plan = collect_import_validation_plan(input_path, config, expected_prefix)?;
    spinner.finish_with_message("Parsed and validated issues");

    let mut result = ImportResult::default();

    // Step 5: Handle renames if requested
    let prefix_renames = if config.rename_on_import {
        build_prefix_renames(storage, &validation_plan, expected_prefix)?
    } else {
        HashMap::new()
    };

    // Preload metadata for O(1) collision detection while streaming the input.
    let metadata = load_import_metadata_maps(storage)?;

    // Phase 1: Scan and Resolve IDs
    let collision_renames = scan_import_collision_renames(
        input_path,
        config,
        &prefix_renames,
        &metadata,
        &mut result,
        validation_plan.record_count,
    )?;

    let jsonl_hash = compute_jsonl_hash(input_path)?;
    let observed_jsonl = observed_jsonl_witness(input_path)?;

    // Phase 2: Execute Actions
    //
    // Disable FK constraints during bulk import so that issues can reference
    // other issues (in dependencies/comments) that haven't been inserted yet.
    // FK integrity is restored and validated after all data is loaded.
    storage
        .execute_raw("PRAGMA foreign_keys = OFF")
        .map_err(|source| BeadsError::WithContext {
            context: "Failed to disable foreign key enforcement before import".to_string(),
            source: Box::new(source),
        })?;

    let progress = create_progress_bar(
        validation_plan.record_count as u64,
        "Importing issues",
        config.show_progress,
    );

    let apply_result = storage.with_write_transaction(|storage| -> Result<ImportResult> {
        let tx_result = stream_import_actions_in_tx(
            storage,
            input_path,
            config,
            &prefix_renames,
            &collision_renames,
            &metadata,
            &result,
            &progress,
        )?;

        storage.set_metadata_in_tx(METADATA_LAST_IMPORT_TIME, &chrono::Utc::now().to_rfc3339())?;
        storage.set_metadata_in_tx(METADATA_JSONL_CONTENT_HASH, &jsonl_hash)?;
        record_observed_jsonl_witness_in_tx(storage, &observed_jsonl)?;

        Ok(tx_result)
    });

    let validate_foreign_keys = apply_result.is_ok();
    let fk_restore_result = restore_foreign_keys_after_import(storage, validate_foreign_keys);

    match finish_import_after_foreign_key_restore(apply_result, fk_restore_result) {
        Ok(import_result) => {
            progress.finish_with_message("Import complete");
            Ok(import_result)
        }
        Err(err) => {
            progress.finish_and_clear();
            Err(err)
        }
    }
}

pub(crate) fn id_matches_expected_prefix(id: &str, expected_prefix: &str) -> bool {
    let normalized_prefix = expected_prefix.trim_end_matches('-');
    if normalized_prefix.is_empty() {
        return false;
    }

    parse_id(id).is_ok_and(|parsed| {
        // Slugged root IDs are shaped as `<prefix>-<slug>-<hash>`.
        // `parse_id` treats the slug as part of the hyphenated prefix, so
        // prefix guardrails must accept this generated prefix family.
        parsed.prefix == normalized_prefix
            || parsed
                .prefix
                .strip_prefix(normalized_prefix)
                .is_some_and(|suffix| suffix.starts_with('-'))
    })
}

/// Process a single import action.
fn process_import_action(
    storage: &SqliteStorage,
    action: &CollisionAction,
    issue: &Issue,
    result: &mut ImportResult,
) -> Result<()> {
    match action {
        CollisionAction::Insert => {
            if insert_new_import_issue(storage, issue)?
                && !storage.has_owned_relation_rows_for_import(&issue.id)?
            {
                storage.insert_new_issue_relations_for_import(issue)?;
            } else {
                sync_issue_relations(storage, issue)?;
            }
            result.imported_count += 1;
            result.created_count += 1;
            record_imported_relation_counts(result, issue);
        }
        CollisionAction::Update { existing_id } => {
            // When updating by external_ref or content_hash, the incoming issue may have
            // a different ID than the existing one. We need to update using the existing ID.
            if existing_id == &issue.id {
                storage.upsert_issue_for_import(issue)?;
                sync_issue_relations(storage, issue)?;
            } else {
                let mut updated_issue = issue.clone();
                updated_issue.id.clone_from(existing_id);
                storage.upsert_issue_for_import(&updated_issue)?;
                sync_issue_relations(storage, &updated_issue)?;
            }
            result.imported_count += 1;
            result.updated_count += 1;
            record_imported_relation_counts(result, issue);
        }
        CollisionAction::Skip { reason } => {
            tracing::debug!(id = %issue.id, reason = %reason, "Skipping issue");
            if reason.starts_with("Tombstone") {
                result.tombstone_skipped += 1;
            } else {
                result.skipped_count += 1;
            }
        }
    }
    Ok(())
}

fn insert_new_import_issue(storage: &SqliteStorage, issue: &Issue) -> Result<bool> {
    match storage.insert_new_issue_for_import(issue) {
        Ok(_) => Ok(true),
        Err(BeadsError::Database(
            fsqlite_error::FrankenError::PrimaryKeyViolation
            | fsqlite_error::FrankenError::UniqueViolation { .. },
        )) => {
            tracing::debug!(
                id = %issue.id,
                "Import insert found a concurrent key collision; falling back to upsert"
            );
            storage.upsert_issue_for_import(issue)?;
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

fn record_imported_relation_counts(result: &mut ImportResult, issue: &Issue) {
    result.labels_imported += issue.labels.len();
    result.dependencies_imported += issue.dependencies.len();
    result.comments_imported += issue.comments.len();
}

/// Sync labels, dependencies, and comments for an imported issue.
fn sync_issue_relations(storage: &SqliteStorage, issue: &Issue) -> Result<()> {
    // Sync labels
    storage.sync_labels_for_import(&issue.id, &issue.labels)?;

    // Sync dependencies
    storage.sync_dependencies_for_import(&issue.id, &issue.dependencies)?;

    // Sync comments
    storage.sync_comments_for_import(&issue.id, &issue.comments)?;

    Ok(())
}

/// Finalize an import by computing the content hash of the imported file.
///
/// # Errors
///
/// Returns an error if the file cannot be read.
pub fn compute_jsonl_hash(path: &Path) -> Result<String> {
    use std::io::BufRead;
    let file = std::fs::File::open(path)?;
    self::path::validate_jsonl_fd_metadata(&file, path)?;
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut line_buf = Vec::with_capacity(4096);

    loop {
        line_buf.clear();
        let bytes_read = reader.read_until(b'\n', &mut line_buf)?;
        if bytes_read == 0 {
            break;
        }

        // Efficiently skip empty or whitespace-only lines without UTF-8 validation.
        // trim_ascii() is a fast byte-based trim.
        let trimmed = line_buf.trim_ascii();
        if !trimmed.is_empty() {
            hasher.update(trimmed);
            hasher.update(b"\n");
        }
    }

    Ok(hex_encode(&hasher.finalize()))
}

// ============================================================================
// 3-Way Merge Types and Functions
// ============================================================================

/// Types of conflicts that can occur during 3-way merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictType {
    /// Issue was modified locally but deleted externally (or vice versa).
    DeleteVsModify,
    /// Issue was modified independently in both local and external stores.
    BothModified,
    /// Issue was created in both local and external with different content.
    ConvergentCreation,
}

/// Result of merging a single issue across base, left (local), and right (external).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeResult {
    /// No action needed (e.g., issue doesn't exist in any source).
    NoAction,
    /// Keep the specified issue.
    Keep(Issue),
    /// Keep the specified issue with a note about the merge decision.
    KeepWithNote(Issue, String),
    /// Delete the issue.
    Delete,
    /// A conflict was detected that requires manual resolution.
    Conflict(ConflictType),
}

/// Context for performing a 3-way merge operation.
#[derive(Debug, Default)]
pub struct MergeContext {
    /// Base state (last known common state).
    pub base: std::collections::HashMap<String, Issue>,
    /// Left state (current SQLite/local changes).
    pub left: std::collections::HashMap<String, Issue>,
    /// Right state (current JSONL/external changes).
    pub right: std::collections::HashMap<String, Issue>,
}

impl MergeContext {
    /// Create a new merge context from the three states.
    #[must_use]
    #[allow(clippy::missing_const_for_fn)]
    pub fn new(
        base: std::collections::HashMap<String, Issue>,
        left: std::collections::HashMap<String, Issue>,
        right: std::collections::HashMap<String, Issue>,
    ) -> Self {
        Self { base, left, right }
    }

    /// Get all unique issue IDs across all three states.
    #[must_use]
    pub fn all_issue_ids(&self) -> std::collections::HashSet<String> {
        let mut ids = std::collections::HashSet::new();
        ids.extend(self.base.keys().cloned());
        ids.extend(self.left.keys().cloned());
        ids.extend(self.right.keys().cloned());
        ids
    }
}

/// Report of a 3-way merge operation.
#[derive(Debug, Default)]
pub struct MergeReport {
    /// Issues that were kept (created or updated).
    pub kept: Vec<Issue>,
    /// Issues that were deleted.
    pub deleted: Vec<String>,
    /// Conflicts that were detected.
    pub conflicts: Vec<(String, ConflictType)>,
    /// Issues that were skipped due to tombstone protection.
    pub tombstone_protected: Vec<String>,
    /// Notes about merge decisions.
    pub notes: Vec<(String, String)>,
}

impl MergeReport {
    /// Returns true if there were any conflicts.
    #[must_use]
    pub fn has_conflicts(&self) -> bool {
        !self.conflicts.is_empty()
    }

    /// Total number of actions taken.
    #[must_use]
    pub fn total_actions(&self) -> usize {
        self.kept.len() + self.deleted.len()
    }
}

/// Strategy for resolving conflicts during merge.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    clap::ValueEnum,
    serde::Serialize,
    serde::Deserialize,
)]
pub enum ConflictResolution {
    /// Always keep the local (`SQLite`) version.
    #[default]
    PreferLocal,
    /// Always keep the external (`JSONL`) version.
    PreferExternal,
    /// Use `updated_at` timestamp to determine winner (or specified strategy)
    PreferNewer,
    /// Report conflict without auto-resolving.
    Manual,
}

/// Merge a single issue given its state in base, left (local), and right (external).
///
/// This implements the core 3-way merge logic for a single issue:
/// - New local issues are kept
/// - New external issues are imported
/// - Deletions are handled based on whether the other side modified
/// - Both-modified uses `updated_at` as tiebreaker (or specified strategy)
///
/// # Arguments
/// * `base` - The issue in the base (common ancestor) state, if it existed
/// * `left` - The issue in the local (`SQLite`) state, if it exists
/// * `right` - The issue in the external (JSONL) state, if it exists
/// * `strategy` - How to resolve conflicts when both sides modified
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn merge_issue(
    base: Option<&Issue>,
    left: Option<&Issue>,
    right: Option<&Issue>,
    strategy: ConflictResolution,
) -> MergeResult {
    match (base, left, right) {
        // Case 1: Only in base (deleted in both local and external) -> no action
        (Some(_), None, None) => MergeResult::Delete,

        // Case 2: Only in left (new local) -> keep
        (None, Some(l), None) => MergeResult::Keep(l.clone()),

        // Case 3: Only in right (new external) -> keep
        (None, None, Some(r)) => MergeResult::Keep(r.clone()),

        // Case 4: In base and left only (deleted in right/external)
        (Some(b), Some(l), None) => {
            // Was it modified locally after base?
            if l.sync_equals(b) {
                // Local unchanged since base, external deleted -> delete
                MergeResult::Delete
            } else {
                // Local modified but external deleted - conflict
                match strategy {
                    ConflictResolution::PreferLocal => MergeResult::KeepWithNote(
                        l.clone(),
                        "Local modified, external deleted - kept local".to_string(),
                    ),
                    ConflictResolution::PreferExternal => MergeResult::Delete,
                    ConflictResolution::PreferNewer => {
                        // Keep local since it was modified more recently than base
                        MergeResult::KeepWithNote(
                            l.clone(),
                            "Local modified after base, external deleted - kept local".to_string(),
                        )
                    }
                    ConflictResolution::Manual => {
                        MergeResult::Conflict(ConflictType::DeleteVsModify)
                    }
                }
            }
        }

        // Case 5: In base and right only (deleted locally)
        (Some(b), None, Some(r)) => {
            // Was it modified externally after base?
            if r.sync_equals(b) {
                // External unchanged since base, local deleted -> delete
                MergeResult::Delete
            } else {
                // External modified but local deleted - conflict
                match strategy {
                    ConflictResolution::PreferLocal => MergeResult::Delete,
                    ConflictResolution::PreferExternal => MergeResult::KeepWithNote(
                        r.clone(),
                        "External modified, local deleted - kept external".to_string(),
                    ),
                    ConflictResolution::PreferNewer => {
                        // Keep external since it was modified more recently than base
                        MergeResult::KeepWithNote(
                            r.clone(),
                            "External modified after base, local deleted - kept external"
                                .to_string(),
                        )
                    }
                    ConflictResolution::Manual => {
                        MergeResult::Conflict(ConflictType::DeleteVsModify)
                    }
                }
            }
        }

        // Case 6: In all three (potentially modified in one or both)
        (Some(b), Some(l), Some(r)) => {
            if l.sync_equals(r) {
                return MergeResult::Keep(l.clone());
            }

            let left_changed = !l.sync_equals(b);
            let right_changed = !r.sync_equals(b);

            match (left_changed, right_changed) {
                // Neither changed OR only left changed - keep left
                (false | true, false) => MergeResult::Keep(l.clone()),
                // Only right changed - keep right
                (false, true) => MergeResult::Keep(r.clone()),
                // Both changed - use strategy
                (true, true) => match strategy {
                    ConflictResolution::PreferLocal => MergeResult::KeepWithNote(
                        l.clone(),
                        "Both modified - kept local".to_string(),
                    ),
                    ConflictResolution::PreferExternal => MergeResult::KeepWithNote(
                        r.clone(),
                        "Both modified - kept external".to_string(),
                    ),
                    ConflictResolution::PreferNewer => {
                        if l.updated_at >= r.updated_at {
                            MergeResult::KeepWithNote(
                                l.clone(),
                                "Both modified - kept local (newer)".to_string(),
                            )
                        } else {
                            MergeResult::KeepWithNote(
                                r.clone(),
                                "Both modified - kept external (newer)".to_string(),
                            )
                        }
                    }
                    ConflictResolution::Manual => MergeResult::Conflict(ConflictType::BothModified),
                },
            }
        }

        // Case 7: In left and right but not base (convergent creation)
        (None, Some(l), Some(r)) => {
            // Same content? Keep one (use left by convention)
            if l.sync_equals(r) {
                MergeResult::Keep(l.clone())
            } else {
                // Different content - both created independently
                match strategy {
                    ConflictResolution::PreferLocal => MergeResult::KeepWithNote(
                        l.clone(),
                        "Convergent creation - kept local".to_string(),
                    ),
                    ConflictResolution::PreferExternal => MergeResult::KeepWithNote(
                        r.clone(),
                        "Convergent creation - kept external".to_string(),
                    ),
                    ConflictResolution::PreferNewer => {
                        if l.updated_at >= r.updated_at {
                            MergeResult::KeepWithNote(
                                l.clone(),
                                "Convergent creation - kept local (newer)".to_string(),
                            )
                        } else {
                            MergeResult::KeepWithNote(
                                r.clone(),
                                "Convergent creation - kept external (newer)".to_string(),
                            )
                        }
                    }
                    ConflictResolution::Manual => {
                        MergeResult::Conflict(ConflictType::ConvergentCreation)
                    }
                }
            }
        }

        // Case 8: Not in any (impossible in practice, but handle gracefully)
        (None, None, None) => MergeResult::NoAction,
    }
}

/// Perform a 3-way merge across all issues in the context.
///
/// This iterates through all unique issue IDs across base, left, and right,
/// and calls `merge_issue` for each to determine the appropriate action.
///
/// # Arguments
/// * `context` - The merge context containing base, left, and right states
/// * `strategy` - How to resolve conflicts when both sides modified
/// * `tombstones` - Optional set of issue IDs that should never be resurrected
///
/// # Returns
/// A `MergeReport` containing all actions taken and any conflicts detected.
#[must_use]
pub fn three_way_merge(
    context: &MergeContext,
    strategy: ConflictResolution,
    tombstones: Option<&HashSet<String, RandomState>>,
) -> MergeReport {
    let mut report = MergeReport::default();
    let empty_tombstones: HashSet<String, RandomState> = HashSet::new();
    let tombstones = tombstones.unwrap_or(&empty_tombstones);

    for id in context.all_issue_ids() {
        let base = context.base.get(&id);
        let left = context.left.get(&id);
        let right = context.right.get(&id);

        // Check tombstone protection: if issue is tombstoned and trying to resurrect
        if tombstones.contains(&id) {
            let local_tombstone =
                left.is_some_and(|issue| issue.status == crate::model::Status::Tombstone);
            let external_non_tombstone =
                right.is_some_and(|issue| issue.status != crate::model::Status::Tombstone);

            if local_tombstone && external_non_tombstone {
                // Import paths never allow JSONL to resurrect a local tombstone.
                // Merge winner flags must preserve that invariant too.
                if let Some(issue) = left {
                    report.kept.push(issue.clone());
                }
                report.tombstone_protected.push(id.clone());
                continue;
            }

            if left.is_none() && external_non_tombstone {
                // Trying to resurrect from external - skip.
                report.tombstone_protected.push(id.clone());
                continue;
            }
        }

        let result = merge_issue(base, left, right, strategy);

        match result {
            MergeResult::NoAction => {}
            MergeResult::Keep(issue) => {
                report.kept.push(issue);
            }
            MergeResult::KeepWithNote(issue, note) => {
                report.notes.push((issue.id.clone(), note));
                report.kept.push(issue);
            }
            MergeResult::Delete => {
                report.deleted.push(id.clone());
            }
            MergeResult::Conflict(conflict_type) => {
                report.conflicts.push((id.clone(), conflict_type));
            }
        }
    }

    report
}

/// Configuration for a 3-way merge operation.
#[derive(Debug, Clone, Default)]
pub struct MergeConfig {
    /// Strategy for resolving conflicts.
    pub strategy: ConflictResolution,
    /// Whether to skip tombstoned issues.
    pub respect_tombstones: bool,
}

/// Save the base snapshot to a file.
///
/// This is used after a successful merge to record the common state.
///
/// # Errors
///
/// Returns an error if the file cannot be written.
pub fn save_base_snapshot<S: ::std::hash::BuildHasher>(
    issues: &std::collections::HashMap<String, Issue, S>,
    jsonl_dir: &Path,
) -> Result<()> {
    let snapshot_path = jsonl_dir.join("beads.base.jsonl");
    let (temp_path, temp_file) = create_base_snapshot_temp_file(&snapshot_path, jsonl_dir)?;
    let mut temp_guard = TempFileGuard::new(temp_path.clone());
    let mut writer = BufWriter::new(temp_file);

    let mut ordered_issues: Vec<_> = issues.values().collect();
    ordered_issues.sort_by(|left, right| left.id.cmp(&right.id));

    let mut buffer = Vec::new();
    for issue in ordered_issues {
        buffer.clear();
        serde_json::to_writer(&mut buffer, issue).map_err(|e| {
            BeadsError::Config(format!("Failed to serialize issue {}: {}", issue.id, e))
        })?;
        writer.write_all(&buffer).map_err(BeadsError::Io)?;
        writer.write_all(b"\n").map_err(BeadsError::Io)?;
    }
    writer.flush()?;
    writer
        .into_inner()
        .map_err(|e| BeadsError::Io(e.into_error()))?
        .sync_all()?;
    require_safe_sync_overwrite_path(&temp_path, jsonl_dir, false, "rename base snapshot")?;
    require_safe_sync_overwrite_path(&snapshot_path, jsonl_dir, false, "overwrite base snapshot")?;
    crate::util::durable_rename(&temp_path, &snapshot_path)?;
    temp_guard.persist();
    Ok(())
}

/// Save the base snapshot from a finalized JSONL export.
///
/// This is used after a successful merge export so `beads.base.jsonl` reflects
/// the exact JSONL state that reached disk, including DB-side merge notes or
/// other derived fields added after the merge report was calculated.
///
/// # Errors
///
/// Returns an error if the finalized JSONL cannot be read or the base snapshot
/// cannot be written.
pub fn save_base_snapshot_from_jsonl(jsonl_path: &Path, jsonl_dir: &Path) -> Result<()> {
    ensure_no_conflict_markers(jsonl_path)?;
    let issues: std::collections::HashMap<String, Issue> = read_issues_from_jsonl(jsonl_path)?
        .into_iter()
        .map(|issue| (issue.id.clone(), issue))
        .collect();
    save_base_snapshot(&issues, jsonl_dir)
}

/// Load the base snapshot from a file.
///
/// Returns an empty map if the snapshot does not exist.
///
/// # Errors
///
/// Returns an error if the file exists but cannot be read or parsed.
pub fn load_base_snapshot(jsonl_dir: &Path) -> Result<std::collections::HashMap<String, Issue>> {
    let snapshot_path = jsonl_dir.join("beads.base.jsonl");
    let mut base = std::collections::HashMap::new();

    if !snapshot_path.exists() {
        return Ok(base);
    }

    require_valid_sync_path(&snapshot_path, jsonl_dir)?;

    // `beads.base.jsonl` is normally .gitignore'd, but if a user's workspace
    // has it committed (or committed by accident once), a botched `git merge`
    // / `git pull` can leave `<<<<<<<` / `=======` / `>>>>>>>` markers in
    // this file just like in `issues.jsonl`. The serde parse below would
    // then fail with a generic "Invalid JSON in base snapshot at line N"
    // that buries the actual problem. Run the conflict-marker scan first so
    // the operator gets the same actionable "merge conflict markers
    // detected" diagnostic that the JSONL path surfaces.
    ensure_no_conflict_markers(&snapshot_path)?;

    let file = File::open(&snapshot_path)?;
    let reader = BufReader::new(file);

    for (line_num, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let issue: Issue = serde_json::from_str(&line).map_err(|e| {
            BeadsError::Config(format!(
                "Invalid JSON in base snapshot at line {}: {}",
                line_num + 1,
                e
            ))
        })?;
        base.insert(issue.id.clone(), issue);
    }

    Ok(base)
}

/// A tombstone row plus its related labels, dependencies, and comments,
/// snapshotted before a rebuild so it can be atomically restored afterwards.
///
/// The option wrappers on the relations let callers partially preserve a
/// tombstone whose relation fetches failed (a pattern the CLI layer already
/// uses): we keep the issue row and skip whatever relation set couldn't be
/// read, rather than losing the tombstone entirely.
#[derive(Clone, Debug)]
pub(crate) struct PreservedTombstone {
    pub(crate) issue: Issue,
    pub(crate) labels: Option<Vec<String>>,
    pub(crate) dependencies: Option<Vec<Dependency>>,
    pub(crate) comments: Option<Vec<Comment>>,
}

/// Snapshot every tombstoned issue in the database, including its labels,
/// dependencies, and comments, so a rebuild can restore deletion-retention
/// state that is not present in the JSONL export.
///
/// This is fully best-effort — the function never returns an error: if
/// the enumeration query fails outright we log and return an empty list
/// (the rebuild still proceeds without tombstone preservation), and
/// per-tombstone relation fetches also degrade gracefully to issue-row-
/// only preservation.
#[must_use]
pub(crate) fn snapshot_tombstones(storage: &SqliteStorage) -> Vec<PreservedTombstone> {
    let mut tombstones = Vec::new();
    let tombstone_ids = match storage.get_issue_ids_by_status(&crate::model::Status::Tombstone) {
        Ok(ids) => ids,
        Err(error) => {
            tracing::warn!(
                error = %error,
                "Failed to enumerate tombstones before rebuild; continuing without tombstone preservation"
            );
            return tombstones;
        }
    };

    for tombstone_id in tombstone_ids {
        let Some(issue) = (match storage.get_issue(&tombstone_id) {
            Ok(issue) => issue,
            Err(error) => {
                tracing::warn!(
                    issue_id = %tombstone_id,
                    error = %error,
                    "Skipping tombstone preservation for issue that could not be read before rebuild"
                );
                continue;
            }
        }) else {
            continue;
        };

        let labels = match storage.get_labels(&tombstone_id) {
            Ok(labels) => Some(labels),
            Err(error) => {
                tracing::warn!(
                    issue_id = %tombstone_id,
                    error = %error,
                    "Failed to snapshot tombstone labels before rebuild; preserving issue row only"
                );
                None
            }
        };
        let dependencies = match storage.get_dependencies_full(&tombstone_id) {
            Ok(dependencies) => Some(dependencies),
            Err(error) => {
                tracing::warn!(
                    issue_id = %tombstone_id,
                    error = %error,
                    "Failed to snapshot tombstone dependencies before rebuild; preserving issue row only"
                );
                None
            }
        };
        let comments = match storage.get_comments(&tombstone_id) {
            Ok(comments) => Some(comments),
            Err(error) => {
                tracing::warn!(
                    issue_id = %tombstone_id,
                    error = %error,
                    "Failed to snapshot tombstone comments before rebuild; preserving issue row only"
                );
                None
            }
        };
        tombstones.push(PreservedTombstone {
            issue,
            labels,
            dependencies,
            comments,
        });
    }
    tombstones
}

/// Restore preserved tombstones after a successful rebuild, wrapping any
/// failure with a message that makes clear the rebuild itself succeeded —
/// only the retention-state restoration step failed.
///
/// The rebuild has already moved the original database family into the
/// recovery directory and replaced it with a clean JSONL import at this
/// point, so on failure the live DB is *valid* (it mirrors the JSONL),
/// just missing whatever local unflushed tombstones we tried to preserve.
/// Without this wrapper, a transient lock-contention retry exhaustion
/// inside `restore_tombstones` would bubble up through callers that
/// otherwise describe the failure as "JSONL may be corrupt" or "database
/// recovery failed", both of which are actively misleading for this
/// specific post-rebuild failure mode. The wrapped message tells the
/// operator: re-running the command is idempotent and safe; the only
/// thing they've lost is local deletions that hadn't yet been flushed.
///
/// Callers should prefer this helper over calling `restore_tombstones`
/// directly when the restore follows an already-completed rebuild. Use
/// the bare `restore_tombstones` when the surrounding transaction is
/// still mid-rebuild and a rollback is still possible.
///
/// # Errors
///
/// Returns a `BeadsError::WithContext` whose source is the original
/// `restore_tombstones` error. Returns `Ok(())` when `tombstones` is
/// empty without calling into the write-transaction retry loop.
pub(crate) fn restore_tombstones_after_rebuild(
    storage: &mut SqliteStorage,
    tombstones: &[PreservedTombstone],
) -> Result<()> {
    if tombstones.is_empty() {
        return Ok(());
    }
    let count = tombstones.len();
    restore_tombstones(storage, tombstones).map_err(|err| BeadsError::WithContext {
        context: format!(
            "Rebuild from JSONL succeeded, but failed to restore {count} preserved \
             tombstone(s). The database now mirrors the JSONL exactly — any local \
             deletions that had not yet been flushed to the JSONL are gone. \
             Re-running the command is idempotent and safe (the rebuild itself \
             completed successfully). If the underlying cause is lock contention, \
             wait for other `br` processes to finish and try again."
        ),
        source: Box::new(err),
    })
}

/// Restore preserved tombstones (and their relations) atomically and mark
/// them dirty so the next flush re-exports them.
///
/// # Errors
///
/// Returns an error if the underlying write transaction fails; the entire
/// restore is rolled back on failure.
pub(crate) fn restore_tombstones(
    storage: &mut SqliteStorage,
    tombstones: &[PreservedTombstone],
) -> Result<()> {
    if tombstones.is_empty() {
        return Ok(());
    }

    let marked_at = Utc::now().to_rfc3339();
    storage.with_write_transaction(|storage| {
        for tombstone in tombstones {
            storage.upsert_issue_for_import(&tombstone.issue)?;
        }
        for tombstone in tombstones {
            if let Some(labels) = &tombstone.labels {
                storage.sync_labels_for_import(&tombstone.issue.id, labels)?;
            }
            if let Some(dependencies) = &tombstone.dependencies {
                storage.sync_dependencies_for_import(&tombstone.issue.id, dependencies)?;
            }
            if let Some(comments) = &tombstone.comments {
                storage.sync_comments_for_import(&tombstone.issue.id, comments)?;
            }
            storage.replace_dirty_issue_marker(&tombstone.issue.id, &marked_at)?;
        }
        Ok(())
    })?;

    tracing::debug!(
        count = tombstones.len(),
        "Restored tombstones atomically after rebuild and marked them dirty for export"
    );
    Ok(())
}

/// Per-ID view of the JSONL used to decide which preserved tombstones
/// should actually be restored after a rebuild. The rebuild imports
/// everything in the JSONL first, so tombstone preservation only needs to
/// fix up rows where the local DB and the JSONL disagree.
///
/// Two buckets:
///
/// - `tombstone_ids`: IDs whose JSONL record carries `status = tombstone`.
///   The deletion has already been flushed, so the rebuild will reimport it
///   as a tombstone on its own — we drop any local preserved tombstone for
///   these IDs.
///
/// - `non_tombstone_updated_at`: IDs whose JSONL record carries a *non*-
///   tombstone status, mapped to the record's `updated_at`. When the local
///   DB has one of these IDs as a tombstone, there is a disagreement: the
///   JSONL says the issue is alive, the DB says it's deleted. Import and
///   rebuild paths must keep the tombstone; reopening is a separate,
///   explicit user action.
#[derive(Debug, Clone, Default)]
pub(crate) struct JsonlTombstoneFilter {
    pub(crate) tombstone_ids: HashSet<String>,
    pub(crate) non_tombstone_updated_at:
        std::collections::HashMap<String, chrono::DateTime<chrono::Utc>>,
}

/// Filter the preserved tombstone set down to those that should actually
/// be restored after the rebuild has reimported the JSONL. Three cases:
///
/// 1. JSONL has this ID as a tombstone: drop from preservation set — the
///    rebuild's own `import_from_jsonl` will reinstate the tombstone.
///
/// 2. JSONL has this ID as a non-tombstone: preserve the local tombstone
///    and restore it after import. This mirrors the normal
///    `import_from_jsonl` tombstone guard, which rejects resurrection even
///    when force-upsert is enabled. Timestamp ordering cannot make a
///    deleted issue live again; the operator must reopen it explicitly.
///
/// 3. JSONL doesn't have this ID at all: the deletion has never been
///    flushed anywhere. Always preserve — otherwise this path would
///    silently lose the local delete.
#[must_use]
pub(crate) fn tombstones_missing_from_jsonl_tombstones(
    tombstones: Vec<PreservedTombstone>,
    jsonl_filter: &JsonlTombstoneFilter,
) -> Vec<PreservedTombstone> {
    let original_count = tombstones.len();
    let mut skipped_already_flushed = 0usize;
    let mut preserved_non_tombstone_conflicts = 0usize;
    let preserved: Vec<PreservedTombstone> = tombstones
        .into_iter()
        .filter(|tombstone| {
            let id = &tombstone.issue.id;
            if jsonl_filter.tombstone_ids.contains(id) {
                skipped_already_flushed += 1;
                return false;
            }
            if jsonl_filter.non_tombstone_updated_at.contains_key(id) {
                preserved_non_tombstone_conflicts += 1;
            }
            true
        })
        .collect();

    if skipped_already_flushed > 0 || preserved_non_tombstone_conflicts > 0 {
        tracing::debug!(
            preserved = preserved.len(),
            skipped_already_flushed,
            preserved_non_tombstone_conflicts,
            original = original_count,
            "Filtered preserved tombstones against JSONL state"
        );
    }

    preserved
}

/// Scan the JSONL once and build a `JsonlTombstoneFilter` we can use to
/// decide which preserved tombstones to restore after a rebuild.
///
/// # Errors
///
/// Returns an error if the JSONL cannot be read, contains invalid JSON, or
/// has duplicate IDs across lines.
pub(crate) fn scan_jsonl_for_tombstone_filter(path: &Path) -> Result<JsonlTombstoneFilter> {
    let file = File::open(path)?;
    let mut reader = BufReader::new(file);
    let mut line_buf = String::new();
    let mut line_num = 0;
    let mut seen_ids = HashSet::new();
    let mut filter = JsonlTombstoneFilter::default();

    loop {
        line_buf.clear();
        let bytes = reader.read_line(&mut line_buf)?;
        if bytes == 0 {
            break;
        }

        line_num += 1;
        let trimmed = line_buf.trim_end_matches(['\n', '\r']);
        if trimmed.trim().is_empty() {
            continue;
        }

        let issue: Issue = serde_json::from_str(trimmed)
            .map_err(|e| BeadsError::Config(format!("Invalid JSON at line {}: {}", line_num, e)))?;

        if !seen_ids.insert(issue.id.clone()) {
            return Err(BeadsError::Config(format!(
                "Duplicate issue id '{}' in {} at line {}",
                issue.id,
                path.display(),
                line_num
            )));
        }

        if issue.status == crate::model::Status::Tombstone {
            filter.tombstone_ids.insert(issue.id);
        } else {
            filter
                .non_tombstone_updated_at
                .insert(issue.id, issue.updated_at);
        }
    }

    Ok(filter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Comment, Issue, IssueType, Priority, Status};
    use chrono::Utc;
    use fsqlite_types::SqliteValue;
    use std::collections::HashMap;
    use std::io::{self, Write};
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    fn make_test_issue(id: &str, title: &str) -> Issue {
        Issue {
            id: id.to_string(),
            content_hash: None,
            title: title.to_string(),
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
            created_at: Utc::now(),
            created_by: None,
            updated_at: Utc::now(),
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
        }
    }

    #[test]
    fn blocking_write_lock_errors_when_lock_path_cannot_open() {
        let temp_dir = TempDir::new().unwrap();
        let beads_dir = temp_dir.path().join(".beads");
        fs::create_dir_all(beads_dir.join(".write.lock")).unwrap();

        let err = blocking_write_lock(&beads_dir).unwrap_err();
        assert!(
            matches!(
                &err,
                BeadsError::Config(message)
                    if message.contains("Failed to open write lock")
                        && message.contains(".write.lock")
            ),
            "unexpected error: {err}"
        );
    }

    #[test]
    #[allow(clippy::incompatible_msrv)]
    fn blocking_write_lock_with_timeout_errors_when_lock_is_held() {
        let temp_dir = TempDir::new().unwrap();
        let beads_dir = temp_dir.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let lock_path = beads_dir.join(".write.lock");
        let held_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .expect("open held write lock");
        held_lock.lock().expect("hold write lock");

        let start = Instant::now();
        let err = blocking_write_lock_with_timeout(&beads_dir, Some(25)).unwrap_err();
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "timeout should fail promptly"
        );
        assert!(
            matches!(
                &err,
                BeadsError::Config(message)
                    if message.contains("Timed out after 25ms")
                        && message.contains(".write.lock")
                        && message.contains("stuck process")
            ),
            "unexpected error: {err}"
        );

        drop(held_lock);
        let acquired =
            blocking_write_lock_with_timeout(&beads_dir, Some(25)).expect("lock after release");
        drop(acquired);
    }

    #[test]
    fn try_sync_lock_errors_when_lock_path_cannot_open() {
        let temp_dir = TempDir::new().unwrap();
        let beads_dir = temp_dir.path().join(".beads");
        fs::create_dir_all(beads_dir.join(".sync.lock")).unwrap();

        let err = try_sync_lock(&beads_dir).unwrap_err();
        assert!(
            matches!(
                &err,
                BeadsError::Config(message)
                    if message.contains("Failed to open sync lock")
                        && message.contains(".sync.lock")
            ),
            "unexpected error: {err}"
        );
    }

    #[test]
    #[allow(clippy::incompatible_msrv)]
    fn try_sync_lock_returns_none_when_lock_is_held() {
        let temp_dir = TempDir::new().unwrap();
        let beads_dir = temp_dir.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let lock_path = beads_dir.join(".sync.lock");
        let held_lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .expect("open held sync lock");
        held_lock.lock().expect("hold sync lock");

        assert!(try_sync_lock(&beads_dir).unwrap().is_none());

        drop(held_lock);
        let acquired = try_sync_lock(&beads_dir)
            .expect("sync lock after release")
            .expect("uncontended lock should be acquired");
        drop(acquired);
    }

    #[test]
    fn export_temp_path_is_pid_scoped_and_sibling_to_target() {
        let target = Path::new("/tmp/issues.jsonl");
        let temp = export_temp_path(target);

        assert_eq!(temp.parent(), target.parent());
        assert_ne!(temp, target);
        assert!(
            temp.display()
                .to_string()
                .contains(&std::process::id().to_string())
        );
        assert!(temp.extension().is_some_and(|ext| ext == "tmp"));
    }

    fn make_issue_at(id: &str, title: &str, updated_at: chrono::DateTime<Utc>) -> Issue {
        let created_at = updated_at - chrono::Duration::seconds(60);
        Issue {
            id: id.to_string(),
            content_hash: None,
            title: title.to_string(),
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
            created_at,
            created_by: None,
            updated_at,
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
        }
    }

    fn set_content_hash(issue: &mut Issue) {
        issue.content_hash = Some(crate::util::content_hash(issue));
    }

    fn fixed_time(secs: i64) -> chrono::DateTime<Utc> {
        chrono::DateTime::from_timestamp(secs, 0).expect("timestamp")
    }

    fn build_collision_maps(
        storage: &SqliteStorage,
    ) -> (
        HashMap<String, String>,
        HashMap<String, String>,
        HashMap<String, crate::storage::sqlite::IssueMetadata>,
    ) {
        let all_meta = storage.get_all_issues_metadata().unwrap();
        let mut meta_by_id = HashMap::new();
        let mut id_by_ext_ref = HashMap::new();
        let mut id_by_hash = HashMap::new();

        for meta in all_meta {
            let issue_id = meta.id.clone();
            if let Some(ext) = meta.external_ref.as_ref() {
                id_by_ext_ref
                    .entry(ext.clone())
                    .or_insert_with(|| issue_id.clone());
            }
            if let Some(hash) = meta.content_hash.as_ref() {
                id_by_hash
                    .entry(hash.clone())
                    .or_insert_with(|| issue_id.clone());
            }
            meta_by_id.insert(issue_id, meta);
        }

        (id_by_ext_ref, id_by_hash, meta_by_id)
    }

    struct LineFailWriter {
        buffer: Vec<u8>,
        current: Vec<u8>,
        fail_on: String,
        failed: bool,
    }

    impl LineFailWriter {
        fn new(fail_on: &str) -> Self {
            Self {
                buffer: Vec::new(),
                current: Vec::new(),
                fail_on: fail_on.to_string(),
                failed: false,
            }
        }

        fn into_string(self) -> String {
            String::from_utf8(self.buffer).unwrap_or_default()
        }
    }

    impl Write for LineFailWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.current.extend_from_slice(buf);
            while let Some(pos) = self.current.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = self.current.drain(..=pos).collect();
                let line_str = String::from_utf8_lossy(&line);
                if !self.failed && line_str.contains(&self.fail_on) {
                    self.failed = true;
                    return Err(io::Error::other("intentional failure"));
                }
                self.buffer.extend_from_slice(&line);
            }
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn test_scan_conflict_markers_detects_all_kinds() {
        let temp = TempDir::new().expect("tempdir");
        let path = temp.path().join("issues.jsonl");
        let contents = "\
{\"id\":\"bd-1\",\"title\":\"ok\"}
<<<<<<< HEAD
{\"id\":\"bd-2\",\"title\":\"conflict\"}
=======
{\"id\":\"bd-2\",\"title\":\"other\"}
>>>>>>> feature-branch
";
        fs::write(&path, contents).expect("write");

        let markers = scan_conflict_markers(&path).expect("scan");
        assert_eq!(markers.len(), 3);
        assert_eq!(markers[0].marker_type, ConflictMarkerType::Start);
        assert_eq!(markers[1].marker_type, ConflictMarkerType::Separator);
        assert_eq!(markers[2].marker_type, ConflictMarkerType::End);
        assert_eq!(markers[0].branch.as_deref(), Some("HEAD"));
        assert_eq!(markers[2].branch.as_deref(), Some("feature-branch"));
    }

    #[test]
    fn test_ensure_no_conflict_markers_errors() {
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("issues.jsonl");
        fs::write(&path, "<<<<<<< HEAD\n").expect("write");

        let err = ensure_no_conflict_markers(&path).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("Merge conflict markers detected"));
    }

    #[test]
    fn test_export_empty_database() {
        let storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("issues.jsonl");

        let config = ExportConfig::default();
        let result = export_to_jsonl(&storage, &output_path, &config).unwrap();

        assert_eq!(result.exported_count, 0);
        assert!(result.exported_ids.is_empty());
        assert!(output_path.exists());
    }

    #[test]
    fn test_save_base_snapshot_skips_stale_regular_temp_file() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let snapshot_path = beads_dir.join("beads.base.jsonl");
        fs::write(&snapshot_path, "old-snapshot\n").unwrap();
        let stale_temp_path = export_temp_path_for_attempt(&snapshot_path, 0);
        let retry_temp_path = export_temp_path_for_attempt(&snapshot_path, 1);
        fs::write(&stale_temp_path, "stale temp\n").unwrap();

        let mut issues = HashMap::new();
        issues.insert(
            "bd-base".to_string(),
            Issue {
                id: "bd-base".to_string(),
                title: "New base snapshot".to_string(),
                ..Issue::default()
            },
        );

        save_base_snapshot(&issues, &beads_dir).unwrap();

        let snapshot = fs::read_to_string(&snapshot_path).unwrap();
        assert!(
            snapshot.contains("\"id\":\"bd-base\""),
            "base snapshot should be rewritten with the requested issue: {snapshot}"
        );
        assert_eq!(
            fs::read_to_string(&stale_temp_path).unwrap(),
            "stale temp\n",
            "stale regular temp file should be left untouched"
        );
        assert!(
            !retry_temp_path.exists(),
            "successful retry temp path should be renamed away"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_save_base_snapshot_rejects_existing_temp_symlink() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let outside_dir = temp.path().join("outside");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&outside_dir).unwrap();

        let snapshot_path = beads_dir.join("beads.base.jsonl");
        fs::write(&snapshot_path, "old-snapshot\n").unwrap();

        let temp_target = outside_dir.join("captured.txt");
        fs::write(&temp_target, "do-not-touch").unwrap();
        let pid = std::process::id();
        symlink(
            &temp_target,
            beads_dir.join(format!("beads.base.jsonl.{pid}.tmp")),
        )
        .unwrap();

        let mut issues = HashMap::new();
        issues.insert(
            "bd-base".to_string(),
            Issue {
                id: "bd-base".to_string(),
                title: "New base snapshot".to_string(),
                ..Issue::default()
            },
        );

        let err = save_base_snapshot(&issues, &beads_dir).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("regular file")
                || message.contains("Temporary base snapshot file")
                || message.contains("Symlink")
                || message.contains("Path"),
            "unexpected error: {message}"
        );
        assert_eq!(
            fs::read_to_string(&snapshot_path).unwrap(),
            "old-snapshot\n",
            "existing base snapshot should remain unchanged on failure"
        );
        assert_eq!(
            fs::read_to_string(&temp_target).unwrap(),
            "do-not-touch",
            "symlink target should not be overwritten"
        );
    }

    #[test]
    fn test_save_base_snapshot_sorts_issues_deterministically() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let mut issues = HashMap::new();
        issues.insert(
            "bd-z".to_string(),
            Issue {
                id: "bd-z".to_string(),
                title: "Last".to_string(),
                ..Issue::default()
            },
        );
        issues.insert(
            "bd-a".to_string(),
            Issue {
                id: "bd-a".to_string(),
                title: "First".to_string(),
                ..Issue::default()
            },
        );

        save_base_snapshot(&issues, &beads_dir).unwrap();

        let lines: Vec<_> = fs::read_to_string(beads_dir.join("beads.base.jsonl"))
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(lines.len(), 2);

        let first: Issue = serde_json::from_str(&lines[0]).unwrap();
        let second: Issue = serde_json::from_str(&lines[1]).unwrap();
        assert_eq!(first.id, "bd-a");
        assert_eq!(second.id, "bd-z");
    }

    #[test]
    fn test_save_base_snapshot_from_jsonl_uses_finalized_export_contents() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let jsonl_path = beads_dir.join("issues.jsonl");
        let issue = Issue {
            id: "bd-final".to_string(),
            title: "Finalized".to_string(),
            comments: vec![Comment {
                id: 1,
                issue_id: "bd-final".to_string(),
                author: "br-sync".to_string(),
                body: "merge note written after report".to_string(),
                created_at: Utc::now(),
            }],
            ..Issue::default()
        };
        fs::write(
            &jsonl_path,
            format!("{}\n", serde_json::to_string(&issue).unwrap()),
        )
        .unwrap();

        save_base_snapshot_from_jsonl(&jsonl_path, &beads_dir).unwrap();

        let base = load_base_snapshot(&beads_dir).unwrap();
        let saved = base.get("bd-final").expect("saved base issue");
        assert_eq!(saved.comments.len(), 1);
        assert_eq!(saved.comments[0].body, "merge note written after report");
    }

    #[cfg(unix)]
    #[test]
    fn test_load_base_snapshot_rejects_symlink_escape() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let outside_dir = temp.path().join("outside");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&outside_dir).unwrap();

        let outside_snapshot = outside_dir.join("beads.base.jsonl");
        fs::write(&outside_snapshot, "{\"id\":\"bd-outside\"}\n").unwrap();
        symlink(&outside_snapshot, beads_dir.join("beads.base.jsonl")).unwrap();

        let err = load_base_snapshot(&beads_dir).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("symlink") || message.contains("Symlink") || message.contains("Path"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn test_export_with_issues() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("issues.jsonl");

        // Create test issues
        let issue1 = make_test_issue("bd-001", "First issue");
        let issue2 = make_test_issue("bd-002", "Second issue");

        storage.create_issue(&issue1, "test").unwrap();
        storage.create_issue(&issue2, "test").unwrap();

        let config = ExportConfig::default();
        let result = export_to_jsonl(&storage, &output_path, &config).unwrap();

        assert_eq!(result.exported_count, 2);
        assert!(result.exported_ids.contains(&"bd-001".to_string()));
        assert!(result.exported_ids.contains(&"bd-002".to_string()));

        // Verify content
        let read_back = read_issues_from_jsonl(&output_path).unwrap();
        assert_eq!(read_back.len(), 2);
        assert_eq!(read_back[0].id, "bd-001");
        assert_eq!(read_back[1].id, "bd-002");
    }

    #[test]
    fn test_safety_guard_empty_over_nonempty() {
        let storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("issues.jsonl");

        // Create existing JSONL with issues
        let issue = make_test_issue("bd-existing", "Existing issue");
        let json = serde_json::to_string(&issue).unwrap();
        fs::write(&output_path, format!("{json}\n")).unwrap();

        // Try to export empty database (should fail)
        let config = ExportConfig {
            force: false,
            ..Default::default()
        };
        let result = export_to_jsonl(&storage, &output_path, &config);

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("empty database"));
    }

    #[test]
    fn test_safety_guard_with_force() {
        let storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("issues.jsonl");

        // Create existing JSONL with issues
        let issue = make_test_issue("bd-existing", "Existing issue");
        let json = serde_json::to_string(&issue).unwrap();
        fs::write(&output_path, format!("{json}\n")).unwrap();

        // Export with force (should succeed)
        let config = ExportConfig {
            force: true,
            ..Default::default()
        };
        let result = export_to_jsonl(&storage, &output_path, &config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_count_issues_in_jsonl() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("test.jsonl");

        // Empty file
        fs::write(&path, "").unwrap();
        assert_eq!(count_issues_in_jsonl(&path).unwrap(), 0);

        // Two issues
        let issue1 = make_test_issue("bd-001", "One");
        let issue2 = make_test_issue("bd-002", "Two");
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&issue1).unwrap(),
            serde_json::to_string(&issue2).unwrap()
        );
        fs::write(&path, content).unwrap();
        assert_eq!(count_issues_in_jsonl(&path).unwrap(), 2);
    }

    #[test]
    fn test_get_issue_ids_from_jsonl() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("test.jsonl");

        let issue1 = make_test_issue("bd-001", "One");
        let issue2 = make_test_issue("bd-002", "Two");
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&issue1).unwrap(),
            serde_json::to_string(&issue2).unwrap()
        );
        fs::write(&path, content).unwrap();

        let ids = get_issue_ids_from_jsonl(&path).unwrap();
        assert_eq!(ids.len(), 2);
        assert!(ids.contains("bd-001"));
        assert!(ids.contains("bd-002"));
    }

    #[test]
    fn test_analyze_jsonl_rejects_duplicate_issue_ids() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("duplicate-ids.jsonl");

        let issue1 = make_test_issue("bd-dup", "Original");
        let issue2 = make_test_issue("bd-dup", "Duplicate");
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&issue1).unwrap(),
            serde_json::to_string(&issue2).unwrap()
        );
        fs::write(&path, content).unwrap();

        let err = analyze_jsonl(&path).unwrap_err();
        assert!(
            matches!(
                &err,
                BeadsError::Config(message)
                    if message.contains("Duplicate issue id 'bd-dup'")
            ),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn test_verify_exported_jsonl_integrity_rejects_corruption_shapes() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("corrupt-export.jsonl");

        let issue1 = make_test_issue("bd-001", "One");
        let issue2 = make_test_issue("bd-002", "Two");
        let json1 = serde_json::to_string(&issue1).unwrap();
        let json2 = serde_json::to_string(&issue2).unwrap();

        let cases = [
            ("collapsed adjacent records", format!("{json1}{json2}\n")),
            ("stray issue prefix", "{\"i{\"id\":\"bd-001\"}\n".to_string()),
            (
                "missing comments array closure",
                "{\"id\":\"bd-001\",\"title\":\"Broken\",\"comments\":[{\"id\":1}\n".to_string(),
            ),
            (
                "object nested in numeric field",
                "{\"id\":\"bd-001\",\"title\":\"Broken\",\"original_size\":{\"id\":\"bd-002\"}}\n"
                    .to_string(),
            ),
        ];

        for (name, content) in cases {
            fs::write(&path, content).unwrap();

            let err = verify_exported_jsonl_integrity(
                &path,
                &["bd-001".to_string(), "bd-002".to_string()],
            )
            .unwrap_err()
            .to_string();
            assert!(
                err.contains("invalid exported JSON at line 1"),
                "{name}: unexpected error: {err}"
            );
        }
    }

    #[test]
    fn test_verify_exported_jsonl_integrity_rejects_missing_expected_issue() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("missing-export.jsonl");

        let issue = make_test_issue("bd-001", "One");
        fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&issue).unwrap()),
        )
        .unwrap();

        let err = verify_exported_jsonl_integrity(
            &path,
            &["bd-001".to_string(), "bd-002".to_string()],
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("expected 2 issues, JSONL has 1 valid issue lines"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_verify_exported_jsonl_integrity_rejects_unexpected_issue() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("unexpected-export.jsonl");

        let issue = make_test_issue("bd-other", "Other");
        fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&issue).unwrap()),
        )
        .unwrap();

        let err = verify_exported_jsonl_integrity(&path, &["bd-001".to_string()])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("unexpected issue id 'bd-other' at line 1"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_export_excludes_ephemerals() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("issues.jsonl");

        // Create regular and ephemeral issues
        let regular = make_test_issue("bd-regular", "Regular issue");
        let mut ephemeral = make_test_issue("bd-ephemeral", "Ephemeral issue");
        ephemeral.ephemeral = true;

        storage.create_issue(&regular, "test").unwrap();
        storage.create_issue(&ephemeral, "test").unwrap();

        let config = ExportConfig::default();
        let result = export_to_jsonl(&storage, &output_path, &config).unwrap();

        // Only regular issue should be exported
        assert_eq!(result.exported_count, 1);
        assert!(result.exported_ids.contains(&"bd-regular".to_string()));
        assert!(!result.exported_ids.contains(&"bd-ephemeral".to_string()));
    }

    #[test]
    fn test_stale_database_guard_prevents_losing_issues() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("issues.jsonl");

        // Create a JSONL with two issues
        let issue1 = make_test_issue("bd-001", "First");
        let issue2 = make_test_issue("bd-002", "Second");
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&issue1).unwrap(),
            serde_json::to_string(&issue2).unwrap()
        );
        fs::write(&output_path, content).unwrap();

        // Only create one issue in DB (missing bd-002)
        storage.create_issue(&issue1, "test").unwrap();

        // Export should fail because it would lose bd-002
        let config = ExportConfig::default();
        let result = export_to_jsonl(&storage, &output_path, &config);

        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("stale database") || err.contains("lose"));
    }

    #[test]
    fn test_stale_database_guard_with_force_succeeds() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("issues.jsonl");

        // Create a JSONL with two issues
        let issue1 = make_test_issue("bd-001", "First");
        let issue2 = make_test_issue("bd-002", "Second");
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&issue1).unwrap(),
            serde_json::to_string(&issue2).unwrap()
        );
        fs::write(&output_path, content).unwrap();

        // Only create one issue in DB
        storage.create_issue(&issue1, "test").unwrap();

        // Export with force should succeed
        let config = ExportConfig {
            force: true,
            ..Default::default()
        };
        let result = export_to_jsonl(&storage, &output_path, &config);
        assert!(result.is_ok());
    }

    #[test]
    fn test_auto_import_if_stale_skips_probe_for_allow_stale() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let beads_dir = temp_dir.path().join(".beads");
        let jsonl_path = beads_dir.join("issues.jsonl");

        fs::create_dir_all(&beads_dir).unwrap();
        fs::write(&jsonl_path, [0xFF_u8, b'\n']).unwrap();
        storage
            .set_metadata(METADATA_JSONL_CONTENT_HASH, "stale-hash")
            .unwrap();

        let result = auto_import_if_stale(
            &mut storage,
            &beads_dir,
            &jsonl_path,
            None,
            false,
            true,
            false,
        )
        .unwrap();
        assert!(!result.attempted);
        assert_eq!(result.imported_count, 0);
    }

    #[test]
    fn test_auto_import_if_stale_skips_probe_for_no_auto_import() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let beads_dir = temp_dir.path().join(".beads");
        let jsonl_path = beads_dir.join("issues.jsonl");

        fs::create_dir_all(&beads_dir).unwrap();
        fs::write(&jsonl_path, [0xFF_u8, b'\n']).unwrap();
        storage
            .set_metadata(METADATA_JSONL_CONTENT_HASH, "stale-hash")
            .unwrap();

        let result = auto_import_if_stale(
            &mut storage,
            &beads_dir,
            &jsonl_path,
            None,
            false,
            false,
            true,
        )
        .unwrap();
        assert!(!result.attempted);
        assert_eq!(result.imported_count, 0);
    }

    #[test]
    fn test_auto_import_probe_validates_external_path_before_hashing() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let beads_dir = temp_dir.path().join(".beads");
        let external_jsonl = temp_dir.path().join("external").join("issues.jsonl");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&external_jsonl).unwrap();
        storage
            .set_metadata(METADATA_JSONL_CONTENT_HASH, "stale-hash")
            .unwrap();

        let err = auto_import_probe(&storage, &beads_dir, &external_jsonl, false).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("outside the beads directory")
                || message.contains("must be a regular file"),
            "unexpected error: {err}"
        );

        let err = auto_import_probe_refreshing_witnesses(
            &mut storage,
            &beads_dir,
            &external_jsonl,
            false,
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("outside the beads directory")
                || message.contains("must be a regular file"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_auto_import_if_stale_validates_external_path_before_hashing() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let beads_dir = temp_dir.path().join(".beads");
        let external_jsonl = temp_dir.path().join("external").join("issues.jsonl");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&external_jsonl).unwrap();
        storage
            .set_metadata(METADATA_JSONL_CONTENT_HASH, "stale-hash")
            .unwrap();

        let err = auto_import_if_stale(
            &mut storage,
            &beads_dir,
            &external_jsonl,
            None,
            false,
            false,
            false,
        )
        .unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains("outside the beads directory")
                || message.contains("must be a regular file"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_compute_staleness_uses_matching_jsonl_mtime_witness() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let jsonl_path = temp_dir.path().join("issues.jsonl");

        fs::write(&jsonl_path, "{\"id\":\"bd-1\"}\n").unwrap();
        let (_, jsonl_mtime_witness) = observed_jsonl_mtime(&jsonl_path).unwrap();
        let current_hash = compute_jsonl_hash(&jsonl_path).unwrap();

        storage
            .set_metadata(METADATA_JSONL_CONTENT_HASH, &current_hash)
            .unwrap();
        storage
            .set_metadata(METADATA_JSONL_MTIME, &jsonl_mtime_witness)
            .unwrap();

        let staleness = compute_staleness(&storage, &jsonl_path).unwrap();
        assert!(staleness.jsonl_exists);
        assert!(!staleness.jsonl_newer);
        assert!(staleness.jsonl_mtime.is_some());
    }

    #[test]
    fn test_compute_staleness_does_not_trust_matching_mtime_without_hash_match() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let jsonl_path = temp_dir.path().join("issues.jsonl");

        fs::write(&jsonl_path, "{\"id\":\"bd-1\"}\n").unwrap();
        let (_, jsonl_mtime_witness) = observed_jsonl_mtime(&jsonl_path).unwrap();

        storage
            .set_metadata(METADATA_JSONL_CONTENT_HASH, "stale-hash")
            .unwrap();
        storage
            .set_metadata(METADATA_JSONL_MTIME, &jsonl_mtime_witness)
            .unwrap();

        let staleness = compute_staleness(&storage, &jsonl_path).unwrap();
        assert!(staleness.jsonl_exists);
        assert!(staleness.jsonl_newer);
        assert!(staleness.jsonl_mtime.is_some());
    }

    #[test]
    fn test_compute_staleness_refreshing_witnesses_backfills_jsonl_size() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let jsonl_path = temp_dir.path().join("issues.jsonl");

        fs::write(&jsonl_path, "{\"id\":\"bd-1\"}\n").unwrap();
        let (_, jsonl_mtime_witness) = observed_jsonl_mtime(&jsonl_path).unwrap();
        let current_hash = compute_jsonl_hash(&jsonl_path).unwrap();
        let jsonl_size = fs::metadata(&jsonl_path).unwrap().len().to_string();

        storage
            .set_metadata(METADATA_JSONL_CONTENT_HASH, &current_hash)
            .unwrap();
        storage
            .set_metadata(METADATA_JSONL_MTIME, &jsonl_mtime_witness)
            .unwrap();

        let staleness = compute_staleness_refreshing_witnesses(&mut storage, &jsonl_path).unwrap();
        assert!(!staleness.jsonl_newer);
        assert_eq!(
            storage.get_metadata(METADATA_JSONL_SIZE).unwrap(),
            Some(jsonl_size)
        );
    }

    #[test]
    fn test_refresh_jsonl_witness_best_effort_ignores_missing_jsonl() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let jsonl_path = temp_dir.path().join("issues.jsonl");

        fs::write(&jsonl_path, "{\"id\":\"bd-1\"}\n").unwrap();
        let observed = observed_jsonl_witness(&jsonl_path).unwrap();
        fs::remove_file(&jsonl_path).unwrap();

        refresh_jsonl_witness_best_effort(&mut storage, &jsonl_path, &observed);

        assert_eq!(storage.get_metadata(METADATA_JSONL_MTIME).unwrap(), None);
        assert_eq!(storage.get_metadata(METADATA_JSONL_SIZE).unwrap(), None);
    }

    #[test]
    fn test_compute_staleness_marks_db_newer_when_force_flush_is_pending() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let jsonl_path = temp_dir.path().join("issues.jsonl");

        storage.set_metadata("needs_flush", "true").unwrap();

        let staleness = compute_staleness(&storage, &jsonl_path).unwrap();
        assert!(staleness.db_newer);
        assert_eq!(staleness.dirty_count, 0);
    }

    #[test]
    fn test_compute_staleness_marks_db_newer_when_jsonl_is_missing_but_db_has_issues() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let jsonl_path = temp_dir.path().join("issues.jsonl");
        let issue = make_test_issue("bd-missing-jsonl", "DB only");
        storage.create_issue(&issue, "tester").unwrap();

        let staleness = compute_staleness(&storage, &jsonl_path).unwrap();
        assert!(staleness.db_newer);
        assert!(!staleness.jsonl_exists);
    }

    #[test]
    fn test_auto_flush_propagates_jsonl_scan_io_errors() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let beads_dir = temp_dir.path().join(".beads");
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::create_dir_all(&jsonl_path).unwrap();

        let issue = make_test_issue("bd-scan-error", "Dirty issue");
        storage.create_issue(&issue, "tester").unwrap();

        let err = auto_flush(&mut storage, &beads_dir, &jsonl_path, false).unwrap_err();
        assert!(
            err.to_string().contains("directory")
                || err.to_string().contains("Is a directory")
                || err.to_string().contains("not a regular file")
                || err.to_string().contains("must be a regular file"),
            "unexpected error: {err}"
        );
        assert_eq!(
            storage.get_dirty_issue_ids().unwrap(),
            vec!["bd-scan-error".to_string()],
            "failed auto-flush must leave dirty markers intact"
        );
    }

    #[test]
    fn test_auto_flush_validates_path_before_reading_existing_jsonl() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let beads_dir = temp_dir.path().join(".beads");
        let outside_jsonl_path = temp_dir.path().join("outside.jsonl");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::write(&outside_jsonl_path, "<<<<<<< HEAD\n").unwrap();

        let issue = make_test_issue("bd-auto-flush-path", "Dirty issue");
        storage.create_issue(&issue, "tester").unwrap();

        let err = auto_flush(&mut storage, &beads_dir, &outside_jsonl_path, false).unwrap_err();
        assert!(
            err.to_string().contains("outside the beads directory"),
            "unexpected error: {err}"
        );
        assert_eq!(
            storage.get_dirty_issue_ids().unwrap(),
            vec!["bd-auto-flush-path".to_string()],
            "rejected auto-flush must leave dirty markers intact"
        );
    }

    #[test]
    fn test_import_records_matching_jsonl_mtime_witness() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let jsonl_path = temp_dir.path().join("issues.jsonl");

        let issue = make_test_issue("bd-import", "Imported issue");
        let json = serde_json::to_string(&issue).unwrap();
        fs::write(&jsonl_path, format!("{json}\n")).unwrap();

        import_from_jsonl(
            &mut storage,
            &jsonl_path,
            &ImportConfig::default(),
            Some("bd-"),
        )
        .unwrap();

        let (_, jsonl_mtime_witness) = observed_jsonl_mtime(&jsonl_path).unwrap();
        let jsonl_size = fs::metadata(&jsonl_path).unwrap().len().to_string();
        assert_eq!(
            storage.get_metadata(METADATA_JSONL_MTIME).unwrap(),
            Some(jsonl_mtime_witness)
        );
        assert_eq!(
            storage.get_metadata(METADATA_JSONL_SIZE).unwrap(),
            Some(jsonl_size)
        );
    }

    #[test]
    fn test_import_skips_child_counters_for_missing_parents() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let jsonl_path = temp_dir.path().join("issues.jsonl");

        let orphan_child = make_test_issue("bd-orphan.6", "Recovered orphan child");
        let json = serde_json::to_string(&orphan_child).unwrap();
        fs::write(&jsonl_path, format!("{json}\n")).unwrap();

        import_from_jsonl(
            &mut storage,
            &jsonl_path,
            &ImportConfig::default(),
            Some("bd-"),
        )
        .unwrap();

        let child_counters = storage
            .execute_raw_query("SELECT parent_id FROM child_counters")
            .unwrap();
        assert!(
            child_counters.is_empty(),
            "orphan child IDs should not rebuild counters for missing parents"
        );
        assert!(
            !storage
                .has_missing_issue_reference("child_counters", "parent_id")
                .unwrap(),
            "child counters must remain free of FK orphans after import"
        );
    }

    #[test]
    fn test_import_rebuilds_nested_child_counters_only_for_existing_parents() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let jsonl_path = temp_dir.path().join("issues.jsonl");

        let orphan_child = make_test_issue("bd-orphan.6", "Recovered orphan child");
        let nested_child = make_test_issue("bd-orphan.6.1", "Recovered nested child");
        let orphan_json = serde_json::to_string(&orphan_child).unwrap();
        let nested_json = serde_json::to_string(&nested_child).unwrap();
        fs::write(&jsonl_path, format!("{orphan_json}\n{nested_json}\n")).unwrap();

        import_from_jsonl(
            &mut storage,
            &jsonl_path,
            &ImportConfig::default(),
            Some("bd-"),
        )
        .unwrap();

        let child_counters = storage
            .execute_raw_query(
                "SELECT parent_id, last_child FROM child_counters ORDER BY parent_id",
            )
            .unwrap();
        assert_eq!(
            child_counters.len(),
            1,
            "only the existing intermediate parent should get a counter"
        );
        assert_eq!(
            child_counters[0]
                .first()
                .and_then(SqliteValue::as_text)
                .unwrap_or(""),
            "bd-orphan.6"
        );
        assert_eq!(
            child_counters[0]
                .get(1)
                .and_then(SqliteValue::as_integer)
                .unwrap_or_default(),
            1
        );
        assert!(
            !storage
                .has_missing_issue_reference("child_counters", "parent_id")
                .unwrap(),
            "nested rebuild should not recreate orphan counters for missing roots"
        );
    }

    #[test]
    fn test_normalize_issue_wisp_detection() {
        let mut issue = make_test_issue("bd-wisp-123", "Wisp issue");
        assert!(!issue.ephemeral);

        normalize_issue(&mut issue);

        // Issue ID containing "-wisp-" should be marked ephemeral
        assert!(issue.ephemeral);
    }

    #[test]
    fn test_normalize_issue_closed_at_repair() {
        let mut issue = make_test_issue("bd-001", "Closed issue");
        issue.status = Status::Closed;
        issue.closed_at = None;

        normalize_issue(&mut issue);

        // closed_at should be set to updated_at for closed issues
        assert!(issue.closed_at.is_some());
        assert_eq!(issue.closed_at, Some(issue.updated_at));
    }

    #[test]
    fn test_normalize_issue_clears_closed_at_for_open() {
        let mut issue = make_test_issue("bd-001", "Open issue");
        issue.status = Status::Open;
        issue.closed_at = Some(Utc::now());

        normalize_issue(&mut issue);

        // closed_at should be cleared for open issues
        assert!(issue.closed_at.is_none());
    }

    #[test]
    fn test_normalize_issue_computes_content_hash() {
        let mut issue = make_test_issue("bd-001", "Test");
        issue.content_hash = None;

        normalize_issue(&mut issue);

        assert!(issue.content_hash.is_some());
        assert!(!issue.content_hash.as_ref().unwrap().is_empty());
    }

    #[test]
    fn test_normalize_issue_hashes_trimmed_external_ref() {
        let mut issue = make_test_issue("bd-001", "Test");
        issue.external_ref = Some("  ext-123  ".to_string());

        normalize_issue(&mut issue);

        let expected_hash = crate::util::content_hash(&issue);
        assert_eq!(issue.external_ref.as_deref(), Some("ext-123"));
        assert_eq!(issue.content_hash.as_deref(), Some(expected_hash.as_str()));
    }

    #[test]
    fn test_normalize_issue_remaps_legacy_done_to_closed() {
        // Go-beads "done" survives round-tripping as Status::Custom; ensure
        // import normalization promotes it to the canonical Closed variant
        // and that closed_at gets populated to satisfy the DB CHECK.
        let mut issue = make_test_issue("bd-001", "Legacy done");
        issue.status = Status::Custom("done".to_string());
        issue.closed_at = None;

        normalize_issue(&mut issue);

        assert_eq!(issue.status, Status::Closed);
        assert!(issue.closed_at.is_some());
    }

    #[test]
    fn test_normalize_issue_remaps_mixed_case_terminal_aliases() {
        for raw in ["Done", "COMPLETE", "completed", "Finished", "Resolved"] {
            let mut issue = make_test_issue("bd-001", "Legacy alias");
            issue.status = Status::Custom(raw.to_string());
            normalize_issue(&mut issue);
            assert_eq!(
                issue.status,
                Status::Closed,
                "alias {raw:?} should map to Closed"
            );
        }
    }

    #[test]
    fn test_normalize_issue_preserves_unknown_custom_status() {
        let mut issue = make_test_issue("bd-001", "Custom status");
        issue.status = Status::Custom("qa-review".to_string());
        normalize_issue(&mut issue);
        assert_eq!(issue.status, Status::Custom("qa-review".to_string()));
    }

    #[test]
    fn test_normalize_issue_normalizes_legacy_standard_dependency_type_with_underscores() {
        let mut issue = make_test_issue("bd-001", "Legacy dependency");
        issue.dependencies.push(crate::model::Dependency {
            issue_id: issue.id.clone(),
            depends_on_id: "bd-002".to_string(),
            dep_type: crate::model::DependencyType::Custom("parent_child".to_string()),
            created_at: Utc::now(),
            created_by: None,
            metadata: None,
            thread_id: None,
        });

        normalize_issue(&mut issue);

        assert_eq!(
            issue.dependencies[0].dep_type,
            crate::model::DependencyType::ParentChild
        );
    }

    #[test]
    fn test_normalize_issue_preserves_custom_dependency_type_with_underscores() {
        let mut issue = make_test_issue("bd-001", "Custom dependency");
        issue.dependencies.push(crate::model::Dependency {
            issue_id: issue.id.clone(),
            depends_on_id: "bd-002".to_string(),
            dep_type: crate::model::DependencyType::Custom("review_needed".to_string()),
            created_at: Utc::now(),
            created_by: None,
            metadata: None,
            thread_id: None,
        });

        normalize_issue(&mut issue);

        assert_eq!(
            issue.dependencies[0].dep_type,
            crate::model::DependencyType::Custom("review_needed".to_string())
        );
    }

    #[test]
    fn test_import_collision_by_id_updates_newer() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        // Create existing issue in DB with older timestamp.
        // Pin both created_at and updated_at so the validator's
        // "updated_at >= created_at" rule holds.
        let mut existing = make_test_issue("test-001", "Old title");
        existing.created_at = Utc::now() - chrono::Duration::hours(2);
        existing.updated_at = Utc::now() - chrono::Duration::hours(1);
        storage.create_issue(&existing, "test").unwrap();

        // Create JSONL with same ID but newer timestamp and new title
        let mut incoming = make_test_issue("test-001", "New title");
        incoming.updated_at = Utc::now();
        let json = serde_json::to_string(&incoming).unwrap();
        fs::write(&path, format!("{json}\n")).unwrap();

        // Import should update since incoming is newer
        let config = ImportConfig::default();
        let result = import_from_jsonl(&mut storage, &path, &config, Some("test-")).unwrap();
        assert_eq!(result.imported_count, 1);
        assert_eq!(result.created_count, 0);
        assert_eq!(result.updated_count, 1);

        // The existing issue should be updated
        let updated = storage.get_issue("test-001").unwrap().unwrap();
        assert_eq!(updated.title, "New title");
    }

    #[test]
    fn test_import_collision_by_id_skips_older() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        // Create existing issue in DB with newer timestamp
        let mut existing = make_test_issue("test-001", "Newer title");
        existing.updated_at = Utc::now();
        storage.create_issue(&existing, "test").unwrap();

        // Create JSONL with same ID but older timestamp
        let mut incoming = make_test_issue("test-001", "Older title");
        incoming.created_at = Utc::now() - chrono::Duration::hours(2); // Fix timestamp to be valid
        incoming.updated_at = Utc::now() - chrono::Duration::hours(1);
        let json = serde_json::to_string(&incoming).unwrap();
        fs::write(&path, format!("{json}\n")).unwrap();

        // Import should skip since existing is newer
        let config = ImportConfig::default();
        let result = import_from_jsonl(&mut storage, &path, &config, Some("test-")).unwrap();
        assert_eq!(result.skipped_count, 1);

        let unchanged = storage.get_issue("test-001").unwrap().unwrap();
        assert_eq!(unchanged.title, "Newer title");
    }

    #[test]
    fn test_import_tombstone_skip_marks_flush_pending() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        let mut tombstone = make_issue_at("bd-tomb", "Deleted locally", fixed_time(100));
        tombstone.status = Status::Tombstone;
        tombstone.deleted_at = Some(fixed_time(100));
        storage.create_issue(&tombstone, "test").unwrap();
        storage
            .clear_dirty_issues_legacy(&["bd-tomb".to_string()])
            .unwrap();
        storage.set_metadata("needs_flush", "false").unwrap();

        let mut incoming = make_issue_at("bd-tomb", "Remote resurrection", fixed_time(200));
        incoming.status = Status::Open;
        let json = serde_json::to_string(&incoming).unwrap();
        fs::write(&path, format!("{json}\n")).unwrap();

        let result =
            import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("bd")).unwrap();
        assert_eq!(result.tombstone_skipped, 1);
        assert_eq!(
            storage.get_metadata("needs_flush").unwrap().as_deref(),
            Some("true")
        );
        assert!(storage.get_export_hash("bd-tomb").unwrap().is_none());

        let still_tombstone = storage.get_issue("bd-tomb").unwrap().unwrap();
        assert_eq!(still_tombstone.status, Status::Tombstone);
    }

    #[test]
    fn test_import_relation_only_local_win_marks_flush_pending() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        let existing = make_issue_at("bd-rel", "Same content", fixed_time(200));
        storage.create_issue(&existing, "test").unwrap();
        storage.add_label("bd-rel", "local-only", "test").unwrap();
        storage
            .clear_dirty_issues_legacy(&["bd-rel".to_string()])
            .unwrap();
        storage.set_metadata("needs_flush", "false").unwrap();

        let incoming = make_issue_at("bd-rel", "Same content", fixed_time(100));
        let json = serde_json::to_string(&incoming).unwrap();
        fs::write(&path, format!("{json}\n")).unwrap();

        let result =
            import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("bd")).unwrap();
        assert_eq!(result.skipped_count, 1);
        assert_eq!(
            storage.get_metadata("needs_flush").unwrap().as_deref(),
            Some("true")
        );
        assert!(storage.get_export_hash("bd-rel").unwrap().is_none());
        assert_eq!(storage.get_labels("bd-rel").unwrap(), vec!["local-only"]);
    }

    #[test]
    fn test_import_collision_by_external_ref_same_id() {
        // Test collision detection by external_ref when IDs also match
        let storage = SqliteStorage::open_memory().unwrap();

        let mut ext_issue = make_issue_at("bd-ext", "External", fixed_time(100));
        ext_issue.external_ref = Some("JIRA-1".to_string());
        set_content_hash(&mut ext_issue);
        storage.upsert_issue_for_import(&ext_issue).unwrap();

        let mut hash_issue = make_issue_at("bd-hash", "Incoming", fixed_time(200));
        set_content_hash(&mut hash_issue);
        storage.upsert_issue_for_import(&hash_issue).unwrap();

        // Incoming has same external_ref as ext_issue - should match on external_ref
        // even though it has same title/content_hash as hash_issue
        let mut incoming = make_issue_at("bd-new", "Incoming", fixed_time(300));
        incoming.external_ref = Some("JIRA-1".to_string());
        let computed_hash = crate::util::content_hash(&incoming);

        let (id_by_ext_ref, id_by_hash, meta_by_id) = build_collision_maps(&storage);
        let collision = detect_collision(
            &incoming,
            &id_by_ext_ref,
            &id_by_hash,
            &meta_by_id,
            &computed_hash,
        );
        assert!(
            matches!(collision, CollisionResult::Match { .. }),
            "expected match"
        );
        if let CollisionResult::Match {
            existing_id,
            match_type,
            phase,
        } = collision
        {
            assert_eq!(existing_id, "bd-ext");
            assert_eq!(match_type, MatchType::ExternalRef);
            assert_eq!(phase, 1);
        }
    }

    #[test]
    fn test_import_tombstone_protection() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        // Create tombstone in DB
        let mut tombstone = make_issue_at("test-001", "Tombstone", fixed_time(100));
        tombstone.status = Status::Tombstone;
        tombstone.deleted_at = Some(Utc::now());
        storage.create_issue(&tombstone, "test").unwrap();

        // Create JSONL with same ID but trying to resurrect
        let mut incoming = make_issue_at("test-001", "Resurrected", fixed_time(200));
        incoming.status = Status::Open;
        let json = serde_json::to_string(&incoming).unwrap();
        fs::write(&path, format!("{json}\n")).unwrap();

        // Import should skip due to tombstone protection
        let config = ImportConfig::default();
        let result = import_from_jsonl(&mut storage, &path, &config, Some("test-")).unwrap();
        assert_eq!(result.tombstone_skipped, 1);

        let still_tombstone = storage.get_issue("test-001").unwrap().unwrap();
        assert_eq!(still_tombstone.status, Status::Tombstone);
    }

    #[test]
    fn test_import_new_issue_creates() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        // Create JSONL with new issue
        let new_issue = make_test_issue("test-new", "Brand new");
        let json = serde_json::to_string(&new_issue).unwrap();
        fs::write(&path, format!("{json}\n")).unwrap();

        let config = ImportConfig::default();
        let result = import_from_jsonl(&mut storage, &path, &config, Some("test-")).unwrap();

        // New issue should be imported
        assert_eq!(result.imported_count, 1);
        assert_eq!(result.created_count, 1);
        assert_eq!(result.updated_count, 0);
        assert_eq!(result.skipped_count, 0);
        assert!(storage.get_issue("test-new").unwrap().is_some());
    }

    #[test]
    fn test_import_stores_content_hash_after_external_ref_trim() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        let mut new_issue = make_test_issue("test-ext", "External ref trim");
        new_issue.external_ref = Some("  ext-123  ".to_string());
        let json = serde_json::to_string(&new_issue).unwrap();
        fs::write(&path, format!("{json}\n")).unwrap();

        let config = ImportConfig::default();
        let result = import_from_jsonl(&mut storage, &path, &config, Some("test-")).unwrap();

        assert_eq!(result.imported_count, 1);
        let stored = storage.get_issue("test-ext").unwrap().unwrap();
        let expected_hash = crate::util::content_hash(&stored);
        assert_eq!(stored.external_ref.as_deref(), Some("ext-123"));
        assert_eq!(stored.content_hash.as_deref(), Some(expected_hash.as_str()));
    }

    #[test]
    fn test_get_issue_ids_missing_file_returns_empty() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("nonexistent.jsonl");

        let ids = get_issue_ids_from_jsonl(&path).unwrap();
        assert!(ids.is_empty());
    }

    #[test]
    fn test_count_issues_missing_file_returns_zero() {
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("nonexistent.jsonl");

        let count = count_issues_in_jsonl(&path).unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_export_computes_content_hash() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("issues.jsonl");

        let issue = make_test_issue("bd-001", "Test");
        storage.create_issue(&issue, "test").unwrap();

        let config = ExportConfig::default();
        let result = export_to_jsonl(&storage, &output_path, &config).unwrap();

        // Result should include a non-empty content hash
        assert!(!result.content_hash.is_empty());
        // Hash should be hex (64 chars for SHA256)
        assert_eq!(result.content_hash.len(), 64);
    }

    #[test]
    fn test_export_deterministic_hash() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();

        let issue = make_test_issue("bd-001", "Deterministic");
        storage.create_issue(&issue, "test").unwrap();

        let config = ExportConfig::default();

        // Export twice to different files
        let path1 = temp_dir.path().join("export1.jsonl");
        let path2 = temp_dir.path().join("export2.jsonl");

        let result1 = export_to_jsonl(&storage, &path1, &config).unwrap();
        let result2 = export_to_jsonl(&storage, &path2, &config).unwrap();

        // Hashes should be identical for same content
        assert_eq!(result1.content_hash, result2.content_hash);
    }

    #[test]
    fn test_import_skips_ephemerals() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        // Create JSONL with ephemeral issue
        let mut ephemeral = make_test_issue("test-001", "Ephemeral");
        ephemeral.ephemeral = true;
        let json = serde_json::to_string(&ephemeral).unwrap();
        fs::write(&path, format!("{json}\n")).unwrap();

        let config = ImportConfig::default();
        let result = import_from_jsonl(&mut storage, &path, &config, Some("test-")).unwrap();
        assert_eq!(result.skipped_count, 1);
        assert_eq!(result.imported_count, 0);
        assert!(storage.get_issue("test-001").unwrap().is_none());
    }

    #[test]
    fn test_import_handles_empty_lines() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        // Create JSONL with empty lines
        let issue = make_test_issue("test-001", "Valid");
        let json = serde_json::to_string(&issue).unwrap();
        let content = format!("\n{json}\n\n\n");
        fs::write(&path, content).unwrap();

        let config = ImportConfig::default();
        let result = import_from_jsonl(&mut storage, &path, &config, Some("test-")).unwrap();
        assert_eq!(result.imported_count, 1);
    }

    #[test]
    fn test_import_keeps_distinct_ids_with_identical_content() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        let issue1 = make_test_issue("test-001", "Same content");
        let issue2 = make_test_issue("test-002", "Same content");
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&issue1).unwrap(),
            serde_json::to_string(&issue2).unwrap()
        );
        fs::write(&path, content).unwrap();

        let result =
            import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-"))
                .unwrap();
        assert_eq!(result.imported_count, 2);
        assert_eq!(result.skipped_count, 0);
        assert!(storage.get_issue("test-001").unwrap().is_some());
        assert!(storage.get_issue("test-002").unwrap().is_some());
    }

    #[test]
    fn test_import_restores_foreign_keys_after_relation_sync_failure() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        let issue = make_test_issue("test-001", "Broken relations");
        let json = serde_json::to_string(&issue).unwrap();
        fs::write(&path, format!("{json}\n")).unwrap();

        storage.execute_test_sql("DROP TABLE comments;").unwrap();

        let err = import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-"))
            .unwrap_err();
        assert!(
            err.to_string().contains("comments"),
            "unexpected error: {err}"
        );

        let fk_enabled = storage
            .execute_raw_query("PRAGMA foreign_keys")
            .unwrap()
            .first()
            .and_then(|row| row.first())
            .and_then(SqliteValue::as_integer)
            .unwrap_or(0);
        assert_eq!(fk_enabled, 1, "foreign key enforcement should be restored");
    }

    #[test]
    fn test_restore_foreign_keys_after_import_errors_on_dangling_rows() {
        let storage = SqliteStorage::open_memory().unwrap();

        storage
            .execute_test_sql(
                "PRAGMA foreign_keys = OFF;
                 INSERT INTO comments (issue_id, author, text, created_at)
                 VALUES ('missing-issue', 'tester', 'dangling', '2026-01-01T00:00:00Z');",
            )
            .unwrap();

        let err = restore_foreign_keys_after_import(&storage, true).unwrap_err();
        assert!(
            err.to_string()
                .contains("orphaned rows in comments.issue_id"),
            "unexpected error: {err}"
        );

        let fk_enabled = storage
            .execute_raw_query("PRAGMA foreign_keys")
            .unwrap()
            .first()
            .and_then(|row| row.first())
            .and_then(SqliteValue::as_integer)
            .unwrap_or(0);
        assert_eq!(fk_enabled, 1, "foreign key enforcement should be restored");
    }

    #[test]
    fn test_import_orphan_cleanup_preserves_external_dependency_endpoints() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let mut epic = make_test_issue("bd-epic", "Epic");
        epic.issue_type = IssueType::Epic;
        storage.create_issue(&epic, "tester").unwrap();

        storage
            .execute_test_sql(
                "PRAGMA foreign_keys = OFF;
                 INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by)
                 VALUES ('external:child:cap', 'bd-epic', 'parent-child', '2026-01-01T00:00:00Z', 'tester');
                 INSERT INTO comments (issue_id, author, text, created_at)
                 VALUES ('missing-issue', 'tester', 'dangling', '2026-01-01T00:00:00Z');
                 PRAGMA foreign_keys = ON;",
            )
            .unwrap();

        let cleaned = cleanup_import_orphans_in_tx(&storage).unwrap();

        assert_eq!(cleaned, 1, "only the real local orphan should be removed");
        let external_rows = storage
            .execute_raw_query(
                "SELECT issue_id, depends_on_id
                 FROM dependencies
                 WHERE issue_id = 'external:child:cap'",
            )
            .unwrap();
        assert_eq!(
            external_rows.len(),
            1,
            "external dependency endpoints must survive import cleanup"
        );
    }

    #[test]
    fn test_import_new_issue_replaces_preexisting_owned_relation_orphans() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        let parent = make_test_issue("bd-parent", "Parent");
        storage.create_issue(&parent, "tester").unwrap();

        storage
            .execute_test_sql(
                "PRAGMA foreign_keys = OFF;
                 INSERT INTO labels (issue_id, label)
                 VALUES ('bd-new', 'stale-label');
                 INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by)
                 VALUES ('bd-new', 'bd-parent', 'blocks', '2026-01-01T00:00:00Z', 'legacy');
                 INSERT INTO comments (issue_id, author, text, created_at)
                 VALUES ('bd-new', 'legacy', 'stale comment', '2026-01-01T00:00:00Z');
                 PRAGMA foreign_keys = ON;",
            )
            .unwrap();

        let issue = make_test_issue("bd-new", "Clean import");
        let json = serde_json::to_string(&issue).unwrap();
        fs::write(&path, format!("{json}\n")).unwrap();

        let result =
            import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("bd-")).unwrap();

        assert_eq!(result.created_count, 1);
        assert!(
            storage.get_labels("bd-new").unwrap().is_empty(),
            "fresh import must delete stale owned labels"
        );
        assert!(
            storage.get_dependencies_full("bd-new").unwrap().is_empty(),
            "fresh import must delete stale owned dependencies"
        );
        assert!(
            storage.get_comments("bd-new").unwrap().is_empty(),
            "fresh import must delete stale owned comments"
        );
    }

    #[test]
    fn test_import_error_reports_foreign_key_restore_failure_when_both_fail() {
        let apply_result: Result<ImportResult> =
            Err(BeadsError::Config("stream import failed".to_string()));
        let fk_restore_result: Result<()> = Err(BeadsError::Config(
            "foreign keys stayed disabled".to_string(),
        ));

        let err =
            finish_import_after_foreign_key_restore(apply_result, fk_restore_result).unwrap_err();
        let msg = err.to_string();

        assert!(
            msg.contains(
                "jsonl import failed, and SQLite foreign key enforcement could not be re-enabled"
            ),
            "unexpected error message: {msg}"
        );
        assert!(
            msg.contains("foreign keys stayed disabled"),
            "restore error should be included: {msg}"
        );
        assert!(
            msg.contains("stream import failed"),
            "original import error should be preserved as the source: {msg}"
        );

        assert!(
            matches!(&err, BeadsError::WithContext { .. }),
            "expected WithContext wrapping both failures"
        );
        if let BeadsError::WithContext { context, source } = err {
            assert!(context.contains("foreign keys stayed disabled"));
            assert_eq!(
                source.to_string(),
                "Configuration error: stream import failed"
            );
        }
    }

    #[test]
    fn test_import_rolls_back_partial_changes_after_relation_sync_failure() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        let existing = make_test_issue("test-existing", "Existing issue");
        storage.create_issue(&existing, "test").unwrap();
        storage
            .set_export_hashes(&[("test-existing".to_string(), "existing-hash".to_string())])
            .unwrap();

        let issue = make_test_issue("test-001", "Broken relations");
        let json = serde_json::to_string(&issue).unwrap();
        fs::write(&path, format!("{json}\n")).unwrap();

        storage.execute_test_sql("DROP TABLE comments;").unwrap();

        let err = import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-"))
            .unwrap_err();
        assert!(
            err.to_string().contains("comments"),
            "unexpected error: {err}"
        );

        assert!(
            storage.get_issue("test-001").unwrap().is_none(),
            "failed import should not leave a partially inserted issue behind"
        );
        assert!(
            storage.get_issue("test-existing").unwrap().is_some(),
            "failed import should preserve pre-existing issues"
        );

        let export_hash_rows = storage
            .execute_raw_query("SELECT issue_id, content_hash FROM export_hashes")
            .unwrap();
        assert_eq!(export_hash_rows.len(), 1, "export hashes should roll back");
        assert_eq!(
            export_hash_rows[0]
                .first()
                .and_then(SqliteValue::as_text)
                .unwrap_or(""),
            "test-existing"
        );
    }

    #[test]
    fn test_detect_collision_external_ref_priority() {
        let storage = SqliteStorage::open_memory().unwrap();

        let mut ext_issue = make_issue_at("bd-ext", "External", fixed_time(100));
        ext_issue.external_ref = Some("JIRA-1".to_string());
        set_content_hash(&mut ext_issue);
        storage.upsert_issue_for_import(&ext_issue).unwrap();

        let mut hash_issue = make_issue_at("bd-hash", "Incoming", fixed_time(200));
        set_content_hash(&mut hash_issue);
        storage.upsert_issue_for_import(&hash_issue).unwrap();

        // Incoming has same external_ref as ext_issue - should match on external_ref
        // even though it has same title/content_hash as hash_issue
        let mut incoming = make_issue_at("bd-new", "Incoming", fixed_time(300));
        incoming.external_ref = Some("JIRA-1".to_string());
        let computed_hash = crate::util::content_hash(&incoming);

        let (id_by_ext_ref, id_by_hash, meta_by_id) = build_collision_maps(&storage);
        let collision = detect_collision(
            &incoming,
            &id_by_ext_ref,
            &id_by_hash,
            &meta_by_id,
            &computed_hash,
        );
        assert!(
            matches!(collision, CollisionResult::Match { .. }),
            "expected match"
        );
        if let CollisionResult::Match {
            existing_id,
            match_type,
            phase,
        } = collision
        {
            assert_eq!(existing_id, "bd-ext");
            assert_eq!(match_type, MatchType::ExternalRef);
            assert_eq!(phase, 1);
        }
    }

    #[test]
    fn test_detect_collision_id_preempts_content_hash() {
        let storage = SqliteStorage::open_memory().unwrap();

        let mut hash_issue = make_issue_at("bd-hash", "Same Content", fixed_time(100));
        set_content_hash(&mut hash_issue);
        storage.upsert_issue_for_import(&hash_issue).unwrap();

        let mut id_issue = make_issue_at("bd-same", "Different Content", fixed_time(100));
        set_content_hash(&mut id_issue);
        storage.upsert_issue_for_import(&id_issue).unwrap();

        let incoming = make_issue_at("bd-same", "Same Content", fixed_time(200));
        let computed_hash = crate::util::content_hash(&incoming);

        let (id_by_ext_ref, id_by_hash, meta_by_id) = build_collision_maps(&storage);
        let collision = detect_collision(
            &incoming,
            &id_by_ext_ref,
            &id_by_hash,
            &meta_by_id,
            &computed_hash,
        );
        assert!(
            matches!(collision, CollisionResult::Match { .. }),
            "expected match"
        );
        if let CollisionResult::Match {
            existing_id,
            match_type,
            phase,
        } = collision
        {
            assert_eq!(existing_id, "bd-same");
            assert_eq!(match_type, MatchType::Id);
            assert_eq!(phase, 2);
        }
    }

    #[test]
    fn test_detect_collision_duplicate_content_hash_keeps_first_match() {
        let storage = SqliteStorage::open_memory().unwrap();

        let mut first = make_issue_at("bd-first", "Same Content", fixed_time(100));
        set_content_hash(&mut first);
        storage.upsert_issue_for_import(&first).unwrap();

        let mut second = make_issue_at("bd-second", "Same Content", fixed_time(200));
        set_content_hash(&mut second);
        storage.upsert_issue_for_import(&second).unwrap();

        let incoming = make_issue_at("bd-new", "Same Content", fixed_time(300));
        let computed_hash = crate::util::content_hash(&incoming);

        let (id_by_ext_ref, id_by_hash, meta_by_id) = build_collision_maps(&storage);
        let collision = detect_collision(
            &incoming,
            &id_by_ext_ref,
            &id_by_hash,
            &meta_by_id,
            &computed_hash,
        );

        assert!(
            matches!(collision, CollisionResult::Match { .. }),
            "expected match"
        );
        if let CollisionResult::Match {
            existing_id,
            match_type,
            phase,
        } = collision
        {
            assert_eq!(existing_id, "bd-first");
            assert_eq!(match_type, MatchType::ContentHash);
            assert_eq!(phase, 3);
        }
    }

    #[test]
    fn test_detect_collision_id_match() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        let existing = make_issue_at("bd-1", "Existing", fixed_time(100));
        storage.create_issue(&existing, "test").unwrap();

        let incoming = make_issue_at("bd-1", "Incoming", fixed_time(200));

        let computed_hash = crate::util::content_hash(&incoming);
        let (id_by_ext_ref, id_by_hash, meta_by_id) = build_collision_maps(&storage);
        let collision = detect_collision(
            &incoming,
            &id_by_ext_ref,
            &id_by_hash,
            &meta_by_id,
            &computed_hash,
        );

        assert!(
            matches!(collision, CollisionResult::Match { .. }),
            "expected match"
        );
        if let CollisionResult::Match {
            existing_id,
            match_type,
            phase,
        } = collision
        {
            assert_eq!(existing_id, "bd-1");
            assert_eq!(match_type, MatchType::Id);
            assert_eq!(phase, 2);
        }
    }

    #[test]
    fn test_determine_action_tombstone_skip() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let mut tombstone = make_issue_at("bd-1", "Tombstone", fixed_time(100));
        tombstone.status = Status::Tombstone;
        storage.create_issue(&tombstone, "test").unwrap();

        let incoming = make_issue_at("bd-1", "Incoming", fixed_time(200));
        let collision = CollisionResult::Match {
            existing_id: "bd-1".to_string(),
            match_type: MatchType::Id,
            phase: 3,
        };
        let (_, _, meta_by_id) = build_collision_maps(&storage);
        let action = determine_action(&collision, &incoming, &meta_by_id, false).unwrap();
        assert!(
            matches!(action, CollisionAction::Skip { .. }),
            "expected tombstone skip"
        );
        if let CollisionAction::Skip { reason } = action {
            assert!(reason.contains("Tombstone protection"));
        }
    }

    #[test]
    fn test_determine_action_timestamp_comparison() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let existing = make_issue_at("bd-1", "Existing", fixed_time(100));
        storage.create_issue(&existing, "test").unwrap();

        let collision = CollisionResult::Match {
            existing_id: "bd-1".to_string(),
            match_type: MatchType::Id,
            phase: 3,
        };
        let (_, _, meta_by_id) = build_collision_maps(&storage);

        let newer = make_issue_at("bd-1", "Incoming", fixed_time(200));
        let action = determine_action(&collision, &newer, &meta_by_id, false).unwrap();
        assert!(
            matches!(action, CollisionAction::Update { .. }),
            "expected update action"
        );

        let equal = make_issue_at("bd-1", "Incoming", fixed_time(100));
        let action = determine_action(&collision, &equal, &meta_by_id, false).unwrap();
        assert!(
            matches!(action, CollisionAction::Skip { .. }),
            "expected equal timestamp skip"
        );
        if let CollisionAction::Skip { reason } = action {
            assert!(reason.contains("Equal timestamps"));
        }

        let older = make_issue_at("bd-1", "Incoming", fixed_time(50));
        let action = determine_action(&collision, &older, &meta_by_id, false).unwrap();
        assert!(
            matches!(action, CollisionAction::Skip { .. }),
            "expected older timestamp skip"
        );
        if let CollisionAction::Skip { reason } = action {
            assert!(reason.contains("Existing is newer"));
        }
    }

    #[test]
    fn test_import_prefix_mismatch_error() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        let issue = make_issue_at("xx-001", "Bad prefix", fixed_time(100));
        let json = serde_json::to_string(&issue).unwrap();
        fs::write(&path, format!("{json}\n")).unwrap();

        let config = ImportConfig::default();
        let err = import_from_jsonl(&mut storage, &path, &config, Some("bd")).unwrap_err();
        assert!(err.to_string().contains("Prefix mismatch"));
    }

    #[test]
    fn test_import_prefix_mismatch_error_for_shared_prefix_superset() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        let issue = make_issue_at("bdx-001", "Looks similar but wrong prefix", fixed_time(100));
        let json = serde_json::to_string(&issue).unwrap();
        fs::write(&path, format!("{json}\n")).unwrap();

        let err = import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("bd"))
            .unwrap_err();
        assert!(err.to_string().contains("Prefix mismatch"));
        assert!(err.to_string().contains("bdx-001"));
    }

    #[test]
    fn test_import_duplicate_external_ref_errors() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        let mut issue1 = make_issue_at("bd-001", "Issue 1", fixed_time(100));
        issue1.external_ref = Some("JIRA-1".to_string());
        let mut issue2 = make_issue_at("bd-002", "Issue 2", fixed_time(120));
        issue2.external_ref = Some("JIRA-1".to_string());

        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&issue1).unwrap(),
            serde_json::to_string(&issue2).unwrap()
        );
        fs::write(&path, content).unwrap();

        let config = ImportConfig::default();
        let err = import_from_jsonl(&mut storage, &path, &config, None).unwrap_err();
        assert!(err.to_string().contains("Duplicate external_ref"));
    }

    #[test]
    fn test_import_duplicate_issue_ids_error() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        let issue1 = make_issue_at("bd-001", "Issue 1", fixed_time(100));
        let issue2 = make_issue_at("bd-001", "Issue 2", fixed_time(120));

        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&issue1).unwrap(),
            serde_json::to_string(&issue2).unwrap()
        );
        fs::write(&path, content).unwrap();

        let err = import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("bd"))
            .unwrap_err();
        assert!(err.to_string().contains("Duplicate issue id 'bd-001'"));
    }

    #[test]
    fn test_import_duplicate_external_ref_clears_and_inserts() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let path = temp_dir.path().join("issues.jsonl");

        let mut issue1 = make_issue_at("bd-001", "Issue 1", fixed_time(100));
        issue1.external_ref = Some("JIRA-1".to_string());
        let mut issue2 = make_issue_at("bd-002", "Issue 2", fixed_time(120));
        issue2.external_ref = Some("JIRA-1".to_string());

        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&issue1).unwrap(),
            serde_json::to_string(&issue2).unwrap()
        );
        fs::write(&path, content).unwrap();

        let config = ImportConfig {
            clear_duplicate_external_refs: true,
            ..Default::default()
        };
        let result = import_from_jsonl(&mut storage, &path, &config, None).unwrap();

        assert_eq!(result.imported_count, 2);
        assert_eq!(result.skipped_count, 0);
        let first = storage.get_issue("bd-001").unwrap().unwrap();
        let second = storage.get_issue("bd-002").unwrap().unwrap();
        assert_eq!(first.external_ref.as_deref(), Some("JIRA-1"));
        assert!(second.external_ref.is_none());
    }

    #[test]
    fn test_export_deterministic_order() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("issues.jsonl");

        let issue_a = make_test_issue("bd-z", "Zed");
        let issue_b = make_test_issue("bd-a", "Aye");
        let issue_c = make_test_issue("bd-m", "Em");

        storage.create_issue(&issue_a, "test").unwrap();
        storage.create_issue(&issue_b, "test").unwrap();
        storage.create_issue(&issue_c, "test").unwrap();

        let config = ExportConfig::default();
        export_to_jsonl(&storage, &output_path, &config).unwrap();

        let ids = read_issues_from_jsonl(&output_path)
            .unwrap()
            .into_iter()
            .map(|issue| issue.id)
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["bd-a", "bd-m", "bd-z"]);
    }

    #[test]
    fn test_normalize_issue_for_export_orders_identical_comments_by_id() {
        let timestamp = fixed_time(100);
        let mut issue = make_test_issue("bd-1", "Ordering");
        issue.comments = vec![
            Comment {
                id: 9,
                issue_id: issue.id.clone(),
                author: "tester".to_string(),
                body: "same".to_string(),
                created_at: timestamp,
            },
            Comment {
                id: 2,
                issue_id: issue.id.clone(),
                author: "tester".to_string(),
                body: "same".to_string(),
                created_at: timestamp,
            },
        ];

        normalize_issue_for_export(&mut issue);

        let ids = issue
            .comments
            .into_iter()
            .map(|comment| comment.id)
            .collect::<Vec<_>>();
        assert_eq!(ids, vec![2, 9]);
    }

    #[test]
    fn test_finalize_export_updates_metadata_and_clears_dirty() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("issues.jsonl");

        let issue = make_test_issue("bd-1", "Issue");
        storage.create_issue(&issue, "test").unwrap();
        assert_eq!(storage.get_dirty_issue_ids().unwrap().len(), 1);

        let config = ExportConfig::default();
        let result = export_to_jsonl(&storage, &output_path, &config).unwrap();
        finalize_export(
            &mut storage,
            &result,
            Some(&result.issue_hashes),
            &output_path,
        )
        .unwrap();

        assert!(storage.get_dirty_issue_ids().unwrap().is_empty());
        assert!(
            storage
                .get_metadata(METADATA_JSONL_CONTENT_HASH)
                .unwrap()
                .is_some()
        );
        assert!(
            storage
                .get_metadata(METADATA_LAST_EXPORT_TIME)
                .unwrap()
                .is_some()
        );
        assert!(
            storage
                .get_metadata(METADATA_JSONL_MTIME)
                .unwrap()
                .is_some()
        );
        assert!(storage.get_metadata(METADATA_JSONL_SIZE).unwrap().is_some());
    }

    #[test]
    fn test_export_hashes_certified_current_requires_no_dirty_and_matching_hash() {
        let mut result = ExportResult {
            exported_count: 1,
            exported_ids: vec!["bd-1".to_string()],
            exported_marked_at: Vec::new(),
            skipped_tombstone_ids: Vec::new(),
            content_hash: "content-hash".to_string(),
            output_path: None,
            issue_hashes: vec![("bd-1".to_string(), "issue-hash".to_string())],
        };

        assert!(export_hashes_certified_current(
            &result,
            Some("content-hash")
        ));
        assert!(!export_hashes_certified_current(
            &result,
            Some("different-hash")
        ));
        assert!(!export_hashes_certified_current(&result, None));

        result
            .exported_marked_at
            .push(("bd-1".to_string(), "dirty-marker".to_string()));
        assert!(!export_hashes_certified_current(
            &result,
            Some("content-hash")
        ));
    }

    #[test]
    fn test_auto_flush_clears_byte_identical_dirty_marker_without_rewrite() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let beads_dir = temp_dir.path().join(".beads");
        let output_path = beads_dir.join("issues.jsonl");
        fs::create_dir_all(&beads_dir).unwrap();

        let issue = make_test_issue("bd-noop", "No-op dirty marker");
        storage.create_issue(&issue, "test").unwrap();

        let first = auto_flush(&mut storage, &beads_dir, &output_path, false).unwrap();
        assert!(first.flushed);
        let before = fs::read_to_string(&output_path).unwrap();

        storage
            .replace_dirty_issue_marker("bd-noop", "manual-dirty-marker")
            .unwrap();

        let second = auto_flush(&mut storage, &beads_dir, &output_path, false).unwrap();
        assert!(
            !second.flushed,
            "byte-identical dirty markers should not rewrite JSONL"
        );
        assert!(storage.get_dirty_issue_ids().unwrap().is_empty());
        assert_eq!(fs::read_to_string(&output_path).unwrap(), before);
    }

    #[test]
    fn test_filter_dirty_metadata_for_export_only_includes_exported_ids() {
        let dirty_metadata = vec![
            ("bd-1".to_string(), "t1".to_string()),
            ("bd-2".to_string(), "t2".to_string()),
            ("bd-3".to_string(), "t3".to_string()),
        ];
        let exported_ids = vec!["bd-1".to_string()];
        let skipped_tombstone_ids = vec!["bd-3".to_string()];

        let filtered = filter_dirty_metadata_for_export(
            &dirty_metadata,
            &exported_ids,
            &skipped_tombstone_ids,
        );

        assert_eq!(
            filtered,
            vec![
                ("bd-1".to_string(), "t1".to_string()),
                ("bd-3".to_string(), "t3".to_string()),
            ]
        );
    }

    #[test]
    fn test_finalize_export_rolls_back_on_failure() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let temp_dir = TempDir::new().unwrap();
        let output_path = temp_dir.path().join("issues.jsonl");

        let issue = make_test_issue("bd-finalize", "Issue");
        storage.create_issue(&issue, "test").unwrap();
        assert_eq!(storage.get_dirty_issue_ids().unwrap().len(), 1);

        let config = ExportConfig::default();
        let result = export_to_jsonl(&storage, &output_path, &config).unwrap();

        let invalid_issue_hashes = vec![("bd-missing".to_string(), "hash".to_string())];

        let err = finalize_export(
            &mut storage,
            &result,
            Some(&invalid_issue_hashes),
            &output_path,
        )
        .unwrap_err();
        assert!(
            matches!(err, BeadsError::Database(_)),
            "unexpected error: {err:?}"
        );

        assert_eq!(
            storage.get_dirty_issue_ids().unwrap(),
            vec!["bd-finalize".to_string()]
        );
        assert!(storage.get_export_hash("bd-finalize").unwrap().is_none());
        assert!(
            storage
                .get_metadata(METADATA_JSONL_CONTENT_HASH)
                .unwrap()
                .is_none()
        );
        assert!(
            storage
                .get_metadata(METADATA_LAST_EXPORT_TIME)
                .unwrap()
                .is_none()
        );
        assert!(
            storage
                .get_metadata(METADATA_JSONL_MTIME)
                .unwrap()
                .is_none()
        );
        assert!(storage.get_metadata(METADATA_JSONL_SIZE).unwrap().is_none());
    }

    #[test]
    fn test_export_policy_strict_fails_on_write_error() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue1 = make_test_issue("bd-001", "First");
        let issue2 = make_test_issue("bd-002", "Second");
        storage.create_issue(&issue1, "test").unwrap();
        storage.create_issue(&issue2, "test").unwrap();

        let mut writer = LineFailWriter::new("bd-002");
        let result = export_to_writer_with_policy(&storage, &mut writer, ExportErrorPolicy::Strict);
        assert!(result.is_err());
    }

    #[test]
    fn test_export_to_writer_streams_large_issue_set_in_id_order() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue_count = (EXPORT_ISSUE_BATCH_SIZE * 2) + 3;

        for index in (0..issue_count).rev() {
            let id = format!("bd-{index:04}");
            let title = format!("Issue {index}");
            let issue = make_test_issue(&id, &title);
            storage.create_issue(&issue, "test").unwrap();
        }

        let mut writer = Vec::new();
        let (result, report) =
            export_to_writer_with_policy(&storage, &mut writer, ExportErrorPolicy::Strict).unwrap();

        assert_eq!(result.exported_count, issue_count);
        assert_eq!(report.issues_exported, issue_count);

        let output = String::from_utf8(writer).unwrap();
        let ids = output
            .lines()
            .map(|line| serde_json::from_str::<Issue>(line).unwrap().id)
            .collect::<Vec<_>>();
        let mut sorted_ids = ids.clone();
        sorted_ids.sort();

        assert_eq!(ids.len(), issue_count);
        assert_eq!(ids, sorted_ids);
    }

    #[test]
    fn test_export_policy_best_effort_skips_write_error() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue1 = make_test_issue("bd-001", "First");
        let issue2 = make_test_issue("bd-002", "Second");
        storage.create_issue(&issue1, "test").unwrap();
        storage.create_issue(&issue2, "test").unwrap();

        let mut writer = LineFailWriter::new("bd-002");
        let (result, report) =
            export_to_writer_with_policy(&storage, &mut writer, ExportErrorPolicy::BestEffort)
                .unwrap();
        assert_eq!(result.exported_count, 1);
        assert_eq!(report.errors.len(), 1);
        let output = writer.into_string();
        assert!(output.contains("bd-001"));
        assert!(!output.contains("bd-002"));
    }

    #[test]
    fn test_export_policy_partial_collects_write_error() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue1 = make_test_issue("bd-001", "First");
        let issue2 = make_test_issue("bd-002", "Second");
        storage.create_issue(&issue1, "test").unwrap();
        storage.create_issue(&issue2, "test").unwrap();

        let mut writer = LineFailWriter::new("bd-002");
        let (result, report) =
            export_to_writer_with_policy(&storage, &mut writer, ExportErrorPolicy::Partial)
                .unwrap();

        assert_eq!(result.exported_count, 1);
        assert_eq!(report.errors.len(), 1);
    }

    #[test]
    fn test_export_policy_required_core_fails_on_issue_error() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue1 = make_test_issue("bd-001", "First");
        let issue2 = make_test_issue("bd-002", "Second");
        storage.create_issue(&issue1, "test").unwrap();
        storage.create_issue(&issue2, "test").unwrap();

        let mut writer = LineFailWriter::new("bd-002");
        let result =
            export_to_writer_with_policy(&storage, &mut writer, ExportErrorPolicy::RequiredCore);
        assert!(result.is_err());
    }

    #[test]
    fn test_export_policy_required_core_allows_non_core_errors() {
        // This test verifies that RequiredCore policy exports all issues successfully
        // and would tolerate non-core errors (Label, Dependency, Comment) if they occurred.
        // The test doesn't generate non-core errors since the setup has no labels/deps.
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue1 = make_test_issue("bd-001", "First");
        let issue2 = make_test_issue("bd-002", "Second");
        storage.create_issue(&issue1, "test").unwrap();
        storage.create_issue(&issue2, "test").unwrap();

        let mut writer = Vec::new();
        let (result, report) =
            export_to_writer_with_policy(&storage, &mut writer, ExportErrorPolicy::RequiredCore)
                .unwrap();

        assert_eq!(result.exported_count, 2);
        // Any errors present should be non-core (Issue errors would cause failure above)
        for err in &report.errors {
            assert_ne!(
                err.entity_type,
                ExportEntityType::Issue,
                "Issue errors should fail RequiredCore policy"
            );
        }
    }

    // ============================================================================
    // PREFLIGHT TESTS (beads_rust-0v1.2.7)
    // ============================================================================

    #[test]
    fn test_preflight_check_status_ordering() {
        // Verify that PreflightCheckStatus can be used for comparison
        assert_ne!(PreflightCheckStatus::Pass, PreflightCheckStatus::Warn);
        assert_ne!(PreflightCheckStatus::Warn, PreflightCheckStatus::Fail);
        assert_ne!(PreflightCheckStatus::Pass, PreflightCheckStatus::Fail);
    }

    #[test]
    fn test_preflight_result_aggregates_status() {
        let mut result = PreflightResult::new();

        // Initial state is Pass
        assert_eq!(result.overall_status, PreflightCheckStatus::Pass);
        assert!(result.is_ok());
        assert!(result.has_no_failures());

        // Add a passing check
        result.add(PreflightCheck::pass("test1", "Test 1", "Passed"));
        assert_eq!(result.overall_status, PreflightCheckStatus::Pass);

        // Add a warning - overall becomes Warn
        result.add(PreflightCheck::warn("test2", "Test 2", "Warning", "Fix it"));
        assert_eq!(result.overall_status, PreflightCheckStatus::Warn);
        assert!(!result.is_ok());
        assert!(result.has_no_failures());

        // Add a failure - overall becomes Fail
        result.add(PreflightCheck::fail("test3", "Test 3", "Failed", "Fix it"));
        assert_eq!(result.overall_status, PreflightCheckStatus::Fail);
        assert!(!result.is_ok());
        assert!(!result.has_no_failures());

        // Check counts
        assert_eq!(result.failures().len(), 1);
        assert_eq!(result.warnings().len(), 1);
    }

    #[test]
    fn test_preflight_result_into_result_succeeds_on_pass() {
        let mut result = PreflightResult::new();
        result.add(PreflightCheck::pass("test", "Test", "OK"));

        let converted = result.into_result();
        assert!(converted.is_ok());
    }

    #[test]
    fn test_preflight_result_into_result_succeeds_on_warn() {
        let mut result = PreflightResult::new();
        result.add(PreflightCheck::warn("test", "Test", "Warning", "Fix"));

        let converted = result.into_result();
        assert!(converted.is_ok());
    }

    #[test]
    fn test_preflight_result_into_result_fails_on_fail() {
        let mut result = PreflightResult::new();
        result.add(PreflightCheck::fail("test", "Test", "Failed", "Fix it"));

        let converted = result.into_result();
        assert!(converted.is_err());

        let err_msg = converted.unwrap_err().to_string();
        assert!(err_msg.contains("Preflight checks failed"));
        assert!(err_msg.contains("test"));
        assert!(err_msg.contains("Failed"));
    }

    #[test]
    fn test_preflight_import_rejects_nonexistent_file() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("nonexistent.jsonl");

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, None).unwrap();

        assert_eq!(result.overall_status, PreflightCheckStatus::Fail);
        assert!(result.failures().iter().any(|c| c.name == "file_readable"));
    }

    #[test]
    fn test_preflight_import_rejects_conflict_markers() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        // Write a file with conflict markers
        let mut file = std::fs::File::create(&jsonl_path).unwrap();
        writeln!(file, "<<<<<<< HEAD").unwrap();
        file.write_all(br#"{"id":"bd-1","title":"Test"}"#).unwrap();
        writeln!(file).unwrap();
        writeln!(file, "=======").unwrap();
        file.write_all(br#"{"id":"bd-1","title":"Test Modified"}"#)
            .unwrap();
        writeln!(file).unwrap();
        writeln!(file, ">>>>>>> branch").unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, None).unwrap();

        assert_eq!(result.overall_status, PreflightCheckStatus::Fail);
        assert!(
            result
                .failures()
                .iter()
                .any(|c| c.name == "no_conflict_markers")
        );
    }

    #[test]
    fn test_preflight_import_does_not_inspect_rejected_path() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let outside_jsonl_path = temp.path().join("outside.jsonl");
        std::fs::write(&outside_jsonl_path, "not json\n").unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            allow_external_jsonl: false,
            ..Default::default()
        };

        let result = preflight_import(&outside_jsonl_path, &config, Some("bd")).unwrap();

        assert_eq!(result.overall_status, PreflightCheckStatus::Fail);
        assert!(
            result
                .failures()
                .iter()
                .any(|c| c.name == "path_validation"),
            "rejected path should fail path validation"
        );
        assert!(
            result.checks.iter().all(|c| c.name != "file_readable"
                && c.name != "no_conflict_markers"
                && c.name != "json_valid"
                && c.name != "prefix_match"),
            "preflight should not read or parse rejected paths: {:?}",
            result.checks
        );
    }

    #[test]
    fn test_preflight_import_passes_valid_jsonl() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        // Write valid JSONL
        let issue = make_test_issue("bd-001", "Test Issue");
        let json = serde_json::to_string(&issue).unwrap();
        std::fs::write(&jsonl_path, format!("{json}\n")).unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, None).unwrap();

        assert_eq!(result.overall_status, PreflightCheckStatus::Pass);
        assert!(result.failures().is_empty());
    }

    #[test]
    fn test_preflight_export_passes_with_valid_setup() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        let storage = SqliteStorage::open_memory().unwrap();
        let config = ExportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_export(&storage, &jsonl_path, &config).unwrap();

        assert_eq!(
            result.overall_status,
            PreflightCheckStatus::Pass,
            "Expected Pass, got {:?}. Failures: {:?}",
            result.overall_status,
            result.failures()
        );
        assert!(result.failures().is_empty());
    }

    // ========================================================================
    // Preflight Guardrail Tests (beads_rust-1quj)
    // ========================================================================

    #[test]
    fn test_preflight_import_rejects_invalid_json_lines() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        // Write JSONL with invalid lines
        let issue1 = make_test_issue("bd-001", "Good issue");
        let issue2 = make_test_issue("bd-002", "Another good issue");
        let good_json_1 = serde_json::to_string(&issue1).unwrap();
        let good_json_2 = serde_json::to_string(&issue2).unwrap();
        let content = format!("{good_json_1}\nNOT VALID JSON\n{good_json_2}\n{{\"broken: true}}\n");
        std::fs::write(&jsonl_path, content).unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, None).unwrap();

        assert_eq!(result.overall_status, PreflightCheckStatus::Fail);
        let failures = result.failures();
        let json_check = failures.iter().find(|c| c.name == "json_valid");
        assert!(json_check.is_some(), "Expected json_valid failure");
        let msg = &json_check.unwrap().message;
        assert!(msg.contains("2 invalid issue record"), "Message was: {msg}");
        assert!(msg.contains("line 2"), "Should mention line 2: {msg}");
        assert!(msg.contains("line 4"), "Should mention line 4: {msg}");
    }

    #[test]
    fn test_preflight_import_passes_valid_json_lines() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        let issue1 = make_test_issue("bd-001", "First");
        let issue2 = make_test_issue("bd-002", "Second");
        let content = format!(
            "{}\n\n{}\n",
            serde_json::to_string(&issue1).unwrap(),
            serde_json::to_string(&issue2).unwrap()
        );
        std::fs::write(&jsonl_path, content).unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, None).unwrap();

        // json_valid should pass
        let json_check = result.checks.iter().find(|c| c.name == "json_valid");
        assert!(json_check.is_some());
        assert_eq!(json_check.unwrap().status, PreflightCheckStatus::Pass);
    }

    #[test]
    fn test_validate_jsonl_issue_records_rejects_duplicate_issue_ids() {
        let temp = TempDir::new().unwrap();
        let jsonl_path = temp.path().join("issues.jsonl");

        let issue = make_test_issue("bd-dup", "Duplicate");
        let issue_json = serde_json::to_string(&issue).unwrap();
        std::fs::write(&jsonl_path, format!("{issue_json}\n{issue_json}\n")).unwrap();

        let summary = validate_jsonl_issue_records(&jsonl_path).unwrap();

        assert_eq!(summary.record_count, 2);
        assert_eq!(summary.invalid_count, 1);
        let preview = summary.preview_messages();
        assert!(
            preview
                .iter()
                .any(|message| message.contains("line 2: Duplicate issue id 'bd-dup'")),
            "expected duplicate-id validation failure in preview, got {preview:?}"
        );
    }

    #[test]
    fn test_preflight_import_rejects_duplicate_issue_ids_during_validation() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        let issue = make_test_issue("bd-dup", "Duplicate");
        let issue_json = serde_json::to_string(&issue).unwrap();
        std::fs::write(&jsonl_path, format!("{issue_json}\n{issue_json}\n")).unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, None).unwrap();

        assert_eq!(result.overall_status, PreflightCheckStatus::Fail);
        let failures = result.failures();
        let json_check = failures
            .iter()
            .find(|c| c.name == "json_valid")
            .expect("expected json_valid failure");
        assert!(
            json_check.message.contains("Duplicate issue id 'bd-dup'"),
            "expected duplicate-id validation message, got {}",
            json_check.message
        );
    }

    #[test]
    fn test_preflight_import_rejects_semantically_invalid_issue_records() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        let mut invalid_issue = make_test_issue("bd-001", "");
        invalid_issue.title.clear();
        std::fs::write(
            &jsonl_path,
            format!("{}\n", serde_json::to_string(&invalid_issue).unwrap()),
        )
        .unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, None).unwrap();

        assert_eq!(result.overall_status, PreflightCheckStatus::Fail);
        let failures = result.failures();
        let json_check = failures.iter().find(|c| c.name == "json_valid");
        assert!(json_check.is_some(), "Expected json_valid failure");
        assert!(
            json_check
                .expect("json_valid failure")
                .message
                .contains("title"),
            "Expected validation failure to mention the empty title"
        );
    }

    #[test]
    fn test_preflight_import_rejects_prefix_mismatch() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        // Write issues with wrong prefix
        let issue1 = make_test_issue("xx-001", "Wrong prefix 1");
        let issue2 = make_test_issue("xx-002", "Wrong prefix 2");
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&issue1).unwrap(),
            serde_json::to_string(&issue2).unwrap()
        );
        std::fs::write(&jsonl_path, content).unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, Some("bd")).unwrap();

        assert_eq!(result.overall_status, PreflightCheckStatus::Fail);
        let failures = result.failures();
        let prefix_check = failures.iter().find(|c| c.name == "prefix_match");
        assert!(prefix_check.is_some(), "Expected prefix_match failure");
        let msg = &prefix_check.unwrap().message;
        assert!(msg.contains("xx-001"), "Should list mismatched ID: {msg}");
        assert!(msg.contains("xx-002"), "Should list mismatched ID: {msg}");
        assert!(msg.contains("2 mismatched"), "Should show count: {msg}");
    }

    #[test]
    fn test_preflight_import_rejects_shared_prefix_superset() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        let issue = make_test_issue("bdx-001", "Wrong shared prefix");
        let json = serde_json::to_string(&issue).unwrap();
        std::fs::write(&jsonl_path, format!("{json}\n")).unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, Some("bd")).unwrap();
        assert_eq!(result.overall_status, PreflightCheckStatus::Fail);
        let failures = result.failures();
        let prefix_check = failures.iter().find(|c| c.name == "prefix_match");
        assert!(prefix_check.is_some(), "Expected prefix_match failure");
        assert!(
            prefix_check.unwrap().message.contains("bdx-001"),
            "Should report the mismatched ID"
        );
    }

    #[test]
    fn test_preflight_import_prefix_check_skipped_when_override() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        // Write issues with wrong prefix
        let issue = make_test_issue("xx-001", "Wrong prefix");
        let json = serde_json::to_string(&issue).unwrap();
        std::fs::write(&jsonl_path, format!("{json}\n")).unwrap();

        let config = ImportConfig {
            skip_prefix_validation: true,
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, Some("bd")).unwrap();

        // prefix_match check should NOT be present when skip_prefix_validation is true
        let prefix_check = result.checks.iter().find(|c| c.name == "prefix_match");
        assert!(
            prefix_check.is_none(),
            "prefix_match check should be skipped when skip_prefix_validation is true"
        );
        // Overall should pass (or at least not fail on prefix)
        assert!(
            result.failures().iter().all(|c| c.name != "prefix_match"),
            "No prefix_match failures expected with override"
        );
    }

    #[test]
    fn test_preflight_import_prefix_passes_matching_prefix() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        let issue1 = make_test_issue("bd-001", "Correct prefix 1");
        let issue2 = make_test_issue("bd-002", "Correct prefix 2");
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&issue1).unwrap(),
            serde_json::to_string(&issue2).unwrap()
        );
        std::fs::write(&jsonl_path, content).unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, Some("bd")).unwrap();

        let prefix_check = result.checks.iter().find(|c| c.name == "prefix_match");
        assert!(
            prefix_check.is_some(),
            "prefix_match check should be present"
        );
        assert_eq!(
            prefix_check.unwrap().status,
            PreflightCheckStatus::Pass,
            "prefix_match should pass for matching prefix"
        );
    }

    #[test]
    fn test_preflight_import_prefix_accepts_slugged_ids() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        let issue = make_test_issue("bd-survey-my-thing-abc123", "Slugged issue");
        let json = serde_json::to_string(&issue).unwrap();
        std::fs::write(&jsonl_path, format!("{json}\n")).unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, Some("bd")).unwrap();
        let prefix_check = result.checks.iter().find(|c| c.name == "prefix_match");
        assert!(
            prefix_check.is_some(),
            "prefix_match check should be present"
        );
        assert_eq!(
            prefix_check.unwrap().status,
            PreflightCheckStatus::Pass,
            "slugged IDs generated from the expected prefix should pass"
        );
    }

    #[test]
    fn test_id_matches_expected_prefix_keeps_non_delimited_supersets_out() {
        assert!(id_matches_expected_prefix(
            "bd-survey-my-thing-abc123",
            "bd"
        ));
        assert!(id_matches_expected_prefix(
            "bd-survey-my-thing-abc123",
            "bd-"
        ));
        assert!(!id_matches_expected_prefix("bdx-survey-abc123", "bd"));
        assert!(!id_matches_expected_prefix("x-bd-survey-abc123", "bd"));
        assert!(!id_matches_expected_prefix("bd-survey-abc123", ""));
    }

    #[test]
    fn test_preflight_import_prefix_no_check_without_expected() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        let issue = make_test_issue("xx-001", "Any prefix");
        let json = serde_json::to_string(&issue).unwrap();
        std::fs::write(&jsonl_path, format!("{json}\n")).unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        // No expected_prefix passed — prefix check should not be added
        let result = preflight_import(&jsonl_path, &config, None).unwrap();

        let prefix_check = result.checks.iter().find(|c| c.name == "prefix_match");
        assert!(
            prefix_check.is_none(),
            "prefix_match check should not run without expected_prefix"
        );
    }

    #[test]
    fn test_preflight_import_conflict_markers_mixed_content() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        // Valid JSONL with embedded conflict markers
        let issue = make_test_issue("bd-001", "Good issue");
        let good_json = serde_json::to_string(&issue).unwrap();
        let content = format!(
            "{good_json}\n<<<<<<< HEAD\n{good_json}\n=======\n{good_json}\n>>>>>>> other\n"
        );
        std::fs::write(&jsonl_path, content).unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, None).unwrap();

        assert_eq!(result.overall_status, PreflightCheckStatus::Fail);
        // Should have both conflict marker AND json validation failures
        assert!(
            result
                .failures()
                .iter()
                .any(|c| c.name == "no_conflict_markers"),
            "Should detect conflict markers"
        );
        assert!(
            result.failures().iter().any(|c| c.name == "json_valid"),
            "Conflict marker lines should also fail JSON validation"
        );
    }

    #[test]
    fn test_preflight_import_success_path_all_checks() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        // Write valid JSONL with correct prefix
        let issue1 = make_test_issue("bd-001", "Issue One");
        let issue2 = make_test_issue("bd-002", "Issue Two");
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&issue1).unwrap(),
            serde_json::to_string(&issue2).unwrap()
        );
        std::fs::write(&jsonl_path, content).unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, Some("bd")).unwrap();

        // All checks should pass
        assert_eq!(
            result.overall_status,
            PreflightCheckStatus::Pass,
            "All checks should pass. Failures: {:?}",
            result
                .failures()
                .iter()
                .map(|c| format!("{}: {}", c.name, c.message))
                .collect::<Vec<_>>()
        );
        assert!(result.failures().is_empty());

        // Verify all expected checks ran
        let check_names: Vec<&str> = result.checks.iter().map(|c| c.name.as_str()).collect();
        assert!(
            check_names.contains(&"beads_dir_exists"),
            "Should check beads dir: {check_names:?}"
        );
        assert!(
            check_names.contains(&"file_readable"),
            "Should check file readable: {check_names:?}"
        );
        assert!(
            check_names.contains(&"no_conflict_markers"),
            "Should check conflict markers: {check_names:?}"
        );
        assert!(
            check_names.contains(&"json_valid"),
            "Should check JSON validity: {check_names:?}"
        );
        assert!(
            check_names.contains(&"prefix_match"),
            "Should check prefix match: {check_names:?}"
        );
    }

    #[test]
    fn test_preflight_import_mixed_prefix_partial_mismatch() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        // Mix of correct and incorrect prefix
        let good_issue = make_test_issue("bd-001", "Good prefix");
        let bad_issue = make_test_issue("xx-002", "Bad prefix");
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&good_issue).unwrap(),
            serde_json::to_string(&bad_issue).unwrap()
        );
        std::fs::write(&jsonl_path, content).unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, Some("bd")).unwrap();

        assert_eq!(result.overall_status, PreflightCheckStatus::Fail);
        let failures = result.failures();
        let prefix_check = failures.iter().find(|c| c.name == "prefix_match");
        assert!(prefix_check.is_some());
        let msg = &prefix_check.unwrap().message;
        assert!(
            msg.contains("1 mismatched"),
            "Should show count of 1: {msg}"
        );
        assert!(msg.contains("xx-002"), "Should list the bad ID: {msg}");
    }

    #[test]
    fn test_preflight_import_prefix_skips_tombstones() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        // Create a tombstone with wrong prefix — should be silently ignored
        let mut tombstone = make_test_issue("xx-001", "Foreign tombstone");
        tombstone.status = Status::Tombstone;
        let good_issue = make_test_issue("bd-001", "Good issue");
        let content = format!(
            "{}\n{}\n",
            serde_json::to_string(&tombstone).unwrap(),
            serde_json::to_string(&good_issue).unwrap()
        );
        std::fs::write(&jsonl_path, content).unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, Some("bd")).unwrap();

        // Tombstone with wrong prefix should not cause failure
        let prefix_check = result.checks.iter().find(|c| c.name == "prefix_match");
        assert!(prefix_check.is_some());
        assert_eq!(
            prefix_check.unwrap().status,
            PreflightCheckStatus::Pass,
            "Tombstones with wrong prefix should be ignored"
        );
    }

    #[test]
    fn test_preflight_import_empty_file_passes_json_check() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        // Empty file
        std::fs::write(&jsonl_path, "").unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, None).unwrap();

        // An empty file should pass JSON validation (no invalid lines)
        let json_check = result.checks.iter().find(|c| c.name == "json_valid");
        assert!(json_check.is_some());
        assert_eq!(json_check.unwrap().status, PreflightCheckStatus::Pass);
    }

    #[test]
    fn test_preflight_import_only_blank_lines_passes_json_check() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");

        // Only whitespace/blank lines
        std::fs::write(&jsonl_path, "\n\n  \n\t\n").unwrap();

        let config = ImportConfig {
            beads_dir: Some(beads_dir),
            ..Default::default()
        };

        let result = preflight_import(&jsonl_path, &config, None).unwrap();

        let json_check = result.checks.iter().find(|c| c.name == "json_valid");
        assert!(json_check.is_some());
        assert_eq!(json_check.unwrap().status, PreflightCheckStatus::Pass);
    }

    // ========================================================================
    // 3-Way Merge Tests
    // ========================================================================

    fn fixed_time_merge(seconds: i64) -> chrono::DateTime<Utc> {
        chrono::DateTime::from_timestamp(seconds, 0).unwrap()
    }

    fn make_issue_with_hash(
        id: &str,
        title: &str,
        updated_at: chrono::DateTime<Utc>,
        hash: Option<&str>,
    ) -> Issue {
        let created_at = updated_at - chrono::Duration::seconds(60);
        Issue {
            id: id.to_string(),
            content_hash: hash.map(str::to_string),
            title: title.to_string(),
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
            created_at,
            created_by: None,
            updated_at,
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
        }
    }

    #[test]
    fn test_merge_new_local_issue_kept() {
        // Issue only in left (new local) should be kept
        let local = make_issue_with_hash("bd-1", "New Local", fixed_time_merge(100), Some("hash1"));
        let result = merge_issue(None, Some(&local), None, ConflictResolution::PreferNewer);
        assert!(matches!(result, MergeResult::Keep(issue) if issue.id == "bd-1"));
    }

    #[test]
    fn test_merge_new_external_issue_kept() {
        // Issue only in right (new external) should be kept
        let external =
            make_issue_with_hash("bd-2", "New External", fixed_time_merge(100), Some("hash2"));
        let result = merge_issue(None, None, Some(&external), ConflictResolution::PreferNewer);
        assert!(matches!(result, MergeResult::Keep(issue) if issue.id == "bd-2"));
    }

    #[test]
    fn test_merge_deleted_both_sides() {
        // Issue in base but deleted in both local and external -> delete
        let base = make_issue_with_hash("bd-3", "Old", fixed_time_merge(100), Some("hash3"));
        let result = merge_issue(Some(&base), None, None, ConflictResolution::PreferNewer);
        assert!(matches!(result, MergeResult::Delete));
    }

    #[test]
    fn test_merge_deleted_external_unmodified_local() {
        // Issue in base and local (unmodified), deleted in external -> delete
        let base = make_issue_with_hash("bd-4", "Base", fixed_time_merge(100), Some("hash4"));
        let result = merge_issue(
            Some(&base),
            Some(&base),
            None,
            ConflictResolution::PreferNewer,
        );
        assert!(matches!(result, MergeResult::Delete));
    }

    #[test]
    fn test_merge_deleted_external_modified_local() {
        // Issue in base and local (modified), deleted in external -> conflict (or keep local with PreferNewer)
        let base = make_issue_with_hash("bd-5", "Base", fixed_time_merge(100), Some("hash5"));
        let local =
            make_issue_with_hash("bd-5", "Modified", fixed_time_merge(200), Some("hash5_mod")); // Modified after base

        let result = merge_issue(
            Some(&base),
            Some(&local),
            None,
            ConflictResolution::PreferNewer,
        );
        assert!(matches!(result, MergeResult::KeepWithNote(..)));
    }

    #[test]
    fn test_merge_deleted_local_modified_external() {
        // Issue in base and external (modified), deleted in local -> conflict (or keep external with PreferNewer)
        let base = make_issue_with_hash("bd-006", "Base", fixed_time_merge(100), Some("hash6"));
        let external = make_issue_with_hash(
            "bd-006",
            "Modified",
            fixed_time_merge(200),
            Some("hash6_ext"),
        );

        let result = merge_issue(
            Some(&base),
            None,
            Some(&external),
            ConflictResolution::PreferNewer,
        );
        assert!(matches!(result, MergeResult::KeepWithNote(issue, _) if issue.title == "Modified"));
    }

    #[test]
    fn test_merge_only_local_modified() {
        // Issue in all three, only local modified -> keep local
        let base = make_issue_with_hash("bd-007", "Base", fixed_time_merge(100), Some("hash7"));
        let local = make_issue_with_hash(
            "bd-007",
            "Modified",
            fixed_time_merge(200),
            Some("hash7_mod"),
        );
        let external = make_issue_with_hash("bd-007", "Base", fixed_time_merge(100), Some("hash7")); // Same as base

        let result = merge_issue(
            Some(&base),
            Some(&local),
            Some(&external),
            ConflictResolution::PreferNewer,
        );
        assert!(matches!(result, MergeResult::Keep(issue) if issue.title == "Modified"));
    }

    #[test]
    fn test_merge_only_external_modified() {
        // Issue in all three, only external modified -> keep external
        let base = make_issue_with_hash("bd-008", "Base", fixed_time_merge(100), Some("hash8"));
        let local = make_issue_with_hash("bd-008", "Base", fixed_time_merge(100), Some("hash8")); // Same as base
        let external = make_issue_with_hash(
            "bd-008",
            "Modified",
            fixed_time_merge(200),
            Some("hash8_ext"),
        );

        let result = merge_issue(
            Some(&base),
            Some(&local),
            Some(&external),
            ConflictResolution::PreferNewer,
        );
        assert!(matches!(result, MergeResult::Keep(issue) if issue.title == "Modified"));
    }

    #[test]
    fn test_merge_both_modified_prefer_newer() {
        // Issue in all three, both modified -> keep newer
        let base = make_issue_with_hash("bd-009", "Base", fixed_time_merge(100), Some("hash9"));
        let local = make_issue_with_hash(
            "bd-009",
            "Local Mod",
            fixed_time_merge(200),
            Some("hash9_local"),
        );
        let external = make_issue_with_hash(
            "bd-009",
            "External Mod",
            fixed_time_merge(300),
            Some("hash9_ext"),
        );

        let result = merge_issue(
            Some(&base),
            Some(&local),
            Some(&external),
            ConflictResolution::PreferNewer,
        );
        assert!(
            matches!(result, MergeResult::KeepWithNote(issue, _) if issue.title == "External Mod")
        );
    }

    #[test]
    fn test_merge_both_modified_prefer_local() {
        let base = make_issue_with_hash("bd-010", "Base", fixed_time_merge(100), Some("hash10"));
        let local = make_issue_with_hash(
            "bd-010",
            "Local Mod",
            fixed_time_merge(200),
            Some("hash10_local"),
        );
        let external = make_issue_with_hash(
            "bd-010",
            "External Mod",
            fixed_time_merge(300),
            Some("hash10_ext"),
        );

        let result = merge_issue(
            Some(&base),
            Some(&local),
            Some(&external),
            ConflictResolution::PreferLocal,
        );
        assert!(
            matches!(result, MergeResult::KeepWithNote(issue, _) if issue.title == "Local Mod")
        );
    }

    #[test]
    fn test_merge_convergent_creation_same_content() {
        // Both created independently with same content hash -> keep one
        let local = make_issue_with_hash("bd-011", "Same", fixed_time_merge(100), Some("hash11"));
        let external =
            make_issue_with_hash("bd-011", "Same", fixed_time_merge(100), Some("hash11"));

        let result = merge_issue(
            None,
            Some(&local),
            Some(&external),
            ConflictResolution::PreferNewer,
        );
        assert!(matches!(result, MergeResult::Keep(..)));
    }

    #[test]
    fn test_merge_convergent_creation_different_content() {
        // Both created independently with different content -> keep newer
        let local = make_issue_with_hash(
            "bd-012",
            "Local",
            fixed_time_merge(100),
            Some("hash12_local"),
        );
        let external = make_issue_with_hash(
            "bd-012",
            "External",
            fixed_time_merge(200),
            Some("hash12_ext"),
        );

        let result = merge_issue(
            None,
            Some(&local),
            Some(&external),
            ConflictResolution::PreferNewer,
        );
        assert!(matches!(result, MergeResult::KeepWithNote(issue, _) if issue.title == "External"));
    }

    #[test]
    fn test_merge_neither_changed() {
        // Issue in all three, neither changed -> keep (use left by convention)
        let base = make_issue_with_hash("bd-013", "Same", fixed_time_merge(100), Some("hash13"));
        let local = make_issue_with_hash("bd-013", "Same", fixed_time_merge(100), Some("hash13"));
        let external =
            make_issue_with_hash("bd-013", "Same", fixed_time_merge(100), Some("hash13"));

        let result = merge_issue(
            Some(&base),
            Some(&local),
            Some(&external),
            ConflictResolution::PreferNewer,
        );
        assert!(matches!(result, MergeResult::Keep(issue) if issue.id == "bd-013"));
    }

    #[test]
    fn test_merge_report_has_conflicts() {
        let mut report = MergeReport::default();
        assert!(!report.has_conflicts());

        report
            .conflicts
            .push(("bd-001".to_string(), ConflictType::DeleteVsModify));
        assert!(report.has_conflicts());
    }

    #[test]
    fn test_merge_report_total_actions() {
        let mut report = MergeReport::default();
        assert_eq!(report.total_actions(), 0);

        report.kept.push(make_test_issue("bd-001", "Kept"));
        report.kept.push(make_test_issue("bd-002", "Kept"));
        report.deleted.push("bd-003".to_string());
        assert_eq!(report.total_actions(), 3);
    }

    // ========================================================================
    // three_way_merge orchestration tests
    // ========================================================================

    #[test]
    fn test_three_way_merge_basic() {
        // Setup: one issue in each state
        let base_issue =
            make_issue_with_hash("bd-001", "Base", fixed_time_merge(100), Some("hash1"));
        let local_issue =
            make_issue_with_hash("bd-002", "Local Only", fixed_time_merge(200), Some("hash2"));
        let external_issue = make_issue_with_hash(
            "bd-003",
            "External Only",
            fixed_time_merge(300),
            Some("hash3"),
        );

        let mut base = std::collections::HashMap::new();
        base.insert("bd-001".to_string(), base_issue.clone());

        let mut left = std::collections::HashMap::new();
        left.insert("bd-001".to_string(), base_issue.clone());
        left.insert("bd-002".to_string(), local_issue);

        let mut right = std::collections::HashMap::new();
        right.insert("bd-001".to_string(), base_issue);
        right.insert("bd-003".to_string(), external_issue);

        let context = MergeContext::new(base, left, right);
        let report = three_way_merge(&context, ConflictResolution::PreferNewer, None);

        // Should keep bd-001 (in all three), bd-002 (local only), bd-003 (external only)
        assert_eq!(report.kept.len(), 3);
        assert!(report.conflicts.is_empty());
        assert!(report.deleted.is_empty());
    }

    #[test]
    fn test_three_way_merge_with_tombstone_protection() {
        // Setup: tombstoned issue trying to resurrect from external
        let external_issue = make_issue_with_hash(
            "bd-tomb",
            "Should Not Resurrect",
            fixed_time_merge(300),
            Some("hash1"),
        );

        let base = std::collections::HashMap::new();
        let left = std::collections::HashMap::new();
        let mut right = std::collections::HashMap::new();
        right.insert("bd-tomb".to_string(), external_issue);

        let context = MergeContext::new(base, left, right);

        // Create tombstones set
        let mut tombstones = std::collections::HashSet::new();
        tombstones.insert("bd-tomb".to_string());

        let report = three_way_merge(&context, ConflictResolution::PreferNewer, Some(&tombstones));

        // Should NOT keep the tombstoned issue
        assert!(report.kept.is_empty());
        assert_eq!(report.tombstone_protected.len(), 1);
        assert!(report.tombstone_protected.contains(&"bd-tomb".to_string()));
    }

    #[test]
    fn test_three_way_merge_tombstone_allows_local() {
        // Setup: tombstoned issue exists in local - should be allowed
        let local_issue = make_issue_with_hash(
            "bd-tomb",
            "Local Tombstoned",
            fixed_time_merge(200),
            Some("hash1"),
        );

        let base = std::collections::HashMap::new();
        let mut left = std::collections::HashMap::new();
        left.insert("bd-tomb".to_string(), local_issue);
        let right = std::collections::HashMap::new();

        let context = MergeContext::new(base, left, right);
        let mut tombstones = std::collections::HashSet::new();
        tombstones.insert("bd-tomb".to_string());

        let report = three_way_merge(&context, ConflictResolution::PreferNewer, Some(&tombstones));

        // Should keep local even if tombstoned
        assert_eq!(report.kept.len(), 1);
        assert!(report.tombstone_protected.is_empty());
    }

    #[test]
    fn test_three_way_merge_tombstone_protection_blocks_external_winner() {
        let base = make_issue_with_hash("bd-tomb", "Base", fixed_time_merge(100), Some("base"));
        let mut local_tombstone =
            make_issue_with_hash("bd-tomb", "Deleted", fixed_time_merge(200), Some("deleted"));
        local_tombstone.status = crate::model::Status::Tombstone;
        local_tombstone.deleted_at = Some(fixed_time_merge(200));
        let external = make_issue_with_hash(
            "bd-tomb",
            "Resurrection attempt",
            fixed_time_merge(300),
            Some("external"),
        );

        let mut base_map = std::collections::HashMap::new();
        base_map.insert("bd-tomb".to_string(), base);
        let mut left = std::collections::HashMap::new();
        left.insert("bd-tomb".to_string(), local_tombstone);
        let mut right = std::collections::HashMap::new();
        right.insert("bd-tomb".to_string(), external);
        let context = MergeContext::new(base_map, left, right);
        let tombstones = std::collections::HashSet::from(["bd-tomb".to_string()]);

        let report = three_way_merge(
            &context,
            ConflictResolution::PreferExternal,
            Some(&tombstones),
        );

        assert!(report.conflicts.is_empty());
        assert_eq!(report.tombstone_protected, vec!["bd-tomb".to_string()]);
        assert_eq!(report.kept.len(), 1);
        assert_eq!(report.kept[0].status, crate::model::Status::Tombstone);
        assert_eq!(report.kept[0].title, "Deleted");
    }

    #[test]
    fn test_three_way_merge_deletions() {
        // Setup: issue in base but deleted in both left and right
        let base_issue =
            make_issue_with_hash("bd-del", "To Delete", fixed_time_merge(100), Some("hash1"));

        let mut base = std::collections::HashMap::new();
        base.insert("bd-del".to_string(), base_issue);

        let left = std::collections::HashMap::new();
        let right = std::collections::HashMap::new();

        let context = MergeContext::new(base, left, right);
        let report = three_way_merge(&context, ConflictResolution::PreferNewer, None);

        assert!(report.kept.is_empty());
        assert_eq!(report.deleted.len(), 1);
        assert!(report.deleted.contains(&"bd-del".to_string()));
    }

    #[test]
    fn test_three_way_merge_empty_context() {
        let context = MergeContext::default();
        let report = three_way_merge(&context, ConflictResolution::PreferNewer, None);

        assert!(report.kept.is_empty());
        assert!(report.deleted.is_empty());
        assert!(report.conflicts.is_empty());
        assert!(report.tombstone_protected.is_empty());
        assert!(report.notes.is_empty());
        assert_eq!(report.total_actions(), 0);
    }

    #[test]
    fn test_merge_conflict_manual_strategy() {
        // Setup: issue deleted externally but modified locally with Manual strategy
        let base_issue =
            make_issue_with_hash("bd-001", "Base", fixed_time_merge(100), Some("base_hash"));
        let local_issue = make_issue_with_hash(
            "bd-001",
            "Modified",
            fixed_time_merge(200),
            Some("mod_hash"),
        );

        let mut base = std::collections::HashMap::new();
        base.insert("bd-001".to_string(), base_issue);
        let mut left = std::collections::HashMap::new();
        left.insert("bd-001".to_string(), local_issue);
        let right = std::collections::HashMap::new();

        let context = MergeContext::new(base, left, right);
        let report = three_way_merge(&context, ConflictResolution::Manual, None);

        // With Manual strategy, delete-vs-modify should be a conflict
        assert_eq!(report.conflicts.len(), 1);
        assert!(matches!(
            report.conflicts[0].1,
            ConflictType::DeleteVsModify
        ));
    }

    #[test]
    fn test_three_way_merge_with_notes() {
        // Setup: issue modified in both left and right
        let base_issue = make_issue_with_hash(
            "bd-001",
            "Base Title",
            fixed_time_merge(100),
            Some("base_hash"),
        );
        let local_issue = make_issue_with_hash(
            "bd-001",
            "Local Modified",
            fixed_time_merge(200),
            Some("mod_hash"),
        );
        let external_issue = make_issue_with_hash(
            "bd-001",
            "External Modified",
            fixed_time_merge(300),
            Some("external_hash"),
        );

        let mut base = std::collections::HashMap::new();
        base.insert("bd-001".to_string(), base_issue);
        let mut left = std::collections::HashMap::new();
        left.insert("bd-001".to_string(), local_issue);
        let mut right = std::collections::HashMap::new();
        right.insert("bd-001".to_string(), external_issue);

        let context = MergeContext::new(base, left, right);
        let report = three_way_merge(&context, ConflictResolution::PreferNewer, None);

        // Should have a note about the merge decision
        assert_eq!(report.kept.len(), 1);
        assert_eq!(report.notes.len(), 1);
        assert!(report.notes[0].1.contains("Both modified"));
    }

    #[test]
    fn test_manual_merge_reports_both_modified_conflict() {
        let base_issue = make_issue_with_hash(
            "bd-001",
            "Base Title",
            fixed_time_merge(100),
            Some("base_hash"),
        );
        let local_issue = make_issue_with_hash(
            "bd-001",
            "Local Title",
            fixed_time_merge(200),
            Some("local_hash"),
        );
        let external_issue = make_issue_with_hash(
            "bd-001",
            "External Title",
            fixed_time_merge(300),
            Some("external_hash"),
        );

        let result = merge_issue(
            Some(&base_issue),
            Some(&local_issue),
            Some(&external_issue),
            ConflictResolution::Manual,
        );

        assert!(matches!(
            result,
            MergeResult::Conflict(ConflictType::BothModified)
        ));
    }

    #[test]
    fn test_manual_merge_reports_convergent_creation_conflict() {
        let local_issue = make_issue_with_hash(
            "bd-001",
            "Local Title",
            fixed_time_merge(200),
            Some("local_hash"),
        );
        let external_issue = make_issue_with_hash(
            "bd-001",
            "External Title",
            fixed_time_merge(300),
            Some("external_hash"),
        );

        let result = merge_issue(
            None,
            Some(&local_issue),
            Some(&external_issue),
            ConflictResolution::Manual,
        );

        assert!(matches!(
            result,
            MergeResult::Conflict(ConflictType::ConvergentCreation)
        ));
    }

    #[test]
    fn test_compute_jsonl_hash_ignores_empty_lines_and_whitespace() {
        let temp_dir = TempDir::new().unwrap();
        let path1 = temp_dir.path().join("file1.jsonl");
        let path2 = temp_dir.path().join("file2.jsonl");

        let content1 = "{\"id\":\"bd-1\"}\n{\"id\":\"bd-2\"}\n";
        // content2 has extra empty lines, different line endings, and extra whitespace
        let content2 = "\n{\"id\":\"bd-1\"}\r\n  \n{\"id\":\"bd-2\"}  \n\n";

        fs::write(&path1, content1).unwrap();
        fs::write(&path2, content2).unwrap();

        let hash1 = compute_jsonl_hash(&path1).unwrap();
        let hash2 = compute_jsonl_hash(&path2).unwrap();

        assert_eq!(
            hash1, hash2,
            "Hashes should be identical regardless of empty lines or whitespace"
        );
    }
}
