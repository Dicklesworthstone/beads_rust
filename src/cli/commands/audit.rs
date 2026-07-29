//! Audit command implementation.

use super::{
    RoutedWorkspaceWriteLock, acquire_routed_workspace_write_lock,
    auto_import_storage_ctx_if_stale, cli_for_routed_workspace, resolve_issue_id,
};
use crate::cli::{
    AuditCommands, AuditCoordinationArgs, AuditLabelArgs, AuditLogArgs, AuditRecordArgs,
    AuditSummaryArgs,
};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::format::{sanitize_terminal_inline, sanitize_terminal_text};
use crate::model::EventType;
use crate::output::{OutputContext, Theme};
use crate::sync::require_valid_sync_path;
use crate::util::id::{IdResolver, ResolverConfig};
use chrono::{DateTime, Utc};
use rich_rust::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static ID_COUNTER: AtomicU64 = AtomicU64::new(0);
const MAX_AUDIT_STDIN_BYTES: usize = 10 * 1024 * 1024;
const MAX_AUDIT_STDIN_BYTES_U64: u64 = 10 * 1024 * 1024;
const MAX_COORDINATION_EVIDENCE_SUMMARY_CHARS: usize = 512;
const MAX_COORDINATION_COMMAND_CHARS: usize = 256;

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
struct AuditCoordinationOutput {
    recorded: usize,
    snapshot_hash: String,
    ids: Vec<String>,
}

#[derive(Debug, Serialize)]
struct AuditLabelOutput {
    id: String,
    parent_id: String,
    label: String,
}

// New structs for Log/Summary JSON output
#[derive(Debug, Serialize)]
struct AuditLogOutput {
    issue_id: String,
    events: Vec<AuditEventOutput>,
}

#[derive(Debug, Serialize)]
struct AuditEventOutput {
    id: i64,
    event_type: String,
    actor: String,
    timestamp: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    old_value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    new_value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    comment: Option<String>,
    // Tier 1 attribution (issue #312, Layer 3 capture-only). Recorded only.
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    harness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
}

#[derive(Debug, Serialize)]
struct AuditSummaryOutput {
    period_days: u32,
    totals: AuditTotals,
    actors: Vec<ActorSummary>,
}

#[derive(Debug, Serialize, Default)]
struct AuditTotals {
    created: usize,
    updated: usize,
    closed: usize,
    comments: usize,
    total: usize,
}

#[derive(Debug, Serialize)]
struct ActorSummary {
    actor: String,
    created: usize,
    updated: usize,
    closed: usize,
    comments: usize,
    total: usize,
}

/// Execute the audit command.
///
/// # Errors
///
/// Returns an error if audit entry creation fails or file IO fails.
pub fn execute(
    command: &AuditCommands,
    _json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let layer = config::load_config(&beads_dir, None, cli)?;
    let actor = config::resolve_actor(&layer);

    match command {
        AuditCommands::Record(args) => record_entry(args, &beads_dir, &actor, ctx),
        AuditCommands::Coordination(args) => {
            record_coordination_entries(args, &beads_dir, &actor, ctx)
        }
        AuditCommands::Label(args) => label_entry(args, &beads_dir, &actor, ctx),
        AuditCommands::Log(args) => execute_log(args, &beads_dir, cli, ctx),
        AuditCommands::Summary(args) => execute_summary(args, &beads_dir, cli, ctx),
    }
}

fn execute_log(
    args: &AuditLogArgs,
    beads_dir: &Path,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let (storage_ctx, issue_id, _routed_write_lock) =
        open_routed_storage_for_issue_input(beads_dir, cli, &args.id)?;
    let events = storage_ctx.storage.get_events(&issue_id, 0)?;

    if ctx.is_quiet() {
        return Ok(());
    }

    if ctx.is_toon() {
        let output = AuditLogOutput {
            issue_id,
            events: events.iter().map(map_event_to_output).collect(),
        };
        ctx.toon(&output);
        return Ok(());
    }

    if ctx.is_json() {
        let output = AuditLogOutput {
            issue_id,
            events: events.iter().map(map_event_to_output).collect(),
        };
        ctx.json_pretty(&output);
        return Ok(());
    }

    if ctx.is_rich() {
        render_audit_log_rich(&issue_id, &events, ctx);
    } else {
        render_audit_log_plain(&issue_id, &events);
    }

    Ok(())
}

