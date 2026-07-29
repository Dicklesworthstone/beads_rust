use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

const ORPHANED_LOCK_FILE_STALE_AFTER: Duration = Duration::from_secs(30 * 60);
const CONFLICT_MARKER_PREFIX_LEN: usize = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum WorkspaceHealth {
    Healthy,
    Degraded,
    Recoverable,
    Unsafe,
}

impl WorkspaceHealth {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Recoverable => "recoverable",
            Self::Unsafe => "unsafe",
        }
    }

    #[must_use]
    pub fn is_operable(self) -> bool {
        matches!(self, Self::Healthy | Self::Degraded)
    }

    #[must_use]
    pub fn needs_recovery(self) -> bool {
        matches!(self, Self::Recoverable)
    }

    #[must_use]
    pub fn is_fatal(self) -> bool {
        matches!(self, Self::Unsafe)
    }
}

impl fmt::Display for WorkspaceHealth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AnomalyClass {
    DatabaseMissing,
    DatabaseNotSqlite,
    DatabaseCorrupt {
        detail: String,
    },
    WalCorrupt,
    SidecarMismatch {
        has_wal: bool,
        has_shm: bool,
    },
    TruncatedWal,
    DuplicateSchemaRows {
        name: String,
        count: i64,
    },
    DuplicateConfigKeys {
        key: String,
        count: i64,
    },
    DuplicateMetadataKeys {
        key: String,
        count: i64,
    },
    JsonlParseError {
        detail: String,
    },
    JsonlConflictMarkers,
    DbJsonlCountMismatch {
        db_count: usize,
        jsonl_count: usize,
    },
    /// Issue ID set differs between DB and JSONL. Emitted in addition to
    /// (or instead of) `DbJsonlCountMismatch` when we have enough
    /// information to enumerate which ids live on which side. Both
    /// stores can have equal cardinality while still disagreeing on
    /// which specific issues exist — the count-only check passes in
    /// that case, the set check does not. See beads_rust#286.
    DbJsonlIdSetMismatch {
        only_db_count: usize,
        only_jsonl_count: usize,
        only_db: Vec<String>,
        only_jsonl: Vec<String>,
        both_count: usize,
    },
    JsonlNewer,
    DbNewer,
    StaleRecoveryArtifacts,
    BlockedCacheStale,
    NullInNotNullColumn {
        table: String,
        column: String,
    },
    DirtyFlagMismatch {
        flag: String,
        expected: bool,
        actual: bool,
    },
    BlockedCacheContentMismatch,
    ReadyProjectionContentMismatch,
    ExportHashMismatch {
        db_hash: String,
        jsonl_hash: String,
    },
    ChildCountDrift {
        issue_id: String,
        stored: i64,
        actual: i64,
    },
    WriteProbeFailed {
        detail: String,
    },
    JournalSidecarPresent,
    OrphanedLockFile,
}

