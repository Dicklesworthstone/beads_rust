//! `bd admin watch` — interactive REPL for the human operator.
//!
//! On startup, replays unread messages addressed to `operator` since
//! the persistent cursor `operator_last_seen_at`. For each message,
//! prompts `[r]eply / [s]kip / [q]uit`. After the backlog drains, the
//! command keeps polling and prompting as new messages arrive.
//!
//! Single-instance: refuses to start when another `bd admin watch`
//! has a fresh heartbeat. Reuses the same `watchers` table the
//! agent-side `bd watch` populates, scoped to `prefix=operator`.
//!
//! Sending a reply goes through the same path as `bd admin msg`:
//! `from = operator`, recipient-online gate disabled.

use crate::config::{self, OPERATOR_PREFIX};
use crate::error::{BeadsError, Result};
use crate::model::Message;
use crate::output::OutputContext;
use crate::storage::SqliteStorage;
use crate::storage::messages::{MessageFilter, generate_message_id};
/// Tighter freshness window for the single-instance check than the
/// `bd msg` typo guard's 60s. Heartbeat lands every
/// `POLL_INTERVAL_SECS`, so 10s leaves room for one missed beat plus
/// slack while still catching crashed / Ctrl-C'd instances quickly
/// — Drop guards don't run on signal-kill, so we'd otherwise lock the
/// operator out for 60s after every untidy exit.
const SINGLE_INSTANCE_TTL_SECS: i64 = 10;
use chrono::{DateTime, Utc};
use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

/// Config key that persists the operator's read-cursor across runs.
/// Stores an RFC3339 timestamp; messages with `sent_at > cursor`
/// are shown as unread on the next `bd admin watch` startup.
const CURSOR_KEY: &str = "operator_last_seen_at";

/// How often to poll for new messages once the unread backlog is
/// drained. Snappy enough that the operator notices new traffic
/// quickly, cheap enough to be fine all day.
const POLL_INTERVAL_SECS: u64 = 2;

/// Drop guard that unregisters the operator watcher row on clean
/// shutdown. Best-effort — if the DB is unreachable at exit, the row
/// will age out via TTL like any other watcher.
struct WatcherGuard {
    beads_dir: PathBuf,
    pid: i64,
    cli: config::CliOverrides,
}

impl Drop for WatcherGuard {
    fn drop(&mut self) {
        if let Ok((mut storage, _paths)) =
            config::open_storage(&self.beads_dir, self.cli.db.as_ref(), self.cli.lock_timeout)
        {
            let _ = storage.unregister_watcher(OPERATOR_PREFIX, self.pid);
        }
    }
}