fn open_routed_storage_for_issue_input(
    local_beads_dir: &Path,
    cli: &config::CliOverrides,
    issue_input: &str,
) -> Result<(config::OpenStorageResult, String, RoutedWorkspaceWriteLock)> {
    let route = config::routing::resolve_route(issue_input, local_beads_dir)?;
    let mut route_cli = cli_for_routed_workspace(cli, route.is_external);

    let routed_write_lock = acquire_routed_workspace_write_lock(
        &route.beads_dir,
        route.is_external,
        route_cli.lock_timeout,
    )?;
    routed_write_lock.mark_cli_write_lock_held(&mut route_cli);
    let mut storage_ctx = config::open_storage_with_cli(&route.beads_dir, &route_cli)?;
    auto_import_storage_ctx_if_stale(&mut storage_ctx, &route_cli)?;
    let config_layer = storage_ctx.load_config(&route_cli)?;
    let id_config = config::id_config_from_layer(&config_layer);
    let resolver = IdResolver::new(ResolverConfig::with_prefix(id_config.prefix));
    let issue_id = resolve_issue_id(&storage_ctx.storage, &resolver, issue_input)?;

    Ok((storage_ctx, issue_id, routed_write_lock))
}

fn execute_summary(
    args: &AuditSummaryArgs,
    beads_dir: &Path,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let storage_ctx = config::open_storage_with_cli(beads_dir, cli)?;
    let events = storage_ctx.storage.get_all_events(0)?;

    let cutoff = Utc::now() - chrono::Duration::days(i64::from(args.days));
    let filtered_events: Vec<_> = events
        .into_iter()
        .filter(|e| e.created_at >= cutoff)
        .collect();

    let mut actor_map: HashMap<String, ActorSummary> = HashMap::new();
    let mut totals = AuditTotals::default();

    for event in &filtered_events {
        let entry = actor_map
            .entry(event.actor.clone())
            .or_insert_with(|| ActorSummary {
                actor: event.actor.clone(),
                created: 0,
                updated: 0,
                closed: 0,
                comments: 0,
                total: 0,
            });

        match event.event_type {
            EventType::Created => {
                entry.created += 1;
                totals.created += 1;
            }
            EventType::Closed => {
                entry.closed += 1;
                totals.closed += 1;
            }
            EventType::Commented => {
                entry.comments += 1;
                totals.comments += 1;
            }
            _ => {
                entry.updated += 1;
                totals.updated += 1;
            }
        }
        entry.total += 1;
        totals.total += 1;
    }

    let mut actors: Vec<_> = actor_map.into_values().collect();
    actors.sort_by_key(|b| std::cmp::Reverse(b.total));

    if ctx.is_quiet() {
        return Ok(());
    }

    if ctx.is_toon() {
        let output = AuditSummaryOutput {
            period_days: args.days,
            totals,
            actors,
        };
        ctx.toon(&output);
        return Ok(());
    }

    if ctx.is_json() {
        let output = AuditSummaryOutput {
            period_days: args.days,
            totals,
            actors,
        };
        ctx.json_pretty(&output);
        return Ok(());
    }

    if ctx.is_rich() {
        render_audit_summary_rich(args.days, &totals, &actors, ctx);
    } else {
        render_audit_summary_plain(args.days, &totals, &actors);
    }

    Ok(())
}

