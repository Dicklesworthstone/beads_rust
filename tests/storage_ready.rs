//! Storage unit tests for ready issues functionality.
//!
//! Tests: `get_ready_issues` with various filters (assignee, unassigned, types,
//! priorities, `labels_and`, `labels_or`, `include_deferred`, limit) and sort policies
//! (Hybrid, Priority, Oldest). Real `SQLite`, no mocks.

mod common;

use beads_rust::model::{DependencyType, IssueType, Priority, Status};
use beads_rust::storage::{ReadyFilters, ReadySortPolicy, SqliteStorage};
use common::{fixtures, test_db};

// ============================================================================
// HELPER FUNCTIONS
// ============================================================================

fn ready_ids(
    storage: &mut SqliteStorage,
    filters: &ReadyFilters,
    sort: ReadySortPolicy,
) -> Vec<String> {
    storage
        .get_ready_issues(filters, sort)
        .unwrap()
        .into_iter()
        .map(|i| i.id)
        .collect()
}

// ============================================================================
// ASSIGNEE FILTER TESTS
// ============================================================================

#[test]
fn ready_filter_by_assignee() {
    let mut storage = test_db();

    let issue1 = fixtures::IssueBuilder::new("Assigned to Alice")
        .with_assignee("alice")
        .build();
    let issue2 = fixtures::IssueBuilder::new("Assigned to Bob")
        .with_assignee("bob")
        .build();
    let issue3 = fixtures::IssueBuilder::new("Unassigned issue").build();

    storage.create_issue(&issue1, "tester").unwrap();
    storage.create_issue(&issue2, "tester").unwrap();
    storage.create_issue(&issue3, "tester").unwrap();

    let filters = ReadyFilters {
        assignee: Some("alice".to_string()),
        ..Default::default()
    };

    let ids = ready_ids(&mut storage, &filters, ReadySortPolicy::Oldest);
    assert_eq!(ids.len(), 1);
    assert!(ids.contains(&issue1.id));
    assert!(!ids.contains(&issue2.id));
    assert!(!ids.contains(&issue3.id));
}

#[test]
fn ready_filter_unassigned_only() {
    let mut storage = test_db();

    let assigned = fixtures::IssueBuilder::new("Assigned issue")
        .with_assignee("someone")
        .build();
    let unassigned1 = fixtures::IssueBuilder::new("Unassigned 1").build();
    let unassigned2 = fixtures::IssueBuilder::new("Unassigned 2").build();

    storage.create_issue(&assigned, "tester").unwrap();
    storage.create_issue(&unassigned1, "tester").unwrap();
    storage.create_issue(&unassigned2, "tester").unwrap();

    let filters = ReadyFilters {
        unassigned: true,
        ..Default::default()
    };

    let ids = ready_ids(&mut storage, &filters, ReadySortPolicy::Oldest);
    assert_eq!(ids.len(), 2);
    assert!(!ids.contains(&assigned.id));
    assert!(ids.contains(&unassigned1.id));
    assert!(ids.contains(&unassigned2.id));
}

// ============================================================================
// TYPE FILTER TESTS
// ============================================================================

#[test]
fn ready_filter_by_single_type() {
    let mut storage = test_db();

    let bug = fixtures::IssueBuilder::new("Bug issue")
        .with_type(IssueType::Bug)
        .build();
    let feature = fixtures::IssueBuilder::new("Feature issue")
        .with_type(IssueType::Feature)
        .build();
    let task = fixtures::IssueBuilder::new("Task issue")
        .with_type(IssueType::Task)
        .build();

    storage.create_issue(&bug, "tester").unwrap();
    storage.create_issue(&feature, "tester").unwrap();
    storage.create_issue(&task, "tester").unwrap();

    let filters = ReadyFilters {
        types: Some(vec![IssueType::Bug]),
        ..Default::default()
    };

    let ids = ready_ids(&mut storage, &filters, ReadySortPolicy::Oldest);
    assert_eq!(ids.len(), 1);
    assert!(ids.contains(&bug.id));
}

