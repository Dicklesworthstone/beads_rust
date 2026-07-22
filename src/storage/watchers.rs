//! Active `bd watch` heartbeat tracking.
//!
//! Each running `bd watch` process owns the single row for its prefix in
//! the `watchers` table (`PRIMARY KEY (prefix)` — process identity is
//! informational only; see the schema comment for why). `bd msg` checks
//! this table to detect typos like `bd msg infra` when the live prefix
//! is `infra1` — messages to non-watching prefixes would otherwise drop
//! silently.
//!
//! [`heartbeat`] is a *self-healing* UPSERT: every poll tick it writes
//! the full row again (not just `last_seen`). This is deliberate. A
//! bare `UPDATE ... WHERE prefix = ? AND pid = ?` is a no-op against a
//! row that no longer exists — if another process's [`sweep_stale`]
//! deletes a live watcher's row (e.g. because a heartbeat stalled past
//! the TTL under DB write-lock contention), the old bare-UPDATE
//! heartbeat could never bring it back, leaving that watcher invisible
//! to `bd who` / unreachable via `bd msg` forever even though it kept
//! running and kept delivering messages. The UPSERT regenerates the row
//! within one tick regardless of whether it was missing, stale, or
//! claimed by a different (dead) pid.
//!
//! Clean shutdowns call [`unregister`] from a Drop guard; crashed
//! watchers self-evict via TTL through [`sweep_stale`].

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

