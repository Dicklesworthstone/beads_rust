//! Read-only client for ghwatch's `state.db`.
//!
//! ghwatch is a sibling tool that watches GitHub Actions (and later
//! k8s pod rollouts) for the operator's repos. It writes a per-(repo,
//! branch, kind) snapshot into its own sqlite at
//! `$XDG_STATE_HOME/ghwatch/state.db`. bd's TUI dashboard opens it
//! read-only on every refresh tick and joins against the watcher's
//! canonical `git_remote` so each agent row can show its current CI
//! state.
//!
//! Contract surfaces (owned by ghwatch — see their repo for the
//! authoritative schema):
//! - `watch_state` view: one row per (repo, branch, kind), non-superseded.
//! - `transitions`: append-only event log (not consumed in v2).
//! - `meta(key TEXT PK, value TEXT)`: schema_version + last_observed_at.
//!
//! All failures are silenced — a missing db, a schema mismatch, a
//! stale heartbeat are all rendered as "no CI data this tick" rather
//! than killing the refresh loop. Liveness gets surfaced as a
//! "ghwatch silent for Xm" footer when meta says so.

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use std::collections::HashMap;
use std::path::PathBuf;

/// Schema version this build expects ghwatch to write. ghwatch bumps
/// this on incompatible changes only (drops/renames), additive
/// changes leave it alone. Mismatch → render an explanatory line in
/// the dashboard, skip the CI data this tick.
pub const SUPPORTED_SCHEMA_VERSION: i64 = 2;

/// If ghwatch's `meta.last_observed_at` is older than this, surface
/// "ghwatch silent for Xm" in the dashboard footer. Lines up with the
/// operator's expectation that ghwatch ticks more often than every
/// few minutes; longer-than-this likely means the process died.
pub const SILENT_AFTER_SECONDS: i64 = 300;

/// Snapshot of one row from ghwatch's `watch_state` view, keyed by
/// canonical repo. We keep only the fields the dashboard renders
/// today; ghwatch may surface more.
#[derive(Debug, Clone)]
pub struct CiRow {
    pub repo: String,
    pub branch: String,
    pub kind: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub workflow: Option<String>,
    pub url: Option<String>,
    pub source_id: Option<String>,
    pub updated_at: Option<DateTime<Utc>>,
    pub observed_at: Option<DateTime<Utc>>,
}

/// Aggregate CI state for the dashboard, plus any soft errors the
/// operator should know about. `ci_rows` is keyed by canonical
/// `repo`; the dashboard joins against `watchers.git_remote`.
#[derive(Debug, Default, Clone)]
pub struct GhwatchStatus {
    pub ci_rows: HashMap<String, Vec<CiRow>>,
    /// When schema_version doesn't match what bd expects, set to a
    /// short human string like "ghwatch schema v3, bd expects v2".
    /// Dashboard renders it; ci_rows stays empty so we don't risk
    /// misinterpreting fields.
    pub schema_mismatch: Option<String>,
    /// Seconds since the last successful ghwatch fetch, if the
    /// `last_observed_at` row is present and older than
    /// `SILENT_AFTER_SECONDS`. Dashboard surfaces "silent for Xm".
    pub silent_for_secs: Option<i64>,
}

impl GhwatchStatus {
    /// True when there's nothing for the dashboard to render — neither
    /// data nor a notable error. Lets callers skip allocations.
    #[must_use]
    pub fn is_quiet(&self) -> bool {
        self.ci_rows.is_empty()
            && self.schema_mismatch.is_none()
            && self.silent_for_secs.is_none()
    }
}

/// Resolve the path to ghwatch's state.db, honoring XDG. Returns
/// None when neither candidate path exists; the dashboard treats
/// that as "ghwatch not installed, nothing to show."
fn discover_state_db() -> Option<PathBuf> {
    let candidates = [
        std::env::var_os("XDG_STATE_HOME")
            .map(PathBuf::from)
            .map(|p| p.join("ghwatch").join("state.db")),
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .map(|p| p.join(".local/state/ghwatch/state.db")),
    ];
    for c in candidates.into_iter().flatten() {
        if c.is_file() {
            return Some(c);
        }
    }
    None
}

/// Open ghwatch's state.db read-only, query everything the dashboard
/// renders, and return the assembled `GhwatchStatus`. Any error
/// short-circuits to an empty/quiet status — the dashboard never
/// crashes on this path.
#[must_use]
pub fn read_status() -> GhwatchStatus {
    let Some(path) = discover_state_db() else {
        return GhwatchStatus::default();
    };
    match read_status_at(&path) {
        Ok(s) => s,
        Err(_) => GhwatchStatus::default(),
    }
}

fn read_status_at(path: &std::path::Path) -> rusqlite::Result<GhwatchStatus> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;

    let mut status = GhwatchStatus::default();

    // 1. schema_version check — mismatch wins; we don't attempt to
    // read the data, because the column shapes may have shifted under
    // us. Older minor versions (additive changes only) are fine.
    let version = meta_int(&conn, "schema_version").unwrap_or(0);
    if version != 0 && version != SUPPORTED_SCHEMA_VERSION {
        status.schema_mismatch = Some(format!(
            "ghwatch schema v{version}, bd expects v{SUPPORTED_SCHEMA_VERSION}"
        ));
        return Ok(status);
    }

    // 2. Liveness — last_observed_at is updated every successful
    // ghwatch fetch. If it's stale, surface "silent for Xm" in the
    // footer (but we still serve any data we already have).
    if let Some(last) = meta_timestamp(&conn, "last_observed_at") {
        let age = (Utc::now() - last).num_seconds();
        if age > SILENT_AFTER_SECONDS {
            status.silent_for_secs = Some(age);
        }
    }

    // 3. The actual rows.
    let mut stmt = conn.prepare(
        "SELECT repo, branch, kind, status, conclusion, workflow, url,
                source_id, updated_at, observed_at
         FROM watch_state",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(CiRow {
            repo: row.get("repo")?,
            branch: row.get::<_, Option<String>>("branch")?.unwrap_or_default(),
            kind: row.get::<_, Option<String>>("kind")?.unwrap_or_default(),
            status: row.get::<_, Option<String>>("status")?.unwrap_or_default(),
            conclusion: row.get::<_, Option<String>>("conclusion")?,
            workflow: row.get::<_, Option<String>>("workflow")?,
            url: row.get::<_, Option<String>>("url")?,
            source_id: row.get::<_, Option<String>>("source_id")?,
            updated_at: row
                .get::<_, Option<String>>("updated_at")?
                .and_then(|s| parse_ts(&s)),
            observed_at: row
                .get::<_, Option<String>>("observed_at")?
                .and_then(|s| parse_ts(&s)),
        })
    })?;

    for r in rows.flatten() {
        status.ci_rows.entry(r.repo.clone()).or_default().push(r);
    }

    Ok(status)
}

fn meta_int(conn: &Connection, key: &str) -> Option<i64> {
    conn.prepare_cached("SELECT value FROM meta WHERE key = ?1")
        .ok()?
        .query_row([key], |row| row.get::<_, String>(0))
        .optional()
        .ok()
        .flatten()
        .and_then(|s| s.parse::<i64>().ok())
}

fn meta_timestamp(conn: &Connection, key: &str) -> Option<DateTime<Utc>> {
    let raw: String = conn
        .prepare_cached("SELECT value FROM meta WHERE key = ?1")
        .ok()?
        .query_row([key], |row| row.get::<_, String>(0))
        .optional()
        .ok()
        .flatten()?;
    parse_ts(&raw)
}

fn parse_ts(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}
