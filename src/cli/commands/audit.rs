//! Audit command implementation.

use crate::cli::{AuditCommands, AuditLabelArgs, AuditLogArgs, AuditRecordArgs, AuditSummaryArgs};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::model::EventType;
use crate::output::{OutputContext, Theme};
use chrono::{DateTime, TimeZone, Utc};
use rich_rust::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
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
    json: bool,
    cli: &config::CliOverrides,
    ctx: &OutputContext,
) -> Result<()> {
    let beads_dir = config::discover_beads_dir(Some(Path::new(".")))?;
    let layer = config::load_config(&beads_dir, None, cli)?;
    let actor = config::resolve_actor(&layer);

    match command {
        AuditCommands::Record(args) => record_entry(args, &beads_dir, &actor, json),
        AuditCommands::Label(args) => label_entry(args, &beads_dir, &actor, json),
        AuditCommands::Log(args) => execute_log(args, &beads_dir, cli, json, ctx),
        AuditCommands::Summary(args) => execute_summary(args, &beads_dir, cli, json, ctx),
    }
}

fn execute_log(
    args: &AuditLogArgs,
    beads_dir: &Path,
    cli: &config::CliOverrides,
    json: bool,
    ctx: &OutputContext,
) -> Result<()> {
    let storage_ctx = config::open_storage_with_cli(beads_dir, cli)?;
    let issue_id = &args.id;
    let events = storage_ctx.storage.get_events(issue_id, 0)?;

    if json {
        let output = AuditLogOutput {
            issue_id: issue_id.clone(),
            events: events.iter().map(map_event_to_output).collect(),
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
        return Ok(());
    }

    if ctx.is_rich() {
        render_audit_log_rich(issue_id, &events, ctx);
    } else {
        render_audit_log_plain(issue_id, &events);
    }

    Ok(())
}

fn execute_summary(
    args: &AuditSummaryArgs,
    beads_dir: &Path,
    cli: &config::CliOverrides,
    json: bool,
    ctx: &OutputContext,
) -> Result<()> {
    let storage_ctx = config::open_storage_with_cli(beads_dir, cli)?;
    let events = storage_ctx.storage.get_all_events(0)?;

    let cutoff = Utc::now() - chrono::Duration::days(args.days as i64);
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
    actors.sort_by(|a, b| b.total.cmp(&a.total));

    if json {
        let output = AuditSummaryOutput {
            period_days: args.days,
            totals,
            actors,
        };
        println!("{}", serde_json::to_string_pretty(&output)?);
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
    }
}

fn record_entry(args: &AuditRecordArgs, beads_dir: &Path, actor: &str, json: bool) -> Result<()> {
    let stdin_piped = !io::stdin().is_terminal();
    let no_fields = no_fields_provided(args);

    let use_stdin = if args.stdin {
        true
    } else if stdin_piped && no_fields {
        false
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

    let mut line = serde_json::to_vec(&entry)?;
    line.push(b'\n');

    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;

    file.write_all(&line)?;

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

fn render_audit_log_rich(issue_id: &str, events: &[crate::model::Event], ctx: &OutputContext) {
    let console = Console::default();
    let theme = ctx.theme();
    let width = ctx.width();

    let mut content = Text::new("");
    content.append("\n");

    for event in events {
        // Timestamp + Actor
        let time_str = event.created_at.format("%Y-%m-%d %H:%M").to_string();
        content.append_styled(&time_str, theme.dimmed.clone());
        content.append("  ");
        content.append_styled(&format!("@{:<10}", event.actor), theme.accent.clone());
        content.append("  ");

        // Event Type
        let type_style = event_type_style(&event.event_type, theme);
        content.append_styled(&format!("{:<15}", event.event_type.as_str()), type_style);
        content.append("\n");

        // Details
        let mut details = String::new();
        if let Some(old) = &event.old_value {
            if let Some(new) = &event.new_value {
                details.push_str(&format!("   {} → {}", old, new));
            } else {
                details.push_str(&format!("   Removed: {}", old));
            }
        } else if let Some(new) = &event.new_value {
            details.push_str(&format!("   Set: {}", new));
        }

        if !details.is_empty() {
            content.append_styled(&details, theme.dimmed.clone());
            content.append("\n");
        }

        if let Some(comment) = &event.comment {
            content.append_styled(&format!("   \"{}\"\n", comment), theme.comment.clone());
        }

        content.append("\n");
    }

    let panel = Panel::from_rich_text(&content, width)
        .title(Text::styled(
            format!("Audit Log: {}", issue_id),
            theme.panel_title.clone(),
        ))
        .box_style(theme.box_style);

    console.print_renderable(&panel);
}

fn render_audit_log_plain(issue_id: &str, events: &[crate::model::Event]) {
    println!("Audit Log: {}", issue_id);
    println!("{}", "-".repeat(40));

    for event in events {
        println!(
            "{}  @{:<10}  {}",
            event.created_at.format("%Y-%m-%d %H:%M"),
            event.actor,
            event.event_type.as_str()
        );

        if let Some(old) = &event.old_value {
            if let Some(new) = &event.new_value {
                println!("   {} -> {}", old, new);
            } else {
                println!("   Removed: {}", old);
            }
        } else if let Some(new) = &event.new_value {
            println!("   Set: {}", new);
        }

        if let Some(comment) = &event.comment {
            println!("   \"{}\"", comment);
        }
        println!();
    }
}

fn render_audit_summary_rich(
    days: u32,
    totals: &AuditTotals,
    actors: &[ActorSummary],
    ctx: &OutputContext,
) {
    let console = Console::default();
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

    console.print_renderable(&panel);
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
