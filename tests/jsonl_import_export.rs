mod common;

use beads_rust::model::{Comment, Dependency, DependencyType, Issue, Priority, Status};
use beads_rust::storage::SqliteStorage;
use beads_rust::sync::{
    ExportConfig, ImportConfig, auto_flush, export_to_jsonl, finalize_export, import_from_jsonl,
    read_issues_from_jsonl,
};
use chrono::{Duration, TimeZone, Utc};
use common::fixtures;
use std::fs;
use std::path::Path;
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
    // Ensure created_at is strictly before any updates (SQLite CURRENT_TIMESTAMP has low precision)
    alpha.created_at = Utc::now() - Duration::hours(1);
    alpha.updated_at = alpha.created_at;
    let mut beta = fixtures::issue("Beta");
    beta.created_at = alpha.created_at;
    beta.updated_at = alpha.created_at;

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
fn import_reads_multiple_jsonl_lines_without_buffer_accumulation() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");
    let issue_a = issue_with_id("test-a", "First");
    let issue_b = issue_with_id("test-b", "Second");
    let json_a = serde_json::to_string(&issue_a).unwrap();
    let json_b = serde_json::to_string(&issue_b).unwrap();
    fs::write(&path, format!("{json_a}\n{json_b}\n")).unwrap();

    let mut storage = SqliteStorage::open_memory().unwrap();
    let import =
        import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-")).unwrap();

    assert_eq!(import.imported_count, 2);
    assert!(storage.get_issue("test-a").unwrap().is_some());
    assert!(storage.get_issue("test-b").unwrap().is_some());
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
fn export_sorts_comments_canonically_after_loading_relations() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    let timestamp = Utc.with_ymd_and_hms(2026, 1, 2, 3, 4, 5).unwrap();
    let mut issue = issue_with_id("test-comment-sort", "Comment sort");
    issue.created_at = timestamp;
    issue.updated_at = timestamp;
    issue.comments = vec![
        Comment {
            id: 0,
            issue_id: issue.id.clone(),
            author: "zara".to_string(),
            body: "second by canonical order".to_string(),
            created_at: timestamp,
        },
        Comment {
            id: 0,
            issue_id: issue.id.clone(),
            author: "alice".to_string(),
            body: "first by canonical order".to_string(),
            created_at: timestamp,
        },
    ];

    storage.create_issue(&issue, "tester").unwrap();

    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");
    export_to_jsonl(&storage, &path, &ExportConfig::default()).unwrap();

    let issues = read_issues_from_jsonl(&path).unwrap();
    assert_eq!(issues.len(), 1);
    let authors: Vec<&str> = issues[0]
        .comments
        .iter()
        .map(|comment| comment.author.as_str())
        .collect();
    assert_eq!(authors, vec!["alice", "zara"]);
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
    let issue = issue_with_id("xx-001", "Mismatch");
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
    issue.created_at = Utc::now() - Duration::hours(2);
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
fn export_import_roundtrip_keeps_optional_text_fields_integrity_safe() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    let issue = issue_with_id("test-opttext", "Optional text fields");
    storage.create_issue(&issue, "tester").unwrap();

    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");
    export_to_jsonl(&storage, &path, &ExportConfig::default()).unwrap();

    let db_path = temp.path().join("import.db");
    let mut imported = SqliteStorage::open(&db_path).unwrap();
    import_from_jsonl(
        &mut imported,
        &path,
        &ImportConfig::default(),
        Some("test-"),
    )
    .unwrap();

    let imported_issue = imported.get_issue(&issue.id).unwrap().unwrap();
    assert_eq!(imported_issue.design, None);
    assert_eq!(imported_issue.acceptance_criteria, None);
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

// ===== Safety Guard Tests =====

#[test]
fn export_empty_db_guard_blocks_overwrite() {
    // Empty database should not overwrite non-empty JSONL without --force
    let storage = SqliteStorage::open_memory().unwrap();
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");

    // Create existing JSONL with content
    let existing = issue_with_id("test-existing", "Existing issue");
    let json = serde_json::to_string(&existing).unwrap();
    fs::write(&path, format!("{json}\n")).unwrap();

    // Try to export empty database (should fail)
    let config = ExportConfig {
        force: false,
        ..Default::default()
    };
    let result = export_to_jsonl(&storage, &path, &config);

    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("empty database"),
        "Expected 'empty database' error, got: {err}"
    );
}

#[test]
fn export_empty_db_guard_bypassed_with_force() {
    // Empty database CAN overwrite non-empty JSONL with --force
    let storage = SqliteStorage::open_memory().unwrap();
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");

    // Create existing JSONL with content
    let existing = issue_with_id("test-existing", "Existing issue");
    let json = serde_json::to_string(&existing).unwrap();
    fs::write(&path, format!("{json}\n")).unwrap();

    // Export empty database with force (should succeed)
    let config = ExportConfig {
        force: true,
        ..Default::default()
    };
    let result = export_to_jsonl(&storage, &path, &config);

    assert!(result.is_ok());
    let export = result.unwrap();
    assert_eq!(export.exported_count, 0);
}

// ===== Tombstone Protection Tests =====

#[test]
fn import_tombstone_protection_prevents_resurrection() {
    // Tombstones in DB should never be resurrected by import
    let mut storage = SqliteStorage::open_memory().unwrap();
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");

    // Create a tombstone in the database
    let mut tombstone = issue_with_id("test-tomb", "Tombstone issue");
    tombstone.status = Status::Tombstone;
    tombstone.deleted_at = Some(Utc::now());
    storage.create_issue(&tombstone, "tester").unwrap();

    // Create JSONL trying to resurrect the tombstone
    let mut incoming = issue_with_id("test-tomb", "Resurrected issue");
    incoming.status = Status::Open;
    incoming.updated_at = Utc::now() + Duration::hours(1);
    let json = serde_json::to_string(&incoming).unwrap();
    fs::write(&path, format!("{json}\n")).unwrap();

    // Import should skip the tombstone
    let result =
        import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-")).unwrap();
    assert_eq!(result.tombstone_skipped, 1);

    // Verify the issue is still a tombstone
    let still_tombstone = storage.get_issue("test-tomb").unwrap().unwrap();
    assert_eq!(still_tombstone.status, Status::Tombstone);
}

