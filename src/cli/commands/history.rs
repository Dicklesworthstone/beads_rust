use crate::cli::HistoryArgs;
use crate::cli::HistoryCommands;
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::{sanitize_terminal_inline, sanitize_terminal_text};
use crate::output::OutputContext;
use crate::sync::history;
use crate::sync::{require_safe_sync_overwrite_path, validate_temp_file_path};
use rich_rust::prelude::*;
use serde_json::json;
use similar::TextDiff;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

/// Result type for diff status: (status_string, diff_available, optional_size_tuple).
type DiffStatusResult = (&'static str, bool, Option<(u64, u64)>);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TextDiffFallbackReason {
    NonUtf8,
    TooLarge,
}

impl TextDiffFallbackReason {
    const fn message(self) -> &'static str {
        match self {
            Self::NonUtf8 => "one or both files are not valid UTF-8",
            Self::TooLarge => "one or both files exceed the text diff size limit",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct TextDiffFallback {
    status: &'static str,
    current_size: u64,
    backup_size: u64,
    reason: TextDiffFallbackReason,
}

enum HistoryFileDiff {
    Text(String),
    Fallback(TextDiffFallback),
}

struct TempRestoreGuard {
    path: PathBuf,
    persist: bool,
}

impl TempRestoreGuard {
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

impl Drop for TempRestoreGuard {
    fn drop(&mut self) {
        if !self.persist && self.path.exists() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

const MAX_HISTORY_RESTORE_TEMP_PATH_ATTEMPTS: u32 = 64;
const MAX_RESTORE_ROLLBACK_PATH_ATTEMPTS: u64 = 1024;
const MAX_HISTORY_TEXT_DIFF_BYTES: u64 = 8 * 1024 * 1024;
const DIFF_COMPARE_BUFFER_SIZE: usize = 16 * 1024;
const RESTORE_NOTE_DB_UNCHANGED: &str = "SQLite is unchanged until you run the import step.";
const RESTORE_NOTE_TOMBSTONE_PROTECTION: &str =
    "Tombstone protection remains active; deleted issues are not resurrected by import.";

fn history_display_text(value: &str) -> String {
    sanitize_terminal_inline(value).into_owned()
}

fn history_display_path(path: &Path) -> String {
    history_display_text(&path.display().to_string())
}

fn history_display_filename(path: &Path) -> String {
    let filename = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    history_display_text(&filename)
}

fn restore_temp_path_for_attempt(target_path: &Path, attempt: u32) -> PathBuf {
    let pid = std::process::id();
    if attempt == 0 {
        return target_path.with_extension(format!("jsonl.{pid}.tmp"));
    }

    let retry_suffix = u64::from(pid)
        .saturating_mul(100)
        .saturating_add(u64::from(attempt));
    target_path.with_extension(format!("jsonl.{retry_suffix}.tmp"))
}

fn create_restore_temp_file(
    target_path: &Path,
    beads_dir: &Path,
) -> Result<(PathBuf, File, TempRestoreGuard)> {
    for attempt in 0..MAX_HISTORY_RESTORE_TEMP_PATH_ATTEMPTS {
        let temp_path = restore_temp_path_for_attempt(target_path, attempt);
        validate_temp_file_path(&temp_path, target_path, beads_dir, true)?;

        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => {
                let temp_guard = TempRestoreGuard::new(temp_path.clone());
                return Ok((temp_path, file, temp_guard));
            }
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
                if fs::symlink_metadata(&temp_path)
                    .is_ok_and(|metadata| metadata.file_type().is_symlink())
                {
                    return Err(BeadsError::Config(format!(
                        "Temporary restore file already exists: {}",
                        history_display_path(&temp_path)
                    )));
                }
            }
            Err(err) => return Err(err.into()),
        }
    }

    Err(BeadsError::Config(format!(
        "Failed to allocate temporary restore file for '{}'",
        history_display_path(target_path)
    )))
}

fn create_restore_rollback_snapshot(
    target_path: &Path,
    beads_dir: &Path,
) -> Result<TempRestoreGuard> {
    let pid = u64::from(std::process::id());

    for offset in 1..=MAX_RESTORE_ROLLBACK_PATH_ATTEMPTS {
        let rollback_path =
            target_path.with_extension(format!("jsonl.{}.tmp", pid.saturating_add(offset)));
        validate_temp_file_path(&rollback_path, target_path, beads_dir, true)?;

        let mut writer = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&rollback_path)
        {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err.into()),
        };
        let rollback_guard = TempRestoreGuard::new(rollback_path);
        let mut reader = File::open(target_path)?;
        io::copy(&mut reader, &mut writer)?;
        writer.sync_all()?;
        drop(writer);
        crate::util::sync_parent_directory(&rollback_guard.path)?;
        return Ok(rollback_guard);
    }

    Err(BeadsError::Config(format!(
        "Failed to allocate rollback snapshot path for '{}'",
        history_display_path(target_path)
    )))
}

fn commit_restored_target_with_rollback<R>(
    temp_path: &Path,
    target_path: &Path,
    rollback_guard: Option<&mut TempRestoreGuard>,
    mut rename_impl: R,
) -> Result<()>
where
    R: FnMut(&Path, &Path) -> io::Result<()>,
{
    match rename_impl(temp_path, target_path) {
        Ok(()) => Ok(()),
        Err(rename_err) => {
            if let Some(rollback_guard) = rollback_guard {
                if !target_path.exists() {
                    return match crate::util::durable_rename(&rollback_guard.path, target_path) {
                        Ok(()) => Err(BeadsError::Config(format!(
                            "Failed to replace '{}' with the restored backup: {rename_err}. The original target was restored.",
                            history_display_path(target_path)
                        ))),
                        Err(rollback_err) => {
                            rollback_guard.persist();
                            Err(BeadsError::Config(format!(
                                "Failed to replace '{}' with the restored backup: {rename_err}. Restoring the original target from '{}' also failed: {rollback_err}",
                                history_display_path(target_path),
                                history_display_path(&rollback_guard.path)
                            )))
                        }
                    };
                }

                rollback_guard.persist();
                return Err(BeadsError::Config(format!(
                    "Failed to replace '{}' with the restored backup: {rename_err}. The original target snapshot was preserved at '{}'.",
                    history_display_path(target_path),
                    history_display_path(&rollback_guard.path)
                )));
            }

            Err(rename_err.into())
        }
    }
}

fn emit_restore_output(
    ctx: &OutputContext,
    backup_name: &str,
    target_path: &Path,
    target_name: &str,
    beads_dir: &Path,
) {
    let next_step = restore_next_step(beads_dir, target_path);
    let notes = [RESTORE_NOTE_DB_UNCHANGED, RESTORE_NOTE_TOMBSTONE_PROTECTION];

    if ctx.is_json() {
        let output = json!({
            "action": "restore",
            "backup": backup_name,
            "target": target_path.display().to_string(),
            "restored": true,
            "next_step": next_step,
            "notes": notes,
        });
        ctx.json_pretty(&output);
        return;
    }

    if ctx.is_toon() {
        let output = json!({
            "action": "restore",
            "backup": backup_name,
            "target": target_path.display().to_string(),
            "restored": true,
            "next_step": next_step,
            "notes": notes,
        });
        ctx.toon(&output);
        return;
    }

    if ctx.is_quiet() {
        return;
    }

    if ctx.is_rich() {
        let theme = ctx.theme();
        let backup_name = history_display_text(backup_name);
        let target_name = history_display_text(target_name);
        let next_step = history_display_text(&next_step);
        let body = format!(
            "Restored {backup_name} to {target_name}.\n\
             Next: {next_step}\n\
             Note: {RESTORE_NOTE_DB_UNCHANGED}\n\
             Note: {RESTORE_NOTE_TOMBSTONE_PROTECTION}"
        );
        let panel = Panel::from_text(&body)
            .title(Text::styled("History Restore", theme.panel_title.clone()))
            .box_style(theme.box_style)
            .border_style(theme.panel_border.clone());
        ctx.render(&panel);
    } else {
        println!(
            "Restored {} to {}",
            history_display_text(backup_name),
            history_display_text(target_name)
        );
        println!("Next: {}", history_display_text(&next_step));
        println!("Note: {RESTORE_NOTE_DB_UNCHANGED}");
        println!("Note: {RESTORE_NOTE_TOMBSTONE_PROTECTION}");
    }
}

fn ensure_regular_backup_file(backup_path: &Path, backup_name: &str) -> Result<()> {
    match fs::symlink_metadata(backup_path) {
        Ok(metadata) => {
            let file_type = metadata.file_type();
            if file_type.is_symlink() {
                return Err(BeadsError::Config(format!(
                    "History backup '{}' must not be a symlink",
                    history_display_text(backup_name)
                )));
            }
            if !file_type.is_file() {
                return Err(BeadsError::Config(format!(
                    "History backup '{}' must be a regular file",
                    history_display_text(backup_name)
                )));
            }
            Ok(())
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => Err(BeadsError::Config(format!(
            "Backup file not found: {}",
            history_display_text(backup_name)
        ))),
        Err(err) => Err(err.into()),
    }
}

/// Execute the history command.
///
/// # Errors
///
/// Returns an error if history operations fail (e.g. IO error, invalid path).
pub fn execute(args: HistoryArgs, cli: &config::CliOverrides, ctx: &OutputContext) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let history_dir = beads_dir.join(".br_history");
    let _ = history::validate_history_dir_path(&history_dir)?;

    match args.command {
        Some(HistoryCommands::Diff { file }) => {
            let active_jsonl_path = config::resolve_paths(&beads_dir, cli.db.as_ref())?.jsonl_path;
            diff_backup(
                &beads_dir,
                &history_dir,
                &file,
                Some(&active_jsonl_path),
                ctx,
            )
        }
        Some(HistoryCommands::Restore { file, force }) => {
            let active_jsonl_path = config::resolve_paths(&beads_dir, cli.db.as_ref())?.jsonl_path;
            restore_backup(
                &beads_dir,
                &history_dir,
                &file,
                force,
                Some(&active_jsonl_path),
                ctx,
            )
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
        let output = history_backup_list_payload(history_dir, &backups);
        ctx.json_pretty(&output);
        return Ok(());
    }

    if ctx.is_toon() {
        let output = history_backup_list_payload(history_dir, &backups);
        ctx.toon(&output);
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
            println!("No backups found in {}", history_display_path(history_dir));
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
            .with_column(Column::new("Target").min_width(24).max_width(56))
            .with_column(Column::new("Size").min_width(8).max_width(12))
            .with_column(Column::new("Timestamp").min_width(20).max_width(26));

        for entry in backups {
            let filename = history_display_filename(&entry.path);
            let target = history_display_path(&entry.target_path);
            let size = format_size(entry.size);
            let timestamp = entry.timestamp.format("%Y-%m-%d %H:%M:%S UTC").to_string();
            let row = Row::new(vec![
                Cell::new(Text::styled(filename, theme.emphasis.clone())),
                Cell::new(Text::new(target)),
                Cell::new(Text::new(size)),
                Cell::new(Text::styled(timestamp, theme.timestamp.clone())),
            ]);
            table.add_row(row);
        }

        ctx.render(&table);
    } else {
        println!("Backups in {}:", history_display_path(history_dir));
        println!(
            "{:<30} {:<36} {:<10} {:<20}",
            "FILENAME", "TARGET", "SIZE", "TIMESTAMP"
        );
        println!("{}", "-".repeat(100));

        for entry in backups {
            let filename = history_display_filename(&entry.path);
            let target = history_display_path(&entry.target_path);
            let size = format_size(entry.size);
            let timestamp = entry.timestamp.format("%Y-%m-%d %H:%M:%S UTC").to_string();
            println!("{filename:<30} {target:<36} {size:<10} {timestamp:<20}");
        }
    }

    Ok(())
}

fn history_backup_list_payload(
    history_dir: &Path,
    backups: &[history::BackupEntry],
) -> serde_json::Value {
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
                "target": entry.target_path.display().to_string(),
                "size_bytes": entry.size,
                "size": format_size(entry.size),
                "timestamp": entry.timestamp.to_rfc3339(),
            })
        })
        .collect();

    json!({
        "directory": history_dir.display().to_string(),
        "count": backups.len(),
        "backups": items,
    })
}

