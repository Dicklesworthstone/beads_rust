//! Event storage operations for `beads_rust`.
//!
//! This module implements the audit event system with:
//! - Event insertion (atomic with mutations)
//! - Event retrieval (newest first, DESC ordering)
//! - Schema definitions for the events table
//!
//! Events are local DB only - never exported to JSONL.

use crate::franken_sync::{Connection, Row};
use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use fsqlite_types::SqliteValue;

use crate::error::{BeadsError, Result};
use crate::model::{Event, EventType};

/// SQL schema for the events table.
///
/// This schema matches the classic bd `events` table structure.
pub const EVENTS_TABLE_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    issue_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    actor TEXT NOT NULL DEFAULT '',
    old_value TEXT,
    new_value TEXT,
    comment TEXT,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    -- Tier 1 attribution audit columns (issue #312, Layer 3 capture-only).
    agent_name TEXT,
    harness TEXT,
    model TEXT,
    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_events_issue ON events(issue_id);
CREATE INDEX IF NOT EXISTS idx_events_created_at ON events(created_at);
CREATE INDEX IF NOT EXISTS idx_events_event_type ON events(event_type);
CREATE INDEX IF NOT EXISTS idx_events_actor ON events(actor);
";

/// Insert an event within a transaction.
///
/// This function should be called within the same transaction (BEGIN/COMMIT)
/// as the mutation that triggered the event. The caller is responsible for
/// managing the transaction boundaries on the connection.
///
/// # Arguments
///
/// * `conn` - Database connection (with an active transaction)
/// * `issue_id` - ID of the issue the event pertains to
/// * `event_type` - Type of event being recorded
/// * `actor` - Username or identifier of the person/agent making the change
/// * `old_value` - Previous value (for changes)
/// * `new_value` - New value (for changes)
/// * `comment` - Optional comment text (for commented events)
///
/// # Errors
///
/// Returns an error if the database insert fails.
pub fn insert_event(
    conn: &Connection,
    issue_id: &str,
    event_type: &EventType,
    actor: &str,
    old_value: Option<&str>,
    new_value: Option<&str>,
    comment: Option<&str>,
) -> Result<i64> {
    let now = Utc::now();
    conn.execute_with_params(
        r"
        INSERT INTO events (issue_id, event_type, actor, old_value, new_value, comment, created_at)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
        ",
        &[
            SqliteValue::from(issue_id),
            SqliteValue::from(event_type.as_str()),
            SqliteValue::from(actor),
            old_value.map_or(SqliteValue::Null, SqliteValue::from),
            new_value.map_or(SqliteValue::Null, SqliteValue::from),
            comment.map_or(SqliteValue::Null, SqliteValue::from),
            SqliteValue::from(now.to_rfc3339()),
        ],
    )?;

    let row = conn.query_row("SELECT last_insert_rowid()")?;
    let id = row.get(0).and_then(SqliteValue::as_integer).unwrap_or(0);
    Ok(id)
}

/// Insert a "created" event for a new issue.
///
/// # Errors
///
/// Returns an error if the database insert fails.
pub fn insert_created_event(conn: &Connection, issue_id: &str, actor: &str) -> Result<i64> {
    insert_event(conn, issue_id, &EventType::Created, actor, None, None, None)
}

/// Insert an "updated" event for a field change.
///
/// # Errors
///
/// Returns an error if the database insert fails.
pub fn insert_updated_event(
    conn: &Connection,
    issue_id: &str,
    actor: &str,
    field: &str,
    old_value: Option<&str>,
    new_value: Option<&str>,
) -> Result<i64> {
    let comment = Some(format!("Updated field: {field}"));
    insert_event(
        conn,
        issue_id,
        &EventType::Updated,
        actor,
        old_value,
        new_value,
        comment.as_deref(),
    )
}

/// Insert a `status_changed` event.
///
/// # Errors
///
/// Returns an error if the database insert fails.
pub fn insert_status_changed_event(
    conn: &Connection,
    issue_id: &str,
    actor: &str,
    old_status: &str,
    new_status: &str,
) -> Result<i64> {
    insert_event(
        conn,
        issue_id,
        &EventType::StatusChanged,
        actor,
        Some(old_status),
        Some(new_status),
        None,
    )
}

/// Insert a "closed" event.
///
/// # Errors
///
/// Returns an error if the database insert fails.
pub fn insert_closed_event(
    conn: &Connection,
    issue_id: &str,
    actor: &str,
    close_reason: Option<&str>,
) -> Result<i64> {
    insert_event(
        conn,
        issue_id,
        &EventType::Closed,
        actor,
        None,
        None,
        close_reason,
    )
}

/// Insert a "reopened" event.
///
/// # Errors
///
/// Returns an error if the database insert fails.
pub fn insert_reopened_event(
    conn: &Connection,
    issue_id: &str,
    actor: &str,
    reason: Option<&str>,
) -> Result<i64> {
    insert_event(
        conn,
        issue_id,
        &EventType::Reopened,
        actor,
        None,
        None,
        reason,
    )
}

/// Insert a "commented" event.
///
/// # Errors
///
/// Returns an error if the database insert fails.
pub fn insert_commented_event(
    conn: &Connection,
    issue_id: &str,
    actor: &str,
    comment_text: &str,
) -> Result<i64> {
    insert_event(
        conn,
        issue_id,
        &EventType::Commented,
        actor,
        None,
        None,
        Some(comment_text),
    )
}

/// Insert a `dependency_added` event.
///
/// # Errors
///
/// Returns an error if the database insert fails.
pub fn insert_dependency_added_event(
    conn: &Connection,
    issue_id: &str,
    actor: &str,
    dep_type: &str,
    depends_on_id: &str,
) -> Result<i64> {
    let comment = format!("Added dependency on {depends_on_id} ({dep_type})");
    insert_event(
        conn,
        issue_id,
        &EventType::DependencyAdded,
        actor,
        None,
        Some(depends_on_id),
        Some(&comment),
    )
}

/// Insert a `dependency_removed` event.
///
/// # Errors
///
/// Returns an error if the database insert fails.
pub fn insert_dependency_removed_event(
    conn: &Connection,
    issue_id: &str,
    actor: &str,
    depends_on_id: &str,
) -> Result<i64> {
    let comment = format!("Removed dependency on {depends_on_id}");
    insert_event(
        conn,
        issue_id,
        &EventType::DependencyRemoved,
        actor,
        Some(depends_on_id),
        None,
        Some(&comment),
    )
}

/// Insert a `label_added` event.
///
/// # Errors
///
/// Returns an error if the database insert fails.
pub fn insert_label_added_event(
    conn: &Connection,
    issue_id: &str,
    actor: &str,
    label: &str,
) -> Result<i64> {
    insert_event(
        conn,
        issue_id,
        &EventType::LabelAdded,
        actor,
        None,
        Some(label),
        None,
    )
}

/// Insert a `label_removed` event.
///
/// # Errors
///
/// Returns an error if the database insert fails.
pub fn insert_label_removed_event(
    conn: &Connection,
    issue_id: &str,
    actor: &str,
    label: &str,
) -> Result<i64> {
    insert_event(
        conn,
        issue_id,
        &EventType::LabelRemoved,
        actor,
        Some(label),
        None,
        None,
    )
}

/// Insert a "deleted" (tombstone) event.
///
/// # Errors
///
/// Returns an error if the database insert fails.
pub fn insert_deleted_event(
    conn: &Connection,
    issue_id: &str,
    actor: &str,
    delete_reason: Option<&str>,
) -> Result<i64> {
    insert_event(
        conn,
        issue_id,
        &EventType::Deleted,
        actor,
        None,
        None,
        delete_reason,
    )
}

/// Insert a "restored" event (if restore is supported).
///
/// # Errors
///
/// Returns an error if the database insert fails.
pub fn insert_restored_event(
    conn: &Connection,
    issue_id: &str,
    actor: &str,
    reason: Option<&str>,
) -> Result<i64> {
    insert_event(
        conn,
        issue_id,
        &EventType::Restored,
        actor,
        None,
        None,
        reason,
    )
}

/// Get events for an issue, ordered by `created_at` DESC (newest first).
///
/// # Arguments
///
/// * `conn` - Database connection
/// * `issue_id` - ID of the issue to get events for
/// * `limit` - Maximum number of events to return (0 = no limit)
///
/// # Errors
///
/// Returns an error if the database query fails.
pub fn get_events(conn: &Connection, issue_id: &str, limit: usize) -> Result<Vec<Event>> {
    let events = conn.query_with_params(
        r"
            SELECT id, issue_id, event_type, actor, old_value, new_value, comment, created_at,
                   agent_name, harness, model
            FROM events
            WHERE issue_id = ?1
            ",
        &[SqliteValue::from(issue_id)],
    )?;

    let mut result: Vec<Event> = events.iter().map(event_from_row).collect::<Result<_>>()?;
    // fsqlite may not honour ORDER BY DESC or LIMIT in all query plans. Fetch
    // the full issue event stream, then enforce both in Rust so LIMIT cannot
    // discard the wrong rows before sorting.
    result.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
    if limit > 0 && result.len() > limit {
        result.truncate(limit);
    }
    Ok(result)
}

fn event_from_row(row: &Row) -> Result<Event> {
    let id = row
        .get(0)
        .and_then(SqliteValue::as_integer)
        .ok_or_else(|| BeadsError::Config("events row missing id".to_string()))?;
    let issue_id = row
        .get(1)
        .and_then(|v| v.as_text())
        .ok_or_else(|| BeadsError::Config("events row missing issue_id".to_string()))?
        .to_string();
    let event_type_str = row.get(2).and_then(|v| v.as_text()).ok_or_else(|| {
        BeadsError::Config(format!("events row missing event_type for {issue_id}"))
    })?;
    let actor = row
        .get(3)
        .and_then(|v| v.as_text())
        .ok_or_else(|| BeadsError::Config(format!("events row missing actor for {issue_id}")))?;
    let actor = actor.to_string();
    let old_value = row.get(4).and_then(|v| v.as_text()).map(String::from);
    let new_value = row.get(5).and_then(|v| v.as_text()).map(String::from);
    let comment = row.get(6).and_then(|v| v.as_text()).map(String::from);
    let created_at_str = row.get(7).and_then(|v| v.as_text()).ok_or_else(|| {
        BeadsError::Config(format!("events row missing created_at for {issue_id}"))
    })?;
    // Tier 1 attribution (issue #312, Layer 3 capture-only). Absent on events
    // recorded without attribution and on rows from pre-v13 databases; both
    // coerce to None. These are recorded-only and never gated on.
    let agent_name = row.get(8).and_then(|v| v.as_text()).map(String::from);
    let harness = row.get(9).and_then(|v| v.as_text()).map(String::from);
    let model = row.get(10).and_then(|v| v.as_text()).map(String::from);

    // Parse event type
    let event_type = parse_event_type(event_type_str);

    // Parse timestamp (support RFC3339 and SQLite default format)
    let created_at = parse_event_timestamp(created_at_str)?;

    Ok(Event {
        id,
        issue_id,
        event_type,
        actor,
        old_value,
        new_value,
        comment,
        created_at,
        agent_name,
        harness,
        model,
    })
}

fn parse_event_timestamp(value: &str) -> Result<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(value) {
        return Ok(dt.with_timezone(&Utc));
    }

    if let Ok(naive) = NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S") {
        return Ok(Utc.from_utc_datetime(&naive));
    }

    Err(BeadsError::Config(format!(
        "Invalid event timestamp: {value}"
    )))
}

