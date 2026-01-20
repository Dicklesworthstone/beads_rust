//! Info command implementation.

use crate::cli::InfoArgs;
use crate::config;
use crate::error::Result;
use crate::output::OutputContext;
use crate::storage::SqliteStorage;
use crate::storage::schema::CURRENT_SCHEMA_VERSION;
use crate::util::parse_id;
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
    _ctx: &OutputContext,
) -> Result<()> {
    if args.whats_new {
        return print_message(json, "No whats-new data available for br.", "whats_new");
    }
    if args.thanks {
        return print_message(
            json,
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

    let output = InfoOutput {
        database_path: db_path.display().to_string(),
        mode: "direct".to_string(),
        daemon_connected: false,
        daemon_fallback_reason: Some("no-daemon".to_string()),
        daemon_detail: Some("br runs in direct mode only".to_string()),
        issue_count,
        config: config_map,
        schema,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    print_human(&output);
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

fn print_message(json: bool, message: &str, key: &str) -> Result<()> {
    if json {
        let payload = serde_json::json!({ key: message });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        println!("{message}");
    }
    Ok(())
}

fn canonicalize_lossy(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}
