//! Doctor command implementation.

#![allow(clippy::option_if_let_else)]

use crate::cli::DoctorArgs;
use crate::cli::commands::doctor_subsystems::engine::{EngineBlock, engine_block};
use crate::cli::commands::doctor_subsystems::exit_codes::DoctorExitCode;
use crate::cli::commands::doctor_subsystems::mutate as chokepoint;
use crate::cli::commands::doctor_subsystems::mutate::{Capabilities, MutateContext, Op};
use crate::cli::commands::doctor_subsystems::refuse_gates::{self, GateOutcome};
use crate::cli::commands::doctor_subsystems::run_dir::{self, RunDir};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::franken_sync::{Connection, Row};
use crate::health::{AnomalyClass, ReliabilityAuditRecord, WorkspaceClassification};
use crate::output::OutputContext;
use crate::storage::SqliteStorage;
use crate::storage::sqlite::PendingSyncMergeInspection;
#[cfg(test)]
use crate::sync::METADATA_SYNC_MERGE_PENDING_LEGACY;
use crate::sync::{
    JsonlSourceSnapshot, JsonlTombstoneFilter, PathValidation, PreservedIssue,
    SyncMergePendingPhase, SyncMergePendingReceipt, blocking_jsonl_family_write_lock_with_timeout,
    capture_jsonl_source_snapshot, compute_staleness, dirty_issues_missing_from_jsonl,
    restore_dirty_issues_after_rebuild, restore_tombstones_after_rebuild, scan_conflict_markers,
    scan_conflict_markers_snapshot, scan_jsonl_snapshot_for_tombstone_filter,
    snapshot_dirty_live_issues, snapshot_tombstones, tombstones_missing_from_jsonl_tombstones,
    validate_jsonl_issue_records, validate_jsonl_snapshot_issue_records, validate_no_git_path,
    validate_sync_path, validate_sync_path_with_external,
};
use chrono::{NaiveDate, Utc};
use fsqlite_error::FrankenError;
use fsqlite_types::SqliteValue;
use rich_rust::prelude::*;
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

/// Check result status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum CheckStatus {
    Ok,
    Warn,
    Error,
}

#[derive(Debug, Clone, Serialize)]
struct CheckResult {
    name: String,
    status: CheckStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorReport {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    workspace_health: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reliability_audit: Option<ReliabilityAuditRecord>,
    checks: Vec<CheckResult>,
}

#[derive(Debug, Clone, Serialize)]
struct DoctorRepairResult {
    imported: usize,
    skipped: usize,
    fk_violations_cleaned: usize,
    /// Unflushed tombstones restored across the rebuild (deletion-retention
    /// state absent from the JSONL).
    preserved_tombstones: usize,
    /// Dirty live issues restored across the rebuild — local creations or
    /// edits that had not yet been flushed to the JSONL (GitHub #394).
    preserved_dirty_issues: usize,
    /// IDs of the restored dirty issues. Post-repair verification uses
    /// this set to accept the intentional DB-ahead-of-JSONL divergence
    /// those rows create until the next flush.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    preserved_dirty_issue_ids: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    verified_backups: Vec<config::RecoveryBackupVerification>,
    /// Per-table counts of DB-only history rows carried across the JSONL
    /// rebuild (GitHub #471). The JSONL export holds issue state only, so
    /// events / gate results / close audit / capacity records must be
    /// snapshotted from the pre-repair database and restored afterwards.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    preserved_history: Vec<PreservedHistoryTableReport>,
    /// Loud, human-readable warnings for history that could NOT be preserved
    /// (e.g. a corrupt page made a table unreadable before the rebuild).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    history_preservation_warnings: Vec<String>,
}

/// One preserved history table's restore outcome (GitHub #471).
#[derive(Debug, Clone, Serialize)]
struct PreservedHistoryTableReport {
    table: String,
    /// Rows restored into the rebuilt database.
    restored: usize,
    /// Rows skipped because their issue no longer exists after the rebuild
    /// (the JSONL is authoritative for issue membership).
    skipped: usize,
}

#[derive(Debug, Clone)]
struct DoctorRun {
    report: DoctorReport,
    jsonl_path: Option<PathBuf>,
}

/// Classification of durable sync-merge state found in database metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingSyncMergeCondition {
    /// A current v2 receipt passed its schema and intent-hash validation.
    Valid,
    /// An older receipt exists and requires an explicit compatible recovery path.
    Legacy,
    /// Current metadata is ambiguous, empty, malformed, or failed validation.
    Malformed,
}

/// Redacted, machine-readable pending sync-merge state for startup guards and
/// doctor diagnostics.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PendingSyncMergeState {
    /// Whether the receipt is valid, legacy, or malformed.
    pub condition: PendingSyncMergeCondition,
    /// Metadata key that caused the pending-state classification.
    pub metadata_key: String,
    /// Stable receipt identity when a valid v2 receipt supplied one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt_id: Option<String>,
    /// Durable saga phase when a valid v2 receipt supplied one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    /// Merge conflict-resolution policy when a valid v2 receipt supplied one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<String>,
    /// Receipt creation time when a valid v2 receipt supplied one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// Expected post-merge JSONL record count when available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_jsonl_issue_count: Option<usize>,
    /// Actionable explanation. Receipt payload bodies are never exposed.
    pub diagnostic: String,
}

impl PendingSyncMergeState {
    /// Return a stable lowercase condition name for logs and robot envelopes.
    #[must_use]
    pub const fn condition_name(&self) -> &'static str {
        match self.condition {
            PendingSyncMergeCondition::Valid => "valid",
            PendingSyncMergeCondition::Legacy => "legacy",
            PendingSyncMergeCondition::Malformed => "malformed",
        }
    }

    fn valid(receipt: &SyncMergePendingReceipt) -> Self {
        let phase = match receipt.phase {
            SyncMergePendingPhase::DatabaseCommitted => "database_committed",
            SyncMergePendingPhase::ExportFinalized => "export_finalized",
        };
        Self {
            condition: PendingSyncMergeCondition::Valid,
            metadata_key: crate::sync::METADATA_SYNC_MERGE_PENDING.to_string(),
            receipt_id: Some(receipt.receipt_id.clone()),
            phase: Some(phase.to_string()),
            resolution: Some(receipt.intent.resolution.clone()),
            created_at: Some(receipt.created_at.clone()),
            expected_jsonl_issue_count: Some(receipt.jsonl_after_issue_count),
            diagnostic: "A committed sync merge still requires JSONL/base artifact reconciliation"
                .to_string(),
        }
    }

    fn legacy(metadata_key: String, row_count: usize, diagnostic: &str) -> Self {
        Self {
            condition: PendingSyncMergeCondition::Legacy,
            metadata_key,
            receipt_id: None,
            phase: None,
            resolution: None,
            created_at: None,
            expected_jsonl_issue_count: None,
            diagnostic: format!("{diagnostic} ({row_count} metadata row(s))"),
        }
    }

    fn malformed(metadata_key: impl Into<String>, diagnostic: String) -> Self {
        Self {
            condition: PendingSyncMergeCondition::Malformed,
            metadata_key: metadata_key.into(),
            receipt_id: None,
            phase: None,
            resolution: None,
            created_at: None,
            expected_jsonl_issue_count: None,
            diagnostic,
        }
    }

    fn from_inspection(inspection: PendingSyncMergeInspection) -> Option<Self> {
        match inspection {
            PendingSyncMergeInspection::Absent => None,
            PendingSyncMergeInspection::Valid(receipt) => Some(Self::valid(&receipt)),
            PendingSyncMergeInspection::Legacy {
                metadata_key,
                row_count,
                diagnostic,
            } => Some(Self::legacy(metadata_key, row_count, &diagnostic)),
            PendingSyncMergeInspection::Malformed {
                metadata_key,
                diagnostic,
            } => Some(Self::malformed(metadata_key, diagnostic)),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DoctorInspectionMode {
    Full,
    Quick,
}

#[derive(Debug, Clone, Default, Serialize)]
struct LocalRepairResult {
    blocked_cache_rebuilt: bool,
    indexes_reindexed: bool,
    vacuumed: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    quarantined_artifacts: Vec<String>,
}

/// Per-`--repair` invocation state: owns the on-disk run-artifact directory
/// and the [`MutateContext`] that every WP3-rewired fixer threads its writes
/// through.
///
/// Constructed once at the top of the `--repair` flow and lazily threaded
/// down to each fixer that has been migrated to the chokepoint. Fixers that
/// are still pre-WP4 (DB-only SQL paths, deep config rebuilds) currently
/// ignore this and fall back to their legacy in-place writes; their
/// migration is tracked under WP4+.
///
/// On `dry_run`, every routed call prints `[dry-run] would mutate …` to
/// stderr without touching disk. The run-dir is still created (so the
/// caller has somewhere to inspect the planned actions) but no
/// `actions.jsonl` lines are appended.
struct DoctorRepairSession {
    run: RunDir,
    ctx: MutateContext,
}

impl DoctorRepairSession {
    /// Build a fresh session rooted at `repo_root`. Creates
    /// `<repo_root>/.doctor/runs/<run-id>/` (or the
    /// `BR_DOCTOR_RUNS_DIR` override) and seats a `MutateContext` with the
    /// default `.beads/` + `.doctor/` capabilities, plus the explicit root
    /// `.gitignore` path so the gitignore fixer can use the chokepoint
    /// without widening write_scopes to the entire repo root.
    ///
    /// `fixer_id` may be a placeholder; callers update it via
    /// [`Self::with_fixer`] before each `mutate()` call.
    fn new(repo_root: &Path, dry_run: bool) -> Result<Self> {
        let (run, actions_file) = run_dir::create_repair_run_dir(repo_root)?;
        let mut capabilities = Capabilities::for_repo(repo_root);
        // Allow the root .gitignore explicitly. It lives at <repo_root>/
        // which is outside .beads/ and .doctor/.
        capabilities.write_scopes.push(repo_root.join(".gitignore"));
        let ctx = MutateContext {
            run_id: run.run_id.clone(),
            run_dir: run.root.clone(),
            capabilities,
            actions_file: Mutex::new(actions_file),
            fixer_id: "doctor".to_string(),
            repo_root: repo_root.to_path_buf(),
            dry_run,
            start_ns: now_ns_for_session(),
        };
        Ok(Self { run, ctx })
    }

    /// Replace the `fixer_id` in-place so the next `mutate()` call records
    /// the right fixer in `actions.jsonl`.
    fn set_fixer(&mut self, fixer_id: &str) {
        self.ctx.fixer_id = fixer_id.to_string();
    }

    /// Wrap a legacy `repair_*` call that mutates one or more files
    /// directly (e.g., VACUUM, REINDEX, blocked-cache rebuild). The
    /// helper snapshots each target verbatim into the run-dir BEFORE
    /// the closure runs and appends a `legacy_op` line to
    /// `actions.jsonl` per target AFTER, so `br doctor undo` can still
    /// restore the pre-mutation bytes from `<run-dir>/backups/`.
    ///
    /// The legacy closure does whatever in-place SQL or rename work the
    /// fixer already implements; this wrapper only adds before/after
    /// audit + verbatim backup. Use
    /// [`Self::record_legacy_mutation_result`] when callers need to
    /// receive the closure's output after the audit wrapper runs.
    fn record_legacy_mutation<F>(
        &mut self,
        fixer_id: &str,
        paths: &[&Path],
        legacy: F,
    ) -> Result<()>
    where
        F: FnOnce() -> Result<()>,
    {
        self.record_legacy_mutation_result(fixer_id, paths, legacy)
            .map(|_| ())
    }

    fn record_legacy_mutation_result<T, F>(
        &mut self,
        fixer_id: &str,
        paths: &[&Path],
        legacy: F,
    ) -> Result<Option<T>>
    where
        F: FnOnce() -> Result<T>,
    {
        let prior_fixer = std::mem::replace(&mut self.ctx.fixer_id, fixer_id.to_string());
        let mut legacy_output = None;
        let result = chokepoint::record_legacy_op(&self.ctx, fixer_id, paths, || {
            legacy_output = Some(legacy()?);
            Ok(())
        });
        self.ctx.fixer_id = prior_fixer;
        result.map(|()| legacy_output)
    }
}

#[allow(dead_code)] // Used by DoctorRepairSession once the scaffold is wired into repair flow.
/// Pass-5 cycle 1: per-FM filter for `--repair --only`/`--skip`.
///
/// Each chokepointed fixer takes the active filter and consults
/// `allows(fm_id)` before running. The filter is built once at the top
/// of `execute()` from `args.only` and `args.skip`. Empty `only` means
/// "all fixers eligible"; `skip` always subtracts from the eligible set.
///
/// Stable contract: FM identifiers are the `fm-<subsystem>-<slug>` form
/// advertised in the capabilities envelope's `finding_id_map`. Unknown
/// names in `only`/`skip` are NOT rejected — the filter is lenient so
/// callers can preconfigure invocations against future fixers.
#[derive(Debug, Clone, Default)]
pub(crate) struct FixerFilter {
    only: Vec<String>,
    skip: Vec<String>,
}

impl FixerFilter {
    pub(crate) fn from_args(only: &[String], skip: &[String]) -> Self {
        Self {
            only: only
                .iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
            skip: skip
                .iter()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect(),
        }
    }

    /// Returns true when `fm_id` should run under the current filter.
    #[must_use]
    pub(crate) fn allows(&self, fm_id: &str) -> bool {
        if !self.only.is_empty() && !self.only.iter().any(|s| s == fm_id) {
            return false;
        }
        if self.skip.iter().any(|s| s == fm_id) {
            return false;
        }
        true
    }

    /// Whether the filter has any non-empty `--only` allowlist. Used by
    /// the legacy-path tracing to log when filtering is in effect.
    #[must_use]
    pub(crate) fn has_only(&self) -> bool {
        !self.only.is_empty()
    }

    /// Whether the filter has any non-empty `--skip` blocklist.
    #[must_use]
    pub(crate) fn has_skip(&self) -> bool {
        !self.skip.is_empty()
    }
}

const FM_BLOCKED_CACHE_STALE: &str = "fm-caches_indexes-blocked-cache-stale";
const FM_PARTIAL_INDEX_STALE: &str = "fm-caches_indexes-partial-index-stale";
const FM_JSONL_ROW_COUNT_MISMATCH: &str = "fm-state_files-jsonl-row-count-mismatch";
const FM_EMPTY_OR_TRUNCATED_DATABASE: &str = "fm-state_files-empty-or-truncated-database";
const FM_SQLITE_PAGE_MALFORMED: &str = "fm-state_files-sqlite-page-malformed";
const FM_WAL_SHM_SIDECAR_ORPHAN: &str = "fm-state_files-wal-shm-sidecar-orphan";
const FM_MISSING_REQUIRED_TABLE: &str = "fm-schemas-missing-required-table";
const FM_MISSING_REQUIRED_COLUMN: &str = "fm-schemas-missing-required-column";
const JSONL_REBUILD_FILTERED_REASON: &str =
    "JSONL rebuild filtered out by --only/--skip (no rebuild-addressed FM allowed)";

fn now_ns_for_session() -> u128 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

#[derive(Debug, Clone, Serialize)]
struct RecoveryAuditRecord {
    phase: String,
    action: String,
    outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    applied_actions: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    quarantined_artifacts: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    verified_backups: Vec<config::RecoveryBackupVerification>,
    #[serde(skip_serializing_if = "Option::is_none")]
    imported: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    skipped: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    fk_violations_cleaned: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PriorJsonlRebuildFailureEvidence {
    path: PathBuf,
    artifact_count: usize,
}

const BLOCKED_CACHE_STALE_FINDING: &str = "blocked_issues_cache is marked stale and needs rebuild";
const BLOCKED_CACHE_CONTENT_MISMATCH_FINDING: &str =
    "blocked_issues_cache content differs from direct dependency graph and needs rebuild";
const READY_PROJECTION_CONTENT_MISMATCH_FINDING: &str =
    "ready projection content differs from direct dependency graph and needs rebuild";
const JSONL_REBUILD_AUTHORITY_ERROR_PREFIX: &str = "Cannot repair: JSONL authority is unsafe";
const JSONL_REBUILD_REPEAT_ERROR_PREFIX: &str =
    "Cannot repair: previous JSONL rebuild verification failed";
/// Emitted when `--dry-run` declines the rebuild. Nothing failed and neither
/// store is implicated, so it is surfaced verbatim rather than being wrapped in
/// a repair-failure message (`beads_rust-krdi`).
const JSONL_REBUILD_DRY_RUN_SKIP_MESSAGE: &str =
    "doctor --repair --dry-run skipped JSONL rebuild; no database writes were applied";
const JSONL_REBUILD_VERIFICATION_FAILED_SUFFIX: &str = ".verification-failed.json";
const ROOT_GITIGNORE_OFFENDING_PATTERNS: &[&str] = &[
    ".beads",
    ".beads/",
    ".beads/*",
    ".beads/**",
    ".beads/.gitignore",
    "/.beads",
    "/.beads/",
    "/.beads/*",
    "/.beads/**",
    "/.beads/.gitignore",
];
const ROOT_GITIGNORE_REPAIR_MESSAGE: &str =
    "Removed offending .beads ignore pattern(s) from root .gitignore";
const NO_OP_REPAIR_MESSAGE: &str = "No errors detected; nothing to repair.";
const REINDEX_INCOMPLETE_MESSAGE: &str = "REINDEX was attempted but did not complete.";

fn is_quick_suppressed_doctor_check(name: &str) -> bool {
    matches!(
        name,
        "db.recoverable_anomalies"
            | "counts.db_vs_jsonl"
            | "sync.metadata"
            | "sqlite.cli_integrity"
            | "sqlite3.integrity_check"
            | "db.write_probe"
    )
}

#[derive(Debug, Default)]
struct SidecarInspection {
    /// Error-level findings that indicate a genuine problem requiring repair.
    findings: Vec<String>,
    /// Informational findings for valid engine-specific sidecar layouts.
    informational_findings: Vec<String>,
    quarantine_candidates: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilesystemPathKind {
    Missing,
    File,
    Directory,
    Symlink,
    Other,
}

impl LocalRepairResult {
    fn applied(&self) -> bool {
        self.blocked_cache_rebuilt
            || self.indexes_reindexed
            || self.vacuumed
            || !self.quarantined_artifacts.is_empty()
    }
}

fn local_repair_applied_actions(repair: &LocalRepairResult) -> Vec<String> {
    let mut actions = Vec::new();
    if repair.blocked_cache_rebuilt {
        actions.push("blocked_cache_rebuilt".to_string());
    }
    if repair.indexes_reindexed {
        actions.push("indexes_reindexed".to_string());
    }
    if repair.vacuumed {
        actions.push("vacuumed".to_string());
    }
    if !repair.quarantined_artifacts.is_empty() {
        actions.push("quarantined_artifacts".to_string());
    }
    actions
}

fn local_repair_audit_record(
    phase: &str,
    outcome: &str,
    repair: &LocalRepairResult,
    reason: Option<String>,
) -> RecoveryAuditRecord {
    RecoveryAuditRecord {
        phase: phase.to_string(),
        action: "local_repair".to_string(),
        outcome: outcome.to_string(),
        reason,
        applied_actions: local_repair_applied_actions(repair),
        quarantined_artifacts: repair.quarantined_artifacts.clone(),
        verified_backups: Vec::new(),
        imported: None,
        skipped: None,
        fk_violations_cleaned: None,
    }
}

fn jsonl_rebuild_audit_record(
    phase: &str,
    outcome: &str,
    repair: Option<&DoctorRepairResult>,
    reason: Option<String>,
) -> RecoveryAuditRecord {
    RecoveryAuditRecord {
        phase: phase.to_string(),
        action: "jsonl_rebuild".to_string(),
        outcome: outcome.to_string(),
        reason,
        applied_actions: Vec::new(),
        quarantined_artifacts: Vec::new(),
        verified_backups: repair.map_or_else(Vec::new, |result| result.verified_backups.clone()),
        imported: repair.map(|result| result.imported),
        skipped: repair.map(|result| result.skipped),
        fk_violations_cleaned: repair.map(|result| result.fk_violations_cleaned),
    }
}

fn emit_recovery_audit_record(record: &RecoveryAuditRecord) {
    let applied_actions = record.applied_actions.join(",");
    tracing::info!(
        target: "br::reliability",
        phase = %record.phase,
        action = %record.action,
        outcome = %record.outcome,
        reason = record.reason.as_deref().unwrap_or(""),
        applied_actions = %applied_actions,
        quarantined_artifacts = record.quarantined_artifacts.len(),
        verified_backups = record.verified_backups.len(),
        verified_backup_details = ?record.verified_backups,
        imported = record.imported.unwrap_or(0),
        skipped = record.skipped.unwrap_or(0),
        fk_violations_cleaned = record.fk_violations_cleaned.unwrap_or(0),
        "doctor recovery audit record"
    );
}

/// Emit a `ConcurrencyLost` refusal payload before exiting with code 5.
///
/// Used by doctor repair lock guards when another process holds the
/// `.write.lock`. JSON callers receive a structured envelope with
/// `exit_code: 5`, `code: "concurrency_lost"`, and the underlying
/// timeout error text; non-JSON callers get a one-line error on stderr.
/// The message intentionally names `.write.lock` so agent scripts can
/// match on it the same way they do for other contention paths.
fn emit_concurrency_lost(beads_dir: &Path, err: &BeadsError, ctx: &OutputContext, operation: &str) {
    let lock_path = beads_dir.join(".write.lock");
    let detail = err.to_string();
    let recovery_audit = RecoveryAuditRecord {
        phase: "doctor.concurrency".to_string(),
        action: "acquire_workspace_write_lock".to_string(),
        outcome: "refused".to_string(),
        reason: Some(format!(
            "workspace write lock at {} is held by another process: {detail}",
            lock_path.display()
        )),
        applied_actions: Vec::new(),
        quarantined_artifacts: Vec::new(),
        verified_backups: Vec::new(),
        imported: None,
        skipped: None,
        fk_violations_cleaned: None,
    };
    emit_recovery_audit_record(&recovery_audit);
    if ctx.is_json() {
        ctx.json(&serde_json::json!({
            "ok": false,
            "exit_code": DoctorExitCode::ConcurrencyLost.as_i32(),
            "code": DoctorExitCode::ConcurrencyLost.as_str(),
            "message": format!(
                "Refusing {operation}: workspace write lock at {} is held by another process",
                lock_path.display()
            ),
            "detail": detail,
            "lock_path": lock_path.display().to_string(),
            "recovery_audit": recovery_audit,
        }));
    } else {
        ctx.error(&format!(
            "Refusing {operation}: workspace write lock at {} is held by another process. \
             Wait for the other br invocation to finish or pass --lock-timeout to wait longer. \
             Underlying error: {detail}",
            lock_path.display()
        ));
    }
}

/// Round-5 fresh-eyes follow-through (`beads_rust-73ux`): emit the
/// structured refusal envelope when a WP1 [`refuse_gates`] gate refuses
/// to allow `--repair` to proceed.
///
/// Mirrors [`emit_concurrency_lost`]: a [`RecoveryAuditRecord`] carrying
/// the gate's reason is logged unconditionally, and JSON callers receive
/// a top-level envelope with `exit_code: 4`,
/// `code: "refused_unsafe"`, and the gate's `evidence` payload (so
/// agent scripts can inspect which gate refused and why without parsing
/// the human message). Non-JSON callers get a one-line refusal on stderr.
///
/// The caller is expected to follow this with
/// `process::exit(DoctorExitCode::RefusedUnsafe.as_i32())`.
fn emit_refused_unsafe(
    operation: &str,
    reason: &str,
    evidence: &serde_json::Value,
    ctx: &OutputContext,
) {
    let gate_name = evidence
        .get("gate")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let recovery_audit = RecoveryAuditRecord {
        phase: "doctor.refuse_gate".to_string(),
        action: format!("gate:{gate_name}"),
        outcome: "refused".to_string(),
        reason: Some(reason.to_string()),
        applied_actions: Vec::new(),
        quarantined_artifacts: Vec::new(),
        verified_backups: Vec::new(),
        imported: None,
        skipped: None,
        fk_violations_cleaned: None,
    };
    emit_recovery_audit_record(&recovery_audit);
    if ctx.is_json() {
        ctx.json(&serde_json::json!({
            "ok": false,
            "exit_code": DoctorExitCode::RefusedUnsafe.as_i32(),
            "code": DoctorExitCode::RefusedUnsafe.as_str(),
            "message": reason,
            "gate": gate_name,
            "evidence": evidence,
            "recovery_audit": recovery_audit,
        }));
    } else {
        ctx.error(&format!(
            "Refusing {operation}: {reason} (gate={gate_name})"
        ));
    }
}

fn refuse_doctor_mutation_if_merge_pending(
    operation: &str,
    db_path: &Path,
    authority: &Arc<crate::sync::DatabaseFamilyWriteLock>,
    ctx: &OutputContext,
) {
    let refusal = match inspect_pending_sync_merge_under_authority(db_path, authority) {
        Ok(None) => return,
        Ok(Some(state)) => {
            let reason = format!(
                "{}; only `br sync --merge` may reconcile this state before generic doctor mutation",
                state.diagnostic
            );
            let evidence = serde_json::json!({
                "gate": "sync.merge_pending",
                "finding": "sync.merge_pending",
                "pending": true,
                "state": state,
                "database_path": db_path.display().to_string(),
                "remediation": "Run `br sync --merge`, verify that it clears the pending receipt, then rerun the requested doctor operation."
            });
            (reason, evidence)
        }
        Err(error) => {
            let reason = format!(
                "could not prove that no sync merge is pending ({error}); doctor mutation fails closed"
            );
            let evidence = serde_json::json!({
                "gate": "sync.merge_pending",
                "finding": "sync.merge_pending",
                "pending": "unknown",
                "database_path": db_path.display().to_string(),
                "inspection_error": error.to_string(),
                "remediation": "Restore read-only access to the database family and rerun `br doctor` before attempting repair."
            });
            (reason, evidence)
        }
    };
    emit_refused_unsafe(operation, &refusal.0, &refusal.1, ctx);
    crate::shutdown::exit_process(DoctorExitCode::RefusedUnsafe.as_i32());
}

impl FilesystemPathKind {
    fn exists(self) -> bool {
        !matches!(self, Self::Missing)
    }

    fn is_regular_file(self) -> bool {
        matches!(self, Self::File)
    }

    fn description(self) -> &'static str {
        match self {
            Self::Missing => "missing",
            Self::File => "regular file",
            Self::Directory => "directory",
            Self::Symlink => "symlink",
            Self::Other => "special filesystem entry",
        }
    }
}

fn push_check(
    checks: &mut Vec<CheckResult>,
    name: &str,
    status: CheckStatus,
    message: Option<String>,
    details: Option<serde_json::Value>,
) {
    // Pass-3 finding-id schema unification: if the check name has
    // a canonical FM mapping in CHECK_NAME_TO_FINDING_ID, inject
    // `finding_id` into the details payload so agents see it
    // inline next to every check (no separate capabilities-envelope
    // lookup required). The injection is additive: callers that
    // pass a JSON object get a new key; callers that pass non-object
    // or None get a fresh object wrapping the FM id alongside the
    // original details under a "data" key.
    let details_with_fm = match finding_id_for(name) {
        Some(fm) => Some(inject_finding_id(details, fm)),
        None => details,
    };
    checks.push(CheckResult {
        name: name.to_string(),
        status,
        message,
        details: details_with_fm,
    });
}

/// Inject `finding_id: "fm-..."` into a CheckResult's details
/// payload. Three input shapes:
/// - `None` -> `Some({"finding_id": fm})`
/// - `Some(Object{..})` -> `Some(Object{"finding_id": fm, ..orig})`
/// - `Some(non-object)` -> `Some({"finding_id": fm, "data": orig})`
///   (defensive; no detector emits non-object details today)
fn inject_finding_id(details: Option<serde_json::Value>, fm: &'static str) -> serde_json::Value {
    use serde_json::Value;
    match details {
        None => serde_json::json!({ "finding_id": fm }),
        Some(Value::Object(mut map)) => {
            // First-write wins: don't override a caller-supplied
            // finding_id (preserves the contract that detectors can
            // assert authority over their own FM mapping).
            map.entry("finding_id".to_string())
                .or_insert(Value::String(fm.to_string()));
            Value::Object(map)
        }
        Some(other) => serde_json::json!({
            "finding_id": fm,
            "data": other,
        }),
    }
}

/// Canonical mapping from `check.name` values (emitted by `push_check`)
/// to `fm-<subsystem>-<slug>` FM identifiers from the Phase-1
/// archaeology in `/data/projects/beads_rust__doctor_workspace/analysis/
/// failure_modes/`. Pass-3 (gap item #3, `diagnostic_specificity`):
/// agents reading `br doctor --json` get the human check name today,
/// but no stable cross-pass identifier they can pin tooling to. This
/// table is the bridge — the capabilities envelope advertises the
/// mapping so consumers can translate either way.
///
/// Additive contract: not every check has an FM mapping (some are
/// scaffolding-only or new pass-3 entries). Lookups return `None`
/// for unmapped names; absence is not a fatal contract violation.
/// As new detectors land, append their (check_name, fm_id) tuple
/// here.
///
/// Stability: the FM identifiers are the canonical form
/// `fm-<subsystem>-<slug>`. Renaming an existing mapping is a
/// breaking change to the agent-facing contract; appending new
/// rows is additive.
pub(crate) const CHECK_NAME_TO_FINDING_ID: &[(&str, &str)] = &[
    // state_files (pass-1 archaeology + pass-1 / pass-2 detectors)
    ("jsonl.parse", "fm-state_files-jsonl-malformed-utf8"),
    ("sync.merge_pending", "fm-state_files-sync-merge-pending"),
    (
        "jsonl.merge_artifacts",
        "fm-state_files-merge-artifact-stuck",
    ),
    ("base_jsonl", "fm-state_files-base-jsonl-missing-or-stale"),
    (
        "base_jsonl.missing_post_flush",
        "fm-state_files-base-jsonl-missing-or-stale",
    ),
    ("dirty_bitmap", "fm-caches_indexes-dirty-bitmap-divergence"),
    (
        "doctor.runs_dir",
        "fm-observability-doctor-runs-dir-grows-unbounded",
    ),
    (
        "doctor.runs_creatable",
        "fm-permissions-doctor-runs-not-creatable",
    ),
    (
        "permissions.recovery_dir",
        "fm-permissions-recovery-dir-not-writable",
    ),
    (
        "permissions.write_lock",
        "fm-state_files-orphaned-write-lock",
    ),
    (
        "permissions.root_gitignore",
        "fm-permissions-gitignore-not-writable-blocks-repair",
    ),
    (
        "permissions.db_sidecars",
        "fm-permissions-db-sidecar-mode-too-open",
    ),
    ("jsonl.duplicate_ids", "fm-state_files-jsonl-duplicate-ids"),
    ("comments.orphans", "fm-caches_indexes-comments-orphans"),
    ("labels.orphans", "fm-caches_indexes-labels-orphans"),
    (
        "dependencies.orphans",
        "fm-caches_indexes-dependencies-orphans",
    ),
    (
        "permissions.config_yaml_secrets",
        "fm-permissions-config-yaml-mode-leaks-secrets",
    ),
    ("br_path_dupes", "fm-external_artifacts-multiple-br-in-path"),
    (
        "gitignore.beads_inner_present",
        "fm-configs-gitignore-leaking-beads",
    ),
    (
        "permissions.jsonl_world_writable",
        "fm-permissions-jsonl-world-writable",
    ),
    ("tmp_files_orphan", "fm-state_files-orphan-tmp-files"),
    ("jsonl_size", "fm-state_files-jsonl-oversized"),
    (
        "br_history.size",
        "fm-state_files-br-history-grows-unbounded",
    ),
    (
        "jsonl_eof_newline",
        "fm-state_files-jsonl-missing-trailing-newline",
    ),
    ("jsonl_crlf", "fm-state_files-jsonl-crlf-line-endings"),
    ("jsonl_bom", "fm-state_files-jsonl-utf8-bom-prefix"),
    ("db_bloat", "fm-caches_indexes-db-bloat-vs-jsonl"),
    ("wal_size", "fm-state_files-wal-oversized"),
    ("startup_cache.health", "fm-configs-startup-cache-poisoned"),
    ("sync_jsonl_path", FM_JSONL_ROW_COUNT_MISMATCH),
    (
        "sync_conflict_markers",
        "fm-state_files-jsonl-conflict-markers",
    ),
    ("db.exists", FM_EMPTY_OR_TRUNCATED_DATABASE),
    ("db.open", FM_SQLITE_PAGE_MALFORMED),
    ("db.sidecars", FM_WAL_SHM_SIDECAR_ORPHAN),
    (
        "db.read_only_open_observational",
        "fm-state_files-read-only-open-not-observational",
    ),
    (
        "db.recovery_artifacts",
        "fm-state_files-recovery-artifacts-orphaned",
    ),
    (
        "db.recovery_artifacts.aged",
        "fm-state_files-recovery-artifacts-orphaned",
    ),
    (
        "db.foreign_recovery_debris",
        "fm-state_files-recovery-artifacts-orphaned",
    ),
    (
        "db.export_hash_cache",
        "fm-caches_indexes-export-hash-cache-divergence",
    ),
    ("db.recoverable_anomalies", FM_BLOCKED_CACHE_STALE),
    ("counts.db_vs_jsonl", FM_JSONL_ROW_COUNT_MISMATCH),
    ("sync.metadata", "fm-state_files-dirty-flag-divergence"),
    ("sqlite.integrity_check", FM_SQLITE_PAGE_MALFORMED),
    ("sqlite3.integrity_check", FM_SQLITE_PAGE_MALFORMED),
    ("db.write_probe", FM_SQLITE_PAGE_MALFORMED),
    ("db.null_defaults", "fm-schemas-missing-required-column"),
    // schemas
    ("schema.tables", FM_MISSING_REQUIRED_TABLE),
    ("schema.columns", FM_MISSING_REQUIRED_COLUMN),
    ("schema.inspect", "fm-schemas-issue-column-order-divergence"),
    // configs
    ("beads_dir", "fm-configs-metadata-json-stale"),
    ("metadata", "fm-configs-metadata-json-stale"),
    ("metadata.json", "fm-configs-metadata-json-stale"),
    (
        "gitignore.beads_inner",
        "fm-configs-gitignore-leaking-beads",
    ),
    ("gitignore.root", "fm-configs-gitignore-leaking-beads"),
    ("config.yaml", "fm-configs-yaml-malformed"),
    ("config.unknown_keys", "fm-configs-unknown-keys"),
    // agent_coordination
    (
        "audit.suspect_close_reasons",
        "fm-agent_coordination-suspect-close-reason",
    ),
    (
        "policy.workflow_statuses",
        "fm-agent_coordination-workflow-status-out-of-set",
    ),
    // routes_external
    ("routes_jsonl", "fm-routes_external-routes-jsonl-corrupt"),
    ("routes.targets", "fm-routes_external-route-target-missing"),
    // observability
    ("rust_log", "fm-observability-rust-log-noisy-breaks-json"),
    // permissions
    ("permissions.beads_dir", "fm-permissions-beads-dir-readonly"),
    // external_artifacts
    (
        "binary_version",
        "fm-external_artifacts-binary-version-mismatch",
    ),
    // concurrency_primitives
    (
        "write_lock",
        "fm-concurrency_primitives-orphaned-write-lock",
    ),
    // #329: `--no-db` (JSONL-only) mode marker — enumerates DB-backed checks
    // that were intentionally skipped.
    (
        "db.no_db_mode",
        "fm-state_files-no-db-mode-db-checks-skipped",
    ),
    // #350: dependency-graph JSONL-audit findings.
    (
        "dep.dead_closed_blocking_edges",
        "fm-dependencies-dead-closed-blocking-edges",
    ),
    (
        "dep.fully_unblocked_open",
        "fm-dependencies-fully-unblocked-open-issues",
    ),
];

/// Look up the canonical `fm-<subsystem>-<slug>` FM identifier for a
/// `check.name` value. Returns `None` for unmapped names (scaffolding
/// checks, new pass-3+ entries not yet registered). Lookup is O(N)
/// over the static table; N is small (~32) so a HashMap would be
/// overkill for a per-doctor-run call.
#[must_use]
#[allow(dead_code)] // public-by-design lookup helper; the table is also iterated directly by capabilities_doctor.
fn finding_id_for(check_name: &str) -> Option<&'static str> {
    CHECK_NAME_TO_FINDING_ID
        .iter()
        .find(|(name, _)| *name == check_name)
        .map(|(_, fm_id)| *fm_id)
}

fn has_error(checks: &[CheckResult]) -> bool {
    checks
        .iter()
        .any(|check| matches!(check.status, CheckStatus::Error))
}

fn has_non_ok(checks: &[CheckResult]) -> bool {
    checks
        .iter()
        .any(|check| !matches!(check.status, CheckStatus::Ok))
}

/// Inspect pending sync-merge metadata through a current-schema read-only
/// connection and one coherent read transaction.
///
/// This is an advisory inspection for doctor reporting and startup warnings.
/// Mutation gates must use [`inspect_pending_sync_merge_under_authority`].
/// Missing databases, stale/future schemas, open failures, and query failures
/// are errors rather than being mistaken for the safe absent state.
///
/// # Errors
///
/// Returns an error if the database family cannot be inspected from a stable
/// snapshot.
pub fn inspect_pending_sync_merge_at_path(db_path: &Path) -> Result<Option<PendingSyncMergeState>> {
    match fs::symlink_metadata(db_path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(BeadsError::SyncConflict {
                message: format!(
                    "Cannot inspect pending sync-merge state because database path '{}' is a symlink",
                    db_path.display()
                ),
            });
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(BeadsError::SyncConflict {
                message: format!(
                    "Cannot inspect pending sync-merge state because database path '{}' is not a regular file",
                    db_path.display()
                ),
            });
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(BeadsError::SyncConflict {
                message: format!(
                    "Pending sync-merge state is unknown because database '{}' is missing",
                    db_path.display()
                ),
            });
        }
        Err(error) => {
            return Err(BeadsError::WithContext {
                context: format!(
                    "Failed to inspect database path '{}' before checking pending sync-merge state",
                    db_path.display()
                ),
                source: Box::new(error),
            });
        }
    }

    let Some(storage) = SqliteStorage::open_current_read_only(db_path)? else {
        // The lock-free lane also declines when an fsqlite namespace sidecar
        // needs the authority-gated owner-only mode repair; that is not a
        // schema problem and must not be reported as one (GitHub #491).
        if SqliteStorage::namespace_sidecars_need_mode_repair(db_path)? {
            return Err(BeadsError::SyncConflict {
                message: format!(
                    "Pending sync-merge state is unknown because an fsqlite namespace sidecar beside database '{}' is group/other accessible; the repair to owner-only (0600) runs once the database-family authority is held",
                    db_path.display()
                ),
            });
        }
        return Err(BeadsError::SyncConflict {
            message: format!(
                "Pending sync-merge state is unknown because database '{}' does not have the current supported schema",
                db_path.display()
            ),
        });
    };
    Ok(PendingSyncMergeState::from_inspection(
        storage.inspect_pending_sync_merge()?,
    ))
}

/// Inspect pending sync-merge metadata while the caller holds inode-bound
/// authority over the database family.
///
/// This is the definitive mutation gate. Authority is verified before and
/// after opening the current-schema read-only connection and around its
/// coherent read transaction. Only an exact absence of both metadata keys
/// returns `Ok(None)`.
///
/// # Errors
///
/// Returns an error for authority drift or any missing, stale, unsupported,
/// unreadable, or unclassifiable database state.
pub fn inspect_pending_sync_merge_under_authority(
    db_path: &Path,
    authority: &Arc<crate::sync::DatabaseFamilyWriteLock>,
) -> Result<Option<PendingSyncMergeState>> {
    Ok(PendingSyncMergeState::from_inspection(
        SqliteStorage::inspect_pending_sync_merge_under_authority(db_path, authority)?,
    ))
}

fn check_pending_sync_merge(db_path: &Path, checks: &mut Vec<CheckResult>) {
    match inspect_pending_sync_merge_at_path(db_path) {
        Ok(None) => push_check(
            checks,
            "sync.merge_pending",
            CheckStatus::Ok,
            Some("No pending sync merge requires artifact reconciliation".to_string()),
            None,
        ),
        Ok(Some(state)) => {
            let status = if state.condition == PendingSyncMergeCondition::Valid {
                CheckStatus::Warn
            } else {
                CheckStatus::Error
            };
            let condition = state.condition_name();
            push_check(
                checks,
                "sync.merge_pending",
                status,
                Some(format!(
                    "Pending sync merge state is {condition}; generic repair and non-merge mutations are disabled"
                )),
                Some(serde_json::json!({
                    "pending": true,
                    "state": state,
                    "remediation": "Run `br sync --merge` to validate and resume the exact pending merge. Do not run generic repair, import, flush, or tracker mutation commands first."
                })),
            );
        }
        Err(error) => push_check(
            checks,
            "sync.merge_pending",
            CheckStatus::Error,
            Some(format!(
                "Could not prove that no sync merge is pending: {error}"
            )),
            Some(serde_json::json!({
                "pending": "unknown",
                "database_path": db_path.display().to_string(),
                "remediation": "Restore read-only access to the database family, then rerun `br doctor`. Mutating commands must remain disabled until pending merge state can be inspected."
            })),
        ),
    }
}

fn push_anomaly(anomalies: &mut Vec<AnomalyClass>, anomaly: AnomalyClass) {
    if !anomalies.contains(&anomaly) {
        anomalies.push(anomaly);
    }
}

fn check_message(check: &CheckResult) -> String {
    check.message.clone().unwrap_or_else(|| check.name.clone())
}

fn check_findings(check: &CheckResult) -> Vec<String> {
    check
        .details
        .as_ref()
        .and_then(|details| details.get("findings"))
        .and_then(serde_json::Value::as_array)
        .map(|findings| {
            findings
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(ToString::to_string)
                .collect()
        })
        .unwrap_or_else(|| check.message.iter().cloned().collect())
}

fn blocked_cache_rebuild_finding(finding: &str) -> bool {
    finding.contains(BLOCKED_CACHE_STALE_FINDING)
        || finding.contains(BLOCKED_CACHE_CONTENT_MISMATCH_FINDING)
        || finding.contains(READY_PROJECTION_CONTENT_MISMATCH_FINDING)
}

fn parse_duplicate_identifier_and_count(finding: &str, marker: &str) -> Option<(String, i64)> {
    let (_, tail) = finding.split_once(marker)?;
    let (identifier, count_tail) = tail.split_once("' (")?;
    let count_text = count_tail
        .strip_suffix(" rows)")
        .or_else(|| count_tail.strip_suffix(" row)"))?;
    let count = count_text.parse().ok()?;
    Some((identifier.to_string(), count))
}

fn append_recoverable_anomaly_findings(check: &CheckResult, anomalies: &mut Vec<AnomalyClass>) {
    for finding in check_findings(check) {
        if finding.contains("sqlite_master contains duplicate") {
            let (name, count) = parse_duplicate_identifier_and_count(&finding, " entries for '")
                .unwrap_or_else(|| ("unknown".to_string(), 2));
            push_anomaly(anomalies, AnomalyClass::DuplicateSchemaRows { name, count });
        } else if finding.contains("config contains duplicate rows") {
            let (key, count) = parse_duplicate_identifier_and_count(&finding, " rows for key '")
                .unwrap_or_else(|| ("unknown".to_string(), 2));
            push_anomaly(anomalies, AnomalyClass::DuplicateConfigKeys { key, count });
        } else if finding.contains("metadata contains duplicate rows") {
            let (key, count) = parse_duplicate_identifier_and_count(&finding, " rows for key '")
                .unwrap_or_else(|| ("unknown".to_string(), 2));
            push_anomaly(
                anomalies,
                AnomalyClass::DuplicateMetadataKeys { key, count },
            );
        } else if finding.contains(BLOCKED_CACHE_STALE_FINDING) {
            push_anomaly(anomalies, AnomalyClass::BlockedCacheStale);
        } else if finding.contains(BLOCKED_CACHE_CONTENT_MISMATCH_FINDING) {
            push_anomaly(anomalies, AnomalyClass::BlockedCacheContentMismatch);
        } else if finding.contains(READY_PROJECTION_CONTENT_MISMATCH_FINDING) {
            push_anomaly(anomalies, AnomalyClass::ReadyProjectionContentMismatch);
        }
    }
}

fn append_null_default_anomalies(check: &CheckResult, anomalies: &mut Vec<AnomalyClass>) {
    let Some(findings) = check
        .details
        .as_ref()
        .and_then(|details| details.get("findings"))
        .and_then(serde_json::Value::as_array)
    else {
        return;
    };

    for finding in findings {
        let table = finding
            .get("table")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        let column = finding
            .get("column")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("unknown");
        push_anomaly(
            anomalies,
            AnomalyClass::NullInNotNullColumn {
                table: table.to_string(),
                column: column.to_string(),
            },
        );
    }
}

fn append_count_mismatch_anomaly(check: &CheckResult, anomalies: &mut Vec<AnomalyClass>) {
    let Some(details) = check.details.as_ref() else {
        return;
    };
    let Some(db_count) = details.get("db").and_then(serde_json::Value::as_i64) else {
        return;
    };
    let Some(jsonl_count) = details.get("jsonl").and_then(serde_json::Value::as_u64) else {
        return;
    };
    let Ok(db_count) = usize::try_from(db_count) else {
        return;
    };
    let Ok(jsonl_count) = usize::try_from(jsonl_count) else {
        return;
    };

    // Only emit the cardinality-only finding when counts actually differ.
    // The check now warns on equal-count + diverging-id-set too (#286);
    // in that case we want the dedicated `DbJsonlIdSetMismatch` anomaly,
    // not the misleading `count_mismatch` one.
    if db_count != jsonl_count {
        push_anomaly(
            anomalies,
            AnomalyClass::DbJsonlCountMismatch {
                db_count,
                jsonl_count,
            },
        );
    }

    // If the check carried an `id_delta` payload, emit the set-mismatch
    // anomaly with the per-side breakdown so operators see *which* ids
    // diverged, not just that some did. The payload is the same shape
    // produced by `IdDelta::to_json`.
    if let Some(id_delta) = details.get("id_delta") {
        let only_db = id_delta
            .get("only_db")
            .and_then(serde_json::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let only_jsonl = id_delta
            .get("only_jsonl")
            .and_then(serde_json::Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let only_db_count = id_delta
            .get("only_db_count")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(only_db.len());
        let only_jsonl_count = id_delta
            .get("only_jsonl_count")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(only_jsonl.len());
        let both_count = id_delta
            .get("both_count")
            .and_then(serde_json::Value::as_u64)
            .and_then(|n| usize::try_from(n).ok())
            .unwrap_or(0);
        if only_db_count > 0 || only_jsonl_count > 0 {
            push_anomaly(
                anomalies,
                AnomalyClass::DbJsonlIdSetMismatch {
                    only_db_count,
                    only_jsonl_count,
                    only_db,
                    only_jsonl,
                    both_count,
                },
            );
        }
    }
}

fn sidecar_presence_from_check(check: &CheckResult) -> (bool, bool) {
    let findings = check
        .details
        .as_ref()
        .and_then(|details| details.get("findings"))
        .and_then(serde_json::Value::as_array);

    if let Some(findings) = findings {
        let has_wal = findings
            .iter()
            .filter_map(serde_json::Value::as_str)
            .any(|finding| finding.starts_with("WAL sidecar"));
        let has_shm = findings
            .iter()
            .filter_map(serde_json::Value::as_str)
            .any(|finding| finding.starts_with("SHM sidecar"));
        if has_wal || has_shm {
            return (has_wal, has_shm);
        }
    }

    let message = check.message.as_deref().unwrap_or_default().trim_start();
    (
        message.starts_with("WAL sidecar"),
        message.starts_with("SHM sidecar"),
    )
}

fn append_sync_metadata_anomalies(check: &CheckResult, anomalies: &mut Vec<AnomalyClass>) {
    let message = check.message.as_deref().unwrap_or_default();
    let pending_import = check
        .details
        .as_ref()
        .and_then(|details| details.get("pending_import"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or_else(|| {
            message.contains("External changes pending import")
                || message.contains("Database and JSONL have diverged")
        });
    let pending_export = check
        .details
        .as_ref()
        .and_then(|details| details.get("pending_export"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or_else(|| {
            message.contains("Local changes pending export")
                || message.contains("Local changes exist but no export is recorded")
                || message.contains("Database and JSONL have diverged")
        });
    if pending_import {
        push_anomaly(anomalies, AnomalyClass::JsonlNewer);
    }
    if pending_export {
        push_anomaly(anomalies, AnomalyClass::DbNewer);
    }
}

fn append_doctor_check_anomalies(check: &CheckResult, anomalies: &mut Vec<AnomalyClass>) {
    match check.name.as_str() {
        "db.exists" if matches!(check.status, CheckStatus::Error) => {
            push_anomaly(anomalies, AnomalyClass::DatabaseMissing);
        }
        "db.open"
        | "schema.tables"
        | "schema.columns"
        | "sqlite.integrity_check"
        | "sqlite3.integrity_check"
            if matches!(check.status, CheckStatus::Error) =>
        {
            push_anomaly(
                anomalies,
                AnomalyClass::DatabaseCorrupt {
                    detail: check_message(check),
                },
            );
        }
        "sqlite.integrity_check" | "sqlite3.integrity_check"
            if is_repairable_integrity_warning_check(check) =>
        {
            push_anomaly(
                anomalies,
                AnomalyClass::DatabaseCorrupt {
                    detail: check_message(check),
                },
            );
        }
        "jsonl.parse" if matches!(check.status, CheckStatus::Error) => {
            push_anomaly(
                anomalies,
                AnomalyClass::JsonlParseError {
                    detail: check_message(check),
                },
            );
        }
        "sync_conflict_markers" if matches!(check.status, CheckStatus::Error) => {
            push_anomaly(anomalies, AnomalyClass::JsonlConflictMarkers);
        }
        "counts.db_vs_jsonl" if matches!(check.status, CheckStatus::Warn) => {
            append_count_mismatch_anomaly(check, anomalies);
        }
        "sync.metadata" => {
            append_sync_metadata_anomalies(check, anomalies);
        }
        // `beads_rust-a53h`: the *aged* check is what identifies stale
        // artifacts. Its sibling `db.recovery_artifacts` is information-only
        // and lists every preserved backup regardless of age (see
        // `check_recovery_artifacts_aged`'s doc comment), so driving the
        // anomaly from it reported a backup written seconds earlier by the
        // running repair as "stale recovery artifacts present" — dragging
        // `workspace_health` to `degraded` and citing, as evidence of an
        // unhealthy workspace, the forensic copies the repair had just made
        // and already enumerated in `recovery_audit.verified_backups`.
        "db.recovery_artifacts.aged" if matches!(check.status, CheckStatus::Warn) => {
            push_anomaly(anomalies, AnomalyClass::StaleRecoveryArtifacts);
        }
        "db.sidecars" if matches!(check.status, CheckStatus::Error) => {
            let message = check.message.as_deref().unwrap_or_default();
            if message.contains("rollback journal") {
                push_anomaly(anomalies, AnomalyClass::JournalSidecarPresent);
            } else {
                let (has_wal, has_shm) = sidecar_presence_from_check(check);
                push_anomaly(
                    anomalies,
                    AnomalyClass::SidecarMismatch { has_wal, has_shm },
                );
            }
        }
        "db.recoverable_anomalies"
            if matches!(check.status, CheckStatus::Error | CheckStatus::Warn) =>
        {
            append_recoverable_anomaly_findings(check, anomalies);
        }
        "db.null_defaults" if matches!(check.status, CheckStatus::Warn) => {
            append_null_default_anomalies(check, anomalies);
        }
        "db.write_probe" if matches!(check.status, CheckStatus::Error) => {
            push_anomaly(
                anomalies,
                AnomalyClass::WriteProbeFailed {
                    detail: check_message(check),
                },
            );
        }
        _ => {}
    }
}

fn classify_doctor_checks(
    db_path: &Path,
    jsonl_path: &Path,
    checks: &[CheckResult],
) -> WorkspaceClassification {
    let mut anomalies = crate::health::classify_file_state(db_path, jsonl_path);
    for check in checks {
        append_doctor_check_anomalies(check, &mut anomalies);
    }
    WorkspaceClassification::from_anomalies(anomalies)
}

fn emit_doctor_reliability_audit(
    phase: &str,
    report_ok: bool,
    audit: &ReliabilityAuditRecord,
    checks: &[CheckResult],
) {
    let warning_count = checks
        .iter()
        .filter(|check| matches!(check.status, CheckStatus::Warn))
        .count();
    let error_count = checks
        .iter()
        .filter(|check| matches!(check.status, CheckStatus::Error))
        .count();
    audit.emit_tracing(phase, if report_ok { "ok" } else { "findings" });
    tracing::info!(
        target: "br::reliability",
        phase,
        ok = report_ok,
        workspace_health = %audit.health,
        anomaly_count = audit.anomaly_count,
        warning_count,
        error_count,
        "doctor check summary"
    );
}

#[cfg(test)]
fn report_has_blocked_cache_stale_finding(report: &DoctorReport) -> bool {
    report_has_blocked_cache_finding(report, |message| {
        message.contains(BLOCKED_CACHE_STALE_FINDING)
    })
}

fn report_has_blocked_cache_rebuild_finding(report: &DoctorReport) -> bool {
    report_has_blocked_cache_finding(report, blocked_cache_rebuild_finding)
}

fn report_has_projection_content_mismatch_finding(report: &DoctorReport) -> bool {
    report_has_blocked_cache_finding(report, |message| {
        message.contains(BLOCKED_CACHE_CONTENT_MISMATCH_FINDING)
            || message.contains(READY_PROJECTION_CONTENT_MISMATCH_FINDING)
    })
}

fn report_has_blocked_cache_finding(
    report: &DoctorReport,
    predicate: impl Fn(&str) -> bool + Copy,
) -> bool {
    report.checks.iter().any(|check| {
        if check.name != "db.recoverable_anomalies" {
            return false;
        }

        if check.message.as_deref().is_some_and(predicate) {
            return true;
        }

        check
            .details
            .as_ref()
            .and_then(|details| details.get("findings"))
            .and_then(serde_json::Value::as_array)
            .is_some_and(|findings| {
                findings
                    .iter()
                    .any(|finding| finding.as_str().is_some_and(predicate))
            })
    })
}

fn report_has_sidecar_anomaly(report: &DoctorReport) -> bool {
    report
        .checks
        .iter()
        .any(|check| check.name == "db.sidecars" && matches!(check.status, CheckStatus::Error))
}

fn filter_allows_recoverable_db_state_repair(
    filter: &FixerFilter,
    has_blocked_cache_rebuild: bool,
    has_sidecar_anomaly: bool,
) -> bool {
    (has_blocked_cache_rebuild && filter.allows(FM_BLOCKED_CACHE_STALE))
        || (has_sidecar_anomaly && filter.allows(FM_WAL_SHM_SIDECAR_ORPHAN))
}

fn filter_allows_jsonl_rebuild(filter: &FixerFilter) -> bool {
    [
        FM_JSONL_ROW_COUNT_MISMATCH,
        FM_EMPTY_OR_TRUNCATED_DATABASE,
        FM_SQLITE_PAGE_MALFORMED,
        FM_MISSING_REQUIRED_TABLE,
        FM_MISSING_REQUIRED_COLUMN,
        FM_BLOCKED_CACHE_STALE,
    ]
    .iter()
    .any(|fm| filter.allows(fm))
}

/// Return true if any integrity check reported non-benign page corruption
/// (e.g., "free space corruption") that VACUUM can fix by rewriting all pages.
fn report_has_page_corruption(report: &DoctorReport) -> bool {
    report.checks.iter().any(|check| {
        if !matches!(check.status, CheckStatus::Error) {
            return false;
        }
        if check.name != "sqlite.integrity_check" && check.name != "sqlite3.integrity_check" {
            return false;
        }
        check.message.as_deref().is_some_and(|msg| {
            let lower = msg.to_lowercase();
            // Match structural page corruption that VACUUM can fix.
            // Note: "out of order" for DESC indexes is a known frankensqlite
            // B-tree artifact (not fixable by VACUUM) and is handled as benign
            // in integrity_messages_only_benign instead.
            lower.contains("free space corruption")
                || lower.contains("malformed")
                || lower.contains("disk image")
        })
    })
}

/// Compact the database to fix page-level anomalies (free space corruption,
/// orphaned pages, B-tree malformation) that arise from frankensqlite's B-tree
/// layer.
///
/// Run in-place VACUUM first, then try to install a compacted copy via VACUUM
/// INTO. If upstream sqlite3 still reports `Page N: never used` afterward,
/// the caller escalates to a JSONL rebuild.
fn acquire_doctor_database_write_authority(
    beads_dir: &Path,
    db_path: &Path,
    lock_timeout: Option<u64>,
) -> Result<Arc<crate::sync::DatabaseFamilyWriteLock>> {
    let write_authority = Arc::new(
        crate::sync::blocking_database_family_write_lock_with_timeout(
            beads_dir,
            db_path,
            lock_timeout,
        )?,
    );
    write_authority.bind_database_inode_for_mutation()?;
    write_authority.verify_database_authority()?;
    Ok(write_authority)
}

fn open_doctor_storage_under_write_authority(
    db_path: &Path,
    write_authority: &Arc<crate::sync::DatabaseFamilyWriteLock>,
) -> Result<SqliteStorage> {
    write_authority.verify_database_authority()?;
    let mut storage = SqliteStorage::open(db_path)?;
    write_authority.verify_database_authority()?;
    storage.attach_write_authority(Arc::clone(write_authority));
    Ok(storage)
}

fn repair_via_vacuum(
    db_path: &Path,
    repair: &mut LocalRepairResult,
    session: Option<&mut DoctorRepairSession>,
    write_authority: &Arc<crate::sync::DatabaseFamilyWriteLock>,
) {
    if !db_path.is_file() {
        tracing::debug!(
            path = %db_path.display(),
            "Skipping VACUUM because the database file is missing"
        );
        return;
    }
    let do_vacuum = |repair: &mut LocalRepairResult| {
        if let Err(err) = write_authority.verify_database_authority() {
            tracing::warn!(
                path = %db_path.display(),
                error = %err,
                "Skipping VACUUM because database-family authority was lost"
            );
            return;
        }
        match open_doctor_storage_under_write_authority(db_path, write_authority) {
            Ok(storage) => {
                if let Err(err) = storage.execute_raw("VACUUM") {
                    tracing::warn!(path = %db_path.display(), error = %err, "VACUUM failed");
                    return;
                }

                repair.vacuumed = true;
                match config::compact_database_via_vacuum_into_in_place(storage, db_path, None) {
                    Ok(_storage) => {
                        tracing::info!(
                            path = %db_path.display(),
                            "VACUUM plus VACUUM INTO compaction completed successfully"
                        );
                    }
                    Err(err) => {
                        tracing::warn!(
                            path = %db_path.display(),
                            error = %err,
                            "VACUUM INTO compaction failed after VACUUM"
                        );
                    }
                }
            }
            Err(err) => {
                tracing::warn!(
                    path = %db_path.display(),
                    error = %err,
                    "Skipping VACUUM because the database could not be opened"
                );
            }
        }
    };
    if let Some(session) = session {
        let family_paths = existing_sqlite_family_paths_for_legacy_op(db_path);
        let family_refs: Vec<&Path> = family_paths.iter().map(PathBuf::as_path).collect();
        let result = session.record_legacy_mutation("repair_via_vacuum", &family_refs, || {
            do_vacuum(repair);
            Ok(())
        });
        if let Err(err) = result {
            tracing::warn!(
                path = %db_path.display(),
                error = %err,
                "Failed to record VACUUM legacy-op audit; mutation still proceeded if possible"
            );
        }
    } else {
        do_vacuum(repair);
    }
}

/// Write probe: verify the database can actually perform writes after repair.
///
/// Issue #245: REINDEX can fix index ordering so `PRAGMA integrity_check`
/// passes, but the underlying B-tree corruption may silently cause writes to
/// fail (reads work, writes get ISSUE_NOT_FOUND).  This probe inserts a row,
/// reads it back, then ROLLS BACK the entire transaction so no data is
/// persisted.  The insert-then-read pattern catches read-after-write
/// divergence that a simple no-op UPDATE would miss.
fn write_probe_after_repair(
    db_path: &Path,
    write_authority: &Arc<crate::sync::DatabaseFamilyWriteLock>,
) -> bool {
    if write_authority.verify_database_authority().is_err() {
        return false;
    }
    let Ok(conn) = Connection::open(db_path.to_string_lossy().into_owned()) else {
        return false;
    };
    let _ = conn.execute("PRAGMA busy_timeout=5000");

    // Use a probe ID that cannot collide with real issues.
    let probe_id = "__doctor_write_probe__";
    let now = chrono::Utc::now().to_rfc3339();

    let probe = (|| -> std::result::Result<(), Box<dyn std::error::Error>> {
        write_authority.verify_database_authority()?;
        conn.execute("BEGIN IMMEDIATE")?;

        conn.execute_with_params(
            "INSERT OR REPLACE INTO issues (id, title, status, priority, created_at, updated_at) \
             VALUES (?, ?, 'open', 2, ?, ?)",
            &[
                SqliteValue::from(probe_id),
                SqliteValue::from("doctor write probe"),
                SqliteValue::from(now.as_str()),
                SqliteValue::from(now.as_str()),
            ],
        )?;

        // Read it back inside the same transaction to verify the read path
        // agrees with what we just wrote. CTE-wrap per #254 so the probe
        // itself does not hit fsqlite's prepared-statement fast-path cache
        // and report false-healthy against a stale plan.
        let rows = conn.query_with_params(
            "WITH target(id_value) AS (SELECT ?) \
             SELECT i.id FROM issues AS i, target AS t \
             WHERE i.id = t.id_value",
            &[SqliteValue::from(probe_id)],
        )?;
        if rows.is_empty() {
            conn.execute("ROLLBACK")?;
            write_authority.verify_database_authority()?;
            tracing::warn!("Write probe: INSERT succeeded but SELECT returned no rows");
            return Err("read-after-write divergence".into());
        }

        // Always ROLLBACK — the probe is non-destructive.  No data is
        // persisted, so JSONL export state stays clean.
        conn.execute("ROLLBACK")?;
        write_authority.verify_database_authority()?;
        Ok(())
    })();

    let probe_ok = match probe {
        Ok(()) => {
            tracing::info!("Post-repair write probe passed");
            true
        }
        Err(err) => {
            tracing::warn!(error = %err, "Post-repair write probe failed — DB may still be corrupt");
            // Best-effort rollback in case we're stuck mid-transaction.
            let _ = conn.execute("ROLLBACK");
            false
        }
    };

    if let Err(err) = conn.close() {
        tracing::warn!(error = %err, "Post-repair write probe connection close failed");
        return false;
    }

    probe_ok && write_authority.verify_database_authority().is_ok()
}

/// Return true if any integrity check reported WARN-level page anomalies
/// (e.g. "page N: never used", "free space corruption", "malformed"): the
/// kind of residue VACUUM can clean up.
///
/// Intentionally distinct from [`report_has_page_corruption`] (ERROR-only),
/// because orphaned pages introduced/exposed by a light-repair pass (notably
/// the blocked-cache rebuild from `repair_recoverable_db_state`) land as
/// WARN-level findings — they don't flip the DB into `ok: false` on their
/// own, but they persist across subsequent `--repair` runs unless we
/// compact the file.
///
/// See #253 for the original report and the exact sequence that leaves the
/// DB in this state.
fn is_warn_level_page_anomaly_check(check: &CheckResult) -> bool {
    if !matches!(check.status, CheckStatus::Warn) {
        return false;
    }
    if check.name != "sqlite.integrity_check" && check.name != "sqlite3.integrity_check" {
        return false;
    }
    check.message.as_deref().is_some_and(|msg| {
        let lower = msg.to_lowercase();
        lower.contains("never used")
            || lower.contains("free space corruption")
            || lower.contains("malformed")
            || lower.contains("disk image")
    })
}

fn report_has_warn_level_page_anomaly(report: &DoctorReport) -> bool {
    report.checks.iter().any(is_warn_level_page_anomaly_check)
}

fn repair_report_verified(report: &DoctorReport) -> bool {
    report.ok && !has_non_ok(&report.checks)
}

/// True when a `sync.metadata` WARN describes *only* the pending-export
/// direction — the database is ahead of the JSONL, and nothing is waiting to be
/// imported.
///
/// This is the benign half of the drift axis: the DB is a superset of the
/// export, no data is at risk, and `sync --flush-only` (which the check's own
/// message recommends) resolves it. The pending-*import* directions
/// (`External changes pending import`, `Database and JSONL have diverged`) are
/// deliberately excluded — after a rebuild *from* JSONL, a JSONL that still
/// reads newer than the database means the rebuild did not land what it read.
///
/// Reads the structured `pending_export`/`pending_import` details rather than
/// matching on message text. `check_sync_metadata` always populates both; if
/// either is absent this returns false, so a hand-built or future report shape
/// fails closed rather than silently verifying.
fn is_pending_export_only_sync_metadata(check: &CheckResult) -> bool {
    if check.name != "sync.metadata" || !matches!(check.status, CheckStatus::Warn) {
        return false;
    }
    let detail_flag = |key: &str| {
        check
            .details
            .as_ref()
            .and_then(|details| details.get(key))
            .and_then(serde_json::Value::as_bool)
    };
    match (detail_flag("pending_export"), detail_flag("pending_import")) {
        (Some(pending_export), Some(pending_import)) => pending_export && !pending_import,
        _ => false,
    }
}

/// Findings that are EXPECTED to remain after a *successful* JSONL rebuild and
/// therefore must not, on their own, fail post-repair verification (issue #375).
///
/// A rebuild-from-JSONL structurally cannot avoid producing these signals:
/// - `db.recovery_artifacts` (WARN): the rebuild deliberately moves the entire
///   pre-repair database family — the corrupt `beads.db` *and* any stale
///   `-wal`/`-shm` sidecars — into `.br_recovery/` as a forensic backup before
///   writing the fresh DB. A fresh, non-aged backup is the success signal, not
///   a defect; the separate `db.recovery_artifacts.aged` check still fails on
///   backups past the TTL.
/// - `rust_log` (WARN): a nag about the *caller's* `RUST_LOG` environment,
///   unrelated to database health.
/// - `sync.metadata` (WARN), pending-export direction only (`beads_rust-a53h`):
///   the rebuild reconstructs the database from JSONL and then restores any
///   unflushed tombstones that the JSONL does not carry, so the fresh database
///   is legitimately ahead of the export with no `last_export_time` recorded.
///   That is the repair working as designed — preserving local state it was
///   asked not to lose — and it resolves with `sync --flush-only`. Treating it
///   as a verification failure made `doctor --repair` exit 7 after succeeding,
///   so no "repair until healthy" loop could ever converge, and every pass
///   wrote three more backups plus a `.verification-failed.json` marker.
///
/// Anything ERROR-level (corrupt rebuilt DB, schema/integrity/count failures)
/// still flips `report.ok` to false and fails verification, so this only
/// relaxes the "must be pristine" constraint for these known-benign warnings.
/// Record loss specifically remains covered elsewhere: `counts.db_vs_jsonl`
/// reports divergence, and `verify_rebuilt_database_postconditions` fails the
/// rebuild outright on a count mismatch or orphaned reference.
fn is_benign_post_rebuild_finding(check: &CheckResult) -> bool {
    if !matches!(check.status, CheckStatus::Warn) {
        return false;
    }
    match check.name.as_str() {
        // `br_path_dupes` reports the host's $PATH shape (two `br` installs).
        // A database rebuild cannot change $PATH, so treating the warning as
        // a verification failure made `--repair` exit 7 on dual-install
        // hosts even when the rebuilt database was fully healthy
        // (beads_rust-ozdh). The warning still surfaces in the report.
        "db.recovery_artifacts" | "rust_log" | "br_path_dupes" => true,
        "sync.metadata" => is_pending_export_only_sync_metadata(check),
        _ => false,
    }
}

/// Post-repair verification for the JSONL-rebuild path.
///
/// Unlike [`repair_report_verified`] (which demands a pristine, zero-non-OK
/// report), this tolerates the handful of benign findings a successful rebuild
/// always leaves behind — see [`is_benign_post_rebuild_finding`]. Without this,
/// the rebuild path could never verify: it always creates a recovery backup, so
/// `repair_report_verified` returned false even when the rebuilt database was
/// fully healthy, forcing `doctor --repair` to exit 7 on the `corrupt_db_text`
/// fixture (issue #375).
///
/// `preserved_dirty_issue_ids` are the dirty unflushed issues this repair
/// run restored across the rebuild (GitHub #394). Those rows intentionally
/// leave the DB ahead of the JSONL until the next flush, so the
/// `counts.db_vs_jsonl` divergence they cause is accepted — but only in
/// exactly that shape (see [`is_preserved_dirty_count_divergence`]).
fn jsonl_rebuild_repair_verified(
    report: &DoctorReport,
    preserved_dirty_issue_ids: &[String],
) -> bool {
    report.ok
        && report.checks.iter().all(|check| {
            matches!(check.status, CheckStatus::Ok)
                || is_benign_post_rebuild_finding(check)
                || is_preserved_dirty_count_divergence(check, preserved_dirty_issue_ids)
        })
}

/// GitHub #394: a rebuild that preserved dirty unflushed issues leaves the
/// DB intentionally ahead of the JSONL until the next `sync --flush-only`,
/// so the post-repair `counts.db_vs_jsonl` warning is expected. Accept it
/// only in exactly that shape:
///
/// - nothing is missing from the DB (`only_jsonl_count == 0` — a row
///   missing from the DB is record loss, never benign), and
/// - the id delta's DB-only preview is complete (not clipped by the
///   preview limit), and
/// - every DB-only id is one this repair run just restored.
///
/// Any other divergence — extra unexplained DB rows, missing rows, a
/// clipped preview we cannot fully attribute — still fails verification.
fn is_preserved_dirty_count_divergence(
    check: &CheckResult,
    preserved_dirty_issue_ids: &[String],
) -> bool {
    if preserved_dirty_issue_ids.is_empty()
        || check.name != "counts.db_vs_jsonl"
        || !matches!(check.status, CheckStatus::Warn)
    {
        return false;
    }
    let Some(delta) = check
        .details
        .as_ref()
        .and_then(|details| details.get("id_delta"))
    else {
        return false;
    };
    if delta
        .get("only_jsonl_count")
        .and_then(serde_json::Value::as_u64)
        != Some(0)
    {
        return false;
    }
    let Some(only_db_count) = delta
        .get("only_db_count")
        .and_then(serde_json::Value::as_u64)
        .and_then(|count| usize::try_from(count).ok())
    else {
        return false;
    };
    let Some(only_db) = delta.get("only_db").and_then(serde_json::Value::as_array) else {
        return false;
    };
    if only_db.len() != only_db_count {
        // Preview clipped: we cannot attribute every divergent row.
        return false;
    }
    only_db.iter().all(|id| {
        id.as_str()
            .is_some_and(|id| preserved_dirty_issue_ids.iter().any(|kept| kept == id))
    })
}

/// Return true if any integrity check reported partial-index row mismatches
/// ("row N missing from index") as a warning.  These can be repaired via `REINDEX`.
fn is_partial_index_warning_check(check: &CheckResult) -> bool {
    if !matches!(check.status, CheckStatus::Warn) {
        return false;
    }
    if check.name != "sqlite.integrity_check" && check.name != "sqlite3.integrity_check" {
        return false;
    }
    check
        .message
        .as_deref()
        .is_some_and(|msg| msg.to_lowercase().contains("missing from index"))
}

fn report_has_partial_index_warnings(report: &DoctorReport) -> bool {
    report.checks.iter().any(is_partial_index_warning_check)
}

fn is_repairable_integrity_warning_check(check: &CheckResult) -> bool {
    is_warn_level_page_anomaly_check(check) || is_partial_index_warning_check(check)
}

fn warning_repair_verified(
    report: &DoctorReport,
    repaired_blocked_cache: bool,
    repaired_partial_index_warnings: bool,
) -> bool {
    report.ok
        && (!repaired_blocked_cache || !report_has_blocked_cache_rebuild_finding(report))
        && (!repaired_partial_index_warnings || !report_has_partial_index_warnings(report))
        && !report_has_warn_level_page_anomaly(report)
}

fn local_repair_message(local_repair: &LocalRepairResult) -> String {
    let mut actions = Vec::new();
    if local_repair.blocked_cache_rebuilt {
        actions.push("rebuilt the blocked cache".to_string());
    }
    if local_repair.indexes_reindexed {
        actions.push("rebuilt all indexes via REINDEX".to_string());
    }
    if local_repair.vacuumed {
        actions.push("compacted database via VACUUM to fix page-level anomalies".to_string());
    }
    if !local_repair.quarantined_artifacts.is_empty() {
        actions.push(format!(
            "quarantined {} anomalous database artifact(s)",
            local_repair.quarantined_artifacts.len()
        ));
    }

    if actions.is_empty() {
        "No remaining errors detected after recoverable-state repair.".to_string()
    } else {
        format!("Repair complete: {}.", actions.join("; "))
    }
}

fn is_offending_root_gitignore_pattern(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty()
        && !trimmed.starts_with('#')
        && !trimmed.starts_with('!')
        && ROOT_GITIGNORE_OFFENDING_PATTERNS.contains(&trimmed)
}

fn repair_outcome_message_from_parts(
    mut messages: Vec<String>,
    local_repair: Option<&LocalRepairResult>,
    incomplete_attempt_message: Option<&str>,
) -> String {
    if let Some(repair) = local_repair {
        if repair.applied() {
            messages.push(local_repair_message(repair));
        } else if let Some(message) = incomplete_attempt_message {
            messages.push(message.to_string());
        }
    }

    if messages.is_empty() {
        NO_OP_REPAIR_MESSAGE.to_string()
    } else {
        messages.join(" ")
    }
}

#[derive(Debug, Clone, Copy)]
struct EarlyRepairSummary {
    gitignore: bool,
    merge_artifacts: bool,
    startup_cache: bool,
    recovery_aged: bool,
    export_hash: bool,
    base_jsonl_symlink: bool,
    base_jsonl_stale: bool,
    orphan_tmp: bool,
    jsonl_eof_newline: bool,
    jsonl_bom: bool,
    jsonl_crlf: bool,
    jsonl_world_writable: bool,
    config_yaml_secret_mode: bool,
    inner_gitignore: bool,
    dirty_bitmap_orphans: bool,
    comments_orphans: bool,
    labels_orphans: bool,
    dependencies_orphans: bool,
    wal_checkpoint: bool,
    null_defaults: bool,
    db_bloat_vacuum: bool,
}

impl EarlyRepairSummary {
    fn applied(self) -> bool {
        self.gitignore
            || self.merge_artifacts
            || self.startup_cache
            || self.recovery_aged
            || self.export_hash
            || self.base_jsonl_symlink
            || self.base_jsonl_stale
            || self.orphan_tmp
            || self.jsonl_eof_newline
            || self.jsonl_bom
            || self.jsonl_crlf
            || self.jsonl_world_writable
            || self.config_yaml_secret_mode
            || self.inner_gitignore
            || self.dirty_bitmap_orphans
            || self.comments_orphans
            || self.labels_orphans
            || self.dependencies_orphans
            || self.wal_checkpoint
            || self.null_defaults
            || self.db_bloat_vacuum
    }

    fn action_labels(self) -> Vec<String> {
        let mut actions = Vec::new();
        if self.gitignore {
            actions.push("gitignore_repaired".to_string());
        }
        if self.merge_artifacts {
            actions.push("merge_artifacts_quarantined".to_string());
        }
        if self.startup_cache {
            actions.push("startup_cache_quarantined".to_string());
        }
        if self.recovery_aged {
            actions.push("recovery_artifacts_aged_quarantined".to_string());
        }
        if self.export_hash {
            actions.push("export_hash_cache_recomputed".to_string());
        }
        if self.base_jsonl_symlink {
            actions.push("base_jsonl_symlink_quarantined".to_string());
        }
        if self.base_jsonl_stale {
            actions.push("base_jsonl_anchor_regenerated".to_string());
        }
        if self.orphan_tmp {
            actions.push("orphan_tmp_quarantined".to_string());
        }
        if self.jsonl_eof_newline {
            actions.push("jsonl_trailing_newline_appended".to_string());
        }
        if self.jsonl_bom {
            actions.push("jsonl_bom_stripped".to_string());
        }
        if self.jsonl_crlf {
            actions.push("jsonl_crlf_converted".to_string());
        }
        if self.jsonl_world_writable {
            actions.push("jsonl_world_write_stripped".to_string());
        }
        if self.config_yaml_secret_mode {
            actions.push("config_yaml_secret_mode_chmod".to_string());
        }
        if self.inner_gitignore {
            actions.push("inner_gitignore_appended".to_string());
        }
        if self.dirty_bitmap_orphans {
            actions.push("dirty_bitmap_orphans_pruned".to_string());
        }
        if self.comments_orphans {
            actions.push("comments_orphans_pruned".to_string());
        }
        if self.labels_orphans {
            actions.push("labels_orphans_pruned".to_string());
        }
        if self.dependencies_orphans {
            actions.push("dependencies_orphans_pruned".to_string());
        }
        if self.wal_checkpoint {
            actions.push("wal_checkpoint_truncated".to_string());
        }
        if self.null_defaults {
            actions.push("null_defaults_backfilled".to_string());
        }
        if self.db_bloat_vacuum {
            actions.push("db_bloat_vacuumed".to_string());
        }
        actions
    }

    fn messages(self) -> Vec<String> {
        let mut messages = Vec::new();
        if self.gitignore {
            messages.push(ROOT_GITIGNORE_REPAIR_MESSAGE.to_string());
        }
        if self.merge_artifacts {
            messages.push("Quarantined stuck merge artifacts.".to_string());
        }
        if self.startup_cache {
            messages.push("Quarantined poisoned startup-cache files.".to_string());
        }
        if self.recovery_aged {
            messages.push("Quarantined aged recovery artifacts.".to_string());
        }
        if self.export_hash {
            messages.push("Recomputed metadata.jsonl_content_hash.".to_string());
        }
        if self.base_jsonl_symlink {
            messages.push("Quarantined symlinked merge anchor.".to_string());
        }
        if self.base_jsonl_stale {
            messages.push("Regenerated stale merge anchor from current JSONL.".to_string());
        }
        if self.orphan_tmp {
            messages.push("Quarantined orphan tmp files.".to_string());
        }
        if self.jsonl_eof_newline {
            messages.push(
                "Appended missing trailing newline to the selected JSONL export.".to_string(),
            );
        }
        if self.jsonl_bom {
            messages.push("Stripped UTF-8 BOM from the selected JSONL export.".to_string());
        }
        if self.jsonl_crlf {
            messages.push(
                "Converted CRLF line endings to LF in the selected JSONL export.".to_string(),
            );
        }
        if self.jsonl_world_writable {
            messages.push("Stripped world-write bit from the selected JSONL export.".to_string());
        }
        if self.config_yaml_secret_mode {
            messages.push(
                "Stripped world-read/write bits from `.beads/config.yaml` (contains secrets)."
                    .to_string(),
            );
        }
        if self.inner_gitignore {
            messages.push("Appended canonical patterns to `.beads/.gitignore`.".to_string());
        }
        if self.dirty_bitmap_orphans {
            messages.push("Pruned orphan rows from dirty_issues table.".to_string());
        }
        if self.comments_orphans {
            messages.push("Pruned orphan rows from comments table.".to_string());
        }
        if self.labels_orphans {
            messages.push("Pruned orphan rows from labels table.".to_string());
        }
        if self.dependencies_orphans {
            messages.push("Pruned orphan rows from dependencies table.".to_string());
        }
        if self.wal_checkpoint {
            messages.push("Truncated the SQLite WAL via PRAGMA wal_checkpoint.".to_string());
        }
        if self.null_defaults {
            messages.push(
                "Backfilled schema-declared defaults into NULL NOT-NULL columns.".to_string(),
            );
        }
        if self.db_bloat_vacuum {
            messages.push(
                "Compacted database via VACUUM to reclaim freelist space (--unsafe-auto-fix opt-in)."
                    .to_string(),
            );
        }
        messages
    }

    fn audit_record(self) -> RecoveryAuditRecord {
        let applied_actions = self.action_labels();
        let outcome = match applied_actions.as_slice() {
            [] => "nothing_to_repair".to_string(),
            [action] => action.clone(),
            _ => "repairs_applied".to_string(),
        };
        let phase = if applied_actions.is_empty() {
            "doctor.noop"
        } else {
            "doctor.early_repair"
        };
        RecoveryAuditRecord {
            phase: phase.to_string(),
            action: "repair".to_string(),
            outcome,
            reason: None,
            applied_actions,
            quarantined_artifacts: Vec::new(),
            verified_backups: Vec::new(),
            imported: None,
            skipped: None,
            fk_violations_cleaned: None,
        }
    }

    fn prepend_actions_to_audit(self, mut record: RecoveryAuditRecord) -> RecoveryAuditRecord {
        let mut early_actions = self.action_labels();
        if early_actions.is_empty() {
            return record;
        }
        early_actions.append(&mut record.applied_actions);
        record.applied_actions = early_actions;
        record
    }
}

fn classify_path_kind(path: &Path) -> Result<FilesystemPathKind> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Ok(FilesystemPathKind::Symlink),
        Ok(metadata) if metadata.is_file() => Ok(FilesystemPathKind::File),
        Ok(metadata) if metadata.is_dir() => Ok(FilesystemPathKind::Directory),
        Ok(_) => Ok(FilesystemPathKind::Other),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(FilesystemPathKind::Missing),
        Err(err) => Err(err.into()),
    }
}

fn database_sidecar_paths(db_path: &Path) -> [(PathBuf, &'static str); 3] {
    let db_string = db_path.to_string_lossy();
    [
        (PathBuf::from(format!("{db_string}-wal")), "WAL"),
        (PathBuf::from(format!("{db_string}-shm")), "SHM"),
        (
            PathBuf::from(format!("{db_string}-journal")),
            "rollback journal",
        ),
    ]
}

fn inspect_database_sidecars(db_path: &Path) -> Result<SidecarInspection> {
    let db_kind = classify_path_kind(db_path)?;
    let mut inspection = SidecarInspection::default();
    let mut wal_kind = FilesystemPathKind::Missing;
    let mut shm_kind = FilesystemPathKind::Missing;

    for (path, label) in database_sidecar_paths(db_path) {
        let kind = classify_path_kind(&path)?;
        match label {
            "WAL" => wal_kind = kind,
            "SHM" => shm_kind = kind,
            _ => {}
        }

        if kind.exists() && !db_kind.is_regular_file() {
            inspection.quarantine_candidates.push(path.clone());
        }

        if kind.exists() && !kind.is_regular_file() {
            inspection.findings.push(format!(
                "{label} sidecar at {} is a {} instead of a regular file",
                path.display(),
                kind.description()
            ));
            inspection.quarantine_candidates.push(path);
        }
    }

    if wal_kind.is_regular_file() && !shm_kind.exists() {
        // FrankenSQLite keeps its WAL index in process-local memory, so a WAL
        // with no `-shm` sidecar is the normal steady state, not a fault.
        // Record the layout as informational; do not quarantine a valid WAL or
        // degrade an otherwise healthy workspace. The integrity and
        // rollback-only write probes below remain the authoritative gates.
        inspection.informational_findings.push(format!(
            "WAL sidecar exists without a matching SHM sidecar at {} (expected for frankensqlite)",
            PathBuf::from(format!("{}-wal", db_path.to_string_lossy())).display()
        ));
    }

    if shm_kind.is_regular_file() && !wal_kind.exists() {
        let shm_path = PathBuf::from(format!("{}-shm", db_path.to_string_lossy()));
        inspection.findings.push(format!(
            "SHM sidecar exists without a matching WAL sidecar at {}",
            shm_path.display()
        ));
        inspection.quarantine_candidates.push(shm_path);
    }

    if shm_kind.is_regular_file() && wal_kind.is_regular_file() {
        // frankensqlite never reads or writes the classic `-shm` index — the
        // WAL index lives in process-local memory — so an SHM beside a live
        // WAL is inert heritage (e.g. an orphan the engine's own open later
        // paired with a fresh WAL). Classify it explicitly instead of
        // silently absorbing it into a "full family" that this engine does
        // not actually use; the file stays in place because it is harmless.
        inspection.informational_findings.push(format!(
            "SHM sidecar at {} is inert beside the WAL (a WAL-only family is expected for frankensqlite; the WAL index lives in process memory)",
            PathBuf::from(format!("{}-shm", db_path.to_string_lossy())).display()
        ));
    }

    if !db_kind.is_regular_file() {
        let has_dangling_sidecars = database_sidecar_paths(db_path)
            .into_iter()
            .any(|(path, _)| classify_path_kind(&path).is_ok_and(FilesystemPathKind::exists));
        if has_dangling_sidecars {
            inspection.findings.push(format!(
                "Database sidecars exist even though the primary database at {} is a {}",
                db_path.display(),
                db_kind.description()
            ));
        }
    }

    inspection.quarantine_candidates.sort();
    inspection.quarantine_candidates.dedup();
    Ok(inspection)
}

fn check_database_sidecars(db_path: &Path, checks: &mut Vec<CheckResult>) -> Result<()> {
    let inspection = inspect_database_sidecars(db_path)?;

    if !inspection.findings.is_empty() {
        push_check(
            checks,
            "db.sidecars",
            CheckStatus::Error,
            Some(inspection.findings[0].clone()),
            Some(serde_json::json!({
                "findings": inspection.findings,
                "quarantine_candidates": inspection
                    .quarantine_candidates
                    .iter()
                    .map(|path| path.display().to_string())
                    .collect::<Vec<_>>(),
            })),
        );
        return Ok(());
    }

    if !inspection.informational_findings.is_empty() {
        push_check(
            checks,
            "db.sidecars",
            CheckStatus::Ok,
            Some(inspection.informational_findings[0].clone()),
            Some(serde_json::json!({
                "findings": inspection.informational_findings,
            })),
        );
        return Ok(());
    }

    push_check(checks, "db.sidecars", CheckStatus::Ok, None, None);
    Ok(())
}

#[cfg(any(unix, windows))]
fn namespace_database_identity(db_path: &Path) -> Result<fsqlite_vfs::FileIdentity> {
    if !fs::symlink_metadata(db_path)?.is_file() {
        return Err(BeadsError::Config(
            "Database pathname is not a regular, non-symlink file".to_string(),
        ));
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(db_path)?;
    if !file.metadata()?.is_file() || !fs::symlink_metadata(db_path)?.is_file() {
        return Err(BeadsError::Config(
            "Database pathname changed during identity inspection".to_string(),
        ));
    }
    fsqlite_vfs::FileIdentity::from_file(&file)?.ok_or_else(|| {
        BeadsError::Config("Database descriptor identity is unavailable".to_string())
    })
}

/// Compare the live main file with the engine's complete namespace ledger.
/// ReadOnlyExisting never creates sidecars or publishes a new identity. The
/// engine holds shared gate/use locks while these descriptor identities are
/// compared; a pathname replacement observed during the probe is inconclusive.
#[cfg(any(unix, windows))]
struct NamespaceIdentityObservation {
    recorded: fsqlite_vfs::FileIdentity,
    current: fsqlite_vfs::FileIdentity,
}

#[cfg(any(unix, windows))]
fn inspect_namespace_identity(db_path: &Path) -> Result<Option<NamespaceIdentityObservation>> {
    use fsqlite_vfs::{NamespaceOpenIntent, PendingNamespaceOpen};

    let paths = fsqlite_namespace_sidecar_paths(db_path);
    let mut present = 0;
    for path in &paths {
        match fs::symlink_metadata(path) {
            Ok(_) => present += 1,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    if present == 0 {
        return Ok(None);
    }
    if present != paths.len() {
        return Err(BeadsError::Config(
            "Namespace sidecar pair is incomplete; identity cannot be established".to_string(),
        ));
    }
    let admission = PendingNamespaceOpen::begin(db_path, NamespaceOpenIntent::ReadOnlyExisting)?;
    let expected = admission.expected_identity().ok_or_else(|| {
        BeadsError::Config("Namespace record has no established identity".to_string())
    })?;
    let current = namespace_database_identity(db_path)?;
    if namespace_database_identity(db_path)? != current {
        return Err(BeadsError::Config(
            "Database pathname changed during identity inspection; retry after writers stop"
                .to_string(),
        ));
    }
    Ok(Some(NamespaceIdentityObservation {
        recorded: expected,
        current,
    }))
}

/// Inspect before any engine open: even a read-only engine open can rebind a
/// quiescent namespace record in FrankenSQLite 0.3.16. A false result forbids
/// live probes so the evidence this check reports remains intact.
fn check_namespace_identity(db_path: &Path, checks: &mut Vec<CheckResult>) -> bool {
    const NAME: &str = "db.namespace_identity";
    #[cfg(any(unix, windows))]
    {
        let sidecars = fsqlite_namespace_sidecar_paths(db_path);
        match inspect_namespace_identity(db_path) {
            Ok(Some(identity)) => {
                let matched = identity.recorded == identity.current;
                push_check(
                    checks,
                    NAME,
                    if matched { CheckStatus::Ok } else { CheckStatus::Error },
                    (!matched).then(|| "Database identity differs from its persistent namespace sidecars (foreign sidecar family). Preserve the main database and every sidecar, including WAL and journal files: they may contain committed data for the displaced database. Stop all writers, capture `br doctor --bundle <path> --include-db`, and validate the authoritative family before operator-directed recovery. No automatic repair is offered.".to_string()),
                    Some(serde_json::json!({
                        "state": if matched { "matched" } else { "mismatch" },
                        "database": db_path, "sidecars": sidecars,
                        "identities_match": matched,
                        "automatic_repair": false,
                    })),
                );
                matched
            }
            Ok(None) => {
                push_check(
                    checks,
                    NAME,
                    CheckStatus::Ok,
                    Some(
                        "No persistent namespace sidecars; identity comparison is not applicable"
                            .to_string(),
                    ),
                    Some(serde_json::json!({ "state": "not_applicable", "database": db_path })),
                );
                true
            }
            Err(error) => {
                push_check(
                    checks,
                    NAME,
                    CheckStatus::Warn,
                    Some(format!(
                        "Namespace identity evidence is unavailable: {error}. This does not establish a foreign sidecar family; preserve all database-family files."
                    )),
                    Some(serde_json::json!({
                        "state": "unavailable", "database": db_path, "sidecars": sidecars,
                        "reason": error.to_string(), "automatic_repair": false,
                    })),
                );
                false
            }
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        push_check(
            checks,
            NAME,
            CheckStatus::Warn,
            Some("Namespace identity inspection is unavailable on this platform".to_string()),
            Some(serde_json::json!({ "state": "unavailable", "database": db_path })),
        );
        false
    }
}

fn check_recovery_artifacts(
    beads_dir: &Path,
    db_path: &Path,
    checks: &mut Vec<CheckResult>,
) -> Result<()> {
    let artifacts = recovery_artifacts_for_db_family(beads_dir, db_path)?
        .into_iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>();

    if artifacts.is_empty() {
        push_check(checks, "db.recovery_artifacts", CheckStatus::Ok, None, None);
    } else {
        push_check(
            checks,
            "db.recovery_artifacts",
            CheckStatus::Warn,
            Some(format!(
                "Preserved recovery artifacts remain for this database family ({} item(s))",
                artifacts.len()
            )),
            Some(serde_json::json!({ "artifacts": artifacts })),
        );
    }

    Ok(())
}

/// Pass-4 cycle 3 — detector for `fm-state_files-recovery-artifacts-orphaned`.
///
/// Parallel to `check_recovery_artifacts` (which is information-only and
/// surfaces ALL preserved artifacts). This narrower check fires only when
/// at least one artifact is older than `RECOVERY_AGED_TTL_DAYS`. The
/// distinction matters because operators commonly keep recent recovery
/// backups for forensic value; auto-quarantining recent artifacts would
/// destroy that value. Aged artifacts past the TTL are the orphan class
/// the doctor offers to clean up via `--repair`.
fn check_recovery_artifacts_aged(
    beads_dir: &Path,
    db_path: &Path,
    checks: &mut Vec<CheckResult>,
) -> Result<()> {
    let aged = recovery_artifacts_aged(beads_dir, db_path)?;
    if aged.is_empty() {
        push_check(
            checks,
            "db.recovery_artifacts.aged",
            CheckStatus::Ok,
            None,
            None,
        );
        return Ok(());
    }
    let display: Vec<String> = aged.iter().map(|p| p.display().to_string()).collect();
    push_check(
        checks,
        "db.recovery_artifacts.aged",
        CheckStatus::Warn,
        Some(format!(
            "{} recovery artifact(s) older than {} days — eligible for quarantine via --repair",
            aged.len(),
            RECOVERY_AGED_TTL_DAYS
        )),
        Some(serde_json::json!({
            "artifacts": display,
            "ttl_days": RECOVERY_AGED_TTL_DAYS,
        })),
    );
    Ok(())
}

fn db_family_prefix(db_path: &Path) -> &str {
    db_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("beads.db")
}

fn recovery_artifacts_for_db_family(beads_dir: &Path, db_path: &Path) -> Result<Vec<PathBuf>> {
    let recovery_dir = config::recovery_dir_for_db_path(db_path, beads_dir);
    let db_prefix = db_family_prefix(db_path);
    let db_parent = db_path.parent().unwrap_or(beads_dir);
    let mut artifacts = Vec::new();

    if recovery_dir.is_dir() {
        for entry in fs::read_dir(&recovery_dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with(db_prefix) {
                artifacts.push(entry.path());
            }
        }
    }

    for entry in fs::read_dir(db_parent)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with(&format!("{db_prefix}.bad_")) {
            artifacts.push(entry.path());
        }
    }

    artifacts.sort();
    artifacts.dedup();
    Ok(artifacts)
}

/// Default TTL (in days) used by the aged-recovery-artifact detector +
/// quarantine fixer. Pass-4 cycle 3 introduces this knob as a hard-coded
/// constant; making it configurable via `metadata.json` is deferred to a
/// follow-up cycle. 30 days matches the pass-1 archaeology spec
/// (`fm-state_files-recovery-artifacts-orphaned`).
const RECOVERY_AGED_TTL_DAYS: u64 = 30;

/// Upper bound on filesystem entries visited while sizing foreign debris, so a
/// pathological tree cannot make `br doctor` slow. Reaching it sets
/// `truncated`, and the reported byte count becomes a lower bound.
const FOREIGN_DEBRIS_SCAN_LIMIT: usize = 4096;

/// `beads_rust-jwrr`: recovery copies left inside `.beads/` by something other
/// than br.
///
/// br writes exactly two directories — `.br_recovery/` and `.br_history/` —
/// and never copies a directory tree. But `.beads/` accumulates manual backups
/// from other tools and from ad-hoc agent sessions, with names like
/// `recovery_<TS>`, `recovery_snapshot_<TS>`, `snapshot_<TS>`,
/// `.beads_snapshot` and `<db>.rebuild_<TS>`. A session that runs
/// `cp -r .beads .beads/recovery_<TS>` copies `.beads` into itself, so those
/// trees nest and compound; one observed workspace reached gigabytes against a
/// database of tens of megabytes.
///
/// br cannot safely remove any of it — it did not create it and cannot know
/// what it is — so this is reported for the operator to act on and is never
/// auto-repaired.
fn is_foreign_recovery_debris_dir_name(name: &str) -> bool {
    // br's own state directories are `.br_recovery` and `.br_history`; never
    // report those as foreign.
    if name.starts_with(".br_") {
        return false;
    }
    name.starts_with("recovery") || name.starts_with("snapshot") || name.starts_with(".beads_snap")
}

/// Loose sibling copies of the database left by a manual rebuild. The `.bad_*`
/// spelling is already covered by [`recovery_artifacts_for_db_family`].
fn is_foreign_recovery_debris_file_name(name: &str, db_prefix: &str) -> bool {
    name.starts_with(&format!("{db_prefix}.rebuild_"))
}

/// What [`check_foreign_recovery_debris`] found. Byte counts come from
/// `fs::metadata` on regular files only; symlinks and specials are counted as
/// entries but contribute no bytes.
#[derive(Debug, Default)]
struct ForeignRecoveryDebris {
    directories: Vec<PathBuf>,
    files: Vec<PathBuf>,
    bytes: u64,
    truncated: bool,
}

impl ForeignRecoveryDebris {
    fn is_empty(&self) -> bool {
        self.directories.is_empty() && self.files.is_empty()
    }
}

/// Add up the regular-file bytes under `root`, visiting at most `remaining`
/// entries. Returns the bytes found and whether the budget ran out.
fn bounded_tree_bytes(root: &Path, remaining: &mut usize) -> (u64, bool) {
    let mut bytes = 0_u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if *remaining == 0 {
                return (bytes, true);
            }
            *remaining -= 1;
            match entry.file_type() {
                Ok(ft) if ft.is_dir() => stack.push(entry.path()),
                Ok(ft) if ft.is_file() => {
                    bytes = bytes.saturating_add(entry.metadata().map_or(0, |m| m.len()));
                }
                _ => {}
            }
        }
    }
    (bytes, false)
}

/// Scan one level of `.beads/` for foreign recovery debris and size it.
fn scan_foreign_recovery_debris(beads_dir: &Path, db_path: &Path) -> Result<ForeignRecoveryDebris> {
    let db_prefix = db_family_prefix(db_path);
    let mut found = ForeignRecoveryDebris::default();
    let mut budget = FOREIGN_DEBRIS_SCAN_LIMIT;

    if !beads_dir.is_dir() {
        return Ok(found);
    }
    for entry in fs::read_dir(beads_dir)?.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() && is_foreign_recovery_debris_dir_name(&name) {
            let (bytes, truncated) = bounded_tree_bytes(&entry.path(), &mut budget);
            found.bytes = found.bytes.saturating_add(bytes);
            found.truncated |= truncated;
            found.directories.push(entry.path());
        } else if file_type.is_file() && is_foreign_recovery_debris_file_name(&name, db_prefix) {
            found.bytes = found
                .bytes
                .saturating_add(entry.metadata().map_or(0, |m| m.len()));
            found.files.push(entry.path());
        }
    }

    found.directories.sort();
    found.files.sort();
    Ok(found)
}

/// Report foreign recovery debris as an **informational** `Ok` check.
///
/// Deliberately not a warning: br did not create these artifacts, cannot
/// classify them, and must not make `br doctor` exit non-zero — breaking a CI
/// gate — over leftovers from a different tool in an otherwise healthy
/// workspace. The finding still appears in the human report and in `--json`,
/// which is what the operator needs to reclaim the space themselves.
fn check_foreign_recovery_debris(
    beads_dir: &Path,
    db_path: &Path,
    checks: &mut Vec<CheckResult>,
) -> Result<()> {
    let found = scan_foreign_recovery_debris(beads_dir, db_path)?;
    if found.is_empty() {
        push_check(
            checks,
            "db.foreign_recovery_debris",
            CheckStatus::Ok,
            None,
            None,
        );
        return Ok(());
    }

    let approx = if found.truncated { "at least " } else { "" };
    let message = format!(
        "{} foreign recovery artifact(s) in .beads/ were not created by br \
         ({} director{}, {} file(s), {approx}{:.1} MB); br will not remove them",
        found.directories.len() + found.files.len(),
        found.directories.len(),
        if found.directories.len() == 1 {
            "y"
        } else {
            "ies"
        },
        found.files.len(),
        found.bytes as f64 / (1024.0 * 1024.0),
    );
    let display = |paths: &[PathBuf]| -> Vec<String> {
        paths.iter().map(|p| p.display().to_string()).collect()
    };
    push_check(
        checks,
        "db.foreign_recovery_debris",
        CheckStatus::Ok,
        Some(message),
        Some(serde_json::json!({
            "directories": display(&found.directories),
            "files": display(&found.files),
            "bytes": found.bytes,
            "bytes_are_lower_bound": found.truncated,
            "remediation": "Inspect with `du -sh .beads/*` and remove what you \
                            no longer need. br never created these and will \
                            not delete them.",
            "finding_id": "fm-state_files-recovery-artifacts-orphaned",
        })),
    );
    Ok(())
}

/// Subset of `recovery_artifacts_for_db_family` whose mtime is older than
/// `now - RECOVERY_AGED_TTL_DAYS`. Pure: no mutations, only `fs::metadata`
/// calls. Entries whose mtime cannot be read are SKIPPED (we don't want to
/// auto-quarantine artifacts we can't reason about).
fn recovery_artifacts_aged(beads_dir: &Path, db_path: &Path) -> Result<Vec<PathBuf>> {
    use std::time::{Duration, SystemTime};
    let all = recovery_artifacts_for_db_family(beads_dir, db_path)?;
    let threshold = Duration::from_secs(RECOVERY_AGED_TTL_DAYS * 24 * 60 * 60);
    let now = SystemTime::now();
    let mut aged = Vec::new();
    for path in all {
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        if now
            .duration_since(mtime)
            .map(|age| age > threshold)
            .unwrap_or(false)
        {
            aged.push(path);
        }
    }
    Ok(aged)
}

fn is_failed_jsonl_rebuild_artifact(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.contains(".rebuild-failed")
                || name.ends_with(JSONL_REBUILD_VERIFICATION_FAILED_SUFFIX)
        })
}

fn prior_jsonl_rebuild_failure_evidence(
    beads_dir: &Path,
    db_path: &Path,
) -> Result<Option<PriorJsonlRebuildFailureEvidence>> {
    let artifacts = recovery_artifacts_for_db_family(beads_dir, db_path)?;
    let evidence_path = artifacts
        .iter()
        .find(|path| is_failed_jsonl_rebuild_artifact(path))
        .cloned();

    Ok(evidence_path.map(|path| PriorJsonlRebuildFailureEvidence {
        path,
        artifact_count: artifacts.len(),
    }))
}

fn repeated_jsonl_rebuild_refusal_message(evidence: &PriorJsonlRebuildFailureEvidence) -> String {
    format!(
        "{JSONL_REBUILD_REPEAT_ERROR_PREFIX}: prior failed recovery evidence remains at '{}' among {} preserved database-family artifact(s). Inspect and preserve the recovery evidence before rerunning with --allow-repeated-repair.",
        evidence.path.display(),
        evidence.artifact_count
    )
}

fn repeated_jsonl_rebuild_refusal_reason(
    beads_dir: &Path,
    db_path: &Path,
    allow_repeated_repair: bool,
) -> Result<Option<String>> {
    if allow_repeated_repair {
        return Ok(None);
    }

    Ok(prior_jsonl_rebuild_failure_evidence(beads_dir, db_path)?
        .as_ref()
        .map(repeated_jsonl_rebuild_refusal_message))
}

fn push_inspection_error(
    checks: &mut Vec<CheckResult>,
    name: &str,
    context: &str,
    err: &BeadsError,
) {
    push_check(
        checks,
        name,
        CheckStatus::Error,
        Some(format!("{context}: {err}")),
        None,
    );
}

fn build_issue_write_probe_check(
    issue_id: &str,
    update_result: std::result::Result<usize, FrankenError>,
    rollback_result: std::result::Result<usize, FrankenError>,
) -> CheckResult {
    let mut details = serde_json::json!({ "issue_id": issue_id });

    match (update_result, rollback_result) {
        (Ok(affected_rows), Ok(_)) => {
            if affected_rows > 0 {
                CheckResult {
                    name: "db.write_probe".to_string(),
                    status: CheckStatus::Ok,
                    message: Some(format!(
                        "Rollback-only issue write succeeded for {issue_id}"
                    )),
                    details: None,
                }
            } else {
                details["affected_rows"] = serde_json::json!(affected_rows);
                CheckResult {
                    name: "db.write_probe".to_string(),
                    status: CheckStatus::Error,
                    message: Some(format!(
                        "Rollback-only issue write affected 0 rows for {issue_id}"
                    )),
                    details: Some(details),
                }
            }
        }
        (Ok(affected_rows), Err(rollback_err)) => {
            details["affected_rows"] = serde_json::json!(affected_rows);
            details["rollback_error"] = serde_json::json!(rollback_err.to_string());
            let message = if affected_rows == 0 {
                format!(
                    "Rollback-only issue write affected 0 rows and rollback also failed: {rollback_err}"
                )
            } else {
                format!("Rollback-only issue write succeeded but rollback failed: {rollback_err}")
            };
            CheckResult {
                name: "db.write_probe".to_string(),
                status: CheckStatus::Error,
                message: Some(message),
                details: Some(details),
            }
        }
        (Err(update_err), Ok(_)) => CheckResult {
            name: "db.write_probe".to_string(),
            status: CheckStatus::Error,
            message: Some(format!("Rollback-only issue write failed: {update_err}")),
            details: Some(details),
        },
        (Err(update_err), Err(rollback_err)) => {
            details["rollback_error"] = serde_json::json!(rollback_err.to_string());
            CheckResult {
                name: "db.write_probe".to_string(),
                status: CheckStatus::Error,
                message: Some(format!(
                    "Rollback-only issue write failed and rollback also failed: {update_err}"
                )),
                details: Some(details),
            }
        }
    }
}

fn repair_database_from_jsonl(
    beads_dir: &Path,
    db_path: &Path,
    jsonl_path: &Path,
    cli: &config::CliOverrides,
    show_progress: bool,
    session: Option<&mut DoctorRepairSession>,
) -> Result<DoctorRepairResult> {
    let jsonl_authority =
        blocking_jsonl_family_write_lock_with_timeout(jsonl_path, cli.lock_timeout)?;
    jsonl_authority.verify_jsonl_authority()?;
    let source = capture_jsonl_source_snapshot(jsonl_path)?;
    preflight_jsonl_rebuild_authority(&source)?;

    if let Some(session) = session {
        let family_paths = existing_sqlite_family_paths_for_legacy_op(db_path);
        let path_refs: Vec<&Path> = family_paths.iter().map(PathBuf::as_path).collect();
        return match session.record_legacy_mutation_result(
            "doctor.jsonl_rebuild",
            &path_refs,
            || {
                repair_database_from_jsonl_after_preflight(
                    beads_dir,
                    db_path,
                    cli,
                    show_progress,
                    &source,
                    &jsonl_authority,
                )
            },
        )? {
            Some(result) => Ok(result),
            None => Err(BeadsError::Config(
                JSONL_REBUILD_DRY_RUN_SKIP_MESSAGE.to_string(),
            )),
        };
    }

    repair_database_from_jsonl_after_preflight(
        beads_dir,
        db_path,
        cli,
        show_progress,
        &source,
        &jsonl_authority,
    )
}

fn repair_database_from_jsonl_after_preflight(
    beads_dir: &Path,
    db_path: &Path,
    cli: &config::CliOverrides,
    show_progress: bool,
    source: &JsonlSourceSnapshot,
    jsonl_authority: &crate::sync::JsonlFamilyWriteLock,
) -> Result<DoctorRepairResult> {
    let bootstrap_layer = config::ConfigLayer::merge_layers(&[
        config::load_startup_config(beads_dir)?,
        cli.as_layer(),
    ]);

    // Snapshot any local tombstones and dirty live issues before the
    // JSONL rebuild. `doctor --repair` reaches this branch only after
    // light repairs failed (or never applied) and the on-disk DB reports
    // errors, so the storage handle here might be limping — but the
    // snapshot helpers are already fault-tolerant (warn+empty on
    // enumeration failure, warn+partial on per-issue failure) and the
    // cost of trying is a few selects. Without this, repair would
    // silently wipe any tombstone the user deleted but had not yet
    // flushed to JSONL (same hazard that `br sync --import-only
    // --rebuild` preserves via snapshot/restore) and any creation or
    // edit that had not yet been flushed (GitHub #394), since the
    // rebuild only replays what's in the JSONL.
    let (preserved_tombstones, preserved_dirty_issues) =
        preserved_state_for_doctor_rebuild(db_path, source);

    // GitHub #471: snapshot every DB-only append-only/history table before
    // the rebuild. The JSONL carries issue state only; without this the
    // rebuild silently empties events, gate results, close/bypass audit
    // records, and capacity state while reporting success.
    let history_snapshot = snapshot_history_for_doctor_rebuild(db_path);
    for warning in &history_snapshot.failures {
        tracing::warn!(
            db_path = %db_path.display(),
            warning,
            "doctor --repair could not snapshot part of the DB-only history before the JSONL rebuild; those rows will be lost"
        );
    }

    let recovery =
        if let Some(authority) = cli.database_family_write_authority_for(beads_dir, db_path) {
            config::repair_database_from_jsonl_snapshot_under_write_authority(
                beads_dir,
                db_path,
                cli.lock_timeout,
                &bootstrap_layer,
                show_progress,
                false,
                source,
                jsonl_authority,
                authority,
            )
        } else {
            config::repair_database_from_jsonl_snapshot(
                beads_dir,
                db_path,
                cli.lock_timeout,
                &bootstrap_layer,
                show_progress,
                false,
                source,
                jsonl_authority,
            )
        };
    let (mut storage, import_result, verified_backups) = recovery?;

    restore_tombstones_after_rebuild(&mut storage, &preserved_tombstones)?;
    restore_dirty_issues_after_rebuild(&mut storage, &preserved_dirty_issues)?;

    // GitHub #471: reattach the DB-only history captured before the rebuild.
    // A restore failure is downgraded to a loud warning rather than failing
    // the whole repair: at this point the rebuilt database is already the
    // best available state, and aborting would leave the workspace corrupt
    // AND still lose the history.
    let mut history_preservation_warnings = history_snapshot.failures.clone();
    let preserved_history = match storage.restore_auxiliary_history_tables(&history_snapshot) {
        Ok(report) => report
            .into_iter()
            .map(|(table, restored, skipped)| PreservedHistoryTableReport {
                table,
                restored,
                skipped,
            })
            .collect(),
        Err(err) => {
            history_preservation_warnings.push(format!(
                "could not restore the {} snapshotted history row(s) into the rebuilt database: {err}",
                history_snapshot.row_count()
            ));
            Vec::new()
        }
    };

    let fk_violations_cleaned = cleanup_repair_missing_issue_references(&mut storage)?;

    Ok(DoctorRepairResult {
        imported: import_result.imported_count,
        skipped: import_result.skipped_count,
        fk_violations_cleaned,
        preserved_tombstones: preserved_tombstones.len(),
        preserved_dirty_issues: preserved_dirty_issues.len(),
        preserved_dirty_issue_ids: preserved_dirty_issues
            .iter()
            .map(|preserved| preserved.issue.id.clone())
            .collect(),
        verified_backups,
        preserved_history,
        history_preservation_warnings,
    })
}

/// Open the pre-repair database (best effort — it is usually corrupt when
/// this runs) and snapshot every DB-only history table so the JSONL rebuild
/// can carry them across (GitHub #471). An unopenable database yields an
/// empty snapshot with a loud failure entry instead of aborting the repair.
fn snapshot_history_for_doctor_rebuild(
    db_path: &Path,
) -> crate::storage::sqlite::AuxiliaryHistorySnapshot {
    let mut snapshot = crate::storage::sqlite::AuxiliaryHistorySnapshot::default();
    if !db_path.is_file() {
        return snapshot;
    }
    match SqliteStorage::open(db_path) {
        Ok(storage) => storage.snapshot_auxiliary_history_tables(),
        Err(err) => {
            snapshot.failures.push(format!(
                "could not open the pre-repair database to preserve history tables ({}): {err}",
                crate::storage::sqlite::AUXILIARY_HISTORY_TABLES.join(", ")
            ));
            snapshot
        }
    }
}

fn cleanup_repair_missing_issue_references(storage: &mut SqliteStorage) -> Result<usize> {
    let missing_references = storage.missing_issue_references()?;
    if missing_references.is_empty() {
        return Ok(0);
    }

    tracing::warn!(
        references = ?missing_references,
        "Missing issue references found after repair import; cleaning local orphans"
    );

    let orphan_tables = &[
        ("dependencies", "issue_id"),
        ("dependencies", "depends_on_id"),
        ("labels", "issue_id"),
        ("comments", "issue_id"),
        ("events", "issue_id"),
        ("dirty_issues", "issue_id"),
        ("export_hashes", "issue_id"),
        ("blocked_issues_cache", "issue_id"),
        ("child_counters", "parent_id"),
        ("close_metadata", "issue_id"),
        ("gate_result_history", "issue_id"),
        ("gate_results", "issue_id"),
        ("capacity_exemption_history", "issue_id"),
        ("capacity_exemptions", "issue_id"),
        ("capacity_occupancy", "issue_id"),
    ];
    let mut cleaned = 0usize;
    let mut dependency_rows_cleaned = 0usize;

    for (table, col) in orphan_tables {
        let external_dependency_filter = match (*table, *col) {
            ("dependencies", "issue_id") => " AND issue_id NOT LIKE 'external:%'",
            ("dependencies", "depends_on_id") => " AND depends_on_id NOT LIKE 'external:%'",
            _ => "",
        };
        let cleanup = format!(
            "DELETE FROM {table} WHERE {col} NOT IN (SELECT id FROM issues){external_dependency_filter}"
        );
        let removed = storage.execute_raw_count(&cleanup)?;
        if *table == "dependencies" {
            dependency_rows_cleaned += removed;
        }
        cleaned += removed;
    }

    let remaining = storage.missing_issue_references()?;
    if !remaining.is_empty() {
        return Err(BeadsError::Config(format!(
            "Repair import finished with orphaned issue references still present: {}",
            remaining.join(", ")
        )));
    }

    if dependency_rows_cleaned > 0 {
        storage.rebuild_blocked_cache(true)?;
    }

    Ok(cleaned)
}

fn preflight_jsonl_rebuild_authority(source: &JsonlSourceSnapshot) -> Result<()> {
    let conflict_markers = scan_conflict_markers_snapshot(source)?;
    if !conflict_markers.is_empty() {
        let preview = conflict_markers
            .iter()
            .take(3)
            .map(|marker| {
                let branch = marker
                    .branch
                    .as_ref()
                    .map_or(String::new(), |branch| format!(" ({branch})"));
                format!("line {}: {:?}{branch}", marker.line, marker.marker_type)
            })
            .collect::<Vec<_>>()
            .join("; ");
        let suffix = if conflict_markers.len() > 3 {
            " ..."
        } else {
            ""
        };
        return Err(BeadsError::Config(format!(
            "{JSONL_REBUILD_AUTHORITY_ERROR_PREFIX}: found {} merge conflict marker(s): {preview}{suffix}. Resolve JSONL conflicts before rebuilding SQLite from it.",
            conflict_markers.len()
        )));
    }

    let validation = validate_jsonl_snapshot_issue_records(source)?;
    if validation.invalid_count > 0 {
        let preview = validation.preview_messages().join("; ");
        let suffix = if validation.invalid_count > validation.failures.len() {
            " ..."
        } else {
            ""
        };
        return Err(BeadsError::Config(format!(
            "{JSONL_REBUILD_AUTHORITY_ERROR_PREFIX}: found {} invalid issue record(s): {preview}{suffix}. Fix JSONL before rebuilding SQLite from it.",
            validation.invalid_count
        )));
    }

    Ok(())
}

fn jsonl_rebuild_failure_outcome(err: &BeadsError) -> &'static str {
    if let BeadsError::Config(message) = err {
        if message.starts_with(JSONL_REBUILD_AUTHORITY_ERROR_PREFIX) {
            return "refused";
        }
        if message == JSONL_REBUILD_DRY_RUN_SKIP_MESSAGE {
            return "skipped";
        }
    }
    "failed"
}

/// Errors that already carry their own accurate, actionable message and must be
/// surfaced verbatim rather than wrapped in a repair-failure message: the
/// preflight authority refusal and the `--dry-run` skip.
fn jsonl_rebuild_error_is_self_describing(outcome: &str) -> bool {
    matches!(outcome, "refused" | "skipped")
}

/// Peel `WithContext` wrappers so classification sees the originating error
/// rather than the annotation layered over it.
fn jsonl_rebuild_root_error(err: &BeadsError) -> &BeadsError {
    match err {
        BeadsError::WithContext { source, .. } => source
            .downcast_ref::<BeadsError>()
            .map_or(err, jsonl_rebuild_root_error),
        _ => err,
    }
}

/// True when a rebuild failed because the database family could not be opened
/// or locked — not because anything is wrong with the JSONL.
///
/// The engine surfaces this as `unable to open database file`
/// ([`FrankenError::CannotOpen`]) when a second `br` process, an editor plugin,
/// or an MCP `br serve` session already holds the workspace. Matching on the
/// variants rather than on message text keeps this stable across engine
/// wording changes.
fn is_database_unavailable_failure(err: &BeadsError) -> bool {
    match jsonl_rebuild_root_error(err) {
        BeadsError::DatabaseLocked { .. } => true,
        BeadsError::Database(inner) => matches!(
            inner,
            FrankenError::CannotOpen { .. }
                | FrankenError::DatabaseLocked { .. }
                | FrankenError::LockFailed { .. }
                | FrankenError::Busy
        ),
        _ => false,
    }
}

/// True when the import rejected the JSONL's *contents*, so pointing the
/// operator at the export is genuinely the right advice.
fn is_jsonl_content_failure(err: &BeadsError) -> bool {
    matches!(
        jsonl_rebuild_root_error(err),
        BeadsError::JsonlParse { .. }
            | BeadsError::PrefixMismatch { .. }
            | BeadsError::ImportCollision { .. }
    )
}

/// Build the operator-facing message for a failed JSONL rebuild.
///
/// `beads_rust-krdi`: this was a single residual bucket that appended "The
/// JSONL file may be corrupt. Try manually editing the JSONL to fix invalid
/// records." to every error that was not an authority refusal — and that
/// bucket is structurally the set of failures which are *not* JSONL
/// corruption, because real corruption is caught earlier by
/// [`preflight_jsonl_rebuild_authority`] and refused with its own message.
/// The advice was therefore wrong for most of them, and actively harmful for
/// the common one: a locked database sent the operator off to hand-edit a
/// perfectly healthy export, and an agent parsing the message would conclude
/// the export was corrupt.
///
/// Each branch now names the remedy that actually applies, and the residual
/// case reports the failure without attributing a cause to either store.
fn jsonl_rebuild_failure_message(err: &BeadsError) -> String {
    if is_database_unavailable_failure(err) {
        return format!(
            "Repair import failed: {err}. \
             The database could not be opened for writing — the JSONL is not implicated. \
             Another process most likely holds this workspace: close other `br` \
             invocations and any MCP `br serve` session, check `.beads/.write.lock`, \
             then retry."
        );
    }
    if is_jsonl_content_failure(err) {
        return format!(
            "Repair import failed: {err}. \
             The import rejected records in the JSONL. \
             Fix the offending records in `.beads/issues.jsonl`, then retry."
        );
    }
    format!(
        "Repair import failed: {err}. \
         No database writes were applied; the workspace is unchanged. \
         Re-run `br doctor --json` to inspect the current state."
    )
}

fn write_jsonl_rebuild_verification_failed_marker(
    beads_dir: &Path,
    db_path: &Path,
    post_repair: &DoctorRun,
    repair_result: &DoctorRepairResult,
    session: &mut DoctorRepairSession,
) -> Result<PathBuf> {
    let recovery_dir = config::recovery_dir_for_db_path(db_path, beads_dir);
    let stamp = Utc::now().format("%Y%m%d_%H%M%S_%f");
    let marker_path = recovery_dir.join(format!(
        "{}.{stamp}{JSONL_REBUILD_VERIFICATION_FAILED_SUFFIX}",
        db_family_prefix(db_path)
    ));
    let failed_checks = post_repair
        .report
        .checks
        .iter()
        .filter(|check| {
            matches!(check.status, CheckStatus::Error) || is_warn_level_page_anomaly_check(check)
        })
        .collect::<Vec<_>>();
    let payload = serde_json::json!({
        "phase": "doctor.jsonl_rebuild",
        "action": "jsonl_rebuild",
        "outcome": "verification_failed",
        "created_at": Utc::now().to_rfc3339(),
        "db_path": db_path.display().to_string(),
        "imported": repair_result.imported,
        "skipped": repair_result.skipped,
        "fk_violations_cleaned": repair_result.fk_violations_cleaned,
        "preserved_tombstones": repair_result.preserved_tombstones,
        "preserved_dirty_issues": repair_result.preserved_dirty_issues,
        "preserved_dirty_issue_ids": &repair_result.preserved_dirty_issue_ids,
        "verified_backups": &repair_result.verified_backups,
        "workspace_health": post_repair.report.workspace_health.as_deref(),
        "failed_checks": failed_checks,
    });
    let bytes = serde_json::to_vec_pretty(&payload)?;
    session.set_fixer("doctor.jsonl_rebuild_verification_marker");
    chokepoint::mutate(
        &session.ctx,
        &marker_path,
        Op::WriteFile {
            content: bytes,
            mode: None,
        },
    )?;
    Ok(marker_path)
}

/// Best-effort snapshot of unflushed local tombstones and dirty live
/// issues, guarded on every failure mode the doctor-repair path may
/// encounter (DB missing, DB can't be opened, JSONL unreadable).
///
/// This mirrors the helper at the config layer (`preserved_unflushed_state`)
/// but has to live here because the doctor-repair entry point is where we
/// have the storage-open attempt — the config helper receives an
/// already-open storage handle. Returns `(tombstones, dirty_live_issues)`;
/// each vector is empty if nothing survives filtering or if any step
/// fails. The rebuild itself always proceeds either way, and the snapshot
/// helpers log their own best-effort warnings.
fn preserved_state_for_doctor_rebuild(
    db_path: &Path,
    source: &JsonlSourceSnapshot,
) -> (Vec<PreservedIssue>, Vec<PreservedIssue>) {
    if !db_path.is_file() {
        return (Vec::new(), Vec::new());
    }
    let storage = match SqliteStorage::open(db_path) {
        Ok(storage) => storage,
        Err(err) => {
            tracing::debug!(
                db_path = %db_path.display(),
                error = %err,
                "Could not open DB for pre-repair preservation snapshot; proceeding without preservation"
            );
            return (Vec::new(), Vec::new());
        }
    };
    let tombstone_snapshot = snapshot_tombstones(&storage);
    let dirty_snapshot = snapshot_dirty_live_issues(&storage);
    drop(storage);
    if tombstone_snapshot.is_empty() && dirty_snapshot.is_empty() {
        return (tombstone_snapshot, dirty_snapshot);
    }
    let jsonl_filter = match scan_jsonl_snapshot_for_tombstone_filter(source) {
        Ok(filter) => filter,
        Err(err) => {
            tracing::debug!(
                jsonl_path = %source.display_path().display(),
                error = %err,
                "Could not scan immutable JSONL snapshot for tombstone filter during doctor --repair; preserving every snapshotted issue and letting the rebuild surface the JSONL error"
            );
            JsonlTombstoneFilter::default()
        }
    };
    (
        tombstones_missing_from_jsonl_tombstones(tombstone_snapshot, &jsonl_filter),
        dirty_issues_missing_from_jsonl(dirty_snapshot, &jsonl_filter),
    )
}

#[cfg(test)]
fn repair_recoverable_db_state(
    beads_dir: &Path,
    db_path: &Path,
    report: &DoctorReport,
    session: Option<&mut DoctorRepairSession>,
    fixer_filter: &FixerFilter,
) -> LocalRepairResult {
    let write_authority = match acquire_doctor_database_write_authority(
        beads_dir,
        db_path,
        Some(crate::sync::default_write_lock_timeout_ms()),
    ) {
        Ok(authority) => authority,
        Err(err) => {
            tracing::warn!(
                path = %db_path.display(),
                error = %err,
                "Skipping recoverable database repair because authority could not be acquired"
            );
            return LocalRepairResult::default();
        }
    };
    repair_recoverable_db_state_under_write_authority(
        beads_dir,
        db_path,
        report,
        session,
        fixer_filter,
        &write_authority,
    )
}

fn repair_recoverable_db_state_under_write_authority(
    beads_dir: &Path,
    db_path: &Path,
    report: &DoctorReport,
    mut session: Option<&mut DoctorRepairSession>,
    fixer_filter: &FixerFilter,
    write_authority: &Arc<crate::sync::DatabaseFamilyWriteLock>,
) -> LocalRepairResult {
    let mut repair = LocalRepairResult::default();

    if report_has_sidecar_anomaly(report)
        && fixer_filter.allows("fm-state_files-wal-shm-sidecar-orphan")
    {
        if let Err(err) = write_authority.verify_database_authority() {
            tracing::warn!(
                path = %db_path.display(),
                error = %err,
                "Skipping sidecar repair because database authority was lost"
            );
            return repair;
        }
        repair_database_sidecars(db_path, &mut repair, session.as_deref_mut());
        if let Err(err) = write_authority.verify_database_authority() {
            tracing::warn!(
                path = %db_path.display(),
                error = %err,
                "Database authority changed during sidecar repair"
            );
            return repair;
        }
    }

    if !db_path.is_file() {
        tracing::debug!(
            path = %db_path.display(),
            "Skipping blocked-cache repair because the database file is missing"
        );
        return repair;
    }

    // A truncated/garbage `-wal` makes the engine refuse to open the
    // database at all. The normal startup path quarantines that sidecar
    // before opening; local repair must do the same or it silently skips
    // the very repair the operator ran `--repair` for.
    //
    // Only retry when the main database file is itself a real SQLite image:
    // if the primary file is not a database, moving its sidecar aside cannot
    // help and the JSONL rebuild path owns that case. Both open attempts go
    // through the authority-verified doctor opener so the database-family
    // write authority is checked on either side of the quarantine.
    let open_for_repair = || {
        open_doctor_storage_under_write_authority(db_path, write_authority).or_else(|first_err| {
            if !db_file_has_sqlite_header(db_path) {
                return Err(first_err);
            }
            crate::config::quarantine_truncated_wal_sidecar(db_path, beads_dir);
            open_doctor_storage_under_write_authority(db_path, write_authority).inspect_err(|_| {
                tracing::debug!(
                    path = %db_path.display(),
                    first_error = %first_err,
                    "Reopen after truncated-WAL quarantine still failed"
                );
            })
        })
    };

    let do_rebuild = |repair: &mut LocalRepairResult| match open_for_repair() {
        Ok(mut storage) => {
            let force_rebuild = report_has_projection_content_mismatch_finding(report);
            let rebuild_result = if force_rebuild {
                storage.rebuild_blocked_cache(true).map(|_| true)
            } else {
                storage.ensure_blocked_cache_fresh()
            };

            match rebuild_result {
                Ok(blocked_cache_rebuilt) => {
                    repair.blocked_cache_rebuilt = blocked_cache_rebuilt;
                }
                Err(err) => {
                    tracing::warn!(
                        path = %db_path.display(),
                        error = %err,
                        "Skipping blocked-cache repair; falling back to JSONL rebuild"
                    );
                }
            }
        }
        Err(err) => {
            tracing::warn!(
                path = %db_path.display(),
                error = %err,
                "Skipping blocked-cache repair because the database could not be opened"
            );
        }
    };
    if let Some(session) = session {
        let family_paths = existing_sqlite_family_paths_for_legacy_op(db_path);
        let family_refs: Vec<&Path> = family_paths.iter().map(PathBuf::as_path).collect();
        let result =
            session.record_legacy_mutation("repair_recoverable_db_state", &family_refs, || {
                do_rebuild(&mut repair);
                Ok(())
            });
        if let Err(err) = result {
            tracing::warn!(
                path = %db_path.display(),
                error = %err,
                "Failed to record blocked-cache rebuild legacy-op audit; mutation still proceeded if possible"
            );
        }
    } else {
        do_rebuild(&mut repair);
    }
    repair
}

/// Rebuild all indexes via `REINDEX` to fix partial-index row mismatches.
///
/// This is safe — `REINDEX` only rebuilds existing indexes from the underlying
/// table data.  It does not modify any row data.
#[cfg(test)]
fn repair_partial_indexes(
    db_path: &Path,
    repair: &mut LocalRepairResult,
    session: Option<&mut DoctorRepairSession>,
) {
    let Some(beads_dir) = db_path.parent() else {
        tracing::warn!(path = %db_path.display(), "Skipping REINDEX for parentless database path");
        return;
    };
    let write_authority = match acquire_doctor_database_write_authority(
        beads_dir,
        db_path,
        Some(crate::sync::default_write_lock_timeout_ms()),
    ) {
        Ok(authority) => authority,
        Err(err) => {
            tracing::warn!(
                path = %db_path.display(),
                error = %err,
                "Skipping REINDEX because database authority could not be acquired"
            );
            return;
        }
    };
    repair_partial_indexes_under_write_authority(db_path, repair, session, &write_authority);
}

fn repair_partial_indexes_under_write_authority(
    db_path: &Path,
    repair: &mut LocalRepairResult,
    session: Option<&mut DoctorRepairSession>,
    write_authority: &Arc<crate::sync::DatabaseFamilyWriteLock>,
) {
    if !db_path.is_file() {
        tracing::debug!(
            path = %db_path.display(),
            "Skipping REINDEX because the database file is missing"
        );
        return;
    }

    let do_reindex = |repair: &mut LocalRepairResult| {
        if let Err(err) = write_authority.verify_database_authority() {
            tracing::warn!(
                path = %db_path.display(),
                error = %err,
                "Skipping REINDEX because database authority was lost"
            );
            return;
        }
        match Connection::open(db_path.to_string_lossy().into_owned()) {
            Ok(conn) => {
                let _ = conn.execute("PRAGMA busy_timeout=30000");
                match conn.execute("REINDEX") {
                    Ok(_) => {
                        tracing::info!(
                            path = %db_path.display(),
                            "REINDEX completed successfully"
                        );
                        repair.indexes_reindexed = true;
                    }
                    Err(err) => {
                        tracing::warn!(
                            path = %db_path.display(),
                            error = %err,
                            "REINDEX failed; partial-index warnings may persist"
                        );
                    }
                }
                if let Err(err) = conn.close() {
                    tracing::warn!(
                        path = %db_path.display(),
                        error = %err,
                        "REINDEX connection close failed"
                    );
                }
                if let Err(err) = write_authority.verify_database_authority() {
                    tracing::warn!(
                        path = %db_path.display(),
                        error = %err,
                        "Database authority changed during REINDEX"
                    );
                    repair.indexes_reindexed = false;
                }
            }
            Err(err) => {
                tracing::warn!(
                    path = %db_path.display(),
                    error = %err,
                    "Skipping REINDEX because the database could not be opened"
                );
            }
        }
    };
    if let Some(session) = session {
        let family_paths = existing_sqlite_family_paths_for_legacy_op(db_path);
        let family_refs: Vec<&Path> = family_paths.iter().map(PathBuf::as_path).collect();
        let result = session.record_legacy_mutation("repair_partial_indexes", &family_refs, || {
            do_reindex(repair);
            Ok(())
        });
        if let Err(err) = result {
            tracing::warn!(
                path = %db_path.display(),
                error = %err,
                "Failed to record REINDEX legacy-op audit; mutation still proceeded if possible"
            );
        }
    } else {
        do_reindex(repair);
    }
}

// Route anomalous SQLite sidecar quarantine through the doctor
// chokepoint. These are removals from the live `.beads/` surface in
// operator terms, so AGENTS.md's no-delete invariant means they must be
// explicit `Op::Rename` moves into the per-run quarantine tree. That gives
// `doctor undo` a concrete inverse move instead of a legacy audit line
// that only points at the vanished source path.
fn repair_database_sidecars(
    db_path: &Path,
    repair: &mut LocalRepairResult,
    session: Option<&mut DoctorRepairSession>,
) {
    match inspect_database_sidecars(db_path) {
        Ok(_) => quarantine_anomalous_sidecars(db_path, repair, session),
        Err(err) => tracing::warn!(
            path = %db_path.display(),
            error = %err,
            "Skipping sidecar repair because filesystem inspection failed"
        ),
    }
}

fn quarantine_anomalous_sidecars(
    db_path: &Path,
    repair: &mut LocalRepairResult,
    session: Option<&mut DoctorRepairSession>,
) {
    match inspect_database_sidecars(db_path) {
        Ok(post_checkpoint_inspection) => {
            let quarantine_paths: BTreeSet<_> = post_checkpoint_inspection
                .quarantine_candidates
                .into_iter()
                .collect();

            if quarantine_paths.is_empty() {
                return;
            }

            let Some(session) = session else {
                tracing::warn!(
                    path = %db_path.display(),
                    "Skipping sidecar quarantine: no doctor repair session is available"
                );
                return;
            };

            session.set_fixer("doctor.database_sidecar_quarantine");
            let mut quarantined = Vec::new();
            for source in &quarantine_paths {
                let Some(name) = source.file_name() else {
                    tracing::warn!(
                        path = %source.display(),
                        "Skipping sidecar quarantine candidate without a file name"
                    );
                    continue;
                };
                let dest = session
                    .run
                    .root
                    .join("quarantine")
                    .join(".beads")
                    .join(name);

                match chokepoint::mutate(&session.ctx, source, Op::Rename { to: dest.clone() }) {
                    Ok(result) if result.ok => {
                        quarantined.push(dest.display().to_string());
                    }
                    Ok(_) => tracing::warn!(
                        path = %source.display(),
                        "Sidecar quarantine no-op"
                    ),
                    Err(err) => tracing::warn!(
                        path = %source.display(),
                        error = %err,
                        "Failed to quarantine anomalous database sidecar artifact"
                    ),
                }
            }
            if !quarantined.is_empty() {
                repair.quarantined_artifacts = quarantined;
            }
        }
        Err(err) => tracing::warn!(
            path = %db_path.display(),
            error = %err,
            "Failed to re-inspect database sidecars after local repair"
        ),
    }
}

thread_local! {
    /// The engine block for the workspace the flat doctor run resolved, so
    /// every report printed by that run (initial, post-repair) carries it
    /// without threading it through the ~70 `DoctorReport` constructors.
    static ENGINE_BLOCK: std::cell::RefCell<Option<EngineBlock>> = const { std::cell::RefCell::new(None) };
}

fn set_engine_block(block: EngineBlock) {
    ENGINE_BLOCK.with(|slot| *slot.borrow_mut() = Some(block));
}

fn current_engine_block() -> Option<EngineBlock> {
    ENGINE_BLOCK.with(|slot| slot.borrow().clone())
}

fn print_report(report: &DoctorReport, ctx: &OutputContext) -> Result<()> {
    if ctx.is_json() {
        match current_engine_block() {
            Some(engine) => {
                let mut value = serde_json::to_value(report).map_err(|err| {
                    BeadsError::validation("doctor", format!("cannot serialize report: {err}"))
                })?;
                if let serde_json::Value::Object(map) = &mut value {
                    map.insert(
                        "engine".to_string(),
                        serde_json::to_value(&engine).unwrap_or(serde_json::Value::Null),
                    );
                }
                ctx.json(&value);
            }
            None => ctx.json(report),
        }
        return Ok(());
    }
    if ctx.is_quiet() {
        return Ok(());
    }
    if ctx.is_rich() {
        render_doctor_rich(report, ctx);
        return Ok(());
    }

    print_report_plain(report);
    Ok(())
}

fn print_report_plain(report: &DoctorReport) {
    println!("br doctor");
    if let Some(health) = &report.workspace_health {
        println!("HEALTH workspace: {health}");
    }
    if let Some(engine) = current_engine_block() {
        println!("ENGINE {}", engine.summary_line());
    }
    for check in &report.checks {
        let label = match check.status {
            CheckStatus::Ok => "OK",
            CheckStatus::Warn => "WARN",
            CheckStatus::Error => "ERROR",
        };
        if let Some(message) = &check.message {
            println!("{label} {}: {}", check.name, message);
        } else {
            println!("{label} {}", check.name);
        }
    }
}

fn render_doctor_rich(report: &DoctorReport, ctx: &OutputContext) {
    let theme = ctx.theme();
    let mut content = Text::new("");

    let mut ok_count = 0usize;
    let mut warn_count = 0usize;
    let mut error_count = 0usize;
    for check in &report.checks {
        match check.status {
            CheckStatus::Ok => ok_count += 1,
            CheckStatus::Warn => warn_count += 1,
            CheckStatus::Error => error_count += 1,
        }
    }

    content.append_styled("Diagnostics Report\n", theme.emphasis.clone());
    content.append("\n");

    content.append_styled("Status: ", theme.dimmed.clone());
    if report.ok {
        content.append_styled("OK", theme.success.clone());
    } else {
        content.append_styled("Issues found", theme.error.clone());
    }
    content.append("\n");

    if let Some(health) = &report.workspace_health {
        content.append_styled("Health: ", theme.dimmed.clone());
        content.append_styled(health, theme.accent.clone());
        content.append("\n");
    }

    content.append_styled("Checks: ", theme.dimmed.clone());
    content.append_styled(
        &format!("{ok_count} ok, {warn_count} warn, {error_count} error"),
        theme.accent.clone(),
    );
    content.append("\n\n");

    for check in &report.checks {
        let (label, style) = match check.status {
            CheckStatus::Ok => ("[OK]", theme.success.clone()),
            CheckStatus::Warn => ("[WARN]", theme.warning.clone()),
            CheckStatus::Error => ("[ERROR]", theme.error.clone()),
        };

        content.append_styled(label, style);
        content.append(" ");
        content.append_styled(&check.name, theme.issue_title.clone());
        if let Some(message) = &check.message {
            content.append_styled(": ", theme.dimmed.clone());
            content.append(message);
        }
        content.append("\n");

        if !matches!(check.status, CheckStatus::Ok)
            && let Some(details) = &check.details
            && let Ok(details_text) = serde_json::to_string_pretty(details)
        {
            for line in details_text.lines() {
                content.append_styled("    ", theme.dimmed.clone());
                content.append_styled(line, theme.dimmed.clone());
                content.append("\n");
            }
        }
    }

    let panel = Panel::from_rich_text(&content, ctx.width())
        .title(Text::styled("Doctor", theme.panel_title.clone()))
        .box_style(theme.box_style)
        .border_style(theme.panel_border.clone());

    ctx.render(&panel);
}

fn collect_table_columns(conn: &Connection, table: &str) -> Result<Vec<String>> {
    let rows = conn.query(&format!("PRAGMA table_info({table})"))?;
    let mut columns = Vec::with_capacity(rows.len());
    for row in &rows {
        if let Some(name) = row.get(1).and_then(SqliteValue::as_text) {
            columns.push(name.to_string());
        }
    }
    Ok(columns)
}

#[allow(clippy::too_many_lines)]
fn required_schema_checks(conn: &Connection, checks: &mut Vec<CheckResult>) -> Result<()> {
    let rows = conn
        .query("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'")?;
    let mut tables = Vec::with_capacity(rows.len());
    for row in &rows {
        if let Some(name) = row.get(0).and_then(SqliteValue::as_text) {
            tables.push(name.to_string());
        }
    }

    let required_tables = [
        "issues",
        "dependencies",
        "labels",
        "comments",
        "events",
        "config",
        "metadata",
        "dirty_issues",
        "export_hashes",
        "blocked_issues_cache",
        "child_counters",
    ];

    // Fallback: if sqlite_master returned nothing (frankensqlite may not
    // support it), probe each required table directly.
    if tables.is_empty() {
        for &table in &required_tables {
            let probe = format!("SELECT 1 FROM {table} LIMIT 1");
            if conn.query(&probe).is_ok() {
                tables.push(table.to_string());
            }
        }
    }

    let missing_tables: Vec<&str> = required_tables
        .iter()
        .copied()
        .filter(|table| !tables.iter().any(|t| t == table))
        .collect();

    if missing_tables.is_empty() {
        push_check(
            checks,
            "schema.tables",
            CheckStatus::Ok,
            None,
            Some(serde_json::json!({ "tables": tables })),
        );
    } else {
        push_check(
            checks,
            "schema.tables",
            CheckStatus::Error,
            Some(format!("Missing tables: {}", missing_tables.join(", "))),
            Some(serde_json::json!({ "missing": missing_tables })),
        );
    }

    let required_columns: &[(&str, &[&str])] = &[
        (
            "issues",
            &[
                "id",
                "title",
                "status",
                "priority",
                "issue_type",
                "created_at",
                "updated_at",
            ],
        ),
        (
            "dependencies",
            &["issue_id", "depends_on_id", "type", "created_at"],
        ),
        (
            "comments",
            &["id", "issue_id", "author", "text", "created_at"],
        ),
        (
            "events",
            &["id", "issue_id", "event_type", "actor", "created_at"],
        ),
    ];

    let mut missing_columns = Vec::new();
    for (table, cols) in required_columns {
        let present = collect_table_columns(conn, table)?;
        let missing: Vec<&str> = cols
            .iter()
            .copied()
            .filter(|col| !present.iter().any(|p| p == col))
            .collect();
        if !missing.is_empty() {
            missing_columns.push(serde_json::json!({
                "table": table,
                "missing": missing,
            }));
        }
    }

    if missing_columns.is_empty() {
        push_check(checks, "schema.columns", CheckStatus::Ok, None, None);
    } else {
        push_check(
            checks,
            "schema.columns",
            CheckStatus::Error,
            Some("Missing required columns".to_string()),
            Some(serde_json::json!({ "tables": missing_columns })),
        );
    }

    Ok(())
}

/// Return true if all integrity check messages are benign frankensqlite artifacts
/// (either "never used" pages, partial-index row mismatches, DESC index ordering
/// differences, or a mix of these).
fn integrity_messages_only_benign(messages: &[String]) -> bool {
    if messages.is_empty() {
        return false;
    }
    let has_benign = messages.iter().any(|msg| {
        let lower = msg.to_lowercase();
        lower.contains("never used")
            || lower.contains("missing from index")
            || lower.contains("out of order")
    });
    if !has_benign {
        return false;
    }
    messages.iter().all(|msg| {
        let lower = msg.to_lowercase();
        lower.contains("never used")
            || lower.contains("missing from index")
            || lower.contains("out of order")
            || lower.contains("*** in database")
    })
}

fn check_integrity(conn: &Connection, checks: &mut Vec<CheckResult>) {
    let rows = match conn.query("PRAGMA integrity_check") {
        Ok(rows) => rows,
        Err(err) => {
            push_check(
                checks,
                "sqlite.integrity_check",
                CheckStatus::Error,
                Some(err.to_string()),
                None,
            );
            return;
        }
    };

    let row_values: Vec<Vec<SqliteValue>> = rows.iter().map(|row| row.values().to_vec()).collect();
    let messages = integrity_check_messages(&row_values);
    if messages.len() == 1 && messages[0].trim().eq_ignore_ascii_case("ok") {
        push_check(
            checks,
            "sqlite.integrity_check",
            CheckStatus::Ok,
            None,
            None,
        );
    } else if integrity_messages_only_benign(&messages) {
        // Unused-page notices and partial-index row mismatches are known frankensqlite
        // artifacts.  The data is intact (write probe passes), so report Warn rather
        // than Error.
        push_check(
            checks,
            "sqlite.integrity_check",
            CheckStatus::Warn,
            Some(messages.join("; ")),
            (messages.len() > 1).then(|| serde_json::json!({ "messages": messages })),
        );
    } else {
        push_check(
            checks,
            "sqlite.integrity_check",
            CheckStatus::Error,
            Some(messages.join("; ")),
            (messages.len() > 1).then(|| serde_json::json!({ "messages": messages })),
        );
    }
}

fn latest_metadata_value(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row_with_params(
        "SELECT value FROM metadata WHERE key = ? ORDER BY rowid DESC LIMIT 1",
        &[SqliteValue::from(key)],
    )
    .ok()
    .and_then(|row| {
        row.get(0)
            .and_then(SqliteValue::as_text)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

/// Pass-4 cycle 4 — detector for `fm-caches_indexes-export-hash-cache-divergence`.
///
/// Compares the value of `metadata.jsonl_content_hash` (the cached
/// top-level JSONL fingerprint) against the current
/// `compute_jsonl_hash(jsonl_path)`. The JSONL on disk is authoritative;
/// the cache is derived. A mismatch means a `br sync` write completed
/// but did not finalize the cache update (or an external edit landed
/// without notifying the cache).
///
/// Narrow scope: this check only flags the TOP-LEVEL hash row. Per-issue
/// `export_hashes` divergence is handled by the existing JSONL→DB
/// rebuild path (`repair_database_from_jsonl`). The pass-1 spec
/// envisioned both surfaces; pass-4 cycle 4 lands just the simpler one
/// because the rebuild path already covers per-issue cases.
fn check_export_hash_cache_divergence(
    conn: &Connection,
    jsonl_path: Option<&Path>,
    checks: &mut Vec<CheckResult>,
) {
    let Some(jsonl) = jsonl_path else {
        push_check(checks, "db.export_hash_cache", CheckStatus::Ok, None, None);
        return;
    };
    if !jsonl.is_file() {
        push_check(checks, "db.export_hash_cache", CheckStatus::Ok, None, None);
        return;
    }
    let Some(stored) = latest_metadata_value(conn, "jsonl_content_hash") else {
        // No cached row at all (or only the empty default sentinel):
        // first sync has not populated the hash yet, so avoid hashing
        // the JSONL just to report OK.
        push_check(checks, "db.export_hash_cache", CheckStatus::Ok, None, None);
        return;
    };
    let Ok(computed) = crate::sync::compute_jsonl_hash(jsonl) else {
        // JSONL unreadable: a different FM owns this surface.
        push_check(checks, "db.export_hash_cache", CheckStatus::Ok, None, None);
        return;
    };
    if stored == computed {
        push_check(checks, "db.export_hash_cache", CheckStatus::Ok, None, None);
    } else {
        push_check(
            checks,
            "db.export_hash_cache",
            CheckStatus::Warn,
            Some("Top-level JSONL content hash in `metadata` differs from computed hash. Cache is stale; doctor --repair will recompute.".to_string()),
            Some(serde_json::json!({
                "stored_top_hash": stored,
                "computed_top_hash": computed,
            })),
        );
    }
}

/// Pass-5 cycle 7: DB-aware detector for the "missing-post-flush" subset
/// of `fm-state_files-base-jsonl-missing-or-stale`.
///
/// The file-based `check_base_jsonl` (in collect_doctor_report) can't
/// distinguish "missing on a fresh clone, no merges yet" from
/// "missing AFTER a sync flush has run". The former is legitimate;
/// the latter means the merge anchor was lost. This check reads
/// `metadata.last_export_time` from the snapshot connection and emits
/// Warn if that value is set AND `.beads/beads.base.jsonl` doesn't
/// exist on disk.
///
/// Issue #378 refinement: a missing anchor is only a real finding when the
/// workspace has drifted. When the database and the live JSONL are
/// verifiably in sync (stored `jsonl_content_hash` matches the computed
/// hash and no issues are marked dirty) — the exact evidence
/// `br sync --status` uses to print "In sync" — the anchor is fully
/// derivable from the live JSONL and the next `br sync --flush-only` (or
/// merge) rewrites it, so this check reports Ok instead of contradicting
/// sync's healthy verdict. When the workspace is NOT verifiably in sync,
/// the Warn stays and now names the exact recovery command.
fn check_base_jsonl_missing_post_flush(
    conn: &Connection,
    beads_dir: &Path,
    jsonl_path: Option<&Path>,
    checks: &mut Vec<CheckResult>,
) {
    let anchor = beads_dir.join("beads.base.jsonl");
    // Cheap fast-path: if the anchor exists (any kind: regular,
    // symlink, etc.) the file-based check already handled it.
    if fs::symlink_metadata(&anchor).is_ok() {
        push_check(
            checks,
            "base_jsonl.missing_post_flush",
            CheckStatus::Ok,
            None,
            None,
        );
        return;
    }
    let last_export = latest_metadata_value(conn, "last_export_time");
    match last_export {
        Some(stamp) if !stamp.is_empty() => {
            if workspace_verifiably_in_sync(conn, jsonl_path) {
                // DB and JSONL agree (`br sync --status` would say
                // "In sync"), so nothing is lost: the anchor is derivable
                // from the live JSONL and the next flush/merge recreates it.
                push_check(
                    checks,
                    "base_jsonl.missing_post_flush",
                    CheckStatus::Ok,
                    Some(
                        "Merge anchor absent, but database and JSONL are in sync; the next `br sync --flush-only` recreates it"
                            .to_string(),
                    ),
                    Some(serde_json::json!({
                        "anchor": anchor.display().to_string(),
                        "last_export_time": stamp,
                        "kind": "missing_but_in_sync",
                    })),
                );
                return;
            }
            push_check(
                checks,
                "base_jsonl.missing_post_flush",
                CheckStatus::Warn,
                Some(format!(
                    "Merge anchor {} is missing despite metadata.last_export_time={stamp}, and the workspace is not verifiably in sync — run `br sync --flush-only` to reconcile and regenerate it",
                    anchor.display()
                )),
                Some(serde_json::json!({
                    "anchor": anchor.display().to_string(),
                    "last_export_time": stamp,
                    "kind": "missing_post_flush",
                })),
            );
        }
        _ => {
            // No prior export → fresh workspace; missing is legitimate.
            push_check(
                checks,
                "base_jsonl.missing_post_flush",
                CheckStatus::Ok,
                None,
                None,
            );
        }
    }
}

/// Whether the workspace passes the same in-sync evidence that
/// `br sync --status` uses for its "In sync" verdict: the live JSONL exists,
/// its computed content hash matches the cached `metadata.jsonl_content_hash`,
/// and no issues are marked dirty (pending re-export). Conservative on any
/// read failure: returns `false` so callers keep warning rather than
/// suppressing a real finding (issue #378).
fn workspace_verifiably_in_sync(conn: &Connection, jsonl_path: Option<&Path>) -> bool {
    let Some(jsonl) = jsonl_path else {
        return false;
    };
    if !jsonl.is_file() {
        return false;
    }
    let Some(stored) = latest_metadata_value(conn, "jsonl_content_hash") else {
        return false;
    };
    let Ok(computed) = crate::sync::compute_jsonl_hash(jsonl) else {
        return false;
    };
    if stored != computed {
        return false;
    }
    let Ok(rows) = conn.query("SELECT COUNT(*) FROM dirty_issues") else {
        return false;
    };
    let dirty_count = rows
        .first()
        .and_then(|row| row.values().first().cloned())
        .and_then(|v| match v {
            SqliteValue::Integer(n) => Some(n),
            _ => None,
        })
        .unwrap_or(i64::MAX);
    dirty_count == 0
}

/// Pass-5 cycle 20: detector for
/// `fm-state_files-jsonl-crlf-line-endings`.
///
/// Warns when the selected JSONL export contains CRLF (`\r\n`) line
/// endings. Operators sometimes import workspaces from Windows
/// terminals or git autocrlf-configured repos that rewrite line
/// endings; CRLF breaks `git diff --no-index` legibility and
/// confuses streaming JSONL parsers that split on `\n`.
///
/// Auto-fixable when the selected JSONL export is inside the workspace.
/// The doctor scans the first 64KB only to keep the check cheap on large
/// workspaces.
const CRLF_SCAN_PREFIX_BYTES: usize = 64 * 1024;

/// Pass-5 cycle 23: detector for
/// `fm-state_files-wal-oversized`.
///
/// Warns when the selected SQLite database WAL exceeds `WAL_OVERSIZED_BYTES`
/// (32MB). A healthy WAL is reset by SQLite's auto-checkpoint at
/// ~4MB; a much larger WAL means checkpoint hasn't run (long-running
/// read snapshot blocking, or a peer process holding the WAL open).
/// Detect-only — operators can run `PRAGMA wal_checkpoint(TRUNCATE)`
/// manually, or `br doctor --repair-indexes` (which checkpoints as
/// a side effect).
const WAL_OVERSIZED_BYTES: u64 = 32 * 1024 * 1024;

fn sqlite_wal_sidecar_path(db_path: &Path) -> PathBuf {
    let mut sidecar = db_path.as_os_str().to_os_string();
    sidecar.push("-wal");
    PathBuf::from(sidecar)
}

fn sqlite_journal_sidecar_path(db_path: &Path) -> PathBuf {
    let mut sidecar = db_path.as_os_str().to_os_string();
    sidecar.push("-journal");
    PathBuf::from(sidecar)
}

/// Whether `db_path` begins with the SQLite file header magic.
///
/// Used to tell "the database is fine but a sidecar blocks the open" apart
/// from "this file was never a database", which need different repairs.
fn db_file_has_sqlite_header(db_path: &Path) -> bool {
    use std::io::Read;
    let Ok(mut file) = fs::File::open(db_path) else {
        return false;
    };
    let mut header = [0u8; 16];
    file.read_exact(&mut header).is_ok() && &header == b"SQLite format 3\0"
}

/// Sidecars fsqlite maintains for its multi-process namespace admission
/// (`-fsqlite-ns-gate`, `-fsqlite-ns-use`). They belong to the database file
/// family and must be carried through backup/quarantine alongside the
/// classic `-wal`/`-shm`/`-journal` trio.
///
/// The suffix list is owned by `config` so this walk and the compaction
/// cleanup cannot drift apart.
fn fsqlite_namespace_sidecar_paths(db_path: &Path) -> Vec<PathBuf> {
    crate::config::FSQLITE_NAMESPACE_SIDECAR_SUFFIXES
        .iter()
        .map(|suffix| {
            let mut sidecar = db_path.as_os_str().to_os_string();
            sidecar.push(suffix);
            PathBuf::from(sidecar)
        })
        .collect()
}

/// The parallel-WAL durability-certificate sidecars (`-wal-cert`,
/// `-wal-cert-head`) fsqlite 0.2+ retains next to the classic `-wal` file.
fn fsqlite_wal_cert_sidecar_paths(db_path: &Path) -> Vec<PathBuf> {
    crate::config::FSQLITE_WAL_CERT_SIDECAR_SUFFIXES
        .iter()
        .map(|suffix| {
            let mut sidecar = db_path.as_os_str().to_os_string();
            sidecar.push(suffix);
            PathBuf::from(sidecar)
        })
        .collect()
}

fn existing_sqlite_family_paths_for_legacy_op(db_path: &Path) -> Vec<PathBuf> {
    let mut paths = vec![db_path.to_path_buf()];
    // fsqlite 0.3.6+ engine-upgrade bookkeeping lives beside the DB and
    // belongs to the same mutation-audit family.
    let migration_state = {
        let mut sidecar = db_path.as_os_str().to_os_string();
        sidecar.push(".fsqlite-migration-state");
        PathBuf::from(sidecar)
    };
    let sidecars = [
        sqlite_wal_sidecar_path(db_path),
        sqlite_shm_sidecar_path(db_path),
        sqlite_journal_sidecar_path(db_path),
        migration_state,
    ]
    .into_iter()
    .chain(fsqlite_namespace_sidecar_paths(db_path))
    .chain(fsqlite_wal_cert_sidecar_paths(db_path));
    for sidecar in sidecars {
        if fs::symlink_metadata(&sidecar)
            .is_ok_and(|metadata| metadata.is_file() && !metadata.file_type().is_symlink())
        {
            paths.push(sidecar);
        }
    }
    paths
}

fn check_wal_oversized(db_path: &Path, checks: &mut Vec<CheckResult>) {
    let path = sqlite_wal_sidecar_path(db_path);
    let Ok(meta) = fs::symlink_metadata(&path) else {
        push_check(checks, "wal_size", CheckStatus::Ok, None, None);
        return;
    };
    if !meta.is_file() || meta.file_type().is_symlink() {
        push_check(checks, "wal_size", CheckStatus::Ok, None, None);
        return;
    }
    let bytes = meta.len();
    if bytes > WAL_OVERSIZED_BYTES {
        push_check(
            checks,
            "wal_size",
            CheckStatus::Warn,
            Some(format!(
                "{} is {}MB (>{}MB threshold); SQLite auto-checkpoint may be blocked by a long-running read snapshot",
                path.display(),
                bytes / (1024 * 1024),
                WAL_OVERSIZED_BYTES / (1024 * 1024)
            )),
            Some(serde_json::json!({
                "path": path.display().to_string(),
                "size_bytes": bytes,
                "threshold_bytes": WAL_OVERSIZED_BYTES,
                "remediation": "Run `br doctor --repair --only fm-state_files-wal-oversized` or manually run `PRAGMA wal_checkpoint(TRUNCATE)` against the selected SQLite database",
            })),
        );
    } else {
        push_check(checks, "wal_size", CheckStatus::Ok, None, None);
    }
}

/// Pass-5 cycle 37 — fixer for
/// `fm-state_files-wal-oversized`.
///
/// Runs `PRAGMA wal_checkpoint(TRUNCATE)` against the selected SQLite
/// database via the legacy chokepoint wrapper. Checkpoint pragmas must
/// run outside an explicit transaction, so this cannot use `Op::DbExec`
/// (which wraps SQL in `BEGIN IMMEDIATE`). The legacy wrapper still
/// snapshots the database plus WAL/SHM sidecars before running the
/// pragma and records `actions.jsonl` entries for undo/forensics.
///
/// The pragma forces WAL pages to be written to the main DB file and
/// then truncates the WAL sidecar to zero bytes. This is a
/// data-equivalent operation — the database's logical state is
/// unchanged; only how it's stored shifts.
///
/// TRUNCATE was chosen over PASSIVE/RESTART because the FM is "WAL
/// grew unboundedly" — a no-op opportunistic checkpoint wouldn't
/// resolve it.
#[cfg(test)]
fn fix_wal_oversized_if_warned(
    db_path: &Path,
    report: &DoctorReport,
    ctx: &OutputContext,
    session: Option<&mut DoctorRepairSession>,
) -> bool {
    let Some(beads_dir) = db_path.parent() else {
        return false;
    };
    let write_authority = match acquire_doctor_database_write_authority(
        beads_dir,
        db_path,
        Some(crate::sync::default_write_lock_timeout_ms()),
    ) {
        Ok(authority) => authority,
        Err(err) => {
            if !ctx.is_json() {
                ctx.warning(&format!(
                    "Skipping wal_checkpoint: database authority unavailable ({err})"
                ));
            }
            return false;
        }
    };
    fix_wal_oversized_if_warned_under_write_authority(
        db_path,
        report,
        ctx,
        session,
        &write_authority,
    )
}

fn fix_wal_oversized_if_warned_under_write_authority(
    db_path: &Path,
    report: &DoctorReport,
    ctx: &OutputContext,
    session: Option<&mut DoctorRepairSession>,
    write_authority: &Arc<crate::sync::DatabaseFamilyWriteLock>,
) -> bool {
    let has_warning = report
        .checks
        .iter()
        .any(|c| c.name == "wal_size" && c.status == CheckStatus::Warn);
    if !has_warning {
        return false;
    }
    let Some(session) = session else {
        if !ctx.is_json() {
            ctx.warning(
                "Skipping wal_checkpoint: no doctor repair session (run-dir creation failed)",
            );
        }
        return false;
    };
    let wal_path = sqlite_wal_sidecar_path(db_path);
    let shm_path = sqlite_shm_sidecar_path(db_path);
    match session.record_legacy_mutation(
        "doctor.wal_checkpoint_truncate",
        &[db_path, &wal_path, &shm_path],
        || checkpoint_wal_truncate(db_path, write_authority),
    ) {
        Ok(()) => {
            if !ctx.is_json() {
                ctx.info("Checkpointed and truncated SQLite WAL");
            }
            true
        }
        Err(err) => {
            if !ctx.is_json() {
                ctx.warning(&format!("Failed to checkpoint WAL: {err}"));
            }
            false
        }
    }
}

fn sqlite_shm_sidecar_path(db_path: &Path) -> PathBuf {
    let mut sidecar = db_path.as_os_str().to_os_string();
    sidecar.push("-shm");
    PathBuf::from(sidecar)
}

fn checkpoint_wal_truncate(
    db_path: &Path,
    write_authority: &Arc<crate::sync::DatabaseFamilyWriteLock>,
) -> Result<()> {
    write_authority.verify_database_authority()?;
    let conn = Connection::open(db_path.to_string_lossy().into_owned())?;
    let checkpoint_complete = match wal_checkpoint_truncate_complete(&conn) {
        Ok(complete) => complete,
        Err(err) => {
            let _ = conn.close();
            return Err(err.into());
        }
    };
    conn.close()?;
    write_authority.verify_database_authority()?;
    if !checkpoint_complete {
        return Err(BeadsError::internal(
            "doctor: WAL checkpoint did not complete; a reader may still hold a snapshot",
        ));
    }

    let wal_path = sqlite_wal_sidecar_path(db_path);
    truncate_oversized_regular_wal_if_needed(&wal_path)?;
    write_authority.verify_database_authority()?;

    Ok(())
}

fn truncate_oversized_regular_wal_if_needed(wal_path: &Path) -> Result<()> {
    if let Ok(meta) = fs::symlink_metadata(wal_path)
        && meta.is_file()
        && !meta.file_type().is_symlink()
        && meta.len() > WAL_OVERSIZED_BYTES
    {
        OpenOptions::new().write(true).open(wal_path)?.set_len(0)?;
    }
    Ok(())
}

/// Pass-10 cycle 62 — `--unsafe-auto-fix`-gated fixer for
/// `fm-caches_indexes-db-bloat-vs-jsonl`.
///
/// When the operator passes `--unsafe-auto-fix` AND `db_bloat` warns,
/// runs `VACUUM` against the database via the legacy chokepoint wrapper.
/// `VACUUM` is normally detect-only because it rewrites every page in the
/// DB (slow on large databases) and operators typically prefer to time
/// these themselves — but it is data-equivalent and reversible via the
/// chokepoint's verbatim DB+WAL+SHM snapshot, so an opt-in is safe.
///
/// Distinct from the existing `repair_via_vacuum` legacy path: that path
/// fires on detected page CORRUPTION (Error level) and runs
/// `VACUUM` + `VACUUM INTO`. This fixer fires on BLOAT (Warn level) and
/// runs just `VACUUM` — enough to reclaim freelist space without the
/// compaction-copy round trip.
///
/// Like `fix_wal_oversized_if_warned`, this uses `record_legacy_mutation`
/// (NOT `Op::DbExec`) because `VACUUM` cannot run inside a transaction.
#[cfg(test)]
#[allow(dead_code)] // Retains the standalone harness entry point for future bloat fixtures.
fn fix_db_bloat_via_vacuum_if_warned(
    db_path: &Path,
    report: &DoctorReport,
    ctx: &OutputContext,
    session: Option<&mut DoctorRepairSession>,
) -> bool {
    let Some(beads_dir) = db_path.parent() else {
        return false;
    };
    let write_authority = match acquire_doctor_database_write_authority(
        beads_dir,
        db_path,
        Some(crate::sync::default_write_lock_timeout_ms()),
    ) {
        Ok(authority) => authority,
        Err(err) => {
            if !ctx.is_json() {
                ctx.warning(&format!(
                    "Skipping db-bloat VACUUM: database authority unavailable ({err})"
                ));
            }
            return false;
        }
    };
    fix_db_bloat_via_vacuum_if_warned_under_write_authority(
        db_path,
        report,
        ctx,
        session,
        &write_authority,
    )
}

fn fix_db_bloat_via_vacuum_if_warned_under_write_authority(
    db_path: &Path,
    report: &DoctorReport,
    ctx: &OutputContext,
    session: Option<&mut DoctorRepairSession>,
    write_authority: &Arc<crate::sync::DatabaseFamilyWriteLock>,
) -> bool {
    let has_warning = report
        .checks
        .iter()
        .any(|c| c.name == "db_bloat" && c.status == CheckStatus::Warn);
    if !has_warning {
        return false;
    }
    let Some(session) = session else {
        if !ctx.is_json() {
            ctx.warning(
                "Skipping db-bloat VACUUM: no doctor repair session (run-dir creation failed)",
            );
        }
        return false;
    };
    let wal_path = sqlite_wal_sidecar_path(db_path);
    let shm_path = sqlite_shm_sidecar_path(db_path);
    match session.record_legacy_mutation(
        "doctor.db_bloat_vacuum",
        &[db_path, &wal_path, &shm_path],
        || vacuum_database(db_path, write_authority),
    ) {
        Ok(()) => {
            if !ctx.is_json() {
                ctx.info(
                    "Compacted database via VACUUM (--unsafe-auto-fix opt-in, bloat-triggered)",
                );
            }
            true
        }
        Err(err) => {
            // `beads_rust-ymik`: log unconditionally. `ctx.warning` is
            // suppressed in JSON mode, so a VACUUM the operator explicitly
            // opted into with `--unsafe-auto-fix` could fail and leave no
            // trace anywhere in the payload — after which the repair reports
            // "No errors detected; nothing to repair." for a finding it had
            // just been pointed at by name.
            tracing::warn!(
                error = %err,
                db_path = %db_path.display(),
                "db-bloat VACUUM failed; database left uncompacted"
            );
            if !ctx.is_json() {
                ctx.warning(&format!("Failed to VACUUM database: {err}"));
            }
            false
        }
    }
}

/// Compact the database via `VACUUM INTO` + atomic rename. Used by
/// [`fix_db_bloat_via_vacuum_if_warned`] under the chokepoint's
/// legacy-mutation wrapper so the DB family is snapshotted for undo.
///
/// `beads_rust-ote7`: this used to run an in-place `VACUUM`, which the engine
/// refuses on any database whose header page count disagrees with its file
/// length — "database image header page count 360 does not match file length
/// page count 4968" — i.e. exactly the bloated databases this fixer exists to
/// compact. The failure was reported as "No errors detected; nothing to
/// repair."
///
/// In-place `VACUUM` only ever succeeded on such a file when some earlier
/// command had already committed a write, because a commit makes the engine
/// publish the *file-length-derived* page count into the header, absorbing any
/// trailing bytes as database pages (see
/// `docs/fsqlite_trailing_pages_report.md`). Depending on that is not a
/// behavior worth preserving: it means the repair only worked after the
/// database had already been silently corrupted.
///
/// `VACUUM INTO` needs no such precondition. It writes a fresh image from the
/// logical page set, so trailing slack is simply not carried over, the product
/// verifies `ok` under both the engine and the system `sqlite3`, and the
/// original is untouched until the atomic rename. Because that rename
/// replaces the database file, the inode-bound database-family authority is
/// re-bound via `rebind_database_inode_after_authorized_replace` rather than
/// re-verified against the stale binding.
fn vacuum_database(
    db_path: &Path,
    write_authority: &Arc<crate::sync::DatabaseFamilyWriteLock>,
) -> Result<()> {
    let storage = open_doctor_storage_under_write_authority(db_path, write_authority)?;
    config::compact_database_via_vacuum_into_in_place(storage, db_path, None)?;
    write_authority.rebind_database_inode_after_authorized_replace()?;
    Ok(())
}

/// Pass-5 cycle 22: detector for
/// `fm-caches_indexes-db-bloat-vs-jsonl`.
///
/// Warns when the selected SQLite database exceeds `DB_BLOAT_RATIO_THRESHOLD`
/// times the size of the selected JSONL export. SQLite retains freelist
/// pages after DELETE operations; a high ratio is a strong signal
/// that VACUUM would reclaim significant disk space.
const DB_BLOAT_RATIO_THRESHOLD: u64 = 10;
const DB_BLOAT_MIN_JSONL_BYTES: u64 = 1024 * 1024;

fn check_db_bloat_vs_jsonl(
    db_path: &Path,
    jsonl_path: Option<&Path>,
    checks: &mut Vec<CheckResult>,
) {
    let Some(jsonl_path) = jsonl_path else {
        push_check(checks, "db_bloat", CheckStatus::Ok, None, None);
        return;
    };
    let (Ok(db_meta), Ok(jsonl_meta)) = (
        fs::symlink_metadata(db_path),
        fs::symlink_metadata(jsonl_path),
    ) else {
        push_check(checks, "db_bloat", CheckStatus::Ok, None, None);
        return;
    };
    if !db_meta.is_file() || !jsonl_meta.is_file() {
        push_check(checks, "db_bloat", CheckStatus::Ok, None, None);
        return;
    }
    let db_bytes = db_meta.len();
    let jsonl_bytes = jsonl_meta.len();
    if jsonl_bytes < DB_BLOAT_MIN_JSONL_BYTES {
        push_check(checks, "db_bloat", CheckStatus::Ok, None, None);
        return;
    }
    if db_bytes > jsonl_bytes.saturating_mul(DB_BLOAT_RATIO_THRESHOLD) {
        push_check(
            checks,
            "db_bloat",
            CheckStatus::Warn,
            Some(format!(
                "{} is {}x the size of {} ({}MB vs {}MB); VACUUM would likely reclaim significant space",
                db_path.display(),
                db_bytes / jsonl_bytes,
                jsonl_path.display(),
                db_bytes / (1024 * 1024),
                jsonl_bytes / (1024 * 1024)
            )),
            Some(serde_json::json!({
                "db_path": db_path.display().to_string(),
                "jsonl_path": jsonl_path.display().to_string(),
                "db_bytes": db_bytes,
                "jsonl_bytes": jsonl_bytes,
                "ratio": db_bytes / jsonl_bytes,
                "threshold": DB_BLOAT_RATIO_THRESHOLD,
                "remediation": "Run `VACUUM` against the selected SQLite database or run `br doctor --repair` if integrity warnings are present",
            })),
        );
    } else {
        push_check(checks, "db_bloat", CheckStatus::Ok, None, None);
    }
}

/// Pass-5 cycle 21: detector for
/// `fm-state_files-jsonl-utf8-bom-prefix`.
///
/// Warns when the selected JSONL export starts with the UTF-8 BOM
/// (`0xEF 0xBB 0xBF`). Some Windows editors (older Visual Studio,
/// classic Notepad) prepend it; most JSONL parsers don't strip it,
/// so the first record fails to parse with a phantom prefix.
/// Auto-fixable: rewrite the file without the leading 3 bytes via
/// `Op::WriteFile` through the chokepoint.
const UTF8_BOM: &[u8] = &[0xEF, 0xBB, 0xBF];

fn path_is_inside_workspace(path: &Path, repo_root: &Path) -> bool {
    // Repair fixers widen write_scopes for selected JSONL paths, so the
    // workspace check must resolve `..` components and symlinks first.
    let (Ok(path), Ok(repo_root)) = (fs::canonicalize(path), fs::canonicalize(repo_root)) else {
        return false;
    };
    path.starts_with(repo_root)
}

fn check_jsonl_utf8_bom(jsonl_path: Option<&Path>, checks: &mut Vec<CheckResult>) {
    use std::io::Read;
    let Some(path) = jsonl_path else {
        push_check(checks, "jsonl_bom", CheckStatus::Ok, None, None);
        return;
    };
    let Ok(meta) = fs::symlink_metadata(path) else {
        push_check(checks, "jsonl_bom", CheckStatus::Ok, None, None);
        return;
    };
    if meta.file_type().is_symlink() || !meta.is_file() || meta.len() < 3 {
        push_check(checks, "jsonl_bom", CheckStatus::Ok, None, None);
        return;
    }
    let Ok(mut f) = fs::File::open(path) else {
        push_check(checks, "jsonl_bom", CheckStatus::Ok, None, None);
        return;
    };
    let mut head = [0u8; 3];
    if f.read_exact(&mut head).is_err() {
        push_check(checks, "jsonl_bom", CheckStatus::Ok, None, None);
        return;
    }
    if head == UTF8_BOM {
        push_check(
            checks,
            "jsonl_bom",
            CheckStatus::Warn,
            Some(format!(
                "{} starts with a UTF-8 BOM (0xEF 0xBB 0xBF); JSONL parsers will fail on the first record",
                path.display()
            )),
            Some(serde_json::json!({
                "path": path.display().to_string(),
                "remediation": "`br doctor --repair` strips the BOM when the selected JSONL is inside the workspace; otherwise strip it manually",
            })),
        );
    } else {
        push_check(checks, "jsonl_bom", CheckStatus::Ok, None, None);
    }
}

/// Pass-5 cycle 21 — fixer for
/// `fm-state_files-jsonl-utf8-bom-prefix`.
///
/// Rewrites the selected JSONL export without the leading 3 bytes via
/// [`chokepoint::mutate(Op::WriteFile)`]. The chokepoint captures
/// pre-rewrite bytes in a verbatim backup so `doctor undo`
/// byte-restores the original BOM-prefixed state.
fn fix_jsonl_utf8_bom_if_warned(
    jsonl_path: Option<&Path>,
    report: &DoctorReport,
    ctx: &OutputContext,
    session: Option<&mut DoctorRepairSession>,
) -> bool {
    let has_warning = report
        .checks
        .iter()
        .any(|c| c.name == "jsonl_bom" && c.status == CheckStatus::Warn);
    if !has_warning {
        return false;
    }
    let Some(session) = session else {
        if !ctx.is_json() {
            ctx.warning("Skipping BOM strip: no doctor repair session (run-dir creation failed)");
        }
        return false;
    };
    let Some(path) = jsonl_path else {
        return false;
    };
    // TOCTOU defense: re-read at fix time.
    let Ok(bytes) = fs::read(path) else {
        return false;
    };
    if !bytes.starts_with(UTF8_BOM) {
        return false;
    }
    if !path_is_inside_workspace(path, &session.ctx.repo_root) {
        if !ctx.is_json() {
            ctx.warning(&format!(
                "Skipping BOM strip for external JSONL outside workspace: {}",
                path.display()
            ));
        }
        return false;
    }
    let stripped = bytes[UTF8_BOM.len()..].to_vec();
    session.set_fixer("doctor.jsonl_bom_strip");
    session
        .ctx
        .capabilities
        .write_scopes
        .push(path.to_path_buf());
    match chokepoint::mutate(
        &session.ctx,
        path,
        Op::WriteFile {
            content: stripped,
            mode: None,
        },
    ) {
        Ok(result) if result.ok => {
            if !ctx.is_json() {
                ctx.info(&format!("Stripped UTF-8 BOM from {}", path.display()));
            }
            true
        }
        Ok(_) => false,
        Err(err) => {
            if !ctx.is_json() {
                ctx.warning(&format!(
                    "Failed to strip BOM from {}: {err}",
                    path.display()
                ));
            }
            false
        }
    }
}

/// Pass-5 cycle 24 — fixer for
/// `fm-state_files-jsonl-crlf-line-endings`.
///
/// Converts CRLF (`\r\n`) sequences to LF (`\n`) in the selected JSONL
/// export via [`chokepoint::mutate(Op::WriteFile)`].
/// Standalone `\r` bytes are preserved — only the specific CRLF
/// pattern the detector flags is normalised. The chokepoint captures
/// pre-rewrite bytes in a verbatim backup so `doctor undo`
/// byte-restores the original CRLF state.
fn fix_jsonl_crlf_endings_if_warned(
    jsonl_path: Option<&Path>,
    report: &DoctorReport,
    ctx: &OutputContext,
    session: Option<&mut DoctorRepairSession>,
) -> bool {
    let has_warning = report
        .checks
        .iter()
        .any(|c| c.name == "jsonl_crlf" && c.status == CheckStatus::Warn);
    if !has_warning {
        return false;
    }
    let Some(session) = session else {
        if !ctx.is_json() {
            ctx.warning(
                "Skipping CRLF→LF conversion: no doctor repair session (run-dir creation failed)",
            );
        }
        return false;
    };
    let Some(path) = jsonl_path else {
        return false;
    };
    let Ok(bytes) = fs::read(path) else {
        return false;
    };
    if !bytes.windows(2).any(|w| w == b"\r\n") {
        return false;
    }
    if !path_is_inside_workspace(path, &session.ctx.repo_root) {
        if !ctx.is_json() {
            ctx.warning(&format!(
                "Skipping CRLF→LF conversion for external JSONL outside workspace: {}",
                path.display()
            ));
        }
        return false;
    }
    let mut converted = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'\r' && bytes[i + 1] == b'\n' {
            converted.push(b'\n');
            i += 2;
        } else {
            converted.push(bytes[i]);
            i += 1;
        }
    }
    session.set_fixer("doctor.jsonl_crlf_to_lf");
    session
        .ctx
        .capabilities
        .write_scopes
        .push(path.to_path_buf());
    match chokepoint::mutate(
        &session.ctx,
        path,
        Op::WriteFile {
            content: converted,
            mode: None,
        },
    ) {
        Ok(result) if result.ok => {
            if !ctx.is_json() {
                ctx.info(&format!("Converted CRLF→LF in {}", path.display()));
            }
            true
        }
        Ok(_) => false,
        Err(err) => {
            if !ctx.is_json() {
                ctx.warning(&format!(
                    "Failed to convert CRLF→LF in {}: {err}",
                    path.display()
                ));
            }
            false
        }
    }
}

fn check_jsonl_crlf_endings(jsonl_path: Option<&Path>, checks: &mut Vec<CheckResult>) {
    use std::io::Read;
    let Some(path) = jsonl_path else {
        push_check(checks, "jsonl_crlf", CheckStatus::Ok, None, None);
        return;
    };
    let Ok(meta) = fs::symlink_metadata(path) else {
        push_check(checks, "jsonl_crlf", CheckStatus::Ok, None, None);
        return;
    };
    if meta.file_type().is_symlink() || !meta.is_file() || meta.len() == 0 {
        push_check(checks, "jsonl_crlf", CheckStatus::Ok, None, None);
        return;
    }
    let Ok(mut f) = fs::File::open(path) else {
        push_check(checks, "jsonl_crlf", CheckStatus::Ok, None, None);
        return;
    };
    let len_clamped = usize::try_from(meta.len()).unwrap_or(CRLF_SCAN_PREFIX_BYTES);
    let mut buf = vec![0u8; CRLF_SCAN_PREFIX_BYTES.min(len_clamped)];
    let Ok(n) = f.read(&mut buf) else {
        push_check(checks, "jsonl_crlf", CheckStatus::Ok, None, None);
        return;
    };
    let has_crlf = buf[..n].windows(2).any(|w| w == b"\r\n");
    if has_crlf {
        push_check(
            checks,
            "jsonl_crlf",
            CheckStatus::Warn,
            Some(format!(
                "{} contains CRLF line endings (scanned first {} bytes); git diff and streaming JSONL parsers may misbehave",
                path.display(),
                n
            )),
            Some(serde_json::json!({
                "path": path.display().to_string(),
                "scanned_bytes": n,
                "remediation": "Convert line endings to LF for the selected JSONL export",
            })),
        );
    } else {
        push_check(checks, "jsonl_crlf", CheckStatus::Ok, None, None);
    }
}

/// Pass-5 cycle 19: detector for
/// `fm-state_files-jsonl-missing-trailing-newline`.
///
/// Warns when the selected JSONL export exists, is non-empty, and its
/// last byte is not `\n`. POSIX text files should end with a newline;
/// missing one causes `grep`, `jq -s`, `sed`, `wc -l`, and most
/// line-oriented tools to silently skip or miscount the last record.
/// Auto-fixable via `fix_jsonl_trailing_newline_if_warned` below.
fn check_jsonl_trailing_newline(jsonl_path: Option<&Path>, checks: &mut Vec<CheckResult>) {
    use std::io::{Read, Seek, SeekFrom};
    let Some(path) = jsonl_path else {
        push_check(checks, "jsonl_eof_newline", CheckStatus::Ok, None, None);
        return;
    };
    let Ok(meta) = fs::symlink_metadata(path) else {
        push_check(checks, "jsonl_eof_newline", CheckStatus::Ok, None, None);
        return;
    };
    if meta.file_type().is_symlink() || !meta.is_file() || meta.len() == 0 {
        push_check(checks, "jsonl_eof_newline", CheckStatus::Ok, None, None);
        return;
    }
    let Ok(mut f) = fs::File::open(path) else {
        push_check(checks, "jsonl_eof_newline", CheckStatus::Ok, None, None);
        return;
    };
    if f.seek(SeekFrom::End(-1)).is_err() {
        push_check(checks, "jsonl_eof_newline", CheckStatus::Ok, None, None);
        return;
    }
    let mut last = [0u8; 1];
    if f.read_exact(&mut last).is_err() {
        push_check(checks, "jsonl_eof_newline", CheckStatus::Ok, None, None);
        return;
    }
    if last[0] == b'\n' {
        push_check(checks, "jsonl_eof_newline", CheckStatus::Ok, None, None);
    } else {
        push_check(
            checks,
            "jsonl_eof_newline",
            CheckStatus::Warn,
            Some(format!(
                "{} does not end with a newline; line-oriented tools may skip the last record",
                path.display()
            )),
            Some(serde_json::json!({
                "path": path.display().to_string(),
                "last_byte": last[0],
                "remediation": "`br doctor --repair` will append a single newline when the selected JSONL is inside the workspace; otherwise append one manually",
            })),
        );
    }
}

/// Pass-5 cycle 19 — fixer for
/// `fm-state_files-jsonl-missing-trailing-newline`.
///
/// Appends a single `\n` to the selected JSONL export via
/// [`chokepoint::mutate(Op::AppendFile)`] when the detector flagged
/// the missing newline. The chokepoint captures the pre-append bytes
/// in a verbatim backup so `doctor undo` can byte-restore the
/// original (no-newline) state.
fn fix_jsonl_trailing_newline_if_warned(
    jsonl_path: Option<&Path>,
    report: &DoctorReport,
    ctx: &OutputContext,
    session: Option<&mut DoctorRepairSession>,
) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let has_warning = report
        .checks
        .iter()
        .any(|c| c.name == "jsonl_eof_newline" && c.status == CheckStatus::Warn);
    if !has_warning {
        return false;
    }
    let Some(session) = session else {
        if !ctx.is_json() {
            ctx.warning(
                "Skipping jsonl-trailing-newline fix: no doctor repair session (run-dir creation failed)",
            );
        }
        return false;
    };
    let Some(path) = jsonl_path else {
        return false;
    };
    // TOCTOU defense: re-check last byte at fix time.
    let Ok(mut f) = fs::File::open(path) else {
        return false;
    };
    if f.seek(SeekFrom::End(-1)).is_err() {
        return false;
    }
    let mut last = [0u8; 1];
    if f.read_exact(&mut last).is_err() {
        return false;
    }
    drop(f);
    if last[0] == b'\n' {
        return false;
    }
    if !path_is_inside_workspace(path, &session.ctx.repo_root) {
        if !ctx.is_json() {
            ctx.warning(&format!(
                "Skipping jsonl-trailing-newline fix for external JSONL outside workspace: {}",
                path.display()
            ));
        }
        return false;
    }
    session.set_fixer("doctor.jsonl_trailing_newline_append");
    session
        .ctx
        .capabilities
        .write_scopes
        .push(path.to_path_buf());
    match chokepoint::mutate(
        &session.ctx,
        path,
        Op::AppendFile {
            content: b"\n".to_vec(),
        },
    ) {
        Ok(result) if result.ok => {
            if !ctx.is_json() {
                ctx.info(&format!("Appended trailing newline to {}", path.display()));
            }
            true
        }
        Ok(_) => false,
        Err(err) => {
            if !ctx.is_json() {
                ctx.warning(&format!(
                    "Failed to append trailing newline to {}: {err}",
                    path.display()
                ));
            }
            false
        }
    }
}

/// Pass-5 cycle 18: detector for
/// `fm-state_files-br-history-grows-unbounded`.
///
/// Warns when `.beads/.br_history/` accumulates more than
/// `BR_HISTORY_SNAPSHOT_THRESHOLD` snapshot files. The history dir
/// stores point-in-time JSONL snapshots that operators rarely
/// inspect, but they consume inodes and slow directory listing.
///
/// Detect-only — pruning history risks data loss; operator decides
/// whether/how to compact.
const BR_HISTORY_SNAPSHOT_THRESHOLD: usize = 100;

fn check_br_history_size(beads_dir: &Path, checks: &mut Vec<CheckResult>) {
    let history = beads_dir.join(".br_history");
    let backups = match crate::sync::history::list_backups(&history, None) {
        Ok(backups) => backups,
        Err(err) => {
            push_check(
                checks,
                "br_history.size",
                CheckStatus::Warn,
                Some(format!(
                    "Could not inspect history directory {}: {err}",
                    history.display()
                )),
                Some(serde_json::json!({
                    "history_dir": history.display().to_string(),
                    "error": err.to_string(),
                    "remediation": "Inspect .beads/.br_history/ permissions and symlink shape",
                })),
            );
            return;
        }
    };
    let snapshot_count = backups.len();
    if snapshot_count > BR_HISTORY_SNAPSHOT_THRESHOLD {
        push_check(
            checks,
            "br_history.size",
            CheckStatus::Warn,
            Some(format!(
                "{} snapshot file(s) accumulated in {}; consider pruning",
                snapshot_count,
                history.display()
            )),
            Some(serde_json::json!({
                "history_dir": history.display().to_string(),
                "snapshot_count": snapshot_count,
                "threshold": BR_HISTORY_SNAPSHOT_THRESHOLD,
                "remediation": "Review and archive old snapshots; `br history` commands manage selectively",
            })),
        );
    } else {
        push_check(checks, "br_history.size", CheckStatus::Ok, None, None);
    }
}

/// Pass-5 cycle 17: detector for
/// `fm-state_files-jsonl-oversized`.
///
/// Warns when `.beads/issues.jsonl` exceeds
/// `JSONL_OVERSIZED_THRESHOLD_BYTES` (100MB). At that scale the sync
/// engine's full-file read on every flush becomes slow and the
/// in-memory parse can pressure low-RAM hosts. Detect-only —
/// compaction (closing stale issues, archiving old comments) is
/// operator-decided.
const JSONL_OVERSIZED_THRESHOLD_BYTES: u64 = 100 * 1024 * 1024;

fn check_jsonl_oversized(jsonl_path: Option<&Path>, checks: &mut Vec<CheckResult>) {
    let Some(path) = jsonl_path else {
        push_check(checks, "jsonl_size", CheckStatus::Ok, None, None);
        return;
    };
    let Ok(meta) = fs::symlink_metadata(path) else {
        push_check(checks, "jsonl_size", CheckStatus::Ok, None, None);
        return;
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        push_check(checks, "jsonl_size", CheckStatus::Ok, None, None);
        return;
    }
    let bytes = meta.len();
    if bytes > JSONL_OVERSIZED_THRESHOLD_BYTES {
        push_check(
            checks,
            "jsonl_size",
            CheckStatus::Warn,
            Some(format!(
                "{} is {} MB (>{} MB threshold); flushes will be slow and may pressure low-RAM hosts",
                path.display(),
                bytes / (1024 * 1024),
                JSONL_OVERSIZED_THRESHOLD_BYTES / (1024 * 1024)
            )),
            Some(serde_json::json!({
                "path": path.display().to_string(),
                "size_bytes": bytes,
                "threshold_bytes": JSONL_OVERSIZED_THRESHOLD_BYTES,
                "remediation": "Close stale issues, archive old comments, or split the workspace",
            })),
        );
    } else {
        push_check(checks, "jsonl_size", CheckStatus::Ok, None, None);
    }
}

/// Pass-5 cycle 33: detector for
/// `fm-state_files-jsonl-duplicate-ids`.
///
/// Streams the selected JSONL export line by line, extracts the
/// top-level `id` value from each record, and warns when any id
/// appears more than once. Two records with the same id is a clear
/// corruption signal - usually the result of an unresolved merge
/// conflict that left both sides' records or a partial JSONL rebuild
/// that double-appended an issue.
///
/// Detect-only - auto-fix is too risky: deciding which copy is
/// canonical depends on operator intent (timestamp? content hash?
/// dependency graph?). If SQLite is authoritative, operators can
/// regenerate JSONL with `br sync --flush-only`; if JSONL is
/// authoritative, they must manually remove stale records before any
/// import/rebuild.
///
/// Records up to 5 duplicate IDs so the operator can locate
/// them quickly. Malformed lines are left to the existing JSONL parse
/// detector; this check only uses records that parse and expose a
/// string `id`.
fn check_jsonl_duplicate_ids(jsonl_path: Option<&Path>, checks: &mut Vec<CheckResult>) {
    use std::collections::BTreeMap;
    use std::io::{BufRead, BufReader};

    let Some(path) = jsonl_path else {
        push_check(checks, "jsonl.duplicate_ids", CheckStatus::Ok, None, None);
        return;
    };
    let Ok(meta) = fs::symlink_metadata(path) else {
        push_check(checks, "jsonl.duplicate_ids", CheckStatus::Ok, None, None);
        return;
    };
    if meta.file_type().is_symlink() || !meta.is_file() || meta.len() == 0 {
        push_check(checks, "jsonl.duplicate_ids", CheckStatus::Ok, None, None);
        return;
    }
    let Ok(file) = fs::File::open(path) else {
        push_check(checks, "jsonl.duplicate_ids", CheckStatus::Ok, None, None);
        return;
    };
    let mut id_counts = BTreeMap::<String, u64>::new();
    for line in BufReader::new(file)
        .lines()
        .map_while(std::result::Result::ok)
    {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if let Some(id) = record.get("id").and_then(serde_json::Value::as_str)
            && !id.is_empty()
        {
            *id_counts.entry(id.to_string()).or_default() += 1;
        }
    }
    let duplicate_counts: Vec<(&String, u64)> = id_counts
        .iter()
        .filter_map(|(id, count)| (*count > 1).then_some((id, *count)))
        .collect();
    if duplicate_counts.is_empty() {
        push_check(checks, "jsonl.duplicate_ids", CheckStatus::Ok, None, None);
        return;
    }
    let total_dup_records: u64 = duplicate_counts.iter().map(|(_, count)| *count).sum();
    let sample_ids: Vec<&str> = duplicate_counts
        .iter()
        .take(5)
        .map(|(id, _)| id.as_str())
        .collect();
    push_check(
        checks,
        "jsonl.duplicate_ids",
        CheckStatus::Warn,
        Some(format!(
            "{} contains {} duplicate id(s) across {} record(s); usually an unresolved merge artifact",
            path.display(),
            duplicate_counts.len(),
            total_dup_records
        )),
        Some(serde_json::json!({
            "path": path.display().to_string(),
            "distinct_duplicate_ids": duplicate_counts.len(),
            "total_duplicate_records": total_dup_records,
            "sample_duplicate_ids": sample_ids,
            "remediation": "If SQLite is authoritative, run `br sync --flush-only` to regenerate JSONL; if JSONL is authoritative, edit out stale duplicate records before import/rebuild",
        })),
    );
}

/// Pass-5 cycle 15: detector for
/// `fm-state_files-orphan-tmp-files`.
///
/// Walks `.beads/` for `*.tmp` and `*.tmp.<digits>` files left behind
/// by interrupted atomic-rename writes. Anything older than 1 hour is
/// almost certainly orphaned (in-flight tmps live milliseconds). Detect
/// and repair via quarantine: `br doctor --repair` re-runs this exact
/// predicate at fix time and renames matching regular files into the
/// per-run quarantine so no bytes are deleted.
const ORPHAN_TMP_AGE_THRESHOLD_SECS: u64 = 60 * 60;

// case_sensitive_file_extension_comparisons fires on `name.ends_with(".tmp")`
// because clippy prefers Path::extension(). We deliberately match exact-case
// `.tmp` (BR's atomic-write writers produce lowercase) and the secondary
// `*.tmp.<digits>` shape doesn't have `.tmp` as the Path extension.
#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn is_orphan_tmp_name(name: &str) -> bool {
    name.ends_with(".tmp")
        || (name.contains(".tmp.")
            && name
                .rsplit('.')
                .next()
                .is_some_and(|s| s.chars().all(|c| c.is_ascii_digit())))
}

fn orphan_tmp_entry(
    entry: &fs::DirEntry,
    now: std::time::SystemTime,
    threshold: std::time::Duration,
) -> Option<(String, PathBuf)> {
    let name_os = entry.file_name();
    let name = name_os.to_str()?;
    if !is_orphan_tmp_name(name) {
        return None;
    }
    let file_type = entry.file_type().ok()?;
    if !file_type.is_file() {
        return None;
    }
    let meta = entry.metadata().ok()?;
    let mtime = meta.modified().ok()?;
    if now
        .duration_since(mtime)
        .map(|age| age > threshold)
        .unwrap_or(false)
    {
        Some((name.to_string(), entry.path()))
    } else {
        None
    }
}

fn check_orphan_tmp_files(beads_dir: &Path, checks: &mut Vec<CheckResult>) {
    use std::time::{Duration, SystemTime};
    let Ok(entries) = fs::read_dir(beads_dir) else {
        push_check(checks, "tmp_files_orphan", CheckStatus::Ok, None, None);
        return;
    };
    let now = SystemTime::now();
    let threshold = Duration::from_secs(ORPHAN_TMP_AGE_THRESHOLD_SECS);
    let mut orphans: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        if let Some((name, _path)) = orphan_tmp_entry(&entry, now, threshold) {
            orphans.push(name);
        }
    }
    orphans.sort();
    if orphans.is_empty() {
        push_check(checks, "tmp_files_orphan", CheckStatus::Ok, None, None);
    } else {
        push_check(
            checks,
            "tmp_files_orphan",
            CheckStatus::Warn,
            Some(format!(
                "{} orphan tmp file(s) older than {}s under {}",
                orphans.len(),
                ORPHAN_TMP_AGE_THRESHOLD_SECS,
                beads_dir.display()
            )),
            Some(serde_json::json!({
                "files": orphans,
                "age_threshold_secs": ORPHAN_TMP_AGE_THRESHOLD_SECS,
                "remediation": "Verify no peer process is writing, then run br doctor --repair to quarantine the tmp files",
            })),
        );
    }
}

/// Pass-5 cycle 14: detector for
/// `fm-permissions-jsonl-world-writable`.
///
/// Warns when the selected JSONL export has the world-write bit
/// (`0o002`) set. A world-writable JSONL data file is a real security
/// concern — any local user could inject malicious issues that
/// subsequent `br sync --flush-only` reads back into the DB.
/// Auto-fixable via `br doctor --repair` for selected JSONL paths
/// inside the workspace. On non-Unix targets this is a no-op.
fn check_jsonl_world_writable(jsonl_path: Option<&Path>, checks: &mut Vec<CheckResult>) {
    let Some(path) = jsonl_path else {
        push_check(
            checks,
            "permissions.jsonl_world_writable",
            CheckStatus::Ok,
            None,
            None,
        );
        return;
    };
    let Ok(meta) = fs::symlink_metadata(path) else {
        push_check(
            checks,
            "permissions.jsonl_world_writable",
            CheckStatus::Ok,
            None,
            None,
        );
        return;
    };
    if meta.file_type().is_symlink() {
        push_check(
            checks,
            "permissions.jsonl_world_writable",
            CheckStatus::Ok,
            None,
            None,
        );
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        if (mode & 0o002) != 0 {
            push_check(
                checks,
                "permissions.jsonl_world_writable",
                CheckStatus::Warn,
                Some(format!(
                    "{} is world-writable (mode {:o}); anyone could inject issues that `br sync` reimports",
                    path.display(),
                    mode & 0o777
                )),
                Some(serde_json::json!({
                    "path": path.display().to_string(),
                    "mode_octal": format!("{:o}", mode & 0o777),
                    "remediation": format!("chmod o-w {}", path.display()),
                })),
            );
            return;
        }
    }
    push_check(
        checks,
        "permissions.jsonl_world_writable",
        CheckStatus::Ok,
        None,
        None,
    );
}

/// Pass-5 cycle 25 — fixer for
/// `fm-permissions-jsonl-world-writable`.
///
/// Strips the world-write bit (`0o002`) from the selected JSONL
/// export's mode via [`chokepoint::mutate(Op::Chmod)`]. This is the
/// first production exercise of `Op::Chmod`. The chokepoint backs up
/// the file bytes verbatim (preserving the ORIGINAL mode via
/// `copy_source_permissions`) so `doctor undo` byte-restores both the
/// content and the original mode.
///
/// Unix-only; non-Unix targets always see the detector emit `Ok` so
/// the fixer is unreachable there. Returns `true` if the chmod
/// succeeded.
fn fix_jsonl_world_writable_if_warned(
    jsonl_path: Option<&Path>,
    report: &DoctorReport,
    ctx: &OutputContext,
    session: Option<&mut DoctorRepairSession>,
) -> bool {
    let has_warning = report
        .checks
        .iter()
        .any(|c| c.name == "permissions.jsonl_world_writable" && c.status == CheckStatus::Warn);
    if !has_warning {
        return false;
    }
    let Some(path) = jsonl_path else {
        if !ctx.is_json() {
            ctx.warning(
                "Skipping world-writable chmod: no JSONL path resolved (workspace not initialised)",
            );
        }
        return false;
    };
    let Some(session) = session else {
        if !ctx.is_json() {
            ctx.warning(
                "Skipping world-writable chmod: no doctor repair session (run-dir creation failed)",
            );
        }
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let Ok(meta) = fs::symlink_metadata(path) else {
            return false;
        };
        if meta.file_type().is_symlink() || !meta.is_file() {
            return false;
        }
        let current = meta.permissions().mode();
        if (current & 0o002) == 0 {
            // TOCTOU: caller may have already chmod'd between detect and fix.
            return false;
        }
        if !path_is_inside_workspace(path, &session.ctx.repo_root) {
            if !ctx.is_json() {
                ctx.warning(&format!(
                    "Skipping world-writable chmod for external JSONL outside workspace: {}",
                    path.display()
                ));
            }
            return false;
        }
        // Keep every other bit; only the world-write bit changes.
        let new_mode = current & !0o002;
        session.set_fixer("doctor.jsonl_world_writable_chmod");
        session
            .ctx
            .capabilities
            .write_scopes
            .push(path.to_path_buf());
        match chokepoint::mutate(&session.ctx, path, Op::Chmod { mode: new_mode }) {
            Ok(result) if result.ok => {
                if !ctx.is_json() {
                    ctx.info(&format!(
                        "Stripped world-write bit from {} (mode {:o}→{:o})",
                        path.display(),
                        current & 0o777,
                        new_mode & 0o777
                    ));
                }
                true
            }
            Ok(_) => false,
            Err(err) => {
                if !ctx.is_json() {
                    ctx.warning(&format!(
                        "Failed to chmod world-write bit off {}: {err}",
                        path.display()
                    ));
                }
                false
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (path, session);
        false
    }
}

/// Pass-5 cycle 13: detector for the inner-gitignore-present subset of
/// `fm-configs-gitignore-leaking-beads`.
///
/// Warns when `.beads/.gitignore` is missing (the workspace can leak
/// transient state like `.write.lock` into git history) OR exists but
/// doesn't list expected ephemeral patterns. Complements the existing
/// `gitignore.beads_inner` check, which only validates that the ROOT
/// `.gitignore` doesn't shadow this file.
///
/// Auto-fixable for the missing/incomplete regular-file cases by
/// appending the canonical patterns. Symlinked ignore files remain
/// operator-managed because replacing them would stomp intent.
fn check_inner_gitignore_present(beads_dir: &Path, checks: &mut Vec<CheckResult>) {
    let path = beads_dir.join(".gitignore");
    let meta = match fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            push_check(
                checks,
                "gitignore.beads_inner_present",
                CheckStatus::Warn,
                Some(format!(
                    "{} is missing; transient .beads/ state can leak into git history",
                    path.display()
                )),
                Some(serde_json::json!({
                    "path": path.display().to_string(),
                    "kind": "missing",
                    "expected_patterns": inner_gitignore_all_append_patterns(),
                    "remediation": format!(
                        "Create {} with at least: {}",
                        path.display(),
                        inner_gitignore_all_append_patterns().join(", ")
                    ),
                })),
            );
            return;
        }
        Err(_) => {
            // Uninspectable but present (perms issue?) — leave to the
            // permissions check.
            push_check(
                checks,
                "gitignore.beads_inner_present",
                CheckStatus::Ok,
                None,
                None,
            );
            return;
        }
    };
    if meta.file_type().is_symlink() {
        push_check(
            checks,
            "gitignore.beads_inner_present",
            CheckStatus::Warn,
            Some(format!(
                "{} is a symlink; git ignore files in the working tree must be regular files to reliably protect .beads/ state",
                path.display()
            )),
            Some(serde_json::json!({
                "path": path.display().to_string(),
                "kind": "symlink",
                "expected_patterns": inner_gitignore_all_append_patterns(),
                "remediation": format!(
                    "Replace {} with a regular file containing at least: {}",
                    path.display(),
                    inner_gitignore_all_append_patterns().join(", ")
                ),
            })),
        );
        return;
    }
    let Ok(contents) = fs::read_to_string(&path) else {
        // Unreadable but present (perms issue?) — leave to the
        // permissions check.
        push_check(
            checks,
            "gitignore.beads_inner_present",
            CheckStatus::Ok,
            None,
            None,
        );
        return;
    };
    let missing = inner_gitignore_missing_patterns(&contents);
    if missing.is_empty() {
        push_check(
            checks,
            "gitignore.beads_inner_present",
            CheckStatus::Ok,
            None,
            None,
        );
    } else {
        push_check(
            checks,
            "gitignore.beads_inner_present",
            CheckStatus::Warn,
            Some(format!(
                "{} exists but is missing expected pattern(s): {}",
                path.display(),
                missing.join(", ")
            )),
            Some(serde_json::json!({
                "path": path.display().to_string(),
                "kind": "incomplete",
                "missing_patterns": missing,
            })),
        );
    }
}

/// Pass-5 cycle 27 — fixer for the missing/incomplete subset of
/// `fm-configs-gitignore-leaking-beads` (inner-gitignore family).
///
/// When the detector warns with `kind="missing"` or `kind="incomplete"`,
/// auto-append the canonical patterns to `.beads/.gitignore` via
/// [`chokepoint::mutate(Op::AppendFile)`]. The chokepoint handles the
/// missing-file case by treating absent contents as empty bytes; the
/// resulting file gets the canonical patterns one per line.
///
/// Symlinks are REFUSED — replacing a symlink with a regular file
/// could break operator intent (compliance workflows, vendored shared
/// configs). The detector's `kind="symlink"` warning remains the
/// operator's responsibility.
///
/// The chokepoint backs up the pre-fix file bytes (or records "did
/// not exist" for missing) so `doctor undo` restores the file —
/// either to its previous incomplete state or by removing it
/// entirely. TOCTOU-safe: missing patterns are re-derived at fix time.
/// One canonical `.beads/.gitignore` coverage expectation (GitHub #427).
///
/// Coverage is judged by PROBES — representative artifact filenames that
/// must all be ignored — so operator-authored equivalents (e.g. a broad
/// `*.db?*` or `*.lock`) satisfy an expectation without textual equality
/// with `append_pattern`. When any probe is uncovered, `doctor --repair`
/// appends `append_pattern`.
struct InnerGitignoreExpectation {
    /// Pattern appended by `doctor --repair` when coverage is missing.
    append_pattern: &'static str,
    /// Representative direct-child filenames that must be ignored.
    probes: &'static [&'static str],
}

/// The database-artifact family plus the original transient-state rules.
///
/// GitHub #427: fsqlite 0.3 grew `-wal-cert`/`-wal-cert-head` durability
/// certificates, `-fsqlite-ns-gate`/`-fsqlite-ns-use` namespace sidecars,
/// and `br doctor migrate-schema` leaves `.<db>.schema-migration-<run>.
/// vacuum-*` copies. Older repositories whose `.beads/.gitignore` predates
/// these artifacts would show them as untracked changes; doctor now
/// actively reconciles coverage instead of only writing rules at init.
const INNER_GITIGNORE_EXPECTATIONS: &[InnerGitignoreExpectation] = &[
    InnerGitignoreExpectation {
        append_pattern: "*.db",
        probes: &["beads.db"],
    },
    InnerGitignoreExpectation {
        append_pattern: "*.db-journal",
        probes: &["beads.db-journal"],
    },
    InnerGitignoreExpectation {
        append_pattern: "*.db-shm",
        probes: &["beads.db-shm"],
    },
    InnerGitignoreExpectation {
        append_pattern: "*.db-wal*",
        probes: &[
            "beads.db-wal",
            "beads.db-wal-cert",
            "beads.db-wal-cert-head",
        ],
    },
    InnerGitignoreExpectation {
        append_pattern: "*-fsqlite-ns-gate",
        probes: &[
            "beads.db-fsqlite-ns-gate",
            ".beads.db.schema-migration-20260101T000000.000000Z-0-0.vacuum-fsqlite-ns-gate",
        ],
    },
    InnerGitignoreExpectation {
        append_pattern: "*-fsqlite-ns-use",
        probes: &[
            "beads.db-fsqlite-ns-use",
            ".beads.db.schema-migration-20260101T000000.000000Z-0-0.vacuum-fsqlite-ns-use",
        ],
    },
    InnerGitignoreExpectation {
        append_pattern: "*.vacuum-wal-cert*",
        probes: &[
            ".beads.db.schema-migration-20260101T000000.000000Z-0-0.vacuum-wal-cert",
            ".beads.db.schema-migration-20260101T000000.000000Z-0-0.vacuum-wal-cert-head",
        ],
    },
    // fsqlite 0.3.6+ records engine-upgrade bookkeeping beside the DB in
    // `<db>.fsqlite-migration-state`; without a rule it shows up as an
    // untracked file in every pre-0.3.6 repository.
    InnerGitignoreExpectation {
        append_pattern: "*.fsqlite-migration-state",
        probes: &["beads.db.fsqlite-migration-state"],
    },
    InnerGitignoreExpectation {
        append_pattern: ".write.lock",
        probes: &[".write.lock"],
    },
    InnerGitignoreExpectation {
        append_pattern: "*.tmp",
        probes: &["probe.tmp"],
    },
];

/// Every append pattern doctor knows how to maintain, for remediation text.
fn inner_gitignore_all_append_patterns() -> Vec<&'static str> {
    INNER_GITIGNORE_EXPECTATIONS
        .iter()
        .map(|expectation| expectation.append_pattern)
        .collect()
}

/// Append patterns whose coverage is missing from `contents`.
fn inner_gitignore_missing_patterns(contents: &str) -> Vec<&'static str> {
    INNER_GITIGNORE_EXPECTATIONS
        .iter()
        .filter(|expectation| {
            expectation
                .probes
                .iter()
                .any(|probe| !inner_gitignore_ignores_probe(contents, probe))
        })
        .map(|expectation| expectation.append_pattern)
        .collect()
}

/// Whether the ignore file `contents` ignore a direct-child file named
/// `probe`. Gitignore semantics for the subset that matters here: last
/// matching rule wins, `!` negates, `#` comments, a leading `/` anchors to
/// the `.beads/` directory (equivalent for direct children), trailing-`/`
/// directory rules and nested (`a/b`) rules never match a file probe.
fn inner_gitignore_ignores_probe(contents: &str, probe: &str) -> bool {
    let mut ignored = false;
    for raw_line in contents.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (negated, pattern) = line
            .strip_prefix('!')
            .map_or((false, line), |pattern| (true, pattern));
        let pattern = pattern.strip_prefix('/').unwrap_or(pattern);
        if pattern.is_empty() || pattern.ends_with('/') || pattern.contains('/') {
            continue;
        }
        if gitignore_glob_matches(pattern, probe) {
            ignored = !negated;
        }
    }
    ignored
}

/// Minimal single-segment gitignore glob: `*` matches any run (including
/// empty), `?` matches exactly one character, everything else is literal.
/// Character classes are treated literally — none of the canonical
/// patterns use them, and a literal mismatch merely means doctor appends a
/// redundant-but-correct rule.
fn gitignore_glob_matches(pattern: &str, name: &str) -> bool {
    let pattern = pattern.as_bytes();
    let name = name.as_bytes();
    let (mut p, mut n) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while n < name.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some((p, n));
            p += 1;
        } else if let Some((star_p, star_n)) = star {
            p = star_p + 1;
            n = star_n + 1;
            star = Some((star_p, star_n + 1));
        } else {
            return false;
        }
    }
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

fn fix_inner_gitignore_if_warned(
    beads_dir: &Path,
    report: &DoctorReport,
    ctx: &OutputContext,
    session: Option<&mut DoctorRepairSession>,
) -> bool {
    let has_warning = report
        .checks
        .iter()
        .any(|c| c.name == "gitignore.beads_inner_present" && c.status == CheckStatus::Warn);
    if !has_warning {
        return false;
    }
    let Some(session) = session else {
        if !ctx.is_json() {
            ctx.warning(
                "Skipping inner-gitignore repair: no doctor repair session (run-dir creation failed)",
            );
        }
        return false;
    };
    let path = beads_dir.join(".gitignore");
    let existing = match fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            // Refuse to convert a symlink into a regular file.
            return false;
        }
        Ok(_) => fs::read_to_string(&path).unwrap_or_default(),
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(_) => return false,
    };
    let missing = inner_gitignore_missing_patterns(&existing);
    if missing.is_empty() {
        return false;
    }
    let mut content: Vec<u8> = Vec::new();
    if !existing.is_empty() && !existing.ends_with('\n') {
        content.push(b'\n');
    }
    for pattern in &missing {
        content.extend_from_slice(pattern.as_bytes());
        content.push(b'\n');
    }
    session.set_fixer("doctor.inner_gitignore_append");
    match chokepoint::mutate(&session.ctx, &path, Op::AppendFile { content }) {
        Ok(result) if result.ok => {
            if !ctx.is_json() {
                ctx.info(&format!(
                    "Appended missing pattern(s) to {}: {}",
                    path.display(),
                    missing.join(", ")
                ));
            }
            true
        }
        Ok(_) => false,
        Err(err) => {
            if !ctx.is_json() {
                ctx.warning(&format!(
                    "Failed to append patterns to {}: {err}",
                    path.display()
                ));
            }
            false
        }
    }
}

/// Pass-5 cycle 12: detector for
/// `fm-external_artifacts-multiple-br-in-path`.
///
/// Walks `$PATH` and reports a warning when more than one executable
/// named `br` is found. Operators with multiple installs (cargo
/// install, system package manager, vendored fork) can be confused
/// about which version is actually invoked. Detect-only — resolving
/// the ambiguity is operator-controlled (PATH ordering, removing
/// stale installs, choosing a canonical location).
///
/// The pure helper `br_binaries_in_path_str` makes the detector
/// testable without env mutation.
fn br_binaries_in_path_str(path_var: &str) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for dir in std::env::split_paths(path_var) {
        let candidate = dir.join("br");
        let canonical = candidate.canonicalize().ok();
        // Dedupe by canonical path so a/PATH/br and b/PATH/br both
        // pointing to the same file via symlinks count once.
        let key = canonical.clone().unwrap_or_else(|| candidate.clone());
        if seen.contains(&key) {
            continue;
        }
        if candidate.is_file() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if let Ok(meta) = fs::metadata(&candidate)
                    && (meta.permissions().mode() & 0o111) != 0
                {
                    found.push(candidate.clone());
                    seen.insert(key);
                }
            }
            #[cfg(not(unix))]
            {
                found.push(candidate.clone());
                seen.insert(key);
            }
        }
    }
    found
}

fn check_multiple_br_in_path(checks: &mut Vec<CheckResult>) {
    let path_var = std::env::var("PATH").unwrap_or_default();
    let binaries = br_binaries_in_path_str(&path_var);
    if binaries.len() <= 1 {
        push_check(checks, "br_path_dupes", CheckStatus::Ok, None, None);
        return;
    }
    let display: Vec<String> = binaries.iter().map(|p| p.display().to_string()).collect();
    push_check(
        checks,
        "br_path_dupes",
        CheckStatus::Warn,
        Some(format!(
            "Found {} `br` executables on $PATH — operator may be confused which one runs",
            binaries.len()
        )),
        Some(serde_json::json!({
            "br_paths": display,
            "remediation": "Reorder PATH so the canonical install resolves first, or remove stale copies",
        })),
    );
}

/// Pass-5 cycle 11: detector for
/// `fm-permissions-config-yaml-mode-leaks-secrets`.
///
/// Warns when `.beads/config.yaml` is world-readable (mode has the
/// other-readable bit `0o004` set) AND its contents contain
/// secret-shaped keywords (`token`, `secret`, `password`, `api_key`).
/// Auto-fixable via `br doctor --repair` by stripping other-read and
/// other-write bits while preserving owner/group permissions.
///
/// On non-Unix targets the mode check is a no-op (always Ok).
fn check_config_yaml_secret_mode(beads_dir: &Path, checks: &mut Vec<CheckResult>) {
    const SECRET_KEYWORDS: &[&str] = &["token", "secret", "password", "api_key", "private_key"];
    let config = beads_dir.join("config.yaml");
    let Ok(meta) = fs::symlink_metadata(&config) else {
        push_check(
            checks,
            "permissions.config_yaml_secrets",
            CheckStatus::Ok,
            None,
            None,
        );
        return;
    };
    // Skip symlinks; the existing config.yaml check covers symlink-shape
    // concerns and we shouldn't follow into out-of-scope targets.
    if meta.file_type().is_symlink() {
        push_check(
            checks,
            "permissions.config_yaml_secrets",
            CheckStatus::Ok,
            None,
            None,
        );
        return;
    }
    #[cfg(unix)]
    let world_readable = {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        (mode & 0o004) != 0
    };
    #[cfg(not(unix))]
    let world_readable = false;
    if !world_readable {
        push_check(
            checks,
            "permissions.config_yaml_secrets",
            CheckStatus::Ok,
            None,
            None,
        );
        return;
    }
    let Ok(contents) = fs::read_to_string(&config) else {
        // Unreadable → covered by the regular config.yaml check.
        push_check(
            checks,
            "permissions.config_yaml_secrets",
            CheckStatus::Ok,
            None,
            None,
        );
        return;
    };
    let lower = contents.to_ascii_lowercase();
    // Match common secret-shaped tokens. Substring match keeps the
    // detector cheap and false-positive-prone — operators who genuinely
    // want a comment about "passwords" can ignore the warning. We
    // surface the matched keyword in details so the operator can
    // verify quickly.
    let matched: Vec<&str> = SECRET_KEYWORDS
        .iter()
        .copied()
        .filter(|kw| lower.contains(kw))
        .collect();
    if matched.is_empty() {
        push_check(
            checks,
            "permissions.config_yaml_secrets",
            CheckStatus::Ok,
            None,
            None,
        );
        return;
    }
    let mode_bits = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            meta.permissions().mode() & 0o777
        }
        #[cfg(not(unix))]
        {
            0u32
        }
    };
    push_check(
        checks,
        "permissions.config_yaml_secrets",
        CheckStatus::Warn,
        Some(format!(
            "{} is world-readable (mode {:o}) and appears to contain secret-shaped values; consider `chmod 0600 {}`",
            config.display(),
            mode_bits,
            config.display()
        )),
        Some(serde_json::json!({
            "path": config.display().to_string(),
            "mode_octal": format!("{mode_bits:o}"),
            "matched_keywords": matched,
            "remediation": format!("chmod 0600 {}", config.display()),
        })),
    );
}

/// Pass-5 cycle 26 — fixer for
/// `fm-permissions-config-yaml-mode-leaks-secrets`.
///
/// When the detector warns that `.beads/config.yaml` is world-readable
/// AND contains secret-shaped keywords, strip the world-read AND
/// world-write bits (`0o006` mask) via [`chokepoint::mutate(Op::Chmod)`].
/// We strip both because a file flagged as containing secrets should
/// not be world-readable OR world-writable; the mask is conservative
/// (group bits preserved) so operators using group-shared configs
/// keep working. The chokepoint backs up the file bytes verbatim with
/// the ORIGINAL mode preserved, so `doctor undo` byte-restores both
/// content and mode through the existing WriteFile-with-mode path.
///
/// Unix-only; non-Unix targets always emit Ok from the detector so the
/// fixer is unreachable there.
fn fix_config_yaml_secret_mode_if_warned(
    beads_dir: &Path,
    report: &DoctorReport,
    ctx: &OutputContext,
    session: Option<&mut DoctorRepairSession>,
) -> bool {
    let has_warning = report
        .checks
        .iter()
        .any(|c| c.name == "permissions.config_yaml_secrets" && c.status == CheckStatus::Warn);
    if !has_warning {
        return false;
    }
    let Some(session) = session else {
        if !ctx.is_json() {
            ctx.warning(
                "Skipping config.yaml secrets chmod: no doctor repair session (run-dir creation failed)",
            );
        }
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = beads_dir.join("config.yaml");
        let Ok(meta) = fs::symlink_metadata(&path) else {
            return false;
        };
        if meta.file_type().is_symlink() || !meta.is_file() {
            return false;
        }
        let current = meta.permissions().mode();
        // Already secured — TOCTOU defense.
        if (current & 0o006) == 0 {
            return false;
        }
        // Strip world-read AND world-write; preserve every other bit.
        let new_mode = current & !0o006;
        session.set_fixer("doctor.config_yaml_secret_chmod");
        match chokepoint::mutate(&session.ctx, &path, Op::Chmod { mode: new_mode }) {
            Ok(result) if result.ok => {
                if !ctx.is_json() {
                    ctx.info(&format!(
                        "Stripped world-read/write bits from {} (mode {:o}→{:o})",
                        path.display(),
                        current & 0o777,
                        new_mode & 0o777
                    ));
                }
                true
            }
            Ok(_) => false,
            Err(err) => {
                if !ctx.is_json() {
                    ctx.warning(&format!(
                        "Failed to chmod config.yaml secret mode {}: {err}",
                        path.display()
                    ));
                }
                false
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (beads_dir, session);
        false
    }
}

/// Detector for `fm-permissions-db-sidecar-mode-too-open` (GitHub #403).
///
/// fsqlite refuses to open a namespace sidecar (`-fsqlite-ns-gate` /
/// `-fsqlite-ns-use`) carrying any bit in `0o077`, and the refusal reaches the
/// operator as a bare `Database error: unable to open database file: '<sidecar>'`
/// — which reads as database corruption. The storage layer now repairs an
/// owned sidecar on open, so this check is the visible half: it names the file
/// and the mode whenever the workspace still holds one that is group- or
/// other-accessible (a sidecar owned by another user is the case the storage
/// layer cannot fix by itself).
///
/// On non-Unix targets the mode check is a no-op (always Ok).
fn check_db_sidecar_modes(db_path: &Path, checks: &mut Vec<CheckResult>) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        use std::os::unix::fs::MetadataExt;

        let mut offenders = Vec::new();
        for sidecar in fsqlite_namespace_sidecar_paths(db_path) {
            let Ok(meta) = fs::symlink_metadata(&sidecar) else {
                continue;
            };
            if !meta.is_file() || meta.file_type().is_symlink() {
                continue;
            }
            let mode = meta.permissions().mode() & 0o7777;
            // A group/other exposure the linked engine admits as it is (bounded
            // by the database file's, FrankenSQLite 0.3.18+) blocks nothing and
            // is not a finding (GitHub #491).
            if !SqliteStorage::namespace_sidecar_mode_is_admitted(mode, meta.gid(), db_path) {
                offenders.push((sidecar, mode));
            }
        }

        if offenders.is_empty() {
            push_check(
                checks,
                "permissions.db_sidecars",
                CheckStatus::Ok,
                None,
                None,
            );
            return;
        }

        let summary = offenders
            .iter()
            .map(|(path, mode)| format!("{} (mode {mode:04o})", path.display()))
            .collect::<Vec<_>>()
            .join(", ");
        // A database carrying exactly the same group/other bits as every
        // offending sidecar is the signature of a mount that reports a fixed
        // mask instead of stored permission bits (WSL drvfs without
        // `metadata`, FAT/exFAT; GitHub #491). chmod cannot fix that, so say
        // so instead of prescribing a no-op.
        let database_mask = fs::symlink_metadata(db_path)
            .ok()
            .filter(|meta| meta.is_file() && !meta.file_type().is_symlink())
            .map(|meta| meta.permissions().mode() & 0o077);
        let filesystem_mask_suspected = database_mask
            .is_some_and(|mask| offenders.iter().all(|(_, mode)| mode & 0o077 == mask));
        let mut remediation = offenders
            .iter()
            .map(|(path, _)| format!("chmod 0600 {}", path.display()))
            .collect::<Vec<_>>()
            .join("; ");
        if filesystem_mask_suspected {
            remediation.push_str(
                "; if chmod has no effect, the filesystem does not persist POSIX permission bits \
                 (a Windows drive under WSL mounted without the `metadata` option, FAT/exFAT): \
                 mount it with `metadata` or keep the .beads database on the Linux filesystem",
            );
        }
        push_check(
            checks,
            "permissions.db_sidecars",
            CheckStatus::Warn,
            Some(format!(
                "fsqlite namespace sidecar(s) carry group/other permission the engine refuses and \
                 block every database open until the mode is owner-only (0600): {summary}. {}",
                SqliteStorage::engine_namespace_sidecar_rule()
            )),
            Some(serde_json::json!({
                "sidecars": offenders
                    .iter()
                    .map(|(path, mode)| serde_json::json!({
                        "path": path.display().to_string(),
                        "mode_octal": format!("{mode:04o}"),
                    }))
                    .collect::<Vec<_>>(),
                "required_mode_octal": "0600",
                "engine_rule": SqliteStorage::engine_namespace_sidecar_rule(),
                "filesystem_mask_suspected": filesystem_mask_suspected,
                "remediation": remediation,
            })),
        );
    }
    #[cfg(not(unix))]
    {
        let _ = db_path;
        push_check(
            checks,
            "permissions.db_sidecars",
            CheckStatus::Ok,
            None,
            None,
        );
    }
}

/// Fixer for `fm-permissions-db-sidecar-mode-too-open` (GitHub #403).
///
/// Strips the group/other bits (`0o077` mask) from every flagged fsqlite
/// namespace sidecar through [`chokepoint::mutate(Op::Chmod)`], so the change
/// is backed up, audited in `actions.jsonl`, and reversible via `doctor undo`.
/// Owner bits are preserved (`0o700` is an accepted mode); only the bits that
/// make the engine refuse the open are removed. A sidecar this process does
/// not own fails the chmod and stays flagged, which is the correct outcome —
/// its owner has to act.
///
/// Unix-only; the detector always reports Ok elsewhere so the fixer is
/// unreachable there.
fn fix_db_sidecar_modes_if_warned(
    db_path: &Path,
    report: &DoctorReport,
    ctx: &OutputContext,
    session: Option<&mut DoctorRepairSession>,
) -> bool {
    let has_warning = report
        .checks
        .iter()
        .any(|c| c.name == "permissions.db_sidecars" && c.status == CheckStatus::Warn);
    if !has_warning {
        return false;
    }
    let Some(session) = session else {
        if !ctx.is_json() {
            ctx.warning(
                "Skipping database sidecar chmod: no doctor repair session (run-dir creation failed)",
            );
        }
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let mut repaired_any = false;
        for sidecar in fsqlite_namespace_sidecar_paths(db_path) {
            let Ok(meta) = fs::symlink_metadata(&sidecar) else {
                continue;
            };
            if meta.file_type().is_symlink() || !meta.is_file() {
                continue;
            }
            let current = meta.permissions().mode();
            // Already owner-only (group/other bits all clear) — TOCTOU defense
            // (the storage layer may have self-healed it between detection and
            // repair) — or an exposure the linked engine admits as it is.
            if current.trailing_zeros() >= 6
                || SqliteStorage::namespace_sidecar_mode_is_admitted(
                    current & 0o7777,
                    meta.gid(),
                    db_path,
                )
            {
                continue;
            }
            let new_mode = current & !0o077;
            session.set_fixer("doctor.db_sidecar_mode_chmod");
            match chokepoint::mutate(&session.ctx, &sidecar, Op::Chmod { mode: new_mode }) {
                Ok(result) if result.ok => {
                    // A mount that ignores chmod (WSL drvfs without
                    // `metadata`, FAT/exFAT) reports success and keeps its
                    // mask; that is not a repair (GitHub #491).
                    let observed = fs::symlink_metadata(&sidecar)
                        .map(|meta| meta.permissions().mode())
                        .unwrap_or(new_mode);
                    if observed & 0o077 != 0 {
                        if !ctx.is_json() {
                            ctx.warning(&format!(
                                "chmod on database sidecar {} returned success but the mode is still {:04o}: this filesystem does not persist POSIX permission bits (a Windows drive under WSL mounted without `metadata`, FAT/exFAT). Mount it with `metadata` or keep the .beads database on the Linux filesystem",
                                sidecar.display(),
                                observed & 0o7777
                            ));
                        }
                        continue;
                    }
                    repaired_any = true;
                    if !ctx.is_json() {
                        ctx.info(&format!(
                            "Restored owner-only mode on {} ({:o}→{:o})",
                            sidecar.display(),
                            current & 0o777,
                            new_mode & 0o777
                        ));
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    if !ctx.is_json() {
                        ctx.warning(&format!(
                            "Failed to chmod database sidecar {}: {err}",
                            sidecar.display()
                        ));
                    }
                }
            }
        }
        repaired_any
    }
    #[cfg(not(unix))]
    {
        let _ = (db_path, session);
        false
    }
}

/// Pass-5 cycle 10: detector for
/// `fm-observability-doctor-runs-dir-grows-unbounded`.
///
/// The doctor's own `.doctor/runs/<run-id>/` directories accumulate
/// every time `--repair` runs. Long-running workspaces can pile up
/// thousands of run-dirs, complicating `doctor undo` audits and using
/// inodes. This check warns when the count exceeds
/// `DOCTOR_RUNS_THRESHOLD` (50).
///
/// Detect-only by design: the doctor cannot prune its own audit
/// history without explicit operator consent. Operators clean up by
/// hand or via `find .doctor/runs/ -maxdepth 1 -mtime +N -exec mv {}
/// quarantine/`. Auto-pruning would corrupt the chokepoint contract
/// because subsequent `doctor undo <run-id>` calls would fail
/// silently on pruned run-ids.
const DOCTOR_RUNS_THRESHOLD: usize = 50;

fn check_doctor_runs_dir_size(repo_root: &Path, checks: &mut Vec<CheckResult>) {
    let runs_dir = repo_root.join(".doctor").join("runs");
    let entries = match fs::read_dir(&runs_dir) {
        Ok(e) => e,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            push_check(checks, "doctor.runs_dir", CheckStatus::Ok, None, None);
            return;
        }
        Err(_) => {
            // Unreadable runs dir is suspicious but a different FM owns
            // permissions issues; emit Ok here and let the permissions
            // detectors carry the signal.
            push_check(checks, "doctor.runs_dir", CheckStatus::Ok, None, None);
            return;
        }
    };
    let run_count = entries
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .count();
    if run_count > DOCTOR_RUNS_THRESHOLD {
        push_check(
            checks,
            "doctor.runs_dir",
            CheckStatus::Warn,
            Some(format!(
                "{run_count} run directories accumulated in {}; consider pruning (operator-driven; doctor cannot auto-prune its own audit history)",
                runs_dir.display()
            )),
            Some(serde_json::json!({
                "runs_dir": runs_dir.display().to_string(),
                "run_count": run_count,
                "threshold": DOCTOR_RUNS_THRESHOLD,
                "remediation": "Operator: review and prune via `find .doctor/runs/ -maxdepth 1 -mtime +30 | xargs mv -t .doctor/runs/quarantine/`",
            })),
        );
    } else {
        push_check(checks, "doctor.runs_dir", CheckStatus::Ok, None, None);
    }
}

/// Pass-5 cycle 28 — detector for
/// `fm-permissions-doctor-runs-not-creatable`.
///
/// Warns when `.doctor/` exists at the repo root but the current
/// process lacks owner-write permission on it. The doctor's
/// `--repair` flow creates a fresh `.doctor/runs/<run-id>/` directory
/// for every run; if `.doctor/` is read-only the chokepoint cannot
/// open the per-run audit log and operators see a confusing
/// `Permission denied` from deep inside the mutate path. Surfacing
/// this up-front during read-only doctor saves a wasted `--repair`
/// attempt.
///
/// Detect-only — operators may have intentionally locked
/// `.doctor/` (compliance, group-shared workflows); auto-chmoding
/// would stomp their decision. The remediation is operator-driven:
/// `chmod u+w .doctor` or move the dir out of the way.
///
/// Unix-only — non-Unix lacks the POSIX mode bits this check relies
/// on, so it always emits Ok there.
fn check_doctor_runs_creatable(repo_root: &Path, checks: &mut Vec<CheckResult>) {
    let doctor_dir = repo_root.join(".doctor");
    let Ok(meta) = fs::symlink_metadata(&doctor_dir) else {
        // Missing is fine — the dir will be created on first --repair.
        push_check(checks, "doctor.runs_creatable", CheckStatus::Ok, None, None);
        return;
    };
    if !meta.is_dir() {
        // `.doctor` exists but isn't a directory; a different FM owns this.
        push_check(checks, "doctor.runs_creatable", CheckStatus::Ok, None, None);
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        // Owner-write bit on the directory. POSIX semantics: writing
        // INTO a directory (creating a new entry) requires `w` on the
        // dir, not on the parent. We check the owner bit because the
        // doctor only ever runs as the workspace owner — group/other
        // writability is a separate concern (and we already flag
        // world-writable JSONL).
        if (mode & 0o200) == 0 {
            push_check(
                checks,
                "doctor.runs_creatable",
                CheckStatus::Warn,
                Some(format!(
                    "{} is not writable by owner (mode {:o}); next `br doctor --repair` will fail to create a per-run audit dir",
                    doctor_dir.display(),
                    mode & 0o777
                )),
                Some(serde_json::json!({
                    "path": doctor_dir.display().to_string(),
                    "mode_octal": format!("{:o}", mode & 0o777),
                    "remediation": format!("`chmod u+w {}` or move it aside before re-running `--repair`", doctor_dir.display()),
                })),
            );
            return;
        }
    }
    push_check(checks, "doctor.runs_creatable", CheckStatus::Ok, None, None);
}

/// Pass-5 cycle 30 — detector for
/// `fm-permissions-recovery-dir-not-writable`.
///
/// Warns when the configured DB family's recovery dir exists but
/// lacks the owner-write bit. The recovery dir is where `--repair`
/// paths move quarantined DB families to before performing destructive
/// rebuilds; if it's read-only the backup move fails and the
/// recoverable-anomaly fixer can't run safely.
///
/// Detect-only — operators may have intentionally locked the recovery
/// dir for compliance (immutable backup folder); auto-chmoding would
/// stomp that decision. Remediation: restore owner-write permission
/// on the reported path or unset the lock-down before re-running
/// `--repair`.
///
/// Unix-only — non-Unix lacks the POSIX mode bits this check relies
/// on, so it always emits Ok there.
fn check_recovery_dir_writable(db_path: &Path, beads_dir: &Path, checks: &mut Vec<CheckResult>) {
    let recovery_dir = config::recovery_dir_for_db_path(db_path, beads_dir);
    let Ok(meta) = fs::symlink_metadata(&recovery_dir) else {
        // Missing is fine — the dir is created on demand by the recovery path.
        push_check(
            checks,
            "permissions.recovery_dir",
            CheckStatus::Ok,
            None,
            None,
        );
        return;
    };
    if !meta.is_dir() {
        // `.br_recovery` exists but isn't a directory; a different FM owns this.
        push_check(
            checks,
            "permissions.recovery_dir",
            CheckStatus::Ok,
            None,
            None,
        );
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        if (mode & 0o200) == 0 {
            push_check(
                checks,
                "permissions.recovery_dir",
                CheckStatus::Warn,
                Some(format!(
                    "{} is not writable by owner (mode {:o}); next `br doctor --repair` will fail to quarantine the DB family before rebuild",
                    recovery_dir.display(),
                    mode & 0o777
                )),
                Some(serde_json::json!({
                    "path": recovery_dir.display().to_string(),
                    "mode_octal": format!("{:o}", mode & 0o777),
                    "remediation": format!("`chmod u+w {}` or move it aside before re-running `--repair`", recovery_dir.display()),
                })),
            );
            return;
        }
    }
    push_check(
        checks,
        "permissions.recovery_dir",
        CheckStatus::Ok,
        None,
        None,
    );
}

/// Pass-5 cycle 31 — detector for the lock-permissions subset of
/// `fm-state_files-orphaned-write-lock`.
///
/// Warns when `.beads/.write.lock` exists but the current process
/// lacks owner-write permission on it. Every mutating `br`
/// invocation (`create`, `update`, `close`, `sync`, `dep add`, …)
/// must open this file `O_RDWR` and acquire an advisory lock; a
/// missing owner-write bit makes the open fail outright with a
/// cryptic `EACCES` from deep inside the storage layer.
///
/// Detect-only — auto-chmoding might collide with operator intent
/// (e.g. immutable lock files used by some hardened CI runners).
/// Remediation: `chmod u+w .beads/.write.lock`, preserving the
/// existing lock inode so live holders retain the same authority.
///
/// Unix-only — non-Unix uses different lock semantics so the check
/// is silenced there.
fn check_write_lock_writable(beads_dir: &Path, checks: &mut Vec<CheckResult>) {
    let lock_path = beads_dir.join(".write.lock");
    let Ok(meta) = fs::symlink_metadata(&lock_path) else {
        // Missing is fine — created on first mutating call.
        push_check(
            checks,
            "permissions.write_lock",
            CheckStatus::Ok,
            None,
            None,
        );
        return;
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        // The non-regular shape itself is flagged (as an error) by the
        // `write_lock` check; stay Ok here so one defect is not
        // double-counted, but say why the permission probe is skipped.
        push_check(
            checks,
            "permissions.write_lock",
            CheckStatus::Ok,
            Some(
                ".beads/.write.lock is not a regular file; permission probe skipped \
                 (the write_lock check reports the non-regular node itself)"
                    .to_string(),
            ),
            None,
        );
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        if (mode & 0o200) == 0 {
            push_check(
                checks,
                "permissions.write_lock",
                CheckStatus::Warn,
                Some(format!(
                    "{} is not writable by owner (mode {:o}); every mutating `br` invocation will fail to acquire the workspace lock",
                    lock_path.display(),
                    mode & 0o777
                )),
                Some(serde_json::json!({
                    "path": lock_path.display().to_string(),
                    "mode_octal": format!("{:o}", mode & 0o777),
                    "remediation": format!("`chmod u+w {}` (preserve the existing lock file)", lock_path.display()),
                })),
            );
            return;
        }
    }
    push_check(
        checks,
        "permissions.write_lock",
        CheckStatus::Ok,
        None,
        None,
    );
}

/// Pass-5 cycle 32 — detector for
/// `fm-permissions-gitignore-not-writable-blocks-repair`.
///
/// Warns when the repo-root `.gitignore` exists but the current
/// process lacks owner-write permission on it. The existing
/// `doctor.gitignore_repair` fixer rewrites this file when the
/// `.beads/` shadow rule is missing; this detector lets that fixer
/// refuse the write before it bypasses an intentionally locked file.
///
/// Detect-only — operators may have intentionally locked the
/// repo-root `.gitignore` (compliance-controlled file, vendored
/// shared config). Remediation: `chmod u+w .gitignore` or hand-edit
/// the file before re-running `--repair`.
///
/// Unix-only — non-Unix lacks POSIX mode bits this check relies on.
fn check_root_gitignore_writable(repo_root: &Path, checks: &mut Vec<CheckResult>) {
    let gitignore = repo_root.join(".gitignore");
    let Ok(meta) = fs::symlink_metadata(&gitignore) else {
        // Missing is fine — the gitignore_repair fixer creates one.
        push_check(
            checks,
            "permissions.root_gitignore",
            CheckStatus::Ok,
            None,
            None,
        );
        return;
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        push_check(
            checks,
            "permissions.root_gitignore",
            CheckStatus::Ok,
            None,
            None,
        );
        return;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        if (mode & 0o200) == 0 {
            push_check(
                checks,
                "permissions.root_gitignore",
                CheckStatus::Warn,
                Some(format!(
                    "{} is not writable by owner (mode {:o}); `doctor --repair` will skip `.gitignore` repair until owner-write is restored",
                    gitignore.display(),
                    mode & 0o777
                )),
                Some(serde_json::json!({
                    "path": gitignore.display().to_string(),
                    "mode_octal": format!("{:o}", mode & 0o777),
                    "remediation": format!("`chmod u+w {}` or hand-edit before re-running `--repair`", gitignore.display()),
                })),
            );
            return;
        }
    }
    push_check(
        checks,
        "permissions.root_gitignore",
        CheckStatus::Ok,
        None,
        None,
    );
}

/// Pass-5 cycle 8: detector for `fm-caches_indexes-dirty-bitmap-divergence`.
///
/// The `dirty_issues` table tracks which issues need re-export. Its
/// schema declares an FK with `ON DELETE CASCADE` so orphans
/// shouldn't normally exist, but if FK enforcement was off when
/// issues were deleted (or a partial-rebuild path bypassed the FK
/// pragma), orphan rows can linger and cause incremental export
/// to attempt rewriting non-existent records. Auto-fixable via a
/// targeted chokepointed prune that snapshots matching rows first.
fn check_dirty_bitmap_divergence(conn: &Connection, checks: &mut Vec<CheckResult>) {
    let Ok(rows) = conn.query(
        "SELECT COUNT(*) FROM dirty_issues d LEFT JOIN issues i ON d.issue_id = i.id WHERE i.id IS NULL",
    ) else {
        // dirty_issues missing → upstream schema check covers it.
        push_check(checks, "dirty_bitmap", CheckStatus::Ok, None, None);
        return;
    };
    let orphan_count = rows
        .first()
        .and_then(|row| row.values().first().cloned())
        .and_then(|v| match v {
            SqliteValue::Integer(n) => Some(n),
            _ => None,
        })
        .unwrap_or(0);
    if orphan_count == 0 {
        push_check(checks, "dirty_bitmap", CheckStatus::Ok, None, None);
        return;
    }
    // Sample up to 5 orphan ids for the operator.
    let sample: Vec<String> = match conn.query(
        "SELECT d.issue_id FROM dirty_issues d LEFT JOIN issues i ON d.issue_id = i.id WHERE i.id IS NULL LIMIT 5",
    ) {
        Ok(rows) => rows
            .iter()
            .filter_map(|row| row.values().first().cloned())
            .filter_map(|v| match v {
                SqliteValue::Text(s) => Some(s.to_string()),
                _ => None,
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    push_check(
        checks,
        "dirty_bitmap",
        CheckStatus::Warn,
        Some(format!(
            "{orphan_count} orphan row(s) in dirty_issues — issue_id has no matching issues row (FK guard probably bypassed during a delete)"
        )),
        Some(serde_json::json!({
            "orphan_count": orphan_count,
            "sample_issue_ids": sample,
            "remediation": "br doctor --repair surgically prunes orphan dirty_issues rows via chokepointed DELETE (full JSONL rebuild remains the heavy hammer alternative)",
        })),
    );
}

/// Pass-5 cycle 29 — fixer for `fm-caches_indexes-dirty-bitmap-divergence`.
///
/// Surgically deletes orphan rows from `dirty_issues` (rows whose
/// `issue_id` no longer exists in `issues`) via
/// [`chokepoint::mutate(Op::DbExec)`]. This is a targeted alternative
/// to the existing `repair_database_from_jsonl` heavy hammer — the
/// chokepoint snapshots every row of `dirty_issues` matching the
/// orphan predicate before the DELETE fires, so `doctor undo` can
/// restore the orphan rows from snapshot.
///
/// Returns `true` if the DELETE succeeded.
const DIRTY_BITMAP_ORPHAN_PREDICATE: &str =
    "NOT EXISTS (SELECT 1 FROM issues WHERE issues.id = dirty_issues.issue_id)";

fn fix_dirty_bitmap_orphans_if_warned(
    db_path: &Path,
    report: &DoctorReport,
    ctx: &OutputContext,
    session: Option<&mut DoctorRepairSession>,
) -> bool {
    let has_warning = report
        .checks
        .iter()
        .any(|c| c.name == "dirty_bitmap" && c.status == CheckStatus::Warn);
    if !has_warning {
        return false;
    }
    let Some(session) = session else {
        if !ctx.is_json() {
            ctx.warning(
                "Skipping dirty-bitmap orphan prune: no doctor repair session (run-dir creation failed)",
            );
        }
        return false;
    };
    session.set_fixer("doctor.dirty_bitmap_orphan_prune");
    let op = Op::DbExec {
        sql: format!("DELETE FROM dirty_issues WHERE {DIRTY_BITMAP_ORPHAN_PREDICATE}"),
        args: Vec::new(),
        affected_tables: vec!["dirty_issues".to_string()],
        // The DELETE's WHERE clause; the chokepoint snapshots every row
        // that matches this predicate so `doctor undo` can restore them.
        affected_predicate: Some(DIRTY_BITMAP_ORPHAN_PREDICATE.to_string()),
    };
    match chokepoint::mutate(&session.ctx, db_path, op) {
        Ok(result) if result.ok => {
            if !ctx.is_json() {
                ctx.info("Pruned orphan rows from dirty_issues");
            }
            true
        }
        Ok(_) => false,
        Err(err) => {
            if !ctx.is_json() {
                ctx.warning(&format!("Failed to prune orphan dirty_issues rows: {err}"));
            }
            false
        }
    }
}

/// Pass-5 cycle 34: detector for `fm-caches_indexes-comments-orphans`.
///
/// The `comments` table declares `FOREIGN KEY (issue_id) REFERENCES
/// issues(id) ON DELETE CASCADE` so orphan rows shouldn't normally
/// exist, but if FK enforcement was off when issues were deleted (or
/// a partial-rebuild path bypassed the FK pragma) orphan comments
/// can linger and cause confusing dangling-reference exports.
///
/// Auto-fixable via [`fix_comments_orphans_if_warned`] below: mirrors
/// cycle 29's dirty-bitmap surgical-DELETE pattern through the
/// chokepoint.
fn check_comments_orphans(conn: &Connection, checks: &mut Vec<CheckResult>) {
    let Ok(rows) = conn.query(
        "SELECT COUNT(*) FROM comments c LEFT JOIN issues i ON c.issue_id = i.id WHERE i.id IS NULL",
    ) else {
        push_check(checks, "comments.orphans", CheckStatus::Ok, None, None);
        return;
    };
    let orphan_count = rows
        .first()
        .and_then(|row| row.values().first().cloned())
        .and_then(|v| match v {
            SqliteValue::Integer(n) => Some(n),
            _ => None,
        })
        .unwrap_or(0);
    if orphan_count == 0 {
        push_check(checks, "comments.orphans", CheckStatus::Ok, None, None);
        return;
    }
    let sample: Vec<String> = match conn.query(
        "SELECT c.issue_id FROM comments c LEFT JOIN issues i ON c.issue_id = i.id WHERE i.id IS NULL LIMIT 5",
    ) {
        Ok(rows) => rows
            .iter()
            .filter_map(|row| row.values().first().cloned())
            .filter_map(|v| match v {
                SqliteValue::Text(s) => Some(s.to_string()),
                _ => None,
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    push_check(
        checks,
        "comments.orphans",
        CheckStatus::Warn,
        Some(format!(
            "{orphan_count} orphan row(s) in comments - issue_id has no matching issues row (FK guard probably bypassed during a delete)"
        )),
        Some(serde_json::json!({
            "orphan_count": orphan_count,
            "sample_issue_ids": sample,
            "remediation": "br doctor --repair surgically prunes orphan comments rows via chokepointed DELETE",
        })),
    );
}

/// Pass-5 cycle 34: fixer for `fm-caches_indexes-comments-orphans`.
///
/// Surgically deletes orphan rows from `comments` (rows whose
/// `issue_id` no longer exists in `issues`) via
/// [`chokepoint::mutate(Op::DbExec)`]. Mirrors the dirty-bitmap
/// fixer from cycle 29 exactly: the chokepoint snapshots every row
/// matching the orphan predicate before the DELETE fires, so
/// `doctor undo` can restore the orphan rows from snapshot.
const COMMENTS_ORPHAN_PREDICATE: &str =
    "NOT EXISTS (SELECT 1 FROM issues WHERE issues.id = comments.issue_id)";

fn fix_comments_orphans_if_warned(
    db_path: &Path,
    report: &DoctorReport,
    ctx: &OutputContext,
    session: Option<&mut DoctorRepairSession>,
) -> bool {
    let has_warning = report
        .checks
        .iter()
        .any(|c| c.name == "comments.orphans" && c.status == CheckStatus::Warn);
    if !has_warning {
        return false;
    }
    let Some(session) = session else {
        if !ctx.is_json() {
            ctx.warning(
                "Skipping comments-orphan prune: no doctor repair session (run-dir creation failed)",
            );
        }
        return false;
    };
    session.set_fixer("doctor.comments_orphan_prune");
    let op = Op::DbExec {
        sql: format!("DELETE FROM comments WHERE {COMMENTS_ORPHAN_PREDICATE}"),
        args: Vec::new(),
        affected_tables: vec!["comments".to_string()],
        affected_predicate: Some(COMMENTS_ORPHAN_PREDICATE.to_string()),
    };
    match chokepoint::mutate(&session.ctx, db_path, op) {
        Ok(result) if result.ok => {
            if !ctx.is_json() {
                ctx.info("Pruned orphan rows from comments");
            }
            true
        }
        Ok(_) => false,
        Err(err) => {
            if !ctx.is_json() {
                ctx.warning(&format!("Failed to prune orphan comments rows: {err}"));
            }
            false
        }
    }
}

/// Pass-5 cycle 35 — detector for `fm-caches_indexes-labels-orphans`.
///
/// The `labels` table declares `FOREIGN KEY (issue_id) REFERENCES
/// issues(id) ON DELETE CASCADE` so orphan rows shouldn't normally
/// exist, but if FK enforcement was off when issues were deleted (or
/// a partial-rebuild path bypassed the FK pragma) orphan label
/// assignments can linger and cause confusing dangling-reference
/// exports or `br ready --label` mis-filtering.
///
/// Auto-fixable via [`fix_labels_orphans_if_warned`] below: mirrors
/// cycle 29's dirty-bitmap surgical-DELETE pattern through the
/// chokepoint.
fn check_labels_orphans(conn: &Connection, checks: &mut Vec<CheckResult>) {
    let Ok(rows) = conn.query(
        "SELECT COUNT(*) FROM labels l LEFT JOIN issues i ON l.issue_id = i.id WHERE i.id IS NULL",
    ) else {
        push_check(checks, "labels.orphans", CheckStatus::Ok, None, None);
        return;
    };
    let orphan_count = rows
        .first()
        .and_then(|row| row.values().first().cloned())
        .and_then(|v| match v {
            SqliteValue::Integer(n) => Some(n),
            _ => None,
        })
        .unwrap_or(0);
    if orphan_count == 0 {
        push_check(checks, "labels.orphans", CheckStatus::Ok, None, None);
        return;
    }
    let sample: Vec<String> = match conn.query(
        "SELECT l.issue_id FROM labels l LEFT JOIN issues i ON l.issue_id = i.id WHERE i.id IS NULL LIMIT 5",
    ) {
        Ok(rows) => rows
            .iter()
            .filter_map(|row| row.values().first().cloned())
            .filter_map(|v| match v {
                SqliteValue::Text(s) => Some(s.to_string()),
                _ => None,
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    push_check(
        checks,
        "labels.orphans",
        CheckStatus::Warn,
        Some(format!(
            "{orphan_count} orphan row(s) in labels — issue_id has no matching issues row (FK guard probably bypassed during a delete)"
        )),
        Some(serde_json::json!({
            "orphan_count": orphan_count,
            "sample_issue_ids": sample,
            "remediation": "br doctor --repair surgically prunes orphan labels rows via chokepointed DELETE",
        })),
    );
}

/// Pass-5 cycle 35 — fixer for `fm-caches_indexes-labels-orphans`.
///
/// Surgically deletes orphan rows from `labels` (rows whose
/// `issue_id` no longer exists in `issues`) via
/// [`chokepoint::mutate(Op::DbExec)`]. Mirrors the dirty-bitmap and
/// comments fixers exactly: chokepoint snapshots every orphan row
/// before the DELETE fires, so `doctor undo` restores them from
/// snapshot via `restore_db_exec`.
const LABELS_ORPHAN_PREDICATE: &str =
    "NOT EXISTS (SELECT 1 FROM issues WHERE issues.id = labels.issue_id)";

fn fix_labels_orphans_if_warned(
    db_path: &Path,
    report: &DoctorReport,
    ctx: &OutputContext,
    session: Option<&mut DoctorRepairSession>,
) -> bool {
    let has_warning = report
        .checks
        .iter()
        .any(|c| c.name == "labels.orphans" && c.status == CheckStatus::Warn);
    if !has_warning {
        return false;
    }
    let Some(session) = session else {
        if !ctx.is_json() {
            ctx.warning(
                "Skipping labels-orphan prune: no doctor repair session (run-dir creation failed)",
            );
        }
        return false;
    };
    session.set_fixer("doctor.labels_orphan_prune");
    let op = Op::DbExec {
        sql: format!("DELETE FROM labels WHERE {LABELS_ORPHAN_PREDICATE}"),
        args: Vec::new(),
        affected_tables: vec!["labels".to_string()],
        affected_predicate: Some(LABELS_ORPHAN_PREDICATE.to_string()),
    };
    match chokepoint::mutate(&session.ctx, db_path, op) {
        Ok(result) if result.ok => {
            if !ctx.is_json() {
                ctx.info("Pruned orphan rows from labels");
            }
            true
        }
        Ok(_) => false,
        Err(err) => {
            if !ctx.is_json() {
                ctx.warning(&format!("Failed to prune orphan labels rows: {err}"));
            }
            false
        }
    }
}

/// Pass-5 cycle 36 — detector for `fm-caches_indexes-dependencies-orphans`.
///
/// The `dependencies` table declares `FOREIGN KEY (issue_id) REFERENCES
/// issues(id) ON DELETE CASCADE` so orphan rows on the issue_id side
/// shouldn't normally exist. The depends_on_id column intentionally
/// has NO FK so it can reference external issues (cross-repo deps),
/// but non-`external:` targets are still local issue references and
/// must exist.
///
/// Auto-fixable via [`fix_dependencies_orphans_if_warned`] below:
/// mirrors cycle 29/34/35's surgical-DELETE pattern through the
/// chokepoint.
const DEPENDENCIES_ORPHAN_PREDICATE: &str = "issue_id NOT IN (SELECT id FROM issues) \
     OR (depends_on_id NOT LIKE 'external:%' \
         AND depends_on_id NOT IN (SELECT id FROM issues))";

fn check_dependencies_orphans(conn: &Connection, checks: &mut Vec<CheckResult>) {
    let Ok(rows) = conn.query(&format!(
        "SELECT COUNT(*) FROM dependencies WHERE {DEPENDENCIES_ORPHAN_PREDICATE}"
    )) else {
        push_check(checks, "dependencies.orphans", CheckStatus::Ok, None, None);
        return;
    };
    let orphan_count = rows
        .first()
        .and_then(|row| row.values().first().cloned())
        .and_then(|v| match v {
            SqliteValue::Integer(n) => Some(n),
            _ => None,
        })
        .unwrap_or(0);
    if orphan_count == 0 {
        push_check(checks, "dependencies.orphans", CheckStatus::Ok, None, None);
        return;
    }
    let sample: Vec<String> = match conn.query(&format!(
        "SELECT issue_id, depends_on_id FROM dependencies \
         WHERE {DEPENDENCIES_ORPHAN_PREDICATE} \
         ORDER BY issue_id, depends_on_id \
         LIMIT 5"
    )) {
        Ok(rows) => rows
            .iter()
            .filter_map(|row| {
                let issue_id = row.values().first().and_then(|v| match v {
                    SqliteValue::Text(s) => Some(s.as_str()),
                    _ => None,
                })?;
                let depends_on_id = row.values().get(1).and_then(|v| match v {
                    SqliteValue::Text(s) => Some(s.as_str()),
                    _ => None,
                })?;
                Some(format!("{issue_id} -> {depends_on_id}"))
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    push_check(
        checks,
        "dependencies.orphans",
        CheckStatus::Warn,
        Some(format!(
            "{orphan_count} orphan row(s) in dependencies — issue_id or non-external depends_on_id has no matching issues row"
        )),
        Some(serde_json::json!({
            "orphan_count": orphan_count,
            "sample_edges": sample,
            "remediation": "br doctor --repair surgically prunes orphan dependencies rows via chokepointed DELETE (external depends_on_id refs are preserved)",
        })),
    );
}

/// Pass-5 cycle 36 — fixer for `fm-caches_indexes-dependencies-orphans`.
///
/// Surgically deletes orphan rows from `dependencies` (rows whose
/// `issue_id` no longer exists in `issues`, or whose non-external
/// `depends_on_id` no longer exists) via [`chokepoint::mutate(Op::DbExec)`].
/// Rows whose `depends_on_id` points to an `external:*` issue are
/// preserved — the schema intentionally omits an FK on that column
/// for cross-repo references.
fn fix_dependencies_orphans_if_warned(
    db_path: &Path,
    report: &DoctorReport,
    ctx: &OutputContext,
    session: Option<&mut DoctorRepairSession>,
) -> bool {
    let has_warning = report
        .checks
        .iter()
        .any(|c| c.name == "dependencies.orphans" && c.status == CheckStatus::Warn);
    if !has_warning {
        return false;
    }
    let Some(session) = session else {
        if !ctx.is_json() {
            ctx.warning(
                "Skipping dependencies-orphan prune: no doctor repair session (run-dir creation failed)",
            );
        }
        return false;
    };
    session.set_fixer("doctor.dependencies_orphan_prune");
    let op = Op::DbExec {
        sql: format!("DELETE FROM dependencies WHERE {DEPENDENCIES_ORPHAN_PREDICATE}"),
        args: Vec::new(),
        affected_tables: vec!["dependencies".to_string()],
        affected_predicate: Some(DEPENDENCIES_ORPHAN_PREDICATE.to_string()),
    };
    match chokepoint::mutate(&session.ctx, db_path, op) {
        Ok(result) if result.ok => {
            if !ctx.is_json() {
                ctx.info("Pruned orphan rows from dependencies");
            }
            true
        }
        Ok(_) => false,
        Err(err) => {
            if !ctx.is_json() {
                ctx.warning(&format!("Failed to prune orphan dependencies rows: {err}"));
            }
            false
        }
    }
}

fn push_recoverable_anomalies_check(checks: &mut Vec<CheckResult>, findings: &[String]) {
    if findings.is_empty() {
        push_check(
            checks,
            "db.recoverable_anomalies",
            CheckStatus::Ok,
            None,
            None,
        );
    } else if findings
        .iter()
        .all(|finding| blocked_cache_rebuild_finding(finding))
    {
        push_check(
            checks,
            "db.recoverable_anomalies",
            CheckStatus::Warn,
            Some(findings[0].clone()),
            Some(serde_json::json!({ "findings": findings })),
        );
    } else {
        push_check(
            checks,
            "db.recoverable_anomalies",
            CheckStatus::Error,
            Some(findings[0].clone()),
            Some(serde_json::json!({ "findings": findings })),
        );
    }
}

/// beads_rust-m3mi: flag closed beads whose `close_reason` matches a
/// well-known audit-suspect pattern (e.g. "Forced close due to cycle"),
/// unless the bead carries an `audit-historical-cycle-close-YYYY-MM-DD` label
/// which acts as the explicit triage escape hatch.
///
/// Default patterns are defined inline; documented allowlist (substring
/// matches that should never be flagged) skips legitimate close-reasons
/// like "auto-closed by doctor" or "merged into".
///
/// Severity: Warn — these are NOT broken DB states; just audit-suspect
/// closures that deserve operator attention.
const HISTORICAL_CYCLE_CLOSE_LABEL_PREFIX: &str = "audit-historical-cycle-close-";

fn is_historical_cycle_close_label(label: &str) -> bool {
    let Some(date) = label
        .trim()
        .strip_prefix(HISTORICAL_CYCLE_CLOSE_LABEL_PREFIX)
    else {
        return false;
    };

    let bytes = date.as_bytes();
    let has_date_shape = bytes.len() == 10
        && bytes[0].is_ascii_digit()
        && bytes[1].is_ascii_digit()
        && bytes[2].is_ascii_digit()
        && bytes[3].is_ascii_digit()
        && bytes[4] == b'-'
        && bytes[5].is_ascii_digit()
        && bytes[6].is_ascii_digit()
        && bytes[7] == b'-'
        && bytes[8].is_ascii_digit()
        && bytes[9].is_ascii_digit();
    has_date_shape && NaiveDate::parse_from_str(date, "%Y-%m-%d").is_ok()
}

fn check_suspect_close_reasons(conn: &Connection, checks: &mut Vec<CheckResult>) {
    // Default patterns: (case-insensitive substring matches; lowercase form below)
    let default_patterns: &[&str] = &[
        "forced close due to cycle",
        "due to dep cycle",
        "due to dependency cycle",
        "temporarily closed",
        "wip close",
    ];
    // Default allowlist: substrings that must never be flagged
    let default_allowlist: &[&str] = &[
        "auto-closed by doctor",
        "closed by epic close-eligible",
        "merged into",
        "superseded by",
    ];

    // Query closed beads' id, close_reason, labels (joined as ASCII-unit-
    // separated, since `br update --add-label` rejects this character so
    // it can never appear inside a label and is a safe split delimiter).
    // GROUP_CONCAT separator can't contain commas in case a label ever
    // does — the validator forbids commas today, but we don't want to
    // create a latent bug if the schema ever changes.
    let rows = match conn.query(
        "SELECT i.id, i.close_reason,
                COALESCE(GROUP_CONCAT(l.label, char(31)), '') AS labels
         FROM issues i
         LEFT JOIN labels l ON l.issue_id = i.id
         WHERE i.status = 'closed' AND i.close_reason IS NOT NULL
         GROUP BY i.id
         ORDER BY i.id",
    ) {
        Ok(rows) => rows,
        Err(err) => {
            push_check(
                checks,
                "audit.suspect_close_reasons",
                CheckStatus::Warn,
                Some(format!(
                    "Failed to query closed beads for close_reason audit: {err}"
                )),
                None,
            );
            return;
        }
    };

    let mut matches: Vec<serde_json::Value> = Vec::new();
    for row in rows {
        let id = row
            .get(0)
            .and_then(SqliteValue::as_text)
            .unwrap_or("")
            .to_string();
        let reason = row
            .get(1)
            .and_then(SqliteValue::as_text)
            .unwrap_or("")
            .to_string();
        let labels = row
            .get(2)
            .and_then(SqliteValue::as_text)
            .unwrap_or("")
            .to_string();
        if id.is_empty() || reason.is_empty() {
            continue;
        }

        // Skip only if a label is the documented dated historical-triage marker.
        // Split on ASCII unit separator (matches the GROUP_CONCAT separator
        // above; safe even if a label ever contained a comma).
        let has_historical_label = labels.split('\x1f').any(is_historical_cycle_close_label);
        if has_historical_label {
            continue;
        }

        let reason_lower = reason.to_lowercase();

        // Skip if any allowlist substring matches
        if default_allowlist.iter().any(|a| reason_lower.contains(a)) {
            continue;
        }

        // Match against suspect patterns
        if let Some(matched) = default_patterns.iter().find(|p| reason_lower.contains(*p)) {
            matches.push(serde_json::json!({
                "bead_id": id,
                "matched_pattern": matched,
                "close_reason": reason,
                "has_historical_label": false,
            }));
        }
    }

    if matches.is_empty() {
        push_check(
            checks,
            "audit.suspect_close_reasons",
            CheckStatus::Ok,
            None,
            None,
        );
        return;
    }

    let count = matches.len();
    push_check(
        checks,
        "audit.suspect_close_reasons",
        CheckStatus::Warn,
        Some(format!(
            "{count} closed bead(s) have audit-suspect close_reason text without an audit-historical-cycle-close-<YYYY-MM-DD> escape-hatch label"
        )),
        Some(serde_json::json!({
            "patterns_used": default_patterns,
            "allowlist_used": default_allowlist,
            "matches": matches,
        })),
    );
}

/// Detector (issue #311): flag live issues whose current `status` is not in
/// the project's configured strict `workflow.statuses` set. Only runs when
/// `.beads/policy.yaml` configures `workflow.strict: true` with a non-empty
/// status set; otherwise it is a silent no-op (no check emitted at all) so
/// repos without the policy see no new doctor output.
///
/// Non-conforming statuses can exist even with enforcement on the write path
/// — e.g. statuses written before the policy was added, or imported from an
/// external source — so this read-side audit closes the gap.
fn check_workflow_statuses(conn: &Connection, beads_dir: &Path, checks: &mut Vec<CheckResult>) {
    let policy = match crate::close_policy::load_for_beads_dir(beads_dir) {
        Ok(policy) => policy,
        Err(err) => {
            push_check(
                checks,
                "policy.workflow_statuses",
                CheckStatus::Warn,
                Some(format!(
                    "Failed to load .beads/policy.yaml for workflow-status audit: {err}"
                )),
                None,
            );
            return;
        }
    };
    let workflow = &policy.workflow;
    if !workflow.is_enforced() {
        // No strict workflow configured — stay silent.
        return;
    }

    let rows =
        match conn.query("SELECT id, status FROM issues WHERE status IS NOT NULL ORDER BY id") {
            Ok(rows) => rows,
            Err(err) => {
                push_check(
                    checks,
                    "policy.workflow_statuses",
                    CheckStatus::Warn,
                    Some(format!(
                        "Failed to query issue statuses for workflow audit: {err}"
                    )),
                    None,
                );
                return;
            }
        };

    let mut offenders: Vec<serde_json::Value> = Vec::new();
    for row in rows {
        let id = row
            .get(0)
            .and_then(SqliteValue::as_text)
            .unwrap_or("")
            .to_string();
        let status = row
            .get(1)
            .and_then(SqliteValue::as_text)
            .unwrap_or("")
            .to_string();
        if id.is_empty() || status.is_empty() {
            continue;
        }
        if !workflow.allows(&status) {
            offenders.push(serde_json::json!({
                "bead_id": id,
                "status": status,
            }));
        }
    }

    if offenders.is_empty() {
        push_check(
            checks,
            "policy.workflow_statuses",
            CheckStatus::Ok,
            None,
            None,
        );
        return;
    }

    let count = offenders.len();
    push_check(
        checks,
        "policy.workflow_statuses",
        CheckStatus::Warn,
        Some(format!(
            "{count} issue(s) have a status outside the strict workflow set ({}). \
             Update each with `br update <id> --status <allowed>`.",
            workflow.allowed_list()
        )),
        Some(serde_json::json!({
            "allowed_statuses": workflow.statuses,
            "offenders": offenders,
        })),
    );
}

fn check_recoverable_anomalies(conn: &Connection, checks: &mut Vec<CheckResult>) -> Result<()> {
    let duplicate_schema_rows = conn.query(
        "SELECT type, name, COUNT(*) AS row_count
         FROM sqlite_master
         WHERE name IN ('blocked_issues_cache', 'idx_blocked_cache_blocked_at')
         GROUP BY type, name
         HAVING COUNT(*) > 1
         ORDER BY row_count DESC, name ASC
         LIMIT 1",
    )?;

    let duplicate_config = conn.query(
        "SELECT key, COUNT(*) AS row_count
         FROM config
         GROUP BY key
         HAVING COUNT(*) > 1
         ORDER BY row_count DESC, key ASC
         LIMIT 1",
    )?;

    let duplicate_metadata = conn.query(
        "SELECT key, COUNT(*) AS row_count
         FROM metadata
         GROUP BY key
         HAVING COUNT(*) > 1
         ORDER BY row_count DESC, key ASC
         LIMIT 1",
    )?;

    let mut findings = Vec::new();

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
        findings.push(format!(
            "sqlite_master contains duplicate {object_type} entries for '{name}' ({row_count} rows)"
        ));
    }

    if let Some(row) = duplicate_config.first() {
        let key = row
            .get(0)
            .and_then(SqliteValue::as_text)
            .unwrap_or("unknown");
        let row_count = row.get(1).and_then(SqliteValue::as_integer).unwrap_or(2);
        findings.push(format!(
            "config contains duplicate rows for key '{key}' ({row_count} rows)"
        ));
    }

    if let Some(row) = duplicate_metadata.first() {
        let key = row
            .get(0)
            .and_then(SqliteValue::as_text)
            .unwrap_or("unknown");
        let row_count = row.get(1).and_then(SqliteValue::as_integer).unwrap_or(2);
        findings.push(format!(
            "metadata contains duplicate rows for key '{key}' ({row_count} rows)"
        ));
    }

    if latest_metadata_value(conn, "blocked_cache_state").as_deref() == Some("stale") {
        findings.push(BLOCKED_CACHE_STALE_FINDING.to_string());
    }
    let blocked_cache_health = SqliteStorage::blocked_cache_projection_health(conn);
    if blocked_cache_health.has_mismatch() {
        findings.push(BLOCKED_CACHE_CONTENT_MISMATCH_FINDING.to_string());
    }
    let ready_projection_health = SqliteStorage::ready_projection_health(conn);
    if ready_projection_health.has_mismatch() {
        findings.push(READY_PROJECTION_CONTENT_MISMATCH_FINDING.to_string());
    }

    push_recoverable_anomalies_check(checks, &findings);
    Ok(())
}

/// (table, column, fix_sql) tuples for every NOT NULL DEFAULT column that
/// can hold a storage-class NULL on legacy databases. Mirrors the schema
/// bootstrap's v8 migration in `backfill_storage_null_in_default_columns`
/// — a doctor warning here means a NULL appeared *after* migration ran
/// (issue #269 covers the legacy backfill, #177 the typeof-vs-IS-NULL
/// rationale).
const NULL_DEFAULT_CHECKS: &[(&str, &str, &str)] = &[
    // issues
    (
        "issues",
        "description",
        "UPDATE issues SET description = '' WHERE typeof(description) = 'null'",
    ),
    (
        "issues",
        "design",
        "UPDATE issues SET design = '' WHERE typeof(design) = 'null'",
    ),
    (
        "issues",
        "acceptance_criteria",
        "UPDATE issues SET acceptance_criteria = '' WHERE typeof(acceptance_criteria) = 'null'",
    ),
    (
        "issues",
        "notes",
        "UPDATE issues SET notes = '' WHERE typeof(notes) = 'null'",
    ),
    (
        "issues",
        "status",
        "UPDATE issues SET status = 'open' WHERE typeof(status) = 'null'",
    ),
    (
        "issues",
        "priority",
        "UPDATE issues SET priority = 2 WHERE typeof(priority) = 'null'",
    ),
    (
        "issues",
        "issue_type",
        "UPDATE issues SET issue_type = 'task' WHERE typeof(issue_type) = 'null'",
    ),
    (
        "issues",
        "source_repo",
        "UPDATE issues SET source_repo = '.' WHERE typeof(source_repo) = 'null'",
    ),
    (
        "issues",
        "ephemeral",
        "UPDATE issues SET ephemeral = 0 WHERE typeof(ephemeral) = 'null'",
    ),
    (
        "issues",
        "pinned",
        "UPDATE issues SET pinned = 0 WHERE typeof(pinned) = 'null'",
    ),
    (
        "issues",
        "is_template",
        "UPDATE issues SET is_template = 0 WHERE typeof(is_template) = 'null'",
    ),
    // dependencies
    (
        "dependencies",
        "type",
        "UPDATE dependencies SET type = 'blocks' WHERE typeof(type) = 'null'",
    ),
    (
        "dependencies",
        "created_by",
        "UPDATE dependencies SET created_by = '' WHERE typeof(created_by) = 'null'",
    ),
    // comments
    (
        "comments",
        "author",
        "UPDATE comments SET author = '' WHERE typeof(author) = 'null'",
    ),
    (
        "comments",
        "text",
        "UPDATE comments SET text = '' WHERE typeof(text) = 'null'",
    ),
    (
        "comments",
        "created_at",
        "UPDATE comments SET created_at = CURRENT_TIMESTAMP WHERE typeof(created_at) = 'null'",
    ),
    // events
    (
        "events",
        "event_type",
        "UPDATE events SET event_type = '' WHERE typeof(event_type) = 'null'",
    ),
    (
        "events",
        "actor",
        "UPDATE events SET actor = '' WHERE typeof(actor) = 'null'",
    ),
    (
        "events",
        "created_at",
        "UPDATE events SET created_at = CURRENT_TIMESTAMP WHERE typeof(created_at) = 'null'",
    ),
];

/// Check for NULL values in NOT NULL columns that should have DEFAULTs.
///
/// Detects rows inserted before the `DEFAULT ''` was added to the schema
/// (e.g., events.actor or comments.author without DEFAULT).
fn check_null_defaults(conn: &Connection, checks: &mut Vec<CheckResult>) {
    // NOTE: We use typeof(column) = 'null' instead of column IS NULL because
    // SQLite's query planner can use partial indexes (e.g., WHERE actor != '')
    // that cause IS NULL predicates to silently return 0 rows even when NULLs
    // exist in the table.  typeof() bypasses the index and checks the actual
    // storage class.  See issue #177 for details.
    let queries: &[(&str, &str, &str)] = NULL_DEFAULT_CHECKS;

    let mut null_findings = Vec::new();

    for (table, column, fix_sql) in queries {
        let count_sql = format!("SELECT COUNT(*) FROM {table} WHERE typeof({column}) = 'null'");
        if let Ok(row) = conn.query_row(&count_sql) {
            let count = row.get(0).and_then(SqliteValue::as_integer).unwrap_or(0);
            if count > 0 {
                null_findings.push(serde_json::json!({
                    "table": table,
                    "column": column,
                    "null_count": count,
                    "fix_sql": fix_sql,
                }));
            }
        }
        // Err case: table might not exist yet; skip silently
    }

    if null_findings.is_empty() {
        push_check(checks, "db.null_defaults", CheckStatus::Ok, None, None);
    } else {
        let first = &null_findings[0];
        let table = first["table"].as_str().unwrap_or("?");
        let column = first["column"].as_str().unwrap_or("?");
        let count = first["null_count"].as_i64().unwrap_or(0);
        push_check(
            checks,
            "db.null_defaults",
            CheckStatus::Warn,
            Some(format!(
                "{table}.{column} has {count} NULL value(s); fix with: {}",
                first["fix_sql"].as_str().unwrap_or("see details")
            )),
            Some(serde_json::json!({ "findings": null_findings })),
        );
    }
}

/// Pass-8 cycle 61 — fixer for the NULL-defaults subset of
/// `fm-schemas-missing-required-column`.
///
/// When `check_null_defaults` warns, backfills each flagged NOT-NULL
/// column's schema-declared DEFAULT into its NULL slots via
/// [`chokepoint::mutate(Op::DbExec)`], one predicate-scoped UPDATE per
/// (table, column). Legacy rows inserted before a `DEFAULT` was added to
/// the schema carry NULLs that violate the NOT-NULL invariant and corrupt
/// JSONL export; the repair is unambiguous (there is exactly one correct
/// value — the column's DEFAULT) and data-corrective.
///
/// SAFETY / blast radius: each UPDATE is scoped by `typeof(<col>) =
/// 'null'`, so only the offending rows are touched, and the chokepoint
/// snapshots exactly those rows (predicate-matched) before the write so
/// `doctor undo` restores the NULLs. The SQL is taken from the vetted
/// `NULL_DEFAULT_CHECKS` constant, never from the report — a tampered
/// report can only select WHICH vetted (table, column) pairs to act on,
/// not inject SQL.
///
/// Lifts the NULL-defaults shape from Tier B to Tier A. The sibling
/// missing-column shape of the same FM stays detect-only (adding a column
/// requires a migration, not a backfill).
fn fix_null_defaults_if_warned(
    db_path: &Path,
    report: &DoctorReport,
    ctx: &OutputContext,
    session: Option<&mut DoctorRepairSession>,
) -> bool {
    let Some(finding) = report
        .checks
        .iter()
        .find(|c| c.name == "db.null_defaults" && c.status == CheckStatus::Warn)
    else {
        return false;
    };
    let Some(session) = session else {
        if !ctx.is_json() {
            ctx.warning(
                "Skipping null-defaults backfill: no doctor repair session (run-dir creation failed)",
            );
        }
        return false;
    };
    // The (table, column) pairs the detector flagged. We act only on these,
    // and only with vetted SQL from NULL_DEFAULT_CHECKS.
    let flagged: Vec<(String, String)> = finding
        .details
        .as_ref()
        .and_then(|d| d.get("findings"))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|f| {
                    Some((
                        f.get("table")?.as_str()?.to_string(),
                        f.get("column")?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    if flagged.is_empty() {
        return false;
    }
    session.set_fixer("doctor.null_defaults_backfill");
    let mut any = false;
    for &(table, column, fix_sql) in NULL_DEFAULT_CHECKS {
        if !flagged
            .iter()
            .any(|(t, c)| t.as_str() == table && c.as_str() == column)
        {
            continue;
        }
        let predicate = format!("typeof({column}) = 'null'");
        let op = Op::DbExec {
            sql: fix_sql.to_string(),
            args: Vec::new(),
            affected_tables: vec![table.to_string()],
            affected_predicate: Some(predicate),
        };
        match chokepoint::mutate(&session.ctx, db_path, op) {
            Ok(result) if result.ok => any = true,
            Ok(_) => {}
            Err(err) => {
                if !ctx.is_json() {
                    ctx.warning(&format!(
                        "Failed to backfill {table}.{column} default: {err}"
                    ));
                }
            }
        }
    }
    if any && !ctx.is_json() {
        ctx.info("Backfilled schema defaults into NULL columns");
    }
    any
}

fn check_issue_write_probe(conn: &Connection, checks: &mut Vec<CheckResult>) {
    let issue_id = match conn.query_row("SELECT id FROM issues ORDER BY id LIMIT 1") {
        Ok(row) => row
            .get(0)
            .and_then(SqliteValue::as_text)
            .map(ToString::to_string),
        Err(FrankenError::QueryReturnedNoRows) => None,
        Err(err) => {
            push_check(
                checks,
                "db.write_probe",
                CheckStatus::Error,
                Some(format!("Failed to select probe issue: {err}")),
                None,
            );
            return;
        }
    };

    let Some(issue_id) = issue_id else {
        push_check(
            checks,
            "db.write_probe",
            CheckStatus::Ok,
            Some("No issues available for rollback-only write probe".to_string()),
            None,
        );
        return;
    };

    let begin_result = conn.execute("BEGIN IMMEDIATE");
    if let Err(err) = begin_result {
        let status = if err.is_transient() {
            CheckStatus::Warn
        } else {
            CheckStatus::Error
        };
        push_check(
            checks,
            "db.write_probe",
            status,
            Some(format!("Failed to begin rollback-only write probe: {err}")),
            Some(serde_json::json!({ "issue_id": issue_id })),
        );
        return;
    }

    let update_result = conn.execute_with_params(
        "UPDATE issues SET priority = priority, status = status WHERE id = ?",
        &[SqliteValue::from(issue_id.as_str())],
    );
    let rollback_result = conn.execute("ROLLBACK");

    checks.push(build_issue_write_probe_check(
        &issue_id,
        update_result,
        rollback_result,
    ));
}

fn sqlite_readonly_immutable_uri(db_path: &Path) -> String {
    use std::fmt::Write as _;

    let absolute_path = if db_path.is_absolute() {
        db_path.to_path_buf()
    } else {
        std::env::current_dir().map_or_else(|_| db_path.to_path_buf(), |cwd| cwd.join(db_path))
    };
    let path = absolute_path.to_string_lossy();
    let mut uri = String::with_capacity(path.len() + 32);
    uri.push_str("file:");
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'.' | b'_' | b'-' | b'~' => {
                uri.push(char::from(byte));
            }
            #[cfg(windows)]
            b'\\' => uri.push('/'),
            #[cfg(windows)]
            b':' => uri.push(':'),
            _ => {
                let _ = write!(&mut uri, "%{byte:02X}");
            }
        }
    }
    uri.push_str("?mode=ro&immutable=1");
    uri
}

fn sqlite_cli_integrity_messages(db_path: &Path) -> Result<Vec<String>> {
    let output = Command::new("sqlite3")
        .arg(sqlite_readonly_immutable_uri(db_path))
        .arg("PRAGMA integrity_check;")
        .output()
        .map_err(|err| BeadsError::Config(format!("failed to run sqlite3: {err}")))?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let mut messages: Vec<String> = stdout
        .lines()
        .chain(stderr.lines())
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect();

    if messages.is_empty() && !output.status.success() {
        messages.push(format!(
            "sqlite3 exited with status {}",
            output.status.code().unwrap_or(-1)
        ));
    }

    if output.status.success() {
        Ok(messages)
    } else {
        Err(BeadsError::Config(messages.join("; ")))
    }
}

fn check_sqlite_cli_integrity(db_path: &Path, checks: &mut Vec<CheckResult>) {
    match sqlite_cli_integrity_messages(db_path) {
        Ok(messages) if messages.len() == 1 && messages[0].eq_ignore_ascii_case("ok") => {
            push_check(
                checks,
                "sqlite3.integrity_check",
                CheckStatus::Ok,
                None,
                None,
            );
        }
        Ok(messages) if integrity_messages_only_benign(&messages) => {
            // Treat never-used page notices and partial-index row mismatches as warnings
            // (known frankensqlite artifacts).
            push_check(
                checks,
                "sqlite3.integrity_check",
                CheckStatus::Warn,
                Some(messages.join("; ")),
                (messages.len() > 1).then(|| serde_json::json!({ "messages": messages })),
            );
        }
        Ok(messages) => {
            push_check(
                checks,
                "sqlite3.integrity_check",
                CheckStatus::Error,
                Some(messages.join("; ")),
                (messages.len() > 1).then(|| serde_json::json!({ "messages": messages })),
            );
        }
        Err(BeadsError::Config(message))
            if message.contains("No such file or directory")
                || message.contains("failed to run sqlite3") =>
        {
            // An absent sqlite3 CLI is a benign host state, not a workspace
            // defect: the built-in fsqlite integrity check above still ran.
            // Warn here would pin `doctor.ok = false` (`!has_non_ok`, #292)
            // on every machine without sqlite3 in PATH — the same
            // benign-state-reported-as-warn class as #432.
            push_check(
                checks,
                "sqlite3.integrity_check",
                CheckStatus::Ok,
                Some("sqlite3 not available; skipping orthogonal integrity validation".to_string()),
                None,
            );
        }
        Err(err) => {
            push_check(
                checks,
                "sqlite3.integrity_check",
                CheckStatus::Error,
                Some(err.to_string()),
                None,
            );
        }
    }
}

fn integrity_check_messages(rows: &[Vec<SqliteValue>]) -> Vec<String> {
    let mut messages = Vec::new();
    for row in rows {
        for value in row {
            if let Some(text) = value.as_text() {
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    messages.push(trimmed.to_string());
                }
            }
        }
    }

    if messages.is_empty() {
        messages.push("integrity_check returned no diagnostic rows".to_string());
    }

    messages
}

/// Classify a stuck merge artifact filename for the `jsonl.merge_artifacts`
/// details payload (beads_rust#344/#328): `.left.jsonl` / `.right.jsonl`
/// variants are the local/remote halves of an interrupted `br sync --merge`;
/// any other `.base.jsonl` variant (the canonical `beads.base.jsonl` anchor
/// is excluded before this runs) is a stale base snapshot.
fn merge_artifact_kind(name: &str) -> &'static str {
    if name.contains(".left.jsonl") {
        "merge-left"
    } else if name.contains(".right.jsonl") {
        "merge-right"
    } else {
        "stale-base-variant"
    }
}

fn check_merge_artifacts(
    beads_dir: &Path,
    canonical_jsonl: &Path,
    checks: &mut Vec<CheckResult>,
) -> Result<()> {
    let mut files = Vec::new();
    let mut artifacts = Vec::new();
    for entry in beads_dir.read_dir()? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        // `beads.base.jsonl` is the canonical 3-way merge anchor written
        // by `br sync --merge` (see sync/mod.rs:5035). It is NOT a stuck
        // temp artifact — the fixer and detector must both treat it as
        // protected so a clean workspace doesn't get falsely flagged.
        if name == "beads.base.jsonl" {
            continue;
        }
        if name.contains(".base.jsonl")
            || name.contains(".left.jsonl")
            || name.contains(".right.jsonl")
        {
            // beads_rust#344/#328: carry per-file classification plus a
            // cheap conflict-marker scan so agents can distinguish a
            // benign leftover from a half-merged file without opening it.
            let path = entry.path();
            artifacts.push(serde_json::json!({
                "path": path.display().to_string(),
                "artifact_kind": merge_artifact_kind(name),
                "conflict_markers_found": crate::health::jsonl_has_conflict_markers(&path),
            }));
            files.push(name.to_string());
        }
    }

    if artifacts.is_empty() {
        push_check(checks, "jsonl.merge_artifacts", CheckStatus::Ok, None, None);
    } else {
        push_check(
            checks,
            "jsonl.merge_artifacts",
            CheckStatus::Warn,
            Some("Merge artifacts detected in .beads/".to_string()),
            Some(serde_json::json!({
                // Back-compat: existing consumers key on `files`.
                "files": files,
                "artifacts": artifacts,
                "canonical_jsonl": canonical_jsonl.display().to_string(),
                "recovery": [
                    { "kind": "quarantine", "command": "br doctor --repair" },
                    { "kind": "undo", "command": "br doctor undo <run-id>" },
                ],
            })),
        );
    }
    Ok(())
}

/// Pass-5 cycle 4: detector for `fm-state_files-base-jsonl-missing-or-stale`.
///
/// The `.beads/beads.base.jsonl` file is the canonical 3-way merge anchor
/// `br sync --merge` reads when reconciling a remote with a local
/// workspace. The pass-1 archaeology covers three failure shapes:
/// (a) symlinked anchor (security risk — an attacker shape per
///     git_sha:401c0495);
/// (b) anchor older than the live `.beads/issues.jsonl` (stale anchor
///     produces incorrect 3-way merges);
/// (c) anchor missing despite a prior sync flush (post-flush
///     workspaces should have an anchor).
///
/// Pass-5 cycle 4 implements (a) and (b). Case (c) requires reading
/// `metadata.last_export` from the DB which couples the detector to the
/// DB open path; deferred to a later cycle. The symlink and stale subsets
/// are repaired by the pass-5 fixers below.
fn check_base_jsonl(beads_dir: &Path, checks: &mut Vec<CheckResult>) {
    let base_path = beads_dir.join("beads.base.jsonl");
    let meta = match fs::symlink_metadata(&base_path) {
        Ok(m) => m,
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            // Missing-on-fresh-clone is legitimate. Without DB access at
            // this layer we cannot distinguish "fresh" from "post-flush
            // missing", so report Ok and let case (c) land in a future
            // cycle.
            push_check(checks, "base_jsonl", CheckStatus::Ok, None, None);
            return;
        }
        Err(err) => {
            push_check(
                checks,
                "base_jsonl",
                CheckStatus::Warn,
                Some(format!("Could not inspect {}: {err}", base_path.display())),
                Some(serde_json::json!({
                    "path": base_path.display().to_string(),
                    "kind": "unreadable",
                })),
            );
            return;
        }
    };

    // (a) Symlink → reject outright. Symlinked merge anchors are an
    // attacker shape: a malicious actor could point the anchor at any
    // file on disk and `br sync --merge` would diff against it.
    if meta.file_type().is_symlink() {
        push_check(
            checks,
            "base_jsonl",
            CheckStatus::Warn,
            Some(format!(
                "{} is a symlink — refusing to trust it as a merge anchor",
                base_path.display()
            )),
            Some(serde_json::json!({
                "path": base_path.display().to_string(),
                "kind": "symlink",
            })),
        );
        return;
    }

    // (b) Stale anchor: base mtime older than live JSONL mtime AND the
    // live JSONL is non-empty. A stale anchor leaks the wrong base into
    // 3-way merges. We compare only file mtimes (no content hash) at
    // this layer to keep the detector pure-stat.
    let live = beads_dir.join("issues.jsonl");
    let Ok(live_meta) = fs::symlink_metadata(&live) else {
        push_check(checks, "base_jsonl", CheckStatus::Ok, None, None);
        return;
    };
    if !live_meta.is_file() || live_meta.len() == 0 {
        push_check(checks, "base_jsonl", CheckStatus::Ok, None, None);
        return;
    }
    let (Ok(base_mtime), Ok(live_mtime)) = (meta.modified(), live_meta.modified()) else {
        push_check(checks, "base_jsonl", CheckStatus::Ok, None, None);
        return;
    };
    if base_mtime < live_mtime {
        push_check(
            checks,
            "base_jsonl",
            CheckStatus::Warn,
            Some(format!(
                "Merge anchor {} is older than the live JSONL — 3-way merges will diff against stale state",
                base_path.display()
            )),
            Some(serde_json::json!({
                "path": base_path.display().to_string(),
                "kind": "stale",
                "live_jsonl": live.display().to_string(),
            })),
        );
        return;
    }

    push_check(checks, "base_jsonl", CheckStatus::Ok, None, None);
}

/// Pass-4 cycle 2: detector for `fm-configs-startup-cache-poisoned`.
///
/// Checks the exact startup-cache file the production read path would use for
/// this workspace (`try_read_startup_cache` in `src/config/mod.rs`) and emits
/// Warn when that file is unreadable or fails to deserialize as a
/// `StartupCacheRecord`. These are the cache failures the production read path
/// silently swallows via `.ok()?` — the file stays on disk poisoning future
/// invocations until something cleans it up.
///
/// The detector is intentionally NARROWER than the full pass-1 spec (it
/// does not surface version/key/witness/semantic drift). Those cases
/// auto-recover on the next normal br invocation and would be noisy to
/// flag. Corrupt-on-disk is the case the operator can't recover from
/// without intervention.
fn check_startup_cache(
    beads_dir: &Path,
    db_override: Option<&PathBuf>,
    checks: &mut Vec<CheckResult>,
) {
    let poisoned = config::doctor_inspect_startup_cache(beads_dir, db_override);
    if poisoned.is_empty() {
        push_check(checks, "startup_cache.health", CheckStatus::Ok, None, None);
        return;
    }
    let cache_dir = config::doctor_startup_cache_dir();
    let files: Vec<serde_json::Value> = poisoned
        .iter()
        .map(|p| {
            let (kind, error) = match &p.kind {
                config::PoisonedStartupCacheKind::Unreadable { error } => {
                    ("unreadable", error.clone())
                }
                config::PoisonedStartupCacheKind::ParseError { error, .. } => {
                    ("parse_error", error.clone())
                }
            };
            serde_json::json!({
                "path": p.path.to_string_lossy(),
                "kind": kind,
                "error": error,
            })
        })
        .collect();
    push_check(
        checks,
        "startup_cache.health",
        CheckStatus::Warn,
        Some(format!(
            "{} poisoned startup-cache file(s) under {}",
            poisoned.len(),
            cache_dir.display()
        )),
        Some(serde_json::json!({
            "poisoned": files,
            "cache_dir": cache_dir.to_string_lossy(),
        })),
    );
}

/// Check whether the project root `.gitignore` contains a pattern that would
/// hide `.beads/.gitignore`, preventing git from reading br's ignore rules.
/// This commonly happens during bd-to-br migration where the old `.gitignore`
/// included patterns like `.beads/`, `.beads/*`, or `.beads/.gitignore`.
fn check_root_gitignore(beads_dir: &Path, checks: &mut Vec<CheckResult>) {
    let Some(project_root) = beads_dir.parent() else {
        return;
    };
    let gitignore_path = project_root.join(".gitignore");
    let content = match read_root_gitignore_content(&gitignore_path) {
        Ok(Some(content)) => content,
        Ok(None) => return,
        Err(err) => {
            push_check(
                checks,
                "gitignore.beads_inner",
                CheckStatus::Warn,
                Some(format!("Could not inspect root .gitignore: {err}")),
                Some(serde_json::json!({
                    "gitignore_path": gitignore_path.display().to_string(),
                })),
            );
            return;
        }
    };

    let offending: Vec<String> = content
        .lines()
        .filter(|line| is_offending_root_gitignore_pattern(line))
        .map(String::from)
        .collect();

    if offending.is_empty() {
        push_check(checks, "gitignore.beads_inner", CheckStatus::Ok, None, None);
    } else {
        push_check(
            checks,
            "gitignore.beads_inner",
            CheckStatus::Warn,
            Some(format!(
                "Root .gitignore excludes .beads/.gitignore — br's ignore rules are ineffective. \
                 Remove the offending line(s) from .gitignore to fix: {}",
                offending.join(", ")
            )),
            Some(serde_json::json!({
                "gitignore_path": gitignore_path.display().to_string(),
                "offending_patterns": offending,
            })),
        );
    }
}

/// Detector: `.beads/routes.jsonl` parses cleanly and every line carries a
/// non-empty `prefix` + `path`. Routes are used by route-aware commands
/// (`br show`, `br update`, `br dep`, etc.) to dispatch cross-workspace
/// operations; one malformed line silently breaks every routed command.
///
/// This is the FM `fm-routes_external-routes-jsonl-corrupt` (P1) detector
/// in the pass-1 archaeology — unblocks the `routes_external` subsystem's
/// fixture suite (`beads_rust-gl1m`).
///
/// Detect-only. The doctor never auto-rewrites `routes.jsonl` because the
/// operator's intent for cross-project routing is unknowable from the
/// outside (deleting an orphan line could lose a routing decision the
/// operator hasn't yet recorded elsewhere). On error, the check surfaces
/// the bad line numbers + reasons; the operator handles the rewrite.
///
/// Status mapping:
/// - `ok` — file missing (routes are optional) OR every non-comment route line
///   is well-formed.
/// - `warn` — at least one line is malformed JSON, missing `prefix`/`path`,
///   has a non-string `prefix`/`path`, or `prefix` is empty.
fn check_routes_jsonl(beads_dir: &Path, checks: &mut Vec<CheckResult>) {
    let routes_path = beads_dir.join("routes.jsonl");

    if !routes_path.is_file() {
        // Routes are optional. Absence is not a finding.
        push_check(
            checks,
            "routes_jsonl",
            CheckStatus::Ok,
            Some("No routes.jsonl present (cross-project routing is optional)".to_string()),
            None,
        );
        return;
    }

    let body = match fs::read_to_string(&routes_path) {
        Ok(s) => s,
        Err(err) => {
            push_check(
                checks,
                "routes_jsonl",
                CheckStatus::Warn,
                Some(format!("Failed to read routes.jsonl: {err}")),
                Some(serde_json::json!({
                    "path": routes_path.display().to_string(),
                })),
            );
            return;
        }
    };

    let mut malformed_lines: Vec<serde_json::Value> = Vec::new();
    let mut valid_count: usize = 0;

    for (idx, raw) in body.lines().enumerate() {
        let line_no = idx + 1;
        let line = raw.trim();
        if config::routing::is_ignorable_route_jsonl_line(line) {
            continue;
        }

        let value: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(err) => {
                malformed_lines.push(serde_json::json!({
                    "line": line_no,
                    "reason": format!("parse_error: {err}"),
                }));
                continue;
            }
        };

        let mut reasons = Vec::new();
        match value.get("prefix") {
            None => reasons.push("missing `prefix` field".to_string()),
            Some(prefix) => match prefix.as_str() {
                Some("") => reasons.push("empty `prefix` field".to_string()),
                Some(_) => {}
                None => reasons.push("non-string `prefix` field".to_string()),
            },
        }
        match value.get("path") {
            None => reasons.push("missing `path` field".to_string()),
            Some(path) => match path.as_str() {
                Some("") => reasons.push("empty `path` field".to_string()),
                Some(_) => {}
                None => reasons.push("non-string `path` field".to_string()),
            },
        }

        if reasons.is_empty() {
            valid_count += 1;
        } else {
            malformed_lines.push(serde_json::json!({
                "line": line_no,
                "reason": reasons.join("; "),
                "reasons": reasons,
            }));
        }
    }

    if malformed_lines.is_empty() {
        push_check(
            checks,
            "routes_jsonl",
            CheckStatus::Ok,
            Some(format!("{valid_count} routes parsed cleanly")),
            Some(serde_json::json!({
                "path": routes_path.display().to_string(),
                "valid_count": valid_count,
            })),
        );
    } else {
        let bad = malformed_lines.len();
        push_check(
            checks,
            "routes_jsonl",
            CheckStatus::Warn,
            Some(format!(
                "{bad} malformed route line(s) in routes.jsonl ({valid_count} valid). Operator must rewrite manually; doctor never auto-rewrites routes."
            )),
            Some(serde_json::json!({
                "path": routes_path.display().to_string(),
                "valid_count": valid_count,
                "malformed_lines": malformed_lines,
            })),
        );
    }
}

/// Detector: routes parse and their target `.beads` directories resolve.
///
/// `routes_jsonl` validates the shape of each line. This companion check follows
/// the same resolution rules as routed commands so stale project paths and broken
/// redirects show up during `br doctor` instead of only when an agent tries to
/// operate on a routed issue.
fn check_routes_targets_resolve(beads_dir: &Path, checks: &mut Vec<CheckResult>) {
    let route_sources = doctor_route_sources(beads_dir);

    if route_sources.is_empty() {
        push_check(
            checks,
            "routes.targets",
            CheckStatus::Ok,
            Some("No local or town routes.jsonl present (route targets are optional)".to_string()),
            None,
        );
        return;
    }

    let mut unresolved_routes = Vec::new();
    let mut inspected_paths = Vec::new();
    let mut route_count = 0usize;
    let mut resolved_count = 0usize;

    for (routes_path, base_dir) in route_sources {
        inspected_paths.push(routes_path.display().to_string());
        match inspect_route_target_source(&routes_path, &base_dir, &mut unresolved_routes) {
            Ok((source_route_count, source_resolved_count)) => {
                route_count += source_route_count;
                resolved_count += source_resolved_count;
            }
            Err(err) => {
                push_check(
                    checks,
                    "routes.targets",
                    CheckStatus::Warn,
                    Some(format!(
                        "Could not resolve route targets because routes.jsonl is invalid: {err}"
                    )),
                    Some(serde_json::json!({
                        "path": routes_path.display().to_string(),
                    })),
                );
                return;
            }
        }
    }

    if unresolved_routes.is_empty() {
        push_check(
            checks,
            "routes.targets",
            CheckStatus::Ok,
            Some(format!("{resolved_count} route target(s) resolved cleanly")),
            Some(serde_json::json!({
                "paths": inspected_paths,
                "route_count": route_count,
                "resolved_count": resolved_count,
            })),
        );
    } else {
        let unresolved_count = unresolved_routes.len();
        push_check(
            checks,
            "routes.targets",
            CheckStatus::Warn,
            Some(format!(
                "{unresolved_count} route target(s) failed to resolve ({resolved_count} resolved). Update routes.jsonl or the referenced project paths."
            )),
            Some(serde_json::json!({
                "paths": inspected_paths,
                "route_count": route_count,
                "resolved_count": resolved_count,
                "unresolved_routes": unresolved_routes,
            })),
        );
    }
}

fn inspect_route_target_source(
    routes_path: &Path,
    base_dir: &Path,
    unresolved_routes: &mut Vec<serde_json::Value>,
) -> Result<(usize, usize)> {
    let routes = config::routing::load_routes(routes_path)?;
    let mut resolved_count = 0usize;

    for route in &routes {
        if route.path.trim().is_empty() {
            unresolved_routes.push(serde_json::json!({
                "route_file": routes_path.display().to_string(),
                "prefix": route.prefix.as_str(),
                "path": route.path.as_str(),
                "reason": "empty route path",
            }));
            continue;
        }

        let target_path = doctor_route_target_beads_dir(route, base_dir);
        match config::routing::follow_redirects(&target_path, 10) {
            Ok(final_path)
                if final_path.is_dir()
                    && final_path
                        .file_name()
                        .is_some_and(config::is_beads_dir_name) =>
            {
                resolved_count += 1;
            }
            Ok(final_path) => unresolved_routes.push(serde_json::json!({
                "route_file": routes_path.display().to_string(),
                "prefix": route.prefix.as_str(),
                "path": route.path.as_str(),
                "target": target_path.display().to_string(),
                "resolved": final_path.display().to_string(),
                "reason": "target is not a beads directory",
            })),
            Err(err) => unresolved_routes.push(serde_json::json!({
                "route_file": routes_path.display().to_string(),
                "prefix": route.prefix.as_str(),
                "path": route.path.as_str(),
                "target": target_path.display().to_string(),
                "reason": err.to_string(),
            })),
        }
    }

    Ok((routes.len(), resolved_count))
}

fn doctor_route_sources(beads_dir: &Path) -> Vec<(PathBuf, PathBuf)> {
    let project_root = beads_dir.parent().unwrap_or(beads_dir);
    let mut sources = Vec::new();

    let local_routes_path = beads_dir.join("routes.jsonl");
    if local_routes_path.is_file() {
        sources.push((local_routes_path, project_root.to_path_buf()));
    }

    if let Some(town_root) = config::routing::find_town_root(project_root) {
        let town_beads_dir = town_root.join(".beads");
        let town_routes_path = town_beads_dir.join("routes.jsonl");
        if town_beads_dir != beads_dir && town_routes_path.is_file() {
            sources.push((town_routes_path, town_root));
        }
    }

    sources
}

fn doctor_route_target_beads_dir(route: &config::routing::RouteEntry, base_dir: &Path) -> PathBuf {
    if route.path == "." {
        return base_dir.join(".beads");
    }

    let path = PathBuf::from(&route.path);
    let resolved = if path.is_absolute() {
        path
    } else {
        base_dir.join(path)
    };

    if resolved.file_name().is_some_and(config::is_beads_dir_name) {
        resolved
    } else {
        resolved.join(".beads")
    }
}

/// Detector: `RUST_LOG` is set to a verbose level that would dump tracing
/// output to stderr and confuse agents parsing `br ... --json`. Agents who
/// shell out without setting `RUST_LOG=error` can get dozens of `info`/`debug`
/// lines per command on stderr, which makes failures hard to spot.
///
/// Detect-only. The doctor cannot mutate the parent shell's environment.
/// Surfaces a warn-level advisory naming the active level + the
/// recommended `RUST_LOG=error` setting from README.md.
///
/// Status mapping:
/// - `ok` - release-build `RUST_LOG` unset, blank, or set to `warn`/`error`/`off`.
/// - `warn` - debug-build `RUST_LOG` unset, or any explicit `info`, `debug`,
///   `trace`, or per-module directive at those levels.
///
/// The advisory carries the canonical fix from README.md: `export RUST_LOG=error`.
fn check_rust_log_noisy(checks: &mut Vec<CheckResult>) {
    let raw = std::env::var("RUST_LOG").ok();
    let raw_ref = raw.as_deref();

    let level = rust_log_volume(raw_ref);
    match level {
        RustLogVolume::Quiet => {
            push_check(
                checks,
                "rust_log",
                CheckStatus::Ok,
                Some(match raw_ref {
                    None => {
                        "RUST_LOG unset; release default is quiet enough for --json".to_string()
                    }
                    Some(v) => format!("RUST_LOG={v} (quiet)"),
                }),
                Some(serde_json::json!({
                    "rust_log": raw_ref,
                })),
            );
        }
        RustLogVolume::Noisy { reason } => {
            push_check(
                checks,
                "rust_log",
                CheckStatus::Warn,
                Some(format!(
                    "RUST_LOG={} would dump verbose tracing to stderr and break agents parsing --json. \
                     Run `export RUST_LOG=error` as documented in README.md.",
                    raw_ref.unwrap_or("(unset)"),
                )),
                Some(serde_json::json!({
                    "rust_log": raw_ref,
                    "reason": reason,
                    "recommended_fix": "export RUST_LOG=error",
                })),
            );
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RustLogVolume {
    Quiet,
    Noisy { reason: &'static str },
}

/// Classify a `RUST_LOG` env value as quiet or noisy. Mirrors the
/// `tracing_subscriber::EnvFilter` precedence: per-module directives
/// override the default. Directives without an explicit level are equivalent
/// to `trace`, so they must be treated as noisy too.
fn rust_log_volume(raw: Option<&str>) -> RustLogVolume {
    let Some(value) = raw else {
        return rust_log_default_volume();
    };
    let normalized = value.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return RustLogVolume::Quiet;
    }
    // Plain bare levels.
    match normalized.as_str() {
        "off" | "error" | "warn" => return RustLogVolume::Quiet,
        "info" => {
            return RustLogVolume::Noisy {
                reason: "bare_level_info",
            };
        }
        "debug" => {
            return RustLogVolume::Noisy {
                reason: "bare_level_debug",
            };
        }
        "trace" => {
            return RustLogVolume::Noisy {
                reason: "bare_level_trace",
            };
        }
        _ => {}
    }

    // Composite directive: any segment of the form `<mod>=<level>`,
    // `<level>`, or target-only `<mod>` that enables info/debug/trace
    // triggers warn. Upstream EnvFilter treats target-only directives as
    // `trace`.
    for segment in normalized.split(',') {
        let seg = segment.trim();
        if seg.is_empty() {
            continue;
        }
        let level_part = seg.rsplit('=').next().unwrap_or(seg).trim();
        match level_part {
            "off" | "error" | "warn" => {}
            "info" => {
                return RustLogVolume::Noisy {
                    reason: "directive_info",
                };
            }
            "debug" => {
                return RustLogVolume::Noisy {
                    reason: "directive_debug",
                };
            }
            "trace" => {
                return RustLogVolume::Noisy {
                    reason: "directive_trace",
                };
            }
            _ if !seg.contains('=') => {
                return RustLogVolume::Noisy {
                    reason: "directive_target_only",
                };
            }
            _ => {
                return RustLogVolume::Noisy {
                    reason: "directive_unclassified",
                };
            }
        }
    }
    RustLogVolume::Quiet
}

fn rust_log_default_volume() -> RustLogVolume {
    if cfg!(debug_assertions) {
        RustLogVolume::Noisy {
            reason: "debug_build_default",
        }
    } else {
        RustLogVolume::Quiet
    }
}

/// Detector: `.beads/` directory and its critical children must be
/// writable by the running process. Pass-1 archaeology filed
/// `fm-permissions-beads-dir-readonly` (P0): a read-only `.beads/`
/// (or an `issues.jsonl` / `beads.db` with cleared user-write bits)
/// causes sync writes to fail mid-stream, leaving the DB and JSONL
/// in an inconsistent torn-write state.
///
/// Detect-only. The doctor never `chmod`s the operator's `.beads/`
/// because the safe action is "tell the operator the exact mode they
/// need", not "guess what the operator wanted". This honors the
/// AGENTS.md no-destructive-mutation contract on permission state.
///
/// Status mapping:
/// - `ok` — `.beads/` and the two critical children (`issues.jsonl`,
///   `beads.db`) all have the user-write bit set on their POSIX mode.
///   Missing children are NOT a finding here (other detectors cover
///   them).
/// - `warn` — at least one of `.beads/`, `.beads/issues.jsonl`, or
///   `.beads/beads.db` exists with mode `& 0o200 == 0`. The check
///   surfaces every affected path + its current mode + the canonical
///   fix command (`chmod u+w <path>`).
///
/// Conservative-by-design:
/// - Only POSIX user-write bit. We do NOT probe ACLs, NFS extended
///   attrs, or `chattr +i` — the doctor returns false negatives on
///   those rather than false positives that would alarm operators
///   on perfectly fine setups.
/// - No probe-write (a probe write would technically be a side
///   effect; the spec calls for a future `Op::ProbeOnly` chokepoint
///   channel we have not yet shipped — `beads_rust-probe-only` is the
///   tracking work).
#[cfg(unix)]
fn check_permissions_beads_dir(beads_dir: &Path, checks: &mut Vec<CheckResult>) {
    use std::os::unix::fs::PermissionsExt;

    let mut readonly: Vec<serde_json::Value> = Vec::new();

    // The directory itself.
    if let Ok(meta) = fs::metadata(beads_dir) {
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o200 == 0 {
            readonly.push(serde_json::json!({
                "path": beads_dir.display().to_string(),
                "mode_octal": format!("{mode:03o}"),
                "fix": format!("chmod u+w {}", beads_dir.display()),
                "kind": "directory",
            }));
        }
    } else {
        // Missing .beads/ is the beads_dir check's job, not this
        // detector's. Emit an Ok ack with the absent context so
        // downstream agents see the detector ran.
        push_check(
            checks,
            "permissions.beads_dir",
            CheckStatus::Ok,
            Some(format!(
                "{} not stat-able; deferring to beads_dir check",
                beads_dir.display(),
            )),
            None,
        );
        return;
    }

    // Critical children.
    for child_name in &["issues.jsonl", "beads.db"] {
        let child = beads_dir.join(child_name);
        let Ok(meta) = fs::metadata(&child) else {
            continue;
        };
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o200 == 0 {
            readonly.push(serde_json::json!({
                "path": child.display().to_string(),
                "mode_octal": format!("{mode:03o}"),
                "fix": format!("chmod u+w {}", child.display()),
                "kind": "file",
            }));
        }
    }

    if readonly.is_empty() {
        push_check(
            checks,
            "permissions.beads_dir",
            CheckStatus::Ok,
            Some("`.beads/` and critical children are user-writable".to_string()),
            Some(serde_json::json!({
                "beads_dir": beads_dir.display().to_string(),
            })),
        );
    } else {
        let paths: Vec<String> = readonly
            .iter()
            .filter_map(|e| {
                e.get("path")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
            })
            .collect();
        push_check(
            checks,
            "permissions.beads_dir",
            CheckStatus::Warn,
            Some(format!(
                "{count} path(s) under .beads/ lack the user-write bit: {paths}. \
                 Operator must fix manually; doctor never auto-chmods.",
                count = readonly.len(),
                paths = paths.join(", "),
            )),
            Some(serde_json::json!({
                "beads_dir": beads_dir.display().to_string(),
                "readonly_paths": readonly,
            })),
        );
    }
}

/// Collect the dotted leaf keys of a YAML mapping (`sync.auto_flush`, ...).
fn flatten_yaml_keys(value: &serde_yml::Value, prefix: &str, out: &mut Vec<String>) {
    let Some(mapping) = value.as_mapping() else {
        return;
    };
    for (key, child) in mapping {
        let Some(key) = key.as_str() else {
            continue;
        };
        let path = if prefix.is_empty() {
            key.to_string()
        } else {
            format!("{prefix}.{key}")
        };
        if child.is_mapping() {
            flatten_yaml_keys(child, &path, out);
        } else {
            out.push(path);
        }
    }
}

/// `config.unknown_keys`: every leaf key in `.beads/config.yaml` that no br
/// getter reads. `br config set` accepts any key, so stale docs (`id.prefix`,
/// `defaults.priority`) produce files that look configured and change
/// nothing; this check names each such key and the nearest real one.
fn check_config_unknown_keys(beads_dir: &Path, checks: &mut Vec<CheckResult>) {
    const NAME: &str = "config.unknown_keys";
    let config_path = beads_dir.join("config.yaml");
    let Ok(body) = fs::read_to_string(&config_path) else {
        push_check(checks, NAME, CheckStatus::Ok, None, None);
        return;
    };
    let Ok(parsed) = serde_yml::from_str::<serde_yml::Value>(&body) else {
        // `config.yaml` reports the parse failure; nothing to add here.
        push_check(checks, NAME, CheckStatus::Ok, None, None);
        return;
    };
    let mut keys = Vec::new();
    flatten_yaml_keys(&parsed, "", &mut keys);
    let unknown: Vec<serde_json::Value> = keys
        .iter()
        .filter(|key| !config::is_known_config_key(key))
        .map(|key| {
            serde_json::json!({
                "key": key,
                "nearest": config::nearest_config_keys(key, 3),
            })
        })
        .collect();
    if unknown.is_empty() {
        push_check(checks, NAME, CheckStatus::Ok, None, None);
        return;
    }
    let listed = unknown
        .iter()
        .filter_map(|entry| entry["key"].as_str())
        .collect::<Vec<_>>()
        .join(", ");
    push_check(
        checks,
        NAME,
        CheckStatus::Warn,
        Some(format!(
            "config.yaml sets {} key(s) br does not read: {listed} (run `br config schema` for the keys it honors)",
            unknown.len()
        )),
        Some(serde_json::json!({
            "path": config_path.display().to_string(),
            "unknown_keys": unknown,
        })),
    );
}

/// Detector: `.beads/config.yaml` parses cleanly as YAML. Pass-1
/// archaeology filed `fm-configs-yaml-malformed` (P1): a malformed
/// project config short-circuits every `br` invocation at startup
/// with an opaque error and no localization. The doctor surfaces the
/// parse error with line/column context so the operator can fix it
/// without grepping unfamiliar code paths.
///
/// Detect-only. The doctor never auto-rewrites `config.yaml`: there's
/// no algorithmic way to know what the operator INTENDED to write, so
/// the safe action is to report + advise.
///
/// Status mapping:
/// - `ok` — `config.yaml` missing (project config is optional) OR
///   parses cleanly as YAML.
/// - `warn` — file exists but `serde_yml::from_str` returns a parse
///   error. Surfaces the error message + the offending file path so
///   the operator can open the file and fix the line `serde_yml`
///   names.
fn check_config_yaml(beads_dir: &Path, checks: &mut Vec<CheckResult>) {
    let config_path = beads_dir.join("config.yaml");

    if !config_path.is_file() {
        push_check(
            checks,
            "config.yaml",
            CheckStatus::Ok,
            Some("No .beads/config.yaml present (project config is optional)".to_string()),
            None,
        );
        return;
    }

    let body = match fs::read_to_string(&config_path) {
        Ok(s) => s,
        Err(err) => {
            push_check(
                checks,
                "config.yaml",
                CheckStatus::Warn,
                Some(format!("Failed to read config.yaml: {err}")),
                Some(serde_json::json!({
                    "path": config_path.display().to_string(),
                })),
            );
            return;
        }
    };

    // Empty file is acceptable (treated as "all defaults").
    if body.trim().is_empty() {
        push_check(
            checks,
            "config.yaml",
            CheckStatus::Ok,
            Some("config.yaml is empty (all defaults)".to_string()),
            Some(serde_json::json!({
                "path": config_path.display().to_string(),
                "bytes": 0_u64,
            })),
        );
        return;
    }

    // Try parsing as a generic `serde_yml::Value` first — that lets us
    // surface the precise parse error without coupling to the full
    // ConfigLayer schema (which is stricter; an unknown-field warning
    // there is different from a malformed YAML structure).
    match serde_yml::from_str::<serde_yml::Value>(&body) {
        Ok(_) => {
            push_check(
                checks,
                "config.yaml",
                CheckStatus::Ok,
                Some(format!("config.yaml parses cleanly ({} bytes)", body.len())),
                Some(serde_json::json!({
                    "path": config_path.display().to_string(),
                    "bytes": body.len(),
                })),
            );
        }
        Err(err) => {
            // serde_yml's Display includes line/col when available; the
            // doctor exposes the raw message so operators don't have to
            // re-run `br` themselves to see it.
            push_check(
                checks,
                "config.yaml",
                CheckStatus::Warn,
                Some(format!(
                    "config.yaml is malformed YAML: {err}. Operator must fix manually; doctor never auto-rewrites config.",
                )),
                Some(serde_json::json!({
                    "path": config_path.display().to_string(),
                    "parse_error": err.to_string(),
                    "recommended_fix": format!(
                        "Open {} in an editor and fix the YAML; \
                         see the parse_error field for the precise location.",
                        config_path.display(),
                    ),
                })),
            );
        }
    }
}

/// Detector: `.beads/metadata.json` is parseable + its declared
/// `database` / `jsonl_export` targets exist on disk. Pass-1
/// archaeology filed `fm-configs-metadata-json-stale` (P1):
/// `metadata.json` drifts from the on-disk DB+JSONL filenames after
/// renames / bd-migration tools / direct edits, leaving br's path
/// resolution and the disk state out of sync.
///
/// Detect-only. The doctor never rewrites `metadata.json` because
/// there's no algorithmic way to know whether the operator intended
/// the metadata or the on-disk files to be authoritative.
///
/// Status mapping:
/// - `ok` — file missing (br treats as defaults) OR file parses AND
///   every explicitly declared non-empty `database` / `jsonl_export`
///   target resolves to an existing file.
/// - `warn` — file exists but is malformed JSON, has the wrong top-level
///   shape, or names files that don't exist. Surfaces the specific
///   reason (parse_error / target_missing) and the conservative
///   operator-fix advice.
fn check_metadata_json(beads_dir: &Path, checks: &mut Vec<CheckResult>) {
    let metadata_path = beads_dir.join("metadata.json");

    if !metadata_path.is_file() {
        push_metadata_json_missing(checks);
        return;
    }

    let body = match fs::read_to_string(&metadata_path) {
        Ok(s) => s,
        Err(err) => return push_metadata_json_read_error(&metadata_path, &err, checks),
    };

    // Empty file is treated as malformed (the loader does the same).
    if body.trim().is_empty() {
        push_metadata_json_empty(&metadata_path, checks);
        return;
    }

    let Some(obj) = parse_metadata_json_object(&metadata_path, &body, checks) else {
        return;
    };
    let database = metadata_declared_field(&obj, "database");
    let jsonl_export = metadata_declared_field(&obj, "jsonl_export");
    let drift = collect_metadata_json_drift(beads_dir, database, jsonl_export);

    if drift.is_empty() {
        push_metadata_json_ok(&metadata_path, body.len(), database, jsonl_export, checks);
    } else {
        push_metadata_json_drift(&metadata_path, &drift, checks);
    }
}

fn push_metadata_json_missing(checks: &mut Vec<CheckResult>) {
    push_check(
        checks,
        "metadata.json",
        CheckStatus::Ok,
        Some(
            "No .beads/metadata.json present (br uses defaults: beads.db + issues.jsonl)"
                .to_string(),
        ),
        None,
    );
}

#[cfg(not(unix))]
fn check_permissions_beads_dir(beads_dir: &Path, checks: &mut Vec<CheckResult>) {
    if !beads_dir.exists() {
        push_check(
            checks,
            "permissions.beads_dir",
            CheckStatus::Ok,
            Some(format!(
                "{} not stat-able; deferring to beads_dir check",
                beads_dir.display(),
            )),
            None,
        );
        return;
    }

    push_check(
        checks,
        "permissions.beads_dir",
        CheckStatus::Ok,
        Some("POSIX user-write bit check is not applicable on this platform".to_string()),
        Some(serde_json::json!({
            "beads_dir": beads_dir.display().to_string(),
            "platform": std::env::consts::OS,
        })),
    );
}

fn push_metadata_json_read_error(
    metadata_path: &Path,
    err: &io::Error,
    checks: &mut Vec<CheckResult>,
) {
    push_check(
        checks,
        "metadata.json",
        CheckStatus::Warn,
        Some(format!("Failed to read metadata.json: {err}")),
        Some(serde_json::json!({
            "path": metadata_path.display().to_string(),
        })),
    );
}

fn push_metadata_json_empty(metadata_path: &Path, checks: &mut Vec<CheckResult>) {
    push_check(
        checks,
        "metadata.json",
        CheckStatus::Warn,
        Some("metadata.json is empty".to_string()),
        Some(serde_json::json!({
            "path": metadata_path.display().to_string(),
            "reason": "empty_file",
            "recommended_fix": format!(
                "Either delete {} so br uses defaults, or write a valid JSON object.",
                metadata_path.display(),
            ),
        })),
    );
}

fn parse_metadata_json_object(
    metadata_path: &Path,
    body: &str,
    checks: &mut Vec<CheckResult>,
) -> Option<serde_json::Map<String, serde_json::Value>> {
    let value = serde_json::from_str::<serde_json::Value>(body)
        .inspect_err(|err| push_metadata_json_parse_error(metadata_path, err, checks))
        .ok()?;

    let serde_json::Value::Object(obj) = value else {
        push_check(
            checks,
            "metadata.json",
            CheckStatus::Warn,
            Some("metadata.json top-level value must be a JSON object".to_string()),
            Some(serde_json::json!({
                "path": metadata_path.display().to_string(),
                "reason": "wrong_top_level_shape",
            })),
        );
        return None;
    };

    Some(obj)
}

fn push_metadata_json_parse_error(
    metadata_path: &Path,
    err: &serde_json::Error,
    checks: &mut Vec<CheckResult>,
) {
    push_check(
        checks,
        "metadata.json",
        CheckStatus::Warn,
        Some(format!("metadata.json is malformed JSON: {err}")),
        Some(serde_json::json!({
            "path": metadata_path.display().to_string(),
            "reason": "parse_error",
            "parse_error": err.to_string(),
            "recommended_fix": format!(
                "Open {} in an editor and fix the JSON; see parse_error for the precise location.",
                metadata_path.display(),
            ),
        })),
    );
}

fn metadata_declared_field<'a>(
    obj: &'a serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> Option<&'a str> {
    obj.get(field)
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
}

fn collect_metadata_json_drift(
    beads_dir: &Path,
    database: Option<&str>,
    jsonl_export: Option<&str>,
) -> Vec<serde_json::Value> {
    let mut drift = Vec::new();

    if let Some(db_name) = database {
        let db_path = resolve_metadata_database_target(beads_dir, db_name);
        if !db_path.exists() {
            drift.push(serde_json::json!({
                "field": "database",
                "value": db_name,
                "expected_path": db_path.display().to_string(),
                "reason": "target_missing",
            }));
        }
    }

    if let Some(jsonl_name) = jsonl_export {
        let jsonl_path = resolve_metadata_jsonl_target(beads_dir, jsonl_name);
        if !jsonl_path.exists() {
            drift.push(serde_json::json!({
                "field": "jsonl_export",
                "value": jsonl_name,
                "expected_path": jsonl_path.display().to_string(),
                "reason": "target_missing",
            }));
        }
    }

    drift
}

fn push_metadata_json_ok(
    metadata_path: &Path,
    bytes: usize,
    database: Option<&str>,
    jsonl_export: Option<&str>,
    checks: &mut Vec<CheckResult>,
) {
    push_check(
        checks,
        "metadata.json",
        CheckStatus::Ok,
        Some(format!(
            "metadata.json parses cleanly ({bytes} bytes); declared targets exist on disk"
        )),
        Some(serde_json::json!({
            "path": metadata_path.display().to_string(),
            "bytes": bytes,
            "database": database,
            "jsonl_export": jsonl_export,
        })),
    );
}

fn push_metadata_json_drift(
    metadata_path: &Path,
    drift: &[serde_json::Value],
    checks: &mut Vec<CheckResult>,
) {
    let fields: Vec<&str> = drift.iter().filter_map(|e| e["field"].as_str()).collect();
    push_check(
        checks,
        "metadata.json",
        CheckStatus::Warn,
        Some(format!(
            "metadata.json declares {n} field(s) ({fields}) pointing at files that don't exist; \
             operator must reconcile by either renaming the on-disk file or editing metadata.json.",
            n = drift.len(),
            fields = fields.join(", "),
        )),
        Some(serde_json::json!({
            "path": metadata_path.display().to_string(),
            "drift": drift,
            "recommended_fix": format!(
                "Inspect {} and the listed expected_path entries; either rename the file or update metadata.json.",
                metadata_path.display(),
            ),
        })),
    );
}

/// Detector: the running `br` binary's compile-time
/// `CARGO_PKG_VERSION` matches the `version` field of any `Cargo.toml`
/// whose `[package].name == "beads_rust"` reachable upward from
/// `beads_dir`. Pass-1 archaeology filed
/// `fm-external_artifacts-binary-version-mismatch` (P1): an operator
/// rebuilds br in-tree but a stale older binary remains on PATH,
/// silently producing different output than the source-tree expects.
///
/// Detect-only and conservative. The doctor cannot replace its own
/// running binary; that is `br upgrade`'s job. We only emit a warn
/// when:
///   1. A reachable `Cargo.toml` declares `name = "beads_rust"`.
///   2. Its `version` field parses as a valid semver.
///   3. The running binary's `CARGO_PKG_VERSION` parses as semver.
///   4. The tree-version is STRICTLY GREATER than the binary-version
///      (running an OLDER binary against a NEWER tree — the dev-loop
///      footgun). Tree behind binary is silent (operator may be
///      working in a side-branch).
///
/// Conservative-by-design:
/// - No network probes.
/// - No PATH-walking sibling-binary enumeration (that's the
///   `fm-external_artifacts-multiple-br-in-path` FM — separate
///   detector / spec).
/// - No GitHub-latest-release comparison (offline-by-default skill
///   policy; live network calls would be opt-in via `--online`).
/// - If no `beads_rust` Cargo.toml is reachable, the detector emits
///   `ok` with `not_in_beads_rust_repo: true` — the common operator
///   case is running br outside its own source tree.
///
/// Status mapping:
/// - `ok` — versions match, OR no reachable beads_rust Cargo.toml,
///   OR either version is unparseable (silent — the operator's
///   tree may be using a non-semver tag).
/// - `warn` — tree version > binary version. Surfaces both
///   versions + the canonical fix (`cargo install --path . --locked`
///   from the repo root).
fn check_binary_version_mismatch(beads_dir: &Path, checks: &mut Vec<CheckResult>) {
    let binary_version_str = env!("CARGO_PKG_VERSION");

    // Walk upward from beads_dir.parent() looking for a Cargo.toml
    // whose [package].name == "beads_rust".
    let Some(repo_root) = find_beads_rust_repo_root(beads_dir) else {
        push_check(
            checks,
            "binary_version",
            CheckStatus::Ok,
            Some(format!(
                "Running br {binary_version_str}; no beads_rust Cargo.toml reachable from .beads/ — not flagging"
            )),
            Some(serde_json::json!({
                "binary_version": binary_version_str,
                "not_in_beads_rust_repo": true,
            })),
        );
        return;
    };

    let cargo_toml_path = repo_root.join("Cargo.toml");
    let Some(tree_version_str) = read_cargo_toml_version(&cargo_toml_path) else {
        // Cargo.toml unreadable or missing version — silent.
        push_check(
            checks,
            "binary_version",
            CheckStatus::Ok,
            Some(format!(
                "Running br {binary_version_str}; beads_rust Cargo.toml at {} has no readable version",
                cargo_toml_path.display(),
            )),
            Some(serde_json::json!({
                "binary_version": binary_version_str,
                "cargo_toml": cargo_toml_path.display().to_string(),
            })),
        );
        return;
    };

    let Ok(binary_version) = semver::Version::parse(binary_version_str) else {
        push_check(
            checks,
            "binary_version",
            CheckStatus::Ok,
            Some(format!(
                "Running br {binary_version_str}; binary version is not parseable semver"
            )),
            None,
        );
        return;
    };
    let Ok(tree_version) = semver::Version::parse(&tree_version_str) else {
        push_check(
            checks,
            "binary_version",
            CheckStatus::Ok,
            Some(format!(
                "Running br {binary_version_str}; Cargo.toml version {tree_version_str} is not parseable semver"
            )),
            None,
        );
        return;
    };

    if tree_version > binary_version {
        push_check(
            checks,
            "binary_version",
            CheckStatus::Warn,
            Some(format!(
                "Running br {binary_version_str} but beads_rust Cargo.toml at {} declares {tree_version_str}. \
                 Rebuild + reinstall to pick up the newer tree.",
                cargo_toml_path.display(),
            )),
            Some(serde_json::json!({
                "binary_version": binary_version_str,
                "tree_version": tree_version_str,
                "cargo_toml": cargo_toml_path.display().to_string(),
                "repo_root": repo_root.display().to_string(),
                "recommended_fix": format!(
                    "cd {} && cargo install --path . --locked",
                    repo_root.display(),
                ),
            })),
        );
    } else {
        push_check(
            checks,
            "binary_version",
            CheckStatus::Ok,
            Some(format!(
                "Running br {binary_version_str}; matches (or is ahead of) Cargo.toml at {} ({})",
                cargo_toml_path.display(),
                tree_version_str,
            )),
            Some(serde_json::json!({
                "binary_version": binary_version_str,
                "tree_version": tree_version_str,
                "cargo_toml": cargo_toml_path.display().to_string(),
            })),
        );
    }
}

/// Walk upward from `start` looking for the containing Rust package.
/// Returns its root only when the nearest package-bearing `Cargo.toml`
/// declares `[package].name == "beads_rust"`.
///
/// A manifest for another package is a repository boundary, even when that
/// package happens to live somewhere below a beads_rust checkout (for example,
/// a test fixture rooted under `target/`). Continuing beyond that boundary can
/// misidentify an unrelated workspace as beads_rust and emit a false version
/// mismatch warning. Workspace-only manifests have no package identity, so the
/// search may continue past them. The walk is bounded at 32 levels to avoid
/// pathological symlink loops.
fn find_beads_rust_repo_root(start: &Path) -> Option<PathBuf> {
    let mut current = start.parent()?.to_path_buf();
    for _ in 0..32 {
        let candidate = current.join("Cargo.toml");
        if let Some(name) = read_cargo_toml_package_name(&candidate) {
            return (name == "beads_rust").then_some(current);
        }
        let parent = current.parent()?;
        if parent == current {
            return None;
        }
        current = parent.to_path_buf();
    }
    None
}

/// Extract `[package].name` from a `Cargo.toml` path. Returns `None` on
/// any I/O or parse failure (intentional — the detector is conservative).
fn read_cargo_toml_package_name(path: &Path) -> Option<String> {
    let body = fs::read_to_string(path).ok()?;
    parse_cargo_toml_package_field(&body, "name")
}

/// Extract `[package].version` from a `Cargo.toml` path. Returns `None`
/// on any I/O or parse failure.
fn read_cargo_toml_version(path: &Path) -> Option<String> {
    let body = fs::read_to_string(path).ok()?;
    parse_cargo_toml_package_field(&body, "version")
}

/// Minimal TOML-by-line parser for `[package]` fields. We do NOT pull
/// in a full TOML crate just for this — `Cargo.toml` is line-oriented
/// in practice and the chance of `[package]\n... = "..."` getting
/// arbitrarily nested is near-zero. Returns the string value of the
/// named field if found INSIDE the `[package]` section, `None`
/// otherwise.
fn parse_cargo_toml_package_field(body: &str, field: &str) -> Option<String> {
    let mut in_package = false;
    for raw in body.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[') {
            // New section starts; check if it's [package].
            let section = rest.trim_end_matches(']').trim();
            in_package = section == "package";
            continue;
        }
        if !in_package {
            continue;
        }
        // Match `<field> = "..."` (single or double quoted).
        if let Some(rest) = line.strip_prefix(field) {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                let rest = rest.trim();
                // Strip optional inline comment.
                let val_with_quotes = rest.split('#').next().unwrap_or("").trim();
                if val_with_quotes.starts_with('"') && val_with_quotes.ends_with('"') {
                    return Some(val_with_quotes[1..val_with_quotes.len() - 1].to_string());
                }
                if val_with_quotes.starts_with('\'') && val_with_quotes.ends_with('\'') {
                    return Some(val_with_quotes[1..val_with_quotes.len() - 1].to_string());
                }
            }
        }
    }
    None
}

/// Default staleness threshold for `.beads/.write.lock`. A lock file
/// whose mtime is older than this with no live `br` process around is
/// almost certainly an orphan from a crashed writer. Operator can
/// override via `BR_DOCTOR_STALE_LOCK_THRESHOLD_SECS`.
///
/// 300 seconds is generous: the longest legitimate write transactions
/// (DB rebuild from JSONL, full VACUUM) complete in well under a
/// minute on the largest real-world workspaces.
const DEFAULT_STALE_LOCK_THRESHOLD_SECS: u64 = 300;

/// Detector for `fm-concurrency_primitives-orphaned-write-lock`.
///
/// `.beads/.write.lock` is a persistent lock *target*, not a record that
/// a lock is held: `blocking_write_lock` opens it once and takes an
/// advisory flock, and flock acquisition never updates mtime. The kernel
/// releases an advisory flock when the owning fd closes — including on
/// kill -9, OOM, and abort — so a leftover regular file with an old
/// mtime does not wedge anything; the next writer's `try_lock` succeeds
/// immediately (GitHub #395, and the `mcp_serve_stale_write_lock`
/// fixture's repair contract states the same model).
///
/// Classification therefore PROBES instead of trusting mtime: a
/// non-blocking `try_lock` on the existing file. Acquired → the lock is
/// free, regardless of file age (released instantly; the probe is the
/// same primitive `blocking_write_lock` fast-paths on). Would-block → a
/// live process holds it — frequently this very doctor run, whose
/// startup acquires the workspace lock to serialize with writers (flock
/// conflicts across open file descriptions even within one process), and
/// in any case a held advisory lock is never an orphan. Only when
/// the probe itself cannot run does the old mtime heuristic surface as a
/// warn — and even then the guidance is to investigate holders, never to
/// move the file aside: renaming a lock file while a holder has the old
/// inode locked would let the next writer create and lock a NEW inode,
/// splitting mutual exclusion across two files.
///
/// Detect-only. The doctor NEVER removes or renames `.write.lock`.
///
/// Status mapping:
/// - `ok` — file missing; mtime within threshold; probe acquired (free);
///   or probe would-block (live holder).
/// - `warn` — mtime in the future (clock skew), or mtime older than the
///   threshold AND the probe could not run.
/// - `error` — the node is not a regular file (symlink, directory, fifo,
///   device, socket): fail closed (beads_rust-5sej). Startup would follow
///   a symlink here, so the shape itself is the defect. Classified from
///   lstat only; the target is never traversed and the node never moved.
fn check_orphaned_write_lock(beads_dir: &Path, checks: &mut Vec<CheckResult>) {
    let lock_path = beads_dir.join(".write.lock");

    let Ok(meta) = fs::symlink_metadata(&lock_path) else {
        push_write_lock_missing(checks);
        return;
    };

    // Fail closed on any non-regular node (beads_rust-5sej): startup's
    // `OpenOptions` on the lock path follows a symlink, so a symlinked
    // `.write.lock` silently relocates mutual exclusion to the target
    // inode, and a directory/fifo/device node breaks acquisition
    // outright. Classified via lstat only — the target is never
    // traversed and the node is never touched.
    if !meta.file_type().is_file() {
        push_write_lock_non_file(&lock_path, &meta, checks);
        return;
    }

    let threshold_secs = stale_lock_threshold_secs();

    let Ok(modified) = meta.modified() else {
        // mtime unreadable — emit Ok with explicit "deferring" note
        // rather than warn (the operator can't act on what we can't
        // measure).
        push_write_lock_mtime_unreadable(&lock_path, checks);
        return;
    };

    let Ok(age) = std::time::SystemTime::now().duration_since(modified) else {
        // Clock skew: mtime is in the future. Surface as Warn with
        // a distinct reason so the operator sees the clock issue.
        push_write_lock_future_mtime(&lock_path, checks);
        return;
    };
    let age_secs = age.as_secs();

    if age_secs < threshold_secs {
        push_write_lock_fresh(&lock_path, age_secs, threshold_secs, checks);
        return;
    }

    // Stale-mtime candidate: probe before classifying (GitHub #395).
    match probe_write_lock_is_free(&lock_path) {
        Some(true) => push_write_lock_free_despite_age(&lock_path, age_secs, checks),
        Some(false) => push_write_lock_held_by_live_process(&lock_path, age_secs, checks),
        None => {
            push_write_lock_stale_unprobed(&lock_path, modified, age_secs, threshold_secs, checks);
        }
    }
}

/// Non-blocking probe of the advisory flock on an EXISTING lock file.
///
/// `Some(true)` — acquired (and immediately released on drop): the lock
/// is free. `Some(false)` — would block: a live process holds it. `None`
/// — the probe could not run (open or lock error), so the caller must
/// fall back to the mtime heuristic. Never creates the file: a doctor
/// check must not mutate the workspace.
#[allow(clippy::incompatible_msrv)]
fn probe_write_lock_is_free(lock_path: &Path) -> Option<bool> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(false)
        .truncate(false)
        .open(lock_path)
        .ok()?;
    match file.try_lock() {
        Ok(()) => Some(true),
        Err(std::fs::TryLockError::WouldBlock) => Some(false),
        Err(std::fs::TryLockError::Error(_)) => None,
    }
}

fn push_write_lock_missing(checks: &mut Vec<CheckResult>) {
    push_check(
        checks,
        "write_lock",
        CheckStatus::Ok,
        Some("No .beads/.write.lock present (no writer contention)".to_string()),
        None,
    );
}

/// Stable classification of a non-regular lock node's shape, from lstat
/// metadata only (no target traversal).
fn write_lock_node_kind(file_type: fs::FileType) -> &'static str {
    if file_type.is_symlink() {
        return "symlink";
    }
    if file_type.is_dir() {
        return "directory";
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileTypeExt;
        if file_type.is_fifo() {
            return "fifo";
        }
        if file_type.is_socket() {
            return "socket";
        }
        if file_type.is_block_device() {
            return "block_device";
        }
        if file_type.is_char_device() {
            return "char_device";
        }
    }
    "unknown"
}

fn push_write_lock_non_file(lock_path: &Path, meta: &fs::Metadata, checks: &mut Vec<CheckResult>) {
    let kind = write_lock_node_kind(meta.file_type());
    push_check(
        checks,
        "write_lock",
        CheckStatus::Error,
        Some(format!(
            ".beads/.write.lock is a {kind}, not a regular file; a symlinked or otherwise \
             non-regular lock node silently relocates or breaks workspace mutual exclusion. \
             Verify no live writer is running, then move the node aside manually — \
             br never modifies the lock node itself; the next mutating call recreates a \
             regular lock file"
        )),
        Some(serde_json::json!({
            "path": lock_path.display().to_string(),
            "reason": "non_regular_lock_node",
            "node_kind": kind,
        })),
    );
}

fn stale_lock_threshold_secs() -> u64 {
    std::env::var("BR_DOCTOR_STALE_LOCK_THRESHOLD_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_STALE_LOCK_THRESHOLD_SECS)
}

fn push_write_lock_mtime_unreadable(lock_path: &Path, checks: &mut Vec<CheckResult>) {
    push_check(
        checks,
        "write_lock",
        CheckStatus::Ok,
        Some(
            ".beads/.write.lock present but mtime unreadable; cannot assess staleness".to_string(),
        ),
        Some(serde_json::json!({
            "path": lock_path.display().to_string(),
        })),
    );
}

fn push_write_lock_future_mtime(lock_path: &Path, checks: &mut Vec<CheckResult>) {
    push_check(
        checks,
        "write_lock",
        CheckStatus::Warn,
        Some(
            ".beads/.write.lock has an mtime in the future (clock skew?); review manually"
                .to_string(),
        ),
        Some(serde_json::json!({
            "path": lock_path.display().to_string(),
            "reason": "mtime_in_future",
        })),
    );
}

fn push_write_lock_fresh(
    lock_path: &Path,
    age_secs: u64,
    threshold_secs: u64,
    checks: &mut Vec<CheckResult>,
) {
    push_check(
        checks,
        "write_lock",
        CheckStatus::Ok,
        Some(format!(
            ".beads/.write.lock is {age_secs}s old (within {threshold_secs}s threshold); not stale"
        )),
        Some(serde_json::json!({
            "path": lock_path.display().to_string(),
            "age_secs": age_secs,
            "threshold_secs": threshold_secs,
            "reason": "persistent_advisory_inode",
        })),
    );
}

fn push_write_lock_free_despite_age(
    lock_path: &Path,
    age_secs: u64,
    checks: &mut Vec<CheckResult>,
) {
    push_check(
        checks,
        "write_lock",
        CheckStatus::Ok,
        Some(format!(
            ".beads/.write.lock file is {age_secs}s old but the advisory lock is FREE \
             (non-blocking probe acquired it). Lock acquisition never updates mtime, \
             so file age alone is not evidence of an orphan."
        )),
        Some(serde_json::json!({
            "path": lock_path.display().to_string(),
            "age_secs": age_secs,
            "reason": "probe_acquired_free",
        })),
    );
}

fn push_write_lock_held_by_live_process(
    lock_path: &Path,
    age_secs: u64,
    checks: &mut Vec<CheckResult>,
) {
    push_check(
        checks,
        "write_lock",
        CheckStatus::Ok,
        Some(
            ".beads/.write.lock is currently held by a live process (non-blocking \
             probe would block) — possibly this doctor run itself, which holds the \
             workspace lock while checking; a held advisory lock is never an orphan"
                .to_string(),
        ),
        Some(serde_json::json!({
            "path": lock_path.display().to_string(),
            "age_secs": age_secs,
            "reason": "persistent_advisory_inode",
            "probe_result": "would_block_live_holder",
        })),
    );
}

fn push_write_lock_stale_unprobed(
    lock_path: &Path,
    modified: std::time::SystemTime,
    age_secs: u64,
    threshold_secs: u64,
    checks: &mut Vec<CheckResult>,
) {
    let mtime_rfc3339 = chrono::DateTime::<chrono::Utc>::from(modified)
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    push_check(
        checks,
        "write_lock",
        CheckStatus::Warn,
        Some(format!(
            ".beads/.write.lock is {age_secs}s old (threshold {threshold_secs}s) and the \
             lock state could not be probed. Investigate whether a process holds it; do \
             NOT move or rename the file — a holder keeps the old inode locked while the \
             next writer would create and lock a new one, splitting mutual exclusion."
        )),
        Some(serde_json::json!({
            "path": lock_path.display().to_string(),
            "mtime": mtime_rfc3339,
            "age_secs": age_secs,
            "threshold_secs": threshold_secs,
            "reason": "stale_mtime",
            "probe": "failed",
            "investigate_holders": "lsof -- .beads/.write.lock; pgrep -af 'br ' | grep -v doctor | grep -v grep",
            "env_override": "BR_DOCTOR_STALE_LOCK_THRESHOLD_SECS",
        })),
    );
}

fn resolve_metadata_database_target(beads_dir: &Path, database: &str) -> PathBuf {
    let candidate = PathBuf::from(database);
    if candidate.is_absolute() {
        candidate
    } else {
        crate::util::resolve_cache_dir(beads_dir).join(candidate)
    }
}

fn resolve_metadata_jsonl_target(beads_dir: &Path, jsonl_export: &str) -> PathBuf {
    let candidate = PathBuf::from(jsonl_export);
    if candidate.is_absolute() {
        candidate
    } else {
        beads_dir.join(candidate)
    }
}

/// When `--repair` is passed and the `gitignore.beads_inner` warning is present,
/// automatically remove the offending lines from the root `.gitignore`.
///
/// When a [`DoctorRepairSession`] is provided, the rewrite is routed through
/// the WP1 [`chokepoint::mutate`] (verbatim backup + `actions.jsonl` line +
/// dry-run support). In production `--repair` flow the session is required
/// before this fixer runs; the no-session branch is retained for narrow unit
/// tests that exercise the legacy writer directly.
fn fix_root_gitignore_if_warned(
    beads_dir: &Path,
    report: &DoctorReport,
    ctx: &OutputContext,
    session: Option<&mut DoctorRepairSession>,
) -> bool {
    let has_warning = report
        .checks
        .iter()
        .any(|c| c.name == "gitignore.beads_inner" && c.status == CheckStatus::Warn);
    if !has_warning {
        return false;
    }
    let root_gitignore_locked = report
        .checks
        .iter()
        .any(|c| c.name == "permissions.root_gitignore" && matches!(c.status, CheckStatus::Warn));
    if root_gitignore_locked {
        if !ctx.is_json() {
            ctx.warning("Skipping .gitignore repair: root .gitignore is not owner-writable");
        }
        return false;
    }
    let Some(project_root) = beads_dir.parent() else {
        return false;
    };
    let gitignore_path = project_root.join(".gitignore");
    let content = match read_root_gitignore_content(&gitignore_path) {
        Ok(Some(content)) => content,
        Ok(None) => return false,
        Err(err) => {
            if !ctx.is_json() {
                ctx.warning(&format!("Skipping .gitignore repair: {err}"));
            }
            return false;
        }
    };

    let filtered: Vec<&str> = content
        .lines()
        .filter(|line| !is_offending_root_gitignore_pattern(line))
        .collect();
    let mut new_content = filtered.join("\n");
    if content.ends_with('\n') {
        new_content.push('\n');
    }

    let write_result = if let Some(session) = session {
        session.set_fixer("doctor.gitignore_repair");
        chokepoint::mutate(
            &session.ctx,
            &gitignore_path,
            Op::WriteFile {
                content: new_content.into_bytes(),
                mode: None,
            },
        )
        .map(|_| ())
    } else {
        write_root_gitignore_atomically(&gitignore_path, new_content.as_bytes())
    };

    if let Err(err) = write_result {
        if !ctx.is_json() {
            ctx.warning(&format!("Failed to fix .gitignore: {err}"));
        }
        false
    } else {
        if !ctx.is_json() {
            ctx.info(ROOT_GITIGNORE_REPAIR_MESSAGE);
        }
        true
    }
}

/// Pass-4 cycle 1 — fixer for `fm-state_files-merge-artifact-stuck`.
///
/// When `--repair` is passed and the `jsonl.merge_artifacts` warning is
/// present, quarantine each stuck `.beads/<name>.{base,left,right}.jsonl`
/// (excluding the canonical `beads.base.jsonl` sync anchor) by renaming
/// it through the [`chokepoint::mutate`] into
/// `<run-dir>/quarantine/.beads/<name>`.
///
/// Per AGENTS.md RULE 1 ("no file deletion without express permission"):
/// the fixer NEVER unlinks. It uses [`Op::Rename`] so `doctor undo
/// <run-id>` reverses the quarantine by un-renaming the bytes back into
/// place. The chokepoint records the rename target in `actions.jsonl`
/// alongside before/after hashes so the inverse is byte-deterministic.
///
/// Returns `true` if at least one artifact was quarantined.
fn fix_merge_artifacts_if_warned(
    beads_dir: &Path,
    report: &DoctorReport,
    ctx: &OutputContext,
    session: Option<&mut DoctorRepairSession>,
) -> bool {
    let has_warning = report
        .checks
        .iter()
        .any(|c| c.name == "jsonl.merge_artifacts" && c.status == CheckStatus::Warn);
    if !has_warning {
        return false;
    }
    let Some(session) = session else {
        if !ctx.is_json() {
            ctx.warning(
                "Skipping merge-artifact quarantine: no doctor repair session (run-dir creation failed)",
            );
        }
        return false;
    };

    let mut artifacts: Vec<PathBuf> = Vec::new();
    let entries = match beads_dir.read_dir() {
        Ok(entries) => entries,
        Err(err) => {
            if !ctx.is_json() {
                ctx.warning(&format!(
                    "Skipping merge-artifact quarantine: could not read {}: {err}",
                    beads_dir.display()
                ));
            }
            return false;
        }
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        // The canonical 3-way merge anchor is sacred — never quarantine it
        // (sync/mod.rs writes this for legitimate merges).
        if name == "beads.base.jsonl" {
            continue;
        }
        if name.contains(".base.jsonl")
            || name.contains(".left.jsonl")
            || name.contains(".right.jsonl")
        {
            artifacts.push(entry.path());
        }
    }

    if artifacts.is_empty() {
        return false;
    }

    session.set_fixer("doctor.merge_artifact_quarantine");
    let mut quarantined = 0_usize;
    for source in &artifacts {
        let Some(name) = source.file_name() else {
            continue;
        };
        let dest = session
            .run
            .root
            .join("quarantine")
            .join(".beads")
            .join(name);
        match chokepoint::mutate(&session.ctx, source, Op::Rename { to: dest.clone() }) {
            Ok(result) if result.ok => {
                quarantined += 1;
            }
            Ok(_) => {
                if !ctx.is_json() {
                    ctx.warning(&format!(
                        "Merge-artifact quarantine no-op for {}",
                        source.display()
                    ));
                }
            }
            Err(err) => {
                if !ctx.is_json() {
                    ctx.warning(&format!("Failed to quarantine {}: {err}", source.display()));
                }
            }
        }
    }

    if quarantined > 0 && !ctx.is_json() {
        ctx.info(&format!(
            "Quarantined {quarantined} stuck merge artifact(s) under {}",
            session.run.root.join("quarantine/.beads").display()
        ));
    }

    quarantined > 0
}

/// Pass-4 cycle 2 — fixer for `fm-configs-startup-cache-poisoned`.
///
/// When `--repair` is passed and `startup_cache.health` is Warn, move the
/// poisoned current-key `startup-*.json` file from the resolved cache dir
/// into `<run-dir>/quarantine/startup-cache/<filename>` via
/// [`chokepoint::mutate(Op::Rename)`]. The cache dir lives OUTSIDE the
/// default workspace (`$XDG_CACHE_HOME/beads/startup/` or similar),
/// so the fixer extends `session.ctx.capabilities.write_scopes` to
/// include the resolved cache dir for the duration of the call. Per
/// AGENTS.md RULE 1, the fixer renames — never unlinks; `doctor undo`
/// byte-reverses the move.
///
/// Returns `true` if at least one poisoned file was quarantined.
fn fix_startup_cache_if_warned(
    beads_dir: &Path,
    db_override: Option<&PathBuf>,
    report: &DoctorReport,
    ctx: &OutputContext,
    session: Option<&mut DoctorRepairSession>,
) -> bool {
    let poisoned = config::doctor_inspect_startup_cache(beads_dir, db_override);
    let cache_dir = config::doctor_startup_cache_dir();
    fix_startup_cache_entries_if_warned(&poisoned, &cache_dir, report, ctx, session)
}

fn fix_startup_cache_entries_if_warned(
    poisoned: &[config::PoisonedStartupCacheFile],
    cache_dir: &Path,
    report: &DoctorReport,
    ctx: &OutputContext,
    session: Option<&mut DoctorRepairSession>,
) -> bool {
    let has_warning = report
        .checks
        .iter()
        .any(|c| c.name == "startup_cache.health" && c.status == CheckStatus::Warn);
    if !has_warning {
        return false;
    }
    let Some(session) = session else {
        if !ctx.is_json() {
            ctx.warning(
                "Skipping startup-cache quarantine: no doctor repair session (run-dir creation failed)",
            );
        }
        return false;
    };

    if poisoned.is_empty() {
        // Detector saw poisoning but it cleared between report and fix —
        // nothing to do.
        return false;
    }

    // The cache dir is outside the default workspace scope ({.beads/,
    // .doctor/}). Authorize the mutation by appending the resolved
    // cache dir to the session's write_scopes. The chokepoint's
    // `ensure_in_scope` accepts any path whose canonical form starts
    // with a registered scope, so a single push is enough.
    if !session
        .ctx
        .capabilities
        .write_scopes
        .iter()
        .any(|s| s == cache_dir)
    {
        session
            .ctx
            .capabilities
            .write_scopes
            .push(cache_dir.to_path_buf());
    }

    session.set_fixer("doctor.startup_cache_quarantine");
    let mut quarantined = 0_usize;
    for entry in poisoned {
        let Some(name) = entry.path.file_name() else {
            continue;
        };
        let dest = session
            .run
            .root
            .join("quarantine")
            .join("startup-cache")
            .join(name);
        match chokepoint::mutate(&session.ctx, &entry.path, Op::Rename { to: dest.clone() }) {
            Ok(result) if result.ok => {
                quarantined += 1;
            }
            Ok(_) => {
                if !ctx.is_json() {
                    ctx.warning(&format!(
                        "Startup-cache quarantine no-op for {}",
                        entry.path.display()
                    ));
                }
            }
            Err(err) => {
                if !ctx.is_json() {
                    ctx.warning(&format!(
                        "Failed to quarantine {}: {err}",
                        entry.path.display()
                    ));
                }
            }
        }
    }

    if quarantined > 0 && !ctx.is_json() {
        ctx.info(&format!(
            "Quarantined {quarantined} poisoned startup-cache file(s) under {}",
            session.run.root.join("quarantine/startup-cache").display()
        ));
    }

    quarantined > 0
}

/// Pass-4 cycle 3 — fixer for `fm-state_files-recovery-artifacts-orphaned`.
///
/// When `--repair` is passed and `db.recovery_artifacts.aged` is Warn,
/// move every past-TTL recovery artifact (per `recovery_artifacts_aged`)
/// into `<run-dir>/quarantine/.beads/.br_recovery/<filename>` via
/// [`chokepoint::mutate(Op::Rename)`]. RECENT artifacts (younger than
/// `RECOVERY_AGED_TTL_DAYS`) are PRESERVED IN PLACE — operators
/// commonly need recent backups for forensic value. Per AGENTS.md
/// RULE 1, the fixer NEVER unlinks; rename means `doctor undo
/// <run-id>` byte-reverses the quarantine.
///
/// Returns `true` if at least one aged artifact was quarantined.
fn fix_recovery_artifacts_aged_if_warned(
    beads_dir: &Path,
    db_path: &Path,
    report: &DoctorReport,
    ctx: &OutputContext,
    session: Option<&mut DoctorRepairSession>,
) -> bool {
    let has_warning = report
        .checks
        .iter()
        .any(|c| c.name == "db.recovery_artifacts.aged" && c.status == CheckStatus::Warn);
    if !has_warning {
        return false;
    }
    let Some(session) = session else {
        if !ctx.is_json() {
            ctx.warning(
                "Skipping recovery-artifact quarantine: no doctor repair session (run-dir creation failed)",
            );
        }
        return false;
    };

    let aged = match recovery_artifacts_aged(beads_dir, db_path) {
        Ok(items) => items,
        Err(err) => {
            if !ctx.is_json() {
                ctx.warning(&format!(
                    "Failed to enumerate aged recovery artifacts: {err}"
                ));
            }
            return false;
        }
    };
    if aged.is_empty() {
        return false;
    }

    session.set_fixer("doctor.recovery_artifacts_aged_quarantine");
    let mut quarantined = 0_usize;
    for source in &aged {
        let Some(name) = source.file_name() else {
            continue;
        };
        // Preserve the .br_recovery/ subdirectory hint in the quarantine
        // layout so an operator inspecting the run-dir can tell which
        // artifacts came from the recovery dir vs which were bare
        // .bad_<TS> siblings. The destination always lives under
        // .doctor/ (in scope) regardless of source location.
        let dest = session
            .run
            .root
            .join("quarantine")
            .join(".beads")
            .join(".br_recovery")
            .join(name);
        match chokepoint::mutate(&session.ctx, source, Op::Rename { to: dest.clone() }) {
            Ok(result) if result.ok => {
                quarantined += 1;
            }
            Ok(_) => {
                if !ctx.is_json() {
                    ctx.warning(&format!(
                        "Aged-artifact quarantine no-op for {}",
                        source.display()
                    ));
                }
            }
            Err(err) => {
                if !ctx.is_json() {
                    ctx.warning(&format!("Failed to quarantine {}: {err}", source.display()));
                }
            }
        }
    }

    if quarantined > 0 && !ctx.is_json() {
        ctx.info(&format!(
            "Quarantined {quarantined} aged recovery artifact(s) under {}",
            session
                .run
                .root
                .join("quarantine/.beads/.br_recovery")
                .display()
        ));
    }

    quarantined > 0
}

/// Pass-4 cycle 4 — fixer for `fm-caches_indexes-export-hash-cache-divergence`.
///
/// When `db.export_hash_cache` is Warn under `--repair`, recompute
/// `compute_jsonl_hash(jsonl_path)` and update the
/// `metadata.jsonl_content_hash` row to match. Routes through
/// [`chokepoint::mutate(Op::DbExec)`] with `affected_tables=["metadata"]`
/// and `affected_predicate="key='jsonl_content_hash'"` so the
/// pre-state is snapshotted as a JSON row and `doctor undo` can
/// restore the original cache value byte-deterministically.
///
/// JSONL on disk is NEVER touched — the cache is derived from the
/// JSONL, never the other way around. The fixer ASSERTS this contract
/// by refusing to run if `jsonl_path` is missing (returns false).
/// The database path is the same resolved path inspected by the detector;
/// callers may use a configured DB filename instead of `.beads/beads.db`.
///
/// Returns `true` if the cache row was updated.
fn fix_export_hash_cache_divergence_if_warned(
    db_path: &Path,
    jsonl_path: Option<&Path>,
    report: &DoctorReport,
    ctx: &OutputContext,
    session: Option<&mut DoctorRepairSession>,
) -> bool {
    let has_warning = report
        .checks
        .iter()
        .any(|c| c.name == "db.export_hash_cache" && c.status == CheckStatus::Warn);
    if !has_warning {
        return false;
    }
    let Some(jsonl) = jsonl_path else {
        if !ctx.is_json() {
            ctx.warning(
                "Skipping export-hash-cache repair: no JSONL path available (authoritative source missing)",
            );
        }
        return false;
    };
    let Some(session) = session else {
        if !ctx.is_json() {
            ctx.warning(
                "Skipping export-hash-cache repair: no doctor repair session (run-dir creation failed)",
            );
        }
        return false;
    };

    let Ok(computed) = crate::sync::compute_jsonl_hash(jsonl).inspect_err(|err| {
        if !ctx.is_json() {
            ctx.warning(&format!(
                "Skipping export-hash-cache repair: failed to compute current JSONL hash: {err}"
            ));
        }
    }) else {
        return false;
    };

    session.set_fixer("doctor.export_hash_cache_repair");
    let op = Op::DbExec {
        sql: "UPDATE metadata SET value = ?1 WHERE key = 'jsonl_content_hash'".to_string(),
        args: vec![chokepoint::DbArg::Text(computed.clone())],
        affected_tables: vec!["metadata".to_string()],
        affected_predicate: Some("key = 'jsonl_content_hash'".to_string()),
    };
    match chokepoint::mutate(&session.ctx, db_path, op) {
        Ok(result) if result.ok => {
            if !ctx.is_json() {
                ctx.info(&format!(
                    "Recomputed metadata.jsonl_content_hash = {}",
                    &computed[..16.min(computed.len())]
                ));
            }
            true
        }
        Ok(_) => false,
        Err(err) => {
            if !ctx.is_json() {
                ctx.warning(&format!("Failed to repair export-hash cache: {err}"));
            }
            false
        }
    }
}

/// Pass-5 cycle 5 — fixer for the SYMLINK subset of
/// `fm-state_files-base-jsonl-missing-or-stale`.
///
/// When the doctor's `base_jsonl` check warns with `details.kind ==
/// "symlink"`, the fixer renames the symlinked anchor into
/// `<run-dir>/quarantine/.beads/beads.base.jsonl` via
/// [`chokepoint::mutate(Op::Rename)`]. Per AGENTS.md RULE 1: rename,
/// never delete. `fs::rename` preserves the link bytes, so the
/// quarantined entry is itself a symlink.
///
/// REVERSIBILITY CAVEAT: the chokepoint's verbatim backup uses
/// `fs::read`/`fs::copy`, which FOLLOW symlinks — so the backup
/// captures the link target's *content*, not the link bytes. On
/// `doctor undo`, `restore_rename`'s `Path::exists()` follows the
/// quarantined symlink's now-orphaned relative target, finds it
/// missing, and falls back to `Op::WriteFile` with the byte backup.
/// The net effect is that undo restores the original path as a
/// REGULAR FILE containing the target's content, not as a symlink.
/// This is a known reversibility gap for symlink sources (the
/// chokepoint's data model is byte-oriented); see
/// `tests/doctor_fixtures/base_jsonl_symlink_quarantine` for the
/// full analysis. Forward quarantine (the security goal — getting
/// the attacker-controlled symlink out of the merge path) is correct
/// regardless.
///
/// Scope of this cycle: SYMLINK case only. The stale-anchor case stays
/// detect-only because regenerating the anchor is operationally what
/// `br sync --flush-only` already does — having the doctor duplicate
/// that behavior would add a second authoritative path for the same
/// derivation, complicating the chokepoint's "fix did not eliminate
/// the finding" verify step under partial filesystems.
///
/// Returns `true` if the symlinked anchor was quarantined.
fn fix_base_jsonl_symlink_if_warned(
    beads_dir: &Path,
    report: &DoctorReport,
    ctx: &OutputContext,
    session: Option<&mut DoctorRepairSession>,
) -> bool {
    // Locate the base_jsonl Warn whose details.kind == "symlink".
    let symlink_finding = report.checks.iter().find(|c| {
        c.name == "base_jsonl"
            && c.status == CheckStatus::Warn
            && c.details
                .as_ref()
                .and_then(|d| d.get("kind"))
                .and_then(|v| v.as_str())
                == Some("symlink")
    });
    let Some(_) = symlink_finding else {
        return false;
    };
    let Some(session) = session else {
        if !ctx.is_json() {
            ctx.warning(
                "Skipping base-jsonl symlink quarantine: no doctor repair session (run-dir creation failed)",
            );
        }
        return false;
    };

    let source = beads_dir.join("beads.base.jsonl");
    // Re-confirm symlink at fix time (TOCTOU defense — if the operator
    // moved the symlink between detect and fix, we want to no-op
    // rather than mutate an unintended target).
    match fs::symlink_metadata(&source) {
        Ok(meta) if meta.file_type().is_symlink() => {}
        _ => return false,
    }

    let dest = session
        .run
        .root
        .join("quarantine")
        .join(".beads")
        .join("beads.base.jsonl");
    session.set_fixer("doctor.base_jsonl_symlink_quarantine");
    match chokepoint::mutate(&session.ctx, &source, Op::Rename { to: dest.clone() }) {
        Ok(result) if result.ok => {
            if !ctx.is_json() {
                ctx.info(&format!(
                    "Quarantined symlinked merge anchor to {}",
                    dest.display()
                ));
            }
            true
        }
        Ok(_) => false,
        Err(err) => {
            if !ctx.is_json() {
                ctx.warning(&format!("Failed to quarantine base.jsonl symlink: {err}"));
            }
            false
        }
    }
}

/// Pass-5 cycle 6 — fixer for the STALE subset of
/// `fm-state_files-base-jsonl-missing-or-stale`.
///
/// When the doctor's `base_jsonl` check warns with `details.kind ==
/// "stale"`, regenerate `.beads/beads.base.jsonl` from the current
/// `.beads/issues.jsonl` bytes via [`chokepoint::mutate(Op::WriteFile)`].
/// This is exactly what `br sync --flush-only` produces as a side
/// effect of a clean export; the doctor surfaces it as a named repair
/// so operators don't need to remember which sync command rewrites
/// the merge anchor.
///
/// Combined with cycle 5's symlink quarantine, this completes
/// Tier B → Tier A for both detector-emitted subsets of the FM. The
/// chokepoint snapshots the pre-fix anchor bytes as the verbatim
/// backup so `doctor undo` restores the stale anchor byte-for-byte
/// (occasionally useful when the operator wants to compare against
/// the prior anchor for forensics).
///
/// Returns `true` if the stale anchor was regenerated.
fn fix_base_jsonl_stale_if_warned(
    beads_dir: &Path,
    report: &DoctorReport,
    ctx: &OutputContext,
    session: Option<&mut DoctorRepairSession>,
) -> bool {
    let stale_finding = report.checks.iter().find(|c| {
        c.name == "base_jsonl"
            && c.status == CheckStatus::Warn
            && c.details
                .as_ref()
                .and_then(|d| d.get("kind"))
                .and_then(|v| v.as_str())
                == Some("stale")
    });
    if stale_finding.is_none() {
        return false;
    }
    let Some(session) = session else {
        if !ctx.is_json() {
            ctx.warning(
                "Skipping base-jsonl regeneration: no doctor repair session (run-dir creation failed)",
            );
        }
        return false;
    };

    let live = beads_dir.join("issues.jsonl");
    let anchor = beads_dir.join("beads.base.jsonl");
    let live_bytes = match fs::read(&live) {
        Ok(b) => b,
        Err(err) => {
            if !ctx.is_json() {
                ctx.warning(&format!(
                    "Skipping base-jsonl regeneration: cannot read {}: {err}",
                    live.display()
                ));
            }
            return false;
        }
    };
    // Defensive: never regenerate from an empty JSONL (would silently
    // truncate the anchor and lose any forensic value). The detector
    // already skips when live is empty, but re-check at fix time
    // (TOCTOU defense).
    if live_bytes.is_empty() {
        return false;
    }

    // The detector deliberately uses only mtimes so its common path stays
    // stat-only. A later write can therefore make issues.jsonl newer even
    // when its bytes still match the anchor. Do not turn that harmless
    // timestamp drift into a recorded mutation: repair must be idempotent,
    // and an identical anchor is already the exact desired state.
    if fs::read(&anchor).is_ok_and(|anchor_bytes| anchor_bytes == live_bytes) {
        return false;
    }

    session.set_fixer("doctor.base_jsonl_regen");
    match chokepoint::mutate(
        &session.ctx,
        &anchor,
        Op::WriteFile {
            content: live_bytes,
            mode: None,
        },
    ) {
        Ok(result) if result.ok => {
            if !ctx.is_json() {
                ctx.info(&format!(
                    "Regenerated merge anchor {} from current JSONL",
                    anchor.display()
                ));
            }
            true
        }
        Ok(_) => false,
        Err(err) => {
            if !ctx.is_json() {
                ctx.warning(&format!("Failed to regenerate base.jsonl anchor: {err}"));
            }
            false
        }
    }
}

/// Pass-5 cycle 16: quarantine fixer for
/// `fm-state_files-orphan-tmp-files`.
///
/// When `tmp_files_orphan` Warn is present under `--repair`, walks the
/// orphan list (re-detected at fix time for TOCTOU defense) and
/// renames each past-threshold tmp file into
/// `<run-dir>/quarantine/.beads/<filename>` via
/// [`chokepoint::mutate(Op::Rename)`]. Per AGENTS.md RULE 1: rename,
/// never delete. `doctor undo` byte-restores the original tmp files.
///
/// The detector itself stays as the source of truth for what counts
/// as "orphan" — this fixer re-runs the same time-threshold check at
/// fix time so a peer-process write that just landed isn't
/// inadvertently quarantined.
///
/// Lifts the FM from Tier B (cycle 15) to Tier A.
fn fix_orphan_tmp_files_if_warned(
    beads_dir: &Path,
    report: &DoctorReport,
    ctx: &OutputContext,
    session: Option<&mut DoctorRepairSession>,
) -> bool {
    use std::time::{Duration, SystemTime};
    let has_warning = report
        .checks
        .iter()
        .any(|c| c.name == "tmp_files_orphan" && c.status == CheckStatus::Warn);
    if !has_warning {
        return false;
    }
    let Some(session) = session else {
        if !ctx.is_json() {
            ctx.warning(
                "Skipping orphan-tmp quarantine: no doctor repair session (run-dir creation failed)",
            );
        }
        return false;
    };

    // Re-run the detector at fix time. Anything that's no-longer-orphan
    // (e.g., a peer process just wrote it) must not be touched.
    let Ok(entries) = fs::read_dir(beads_dir) else {
        return false;
    };
    let now = SystemTime::now();
    let threshold = Duration::from_secs(ORPHAN_TMP_AGE_THRESHOLD_SECS);
    let mut orphans: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        if let Some((_name, path)) = orphan_tmp_entry(&entry, now, threshold) {
            orphans.push(path);
        }
    }
    orphans.sort();
    if orphans.is_empty() {
        return false;
    }

    session.set_fixer("doctor.orphan_tmp_quarantine");
    let mut quarantined = 0_usize;
    for source in &orphans {
        let Some(name) = source.file_name() else {
            continue;
        };
        let dest = session
            .run
            .root
            .join("quarantine")
            .join(".beads")
            .join(name);
        match chokepoint::mutate(&session.ctx, source, Op::Rename { to: dest.clone() }) {
            Ok(result) if result.ok => quarantined += 1,
            Ok(_) => {
                if !ctx.is_json() {
                    ctx.warning(&format!(
                        "Orphan-tmp quarantine no-op for {}",
                        source.display()
                    ));
                }
            }
            Err(err) => {
                if !ctx.is_json() {
                    ctx.warning(&format!("Failed to quarantine {}: {err}", source.display()));
                }
            }
        }
    }
    if quarantined > 0 && !ctx.is_json() {
        ctx.info(&format!(
            "Quarantined {quarantined} orphan tmp file(s) under {}",
            session.run.root.join("quarantine/.beads").display()
        ));
    }
    quarantined > 0
}

fn read_root_gitignore_content(gitignore_path: &Path) -> Result<Option<String>> {
    let metadata = match fs::symlink_metadata(gitignore_path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };

    if metadata.file_type().is_symlink() {
        return Err(BeadsError::Config(format!(
            "refusing to inspect or repair symlinked root .gitignore: {}",
            gitignore_path.display()
        )));
    }

    if !metadata.is_file() {
        return Err(BeadsError::Config(format!(
            "root .gitignore is not a regular file: {}",
            gitignore_path.display()
        )));
    }

    Ok(Some(fs::read_to_string(gitignore_path)?))
}

fn root_gitignore_temp_path(gitignore_path: &Path, attempt: u32) -> PathBuf {
    let pid = std::process::id();
    let file_name = gitignore_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(".gitignore");
    let temp_name = if attempt == 0 {
        format!("{file_name}.{pid}.tmp")
    } else {
        format!("{file_name}.{pid}.{attempt}.tmp")
    };
    gitignore_path.with_file_name(temp_name)
}

fn create_root_gitignore_temp_file(gitignore_path: &Path) -> Result<(PathBuf, fs::File)> {
    for attempt in 0..64_u32 {
        let temp_path = root_gitignore_temp_path(gitignore_path, attempt);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => return Ok((temp_path, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err.into()),
        }
    }

    Err(BeadsError::Config(format!(
        "Failed to allocate temp .gitignore file for {}",
        gitignore_path.display()
    )))
}

fn write_root_gitignore_atomically(gitignore_path: &Path, contents: &[u8]) -> Result<()> {
    let permissions = fs::symlink_metadata(gitignore_path)?.permissions();
    let (temp_path, mut temp_file) = create_root_gitignore_temp_file(gitignore_path)?;
    if let Err(err) = fs::set_permissions(&temp_path, permissions) {
        tracing::warn!(
            path = %gitignore_path.display(),
            error = %err,
            "Failed to apply original .gitignore permissions before atomic rewrite"
        );
    }
    if let Err(err) = temp_file
        .write_all(contents)
        .and_then(|()| temp_file.sync_all())
    {
        drop(temp_file);
        let _ = fs::remove_file(&temp_path);
        return Err(err.into());
    }
    drop(temp_file);
    crate::util::durable_rename(&temp_path, gitignore_path).inspect_err(|_| {
        let _ = fs::remove_file(&temp_path);
    })?;
    Ok(())
}

fn discover_jsonl(beads_dir: &Path) -> Option<PathBuf> {
    let issues = beads_dir.join("issues.jsonl");
    if issues.exists() {
        return Some(issues);
    }
    let legacy = beads_dir.join("beads.jsonl");
    if legacy.exists() {
        return Some(legacy);
    }
    None
}

fn should_fallback_to_workspace_jsonl(beads_dir: &Path, paths: &config::ConfigPaths) -> bool {
    let has_env_override = std::env::var("BEADS_JSONL").is_ok_and(|value| !value.trim().is_empty());

    !has_env_override
        && paths.metadata.jsonl_export == "issues.jsonl"
        && paths.jsonl_path == beads_dir.join("issues.jsonl")
}

fn select_doctor_jsonl_path(beads_dir: &Path, paths: &config::ConfigPaths) -> Option<PathBuf> {
    if paths.jsonl_path.exists() {
        Some(paths.jsonl_path.clone())
    } else if should_fallback_to_workspace_jsonl(beads_dir, paths) {
        discover_jsonl(beads_dir)
    } else {
        Some(paths.jsonl_path.clone())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JsonlCountState {
    Available(usize),
    Invalid,
    Missing,
    Unreadable,
}

/// ID-set delta between SQLite store and JSONL stream, used by
/// `counts.db_vs_jsonl` to surface the structural shape of any
/// divergence rather than just the cardinality difference. The
/// detection cap (`IdDelta::PER_SIDE_PREVIEW_LIMIT`) keeps the
/// emitted JSON bounded on huge drifts; the counts always reflect
/// the true total even when the previews are clipped.
struct IdDelta {
    only_db: Vec<String>,
    only_jsonl: Vec<String>,
    both_count: usize,
}

impl IdDelta {
    /// Maximum number of ids we serialize per side into the `details`
    /// JSON. Operators almost always want the *first few* divergent
    /// ids so they can grep the JSONL or the DB for the row; the rest
    /// is noise.
    const PER_SIDE_PREVIEW_LIMIT: usize = 50;

    fn to_json(&self) -> serde_json::Value {
        let cap = Self::PER_SIDE_PREVIEW_LIMIT;
        let mut only_db_preview = self.only_db.clone();
        let mut only_jsonl_preview = self.only_jsonl.clone();
        only_db_preview.sort();
        only_jsonl_preview.sort();
        only_db_preview.truncate(cap);
        only_jsonl_preview.truncate(cap);
        serde_json::json!({
            "only_db_count": self.only_db.len(),
            "only_jsonl_count": self.only_jsonl.len(),
            "both_count": self.both_count,
            "only_db": only_db_preview,
            "only_jsonl": only_jsonl_preview,
            "preview_limit": cap,
        })
    }
}

/// Compute the per-id set delta between the DB's `issues` table and
/// the JSONL stream at `jsonl_path`. Both sides honor the same
/// ephemeral/wisp filter the cardinality check uses so the comparison
/// is apples-to-apples. The function is read-only and bounded by the
/// in-memory HashSet of the union of ids — fine for any realistic
/// `.beads/` size.
fn compute_db_jsonl_id_delta(conn: &Connection, jsonl_path: &Path) -> Result<IdDelta> {
    use std::collections::HashSet;
    use std::io::{BufRead, BufReader};

    // DB side: include the same filter the cardinality check uses so
    // counts and ids agree on the same population.
    let rows = conn.query(
        "SELECT id FROM issues \
         WHERE (ephemeral = 0 OR ephemeral IS NULL) AND id NOT LIKE '%-wisp-%'",
    )?;
    let mut db_ids: HashSet<String> = HashSet::with_capacity(rows.len());
    for row in &rows {
        if let Some(id) = row.get(0).and_then(SqliteValue::as_text) {
            db_ids.insert(id.to_string());
        }
    }

    // JSONL side: re-parse each record. We can't reuse the validator's
    // summary here because it doesn't carry the id set, and threading
    // the set through `JsonlCountState` would require touching every
    // callsite of `check_jsonl`. The re-read is bounded to once per
    // doctor run and only happens on the warn path.
    let file = std::fs::File::open(jsonl_path)?;
    let reader = BufReader::with_capacity(2 * 1024 * 1024, file);
    let mut jsonl_ids: HashSet<String> = HashSet::new();
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Lightweight extract: pull just `id` rather than deserializing
        // the full Issue. Doctor isn't a hot path but the JSONL can be
        // large and we don't need the rest of the fields.
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed)
            && let Some(id) = value.get("id").and_then(serde_json::Value::as_str)
        {
            // Apply the same wisp filter as the DB query so the
            // comparison stays apples-to-apples.
            if id.contains("-wisp-") {
                continue;
            }
            jsonl_ids.insert(id.to_string());
        }
    }

    let mut only_db: Vec<String> = db_ids.difference(&jsonl_ids).cloned().collect();
    let mut only_jsonl: Vec<String> = jsonl_ids.difference(&db_ids).cloned().collect();
    only_db.sort();
    only_jsonl.sort();
    let both_count = db_ids.intersection(&jsonl_ids).count();

    Ok(IdDelta {
        only_db,
        only_jsonl,
        both_count,
    })
}

fn check_jsonl(path: &Path, checks: &mut Vec<CheckResult>) -> Result<JsonlCountState> {
    let summary = validate_jsonl_issue_records(path)?;

    if summary.invalid_count == 0 {
        push_check(
            checks,
            "jsonl.parse",
            CheckStatus::Ok,
            Some(format!("Parsed {} records", summary.record_count)),
            Some(serde_json::json!({
                "path": path.display().to_string(),
                "records": summary.record_count
            })),
        );
        Ok(JsonlCountState::Available(summary.record_count))
    } else {
        let preview = summary.preview_messages();
        push_check(
            checks,
            "jsonl.parse",
            CheckStatus::Error,
            Some(format!(
                "Malformed or invalid issue records: {} ({})",
                summary.invalid_count,
                preview.join("; ")
            )),
            Some(serde_json::json!({
                "path": path.display().to_string(),
                "records": summary.record_count,
                "invalid_lines": summary
                    .failures
                    .iter()
                    .map(|failure| failure.line)
                    .collect::<Vec<_>>(),
                "invalid_count": summary.invalid_count,
                "invalid_examples": summary
                    .failures
                    .iter()
                    .map(|failure| serde_json::json!({
                        "line": failure.line,
                        "error": failure.message
                    }))
                    .collect::<Vec<_>>()
            })),
        );
        Ok(JsonlCountState::Invalid)
    }
}

fn check_db_count(
    conn: &Connection,
    jsonl_count: JsonlCountState,
    jsonl_path: Option<&Path>,
    checks: &mut Vec<CheckResult>,
) -> Result<()> {
    let db_count: i64 = conn.query_row(
        "SELECT count(*) FROM issues WHERE (ephemeral = 0 OR ephemeral IS NULL) AND id NOT LIKE '%-wisp-%'",
    )?
        .get(0)
        .and_then(SqliteValue::as_integer)
        .unwrap_or(0);

    match jsonl_count {
        JsonlCountState::Available(jsonl_count) => {
            check_available_db_count(conn, db_count, jsonl_count, jsonl_path, checks);
        }
        JsonlCountState::Invalid => {
            push_check(
                checks,
                "counts.db_vs_jsonl",
                CheckStatus::Warn,
                Some("JSONL is invalid; cannot compare counts".to_string()),
                Some(serde_json::json!({ "db": db_count })),
            );
        }
        JsonlCountState::Missing => {
            push_check(
                checks,
                "counts.db_vs_jsonl",
                CheckStatus::Warn,
                Some("JSONL not found; cannot compare counts".to_string()),
                Some(serde_json::json!({ "db": db_count })),
            );
        }
        JsonlCountState::Unreadable => {
            push_check(
                checks,
                "counts.db_vs_jsonl",
                CheckStatus::Warn,
                Some("JSONL unreadable; cannot compare counts".to_string()),
                Some(serde_json::json!({ "db": db_count })),
            );
        }
    }

    Ok(())
}

fn check_available_db_count(
    conn: &Connection,
    db_count: i64,
    jsonl_count: usize,
    jsonl_path: Option<&Path>,
    checks: &mut Vec<CheckResult>,
) {
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let db_count_usize = db_count as usize;

    if db_count_usize == jsonl_count {
        if push_matching_count_id_delta_check(conn, db_count, jsonl_count, jsonl_path, checks) {
            return;
        }
        push_check(
            checks,
            "counts.db_vs_jsonl",
            CheckStatus::Ok,
            Some(format!("Both have {db_count} records")),
            None,
        );
        return;
    }

    let mut details = serde_json::json!({
        "db": db_count,
        "jsonl": jsonl_count,
    });
    if let Some(path) = jsonl_path {
        match compute_db_jsonl_id_delta(conn, path) {
            Ok(delta) => details["id_delta"] = delta.to_json(),
            Err(delta_err) => {
                details["id_delta_error"] = serde_json::json!(delta_err.to_string());
            }
        }
    }
    push_check(
        checks,
        "counts.db_vs_jsonl",
        CheckStatus::Warn,
        Some("DB and JSONL counts differ".to_string()),
        Some(details),
    );
}

fn push_matching_count_id_delta_check(
    conn: &Connection,
    db_count: i64,
    jsonl_count: usize,
    jsonl_path: Option<&Path>,
    checks: &mut Vec<CheckResult>,
) -> bool {
    let Some(path) = jsonl_path else {
        return false;
    };

    match compute_db_jsonl_id_delta(conn, path) {
        Ok(delta) if !delta.only_db.is_empty() || !delta.only_jsonl.is_empty() => {
            push_check(
                checks,
                "counts.db_vs_jsonl",
                CheckStatus::Warn,
                Some(format!(
                    "DB and JSONL counts match ({}) but id sets diverge: {} only in DB, {} only in JSONL",
                    db_count,
                    delta.only_db.len(),
                    delta.only_jsonl.len(),
                )),
                Some(serde_json::json!({
                    "db": db_count,
                    "jsonl": jsonl_count,
                    "id_delta": delta.to_json(),
                })),
            );
            true
        }
        Ok(_) => false,
        Err(delta_err) => {
            push_check(
                checks,
                "counts.db_vs_jsonl",
                CheckStatus::Warn,
                Some(format!(
                    "DB and JSONL counts match ({db_count}) but id-set verification failed: {delta_err}"
                )),
                Some(serde_json::json!({
                    "db": db_count,
                    "jsonl": jsonl_count,
                    "id_delta_error": delta_err.to_string(),
                })),
            );
            true
        }
    }
}

// ============================================================================
// SYNC SAFETY CHECKS (beads_rust-0v1.2.6)
// ============================================================================

/// Check if the JSONL path is within the sync allowlist.
///
/// This validates that the JSONL path:
/// 1. Does not target git internals (.git/)
/// 2. Is within the .beads directory, or passes the configured external-path policy
/// 3. Has an allowed extension
#[allow(clippy::too_many_lines)]
fn check_sync_jsonl_path(jsonl_path: &Path, beads_dir: &Path, checks: &mut Vec<CheckResult>) {
    let check_name = "sync_jsonl_path";

    // 1. Check if path is valid UTF-8
    if let Some(_name) = jsonl_path.file_name().and_then(|n| n.to_str()) {
        // 2. Check for git path access (critical safety invariant)
        let git_check = validate_no_git_path(jsonl_path);
        if !git_check.is_allowed() {
            let reason = git_check.rejection_reason().unwrap_or_default();
            push_check(
                checks,
                check_name,
                CheckStatus::Error,
                Some(format!("JSONL path targets git internals: {reason}")),
                Some(serde_json::json!({
                    "path": jsonl_path.display().to_string(),
                    "reason": reason,
                    "remediation": "Move JSONL file inside .beads/ directory"
                })),
            );
            return;
        }

        let is_external = config::resolved_jsonl_path_is_external(beads_dir, jsonl_path);
        if is_external {
            match validate_sync_path_with_external(jsonl_path, beads_dir, true) {
                Ok(()) => {
                    push_check(
                        checks,
                        check_name,
                        CheckStatus::Ok,
                        Some("Configured external JSONL path is valid for sync I/O".to_string()),
                        Some(serde_json::json!({
                            "path": jsonl_path.display().to_string(),
                            "beads_dir": beads_dir.display().to_string(),
                            "external": true
                        })),
                    );
                }
                Err(err) => {
                    push_check(
                        checks,
                        check_name,
                        CheckStatus::Error,
                        Some(format!("Configured external JSONL path is invalid: {err}")),
                        Some(serde_json::json!({
                            "path": jsonl_path.display().to_string(),
                            "beads_dir": beads_dir.display().to_string(),
                            "external": true
                        })),
                    );
                }
            }
            return;
        }

        // 3. Check if path is within beads_dir allowlist
        let path_validation = validate_sync_path(jsonl_path, beads_dir);
        match path_validation {
            PathValidation::Allowed => {
                push_check(
                    checks,
                    check_name,
                    CheckStatus::Ok,
                    Some("JSONL path is within sync allowlist".to_string()),
                    Some(serde_json::json!({
                        "path": jsonl_path.display().to_string(),
                        "beads_dir": beads_dir.display().to_string()
                    })),
                );
            }
            PathValidation::OutsideBeadsDir {
                path,
                beads_dir: bd,
            } => {
                push_check(
                    checks,
                    check_name,
                    CheckStatus::Warn,
                    Some("JSONL path is outside .beads/ directory".to_string()),
                    Some(serde_json::json!({
                        "path": path.display().to_string(),
                        "beads_dir": bd.display().to_string(),
                        "remediation": "Use --allow-external-jsonl flag or move JSONL inside .beads/"
                    })),
                );
            }
            PathValidation::DisallowedExtension { path, extension } => {
                push_check(
                    checks,
                    check_name,
                    CheckStatus::Error,
                    Some(format!("JSONL path has disallowed extension: {extension}")),
                    Some(serde_json::json!({
                        "path": path.display().to_string(),
                        "extension": extension,
                        "remediation": "Use a .jsonl extension for JSONL files"
                    })),
                );
            }
            PathValidation::TraversalAttempt { path } => {
                push_check(
                    checks,
                    check_name,
                    CheckStatus::Error,
                    Some("JSONL path contains traversal sequences".to_string()),
                    Some(serde_json::json!({
                        "path": path.display().to_string(),
                        "remediation": "Remove '..' sequences from path"
                    })),
                );
            }
            PathValidation::SymlinkEscape { path, target } => {
                push_check(
                    checks,
                    check_name,
                    CheckStatus::Error,
                    Some("JSONL path is a symlink pointing outside .beads/".to_string()),
                    Some(serde_json::json!({
                        "symlink": path.display().to_string(),
                        "target": target.display().to_string(),
                        "remediation": "Remove symlink and use a regular file inside .beads/"
                    })),
                );
            }
            PathValidation::CanonicalizationFailed { path, error } => {
                push_check(
                    checks,
                    check_name,
                    CheckStatus::Warn,
                    Some(format!("Could not verify JSONL path: {error}")),
                    Some(serde_json::json!({
                        "path": path.display().to_string(),
                        "error": error
                    })),
                );
            }
            PathValidation::NonRegularFile { path } => {
                push_check(
                    checks,
                    check_name,
                    CheckStatus::Error,
                    Some("JSONL path is not a regular file".to_string()),
                    Some(serde_json::json!({
                        "path": path.display().to_string(),
                        "remediation": "Replace the path with a regular .jsonl file"
                    })),
                );
            }
            PathValidation::GitPathAttempt { path } => {
                // Already handled above, but include for completeness
                push_check(
                    checks,
                    check_name,
                    CheckStatus::Error,
                    Some("JSONL path targets git internals".to_string()),
                    Some(serde_json::json!({
                        "path": path.display().to_string(),
                        "remediation": "Move JSONL file inside .beads/ directory"
                    })),
                );
            }
        }
    } else {
        push_check(
            checks,
            check_name,
            CheckStatus::Error,
            Some("Invalid JSONL path (not valid UTF-8)".to_string()),
            Some(serde_json::json!({
                "path": jsonl_path.display().to_string(),
                "remediation": "Ensure the path is valid UTF-8"
            })),
        );
    }
}

/// Check for git merge conflict markers in the JSONL file.
///
/// Conflict markers indicate an unresolved merge and must be resolved
/// before any sync operations can proceed safely.
#[allow(clippy::unnecessary_wraps)]
fn check_sync_conflict_markers(jsonl_path: &Path, checks: &mut Vec<CheckResult>) {
    let check_name = "sync_conflict_markers";

    if !jsonl_path.exists() {
        return;
    }

    match scan_conflict_markers(jsonl_path) {
        Ok(markers) => {
            if markers.is_empty() {
                push_check(
                    checks,
                    check_name,
                    CheckStatus::Ok,
                    Some("No merge conflict markers found".to_string()),
                    None,
                );
            } else {
                // Format first few markers for display
                let preview: Vec<serde_json::Value> = markers
                    .iter()
                    .take(5)
                    .map(|m| {
                        serde_json::json!({
                            "line": m.line,
                            "type": format!("{:?}", m.marker_type),
                            "branch": m.branch.as_deref().unwrap_or("")
                        })
                    })
                    .collect();

                push_check(
                    checks,
                    check_name,
                    CheckStatus::Error,
                    Some(format!(
                        "Found {} merge conflict marker(s) in JSONL",
                        markers.len()
                    )),
                    Some(serde_json::json!({
                        "path": jsonl_path.display().to_string(),
                        "count": markers.len(),
                        "markers_preview": preview,
                        "remediation": "Resolve git merge conflicts in the JSONL file before running sync"
                    })),
                );
            }
        }
        Err(e) => {
            push_check(
                checks,
                check_name,
                CheckStatus::Warn,
                Some(format!("Could not scan for conflict markers: {e}")),
                Some(serde_json::json!({
                    "path": jsonl_path.display().to_string(),
                    "error": e.to_string()
                })),
            );
        }
    }
}

/// Check sync metadata consistency.
///
/// Validates that sync-related metadata is consistent and not stale.
#[allow(clippy::too_many_lines)]
fn check_sync_metadata(
    conn: &Connection,
    db_path: &Path,
    jsonl_path: Option<&Path>,
    checks: &mut Vec<CheckResult>,
) {
    // Get metadata for diagnostic details
    let last_import = latest_metadata_value(conn, "last_import_time");
    let last_export = latest_metadata_value(conn, "last_export_time");
    let jsonl_hash = latest_metadata_value(conn, "jsonl_content_hash");

    // Check dirty issues count
    let dirty_count: i64 = conn
        .query_row("SELECT count(*) FROM dirty_issues")
        .ok()
        .and_then(|row| row.get(0).and_then(SqliteValue::as_integer))
        .unwrap_or(0);

    let mut details = serde_json::json!({
        "dirty_issues": dirty_count
    });

    if let Some(ts) = &last_import {
        details["last_import"] = serde_json::json!(ts);
    }
    if let Some(ts) = &last_export {
        details["last_export"] = serde_json::json!(ts);
    }
    if let Some(hash) = &jsonl_hash {
        details["jsonl_hash"] = serde_json::json!(&hash[..16.min(hash.len())]);
    }

    // Determine staleness using the canonical compute_staleness() from sync module.
    // This avoids duplicating logic that accounts for last_export_time, mtime witness
    // fast-path, and content hash verification (issue #173).
    let (jsonl_exists, jsonl_newer, db_newer) = if let Some(p) = jsonl_path {
        match SqliteStorage::open(db_path).and_then(|storage| compute_staleness(&storage, p)) {
            Ok(staleness) => (
                staleness.jsonl_exists,
                staleness.jsonl_newer,
                staleness.db_newer,
            ),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "compute_staleness failed in doctor; falling back to dirty-count only"
                );
                (p.exists(), false, dirty_count > 0)
            }
        }
    } else {
        (false, false, dirty_count > 0)
    };
    details["jsonl_exists"] = serde_json::json!(jsonl_exists);
    details["jsonl_newer"] = serde_json::json!(jsonl_newer);
    details["db_newer"] = serde_json::json!(db_newer);
    details["pending_import"] = serde_json::json!(jsonl_newer);
    details["pending_export"] = serde_json::json!(db_newer);

    match (jsonl_newer, db_newer) {
        (false, false) => {
            push_check(
                checks,
                "sync.metadata",
                CheckStatus::Ok,
                Some("Database and JSONL are in sync".to_string()),
                Some(details),
            );
        }
        (true, false) => {
            // beads_rust#330: report the pending-import direction at the
            // same advisory severity as the pending-export direction so
            // the two drift directions are consistent.
            //
            // Exception: a *fresh* workspace (`br init`) leaves an empty
            // `issues.jsonl` whose mtime trails the DB's last_export, so
            // `jsonl_newer` reads true even though there is nothing to
            // import (the empty JSONL and an empty DB agree). Flagging
            // that as Warn would make `br doctor` exit non-zero on a
            // brand-new, perfectly healthy workspace. Only Warn when the
            // JSONL actually carries importable content; the benign
            // empty-JSONL case stays Ok with a clarifying message.
            let jsonl_has_importable_content = jsonl_path
                .and_then(|p| std::fs::metadata(p).ok())
                .is_some_and(|meta| meta.len() > 0);
            if jsonl_has_importable_content {
                push_check(
                    checks,
                    "sync.metadata",
                    CheckStatus::Warn,
                    Some("External changes pending import".to_string()),
                    Some(details),
                );
            } else {
                push_check(
                    checks,
                    "sync.metadata",
                    CheckStatus::Ok,
                    Some(
                        "External changes pending import (empty JSONL, nothing to import)"
                            .to_string(),
                    ),
                    Some(details),
                );
            }
        }
        (false, true) => {
            let message = if last_export.is_none() && dirty_count > 0 {
                "Local changes exist but no export is recorded; consider running sync --flush-only"
            } else {
                "Local changes pending export"
            };
            push_check(
                checks,
                "sync.metadata",
                CheckStatus::Warn,
                Some(message.to_string()),
                Some(details),
            );
        }
        (true, true) => {
            push_check(
                checks,
                "sync.metadata",
                CheckStatus::Warn,
                Some("Database and JSONL have diverged (merge required)".to_string()),
                Some(details),
            );
        }
    }
}

/// Recovery handler for `br doctor --repair-indexes` (beads_rust#288).
///
/// Walks every user index attached to the `issues` table family and
/// runs `REINDEX "<name>"` inside a single transaction with a verbatim
/// pre-snapshot backup of `beads.db`. Distinct from `--repair`
/// (which is `--rebuild` in disguise, destructive on the tombstone
/// preservation contract): this path mutates only the index B-trees,
/// never issue rows. Operators reach for this when
/// `PRAGMA integrity_check` returns ok but `br doctor` reports
/// "index <name> contains rowid N for a table row that does not
/// satisfy the partial index predicate" — that's older SQLite not
/// validating partial predicates on `integrity_check`.
#[allow(clippy::too_many_lines)]
fn execute_repair_indexes(
    beads_dir: &Path,
    paths: &config::ConfigPaths,
    ctx: &OutputContext,
    args: &DoctorArgs,
    cli: &config::CliOverrides,
) -> Result<()> {
    // Acquire the workspace write lock the same way --repair does.
    // A REINDEX inside a transaction is a write, and concurrency with
    // another writer is operator error.
    let write_authority = if let Some(authority) =
        cli.database_family_write_authority_for(beads_dir, &paths.db_path)
    {
        if let Err(err) = authority.verify_database_authority() {
            emit_concurrency_lost(beads_dir, &err, ctx, "--repair-indexes");
            crate::shutdown::exit_process(DoctorExitCode::ConcurrencyLost.as_i32());
        }
        Arc::clone(authority)
    } else {
        match acquire_doctor_database_write_authority(beads_dir, &paths.db_path, Some(0)) {
            Ok(authority) => authority,
            Err(err) => {
                emit_concurrency_lost(beads_dir, &err, ctx, "--repair-indexes");
                crate::shutdown::exit_process(DoctorExitCode::ConcurrencyLost.as_i32());
            }
        }
    };

    match refuse_gates::run_all(beads_dir, &paths.db_path) {
        GateOutcome::Allow => {}
        GateOutcome::Refuse {
            code: _,
            reason,
            evidence,
        } => {
            emit_refused_unsafe("--repair-indexes", &reason, &evidence, ctx);
            crate::shutdown::exit_process(DoctorExitCode::RefusedUnsafe.as_i32());
        }
    }

    if !args.dry_run {
        refuse_doctor_mutation_if_merge_pending(
            "--repair-indexes",
            &paths.db_path,
            &write_authority,
            ctx,
        );
    }

    // Pre-snapshot backup. Same shape `--repair` uses: copy the live
    // DB to a sidecar before mutating, restore on any failure inside
    // the REINDEX transaction. The snapshot path lives next to the
    // DB so it shares the same filesystem and rename is atomic.
    let snapshot_path = paths.db_path.with_extension("db.pre-repair-indexes");
    let wal_path = PathBuf::from(format!("{}-wal", paths.db_path.to_string_lossy()));
    let shm_path = PathBuf::from(format!("{}-shm", paths.db_path.to_string_lossy()));

    if args.dry_run {
        ctx.info(&format!(
            "[dry-run] Would snapshot {} -> {} and REINDEX every user index on the issues family",
            paths.db_path.display(),
            snapshot_path.display(),
        ));
        return Ok(());
    }

    checkpoint_and_snapshot_repair_indexes(&paths.db_path, &snapshot_path, &write_authority)?;

    // Open the DB and enumerate every user-defined index so we don't
    // reindex sqlite_autoindex_* or any internal index — those are
    // managed by SQLite and not the partial-index class we're after.
    write_authority.verify_database_authority()?;
    let conn = Connection::open(paths.db_path.to_string_lossy().into_owned())?;
    write_authority.verify_database_authority()?;
    let rows = match conn.query(
        "SELECT name FROM sqlite_master \
         WHERE type = 'index' \
           AND name NOT LIKE 'sqlite_autoindex_%' \
           AND sql IS NOT NULL \
         ORDER BY name",
    ) {
        Ok(rows) => rows,
        Err(err) => {
            close_repair_indexes_connection(conn, "after index enumeration failed");
            return Err(err.into());
        }
    };
    let index_names: Vec<String> = rows
        .iter()
        .filter_map(|row| row.get(0).and_then(SqliteValue::as_text).map(String::from))
        .collect();

    if index_names.is_empty() {
        // No user indexes — nothing to reindex. Leave the snapshot in
        // place so the operator has a recoverable pre-state regardless.
        ctx.info("doctor --repair-indexes: no user indexes found; nothing to do");
        conn.close()?;
        write_authority.verify_database_authority()?;
        return Ok(());
    }

    // Run all REINDEX statements inside a single transaction so the
    // tree is either fully repaired or the live DB returns to its
    // pre-state on any failure mid-pass. This avoids the
    // half-rebuilt-tree window the user flagged in their #288 repro
    // (step 2 -> 3 -> 4 iterative discovery).
    if let Err(err) = conn.execute("BEGIN IMMEDIATE") {
        close_repair_indexes_connection(conn, "after BEGIN IMMEDIATE failed");
        return Err(err.into());
    }
    let reindex_result: Result<usize> = (|| {
        write_authority.verify_database_authority()?;
        let mut reindexed_count = 0;
        for name in &index_names {
            conn.execute(&format!("REINDEX {}", quote_sql_identifier(name)))?;
            reindexed_count += 1;
        }
        write_authority.verify_database_authority()?;
        conn.execute("COMMIT")?;
        write_authority.verify_database_authority()?;
        Ok(reindexed_count)
    })();

    match reindex_result {
        Ok(reindexed_count) => {
            conn.close()?;
            write_authority.verify_database_authority()?;
            ctx.success(&format!(
                "doctor --repair-indexes: REINDEX completed on {reindexed_count} user indexes (pre-snapshot retained at {})",
                snapshot_path.display(),
            ));
            Ok(())
        }
        Err(err) => {
            // Rollback inside the connection first, then restore from
            // the pre-snapshot for defense in depth — if rollback
            // itself failed (corrupt WAL etc.), the snapshot is the
            // authoritative pre-state.
            let _ = conn.execute("ROLLBACK");
            tracing::warn!(
                error = %err,
                snapshot = %snapshot_path.display(),
                "doctor --repair-indexes: REINDEX failed; rolling back from pre-snapshot"
            );
            // Close the connection before the file copy so we don't
            // race the SQLite WAL machinery on the live DB.
            close_repair_indexes_connection(conn, "before restoring pre-snapshot");
            restore_repair_indexes_snapshot(
                paths,
                &snapshot_path,
                [&wal_path, &shm_path],
                &err,
                &write_authority,
            )?;
            Err(err)
        }
    }
}

fn checkpoint_and_snapshot_repair_indexes(
    db_path: &Path,
    snapshot_path: &Path,
    write_authority: &Arc<crate::sync::DatabaseFamilyWriteLock>,
) -> Result<()> {
    // WAL-safety contract: checkpoint before snapshot so the `.db`
    // file alone is a complete pre-state for restore-via-copy. When
    // checkpoint fails (concurrent reader holding a snapshot,
    // unsupported journal mode, etc.) we MUST also snapshot the WAL
    // and SHM sidecars so the restore path can restore the full
    // pre-state — otherwise a restore would overwrite the live DB
    // with a snapshot that's missing whatever data the WAL still
    // held, silently destroying committed-but-uncheckpointed work.
    write_authority.verify_database_authority()?;
    let conn = Connection::open(db_path.to_string_lossy().into_owned())?;
    let checkpoint_complete = match wal_checkpoint_truncate_complete(&conn) {
        Ok(complete) => complete,
        Err(checkpoint_err) => {
            tracing::warn!(
                error = %checkpoint_err,
                "doctor --repair-indexes: wal_checkpoint(TRUNCATE) before snapshot failed; will also snapshot WAL/SHM sidecars to preserve full pre-state"
            );
            false
        }
    };
    close_repair_indexes_connection(conn, "after pre-snapshot WAL checkpoint");
    write_authority.verify_database_authority()?;

    // Clear sidecar snapshots from any previous `--repair-indexes`
    // invocation BEFORE writing the new ones. Without this cleanup,
    // a previous run that failed checkpoint (and therefore wrote
    // `<snapshot>-wal` / `<snapshot>-shm`) could leave those files
    // on disk; if the current run's checkpoint succeeds, we
    // wouldn't overwrite them, and the restore path would
    // incorrectly find them and use them as the "pre-state" — a
    // Frankenstein mix of two different points in time. The
    // missing-sidecar-snapshot signal is load-bearing in the
    // restore logic, so we keep it accurate by clearing first.
    let wal_snap = PathBuf::from(format!("{}-wal", snapshot_path.to_string_lossy()));
    let shm_snap = PathBuf::from(format!("{}-shm", snapshot_path.to_string_lossy()));
    for stale in [&wal_snap, &shm_snap] {
        match std::fs::remove_file(stale) {
            Ok(()) => tracing::debug!(
                path = %stale.display(),
                "doctor --repair-indexes: cleared stale sidecar snapshot from a previous run"
            ),
            Err(rm_err) if rm_err.kind() == std::io::ErrorKind::NotFound => {}
            Err(rm_err) => {
                return Err(BeadsError::Internal {
                    message: format!(
                        "doctor --repair-indexes: failed to remove stale sidecar snapshot {}; refusing to continue because restore could incorrectly use it as pre-state: {rm_err}",
                        stale.display(),
                    ),
                });
            }
        }
    }

    ensure_repair_indexes_snapshot_target_safe(snapshot_path)?;
    std::fs::copy(db_path, snapshot_path).map_err(|err| BeadsError::Internal {
        message: format!(
            "doctor --repair-indexes: pre-snapshot backup failed ({}): {err}",
            snapshot_path.display(),
        ),
    })?;
    write_authority.verify_database_authority()?;

    // When checkpoint failed, also snapshot the WAL/SHM sidecars
    // alongside the `.db` so the restore path can restore the full
    // pre-state. We use a sidecar-paths convention parallel to the
    // main snapshot: `<snapshot_path>-wal`, `<snapshot_path>-shm`.
    // When checkpoint succeeded we deliberately skip — the `.db`
    // snapshot is complete, and missing sidecar snapshots are the
    // signal to the restore path that it should delete the live
    // sidecars (they have no valid pre-state to restore).
    if !checkpoint_complete {
        let wal_live = PathBuf::from(format!("{}-wal", db_path.to_string_lossy()));
        let shm_live = PathBuf::from(format!("{}-shm", db_path.to_string_lossy()));
        for (live, snap, kind) in [(&wal_live, &wal_snap, "WAL"), (&shm_live, &shm_snap, "SHM")] {
            if live.exists()
                && let Err(copy_err) = std::fs::copy(live, snap)
            {
                return Err(BeadsError::Internal {
                    message: format!(
                        "doctor --repair-indexes: post-checkpoint-failure {kind} snapshot ({}) failed: {copy_err}",
                        snap.display(),
                    ),
                });
            }
        }
    }
    write_authority.verify_database_authority()?;
    Ok(())
}

fn ensure_repair_indexes_snapshot_target_safe(snapshot_path: &Path) -> Result<()> {
    match fs::symlink_metadata(snapshot_path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(BeadsError::Internal {
            message: format!(
                "doctor --repair-indexes: refusing to write pre-snapshot backup through symlink {}",
                snapshot_path.display(),
            ),
        }),
        Ok(metadata) if !metadata.file_type().is_file() => Err(BeadsError::Internal {
            message: format!(
                "doctor --repair-indexes: refusing to overwrite non-file pre-snapshot target {}",
                snapshot_path.display(),
            ),
        }),
        Ok(_) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(BeadsError::Internal {
            message: format!(
                "doctor --repair-indexes: failed to inspect pre-snapshot target {}: {err}",
                snapshot_path.display(),
            ),
        }),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WalCheckpointStats {
    busy: i64,
    log_frames: i64,
    checkpointed_frames: i64,
}

impl WalCheckpointStats {
    const fn complete(self) -> bool {
        self.busy == 0 && (self.log_frames < 0 || self.checkpointed_frames >= self.log_frames)
    }
}

fn wal_checkpoint_truncate_complete(conn: &Connection) -> std::result::Result<bool, FrankenError> {
    let rows = conn.query("PRAGMA wal_checkpoint(TRUNCATE)")?;
    let Some(row) = rows.first() else {
        return Ok(false);
    };
    Ok(wal_checkpoint_stats_from_row(row).is_some_and(WalCheckpointStats::complete))
}

fn wal_checkpoint_stats_from_row(row: &Row) -> Option<WalCheckpointStats> {
    Some(WalCheckpointStats {
        busy: sqlite_value_i64(row.get(0))?,
        log_frames: sqlite_value_i64(row.get(1))?,
        checkpointed_frames: sqlite_value_i64(row.get(2))?,
    })
}

const fn sqlite_value_i64(value: Option<&SqliteValue>) -> Option<i64> {
    match value {
        Some(SqliteValue::Integer(value)) => Some(*value),
        _ => None,
    }
}

fn restore_repair_indexes_snapshot(
    paths: &config::ConfigPaths,
    snapshot_path: &Path,
    sidecars: [&PathBuf; 2],
    original_err: &BeadsError,
    write_authority: &Arc<crate::sync::DatabaseFamilyWriteLock>,
) -> Result<()> {
    write_authority.verify_database_authority()?;
    std::fs::copy(snapshot_path, &paths.db_path).map_err(|copy_err| BeadsError::Internal {
        message: format!(
            "doctor --repair-indexes: REINDEX failed and pre-snapshot restore also failed: original={original_err}, restore={copy_err}",
        ),
    })?;
    write_authority.verify_database_authority()?;

    // WAL/SHM restore handling. Two paths:
    //
    // (A) `<snapshot>-wal` / `<snapshot>-shm` exist on disk: the
    //     pre-snapshot checkpoint failed and we preserved the live
    //     sidecars verbatim. Copy them back over the live sidecars
    //     so the post-restore state is byte-identical to the
    //     pre-repair-indexes pre-state. This is the only path that
    //     preserves committed-but-uncheckpointed WAL frames the
    //     user had at the time of the call.
    //
    // (B) Sidecar snapshots don't exist: the pre-snapshot checkpoint
    //     succeeded, so the `.db` snapshot is complete and any live
    //     WAL/SHM contains only post-snapshot frames from the failed
    //     REINDEX transaction. Delete the live sidecars so they
    //     can't replay against the restored `.db` and silently undo
    //     the rollback.
    for sidecar in sidecars {
        let sidecar_snapshot = PathBuf::from(format!(
            "{}-{}",
            snapshot_path.display(),
            sidecar_suffix(sidecar).unwrap_or("orphan"),
        ));
        if sidecar_snapshot.exists() {
            match std::fs::copy(&sidecar_snapshot, sidecar) {
                Ok(_) => tracing::debug!(
                    snapshot = %sidecar_snapshot.display(),
                    live = %sidecar.display(),
                    "doctor --repair-indexes: restored sidecar from pre-checkpoint snapshot"
                ),
                Err(copy_err) => {
                    return Err(BeadsError::Internal {
                        message: format!(
                            "doctor --repair-indexes: restored DB snapshot but failed to restore sidecar snapshot {} -> {}: original={original_err}, restore_sidecar={copy_err}",
                            sidecar_snapshot.display(),
                            sidecar.display(),
                        ),
                    });
                }
            }
            // The snapshot copy stays on disk as forensic evidence
            // — operators can verify the restore by comparing the
            // restored sidecar against `<snapshot>-{wal,shm}`.
        } else {
            match std::fs::remove_file(sidecar) {
                Ok(()) => tracing::debug!(
                    path = %sidecar.display(),
                    "doctor --repair-indexes: cleared sidecar after restore (checkpoint succeeded; live sidecar held only post-REINDEX frames)"
                ),
                Err(rm_err) if rm_err.kind() == std::io::ErrorKind::NotFound => {}
                Err(rm_err) => tracing::warn!(
                    error = %rm_err,
                    path = %sidecar.display(),
                    "doctor --repair-indexes: failed to remove sidecar after restore; next open may replay stale WAL frames"
                ),
            }
        }
    }
    write_authority.verify_database_authority()?;
    Ok(())
}

/// Extract the sidecar kind (`wal` or `shm`) from a live sidecar
/// path, so the restore loop can locate the matching snapshot file.
/// Returns `None` if the path doesn't end in `-wal` or `-shm` — that
/// would be a programmer error (the caller is supposed to pass only
/// `.db-wal` / `.db-shm` paths) but we degrade to logging-only
/// rather than panicking.
fn sidecar_suffix(sidecar: &Path) -> Option<&'static str> {
    let s = sidecar.to_string_lossy();
    if s.ends_with("-wal") {
        Some("wal")
    } else if s.ends_with("-shm") {
        Some("shm")
    } else {
        None
    }
}

fn close_repair_indexes_connection(conn: Connection, context: &str) {
    if let Err(close_err) = conn.close() {
        tracing::warn!(
            error = %close_err,
            context,
            "doctor --repair-indexes: failed to close connection"
        );
    }
}

fn quote_sql_identifier(name: &str) -> String {
    let mut quoted = String::with_capacity(name.len() + 2);
    quoted.push('"');
    for c in name.chars() {
        if matches!(c, '"') {
            quoted.push('"');
        }
        quoted.push(c);
    }
    quoted.push('"');
    quoted
}

/// Resolve the effective `no_db` flag for a doctor run the same way the
/// storage layer does at runtime: merge the startup config layers (legacy
/// user / user / project / env) with the CLI override layer and read the
/// resolved `no-db` value. This honors a config-file `no-db: true` as well as
/// the `--no-db` flag. #329
fn resolve_doctor_no_db(beads_dir: &Path, cli: &config::CliOverrides) -> bool {
    // If startup config can't be loaded, fall back to the explicit CLI
    // override (the conservative, DB-running default is `false`).
    let Ok(startup) = config::load_startup_config(beads_dir) else {
        return cli.no_db.unwrap_or(false);
    };
    let merged = config::ConfigLayer::merge_layers(&[startup, cli.as_layer()]);
    config::no_db_from_layer(&merged).unwrap_or(false)
}

#[cfg(test)]
fn collect_doctor_report(beads_dir: &Path, paths: &config::ConfigPaths) -> Result<DoctorRun> {
    collect_doctor_report_with_mode(beads_dir, paths, DoctorInspectionMode::Full)
}

#[cfg(test)]
fn collect_doctor_report_with_mode(
    beads_dir: &Path,
    paths: &config::ConfigPaths,
    mode: DoctorInspectionMode,
) -> Result<DoctorRun> {
    collect_doctor_report_with_mode_and_db_override(beads_dir, paths, None, mode, false)
}

fn collect_doctor_report_for_cli(
    beads_dir: &Path,
    paths: &config::ConfigPaths,
    cli: &config::CliOverrides,
) -> Result<DoctorRun> {
    let no_db = resolve_doctor_no_db(beads_dir, cli);
    collect_doctor_report_with_mode_and_db_override(
        beads_dir,
        paths,
        cli.db.as_ref(),
        DoctorInspectionMode::Full,
        no_db,
    )
}

/// What `br doctor explain` observed for one check when it ran the flat
/// doctor checks against the discovered workspace.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ExplainObservation {
    /// No `.beads/` workspace was discovered, so nothing could be observed.
    NoWorkspace,
    /// The checks ran but none carried this name or finding id.
    NotObserved,
    /// The check ran; this is its result exactly as `br doctor --json` reports it.
    Check(serde_json::Value),
}

/// Run the flat doctor checks (never `--repair`) and return the check whose
/// name is `check_name` or whose details carry `finding_id`.
///
/// Used by `br doctor explain`, which must describe the workspace as it is
/// now: ordinary doctor runs do not persist a report (only `--repair` runs
/// create a run directory), so the checks are re-executed rather than read
/// from disk.
///
/// # Errors
///
/// Propagates path resolution and check execution failures.
pub(crate) fn observe_check_for_explain(
    cli: &config::CliOverrides,
    check_name: &str,
    finding_id: &str,
) -> Result<ExplainObservation> {
    let Some(beads_dir) = config::discover_optional_beads_dir_with_cli(cli)? else {
        return Ok(ExplainObservation::NoWorkspace);
    };
    let paths = config::resolve_paths(&beads_dir, cli.db.as_ref())?;
    let run = collect_doctor_report_for_cli(&beads_dir, &paths, cli)?;
    let observed = run.report.checks.iter().find(|check| {
        check.name == check_name
            || check
                .details
                .as_ref()
                .and_then(|details| details.get("finding_id"))
                .and_then(serde_json::Value::as_str)
                == Some(finding_id)
    });
    match observed {
        Some(check) => Ok(ExplainObservation::Check(
            serde_json::to_value(check).map_err(BeadsError::Json)?,
        )),
        None => Ok(ExplainObservation::NotObserved),
    }
}

fn collect_doctor_report_with_mode_and_db_override(
    beads_dir: &Path,
    paths: &config::ConfigPaths,
    db_override: Option<&PathBuf>,
    mode: DoctorInspectionMode,
    no_db: bool,
) -> Result<DoctorRun> {
    let mut checks = Vec::new();
    // Even --no-db inspects pending merge state below. Check namespace
    // evidence before that read-only open can rebind a quiescent generation.
    let live_database_inspectable =
        !paths.db_path.exists() || check_namespace_identity(&paths.db_path, &mut checks);
    check_merge_artifacts(beads_dir, &paths.jsonl_path, &mut checks)?;
    check_base_jsonl(beads_dir, &mut checks);
    // Pass-5 cycle 10: doctor's own runs dir size (operator-prunable).
    let repo_root = beads_dir.parent().unwrap_or(beads_dir);
    check_doctor_runs_dir_size(repo_root, &mut checks);
    // Pass-5 cycle 28: writability of `.doctor/` so the next --repair
    // won't crash deep inside the chokepoint.
    check_doctor_runs_creatable(repo_root, &mut checks);
    // Pass-5 cycle 30: writability of the active DB recovery dir so the
    // next --repair won't crash mid-backup.
    //
    // #329: this is a DB-backed check (it probes the database's recovery
    // sidecar dir). Under `--no-db` we do not recover the DB, so skip this
    // recovery-readiness check and report it in `db.no_db_mode` below.
    if !no_db {
        check_recovery_dir_writable(&paths.db_path, beads_dir, &mut checks);
    }
    // Pass-5 cycle 31: writability of `.beads/.write.lock` so we don't
    // mask EACCES failures as cryptic "could not open lock" errors.
    check_write_lock_writable(beads_dir, &mut checks);
    // Pass-5 cycle 32: writability of the repo-root `.gitignore` —
    // surfaces a real blocker to the existing gitignore_repair fixer.
    check_root_gitignore_writable(repo_root, &mut checks);
    // GitHub #403: an fsqlite namespace sidecar with group/other bits blocks
    // every database open while doctor otherwise reports "healthy". This is a
    // pure stat of the database family, so it runs in `--no-db` mode too.
    check_db_sidecar_modes(&paths.db_path, &mut checks);
    // Pass-5 cycle 11: world-readable config.yaml containing secrets.
    check_config_yaml_secret_mode(beads_dir, &mut checks);
    // Pass-5 cycle 12: multiple `br` binaries on $PATH (env-based,
    // not workspace-scoped).
    check_multiple_br_in_path(&mut checks);
    // Pass-5 cycle 13: .beads/.gitignore present + expected patterns.
    check_inner_gitignore_present(beads_dir, &mut checks);
    // Pass-5 cycle 15: orphan *.tmp files from interrupted atomic writes.
    check_orphan_tmp_files(beads_dir, &mut checks);
    // Pass-5 cycle 18: .br_history/ snapshot accumulation (inode pressure).
    check_br_history_size(beads_dir, &mut checks);
    check_root_gitignore(beads_dir, &mut checks);
    check_routes_jsonl(beads_dir, &mut checks);
    check_rust_log_noisy(&mut checks);
    check_permissions_beads_dir(beads_dir, &mut checks);
    check_config_yaml(beads_dir, &mut checks);
    check_config_unknown_keys(beads_dir, &mut checks);
    check_metadata_json(beads_dir, &mut checks);
    check_binary_version_mismatch(beads_dir, &mut checks);
    check_orphaned_write_lock(beads_dir, &mut checks);
    check_routes_targets_resolve(beads_dir, &mut checks);
    check_startup_cache(beads_dir, db_override, &mut checks);
    // A committed merge is a durable saga, not a generic corruption class.
    // Surface it before any ordinary DB/JSONL consistency findings so
    // operators know that only `br sync --merge` may advance the state.
    // This check still runs in `--no-db` mode because a JSONL-only rewrite
    // could otherwise overwrite the pending merge's expected artifact.
    if live_database_inspectable {
        check_pending_sync_merge(&paths.db_path, &mut checks);
    } else {
        push_check(
            &mut checks,
            "sync.merge_pending",
            CheckStatus::Error,
            Some("Pending sync-merge state is unknown: live database inspection was skipped to preserve the namespace identity evidence".to_string()),
            Some(serde_json::json!({ "inspected": false, "blocked_by": "db.namespace_identity" })),
        );
    }

    // The JSONL-side audit always runs — it is the source of truth under the
    // `--no-db` (JSONL-only) contract.
    let (jsonl_path, jsonl_count) = inspect_doctor_jsonl(beads_dir, paths, mode, &mut checks);
    if no_db {
        // #329: under `--no-db`, skip database consistency and recovery
        // checks. Namespace admission and pending-merge evidence still run
        // above to protect the JSONL artifact from an unsafe rewrite.
        // Skip them and emit a single Ok check that enumerates exactly which
        // checks were skipped, so robot callers can see the scope of the
        // reduced run rather than silently missing findings.
        let skipped = [
            "db.recovery_dir_writable",
            "db.bloat_vs_jsonl",
            "db.wal_oversized",
            "db.inspect",
        ];
        push_check(
            &mut checks,
            "db.no_db_mode",
            CheckStatus::Ok,
            Some(format!(
                "--no-db (JSONL-only): skipped {} DB-backed check(s)",
                skipped.len()
            )),
            Some(serde_json::json!({ "skipped_checks": skipped })),
        );
    } else {
        // Pass-5 cycle 22: db-to-selected-jsonl size ratio (VACUUM candidate).
        check_db_bloat_vs_jsonl(&paths.db_path, jsonl_path.as_deref(), &mut checks);
        // Pass-5 cycle 23: selected DB WAL sidecar oversized (checkpoint candidate).
        check_wal_oversized(&paths.db_path, &mut checks);
        inspect_doctor_database(
            beads_dir,
            &paths.db_path,
            jsonl_path.as_deref(),
            jsonl_count,
            mode,
            live_database_inspectable,
            &mut checks,
        );
    }

    let classification = classify_doctor_checks(&paths.db_path, &paths.jsonl_path, &checks);
    let reliability_audit = classification.audit_record("doctor.inspect");
    let ok = match mode {
        DoctorInspectionMode::Full => !has_error(&checks),
        DoctorInspectionMode::Quick => !has_non_ok(&checks),
    };
    emit_doctor_reliability_audit("inspect", ok, &reliability_audit, &checks);

    Ok(DoctorRun {
        report: DoctorReport {
            ok,
            workspace_health: Some(classification.health.to_string()),
            reliability_audit: Some(reliability_audit),
            checks,
        },
        jsonl_path,
    })
}

fn inspect_doctor_jsonl(
    beads_dir: &Path,
    paths: &config::ConfigPaths,
    mode: DoctorInspectionMode,
    checks: &mut Vec<CheckResult>,
) -> (Option<PathBuf>, JsonlCountState) {
    let jsonl_path = select_doctor_jsonl_path(beads_dir, paths);
    // Pass-5 cycle 14: selected JSONL world-writable security check.
    check_jsonl_world_writable(jsonl_path.as_deref(), checks);
    // Pass-5 cycle 20: selected JSONL CRLF line endings.
    check_jsonl_crlf_endings(jsonl_path.as_deref(), checks);
    // Pass-5 cycle 21: UTF-8 BOM at start of selected JSONL.
    check_jsonl_utf8_bom(jsonl_path.as_deref(), checks);
    // Pass-5 cycle 19: selected JSONL trailing newline convention.
    check_jsonl_trailing_newline(jsonl_path.as_deref(), checks);
    // Pass-5 cycle 17: oversized JSONL (slow flushes, RAM pressure).
    check_jsonl_oversized(jsonl_path.as_deref(), checks);
    // Pass-5 cycle 33: scan for duplicate `id` values in the JSONL
    // (merge-artifact corruption signal).
    check_jsonl_duplicate_ids(jsonl_path.as_deref(), checks);
    let jsonl_count = if let Some(path) = jsonl_path.as_ref() {
        check_sync_jsonl_path(path, beads_dir, checks);
        check_sync_conflict_markers(path, checks);

        match check_jsonl(path, checks) {
            Ok(count) => count,
            Err(err) => {
                push_check(
                    checks,
                    "jsonl.parse",
                    CheckStatus::Error,
                    Some(format!("Failed to read JSONL: {err}")),
                    Some(serde_json::json!({ "path": path.display().to_string() })),
                );
                JsonlCountState::Unreadable
            }
        }
    } else {
        push_check(
            checks,
            "jsonl.parse",
            CheckStatus::Warn,
            Some("No JSONL file found (.beads/issues.jsonl or .beads/beads.jsonl)".to_string()),
            None,
        );
        JsonlCountState::Missing
    };

    // #350: Full-mode dependency-graph audit over the JSONL source of truth.
    // Only runs when the JSONL parsed cleanly (a malformed file is already
    // surfaced by `jsonl.parse` above and would poison the graph).
    if matches!(mode, DoctorInspectionMode::Full)
        && matches!(jsonl_count, JsonlCountState::Available(_))
        && let Some(path) = jsonl_path.as_ref()
    {
        check_dependency_graph_jsonl(path, checks);
    }

    (jsonl_path, jsonl_count)
}

/// #350 (+ #432 severity split): audit the dependency graph from the JSONL
/// source of truth and surface two findings:
///
/// - `dep.dead_closed_blocking_edges`: open issues with a blocking dependency
///   whose target is closed/tombstoned ("satisfied" — the normal end state of
///   completed work, reported `Ok` with details) or entirely absent from the
///   JSONL ("dangling" — a real defect, reported `Warn`). #432: the two are
///   opposite conditions and must never share one severity; only dangling
///   edges degrade the check, and `details` always carries both partitions
///   (`satisfied_blockers` / `dangling_blockers`) so consumers never have to
///   re-read the database to tell them apart.
/// - `dep.fully_unblocked_open`: open issues that DO declare blocking
///   dependencies but where EVERY blocker is now closed/tombstoned/absent.
///   #432: an issue whose blockers all completed is the benign steady state,
///   not a defect — it is reported `Ok` with details, split into `ready`
///   (satisfies every `br ready` condition derivable from the record) and
///   `excluded` (deliberately kept off the ready queue: claimed, deferred,
///   pinned, ephemeral/wisp, template, custom status — with the reason
///   named). The one derivable inconsistency — status still `blocked` while
///   no live blocker remains — is reported `Warn` (`stale_blocked`).
///
/// "Open" here means non-terminal (not `closed`/`tombstone`) and non-draft —
/// the same work-surface notion `br ready` uses. Blocking edge types are the
/// model's canonical blocking set (`DependencyType::is_blocking`). External
/// targets (`external:` prefix) are not treated as dead, since `br` cannot
/// know their state.
///
/// The `Ok`-with-details shape follows the established `db.sidecars` /
/// `db.no_db_mode` pattern: fully visible and machine-readable without
/// degrading a healthy workspace (`doctor.ok` treats every warn as a failed
/// health check, #292).
fn check_dependency_graph_jsonl(path: &Path, checks: &mut Vec<CheckResult>) {
    let issues = match read_jsonl_issues_for_graph(path) {
        Ok(issues) => issues,
        Err(err) => {
            // Non-fatal: jsonl.parse already owns the hard parse error. Emit a
            // Warn so the graph audit's absence is visible, not silent.
            push_check(
                checks,
                "dep.dead_closed_blocking_edges",
                CheckStatus::Warn,
                Some(format!("Could not read JSONL for dependency audit: {err}")),
                Some(serde_json::json!({ "path": path.display().to_string() })),
            );
            return;
        }
    };

    // Index status by id. Terminal = closed or tombstone (a satisfied blocker).
    let mut status_by_id: std::collections::HashMap<String, crate::model::Status> =
        std::collections::HashMap::with_capacity(issues.len());
    for issue in &issues {
        status_by_id.insert(issue.id.clone(), issue.status.clone());
    }

    // A blocker is "live" (still blocking) when it exists AND is non-terminal.
    // A blocker is "dead" when its target is terminal OR absent (and not an
    // external ref, whose state `br` cannot resolve). #432: the two dead
    // states are opposite conditions — a present-but-terminal blocker is a
    // SATISFIED dependency (the normal end of completed work), while an
    // absent one is a DANGLING edge (points at nothing) — so classify rather
    // than collapse.
    let blocker_fate = |target: &str| -> BlockerFate {
        if target.starts_with("external:") {
            return BlockerFate::Live;
        }
        match status_by_id.get(target) {
            Some(status) if status.is_terminal() => BlockerFate::Satisfied,
            Some(_) => BlockerFate::Live,
            None => BlockerFate::Dangling,
        }
    };

    let mut dead_edge_issues: Vec<serde_json::Value> = Vec::new();
    let mut dangling_edge_issue_ids: Vec<String> = Vec::new();
    let mut fully_unblocked_issues: Vec<FullyUnblockedIssue> = Vec::new();

    for issue in &issues {
        // Only audit open (non-terminal, non-draft) issues — the work surface.
        if issue.status.is_terminal() || issue.status.is_draft() {
            continue;
        }

        // Forward "blocked_by" edges only: an issue's own `depends_on_id`
        // targets it is blocked by. Restricted to the forward blocking types
        // (`blocks`/`conditional-blocks`/`waits-for`) — `parent-child` is
        // INTENTIONALLY excluded because its blocking direction is reversed
        // (the parent is blocked by the child, encoded as `issue_id=child,
        // depends_on_id=parent`), so it is not a forward "blocked_by" edge for
        // `issue`. This matches the ready-query forward-edge semantics.
        let blocking_targets: Vec<&str> = issue
            .dependencies
            .iter()
            .filter(|dep| {
                matches!(
                    dep.dep_type,
                    crate::model::DependencyType::Blocks
                        | crate::model::DependencyType::ConditionalBlocks
                        | crate::model::DependencyType::WaitsFor
                )
            })
            .map(|dep| dep.depends_on_id.as_str())
            // An issue can't meaningfully block itself; ignore self-edges.
            .filter(|target| *target != issue.id)
            .collect();

        if blocking_targets.is_empty() {
            continue;
        }

        let mut satisfied: Vec<&str> = Vec::new();
        let mut dangling: Vec<&str> = Vec::new();
        for target in &blocking_targets {
            match blocker_fate(target) {
                BlockerFate::Live => {}
                BlockerFate::Satisfied => satisfied.push(target),
                BlockerFate::Dangling => dangling.push(target),
            }
        }
        let dead_count = satisfied.len() + dangling.len();

        if dead_count > 0 {
            // `dead_blockers` is retained for consumers of the pre-#432
            // shape; `satisfied_blockers`/`dangling_blockers` carry the
            // discrimination.
            let mut dead: Vec<&str> = Vec::with_capacity(dead_count);
            dead.extend(&satisfied);
            dead.extend(&dangling);
            dead_edge_issues.push(serde_json::json!({
                "id": issue.id,
                "dead_blockers": dead,
                "satisfied_blockers": satisfied,
                "dangling_blockers": dangling,
            }));
            if !dangling.is_empty() {
                dangling_edge_issue_ids.push(issue.id.clone());
            }
        }

        // Fully unblocked: every declared blocker is dead (closed/absent), so
        // nothing live remains to block this open issue.
        if dead_count == blocking_targets.len() {
            fully_unblocked_issues.push(classify_fully_unblocked(issue));
        }
    }

    emit_dead_closed_blocking_edges(&dead_edge_issues, &dangling_edge_issue_ids, checks);
    emit_fully_unblocked_open(&fully_unblocked_issues, checks);
}

/// #432: classification of a dead blocking edge's target.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockerFate {
    /// Present and non-terminal (or an `external:` ref br cannot resolve) —
    /// still a real blocker.
    Live,
    /// Present and closed/tombstoned — the dependency was satisfied. Benign.
    Satisfied,
    /// Absent from the JSONL entirely — the edge points at nothing. A defect.
    Dangling,
}

/// #432: a fully-unblocked open issue plus its readiness classification,
/// derived purely from fields on the already-deserialized record (the same
/// conditions `br ready` applies, minus the blocked cache — which is exactly
/// what the graph audit has just recomputed as empty).
struct FullyUnblockedIssue {
    id: String,
    /// Reasons this issue is deliberately excluded from `br ready` despite
    /// having no live blockers (claimed, deferred, pinned, ...). Empty =
    /// the issue satisfies every derivable ready condition.
    excluded_reasons: Vec<&'static str>,
    /// True when status is still `blocked` although no live blocker remains —
    /// the one derivable real inconsistency in this check.
    stale_blocked: bool,
}

fn classify_fully_unblocked(issue: &crate::model::Issue) -> FullyUnblockedIssue {
    use crate::model::Status;
    let mut excluded_reasons: Vec<&'static str> = Vec::new();
    let mut stale_blocked = false;
    match &issue.status {
        // Open is ready; terminal and draft statuses were filtered out
        // before this point.
        Status::Open | Status::Closed | Status::Tombstone | Status::Draft => {}
        Status::InProgress => excluded_reasons.push("claimed (status in_progress)"),
        Status::Blocked => stale_blocked = true,
        Status::Deferred => excluded_reasons.push("status deferred"),
        Status::Pinned => excluded_reasons.push("status pinned"),
        Status::Custom(_) => excluded_reasons.push("custom status"),
    }
    if issue
        .defer_until
        .is_some_and(|until| until > chrono::Utc::now())
    {
        excluded_reasons.push("defer_until in the future");
    }
    if issue.pinned {
        excluded_reasons.push("pinned");
    }
    if issue.ephemeral {
        excluded_reasons.push("ephemeral");
    }
    if issue.id.contains("-wisp-") {
        excluded_reasons.push("wisp");
    }
    if issue.is_template {
        excluded_reasons.push("template");
    }
    FullyUnblockedIssue {
        id: issue.id.clone(),
        excluded_reasons,
        stale_blocked,
    }
}

/// Read and parse JSONL issue records for the dependency-graph audit, skipping
/// blank lines and ignoring records that fail to deserialize (those are
/// already reported by `jsonl.parse`).
fn read_jsonl_issues_for_graph(path: &Path) -> Result<Vec<crate::model::Issue>> {
    use std::io::BufRead;
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let mut issues = Vec::new();
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(issue) = serde_json::from_str::<crate::model::Issue>(trimmed) {
            issues.push(issue);
        }
    }
    Ok(issues)
}

/// #432: dangling edges (blocker absent from the JSONL) are the defect and
/// warn; satisfied edges (blocker present and closed) are the normal end
/// state of completed work and are reported `Ok` with full details. Both
/// partitions are always carried in `details.issues[]` so a consumer never
/// has to re-read the database to separate them.
fn emit_dead_closed_blocking_edges(
    dead_edge_issues: &[serde_json::Value],
    dangling_edge_issue_ids: &[String],
    checks: &mut Vec<CheckResult>,
) {
    if dead_edge_issues.is_empty() {
        push_check(
            checks,
            "dep.dead_closed_blocking_edges",
            CheckStatus::Ok,
            None,
            None,
        );
        return;
    }
    if dangling_edge_issue_ids.is_empty() {
        // Every dead edge is a satisfied dependency — benign steady state.
        // Do NOT recommend `br dep remove` here: satisfied edges are project
        // history (`br dep tree` displays them), and closing work regenerates
        // the state (#432, and the #426 bulk-delete incident).
        push_check(
            checks,
            "dep.dead_closed_blocking_edges",
            CheckStatus::Ok,
            Some(format!(
                "{} open issue(s) have blocking edges whose blockers are closed (satisfied dependencies — the normal result of completing work)",
                dead_edge_issues.len()
            )),
            Some(serde_json::json!({
                "count": dead_edge_issues.len(),
                "issues": dead_edge_issues,
                "dangling_count": 0,
                "note": "Satisfied edges record completed dependencies and need no action; removing them would delete dependency history.",
            })),
        );
        return;
    }
    push_check(
        checks,
        "dep.dead_closed_blocking_edges",
        CheckStatus::Warn,
        Some(format!(
            "{} open issue(s) have dangling blocking edges (blocker absent from JSONL): {}",
            dangling_edge_issue_ids.len(),
            dangling_edge_issue_ids.join(", ")
        )),
        Some(serde_json::json!({
            "count": dead_edge_issues.len(),
            "issues": dead_edge_issues,
            "dangling_count": dangling_edge_issue_ids.len(),
            "remediation": "Remove or update the dangling `blocks`/dependency edges (e.g. `br dep remove`) so each blocker reflects an existing issue. Edges whose blockers are merely closed (`satisfied_blockers`) are history and should be left alone.",
        })),
    );
}

/// #432: an open issue whose blockers all completed is the benign steady
/// state (`Ok` with details, split into `ready` and deliberately `excluded`);
/// the one derivable defect — status still `blocked` with no live blocker —
/// warns.
fn emit_fully_unblocked_open(
    fully_unblocked: &[FullyUnblockedIssue],
    checks: &mut Vec<CheckResult>,
) {
    if fully_unblocked.is_empty() {
        push_check(
            checks,
            "dep.fully_unblocked_open",
            CheckStatus::Ok,
            None,
            None,
        );
        return;
    }
    let ids: Vec<&str> = fully_unblocked
        .iter()
        .map(|issue| issue.id.as_str())
        .collect();
    let stale_blocked: Vec<&str> = fully_unblocked
        .iter()
        .filter(|issue| issue.stale_blocked)
        .map(|issue| issue.id.as_str())
        .collect();
    let ready: Vec<&str> = fully_unblocked
        .iter()
        .filter(|issue| !issue.stale_blocked && issue.excluded_reasons.is_empty())
        .map(|issue| issue.id.as_str())
        .collect();
    let excluded: Vec<serde_json::Value> = fully_unblocked
        .iter()
        .filter(|issue| !issue.stale_blocked && !issue.excluded_reasons.is_empty())
        .map(|issue| {
            serde_json::json!({
                "id": issue.id,
                "reasons": issue.excluded_reasons,
            })
        })
        .collect();
    let details = serde_json::json!({
        "count": fully_unblocked.len(),
        "issues": ids,
        "ready": ready,
        "excluded": excluded,
        "stale_blocked": stale_blocked,
    });
    if stale_blocked.is_empty() {
        push_check(
            checks,
            "dep.fully_unblocked_open",
            CheckStatus::Ok,
            Some(format!(
                "{} open issue(s) have all blockers completed ({} ready to work, {} deliberately excluded from ready)",
                fully_unblocked.len(),
                ready.len(),
                excluded.len()
            )),
            Some(details),
        );
        return;
    }
    let mut details = details;
    if let Some(map) = details.as_object_mut() {
        map.insert(
            "remediation".to_string(),
            serde_json::Value::String(
                "These issues have status `blocked` but no live blocker remains — update their status (e.g. `br update <id> --status open`) so they surface as ready.".to_string(),
            ),
        );
    }
    push_check(
        checks,
        "dep.fully_unblocked_open",
        CheckStatus::Warn,
        Some(format!(
            "{} open issue(s) still have status `blocked` although every blocker is closed or absent: {}",
            stale_blocked.len(),
            stale_blocked.join(", ")
        )),
        Some(details),
    );
}

/// Runtime witness for the read-only contract (GitHub #476).
///
/// On a private copy of the database family, a current-schema read-only open
/// must leave the main file, WAL, and rollback journal byte-identical and may
/// touch the WAL-index only inside the reader-mark array. The unit test that
/// pins this contract only runs in CI; this check runs it against the engine
/// and database actually installed, so an engine bump that starts writing on
/// open shows up in `br doctor` instead of in a user's corrupted workspace.
/// Warn-level because the probe runs on a copy and never on the caller's
/// database.
fn check_read_only_open_observational(db_path: &Path, checks: &mut Vec<CheckResult>) -> Result<()> {
    const NAME: &str = "db.read_only_open_observational";
    let probe = crate::storage::sqlite::probe_read_only_open_is_observational(db_path)?;
    if let Some(reason) = &probe.skipped {
        push_check(
            checks,
            NAME,
            CheckStatus::Ok,
            Some(format!("Read-only open probe skipped: {reason}")),
            Some(serde_json::json!({ "skipped": reason })),
        );
        return Ok(());
    }
    if !probe.opened {
        push_check(
            checks,
            NAME,
            CheckStatus::Ok,
            Some(
                "Read-only current-schema open is not applicable (schema is not current or a sidecar repair is pending)"
                    .to_string(),
            ),
            Some(serde_json::json!({
                "opened": false,
                "copied_bytes": probe.copied_bytes,
            })),
        );
        return Ok(());
    }
    if probe.diffs.is_empty() {
        push_check(
            checks,
            NAME,
            CheckStatus::Ok,
            None,
            Some(serde_json::json!({
                "opened": true,
                "copied_bytes": probe.copied_bytes,
            })),
        );
        return Ok(());
    }
    push_check(
        checks,
        NAME,
        CheckStatus::Warn,
        Some(format!(
            "Read-only open changed {} database-family artifact(s) outside the WAL-index reader-mark exemption",
            probe.diffs.len()
        )),
        Some(serde_json::json!({
            "opened": true,
            "copied_bytes": probe.copied_bytes,
            "diffs": probe.diffs,
        })),
    );
    Ok(())
}

fn inspect_doctor_database(
    beads_dir: &Path,
    db_path: &Path,
    jsonl_path: Option<&Path>,
    jsonl_count: JsonlCountState,
    mode: DoctorInspectionMode,
    live_database_inspectable: bool,
    checks: &mut Vec<CheckResult>,
) {
    if let Err(err) = check_recovery_artifacts(beads_dir, db_path, checks) {
        push_inspection_error(
            checks,
            "db.recovery_artifacts",
            "Failed to inspect preserved recovery artifacts",
            &err,
        );
    }
    if let Err(err) = check_recovery_artifacts_aged(beads_dir, db_path, checks) {
        push_inspection_error(
            checks,
            "db.recovery_artifacts.aged",
            "Failed to inspect aged recovery artifacts",
            &err,
        );
    }
    if let Err(err) = check_foreign_recovery_debris(beads_dir, db_path, checks) {
        push_inspection_error(
            checks,
            "db.foreign_recovery_debris",
            "Failed to inspect foreign recovery debris",
            &err,
        );
    }
    if let Err(err) = check_database_sidecars(db_path, checks) {
        push_inspection_error(
            checks,
            "db.sidecars",
            "Failed to inspect database sidecars",
            &err,
        );
    }

    if db_path.exists() {
        if !live_database_inspectable {
            push_check(
                checks,
                "db.read_only_open_observational",
                CheckStatus::Warn,
                Some(
                    "Live open probe skipped to preserve the namespace identity evidence"
                        .to_string(),
                ),
                Some(serde_json::json!({ "opened": false, "blocked_by": "db.namespace_identity" })),
            );
        } else if let Err(err) = check_read_only_open_observational(db_path, checks) {
            push_inspection_error(
                checks,
                "db.read_only_open_observational",
                "Failed to probe the read-only open contract",
                &err,
            );
        }
        inspect_existing_doctor_database(db_path, jsonl_path, jsonl_count, mode, checks);
    } else {
        push_check(
            checks,
            "db.exists",
            CheckStatus::Error,
            Some(format!("Missing database file at {}", db_path.display())),
            Some(serde_json::json!({ "path": db_path.display().to_string() })),
        );
    }
}

fn inspect_existing_doctor_database(
    db_path: &Path,
    jsonl_path: Option<&Path>,
    jsonl_count: JsonlCountState,
    mode: DoctorInspectionMode,
    checks: &mut Vec<CheckResult>,
) {
    if let Err(err) = config::with_database_family_snapshot(db_path, |snapshot_db_path| {
        let conn = Connection::open(snapshot_db_path.to_string_lossy().into_owned())?;
        let _ = conn.execute("PRAGMA busy_timeout=30000");
        if let Err(err) = required_schema_checks(&conn, checks) {
            push_inspection_error(
                checks,
                "schema.inspect",
                "Failed to inspect database schema",
                &err,
            );
        }
        if mode == DoctorInspectionMode::Full
            && let Err(err) = check_recoverable_anomalies(&conn, checks)
        {
            push_inspection_error(
                checks,
                "db.recoverable_anomalies",
                "Failed to inspect recoverable anomalies",
                &err,
            );
        }
        check_null_defaults(&conn, checks);
        check_integrity(&conn, checks);
        // Pass-4 cycle 4: detect metadata.jsonl_content_hash drift vs
        // the JSONL on disk. Uses the SNAPSHOT connection so the live
        // DB family (incl. SHM/WAL sidecars) is undisturbed.
        check_export_hash_cache_divergence(&conn, jsonl_path, checks);
        // Pass-5 cycle 7: missing-post-flush anchor (DB-aware variant of
        // the file-based base_jsonl check). Uses metadata.last_export_time
        // to distinguish fresh-clone-missing from post-flush-missing.
        // The anchor file is checked at the REAL beads_dir (db_path's
        // parent), not the snapshot dir, because the missing-on-disk
        // condition must reference the live workspace.
        let real_beads_dir = db_path.parent().unwrap_or(db_path);
        check_base_jsonl_missing_post_flush(&conn, real_beads_dir, jsonl_path, checks);
        // Pass-5 cycle 8: detect orphan rows in dirty_issues (FK
        // CASCADE bypassed). Uses the snapshot connection so live DB
        // family is undisturbed.
        check_dirty_bitmap_divergence(&conn, checks);
        // Pass-5 cycle 34: orphan rows in comments (FK off during delete).
        check_comments_orphans(&conn, checks);
        // Pass-5 cycle 35: orphan rows in labels (FK off during delete).
        check_labels_orphans(&conn, checks);
        // Pass-5 cycle 36: orphan rows in dependencies (issue_id side only;
        // depends_on_id intentionally unguarded for cross-repo refs).
        check_dependencies_orphans(&conn, checks);
        // beads_rust-m3mi: audit-suspect close_reasons (warn level)
        check_suspect_close_reasons(&conn, checks);
        // issue #311: flag issues whose status falls outside a strict
        // workflow.statuses set (no-op unless configured).
        check_workflow_statuses(&conn, real_beads_dir, checks);
        if mode == DoctorInspectionMode::Full {
            if let Err(err) = check_db_count(&conn, jsonl_count, jsonl_path, checks) {
                push_inspection_error(
                    checks,
                    "counts.db_vs_jsonl",
                    "Failed to compare database and JSONL counts",
                    &err,
                );
            }
            check_sync_metadata(&conn, snapshot_db_path, jsonl_path, checks);
            check_issue_write_probe(&conn, checks);
        }
        conn.close()?;
        Ok(())
    }) {
        push_check(
            checks,
            "db.open",
            CheckStatus::Error,
            Some(format!("Failed to open DB snapshot for inspection: {err}")),
            Some(serde_json::json!({ "path": db_path.display().to_string() })),
        );
    }
    if mode == DoctorInspectionMode::Full {
        check_sqlite_cli_integrity(db_path, checks);
    }
}

/// Execute the doctor command.
///
/// # Errors
///
/// Returns an error if report serialization fails or if IO operations fail.
///
/// # Panics
///
/// Panics if the internal invariant that the repair write authority is
/// acquired before the pending-merge gate is violated.
#[allow(clippy::too_many_lines)]
pub fn execute(args: &DoctorArgs, cli: &config::CliOverrides, ctx: &OutputContext) -> Result<()> {
    // `--selftest` never looks at the caller's workspace: it drives the
    // binary through a lifecycle in a throwaway directory and exits.
    if args.selftest {
        return crate::cli::commands::doctor_subsystems::selftest::execute(args, ctx);
    }
    // `--bundle` only reads the workspace (its captured commands re-invoke
    // this binary, which take their own locks) and writes one archive.
    if args.bundle.is_some() {
        return crate::cli::commands::doctor_subsystems::bundle::execute(args, cli, ctx);
    }
    // WP6: dispatch to the agent-ergonomics surface when a subcommand is
    // present. The flat handler below stays untouched.
    if let Some(sub) = &args.subcommand {
        let undo_write_authority = if let crate::cli::DoctorSubcommand::Undo(undo) = sub
            && !undo.dry_run
            && let Some(beads_dir) = config::discover_optional_beads_dir_with_cli(cli)?
        {
            let paths = config::resolve_paths(&beads_dir, cli.db.as_ref())?;
            let write_authority = if let Some(authority) =
                cli.database_family_write_authority_for(&beads_dir, &paths.db_path)
            {
                if let Err(err) = authority.verify_database_authority() {
                    emit_concurrency_lost(&beads_dir, &err, ctx, "doctor undo");
                    crate::shutdown::exit_process(DoctorExitCode::ConcurrencyLost.as_i32());
                }
                Arc::clone(authority)
            } else {
                match acquire_doctor_database_write_authority(
                    &beads_dir,
                    &paths.db_path,
                    cli.lock_timeout,
                ) {
                    Ok(authority) => authority,
                    Err(err) => {
                        emit_concurrency_lost(&beads_dir, &err, ctx, "doctor undo");
                        crate::shutdown::exit_process(DoctorExitCode::ConcurrencyLost.as_i32());
                    }
                }
            };
            refuse_doctor_mutation_if_merge_pending(
                "doctor undo",
                &paths.db_path,
                &write_authority,
                ctx,
            );
            Some(write_authority)
        } else {
            None
        };
        let result =
            crate::cli::commands::doctor_subsystems::surface::dispatch_subcommand(sub, cli, ctx);
        drop(undo_write_authority);
        return result;
    }
    let Some(beads_dir) = config::discover_optional_beads_dir_with_cli(cli)? else {
        let mut checks = Vec::new();
        push_check(
            &mut checks,
            "beads_dir",
            CheckStatus::Error,
            Some("Missing .beads directory (run `br init`)".to_string()),
            None,
        );
        let report = DoctorReport {
            ok: !has_error(&checks),
            workspace_health: None,
            reliability_audit: None,
            checks,
        };
        print_report(&report, ctx)?;
        // Phase 10 cold-prober finding (`beads_rust-s7nx`): missing
        // .beads/ is the canonical `no_input` (66) condition per the
        // documented exit-code dictionary. `br doctor health` already
        // returns 66 for this case; the flat `br doctor` path used to
        // exit 1, which conflicted with the per-`DoctorExitCode` contract
        // and made CI / pre-commit hooks unable to distinguish "no
        // workspace here" from "workspace has findings".
        crate::shutdown::exit_process(DoctorExitCode::NoInput.as_i32());
    };

    let paths = match config::resolve_paths(&beads_dir, cli.db.as_ref()) {
        Ok(paths) => paths,
        Err(err) => {
            let mut checks = Vec::new();
            // Pass-2 / WP5: when resolve_paths fails (typically because
            // .beads/config.yaml has a YAML parse error), run the
            // file-parse detectors that DON'T need a fully-resolved
            // ConfigPaths handle. `check_config_yaml` surfaces the
            // precise serde_yml error + the canonical "Open the file"
            // fix; without this, the operator gets the bare
            // "Failed to read metadata.json" message which routes
            // them to the WRONG file (config.yaml is broken, not
            // metadata.json).
            check_config_yaml(&beads_dir, &mut checks);
            check_metadata_json(&beads_dir, &mut checks);
            push_check(
                &mut checks,
                "metadata",
                CheckStatus::Error,
                Some(format!("Failed to read metadata.json: {err}")),
                None,
            );
            let report = DoctorReport {
                ok: !has_error(&checks),
                workspace_health: None,
                reliability_audit: None,
                checks,
            };
            print_report(&report, ctx)?;
            crate::shutdown::exit_process(1);
        }
    };
    set_engine_block(engine_block(&beads_dir, &paths.db_path));

    // Round-3 fresh-eyes finding (`beads_rust-sexc`): every other mutating
    // subcommand (`update`, `delete`, `close`, `dep`, `label`, …) routes its
    // writes through `acquire_routed_workspace_write_lock` so concurrent
    // processes cannot tear the on-disk DB family. `--repair` performs the
    // most invasive mutations of all (VACUUM, REINDEX, JSONL rebuild,
    // chokepointed file/DB ops), but its execute() previously had no
    // explicit guard — it relied entirely on `main.rs`'s startup write lock
    // (see `needs_write_lock()` matching `Commands::Doctor(_)`). That works
    // when `br doctor` is invoked through the binary, but it offers no
    // defense-in-depth for callers that exercise `commands::doctor::execute`
    // directly (e.g., embedded use, future test harnesses) and the failure
    // mode under contention is opaque (generic `BeadsError::Config` →
    // exit code 1) instead of the structured `ConcurrencyLost` (exit 5)
    // documented in `doctor_subsystems::exit_codes`.
    //
    // Wire the lock here, BEFORE live report collection, run-dir creation,
    // fixer dispatch, or mutate() call:
    //   * If `main.rs` already holds the workspace write lock for this
    //     beads_dir (the common case under the binary), reuse it — flock()
    //     is per-open-file-description on Linux and re-locking from a
    //     fresh handle in the same process would deadlock against our
    //     own startup guard.
    //   * Otherwise, acquire the lock ourselves with the configured
    //     `--lock-timeout`. On contention, exit with structured exit
    //     code 5 (`ConcurrencyLost`) so agent scripts and CI can
    //     reliably distinguish "another process held the lock" from
    //     other failure modes.
    //
    // The guard binds to a let in `execute()` so it lives until function
    // return — RAII drop releases the lock on success, panic, and every
    // early-return below. Releasing the lock implicitly on panic is
    // load-bearing: a panicking fixer must not strand the `.write.lock`
    // file held against subsequent runs.
    let repair_write_authority: Option<Arc<crate::sync::DatabaseFamilyWriteLock>> = if args.repair
        && !args.robot_triage
    {
        if let Some(authority) = cli.database_family_write_authority_for(&beads_dir, &paths.db_path)
        {
            // main.rs already serialized us against concurrent writers.
            // Reuse that exact inode-bound capability; a fresh flock
            // handle in the same process would deadlock on Linux.
            if let Err(err) = authority.verify_database_authority() {
                emit_concurrency_lost(&beads_dir, &err, ctx, "--repair");
                crate::shutdown::exit_process(DoctorExitCode::ConcurrencyLost.as_i32());
            }
            Some(Arc::clone(authority))
        } else {
            // Phase 10 cold-prober finding (`beads_rust-mbpq` P0):
            // the documented contract is "try-lock or refuse with
            // exit 5 (concurrency_lost)" — NOT "block up to
            // --lock-timeout then refuse". Concurrency between two
            // `br doctor --repair` invocations is an unrecoverable
            // operator-level condition: blocking for 30s and then
            // erroring serves nobody. Operators who genuinely want
            // to wait can pass `--lock-timeout 30000` explicitly;
            // the default doctor path tries once and refuses fast.
            let timeout_ms = cli.lock_timeout.unwrap_or(0);
            match crate::sync::blocking_database_family_write_lock_with_timeout(
                &beads_dir,
                &paths.db_path,
                Some(timeout_ms),
            ) {
                Ok(authority) => {
                    let authority = Arc::new(authority);
                    if let Err(err) = authority
                        .bind_database_inode_for_mutation()
                        .and_then(|_| authority.verify_database_authority())
                    {
                        emit_concurrency_lost(&beads_dir, &err, ctx, "--repair");
                        crate::shutdown::exit_process(DoctorExitCode::ConcurrencyLost.as_i32());
                    }
                    Some(authority)
                }
                Err(err) => {
                    emit_concurrency_lost(&beads_dir, &err, ctx, "--repair");
                    crate::shutdown::exit_process(DoctorExitCode::ConcurrencyLost.as_i32());
                }
            }
        }
    } else {
        None
    };

    // Round-5 fresh-eyes follow-through (`beads_rust-73ux`): the WP1
    // refuse-unsafe gates (schema-version-downgrade,
    // recovery-fingerprint-integrity) must run BEFORE any
    // run-dir creation, fixer dispatch, or `mutate()` call — they are
    // *precondition* checks. We run them AFTER the workspace write lock
    // is held so a concurrent writer cannot mutate the DB header (and
    // thus the gate's verdict) between our gate read and the chokepoint
    // execution. Pure-read; never mutates.
    //
    // On Refuse, exit 4 (`RefusedUnsafe`) with a structured envelope.
    // The lock guard's RAII drop releases `.write.lock` on this exit.
    if args.repair && !args.robot_triage {
        match refuse_gates::run_all(&beads_dir, &paths.db_path) {
            GateOutcome::Allow => {}
            GateOutcome::Refuse {
                code: _,
                reason,
                evidence,
            } => {
                emit_refused_unsafe("--repair", &reason, &evidence, ctx);
                crate::shutdown::exit_process(DoctorExitCode::RefusedUnsafe.as_i32());
            }
        }
    }

    if args.repair && !args.dry_run && !args.robot_triage {
        let write_authority = repair_write_authority
            .as_ref()
            .expect("repair write authority acquired before pending-merge gate");
        refuse_doctor_mutation_if_merge_pending("--repair", &paths.db_path, write_authority, ctx);
    }

    // --repair-indexes (#288): REINDEX-only recovery path, strictly
    // narrower than --repair. It must run before live report collection
    // because report collection opens the DB; refuse-unsafe gates need
    // to inspect the pre-open on-disk state.
    if args.repair_indexes && !args.robot_triage {
        return execute_repair_indexes(&beads_dir, &paths, ctx, args, cli);
    }

    let inspection_mode = if args.quick && !args.repair && !args.robot_triage {
        DoctorInspectionMode::Quick
    } else {
        DoctorInspectionMode::Full
    };
    // #329: resolve `--no-db` (JSONL-only) the same way the storage layer does
    // so DB-backed checks are skipped and reported via `db.no_db_mode`.
    let no_db = resolve_doctor_no_db(&beads_dir, cli);
    let mut initial = collect_doctor_report_with_mode_and_db_override(
        &beads_dir,
        &paths,
        cli.db.as_ref(),
        inspection_mode,
        no_db,
    )?;

    // WP6: --robot-triage short-circuits the flat run with a single
    // `br.doctor.triage.v1` envelope. Read-only; no further dispatch.
    if args.robot_triage {
        emit_flat_robot_triage(&initial.report);
        return Ok(());
    }

    // Build the per-run session once when --repair is requested. Every repaired
    // write must have a run-dir, verbatim backup, and actions.jsonl trail. If
    // the run-dir cannot be created, fail closed before any fixer can fall back
    // to legacy in-place writes or accidentally ignore --dry-run.
    let mut session: Option<DoctorRepairSession> = if args.repair {
        let repo_root = beads_dir.parent().unwrap_or(&beads_dir);
        Some(DoctorRepairSession::new(repo_root, args.dry_run).map_err(|err| {
            BeadsError::Config(format!(
                "Cannot run doctor --repair without a reversible repair session: failed to create doctor run directory ({err}). No repair writes were applied. Set {} to a writable directory if the workspace is read-only.",
                run_dir::ENV_RUNS_DIR
            ))
        })?)
    } else {
        None
    };

    // Pass-5 cycle 1: per-FM filter built from --only/--skip flags.
    // The 5 chokepointed fixers below consult the filter; legacy
    // repair_* paths run unconditionally for now.
    let fixer_filter = FixerFilter::from_args(&args.only, &args.skip);

    // Auto-fix root .gitignore if --repair is passed and the warning is present.
    let gitignore_repaired =
        if args.repair && fixer_filter.allows("fm-configs-gitignore-leaking-beads") {
            let repaired =
                fix_root_gitignore_if_warned(&beads_dir, &initial.report, ctx, session.as_mut());
            if repaired {
                initial = collect_doctor_report_for_cli(&beads_dir, &paths, cli)?;
            }
            repaired
        } else {
            false
        };

    // Pass-4 cycle 1: auto-quarantine stuck merge artifacts under --repair.
    // Per AGENTS.md RULE 1 (no-delete) and the repair-spec for
    // fm-state_files-merge-artifact-stuck, the fixer moves the artifacts
    // into <run-dir>/quarantine/.beads/ via the chokepoint so
    // `doctor undo` can byte-reverse the move.
    let merge_artifacts_repaired =
        if args.repair && fixer_filter.allows("fm-state_files-merge-artifact-stuck") {
            let repaired =
                fix_merge_artifacts_if_warned(&beads_dir, &initial.report, ctx, session.as_mut());
            if repaired {
                initial = collect_doctor_report_for_cli(&beads_dir, &paths, cli)?;
            }
            repaired
        } else {
            false
        };

    // Pass-4 cycle 2: auto-quarantine poisoned startup-cache files. The
    // cache lives outside the default workspace, so the fixer extends
    // the session's write_scopes to include the resolved cache dir.
    let startup_cache_repaired =
        if args.repair && fixer_filter.allows("fm-configs-startup-cache-poisoned") {
            let repaired = fix_startup_cache_if_warned(
                &beads_dir,
                cli.db.as_ref(),
                &initial.report,
                ctx,
                session.as_mut(),
            );
            if repaired {
                initial = collect_doctor_report_for_cli(&beads_dir, &paths, cli)?;
            }
            repaired
        } else {
            false
        };

    // Pass-4 cycle 3: quarantine past-TTL recovery artifacts. The fixer
    // PRESERVES recent recovery backups (operators commonly need them
    // for forensic value) and only moves artifacts older than
    // RECOVERY_AGED_TTL_DAYS into the run-dir quarantine.
    let recovery_aged_repaired =
        if args.repair && fixer_filter.allows("fm-state_files-recovery-artifacts-orphaned") {
            let repaired = fix_recovery_artifacts_aged_if_warned(
                &beads_dir,
                &paths.db_path,
                &initial.report,
                ctx,
                session.as_mut(),
            );
            if repaired {
                initial = collect_doctor_report_for_cli(&beads_dir, &paths, cli)?;
            }
            repaired
        } else {
            false
        };

    // Pass-4 cycle 4: recompute metadata.jsonl_content_hash from the
    // authoritative JSONL on disk if the cached value has drifted.
    // JSONL is NEVER mutated; only the cache row updates.
    let export_hash_repaired =
        if args.repair && fixer_filter.allows("fm-caches_indexes-export-hash-cache-divergence") {
            let repaired = fix_export_hash_cache_divergence_if_warned(
                &paths.db_path,
                initial.jsonl_path.as_deref(),
                &initial.report,
                ctx,
                session.as_mut(),
            );
            if repaired {
                initial = collect_doctor_report_for_cli(&beads_dir, &paths, cli)?;
            }
            repaired
        } else {
            false
        };

    // Pass-5 cycle 5: quarantine symlinked merge anchor under --repair.
    // Only the SYMLINK subset of fm-state_files-base-jsonl-missing-or-stale
    // is auto-fixed; the stale-anchor subset is regenerated from the
    // live JSONL bytes by cycle 6's fixer below.
    let base_jsonl_symlink_repaired = if args.repair
        && fixer_filter.allows("fm-state_files-base-jsonl-missing-or-stale")
    {
        let repaired =
            fix_base_jsonl_symlink_if_warned(&beads_dir, &initial.report, ctx, session.as_mut());
        if repaired {
            initial = collect_doctor_report_for_cli(&beads_dir, &paths, cli)?;
        }
        repaired
    } else {
        false
    };

    // Pass-5 cycle 16: quarantine orphan *.tmp files under .beads/
    // via Op::Rename (same pattern as cycle 1's merge-artifact-stuck).
    let orphan_tmp_repaired =
        if args.repair && fixer_filter.allows("fm-state_files-orphan-tmp-files") {
            let repaired =
                fix_orphan_tmp_files_if_warned(&beads_dir, &initial.report, ctx, session.as_mut());
            if repaired {
                initial = collect_doctor_report_for_cli(&beads_dir, &paths, cli)?;
            }
            repaired
        } else {
            false
        };

    // Pass-5 cycle 19: append trailing newline to the selected JSONL via Op::AppendFile.
    let jsonl_eof_newline_repaired =
        if args.repair && fixer_filter.allows("fm-state_files-jsonl-missing-trailing-newline") {
            let repaired = fix_jsonl_trailing_newline_if_warned(
                initial.jsonl_path.as_deref(),
                &initial.report,
                ctx,
                session.as_mut(),
            );
            if repaired {
                initial = collect_doctor_report_for_cli(&beads_dir, &paths, cli)?;
            }
            repaired
        } else {
            false
        };
    let _ = jsonl_eof_newline_repaired;

    // Pass-5 cycle 21: strip UTF-8 BOM from the selected JSONL via Op::WriteFile.
    let jsonl_bom_repaired =
        if args.repair && fixer_filter.allows("fm-state_files-jsonl-utf8-bom-prefix") {
            let repaired = fix_jsonl_utf8_bom_if_warned(
                initial.jsonl_path.as_deref(),
                &initial.report,
                ctx,
                session.as_mut(),
            );
            if repaired {
                initial = collect_doctor_report_for_cli(&beads_dir, &paths, cli)?;
            }
            repaired
        } else {
            false
        };
    let _ = jsonl_bom_repaired;

    // Pass-5 cycle 24: convert CRLF→LF in the selected JSONL via Op::WriteFile.
    let jsonl_crlf_repaired =
        if args.repair && fixer_filter.allows("fm-state_files-jsonl-crlf-line-endings") {
            let repaired = fix_jsonl_crlf_endings_if_warned(
                initial.jsonl_path.as_deref(),
                &initial.report,
                ctx,
                session.as_mut(),
            );
            if repaired {
                initial = collect_doctor_report_for_cli(&beads_dir, &paths, cli)?;
            }
            repaired
        } else {
            false
        };
    let _ = jsonl_crlf_repaired;

    // Pass-5 cycle 6: regenerate the derived merge anchor only after
    // every content-normalizing JSONL fixer has run. If the live JSONL
    // contains a BOM, CRLF endings, or lacks its final newline, writing
    // the anchor first captures the malformed bytes; the normalization
    // then immediately makes that new anchor stale and forces a second
    // repair pass. Source-before-derived ordering keeps the repair
    // idempotent and ensures undo records only one anchor mutation.
    let base_jsonl_stale_repaired =
        if args.repair && fixer_filter.allows("fm-state_files-base-jsonl-missing-or-stale") {
            let repaired =
                fix_base_jsonl_stale_if_warned(&beads_dir, &initial.report, ctx, session.as_mut());
            if repaired {
                initial = collect_doctor_report_for_cli(&beads_dir, &paths, cli)?;
            }
            repaired
        } else {
            false
        };

    // Pass-5 cycle 25: strip world-write bit from issues.jsonl via Op::Chmod.
    let jsonl_world_writable_repaired =
        if args.repair && fixer_filter.allows("fm-permissions-jsonl-world-writable") {
            let repaired = fix_jsonl_world_writable_if_warned(
                initial.jsonl_path.as_deref(),
                &initial.report,
                ctx,
                session.as_mut(),
            );
            if repaired {
                initial = collect_doctor_report_for_cli(&beads_dir, &paths, cli)?;
            }
            repaired
        } else {
            false
        };
    let _ = jsonl_world_writable_repaired;

    // Pass-5 cycle 26: strip world-read/write bits from .beads/config.yaml
    // when it contains secrets, via Op::Chmod.
    let config_yaml_secret_mode_repaired =
        if args.repair && fixer_filter.allows("fm-permissions-config-yaml-mode-leaks-secrets") {
            let repaired = fix_config_yaml_secret_mode_if_warned(
                &beads_dir,
                &initial.report,
                ctx,
                session.as_mut(),
            );
            if repaired {
                initial = collect_doctor_report_for_cli(&beads_dir, &paths, cli)?;
            }
            repaired
        } else {
            false
        };
    let _ = config_yaml_secret_mode_repaired;

    // GitHub #403: restore owner-only mode on fsqlite namespace sidecars that
    // would otherwise wedge every database open, via Op::Chmod.
    let db_sidecar_mode_repaired = if args.repair
        && fixer_filter.allows("fm-permissions-db-sidecar-mode-too-open")
    {
        let repaired =
            fix_db_sidecar_modes_if_warned(&paths.db_path, &initial.report, ctx, session.as_mut());
        if repaired {
            initial = collect_doctor_report_for_cli(&beads_dir, &paths, cli)?;
        }
        repaired
    } else {
        false
    };
    let _ = db_sidecar_mode_repaired;

    // Pass-5 cycle 27: append missing canonical patterns to .beads/.gitignore
    // via Op::AppendFile.
    let inner_gitignore_repaired =
        if args.repair && fixer_filter.allows("fm-configs-gitignore-leaking-beads") {
            let repaired =
                fix_inner_gitignore_if_warned(&beads_dir, &initial.report, ctx, session.as_mut());
            if repaired {
                initial = collect_doctor_report_for_cli(&beads_dir, &paths, cli)?;
            }
            repaired
        } else {
            false
        };
    let _ = inner_gitignore_repaired;

    // Pass-5 cycle 29: surgically prune orphan dirty_issues rows via Op::DbExec.
    let dirty_bitmap_orphans_repaired =
        if args.repair && fixer_filter.allows("fm-caches_indexes-dirty-bitmap-divergence") {
            let repaired = fix_dirty_bitmap_orphans_if_warned(
                &paths.db_path,
                &initial.report,
                ctx,
                session.as_mut(),
            );
            if repaired {
                initial = collect_doctor_report_for_cli(&beads_dir, &paths, cli)?;
            }
            repaired
        } else {
            false
        };
    let _ = dirty_bitmap_orphans_repaired;

    // Pass-5 cycle 34: surgically prune orphan comments rows via Op::DbExec.
    let comments_orphans_repaired = if args.repair
        && fixer_filter.allows("fm-caches_indexes-comments-orphans")
    {
        let repaired =
            fix_comments_orphans_if_warned(&paths.db_path, &initial.report, ctx, session.as_mut());
        if repaired {
            initial = collect_doctor_report_for_cli(&beads_dir, &paths, cli)?;
        }
        repaired
    } else {
        false
    };
    let _ = comments_orphans_repaired;

    // Pass-5 cycle 35: surgically prune orphan labels rows via Op::DbExec.
    let labels_orphans_repaired = if args.repair
        && fixer_filter.allows("fm-caches_indexes-labels-orphans")
    {
        let repaired =
            fix_labels_orphans_if_warned(&paths.db_path, &initial.report, ctx, session.as_mut());
        if repaired {
            initial = collect_doctor_report_for_cli(&beads_dir, &paths, cli)?;
        }
        repaired
    } else {
        false
    };
    let _ = labels_orphans_repaired;

    // Pass-5 cycle 36: surgically prune orphan dependencies rows via Op::DbExec.
    let dependencies_orphans_repaired =
        if args.repair && fixer_filter.allows("fm-caches_indexes-dependencies-orphans") {
            let repaired = fix_dependencies_orphans_if_warned(
                &paths.db_path,
                &initial.report,
                ctx,
                session.as_mut(),
            );
            if repaired {
                initial = collect_doctor_report_for_cli(&beads_dir, &paths, cli)?;
            }
            repaired
        } else {
            false
        };
    let _ = dependencies_orphans_repaired;

    // Pass-5 cycle 37: PRAGMA wal_checkpoint(TRUNCATE) when the WAL is oversized.
    let wal_checkpoint_repaired =
        if args.repair && fixer_filter.allows("fm-state_files-wal-oversized") {
            let write_authority =
                repair_write_authority
                    .as_ref()
                    .ok_or_else(|| BeadsError::SyncConflict {
                        message: "WAL repair reached mutation without database-family authority"
                            .to_string(),
                    })?;
            let repaired = fix_wal_oversized_if_warned_under_write_authority(
                &paths.db_path,
                &initial.report,
                ctx,
                session.as_mut(),
                write_authority,
            );
            if repaired {
                initial = collect_doctor_report_for_cli(&beads_dir, &paths, cli)?;
            }
            repaired
        } else {
            false
        };
    let _ = wal_checkpoint_repaired;

    // Pass-8 cycle 61: backfill schema-declared defaults into NULL NOT-NULL
    // columns (legacy rows predating a `DEFAULT` addition) via Op::DbExec.
    let null_defaults_repaired =
        if args.repair && fixer_filter.allows("fm-schemas-missing-required-column") {
            let repaired =
                fix_null_defaults_if_warned(&paths.db_path, &initial.report, ctx, session.as_mut());
            if repaired {
                initial = collect_doctor_report_for_cli(&beads_dir, &paths, cli)?;
            }
            repaired
        } else {
            false
        };
    let _ = null_defaults_repaired;

    // Pass-10 cycle 62: `--unsafe-auto-fix`-gated VACUUM on db bloat.
    // VACUUM is normally detect-only (expensive, operator-timed) but
    // data-equivalent + reversible via the legacy chokepoint snapshot,
    // so an explicit opt-in is safe.
    let db_bloat_vacuum_repaired = if args.repair
        && args.unsafe_auto_fix
        && fixer_filter.allows("fm-caches_indexes-db-bloat-vs-jsonl")
    {
        let write_authority =
            repair_write_authority
                .as_ref()
                .ok_or_else(|| BeadsError::SyncConflict {
                    message: "VACUUM repair reached mutation without database-family authority"
                        .to_string(),
                })?;
        let repaired = fix_db_bloat_via_vacuum_if_warned_under_write_authority(
            &paths.db_path,
            &initial.report,
            ctx,
            session.as_mut(),
            write_authority,
        );
        if repaired {
            initial = collect_doctor_report_for_cli(&beads_dir, &paths, cli)?;
        }
        repaired
    } else {
        false
    };
    let _ = db_bloat_vacuum_repaired;

    // Replay-idempotence reconciliation: the export-hash-cache fixer
    // above runs BEFORE the JSONL-mutating fixers (BOM strip, CRLF,
    // trailing-newline, world-writable mode-strip). If any of those
    // rewrote the JSONL bytes, the cached `metadata.jsonl_content_hash`
    // is now stale even though the cache fixer's first pass reported
    // success. Re-run the cache fixer at the end so the workspace is
    // genuinely quiescent — the second `--repair` invocation under
    // REPLAY_IDEMPOTENCE=1 finds nothing to do, and the chokepoint
    // emits zero actions on the no-op replay run.
    //
    // The first-pass `export_hash_repaired` flag retains its semantics
    // (it tracks whether the FIRST cache mismatch was repaired); this
    // tail call is reported under the same fixer-id and recorded in
    // actions.jsonl exactly like the first pass.
    let jsonl_was_mutated = jsonl_bom_repaired
        || jsonl_crlf_repaired
        || jsonl_eof_newline_repaired
        || jsonl_world_writable_repaired;
    let export_hash_reconciled = if args.repair
        && jsonl_was_mutated
        && fixer_filter.allows("fm-caches_indexes-export-hash-cache-divergence")
    {
        let reconciled = fix_export_hash_cache_divergence_if_warned(
            &paths.db_path,
            initial.jsonl_path.as_deref(),
            &initial.report,
            ctx,
            session.as_mut(),
        );
        if reconciled {
            initial = collect_doctor_report_for_cli(&beads_dir, &paths, cli)?;
        }
        reconciled
    } else {
        false
    };
    let _ = export_hash_reconciled;

    if args.repair && (fixer_filter.has_only() || fixer_filter.has_skip()) {
        tracing::info!(
            target: "br::doctor::filter",
            only = ?args.only,
            skip = ?args.skip,
            "doctor --repair filter active"
        );
    }
    let early_repair = EarlyRepairSummary {
        gitignore: gitignore_repaired,
        merge_artifacts: merge_artifacts_repaired,
        startup_cache: startup_cache_repaired,
        recovery_aged: recovery_aged_repaired,
        export_hash: export_hash_repaired,
        base_jsonl_symlink: base_jsonl_symlink_repaired,
        base_jsonl_stale: base_jsonl_stale_repaired,
        orphan_tmp: orphan_tmp_repaired,
        jsonl_eof_newline: jsonl_eof_newline_repaired,
        jsonl_bom: jsonl_bom_repaired,
        jsonl_crlf: jsonl_crlf_repaired,
        jsonl_world_writable: jsonl_world_writable_repaired,
        config_yaml_secret_mode: config_yaml_secret_mode_repaired,
        inner_gitignore: inner_gitignore_repaired,
        dirty_bitmap_orphans: dirty_bitmap_orphans_repaired,
        comments_orphans: comments_orphans_repaired,
        labels_orphans: labels_orphans_repaired,
        dependencies_orphans: dependencies_orphans_repaired,
        wal_checkpoint: wal_checkpoint_repaired,
        null_defaults: null_defaults_repaired,
        db_bloat_vacuum: db_bloat_vacuum_repaired,
    };

    if !args.repair {
        if args.quick {
            // Phase 8: drop the slow detectors so pre-commit / CI can
            // gate on a sub-second `br doctor --quick --json`. The
            // dropped detectors are: db.recoverable_anomalies,
            // counts.db_vs_jsonl, sync.metadata, sqlite3.integrity_check,
            // db.write_probe. The remaining checks are all O(file
            // existence) or single PRAGMA so they stay cheap.
            initial
                .report
                .checks
                .retain(|c| !is_quick_suppressed_doctor_check(&c.name));
        }
        // Per `DoctorExitCode` (and the surface documentation in
        // `doctor_subsystems/surface.rs`), `br doctor` without `--repair`
        // should exit `FindingsPresent` (1) whenever any check is not OK
        // — both WARN and Error. Quick mode already recomputed `ok` this
        // way after filtering; the full-mode path used to leave `ok`
        // pinned to `!has_error(&checks)` (set by report construction)
        // which left WARN findings shaped as `ok: true` in JSON while
        // we wanted them to fail. Recompute unconditionally so the JSON
        // `ok` field and the exit code agree on the same predicate
        // (`!has_non_ok`). See #292.
        initial.report.ok = !has_non_ok(&initial.report.checks);
        print_report(&initial.report, ctx)?;
        if !initial.report.ok {
            crate::shutdown::exit_process(DoctorExitCode::FindingsPresent.as_i32());
        }
        return Ok(());
    }

    let repair_write_authority =
        repair_write_authority
            .as_ref()
            .ok_or_else(|| {
                BeadsError::SyncConflict {
            message:
                "doctor --repair reached mutation planning without database-family write authority"
                    .to_string(),
        }
            })?;
    let mut local_repair = LocalRepairResult::default();

    if initial.report.ok {
        // Pass-5 cycle 2: legacy repair_* paths now consult the
        // FixerFilter. AND-ing the predicate against `filter.allows(...)`
        // makes downstream `if has_X` branches treat a filter-excluded
        // FM as "no finding to repair", which keeps the
        // local_repair_audit_record's `verified` invariant intact.
        let has_blocked_cache_rebuild = report_has_blocked_cache_rebuild_finding(&initial.report)
            && fixer_filter.allows(FM_BLOCKED_CACHE_STALE);
        let has_partial_index_warnings = report_has_partial_index_warnings(&initial.report)
            && fixer_filter.allows(FM_PARTIAL_INDEX_STALE);
        let has_warn_page_anomalies = report_has_warn_level_page_anomaly(&initial.report)
            && fixer_filter.allows(FM_SQLITE_PAGE_MALFORMED);

        // Even when there are no errors, planned deferred cache rebuilds and
        // integrity warnings can be repaired. Run those local repairs when
        // --repair is passed and the warnings are present.
        if has_blocked_cache_rebuild || has_partial_index_warnings || has_warn_page_anomalies {
            local_repair = if has_blocked_cache_rebuild {
                repair_recoverable_db_state_under_write_authority(
                    &beads_dir,
                    &paths.db_path,
                    &initial.report,
                    session.as_mut(),
                    &fixer_filter,
                    repair_write_authority,
                )
            } else {
                LocalRepairResult::default()
            };

            if has_partial_index_warnings {
                repair_partial_indexes_under_write_authority(
                    &paths.db_path,
                    &mut local_repair,
                    session.as_mut(),
                    repair_write_authority,
                );
            }

            if has_warn_page_anomalies {
                repair_via_vacuum(
                    &paths.db_path,
                    &mut local_repair,
                    session.as_mut(),
                    repair_write_authority,
                );
            }

            let post_warning_repair = collect_doctor_report_for_cli(&beads_dir, &paths, cli)?;
            let verified = warning_repair_verified(
                &post_warning_repair.report,
                has_blocked_cache_rebuild,
                has_partial_index_warnings,
            );
            let repair_message = repair_outcome_message_from_parts(
                early_repair.messages(),
                Some(&local_repair),
                has_partial_index_warnings.then_some(REINDEX_INCOMPLETE_MESSAGE),
            );
            let recovery_audit = early_repair.prepend_actions_to_audit(local_repair_audit_record(
                "doctor.warn_repair",
                if verified {
                    "verified"
                } else if has_warn_page_anomalies {
                    "needs_jsonl_rebuild"
                } else {
                    "verification_failed"
                },
                &local_repair,
                (!verified).then(|| {
                    if has_warn_page_anomalies {
                        "local warning repair did not clear page-level integrity warnings"
                            .to_string()
                    } else {
                        "local warning repair did not clear all requested warnings".to_string()
                    }
                }),
            ));
            emit_recovery_audit_record(&recovery_audit);
            if verified {
                if ctx.is_json() {
                    ctx.json(&serde_json::json!({
                        "report": initial.report,
                        "repaired": early_repair.applied() || local_repair.applied(),
                        "local_repair": local_repair,
                        "recovery_audit": recovery_audit,
                        "message": repair_message,
                        "post_repair": post_warning_repair.report,
                        "verified": true,
                    }));
                } else {
                    print_report(&initial.report, ctx)?;
                    ctx.info(&repair_message);
                    ctx.info("Post-repair verification:");
                    print_report(&post_warning_repair.report, ctx)?;
                }
                return Ok(());
            }

            if !ctx.is_json() {
                ctx.info(&repair_message);
                ctx.info(
                    "Local warning repair did not clear all integrity warnings; rebuilding DB from JSONL...",
                );
            }
        } else {
            // Early-only repair path: when the initial report is OK and no
            // local-repair-eligible anomalies are present, only the early
            // fixers (gitignore, merge artifacts, inner_gitignore_append,
            // orphan rows, WAL checkpoint, null defaults, etc.) may have
            // fired. Each early fixer re-collects the report on success
            // (`initial = collect_doctor_report_for_cli(...)` after every
            // fix), so `initial.report` here is already the post-repair
            // state — emit it as `post_repair` and surface the
            // `verified` invariant the test contract / repair surface
            // expects from any RepairApplied outcome (see
            // assert_repair_applied_surface in tests/workspace_failure_replay.rs).
            let verified = repair_report_verified(&initial.report);
            let recovery_audit = early_repair.audit_record();
            emit_recovery_audit_record(&recovery_audit);
            if ctx.is_json() {
                ctx.json(&serde_json::json!({
                    "report": initial.report,
                    "repaired": early_repair.applied(),
                    "recovery_audit": recovery_audit,
                    "message": repair_outcome_message_from_parts(early_repair.messages(), None, None),
                    "post_repair": initial.report,
                    "verified": verified,
                }));
            } else {
                print_report(&initial.report, ctx)?;
                ctx.info(&repair_outcome_message_from_parts(
                    early_repair.messages(),
                    None,
                    None,
                ));
            }
            return Ok(());
        }
    }

    // Pass-5 cycle 2: gate the post-failure fallback repair_* paths on
    // the FixerFilter. AND the predicate against `filter.allows(...)`
    // so the audit chain naturally treats filter-excluded FMs as
    // "no finding to act on".
    let has_blocked_cache_rebuild = report_has_blocked_cache_rebuild_finding(&initial.report);
    let has_sidecar_anomaly = report_has_sidecar_anomaly(&initial.report);
    if !local_repair.applied()
        && filter_allows_recoverable_db_state_repair(
            &fixer_filter,
            has_blocked_cache_rebuild,
            has_sidecar_anomaly,
        )
    {
        local_repair = repair_recoverable_db_state_under_write_authority(
            &beads_dir,
            &paths.db_path,
            &initial.report,
            session.as_mut(),
            &fixer_filter,
            repair_write_authority,
        );
    }

    // Also attempt REINDEX if partial-index warnings are present alongside errors.
    if !local_repair.indexes_reindexed
        && fixer_filter.allows(FM_PARTIAL_INDEX_STALE)
        && report_has_partial_index_warnings(&initial.report)
    {
        repair_partial_indexes_under_write_authority(
            &paths.db_path,
            &mut local_repair,
            session.as_mut(),
            repair_write_authority,
        );
    }

    // VACUUM to fix page-level anomalies (free space corruption, malformed
    // B-tree pages) caused by frankensqlite's B-tree layer differences with
    // C sqlite3 (#237, #245).  VACUUM rewrites every page from scratch, so
    // it fixes both index and table corruption.
    if fixer_filter.allows(FM_SQLITE_PAGE_MALFORMED) && report_has_page_corruption(&initial.report)
    {
        repair_via_vacuum(
            &paths.db_path,
            &mut local_repair,
            session.as_mut(),
            repair_write_authority,
        );
    }

    let mut after_local_repair = if local_repair.applied() {
        collect_doctor_report_for_cli(&beads_dir, &paths, cli)?
    } else {
        initial.clone()
    };

    // #253: the light-repair passes above (notably `reset_blocked_cache_table`
    // via `repair_recoverable_db_state`) can leave orphaned pages behind that
    // surface as WARN-level `page N: never used` integrity findings. These
    // don't flip the DB into ERROR status, so the pre-repair
    // `report_has_page_corruption` gate misses them — but they're still
    // page-level residue that VACUUM resolves cleanly. Run VACUUM once to
    // compact, and re-collect the report.  Guarded by `!local_repair.vacuumed`
    // so we never loop.
    if !local_repair.vacuumed
        && fixer_filter.allows(FM_SQLITE_PAGE_MALFORMED)
        && report_has_warn_level_page_anomaly(&after_local_repair.report)
    {
        tracing::info!(
            path = %paths.db_path.display(),
            "Post-repair report has WARN-level page anomalies; running VACUUM to clean up orphaned pages"
        );
        repair_via_vacuum(
            &paths.db_path,
            &mut local_repair,
            session.as_mut(),
            repair_write_authority,
        );
        if local_repair.vacuumed {
            after_local_repair = collect_doctor_report_for_cli(&beads_dir, &paths, cli)?;
        }
    }

    if repair_report_verified(&after_local_repair.report) {
        // Issue #245: REINDEX can fix index ordering so integrity_check
        // passes, but the underlying B-tree corruption may still cause
        // writes to fail (reads work, writes get ISSUE_NOT_FOUND).  Run a
        // write probe to confirm that the DB is truly healthy before
        // declaring success.
        let write_probe_ok = write_probe_after_repair(&paths.db_path, repair_write_authority);
        if !write_probe_ok {
            let recovery_audit = early_repair.prepend_actions_to_audit(local_repair_audit_record(
                "doctor.local_repair",
                "write_probe_failed",
                &local_repair,
                Some("rollback-only write probe failed after local repair".to_string()),
            ));
            emit_recovery_audit_record(&recovery_audit);
            tracing::warn!(
                "Post-repair write probe failed — local repair insufficient, \
                 falling through to full JSONL rebuild"
            );
            // Don't return early — fall through to JSONL rebuild below.
        } else {
            let repair_message = repair_outcome_message_from_parts(
                early_repair.messages(),
                Some(&local_repair),
                None,
            );
            let recovery_audit = early_repair.prepend_actions_to_audit(local_repair_audit_record(
                "doctor.local_repair",
                "verified",
                &local_repair,
                None,
            ));
            emit_recovery_audit_record(&recovery_audit);
            if ctx.is_json() {
                ctx.json(&serde_json::json!({
                    "report": initial.report,
                    "repaired": early_repair.applied() || local_repair.applied(),
                    "local_repair": local_repair,
                    "recovery_audit": recovery_audit,
                    "message": repair_message,
                    "post_repair": after_local_repair.report,
                    "verified": true,
                }));
            } else {
                print_report(&initial.report, ctx)?;
                ctx.info(&repair_message);
                ctx.info("Post-repair verification:");
                print_report(&after_local_repair.report, ctx)?;
            }
            return Ok(());
        }
    } else if local_repair.applied() {
        let reason = if after_local_repair.report.ok {
            "local repair did not clear page-level integrity warnings"
        } else {
            "local repair did not clear doctor errors"
        };
        let recovery_audit = early_repair.prepend_actions_to_audit(local_repair_audit_record(
            "doctor.local_repair",
            "needs_jsonl_rebuild",
            &local_repair,
            Some(reason.to_string()),
        ));
        emit_recovery_audit_record(&recovery_audit);
    }

    // Pass-5 cycle 3: gate the broad JSONL rebuild path on the
    // FixerFilter. The rebuild addresses multiple FMs simultaneously
    // (db.open, counts.db_vs_jsonl, schema.tables, schema.columns,
    // blocked_cache_* rebuild). If `--only` is set and excludes ALL of
    // them, the operator has explicitly opted out of this remediation class.
    // Check the filter BEFORE JSONL path and repeat-repair preflights so an
    // excluded rebuild is reported as a filter refusal, not as a missing-input
    // or stale-failure artifact problem for work the caller did not ask us to do.
    if !filter_allows_jsonl_rebuild(&fixer_filter) {
        let recovery_audit = early_repair.prepend_actions_to_audit(jsonl_rebuild_audit_record(
            "doctor.jsonl_rebuild",
            "refused",
            None,
            Some(JSONL_REBUILD_FILTERED_REASON.to_string()),
        ));
        emit_recovery_audit_record(&recovery_audit);
        if ctx.is_json() {
            ctx.json(&serde_json::json!({
                "ok": false,
                "exit_code": DoctorExitCode::RefusedUnsafe.as_i32(),
                "code": DoctorExitCode::RefusedUnsafe.as_str(),
                "report": initial.report,
                "repaired": early_repair.applied(),
                "recovery_audit": recovery_audit,
                "message": "JSONL rebuild refused: filtered out by --only/--skip",
            }));
        } else {
            print_report(&initial.report, ctx)?;
            ctx.error("Refusing JSONL rebuild: filtered out by --only/--skip");
        }
        crate::shutdown::exit_process(DoctorExitCode::RefusedUnsafe.as_i32());
    }

    let Some(jsonl_path) = initial.jsonl_path.as_ref() else {
        let recovery_audit = early_repair.prepend_actions_to_audit(jsonl_rebuild_audit_record(
            "doctor.jsonl_rebuild",
            "refused",
            None,
            Some("no JSONL file found to rebuild from".to_string()),
        ));
        emit_recovery_audit_record(&recovery_audit);
        return Err(BeadsError::Config(
            "Cannot repair: no JSONL file found to rebuild from".to_string(),
        ));
    };

    if let Some(reason) = repeated_jsonl_rebuild_refusal_reason(
        &beads_dir,
        &paths.db_path,
        args.allow_repeated_repair,
    )? {
        let recovery_audit = early_repair.prepend_actions_to_audit(jsonl_rebuild_audit_record(
            "doctor.jsonl_rebuild",
            "refused",
            None,
            Some(reason.clone()),
        ));
        emit_recovery_audit_record(&recovery_audit);
        return Err(BeadsError::Config(reason));
    }

    if !ctx.is_json() {
        print_report(&initial.report, ctx)?;
        ctx.info("Repairing: rebuilding DB from JSONL...");
    }

    let repair_result = match repair_database_from_jsonl(
        &beads_dir,
        &paths.db_path,
        jsonl_path,
        cli,
        !ctx.is_json(),
        session.as_mut(),
    ) {
        Ok(result) => result,
        Err(err) => {
            let outcome = jsonl_rebuild_failure_outcome(&err);
            let recovery_audit = early_repair.prepend_actions_to_audit(jsonl_rebuild_audit_record(
                "doctor.jsonl_rebuild",
                outcome,
                None,
                Some(err.to_string()),
            ));
            emit_recovery_audit_record(&recovery_audit);
            if jsonl_rebuild_error_is_self_describing(outcome) {
                return Err(err);
            }
            return Err(BeadsError::Config(jsonl_rebuild_failure_message(&err)));
        }
    };

    let post_repair = collect_doctor_report_for_cli(&beads_dir, &paths, cli)?;
    let post_repair_verified = jsonl_rebuild_repair_verified(
        &post_repair.report,
        &repair_result.preserved_dirty_issue_ids,
    );
    let verification_failure_marker = if post_repair_verified {
        None
    } else {
        let marker_session = session.as_mut().ok_or_else(|| {
            BeadsError::Config(
                "Cannot write JSONL rebuild verification marker without a reversible repair session"
                    .to_string(),
            )
        })?;
        Some(write_jsonl_rebuild_verification_failed_marker(
            &beads_dir,
            &paths.db_path,
            &post_repair,
            &repair_result,
            marker_session,
        )?)
    };
    let verification_failure_reason = verification_failure_marker.as_ref().map(|path| {
        format!(
            "post-repair verification failed; evidence marker written to '{}'",
            path.display()
        )
    });
    let recovery_audit = early_repair.prepend_actions_to_audit(jsonl_rebuild_audit_record(
        "doctor.jsonl_rebuild",
        if post_repair_verified {
            "verified"
        } else {
            "verification_failed"
        },
        Some(&repair_result),
        verification_failure_reason.clone(),
    ));
    emit_recovery_audit_record(&recovery_audit);

    if ctx.is_json() {
        ctx.json(&serde_json::json!({
            "report": initial.report,
            "repaired": true,
            "local_repair": local_repair,
            "recovery_audit": recovery_audit,
            "imported": repair_result.imported,
            "skipped": repair_result.skipped,
            "fk_violations_cleaned": repair_result.fk_violations_cleaned,
            "preserved_tombstones": repair_result.preserved_tombstones,
            "preserved_dirty_issues": repair_result.preserved_dirty_issues,
            "preserved_dirty_issue_ids": &repair_result.preserved_dirty_issue_ids,
            "preserved_history": &repair_result.preserved_history,
            "history_preservation_warnings": &repair_result.history_preservation_warnings,
            "verified_backups": &repair_result.verified_backups,
            "post_repair": post_repair.report,
            "verified": post_repair_verified,
            "recovery_failure_marker": verification_failure_marker
                .as_ref()
                .map(|path| path.display().to_string()),
        }));
    } else {
        ctx.info(&format!(
            "Repair complete: imported {}, skipped {}",
            repair_result.imported, repair_result.skipped
        ));
        if repair_result.preserved_dirty_issues > 0 {
            ctx.info(&format!(
                "Preserved {} unflushed local issue(s) across the rebuild; run `br sync --flush-only` to export them",
                repair_result.preserved_dirty_issues
            ));
        }
        if !repair_result.preserved_history.is_empty() {
            let summary = repair_result
                .preserved_history
                .iter()
                .map(|entry| {
                    if entry.skipped > 0 {
                        format!(
                            "{} {} (+{} orphaned skipped)",
                            entry.restored, entry.table, entry.skipped
                        )
                    } else {
                        format!("{} {}", entry.restored, entry.table)
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            ctx.info(&format!(
                "Preserved DB-only history across the rebuild: {summary}"
            ));
        }
        for warning in &repair_result.history_preservation_warnings {
            ctx.warning(&format!("History NOT fully preserved: {warning}"));
        }
        if let Some(reason) = verification_failure_reason.as_deref() {
            ctx.warning(reason);
        }
        ctx.info("Post-repair verification:");
        print_report(&post_repair.report, ctx)?;
    }

    if !post_repair_verified {
        return Err(BeadsError::Config(
            "Repair completed, but post-repair verification still found issues".to_string(),
        ));
    }

    Ok(())
}

/// Build and emit the `br.doctor.triage.v1` envelope from a flat
/// `DoctorReport`. Used by `--robot-triage` to short-circuit the flat
/// run with a single JSON read.
fn emit_flat_robot_triage(report: &DoctorReport) {
    use crate::cli::commands::doctor_subsystems::surface::{
        TriageFinding, build_triage_envelope, emit_robot_triage,
    };
    let mut healthy = 0usize;
    let mut warn = 0usize;
    let mut error = 0usize;
    let mut findings: Vec<TriageFinding> = Vec::new();
    for c in &report.checks {
        match c.status {
            CheckStatus::Ok => healthy += 1,
            CheckStatus::Warn => {
                warn += 1;
                findings.push(TriageFinding {
                    id: c.name.clone(),
                    severity: "P2".to_string(),
                    message: c.message.clone().unwrap_or_default(),
                });
            }
            CheckStatus::Error => {
                error += 1;
                findings.push(TriageFinding {
                    id: c.name.clone(),
                    severity: "P0".to_string(),
                    message: c.message.clone().unwrap_or_default(),
                });
            }
        }
    }
    let envelope = build_triage_envelope(healthy, warn, error, findings);
    emit_robot_triage(&envelope);
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::franken_sync::Connection;
    use crate::health::{AnomalyClass, WorkspaceHealth};
    use crate::model::{Issue, IssueType, Priority, Status};
    use crate::storage::SqliteStorage;
    use chrono::Utc;
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::{NamedTempFile, TempDir};

    fn find_check<'a>(checks: &'a [CheckResult], name: &str) -> Option<&'a CheckResult> {
        checks.iter().find(|check| check.name == name)
    }

    fn backdate_file_two_hours(path: &Path) {
        let two_hours_ago =
            std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 60 * 60);
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(two_hours_ago))
            .unwrap();
    }

    fn sample_issue(id: &str, title: &str) -> Issue {
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
            bypassed_policy: None,
            bypass_reason: None,
            policy_gates_fired: None,
            due_at: None,
            defer_until: None,
            external_ref: None,
            source_system: None,
            source_repo: None,
            source_repo_path: None,
            agent_context: None,
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
            labels: Vec::new(),
            dependencies: Vec::new(),
            comments: Vec::new(),
        }
    }

    fn create_sample_issue(storage: &mut SqliteStorage, id: &str, title: &str) {
        storage
            .create_issue(&sample_issue(id, title), "tester")
            .unwrap();
    }

    fn install_valid_pending_merge_receipt(db_path: &Path) -> SyncMergePendingReceipt {
        let mut storage = SqliteStorage::open(db_path).expect("open pending-receipt fixture");
        let database_before =
            crate::sync::capture_sync_database_witness(&storage).expect("capture database before");
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
            actor: "doctor-test".to_string(),
            event_attribution: crate::storage::EventAttribution::default(),
            capacity_policy: crate::close_policy::CapacityPolicy::default(),
            retention_days: None,
            export_as_of: chrono::DateTime::parse_from_rfc3339("2026-07-27T00:00:00Z")
                .expect("fixed export timestamp")
                .with_timezone(&Utc),
            changed_kept_issue_ids: Vec::new(),
            kept_issue_witnesses: Vec::new(),
            deleted_issue_ids: Vec::new(),
            note_witnesses: Vec::new(),
            database_before,
        };
        let database_after =
            crate::sync::capture_sync_merge_core_witness(&storage).expect("capture database after");
        let receipt = SyncMergePendingReceipt::new(
            intent,
            "2026-07-27T00:00:00Z".to_string(),
            database_after,
            "5".repeat(64),
            0,
            &[],
            Vec::new(),
        )
        .expect("construct receipt");
        receipt.validate().expect("fixture receipt must validate");
        storage
            .set_metadata(
                crate::sync::METADATA_SYNC_MERGE_PENDING,
                &serde_json::to_string(&receipt).expect("serialize receipt"),
            )
            .expect("persist pending receipt");
        receipt
    }

    #[test]
    fn read_only_open_probe_check_is_ok_on_a_healthy_database() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        drop(SqliteStorage::open(&db_path).unwrap());

        let mut checks = Vec::new();
        check_read_only_open_observational(&db_path, &mut checks).unwrap();
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].name, "db.read_only_open_observational");
        assert!(
            matches!(checks[0].status, CheckStatus::Ok),
            "expected Ok, got {:?}: {:?}",
            checks[0].status,
            checks[0].message
        );
        let details = checks[0].details.as_ref().expect("details");
        assert_eq!(details["opened"], serde_json::Value::Bool(true));
        assert_eq!(
            details["finding_id"],
            serde_json::Value::String(
                "fm-state_files-read-only-open-not-observational".to_string()
            )
        );
    }

    fn database_family_bytes(db_path: &Path) -> BTreeMap<String, Option<Vec<u8>>> {
        crate::storage::sqlite::database_family_snapshot(db_path).expect("read database family")
    }

    /// Assert that a read-only operation left the database family untouched:
    /// the main file, WAL, and rollback journal must be byte-identical, and the
    /// WAL-index (`-shm`) may differ only inside the reader-mark array
    /// (`crate::storage::sqlite::SHM_READ_MARK_RANGE`, GitHub #476).
    ///
    /// The comparison itself lives in the storage layer so this test, the
    /// storage-level tests, and the `db.read_only_open_observational` doctor
    /// check all enforce one definition of the contract. Failures name the
    /// artifact and the first differing offsets instead of dumping whole files.
    fn assert_database_family_read_only(
        before: &BTreeMap<String, Option<Vec<u8>>>,
        after: &BTreeMap<String, Option<Vec<u8>>>,
        context: &str,
    ) {
        let diffs = crate::storage::sqlite::database_family_read_only_diffs(before, after);
        assert!(
            diffs.is_empty(),
            "{context}: read-only contract violated: {diffs:#?}"
        );
    }

    #[test]
    fn pending_sync_merge_read_only_inspector_accepts_valid_v2_receipt() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let receipt = install_valid_pending_merge_receipt(&db_path);

        let state = inspect_pending_sync_merge_at_path(&db_path)
            .expect("inspect pending receipt")
            .expect("pending receipt must be visible");

        assert_eq!(state.condition, PendingSyncMergeCondition::Valid);
        assert_eq!(
            state.receipt_id.as_deref(),
            Some(receipt.receipt_id.as_str())
        );
        assert_eq!(state.phase.as_deref(), Some("database_committed"));
        assert_eq!(state.resolution.as_deref(), Some("manual"));
        assert_eq!(state.expected_jsonl_issue_count, Some(0));
    }

    #[test]
    fn pending_sync_merge_authority_inspector_is_coherent_and_byte_identical() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let receipt = install_valid_pending_merge_receipt(&db_path);
        let authority = Arc::new(
            crate::sync::blocking_database_family_write_lock_with_timeout(
                temp.path(),
                &db_path,
                Some(1_000),
            )
            .unwrap(),
        );
        authority.bind_database_inode_for_mutation().unwrap();
        let before = database_family_bytes(&db_path);

        let state = inspect_pending_sync_merge_under_authority(&db_path, &authority)
            .expect("inspect under authority")
            .expect("pending receipt must remain visible");

        assert_eq!(
            state.receipt_id.as_deref(),
            Some(receipt.receipt_id.as_str())
        );
        // The main file, WAL, and journal must be byte-identical; the
        // WAL-index may only carry the reader mark the WAL read lock
        // registered (GitHub #476).
        assert_database_family_read_only(
            &before,
            &database_family_bytes(&db_path),
            "read-only authority inspection",
        );
    }

    #[test]
    fn pending_sync_merge_authority_inspector_treats_missing_database_as_absent() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("missing.db");
        let authority = Arc::new(
            crate::sync::blocking_database_family_write_lock_with_timeout(
                temp.path(),
                &db_path,
                Some(1_000),
            )
            .unwrap(),
        );

        let state = inspect_pending_sync_merge_under_authority(&db_path, &authority)
            .expect("inspect missing database under authority");

        assert!(
            state.is_none(),
            "a definitively missing database cannot contain a pending merge receipt"
        );
        assert!(
            !db_path.exists(),
            "read-only pending-state inspection must not initialize a missing database"
        );
    }

    /// GitHub #412 secondary regression: a stale-schema database must surface
    /// as `SchemaMismatch` (routing to the reviewed migration guidance), never
    /// as a generic "database is busy" that leaves migration unreachable.
    #[test]
    fn pending_sync_merge_authority_inspector_surfaces_schema_mismatch_for_stale_schema() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        {
            let storage = SqliteStorage::open(&db_path).unwrap();
            storage.execute_raw("PRAGMA user_version = 16").unwrap();
        }
        let authority = Arc::new(
            crate::sync::blocking_database_family_write_lock_with_timeout(
                temp.path(),
                &db_path,
                Some(1_000),
            )
            .unwrap(),
        );

        let err = inspect_pending_sync_merge_under_authority(&db_path, &authority).unwrap_err();

        assert!(
            matches!(err, BeadsError::SchemaMismatch { found: 16, .. }),
            "stale schema must surface as SchemaMismatch, not engine contention: {err}"
        );
    }

    #[test]
    fn pending_sync_merge_read_only_inspector_distinguishes_legacy_and_malformed() {
        let legacy = TempDir::new().unwrap();
        let legacy_db = legacy.path().join("beads.db");
        let mut storage = SqliteStorage::open(&legacy_db).unwrap();
        storage
            .set_metadata(METADATA_SYNC_MERGE_PENDING_LEGACY, "legacy-receipt")
            .unwrap();
        drop(storage);
        let legacy_state = inspect_pending_sync_merge_at_path(&legacy_db)
            .unwrap()
            .expect("legacy state");
        assert_eq!(legacy_state.condition, PendingSyncMergeCondition::Legacy);
        assert_eq!(
            legacy_state.metadata_key,
            METADATA_SYNC_MERGE_PENDING_LEGACY
        );

        let malformed = TempDir::new().unwrap();
        let malformed_db = malformed.path().join("beads.db");
        let mut storage = SqliteStorage::open(&malformed_db).unwrap();
        storage
            .set_metadata(crate::sync::METADATA_SYNC_MERGE_PENDING, "{")
            .unwrap();
        drop(storage);
        let malformed_state = inspect_pending_sync_merge_at_path(&malformed_db)
            .unwrap()
            .expect("malformed state");
        assert_eq!(
            malformed_state.condition,
            PendingSyncMergeCondition::Malformed
        );
        assert!(
            malformed_state.diagnostic.contains("not valid JSON"),
            "{malformed_state:?}"
        );
    }

    #[test]
    fn pending_sync_merge_read_only_inspector_rejects_dual_legacy_and_current_keys() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        storage
            .set_metadata(METADATA_SYNC_MERGE_PENDING_LEGACY, "legacy-receipt")
            .unwrap();
        storage
            .set_metadata(crate::sync::METADATA_SYNC_MERGE_PENDING, "{}")
            .unwrap();
        drop(storage);

        let state = inspect_pending_sync_merge_at_path(&db_path)
            .unwrap()
            .expect("dual-key state");

        assert_eq!(state.condition, PendingSyncMergeCondition::Malformed);
        assert!(
            state
                .metadata_key
                .contains(METADATA_SYNC_MERGE_PENDING_LEGACY),
            "{state:?}"
        );
        assert!(
            state
                .metadata_key
                .contains(crate::sync::METADATA_SYNC_MERGE_PENDING),
            "{state:?}"
        );
        assert!(
            state.diagnostic.contains("both legacy (1) and current (1)"),
            "{state:?}"
        );
    }

    #[test]
    fn pending_sync_merge_read_only_inspector_rejects_duplicate_and_empty_rows() {
        let duplicate = TempDir::new().unwrap();
        let duplicate_db = duplicate.path().join("beads.db");
        drop(SqliteStorage::open(&duplicate_db).unwrap());
        let conn = Connection::open(duplicate_db.to_string_lossy().into_owned()).unwrap();
        for value in ["{}", "{}"] {
            conn.execute_with_params(
                "INSERT INTO metadata (key, value) VALUES (?, ?)",
                &[
                    SqliteValue::from(crate::sync::METADATA_SYNC_MERGE_PENDING),
                    SqliteValue::from(value),
                ],
            )
            .unwrap();
        }
        conn.close().unwrap();
        let duplicate_state = inspect_pending_sync_merge_at_path(&duplicate_db)
            .unwrap()
            .expect("duplicate state");
        assert_eq!(
            duplicate_state.condition,
            PendingSyncMergeCondition::Malformed
        );
        assert!(
            duplicate_state.diagnostic.contains("duplicate receipts"),
            "{duplicate_state:?}"
        );

        let duplicate_legacy = TempDir::new().unwrap();
        let duplicate_legacy_db = duplicate_legacy.path().join("beads.db");
        drop(SqliteStorage::open(&duplicate_legacy_db).unwrap());
        let conn = Connection::open(duplicate_legacy_db.to_string_lossy().into_owned()).unwrap();
        for value in ["legacy-a", "legacy-b"] {
            conn.execute_with_params(
                "INSERT INTO metadata (key, value) VALUES (?, ?)",
                &[
                    SqliteValue::from(METADATA_SYNC_MERGE_PENDING_LEGACY),
                    SqliteValue::from(value),
                ],
            )
            .unwrap();
        }
        conn.close().unwrap();
        let duplicate_legacy_state = inspect_pending_sync_merge_at_path(&duplicate_legacy_db)
            .unwrap()
            .expect("duplicate legacy state");
        assert_eq!(
            duplicate_legacy_state.condition,
            PendingSyncMergeCondition::Malformed
        );
        assert_eq!(
            duplicate_legacy_state.metadata_key,
            METADATA_SYNC_MERGE_PENDING_LEGACY
        );
        assert!(
            duplicate_legacy_state
                .diagnostic
                .contains("duplicate receipts"),
            "{duplicate_legacy_state:?}"
        );

        let empty = TempDir::new().unwrap();
        let empty_db = empty.path().join("beads.db");
        let mut storage = SqliteStorage::open(&empty_db).unwrap();
        storage
            .set_metadata(crate::sync::METADATA_SYNC_MERGE_PENDING, " ")
            .unwrap();
        drop(storage);
        let empty_state = inspect_pending_sync_merge_at_path(&empty_db)
            .unwrap()
            .expect("empty state");
        assert_eq!(empty_state.condition, PendingSyncMergeCondition::Malformed);
        assert!(
            empty_state.diagnostic.contains("NULL or empty"),
            "{empty_state:?}"
        );
    }

    #[test]
    fn doctor_pending_merge_check_is_dedicated_and_never_repairs() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        install_valid_pending_merge_receipt(&db_path);
        let mut checks = Vec::new();

        check_pending_sync_merge(&db_path, &mut checks);

        let check = find_check(&checks, "sync.merge_pending").expect("dedicated pending check");
        assert_eq!(check.status, CheckStatus::Warn);
        let details = check.details.as_ref().expect("structured pending details");
        assert_eq!(details["pending"], true);
        assert_eq!(details["state"]["condition"], "valid");
        assert!(
            details["remediation"]
                .as_str()
                .is_some_and(|text| text.contains("br sync --merge")),
            "{details}"
        );
    }

    // The four pending-merge CLI gate scenarios (read-only list skips a newer
    // JSONL generation, `create` and `--no-db create` refused before any
    // write, doctor read-only + every repair surface refused) run end to end
    // in tests/e2e_basic_lifecycle.rs
    // (`e2e_pending_merge_gate_refuses_file_only_mutations_without_changing_witnesses`),
    // where the pending receipt comes from a real interrupted merge.

    fn insert_dependency_row(conn: &Connection, issue_id: &str, depends_on_id: &str) {
        conn.execute_with_params(
            "INSERT INTO dependencies(issue_id, depends_on_id, type) VALUES (?1, ?2, ?3)",
            &[
                SqliteValue::Text(issue_id.into()),
                SqliteValue::Text(depends_on_id.into()),
                SqliteValue::Text("blocks".into()),
            ],
        )
        .unwrap();
    }

    /// #350 helper: build a JSONL issue with a `blocks` dependency on each of
    /// `blocked_by`, set to `status`.
    fn issue_with_blockers(id: &str, status: Status, blocked_by: &[&str]) -> Issue {
        let mut issue = sample_issue(id, id);
        issue.status = status;
        issue.dependencies = blocked_by
            .iter()
            .map(|target| crate::model::Dependency {
                issue_id: id.to_string(),
                depends_on_id: (*target).to_string(),
                dep_type: crate::model::DependencyType::Blocks,
                created_at: Utc::now(),
                created_by: None,
                metadata: None,
                thread_id: None,
            })
            .collect();
        issue
    }

    fn write_issues_jsonl(path: &Path, issues: &[Issue]) {
        let mut body = String::new();
        for issue in issues {
            body.push_str(&serde_json::to_string(issue).unwrap());
            body.push('\n');
        }
        fs::write(path, body).unwrap();
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn test_dep_graph_jsonl_flags_dead_edges_and_fully_unblocked() {
        // #350: fixture graph —
        //  bd-a (open) blocked_by bd-closed (closed)  -> dead edge + fully unblocked
        //  bd-b (open) blocked_by bd-open (open)       -> live, not flagged
        //  bd-c (open) blocked_by bd-missing (absent)  -> dead edge + fully unblocked
        //  bd-d (open) blocked_by [bd-closed, bd-open] -> 1 dead edge, NOT fully unblocked
        //  bd-closed (closed) blocked_by ...           -> terminal, skipped
        let temp = TempDir::new().unwrap();
        let jsonl = temp.path().join("issues.jsonl");
        let issues = vec![
            issue_with_blockers("bd-a", Status::Open, &["bd-closed"]),
            issue_with_blockers("bd-b", Status::Open, &["bd-open"]),
            issue_with_blockers("bd-c", Status::Open, &["bd-missing"]),
            issue_with_blockers("bd-d", Status::Open, &["bd-closed", "bd-open"]),
            {
                let mut closed = sample_issue("bd-closed", "bd-closed");
                closed.status = Status::Closed;
                closed.closed_at = Some(Utc::now());
                closed
            },
            sample_issue("bd-open", "bd-open"),
        ];
        write_issues_jsonl(&jsonl, &issues);

        let mut checks = Vec::new();
        check_dependency_graph_jsonl(&jsonl, &mut checks);

        // #432: the dead-edge check warns because bd-c's blocker is DANGLING
        // (absent from the JSONL). Satisfied edges (bd-a, bd-d -> bd-closed)
        // are carried in details but do not on their own degrade the check.
        let dead = find_check(&checks, "dep.dead_closed_blocking_edges").expect("dead-edge check");
        assert_eq!(dead.status, CheckStatus::Warn, "{dead:?}");
        let details = dead.details.as_ref().unwrap();
        // bd-a, bd-c, bd-d each have >=1 dead blocking edge (compat count).
        assert_eq!(
            details.get("count").and_then(serde_json::Value::as_u64),
            Some(3),
            "expected 3 issues with dead edges: {dead:?}"
        );
        assert_eq!(
            details
                .get("dangling_count")
                .and_then(serde_json::Value::as_u64),
            Some(1),
            "only bd-c has a dangling blocker: {dead:?}"
        );
        // The warn message names only the dangling issue.
        let message = dead.message.as_deref().unwrap();
        assert!(message.contains("bd-c"), "{message}");
        assert!(!message.contains("bd-a"), "{message}");
        // Per-issue partitions discriminate satisfied vs dangling.
        let issues = details.get("issues").and_then(serde_json::Value::as_array);
        let entry = |id: &str| -> &serde_json::Value {
            issues
                .unwrap()
                .iter()
                .find(|item| item.get("id").and_then(serde_json::Value::as_str) == Some(id))
                .unwrap_or_else(|| panic!("no dead-edge entry for {id}: {dead:?}"))
        };
        assert_eq!(
            entry("bd-a").get("satisfied_blockers").unwrap(),
            &serde_json::json!(["bd-closed"])
        );
        assert_eq!(
            entry("bd-a").get("dangling_blockers").unwrap(),
            &serde_json::json!([])
        );
        assert_eq!(
            entry("bd-c").get("dangling_blockers").unwrap(),
            &serde_json::json!(["bd-missing"])
        );
        assert_eq!(
            entry("bd-c").get("satisfied_blockers").unwrap(),
            &serde_json::json!([])
        );
        // Pre-#432 compat key still carries the union.
        assert_eq!(
            entry("bd-d").get("dead_blockers").unwrap(),
            &serde_json::json!(["bd-closed"])
        );

        // #432: fully-unblocked open issues in plain `open` status are the
        // benign steady state — Ok with details, all listed as ready.
        let unblocked =
            find_check(&checks, "dep.fully_unblocked_open").expect("fully-unblocked check");
        assert_eq!(unblocked.status, CheckStatus::Ok, "{unblocked:?}");
        let udetails = unblocked.details.as_ref().unwrap();
        let unblocked_ids: Vec<String> = udetails
            .get("issues")
            .and_then(serde_json::Value::as_array)
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        // bd-a and bd-c are fully unblocked (all blockers dead). bd-d is NOT
        // (bd-open is still live). bd-b is NOT (its only blocker is live).
        assert!(
            unblocked_ids.contains(&"bd-a".to_string()),
            "{unblocked_ids:?}"
        );
        assert!(
            unblocked_ids.contains(&"bd-c".to_string()),
            "{unblocked_ids:?}"
        );
        assert!(
            !unblocked_ids.contains(&"bd-d".to_string()),
            "{unblocked_ids:?}"
        );
        assert!(
            !unblocked_ids.contains(&"bd-b".to_string()),
            "{unblocked_ids:?}"
        );
        assert_eq!(unblocked_ids.len(), 2, "{unblocked_ids:?}");
        assert_eq!(
            udetails.get("ready").unwrap(),
            &serde_json::json!(["bd-a", "bd-c"]),
            "{unblocked:?}"
        );
        assert_eq!(
            udetails.get("stale_blocked").unwrap(),
            &serde_json::json!([]),
            "{unblocked:?}"
        );
    }

    #[test]
    fn test_dep_graph_jsonl_satisfied_only_reports_ok_with_details() {
        // #432 core scenario: `create` / `dep add` / `close` and nothing
        // else. The blocker is present and closed — a satisfied dependency —
        // so BOTH checks must be Ok (doctor.ok stays true) while still
        // carrying full machine-readable details.
        let temp = TempDir::new().unwrap();
        let jsonl = temp.path().join("issues.jsonl");
        let issues = vec![issue_with_blockers("bd-dep", Status::Open, &["bd-done"]), {
            let mut closed = sample_issue("bd-done", "bd-done");
            closed.status = Status::Closed;
            closed.closed_at = Some(Utc::now());
            closed
        }];
        write_issues_jsonl(&jsonl, &issues);

        let mut checks = Vec::new();
        check_dependency_graph_jsonl(&jsonl, &mut checks);

        let dead = find_check(&checks, "dep.dead_closed_blocking_edges").unwrap();
        assert_eq!(dead.status, CheckStatus::Ok, "{dead:?}");
        let details = dead.details.as_ref().unwrap();
        assert_eq!(
            details
                .get("dangling_count")
                .and_then(serde_json::Value::as_u64),
            Some(0)
        );
        let entry = &details.get("issues").unwrap().as_array().unwrap()[0];
        assert_eq!(
            entry.get("satisfied_blockers").unwrap(),
            &serde_json::json!(["bd-done"])
        );
        // The benign path must NOT steer operators toward `br dep remove`
        // (#432 / the #426 bulk-delete incident).
        assert!(details.get("remediation").is_none(), "{dead:?}");
        assert!(
            !dead
                .message
                .as_deref()
                .unwrap_or_default()
                .contains("dep remove"),
            "{dead:?}"
        );

        let unblocked = find_check(&checks, "dep.fully_unblocked_open").unwrap();
        assert_eq!(unblocked.status, CheckStatus::Ok, "{unblocked:?}");
        assert_eq!(
            unblocked.details.as_ref().unwrap().get("ready").unwrap(),
            &serde_json::json!(["bd-dep"])
        );
    }

    #[test]
    fn test_dep_graph_jsonl_stale_blocked_status_warns() {
        // #432: status still `blocked` while every blocker is closed is the
        // one derivable real inconsistency — it warns. A deliberately
        // deferred issue with the same graph shape is excluded-with-reason,
        // not a finding.
        let temp = TempDir::new().unwrap();
        let jsonl = temp.path().join("issues.jsonl");
        let issues = vec![
            issue_with_blockers("bd-stale", Status::Blocked, &["bd-done"]),
            issue_with_blockers("bd-deferred", Status::Deferred, &["bd-done"]),
            {
                let mut closed = sample_issue("bd-done", "bd-done");
                closed.status = Status::Closed;
                closed.closed_at = Some(Utc::now());
                closed
            },
        ];
        write_issues_jsonl(&jsonl, &issues);

        let mut checks = Vec::new();
        check_dependency_graph_jsonl(&jsonl, &mut checks);

        let unblocked = find_check(&checks, "dep.fully_unblocked_open").unwrap();
        assert_eq!(unblocked.status, CheckStatus::Warn, "{unblocked:?}");
        let details = unblocked.details.as_ref().unwrap();
        assert_eq!(
            details.get("stale_blocked").unwrap(),
            &serde_json::json!(["bd-stale"])
        );
        let excluded = details.get("excluded").unwrap().as_array().unwrap();
        assert_eq!(excluded.len(), 1, "{unblocked:?}");
        assert_eq!(
            excluded[0].get("id").and_then(serde_json::Value::as_str),
            Some("bd-deferred")
        );
        let message = unblocked.message.as_deref().unwrap();
        assert!(message.contains("bd-stale"), "{message}");
        assert!(!message.contains("bd-deferred"), "{message}");

        // Satisfied-only edges: the dead-edge check stays Ok.
        let dead = find_check(&checks, "dep.dead_closed_blocking_edges").unwrap();
        assert_eq!(dead.status, CheckStatus::Ok, "{dead:?}");
    }

    #[test]
    fn test_dep_graph_jsonl_clean_graph_reports_ok() {
        // A graph with only live blockers must produce two Ok checks.
        let temp = TempDir::new().unwrap();
        let jsonl = temp.path().join("issues.jsonl");
        let issues = vec![
            issue_with_blockers("bd-x", Status::Open, &["bd-y"]),
            sample_issue("bd-y", "bd-y"),
        ];
        write_issues_jsonl(&jsonl, &issues);

        let mut checks = Vec::new();
        check_dependency_graph_jsonl(&jsonl, &mut checks);

        assert_eq!(
            find_check(&checks, "dep.dead_closed_blocking_edges")
                .unwrap()
                .status,
            CheckStatus::Ok
        );
        assert_eq!(
            find_check(&checks, "dep.fully_unblocked_open")
                .unwrap()
                .status,
            CheckStatus::Ok
        );
    }

    fn dependency_row_count(conn: &Connection, issue_id: &str, depends_on_id: &str) -> i64 {
        let rows = conn
            .query("SELECT issue_id, depends_on_id FROM dependencies")
            .unwrap();
        rows.iter()
            .filter(|row| {
                let values = row.values();
                matches!(
                    (values.first(), values.get(1)),
                    (Some(SqliteValue::Text(owner)), Some(SqliteValue::Text(target)))
                        if owner.as_str() == issue_id && target.as_str() == depends_on_id
                )
            })
            .count()
            .try_into()
            .expect("dependency row count should fit in i64")
    }

    fn seed_dependency_orphan_repair_fixture(db_path: &Path) {
        let mut storage = SqliteStorage::open(db_path).unwrap();
        for (id, title) in [
            ("bd-keep-d", "Keep external target"),
            ("bd-owner-d", "Prune missing local target"),
            ("bd-valid-d", "Keep valid owner"),
            ("bd-target-d", "Keep valid target"),
        ] {
            create_sample_issue(&mut storage, id, title);
        }
        drop(storage);

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let _ = conn.execute("PRAGMA foreign_keys = OFF");
        for (issue_id, depends_on_id) in [
            ("bd-orphan-fix-d", "bd-other"),
            ("bd-owner-d", "bd-missing-local-target"),
            ("bd-keep-d", "external:upstream-1"),
            ("bd-valid-d", "bd-target-d"),
        ] {
            insert_dependency_row(&conn, issue_id, depends_on_id);
        }
    }

    fn dependency_orphan_report(db_path: &Path) -> DoctorReport {
        let mut report = DoctorReport {
            ok: false,
            workspace_health: Some("degraded".to_string()),
            reliability_audit: None,
            checks: Vec::new(),
        };
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        check_dependencies_orphans(&conn, &mut report.checks);
        report
    }

    #[test]
    fn test_repair_orphan_cleanup_preserves_external_dependency_endpoints() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let mut epic = sample_issue("bd-epic", "Epic");
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

        let cleaned = cleanup_repair_missing_issue_references(&mut storage).unwrap();

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
            "external dependency endpoints must survive doctor repair cleanup"
        );
    }

    #[test]
    fn test_repair_orphan_cleanup_rebuilds_blocked_cache_after_dependency_cleanup() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue = sample_issue("bd-local", "Local");
        storage.create_issue(&issue, "tester").unwrap();

        storage
            .execute_test_sql(
                "PRAGMA foreign_keys = OFF;
                 INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by)
                 VALUES ('bd-local', 'bd-missing', 'blocks', '2026-01-01T00:00:00Z', 'tester');
                 INSERT INTO blocked_issues_cache (issue_id, blocked_by, blocked_at)
                 VALUES ('bd-local', '[\"bd-missing\"]', '2026-01-01T00:00:00Z');
                 PRAGMA foreign_keys = ON;",
            )
            .unwrap();

        let cleaned = cleanup_repair_missing_issue_references(&mut storage).unwrap();

        assert_eq!(
            cleaned, 1,
            "only the missing dependency row should be removed"
        );
        let cache_rows = storage
            .execute_raw_query(
                "SELECT issue_id
                 FROM blocked_issues_cache
                 WHERE issue_id = 'bd-local'",
            )
            .unwrap();
        assert!(
            cache_rows.is_empty(),
            "dependency cleanup must rebuild stale blocked cache rows"
        );
    }

    #[test]
    fn test_classify_doctor_checks_marks_write_probe_failure_recoverable() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let jsonl_path = temp.path().join("issues.jsonl");
        let checks = vec![CheckResult {
            name: "db.write_probe".to_string(),
            status: CheckStatus::Error,
            message: Some(
                "Rollback-only issue write failed: database disk image is malformed".to_string(),
            ),
            details: Some(serde_json::json!({ "issue_id": "bd-probe" })),
        }];

        let classification = classify_doctor_checks(&db_path, &jsonl_path, &checks);

        assert_eq!(classification.health, WorkspaceHealth::Recoverable);
        assert!(
            classification
                .anomalies
                .iter()
                .any(|anomaly| matches!(anomaly, AnomalyClass::WriteProbeFailed { .. })),
            "expected write-probe failure anomaly: {:?}",
            classification.anomalies
        );
    }

    #[test]
    fn test_classify_doctor_checks_marks_invalid_jsonl_unsafe() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let jsonl_path = temp.path().join("issues.jsonl");
        let checks = vec![CheckResult {
            name: "jsonl.parse".to_string(),
            status: CheckStatus::Error,
            message: Some("Malformed or invalid issue records: 1".to_string()),
            details: Some(serde_json::json!({ "path": jsonl_path.display().to_string() })),
        }];

        let classification = classify_doctor_checks(&db_path, &jsonl_path, &checks);

        assert_eq!(classification.health, WorkspaceHealth::Unsafe);
        assert!(
            classification
                .anomalies
                .iter()
                .any(|anomaly| matches!(anomaly, AnomalyClass::JsonlParseError { .. })),
            "expected JSONL parse anomaly: {:?}",
            classification.anomalies
        );
    }

    #[test]
    fn test_classify_doctor_checks_marks_repairable_integrity_warnings_recoverable() {
        for (check_name, message) in [
            ("sqlite.integrity_check", "Page 55: never used"),
            (
                "sqlite3.integrity_check",
                "row 42 missing from index idx_foo",
            ),
        ] {
            let temp = TempDir::new().unwrap();
            let db_path = temp.path().join("beads.db");
            let jsonl_path = temp.path().join("issues.jsonl");
            let checks = vec![CheckResult {
                name: check_name.to_string(),
                status: CheckStatus::Warn,
                message: Some(message.to_string()),
                details: None,
            }];

            let classification = classify_doctor_checks(&db_path, &jsonl_path, &checks);

            assert_eq!(classification.health, WorkspaceHealth::Recoverable);
            assert!(
                classification.anomalies.iter().any(|anomaly| {
                    matches!(
                        anomaly,
                        AnomalyClass::DatabaseCorrupt { detail } if detail == message
                    )
                }),
                "expected repairable integrity warning anomaly for {check_name}: {:?}",
                classification.anomalies
            );
        }
    }

    #[test]
    fn test_classify_doctor_checks_ignores_benign_integrity_warning() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let jsonl_path = temp.path().join("issues.jsonl");
        let checks = vec![CheckResult {
            name: "sqlite.integrity_check".to_string(),
            status: CheckStatus::Warn,
            message: Some("out of order index idx_foo".to_string()),
            details: None,
        }];

        let classification = classify_doctor_checks(&db_path, &jsonl_path, &checks);

        assert_eq!(classification.health, WorkspaceHealth::Healthy);
        assert!(
            classification.anomalies.is_empty(),
            "benign integrity warning should not create health anomalies: {:?}",
            classification.anomalies
        );
    }

    #[test]
    fn test_classify_doctor_checks_marks_count_mismatch_degraded() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let jsonl_path = temp.path().join("issues.jsonl");
        let checks = vec![CheckResult {
            name: "counts.db_vs_jsonl".to_string(),
            status: CheckStatus::Warn,
            message: Some("DB and JSONL counts differ".to_string()),
            details: Some(serde_json::json!({ "db": 2, "jsonl": 1 })),
        }];

        let classification = classify_doctor_checks(&db_path, &jsonl_path, &checks);

        assert_eq!(classification.health, WorkspaceHealth::Degraded);
        assert!(
            classification.anomalies.iter().any(|anomaly| {
                matches!(
                    anomaly,
                    AnomalyClass::DbJsonlCountMismatch {
                        db_count: 2,
                        jsonl_count: 1
                    }
                )
            }),
            "expected count mismatch anomaly: {:?}",
            classification.anomalies
        );
    }

    #[test]
    fn test_classify_doctor_checks_prefers_sync_metadata_booleans_over_message() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let jsonl_path = temp.path().join("issues.jsonl");
        let checks = vec![CheckResult {
            name: "sync.metadata".to_string(),
            status: CheckStatus::Ok,
            message: Some("External changes pending import".to_string()),
            details: Some(serde_json::json!({
                "pending_import": false,
                "pending_export": false,
                "jsonl_newer": false,
                "db_newer": false,
            })),
        }];

        let classification = classify_doctor_checks(&db_path, &jsonl_path, &checks);

        assert_eq!(classification.health, WorkspaceHealth::Healthy);
        assert!(
            classification.anomalies.is_empty(),
            "machine sync booleans must override stale prose: {:?}",
            classification.anomalies
        );
    }

    #[test]
    fn test_classify_doctor_checks_records_ok_pending_import_as_degraded_advisory() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let jsonl_path = temp.path().join("issues.jsonl");
        let checks = vec![CheckResult {
            name: "sync.metadata".to_string(),
            status: CheckStatus::Ok,
            message: Some("External changes pending import".to_string()),
            details: Some(serde_json::json!({
                "pending_import": true,
                "pending_export": false,
                "jsonl_newer": true,
                "db_newer": false,
            })),
        }];

        let classification = classify_doctor_checks(&db_path, &jsonl_path, &checks);

        assert_eq!(classification.health, WorkspaceHealth::Degraded);
        assert!(
            classification
                .anomalies
                .iter()
                .any(|anomaly| matches!(anomaly, AnomalyClass::JsonlNewer)),
            "pending import remains an advisory health anomaly: {:?}",
            classification.anomalies
        );
        assert!(
            !classification
                .anomalies
                .iter()
                .any(|anomaly| matches!(anomaly, AnomalyClass::DbNewer)),
            "one-way import must not invent DB-newer evidence: {:?}",
            classification.anomalies
        );
    }

    #[test]
    fn test_classify_doctor_checks_records_both_sync_metadata_directions() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let jsonl_path = temp.path().join("issues.jsonl");
        let checks = vec![CheckResult {
            name: "sync.metadata".to_string(),
            status: CheckStatus::Warn,
            message: Some("Database and JSONL have diverged (merge required)".to_string()),
            details: Some(serde_json::json!({
                "pending_import": true,
                "pending_export": true,
                "jsonl_newer": true,
                "db_newer": true,
            })),
        }];

        let classification = classify_doctor_checks(&db_path, &jsonl_path, &checks);

        assert_eq!(classification.health, WorkspaceHealth::Degraded);
        assert!(
            classification
                .anomalies
                .iter()
                .any(|anomaly| matches!(anomaly, AnomalyClass::JsonlNewer)),
            "expected JSONL-newer anomaly: {:?}",
            classification.anomalies
        );
        assert!(
            classification
                .anomalies
                .iter()
                .any(|anomaly| matches!(anomaly, AnomalyClass::DbNewer)),
            "expected DB-newer anomaly: {:?}",
            classification.anomalies
        );
    }

    #[test]
    fn test_classify_doctor_checks_falls_back_to_sync_metadata_message() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let jsonl_path = temp.path().join("issues.jsonl");
        let checks = vec![
            CheckResult {
                name: "sync.metadata".to_string(),
                status: CheckStatus::Warn,
                message: Some("Database and JSONL have diverged (merge required)".to_string()),
                details: None,
            },
            CheckResult {
                name: "sync.metadata".to_string(),
                status: CheckStatus::Warn,
                message: Some(
                    "Local changes exist but no export is recorded; consider running sync --flush-only"
                        .to_string(),
                ),
                details: None,
            },
        ];

        let classification = classify_doctor_checks(&db_path, &jsonl_path, &checks);

        assert_eq!(classification.health, WorkspaceHealth::Degraded);
        assert!(
            classification
                .anomalies
                .iter()
                .any(|anomaly| matches!(anomaly, AnomalyClass::JsonlNewer)),
            "expected JSONL-newer fallback anomaly: {:?}",
            classification.anomalies
        );
        assert!(
            classification
                .anomalies
                .iter()
                .any(|anomaly| matches!(anomaly, AnomalyClass::DbNewer)),
            "expected DB-newer fallback anomaly: {:?}",
            classification.anomalies
        );

        let local_only_checks = vec![CheckResult {
            name: "sync.metadata".to_string(),
            status: CheckStatus::Warn,
            message: Some(
                "Local changes exist but no export is recorded; consider running sync --flush-only"
                    .to_string(),
            ),
            details: None,
        }];
        let local_only = classify_doctor_checks(&db_path, &jsonl_path, &local_only_checks);
        assert!(
            local_only
                .anomalies
                .iter()
                .any(|anomaly| matches!(anomaly, AnomalyClass::DbNewer)),
            "expected no-export fallback to classify DB-newer: {:?}",
            local_only.anomalies
        );
        assert!(
            !local_only
                .anomalies
                .iter()
                .any(|anomaly| matches!(anomaly, AnomalyClass::JsonlNewer)),
            "no-export fallback must not invent JSONL-newer: {:?}",
            local_only.anomalies
        );
    }

    #[test]
    fn test_classify_doctor_checks_preserves_id_delta_total_counts() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let jsonl_path = temp.path().join("issues.jsonl");
        let checks = vec![CheckResult {
            name: "counts.db_vs_jsonl".to_string(),
            status: CheckStatus::Warn,
            message: Some("DB and JSONL counts match but id sets diverge".to_string()),
            details: Some(serde_json::json!({
                "db": 100,
                "jsonl": 100,
                "id_delta": {
                    "only_db_count": 100,
                    "only_jsonl_count": 100,
                    "both_count": 0,
                    "only_db": ["db-1", "db-2"],
                    "only_jsonl": ["jsonl-1"],
                    "preview_limit": 2
                }
            })),
        }];

        let classification = classify_doctor_checks(&db_path, &jsonl_path, &checks);

        assert_eq!(classification.health, WorkspaceHealth::Degraded);
        let Some(AnomalyClass::DbJsonlIdSetMismatch {
            only_db_count,
            only_jsonl_count,
            only_db,
            only_jsonl,
            both_count,
        }) = classification
            .anomalies
            .iter()
            .find(|anomaly| matches!(anomaly, AnomalyClass::DbJsonlIdSetMismatch { .. }))
        else {
            panic!(
                "expected id-set mismatch anomaly: {:?}",
                classification.anomalies
            );
        };
        assert_eq!(*only_db_count, 100);
        assert_eq!(*only_jsonl_count, 100);
        assert_eq!(*both_count, 0);
        assert_eq!(only_db.as_slice(), ["db-1".to_string(), "db-2".to_string()]);
        assert_eq!(only_jsonl.as_slice(), ["jsonl-1".to_string()]);
    }

    #[test]
    fn test_classify_doctor_checks_marks_warn_recoverable_anomalies_degraded() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let jsonl_path = temp.path().join("issues.jsonl");
        let checks = vec![CheckResult {
            name: "db.recoverable_anomalies".to_string(),
            status: CheckStatus::Warn,
            message: Some(BLOCKED_CACHE_STALE_FINDING.to_string()),
            details: Some(serde_json::json!({
                "findings": [
                    BLOCKED_CACHE_STALE_FINDING,
                    BLOCKED_CACHE_CONTENT_MISMATCH_FINDING,
                    READY_PROJECTION_CONTENT_MISMATCH_FINDING,
                ]
            })),
        }];

        let classification = classify_doctor_checks(&db_path, &jsonl_path, &checks);

        assert_eq!(classification.health, WorkspaceHealth::Degraded);
        let anomalies = &classification.anomalies;
        assert!(
            anomalies
                .iter()
                .any(|anomaly| matches!(anomaly, AnomalyClass::BlockedCacheStale)),
            "expected blocked-cache stale anomaly: {:?}",
            anomalies
        );
        assert!(
            anomalies
                .iter()
                .any(|anomaly| matches!(anomaly, AnomalyClass::BlockedCacheContentMismatch)),
            "expected blocked-cache content mismatch anomaly: {:?}",
            anomalies
        );
        assert!(
            anomalies
                .iter()
                .any(|anomaly| matches!(anomaly, AnomalyClass::ReadyProjectionContentMismatch)),
            "expected ready projection mismatch anomaly: {:?}",
            anomalies
        );
    }

    #[test]
    fn test_classify_doctor_checks_preserves_duplicate_finding_details() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let jsonl_path = temp.path().join("issues.jsonl");
        let checks = vec![CheckResult {
            name: "db.recoverable_anomalies".to_string(),
            status: CheckStatus::Error,
            message: Some(
                "sqlite_master contains duplicate table entries for 'blocked_issues_cache' (3 rows)"
                    .to_string(),
            ),
            details: Some(serde_json::json!({
                "findings": [
                    "sqlite_master contains duplicate table entries for 'blocked_issues_cache' (3 rows)",
                    "config contains duplicate rows for key 'issue_prefix' (4 rows)",
                    "metadata contains duplicate rows for key 'project' (5 rows)",
                ]
            })),
        }];

        let classification = classify_doctor_checks(&db_path, &jsonl_path, &checks);

        assert_eq!(classification.health, WorkspaceHealth::Recoverable);
        assert!(
            classification.anomalies.iter().any(|anomaly| {
                matches!(
                    anomaly,
                    AnomalyClass::DuplicateSchemaRows { name, count }
                        if name == "blocked_issues_cache" && *count == 3
                )
            }),
            "expected schema duplicate details: {:?}",
            classification.anomalies
        );
        assert!(
            classification.anomalies.iter().any(|anomaly| {
                matches!(
                    anomaly,
                    AnomalyClass::DuplicateConfigKeys { key, count }
                        if key == "issue_prefix" && *count == 4
                )
            }),
            "expected config duplicate details: {:?}",
            classification.anomalies
        );
        assert!(
            classification.anomalies.iter().any(|anomaly| {
                matches!(
                    anomaly,
                    AnomalyClass::DuplicateMetadataKeys { key, count }
                        if key == "project" && *count == 5
                )
            }),
            "expected metadata duplicate details: {:?}",
            classification.anomalies
        );
    }

    #[test]
    fn test_classify_doctor_checks_preserves_shm_only_sidecar_presence() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let jsonl_path = temp.path().join("issues.jsonl");
        let shm_path = PathBuf::from(format!("{}-shm", db_path.to_string_lossy()));
        let checks = vec![CheckResult {
            name: "db.sidecars".to_string(),
            status: CheckStatus::Error,
            message: Some(format!(
                "SHM sidecar exists without a matching WAL sidecar at {}",
                shm_path.display()
            )),
            details: Some(serde_json::json!({
                "findings": [
                    format!(
                        "SHM sidecar exists without a matching WAL sidecar at {}",
                        shm_path.display()
                    )
                ],
                "quarantine_candidates": [shm_path.display().to_string()],
            })),
        }];

        let classification = classify_doctor_checks(&db_path, &jsonl_path, &checks);

        assert_eq!(classification.health, WorkspaceHealth::Degraded);
        assert!(
            classification.anomalies.iter().any(|anomaly| {
                matches!(
                    anomaly,
                    AnomalyClass::SidecarMismatch {
                        has_wal: false,
                        has_shm: true
                    }
                )
            }),
            "expected SHM-only sidecar anomaly: {:?}",
            classification.anomalies
        );
    }

    #[test]
    fn test_classify_doctor_checks_preserves_shm_only_sidecar_presence_without_details() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let jsonl_path = temp.path().join("issues.jsonl");
        let checks = vec![CheckResult {
            name: "db.sidecars".to_string(),
            status: CheckStatus::Error,
            message: Some("SHM sidecar exists without a matching WAL sidecar".to_string()),
            details: None,
        }];

        let classification = classify_doctor_checks(&db_path, &jsonl_path, &checks);

        assert!(
            classification.anomalies.iter().any(|anomaly| {
                matches!(
                    anomaly,
                    AnomalyClass::SidecarMismatch {
                        has_wal: false,
                        has_shm: true
                    }
                )
            }),
            "expected SHM-only sidecar anomaly: {:?}",
            classification.anomalies
        );
    }

    #[test]
    fn test_local_repair_audit_records_applied_actions_and_artifacts() {
        let repair = LocalRepairResult {
            blocked_cache_rebuilt: true,
            indexes_reindexed: true,
            vacuumed: false,
            quarantined_artifacts: vec![".beads/.br_recovery/beads.db-shm.test".to_string()],
        };

        let audit = local_repair_audit_record(
            "doctor.local_repair",
            "verified",
            &repair,
            Some("post-repair checks passed".to_string()),
        );

        assert_eq!(audit.phase, "doctor.local_repair");
        assert_eq!(audit.action, "local_repair");
        assert_eq!(audit.outcome, "verified");
        assert_eq!(
            audit.applied_actions,
            vec![
                "blocked_cache_rebuilt".to_string(),
                "indexes_reindexed".to_string(),
                "quarantined_artifacts".to_string()
            ]
        );
        assert_eq!(audit.quarantined_artifacts.len(), 1);
        assert_eq!(audit.reason.as_deref(), Some("post-repair checks passed"));
    }

    #[test]
    fn test_jsonl_rebuild_audit_records_import_counts() {
        let repair = DoctorRepairResult {
            imported: 3,
            skipped: 1,
            fk_violations_cleaned: 2,
            preserved_tombstones: 0,
            preserved_dirty_issues: 0,
            preserved_dirty_issue_ids: Vec::new(),
            verified_backups: Vec::new(),
            preserved_history: Vec::new(),
            history_preservation_warnings: Vec::new(),
        };

        let audit =
            jsonl_rebuild_audit_record("doctor.jsonl_rebuild", "verified", Some(&repair), None);

        assert_eq!(audit.action, "jsonl_rebuild");
        assert_eq!(audit.imported, Some(3));
        assert_eq!(audit.skipped, Some(1));
        assert_eq!(audit.fk_violations_cleaned, Some(2));
    }

    #[test]
    fn test_repeated_jsonl_rebuild_refusal_reason_detects_failed_marker() -> Result<()> {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        fs::create_dir_all(&beads_dir)?;

        let recovery_dir = config::recovery_dir_for_db_path(&db_path, &beads_dir);
        fs::create_dir_all(&recovery_dir)?;
        let marker = recovery_dir.join(format!(
            "beads.db.20260421_120000_000000{}",
            JSONL_REBUILD_VERIFICATION_FAILED_SUFFIX
        ));
        fs::write(&marker, b"{\"outcome\":\"verification_failed\"}")?;

        let reason =
            repeated_jsonl_rebuild_refusal_reason(&beads_dir, &db_path, false)?.expect("reason");
        assert!(reason.contains(JSONL_REBUILD_REPEAT_ERROR_PREFIX));
        assert!(reason.contains(&marker.display().to_string()));
        assert!(reason.contains("--allow-repeated-repair"));

        let allowed = repeated_jsonl_rebuild_refusal_reason(&beads_dir, &db_path, true)?;
        assert!(allowed.is_none(), "explicit override should permit retry");
        Ok(())
    }

    #[test]
    fn test_write_jsonl_rebuild_verification_failed_marker_records_failed_checks() -> Result<()> {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        fs::create_dir_all(&beads_dir)?;

        let post_repair = DoctorRun {
            report: DoctorReport {
                ok: false,
                workspace_health: Some("unsafe".to_string()),
                reliability_audit: None,
                checks: vec![
                    CheckResult {
                        name: "sqlite.integrity_check".to_string(),
                        status: CheckStatus::Error,
                        message: Some("database disk image is malformed".to_string()),
                        details: None,
                    },
                    CheckResult {
                        name: "jsonl.parse".to_string(),
                        status: CheckStatus::Ok,
                        message: Some("Parsed 1 records".to_string()),
                        details: None,
                    },
                ],
            },
            jsonl_path: None,
        };
        let repair = DoctorRepairResult {
            imported: 1,
            skipped: 0,
            fk_violations_cleaned: 0,
            preserved_tombstones: 0,
            preserved_dirty_issues: 0,
            preserved_dirty_issue_ids: Vec::new(),
            verified_backups: Vec::new(),
            preserved_history: Vec::new(),
            history_preservation_warnings: Vec::new(),
        };
        let mut session =
            DoctorRepairSession::new(temp.path(), /* dry_run = */ false).expect("session builds");

        let marker = write_jsonl_rebuild_verification_failed_marker(
            &beads_dir,
            &db_path,
            &post_repair,
            &repair,
            &mut session,
        )?;
        assert!(
            marker
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(JSONL_REBUILD_VERIFICATION_FAILED_SUFFIX)),
            "unexpected marker path: {}",
            marker.display()
        );

        let payload: serde_json::Value = serde_json::from_slice(&fs::read(&marker)?)?;
        assert_eq!(payload["outcome"], "verification_failed");
        assert_eq!(payload["workspace_health"], "unsafe");
        assert_eq!(payload["imported"], 1);
        let failed_checks = payload["failed_checks"]
            .as_array()
            .expect("failed check array");
        assert_eq!(failed_checks.len(), 1);
        assert_eq!(failed_checks[0]["name"], "sqlite.integrity_check");

        let evidence = prior_jsonl_rebuild_failure_evidence(&beads_dir, &db_path)?
            .expect("marker should become repeated-repair evidence");
        assert_eq!(evidence.path, marker);

        let actions = fs::read_to_string(&session.run.actions_file)?;
        let action: serde_json::Value = serde_json::from_str(
            actions
                .lines()
                .find(|line| !line.trim().is_empty())
                .expect("actions.jsonl should contain the marker write"),
        )?;
        assert_eq!(action["op"], "write_file");
        assert_eq!(
            action["fixer_id"],
            "doctor.jsonl_rebuild_verification_marker"
        );
        assert!(
            action["path"]
                .as_str()
                .is_some_and(|path| path.starts_with(".beads/.br_recovery/beads.db."))
        );
        Ok(())
    }

    #[test]
    fn test_check_root_gitignore_warns_for_directory_patterns() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::write(
            temp.path().join(".gitignore"),
            ".beads/\n/.beads/*\n!.beads/.gitignore\nkeep-me\n",
        )
        .unwrap();

        let mut checks = Vec::new();
        check_root_gitignore(&beads_dir, &mut checks);

        let check = find_check(&checks, "gitignore.beads_inner").expect("gitignore check");
        assert!(matches!(check.status, CheckStatus::Warn));

        let offending = check
            .details
            .as_ref()
            .and_then(|details| details.get("offending_patterns"))
            .and_then(serde_json::Value::as_array)
            .expect("offending patterns");

        assert_eq!(
            offending,
            &vec![
                serde_json::Value::String(".beads/".to_string()),
                serde_json::Value::String("/.beads/*".to_string()),
            ]
        );
    }

    #[test]
    fn test_fix_root_gitignore_if_warned_removes_all_offending_patterns() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let gitignore_path = temp.path().join(".gitignore");
        fs::write(
            &gitignore_path,
            ".beads/\nkeep-me\n/.beads/.gitignore\n!.beads/.gitignore\n",
        )
        .unwrap();

        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        let _storage = SqliteStorage::open(&db_path).unwrap();
        fs::write(
            &jsonl_path,
            format!(
                "{}\n",
                serde_json::to_string(&sample_issue("bd-test01", "Valid issue")).unwrap()
            ),
        )
        .unwrap();

        let paths = config::ConfigPaths {
            beads_dir: beads_dir.clone(),
            db_path,
            jsonl_path,
            metadata: config::Metadata::default(),
        };

        let report_before = collect_doctor_report(&beads_dir, &paths).expect("doctor report");
        let before_check =
            find_check(&report_before.report.checks, "gitignore.beads_inner").expect("warning");
        assert!(matches!(before_check.status, CheckStatus::Warn));

        let ctx = OutputContext::from_output_format(crate::cli::OutputFormat::Json, false, true);
        assert!(fix_root_gitignore_if_warned(
            &beads_dir,
            &report_before.report,
            &ctx,
            None,
        ));
        assert_eq!(
            fs::read_to_string(&gitignore_path).unwrap(),
            "keep-me\n!.beads/.gitignore\n"
        );

        let report_after = collect_doctor_report(&beads_dir, &paths).expect("doctor report");
        let after_check =
            find_check(&report_after.report.checks, "gitignore.beads_inner").expect("status");
        assert!(matches!(after_check.status, CheckStatus::Ok));
    }

    #[test]
    fn test_doctor_no_db_skips_db_backed_checks_and_reports_no_db_mode() {
        // #329: under `--no-db`, doctor skips database consistency/recovery
        // checks and emits `db.no_db_mode` listing them. The JSONL audit and
        // namespace/pending-merge safety checks still run.
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        // A real DB exists on disk so the DB-backed checks WOULD run if not
        // suppressed; this proves suppression is by the flag, not by absence.
        let _storage = SqliteStorage::open(&db_path).unwrap();
        fs::write(
            &jsonl_path,
            format!(
                "{}\n",
                serde_json::to_string(&sample_issue("bd-test01", "Valid issue")).unwrap()
            ),
        )
        .unwrap();

        let paths = config::ConfigPaths {
            beads_dir: beads_dir.clone(),
            db_path,
            jsonl_path,
            metadata: config::Metadata::default(),
        };

        // no_db = true -> DB-backed checks skipped.
        let run = collect_doctor_report_with_mode_and_db_override(
            &beads_dir,
            &paths,
            None,
            DoctorInspectionMode::Full,
            true,
        )
        .expect("doctor report");
        let checks = &run.report.checks;

        let marker = find_check(checks, "db.no_db_mode").expect("db.no_db_mode present");
        assert_eq!(marker.status, CheckStatus::Ok);
        let skipped: Vec<String> = marker
            .details
            .as_ref()
            .and_then(|d| d.get("skipped_checks"))
            .and_then(serde_json::Value::as_array)
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert!(skipped.contains(&"db.inspect".to_string()), "{skipped:?}");
        assert!(
            skipped.contains(&"db.bloat_vs_jsonl".to_string()),
            "{skipped:?}"
        );

        // None of the DB-backed checks may appear.
        for db_check in [
            "db.exists",
            "db.sidecars",
            "db.recovery_artifacts",
            "schema.tables",
            "counts.db_vs_jsonl",
            "permissions.recovery_dir",
        ] {
            assert!(
                find_check(checks, db_check).is_none(),
                "DB-backed check `{db_check}` must be skipped under --no-db: {:?}",
                checks.iter().map(|c| &c.name).collect::<Vec<_>>()
            );
        }

        // The JSONL-side audit still runs.
        assert!(
            find_check(checks, "jsonl.parse").is_some(),
            "JSONL audit must still run under --no-db"
        );

        // Contrast: with no_db = false, the DB-backed checks DO run.
        let run_db = collect_doctor_report_with_mode_and_db_override(
            &beads_dir,
            &paths,
            None,
            DoctorInspectionMode::Full,
            false,
        )
        .expect("doctor report with db");
        assert!(find_check(&run_db.report.checks, "db.no_db_mode").is_none());
        assert!(find_check(&run_db.report.checks, "db.sidecars").is_some());
    }

    #[test]
    fn test_check_base_jsonl_missing_is_ok() {
        // Missing on fresh clone is legitimate; report Ok.
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let mut checks = Vec::new();
        check_base_jsonl(&beads_dir, &mut checks);
        let check = find_check(&checks, "base_jsonl").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok), "{check:?}");
    }

    #[test]
    fn test_check_base_jsonl_stale_anchor_warns() {
        // base.jsonl mtime < live JSONL mtime → warn stale.
        // Use std::fs::FileTimes to deterministically backdate the base
        // anchor rather than relying on real-time sleeps (which are
        // flaky on coarse-mtime filesystems).
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let base = beads_dir.join("beads.base.jsonl");
        let live = beads_dir.join("issues.jsonl");
        fs::write(&base, b"{\"id\":\"bd-old\"}\n").unwrap();
        fs::write(&live, b"{\"id\":\"bd-new\"}\n").unwrap();

        let two_hours_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        let times = std::fs::FileTimes::new()
            .set_accessed(two_hours_ago)
            .set_modified(two_hours_ago);
        let base_file = std::fs::OpenOptions::new().write(true).open(&base).unwrap();
        base_file.set_times(times).unwrap();
        drop(base_file);

        let mut checks = Vec::new();
        check_base_jsonl(&beads_dir, &mut checks);
        let check = find_check(&checks, "base_jsonl").expect("check present");
        assert!(
            matches!(check.status, CheckStatus::Warn),
            "{check:?} should be Warn for stale anchor"
        );
        let kind = check
            .details
            .as_ref()
            .and_then(|d| d.get("kind"))
            .and_then(|v| v.as_str());
        assert_eq!(kind, Some("stale"));
    }

    #[cfg(unix)]
    #[test]
    fn test_check_base_jsonl_symlink_warns() {
        // Symlinked anchor → warn (security risk per 401c0495).
        use std::os::unix::fs::symlink;
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let outside = temp.path().join("outside.jsonl");
        fs::write(&outside, b"{\"id\":\"bd-outside\"}\n").unwrap();
        symlink(&outside, beads_dir.join("beads.base.jsonl")).unwrap();

        let mut checks = Vec::new();
        check_base_jsonl(&beads_dir, &mut checks);
        let check = find_check(&checks, "base_jsonl").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn));
        let kind = check
            .details
            .as_ref()
            .and_then(|d| d.get("kind"))
            .and_then(|v| v.as_str());
        assert_eq!(kind, Some("symlink"));
    }

    #[cfg(unix)]
    #[test]
    fn test_fix_base_jsonl_symlink_refuses_out_of_scope_target() {
        // Pass-5 cycle 5: the fixer (via the chokepoint's canonicalized
        // write_scope check) MUST refuse to follow a symlink whose
        // target resolves outside `.beads/` or `.doctor/`. This is the
        // safety behavior that protects operators against an attacker
        // shape where the merge anchor points at an arbitrary file.
        // The detector flags the symlink; the fixer correctly declines
        // to quarantine it (operator must remove the symlink manually).
        use std::os::unix::fs::symlink;
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let outside = temp.path().join("outside.jsonl");
        fs::write(&outside, b"{\"id\":\"bd-outside\"}\n").unwrap();
        let symlink_path = beads_dir.join("beads.base.jsonl");
        symlink(&outside, &symlink_path).unwrap();
        fs::write(beads_dir.join("issues.jsonl"), b"{}\n").unwrap();

        let mut report = DoctorReport {
            ok: false,
            workspace_health: Some("degraded".to_string()),
            reliability_audit: None,
            checks: Vec::new(),
        };
        check_base_jsonl(&beads_dir, &mut report.checks);
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name == "base_jsonl" && matches!(c.status, CheckStatus::Warn))
        );

        let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
            .expect("session must build");
        let ctx = OutputContext::from_output_format(crate::cli::OutputFormat::Text, false, true);
        // Out-of-scope symlink target → fixer returns false (chokepoint
        // refuses per safety) AND the symlink remains at its original
        // path (no partial mutation).
        let result =
            fix_base_jsonl_symlink_if_warned(&beads_dir, &report, &ctx, Some(&mut session));
        assert!(
            !result,
            "fixer must refuse when symlink target is outside write_scopes"
        );
        assert!(
            fs::symlink_metadata(&symlink_path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "symlink must remain in place when refused"
        );
        assert_eq!(
            fs::read(&outside).unwrap(),
            b"{\"id\":\"bd-outside\"}\n",
            "symlink target's bytes must not be modified"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_fix_base_jsonl_symlink_quarantines_in_scope_target() {
        // Pass-5 cycle 5: when the symlink points at a file INSIDE the
        // workspace (e.g., a sibling JSONL), the chokepoint allows the
        // rename and the fixer quarantines the symlink.
        use std::os::unix::fs::symlink;
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        // In-scope target lives under .beads/
        let inside_target = beads_dir.join("sibling.jsonl");
        fs::write(&inside_target, b"{\"id\":\"bd-sibling\"}\n").unwrap();
        let symlink_path = beads_dir.join("beads.base.jsonl");
        symlink(&inside_target, &symlink_path).unwrap();
        fs::write(beads_dir.join("issues.jsonl"), b"{}\n").unwrap();

        let mut report = DoctorReport {
            ok: false,
            workspace_health: Some("degraded".to_string()),
            reliability_audit: None,
            checks: Vec::new(),
        };
        check_base_jsonl(&beads_dir, &mut report.checks);
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name == "base_jsonl" && matches!(c.status, CheckStatus::Warn))
        );

        let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
            .expect("session must build");
        let ctx = OutputContext::from_output_format(crate::cli::OutputFormat::Text, false, true);
        assert!(fix_base_jsonl_symlink_if_warned(
            &beads_dir,
            &report,
            &ctx,
            Some(&mut session),
        ));

        // Source symlink gone, quarantine populated.
        assert!(
            !symlink_path.exists() && fs::symlink_metadata(&symlink_path).is_err(),
            "source symlink should be moved out of .beads/"
        );
        let q = session.run.root.join("quarantine/.beads/beads.base.jsonl");
        assert!(
            q.exists(),
            "quarantine should hold the moved symlink content at {q:?}"
        );
        // Sibling target is untouched.
        assert_eq!(
            fs::read(&inside_target).unwrap(),
            b"{\"id\":\"bd-sibling\"}\n",
            "symlink target's bytes must not be modified"
        );

        let actions = fs::read_to_string(&session.run.actions_file).unwrap();
        let rename_count = actions
            .lines()
            .filter(|l| l.contains("\"op\":\"rename\""))
            .count();
        assert_eq!(rename_count, 1, "actions.jsonl: {actions}");
    }

    #[test]
    fn test_fix_base_jsonl_stale_regen_writes_live_bytes() {
        // Pass-5 cycle 6: stale anchor (older mtime, regular file)
        // gets regenerated from current JSONL bytes via Op::WriteFile.
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let anchor = beads_dir.join("beads.base.jsonl");
        let live = beads_dir.join("issues.jsonl");
        fs::write(&anchor, b"{\"id\":\"bd-old\"}\n").unwrap();
        fs::write(&live, b"{\"id\":\"bd-new-1\"}\n{\"id\":\"bd-new-2\"}\n").unwrap();

        // Backdate the anchor so the detector flags it stale.
        let two_hours_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        let anchor_file = std::fs::OpenOptions::new()
            .write(true)
            .open(&anchor)
            .unwrap();
        anchor_file
            .set_times(std::fs::FileTimes::new().set_modified(two_hours_ago))
            .unwrap();
        drop(anchor_file);

        let mut report = DoctorReport {
            ok: false,
            workspace_health: Some("degraded".to_string()),
            reliability_audit: None,
            checks: Vec::new(),
        };
        check_base_jsonl(&beads_dir, &mut report.checks);
        let check = find_check(&report.checks, "base_jsonl").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn));
        let kind = check
            .details
            .as_ref()
            .and_then(|d| d.get("kind"))
            .and_then(|v| v.as_str());
        assert_eq!(kind, Some("stale"));

        let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
            .expect("session must build");
        let ctx = OutputContext::from_output_format(crate::cli::OutputFormat::Text, false, true);
        assert!(fix_base_jsonl_stale_if_warned(
            &beads_dir,
            &report,
            &ctx,
            Some(&mut session),
        ));

        // Anchor now contains the live JSONL bytes.
        assert_eq!(
            fs::read(&anchor).unwrap(),
            fs::read(&live).unwrap(),
            "regenerated anchor must equal live JSONL bytes"
        );

        // actions.jsonl records one write_file op.
        let actions = fs::read_to_string(&session.run.actions_file).unwrap();
        let write_count = actions
            .lines()
            .filter(|l| l.contains("\"op\":\"write_file\""))
            .count();
        assert_eq!(write_count, 1, "actions.jsonl: {actions}");
    }

    #[test]
    fn test_fix_base_jsonl_stale_skips_byte_identical_anchor() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let anchor = beads_dir.join("beads.base.jsonl");
        let live = beads_dir.join("issues.jsonl");
        let content = b"{\"id\":\"bd-same\"}\n";
        fs::write(&anchor, content).unwrap();
        fs::write(&live, content).unwrap();

        let two_hours_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(7200);
        let anchor_file = std::fs::OpenOptions::new()
            .write(true)
            .open(&anchor)
            .unwrap();
        anchor_file
            .set_times(std::fs::FileTimes::new().set_modified(two_hours_ago))
            .unwrap();
        drop(anchor_file);

        let mut report = DoctorReport {
            ok: false,
            workspace_health: Some("degraded".to_string()),
            reliability_audit: None,
            checks: Vec::new(),
        };
        check_base_jsonl(&beads_dir, &mut report.checks);
        let check = find_check(&report.checks, "base_jsonl").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn));

        let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
            .expect("session must build");
        let ctx = OutputContext::from_output_format(crate::cli::OutputFormat::Text, false, true);
        assert!(!fix_base_jsonl_stale_if_warned(
            &beads_dir,
            &report,
            &ctx,
            Some(&mut session),
        ));

        assert_eq!(fs::read(&anchor).unwrap(), content);
        let actions = fs::read_to_string(&session.run.actions_file).unwrap();
        assert!(
            actions.is_empty(),
            "identical anchor must be a no-op: {actions}"
        );
    }

    #[test]
    fn test_fix_base_jsonl_stale_refuses_empty_live_jsonl() {
        // TOCTOU defense: never regenerate from an empty live JSONL
        // (would silently truncate the anchor).
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let anchor = beads_dir.join("beads.base.jsonl");
        fs::write(&anchor, b"{\"id\":\"bd-anchor\"}\n").unwrap();
        // Live JSONL is empty (could happen mid-restore).
        fs::write(beads_dir.join("issues.jsonl"), b"").unwrap();

        // Synthesize a stale-finding report so the fixer is invoked.
        let mut report = DoctorReport {
            ok: false,
            workspace_health: Some("degraded".to_string()),
            reliability_audit: None,
            checks: Vec::new(),
        };
        push_check(
            &mut report.checks,
            "base_jsonl",
            CheckStatus::Warn,
            Some("synthetic stale".to_string()),
            Some(serde_json::json!({"kind": "stale"})),
        );

        let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
            .expect("session must build");
        let ctx = OutputContext::from_output_format(crate::cli::OutputFormat::Text, false, true);
        assert!(!fix_base_jsonl_stale_if_warned(
            &beads_dir,
            &report,
            &ctx,
            Some(&mut session),
        ));
        // Anchor preserved.
        assert_eq!(fs::read(&anchor).unwrap(), b"{\"id\":\"bd-anchor\"}\n");
    }

    #[test]
    fn test_check_base_jsonl_missing_post_flush_warns_after_export() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        storage
            .set_metadata(
                crate::sync::METADATA_LAST_EXPORT_TIME,
                "2026-05-01T00:00:00Z",
            )
            .unwrap();
        drop(storage);

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let mut checks = Vec::new();
        check_base_jsonl_missing_post_flush(&conn, &beads_dir, None, &mut checks);

        let check = find_check(&checks, "base_jsonl.missing_post_flush").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn));
        assert_eq!(
            check
                .details
                .as_ref()
                .and_then(|details| details.get("kind"))
                .and_then(serde_json::Value::as_str),
            Some("missing_post_flush")
        );
    }

    #[test]
    fn test_check_base_jsonl_missing_post_flush_allows_fresh_missing_anchor() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");
        let _storage = SqliteStorage::open(&db_path).unwrap();

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let mut checks = Vec::new();
        check_base_jsonl_missing_post_flush(&conn, &beads_dir, None, &mut checks);

        let check = find_check(&checks, "base_jsonl.missing_post_flush").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    #[test]
    fn test_check_base_jsonl_missing_post_flush_ok_when_verifiably_in_sync() {
        // Issue #378: a flush-only workspace (anchor never produced) whose DB
        // and JSONL are verifiably in sync must NOT warn — `br sync --status`
        // reports "In sync" from the same evidence, and a contradictory
        // doctor warning is exactly the reported inconsistency.
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::write(&jsonl_path, b"{\"id\":\"bd-1\"}\n").unwrap();
        let computed_hash = crate::sync::compute_jsonl_hash(&jsonl_path).unwrap();

        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        storage
            .set_metadata(
                crate::sync::METADATA_LAST_EXPORT_TIME,
                "2026-05-01T00:00:00Z",
            )
            .unwrap();
        storage
            .set_metadata(crate::sync::METADATA_JSONL_CONTENT_HASH, &computed_hash)
            .unwrap();
        drop(storage);

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let mut checks = Vec::new();
        check_base_jsonl_missing_post_flush(&conn, &beads_dir, Some(&jsonl_path), &mut checks);

        let check = find_check(&checks, "base_jsonl.missing_post_flush").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok), "{check:?}");
        assert_eq!(
            check
                .details
                .as_ref()
                .and_then(|details| details.get("kind"))
                .and_then(serde_json::Value::as_str),
            Some("missing_but_in_sync")
        );
    }

    #[test]
    fn test_check_base_jsonl_missing_post_flush_warns_when_hash_diverged() {
        // Issue #378: the warning must survive for the genuinely suspicious
        // shape — export metadata present but the live JSONL no longer
        // matches the cached content hash.
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::write(&jsonl_path, b"{\"id\":\"bd-1\"}\n").unwrap();

        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        storage
            .set_metadata(
                crate::sync::METADATA_LAST_EXPORT_TIME,
                "2026-05-01T00:00:00Z",
            )
            .unwrap();
        storage
            .set_metadata(crate::sync::METADATA_JSONL_CONTENT_HASH, "stale-hash")
            .unwrap();
        drop(storage);

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let mut checks = Vec::new();
        check_base_jsonl_missing_post_flush(&conn, &beads_dir, Some(&jsonl_path), &mut checks);

        let check = find_check(&checks, "base_jsonl.missing_post_flush").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
        assert!(
            check
                .message
                .as_deref()
                .is_some_and(|m| m.contains("br sync --flush-only")),
            "warning should name the recovery command: {check:?}"
        );
    }

    #[test]
    fn test_check_dirty_bitmap_clean_workspace_ok() {
        // No orphan rows in dirty_issues → check returns Ok.
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let _storage = SqliteStorage::open(&db_path).unwrap();
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let mut checks = Vec::new();
        check_dirty_bitmap_divergence(&conn, &mut checks);
        let check = find_check(&checks, "dirty_bitmap").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok), "{check:?}");
    }

    #[test]
    fn test_check_dirty_bitmap_orphan_row_warns() {
        // Insert an orphan into dirty_issues with FK guard disabled,
        // then assert the detector finds it and reports kind details.
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let _storage = SqliteStorage::open(&db_path).unwrap();
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        // Disable FK enforcement so the orphan insert succeeds.
        let _ = conn.execute("PRAGMA foreign_keys = OFF");
        conn.execute_with_params(
            "INSERT INTO dirty_issues(issue_id, marked_at) VALUES (?1, ?2)",
            &[
                SqliteValue::Text("bd-orphan-1".into()),
                SqliteValue::Text("2026-05-14T00:00:00Z".into()),
            ],
        )
        .unwrap();

        let mut checks = Vec::new();
        check_dirty_bitmap_divergence(&conn, &mut checks);
        let check = find_check(&checks, "dirty_bitmap").expect("check present");
        assert!(
            matches!(check.status, CheckStatus::Warn),
            "{check:?} should be Warn"
        );
        let orphan_count = check
            .details
            .as_ref()
            .and_then(|d| d.get("orphan_count"))
            .and_then(serde_json::Value::as_i64);
        assert_eq!(orphan_count, Some(1));
        let sample = check
            .details
            .as_ref()
            .and_then(|d| d.get("sample_issue_ids"))
            .and_then(|v| v.as_array())
            .map(Vec::len)
            .unwrap_or(0);
        assert_eq!(sample, 1);
    }

    #[test]
    fn test_check_dependencies_orphans_warns_on_orphan_row() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let _storage = SqliteStorage::open(&db_path).unwrap();
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let _ = conn.execute("PRAGMA foreign_keys = OFF");
        conn.execute_with_params(
            "INSERT INTO dependencies(issue_id, depends_on_id, type) VALUES (?1, ?2, ?3)",
            &[
                SqliteValue::Text("bd-orphan-d".into()),
                SqliteValue::Text("bd-other".into()),
                SqliteValue::Text("blocks".into()),
            ],
        )
        .unwrap();

        let mut checks = Vec::new();
        check_dependencies_orphans(&conn, &mut checks);
        let check = find_check(&checks, "dependencies.orphans").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
        let count = check
            .details
            .as_ref()
            .and_then(|d| d.get("orphan_count"))
            .and_then(serde_json::Value::as_i64);
        assert_eq!(count, Some(1));
    }

    #[test]
    fn test_check_dependencies_orphans_warns_on_missing_local_target() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        storage
            .create_issue(
                &sample_issue("bd-owner-d", "Dependency owner with missing target"),
                "tester",
            )
            .unwrap();
        drop(storage);
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        conn.execute_with_params(
            "INSERT INTO dependencies(issue_id, depends_on_id, type) VALUES (?1, ?2, ?3)",
            &[
                SqliteValue::Text("bd-owner-d".into()),
                SqliteValue::Text("bd-missing-local-target".into()),
                SqliteValue::Text("blocks".into()),
            ],
        )
        .unwrap();

        let mut checks = Vec::new();
        check_dependencies_orphans(&conn, &mut checks);
        let check = find_check(&checks, "dependencies.orphans").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
        let count = check
            .details
            .as_ref()
            .and_then(|d| d.get("orphan_count"))
            .and_then(serde_json::Value::as_i64);
        assert_eq!(count, Some(1));
        let sample_edges = check
            .details
            .as_ref()
            .and_then(|d| d.get("sample_edges"))
            .and_then(serde_json::Value::as_array)
            .expect("sample_edges array");
        assert!(
            sample_edges
                .iter()
                .any(|edge| edge.as_str() == Some("bd-owner-d -> bd-missing-local-target")),
            "missing local target should be sampled: {sample_edges:?}"
        );
    }

    #[test]
    fn test_check_dependencies_orphans_clean_workspace_ok() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        storage
            .create_issue(
                &sample_issue("bd-local-d", "Local dependency owner"),
                "tester",
            )
            .unwrap();
        drop(storage);
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        conn.execute_with_params(
            "INSERT INTO dependencies(issue_id, depends_on_id, type) VALUES (?1, ?2, ?3)",
            &[
                SqliteValue::Text("bd-local-d".into()),
                SqliteValue::Text("external:upstream-1".into()),
                SqliteValue::Text("blocks".into()),
            ],
        )
        .unwrap();
        let mut checks = Vec::new();
        check_dependencies_orphans(&conn, &mut checks);
        let check = find_check(&checks, "dependencies.orphans").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    /// issue #311: write a strict workflow policy and a non-conforming issue;
    /// the detector must Warn and name the offender.
    #[test]
    fn test_check_workflow_statuses_warns_on_out_of_set_status() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path();
        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        // Conforming issue.
        storage
            .create_issue(&sample_issue("bd-ok", "Open issue"), "tester")
            .unwrap();
        // Non-conforming issue: custom status outside the allowed set.
        let mut bad = sample_issue("bd-bad", "Has bogus status");
        bad.status = Status::Custom("completed".to_string());
        storage.create_issue(&bad, "tester").unwrap();
        drop(storage);

        fs::write(
            beads_dir.join(crate::close_policy::POLICY_FILE_NAME),
            "workflow:\n  strict: true\n  statuses: [\"open\", \"in_progress\", \"closed\"]\n",
        )
        .unwrap();

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let mut checks = Vec::new();
        check_workflow_statuses(&conn, beads_dir, &mut checks);
        let check = find_check(&checks, "policy.workflow_statuses").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
        let offenders = check
            .details
            .as_ref()
            .and_then(|d| d.get("offenders"))
            .and_then(serde_json::Value::as_array)
            .expect("offenders array");
        assert_eq!(offenders.len(), 1);
        assert_eq!(
            offenders[0].get("bead_id").and_then(|v| v.as_str()),
            Some("bd-bad")
        );
        assert_eq!(
            offenders[0].get("status").and_then(|v| v.as_str()),
            Some("completed")
        );
    }

    /// issue #311: when every issue conforms, the detector reports Ok.
    #[test]
    fn test_check_workflow_statuses_ok_when_all_conform() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path();
        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        storage
            .create_issue(&sample_issue("bd-ok-1", "Open issue"), "tester")
            .unwrap();
        drop(storage);

        fs::write(
            beads_dir.join(crate::close_policy::POLICY_FILE_NAME),
            "workflow:\n  strict: true\n  statuses: [\"open\", \"closed\"]\n",
        )
        .unwrap();

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let mut checks = Vec::new();
        check_workflow_statuses(&conn, beads_dir, &mut checks);
        let check = find_check(&checks, "policy.workflow_statuses").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok), "{check:?}");
    }

    /// issue #311: with no workflow policy configured the detector stays
    /// silent — it emits no check at all, so repos without the policy see no
    /// new doctor output.
    #[test]
    fn test_check_workflow_statuses_silent_without_policy() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path();
        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let mut bad = sample_issue("bd-x", "Weird status");
        bad.status = Status::Custom("completed".to_string());
        storage.create_issue(&bad, "tester").unwrap();
        drop(storage);

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let mut checks = Vec::new();
        check_workflow_statuses(&conn, beads_dir, &mut checks);
        assert!(
            find_check(&checks, "policy.workflow_statuses").is_none(),
            "no check should be emitted without a strict workflow policy"
        );
    }

    #[test]
    fn test_fix_dependencies_orphans_prunes_via_chokepoint() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");
        seed_dependency_orphan_repair_fixture(&db_path);

        let report = dependency_orphan_report(&db_path);
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name == "dependencies.orphans" && matches!(c.status, CheckStatus::Warn))
        );

        let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
            .expect("session must build");
        let ctx = OutputContext::from_output_format(crate::cli::OutputFormat::Text, false, true);
        assert!(fix_dependencies_orphans_if_warned(
            &db_path,
            &report,
            &ctx,
            Some(&mut session),
        ));

        let mut after = Vec::new();
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        check_dependencies_orphans(&conn, &mut after);
        let check = find_check(&after, "dependencies.orphans").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok), "{check:?}");
        assert_eq!(
            dependency_row_count(&conn, "bd-keep-d", "external:upstream-1"),
            1,
            "repair must preserve missing/external depends_on_id rows"
        );
        assert_eq!(
            dependency_row_count(&conn, "bd-valid-d", "bd-target-d"),
            1,
            "repair must preserve valid local dependency rows"
        );
        assert_eq!(
            dependency_row_count(&conn, "bd-owner-d", "bd-missing-local-target"),
            0,
            "repair must prune non-external missing depends_on_id rows"
        );
    }

    #[test]
    fn test_check_labels_orphans_warns_on_orphan_row() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let _storage = SqliteStorage::open(&db_path).unwrap();
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let _ = conn.execute("PRAGMA foreign_keys = OFF");
        conn.execute_with_params(
            "INSERT INTO labels(issue_id, label) VALUES (?1, ?2)",
            &[
                SqliteValue::Text("bd-orphan-l".into()),
                SqliteValue::Text("doc".into()),
            ],
        )
        .unwrap();

        let mut checks = Vec::new();
        check_labels_orphans(&conn, &mut checks);
        let check = find_check(&checks, "labels.orphans").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
        let count = check
            .details
            .as_ref()
            .and_then(|d| d.get("orphan_count"))
            .and_then(serde_json::Value::as_i64);
        assert_eq!(count, Some(1));
    }

    #[test]
    fn test_check_labels_orphans_clean_workspace_ok() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let _storage = SqliteStorage::open(&db_path).unwrap();
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let mut checks = Vec::new();
        check_labels_orphans(&conn, &mut checks);
        let check = find_check(&checks, "labels.orphans").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    #[test]
    fn test_fix_labels_orphans_prunes_via_chokepoint() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");
        let _storage = SqliteStorage::open(&db_path).unwrap();
        {
            let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
            let _ = conn.execute("PRAGMA foreign_keys = OFF");
            conn.execute_with_params(
                "INSERT INTO labels(issue_id, label) VALUES (?1, ?2)",
                &[
                    SqliteValue::Text("bd-orphan-fix-l".into()),
                    SqliteValue::Text("doc".into()),
                ],
            )
            .unwrap();
        }

        let mut report = DoctorReport {
            ok: false,
            workspace_health: Some("degraded".to_string()),
            reliability_audit: None,
            checks: Vec::new(),
        };
        {
            let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
            check_labels_orphans(&conn, &mut report.checks);
        }
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name == "labels.orphans" && matches!(c.status, CheckStatus::Warn))
        );

        let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
            .expect("session must build");
        let ctx = OutputContext::from_output_format(crate::cli::OutputFormat::Text, false, true);
        assert!(fix_labels_orphans_if_warned(
            &db_path,
            &report,
            &ctx,
            Some(&mut session),
        ));

        let mut after = Vec::new();
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        check_labels_orphans(&conn, &mut after);
        let check = find_check(&after, "labels.orphans").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok), "{check:?}");
    }

    #[test]
    fn test_check_comments_orphans_warns_on_orphan_row() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let _storage = SqliteStorage::open(&db_path).unwrap();
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let _ = conn.execute("PRAGMA foreign_keys = OFF");
        conn.execute_with_params(
            "INSERT INTO comments(issue_id, text, created_at, author) VALUES (?1, ?2, ?3, ?4)",
            &[
                SqliteValue::Text("bd-orphan-c".into()),
                SqliteValue::Text("orphan body".into()),
                SqliteValue::Text("2026-05-15T00:00:00Z".into()),
                SqliteValue::Text("ghost".into()),
            ],
        )
        .unwrap();

        let mut checks = Vec::new();
        check_comments_orphans(&conn, &mut checks);
        let check = find_check(&checks, "comments.orphans").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
        let count = check
            .details
            .as_ref()
            .and_then(|d| d.get("orphan_count"))
            .and_then(serde_json::Value::as_i64);
        assert_eq!(count, Some(1));
    }

    #[test]
    fn test_check_comments_orphans_clean_workspace_ok() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let _storage = SqliteStorage::open(&db_path).unwrap();
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let mut checks = Vec::new();
        check_comments_orphans(&conn, &mut checks);
        let check = find_check(&checks, "comments.orphans").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    #[test]
    fn test_fix_comments_orphans_prunes_via_chokepoint() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");
        let _storage = SqliteStorage::open(&db_path).unwrap();
        {
            let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
            let _ = conn.execute("PRAGMA foreign_keys = OFF");
            conn.execute_with_params(
                "INSERT INTO comments(issue_id, text, created_at, author) VALUES (?1, ?2, ?3, ?4)",
                &[
                    SqliteValue::Text("bd-orphan-fix".into()),
                    SqliteValue::Text("orphan body".into()),
                    SqliteValue::Text("2026-05-15T00:00:00Z".into()),
                    SqliteValue::Text("ghost".into()),
                ],
            )
            .unwrap();
        }

        let mut report = DoctorReport {
            ok: false,
            workspace_health: Some("degraded".to_string()),
            reliability_audit: None,
            checks: Vec::new(),
        };
        {
            let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
            check_comments_orphans(&conn, &mut report.checks);
        }
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name == "comments.orphans" && matches!(c.status, CheckStatus::Warn))
        );

        let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
            .expect("session must build");
        let ctx = OutputContext::from_output_format(crate::cli::OutputFormat::Text, false, true);
        assert!(fix_comments_orphans_if_warned(
            &db_path,
            &report,
            &ctx,
            Some(&mut session),
        ));

        let mut after = Vec::new();
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        check_comments_orphans(&conn, &mut after);
        let check = find_check(&after, "comments.orphans").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok), "{check:?}");
    }

    #[test]
    fn test_fix_null_defaults_backfills_via_chokepoint() {
        // The canonical events.actor is `TEXT NOT NULL DEFAULT ''`, and fsqlite
        // enforces NOT NULL at INSERT — so to reproduce the detector's intended
        // target (rows predating a NOT NULL DEFAULT addition during schema
        // migration, where the storage retained pre-existing NULLs in older
        // rows) we DROP and recreate `events` locally without NOT NULL on
        // `actor`. The detector and fixer only touch that column, so a minimal
        // column set suffices. No FKs reference events.id, so the drop is safe.
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");
        let storage = SqliteStorage::open(&db_path).unwrap();
        drop(storage);
        {
            let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
            conn.execute("DROP TABLE events").unwrap();
            conn.execute(
                "CREATE TABLE events (
                    id INTEGER PRIMARY KEY,
                    issue_id TEXT NOT NULL,
                    event_type TEXT NOT NULL DEFAULT '',
                    actor TEXT,
                    created_at TEXT NOT NULL DEFAULT '',
                    metadata TEXT
                )",
            )
            .unwrap();
            conn.execute_with_params(
                "INSERT INTO events(issue_id, event_type, actor, created_at) VALUES (?1, ?2, ?3, ?4)",
                &[
                    SqliteValue::Text("bd-null-fix".into()),
                    SqliteValue::Text("created".into()),
                    SqliteValue::Null,
                    SqliteValue::Text("2026-05-15T00:00:00Z".into()),
                ],
            )
            .unwrap();
        }

        let mut report = DoctorReport {
            ok: false,
            workspace_health: Some("degraded".to_string()),
            reliability_audit: None,
            checks: Vec::new(),
        };
        {
            let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
            check_null_defaults(&conn, &mut report.checks);
        }
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name == "db.null_defaults" && matches!(c.status, CheckStatus::Warn)),
            "precondition: detector must warn on the injected NULL"
        );

        let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
            .expect("session must build");
        let ctx = OutputContext::from_output_format(crate::cli::OutputFormat::Text, false, true);
        assert!(fix_null_defaults_if_warned(
            &db_path,
            &report,
            &ctx,
            Some(&mut session),
        ));

        // Detector now clean, and the NULL was backfilled to the schema default ''.
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let mut after = Vec::new();
        check_null_defaults(&conn, &mut after);
        let check = find_check(&after, "db.null_defaults").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok), "{check:?}");
        let rows = conn
            .query("SELECT actor FROM events WHERE issue_id = 'bd-null-fix'")
            .unwrap();
        let actor = rows.first().and_then(|row| row.values().first().cloned());
        assert!(
            matches!(actor, Some(SqliteValue::Text(ref s)) if s.is_empty()),
            "actor should be backfilled to '': {actor:?}"
        );
    }

    #[test]
    fn test_fix_dirty_bitmap_orphans_prunes_via_chokepoint() {
        // Insert an orphan dirty_issues row (FK off), then assert the
        // fixer prunes it through the chokepoint and the post-fix
        // detector reports clean.
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");
        let _storage = SqliteStorage::open(&db_path).unwrap();
        {
            let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
            let _ = conn.execute("PRAGMA foreign_keys = OFF");
            conn.execute_with_params(
                "INSERT INTO dirty_issues(issue_id, marked_at) VALUES (?1, ?2)",
                &[
                    SqliteValue::Text("bd-orphan-fix".into()),
                    SqliteValue::Text("2026-05-15T00:00:00Z".into()),
                ],
            )
            .unwrap();
        }

        let mut report = DoctorReport {
            ok: false,
            workspace_health: Some("degraded".to_string()),
            reliability_audit: None,
            checks: Vec::new(),
        };
        {
            let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
            check_dirty_bitmap_divergence(&conn, &mut report.checks);
        }
        let warned = report
            .checks
            .iter()
            .any(|c| c.name == "dirty_bitmap" && matches!(c.status, CheckStatus::Warn));
        assert!(warned, "precondition: detector must warn");

        let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
            .expect("session must build");
        let ctx = OutputContext::from_output_format(crate::cli::OutputFormat::Text, false, true);
        assert!(fix_dirty_bitmap_orphans_if_warned(
            &db_path,
            &report,
            &ctx,
            Some(&mut session),
        ));

        // Re-run the detector against the same DB; it must now be clean.
        let mut after = Vec::new();
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        check_dirty_bitmap_divergence(&conn, &mut after);
        let check = find_check(&after, "dirty_bitmap").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok), "{check:?}");
    }

    #[test]
    fn test_fix_dirty_bitmap_orphans_prunes_null_issue_id() {
        // SQLite permits NULL in a TEXT PRIMARY KEY on legacy tables.
        // The detector counts that as an orphan via LEFT JOIN; the
        // fixer must use NOT EXISTS rather than NOT IN so the same row
        // is actually pruned.
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");
        let _storage = SqliteStorage::open(&db_path).unwrap();
        {
            let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
            let _ = conn.execute("PRAGMA foreign_keys = OFF");
            conn.execute_with_params(
                "INSERT INTO dirty_issues(issue_id, marked_at) VALUES (?1, ?2)",
                &[
                    SqliteValue::Null,
                    SqliteValue::Text("2026-05-15T00:00:00Z".into()),
                ],
            )
            .unwrap();
        }

        let mut report = DoctorReport {
            ok: false,
            workspace_health: Some("degraded".to_string()),
            reliability_audit: None,
            checks: Vec::new(),
        };
        {
            let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
            check_dirty_bitmap_divergence(&conn, &mut report.checks);
        }
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name == "dirty_bitmap" && matches!(c.status, CheckStatus::Warn)),
            "precondition: NULL dirty_issues issue_id must warn"
        );

        let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
            .expect("session must build");
        assert!(fix_dirty_bitmap_orphans_if_warned(
            &db_path,
            &report,
            &quiet_ctx(),
            Some(&mut session),
        ));

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let count = conn
            .query_row("SELECT COUNT(*) FROM dirty_issues")
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap_or(-1);
        assert_eq!(count, 0, "NULL issue_id orphan should be pruned");
    }

    #[test]
    fn test_fix_dirty_bitmap_orphans_noop_when_clean() {
        // No orphan rows → no warning → fixer is a no-op.
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");
        let _storage = SqliteStorage::open(&db_path).unwrap();

        let mut report = DoctorReport {
            ok: true,
            workspace_health: Some("healthy".to_string()),
            reliability_audit: None,
            checks: Vec::new(),
        };
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        check_dirty_bitmap_divergence(&conn, &mut report.checks);

        let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
            .expect("session must build");
        let ctx = OutputContext::from_output_format(crate::cli::OutputFormat::Text, false, true);
        assert!(!fix_dirty_bitmap_orphans_if_warned(
            &db_path,
            &report,
            &ctx,
            Some(&mut session),
        ));
    }

    #[test]
    fn test_check_doctor_runs_dir_below_threshold_ok() {
        // Few run dirs → Ok.
        let temp = TempDir::new().unwrap();
        let runs = temp.path().join(".doctor").join("runs");
        fs::create_dir_all(&runs).unwrap();
        for i in 0..5 {
            fs::create_dir_all(runs.join(format!("run-{i}"))).unwrap();
        }
        let mut checks = Vec::new();
        check_doctor_runs_dir_size(temp.path(), &mut checks);
        let check = find_check(&checks, "doctor.runs_dir").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    #[test]
    fn test_check_doctor_runs_dir_above_threshold_warns() {
        // >threshold run dirs → Warn with count.
        let temp = TempDir::new().unwrap();
        let runs = temp.path().join(".doctor").join("runs");
        fs::create_dir_all(&runs).unwrap();
        for i in 0..(DOCTOR_RUNS_THRESHOLD + 5) {
            fs::create_dir_all(runs.join(format!("run-{i}"))).unwrap();
        }
        let mut checks = Vec::new();
        check_doctor_runs_dir_size(temp.path(), &mut checks);
        let check = find_check(&checks, "doctor.runs_dir").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn));
        let run_count = check
            .details
            .as_ref()
            .and_then(|d| d.get("run_count"))
            .and_then(serde_json::Value::as_u64);
        assert_eq!(run_count, Some((DOCTOR_RUNS_THRESHOLD + 5) as u64));
    }

    #[test]
    fn test_check_doctor_runs_dir_missing_is_ok() {
        // No .doctor/runs at all → Ok (fresh workspace).
        let temp = TempDir::new().unwrap();
        let mut checks = Vec::new();
        check_doctor_runs_dir_size(temp.path(), &mut checks);
        let check = find_check(&checks, "doctor.runs_dir").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    #[test]
    fn test_check_doctor_runs_creatable_ok_when_missing() {
        // No .doctor/ at all → Ok (will be created on first --repair).
        let temp = TempDir::new().unwrap();
        let mut checks = Vec::new();
        check_doctor_runs_creatable(temp.path(), &mut checks);
        let check = find_check(&checks, "doctor.runs_creatable").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    #[test]
    fn test_check_doctor_runs_creatable_ok_when_writable() {
        let temp = TempDir::new().unwrap();
        fs::create_dir_all(temp.path().join(".doctor")).unwrap();
        let mut checks = Vec::new();
        check_doctor_runs_creatable(temp.path(), &mut checks);
        let check = find_check(&checks, "doctor.runs_creatable").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    #[test]
    fn test_check_root_gitignore_writable_ok_when_missing() {
        let temp = TempDir::new().unwrap();
        let mut checks = Vec::new();
        check_root_gitignore_writable(temp.path(), &mut checks);
        let check = find_check(&checks, "permissions.root_gitignore").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    #[test]
    fn test_check_root_gitignore_writable_ok_when_writable() {
        let temp = TempDir::new().unwrap();
        fs::write(temp.path().join(".gitignore"), b"target/\n").unwrap();
        let mut checks = Vec::new();
        check_root_gitignore_writable(temp.path(), &mut checks);
        let check = find_check(&checks, "permissions.root_gitignore").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    #[cfg(unix)]
    #[test]
    fn test_check_root_gitignore_writable_warns_when_not_writable() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        let gi = temp.path().join(".gitignore");
        fs::write(&gi, b"target/\n").unwrap();
        fs::set_permissions(&gi, fs::Permissions::from_mode(0o444)).unwrap();

        let mut checks = Vec::new();
        check_root_gitignore_writable(temp.path(), &mut checks);
        let check = find_check(&checks, "permissions.root_gitignore").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
        let details = check.details.as_ref().expect("details");
        assert_eq!(
            details.get("mode_octal").and_then(|v| v.as_str()),
            Some("444")
        );

        // Restore so TempDir's drop can clean up.
        fs::set_permissions(&gi, fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[test]
    fn test_check_write_lock_writable_ok_when_missing() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let mut checks = Vec::new();
        check_write_lock_writable(&beads_dir, &mut checks);
        let check = find_check(&checks, "permissions.write_lock").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    #[test]
    fn test_check_write_lock_writable_ok_when_writable() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::write(beads_dir.join(".write.lock"), b"").unwrap();
        let mut checks = Vec::new();
        check_write_lock_writable(&beads_dir, &mut checks);
        let check = find_check(&checks, "permissions.write_lock").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    #[cfg(unix)]
    #[test]
    fn test_check_write_lock_writable_warns_when_not_writable() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let lock = beads_dir.join(".write.lock");
        fs::write(&lock, b"").unwrap();
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o444)).unwrap();

        let mut checks = Vec::new();
        check_write_lock_writable(&beads_dir, &mut checks);
        let check = find_check(&checks, "permissions.write_lock").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
        let details = check.details.as_ref().expect("details");
        assert_eq!(
            details.get("mode_octal").and_then(|v| v.as_str()),
            Some("444")
        );

        // Restore so TempDir's drop can clean up.
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[test]
    fn test_check_recovery_dir_writable_ok_when_missing() {
        // No .br_recovery at all → Ok (created on demand).
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");
        let mut checks = Vec::new();
        check_recovery_dir_writable(&db_path, &beads_dir, &mut checks);
        let check = find_check(&checks, "permissions.recovery_dir").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    #[test]
    fn test_check_recovery_dir_writable_ok_when_writable() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(beads_dir.join(".br_recovery")).unwrap();
        let db_path = beads_dir.join("beads.db");
        let mut checks = Vec::new();
        check_recovery_dir_writable(&db_path, &beads_dir, &mut checks);
        let check = find_check(&checks, "permissions.recovery_dir").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    #[cfg(unix)]
    #[test]
    fn test_check_recovery_dir_writable_warns_when_not_writable() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let recovery = beads_dir.join(".br_recovery");
        fs::create_dir_all(&recovery).unwrap();
        fs::set_permissions(&recovery, fs::Permissions::from_mode(0o555)).unwrap();

        let db_path = beads_dir.join("beads.db");
        let mut checks = Vec::new();
        check_recovery_dir_writable(&db_path, &beads_dir, &mut checks);
        let check = find_check(&checks, "permissions.recovery_dir").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
        let details = check.details.as_ref().expect("details");
        assert_eq!(
            details.get("mode_octal").and_then(|v| v.as_str()),
            Some("555")
        );

        // Restore so TempDir's drop can clean up.
        fs::set_permissions(&recovery, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn test_check_recovery_dir_writable_uses_configured_db_parent() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(beads_dir.join(".br_recovery")).unwrap();
        let db_dir = temp.path().join("configured-db");
        let recovery = db_dir.join(".br_recovery");
        fs::create_dir_all(&recovery).unwrap();
        fs::set_permissions(&recovery, fs::Permissions::from_mode(0o555)).unwrap();

        let db_path = db_dir.join("beads.db");
        let mut checks = Vec::new();
        check_recovery_dir_writable(&db_path, &beads_dir, &mut checks);
        let check = find_check(&checks, "permissions.recovery_dir").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
        let expected_path = recovery.to_string_lossy().into_owned();
        assert_eq!(
            check
                .details
                .as_ref()
                .and_then(|details| details.get("path"))
                .and_then(|path| path.as_str()),
            Some(expected_path.as_str())
        );

        // Restore so TempDir's drop can clean up.
        fs::set_permissions(&recovery, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn test_check_doctor_runs_creatable_warns_when_not_writable() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        let doctor = temp.path().join(".doctor");
        fs::create_dir_all(&doctor).unwrap();
        // Strip owner-write so the next --repair can't create a run-dir.
        fs::set_permissions(&doctor, fs::Permissions::from_mode(0o555)).unwrap();

        let mut checks = Vec::new();
        check_doctor_runs_creatable(temp.path(), &mut checks);
        let check = find_check(&checks, "doctor.runs_creatable").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
        let details = check.details.as_ref().expect("details");
        assert_eq!(
            details.get("mode_octal").and_then(|v| v.as_str()),
            Some("555")
        );

        // Restore so TempDir's drop can clean up.
        fs::set_permissions(&doctor, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn test_check_config_yaml_secret_mode_warns_on_world_readable_with_secrets() {
        // Pass-5 cycle 11: world-readable config.yaml containing a
        // secret-shaped keyword → warn.
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let config = beads_dir.join("config.yaml");
        fs::write(&config, b"github_token: ghp_abc123\n").unwrap();
        let mut perms = fs::metadata(&config).unwrap().permissions();
        perms.set_mode(0o644); // world-readable
        fs::set_permissions(&config, perms).unwrap();

        let mut checks = Vec::new();
        check_config_yaml_secret_mode(&beads_dir, &mut checks);
        let check = find_check(&checks, "permissions.config_yaml_secrets").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
        let matched = check
            .details
            .as_ref()
            .and_then(|d| d.get("matched_keywords"))
            .and_then(|v| v.as_array())
            .map(Vec::len)
            .unwrap_or(0);
        assert!(matched >= 1);
    }

    #[cfg(unix)]
    #[test]
    fn test_check_config_yaml_secret_mode_ok_when_mode_0600() {
        // mode 0600 → not world-readable → Ok regardless of contents.
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let config = beads_dir.join("config.yaml");
        fs::write(&config, b"github_token: ghp_xyz\npassword: hunter2\n").unwrap();
        let mut perms = fs::metadata(&config).unwrap().permissions();
        perms.set_mode(0o600);
        fs::set_permissions(&config, perms).unwrap();

        let mut checks = Vec::new();
        check_config_yaml_secret_mode(&beads_dir, &mut checks);
        let check = find_check(&checks, "permissions.config_yaml_secrets").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok), "{check:?}");
    }

    #[cfg(unix)]
    #[test]
    fn test_check_config_yaml_secret_mode_ok_when_no_secrets() {
        // World-readable but no secret-keywords → Ok.
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let config = beads_dir.join("config.yaml");
        fs::write(&config, b"theme: dark\neditor: vim\n").unwrap();
        let mut perms = fs::metadata(&config).unwrap().permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&config, perms).unwrap();

        let mut checks = Vec::new();
        check_config_yaml_secret_mode(&beads_dir, &mut checks);
        let check = find_check(&checks, "permissions.config_yaml_secrets").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok), "{check:?}");
    }

    #[cfg(unix)]
    #[test]
    fn test_br_binaries_in_path_str_finds_multiple() {
        // Pass-5 cycle 12: pure helper finds duplicate br executables
        // across a synthesized PATH string.
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        let dir_a = temp.path().join("a");
        let dir_b = temp.path().join("b");
        fs::create_dir_all(&dir_a).unwrap();
        fs::create_dir_all(&dir_b).unwrap();
        let br_a = dir_a.join("br");
        let br_b = dir_b.join("br");
        fs::write(&br_a, b"#!/bin/sh\n").unwrap();
        fs::write(&br_b, b"#!/bin/sh\n").unwrap();
        let mut perms = fs::metadata(&br_a).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&br_a, perms.clone()).unwrap();
        fs::set_permissions(&br_b, perms).unwrap();

        let path_var = format!("{}:{}", dir_a.display(), dir_b.display());
        let found = br_binaries_in_path_str(&path_var);
        assert_eq!(found.len(), 2);
    }

    #[cfg(unix)]
    #[test]
    fn test_br_binaries_in_path_str_skips_non_executable() {
        // Non-executable file shouldn't count as a `br` binary.
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        let dir = temp.path().join("d");
        fs::create_dir_all(&dir).unwrap();
        let br = dir.join("br");
        fs::write(&br, b"not a binary").unwrap();
        let mut perms = fs::metadata(&br).unwrap().permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&br, perms).unwrap();

        let path_var = dir.display().to_string();
        let found = br_binaries_in_path_str(&path_var);
        assert!(found.is_empty(), "non-executable should be skipped");
    }

    #[cfg(unix)]
    #[test]
    fn test_br_binaries_in_path_str_dedupes_canonical_path() {
        // Two PATH entries pointing at the same canonical br via a
        // symlinked directory should count as one binary.
        use std::os::unix::fs::PermissionsExt;
        use std::os::unix::fs::symlink;
        let temp = TempDir::new().unwrap();
        let real_dir = temp.path().join("real");
        fs::create_dir_all(&real_dir).unwrap();
        let br = real_dir.join("br");
        fs::write(&br, b"#!/bin/sh\n").unwrap();
        let mut perms = fs::metadata(&br).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&br, perms).unwrap();
        let link_dir = temp.path().join("link");
        symlink(&real_dir, &link_dir).unwrap();

        let path_var = format!("{}:{}", real_dir.display(), link_dir.display());
        let found = br_binaries_in_path_str(&path_var);
        assert_eq!(found.len(), 1, "canonical dedup expected: {found:?}");
    }

    #[test]
    fn test_check_inner_gitignore_present_missing_warns() {
        // Pass-5 cycle 13: no .beads/.gitignore at all → warn kind="missing".
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let mut checks = Vec::new();
        check_inner_gitignore_present(&beads_dir, &mut checks);
        let check = find_check(&checks, "gitignore.beads_inner_present").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
        assert_eq!(
            check
                .details
                .as_ref()
                .and_then(|d| d.get("kind"))
                .and_then(|v| v.as_str()),
            Some("missing")
        );
    }

    #[test]
    fn test_check_inner_gitignore_present_complete_ok() {
        // Complete .gitignore covering every expectation → ok. Written as
        // the canonical append patterns themselves, which must always
        // self-satisfy their own probes.
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let contents = inner_gitignore_all_append_patterns().join("\n") + "\n";
        fs::write(beads_dir.join(".gitignore"), contents).unwrap();

        let mut checks = Vec::new();
        check_inner_gitignore_present(&beads_dir, &mut checks);
        let check = find_check(&checks, "gitignore.beads_inner_present").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok), "{check:?}");
    }

    #[test]
    fn test_check_inner_gitignore_operator_megamix_equivalents_are_ok() {
        // GitHub #427: operator-authored equivalents (broad `*.db?*`,
        // `*.lock`, `*-fsqlite-ns-*` generics) must satisfy coverage via
        // probes without textual equality with the canonical patterns.
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::write(
            beads_dir.join(".gitignore"),
            b"*.db\n*.db?*\n*-fsqlite-ns-gate\n*-fsqlite-ns-use\n*.lock\n*.tmp\n",
        )
        .unwrap();

        let mut checks = Vec::new();
        check_inner_gitignore_present(&beads_dir, &mut checks);
        let check = find_check(&checks, "gitignore.beads_inner_present").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok), "{check:?}");
    }

    #[test]
    fn test_check_inner_gitignore_flags_missing_db_artifact_family() {
        // A pre-fsqlite-0.3 ignore file (bare `*.db` only) must warn and
        // name the sidecar/vacuum patterns it is missing.
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::write(beads_dir.join(".gitignore"), b"*.db\n.write.lock\n*.tmp\n").unwrap();

        let mut checks = Vec::new();
        check_inner_gitignore_present(&beads_dir, &mut checks);
        let check = find_check(&checks, "gitignore.beads_inner_present").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
        let missing: Vec<String> = check
            .details
            .as_ref()
            .and_then(|d| d.get("missing_patterns"))
            .and_then(|v| v.as_array())
            .map(|patterns| {
                patterns
                    .iter()
                    .filter_map(|p| p.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        for expected in [
            "*.db-wal*",
            "*-fsqlite-ns-gate",
            "*-fsqlite-ns-use",
            "*.vacuum-wal-cert*",
        ] {
            assert!(
                missing.iter().any(|p| p == expected),
                "expected {expected} in missing patterns, got {missing:?}"
            );
        }
    }

    #[test]
    fn test_inner_gitignore_broad_lock_rule_and_later_negation() {
        assert!(inner_gitignore_ignores_probe(
            "*.lock\n*.tmp\n",
            ".write.lock"
        ));
        assert!(!inner_gitignore_ignores_probe(
            "*.lock\n!.write.lock\n*.tmp\n",
            ".write.lock"
        ));
    }

    #[test]
    fn test_gitignore_glob_matches_semantics() {
        // `*` spans any run including empty; `?` is exactly one char.
        assert!(gitignore_glob_matches("*.db?*", "beads.db-wal"));
        assert!(gitignore_glob_matches(
            "*.db?*",
            ".beads.db.schema-migration-20260101T000000.000000Z-0-0.vacuum-fsqlite-ns-gate"
        ));
        assert!(!gitignore_glob_matches("*.db?*", "beads.db"));
        assert!(gitignore_glob_matches("*.db", "beads.db"));
        assert!(gitignore_glob_matches(
            "*-fsqlite-ns-gate",
            "beads.db-fsqlite-ns-gate"
        ));
        assert!(!gitignore_glob_matches("*.tmp", "probe.tmpx"));
        assert!(gitignore_glob_matches("probe.?mp", "probe.tmp"));
        assert!(!gitignore_glob_matches("probe.?mp", "probe.mp"));
    }

    #[test]
    fn test_check_inner_gitignore_present_incomplete_warns() {
        // Has .gitignore but missing one expected pattern → warn kind="incomplete".
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        // Everything except *.tmp.
        let contents = inner_gitignore_all_append_patterns()
            .into_iter()
            .filter(|pattern| *pattern != "*.tmp")
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        fs::write(beads_dir.join(".gitignore"), contents).unwrap();

        let mut checks = Vec::new();
        check_inner_gitignore_present(&beads_dir, &mut checks);
        let check = find_check(&checks, "gitignore.beads_inner_present").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn));
        assert_eq!(
            check
                .details
                .as_ref()
                .and_then(|d| d.get("kind"))
                .and_then(|v| v.as_str()),
            Some("incomplete")
        );
        let missing = check
            .details
            .as_ref()
            .and_then(|d| d.get("missing_patterns"))
            .and_then(|v| v.as_array())
            .map(Vec::len)
            .unwrap_or(0);
        assert_eq!(missing, 1);
    }

    #[cfg(unix)]
    #[test]
    fn test_check_inner_gitignore_present_symlink_warns_even_with_valid_target() {
        // Git ignore rules in the working tree must be regular files.
        // Reading through a symlink here would make doctor report a
        // false healthy state while git ignores the symlinked ignore file.
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let target = temp.path().join("shared-ignore");
        fs::write(&target, b".write.lock\n*.tmp\n").unwrap();
        symlink(&target, beads_dir.join(".gitignore")).unwrap();

        let mut checks = Vec::new();
        check_inner_gitignore_present(&beads_dir, &mut checks);
        let check = find_check(&checks, "gitignore.beads_inner_present").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
        assert_eq!(
            check
                .details
                .as_ref()
                .and_then(|d| d.get("kind"))
                .and_then(|v| v.as_str()),
            Some("symlink")
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_check_jsonl_world_writable_warns_on_world_writable() {
        // Pass-5 cycle 14: world-writable mode → warn.
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let jsonl = beads_dir.join("issues.jsonl");
        fs::write(&jsonl, b"{}\n").unwrap();
        let mut perms = fs::metadata(&jsonl).unwrap().permissions();
        perms.set_mode(0o666); // world-writable
        fs::set_permissions(&jsonl, perms).unwrap();

        let mut checks = Vec::new();
        check_jsonl_world_writable(Some(&jsonl), &mut checks);
        let check = find_check(&checks, "permissions.jsonl_world_writable").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
    }

    #[cfg(unix)]
    #[test]
    fn test_check_jsonl_world_writable_ok_when_mode_0644() {
        // Mode 0644 (world-readable but not world-writable) → ok.
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let jsonl = beads_dir.join("issues.jsonl");
        fs::write(&jsonl, b"{}\n").unwrap();
        let mut perms = fs::metadata(&jsonl).unwrap().permissions();
        perms.set_mode(0o644);
        fs::set_permissions(&jsonl, perms).unwrap();

        let mut checks = Vec::new();
        check_jsonl_world_writable(Some(&jsonl), &mut checks);
        let check = find_check(&checks, "permissions.jsonl_world_writable").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok), "{check:?}");
    }

    #[test]
    fn test_check_jsonl_world_writable_missing_is_ok() {
        // No issues.jsonl at all → ok (other checks own the missing case).
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let mut checks = Vec::new();
        check_jsonl_world_writable(None, &mut checks);
        let check = find_check(&checks, "permissions.jsonl_world_writable").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    #[test]
    fn test_check_orphan_tmp_files_old_tmp_warns() {
        // Pass-5 cycle 15: a tmp file backdated >1 hour → warn.
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let tmp_path = beads_dir.join("issues.jsonl.99999.tmp");
        fs::write(&tmp_path, b"partial write").unwrap();
        // Backdate beyond the 1-hour threshold.
        let two_hours_ago =
            std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 60 * 60);
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&tmp_path)
            .unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(two_hours_ago))
            .unwrap();
        drop(f);

        let mut checks = Vec::new();
        check_orphan_tmp_files(&beads_dir, &mut checks);
        let check = find_check(&checks, "tmp_files_orphan").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
        let files = check
            .details
            .as_ref()
            .and_then(|d| d.get("files"))
            .and_then(|v| v.as_array())
            .map(Vec::len)
            .unwrap_or(0);
        assert_eq!(files, 1);
    }

    #[test]
    fn test_check_orphan_tmp_files_fresh_tmp_ok() {
        // Fresh tmp (current mtime) is not orphan — could be an in-flight write.
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::write(beads_dir.join("issues.jsonl.123.tmp"), b"fresh").unwrap();

        let mut checks = Vec::new();
        check_orphan_tmp_files(&beads_dir, &mut checks);
        let check = find_check(&checks, "tmp_files_orphan").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok), "{check:?}");
    }

    #[test]
    fn test_check_orphan_tmp_files_ignores_non_tmp_files() {
        // Plain `.jsonl` shouldn't match the tmp pattern even if old.
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let jsonl = beads_dir.join("issues.jsonl");
        fs::write(&jsonl, b"{}\n").unwrap();
        let two_hours_ago =
            std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 60 * 60);
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&jsonl)
            .unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(two_hours_ago))
            .unwrap();
        drop(f);

        let mut checks = Vec::new();
        check_orphan_tmp_files(&beads_dir, &mut checks);
        let check = find_check(&checks, "tmp_files_orphan").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    #[test]
    fn test_check_jsonl_duplicate_ids_warns_on_dup() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let jsonl = beads_dir.join("issues.jsonl");
        // 3 records: two share id=bd-aaa, one unique. The second
        // duplicate intentionally uses valid JSON whitespace that a
        // literal `"id":"` substring scan would miss.
        fs::write(
            &jsonl,
            "{\"id\":\"bd-aaa\",\"title\":\"first\"}\n\
             {\"id\":\"bd-bbb\",\"title\":\"unique\"}\n\
             {\"id\": \"bd-aaa\", \"title\":\"merge-conflict-side\"}\n",
        )
        .unwrap();

        let mut checks = Vec::new();
        check_jsonl_duplicate_ids(Some(&jsonl), &mut checks);
        let check = find_check(&checks, "jsonl.duplicate_ids").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
        let details = check.details.as_ref().expect("details");
        assert_eq!(
            details
                .get("distinct_duplicate_ids")
                .and_then(serde_json::Value::as_u64),
            Some(1)
        );
        assert_eq!(
            details
                .get("total_duplicate_records")
                .and_then(serde_json::Value::as_u64),
            Some(2)
        );
        let sample = details
            .get("sample_duplicate_ids")
            .and_then(|v| v.as_array())
            .expect("sample array");
        assert_eq!(sample.len(), 1);
        assert_eq!(sample[0].as_str(), Some("bd-aaa"));
    }

    #[test]
    fn test_check_jsonl_duplicate_ids_ok_when_unique() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let jsonl = beads_dir.join("issues.jsonl");
        fs::write(
            &jsonl,
            "{\"id\":\"bd-aaa\"}\n{\"id\":\"bd-bbb\"}\n{\"id\":\"bd-ccc\"}\n",
        )
        .unwrap();

        let mut checks = Vec::new();
        check_jsonl_duplicate_ids(Some(&jsonl), &mut checks);
        let check = find_check(&checks, "jsonl.duplicate_ids").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    #[test]
    fn test_check_jsonl_duplicate_ids_missing_is_ok() {
        // No JSONL path -> ok. Missing-or-empty cases handled by other checks.
        let mut checks = Vec::new();
        check_jsonl_duplicate_ids(None, &mut checks);
        let check = find_check(&checks, "jsonl.duplicate_ids").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    #[test]
    fn test_check_jsonl_oversized_warns_above_threshold() {
        // Pass-5 cycle 17: file size > threshold → warn.
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let jsonl = beads_dir.join("issues.jsonl");
        // Write a sparse file beyond the threshold using set_len(). The
        // OS treats this as zero-filled without actually consuming
        // 100MB on disk (filesystem-dependent; works on tmpfs/ext4/xfs).
        let f = fs::File::create(&jsonl).unwrap();
        f.set_len(JSONL_OVERSIZED_THRESHOLD_BYTES + 1).unwrap();
        drop(f);

        let mut checks = Vec::new();
        check_jsonl_oversized(Some(&jsonl), &mut checks);
        let check = find_check(&checks, "jsonl_size").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
        let size_bytes = check
            .details
            .as_ref()
            .and_then(|d| d.get("size_bytes"))
            .and_then(serde_json::Value::as_u64);
        assert_eq!(size_bytes, Some(JSONL_OVERSIZED_THRESHOLD_BYTES + 1));
    }

    #[test]
    fn test_check_jsonl_oversized_ok_below_threshold() {
        // Small JSONL → ok.
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let jsonl = beads_dir.join("issues.jsonl");
        fs::write(&jsonl, b"{\"id\":\"bd-tiny\"}\n").unwrap();
        let mut checks = Vec::new();
        check_jsonl_oversized(Some(&jsonl), &mut checks);
        let check = find_check(&checks, "jsonl_size").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    #[test]
    fn test_check_br_history_above_threshold_warns() {
        // Pass-5 cycle 18: > threshold snapshot files → warn with count.
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let history = beads_dir.join(".br_history");
        fs::create_dir_all(&history).unwrap();
        for i in 0..(BR_HISTORY_SNAPSHOT_THRESHOLD + 5) {
            fs::write(
                history.join(format!("issues.20250101_000000.{i}.jsonl")),
                b"{}\n",
            )
            .unwrap();
        }
        let mut checks = Vec::new();
        check_br_history_size(&beads_dir, &mut checks);
        let check = find_check(&checks, "br_history.size").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
        let count = check
            .details
            .as_ref()
            .and_then(|d| d.get("snapshot_count"))
            .and_then(serde_json::Value::as_u64);
        assert_eq!(count, Some((BR_HISTORY_SNAPSHOT_THRESHOLD + 5) as u64));
    }

    #[test]
    fn test_check_br_history_below_threshold_ok() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let history = beads_dir.join(".br_history");
        fs::create_dir_all(&history).unwrap();
        for i in 0..5 {
            fs::write(
                history.join(format!("issues.20250101_000000.{i}.jsonl")),
                b"{}\n",
            )
            .unwrap();
        }
        let mut checks = Vec::new();
        check_br_history_size(&beads_dir, &mut checks);
        let check = find_check(&checks, "br_history.size").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    #[test]
    fn test_check_br_history_metadata_sidecars_do_not_count_as_snapshots() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let history = beads_dir.join(".br_history");
        fs::create_dir_all(&history).unwrap();
        for i in 0..BR_HISTORY_SNAPSHOT_THRESHOLD {
            let backup = history.join(format!("issues.20250101_000000.{i}.jsonl"));
            fs::write(&backup, b"{}\n").unwrap();
            fs::write(backup.with_extension("jsonl.meta.json"), b"{not-json").unwrap();
        }
        let mut checks = Vec::new();
        check_br_history_size(&beads_dir, &mut checks);
        let check = find_check(&checks, "br_history.size").expect("check present");
        assert!(
            matches!(check.status, CheckStatus::Ok),
            "metadata sidecars must not push threshold backups over the limit: {check:?}"
        );
    }

    #[test]
    fn test_check_br_history_missing_is_ok() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let mut checks = Vec::new();
        check_br_history_size(&beads_dir, &mut checks);
        let check = find_check(&checks, "br_history.size").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    #[test]
    fn test_check_jsonl_crlf_endings_warns_on_crlf() {
        // Pass-5 cycle 20: CRLF in JSONL → warn.
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::write(
            beads_dir.join("issues.jsonl"),
            b"{\"id\":\"bd-windows\"}\r\n",
        )
        .unwrap();

        let mut checks = Vec::new();
        check_jsonl_crlf_endings(Some(&beads_dir.join("issues.jsonl")), &mut checks);
        let check = find_check(&checks, "jsonl_crlf").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
    }

    #[test]
    fn test_check_jsonl_crlf_endings_lf_only_is_ok() {
        // Plain LF → ok.
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::write(beads_dir.join("issues.jsonl"), b"{\"id\":\"bd-unix\"}\n").unwrap();

        let mut checks = Vec::new();
        check_jsonl_crlf_endings(Some(&beads_dir.join("issues.jsonl")), &mut checks);
        let check = find_check(&checks, "jsonl_crlf").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    #[test]
    fn test_check_jsonl_crlf_endings_missing_is_ok() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let mut checks = Vec::new();
        check_jsonl_crlf_endings(None, &mut checks);
        let check = find_check(&checks, "jsonl_crlf").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    #[test]
    fn test_check_jsonl_utf8_bom_warns_when_present() {
        // Pass-5 cycle 21: BOM in JSONL → warn.
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let mut bom_bytes = UTF8_BOM.to_vec();
        bom_bytes.extend_from_slice(b"{\"id\":\"bd-bom\"}\n");
        fs::write(beads_dir.join("issues.jsonl"), &bom_bytes).unwrap();

        let mut checks = Vec::new();
        check_jsonl_utf8_bom(Some(&beads_dir.join("issues.jsonl")), &mut checks);
        let check = find_check(&checks, "jsonl_bom").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
    }

    #[test]
    fn test_check_jsonl_utf8_bom_ok_when_clean() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::write(beads_dir.join("issues.jsonl"), b"{\"id\":\"bd-clean\"}\n").unwrap();
        let mut checks = Vec::new();
        check_jsonl_utf8_bom(Some(&beads_dir.join("issues.jsonl")), &mut checks);
        let check = find_check(&checks, "jsonl_bom").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    #[test]
    fn test_fix_jsonl_utf8_bom_strips_via_chokepoint() {
        // The fixer must rewrite the file without the leading BOM,
        // preserving the rest of the bytes verbatim.
        let temp = TempDir::new().unwrap();
        let jsonl_dir = temp.path().join("external");
        fs::create_dir_all(&jsonl_dir).unwrap();
        let jsonl_path = jsonl_dir.join("issues.jsonl");
        let payload = b"{\"id\":\"bd-bom\"}\n{\"id\":\"bd-other\"}\n";
        let mut bom_bytes = UTF8_BOM.to_vec();
        bom_bytes.extend_from_slice(payload);
        fs::write(&jsonl_path, &bom_bytes).unwrap();

        let mut report = DoctorReport {
            ok: false,
            workspace_health: Some("degraded".to_string()),
            reliability_audit: None,
            checks: Vec::new(),
        };
        check_jsonl_utf8_bom(Some(&jsonl_path), &mut report.checks);

        let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
            .expect("session must build");
        let ctx = OutputContext::from_output_format(crate::cli::OutputFormat::Text, false, true);
        assert!(fix_jsonl_utf8_bom_if_warned(
            Some(&jsonl_path),
            &report,
            &ctx,
            Some(&mut session),
        ));
        let after = fs::read(&jsonl_path).unwrap();
        assert_eq!(after, payload, "BOM stripped, payload preserved");
        assert!(
            session
                .run
                .root
                .join("backups/external/issues.jsonl")
                .is_file(),
            "selected in-workspace JSONL should be backed up relative to repo root"
        );
    }

    #[test]
    fn test_fix_jsonl_utf8_bom_skips_traversal_outside_workspace() {
        let parent = TempDir::new().unwrap();
        let repo = parent.path().join("repo");
        let outside = parent.path().join("outside");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let outside_jsonl = outside.join("issues.jsonl");
        let mut original = UTF8_BOM.to_vec();
        original.extend_from_slice(b"{\"id\":\"bd-outside\"}\n");
        fs::write(&outside_jsonl, &original).unwrap();
        let traversal_path = repo.join("../outside/issues.jsonl");
        let mut report = DoctorReport {
            ok: false,
            workspace_health: Some("degraded".to_string()),
            reliability_audit: None,
            checks: Vec::new(),
        };
        check_jsonl_utf8_bom(Some(&traversal_path), &mut report.checks);
        let mut session = DoctorRepairSession::new(&repo, /* dry_run = */ false).expect("session");

        assert!(!fix_jsonl_utf8_bom_if_warned(
            Some(&traversal_path),
            &report,
            &quiet_ctx(),
            Some(&mut session),
        ));
        assert_eq!(
            fs::read(&outside_jsonl).unwrap(),
            original,
            "traversal-selected outside JSONL must not be rewritten"
        );
        assert_eq!(
            fs::read_to_string(&session.run.actions_file).unwrap(),
            "",
            "skipped traversal repairs must not write an undo action"
        );
    }

    #[test]
    fn test_check_db_bloat_warns_on_high_ratio() {
        // Pass-5 cycle 22: db >> jsonl by ratio → warn.
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        // JSONL: just above the 1MB minimum threshold.
        let jsonl_path = beads_dir.join("issues.jsonl");
        let jsonl_f = fs::File::create(&jsonl_path).unwrap();
        jsonl_f.set_len(DB_BLOAT_MIN_JSONL_BYTES + 1024).unwrap();
        drop(jsonl_f);
        // DB: 20x the JSONL size (well above the 10x threshold).
        let db_path = beads_dir.join("beads.db");
        let db_f = fs::File::create(&db_path).unwrap();
        db_f.set_len((DB_BLOAT_MIN_JSONL_BYTES + 1024) * 20)
            .unwrap();
        drop(db_f);

        let mut checks = Vec::new();
        check_db_bloat_vs_jsonl(&db_path, Some(&jsonl_path), &mut checks);
        let check = find_check(&checks, "db_bloat").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
        let ratio = check
            .details
            .as_ref()
            .and_then(|d| d.get("ratio"))
            .and_then(serde_json::Value::as_u64);
        assert_eq!(ratio, Some(20));
    }

    /// `beads_rust-jwrr`: `.beads/` accumulates manual backups from other tools
    /// and ad-hoc agent sessions. br must surface them without claiming them as
    /// its own and without failing an otherwise healthy workspace.
    #[test]
    fn test_foreign_recovery_debris_is_reported_without_failing_the_workspace() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");
        fs::write(&db_path, b"not a real db, only the name matters here").unwrap();

        // br's own state directories must never be reported as foreign.
        fs::create_dir_all(beads_dir.join(".br_recovery")).unwrap();
        fs::create_dir_all(beads_dir.join(".br_history")).unwrap();

        // The spellings observed in the wild, including the nesting that a
        // `cp -r .beads .beads/recovery_<TS>` produces.
        for dir in [
            "recovery",
            "recovery_20260319T032504Z",
            "recovery_snapshot_20260322T032047Z",
            "snapshot_20260322T032111Z",
            ".beads_snapshot",
        ] {
            fs::create_dir_all(beads_dir.join(dir)).unwrap();
        }
        fs::write(
            beads_dir.join("recovery_20260319T032504Z/payload.bin"),
            vec![7_u8; 2048],
        )
        .unwrap();
        fs::write(
            beads_dir.join("beads.db.rebuild_20260321T073015Z"),
            vec![1_u8; 1024],
        )
        .unwrap();

        let mut checks = Vec::new();
        check_foreign_recovery_debris(&beads_dir, &db_path, &mut checks).unwrap();
        let check = find_check(&checks, "db.foreign_recovery_debris").expect("check present");

        // Informational, never a warning: br did not create this and must not
        // make `br doctor` exit non-zero over another tool's leftovers.
        assert!(matches!(check.status, CheckStatus::Ok), "{check:?}");

        let details = check.details.as_ref().expect("details");
        let dirs: Vec<&str> = details["directories"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(
            dirs.len(),
            5,
            "all five spellings should be caught: {dirs:?}"
        );
        assert!(
            !dirs
                .iter()
                .any(|d| d.contains(".br_recovery") || d.contains(".br_history")),
            "br's own directories must not be reported as foreign: {dirs:?}"
        );
        assert_eq!(details["files"].as_array().unwrap().len(), 1);
        // 2048 bytes inside the debris tree + the 1024-byte rebuild sibling.
        assert_eq!(details["bytes"].as_u64(), Some(3072));
        assert_eq!(details["bytes_are_lower_bound"].as_bool(), Some(false));
    }

    #[test]
    fn test_clean_workspace_reports_no_foreign_recovery_debris() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(beads_dir.join(".br_recovery")).unwrap();
        fs::create_dir_all(beads_dir.join(".br_history")).unwrap();
        let db_path = beads_dir.join("beads.db");
        fs::write(&db_path, b"db").unwrap();
        fs::write(beads_dir.join("issues.jsonl"), b"").unwrap();
        // A legitimate br backup sibling belongs to `db.recovery_artifacts`,
        // not to this check.
        fs::write(beads_dir.join("beads.db.bad_20260312T000000Z"), b"x").unwrap();

        let mut checks = Vec::new();
        check_foreign_recovery_debris(&beads_dir, &db_path, &mut checks).unwrap();
        let check = find_check(&checks, "db.foreign_recovery_debris").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
        assert!(check.message.is_none(), "{check:?}");
        // `push_check` always stamps a `finding_id`, so the absence of debris
        // is expressed by the payload keys being absent, not by `details`.
        if let Some(details) = check.details.as_ref() {
            assert!(details.get("directories").is_none(), "{check:?}");
            assert!(details.get("files").is_none(), "{check:?}");
            assert!(details.get("bytes").is_none(), "{check:?}");
        }
    }

    #[test]
    fn test_foreign_debris_name_predicates() {
        assert!(is_foreign_recovery_debris_dir_name("recovery"));
        assert!(is_foreign_recovery_debris_dir_name(
            "recovery_20260319T032504Z"
        ));
        assert!(is_foreign_recovery_debris_dir_name(
            "recovery_20260322T032013Z_codex_beads_repair"
        ));
        assert!(is_foreign_recovery_debris_dir_name(
            "snapshot_20260322T032111Z"
        ));
        assert!(is_foreign_recovery_debris_dir_name(".beads_snapshot"));
        // br's own state, and unrelated entries.
        assert!(!is_foreign_recovery_debris_dir_name(".br_recovery"));
        assert!(!is_foreign_recovery_debris_dir_name(".br_history"));
        assert!(!is_foreign_recovery_debris_dir_name("issues.jsonl"));
        assert!(!is_foreign_recovery_debris_dir_name(".doctor"));

        assert!(is_foreign_recovery_debris_file_name(
            "beads.db.rebuild_20260321T073015Z",
            "beads.db"
        ));
        // `.bad_*` is already owned by `db.recovery_artifacts`.
        assert!(!is_foreign_recovery_debris_file_name(
            "beads.db.bad_20260312T000000Z",
            "beads.db"
        ));
        assert!(!is_foreign_recovery_debris_file_name(
            "beads.db", "beads.db"
        ));
    }

    /// `beads_rust-ote7`: `vacuum_database` must compact a database that carries
    /// whole zero pages past its logical end — the exact shape `db_bloat` fires
    /// on. In-place `VACUUM` refuses those with "database image header page
    /// count N does not match file length page count M", and the refusal used
    /// to surface as "No errors detected; nothing to repair."
    #[test]
    fn test_vacuum_database_compacts_a_database_with_trailing_pages() {
        const APPENDED_PAGES: u32 = 64;

        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");

        // A real, valid database with something in it.
        {
            let mut storage = SqliteStorage::open(&db_path).unwrap();
            for i in 0..50 {
                let mut issue = sample_issue(&format!("bd-bloat{i:03}"), "bloat fixture issue");
                issue.description = Some("x".repeat(4096));
                storage.create_issue(&issue, "test").unwrap();
            }
            storage.checkpoint_full().ok();
        }
        let header_pages_before = database_header_page_count(&db_path);
        let logical_size = fs::metadata(&db_path).unwrap().len();

        // Append whole zero pages past the logical end. SQLite reads the extent
        // from the header and ignores these; the file is still valid.
        {
            let mut f = fs::OpenOptions::new().append(true).open(&db_path).unwrap();
            f.write_all(&vec![0_u8; 4096 * APPENDED_PAGES as usize])
                .unwrap();
            f.flush().unwrap();
        }
        let bloated_size = fs::metadata(&db_path).unwrap().len();
        assert!(
            bloated_size > logical_size,
            "fixture should have grown: {bloated_size} vs {logical_size}"
        );

        let write_authority = std::sync::Arc::new(
            crate::sync::blocking_database_family_write_lock_with_timeout(
                &beads_dir, &db_path, None,
            )
            .expect("acquire database-family write authority for vacuum test"),
        );
        vacuum_database(&db_path, &write_authority)
            .expect("vacuum must compact a trailing-pages database");

        let after = fs::metadata(&db_path).unwrap().len();
        assert!(
            after < bloated_size,
            "VACUUM should reclaim the appended slack: {after} still >= {bloated_size}"
        );
        // The appended pages must be dropped, not adopted into the database.
        // The page count is not compared to `header_pages_before` directly:
        // that reading can lag the true extent by a few pages depending on when
        // the WAL was folded in. What must hold is that the 64 appended pages
        // did not become part of the database.
        let header_pages_after = database_header_page_count(&db_path);
        assert!(
            header_pages_after < header_pages_before + APPENDED_PAGES,
            "compaction absorbed the appended pages: {header_pages_after} \
             (was {header_pages_before}, appended {APPENDED_PAGES})"
        );
        // The decisive invariant: the compacted file has no slack left at all,
        // so the header's extent and the file length agree exactly. That is
        // precisely what in-place VACUUM could not achieve on this input.
        assert_eq!(
            u64::from(header_pages_after) * 4096,
            after,
            "compacted file should contain exactly its logical pages"
        );
        // And the data must survive.
        let storage = SqliteStorage::open(&db_path).unwrap();
        assert!(storage.get_issue("bd-bloat000").unwrap().is_some());
        assert!(storage.get_issue("bd-bloat049").unwrap().is_some());
    }

    /// Read the page count from the SQLite header (page 1, offset 28) without
    /// going through the engine, so the assertion above is independent of
    /// whatever the engine believes the extent to be.
    fn database_header_page_count(db_path: &Path) -> u32 {
        let bytes = fs::read(db_path).unwrap();
        u32::from_be_bytes([bytes[28], bytes[29], bytes[30], bytes[31]])
    }

    #[test]
    fn test_check_db_bloat_ok_under_threshold() {
        // DB only 2x the JSONL → ok.
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");
        let jsonl_f = fs::File::create(&jsonl_path).unwrap();
        jsonl_f.set_len(DB_BLOAT_MIN_JSONL_BYTES + 1024).unwrap();
        drop(jsonl_f);
        let db_path = beads_dir.join("beads.db");
        let db_f = fs::File::create(&db_path).unwrap();
        db_f.set_len((DB_BLOAT_MIN_JSONL_BYTES + 1024) * 2).unwrap();
        drop(db_f);

        let mut checks = Vec::new();
        check_db_bloat_vs_jsonl(&db_path, Some(&jsonl_path), &mut checks);
        let check = find_check(&checks, "db_bloat").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    #[test]
    fn test_check_db_bloat_skip_small_workspaces() {
        // JSONL < 1 MB minimum → check skipped (ok). DB much larger
        // doesn't matter; the ratio is meaningless for tiny workspaces.
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::write(&jsonl_path, b"tiny").unwrap();
        let db_path = beads_dir.join("beads.db");
        let db_f = fs::File::create(&db_path).unwrap();
        db_f.set_len(100 * 1024 * 1024).unwrap();
        drop(db_f);

        let mut checks = Vec::new();
        check_db_bloat_vs_jsonl(&db_path, Some(&jsonl_path), &mut checks);
        let check = find_check(&checks, "db_bloat").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    #[test]
    fn test_check_db_bloat_uses_selected_db_and_jsonl_paths() {
        let temp = TempDir::new().unwrap();
        let db_dir = temp.path().join("db");
        let jsonl_dir = temp.path().join("external");
        fs::create_dir_all(&db_dir).unwrap();
        fs::create_dir_all(&jsonl_dir).unwrap();

        let db_path = db_dir.join("custom.sqlite");
        let jsonl_path = jsonl_dir.join("issues.jsonl");
        let jsonl_f = fs::File::create(&jsonl_path).unwrap();
        jsonl_f.set_len(DB_BLOAT_MIN_JSONL_BYTES + 1024).unwrap();
        drop(jsonl_f);
        let db_f = fs::File::create(&db_path).unwrap();
        db_f.set_len((DB_BLOAT_MIN_JSONL_BYTES + 1024) * 20)
            .unwrap();
        drop(db_f);

        let mut checks = Vec::new();
        check_db_bloat_vs_jsonl(&db_path, Some(&jsonl_path), &mut checks);
        let check = find_check(&checks, "db_bloat").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
        assert_eq!(
            check
                .details
                .as_ref()
                .and_then(|details| details.get("db_path"))
                .and_then(serde_json::Value::as_str),
            Some(db_path.to_string_lossy().as_ref())
        );
        assert_eq!(
            check
                .details
                .as_ref()
                .and_then(|details| details.get("jsonl_path"))
                .and_then(serde_json::Value::as_str),
            Some(jsonl_path.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn test_fix_inner_gitignore_creates_when_missing() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        // No .gitignore at all → fixer creates it with both canonical patterns.

        let mut report = DoctorReport {
            ok: false,
            workspace_health: Some("degraded".to_string()),
            reliability_audit: None,
            checks: Vec::new(),
        };
        check_inner_gitignore_present(&beads_dir, &mut report.checks);

        let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
            .expect("session must build");
        let ctx = OutputContext::from_output_format(crate::cli::OutputFormat::Text, false, true);
        assert!(fix_inner_gitignore_if_warned(
            &beads_dir,
            &report,
            &ctx,
            Some(&mut session),
        ));
        let after = fs::read_to_string(beads_dir.join(".gitignore")).unwrap();
        assert!(
            after.lines().any(|l| l.trim() == ".write.lock"),
            "{after:?}"
        );
        assert!(after.lines().any(|l| l.trim() == "*.tmp"), "{after:?}");
    }

    #[test]
    fn test_fix_inner_gitignore_appends_when_incomplete() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        // .gitignore has one canonical pattern + a custom one; missing *.tmp.
        fs::write(
            beads_dir.join(".gitignore"),
            "# operator custom\n.write.lock\nlocal-cache/\n",
        )
        .unwrap();

        let mut report = DoctorReport {
            ok: false,
            workspace_health: Some("degraded".to_string()),
            reliability_audit: None,
            checks: Vec::new(),
        };
        check_inner_gitignore_present(&beads_dir, &mut report.checks);

        let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
            .expect("session must build");
        let ctx = OutputContext::from_output_format(crate::cli::OutputFormat::Text, false, true);
        assert!(fix_inner_gitignore_if_warned(
            &beads_dir,
            &report,
            &ctx,
            Some(&mut session),
        ));
        let after = fs::read_to_string(beads_dir.join(".gitignore")).unwrap();
        // Existing operator lines preserved verbatim.
        assert!(after.contains("# operator custom"), "{after:?}");
        assert!(after.contains("local-cache/"), "{after:?}");
        // Both canonical patterns now present.
        assert!(
            after.lines().any(|l| l.trim() == ".write.lock"),
            "{after:?}"
        );
        assert!(after.lines().any(|l| l.trim() == "*.tmp"), "{after:?}");
    }

    #[test]
    fn test_fix_inner_gitignore_refuses_symlink() {
        #[cfg(unix)]
        {
            let temp = TempDir::new().unwrap();
            let beads_dir = temp.path().join(".beads");
            fs::create_dir_all(&beads_dir).unwrap();
            // Make .beads/.gitignore a symlink to an external file.
            let external = temp.path().join("external.gitignore");
            fs::write(&external, ".write.lock\n").unwrap();
            std::os::unix::fs::symlink(&external, beads_dir.join(".gitignore")).unwrap();

            let mut report = DoctorReport {
                ok: false,
                workspace_health: Some("degraded".to_string()),
                reliability_audit: None,
                checks: Vec::new(),
            };
            check_inner_gitignore_present(&beads_dir, &mut report.checks);

            let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
                .expect("session must build");
            let ctx =
                OutputContext::from_output_format(crate::cli::OutputFormat::Text, false, true);
            // Detector warned for symlink, but fixer must refuse — operator
            // intent (compliance, vendored share) cannot be stomped.
            assert!(!fix_inner_gitignore_if_warned(
                &beads_dir,
                &report,
                &ctx,
                Some(&mut session),
            ));
            // Symlink still in place pointing at the same target.
            let after_meta = fs::symlink_metadata(beads_dir.join(".gitignore")).unwrap();
            assert!(after_meta.file_type().is_symlink());
        }
    }

    /// GitHub #403: a group/other-accessible fsqlite namespace sidecar wedges
    /// every database open while doctor reported the workspace healthy and
    /// said nothing at all. The check must name the file and its mode, and
    /// `--repair` must restore owner-only permissions through the chokepoint.
    #[cfg(unix)]
    #[test]
    fn test_db_sidecar_mode_check_flags_and_repairs_over_permissive_sidecar() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");
        fs::write(&db_path, b"SQLite format 3\0").unwrap();
        let gate = beads_dir.join("beads.db-fsqlite-ns-gate");
        let use_file = beads_dir.join("beads.db-fsqlite-ns-use");
        fs::write(&gate, b"FSQLNS01").unwrap();
        fs::write(&use_file, b"FSQLNS01").unwrap();
        fs::set_permissions(&gate, fs::Permissions::from_mode(0o664)).unwrap();
        fs::set_permissions(&use_file, fs::Permissions::from_mode(0o600)).unwrap();

        let mut report = DoctorReport {
            ok: false,
            workspace_health: Some("degraded".to_string()),
            reliability_audit: None,
            checks: Vec::new(),
        };
        check_db_sidecar_modes(&db_path, &mut report.checks);
        let check = find_check(&report.checks, "permissions.db_sidecars").expect("check present");
        assert_eq!(check.status, CheckStatus::Warn);
        let message = check.message.clone().unwrap_or_default();
        assert!(
            message.contains("beads.db-fsqlite-ns-gate") && message.contains("0664"),
            "message must name the sidecar and its mode: {message}"
        );
        assert!(
            !message.contains("beads.db-fsqlite-ns-use"),
            "the owner-only sidecar must not be flagged: {message}"
        );

        let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
            .expect("session must build");
        let ctx = OutputContext::from_output_format(crate::cli::OutputFormat::Text, false, true);
        assert!(fix_db_sidecar_modes_if_warned(
            &db_path,
            &report,
            &ctx,
            Some(&mut session),
        ));
        let repaired = fs::metadata(&gate).unwrap().permissions().mode() & 0o777;
        assert_eq!(repaired & 0o077, 0, "group/other bits must be cleared");
        assert_eq!(repaired, 0o600, "owner bits preserved, group/other removed");

        // Re-detect is clean, and a second repair is a no-op.
        let mut after = Vec::new();
        check_db_sidecar_modes(&db_path, &mut after);
        assert_eq!(
            find_check(&after, "permissions.db_sidecars")
                .expect("check present")
                .status,
            CheckStatus::Ok
        );
        assert!(!fix_db_sidecar_modes_if_warned(
            &db_path,
            &report,
            &ctx,
            Some(&mut session),
        ));
    }

    #[cfg(unix)]
    #[test]
    fn test_fix_config_yaml_secret_mode_chmods_via_chokepoint() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let config = beads_dir.join("config.yaml");
        fs::write(&config, b"github_token: abc123\nfoo: bar\n").unwrap();
        // World-readable + world-writable; contains secret-shaped keyword.
        fs::set_permissions(&config, fs::Permissions::from_mode(0o666)).unwrap();
        let before = fs::metadata(&config).unwrap().permissions().mode() & 0o777;

        let mut report = DoctorReport {
            ok: false,
            workspace_health: Some("degraded".to_string()),
            reliability_audit: None,
            checks: Vec::new(),
        };
        check_config_yaml_secret_mode(&beads_dir, &mut report.checks);

        let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
            .expect("session must build");
        let ctx = OutputContext::from_output_format(crate::cli::OutputFormat::Text, false, true);
        assert!(fix_config_yaml_secret_mode_if_warned(
            &beads_dir,
            &report,
            &ctx,
            Some(&mut session),
        ));
        let after = fs::metadata(&config).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            after & 0o006,
            0,
            "world-read/write bits must be cleared (mode {after:o})"
        );
        assert_eq!(after, before & !0o006, "only world bits removed");
    }

    #[cfg(unix)]
    #[test]
    fn test_fix_config_yaml_secret_mode_noop_when_no_warning() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let config = beads_dir.join("config.yaml");
        fs::write(&config, b"foo: bar\n").unwrap();
        // World-readable but NO secrets → no warn → no fix.
        fs::set_permissions(&config, fs::Permissions::from_mode(0o644)).unwrap();

        let mut report = DoctorReport {
            ok: true,
            workspace_health: Some("healthy".to_string()),
            reliability_audit: None,
            checks: Vec::new(),
        };
        check_config_yaml_secret_mode(&beads_dir, &mut report.checks);

        let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
            .expect("session must build");
        let ctx = OutputContext::from_output_format(crate::cli::OutputFormat::Text, false, true);
        assert!(!fix_config_yaml_secret_mode_if_warned(
            &beads_dir,
            &report,
            &ctx,
            Some(&mut session),
        ));
        let after = fs::metadata(&config).unwrap().permissions().mode() & 0o777;
        assert_eq!(after, 0o644, "mode untouched when no warning");
    }

    #[cfg(unix)]
    #[test]
    fn test_fix_jsonl_world_writable_chmods_via_chokepoint() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let jsonl = beads_dir.join("issues.jsonl");
        fs::write(&jsonl, b"{\"id\":\"bd-1\"}\n").unwrap();
        // Set world-writable mode.
        fs::set_permissions(&jsonl, fs::Permissions::from_mode(0o666)).unwrap();
        let before = fs::metadata(&jsonl).unwrap().permissions().mode() & 0o777;
        assert_ne!(before & 0o002, 0, "precondition: world-write bit set");

        let mut report = DoctorReport {
            ok: false,
            workspace_health: Some("degraded".to_string()),
            reliability_audit: None,
            checks: Vec::new(),
        };
        check_jsonl_world_writable(Some(&jsonl), &mut report.checks);

        let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
            .expect("session must build");
        let ctx = OutputContext::from_output_format(crate::cli::OutputFormat::Text, false, true);
        assert!(fix_jsonl_world_writable_if_warned(
            Some(&jsonl),
            &report,
            &ctx,
            Some(&mut session),
        ));
        let after = fs::metadata(&jsonl).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            after & 0o002,
            0,
            "world-write bit must be cleared (mode {after:o})"
        );
        // Owner/group bits preserved: only the world-write bit changed.
        assert_eq!(after, before & !0o002, "only world-write bit removed");
    }

    #[cfg(unix)]
    #[test]
    fn test_fix_jsonl_world_writable_allows_selected_in_workspace_jsonl() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        let jsonl_dir = temp.path().join("external");
        fs::create_dir_all(&jsonl_dir).unwrap();
        let jsonl = jsonl_dir.join("issues.jsonl");
        fs::write(&jsonl, b"{\"id\":\"bd-external\"}\n").unwrap();
        fs::set_permissions(&jsonl, fs::Permissions::from_mode(0o666)).unwrap();

        let mut report = DoctorReport {
            ok: false,
            workspace_health: Some("degraded".to_string()),
            reliability_audit: None,
            checks: Vec::new(),
        };
        check_jsonl_world_writable(Some(&jsonl), &mut report.checks);

        let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
            .expect("session must build");
        assert!(fix_jsonl_world_writable_if_warned(
            Some(&jsonl),
            &report,
            &quiet_ctx(),
            Some(&mut session),
        ));
        let after = fs::metadata(&jsonl).unwrap().permissions().mode() & 0o777;
        assert_eq!(after, 0o664, "only the world-write bit is stripped");
    }

    #[cfg(unix)]
    #[test]
    fn test_fix_jsonl_world_writable_noop_when_clean() {
        use std::os::unix::fs::PermissionsExt;
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let jsonl = beads_dir.join("issues.jsonl");
        fs::write(&jsonl, b"{\"id\":\"bd-clean\"}\n").unwrap();
        fs::set_permissions(&jsonl, fs::Permissions::from_mode(0o644)).unwrap();

        let mut report = DoctorReport {
            ok: true,
            workspace_health: Some("healthy".to_string()),
            reliability_audit: None,
            checks: Vec::new(),
        };
        check_jsonl_world_writable(Some(&jsonl), &mut report.checks);

        let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
            .expect("session must build");
        let ctx = OutputContext::from_output_format(crate::cli::OutputFormat::Text, false, true);
        assert!(!fix_jsonl_world_writable_if_warned(
            Some(&jsonl),
            &report,
            &ctx,
            Some(&mut session),
        ));
        let after = fs::metadata(&jsonl).unwrap().permissions().mode() & 0o777;
        assert_eq!(after, 0o644, "mode untouched when no warning");
    }

    #[test]
    fn test_fix_jsonl_crlf_converts_via_chokepoint() {
        // The fixer must rewrite the file converting CRLF→LF and
        // preserve every other byte verbatim.
        let temp = TempDir::new().unwrap();
        let jsonl_dir = temp.path().join("external");
        fs::create_dir_all(&jsonl_dir).unwrap();
        let jsonl_path = jsonl_dir.join("issues.jsonl");
        let mixed = b"{\"id\":\"bd-1\"}\r\n{\"id\":\"bd-2\"}\r\nfinal-no-eol";
        fs::write(&jsonl_path, mixed).unwrap();

        let mut report = DoctorReport {
            ok: false,
            workspace_health: Some("degraded".to_string()),
            reliability_audit: None,
            checks: Vec::new(),
        };
        check_jsonl_crlf_endings(Some(&jsonl_path), &mut report.checks);

        let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
            .expect("session must build");
        let ctx = OutputContext::from_output_format(crate::cli::OutputFormat::Text, false, true);
        assert!(fix_jsonl_crlf_endings_if_warned(
            Some(&jsonl_path),
            &report,
            &ctx,
            Some(&mut session),
        ));
        let after = fs::read(&jsonl_path).unwrap();
        assert_eq!(
            after, b"{\"id\":\"bd-1\"}\n{\"id\":\"bd-2\"}\nfinal-no-eol",
            "CRLF converted to LF; payload otherwise preserved"
        );
        assert!(
            session
                .run
                .root
                .join("backups/external/issues.jsonl")
                .is_file(),
            "selected in-workspace JSONL should be backed up relative to repo root"
        );
    }

    #[test]
    fn test_fix_jsonl_crlf_noop_when_clean() {
        // No CRLF in the warned check → fixer must not mutate.
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let clean = b"{\"id\":\"bd-clean\"}\n";
        fs::write(beads_dir.join("issues.jsonl"), clean).unwrap();

        let mut report = DoctorReport {
            ok: true,
            workspace_health: Some("healthy".to_string()),
            reliability_audit: None,
            checks: Vec::new(),
        };
        let jsonl_path = beads_dir.join("issues.jsonl");
        check_jsonl_crlf_endings(Some(&jsonl_path), &mut report.checks);

        let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
            .expect("session must build");
        let ctx = OutputContext::from_output_format(crate::cli::OutputFormat::Text, false, true);
        // No "warn" → returns false without touching the file.
        assert!(!fix_jsonl_crlf_endings_if_warned(
            Some(&jsonl_path),
            &report,
            &ctx,
            Some(&mut session),
        ));
        let after = fs::read(beads_dir.join("issues.jsonl")).unwrap();
        assert_eq!(after, clean, "file untouched when no warning");
    }

    #[test]
    fn test_fix_jsonl_crlf_skips_traversal_outside_workspace() {
        let parent = TempDir::new().unwrap();
        let repo = parent.path().join("repo");
        let outside = parent.path().join("outside");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let outside_jsonl = outside.join("issues.jsonl");
        let original = b"{\"id\":\"bd-outside\"}\r\n";
        fs::write(&outside_jsonl, original).unwrap();
        let traversal_path = repo.join("../outside/issues.jsonl");
        let report = DoctorReport {
            ok: false,
            workspace_health: None,
            reliability_audit: None,
            checks: vec![CheckResult {
                name: "jsonl_crlf".to_string(),
                status: CheckStatus::Warn,
                message: None,
                details: None,
            }],
        };
        let mut session = DoctorRepairSession::new(&repo, /* dry_run = */ false).expect("session");

        assert!(!fix_jsonl_crlf_endings_if_warned(
            Some(&traversal_path),
            &report,
            &quiet_ctx(),
            Some(&mut session),
        ));
        assert_eq!(
            fs::read(&outside_jsonl).unwrap(),
            original,
            "traversal-selected outside JSONL must not be rewritten"
        );
        assert_eq!(
            fs::read_to_string(&session.run.actions_file).unwrap(),
            "",
            "skipped traversal repairs must not write an undo action"
        );
    }

    #[test]
    fn test_fix_wal_oversized_truncates_valid_wal_via_chokepoint() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");
        let wal_writer = create_valid_oversized_wal(&db_path);
        wal_writer.close().unwrap();

        let mut report = DoctorReport {
            ok: false,
            workspace_health: Some("degraded".to_string()),
            reliability_audit: None,
            checks: Vec::new(),
        };
        check_wal_oversized(&db_path, &mut report.checks);
        assert!(
            report
                .checks
                .iter()
                .any(|c| c.name == "wal_size" && matches!(c.status, CheckStatus::Warn))
        );

        let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
            .expect("session must build");
        let ctx = OutputContext::from_output_format(crate::cli::OutputFormat::Text, false, true);
        assert!(fix_wal_oversized_if_warned(
            &db_path,
            &report,
            &ctx,
            Some(&mut session),
        ));

        let mut after = Vec::new();
        check_wal_oversized(&db_path, &mut after);
        let check = find_check(&after, "wal_size").expect("check present");
        assert!(
            matches!(check.status, CheckStatus::Ok),
            "WAL must be at-or-below threshold after PRAGMA wal_checkpoint(TRUNCATE): {check:?}"
        );
    }

    #[test]
    fn test_truncate_oversized_regular_wal_if_needed_shrinks_padded_sidecar() {
        let temp = TempDir::new().unwrap();
        let wal_path = temp.path().join("beads.db-wal");
        fs::File::create(&wal_path)
            .unwrap()
            .set_len(WAL_OVERSIZED_BYTES + 1)
            .unwrap();

        truncate_oversized_regular_wal_if_needed(&wal_path).unwrap();

        assert!(
            fs::metadata(&wal_path).map_or(0, |meta| meta.len()) <= WAL_OVERSIZED_BYTES,
            "complete checkpoint fallback must shrink padded WAL sidecars"
        );
    }

    fn create_valid_oversized_wal(db_path: &Path) -> Connection {
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        conn.execute("PRAGMA journal_mode = WAL").unwrap();
        conn.execute("PRAGMA wal_autocheckpoint = 0").unwrap();
        conn.execute("CREATE TABLE wal_growth (id INTEGER PRIMARY KEY, payload TEXT)")
            .unwrap();

        let payload = "x".repeat(1024 * 1024);
        let wal_path = sqlite_wal_sidecar_path(db_path);
        for _ in 0..(WAL_OVERSIZED_BYTES / (1024 * 1024) + 4) {
            conn.execute_with_params(
                "INSERT INTO wal_growth(payload) VALUES (?1)",
                &[SqliteValue::Text(payload.as_str().into())],
            )
            .unwrap();
            if fs::metadata(&wal_path).map_or(0, |meta| meta.len()) > WAL_OVERSIZED_BYTES {
                break;
            }
        }

        let wal_size = fs::metadata(&wal_path).map_or(0, |meta| meta.len());
        assert!(
            wal_size > WAL_OVERSIZED_BYTES,
            "test setup must create a valid oversized WAL, got {wal_size} bytes"
        );
        conn
    }

    #[test]
    fn test_fix_wal_oversized_noop_when_no_warning() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");
        let _storage = SqliteStorage::open(&db_path).unwrap();

        let mut report = DoctorReport {
            ok: true,
            workspace_health: Some("healthy".to_string()),
            reliability_audit: None,
            checks: Vec::new(),
        };
        check_wal_oversized(&db_path, &mut report.checks);

        let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
            .expect("session must build");
        let ctx = OutputContext::from_output_format(crate::cli::OutputFormat::Text, false, true);
        assert!(!fix_wal_oversized_if_warned(
            &db_path,
            &report,
            &ctx,
            Some(&mut session),
        ));
    }

    #[test]
    fn test_check_wal_oversized_warns_on_oversized() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let wal = fs::File::create(beads_dir.join("beads.db-wal")).unwrap();
        wal.set_len(WAL_OVERSIZED_BYTES + 1).unwrap();
        drop(wal);

        let mut checks = Vec::new();
        check_wal_oversized(&beads_dir.join("beads.db"), &mut checks);
        let check = find_check(&checks, "wal_size").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
        let details = check.details.as_ref().expect("details");
        assert_eq!(
            details
                .get("threshold_bytes")
                .and_then(serde_json::Value::as_u64),
            Some(WAL_OVERSIZED_BYTES)
        );
    }

    #[test]
    fn test_check_wal_oversized_ok_on_normal_size() {
        // 4MB WAL is healthy — SQLite auto-checkpoint runs at ~4MB.
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let wal = fs::File::create(beads_dir.join("beads.db-wal")).unwrap();
        wal.set_len(4 * 1024 * 1024).unwrap();
        drop(wal);

        let mut checks = Vec::new();
        check_wal_oversized(&beads_dir.join("beads.db"), &mut checks);
        let check = find_check(&checks, "wal_size").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    #[test]
    fn test_check_wal_oversized_missing_is_ok() {
        // No WAL file at all → ok (workspace not in WAL mode or freshly checkpointed).
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let mut checks = Vec::new();
        check_wal_oversized(&beads_dir.join("beads.db"), &mut checks);
        let check = find_check(&checks, "wal_size").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    #[test]
    fn test_sqlite_wal_sidecar_path_preserves_non_utf8_bytes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::{OsStrExt, OsStringExt};

        let db_path = PathBuf::from(OsString::from_vec(b"/tmp/br-\xFF.sqlite".to_vec()));
        let wal_path = sqlite_wal_sidecar_path(&db_path);

        assert_eq!(wal_path.as_os_str().as_bytes(), b"/tmp/br-\xFF.sqlite-wal");
    }

    #[test]
    fn test_check_wal_oversized_uses_selected_db_sidecar_path() {
        let temp = TempDir::new().unwrap();
        let db_dir = temp.path().join("db");
        fs::create_dir_all(&db_dir).unwrap();
        let db_path = db_dir.join("custom.sqlite");
        let wal_path = sqlite_wal_sidecar_path(&db_path);
        let wal = fs::File::create(&wal_path).unwrap();
        wal.set_len(WAL_OVERSIZED_BYTES + 1).unwrap();
        drop(wal);

        let mut checks = Vec::new();
        check_wal_oversized(&db_path, &mut checks);
        let check = find_check(&checks, "wal_size").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
        assert_eq!(
            check
                .details
                .as_ref()
                .and_then(|details| details.get("path"))
                .and_then(serde_json::Value::as_str),
            Some(wal_path.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn test_check_jsonl_oversized_missing_is_ok() {
        // No JSONL at all → ok (other checks cover missing-file case).
        let mut checks = Vec::new();
        check_jsonl_oversized(None, &mut checks);
        let check = find_check(&checks, "jsonl_size").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    #[test]
    fn test_check_jsonl_oversized_uses_selected_external_path() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let external_dir = temp.path().join("external");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&external_dir).unwrap();
        fs::write(beads_dir.join("issues.jsonl"), b"{\"id\":\"bd-small\"}\n").unwrap();
        let external_jsonl = external_dir.join("issues.jsonl");
        let f = fs::File::create(&external_jsonl).unwrap();
        f.set_len(JSONL_OVERSIZED_THRESHOLD_BYTES + 1).unwrap();
        drop(f);

        let mut checks = Vec::new();
        check_jsonl_oversized(Some(&external_jsonl), &mut checks);
        let check = find_check(&checks, "jsonl_size").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
        assert_eq!(
            check
                .details
                .as_ref()
                .and_then(|d| d.get("path"))
                .and_then(serde_json::Value::as_str),
            Some(external_jsonl.to_string_lossy().as_ref())
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_check_orphan_tmp_files_ignores_symlinked_tmp_names() {
        // A symlink named like an atomic-write tmp must not be reported
        // based on its target's mtime. The detector is scoped to real
        // files under `.beads/`, not external targets reached through
        // symlinks.
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let target = temp.path().join("outside-target");
        fs::write(&target, b"old outside target").unwrap();
        let two_hours_ago =
            std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 60 * 60);
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&target)
            .unwrap();
        f.set_times(std::fs::FileTimes::new().set_modified(two_hours_ago))
            .unwrap();
        drop(f);
        symlink(&target, beads_dir.join("issues.jsonl.99999.tmp")).unwrap();

        let mut checks = Vec::new();
        check_orphan_tmp_files(&beads_dir, &mut checks);
        let check = find_check(&checks, "tmp_files_orphan").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok), "{check:?}");
    }

    #[test]
    fn test_check_orphan_tmp_files_reports_files_sorted() {
        // Stable robot output matters for agents diffing doctor results.
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let two_hours_ago =
            std::time::SystemTime::now() - std::time::Duration::from_secs(2 * 60 * 60);
        for name in ["z.tmp", "a.tmp"] {
            let tmp_path = beads_dir.join(name);
            fs::write(&tmp_path, b"partial write").unwrap();
            let f = std::fs::OpenOptions::new()
                .write(true)
                .open(&tmp_path)
                .unwrap();
            f.set_times(std::fs::FileTimes::new().set_modified(two_hours_ago))
                .unwrap();
        }

        let mut checks = Vec::new();
        check_orphan_tmp_files(&beads_dir, &mut checks);
        let check = find_check(&checks, "tmp_files_orphan").expect("check present");
        let files: Vec<&str> = check
            .details
            .as_ref()
            .and_then(|d| d.get("files"))
            .and_then(|v| v.as_array())
            .expect("files array")
            .iter()
            .map(|v| v.as_str().expect("file name string"))
            .collect();
        assert_eq!(files, ["a.tmp", "z.tmp"]);
    }

    #[test]
    fn test_fix_orphan_tmp_files_quarantines_old_regular_files_only() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let old_tmp = beads_dir.join("issues.jsonl.99999.tmp");
        let fresh_tmp = beads_dir.join("issues.jsonl.12345.tmp");
        fs::write(&old_tmp, b"old partial write").unwrap();
        fs::write(&fresh_tmp, b"fresh partial write").unwrap();
        backdate_file_two_hours(&old_tmp);

        let report = DoctorReport {
            ok: false,
            workspace_health: None,
            reliability_audit: None,
            checks: vec![CheckResult {
                name: "tmp_files_orphan".to_string(),
                status: CheckStatus::Warn,
                message: None,
                details: None,
            }],
        };
        let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
            .expect("session must build");

        assert!(fix_orphan_tmp_files_if_warned(
            &beads_dir,
            &report,
            &quiet_ctx(),
            Some(&mut session),
        ));
        assert!(!old_tmp.exists(), "old tmp must be moved into quarantine");
        assert!(fresh_tmp.is_file(), "fresh tmp must be preserved in place");
        assert!(
            session
                .run
                .root
                .join("quarantine/.beads/issues.jsonl.99999.tmp")
                .is_file()
        );

        let actions_before = fs::read_to_string(&session.run.actions_file).unwrap();
        assert_eq!(actions_before.matches("\"op\":\"rename\"").count(), 1);
        assert!(!fix_orphan_tmp_files_if_warned(
            &beads_dir,
            &report,
            &quiet_ctx(),
            Some(&mut session),
        ));
        let actions_after = fs::read_to_string(&session.run.actions_file).unwrap();
        assert_eq!(actions_after, actions_before, "second pass must be a no-op");
    }

    #[test]
    fn test_fix_orphan_tmp_files_ignores_symlinked_tmp_names() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let real_orphan = beads_dir.join("real.tmp");
        fs::write(&real_orphan, b"old regular tmp").unwrap();
        backdate_file_two_hours(&real_orphan);

        let symlink_target = beads_dir.join("target-data");
        let symlink_path = beads_dir.join("linked.tmp");
        fs::write(&symlink_target, b"old target behind symlink").unwrap();
        backdate_file_two_hours(&symlink_target);
        symlink(&symlink_target, &symlink_path).unwrap();

        let report = DoctorReport {
            ok: false,
            workspace_health: None,
            reliability_audit: None,
            checks: vec![CheckResult {
                name: "tmp_files_orphan".to_string(),
                status: CheckStatus::Warn,
                message: None,
                details: None,
            }],
        };
        let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
            .expect("session must build");

        assert!(fix_orphan_tmp_files_if_warned(
            &beads_dir,
            &report,
            &quiet_ctx(),
            Some(&mut session),
        ));

        assert!(
            !real_orphan.exists(),
            "regular orphan should be quarantined"
        );
        assert!(
            fs::symlink_metadata(&symlink_path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "symlink-shaped tmp names are not detector-owned and must stay in place"
        );
        assert!(
            symlink_target.is_file(),
            "the symlink target must not be modified"
        );
        assert!(
            !session
                .run
                .root
                .join("quarantine/.beads/linked.tmp")
                .exists(),
            "repair must not quarantine symlink-shaped tmp names"
        );

        let actions = fs::read_to_string(&session.run.actions_file).unwrap();
        assert_eq!(actions.matches("\"op\":\"rename\"").count(), 1);
    }

    #[test]
    fn test_fixer_filter_empty_allows_everything() {
        // Pass-5 cycle 1: default (no --only/--skip) accepts every FM.
        let filter = FixerFilter::default();
        assert!(filter.allows("fm-anything"));
        assert!(filter.allows("fm-state_files-merge-artifact-stuck"));
        assert!(!filter.has_only());
        assert!(!filter.has_skip());
    }

    #[test]
    fn test_fixer_filter_only_allowlist() {
        // --only A,B → A and B run; C is blocked.
        let filter = FixerFilter::from_args(&["fm-a".to_string(), "fm-b".to_string()], &[]);
        assert!(filter.allows("fm-a"));
        assert!(filter.allows("fm-b"));
        assert!(!filter.allows("fm-c"));
        assert!(filter.has_only());
    }

    #[test]
    fn test_fixer_filter_skip_blocklist() {
        // --skip X → X is blocked; everything else runs.
        let filter = FixerFilter::from_args(&[], &["fm-blocked".to_string()]);
        assert!(filter.allows("fm-other"));
        assert!(!filter.allows("fm-blocked"));
        assert!(filter.has_skip());
    }

    #[test]
    fn test_fixer_filter_skip_overrides_only() {
        // --only A --skip A → A is blocked (skip subtracts from only).
        let filter = FixerFilter::from_args(&["fm-a".to_string()], &["fm-a".to_string()]);
        assert!(!filter.allows("fm-a"));
    }

    #[test]
    fn test_fixer_filter_trims_whitespace_and_drops_empties() {
        // Whitespace + empty strings from comma-split are normalized
        // away so `--only ,fm-a, ,fm-b,` works.
        let filter = FixerFilter::from_args(
            &["  fm-a  ".to_string(), String::new(), " fm-b".to_string()],
            &[],
        );
        assert!(filter.allows("fm-a"));
        assert!(filter.allows("fm-b"));
        assert!(!filter.allows("fm-c"));
    }

    /// Property-based generalization of the FixerFilter contract
    /// (hardens blast_radius_containment: `--only`/`--skip` scoping is
    /// what keeps a targeted `--repair` from touching unrelated FMs).
    /// The example-based tests above pin specific cases; this cross-checks
    /// the implementation against an INDEPENDENT set-semantics reference
    /// over arbitrary only/skip/query combinations, drawn from a small id
    /// space so only/skip/query overlap frequently.
    mod filter_props {
        use super::*;
        use proptest::prelude::*;

        fn id_strategy() -> impl Strategy<Value = String> {
            proptest::sample::select(vec!["fm-a", "fm-b", "fm-c", "fm-d", "fm-e"])
                .prop_map(ToString::to_string)
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(128))]

            /// allows(q) == (only empty OR only contains q) AND NOT skip contains q.
            #[test]
            fn prop_filter_matches_set_semantics(
                only in proptest::collection::vec(id_strategy(), 0..5),
                skip in proptest::collection::vec(id_strategy(), 0..5),
                query in id_strategy(),
            ) {
                let filter = FixerFilter::from_args(&only, &skip);
                let only_set: std::collections::HashSet<&str> =
                    only.iter().map(String::as_str).collect();
                let skip_set: std::collections::HashSet<&str> =
                    skip.iter().map(String::as_str).collect();
                let expected = (only_set.is_empty() || only_set.contains(query.as_str()))
                    && !skip_set.contains(query.as_str());
                prop_assert_eq!(filter.allows(&query), expected);
            }

            /// skip ALWAYS wins: an id in skip is denied even when also in only.
            #[test]
            fn prop_filter_skip_always_overrides_only(query in id_strategy()) {
                let filter = FixerFilter::from_args(
                    std::slice::from_ref(&query),
                    std::slice::from_ref(&query),
                );
                prop_assert!(!filter.allows(&query));
            }

            /// Whitespace padding and interleaved empty strings are
            /// normalized away and do not change the decision.
            #[test]
            fn prop_filter_normalizes_whitespace(base in id_strategy()) {
                let clean = FixerFilter::from_args(std::slice::from_ref(&base), &[]);
                let messy = FixerFilter::from_args(
                    &[String::new(), format!("  {base}  "), "   ".to_string()],
                    &[],
                );
                prop_assert!(messy.allows(&base));
                prop_assert_eq!(clean.allows(&base), messy.allows(&base));
                // An id NOT in the (normalized) allowlist is still denied identically.
                let other = if base.as_str() == "fm-a" { "fm-b" } else { "fm-a" };
                prop_assert_eq!(clean.allows(other), messy.allows(other));
            }
        }
    }

    /// Drift gate (beads_rust-oow2): every `fm-` id the runtime fixer
    /// gates consult via `FixerFilter::allows` must be advertised in the
    /// capabilities envelope's `fixers[].filter_ids` — that field is the
    /// documented vocabulary for `--only`/`--skip`, and an unadvertised
    /// gate id is undiscoverable (its fixer is silently disabled by any
    /// `--only` list with no way to name it back in; historically this
    /// hid `fm-caches_indexes-partial-index-stale` entirely). And the
    /// reverse: every advertised filter id must actually be consulted by
    /// a gate, so the envelope cannot advertise vocabulary the runtime
    /// ignores.
    #[test]
    fn every_fixer_filter_gate_id_is_advertised_in_capabilities() {
        use std::collections::{HashMap, HashSet};

        let caps =
            crate::cli::commands::doctor_subsystems::capabilities_doctor::DoctorCapabilities::build(
            );
        let advertised: HashSet<&str> = caps
            .fixers
            .iter()
            .flat_map(|fixer| fixer.filter_ids.iter().map(String::as_str))
            .collect();

        // Scan the runtime half of this file (everything before the test
        // module) for the ids the gates actually consult.
        let source = include_str!("doctor.rs");
        let runtime = source
            .split("#[cfg(all(test, unix))]")
            .next()
            .expect("split never yields zero items");

        // Resolve `const FM_*` definitions to their string values.
        let mut const_values: HashMap<String, &str> = HashMap::new();
        for line in runtime.lines() {
            let trimmed = line.trim_start();
            let Some(rest) = trimmed.strip_prefix("const FM_") else {
                continue;
            };
            let Some((name, value_rest)) = rest.split_once(':') else {
                continue;
            };
            let mut quoted = value_rest.split('"');
            let _ = quoted.next();
            if let Some(value) = quoted.next() {
                const_values.insert(format!("FM_{}", name.trim()), value);
            }
        }

        let mut gate_ids: HashSet<String> = HashSet::new();
        for line in runtime.lines() {
            let mut rest = line;
            while let Some(pos) = rest.find("allows(") {
                let after = &rest[pos + "allows(".len()..];
                let Some(end) = after.find(')') else { break };
                let arg = after[..end].trim();
                if let Some(literal) = arg.strip_prefix('"').and_then(|a| a.strip_suffix('"')) {
                    gate_ids.insert(literal.to_string());
                } else if let Some(value) = const_values.get(arg) {
                    gate_ids.insert((*value).to_string());
                }
                // Bare closure vars (the `.any(|fm| filter.allows(fm))`
                // shape) carry no id here; the array scan below covers
                // the constants such closures iterate.
                rest = &after[end..];
            }
        }

        // `filter_allows_jsonl_rebuild` consults an ARRAY of FM_
        // constants via a closure; pull its idents from the function
        // body (everything up to the `.iter()` that starts the closure).
        let rebuild_body = runtime
            .split("fn filter_allows_jsonl_rebuild")
            .nth(1)
            .expect("filter_allows_jsonl_rebuild present in runtime source");
        let rebuild_array = rebuild_body
            .split(".iter()")
            .next()
            .expect("split never yields zero items");
        for token in rebuild_array.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')) {
            if let Some(value) = const_values.get(token) {
                gate_ids.insert((*value).to_string());
            }
        }

        assert!(
            gate_ids.len() >= 20,
            "suspiciously few gate ids found by the source scan ({}); did the scan break?",
            gate_ids.len()
        );

        let mut unadvertised: Vec<&String> = gate_ids
            .iter()
            .filter(|id| !advertised.contains(id.as_str()))
            .collect();
        unadvertised.sort();
        assert!(
            unadvertised.is_empty(),
            "fixer gate id(s) consulted by FixerFilter::allows but missing from \
             capabilities fixers[].filter_ids: {unadvertised:?}"
        );

        let mut unconsulted: Vec<&&str> = advertised
            .iter()
            .filter(|id| !gate_ids.contains(**id))
            .collect();
        unconsulted.sort();
        assert!(
            unconsulted.is_empty(),
            "capabilities fixers[].filter_ids advertise id(s) no runtime gate \
             consults: {unconsulted:?}"
        );
    }

    #[test]
    fn test_recoverable_db_state_filter_preserves_sidecar_fm() {
        let sidecar_only = FixerFilter::from_args(&[FM_WAL_SHM_SIDECAR_ORPHAN.to_string()], &[]);
        assert!(filter_allows_recoverable_db_state_repair(
            &sidecar_only,
            false,
            true
        ));
        assert!(!filter_allows_recoverable_db_state_repair(
            &sidecar_only,
            true,
            false
        ));

        let blocked_cache_only = FixerFilter::from_args(&[FM_BLOCKED_CACHE_STALE.to_string()], &[]);
        assert!(filter_allows_recoverable_db_state_repair(
            &blocked_cache_only,
            true,
            false
        ));
        assert!(!filter_allows_recoverable_db_state_repair(
            &blocked_cache_only,
            false,
            true
        ));

        let skip_sidecar = FixerFilter::from_args(&[], &[FM_WAL_SHM_SIDECAR_ORPHAN.to_string()]);
        assert!(!filter_allows_recoverable_db_state_repair(
            &skip_sidecar,
            false,
            true
        ));
    }

    #[test]
    fn test_jsonl_rebuild_filter_matches_addressed_fms() {
        for fm in [
            FM_JSONL_ROW_COUNT_MISMATCH,
            FM_EMPTY_OR_TRUNCATED_DATABASE,
            FM_SQLITE_PAGE_MALFORMED,
            FM_MISSING_REQUIRED_TABLE,
            FM_MISSING_REQUIRED_COLUMN,
            FM_BLOCKED_CACHE_STALE,
        ] {
            let filter = FixerFilter::from_args(&[fm.to_string()], &[]);
            assert!(
                filter_allows_jsonl_rebuild(&filter),
                "JSONL rebuild should run for addressed FM {fm}"
            );
        }

        let sidecar_only = FixerFilter::from_args(&[FM_WAL_SHM_SIDECAR_ORPHAN.to_string()], &[]);
        assert!(!filter_allows_jsonl_rebuild(&sidecar_only));

        let skip_page = FixerFilter::from_args(&[], &[FM_SQLITE_PAGE_MALFORMED.to_string()]);
        assert!(filter_allows_jsonl_rebuild(&skip_page));

        let only_page_skip_page = FixerFilter::from_args(
            &[FM_SQLITE_PAGE_MALFORMED.to_string()],
            &[FM_SQLITE_PAGE_MALFORMED.to_string()],
        );
        assert!(!filter_allows_jsonl_rebuild(&only_page_skip_page));
    }

    #[test]
    fn test_fix_merge_artifacts_quarantines_via_chokepoint() {
        // Pass-4 cycle 1: the fixer must MOVE stuck artifacts into the
        // run-dir quarantine via Op::Rename (never delete), and the
        // post-repair detector must report ok.
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        let _storage = SqliteStorage::open(&db_path).unwrap();
        fs::write(
            &jsonl_path,
            format!(
                "{}\n",
                serde_json::to_string(&sample_issue("bd-test01", "Valid issue")).unwrap()
            ),
        )
        .unwrap();
        // Plant the merge artifacts.
        fs::write(beads_dir.join("issues.base.jsonl"), b"").unwrap();
        fs::write(beads_dir.join("issues.left.jsonl"), b"").unwrap();
        fs::write(beads_dir.join("issues.right.jsonl"), b"").unwrap();
        // The canonical anchor must NOT be touched.
        fs::write(beads_dir.join("beads.base.jsonl"), b"canonical-anchor").unwrap();

        let paths = config::ConfigPaths {
            beads_dir: beads_dir.clone(),
            db_path,
            jsonl_path,
            metadata: config::Metadata::default(),
        };

        let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
            .expect("session must build");
        let report_before = collect_doctor_report(&beads_dir, &paths).expect("doctor report");
        let before_check = find_check(&report_before.report.checks, "jsonl.merge_artifacts")
            .expect("merge_artifacts check");
        assert!(matches!(before_check.status, CheckStatus::Warn));

        let ctx = OutputContext::from_output_format(crate::cli::OutputFormat::Json, false, true);
        assert!(fix_merge_artifacts_if_warned(
            &beads_dir,
            &report_before.report,
            &ctx,
            Some(&mut session),
        ));

        // Source artifacts gone.
        assert!(!beads_dir.join("issues.base.jsonl").exists());
        assert!(!beads_dir.join("issues.left.jsonl").exists());
        assert!(!beads_dir.join("issues.right.jsonl").exists());
        // Canonical anchor untouched.
        assert_eq!(
            fs::read(beads_dir.join("beads.base.jsonl")).unwrap(),
            b"canonical-anchor"
        );
        // Quarantine populated.
        let q = session.run.root.join("quarantine/.beads");
        assert!(q.join("issues.base.jsonl").is_file());
        assert!(q.join("issues.left.jsonl").is_file());
        assert!(q.join("issues.right.jsonl").is_file());

        // actions.jsonl records three rename ops.
        let actions = fs::read_to_string(&session.run.actions_file).unwrap();
        let rename_count = actions
            .lines()
            .filter(|l| l.contains("\"op\":\"rename\""))
            .count();
        assert_eq!(rename_count, 3, "actions.jsonl: {actions}");

        // Re-detect: status returns to ok.
        let report_after = collect_doctor_report(&beads_dir, &paths).expect("doctor report");
        let after_check = find_check(&report_after.report.checks, "jsonl.merge_artifacts")
            .expect("merge_artifacts check");
        assert!(
            matches!(after_check.status, CheckStatus::Ok),
            "post-repair status must be ok, got {:?}",
            after_check.status
        );
    }

    #[test]
    fn test_merge_artifacts_warn_carries_classification_and_recovery() {
        // beads_rust#344/#328: the warn's details must classify each
        // flagged artifact, scan it for conflict markers, and point at the
        // canonical JSONL plus the structured recovery actions.
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        let _storage = SqliteStorage::open(&db_path).unwrap();
        fs::write(&jsonl_path, b"").unwrap();
        fs::write(beads_dir.join("issues.base.jsonl"), b"").unwrap();
        fs::write(
            beads_dir.join("issues.left.jsonl"),
            b"<<<<<<< HEAD\n{\"id\":\"bd-a\"}\n=======\n{\"id\":\"bd-b\"}\n>>>>>>> theirs\n",
        )
        .unwrap();
        fs::write(beads_dir.join("issues.right.jsonl"), b"{\"id\":\"bd-c\"}\n").unwrap();
        // The protected anchor must stay excluded from the details.
        fs::write(beads_dir.join("beads.base.jsonl"), b"canonical-anchor").unwrap();

        let mut checks = Vec::new();
        check_merge_artifacts(&beads_dir, &jsonl_path, &mut checks).expect("merge artifact scan");
        let check = find_check(&checks, "jsonl.merge_artifacts").expect("merge_artifacts check");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");

        let details = check.details.as_ref().expect("warn details");
        assert_eq!(
            details["canonical_jsonl"],
            serde_json::json!(jsonl_path.display().to_string())
        );
        let recovery = details["recovery"].as_array().expect("recovery actions");
        assert_eq!(recovery.len(), 2, "{details}");
        assert_eq!(recovery[0]["kind"], "quarantine");
        assert_eq!(recovery[0]["command"], "br doctor --repair");
        assert_eq!(recovery[1]["kind"], "undo");
        assert_eq!(recovery[1]["command"], "br doctor undo <run-id>");

        let artifacts = details["artifacts"].as_array().expect("artifact entries");
        assert_eq!(artifacts.len(), 3, "{details}");
        let by_kind = |kind: &str| {
            artifacts
                .iter()
                .find(|a| a["artifact_kind"] == kind)
                .unwrap_or_else(|| panic!("missing {kind} entry: {details}"))
        };
        let left = by_kind("merge-left");
        assert_eq!(left["conflict_markers_found"], true, "{details}");
        assert!(
            left["path"]
                .as_str()
                .is_some_and(|p| p.ends_with("issues.left.jsonl")),
            "{details}"
        );
        assert_eq!(by_kind("merge-right")["conflict_markers_found"], false);
        assert_eq!(
            by_kind("stale-base-variant")["conflict_markers_found"],
            false
        );
        assert!(
            !artifacts.iter().any(|a| a["path"]
                .as_str()
                .is_some_and(|p| p.ends_with("beads.base.jsonl"))),
            "protected anchor must not be classified as an artifact: {details}"
        );

        // Back-compat `files` list survives, and the finding_id wiring
        // stays on the canonical FM identifier.
        let listed = details["files"].as_array().expect("files list");
        assert_eq!(listed.len(), 3, "{details}");
        assert_eq!(details["finding_id"], "fm-state_files-merge-artifact-stuck");
    }

    #[test]
    fn test_fix_merge_artifacts_is_idempotent_no_op_on_second_call() {
        // The idempotence contract: a second --repair against a clean
        // workspace must find nothing to quarantine (no actions emitted).
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        let _storage = SqliteStorage::open(&db_path).unwrap();
        fs::write(&jsonl_path, b"").unwrap();

        let paths = config::ConfigPaths {
            beads_dir: beads_dir.clone(),
            db_path,
            jsonl_path,
            metadata: config::Metadata::default(),
        };
        let report = collect_doctor_report(&beads_dir, &paths).expect("doctor report");
        let ctx = OutputContext::from_output_format(crate::cli::OutputFormat::Json, false, true);
        let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
            .expect("session must build");

        // No artifacts present → no warning → fixer returns false.
        assert!(!fix_merge_artifacts_if_warned(
            &beads_dir,
            &report.report,
            &ctx,
            Some(&mut session),
        ));
        let actions = fs::read_to_string(&session.run.actions_file).unwrap_or_default();
        let rename_count = actions.matches("\"op\":\"rename\"").count();
        assert_eq!(rename_count, 0, "no actions expected: {actions:?}");
    }

    #[test]
    fn test_fix_startup_cache_quarantines_poisoned_files_via_chokepoint() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let cache_dir = temp.path().join("startup-cache");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&cache_dir).unwrap();

        let current_cache = config::doctor_startup_cache_path_at(&cache_dir, &beads_dir, None);
        fs::write(&current_cache, "not-json-at-all\n").unwrap();
        let unrelated_cache = cache_dir.join("startup-deadbeef.json");
        fs::write(&unrelated_cache, "also-not-json\n").unwrap();

        let poisoned = config::doctor_inspect_startup_cache_at(&cache_dir, &beads_dir, None);
        assert_eq!(poisoned.len(), 1);
        assert_eq!(poisoned[0].path, current_cache);

        let report = DoctorReport {
            ok: false,
            workspace_health: None,
            reliability_audit: None,
            checks: vec![CheckResult {
                name: "startup_cache.health".to_string(),
                status: CheckStatus::Warn,
                message: None,
                details: None,
            }],
        };
        let ctx = OutputContext::from_output_format(crate::cli::OutputFormat::Json, false, true);
        let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
            .expect("session must build");

        assert!(fix_startup_cache_entries_if_warned(
            &poisoned,
            &cache_dir,
            &report,
            &ctx,
            Some(&mut session),
        ));

        assert!(!current_cache.exists());
        assert!(
            unrelated_cache.is_file(),
            "unrelated cache key must not be quarantined"
        );
        assert!(
            session
                .run
                .root
                .join("quarantine/startup-cache")
                .join(current_cache.file_name().unwrap())
                .is_file()
        );

        let after = config::doctor_inspect_startup_cache_at(&cache_dir, &beads_dir, None);
        assert!(after.is_empty());
        let actions_before = fs::read_to_string(&session.run.actions_file).unwrap();
        assert!(!fix_startup_cache_entries_if_warned(
            &after,
            &cache_dir,
            &report,
            &ctx,
            Some(&mut session),
        ));
        let actions_after = fs::read_to_string(&session.run.actions_file).unwrap();
        assert_eq!(actions_after, actions_before, "second pass must be a no-op");
    }

    #[cfg(unix)]
    #[test]
    fn test_check_root_gitignore_warns_for_symlink() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let outside = TempDir::new().unwrap();
        let outside_gitignore = outside.path().join("gitignore-target");
        fs::write(&outside_gitignore, ".beads/\n").unwrap();
        symlink(&outside_gitignore, temp.path().join(".gitignore")).unwrap();

        let mut checks = Vec::new();
        check_root_gitignore(&beads_dir, &mut checks);

        let check = find_check(&checks, "gitignore.beads_inner").expect("gitignore check");
        assert!(matches!(check.status, CheckStatus::Warn));
        assert!(
            check
                .message
                .as_deref()
                .is_some_and(|message| message.contains("symlinked root .gitignore")),
            "warning should explain that symlinked .gitignore is unsupported: {check:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_fix_root_gitignore_if_warned_refuses_symlink_target() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let outside = TempDir::new().unwrap();
        let outside_gitignore = outside.path().join("gitignore-target");
        let original = ".beads/\nkeep-me\n";
        fs::write(&outside_gitignore, original).unwrap();
        let root_gitignore = temp.path().join(".gitignore");
        symlink(&outside_gitignore, &root_gitignore).unwrap();

        let mut checks = Vec::new();
        check_root_gitignore(&beads_dir, &mut checks);
        let report = DoctorReport {
            ok: true,
            workspace_health: None,
            reliability_audit: None,
            checks,
        };
        let ctx = OutputContext::from_output_format(crate::cli::OutputFormat::Json, false, true);

        assert!(!fix_root_gitignore_if_warned(
            &beads_dir, &report, &ctx, None
        ));
        assert_eq!(fs::read_to_string(&outside_gitignore).unwrap(), original);
        assert!(
            fs::symlink_metadata(&root_gitignore)
                .unwrap()
                .file_type()
                .is_symlink(),
            "doctor repair must leave the symlink itself untouched"
        );
    }

    #[test]
    fn test_repair_outcome_message_combines_gitignore_and_incomplete_reindex() {
        let message = repair_outcome_message_from_parts(
            vec![ROOT_GITIGNORE_REPAIR_MESSAGE.to_string()],
            Some(&LocalRepairResult::default()),
            Some(REINDEX_INCOMPLETE_MESSAGE),
        );

        assert!(message.contains(ROOT_GITIGNORE_REPAIR_MESSAGE));
        assert!(message.contains(REINDEX_INCOMPLETE_MESSAGE));
    }

    #[test]
    fn test_early_repair_summary_reports_export_hash_repairs() {
        let summary = EarlyRepairSummary {
            gitignore: false,
            merge_artifacts: false,
            startup_cache: false,
            recovery_aged: false,
            export_hash: true,
            base_jsonl_symlink: false,
            base_jsonl_stale: false,
            orphan_tmp: false,
            jsonl_eof_newline: false,
            jsonl_bom: false,
            jsonl_crlf: false,
            jsonl_world_writable: false,
            config_yaml_secret_mode: false,
            inner_gitignore: false,
            dirty_bitmap_orphans: false,
            comments_orphans: false,
            labels_orphans: false,
            dependencies_orphans: false,
            wal_checkpoint: false,
            null_defaults: false,
            db_bloat_vacuum: false,
        };

        assert!(summary.applied());
        assert_eq!(
            summary.action_labels(),
            vec!["export_hash_cache_recomputed".to_string()]
        );
        assert_eq!(
            repair_outcome_message_from_parts(summary.messages(), None, None),
            "Recomputed metadata.jsonl_content_hash."
        );
        let audit = summary.audit_record();
        assert_eq!(audit.phase, "doctor.early_repair");
        assert_eq!(audit.outcome, "export_hash_cache_recomputed");
        assert_eq!(
            audit.applied_actions,
            vec!["export_hash_cache_recomputed".to_string()]
        );

        let local_repair = LocalRepairResult {
            blocked_cache_rebuilt: true,
            ..LocalRepairResult::default()
        };
        let combined_audit = summary.prepend_actions_to_audit(local_repair_audit_record(
            "doctor.local_repair",
            "verified",
            &local_repair,
            None,
        ));
        assert_eq!(
            combined_audit.applied_actions,
            vec![
                "export_hash_cache_recomputed".to_string(),
                "blocked_cache_rebuilt".to_string()
            ]
        );
    }

    #[test]
    fn test_early_repair_summary_reports_db_bloat_vacuum() {
        let summary = EarlyRepairSummary {
            gitignore: false,
            merge_artifacts: false,
            startup_cache: false,
            recovery_aged: false,
            export_hash: false,
            base_jsonl_symlink: false,
            base_jsonl_stale: false,
            orphan_tmp: false,
            jsonl_eof_newline: false,
            jsonl_bom: false,
            jsonl_crlf: false,
            jsonl_world_writable: false,
            config_yaml_secret_mode: false,
            inner_gitignore: false,
            dirty_bitmap_orphans: false,
            comments_orphans: false,
            labels_orphans: false,
            dependencies_orphans: false,
            wal_checkpoint: false,
            null_defaults: false,
            db_bloat_vacuum: true,
        };

        assert!(summary.applied());
        assert_eq!(
            summary.action_labels(),
            vec!["db_bloat_vacuumed".to_string()]
        );
        assert_eq!(
            repair_outcome_message_from_parts(summary.messages(), None, None),
            "Compacted database via VACUUM to reclaim freelist space (--unsafe-auto-fix opt-in)."
        );
        let audit = summary.audit_record();
        assert_eq!(audit.phase, "doctor.early_repair");
        assert_eq!(audit.outcome, "db_bloat_vacuumed");
        assert_eq!(audit.applied_actions, vec!["db_bloat_vacuumed".to_string()]);
    }

    #[test]
    fn test_early_repair_summary_reports_null_defaults_backfill() {
        let summary = EarlyRepairSummary {
            gitignore: false,
            merge_artifacts: false,
            startup_cache: false,
            recovery_aged: false,
            export_hash: false,
            base_jsonl_symlink: false,
            base_jsonl_stale: false,
            orphan_tmp: false,
            jsonl_eof_newline: false,
            jsonl_bom: false,
            jsonl_crlf: false,
            jsonl_world_writable: false,
            config_yaml_secret_mode: false,
            inner_gitignore: false,
            dirty_bitmap_orphans: false,
            comments_orphans: false,
            labels_orphans: false,
            dependencies_orphans: false,
            wal_checkpoint: false,
            null_defaults: true,
            db_bloat_vacuum: false,
        };

        assert!(summary.applied());
        assert_eq!(
            summary.action_labels(),
            vec!["null_defaults_backfilled".to_string()]
        );
        assert_eq!(
            repair_outcome_message_from_parts(summary.messages(), None, None),
            "Backfilled schema-declared defaults into NULL NOT-NULL columns."
        );
        let audit = summary.audit_record();
        assert_eq!(audit.phase, "doctor.early_repair");
        assert_eq!(audit.outcome, "null_defaults_backfilled");
        assert_eq!(
            audit.applied_actions,
            vec!["null_defaults_backfilled".to_string()]
        );
    }

    #[test]
    fn test_early_repair_summary_reports_base_jsonl_symlink_quarantine() {
        let summary = EarlyRepairSummary {
            gitignore: false,
            merge_artifacts: false,
            startup_cache: false,
            recovery_aged: false,
            export_hash: false,
            base_jsonl_symlink: true,
            base_jsonl_stale: false,
            orphan_tmp: false,
            jsonl_eof_newline: false,
            jsonl_bom: false,
            jsonl_crlf: false,
            jsonl_world_writable: false,
            config_yaml_secret_mode: false,
            inner_gitignore: false,
            dirty_bitmap_orphans: false,
            comments_orphans: false,
            labels_orphans: false,
            dependencies_orphans: false,
            wal_checkpoint: false,
            null_defaults: false,
            db_bloat_vacuum: false,
        };

        assert!(summary.applied());
        assert_eq!(
            summary.action_labels(),
            vec!["base_jsonl_symlink_quarantined".to_string()]
        );
        assert_eq!(
            repair_outcome_message_from_parts(summary.messages(), None, None),
            "Quarantined symlinked merge anchor."
        );
        let audit = summary.audit_record();
        assert_eq!(audit.phase, "doctor.early_repair");
        assert_eq!(audit.outcome, "base_jsonl_symlink_quarantined");
        assert_eq!(
            audit.applied_actions,
            vec!["base_jsonl_symlink_quarantined".to_string()]
        );
    }

    #[test]
    fn test_early_repair_summary_reports_base_jsonl_stale_regen() {
        let summary = EarlyRepairSummary {
            gitignore: false,
            merge_artifacts: false,
            startup_cache: false,
            recovery_aged: false,
            export_hash: false,
            base_jsonl_symlink: false,
            base_jsonl_stale: true,
            orphan_tmp: false,
            jsonl_eof_newline: false,
            jsonl_bom: false,
            jsonl_crlf: false,
            jsonl_world_writable: false,
            config_yaml_secret_mode: false,
            inner_gitignore: false,
            dirty_bitmap_orphans: false,
            comments_orphans: false,
            labels_orphans: false,
            dependencies_orphans: false,
            wal_checkpoint: false,
            null_defaults: false,
            db_bloat_vacuum: false,
        };

        assert!(summary.applied());
        assert_eq!(
            summary.action_labels(),
            vec!["base_jsonl_anchor_regenerated".to_string()]
        );
        assert_eq!(
            repair_outcome_message_from_parts(summary.messages(), None, None),
            "Regenerated stale merge anchor from current JSONL."
        );
        let audit = summary.audit_record();
        assert_eq!(audit.phase, "doctor.early_repair");
        assert_eq!(audit.outcome, "base_jsonl_anchor_regenerated");
        assert_eq!(
            audit.applied_actions,
            vec!["base_jsonl_anchor_regenerated".to_string()]
        );
    }

    #[test]
    fn test_early_repair_summary_reports_orphan_tmp_quarantine() {
        let summary = EarlyRepairSummary {
            gitignore: false,
            merge_artifacts: false,
            startup_cache: false,
            recovery_aged: false,
            export_hash: false,
            base_jsonl_symlink: false,
            base_jsonl_stale: false,
            orphan_tmp: true,
            jsonl_eof_newline: false,
            jsonl_bom: false,
            jsonl_crlf: false,
            jsonl_world_writable: false,
            config_yaml_secret_mode: false,
            inner_gitignore: false,
            dirty_bitmap_orphans: false,
            comments_orphans: false,
            labels_orphans: false,
            dependencies_orphans: false,
            wal_checkpoint: false,
            null_defaults: false,
            db_bloat_vacuum: false,
        };

        assert!(summary.applied());
        assert_eq!(
            summary.action_labels(),
            vec!["orphan_tmp_quarantined".to_string()]
        );
        assert_eq!(
            repair_outcome_message_from_parts(summary.messages(), None, None),
            "Quarantined orphan tmp files."
        );
        let audit = summary.audit_record();
        assert_eq!(audit.phase, "doctor.early_repair");
        assert_eq!(audit.outcome, "orphan_tmp_quarantined");
        assert_eq!(
            audit.applied_actions,
            vec!["orphan_tmp_quarantined".to_string()]
        );
    }

    #[test]
    fn test_check_jsonl_detects_malformed() -> Result<()> {
        let mut file = NamedTempFile::new().unwrap();
        std::io::Write::write_all(file.as_file_mut(), b"{\"id\":\"ok\"}\n")?;
        std::io::Write::write_all(file.as_file_mut(), b"{bad json}\n")?;

        let mut checks = Vec::new();
        let state = check_jsonl(file.path(), &mut checks).unwrap();
        assert_eq!(state, JsonlCountState::Invalid);

        let check = find_check(&checks, "jsonl.parse").expect("check present");
        assert!(matches!(check.status, CheckStatus::Error));

        Ok(())
    }

    #[test]
    fn test_check_jsonl_detects_invalid_issue_records() -> Result<()> {
        let mut file = NamedTempFile::new().unwrap();
        let mut invalid_issue = sample_issue("bd-bad01", "");
        invalid_issue.id = "bd-bad01".to_string();
        let encoded = serde_json::to_string(&invalid_issue)?;
        std::io::Write::write_all(file.as_file_mut(), encoded.as_bytes())?;
        std::io::Write::write_all(file.as_file_mut(), b"\n")?;

        let mut checks = Vec::new();
        let state = check_jsonl(file.path(), &mut checks).unwrap();
        assert_eq!(state, JsonlCountState::Invalid);

        let check = find_check(&checks, "jsonl.parse").expect("check present");
        assert!(matches!(check.status, CheckStatus::Error));
        assert!(
            check
                .message
                .as_deref()
                .is_some_and(|message| message.contains("title")),
            "unexpected check message: {:?}",
            check.message
        );

        Ok(())
    }

    #[test]
    fn test_check_jsonl_returns_count_only_for_valid_records() -> Result<()> {
        let mut file = NamedTempFile::new().unwrap();
        let issue = sample_issue("bd-good01", "Good issue");
        let encoded = serde_json::to_string(&issue)?;
        std::io::Write::write_all(file.as_file_mut(), encoded.as_bytes())?;
        std::io::Write::write_all(file.as_file_mut(), b"\n")?;

        let mut checks = Vec::new();
        let state = check_jsonl(file.path(), &mut checks).unwrap();
        assert_eq!(state, JsonlCountState::Available(1));

        let check = find_check(&checks, "jsonl.parse").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));

        Ok(())
    }

    #[test]
    fn test_collect_doctor_report_skips_count_comparison_for_invalid_jsonl() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        storage
            .create_issue(
                &sample_issue("bd-test01", "Doctor count source"),
                "doctor-test",
            )
            .unwrap();

        let valid_json =
            serde_json::to_string(&sample_issue("bd-test01", "Doctor count source")).unwrap();
        fs::write(&jsonl_path, format!("{valid_json}\n{{bad json}}\n")).unwrap();

        let paths = config::ConfigPaths {
            beads_dir: beads_dir.clone(),
            db_path,
            jsonl_path,
            metadata: config::Metadata::default(),
        };

        let report = collect_doctor_report(&beads_dir, &paths).expect("doctor report");
        let parse_check = find_check(&report.report.checks, "jsonl.parse").expect("jsonl parse");
        let counts_check =
            find_check(&report.report.checks, "counts.db_vs_jsonl").expect("count check");

        assert!(matches!(parse_check.status, CheckStatus::Error));
        assert!(matches!(counts_check.status, CheckStatus::Warn));
        assert!(
            counts_check
                .message
                .as_deref()
                .is_some_and(|message| message.contains("JSONL is invalid")),
            "unexpected count-check message: {:?}",
            counts_check.message
        );
    }

    #[test]
    fn test_collect_doctor_report_warns_on_equal_count_id_set_mismatch() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        storage
            .create_issue(&sample_issue("bd-db01", "Only in DB"), "doctor-test")
            .unwrap();

        let json = serde_json::to_string(&sample_issue("bd-jsonl01", "Only in JSONL")).unwrap();
        fs::write(&jsonl_path, format!("{json}\n")).unwrap();

        let paths = config::ConfigPaths {
            beads_dir: beads_dir.clone(),
            db_path,
            jsonl_path,
            metadata: config::Metadata::default(),
        };

        let report = collect_doctor_report(&beads_dir, &paths).expect("doctor report");
        let counts_check =
            find_check(&report.report.checks, "counts.db_vs_jsonl").expect("count check");

        assert!(matches!(counts_check.status, CheckStatus::Warn));
        assert!(
            counts_check
                .message
                .as_deref()
                .is_some_and(|message| message.contains("id sets diverge")),
            "unexpected count-check message: {:?}",
            counts_check.message
        );
        let id_delta = counts_check
            .details
            .as_ref()
            .and_then(|details| details.get("id_delta"))
            .expect("id delta details");
        assert_eq!(id_delta["only_db_count"], 1);
        assert_eq!(id_delta["only_jsonl_count"], 1);
        assert_eq!(id_delta["only_db"][0], "bd-db01");
        assert_eq!(id_delta["only_jsonl"][0], "bd-jsonl01");
    }

    #[test]
    fn test_required_schema_checks_missing_tables() {
        let conn = Connection::open(":memory:").unwrap();
        let mut checks = Vec::new();
        required_schema_checks(&conn, &mut checks).unwrap();

        let tables = find_check(&checks, "schema.tables").expect("tables check");
        assert!(matches!(tables.status, CheckStatus::Error));
    }

    #[test]
    fn test_collect_doctor_report_reports_missing_metadata_tables_without_aborting() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        conn.execute(
            r"
            CREATE TABLE issues (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                status TEXT NOT NULL,
                priority INTEGER NOT NULL,
                issue_type TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            )
            ",
        )
        .unwrap();
        conn.close().unwrap();
        fs::write(&jsonl_path, "{\"id\":\"bd-test\"}\n").unwrap();

        let paths = config::ConfigPaths {
            beads_dir: beads_dir.clone(),
            db_path,
            jsonl_path,
            metadata: config::Metadata::default(),
        };

        let report = collect_doctor_report(&beads_dir, &paths).expect("doctor report");
        let anomaly_check = find_check(&report.report.checks, "db.recoverable_anomalies")
            .expect("recoverable anomalies check");

        assert!(matches!(anomaly_check.status, CheckStatus::Error));
        assert!(
            anomaly_check
                .message
                .as_deref()
                .is_some_and(|message| message.contains("Failed to inspect recoverable anomalies")),
            "unexpected check message: {:?}",
            anomaly_check.message
        );
    }

    #[test]
    fn test_collect_doctor_report_quick_skips_slow_detectors_before_execution() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        conn.execute(
            r"
            CREATE TABLE issues (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                status TEXT NOT NULL,
                priority INTEGER NOT NULL,
                issue_type TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            )
            ",
        )
        .unwrap();
        fs::write(&jsonl_path, "{\"id\":\"bd-test\"}\n").unwrap();

        let paths = config::ConfigPaths {
            beads_dir: beads_dir.clone(),
            db_path,
            jsonl_path,
            metadata: config::Metadata::default(),
        };

        let quick =
            collect_doctor_report_with_mode(&beads_dir, &paths, DoctorInspectionMode::Quick)
                .expect("quick doctor report");

        assert!(
            find_check(&quick.report.checks, "schema.tables").is_some(),
            "quick mode should still run cheap schema checks"
        );
        for skipped in [
            "db.recoverable_anomalies",
            "counts.db_vs_jsonl",
            "sync.metadata",
            "sqlite3.integrity_check",
            "db.write_probe",
        ] {
            assert!(
                find_check(&quick.report.checks, skipped).is_none(),
                "quick mode should not execute slow detector {skipped}"
            );
        }
    }

    #[test]
    fn test_collect_doctor_report_quick_treats_warnings_as_findings() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let db_path = beads_dir.join("beads.db");
        {
            let _storage = SqliteStorage::open(&db_path).unwrap();
        }

        let paths = config::ConfigPaths {
            beads_dir: beads_dir.clone(),
            db_path,
            jsonl_path: beads_dir.join("issues.jsonl"),
            metadata: config::Metadata::default(),
        };

        let quick =
            collect_doctor_report_with_mode(&beads_dir, &paths, DoctorInspectionMode::Quick)
                .expect("quick doctor report");

        let jsonl_check = find_check(&quick.report.checks, "jsonl.parse").expect("jsonl warning");
        assert!(matches!(jsonl_check.status, CheckStatus::Warn));
        assert!(
            !quick.report.ok,
            "quick mode should fail its gate when a warning-level finding remains"
        );
    }

    #[test]
    fn test_select_doctor_jsonl_path_keeps_missing_explicit_override() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let configured_jsonl = beads_dir.join("custom.jsonl");
        let legacy_jsonl = beads_dir.join("issues.jsonl");
        fs::write(&legacy_jsonl, "{\"id\":\"bd-legacy\"}\n").unwrap();

        let paths = config::ConfigPaths {
            beads_dir: beads_dir.clone(),
            db_path: beads_dir.join("beads.db"),
            jsonl_path: configured_jsonl.clone(),
            metadata: config::Metadata {
                database: "beads.db".to_string(),
                jsonl_export: "custom.jsonl".to_string(),
                backend: None,
                deletions_retention_days: None,
            },
        };

        assert_eq!(
            select_doctor_jsonl_path(&beads_dir, &paths),
            Some(configured_jsonl)
        );
    }

    #[test]
    fn test_collect_doctor_report_surfaces_missing_explicit_metadata_jsonl() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let db_path = beads_dir.join("beads.db");
        let configured_jsonl = beads_dir.join("custom.jsonl");
        fs::write(beads_dir.join("issues.jsonl"), "{\"id\":\"bd-legacy\"}\n").unwrap();
        let _storage = SqliteStorage::open(&db_path).unwrap();

        let paths = config::ConfigPaths {
            beads_dir: beads_dir.clone(),
            db_path,
            jsonl_path: configured_jsonl.clone(),
            metadata: config::Metadata {
                database: "beads.db".to_string(),
                jsonl_export: "custom.jsonl".to_string(),
                backend: None,
                deletions_retention_days: None,
            },
        };

        let report = collect_doctor_report(&beads_dir, &paths).expect("doctor report");
        let parse_check = find_check(&report.report.checks, "jsonl.parse").expect("jsonl parse");

        assert!(matches!(parse_check.status, CheckStatus::Error));
        assert_eq!(report.jsonl_path, Some(configured_jsonl.clone()));
        assert_eq!(
            parse_check
                .details
                .as_ref()
                .and_then(|details| details.get("path"))
                .and_then(serde_json::Value::as_str),
            Some(configured_jsonl.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn test_collect_doctor_report_accepts_configured_external_jsonl() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let external_dir = temp.path().join("external");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&external_dir).unwrap();

        let db_path = beads_dir.join("beads.db");
        let external_jsonl = external_dir.join("issues.jsonl");
        fs::write(&external_jsonl, "{\"id\":\"bd-external\"}\n").unwrap();
        let _storage = SqliteStorage::open(&db_path).unwrap();

        let paths = config::ConfigPaths {
            beads_dir: beads_dir.clone(),
            db_path,
            jsonl_path: external_jsonl.clone(),
            metadata: config::Metadata {
                database: "beads.db".to_string(),
                jsonl_export: external_jsonl.to_string_lossy().into_owned(),
                backend: None,
                deletions_retention_days: None,
            },
        };

        let report = collect_doctor_report(&beads_dir, &paths).expect("doctor report");
        let sync_path_check =
            find_check(&report.report.checks, "sync_jsonl_path").expect("sync path check");

        assert!(matches!(sync_path_check.status, CheckStatus::Ok));
        assert_eq!(
            sync_path_check
                .details
                .as_ref()
                .and_then(|details| details.get("path"))
                .and_then(serde_json::Value::as_str),
            Some(external_jsonl.to_string_lossy().as_ref())
        );
        assert_eq!(
            sync_path_check
                .details
                .as_ref()
                .and_then(|details| details.get("external"))
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_collect_doctor_report_checks_selected_external_jsonl_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let external_dir = temp.path().join("external");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&external_dir).unwrap();

        let db_path = beads_dir.join("beads.db");
        let external_jsonl = external_dir.join("issues.jsonl");
        fs::write(beads_dir.join("issues.jsonl"), "{\"id\":\"bd-local\"}\n").unwrap();
        fs::write(&external_jsonl, "{\"id\":\"bd-external\"}\n").unwrap();
        let mut perms = fs::metadata(&external_jsonl).unwrap().permissions();
        perms.set_mode(0o666);
        fs::set_permissions(&external_jsonl, perms).unwrap();
        let _storage = SqliteStorage::open(&db_path).unwrap();

        let paths = config::ConfigPaths {
            beads_dir: beads_dir.clone(),
            db_path,
            jsonl_path: external_jsonl.clone(),
            metadata: config::Metadata {
                database: "beads.db".to_string(),
                jsonl_export: external_jsonl.to_string_lossy().into_owned(),
                backend: None,
                deletions_retention_days: None,
            },
        };

        let report = collect_doctor_report(&beads_dir, &paths).expect("doctor report");
        let permission_check =
            find_check(&report.report.checks, "permissions.jsonl_world_writable")
                .expect("permission check");

        assert!(
            matches!(permission_check.status, CheckStatus::Warn),
            "{permission_check:?}"
        );
        assert_eq!(
            permission_check
                .details
                .as_ref()
                .and_then(|details| details.get("path"))
                .and_then(serde_json::Value::as_str),
            Some(external_jsonl.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn test_collect_doctor_report_checks_selected_external_jsonl_trailing_newline() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let external_dir = temp.path().join("external");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&external_dir).unwrap();

        let db_path = beads_dir.join("beads.db");
        let external_jsonl = external_dir.join("issues.jsonl");
        fs::write(beads_dir.join("issues.jsonl"), "{\"id\":\"bd-local\"}\n").unwrap();
        fs::write(&external_jsonl, "{\"id\":\"bd-external\"}").unwrap();
        let _storage = SqliteStorage::open(&db_path).unwrap();

        let paths = config::ConfigPaths {
            beads_dir: beads_dir.clone(),
            db_path,
            jsonl_path: external_jsonl.clone(),
            metadata: config::Metadata {
                database: "beads.db".to_string(),
                jsonl_export: external_jsonl.to_string_lossy().into_owned(),
                backend: None,
                deletions_retention_days: None,
            },
        };

        let report = collect_doctor_report(&beads_dir, &paths).expect("doctor report");
        let newline_check =
            find_check(&report.report.checks, "jsonl_eof_newline").expect("newline check");

        assert!(
            matches!(newline_check.status, CheckStatus::Warn),
            "{newline_check:?}"
        );
        assert_eq!(
            newline_check
                .details
                .as_ref()
                .and_then(|details| details.get("path"))
                .and_then(serde_json::Value::as_str),
            Some(external_jsonl.to_string_lossy().as_ref())
        );
    }

    #[test]
    fn test_collect_doctor_report_checks_selected_external_jsonl_bom_and_crlf() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let external_dir = temp.path().join("external");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&external_dir).unwrap();

        let db_path = beads_dir.join("beads.db");
        let external_jsonl = external_dir.join("issues.jsonl");
        fs::write(beads_dir.join("issues.jsonl"), "{\"id\":\"bd-local\"}\n").unwrap();
        let mut external_bytes = UTF8_BOM.to_vec();
        external_bytes.extend_from_slice(b"{\"id\":\"bd-external\"}\r\n");
        fs::write(&external_jsonl, external_bytes).unwrap();
        let _storage = SqliteStorage::open(&db_path).unwrap();

        let paths = config::ConfigPaths {
            beads_dir: beads_dir.clone(),
            db_path,
            jsonl_path: external_jsonl.clone(),
            metadata: config::Metadata {
                database: "beads.db".to_string(),
                jsonl_export: external_jsonl.to_string_lossy().into_owned(),
                backend: None,
                deletions_retention_days: None,
            },
        };

        let report = collect_doctor_report(&beads_dir, &paths).expect("doctor report");
        for check_name in ["jsonl_bom", "jsonl_crlf"] {
            let check = find_check(&report.report.checks, check_name).expect("selected path check");
            assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
            assert_eq!(
                check
                    .details
                    .as_ref()
                    .and_then(|details| details.get("path"))
                    .and_then(serde_json::Value::as_str),
                Some(external_jsonl.to_string_lossy().as_ref()),
                "{check_name} must report the selected external JSONL"
            );
        }
    }

    #[test]
    fn test_fix_jsonl_trailing_newline_appends_selected_in_workspace_path() {
        let temp = TempDir::new().unwrap();
        let external_dir = temp.path().join("external");
        fs::create_dir_all(&external_dir).unwrap();
        let external_jsonl = external_dir.join("issues.jsonl");
        fs::write(&external_jsonl, "{\"id\":\"bd-external\"}").unwrap();
        let report = DoctorReport {
            ok: false,
            workspace_health: None,
            reliability_audit: None,
            checks: vec![CheckResult {
                name: "jsonl_eof_newline".to_string(),
                status: CheckStatus::Warn,
                message: None,
                details: None,
            }],
        };
        let mut session = DoctorRepairSession::new(temp.path(), /* dry_run = */ false)
            .expect("session must build");

        assert!(fix_jsonl_trailing_newline_if_warned(
            Some(&external_jsonl),
            &report,
            &quiet_ctx(),
            Some(&mut session),
        ));
        assert_eq!(
            fs::read_to_string(&external_jsonl).unwrap(),
            "{\"id\":\"bd-external\"}\n"
        );
        assert_eq!(
            fs::read_to_string(session.run.root.join("backups/external/issues.jsonl")).unwrap(),
            "{\"id\":\"bd-external\"}"
        );
    }

    #[test]
    fn test_fix_jsonl_trailing_newline_skips_path_outside_workspace() {
        let repo = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let external_jsonl = outside.path().join("issues.jsonl");
        fs::write(&external_jsonl, "{\"id\":\"bd-outside\"}").unwrap();
        let report = DoctorReport {
            ok: false,
            workspace_health: None,
            reliability_audit: None,
            checks: vec![CheckResult {
                name: "jsonl_eof_newline".to_string(),
                status: CheckStatus::Warn,
                message: None,
                details: None,
            }],
        };
        let mut session =
            DoctorRepairSession::new(repo.path(), /* dry_run = */ false).expect("session");

        assert!(!fix_jsonl_trailing_newline_if_warned(
            Some(&external_jsonl),
            &report,
            &quiet_ctx(),
            Some(&mut session),
        ));
        assert_eq!(
            fs::read_to_string(&external_jsonl).unwrap(),
            "{\"id\":\"bd-outside\"}"
        );
        assert_eq!(
            fs::read_to_string(&session.run.actions_file).unwrap(),
            "",
            "skipped external repairs must not write an undo action"
        );
    }

    #[test]
    fn test_fix_jsonl_trailing_newline_skips_traversal_outside_workspace() {
        let parent = TempDir::new().unwrap();
        let repo = parent.path().join("repo");
        let outside = parent.path().join("outside");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&outside).unwrap();
        let outside_jsonl = outside.join("issues.jsonl");
        fs::write(&outside_jsonl, "{\"id\":\"bd-outside\"}").unwrap();
        let traversal_path = repo.join("../outside/issues.jsonl");
        let report = DoctorReport {
            ok: false,
            workspace_health: None,
            reliability_audit: None,
            checks: vec![CheckResult {
                name: "jsonl_eof_newline".to_string(),
                status: CheckStatus::Warn,
                message: None,
                details: None,
            }],
        };
        let mut session = DoctorRepairSession::new(&repo, /* dry_run = */ false).expect("session");

        assert!(!fix_jsonl_trailing_newline_if_warned(
            Some(&traversal_path),
            &report,
            &quiet_ctx(),
            Some(&mut session),
        ));
        assert_eq!(
            fs::read_to_string(&outside_jsonl).unwrap(),
            "{\"id\":\"bd-outside\"}",
            "traversal-selected outside JSONL must not be rewritten"
        );
        assert_eq!(
            fs::read_to_string(&session.run.actions_file).unwrap(),
            "",
            "skipped traversal repairs must not write an undo action"
        );
    }

    #[test]
    fn test_integrity_check_messages_collects_all_rows() {
        let messages = integrity_check_messages(&[
            vec![SqliteValue::Text("row 1 missing from index idx_a".into())],
            vec![SqliteValue::Text("row 2 missing from index idx_a".into())],
        ]);

        assert_eq!(
            messages,
            vec![
                "row 1 missing from index idx_a".to_string(),
                "row 2 missing from index idx_a".to_string(),
            ]
        );
    }

    #[test]
    fn test_check_sync_metadata_clean_export_after_import_is_not_pending_import() -> Result<()> {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let jsonl_path = temp.path().join("issues.jsonl");
        fs::write(&jsonl_path, "{\"id\":\"bd-clean\"}\n").unwrap();
        let jsonl_hash = crate::sync::compute_jsonl_hash(&jsonl_path)?;

        let mut storage = SqliteStorage::open(&db_path).unwrap();
        storage.set_metadata("last_import_time", "2026-01-01T00:00:00Z")?;
        storage.set_metadata("last_export_time", "2999-01-01T00:00:00Z")?;
        storage.set_metadata("jsonl_content_hash", &jsonl_hash)?;
        drop(storage);

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let mut checks = Vec::new();
        check_sync_metadata(&conn, &db_path, Some(&jsonl_path), &mut checks);

        let check = find_check(&checks, "sync.metadata").expect("sync metadata check");
        assert!(matches!(check.status, CheckStatus::Ok), "{check:?}");
        assert_eq!(
            check.message.as_deref(),
            Some("Database and JSONL are in sync"),
            "last_export > last_import is history, not a pending import"
        );
        let details = check.details.as_ref().expect("sync details");
        assert_eq!(details["dirty_issues"], 0);
        assert_eq!(details["jsonl_newer"], false);
        assert_eq!(details["db_newer"], false);
        assert_eq!(details["pending_import"], false);
        assert_eq!(details["pending_export"], false);

        Ok(())
    }

    #[test]
    fn test_check_sync_metadata_pending_import_without_local_dirty_is_warn() -> Result<()> {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let jsonl_path = temp.path().join("issues.jsonl");
        fs::write(&jsonl_path, "{\"id\":\"bd-remote\"}\n").unwrap();

        let storage = SqliteStorage::open(&db_path).unwrap();
        assert_eq!(storage.get_dirty_issue_count()?, 0);
        drop(storage);

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let mut checks = Vec::new();
        check_sync_metadata(&conn, &db_path, Some(&jsonl_path), &mut checks);

        let check = find_check(&checks, "sync.metadata").expect("sync metadata check");
        assert!(
            matches!(check.status, CheckStatus::Warn),
            "pending-import is an advisory Warn, matching the pending-export \
             direction (beads_rust#330): {check:?}"
        );
        assert_eq!(
            check.message.as_deref(),
            Some("External changes pending import"),
            "pure JSONL-newer state is explicit import work, not a doctor failure"
        );
        let details = check.details.as_ref().expect("sync details");
        assert_eq!(details["jsonl_newer"], true);
        assert_eq!(details["db_newer"], false);
        assert_eq!(details["pending_import"], true);
        assert_eq!(details["pending_export"], false);

        Ok(())
    }

    #[test]
    fn test_check_sync_metadata_empty_jsonl_pending_import_stays_ok() -> Result<()> {
        // beads_rust#330 regression guard: a fresh `br init` leaves an
        // empty issues.jsonl whose mtime trails the DB, so `jsonl_newer`
        // reads true with nothing to import. That benign state must stay
        // Ok so `br doctor` keeps exiting 0 on a brand-new workspace.
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let jsonl_path = temp.path().join("issues.jsonl");
        // Empty JSONL — the fresh-init shape.
        fs::write(&jsonl_path, b"").unwrap();

        let storage = SqliteStorage::open(&db_path).unwrap();
        assert_eq!(storage.get_dirty_issue_count()?, 0);
        drop(storage);

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let mut checks = Vec::new();
        check_sync_metadata(&conn, &db_path, Some(&jsonl_path), &mut checks);

        let check = find_check(&checks, "sync.metadata").expect("sync metadata check");
        assert!(
            matches!(check.status, CheckStatus::Ok),
            "empty-JSONL pending-import is benign (nothing to import): {check:?}"
        );
        assert_eq!(
            check.message.as_deref(),
            Some("External changes pending import (empty JSONL, nothing to import)")
        );
        let details = check.details.as_ref().expect("sync details");
        assert_eq!(details["jsonl_newer"], true);
        assert_eq!(details["pending_import"], true);

        Ok(())
    }

    #[test]
    fn test_check_sync_metadata_no_export_does_not_hide_divergence() -> Result<()> {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let jsonl_path = temp.path().join("issues.jsonl");
        fs::write(&jsonl_path, "{\"id\":\"bd-remote\"}\n").unwrap();

        let mut storage = SqliteStorage::open(&db_path).unwrap();
        storage.create_issue(&sample_issue("bd-local", "Local dirty issue"), "tester")?;
        drop(storage);

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let mut checks = Vec::new();
        check_sync_metadata(&conn, &db_path, Some(&jsonl_path), &mut checks);

        let check = find_check(&checks, "sync.metadata").expect("sync metadata check");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
        assert_eq!(
            check.message.as_deref(),
            Some("Database and JSONL have diverged (merge required)"),
            "doctor must not suggest flush-only when JSONL is also newer"
        );
        let details = check.details.as_ref().expect("sync details");
        assert_eq!(details["jsonl_newer"], true);
        assert_eq!(details["db_newer"], true);
        assert_eq!(details["pending_import"], true);
        assert_eq!(details["pending_export"], true);

        Ok(())
    }

    #[test]
    fn test_check_recoverable_anomalies_detects_duplicate_config_and_metadata() -> Result<()> {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let _storage = SqliteStorage::open(&db_path).unwrap();

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        conn.execute("INSERT INTO config (key, value) VALUES ('issue_prefix', 'dup-a')")
            .unwrap();
        conn.execute("INSERT INTO config (key, value) VALUES ('issue_prefix', 'dup-b')")
            .unwrap();
        conn.execute("INSERT INTO metadata (key, value) VALUES ('project', 'dup-a')")
            .unwrap();
        conn.execute("INSERT INTO metadata (key, value) VALUES ('project', 'dup-b')")
            .unwrap();

        let mut checks = Vec::new();
        check_recoverable_anomalies(&conn, &mut checks)?;

        let check = find_check(&checks, "db.recoverable_anomalies").expect("check present");
        assert!(matches!(check.status, CheckStatus::Error));

        let findings = check
            .details
            .as_ref()
            .and_then(|details| details.get("findings"))
            .and_then(serde_json::Value::as_array)
            .expect("findings array");
        assert!(
            findings.iter().any(|finding| {
                finding
                    .as_str()
                    .is_some_and(|message| message.contains("config contains duplicate rows"))
            }),
            "expected duplicate config finding: {findings:?}"
        );
        assert!(
            findings.iter().any(|finding| {
                finding
                    .as_str()
                    .is_some_and(|message| message.contains("metadata contains duplicate rows"))
            }),
            "expected duplicate metadata finding: {findings:?}"
        );

        Ok(())
    }

    #[test]
    fn test_check_recoverable_anomalies_treats_stale_blocked_cache_as_warning() -> Result<()> {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        storage.mark_blocked_cache_stale()?;

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let mut checks = Vec::new();
        check_recoverable_anomalies(&conn, &mut checks)?;

        let check = find_check(&checks, "db.recoverable_anomalies").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn));
        assert_eq!(check.message.as_deref(), Some(BLOCKED_CACHE_STALE_FINDING));

        Ok(())
    }

    #[test]
    fn test_check_recoverable_anomalies_warns_on_blocked_cache_content_mismatch() -> Result<()> {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let blocker = sample_issue("bd-blocker", "Blocker");
        let target = sample_issue("bd-target", "Target");
        storage.create_issue(&blocker, "tester")?;
        storage.create_issue(&target, "tester")?;
        storage.add_dependency(&target.id, &blocker.id, "blocks", "tester")?;
        assert!(storage.ensure_blocked_cache_fresh()?);
        storage.execute_test_sql(
            "UPDATE blocked_issues_cache
             SET blocked_by = '[\"bd-other:open\"]'
             WHERE issue_id = 'bd-target'",
        )?;
        drop(storage);

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let mut checks = Vec::new();
        check_recoverable_anomalies(&conn, &mut checks)?;

        let check = find_check(&checks, "db.recoverable_anomalies").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn));
        assert_eq!(
            check.message.as_deref(),
            Some(BLOCKED_CACHE_CONTENT_MISMATCH_FINDING)
        );
        let findings = check
            .details
            .as_ref()
            .and_then(|details| details.get("findings"))
            .and_then(serde_json::Value::as_array)
            .expect("findings array");
        assert!(findings.iter().any(|finding| {
            finding
                .as_str()
                .is_some_and(|message| message == BLOCKED_CACHE_CONTENT_MISMATCH_FINDING)
        }));

        Ok(())
    }

    #[test]
    fn test_check_recoverable_anomalies_warns_on_ready_projection_mismatch() -> Result<()> {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let blocker = sample_issue("bd-blocker", "Blocker");
        let target = sample_issue("bd-target", "Target");
        storage.create_issue(&blocker, "tester")?;
        storage.create_issue(&target, "tester")?;
        storage.add_dependency(&target.id, &blocker.id, "blocks", "tester")?;
        assert!(storage.ensure_blocked_cache_fresh()?);
        storage.execute_test_sql(
            "DELETE FROM blocked_issues_cache
             WHERE issue_id = 'bd-target'",
        )?;
        drop(storage);

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let mut checks = Vec::new();
        check_recoverable_anomalies(&conn, &mut checks)?;

        let check = find_check(&checks, "db.recoverable_anomalies").expect("check present");
        assert!(matches!(check.status, CheckStatus::Warn));
        let findings = check
            .details
            .as_ref()
            .and_then(|details| details.get("findings"))
            .and_then(serde_json::Value::as_array)
            .expect("findings array");
        assert!(findings.iter().any(|finding| {
            finding
                .as_str()
                .is_some_and(|message| message == READY_PROJECTION_CONTENT_MISMATCH_FINDING)
        }));

        Ok(())
    }

    #[test]
    fn test_repair_recoverable_db_state_rebuilds_blocked_cache_content_mismatch() -> Result<()> {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let blocker = sample_issue("bd-blocker", "Blocker");
        let target = sample_issue("bd-target", "Target");
        storage.create_issue(&blocker, "tester")?;
        storage.create_issue(&target, "tester")?;
        storage.add_dependency(&target.id, &blocker.id, "blocks", "tester")?;
        assert!(storage.ensure_blocked_cache_fresh()?);
        storage.execute_test_sql(
            "UPDATE blocked_issues_cache
             SET blocked_by = '[\"bd-other:open\"]'
             WHERE issue_id = 'bd-target'",
        )?;
        drop(storage);

        let report = DoctorReport {
            ok: true,
            workspace_health: None,
            reliability_audit: None,
            checks: vec![CheckResult {
                name: "db.recoverable_anomalies".to_string(),
                status: CheckStatus::Warn,
                message: Some(BLOCKED_CACHE_CONTENT_MISMATCH_FINDING.to_string()),
                details: Some(serde_json::json!({
                    "findings": [BLOCKED_CACHE_CONTENT_MISMATCH_FINDING],
                })),
            }],
        };

        let repair = repair_recoverable_db_state(
            temp.path(),
            &db_path,
            &report,
            None,
            &FixerFilter::default(),
        );

        assert!(repair.blocked_cache_rebuilt);
        let storage = SqliteStorage::open(&db_path).unwrap();
        let rows = storage.execute_raw_query(
            "SELECT blocked_by
             FROM blocked_issues_cache
             WHERE issue_id = 'bd-target'",
        )?;
        let blocked_by = rows
            .first()
            .and_then(|row| row.first())
            .and_then(SqliteValue::as_text)
            .unwrap_or("");
        assert_eq!(blocked_by, "[\"bd-blocker:open\"]");

        Ok(())
    }

    #[test]
    fn test_repair_recoverable_db_state_rebuilds_ready_projection_mismatch() -> Result<()> {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let blocker = sample_issue("bd-blocker", "Blocker");
        let target = sample_issue("bd-target", "Target");
        storage.create_issue(&blocker, "tester")?;
        storage.create_issue(&target, "tester")?;
        storage.add_dependency(&target.id, &blocker.id, "blocks", "tester")?;
        assert!(storage.ensure_blocked_cache_fresh()?);
        storage.execute_test_sql(
            "DELETE FROM blocked_issues_cache
             WHERE issue_id = 'bd-target'",
        )?;
        drop(storage);

        let report = DoctorReport {
            ok: true,
            workspace_health: None,
            reliability_audit: None,
            checks: vec![CheckResult {
                name: "db.recoverable_anomalies".to_string(),
                status: CheckStatus::Warn,
                message: Some(READY_PROJECTION_CONTENT_MISMATCH_FINDING.to_string()),
                details: Some(serde_json::json!({
                    "findings": [READY_PROJECTION_CONTENT_MISMATCH_FINDING],
                })),
            }],
        };

        let repair = repair_recoverable_db_state(
            temp.path(),
            &db_path,
            &report,
            None,
            &FixerFilter::default(),
        );

        assert!(repair.blocked_cache_rebuilt);
        let storage = SqliteStorage::open(&db_path).unwrap();
        let rows = storage.execute_raw_query(
            "SELECT blocked_by
             FROM blocked_issues_cache
             WHERE issue_id = 'bd-target'",
        )?;
        assert_eq!(rows.len(), 1);

        Ok(())
    }

    #[test]
    fn test_check_database_sidecars_accepts_wal_without_shm() -> Result<()> {
        // frankensqlite does not create SHM files — WAL without SHM is expected and should
        // be informational without degrading an otherwise healthy workspace.
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        fs::write(&db_path, b"sqlite-header-placeholder")?;
        fs::write(
            PathBuf::from(format!("{}-wal", db_path.to_string_lossy())),
            b"synthetic wal",
        )?;

        let mut checks = Vec::new();
        check_database_sidecars(&db_path, &mut checks)?;

        let check = find_check(&checks, "db.sidecars").expect("sidecar check");
        assert!(
            matches!(check.status, CheckStatus::Ok),
            "expected Ok for FrankenSQLite WAL-without-SHM, got {:?}",
            check.status
        );
        assert!(
            check.message.as_deref().is_some_and(|message| {
                message.contains("WAL sidecar exists without a matching SHM sidecar")
            }),
            "unexpected sidecar message: {:?}",
            check.message
        );
        Ok(())
    }

    #[test]
    fn test_check_database_sidecars_rejects_invalid_layouts() -> Result<()> {
        let assert_error = |db_path: &Path, expected: &str| -> Result<()> {
            let mut checks = Vec::new();
            check_database_sidecars(db_path, &mut checks)?;
            let check = find_check(&checks, "db.sidecars").expect("sidecar check");
            assert_eq!(check.status, CheckStatus::Error, "{check:?}");
            assert!(
                check
                    .message
                    .as_deref()
                    .is_some_and(|message| message.contains(expected)),
                "expected `{expected}` in sidecar error: {check:?}"
            );
            Ok(())
        };

        let shm_only = TempDir::new()?;
        let shm_only_db = shm_only.path().join("beads.db");
        fs::write(&shm_only_db, b"sqlite-header-placeholder")?;
        fs::write(
            PathBuf::from(format!("{}-shm", shm_only_db.to_string_lossy())),
            b"orphan shm",
        )?;
        assert_error(&shm_only_db, "SHM sidecar exists without a matching WAL")?;

        let non_file = TempDir::new()?;
        let non_file_db = non_file.path().join("beads.db");
        fs::write(&non_file_db, b"sqlite-header-placeholder")?;
        fs::create_dir(PathBuf::from(format!(
            "{}-wal",
            non_file_db.to_string_lossy()
        )))?;
        assert_error(&non_file_db, "directory instead of a regular file")?;

        let dangling = TempDir::new()?;
        let missing_db = dangling.path().join("beads.db");
        fs::write(
            PathBuf::from(format!("{}-wal", missing_db.to_string_lossy())),
            b"dangling wal",
        )?;
        assert_error(&missing_db, "Database sidecars exist even though")?;

        Ok(())
    }

    #[test]
    fn sqlite_readonly_immutable_uri_escapes_uri_control_bytes() {
        let uri = sqlite_readonly_immutable_uri(Path::new("/tmp/br doctor/a b?c#d%.db"));

        assert_eq!(
            uri,
            "file:/tmp/br%20doctor/a%20b%3Fc%23d%25.db?mode=ro&immutable=1"
        );
    }

    #[test]
    fn sqlite_cli_integrity_does_not_create_shm_sidecar() -> Result<()> {
        if Command::new("sqlite3").arg("-version").output().is_err() {
            return Ok(());
        }

        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let wal_path = PathBuf::from(format!("{}-wal", db_path.to_string_lossy()));
        let shm_path = PathBuf::from(format!("{}-shm", db_path.to_string_lossy()));
        fs::write(&db_path, b"this is not a SQLite")?;
        fs::write(&wal_path, b"synthetic wal")?;

        assert!(!shm_path.exists(), "test precondition violated");
        let result = sqlite_cli_integrity_messages(&db_path);
        assert!(
            result.is_err(),
            "malformed database should still surface an integrity failure"
        );
        assert!(
            !shm_path.exists(),
            "sqlite3 integrity fallback must not create a live SHM sidecar"
        );

        Ok(())
    }

    #[test]
    fn test_check_recovery_artifacts_warns_on_preserved_database_family() -> Result<()> {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let db_path = beads_dir.join("beads.db");
        fs::create_dir_all(&beads_dir)?;
        fs::write(beads_dir.join("beads.db.bad_20260312T000000Z"), b"backup")?;
        let recovery_dir = config::recovery_dir_for_db_path(&db_path, &beads_dir);
        fs::create_dir_all(&recovery_dir)?;
        fs::write(
            recovery_dir.join("beads.db.20260312T000000Z.rebuild-failed"),
            b"preserved",
        )?;

        let mut checks = Vec::new();
        check_recovery_artifacts(&beads_dir, &db_path, &mut checks)?;

        let check = find_check(&checks, "db.recovery_artifacts").expect("recovery artifact check");
        assert!(matches!(check.status, CheckStatus::Warn));
        let artifacts = check
            .details
            .as_ref()
            .and_then(|details| details.get("artifacts"))
            .and_then(serde_json::Value::as_array)
            .expect("artifact list");
        assert_eq!(artifacts.len(), 2);
        Ok(())
    }

    #[test]
    fn test_repair_recoverable_db_state_quarantines_orphan_shm_sidecar() -> Result<()> {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir)?;
        let db_path = beads_dir.join("beads.db");
        {
            let _storage = SqliteStorage::open(&db_path)?;
        }
        let wal_path = PathBuf::from(format!("{}-wal", db_path.to_string_lossy()));
        let shm_path = PathBuf::from(format!("{}-shm", db_path.to_string_lossy()));
        let _ = fs::remove_file(&wal_path);
        let _ = fs::remove_file(&shm_path);
        fs::write(&shm_path, b"orphan shm")?;

        let report = DoctorReport {
            ok: false,
            workspace_health: None,
            reliability_audit: None,
            checks: vec![CheckResult {
                name: "db.sidecars".to_string(),
                status: CheckStatus::Error,
                message: Some("SHM sidecar exists without a matching WAL sidecar".to_string()),
                details: None,
            }],
        };

        let mut session =
            DoctorRepairSession::new(temp.path(), /* dry_run = */ false).expect("session builds");

        let repair = repair_recoverable_db_state(
            &beads_dir,
            &db_path,
            &report,
            Some(&mut session),
            &FixerFilter::default(),
        );
        assert!(
            !repair.quarantined_artifacts.is_empty(),
            "expected local repair to quarantine the orphan SHM sidecar"
        );

        let quarantine_path = session
            .run
            .root
            .join("quarantine")
            .join(".beads")
            .join("beads.db-shm");
        // The storage engine may immediately recreate its own `-shm` when the
        // repair reopens the database, so the live path existing again is not
        // a failure. What must hold is that the *orphan* bytes were moved out
        // rather than left in place.
        assert_ne!(
            fs::read(&shm_path).ok().as_deref(),
            Some(b"orphan shm".as_slice()),
            "orphan SHM content should no longer be at the live sidecar path"
        );
        assert_eq!(
            fs::read(&quarantine_path)?,
            b"orphan shm",
            "the quarantined copy should hold the orphan bytes"
        );
        assert!(
            quarantine_path.is_file(),
            "orphan SHM should be quarantined at {}",
            quarantine_path.display()
        );
        assert!(
            repair
                .quarantined_artifacts
                .iter()
                .any(|path| path == &quarantine_path.display().to_string()),
            "repair result should report the chokepoint quarantine path: {:?}",
            repair.quarantined_artifacts
        );

        let actions = fs::read_to_string(&session.run.actions_file)?;
        let action: serde_json::Value = serde_json::from_str(
            actions
                .lines()
                .find(|line| !line.trim().is_empty())
                .expect("actions.jsonl should contain the sidecar rename"),
        )?;
        assert_eq!(action["op"], "rename");
        assert_eq!(action["fixer_id"], "doctor.database_sidecar_quarantine");
        assert_eq!(action["path"], ".beads/beads.db-shm");
        assert_eq!(
            action["rename_to"].as_str(),
            Some(quarantine_path.to_string_lossy().as_ref())
        );
        Ok(())
    }

    #[test]
    fn test_repair_recoverable_db_state_skips_repair_for_valid_wal_without_shm() -> Result<()> {
        // WAL-without-SHM is an informational Ok for FrankenSQLite compatibility.
        // repair_recoverable_db_state should NOT attempt any repair for this condition.
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir)?;
        let db_path = beads_dir.join("beads.db");
        fs::write(&db_path, b"not a sqlite database")?;
        let wal_path = PathBuf::from(format!("{}-wal", db_path.to_string_lossy()));
        fs::write(&wal_path, b"frankensqlite wal without shm")?;

        let mut checks = Vec::new();
        check_database_sidecars(&db_path, &mut checks)?;
        assert_eq!(
            find_check(&checks, "db.sidecars").map(|check| &check.status),
            Some(&CheckStatus::Ok)
        );
        let report = DoctorReport {
            ok: true,
            workspace_health: None,
            reliability_audit: None,
            checks,
        };

        let repair = repair_recoverable_db_state(
            &beads_dir,
            &db_path,
            &report,
            None,
            &FixerFilter::default(),
        );
        assert!(
            repair.quarantined_artifacts.is_empty(),
            "valid FrankenSQLite WAL should not be quarantined"
        );
        assert!(
            wal_path.exists(),
            "valid FrankenSQLite WAL should remain untouched"
        );
        Ok(())
    }

    #[test]
    fn test_report_has_blocked_cache_stale_finding_detects_detail_entry() {
        let report = DoctorReport {
            ok: false,
            workspace_health: None,
            reliability_audit: None,
            checks: vec![CheckResult {
                name: "db.recoverable_anomalies".to_string(),
                status: CheckStatus::Error,
                message: Some("config contains duplicate rows".to_string()),
                details: Some(serde_json::json!({
                    "findings": [
                        "config contains duplicate rows for key 'issue_prefix' (2 rows)",
                        BLOCKED_CACHE_STALE_FINDING,
                    ]
                })),
            }],
        };

        assert!(report_has_blocked_cache_stale_finding(&report));
    }

    #[test]
    fn test_report_has_blocked_cache_stale_finding_ignores_other_recoverable_errors() {
        let report = DoctorReport {
            ok: false,
            workspace_health: None,
            reliability_audit: None,
            checks: vec![CheckResult {
                name: "db.recoverable_anomalies".to_string(),
                status: CheckStatus::Error,
                message: Some("config contains duplicate rows".to_string()),
                details: Some(serde_json::json!({
                    "findings": [
                        "config contains duplicate rows for key 'issue_prefix' (2 rows)",
                        "metadata contains duplicate rows for key 'project' (2 rows)",
                    ]
                })),
            }],
        };

        assert!(!report_has_blocked_cache_stale_finding(&report));
    }

    #[test]
    fn test_check_issue_write_probe_succeeds_on_healthy_database() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");

        {
            let mut storage = SqliteStorage::open(&db_path).unwrap();
            storage
                .create_issue(&sample_issue("bd-probe", "Probe me"), "tester")
                .unwrap();
        }

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let mut checks = Vec::new();
        check_issue_write_probe(&conn, &mut checks);

        let check = find_check(&checks, "db.write_probe").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
        assert!(
            check
                .message
                .as_deref()
                .is_some_and(|message| message.contains("bd-probe")),
            "unexpected check message: {:?}",
            check.message
        );
    }

    #[test]
    fn test_inspect_existing_doctor_database_uses_snapshot_write_probe() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");

        {
            let mut storage = SqliteStorage::open(&db_path).unwrap();
            storage
                .create_issue(&sample_issue("bd-probe", "Probe me"), "tester")
                .unwrap();
        }

        let lock_conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        lock_conn.execute("BEGIN IMMEDIATE").unwrap();

        let mut checks = Vec::new();
        inspect_existing_doctor_database(
            &db_path,
            None,
            JsonlCountState::Missing,
            DoctorInspectionMode::Full,
            &mut checks,
        );

        let check = find_check(&checks, "db.write_probe").expect("check present");
        assert!(
            matches!(check.status, CheckStatus::Ok),
            "unexpected snapshot write probe status: {:?}",
            check.status
        );
        assert!(
            check.message.as_deref().is_some_and(|message| {
                message.contains("Rollback-only issue write succeeded for bd-probe")
            }),
            "unexpected check message: {:?}",
            check.message
        );

        lock_conn.execute("ROLLBACK").unwrap();
    }

    #[test]
    fn test_build_issue_write_probe_check_marks_rollback_failure_as_error() {
        let check = build_issue_write_probe_check(
            "bd-probe",
            Ok(1),
            Err(FrankenError::Internal("rollback failed".to_string())),
        );

        assert!(matches!(check.status, CheckStatus::Error));
        assert!(
            check
                .message
                .as_deref()
                .is_some_and(|message| message.contains("rollback failed")),
            "unexpected check message: {:?}",
            check.message
        );
        assert_eq!(check.details.unwrap()["issue_id"], "bd-probe");
    }

    #[test]
    fn test_build_issue_write_probe_check_marks_zero_row_update_as_error() {
        let check = build_issue_write_probe_check("bd-probe", Ok(0), Ok(0));

        assert!(matches!(check.status, CheckStatus::Error));
        assert!(
            check
                .message
                .as_deref()
                .is_some_and(|message| message.contains("affected 0 rows")),
            "unexpected check message: {:?}",
            check.message
        );
        let details = check
            .details
            .expect("zero-row error should include details");
        assert_eq!(details["issue_id"], "bd-probe");
        assert_eq!(details["affected_rows"], 0);
    }

    #[test]
    fn test_build_issue_write_probe_check_reports_zero_row_update_before_rollback_failure() {
        let check = build_issue_write_probe_check(
            "bd-probe",
            Ok(0),
            Err(FrankenError::Internal("rollback failed".to_string())),
        );

        assert!(matches!(check.status, CheckStatus::Error));
        assert!(
            check
                .message
                .as_deref()
                .is_some_and(|message| message.contains("affected 0 rows")),
            "unexpected check message: {:?}",
            check.message
        );
        let details = check
            .details
            .expect("rollback failure should include details");
        assert_eq!(details["issue_id"], "bd-probe");
        assert_eq!(details["affected_rows"], 0);
        assert!(
            details["rollback_error"]
                .as_str()
                .is_some_and(|message| message.contains("rollback failed")),
            "unexpected rollback error detail: {}",
            details["rollback_error"]
        );
    }

    #[test]
    fn test_build_issue_write_probe_check_preserves_write_failure() {
        let check = build_issue_write_probe_check(
            "bd-probe",
            Err(FrankenError::Internal("write failed".to_string())),
            Ok(0),
        );

        assert!(matches!(check.status, CheckStatus::Error));
        assert!(
            check
                .message
                .as_deref()
                .is_some_and(|message| message.contains("write failed")),
            "unexpected check message: {:?}",
            check.message
        );
    }

    #[test]
    fn test_repair_database_from_jsonl_restores_original_db_on_import_failure() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");

        {
            let mut storage = SqliteStorage::open(&db_path).unwrap();
            let issue = Issue {
                id: "bd-keep".to_string(),
                content_hash: None,
                title: "Keep me".to_string(),
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
                bypassed_policy: None,
                bypass_reason: None,
                policy_gates_fired: None,
                due_at: None,
                defer_until: None,
                external_ref: None,
                source_system: None,
                source_repo: None,
                source_repo_path: None,
                agent_context: None,
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
                labels: Vec::new(),
                dependencies: Vec::new(),
                comments: Vec::new(),
            };
            storage.create_issue(&issue, "tester").unwrap();
        }

        fs::write(&jsonl_path, "not valid json\n").unwrap();

        let err = repair_database_from_jsonl(
            &beads_dir,
            &db_path,
            &jsonl_path,
            &config::CliOverrides::default(),
            false,
            None,
        )
        .unwrap_err();
        let err_msg = err.to_string();
        assert!(
            err_msg.contains("invalid issue record")
                || err_msg.contains("Preflight checks failed")
                || err_msg.contains("Invalid JSON"),
            "unexpected error: {err}"
        );

        let reopened = SqliteStorage::open(&db_path).unwrap();
        let issue = reopened
            .get_issue("bd-keep")
            .unwrap()
            .expect("original DB should be restored after failed repair");
        assert_eq!(issue.title, "Keep me");

        let recovery_dir = beads_dir.join(".br_recovery");
        let backup_count =
            fs::read_dir(&recovery_dir).map_or(0, |entries| entries.flatten().count());
        assert_eq!(
            backup_count, 0,
            "preflight failures should not create recovery backups"
        );
    }

    #[test]
    fn test_repair_database_from_jsonl_refuses_conflict_markers_without_backup() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");

        {
            let mut storage = SqliteStorage::open(&db_path).unwrap();
            storage
                .create_issue(&sample_issue("bd-keep", "Keep me"), "tester")
                .unwrap();
        }

        fs::write(
            &jsonl_path,
            "<<<<<<< HEAD\n{\"id\":\"bd-keep\"}\n=======\n{\"id\":\"bd-other\"}\n>>>>>>> branch\n",
        )
        .unwrap();

        let err = repair_database_from_jsonl(
            &beads_dir,
            &db_path,
            &jsonl_path,
            &config::CliOverrides::default(),
            false,
            None,
        )
        .unwrap_err();
        let err_msg = err.to_string();
        assert!(
            err_msg.contains(JSONL_REBUILD_AUTHORITY_ERROR_PREFIX)
                && err_msg.contains("merge conflict marker"),
            "unexpected error: {err_msg}"
        );
        assert_eq!(jsonl_rebuild_failure_outcome(&err), "refused");

        let reopened = SqliteStorage::open(&db_path).unwrap();
        let issue = reopened
            .get_issue("bd-keep")
            .unwrap()
            .expect("original DB should remain untouched after refused repair");
        assert_eq!(issue.title, "Keep me");

        let recovery_dir = beads_dir.join(".br_recovery");
        let backup_count =
            fs::read_dir(&recovery_dir).map_or(0, |entries| entries.flatten().count());
        assert_eq!(
            backup_count, 0,
            "JSONL authority preflight failures should not create recovery backups"
        );
    }

    #[test]
    fn test_repair_database_from_jsonl_refuses_duplicate_ids_without_backup() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");

        {
            let mut storage = SqliteStorage::open(&db_path).unwrap();
            storage
                .create_issue(&sample_issue("bd-keep", "Keep me"), "tester")
                .unwrap();
        }

        let issue = sample_issue("bd-dup", "Duplicate");
        let issue_json = serde_json::to_string(&issue).unwrap();
        fs::write(&jsonl_path, format!("{issue_json}\n{issue_json}\n")).unwrap();

        let err = repair_database_from_jsonl(
            &beads_dir,
            &db_path,
            &jsonl_path,
            &config::CliOverrides::default(),
            false,
            None,
        )
        .unwrap_err();
        let err_msg = err.to_string();
        assert!(
            err_msg.contains(JSONL_REBUILD_AUTHORITY_ERROR_PREFIX)
                && err_msg.contains("Duplicate issue id 'bd-dup'"),
            "unexpected error: {err_msg}"
        );
        assert_eq!(jsonl_rebuild_failure_outcome(&err), "refused");

        let reopened = SqliteStorage::open(&db_path).unwrap();
        let issue = reopened
            .get_issue("bd-keep")
            .unwrap()
            .expect("original DB should remain untouched after refused repair");
        assert_eq!(issue.title, "Keep me");

        let recovery_dir = beads_dir.join(".br_recovery");
        let backup_count =
            fs::read_dir(&recovery_dir).map_or(0, |entries| entries.flatten().count());
        assert_eq!(
            backup_count, 0,
            "JSONL authority preflight failures should not create recovery backups"
        );
    }

    #[test]
    fn locked_database_failure_does_not_blame_the_jsonl() {
        // beads_rust-krdi: when another process holds the workspace the engine
        // reports "unable to open database file". The old residual bucket told
        // the operator the export was corrupt and to hand-edit it — advice that
        // is both wrong and destructive to a healthy file.
        let err = BeadsError::Database(FrankenError::CannotOpen {
            path: PathBuf::from("/tmp/ws/.beads/beads.db"),
        });
        assert_eq!(
            err.to_string(),
            "Database error: unable to open database file: '/tmp/ws/.beads/beads.db'",
            "the engine wording this classifier exists for"
        );
        assert!(is_database_unavailable_failure(&err));
        assert!(!is_jsonl_content_failure(&err));

        // The other ways the workspace can be held are classified the same way.
        for held in [
            BeadsError::Database(FrankenError::DatabaseLocked {
                path: PathBuf::from("/tmp/ws/.beads/beads.db"),
            }),
            BeadsError::Database(FrankenError::LockFailed {
                detail: "F_SETLK contention beyond busy_timeout".to_string(),
            }),
            BeadsError::Database(FrankenError::Busy),
        ] {
            assert!(
                is_database_unavailable_failure(&held),
                "should classify as unavailable: {held}"
            );
        }

        let message = jsonl_rebuild_failure_message(&err);
        assert!(
            message.contains("the JSONL is not implicated"),
            "message must clear the export: {message}"
        );
        assert!(
            message.contains(".beads/.write.lock") && message.contains("br serve"),
            "message must name the real remedy: {message}"
        );
        assert!(
            !message.to_lowercase().contains("jsonl file may be corrupt")
                && !message.contains("manually editing"),
            "message must not advise editing a healthy JSONL: {message}"
        );
    }

    #[test]
    fn jsonl_content_failure_still_points_at_the_export() {
        // The one class where blaming the JSONL is correct keeps doing so.
        let err = BeadsError::JsonlParse {
            line: 12,
            reason: "missing field `title`".to_string(),
        };
        assert!(is_jsonl_content_failure(&err));
        assert!(!is_database_unavailable_failure(&err));

        let message = jsonl_rebuild_failure_message(&err);
        assert!(
            message.contains(".beads/issues.jsonl") && message.contains("rejected records"),
            "message should direct the operator at the export: {message}"
        );
    }

    #[test]
    fn unclassified_failure_attributes_no_cause() {
        // The residual case must describe what happened without inventing a
        // culprit, and must state that nothing was written.
        let err = BeadsError::Config("run directory is not writable".to_string());
        assert!(!is_database_unavailable_failure(&err));
        assert!(!is_jsonl_content_failure(&err));

        let message = jsonl_rebuild_failure_message(&err);
        assert!(
            message.contains("No database writes were applied"),
            "message should state the workspace is unchanged: {message}"
        );
        assert!(
            !message.to_lowercase().contains("corrupt"),
            "residual failures must not assert corruption: {message}"
        );
    }

    #[test]
    fn database_unavailable_classification_sees_through_context_wrappers() {
        let wrapped = BeadsError::WithContext {
            context: "rebuilding database family".to_string(),
            source: Box::new(BeadsError::DatabaseLocked {
                path: PathBuf::from("/tmp/ws/.beads/beads.db"),
            }),
        };
        assert!(is_database_unavailable_failure(&wrapped));
    }

    #[test]
    fn dry_run_skip_is_reported_as_skipped_not_as_a_failure() {
        // beads_rust-krdi: `--repair --dry-run` declined to write. Nothing
        // failed, so it must not be wrapped in a repair-failure message.
        let err = BeadsError::Config(JSONL_REBUILD_DRY_RUN_SKIP_MESSAGE.to_string());
        let outcome = jsonl_rebuild_failure_outcome(&err);
        assert_eq!(outcome, "skipped");
        assert!(jsonl_rebuild_error_is_self_describing(outcome));

        // The authority refusal keeps its own verbatim treatment...
        let refused = BeadsError::Config(format!(
            "{JSONL_REBUILD_AUTHORITY_ERROR_PREFIX}: found 1 merge conflict marker(s)"
        ));
        assert!(jsonl_rebuild_error_is_self_describing(
            jsonl_rebuild_failure_outcome(&refused)
        ));
        // ...while a genuine failure is still wrapped.
        assert!(!jsonl_rebuild_error_is_self_describing(
            jsonl_rebuild_failure_outcome(&BeadsError::Config("disk full".to_string()))
        ));
    }

    #[test]
    fn test_repair_database_from_jsonl_restores_issue_prefix_from_jsonl() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");

        let issue = Issue {
            id: "proj-abc123".to_string(),
            content_hash: None,
            title: "Imported".to_string(),
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
            bypassed_policy: None,
            bypass_reason: None,
            policy_gates_fired: None,
            due_at: None,
            defer_until: None,
            external_ref: None,
            source_system: None,
            source_repo: None,
            source_repo_path: None,
            agent_context: None,
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
            labels: Vec::new(),
            dependencies: Vec::new(),
            comments: Vec::new(),
        };
        fs::write(
            &jsonl_path,
            format!("{}\n", serde_json::to_string(&issue).unwrap()),
        )
        .unwrap();

        let result = repair_database_from_jsonl(
            &beads_dir,
            &db_path,
            &jsonl_path,
            &config::CliOverrides::default(),
            false,
            None,
        )
        .unwrap();

        assert_eq!(result.imported, 1);

        let reopened = SqliteStorage::open(&db_path).unwrap();
        assert_eq!(
            reopened.get_config("issue_prefix").unwrap().as_deref(),
            Some("proj")
        );
    }

    #[test]
    fn test_repair_database_from_jsonl_records_legacy_op_when_session_present() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let db_path = beads_dir.join("beads.db");
        let jsonl_path = beads_dir.join("issues.jsonl");

        {
            let mut storage = SqliteStorage::open(&db_path).unwrap();
            storage
                .create_issue(&sample_issue("bd-old", "Old"), "tester")
                .unwrap();
        }
        let pre_rebuild_db = fs::read(&db_path).unwrap();

        let issue = sample_issue("bd-new", "New");
        fs::write(
            &jsonl_path,
            format!("{}\n", serde_json::to_string(&issue).unwrap()),
        )
        .unwrap();

        let mut session = DoctorRepairSession::new(temp.path(), false).unwrap();
        let result = repair_database_from_jsonl(
            &beads_dir,
            &db_path,
            &jsonl_path,
            &config::CliOverrides::default(),
            false,
            Some(&mut session),
        )
        .unwrap();

        assert_eq!(result.imported, 1);

        let backup = session.run.root.join("backups").join(".beads/beads.db");
        assert_eq!(fs::read(&backup).unwrap(), pre_rebuild_db);

        let actions = fs::read_to_string(&session.run.actions_file).unwrap();
        let mut found_jsonl_rebuild_action = false;
        for line in actions.lines() {
            let action: serde_json::Value = serde_json::from_str(line).unwrap();
            if action["op"] == "legacy_op"
                && action["fixer_id"] == "doctor.jsonl_rebuild"
                && action["path"] == ".beads/beads.db"
                && action["ok"] == true
            {
                found_jsonl_rebuild_action = true;
            }
        }
        assert!(
            found_jsonl_rebuild_action,
            "expected a successful doctor.jsonl_rebuild action; got {actions}"
        );

        let reopened = SqliteStorage::open(&db_path).unwrap();
        assert!(
            reopened.get_issue("bd-new").unwrap().is_some(),
            "rebuilt database should contain the JSONL issue"
        );
    }

    /// The `.beads/`-relative names of every database-family file that
    /// actually exists on disk.
    ///
    /// Derived from a directory listing rather than a hardcoded list: which
    /// sidecars the storage engine leaves behind is an fsqlite implementation
    /// detail (0.1.18 added `-fsqlite-ns-gate` / `-fsqlite-ns-use` and now
    /// also retains `-shm`). The contract these tests assert is "the legacy
    /// audit covers the whole existing family", which must not have to be
    /// restated every time the engine adds a sidecar.
    fn on_disk_db_family_names(beads_dir: &Path, db_file_name: &str) -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        for entry in fs::read_dir(beads_dir).unwrap() {
            let entry = entry.unwrap();
            if !entry.file_type().unwrap().is_file() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(db_file_name) {
                names.insert(format!(".beads/{name}"));
            }
        }
        names
    }

    #[test]
    fn test_existing_sqlite_family_paths_for_legacy_op_includes_only_existing_sidecars() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let wal_path = sqlite_wal_sidecar_path(&db_path);
        let shm_path = sqlite_shm_sidecar_path(&db_path);
        let journal_path = sqlite_journal_sidecar_path(&db_path);
        let wal_cert_paths = fsqlite_wal_cert_sidecar_paths(&db_path);
        let wal_cert_path = wal_cert_paths[0].clone();
        let wal_cert_head_path = wal_cert_paths[1].clone();

        fs::write(&db_path, b"db").unwrap();
        fs::write(&wal_path, b"wal").unwrap();
        fs::write(&journal_path, b"journal").unwrap();
        fs::write(&wal_cert_path, b"wal-cert").unwrap();

        let paths = existing_sqlite_family_paths_for_legacy_op(&db_path);
        assert_eq!(paths, vec![db_path, wal_path, journal_path, wal_cert_path]);
        assert!(!paths.contains(&shm_path));
        assert!(!paths.contains(&wal_cert_head_path));
    }

    #[test]
    fn test_repair_via_vacuum_records_existing_sqlite_family_paths() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");

        {
            let mut storage = SqliteStorage::open(&db_path).unwrap();
            storage
                .create_issue(&sample_issue("bd-vac-family", "Vacuum family"), "tester")
                .unwrap();
        }

        let wal_path = sqlite_wal_sidecar_path(&db_path);
        let journal_path = sqlite_journal_sidecar_path(&db_path);
        fs::write(&wal_path, b"wal-before-vacuum").unwrap();
        fs::write(&journal_path, b"journal-before-vacuum").unwrap();

        let write_authority = Arc::new(
            crate::sync::blocking_database_family_write_lock_with_timeout(
                &beads_dir,
                &db_path,
                Some(1_000),
            )
            .unwrap(),
        );
        write_authority.bind_database_inode_for_mutation().unwrap();
        let mut session = DoctorRepairSession::new(temp.path(), false).unwrap();
        let mut repair = LocalRepairResult::default();
        repair_via_vacuum(&db_path, &mut repair, Some(&mut session), &write_authority);

        let actions = fs::read_to_string(&session.run.actions_file).unwrap();
        let action_paths: BTreeSet<String> = actions
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|action| {
                action["op"] == "legacy_op" && action["fixer_id"] == "repair_via_vacuum"
            })
            .map(|action| action["path"].as_str().unwrap().to_string())
            .collect();

        assert_eq!(
            action_paths,
            on_disk_db_family_names(&beads_dir, "beads.db"),
            "VACUUM legacy audit must cover the existing SQLite file family; actions={actions}"
        );
        assert_eq!(
            fs::read(session.run.root.join("backups/.beads/beads.db-wal")).unwrap(),
            b"wal-before-vacuum"
        );
        assert_eq!(
            fs::read(session.run.root.join("backups/.beads/beads.db-journal")).unwrap(),
            b"journal-before-vacuum"
        );
    }

    #[test]
    fn test_repair_partial_indexes_records_existing_sqlite_family_paths() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");

        {
            let mut storage = SqliteStorage::open(&db_path).unwrap();
            storage
                .create_issue(&sample_issue("bd-ri-family", "Index family"), "tester")
                .unwrap();
        }

        let wal_path = sqlite_wal_sidecar_path(&db_path);
        let journal_path = sqlite_journal_sidecar_path(&db_path);
        fs::write(&wal_path, b"wal-prestate").unwrap();
        fs::write(&journal_path, b"journal-prestate").unwrap();

        let mut session = DoctorRepairSession::new(temp.path(), false).unwrap();
        let mut repair = LocalRepairResult::default();
        repair_partial_indexes(&db_path, &mut repair, Some(&mut session));

        let actions = fs::read_to_string(&session.run.actions_file).unwrap();
        let action_paths: BTreeSet<String> = actions
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|action| {
                action["op"] == "legacy_op" && action["fixer_id"] == "repair_partial_indexes"
            })
            .map(|action| action["path"].as_str().unwrap().to_string())
            .collect();

        assert_eq!(
            action_paths,
            on_disk_db_family_names(&beads_dir, "beads.db"),
            "REINDEX legacy audit must cover the existing SQLite file family; actions={actions}"
        );
        assert_eq!(
            fs::read(session.run.root.join("backups/.beads/beads.db-wal")).unwrap(),
            b"wal-prestate"
        );
        assert_eq!(
            fs::read(session.run.root.join("backups/.beads/beads.db-journal")).unwrap(),
            b"journal-prestate"
        );
    }

    #[test]
    fn test_repair_recoverable_db_state_records_existing_sqlite_family_paths() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");

        {
            let mut storage = SqliteStorage::open(&db_path).unwrap();
            let blocker = sample_issue("bd-cache-blocker", "Cache blocker");
            let target = sample_issue("bd-cache-target", "Cache target");
            storage.create_issue(&blocker, "tester").unwrap();
            storage.create_issue(&target, "tester").unwrap();
            storage
                .add_dependency(&target.id, &blocker.id, "blocks", "tester")
                .unwrap();
            assert!(storage.ensure_blocked_cache_fresh().unwrap());
            storage
                .execute_test_sql(
                    "UPDATE blocked_issues_cache
                     SET blocked_by = '[\"bd-other:open\"]'
                     WHERE issue_id = 'bd-cache-target'",
                )
                .unwrap();
        }

        // A short garbage WAL both marks the backup content and blocks the
        // engine from opening the database — the exact case the local repair
        // now clears by quarantining the sidecar and retrying. (Journal-family
        // backup coverage lives in the VACUUM and REINDEX tests, whose repair
        // paths do not need a successful `SqliteStorage::open`.)
        let wal_path = sqlite_wal_sidecar_path(&db_path);
        fs::write(&wal_path, b"wal-before-blocked-cache").unwrap();

        // The audit records the family as it existed going in; the repair may
        // legitimately move a corrupt sidecar aside while it runs.
        let expected_family = on_disk_db_family_names(&beads_dir, "beads.db");

        let report = DoctorReport {
            ok: true,
            workspace_health: None,
            reliability_audit: None,
            checks: vec![CheckResult {
                name: "db.recoverable_anomalies".to_string(),
                status: CheckStatus::Warn,
                message: Some(BLOCKED_CACHE_CONTENT_MISMATCH_FINDING.to_string()),
                details: Some(serde_json::json!({
                    "findings": [BLOCKED_CACHE_CONTENT_MISMATCH_FINDING],
                })),
            }],
        };
        let mut session = DoctorRepairSession::new(temp.path(), false).unwrap();
        let repair = repair_recoverable_db_state(
            &beads_dir,
            &db_path,
            &report,
            Some(&mut session),
            &FixerFilter::default(),
        );

        assert!(repair.blocked_cache_rebuilt);
        let actions = fs::read_to_string(&session.run.actions_file).unwrap();
        let action_paths: BTreeSet<String> = actions
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|action| {
                action["op"] == "legacy_op" && action["fixer_id"] == "repair_recoverable_db_state"
            })
            .map(|action| action["path"].as_str().unwrap().to_string())
            .collect();

        assert_eq!(
            action_paths, expected_family,
            "blocked-cache rebuild legacy audit must cover the existing SQLite file family; actions={actions}"
        );
        assert_eq!(
            fs::read(session.run.root.join("backups/.beads/beads.db-wal")).unwrap(),
            b"wal-before-blocked-cache",
            "the pre-repair WAL must be backed up verbatim before it is quarantined"
        );
    }

    #[test]
    fn test_repair_recoverable_db_state_skips_missing_db() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = temp.path().join("missing.db");
        let report = DoctorReport {
            ok: false,
            workspace_health: None,
            reliability_audit: None,
            checks: Vec::new(),
        };

        let local_repair = repair_recoverable_db_state(
            &beads_dir,
            &db_path,
            &report,
            None,
            &FixerFilter::default(),
        );
        assert!(!local_repair.blocked_cache_rebuilt);
    }

    // ===================================================================
    // #253: WARN-level page anomalies left by the light-repair pass
    // should trigger a follow-up VACUUM so the DB ends in a clean state
    // and subsequent `--repair` runs don't report "nothing to repair"
    // with integrity_check still dirty.
    // ===================================================================

    #[test]
    fn report_has_warn_level_page_anomaly_matches_orphan_page_warn() {
        for msg in [
            "page 55 is never used",
            "*** in database main ***; Page 55: never used; Page 264: never used",
            "database disk image is malformed",
            "Tree 28 page 28: free space corruption",
        ] {
            let report = DoctorReport {
                ok: true, // WARNs don't flip ok → false
                workspace_health: None,
                reliability_audit: None,
                checks: vec![CheckResult {
                    name: "sqlite.integrity_check".to_string(),
                    status: CheckStatus::Warn,
                    message: Some(msg.to_string()),
                    details: None,
                }],
            };
            assert!(
                report_has_warn_level_page_anomaly(&report),
                "expected WARN-level page anomaly to match: {msg:?}"
            );
        }

        // sqlite3 binary variant should also match (the C sqlite3 cross-check).
        let report = DoctorReport {
            ok: true,
            workspace_health: None,
            reliability_audit: None,
            checks: vec![CheckResult {
                name: "sqlite3.integrity_check".to_string(),
                status: CheckStatus::Warn,
                message: Some("Page 55: never used".to_string()),
                details: None,
            }],
        };
        assert!(report_has_warn_level_page_anomaly(&report));
    }

    #[test]
    fn report_has_warn_level_page_anomaly_ignores_non_page_warns() {
        // "missing from index" is a partial-index REINDEX case, not a VACUUM case.
        let report = DoctorReport {
            ok: true,
            workspace_health: None,
            reliability_audit: None,
            checks: vec![CheckResult {
                name: "sqlite.integrity_check".to_string(),
                status: CheckStatus::Warn,
                message: Some("row 42 missing from index idx_foo".to_string()),
                details: None,
            }],
        };
        assert!(!report_has_warn_level_page_anomaly(&report));

        // Out-of-order index warning: known frankensqlite DESC-index artifact,
        // not a VACUUM case.
        let report = DoctorReport {
            ok: true,
            workspace_health: None,
            reliability_audit: None,
            checks: vec![CheckResult {
                name: "sqlite.integrity_check".to_string(),
                status: CheckStatus::Warn,
                message: Some("out of order index idx_foo".to_string()),
                details: None,
            }],
        };
        assert!(!report_has_warn_level_page_anomaly(&report));
    }

    #[test]
    fn report_has_warn_level_page_anomaly_ignores_error_level_findings() {
        // ERROR-level findings are handled by the existing
        // `report_has_page_corruption` path; this predicate is
        // specifically scoped to WARN-level residue left after light repair.
        let report = DoctorReport {
            ok: false,
            workspace_health: None,
            reliability_audit: None,
            checks: vec![CheckResult {
                name: "sqlite.integrity_check".to_string(),
                status: CheckStatus::Error,
                message: Some("page 55 is never used".to_string()),
                details: None,
            }],
        };
        assert!(!report_has_warn_level_page_anomaly(&report));
    }

    #[test]
    fn report_has_warn_level_page_anomaly_ignores_non_integrity_checks() {
        let report = DoctorReport {
            ok: true,
            workspace_health: None,
            reliability_audit: None,
            checks: vec![CheckResult {
                name: "db.sidecars".to_string(),
                status: CheckStatus::Warn,
                message: Some("WAL sidecar exists without a matching SHM sidecar".to_string()),
                details: None,
            }],
        };
        assert!(!report_has_warn_level_page_anomaly(&report));
    }

    #[test]
    fn warning_repair_verified_requires_repaired_page_warning_to_clear() {
        let dirty_report = DoctorReport {
            ok: true,
            workspace_health: None,
            reliability_audit: None,
            checks: vec![CheckResult {
                name: "sqlite3.integrity_check".to_string(),
                status: CheckStatus::Warn,
                message: Some("Page 55: never used".to_string()),
                details: None,
            }],
        };
        assert!(!warning_repair_verified(&dirty_report, false, false));

        let clean_report = DoctorReport {
            ok: true,
            workspace_health: None,
            reliability_audit: None,
            checks: vec![CheckResult {
                name: "sqlite3.integrity_check".to_string(),
                status: CheckStatus::Ok,
                message: None,
                details: None,
            }],
        };
        assert!(warning_repair_verified(&clean_report, false, false));
    }

    #[test]
    fn jsonl_rebuild_verification_tolerates_benign_post_rebuild_warnings() {
        // Issue #375: a successful rebuild-from-JSONL always leaves a fresh
        // recovery backup and can inherit a RUST_LOG nag from the caller's
        // environment. Neither means the rebuilt DB is unhealthy, so
        // post-repair verification must still pass.
        let report = DoctorReport {
            ok: true,
            workspace_health: None,
            reliability_audit: None,
            checks: vec![
                CheckResult {
                    name: "rust_log".to_string(),
                    status: CheckStatus::Warn,
                    message: Some("RUST_LOG=beads_rust=debug is verbose".to_string()),
                    details: None,
                },
                CheckResult {
                    name: "db.recovery_artifacts".to_string(),
                    status: CheckStatus::Warn,
                    message: Some("Preserved recovery artifacts remain (2 item(s))".to_string()),
                    details: None,
                },
                CheckResult {
                    name: "sqlite.integrity_check".to_string(),
                    status: CheckStatus::Ok,
                    message: None,
                    details: None,
                },
            ],
        };
        // Strict verifier (used by the light-repair path) rejects these...
        assert!(!repair_report_verified(&report));
        // ...but the rebuild path's verifier tolerates them.
        assert!(jsonl_rebuild_repair_verified(&report, &[]));
    }

    #[test]
    fn jsonl_rebuild_verification_still_fails_on_real_defects() {
        // An ERROR-level finding (e.g. rebuilt DB still not openable) flips
        // report.ok to false and must fail verification.
        let error_report = DoctorReport {
            ok: false,
            workspace_health: None,
            reliability_audit: None,
            checks: vec![CheckResult {
                name: "db.open".to_string(),
                status: CheckStatus::Error,
                message: Some("Failed to open DB snapshot for inspection".to_string()),
                details: None,
            }],
        };
        assert!(!jsonl_rebuild_repair_verified(&error_report, &[]));

        // A non-benign WARN (page anomaly) is NOT tolerated even though
        // report.ok is true: it signals residual corruption after rebuild.
        let page_warn_report = DoctorReport {
            ok: true,
            workspace_health: None,
            reliability_audit: None,
            checks: vec![CheckResult {
                name: "sqlite3.integrity_check".to_string(),
                status: CheckStatus::Warn,
                message: Some("Page 55: never used".to_string()),
                details: None,
            }],
        };
        assert!(!jsonl_rebuild_repair_verified(&page_warn_report, &[]));

        // A db.sidecars ERROR (dangling non-file sidecar) is not tolerated
        // even though the check name is on the benign list.
        let sidecar_error_report = DoctorReport {
            ok: false,
            workspace_health: None,
            reliability_audit: None,
            checks: vec![CheckResult {
                name: "db.sidecars".to_string(),
                status: CheckStatus::Error,
                message: Some("WAL sidecar is a directory instead of a regular file".to_string()),
                details: None,
            }],
        };
        assert!(!jsonl_rebuild_repair_verified(&sidecar_error_report, &[]));
    }

    /// Build a `sync.metadata` WARN carrying the drift flags `check_sync_metadata`
    /// emits, so the verification predicate is exercised against the real detail
    /// shape rather than a hand-waved one.
    fn sync_metadata_warn(pending_import: bool, pending_export: bool) -> CheckResult {
        CheckResult {
            name: "sync.metadata".to_string(),
            status: CheckStatus::Warn,
            message: Some("Local changes exist but no export is recorded".to_string()),
            details: Some(serde_json::json!({
                "pending_import": pending_import,
                "pending_export": pending_export,
                "jsonl_newer": pending_import,
                "db_newer": pending_export,
                "dirty_issues": 1,
            })),
        }
    }

    fn report_with(checks: Vec<CheckResult>) -> DoctorReport {
        DoctorReport {
            ok: true,
            workspace_health: None,
            reliability_audit: None,
            checks,
        }
    }

    #[test]
    fn jsonl_rebuild_verification_tolerates_pending_export_after_rebuild() {
        // beads_rust-a53h: a rebuild restores unflushed tombstones that the
        // JSONL does not carry, so the fresh database is legitimately ahead of
        // the export with no last_export_time recorded. That is the repair
        // preserving local state, not a defect, and it must not make a
        // successful repair exit 7.
        let report = report_with(vec![sync_metadata_warn(false, true)]);
        assert!(jsonl_rebuild_repair_verified(&report, &[]));
        // The strict verifier used by the light-repair path is unchanged.
        assert!(!repair_report_verified(&report));
    }

    #[test]
    fn jsonl_rebuild_verification_rejects_pending_import_after_rebuild() {
        // The other drift direction stays fatal: we just rebuilt the database
        // *from* the JSONL, so a JSONL that still reads newer means the rebuild
        // did not land what it read.
        assert!(!jsonl_rebuild_repair_verified(
            &report_with(vec![sync_metadata_warn(true, false)]),
            &[]
        ));
        // Divergence (both directions) is likewise not benign.
        assert!(!jsonl_rebuild_repair_verified(
            &report_with(vec![sync_metadata_warn(true, true)]),
            &[]
        ));
    }

    #[test]
    fn sync_metadata_benignity_fails_closed_without_drift_details() {
        // Missing details must not be read as "pending export only".
        let no_details = CheckResult {
            name: "sync.metadata".to_string(),
            status: CheckStatus::Warn,
            message: Some("Local changes pending export".to_string()),
            details: None,
        };
        assert!(!is_pending_export_only_sync_metadata(&no_details));
        assert!(!jsonl_rebuild_repair_verified(
            &report_with(vec![no_details]),
            &[]
        ));

        // So must details that carry only one half of the pair.
        let half_details = CheckResult {
            name: "sync.metadata".to_string(),
            status: CheckStatus::Warn,
            message: Some("Local changes pending export".to_string()),
            details: Some(serde_json::json!({ "pending_export": true })),
        };
        assert!(!is_pending_export_only_sync_metadata(&half_details));
    }

    /// Build the `counts.db_vs_jsonl` WARN shape `check_available_db_count`
    /// emits, so the preserved-dirty predicate is exercised against the real
    /// detail layout.
    fn count_divergence_warn(only_db: &[&str], only_jsonl: &[&str]) -> CheckResult {
        CheckResult {
            name: "counts.db_vs_jsonl".to_string(),
            status: CheckStatus::Warn,
            message: Some("DB and JSONL counts differ".to_string()),
            details: Some(serde_json::json!({
                "db": 1 + only_db.len(),
                "jsonl": 1 + only_jsonl.len(),
                "id_delta": {
                    "only_db_count": only_db.len(),
                    "only_jsonl_count": only_jsonl.len(),
                    "both_count": 1,
                    "only_db": only_db,
                    "only_jsonl": only_jsonl,
                    "preview_limit": 50,
                },
            })),
        }
    }

    #[test]
    fn jsonl_rebuild_verification_tolerates_preserved_dirty_count_divergence() {
        // GitHub #394: restoring a dirty unflushed issue across the rebuild
        // leaves the DB one row ahead of the JSONL until the next flush.
        // That exact divergence — and only that one — must not fail
        // verification.
        let preserved = vec!["bd-kept".to_string()];
        let report = report_with(vec![count_divergence_warn(&["bd-kept"], &[])]);
        assert!(jsonl_rebuild_repair_verified(&report, &preserved));

        // Without a preserved set, the same divergence stays fatal.
        assert!(!jsonl_rebuild_repair_verified(&report, &[]));
    }

    #[test]
    fn preserved_dirty_count_divergence_fails_closed() {
        let preserved = vec!["bd-kept".to_string()];

        // A DB-only id we did NOT restore is unexplained drift.
        assert!(!jsonl_rebuild_repair_verified(
            &report_with(vec![count_divergence_warn(&["bd-kept", "bd-mystery"], &[])]),
            &preserved
        ));

        // A row missing from the DB is record loss, never benign.
        assert!(!jsonl_rebuild_repair_verified(
            &report_with(vec![count_divergence_warn(&["bd-kept"], &["bd-lost"])]),
            &preserved
        ));

        // A warn without the id_delta details cannot be attributed.
        let bare_warn = CheckResult {
            name: "counts.db_vs_jsonl".to_string(),
            status: CheckStatus::Warn,
            message: Some("DB and JSONL counts differ".to_string()),
            details: Some(serde_json::json!({ "db": 2, "jsonl": 1 })),
        };
        assert!(!jsonl_rebuild_repair_verified(
            &report_with(vec![bare_warn]),
            &preserved
        ));

        // A clipped preview (only_db shorter than only_db_count) cannot
        // attribute every divergent row.
        let mut clipped = count_divergence_warn(&["bd-kept"], &[]);
        if let Some(details) = clipped.details.as_mut() {
            details["id_delta"]["only_db_count"] = serde_json::json!(2);
        }
        assert!(!jsonl_rebuild_repair_verified(
            &report_with(vec![clipped]),
            &preserved
        ));
    }

    #[test]
    fn fresh_recovery_backups_are_not_classified_as_stale() {
        // beads_rust-a53h: `db.recovery_artifacts` is information-only and
        // lists every preserved backup regardless of age, including the one the
        // running repair just wrote. Only the TTL-based `.aged` sibling may
        // claim artifacts are stale.
        let mut anomalies = Vec::new();
        append_doctor_check_anomalies(
            &CheckResult {
                name: "db.recovery_artifacts".to_string(),
                status: CheckStatus::Warn,
                message: Some(
                    "Preserved recovery artifacts remain for this database family (3 item(s))"
                        .to_string(),
                ),
                details: None,
            },
            &mut anomalies,
        );
        assert!(
            anomalies.is_empty(),
            "a fresh preserved backup is the success signal of a repair, not a \
             workspace anomaly: {anomalies:?}"
        );

        let mut aged_anomalies = Vec::new();
        append_doctor_check_anomalies(
            &CheckResult {
                name: "db.recovery_artifacts.aged".to_string(),
                status: CheckStatus::Warn,
                message: Some("2 recovery artifact(s) older than 30 days".to_string()),
                details: None,
            },
            &mut aged_anomalies,
        );
        assert_eq!(aged_anomalies, vec![AnomalyClass::StaleRecoveryArtifacts]);
    }

    #[test]
    fn warning_repair_verified_rejects_page_warning_introduced_by_other_repair() {
        let report = DoctorReport {
            ok: true,
            workspace_health: None,
            reliability_audit: None,
            checks: vec![
                CheckResult {
                    name: "db.recoverable_anomalies".to_string(),
                    status: CheckStatus::Ok,
                    message: None,
                    details: None,
                },
                CheckResult {
                    name: "sqlite3.integrity_check".to_string(),
                    status: CheckStatus::Warn,
                    message: Some("Page 55: never used".to_string()),
                    details: None,
                },
            ],
        };

        assert!(!warning_repair_verified(&report, true, false));
    }

    #[test]
    fn repair_report_verified_rejects_residual_warn_findings() {
        let dirty_report = DoctorReport {
            ok: true,
            workspace_health: None,
            reliability_audit: None,
            checks: vec![CheckResult {
                name: "sync.metadata".to_string(),
                status: CheckStatus::Warn,
                message: Some("Local changes pending export".to_string()),
                details: Some(serde_json::json!({
                    "finding_id": "fm-state_files-dirty-flag-divergence",
                    "dirty_issues": 1,
                    "pending_export": true,
                })),
            }],
        };

        assert!(
            !repair_report_verified(&dirty_report),
            "repair verification must not claim success while WARN findings remain"
        );

        let clean_report = DoctorReport {
            ok: true,
            workspace_health: None,
            reliability_audit: None,
            checks: vec![CheckResult {
                name: "sync.metadata".to_string(),
                status: CheckStatus::Ok,
                message: None,
                details: Some(serde_json::json!({
                    "dirty_issues": 0,
                    "pending_export": false,
                })),
            }],
        };

        assert!(repair_report_verified(&clean_report));
    }

    // ========================================================================
    // beads_rust-m3mi: audit.suspect_close_reasons check tests (added 2026-05-09)
    // ========================================================================

    fn closed_issue_with_reason(id: &str, title: &str, reason: &str) -> Issue {
        let mut issue = sample_issue(id, title);
        issue.status = Status::Closed;
        issue.closed_at = Some(Utc::now());
        issue.close_reason = Some(reason.to_string());
        issue
    }

    #[test]
    fn check_suspect_close_reasons_finds_matching_bead() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let bad = closed_issue_with_reason(
            "br-bad",
            "Bad close",
            "Implemented foo. Forced close due to cycle.",
        );
        storage.create_issue(&bad, "tester").unwrap();

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let mut checks = Vec::new();
        check_suspect_close_reasons(&conn, &mut checks);

        let check = find_check(&checks, "audit.suspect_close_reasons").expect("check present");
        assert!(
            matches!(check.status, CheckStatus::Warn),
            "expected Warn, got {:?}",
            check.status
        );
        let details = check.details.as_ref().expect("details present");
        let matches_arr = details["matches"].as_array().expect("matches array");
        assert_eq!(matches_arr.len(), 1);
        assert_eq!(matches_arr[0]["bead_id"], "br-bad");
    }

    #[test]
    fn check_suspect_close_reasons_skips_beads_with_historical_label() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let mut triaged = closed_issue_with_reason(
            "br-triaged",
            "Triaged",
            "Implemented bar. Forced close due to cycle.",
        );
        triaged.labels = vec!["audit-historical-cycle-close-2026-05-09".to_string()];
        storage.create_issue(&triaged, "tester").unwrap();

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let mut checks = Vec::new();
        check_suspect_close_reasons(&conn, &mut checks);

        let check = find_check(&checks, "audit.suspect_close_reasons").expect("check present");
        assert!(
            matches!(check.status, CheckStatus::Ok),
            "historical-label bead must NOT trigger warn; got {:?}",
            check.status
        );
    }

    #[test]
    fn check_suspect_close_reasons_rejects_malformed_historical_label() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let mut malformed = closed_issue_with_reason(
            "br-malformed",
            "Malformed label",
            "Implemented bar. Forced close due to cycle.",
        );
        malformed.labels = vec!["audit-historical-cycle-close-not-a-date".to_string()];
        storage.create_issue(&malformed, "tester").unwrap();

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let mut checks = Vec::new();
        check_suspect_close_reasons(&conn, &mut checks);

        let check = find_check(&checks, "audit.suspect_close_reasons").expect("check present");
        assert!(
            matches!(check.status, CheckStatus::Warn),
            "malformed historical-label bead must trigger warn; got {:?}",
            check.status
        );
        let details = check.details.as_ref().expect("details present");
        let matches_arr = details["matches"].as_array().expect("matches array");
        assert_eq!(matches_arr.len(), 1);
        assert_eq!(matches_arr[0]["bead_id"], "br-malformed");
    }

    #[test]
    fn check_suspect_close_reasons_does_not_honor_undocumented_allowed_label() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let mut suspect = closed_issue_with_reason(
            "br-allowed",
            "Undocumented allow label",
            "Implemented baz. Forced close due to cycle.",
        );
        suspect.labels = vec!["audit-suspect-allowed".to_string()];
        storage.create_issue(&suspect, "tester").unwrap();

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let mut checks = Vec::new();
        check_suspect_close_reasons(&conn, &mut checks);

        let check = find_check(&checks, "audit.suspect_close_reasons").expect("check present");
        assert!(
            matches!(check.status, CheckStatus::Warn),
            "undocumented allow label must trigger warn; got {:?}",
            check.status
        );
        let details = check.details.as_ref().expect("details present");
        let matches_arr = details["matches"].as_array().expect("matches array");
        assert_eq!(matches_arr.len(), 1);
        assert_eq!(matches_arr[0]["bead_id"], "br-allowed");
    }

    #[test]
    fn check_suspect_close_reasons_returns_ok_when_no_matches() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let normal = closed_issue_with_reason(
            "br-normal",
            "Normal close",
            "Verified by tests/e2e_basic_lifecycle.rs::list_basic.",
        );
        storage.create_issue(&normal, "tester").unwrap();

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let mut checks = Vec::new();
        check_suspect_close_reasons(&conn, &mut checks);

        let check = find_check(&checks, "audit.suspect_close_reasons").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    #[test]
    fn check_suspect_close_reasons_skips_default_allowlist() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let auto = closed_issue_with_reason(
            "br-auto",
            "Auto-closed",
            "auto-closed by doctor: stale recovery artifact",
        );
        storage.create_issue(&auto, "tester").unwrap();

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let mut checks = Vec::new();
        check_suspect_close_reasons(&conn, &mut checks);

        let check = find_check(&checks, "audit.suspect_close_reasons").expect("check present");
        assert!(
            matches!(check.status, CheckStatus::Ok),
            "allowlist entry must NOT trigger warn; got {:?}",
            check.status
        );
    }

    // ---------------------------------------------------------------
    // WP3 chokepoint integration tests (Task E)
    //
    // These tests cover the four invariants the WP3 spec requires once
    // the doctor's WP1 chokepoint becomes load-bearing:
    //
    //   1. `--dry-run` writes nothing to disk — neither the target file
    //      nor an `actions.jsonl` line.
    //   2. A real (non-dry-run) repair appends to `actions.jsonl` and
    //      the line parses as JSON with the WP1 schema.
    //   3. `write_undo_sh` produces an executable bash script.
    //   4. The "no delete" invariant: when the doctor would otherwise
    //      remove a file, it instead renames it into the per-run
    //      quarantine area.
    //
    // We exercise these via the gitignore fixer (the only path WP3
    // wires through `mutate()` end-to-end without touching the
    // config-crate boundary), plus a direct chokepoint call for the
    // quarantine-not-delete invariant. The other repair phases (DB
    // rebuild, sidecar quarantine, REINDEX/VACUUM) are deferred to WP4.
    // ---------------------------------------------------------------

    /// Build a minimal doctor fixture: a repo root containing `.beads/`
    /// and a root `.gitignore` containing an offending pattern that
    /// `fix_root_gitignore_if_warned` will rewrite.
    fn make_doctor_fixture(tmp: &Path) -> (PathBuf, PathBuf, DoctorReport) {
        let beads_dir = tmp.join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let gitignore = tmp.join(".gitignore");
        fs::write(&gitignore, b"keep-me\n.beads/\n").unwrap();

        // Synthesise the warning the fixer keys off of.
        let report = DoctorReport {
            ok: false,
            workspace_health: None,
            reliability_audit: None,
            checks: vec![CheckResult {
                name: "gitignore.beads_inner".to_string(),
                status: CheckStatus::Warn,
                message: Some("offending pattern".to_string()),
                details: None,
            }],
        };
        (beads_dir, gitignore, report)
    }

    fn quiet_ctx() -> OutputContext {
        OutputContext::from_output_format(crate::cli::OutputFormat::Json, false, true)
    }

    #[test]
    fn wp3_repair_dry_run_writes_no_files() {
        let tmp = TempDir::new().unwrap();
        let (beads_dir, gitignore, report) = make_doctor_fixture(tmp.path());

        // Build the session first — `create_run_dir` may append a
        // `.doctor/` line to `.gitignore` (this is an idempotent,
        // documented setup write that happens once per repair run, not
        // a fixer mutation). Snapshot AFTER session construction so we
        // measure only what the dry-run fixer attempts.
        let mut session =
            DoctorRepairSession::new(tmp.path(), /* dry_run = */ true).expect("session must build");
        let post_session_baseline = fs::read(&gitignore).unwrap();
        let actions_path = session.run.actions_file.clone();

        let result =
            fix_root_gitignore_if_warned(&beads_dir, &report, &quiet_ctx(), Some(&mut session));
        assert!(result, "fixer must report success even in dry-run");

        // The fixer must not touch the file in dry-run.
        assert_eq!(
            fs::read(&gitignore).unwrap(),
            post_session_baseline,
            "dry-run must not mutate the target file"
        );

        // No actions.jsonl line should have been appended by the fixer.
        let actions = fs::read(&actions_path).unwrap_or_default();
        assert!(
            actions.is_empty(),
            "dry-run must not append to actions.jsonl; got {} bytes",
            actions.len()
        );
    }

    #[test]
    fn wp3_repair_writes_actions_jsonl() {
        let tmp = TempDir::new().unwrap();
        let (beads_dir, _gitignore, report) = make_doctor_fixture(tmp.path());

        let mut session = DoctorRepairSession::new(tmp.path(), /* dry_run = */ false)
            .expect("session must build");
        let actions_path = session.run.actions_file.clone();
        let result =
            fix_root_gitignore_if_warned(&beads_dir, &report, &quiet_ctx(), Some(&mut session));
        assert!(result, "fixer must report success");

        let actions = fs::read_to_string(&actions_path).expect("actions.jsonl readable");
        let lines: Vec<&str> = actions.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(
            lines.len(),
            1,
            "exactly one action recorded; got {actions:?}"
        );

        let value: serde_json::Value = serde_json::from_str(lines[0]).expect("valid JSON line");
        assert_eq!(value["op"], "write_file");
        assert_eq!(value["fixer_id"], "doctor.gitignore_repair");
        assert_eq!(value["ok"], true);
        assert!(
            value["before_hash"]
                .as_str()
                .unwrap()
                .starts_with("sha256:"),
            "before_hash present"
        );
        assert!(
            value["after_hash"].as_str().unwrap().starts_with("sha256:"),
            "after_hash present"
        );
    }

    #[test]
    fn wp3_repair_skips_locked_root_gitignore() {
        let tmp = TempDir::new().unwrap();
        let (beads_dir, gitignore, mut report) = make_doctor_fixture(tmp.path());
        report.checks.push(CheckResult {
            name: "permissions.root_gitignore".to_string(),
            status: CheckStatus::Warn,
            message: Some("root .gitignore is not owner-writable".to_string()),
            details: None,
        });

        let mut session = DoctorRepairSession::new(tmp.path(), /* dry_run = */ false)
            .expect("session must build");
        fs::set_permissions(&gitignore, fs::Permissions::from_mode(0o444)).unwrap();
        let before = fs::read(&gitignore).unwrap();
        let actions_path = session.run.actions_file.clone();

        let result =
            fix_root_gitignore_if_warned(&beads_dir, &report, &quiet_ctx(), Some(&mut session));
        assert!(!result, "locked root .gitignore repair must be skipped");
        assert_eq!(
            fs::read(&gitignore).unwrap(),
            before,
            "locked root .gitignore must not be rewritten"
        );
        assert!(
            fs::read(&actions_path).unwrap_or_default().is_empty(),
            "skipped repair must not append an action"
        );

        fs::set_permissions(&gitignore, fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[test]
    fn wp3_repair_creates_undo_sh() {
        let tmp = TempDir::new().unwrap();
        let session = DoctorRepairSession::new(tmp.path(), /* dry_run = */ false)
            .expect("session must build");
        run_dir::write_undo_sh(&session.run).expect("undo.sh write");

        let undo = &session.run.undo_script;
        assert!(undo.is_file(), "undo.sh exists at {}", undo.display());
        let mode = fs::metadata(undo).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o755,
            "undo.sh must be world-executable; got {mode:o}"
        );
        let body = fs::read_to_string(undo).unwrap();
        assert!(body.starts_with("#!/usr/bin/env bash"));
    }

    #[test]
    fn wp3_quarantine_renames_not_deletes() {
        // The "no delete" invariant: when the doctor wants to remove a
        // file, it must instead `Op::Rename` it into the per-run
        // quarantine area. We exercise this via a direct chokepoint
        // call against an orphan `.wal` sidecar — the same kind of file
        // the sidecar repair targets.
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let orphan_wal = beads_dir.join("beads.db-wal");
        fs::write(&orphan_wal, b"orphan-wal-bytes").unwrap();

        let mut session = DoctorRepairSession::new(tmp.path(), /* dry_run = */ false)
            .expect("session must build");
        session.set_fixer("doctor.test.quarantine");
        let quarantine_dst = session.run.root.join("quarantine/.beads/beads.db-wal");
        let result = chokepoint::mutate(
            &session.ctx,
            &orphan_wal,
            Op::Rename {
                to: quarantine_dst.clone(),
            },
        )
        .expect("rename into quarantine must succeed");
        assert!(result.ok);

        // The orphan must be moved, not deleted.
        assert!(!orphan_wal.exists(), "source must be moved out of .beads/");
        assert!(
            quarantine_dst.exists(),
            "destination must exist at {}",
            quarantine_dst.display()
        );
        assert_eq!(fs::read(&quarantine_dst).unwrap(), b"orphan-wal-bytes");

        // The actions.jsonl line records the rename target so
        // `doctor undo` and the bash fallback can reverse it.
        let actions = fs::read_to_string(&session.run.actions_file).unwrap();
        let line = actions
            .lines()
            .find(|l| !l.is_empty())
            .expect("actions.jsonl has a line");
        let value: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(value["op"], "rename");
        assert!(value.get("rename_to").is_some(), "rename_to recorded");
    }

    #[test]
    fn check_permissions_beads_dir_reports_ok_for_temp_beads_dir() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let mut checks = Vec::new();
        check_permissions_beads_dir(&beads_dir, &mut checks);

        let check = find_check(&checks, "permissions.beads_dir").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok), "{check:?}");
    }

    #[test]
    fn check_permissions_beads_dir_defers_when_metadata_unavailable() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads-missing");

        let mut checks = Vec::new();
        check_permissions_beads_dir(&beads_dir, &mut checks);

        let check = find_check(&checks, "permissions.beads_dir").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok), "{check:?}");
        assert!(
            check
                .message
                .as_deref()
                .is_some_and(|message| message.contains("deferring to beads_dir")),
            "{check:?}"
        );
    }

    // --- check_routes_jsonl tests (pass-2 / WP5 unblock for routes_external) ---

    #[test]
    fn check_routes_jsonl_missing_is_ok() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        // No routes.jsonl planted.
        let mut checks = Vec::new();
        check_routes_jsonl(&beads_dir, &mut checks);
        let check = find_check(&checks, "routes_jsonl").expect("routes_jsonl present");
        assert!(
            matches!(check.status, CheckStatus::Ok),
            "missing routes.jsonl must be Ok (routing is optional); got {:?}",
            check.status
        );
    }

    #[test]
    fn check_routes_jsonl_well_formed_is_ok() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let routes = beads_dir.join("routes.jsonl");
        fs::write(
            &routes,
            "{\"prefix\":\"api-\",\"path\":\"../api\"}\n\
             {\"prefix\":\"ops-\",\"path\":\"/srv/projects/ops/.beads\"}\n",
        )
        .unwrap();
        let mut checks = Vec::new();
        check_routes_jsonl(&beads_dir, &mut checks);
        let check = find_check(&checks, "routes_jsonl").expect("routes_jsonl present");
        assert!(
            matches!(check.status, CheckStatus::Ok),
            "well-formed routes.jsonl must be Ok; got {:?}",
            check.status
        );
        let details = check.details.as_ref().expect("details present");
        assert_eq!(details["valid_count"], 2);
    }

    #[test]
    fn check_routes_jsonl_parse_error_warns() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let routes = beads_dir.join("routes.jsonl");
        // One good line + one malformed line.
        fs::write(
            &routes,
            "{\"prefix\":\"api-\",\"path\":\"../api\"}\n\
             {not json at all}\n",
        )
        .unwrap();
        let mut checks = Vec::new();
        check_routes_jsonl(&beads_dir, &mut checks);
        let check = find_check(&checks, "routes_jsonl").expect("routes_jsonl present");
        assert!(
            matches!(check.status, CheckStatus::Warn),
            "malformed line must trigger Warn; got {:?}",
            check.status
        );
        let details = check.details.as_ref().expect("details present");
        let bad = details["malformed_lines"].as_array().unwrap();
        assert_eq!(bad.len(), 1);
        assert_eq!(bad[0]["line"], 2);
        assert!(
            bad[0]["reason"].as_str().unwrap().contains("parse_error"),
            "reason must name parse_error"
        );
    }

    #[test]
    fn check_routes_jsonl_missing_prefix_field_warns() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let routes = beads_dir.join("routes.jsonl");
        // Missing prefix entirely.
        fs::write(&routes, "{\"path\":\"../api\"}\n").unwrap();
        let mut checks = Vec::new();
        check_routes_jsonl(&beads_dir, &mut checks);
        let check = find_check(&checks, "routes_jsonl").expect("routes_jsonl present");
        assert!(matches!(check.status, CheckStatus::Warn));
        let bad = check.details.as_ref().unwrap()["malformed_lines"]
            .as_array()
            .unwrap();
        assert!(
            bad[0]["reason"]
                .as_str()
                .unwrap()
                .contains("missing `prefix`")
        );
    }

    #[test]
    fn check_routes_jsonl_non_string_fields_warn_clearly() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let routes = beads_dir.join("routes.jsonl");
        fs::write(&routes, "{\"prefix\":42,\"path\":[\"../api\"]}\n").unwrap();
        let mut checks = Vec::new();
        check_routes_jsonl(&beads_dir, &mut checks);
        let check = find_check(&checks, "routes_jsonl").expect("routes_jsonl present");
        assert!(matches!(check.status, CheckStatus::Warn));
        let bad = check.details.as_ref().unwrap()["malformed_lines"]
            .as_array()
            .unwrap();
        let reason = bad[0]["reason"].as_str().unwrap();
        assert!(reason.contains("non-string `prefix`"), "{reason}");
        assert!(reason.contains("non-string `path`"), "{reason}");
    }

    #[test]
    fn check_routes_jsonl_empty_path_warns() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let routes = beads_dir.join("routes.jsonl");
        fs::write(&routes, "{\"prefix\":\"api-\",\"path\":\"\"}\n").unwrap();
        let mut checks = Vec::new();
        check_routes_jsonl(&beads_dir, &mut checks);
        let check = find_check(&checks, "routes_jsonl").expect("routes_jsonl present");
        assert!(matches!(check.status, CheckStatus::Warn));
        let bad = check.details.as_ref().unwrap()["malformed_lines"]
            .as_array()
            .unwrap();
        assert!(bad[0]["reason"].as_str().unwrap().contains("empty `path`"));
    }

    #[test]
    fn check_routes_jsonl_skips_blank_lines() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let routes = beads_dir.join("routes.jsonl");
        fs::write(
            &routes,
            "\n\
             {\"prefix\":\"api-\",\"path\":\"../api\"}\n\
             \n\
             {\"prefix\":\"ops-\",\"path\":\"../ops\"}\n\
             \n",
        )
        .unwrap();
        let mut checks = Vec::new();
        check_routes_jsonl(&beads_dir, &mut checks);
        let check = find_check(&checks, "routes_jsonl").expect("routes_jsonl present");
        assert!(matches!(check.status, CheckStatus::Ok));
        let details = check.details.as_ref().expect("details present");
        assert_eq!(
            details["valid_count"], 2,
            "blank lines must not be counted as routes"
        );
    }

    #[test]
    fn check_routes_jsonl_skips_comment_lines() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let routes = beads_dir.join("routes.jsonl");
        fs::write(
            &routes,
            "# local routes\n\
             {\"prefix\":\"api-\",\"path\":\"../api\"}\n\
             \n\
             # town-level route intentionally omitted in this fixture\n",
        )
        .unwrap();
        let mut checks = Vec::new();
        check_routes_jsonl(&beads_dir, &mut checks);
        let check = find_check(&checks, "routes_jsonl").expect("routes_jsonl present");
        assert!(matches!(check.status, CheckStatus::Ok));
        let details = check.details.as_ref().expect("details present");
        assert_eq!(
            details["valid_count"], 1,
            "comment lines must not be counted as routes"
        );
    }

    #[test]
    fn check_routes_targets_resolve_missing_routes_file_is_ok() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let mut checks = Vec::new();
        check_routes_targets_resolve(&beads_dir, &mut checks);

        let check = find_check(&checks, "routes.targets").expect("routes.targets present");
        assert!(matches!(check.status, CheckStatus::Ok), "{check:?}");
    }

    #[test]
    fn check_routes_targets_resolve_project_root_route_is_ok() {
        let tmp = TempDir::new().unwrap();
        let project_root = tmp.path();
        let beads_dir = project_root.join(".beads");
        let api_beads = project_root.join("api").join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&api_beads).unwrap();
        fs::write(
            beads_dir.join("routes.jsonl"),
            "{\"prefix\":\"api-\",\"path\":\"api\"}\n",
        )
        .unwrap();

        let mut checks = Vec::new();
        check_routes_targets_resolve(&beads_dir, &mut checks);

        let check = find_check(&checks, "routes.targets").expect("routes.targets present");
        assert!(matches!(check.status, CheckStatus::Ok), "{check:?}");
        let details = check.details.as_ref().expect("details present");
        assert_eq!(details["route_count"], 1);
        assert_eq!(details["resolved_count"], 1);
    }

    #[test]
    fn check_routes_targets_resolve_missing_target_warns() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::write(
            beads_dir.join("routes.jsonl"),
            "{\"prefix\":\"api-\",\"path\":\"missing\"}\n",
        )
        .unwrap();

        let mut checks = Vec::new();
        check_routes_targets_resolve(&beads_dir, &mut checks);

        let check = find_check(&checks, "routes.targets").expect("routes.targets present");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
        let details = check.details.as_ref().expect("details present");
        let unresolved = details["unresolved_routes"].as_array().unwrap();
        assert_eq!(unresolved.len(), 1);
        assert!(
            unresolved[0]["reason"]
                .as_str()
                .unwrap()
                .contains("Redirect target not found"),
            "{unresolved:?}"
        );
    }

    #[test]
    fn check_routes_targets_resolve_town_root_routes() {
        let tmp = TempDir::new().unwrap();
        let town_root = tmp.path().join("town");
        let project_root = town_root.join("projects").join("client");
        let beads_dir = project_root.join(".beads");
        let town_beads_dir = town_root.join(".beads");
        fs::create_dir_all(town_root.join("mayor")).unwrap();
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&town_beads_dir).unwrap();
        fs::write(town_root.join("mayor").join("town.json"), "{}").unwrap();
        fs::write(
            town_beads_dir.join("routes.jsonl"),
            "{\"prefix\":\"ghost-\",\"path\":\"missing-project\"}\n",
        )
        .unwrap();

        let mut checks = Vec::new();
        check_routes_targets_resolve(&beads_dir, &mut checks);

        let check = find_check(&checks, "routes.targets").expect("routes.targets present");
        assert!(matches!(check.status, CheckStatus::Warn), "{check:?}");
        let details = check.details.as_ref().expect("details present");
        assert_eq!(details["route_count"], 1);
        let unresolved = details["unresolved_routes"].as_array().unwrap();
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0]["prefix"], "ghost-");
        assert_eq!(
            unresolved[0]["target"],
            town_root
                .join("missing-project/.beads")
                .display()
                .to_string()
        );
        assert_eq!(
            unresolved[0]["route_file"],
            town_beads_dir.join("routes.jsonl").display().to_string()
        );
    }

    // --- rust_log_volume / check_rust_log_noisy tests (pass-2 / WP5) ---
    //
    // We test the pure classifier `rust_log_volume(Option<&str>)`
    // directly. The wrapper `check_rust_log_noisy(checks)` reads
    // `std::env::var("RUST_LOG")` which makes it sensitive to the
    // ambient test-runner environment — exercising it through the
    // classifier keeps the tests deterministic without needing
    // `#![feature(restricted_std)]` or unsafe `std::env::set_var`.

    #[test]
    fn rust_log_volume_classifies_unset_like_compiled_default() {
        assert_eq!(rust_log_volume(None), rust_log_default_volume());
    }

    #[test]
    fn rust_log_volume_classifies_blank_as_quiet() {
        assert_eq!(rust_log_volume(Some("")), RustLogVolume::Quiet);
        assert_eq!(rust_log_volume(Some("   ")), RustLogVolume::Quiet);
    }

    #[test]
    fn rust_log_volume_classifies_quiet_levels_as_quiet() {
        for level in &["off", "error", "warn", "OFF", "Error", "WARN"] {
            assert_eq!(
                rust_log_volume(Some(level)),
                RustLogVolume::Quiet,
                "level {level} should be quiet"
            );
        }
    }

    #[test]
    fn rust_log_volume_classifies_bare_info_as_noisy() {
        let v = rust_log_volume(Some("info"));
        match v {
            RustLogVolume::Noisy { reason } => assert_eq!(reason, "bare_level_info"),
            RustLogVolume::Quiet => panic!("expected Noisy, got {v:?}"),
        }
    }

    #[test]
    fn rust_log_volume_classifies_bare_debug_as_noisy() {
        let v = rust_log_volume(Some("debug"));
        match v {
            RustLogVolume::Noisy { reason } => assert_eq!(reason, "bare_level_debug"),
            RustLogVolume::Quiet => panic!("expected Noisy, got {v:?}"),
        }
    }

    #[test]
    fn rust_log_volume_classifies_bare_trace_as_noisy() {
        let v = rust_log_volume(Some("trace"));
        match v {
            RustLogVolume::Noisy { reason } => assert_eq!(reason, "bare_level_trace"),
            RustLogVolume::Quiet => panic!("expected Noisy, got {v:?}"),
        }
    }

    #[test]
    fn rust_log_volume_classifies_per_module_directive_with_info_as_noisy() {
        // Per-module directives that ask for info/debug/trace must
        // still trip the warn — that's the real-world footgun (a
        // developer leaves `beads_rust=info` set and forgets).
        let v = rust_log_volume(Some("beads_rust=info"));
        match v {
            RustLogVolume::Noisy { reason } => assert_eq!(reason, "directive_info"),
            RustLogVolume::Quiet => panic!("expected Noisy, got {v:?}"),
        }
    }

    #[test]
    fn rust_log_volume_classifies_target_only_directive_as_noisy() {
        let v = rust_log_volume(Some("beads_rust"));
        match v {
            RustLogVolume::Noisy { reason } => assert_eq!(reason, "directive_target_only"),
            RustLogVolume::Quiet => panic!("expected Noisy, got {v:?}"),
        }
    }

    #[test]
    fn rust_log_volume_classifies_composite_with_one_noisy_directive_as_noisy() {
        // First directive is quiet, second is noisy → overall noisy.
        let v = rust_log_volume(Some("warn,beads_rust=debug"));
        match v {
            RustLogVolume::Noisy { reason } => assert_eq!(reason, "directive_debug"),
            RustLogVolume::Quiet => panic!("expected Noisy, got {v:?}"),
        }
    }

    #[test]
    fn rust_log_volume_classifies_composite_with_target_only_directive_as_noisy() {
        let v = rust_log_volume(Some("error,beads_rust"));
        match v {
            RustLogVolume::Noisy { reason } => assert_eq!(reason, "directive_target_only"),
            RustLogVolume::Quiet => panic!("expected Noisy, got {v:?}"),
        }
    }

    #[test]
    fn rust_log_volume_classifies_span_filter_without_level_as_noisy() {
        let v = rust_log_volume(Some("[span{field=value}]"));
        match v {
            RustLogVolume::Noisy { reason } => assert_eq!(reason, "directive_unclassified"),
            RustLogVolume::Quiet => panic!("expected Noisy, got {v:?}"),
        }
    }

    #[test]
    fn rust_log_volume_classifies_composite_all_quiet_as_quiet() {
        let v = rust_log_volume(Some("error,fsqlite=warn,beads_rust=off"));
        assert_eq!(v, RustLogVolume::Quiet);
    }

    // --- check_permissions_beads_dir tests (pass-2 / WP5) ---

    #[test]
    fn check_permissions_beads_dir_normal_workspace_is_ok() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::write(beads_dir.join("issues.jsonl"), b"").unwrap();
        fs::write(beads_dir.join("beads.db"), b"").unwrap();
        let mode = fs::metadata(&beads_dir).unwrap().permissions().mode() & 0o777;
        assert!(mode & 0o200 != 0, "test setup: .beads must start writable");

        let mut checks = Vec::new();
        check_permissions_beads_dir(&beads_dir, &mut checks);
        let check =
            find_check(&checks, "permissions.beads_dir").expect("permissions.beads_dir present");
        assert!(
            matches!(check.status, CheckStatus::Ok),
            "writable workspace must be Ok; got {:?}",
            check.status
        );
    }

    #[test]
    fn check_permissions_beads_dir_readonly_directory_warns() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let mut perms = fs::metadata(&beads_dir).unwrap().permissions();
        perms.set_mode(0o555);
        fs::set_permissions(&beads_dir, perms).unwrap();

        let mut checks = Vec::new();
        check_permissions_beads_dir(&beads_dir, &mut checks);
        // Restore writability so TempDir can clean up.
        let mut restore = fs::metadata(&beads_dir).unwrap().permissions();
        restore.set_mode(0o755);
        fs::set_permissions(&beads_dir, restore).ok();

        let check =
            find_check(&checks, "permissions.beads_dir").expect("permissions.beads_dir present");
        assert!(
            matches!(check.status, CheckStatus::Warn),
            "readonly .beads/ must trigger Warn; got {:?}",
            check.status
        );
        let details = check.details.as_ref().expect("details present");
        let readonly = details["readonly_paths"].as_array().unwrap();
        assert!(
            !readonly.is_empty(),
            "readonly_paths must enumerate the dir"
        );
        assert_eq!(readonly[0]["kind"], "directory");
        assert!(
            readonly[0]["fix"]
                .as_str()
                .unwrap()
                .starts_with("chmod u+w "),
            "fix must name the canonical chmod command: {readonly:?}"
        );
    }

    #[test]
    fn check_permissions_beads_dir_readonly_issues_jsonl_warns() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let jsonl = beads_dir.join("issues.jsonl");
        fs::write(&jsonl, b"").unwrap();
        let mut perms = fs::metadata(&jsonl).unwrap().permissions();
        perms.set_mode(0o444);
        fs::set_permissions(&jsonl, perms).unwrap();

        let mut checks = Vec::new();
        check_permissions_beads_dir(&beads_dir, &mut checks);
        // Restore for cleanup.
        let mut restore = fs::metadata(&jsonl).unwrap().permissions();
        restore.set_mode(0o644);
        fs::set_permissions(&jsonl, restore).ok();

        let check =
            find_check(&checks, "permissions.beads_dir").expect("permissions.beads_dir present");
        assert!(matches!(check.status, CheckStatus::Warn));
        let details = check.details.as_ref().expect("details present");
        let readonly = details["readonly_paths"].as_array().unwrap();
        assert!(
            readonly
                .iter()
                .filter_map(|e| e["kind"].as_str())
                .any(|kind| kind == "file"),
            "readonly_paths must flag the file kind: {readonly:?}"
        );
    }

    #[test]
    fn check_permissions_beads_dir_missing_beads_dir_does_not_panic() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join("does_not_exist");
        let mut checks = Vec::new();
        check_permissions_beads_dir(&beads_dir, &mut checks);
        let check =
            find_check(&checks, "permissions.beads_dir").expect("permissions.beads_dir present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    // --- check_config_yaml tests (pass-2 / WP5) ---

    #[test]
    fn check_config_yaml_missing_is_ok() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let mut checks = Vec::new();
        check_config_yaml(&beads_dir, &mut checks);
        let check = find_check(&checks, "config.yaml").expect("config.yaml present");
        assert!(
            matches!(check.status, CheckStatus::Ok),
            "missing config.yaml must be Ok (project config is optional); got {:?}",
            check.status
        );
    }

    #[test]
    fn check_config_yaml_empty_is_ok() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::write(beads_dir.join("config.yaml"), b"").unwrap();
        let mut checks = Vec::new();
        check_config_yaml(&beads_dir, &mut checks);
        let check = find_check(&checks, "config.yaml").expect("config.yaml present");
        assert!(
            matches!(check.status, CheckStatus::Ok),
            "empty config.yaml means all-defaults; must be Ok"
        );
    }

    #[test]
    fn check_config_yaml_valid_is_ok() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let yaml = "id:\n  prefix: \"proj\"\ndefaults:\n  priority: 2\n  type: task\n";
        fs::write(beads_dir.join("config.yaml"), yaml).unwrap();
        let mut checks = Vec::new();
        check_config_yaml(&beads_dir, &mut checks);
        let check = find_check(&checks, "config.yaml").expect("config.yaml present");
        assert!(
            matches!(check.status, CheckStatus::Ok),
            "valid config.yaml must be Ok; got {:?}",
            check.status
        );
        let details = check.details.as_ref().expect("details present");
        assert!(details["bytes"].as_u64().unwrap() > 0);
    }

    #[test]
    fn check_config_yaml_malformed_warns_with_parse_error() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        // Mis-indented mapping: a YAML parser surfaces a clear error.
        let yaml = "id:\n  prefix: \"proj\"\n  - bad\n  invalid_block_mapping\n";
        fs::write(beads_dir.join("config.yaml"), yaml).unwrap();
        let mut checks = Vec::new();
        check_config_yaml(&beads_dir, &mut checks);
        let check = find_check(&checks, "config.yaml").expect("config.yaml present");
        assert!(
            matches!(check.status, CheckStatus::Warn),
            "malformed YAML must trigger Warn; got {:?}",
            check.status
        );
        let details = check.details.as_ref().expect("details present");
        assert!(
            details["parse_error"].as_str().is_some(),
            "parse_error must be populated"
        );
        assert!(
            details["recommended_fix"]
                .as_str()
                .unwrap()
                .contains("Open"),
            "recommended_fix should advise the operator to open the file"
        );
    }

    #[test]
    fn check_config_yaml_completely_invalid_warns() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        // Mismatched quotes / control characters in a flow-style value.
        let yaml = "[broken: yaml,\n  with: unterminated: \"quote\n  another: line\n";
        fs::write(beads_dir.join("config.yaml"), yaml).unwrap();
        let mut checks = Vec::new();
        check_config_yaml(&beads_dir, &mut checks);
        let check = find_check(&checks, "config.yaml").expect("config.yaml present");
        assert!(
            matches!(check.status, CheckStatus::Warn),
            "syntax-error YAML must trigger Warn; got {:?}",
            check.status
        );
    }

    // --- check_metadata_json tests (pass-2 / WP5) ---

    #[test]
    fn check_metadata_json_missing_is_ok() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let mut checks = Vec::new();
        check_metadata_json(&beads_dir, &mut checks);
        let check = find_check(&checks, "metadata.json").expect("metadata.json present");
        assert!(
            matches!(check.status, CheckStatus::Ok),
            "missing metadata.json must be Ok (br uses defaults); got {:?}",
            check.status
        );
    }

    #[test]
    fn check_metadata_json_empty_warns() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::write(beads_dir.join("metadata.json"), b"").unwrap();
        let mut checks = Vec::new();
        check_metadata_json(&beads_dir, &mut checks);
        let check = find_check(&checks, "metadata.json").expect("metadata.json present");
        assert!(
            matches!(check.status, CheckStatus::Warn),
            "empty metadata.json must be Warn (br loader rejects); got {:?}",
            check.status
        );
        let details = check.details.as_ref().expect("details present");
        assert_eq!(details["reason"], "empty_file");
    }

    #[test]
    fn check_metadata_json_valid_with_existing_targets_is_ok() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");
        fs::write(&db_path, b"").unwrap();
        fs::write(beads_dir.join("issues.jsonl"), b"").unwrap();
        let metadata = serde_json::json!({
            "database": db_path.display().to_string(),
            "jsonl_export": "issues.jsonl",
        });
        fs::write(beads_dir.join("metadata.json"), metadata.to_string()).unwrap();
        let mut checks = Vec::new();
        check_metadata_json(&beads_dir, &mut checks);
        let check = find_check(&checks, "metadata.json").expect("metadata.json present");
        assert!(
            matches!(check.status, CheckStatus::Ok),
            "valid metadata.json with present targets must be Ok; got {:?}",
            check.status
        );
    }

    #[test]
    fn check_metadata_json_whitespace_targets_use_defaults() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::write(
            beads_dir.join("metadata.json"),
            br#"{"database":"  ","jsonl_export":"\t"}"#,
        )
        .unwrap();
        let mut checks = Vec::new();
        check_metadata_json(&beads_dir, &mut checks);
        let check = find_check(&checks, "metadata.json").expect("metadata.json present");
        assert!(
            matches!(check.status, CheckStatus::Ok),
            "whitespace metadata targets load as defaults and must not drift-warn; got {:?}",
            check.status
        );
    }

    #[test]
    fn check_metadata_json_preserves_nonempty_target_whitespace() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::write(beads_dir.join("beads.db"), b"").unwrap();
        fs::write(beads_dir.join("issues.jsonl"), b"").unwrap();
        fs::write(
            beads_dir.join("metadata.json"),
            br#"{"database":" beads.db ","jsonl_export":" issues.jsonl "}"#,
        )
        .unwrap();
        let mut checks = Vec::new();
        check_metadata_json(&beads_dir, &mut checks);
        let check = find_check(&checks, "metadata.json").expect("metadata.json present");
        assert!(
            matches!(check.status, CheckStatus::Warn),
            "non-empty whitespace is part of the runtime path and must drift-warn; got {:?}",
            check.status
        );
        let details = check.details.as_ref().expect("details present");
        let drift = details["drift"].as_array().expect("drift array");
        assert_eq!(
            drift.len(),
            2,
            "both whitespace-padded targets should drift"
        );
        assert!(
            drift
                .iter()
                .any(|entry| { entry["field"] == "database" && entry["value"] == " beads.db " })
        );
        assert!(drift.iter().any(|entry| {
            entry["field"] == "jsonl_export" && entry["value"] == " issues.jsonl "
        }));
    }

    #[test]
    fn check_metadata_json_malformed_warns_with_parse_error() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::write(
            beads_dir.join("metadata.json"),
            br#"{ "database": "beads.db", not_quoted_key: "issues.jsonl" }"#,
        )
        .unwrap();
        let mut checks = Vec::new();
        check_metadata_json(&beads_dir, &mut checks);
        let check = find_check(&checks, "metadata.json").expect("metadata.json present");
        assert!(matches!(check.status, CheckStatus::Warn));
        let details = check.details.as_ref().expect("details present");
        assert_eq!(details["reason"], "parse_error");
        assert!(
            details["parse_error"].as_str().is_some(),
            "parse_error must be populated"
        );
    }

    #[test]
    fn check_metadata_json_non_object_top_level_warns() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::write(
            beads_dir.join("metadata.json"),
            br#"["not", "an", "object"]"#,
        )
        .unwrap();
        let mut checks = Vec::new();
        check_metadata_json(&beads_dir, &mut checks);
        let check = find_check(&checks, "metadata.json").expect("metadata.json present");
        assert!(matches!(check.status, CheckStatus::Warn));
        let details = check.details.as_ref().expect("details present");
        assert_eq!(details["reason"], "wrong_top_level_shape");
    }

    #[test]
    fn check_metadata_json_drift_warns() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        // Declare files that don't exist.
        let metadata = serde_json::json!({
            "database": tmp.path().join("renamed.db").display().to_string(),
            "jsonl_export": tmp.path().join("renamed.jsonl").display().to_string(),
        });
        fs::write(beads_dir.join("metadata.json"), metadata.to_string()).unwrap();
        let mut checks = Vec::new();
        check_metadata_json(&beads_dir, &mut checks);
        let check = find_check(&checks, "metadata.json").expect("metadata.json present");
        assert!(matches!(check.status, CheckStatus::Warn));
        let details = check.details.as_ref().expect("details present");
        let drift = details["drift"].as_array().expect("drift array");
        assert_eq!(drift.len(), 2, "both declared targets should be flagged");
        let fields: Vec<&str> = drift.iter().filter_map(|e| e["field"].as_str()).collect();
        assert!(fields.contains(&"database"));
        assert!(fields.contains(&"jsonl_export"));
    }

    #[test]
    fn check_metadata_json_partial_drift_warns_only_for_missing() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        // beads.db exists; jsonl_export points at a missing file.
        let db_path = beads_dir.join("beads.db");
        fs::write(&db_path, b"").unwrap();
        let metadata = serde_json::json!({
            "database": db_path.display().to_string(),
            "jsonl_export": "missing.jsonl",
        });
        fs::write(beads_dir.join("metadata.json"), metadata.to_string()).unwrap();
        let mut checks = Vec::new();
        check_metadata_json(&beads_dir, &mut checks);
        let check = find_check(&checks, "metadata.json").expect("metadata.json present");
        assert!(matches!(check.status, CheckStatus::Warn));
        let details = check.details.as_ref().expect("details present");
        let drift = details["drift"].as_array().expect("drift array");
        assert_eq!(drift.len(), 1, "only the missing field should be flagged");
        assert_eq!(drift[0]["field"], "jsonl_export");
    }

    // --- check_binary_version_mismatch + helpers tests (pass-2 / WP5) ---

    #[test]
    fn parse_cargo_toml_package_field_finds_version() {
        let body = r#"
[package]
name = "beads_rust"
version = "0.2.6"
edition = "2024"
"#;
        assert_eq!(
            parse_cargo_toml_package_field(body, "version").as_deref(),
            Some("0.2.6")
        );
        assert_eq!(
            parse_cargo_toml_package_field(body, "name").as_deref(),
            Some("beads_rust")
        );
    }

    #[test]
    fn parse_cargo_toml_package_field_ignores_other_sections() {
        let body = r#"
[package]
name = "beads_rust"
version = "0.2.6"

[dependencies]
version = "1.0.0"
name = "other"
"#;
        // Must return the [package].version, not [dependencies].version.
        assert_eq!(
            parse_cargo_toml_package_field(body, "version").as_deref(),
            Some("0.2.6")
        );
        assert_eq!(
            parse_cargo_toml_package_field(body, "name").as_deref(),
            Some("beads_rust")
        );
    }

    #[test]
    fn parse_cargo_toml_package_field_handles_inline_comments() {
        let body = r#"
[package]
name = "beads_rust"
version = "0.2.6"  # release candidate
"#;
        assert_eq!(
            parse_cargo_toml_package_field(body, "version").as_deref(),
            Some("0.2.6")
        );
    }

    #[test]
    fn parse_cargo_toml_package_field_returns_none_for_missing() {
        let body = r#"
[package]
name = "beads_rust"
"#;
        assert!(parse_cargo_toml_package_field(body, "version").is_none());
    }

    #[test]
    fn find_beads_rust_repo_root_finds_self() {
        // The .beads_dir we pass is /tmp/<dir>/.beads where
        // <dir>/Cargo.toml has name = "beads_rust".
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let cargo = tmp.path().join("Cargo.toml");
        fs::write(
            &cargo,
            r#"[package]
name = "beads_rust"
version = "0.99.0"
edition = "2024"
"#,
        )
        .unwrap();
        let root = find_beads_rust_repo_root(&beads_dir);
        assert_eq!(root.as_deref(), Some(tmp.path()));
    }

    #[test]
    fn find_beads_rust_repo_root_returns_none_for_other_repo() {
        let tmp = TempDir::new().unwrap();
        // Model an unrelated package nested beneath a beads_rust checkout.
        // This is also what happens when TMPDIR points inside the checkout:
        // the nearest package manifest must stop the search before the outer
        // beads_rust manifest can produce a false-positive match.
        fs::write(
            tmp.path().join("Cargo.toml"),
            r#"[package]
name = "beads_rust"
version = "0.99.0"
"#,
        )
        .unwrap();
        let other_repo = tmp.path().join("other_repo");
        let beads_dir = other_repo.join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let cargo = other_repo.join("Cargo.toml");
        // Wrong [package].name.
        fs::write(
            &cargo,
            r#"[package]
name = "some_other_crate"
version = "1.0.0"
"#,
        )
        .unwrap();
        let root = find_beads_rust_repo_root(&beads_dir);
        assert!(root.is_none(), "must NOT match unrelated Cargo.toml");
    }

    #[test]
    fn check_binary_version_mismatch_not_in_repo_is_ok() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        // No Cargo.toml anywhere upward — common when br runs outside
        // its own source tree.
        let mut checks = Vec::new();
        check_binary_version_mismatch(&beads_dir, &mut checks);
        let check = find_check(&checks, "binary_version").expect("binary_version present");
        assert!(
            matches!(check.status, CheckStatus::Ok),
            "no beads_rust Cargo.toml reachable should be Ok; got {:?}",
            check.status
        );
    }

    #[test]
    fn check_binary_version_mismatch_tree_ahead_warns() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        // Plant a Cargo.toml whose version is FAR ahead of any binary
        // version we'd ship (99.99.99 > 0.x.y).
        fs::write(
            tmp.path().join("Cargo.toml"),
            r#"[package]
name = "beads_rust"
version = "99.99.99"
edition = "2024"
"#,
        )
        .unwrap();
        let mut checks = Vec::new();
        check_binary_version_mismatch(&beads_dir, &mut checks);
        let check = find_check(&checks, "binary_version").expect("binary_version present");
        assert!(
            matches!(check.status, CheckStatus::Warn),
            "tree-ahead-of-binary must trigger Warn; got {:?}",
            check.status
        );
        let details = check.details.as_ref().expect("details present");
        assert_eq!(details["tree_version"], "99.99.99");
        assert!(
            details["recommended_fix"]
                .as_str()
                .unwrap()
                .contains("cargo install --path"),
            "recommended_fix should name the canonical rebuild command"
        );
    }

    #[test]
    fn check_binary_version_mismatch_tree_behind_is_ok() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        // Plant a Cargo.toml with version "0.0.1" — guaranteed BEHIND
        // any released binary. Operator may be working on a side branch.
        fs::write(
            tmp.path().join("Cargo.toml"),
            r#"[package]
name = "beads_rust"
version = "0.0.1"
edition = "2024"
"#,
        )
        .unwrap();
        let mut checks = Vec::new();
        check_binary_version_mismatch(&beads_dir, &mut checks);
        let check = find_check(&checks, "binary_version").expect("binary_version present");
        assert!(
            matches!(check.status, CheckStatus::Ok),
            "tree-behind-binary must be silent (operator on side branch); got {:?}",
            check.status
        );
    }

    #[test]
    fn check_binary_version_mismatch_non_semver_tree_is_ok() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        // Non-semver tree version — silent (operator may be using a
        // CalVer or git-sha tag).
        fs::write(
            tmp.path().join("Cargo.toml"),
            r#"[package]
name = "beads_rust"
version = "2026-05-11-abc123"
"#,
        )
        .unwrap();
        let mut checks = Vec::new();
        check_binary_version_mismatch(&beads_dir, &mut checks);
        let check = find_check(&checks, "binary_version").expect("binary_version present");
        assert!(matches!(check.status, CheckStatus::Ok));
    }

    // --- check_orphaned_write_lock tests (pass-2 / WP5) ---

    #[test]
    fn check_orphaned_write_lock_missing_is_ok() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        // No .write.lock planted.
        let mut checks = Vec::new();
        check_orphaned_write_lock(&beads_dir, &mut checks);
        let check = find_check(&checks, "write_lock").expect("write_lock present");
        assert!(
            matches!(check.status, CheckStatus::Ok),
            "missing .write.lock must be Ok (no contention); got {:?}",
            check.status
        );
    }

    #[test]
    fn check_orphaned_write_lock_fresh_is_ok() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::write(beads_dir.join(".write.lock"), b"").unwrap();
        // Fresh file: mtime is now → well within the 300s default
        // threshold.
        let mut checks = Vec::new();
        check_orphaned_write_lock(&beads_dir, &mut checks);
        let check = find_check(&checks, "write_lock").expect("write_lock present");
        assert!(
            matches!(check.status, CheckStatus::Ok),
            "fresh .write.lock must be Ok; got {:?}",
            check.status
        );
    }

    /// beads_rust-5sej: a symlinked lock node is the defect itself (startup
    /// `OpenOptions` follows it), so doctor fails closed with a typed
    /// diagnostic instead of deferring to a detector that does not exist.
    #[test]
    fn check_orphaned_write_lock_symlink_fails_closed() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let target = beads_dir.join("target_file");
        fs::write(&target, b"").unwrap();
        std::os::unix::fs::symlink(&target, beads_dir.join(".write.lock")).unwrap();
        let mut checks = Vec::new();
        check_orphaned_write_lock(&beads_dir, &mut checks);
        let check = find_check(&checks, "write_lock").expect("write_lock present");
        assert!(
            matches!(check.status, CheckStatus::Error),
            "symlink .write.lock must fail closed; got {:?}",
            check.status
        );
        let details = check.details.as_ref().expect("details");
        assert_eq!(
            details.get("reason").and_then(|v| v.as_str()),
            Some("non_regular_lock_node")
        );
        assert_eq!(
            details.get("node_kind").and_then(|v| v.as_str()),
            Some("symlink")
        );
        // Fail-closed classification must not traverse or disturb either
        // the symlink or its target.
        let lock_meta = fs::symlink_metadata(beads_dir.join(".write.lock")).unwrap();
        assert!(lock_meta.file_type().is_symlink());
        assert!(target.exists());
    }

    /// beads_rust-5sej: a directory in the lock slot breaks acquisition
    /// outright and must fail closed with the same typed diagnostic.
    #[test]
    fn check_orphaned_write_lock_directory_fails_closed() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(beads_dir.join(".write.lock")).unwrap();
        let mut checks = Vec::new();
        check_orphaned_write_lock(&beads_dir, &mut checks);
        let check = find_check(&checks, "write_lock").expect("write_lock present");
        assert!(matches!(check.status, CheckStatus::Error), "{check:?}");
        let details = check.details.as_ref().expect("details");
        assert_eq!(
            details.get("node_kind").and_then(|v| v.as_str()),
            Some("directory")
        );
        // Doctor never removes or renames the node.
        assert!(beads_dir.join(".write.lock").is_dir());
    }

    /// beads_rust-5sej: the permissions probe defers to the write_lock
    /// check for non-regular nodes instead of silently reporting Ok with
    /// no message.
    #[test]
    fn check_write_lock_writable_defers_on_symlink_with_message() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let target = beads_dir.join("target_file");
        fs::write(&target, b"").unwrap();
        std::os::unix::fs::symlink(&target, beads_dir.join(".write.lock")).unwrap();
        let mut checks = Vec::new();
        check_write_lock_writable(&beads_dir, &mut checks);
        let check = find_check(&checks, "permissions.write_lock").expect("check present");
        assert!(matches!(check.status, CheckStatus::Ok));
        assert!(
            check
                .message
                .as_deref()
                .is_some_and(|m| m.contains("not a regular file")),
            "{check:?}"
        );
    }

    /// GitHub #395: flock acquisition never updates mtime, so an old but
    /// FREE lock file must classify Ok via the non-blocking probe, not
    /// warn forever on the mtime heuristic.
    #[test]
    fn check_orphaned_write_lock_old_but_free_probes_ok() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let lock_path = beads_dir.join(".write.lock");
        fs::write(&lock_path, b"").unwrap();
        // Back-date far past any threshold.
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3_600_000);
        let file = fs::OpenOptions::new().write(true).open(&lock_path).unwrap();
        file.set_times(fs::FileTimes::new().set_modified(old))
            .unwrap();
        drop(file);

        let mut checks = Vec::new();
        check_orphaned_write_lock(&beads_dir, &mut checks);
        let check = find_check(&checks, "write_lock").expect("write_lock present");
        assert!(
            matches!(check.status, CheckStatus::Ok),
            "an ancient but free lock must be Ok; got {:?} ({:?})",
            check.status,
            check.message
        );
        assert_eq!(
            check
                .details
                .as_ref()
                .and_then(|d| d.get("reason"))
                .and_then(|r| r.as_str()),
            Some("probe_acquired_free")
        );
    }

    /// GitHub #395: a lock held by a live process reports Ok (busy
    /// workspace), never a stale-orphan warn.
    #[test]
    #[allow(clippy::incompatible_msrv)]
    fn check_orphaned_write_lock_live_holder_probes_ok() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let lock_path = beads_dir.join(".write.lock");
        fs::write(&lock_path, b"").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(3_600_000);
        let holder = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        holder
            .set_times(fs::FileTimes::new().set_modified(old))
            .unwrap();
        holder.lock().unwrap();

        let mut checks = Vec::new();
        check_orphaned_write_lock(&beads_dir, &mut checks);
        drop(holder);
        let check = find_check(&checks, "write_lock").expect("write_lock present");
        assert!(
            matches!(check.status, CheckStatus::Ok),
            "a live holder must be Ok; got {:?} ({:?})",
            check.status,
            check.message
        );
        assert_eq!(
            check
                .details
                .as_ref()
                .and_then(|d| d.get("reason"))
                .and_then(|r| r.as_str()),
            Some("persistent_advisory_inode")
        );
    }

    // -----------------------------------------------------------------
    // id-set divergence detection (beads_rust#286)
    // -----------------------------------------------------------------

    /// Helper for the id-delta tests: insert a row into the `issues`
    /// table using only the columns the cardinality query reads (the
    /// schema has many NOT NULL columns we don't care about for this
    /// test, but the default values cover them).
    fn insert_minimal_issue(storage: &mut SqliteStorage, id: &str) {
        let issue = sample_issue(id, "test");
        storage.create_issue(&issue, "test").unwrap();
    }

    /// Helper: write an `issues.jsonl` containing exactly the supplied
    /// id list, each as a minimal valid record.
    fn write_jsonl_with_ids(path: &Path, ids: &[&str]) {
        let mut buf = String::new();
        for id in ids {
            buf.push_str(&format!(
                "{{\"id\":\"{}\",\"title\":\"t\",\"status\":\"open\",\"priority\":2,\"issue_type\":\"task\",\"created_at\":\"2026-05-01T00:00:00Z\",\"updated_at\":\"2026-05-01T00:00:00Z\"}}\n",
                id
            ));
        }
        fs::write(path, buf).unwrap();
    }

    #[test]
    fn compute_db_jsonl_id_delta_detects_only_db_only_jsonl_and_both() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        insert_minimal_issue(&mut storage, "bd-1");
        insert_minimal_issue(&mut storage, "bd-2");
        insert_minimal_issue(&mut storage, "bd-only-db");
        drop(storage);

        let jsonl_path = tmp.path().join("issues.jsonl");
        write_jsonl_with_ids(&jsonl_path, &["bd-1", "bd-2", "bd-only-jsonl"]);

        let conn =
            Connection::open(db_path.to_string_lossy().into_owned()).expect("open db for read");
        let delta = compute_db_jsonl_id_delta(&conn, &jsonl_path).expect("id delta should succeed");

        // The intersection contains bd-1 and bd-2.
        assert_eq!(delta.both_count, 2);
        assert_eq!(delta.only_db, vec!["bd-only-db".to_string()]);
        assert_eq!(delta.only_jsonl, vec!["bd-only-jsonl".to_string()]);
    }

    #[test]
    fn compute_db_jsonl_id_delta_is_empty_when_stores_agree() {
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        insert_minimal_issue(&mut storage, "bd-a");
        insert_minimal_issue(&mut storage, "bd-b");
        drop(storage);

        let jsonl_path = tmp.path().join("issues.jsonl");
        write_jsonl_with_ids(&jsonl_path, &["bd-a", "bd-b"]);

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let delta = compute_db_jsonl_id_delta(&conn, &jsonl_path).unwrap();

        assert_eq!(delta.both_count, 2);
        assert!(delta.only_db.is_empty());
        assert!(delta.only_jsonl.is_empty());
    }

    // -----------------------------------------------------------------
    // br doctor --repair-indexes (beads_rust#288)
    // -----------------------------------------------------------------

    #[test]
    fn wal_checkpoint_stats_complete_accepts_full_checkpoint() {
        let stats = WalCheckpointStats {
            busy: 0,
            log_frames: 7,
            checkpointed_frames: 7,
        };

        assert!(stats.complete());
    }

    #[test]
    fn wal_checkpoint_stats_complete_accepts_non_wal_sentinel() {
        let stats = WalCheckpointStats {
            busy: 0,
            log_frames: -1,
            checkpointed_frames: -1,
        };

        assert!(stats.complete());
    }

    #[test]
    fn wal_checkpoint_stats_complete_rejects_busy_checkpoint() {
        let stats = WalCheckpointStats {
            busy: 1,
            log_frames: 7,
            checkpointed_frames: 0,
        };

        assert!(!stats.complete());
    }

    #[test]
    fn wal_checkpoint_stats_complete_rejects_partial_checkpoint() {
        let stats = WalCheckpointStats {
            busy: 0,
            log_frames: 7,
            checkpointed_frames: 3,
        };

        assert!(!stats.complete());
    }

    #[test]
    fn execute_repair_indexes_succeeds_against_healthy_db_and_retains_snapshot() {
        use crate::config::ConfigPaths;
        use crate::output::OutputContext;

        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");

        // Seed a real schema (so REINDEX has user indexes to act on)
        // + a couple of issue rows so the snapshot is a non-trivial
        // file. The repair-indexes path doesn't touch issues rows,
        // but the test pins that they're untouched after.
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let issue = sample_issue("bd-ri-1", "indexable");
        storage.create_issue(&issue, "test").unwrap();
        drop(storage);

        let paths = ConfigPaths {
            beads_dir: beads_dir.clone(),
            db_path: db_path.clone(),
            jsonl_path: beads_dir.join("issues.jsonl"),
            metadata: crate::config::Metadata::default(),
        };
        let args = DoctorArgs {
            repair: false,
            repair_indexes: true,
            allow_repeated_repair: false,
            dry_run: false,
            robot_triage: false,
            quick: false,
            only: Vec::new(),
            skip: Vec::new(),
            unsafe_auto_fix: false,
            selftest: false,
            selftest_dir: None,
            keep: false,
            bundle: None,
            include_db: false,
            include_jsonl: false,
            bundle_events: 200,
            subcommand: None,
        };
        let ctx = OutputContext::from_flags(false, true, true);
        let cli = crate::config::CliOverrides::default();

        let result = execute_repair_indexes(&beads_dir, &paths, &ctx, &args, &cli);
        assert!(
            result.is_ok(),
            "repair-indexes against a healthy DB must succeed: {:?}",
            result
        );

        // Pre-snapshot backup must remain on disk so the operator
        // has a recoverable pre-state — explicitly part of the
        // contract per the #288 fix comment.
        let snapshot_path = db_path.with_extension("db.pre-repair-indexes");
        assert!(
            snapshot_path.exists(),
            "pre-snapshot backup must be retained at {}",
            snapshot_path.display(),
        );

        // Issue row must survive the REINDEX — the path is
        // index-only and must never touch row data.
        let storage = SqliteStorage::open(&db_path).unwrap();
        let reloaded = storage.get_issue("bd-ri-1").unwrap();
        assert!(reloaded.is_some(), "issue row must survive REINDEX");
    }

    /// Regression: pin that stale `<snapshot>-wal` / `<snapshot>-shm`
    /// files from a previous `--repair-indexes` invocation that hit
    /// a checkpoint failure don't leak into a subsequent successful
    /// invocation's restore semantics. Without the explicit cleanup
    /// at the start of `checkpoint_and_snapshot_repair_indexes`, the
    /// second invocation would see stale sidecar snapshots, treat
    /// them as the "pre-state for current run", and a restore-on-
    /// failure path would resurrect the wrong WAL data.
    #[test]
    fn execute_repair_indexes_clears_stale_sidecar_snapshots_from_previous_run() {
        use crate::config::ConfigPaths;
        use crate::output::OutputContext;

        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");

        let mut storage = SqliteStorage::open(&db_path).unwrap();
        storage
            .create_issue(&sample_issue("bd-stale-sidecar", "test"), "test")
            .unwrap();
        drop(storage);

        // Plant stale sidecar-snapshot files as if a previous run had
        // failed its checkpoint and snapshotted them. The contents
        // are arbitrary — the test only cares that they exist before
        // and DON'T exist after the next successful invocation.
        let snapshot_path = db_path.with_extension("db.pre-repair-indexes");
        let stale_wal = PathBuf::from(format!("{}-wal", snapshot_path.to_string_lossy()));
        let stale_shm = PathBuf::from(format!("{}-shm", snapshot_path.to_string_lossy()));
        fs::write(&stale_wal, b"stale wal from a previous run").unwrap();
        fs::write(&stale_shm, b"stale shm from a previous run").unwrap();

        let paths = ConfigPaths {
            beads_dir: beads_dir.clone(),
            db_path: db_path.clone(),
            jsonl_path: beads_dir.join("issues.jsonl"),
            metadata: crate::config::Metadata::default(),
        };
        let args = DoctorArgs {
            repair: false,
            repair_indexes: true,
            allow_repeated_repair: false,
            dry_run: false,
            robot_triage: false,
            quick: false,
            only: Vec::new(),
            skip: Vec::new(),
            unsafe_auto_fix: false,
            selftest: false,
            selftest_dir: None,
            keep: false,
            bundle: None,
            include_db: false,
            include_jsonl: false,
            bundle_events: 200,
            subcommand: None,
        };
        let ctx = OutputContext::from_flags(false, true, true);
        let cli = crate::config::CliOverrides::default();

        execute_repair_indexes(&beads_dir, &paths, &ctx, &args, &cli)
            .expect("repair-indexes must succeed on healthy DB");

        // Stale sidecar snapshots from the planted previous run must
        // not survive the new invocation. A successful checkpoint
        // means the new `.db` snapshot is complete on its own, so the
        // sidecar snapshots must be cleared — otherwise a future
        // restore would falsely restore the planted stale WAL.
        assert!(
            !stale_wal.exists(),
            "stale snapshot-wal from previous run must be cleared on a successful checkpoint; still present at {}",
            stale_wal.display(),
        );
        assert!(
            !stale_shm.exists(),
            "stale snapshot-shm from previous run must be cleared on a successful checkpoint; still present at {}",
            stale_shm.display(),
        );

        // The current run's `.db` snapshot must exist as forensic
        // evidence regardless.
        assert!(
            snapshot_path.exists(),
            "current run's pre-snapshot must be retained at {}",
            snapshot_path.display(),
        );
    }

    #[test]
    fn checkpoint_and_snapshot_repair_indexes_refuses_unremovable_stale_sidecar_snapshot() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");

        let mut storage = SqliteStorage::open(&db_path).unwrap();
        storage
            .create_issue(&sample_issue("bd-stale-sidecar-dir", "test"), "test")
            .unwrap();
        drop(storage);

        let snapshot_path = db_path.with_extension("db.pre-repair-indexes");
        let stale_wal = PathBuf::from(format!("{}-wal", snapshot_path.to_string_lossy()));
        fs::create_dir(&stale_wal).unwrap();
        let write_authority =
            acquire_doctor_database_write_authority(&beads_dir, &db_path, Some(1_000)).unwrap();

        let err =
            checkpoint_and_snapshot_repair_indexes(&db_path, &snapshot_path, &write_authority)
                .expect_err("unremovable stale sidecar snapshot must fail closed");
        let message = err.to_string();
        assert!(
            message.contains("failed to remove stale sidecar snapshot")
                && message.contains("refusing to continue"),
            "unexpected error: {message}",
        );
        assert!(
            !snapshot_path.exists(),
            "must fail before writing a new DB snapshot at {}",
            snapshot_path.display(),
        );
    }

    #[cfg(unix)]
    #[test]
    fn checkpoint_and_snapshot_repair_indexes_refuses_symlinked_snapshot_target() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");

        let mut storage = SqliteStorage::open(&db_path).unwrap();
        storage
            .create_issue(&sample_issue("bd-snapshot-symlink", "test"), "test")
            .unwrap();
        drop(storage);

        let outside = TempDir::new().unwrap();
        let outside_target = outside.path().join("must-not-overwrite");
        fs::write(&outside_target, b"preserve me").unwrap();

        let snapshot_path = db_path.with_extension("db.pre-repair-indexes");
        symlink(&outside_target, &snapshot_path).unwrap();
        let write_authority =
            acquire_doctor_database_write_authority(&beads_dir, &db_path, Some(1_000)).unwrap();

        let err =
            checkpoint_and_snapshot_repair_indexes(&db_path, &snapshot_path, &write_authority)
                .expect_err("symlinked pre-snapshot target must fail closed");
        let message = err.to_string();
        assert!(
            message.contains("refusing to write pre-snapshot backup through symlink"),
            "unexpected error: {message}",
        );
        assert_eq!(
            fs::read(&outside_target).unwrap(),
            b"preserve me",
            "snapshot creation must not write through the symlink target"
        );
        assert!(
            fs::symlink_metadata(&snapshot_path)
                .unwrap()
                .file_type()
                .is_symlink(),
            "refused snapshot symlink should be left in place for operator inspection"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_fix_recovery_artifacts_aged_is_idempotent_no_op_on_second_call() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");

        let mut storage = SqliteStorage::open(&db_path).unwrap();
        storage
            .create_issue(&sample_issue("bd-aged-recovery", "test"), "test")
            .unwrap();
        drop(storage);

        let recovery_dir = beads_dir.join(".br_recovery");
        fs::create_dir_all(&recovery_dir).unwrap();
        let aged = recovery_dir.join("beads.db.20250101T000000Z");
        let recent = recovery_dir.join("beads.db.20260512T000000Z");
        let bad_sibling = beads_dir.join("beads.db.bad_20250101T000000Z");
        fs::copy(&db_path, &aged).unwrap();
        fs::copy(&db_path, &recent).unwrap();
        fs::copy(&db_path, &bad_sibling).unwrap();

        for path in [&aged, &bad_sibling] {
            let status = Command::new("touch")
                .args(["-d", "60 days ago"])
                .arg(path)
                .status()
                .unwrap();
            assert!(status.success(), "touch failed for {}", path.display());
        }

        let report = DoctorReport {
            ok: false,
            workspace_health: None,
            reliability_audit: None,
            checks: vec![CheckResult {
                name: "db.recovery_artifacts.aged".to_string(),
                status: CheckStatus::Warn,
                message: None,
                details: None,
            }],
        };
        let ctx = OutputContext::from_output_format(crate::cli::OutputFormat::Json, false, true);
        let mut session = DoctorRepairSession::new(tmp.path(), /* dry_run = */ false)
            .expect("session must build");

        assert!(fix_recovery_artifacts_aged_if_warned(
            &beads_dir,
            &db_path,
            &report,
            &ctx,
            Some(&mut session),
        ));
        assert!(!aged.exists());
        assert!(!bad_sibling.exists());
        assert!(
            recent.is_file(),
            "recent recovery artifacts must remain in place"
        );

        let actions_before = fs::read_to_string(&session.run.actions_file).unwrap();
        assert_eq!(actions_before.matches("\"op\":\"rename\"").count(), 2);
        assert!(!fix_recovery_artifacts_aged_if_warned(
            &beads_dir,
            &db_path,
            &report,
            &ctx,
            Some(&mut session),
        ));
        let actions_after = fs::read_to_string(&session.run.actions_file).unwrap();
        assert_eq!(actions_after, actions_before, "second pass must be a no-op");
    }

    #[test]
    fn test_fix_export_hash_cache_uses_resolved_database_path() {
        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let default_db = beads_dir.join("beads.db");
        let custom_db = beads_dir.join("custom.db");
        let jsonl_path = beads_dir.join("issues.jsonl");
        fs::write(&jsonl_path, br#"{"id":"custom-db-path"}"#).unwrap();

        let mut default_storage = SqliteStorage::open(&default_db).unwrap();
        default_storage
            .set_metadata(
                crate::sync::METADATA_JSONL_CONTENT_HASH,
                "default-db-must-not-change",
            )
            .unwrap();
        drop(default_storage);

        let mut custom_storage = SqliteStorage::open(&custom_db).unwrap();
        custom_storage
            .set_metadata(crate::sync::METADATA_JSONL_CONTENT_HASH, "custom-db-stale")
            .unwrap();
        drop(custom_storage);

        let report = DoctorReport {
            ok: false,
            workspace_health: None,
            reliability_audit: None,
            checks: vec![CheckResult {
                name: "db.export_hash_cache".to_string(),
                status: CheckStatus::Warn,
                message: None,
                details: None,
            }],
        };
        let ctx = OutputContext::from_output_format(crate::cli::OutputFormat::Json, false, true);
        let mut session = DoctorRepairSession::new(tmp.path(), /* dry_run = */ false)
            .expect("session must build");
        let expected_hash = crate::sync::compute_jsonl_hash(&jsonl_path).unwrap();

        assert!(fix_export_hash_cache_divergence_if_warned(
            &custom_db,
            Some(&jsonl_path),
            &report,
            &ctx,
            Some(&mut session),
        ));

        let custom_storage = SqliteStorage::open(&custom_db).unwrap();
        assert_eq!(
            custom_storage
                .get_metadata(crate::sync::METADATA_JSONL_CONTENT_HASH)
                .unwrap(),
            Some(expected_hash)
        );
        let default_storage = SqliteStorage::open(&default_db).unwrap();
        assert_eq!(
            default_storage
                .get_metadata(crate::sync::METADATA_JSONL_CONTENT_HASH)
                .unwrap(),
            Some("default-db-must-not-change".to_string())
        );
    }

    #[test]
    fn execute_repair_indexes_reuses_startup_write_lock() {
        use crate::config::{CliOverrides, ConfigPaths};
        use crate::output::OutputContext;

        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");

        let mut storage = SqliteStorage::open(&db_path).unwrap();
        storage
            .create_issue(&sample_issue("bd-ri-lock-1", "lock reuse"), "test")
            .unwrap();
        drop(storage);

        let startup_lock = std::sync::Arc::new(
            crate::sync::blocking_database_family_write_lock_with_timeout(
                &beads_dir,
                &db_path,
                Some(0),
            )
            .unwrap(),
        );
        let mut cli = CliOverrides::default();
        cli.mark_database_family_lock_held(&beads_dir, &startup_lock);
        let paths = ConfigPaths {
            beads_dir: beads_dir.clone(),
            db_path: db_path.clone(),
            jsonl_path: beads_dir.join("issues.jsonl"),
            metadata: crate::config::Metadata::default(),
        };
        let args = DoctorArgs {
            repair: false,
            repair_indexes: true,
            allow_repeated_repair: false,
            dry_run: false,
            robot_triage: false,
            quick: false,
            only: Vec::new(),
            skip: Vec::new(),
            unsafe_auto_fix: false,
            selftest: false,
            selftest_dir: None,
            keep: false,
            bundle: None,
            include_db: false,
            include_jsonl: false,
            bundle_events: 200,
            subcommand: None,
        };
        let ctx = OutputContext::from_flags(false, true, true);

        execute_repair_indexes(&beads_dir, &paths, &ctx, &args, &cli).unwrap();

        let snapshot_path = db_path.with_extension("db.pre-repair-indexes");
        assert!(
            snapshot_path.exists(),
            "held-lock repair-indexes should still retain the pre-snapshot"
        );
        let storage = SqliteStorage::open(&db_path).unwrap();
        assert!(storage.get_issue("bd-ri-lock-1").unwrap().is_some());
    }

    #[test]
    fn execute_repair_indexes_quotes_names_that_need_quoting() {
        use crate::config::ConfigPaths;
        use crate::output::OutputContext;

        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");

        let mut storage = SqliteStorage::open(&db_path).unwrap();
        storage
            .create_issue(&sample_issue("bd-ri-quoted-1", "quoted index"), "test")
            .unwrap();
        drop(storage);

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        conn.execute(r#"CREATE INDEX "123doctor_weird" ON issues(title)"#)
            .unwrap();
        conn.execute(r#"CREATE INDEX "doctor""quoted" ON issues(status)"#)
            .unwrap();
        conn.close().unwrap();

        let paths = ConfigPaths {
            beads_dir: beads_dir.clone(),
            db_path: db_path.clone(),
            jsonl_path: beads_dir.join("issues.jsonl"),
            metadata: crate::config::Metadata::default(),
        };
        let args = DoctorArgs {
            repair: false,
            repair_indexes: true,
            allow_repeated_repair: false,
            dry_run: false,
            robot_triage: false,
            quick: false,
            only: Vec::new(),
            skip: Vec::new(),
            unsafe_auto_fix: false,
            selftest: false,
            selftest_dir: None,
            keep: false,
            bundle: None,
            include_db: false,
            include_jsonl: false,
            bundle_events: 200,
            subcommand: None,
        };
        let ctx = OutputContext::from_flags(false, true, true);
        let cli = crate::config::CliOverrides::default();

        execute_repair_indexes(&beads_dir, &paths, &ctx, &args, &cli).unwrap();

        // The two deliberately noncanonical indexes must remain present so
        // this test exercises their quoted REINDEX statements.  A normal
        // SqliteStorage open correctly rejects unexpected explicit indexes,
        // so inspect the preserved issue through the raw connection instead.
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let issue = conn
            .query_row("SELECT id FROM issues WHERE id = 'bd-ri-quoted-1'")
            .unwrap();
        assert_eq!(
            issue.get(0).and_then(SqliteValue::as_text),
            Some("bd-ri-quoted-1")
        );
        conn.close().unwrap();
    }

    #[test]
    fn quote_sql_identifier_escapes_embedded_quotes() {
        assert_eq!(quote_sql_identifier("doctor_idx"), "\"doctor_idx\"");
        assert_eq!(quote_sql_identifier("123doctor_idx"), "\"123doctor_idx\"");
        assert_eq!(
            quote_sql_identifier("doctor\"quoted"),
            "\"doctor\"\"quoted\""
        );
    }

    #[test]
    fn execute_repair_indexes_dry_run_skips_mutation() {
        use crate::config::ConfigPaths;
        use crate::output::OutputContext;

        let tmp = TempDir::new().unwrap();
        let beads_dir = tmp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let db_path = beads_dir.join("beads.db");

        let mut storage = SqliteStorage::open(&db_path).unwrap();
        storage
            .create_issue(&sample_issue("bd-ri-dry-1", "untouched"), "test")
            .unwrap();
        drop(storage);

        let pre_mtime = fs::metadata(&db_path).unwrap().modified().unwrap();

        let paths = ConfigPaths {
            beads_dir: beads_dir.clone(),
            db_path: db_path.clone(),
            jsonl_path: beads_dir.join("issues.jsonl"),
            metadata: crate::config::Metadata::default(),
        };
        let args = DoctorArgs {
            repair: false,
            repair_indexes: true,
            allow_repeated_repair: false,
            dry_run: true,
            robot_triage: false,
            quick: false,
            only: Vec::new(),
            skip: Vec::new(),
            unsafe_auto_fix: false,
            selftest: false,
            selftest_dir: None,
            keep: false,
            bundle: None,
            include_db: false,
            include_jsonl: false,
            bundle_events: 200,
            subcommand: None,
        };
        let ctx = OutputContext::from_flags(false, true, true);
        let cli = crate::config::CliOverrides::default();

        execute_repair_indexes(&beads_dir, &paths, &ctx, &args, &cli).unwrap();

        // Dry-run must not even take the pre-snapshot — the file
        // shouldn't exist.
        let snapshot_path = db_path.with_extension("db.pre-repair-indexes");
        assert!(
            !snapshot_path.exists(),
            "dry-run must not create a snapshot: {}",
            snapshot_path.display(),
        );

        // DB mtime should not have changed.
        let post_mtime = fs::metadata(&db_path).unwrap().modified().unwrap();
        assert_eq!(
            pre_mtime, post_mtime,
            "dry-run must not modify the live DB file"
        );
    }

    #[test]
    fn compute_db_jsonl_id_delta_skips_wisp_ids() {
        // Wisp-suffixed ids are intentionally filtered out of the DB
        // cardinality query (`id NOT LIKE '%-wisp-%'`). The delta
        // computation must apply the same filter on the JSONL side
        // so a wisp record present in JSONL but absent from the DB
        // doesn't surface as a spurious `only_jsonl` finding.
        let tmp = TempDir::new().unwrap();
        let db_path = tmp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        insert_minimal_issue(&mut storage, "bd-a");
        drop(storage);

        let jsonl_path = tmp.path().join("issues.jsonl");
        write_jsonl_with_ids(&jsonl_path, &["bd-a", "bd-wisp-ephemeral"]);

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let delta = compute_db_jsonl_id_delta(&conn, &jsonl_path).unwrap();

        // The wisp id must be filtered out, otherwise the delta would
        // (incorrectly) report bd-wisp-ephemeral as only_jsonl.
        assert_eq!(delta.both_count, 1);
        assert!(delta.only_db.is_empty(), "{:?}", delta.only_db);
        assert!(
            delta.only_jsonl.is_empty(),
            "wisp ids must be filtered from the JSONL side too: {:?}",
            delta.only_jsonl
        );
    }

    // --- finding-id table tests (pass-3 / gap item #3) ---

    #[test]
    fn finding_id_table_has_no_duplicate_check_names() {
        // Each check.name maps to AT MOST one canonical FM. A duplicate
        // row would silently shadow the second entry under the
        // linear-scan lookup; reject at test time.
        let mut seen: std::collections::HashSet<&'static str> = std::collections::HashSet::new();
        for (name, _) in CHECK_NAME_TO_FINDING_ID {
            assert!(
                seen.insert(*name),
                "duplicate check_name {name} in CHECK_NAME_TO_FINDING_ID"
            );
        }
    }

    #[test]
    fn finding_id_table_uses_canonical_fm_form() {
        // FM identifiers must match `fm-<subsystem>-<slug>` (no
        // leading/trailing whitespace, no underscores in the
        // subsystem half — the workspace uses snake_case for
        // subsystem names and kebab-case for slugs).
        for (name, fm_id) in CHECK_NAME_TO_FINDING_ID {
            assert!(
                fm_id.starts_with("fm-"),
                "finding_id for {name} must start with `fm-`: {fm_id}"
            );
            assert!(
                fm_id.len() > "fm-".len(),
                "finding_id for {name} is just the prefix: {fm_id}"
            );
            assert!(
                !fm_id.contains(' '),
                "finding_id for {name} contains whitespace: {fm_id}"
            );
        }
    }

    #[test]
    fn finding_id_for_returns_canonical_form() {
        // Spot-check three known entries across distinct subsystems.
        assert_eq!(
            finding_id_for("routes_jsonl"),
            Some("fm-routes_external-routes-jsonl-corrupt")
        );
        assert_eq!(
            finding_id_for("rust_log"),
            Some("fm-observability-rust-log-noisy-breaks-json")
        );
        assert_eq!(
            finding_id_for("audit.suspect_close_reasons"),
            Some("fm-agent_coordination-suspect-close-reason")
        );
    }

    #[test]
    fn finding_id_for_returns_none_for_unmapped() {
        assert_eq!(finding_id_for("definitely_not_a_real_check"), None);
        assert_eq!(finding_id_for(""), None);
    }

    #[test]
    fn push_check_injects_finding_id_into_object_details() {
        let mut checks = Vec::new();

        push_check(
            &mut checks,
            "db.exists",
            CheckStatus::Ok,
            None,
            Some(serde_json::json!({ "path": ".beads/beads.db" })),
        );

        let details = checks[0]
            .details
            .as_ref()
            .expect("mapped checks should carry finding_id details");
        assert_eq!(
            details
                .get("finding_id")
                .and_then(serde_json::Value::as_str),
            Some("fm-state_files-empty-or-truncated-database")
        );
        assert_eq!(
            details.get("path").and_then(serde_json::Value::as_str),
            Some(".beads/beads.db")
        );
    }

    #[test]
    fn push_check_wraps_non_object_details_with_finding_id() {
        let mut checks = Vec::new();

        push_check(
            &mut checks,
            "db.exists",
            CheckStatus::Warn,
            None,
            Some(serde_json::json!(["legacy-shape"])),
        );

        let details = checks[0]
            .details
            .as_ref()
            .expect("mapped checks should carry finding_id details");
        assert_eq!(
            details
                .get("finding_id")
                .and_then(serde_json::Value::as_str),
            Some("fm-state_files-empty-or-truncated-database")
        );
        assert_eq!(
            details.get("data"),
            Some(&serde_json::json!(["legacy-shape"]))
        );
    }

    #[test]
    fn push_check_leaves_unmapped_details_unchanged() {
        let mut checks = Vec::new();
        let original = serde_json::json!({ "path": ".beads/other" });

        push_check(
            &mut checks,
            "unmapped.check",
            CheckStatus::Error,
            None,
            Some(original.clone()),
        );

        assert_eq!(checks[0].details, Some(original));
    }
}
