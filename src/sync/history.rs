//! Local history backup for JSONL exports.
//!
//! This module handles:
//! - Creating timestamped backups of `issues.jsonl` before export
//! - Rotating backups based on count and age
//! - Listing and restoring backups

use crate::error::{BeadsError, Result};
use crate::sync::path::validate_sync_path_with_external;
use crate::sync::{JsonlSourceSnapshot, capture_optional_jsonl_source};
use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;

/// Configuration for history backups.
#[derive(Debug, Clone)]
pub struct HistoryConfig {
    pub enabled: bool,
    pub max_count: usize,
    pub max_age_days: u32,
    /// Minimum interval, in seconds, between successive `.br_history` snapshots
    /// of the same target. Under bursty mutation, every `br` edit auto-flushes
    /// the JSONL and previously copied the whole file into a fresh timestamped
    /// backup each time — O(repo-size) write amplification per edit (#313).
    /// When the most recent backup is younger than this floor, the snapshot is
    /// skipped (the export itself still proceeds), collapsing edit bursts into
    /// at most one snapshot per interval while preserving recent-history
    /// granularity. `0` disables the throttle (snapshot on every export).
    pub min_interval_secs: u64,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_count: 100,
            max_age_days: 30,
            min_interval_secs: 5,
        }
    }
}

/// Backup entry metadata.
#[derive(Debug, Clone)]
pub struct BackupEntry {
    pub path: PathBuf,
    pub timestamp: DateTime<Utc>,
    pub size: u64,
    pub target_path: PathBuf,
    pub target_key: String,
}

struct BackupFileGuard {
    path: PathBuf,
    persist: bool,
}

impl BackupFileGuard {
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

impl Drop for BackupFileGuard {
    fn drop(&mut self) {
        if !self.persist {
            // Use remove_file directly and ignore NotFound to be TOCTOU-safe.
            if let Err(cleanup_err) = fs::remove_file(&self.path)
                && cleanup_err.kind() != io::ErrorKind::NotFound
            {
                tracing::warn!(
                    backup = %self.path.display(),
                    error = %cleanup_err,
                    "Failed to remove partially written history backup"
                );
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum BackupTarget {
    Relative { path: String },
    Absolute { path: String },
}

impl BackupTarget {
    fn from_target_path(beads_dir: &Path, target_path: &Path) -> Self {
        if let Ok(relative) = target_path.strip_prefix(beads_dir) {
            return Self::Relative {
                path: relative.to_string_lossy().into_owned(),
            };
        }

        Self::Absolute {
            path: target_path.to_string_lossy().into_owned(),
        }
    }

    fn resolve_path(&self, beads_dir: &Path) -> Result<PathBuf> {
        match self {
            Self::Relative { path } => {
                let relative_path = Path::new(path);
                if relative_path.is_absolute()
                    || relative_path.components().any(|component| {
                        matches!(
                            component,
                            Component::ParentDir | Component::Prefix(_) | Component::RootDir
                        )
                    })
                {
                    return Err(BeadsError::Config(format!(
                        "Invalid relative backup target metadata path: {path}"
                    )));
                }
                Ok(beads_dir.join(relative_path))
            }
            Self::Absolute { path } => Ok(PathBuf::from(path)),
        }
    }

    fn key(&self) -> String {
        match self {
            Self::Relative { path } => format!("relative:{path}"),
            Self::Absolute { path } => format!("absolute:{path}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct BackupMetadata {
    target: BackupTarget,
}

impl BackupMetadata {
    fn from_target_path(beads_dir: &Path, target_path: &Path) -> Self {
        Self {
            target: BackupTarget::from_target_path(beads_dir, target_path),
        }
    }
}

fn parse_backup_timestamp(ts_str: &str) -> Option<DateTime<Utc>> {
    for fmt in ["%Y%m%d_%H%M%S_%f", "%Y%m%d_%H%M%S"] {
        if let Ok(dt) = NaiveDateTime::parse_from_str(ts_str, fmt) {
            return Some(Utc.from_utc_datetime(&dt));
        }
    }

    None
}

static BACKUP_FILENAME_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?P<stem>.+?)\.(?P<ts>\d{8}_\d{6}(?:_\d{1,9})?)(\.\d+)?$")
        .expect("static regex compilation must not fail")
});

pub(crate) fn parse_backup_filename(filename: &str) -> Option<(String, DateTime<Utc>)> {
    let without_ext = filename.strip_suffix(".jsonl")?;

    // Pattern: <stem>.<timestamp>[.<collision_index>]
    // Timestamp formats: YYYYMMDD_HHMMSS or YYYYMMDD_HHMMSS_<fractional-seconds>
    // where the fractional component can be microsecond or nanosecond precision.
    let caps = BACKUP_FILENAME_REGEX.captures(without_ext)?;

    let stem = caps.name("stem")?.as_str().to_string();
    let timestamp_str = caps.name("ts")?.as_str();

    let timestamp = parse_backup_timestamp(timestamp_str)?;
    Some((stem, timestamp))
}

fn create_backup_file(history_dir: &Path, file_stem: &str) -> Result<(PathBuf, File)> {
    let timestamp = Utc::now().format("%Y%m%d_%H%M%S_%f").to_string();

    create_backup_file_for_timestamp(history_dir, file_stem, &timestamp)
}

fn create_backup_file_for_timestamp(
    history_dir: &Path,
    file_stem: &str,
    timestamp: &str,
) -> Result<(PathBuf, File)> {
    for collision_idx in 0..1024_u32 {
        let backup_name = if collision_idx == 0 {
            format!("{file_stem}.{timestamp}.jsonl")
        } else {
            format!("{file_stem}.{timestamp}.{collision_idx}.jsonl")
        };
        let backup_path = history_dir.join(backup_name);
        if history_artifact_metadata(&backup_metadata_path(&backup_path), "backup metadata")?
            .is_some()
        {
            continue;
        }

        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&backup_path)
        {
            Ok(file) => return Ok((backup_path, file)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {}
            Err(err) => return Err(err.into()),
        }
    }

    Err(BeadsError::Config(format!(
        "Failed to allocate unique backup path for {file_stem}"
    )))
}

fn backup_metadata_path(backup_path: &Path) -> PathBuf {
    backup_path.with_extension("jsonl.meta.json")
}

pub(crate) fn validate_history_dir_path(history_dir: &Path) -> Result<bool> {
    match fs::symlink_metadata(history_dir) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                return Err(BeadsError::Config(format!(
                    "History directory '{}' must not be a symlink",
                    history_dir.display()
                )));
            }
            if !file_type.is_dir() {
                return Err(BeadsError::Config(format!(
                    "History directory '{}' must be a directory",
                    history_dir.display()
                )));
            }
            Ok(true)
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(BeadsError::Io(err)),
    }
}

fn ensure_history_dir_path(history_dir: &Path) -> Result<()> {
    if validate_history_dir_path(history_dir)? {
        return Ok(());
    }

    fs::create_dir_all(history_dir).map_err(BeadsError::Io)?;

    if validate_history_dir_path(history_dir)? {
        Ok(())
    } else {
        Err(BeadsError::Config(format!(
            "Failed to create history directory '{}'",
            history_dir.display()
        )))
    }
}

fn history_artifact_metadata(path: &Path, label: &str) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                return Err(BeadsError::Config(format!(
                    "History {label} '{}' must not be a symlink",
                    path.display()
                )));
            }
            if !file_type.is_file() {
                return Err(BeadsError::Config(format!(
                    "History {label} '{}' must be a regular file",
                    path.display()
                )));
            }
            Ok(Some(metadata))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(BeadsError::Io(err)),
    }
}

