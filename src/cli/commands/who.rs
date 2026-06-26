//! `bd who` — list agents currently watching for messages.
//!
//! Reads the `watchers` table populated by `bd watch` heartbeats. This
//! is the *listener* signal — distinct from agent presence
//! (`bd working` / `bd idle`), which tracks whether the agent is
//! actively running a task. An agent can be working without watching,
//! or idle but still listening; `bd msg` cares only about whether
//! they're listening.
//!
//! The human operator gets a synthetic row pinned at the top, always
//! shown even when no `bd admin watch` is running. Its state tells
//! agents whether a `bd msg operator` is likely to land in front of
//! human eyes soon ([present]), will be queued for the next attend
//! session ([attending]), or won't be read until the operator next
//! checks in ([away]).

use crate::cli::{OutputFormat, WhoArgs, resolve_output_format_basic};
use crate::config::{self, OPERATOR_PREFIX};
use crate::error::Result;
use crate::output::OutputContext;
use crate::storage::SqliteStorage;
use crate::storage::messages::MessageFilter;
use crate::storage::watchers::{WATCHER_TTL_SECONDS, WatcherRow};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::io::Write;

/// Cursor key shared with `bd admin watch` — last timestamp the
/// operator handled a message in the REPL. Imported here so `bd who`
/// can call the operator "present" while they're actively replying.
const OPERATOR_CURSOR_KEY: &str = "operator_last_seen_at";

/// `bd admin watch` running with operator activity (cursor or
/// outbox send) within this window → `[present]`. Otherwise (watch
/// up, no recent activity) → `[attending]`. Tuned so a coffee break
/// drops the badge to `attending` rather than misleading agents.
const OPERATOR_PRESENT_WINDOW_SECS: i64 = 300;

/// Tighter freshness window for "is the operator watching right
/// now" than the general 60s TTL `bd msg` uses for the typo guard.
/// Mirrors the single-instance threshold in `bd admin watch` so the
/// two views agree.
const OPERATOR_WATCH_TTL_SECS: i64 = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OperatorState {
    /// `bd admin watch` is up AND the operator has acknowledged a
    /// message (replied, skipped) or sent within
    /// [`OPERATOR_PRESENT_WINDOW_SECS`].
    Present,
    /// `bd admin watch` is up but no recent activity — the human may
    /// be away from the terminal.
    Attending,
    /// `bd admin watch` is not running. Messages persist for next
    /// time.
    Away,
}

impl OperatorState {
    fn label(self) -> &'static str {
        match self {
            Self::Present => "[present]",
            Self::Attending => "[attending]",
            Self::Away => "[away]",
        }
    }
}

/// Execute `bd who`.
///
/// # Errors
///
/// Returns an error if storage open or query fails.
pub fn execute(args: &WhoArgs, cli: &config::CliOverrides, ctx: &OutputContext) -> Result<()> {
    let beads_dir = config::discover_beads_dir_with_cli(cli)?;
    let mut storage_ctx = config::open_storage_with_cli(&beads_dir, cli)?;

    let ttl = args.ttl.unwrap_or(WATCHER_TTL_SECONDS);
    let now = Utc::now();
    // Opportunistic GC keeps the table tidy.
    let _ = storage_ctx.storage.sweep_stale_watchers(now, ttl);

    let rows = storage_ctx.storage.list_all_watchers()?;
    // Strip any operator watcher rows from the agent list — we render
    // operator separately at the top regardless of presence.
    let (operator_rows, fresh): (Vec<WatcherRow>, Vec<WatcherRow>) = rows
        .into_iter()
        .filter(|r| (now - r.last_seen).num_seconds() <= ttl)
        .partition(|r| r.prefix.eq_ignore_ascii_case(OPERATOR_PREFIX));

    let operator_state = compute_operator_state(&storage_ctx.storage, &operator_rows, now)?;
    let operator_watch_row = operator_rows
        .into_iter()
        .max_by_key(|r| r.last_seen);

    let format = resolve_output_format_basic(args.format, ctx.is_json(), false);

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    match format {
        OutputFormat::Json | OutputFormat::Toon => render_json(
            &mut out,
            &fresh,
            operator_state,
            operator_watch_row.as_ref(),
            now,
        ),
        _ => render_text(
            &mut out,
            &fresh,
            operator_state,
            operator_watch_row.as_ref(),
            args.long_format,
            now,
        ),
    }
}

