//! Storage list filter unit tests with real `SQLite` (no mocks).
//!
//! Tests `list_issues` with comprehensive filter combinations including:
//! - Status filters (open, closed, `in_progress`)
//! - Priority filters (P0-P4)
//! - Type filters (bug, feature, task)
//! - Assignee/unassigned filters
//! - Title contains filter
//! - Limit filter
//! - Include closed filter
//! - Include templates filter
//! - Combined filter tests
#![allow(clippy::similar_names)]

mod common;

use beads_rust::model::{IssueType, Priority, Status};
use beads_rust::storage::ListFilters;
use common::{fixtures::IssueBuilder, test_db};

// ============================================================================
// STATUS FILTER TESTS
// ============================================================================

#[test]
fn filter_by_single_status_open() {
    let mut storage = test_db();

    // Create issues with different statuses
    let open_issue = IssueBuilder::new("open-issue")
        .with_status(Status::Open)
        .build();
    let in_progress_issue = IssueBuilder::new("in-progress-issue")
        .with_status(Status::InProgress)
        .build();
    let closed_issue = IssueBuilder::new("closed-issue")
        .with_status(Status::Closed)
        .build();

    storage.create_issue(&open_issue, "tester").unwrap();
    storage.create_issue(&in_progress_issue, "tester").unwrap();
    storage.create_issue(&closed_issue, "tester").unwrap();

    let filters = ListFilters {
        statuses: Some(vec![Status::Open]),
        include_closed: true, // Need to include closed to test status filter independently
        ..Default::default()
    };

    let results = storage.list_issues(&filters).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, open_issue.id);
    assert_eq!(results[0].status, Status::Open);
}

#[test]
fn filter_by_single_status_in_progress() {
    let mut storage = test_db();

    let open_issue = IssueBuilder::new("open-issue-2")
        .with_status(Status::Open)
        .build();
    let in_progress_issue = IssueBuilder::new("in-progress-issue-2")
        .with_status(Status::InProgress)
        .build();

    storage.create_issue(&open_issue, "tester").unwrap();
    storage.create_issue(&in_progress_issue, "tester").unwrap();

    let filters = ListFilters {
        statuses: Some(vec![Status::InProgress]),
        ..Default::default()
    };

    let results = storage.list_issues(&filters).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, in_progress_issue.id);
    assert_eq!(results[0].status, Status::InProgress);
}

#[test]
fn filter_by_multiple_statuses() {
    let mut storage = test_db();

    let open_issue = IssueBuilder::new("multi-status-open")
        .with_status(Status::Open)
        .build();
    let in_progress_issue = IssueBuilder::new("multi-status-inprog")
        .with_status(Status::InProgress)
        .build();
    let blocked_issue = IssueBuilder::new("multi-status-blocked")
        .with_status(Status::Blocked)
        .build();

    storage.create_issue(&open_issue, "tester").unwrap();
    storage.create_issue(&in_progress_issue, "tester").unwrap();
    storage.create_issue(&blocked_issue, "tester").unwrap();

    let filters = ListFilters {
        statuses: Some(vec![Status::Open, Status::InProgress]),
        ..Default::default()
    };

    let results = storage.list_issues(&filters).unwrap();
    assert_eq!(results.len(), 2);

    let ids: Vec<_> = results.iter().map(|i| i.id.as_str()).collect();
    assert!(ids.contains(&open_issue.id.as_str()));
    assert!(ids.contains(&in_progress_issue.id.as_str()));
    assert!(!ids.contains(&blocked_issue.id.as_str()));
}