fn remove_history_artifact_if_present(path: &Path, label: &str) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if file_type.is_symlink() || file_type.is_file() {
                if let Err(err) = fs::remove_file(path)
                    && err.kind() != io::ErrorKind::NotFound
                {
                    return Err(BeadsError::Io(err));
                }
                return Ok(());
            }
            Err(BeadsError::Config(format!(
                "History {label} '{}' must be a regular file",
                path.display()
            )))
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(BeadsError::Io(err)),
    }
}

fn remove_backup_artifacts(backup_path: &Path) -> Result<()> {
    remove_history_artifact_if_present(backup_path, "backup file")?;
    let metadata_path = backup_metadata_path(backup_path);
    remove_history_artifact_if_present(&metadata_path, "backup metadata")?;
    Ok(())
}

fn read_backup_metadata(backup_path: &Path) -> Result<Option<BackupMetadata>> {
    let metadata_path = backup_metadata_path(backup_path);
    if history_artifact_metadata(&metadata_path, "backup metadata")?.is_none() {
        return Ok(None);
    }

    let contents = fs::read(&metadata_path).map_err(BeadsError::Io)?;
    let metadata = serde_json::from_slice(&contents).map_err(|err| {
        BeadsError::Config(format!(
            "Failed to parse history backup metadata '{}': {err}",
            metadata_path.display()
        ))
    })?;
    Ok(Some(metadata))
}

fn write_backup_metadata(beads_dir: &Path, target_path: &Path, backup_path: &Path) -> Result<()> {
    let metadata = BackupMetadata::from_target_path(beads_dir, target_path);
    let contents = serde_json::to_vec(&metadata).map_err(|err| {
        BeadsError::Config(format!(
            "Failed to serialize history backup metadata for '{}': {err}",
            backup_path.display()
        ))
    })?;
    let metadata_path = backup_metadata_path(backup_path);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&metadata_path)
        .map_err(BeadsError::Io)?;

    let write_result = (|| -> Result<()> {
        file.write_all(&contents).map_err(BeadsError::Io)?;
        file.sync_all().map_err(BeadsError::Io)?;
        Ok(())
    })();

    drop(file);

    if let Err(err) = write_result {
        if let Err(cleanup_err) = fs::remove_file(&metadata_path) {
            tracing::warn!(
                metadata = %metadata_path.display(),
                cleanup_error = %cleanup_err,
                "Failed to remove partially written history metadata"
            );
        }
        return Err(err);
    }

    crate::util::sync_parent_directory(&metadata_path).map_err(BeadsError::Io)?;

    Ok(())
}

fn legacy_backup_target_path(beads_dir: &Path, backup_name: &str) -> Result<PathBuf> {
    let Some((stem, _timestamp)) = parse_backup_filename(backup_name) else {
        return Err(BeadsError::Config(format!(
            "Invalid backup filename format: {backup_name}"
        )));
    };

    Ok(beads_dir.join(format!("{stem}.jsonl")))
}

fn invalid_metadata_target_path(backup_name: &str) -> PathBuf {
    PathBuf::from(format!("<invalid metadata: {backup_name}>"))
}

