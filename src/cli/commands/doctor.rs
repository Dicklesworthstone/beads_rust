//! Doctor command implementation.

#![allow(clippy::option_if_let_else)]

use crate::config;
use crate::error::Result;
use crate::output::OutputContext;
use crate::sync::{
    PathValidation, scan_conflict_markers, validate_no_git_path, validate_sync_path,
};
use rich_rust::prelude::*;
use rusqlite::{Connection, OpenFlags};
use serde::Serialize;
use std::fs::{self, File};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

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
    checks: Vec<CheckResult>,
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

fn print_report(report: &DoctorReport, json: bool, ctx: &OutputContext) -> Result<()> {
    if json || ctx.is_json() {
        println!("{}", serde_json::to_string(&report)?);
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

        if !matches!(check.status, CheckStatus::Ok) {
            if let Some(details) = &check.details {
                if let Ok(details_text) = serde_json::to_string_pretty(details) {
                    for line in details_text.lines() {
                        content.append_styled("    ", theme.dimmed.clone());
                        content.append_styled(line, theme.dimmed.clone());
                        content.append("\n");
                    }
                }
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
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    let mut columns = Vec::new();
    for row in rows {
        columns.push(row?);
    }
    Ok(columns)
}

fn required_schema_checks(conn: &Connection, checks: &mut Vec<CheckResult>) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    let mut tables = Vec::new();
    for row in rows {
        tables.push(row?);
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

fn check_integrity(conn: &Connection, checks: &mut Vec<CheckResult>) -> Result<()> {
    let result: String = conn.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if result.trim().eq_ignore_ascii_case("ok") {
        push_check(
            checks,
            "sqlite.integrity_check",
            CheckStatus::Ok,
            None,
            None,
        );
    } else {
        push_check(
            checks,
            "sqlite.integrity_check",
            CheckStatus::Error,
            Some(result),
            None,
        );
    }
    Ok(())
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

fn check_jsonl(path: &Path, checks: &mut Vec<CheckResult>) -> Result<usize> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut total = 0usize;
    let mut invalid = Vec::new();
    let mut invalid_count = 0usize;

    for (idx, line) in reader.lines().enumerate() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        total += 1;
        if serde_json::from_str::<serde_json::Value>(trimmed).is_err() {
            invalid_count += 1;
            if invalid.len() < 10 {
                invalid.push(idx + 1);
            }
        }
    }

    if invalid.is_empty() {
        push_check(
            checks,
            "jsonl.parse",
            CheckStatus::Ok,
            Some(format!("Parsed {total} records")),
            Some(serde_json::json!({
                "path": path.display().to_string(),
                "records": total
            })),
        );
    } else {
        push_check(
            checks,
            "jsonl.parse",
            CheckStatus::Error,
            Some(format!(
                "Malformed JSONL lines: {invalid_count} (first: {invalid:?})"
            )),
            Some(serde_json::json!({
                "path": path.display().to_string(),
                "records": total,
                "invalid_lines": invalid,
                "invalid_count": invalid_count
            })),
        );
    }

    Ok(total)
}

fn check_db_count(
    conn: &Connection,
    jsonl_count: Option<usize>,
    checks: &mut Vec<CheckResult>,
) -> Result<()> {
    let db_count: i64 = conn.query_row(
        "SELECT count(*) FROM issues WHERE (ephemeral = 0 OR ephemeral IS NULL) AND id NOT LIKE '%-wisp-%'",
        [],
        |row| row.get(0),
    )?;

    if let Some(jsonl_count) = jsonl_count {
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
    } else {
        push_check(
            checks,
            "counts.db_vs_jsonl",
            CheckStatus::Warn,
            Some("JSONL not found; cannot compare counts".to_string()),
            Some(serde_json::json!({ "db": db_count })),
        );
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
/// 2. Is within the .beads directory (or has explicit external opt-in)
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
    jsonl_path: Option<&Path>,
    checks: &mut Vec<CheckResult>,
) {
    // Get metadata
    let last_import: Option<String> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = 'last_import_time'",
            [],
            |row| row.get(0),
        )
        .ok();

    let last_export: Option<String> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = 'last_export_time'",
            [],
            |row| row.get(0),
        )
        .ok();

    let jsonl_hash: Option<String> = conn
        .query_row(
            "SELECT value FROM metadata WHERE key = 'jsonl_content_hash'",
            [],
            |row| row.get(0),
        )
        .ok();

    // Check dirty issues count
    let dirty_count: i64 = conn
        .query_row("SELECT count(*) FROM dirty_issues", [], |row| row.get(0))
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

    // Determine staleness
    let (jsonl_newer, db_newer) = if let Some(p) = jsonl_path {
        if p.exists() {
            let jsonl_mtime = fs::metadata(p).and_then(|m| m.modified()).ok();

            // JSONL is newer if it was modified after last import
            let j_newer = last_import.as_ref().is_none_or(|import_time| {
                chrono::DateTime::parse_from_rfc3339(import_time).is_ok_and(|import_ts| {
                    let import_sys_time = std::time::SystemTime::from(import_ts);
                    jsonl_mtime.is_some_and(|m| m > import_sys_time)
                })
            });

            // DB is newer if there are dirty issues
            let d_newer = dirty_count > 0;
            (j_newer, d_newer)
        } else {
            (false, dirty_count > 0)
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

/// Execute the doctor command.
///
/// # Errors
///
/// Returns an error if report serialization fails or if IO operations fail.
#[allow(clippy::too_many_lines)]
pub fn execute(json: bool, cli: &config::CliOverrides, ctx: &OutputContext) -> Result<()> {
    let mut checks = Vec::new();
    let Ok(beads_dir) = config::discover_beads_dir(None) else {
        push_check(
            &mut checks,
            "beads_dir",
            CheckStatus::Error,
            Some("Missing .beads directory (run `br init`)".to_string()),
            None,
        );
        let report = DoctorReport {
            ok: !has_error(&checks),
            checks,
        };
        print_report(&report, json, ctx)?;
        std::process::exit(1);
    };

    let paths = match config::resolve_paths(&beads_dir, cli.db.as_ref()) {
        Ok(paths) => paths,
        Err(err) => {
            push_check(
                &mut checks,
                "metadata",
                CheckStatus::Error,
                Some(format!("Failed to read metadata.json: {err}")),
                None,
            );
            let report = DoctorReport {
                ok: !has_error(&checks),
                checks,
            };
            print_report(&report, json, ctx)?;
            std::process::exit(1);
        }
    };

    check_merge_artifacts(&beads_dir, &mut checks)?;

    let jsonl_path = if paths.jsonl_path.exists() {
        Some(paths.jsonl_path.clone())
    } else {
        discover_jsonl(&beads_dir)
    };
    let jsonl_count = if let Some(path) = jsonl_path.as_ref() {
        // SYNC SAFETY CHECKS (beads_rust-0v1.2.6)
        // Check JSONL path is within sync allowlist
        check_sync_jsonl_path(path, &beads_dir, &mut checks);

        // Check for merge conflict markers
        check_sync_conflict_markers(path, &mut checks);

        match check_jsonl(path, &mut checks) {
            Ok(count) => Some(count),
            Err(err) => {
                push_check(
                    &mut checks,
                    "jsonl.parse",
                    CheckStatus::Error,
                    Some(format!("Failed to read JSONL: {err}")),
                    Some(serde_json::json!({ "path": path.display().to_string() })),
                );
                None
            }
        }
    } else {
        push_check(
            &mut checks,
            "jsonl.parse",
            CheckStatus::Warn,
            Some("No JSONL file found (.beads/issues.jsonl or .beads/beads.jsonl)".to_string()),
            None,
        );
        None
    };

    let db_path = paths.db_path;
    if db_path.exists() {
        match Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(conn) => {
                required_schema_checks(&conn, &mut checks)?;
                check_integrity(&conn, &mut checks)?;
                check_db_count(&conn, jsonl_count, &mut checks)?;

                // SYNC SAFETY CHECK: metadata consistency (beads_rust-0v1.2.6)
                check_sync_metadata(&conn, Some(&paths.jsonl_path), &mut checks);
            }
            Err(err) => {
                push_check(
                    &mut checks,
                    "db.open",
                    CheckStatus::Error,
                    Some(format!("Failed to open DB read-only: {err}")),
                    Some(serde_json::json!({ "path": db_path.display().to_string() })),
                );
            }
        }
    } else {
        push_check(
            &mut checks,
            "db.exists",
            CheckStatus::Error,
            Some(format!("Missing database file at {}", db_path.display())),
            Some(serde_json::json!({ "path": db_path.display().to_string() })),
        );
    }

    let report = DoctorReport {
        ok: !has_error(&checks),
        checks,
    };
    print_report(&report, json, ctx)?;

    if !report.ok {
        std::process::exit(1);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use tempfile::NamedTempFile;

    fn find_check<'a>(checks: &'a [CheckResult], name: &str) -> Option<&'a CheckResult> {
        checks.iter().find(|check| check.name == name)
    }

    #[test]
    fn test_check_jsonl_detects_malformed() -> Result<()> {
        let mut file = NamedTempFile::new().unwrap();
        std::io::Write::write_all(file.as_file_mut(), b"{\"id\":\"ok\"}\n")?;
        std::io::Write::write_all(file.as_file_mut(), b"{bad json}\n")?;

        let mut checks = Vec::new();
        let count = check_jsonl(file.path(), &mut checks).unwrap();
        assert_eq!(count, 2);

        let check = find_check(&checks, "jsonl.parse").expect("check present");
        assert!(matches!(check.status, CheckStatus::Error));

        Ok(())
    }

    #[test]
    fn test_required_schema_checks_missing_tables() {
        let conn = Connection::open_in_memory().unwrap();
        let mut checks = Vec::new();
        required_schema_checks(&conn, &mut checks).unwrap();

        let tables = find_check(&checks, "schema.tables").expect("tables check");
        assert!(matches!(tables.status, CheckStatus::Error));
    }
}