#[test]
fn filter_by_closed_status_requires_include_closed() {
    let mut storage = test_db();

    let open_issue = IssueBuilder::new("closed-test-open")
        .with_status(Status::Open)
        .build();
    let closed_issue = IssueBuilder::new("closed-test-closed")
        .with_status(Status::Closed)
        .build();

    storage.create_issue(&open_issue, "tester").unwrap();
    storage.create_issue(&closed_issue, "tester").unwrap();

    // Without include_closed, closed issues are excluded even if in statuses filter
    let filters_no_include = ListFilters {
        statuses: Some(vec![Status::Closed]),
        include_closed: false,
        ..Default::default()
    };

    let results = storage.list_issues(&filters_no_include).unwrap();
    assert_eq!(results.len(), 0);

    // With include_closed, we can filter to just closed
    let filters_with_include = ListFilters {
        statuses: Some(vec![Status::Closed]),
        include_closed: true,
        ..Default::default()
    };

    let results = storage.list_issues(&filters_with_include).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, closed_issue.id);
}

// ============================================================================
// PRIORITY FILTER TESTS
// ============================================================================

#[test]
fn filter_by_single_priority_critical() {
    let mut storage = test_db();

    let critical_issue = IssueBuilder::new("priority-critical")
        .with_priority(Priority::CRITICAL)
        .build();
    let high_issue = IssueBuilder::new("priority-high")
        .with_priority(Priority::HIGH)
        .build();
    let medium_issue = IssueBuilder::new("priority-medium")
        .with_priority(Priority::MEDIUM)
        .build();

    storage.create_issue(&critical_issue, "tester").unwrap();
    storage.create_issue(&high_issue, "tester").unwrap();
    storage.create_issue(&medium_issue, "tester").unwrap();

    let filters = ListFilters {
        priorities: Some(vec![Priority::CRITICAL]),
        ..Default::default()
    };

    let results = storage.list_issues(&filters).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, critical_issue.id);
    assert_eq!(results[0].priority, Priority::CRITICAL);
}

#[test]
fn filter_by_multiple_priorities() {
    let mut storage = test_db();

    let p0 = IssueBuilder::new("multi-prio-p0")
        .with_priority(Priority::CRITICAL)
        .build();
    let p1 = IssueBuilder::new("multi-prio-p1")
        .with_priority(Priority::HIGH)
        .build();
    let p2 = IssueBuilder::new("multi-prio-p2")
        .with_priority(Priority::MEDIUM)
        .build();
    let p3 = IssueBuilder::new("multi-prio-p3")
        .with_priority(Priority::LOW)
        .build();
    let p4 = IssueBuilder::new("multi-prio-p4")
        .with_priority(Priority::BACKLOG)
        .build();

    storage.create_issue(&p0, "tester").unwrap();
    storage.create_issue(&p1, "tester").unwrap();
    storage.create_issue(&p2, "tester").unwrap();
    storage.create_issue(&p3, "tester").unwrap();
    storage.create_issue(&p4, "tester").unwrap();

    // Filter for high-priority items (P0 and P1)
    let filters = ListFilters {
        priorities: Some(vec![Priority::CRITICAL, Priority::HIGH]),
        ..Default::default()
    };

    let results = storage.list_issues(&filters).unwrap();
    assert_eq!(results.len(), 2);

    let ids: Vec<_> = results.iter().map(|i| i.id.as_str()).collect();
    assert!(ids.contains(&p0.id.as_str()));
    assert!(ids.contains(&p1.id.as_str()));
}

#[test]
fn filter_by_all_priority_levels() {
    let mut storage = test_db();

    let p0 = IssueBuilder::new("all-prio-p0")
        .with_priority(Priority::CRITICAL)
        .build();
    let p4 = IssueBuilder::new("all-prio-p4")
        .with_priority(Priority::BACKLOG)
        .build();

    storage.create_issue(&p0, "tester").unwrap();
    storage.create_issue(&p4, "tester").unwrap();

    // All priority levels should work
    for prio in [
        Priority::CRITICAL,
        Priority::HIGH,
        Priority::MEDIUM,
        Priority::LOW,
        Priority::BACKLOG,
    ] {
        let filters = ListFilters {
            priorities: Some(vec![prio]),
            ..Default::default()
        };
        // Just verify no error - not all priorities have issues
        let _ = storage.list_issues(&filters).unwrap();
    }
}