fn backup_target_details(
    history_dir: &Path,
    backup_path: &Path,
    backup_name: &str,
) -> (PathBuf, String) {
    let Some(beads_dir) = history_dir.parent() else {
        let fallback = PathBuf::from(backup_name);
        return (fallback, format!("orphan-history:{backup_name}"));
    };

    match read_backup_metadata(backup_path) {
        Ok(Some(metadata)) => match metadata.target.resolve_path(beads_dir) {
            Ok(target_path) => (target_path, metadata.target.key()),
            Err(err) => {
                tracing::warn!(
                    backup = %backup_path.display(),
                    error = %err,
                    "Ignoring invalid history backup metadata during history rotation/listing"
                );
                (
                    invalid_metadata_target_path(backup_name),
                    format!("invalid-metadata:{backup_name}"),
                )
            }
        },
        Ok(None) => legacy_backup_target_path(beads_dir, backup_name).map_or_else(
            |_| {
                let fallback = PathBuf::from(backup_name);
                (fallback, format!("legacy-name:{backup_name}"))
            },
            |target_path| {
                let target_key = BackupMetadata::from_target_path(beads_dir, &target_path)
                    .target
                    .key();
                (target_path, target_key)
            },
        ),
        Err(err) => {
            tracing::warn!(
                backup = %backup_path.display(),
                error = %err,
                "Ignoring unreadable history backup metadata during history rotation/listing"
            );
            (
                invalid_metadata_target_path(backup_name),
                format!("invalid-metadata:{backup_name}"),
            )
        }
    }
}

fn target_key_for_path(beads_dir: &Path, target_path: &Path) -> String {
    BackupMetadata::from_target_path(beads_dir, target_path)
        .target
        .key()
}

pub(crate) fn resolve_backup_target_path(beads_dir: &Path, backup_path: &Path) -> Result<PathBuf> {
    let backup_name = backup_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            BeadsError::Config(format!(
                "Invalid backup filename format: {}",
                backup_path.display()
            ))
        })?;
    if parse_backup_filename(backup_name).is_none() {
        return Err(BeadsError::Config(format!(
            "Invalid backup filename format: {backup_name}"
        )));
    }
    let metadata = read_backup_metadata(backup_path)?.ok_or_else(|| {
        BeadsError::Config(format!(
            "History backup '{backup_name}' is missing target metadata and cannot be safely restored or diffed"
        ))
    })?;
    let target_path = metadata.target.resolve_path(beads_dir)?;

    validate_sync_path_with_external(&target_path, beads_dir, true)?;
    Ok(target_path)
}

/// Backup the JSONL file before export.
///
/// # Errors
///
/// Returns an error if the backup cannot be created.
pub fn backup_before_export(
    beads_dir: &Path,
    config: &HistoryConfig,
    target_path: &Path,
) -> Result<()> {
    if !config.enabled {
        return Ok(());
    }
    let Some(source) = capture_optional_jsonl_source(target_path)? else {
        return Ok(());
    };
    backup_before_export_snapshot(beads_dir, config, target_path, &source)
}

pub(crate) fn backup_before_export_snapshot(
    beads_dir: &Path,
    config: &HistoryConfig,
    target_path: &Path,
    source: &JsonlSourceSnapshot,
) -> Result<()> {
    if !config.enabled {
        return Ok(());
    }
    let history_dir = beads_dir.join(".br_history");

    ensure_history_dir_path(&history_dir)?;

    // Determine backup filename based on target filename
    let file_stem = target_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("issues");
    let target_key = target_key_for_path(beads_dir, target_path);

    // Skip a new snapshot when the most recent backup is either identical
    // content (dedup) or younger than the configured throttle floor (#313).
    // We match by full target identity so similarly named exports do not
    // collapse each other's history.
    if let Some(latest) = get_latest_backup(&history_dir, &target_key)? {
        if snapshot_and_file_are_identical(source, &latest.path)? {
            tracing::debug!(
                "Skipping backup: identical to latest {}",
                latest.path.display()
            );
            return Ok(());
        }

        // Time-throttle: under bursty mutation every `br` edit auto-flushes the
        // JSONL, and copying the whole file into a fresh backup each time is
        // O(repo-size) write amplification. If the latest snapshot is younger
        // than `min_interval_secs`, skip this one — the export still proceeds,
        // so the file is current; we just don't keep a near-duplicate snapshot
        // per edit. A future (older-than-floor) edit takes the next snapshot.
        if config.min_interval_secs > 0 {
            // Compare in whole seconds: building a chrono `Duration` from a
            // possibly-huge `min_interval_secs` could overflow/panic. `age_secs
            // >= 0` guards against a future-dated latest backup (clock skew): we
            // never skip based on a backup that appears newer than "now".
            let age_secs = Utc::now()
                .signed_duration_since(latest.timestamp)
                .num_seconds();
            // `try_from` fails for a negative age (future-dated latest / clock
            // skew), so we never throttle on a backup that appears newer than now.
            if u64::try_from(age_secs).is_ok_and(|age| age < config.min_interval_secs) {
                tracing::debug!(
                    "Skipping backup: throttled ({age_secs}s since latest < {}s floor)",
                    config.min_interval_secs
                );
                return Ok(());
            }
        }
    }

    // Create a timestamped backup file with collision resistance and no
    // overwrite race so pre-existing symlinks or files are never clobbered.
    let (backup_path, mut backup_file) = create_backup_file(&history_dir, file_stem)?;
    let mut backup_guard = BackupFileGuard::new(backup_path.clone());
    io::copy(&mut source.reader(), &mut backup_file).map_err(BeadsError::Io)?;
    backup_file.sync_all().map_err(BeadsError::Io)?;
    crate::util::sync_parent_directory(&backup_path).map_err(BeadsError::Io)?;
    write_backup_metadata(beads_dir, target_path, &backup_path)?;
    backup_guard.persist();
    tracing::debug!("Created backup: {}", backup_path.display());

    // Rotate history for this specific target
    rotate_history(&history_dir, config, &target_key)?;

    Ok(())
}

