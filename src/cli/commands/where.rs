//! Where command implementation.

use crate::config;
use crate::config::routing::follow_redirects;
use crate::error::Result;
use crate::output::OutputContext;
use crate::util::parse_id;
use serde::Serialize;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

#[derive(Serialize)]
struct WhereOutput {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    redirected_from: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prefix: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    database_path: Option<String>,
}

/// Execute the where command.
///
/// # Errors
///
/// Returns an error if redirect resolution fails.
pub fn execute(json: bool, cli: &config::CliOverrides, _ctx: &OutputContext) -> Result<()> {
    let Ok(beads_dir) = config::discover_beads_dir(Some(Path::new("."))) else {
        return handle_missing_beads(json);
    };

    let final_dir = follow_redirects(&beads_dir, 10)?;
    let redirected_from = if final_dir == beads_dir {
        None
    } else {
        Some(canonicalize_lossy(&beads_dir).display().to_string())
    };

    let paths = config::ConfigPaths::resolve(&final_dir, cli.db.as_ref())?;
    let database_path = canonicalize_lossy(&paths.db_path).display().to_string();
    let prefix = detect_prefix(&final_dir, &paths.jsonl_path, cli);

    let output = WhereOutput {
        path: canonicalize_lossy(&final_dir).display().to_string(),
        redirected_from,
        prefix,
        database_path: Some(database_path),
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        print_human(&output);
    }

    Ok(())
}

fn detect_prefix(
    beads_dir: &Path,
    jsonl_path: &Path,
    cli: &config::CliOverrides,
) -> Option<String> {
    if let Ok(storage_ctx) = config::open_storage_with_cli(beads_dir, cli) {
        if let Ok(Some(prefix)) = storage_ctx.storage.get_config("issue_prefix") {
            if !prefix.trim().is_empty() {
                return Some(prefix);
            }
        }

        if let Ok(ids) = storage_ctx.storage.get_all_ids() {
            if let Some(prefix) = ids
                .first()
                .and_then(|id| parse_id(id).ok().map(|parsed| parsed.prefix))
            {
                return Some(prefix);
            }
        }
    }

    prefix_from_jsonl(jsonl_path)
}

fn prefix_from_jsonl(path: &Path) -> Option<String> {
    if !path.is_file() {
        return None;
    }

    let file = File::open(path).ok()?;
    let reader = BufReader::new(file);
    for line in reader.lines().map_while(std::result::Result::ok) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let value: serde_json::Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(_) => continue,
        };
        let Some(id) = value.get("id").and_then(|value| value.as_str()) else {
            continue;
        };
        if let Ok(parsed) = parse_id(id) {
            return Some(parsed.prefix);
        }
    }

    None
}

fn print_human(output: &WhereOutput) {
    println!("{}", output.path);
    if let Some(origin) = &output.redirected_from {
        println!("  (via redirect from {origin})");
    }
    if let Some(prefix) = &output.prefix {
        println!("  prefix: {prefix}");
    }
    if let Some(db_path) = &output.database_path {
        println!("  database: {db_path}");
    }
}

fn handle_missing_beads(json: bool) -> Result<()> {
    if json {
        let payload = serde_json::json!({ "error": "no beads directory found" });
        println!("{}", serde_json::to_string_pretty(&payload)?);
    } else {
        eprintln!("No beads directory found.");
        eprintln!("Run `br init` to create one.");
    }
    std::process::exit(1);
}

fn canonicalize_lossy(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}
