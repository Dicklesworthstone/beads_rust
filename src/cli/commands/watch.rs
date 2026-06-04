//! Watch a prefix for bead state changes and / or incoming messages.
//!
//! Polls the database on an interval. Bead events are debounced and grouped
//! by the bead's `sender` field so a stager dripping a batch of beads
//! produces one rollup per sender. The initial snapshot is silent.

use crate::cli::{OutputFormat, WatchArgs, resolve_output_format_basic};
use crate::config;
use crate::error::{BeadsError, Result};
use crate::model::Status;
use crate::output::OutputContext;
use crate::storage::ListFilters;
use crate::util::id::split_prefix_remainder;
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::thread;
use std::time::Duration;

/// Drop guard that unregisters this `bd watch` from the `watchers`
/// table on clean shutdown. Best-effort — failures are silenced (the
/// row will age out via TTL if we can't reach the DB anymore).
struct WatcherGuard {
    beads_dir: PathBuf,
    prefix: String,
    pid: i64,
    cli: config::CliOverrides,
}

impl Drop for WatcherGuard {
    fn drop(&mut self) {
        if let Ok((mut storage, _paths)) =
            config::open_storage(&self.beads_dir, self.cli.db.as_ref(), self.cli.lock_timeout)
        {
            let _ = storage.unregister_watcher(&self.prefix, self.pid);
        }
    }
}

#[derive(Clone, PartialEq, Eq)]
struct BeadState {
    status: Status,
    title: String,
    sender: Option<String>,
    created_by: Option<String>,
}

#[derive(Clone)]
enum BatchChange {
    Created(BeadState),
    StatusChanged { from: Status, current: BeadState },
    Deleted(BeadState),
}

#[derive(Clone)]
struct SenderBatch {
    sender: Option<String>,
    changes: HashMap<String, BatchChange>,
    batch_start: DateTime<Utc>,
    last_event: DateTime<Utc>,
}

impl SenderBatch {
    fn new(sender: Option<String>, now: DateTime<Utc>) -> Self {
        Self {
            sender,
            changes: HashMap::new(),
            batch_start: now,
            last_event: now,
        }
    }

    fn should_flush(&self, now: DateTime<Utc>, debounce: Duration, debounce_max: Duration) -> bool {
        if self.changes.is_empty() {
            return false;
        }
        let since_last = (now - self.last_event).to_std().unwrap_or(Duration::ZERO);
        let since_start = (now - self.batch_start).to_std().unwrap_or(Duration::ZERO);
        since_last >= debounce || since_start >= debounce_max
    }
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

#[derive(Serialize)]
struct BatchJson<'a> {
    event: &'static str,
    ts: String,
    prefix: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    from: Option<&'a str>,
    count: usize,
    window_secs: i64,
    beads: Vec<BatchBeadJson<'a>>,
}