// ============================================================================
// TYPE FILTER TESTS
// ============================================================================

#[test]
fn filter_by_single_type_bug() {
    let mut storage = test_db();

    let bug_issue = IssueBuilder::new("type-bug")
        .with_type(IssueType::Bug)
        .build();
    let feature_issue = IssueBuilder::new("type-feature")
        .with_type(IssueType::Feature)
        .build();
    let task_issue = IssueBuilder::new("type-task")
        .with_type(IssueType::Task)
        .build();

    storage.create_issue(&bug_issue, "tester").unwrap();
    storage.create_issue(&feature_issue, "tester").unwrap();
    storage.create_issue(&task_issue, "tester").unwrap();

    let filters = ListFilters {
        types: Some(vec![IssueType::Bug]),
        ..Default::default()
    };

    let results = storage.list_issues(&filters).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, bug_issue.id);
    assert_eq!(results[0].issue_type, IssueType::Bug);
}

#[test]
fn filter_by_multiple_types() {
    let mut storage = test_db();

    let bug = IssueBuilder::new("multi-type-bug")
        .with_type(IssueType::Bug)
        .build();
    let feature = IssueBuilder::new("multi-type-feature")
        .with_type(IssueType::Feature)
        .build();
    let task = IssueBuilder::new("multi-type-task")
        .with_type(IssueType::Task)
        .build();
    let epic = IssueBuilder::new("multi-type-epic")
        .with_type(IssueType::Epic)
        .build();

    storage.create_issue(&bug, "tester").unwrap();
    storage.create_issue(&feature, "tester").unwrap();
    storage.create_issue(&task, "tester").unwrap();
    storage.create_issue(&epic, "tester").unwrap();

    let filters = ListFilters {
        types: Some(vec![IssueType::Bug, IssueType::Feature]),
        ..Default::default()
    };

    let results = storage.list_issues(&filters).unwrap();
    assert_eq!(results.len(), 2);

    let ids: Vec<_> = results.iter().map(|i| i.id.as_str()).collect();
    assert!(ids.contains(&bug.id.as_str()));
    assert!(ids.contains(&feature.id.as_str()));
}

#[test]
fn filter_by_all_issue_types() {
    let mut storage = test_db();

    let task = IssueBuilder::new("all-types-task")
        .with_type(IssueType::Task)
        .build();
    storage.create_issue(&task, "tester").unwrap();

    // All issue types should work without error
    for issue_type in [
        IssueType::Task,
        IssueType::Bug,
        IssueType::Feature,
        IssueType::Epic,
        IssueType::Chore,
        IssueType::Question,
        IssueType::Docs,
    ] {
        let filters = ListFilters {
            types: Some(vec![issue_type]),
            ..Default::default()
        };
        let _ = storage.list_issues(&filters).unwrap();
    }
}

// ============================================================================
// ASSIGNEE FILTER TESTS
// ============================================================================

#[test]
fn filter_by_specific_assignee() {
    let mut storage = test_db();

    let alice_issue = IssueBuilder::new("assignee-alice")
        .with_assignee("alice")
        .build();
    let bob_issue = IssueBuilder::new("assignee-bob")
        .with_assignee("bob")
        .build();
    let unassigned_issue = IssueBuilder::new("assignee-none").build();

    storage.create_issue(&alice_issue, "tester").unwrap();
    storage.create_issue(&bob_issue, "tester").unwrap();
    storage.create_issue(&unassigned_issue, "tester").unwrap();

    let filters = ListFilters {
        assignee: Some("alice".to_string()),
        ..Default::default()
    };

    let results = storage.list_issues(&filters).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, alice_issue.id);
    assert_eq!(results[0].assignee, Some("alice".to_string()));
}