fn map_event_to_output(event: &crate::model::Event) -> AuditEventOutput {
    AuditEventOutput {
        id: event.id,
        event_type: event.event_type.as_str().to_string(),
        actor: event.actor.clone(),
        timestamp: event.created_at,
        old_value: event.old_value.clone(),
        new_value: event.new_value.clone(),
        comment: event.comment.clone(),
        agent_name: event.agent_name.clone(),
        harness: event.harness.clone(),
        model: event.model.clone(),
    }
}

fn record_entry(
    args: &AuditRecordArgs,
    beads_dir: &Path,
    actor: &str,
    ctx: &OutputContext,
) -> Result<()> {
    let use_stdin = args.stdin;

    let mut entry = if use_stdin {
        let input = read_audit_stdin_limited(io::stdin())?;
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

    if ctx.is_toon() {
        ctx.toon(&output);
    } else if ctx.is_json() {
        ctx.json_pretty(&output);
    } else if !ctx.is_quiet() {
        println!("{id}");
    }

    Ok(())
}

fn record_coordination_entries(
    args: &AuditCoordinationArgs,
    beads_dir: &Path,
    actor: &str,
    ctx: &OutputContext,
) -> Result<()> {
    if !args.stdin {
        return Err(BeadsError::validation(
            "stdin",
            "audit coordination requires --stdin",
        ));
    }

    let input = read_audit_stdin_limited(io::stdin())?;
    let snapshot = parse_coordination_snapshot_stream(&input)?;
    let snapshot_hash = stable_snapshot_hash(&snapshot.source_values)?;
    let command = bounded_clean_text(&args.command, MAX_COORDINATION_COMMAND_CHARS);
    let incidents = snapshot
        .claims
        .iter()
        .map(coordination_incident_from_claim)
        .collect::<Result<Vec<_>>>()?;

    let mut ids = Vec::with_capacity(incidents.len());
    for incident in &incidents {
        let mut entry = AuditEntry {
            id: None,
            kind: "coordination_incident".to_string(),
            created_at: None,
            actor: clean_actor(actor),
            issue_id: Some(incident.issue_id.clone()),
            model: None,
            prompt: None,
            response: None,
            error: None,
            tool_name: None,
            exit_code: None,
            parent_id: None,
            label: None,
            reason: None,
            extra: Some(coordination_extra_fields(
                &command,
                &snapshot_hash,
                incident,
            )),
        };
        ids.push(append_entry(beads_dir, &mut entry)?);
    }

    let output = AuditCoordinationOutput {
        recorded: ids.len(),
        snapshot_hash,
        ids,
    };

    if ctx.is_toon() {
        ctx.toon(&output);
    } else if ctx.is_json() {
        ctx.json_pretty(&output);
    } else if !ctx.is_quiet() {
        if output.ids.is_empty() {
            println!("recorded 0 coordination audit entries");
        } else {
            for id in &output.ids {
                println!("{id}");
            }
        }
    }

    Ok(())
}

fn label_entry(
    args: &AuditLabelArgs,
    beads_dir: &Path,
    actor: &str,
    ctx: &OutputContext,
) -> Result<()> {
    let label = args
        .label
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| BeadsError::validation("label", "required"))?
        .to_string();
    let parent_id = args.entry_id.trim();
    if parent_id.is_empty() {
        return Err(BeadsError::validation("entry_id", "required"));
    }
    if !audit_entry_exists(beads_dir, parent_id)? {
        return Err(BeadsError::validation(
            "entry_id",
            format!("Audit entry '{parent_id}' not found"),
        ));
    }

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
        parent_id: Some(parent_id.to_string()),
        label: Some(label.clone()),
        reason: clean_opt(args.reason.as_deref()),
        extra: None,
    };

    let id = append_entry(beads_dir, &mut entry)?;
    let output = AuditLabelOutput {
        id: id.clone(),
        parent_id: parent_id.to_string(),
        label,
    };

    if ctx.is_toon() {
        ctx.toon(&output);
    } else if ctx.is_json() {
        ctx.json_pretty(&output);
    } else if !ctx.is_quiet() {
        println!("{id}");
    }

    Ok(())
}