fn historical_tombstone(id: &str, revision_hour: i64) -> Issue {
    let mut issue = issue_with_id(id, "Retained tombstone");
    issue.created_at = Utc.with_ymd_and_hms(2026, 11, 12, 0, 0, 0).unwrap();
    issue.updated_at = issue.created_at + Duration::hours(revision_hour);
    issue.status = Status::Tombstone;
    issue.deleted_at = Some(issue.created_at + Duration::hours(2));
    issue
}

fn fill_tombstone_history(issue: &mut Issue) {
    issue.created_by = Some("original-creator".to_string());
    issue.closed_at = Some(issue.created_at + Duration::hours(1));
    issue.close_reason = Some("peer completed the work".to_string());
    issue.closed_by_session = Some("closing-session".to_string());
    issue.closed_by = Some("closing-actor".to_string());
    issue.closed_by_agent_name = Some("closing-agent".to_string());
    issue.closed_by_harness = Some("closing-harness".to_string());
    issue.closed_by_model = Some("closing-model".to_string());
    issue.deleted_by = Some("deleting-actor".to_string());
    issue.delete_reason = Some("retired after completion".to_string());
    issue.original_type = Some("task".to_string());
    issue.bypassed_policy = Some(true);
    issue.bypass_reason = Some("reviewed exception".to_string());
    issue.policy_gates_fired = Some(vec!["review".to_string()]);
}

fn write_issue_record(path: &Path, issue: &Issue) {
    fs::write(path, format!("{}\n", serde_json::to_string(issue).unwrap())).unwrap();
}

fn assert_tombstone_history(actual: &Issue, expected: &Issue) {
    assert_eq!(actual.status, Status::Tombstone);
    assert_eq!(actual.created_by, expected.created_by);
    assert_eq!(actual.closed_at, expected.closed_at);
    assert_eq!(actual.close_reason, expected.close_reason);
    assert_eq!(actual.closed_by_session, expected.closed_by_session);
    assert_eq!(actual.closed_by, expected.closed_by);
    assert_eq!(actual.closed_by_agent_name, expected.closed_by_agent_name);
    assert_eq!(actual.closed_by_harness, expected.closed_by_harness);
    assert_eq!(actual.closed_by_model, expected.closed_by_model);
    assert_eq!(actual.deleted_at, expected.deleted_at);
    assert_eq!(actual.deleted_by, expected.deleted_by);
    assert_eq!(actual.delete_reason, expected.delete_reason);
    assert_eq!(actual.original_type, expected.original_type);
    assert_eq!(actual.bypassed_policy, expected.bypassed_policy);
    assert_eq!(actual.bypass_reason, expected.bypass_reason);
    assert_eq!(actual.policy_gates_fired, expected.policy_gates_fired);
}

#[test]
fn tombstone_import_enriches_older_equal_and_newer_history() {
    for revision_hour in [3, 4, 5] {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let local = historical_tombstone("test-history", 4);
        storage.upsert_issue_for_import(&local).unwrap();
        let mut incoming = historical_tombstone("test-history", revision_hour);
        fill_tombstone_history(&mut incoming);
        incoming.title = "A peer title cannot revive or replace the local tombstone".to_string();
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("issues.jsonl");
        write_issue_record(&path, &incoming);

        let result =
            import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-"))
                .unwrap();
        assert_eq!(result.updated_count, 1, "revision hour {revision_hour}");
        assert_eq!(result.tombstone_enriched, 1, "safe enrichment is reported");
        assert_eq!(result.tombstone_skipped, 0, "safe history was not skipped");
        let enriched = storage.get_issue_for_export(&local.id).unwrap().unwrap();
        assert_tombstone_history(&enriched, &incoming);
        assert_eq!(enriched.title, local.title);
        assert_eq!(
            enriched.updated_at,
            local.updated_at.max(incoming.updated_at)
        );
        assert!(storage.get_events(&local.id, 0).unwrap().is_empty());

        export_to_jsonl(&storage, &path, &ExportConfig::default()).unwrap();
        let exported = read_issues_from_jsonl(&path).unwrap();
        assert_tombstone_history(&exported[0], &incoming);
    }
}

#[test]
fn tombstone_import_retains_known_history_from_a_newer_sparse_peer() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    let mut local = historical_tombstone("test-sparse", 4);
    fill_tombstone_history(&mut local);
    storage.upsert_issue_for_import(&local).unwrap();
    let mut incoming = historical_tombstone("test-sparse", 5);
    incoming.deleted_at = None;
    incoming.closed_by = Some("   ".to_string());
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");
    write_issue_record(&path, &incoming);

    import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-")).unwrap();
    let retained = storage.get_issue_for_export(&local.id).unwrap().unwrap();
    assert_tombstone_history(&retained, &local);
    assert_eq!(retained.updated_at, incoming.updated_at);
    export_to_jsonl(&storage, &path, &ExportConfig::default()).unwrap();
    assert_tombstone_history(&read_issues_from_jsonl(&path).unwrap()[0], &local);
}

#[test]
fn tombstone_import_does_not_invent_an_unknown_closure_timestamp() {
    for bypassed_policy in [None, Some(false)] {
        for policy_gates_fired in [None, Some(Vec::new())] {
            let mut incoming = historical_tombstone("test-never-closed", 4);
            incoming.deleted_at = None;
            incoming.bypassed_policy = bypassed_policy;
            incoming.policy_gates_fired = policy_gates_fired;
            let temp = TempDir::new().unwrap();
            let path = temp.path().join("issues.jsonl");
            write_issue_record(&path, &incoming);
            for _ in 0..2 {
                let mut receiver = SqliteStorage::open_memory().unwrap();
                import_from_jsonl(
                    &mut receiver,
                    &path,
                    &ImportConfig::default(),
                    Some("test-"),
                )
                .unwrap();
                let repeated = import_from_jsonl(
                    &mut receiver,
                    &path,
                    &ImportConfig::default(),
                    Some("test-"),
                )
                .unwrap();
                assert_eq!(repeated.updated_count, 0);
                assert_eq!(repeated.tombstone_enriched, 0);
                let retained = receiver
                    .get_issue_for_export(&incoming.id)
                    .unwrap()
                    .unwrap();
                assert_eq!(retained.closed_at, None);
                assert_eq!(retained.deleted_at, None);
                assert_eq!(retained.closed_by, None);
                assert_eq!(retained.bypassed_policy, None);
                assert_eq!(retained.policy_gates_fired, None);
                export_to_jsonl(&receiver, &path, &ExportConfig::default()).unwrap();
            }
            let exported = read_issues_from_jsonl(&path).unwrap();
            assert_eq!(exported[0].closed_at, None);
            assert_eq!(exported[0].deleted_at, None);
        }
    }
}

