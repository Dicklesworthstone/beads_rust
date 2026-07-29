//! Metamorphic property test: claimed issues disappear from ready list.
//!
//! If issue x is in `ready()` and we claim x (set status=in_progress + assignee),
//! then x must NOT appear in subsequent `ready()` results.

use beads_rust::model::{Issue, IssueType, Priority, Status};
use beads_rust::storage::{IssueUpdate, ReadyFilters, ReadySortPolicy, SqliteStorage};
use chrono::{TimeZone, Utc};
use proptest::prelude::*;

fn make_open_issue(suffix: &str, title: &str, priority: Priority) -> Issue {
    let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    Issue {
        id: format!("bd-{suffix}"),
        content_hash: None,
        title: title.to_string(),
        description: None,
        design: None,
        acceptance_criteria: None,
        notes: None,
        status: Status::Open,
        priority,
        issue_type: IssueType::Task,
        assignee: None,
        owner: None,
        estimated_minutes: None,
        created_at: now,
        created_by: Some("proptest".to_string()),
        updated_at: now,
        closed_at: None,
        close_reason: None,
        closed_by_session: None,
        due_at: None,
        defer_until: None,
        external_ref: None,
        source_system: None,
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
        pinned: false,
        is_template: false,
        labels: Vec::new(),
        dependencies: Vec::new(),
        comments: Vec::new(),
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        ..Default::default()
    })]

    #[test]
    fn claimed_issue_excluded_from_ready(
        suffix in "[a-z0-9]{6,10}",
        title in "[A-Za-z][A-Za-z0-9 ]{1,40}",
        actor in "[a-z]{3,10}",
        priority in (0i32..=4).prop_map(Priority),
    ) {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue = make_open_issue(&suffix, &title, priority);
        let issue_id = issue.id.clone();
        storage.create_issue(&issue, "proptest").unwrap();

        let ready_before = storage
            .get_ready_issues(&ReadyFilters::default(), ReadySortPolicy::Priority)
            .unwrap();
        let found_before = ready_before.iter().any(|i| i.id == issue_id);
        prop_assert!(found_before, "Open unblocked issue {} should be in ready list", issue_id);

        storage
            .update_issue(
                &issue_id,
                &IssueUpdate {
                    status: Some(Status::InProgress),
                    assignee: Some(Some(actor.clone())),
                    claim_actor: Some(actor),
                    ..Default::default()
                },
                "proptest",
            )
            .unwrap();

        let ready_after = storage
            .get_ready_issues(&ReadyFilters::default(), ReadySortPolicy::Priority)
            .unwrap();
        prop_assert!(
            ready_after.iter().all(|i| i.id != issue_id),
            "Claimed issue {} still appears in ready list after claim",
            issue_id,
        );
    }

    #[test]
    fn closing_issue_excludes_from_ready(
        suffix in "[a-z0-9]{6,10}",
        title in "[A-Za-z][A-Za-z0-9 ]{1,40}",
        priority in (0i32..=4).prop_map(Priority),
    ) {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue = make_open_issue(&suffix, &title, priority);
        let issue_id = issue.id.clone();
        storage.create_issue(&issue, "proptest").unwrap();

        storage
            .update_issue(
                &issue_id,
                &IssueUpdate {
                    status: Some(Status::Closed),
                    close_reason: Some(Some("done".to_string())),
                    ..Default::default()
                },
                "proptest",
            )
            .unwrap();

        let ready_after = storage
            .get_ready_issues(&ReadyFilters::default(), ReadySortPolicy::Priority)
            .unwrap();
        prop_assert!(
            ready_after.iter().all(|i| i.id != issue_id),
            "Closed issue {} still appears in ready list",
            issue_id,
        );
    }
}
