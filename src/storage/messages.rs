//! Storage operations for the ephemeral messaging system.
//!
//! Messages live in their own table, distinct from issues. They are
//! conversational (not work items), auto-expire after a TTL once read,
//! and never round-trip through JSONL.

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use crate::error::Result;
use crate::model::Message;
use crate::util::id::compute_id_hash;

fn parse_db_timestamp(value: &str) -> DateTime<Utc> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return dt.with_timezone(&Utc);
    }
    if let Ok(naive) = NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S") {
        return Utc.from_utc_datetime(&naive);
    }
    Utc::now()
}

/// Filter for listing messages.
#[derive(Debug, Default, Clone)]
pub struct MessageFilter {
    pub to_prefix: Option<String>,
    pub from_prefix: Option<String>,
    pub only_unread: bool,
    pub limit: Option<usize>,
}

/// Generate a short, opaque message ID. Collisions are checked by the caller.
#[must_use]
pub fn generate_message_id(from: &str, to: &str, body: &str, sent_at: DateTime<Utc>, nonce: u32) -> String {
    let seed = format!(
        "msg|{from}|{to}|{}|{}|{nonce}",
        body,
        sent_at.timestamp_nanos_opt().unwrap_or(0)
    );
    let hash = compute_id_hash(&seed, 5);
    format!("msg-{hash}")
}

/// Insert a new message.
///
/// # Errors
///
/// Returns an error if the DB insert fails.
pub fn insert_message(conn: &Connection, msg: &Message) -> Result<()> {
    conn.execute(
        "INSERT INTO messages (id, from_prefix, to_prefix, body, sent_at, read_at, in_reply_to)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            msg.id,
            msg.from_prefix,
            msg.to_prefix,
            msg.body,
            msg.sent_at.to_rfc3339(),
            msg.read_at.map(|t| t.to_rfc3339()),
            msg.in_reply_to,
        ],
    )?;
    Ok(())
}

/// Check whether a message ID is already in use.
///
/// # Errors
///
/// Returns an error if the DB query fails.
pub fn message_id_exists(conn: &Connection, id: &str) -> Result<bool> {
    let exists: bool = conn
        .prepare_cached("SELECT 1 FROM messages WHERE id = ?1")?
        .exists([id])?;
    Ok(exists)
}

/// Fetch a single message by ID.
///
/// # Errors
///
/// Returns an error if the DB query fails.
pub fn get_message(conn: &Connection, id: &str) -> Result<Option<Message>> {
    conn.prepare_cached(
        "SELECT id, from_prefix, to_prefix, body, sent_at, read_at, in_reply_to
         FROM messages WHERE id = ?1",
    )?
    .query_row([id], row_to_message)
    .optional()
    .map_err(Into::into)
}

