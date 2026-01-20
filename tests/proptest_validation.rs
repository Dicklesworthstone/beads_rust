//! Property-based tests for issue validation.
//!
//! Uses proptest to verify that:
//! - Valid issues always pass validation
//! - Invalid priorities fail validation
//! - Empty titles fail validation
//! - Timestamp invariants are enforced

use chrono::{TimeZone, Utc};
use proptest::prelude::*;
use tracing::info;

use beads_rust::model::{Issue, IssueType, Priority, Status};
use beads_rust::validation::{IssueValidator, LabelValidator};

/// Initialize test logging for proptest
fn init_test_logging() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_test_writer()
        .try_init();
}

/// Create a valid test issue with the given title
fn make_valid_issue(title: &str) -> Issue {
    let now = Utc::now();
    Issue {
        id: "bd-test123".to_string(),
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
        created_at: now,
        created_by: None,
        updated_at: now,
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

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 100,
        ..Default::default()
    })]

    /// Property: Valid issues with good titles always pass validation
    #[test]
    fn valid_issue_passes(title in "[a-zA-Z0-9 ]{1,100}") {
        init_test_logging();
        info!("proptest_valid_issue: title_len={len}", len = title.len());

        // Skip if title is whitespace-only after generation
        prop_assume!(!title.trim().is_empty());

        let issue = make_valid_issue(&title);
        let result = IssueValidator::validate(&issue);

        prop_assert!(
            result.is_ok(),
            "Valid issue should pass validation: {result:?}"
        );
    }

    /// Property: Invalid priority (> 4) fails validation
    #[test]
    fn invalid_priority_fails(priority in 5i32..100i32) {
        init_test_logging();
        info!("proptest_invalid_priority: priority={priority}");

        let mut issue = make_valid_issue("Test Issue");
        issue.priority = Priority(priority);

        let result = IssueValidator::validate(&issue);

        prop_assert!(result.is_err(), "Priority {priority} should fail validation");
        let errors = result.unwrap_err();
        prop_assert!(
            errors.iter().any(|e| e.field == "priority"),
            "Should have priority error"
        );
    }

    /// Property: Negative priority fails validation
    #[test]
    fn negative_priority_fails(priority in -100i32..-1i32) {
        init_test_logging();
        info!("proptest_negative_priority: priority={priority}");

        let mut issue = make_valid_issue("Test Issue");
        issue.priority = Priority(priority);

        let result = IssueValidator::validate(&issue);

        prop_assert!(result.is_err(), "Priority {priority} should fail validation");
    }

    /// Property: Valid priority (0-4) passes validation
    #[test]
    fn valid_priority_passes(priority in 0i32..=4i32) {
        init_test_logging();
        info!("proptest_valid_priority: priority={priority}");

        let mut issue = make_valid_issue("Test Issue");
        issue.priority = Priority(priority);

        let result = IssueValidator::validate(&issue);

        prop_assert!(result.is_ok(), "Priority {priority} should pass validation");
    }

    /// Property: Empty title fails validation
    #[test]
    fn empty_title_fails(whitespace in "\\s{0,10}") {
        init_test_logging();
        info!(
            "proptest_empty_title: whitespace_len={len}",
            len = whitespace.len()
        );

        let mut issue = make_valid_issue("Valid");
        issue.title = whitespace;

        let result = IssueValidator::validate(&issue);

        prop_assert!(result.is_err(), "Empty/whitespace title should fail");
        let errors = result.unwrap_err();
        prop_assert!(
            errors.iter().any(|e| e.field == "title"),
            "Should have title error"
        );
    }

    /// Property: Title over 500 chars fails validation
    #[test]
    fn long_title_fails(len in 501usize..600usize) {
        init_test_logging();
        info!("proptest_long_title: len={len}");

        let mut issue = make_valid_issue("Valid");
        issue.title = "x".repeat(len);

        let result = IssueValidator::validate(&issue);

        prop_assert!(result.is_err(), "Title with {len} chars should fail");
        let errors = result.unwrap_err();
        prop_assert!(
            errors.iter().any(|e| e.field == "title"),
            "Should have title error"
        );
    }

    /// Property: Title up to 500 chars passes validation
    #[test]
    fn title_at_limit_passes(len in 1usize..=500usize) {
        init_test_logging();
        info!("proptest_title_limit: len={len}");

        let mut issue = make_valid_issue("Valid");
        issue.title = "x".repeat(len);

        let result = IssueValidator::validate(&issue);

        prop_assert!(result.is_ok(), "Title with {len} chars should pass");
    }

    /// Property: Description over 100KB fails validation
    #[test]
    fn large_description_fails(extra_bytes in 1usize..1000usize) {
        init_test_logging();
        let len = 102_400 + extra_bytes;
        info!("proptest_large_desc: len={len}");

        let mut issue = make_valid_issue("Test Issue");
        issue.description = Some("x".repeat(len));

        let result = IssueValidator::validate(&issue);

        prop_assert!(result.is_err(), "Description with {len} bytes should fail");
        let errors = result.unwrap_err();
        prop_assert!(
            errors.iter().any(|e| e.field == "description"),
            "Should have description error"
        );
    }

    /// Property: updated_at before created_at fails validation
    #[test]
    fn updated_before_created_fails(days_before in 1u32..100u32) {
        init_test_logging();
        info!("proptest_timestamp_order: days_before={days_before}");

        let mut issue = make_valid_issue("Test Issue");
        issue.created_at = Utc.with_ymd_and_hms(2026, 6, 15, 12, 0, 0).unwrap();
        issue.updated_at = issue.created_at - chrono::Duration::days(i64::from(days_before));

        let result = IssueValidator::validate(&issue);

        prop_assert!(result.is_err(), "updated_at before created_at should fail");
        let errors = result.unwrap_err();
        prop_assert!(
            errors.iter().any(|e| e.field == "updated_at"),
            "Should have updated_at error"
        );
    }

    /// Property: Valid label format passes validation
    #[test]
    fn valid_label_passes(label in "[a-zA-Z0-9_:-]{1,50}") {
        init_test_logging();
        info!("proptest_valid_label: label={label}");

        let result = LabelValidator::validate(&label);

        prop_assert!(result.is_ok(), "Label '{label}' should pass validation");
    }

    /// Property: Label with spaces fails validation
    #[test]
    fn label_with_space_fails(
        prefix in "[a-z]{1,10}",
        suffix in "[a-z]{1,10}",
    ) {
        init_test_logging();
        let label = format!("{prefix} {suffix}");
        info!("proptest_label_space: label={label}");

        let result = LabelValidator::validate(&label);

        prop_assert!(result.is_err(), "Label with space should fail: '{label}'");
    }

    /// Property: Empty label fails validation
    #[test]
    fn empty_label_fails(_dummy in 0..1u8) {
        init_test_logging();

        let result = LabelValidator::validate("");

        prop_assert!(result.is_err(), "Empty label should fail");
    }

    /// Property: Label over 50 chars fails validation
    #[test]
    fn long_label_fails(len in 51usize..100usize) {
        init_test_logging();
        let label = "x".repeat(len);
        info!("proptest_long_label: len={len}");

        let result = LabelValidator::validate(&label);

        prop_assert!(result.is_err(), "Label with {len} chars should fail");
    }

    /// Property: External ref with whitespace fails validation
    #[test]
    fn external_ref_whitespace_fails(
        prefix in "[a-z]{1,10}",
        suffix in "[a-z]{1,10}",
    ) {
        init_test_logging();
        let external_ref = format!("{prefix} {suffix}");
        info!("proptest_external_ref: external_ref={external_ref}");

        let mut issue = make_valid_issue("Test Issue");
        issue.external_ref = Some(external_ref);

        let result = IssueValidator::validate(&issue);

        prop_assert!(result.is_err(), "External ref with whitespace should fail");
        let errors = result.unwrap_err();
        prop_assert!(
            errors.iter().any(|e| e.field == "external_ref"),
            "Should have external_ref error"
        );
    }

    /// Property: Valid external ref without whitespace passes validation
    #[test]
    fn valid_external_ref_passes(external_ref in "[a-zA-Z0-9_/-]{1,50}") {
        init_test_logging();
        info!("proptest_valid_external_ref: external_ref={external_ref}");

        let mut issue = make_valid_issue("Test Issue");
        issue.external_ref = Some(external_ref.clone());

        let result = IssueValidator::validate(&issue);

        prop_assert!(result.is_ok(), "Valid external ref should pass: '{external_ref}'");
    }
}

