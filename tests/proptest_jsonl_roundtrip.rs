//! Property-based tests for JSONL export/import round-trip stability.
//!
//! These tests exercise the sync invariant that JSONL export followed by import
//! preserves the canonical issue payload and content hashes.

use beads_rust::model::{Comment, Dependency, DependencyType, Issue, IssueType, Priority, Status};
use beads_rust::storage::SqliteStorage;
use beads_rust::sync::{
    ExportConfig, ImportConfig, export_to_jsonl, import_from_jsonl, read_issues_from_jsonl,
};
use chrono::{Duration, TimeZone, Utc};
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use tempfile::TempDir;
use tracing::info;

#[derive(Debug, Clone)]
struct RoundTripCase {
    source: Issue,
    blocker: Issue,
    labels: Vec<String>,
    comments: Vec<Comment>,
    dependency: Dependency,
}

/// Initialize test logging for proptest.
fn init_test_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init();
}

fn optional_text() -> impl Strategy<Value = Option<String>> {
    prop::option::of("[A-Za-z0-9][A-Za-z0-9 _.,:;!?/-]{0,79}")
}

fn status_strategy() -> impl Strategy<Value = Status> {
    prop_oneof![
        Just(Status::Open),
        Just(Status::InProgress),
        Just(Status::Draft),
        Just(Status::Closed),
    ]
}

fn issue_type_strategy() -> impl Strategy<Value = IssueType> {
    prop_oneof![
        Just(IssueType::Task),
        Just(IssueType::Bug),
        Just(IssueType::Feature),
        Just(IssueType::Epic),
        Just(IssueType::Chore),
        Just(IssueType::Docs),
        Just(IssueType::Question),
    ]
}

fn priority_strategy() -> impl Strategy<Value = Priority> {
    (0i32..=4).prop_map(Priority)
}

#[allow(clippy::too_many_arguments)]
fn make_issue(
    id: String,
    title: String,
    description: Option<String>,
    design: Option<String>,
    acceptance_criteria: Option<String>,
    notes: Option<String>,
    status: Status,
    priority: Priority,
    issue_type: IssueType,
    assignee: Option<String>,
    owner: Option<String>,
    estimated_minutes: Option<i32>,
    created_by: Option<String>,
    external_ref: Option<String>,
    source_system: Option<String>,
    pinned: bool,
    is_template: bool,
    created_offset_secs: i64,
    update_delta_secs: i64,
) -> Issue {
    let created_at =
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap() + Duration::seconds(created_offset_secs);
    let updated_at = created_at + Duration::seconds(update_delta_secs);
    let closed_at = status.is_terminal().then_some(updated_at);

    Issue {
        id,
        content_hash: None,
        title,
        description,
        design,
        acceptance_criteria,
        notes,
        status,
        priority,
        issue_type,
        assignee,
        owner,
        estimated_minutes,
        created_at,
        created_by,
        updated_at,
        closed_at,
        close_reason: closed_at.map(|_| "done".to_string()),
        closed_by_session: None,
        due_at: None,
        defer_until: None,
        external_ref,
        source_system,
        source_repo: Some(".".to_string()),
        source_repo_path: None,
        agent_context: None,
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
        pinned,
        is_template,
        labels: Vec::new(),
        dependencies: Vec::new(),
        comments: Vec::new(),
    }
}

fn canonical_labels(labels: Vec<String>) -> Vec<String> {
    labels
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn hash_map(issue_hashes: &[(String, String)]) -> BTreeMap<String, String> {
    issue_hashes.iter().cloned().collect()
}

fn find_issue<'a>(issues: &'a [Issue], issue_id: &str) -> &'a Issue {
    issues
        .iter()
        .find(|issue| issue.id == issue_id)
        .expect("exported issue exists")
}

fn populate_storage(case: &RoundTripCase) -> SqliteStorage {
    let mut storage = SqliteStorage::open_memory().unwrap();
    storage.create_issue(&case.source, "proptest").unwrap();
    storage.create_issue(&case.blocker, "proptest").unwrap();
    storage
        .sync_labels_for_import(&case.source.id, &case.labels)
        .unwrap();
    storage
        .sync_dependencies_for_import(&case.source.id, std::slice::from_ref(&case.dependency))
        .unwrap();
    storage
        .sync_comments_for_import(&case.source.id, &case.comments)
        .unwrap();
    storage
}

