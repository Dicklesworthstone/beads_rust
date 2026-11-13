mod common;

use beads_rust::model::{
    Comment, CommentState, Dependency, DependencyType, Issue, comments_sync_equal,
    normalize_comment_revisions,
};
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

fn revision_time(seconds: i64) -> chrono::DateTime<chrono::Utc> {
    chrono::DateTime::from_timestamp(1_700_000_000 + seconds, 0).unwrap()
}

fn revision_issue() -> Issue {
    let mut issue = Issue {
        id: "test-revisions".to_string(),
        title: "Portable comment revision fixture".to_string(),
        created_at: revision_time(10),
        updated_at: revision_time(100),
        ..Issue::default()
    };
    issue.comments.push(Comment {
        id: 17,
        issue_id: issue.id.clone(),
        author: "original-author".to_string(),
        body: "sensitive-original-fixture".to_string(),
        created_at: revision_time(20),
        revision: None,
    });
    normalize_comment_revisions(&mut issue.comments).unwrap();
    issue
}

fn store_revision_fixture(storage: &SqliteStorage, issue: &Issue) {
    storage.upsert_issue_for_import(issue).unwrap();
    storage
        .sync_comments_for_import(&issue.id, &issue.comments)
        .unwrap();
}

fn write_revision_records(path: &std::path::Path, issues: &[Issue]) {
    let mut bytes = Vec::new();
    for issue in issues {
        serde_json::to_writer(&mut bytes, issue).unwrap();
        bytes.push(b'\n');
    }
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn comment_revision_equal_count_import_matrix_preserves_ids_attribution_and_repetition() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("incoming.jsonl");
    let output = temp.path().join("roundtrip.jsonl");
    for state in [
        CommentState::Active,
        CommentState::Redacted,
        CommentState::Deleted,
    ] {
        for incoming_time in [50, 100, 200] {
            let original = revision_issue();
            let mut storage = SqliteStorage::open_memory().unwrap();
            store_revision_fixture(&storage, &original);
            let mut incoming = original.clone();
            incoming.comments[0].id = 900;
            incoming.comments[0]
                .revise(
                    state,
                    if state == CommentState::Active {
                        "corrected text"
                    } else {
                        ""
                    },
                    "editing-actor",
                    revision_time(30),
                )
                .unwrap();
            incoming.updated_at = revision_time(incoming_time);
            write_revision_records(&path, &[incoming.clone()]);
            let result =
                import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test"))
                    .unwrap();
            assert_eq!(result.updated_count, usize::from(incoming_time > 100));
            assert_eq!(result.skipped_count, usize::from(incoming_time <= 100));
            let saved = storage.get_comments(&original.id).unwrap();
            assert_eq!(saved.len(), 1);
            assert_eq!(saved[0].id, original.comments[0].id);
            assert_eq!(saved[0].author, original.comments[0].author);
            assert_eq!(saved[0].created_at, original.comments[0].created_at);
            assert!(comments_sync_equal(&saved, &incoming.comments));
            for _ in 0..2 {
                import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test"))
                    .unwrap();
                assert_eq!(storage.get_comments(&original.id).unwrap(), saved);
            }
            let mut legacy = original.clone();
            legacy.comments[0].revision = None;
            write_revision_records(&path, &[legacy]);
            for force_upsert in [false, true] {
                import_from_jsonl(
                    &mut storage,
                    &path,
                    &ImportConfig {
                        force_upsert,
                        ..Default::default()
                    },
                    Some("test"),
                )
                .unwrap();
                assert_eq!(storage.get_comments(&original.id).unwrap(), saved);
            }
            // Each case has a distinct output generation so the export guard
            // never mistakes a previous test case's branch for this source.
            write_revision_records(&output, &[]);
            export_to_jsonl(&storage, &output, &ExportConfig::default()).unwrap();
            let encoded = std::fs::read_to_string(&output).unwrap();
            assert!(!encoded.contains("sensitive-original-fixture"));
            let mut fresh = SqliteStorage::open_memory().unwrap();
            import_from_jsonl(&mut fresh, &output, &ImportConfig::default(), Some("test")).unwrap();
            assert!(comments_sync_equal(
                &fresh.get_comments(&original.id).unwrap(),
                &saved
            ));
        }
    }
}

