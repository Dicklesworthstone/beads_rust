//! Storage CRUD unit tests with real `SQLite` (no mocks).
//!
//! Tests `create_issue`, `get_issue`, `update_issue`, `delete_issue` operations.
//! Verifies event creation, dirty marking, and transaction behavior.
#![allow(clippy::similar_names)]

mod common;

use beads_rust::model::{DependencyType, EventType, Issue, IssueType, Priority, Status};
use beads_rust::storage::{IssueUpdate, SqliteStorage};
use chrono::{Duration, Utc};
use common::{fixtures, test_db, test_db_with_dir};

// ============================================================================
// CREATE ISSUE TESTS
// ============================================================================

#[test]
fn create_issue_minimal_fields() {
    let mut storage = test_db();
    let issue = fixtures::issue("minimal-create");

    storage.create_issue(&issue, "tester").unwrap();

    let retrieved = storage.get_issue(&issue.id).unwrap().expect("issue exists");
    assert_eq!(retrieved.id, issue.id);
    assert_eq!(retrieved.title, "minimal-create");
    assert_eq!(retrieved.status, Status::Open);
    assert_eq!(retrieved.priority, Priority::MEDIUM);
    assert_eq!(retrieved.issue_type, IssueType::Task);
}

#[test]
fn create_issue_all_fields_populated() {
    let mut storage = test_db();
    let now = Utc::now();
    let due_date = now + Duration::days(7);
    let defer_date = now + Duration::days(1);

    let issue = Issue {
        id: "test-all-fields".to_string(),
        title: "All Fields Issue".to_string(),
        description: Some("Detailed description".to_string()),
        design: Some("Technical design notes".to_string()),
        acceptance_criteria: Some("Must pass all tests".to_string()),
        notes: Some("Additional notes".to_string()),
        status: Status::Open,
        priority: Priority::HIGH,
        issue_type: IssueType::Feature,
        assignee: Some("alice".to_string()),
        owner: Some("bob".to_string()),
        estimated_minutes: Some(120),
        created_at: now,
        created_by: Some("creator".to_string()),
        updated_at: now,
        due_at: Some(due_date),
        defer_until: Some(defer_date),
        external_ref: Some("JIRA-123".to_string()),
        ephemeral: false,
        pinned: true,
        is_template: false,
        // Relations are populated separately
        labels: vec![],
        dependencies: vec![],
        comments: vec![],
        // Other optional fields
        content_hash: None,
        closed_at: None,
        close_reason: None,
        closed_by_session: None,
        source_system: None,
        deleted_at: None,
        deleted_by: None,
        delete_reason: None,
        original_type: None,
        compaction_level: None,
        compacted_at: None,
        compacted_at_commit: None,
        original_size: None,
        sender: None,
    };

    storage.create_issue(&issue, "tester").unwrap();

    let retrieved = storage.get_issue(&issue.id).unwrap().expect("issue exists");
    assert_eq!(retrieved.title, "All Fields Issue");
    assert_eq!(
        retrieved.description,
        Some("Detailed description".to_string())
    );
    // Note: create_issue only stores basic fields; design, acceptance_criteria, notes
    // require update_issue or upsert_issue_for_import for full field population
    assert_eq!(retrieved.priority, Priority::HIGH);
    assert_eq!(retrieved.issue_type, IssueType::Feature);
    assert_eq!(retrieved.assignee, Some("alice".to_string()));
    assert_eq!(retrieved.owner, Some("bob".to_string()));
    assert_eq!(retrieved.estimated_minutes, Some(120));
    assert_eq!(retrieved.external_ref, Some("JIRA-123".to_string()));
    assert!(retrieved.pinned);
    assert!(!retrieved.is_template);
    assert!(retrieved.due_at.is_some());
    assert!(retrieved.defer_until.is_some());
}

#[test]
fn create_issue_records_created_event() {
    let mut storage = test_db();
    let issue = fixtures::issue("event-create");

    storage.create_issue(&issue, "event-actor").unwrap();

    // Get events for the issue
    let details = storage
        .get_issue_details(&issue.id, false, true, 100)
        .unwrap()
        .expect("issue exists");

    assert!(!details.events.is_empty());
    let created_event = details
        .events
        .iter()
        .find(|e| e.event_type == EventType::Created);
    assert!(created_event.is_some());
    assert_eq!(created_event.unwrap().actor, "event-actor");
}