#[derive(Serialize)]
struct BatchBeadJson<'a> {
    id: &'a str,
    change: &'static str,
    status: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    from_status: Option<&'a str>,
    title: &'a str,
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
    if args.debounce_max < args.debounce {
        return Err(BeadsError::validation(
            "debounce_max",
            "must be >= --debounce",
        ));
    }

    let format = resolve_output_format_basic(args.format, ctx.is_json(), false);
    let interval = Duration::from_secs(args.interval);
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let prefix = resolve_prefix(args, &beads_dir, cli)?;

    // Register this watcher and arrange to unregister on Drop. `bd msg`
    // checks this table to flag typos that would otherwise drop messages
    // into an unwatched queue. Crashed watchers self-evict via TTL.
    let pid = i64::try_from(std::process::id()).unwrap_or(0);
    let my_started_at = Utc::now();
    let startup_reload_gen = {
        let (mut storage, _paths) =
            config::open_storage(&beads_dir, cli.db.as_ref(), cli.lock_timeout)?;
        storage.register_watcher(&prefix, pid, my_started_at)?;
        crate::cli::commands::reload::read_generation(&storage)?
    };
    let _watcher_guard = WatcherGuard {
        beads_dir: beads_dir.clone(),
        prefix: prefix.clone(),
        pid,
        cli: cli.clone(),
    };

    let watch_beads = !args.inbox;
    let watch_inbox = !args.no_inbox;

    let actor = if watch_beads {
        Some(resolved_actor(&beads_dir, cli)?)
    } else {
        None
    };
    let status_filter = parse_status_filter(&args.status)?;
    let debounce = Duration::from_secs(args.debounce);
    let debounce_max = Duration::from_secs(args.debounce_max);
    let streaming = debounce.is_zero();

    let mut bead_snapshot = if watch_beads {
        snapshot_state(&beads_dir, cli, &prefix)?
    } else {
        HashMap::new()
    };
    let mut batches: HashMap<Option<String>, SenderBatch> = HashMap::new();
    let mut seen_msgs: HashSet<String> = if watch_inbox {
        inbox_messages(&beads_dir, cli, &prefix)?
            .into_iter()
            .map(|m| m.id)
            .collect()
    } else {
        HashSet::new()
    };

    let mut tick: u64 = 0;
    loop {
        if let Some(max) = args.max_ticks {
            if tick >= max {
                if watch_beads {
                    flush_batches(
                        &mut batches,
                        &prefix,
                        format,
                        Utc::now(),
                        true,
                        debounce,
                        debounce_max,
                    )?;
                }
                break;
            }
        }
        thread::sleep(interval);
        tick += 1;

        let now = Utc::now();

        // Heartbeat + reload check + supersede check. Best-effort — a
        // single failed write shouldn't kill the watch loop. `bd msg`
        // uses the heartbeat to detect typos like `bd msg infra` vs
        // `infra1`. The reload-gen check lets `bd admin reload` ask
        // running watchers to exit cleanly so a freshly-installed bd
        // binary can take over. The supersede check is newest-wins
        // per prefix: if another bd watch started after this one for
        // the same prefix, exit so the agent stops getting duplicate
        // notifications.
        if let Ok((mut storage, _paths)) =
            config::open_storage(&beads_dir, cli.db.as_ref(), cli.lock_timeout)
        {
            let _ = storage.heartbeat_watcher(&prefix, pid, now);

            if let Ok(current_gen) =
                crate::cli::commands::reload::read_generation(&storage)
                && current_gen > startup_reload_gen
            {
                let stdout = std::io::stdout();
                let mut out = stdout.lock();
                let _ = writeln!(
                    out,
                    "[{}] BD_RELOAD: bd reload requested at {}; exiting so a \
                     new bd watch can pick up the latest binary.",
                    now.to_rfc3339(),
                    chrono::DateTime::<Utc>::from_timestamp(current_gen, 0)
                        .map(|t| t.to_rfc3339())
                        .unwrap_or_else(|| current_gen.to_string())
                );
                if watch_beads {
                    flush_batches(
                        &mut batches,
                        &prefix,
                        format,
                        now,
                        true,
                        debounce,
                        debounce_max,
                    )?;
                }
                break;
            }

            if let Ok(Some(winner)) =
                storage.newest_other_watcher(&prefix, pid, my_started_at)
            {
                let stdout = std::io::stdout();
                let mut out = stdout.lock();
                let _ = writeln!(
                    out,
                    "[{}] BD_SUPERSEDED: another bd watch started for prefix \
                     '{}' at {} (pid {}); exiting to avoid duplicate notifications.",
                    now.to_rfc3339(),
                    prefix,
                    winner.started_at.to_rfc3339(),
                    winner.pid,
                );
                if watch_beads {
                    flush_batches(
                        &mut batches,
                        &prefix,
                        format,
                        now,
                        true,
                        debounce,
                        debounce_max,
                    )?;
                }
                break;
            }
        }

        if watch_beads {
            let current = match snapshot_state(&beads_dir, cli, &prefix) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("watch: bead snapshot failed: {e}");
                    HashMap::new()
                }
            };
            if !current.is_empty() || !bead_snapshot.is_empty() {
                let actor_str = actor.as_deref().unwrap_or("");
                if streaming {
                    stream_diff(
                        &bead_snapshot,
                        &current,
                        status_filter.as_ref(),
                        actor_str,
                        args.include_self,
                        format,
                    )?;
                } else {
                    ingest_diff(
                        &bead_snapshot,
                        &current,
                        status_filter.as_ref(),
                        actor_str,
                        args.include_self,
                        now,
                        &mut batches,
                    );
                    flush_batches(
                        &mut batches,
                        &prefix,
                        format,
                        now,
                        false,
                        debounce,
                        debounce_max,
                    )?;
                }
                bead_snapshot = current;
            }
        }

        if watch_inbox {
            match inbox_messages(&beads_dir, cli, &prefix) {
                Ok(messages) => {
                    let stdout = std::io::stdout();
                    let mut out = stdout.lock();
                    for msg in &messages {
                        if seen_msgs.insert(msg.id.clone()) {
                            emit_message_event(&mut out, msg, format)?;
                        }
                    }
                    out.flush().ok();
                }
                Err(e) => {
                    eprintln!("watch: inbox snapshot failed: {e}");
                }
            }
        }
    }
    Ok(())
}