#[test]
fn ready_filter_by_multiple_types() {
    let mut storage = test_db();

    let bug = fixtures::IssueBuilder::new("Bug issue")
        .with_type(IssueType::Bug)
        .build();
    let feature = fixtures::IssueBuilder::new("Feature issue")
        .with_type(IssueType::Feature)
        .build();
    let task = fixtures::IssueBuilder::new("Task issue")
        .with_type(IssueType::Task)
        .build();

    storage.create_issue(&bug, "tester").unwrap();
    storage.create_issue(&feature, "tester").unwrap();
    storage.create_issue(&task, "tester").unwrap();

    let filters = ReadyFilters {
        types: Some(vec![IssueType::Bug, IssueType::Feature]),
        ..Default::default()
    };

    let ids = ready_ids(&mut storage, &filters, ReadySortPolicy::Oldest);
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&bug.id));
    assert!(ids.contains(&feature.id));
    assert!(!ids.contains(&task.id));
}

// ============================================================================
// PRIORITY FILTER TESTS
// ============================================================================

#[test]
fn ready_filter_by_single_priority() {
    let mut storage = test_db();

    let p0 = fixtures::IssueBuilder::new("Critical issue")
        .with_priority(Priority::CRITICAL)
        .build();
    let p1 = fixtures::IssueBuilder::new("High issue")
        .with_priority(Priority::HIGH)
        .build();
    let p2 = fixtures::IssueBuilder::new("Medium issue")
        .with_priority(Priority::MEDIUM)
        .build();

    storage.create_issue(&p0, "tester").unwrap();
    storage.create_issue(&p1, "tester").unwrap();
    storage.create_issue(&p2, "tester").unwrap();

    let filters = ReadyFilters {
        priorities: Some(vec![Priority::CRITICAL]),
        ..Default::default()
    };

    let ids = ready_ids(&mut storage, &filters, ReadySortPolicy::Oldest);
    assert_eq!(ids.len(), 1);
    assert!(ids.contains(&p0.id));
}

#[test]
fn ready_filter_by_multiple_priorities() {
    let mut storage = test_db();

    let p0 = fixtures::IssueBuilder::new("Critical issue")
        .with_priority(Priority::CRITICAL)
        .build();
    let p1 = fixtures::IssueBuilder::new("High issue")
        .with_priority(Priority::HIGH)
        .build();
    let p2 = fixtures::IssueBuilder::new("Medium issue")
        .with_priority(Priority::MEDIUM)
        .build();
    let p3 = fixtures::IssueBuilder::new("Low issue")
        .with_priority(Priority::LOW)
        .build();

    storage.create_issue(&p0, "tester").unwrap();
    storage.create_issue(&p1, "tester").unwrap();
    storage.create_issue(&p2, "tester").unwrap();
    storage.create_issue(&p3, "tester").unwrap();

    let filters = ReadyFilters {
        priorities: Some(vec![Priority::CRITICAL, Priority::HIGH]),
        ..Default::default()
    };

    let ids = ready_ids(&mut storage, &filters, ReadySortPolicy::Oldest);
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&p0.id));
    assert!(ids.contains(&p1.id));
    assert!(!ids.contains(&p2.id));
    assert!(!ids.contains(&p3.id));
}

// ============================================================================
// LABEL FILTER TESTS
// ============================================================================

#[test]
fn ready_filter_by_labels_and_single() {
    let mut storage = test_db();

    let issue1 = fixtures::issue("Has backend label");
    let issue2 = fixtures::issue("Has frontend label");
    let issue3 = fixtures::issue("No labels");

    storage.create_issue(&issue1, "tester").unwrap();
    storage.create_issue(&issue2, "tester").unwrap();
    storage.create_issue(&issue3, "tester").unwrap();

    storage.add_label(&issue1.id, "backend", "tester").unwrap();
    storage.add_label(&issue2.id, "frontend", "tester").unwrap();

    let filters = ReadyFilters {
        labels_and: vec!["backend".to_string()],
        ..Default::default()
    };

    let ids = ready_ids(&mut storage, &filters, ReadySortPolicy::Oldest);
    assert_eq!(ids.len(), 1);
    assert!(ids.contains(&issue1.id));
}

#[test]
fn ready_filter_by_labels_and_multiple() {
    let mut storage = test_db();

    let issue1 = fixtures::issue("Has both labels");
    let issue2 = fixtures::issue("Has only backend");
    let issue3 = fixtures::issue("Has only frontend");

    storage.create_issue(&issue1, "tester").unwrap();
    storage.create_issue(&issue2, "tester").unwrap();
    storage.create_issue(&issue3, "tester").unwrap();

    storage.add_label(&issue1.id, "backend", "tester").unwrap();
    storage.add_label(&issue1.id, "urgent", "tester").unwrap();
    storage.add_label(&issue2.id, "backend", "tester").unwrap();
    storage.add_label(&issue3.id, "urgent", "tester").unwrap();

    // AND logic: must have both labels
    let filters = ReadyFilters {
        labels_and: vec!["backend".to_string(), "urgent".to_string()],
        ..Default::default()
    };

    let ids = ready_ids(&mut storage, &filters, ReadySortPolicy::Oldest);
    assert_eq!(ids.len(), 1);
    assert!(ids.contains(&issue1.id));
}