/// Get all events across all issues, ordered by `created_at` DESC.
///
/// Useful for audit trails and debugging.
///
/// # Errors
///
/// Returns an error if the database query fails.
pub fn get_all_events(conn: &Connection, limit: usize) -> Result<Vec<Event>> {
    let rows = conn.query(
        r"
            SELECT id, issue_id, event_type, actor, old_value, new_value, comment, created_at,
                   agent_name, harness, model
            FROM events
            ",
    )?;

    let mut result: Vec<Event> = rows.iter().map(event_from_row).collect::<Result<_>>()?;
    // fsqlite may not honour ORDER BY DESC or LIMIT in all query plans. Fetch
    // the full audit stream, then enforce both in Rust so LIMIT cannot discard
    // the wrong rows before sorting.
    result.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(b.id.cmp(&a.id)));
    if limit > 0 && result.len() > limit {
        result.truncate(limit);
    }
    Ok(result)
}

/// Get event count for an issue.
///
/// # Errors
///
/// Returns an error if the database query fails.
pub fn count_events(conn: &Connection, issue_id: &str) -> Result<i64> {
    let row = conn.query_row_with_params(
        "SELECT COUNT(*) FROM events WHERE issue_id = ?1",
        &[SqliteValue::from(issue_id)],
    )?;
    let count = row.get(0).and_then(SqliteValue::as_integer).unwrap_or(0);
    Ok(count)
}

