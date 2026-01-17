mod common;

use beads_rust::model::{DependencyType, Issue, Priority, Status};
use beads_rust::storage::SqliteStorage;
use beads_rust::sync::{
    ExportConfig, ImportConfig, export_to_jsonl, import_from_jsonl, read_issues_from_jsonl,
};
use chrono::{Duration, Utc};
use common::fixtures;
use std::fs;
use tempfile::TempDir;

fn issue_with_id(id: &str, title: &str) -> Issue {
    let mut issue = fixtures::issue(title);
    issue.id = id.to_string();
    issue
}

#[test]
fn export_import_roundtrip_preserves_relationships() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    let mut alpha = fixtures::issue("Alpha");
    let mut beta = fixtures::issue("Beta");

    alpha.priority = Priority::HIGH;
    alpha.external_ref = Some("ext-1".to_string());
    beta.status = Status::InProgress;

    storage.create_issue(&alpha, "tester").unwrap();
    storage.create_issue(&beta, "tester").unwrap();
    storage
        .add_dependency(
            &beta.id,
            &alpha.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();
    storage.add_label(&alpha.id, "alpha", "tester").unwrap();
    storage
        .add_comment(&alpha.id, "tester", "first comment")
        .unwrap();

    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");
    let export = export_to_jsonl(&storage, &path, &ExportConfig::default()).unwrap();
    assert_eq!(export.exported_count, 2);

    let mut imported = SqliteStorage::open_memory().unwrap();
    let import = import_from_jsonl(
        &mut imported,
        &path,
        &ImportConfig::default(),
        Some("test-"),
    )
    .unwrap();
    assert_eq!(import.imported_count, 2);

    let imported_alpha = imported.get_issue(&alpha.id).unwrap().unwrap();
    assert_eq!(imported_alpha.title, alpha.title);
    assert_eq!(imported_alpha.external_ref, Some("ext-1".to_string()));

    let labels = imported.get_labels(&alpha.id).unwrap();
    assert_eq!(labels, vec!["alpha".to_string()]);

    let deps = imported.get_dependencies(&beta.id).unwrap();
    assert_eq!(deps, vec![alpha.id.clone()]);

    let comments = imported.get_comments(&alpha.id).unwrap();
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].body, "first comment");
}

#[test]
fn export_sorts_by_id() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    let issue_b = issue_with_id("test-b", "B");
    let issue_a = issue_with_id("test-a", "A");

    storage.create_issue(&issue_b, "tester").unwrap();
    storage.create_issue(&issue_a, "tester").unwrap();

    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");
    export_to_jsonl(&storage, &path, &ExportConfig::default()).unwrap();

    let issues = read_issues_from_jsonl(&path).unwrap();
    let ids: Vec<&str> = issues.iter().map(|issue| issue.id.as_str()).collect();
    assert_eq!(ids, vec!["test-a", "test-b"]);
}

#[test]
fn import_rejects_malformed_json() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");
    fs::write(&path, "not json\n").unwrap();

    let mut storage = SqliteStorage::open_memory().unwrap();
    let err = import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-"))
        .unwrap_err();
    assert!(err.to_string().contains("Invalid JSON"));
}

#[test]
fn import_rejects_prefix_mismatch() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");
    let issue = issue_with_id("xx-1", "Mismatch");
    let json = serde_json::to_string(&issue).unwrap();
    fs::write(&path, format!("{json}\n")).unwrap();

    let mut storage = SqliteStorage::open_memory().unwrap();
    let err = import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-"))
        .unwrap_err();
    assert!(err.to_string().contains("Prefix mismatch"));
}

#[test]
fn import_sets_closed_at_when_missing() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");
    let mut issue = issue_with_id("test-closed", "Closed");
    issue.status = Status::Closed;
    issue.updated_at = Utc::now() - Duration::hours(1);
    issue.closed_at = None;
    let json = serde_json::to_string(&issue).unwrap();
    fs::write(&path, format!("{json}\n")).unwrap();

    let mut storage = SqliteStorage::open_memory().unwrap();
    import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-")).unwrap();

    let imported = storage.get_issue(&issue.id).unwrap().unwrap();
    assert_eq!(imported.closed_at, Some(issue.updated_at));
}

#[test]
fn import_rejects_conflict_markers() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");
    fs::write(&path, "<<<<<<< HEAD\n").unwrap();

    let mut storage = SqliteStorage::open_memory().unwrap();
    let err = import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-"))
        .unwrap_err();
    assert!(err.to_string().contains("Merge conflict markers detected"));
}