#[test]
fn ready_filter_by_labels_or() {
    let mut storage = test_db();

    let issue1 = fixtures::issue("Has backend label");
    let issue2 = fixtures::issue("Has frontend label");
    let issue3 = fixtures::issue("No labels");

    storage.create_issue(&issue1, "tester").unwrap();
    storage.create_issue(&issue2, "tester").unwrap();
    storage.create_issue(&issue3, "tester").unwrap();

    storage.add_label(&issue1.id, "backend", "tester").unwrap();
    storage.add_label(&issue2.id, "frontend", "tester").unwrap();

    // OR logic: has any of the labels
    let filters = ReadyFilters {
        labels_or: vec!["backend".to_string(), "frontend".to_string()],
        ..Default::default()
    };

    let ids = ready_ids(&mut storage, &filters, ReadySortPolicy::Oldest);
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&issue1.id));
    assert!(ids.contains(&issue2.id));
    assert!(!ids.contains(&issue3.id));
}

// ============================================================================
// LIMIT FILTER TESTS
// ============================================================================

#[test]
fn ready_filter_with_limit() {
    let mut storage = test_db();

    // Create 5 issues
    for i in 1..=5 {
        let issue = fixtures::issue(&format!("Issue {i}"));
        storage.create_issue(&issue, "tester").unwrap();
    }

    let filters = ReadyFilters {
        limit: Some(3),
        ..Default::default()
    };

    let ids = ready_ids(&mut storage, &filters, ReadySortPolicy::Oldest);
    assert_eq!(ids.len(), 3);
}

#[test]
fn ready_filter_limit_zero_returns_all() {
    let mut storage = test_db();

    // Create 3 issues
    for i in 1..=3 {
        let issue = fixtures::issue(&format!("Issue {i}"));
        storage.create_issue(&issue, "tester").unwrap();
    }

    let filters = ReadyFilters {
        limit: Some(0), // Zero means no limit
        ..Default::default()
    };

    let ids = ready_ids(&mut storage, &filters, ReadySortPolicy::Oldest);
    assert_eq!(ids.len(), 3);
}

#[test]
fn ready_filter_limit_greater_than_total() {
    let mut storage = test_db();

    // Create 2 issues
    let issue1 = fixtures::issue("Issue 1");
    let issue2 = fixtures::issue("Issue 2");
    storage.create_issue(&issue1, "tester").unwrap();
    storage.create_issue(&issue2, "tester").unwrap();

    let filters = ReadyFilters {
        limit: Some(100), // More than available
        ..Default::default()
    };

    let ids = ready_ids(&mut storage, &filters, ReadySortPolicy::Oldest);
    assert_eq!(ids.len(), 2);
}

// ============================================================================
// SORT POLICY TESTS
// ============================================================================

#[test]
fn ready_sort_policy_priority() {
    let mut storage = test_db();

    let p2 = fixtures::IssueBuilder::new("Medium first")
        .with_priority(Priority::MEDIUM)
        .build();
    let p0 = fixtures::IssueBuilder::new("Critical second")
        .with_priority(Priority::CRITICAL)
        .build();
    let p1 = fixtures::IssueBuilder::new("High third")
        .with_priority(Priority::HIGH)
        .build();

    // Create in specific order to test that sorting overrides creation order
    storage.create_issue(&p2, "tester").unwrap();
    storage.create_issue(&p0, "tester").unwrap();
    storage.create_issue(&p1, "tester").unwrap();

    let filters = ReadyFilters::default();
    let ids = ready_ids(&mut storage, &filters, ReadySortPolicy::Priority);

    // Should be sorted by priority: P0, P1, P2
    assert_eq!(ids.len(), 3);
    assert_eq!(ids[0], p0.id);
    assert_eq!(ids[1], p1.id);
    assert_eq!(ids[2], p2.id);
}