impl AnomalyClass {
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::DatabaseMissing => "database_missing",
            Self::DatabaseNotSqlite => "database_not_sqlite",
            Self::DatabaseCorrupt { .. } => "database_corrupt",
            Self::WalCorrupt => "wal_corrupt",
            Self::SidecarMismatch { .. } => "sidecar_mismatch",
            Self::TruncatedWal => "truncated_wal",
            Self::DuplicateSchemaRows { .. } => "duplicate_schema_rows",
            Self::DuplicateConfigKeys { .. } => "duplicate_config_keys",
            Self::DuplicateMetadataKeys { .. } => "duplicate_metadata_keys",
            Self::JsonlParseError { .. } => "jsonl_parse_error",
            Self::JsonlConflictMarkers => "jsonl_conflict_markers",
            Self::DbJsonlCountMismatch { .. } => "db_jsonl_count_mismatch",
            Self::DbJsonlIdSetMismatch { .. } => "db_jsonl_id_set_mismatch",
            Self::JsonlNewer => "jsonl_newer",
            Self::DbNewer => "db_newer",
            Self::StaleRecoveryArtifacts => "stale_recovery_artifacts",
            Self::BlockedCacheStale => "blocked_cache_stale",
            Self::NullInNotNullColumn { .. } => "null_in_not_null_column",
            Self::DirtyFlagMismatch { .. } => "dirty_flag_mismatch",
            Self::BlockedCacheContentMismatch => "blocked_cache_content_mismatch",
            Self::ReadyProjectionContentMismatch => "ready_projection_content_mismatch",
            Self::ExportHashMismatch { .. } => "export_hash_mismatch",
            Self::ChildCountDrift { .. } => "child_count_drift",
            Self::WriteProbeFailed { .. } => "write_probe_failed",
            Self::JournalSidecarPresent => "journal_sidecar_present",
            Self::OrphanedLockFile => "orphaned_lock_file",
        }
    }

    #[must_use]
    pub fn severity(&self) -> WorkspaceHealth {
        match self {
            Self::DatabaseNotSqlite
            | Self::DatabaseCorrupt { .. }
            | Self::WalCorrupt
            | Self::DatabaseMissing
            | Self::DuplicateSchemaRows { .. }
            | Self::DuplicateConfigKeys { .. }
            | Self::DuplicateMetadataKeys { .. }
            | Self::TruncatedWal
            | Self::WriteProbeFailed { .. } => WorkspaceHealth::Recoverable,

            Self::JsonlConflictMarkers | Self::JsonlParseError { .. } => WorkspaceHealth::Unsafe,

            Self::SidecarMismatch { .. }
            | Self::DbJsonlCountMismatch { .. }
            | Self::DbJsonlIdSetMismatch { .. }
            | Self::JsonlNewer
            | Self::DbNewer
            | Self::StaleRecoveryArtifacts
            | Self::BlockedCacheStale
            | Self::NullInNotNullColumn { .. }
            | Self::DirtyFlagMismatch { .. }
            | Self::BlockedCacheContentMismatch
            | Self::ReadyProjectionContentMismatch
            | Self::ExportHashMismatch { .. }
            | Self::ChildCountDrift { .. }
            | Self::JournalSidecarPresent
            | Self::OrphanedLockFile => WorkspaceHealth::Degraded,
        }
    }

    #[must_use]
    pub fn audit_entry(&self) -> AnomalyAuditEntry {
        AnomalyAuditEntry {
            code: self.code().to_string(),
            severity: self.severity().as_str().to_string(),
            message: self.to_string(),
        }
    }
}

impl fmt::Display for AnomalyClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DatabaseMissing => f.write_str("database file missing"),
            Self::DatabaseNotSqlite => f.write_str("database file is not SQLite"),
            Self::DatabaseCorrupt { detail } => write!(f, "database corrupt: {detail}"),
            Self::WalCorrupt => f.write_str("WAL file corrupt"),
            Self::SidecarMismatch { has_wal, has_shm } => {
                write!(f, "sidecar mismatch (WAL={has_wal}, SHM={has_shm})")
            }
            Self::TruncatedWal => f.write_str("truncated WAL sidecar (<32 bytes)"),
            Self::DuplicateSchemaRows { name, count } => {
                write!(
                    f,
                    "duplicate sqlite_master entries for '{name}' ({count} rows)"
                )
            }
            Self::DuplicateConfigKeys { key, count } => {
                write!(f, "duplicate config rows for key '{key}' ({count} rows)")
            }
            Self::DuplicateMetadataKeys { key, count } => {
                write!(f, "duplicate metadata rows for key '{key}' ({count} rows)")
            }
            Self::JsonlParseError { detail } => write!(f, "JSONL parse error: {detail}"),
            Self::JsonlConflictMarkers => f.write_str("JSONL contains merge conflict markers"),
            Self::DbJsonlCountMismatch {
                db_count,
                jsonl_count,
            } => {
                write!(
                    f,
                    "DB/JSONL count mismatch (db={db_count}, jsonl={jsonl_count})"
                )
            }
            Self::DbJsonlIdSetMismatch {
                only_db_count,
                only_jsonl_count,
                only_db,
                only_jsonl,
                both_count,
            } => fmt_db_jsonl_id_set_mismatch(
                f,
                *only_db_count,
                *only_jsonl_count,
                only_db,
                only_jsonl,
                *both_count,
            ),
            Self::JsonlNewer => f.write_str("JSONL has newer data than database"),
            Self::DbNewer => f.write_str("database has newer data than JSONL"),
            Self::StaleRecoveryArtifacts => f.write_str("stale recovery artifacts present"),
            Self::BlockedCacheStale => f.write_str("blocked_issues_cache marked stale"),
            Self::NullInNotNullColumn { table, column } => {
                write!(f, "NULL in NOT NULL column {table}.{column}")
            }
            Self::DirtyFlagMismatch {
                flag,
                expected,
                actual,
            } => {
                write!(
                    f,
                    "dirty flag '{flag}' mismatch (expected={expected}, actual={actual})"
                )
            }
            Self::BlockedCacheContentMismatch => {
                f.write_str("blocked_issues_cache content differs from dependency graph")
            }
            Self::ReadyProjectionContentMismatch => {
                f.write_str("ready projection content differs from direct dependency graph")
            }
            Self::ExportHashMismatch {
                db_hash,
                jsonl_hash,
            } => {
                write!(f, "export hash mismatch (db={db_hash}, jsonl={jsonl_hash})")
            }
            Self::ChildCountDrift {
                issue_id,
                stored,
                actual,
            } => {
                write!(
                    f,
                    "child_count drift for '{issue_id}' (stored={stored}, actual={actual})"
                )
            }
            Self::WriteProbeFailed { detail } => write!(f, "write probe failed: {detail}"),
            Self::JournalSidecarPresent => {
                f.write_str("journal sidecar present (incomplete transaction)")
            }
            Self::OrphanedLockFile => f.write_str("orphaned lock file (.beads.lock) present"),
        }
    }
}

