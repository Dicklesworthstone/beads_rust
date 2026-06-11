//! `bd who` — list agents currently watching for messages.
//!
//! Reads the `watchers` table populated by `bd watch` heartbeats. This
//! is the *listener* signal — distinct from agent presence
//! (`bd working` / `bd idle`), which tracks whether the agent is
//! actively running a task. An agent can be working without watching,
//! or idle but still listening; `bd msg` cares only about whether
//! they're listening.

use crate::cli::{OutputFormat, WhoArgs, resolve_output_format_basic};
use crate::config;
use crate::error::Result;
use crate::output::OutputContext;
use crate::storage::watchers::{WATCHER_TTL_SECONDS, WatcherRow};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::io::Write;

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
    let fresh: Vec<WatcherRow> = rows
        .into_iter()
        .filter(|r| (now - r.last_seen).num_seconds() <= ttl)
        .collect();

    let format = resolve_output_format_basic(args.format, ctx.is_json(), false);

    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    match format {
        OutputFormat::Json | OutputFormat::Toon => render_json(&mut out, &fresh, now),
        _ => render_text(&mut out, &fresh, args.long_format, now),
    }
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
}

fn render_json<W: Write>(out: &mut W, rows: &[WatcherRow], now: DateTime<Utc>) -> Result<()> {
    let view: Vec<WhoRowJson> = rows
        .iter()
        .map(|r| WhoRowJson {
            prefix: &r.prefix,
            pid: r.pid,
            started_at: r.started_at.to_rfc3339(),
            last_seen: r.last_seen.to_rfc3339(),
            last_seen_secs_ago: (now - r.last_seen).num_seconds().max(0),
            cwd: &r.cwd,
            git_remote: &r.git_remote,
        })
        .collect();
    writeln!(out, "{}", serde_json::to_string(&view)?)?;
    Ok(())
}

fn render_text<W: Write>(
    out: &mut W,
    rows: &[WatcherRow],
    verbose: bool,
    now: DateTime<Utc>,
) -> Result<()> {
    if rows.is_empty() {
        writeln!(out, "(no agents currently watching)")?;
        return Ok(());
    }

    if verbose {
        writeln!(
            out,
            "PREFIX               PID        STARTED              LAST SEEN   GIT REMOTE                              CWD"
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
        // Compact: collapse multiple PIDs per prefix to one line, using the
        // freshest last_seen.
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
