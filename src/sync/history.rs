//! Local history backup for JSONL exports.
//!
//! This module handles:
//! - Creating timestamped backups of `issues.jsonl` before export
//! - Rotating backups based on count and age
//! - Listing and restoring backups

use crate::error::{BeadsError, Result};
use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use std::fs::{self, File};
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

/// Configuration for history backups.
#[derive(Debug, Clone)]
pub struct HistoryConfig {
    pub enabled: bool,
    pub max_count: usize,
    pub max_age_days: u32,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_count: 100,
            max_age_days: 30,
        }
    }
}

/// Backup entry metadata.
#[derive(Debug, Clone)]
pub struct BackupEntry {
    pub path: PathBuf,
    pub timestamp: DateTime<Utc>,
    pub size: u64,
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

    let history_dir = beads_dir.join(".br_history");

    if !target_path.exists() {
        return Ok(());
    }

    // Create history directory if it doesn't exist
    if !history_dir.exists() {
        fs::create_dir_all(&history_dir).map_err(BeadsError::Io)?;
    }

    // Determine backup filename based on target filename
    let file_stem = target_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("issues");

    // Create timestamped backup
    let timestamp = Utc::now().format("%Y%m%d_%H%M%S");
    let backup_name = format!("{file_stem}.{timestamp}.jsonl");
    let backup_path = history_dir.join(backup_name);

    // Check if the content is identical to the most recent backup (deduplication)
    // We only check against backups that match the target's stem to avoid false positives
    // across different files, though collisions are unlikely with timestamps.
    if let Some(latest) = get_latest_backup(&history_dir, Some(file_stem))? {
        if files_are_identical(target_path, &latest.path)? {
            tracing::debug!(
                "Skipping backup: identical to latest {}",
                latest.path.display()
            );
            return Ok(());
        }
    }

    fs::copy(target_path, &backup_path).map_err(BeadsError::Io)?;
    tracing::debug!("Created backup: {}", backup_path.display());

    // Rotate history for this file stem
    rotate_history(&history_dir, config, file_stem)?;

    Ok(())
}

/// Rotate history backups based on config limits.
///
/// # Errors
///
/// Returns an error if listing or deleting backups fails.
fn rotate_history(history_dir: &Path, config: &HistoryConfig, file_stem: &str) -> Result<()> {
    // Only rotate backups for this specific file
    let prefix = format!("{file_stem}.");
    let backups = list_backups(history_dir, Some(&prefix))?;

    if backups.is_empty() {
        return Ok(());
    }

    // Determine cutoff time
    let now = Utc::now();
    let cutoff = now - chrono::Duration::days(i64::from(config.max_age_days));

    let mut deleted_count = 0;

    // Filter by age
    for (idx, entry) in backups.iter().enumerate() {
        let is_too_old = entry.timestamp < cutoff;
        let is_dominated = idx >= config.max_count;

        if is_too_old || is_dominated {
            fs::remove_file(&entry.path).map_err(BeadsError::Io)?;
            deleted_count += 1;
        }
    }

    if deleted_count > 0 {
        tracing::debug!("Pruned {} old backup(s) for {}", deleted_count, file_stem);
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
    if !history_dir.exists() {
        return Ok(Vec::new());
    }

    let mut backups = Vec::new();

    for entry in fs::read_dir(history_dir)? {
        let entry = entry?;
        let path = entry.path();

        if !path.is_file() {
            continue;
        }

        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };

        if let Some(prefix) = filter_prefix {
            if !name.starts_with(prefix) {
                continue;
            }
        }

        let is_jsonl = path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"));
        if !is_jsonl {
            continue;
        }

        // Parse timestamp from filename: <stem>.YYYYMMDD_HHMMSS.jsonl
        // We split by dot and treat the second-to-last component as the timestamp.
        let parts: Vec<&str> = name.split('.').collect();
        if parts.len() < 3 {
            continue;
        }

        let ts_str = parts[parts.len() - 2];
        if ts_str.len() != 15 {
            continue;
        }

        let Ok(dt) = NaiveDateTime::parse_from_str(ts_str, "%Y%m%d_%H%M%S") else {
            continue;
        };

        let Ok(metadata) = fs::metadata(&path) else {
            continue;
        };

        let timestamp = Utc.from_utc_datetime(&dt);
        backups.push(BackupEntry {
            path,
            timestamp,
            size: metadata.len(),
        });
    }

    // Sort newest first
    backups.sort_by_key(|b| std::cmp::Reverse(b.timestamp));

    Ok(backups)
}

fn get_latest_backup(history_dir: &Path, filter_stem: Option<&str>) -> Result<Option<BackupEntry>> {
    // If filtering by stem, ensure we match "{stem}." to avoid prefix collisions
    // e.g. "issues" shouldn't match "issues_archive"
    let prefix = filter_stem.map(|s| format!("{s}."));
    let backups = list_backups(history_dir, prefix.as_deref())?;

    Ok(backups.into_iter().next())
}

/// Compare two files by content hash.
fn files_are_identical(p1: &Path, p2: &Path) -> Result<bool> {
    let f1 = File::open(p1).map_err(BeadsError::Io)?;
    let f2 = File::open(p2).map_err(BeadsError::Io)?;

    let len1 = f1.metadata().map_err(BeadsError::Io)?.len();
    let len2 = f2.metadata().map_err(BeadsError::Io)?.len();

    if len1 != len2 {
        return Ok(false);
    }

    let mut reader1 = BufReader::new(f1);
    let mut reader2 = BufReader::new(f2);

    let mut buf1 = [0u8; 8192];
    let mut buf2 = [0u8; 8192];

    loop {
        let n1 = reader1.read(&mut buf1).map_err(BeadsError::Io)?;
        if n1 == 0 {
            break;
        }

        // Fill buffer 2 to match n1
        let mut n2_total = 0;
        while n2_total < n1 {
            let n2 = reader2
                .read(&mut buf2[n2_total..n1])
                .map_err(BeadsError::Io)?;
            if n2 == 0 {
                return Ok(false); // Unexpected EOF
            }
            n2_total += n2;
        }

        if buf1[..n1] != buf2[..n1] {
            return Ok(false);
        }
    }

    Ok(true)
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
    let mut backups = list_backups(history_dir, None)?;

    // Sort by timestamp descending (newest first)
    backups.sort_by_key(|b| std::cmp::Reverse(b.timestamp));

    let mut deleted_count = 0;

    // Calculate age cutoff if provided
    let cutoff = older_than_days.map(|days| Utc::now() - chrono::Duration::days(i64::from(days)));

    for (i, entry) in backups.iter().enumerate() {
        // Delete if we have exceeded the count limit OR the age limit
        let is_count_exceeded = i >= keep;
        let is_age_exceeded = cutoff.is_some_and(|c| entry.timestamp < c);

        if is_count_exceeded || is_age_exceeded {
            if let Err(e) = fs::remove_file(&entry.path) {
                tracing::warn!("Failed to delete backup {}: {}", entry.path.display(), e);
            } else {
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
        rotate_history(&history_dir, &config, "issues").unwrap();

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
}