#[derive(Debug, Clone)]
struct CoordinationIncident {
    issue_id: String,
    classification: String,
    evidence_summary: String,
    suggested_action: String,
}

#[derive(Debug)]
struct CoordinationSnapshotInput {
    source_values: Vec<Value>,
    claims: Vec<Value>,
}

fn parse_coordination_snapshot_stream(raw: &str) -> Result<CoordinationSnapshotInput> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(BeadsError::validation(
            "stdin",
            "expected coordination status JSON but stdin was empty",
        ));
    }

    match serde_json::from_str::<Value>(trimmed) {
        Ok(value) => {
            let claims = coordination_claims_from_value(&value)?;
            return Ok(CoordinationSnapshotInput {
                source_values: vec![value],
                claims,
            });
        }
        Err(err) if !trimmed.contains('\n') => {
            return Err(BeadsError::validation(
                "stdin",
                format!("invalid coordination JSON: {err}"),
            ));
        }
        Err(_) => {}
    }

    let mut source_values = Vec::new();
    let mut claims = Vec::new();
    for (idx, line) in raw.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let value = serde_json::from_str::<Value>(line).map_err(|err| {
            BeadsError::validation(
                "stdin",
                format!("line {} is not valid coordination JSON: {err}", idx + 1),
            )
        })?;
        claims.extend(coordination_claims_from_value(&value)?);
        source_values.push(value);
    }

    Ok(CoordinationSnapshotInput {
        source_values,
        claims,
    })
}

fn coordination_claims_from_value(value: &Value) -> Result<Vec<Value>> {
    if let Some(array) = value.as_array() {
        let mut claims = Vec::new();
        for item in array {
            claims.extend(coordination_claims_from_value(item)?);
        }
        return Ok(claims);
    }

    if let Some(claims) = value.get("claims") {
        let claims = claims.as_array().ok_or_else(|| {
            BeadsError::validation("claims", "coordination status claims must be an array")
        })?;
        return Ok(claims.clone());
    }

    if value.get("issue").is_some() && value.get("assessment").is_some() {
        return Ok(vec![value.clone()]);
    }

    Err(BeadsError::validation(
        "coordination_snapshot",
        "expected coordination status object with claims[] or a coordination claim row",
    ))
}

fn coordination_incident_from_claim(claim: &Value) -> Result<CoordinationIncident> {
    let issue_id = required_claim_string(claim, "/issue/id", "issue.id")?;
    let classification = required_claim_string(
        claim,
        "/assessment/classification",
        "assessment.classification",
    )?;
    let suggested_action = required_claim_string(
        claim,
        "/assessment/recommended_action",
        "assessment.recommended_action",
    )?;
    let evidence_summary = required_claim_string(claim, "/evidence_summary", "evidence_summary")?;

    Ok(CoordinationIncident {
        issue_id: bounded_clean_text(&issue_id, MAX_COORDINATION_COMMAND_CHARS),
        classification: bounded_clean_text(&classification, MAX_COORDINATION_COMMAND_CHARS),
        evidence_summary: bounded_clean_text(
            &evidence_summary,
            MAX_COORDINATION_EVIDENCE_SUMMARY_CHARS,
        ),
        suggested_action: bounded_clean_text(&suggested_action, MAX_COORDINATION_COMMAND_CHARS),
    })
}

fn required_claim_string(claim: &Value, pointer: &str, field: &str) -> Result<String> {
    claim
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| BeadsError::validation(field, "required coordination claim field"))
}

