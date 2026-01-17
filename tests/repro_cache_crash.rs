use beads_rust::model::{Issue, IssueType, Priority, Status};
use beads_rust::storage::SqliteStorage;
use chrono::Utc;

fn make_issue(id: &str, title: &str) -> Issue {
    Issue {
        id: id.to_string(),
        title: title.to_string(),
        status: Status::Open,
        priority: Priority::MEDIUM,
        issue_type: IssueType::Task,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        content_hash: None,
        description: None,
        design: None,
        acceptance_criteria: None,
        notes: None,
        assignee: None,
        owner: None,
        estimated_minutes: None,
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
    }
}

#[test]
fn test_rebuild_blocked_cache_crash_with_multiple_parents() {
    let mut storage = SqliteStorage::open_memory().unwrap();

    // Create blockers D and E (open status)
    storage
        .create_issue(&make_issue("bd-D", "Blocker D"), "test")
        .unwrap();
    storage
        .create_issue(&make_issue("bd-E", "Blocker E"), "test")
        .unwrap();

    // Create parents B and C
    storage
        .create_issue(&make_issue("bd-B", "Parent B"), "test")
        .unwrap();
    storage
        .create_issue(&make_issue("bd-C", "Parent C"), "test")
        .unwrap();

    // Create child A
    storage
        .create_issue(&make_issue("bd-A", "Child A"), "test")
        .unwrap();

    // Make B blocked by D
    storage
        .add_dependency("bd-B", "bd-D", "blocks", "test")
        .unwrap();

    // Make C blocked by E
    storage
        .add_dependency("bd-C", "bd-E", "blocks", "test")
        .unwrap();

    // Make A child of B AND C (diamond dependency / multiple inheritance)
    storage
        .add_dependency("bd-A", "bd-B", "parent-child", "test")
        .unwrap();
    storage
        .add_dependency("bd-A", "bd-C", "parent-child", "test")
        .unwrap();

    // Rebuild cache
    // This should initially calculate B and C as blocked.
    // Then transitive pass should find A is blocked by B, AND A is blocked by C.
    // If logic is buggy, it will try to insert A twice into blocked_issues_cache.
    storage
        .rebuild_blocked_cache(true)
        .expect("Should not crash on multiple blocked parents");

    assert!(storage.is_blocked("bd-A").unwrap(), "A should be blocked");
    println!("Test finished successfully");
}