/// Streaming mode: emit one event per diff immediately.
fn stream_diff(
    prev: &HashMap<String, BeadState>,
    curr: &HashMap<String, BeadState>,
    status_filter: Option<&HashSet<Status>>,
    actor: &str,
    include_self: bool,
    format: OutputFormat,
) -> Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    for (id, state) in curr {
        if !include_self && is_self(state, actor) {
            continue;
        }
        match prev.get(id) {
            None => {
                if status_filter.is_none_or(|f| f.contains(&state.status)) {
                    emit_single_event(&mut out, "created", id, state, None, format)?;
                }
            }
            Some(prev_state) if prev_state.status != state.status => {
                let prev_matches = status_filter.is_none_or(|f| f.contains(&prev_state.status));
                let curr_matches = status_filter.is_none_or(|f| f.contains(&state.status));
                if prev_matches || curr_matches {
                    let kind = status_event_kind(&state.status);
                    emit_single_event(&mut out, kind, id, state, Some(&prev_state.status), format)?;
                }
            }
            _ => {}
        }
    }

    for (id, state) in prev {
        if !include_self && is_self(state, actor) {
            continue;
        }
        if !curr.contains_key(id) && status_filter.is_none_or(|f| f.contains(&state.status)) {
            emit_single_event(&mut out, "deleted", id, state, Some(&state.status), format)?;
        }
    }

    out.flush().ok();
    Ok(())
}

/// Debounced mode: accrue per-sender net diffs into batches for later flushing.
fn ingest_diff(
    prev: &HashMap<String, BeadState>,
    curr: &HashMap<String, BeadState>,
    status_filter: Option<&HashSet<Status>>,
    actor: &str,
    include_self: bool,
    now: DateTime<Utc>,
    batches: &mut HashMap<Option<String>, SenderBatch>,
) {
    for (id, state) in curr {
        if !include_self && is_self(state, actor) {
            continue;
        }
        match prev.get(id) {
            None => {
                if status_filter.is_none_or(|f| f.contains(&state.status)) {
                    record_change(
                        batches,
                        state.sender.clone(),
                        id,
                        BatchChange::Created(state.clone()),
                        now,
                    );
                }
            }
            Some(prev_state) if prev_state.status != state.status => {
                let prev_matches = status_filter.is_none_or(|f| f.contains(&prev_state.status));
                let curr_matches = status_filter.is_none_or(|f| f.contains(&state.status));
                if prev_matches || curr_matches {
                    record_change(
                        batches,
                        state.sender.clone(),
                        id,
                        BatchChange::StatusChanged {
                            from: prev_state.status.clone(),
                            current: state.clone(),
                        },
                        now,
                    );
                }
            }
            _ => {}
        }
    }

    for (id, state) in prev {
        if !include_self && is_self(state, actor) {
            continue;
        }
        if !curr.contains_key(id) && status_filter.is_none_or(|f| f.contains(&state.status)) {
            record_change(
                batches,
                state.sender.clone(),
                id,
                BatchChange::Deleted(state.clone()),
                now,
            );
        }
    }
}

