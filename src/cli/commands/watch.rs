//! Watch a prefix for create / status-change events.
//!
//! Polls the database on an interval and emits one event per state change
//! for beads whose ID has the given prefix. The initial snapshot is silent;
//! only subsequent changes produce output.

use crate::cli::{OutputFormat, WatchArgs, resolve_output_format_basic};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::model::Status;
use crate::output::OutputContext;
use crate::storage::ListFilters;
use crate::util::id::split_prefix_remainder;
use chrono::Utc;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;
use std::str::FromStr;
use std::thread;
use std::time::Duration;

#[derive(Clone)]
struct BeadState {
    status: Status,
    title: String,
    sender: Option<String>,
}

#[derive(Serialize)]
struct EventJson<'a> {
    ts: String,
    id: &'a str,
    event: &'a str,
    status: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    from_status: Option<&'a str>,
    title: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<&'a str>,
}

/// Execute the watch command.
///
/// # Errors
///
/// Returns an error if the initial snapshot cannot be taken or arguments are invalid.
pub fn execute(args: &WatchArgs, cli: &config::CliOverrides, ctx: &OutputContext) -> Result<()> {
    if args.interval < 1 {
        return Err(BeadsError::validation("interval", "must be >= 1 second"));
    }

    let format = resolve_output_format_basic(args.format, ctx.is_json(), false);
    let interval = Duration::from_secs(args.interval);
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let prefix = resolve_prefix(args, &beads_dir, cli)?;

    if args.inbox {
        return watch_inbox(args, cli, &beads_dir, &prefix, interval, format);
    }

    let status_filter = parse_status_filter(&args.status)?;
    let mut snapshot = snapshot_state(&beads_dir, cli, &prefix)?;

    let mut tick: u64 = 0;
    loop {
        if let Some(max) = args.max_ticks {
            if tick >= max {
                break;
            }
        }
        thread::sleep(interval);
        tick += 1;

        let current = match snapshot_state(&beads_dir, cli, &prefix) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("watch: snapshot failed: {e}");
                continue;
            }
        };

        emit_diff(&snapshot, &current, status_filter.as_ref(), format)?;
        snapshot = current;
    }
    Ok(())
}

/// Watch the inbox for new incoming messages addressed to the resolved prefix.
fn watch_inbox(
    args: &WatchArgs,
    cli: &config::CliOverrides,
    beads_dir: &Path,
    prefix: &str,
    interval: Duration,
    format: OutputFormat,
) -> Result<()> {
    use crate::storage::messages::MessageFilter;

    let initial = inbox_snapshot(beads_dir, cli, prefix)?;
    let mut seen: HashSet<String> = initial.into_iter().collect();

    let mut tick: u64 = 0;
    loop {
        if let Some(max) = args.max_ticks {
            if tick >= max {
                break;
            }
        }
        thread::sleep(interval);
        tick += 1;

        let messages = match inbox_messages(beads_dir, cli, prefix) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("watch: inbox snapshot failed: {e}");
                continue;
            }
        };

        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        for msg in &messages {
            if seen.insert(msg.id.clone()) {
                emit_message_event(&mut out, msg, format)?;
            }
        }
        out.flush().ok();

        let _ = MessageFilter::default(); // touch import for unused-warning safety
    }
    Ok(())
}

fn inbox_snapshot(
    beads_dir: &Path,
    cli: &config::CliOverrides,
    prefix: &str,
) -> Result<Vec<String>> {
    Ok(inbox_messages(beads_dir, cli, prefix)?
        .into_iter()
        .map(|m| m.id)
        .collect())
}

fn inbox_messages(
    beads_dir: &Path,
    cli: &config::CliOverrides,
    prefix: &str,
) -> Result<Vec<crate::model::Message>> {
    use crate::storage::messages::MessageFilter;
    let (storage, _paths) = config::open_storage(beads_dir, cli.db.as_ref(), cli.lock_timeout)?;
    let filter = MessageFilter {
        to_prefix: Some(prefix.to_string()),
        ..Default::default()
    };
    storage.list_messages(&filter)
}