/// Execute the operator-side interactive watch.
///
/// # Errors
///
/// Returns an error if storage open fails or the single-instance lock
/// is held by another `bd admin watch` process. REPL itself tolerates
/// malformed input (loops back to the prompt).
pub fn execute(cli: &config::CliOverrides, _ctx: &OutputContext) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;

    // Single-instance: refuse if another `bd admin watch` is still
    // sending heartbeats within SINGLE_INSTANCE_TTL_SECS. Stale rows
    // from prior crashed / Ctrl-C'd instances simply won't match the
    // query (Drop guards don't run on signal-kill); they age out via
    // the normal 60s sweep elsewhere. We deliberately don't sweep
    // here at the tighter 10s TTL, since that would also kick the
    // agent-side `bd watch` rows that `bd msg` depends on.
    let now = Utc::now();
    if storage_ctx
        .storage
        .is_prefix_watched(OPERATOR_PREFIX, now, SINGLE_INSTANCE_TTL_SECS)?
    {
        return Err(BeadsError::validation(
            "operator",
            "another `bd admin watch` is already running. Exit that one \
             first (or wait a few seconds for a crashed instance's \
             heartbeat to expire).",
        ));
    }

    let pid = i64::try_from(std::process::id()).unwrap_or(0);
    let started_at = Utc::now();
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.into_os_string().into_string().ok())
        .unwrap_or_default();
    // Registration is just the first heartbeat — heartbeat_watcher is
    // a self-healing UPSERT (see storage::watchers::heartbeat), so
    // there's no separate one-shot register step.
    storage_ctx.storage.heartbeat_watcher(
        OPERATOR_PREFIX,
        pid,
        started_at,
        started_at,
        &cwd,
        "",
    )?;
    let _guard = WatcherGuard {
        beads_dir: beads_dir.clone(),
        pid,
        cli: cli.clone(),
    };

    let stdout = std::io::stdout();
    let stdin = std::io::stdin();
    let mut out = stdout.lock();
    let mut input = stdin.lock();

    writeln!(
        out,
        "operator watch (pid {pid}). poll {POLL_INTERVAL_SECS}s. [q] at any prompt to exit."
    )?;

    // First-ever run: no cursor persisted yet. Default to epoch so the
    // operator sees their full unread history on the first
    // `bd admin watch`, rather than silently skipping any
    // messages that piled up before this command existed. Subsequent
    // runs use the persisted cursor from the last session.
    let mut cursor = read_cursor(&storage_ctx.storage)?.unwrap_or_else(|| {
        DateTime::<Utc>::from_timestamp(0, 0).unwrap_or(started_at)
    });
    cursor = drain_pending(
        &mut storage_ctx.storage,
        &mut out,
        &mut input,
        cursor,
        Some("unread on entry"),
    )?;

    // Live tail loop. Between drains we sleep, heartbeat, and check
    // for supersede. Ctrl-C exits cleanly via the Drop guard.
    loop {
        thread::sleep(Duration::from_secs(POLL_INTERVAL_SECS));
        let now = Utc::now();
        let ttl = crate::storage::watchers::WATCHER_TTL_SECONDS;
        let _ = storage_ctx.storage.sweep_stale_watchers(now, ttl);
        // Supersede check MUST run before the heartbeat UPSERT below —
        // `watchers` keys on prefix alone, so heartbeating first would
        // claim the row before we ever got to see who held it. See
        // the `bd watch` tick loop (cli/commands/watch.rs) for the
        // full rationale; this REPL mirrors the same ordering.
        if let Ok(Some(winner)) = storage_ctx
            .storage
            .newest_other_watcher(OPERATOR_PREFIX, pid, started_at, now, ttl)
        {
            writeln!(
                out,
                "\nBD_SUPERSEDED: another `bd admin watch` started at {} \
                 (pid {}); exiting.",
                winner.started_at.to_rfc3339(),
                winner.pid,
            )?;
            // Persist cursor before exit so the new instance picks up
            // from the same point.
            let _ = write_cursor(&mut storage_ctx.storage, cursor);
            return Ok(());
        }
        let _ =
            storage_ctx
                .storage
                .heartbeat_watcher(OPERATOR_PREFIX, pid, started_at, now, &cwd, "");
        cursor = drain_pending(
            &mut storage_ctx.storage,
            &mut out,
            &mut input,
            cursor,
            None,
        )?;
    }
}

/// Process all messages addressed to operator with `sent_at > cursor`,
/// returning the advanced cursor. `header` (when set) is printed when
/// the backlog is non-empty — used for the entry-time "unread on
/// entry (N):" banner.
fn drain_pending<R: BufRead, W: Write>(
    storage: &mut SqliteStorage,
    out: &mut W,
    input: &mut R,
    mut cursor: DateTime<Utc>,
    header: Option<&str>,
) -> Result<DateTime<Utc>> {
    let pending = list_pending_after(storage, cursor)?;
    if pending.is_empty() {
        return Ok(cursor);
    }

    if let Some(label) = header {
        writeln!(out, "\n{label} ({}):", pending.len())?;
    }

    for msg in pending {
        // If the message arrived during the loop and is older than
        // cursor (race), skip it.
        if msg.sent_at <= cursor {
            continue;
        }
        match handle_message(storage, out, input, &msg)? {
            Action::Replied | Action::Skipped => {
                cursor = msg.sent_at;
                if let Err(e) = write_cursor(storage, cursor) {
                    writeln!(out, "  (cursor persist failed: {e})")?;
                }
            }
            Action::Quit => {
                // Don't advance cursor for the message that was just
                // shown — operator hit q without choosing. Persist
                // whatever cursor we had, then exit cleanly.
                let _ = write_cursor(storage, cursor);
                std::process::exit(0);
            }
        }
    }
    Ok(cursor)
}

fn list_pending_after(storage: &SqliteStorage, cursor: DateTime<Utc>) -> Result<Vec<Message>> {
    let filter = MessageFilter {
        to_prefix: Some(OPERATOR_PREFIX.to_string()),
        from_prefix: None,
        only_unread: false,
        limit: None,
        only_asks: None,
    };
    let mut messages = storage.list_messages(&filter)?;
    // list_messages returns newest-first; flip so we present chronologically.
    messages.sort_by_key(|m| m.sent_at);
    messages.retain(|m| m.sent_at > cursor);
    Ok(messages)
}

enum Action {
    Replied,
    Skipped,
    Quit,
}

