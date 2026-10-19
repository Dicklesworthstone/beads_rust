//! Regression coverage for the local replay/import boundary (GitHub #570).
//!
//! Exercise the shared batch restore, not a test-only SQL approximation. Read
//! provenance from its persisted column because export projections are allowed
//! to omit machine-local fields.

use crate::model::{Comment, Issue};
use crate::storage::SqliteStorage;
use crate::sync::{self, PreservedIssue};

fn issue(id: &str, path: Option<&str>) -> Issue {
    let mut issue: Issue = serde_json::from_value(serde_json::json!({
        "id": id,
        "title": format!("title {id}"),
        "created_at": "2026-01-01T00:00:00Z",
        "updated_at": "2026-01-01T00:00:00Z",
    }))
    .unwrap();
    issue.source_repo_path = path.map(str::to_owned);
    issue
}

fn preserved(issue: Issue) -> PreservedIssue {
    PreservedIssue {
        labels: Some(issue.labels.clone()),
        dependencies: Some(issue.dependencies.clone()),
        comments: Some(issue.comments.clone()),
        issue,
    }
}

fn stored_path(storage: &SqliteStorage, id: &str) -> Option<String> {
    let rows = storage
        .execute_raw_query("SELECT id, source_repo_path FROM issues")
        .unwrap();
    let row = rows
        .iter()
        .find(|row| row[0].as_text() == Some(id))
        .expect("the issue's persisted provenance row must exist");
    row[1].as_text().map(str::to_owned)
}

#[test]
fn schema_heal_restore_preserves_known_local_source_repo_path() {
    for imported_path in [None, Some("/other/clone")] {
        for local_path in ["/local/workspace", "/local/it's-a-project", ""] {
            let mut storage = SqliteStorage::open_memory().unwrap();
            let imported = issue("lp-abc", imported_path);
            storage.create_issue(&imported, "test").unwrap();
            let unrelated = issue("lp-other", Some("/unrelated/workspace"));
            storage.create_issue(&unrelated, "test").unwrap();
            let unrelated_before = storage.get_issue(&unrelated.id).unwrap();
            // No scalar revision changes: the regular import equality fast
            // path must not suppress restoration of a known local path.
            let mut local = imported.clone();
            local.source_repo_path = Some(local_path.to_owned());
            let plan = [preserved(local)];
            for _ in 0..2 {
                sync::restore_dirty_issues_after_rebuild(&mut storage, &plan).unwrap();
                assert_eq!(
                    stored_path(&storage, &imported.id).as_deref(),
                    Some(local_path)
                );
                assert_eq!(
                    stored_path(&storage, &unrelated.id).as_deref(),
                    Some("/unrelated/workspace")
                );
                assert_eq!(storage.get_issue(&unrelated.id).unwrap(), unrelated_before);
            }
            let dirty = storage
                .execute_raw_query("SELECT issue_id FROM dirty_issues")
                .unwrap();
            assert!(
                dirty
                    .iter()
                    .any(|row| row[0].as_text() == Some(imported.id.as_str()))
            );
        }
    }
}

#[test]
fn ordinary_import_still_keeps_the_existing_local_source_repo_path() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    let local = issue("lp-abc", Some("/this/clone"));
    storage.create_issue(&local, "test").unwrap();
    let mut incoming = local.clone();
    incoming.title = "a legitimate imported edit".to_owned();
    incoming.updated_at += chrono::Duration::seconds(1);
    incoming.source_repo_path = Some("/foreign/clone".to_owned());
    storage.upsert_issue_for_import(&incoming).unwrap();
    assert_eq!(
        storage.get_issue(&local.id).unwrap().unwrap().title,
        incoming.title
    );
    assert_eq!(
        stored_path(&storage, &local.id).as_deref(),
        Some("/this/clone")
    );
}

#[test]
fn schema_heal_restore_with_absent_local_path_keeps_imported_provenance() {
    for imported_path in [None, Some("/imported/clone")] {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let imported = issue("lp-abc", imported_path);
        storage.create_issue(&imported, "test").unwrap();
        let mut local = imported.clone();
        local.title = "saved local edit".to_owned();
        local.source_repo_path = None;
        for _ in 0..2 {
            sync::restore_dirty_issues_after_rebuild(&mut storage, &[preserved(local.clone())])
                .unwrap();
            assert_eq!(stored_path(&storage, &local.id).as_deref(), imported_path);
            assert_eq!(
                storage.get_issue(&local.id).unwrap().unwrap().title,
                local.title
            );
        }
    }
}

#[test]
fn schema_heal_restore_inserts_a_missing_local_issue_with_its_provenance() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    let local = issue("lp-new", Some("/local/workspace"));
    for _ in 0..2 {
        sync::restore_dirty_issues_after_rebuild(&mut storage, &[preserved(local.clone())])
            .unwrap();
        assert_eq!(
            stored_path(&storage, &local.id).as_deref(),
            Some("/local/workspace")
        );
    }
}

#[test]
fn schema_heal_restore_rolls_back_local_provenance_on_later_conflict() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    let mut imported = issue("lp-abc", Some("/other/clone"));
    let peer_comment: Comment = serde_json::from_value(serde_json::json!({
        "id": 1,
        "issue_id": imported.id,
        "author": "peer",
        "text": "peer-only history",
        "created_at": "2026-01-01T00:00:00Z",
    }))
    .unwrap();
    imported.comments.push(peer_comment);
    storage.create_issue(&imported, "test").unwrap();
    storage
        .sync_comments_for_import(&imported.id, &imported.comments)
        .unwrap();
    let mut owner = issue("lp-owner", Some("/owner/workspace"));
    owner.external_ref = Some("unique-restoration-reference".to_owned());
    storage.create_issue(&owner, "test").unwrap();
    let issues_before = storage
        .execute_raw_query("SELECT * FROM issues ORDER BY id")
        .unwrap();
    let dirty_before = storage
        .execute_raw_query("SELECT * FROM dirty_issues ORDER BY issue_id")
        .unwrap();
    let comments_before = storage.get_comments(&imported.id).unwrap();
    let mut local = imported.clone();
    local.title = "this change must roll back".to_owned();
    local.source_repo_path = Some("/local/workspace".to_owned());
    let mut conflicting = issue("lp-new", None);
    conflicting.external_ref.clone_from(&owner.external_ref);
    assert!(
        sync::restore_dirty_issues_after_rebuild(
            &mut storage,
            &[preserved(local.clone()), preserved(conflicting)],
        )
        .is_err()
    );
    assert_eq!(
        storage
            .execute_raw_query("SELECT * FROM issues ORDER BY id")
            .unwrap(),
        issues_before
    );
    assert_eq!(
        storage
            .execute_raw_query("SELECT * FROM dirty_issues ORDER BY issue_id")
            .unwrap(),
        dirty_before
    );
    assert_eq!(storage.get_comments(&imported.id).unwrap(), comments_before);
    assert_eq!(
        stored_path(&storage, &imported.id).as_deref(),
        Some("/other/clone")
    );
    // Positive control: neither a permanently quarantined connection nor an
    // invalid first payload may masquerade as successful rollback coverage.
    sync::restore_dirty_issues_after_rebuild(&mut storage, &[preserved(local)]).unwrap();
    assert_eq!(
        stored_path(&storage, &imported.id).as_deref(),
        Some("/local/workspace")
    );
    assert_eq!(storage.get_comments(&imported.id).unwrap(), comments_before);
}