#[test]
fn comment_revision_two_machine_roundtrip_converges_through_redaction_and_deletion() {
    let temp = TempDir::new().unwrap();
    let a_path = temp.path().join("a.jsonl");
    let b_path = temp.path().join("b.jsonl");
    let legacy_path = temp.path().join("legacy.jsonl");
    let mut a = SqliteStorage::open(&temp.path().join("a.db")).unwrap();
    let mut b = SqliteStorage::open(&temp.path().join("b.db")).unwrap();
    let original = revision_issue();
    store_revision_fixture(&a, &original);
    let mut b_original = original.clone();
    b_original.comments[0].id = 42;
    store_revision_fixture(&b, &b_original);
    export_to_jsonl(&a, &a_path, &ExportConfig::default()).unwrap();
    import_from_jsonl(&mut b, &a_path, &ImportConfig::default(), Some("test")).unwrap();
    assert_eq!(b.get_comments(&original.id).unwrap()[0].id, 42);

    let mut removed = original.clone();
    removed.comments[0]
        .revise(
            CommentState::Redacted,
            "",
            "privacy-actor",
            revision_time(30),
        )
        .unwrap();
    removed.updated_at = revision_time(50);
    store_revision_fixture(&a, &removed);
    let mut edited = b_original;
    for time in [200, 300, 400] {
        edited.comments[0]
            .revise(
                CommentState::Active,
                &format!("concurrent-private-{time}"),
                "remote-actor",
                revision_time(time),
            )
            .unwrap();
    }
    edited.updated_at = revision_time(900);
    store_revision_fixture(&b, &edited);
    export_to_jsonl(&a, &a_path, &ExportConfig::default()).unwrap();
    export_to_jsonl(&b, &b_path, &ExportConfig::default()).unwrap();
    import_from_jsonl(&mut a, &b_path, &ImportConfig::default(), Some("test")).unwrap();
    import_from_jsonl(&mut b, &a_path, &ImportConfig::default(), Some("test")).unwrap();
    for machine in [&a, &b] {
        let comments = machine.get_comments(&original.id).unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].state(), CommentState::Redacted);
        assert!(comments[0].body.is_empty());
    }
    assert_eq!(a.get_comments(&original.id).unwrap()[0].id, 17);
    assert_eq!(b.get_comments(&original.id).unwrap()[0].id, 42);
    assert!(comments_sync_equal(
        &a.get_comments(&original.id).unwrap(),
        &b.get_comments(&original.id).unwrap()
    ));

    let mut deleted = b.get_issue_for_export(&original.id).unwrap().unwrap();
    deleted.comments[0]
        .revise(
            CommentState::Deleted,
            "",
            "deleting-actor",
            revision_time(600),
        )
        .unwrap();
    deleted.updated_at = revision_time(600);
    store_revision_fixture(&b, &deleted);
    export_to_jsonl(&b, &b_path, &ExportConfig::default()).unwrap();
    import_from_jsonl(&mut a, &b_path, &ImportConfig::default(), Some("test")).unwrap();
    export_to_jsonl(&a, &a_path, &ExportConfig::default()).unwrap();
    import_from_jsonl(&mut b, &a_path, &ImportConfig::default(), Some("test")).unwrap();
    let settled = a.get_comments(&original.id).unwrap();
    assert_eq!(settled[0].state(), CommentState::Deleted);
    let mut legacy = original;
    legacy.comments[0].revision = None;
    write_revision_records(&legacy_path, &[legacy.clone()]);
    for machine in [&mut a, &mut b] {
        for _ in 0..2 {
            import_from_jsonl(
                machine,
                &legacy_path,
                &ImportConfig::default(),
                Some("test"),
            )
            .unwrap();
            assert!(comments_sync_equal(
                &machine.get_comments(&legacy.id).unwrap(),
                &settled
            ));
        }
    }
    let mut fresh = SqliteStorage::open(&temp.path().join("fresh.db")).unwrap();
    import_from_jsonl(&mut fresh, &a_path, &ImportConfig::default(), Some("test")).unwrap();
    assert!(comments_sync_equal(
        &fresh.get_comments(&legacy.id).unwrap(),
        &settled
    ));
    for path in [&a_path, &b_path] {
        let encoded = std::fs::read_to_string(path).unwrap();
        assert!(!encoded.contains("sensitive-original-fixture"));
        assert!(!encoded.contains("concurrent-private"));
    }
}