fn assert_mixed_case_known_value_round_trip(
    raw_status: &str,
    raw_issue_type: &str,
    expected_status: &Status,
    expected_issue_type: &IssueType,
    suffix: &str,
) {
    let temp = TempDir::new().unwrap();
    let input_path = temp.path().join("mixed-case.jsonl");
    let export_path = temp.path().join("exported.jsonl");
    let canonical_path = temp.path().join("canonical.jsonl");

    let canonical_issue = make_issue(
        format!("bd-{suffix}"),
        format!("Mixed case import {suffix}"),
        Some("Import should normalize known enum case variants".to_string()),
        None,
        None,
        None,
        expected_status.clone(),
        Priority::HIGH,
        expected_issue_type.clone(),
        Some("proptest".to_string()),
        None,
        None,
        Some("proptest".to_string()),
        None,
        None,
        false,
        false,
        0,
        30,
    );
    let expected_hash = canonical_issue.compute_content_hash();

    let mut incoming = serde_json::to_value(&canonical_issue).unwrap();
    incoming["status"] = serde_json::Value::String(raw_status.to_string());
    incoming["issue_type"] = serde_json::Value::String(raw_issue_type.to_string());
    fs::write(
        &input_path,
        format!("{}\n", serde_json::to_string(&incoming).unwrap()),
    )
    .unwrap();

    let mut imported = SqliteStorage::open_memory().unwrap();
    let import_result = import_from_jsonl(
        &mut imported,
        &input_path,
        &ImportConfig::default(),
        Some("bd-"),
    )
    .unwrap();
    assert_eq!(import_result.imported_count, 1);

    let stored = imported
        .get_issue(&canonical_issue.id)
        .unwrap()
        .expect("mixed-case issue imported");
    assert_eq!(&stored.status, expected_status);
    assert_eq!(&stored.issue_type, expected_issue_type);
    assert_eq!(stored.content_hash.as_deref(), Some(expected_hash.as_str()));

    let export_result = export_to_jsonl(&imported, &export_path, &ExportConfig::default()).unwrap();
    let exported_issues = read_issues_from_jsonl(&export_path).unwrap();
    let exported = find_issue(&exported_issues, &canonical_issue.id);
    assert_eq!(exported.status, stored.status);
    assert_eq!(exported.issue_type, stored.issue_type);
    assert_eq!(
        hash_map(&export_result.issue_hashes).get(&canonical_issue.id),
        Some(&expected_hash)
    );

    let mut canonical_storage = SqliteStorage::open_memory().unwrap();
    canonical_storage
        .create_issue(&canonical_issue, "proptest")
        .unwrap();
    let canonical_export = export_to_jsonl(
        &canonical_storage,
        &canonical_path,
        &ExportConfig::default(),
    )
    .unwrap();

    assert_eq!(export_result.content_hash, canonical_export.content_hash);
    assert_eq!(
        fs::read_to_string(&export_path).unwrap(),
        fs::read_to_string(&canonical_path).unwrap(),
        "mixed-case import should canonicalize to byte-identical JSONL"
    );
}

#[test]
fn jsonl_import_normalizes_mixed_case_known_status_and_issue_type() {
    assert_mixed_case_known_value_round_trip(
        "In_Progress",
        "Bug",
        &Status::InProgress,
        &IssueType::Bug,
        "mixedcase0",
    );
    assert_mixed_case_known_value_round_trip(
        "INPROGRESS",
        "FEATURE",
        &Status::InProgress,
        &IssueType::Feature,
        "mixedcase1",
    );
    assert_mixed_case_known_value_round_trip(
        "DRAFT",
        "Question",
        &Status::Draft,
        &IssueType::Question,
        "mixedcase2",
    );
    assert_mixed_case_known_value_round_trip(
        "TOMBSTONE",
        "Docs",
        &Status::Tombstone,
        &IssueType::Docs,
        "mixedcase3",
    );
    assert_mixed_case_known_value_round_trip(
        "PINNED",
        "Task",
        &Status::Pinned,
        &IssueType::Task,
        "mixedcase4",
    );
}