#[test]
fn filter_by_unassigned() {
    let mut storage = test_db();

    let assigned_issue = IssueBuilder::new("unassigned-test-assigned")
        .with_assignee("someone")
        .build();
    let unassigned_issue1 = IssueBuilder::new("unassigned-test-1").build();
    let unassigned_issue2 = IssueBuilder::new("unassigned-test-2").build();

    storage.create_issue(&assigned_issue, "tester").unwrap();
    storage.create_issue(&unassigned_issue1, "tester").unwrap();
    storage.create_issue(&unassigned_issue2, "tester").unwrap();

    let filters = ListFilters {
        unassigned: true,
        ..Default::default()
    };

    let results = storage.list_issues(&filters).unwrap();
    assert_eq!(results.len(), 2);

    for issue in &results {
        assert!(issue.assignee.is_none());
    }
}

#[test]
fn assignee_filter_is_case_sensitive() {
    let mut storage = test_db();

    let issue = IssueBuilder::new("case-sensitive-assignee")
        .with_assignee("Alice")
        .build();
    storage.create_issue(&issue, "tester").unwrap();

    // Lowercase should not match
    let filters_lowercase = ListFilters {
        assignee: Some("alice".to_string()),
        ..Default::default()
    };
    let results = storage.list_issues(&filters_lowercase).unwrap();
    assert_eq!(results.len(), 0);

    // Exact case should match
    let filters_exact = ListFilters {
        assignee: Some("Alice".to_string()),
        ..Default::default()
    };
    let results = storage.list_issues(&filters_exact).unwrap();
    assert_eq!(results.len(), 1);
}

// ============================================================================
// TITLE CONTAINS FILTER TESTS
// ============================================================================

#[test]
fn filter_by_title_contains() {
    let mut storage = test_db();

    let api_issue = IssueBuilder::new("Implement API endpoint").build();
    let db_issue = IssueBuilder::new("Database migration").build();
    let api_db_issue = IssueBuilder::new("API database connection").build();

    storage.create_issue(&api_issue, "tester").unwrap();
    storage.create_issue(&db_issue, "tester").unwrap();
    storage.create_issue(&api_db_issue, "tester").unwrap();

    let filters = ListFilters {
        title_contains: Some("API".to_string()),
        ..Default::default()
    };

    let results = storage.list_issues(&filters).unwrap();
    assert_eq!(results.len(), 2);

    let titles: Vec<_> = results.iter().map(|i| i.title.as_str()).collect();
    assert!(titles.contains(&"Implement API endpoint"));
    assert!(titles.contains(&"API database connection"));
}

#[test]
fn filter_by_title_contains_case_insensitive() {
    let mut storage = test_db();

    let issue1 = IssueBuilder::new("API endpoint").build();
    let issue2 = IssueBuilder::new("api client").build();
    let issue3 = IssueBuilder::new("Other task").build();

    storage.create_issue(&issue1, "tester").unwrap();
    storage.create_issue(&issue2, "tester").unwrap();
    storage.create_issue(&issue3, "tester").unwrap();

    // SQLite LIKE is case-insensitive by default
    let filters = ListFilters {
        title_contains: Some("api".to_string()),
        ..Default::default()
    };

    let results = storage.list_issues(&filters).unwrap();
    assert_eq!(results.len(), 2);
}

#[test]
fn filter_by_title_contains_partial_match() {
    let mut storage = test_db();

    let issue = IssueBuilder::new("Authentication system").build();
    storage.create_issue(&issue, "tester").unwrap();

    // Partial match should work
    let filters = ListFilters {
        title_contains: Some("Auth".to_string()),
        ..Default::default()
    };

    let results = storage.list_issues(&filters).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].title, "Authentication system");
}

// ============================================================================
// LIMIT FILTER TESTS
// ============================================================================

#[test]
fn filter_with_limit() {
    let mut storage = test_db();

    // Create 5 issues
    for i in 0..5 {
        let issue = IssueBuilder::new(&format!("limit-test-{i}")).build();
        storage.create_issue(&issue, "tester").unwrap();
    }

    let filters = ListFilters {
        limit: Some(3),
        ..Default::default()
    };

    let results = storage.list_issues(&filters).unwrap();
    assert_eq!(results.len(), 3);
}