/// Preserve the exact source generation before an operator-authorized JSONL
/// salvage. Unlike ordinary export history, this backup is never throttled,
/// deduplicated, or rotated: rejected source bytes must remain recoverable.
///
/// # Errors
///
/// Returns an error if the history directory, backup, metadata, or durability
/// certificate cannot be created safely.
pub(crate) fn backup_before_jsonl_salvage(
    beads_dir: &Path,
    target_path: &Path,
    source: &JsonlSourceSnapshot,
) -> Result<PathBuf> {
    let history_dir = beads_dir.join(".br_history");
    ensure_history_dir_path(&history_dir)?;

    let target_stem = target_path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("issues");
    let salvage_stem = format!("{target_stem}.pre-salvage");
    let (backup_path, mut backup_file) = create_backup_file(&history_dir, &salvage_stem)?;
    let mut backup_guard = BackupFileGuard::new(backup_path.clone());

    io::copy(&mut source.reader(), &mut backup_file).map_err(BeadsError::Io)?;
    backup_file.sync_all().map_err(BeadsError::Io)?;
    crate::util::sync_parent_directory(&backup_path).map_err(BeadsError::Io)?;
    write_backup_metadata(beads_dir, target_path, &backup_path)?;
    backup_guard.persist();

    Ok(backup_path)
}

/// Rotate history backups based on config limits.
///
/// # Errors
///
/// Returns an error if listing or deleting backups fails.
fn rotate_history(history_dir: &Path, config: &HistoryConfig, target_key: &str) -> Result<()> {
    let mut backups: Vec<_> = list_backups(history_dir, None)?
        .into_iter()
        .filter(|entry| entry.target_key == target_key)
        .collect();

    if backups.is_empty() {
        return Ok(());
    }

    // Sort newest first to ensure limit-based pruning targets the oldest entries
    backups.sort_by_key(|b| std::cmp::Reverse(b.timestamp));

    // Determine cutoff time
    let now = Utc::now();
    let max_safe_days = 365_i64 * 1000;
    let days_i64 = i64::from(config.max_age_days).min(max_safe_days);
    let cutoff = now - chrono::Duration::days(days_i64);

    let mut deleted_count = 0;

    // Filter by age
    for (idx, entry) in backups.iter().enumerate() {
        let is_too_old = entry.timestamp < cutoff;
        let is_dominated = idx >= config.max_count;

        if is_too_old || is_dominated {
            remove_backup_artifacts(&entry.path)?;
            deleted_count += 1;
        }
    }

    if deleted_count > 0 {
        tracing::debug!("Pruned {} old backup(s) for {}", deleted_count, target_key);
    }

    Ok(())
}

/// List available backups sorted by date (newest first).
///
/// # Arguments
///
/// * `history_dir` - Directory containing backups
/// * `filter_prefix` - Optional prefix to filter filenames (e.g. "issues.")
///
/// # Errors
///
/// Returns an error if the directory cannot be read.
pub fn list_backups(history_dir: &Path, filter_prefix: Option<&str>) -> Result<Vec<BackupEntry>> {
    if !validate_history_dir_path(history_dir)? {
        return Ok(Vec::new());
    }

    let mut backups = Vec::new();

    for entry in fs::read_dir(history_dir)? {
        let entry = entry?;
        let path = entry.path();

        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        if let Some(prefix) = filter_prefix
            && !name.starts_with(prefix)
        {
            continue;
        }

        let is_jsonl = path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"));
        if !is_jsonl {
            continue;
        }

        let Some((_, timestamp)) = parse_backup_filename(name) else {
            continue;
        };

        let metadata = match history_artifact_metadata(&path, "backup file") {
            Ok(Some(metadata)) => metadata,
            Ok(None) => continue,
            Err(err) => {
                tracing::warn!(
                    backup = %path.display(),
                    error = %err,
                    "Ignoring unsafe history backup entry during history listing"
                );
                continue;
            }
        };
        let (target_path, target_key) = backup_target_details(history_dir, &path, name);

        backups.push(BackupEntry {
            path,
            timestamp,
            size: metadata.len(),
            target_path,
            target_key,
        });
    }

    // Sort newest first
    backups.sort_by_key(|b| std::cmp::Reverse(b.timestamp));

    Ok(backups)
}

fn get_latest_backup(history_dir: &Path, target_key: &str) -> Result<Option<BackupEntry>> {
    Ok(list_backups(history_dir, None)?
        .into_iter()
        .find(|entry| entry.target_key == target_key))
}

fn snapshot_and_file_are_identical(source: &JsonlSourceSnapshot, path: &Path) -> Result<bool> {
    let file = File::open(path).map_err(BeadsError::Io)?;
    if source.size() != file.metadata().map_err(BeadsError::Io)?.len() {
        return Ok(false);
    }

    readers_are_identical(source.reader(), BufReader::new(file))
}

fn readers_are_identical(mut reader1: impl Read, mut reader2: impl Read) -> Result<bool> {
    let mut buf1 = [0u8; 8192];
    let mut buf2 = [0u8; 8192];

    loop {
        let n1 = reader1.read(&mut buf1).map_err(BeadsError::Io)?;
        if n1 == 0 {
            return Ok(reader2.read(&mut buf2[..1]).map_err(BeadsError::Io)? == 0);
        }

        let mut n2_total = 0;
        while n2_total < n1 {
            let n2 = reader2
                .read(&mut buf2[n2_total..n1])
                .map_err(BeadsError::Io)?;
            if n2 == 0 {
                return Ok(false);
            }
            n2_total += n2;
        }

        if buf1[..n1] != buf2[..n1] {
            return Ok(false);
        }
    }
}