fn emit_message_event<W: Write>(
    out: &mut W,
    msg: &crate::model::Message,
    format: OutputFormat,
) -> Result<()> {
    let event = if msg.in_reply_to.is_some() {
        "message_replied"
    } else {
        "message_received"
    };

    match format {
        OutputFormat::Json | OutputFormat::Toon => {
            let line = serde_json::to_string(&serde_json::json!({
                "ts": Utc::now().to_rfc3339(),
                "event": event,
                "id": msg.id,
                "from": msg.from_prefix,
                "to": msg.to_prefix,
                "sent_at": msg.sent_at.to_rfc3339(),
                "in_reply_to": msg.in_reply_to,
                "body_preview": msg.body.chars().take(200).collect::<String>(),
                "truncated": msg.body.len() > 200,
            }))?;
            writeln!(out, "{line}")?;
        }
        _ => {
            let preview: String = msg.body.chars().take(200).collect();
            let truncated = msg.body.len() > 200;
            let reply_part = msg
                .in_reply_to
                .as_ref()
                .map(|r| format!(" ↪{r}"))
                .unwrap_or_default();
            writeln!(
                out,
                "[{ts}] {id} from {from}{reply_part} {event}: {preview}",
                ts = Utc::now().to_rfc3339(),
                id = msg.id,
                from = msg.from_prefix,
            )?;
            if truncated {
                writeln!(
                    out,
                    "  ... [truncated; run `bd inbox {id}` for the rest]",
                    id = msg.id
                )?;
            }
        }
    }
    Ok(())
}

fn resolve_prefix(
    args: &WatchArgs,
    beads_dir: &Path,
    cli: &config::CliOverrides,
) -> Result<String> {
    if let Some(p) = args.prefix.as_ref().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        return Ok(p.to_string());
    }
    let (storage, _paths) = config::open_storage(beads_dir, cli.db.as_ref(), cli.lock_timeout)?;
    let layer = config::load_config(beads_dir, Some(&storage), cli)?;
    let prefix = config::id_config_from_layer(&layer).prefix;
    if prefix.trim().is_empty() {
        return Err(BeadsError::validation(
            "prefix",
            "could not resolve a prefix from --prefix, BD_ISSUE_PREFIX, or config",
        ));
    }
    Ok(prefix)
}

fn parse_status_filter(raw: &[String]) -> Result<Option<HashSet<Status>>> {
    if raw.is_empty() {
        return Ok(None);
    }
    let mut set = HashSet::new();
    for s in raw {
        let trimmed = s.trim();
        if trimmed.is_empty() {
            continue;
        }
        set.insert(Status::from_str(trimmed)?);
    }
    if set.is_empty() {
        Ok(None)
    } else {
        Ok(Some(set))
    }
}

fn snapshot_state(
    beads_dir: &Path,
    cli: &config::CliOverrides,
    prefix: &str,
) -> Result<HashMap<String, BeadState>> {
    let (storage, _paths) = config::open_storage(beads_dir, cli.db.as_ref(), cli.lock_timeout)?;
    let filters = ListFilters {
        include_closed: true,
        include_deferred: true,
        ..Default::default()
    };
    let mut map = HashMap::new();
    for issue in storage.list_issues(&filters)? {
        if !id_has_prefix(&issue.id, prefix) {
            continue;
        }
        map.insert(
            issue.id.clone(),
            BeadState {
                status: issue.status,
                title: issue.title,
                sender: issue.sender,
            },
        );
    }
    Ok(map)
}

fn id_has_prefix(id: &str, prefix: &str) -> bool {
    split_prefix_remainder(id).is_some_and(|(p, _)| p == prefix)
}

fn emit_diff(
    prev: &HashMap<String, BeadState>,
    curr: &HashMap<String, BeadState>,
    status_filter: Option<&HashSet<Status>>,
    format: OutputFormat,
) -> Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    for (id, state) in curr {
        match prev.get(id) {
            None => {
                if status_filter.is_none_or(|f| f.contains(&state.status)) {
                    emit_event(&mut out, "created", id, state, None, format)?;
                }
            }
            Some(prev_state) if prev_state.status != state.status => {
                let prev_matches = status_filter.is_none_or(|f| f.contains(&prev_state.status));
                let curr_matches = status_filter.is_none_or(|f| f.contains(&state.status));
                if prev_matches || curr_matches {
                    let kind = status_event_kind(&state.status);
                    emit_event(&mut out, kind, id, state, Some(&prev_state.status), format)?;
                }
            }
            _ => {}
        }
    }

    for (id, state) in prev {
        if !curr.contains_key(id)
            && status_filter.is_none_or(|f| f.contains(&state.status))
        {
            emit_event(&mut out, "deleted", id, state, Some(&state.status), format)?;
        }
    }

    out.flush().ok();
    Ok(())
}