#[test]
fn tombstone_import_keeps_closure_evidence_with_an_unknown_timestamp() {
    let mut incoming = historical_tombstone("test-unknown-close-time", 4);
    incoming.close_reason = Some("historical completion with no recorded clock".to_string());
    incoming.closed_by = Some("historical-closer".to_string());
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");
    write_issue_record(&path, &incoming);
    let mut storage = SqliteStorage::open_memory().unwrap();
    import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-")).unwrap();
    export_to_jsonl(&storage, &path, &ExportConfig::default()).unwrap();
    let exported = read_issues_from_jsonl(&path).unwrap();
    assert_eq!(exported[0].closed_at, None);
    assert_eq!(exported[0].close_reason, incoming.close_reason);
    assert_eq!(exported[0].closed_by, incoming.closed_by);
}

#[test]
fn tombstone_import_adds_closure_from_a_live_peer_without_resurrection() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    let mut local = historical_tombstone("test-peer-close", 4);
    local.deleted_by = Some("deleting-actor".to_string());
    local.delete_reason = Some("retired after completion".to_string());
    local.original_type = Some("task".to_string());
    storage.upsert_issue_for_import(&local).unwrap();
    let mut incoming = local.clone();
    fill_tombstone_history(&mut incoming);
    incoming.status = Status::Closed;
    incoming.updated_at += Duration::hours(1);
    incoming.deleted_at = None;
    incoming.deleted_by = None;
    incoming.delete_reason = None;
    incoming.original_type = None;
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");
    write_issue_record(&path, &incoming);

    import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-")).unwrap();
    let retained = storage.get_issue_for_export(&local.id).unwrap().unwrap();
    let mut expected = local;
    fill_tombstone_history(&mut expected);
    assert_tombstone_history(&retained, &expected);
}

#[test]
fn tombstone_resurrection_attempt_can_be_exported_without_advancing_its_clock() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    let mut local = historical_tombstone("test-rejected-live", 4);
    local.bypassed_policy = Some(true);
    local.bypass_reason = Some("historical exception".to_string());
    local.policy_gates_fired = Some(vec!["tests".to_string(), "review".to_string()]);
    storage.upsert_issue_for_import(&local).unwrap();
    let mut incoming = issue_with_id(&local.id, "Attempted resurrection");
    incoming.created_at = local.created_at;
    incoming.updated_at = local.updated_at + Duration::hours(1);
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");
    write_issue_record(&path, &incoming);

    let result =
        import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-")).unwrap();
    assert_eq!(result.tombstone_skipped, 1);
    assert_eq!(result.tombstone_enriched, 0);
    export_to_jsonl(&storage, &path, &ExportConfig::default()).unwrap();
    let exported = read_issues_from_jsonl(&path).unwrap();
    assert_tombstone_history(&exported[0], &local);
    assert_eq!(exported[0].updated_at, local.updated_at);
    assert_eq!(exported[0].title, local.title);
}

#[test]
fn tombstone_alias_enrichment_revokes_a_previously_certified_sparse_record() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    let mut local = historical_tombstone("test-cert-target", 4);
    local.external_ref = Some("HISTORY-CERTIFICATE".to_string());
    storage.upsert_issue_for_import(&local).unwrap();
    let mut exact_alias = local.clone();
    exact_alias.id = "test-cert-alias".to_string();
    let mut enriched = local.clone();
    enriched.external_ref = None;
    fill_tombstone_history(&mut enriched);
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");
    fs::write(
        &path,
        format!(
            "{}\n{}\n",
            serde_json::to_string(&exact_alias).unwrap(),
            serde_json::to_string(&enriched).unwrap()
        ),
    )
    .unwrap();

    for expected_enriched in [1, 0] {
        let result =
            import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-"))
                .unwrap();
        assert_eq!(result.tombstone_enriched, expected_enriched);
        assert_eq!(result.export_hashes_recorded, 0);
        assert!(storage.get_export_hash(&local.id).unwrap().is_none());
        let retained = storage.get_issue_for_export(&local.id).unwrap().unwrap();
        assert_tombstone_history(&retained, &enriched);
        assert_eq!(retained.external_ref, local.external_ref);
        assert!(storage.get_issue(&exact_alias.id).unwrap().is_none());
    }
}

#[test]
fn a_later_alias_cannot_resurrect_a_tombstone_created_in_the_same_import() {
    for force_upsert in [false, true] {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let mut local = issue_with_id("test-batch-delete", "Original live issue");
        local.created_at = Utc.with_ymd_and_hms(2026, 11, 12, 0, 0, 0).unwrap();
        local.updated_at = local.created_at + Duration::hours(3);
        local.external_ref = Some("SAME-IMPORT-DELETION".to_string());
        storage.upsert_issue_for_import(&local).unwrap();
        let mut deletion = historical_tombstone("test-batch-alias", 4);
        fill_tombstone_history(&mut deletion);
        deletion.external_ref = local.external_ref.clone();
        let mut later_live = local.clone();
        later_live.updated_at = local.created_at + Duration::hours(5);
        later_live.external_ref = None;
        later_live.title = "Must not resurrect the accepted deletion".to_string();
        let temp = TempDir::new().unwrap();
        let path = temp.path().join("issues.jsonl");
        fs::write(
            &path,
            format!(
                "{}\n{}\n",
                serde_json::to_string(&deletion).unwrap(),
                serde_json::to_string(&later_live).unwrap()
            ),
        )
        .unwrap();

        let config = ImportConfig {
            force_upsert,
            ..Default::default()
        };
        import_from_jsonl(&mut storage, &path, &config, Some("test-")).unwrap();
        let retained = storage.get_issue_for_export(&local.id).unwrap().unwrap();
        assert_tombstone_history(&retained, &deletion);
        assert_eq!(retained.title, deletion.title);
        assert!(storage.get_issue(&deletion.id).unwrap().is_none());
    }
}