fn handle_message<R: BufRead, W: Write>(
    storage: &mut SqliteStorage,
    out: &mut W,
    input: &mut R,
    msg: &Message,
) -> Result<Action> {
    let age = format_age_compact(seconds_since(msg.sent_at));
    writeln!(out)?;
    writeln!(out, "  ── {} from {} ({age} ago)", msg.id, msg.from_prefix)?;
    for line in msg.body.lines() {
        writeln!(out, "  │ {line}")?;
    }
    loop {
        write!(out, "  [r]eply / [s]kip / [q]uit > ")?;
        out.flush()?;
        let mut line = String::new();
        let n = input.read_line(&mut line)?;
        if n == 0 {
            // EOF (Ctrl-D) — treat as quit.
            writeln!(out)?;
            return Ok(Action::Quit);
        }
        let trimmed = line.trim().to_ascii_lowercase();
        match trimmed.as_str() {
            "r" | "reply" => match read_reply_body(out, input)? {
                Some(body) => {
                    let now = Utc::now();
                    let reply = build_reply(storage, msg, body, now)?;
                    storage.insert_message(&reply)?;
                    storage.mark_message_read(&msg.id, now)?;
                    writeln!(out, "  → sent {} to {}", reply.id, msg.from_prefix)?;
                    return Ok(Action::Replied);
                }
                None => {
                    writeln!(out, "  (empty body — back to prompt)")?;
                    continue;
                }
            },
            "s" | "skip" | "" => return Ok(Action::Skipped),
            "q" | "quit" => return Ok(Action::Quit),
            _ => writeln!(out, "  (unrecognized — type r, s, or q)")?,
        }
    }
}

/// Read a single-line reply from the operator. Returns `None` on
/// empty input (operator changed their mind). For multi-line bodies
/// the operator can pipe a heredoc via `bd admin msg` later.
fn read_reply_body<R: BufRead, W: Write>(
    out: &mut W,
    input: &mut R,
) -> Result<Option<String>> {
    write!(out, "  reply > ")?;
    out.flush()?;
    let mut buf = String::new();
    let n = input.read_line(&mut buf)?;
    if n == 0 {
        return Ok(None);
    }
    let body = buf.trim().to_string();
    if body.is_empty() {
        Ok(None)
    } else {
        Ok(Some(body))
    }
}

fn build_reply(
    storage: &SqliteStorage,
    original: &Message,
    body: String,
    now: DateTime<Utc>,
) -> Result<Message> {
    let id = pick_reply_id(storage, OPERATOR_PREFIX, &original.from_prefix, &body, now)?;
    Ok(Message {
        id,
        from_prefix: OPERATOR_PREFIX.to_string(),
        to_prefix: original.from_prefix.clone(),
        body,
        sent_at: now,
        read_at: None,
        in_reply_to: Some(original.id.clone()),
        choices: None,
    })
}

fn pick_reply_id(
    storage: &SqliteStorage,
    from: &str,
    to: &str,
    body: &str,
    now: DateTime<Utc>,
) -> Result<String> {
    for nonce in 0..1000 {
        let candidate = generate_message_id(from, to, body, now, nonce);
        if !storage.message_id_exists(&candidate)? {
            return Ok(candidate);
        }
    }
    Err(BeadsError::validation(
        "id",
        "could not allocate a unique reply ID after 1000 attempts",
    ))
}

fn read_cursor(storage: &SqliteStorage) -> Result<Option<DateTime<Utc>>> {
    let Some(raw) = storage.get_config(CURSOR_KEY)? else {
        return Ok(None);
    };
    DateTime::parse_from_rfc3339(&raw)
        .map(|dt| Some(dt.with_timezone(&Utc)))
        .or(Ok(None))
}

fn write_cursor(storage: &mut SqliteStorage, value: DateTime<Utc>) -> Result<()> {
    storage.set_config(CURSOR_KEY, &value.to_rfc3339())
}

fn seconds_since(t: DateTime<Utc>) -> i64 {
    Utc::now().signed_duration_since(t).num_seconds()
}