fn fmt_db_jsonl_id_set_mismatch(
    f: &mut fmt::Formatter<'_>,
    only_db_count: usize,
    only_jsonl_count: usize,
    only_db: &[String],
    only_jsonl: &[String],
    both_count: usize,
) -> fmt::Result {
    write!(
        f,
        "DB/JSONL id-set mismatch (only_db={}: [{}]; only_jsonl={}: [{}]; both={both_count})",
        only_db_count,
        preview_anomaly_ids(only_db),
        only_jsonl_count,
        preview_anomaly_ids(only_jsonl),
    )
}

fn preview_anomaly_ids(ids: &[String]) -> String {
    const LIMIT: usize = 3;
    if ids.len() <= LIMIT {
        ids.join(", ")
    } else {
        format!(
            "{}, … (+{} more)",
            ids[..LIMIT].join(", "),
            ids.len() - LIMIT
        )
    }
}

#[derive(Debug, Clone)]
pub struct WorkspaceClassification {
    pub health: WorkspaceHealth,
    pub anomalies: Vec<AnomalyClass>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct AnomalyAuditEntry {
    pub code: String,
    pub severity: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ReliabilityAuditRecord {
    pub source: String,
    pub health: String,
    pub anomaly_count: usize,
    pub anomalies: Vec<AnomalyAuditEntry>,
}

impl ReliabilityAuditRecord {
    #[must_use]
    pub fn anomaly_codes_csv(&self) -> String {
        self.anomalies
            .iter()
            .map(|entry| entry.code.as_str())
            .collect::<Vec<_>>()
            .join(",")
    }

    pub fn emit_tracing(&self, phase: &str, outcome: &str) {
        let anomaly_codes = self.anomaly_codes_csv();
        tracing::info!(
            target: "br::reliability",
            source = %self.source,
            phase,
            outcome,
            workspace_health = %self.health,
            anomaly_count = self.anomaly_count,
            anomaly_codes = %anomaly_codes,
            "reliability audit record"
        );
    }
}

impl WorkspaceClassification {
    #[must_use]
    pub fn healthy() -> Self {
        Self {
            health: WorkspaceHealth::Healthy,
            anomalies: Vec::new(),
        }
    }

    #[must_use]
    pub fn from_anomalies(anomalies: Vec<AnomalyClass>) -> Self {
        let health = anomalies
            .iter()
            .map(AnomalyClass::severity)
            .max()
            .unwrap_or(WorkspaceHealth::Healthy);
        Self { health, anomalies }
    }

    #[must_use]
    pub fn is_operable(&self) -> bool {
        self.health.is_operable()
    }

    #[must_use]
    pub fn needs_recovery(&self) -> bool {
        self.health.needs_recovery()
    }

    #[must_use]
    pub fn recovery_possible(&self) -> bool {
        !matches!(self.health, WorkspaceHealth::Unsafe)
    }

    #[must_use]
    pub fn audit_record(&self, source: impl Into<String>) -> ReliabilityAuditRecord {
        let anomalies = self
            .anomalies
            .iter()
            .map(AnomalyClass::audit_entry)
            .collect::<Vec<_>>();
        ReliabilityAuditRecord {
            source: source.into(),
            health: self.health.as_str().to_string(),
            anomaly_count: anomalies.len(),
            anomalies,
        }
    }
}

impl fmt::Display for WorkspaceClassification {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.health)?;
        if !self.anomalies.is_empty() {
            write!(f, " ({} anomalies)", self.anomalies.len())?;
        }
        Ok(())
    }
}

#[must_use]
fn sqlite_sidecar_path(db_path: &Path, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}{}", db_path.to_string_lossy(), suffix))
}