/// Show diff between current state and a backup.
fn diff_backup(
    beads_dir: &Path,
    history_dir: &Path,
    filename: &str,
    active_jsonl_path: Option<&Path>,
    ctx: &OutputContext,
) -> Result<()> {
    let backup_name = validated_backup_filename(filename)?;
    let backup_path = history_dir.join(&backup_name);
    ensure_regular_backup_file(&backup_path, &backup_name)?;

    let current_path = current_jsonl_path_for_backup(beads_dir, &backup_name, active_jsonl_path)?;
    let current_name = current_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();
    if !current_path.exists() {
        return Err(BeadsError::Config(format!(
            "Current {} not found",
            history_display_text(&current_name)
        )));
    }

    if ctx.is_json() {
        let (status_label, diff_available, size_fallback) =
            diff_status_for_json(&current_path, &backup_path)?;
        let output = json!({
            "action": "diff",
            "backup": backup_name,
            "current": current_path.display().to_string(),
            "status": status_label,
            "diff_available": diff_available,
            "current_size_bytes": size_fallback.map(|sizes| sizes.0),
            "backup_size_bytes": size_fallback.map(|sizes| sizes.1),
        });
        ctx.json_pretty(&output);
        return Ok(());
    }

    if ctx.is_toon() {
        let (status_label, diff_available, size_fallback) =
            diff_status_for_json(&current_path, &backup_path)?;
        let output = json!({
            "action": "diff",
            "backup": backup_name,
            "current": current_path.display().to_string(),
            "status": status_label,
            "diff_available": diff_available,
            "current_size_bytes": size_fallback.map(|sizes| sizes.0),
            "backup_size_bytes": size_fallback.map(|sizes| sizes.1),
        });
        ctx.toon(&output);
        return Ok(());
    }

    if ctx.is_quiet() {
        return Ok(());
    }

    if ctx.is_rich() {
        let theme = ctx.theme();
        let header = format!(
            "Current: {}\nBackup: {}",
            history_display_text(&current_name),
            history_display_text(&backup_name)
        );
        let panel = Panel::from_text(&header)
            .title(Text::styled("History Diff", theme.panel_title.clone()))
            .box_style(theme.box_style)
            .border_style(theme.panel_border.clone());
        ctx.render(&panel);
    } else {
        println!(
            "Diffing current {} vs {}...",
            history_display_text(&current_name),
            history_display_text(&backup_name)
        );
    }

    match history_diff_for_files(&current_path, &backup_path)? {
        HistoryFileDiff::Text(diff) => {
            if diff.is_empty() {
                if ctx.is_rich() {
                    ctx.success("Files are identical.");
                } else {
                    println!("Files are identical.");
                }
            } else {
                print!("{diff}");
            }
        }
        HistoryFileDiff::Fallback(fallback) => {
            emit_diff_fallback(ctx, fallback);
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
    active_jsonl_path: Option<&Path>,
    ctx: &OutputContext,
) -> Result<()> {
    let backup_name = validated_backup_filename(filename)?;
    let backup_path = history_dir.join(&backup_name);
    ensure_regular_backup_file(&backup_path, &backup_name)?;

    let target_path = current_jsonl_path_for_backup(beads_dir, &backup_name, active_jsonl_path)?;
    let target_name = target_path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .to_string();

    if target_path.exists() && !force {
        return Err(BeadsError::Config(format!(
            "Current {} exists. Use --force to overwrite.",
            history_display_text(&target_name)
        )));
    }

    validate_temp_file_path(
        &restore_temp_path_for_attempt(&target_path, 0),
        &target_path,
        beads_dir,
        true,
    )?;
    if let Some(parent) = target_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut reader = File::open(&backup_path)?;
    let (temp_path, mut writer, mut temp_guard) =
        create_restore_temp_file(&target_path, beads_dir)?;
    io::copy(&mut reader, &mut writer)?;
    writer.sync_all()?;
    drop(writer);
    let mut rollback_guard = None;
    if force && target_path.exists() {
        require_safe_sync_overwrite_path(
            &target_path,
            beads_dir,
            true,
            "overwrite history restore target",
        )?;
        rollback_guard = Some(create_restore_rollback_snapshot(&target_path, beads_dir)?);
    }
    commit_restored_target_with_rollback(
        &temp_path,
        &target_path,
        rollback_guard.as_mut(),
        crate::util::durable_rename,
    )?;
    temp_guard.persist();
    emit_restore_output(ctx, &backup_name, &target_path, &target_name, beads_dir);

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
        ctx.json_pretty(&output);
        return Ok(());
    }

    if ctx.is_toon() {
        let output = json!({
            "action": "prune",
            "deleted": deleted,
            "keep": keep,
            "older_than_days": older_than_days,
        });
        ctx.toon(&output);
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

fn diff_status_for_json(current_path: &Path, backup_path: &Path) -> Result<DiffStatusResult> {
    let summary = summarize_diff_files(current_path, backup_path)?;
    if summary.diff_available {
        Ok((summary.status, true, None))
    } else {
        Ok((
            summary.status,
            false,
            Some((summary.current_size, summary.backup_size)),
        ))
    }
}

#[derive(Debug, Clone, Copy)]
struct DiffFileSummary {
    status: &'static str,
    diff_available: bool,
    current_size: u64,
    backup_size: u64,
    fallback_reason: Option<TextDiffFallbackReason>,
}

fn summarize_diff_files(current_path: &Path, backup_path: &Path) -> Result<DiffFileSummary> {
    let current_size = fs::metadata(current_path)?.len();
    let backup_size = fs::metadata(backup_path)?.len();
    let identical = files_are_byte_identical(current_path, backup_path, current_size, backup_size)?;
    let status = if identical { "identical" } else { "different" };

    if current_size > MAX_HISTORY_TEXT_DIFF_BYTES || backup_size > MAX_HISTORY_TEXT_DIFF_BYTES {
        return Ok(DiffFileSummary {
            status,
            diff_available: false,
            current_size,
            backup_size,
            fallback_reason: Some(TextDiffFallbackReason::TooLarge),
        });
    }

    let diff_available = file_is_utf8(current_path)? && file_is_utf8(backup_path)?;
    Ok(DiffFileSummary {
        status,
        diff_available,
        current_size,
        backup_size,
        fallback_reason: if diff_available {
            None
        } else {
            Some(TextDiffFallbackReason::NonUtf8)
        },
    })
}

fn history_diff_for_files(current_path: &Path, backup_path: &Path) -> Result<HistoryFileDiff> {
    let summary = summarize_diff_files(current_path, backup_path)?;
    if let Some(reason) = summary.fallback_reason {
        return Ok(HistoryFileDiff::Fallback(TextDiffFallback {
            status: summary.status,
            current_size: summary.current_size,
            backup_size: summary.backup_size,
            reason,
        }));
    }

    unified_diff_for_files(current_path, backup_path).map(HistoryFileDiff::Text)
}

fn unified_diff_for_files(current_path: &Path, backup_path: &Path) -> Result<String> {
    let current = fs::read_to_string(current_path)?;
    let backup = fs::read_to_string(backup_path)?;
    let diff = TextDiff::from_lines(&current, &backup);
    let current_header = history_display_path(current_path);
    let backup_header = history_display_path(backup_path);
    let diff = diff
        .unified_diff()
        .header(&current_header, &backup_header)
        .to_string();
    Ok(sanitize_terminal_text(&diff).into_owned())
}

fn emit_diff_fallback(ctx: &OutputContext, fallback: TextDiffFallback) {
    let prefix = if fallback.status == "identical" {
        "Files are byte-identical."
    } else {
        "Files differ."
    };
    let message = format!(
        "{prefix} Text diff unavailable: {}. Current size: {}; backup size: {}.",
        fallback.reason.message(),
        format_size(fallback.current_size),
        format_size(fallback.backup_size)
    );

    if ctx.is_rich() {
        ctx.warning(&message);
    } else {
        println!("{message}");
    }
}

fn file_is_utf8(path: &Path) -> Result<bool> {
    let bytes = fs::read(path)?;
    Ok(std::str::from_utf8(&bytes).is_ok())
}

fn files_are_byte_identical(
    current_path: &Path,
    backup_path: &Path,
    current_size: u64,
    backup_size: u64,
) -> Result<bool> {
    if current_size != backup_size {
        return Ok(false);
    }

    let mut current = File::open(current_path)?;
    let mut backup = File::open(backup_path)?;
    let mut current_buf = [0_u8; DIFF_COMPARE_BUFFER_SIZE];
    let mut backup_buf = [0_u8; DIFF_COMPARE_BUFFER_SIZE];

    loop {
        let current_read = current.read(&mut current_buf)?;
        let backup_read = backup.read(&mut backup_buf)?;
        if current_read != backup_read {
            return Ok(false);
        }
        if current_read == 0 {
            return Ok(true);
        }
        if current_buf.get(..current_read) != backup_buf.get(..backup_read) {
            return Ok(false);
        }
    }
}

fn current_jsonl_path_for_backup(
    beads_dir: &Path,
    filename: &str,
    active_jsonl_path: Option<&Path>,
) -> Result<PathBuf> {
    let cwd = std::env::current_dir().ok();
    current_jsonl_path_for_backup_with_cwd(beads_dir, filename, active_jsonl_path, cwd.as_deref())
}

fn current_jsonl_path_for_backup_with_cwd(
    beads_dir: &Path,
    filename: &str,
    active_jsonl_path: Option<&Path>,
    cwd: Option<&Path>,
) -> Result<PathBuf> {
    let backup_name = validated_backup_filename(filename)?;
    let target_path = history::resolve_backup_target_path(
        beads_dir,
        &beads_dir.join(".br_history").join(backup_name),
    )?;
    let is_external_target = is_external_jsonl_target(beads_dir, &target_path);

    if is_external_target {
        let active_jsonl_path = active_jsonl_path.ok_or_else(|| {
            BeadsError::Config(format!(
                "External backup target '{}' requires the current active JSONL path",
                history_display_path(&target_path)
            ))
        })?;
        let normalized_target = normalize_jsonl_match_path(&target_path, cwd);
        let normalized_active = normalize_jsonl_match_path(active_jsonl_path, cwd);
        let canonical_target =
            dunce::canonicalize(&normalized_target).unwrap_or_else(|_| normalized_target.clone());
        let canonical_active =
            dunce::canonicalize(&normalized_active).unwrap_or_else(|_| normalized_active.clone());
        if canonical_target != canonical_active {
            return Err(BeadsError::Config(format!(
                "Backup target '{}' does not match the active JSONL path '{}'",
                history_display_path(&target_path),
                history_display_path(active_jsonl_path)
            )));
        }
    }

    Ok(target_path)
}

fn is_external_jsonl_target(beads_dir: &Path, target_path: &Path) -> bool {
    let canonical_beads =
        dunce::canonicalize(beads_dir).unwrap_or_else(|_| beads_dir.to_path_buf());
    !target_path.starts_with(beads_dir) && !target_path.starts_with(&canonical_beads)
}

fn is_default_jsonl_target(beads_dir: &Path, target_path: &Path) -> bool {
    let default_target = beads_dir.join("issues.jsonl");
    if target_path == default_target {
        return true;
    }

    let canonical_target =
        dunce::canonicalize(target_path).unwrap_or_else(|_| target_path.to_path_buf());
    let canonical_default = dunce::canonicalize(&default_target).unwrap_or(default_target);
    canonical_target == canonical_default
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn restore_next_step(beads_dir: &Path, target_path: &Path) -> String {
    let mut command = String::new();

    if !is_default_jsonl_target(beads_dir, target_path) {
        command.push_str("BEADS_JSONL=");
        command.push_str(&shell_quote(&target_path.display().to_string()));
        command.push(' ');
    }

    command.push_str("br sync --import-only --force");

    if is_external_jsonl_target(beads_dir, target_path) {
        command.push_str(" --allow-external-jsonl");
    }

    command
}

fn normalize_jsonl_match_path(path: &Path, cwd: Option<&Path>) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else if let Some(cwd) = cwd {
        cwd.join(path)
    } else {
        path.to_path_buf()
    }
}

fn validated_backup_filename(filename: &str) -> Result<String> {
    let mut components = Path::new(filename).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(name)), None) => {
            name.to_str().map(str::to_string).ok_or_else(|| {
                BeadsError::Config(format!(
                    "Invalid backup filename format: {}",
                    history_display_text(filename)
                ))
            })
        }
        _ => Err(BeadsError::Config(format!(
            "Invalid backup filename format: {}",
            history_display_text(filename)
        ))),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::OutputContext;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn history_display_text_sanitizes_terminal_controls() {
        let rendered = history_display_text("issues\x1b[2J\rreset\x08\nnext\x07\u{9b}.jsonl");

        assert!(!rendered.chars().any(char::is_control));
        assert!(rendered.contains("\\u{1b}[2J"));
        assert!(rendered.contains("\\r"));
        assert!(rendered.contains("\\u{8}"));
        assert!(rendered.contains("\\n"));
        assert!(rendered.contains("\\u{7}"));
        assert!(rendered.contains("\\u{9b}"));
    }

    #[test]
    fn validated_backup_filename_errors_escape_terminal_controls() {
        let err = validated_backup_filename("bad\x1b[2J/name").unwrap_err();

        assert!(
            matches!(err, BeadsError::Config(_)),
            "unexpected error: {err:?}"
        );
        if let BeadsError::Config(message) = err {
            assert!(!message.chars().any(char::is_control));
            assert!(message.contains("\\u{1b}[2J"));
        }
    }

    #[test]
    fn unified_diff_for_files_escapes_terminal_controls() {
        let temp = TempDir::new().unwrap();
        let current = temp.path().join("current\x1b[2J\nfake.jsonl");
        let backup = temp.path().join("backup.jsonl");
        fs::write(&current, "{\"title\":\"current\u{7}\"}\n").unwrap();
        fs::write(&backup, "{\"title\":\"backup\u{8}\"}\n").unwrap();

        let diff = unified_diff_for_files(&current, &backup).unwrap();

        assert!(
            !diff
                .chars()
                .any(|ch| ch.is_control() && ch != '\n' && ch != '\t')
        );
        assert!(diff.contains("\\u{1b}[2J"));
        assert!(diff.contains("\\nfake.jsonl"));
        assert!(diff.contains("\\u{7}"));
        assert!(diff.contains("\\u{8}"));
    }

    #[test]
    fn test_current_jsonl_path_for_backup_rejects_missing_target_metadata() {
        let temp = TempDir::new().unwrap();
        let history_dir = temp.path().join(".br_history");
        fs::create_dir_all(&history_dir).unwrap();
        fs::write(
            history_dir.join("issues.20260220_120000.jsonl"),
            "backup-state\n",
        )
        .unwrap();

        let err = current_jsonl_path_for_backup(temp.path(), "issues.20260220_120000.jsonl", None)
            .unwrap_err();
        match err {
            BeadsError::Config(msg) => {
                assert!(
                    msg.contains("missing target metadata"),
                    "unexpected message: {msg}"
                );
            }
            other => assert!(
                matches!(other, BeadsError::Config(_)),
                "unexpected error: {other:?}"
            ),
        }
    }

    #[test]
    fn test_current_jsonl_path_for_backup_rejects_invalid_name() {
        let temp = TempDir::new().unwrap();
        let err = current_jsonl_path_for_backup(temp.path(), "issues.not-a-timestamp.jsonl", None)
            .unwrap_err();

        match err {
            BeadsError::Config(msg) => assert!(msg.contains("Invalid backup filename format")),
            other => assert!(
                matches!(other, BeadsError::Config(_)),
                "unexpected error: {other:?}"
            ),
        }
    }

    #[test]
    fn test_current_jsonl_path_for_backup_rejects_path_traversal() {
        let temp = TempDir::new().unwrap();
        let err =
            current_jsonl_path_for_backup(temp.path(), "../issues.20260220_120000.jsonl", None)
                .unwrap_err();

        match err {
            BeadsError::Config(msg) => assert!(msg.contains("Invalid backup filename format")),
            other => assert!(
                matches!(other, BeadsError::Config(_)),
                "unexpected error: {other:?}"
            ),
        }
    }

    #[test]
    fn test_unified_diff_for_files_reports_differences() {
        let temp = TempDir::new().unwrap();
        let current = temp.path().join("current.jsonl");
        let backup = temp.path().join("backup.jsonl");
        fs::write(&current, "{\"id\":\"one\",\"title\":\"current\"}\n").unwrap();
        fs::write(&backup, "{\"id\":\"one\",\"title\":\"backup\"}\n").unwrap();

        let diff = unified_diff_for_files(&current, &backup).unwrap();

        assert!(
            diff.contains("--- ") && diff.contains("+++ ") && diff.contains("@@"),
            "diff should include unified diff headers: {diff}"
        );
        assert!(
            diff.contains("-{\"id\":\"one\",\"title\":\"current\"}")
                && diff.contains("+{\"id\":\"one\",\"title\":\"backup\"}"),
            "diff should include current and backup lines: {diff}"
        );
    }

    #[test]
    fn test_unified_diff_for_files_returns_empty_for_identical_files() {
        let temp = TempDir::new().unwrap();
        let current = temp.path().join("current.jsonl");
        let backup = temp.path().join("backup.jsonl");
        fs::write(&current, "{\"id\":\"one\",\"title\":\"same\"}\n").unwrap();
        fs::write(&backup, "{\"id\":\"one\",\"title\":\"same\"}\n").unwrap();

        let diff = unified_diff_for_files(&current, &backup).unwrap();

        assert!(
            diff.is_empty(),
            "identical files should not emit diff: {diff}"
        );
    }

    #[test]
    fn test_diff_status_for_json_falls_back_for_invalid_utf8() {
        let temp = TempDir::new().unwrap();
        let current = temp.path().join("current.jsonl");
        let backup = temp.path().join("backup.jsonl");
        fs::write(&current, [0xff, b'{', b'}']).unwrap();
        fs::write(&backup, b"{}").unwrap();

        let (status, diff_available, sizes) = diff_status_for_json(&current, &backup).unwrap();

        assert_eq!(status, "different");
        assert!(!diff_available, "invalid UTF-8 cannot produce text diff");
        assert_eq!(sizes, Some((3, 2)));
    }

    #[test]
    fn test_history_diff_for_files_reports_identical_non_utf8_without_text_diff() {
        let temp = TempDir::new().unwrap();
        let current = temp.path().join("current.jsonl");
        let backup = temp.path().join("backup.jsonl");
        fs::write(&current, [0xff, 0x00, 0xfe]).unwrap();
        fs::write(&backup, [0xff, 0x00, 0xfe]).unwrap();

        let diff = history_diff_for_files(&current, &backup).unwrap();

        assert!(
            matches!(&diff, HistoryFileDiff::Fallback(_)),
            "non-UTF-8 files should not produce text diff"
        );
        if let HistoryFileDiff::Fallback(fallback) = diff {
            assert_eq!(fallback.status, "identical");
            assert_eq!(fallback.current_size, 3);
            assert_eq!(fallback.backup_size, 3);
            assert_eq!(fallback.reason, TextDiffFallbackReason::NonUtf8);
        }
    }

    #[test]
    fn test_diff_status_for_json_falls_back_for_large_inputs() {
        let temp = TempDir::new().unwrap();
        let current = temp.path().join("current.jsonl");
        let backup = temp.path().join("backup.jsonl");
        let current_file = fs::File::create(&current).unwrap();
        current_file
            .set_len(MAX_HISTORY_TEXT_DIFF_BYTES + 1)
            .unwrap();
        fs::write(&backup, "small\n").unwrap();

        let (status, diff_available, sizes) = diff_status_for_json(&current, &backup).unwrap();

        assert_eq!(status, "different");
        assert!(!diff_available, "large inputs should not build full diffs");
        assert_eq!(sizes, Some((MAX_HISTORY_TEXT_DIFF_BYTES + 1, 6)));
    }

    #[test]
    fn test_restore_backup_uses_metadata_target_path() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let history_dir = beads_dir.join(".br_history");
        fs::create_dir_all(&beads_dir).unwrap();

        let target_path = beads_dir.join("custom.jsonl");
        fs::write(&target_path, "new-state\n").unwrap();
        let config = history::HistoryConfig {
            enabled: true,
            max_count: 10,
            max_age_days: 30,
            min_interval_secs: 0,
        };
        history::backup_before_export(&beads_dir, &config, &target_path).unwrap();

        let backup_name = history::list_backups(&history_dir, None)
            .unwrap()
            .into_iter()
            .next()
            .and_then(|entry| {
                entry
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .expect("backup filename");
        fs::write(&target_path, "old-state\n").unwrap();

        let ctx = OutputContext::from_flags(false, true, true);
        restore_backup(&beads_dir, &history_dir, &backup_name, true, None, &ctx).unwrap();

        assert_eq!(
            fs::read_to_string(beads_dir.join("custom.jsonl")).unwrap(),
            "new-state\n"
        );
        assert!(!beads_dir.join("issues.jsonl").exists());
    }

    #[test]
    fn test_current_jsonl_path_for_backup_reads_target_metadata() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let history_dir = beads_dir.join(".br_history");
        let external_dir = temp.path().join("external");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&external_dir).unwrap();

        let external_target = external_dir.join("issues.jsonl");
        fs::write(&external_target, "external-state\n").unwrap();

        let config = history::HistoryConfig {
            enabled: true,
            max_count: 10,
            max_age_days: 30,
            min_interval_secs: 0,
        };
        history::backup_before_export(&beads_dir, &config, &external_target).unwrap();

        let backup_name = history::list_backups(&history_dir, None)
            .unwrap()
            .into_iter()
            .next()
            .and_then(|entry| {
                entry
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .expect("backup filename");

        let resolved =
            current_jsonl_path_for_backup(&beads_dir, &backup_name, Some(&external_target))
                .unwrap();
        assert_eq!(resolved, external_target);
    }

    #[test]
    fn test_current_jsonl_path_for_backup_accepts_relative_external_active_path_when_missing() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let history_dir = beads_dir.join(".br_history");
        let external_dir = temp.path().join("external");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(&external_dir).unwrap();

        let external_target = external_dir.join("issues.jsonl");
        fs::write(&external_target, "external-state\n").unwrap();

        let config = history::HistoryConfig {
            enabled: true,
            max_count: 10,
            max_age_days: 30,
            min_interval_secs: 0,
        };
        history::backup_before_export(&beads_dir, &config, &external_target).unwrap();

        let backup_name = history::list_backups(&history_dir, None)
            .unwrap()
            .into_iter()
            .next()
            .and_then(|entry| {
                entry
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .expect("backup filename");

        fs::remove_file(&external_target).unwrap();

        let resolved = current_jsonl_path_for_backup_with_cwd(
            &beads_dir,
            &backup_name,
            Some(Path::new("external/issues.jsonl")),
            Some(temp.path()),
        )
        .unwrap();
        assert_eq!(resolved, external_target);
    }

    #[test]
    fn test_restore_next_step_uses_default_import_for_internal_targets() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let internal_target = beads_dir.join("issues.jsonl");

        assert_eq!(
            restore_next_step(&beads_dir, &internal_target),
            "br sync --import-only --force"
        );
    }

    #[test]
    fn test_restore_next_step_sets_jsonl_path_for_internal_custom_targets() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let custom_target = beads_dir.join("custom.jsonl");

        assert_eq!(
            restore_next_step(&beads_dir, &custom_target),
            format!(
                "BEADS_JSONL={} br sync --import-only --force",
                shell_quote(&custom_target.display().to_string())
            )
        );
    }

    #[test]
    fn test_restore_next_step_requires_external_flag_for_external_targets() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let external_target = temp.path().join("external").join("issues.jsonl");

        assert_eq!(
            restore_next_step(&beads_dir, &external_target),
            format!(
                "BEADS_JSONL={} br sync --import-only --force --allow-external-jsonl",
                shell_quote(&external_target.display().to_string())
            )
        );
    }

    #[test]
    fn test_shell_quote_escapes_single_quotes() {
        assert_eq!(
            shell_quote("/tmp/issue's.jsonl"),
            r#"'/tmp/issue'"'"'s.jsonl'"#
        );
    }

    #[test]
    fn test_restore_backup_recreates_missing_target_parent_directories() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let history_dir = beads_dir.join(".br_history");
        let nested_target = beads_dir.join("snapshots").join("issues.jsonl");
        fs::create_dir_all(&beads_dir).unwrap();
        fs::create_dir_all(nested_target.parent().unwrap()).unwrap();
        fs::write(&nested_target, "nested-state\n").unwrap();

        let config = history::HistoryConfig {
            enabled: true,
            max_count: 10,
            max_age_days: 30,
            min_interval_secs: 0,
        };
        history::backup_before_export(&beads_dir, &config, &nested_target).unwrap();

        fs::remove_dir_all(nested_target.parent().unwrap()).unwrap();

        let backup_name = history::list_backups(&history_dir, None)
            .unwrap()
            .into_iter()
            .next()
            .and_then(|entry| {
                entry
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .expect("backup filename");

        let ctx = OutputContext::from_flags(false, true, true);
        restore_backup(&beads_dir, &history_dir, &backup_name, true, None, &ctx).unwrap();

        assert_eq!(
            fs::read_to_string(&nested_target).unwrap(),
            "nested-state\n"
        );
    }

    #[test]
    fn test_restore_backup_skips_stale_regular_temp_file() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let history_dir = beads_dir.join(".br_history");
        fs::create_dir_all(&beads_dir).unwrap();

        let target_path = beads_dir.join("issues.jsonl");
        fs::write(&target_path, "backup-state\n").unwrap();
        let config = history::HistoryConfig {
            enabled: true,
            max_count: 10,
            max_age_days: 30,
            min_interval_secs: 0,
        };
        history::backup_before_export(&beads_dir, &config, &target_path).unwrap();
        fs::write(&target_path, "current-state\n").unwrap();

        let stale_temp_path = restore_temp_path_for_attempt(&target_path, 0);
        fs::write(&stale_temp_path, "stale temp\n").unwrap();

        let backup_name = history::list_backups(&history_dir, None)
            .unwrap()
            .into_iter()
            .next()
            .and_then(|entry| {
                entry
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .expect("backup filename");

        let ctx = OutputContext::from_flags(false, true, true);
        restore_backup(&beads_dir, &history_dir, &backup_name, true, None, &ctx).unwrap();

        assert_eq!(fs::read_to_string(&target_path).unwrap(), "backup-state\n");
        assert_eq!(
            fs::read_to_string(&stale_temp_path).unwrap(),
            "stale temp\n",
            "restore should not overwrite or delete a stale regular temp file"
        );
    }

    #[test]
    fn test_restore_backup_cleans_temp_file_when_rename_fails() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let history_dir = beads_dir.join(".br_history");
        fs::create_dir_all(&beads_dir).unwrap();

        let target_path = beads_dir.join("issues.jsonl");
        fs::write(&target_path, "restored-state\n").unwrap();
        let config = history::HistoryConfig {
            enabled: true,
            max_count: 10,
            max_age_days: 30,
            min_interval_secs: 0,
        };
        history::backup_before_export(&beads_dir, &config, &target_path).unwrap();

        let backup_name = history::list_backups(&history_dir, None)
            .unwrap()
            .into_iter()
            .next()
            .and_then(|entry| {
                entry
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .expect("backup filename");
        let target_dir = beads_dir.join("issues.jsonl");
        fs::remove_file(&target_dir).unwrap();
        fs::create_dir_all(&target_dir).unwrap();
        fs::write(target_dir.join("occupied.txt"), "keep").unwrap();

        let ctx = OutputContext::from_flags(false, true, true);
        let err =
            restore_backup(&beads_dir, &history_dir, &backup_name, true, None, &ctx).unwrap_err();
        assert!(
            matches!(err, BeadsError::Io(_) | BeadsError::Config(_)),
            "unexpected error: {err}"
        );
        let pid = std::process::id();
        assert!(
            !beads_dir.join(format!("issues.jsonl.{pid}.tmp")).exists(),
            "failed restore should clean up the temporary restore file"
        );
    }

    #[test]
    fn test_commit_restored_target_restores_original_file_when_replace_fails() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let target_path = beads_dir.join("issues.jsonl");
        let temp_path = beads_dir.join("issues.jsonl.100.tmp");
        fs::write(&target_path, "original-state\n").unwrap();
        fs::write(&temp_path, "restored-state\n").unwrap();

        let mut rollback_guard =
            create_restore_rollback_snapshot(&target_path, &beads_dir).unwrap();
        fs::remove_file(&target_path).unwrap();

        let err = commit_restored_target_with_rollback(
            &temp_path,
            &target_path,
            Some(&mut rollback_guard),
            |_from, _to| {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "forced failure",
                ))
            },
        )
        .unwrap_err();

        match err {
            BeadsError::Config(message) => {
                assert!(
                    message.contains("original target was restored"),
                    "unexpected message: {message}"
                );
            }
            other => assert!(
                matches!(other, BeadsError::Config(_)),
                "unexpected error: {other:?}"
            ),
        }
        assert_eq!(
            fs::read_to_string(&target_path).unwrap(),
            "original-state\n"
        );
        assert_eq!(fs::read_to_string(&temp_path).unwrap(), "restored-state\n");
        assert!(!rollback_guard.path.exists());
    }

    #[test]
    fn test_current_jsonl_path_for_backup_rejects_tampered_absolute_metadata() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let history_dir = beads_dir.join(".br_history");
        fs::create_dir_all(&history_dir).unwrap();

        let backup_name = "issues.20260220_120000.jsonl";
        let backup_path = history_dir.join(backup_name);
        fs::write(&backup_path, "backup\n").unwrap();
        fs::write(
            backup_path.with_extension("jsonl.meta.json"),
            serde_json::json!({
                "target": {
                    "kind": "absolute",
                    "path": temp.path().join("escape.txt").display().to_string(),
                }
            })
            .to_string(),
        )
        .unwrap();

        let active_jsonl_path = beads_dir.join("issues.jsonl");
        let err = current_jsonl_path_for_backup(&beads_dir, backup_name, Some(&active_jsonl_path))
            .unwrap_err();
        match err {
            BeadsError::Config(msg) => {
                assert!(
                    msg.contains(".jsonl")
                        || msg.contains("traversal")
                        || msg.contains("regular file")
                        || msg.contains("Path"),
                    "unexpected message: {msg}"
                );
            }
            other => assert!(
                matches!(other, BeadsError::Config(_)),
                "unexpected error: {other:?}"
            ),
        }
    }

    #[test]
    fn test_diff_backup_reports_missing_current_stem_file() {
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let history_dir = beads_dir.join(".br_history");
        fs::create_dir_all(&beads_dir).unwrap();

        let target_path = beads_dir.join("custom.jsonl");
        fs::write(&target_path, "backup\n").unwrap();
        let config = history::HistoryConfig {
            enabled: true,
            max_count: 10,
            max_age_days: 30,
            min_interval_secs: 0,
        };
        history::backup_before_export(&beads_dir, &config, &target_path).unwrap();
        let backup_name = history::list_backups(&history_dir, None)
            .unwrap()
            .into_iter()
            .next()
            .and_then(|entry| {
                entry
                    .path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .expect("backup filename");
        fs::remove_file(&target_path).unwrap();

        let ctx = OutputContext::from_flags(false, true, true);
        let err = diff_backup(&beads_dir, &history_dir, &backup_name, None, &ctx).unwrap_err();

        match err {
            BeadsError::Config(msg) => assert!(msg.contains("Current custom.jsonl not found")),
            other => assert!(
                matches!(other, BeadsError::Config(_)),
                "unexpected error: {other:?}"
            ),
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_restore_backup_rejects_symlinked_backup_file() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let history_dir = beads_dir.join(".br_history");
        let outside_dir = temp.path().join("outside");
        fs::create_dir_all(&history_dir).unwrap();
        fs::create_dir_all(&outside_dir).unwrap();

        let backup_name = "issues.20260220_120000.jsonl";
        let outside_backup = outside_dir.join("backup.jsonl");
        fs::write(&outside_backup, "backup\n").unwrap();
        symlink(&outside_backup, history_dir.join(backup_name)).unwrap();

        let ctx = OutputContext::from_flags(false, true, true);
        let err =
            restore_backup(&beads_dir, &history_dir, backup_name, true, None, &ctx).unwrap_err();
        match err {
            BeadsError::Config(msg) => assert!(msg.contains("must not be a symlink")),
            other => assert!(
                matches!(other, BeadsError::Config(_)),
                "unexpected error: {other:?}"
            ),
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_restore_backup_rejects_internal_target_through_symlinked_parent() {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let history_dir = beads_dir.join(".br_history");
        let outside_dir = temp.path().join("outside");
        fs::create_dir_all(&history_dir).unwrap();
        fs::create_dir_all(&outside_dir).unwrap();
        symlink(&outside_dir, beads_dir.join("linked")).unwrap();

        let backup_name = "issues.20260220_120000.jsonl";
        let backup_path = history_dir.join(backup_name);
        fs::write(&backup_path, "restored\n").unwrap();
        fs::write(
            backup_path.with_extension("jsonl.meta.json"),
            serde_json::json!({
                "target": {
                    "kind": "relative",
                    "path": "linked/issues.jsonl",
                }
            })
            .to_string(),
        )
        .unwrap();

        let ctx = OutputContext::from_flags(false, true, true);
        let err =
            restore_backup(&beads_dir, &history_dir, backup_name, true, None, &ctx).unwrap_err();

        assert!(
            matches!(err, BeadsError::Config(_)),
            "unexpected error: {err:?}"
        );
        assert!(
            !outside_dir.join("issues.jsonl").exists(),
            "restore must not write through symlinked .beads parents"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_restore_backup_rejects_missing_descendant_under_symlinked_parent_without_side_effects()
    {
        use std::os::unix::fs::symlink;

        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        let history_dir = beads_dir.join(".br_history");
        let outside_dir = temp.path().join("outside");
        fs::create_dir_all(&history_dir).unwrap();
        fs::create_dir_all(&outside_dir).unwrap();
        symlink(&outside_dir, beads_dir.join("linked")).unwrap();

        let backup_name = "issues.20260220_120000.jsonl";
        let backup_path = history_dir.join(backup_name);
        fs::write(&backup_path, "restored\n").unwrap();
        fs::write(
            backup_path.with_extension("jsonl.meta.json"),
            serde_json::json!({
                "target": {
                    "kind": "relative",
                    "path": "linked/nested/issues.jsonl",
                }
            })
            .to_string(),
        )
        .unwrap();

        let ctx = OutputContext::from_flags(false, true, true);
        let err =
            restore_backup(&beads_dir, &history_dir, backup_name, true, None, &ctx).unwrap_err();

        assert!(
            matches!(err, BeadsError::Config(_)),
            "unexpected error: {err:?}"
        );
        assert!(
            !outside_dir.join("nested").exists(),
            "restore must not create directories through symlinked .beads parents"
        );
    }

    #[test]
    fn commit_restored_target_replaces_existing_atomically() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target.jsonl");
        let temp = dir.path().join("temp.jsonl");

        fs::write(&target, "original content\n").unwrap();
        fs::write(&temp, "restored content\n").unwrap();

        let result =
            commit_restored_target_with_rollback(&temp, &target, None, crate::util::durable_rename);
        assert!(result.is_ok(), "rename over existing should succeed");
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "restored content\n",
            "target should have restored content"
        );
        assert!(!temp.exists(), "temp should be gone after rename");
    }

    #[test]
    fn commit_restored_target_rollback_on_rename_failure() {
        let dir = TempDir::new().unwrap();
        let target = dir.path().join("target.jsonl");
        let rollback_path = dir.path().join("rollback.jsonl");

        fs::write(&target, "original\n").unwrap();
        fs::write(&rollback_path, "original\n").unwrap();
        let mut rollback_guard = TempRestoreGuard::new(rollback_path.clone());

        let result = commit_restored_target_with_rollback(
            &dir.path().join("nonexistent_temp.jsonl"),
            &target,
            Some(&mut rollback_guard),
            |_from, _to| Err(io::Error::other("injected")),
        );
        assert!(result.is_err());
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "original\n",
            "target should be unchanged after failed rename"
        );
    }
}