fn coordination_extra_fields(
    command: &str,
    snapshot_hash: &str,
    incident: &CoordinationIncident,
) -> Map<String, Value> {
    Map::from_iter([
        ("command".to_string(), Value::String(command.to_string())),
        (
            "issue_id".to_string(),
            Value::String(incident.issue_id.clone()),
        ),
        (
            "classification".to_string(),
            Value::String(incident.classification.clone()),
        ),
        (
            "evidence_summary".to_string(),
            Value::String(incident.evidence_summary.clone()),
        ),
        (
            "snapshot_hash".to_string(),
            Value::String(snapshot_hash.to_string()),
        ),
        (
            "suggested_action".to_string(),
            Value::String(incident.suggested_action.clone()),
        ),
    ])
}

fn bounded_clean_text(value: &str, max_chars: usize) -> String {
    let sanitized = sanitize_terminal_inline(value.trim());
    if sanitized.chars().count() <= max_chars {
        return sanitized.into_owned();
    }

    let keep = max_chars.saturating_sub(3);
    let mut truncated = sanitized.chars().take(keep).collect::<String>();
    truncated.push_str("...");
    truncated
}

fn stable_snapshot_hash(values: &[Value]) -> Result<String> {
    let normalized = values.iter().map(normalize_json_value).collect::<Vec<_>>();
    let bytes = serde_json::to_vec(&normalized)?;
    let digest = Sha256::digest(&bytes);
    Ok(lower_hex(&digest))
}

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        encoded.push(char::from(HEX[usize::from(byte >> 4)]));
        encoded.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    encoded
}

fn normalize_json_value(value: &Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(items.iter().map(normalize_json_value).collect()),
        Value::Object(map) => {
            let mut keys = map.keys().collect::<Vec<_>>();
            keys.sort_unstable();
            let mut normalized = Map::new();
            for key in keys {
                normalized.insert(key.clone(), normalize_json_value(&map[key]));
            }
            Value::Object(normalized)
        }
        _ => value.clone(),
    }
}

fn read_audit_stdin_limited<R: Read>(reader: R) -> Result<String> {
    let mut reader = reader.take(MAX_AUDIT_STDIN_BYTES_U64.saturating_add(1));
    let mut input = Vec::new();
    reader.read_to_end(&mut input)?;
    if input.len() > MAX_AUDIT_STDIN_BYTES {
        return Err(BeadsError::validation(
            "stdin",
            format!("stdin input exceeds maximum size of {MAX_AUDIT_STDIN_BYTES} bytes"),
        ));
    }

    String::from_utf8(input).map_err(|e| {
        BeadsError::validation("stdin", format!("stdin input must be valid UTF-8: {e}"))
    })
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

    let mut line = serde_json::to_vec(&entry)?;
    line.push(b'\n');

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;

    file.write_all(&line)?;
    file.sync_all()?;

    Ok(entry.id.as_ref().expect("id set before append").clone())
}

fn interactions_file_path(beads_dir: &Path) -> PathBuf {
    beads_dir.join("interactions.jsonl")
}

fn ensure_interactions_file(beads_dir: &Path) -> Result<PathBuf> {
    if !beads_dir.exists() {
        return Err(BeadsError::NotInitialized);
    }

    fs::create_dir_all(beads_dir)?;
    let path = interactions_file_path(beads_dir);
    require_valid_sync_path(&path, beads_dir)?;
    if !path.exists() {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
    }
    Ok(path)
}

fn audit_entry_exists(beads_dir: &Path, entry_id: &str) -> Result<bool> {
    #[derive(Deserialize)]
    struct PartialId {
        id: Option<String>,
    }

    let entry_id = entry_id.trim();
    if entry_id.is_empty() {
        return Err(BeadsError::validation("entry_id", "required"));
    }

    let path = interactions_file_path(beads_dir);
    require_valid_sync_path(&path, beads_dir)?;

    let file = match fs::File::open(&path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(err) => return Err(err.into()),
    };

    let reader = BufReader::new(file);

    let id_pattern = format!("\"{}\"", entry_id);

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        // Fast path string check before paying JSON parse cost
        if !line.contains(&id_pattern) {
            continue;
        }
        match serde_json::from_str::<PartialId>(&line) {
            Ok(entry) => {
                if entry.id.as_deref() == Some(entry_id) {
                    return Ok(true);
                }
            }
            Err(e) => {
                tracing::debug!(
                    error = %e,
                    "Skipping malformed audit entry during existence check"
                );
            }
        }
    }

    Ok(false)
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
    // Use 8 bytes (16 hex chars) for much lower collision probability
    let bytes = &digest[..8];
    format!(
        "int-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7]
    )
}

