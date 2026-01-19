use beads_rust::model::{Dependency, DependencyType, Issue, IssueType, Priority, Status};
use beads_rust::storage::SqliteStorage;
use beads_rust::sync::{ImportConfig, import_from_jsonl};
use chrono::Utc;
use std::fs;
use tempfile::TempDir;

fn make_issue(id: &str, title: &str) -> Issue {
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
        issue_type: IssueType::Task,
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
fn test_import_collision_remaps_dependencies() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    let temp_dir = TempDir::new().unwrap();
    let jsonl_path = temp_dir.path().join("issues.jsonl");

    // 1. Create DB with issue bd-1 (external_ref="EXT-1")
    let mut db_issue = make_issue("bd-1", "Original");
    db_issue.external_ref = Some("EXT-1".to_string());
    // Ensure it has a hash
    db_issue.content_hash = Some(beads_rust::util::content_hash(&db_issue));
    storage.create_issue(&db_issue, "user").unwrap();

    // 2. Create JSONL with:
    //    - bd-2 (external_ref="EXT-1") -> Should collide with bd-1 and trigger update
    //    - bd-3 (depends on bd-2)      -> Should be remapped to depend on bd-1
    let mut jsonl_issue1 = make_issue("bd-2", "Updated via Import");
    jsonl_issue1.external_ref = Some("EXT-1".to_string());
    jsonl_issue1.updated_at = Utc::now() + chrono::Duration::seconds(10); // Newer

    let mut jsonl_issue2 = make_issue("bd-3", "Dependent");
    jsonl_issue2.dependencies.push(Dependency {
        issue_id: "bd-3".to_string(),
        depends_on_id: "bd-2".to_string(),
        dep_type: DependencyType::Blocks,
        created_at: Utc::now(),
        created_by: None,
        metadata: None,
        thread_id: None,
    });

    let content = format!(
        "{}\n{}\n",
        serde_json::to_string(&jsonl_issue1).unwrap(),
        serde_json::to_string(&jsonl_issue2).unwrap()
    );
    fs::write(&jsonl_path, content).unwrap();

    // 3. Import
    let config = ImportConfig::default();
    let _result = import_from_jsonl(&mut storage, &jsonl_path, &config, None).unwrap();

    // 4. Verify
    // bd-1 should be updated
    let bd1 = storage.get_issue("bd-1").unwrap().unwrap();
    assert_eq!(bd1.title, "Updated via Import");

    // bd-2 should NOT exist
    assert!(
        storage.get_issue("bd-2").unwrap().is_none(),
        "bd-2 should have been merged into bd-1"
    );

    // bd-3 should exist
    let bd3 = storage.get_issue("bd-3").unwrap().unwrap();
    assert_eq!(bd3.title, "Dependent");

    // bd-3 should depend on bd-1 (NOT bd-2)
    let deps = storage.get_dependencies("bd-3").unwrap();
    assert!(
        deps.contains(&"bd-1".to_string()),
        "bd-3 should depend on bd-1"
    );
    assert!(
        !deps.contains(&"bd-2".to_string()),
        "bd-3 should not depend on bd-2"
    );
}