#[test]
fn create_issue_marks_dirty() {
    let mut storage = test_db();
    let issue = fixtures::issue("dirty-create");

    storage.create_issue(&issue, "tester").unwrap();

    let dirty_ids = storage.get_dirty_issue_ids().unwrap();
    assert!(dirty_ids.contains(&issue.id));
}

#[test]
fn create_duplicate_id_fails() {
    let mut storage = test_db();
    let issue = fixtures::issue("duplicate-id");

    storage.create_issue(&issue, "tester").unwrap();

    // Attempt to create with same ID should fail
    let result = storage.create_issue(&issue, "tester");
    assert!(result.is_err());
}

// ============================================================================
// GET ISSUE TESTS
// ============================================================================

#[test]
fn get_issue_returns_none_for_nonexistent() {
    let storage = test_db();

    let result = storage.get_issue("nonexistent-id").unwrap();
    assert!(result.is_none());
}

#[test]
fn get_issue_returns_issue_with_correct_types() {
    let mut storage = test_db();
    let mut issue = fixtures::issue("type-check");
    issue.priority = Priority::CRITICAL;
    issue.issue_type = IssueType::Bug;
    issue.status = Status::InProgress;

    storage.create_issue(&issue, "tester").unwrap();

    let retrieved = storage.get_issue(&issue.id).unwrap().expect("issue exists");
    assert_eq!(retrieved.priority, Priority::CRITICAL);
    assert_eq!(retrieved.issue_type, IssueType::Bug);
    assert_eq!(retrieved.status, Status::InProgress);
}