fn render_audit_log_rich(issue_id: &str, events: &[crate::model::Event], ctx: &OutputContext) {
    let _console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");
    content.append("\n");

    for event in events {
        // Timestamp + Actor
        let time_str = event.created_at.format("%Y-%m-%d %H:%M").to_string();
        content.append_styled(&time_str, theme.dimmed.clone());
        content.append("  ");
        content.append_styled(
            &format!("@{:<10}", sanitize_terminal_inline(&event.actor)),
            theme.accent.clone(),
        );
        content.append("  ");

        // Event Type
        let type_style = event_type_style(&event.event_type, theme);
        content.append_styled(&format!("{:<15}", event.event_type.as_str()), type_style);
        content.append("\n");

        // Details
        let mut details = String::new();
        if let Some(old) = &event.old_value {
            if let Some(new) = &event.new_value {
                details.push_str(&format!(
                    "   {} → {}",
                    sanitize_terminal_inline(old),
                    sanitize_terminal_inline(new)
                ));
            } else {
                details.push_str(&format!("   Removed: {}", sanitize_terminal_inline(old)));
            }
        } else if let Some(new) = &event.new_value {
            details.push_str(&format!("   Set: {}", sanitize_terminal_inline(new)));
        }

        if !details.is_empty() {
            content.append_styled(&details, theme.dimmed.clone());
            content.append("\n");
        }

        if let Some(comment) = &event.comment {
            content.append_styled(
                &format!("   \"{}\"\n", sanitize_terminal_text(comment)),
                theme.comment.clone(),
            );
        }

        if let Some(attribution) = format_event_attribution(event) {
            content.append_styled(&format!("   {attribution}\n"), theme.dimmed.clone());
        }

        content.append("\n");
    }

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled(
            format!("Audit Log: {}", issue_id),
            theme.panel_title.clone(),
        ))
        .box_style(theme.box_style);

    ctx.render(&panel);
}

fn render_audit_log_plain(issue_id: &str, events: &[crate::model::Event]) {
    println!("Audit Log: {}", issue_id);
    println!("{}", "-".repeat(40));

    for event in events {
        println!(
            "{}  @{:<10}  {}",
            event.created_at.format("%Y-%m-%d %H:%M"),
            sanitize_terminal_inline(&event.actor),
            event.event_type.as_str()
        );

        if let Some(old) = &event.old_value {
            if let Some(new) = &event.new_value {
                println!(
                    "   {} -> {}",
                    sanitize_terminal_inline(old),
                    sanitize_terminal_inline(new)
                );
            } else {
                println!("   Removed: {}", sanitize_terminal_inline(old));
            }
        } else if let Some(new) = &event.new_value {
            println!("   Set: {}", sanitize_terminal_inline(new));
        }

        if let Some(comment) = &event.comment {
            println!("   \"{}\"", sanitize_terminal_text(comment));
        }
        if let Some(attribution) = format_event_attribution(event) {
            println!("   {attribution}");
        }
        println!();
    }
}

/// Format the captured Tier 1 attribution (issue #312, Layer 3 capture-only)
/// for display, e.g. `via agent=foo harness=bar model=baz`. Returns `None`
/// when the event carries no attribution. Recorded/displayed only — never
/// used for any gating decision.
fn format_event_attribution(event: &crate::model::Event) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(agent) = &event.agent_name {
        parts.push(format!("agent={}", sanitize_terminal_inline(agent)));
    }
    if let Some(harness) = &event.harness {
        parts.push(format!("harness={}", sanitize_terminal_inline(harness)));
    }
    if let Some(model) = &event.model {
        parts.push(format!("model={}", sanitize_terminal_inline(model)));
    }
    if parts.is_empty() {
        None
    } else {
        Some(format!("via {}", parts.join(" ")))
    }
}