/// Parse event type string to `EventType` enum.
fn parse_event_type(s: &str) -> EventType {
    let normalized = s.to_lowercase();
    match normalized.as_str() {
        "created" => EventType::Created,
        "updated" => EventType::Updated,
        "status_changed" => EventType::StatusChanged,
        "priority_changed" => EventType::PriorityChanged,
        "assignee_changed" => EventType::AssigneeChanged,
        "commented" => EventType::Commented,
        "closed" => EventType::Closed,
        "reopened" => EventType::Reopened,
        "dependency_added" => EventType::DependencyAdded,
        "dependency_removed" => EventType::DependencyRemoved,
        "label_added" => EventType::LabelAdded,
        "label_removed" => EventType::LabelRemoved,
        "compacted" => EventType::Compacted,
        "deleted" => EventType::Deleted,
        "restored" => EventType::Restored,
        other => EventType::Custom(other.to_string()),
    }
}

/// Initialize the events table in the database.
///
/// # Errors
///
/// Returns an error if table creation fails.
pub fn init_events_table(conn: &Connection) -> Result<()> {
    super::schema::execute_batch(conn, EVENTS_TABLE_SCHEMA)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::franken_sync::Connection;
    use crate::storage::schema::execute_batch;

    fn setup_test_db() -> Connection {
        let conn = Connection::open(":memory:").expect("Failed to create in-memory database");

        // Create minimal issues table for foreign key
        execute_batch(
            &conn,
            r"
            CREATE TABLE issues (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'open'
            );
            ",
        )
        .expect("Failed to create issues table");

        // Create events table
        init_events_table(&conn).expect("Failed to create events table");

        // Insert a test issue
        conn.execute("INSERT INTO issues (id, title) VALUES ('test-001', 'Test Issue')")
            .expect("Failed to insert test issue");

        conn
    }

    #[test]
    fn test_parse_event_type_normalizes_known_variant_case() {
        assert_eq!(parse_event_type("CREATED"), EventType::Created);
        assert_eq!(parse_event_type("Status_Changed"), EventType::StatusChanged);
    }

    #[test]
    fn test_parse_event_type_normalizes_custom_case() {
        assert_eq!(
            parse_event_type("My_Custom_Event"),
            EventType::Custom("my_custom_event".to_string())
        );
    }

    #[test]
    fn test_insert_created_event() {
        let conn = setup_test_db();
        conn.execute("BEGIN").expect("Failed to start tx");

        let id = insert_created_event(&conn, "test-001", "alice").expect("Failed to insert event");
        conn.execute("COMMIT").expect("Failed to commit");

        assert!(id > 0);

        let events = get_events(&conn, "test-001", 0).expect("Failed to get events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::Created);
        assert_eq!(events[0].actor, "alice");
    }

    #[test]
    fn test_insert_status_changed_event() {
        let conn = setup_test_db();
        conn.execute("BEGIN").expect("Failed to start tx");

        insert_status_changed_event(&conn, "test-001", "bob", "open", "in_progress")
            .expect("Failed to insert event");
        conn.execute("COMMIT").expect("Failed to commit");

        let events = get_events(&conn, "test-001", 0).expect("Failed to get events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::StatusChanged);
        assert_eq!(events[0].old_value.as_deref(), Some("open"));
        assert_eq!(events[0].new_value.as_deref(), Some("in_progress"));
    }

    #[test]
    fn test_insert_closed_event() {
        let conn = setup_test_db();
        conn.execute("BEGIN").expect("Failed to start tx");

        insert_closed_event(&conn, "test-001", "carol", Some("Completed the work"))
            .expect("Failed to insert event");
        conn.execute("COMMIT").expect("Failed to commit");

        let events = get_events(&conn, "test-001", 0).expect("Failed to get events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::Closed);
        assert_eq!(events[0].comment.as_deref(), Some("Completed the work"));
    }

    #[test]
    fn test_insert_commented_event() {
        let conn = setup_test_db();
        conn.execute("BEGIN").expect("Failed to start tx");

        insert_commented_event(&conn, "test-001", "dave", "This is a comment")
            .expect("Failed to insert event");
        conn.execute("COMMIT").expect("Failed to commit");

        let events = get_events(&conn, "test-001", 0).expect("Failed to get events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::Commented);
        assert_eq!(events[0].comment.as_deref(), Some("This is a comment"));
    }

    #[test]
    fn test_insert_dependency_added_event() {
        let conn = setup_test_db();

        // Add second issue for dependency
        conn.execute("INSERT INTO issues (id, title) VALUES ('test-002', 'Blocking Issue')")
            .expect("Failed to insert second issue");

        conn.execute("BEGIN").expect("Failed to start tx");
        insert_dependency_added_event(&conn, "test-001", "eve", "blocks", "test-002")
            .expect("Failed to insert event");
        conn.execute("COMMIT").expect("Failed to commit");

        let events = get_events(&conn, "test-001", 0).expect("Failed to get events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::DependencyAdded);
        assert_eq!(events[0].new_value.as_deref(), Some("test-002"));
        assert!(events[0].comment.as_ref().unwrap().contains("blocks"));
    }

    #[test]
    fn test_insert_label_events() {
        let conn = setup_test_db();
        conn.execute("BEGIN").expect("Failed to start tx");

        insert_label_added_event(&conn, "test-001", "frank", "urgent")
            .expect("Failed to insert label added event");
        insert_label_removed_event(&conn, "test-001", "frank", "urgent")
            .expect("Failed to insert label removed event");
        conn.execute("COMMIT").expect("Failed to commit");

        let events = get_events(&conn, "test-001", 0).expect("Failed to get events");
        assert_eq!(events.len(), 2);

        // Events are DESC order, so removed is first
        assert_eq!(events[0].event_type, EventType::LabelRemoved);
        assert_eq!(events[0].old_value.as_deref(), Some("urgent"));

        assert_eq!(events[1].event_type, EventType::LabelAdded);
        assert_eq!(events[1].new_value.as_deref(), Some("urgent"));
    }

    #[test]
    fn test_get_events_ordering() {
        let conn = setup_test_db();

        // Insert multiple events
        for i in 0..5 {
            conn.execute("BEGIN").expect("Failed to start tx");
            insert_commented_event(&conn, "test-001", "user", &format!("Comment {i}"))
                .expect("Failed to insert event");
            conn.execute("COMMIT").expect("Failed to commit");
        }

        let events = get_events(&conn, "test-001", 0).expect("Failed to get events");
        assert_eq!(events.len(), 5);

        // Verify DESC ordering (newest first)
        assert!(events[0].comment.as_ref().unwrap().contains("Comment 4"));
        assert!(events[4].comment.as_ref().unwrap().contains("Comment 0"));
    }

    #[test]
    fn test_get_events_with_limit() {
        let conn = setup_test_db();

        // Insert 10 events
        for i in 0..10 {
            conn.execute("BEGIN").expect("Failed to start tx");
            insert_commented_event(&conn, "test-001", "user", &format!("Comment {i}"))
                .expect("Failed to insert event");
            conn.execute("COMMIT").expect("Failed to commit");
        }

        // Get only 3 events
        let events = get_events(&conn, "test-001", 3).expect("Failed to get events");
        assert_eq!(events.len(), 3);

        // Should be newest 3
        assert!(events[0].comment.as_ref().unwrap().contains("Comment 9"));
        assert!(events[2].comment.as_ref().unwrap().contains("Comment 7"));
    }

    #[test]
    fn test_get_events_limit_is_applied_after_timestamp_sort() {
        let conn = setup_test_db();

        let rows = [
            ("oldest", "2026-04-22T10:00:00Z"),
            ("newest", "2026-04-22T13:00:00Z"),
            ("middle", "2026-04-22T12:00:00Z"),
            ("newer", "2026-04-22T12:30:00Z"),
        ];
        for (comment, created_at) in rows {
            conn.execute_with_params(
                "INSERT INTO events (issue_id, event_type, actor, comment, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                &[
                    SqliteValue::from("test-001"),
                    SqliteValue::from("commented"),
                    SqliteValue::from("user"),
                    SqliteValue::from(comment),
                    SqliteValue::from(created_at),
                ],
            )
            .expect("insert event");
        }

        let events = get_events(&conn, "test-001", 2).expect("events");

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].comment.as_deref(), Some("newest"));
        assert_eq!(events[1].comment.as_deref(), Some("newer"));
    }

    #[test]
    fn test_get_events_normalizes_persisted_event_type_case() {
        let conn = setup_test_db();

        let rows = [
            ("CREATED", "known", "2026-04-22T10:00:00Z"),
            ("My_Custom_Event", "custom", "2026-04-22T11:00:00Z"),
        ];
        for (event_type, comment, created_at) in rows {
            conn.execute_with_params(
                "INSERT INTO events (issue_id, event_type, actor, comment, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                &[
                    SqliteValue::from("test-001"),
                    SqliteValue::from(event_type),
                    SqliteValue::from("legacy"),
                    SqliteValue::from(comment),
                    SqliteValue::from(created_at),
                ],
            )
            .expect("insert event");
        }

        let events = get_events(&conn, "test-001", 0).expect("events");

        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0].event_type,
            EventType::Custom("my_custom_event".to_string())
        );
        assert_eq!(events[1].event_type, EventType::Created);
    }

    #[test]
    fn test_count_events() {
        let conn = setup_test_db();

        // Insert events
        for _ in 0..5 {
            conn.execute("BEGIN").expect("Failed to start tx");
            insert_commented_event(&conn, "test-001", "user", "A comment")
                .expect("Failed to insert event");
            conn.execute("COMMIT").expect("Failed to commit");
        }

        let count = count_events(&conn, "test-001").expect("Failed to count events");
        assert_eq!(count, 5);
    }

    #[test]
    fn test_deleted_and_restored_events() {
        let conn = setup_test_db();
        conn.execute("BEGIN").expect("Failed to start tx");

        insert_deleted_event(&conn, "test-001", "admin", Some("Duplicate issue"))
            .expect("Failed to insert deleted event");
        insert_restored_event(&conn, "test-001", "admin", Some("Not a duplicate"))
            .expect("Failed to insert restored event");
        conn.execute("COMMIT").expect("Failed to commit");

        let events = get_events(&conn, "test-001", 0).expect("Failed to get events");
        assert_eq!(events.len(), 2);

        // Restored is newer (first in DESC order)
        assert_eq!(events[0].event_type, EventType::Restored);
        assert_eq!(events[0].comment.as_deref(), Some("Not a duplicate"));

        assert_eq!(events[1].event_type, EventType::Deleted);
        assert_eq!(events[1].comment.as_deref(), Some("Duplicate issue"));
    }

    #[test]
    fn test_reopened_event() {
        let conn = setup_test_db();
        conn.execute("BEGIN").expect("Failed to start tx");

        insert_reopened_event(&conn, "test-001", "manager", Some("Need more work"))
            .expect("Failed to insert reopened event");
        conn.execute("COMMIT").expect("Failed to commit");

        let events = get_events(&conn, "test-001", 0).expect("Failed to get events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::Reopened);
        assert_eq!(events[0].comment.as_deref(), Some("Need more work"));
    }

    #[test]
    fn test_get_all_events() {
        let conn = setup_test_db();

        // Add second issue
        conn.execute("INSERT INTO issues (id, title) VALUES ('test-002', 'Second Issue')")
            .expect("Failed to insert second issue");

        // Insert events for both issues
        conn.execute("BEGIN").expect("Failed to start tx");
        insert_created_event(&conn, "test-001", "alice").expect("Failed to insert event");
        insert_created_event(&conn, "test-002", "bob").expect("Failed to insert event");
        conn.execute("COMMIT").expect("Failed to commit");

        let all_events = get_all_events(&conn, 0).expect("Failed to get all events");
        assert_eq!(all_events.len(), 2);
    }

    #[test]
    fn test_multiple_event_types_sequence() {
        let conn = setup_test_db();

        // Simulate a typical issue lifecycle
        conn.execute("BEGIN").expect("Failed to start tx");
        insert_created_event(&conn, "test-001", "alice").expect("Created");
        conn.execute("COMMIT").expect("Commit");

        conn.execute("BEGIN").expect("Failed to start tx");
        insert_status_changed_event(&conn, "test-001", "alice", "open", "in_progress")
            .expect("Status change");
        conn.execute("COMMIT").expect("Commit");

        conn.execute("BEGIN").expect("Failed to start tx");
        insert_commented_event(&conn, "test-001", "bob", "Working on this").expect("Comment");
        conn.execute("COMMIT").expect("Commit");

        conn.execute("BEGIN").expect("Failed to start tx");
        insert_closed_event(&conn, "test-001", "alice", Some("Done")).expect("Closed");
        conn.execute("COMMIT").expect("Commit");

        let events = get_events(&conn, "test-001", 0).expect("Failed to get events");
        assert_eq!(events.len(), 4);

        // Verify order (newest first)
        assert_eq!(events[0].event_type, EventType::Closed);
        assert_eq!(events[1].event_type, EventType::Commented);
        assert_eq!(events[2].event_type, EventType::StatusChanged);
        assert_eq!(events[3].event_type, EventType::Created);
    }

    #[test]
    fn test_event_attribution_round_trips_through_columns() {
        // Issue #312, Layer 3 capture-only: attribution written to the events
        // table round-trips back through the read path unchanged.
        let conn = setup_test_db();
        conn.execute_with_params(
            "INSERT INTO events (issue_id, event_type, actor, created_at, agent_name, harness, model)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            &[
                SqliteValue::from("test-001"),
                SqliteValue::from("status_changed"),
                SqliteValue::from("alice"),
                SqliteValue::from("2026-06-07T10:00:00Z"),
                SqliteValue::from("agent-1"),
                SqliteValue::from("codex-cli"),
                SqliteValue::from("opus-4"),
            ],
        )
        .expect("insert attributed event");

        let events = get_events(&conn, "test-001", 0).expect("get events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].agent_name.as_deref(), Some("agent-1"));
        assert_eq!(events[0].harness.as_deref(), Some("codex-cli"));
        assert_eq!(events[0].model.as_deref(), Some("opus-4"));
    }

    #[test]
    fn test_event_without_attribution_reads_as_none() {
        // Absent attribution (the common case) must coerce to None, never to a
        // blank string or invalid value.
        let conn = setup_test_db();
        conn.execute("BEGIN").expect("begin");
        insert_created_event(&conn, "test-001", "alice").expect("insert event");
        conn.execute("COMMIT").expect("commit");

        let events = get_events(&conn, "test-001", 0).expect("get events");
        assert_eq!(events.len(), 1);
        assert!(events[0].agent_name.is_none());
        assert!(events[0].harness.is_none());
        assert!(events[0].model.is_none());
    }

    #[test]
    fn test_partial_attribution_round_trips() {
        // Only some attribution fields supplied; the others stay None.
        let conn = setup_test_db();
        conn.execute_with_params(
            "INSERT INTO events (issue_id, event_type, actor, created_at, model)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            &[
                SqliteValue::from("test-001"),
                SqliteValue::from("updated"),
                SqliteValue::from("bob"),
                SqliteValue::from("2026-06-07T11:00:00Z"),
                SqliteValue::from("opus-4"),
            ],
        )
        .expect("insert partially-attributed event");

        let events = get_events(&conn, "test-001", 0).expect("get events");
        assert_eq!(events.len(), 1);
        assert!(events[0].agent_name.is_none());
        assert!(events[0].harness.is_none());
        assert_eq!(events[0].model.as_deref(), Some("opus-4"));
    }

    #[test]
    fn test_get_events_errors_on_invalid_timestamp() {
        let conn = setup_test_db();

        conn.execute("BEGIN").expect("Failed to start tx");
        conn.execute_with_params(
            "INSERT INTO events (issue_id, event_type, actor, created_at) VALUES (?1, ?2, ?3, ?4)",
            &[
                SqliteValue::from("test-001"),
                SqliteValue::from("created"),
                SqliteValue::from("alice"),
                SqliteValue::from("definitely-not-a-timestamp"),
            ],
        )
        .expect("insert malformed event");
        conn.execute("COMMIT").expect("commit malformed event");

        let err = get_events(&conn, "test-001", 0).unwrap_err();
        assert!(
            matches!(err, BeadsError::Config(ref msg) if msg.contains("Invalid event timestamp")),
            "unexpected error: {err:?}"
        );
    }
}