#[must_use]
pub fn classify_file_state(db_path: &Path, jsonl_path: &Path) -> Vec<AnomalyClass> {
    let mut anomalies = Vec::new();

    if !db_path.is_file() && jsonl_path.is_file() {
        anomalies.push(AnomalyClass::DatabaseMissing);
    }

    if db_path.is_file()
        && let Ok(mut file) = std::fs::File::open(db_path)
    {
        use std::io::Read;
        let mut header = [0u8; 16];
        if file.read_exact(&mut header).is_err() || &header != b"SQLite format 3\0" {
            anomalies.push(AnomalyClass::DatabaseNotSqlite);
        }
    }

    let wal_path = sqlite_sidecar_path(db_path, "-wal");
    let shm_path = sqlite_sidecar_path(db_path, "-shm");
    let has_wal = wal_path.is_file();
    let has_shm = shm_path.is_file();

    if has_shm && !has_wal {
        anomalies.push(AnomalyClass::SidecarMismatch { has_wal, has_shm });
    }

    // A WAL header shorter than 32 bytes is a partial write — except for the
    // single value 0. A 0-byte WAL is the documented resting state after a
    // successful `PRAGMA wal_checkpoint(TRUNCATE)` with no concurrent readers,
    // which `SqliteStorage::Drop` runs on every mutating br invocation. Treating
    // it as `TruncatedWal` would false-alarm a healthy store into `Recoverable`,
    // so floor the heuristic at `> 0` — the health-path counterpart of the same
    // `> 0` floor the recovery path's `quarantine_truncated_wal_sidecar`
    // already carries (#291). Partial headers in `(0, 32)` still classify.
    if has_wal
        && let Ok(meta) = std::fs::metadata(&wal_path)
        && meta.len() > 0
        && meta.len() < 32
    {
        anomalies.push(AnomalyClass::TruncatedWal);
    }

    if jsonl_path.is_file() && jsonl_has_conflict_markers(jsonl_path) {
        anomalies.push(AnomalyClass::JsonlConflictMarkers);
    }

    let journal_path = sqlite_sidecar_path(db_path, "-journal");
    if journal_path.is_file() {
        anomalies.push(AnomalyClass::JournalSidecarPresent);
    }

    let lock_path = db_path
        .parent()
        .map(|p| p.join(".beads.lock"))
        .unwrap_or_else(|| db_path.with_file_name(".beads.lock"));
    if lock_path.is_file() && is_orphaned_lock_file(&lock_path, SystemTime::now()) {
        anomalies.push(AnomalyClass::OrphanedLockFile);
    }

    anomalies
}

/// Returns true when any line of `path` starts with a git conflict
/// marker (`<<<<<<<`, `=======`, `>>>>>>>`, or `|||||||`). Reads the
/// file as raw bytes so non-UTF-8 content cannot hide markers; any
/// open/read failure conservatively reports `false` (absence of
/// evidence, not evidence of corruption).
#[must_use]
pub fn jsonl_has_conflict_markers(path: &Path) -> bool {
    use std::io::BufRead as _;

    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut reader = std::io::BufReader::with_capacity(64 * 1024, file);
    let mut prefix = [0_u8; CONFLICT_MARKER_PREFIX_LEN];
    let mut prefix_len = 0_usize;
    let mut reading_prefix = true;

    loop {
        let buffer = match reader.fill_buf() {
            Ok([]) | Err(_) => return false,
            Ok(buffer) => buffer,
        };

        let mut consumed = 0;
        for &byte in buffer {
            consumed += 1;

            if reading_prefix && byte != b'\n' {
                if let Some(slot) = prefix.get_mut(prefix_len) {
                    *slot = byte;
                    prefix_len += 1;
                }
                if prefix_len == CONFLICT_MARKER_PREFIX_LEN {
                    if is_jsonl_conflict_marker_prefix(prefix) {
                        return true;
                    }
                    reading_prefix = false;
                }
            }

            if byte == b'\n' {
                prefix_len = 0;
                reading_prefix = true;
            }
        }

        reader.consume(consumed);
    }
}

fn is_jsonl_conflict_marker_prefix(prefix: [u8; CONFLICT_MARKER_PREFIX_LEN]) -> bool {
    prefix == *b"<<<<<<<" || prefix == *b">>>>>>>" || prefix == *b"=======" || prefix == *b"|||||||"
}

fn is_orphaned_lock_file(lock_path: &Path, now: SystemTime) -> bool {
    std::fs::metadata(lock_path)
        .ok()
        .and_then(|metadata| metadata.modified().ok())
        .is_some_and(|modified| lock_modified_time_is_stale(modified, now))
}