fn render_audit_summary_rich(
    days: u32,
    totals: &AuditTotals,
    actors: &[ActorSummary],
    ctx: &OutputContext,
) {
    let _console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");

    // Header row
    content.append_styled(
        &format!(
            "{:<15} {:>8} {:>8} {:>8} {:>8} {:>8}\n",
            "Actor", "Created", "Updated", "Closed", "Comments", "Total"
        ),
        theme.table_header.clone(),
    );
    content.append_styled(&format!("{}\n", "─".repeat(60)), theme.dimmed.clone());

    // Rows
    for actor in actors {
        content.append_styled(&format!("{:<15}", actor.actor), theme.accent.clone());
        content.append(&format!(
            " {:>8} {:>8} {:>8} {:>8} ",
            actor.created, actor.updated, actor.closed, actor.comments
        ));
        content.append_styled(&format!("{:>8}\n", actor.total), theme.emphasis.clone());
    }

    content.append("\n");
    content.append_styled(&format!("{:<15}", "TOTAL"), theme.table_header.clone());
    content.append_styled(
        &format!(
            " {:>8} {:>8} {:>8} {:>8} {:>8}",
            totals.created, totals.updated, totals.closed, totals.comments, totals.total
        ),
        theme.emphasis.clone(),
    );

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled(
            format!("Audit Summary (last {} days)", days),
            theme.panel_title.clone(),
        ))
        .box_style(theme.box_style);

    ctx.render(&panel);
}

fn render_audit_summary_plain(days: u32, totals: &AuditTotals, actors: &[ActorSummary]) {
    println!("Audit Summary (last {} days)", days);
    println!(
        "{:<15} {:>8} {:>8} {:>8} {:>8} {:>8}",
        "Actor", "Created", "Updated", "Closed", "Comments", "Total"
    );
    println!("{}", "-".repeat(65));

    for actor in actors {
        println!(
            "{:<15} {:>8} {:>8} {:>8} {:>8} {:>8}",
            actor.actor, actor.created, actor.updated, actor.closed, actor.comments, actor.total
        );
    }

    println!("{}", "-".repeat(65));
    println!(
        "{:<15} {:>8} {:>8} {:>8} {:>8} {:>8}",
        "TOTAL", totals.created, totals.updated, totals.closed, totals.comments, totals.total
    );
}