#[test]
fn filter_with_limit_zero_returns_all() {
    let mut storage = test_db();

    for i in 0..3 {
        let issue = IssueBuilder::new(&format!("limit-zero-{i}")).build();
        storage.create_issue(&issue, "tester").unwrap();
    }

    // Limit of 0 should return all (per implementation)
    let filters = ListFilters {
        limit: Some(0),
        ..Default::default()
    };

    let results = storage.list_issues(&filters).unwrap();
    assert_eq!(results.len(), 3);
}

#[test]
fn filter_with_limit_larger_than_result_count() {
    let mut storage = test_db();

    for i in 0..3 {
        let issue = IssueBuilder::new(&format!("limit-large-{i}")).build();
        storage.create_issue(&issue, "tester").unwrap();
    }

    let filters = ListFilters {
        limit: Some(100),
        ..Default::default()
    };

    let results = storage.list_issues(&filters).unwrap();
    assert_eq!(results.len(), 3);
}

// ============================================================================
// INCLUDE_CLOSED FILTER TESTS
// ============================================================================

#[test]
fn default_excludes_closed_issues() {
    let mut storage = test_db();

    let open_issue = IssueBuilder::new("include-closed-open")
        .with_status(Status::Open)
        .build();
    let closed_issue = IssueBuilder::new("include-closed-closed")
        .with_status(Status::Closed)
        .build();

    storage.create_issue(&open_issue, "tester").unwrap();
    storage.create_issue(&closed_issue, "tester").unwrap();

    // Default filters (include_closed = false)
    let filters = ListFilters::default();

    let results = storage.list_issues(&filters).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, open_issue.id);
}

#[test]
fn include_closed_returns_all() {
    let mut storage = test_db();

    let open_issue = IssueBuilder::new("include-all-open")
        .with_status(Status::Open)
        .build();
    let closed_issue = IssueBuilder::new("include-all-closed")
        .with_status(Status::Closed)
        .build();

    storage.create_issue(&open_issue, "tester").unwrap();
    storage.create_issue(&closed_issue, "tester").unwrap();

    let filters = ListFilters {
        include_closed: true,
        ..Default::default()
    };

    let results = storage.list_issues(&filters).unwrap();
    assert_eq!(results.len(), 2);
}

#[test]
fn tombstone_excluded_by_default() {
    let mut storage = test_db();

    let active_issue = IssueBuilder::new("tombstone-active")
        .with_status(Status::Open)
        .build();
    let deleted_issue = IssueBuilder::new("tombstone-deleted")
        .with_status(Status::Open)
        .build();

    storage.create_issue(&active_issue, "tester").unwrap();
    storage.create_issue(&deleted_issue, "tester").unwrap();

    // Delete to create tombstone
    storage
        .delete_issue(&deleted_issue.id, "deleter", "test cleanup")
        .unwrap();

    let filters = ListFilters::default();
    let results = storage.list_issues(&filters).unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, active_issue.id);
}

// ============================================================================
// INCLUDE_TEMPLATES FILTER TESTS
// ============================================================================

#[test]
fn default_excludes_templates() {
    let mut storage = test_db();

    let regular_issue = IssueBuilder::new("template-regular").build();
    let template_issue = IssueBuilder::new("template-template")
        .with_template()
        .build();

    storage.create_issue(&regular_issue, "tester").unwrap();
    storage.create_issue(&template_issue, "tester").unwrap();

    // Default filters (include_templates = false)
    let filters = ListFilters::default();

    let results = storage.list_issues(&filters).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, regular_issue.id);
}

#[test]
fn include_templates_returns_all() {
    let mut storage = test_db();

    let regular_issue = IssueBuilder::new("include-tpl-regular").build();
    let template_issue = IssueBuilder::new("include-tpl-template")
        .with_template()
        .build();

    storage.create_issue(&regular_issue, "tester").unwrap();
    storage.create_issue(&template_issue, "tester").unwrap();

    let filters = ListFilters {
        include_templates: true,
        ..Default::default()
    };

    let results = storage.list_issues(&filters).unwrap();
    assert_eq!(results.len(), 2);
}