/// Self-healing heartbeat UPSERT for the (single) row owned by
/// `prefix`.
///
/// Writes `prefix`, `pid`, `last_seen`, `cwd`, `git_remote`
/// unconditionally, and either creates the row or claims/refreshes it
/// via `ON CONFLICT(prefix) DO UPDATE`. This is called both to
/// register a watcher at startup and on every poll tick afterward —
/// there is no separate one-shot "register" step, so a row deleted out
/// from under a live watcher (stale sweep racing a slow heartbeat,
/// manual DB surgery, etc.) regenerates on the very next tick.
///
/// `started_at` semantics: if the existing row already belongs to
/// `pid` (this is just a routine refresh), its `started_at` is left
/// untouched — the caller's `my_started_at` argument is only used to
/// seed a brand-new row or to overwrite a row that belongs to a
/// *different* pid (claiming a stale/dead/evicted slot). Callers
/// should therefore pass the same `my_started_at` (captured once at
/// process startup) on every call.
///
/// # Errors
///
/// Returns an error if the DB write fails.
pub fn heartbeat(
    conn: &Connection,
    prefix: &str,
    pid: i64,
    my_started_at: DateTime<Utc>,
    now: DateTime<Utc>,
    cwd: &str,
    git_remote: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO watchers (prefix, pid, started_at, last_seen, cwd, git_remote)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(prefix) DO UPDATE SET
             pid = excluded.pid,
             started_at = CASE WHEN watchers.pid = excluded.pid
                                THEN watchers.started_at
                                ELSE excluded.started_at
                           END,
             last_seen = excluded.last_seen,
             cwd = excluded.cwd,
             git_remote = excluded.git_remote",
        params![
            prefix,
            pid,
            my_started_at.to_rfc3339(),
            now.to_rfc3339(),
            cwd,
            git_remote,
        ],
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
        .prepare_cached("SELECT 1 FROM watchers WHERE prefix = ?1 AND last_seen >= ?2 LIMIT 1")?
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
    let mut stmt =
        conn.prepare("SELECT DISTINCT prefix FROM watchers WHERE last_seen >= ?1 ORDER BY prefix")?;
    let rows = stmt
        .query_map([cutoff.to_rfc3339()], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Whether any *other* live watcher for the same prefix legitimately
/// supersedes ours.
///
/// Used by `bd watch` to implement newest-wins-per-prefix: when an
/// older watcher sees a newer one, it exits to avoid duplicate
/// notifications. A candidate supersedes us only when it is BOTH:
///   * fresh — `last_seen >= now - ttl_seconds`, so a crashed /
///     `kill -9`'d duplicate that never unregistered can't evict a
///     live watcher; and
///   * legitimately newer — `started_at` strictly greater than ours
///     but NOT dated in the future relative to `now` (a clock-skewed
///     row that registered a future timestamp must not win forever).
///
/// Ties on `started_at` are broken by the caller on `pid`.
///
/// Because `watchers` now keys on prefix alone, at most one row can
/// exist for `prefix` at any instant; this returns that row (if it
/// belongs to someone else and out-ranks us) rather than picking a
/// winner out of several coexisting rows. Callers MUST run this check
/// (and act on it) *before* calling [`heartbeat`] for the same tick —
/// heartbeating first would silently claim/overwrite the very row this
/// query needed to see.
///
/// # Errors
///
/// Returns an error if the DB query fails.
pub fn is_superseded(
    conn: &Connection,
    prefix: &str,
    my_pid: i64,
    my_started_at: DateTime<Utc>,
    now: DateTime<Utc>,
    ttl_seconds: i64,
) -> Result<bool> {
    Ok(newest_other_watcher(conn, prefix, my_pid, my_started_at, now, ttl_seconds)?.is_some())
}

/// Find the newest other *live* watcher that legitimately supersedes
/// ours for `prefix`.
///
/// Returns None if this is the only / newest watcher. Used to render
/// BD_SUPERSEDED messages with concrete (pid, started_at) of the
/// winner. Freshness and future-timestamp gating mirror [`is_superseded`]: a
/// stale row (dead process) or a future-dated row (clock skew) is
/// never treated as a winner. A candidate whose `started_at` exactly
/// equals ours only wins if its `pid` is greater, giving a stable,
/// symmetric tie-break so two watchers that booted in the same tick
/// don't both decide to exit.
///
/// # Errors
///
/// Returns an error if the DB query fails.
pub fn newest_other_watcher(
    conn: &Connection,
    prefix: &str,
    my_pid: i64,
    my_started_at: DateTime<Utc>,
    now: DateTime<Utc>,
    ttl_seconds: i64,
) -> Result<Option<WatcherRow>> {
    let cutoff = now - chrono::Duration::seconds(ttl_seconds);
    let mut stmt = conn.prepare(
        "SELECT prefix, pid, started_at, last_seen, cwd, git_remote FROM watchers
         WHERE prefix = ?1
           AND pid <> ?2
           AND last_seen >= ?3
           AND started_at <= ?4
           AND ( started_at > ?5
                 OR (started_at = ?5 AND pid > ?6) )
         ORDER BY started_at DESC, pid DESC LIMIT 1",
    )?;
    let row = stmt
        .query_row(
            params![
                prefix,
                my_pid,
                cutoff.to_rfc3339(),
                now.to_rfc3339(),
                my_started_at.to_rfc3339(),
                my_pid,
            ],
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
    let mut stmt =
        conn.prepare("SELECT prefix, pid, started_at, last_seen, cwd, git_remote FROM watchers")?;
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
pub fn sweep_stale(conn: &Connection, now: DateTime<Utc>, ttl_seconds: i64) -> Result<usize> {
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
        cwd: row
            .get::<_, Option<String>>("cwd")
            .ok()
            .flatten()
            .unwrap_or_default(),
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
    fn heartbeat_unregister_roundtrip() {
        let conn = open_mem();
        let now = Utc::now();
        heartbeat(&conn, "arc1", 42, now, now, "", "").unwrap();
        assert!(is_active(&conn, "arc1", now, 60).unwrap());

        let later = now + chrono::Duration::seconds(10);
        heartbeat(&conn, "arc1", 42, now, later, "", "").unwrap();
        assert_eq!(list_all(&conn).unwrap()[0].last_seen, later);

        unregister(&conn, "arc1", 42).unwrap();
        assert!(!is_active(&conn, "arc1", later, 60).unwrap());
    }

    #[test]
    fn stale_row_not_active() {
        let conn = open_mem();
        let now = Utc::now();
        let started = now - chrono::Duration::seconds(120);
        heartbeat(&conn, "arc1", 1, started, started, "", "").unwrap();
        assert!(!is_active(&conn, "arc1", now, 60).unwrap());
        // But it's still in list_all (we GC via sweep, not on read).
        assert_eq!(list_all(&conn).unwrap().len(), 1);
    }

    #[test]
    fn distinct_prefixes_coexist() {
        let conn = open_mem();
        let now = Utc::now();
        heartbeat(&conn, "arc1", 1, now, now, "", "").unwrap();
        heartbeat(&conn, "app2", 2, now, now, "", "").unwrap();
        assert_eq!(list_all(&conn).unwrap().len(), 2);
        assert!(is_active(&conn, "arc1", now, 60).unwrap());
        assert!(is_active(&conn, "app2", now, 60).unwrap());

        let mut active = active_prefixes(&conn, now, 60).unwrap();
        active.sort();
        assert_eq!(active, vec!["app2".to_string(), "arc1".to_string()]);
    }

    #[test]
    fn active_prefixes_skips_stale() {
        let conn = open_mem();
        let now = Utc::now();
        heartbeat(&conn, "fresh", 1, now, now, "", "").unwrap();
        let stale_start = now - chrono::Duration::seconds(300);
        heartbeat(&conn, "stale", 2, stale_start, stale_start, "", "").unwrap();
        let active = active_prefixes(&conn, now, 60).unwrap();
        assert_eq!(active, vec!["fresh".to_string()]);
    }

    #[test]
    fn sweep_drops_only_stale() {
        let conn = open_mem();
        let now = Utc::now();
        heartbeat(&conn, "fresh", 1, now, now, "", "").unwrap();
        let stale_start = now - chrono::Duration::seconds(300);
        heartbeat(&conn, "stale", 2, stale_start, stale_start, "", "").unwrap();
        let deleted = sweep_stale(&conn, now, 60).unwrap();
        assert_eq!(deleted, 1);
        assert_eq!(list_all(&conn).unwrap().len(), 1);
    }

    #[test]
    fn same_pid_refresh_preserves_started_at() {
        // A routine tick from the SAME process must not disturb
        // started_at even if (by caller bug) a different value were
        // passed — the CASE branches on the existing row's pid, not
        // on the argument, so this also guards against accidental
        // started_at drift on every tick.
        let conn = open_mem();
        let t1 = Utc::now() - chrono::Duration::seconds(120);
        heartbeat(&conn, "arc1", 1, t1, t1, "", "").unwrap();
        let t2 = Utc::now();
        // Same pid, deliberately a different (wrong) my_started_at —
        // must be ignored because the row is already ours.
        heartbeat(&conn, "arc1", 1, t2, t2, "", "").unwrap();
        let row = &list_all(&conn).unwrap()[0];
        assert_eq!(
            row.started_at, t1,
            "own row's started_at must survive a refresh"
        );
        assert_eq!(row.last_seen, t2);
    }

    // ---- Incident regression: resurrection after row loss ----------

    #[test]
    fn heartbeat_after_delete_resurrects_row() {
        // The core incident regression: sweep_stale (or any other
        // deletion) removes a live watcher's row; the very next
        // heartbeat must bring it back rather than being a silent
        // UPDATE no-op.
        let conn = open_mem();
        let started = Utc::now() - chrono::Duration::seconds(10);
        heartbeat(&conn, "arc1", 100, started, started, "cwd", "host/o/r").unwrap();
        assert_eq!(list_all(&conn).unwrap().len(), 1);

        // Simulate the row being deleted out from under the live
        // watcher (e.g. a racing sweep_stale on another process).
        conn.execute("DELETE FROM watchers WHERE prefix = 'arc1'", [])
            .unwrap();
        assert!(list_all(&conn).unwrap().is_empty());

        let now = Utc::now();
        heartbeat(&conn, "arc1", 100, started, now, "cwd", "host/o/r").unwrap();
        let rows = list_all(&conn).unwrap();
        assert_eq!(rows.len(), 1, "row must be resurrected within one tick");
        assert_eq!(rows[0].pid, 100);
        assert_eq!(rows[0].started_at, started);
        assert_eq!(rows[0].last_seen, now);
    }

    #[test]
    fn sweep_then_heartbeat_resurrects() {
        let conn = open_mem();
        let started = Utc::now() - chrono::Duration::seconds(500);
        heartbeat(&conn, "arc1", 7, started, started, "", "").unwrap();

        let now = Utc::now();
        let deleted = sweep_stale(&conn, now, 60).unwrap();
        assert_eq!(deleted, 1);
        assert!(list_all(&conn).unwrap().is_empty());

        heartbeat(&conn, "arc1", 7, started, now, "", "").unwrap();
        let rows = list_all(&conn).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].pid, 7);
        assert_eq!(rows[0].last_seen, now);
    }

    #[test]
    fn upsert_claims_row_from_old_pid() {
        // Agent harness restarted `bd watch` with a new pid but the
        // same prefix: the new process's heartbeat claims the
        // existing row (new pid, new started_at) rather than
        // colliding on a composite key.
        let conn = open_mem();
        let old_started = Utc::now() - chrono::Duration::seconds(300);
        heartbeat(&conn, "arc1", 111, old_started, old_started, "old/cwd", "").unwrap();

        let new_started = Utc::now();
        heartbeat(
            &conn,
            "arc1",
            222,
            new_started,
            new_started,
            "new/cwd",
            "host/o/r",
        )
        .unwrap();

        let rows = list_all(&conn).unwrap();
        assert_eq!(rows.len(), 1, "still exactly one row for the prefix");
        assert_eq!(rows[0].pid, 222);
        assert_eq!(rows[0].started_at, new_started);
        assert_eq!(rows[0].cwd, "new/cwd");
        assert_eq!(rows[0].git_remote, "host/o/r");
    }

    // ---- Supersede semantics under single-row-per-prefix -----------

    #[test]
    fn live_newer_watcher_supersedes() {
        // A genuinely newer, fresh watcher should supersede the older one.
        // Simulated by heartbeating as the "other" pid FIRST (my own
        // pid never having written a row yet) — mirrors the real tick
        // order: check before you (over)write.
        let conn = open_mem();
        let now = Utc::now();
        let mine = now - chrono::Duration::seconds(30);
        heartbeat(&conn, "arc1", 2, now, now, "", "").unwrap();
        assert!(is_superseded(&conn, "arc1", 1, mine, now, 60).unwrap());
        let winner = newest_other_watcher(&conn, "arc1", 1, mine, now, 60)
            .unwrap()
            .unwrap();
        assert_eq!(winner.pid, 2);
    }

    #[test]
    fn stale_dead_watcher_does_not_supersede() {
        // Regression: a crashed / kill -9'd duplicate leaves a row with a
        // newer started_at but a stale last_seen. It must NOT evict the
        // live watcher — that is the "one agent never gets messages" bug.
        let conn = open_mem();
        let now = Utc::now();
        let mine = now - chrono::Duration::seconds(30);
        // Dead duplicate: started_at newer than mine, but last_seen is
        // 5 minutes stale (well past the 60s TTL).
        let dead_started = now - chrono::Duration::seconds(10);
        heartbeat(
            &conn,
            "arc1",
            2,
            dead_started,
            now - chrono::Duration::seconds(300),
            "",
            "",
        )
        .unwrap();
        assert!(!is_superseded(&conn, "arc1", 1, mine, now, 60).unwrap());
        assert!(
            newest_other_watcher(&conn, "arc1", 1, mine, now, 60)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn future_dated_watcher_does_not_supersede() {
        // Regression: clock skew can register a watcher with a started_at
        // in the future, which would out-rank every real watcher forever.
        // A future-dated row (relative to `now`) must not win even when
        // its heartbeat is fresh.
        let conn = open_mem();
        let now = Utc::now();
        let mine = now - chrono::Duration::seconds(30);
        // Skewed watcher: started_at 10 minutes in the FUTURE, fresh heartbeat.
        let future = now + chrono::Duration::seconds(600);
        heartbeat(&conn, "arc1", 2, future, now, "", "").unwrap();
        assert!(!is_superseded(&conn, "arc1", 1, mine, now, 60).unwrap());
        assert!(
            newest_other_watcher(&conn, "arc1", 1, mine, now, 60)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn equal_started_at_breaks_tie_on_pid() {
        // Two watchers that booted in the same instant must not both
        // decide to exit. The higher pid wins deterministically; the
        // lower pid sees itself superseded, the higher pid does not.
        // Since only one row can exist per prefix, we compare each
        // side's view against the other's row directly (pre-write).
        let conn = open_mem();
        let now = Utc::now();

        // From pid 5's perspective: pid 9's row already exists.
        heartbeat(&conn, "arc1", 9, now, now, "", "").unwrap();
        assert!(is_superseded(&conn, "arc1", 5, now, now, 60).unwrap());
        assert_eq!(
            newest_other_watcher(&conn, "arc1", 5, now, now, 60)
                .unwrap()
                .unwrap()
                .pid,
            9
        );

        // From pid 9's perspective: pid 5's row exists instead.
        let conn2 = open_mem();
        heartbeat(&conn2, "arc1", 5, now, now, "", "").unwrap();
        assert!(!is_superseded(&conn2, "arc1", 9, now, now, 60).unwrap());
        assert!(
            newest_other_watcher(&conn2, "arc1", 9, now, now, 60)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn only_watcher_is_not_superseded() {
        let conn = open_mem();
        let now = Utc::now();
        heartbeat(&conn, "arc1", 1, now, now, "", "").unwrap();
        assert!(!is_superseded(&conn, "arc1", 1, now, now, 60).unwrap());
        assert!(
            newest_other_watcher(&conn, "arc1", 1, now, now, 60)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn no_row_at_all_is_not_superseded() {
        // A brand-new watcher on a prefix nobody has ever watched:
        // the check must not error or spuriously supersede.
        let conn = open_mem();
        let now = Utc::now();
        assert!(!is_superseded(&conn, "arc1", 1, now, now, 60).unwrap());
    }

    #[test]
    fn two_racing_watchers_converge_to_one_survivor() {
        // Simulates the actual tick sequence used by `bd watch`:
        // check-newest-other, and only heartbeat if not superseded.
        // Two watchers (different pids, different started_at) tick
        // repeatedly; within a handful of ticks exactly one survives
        // and the other never resumes writing once it observes it is
        // superseded.
        let conn = open_mem();
        let base = Utc::now() - chrono::Duration::seconds(60);
        let a_pid = 1;
        let a_started = base;
        let b_pid = 2;
        let b_started = base + chrono::Duration::seconds(1); // B is newer -> should win

        let ttl = 60;
        let mut a_alive = true;
        let mut b_alive = true;
        let mut b_ticks = 0u32;

        for i in 0..10 {
            let now = base + chrono::Duration::seconds(2 + i);

            if a_alive {
                if newest_other_watcher(&conn, "arc1", a_pid, a_started, now, ttl)
                    .unwrap()
                    .is_some()
                {
                    a_alive = false; // superseded -> exits, never heartbeats again
                } else {
                    heartbeat(&conn, "arc1", a_pid, a_started, now, "", "").unwrap();
                }
            }
            if b_alive {
                if newest_other_watcher(&conn, "arc1", b_pid, b_started, now, ttl)
                    .unwrap()
                    .is_some()
                {
                    b_alive = false;
                } else {
                    heartbeat(&conn, "arc1", b_pid, b_started, now, "", "").unwrap();
                    b_ticks += 1;
                }
            }
        }

        // B (the legitimately newer watcher) must never see itself
        // superseded by the older A.
        assert!(b_alive, "the newer watcher must never exit");
        assert!(b_ticks > 0);
        // A must have converged to "superseded" within the simulated
        // ticks -- never both exiting, never both surviving.
        assert!(!a_alive, "the older watcher must eventually exit");

        let rows = list_all(&conn).unwrap();
        assert_eq!(rows.len(), 1, "exactly one surviving row for the prefix");
        assert_eq!(rows[0].pid, b_pid);
        assert_eq!(rows[0].started_at, b_started);
    }

    // ---- Migration: old composite-PK DBs upgrade cleanly -----------

    #[test]
    fn migration_collapses_duplicate_prefix_rows_to_freshest() {
        use crate::storage::schema::apply_schema;

        let conn = Connection::open_in_memory().unwrap();
        // Build the OLD composite-PK shape by hand (what a pre-upgrade
        // `bd` would have created), with a couple of other tables
        // thrown in to prove the rebuild doesn't touch unrelated data.
        conn.execute_batch(
            "CREATE TABLE watchers (
                prefix TEXT NOT NULL,
                pid INTEGER NOT NULL,
                started_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                last_seen DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                cwd TEXT NOT NULL DEFAULT '',
                git_remote TEXT NOT NULL DEFAULT '',
                PRIMARY KEY (prefix, pid)
             );
             CREATE TABLE config (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )
        .unwrap();

        let now = Utc::now();
        let old = now - chrono::Duration::seconds(120);
        let mid = now - chrono::Duration::seconds(60);
        conn.execute(
            "INSERT INTO watchers (prefix, pid, started_at, last_seen, cwd, git_remote)
             VALUES ('arc1', 1, ?1, ?1, 'a', 'ra')",
            params![old.to_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO watchers (prefix, pid, started_at, last_seen, cwd, git_remote)
             VALUES ('arc1', 2, ?1, ?1, 'b', 'rb')",
            params![mid.to_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO watchers (prefix, pid, started_at, last_seen, cwd, git_remote)
             VALUES ('arc1', 3, ?1, ?1, 'c', 'rc')",
            params![now.to_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO watchers (prefix, pid, started_at, last_seen, cwd, git_remote)
             VALUES ('app2', 9, ?1, ?1, 'd', 'rd')",
            params![now.to_rfc3339()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO config (key, value) VALUES ('unrelated', 'kept')",
            [],
        )
        .unwrap();

        apply_schema(&conn).unwrap();

        let rows = list_all(&conn).unwrap();
        assert_eq!(rows.len(), 2, "one row survives per prefix");
        let arc1 = rows.iter().find(|r| r.prefix == "arc1").unwrap();
        assert_eq!(arc1.pid, 3, "freshest (last_seen) row wins");
        assert_eq!(arc1.cwd, "c");
        let app2 = rows.iter().find(|r| r.prefix == "app2").unwrap();
        assert_eq!(app2.pid, 9);

        // Unrelated data untouched.
        let kept: String = conn
            .query_row(
                "SELECT value FROM config WHERE key = 'unrelated'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, "kept");

        // New shape enforces uniqueness per prefix now.
        let err = conn.execute(
            "INSERT INTO watchers (prefix, pid, started_at, last_seen, cwd, git_remote)
             VALUES ('arc1', 4, ?1, ?1, 'x', 'rx')",
            params![now.to_rfc3339()],
        );
        assert!(
            err.is_err(),
            "prefix alone must now be the primary key (conflicting insert should fail)"
        );

        // Idempotent: re-applying schema on the already-migrated DB is a no-op.
        apply_schema(&conn).unwrap();
        assert_eq!(list_all(&conn).unwrap().len(), 2);
    }

    #[test]
    fn migration_noop_on_fresh_schema() {
        let conn = open_mem();
        let now = Utc::now();
        heartbeat(&conn, "arc1", 1, now, now, "", "").unwrap();
        apply_schema(&conn).unwrap();
        assert_eq!(list_all(&conn).unwrap().len(), 1);
    }
}