#[test]
fn a_later_update_preserves_history_from_a_pending_skipped_alias() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    let mut local = historical_tombstone("test-pending-history", 3);
    local.status = Status::Closed;
    local.closed_at = Some(local.created_at + Duration::hours(1));
    local.deleted_at = None;
    local.content_hash = Some(local.compute_content_hash());
    storage.upsert_issue_for_import(&local).unwrap();

    let mut deletion = local.clone();
    deletion.status = Status::Tombstone;
    deletion.updated_at = local.created_at + Duration::hours(4);
    deletion.deleted_at = Some(local.created_at + Duration::hours(4));
    let mut older_alias = local.clone();
    older_alias.id = "test-pending-older".to_string();
    older_alias.updated_at = local.created_at + Duration::hours(2);
    older_alias.close_reason = Some("Historical reason from the older alias".to_string());
    let mut later_alias = local.clone();
    later_alias.id = "test-pending-later".to_string();
    later_alias.updated_at = local.created_at + Duration::hours(5);

    // Closure reasons and timestamps are excluded from content hashes. Both
    // aliases therefore resolve to the same original closed issue, while the
    // first row changes its transaction-local status to a tombstone.
    assert_eq!(
        local.compute_content_hash(),
        older_alias.compute_content_hash()
    );
    assert_eq!(
        local.compute_content_hash(),
        later_alias.compute_content_hash()
    );
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");
    fs::write(
        &path,
        format!(
            "{}\n{}\n{}\n",
            serde_json::to_string(&deletion).unwrap(),
            serde_json::to_string(&older_alias).unwrap(),
            serde_json::to_string(&later_alias).unwrap()
        ),
    )
    .unwrap();

    let result =
        import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-")).unwrap();
    assert_eq!(result.tombstone_enriched, 1);
    let retained = storage.get_issue_for_export(&local.id).unwrap().unwrap();
    deletion.close_reason = older_alias.close_reason;
    assert_tombstone_history(&retained, &deletion);
    assert!(storage.get_issue(&older_alias.id).unwrap().is_none());
    assert!(storage.get_issue(&later_alias.id).unwrap().is_none());
}

#[test]
fn a_skipped_older_tombstone_does_not_protect_a_still_live_issue() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    let mut local = issue_with_id("test-older-deletion", "Still live");
    local.created_at = Utc.with_ymd_and_hms(2026, 11, 12, 0, 0, 0).unwrap();
    local.updated_at = local.created_at + Duration::hours(4);
    local.external_ref = Some("OLDER-SKIPPED-DELETION".to_string());
    storage.upsert_issue_for_import(&local).unwrap();
    let mut old_deletion = historical_tombstone("test-older-deletion-alias", 3);
    old_deletion.external_ref = local.external_ref.clone();
    let mut later_live = local.clone();
    later_live.updated_at = local.created_at + Duration::hours(5);
    later_live.external_ref = None;
    later_live.title = "Latest live revision".to_string();
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");
    fs::write(
        &path,
        format!(
            "{}\n{}\n",
            serde_json::to_string(&old_deletion).unwrap(),
            serde_json::to_string(&later_live).unwrap()
        ),
    )
    .unwrap();

    let result =
        import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-")).unwrap();
    assert_eq!(result.tombstone_enriched, 0);
    assert_eq!(result.updated_count, 1);
    let retained = storage.get_issue_for_export(&local.id).unwrap().unwrap();
    assert_eq!(retained.status, Status::Open);
    assert_eq!(retained.title, later_live.title);
    assert_eq!(retained.updated_at, later_live.updated_at);
    assert!(retained.deleted_at.is_none());
    assert!(storage.get_issue(&old_deletion.id).unwrap().is_none());
}

#[test]
fn a_winning_tombstone_preserves_omitted_local_bypass_audit_without_certifying_source() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    let mut local = historical_tombstone("test-audit-certificate", 3);
    local.status = Status::Closed;
    local.deleted_at = None;
    local.closed_at = Some(local.created_at + Duration::hours(1));
    local.closed_by = Some("historical-closer".to_string());
    local.bypassed_policy = Some(true);
    local.bypass_reason = Some("reviewed historical exception".to_string());
    local.policy_gates_fired = Some(vec!["review".to_string()]);
    storage.upsert_issue_for_import(&local).unwrap();
    let mut incoming = local.clone();
    incoming.status = Status::Tombstone;
    incoming.updated_at = local.created_at + Duration::hours(4);
    incoming.deleted_at = Some(incoming.updated_at);
    incoming.bypassed_policy = None;
    incoming.bypass_reason = None;
    incoming.policy_gates_fired = None;
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");
    write_issue_record(&path, &incoming);

    let result =
        import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-")).unwrap();
    assert_eq!(result.updated_count, 1);
    assert_eq!(result.export_hashes_recorded, 0);
    assert!(storage.get_export_hash(&local.id).unwrap().is_none());
    assert_eq!(
        storage.get_metadata("needs_flush").unwrap().as_deref(),
        Some("true")
    );
    let mut expected = incoming;
    expected.bypassed_policy = local.bypassed_policy;
    expected.bypass_reason = local.bypass_reason;
    expected.policy_gates_fired = local.policy_gates_fired;
    let retained = storage.get_issue_for_export(&local.id).unwrap().unwrap();
    assert_tombstone_history(&retained, &expected);

    export_to_jsonl(&storage, &path, &ExportConfig::default()).unwrap();
    let mut peer = SqliteStorage::open_memory().unwrap();
    import_from_jsonl(&mut peer, &path, &ImportConfig::default(), Some("test-")).unwrap();
    assert_tombstone_history(
        &peer.get_issue_for_export(&local.id).unwrap().unwrap(),
        &expected,
    );
}

#[test]
fn tombstone_export_preserves_dependency_facts_omitted_by_a_sparse_source() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    let target = issue_with_id("test-sparse-dep-target", "Historical dependency target");
    storage.create_issue(&target, "tester").unwrap();
    let mut local = historical_tombstone("test-sparse-dependency", 4);
    local.dependencies = vec![Dependency {
        issue_id: local.id.clone(),
        depends_on_id: target.id.clone(),
        dep_type: DependencyType::Related,
        created_at: local.created_at + Duration::minutes(20),
        created_by: Some("historical-dependency-author".to_string()),
        metadata: Some(r#"{"evidence":"reviewed"}"#.to_string()),
        thread_id: Some("historical-thread".to_string()),
    }];
    storage.create_issue(&local, "tester").unwrap();
    let retained = storage.get_issue_for_export(&local.id).unwrap().unwrap();
    let mut incoming = retained.clone();
    incoming.dependencies[0].created_by = None;
    incoming.dependencies[0].metadata = None;
    incoming.dependencies[0].thread_id = None;
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");
    write_issue_record(&path, &incoming);

    // Missing legacy fields are absence of evidence. Export must retain the
    // known actor and metadata, without inventing conflicting import defaults.
    export_to_jsonl(&storage, &path, &ExportConfig::default()).unwrap();
    let exported = read_issues_from_jsonl(&path).unwrap();
    let exported = exported.iter().find(|issue| issue.id == local.id).unwrap();
    assert_eq!(exported.dependencies, retained.dependencies);
    assert_tombstone_history(exported, &retained);
}