fn status_event_kind(status: &Status) -> &'static str {
    match status {
        Status::Open => "opened",
        Status::InProgress => "started",
        Status::Closed => "closed",
        Status::Deferred => "deferred",
        Status::Blocked => "blocked",
        Status::Tombstone => "deleted",
        Status::Pinned => "pinned",
        Status::Custom(_) => "status_changed",
    }
}

fn emit_event<W: Write>(
    out: &mut W,
    event: &str,
    id: &str,
    state: &BeadState,
    from_status: Option<&Status>,
    format: OutputFormat,
) -> Result<()> {
    let ts = Utc::now().to_rfc3339();
    let status_str = state.status.as_str();
    let from_status_str = from_status.map(Status::as_str);

    match format {
        OutputFormat::Json | OutputFormat::Toon => {
            let ev = EventJson {
                ts: ts.clone(),
                id,
                event,
                status: status_str,
                from_status: from_status_str,
                title: &state.title,
                from: state.sender.as_deref(),
            };
            let line = serde_json::to_string(&ev)?;
            writeln!(out, "{line}")?;
        }
        _ => {
            let from_part = state
                .sender
                .as_ref()
                .map(|s| format!(" from {s}"))
                .unwrap_or_default();
            let detail = match from_status_str {
                Some(prev) if event != "deleted" && event != "created" => format!(" (was: {prev})"),
                _ => String::new(),
            };
            writeln!(
                out,
                "[{ts}] {id}{from_part} {event}{detail}: {title}",
                title = state.title
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(status: Status, title: &str, sender: Option<&str>) -> BeadState {
        BeadState {
            status,
            title: title.to_string(),
            sender: sender.map(String::from),
        }
    }

    #[test]
    fn id_has_prefix_matches_only_full_prefix() {
        assert!(id_has_prefix("app1-abc", "app1"));
        assert!(!id_has_prefix("app10-abc", "app1"));
        assert!(!id_has_prefix("app1abc", "app1"));
        assert!(!id_has_prefix("app1", "app1"));
    }

    #[test]
    fn diff_emits_created_for_new_bead() {
        let prev: HashMap<String, BeadState> = HashMap::new();
        let mut curr: HashMap<String, BeadState> = HashMap::new();
        curr.insert("bd-a".to_string(), state(Status::Open, "x", None));
        let mut buf = Vec::new();
        for (id, st) in &curr {
            if prev.get(id).is_none() {
                emit_event(&mut buf, "created", id, st, None, OutputFormat::Json).unwrap();
            }
        }
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("\"event\":\"created\""));
        assert!(s.contains("\"id\":\"bd-a\""));
    }

    #[test]
    fn diff_emits_closed_on_status_change() {
        let mut prev = HashMap::new();
        prev.insert("bd-a".to_string(), state(Status::Open, "x", None));
        let mut curr = HashMap::new();
        curr.insert("bd-a".to_string(), state(Status::Closed, "x", None));
        let mut buf = Vec::new();
        for (id, st) in &curr {
            if let Some(prev_st) = prev.get(id) {
                if prev_st.status != st.status {
                    emit_event(
                        &mut buf,
                        status_event_kind(&st.status),
                        id,
                        st,
                        Some(&prev_st.status),
                        OutputFormat::Json,
                    )
                    .unwrap();
                }
            }
        }
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("\"event\":\"closed\""));
        assert!(s.contains("\"from_status\":\"open\""));
    }

    #[test]
    fn text_format_includes_sender_when_present() {
        let mut buf = Vec::new();
        emit_event(
            &mut buf,
            "created",
            "app2-abc",
            &state(Status::Open, "fix typo", Some("app1")),
            None,
            OutputFormat::Text,
        )
        .unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("app2-abc"));
        assert!(s.contains("from app1"));
        assert!(s.contains("created"));
        assert!(s.contains("fix typo"));
    }

    #[test]
    fn parse_status_filter_empty_returns_none() {
        assert!(parse_status_filter(&[]).unwrap().is_none());
        assert!(
            parse_status_filter(&["".to_string(), "  ".to_string()])
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn parse_status_filter_parses_set() {
        let set = parse_status_filter(&["open".to_string(), "deferred".to_string()])
            .unwrap()
            .unwrap();
        assert!(set.contains(&Status::Open));
        assert!(set.contains(&Status::Deferred));
        assert!(!set.contains(&Status::Closed));
    }
}