// ============================================================================
// COMBINED FILTER TESTS
// ============================================================================

#[test]
fn combined_status_and_priority_filters() {
    let mut storage = test_db();

    let open_p0 = IssueBuilder::new("combined-open-p0")
        .with_status(Status::Open)
        .with_priority(Priority::CRITICAL)
        .build();
    let open_p2 = IssueBuilder::new("combined-open-p2")
        .with_status(Status::Open)
        .with_priority(Priority::MEDIUM)
        .build();
    let inprog_p0 = IssueBuilder::new("combined-inprog-p0")
        .with_status(Status::InProgress)
        .with_priority(Priority::CRITICAL)
        .build();

    storage.create_issue(&open_p0, "tester").unwrap();
    storage.create_issue(&open_p2, "tester").unwrap();
    storage.create_issue(&inprog_p0, "tester").unwrap();

    // Filter for open AND critical
    let filters = ListFilters {
        statuses: Some(vec![Status::Open]),
        priorities: Some(vec![Priority::CRITICAL]),
        ..Default::default()
    };

    let results = storage.list_issues(&filters).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, open_p0.id);
}

#[test]
fn combined_type_and_assignee_filters() {
    let mut storage = test_db();

    let bug_alice = IssueBuilder::new("combined-bug-alice")
        .with_type(IssueType::Bug)
        .with_assignee("alice")
        .build();
    let bug_bob = IssueBuilder::new("combined-bug-bob")
        .with_type(IssueType::Bug)
        .with_assignee("bob")
        .build();
    let feature_alice = IssueBuilder::new("combined-feature-alice")
        .with_type(IssueType::Feature)
        .with_assignee("alice")
        .build();

    storage.create_issue(&bug_alice, "tester").unwrap();
    storage.create_issue(&bug_bob, "tester").unwrap();
    storage.create_issue(&feature_alice, "tester").unwrap();

    // Filter for bugs assigned to alice
    let filters = ListFilters {
        types: Some(vec![IssueType::Bug]),
        assignee: Some("alice".to_string()),
        ..Default::default()
    };

    let results = storage.list_issues(&filters).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, bug_alice.id);
}

#[test]
fn combined_title_and_limit_filters() {
    let mut storage = test_db();

    // Create multiple API-related issues
    for i in 0..5 {
        let issue = IssueBuilder::new(&format!("API endpoint {i}")).build();
        storage.create_issue(&issue, "tester").unwrap();
    }

    // Also create non-API issues
    for i in 0..3 {
        let issue = IssueBuilder::new(&format!("Database task {i}")).build();
        storage.create_issue(&issue, "tester").unwrap();
    }

    // Filter for API issues with limit
    let filters = ListFilters {
        title_contains: Some("API".to_string()),
        limit: Some(3),
        ..Default::default()
    };

    let results = storage.list_issues(&filters).unwrap();
    assert_eq!(results.len(), 3);

    // All should have API in title
    for issue in &results {
        assert!(issue.title.contains("API"));
    }
}