#[test]
fn comment_revision_concurrent_active_import_is_atomic_even_when_forced() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("incoming.jsonl");
    let original = revision_issue();
    let mut storage = SqliteStorage::open_memory().unwrap();
    storage.create_issue(&original, "original-author").unwrap();
    store_revision_fixture(&storage, &original);
    let mut local = original.clone();
    local.comments[0]
        .revise(
            CommentState::Active,
            "private-local-branch",
            "original-author",
            revision_time(30),
        )
        .unwrap();
    store_revision_fixture(&storage, &local);
    let mut remote = original.clone();
    remote.comments[0]
        .revise(
            CommentState::Active,
            "private-remote-branch",
            "original-author",
            revision_time(40),
        )
        .unwrap();
    remote.updated_at = revision_time(200);
    let preceding = Issue {
        id: "test-aaa".to_string(),
        title: "Must roll back".to_string(),
        comments: Vec::new(),
        ..original.clone()
    };
    write_revision_records(&path, &[preceding.clone(), remote]);
    let before_issue = storage.get_issue_for_export(&original.id).unwrap();
    let before_events = storage.get_all_events(0).unwrap();
    let before_dirty = storage.get_dirty_issue_metadata().unwrap();
    let before_hash = storage.get_export_hash(&original.id).unwrap();
    for force_upsert in [false, true] {
        let error = import_from_jsonl(
            &mut storage,
            &path,
            &ImportConfig {
                force_upsert,
                ..Default::default()
            },
            Some("test"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("Concurrent edits"));
        assert!(!error.to_string().contains("private-local-branch"));
        assert!(!error.to_string().contains("private-remote-branch"));
        assert_eq!(
            storage.get_issue_for_export(&original.id).unwrap(),
            before_issue
        );
        assert_eq!(storage.get_all_events(0).unwrap(), before_events);
        assert_eq!(storage.get_dirty_issue_metadata().unwrap(), before_dirty);
        assert_eq!(storage.get_export_hash(&original.id).unwrap(), before_hash);
        assert!(storage.get_issue(&preceding.id).unwrap().is_none());
    }
}

#[test]
fn comment_revision_legacy_identical_siblings_survive_redaction_and_repeated_imports() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("legacy.jsonl");
    let mut original = revision_issue();
    original.comments[0].revision = None;
    let mut sibling = original.comments[0].clone();
    sibling.id = 18;
    original.comments.push(sibling);
    write_revision_records(&path, &[original.clone()]);
    let mut storage = SqliteStorage::open_memory().unwrap();
    import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test")).unwrap();
    let comments = storage.get_comments(&original.id).unwrap();
    assert_eq!(comments.len(), 2);
    assert_ne!(comments[0].revision, comments[1].revision);
    storage
        .redact_comment(
            &original.id,
            comments[0].id,
            "original-author",
            comments[0].revision_head(),
            false,
        )
        .unwrap();
    for _ in 0..3 {
        import_from_jsonl(
            &mut storage,
            &path,
            &ImportConfig {
                force_upsert: true,
                ..Default::default()
            },
            Some("test"),
        )
        .unwrap();
        let current = storage.get_comments(&original.id).unwrap();
        assert_eq!(current.len(), 2);
        assert_eq!(
            current
                .iter()
                .filter(|comment| comment.state() == CommentState::Redacted)
                .count(),
            1
        );
        assert_eq!(
            current
                .iter()
                .filter(|comment| comment.body == "sensitive-original-fixture")
                .count(),
            1
        );
    }
    let visible = storage
        .get_comments(&original.id)
        .unwrap()
        .into_iter()
        .find(|comment| comment.state() == CommentState::Active)
        .unwrap();
    storage
        .delete_comment(
            &original.id,
            visible.id,
            "original-author",
            visible.revision_head(),
            false,
        )
        .unwrap();
    import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test")).unwrap();
    let final_comments = storage.get_comments(&original.id).unwrap();
    assert_eq!(final_comments.len(), 2);
    assert!(final_comments.iter().all(|comment| comment.body.is_empty()));
}