/// Determine the operator's [`OperatorState`] for the synthetic row.
/// "Present" requires both a fresh `bd admin watch` heartbeat AND
/// some sign the human is actually at the keyboard (cursor advance
/// or outbox send within [`OPERATOR_PRESENT_WINDOW_SECS`]).
fn compute_operator_state(
    storage: &SqliteStorage,
    operator_rows: &[WatcherRow],
    now: DateTime<Utc>,
) -> Result<OperatorState> {
    let watch_up = operator_rows
        .iter()
        .any(|r| (now - r.last_seen).num_seconds() <= OPERATOR_WATCH_TTL_SECS);
    if !watch_up {
        return Ok(OperatorState::Away);
    }
    let last_activity = latest_operator_activity(storage)?;
    let recently_active = last_activity
        .is_some_and(|t| (now - t).num_seconds() <= OPERATOR_PRESENT_WINDOW_SECS);
    Ok(if recently_active {
        OperatorState::Present
    } else {
        OperatorState::Attending
    })
}

/// Latest signal that the human is actively engaged: the persisted
/// `bd admin watch` cursor (advances on each reply or skip) or the
/// most recent outbox send from `operator`. Returns `None` when the
/// operator has never interacted via either path.
fn latest_operator_activity(storage: &SqliteStorage) -> Result<Option<DateTime<Utc>>> {
    let cursor = storage
        .get_config(OPERATOR_CURSOR_KEY)?
        .and_then(|raw| DateTime::parse_from_rfc3339(&raw).ok())
        .map(|dt| dt.with_timezone(&Utc));

    let outbox_filter = MessageFilter {
        from_prefix: Some(OPERATOR_PREFIX.to_string()),
        limit: Some(1),
        ..Default::default()
    };
    let outbox_latest = storage
        .list_messages(&outbox_filter)?
        .into_iter()
        .next()
        .map(|m| m.sent_at);

    Ok(match (cursor, outbox_latest) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    })
}

#[derive(Serialize)]
struct WhoRowJson<'a> {
    prefix: &'a str,
    pid: i64,
    started_at: String,
    last_seen: String,
    last_seen_secs_ago: i64,
    cwd: &'a str,
    git_remote: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<&'static str>,
}

fn render_json<W: Write>(
    out: &mut W,
    rows: &[WatcherRow],
    operator_state: OperatorState,
    operator_row: Option<&WatcherRow>,
    now: DateTime<Utc>,
) -> Result<()> {
    let mut view: Vec<WhoRowJson> = Vec::with_capacity(rows.len() + 1);
    view.push(WhoRowJson {
        prefix: OPERATOR_PREFIX,
        pid: operator_row.map_or(0, |r| r.pid),
        started_at: operator_row
            .map(|r| r.started_at.to_rfc3339())
            .unwrap_or_default(),
        last_seen: operator_row
            .map(|r| r.last_seen.to_rfc3339())
            .unwrap_or_default(),
        last_seen_secs_ago: operator_row
            .map_or(-1, |r| (now - r.last_seen).num_seconds().max(0)),
        cwd: operator_row.map(|r| r.cwd.as_str()).unwrap_or(""),
        git_remote: operator_row.map(|r| r.git_remote.as_str()).unwrap_or(""),
        state: Some(operator_state.label().trim_matches(['[', ']'])),
    });
    for r in rows {
        view.push(WhoRowJson {
            prefix: &r.prefix,
            pid: r.pid,
            started_at: r.started_at.to_rfc3339(),
            last_seen: r.last_seen.to_rfc3339(),
            last_seen_secs_ago: (now - r.last_seen).num_seconds().max(0),
            cwd: &r.cwd,
            git_remote: &r.git_remote,
            state: None,
        });
    }
    writeln!(out, "{}", serde_json::to_string(&view)?)?;
    Ok(())
}