#[test]
fn tombstone_import_unions_related_historical_evidence() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    for id in ["test-local-target", "test-peer-target"] {
        storage
            .create_issue(&issue_with_id(id, id), "tester")
            .unwrap();
    }
    let mut local = historical_tombstone("test-relations", 4);
    local.labels = vec!["local-history".to_string()];
    local.comments = vec![Comment {
        id: 1,
        issue_id: local.id.clone(),
        author: "local-author".to_string(),
        body: "Local audit evidence".to_string(),
        created_at: local.created_at + Duration::minutes(10),
    }];
    local.dependencies = vec![Dependency {
        issue_id: local.id.clone(),
        depends_on_id: "test-local-target".to_string(),
        dep_type: DependencyType::Related,
        created_at: local.created_at + Duration::minutes(20),
        created_by: Some("local-author".to_string()),
        metadata: None,
        thread_id: None,
    }];
    storage.create_issue(&local, "tester").unwrap();
    let events_before = storage.get_events(&local.id, 0).unwrap();
    let mut incoming = historical_tombstone("test-relations", 3);
    incoming.labels = vec!["peer-history".to_string()];
    incoming.comments = vec![Comment {
        id: 2,
        issue_id: local.id.clone(),
        author: "peer-author".to_string(),
        body: "Peer audit evidence".to_string(),
        created_at: local.created_at + Duration::minutes(30),
    }];
    incoming.dependencies = vec![Dependency {
        issue_id: local.id.clone(),
        depends_on_id: "test-peer-target".to_string(),
        dep_type: DependencyType::Related,
        created_at: local.created_at + Duration::minutes(40),
        created_by: Some("peer-author".to_string()),
        metadata: None,
        thread_id: None,
    }];
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");
    write_issue_record(&path, &incoming);

    import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-")).unwrap();
    let retained = storage.get_issue_for_export(&local.id).unwrap().unwrap();
    assert_eq!(retained.status, Status::Tombstone);
    assert_eq!(retained.labels, vec!["local-history", "peer-history"]);
    assert_eq!(retained.dependencies.len(), 2);
    assert_eq!(retained.comments.len(), 2);
    assert!(
        retained
            .comments
            .iter()
            .any(|comment| comment.body == "Local audit evidence")
    );
    assert!(
        retained
            .comments
            .iter()
            .any(|comment| comment.body == "Peer audit evidence")
    );
    assert_eq!(storage.get_events(&local.id, 0).unwrap(), events_before);
    export_to_jsonl(&storage, &path, &ExportConfig::default()).unwrap();
    let exported = read_issues_from_jsonl(&path).unwrap();
    let exported = exported.iter().find(|issue| issue.id == local.id).unwrap();
    assert!(retained.sync_equals(exported));
}

#[test]
fn tombstone_history_conflicts_roll_back_every_imported_record() {
    for field in [
        "closed_at",
        "close_reason",
        "closed_by",
        "deleted_at",
        "deleted_by",
        "delete_reason",
        "bypass_reason",
    ] {
        for revision_hour in [3, 4, 5] {
            let mut storage = SqliteStorage::open_memory().unwrap();
            let mut local = historical_tombstone("test-conflict", 4);
            fill_tombstone_history(&mut local);
            storage.upsert_issue_for_import(&local).unwrap();
            let before = storage.get_issue_for_export(&local.id).unwrap().unwrap();
            let before_audit = storage.get_close_metadata(&local.id).unwrap();
            let mut incoming = local.clone();
            incoming.updated_at = incoming.created_at + Duration::hours(revision_hour);
            match field {
                "closed_at" => {
                    incoming.closed_at = Some(incoming.created_at + Duration::minutes(30))
                }
                "close_reason" => {
                    incoming.close_reason = Some("contradictory close reason".to_string())
                }
                "closed_by" => incoming.closed_by = Some("contradictory closer".to_string()),
                "deleted_at" => {
                    incoming.deleted_at = Some(incoming.created_at + Duration::minutes(90))
                }
                "deleted_by" => incoming.deleted_by = Some("contradictory deleter".to_string()),
                "delete_reason" => {
                    incoming.delete_reason = Some("contradictory deletion reason".to_string())
                }
                "bypass_reason" => {
                    incoming.bypass_reason = Some("contradictory waiver".to_string())
                }
                _ => unreachable!(),
            }
            let earlier_record = issue_with_id("test-before-conflict", "Must roll back");
            let temp = TempDir::new().unwrap();
            let path = temp.path().join("issues.jsonl");
            let source = format!(
                "{}\n{}\n",
                serde_json::to_string(&earlier_record).unwrap(),
                serde_json::to_string(&incoming).unwrap()
            );
            fs::write(&path, &source).unwrap();

            let error =
                import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-"))
                    .unwrap_err();
            assert!(
                error.to_string().contains(field),
                "{field}/{revision_hour}: {error}"
            );
            assert_eq!(
                storage.get_issue_for_export(&local.id).unwrap(),
                Some(before)
            );
            assert_eq!(storage.get_close_metadata(&local.id).unwrap(), before_audit);
            assert!(storage.get_issue(&earlier_record.id).unwrap().is_none());
            assert_eq!(fs::read_to_string(&path).unwrap(), source);
        }
    }
}