#[test]
fn comment_revision_alias_import_enrolls_before_rekey_and_respects_later_suppression() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("aliases.jsonl");
    let mut target = revision_issue();
    target.comments.clear();
    let mut alias = target.clone();
    alias.id = "test-alias".to_string();
    alias.updated_at = revision_time(200);
    alias.comments = revision_issue().comments;
    alias.comments[0].issue_id.clone_from(&alias.id);
    alias.comments[0].revision = None;
    let mut storage = SqliteStorage::open_memory().unwrap();
    store_revision_fixture(&storage, &target);
    write_revision_records(&path, &[alias.clone()]);
    import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test")).unwrap();
    let saved = storage.get_comments(&target.id).unwrap();
    assert_eq!(saved.len(), 1);
    let mut expected = alias.comments.clone();
    normalize_comment_revisions(&mut expected).unwrap();
    assert_eq!(saved[0].revision, expected[0].revision);
    storage
        .redact_comment(
            &target.id,
            saved[0].id,
            "original-author",
            saved[0].revision_head(),
            false,
        )
        .unwrap();
    for records in [
        vec![alias.clone(), target.clone()],
        vec![target.clone(), alias],
    ] {
        write_revision_records(&path, &records);
        for _ in 0..2 {
            import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test")).unwrap();
            let current = storage.get_comments(&target.id).unwrap();
            assert_eq!(current.len(), 1);
            assert_eq!(current[0].id, saved[0].id);
            assert_eq!(current[0].state(), CommentState::Redacted);
            assert!(current[0].body.is_empty());
        }
    }
}

#[test]
fn comment_revision_preupgrade_alias_import_refuses_lost_provenance_atomically() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("preupgrade-alias.jsonl");
    for (alias_id, rename_on_import) in [("test-alias", false), ("old-revisions", true)] {
        for state in [CommentState::Redacted, CommentState::Deleted] {
            let mut original = revision_issue();
            if rename_on_import {
                original.external_ref = Some(alias_id.to_string());
            }
            let mut storage = SqliteStorage::open_memory().unwrap();
            store_revision_fixture(&storage, &original);
            // Simulate a row already rekeyed by the old importer: its owning
            // issue has changed and no portable origin metadata was retained.
            storage
                .execute_raw("UPDATE comments SET revision = NULL")
                .unwrap();
            if state == CommentState::Redacted {
                storage
                    .redact_comment(&original.id, 17, "original-author", None, false)
                    .unwrap();
            } else {
                storage
                    .delete_comment(&original.id, 17, "original-author", None, false)
                    .unwrap();
            }
            let before = storage.get_issue_for_export(&original.id).unwrap();
            let before_events = storage.get_all_events(0).unwrap();
            let before_dirty = storage.get_dirty_issue_metadata().unwrap();
            let preceding = Issue {
                id: "test-before-alias".to_string(),
                title: "Must remain absent after refusal".to_string(),
                external_ref: None,
                comments: Vec::new(),
                ..original.clone()
            };
            for enrolled_alias in [false, true] {
                let mut alias = original.clone();
                alias.id = alias_id.to_string();
                alias.updated_at = revision_time(900);
                alias.comments[0].issue_id.clone_from(&alias.id);
                alias.comments[0].revision = None;
                if enrolled_alias {
                    normalize_comment_revisions(&mut alias.comments).unwrap();
                }
                write_revision_records(&path, &[preceding.clone(), alias]);
                for force_upsert in [false, true] {
                    let error = import_from_jsonl(
                        &mut storage,
                        &path,
                        &ImportConfig {
                            force_upsert,
                            rename_on_import,
                            ..Default::default()
                        },
                        Some("test"),
                    )
                    .unwrap_err();
                    assert!(error.to_string().contains("legacy comment"), "{error}");
                    assert!(!error.to_string().contains("sensitive-original-fixture"));
                    assert_eq!(storage.get_issue_for_export(&original.id).unwrap(), before);
                    assert_eq!(storage.get_all_events(0).unwrap(), before_events);
                    assert_eq!(storage.get_dirty_issue_metadata().unwrap(), before_dirty);
                    assert!(storage.get_issue(&preceding.id).unwrap().is_none());
                }
            }
        }
    }
}