fn render_text<W: Write>(
    out: &mut W,
    rows: &[WatcherRow],
    operator_state: OperatorState,
    operator_row: Option<&WatcherRow>,
    verbose: bool,
    now: DateTime<Utc>,
) -> Result<()> {
    if verbose {
        writeln!(
            out,
            "PREFIX               PID        STARTED              LAST SEEN   GIT REMOTE                              CWD"
        )?;
        // Synthetic operator row first. PID/started/cwd come from the
        // bd admin watch row when present; placeholders otherwise.
        let (op_pid, op_started, op_last_seen, op_cwd) = match operator_row {
            Some(r) => (
                r.pid.to_string(),
                r.started_at.format("%Y-%m-%d %H:%M:%S").to_string(),
                format_age_compact((now - r.last_seen).num_seconds()),
                if r.cwd.is_empty() {
                    "-".to_string()
                } else {
                    tail_path(&r.cwd, 40)
                },
            ),
            None => ("-".into(), "-".into(), "-".into(), "-".into()),
        };
        let state_label = operator_state.label();
        let ago_or_state = if matches!(operator_state, OperatorState::Away) {
            state_label.to_string()
        } else {
            format!("{state_label} ({op_last_seen})")
        };
        writeln!(
            out,
            "{prefix:<20} {pid:<10} {started:<20} {ago:<10}  {remote:<40} {cwd}",
            prefix = OPERATOR_PREFIX,
            pid = op_pid,
            started = op_started,
            ago = ago_or_state,
            remote = "-",
            cwd = op_cwd,
        )?;
        for r in rows {
            let ago = format_age_compact((now - r.last_seen).num_seconds());
            let remote = if r.git_remote.is_empty() {
                "-".to_string()
            } else {
                r.git_remote.clone()
            };
            let cwd = if r.cwd.is_empty() {
                "-".to_string()
            } else {
                tail_path(&r.cwd, 40)
            };
            writeln!(
                out,
                "{prefix:<20} {pid:<10} {started:<20} {ago:<10}  {remote:<40} {cwd}",
                prefix = r.prefix,
                pid = r.pid,
                started = r.started_at.format("%Y-%m-%d %H:%M:%S"),
                ago = format!("{ago} ago"),
            )?;
        }
    } else {
        // Compact: operator row first, then agents collapsed by prefix.
        writeln!(
            out,
            "{prefix:<20} {state}",
            prefix = OPERATOR_PREFIX,
            state = operator_state.label(),
        )?;
        let mut by_prefix: std::collections::BTreeMap<&str, &WatcherRow> =
            std::collections::BTreeMap::new();
        for r in rows {
            by_prefix
                .entry(&r.prefix)
                .and_modify(|cur| {
                    if r.last_seen > cur.last_seen {
                        *cur = r;
                    }
                })
                .or_insert(r);
        }
        for (prefix, r) in &by_prefix {
            let ago = format_age_compact((now - r.last_seen).num_seconds());
            writeln!(out, "{prefix:<20} {ago} ago", prefix = prefix, ago = ago)?;
        }
    }
    Ok(())
}