#[test]
fn ready_sort_policy_oldest() {
    let mut storage = test_db();

    // Create issues - they get created_at in order
    let first = fixtures::issue("First created");
    let second = fixtures::issue("Second created");
    let third = fixtures::issue("Third created");

    storage.create_issue(&first, "tester").unwrap();
    storage.create_issue(&second, "tester").unwrap();
    storage.create_issue(&third, "tester").unwrap();

    let filters = ReadyFilters::default();
    let ids = ready_ids(&mut storage, &filters, ReadySortPolicy::Oldest);

    // Should be sorted by created_at ASC (oldest first)
    assert_eq!(ids.len(), 3);
    assert_eq!(ids[0], first.id);
    assert_eq!(ids[1], second.id);
    assert_eq!(ids[2], third.id);
}

#[test]
fn ready_sort_policy_hybrid() {
    let mut storage = test_db();

    // Create issues with different priorities - P0/P1 should come first
    let p3 = fixtures::IssueBuilder::new("Low priority")
        .with_priority(Priority::LOW)
        .build();
    let p0 = fixtures::IssueBuilder::new("Critical priority")
        .with_priority(Priority::CRITICAL)
        .build();
    let p2 = fixtures::IssueBuilder::new("Medium priority")
        .with_priority(Priority::MEDIUM)
        .build();
    let p1 = fixtures::IssueBuilder::new("High priority")
        .with_priority(Priority::HIGH)
        .build();

    // Create in mixed order
    storage.create_issue(&p3, "tester").unwrap();
    storage.create_issue(&p0, "tester").unwrap();
    storage.create_issue(&p2, "tester").unwrap();
    storage.create_issue(&p1, "tester").unwrap();

    let filters = ReadyFilters::default();
    let ids = ready_ids(&mut storage, &filters, ReadySortPolicy::Hybrid);

    // Hybrid: P0/P1 first (by created_at), then P2+ (by created_at)
    // P0 and P1 should be in first two positions
    assert_eq!(ids.len(), 4);

    // First two should be P0/P1 (critical/high) in creation order
    assert!(ids[0] == p0.id || ids[0] == p1.id);
    assert!(ids[1] == p0.id || ids[1] == p1.id);

    // Last two should be P2/P3 (medium/low) in creation order
    assert!(ids[2] == p2.id || ids[2] == p3.id);
    assert!(ids[3] == p2.id || ids[3] == p3.id);
}

// ============================================================================
// COMBINED FILTER TESTS
// ============================================================================

#[test]
fn ready_combined_assignee_and_type_filter() {
    let mut storage = test_db();

    let alice_bug = fixtures::IssueBuilder::new("Alice bug")
        .with_assignee("alice")
        .with_type(IssueType::Bug)
        .build();
    let alice_task = fixtures::IssueBuilder::new("Alice task")
        .with_assignee("alice")
        .with_type(IssueType::Task)
        .build();
    let bob_bug = fixtures::IssueBuilder::new("Bob bug")
        .with_assignee("bob")
        .with_type(IssueType::Bug)
        .build();

    storage.create_issue(&alice_bug, "tester").unwrap();
    storage.create_issue(&alice_task, "tester").unwrap();
    storage.create_issue(&bob_bug, "tester").unwrap();

    let filters = ReadyFilters {
        assignee: Some("alice".to_string()),
        types: Some(vec![IssueType::Bug]),
        ..Default::default()
    };

    let ids = ready_ids(&mut storage, &filters, ReadySortPolicy::Oldest);
    assert_eq!(ids.len(), 1);
    assert!(ids.contains(&alice_bug.id));
}

#[test]
fn ready_combined_priority_and_label_filter() {
    let mut storage = test_db();

    let p0_backend = fixtures::IssueBuilder::new("Critical backend")
        .with_priority(Priority::CRITICAL)
        .build();
    let p0_frontend = fixtures::IssueBuilder::new("Critical frontend")
        .with_priority(Priority::CRITICAL)
        .build();
    let p2_backend = fixtures::IssueBuilder::new("Medium backend")
        .with_priority(Priority::MEDIUM)
        .build();

    storage.create_issue(&p0_backend, "tester").unwrap();
    storage.create_issue(&p0_frontend, "tester").unwrap();
    storage.create_issue(&p2_backend, "tester").unwrap();

    storage
        .add_label(&p0_backend.id, "backend", "tester")
        .unwrap();
    storage
        .add_label(&p0_frontend.id, "frontend", "tester")
        .unwrap();
    storage
        .add_label(&p2_backend.id, "backend", "tester")
        .unwrap();

    let filters = ReadyFilters {
        priorities: Some(vec![Priority::CRITICAL]),
        labels_and: vec!["backend".to_string()],
        ..Default::default()
    };

    let ids = ready_ids(&mut storage, &filters, ReadySortPolicy::Oldest);
    assert_eq!(ids.len(), 1);
    assert!(ids.contains(&p0_backend.id));
}