/// Prune old backups based on count and age.
///
/// # Errors
///
/// Returns an error if listing or deleting backups fails.
pub fn prune_backups(
    history_dir: &Path,
    keep: usize,
    older_than_days: Option<u32>,
) -> Result<usize> {
    let cutoff = older_than_days.map(|days| {
        let max_safe_days = 365_i64 * 1000;
        let days_i64 = i64::from(days).min(max_safe_days);
        Utc::now() - chrono::Duration::days(days_i64)
    });
    let mut backups_by_target: HashMap<String, Vec<BackupEntry>> = HashMap::new();

    for entry in list_backups(history_dir, None)? {
        backups_by_target
            .entry(entry.target_key.clone())
            .or_default()
            .push(entry);
    }

    let mut deleted_count = 0;

    for backups in backups_by_target.values_mut() {
        backups.sort_by_key(|b| std::cmp::Reverse(b.timestamp));

        for (i, entry) in backups.iter().enumerate() {
            let is_count_exceeded = i >= keep;
            let is_age_exceeded = cutoff.is_some_and(|c| entry.timestamp < c);

            if is_count_exceeded || is_age_exceeded {
                remove_backup_artifacts(&entry.path).map_err(|err| {
                    BeadsError::Config(format!(
                        "Failed to delete backup '{}': {err}",
                        entry.path.display()
                    ))
                })?;
                deleted_count += 1;
            }
        }
    }

    Ok(deleted_count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::symlink;
    use tempfile::TempDir;

    #[test]
    fn test_backup_rotation() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let history_dir = beads_dir.join(".br_history");
        fs::create_dir_all(&history_dir).unwrap();

        let config = HistoryConfig {
            enabled: true,
            max_count: 2,
            max_age_days: 30,
            min_interval_secs: 0,
        };

        // Manually create 3 backup files with distinct timestamps
        // Use very recent dates to avoid age pruning
        let now = Utc::now();
        let t1 = (now - chrono::Duration::hours(3)).format("%Y%m%d_%H%M%S");
        let t2 = (now - chrono::Duration::hours(2)).format("%Y%m%d_%H%M%S");
        let t3 = (now - chrono::Duration::hours(1)).format("%Y%m%d_%H%M%S");

        let file1 = format!("issues.{t1}.jsonl");
        let file2 = format!("issues.{t2}.jsonl");
        let file3 = format!("issues.{t3}.jsonl");

        let test_files = [&file1, &file2, &file3];

        for name in &test_files {
            File::create(history_dir.join(name)).unwrap();
        }

        // Verify initial state
        let backups = list_backups(&history_dir, None).unwrap();
        assert_eq!(backups.len(), 3);

        // Run rotation for "issues" stem
        let target_key = target_key_for_path(&beads_dir, &beads_dir.join("issues.jsonl"));
        rotate_history(&history_dir, &config, &target_key).unwrap();

        // Should keep only max_count (2) newest files
        let remaining = list_backups(&history_dir, None).unwrap();
        assert_eq!(remaining.len(), 2);

        // Ensure the oldest one was deleted
        assert!(
            !remaining
                .iter()
                .any(|b| b.path.to_string_lossy().contains(&t1.to_string()))
        );
        // Ensure newer ones kept
        assert!(
            remaining
                .iter()
                .any(|b| b.path.to_string_lossy().contains(&t2.to_string()))
        );
        assert!(
            remaining
                .iter()
                .any(|b| b.path.to_string_lossy().contains(&t3.to_string()))
        );
    }

    #[test]
    fn test_backup_before_export_throttles_within_interval() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();
        let history_dir = beads_dir.join(".br_history");
        let target = beads_dir.join("issues.jsonl");

        // Large floor so the second (immediate) snapshot is always throttled.
        let config = HistoryConfig {
            enabled: true,
            max_count: 100,
            max_age_days: 30,
            min_interval_secs: 3600,
        };

        fs::write(&target, "v1\n").unwrap();
        backup_before_export(&beads_dir, &config, &target).unwrap();
        // Changed content, so the identical-content dedup does NOT fire — only
        // the time-throttle can skip this second snapshot (#313).
        fs::write(&target, "v2-different\n").unwrap();
        backup_before_export(&beads_dir, &config, &target).unwrap();

        let backups = list_backups(&history_dir, Some("issues.")).unwrap();
        assert_eq!(
            backups.len(),
            1,
            "a changed file within the throttle floor must not create a second snapshot (#313)"
        );

        // With the throttle disabled, the same changed-content export does snapshot.
        let unthrottled = HistoryConfig {
            min_interval_secs: 0,
            ..config.clone()
        };
        backup_before_export(&beads_dir, &unthrottled, &target).unwrap();
        let backups = list_backups(&history_dir, Some("issues.")).unwrap();
        assert!(
            backups.len() >= 2,
            "with the throttle disabled a changed file snapshots again"
        );
    }

    #[test]
    fn test_backup_before_export_keeps_same_stem_targets_separate() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let history_dir = beads_dir.join(".br_history");
        let external_dir = temp.path().join("external");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&external_dir).unwrap();

        let config = HistoryConfig {
            enabled: true,
            max_count: 10,
            max_age_days: 30,
            min_interval_secs: 0,
        };

        let internal_target = beads_dir.join("issues.jsonl");
        let external_target = external_dir.join("issues.jsonl");
        fs::write(&internal_target, "internal\n").unwrap();
        fs::write(&external_target, "external\n").unwrap();

        backup_before_export(&beads_dir, &config, &internal_target).unwrap();
        backup_before_export(&beads_dir, &config, &external_target).unwrap();

        let backups = list_backups(&history_dir, Some("issues.")).unwrap();
        assert_eq!(backups.len(), 2);
        assert!(
            backups
                .iter()
                .any(|entry| entry.target_path == internal_target)
        );
        assert!(
            backups
                .iter()
                .any(|entry| entry.target_path == external_target)
        );
        assert_eq!(
            backups
                .iter()
                .map(|entry| entry.target_key.as_str())
                .collect::<std::collections::HashSet<_>>()
                .len(),
            2
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_backup_before_export_rejects_symlinked_source_target() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let outside_dir = temp.path().join("outside");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&outside_dir).unwrap();

        let outside_target = outside_dir.join("issues.jsonl");
        fs::write(&outside_target, "outside\n").unwrap();
        let target_path = beads_dir.join("issues.jsonl");
        symlink(&outside_target, &target_path).unwrap();

        let err =
            backup_before_export(&beads_dir, &HistoryConfig::default(), &target_path).unwrap_err();
        assert!(
            matches!(&err, BeadsError::Config(_)),
            "unexpected error: {err:?}"
        );
        let BeadsError::Config(message) = err else {
            return;
        };
        assert!(
            message.contains("must not be a symlink"),
            "unexpected message: {message}"
        );
        assert!(
            !beads_dir.join(".br_history").exists(),
            "rejected symlinked source must not create history state"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_backup_before_export_rejects_broken_symlink_source_target() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let target_path = beads_dir.join("issues.jsonl");
        symlink(temp.path().join("missing.jsonl"), &target_path).unwrap();

        let err =
            backup_before_export(&beads_dir, &HistoryConfig::default(), &target_path).unwrap_err();
        assert!(
            matches!(&err, BeadsError::Config(_)),
            "unexpected error: {err:?}"
        );
        let BeadsError::Config(message) = err else {
            return;
        };
        assert!(
            message.contains("must not be a symlink"),
            "unexpected message: {message}"
        );
        assert!(
            !beads_dir.join(".br_history").exists(),
            "rejected broken symlink source must not create history state"
        );
    }

    #[test]
    fn test_backup_before_export_rejects_directory_source_target() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let target_path = beads_dir.join("issues.jsonl");
        fs::create_dir(&target_path).unwrap();

        let err =
            backup_before_export(&beads_dir, &HistoryConfig::default(), &target_path).unwrap_err();
        assert!(
            matches!(&err, BeadsError::Config(_)),
            "unexpected error: {err:?}"
        );
        let BeadsError::Config(message) = err else {
            return;
        };
        assert!(
            message.contains("must be a regular file") || message.contains("is not a regular file"),
            "unexpected message: {message}"
        );
        assert!(
            !beads_dir.join(".br_history").exists(),
            "rejected directory source must not create history state"
        );
    }

    #[test]
    fn test_prune_backups_removes_metadata_sidecars() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let history_dir = beads_dir.join(".br_history");
        fs::create_dir_all(&beads_dir).unwrap();

        let config = HistoryConfig {
            enabled: true,
            max_count: 10,
            max_age_days: 30,
            min_interval_secs: 0,
        };
        let target = beads_dir.join("issues.jsonl");
        fs::write(&target, "issue\n").unwrap();
        backup_before_export(&beads_dir, &config, &target).unwrap();

        let backup = list_backups(&history_dir, None)
            .unwrap()
            .into_iter()
            .next()
            .expect("backup entry");
        let metadata_path = backup_metadata_path(&backup.path);
        assert!(metadata_path.is_file());

        let deleted = prune_backups(&history_dir, 0, None).unwrap();
        assert_eq!(deleted, 1);
        assert!(!backup.path.exists());
        assert!(!metadata_path.exists());
    }

    #[test]
    fn test_deduplication() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let history_dir = beads_dir.join(".br_history");
        fs::create_dir_all(&beads_dir).unwrap();

        let jsonl_path = beads_dir.join("issues.jsonl");
        File::create(&jsonl_path)
            .unwrap()
            .write_all(b"content")
            .unwrap();

        let config = HistoryConfig::default();

        // First backup
        backup_before_export(&beads_dir, &config, &jsonl_path).unwrap();

        // Second backup (same content) - should be skipped
        backup_before_export(&beads_dir, &config, &jsonl_path).unwrap();

        let backups = list_backups(&history_dir, None).unwrap();
        assert_eq!(backups.len(), 1);
    }

    #[test]
    fn test_list_backups_parsing() {
        let temp = TempDir::new().unwrap();
        let history_dir = temp.path();

        // Create files with manual timestamps
        File::create(history_dir.join("issues.20230101_100000.jsonl")).unwrap();
        File::create(history_dir.join("issues.20230102_100000.jsonl")).unwrap();
        File::create(history_dir.join("issues.invalid_name.jsonl")).unwrap();

        let backups = list_backups(history_dir, None).unwrap();
        assert_eq!(backups.len(), 2);

        // Newest first
        assert!(backups[0].path.to_string_lossy().contains("20230102"));
        assert!(backups[1].path.to_string_lossy().contains("20230101"));
    }

    #[test]
    fn test_list_backups_parses_high_precision_timestamps_and_collision_suffix() {
        let temp = TempDir::new().unwrap();
        let history_dir = temp.path();

        File::create(history_dir.join("issues.20230101_100000_123456.1.jsonl")).unwrap();
        File::create(history_dir.join("issues.20230101_100001_654321.jsonl")).unwrap();
        File::create(history_dir.join("issues.20230101_100002_123456789.jsonl")).unwrap();

        let backups = list_backups(history_dir, None).unwrap();
        assert_eq!(backups.len(), 3);
        assert!(
            backups[0]
                .path
                .to_string_lossy()
                .contains("20230101_100002_123456789")
        );
        assert!(
            backups[1]
                .path
                .to_string_lossy()
                .contains("20230101_100001_654321")
        );
        assert!(
            backups[2]
                .path
                .to_string_lossy()
                .contains("20230101_100000_123456.1")
        );
    }

    #[test]
    fn test_rapid_distinct_backups_do_not_collide() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let target = beads_dir.join("issues.jsonl");
        // Disable the snapshot throttle: this test exercises filename
        // collision-resistance for two same-second backups, which requires
        // both snapshots to actually be written (#313).
        let config = HistoryConfig {
            min_interval_secs: 0,
            ..HistoryConfig::default()
        };

        File::create(&target)
            .unwrap()
            .write_all(b"version-1")
            .unwrap();
        backup_before_export(&beads_dir, &config, &target).unwrap();

        File::create(&target)
            .unwrap()
            .write_all(b"version-2")
            .unwrap();
        backup_before_export(&beads_dir, &config, &target).unwrap();

        let backups = list_backups(&beads_dir.join(".br_history"), Some("issues.")).unwrap();
        assert_eq!(backups.len(), 2);
    }

    #[test]
    fn test_prune_backups() {
        let temp = TempDir::new().unwrap();
        let history_dir = temp.path();

        // Create 5 files
        for i in 0..5 {
            let ts = Utc::now() - chrono::Duration::days(i64::from(i));
            let ts_str = ts.format("%Y%m%d_%H%M%S");
            File::create(history_dir.join(format!("issues.{ts_str}.jsonl"))).unwrap();
        }

        // Keep 3
        let deleted = prune_backups(history_dir, 3, None).unwrap();
        assert_eq!(deleted, 2);
        assert_eq!(list_backups(history_dir, None).unwrap().len(), 3);

        // Keep 100 (default), older than 2 days
        // Files remaining: 0, 1, 2 days old.
        // older_than 2 means delete anything older than 48h (effectively file 2)
        // file 1 (24h old) is kept.
        let deleted_age = prune_backups(history_dir, 100, Some(2)).unwrap();
        assert_eq!(deleted_age, 1);
        assert_eq!(list_backups(history_dir, None).unwrap().len(), 2);
    }

    #[test]
    fn test_prune_backups_applies_keep_per_stem() {
        let temp = TempDir::new().unwrap();
        let history_dir = temp.path();
        let now = Utc::now();

        for (stem, hours_ago) in [
            ("issues", 4_i64),
            ("issues", 3),
            ("archive", 2),
            ("archive", 1),
        ] {
            let ts = (now - chrono::Duration::hours(hours_ago)).format("%Y%m%d_%H%M%S");
            File::create(history_dir.join(format!("{stem}.{ts}.jsonl"))).unwrap();
        }

        let deleted = prune_backups(history_dir, 1, None).unwrap();
        assert_eq!(deleted, 2);
        assert_eq!(list_backups(history_dir, Some("issues.")).unwrap().len(), 1);
        assert_eq!(
            list_backups(history_dir, Some("archive.")).unwrap().len(),
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_list_backups_ignores_symlinked_backup_files() {
        let temp = TempDir::new().unwrap();
        let history_dir = temp.path();
        let outside = temp.path().join("outside.jsonl");
        fs::write(&outside, "outside\n").unwrap();
        symlink(&outside, history_dir.join("issues.20260307_120000.jsonl")).unwrap();

        let backups = list_backups(history_dir, None).unwrap();
        assert!(backups.is_empty(), "symlinked backup files must be ignored");
    }

    #[cfg(unix)]
    #[test]
    fn test_list_backups_rejects_symlinked_history_directory() {
        let temp = TempDir::new().unwrap();
        let outside_dir = temp.path().join("outside");
        let history_dir = temp.path().join(".br_history");
        fs::create_dir_all(&outside_dir).unwrap();
        fs::write(outside_dir.join("issues.20260307_120000.jsonl"), "backup\n").unwrap();
        symlink(&outside_dir, &history_dir).unwrap();

        let err = list_backups(&history_dir, None).unwrap_err();
        assert!(
            matches!(&err, BeadsError::Config(_)),
            "unexpected error: {err:?}"
        );
        let BeadsError::Config(message) = err else {
            return;
        };
        assert!(
            message.contains("must not be a symlink"),
            "unexpected message: {message}"
        );
    }

    #[test]
    fn test_prune_backups_returns_error_on_partial_deletion_failure() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let history_dir = beads_dir.join(".br_history");
        fs::create_dir_all(&history_dir).unwrap();

        let target_path = beads_dir.join("issues.jsonl");
        fs::write(&target_path, "issue\n").unwrap();

        let backup_path = history_dir.join("issues.20260307_120000_000000.jsonl");
        fs::write(&backup_path, "backup\n").unwrap();
        write_backup_metadata(&beads_dir, &target_path, &backup_path).unwrap();

        fs::remove_file(backup_metadata_path(&backup_path)).unwrap();
        fs::create_dir(backup_metadata_path(&backup_path)).unwrap();

        let err = prune_backups(&history_dir, 0, None).unwrap_err();
        assert!(
            matches!(&err, BeadsError::Config(_)),
            "unexpected error: {err:?}"
        );
        let BeadsError::Config(message) = err else {
            return;
        };
        assert!(
            message.contains("Failed to delete backup"),
            "unexpected message: {message}"
        );
    }

    #[test]
    fn test_list_backups_marks_invalid_metadata_without_guessing_target() {
        let temp = TempDir::new().unwrap();
        let history_dir = temp.path();
        let backup_name = "issues.20260307_120000.jsonl";
        let backup_path = history_dir.join(backup_name);
        fs::write(&backup_path, "backup\n").unwrap();
        fs::write(backup_metadata_path(&backup_path), "{not-json").unwrap();

        let backups = list_backups(history_dir, None).unwrap();
        assert_eq!(backups.len(), 1);
        assert_eq!(
            backups[0].target_key,
            format!("invalid-metadata:{backup_name}")
        );
        assert_eq!(
            backups[0].target_path,
            invalid_metadata_target_path(backup_name)
        );
    }

    #[test]
    fn test_list_backups_marks_invalid_relative_metadata_without_guessing_target() {
        let temp = TempDir::new().unwrap();
        let history_dir = temp.path();
        let cases = [
            (
                "issues.20260307_120000.jsonl",
                temp.path().join("escape.jsonl").display().to_string(),
            ),
            (
                "labels.20260307_120001.jsonl",
                "../escape.jsonl".to_string(),
            ),
        ];

        for (backup_name, metadata_path) in cases {
            let backup_path = history_dir.join(backup_name);
            fs::write(&backup_path, "backup\n").unwrap();
            fs::write(
                backup_metadata_path(&backup_path),
                serde_json::json!({
                    "target": {
                        "kind": "relative",
                        "path": metadata_path,
                    }
                })
                .to_string(),
            )
            .unwrap();
        }

        let backups = list_backups(history_dir, None).unwrap();
        assert_eq!(backups.len(), 2);
        for backup in backups {
            let backup_name = backup
                .path
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap();
            assert_eq!(backup.target_key, format!("invalid-metadata:{backup_name}"));
            assert_eq!(
                backup.target_path,
                invalid_metadata_target_path(backup_name)
            );
        }
    }

    #[test]
    fn test_resolve_backup_target_path_rejects_invalid_relative_metadata() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let history_dir = beads_dir.join(".br_history");
        fs::create_dir_all(&history_dir).unwrap();

        let backup_path = history_dir.join("issues.20260307_120000.jsonl");
        fs::write(&backup_path, "backup\n").unwrap();
        fs::write(
            backup_metadata_path(&backup_path),
            serde_json::json!({
                "target": {
                    "kind": "relative",
                    "path": temp.path().join("escape.jsonl").display().to_string(),
                }
            })
            .to_string(),
        )
        .unwrap();

        let err = resolve_backup_target_path(&beads_dir, &backup_path).unwrap_err();
        assert!(
            matches!(&err, BeadsError::Config(message) if message.contains("Invalid relative backup target metadata path")),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn test_create_backup_file_skips_stale_metadata_sidecar() {
        let temp = TempDir::new().unwrap();
        let history_dir = temp.path();
        let timestamp = "20260307_120000_000000";
        let first_backup_path = history_dir.join(format!("issues.{timestamp}.jsonl"));
        let stale_metadata_path = backup_metadata_path(&first_backup_path);
        fs::write(&stale_metadata_path, "stale metadata\n").unwrap();

        let (backup_path, file) =
            create_backup_file_for_timestamp(history_dir, "issues", timestamp).unwrap();
        drop(file);

        assert_eq!(
            backup_path.file_name().and_then(|name| name.to_str()),
            Some("issues.20260307_120000_000000.1.jsonl")
        );
        assert!(
            !first_backup_path.exists(),
            "allocator should not create a backup next to stale metadata"
        );
        assert_eq!(
            fs::read_to_string(&stale_metadata_path).unwrap(),
            "stale metadata\n",
            "stale metadata sidecar should be left untouched"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_write_backup_metadata_rejects_existing_symlink() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let history_dir = beads_dir.join(".br_history");
        let outside_dir = temp.path().join("outside");
        fs::create_dir_all(&history_dir).unwrap();
        fs::create_dir_all(&outside_dir).unwrap();

        let target_path = beads_dir.join("issues.jsonl");
        fs::write(&target_path, "issue\n").unwrap();

        let backup_path = history_dir.join("issues.20260307_120000_000000.jsonl");
        fs::write(&backup_path, "backup\n").unwrap();

        let metadata_target = outside_dir.join("captured.json");
        fs::write(&metadata_target, "do-not-touch").unwrap();
        symlink(&metadata_target, backup_metadata_path(&backup_path)).unwrap();

        let err = write_backup_metadata(&beads_dir, &target_path, &backup_path).unwrap_err();
        assert!(matches!(err, BeadsError::Io(_)), "unexpected error: {err}");
        assert_eq!(
            fs::read_to_string(&metadata_target).unwrap(),
            "do-not-touch",
            "existing metadata symlink target should not be overwritten"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_backup_before_export_rejects_symlinked_history_directory() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let outside_dir = temp.path().join("outside");
        let history_dir = beads_dir.join(".br_history");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&outside_dir).unwrap();
        symlink(&outside_dir, &history_dir).unwrap();

        let target_path = beads_dir.join("issues.jsonl");
        fs::write(&target_path, "issue\n").unwrap();

        let err =
            backup_before_export(&beads_dir, &HistoryConfig::default(), &target_path).unwrap_err();
        assert!(
            matches!(&err, BeadsError::Config(_)),
            "unexpected error: {err:?}"
        );
        let BeadsError::Config(message) = err else {
            return;
        };
        assert!(
            message.contains("must not be a symlink"),
            "unexpected message: {message}"
        );

        assert!(
            fs::read_dir(&outside_dir).unwrap().next().is_none(),
            "symlinked history directory must not receive backups"
        );
    }
}
