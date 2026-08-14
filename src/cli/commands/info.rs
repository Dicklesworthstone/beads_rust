//! Info command implementation.

use crate::cli::InfoArgs;
use crate::config;
use crate::error::Result;
use crate::format::sanitize_terminal_inline;
use crate::franken_sync::Connection;
use crate::franken_sync::compat::{OpenFlags, open_with_flags};
use crate::output::{OutputContext, OutputMode};
use crate::storage::SqliteStorage;
use crate::util::parse_id;
use fsqlite_types::SqliteValue;
use rich_rust::prelude::*;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::debug;

#[derive(Serialize)]
struct SchemaInfo {
    tables: Vec<String>,
    schema_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    config: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    sample_issue_ids: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    detected_prefix: Option<String>,
}

#[derive(Serialize)]
struct ProjectionInfo {
    schema_version: String,
    blocked_cache_state: String,
    blocked_cache_stale: bool,
    parity_status: String,
    ready_parity_status: String,
    rebuild_needed: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    rebuild_reasons: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    issue_rows: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dependency_rows: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cached_blocked_rows: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    direct_blocked_rows: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cached_missing_rows: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cached_extra_rows: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cached_mismatched_rows: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cached_ready_rows: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    direct_ready_rows: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cached_ready_missing_rows: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cached_ready_extra_rows: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    child_counter_rows: Option<usize>,
}

#[derive(Serialize)]
struct InfoOutput {
    database_path: String,
    beads_dir: String,
    mode: String,
    daemon_connected: bool,
    #[serde(skip)]
    resolved_prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    daemon_fallback_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    daemon_detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    issue_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    config: Option<HashMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    schema: Option<SchemaInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    projections: Option<ProjectionInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    db_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    jsonl_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    jsonl_size: Option<u64>,
}

#[derive(Default)]
struct InfoSnapshot {
    issue_count: Option<usize>,
    config_map: Option<HashMap<String, String>>,
    detected_prefix: Option<String>,
    schema: Option<SchemaInfo>,
    projections: Option<ProjectionInfo>,
}

/// Execute the info command.
///
/// # Errors
///
/// Returns an error if configuration or storage access fails.
pub fn execute(args: &InfoArgs, cli: &config::CliOverrides, ctx: &OutputContext) -> Result<()> {
    if args.whats_new {
        return print_message(ctx, "No whats-new data available for br.", "whats_new");
    }
    if args.thanks {
        return print_message(
            ctx,
            "Thanks for using br. See README for project acknowledgements.",
            "thanks",
        );
    }

    let output = collect_info_output(args, cli)?;

    if matches!(ctx.mode(), OutputMode::Quiet) {
        return Ok(());
    }

    if ctx.is_json() {
        ctx.json_pretty(&output);
        return Ok(());
    }

    if ctx.is_toon() {
        ctx.toon(&output);
        return Ok(());
    }

    if ctx.is_rich() {
        render_info_rich(&output, ctx);
    } else {
        print_human(&output);
    }

    Ok(())
}

fn collect_info_output(args: &InfoArgs, cli: &config::CliOverrides) -> Result<InfoOutput> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let startup = config::load_startup_config_with_paths(&beads_dir, cli.db.as_ref())?;
    let snapshot = load_info_snapshot_without_recovery(args, &startup.paths);
    let resolved_prefix = config::configured_issue_prefix_from_map(&startup.merged_config.runtime)
        .or_else(|| snapshot.detected_prefix.clone())
        .or_else(|| {
            config::first_prefix_from_jsonl(&startup.paths.jsonl_path)
                .ok()
                .flatten()
        });

    let db_path = canonicalize_lossy(&startup.paths.db_path);
    let db_size = std::fs::metadata(&startup.paths.db_path)
        .map(|metadata| metadata.len())
        .ok();
    let jsonl_size = std::fs::metadata(&startup.paths.jsonl_path)
        .map(|metadata| metadata.len())
        .ok();

    Ok(InfoOutput {
        database_path: db_path.display().to_string(),
        beads_dir: canonicalize_lossy(&beads_dir).display().to_string(),
        mode: "direct".to_string(),
        daemon_connected: false,
        resolved_prefix,
        daemon_fallback_reason: Some("no-daemon".to_string()),
        daemon_detail: Some("br runs in direct mode only".to_string()),
        issue_count: snapshot.issue_count,
        config: snapshot.config_map,
        schema: snapshot.schema,
        projections: snapshot.projections,
        db_size,
        jsonl_path: Some(
            canonicalize_lossy(&startup.paths.jsonl_path)
                .display()
                .to_string(),
        ),
        jsonl_size,
    })
}

fn load_info_snapshot_without_recovery(
    args: &InfoArgs,
    paths: &config::ConfigPaths,
) -> InfoSnapshot {
    if !paths.db_path.is_file() {
        return InfoSnapshot::default();
    }

    if !db_path_is_symlink(&paths.db_path) {
        match load_info_snapshot_direct_read_only(args, &paths.db_path) {
            Ok(snapshot) => return snapshot,
            Err(err) => {
                debug!(
                    path = %paths.db_path.display(),
                    error = %err,
                    "Direct read-only info query failed; falling back to database-family snapshot"
                );
            }
        }
    }

    match config::with_database_family_snapshot(&paths.db_path, |snapshot_db_path| {
        let conn = Connection::open(snapshot_db_path.to_string_lossy().into_owned())?;
        let snapshot = collect_info_snapshot(args, &conn);
        conn.close()?;
        Ok(snapshot)
    }) {
        Ok(snapshot) => snapshot,
        Err(err) => {
            debug!(
                path = %paths.db_path.display(),
                error = %err,
                "Skipping DB-backed info details because the database could not be snapshotted for read-only access"
            );
            InfoSnapshot::default()
        }
    }
}

