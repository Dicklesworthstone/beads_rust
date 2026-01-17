mod common;

use beads_rust::model::{DependencyType, Issue, IssueType, Priority, Status};
use beads_rust::storage::{ListFilters, ReadyFilters, ReadySortPolicy};
use chrono::{Duration, Utc};
use common::{fixtures, test_db, test_db_with_dir};
use rusqlite::Connection;
use std::collections::HashSet;

fn table_names(conn: &Connection) -> HashSet<String> {
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .expect("prepare table list");
    stmt.query_map([], |row| row.get(0))
        .expect("query table list")
        .collect::<std::result::Result<HashSet<String>, _>>()
        .expect("collect table list")
}

fn column_names(conn: &Connection, table: &str) -> HashSet<String> {
    let mut stmt = conn
        .prepare("SELECT name FROM pragma_table_info(?)")
        .expect("prepare table info");
    stmt.query_map([table], |row| row.get(0))
        .expect("query table info")
        .collect::<std::result::Result<HashSet<String>, _>>()
        .expect("collect table info")
}

fn issue_ids(issues: &[Issue]) -> HashSet<String> {
    issues.iter().map(|issue| issue.id.clone()).collect()
}

#[test]
fn schema_tables_and_columns_exist() {
    let (storage, dir) = test_db_with_dir();
    let db_path = dir.path().join(".beads").join("beads.db");

    drop(storage);
    let conn = Connection::open(db_path).expect("open db");

    let tables = table_names(&conn);
    for table in [
        "issues",
        "dependencies",
        "labels",
        "comments",
        "events",
        "config",
        "metadata",
        "dirty_issues",
        "export_hashes",
        "blocked_issues_cache",
        "child_counters",
    ] {
        assert!(tables.contains(table), "missing table: {table}");
    }

    let issue_columns = column_names(&conn, "issues");
    for column in [
        "id",
        "content_hash",
        "title",
        "status",
        "priority",
        "issue_type",
        "created_at",
        "updated_at",
    ] {
        assert!(issue_columns.contains(column), "missing issues.{column}");
    }

    let blocked_columns = column_names(&conn, "blocked_issues_cache");
    for column in ["issue_id", "blocked_by_json"] {
        assert!(
            blocked_columns.contains(column),
            "missing blocked_issues_cache.{column}"
        );
    }
}

#[test]
fn label_crud_roundtrip() {
    let mut storage = test_db();
    let issue = fixtures::issue("label-crud");

    storage.create_issue(&issue, "tester").unwrap();
    assert!(storage.add_label(&issue.id, "bug", "tester").unwrap());
    assert!(!storage.add_label(&issue.id, "bug", "tester").unwrap());

    let mut labels = storage.get_labels(&issue.id).unwrap();
    labels.sort();
    assert_eq!(labels, vec!["bug".to_string()]);

    storage
        .set_labels(
            &issue.id,
            &["alpha".to_string(), "beta".to_string()],
            "tester",
        )
        .unwrap();
    let mut labels = storage.get_labels(&issue.id).unwrap();
    labels.sort();
    assert_eq!(labels, vec!["alpha".to_string(), "beta".to_string()]);

    assert!(storage.remove_label(&issue.id, "alpha", "tester").unwrap());
    let labels = storage.get_labels(&issue.id).unwrap();
    assert_eq!(labels, vec!["beta".to_string()]);
}