/// List messages matching the filter, ordered newest first.
///
/// # Errors
///
/// Returns an error if the DB query fails.
pub fn list_messages(conn: &Connection, filter: &MessageFilter) -> Result<Vec<Message>> {
    let mut sql = String::from(
        "SELECT id, from_prefix, to_prefix, body, sent_at, read_at, in_reply_to
         FROM messages WHERE 1=1",
    );
    let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

    if let Some(to) = &filter.to_prefix {
        sql.push_str(" AND to_prefix = ?");
        params_vec.push(Box::new(to.clone()));
    }
    if let Some(from) = &filter.from_prefix {
        sql.push_str(" AND from_prefix = ?");
        params_vec.push(Box::new(from.clone()));
    }
    if filter.only_unread {
        sql.push_str(" AND read_at IS NULL");
    }
    sql.push_str(" ORDER BY sent_at DESC");
    if let Some(limit) = filter.limit {
        sql.push_str(&format!(" LIMIT {limit}"));
    }

    let mut stmt = conn.prepare(&sql)?;
    let params_ref: Vec<&dyn rusqlite::ToSql> = params_vec.iter().map(|b| b.as_ref()).collect();
    let rows = stmt
        .query_map(params_ref.as_slice(), row_to_message)?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Mark a message as read. Returns whether the message exists and was previously unread.
///
/// # Errors
///
/// Returns an error if the DB update fails.
pub fn mark_message_read(conn: &Connection, id: &str, now: DateTime<Utc>) -> Result<bool> {
    let updated = conn.execute(
        "UPDATE messages SET read_at = ?1 WHERE id = ?2 AND read_at IS NULL",
        params![now.to_rfc3339(), id],
    )?;
    Ok(updated > 0)
}

/// Sweep messages that have been read longer than `ttl_days` ago.
/// Returns the number of rows deleted.
///
/// # Errors
///
/// Returns an error if the DB delete fails.
pub fn sweep_read_messages(conn: &Connection, ttl_days: i64, now: DateTime<Utc>) -> Result<usize> {
    let cutoff = now - chrono::Duration::days(ttl_days);
    let deleted = conn.execute(
        "DELETE FROM messages WHERE read_at IS NOT NULL AND read_at < ?1",
        params![cutoff.to_rfc3339()],
    )?;
    Ok(deleted)
}

fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    let sent_at_str: String = row.get("sent_at")?;
    let read_at_str: Option<String> = row.get("read_at")?;
    Ok(Message {
        id: row.get("id")?,
        from_prefix: row.get("from_prefix")?,
        to_prefix: row.get("to_prefix")?,
        body: row.get("body")?,
        sent_at: parse_db_timestamp(&sent_at_str),
        read_at: read_at_str.map(|s| parse_db_timestamp(&s)),
        in_reply_to: row.get("in_reply_to")?,
    })
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

    fn msg(id: &str, from: &str, to: &str, body: &str, reply: Option<&str>) -> Message {
        Message {
            id: id.to_string(),
            from_prefix: from.to_string(),
            to_prefix: to.to_string(),
            body: body.to_string(),
            sent_at: Utc::now(),
            read_at: None,
            in_reply_to: reply.map(String::from),
        }
    }

    #[test]
    fn insert_and_get_roundtrip() {
        let conn = open_mem();
        let m = msg("msg-aaa", "app1", "app2", "hello", None);
        insert_message(&conn, &m).unwrap();
        let got = get_message(&conn, "msg-aaa").unwrap().unwrap();
        assert_eq!(got.from_prefix, "app1");
        assert_eq!(got.to_prefix, "app2");
        assert_eq!(got.body, "hello");
        assert!(got.read_at.is_none());
    }

    #[test]
    fn list_filters_by_recipient_and_unread() {
        let conn = open_mem();
        insert_message(&conn, &msg("msg-a", "app1", "app2", "to app2", None)).unwrap();
        insert_message(&conn, &msg("msg-b", "app1", "app3", "to app3", None)).unwrap();
        insert_message(&conn, &msg("msg-c", "app2", "app2", "to self", None)).unwrap();
        mark_message_read(&conn, "msg-a", Utc::now()).unwrap();

        let to_app2 = list_messages(
            &conn,
            &MessageFilter {
                to_prefix: Some("app2".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(to_app2.len(), 2);

        let unread_to_app2 = list_messages(
            &conn,
            &MessageFilter {
                to_prefix: Some("app2".into()),
                only_unread: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(unread_to_app2.len(), 1);
        assert_eq!(unread_to_app2[0].id, "msg-c");
    }

    #[test]
    fn mark_read_is_idempotent() {
        let conn = open_mem();
        insert_message(&conn, &msg("msg-x", "a", "b", "h", None)).unwrap();
        let first = mark_message_read(&conn, "msg-x", Utc::now()).unwrap();
        let second = mark_message_read(&conn, "msg-x", Utc::now()).unwrap();
        assert!(first);
        assert!(!second);
    }

    #[test]
    fn sweep_drops_only_aged_read_messages() {
        let conn = open_mem();
        let now = Utc::now();
        let mut old = msg("msg-old", "a", "b", "old", None);
        old.read_at = Some(now - chrono::Duration::days(30));
        let mut recent = msg("msg-recent", "a", "b", "recent", None);
        recent.read_at = Some(now - chrono::Duration::days(1));
        let unread = msg("msg-unread", "a", "b", "unread", None);

        insert_message(&conn, &old).unwrap();
        insert_message(&conn, &recent).unwrap();
        insert_message(&conn, &unread).unwrap();

        let deleted = sweep_read_messages(&conn, 7, now).unwrap();
        assert_eq!(deleted, 1);
        assert!(get_message(&conn, "msg-old").unwrap().is_none());
        assert!(get_message(&conn, "msg-recent").unwrap().is_some());
        assert!(get_message(&conn, "msg-unread").unwrap().is_some());
    }

    #[test]
    fn reply_chain_preserves_in_reply_to() {
        let conn = open_mem();
        insert_message(&conn, &msg("msg-1", "a", "b", "q", None)).unwrap();
        insert_message(&conn, &msg("msg-2", "b", "a", "a", Some("msg-1"))).unwrap();
        let r = get_message(&conn, "msg-2").unwrap().unwrap();
        assert_eq!(r.in_reply_to.as_deref(), Some("msg-1"));
    }
}