#[test]
fn get_issue_for_export_includes_relations() {
    let mut storage = test_db();

    // Create two issues with dependency
    let blocking_issue = fixtures::issue("export-blocker");
    let blocked_issue = fixtures::issue("export-blocked");

    storage.create_issue(&blocking_issue, "tester").unwrap();
    storage.create_issue(&blocked_issue, "tester").unwrap();

    // Add dependency
    storage
        .add_dependency(
            &blocked_issue.id,
            &blocking_issue.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();

    // Add label
    storage
        .add_label(&blocked_issue.id, "test-label", "tester")
        .unwrap();

    // Add comment
    storage
        .add_comment(&blocked_issue.id, "commenter", "Test comment")
        .unwrap();

    // Get for export - should include all relations
    let exported = storage
        .get_issue_for_export(&blocked_issue.id)
        .unwrap()
        .expect("issue exists");

    assert!(!exported.dependencies.is_empty());
    assert!(!exported.labels.is_empty());
    assert!(!exported.comments.is_empty());
    assert!(exported.labels.contains(&"test-label".to_string()));
}

#[test]
fn get_issue_details_includes_events_and_relations() {
    let mut storage = test_db();
    let issue = fixtures::issue("details-test");

    storage.create_issue(&issue, "tester").unwrap();
    storage
        .add_label(&issue.id, "detail-label", "tester")
        .unwrap();

    let details = storage
        .get_issue_details(&issue.id, true, true, 100)
        .unwrap()
        .expect("issue exists");

    assert_eq!(details.issue.id, issue.id);
    assert!(details.labels.contains(&"detail-label".to_string()));
    // Should have at least the Created event
    assert!(!details.events.is_empty());
}

// ============================================================================
// UPDATE ISSUE TESTS
// ============================================================================

#[test]
fn update_issue_single_field_title() {
    let mut storage = test_db();
    let issue = fixtures::issue("update-title");

    storage.create_issue(&issue, "tester").unwrap();

    let update = IssueUpdate {
        title: Some("Updated Title".to_string()),
        ..Default::default()
    };

    let updated = storage.update_issue(&issue.id, &update, "updater").unwrap();
    assert_eq!(updated.title, "Updated Title");

    // Verify persisted
    let retrieved = storage.get_issue(&issue.id).unwrap().expect("issue exists");
    assert_eq!(retrieved.title, "Updated Title");
}

#[test]
fn update_issue_multiple_fields() {
    let mut storage = test_db();
    let issue = fixtures::issue("update-multiple");

    storage.create_issue(&issue, "tester").unwrap();

    let update = IssueUpdate {
        title: Some("New Title".to_string()),
        description: Some(Some("New description".to_string())),
        priority: Some(Priority::HIGH),
        assignee: Some(Some("new-assignee".to_string())),
        ..Default::default()
    };

    let updated = storage.update_issue(&issue.id, &update, "updater").unwrap();
    assert_eq!(updated.title, "New Title");
    assert_eq!(updated.description, Some("New description".to_string()));
    assert_eq!(updated.priority, Priority::HIGH);
    assert_eq!(updated.assignee, Some("new-assignee".to_string()));
}

#[test]
fn update_issue_status_records_event() {
    let mut storage = test_db();
    let issue = fixtures::issue("update-status-event");

    storage.create_issue(&issue, "tester").unwrap();

    let update = IssueUpdate {
        status: Some(Status::InProgress),
        ..Default::default()
    };

    storage.update_issue(&issue.id, &update, "updater").unwrap();

    let details = storage
        .get_issue_details(&issue.id, false, true, 100)
        .unwrap()
        .expect("issue exists");

    let status_event = details
        .events
        .iter()
        .find(|e| e.event_type == EventType::StatusChanged);
    assert!(status_event.is_some());

    let event = status_event.unwrap();
    assert_eq!(event.old_value, Some("open".to_string()));
    assert_eq!(event.new_value, Some("in_progress".to_string()));
}

#[test]
fn update_issue_priority_records_event() {
    let mut storage = test_db();
    let issue = fixtures::issue("update-priority-event");

    storage.create_issue(&issue, "tester").unwrap();

    let update = IssueUpdate {
        priority: Some(Priority::CRITICAL),
        ..Default::default()
    };

    storage.update_issue(&issue.id, &update, "updater").unwrap();

    let details = storage
        .get_issue_details(&issue.id, false, true, 100)
        .unwrap()
        .expect("issue exists");

    let priority_event = details
        .events
        .iter()
        .find(|e| e.event_type == EventType::PriorityChanged);
    assert!(priority_event.is_some());
}

#[test]
fn update_issue_empty_update_is_noop() {
    let mut storage = test_db();
    let issue = fixtures::issue("empty-update");

    storage.create_issue(&issue, "tester").unwrap();

    // Clear dirty flags first
    let dirty = storage.get_dirty_issue_ids().unwrap();
    storage.clear_dirty_flags(&dirty).unwrap();

    let update = IssueUpdate::default();
    let updated = storage.update_issue(&issue.id, &update, "updater").unwrap();

    // Should return the original issue unchanged
    assert_eq!(updated.title, issue.title);

    // Should NOT mark dirty since nothing changed
    let dirty_after = storage.get_dirty_issue_ids().unwrap();
    assert!(dirty_after.is_empty());
}

#[test]
fn update_issue_marks_dirty() {
    let mut storage = test_db();
    let issue = fixtures::issue("dirty-update");

    storage.create_issue(&issue, "tester").unwrap();

    // Clear dirty from create
    let dirty = storage.get_dirty_issue_ids().unwrap();
    storage.clear_dirty_flags(&dirty).unwrap();

    let update = IssueUpdate {
        title: Some("Changed".to_string()),
        ..Default::default()
    };
    storage.update_issue(&issue.id, &update, "updater").unwrap();

    let dirty_ids = storage.get_dirty_issue_ids().unwrap();
    assert!(dirty_ids.contains(&issue.id));
}

#[test]
fn update_nonexistent_issue_fails() {
    let mut storage = test_db();

    let update = IssueUpdate {
        title: Some("New Title".to_string()),
        ..Default::default()
    };

    let result = storage.update_issue("nonexistent", &update, "updater");
    assert!(result.is_err());
}

#[test]
fn update_issue_clear_optional_fields() {
    let mut storage = test_db();

    // Create issue with all optional fields set
    let issue = Issue {
        id: "test-clear-fields".to_string(),
        title: "Clear Fields Test".to_string(),
        description: Some("Description".to_string()),
        assignee: Some("alice".to_string()),
        owner: Some("bob".to_string()),
        estimated_minutes: Some(60),
        status: Status::Open,
        priority: Priority::MEDIUM,
        issue_type: IssueType::Task,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        content_hash: None,
        design: None,
        acceptance_criteria: None,
        notes: None,
        created_by: None,
        closed_at: None,
        close_reason: None,
        closed_by_session: None,
        due_at: None,
        defer_until: None,
        external_ref: None,
        source_system: None,
        deleted_at: None,
        deleted_by: None,
        delete_reason: None,
        original_type: None,
        compaction_level: None,
        compacted_at: None,
        compacted_at_commit: None,
        original_size: None,
        sender: None,
        ephemeral: false,
        pinned: false,
        is_template: false,
        labels: vec![],
        dependencies: vec![],
        comments: vec![],
    };

    storage.create_issue(&issue, "tester").unwrap();

    // Clear the optional fields by setting to None
    let update = IssueUpdate {
        description: Some(None),
        assignee: Some(None),
        owner: Some(None),
        estimated_minutes: Some(None),
        ..Default::default()
    };

    let updated = storage.update_issue(&issue.id, &update, "updater").unwrap();
    assert!(updated.description.is_none());
    assert!(updated.assignee.is_none());
    assert!(updated.owner.is_none());
    assert!(updated.estimated_minutes.is_none());
}

// ============================================================================
// DELETE ISSUE TESTS (Soft Delete / Tombstone)
// ============================================================================

#[test]
fn delete_issue_creates_tombstone() {
    let mut storage = test_db();
    let issue = fixtures::issue("soft-delete");

    storage.create_issue(&issue, "tester").unwrap();
    storage
        .delete_issue(&issue.id, "deleter", "No longer needed")
        .unwrap();

    let deleted = storage.get_issue(&issue.id).unwrap().expect("issue exists");
    assert_eq!(deleted.status, Status::Tombstone);
    assert!(deleted.deleted_at.is_some());
    assert_eq!(deleted.deleted_by, Some("deleter".to_string()));
    assert_eq!(deleted.delete_reason, Some("No longer needed".to_string()));
    assert_eq!(deleted.original_type, Some("task".to_string()));
}

#[test]
fn delete_issue_records_event() {
    let mut storage = test_db();
    let issue = fixtures::issue("delete-event");

    storage.create_issue(&issue, "tester").unwrap();
    storage
        .delete_issue(&issue.id, "deleter", "Test deletion")
        .unwrap();

    let details = storage
        .get_issue_details(&issue.id, false, true, 100)
        .unwrap()
        .expect("issue exists");

    let deleted_event = details
        .events
        .iter()
        .find(|e| e.event_type == EventType::Deleted);
    assert!(deleted_event.is_some());
    assert_eq!(deleted_event.unwrap().actor, "deleter");
}

#[test]
fn delete_issue_marks_dirty() {
    let mut storage = test_db();
    let issue = fixtures::issue("dirty-delete");

    storage.create_issue(&issue, "tester").unwrap();

    // Clear dirty from create
    let dirty = storage.get_dirty_issue_ids().unwrap();
    storage.clear_dirty_flags(&dirty).unwrap();

    storage
        .delete_issue(&issue.id, "deleter", "Cleanup")
        .unwrap();

    let dirty_ids = storage.get_dirty_issue_ids().unwrap();
    assert!(dirty_ids.contains(&issue.id));
}

#[test]
fn delete_nonexistent_issue_fails() {
    let mut storage = test_db();

    let result = storage.delete_issue("nonexistent", "deleter", "reason");
    assert!(result.is_err());
}

#[test]
fn deleted_issues_excluded_from_list() {
    let mut storage = test_db();

    let issue1 = fixtures::issue("list-active");
    let issue2 = fixtures::issue("list-deleted");

    storage.create_issue(&issue1, "tester").unwrap();
    storage.create_issue(&issue2, "tester").unwrap();
    storage
        .delete_issue(&issue2.id, "deleter", "removed")
        .unwrap();

    let filters = beads_rust::storage::ListFilters::default();
    let listed = storage.list_issues(&filters).unwrap();

    let ids: Vec<_> = listed.iter().map(|i| i.id.clone()).collect();
    assert!(ids.contains(&issue1.id));
    assert!(!ids.contains(&issue2.id));
}

// ============================================================================
// DIRTY TRACKING TESTS
// ============================================================================

#[test]
fn get_dirty_issue_ids_returns_all_dirty() {
    let mut storage = test_db();

    let issue1 = fixtures::issue("dirty-1");
    let issue2 = fixtures::issue("dirty-2");
    let issue3 = fixtures::issue("dirty-3");

    storage.create_issue(&issue1, "tester").unwrap();
    storage.create_issue(&issue2, "tester").unwrap();
    storage.create_issue(&issue3, "tester").unwrap();

    let dirty_ids = storage.get_dirty_issue_ids().unwrap();
    assert_eq!(dirty_ids.len(), 3);
    assert!(dirty_ids.contains(&issue1.id));
    assert!(dirty_ids.contains(&issue2.id));
    assert!(dirty_ids.contains(&issue3.id));
}

#[test]
fn clear_dirty_flags_removes_specified() {
    let mut storage = test_db();

    let issue1 = fixtures::issue("clear-dirty-1");
    let issue2 = fixtures::issue("clear-dirty-2");

    storage.create_issue(&issue1, "tester").unwrap();
    storage.create_issue(&issue2, "tester").unwrap();

    // Clear only issue1
    storage
        .clear_dirty_flags(std::slice::from_ref(&issue1.id))
        .unwrap();

    let dirty_ids = storage.get_dirty_issue_ids().unwrap();
    assert!(!dirty_ids.contains(&issue1.id));
    assert!(dirty_ids.contains(&issue2.id));
}

#[test]
fn clear_dirty_flags_empty_list_is_noop() {
    let mut storage = test_db();
    let issue = fixtures::issue("clear-empty");

    storage.create_issue(&issue, "tester").unwrap();

    let count = storage.clear_dirty_flags(&[]).unwrap();
    assert_eq!(count, 0);

    let dirty_ids = storage.get_dirty_issue_ids().unwrap();
    assert!(dirty_ids.contains(&issue.id));
}

// ============================================================================
// TRANSACTION BEHAVIOR TESTS
// ============================================================================

#[test]
fn failed_create_does_not_persist() {
    let mut storage = test_db();
    let issue = fixtures::issue("transaction-test");

    storage.create_issue(&issue, "tester").unwrap();

    // Attempt to create duplicate (should fail)
    let result = storage.create_issue(&issue, "tester");
    assert!(result.is_err());

    // Original should still exist unchanged
    let retrieved = storage.get_issue(&issue.id).unwrap().expect("issue exists");
    assert_eq!(retrieved.title, issue.title);

    // Only one issue should exist
    let all_issues = storage
        .list_issues(&beads_rust::storage::ListFilters::default())
        .unwrap();
    assert_eq!(all_issues.len(), 1);
}

// ============================================================================
// UPSERT TESTS
// ============================================================================

#[test]
fn upsert_issue_for_import_creates_new() {
    let mut storage = test_db();
    let issue = fixtures::issue("upsert-new");

    storage.upsert_issue_for_import(&issue).unwrap();

    let retrieved = storage.get_issue(&issue.id).unwrap().expect("issue exists");
    assert_eq!(retrieved.title, issue.title);
}

#[test]
fn upsert_issue_for_import_updates_existing() {
    let mut storage = test_db();
    let mut issue = fixtures::issue("upsert-existing");

    storage.create_issue(&issue, "tester").unwrap();

    // Modify and upsert
    issue.title = "Updated via upsert".to_string();
    issue.description = Some("New description".to_string());

    storage.upsert_issue_for_import(&issue).unwrap();

    let retrieved = storage.get_issue(&issue.id).unwrap().expect("issue exists");
    assert_eq!(retrieved.title, "Updated via upsert");
    assert_eq!(retrieved.description, Some("New description".to_string()));
}

#[test]
fn upsert_issue_stores_all_fields() {
    let mut storage = test_db();
    let now = Utc::now();

    let issue = Issue {
        id: "test-upsert-all".to_string(),
        title: "Upsert All Fields".to_string(),
        description: Some("Detailed description".to_string()),
        design: Some("Technical design notes".to_string()),
        acceptance_criteria: Some("Must pass all tests".to_string()),
        notes: Some("Additional notes".to_string()),
        status: Status::Open,
        priority: Priority::HIGH,
        issue_type: IssueType::Feature,
        assignee: Some("alice".to_string()),
        owner: Some("bob".to_string()),
        estimated_minutes: Some(120),
        created_at: now,
        created_by: Some("creator".to_string()),
        updated_at: now,
        due_at: Some(now + Duration::days(7)),
        defer_until: Some(now + Duration::days(1)),
        external_ref: Some("JIRA-456".to_string()),
        ephemeral: false,
        pinned: true,
        is_template: false,
        labels: vec![],
        dependencies: vec![],
        comments: vec![],
        content_hash: Some("abc123".to_string()),
        closed_at: None,
        close_reason: None,
        closed_by_session: None,
        source_system: Some("test".to_string()),
        deleted_at: None,
        deleted_by: None,
        delete_reason: None,
        original_type: None,
        compaction_level: None,
        compacted_at: None,
        compacted_at_commit: None,
        original_size: None,
        sender: None,
    };

    storage.upsert_issue_for_import(&issue).unwrap();

    let retrieved = storage.get_issue(&issue.id).unwrap().expect("issue exists");
    assert_eq!(retrieved.title, "Upsert All Fields");
    assert_eq!(
        retrieved.description,
        Some("Detailed description".to_string())
    );
    assert_eq!(retrieved.design, Some("Technical design notes".to_string()));
    assert_eq!(
        retrieved.acceptance_criteria,
        Some("Must pass all tests".to_string())
    );
    assert_eq!(retrieved.notes, Some("Additional notes".to_string()));
    assert_eq!(retrieved.priority, Priority::HIGH);
    assert_eq!(retrieved.issue_type, IssueType::Feature);
    assert_eq!(retrieved.assignee, Some("alice".to_string()));
    assert_eq!(retrieved.owner, Some("bob".to_string()));
    assert_eq!(retrieved.estimated_minutes, Some(120));
    assert_eq!(retrieved.external_ref, Some("JIRA-456".to_string()));
    assert_eq!(retrieved.content_hash, Some("abc123".to_string()));
    assert_eq!(retrieved.source_system, Some("test".to_string()));
    assert!(retrieved.pinned);
    assert!(!retrieved.is_template);
}

// ============================================================================
// ID EXISTENCE TESTS
// ============================================================================

#[test]
fn id_exists_returns_true_for_existing() {
    let mut storage = test_db();
    let issue = fixtures::issue("exists-test");

    storage.create_issue(&issue, "tester").unwrap();

    assert!(storage.id_exists(&issue.id).unwrap());
}

#[test]
fn id_exists_returns_false_for_nonexistent() {
    let storage = test_db();

    assert!(!storage.id_exists("nonexistent-id").unwrap());
}

// ============================================================================
// COUNT TESTS
// ============================================================================

#[test]
fn count_issues_returns_correct_count() {
    let mut storage = test_db();

    assert_eq!(storage.count_issues().unwrap(), 0);

    let issue1 = fixtures::issue("count-1");
    let issue2 = fixtures::issue("count-2");
    let issue3 = fixtures::issue("count-3");

    storage.create_issue(&issue1, "tester").unwrap();
    assert_eq!(storage.count_issues().unwrap(), 1);

    storage.create_issue(&issue2, "tester").unwrap();
    storage.create_issue(&issue3, "tester").unwrap();
    assert_eq!(storage.count_issues().unwrap(), 3);
}

// ============================================================================
// PERSISTENCE TESTS (with file-backed DB)
// ============================================================================

#[test]
fn data_persists_across_connections() {
    let (mut storage, dir) = test_db_with_dir();
    let db_path = dir.path().join(".beads").join("beads.db");
    let issue = fixtures::issue("persist-test");

    storage.create_issue(&issue, "tester").unwrap();
    drop(storage);

    // Reopen and verify
    let storage2 = SqliteStorage::open(&db_path).unwrap();
    let retrieved = storage2
        .get_issue(&issue.id)
        .unwrap()
        .expect("issue exists");
    assert_eq!(retrieved.title, issue.title);
}
