//! Golden storage snapshots for representative SQLite -> JSONL behavior.
//!
//! The snapshot intentionally masks volatile timestamps while preserving the
//! row shape, event sequence, content hash, and JSONL field layout.

use beads_rust::model::{Issue, IssueType, Priority, Status};
use beads_rust::storage::{IssueUpdate, SqliteStorage};
use chrono::{TimeZone, Utc};
use fsqlite::Connection;
use fsqlite_types::SqliteValue;
use insta::assert_snapshot;
use serde_json::Value;
use std::fmt::Write;
use tempfile::TempDir;

fn value_text(row: &fsqlite::Row, idx: usize) -> String {
    row.get(idx)
        .and_then(SqliteValue::as_text)
        .unwrap_or("")
        .to_string()
}

fn value_i64(row: &fsqlite::Row, idx: usize) -> i64 {
    row.get(idx).and_then(SqliteValue::as_integer).unwrap_or(0)
}

fn fixed_issue() -> Issue {
    let created_at = Utc.with_ymd_and_hms(2026, 4, 22, 12, 0, 0).unwrap();
    Issue {
        id: "storage-golden-1".to_string(),
        content_hash: None,
        title: "Golden storage lifecycle".to_string(),
        description: Some("Initial description".to_string()),
        design: None,
        acceptance_criteria: None,
        notes: None,
        status: Status::Open,
        priority: Priority::MEDIUM,
        issue_type: IssueType::Task,
        assignee: Some("alice".to_string()),
        owner: Some("storage-review".to_string()),
        estimated_minutes: Some(30),
        created_at,
        created_by: Some("golden-create".to_string()),
        updated_at: created_at,
        closed_at: None,
        close_reason: None,
        closed_by_session: None,
        due_at: None,
        defer_until: None,
        external_ref: Some("GOLDEN-1".to_string()),
        source_system: None,
        source_repo: Some(".".to_string()),
        source_repo_path: None,
        agent_context: None,
        deleted_at: None,
        deleted_by: None,
        delete_reason: None,
        original_type: None,
        compaction_level: Some(0),
        compacted_at: None,
        compacted_at_commit: None,
        original_size: Some(0),
        sender: None,
        ephemeral: false,
        pinned: false,
        is_template: false,
        labels: vec![],
        dependencies: vec![],
        comments: vec![],
    }
}

fn push_issue_row_snapshot(out: &mut String, conn: &Connection) {
    let row = conn
        .query_row(
            "SELECT id, title, status, priority, issue_type, assignee, description, \
                    close_reason, closed_by_session, created_at, updated_at, closed_at, content_hash \
             FROM issues WHERE id = 'storage-golden-1'",
        )
        .expect("issue row");

    writeln!(out, "issue:").unwrap();
    writeln!(out, "  id: {}", value_text(&row, 0)).unwrap();
    writeln!(out, "  title: {}", value_text(&row, 1)).unwrap();
    writeln!(out, "  status: {}", value_text(&row, 2)).unwrap();
    writeln!(out, "  priority: {}", value_i64(&row, 3)).unwrap();
    writeln!(out, "  issue_type: {}", value_text(&row, 4)).unwrap();
    writeln!(out, "  assignee: {}", value_text(&row, 5)).unwrap();
    writeln!(out, "  description: {}", value_text(&row, 6)).unwrap();
    writeln!(out, "  close_reason: {}", value_text(&row, 7)).unwrap();
    writeln!(out, "  closed_by_session: {}", value_text(&row, 8)).unwrap();
    writeln!(out, "  created_at: {}", value_text(&row, 9)).unwrap();
    writeln!(out, "  updated_at: <updated_at>").unwrap();
    writeln!(out, "  closed_at: {}", value_text(&row, 11)).unwrap();
    writeln!(out, "  content_hash: {}", value_text(&row, 12)).unwrap();
}

fn push_events_snapshot(out: &mut String, conn: &Connection) {
    let rows = conn
        .query(
            "SELECT id, event_type, actor, old_value, new_value, comment \
             FROM events WHERE issue_id = 'storage-golden-1' ORDER BY id ASC",
        )
        .expect("event rows");

    writeln!(out, "events:").unwrap();
    for row in &rows {
        writeln!(
            out,
            "  - id={} type={} actor={} old={} new={} comment={}",
            value_i64(row, 0),
            value_text(row, 1),
            value_text(row, 2),
            value_text(row, 3),
            value_text(row, 4),
            value_text(row, 5)
        )
        .unwrap();
    }
}

fn normalized_jsonl(jsonl: &[u8]) -> String {
    let mut out = String::new();
    for line in String::from_utf8_lossy(jsonl).lines() {
        let mut value: Value = serde_json::from_str(line).expect("JSONL issue");
        if let Some(obj) = value.as_object_mut() {
            obj.insert(
                "updated_at".to_string(),
                Value::String("<updated_at>".to_string()),
            );
        }
        writeln!(out, "{}", serde_json::to_string_pretty(&value).unwrap()).unwrap();
    }
    out
}

#[test]
fn golden_create_update_close_sqlite_rows_and_jsonl() {
    let dir = TempDir::new().expect("temp dir");
    let db_path = dir.path().join("storage-golden.db");
    let mut storage = SqliteStorage::open(&db_path).expect("open storage");

    let issue = fixed_issue();
    storage
        .create_issue(&issue, "golden-create")
        .expect("create issue");

    storage
        .update_issue(
            "storage-golden-1",
            &IssueUpdate {
                title: Some("Golden storage lifecycle updated".to_string()),
                description: Some(Some("Updated description".to_string())),
                priority: Some(Priority::HIGH),
                ..IssueUpdate::default()
            },
            "golden-update",
        )
        .expect("update issue");

    let closed_at = Utc.with_ymd_and_hms(2026, 4, 23, 8, 30, 0).unwrap();
    storage
        .update_issue(
            "storage-golden-1",
            &IssueUpdate {
                status: Some(Status::Closed),
                closed_at: Some(Some(closed_at)),
                close_reason: Some(Some("completed golden flow".to_string())),
                closed_by_session: Some(Some("golden-session".to_string())),
                ..IssueUpdate::default()
            },
            "golden-close",
        )
        .expect("close issue");

    let conn = Connection::open(db_path.to_string_lossy().into_owned()).expect("open raw db");
    let mut snapshot = String::new();
    push_issue_row_snapshot(&mut snapshot, &conn);
    push_events_snapshot(&mut snapshot, &conn);

    let mut jsonl = Vec::new();
    beads_rust::sync::export_to_writer(&storage, &mut jsonl).expect("export JSONL");
    writeln!(snapshot, "jsonl:").unwrap();
    snapshot.push_str(&normalized_jsonl(&jsonl));

    assert_snapshot!("create_update_close_sqlite_rows_and_jsonl", snapshot);
}