#[test]
fn dependency_crud_updates_blocked_cache() {
    let mut storage = test_db();
    let blocking_issue = fixtures::issue("blocker");
    let blocked_issue = fixtures::issue("blocked");

    storage.create_issue(&blocking_issue, "tester").unwrap();
    storage.create_issue(&blocked_issue, "tester").unwrap();

    let added = storage
        .add_dependency(
            &blocked_issue.id,
            &blocking_issue.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();
    assert!(added);

    let blocked_ids = storage.get_blocked_ids().unwrap();
    assert!(blocked_ids.contains(&blocked_issue.id));

    let blocked_issues = storage.get_blocked_issues().unwrap();
    let blocked_entry = blocked_issues
        .iter()
        .find(|(issue, _)| issue.id == blocked_issue.id)
        .expect("blocked entry");
    let expected_prefix = format!("{}:", blocking_issue.id);
    assert!(
        blocked_entry
            .1
            .iter()
            .any(|blocker| blocker.starts_with(&expected_prefix))
    );

    let removed = storage
        .remove_dependency(&blocked_issue.id, &blocking_issue.id, "tester")
        .unwrap();
    assert!(removed);
    storage.rebuild_blocked_cache(true).unwrap();

    let blocked_ids = storage.get_blocked_ids().unwrap();
    assert!(!blocked_ids.contains(&blocked_issue.id));
}

#[test]
fn ready_filters_exclude_blocked_and_deferred() {
    let mut storage = test_db();

    let mut ready = fixtures::issue("ready");
    let mut blocked_issue = fixtures::issue("blocked-ready");
    let mut deferred = fixtures::issue("deferred-ready");
    let blocking_issue = fixtures::issue("blocker");

    deferred.defer_until = Some(Utc::now() + Duration::days(1));
    ready.priority = Priority::HIGH;
    blocked_issue.priority = Priority::HIGH;
    deferred.priority = Priority::HIGH;

    storage.create_issue(&ready, "tester").unwrap();
    storage.create_issue(&blocked_issue, "tester").unwrap();
    storage.create_issue(&deferred, "tester").unwrap();
    storage.create_issue(&blocking_issue, "tester").unwrap();

    storage
        .add_dependency(
            &blocked_issue.id,
            &blocking_issue.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();

    for issue_id in [&ready.id, &blocked_issue.id, &deferred.id] {
        storage.add_label(issue_id, "alpha", "tester").unwrap();
    }

    let filters = ReadyFilters {
        labels_and: vec!["alpha".to_string()],
        ..Default::default()
    };
    let ready_issues = storage
        .get_ready_issues(&filters, ReadySortPolicy::Priority)
        .unwrap();
    let ids = issue_ids(&ready_issues);

    assert!(ids.contains(&ready.id));
    assert!(!ids.contains(&blocked_issue.id));
    assert!(!ids.contains(&deferred.id));
}

#[test]
fn list_filters_respect_title_priority_and_closed() {
    let mut storage = test_db();

    let mut open = fixtures::issue("Alpha open");
    open.priority = Priority::HIGH;
    open.issue_type = IssueType::Bug;

    let mut closed = fixtures::issue("Alpha closed");
    closed.priority = Priority::HIGH;
    closed.status = Status::Closed;

    let mut other = fixtures::issue("Beta other");
    other.priority = Priority::LOW;

    storage.create_issue(&open, "tester").unwrap();
    storage.create_issue(&closed, "tester").unwrap();
    storage.create_issue(&other, "tester").unwrap();

    let filters = ListFilters {
        title_contains: Some("Alpha".to_string()),
        priorities: Some(vec![Priority::HIGH]),
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    let ids = issue_ids(&issues);
    assert!(ids.contains(&open.id));
    assert!(!ids.contains(&closed.id));
    assert!(!ids.contains(&other.id));

    let filters = ListFilters {
        title_contains: Some("Alpha".to_string()),
        priorities: Some(vec![Priority::HIGH]),
        include_closed: true,
        limit: Some(1),
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    assert_eq!(issues.len(), 1);
}

// ============================================================================
// List Filters: Comprehensive Test Suite
// Tests for beads_rust-6ug: Storage unit tests: List filters and query combinations
// ============================================================================

#[test]
fn list_filters_status_single() {
    let mut storage = test_db();

    let open = fixtures::IssueBuilder::new("open issue")
        .with_status(Status::Open)
        .build();
    let in_progress = fixtures::IssueBuilder::new("in progress issue")
        .with_status(Status::InProgress)
        .build();
    let closed = fixtures::IssueBuilder::new("closed issue")
        .with_status(Status::Closed)
        .build();
    let deferred = fixtures::IssueBuilder::new("deferred issue")
        .with_status(Status::Deferred)
        .build();

    storage.create_issue(&open, "tester").unwrap();
    storage.create_issue(&in_progress, "tester").unwrap();
    storage.create_issue(&closed, "tester").unwrap();
    storage.create_issue(&deferred, "tester").unwrap();

    // Filter for open only
    let filters = ListFilters {
        statuses: Some(vec![Status::Open]),
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    let ids = issue_ids(&issues);
    assert!(ids.contains(&open.id));
    assert!(!ids.contains(&in_progress.id));
    assert!(!ids.contains(&closed.id));
    assert!(!ids.contains(&deferred.id));
    assert_eq!(issues.len(), 1);

    // Filter for in_progress only
    let filters = ListFilters {
        statuses: Some(vec![Status::InProgress]),
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    assert_eq!(issues.len(), 1);
    assert!(issue_ids(&issues).contains(&in_progress.id));
}

#[test]
fn list_filters_status_multiple() {
    let mut storage = test_db();

    let open = fixtures::IssueBuilder::new("open")
        .with_status(Status::Open)
        .build();
    let in_progress = fixtures::IssueBuilder::new("in_progress")
        .with_status(Status::InProgress)
        .build();
    let blocked = fixtures::IssueBuilder::new("blocked")
        .with_status(Status::Blocked)
        .build();
    let closed = fixtures::IssueBuilder::new("closed")
        .with_status(Status::Closed)
        .build();

    storage.create_issue(&open, "tester").unwrap();
    storage.create_issue(&in_progress, "tester").unwrap();
    storage.create_issue(&blocked, "tester").unwrap();
    storage.create_issue(&closed, "tester").unwrap();

    // Filter for multiple statuses
    let filters = ListFilters {
        statuses: Some(vec![Status::Open, Status::InProgress, Status::Blocked]),
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    let ids = issue_ids(&issues);
    assert!(ids.contains(&open.id));
    assert!(ids.contains(&in_progress.id));
    assert!(ids.contains(&blocked.id));
    assert!(!ids.contains(&closed.id));
    assert_eq!(issues.len(), 3);
}

#[test]
fn list_filters_priority_single() {
    let mut storage = test_db();

    let p0 = fixtures::IssueBuilder::new("critical")
        .with_priority(Priority::CRITICAL)
        .build();
    let p1 = fixtures::IssueBuilder::new("high")
        .with_priority(Priority::HIGH)
        .build();
    let p2 = fixtures::IssueBuilder::new("medium")
        .with_priority(Priority::MEDIUM)
        .build();
    let p3 = fixtures::IssueBuilder::new("low")
        .with_priority(Priority::LOW)
        .build();
    let p4 = fixtures::IssueBuilder::new("backlog")
        .with_priority(Priority::BACKLOG)
        .build();

    storage.create_issue(&p0, "tester").unwrap();
    storage.create_issue(&p1, "tester").unwrap();
    storage.create_issue(&p2, "tester").unwrap();
    storage.create_issue(&p3, "tester").unwrap();
    storage.create_issue(&p4, "tester").unwrap();

    // Filter for P0 (critical) only
    let filters = ListFilters {
        priorities: Some(vec![Priority::CRITICAL]),
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    assert_eq!(issues.len(), 1);
    assert!(issue_ids(&issues).contains(&p0.id));

    // Filter for P4 (backlog) only
    let filters = ListFilters {
        priorities: Some(vec![Priority::BACKLOG]),
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    assert_eq!(issues.len(), 1);
    assert!(issue_ids(&issues).contains(&p4.id));
}

#[test]
fn list_filters_priority_range() {
    let mut storage = test_db();

    let p0 = fixtures::IssueBuilder::new("critical")
        .with_priority(Priority::CRITICAL)
        .build();
    let p1 = fixtures::IssueBuilder::new("high")
        .with_priority(Priority::HIGH)
        .build();
    let p2 = fixtures::IssueBuilder::new("medium")
        .with_priority(Priority::MEDIUM)
        .build();
    let p3 = fixtures::IssueBuilder::new("low")
        .with_priority(Priority::LOW)
        .build();
    let p4 = fixtures::IssueBuilder::new("backlog")
        .with_priority(Priority::BACKLOG)
        .build();

    storage.create_issue(&p0, "tester").unwrap();
    storage.create_issue(&p1, "tester").unwrap();
    storage.create_issue(&p2, "tester").unwrap();
    storage.create_issue(&p3, "tester").unwrap();
    storage.create_issue(&p4, "tester").unwrap();

    // Filter for P0-P1 (critical and high)
    let filters = ListFilters {
        priorities: Some(vec![Priority::CRITICAL, Priority::HIGH]),
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    let ids = issue_ids(&issues);
    assert_eq!(issues.len(), 2);
    assert!(ids.contains(&p0.id));
    assert!(ids.contains(&p1.id));
    assert!(!ids.contains(&p2.id));

    // Filter for P2-P4 (medium, low, backlog)
    let filters = ListFilters {
        priorities: Some(vec![Priority::MEDIUM, Priority::LOW, Priority::BACKLOG]),
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    assert_eq!(issues.len(), 3);
}

#[test]
fn list_filters_type_single() {
    let mut storage = test_db();

    let bug = fixtures::IssueBuilder::new("bug issue")
        .with_type(IssueType::Bug)
        .build();
    let feature = fixtures::IssueBuilder::new("feature issue")
        .with_type(IssueType::Feature)
        .build();
    let task = fixtures::IssueBuilder::new("task issue")
        .with_type(IssueType::Task)
        .build();
    let epic = fixtures::IssueBuilder::new("epic issue")
        .with_type(IssueType::Epic)
        .build();

    storage.create_issue(&bug, "tester").unwrap();
    storage.create_issue(&feature, "tester").unwrap();
    storage.create_issue(&task, "tester").unwrap();
    storage.create_issue(&epic, "tester").unwrap();

    // Filter for bugs only
    let filters = ListFilters {
        types: Some(vec![IssueType::Bug]),
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    assert_eq!(issues.len(), 1);
    assert!(issue_ids(&issues).contains(&bug.id));

    // Filter for features only
    let filters = ListFilters {
        types: Some(vec![IssueType::Feature]),
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    assert_eq!(issues.len(), 1);
    assert!(issue_ids(&issues).contains(&feature.id));
}

#[test]
fn list_filters_type_multiple() {
    let mut storage = test_db();

    let bug = fixtures::IssueBuilder::new("bug")
        .with_type(IssueType::Bug)
        .build();
    let feature = fixtures::IssueBuilder::new("feature")
        .with_type(IssueType::Feature)
        .build();
    let task = fixtures::IssueBuilder::new("task")
        .with_type(IssueType::Task)
        .build();
    let chore = fixtures::IssueBuilder::new("chore")
        .with_type(IssueType::Chore)
        .build();

    storage.create_issue(&bug, "tester").unwrap();
    storage.create_issue(&feature, "tester").unwrap();
    storage.create_issue(&task, "tester").unwrap();
    storage.create_issue(&chore, "tester").unwrap();

    // Filter for bugs and features
    let filters = ListFilters {
        types: Some(vec![IssueType::Bug, IssueType::Feature]),
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    let ids = issue_ids(&issues);
    assert_eq!(issues.len(), 2);
    assert!(ids.contains(&bug.id));
    assert!(ids.contains(&feature.id));
    assert!(!ids.contains(&task.id));
    assert!(!ids.contains(&chore.id));
}

#[test]
fn list_filters_assignee() {
    let mut storage = test_db();

    let alice_issue = fixtures::IssueBuilder::new("alice task")
        .with_assignee("alice")
        .build();
    let bob_issue = fixtures::IssueBuilder::new("bob task")
        .with_assignee("bob")
        .build();
    let unassigned = fixtures::IssueBuilder::new("unassigned task").build();

    storage.create_issue(&alice_issue, "tester").unwrap();
    storage.create_issue(&bob_issue, "tester").unwrap();
    storage.create_issue(&unassigned, "tester").unwrap();

    // Filter for alice's issues
    let filters = ListFilters {
        assignee: Some("alice".to_string()),
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    assert_eq!(issues.len(), 1);
    assert!(issue_ids(&issues).contains(&alice_issue.id));

    // Filter for bob's issues
    let filters = ListFilters {
        assignee: Some("bob".to_string()),
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    assert_eq!(issues.len(), 1);
    assert!(issue_ids(&issues).contains(&bob_issue.id));
}

#[test]
fn list_filters_unassigned() {
    let mut storage = test_db();

    let assigned = fixtures::IssueBuilder::new("assigned task")
        .with_assignee("alice")
        .build();
    let unassigned1 = fixtures::IssueBuilder::new("unassigned task 1").build();
    let unassigned2 = fixtures::IssueBuilder::new("unassigned task 2").build();

    storage.create_issue(&assigned, "tester").unwrap();
    storage.create_issue(&unassigned1, "tester").unwrap();
    storage.create_issue(&unassigned2, "tester").unwrap();

    // Filter for unassigned issues
    let filters = ListFilters {
        unassigned: true,
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    let ids = issue_ids(&issues);
    assert_eq!(issues.len(), 2);
    assert!(!ids.contains(&assigned.id));
    assert!(ids.contains(&unassigned1.id));
    assert!(ids.contains(&unassigned2.id));
}

#[test]
fn list_filters_include_closed() {
    let mut storage = test_db();

    let open = fixtures::IssueBuilder::new("open")
        .with_status(Status::Open)
        .build();
    let closed = fixtures::IssueBuilder::new("closed")
        .with_status(Status::Closed)
        .build();
    let mut tombstone = fixtures::IssueBuilder::new("tombstone").build();
    tombstone.status = Status::Tombstone;

    storage.create_issue(&open, "tester").unwrap();
    storage.create_issue(&closed, "tester").unwrap();
    storage.create_issue(&tombstone, "tester").unwrap();

    // Default: exclude closed
    let filters = ListFilters::default();
    let issues = storage.list_issues(&filters).unwrap();
    let ids = issue_ids(&issues);
    assert!(ids.contains(&open.id));
    assert!(!ids.contains(&closed.id));
    assert!(!ids.contains(&tombstone.id));

    // Include closed
    let filters = ListFilters {
        include_closed: true,
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    let ids = issue_ids(&issues);
    assert!(ids.contains(&open.id));
    assert!(ids.contains(&closed.id));
    assert!(ids.contains(&tombstone.id));
}

#[test]
fn list_filters_include_templates() {
    let mut storage = test_db();

    let regular = fixtures::IssueBuilder::new("regular issue").build();
    let template = fixtures::IssueBuilder::new("template issue")
        .with_template()
        .build();

    storage.create_issue(&regular, "tester").unwrap();
    storage.create_issue(&template, "tester").unwrap();

    // Default: exclude templates
    let filters = ListFilters::default();
    let issues = storage.list_issues(&filters).unwrap();
    let ids = issue_ids(&issues);
    assert!(ids.contains(&regular.id));
    assert!(!ids.contains(&template.id));

    // Include templates
    let filters = ListFilters {
        include_templates: true,
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    let ids = issue_ids(&issues);
    assert!(ids.contains(&regular.id));
    assert!(ids.contains(&template.id));
}

#[test]
fn list_filters_title_contains() {
    let mut storage = test_db();

    let alpha = fixtures::IssueBuilder::new("Alpha task").build();
    let beta = fixtures::IssueBuilder::new("Beta task").build();
    let alpha_beta = fixtures::IssueBuilder::new("Alpha Beta task").build();

    storage.create_issue(&alpha, "tester").unwrap();
    storage.create_issue(&beta, "tester").unwrap();
    storage.create_issue(&alpha_beta, "tester").unwrap();

    // Search for "Alpha"
    let filters = ListFilters {
        title_contains: Some("Alpha".to_string()),
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    let ids = issue_ids(&issues);
    assert_eq!(issues.len(), 2);
    assert!(ids.contains(&alpha.id));
    assert!(ids.contains(&alpha_beta.id));
    assert!(!ids.contains(&beta.id));

    // Search for "Beta"
    let filters = ListFilters {
        title_contains: Some("Beta".to_string()),
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    assert_eq!(issues.len(), 2);
}

#[test]
fn list_filters_limit() {
    let mut storage = test_db();

    for i in 0..10 {
        let issue = fixtures::IssueBuilder::new(&format!("Issue {i}")).build();
        storage.create_issue(&issue, "tester").unwrap();
    }

    // Limit to 3
    let filters = ListFilters {
        limit: Some(3),
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    assert_eq!(issues.len(), 3);

    // Limit to 5
    let filters = ListFilters {
        limit: Some(5),
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    assert_eq!(issues.len(), 5);

    // Limit 0 (should return all)
    let filters = ListFilters {
        limit: Some(0),
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    assert_eq!(issues.len(), 10);

    // No limit
    let filters = ListFilters::default();
    let issues = storage.list_issues(&filters).unwrap();
    assert_eq!(issues.len(), 10);
}

// ============================================================================
// Combined Filter Tests (15+ combinations as per beads_rust-6ug requirements)
// ============================================================================

#[test]
fn list_filters_combined_status_and_priority() {
    let mut storage = test_db();

    let open_high = fixtures::IssueBuilder::new("open high")
        .with_status(Status::Open)
        .with_priority(Priority::HIGH)
        .build();
    let open_low = fixtures::IssueBuilder::new("open low")
        .with_status(Status::Open)
        .with_priority(Priority::LOW)
        .build();
    let closed_high = fixtures::IssueBuilder::new("closed high")
        .with_status(Status::Closed)
        .with_priority(Priority::HIGH)
        .build();

    storage.create_issue(&open_high, "tester").unwrap();
    storage.create_issue(&open_low, "tester").unwrap();
    storage.create_issue(&closed_high, "tester").unwrap();

    // Open AND high priority
    let filters = ListFilters {
        statuses: Some(vec![Status::Open]),
        priorities: Some(vec![Priority::HIGH]),
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    assert_eq!(issues.len(), 1);
    assert!(issue_ids(&issues).contains(&open_high.id));
}

#[test]
fn list_filters_combined_type_and_assignee() {
    let mut storage = test_db();

    let alice_bug = fixtures::IssueBuilder::new("alice bug")
        .with_type(IssueType::Bug)
        .with_assignee("alice")
        .build();
    let alice_feature = fixtures::IssueBuilder::new("alice feature")
        .with_type(IssueType::Feature)
        .with_assignee("alice")
        .build();
    let bob_bug = fixtures::IssueBuilder::new("bob bug")
        .with_type(IssueType::Bug)
        .with_assignee("bob")
        .build();

    storage.create_issue(&alice_bug, "tester").unwrap();
    storage.create_issue(&alice_feature, "tester").unwrap();
    storage.create_issue(&bob_bug, "tester").unwrap();

    // Bugs assigned to alice
    let filters = ListFilters {
        types: Some(vec![IssueType::Bug]),
        assignee: Some("alice".to_string()),
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    assert_eq!(issues.len(), 1);
    assert!(issue_ids(&issues).contains(&alice_bug.id));
}

#[test]
fn list_filters_combined_status_type_priority() {
    let mut storage = test_db();

    let match_issue = fixtures::IssueBuilder::new("matching issue")
        .with_status(Status::InProgress)
        .with_type(IssueType::Bug)
        .with_priority(Priority::CRITICAL)
        .build();
    let wrong_status = fixtures::IssueBuilder::new("wrong status")
        .with_status(Status::Open)
        .with_type(IssueType::Bug)
        .with_priority(Priority::CRITICAL)
        .build();
    let wrong_type = fixtures::IssueBuilder::new("wrong type")
        .with_status(Status::InProgress)
        .with_type(IssueType::Feature)
        .with_priority(Priority::CRITICAL)
        .build();
    let wrong_priority = fixtures::IssueBuilder::new("wrong priority")
        .with_status(Status::InProgress)
        .with_type(IssueType::Bug)
        .with_priority(Priority::LOW)
        .build();

    storage.create_issue(&match_issue, "tester").unwrap();
    storage.create_issue(&wrong_status, "tester").unwrap();
    storage.create_issue(&wrong_type, "tester").unwrap();
    storage.create_issue(&wrong_priority, "tester").unwrap();

    // in_progress AND bug AND critical
    let filters = ListFilters {
        statuses: Some(vec![Status::InProgress]),
        types: Some(vec![IssueType::Bug]),
        priorities: Some(vec![Priority::CRITICAL]),
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    assert_eq!(issues.len(), 1);
    assert!(issue_ids(&issues).contains(&match_issue.id));
}

#[test]
fn list_filters_combined_title_and_type() {
    let mut storage = test_db();

    let api_bug = fixtures::IssueBuilder::new("API bug fix")
        .with_type(IssueType::Bug)
        .build();
    let api_feature = fixtures::IssueBuilder::new("API feature")
        .with_type(IssueType::Feature)
        .build();
    let ui_bug = fixtures::IssueBuilder::new("UI bug fix")
        .with_type(IssueType::Bug)
        .build();

    storage.create_issue(&api_bug, "tester").unwrap();
    storage.create_issue(&api_feature, "tester").unwrap();
    storage.create_issue(&ui_bug, "tester").unwrap();

    // Title contains "API" AND type is bug
    let filters = ListFilters {
        title_contains: Some("API".to_string()),
        types: Some(vec![IssueType::Bug]),
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    assert_eq!(issues.len(), 1);
    assert!(issue_ids(&issues).contains(&api_bug.id));
}

#[test]
fn list_filters_combined_unassigned_and_priority() {
    let mut storage = test_db();

    let unassigned_high = fixtures::IssueBuilder::new("unassigned high")
        .with_priority(Priority::HIGH)
        .build();
    let unassigned_low = fixtures::IssueBuilder::new("unassigned low")
        .with_priority(Priority::LOW)
        .build();
    let assigned_high = fixtures::IssueBuilder::new("assigned high")
        .with_priority(Priority::HIGH)
        .with_assignee("alice")
        .build();

    storage.create_issue(&unassigned_high, "tester").unwrap();
    storage.create_issue(&unassigned_low, "tester").unwrap();
    storage.create_issue(&assigned_high, "tester").unwrap();

    // Unassigned AND high priority
    let filters = ListFilters {
        unassigned: true,
        priorities: Some(vec![Priority::HIGH]),
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    assert_eq!(issues.len(), 1);
    assert!(issue_ids(&issues).contains(&unassigned_high.id));
}

#[test]
fn list_filters_combined_multiple_statuses_and_types() {
    let mut storage = test_db();

    let open_bug = fixtures::IssueBuilder::new("open bug")
        .with_status(Status::Open)
        .with_type(IssueType::Bug)
        .build();
    let progress_bug = fixtures::IssueBuilder::new("progress bug")
        .with_status(Status::InProgress)
        .with_type(IssueType::Bug)
        .build();
    let open_feature = fixtures::IssueBuilder::new("open feature")
        .with_status(Status::Open)
        .with_type(IssueType::Feature)
        .build();
    let closed_bug = fixtures::IssueBuilder::new("closed bug")
        .with_status(Status::Closed)
        .with_type(IssueType::Bug)
        .build();
    let open_task = fixtures::IssueBuilder::new("open task")
        .with_status(Status::Open)
        .with_type(IssueType::Task)
        .build();

    storage.create_issue(&open_bug, "tester").unwrap();
    storage.create_issue(&progress_bug, "tester").unwrap();
    storage.create_issue(&open_feature, "tester").unwrap();
    storage.create_issue(&closed_bug, "tester").unwrap();
    storage.create_issue(&open_task, "tester").unwrap();

    // (open OR in_progress) AND (bug OR feature)
    let filters = ListFilters {
        statuses: Some(vec![Status::Open, Status::InProgress]),
        types: Some(vec![IssueType::Bug, IssueType::Feature]),
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    let ids = issue_ids(&issues);
    assert_eq!(issues.len(), 3);
    assert!(ids.contains(&open_bug.id));
    assert!(ids.contains(&progress_bug.id));
    assert!(ids.contains(&open_feature.id));
    assert!(!ids.contains(&closed_bug.id));
    assert!(!ids.contains(&open_task.id));
}

#[test]
fn list_filters_combined_all_priority_levels() {
    let mut storage = test_db();

    let issues: Vec<_> = (0..=4)
        .map(|p| {
            fixtures::IssueBuilder::new(&format!("priority {p} issue"))
                .with_priority(Priority(p))
                .build()
        })
        .collect();

    for issue in &issues {
        storage.create_issue(issue, "tester").unwrap();
    }

    // Filter for all priority levels explicitly
    let filters = ListFilters {
        priorities: Some(vec![
            Priority::CRITICAL,
            Priority::HIGH,
            Priority::MEDIUM,
            Priority::LOW,
            Priority::BACKLOG,
        ]),
        ..Default::default()
    };
    let result = storage.list_issues(&filters).unwrap();
    assert_eq!(result.len(), 5);
}

#[test]
fn list_filters_combined_with_limit() {
    let mut storage = test_db();

    for i in 0..10 {
        let issue = fixtures::IssueBuilder::new(&format!("Issue {i}"))
            .with_type(IssueType::Bug)
            .with_priority(Priority::HIGH)
            .build();
        storage.create_issue(&issue, "tester").unwrap();
    }

    // Add some non-matching issues
    for i in 0..5 {
        let issue = fixtures::IssueBuilder::new(&format!("Feature {i}"))
            .with_type(IssueType::Feature)
            .with_priority(Priority::LOW)
            .build();
        storage.create_issue(&issue, "tester").unwrap();
    }

    // Filter for bugs with high priority, limit to 5
    let filters = ListFilters {
        types: Some(vec![IssueType::Bug]),
        priorities: Some(vec![Priority::HIGH]),
        limit: Some(5),
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    assert_eq!(issues.len(), 5);

    // Verify they're all bugs with high priority
    for issue in &issues {
        assert_eq!(issue.issue_type, IssueType::Bug);
        assert_eq!(issue.priority, Priority::HIGH);
    }
}

#[test]
fn list_filters_combined_include_closed_with_status_filter() {
    let mut storage = test_db();

    let open = fixtures::IssueBuilder::new("open")
        .with_status(Status::Open)
        .build();
    let closed = fixtures::IssueBuilder::new("closed")
        .with_status(Status::Closed)
        .build();
    let in_progress = fixtures::IssueBuilder::new("in_progress")
        .with_status(Status::InProgress)
        .build();

    storage.create_issue(&open, "tester").unwrap();
    storage.create_issue(&closed, "tester").unwrap();
    storage.create_issue(&in_progress, "tester").unwrap();

    // Status filter for closed with include_closed=true
    let filters = ListFilters {
        statuses: Some(vec![Status::Closed]),
        include_closed: true,
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    assert_eq!(issues.len(), 1);
    assert!(issue_ids(&issues).contains(&closed.id));
}

#[test]
fn list_filters_combined_five_filters() {
    let mut storage = test_db();

    // Create the target issue that matches all filters
    let target = fixtures::IssueBuilder::new("API bug fix")
        .with_status(Status::InProgress)
        .with_type(IssueType::Bug)
        .with_priority(Priority::HIGH)
        .with_assignee("alice")
        .build();

    // Create non-matching issues (each misses one filter)
    let wrong_title = fixtures::IssueBuilder::new("UI bug fix")
        .with_status(Status::InProgress)
        .with_type(IssueType::Bug)
        .with_priority(Priority::HIGH)
        .with_assignee("alice")
        .build();
    let wrong_status = fixtures::IssueBuilder::new("API bug")
        .with_status(Status::Open)
        .with_type(IssueType::Bug)
        .with_priority(Priority::HIGH)
        .with_assignee("alice")
        .build();
    let wrong_type = fixtures::IssueBuilder::new("API task")
        .with_status(Status::InProgress)
        .with_type(IssueType::Task)
        .with_priority(Priority::HIGH)
        .with_assignee("alice")
        .build();
    let wrong_priority = fixtures::IssueBuilder::new("API bug low")
        .with_status(Status::InProgress)
        .with_type(IssueType::Bug)
        .with_priority(Priority::LOW)
        .with_assignee("alice")
        .build();
    let wrong_assignee = fixtures::IssueBuilder::new("API bug bob")
        .with_status(Status::InProgress)
        .with_type(IssueType::Bug)
        .with_priority(Priority::HIGH)
        .with_assignee("bob")
        .build();

    storage.create_issue(&target, "tester").unwrap();
    storage.create_issue(&wrong_title, "tester").unwrap();
    storage.create_issue(&wrong_status, "tester").unwrap();
    storage.create_issue(&wrong_type, "tester").unwrap();
    storage.create_issue(&wrong_priority, "tester").unwrap();
    storage.create_issue(&wrong_assignee, "tester").unwrap();

    // All five filters
    let filters = ListFilters {
        title_contains: Some("API".to_string()),
        statuses: Some(vec![Status::InProgress]),
        types: Some(vec![IssueType::Bug]),
        priorities: Some(vec![Priority::HIGH]),
        assignee: Some("alice".to_string()),
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    assert_eq!(issues.len(), 1);
    assert!(issue_ids(&issues).contains(&target.id));
}

#[test]
fn list_filters_empty_result() {
    let mut storage = test_db();

    let issue = fixtures::IssueBuilder::new("Test issue")
        .with_status(Status::Open)
        .with_type(IssueType::Task)
        .build();
    storage.create_issue(&issue, "tester").unwrap();

    // Filter that matches nothing
    let filters = ListFilters {
        statuses: Some(vec![Status::Blocked]),
        types: Some(vec![IssueType::Epic]),
        ..Default::default()
    };
    let issues = storage.list_issues(&filters).unwrap();
    assert!(issues.is_empty());
}

#[test]
fn list_filters_no_filters_returns_all_open() {
    let mut storage = test_db();

    let open1 = fixtures::IssueBuilder::new("open1")
        .with_status(Status::Open)
        .build();
    let open2 = fixtures::IssueBuilder::new("open2")
        .with_status(Status::InProgress)
        .build();
    let closed = fixtures::IssueBuilder::new("closed")
        .with_status(Status::Closed)
        .build();

    storage.create_issue(&open1, "tester").unwrap();
    storage.create_issue(&open2, "tester").unwrap();
    storage.create_issue(&closed, "tester").unwrap();

    // Default filters (no filters)
    let filters = ListFilters::default();
    let issues = storage.list_issues(&filters).unwrap();
    let ids = issue_ids(&issues);

    // Should return non-closed issues (open, in_progress)
    assert_eq!(issues.len(), 2);
    assert!(ids.contains(&open1.id));
    assert!(ids.contains(&open2.id));
    assert!(!ids.contains(&closed.id));
}

// ============================================================================
// Dependency/Dependent Count Accuracy Tests
// ============================================================================

#[test]
fn list_issues_with_counts_accurate_dependencies() {
    let mut storage = test_db();

    let parent = fixtures::IssueBuilder::new("parent issue").build();
    let child1 = fixtures::IssueBuilder::new("child 1").build();
    let child2 = fixtures::IssueBuilder::new("child 2").build();
    let grandchild = fixtures::IssueBuilder::new("grandchild").build();

    storage.create_issue(&parent, "tester").unwrap();
    storage.create_issue(&child1, "tester").unwrap();
    storage.create_issue(&child2, "tester").unwrap();
    storage.create_issue(&grandchild, "tester").unwrap();

    // parent blocks child1 and child2
    storage
        .add_dependency(&child1.id, &parent.id, "blocks", "tester")
        .unwrap();
    storage
        .add_dependency(&child2.id, &parent.id, "blocks", "tester")
        .unwrap();
    // child1 blocks grandchild
    storage
        .add_dependency(&grandchild.id, &child1.id, "blocks", "tester")
        .unwrap();

    // Verify dependency counts via count helpers
    assert_eq!(storage.count_dependents(&parent.id).unwrap(), 2); // child1, child2 depend on parent
    assert_eq!(storage.count_dependencies(&parent.id).unwrap(), 0); // parent has no dependencies

    assert_eq!(storage.count_dependents(&child1.id).unwrap(), 1); // grandchild depends on child1
    assert_eq!(storage.count_dependencies(&child1.id).unwrap(), 1); // child1 depends on parent

    assert_eq!(storage.count_dependents(&grandchild.id).unwrap(), 0); // nothing depends on grandchild
    assert_eq!(storage.count_dependencies(&grandchild.id).unwrap(), 1); // grandchild depends on child1
}

#[test]
fn find_by_content_hash_roundtrip() {
    let mut storage = test_db();
    let mut issue = fixtures::issue("hash-lookup");
    issue.content_hash = Some("hash-abc123".to_string());

    storage.upsert_issue_for_import(&issue).unwrap();

    let found = storage
        .find_by_content_hash("hash-abc123")
        .unwrap()
        .expect("content hash");
    assert_eq!(found.id, issue.id);
}
