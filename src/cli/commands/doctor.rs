//! Doctor command implementation.

#![allow(clippy::option_if_let_else)]

use crate::cli::DoctorArgs;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::health::{AnomalyClass, ReliabilityAuditRecord, WorkspaceClassification};
use crate::output::OutputContext;
use crate::storage::SqliteStorage;
use crate::sync::{
    JsonlTombstoneFilter, PathValidation, PreservedTombstone, compute_staleness,
    restore_tombstones_after_rebuild, scan_conflict_markers, scan_jsonl_for_tombstone_filter,
    snapshot_tombstones, tombstones_missing_from_jsonl_tombstones, validate_jsonl_issue_records,
    validate_no_git_path, validate_sync_path, validate_sync_path_with_external,
};
use chrono::Utc;
use fsqlite::Connection;
use fsqlite_error::FrankenError;
use fsqlite_types::SqliteValue;
use rich_rust::prelude::*;
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

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
    #[serde(skip_serializing_if = "Vec::is_empty")]
    verified_backups: Vec<config::RecoveryBackupVerification>,
}

#[derive(Debug, Clone)]
struct DoctorRun {
    report: DoctorReport,
    jsonl_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, Serialize)]
struct LocalRepairResult {
    blocked_cache_rebuilt: bool,
    indexes_reindexed: bool,
    vacuumed: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    quarantined_artifacts: Vec<String>,
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

#[derive(Debug, Default)]
struct SidecarInspection {
    /// Error-level findings that indicate a genuine problem requiring repair.
    findings: Vec<String>,
    /// Warning-level findings that are informational but do not require repair.
    warning_findings: Vec<String>,
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

fn no_op_repair_audit_record(repaired_gitignore: bool) -> RecoveryAuditRecord {
    let applied_actions = if repaired_gitignore {
        vec!["gitignore_repaired".to_string()]
    } else {
        Vec::new()
    };
    RecoveryAuditRecord {
        phase: "doctor.noop".to_string(),
        action: "repair".to_string(),
        outcome: if repaired_gitignore {
            "gitignore_repaired".to_string()
        } else {
            "nothing_to_repair".to_string()
        },
        reason: None,
        applied_actions,
        quarantined_artifacts: Vec::new(),
        verified_backups: Vec::new(),
        imported: None,
        skipped: None,
        fk_violations_cleaned: None,
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
    checks.push(CheckResult {
        name: name.to_string(),
        status,
        message,
        details,
    });
}

fn has_error(checks: &[CheckResult]) -> bool {
    checks
        .iter()
        .any(|check| matches!(check.status, CheckStatus::Error))
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

    push_anomaly(
        anomalies,
        AnomalyClass::DbJsonlCountMismatch {
            db_count,
            jsonl_count,
        },
    );
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
            let message = check.message.as_deref().unwrap_or_default();
            if message.contains("External changes pending import") {
                push_anomaly(anomalies, AnomalyClass::JsonlNewer);
            } else if message.contains("Local changes pending export") {
                push_anomaly(anomalies, AnomalyClass::DbNewer);
            }
        }
        "db.recovery_artifacts" if matches!(check.status, CheckStatus::Warn) => {
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
fn repair_via_vacuum(db_path: &Path, repair: &mut LocalRepairResult) {
    if !db_path.is_file() {
        tracing::debug!(
            path = %db_path.display(),
            "Skipping VACUUM because the database file is missing"
        );
        return;
    }
    match SqliteStorage::open(db_path) {
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
}

/// Write probe: verify the database can actually perform writes after repair.
///
/// Issue #245: REINDEX can fix index ordering so `PRAGMA integrity_check`
/// passes, but the underlying B-tree corruption may silently cause writes to
/// fail (reads work, writes get ISSUE_NOT_FOUND).  This probe inserts a row,
/// reads it back, then ROLLS BACK the entire transaction so no data is
/// persisted.  The insert-then-read pattern catches read-after-write
/// divergence that a simple no-op UPDATE would miss.
fn write_probe_after_repair(db_path: &Path) -> bool {
    let Ok(conn) = Connection::open(db_path.to_string_lossy().into_owned()) else {
        return false;
    };
    let _ = conn.execute("PRAGMA busy_timeout=5000");

    // Use a probe ID that cannot collide with real issues.
    let probe_id = "__doctor_write_probe__";
    let now = chrono::Utc::now().to_rfc3339();

    let probe = (|| -> std::result::Result<(), Box<dyn std::error::Error>> {
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
            tracing::warn!("Write probe: INSERT succeeded but SELECT returned no rows");
            return Err("read-after-write divergence".into());
        }

        // Always ROLLBACK — the probe is non-destructive.  No data is
        // persisted, so JSONL export state stays clean.
        conn.execute("ROLLBACK")?;
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

    probe_ok
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
    report.ok && !report_has_warn_level_page_anomaly(report)
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

fn repair_outcome_message(
    gitignore_repaired: bool,
    local_repair: Option<&LocalRepairResult>,
    incomplete_attempt_message: Option<&str>,
) -> String {
    let mut messages = Vec::new();

    if gitignore_repaired {
        messages.push(ROOT_GITIGNORE_REPAIR_MESSAGE.to_string());
    }

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
        // frankensqlite manages the WAL index in process-local memory rather than in an SHM
        // file, so a WAL without a sibling SHM is the normal operating state — not an error.
        // We record this as a warning finding so callers can observe it, but we do not
        // quarantine the WAL, because the WAL is valid and the database is accessible.
        // The db.write_probe check validates liveness.
        inspection.warning_findings.push(format!(
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

    if !db_kind.is_regular_file() {
        let has_dangling_sidecars = database_sidecar_paths(db_path)
            .into_iter()
            .any(|(path, _)| {
                classify_path_kind(&path)
                    .ok()
                    .is_some_and(FilesystemPathKind::exists)
            });
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

    if !inspection.warning_findings.is_empty() {
        push_check(
            checks,
            "db.sidecars",
            CheckStatus::Warn,
            Some(inspection.warning_findings[0].clone()),
            Some(serde_json::json!({
                "findings": inspection.warning_findings,
            })),
        );
        return Ok(());
    }

    push_check(checks, "db.sidecars", CheckStatus::Ok, None, None);
    Ok(())
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
) -> Result<DoctorRepairResult> {
    preflight_jsonl_rebuild_authority(jsonl_path)?;

    let bootstrap_layer = config::ConfigLayer::merge_layers(&[
        config::load_startup_config(beads_dir)?,
        cli.as_layer(),
    ]);

    // Snapshot any local tombstones before the JSONL rebuild. `doctor
    // --repair` reaches this branch only after light repairs failed (or
    // never applied) and the on-disk DB reports errors, so the storage
    // handle here might be limping — but `snapshot_tombstones` is already
    // fault-tolerant (warn+empty on enumeration failure, warn+partial on
    // per-tombstone failure) and the cost of trying is a few selects.
    // Without this, repair would silently wipe any tombstone the user
    // deleted but had not yet flushed to JSONL (same hazard that
    // `br sync --rebuild` preserves via snapshot/restore), since the
    // rebuild only replays what's in the JSONL.
    let preserved_tombstones = preserved_tombstones_for_doctor_rebuild(db_path, jsonl_path);

    let (mut storage, import_result, verified_backups) = config::repair_database_from_jsonl(
        beads_dir,
        db_path,
        jsonl_path,
        cli.lock_timeout,
        &bootstrap_layer,
        show_progress,
        false,
    )?;

    restore_tombstones_after_rebuild(&mut storage, &preserved_tombstones)?;

    let fk_violations_cleaned = cleanup_repair_missing_issue_references(&mut storage)?;

    Ok(DoctorRepairResult {
        imported: import_result.imported_count,
        skipped: import_result.skipped_count,
        fk_violations_cleaned,
        verified_backups,
    })
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

fn preflight_jsonl_rebuild_authority(jsonl_path: &Path) -> Result<()> {
    let conflict_markers = scan_conflict_markers(jsonl_path)?;
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

    let validation = validate_jsonl_issue_records(jsonl_path)?;
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
    if let BeadsError::Config(message) = err
        && message.starts_with(JSONL_REBUILD_AUTHORITY_ERROR_PREFIX)
    {
        return "refused";
    }
    "failed"
}

fn write_jsonl_rebuild_verification_failed_marker(
    beads_dir: &Path,
    db_path: &Path,
    post_repair: &DoctorRun,
    repair_result: &DoctorRepairResult,
) -> Result<PathBuf> {
    let recovery_dir = config::recovery_dir_for_db_path(db_path, beads_dir);
    fs::create_dir_all(&recovery_dir)?;
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
        "verified_backups": &repair_result.verified_backups,
        "workspace_health": post_repair.report.workspace_health.as_deref(),
        "failed_checks": failed_checks,
    });
    fs::write(&marker_path, serde_json::to_vec_pretty(&payload)?)?;
    Ok(marker_path)
}

/// Best-effort snapshot of unflushed local tombstones, guarded on every
/// failure mode the doctor-repair path may encounter (DB missing, DB can't
/// be opened, JSONL unreadable).
///
/// This mirrors the helper at the config layer (`preserved_unflushed_tombstones`)
/// but has to live here because the doctor-repair entry point is where we have
/// the storage-open attempt — the config helper receives an already-open
/// storage handle. Returns an empty vector if no tombstones survive filtering
/// or if any step fails; the rebuild itself always proceeds either way, and
/// `snapshot_tombstones` logs its own best-effort warnings.
fn preserved_tombstones_for_doctor_rebuild(
    db_path: &Path,
    jsonl_path: &Path,
) -> Vec<PreservedTombstone> {
    if !db_path.is_file() {
        return Vec::new();
    }
    let storage = match SqliteStorage::open(db_path) {
        Ok(storage) => storage,
        Err(err) => {
            tracing::debug!(
                db_path = %db_path.display(),
                error = %err,
                "Could not open DB for pre-repair tombstone snapshot; proceeding without preservation"
            );
            return Vec::new();
        }
    };
    let snapshot = snapshot_tombstones(&storage);
    drop(storage);
    if snapshot.is_empty() {
        return snapshot;
    }
    let jsonl_filter = if jsonl_path.is_file() {
        match scan_jsonl_for_tombstone_filter(jsonl_path) {
            Ok(filter) => filter,
            Err(err) => {
                tracing::debug!(
                    jsonl_path = %jsonl_path.display(),
                    error = %err,
                    "Could not scan JSONL for tombstone filter during doctor --repair; preserving every snapshotted tombstone and letting the rebuild surface the JSONL error"
                );
                JsonlTombstoneFilter::default()
            }
        }
    } else {
        JsonlTombstoneFilter::default()
    };
    tombstones_missing_from_jsonl_tombstones(snapshot, &jsonl_filter)
}

fn repair_recoverable_db_state(
    beads_dir: &Path,
    db_path: &Path,
    report: &DoctorReport,
) -> LocalRepairResult {
    let mut repair = LocalRepairResult::default();

    if report_has_sidecar_anomaly(report) {
        repair_database_sidecars(beads_dir, db_path, &mut repair);
    }

    if !db_path.is_file() {
        tracing::debug!(
            path = %db_path.display(),
            "Skipping blocked-cache repair because the database file is missing"
        );
        return repair;
    }

    match SqliteStorage::open(db_path) {
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
                    repair
                }
                Err(err) => {
                    tracing::warn!(
                        path = %db_path.display(),
                        error = %err,
                        "Skipping blocked-cache repair; falling back to JSONL rebuild"
                    );
                    repair
                }
            }
        }
        Err(err) => {
            tracing::warn!(
                path = %db_path.display(),
                error = %err,
                "Skipping blocked-cache repair because the database could not be opened"
            );
            repair
        }
    }
}

/// Rebuild all indexes via `REINDEX` to fix partial-index row mismatches.
///
/// This is safe — `REINDEX` only rebuilds existing indexes from the underlying
/// table data.  It does not modify any row data.
fn repair_partial_indexes(db_path: &Path, repair: &mut LocalRepairResult) {
    if !db_path.is_file() {
        tracing::debug!(
            path = %db_path.display(),
            "Skipping REINDEX because the database file is missing"
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
        }
        Err(err) => {
            tracing::warn!(
                path = %db_path.display(),
                error = %err,
                "Skipping REINDEX because the database could not be opened"
            );
        }
    }
}

fn repair_database_sidecars(beads_dir: &Path, db_path: &Path, repair: &mut LocalRepairResult) {
    match inspect_database_sidecars(db_path) {
        Ok(_) => quarantine_anomalous_sidecars(beads_dir, db_path, repair),
        Err(err) => tracing::warn!(
            path = %db_path.display(),
            error = %err,
            "Skipping sidecar repair because filesystem inspection failed"
        ),
    }
}

fn quarantine_anomalous_sidecars(beads_dir: &Path, db_path: &Path, repair: &mut LocalRepairResult) {
    match inspect_database_sidecars(db_path) {
        Ok(post_checkpoint_inspection) => {
            let quarantine_paths: BTreeSet<_> = post_checkpoint_inspection
                .quarantine_candidates
                .into_iter()
                .collect();

            if !quarantine_paths.is_empty() {
                match config::quarantine_database_artifacts(
                    db_path,
                    beads_dir,
                    quarantine_paths,
                    "doctor-quarantine",
                ) {
                    Ok(quarantined) => {
                        repair.quarantined_artifacts = quarantined
                            .into_iter()
                            .map(|path| path.display().to_string())
                            .collect();
                    }
                    Err(err) => tracing::warn!(
                        path = %db_path.display(),
                        error = %err,
                        "Failed to quarantine anomalous database sidecar artifacts"
                    ),
                }
            }
        }
        Err(err) => tracing::warn!(
            path = %db_path.display(),
            error = %err,
            "Failed to re-inspect database sidecars after local repair"
        ),
    }
}

#[allow(clippy::unnecessary_wraps)]
fn print_report(report: &DoctorReport, ctx: &OutputContext) -> Result<()> {
    if ctx.is_json() {
        ctx.json(report);
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

    let blocked_cache_stale = conn.query(
        "SELECT value
         FROM metadata
         WHERE key = 'blocked_cache_state'
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

    if blocked_cache_stale
        .first()
        .and_then(|row| row.get(0).and_then(SqliteValue::as_text))
        == Some("stale")
    {
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

fn sqlite_cli_integrity_messages(db_path: &Path) -> Result<Vec<String>> {
    let output = Command::new("sqlite3")
        .arg(db_path)
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
            push_check(
                checks,
                "sqlite3.integrity_check",
                CheckStatus::Warn,
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

fn check_merge_artifacts(beads_dir: &Path, checks: &mut Vec<CheckResult>) -> Result<()> {
    let mut artifacts = Vec::new();
    for entry in beads_dir.read_dir()? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name.contains(".base.jsonl")
            || name.contains(".left.jsonl")
            || name.contains(".right.jsonl")
        {
            artifacts.push(name.to_string());
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
            Some(serde_json::json!({ "files": artifacts })),
        );
    }
    Ok(())
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

/// When `--repair` is passed and the `gitignore.beads_inner` warning is present,
/// automatically remove the offending lines from the root `.gitignore`.
fn fix_root_gitignore_if_warned(
    beads_dir: &Path,
    report: &DoctorReport,
    ctx: &OutputContext,
) -> bool {
    let has_warning = report
        .checks
        .iter()
        .any(|c| c.name == "gitignore.beads_inner" && c.status == CheckStatus::Warn);
    if !has_warning {
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

    if let Err(err) = write_root_gitignore_atomically(&gitignore_path, new_content.as_bytes()) {
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
    let has_env_override = std::env::var("BEADS_JSONL")
        .ok()
        .is_some_and(|value| !value.trim().is_empty());

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
            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            let db_count_usize = db_count as usize;
            if db_count_usize == jsonl_count {
                push_check(
                    checks,
                    "counts.db_vs_jsonl",
                    CheckStatus::Ok,
                    Some(format!("Both have {db_count} records")),
                    None,
                );
            } else {
                push_check(
                    checks,
                    "counts.db_vs_jsonl",
                    CheckStatus::Warn,
                    Some("DB and JSONL counts differ".to_string()),
                    Some(serde_json::json!({
                        "db": db_count,
                        "jsonl": jsonl_count
                    })),
                );
            }
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
    let last_import: Option<String> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = 'last_import_time' ORDER BY rowid DESC LIMIT 1",
        )
        .ok()
        .and_then(|row| {
            row.get(0)
                .and_then(SqliteValue::as_text)
                .filter(|value| !value.is_empty())
                .map(String::from)
        });

    let last_export: Option<String> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = 'last_export_time' ORDER BY rowid DESC LIMIT 1",
        )
        .ok()
        .and_then(|row| {
            row.get(0)
                .and_then(SqliteValue::as_text)
                .filter(|value| !value.is_empty())
                .map(String::from)
        });

    let jsonl_hash: Option<String> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = 'jsonl_content_hash' ORDER BY rowid DESC LIMIT 1",
        )
        .ok()
        .and_then(|row| {
            row.get(0)
                .and_then(SqliteValue::as_text)
                .filter(|value| !value.is_empty())
                .map(String::from)
        });

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
    let (jsonl_newer, db_newer) = if let Some(p) = jsonl_path {
        match SqliteStorage::open(db_path).and_then(|storage| compute_staleness(&storage, p)) {
            Ok(staleness) => (staleness.jsonl_newer, staleness.db_newer),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    "compute_staleness failed in doctor; falling back to dirty-count only"
                );
                (false, dirty_count > 0)
            }
        }
    } else {
        (false, dirty_count > 0)
    };

    // Check 1: Metadata consistency
    if last_export.is_none() && dirty_count > 0 {
        push_check(
            checks,
            "sync.metadata",
            CheckStatus::Warn,
            Some(
                "JSONL exists but no export recorded; consider running sync --flush-only"
                    .to_string(),
            ),
            Some(details),
        );
    } else {
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
                push_check(
                    checks,
                    "sync.metadata",
                    CheckStatus::Ok, // Acceptable state
                    Some("External changes pending import".to_string()),
                    Some(details),
                );
            }
            (false, true) => {
                push_check(
                    checks,
                    "sync.metadata",
                    CheckStatus::Ok, // Acceptable state
                    Some("Local changes pending export".to_string()),
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
}

fn collect_doctor_report(beads_dir: &Path, paths: &config::ConfigPaths) -> Result<DoctorRun> {
    let mut checks = Vec::new();
    check_merge_artifacts(beads_dir, &mut checks)?;
    check_root_gitignore(beads_dir, &mut checks);

    let (jsonl_path, jsonl_count) = inspect_doctor_jsonl(beads_dir, paths, &mut checks);
    inspect_doctor_database(
        beads_dir,
        &paths.db_path,
        jsonl_path.as_deref(),
        jsonl_count,
        &mut checks,
    );

    let classification = classify_doctor_checks(&paths.db_path, &paths.jsonl_path, &checks);
    let reliability_audit = classification.audit_record("doctor.inspect");
    let ok = !has_error(&checks);
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
    checks: &mut Vec<CheckResult>,
) -> (Option<PathBuf>, JsonlCountState) {
    let jsonl_path = select_doctor_jsonl_path(beads_dir, paths);
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

    (jsonl_path, jsonl_count)
}

fn inspect_doctor_database(
    beads_dir: &Path,
    db_path: &Path,
    jsonl_path: Option<&Path>,
    jsonl_count: JsonlCountState,
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
    if let Err(err) = check_database_sidecars(db_path, checks) {
        push_inspection_error(
            checks,
            "db.sidecars",
            "Failed to inspect database sidecars",
            &err,
        );
    }

    if db_path.exists() {
        inspect_existing_doctor_database(db_path, jsonl_path, jsonl_count, checks);
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
    checks: &mut Vec<CheckResult>,
) {
    match config::with_database_family_snapshot(db_path, |snapshot_db_path| {
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
        if let Err(err) = check_recoverable_anomalies(&conn, checks) {
            push_inspection_error(
                checks,
                "db.recoverable_anomalies",
                "Failed to inspect recoverable anomalies",
                &err,
            );
        }
        check_null_defaults(&conn, checks);
        check_integrity(&conn, checks);
        if let Err(err) = check_db_count(&conn, jsonl_count, checks) {
            push_inspection_error(
                checks,
                "counts.db_vs_jsonl",
                "Failed to compare database and JSONL counts",
                &err,
            );
        }
        check_sync_metadata(&conn, snapshot_db_path, jsonl_path, checks);
        check_issue_write_probe(&conn, checks);
        conn.close()?;
        Ok(())
    }) {
        Ok(()) => {
            check_sqlite_cli_integrity(db_path, checks);
        }
        Err(err) => {
            push_check(
                checks,
                "db.open",
                CheckStatus::Error,
                Some(format!("Failed to open DB snapshot for inspection: {err}")),
                Some(serde_json::json!({ "path": db_path.display().to_string() })),
            );
            check_sqlite_cli_integrity(db_path, checks);
        }
    }
}

/// Execute the doctor command.
///
/// # Errors
///
/// Returns an error if report serialization fails or if IO operations fail.
#[allow(clippy::too_many_lines)]
pub fn execute(args: &DoctorArgs, cli: &config::CliOverrides, ctx: &OutputContext) -> Result<()> {
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
        std::process::exit(1);
    };

    let paths = match config::resolve_paths(&beads_dir, cli.db.as_ref()) {
        Ok(paths) => paths,
        Err(err) => {
            let mut checks = Vec::new();
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
            std::process::exit(1);
        }
    };

    let mut initial = collect_doctor_report(&beads_dir, &paths)?;

    // Auto-fix root .gitignore if --repair is passed and the warning is present.
    let gitignore_repaired = if args.repair {
        let repaired = fix_root_gitignore_if_warned(&beads_dir, &initial.report, ctx);
        if repaired {
            initial = collect_doctor_report(&beads_dir, &paths)?;
        }
        repaired
    } else {
        false
    };

    if !args.repair {
        print_report(&initial.report, ctx)?;
        if !initial.report.ok {
            std::process::exit(1);
        }
        return Ok(());
    }

    let mut local_repair = LocalRepairResult::default();

    if initial.report.ok {
        let has_blocked_cache_rebuild = report_has_blocked_cache_rebuild_finding(&initial.report);
        let has_partial_index_warnings = report_has_partial_index_warnings(&initial.report);
        let has_warn_page_anomalies = report_has_warn_level_page_anomaly(&initial.report);

        // Even when there are no errors, planned deferred cache rebuilds and
        // integrity warnings can be repaired. Run those local repairs when
        // --repair is passed and the warnings are present.
        if has_blocked_cache_rebuild || has_partial_index_warnings || has_warn_page_anomalies {
            local_repair = if has_blocked_cache_rebuild {
                repair_recoverable_db_state(&beads_dir, &paths.db_path, &initial.report)
            } else {
                LocalRepairResult::default()
            };

            if has_partial_index_warnings {
                repair_partial_indexes(&paths.db_path, &mut local_repair);
            }

            if has_warn_page_anomalies {
                repair_via_vacuum(&paths.db_path, &mut local_repair);
            }

            let post_warning_repair = collect_doctor_report(&beads_dir, &paths)?;
            let verified = warning_repair_verified(
                &post_warning_repair.report,
                has_blocked_cache_rebuild,
                has_partial_index_warnings,
            );
            let repair_message = repair_outcome_message(
                gitignore_repaired,
                Some(&local_repair),
                has_partial_index_warnings.then_some(REINDEX_INCOMPLETE_MESSAGE),
            );
            let recovery_audit = local_repair_audit_record(
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
            );
            emit_recovery_audit_record(&recovery_audit);
            if verified {
                if ctx.is_json() {
                    ctx.json(&serde_json::json!({
                        "report": initial.report,
                        "repaired": gitignore_repaired || local_repair.applied(),
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
            let recovery_audit = no_op_repair_audit_record(gitignore_repaired);
            emit_recovery_audit_record(&recovery_audit);
            if ctx.is_json() {
                ctx.json(&serde_json::json!({
                    "report": initial.report,
                    "repaired": gitignore_repaired,
                    "recovery_audit": recovery_audit,
                    "message": repair_outcome_message(gitignore_repaired, None, None)
                }));
            } else {
                print_report(&initial.report, ctx)?;
                ctx.info(&repair_outcome_message(gitignore_repaired, None, None));
            }
            return Ok(());
        }
    }

    if !local_repair.applied()
        && (report_has_blocked_cache_rebuild_finding(&initial.report)
            || report_has_sidecar_anomaly(&initial.report))
    {
        local_repair = repair_recoverable_db_state(&beads_dir, &paths.db_path, &initial.report);
    }

    // Also attempt REINDEX if partial-index warnings are present alongside errors.
    if !local_repair.indexes_reindexed && report_has_partial_index_warnings(&initial.report) {
        repair_partial_indexes(&paths.db_path, &mut local_repair);
    }

    // VACUUM to fix page-level anomalies (free space corruption, malformed
    // B-tree pages) caused by frankensqlite's B-tree layer differences with
    // C sqlite3 (#237, #245).  VACUUM rewrites every page from scratch, so
    // it fixes both index and table corruption.
    if report_has_page_corruption(&initial.report) {
        repair_via_vacuum(&paths.db_path, &mut local_repair);
    }

    let mut after_local_repair = if local_repair.applied() {
        collect_doctor_report(&beads_dir, &paths)?
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
    if !local_repair.vacuumed && report_has_warn_level_page_anomaly(&after_local_repair.report) {
        tracing::info!(
            path = %paths.db_path.display(),
            "Post-repair report has WARN-level page anomalies; running VACUUM to clean up orphaned pages"
        );
        repair_via_vacuum(&paths.db_path, &mut local_repair);
        if local_repair.vacuumed {
            after_local_repair = collect_doctor_report(&beads_dir, &paths)?;
        }
    }

    if repair_report_verified(&after_local_repair.report) {
        // Issue #245: REINDEX can fix index ordering so integrity_check
        // passes, but the underlying B-tree corruption may still cause
        // writes to fail (reads work, writes get ISSUE_NOT_FOUND).  Run a
        // write probe to confirm that the DB is truly healthy before
        // declaring success.
        let write_probe_ok = write_probe_after_repair(&paths.db_path);
        if !write_probe_ok {
            let recovery_audit = local_repair_audit_record(
                "doctor.local_repair",
                "write_probe_failed",
                &local_repair,
                Some("rollback-only write probe failed after local repair".to_string()),
            );
            emit_recovery_audit_record(&recovery_audit);
            tracing::warn!(
                "Post-repair write probe failed — local repair insufficient, \
                 falling through to full JSONL rebuild"
            );
            // Don't return early — fall through to JSONL rebuild below.
        } else {
            let repair_message =
                repair_outcome_message(gitignore_repaired, Some(&local_repair), None);
            let recovery_audit =
                local_repair_audit_record("doctor.local_repair", "verified", &local_repair, None);
            emit_recovery_audit_record(&recovery_audit);
            if ctx.is_json() {
                ctx.json(&serde_json::json!({
                    "report": initial.report,
                    "repaired": gitignore_repaired || local_repair.applied(),
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
        let recovery_audit = local_repair_audit_record(
            "doctor.local_repair",
            "needs_jsonl_rebuild",
            &local_repair,
            Some(reason.to_string()),
        );
        emit_recovery_audit_record(&recovery_audit);
    }

    let Some(jsonl_path) = initial.jsonl_path.as_ref() else {
        let recovery_audit = jsonl_rebuild_audit_record(
            "doctor.jsonl_rebuild",
            "refused",
            None,
            Some("no JSONL file found to rebuild from".to_string()),
        );
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
        let recovery_audit = jsonl_rebuild_audit_record(
            "doctor.jsonl_rebuild",
            "refused",
            None,
            Some(reason.clone()),
        );
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
    ) {
        Ok(result) => result,
        Err(err) => {
            let outcome = jsonl_rebuild_failure_outcome(&err);
            let recovery_audit = jsonl_rebuild_audit_record(
                "doctor.jsonl_rebuild",
                outcome,
                None,
                Some(err.to_string()),
            );
            emit_recovery_audit_record(&recovery_audit);
            if outcome == "refused" {
                return Err(err);
            }
            return Err(BeadsError::Config(format!(
                "Repair import failed: {err}. \
             The JSONL file may be corrupt. \
             Try manually editing the JSONL to fix invalid records."
            )));
        }
    };

    let post_repair = collect_doctor_report(&beads_dir, &paths)?;
    let post_repair_verified = repair_report_verified(&post_repair.report);
    let verification_failure_marker = if post_repair_verified {
        None
    } else {
        Some(write_jsonl_rebuild_verification_failed_marker(
            &beads_dir,
            &paths.db_path,
            &post_repair,
            &repair_result,
        )?)
    };
    let verification_failure_reason = verification_failure_marker.as_ref().map(|path| {
        format!(
            "post-repair verification failed; evidence marker written to '{}'",
            path.display()
        )
    });
    let recovery_audit = jsonl_rebuild_audit_record(
        "doctor.jsonl_rebuild",
        if post_repair_verified {
            "verified"
        } else {
            "verification_failed"
        },
        Some(&repair_result),
        verification_failure_reason.clone(),
    );
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::{AnomalyClass, WorkspaceHealth};
    use crate::model::{Issue, IssueType, Priority, Status};
    use crate::storage::SqliteStorage;
    use chrono::Utc;
    use fsqlite::Connection;
    use std::fs;
    use tempfile::{NamedTempFile, TempDir};

    fn find_check<'a>(checks: &'a [CheckResult], name: &str) -> Option<&'a CheckResult> {
        checks.iter().find(|check| check.name == name)
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
            labels: Vec::new(),
            dependencies: Vec::new(),
            comments: Vec::new(),
        }
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
            verified_backups: Vec::new(),
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
            verified_backups: Vec::new(),
        };

        let marker = write_jsonl_rebuild_verification_failed_marker(
            &beads_dir,
            &db_path,
            &post_repair,
            &repair,
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
            &ctx
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

        assert!(!fix_root_gitignore_if_warned(&beads_dir, &report, &ctx));
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
        let message = repair_outcome_message(
            true,
            Some(&LocalRepairResult::default()),
            Some(REINDEX_INCOMPLETE_MESSAGE),
        );

        assert!(message.contains(ROOT_GITIGNORE_REPAIR_MESSAGE));
        assert!(message.contains(REINDEX_INCOMPLETE_MESSAGE));
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

        let repair = repair_recoverable_db_state(temp.path(), &db_path, &report);

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

        let repair = repair_recoverable_db_state(temp.path(), &db_path, &report);

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
    fn test_check_database_sidecars_warns_on_wal_without_shm() -> Result<()> {
        // frankensqlite does not create SHM files — WAL without SHM is expected and should
        // produce a Warn (informational) rather than an Error that triggers repair.
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
            matches!(check.status, CheckStatus::Warn),
            "expected Warn for WAL-without-SHM, got {:?}",
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

        let repair = repair_recoverable_db_state(&beads_dir, &db_path, &report);
        assert!(
            !repair.quarantined_artifacts.is_empty(),
            "expected local repair to quarantine the orphan SHM sidecar"
        );

        let recovery_dir = config::recovery_dir_for_db_path(&db_path, &beads_dir);
        let backups: Vec<_> = fs::read_dir(&recovery_dir)?
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            backups
                .iter()
                .any(|name| name.starts_with("beads.db-shm.")
                    && name.ends_with(".doctor-quarantine")),
            "expected quarantined SHM backup in recovery dir: {backups:?}"
        );
        Ok(())
    }

    #[test]
    fn test_repair_recoverable_db_state_skips_repair_for_wal_without_shm_warn() -> Result<()> {
        // WAL-without-SHM is now a Warn (not Error) for frankensqlite compatibility.
        // repair_recoverable_db_state should NOT attempt any repair for this condition.
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir)?;
        let db_path = beads_dir.join("beads.db");
        fs::write(&db_path, b"not a sqlite database")?;
        let wal_path = PathBuf::from(format!("{}-wal", db_path.to_string_lossy()));
        fs::write(&wal_path, b"frankensqlite wal without shm")?;

        let report = DoctorReport {
            ok: true, // Warn-only report is considered ok
            workspace_health: None,
            reliability_audit: None,
            checks: vec![CheckResult {
                name: "db.sidecars".to_string(),
                status: CheckStatus::Warn,
                message: Some(
                    "WAL sidecar exists without a matching SHM sidecar (expected for frankensqlite)"
                        .to_string(),
                ),
                details: None,
            }],
        };

        let repair = repair_recoverable_db_state(&beads_dir, &db_path, &report);
        assert!(
            repair.quarantined_artifacts.is_empty(),
            "WAL should not be quarantined for a Warn-level sidecar check"
        );
        assert!(
            wal_path.exists(),
            "WAL file should remain untouched when sidecar check is only a warning"
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
        inspect_existing_doctor_database(&db_path, None, JsonlCountState::Missing, &mut checks);

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

        let local_repair = repair_recoverable_db_state(&beads_dir, &db_path, &report);
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
}
