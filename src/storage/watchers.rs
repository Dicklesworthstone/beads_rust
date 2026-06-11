//! Active `bd watch` heartbeat tracking.
//!
//! Each running `bd watch` process owns a row in the `watchers` table
//! and updates `last_seen` on every poll tick. `bd msg` checks this
//! table to detect typos like `bd msg infra` when the live prefix is
//! `infra1` — messages to non-watching prefixes would otherwise drop
//! silently. Crashed watchers self-evict via TTL; clean shutdowns
//! call [`unregister`] from a Drop guard.

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::Result;

/// Default time after which a watcher row is considered stale. The
/// `bd watch` poll interval default is 2s; this gives ~30 missed ticks
/// of slack before we treat the watcher as dead.
pub const WATCHER_TTL_SECONDS: i64 = 60;

/// One active watcher process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatcherRow {
    pub prefix: String,
    pub pid: i64,
    pub started_at: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    /// Working directory the `bd watch` process was launched from.
    /// Empty when discovery failed (no permission, etc.).
    pub cwd: String,
    /// Canonical `git remote get-url origin` of the cwd, normalized
    /// to `host/owner/repo`. Empty when there's no git checkout,
    /// no `origin` remote, or `git` isn't on PATH. Used by the
    /// dashboard to join against `ghwatch.watch_state.repo`.
    pub git_remote: String,
}

/// Register a new watcher (prefix, pid). If a row already exists for
/// this pair (e.g., a watcher restarted with the same PID), reset
/// `started_at` / `last_seen` / `cwd` / `git_remote` to the new values.
///
/// # Errors
///
/// Returns an error if the DB write fails.
pub fn register(
    conn: &Connection,
    prefix: &str,
    pid: i64,
    now: DateTime<Utc>,
    cwd: &str,
    git_remote: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO watchers (prefix, pid, started_at, last_seen, cwd, git_remote)
         VALUES (?1, ?2, ?3, ?3, ?4, ?5)
         ON CONFLICT(prefix, pid) DO UPDATE SET started_at = excluded.started_at,
                                                last_seen = excluded.last_seen,
                                                cwd = excluded.cwd,
                                                git_remote = excluded.git_remote",
        params![prefix, pid, now.to_rfc3339(), cwd, git_remote],
    )?;
    Ok(())
}

/// Update `last_seen` for an existing watcher. No-op if the row was
/// already evicted (e.g., the DB was wiped) — callers shouldn't fail
/// on that.
///
/// # Errors
///
/// Returns an error if the DB write fails.
pub fn heartbeat(conn: &Connection, prefix: &str, pid: i64, now: DateTime<Utc>) -> Result<()> {
    conn.execute(
        "UPDATE watchers SET last_seen = ?1 WHERE prefix = ?2 AND pid = ?3",
        params![now.to_rfc3339(), prefix, pid],
    )?;
    Ok(())
}

/// Remove a watcher row. Called from `bd watch`'s Drop guard on clean
/// shutdown.
///
/// # Errors
///
/// Returns an error if the DB write fails.
pub fn unregister(conn: &Connection, prefix: &str, pid: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM watchers WHERE prefix = ?1 AND pid = ?2",
        params![prefix, pid],
    )?;
    Ok(())
}

/// Whether `prefix` has any watcher with `last_seen` newer than
/// `now - ttl_seconds`.
///
/// # Errors
///
/// Returns an error if the DB query fails.
pub fn is_active(
    conn: &Connection,
    prefix: &str,
    now: DateTime<Utc>,
    ttl_seconds: i64,
) -> Result<bool> {
    let cutoff = now - chrono::Duration::seconds(ttl_seconds);
    let exists: bool = conn
        .prepare_cached(
            "SELECT 1 FROM watchers WHERE prefix = ?1 AND last_seen >= ?2 LIMIT 1",
        )?
        .exists(params![prefix, cutoff.to_rfc3339()])?;
    Ok(exists)
}

