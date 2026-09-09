mod common;

use beads_rust::model::{Dependency, DependencyType, Issue};
use beads_rust::storage::SqliteStorage;
use beads_rust::sync::{ExportConfig, ImportConfig, export_to_jsonl, import_from_jsonl};
use common::fixtures;
use tempfile::TempDir;

#[test]
fn test_relation_updates_bump_timestamp_and_sync() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");

    // 1. Setup Source DB
    let mut source_db = SqliteStorage::open_memory().unwrap();
    let issue = fixtures::issue("Test Issue");
    source_db.create_issue(&issue, "tester").unwrap();

    // Initial export
    export_to_jsonl(&source_db, &path, &ExportConfig::default()).unwrap();

    // 2. Setup Target DB (simulating another machine)
    let mut target_db = SqliteStorage::open_memory().unwrap();
    import_from_jsonl(
        &mut target_db,
        &path,
        &ImportConfig::default(),
        Some("test-"),
    )
    .unwrap();

    // Verify sync baseline
    let _target_issue = target_db.get_issue(&issue.id).unwrap().unwrap();
    let target_labels = target_db.get_labels(&issue.id).unwrap();
    assert!(target_labels.is_empty());

    // 3. Modify Source: Add Label
    // Sleep to ensure timestamp would advance if we updated it
    std::thread::sleep(std::time::Duration::from_millis(100));

    // Check timestamp before
    let before_update = source_db.get_issue(&issue.id).unwrap().unwrap().updated_at;

    source_db.add_label(&issue.id, "bug", "tester").unwrap();

    // Check timestamp after
    let after_update = source_db.get_issue(&issue.id).unwrap().unwrap().updated_at;

    // ASSERTION 1: Timestamp should change
    assert!(
        after_update > before_update,
        "Adding label should update issue timestamp"
    );

    // 4. Sync Source -> Target
    export_to_jsonl(&source_db, &path, &ExportConfig::default()).unwrap();

    let import_result = import_from_jsonl(
        &mut target_db,
        &path,
        &ImportConfig::default(),
        Some("test-"),
    )
    .unwrap();

    // ASSERTION 2: Import should update, not skip
    assert_eq!(
        import_result.imported_count, 1,
        "Should import updated issue"
    );
    assert_eq!(
        import_result.skipped_count, 0,
        "Should not skip updated issue"
    );

    // ASSERTION 3: Label should be present in target
    let target_labels = target_db.get_labels(&issue.id).unwrap();
    assert_eq!(target_labels, vec!["bug".to_string()]);
}

#[test]
fn test_dependency_updates_bump_timestamp() {
    let mut db = SqliteStorage::open_memory().unwrap();
    let issue1 = fixtures::issue("Issue 1");
    let issue2 = fixtures::issue("Issue 2");
    db.create_issue(&issue1, "tester").unwrap();
    db.create_issue(&issue2, "tester").unwrap();

    let before = db.get_issue(&issue1.id).unwrap().unwrap().updated_at;
    std::thread::sleep(std::time::Duration::from_millis(100));

    db.add_dependency(&issue1.id, &issue2.id, "blocks", "tester")
        .unwrap();

    let after = db.get_issue(&issue1.id).unwrap().unwrap().updated_at;
    assert!(
        after > before,
        "Adding dependency should update issue timestamp"
    );
}

