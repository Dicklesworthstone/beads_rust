#![allow(clippy::module_name_repetitions)]

#[path = "../common/mod.rs"]
mod common;

use common::cli::{BrWorkspace, run_br};
use regex::Regex;
use serde_json::Value;

pub fn init_workspace() -> BrWorkspace {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init", "--prefix", "bd"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    workspace
}

pub fn create_issue(workspace: &BrWorkspace, title: &str, label: &str) -> String {
    let output = run_br(workspace, ["create", title], label);
    assert!(output.status.success(), "create failed: {}", output.stderr);
    parse_created_id(&output.stdout)
}

fn parse_created_id(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or("");
    let id_part = line
        .strip_prefix("Created ")
        .and_then(|rest| rest.split(':').next())
        .unwrap_or("");
    id_part.trim().to_string()
}

pub fn normalize_output(output: &str) -> String {
    let mut normalized = output.to_string();

    let id_re = Regex::new(r"\b[a-zA-Z0-9_-]+-[a-z0-9]{3,}\b").expect("id regex");
    normalized = id_re.replace_all(&normalized, "ID-REDACTED").to_string();

    // Match full ISO timestamps including sub-second precision and timezone
    let ts_full_re =
        Regex::new(r"\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}(\.\d+)?Z?").expect("full timestamp regex");
    normalized = ts_full_re
        .replace_all(&normalized, "YYYY-MM-DDTHH:MM:SS")
        .to_string();

    let date_re = Regex::new(r"\d{4}-\d{2}-\d{2}").expect("date regex");
    normalized = date_re.replace_all(&normalized, "YYYY-MM-DD").to_string();

    // Mask git hash in version string e.g. (main@91a4389)
    let version_re = Regex::new(r"\(main@[a-f0-9]+\)").expect("version regex");
    normalized = version_re
        .replace_all(&normalized, "(main@GIT_HASH)")
        .to_string();

    // Normalize line numbers in log messages e.g. src/storage/sqlite.rs:1077: → src/storage/sqlite.rs:LINE:
    // This prevents snapshot failures when code is modified and line numbers shift
    let line_num_re = Regex::new(r"\.rs:\d+:").expect("line number regex");
    normalized = line_num_re
        .replace_all(&normalized, ".rs:LINE:")
        .to_string();

    normalized
}

fn normalize_id_string(s: &str) -> String {
    // Normalize strings that contain issue IDs like "bd-abc:open" or "bd-xyz"
    let id_re = Regex::new(r"\b[a-zA-Z0-9_]+-[a-z0-9]{3,}\b").expect("id regex");
    id_re.replace_all(s, "ISSUE_ID").to_string()
}

pub fn normalize_json(json: &Value) -> Value {
    match json {
        Value::Object(map) => {
            let mut new_map = serde_json::Map::new();
            for (key, value) in map {
                let normalized_value = match key.as_str() {
                    "id" | "issue_id" | "depends_on_id" | "blocks_id" => {
                        Value::String("ISSUE_ID".to_string())
                    }
                    "created_at" | "updated_at" | "closed_at" | "due_at" | "defer_until" => {
                        Value::String("TIMESTAMP".to_string())
                    }
                    "content_hash" => Value::String("HASH".to_string()),
                    // Handle blocked_by array which contains ID:status strings
                    "blocked_by" | "blocks" | "depends_on" => {
                        if let Value::Array(items) = value {
                            Value::Array(
                                items
                                    .iter()
                                    .map(|v| {
                                        if let Value::String(s) = v {
                                            Value::String(normalize_id_string(s))
                                        } else {
                                            normalize_json(v)
                                        }
                                    })
                                    .collect(),
                            )
                        } else {
                            normalize_json(value)
                        }
                    }
                    _ => normalize_json(value),
                };
                new_map.insert(key.clone(), normalized_value);
            }
            Value::Object(new_map)
        }
        Value::Array(items) => Value::Array(items.iter().map(normalize_json).collect()),
        other => other.clone(),
    }
}

pub fn normalize_jsonl(contents: &str) -> String {
    let mut lines = Vec::new();
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = serde_json::from_str(line).expect("jsonl line");
        let normalized = normalize_json(&value);
        lines.push(serde_json::to_string(&normalized).expect("jsonl normalize"));
    }
    // Sort lines to ensure deterministic output (IDs are content-hash based and vary)
    lines.sort();
    lines.join("\n")
}

mod cli_output;
mod error_messages;
mod json_output;
mod jsonl_format;