/// Right-truncate a path to at most `max` chars by keeping the
/// trailing segments and prepending `…/` when truncation occurs.
/// `/home/toad/bit/beads_rust` with max=15 → `…/bit/beads_rust`.
fn tail_path(path: &str, max: usize) -> String {
    if path.chars().count() <= max {
        return path.to_string();
    }
    // Walk the path right-to-left building up segments until adding
    // the next one would exceed the budget.
    let segments: Vec<&str> = path.split('/').collect();
    let mut acc = String::new();
    for seg in segments.iter().rev() {
        let candidate = if acc.is_empty() {
            seg.to_string()
        } else {
            format!("{seg}/{acc}")
        };
        // Account for the `…/` prefix the truncation will add.
        if candidate.chars().count() + 2 > max {
            break;
        }
        acc = candidate;
    }
    if acc.is_empty() {
        // Last resort: keep the rightmost `max-2` chars.
        let suffix: String = path.chars().rev().take(max.saturating_sub(2)).collect();
        let suffix: String = suffix.chars().rev().collect();
        return format!("…{suffix}");
    }
    format!("…/{acc}")
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
    use crate::model::Message;

    fn test_storage() -> SqliteStorage {
        SqliteStorage::open_memory().unwrap()
    }

    fn operator_row_with_last_seen(last_seen: DateTime<Utc>) -> WatcherRow {
        WatcherRow {
            prefix: OPERATOR_PREFIX.to_string(),
            pid: 9999,
            started_at: last_seen,
            last_seen,
            cwd: String::new(),
            git_remote: String::new(),
        }
    }

    #[test]
    fn operator_state_away_when_no_watcher() {
        let s = test_storage();
        let now = Utc::now();
        assert_eq!(
            compute_operator_state(&s, &[], now).unwrap(),
            OperatorState::Away
        );
    }

    #[test]
    fn operator_state_away_when_watcher_stale() {
        let s = test_storage();
        let now = Utc::now();
        let row = operator_row_with_last_seen(now - chrono::Duration::seconds(30));
        assert_eq!(
            compute_operator_state(&s, &[row], now).unwrap(),
            OperatorState::Away
        );
    }

    #[test]
    fn operator_state_attending_when_watch_fresh_no_activity() {
        let s = test_storage();
        let now = Utc::now();
        let row = operator_row_with_last_seen(now - chrono::Duration::seconds(3));
        assert_eq!(
            compute_operator_state(&s, &[row], now).unwrap(),
            OperatorState::Attending
        );
    }

    #[test]
    fn operator_state_present_when_cursor_recent() {
        let mut s = test_storage();
        let now = Utc::now();
        let recent = now - chrono::Duration::seconds(30);
        s.set_config(OPERATOR_CURSOR_KEY, &recent.to_rfc3339()).unwrap();
        let row = operator_row_with_last_seen(now - chrono::Duration::seconds(2));
        assert_eq!(
            compute_operator_state(&s, &[row], now).unwrap(),
            OperatorState::Present
        );
    }

    #[test]
    fn operator_state_present_when_outbox_recent() {
        let mut s = test_storage();
        let now = Utc::now();
        let recent = now - chrono::Duration::seconds(15);
        s.insert_message(&Message {
            id: "msg-x".into(),
            from_prefix: OPERATOR_PREFIX.into(),
            to_prefix: "arc3".into(),
            body: "hi".into(),
            sent_at: recent,
            read_at: None,
            in_reply_to: None,
            choices: None,
        })
        .unwrap();
        let row = operator_row_with_last_seen(now - chrono::Duration::seconds(2));
        assert_eq!(
            compute_operator_state(&s, &[row], now).unwrap(),
            OperatorState::Present
        );
    }

    #[test]
    fn operator_state_attending_when_activity_stale() {
        let mut s = test_storage();
        let now = Utc::now();
        // Activity > 5 min ago: doesn't promote to "present".
        let stale = now - chrono::Duration::seconds(OPERATOR_PRESENT_WINDOW_SECS + 60);
        s.set_config(OPERATOR_CURSOR_KEY, &stale.to_rfc3339()).unwrap();
        let row = operator_row_with_last_seen(now - chrono::Duration::seconds(2));
        assert_eq!(
            compute_operator_state(&s, &[row], now).unwrap(),
            OperatorState::Attending
        );
    }

    #[test]
    fn latest_operator_activity_picks_max() {
        let mut s = test_storage();
        let now = Utc::now();
        let cursor_t = now - chrono::Duration::seconds(120);
        let outbox_t = now - chrono::Duration::seconds(30);
        s.set_config(OPERATOR_CURSOR_KEY, &cursor_t.to_rfc3339())
            .unwrap();
        s.insert_message(&Message {
            id: "msg-y".into(),
            from_prefix: OPERATOR_PREFIX.into(),
            to_prefix: "arc3".into(),
            body: "hi".into(),
            sent_at: outbox_t,
            read_at: None,
            in_reply_to: None,
            choices: None,
        })
        .unwrap();
        let latest = latest_operator_activity(&s).unwrap().unwrap();
        // Allow ±1s for RFC3339 round-trip precision.
        assert!((latest - outbox_t).num_seconds().abs() <= 1);
    }
}
