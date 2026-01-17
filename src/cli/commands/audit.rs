//! Audit command implementation.

use crate::cli::{AuditCommands, AuditLabelArgs, AuditRecordArgs};
use crate::config;
use crate::error::{BeadsError, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{self, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
struct AuditEntry {
    id: Option<String>,
    kind: String,
    created_at: Option<DateTime<Utc>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    actor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    issue_id: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    response: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    exit_code: Option<i32>,

    #[serde(skip_serializing_if = "Option::is_none")]
    parent_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    extra: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug, Serialize)]
struct AuditRecordOutput {
    id: String,
    kind: String,
}

#[derive(Debug, Serialize)]
struct AuditLabelOutput {
    id: String,
    parent_id: String,
    label: String,
}

/// Execute the audit command.
///
/// # Errors
///
/// Returns an error if audit entry creation fails or file IO fails.
pub fn execute(command: &AuditCommands, json: bool, cli: &config::CliOverrides) -> Result<()> {
    let beads_dir = config::discover_beads_dir(Some(Path::new(".")))?;
    let layer = config::load_config(&beads_dir, None, cli)?;
    let actor = config::resolve_actor(&layer);

    match command {
        AuditCommands::Record(args) => record_entry(args, &beads_dir, &actor, json),
        AuditCommands::Label(args) => label_entry(args, &beads_dir, &actor, json),
    }
}

fn record_entry(args: &AuditRecordArgs, beads_dir: &Path, actor: &str, json: bool) -> Result<()> {
    let stdin_piped = !io::stdin().is_terminal();
    let no_fields = no_fields_provided(args);

    // Only read from stdin if explicitly requested OR if stdin is piped AND has content
    let use_stdin = if args.stdin {
        // Explicitly requested: always use stdin
        true
    } else if stdin_piped && no_fields {
        // Check if stdin actually has content - peek at stdin with a non-blocking check
        // For safety, we'll read stdin content and check if it's empty
        false // Don't auto-detect; require explicit --stdin or CLI args
    } else {
        false
    };

    let mut entry = if use_stdin {
        let mut input = String::new();
        io::stdin().read_to_string(&mut input)?;
        let trimmed = input.trim();
        if trimmed.is_empty() {
            return Err(BeadsError::validation(
                "stdin",
                "expected JSON input but stdin was empty",
            ));
        }
        let mut entry: AuditEntry = serde_json::from_str(trimmed)?;
        if let Some(override_actor) = clean_actor(actor) {
            entry.actor = Some(override_actor);
        }
        entry
    } else {
        let kind = args
            .kind
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| BeadsError::validation("kind", "required"))?
            .to_string();

        AuditEntry {
            id: None,
            kind,
            created_at: None,
            actor: clean_actor(actor),
            issue_id: clean_opt(args.issue_id.as_deref()),
            model: clean_opt(args.model.as_deref()),
            prompt: clean_opt(args.prompt.as_deref()),
            response: clean_opt(args.response.as_deref()),
            error: clean_opt(args.error.as_deref()),
            tool_name: clean_opt(args.tool_name.as_deref()),
            exit_code: args.exit_code,
            parent_id: None,
            label: None,
            reason: None,
            extra: None,
        }
    };

    let id = append_entry(beads_dir, &mut entry)?;
    let output = AuditRecordOutput {
        id: id.clone(),
        kind: entry.kind.clone(),
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("{id}");
    }

    Ok(())
}

fn label_entry(args: &AuditLabelArgs, beads_dir: &Path, actor: &str, json: bool) -> Result<()> {
    let label = args
        .label
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| BeadsError::validation("label", "required"))?
        .to_string();

    let mut entry = AuditEntry {
        id: None,
        kind: "label".to_string(),
        created_at: None,
        actor: clean_actor(actor),
        issue_id: None,
        model: None,
        prompt: None,
        response: None,
        error: None,
        tool_name: None,
        exit_code: None,
        parent_id: Some(args.entry_id.clone()),
        label: Some(label.clone()),
        reason: clean_opt(args.reason.as_deref()),
        extra: None,
    };

    let id = append_entry(beads_dir, &mut entry)?;
    let output = AuditLabelOutput {
        id: id.clone(),
        parent_id: args.entry_id.clone(),
        label,
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&output)?);
    } else {
        println!("{id}");
    }

    Ok(())
}