fn db_path_is_symlink(db_path: &Path) -> bool {
    std::fs::symlink_metadata(db_path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
}

fn load_info_snapshot_direct_read_only(args: &InfoArgs, db_path: &Path) -> Result<InfoSnapshot> {
    let conn = open_with_flags(
        db_path.to_string_lossy().as_ref(),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )?;
    let snapshot = collect_info_snapshot(args, &conn);
    conn.close()?;
    Ok(snapshot)
}

fn collect_info_snapshot(args: &InfoArgs, conn: &Connection) -> InfoSnapshot {
    let issue_count = query_issue_count(conn);
    let config_map = load_config_map(conn);
    let detected_prefix = detect_prefix(conn, config_map.as_ref());
    let schema = if args.schema {
        Some(build_schema_info(
            conn,
            config_map.as_ref(),
            detected_prefix.clone(),
        ))
    } else {
        None
    };
    let projections = args.projections.then(|| build_projection_info(conn));

    InfoSnapshot {
        issue_count,
        config_map,
        detected_prefix,
        schema,
        projections,
    }
}

fn query_issue_count(conn: &Connection) -> Option<usize> {
    conn.query_row("SELECT COUNT(*) FROM issues")
        .ok()
        .and_then(|row| row.get(0).and_then(SqliteValue::as_integer))
        .and_then(|count| usize::try_from(count).ok())
}

fn load_config_map(conn: &Connection) -> Option<HashMap<String, String>> {
    let rows = conn.query("SELECT key, value FROM config").ok()?;
    let mut config_map = HashMap::new();

    for row in rows {
        let Some(key) = row.get(0).and_then(SqliteValue::as_text) else {
            continue;
        };
        let Some(value) = row.get(1).and_then(SqliteValue::as_text) else {
            continue;
        };
        config_map.insert(key.to_string(), value.to_string());
    }

    (!config_map.is_empty()).then_some(config_map)
}

fn build_projection_info(conn: &Connection) -> ProjectionInfo {
    let blocked_cache_state = metadata_value(conn, "blocked_cache_state")
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "fresh".to_string());
    let blocked_cache_stale = blocked_cache_state == "stale";
    let cached_blocked_rows = projection_row_count(conn, "blocked_issues_cache");
    let child_counter_rows = projection_row_count(conn, "child_counters");
    let projection_health = SqliteStorage::blocked_cache_projection_health(conn);
    let ready_health = SqliteStorage::ready_projection_health(conn);

    let mut rebuild_reasons = Vec::new();
    if blocked_cache_stale {
        rebuild_reasons.push("blocked_cache_marked_stale".to_string());
    }
    if projection_health.has_mismatch() {
        rebuild_reasons.push("blocked_cache_content_mismatch".to_string());
    }
    if ready_health.has_mismatch() {
        rebuild_reasons.push("ready_projection_content_mismatch".to_string());
    }
    if cached_blocked_rows.is_none() {
        rebuild_reasons.push("blocked_issues_cache_missing".to_string());
    }
    if child_counter_rows.is_none() {
        rebuild_reasons.push("child_counters_missing".to_string());
    }

    ProjectionInfo {
        schema_version: "br.graph-projections.v1".to_string(),
        blocked_cache_state,
        blocked_cache_stale,
        parity_status: combined_projection_parity_status(
            &projection_health.parity_status,
            &ready_health.parity_status,
        ),
        ready_parity_status: ready_health.parity_status,
        rebuild_needed: !rebuild_reasons.is_empty(),
        rebuild_reasons,
        issue_rows: query_issue_count(conn),
        dependency_rows: projection_row_count(conn, "dependencies"),
        cached_blocked_rows,
        direct_blocked_rows: projection_health.direct_blocked_rows,
        cached_missing_rows: projection_health.cached_missing_rows,
        cached_extra_rows: projection_health.cached_extra_rows,
        cached_mismatched_rows: projection_health.cached_mismatched_rows,
        cached_ready_rows: ready_health.cached_ready_rows,
        direct_ready_rows: ready_health.direct_ready_rows,
        cached_ready_missing_rows: ready_health.cached_ready_missing_rows,
        cached_ready_extra_rows: ready_health.cached_ready_extra_rows,
        child_counter_rows,
    }
}

fn combined_projection_parity_status(blocked_status: &str, ready_status: &str) -> String {
    let statuses = [blocked_status, ready_status];
    if statuses.contains(&"mismatch") {
        "mismatch".to_string()
    } else if statuses.contains(&"unavailable") {
        "unavailable".to_string()
    } else {
        "matches".to_string()
    }
}

