//! Local history backup for JSONL exports.
//!
//! This module handles:
//! - Creating timestamped backups of `issues.jsonl` before export
//! - Rotating backups based on count and age
//! - Listing and restoring backups

use crate::error::{BeadsError, Result};
use chrono::{DateTime, Utc};
use std::fs;
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
pub fn backup_before_export(beads_dir: &Path, config: &HistoryConfig) -> Result<()> {
    if !config.enabled {
        return Ok(());
    }

    let history_dir = beads_dir.join(".br_history");
    let current_jsonl = beads_dir.join("issues.jsonl");

    if !current_jsonl.exists() {
        return Ok(());
    }

    // Create history directory if it doesn't exist
    if !history_dir.exists() {
        fs::create_dir_all(&history_dir).map_err(BeadsError::Io)?;
    }

    // Create timestamped backup
    let timestamp = Utc::now().format("%Y%m%d_%H%M%S");
    let backup_name = format!("issues.{timestamp}.jsonl");
    let backup_path = history_dir.join(backup_name);

    // Check if the content is identical to the most recent backup (deduplication)
    if let Some(latest) = get_latest_backup(&history_dir)? {
        if files_are_identical(&current_jsonl, &latest.path)? {
            tracing::debug!("Skipping backup: identical to latest {}", latest.path.display());
            return Ok(());
        }
    }

    fs::copy(&current_jsonl, &backup_path).map_err(BeadsError::Io)?;
    tracing::debug!("Created backup: {}", backup_path.display());

    // Rotate history
    rotate_history(&history_dir, config)?;

    Ok(())
}

/// Rotate history backups based on config limits.
///
/// # Errors
///
/// Returns an error if listing or deleting backups fails.
fn rotate_history(history_dir: &Path, config: &HistoryConfig) -> Result<()> {
    let backups = list_backups(history_dir)?;

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
        tracing::debug!("Pruned {} old backup(s)", deleted_count);
    }

    Ok(())
}

/// List available backups sorted by date (newest first).
///
/// # Errors
///
/// Returns an error if the directory cannot be read.
pub fn list_backups(history_dir: &Path) -> Result<Vec<BackupEntry>> {
    if !history_dir.exists() {
        return Ok(Vec::new());
    }

    let mut backups = Vec::new();

    for entry in fs::read_dir(history_dir)? {
        let entry = entry?;
        let path = entry.path();

        if path.is_file() {
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with("issues.")
                    && Path::new(name).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
                {
                    // Parse timestamp from filename: issues.YYYYMMDD_HHMMSS.jsonl
                    if let Ok(metadata) = fs::metadata(&path) {
                        let timestamp = DateTime::<Utc>::from(metadata.modified().map_err(BeadsError::Io)?);
                        backups.push(BackupEntry {
                            path,
                            timestamp,
                            size: metadata.len(),
                        });
                    }
                }
            }
        }
    }

    // Sort newest first
    backups.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

    Ok(backups)
}

fn get_latest_backup(history_dir: &Path) -> Result<Option<BackupEntry>> {
    let backups = list_backups(history_dir)?;
    Ok(backups.into_iter().next())
}

/// Compare two files by content hash.
fn files_are_identical(p1: &Path, p2: &Path) -> Result<bool> {
    let c1 = fs::read(p1).map_err(BeadsError::Io)?;
    let c2 = fs::read(p2).map_err(BeadsError::Io)?;
    Ok(c1 == c2)
}

/// Prune old backups based on count and age.
///
/// # Errors
///
/// Returns an error if listing or deleting backups fails.
pub fn prune_backups(history_dir: &Path, keep: usize, older_than_days: Option<u32>) -> Result<usize> {
    let mut backups = list_backups(history_dir)?;

    // Sort by timestamp descending (newest first)
    backups.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));

    let mut deleted_count = 0;

    // Calculate age cutoff if provided
    let cutoff = older_than_days.map(|days| Utc::now() - chrono::Duration::days(i64::from(days)));

    // Keep the first `keep` backups regardless of age
    for (i, entry) in backups.iter().enumerate() {
        if i < keep {
            continue;
        }

        // Check if expired
        let expired = cutoff.is_some_and(|c| entry.timestamp < c);

        if expired {
            if let Err(e) = fs::remove_file(&entry.path) {
                tracing::warn!("Failed to delete backup {}: {}", entry.path.display(), e);
            } else {
                deleted_count += 1;
            }
        }
    }

    Ok(deleted_count)
}

// Re-export needed chrono type for parsing
// use chrono::NaiveDateTime;
