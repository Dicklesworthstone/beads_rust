//! Per-prefix agent presence tracking (working / idle).
//!
//! Populated by the undocumented `bd working` / `bd idle` commands wired
//! into Claude Code's lifecycle hooks. Surfaces in `bd dash` as a badge
//! next to each prefix header.

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::Result;

/// One agent's presence snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresenceRow {
    pub prefix: String,
    pub state: PresenceState,
    pub last_changed: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceState {
    Working,
    Idle,
}

impl PresenceState {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Working => "working",
            Self::Idle => "idle",
        }
    }
}

/// Upsert presence for `prefix` to `state`, setting last_changed to `now`.
///
/// # Errors
///
/// Returns an error if the DB write fails.
pub fn set_presence(
    conn: &Connection,
    prefix: &str,
    state: PresenceState,
    now: DateTime<Utc>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO agent_presence (prefix, state, last_changed)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(prefix) DO UPDATE SET state = excluded.state,
                                           last_changed = excluded.last_changed",
        params![prefix, state.as_str(), now.to_rfc3339()],
    )?;
    Ok(())
}

/// Fetch presence for a single prefix, if any.
///
/// # Errors
///
/// Returns an error if the DB query fails.
pub fn get_presence(conn: &Connection, prefix: &str) -> Result<Option<PresenceRow>> {
    conn.prepare_cached(
        "SELECT prefix, state, last_changed FROM agent_presence WHERE prefix = ?1",
    )?
    .query_row([prefix], row_to_presence)
    .optional()
    .map_err(Into::into)
}

/// Fetch all presence rows.
///
/// # Errors
///
/// Returns an error if the DB query fails.
pub fn all_presence(conn: &Connection) -> Result<Vec<PresenceRow>> {
    let mut stmt = conn.prepare("SELECT prefix, state, last_changed FROM agent_presence")?;
    let rows = stmt
        .query_map([], row_to_presence)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

fn row_to_presence(row: &rusqlite::Row<'_>) -> rusqlite::Result<PresenceRow> {
    let state_str: String = row.get("state")?;
    let state = match state_str.as_str() {
        "working" => PresenceState::Working,
        _ => PresenceState::Idle,
    };
    let ts_str: String = row.get("last_changed")?;
    let last_changed = parse_db_timestamp(&ts_str);
    Ok(PresenceRow {
        prefix: row.get("prefix")?,
        state,
        last_changed,
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
    fn set_then_get_roundtrip() {
        let conn = open_mem();
        let now = Utc::now();
        set_presence(&conn, "arc1", PresenceState::Working, now).unwrap();
        let row = get_presence(&conn, "arc1").unwrap().unwrap();
        assert_eq!(row.prefix, "arc1");
        assert_eq!(row.state, PresenceState::Working);
    }

    #[test]
    fn set_is_upsert() {
        let conn = open_mem();
        let t1 = Utc::now();
        set_presence(&conn, "arc1", PresenceState::Working, t1).unwrap();
        let t2 = t1 + chrono::Duration::minutes(5);
        set_presence(&conn, "arc1", PresenceState::Idle, t2).unwrap();
        let row = get_presence(&conn, "arc1").unwrap().unwrap();
        assert_eq!(row.state, PresenceState::Idle);
        // No second row appeared.
        assert_eq!(all_presence(&conn).unwrap().len(), 1);
    }

    #[test]
    fn missing_prefix_returns_none() {
        let conn = open_mem();
        assert!(get_presence(&conn, "ghost").unwrap().is_none());
    }
}
