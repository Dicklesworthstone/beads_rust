use beads_rust::model::{Issue, IssueType, Priority, Status};
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
        description: Some("Same description".to_string()),
        status: Status::Open,
        priority: Priority::MEDIUM,
        issue_type: IssueType::Task,
        created_at: Utc::now(),
        updated_at: Utc::now(),
        // Defaults
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
fn test_collision_identical_content_different_labels() {
    // This test verifies that issues with IDENTICAL content (title, desc, etc.)
    // but DIFFERENT labels are considered to have the SAME content hash.
    //
    // If they have different IDs, they coexist in DB.
    // But if one is imported as "new" (unknown ID or different ID) but matches content hash,
    // it will collide and trigger a merge (update) of the existing issue.

    let mut storage = SqliteStorage::open_memory().unwrap();
    let temp_dir = TempDir::new().unwrap();
    let jsonl_path = temp_dir.path().join("issues.jsonl");

    // 1. Create Issue A in DB
    let mut issue_a = make_issue("bd-a", "Identical Task");
    issue_a.labels = vec!["label-a".to_string()];
    // Compute hash (excludes labels)
    issue_a.content_hash = Some(issue_a.compute_content_hash());
    storage.create_issue(&issue_a, "setup").unwrap();
    storage.add_label("bd-a", "label-a", "setup").unwrap();

    // 2. Create Issue B in JSONL (Same content, different label, different ID)
    // Timestamp is NEWER to force update
    let mut issue_b = make_issue("bd-b", "Identical Task");
    issue_b.labels = vec!["label-b".to_string()];
    issue_b.updated_at = Utc::now() + chrono::Duration::hours(1);
    // Hash should be IDENTICAL to A because labels are excluded
    let hash_b = issue_b.compute_content_hash();
    assert_eq!(
        issue_a.content_hash.unwrap(),
        hash_b,
        "Hashes must match for collision logic"
    );

    let json = serde_json::to_string(&issue_b).unwrap();
    fs::write(&jsonl_path, format!("{json}\n")).unwrap();

    // 3. Import
    // Expectation:
    // - Detects collision by ContentHash (Phase 2) against bd-a
    // - Since bd-b is newer, it UPDATES bd-a
    // - bd-b ID is remapped to bd-a
    // - bd-a gains "label-b" (and loses "label-a" because import syncs labels)
    let config = ImportConfig::default();
    let result = import_from_jsonl(&mut storage, &jsonl_path, &config, None).unwrap();

    assert_eq!(result.imported_count, 1);

    // 4. Verify DB state
    // bd-a should exist and be updated
    let loaded_a = storage.get_issue("bd-a").unwrap().unwrap();
    assert_eq!(loaded_a.id, "bd-a");
    let labels_a = storage.get_labels("bd-a").unwrap();

    // bd-b should NOT exist (it was merged into a)
    let loaded_b = storage.get_issue("bd-b").unwrap();
    assert!(loaded_b.is_none(), "bd-b should be merged into bd-a");

    // bd-a should have label-b (import syncs/replaces labels)
    assert!(labels_a.contains(&"label-b".to_string()));
    assert!(!labels_a.contains(&"label-a".to_string())); // Replaced
}
