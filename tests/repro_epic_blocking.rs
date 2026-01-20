use beads_rust::model::{Issue, IssueType, Priority, Status};
use beads_rust::storage::SqliteStorage;
use chrono::Utc;

fn create_issue(id: &str, title: &str, issue_type: IssueType) -> Issue {
    Issue {
        id: id.to_string(),
        content_hash: None,
        title: title.to_string(),
        description: None,
        design: None,
        acceptance_criteria: None,
        notes: None,
        status: Status::Open,
        priority: Priority::MEDIUM,
        issue_type,
        assignee: None,
        owner: None,
        estimated_minutes: None,
        created_at: Utc::now(),
        created_by: None,
        updated_at: Utc::now(),
        closed_at: None,
        close_reason: None,
        closed_by_session: None,
        due_at: None,
        defer_until: None,
        external_ref: None,
        source_system: None,
        source_repo: None,
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
fn test_epic_does_not_block_task() {
    let mut storage = SqliteStorage::open_memory().unwrap();

    let epic = create_issue("bd-epic", "Epic", IssueType::Epic);
    storage.create_issue(&epic, "user").unwrap();

    let task = create_issue("bd-task", "Task", IssueType::Task);
    storage.create_issue(&task, "user").unwrap();

    // Task depends on Epic (parent-child)
    storage
        .add_dependency("bd-task", "bd-epic", "parent-child", "user")
        .unwrap();

    // Rebuild cache
    storage.rebuild_blocked_cache(true).unwrap();

    // Task should NOT be blocked just because Epic is Open
    assert!(
        !storage.is_blocked("bd-task").unwrap(),
        "Task should not be blocked by Open Epic"
    );
}

#[test]
fn test_epic_blocking_propagates_to_task() {
    let mut storage = SqliteStorage::open_memory().unwrap();

    let epic = create_issue("bd-epic", "Epic", IssueType::Epic);
    storage.create_issue(&epic, "user").unwrap();

    let task = create_issue("bd-task", "Task", IssueType::Task);
    storage.create_issue(&task, "user").unwrap();

    let blocker = create_issue("bd-blocker", "Blocker", IssueType::Bug);
    storage.create_issue(&blocker, "user").unwrap();

    // Task depends on Epic (parent-child)
    storage
        .add_dependency("bd-task", "bd-epic", "parent-child", "user")
        .unwrap();

    // Epic depends on Blocker (blocks)
    storage
        .add_dependency("bd-epic", "bd-blocker", "blocks", "user")
        .unwrap();

    // Rebuild cache
    storage.rebuild_blocked_cache(true).unwrap();

    // Epic should be blocked
    assert!(
        storage.is_blocked("bd-epic").unwrap(),
        "Epic should be blocked by Blocker"
    );

    // Task should be blocked (transitively)
    assert!(
        storage.is_blocked("bd-task").unwrap(),
        "Task should be blocked by blocked Epic"
    );
}
