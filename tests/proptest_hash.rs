//! Property-based tests for content hashing.
//!
//! Uses proptest to verify that:
//! - Hash output is always valid hex format
//! - Hashing is deterministic
//! - Content changes produce hash changes
//! - Hash is SHA256 (64 hex chars)

use chrono::Utc;
use proptest::prelude::*;
use std::collections::HashSet;
use tracing::info;

use beads_rust::model::{Issue, IssueType, Priority, Status};
use beads_rust::util::{ContentHashable, content_hash, content_hash_from_parts};

/// Initialize test logging for proptest
fn init_test_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init();
}

/// Create a test issue with the given title and description
fn make_issue(title: &str, description: Option<&str>) -> Issue {
    Issue {
        id: "bd-test".to_string(),
        content_hash: None,
        title: title.to_string(),
        description: description.map(ToString::to_string),
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

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 100,
        ..Default::default()
    })]

    /// Property: Hash output is always valid 64-char hex string (SHA256)
    #[test]
    fn hash_valid_hex_format(title in "\\PC{1,200}") {
        init_test_logging();
        info!(
            "proptest_hash_format: title_len={len}",
            len = title.len()
        );

        let issue = make_issue(&title, None);
        let hash = content_hash(&issue);

        info!("proptest_hash_format: hash={hash}");

        prop_assert_eq!(hash.len(), 64, "SHA256 hash should be 64 hex chars");
        prop_assert!(
            hash.chars().all(|c: char| c.is_ascii_hexdigit()),
            "Hash must be valid hex: {hash}"
        );
        // SHA256 hex uses lowercase
        prop_assert!(
            hash.chars().all(|c: char| !c.is_ascii_uppercase()),
            "Hash should be lowercase hex: {hash}"
        );
    }

    /// Property: Hash is deterministic for same issue
    #[test]
    fn hash_deterministic(
        title in "\\PC{1,100}",
        description in proptest::option::of("\\PC{0,200}"),
    ) {
        init_test_logging();
        info!(
            "proptest_hash_deterministic: title_len={len}",
            len = title.len()
        );

        let issue = make_issue(&title, description.as_deref());

        let hash1 = content_hash(&issue);
        let hash2 = content_hash(&issue);

        prop_assert_eq!(hash1, hash2, "Same issue must produce same hash");
    }

    /// Property: Different titles produce different hashes
    #[test]
    fn hash_changes_with_title(
        title1 in "[a-zA-Z0-9 ]{5,50}",
        title2 in "[a-zA-Z0-9 ]{5,50}",
    ) {
        init_test_logging();

        prop_assume!(title1 != title2);

        let issue1 = make_issue(&title1, None);
        let issue2 = make_issue(&title2, None);

        let hash1 = content_hash(&issue1);
        let hash2 = content_hash(&issue2);

        prop_assert_ne!(hash1, hash2, "Different titles should produce different hashes");
    }

    /// Property: ContentHashable trait produces same result as direct function
    #[test]
    fn trait_matches_function(title in "\\PC{1,100}") {
        init_test_logging();

        let issue = make_issue(&title, None);

        let trait_hash = ContentHashable::content_hash(&issue);
        let fn_hash = content_hash(&issue);

        prop_assert_eq!(trait_hash, fn_hash, "Trait and function should produce same hash");
    }

    /// Property: content_hash_from_parts produces same result as content_hash
    #[test]
    fn parts_match_direct(
        title in "\\PC{1,100}",
        description in proptest::option::of("\\PC{0,100}"),
        notes in proptest::option::of("\\PC{0,100}"),
    ) {
        init_test_logging();

        let mut issue = make_issue(&title, description.as_deref());
        issue.notes = notes;

        let direct = content_hash(&issue);
        let from_parts = content_hash_from_parts(
            &issue.title,
            issue.description.as_deref(),
            issue.design.as_deref(),
            issue.acceptance_criteria.as_deref(),
            issue.notes.as_deref(),
            &issue.status,
            &issue.priority,
            &issue.issue_type,
            issue.assignee.as_deref(),
            issue.owner.as_deref(),
            issue.estimated_minutes,
            issue.due_at,
            issue.defer_until,
            issue.external_ref.as_deref(),
            issue.close_reason.as_deref(),
            issue.pinned,
            issue.is_template,
        );

        prop_assert_eq!(direct, from_parts, "Direct and from_parts should match");
    }

    /// Property: Hash changes when status changes
    #[test]
    fn hash_changes_with_status(title in "\\PC{1,50}") {
        init_test_logging();

        let mut issue = make_issue(&title, None);
        let hash_open = content_hash(&issue);

        issue.status = Status::Closed;
        let hash_closed = content_hash(&issue);

        prop_assert_ne!(hash_open, hash_closed, "Status change should change hash");
    }

    /// Property: Hash changes when priority changes
    #[test]
    fn hash_changes_with_priority(title in "\\PC{1,50}") {
        init_test_logging();

        let mut issue = make_issue(&title, None);
        let hash_p2 = content_hash(&issue);

        issue.priority = Priority::CRITICAL;
        let hash_p0 = content_hash(&issue);

        prop_assert_ne!(hash_p2, hash_p0, "Priority change should change hash");
    }

    /// Property: Hash changes when pinned flag changes
    #[test]
    fn hash_changes_with_pinned(title in "\\PC{1,50}") {
        init_test_logging();

        let mut issue = make_issue(&title, None);
        let hash_unpinned = content_hash(&issue);

        issue.pinned = true;
        let hash_pinned = content_hash(&issue);

        prop_assert_ne!(hash_unpinned, hash_pinned, "Pinned change should change hash");
    }

    /// Property: Hash ignores timestamp changes
    #[test]
    fn hash_ignores_timestamps(title in "\\PC{1,50}") {
        init_test_logging();

        let mut issue = make_issue(&title, None);
        let hash1 = content_hash(&issue);

        // Change timestamps
        issue.updated_at = Utc::now();
        let hash2 = content_hash(&issue);

        prop_assert_eq!(hash1, hash2, "Timestamp changes should not affect hash");
    }
}

/// Property: Low collision rate in batch hashing
#[test]
fn hash_low_collision_rate() {
    init_test_logging();
    info!("proptest_hash_collision: starting collision test");

    let mut hashes = HashSet::new();
    let batch_size = 1000;

    for i in 0..batch_size {
        let title = format!("Unique Issue Title Number {i} with extra text");
        let issue = make_issue(&title, Some(&format!("Description for issue {i}")));
        let hash = content_hash(&issue);

        assert!(
            !hashes.contains(&hash),
            "Collision detected at iteration {i}: hash={hash}"
        );
        hashes.insert(hash);
    }

    assert_eq!(
        hashes.len(),
        batch_size,
        "Should have {batch_size} unique hashes"
    );
    info!("proptest_hash_collision: PASS - {batch_size} unique hashes, 0 collisions");
}

/// Property: Hash is stable across issue type changes
#[test]
fn hash_changes_with_issue_type() {
    init_test_logging();

    let mut issue = make_issue("Test Issue", None);
    let hash_task = content_hash(&issue);

    issue.issue_type = IssueType::Bug;
    let hash_bug = content_hash(&issue);

    issue.issue_type = IssueType::Feature;
    let hash_feature = content_hash(&issue);

    assert_ne!(hash_task, hash_bug, "Task vs Bug should differ");
    assert_ne!(hash_task, hash_feature, "Task vs Feature should differ");
    assert_ne!(hash_bug, hash_feature, "Bug vs Feature should differ");

    info!("proptest_hash_type: PASS - different types produce different hashes");
}
