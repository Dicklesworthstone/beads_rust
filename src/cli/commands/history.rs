use crate::cli::HistoryArgs;
use crate::cli::HistoryCommands;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::sync::history;
use colored::Colorize;
use std::path::Path;

/// Execute the history command.
///
/// # Errors
///
/// Returns an error if history operations fail (e.g. IO error, invalid path).
pub fn execute(args: HistoryArgs, _cli: &config::CliOverrides) -> Result<()> {
    let beads_dir = config::discover_beads_dir(Some(Path::new(".")))?;
    let history_dir = beads_dir.join(".br_history");

    match args.command {
        Some(HistoryCommands::Diff { file }) => diff_backup(&beads_dir, &history_dir, &file),
        Some(HistoryCommands::Restore { file, force }) => {
            restore_backup(&beads_dir, &history_dir, &file, force)
        }
        Some(HistoryCommands::Prune { keep, older_than }) => {
            prune_backups(&history_dir, keep, older_than)
        }
        Some(HistoryCommands::List) | None => list_backups(&history_dir),
    }
}

/// List available backups.
fn list_backups(history_dir: &Path) -> Result<()> {
    let backups = history::list_backups(history_dir)?;

    if backups.is_empty() {
        println!("No backups found in {}", history_dir.display());
        return Ok(());
    }

    println!("Backups in {}:", history_dir.display());
    println!("{:<30} {:<10} {:<20}", "FILENAME", "SIZE", "TIMESTAMP");
    println!("{}", "-".repeat(62));

    for entry in backups {
        let filename = entry.path.file_name().unwrap().to_string_lossy();
        let size = format_size(entry.size);
        let timestamp = entry.timestamp.format("%Y-%m-%d %H:%M:%S UTC").to_string();
        println!("{filename:<30} {size:<10} {timestamp:<20}");
    }

    Ok(())
}

/// Show diff between current state and a backup.
fn diff_backup(beads_dir: &Path, history_dir: &Path, filename: &str) -> Result<()> {
    let backup_path = history_dir.join(filename);
    if !backup_path.exists() {
        return Err(BeadsError::Config(format!(
            "Backup file not found: {filename}"
        )));
    }

    let current_path = beads_dir.join("issues.jsonl");
    if !current_path.exists() {
        return Err(BeadsError::Config(
            "Current issues.jsonl not found".to_string(),
        ));
    }

    println!(
        "Diffing {} vs {}...",
        "current issues.jsonl".green(),
        filename.red()
    );

    // Simple diff by reading both files and comparing lines?
    // Or just shelling out to `diff`?
    // Since we are a CLI, shelling out to `diff` is often better for UX if available.
    // But let's do a simple internal diff to avoid dependencies.
    // Or better, use `bv --robot-diff` if possible? No, br should be standalone.

    // Let's shell out to `diff -u` for now as it's standard on linux/mac.
    // If it fails, we fallback or error.
    let status = std::process::Command::new("diff")
        .args([
            "-u",
            "--color=always",
            current_path.to_str().unwrap(),
            backup_path.to_str().unwrap(),
        ])
        .status();

    if let Ok(s) = status {
        if s.success() {
            println!("Files are identical.");
        }
        // diff returns 1 if differences found, which is fine/expected.
    } else {
        println!("'diff' command not found. Comparing sizes:");
        let current_size = std::fs::metadata(&current_path)?.len();
        let backup_size = std::fs::metadata(&backup_path)?.len();
        println!("Current: {current_size} bytes");
        println!("Backup:  {backup_size} bytes");
    }

    Ok(())
}

/// Restore a backup.
fn restore_backup(beads_dir: &Path, history_dir: &Path, filename: &str, force: bool) -> Result<()> {
    let backup_path = history_dir.join(filename);
    if !backup_path.exists() {
        return Err(BeadsError::Config(format!(
            "Backup file not found: {filename}"
        )));
    }

    let target_path = beads_dir.join("issues.jsonl");

    if target_path.exists() && !force {
        return Err(BeadsError::Config(
            "Current issues.jsonl exists. Use --force to overwrite.".to_string(),
        ));
    }

    // Copy backup to issues.jsonl
    std::fs::copy(&backup_path, &target_path)?;
    println!("Restored {filename} to issues.jsonl");
    println!("Run 'br sync --import-only --force' to import this state into the database.");

    Ok(())
}

/// Prune old backups.
fn prune_backups(history_dir: &Path, keep: usize, older_than_days: Option<u32>) -> Result<()> {
    let deleted = crate::sync::history::prune_backups(history_dir, keep, older_than_days)?;
    println!("Pruned {deleted} backup(s).");
    Ok(())
}

#[allow(clippy::cast_precision_loss)]
fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * 1024;

    if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}