/// Collapse a new change for `id` into the existing batch, applying net-delta
/// semantics so create-then-delete (etc.) disappears.
fn record_change(
    batches: &mut HashMap<Option<String>, SenderBatch>,
    sender: Option<String>,
    id: &str,
    change: BatchChange,
    now: DateTime<Utc>,
) {
    let batch = batches
        .entry(sender.clone())
        .or_insert_with(|| SenderBatch::new(sender, now));

    let collapsed = match (batch.changes.remove(id), change) {
        (Some(BatchChange::Created(_)), BatchChange::Deleted(_)) => None,
        (Some(BatchChange::Created(_)), BatchChange::StatusChanged { current, .. }) => {
            Some(BatchChange::Created(current))
        }
        (Some(BatchChange::StatusChanged { from, .. }), BatchChange::StatusChanged { current, .. }) => {
            if from == current.status {
                None
            } else {
                Some(BatchChange::StatusChanged { from, current })
            }
        }
        (Some(BatchChange::StatusChanged { current, .. }), BatchChange::Deleted(_)) => {
            Some(BatchChange::Deleted(current))
        }
        (Some(BatchChange::Deleted(_)), BatchChange::Created(state)) => {
            Some(BatchChange::Created(state))
        }
        (None, change) => Some(change),
        (_, change) => Some(change),
    };

    if let Some(c) = collapsed {
        batch.changes.insert(id.to_string(), c);
    }
    batch.last_event = now;
}

fn flush_batches(
    batches: &mut HashMap<Option<String>, SenderBatch>,
    prefix: &str,
    format: OutputFormat,
    now: DateTime<Utc>,
    force: bool,
    debounce: Duration,
    debounce_max: Duration,
) -> Result<()> {
    let ready: Vec<Option<String>> = batches
        .iter()
        .filter(|(_, b)| !b.changes.is_empty() && (force || b.should_flush(now, debounce, debounce_max)))
        .map(|(k, _)| k.clone())
        .collect();
    for key in ready {
        if let Some(batch) = batches.remove(&key) {
            emit_batch(prefix, &batch, now, format)?;
        }
    }
    // Drop empty batches that may have been collapsed to zero.
    batches.retain(|_, b| !b.changes.is_empty());
    Ok(())
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

/// Strict prefix resolution for `bd watch`.
///
/// Unlike the rest of the CLI, watch refuses to fall back to project / user
/// config or the default "bd". Reasoning: when an agent boots and starts a
/// watch, the harness is *supposed* to set BD_ISSUE_PREFIX. Silently watching
/// the wrong prefix would mean missing notifications addressed to the agent.
fn resolve_prefix(args: &WatchArgs, _beads_dir: &Path, _cli: &config::CliOverrides) -> Result<String> {
    if let Some(p) = args
        .prefix
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        return Ok(p.to_string());
    }
    if let Ok(env_prefix) = std::env::var("BD_ISSUE_PREFIX") {
        let trimmed = env_prefix.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    Err(BeadsError::validation(
        "prefix",
        "BD_ISSUE_PREFIX is not set and --prefix was not supplied. \
         Set BD_ISSUE_PREFIX in the agent environment so watch knows \
         which inbox to monitor.",
    ))
}

fn resolved_actor(beads_dir: &Path, cli: &config::CliOverrides) -> Result<String> {
    let (storage, _paths) = config::open_storage(beads_dir, cli.db.as_ref(), cli.lock_timeout)?;
    let layer = config::load_config(beads_dir, Some(&storage), cli)?;
    Ok(config::resolve_actor(&layer))
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
                created_by: issue.created_by,
            },
        );
    }
    Ok(map)
}

fn id_has_prefix(id: &str, prefix: &str) -> bool {
    split_prefix_remainder(id).is_some_and(|(p, _)| p == prefix)
}