#[test]
fn tombstone_history_import_export_is_idempotent() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    let local = historical_tombstone("test-repeat", 4);
    storage.upsert_issue_for_import(&local).unwrap();
    let mut incoming = historical_tombstone("test-repeat", 5);
    fill_tombstone_history(&mut incoming);
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");
    write_issue_record(&path, &incoming);
    import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-")).unwrap();
    export_to_jsonl(&storage, &path, &ExportConfig::default()).unwrap();
    let expected_bytes = fs::read(&path).unwrap();
    let expected_audit = storage.get_close_metadata(&local.id).unwrap();

    for _ in 0..3 {
        let result =
            import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-"))
                .unwrap();
        assert_eq!(result.updated_count, 0);
        assert_eq!(result.tombstone_enriched, 0);
        assert_eq!(result.created_count, 0);
        export_to_jsonl(&storage, &path, &ExportConfig::default()).unwrap();
        assert_eq!(fs::read(&path).unwrap(), expected_bytes);
        assert_eq!(
            storage.get_close_metadata(&local.id).unwrap(),
            expected_audit
        );
        assert!(storage.get_events(&local.id, 0).unwrap().is_empty());
    }
}

#[test]
fn tombstone_import_refuses_unrepresentable_partial_bypass_audit() {
    for bypassed_policy in [None, Some(false)] {
        for existing in [false, true] {
            for reason_only in [false, true] {
                let mut storage = SqliteStorage::open_memory().unwrap();
                let mut incoming = historical_tombstone("test-partial-audit", 4);
                if existing {
                    storage.upsert_issue_for_import(&incoming).unwrap();
                }
                let before = storage.get_issue_for_export(&incoming.id).unwrap();
                incoming.bypassed_policy = bypassed_policy;
                if reason_only {
                    incoming.bypass_reason =
                        Some("historical waiver with unknown disposition".to_string());
                } else {
                    incoming.policy_gates_fired = Some(vec!["review".to_string()]);
                }
                let temp = TempDir::new().unwrap();
                let path = temp.path().join("issues.jsonl");
                write_issue_record(&path, &incoming);
                let source = fs::read(&path).unwrap();
                let error =
                    import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-"))
                        .unwrap_err();
                assert!(error.to_string().contains("bypassed_policy"), "{error}");
                assert_eq!(storage.get_issue_for_export(&incoming.id).unwrap(), before);
                assert!(storage.get_close_metadata(&incoming.id).unwrap().is_none());
                assert_eq!(fs::read(&path).unwrap(), source);
            }
        }
    }
}

#[test]
fn tombstone_history_converges_across_three_divergent_peers() {
    let temp = TempDir::new().unwrap();
    let path_a = temp.path().join("a.jsonl");
    let path_b = temp.path().join("b.jsonl");
    let path_c = temp.path().join("c.jsonl");
    let mut a = SqliteStorage::open_memory().unwrap();
    let mut b = SqliteStorage::open_memory().unwrap();
    let mut c = SqliteStorage::open_memory().unwrap();
    let mut at_a = historical_tombstone("test-diverged", 4);
    at_a.deleted_by = Some("deleting-actor".to_string());
    at_a.delete_reason = Some("retired after completion".to_string());
    at_a.original_type = Some("task".to_string());
    let mut at_b = historical_tombstone("test-diverged", 3);
    fill_tombstone_history(&mut at_b);
    at_b.closed_by_session = None;
    at_b.closed_by_model = None;
    at_b.deleted_at = None;
    at_b.deleted_by = None;
    at_b.delete_reason = None;
    at_b.original_type = None;
    let mut at_c = historical_tombstone("test-diverged", 5);
    at_c.closed_at = at_b.closed_at;
    at_c.closed_by_session = Some("closing-session".to_string());
    at_c.closed_by_model = Some("closing-model".to_string());
    at_c.bypassed_policy = Some(true);
    at_c.bypass_reason = at_b.bypass_reason.clone();
    at_c.policy_gates_fired = Some(vec!["tests".to_string()]);
    for (storage, issue, path) in [
        (&mut a, &at_a, &path_a),
        (&mut b, &at_b, &path_b),
        (&mut c, &at_c, &path_c),
    ] {
        storage.upsert_issue_for_import(issue).unwrap();
        export_to_jsonl(storage, path, &ExportConfig::default()).unwrap();
    }
    for (storage, source, target) in [
        (&mut a, &path_b, &path_a),
        (&mut c, &path_a, &path_c),
        (&mut b, &path_c, &path_b),
    ] {
        import_from_jsonl(storage, source, &ImportConfig::default(), Some("test-")).unwrap();
        export_to_jsonl(storage, target, &ExportConfig::default()).unwrap();
    }
    import_from_jsonl(&mut a, &path_b, &ImportConfig::default(), Some("test-")).unwrap();
    export_to_jsonl(&a, &path_a, &ExportConfig::default()).unwrap();
    let mut expected = historical_tombstone("test-diverged", 5);
    fill_tombstone_history(&mut expected);
    expected.policy_gates_fired = Some(vec!["review".to_string(), "tests".to_string()]);
    for storage in [&a, &b, &c] {
        let issue = storage.get_issue_for_export(&expected.id).unwrap().unwrap();
        assert_tombstone_history(&issue, &expected);
        assert_eq!(issue.updated_at, expected.updated_at);
    }
    assert_eq!(fs::read(&path_a).unwrap(), fs::read(&path_b).unwrap());
    assert_eq!(fs::read(&path_a).unwrap(), fs::read(&path_c).unwrap());
}

#[test]
fn tombstone_history_survives_the_next_mutations_automatic_export() {
    let temp = TempDir::new().unwrap();
    let beads_dir = temp.path().join(".beads");
    fs::create_dir(&beads_dir).unwrap();
    let path = beads_dir.join("issues.jsonl");
    let mut storage = SqliteStorage::open_memory().unwrap();
    let local = historical_tombstone("test-autoflush", 4);
    storage.upsert_issue_for_import(&local).unwrap();
    let mut incoming = historical_tombstone("test-autoflush", 5);
    fill_tombstone_history(&mut incoming);
    write_issue_record(&path, &incoming);
    import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-")).unwrap();
    storage
        .create_issue(
            &issue_with_id("test-unrelated", "Unrelated mutation"),
            "tester",
        )
        .unwrap();

    let result = auto_flush(
        &mut storage,
        &beads_dir,
        &path,
        false,
        None,
        beads_rust::sync::history::HistoryConfig::default(),
    )
    .unwrap();
    assert!(result.flushed);
    let exported = read_issues_from_jsonl(&path).unwrap();
    let retained = exported.iter().find(|issue| issue.id == local.id).unwrap();
    assert_tombstone_history(retained, &incoming);
    assert!(exported.iter().any(|issue| issue.id == "test-unrelated"));
}

