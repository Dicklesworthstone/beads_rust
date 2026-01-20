use beads_rust::model::{Issue, IssueType, Priority, Status};
use beads_rust::storage::SqliteStorage;
use chrono::Utc;

fn make_issue(id: &str, title: &str, status: Status) -> Issue {
    Issue {
        id: id.to_string(),
        title: title.to_string(),
        status,
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
fn test_pinned_status_blocks_dependents() {
    // This test verifies that an issue with Status::Pinned DOES block issues that depend on it.
    // Current hypothesis: rebuild_blocked_cache_impl misses 'pinned' in its status filter.

    let mut storage = SqliteStorage::open_memory().unwrap();

    // 1. Create Pinned Issue A
    let issue_a = make_issue("bd-A", "Pinned Blocker", Status::Pinned);
    storage.create_issue(&issue_a, "setup").unwrap();

    // 2. Create Issue B that depends on A
    let issue_b = make_issue("bd-B", "Blocked Task", Status::Open);
    storage.create_issue(&issue_b, "setup").unwrap();
    storage
        .add_dependency("bd-B", "bd-A", "blocks", "setup")
        .unwrap();

    // 3. Rebuild cache (add_dependency does this, but let's be explicit)
    storage.rebuild_blocked_cache(true).unwrap();

    // 4. Verify B is blocked
    let is_blocked = storage.is_blocked("bd-B").unwrap();

    // Debug info
    if !is_blocked {
        let blockers = storage.get_blocked_issues().unwrap();
        println!("Blocked issues in cache: {:?}", blockers.len());
        for (i, b) in blockers {
            println!("  {} blocked by {:?}", i.id, b);
        }
    }

    assert!(is_blocked, "bd-B should be blocked by pinned bd-A");
}

#[test]
fn test_custom_status_blocks_dependents() {
    // Verify that a custom status also blocks
    let mut storage = SqliteStorage::open_memory().unwrap();

    let issue_a = make_issue(
        "bd-A",
        "Custom Blocker",
        Status::Custom("review".to_string()),
    );
    storage.create_issue(&issue_a, "setup").unwrap();

    let issue_b = make_issue("bd-B", "Blocked Task", Status::Open);
    storage.create_issue(&issue_b, "setup").unwrap();
    storage
        .add_dependency("bd-B", "bd-A", "blocks", "setup")
        .unwrap();

    storage.rebuild_blocked_cache(true).unwrap();

    let is_blocked = storage.is_blocked("bd-B").unwrap();
    assert!(is_blocked, "bd-B should be blocked by custom status bd-A");
}