fn lock_modified_time_is_stale(modified: SystemTime, now: SystemTime) -> bool {
    matches!(
        now.duration_since(modified),
        Ok(age) if age >= ORPHANED_LOCK_FILE_STALE_AFTER
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::TempDir;

    fn setup_workspace() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("beads.db");
        let jsonl_path = dir.path().join("issues.jsonl");
        (dir, db_path, jsonl_path)
    }

    #[test]
    fn healthy_workspace_has_no_anomalies() {
        let (_dir, db_path, jsonl_path) = setup_workspace();
        let mut f = std::fs::File::create(&db_path).unwrap();
        f.write_all(b"SQLite format 3\0").unwrap();
        f.write_all(&[0u8; 100]).unwrap();
        std::fs::write(&jsonl_path, "{\"id\":\"test-1\"}\n").unwrap();

        let anomalies = classify_file_state(&db_path, &jsonl_path);
        assert!(anomalies.is_empty());
        let classification = WorkspaceClassification::from_anomalies(anomalies);
        assert_eq!(classification.health, WorkspaceHealth::Healthy);
        assert!(classification.is_operable());
    }

    #[test]
    fn missing_db_with_jsonl_is_recoverable() {
        let (_dir, db_path, jsonl_path) = setup_workspace();
        std::fs::write(&jsonl_path, "{\"id\":\"test-1\"}\n").unwrap();

        let anomalies = classify_file_state(&db_path, &jsonl_path);
        assert_eq!(anomalies.len(), 1);
        assert!(matches!(anomalies[0], AnomalyClass::DatabaseMissing));
        let classification = WorkspaceClassification::from_anomalies(anomalies);
        assert_eq!(classification.health, WorkspaceHealth::Recoverable);
        assert!(classification.recovery_possible());
    }

    #[test]
    fn non_sqlite_db_is_recoverable() {
        let (_dir, db_path, jsonl_path) = setup_workspace();
        std::fs::write(&db_path, "this is not a sqlite file").unwrap();
        std::fs::write(&jsonl_path, "{\"id\":\"test-1\"}\n").unwrap();

        let anomalies = classify_file_state(&db_path, &jsonl_path);
        assert!(
            anomalies
                .iter()
                .any(|a| matches!(a, AnomalyClass::DatabaseNotSqlite))
        );
        let classification = WorkspaceClassification::from_anomalies(anomalies);
        assert_eq!(classification.health, WorkspaceHealth::Recoverable);
    }

    #[test]
    fn conflict_markers_in_jsonl_is_unsafe() {
        let (_dir, db_path, jsonl_path) = setup_workspace();
        let mut f = std::fs::File::create(&db_path).unwrap();
        f.write_all(b"SQLite format 3\0").unwrap();
        f.write_all(&[0u8; 100]).unwrap();
        std::fs::write(
            &jsonl_path,
            "<<<<<<< HEAD\n{\"id\":\"a\"}\n=======\n{\"id\":\"b\"}\n>>>>>>> branch\n",
        )
        .unwrap();

        let anomalies = classify_file_state(&db_path, &jsonl_path);
        assert!(
            anomalies
                .iter()
                .any(|a| matches!(a, AnomalyClass::JsonlConflictMarkers))
        );
        let classification = WorkspaceClassification::from_anomalies(anomalies);
        assert_eq!(classification.health, WorkspaceHealth::Unsafe);
        assert!(!classification.recovery_possible());
    }

    #[test]
    fn diff3_style_conflict_markers_are_detected() {
        let (_dir, db_path, jsonl_path) = setup_workspace();
        let mut f = std::fs::File::create(&db_path).unwrap();
        f.write_all(b"SQLite format 3\0").unwrap();
        f.write_all(&[0u8; 100]).unwrap();
        std::fs::write(
            &jsonl_path,
            "<<<<<<< HEAD\n{\"id\":\"a\"}\n||||||| merged common ancestors\n{\"id\":\"base\"}\n=======\n{\"id\":\"b\"}\n>>>>>>> branch\n",
        )
        .unwrap();

        let anomalies = classify_file_state(&db_path, &jsonl_path);
        assert!(
            anomalies
                .iter()
                .any(|a| matches!(a, AnomalyClass::JsonlConflictMarkers))
        );
    }

    #[test]
    fn conflict_markers_are_detected_in_non_utf8_jsonl() {
        let (_dir, db_path, jsonl_path) = setup_workspace();
        let mut f = std::fs::File::create(&db_path).unwrap();
        f.write_all(b"SQLite format 3\0").unwrap();
        f.write_all(&[0u8; 100]).unwrap();
        std::fs::write(&jsonl_path, b"{\"id\":\"a\"}\n\xff\n<<<<<<< HEAD\n").unwrap();

        let anomalies = classify_file_state(&db_path, &jsonl_path);
        assert!(
            anomalies
                .iter()
                .any(|a| matches!(a, AnomalyClass::JsonlConflictMarkers)),
            "non-UTF-8 bytes must not hide merge conflict markers: {anomalies:?}"
        );
    }

    #[test]
    fn tiny_db_file_below_sqlite_magic_is_flagged_as_not_sqlite() {
        let (_dir, db_path, jsonl_path) = setup_workspace();
        // Only 8 bytes — less than the 16-byte SQLite magic header.
        std::fs::write(&db_path, b"short").unwrap();
        std::fs::write(&jsonl_path, "{\"id\":\"test-1\"}\n").unwrap();

        let anomalies = classify_file_state(&db_path, &jsonl_path);
        assert!(
            anomalies
                .iter()
                .any(|a| matches!(a, AnomalyClass::DatabaseNotSqlite))
        );
    }

    #[test]
    fn wal_without_shm_is_expected_for_frankensqlite() {
        let (_dir, db_path, jsonl_path) = setup_workspace();
        let mut f = std::fs::File::create(&db_path).unwrap();
        f.write_all(b"SQLite format 3\0").unwrap();
        f.write_all(&[0u8; 100]).unwrap();
        std::fs::write(&jsonl_path, "{\"id\":\"test-1\"}\n").unwrap();
        let wal_path = db_path.with_extension("db-wal");
        std::fs::write(&wal_path, [0u8; 64]).unwrap();

        let anomalies = classify_file_state(&db_path, &jsonl_path);
        assert!(
            !anomalies
                .iter()
                .any(|a| matches!(a, AnomalyClass::SidecarMismatch { .. })),
            "WAL-without-SHM is expected for frankensqlite and should not be a sidecar mismatch: {anomalies:?}"
        );
        let classification = WorkspaceClassification::from_anomalies(anomalies);
        assert_eq!(classification.health, WorkspaceHealth::Healthy);
        assert!(classification.is_operable());
    }

    #[test]
    fn shm_without_wal_is_degraded_sidecar_mismatch() {
        let (_dir, db_path, jsonl_path) = setup_workspace();
        let mut f = std::fs::File::create(&db_path).unwrap();
        f.write_all(b"SQLite format 3\0").unwrap();
        f.write_all(&[0u8; 100]).unwrap();
        std::fs::write(&jsonl_path, "{\"id\":\"test-1\"}\n").unwrap();
        let shm_path = db_path.with_extension("db-shm");
        std::fs::write(&shm_path, [0u8; 64]).unwrap();

        let anomalies = classify_file_state(&db_path, &jsonl_path);
        assert!(
            anomalies.iter().any(|a| {
                matches!(
                    a,
                    AnomalyClass::SidecarMismatch {
                        has_wal: false,
                        has_shm: true
                    }
                )
            }),
            "SHM-without-WAL should be a sidecar mismatch: {anomalies:?}"
        );
        let classification = WorkspaceClassification::from_anomalies(anomalies);
        assert_eq!(classification.health, WorkspaceHealth::Degraded);
        assert!(classification.is_operable());
    }

    #[test]
    fn custom_db_filename_uses_sqlite_append_style_shm_path() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("issues.sqlite");
        let jsonl_path = dir.path().join("issues.jsonl");
        let mut f = std::fs::File::create(&db_path).unwrap();
        f.write_all(b"SQLite format 3\0").unwrap();
        f.write_all(&[0u8; 100]).unwrap();
        std::fs::write(&jsonl_path, "{\"id\":\"test-1\"}\n").unwrap();
        let shm_path = sqlite_sidecar_path(&db_path, "-shm");
        std::fs::write(&shm_path, [0u8; 64]).unwrap();

        let anomalies = classify_file_state(&db_path, &jsonl_path);
        assert!(
            anomalies.iter().any(|a| {
                matches!(
                    a,
                    AnomalyClass::SidecarMismatch {
                        has_wal: false,
                        has_shm: true
                    }
                )
            }),
            "custom DB filename SHM sidecar should be detected at {shm_path:?}: {anomalies:?}"
        );
    }

    #[test]
    fn custom_db_filename_uses_sqlite_append_style_wal_path() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("issues.sqlite");
        let jsonl_path = dir.path().join("issues.jsonl");
        let mut f = std::fs::File::create(&db_path).unwrap();
        f.write_all(b"SQLite format 3\0").unwrap();
        f.write_all(&[0u8; 100]).unwrap();
        std::fs::write(&jsonl_path, "{\"id\":\"test-1\"}\n").unwrap();
        let wal_path = sqlite_sidecar_path(&db_path, "-wal");
        std::fs::write(&wal_path, b"short wal").unwrap();

        let anomalies = classify_file_state(&db_path, &jsonl_path);
        assert!(
            anomalies
                .iter()
                .any(|a| matches!(a, AnomalyClass::TruncatedWal)),
            "custom DB filename WAL sidecar should be detected at {wal_path:?}: {anomalies:?}"
        );
    }

    #[test]
    fn zero_byte_wal_is_clean_checkpoint_state_not_truncated() {
        // #358 (health-path counterpart of #291): a 0-byte WAL is the documented
        // resting state after `PRAGMA wal_checkpoint(TRUNCATE)` (run by
        // `SqliteStorage::Drop` on every mutating br invocation), not corruption.
        // Classifying it as `TruncatedWal` would false-alarm a healthy store into
        // `Recoverable`. The `< 32` guard must carry a `> 0` floor, mirroring the
        // recovery path's `quarantine_truncated_wal_sidecar` (#291).
        let (_dir, db_path, jsonl_path) = setup_workspace();
        let mut f = std::fs::File::create(&db_path).unwrap();
        f.write_all(b"SQLite format 3\0").unwrap();
        f.write_all(&[0u8; 100]).unwrap();
        std::fs::write(&jsonl_path, "{\"id\":\"test-1\"}\n").unwrap();
        let wal_path = sqlite_sidecar_path(&db_path, "-wal");
        std::fs::write(&wal_path, b"").unwrap(); // 0 bytes — post-wal_checkpoint(TRUNCATE)

        let anomalies = classify_file_state(&db_path, &jsonl_path);
        assert!(
            !anomalies
                .iter()
                .any(|a| matches!(a, AnomalyClass::TruncatedWal)),
            "0-byte WAL is the clean post-checkpoint state, not corruption: {anomalies:?}"
        );
        assert_eq!(
            WorkspaceClassification::from_anomalies(anomalies).health,
            WorkspaceHealth::Healthy
        );
    }

    #[test]
    fn one_byte_wal_still_classifies_as_truncated() {
        // The `> 0` floor (#358) must not weaken the partial-write contract: a
        // WAL header with 1..32 bytes is a genuine truncation and still flags.
        let (_dir, db_path, jsonl_path) = setup_workspace();
        let mut f = std::fs::File::create(&db_path).unwrap();
        f.write_all(b"SQLite format 3\0").unwrap();
        f.write_all(&[0u8; 100]).unwrap();
        std::fs::write(&jsonl_path, "{\"id\":\"test-1\"}\n").unwrap();
        let wal_path = sqlite_sidecar_path(&db_path, "-wal");
        std::fs::write(&wal_path, [0u8; 1]).unwrap(); // 1 byte — partial write

        let anomalies = classify_file_state(&db_path, &jsonl_path);
        assert!(
            anomalies
                .iter()
                .any(|a| matches!(a, AnomalyClass::TruncatedWal)),
            "a 1-byte WAL is a partial write and must still classify as truncated: {anomalies:?}"
        );
    }

    #[test]
    fn classification_uses_worst_anomaly() {
        let anomalies = vec![
            AnomalyClass::SidecarMismatch {
                has_wal: true,
                has_shm: false,
            },
            AnomalyClass::JsonlConflictMarkers,
        ];
        let classification = WorkspaceClassification::from_anomalies(anomalies);
        assert_eq!(classification.health, WorkspaceHealth::Unsafe);
    }

    #[test]
    fn anomaly_audit_entry_has_stable_code_and_severity() {
        let anomaly = AnomalyClass::WriteProbeFailed {
            detail: "database disk image is malformed".to_string(),
        };
        let entry = anomaly.audit_entry();

        assert_eq!(entry.code, "write_probe_failed");
        assert_eq!(entry.severity, "recoverable");
        assert!(entry.message.contains("database disk image is malformed"));
    }

    #[test]
    fn workspace_classification_builds_reliability_audit_record() {
        let classification = WorkspaceClassification::from_anomalies(vec![
            AnomalyClass::DbJsonlCountMismatch {
                db_count: 3,
                jsonl_count: 2,
            },
            AnomalyClass::JsonlNewer,
        ]);

        let record = classification.audit_record("doctor.inspect");

        assert_eq!(record.source, "doctor.inspect");
        assert_eq!(record.health, "degraded");
        assert_eq!(record.anomaly_count, 2);
        assert_eq!(
            record.anomaly_codes_csv(),
            "db_jsonl_count_mismatch,jsonl_newer"
        );
    }

    #[test]
    fn anomaly_severity_ordering_is_correct() {
        assert!(WorkspaceHealth::Healthy < WorkspaceHealth::Degraded);
        assert!(WorkspaceHealth::Degraded < WorkspaceHealth::Recoverable);
        assert!(WorkspaceHealth::Recoverable < WorkspaceHealth::Unsafe);
    }

    #[test]
    fn journal_sidecar_detected() {
        let (_dir, db_path, jsonl_path) = setup_workspace();
        let mut f = std::fs::File::create(&db_path).unwrap();
        f.write_all(b"SQLite format 3\0").unwrap();
        f.write_all(&[0u8; 100]).unwrap();
        std::fs::write(&jsonl_path, "{\"id\":\"test-1\"}\n").unwrap();
        let journal_path = db_path.with_extension("db-journal");
        std::fs::write(&journal_path, b"journal data").unwrap();

        let anomalies = classify_file_state(&db_path, &jsonl_path);
        assert!(
            anomalies
                .iter()
                .any(|a| matches!(a, AnomalyClass::JournalSidecarPresent))
        );
        let c = WorkspaceClassification::from_anomalies(anomalies);
        assert_eq!(c.health, WorkspaceHealth::Degraded);
    }

    #[test]
    fn recent_lock_file_is_not_orphaned() {
        let (_dir, db_path, jsonl_path) = setup_workspace();
        let mut f = std::fs::File::create(&db_path).unwrap();
        f.write_all(b"SQLite format 3\0").unwrap();
        f.write_all(&[0u8; 100]).unwrap();
        std::fs::write(&jsonl_path, "{\"id\":\"test-1\"}\n").unwrap();
        let lock_path = db_path.parent().unwrap().join(".beads.lock");
        std::fs::write(&lock_path, "pid:12345").unwrap();

        let anomalies = classify_file_state(&db_path, &jsonl_path);
        assert!(
            !anomalies
                .iter()
                .any(|a| matches!(a, AnomalyClass::OrphanedLockFile))
        );
        let c = WorkspaceClassification::from_anomalies(anomalies);
        assert_eq!(c.health, WorkspaceHealth::Healthy);
    }

    #[test]
    fn stale_lock_modified_time_is_orphaned() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(60 * 60);
        let stale_modified = now
            .checked_sub(ORPHANED_LOCK_FILE_STALE_AFTER + Duration::from_secs(1))
            .unwrap();
        let recent_age = ORPHANED_LOCK_FILE_STALE_AFTER.saturating_sub(Duration::from_secs(1));
        let recent_modified = now.checked_sub(recent_age).unwrap();
        let future_modified = now + Duration::from_secs(1);

        assert!(lock_modified_time_is_stale(stale_modified, now));
        assert!(!lock_modified_time_is_stale(recent_modified, now));
        assert!(!lock_modified_time_is_stale(future_modified, now));
    }

    #[test]
    fn new_anomaly_classes_have_correct_severity() {
        assert_eq!(
            AnomalyClass::DirtyFlagMismatch {
                flag: "needs_flush".to_string(),
                expected: true,
                actual: false,
            }
            .severity(),
            WorkspaceHealth::Degraded
        );
        assert_eq!(
            AnomalyClass::ExportHashMismatch {
                db_hash: "abc".to_string(),
                jsonl_hash: "def".to_string(),
            }
            .severity(),
            WorkspaceHealth::Degraded
        );
        assert_eq!(
            AnomalyClass::ChildCountDrift {
                issue_id: "x-1".to_string(),
                stored: 3,
                actual: 2,
            }
            .severity(),
            WorkspaceHealth::Degraded
        );
        assert_eq!(
            AnomalyClass::JournalSidecarPresent.severity(),
            WorkspaceHealth::Degraded
        );
        assert_eq!(
            AnomalyClass::WriteProbeFailed {
                detail: "write failed".to_string(),
            }
            .severity(),
            WorkspaceHealth::Recoverable
        );
        assert_eq!(
            AnomalyClass::OrphanedLockFile.severity(),
            WorkspaceHealth::Degraded
        );
    }
}