#[test]
fn export_refuses_to_discard_tombstone_history_and_keeps_source_bytes() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    let local = historical_tombstone("test-export-guard", 4);
    storage.upsert_issue_for_import(&local).unwrap();
    let mut incoming = historical_tombstone("test-export-guard", 5);
    fill_tombstone_history(&mut incoming);
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");
    write_issue_record(&path, &incoming);
    let before = fs::read(&path).unwrap();

    for force in [false, true] {
        let config = ExportConfig {
            force,
            ..Default::default()
        };
        let error = export_to_jsonl(&storage, &path, &config).unwrap_err();
        assert!(error.to_string().contains(&local.id), "{error}");
        assert_eq!(fs::read(&path).unwrap(), before);
    }
    import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-")).unwrap();
    export_to_jsonl(&storage, &path, &ExportConfig::default()).unwrap();
    assert_tombstone_history(&read_issues_from_jsonl(&path).unwrap()[0], &incoming);
}

#[test]
fn expired_tombstone_history_is_imported_before_retention_removes_its_jsonl_record() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    let local = historical_tombstone("test-expired-history", 4);
    storage.upsert_issue_for_import(&local).unwrap();
    let mut incoming = historical_tombstone("test-expired-history", 5);
    fill_tombstone_history(&mut incoming);
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");
    write_issue_record(&path, &incoming);
    let before = fs::read(&path).unwrap();
    let config = ExportConfig {
        retention_days: Some(30),
        export_as_of: Some(local.created_at + Duration::days(40)),
        ..Default::default()
    };

    for force in [false, true] {
        let error = export_to_jsonl(
            &storage,
            &path,
            &ExportConfig {
                force,
                ..config.clone()
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains(&local.id), "{error}");
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-")).unwrap();
    let result = export_to_jsonl(&storage, &path, &config).unwrap();
    assert_eq!(result.exported_count, 0);
    assert!(read_issues_from_jsonl(&path).unwrap().is_empty());
    assert_tombstone_history(
        &storage.get_issue_for_export(&local.id).unwrap().unwrap(),
        &incoming,
    );
}

// ===== Collision Detection Tests =====

#[test]
fn import_collision_by_id_updates_when_newer() {
    // When importing an issue with same ID but newer timestamp, it should update
    let mut storage = SqliteStorage::open_memory().unwrap();
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");

    // Create existing issue with older timestamp
    let mut existing = issue_with_id("test-001", "Old title");
    existing.updated_at = Utc::now() - Duration::hours(1);
    storage.create_issue(&existing, "tester").unwrap();

    // Create JSONL with same ID but newer timestamp
    let mut incoming = issue_with_id("test-001", "New title");
    incoming.updated_at = Utc::now();
    let json = serde_json::to_string(&incoming).unwrap();
    fs::write(&path, format!("{json}\n")).unwrap();

    // Import should update
    let result =
        import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-")).unwrap();
    assert_eq!(result.imported_count, 1);

    // Verify the update
    let updated = storage.get_issue("test-001").unwrap().unwrap();
    assert_eq!(updated.title, "New title");
}

#[test]
fn import_collision_by_id_skips_when_older() {
    // When importing an issue with same ID but older timestamp, it should skip
    let mut storage = SqliteStorage::open_memory().unwrap();
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");

    // Create existing issue with newer timestamp
    let mut existing = issue_with_id("test-001", "Newer title");
    existing.created_at = Utc::now() - Duration::hours(2);
    existing.updated_at = Utc::now();
    storage.create_issue(&existing, "tester").unwrap();

    // Create JSONL with same ID but older timestamp
    let mut incoming = issue_with_id("test-001", "Older title");
    incoming.created_at = existing.created_at;
    incoming.updated_at = Utc::now() - Duration::hours(1);
    let json = serde_json::to_string(&incoming).unwrap();
    fs::write(&path, format!("{json}\n")).unwrap();

    // Import should skip
    let result =
        import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-")).unwrap();
    assert_eq!(result.skipped_count, 1);

    // Verify no change
    let unchanged = storage.get_issue("test-001").unwrap().unwrap();
    assert_eq!(unchanged.title, "Newer title");
}

#[test]
fn import_collision_by_external_ref() {
    // When importing an issue with matching external_ref, it should match (phase 1)
    let mut storage = SqliteStorage::open_memory().unwrap();
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");

    // Create existing issue with external_ref
    let mut existing = issue_with_id("test-001", "Existing");
    existing.external_ref = Some("JIRA-123".to_string());
    existing.updated_at = Utc::now() - Duration::hours(1);
    storage.create_issue(&existing, "tester").unwrap();

    // Create JSONL with SAME external_ref and same ID, newer timestamp
    let mut incoming = issue_with_id("test-001", "Incoming updated");
    incoming.external_ref = Some("JIRA-123".to_string());
    incoming.updated_at = Utc::now();
    let json = serde_json::to_string(&incoming).unwrap();
    fs::write(&path, format!("{json}\n")).unwrap();

    // Import should update (matched by external_ref in phase 1)
    let result =
        import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-")).unwrap();
    assert_eq!(result.imported_count, 1);

    // Verify the update
    let updated = storage.get_issue("test-001").unwrap().unwrap();
    assert_eq!(updated.title, "Incoming updated");
}

// ===== Ephemeral Issue Tests =====

#[test]
fn import_skips_ephemeral_issues() {
    // Ephemeral issues should be skipped during import
    let mut storage = SqliteStorage::open_memory().unwrap();
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");

    // Create JSONL with ephemeral issue
    let mut ephemeral = issue_with_id("test-eph", "Ephemeral issue");
    ephemeral.ephemeral = true;
    let json = serde_json::to_string(&ephemeral).unwrap();
    fs::write(&path, format!("{json}\n")).unwrap();

    // Import should skip
    let result =
        import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-")).unwrap();
    assert_eq!(result.skipped_count, 1);
    assert_eq!(result.imported_count, 0);

    // Verify the issue was not created
    assert!(storage.get_issue("test-eph").unwrap().is_none());
}

// ===== Prefix Validation Tests =====

#[test]
fn import_skip_prefix_validation_allows_mismatch() {
    // With skip_prefix_validation, mismatched prefixes should be allowed
    let mut storage = SqliteStorage::open_memory().unwrap();
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");

    // Create JSONL with different prefix
    let issue = issue_with_id("other-001", "Different prefix");
    let json = serde_json::to_string(&issue).unwrap();
    fs::write(&path, format!("{json}\n")).unwrap();

    // Import with skip_prefix_validation should succeed
    let config = ImportConfig {
        skip_prefix_validation: true,
        ..Default::default()
    };
    let result = import_from_jsonl(&mut storage, &path, &config, Some("test-")).unwrap();
    assert_eq!(result.imported_count, 1);
}

// ===== Deterministic Export Tests =====

#[test]
fn export_produces_deterministic_content_hash() {
    // Multiple exports of the same data should produce the same content hash
    let mut storage = SqliteStorage::open_memory().unwrap();
    let temp = TempDir::new().unwrap();

    // Create test issue
    let issue = issue_with_id("test-det", "Deterministic test");
    storage.create_issue(&issue, "tester").unwrap();

    let config = ExportConfig::default();

    // Export twice to different files
    let path1 = temp.path().join("export1.jsonl");
    let path2 = temp.path().join("export2.jsonl");

    let result1 = export_to_jsonl(&storage, &path1, &config).unwrap();
    let result2 = export_to_jsonl(&storage, &path2, &config).unwrap();

    // Hashes should be identical
    assert_eq!(result1.content_hash, result2.content_hash);
    assert!(!result1.content_hash.is_empty());
}

// ===== Empty Lines Handling Tests =====

#[test]
fn import_handles_empty_lines_gracefully() {
    // JSONL with empty lines interspersed should still import correctly
    let mut storage = SqliteStorage::open_memory().unwrap();
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");

    // Create JSONL with empty lines
    let issue = issue_with_id("test-001", "Valid issue");
    let json = serde_json::to_string(&issue).unwrap();
    let content = format!("\n\n{json}\n\n\n");
    fs::write(&path, content).unwrap();

    // Import should succeed, ignoring empty lines
    let result =
        import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-")).unwrap();
    assert_eq!(result.imported_count, 1);
}

// ===== New Issue Creation Tests =====

#[test]
fn import_creates_new_issues() {
    // New issues (not in DB) should be created
    let mut storage = SqliteStorage::open_memory().unwrap();
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");

    // Create JSONL with new issue
    let issue = issue_with_id("test-new", "Brand new issue");
    let json = serde_json::to_string(&issue).unwrap();
    fs::write(&path, format!("{json}\n")).unwrap();

    // Import should create the issue
    let result =
        import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-")).unwrap();
    assert_eq!(result.imported_count, 1);
    assert_eq!(result.skipped_count, 0);

    // Verify the issue exists
    let created = storage.get_issue("test-new").unwrap().unwrap();
    assert_eq!(created.title, "Brand new issue");
}

#[test]
fn import_repopulates_export_hashes() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");

    // Create and export an issue
    let issue = issue_with_id("test-hash", "Hash Test");
    storage.create_issue(&issue, "tester").unwrap();
    let export_result = export_to_jsonl(&storage, &path, &ExportConfig::default()).unwrap();
    finalize_export(
        &mut storage,
        &export_result,
        Some(&export_result.issue_hashes),
        &path,
    )
    .unwrap();
    let original_hash = export_result.issue_hashes[0].1.clone();

    // Verify hash exists
    assert_eq!(
        storage.get_export_hash("test-hash").unwrap().unwrap().0,
        original_hash
    );

    // Clear hash manually
    storage.clear_all_export_hashes().unwrap();
    assert!(storage.get_export_hash("test-hash").unwrap().is_none());

    // Import the file back
    import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-")).unwrap();

    // Verify hash is restored
    let (restored_hash, _) = storage.get_export_hash("test-hash").unwrap().unwrap();
    assert_eq!(restored_hash, original_hash);
}