#[test]
fn typed_dependencies_preserve_both_payloads_through_export_import_and_replacement() {
    let temp = TempDir::new().unwrap();
    let source_path = temp.path().join("source.db");
    let target_path = temp.path().join("target.db");
    let jsonl_path = temp.path().join("issues.jsonl");
    let mut source = SqliteStorage::open(&source_path).unwrap();
    let issue = fixtures::issue("Independent typed relations");
    let blocker = fixtures::issue("Shared relation target");
    source.create_issue(&issue, "source-author").unwrap();
    source.create_issue(&blocker, "target-author").unwrap();
    for (kind, actor, metadata) in [
        (
            "blocks",
            "blocking-author",
            r#"{"reason":"required input"}"#,
        ),
        (
            "related",
            "related-author",
            r#"{"reason":"design context"}"#,
        ),
    ] {
        assert!(
            source
                .add_dependency_with_metadata(&issue.id, &blocker.id, kind, actor, Some(metadata))
                .unwrap()
        );
    }
    source
        .add_label(&issue.id, "retained-label", "source-author")
        .unwrap();
    source
        .add_comment(
            &issue.id,
            "discussion-author",
            "Preserve the source discussion",
        )
        .unwrap();
    export_to_jsonl(&source, &jsonl_path, &ExportConfig::default()).unwrap();
    let expected = source.get_dependencies_full(&issue.id).unwrap();
    assert_eq!(expected.len(), 2);
    drop(source);
    let reopened = SqliteStorage::open(&source_path).unwrap();
    assert_typed_dependencies(&reopened, &issue.id, &expected);
    drop(reopened);

    let mut target = SqliteStorage::open(&target_path).unwrap();
    let imported = import_from_jsonl(
        &mut target,
        &jsonl_path,
        &ImportConfig::default(),
        Some("test-"),
    )
    .unwrap();
    assert_eq!(imported.created_count, 2);
    assert_typed_dependencies(&target, &issue.id, &expected);
    let comments = target.get_comments(&issue.id).unwrap();
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].author, "discussion-author");
    assert_eq!(comments[0].body, "Preserve the source discussion");
    assert_eq!(target.get_labels(&issue.id).unwrap(), ["retained-label"]);
    let repeated = import_from_jsonl(
        &mut target,
        &jsonl_path,
        &ImportConfig::default(),
        Some("test-"),
    )
    .unwrap();
    assert_eq!(repeated.created_count, 0);
    assert_eq!(repeated.updated_count, 0);
    assert_eq!(repeated.skipped_count, 2);
    assert_typed_dependencies(&target, &issue.id, &expected);

    let replacement = replace_related_payload_in_jsonl(&jsonl_path, &issue.id);
    let imported = import_from_jsonl(
        &mut target,
        &jsonl_path,
        &ImportConfig::default(),
        Some("test-"),
    )
    .unwrap();
    assert_eq!(imported.updated_count, 1);
    assert_eq!(imported.skipped_count, 1);
    assert_typed_dependencies(&target, &issue.id, &replacement);
    assert_eq!(target.get_comments(&issue.id).unwrap(), comments);
    assert_eq!(target.get_labels(&issue.id).unwrap(), ["retained-label"]);
    assert_eq!(
        target.get_issue(&issue.id).unwrap().unwrap().title,
        issue.title
    );
    drop(target);
    let reopened = SqliteStorage::open(&target_path).unwrap();
    assert_typed_dependencies(&reopened, &issue.id, &replacement);
    assert_eq!(reopened.get_comments(&issue.id).unwrap(), comments);
}

fn assert_typed_dependencies(storage: &SqliteStorage, issue_id: &str, expected: &[Dependency]) {
    let mut actual = storage.get_dependencies_full(issue_id).unwrap();
    let mut expected = expected.to_vec();
    actual.sort_by(|left, right| left.dep_type.as_str().cmp(right.dep_type.as_str()));
    expected.sort_by(|left, right| left.dep_type.as_str().cmp(right.dep_type.as_str()));
    assert_eq!(actual.len(), 2);
    assert_eq!(actual[0].dep_type, DependencyType::Blocks);
    assert_eq!(actual[1].dep_type, DependencyType::Related);
    assert_eq!(actual[0].depends_on_id, actual[1].depends_on_id);
    assert_eq!(actual, expected);
}

fn replace_related_payload_in_jsonl(path: &std::path::Path, issue_id: &str) -> Vec<Dependency> {
    let mut records: Vec<Issue> = std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let issue = records
        .iter_mut()
        .find(|issue| issue.id == issue_id)
        .unwrap();
    assert_eq!(
        issue.dependencies.len(),
        2,
        "actual export must retain both types"
    );
    let blocking = issue
        .dependencies
        .iter()
        .find(|dep| dep.dep_type == DependencyType::Blocks)
        .unwrap()
        .clone();
    let related = issue
        .dependencies
        .iter_mut()
        .find(|dep| dep.dep_type == DependencyType::Related)
        .unwrap();
    related.metadata = Some(r#"{"reason":"updated design context"}"#.to_string());
    related.thread_id = Some("replacement-discussion".to_string());
    related.created_at += chrono::Duration::seconds(1);
    issue.updated_at += chrono::Duration::seconds(2);
    let expected = issue.dependencies.clone();
    assert_eq!(
        expected
            .iter()
            .find(|dep| dep.dep_type == DependencyType::Blocks),
        Some(&blocking)
    );
    let mut bytes = Vec::new();
    for record in records {
        serde_json::to_writer(&mut bytes, &record).unwrap();
        bytes.push(b'\n');
    }
    std::fs::write(path, bytes).unwrap();
    expected
}