fn no_fields_provided(args: &AuditRecordArgs) -> bool {
    is_empty_opt(args.kind.as_deref())
        && is_empty_opt(args.issue_id.as_deref())
        && is_empty_opt(args.model.as_deref())
        && is_empty_opt(args.prompt.as_deref())
        && is_empty_opt(args.response.as_deref())
        && is_empty_opt(args.tool_name.as_deref())
        && is_empty_opt(args.error.as_deref())
        && args.exit_code.is_none()
}

fn is_empty_opt(value: Option<&str>) -> bool {
    value.is_none_or(|v| v.trim().is_empty())
}

fn clean_opt(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

fn clean_actor(actor: &str) -> Option<String> {
    let trimmed = actor.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn append_entry(beads_dir: &Path, entry: &mut AuditEntry) -> Result<String> {
    let path = ensure_interactions_file(beads_dir)?;

    let kind = entry.kind.trim();
    if kind.is_empty() {
        return Err(BeadsError::validation("kind", "required"));
    }
    entry.kind = kind.to_string();

    if entry.id.as_ref().is_none_or(|id| id.trim().is_empty()) {
        entry.id = Some(new_audit_id());
    }

    if entry.created_at.is_none() {
        entry.created_at = Some(Utc::now());
    }

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;

    let mut writer = io::BufWriter::new(&mut file);
    serde_json::to_writer(&mut writer, &entry)?;
    writer.write_all(b"\n")?;
    writer.flush()?;

    Ok(entry.id.as_ref().expect("id set before append").clone())
}

fn ensure_interactions_file(beads_dir: &Path) -> Result<PathBuf> {
    if !beads_dir.exists() {
        return Err(BeadsError::NotInitialized);
    }

    fs::create_dir_all(beads_dir)?;
    let path = beads_dir.join("interactions.jsonl");
    if !path.exists() {
        fs::write(&path, b"")?;
    }
    Ok(path)
}

fn new_audit_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let counter = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();

    let mut hasher = Sha256::new();
    hasher.update(nanos.to_le_bytes());
    hasher.update(counter.to_le_bytes());
    hasher.update(pid.to_le_bytes());

    let digest = hasher.finalize();
    let bytes = &digest[..4];
    format!(
        "int-{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3]
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_beads_dir() -> TempDir {
        let dir = TempDir::new().expect("tempdir");
        let beads_dir = dir.path().join(".beads");
        fs::create_dir_all(&beads_dir).expect("create beads dir");
        dir
    }

    fn base_entry(kind: &str) -> AuditEntry {
        AuditEntry {
            id: None,
            kind: kind.to_string(),
            created_at: None,
            actor: None,
            issue_id: None,
            model: None,
            prompt: None,
            response: None,
            error: None,
            tool_name: None,
            exit_code: None,
            parent_id: None,
            label: None,
            reason: None,
            extra: None,
        }
    }

    #[test]
    fn test_append_preserves_order() {
        let dir = temp_beads_dir();
        let beads_dir = dir.path().join(".beads");

        let mut entry_a = base_entry("llm_call");
        let id_a = append_entry(&beads_dir, &mut entry_a).expect("append A");

        let mut entry_b = base_entry("tool_call");
        let id_b = append_entry(&beads_dir, &mut entry_b).expect("append B");

        let contents =
            fs::read_to_string(beads_dir.join("interactions.jsonl")).expect("read interactions");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);

        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();

        assert_eq!(first["id"], id_a);
        assert_eq!(second["id"], id_b);
    }

    #[test]
    fn test_record_output_shape() {
        let output = AuditRecordOutput {
            id: "int-1a2b3c4d".to_string(),
            kind: "llm_call".to_string(),
        };
        let json = serde_json::to_value(output).unwrap();
        assert_eq!(json["id"], "int-1a2b3c4d");
        assert_eq!(json["kind"], "llm_call");
    }

    #[test]
    fn test_label_output_shape() {
        let output = AuditLabelOutput {
            id: "int-2b3c4d5e".to_string(),
            parent_id: "int-aaaa1111".to_string(),
            label: "good".to_string(),
        };
        let json = serde_json::to_value(output).unwrap();
        assert_eq!(json["id"], "int-2b3c4d5e");
        assert_eq!(json["parent_id"], "int-aaaa1111");
        assert_eq!(json["label"], "good");
    }
}
