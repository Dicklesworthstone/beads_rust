//! Info command implementation.

use crate::cli::InfoArgs;
use crate::config;
use crate::error::Result;
use crate::output::OutputContext;
use crate::storage::SqliteStorage;
use crate::storage::schema::CURRENT_SCHEMA_VERSION;
use crate::util::parse_id;
use rich_rust::prelude::*;
use serde::Serialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const SCHEMA_TABLES: &[&str] = &[
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
struct InfoOutput {
    database_path: String,
    beads_dir: String,
    mode: String,
    daemon_connected: bool,
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
    db_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    jsonl_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    jsonl_size: Option<u64>,
}

/// Execute the info command.
///
/// # Errors
///
/// Returns an error if configuration or storage access fails.
pub fn execute(
    args: &InfoArgs,
    json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    if args.whats_new {
        return print_message(
            json,
            ctx,
            "No whats-new data available for br.",
            "whats_new",
        );
    }
    if args.thanks {
        return print_message(
            json,
            ctx,
            "Thanks for using br. See README for project acknowledgements.",
            "thanks",
        );
    }

    let beads_dir = config::discover_beads_dir(Some(Path::new(".")))?;
    let storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;
    let db_path = canonicalize_lossy(&storage_ctx.paths.db_path);

    let issue_count = storage_ctx.storage.count_issues().ok();
    let config_map = storage_ctx
        .storage
        .get_all_config()
        .ok()
        .filter(|map| !map.is_empty());
    let schema = if args.schema {
        Some(build_schema_info(&storage_ctx.storage, config_map.as_ref()))
    } else {
        None
    };

    // Get additional info for rich output
    let db_size = std::fs::metadata(&storage_ctx.paths.db_path)
        .map(|m| m.len())
        .ok();
    let jsonl_size = std::fs::metadata(&storage_ctx.paths.jsonl_path)
        .map(|m| m.len())
        .ok();

    let output = InfoOutput {
        database_path: db_path.display().to_string(),
        beads_dir: canonicalize_lossy(&beads_dir).display().to_string(),
        mode: "direct".to_string(),
        daemon_connected: false,
        daemon_fallback_reason: Some("no-daemon".to_string()),
        daemon_detail: Some("br runs in direct mode only".to_string()),
        issue_count,
        config: config_map,
        schema,
        db_size,
        jsonl_path: Some(
            canonicalize_lossy(&storage_ctx.paths.jsonl_path)
                .display()
                .to_string(),
        ),
        jsonl_size,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    if ctx.is_rich() {
        render_info_rich(&output, ctx);
    } else {
        print_human(&output);
    }

    Ok(())
}

fn build_schema_info(
    storage: &SqliteStorage,
    config_map: Option<&HashMap<String, String>>,
) -> SchemaInfo {
    let mut ids = storage.get_all_ids().unwrap_or_default();
    let sample_issue_ids: Vec<String> = ids.drain(..ids.len().min(3)).collect();

    let mut detected_prefix = config_map
        .and_then(|map| map.get("issue_prefix").cloned())
        .filter(|value| !value.trim().is_empty());

    if detected_prefix.is_none() {
        detected_prefix = sample_issue_ids
            .first()
            .and_then(|id| parse_id(id).ok().map(|parsed| parsed.prefix));
    }

    SchemaInfo {
        tables: SCHEMA_TABLES.iter().map(ToString::to_string).collect(),
        schema_version: CURRENT_SCHEMA_VERSION.to_string(),
        config: config_map.cloned(),
        sample_issue_ids,
        detected_prefix,
    }
}

fn print_human(info: &InfoOutput) {
    println!("Beads Database Information");
    println!("Database: {}", info.database_path);
    println!("Mode: {}", info.mode);

    if info.daemon_connected {
        println!("Daemon: connected");
    } else if let Some(reason) = &info.daemon_fallback_reason {
        println!("Daemon: not connected ({reason})");
        if let Some(detail) = &info.daemon_detail {
            println!("  {detail}");
        }
    }

    if let Some(count) = info.issue_count {
        println!("Issue count: {count}");
    }

    if let Some(config_map) = &info.config {
        if let Some(prefix) = config_map.get("issue_prefix") {
            println!("Issue prefix: {prefix}");
        }
    }

    if let Some(schema) = &info.schema {
        println!();
        println!("Schema:");
        println!("  Version: {}", schema.schema_version);
        println!("  Tables: {}", schema.tables.join(", "));
        if let Some(prefix) = &schema.detected_prefix {
            println!("  Detected prefix: {prefix}");
        }
        if !schema.sample_issue_ids.is_empty() {
            println!("  Sample IDs: {}", schema.sample_issue_ids.join(", "));
        }
    }
}

fn print_message(json: bool, ctx: &OutputContext, message: &str, key: &str) -> Result<()> {
    if json {
        let payload = serde_json::json!({ key: message });
        println!("{}", serde_json::to_string_pretty(&payload)?);
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
    content.append_styled(&info.beads_dir, theme.accent.clone());
    content.append("\n");

    // Prefix (if available)
    if let Some(config_map) = &info.config {
        if let Some(prefix) = config_map.get("issue_prefix") {
            content.append_styled("Prefix      ", theme.dimmed.clone());
            content.append_styled(prefix, theme.issue_id.clone());
            content.append("\n");
        }
    }

    content.append("\n");

    // Database section
    content.append_styled("Database\n", theme.section.clone());
    content.append_styled("  Path      ", theme.dimmed.clone());
    content.append_styled(&info.database_path, theme.accent.clone());
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
        content.append_styled(jsonl_path, theme.accent.clone());
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
    content.append(&info.mode);
    if !info.daemon_connected {
        content.append_styled(" (no daemon)", theme.muted.clone());
    }
    content.append("\n");

    // Schema section (if requested)
    if let Some(schema) = &info.schema {
        content.append("\n");
        content.append_styled("Schema\n", theme.section.clone());
        content.append_styled("  Version   ", theme.dimmed.clone());
        content.append(&schema.schema_version);
        content.append("\n");

        content.append_styled("  Tables    ", theme.dimmed.clone());
        content.append(&schema.tables.join(", "));
        content.append("\n");

        if let Some(prefix) = &schema.detected_prefix {
            content.append_styled("  Prefix    ", theme.dimmed.clone());
            content.append_styled(prefix, theme.issue_id.clone());
            content.append("\n");
        }

        if !schema.sample_issue_ids.is_empty() {
            content.append_styled("  Samples   ", theme.dimmed.clone());
            content.append(&schema.sample_issue_ids.join(", "));
            content.append("\n");
        }
    }

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled(
            "Project Information",
            theme.panel_title.clone(),
        ))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
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
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
