//! Event storage operations for `beads_rust`.
//!
//! This module implements the audit event system with:
//! - Event insertion (atomic with mutations)
//! - Event retrieval (newest first, DESC ordering)
//! - Schema definitions for the events table
//!
//! Events are local DB only - never exported to JSONL.

use chrono::{DateTime, Utc};
use rusqlite::{Connection, Transaction, params};

use crate::error::Result;
use crate::model::{Event, EventType};

/// SQL schema for the events table.
///
/// This schema matches the classic bd `events` table structure.
pub const EVENTS_TABLE_SCHEMA: &str = r"
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    issue_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    actor TEXT NOT NULL,
    old_value TEXT,
    new_value TEXT,
    comment TEXT,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_events_issue ON events(issue_id);
CREATE INDEX IF NOT EXISTS idx_events_created_at ON events(created_at);
CREATE INDEX IF NOT EXISTS idx_events_event_type ON events(event_type);
CREATE INDEX IF NOT EXISTS idx_events_actor ON events(actor);
";

/// Insert an event within a transaction.
///
/// This function should be called within the same transaction as the
/// mutation that triggered the event. The caller is responsible for
/// managing the transaction.
///
/// # Arguments
///
/// * `tx` - Active database transaction
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
    tx: &Transaction<'_>,
    issue_id: &str,
    event_type: &EventType,
    actor: &str,
    old_value: Option<&str>,
    new_value: Option<&str>,
    comment: Option<&str>,
) -> Result<i64> {
    let now = Utc::now();
    tx.execute(
        r"
        INSERT INTO events (issue_id, event_type, actor, old_value, new_value, comment, created_at)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
        ",
        params![
            issue_id,
            event_type.as_str(),
            actor,
            old_value,
            new_value,
            comment,
            now.to_rfc3339(),
        ],
    )?;

    Ok(tx.last_insert_rowid())
}

/// Insert a "created" event for a new issue.
///
/// # Errors
///
/// Returns an error if the database insert fails.
pub fn insert_created_event(tx: &Transaction<'_>, issue_id: &str, actor: &str) -> Result<i64> {
    insert_event(tx, issue_id, &EventType::Created, actor, None, None, None)
}