/// Property: All standard statuses are valid for issues
#[test]
fn all_standard_statuses_valid() {
    init_test_logging();
    info!("proptest_statuses: testing all standard statuses");

    let statuses = [
        Status::Open,
        Status::InProgress,
        Status::Blocked,
        Status::Deferred,
        Status::Closed,
        Status::Tombstone,
        Status::Pinned,
    ];

    for status in statuses {
        let mut issue = make_valid_issue("Test Issue");
        issue.status = status.clone();

        let result = IssueValidator::validate(&issue);
        assert!(result.is_ok(), "Status {status:?} should be valid");
    }

    info!("proptest_statuses: PASS - all standard statuses valid");
}

/// Property: All standard issue types are valid
#[test]
fn all_standard_types_valid() {
    init_test_logging();
    info!("proptest_types: testing all standard issue types");

    let types = [
        IssueType::Task,
        IssueType::Bug,
        IssueType::Feature,
        IssueType::Epic,
        IssueType::Chore,
        IssueType::Docs,
        IssueType::Question,
    ];

    for issue_type in types {
        let mut issue = make_valid_issue("Test Issue");
        issue.issue_type = issue_type.clone();

        let result = IssueValidator::validate(&issue);
        assert!(result.is_ok(), "IssueType {issue_type:?} should be valid");
    }

    info!("proptest_types: PASS - all standard types valid");
}