#[test]
fn import_deduplicates_export_hash_rebuild_when_multiple_records_target_same_issue() {
    let mut storage = SqliteStorage::open_memory().unwrap();
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");

    let base_time = Utc::now() - Duration::hours(2);
    let mut existing = issue_with_id("test-existing", "Existing");
    existing.created_at = base_time;
    existing.updated_at = base_time;
    existing.external_ref = Some("EXT-1".to_string());
    storage.create_issue(&existing, "tester").unwrap();
    storage
        .set_export_hashes(&[("test-existing".to_string(), "stale-hash".to_string())])
        .unwrap();

    let mut by_external_ref = issue_with_id("test-remap", "Intermediate update");
    by_external_ref.created_at = base_time + Duration::minutes(5);
    by_external_ref.updated_at = base_time + Duration::minutes(10);
    by_external_ref.external_ref = Some("EXT-1".to_string());

    // A later version of the existing issue keeps its creation time; a
    // different `created_at` under the same id is a different issue (#512).
    let mut by_id = issue_with_id("test-existing", "Final update");
    by_id.created_at = base_time;
    by_id.updated_at = base_time + Duration::minutes(20);

    let json = format!(
        "{}\n{}\n",
        serde_json::to_string(&by_external_ref).unwrap(),
        serde_json::to_string(&by_id).unwrap()
    );
    fs::write(&path, json).unwrap();

    import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-")).unwrap();

    assert!(
        storage.get_issue("test-remap").unwrap().is_none(),
        "collision-matched issue should be merged into the existing record"
    );

    let imported = storage.get_issue("test-existing").unwrap().unwrap();
    assert_eq!(imported.title, "Final update");

    let (stored_hash, _) = storage.get_export_hash("test-existing").unwrap().unwrap();
    assert_eq!(Some(stored_hash.as_str()), imported.content_hash.as_deref());
}

#[test]
fn import_rejects_invalid_id_format() {
    // Import now validates issues, so invalid IDs should be rejected.
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("issues.jsonl");
    let issue = issue_with_id("test-INVALID", "Invalid ID");
    let json = serde_json::to_string(&issue).unwrap();
    fs::write(&path, format!("{json}\n")).unwrap();

    let mut storage = SqliteStorage::open_memory().unwrap();
    let result = import_from_jsonl(&mut storage, &path, &ImportConfig::default(), Some("test-"));

    assert!(result.is_err(), "Import should fail for invalid IDs");
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("Validation failed"),
        "Expected validation error, got: {err}"
    );
}