/// Distinct prefixes with at least one fresh watcher row.
///
/// # Errors
///
/// Returns an error if the DB query fails.
pub fn active_prefixes(
    conn: &Connection,
    now: DateTime<Utc>,
    ttl_seconds: i64,
) -> Result<Vec<String>> {
    let cutoff = now - chrono::Duration::seconds(ttl_seconds);
    let mut stmt = conn.prepare(
        "SELECT DISTINCT prefix FROM watchers WHERE last_seen >= ?1 ORDER BY prefix",
    )?;
    let rows = stmt
        .query_map([cutoff.to_rfc3339()], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Whether any *other* watcher row for the same prefix has a strictly
/// later `started_at` than `my_started_at`. Used by `bd watch` to
/// implement newest-wins-per-prefix: when an older watcher sees a
/// newer one, it exits to avoid duplicate notifications.
///
/// # Errors
///
/// Returns an error if the DB query fails.
pub fn is_superseded(
    conn: &Connection,
    prefix: &str,
    my_pid: i64,
    my_started_at: DateTime<Utc>,
) -> Result<bool> {
    let exists: bool = conn
        .prepare_cached(
            "SELECT 1 FROM watchers
             WHERE prefix = ?1
               AND pid <> ?2
               AND started_at > ?3
             LIMIT 1",
        )?
        .exists(params![prefix, my_pid, my_started_at.to_rfc3339()])?;
    Ok(exists)
}

/// Find the newest other watcher for `prefix`. Returns None if this
/// is the only / newest watcher. Used to render BD_SUPERSEDED
/// messages with concrete (pid, started_at) of the winner.
///
/// # Errors
///
/// Returns an error if the DB query fails.
pub fn newest_other_watcher(
    conn: &Connection,
    prefix: &str,
    my_pid: i64,
    my_started_at: DateTime<Utc>,
) -> Result<Option<WatcherRow>> {
    let mut stmt = conn.prepare(
        "SELECT prefix, pid, started_at, last_seen, cwd, git_remote FROM watchers
         WHERE prefix = ?1 AND pid <> ?2 AND started_at > ?3
         ORDER BY started_at DESC LIMIT 1",
    )?;
    let row = stmt
        .query_row(
            params![prefix, my_pid, my_started_at.to_rfc3339()],
            row_to_watcher,
        )
        .optional()?;
    Ok(row)
}

/// All watcher rows (regardless of staleness). For diagnostics.
///
/// # Errors
///
/// Returns an error if the DB query fails.
pub fn list_all(conn: &Connection) -> Result<Vec<WatcherRow>> {
    let mut stmt = conn.prepare(
        "SELECT prefix, pid, started_at, last_seen, cwd, git_remote FROM watchers",
    )?;
    let rows = stmt
        .query_map([], row_to_watcher)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Drop watcher rows whose `last_seen` is older than `ttl_seconds`.
/// Called opportunistically by `bd msg` / `bd dash` to keep the table
/// from accumulating crashed-watcher debris.
///
/// # Errors
///
/// Returns an error if the DB delete fails.
pub fn sweep_stale(
    conn: &Connection,
    now: DateTime<Utc>,
    ttl_seconds: i64,
) -> Result<usize> {
    let cutoff = now - chrono::Duration::seconds(ttl_seconds);
    let deleted = conn.execute(
        "DELETE FROM watchers WHERE last_seen < ?1",
        params![cutoff.to_rfc3339()],
    )?;
    Ok(deleted)
}

fn row_to_watcher(row: &rusqlite::Row<'_>) -> rusqlite::Result<WatcherRow> {
    let started: String = row.get("started_at")?;
    let seen: String = row.get("last_seen")?;
    Ok(WatcherRow {
        prefix: row.get("prefix")?,
        pid: row.get("pid")?,
        started_at: parse_db_timestamp(&started),
        last_seen: parse_db_timestamp(&seen),
        // cwd / git_remote columns are only present on databases that
        // went through the ghwatch-integration migration. Treat
        // failure as empty so we stay backward compatible with older
        // schemas that other tooling might still write.
        cwd: row.get::<_, Option<String>>("cwd").ok().flatten().unwrap_or_default(),
        git_remote: row
            .get::<_, Option<String>>("git_remote")
            .ok()
            .flatten()
            .unwrap_or_default(),
    })
}

fn parse_db_timestamp(value: &str) -> DateTime<Utc> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return dt.with_timezone(&Utc);
    }
    if let Ok(naive) = NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S") {
        return Utc.from_utc_datetime(&naive);
    }
    Utc::now()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::schema::apply_schema;

    fn open_mem() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn register_heartbeat_unregister_roundtrip() {
        let conn = open_mem();
        let now = Utc::now();
        register(&conn, "arc1", 42, now, "", "").unwrap();
        assert!(is_active(&conn, "arc1", now, 60).unwrap());

        let later = now + chrono::Duration::seconds(10);
        heartbeat(&conn, "arc1", 42, later).unwrap();
        assert_eq!(list_all(&conn).unwrap()[0].last_seen, later);

        unregister(&conn, "arc1", 42).unwrap();
        assert!(!is_active(&conn, "arc1", later, 60).unwrap());
    }

    #[test]
    fn stale_row_not_active() {
        let conn = open_mem();
        let now = Utc::now();
        register(&conn, "arc1", 1, now - chrono::Duration::seconds(120), "", "").unwrap();
        assert!(!is_active(&conn, "arc1", now, 60).unwrap());
        // But it's still in list_all (we GC via sweep, not on read).
        assert_eq!(list_all(&conn).unwrap().len(), 1);
    }

    #[test]
    fn multiple_pids_per_prefix_allowed() {
        let conn = open_mem();
        let now = Utc::now();
        register(&conn, "arc1", 1, now, "", "").unwrap();
        register(&conn, "arc1", 2, now, "", "").unwrap();
        assert_eq!(list_all(&conn).unwrap().len(), 2);
        assert!(is_active(&conn, "arc1", now, 60).unwrap());

        // active_prefixes dedupes
        let active = active_prefixes(&conn, now, 60).unwrap();
        assert_eq!(active, vec!["arc1".to_string()]);
    }

    #[test]
    fn active_prefixes_skips_stale() {
        let conn = open_mem();
        let now = Utc::now();
        register(&conn, "fresh", 1, now, "", "").unwrap();
        register(&conn, "stale", 2, now - chrono::Duration::seconds(300), "", "").unwrap();
        let active = active_prefixes(&conn, now, 60).unwrap();
        assert_eq!(active, vec!["fresh".to_string()]);
    }

    #[test]
    fn sweep_drops_only_stale() {
        let conn = open_mem();
        let now = Utc::now();
        register(&conn, "fresh", 1, now, "", "").unwrap();
        register(&conn, "stale", 2, now - chrono::Duration::seconds(300), "", "").unwrap();
        let deleted = sweep_stale(&conn, now, 60).unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(list_all(&conn).unwrap().len(), 1);
    }

    #[test]
    fn re_register_resets_timestamps() {
        let conn = open_mem();
        let t1 = Utc::now() - chrono::Duration::seconds(120);
        register(&conn, "arc1", 1, t1, "", "").unwrap();
        let t2 = Utc::now();
        register(&conn, "arc1", 1, t2, "", "").unwrap();
        let row = &list_all(&conn).unwrap()[0];
        assert_eq!(row.started_at, t2);
        assert_eq!(row.last_seen, t2);
    }
}