fn metadata_value(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row_with_params(
        "SELECT value FROM metadata WHERE key = ? ORDER BY rowid DESC LIMIT 1",
        &[SqliteValue::from(key)],
    )
    .ok()
    .and_then(|row| {
        row.get(0)
            .and_then(SqliteValue::as_text)
            .map(str::to_string)
    })
}

fn projection_row_count(conn: &Connection, table: &str) -> Option<usize> {
    let sql = match table {
        "blocked_issues_cache" => "SELECT COUNT(*) FROM blocked_issues_cache",
        "child_counters" => "SELECT COUNT(*) FROM child_counters",
        "dependencies" => "SELECT COUNT(*) FROM dependencies",
        _ => return None,
    };

    conn.query_row(sql)
        .ok()
        .and_then(|row| row.get(0).and_then(SqliteValue::as_integer))
        .and_then(|count| usize::try_from(count).ok())
}

fn build_schema_info(
    conn: &Connection,
    config_map: Option<&HashMap<String, String>>,
    detected_prefix: Option<String>,
) -> SchemaInfo {
    let tables = actual_table_names(conn);
    let sample_issue_ids: Vec<String> = conn
        .query("SELECT id FROM issues ORDER BY id LIMIT 3")
        .ok()
        .map(|rows| {
            rows.into_iter()
                .filter_map(|row| {
                    row.get(0)
                        .and_then(SqliteValue::as_text)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default();

    SchemaInfo {
        tables,
        schema_version: actual_schema_version(conn),
        config: config_map.cloned(),
        sample_issue_ids,
        detected_prefix,
    }
}

fn detect_prefix(
    conn: &Connection,
    config_map: Option<&HashMap<String, String>>,
) -> Option<String> {
    config_map
        .and_then(config::configured_issue_prefix_from_map)
        .or_else(|| {
            conn.query("SELECT id FROM issues ORDER BY id LIMIT 1")
                .ok()
                .and_then(|rows| rows.first().cloned())
                .and_then(|row| {
                    row.get(0)
                        .and_then(SqliteValue::as_text)
                        .map(str::to_string)
                })
                .and_then(|id| parse_id(&id).ok().map(|parsed| parsed.prefix))
        })
}

fn actual_table_names(conn: &Connection) -> Vec<String> {
    conn.query(
        "SELECT name FROM sqlite_master \
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
         ORDER BY name",
    )
    .ok()
    .map(|rows| {
        rows.into_iter()
            .filter_map(|row| {
                row.get(0)
                    .and_then(SqliteValue::as_text)
                    .map(str::to_string)
            })
            .collect()
    })
    .unwrap_or_default()
}

fn actual_schema_version(conn: &Connection) -> String {
    conn.query_row("PRAGMA user_version")
        .ok()
        .and_then(|row| row.get(0).and_then(SqliteValue::as_integer))
        .map_or_else(|| "unknown".to_string(), |version| version.to_string())
}

fn print_human(info: &InfoOutput) {
    println!("Beads Database Information");
    println!("Beads dir: {}", info_display_text(&info.beads_dir));
    println!("Database: {}", info_display_text(&info.database_path));
    if let Some(size) = info.db_size {
        println!("Database size: {}", format_bytes(size));
    }
    if let Some(jsonl_path) = &info.jsonl_path {
        println!("JSONL: {}", info_display_text(jsonl_path));
        if let Some(size) = info.jsonl_size {
            println!("JSONL size: {}", format_bytes(size));
        }
    }
    println!("Mode: {}", info_display_text(&info.mode));

    if info.daemon_connected {
        println!("Daemon: connected");
    } else if let Some(reason) = &info.daemon_fallback_reason {
        println!("Daemon: not connected ({})", info_display_text(reason));
        if let Some(detail) = &info.daemon_detail {
            println!("  {}", info_display_text(detail));
        }
    }

    if let Some(count) = info.issue_count {
        println!("Issue count: {count}");
    }

    if let Some(prefix) = info.resolved_prefix.as_deref() {
        println!("Issue prefix: {}", info_display_text(prefix));
    }

    if let Some(schema) = &info.schema {
        println!();
        println!("Schema:");
        println!("  Version: {}", info_display_text(&schema.schema_version));
        println!(
            "  Tables: {}",
            info_display_list(schema.tables.iter().map(String::as_str))
        );
        if let Some(prefix) = &schema.detected_prefix {
            println!("  Detected prefix: {}", info_display_text(prefix));
        }
        if !schema.sample_issue_ids.is_empty() {
            println!(
                "  Sample IDs: {}",
                info_display_list(schema.sample_issue_ids.iter().map(String::as_str))
            );
        }
    }

    if let Some(projections) = &info.projections {
        print_projection_human(projections);
    }
}

fn print_projection_human(projections: &ProjectionInfo) {
    println!();
    println!("Graph projections:");
    println!(
        "  Blocked cache: {}",
        info_display_text(&projections.blocked_cache_state)
    );
    println!(
        "  Rebuild needed: {}",
        if projections.rebuild_needed {
            "yes"
        } else {
            "no"
        }
    );
    println!(
        "  Cache parity: {}",
        info_display_text(&projections.parity_status)
    );
    println!(
        "  Ready parity: {}",
        info_display_text(&projections.ready_parity_status)
    );
    if let Some(count) = projections.cached_blocked_rows {
        println!("  Cached blocked rows: {count}");
    }
    if let Some(count) = projections.direct_blocked_rows {
        println!("  Direct blocked rows: {count}");
    }
    if let Some(count) = projections.cached_missing_rows {
        println!("  Missing cache rows: {count}");
    }
    if let Some(count) = projections.cached_extra_rows {
        println!("  Extra cache rows: {count}");
    }
    if let Some(count) = projections.cached_mismatched_rows {
        println!("  Mismatched cache rows: {count}");
    }
    if let Some(count) = projections.cached_ready_rows {
        println!("  Cached ready rows: {count}");
    }
    if let Some(count) = projections.direct_ready_rows {
        println!("  Direct ready rows: {count}");
    }
    if let Some(count) = projections.cached_ready_missing_rows {
        println!("  Missing ready rows: {count}");
    }
    if let Some(count) = projections.cached_ready_extra_rows {
        println!("  Extra ready rows: {count}");
    }
    if let Some(count) = projections.child_counter_rows {
        println!("  Child counter rows: {count}");
    }
    if !projections.rebuild_reasons.is_empty() {
        println!(
            "  Rebuild reasons: {}",
            info_display_list(projections.rebuild_reasons.iter().map(String::as_str))
        );
    }
}

#[allow(clippy::unnecessary_wraps)]
fn print_message(ctx: &OutputContext, message: &str, key: &str) -> Result<()> {
    if ctx.is_json() {
        let payload = serde_json::json!({ key: message });
        ctx.json_pretty(&payload);
    } else if ctx.is_toon() {
        let payload = serde_json::json!({ key: message });
        ctx.toon(&payload);
    } else if matches!(ctx.mode(), OutputMode::Quiet) {
        return Ok(());
    } else if ctx.is_rich() {
        let console = Console::default();
        let theme = ctx.theme();
        let text = Text::styled(message, theme.muted.clone());
        console.print_renderable(&text);
    } else {
        println!("{message}");
    }
    Ok(())
}

/// Render project info as a rich panel.
fn render_info_rich(info: &InfoOutput, ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");

    // Location section
    content.append_styled("Location    ", theme.dimmed.clone());
    content.append_styled(&info_display_text(&info.beads_dir), theme.accent.clone());
    content.append("\n");

    // Prefix (if available)
    if let Some(prefix) = info.resolved_prefix.as_deref() {
        content.append_styled("Prefix      ", theme.dimmed.clone());
        content.append_styled(&info_display_text(prefix), theme.issue_id.clone());
        content.append("\n");
    }

    content.append("\n");

    // Database section
    content.append_styled("Database\n", theme.section.clone());
    content.append_styled("  Path      ", theme.dimmed.clone());
    content.append_styled(
        &info_display_text(&info.database_path),
        theme.accent.clone(),
    );
    content.append("\n");

    if let Some(size) = info.db_size {
        content.append_styled("  Size      ", theme.dimmed.clone());
        content.append(&format_bytes(size));
        content.append("\n");
    }

    if let Some(count) = info.issue_count {
        content.append_styled("  Issues    ", theme.dimmed.clone());
        content.append_styled(&format!("{count}"), theme.emphasis.clone());
        content.append_styled(" total\n", theme.dimmed.clone());
    }

    // JSONL section
    if let Some(jsonl_path) = &info.jsonl_path {
        content.append("\n");
        content.append_styled("JSONL\n", theme.section.clone());
        content.append_styled("  Path      ", theme.dimmed.clone());
        content.append_styled(&info_display_text(jsonl_path), theme.accent.clone());
        content.append("\n");

        if let Some(size) = info.jsonl_size {
            content.append_styled("  Size      ", theme.dimmed.clone());
            content.append(&format_bytes(size));
            content.append("\n");
        }
    }

    // Mode section
    content.append("\n");
    content.append_styled("Mode        ", theme.dimmed.clone());
    content.append(&info_display_text(&info.mode));
    if !info.daemon_connected {
        content.append_styled(" (no daemon)", theme.muted.clone());
    }
    content.append("\n");

    // Schema section (if requested)
    if let Some(schema) = &info.schema {
        content.append("\n");
        content.append_styled("Schema\n", theme.section.clone());
        content.append_styled("  Version   ", theme.dimmed.clone());
        content.append(&info_display_text(&schema.schema_version));
        content.append("\n");

        content.append_styled("  Tables    ", theme.dimmed.clone());
        content.append(&info_display_list(schema.tables.iter().map(String::as_str)));
        content.append("\n");

        if let Some(prefix) = &schema.detected_prefix {
            content.append_styled("  Prefix    ", theme.dimmed.clone());
            content.append_styled(&info_display_text(prefix), theme.issue_id.clone());
            content.append("\n");
        }

        if !schema.sample_issue_ids.is_empty() {
            content.append_styled("  Samples   ", theme.dimmed.clone());
            content.append(&info_display_list(
                schema.sample_issue_ids.iter().map(String::as_str),
            ));
            content.append("\n");
        }
    }

    if let Some(projections) = &info.projections {
        append_projection_rich(&mut content, projections, ctx);
    }

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled(
            "Project Information",
            theme.panel_title.clone(),
        ))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

fn append_projection_rich(content: &mut Text, projections: &ProjectionInfo, ctx: &OutputContext) {
    let theme = ctx.theme();
    content.append("\n");
    content.append_styled("Graph projections\n", theme.section.clone());
    content.append_styled("  Blocked  ", theme.dimmed.clone());
    content.append(&info_display_text(&projections.blocked_cache_state));
    content.append("\n");
    content.append_styled("  Rebuild  ", theme.dimmed.clone());
    content.append(if projections.rebuild_needed {
        "needed"
    } else {
        "not needed"
    });
    content.append("\n");
    content.append_styled("  Parity   ", theme.dimmed.clone());
    content.append(&info_display_text(&projections.parity_status));
    content.append("\n");
    content.append_styled("  Ready    ", theme.dimmed.clone());
    content.append(&info_display_text(&projections.ready_parity_status));
    content.append(" parity\n");
    if let Some(count) = projections.cached_blocked_rows {
        content.append_styled("  Cached   ", theme.dimmed.clone());
        content.append(&count.to_string());
        content.append(" blocked rows\n");
    }
    if let Some(count) = projections.direct_blocked_rows {
        content.append_styled("  Direct   ", theme.dimmed.clone());
        content.append(&count.to_string());
        content.append(" blocked rows\n");
    }
    if let Some(count) = projections.cached_ready_rows {
        content.append_styled("  Ready    ", theme.dimmed.clone());
        content.append(&count.to_string());
        content.append(" cached rows\n");
    }
    if let Some(count) = projections.direct_ready_rows {
        content.append_styled("  Direct   ", theme.dimmed.clone());
        content.append(&count.to_string());
        content.append(" ready rows\n");
    }
    if let Some(count) = projections.child_counter_rows {
        content.append_styled("  Children ", theme.dimmed.clone());
        content.append(&count.to_string());
        content.append(" counter rows\n");
    }
}

/// Format bytes as human-readable size.
fn format_bytes(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn canonicalize_lossy(path: &Path) -> PathBuf {
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn info_display_text(value: &str) -> String {
    sanitize_terminal_inline(value).into_owned()
}

fn info_display_list<'a>(values: impl IntoIterator<Item = &'a str>) -> String {
    values
        .into_iter()
        .map(info_display_text)
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::InfoArgs;
    use crate::config::CliOverrides;
    use crate::storage::SqliteStorage;
    use crate::storage::schema::CURRENT_SCHEMA_VERSION;
    use std::env;
    use std::path::Path;
    use std::path::PathBuf;

    use tempfile::TempDir;

    struct DirGuard {
        previous: PathBuf,
    }

    impl DirGuard {
        fn new(target: &Path) -> Self {
            let previous = env::current_dir().unwrap_or_else(|_| PathBuf::from("/tmp"));
            env::set_current_dir(target).expect("set current dir");
            Self { previous }
        }
    }

    impl Drop for DirGuard {
        fn drop(&mut self) {
            let _ = env::set_current_dir(&self.previous);
        }
    }

    #[test]
    fn test_format_bytes_small() {
        assert_eq!(format_bytes(500), "500 B");
        assert_eq!(format_bytes(0), "0 B");
    }

    #[test]
    fn test_format_bytes_kb() {
        assert_eq!(format_bytes(1024), "1.0 KB");
        assert_eq!(format_bytes(1536), "1.5 KB");
    }

    #[test]
    fn test_format_bytes_mb() {
        assert_eq!(format_bytes(1024 * 1024), "1.0 MB");
        assert_eq!(format_bytes(2_500_000), "2.4 MB");
    }

    #[test]
    fn test_format_bytes_gb() {
        assert_eq!(format_bytes(1024 * 1024 * 1024), "1.0 GB");
    }

    #[test]
    fn info_display_helpers_sanitize_terminal_controls() {
        let rendered = info_display_text("/tmp/bd\x1b[2J\rreset\x08\nnext\x07\u{9b}");

        assert!(!rendered.chars().any(char::is_control));
        assert!(rendered.contains("\\u{1b}[2J"));
        assert!(rendered.contains("\\r"));
        assert!(rendered.contains("\\u{8}"));
        assert!(rendered.contains("\\n"));
        assert!(rendered.contains("\\u{7}"));
        assert!(rendered.contains("\\u{9b}"));

        let rendered_list = info_display_list(["issues", "bad\x1b[2Jtable"]);
        assert!(!rendered_list.chars().any(char::is_control));
        assert!(rendered_list.contains("issues, bad\\u{1b}[2Jtable"));
    }

    #[test]
    fn test_collect_info_output_does_not_create_missing_db() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        std::fs::write(
            beads_dir.join("metadata.json"),
            r#"{"database":"beads.db","jsonl_export":"issues.jsonl"}"#,
        )
        .unwrap();

        let _guard = DirGuard::new(temp.path());

        let output = collect_info_output(&InfoArgs::default(), &CliOverrides::default()).unwrap();

        assert!(
            !beads_dir.join("beads.db").exists(),
            "info collection should not create a missing database"
        );
        assert!(output.issue_count.is_none());
        assert_eq!(
            output.database_path,
            beads_dir.join("beads.db").display().to_string()
        );
    }

    #[test]
    fn test_collect_info_output_reads_existing_db_without_recovery() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        std::fs::write(
            beads_dir.join("metadata.json"),
            r#"{"database":"beads.db","jsonl_export":"issues.jsonl"}"#,
        )
        .unwrap();

        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        storage.set_config("issue_prefix", "bd").unwrap();
        let issue = crate::model::Issue {
            id: "bd-abc12".to_string(),
            title: "Example".to_string(),
            issue_type: crate::model::IssueType::Task,
            priority: crate::model::Priority::LOW,
            ..crate::model::Issue::default()
        };
        storage.create_issue(&issue, "test").unwrap();

        let _guard = DirGuard::new(temp.path());

        let output = collect_info_output(
            &InfoArgs {
                schema: true,
                ..InfoArgs::default()
            },
            &CliOverrides::default(),
        )
        .unwrap();

        assert_eq!(output.issue_count, Some(1));
        assert_eq!(
            output
                .config
                .as_ref()
                .and_then(|config| config.get("issue_prefix"))
                .map(String::as_str),
            Some("bd")
        );
        let expected_version = CURRENT_SCHEMA_VERSION.to_string();
        assert_eq!(
            output
                .schema
                .as_ref()
                .map(|schema| schema.schema_version.as_str()),
            Some(expected_version.as_str())
        );
        let expected_tables = vec![
            "blocked_issues_cache".to_string(),
            "capacity_exemption_history".to_string(),
            "capacity_exemptions".to_string(),
            "capacity_occupancy".to_string(),
            "child_counters".to_string(),
            "close_metadata".to_string(),
            "comments".to_string(),
            "config".to_string(),
            "dependencies".to_string(),
            "dirty_issues".to_string(),
            "events".to_string(),
            "export_hashes".to_string(),
            "gate_result_history".to_string(),
            "gate_results".to_string(),
            "issues".to_string(),
            "labels".to_string(),
            "metadata".to_string(),
        ];
        assert_eq!(
            output
                .schema
                .as_ref()
                .map(|schema| schema.tables.as_slice()),
            Some(expected_tables.as_slice())
        );
        assert_eq!(
            output
                .schema
                .as_ref()
                .and_then(|schema| schema.detected_prefix.as_deref()),
            Some("bd")
        );
    }

    #[test]
    fn test_collect_info_output_reports_fresh_projection_health() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        std::fs::write(
            beads_dir.join("metadata.json"),
            r#"{"database":"beads.db","jsonl_export":"issues.jsonl"}"#,
        )
        .unwrap();

        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let blocker = crate::model::Issue {
            id: "bd-blocker".to_string(),
            title: "Blocker".to_string(),
            issue_type: crate::model::IssueType::Task,
            priority: crate::model::Priority::HIGH,
            ..crate::model::Issue::default()
        };
        let target = crate::model::Issue {
            id: "bd-target".to_string(),
            title: "Target".to_string(),
            issue_type: crate::model::IssueType::Task,
            priority: crate::model::Priority::LOW,
            ..crate::model::Issue::default()
        };
        storage.create_issue(&blocker, "test").unwrap();
        storage.create_issue(&target, "test").unwrap();
        storage
            .add_dependency(
                &target.id,
                &blocker.id,
                crate::model::DependencyType::Blocks.as_str(),
                "test",
            )
            .unwrap();
        assert!(storage.ensure_blocked_cache_fresh().unwrap());
        drop(storage);

        let _guard = DirGuard::new(temp.path());
        let output = collect_info_output(
            &InfoArgs {
                projections: true,
                ..InfoArgs::default()
            },
            &CliOverrides::default(),
        )
        .unwrap();
        let projections = output.projections.as_ref().unwrap();

        assert_eq!(projections.schema_version, "br.graph-projections.v1");
        assert_eq!(projections.blocked_cache_state, "fresh");
        assert!(!projections.blocked_cache_stale);
        assert_eq!(projections.parity_status, "matches");
        assert_eq!(projections.ready_parity_status, "matches");
        assert!(!projections.rebuild_needed);
        assert_eq!(projections.issue_rows, Some(2));
        assert_eq!(projections.dependency_rows, Some(1));
        assert_eq!(projections.cached_blocked_rows, Some(1));
        assert_eq!(projections.direct_blocked_rows, Some(1));
        assert_eq!(projections.cached_missing_rows, Some(0));
        assert_eq!(projections.cached_extra_rows, Some(0));
        assert_eq!(projections.cached_mismatched_rows, Some(0));
        assert_eq!(projections.cached_ready_rows, Some(1));
        assert_eq!(projections.direct_ready_rows, Some(1));
        assert_eq!(projections.cached_ready_missing_rows, Some(0));
        assert_eq!(projections.cached_ready_extra_rows, Some(0));
    }

    #[test]
    fn test_collect_info_output_reports_stale_projection_rebuild_needed() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        std::fs::write(
            beads_dir.join("metadata.json"),
            r#"{"database":"beads.db","jsonl_export":"issues.jsonl"}"#,
        )
        .unwrap();

        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        storage.mark_blocked_cache_stale().unwrap();
        drop(storage);

        let _guard = DirGuard::new(temp.path());
        let output = collect_info_output(
            &InfoArgs {
                projections: true,
                ..InfoArgs::default()
            },
            &CliOverrides::default(),
        )
        .unwrap();
        let projections = output.projections.as_ref().unwrap();

        assert_eq!(projections.blocked_cache_state, "stale");
        assert!(projections.blocked_cache_stale);
        assert_eq!(projections.parity_status, "matches");
        assert_eq!(projections.ready_parity_status, "matches");
        assert!(projections.rebuild_needed);
        assert_eq!(
            projections.rebuild_reasons,
            vec!["blocked_cache_marked_stale".to_string()]
        );
    }

    #[test]
    fn test_collect_info_output_reports_projection_parity_mismatch() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        std::fs::write(
            beads_dir.join("metadata.json"),
            r#"{"database":"beads.db","jsonl_export":"issues.jsonl"}"#,
        )
        .unwrap();

        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let blocker = crate::model::Issue {
            id: "bd-real-blocker".to_string(),
            title: "Real blocker".to_string(),
            issue_type: crate::model::IssueType::Task,
            priority: crate::model::Priority::HIGH,
            ..crate::model::Issue::default()
        };
        let target = crate::model::Issue {
            id: "bd-parity-target".to_string(),
            title: "Target".to_string(),
            issue_type: crate::model::IssueType::Task,
            priority: crate::model::Priority::LOW,
            ..crate::model::Issue::default()
        };
        storage.create_issue(&blocker, "test").unwrap();
        storage.create_issue(&target, "test").unwrap();
        storage
            .add_dependency(
                &target.id,
                &blocker.id,
                crate::model::DependencyType::Blocks.as_str(),
                "test",
            )
            .unwrap();
        assert!(storage.ensure_blocked_cache_fresh().unwrap());
        storage
            .execute_test_sql(
                "UPDATE blocked_issues_cache
                 SET blocked_by = '[\"bd-other:open\"]'
                 WHERE issue_id = 'bd-parity-target'",
            )
            .unwrap();
        drop(storage);

        let _guard = DirGuard::new(temp.path());
        let output = collect_info_output(
            &InfoArgs {
                projections: true,
                ..InfoArgs::default()
            },
            &CliOverrides::default(),
        )
        .unwrap();
        let projections = output.projections.as_ref().unwrap();

        assert_eq!(projections.blocked_cache_state, "fresh");
        assert_eq!(projections.parity_status, "mismatch");
        assert_eq!(projections.ready_parity_status, "matches");
        assert!(projections.rebuild_needed);
        assert_eq!(projections.direct_blocked_rows, Some(1));
        assert_eq!(projections.cached_missing_rows, Some(0));
        assert_eq!(projections.cached_extra_rows, Some(0));
        assert_eq!(projections.cached_mismatched_rows, Some(1));
        assert_eq!(projections.cached_ready_rows, Some(1));
        assert_eq!(projections.direct_ready_rows, Some(1));
        assert_eq!(projections.cached_ready_missing_rows, Some(0));
        assert_eq!(projections.cached_ready_extra_rows, Some(0));
        assert!(
            projections
                .rebuild_reasons
                .contains(&"blocked_cache_content_mismatch".to_string())
        );
    }

    #[test]
    fn test_collect_info_output_reports_ready_projection_parity_mismatch() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        std::fs::write(
            beads_dir.join("metadata.json"),
            r#"{"database":"beads.db","jsonl_export":"issues.jsonl"}"#,
        )
        .unwrap();

        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let blocker = crate::model::Issue {
            id: "bd-ready-blocker".to_string(),
            title: "Ready blocker".to_string(),
            issue_type: crate::model::IssueType::Task,
            priority: crate::model::Priority::HIGH,
            ..crate::model::Issue::default()
        };
        let target = crate::model::Issue {
            id: "bd-ready-target".to_string(),
            title: "Ready target".to_string(),
            issue_type: crate::model::IssueType::Task,
            priority: crate::model::Priority::LOW,
            ..crate::model::Issue::default()
        };
        storage.create_issue(&blocker, "test").unwrap();
        storage.create_issue(&target, "test").unwrap();
        storage
            .add_dependency(
                &target.id,
                &blocker.id,
                crate::model::DependencyType::Blocks.as_str(),
                "test",
            )
            .unwrap();
        assert!(storage.ensure_blocked_cache_fresh().unwrap());
        storage
            .execute_test_sql("DELETE FROM blocked_issues_cache WHERE issue_id = 'bd-ready-target'")
            .unwrap();
        drop(storage);

        let _guard = DirGuard::new(temp.path());
        let output = collect_info_output(
            &InfoArgs {
                projections: true,
                ..InfoArgs::default()
            },
            &CliOverrides::default(),
        )
        .unwrap();
        let projections = output.projections.as_ref().unwrap();

        assert_eq!(projections.blocked_cache_state, "fresh");
        assert_eq!(projections.parity_status, "mismatch");
        assert_eq!(projections.ready_parity_status, "mismatch");
        assert!(projections.rebuild_needed);
        assert_eq!(projections.cached_missing_rows, Some(1));
        assert_eq!(projections.cached_ready_rows, Some(2));
        assert_eq!(projections.direct_ready_rows, Some(1));
        assert_eq!(projections.cached_ready_missing_rows, Some(0));
        assert_eq!(projections.cached_ready_extra_rows, Some(1));
        assert!(
            projections
                .rebuild_reasons
                .contains(&"ready_projection_content_mismatch".to_string())
        );
    }

    #[test]
    fn test_collect_info_output_reports_actual_schema_snapshot() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        std::fs::write(
            beads_dir.join("metadata.json"),
            r#"{"database":"beads.db","jsonl_export":"issues.jsonl"}"#,
        )
        .unwrap();

        let db_path = beads_dir.join("beads.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        conn.execute("CREATE TABLE issues (id TEXT PRIMARY KEY)")
            .unwrap();
        conn.execute("PRAGMA user_version = 1").unwrap();

        let _guard = DirGuard::new(temp.path());

        let output = collect_info_output(
            &InfoArgs {
                schema: true,
                ..InfoArgs::default()
            },
            &CliOverrides::default(),
        )
        .unwrap();

        let schema = output.schema.expect("schema details");
        assert_eq!(schema.schema_version, "1");
        assert_eq!(schema.tables, vec!["issues".to_string()]);
        assert!(schema.config.is_none());
        assert!(schema.sample_issue_ids.is_empty());
    }

    #[test]
    fn test_collect_info_output_prefers_startup_issue_prefix_over_db_config() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        std::fs::write(
            beads_dir.join("metadata.json"),
            r#"{"database":"beads.db","jsonl_export":"issues.jsonl"}"#,
        )
        .unwrap();
        std::fs::write(beads_dir.join("config.yaml"), "issue_prefix: proj\n").unwrap();

        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        storage.set_config("issue_prefix", "bd").unwrap();

        let _guard = DirGuard::new(temp.path());

        let output = collect_info_output(&InfoArgs::default(), &CliOverrides::default()).unwrap();

        assert_eq!(output.resolved_prefix.as_deref(), Some("proj"));
        assert_eq!(
            output
                .config
                .as_ref()
                .and_then(|config| config.get("issue_prefix"))
                .map(String::as_str),
            Some("bd"),
            "serialized DB config should remain unchanged"
        );
    }

    #[test]
    fn test_collect_info_output_detects_prefix_without_schema_flag() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        std::fs::write(
            beads_dir.join("metadata.json"),
            r#"{"database":"beads.db","jsonl_export":"issues.jsonl"}"#,
        )
        .unwrap();

        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        storage.set_config("issue_prefix", "proj").unwrap();
        let issue = crate::model::Issue {
            id: "proj-abc12".to_string(),
            title: "Example".to_string(),
            issue_type: crate::model::IssueType::Task,
            priority: crate::model::Priority::LOW,
            ..crate::model::Issue::default()
        };
        storage.create_issue(&issue, "test").unwrap();
        storage.delete_config("issue_prefix").unwrap();

        let _guard = DirGuard::new(temp.path());

        let output = collect_info_output(&InfoArgs::default(), &CliOverrides::default()).unwrap();

        assert_eq!(output.resolved_prefix.as_deref(), Some("proj"));
        assert!(output.schema.is_none());
        assert!(
            output
                .config
                .as_ref()
                .and_then(|config| config.get("issue_prefix"))
                .is_none()
        );
    }

    #[test]
    fn test_collect_info_output_uses_jsonl_prefix_when_db_is_missing() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        std::fs::write(
            beads_dir.join("metadata.json"),
            r#"{"database":"beads.db","jsonl_export":"issues.jsonl"}"#,
        )
        .unwrap();
        std::fs::write(
            beads_dir.join("issues.jsonl"),
            r#"{"id":"proj-abc12","title":"Example"}"#,
        )
        .unwrap();

        let _guard = DirGuard::new(temp.path());

        let output = collect_info_output(&InfoArgs::default(), &CliOverrides::default()).unwrap();

        assert_eq!(output.resolved_prefix.as_deref(), Some("proj"));
        assert!(output.issue_count.is_none());
        assert!(output.config.is_none());
        assert!(output.schema.is_none());
    }

    #[test]
    fn test_collect_info_output_accepts_startup_prefix_alias() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        std::fs::write(
            beads_dir.join("metadata.json"),
            r#"{"database":"beads.db","jsonl_export":"issues.jsonl"}"#,
        )
        .unwrap();
        std::fs::write(beads_dir.join("config.yaml"), "prefix: proj\n").unwrap();

        let _guard = DirGuard::new(temp.path());

        let output = collect_info_output(&InfoArgs::default(), &CliOverrides::default()).unwrap();

        assert_eq!(output.resolved_prefix.as_deref(), Some("proj"));
    }

    #[test]
    fn test_collect_info_output_accepts_db_prefix_alias() {
        let _lock = crate::util::test_helpers::TEST_DIR_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let temp = TempDir::new().unwrap();
        let beads_dir = temp.path().join(".beads");
        std::fs::create_dir_all(&beads_dir).unwrap();
        std::fs::write(
            beads_dir.join("metadata.json"),
            r#"{"database":"beads.db","jsonl_export":"issues.jsonl"}"#,
        )
        .unwrap();

        let db_path = beads_dir.join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        storage.set_config("prefix", "proj").unwrap();

        let _guard = DirGuard::new(temp.path());

        let output = collect_info_output(&InfoArgs::default(), &CliOverrides::default()).unwrap();

        assert_eq!(output.resolved_prefix.as_deref(), Some("proj"));
        assert_eq!(
            output
                .config
                .as_ref()
                .and_then(|config| config.get("prefix"))
                .map(String::as_str),
            Some("proj")
        );
    }
}
