use crate::cli::HistoryArgs;
use crate::cli::HistoryCommands;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::output::OutputContext;
use crate::sync::history;
use rich_rust::prelude::*;
use serde_json::json;
use std::path::Path;

/// Execute the history command.
///
/// # Errors
///
/// Returns an error if history operations fail (e.g. IO error, invalid path).
pub fn execute(args: HistoryArgs, _cli: &config::CliOverrides, ctx: &OutputContext) -> Result<()> {
    let beads_dir = config::discover_beads_dir(Some(Path::new(".")))?;
    let history_dir = beads_dir.join(".br_history");

    match args.command {
        Some(HistoryCommands::Diff { file }) => diff_backup(&beads_dir, &history_dir, &file, ctx),
        Some(HistoryCommands::Restore { file, force }) => {
            restore_backup(&beads_dir, &history_dir, &file, force, ctx)
        }
        Some(HistoryCommands::Prune { keep, older_than }) => {
            prune_backups(&history_dir, keep, older_than, ctx)
        }
        Some(HistoryCommands::List) | None => list_backups(&history_dir, ctx),
    }
}

/// List available backups.
fn list_backups(history_dir: &Path, ctx: &OutputContext) -> Result<()> {
    let backups = history::list_backups(history_dir, None)?;

    if ctx.is_json() {
        let items: Vec<_> = backups
            .iter()
            .map(|entry| {
                let filename = entry
                    .path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string();
                json!({
                    "filename": filename,
                    "size_bytes": entry.size,
                    "size": format_size(entry.size),
                    "timestamp": entry.timestamp.to_rfc3339(),
                })
            })
            .collect();
        let output = json!({
            "directory": history_dir.display().to_string(),
            "count": backups.len(),
            "backups": items,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    if ctx.is_quiet() {
        return Ok(());
    }

    if backups.is_empty() {
        if ctx.is_rich() {
            let theme = ctx.theme();
            let panel = Panel::from_text("No backups found.")
                .title(Text::styled("History Backups", theme.panel_title.clone()))
                .box_style(theme.box_style)
                .border_style(theme.panel_border.clone());
            ctx.render(&panel);
        } else {
            println!("No backups found in {}", history_dir.display());
        }
        return Ok(());
    }

    if ctx.is_rich() {
        let theme = ctx.theme();
        let mut table = Table::new()
            .box_style(theme.box_style)
            .border_style(theme.panel_border.clone())
            .title(Text::styled("History Backups", theme.panel_title.clone()));

        table = table
            .with_column(Column::new("Filename").min_width(20).max_width(40))
            .with_column(Column::new("Size").min_width(8).max_width(12))
            .with_column(Column::new("Timestamp").min_width(20).max_width(26));

        for entry in backups {
            let filename = entry
                .path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            let size = format_size(entry.size);
            let timestamp = entry.timestamp.format("%Y-%m-%d %H:%M:%S UTC").to_string();
            let row = Row::new(vec![
                Cell::new(Text::styled(filename, theme.emphasis.clone())),
                Cell::new(Text::new(size)),
                Cell::new(Text::styled(timestamp, theme.timestamp.clone())),
            ]);
            table.add_row(row);
        }

        ctx.render(&table);
    } else {
        println!("Backups in {}:", history_dir.display());
        println!("{:<30} {:<10} {:<20}", "FILENAME", "SIZE", "TIMESTAMP");
        println!("{}", "-".repeat(62));

        for entry in backups {
            let filename = entry.path.file_name().unwrap_or_default().to_string_lossy();
            let size = format_size(entry.size);
            let timestamp = entry.timestamp.format("%Y-%m-%d %H:%M:%S UTC").to_string();
            println!("{filename:<30} {size:<10} {timestamp:<20}");
        }
    }

    Ok(())
}

/// Show diff between current state and a backup.
fn diff_backup(
    beads_dir: &Path,
    history_dir: &Path,
    filename: &str,
    ctx: &OutputContext,
) -> Result<()> {
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

    if ctx.is_json() {
        let (status_label, diff_available, size_fallback) =
            diff_status_for_json(&current_path, &backup_path)?;
        let output = json!({
            "action": "diff",
            "backup": filename,
            "current": current_path.display().to_string(),
            "status": status_label,
            "diff_available": diff_available,
            "current_size_bytes": size_fallback.map(|sizes| sizes.0),
            "backup_size_bytes": size_fallback.map(|sizes| sizes.1),
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    if ctx.is_quiet() {
        return Ok(());
    }

    if ctx.is_rich() {
        let theme = ctx.theme();
        let header = format!("Current: issues.jsonl\nBackup: {filename}");
        let panel = Panel::from_text(&header)
            .title(Text::styled("History Diff", theme.panel_title.clone()))
            .box_style(theme.box_style)
            .border_style(theme.panel_border.clone());
        ctx.render(&panel);
    } else {
        println!("Diffing current issues.jsonl vs {filename}...");
    }

    // Let's shell out to `diff -u` for now as it's standard on linux/mac.
    // Avoid GNU-only flags (like --color) to keep this portable.
    let status = std::process::Command::new("diff")
        .arg("-u")
        .arg(&current_path)
        .arg(&backup_path)
        .status();

    match status {
        Ok(s) => {
            if s.success() {
                if ctx.is_rich() {
                    ctx.success("Files are identical.");
                } else {
                    println!("Files are identical.");
                }
            }
            // diff returns 1 if differences found, which is fine/expected.
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let current_size = std::fs::metadata(&current_path)?.len();
            let backup_size = std::fs::metadata(&backup_path)?.len();
            let current_human = format_size(current_size);
            let backup_human = format_size(backup_size);
            if ctx.is_rich() {
                let theme = ctx.theme();
                let body = format!(
                    "Diff tool not available; comparing sizes.\nCurrent: {current_human} ({current_size} bytes)\nBackup:  {backup_human} ({backup_size} bytes)"
                );
                let panel = Panel::from_text(&body)
                    .title(Text::styled("History Diff", theme.panel_title.clone()))
                    .box_style(theme.box_style)
                    .border_style(theme.panel_border.clone());
                ctx.render(&panel);
            } else {
                println!("'diff' command not found. Comparing sizes:");
                println!("Current: {current_size} bytes");
                println!("Backup:  {backup_size} bytes");
            }
        }
        Err(err) => {
            return Err(BeadsError::Config(format!("Failed to run diff: {err}")));
        }
    }

    Ok(())
}

/// Restore a backup.
fn restore_backup(
    beads_dir: &Path,
    history_dir: &Path,
    filename: &str,
    force: bool,
    ctx: &OutputContext,
) -> Result<()> {
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

    if ctx.is_json() {
        let output = json!({
            "action": "restore",
            "backup": filename,
            "target": target_path.display().to_string(),
            "restored": true,
            "next_step": "br sync --import-only --force",
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    if ctx.is_quiet() {
        return Ok(());
    }

    if ctx.is_rich() {
        let theme = ctx.theme();
        let body =
            format!("Restored {filename} to issues.jsonl.\nNext: br sync --import-only --force");
        let panel = Panel::from_text(&body)
            .title(Text::styled("History Restore", theme.panel_title.clone()))
            .box_style(theme.box_style)
            .border_style(theme.panel_border.clone());
        ctx.render(&panel);
    } else {
        println!("Restored {filename} to issues.jsonl");
        println!("Run 'br sync --import-only --force' to import this state into the database.");
    }

    Ok(())
}

/// Prune old backups.
fn prune_backups(
    history_dir: &Path,
    keep: usize,
    older_than_days: Option<u32>,
    ctx: &OutputContext,
) -> Result<()> {
    let deleted = crate::sync::history::prune_backups(history_dir, keep, older_than_days)?;

    if ctx.is_json() {
        let output = json!({
            "action": "prune",
            "deleted": deleted,
            "keep": keep,
            "older_than_days": older_than_days,
        });
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    if ctx.is_quiet() {
        return Ok(());
    }

    if ctx.is_rich() {
        let theme = ctx.theme();
        let mut body = format!("Pruned {deleted} backup(s).");
        if let Some(days) = older_than_days {
            body.push_str(&format!(
                "\nCriteria: keep {keep}, delete older than {days} days"
            ));
        } else {
            body.push_str(&format!("\nCriteria: keep {keep} newest backups"));
        }
        let panel = Panel::from_text(&body)
            .title(Text::styled("History Prune", theme.panel_title.clone()))
            .box_style(theme.box_style)
            .border_style(theme.panel_border.clone());
        ctx.render(&panel);
    } else {
        println!("Pruned {deleted} backup(s).");
    }
    Ok(())
}

fn diff_status_for_json(
    current_path: &Path,
    backup_path: &Path,
) -> Result<(&'static str, bool, Option<(u64, u64)>)> {
    let output = std::process::Command::new("diff")
        .arg("-u")
        .arg(current_path)
        .arg(backup_path)
        .output();

    match output {
        Ok(out) => {
            if out.status.success() {
                Ok(("identical", true, None))
            } else if out.status.code() == Some(1) {
                Ok(("different", true, None))
            } else {
                Ok(("error", true, None))
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let current_size = std::fs::metadata(current_path)?.len();
            let backup_size = std::fs::metadata(backup_path)?.len();
            Ok(("diff_unavailable", false, Some((current_size, backup_size))))
        }
        Err(_) => Ok(("error", false, None)),
    }
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