#[test]
fn combined_multiple_statuses_types_priorities() {
    let mut storage = test_db();

    // Create a variety of issues
    let issues = vec![
        IssueBuilder::new("multi-combo-1")
            .with_status(Status::Open)
            .with_type(IssueType::Bug)
            .with_priority(Priority::HIGH)
            .build(),
        IssueBuilder::new("multi-combo-2")
            .with_status(Status::InProgress)
            .with_type(IssueType::Feature)
            .with_priority(Priority::MEDIUM)
            .build(),
        IssueBuilder::new("multi-combo-3")
            .with_status(Status::Open)
            .with_type(IssueType::Task)
            .with_priority(Priority::CRITICAL)
            .build(),
        IssueBuilder::new("multi-combo-4")
            .with_status(Status::Blocked)
            .with_type(IssueType::Bug)
            .with_priority(Priority::HIGH)
            .build(),
    ];

    for issue in &issues {
        storage.create_issue(issue, "tester").unwrap();
    }

    // Complex filter: (Open OR InProgress) AND (Bug OR Feature) AND (Critical OR High)
    let filters = ListFilters {
        statuses: Some(vec![Status::Open, Status::InProgress]),
        types: Some(vec![IssueType::Bug, IssueType::Feature]),
        priorities: Some(vec![Priority::CRITICAL, Priority::HIGH]),
        ..Default::default()
    };

    let results = storage.list_issues(&filters).unwrap();

    // Should only match issue 1 (open, bug, high)
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].title, "multi-combo-1");
}

// ============================================================================
// SORT ORDER TESTS
// ============================================================================

#[test]
fn results_sorted_by_priority_then_created_at() {
    let mut storage = test_db();

    // Create issues with different priorities (in non-priority order)
    let low = IssueBuilder::new("sort-low")
        .with_priority(Priority::LOW)
        .build();
    let critical = IssueBuilder::new("sort-critical")
        .with_priority(Priority::CRITICAL)
        .build();
    let medium = IssueBuilder::new("sort-medium")
        .with_priority(Priority::MEDIUM)
        .build();

    storage.create_issue(&low, "tester").unwrap();
    storage.create_issue(&critical, "tester").unwrap();
    storage.create_issue(&medium, "tester").unwrap();

    let filters = ListFilters::default();
    let results = storage.list_issues(&filters).unwrap();

    // Should be sorted by priority (P0, P2, P3 for our created issues)
    assert_eq!(results[0].priority, Priority::CRITICAL); // P0
    assert_eq!(results[1].priority, Priority::MEDIUM); // P2
    assert_eq!(results[2].priority, Priority::LOW); // P3
}

// ============================================================================
// EDGE CASE TESTS
// ============================================================================

#[test]
fn empty_filters_returns_all_non_closed() {
    let mut storage = test_db();

    let open = IssueBuilder::new("empty-filter-open")
        .with_status(Status::Open)
        .build();
    let closed = IssueBuilder::new("empty-filter-closed")
        .with_status(Status::Closed)
        .build();

    storage.create_issue(&open, "tester").unwrap();
    storage.create_issue(&closed, "tester").unwrap();

    let filters = ListFilters::default();
    let results = storage.list_issues(&filters).unwrap();

    // Default excludes closed
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, open.id);
}

#[test]
fn empty_status_vec_returns_all() {
    let mut storage = test_db();

    let issue = IssueBuilder::new("empty-vec-test").build();
    storage.create_issue(&issue, "tester").unwrap();

    let filters = ListFilters {
        statuses: Some(vec![]),
        ..Default::default()
    };

    let results = storage.list_issues(&filters).unwrap();
    // Empty vec in filter should not restrict results
    assert_eq!(results.len(), 1);
}

#[test]
fn filter_no_matches_returns_empty() {
    let mut storage = test_db();

    let issue = IssueBuilder::new("no-match-test")
        .with_priority(Priority::MEDIUM)
        .build();
    storage.create_issue(&issue, "tester").unwrap();

    let filters = ListFilters {
        priorities: Some(vec![Priority::CRITICAL]),
        ..Default::default()
    };

    let results = storage.list_issues(&filters).unwrap();
    assert!(results.is_empty());
}

#[test]
fn filter_with_special_characters_in_title() {
    let mut storage = test_db();

    let issue = IssueBuilder::new("Test % wildcard _ underscore").build();
    storage.create_issue(&issue, "tester").unwrap();

    // SQL LIKE special characters should be handled
    let filters = ListFilters {
        title_contains: Some("%".to_string()),
        ..Default::default()
    };

    let results = storage.list_issues(&filters).unwrap();
    // Should match because title contains literal %
    assert_eq!(results.len(), 1);
}