fn format_age_compact(secs: i64) -> String {
    if secs < 0 {
        return "now".to_string();
    }
    if secs < 60 {
        return format!("{secs}s");
    }
    let m = secs / 60;
    if m < 60 {
        return format!("{m}m");
    }
    let h = m / 60;
    if h < 24 {
        return format!("{h}h");
    }
    let d = h / 24;
    if d < 7 {
        return format!("{d}d");
    }
    let w = d / 7;
    format!("{w}w")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::SqliteStorage;

    fn test_storage() -> SqliteStorage {
        SqliteStorage::open_memory().unwrap()
    }

    fn make_msg(id: &str, from: &str, body: &str, sent_at: DateTime<Utc>) -> Message {
        Message {
            id: id.to_string(),
            from_prefix: from.to_string(),
            to_prefix: OPERATOR_PREFIX.to_string(),
            body: body.to_string(),
            sent_at,
            read_at: None,
            in_reply_to: None,
            choices: None,
        }
    }

    #[test]
    fn cursor_round_trips() {
        let mut s = test_storage();
        assert!(read_cursor(&s).unwrap().is_none());
        let t = Utc::now();
        write_cursor(&mut s, t).unwrap();
        let read = read_cursor(&s).unwrap().unwrap();
        // RFC3339 round-trip preserves to ns; equality should hold.
        assert_eq!(read.timestamp(), t.timestamp());
    }

    #[test]
    fn list_pending_after_filters_by_cursor_and_orders_oldest_first() {
        let mut s = test_storage();
        let base = Utc::now() - chrono::Duration::seconds(300);
        s.insert_message(&make_msg("m1", "arc3", "a", base)).unwrap();
        s.insert_message(&make_msg(
            "m2",
            "arc3",
            "b",
            base + chrono::Duration::seconds(10),
        ))
        .unwrap();
        s.insert_message(&make_msg(
            "m3",
            "beads1",
            "c",
            base + chrono::Duration::seconds(20),
        ))
        .unwrap();

        let after_m1 = list_pending_after(&s, base).unwrap();
        assert_eq!(after_m1.len(), 2);
        assert_eq!(after_m1[0].id, "m2");
        assert_eq!(after_m1[1].id, "m3");

        let after_m3 = list_pending_after(&s, base + chrono::Duration::seconds(20)).unwrap();
        assert!(after_m3.is_empty());
    }

    #[test]
    fn list_pending_after_scopes_to_operator() {
        let mut s = test_storage();
        let base = Utc::now() - chrono::Duration::seconds(60);
        s.insert_message(&make_msg("m1", "arc3", "to op", base))
            .unwrap();
        // Message to a different prefix should NOT appear.
        let mut other = make_msg("m2", "arc3", "to other", base + chrono::Duration::seconds(5));
        other.to_prefix = "arc1".to_string();
        s.insert_message(&other).unwrap();

        let pending = list_pending_after(&s, base - chrono::Duration::seconds(1)).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].id, "m1");
    }

    #[test]
    fn format_age_compact_units() {
        assert_eq!(format_age_compact(-5), "now");
        assert_eq!(format_age_compact(0), "0s");
        assert_eq!(format_age_compact(45), "45s");
        assert_eq!(format_age_compact(60), "1m");
        assert_eq!(format_age_compact(3600), "1h");
        assert_eq!(format_age_compact(86_400), "1d");
        assert_eq!(format_age_compact(7 * 86_400), "1w");
    }

    #[test]
    fn handle_message_skip_action() {
        let mut s = test_storage();
        let msg = make_msg("m1", "arc3", "hello", Utc::now());
        let mut out = Vec::new();
        let mut input = std::io::Cursor::new(b"s\n".to_vec());
        let action = handle_message(&mut s, &mut out, &mut input, &msg).unwrap();
        assert!(matches!(action, Action::Skipped));
    }

    #[test]
    fn handle_message_reply_inserts_message_and_marks_read() {
        let mut s = test_storage();
        let msg = make_msg("m1", "arc3", "hi", Utc::now());
        s.insert_message(&msg).unwrap();
        let mut out = Vec::new();
        // r → reply body → newline ends
        let mut input = std::io::Cursor::new(b"r\nok proceeding. Out\n".to_vec());
        let action = handle_message(&mut s, &mut out, &mut input, &msg).unwrap();
        assert!(matches!(action, Action::Replied));

        // Original ask should now be read.
        let orig = s.get_message("m1").unwrap().unwrap();
        assert!(orig.read_at.is_some());

        // Reply should be a new message from=operator, to=arc3,
        // in_reply_to=m1.
        let filter = MessageFilter {
            from_prefix: Some(OPERATOR_PREFIX.to_string()),
            to_prefix: Some("arc3".to_string()),
            ..Default::default()
        };
        let outbox = s.list_messages(&filter).unwrap();
        assert_eq!(outbox.len(), 1);
        assert_eq!(outbox[0].body, "ok proceeding. Out");
        assert_eq!(outbox[0].in_reply_to.as_deref(), Some("m1"));
    }

    #[test]
    fn handle_message_quit_action() {
        let mut s = test_storage();
        let msg = make_msg("m1", "arc3", "hi", Utc::now());
        let mut out = Vec::new();
        let mut input = std::io::Cursor::new(b"q\n".to_vec());
        let action = handle_message(&mut s, &mut out, &mut input, &msg).unwrap();
        assert!(matches!(action, Action::Quit));
    }
}