// ============================================================================
// BLOCKED ISSUE INTERACTION TESTS
// ============================================================================

#[test]
fn ready_excludes_blocked_issues_with_filters() {
    let mut storage = test_db();

    let blocker = fixtures::IssueBuilder::new("Blocker")
        .with_type(IssueType::Bug)
        .build();
    let blocked_issue = fixtures::IssueBuilder::new("Blocked bug")
        .with_type(IssueType::Bug)
        .build();
    let unblocked_bug = fixtures::IssueBuilder::new("Unblocked bug")
        .with_type(IssueType::Bug)
        .build();

    storage.create_issue(&blocker, "tester").unwrap();
    storage.create_issue(&blocked_issue, "tester").unwrap();
    storage.create_issue(&unblocked_bug, "tester").unwrap();

    // Add dependency - blocked depends on blocker
    storage
        .add_dependency(
            &blocked_issue.id,
            &blocker.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();

    // Filter by type=bug - should still exclude blocked
    let filters = ReadyFilters {
        types: Some(vec![IssueType::Bug]),
        ..Default::default()
    };

    let ids = ready_ids(&mut storage, &filters, ReadySortPolicy::Oldest);
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&blocker.id));
    assert!(ids.contains(&unblocked_bug.id));
    assert!(!ids.contains(&blocked_issue.id));
}

// ============================================================================
// STATUS INTERACTION TESTS
// ============================================================================

#[test]
fn ready_includes_open_and_in_progress() {
    let mut storage = test_db();

    let open = fixtures::IssueBuilder::new("Open issue")
        .with_status(Status::Open)
        .build();
    let in_progress = fixtures::IssueBuilder::new("In progress issue")
        .with_status(Status::InProgress)
        .build();
    let closed = fixtures::IssueBuilder::new("Closed issue")
        .with_status(Status::Closed)
        .build();
    let deferred = fixtures::IssueBuilder::new("Deferred issue")
        .with_status(Status::Deferred)
        .build();

    storage.create_issue(&open, "tester").unwrap();
    storage.create_issue(&in_progress, "tester").unwrap();
    storage.create_issue(&closed, "tester").unwrap();
    storage.create_issue(&deferred, "tester").unwrap();

    let filters = ReadyFilters::default();
    let ids = ready_ids(&mut storage, &filters, ReadySortPolicy::Oldest);

    assert!(ids.contains(&open.id));
    assert!(ids.contains(&in_progress.id));
    assert!(!ids.contains(&closed.id));
    assert!(!ids.contains(&deferred.id));
}

#[test]
fn ready_include_deferred_flag() {
    let mut storage = test_db();

    // Create an issue that has open status but a future defer_until date
    // The ready query excludes issues where defer_until > now (unless include_deferred)
    let open_no_defer = fixtures::IssueBuilder::new("Open no defer")
        .with_status(Status::Open)
        .build();

    let mut open_with_defer = fixtures::IssueBuilder::new("Open with future defer")
        .with_status(Status::Open)
        .build();
    // Set defer_until to a future date
    open_with_defer.defer_until = Some(chrono::Utc::now() + chrono::Duration::days(30));

    storage.create_issue(&open_no_defer, "tester").unwrap();
    storage.create_issue(&open_with_defer, "tester").unwrap();

    // Without include_deferred - deferred should be excluded
    let filters_no_deferred = ReadyFilters {
        include_deferred: false,
        ..Default::default()
    };
    let ids = ready_ids(&mut storage, &filters_no_deferred, ReadySortPolicy::Oldest);
    assert!(ids.contains(&open_no_defer.id));
    // Open issue with future defer_until should be excluded
    assert!(!ids.contains(&open_with_defer.id));

    // With include_deferred - deferred should be included
    let filters_with_deferred = ReadyFilters {
        include_deferred: true,
        ..Default::default()
    };
    let ids = ready_ids(&mut storage, &filters_with_deferred, ReadySortPolicy::Oldest);
    assert!(ids.contains(&open_no_defer.id));
    assert!(ids.contains(&open_with_defer.id));
}