fn is_self(state: &BeadState, actor: &str) -> bool {
    state.sender.is_none() && state.created_by.as_deref().is_some_and(|c| c == actor)
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

fn emit_single_event<W: Write>(
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
                ts,
                id,
                event,
                status: status_str,
                from_status: from_status_str,
                title: &state.title,
                from: state.sender.as_deref(),
            };
            writeln!(out, "{}", serde_json::to_string(&ev)?)?;
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

fn emit_batch(
    prefix: &str,
    batch: &SenderBatch,
    now: DateTime<Utc>,
    format: OutputFormat,
) -> Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    let window_secs = (now - batch.batch_start).num_seconds().max(0);
    let mut entries: Vec<(&String, &BatchChange)> = batch.changes.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));

    let from_label = batch.sender.as_deref();
    match format {
        OutputFormat::Json | OutputFormat::Toon => {
            let beads: Vec<BatchBeadJson> = entries
                .iter()
                .map(|(id, change)| match change {
                    BatchChange::Created(s) => BatchBeadJson {
                        id: id.as_str(),
                        change: "created",
                        status: s.status.as_str(),
                        from_status: None,
                        title: &s.title,
                    },
                    BatchChange::StatusChanged { from, current } => BatchBeadJson {
                        id: id.as_str(),
                        change: "status_changed",
                        status: current.status.as_str(),
                        from_status: Some(from.as_str()),
                        title: &current.title,
                    },
                    BatchChange::Deleted(s) => BatchBeadJson {
                        id: id.as_str(),
                        change: "deleted",
                        status: s.status.as_str(),
                        from_status: None,
                        title: &s.title,
                    },
                })
                .collect();
            let obj = BatchJson {
                event: "batch",
                ts: now.to_rfc3339(),
                prefix,
                from: from_label,
                count: beads.len(),
                window_secs,
                beads,
            };
            writeln!(out, "{}", serde_json::to_string(&obj)?)?;
        }
        _ => {
            let from_part = from_label
                .map(|s| format!(" from {s}"))
                .unwrap_or_else(|| " from self".to_string());
            let ts = now.to_rfc3339();
            writeln!(
                out,
                "[{ts}] prefix={prefix} — {n} beads{from_part} (debounced {window_secs}s):",
                n = entries.len()
            )?;
            for (id, change) in entries {
                match change {
                    BatchChange::Created(s) => writeln!(
                        out,
                        "  + {id} created ({status}): {title}",
                        status = s.status.as_str(),
                        title = s.title
                    )?,
                    BatchChange::StatusChanged { from, current } => writeln!(
                        out,
                        "  ~ {id} {from} → {to}: {title}",
                        from = from.as_str(),
                        to = current.status.as_str(),
                        title = current.title
                    )?,
                    BatchChange::Deleted(s) => writeln!(
                        out,
                        "  - {id} deleted (was: {prev}): {title}",
                        prev = s.status.as_str(),
                        title = s.title
                    )?,
                }
            }
        }
    }
    out.flush().ok();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bead(
        status: Status,
        title: &str,
        sender: Option<&str>,
        created_by: Option<&str>,
    ) -> BeadState {
        BeadState {
            status,
            title: title.to_string(),
            sender: sender.map(String::from),
            created_by: created_by.map(String::from),
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
    fn is_self_requires_no_sender_and_matching_creator() {
        let me = bead(Status::Open, "x", None, Some("toad"));
        let theirs = bead(Status::Open, "x", Some("app2"), Some("toad"));
        let other = bead(Status::Open, "x", None, Some("other"));
        assert!(is_self(&me, "toad"));
        assert!(!is_self(&theirs, "toad"));
        assert!(!is_self(&other, "toad"));
    }

    #[test]
    fn batch_groups_changes_by_sender() {
        let now = Utc::now();
        let mut batches = HashMap::new();
        let prev = HashMap::new();
        let mut curr = HashMap::new();

        curr.insert(
            "p-1".to_string(),
            bead(Status::Open, "a", Some("app2"), Some("alice")),
        );
        curr.insert(
            "p-2".to_string(),
            bead(Status::Open, "b", Some("app3"), Some("bob")),
        );
        curr.insert(
            "p-3".to_string(),
            bead(Status::Open, "c", Some("app2"), Some("alice")),
        );

        ingest_diff(&prev, &curr, None, "me", true, now, &mut batches);

        assert_eq!(batches.len(), 2);
        assert_eq!(
            batches.get(&Some("app2".to_string())).unwrap().changes.len(),
            2
        );
        assert_eq!(
            batches.get(&Some("app3".to_string())).unwrap().changes.len(),
            1
        );
    }

    #[test]
    fn self_filter_drops_own_creates_keeps_foreign() {
        let now = Utc::now();
        let mut batches = HashMap::new();
        let prev = HashMap::new();
        let mut curr = HashMap::new();
        curr.insert(
            "p-1".to_string(),
            bead(Status::Open, "mine", None, Some("toad")),
        );
        curr.insert(
            "p-2".to_string(),
            bead(Status::Open, "theirs", Some("app2"), Some("toad")),
        );

        ingest_diff(&prev, &curr, None, "toad", false, now, &mut batches);

        assert_eq!(batches.len(), 1);
        assert!(batches.contains_key(&Some("app2".to_string())));
    }

    #[test]
    fn collapse_create_then_delete_drops_event() {
        let now = Utc::now();
        let mut batches = HashMap::new();
        let state = bead(Status::Open, "t", Some("app2"), Some("alice"));

        record_change(
            &mut batches,
            Some("app2".into()),
            "p-1",
            BatchChange::Created(state.clone()),
            now,
        );
        assert_eq!(
            batches.get(&Some("app2".to_string())).unwrap().changes.len(),
            1
        );

        record_change(
            &mut batches,
            Some("app2".into()),
            "p-1",
            BatchChange::Deleted(state),
            now,
        );
        assert!(
            batches
                .get(&Some("app2".to_string()))
                .map(|b| b.changes.is_empty())
                .unwrap_or(true)
        );
    }

    #[test]
    fn collapse_status_roundtrip_drops_event() {
        let now = Utc::now();
        let mut batches = HashMap::new();
        let open_state = bead(Status::Open, "t", Some("app2"), Some("alice"));
        let deferred_state = bead(Status::Deferred, "t", Some("app2"), Some("alice"));

        record_change(
            &mut batches,
            Some("app2".into()),
            "p-1",
            BatchChange::StatusChanged {
                from: Status::Open,
                current: deferred_state,
            },
            now,
        );
        record_change(
            &mut batches,
            Some("app2".into()),
            "p-1",
            BatchChange::StatusChanged {
                from: Status::Deferred,
                current: open_state,
            },
            now,
        );

        assert!(
            batches
                .get(&Some("app2".to_string()))
                .map(|b| b.changes.is_empty())
                .unwrap_or(true)
        );
    }

    #[test]
    fn should_flush_on_quiet_window() {
        let start = Utc::now();
        let mut b = SenderBatch::new(Some("app2".into()), start);
        b.changes.insert(
            "p-1".to_string(),
            BatchChange::Created(bead(Status::Open, "x", Some("app2"), Some("alice"))),
        );
        assert!(!b.should_flush(start, Duration::from_secs(120), Duration::from_secs(600)));
        let later = start + chrono::Duration::seconds(121);
        assert!(b.should_flush(later, Duration::from_secs(120), Duration::from_secs(600)));
    }

    #[test]
    fn should_flush_on_max_age_ceiling() {
        let start = Utc::now();
        let mut b = SenderBatch::new(Some("app2".into()), start);
        b.changes.insert(
            "p-1".to_string(),
            BatchChange::Created(bead(Status::Open, "x", Some("app2"), Some("alice"))),
        );
        let still_dripping = start + chrono::Duration::seconds(550);
        b.last_event = still_dripping;
        assert!(!b.should_flush(
            still_dripping + chrono::Duration::seconds(30),
            Duration::from_secs(120),
            Duration::from_secs(600)
        ));
        let past_ceiling = start + chrono::Duration::seconds(610);
        b.last_event = past_ceiling;
        assert!(b.should_flush(
            past_ceiling,
            Duration::from_secs(120),
            Duration::from_secs(600)
        ));
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
    }
}
