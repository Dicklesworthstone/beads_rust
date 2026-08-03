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
//
// Long by construction: this is the watch loop itself — argument
// validation, watcher registration, the poll/debounce/flush cycle and
// shutdown, in the order they happen. The locals are shared across every
// stage, so extracting phases would mean inventing a state struct purely
// to satisfy a line count. If this is ever split it should be a
// deliberate refactor with its own review, not a side effect of a lint.
#[allow(clippy::too_many_lines)]
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
    //
    // cwd + git_remote are stored too so the dashboard can map
    // (prefix → repo) for the ghwatch CI integration. Both are
    // best-effort — agents launched outside a git checkout simply
    // record empty strings and get no CI badge.
    // process::id() is u32, so widening to i64 is infallible — the old
    // try_from/unwrap_or(0) could only ever have recorded a bogus pid 0.
    let pid = i64::from(std::process::id());
    let my_started_at = Utc::now();
    let watcher_cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.into_os_string().into_string().ok())
        .unwrap_or_default();
    let watcher_git_remote = discover_git_remote(&watcher_cwd);
    let startup_reload_gen = {
        let (mut storage, _paths) =
            config::open_storage(&beads_dir, cli.db.as_ref(), cli.lock_timeout)?;
        // Registration is just the first heartbeat — heartbeat_watcher
        // is a self-healing UPSERT, so there's no separate one-shot
        // register step (see storage::watchers::heartbeat).
        storage.heartbeat_watcher(
            &prefix,
            pid,
            my_started_at,
            my_started_at,
            &watcher_cwd,
            &watcher_git_remote,
        )?;
        crate::cli::commands::reload::read_generation(&storage)?
    };
    // Anchor against ghwatch's transitions table so we only emit CI
    // notifications for runs that finish after this watch starts.
    // Runs in flight when we boot still surface (their COMPLETED
    // transition lands after, hence id > anchor), but historical
    // outcomes don't re-fire on every bd watch restart.
    let mut seen_transition_id = if watcher_git_remote.is_empty() {
        0
    } else {
        crate::cli::commands::ghwatch::current_max_transition_id()
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
    // Cross-restart inbox cursor.
    //
    // `bd watch` emits each inbox message exactly once, but it never marks
    // messages *read* — only `bd inbox` does. So read/unread state cannot
    // decide what has already been surfaced across a restart: using it,
    // every *unread* message replays on every respawn, flooding the
    // monitor with the entire backlog (weeks of stale messages). Using it
    // the other way (pre-seed the whole inbox as seen) silently swallows
    // messages that arrived while the watch was down.
    //
    // Instead we persist a per-prefix high-water mark: the newest message
    // `sent_at` we have emitted. On restart:
    //   * messages with `sent_at <= cursor` were surfaced in a prior
    //     session -> seed them as seen (don't re-fire);
    //   * messages with `sent_at > cursor` arrived while this watch was
    //     down (crash/restart or supersede-and-respawn window) -> let
    //     them surface on the first poll tick.
    // On the very first run for a prefix (no cursor yet) we seed the
    // cursor at the newest existing message, so we start "from now"
    // rather than replaying history.
    let mut inbox_cursor: DateTime<Utc> = Utc::now();
    let mut seen_msgs: HashSet<String> = HashSet::new();
    if watch_inbox {
        let messages = inbox_messages(&beads_dir, cli, &prefix)?;
        let persisted = read_inbox_cursor(&beads_dir, cli, &prefix);
        let (cursor, seen) = seed_startup(&messages, persisted, Utc::now());
        inbox_cursor = cursor;
        seen_msgs = seen;
        // Persist immediately so a later restart resumes from here even if
        // no new message arrives in the meantime.
        write_inbox_cursor(&beads_dir, cli, &prefix, inbox_cursor);
    }

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
            // Evict dead watcher rows before consulting the supersede
            // table. Without this, a crashed / kill -9'd duplicate
            // watch (which never ran its Drop unregister) — or one
            // that registered a clock-skewed future started_at —
            // lingers with a stale heartbeat and out-ranks us forever,
            // silently knocking this agent's inbox offline. The
            // supersede query below also freshness-gates, but sweeping
            // here keeps the table clean for `bd who` / `bd msg` too.
            let ttl = crate::storage::watchers::WATCHER_TTL_SECONDS;
            let _ = storage.sweep_stale_watchers(now, ttl);

            // Supersede check MUST run before the heartbeat UPSERT
            // below. `watchers` now keys on prefix alone, so a
            // heartbeat is a claim: if we wrote first, this check
            // would only ever see our own row and the older-watcher
            // side would never notice it lost. Checking first (and
            // skipping the write on loss) keeps two racing watchers
            // race-tolerant — they converge to exactly one survivor
            // within a few ticks, and the winner never sees itself as
            // superseded.
            if let Ok(Some(winner)) =
                storage.newest_other_watcher(&prefix, pid, my_started_at, now, ttl)
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

            // Not superseded: (re)claim/refresh our row. Self-healing
            // — if our row was deleted (the historical incident: a
            // racing sweep evicted a perfectly alive watcher because
            // its heartbeat stalled under DB write-lock contention),
            // this recreates it within this single tick rather than
            // silently no-op'ing like the old bare UPDATE did.
            let _ = storage.heartbeat_watcher(
                &prefix,
                pid,
                my_started_at,
                now,
                &watcher_cwd,
                &watcher_git_remote,
            );

            if let Ok(current_gen) =
                crate::cli::commands::reload::read_generation(&storage)
                && current_gen > startup_reload_gen
            {
                // Roll a random sleep in 0..spread seconds before
                // printing BD_RELOAD. Without this, N agents notice
                // the reload on the same tick and re-spawn within the
                // same second, tripping the LLM API rate-limit when
                // they all re-invoke the /bdwatch skill at once.
                let spread = crate::cli::commands::reload::read_spread(&storage)
                    .unwrap_or(crate::cli::commands::reload::DEFAULT_SPREAD_SECS);
                let jitter_secs = if spread == 0 {
                    0
                } else {
                    use rand::Rng;
                    rand::rng().random_range(0..=spread)
                };

                // Drop the storage handle before sleeping so we don't
                // hold a connection open for the jitter duration.
                drop(storage);
                if jitter_secs > 0 {
                    thread::sleep(Duration::from_secs(jitter_secs));
                }

                let now = Utc::now();
                let stdout = std::io::stdout();
                let mut out = stdout.lock();
                let _ = writeln!(
                    out,
                    "[{}] BD_RELOAD: bd reload requested at {} (jitter {}s); \
                     exiting so a new bd watch can pick up the latest binary.",
                    now.to_rfc3339(),
                    chrono::DateTime::<Utc>::from_timestamp(current_gen, 0)
                        .map(|t| t.to_rfc3339())
                        .unwrap_or_else(|| current_gen.to_string()),
                    jitter_secs,
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

        // CI notifications from ghwatch. Each new completed-and-not-
        // superseded run on our repo lands as one stream line. The
        // session-overlap gate (observed_at >= my_started_at) keeps
        // historical outcomes from re-firing on bd watch restart.
        if !watcher_git_remote.is_empty() {
            let transitions = crate::cli::commands::ghwatch::read_new_transitions(
                &watcher_git_remote,
                seen_transition_id,
            );
            for t in transitions {
                seen_transition_id = seen_transition_id.max(t.id);
                let session_overlap = t
                    .observed_at
                    .is_some_and(|ts| ts >= my_started_at);
                if !session_overlap {
                    continue;
                }
                let stdout = std::io::stdout();
                let mut out = stdout.lock();
                emit_ci_transition_event(&mut out, &t, now);
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
                    let mut advanced = false;
                    for msg in &messages {
                        if seen_msgs.insert(msg.id.clone()) {
                            emit_message_event(&mut out, msg, format)?;
                            if msg.sent_at > inbox_cursor {
                                inbox_cursor = msg.sent_at;
                                advanced = true;
                            }
                        }
                    }
                    out.flush().ok();
                    // Advance the persisted high-water mark so the next
                    // respawn resumes here instead of replaying these.
                    if advanced {
                        write_inbox_cursor(&beads_dir, cli, &prefix, inbox_cursor);
                    }
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
        // Anything else (including no prior change for this id) keeps
        // the newest observation as-is.
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

/// Config key holding the per-prefix inbox high-water mark (the newest
/// message `sent_at` this watch has already emitted). Namespaced by
/// prefix so each watched inbox tracks its own cursor.
fn inbox_cursor_key(prefix: &str) -> String {
    format!("watch_inbox_cursor_{prefix}")
}

/// Decide the startup high-water mark and which message ids to pre-seed
/// as already-surfaced, given the current inbox and an optional persisted
/// cursor. Pure, so the restart semantics can be unit-tested without
/// touching storage.
///
/// * With a persisted cursor (a restart): messages at or before it were
///   surfaced in a prior session and are pre-seeded; anything newer
///   arrived while the watch was down and is left to fire on first tick.
/// * Without one (first run for this prefix): the cursor starts at the
///   newest existing message (or `now` if the inbox is empty), so the
///   whole existing backlog is pre-seeded and we start "from now" instead
///   of replaying history.
fn seed_startup(
    messages: &[crate::model::Message],
    persisted: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
) -> (DateTime<Utc>, HashSet<String>) {
    let newest = messages.iter().map(|m| m.sent_at).max();
    let cursor = persisted.or(newest).unwrap_or(now);
    let seen = messages
        .iter()
        .filter(|m| m.sent_at <= cursor)
        .map(|m| m.id.clone())
        .collect();
    (cursor, seen)
}

/// Read the persisted inbox cursor for `prefix`, if any. Any failure
/// (missing key, unparseable value, DB open error) yields `None`, which
/// callers treat as "first run" — never a reason to abort the watch.
fn read_inbox_cursor(
    beads_dir: &Path,
    cli: &config::CliOverrides,
    prefix: &str,
) -> Option<DateTime<Utc>> {
    let (storage, _paths) =
        config::open_storage(beads_dir, cli.db.as_ref(), cli.lock_timeout).ok()?;
    let raw = storage.get_config(&inbox_cursor_key(prefix)).ok()??;
    DateTime::parse_from_rfc3339(&raw)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Persist the inbox cursor for `prefix`. Best-effort: a write failure
/// just means the next restart may replay a few already-seen messages,
/// which is far less harmful than aborting the watch loop.
fn write_inbox_cursor(
    beads_dir: &Path,
    cli: &config::CliOverrides,
    prefix: &str,
    value: DateTime<Utc>,
) {
    if let Ok((mut storage, _paths)) =
        config::open_storage(beads_dir, cli.db.as_ref(), cli.lock_timeout)
    {
        let _ = storage.set_config(&inbox_cursor_key(prefix), &value.to_rfc3339());
    }
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

/// Print a single CI-transition event line. Plain-text format only —
/// the Monitor that wraps `bd watch` surfaces one notification per
/// line regardless of format. Best-effort: write failures are
/// silenced (a missed CI ping shouldn't kill the watch loop).
fn emit_ci_transition_event<W: Write>(
    out: &mut W,
    t: &crate::cli::commands::ghwatch::TransitionRow,
    now: DateTime<Utc>,
) {
    let glyph = match t.to_status.as_str() {
        "success" => "✓",
        "failure" => "✗",
        _ => "•",
    };
    let workflow = t.workflow.as_deref().unwrap_or("(workflow)");
    let url = t
        .url
        .as_deref()
        .map(|u| format!(" {u}"))
        .unwrap_or_default();
    let source = t
        .source_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| format!(" ({s})"))
        .unwrap_or_default();
    let branch = if t.branch.is_empty() {
        String::new()
    } else {
        format!("@{}", t.branch)
    };
    let _ = writeln!(
        out,
        "[{ts}] CI {glyph} {status} {repo}{branch} — {workflow}{source}{url}",
        ts = now.to_rfc3339(),
        status = t.to_status,
        repo = t.repo,
    );
}

// The human-readable firehose keeps a short 200-char preview and points
// the reader at `bd inbox <id>` for the rest. Structured consumers
// (JSON / TOON) are typically other agents that must act on the message
// directly, so they get a very generous cap — a full bead-length body
// survives intact. Both are compared by character count (not bytes) so
// multi-byte text isn't clipped early.
const TEXT_PREVIEW_CHARS: usize = 200;
const STRUCTURED_PREVIEW_CHARS: usize = 100_000;

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

    let body_chars = msg.body.chars().count();

    match format {
        OutputFormat::Json | OutputFormat::Toon => {
            let truncated = body_chars > STRUCTURED_PREVIEW_CHARS;
            let body_preview: String = if truncated {
                msg.body.chars().take(STRUCTURED_PREVIEW_CHARS).collect()
            } else {
                msg.body.clone()
            };
            let line = serde_json::to_string(&serde_json::json!({
                "ts": Utc::now().to_rfc3339(),
                "event": event,
                "id": msg.id,
                "from": msg.from_prefix,
                "to": msg.to_prefix,
                "sent_at": msg.sent_at.to_rfc3339(),
                "in_reply_to": msg.in_reply_to,
                "body_preview": body_preview,
                "truncated": truncated,
            }))?;
            writeln!(out, "{line}")?;
        }
        _ => {
            let preview: String = msg.body.chars().take(TEXT_PREVIEW_CHARS).collect();
            let truncated = body_chars > TEXT_PREVIEW_CHARS;
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

/// Best-effort discovery of the canonical git remote URL for the
/// directory `cwd`. Runs `git -C <cwd> remote get-url origin` and
/// passes the result through the shared `canonicalize_repo_url`
/// normalization so it joins cleanly against ghwatch's `repo`
/// column. Returns empty string on any failure (not a git checkout,
/// no `origin` remote, `git` not on PATH, non-UTF8 output, etc.).
fn discover_git_remote(cwd: &str) -> String {
    if cwd.is_empty() {
        return String::new();
    }
    let output = std::process::Command::new("git")
        .args(["-C", cwd, "remote", "get-url", "origin"])
        .output();
    let stdout = match output {
        Ok(o) if o.status.success() => o.stdout,
        _ => return String::new(),
    };
    let Ok(raw) = String::from_utf8(stdout) else {
        return String::new();
    };
    crate::util::git::canonicalize_repo_url(raw.trim())
}

/// Prefix resolution for `bd watch`.
///
/// Unlike the rest of the CLI, watch refuses to fall back to project / user
/// config or the default "bd". Reasoning: when an agent boots and starts a
/// watch, the harness is *supposed* to set BD_AGENT_ID. Silently watching
/// the wrong prefix would mean missing notifications addressed to the agent.
/// Resolution order: `--prefix` flag, then `BD_AGENT_ID`, then inference
/// from a live `bd watch` already in this process's ancestry (see
/// [`config::resolve_agent_identity_with_storage`]) — e.g. a restarted
/// monitor process that lost its env var but still has a sibling watch
/// running under the same agent host process.
fn resolve_prefix(args: &WatchArgs, beads_dir: &Path, cli: &config::CliOverrides) -> Result<String> {
    let candidate = if let Some(p) = args
        .prefix
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        p.to_string()
    } else {
        let (storage, _paths) = config::open_storage(beads_dir, cli.db.as_ref(), cli.lock_timeout)?;
        config::resolve_agent_identity_with_storage(&storage).map_err(|e| {
            let reason = match &e {
                BeadsError::Validation { reason, .. } => reason.clone(),
                other => other.to_string(),
            };
            BeadsError::validation(
                "prefix",
                format!(
                    "{reason} (bd watch also accepts an explicit --prefix flag \
                     instead of BD_AGENT_ID.)"
                ),
            )
        })?
    };

    if candidate.eq_ignore_ascii_case(config::OPERATOR_PREFIX) {
        return Err(BeadsError::validation(
            "prefix",
            "'operator' is reserved for the human operator; agents cannot \
             watch this prefix. (If you're the human operator, the \
             operator-side surface is `bd admin inbox` / `bd admin msg`.)",
        ));
    }

    Ok(candidate)
}

/// Resolve the actor that [`is_self`] compares `created_by` against.
///
/// This MUST use the storage-aware resolver, not plain
/// [`config::resolve_actor`]: `created_by` is written by
/// [`config::resolve_actor_with_storage`] (agent identity spliced in
/// ahead of `$USER`), so resolving the comparison side without
/// identity makes every self-created bead look foreign — "beads1"
/// (stored) vs "toad" (`$USER`) — and the documented
/// `--include-self`-off default silently stops filtering anything.
/// That was the regression in `beads1-s36s7`: both sides of a `==`
/// must come from the same resolver.
fn resolved_actor(beads_dir: &Path, cli: &config::CliOverrides) -> Result<String> {
    let (storage, _paths) = config::open_storage(beads_dir, cli.db.as_ref(), cli.lock_timeout)?;
    let layer = config::load_config(beads_dir, Some(&storage), cli)?;
    Ok(config::resolve_actor_with_storage(&layer, &storage))
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

    fn msg(id: &str, secs: i64) -> crate::model::Message {
        crate::model::Message {
            id: id.to_string(),
            from_prefix: "other".to_string(),
            to_prefix: "me".to_string(),
            body: "b".to_string(),
            sent_at: chrono::DateTime::from_timestamp(secs, 0).unwrap(),
            read_at: None,
            in_reply_to: None,
            choices: None,
        }
    }

    #[test]
    fn seed_startup_first_run_seeds_whole_backlog() {
        // No persisted cursor: every existing message is pre-seeded so the
        // watch starts "from now" and does not replay the backlog.
        let now = chrono::DateTime::from_timestamp(1000, 0).unwrap();
        let inbox = vec![msg("a", 100), msg("b", 200), msg("c", 300)];
        let (cursor, seen) = seed_startup(&inbox, None, now);
        assert_eq!(cursor, inbox[2].sent_at, "cursor pins to newest message");
        assert_eq!(seen.len(), 3, "all existing messages pre-seeded (no flood)");
    }

    #[test]
    fn seed_startup_empty_inbox_uses_now() {
        let now = chrono::DateTime::from_timestamp(1000, 0).unwrap();
        let (cursor, seen) = seed_startup(&[], None, now);
        assert_eq!(cursor, now);
        assert!(seen.is_empty());
    }

    #[test]
    fn seed_startup_restart_surfaces_only_newer_than_cursor() {
        // Restart with a persisted cursor between the old and the new
        // message: the old one is pre-seeded (already surfaced), the new
        // one that arrived while down is left to fire.
        let now = chrono::DateTime::from_timestamp(9999, 0).unwrap();
        let persisted = chrono::DateTime::from_timestamp(150, 0);
        let inbox = vec![msg("old", 100), msg("new", 200)];
        let (cursor, seen) = seed_startup(&inbox, persisted, now);
        assert_eq!(cursor, persisted.unwrap(), "cursor honors the persisted value");
        assert!(seen.contains("old"), "already-surfaced message stays seen");
        assert!(
            !seen.contains("new"),
            "message queued during downtime must surface, not be swallowed"
        );
    }

    #[test]
    fn seed_startup_restart_at_boundary_is_inclusive() {
        // A message exactly at the cursor was already surfaced (<=).
        let now = chrono::DateTime::from_timestamp(9999, 0).unwrap();
        let persisted = chrono::DateTime::from_timestamp(200, 0);
        let inbox = vec![msg("at", 200), msg("after", 201)];
        let (_cursor, seen) = seed_startup(&inbox, persisted, now);
        assert!(seen.contains("at"), "boundary message is inclusive");
        assert!(!seen.contains("after"));
    }

    #[test]
    fn inbox_cursor_key_is_prefix_namespaced() {
        assert_eq!(inbox_cursor_key("agent3"), "watch_inbox_cursor_agent3");
        assert_ne!(inbox_cursor_key("agent1"), inbox_cursor_key("agent2"));
    }

    #[test]
    fn id_has_prefix_matches_only_full_prefix() {
        assert!(id_has_prefix("app1-abc", "app1"));
        assert!(!id_has_prefix("app10-abc", "app1"));
        assert!(!id_has_prefix("app1abc", "app1"));
        assert!(!id_has_prefix("app1", "app1"));
    }

    /// Pure comparison logic only. Note what this deliberately does
    /// NOT cover: both sides of the `==` are hardcoded here, so it
    /// passes even when the *resolution* of the two sides diverges —
    /// which is exactly how `beads1-s36s7` shipped (`created_by`
    /// resolved with agent identity, the comparison actor without).
    /// The seam is crossed instead by
    /// `tests/e2e_watch_self_filter.rs`, which runs a real `br watch`
    /// against beads written by a real `br create`.
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
            parse_status_filter(&[String::new(), "  ".to_string()])
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