/// Insert an "updated" event for a field change.
///
/// # Errors
///
/// Returns an error if the database insert fails.
pub fn insert_updated_event(
    tx: &Transaction<'_>,
    issue_id: &str,
    actor: &str,
    field: &str,
    old_value: Option<&str>,
    new_value: Option<&str>,
) -> Result<i64> {
    let comment = Some(format!("Updated field: {field}"));
    insert_event(
        tx,
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
    tx: &Transaction<'_>,
    issue_id: &str,
    actor: &str,
    old_status: &str,
    new_status: &str,
) -> Result<i64> {
    insert_event(
        tx,
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
    tx: &Transaction<'_>,
    issue_id: &str,
    actor: &str,
    close_reason: Option<&str>,
) -> Result<i64> {
    insert_event(
        tx,
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
    tx: &Transaction<'_>,
    issue_id: &str,
    actor: &str,
    reason: Option<&str>,
) -> Result<i64> {
    insert_event(
        tx,
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
    tx: &Transaction<'_>,
    issue_id: &str,
    actor: &str,
    comment_text: &str,
) -> Result<i64> {
    insert_event(
        tx,
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
    tx: &Transaction<'_>,
    issue_id: &str,
    actor: &str,
    dep_type: &str,
    depends_on_id: &str,
) -> Result<i64> {
    let comment = format!("Added dependency on {depends_on_id} ({dep_type})");
    insert_event(
        tx,
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
    tx: &Transaction<'_>,
    issue_id: &str,
    actor: &str,
    depends_on_id: &str,
) -> Result<i64> {
    let comment = format!("Removed dependency on {depends_on_id}");
    insert_event(
        tx,
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
    tx: &Transaction<'_>,
    issue_id: &str,
    actor: &str,
    label: &str,
) -> Result<i64> {
    insert_event(
        tx,
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
    tx: &Transaction<'_>,
    issue_id: &str,
    actor: &str,
    label: &str,
) -> Result<i64> {
    insert_event(
        tx,
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
    tx: &Transaction<'_>,
    issue_id: &str,
    actor: &str,
    delete_reason: Option<&str>,
) -> Result<i64> {
    insert_event(
        tx,
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
    tx: &Transaction<'_>,
    issue_id: &str,
    actor: &str,
    reason: Option<&str>,
) -> Result<i64> {
    insert_event(
        tx,
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
    let query = if limit > 0 {
        format!(
            r"
            SELECT id, issue_id, event_type, actor, old_value, new_value, comment, created_at
            FROM events
            WHERE issue_id = ?1
            ORDER BY created_at DESC
            LIMIT {limit}
            "
        )
    } else {
        r"
            SELECT id, issue_id, event_type, actor, old_value, new_value, comment, created_at
            FROM events
            WHERE issue_id = ?1
            ORDER BY created_at DESC
            "
        .to_string()
    };

    let mut stmt = conn.prepare(&query)?;
    let events = stmt
        .query_map(params![issue_id], |row| {
            let id: i64 = row.get(0)?;
            let issue_id: String = row.get(1)?;
            let event_type_str: String = row.get(2)?;
            let actor: String = row.get(3)?;
            let old_value: Option<String> = row.get(4)?;
            let new_value: Option<String> = row.get(5)?;
            let comment: Option<String> = row.get(6)?;
            let created_at_str: String = row.get(7)?;

            // Parse event type
            let event_type = parse_event_type(&event_type_str);

            // Parse timestamp
            let created_at = DateTime::parse_from_rfc3339(&created_at_str)
                .map_or_else(|_| Utc::now(), |dt| dt.with_timezone(&Utc));

            Ok(Event {
                id,
                issue_id,
                event_type,
                actor,
                old_value,
                new_value,
                comment,
                created_at,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(events)
}

/// Get all events across all issues, ordered by `created_at` DESC.
///
/// Useful for audit trails and debugging.
///
/// # Errors
///
/// Returns an error if the database query fails.
pub fn get_all_events(conn: &Connection, limit: usize) -> Result<Vec<Event>> {
    let query = if limit > 0 {
        format!(
            r"
            SELECT id, issue_id, event_type, actor, old_value, new_value, comment, created_at
            FROM events
            ORDER BY created_at DESC
            LIMIT {limit}
            "
        )
    } else {
        r"
            SELECT id, issue_id, event_type, actor, old_value, new_value, comment, created_at
            FROM events
            ORDER BY created_at DESC
            "
        .to_string()
    };

    let mut stmt = conn.prepare(&query)?;
    let events = stmt
        .query_map([], |row| {
            let id: i64 = row.get(0)?;
            let issue_id: String = row.get(1)?;
            let event_type_str: String = row.get(2)?;
            let actor: String = row.get(3)?;
            let old_value: Option<String> = row.get(4)?;
            let new_value: Option<String> = row.get(5)?;
            let comment: Option<String> = row.get(6)?;
            let created_at_str: String = row.get(7)?;

            let event_type = parse_event_type(&event_type_str);
            let created_at = DateTime::parse_from_rfc3339(&created_at_str)
                .map_or_else(|_| Utc::now(), |dt| dt.with_timezone(&Utc));

            Ok(Event {
                id,
                issue_id,
                event_type,
                actor,
                old_value,
                new_value,
                comment,
                created_at,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;

    Ok(events)
}

/// Get event count for an issue.
///
/// # Errors
///
/// Returns an error if the database query fails.
pub fn count_events(conn: &Connection, issue_id: &str) -> Result<i64> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM events WHERE issue_id = ?1",
        params![issue_id],
        |row| row.get(0),
    )?;
    Ok(count)
}

/// Parse event type string to `EventType` enum.
fn parse_event_type(s: &str) -> EventType {
    match s {
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
    conn.execute_batch(EVENTS_TABLE_SCHEMA)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn setup_test_db() -> Connection {
        let conn = Connection::open_in_memory().expect("Failed to create in-memory database");

        // Create minimal issues table for foreign key
        conn.execute_batch(
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
        conn.execute(
            "INSERT INTO issues (id, title) VALUES ('test-001', 'Test Issue')",
            [],
        )
        .expect("Failed to insert test issue");

        conn
    }

    #[test]
    fn test_insert_created_event() {
        let conn = setup_test_db();
        let tx = conn.unchecked_transaction().expect("Failed to start tx");

        let id = insert_created_event(&tx, "test-001", "alice").expect("Failed to insert event");
        tx.commit().expect("Failed to commit");

        assert!(id > 0);

        let events = get_events(&conn, "test-001", 0).expect("Failed to get events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::Created);
        assert_eq!(events[0].actor, "alice");
    }

    #[test]
    fn test_insert_status_changed_event() {
        let conn = setup_test_db();
        let tx = conn.unchecked_transaction().expect("Failed to start tx");

        insert_status_changed_event(&tx, "test-001", "bob", "open", "in_progress")
            .expect("Failed to insert event");
        tx.commit().expect("Failed to commit");

        let events = get_events(&conn, "test-001", 0).expect("Failed to get events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::StatusChanged);
        assert_eq!(events[0].old_value.as_deref(), Some("open"));
        assert_eq!(events[0].new_value.as_deref(), Some("in_progress"));
    }

    #[test]
    fn test_insert_closed_event() {
        let conn = setup_test_db();
        let tx = conn.unchecked_transaction().expect("Failed to start tx");

        insert_closed_event(&tx, "test-001", "carol", Some("Completed the work"))
            .expect("Failed to insert event");
        tx.commit().expect("Failed to commit");

        let events = get_events(&conn, "test-001", 0).expect("Failed to get events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::Closed);
        assert_eq!(events[0].comment.as_deref(), Some("Completed the work"));
    }

    #[test]
    fn test_insert_commented_event() {
        let conn = setup_test_db();
        let tx = conn.unchecked_transaction().expect("Failed to start tx");

        insert_commented_event(&tx, "test-001", "dave", "This is a comment")
            .expect("Failed to insert event");
        tx.commit().expect("Failed to commit");

        let events = get_events(&conn, "test-001", 0).expect("Failed to get events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::Commented);
        assert_eq!(events[0].comment.as_deref(), Some("This is a comment"));
    }

    #[test]
    fn test_insert_dependency_added_event() {
        let conn = setup_test_db();

        // Add second issue for dependency
        conn.execute(
            "INSERT INTO issues (id, title) VALUES ('test-002', 'Blocking Issue')",
            [],
        )
        .expect("Failed to insert second issue");

        let tx = conn.unchecked_transaction().expect("Failed to start tx");
        insert_dependency_added_event(&tx, "test-001", "eve", "blocks", "test-002")
            .expect("Failed to insert event");
        tx.commit().expect("Failed to commit");

        let events = get_events(&conn, "test-001", 0).expect("Failed to get events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::DependencyAdded);
        assert_eq!(events[0].new_value.as_deref(), Some("test-002"));
        assert!(events[0].comment.as_ref().unwrap().contains("blocks"));
    }

    #[test]
    fn test_insert_label_events() {
        let conn = setup_test_db();
        let tx = conn.unchecked_transaction().expect("Failed to start tx");

        insert_label_added_event(&tx, "test-001", "frank", "urgent")
            .expect("Failed to insert label added event");
        insert_label_removed_event(&tx, "test-001", "frank", "urgent")
            .expect("Failed to insert label removed event");
        tx.commit().expect("Failed to commit");

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
            let tx = conn.unchecked_transaction().expect("Failed to start tx");
            insert_commented_event(&tx, "test-001", "user", &format!("Comment {i}"))
                .expect("Failed to insert event");
            tx.commit().expect("Failed to commit");
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
            let tx = conn.unchecked_transaction().expect("Failed to start tx");
            insert_commented_event(&tx, "test-001", "user", &format!("Comment {i}"))
                .expect("Failed to insert event");
            tx.commit().expect("Failed to commit");
        }

        // Get only 3 events
        let events = get_events(&conn, "test-001", 3).expect("Failed to get events");
        assert_eq!(events.len(), 3);

        // Should be newest 3
        assert!(events[0].comment.as_ref().unwrap().contains("Comment 9"));
        assert!(events[2].comment.as_ref().unwrap().contains("Comment 7"));
    }

    #[test]
    fn test_count_events() {
        let conn = setup_test_db();

        // Insert events
        for _ in 0..5 {
            let tx = conn.unchecked_transaction().expect("Failed to start tx");
            insert_commented_event(&tx, "test-001", "user", "A comment")
                .expect("Failed to insert event");
            tx.commit().expect("Failed to commit");
        }

        let count = count_events(&conn, "test-001").expect("Failed to count events");
        assert_eq!(count, 5);
    }

    #[test]
    fn test_deleted_and_restored_events() {
        let conn = setup_test_db();
        let tx = conn.unchecked_transaction().expect("Failed to start tx");

        insert_deleted_event(&tx, "test-001", "admin", Some("Duplicate issue"))
            .expect("Failed to insert deleted event");
        insert_restored_event(&tx, "test-001", "admin", Some("Not a duplicate"))
            .expect("Failed to insert restored event");
        tx.commit().expect("Failed to commit");

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
        let tx = conn.unchecked_transaction().expect("Failed to start tx");

        insert_reopened_event(&tx, "test-001", "manager", Some("Need more work"))
            .expect("Failed to insert reopened event");
        tx.commit().expect("Failed to commit");

        let events = get_events(&conn, "test-001", 0).expect("Failed to get events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::Reopened);
        assert_eq!(events[0].comment.as_deref(), Some("Need more work"));
    }

    #[test]
    fn test_get_all_events() {
        let conn = setup_test_db();

        // Add second issue
        conn.execute(
            "INSERT INTO issues (id, title) VALUES ('test-002', 'Second Issue')",
            [],
        )
        .expect("Failed to insert second issue");

        // Insert events for both issues
        let tx = conn.unchecked_transaction().expect("Failed to start tx");
        insert_created_event(&tx, "test-001", "alice").expect("Failed to insert event");
        insert_created_event(&tx, "test-002", "bob").expect("Failed to insert event");
        tx.commit().expect("Failed to commit");

        let all_events = get_all_events(&conn, 0).expect("Failed to get all events");
        assert_eq!(all_events.len(), 2);
    }

    #[test]
    fn test_multiple_event_types_sequence() {
        let conn = setup_test_db();

        // Simulate a typical issue lifecycle
        let tx = conn.unchecked_transaction().expect("Failed to start tx");
        insert_created_event(&tx, "test-001", "alice").expect("Created");
        tx.commit().expect("Commit");

        let tx = conn.unchecked_transaction().expect("Failed to start tx");
        insert_status_changed_event(&tx, "test-001", "alice", "open", "in_progress")
            .expect("Status change");
        tx.commit().expect("Commit");

        let tx = conn.unchecked_transaction().expect("Failed to start tx");
        insert_commented_event(&tx, "test-001", "bob", "Working on this").expect("Comment");
        tx.commit().expect("Commit");

        let tx = conn.unchecked_transaction().expect("Failed to start tx");
        insert_closed_event(&tx, "test-001", "alice", Some("Done")).expect("Closed");
        tx.commit().expect("Commit");

        let events = get_events(&conn, "test-001", 0).expect("Failed to get events");
        assert_eq!(events.len(), 4);

        // Verify order (newest first)
        assert_eq!(events[0].event_type, EventType::Closed);
        assert_eq!(events[1].event_type, EventType::Commented);
        assert_eq!(events[2].event_type, EventType::StatusChanged);
        assert_eq!(events[3].event_type, EventType::Created);
    }
}