#[test]
fn jsonl_import_normalizes_custom_status_and_issue_type_case() {
    assert_mixed_case_known_value_round_trip(
        "QaReview",
        "Odd_Type",
        &Status::Custom("qareview".to_string()),
        &IssueType::Custom("odd_type".to_string()),
        "customcase",
    );
}

prop_compose! {
    fn round_trip_case()(
        suffix in "[a-z0-9]{8,12}",
        title in "[A-Za-z0-9][A-Za-z0-9 _.,:;!?/-]{0,79}",
        description in optional_text(),
        design in optional_text(),
        acceptance_criteria in optional_text(),
        notes in optional_text(),
        status in status_strategy(),
        priority in priority_strategy(),
        issue_type in issue_type_strategy(),
        assignee in prop::option::of("[a-z][a-z0-9_-]{0,15}"),
        owner in prop::option::of("[a-z][a-z0-9_-]{0,15}"),
        estimated_minutes in prop::option::of(1i32..=20_000),
        created_by in prop::option::of("[a-z][a-z0-9_-]{0,15}"),
        external_ref in prop::option::of("EXT-[0-9]{1,6}"),
        source_system in prop::option::of("[a-z][a-z0-9_-]{0,15}"),
        pinned in any::<bool>(),
        is_template in any::<bool>(),
        created_offset_secs in 0i64..=100_000,
        update_delta_secs in 0i64..=100_000,
        label in "[a-z][a-z0-9]{2,15}",
        comment_bodies in prop::collection::vec("[A-Za-z0-9][A-Za-z0-9 _.,:;!?/-]{0,79}", 1..=3),
        comment_author in "[a-z][a-z0-9_-]{0,15}",
        dep_type in prop_oneof![
            Just(DependencyType::Blocks),
            Just(DependencyType::ParentChild),
            Just(DependencyType::WaitsFor),
            Just(DependencyType::Related),
        ],
    ) -> RoundTripCase {
        let source_id = format!("bd-{suffix}");
        let blocker_id = format!("bd-target{suffix}");
        let source = make_issue(
            source_id.clone(),
            title,
            description,
            design,
            acceptance_criteria,
            notes,
            status,
            priority,
            issue_type,
            assignee,
            owner,
            estimated_minutes,
            created_by,
            external_ref,
            source_system,
            pinned,
            is_template,
            created_offset_secs,
            update_delta_secs,
        );
        let blocker = make_issue(
            blocker_id.clone(),
            format!("Blocker {suffix}"),
            None,
            None,
            None,
            None,
            Status::Open,
            Priority::LOW,
            IssueType::Task,
            None,
            None,
            None,
            Some("proptest".to_string()),
            None,
            None,
            false,
            false,
            created_offset_secs,
            update_delta_secs,
        );
        let labels = canonical_labels(vec![label]);
        let comment_base = source.created_at + Duration::seconds(1);
        let comments = comment_bodies
            .into_iter()
            .enumerate()
            .map(|(index, body)| Comment {
                id: i64::try_from(index + 1).unwrap(),
                issue_id: source_id.clone(),
                author: comment_author.clone(),
                body,
                created_at: comment_base + Duration::seconds(i64::try_from(index).unwrap()),
            })
            .collect();
        let dependency = Dependency {
            issue_id: source_id,
            depends_on_id: blocker_id,
            dep_type,
            created_at: source.created_at + Duration::seconds(2),
            created_by: Some("proptest".to_string()),
            metadata: Some("{}".to_string()),
            thread_id: Some("roundtrip".to_string()),
        };

        RoundTripCase {
            source,
            blocker,
            labels,
            comments,
            dependency,
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        ..Default::default()
    })]

    /// Property: export -> import -> export preserves JSONL identity, hashes, and relations.
    #[test]
    fn jsonl_round_trip_preserves_identity_hashes_and_relations(case in round_trip_case()) {
        init_test_logging();
        info!(
            issue_id = %case.source.id,
            labels = case.labels.len(),
            comments = case.comments.len(),
            "proptest_jsonl_roundtrip"
        );

        let temp = TempDir::new().unwrap();
        let first_path = temp.path().join("first.jsonl");
        let second_path = temp.path().join("second.jsonl");

        let original = populate_storage(&case);
        let first_export = export_to_jsonl(&original, &first_path, &ExportConfig::default()).unwrap();
        let first_issues = beads_rust::sync::read_issues_from_jsonl(&first_path).unwrap();
        let first_hashes = hash_map(&first_export.issue_hashes);
        let source_hash = first_hashes
            .get(&case.source.id)
            .expect("source hash exported")
            .clone();

        let mut imported = SqliteStorage::open_memory().unwrap();
        let import_result =
            import_from_jsonl(&mut imported, &first_path, &ImportConfig::default(), Some("bd-"))
                .unwrap();
        prop_assert_eq!(import_result.imported_count, 2);

        let second_export = export_to_jsonl(&imported, &second_path, &ExportConfig::default()).unwrap();
        let second_issues = beads_rust::sync::read_issues_from_jsonl(&second_path).unwrap();
        let second_hashes = hash_map(&second_export.issue_hashes);

        prop_assert_eq!(
            first_export.content_hash,
            second_export.content_hash,
            "JSONL file hash should be stable across export/import/export"
        );
        prop_assert_eq!(
            &first_issues,
            &second_issues,
            "serialize -> deserialize -> serialize should preserve canonical issue payloads"
        );
        prop_assert_eq!(
            second_hashes.get(&case.source.id),
            Some(&source_hash),
            "source issue content_hash should be stable across round-trip"
        );

        let imported_source = imported
            .get_issue(&case.source.id)
            .unwrap()
            .expect("source issue imported");
        prop_assert_eq!(
            imported_source.content_hash.as_deref(),
            Some(source_hash.as_str()),
            "imported storage content_hash should match exported canonical hash"
        );

        let first_source = find_issue(&first_issues, &case.source.id);
        let second_source = find_issue(&second_issues, &case.source.id);
        prop_assert!(!first_source.labels.is_empty());
        prop_assert!(!first_source.dependencies.is_empty());
        prop_assert!(!first_source.comments.is_empty());
        prop_assert_eq!(&first_source.labels, &second_source.labels);
        prop_assert_eq!(&first_source.dependencies, &second_source.dependencies);
        prop_assert_eq!(&first_source.comments, &second_source.comments);
    }

    /// Property: importing the same JSONL file twice into the same database is
    /// idempotent. The second import must not duplicate labels, dependencies,
    /// comments, or drift content hashes.
    #[test]
    fn jsonl_reimport_is_idempotent(case in round_trip_case()) {
        init_test_logging();
        info!(
            issue_id = %case.source.id,
            "proptest_jsonl_reimport_idempotent"
        );

        let temp = TempDir::new().unwrap();
        let input_path = temp.path().join("input.jsonl");
        let after_first_path = temp.path().join("after-first.jsonl");
        let after_second_path = temp.path().join("after-second.jsonl");

        let original = populate_storage(&case);
        export_to_jsonl(&original, &input_path, &ExportConfig::default()).unwrap();

        let mut imported = SqliteStorage::open_memory().unwrap();
        let first_import = import_from_jsonl(
            &mut imported,
            &input_path,
            &ImportConfig::default(),
            Some("bd-"),
        )
        .unwrap();
        prop_assert_eq!(first_import.imported_count, 2);

        let first_export =
            export_to_jsonl(&imported, &after_first_path, &ExportConfig::default()).unwrap();
        let first_issues = read_issues_from_jsonl(&after_first_path).unwrap();
        let first_hashes = hash_map(&first_export.issue_hashes);

        let second_import = import_from_jsonl(
            &mut imported,
            &input_path,
            &ImportConfig::default(),
            Some("bd-"),
        )
        .unwrap();
        prop_assert!(
            second_import.imported_count <= first_import.imported_count,
            "re-import should not create more issue rows than first import"
        );

        let second_export =
            export_to_jsonl(&imported, &after_second_path, &ExportConfig::default()).unwrap();
        let second_issues = read_issues_from_jsonl(&after_second_path).unwrap();

        prop_assert_eq!(
            first_export.content_hash,
            second_export.content_hash,
            "second import changed canonical JSONL content hash"
        );
        prop_assert_eq!(
            first_hashes,
            hash_map(&second_export.issue_hashes),
            "second import changed per-issue content hashes"
        );
        prop_assert_eq!(
            fs::read_to_string(&after_first_path).unwrap(),
            fs::read_to_string(&after_second_path).unwrap(),
            "second import changed exported JSONL bytes"
        );
        prop_assert_eq!(
            &first_issues,
            &second_issues,
            "second import changed canonical issue payloads"
        );

        let first_source = find_issue(&first_issues, &case.source.id);
        let second_source = find_issue(&second_issues, &case.source.id);
        prop_assert_eq!(&first_source.labels, &second_source.labels);
        prop_assert_eq!(&first_source.dependencies, &second_source.dependencies);
        prop_assert_eq!(&first_source.comments, &second_source.comments);
    }

    /// Property: importing the same JSONL issue set in a different line order
    /// produces the same canonical database state after export.
    #[test]
    fn jsonl_import_is_invariant_to_issue_line_order(case in round_trip_case()) {
        init_test_logging();
        info!(
            issue_id = %case.source.id,
            dependency_target = %case.blocker.id,
            "proptest_jsonl_import_order_invariance"
        );

        let temp = TempDir::new().unwrap();
        let original_path = temp.path().join("original.jsonl");
        let permuted_path = temp.path().join("permuted.jsonl");
        let original_export_path = temp.path().join("original-export.jsonl");
        let permuted_export_path = temp.path().join("permuted-export.jsonl");

        let original = populate_storage(&case);
        export_to_jsonl(&original, &original_path, &ExportConfig::default()).unwrap();

        let original_jsonl = fs::read_to_string(&original_path).unwrap();
        let mut permuted_lines = original_jsonl.lines().collect::<Vec<_>>();
        permuted_lines.reverse();
        fs::write(&permuted_path, format!("{}\n", permuted_lines.join("\n"))).unwrap();

        let mut original_import = SqliteStorage::open_memory().unwrap();
        let original_result = import_from_jsonl(
            &mut original_import,
            &original_path,
            &ImportConfig::default(),
            Some("bd-"),
        )
        .unwrap();

        let mut permuted_import = SqliteStorage::open_memory().unwrap();
        let permuted_result = import_from_jsonl(
            &mut permuted_import,
            &permuted_path,
            &ImportConfig::default(),
            Some("bd-"),
        )
        .unwrap();

        prop_assert_eq!(original_result.imported_count, 2);
        prop_assert_eq!(permuted_result.imported_count, original_result.imported_count);

        let original_export =
            export_to_jsonl(&original_import, &original_export_path, &ExportConfig::default())
                .unwrap();
        let permuted_export =
            export_to_jsonl(&permuted_import, &permuted_export_path, &ExportConfig::default())
                .unwrap();
        let original_issues = read_issues_from_jsonl(&original_export_path).unwrap();
        let permuted_issues = read_issues_from_jsonl(&permuted_export_path).unwrap();

        prop_assert_eq!(
            original_export.content_hash,
            permuted_export.content_hash,
            "canonical JSONL hash must not depend on input line order"
        );
        prop_assert_eq!(
            hash_map(&original_export.issue_hashes),
            hash_map(&permuted_export.issue_hashes),
            "per-issue export hashes must not depend on input line order"
        );
        prop_assert_eq!(
            &original_issues,
            &permuted_issues,
            "canonical re-export must be byte-equivalent after permuted import"
        );

        let original_source = find_issue(&original_issues, &case.source.id);
        let permuted_source = find_issue(&permuted_issues, &case.source.id);
        prop_assert_eq!(&original_source.labels, &permuted_source.labels);
        prop_assert_eq!(&original_source.dependencies, &permuted_source.dependencies);
        prop_assert_eq!(&original_source.comments, &permuted_source.comments);
    }
}