fn event_type_style(event_type: &EventType, theme: &Theme) -> rich_rust::Style {
    use rich_rust::Color;
    match event_type {
        EventType::Created => Style::new().color(Color::parse("green").unwrap()),
        EventType::Closed => Style::new().color(Color::parse("blue").unwrap()),
        EventType::Updated => Style::new().color(Color::parse("yellow").unwrap()),
        EventType::Commented => theme.dimmed.clone(),
        _ => Style::new(),
    }
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

    fn coordination_claim(issue_id: &str) -> serde_json::Value {
        serde_json::json!({
            "issue": {
                "id": issue_id
            },
            "assessment": {
                "classification": "no_mail_snapshot",
                "recommended_action": "inspect_mail"
            },
            "evidence_summary": "updated_at=2026-05-08T00:00:00Z, assignee=agent-a"
        })
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
    fn coordination_snapshot_parser_accepts_status_object_and_jsonl_rows() {
        let status = serde_json::json!({
            "schema_version": "br.coordination.v1",
            "claims": [coordination_claim("bd-one"), coordination_claim("bd-two")]
        });
        let parsed = parse_coordination_snapshot_stream(&status.to_string()).expect("status");
        assert_eq!(parsed.claims.len(), 2);

        let jsonl = format!(
            "{}\n{}\n",
            coordination_claim("bd-three"),
            coordination_claim("bd-four")
        );
        let parsed_jsonl = parse_coordination_snapshot_stream(&jsonl).expect("jsonl");
        assert_eq!(parsed_jsonl.claims.len(), 2);
    }

    #[test]
    fn coordination_incident_requires_normalized_fields() {
        let mut claim = coordination_claim("bd-one");
        claim["assessment"]["recommended_action"] = serde_json::Value::Null;

        let err = coordination_incident_from_claim(&claim).expect_err("missing action");

        assert!(
            matches!(&err, BeadsError::Validation { field, reason } if field == "assessment.recommended_action" && reason.contains("required")),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn coordination_evidence_summary_is_bounded_and_sanitized() {
        let dirty = format!("{}{}", "\u{1b}[31m", "x".repeat(800));
        let mut claim = coordination_claim("bd-one");
        claim["evidence_summary"] = serde_json::Value::String(dirty);

        let incident = coordination_incident_from_claim(&claim).expect("incident");

        assert!(incident.evidence_summary.len() <= MAX_COORDINATION_EVIDENCE_SUMMARY_CHARS);
        assert!(!incident.evidence_summary.contains('\u{1b}'));
        assert!(incident.evidence_summary.ends_with("..."));
    }

    #[test]
    fn coordination_snapshot_hash_is_stable_for_object_key_order() {
        let first = serde_json::json!({"b": 2, "a": {"d": 4, "c": 3}});
        let second = serde_json::json!({"a": {"c": 3, "d": 4}, "b": 2});

        assert_eq!(
            stable_snapshot_hash(&[first]).expect("first hash"),
            stable_snapshot_hash(&[second]).expect("second hash")
        );
    }

    #[test]
    fn coordination_snapshot_parser_rejects_malformed_json() {
        let err = parse_coordination_snapshot_stream("{").expect_err("malformed JSON");

        assert!(
            matches!(&err, BeadsError::Validation { field, reason } if field == "stdin" && reason.contains("invalid coordination JSON")),
            "unexpected error: {err:?}"
        );
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

    #[test]
    fn test_read_audit_stdin_limited_checks_size_before_utf8_parse() {
        let mut payload = vec![b'a'; MAX_AUDIT_STDIN_BYTES];
        payload.push(0xc3);

        let err = read_audit_stdin_limited(std::io::Cursor::new(payload)).unwrap_err();

        assert!(
            matches!(&err, BeadsError::Validation { field, reason } if field == "stdin" && reason.contains("maximum size")),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn test_read_audit_stdin_limited_rejects_invalid_utf8() {
        let err = read_audit_stdin_limited(std::io::Cursor::new([0xff])).unwrap_err();

        assert!(
            matches!(&err, BeadsError::Validation { field, reason } if field == "stdin" && reason.contains("valid UTF-8")),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn test_audit_entry_exists_finds_existing_parent() {
        let dir = temp_beads_dir();
        let beads_dir = dir.path().join(".beads");

        let mut entry = base_entry("llm_call");
        let entry_id = append_entry(&beads_dir, &mut entry).expect("append interaction");

        assert!(audit_entry_exists(&beads_dir, &entry_id).expect("lookup existing entry"));
        assert!(!audit_entry_exists(&beads_dir, "int-missing").expect("lookup missing entry"));
    }

    #[cfg(unix)]
    #[test]
    fn test_ensure_interactions_file_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let dir = temp_beads_dir();
        let beads_dir = dir.path().join(".beads");
        let outside_dir = TempDir::new().expect("outside tempdir");
        let outside_path = outside_dir.path().join("captured.jsonl");
        fs::write(&outside_path, "outside").expect("write outside file");

        symlink(&outside_path, beads_dir.join("interactions.jsonl")).expect("create symlink");

        let err = ensure_interactions_file(&beads_dir).unwrap_err();
        assert!(
            matches!(err, BeadsError::Config(_)),
            "unexpected error: {err:?}"
        );
    }
}