#[test]
fn comment_revision_alias_join_applies_all_terminal_evidence_before_active_conflicts() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("aliases.jsonl");
    let original = revision_issue();
    let mut left = original.clone();
    left.comments[0]
        .revise(
            CommentState::Active,
            "private-left",
            "original-author",
            revision_time(30),
        )
        .unwrap();
    let mut right = original.clone();
    right.comments[0]
        .revise(
            CommentState::Active,
            "private-right",
            "original-author",
            revision_time(40),
        )
        .unwrap();
    let mut removed = original.clone();
    removed.comments[0]
        .revise(
            CommentState::Redacted,
            "",
            "original-author",
            revision_time(50),
        )
        .unwrap();
    let aliases = [left, right, removed.clone()].map(|mut issue| {
        let state = issue.comments[0].revision.as_ref().unwrap();
        issue.id = format!("test-alias-{}", &state.head[..8]);
        issue.comments[0].issue_id.clone_from(&issue.id);
        issue
    });
    for local_terminal in [false, true] {
        for incoming_time in [60, 200] {
            for order in [
                [0, 1, 2],
                [0, 2, 1],
                [1, 0, 2],
                [1, 2, 0],
                [2, 0, 1],
                [2, 1, 0],
            ] {
                let mut storage = SqliteStorage::open_memory().unwrap();
                store_revision_fixture(&storage, if local_terminal { &removed } else { &original });
                let records = order
                    .into_iter()
                    .filter(|index| !local_terminal || *index != 2)
                    .map(|index| {
                        let mut issue = aliases[index].clone();
                        issue.updated_at = revision_time(incoming_time);
                        issue
                    })
                    .collect::<Vec<_>>();
                write_revision_records(&path, &records);
                for _ in 0..2 {
                    import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test"))
                        .unwrap();
                    let current = storage.get_comments(&original.id).unwrap();
                    assert_eq!(current.len(), 1);
                    assert_eq!(current[0].id, original.comments[0].id);
                    assert!(comments_sync_equal(&current, &removed.comments));
                }
            }
        }
    }
}

#[test]
fn comment_revision_bulk_import_reserves_local_ids_across_all_target_orders() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("global-rowids.jsonl");
    for peer_action in ["insert", "update", "skip"] {
        for peer_first in [false, true] {
            let mut storage = SqliteStorage::open_memory().unwrap();
            let mut original = revision_issue();
            original.id = "test-owner-a".to_string();
            original.comments[0].issue_id.clone_from(&original.id);
            original.comments[0].revision = None;
            normalize_comment_revisions(&mut original.comments).unwrap();
            store_revision_fixture(&storage, &original);
            let mut update = original.clone();
            update.title = "Scalar update keeps A's comment rowid".to_string();
            update.updated_at = revision_time(200);
            update.comments[0].id = 999;

            let mut peer = Issue {
                id: "test-owner-b".to_string(),
                title: "Independent issue B".to_string(),
                comments: Vec::new(),
                ..original.clone()
            };
            if peer_action != "insert" {
                store_revision_fixture(&storage, &peer);
            }
            peer.updated_at = revision_time(if peer_action == "skip" { 50 } else { 200 });
            peer.comments.push(Comment {
                id: 17,
                issue_id: peer.id.clone(),
                author: "peer-author".to_string(),
                body: "Independent B comment shares only a foreign rowid".to_string(),
                created_at: revision_time(30),
                revision: None,
            });
            let records = if peer_first {
                vec![peer.clone(), update]
            } else {
                vec![update, peer.clone()]
            };
            write_revision_records(&path, &records);
            import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test")).unwrap();
            let a_comments = storage.get_comments(&original.id).unwrap();
            let b_comments = storage.get_comments(&peer.id).unwrap();
            assert_eq!(a_comments.len(), 1);
            assert_eq!(b_comments.len(), 1);
            assert_eq!(
                a_comments[0].id, 17,
                "{peer_action}, peer_first={peer_first}"
            );
            assert_ne!(b_comments[0].id, 17);
            assert!(comments_sync_equal(&a_comments, &original.comments));
            assert!(comments_sync_equal(&b_comments, &peer.comments));
            import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test")).unwrap();
            assert_eq!(storage.get_comments(&original.id).unwrap(), a_comments);
            assert_eq!(storage.get_comments(&peer.id).unwrap(), b_comments);
        }
    }
}
