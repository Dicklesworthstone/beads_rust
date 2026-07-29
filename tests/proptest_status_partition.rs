//! Metamorphic property test: status partition is exhaustive and disjoint.
//!
//! For any set of issues, querying each known status individually and taking
//! the union must yield exactly the same set as querying with no status filter.
//! This is the TLP (Ternary Logic Partitioning) pattern applied to the issue
//! tracker's status dimension.

use beads_rust::model::{Issue, IssueType, Priority, Status};
use beads_rust::storage::{ListFilters, SqliteStorage};
use chrono::{TimeZone, Utc};
use proptest::prelude::*;
use std::collections::BTreeSet;

fn status_strategy() -> impl Strategy<Value = Status> {
    prop_oneof![
        Just(Status::Open),
        Just(Status::InProgress),
        Just(Status::Blocked),
        Just(Status::Deferred),
        Just(Status::Draft),
        Just(Status::Closed),
        Just(Status::Tombstone),
        Just(Status::Pinned),
    ]
}

fn priority_strategy() -> impl Strategy<Value = Priority> {
    (0i32..=4).prop_map(Priority)
}

fn issue_type_strategy() -> impl Strategy<Value = IssueType> {
    prop_oneof![
        Just(IssueType::Task),
        Just(IssueType::Bug),
        Just(IssueType::Feature),
    ]
}

fn make_issue(
    suffix: &str,
    title: &str,
    status: Status,
    priority: Priority,
    issue_type: IssueType,
) -> Issue {
    let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
    let closed_at = status.is_terminal().then_some(now);
    Issue {
        id: format!("bd-{suffix}"),
        content_hash: None,
        title: title.to_string(),
        description: None,
        design: None,
        acceptance_criteria: None,
        notes: None,
        status,
        priority,
        issue_type,
        assignee: None,
        owner: None,
        estimated_minutes: None,
        created_at: now,
        created_by: Some("proptest".to_string()),
        updated_at: now,
        closed_at,
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

const ALL_KNOWN_STATUSES: &[Status] = &[
    Status::Open,
    Status::InProgress,
    Status::Blocked,
    Status::Deferred,
    Status::Draft,
    Status::Closed,
    Status::Tombstone,
    Status::Pinned,
];

prop_compose! {
    fn issue_spec()(
        status in status_strategy(),
        priority in priority_strategy(),
        issue_type in issue_type_strategy(),
    ) -> (Status, Priority, IssueType) {
        (status, priority, issue_type)
    }
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 64,
        ..Default::default()
    })]

    /// Metamorphic relation: the union of per-status queries equals the full list.
    ///
    /// This is the TLP (Ternary Logic Partitioning) property: partitioning the
    /// issue set by status must be both exhaustive (every issue appears) and
    /// disjoint (no issue appears in two partitions).
    #[test]
    fn status_partition_is_exhaustive_and_disjoint(
        specs in prop::collection::vec(issue_spec(), 2..=12),
    ) {
        let mut storage = SqliteStorage::open_memory().unwrap();

        for (i, (status, priority, issue_type)) in specs.iter().enumerate() {
            let issue = make_issue(
                &format!("part{i:03}"),
                &format!("Partition test {i}"),
                status.clone(),
                *priority,
                issue_type.clone(),
            );
            storage.create_issue(&issue, "proptest").unwrap();
        }

        let count = specs.len();

        // Explicit enumeration of all statuses to get the true universe,
        // including tombstones (which list_issues hides by default even with
        // include_closed=true — by design, since tombstones are soft-deleted).
        let all_issues = storage
            .list_issues(&ListFilters {
                statuses: Some(ALL_KNOWN_STATUSES.to_vec()),
                include_closed: true,
                include_deferred: true,
                include_templates: true,
                ..Default::default()
            })
            .unwrap();

        // Per-status queries
        let mut partitioned_ids = BTreeSet::new();
        let mut partition_total = 0usize;

        for status in ALL_KNOWN_STATUSES {
            let subset = storage
                .list_issues(&ListFilters {
                    statuses: Some(vec![status.clone()]),
                    include_closed: true,
                    include_deferred: true,
                    include_templates: true,
                    ..Default::default()
                })
                .unwrap();

            for issue in &subset {
                prop_assert_eq!(
                    &issue.status,
                    status,
                    "Issue {} has status {:?} but was returned by status={:?} query",
                    issue.id,
                    issue.status,
                    status,
                );
                let is_new = partitioned_ids.insert(issue.id.clone());
                prop_assert!(
                    is_new,
                    "Issue {} appeared in multiple status partitions",
                    issue.id,
                );
            }
            partition_total += subset.len();
        }

        let all_ids: BTreeSet<_> = all_issues.iter().map(|i| i.id.clone()).collect();

        // Exhaustive: every issue in "all" appears in exactly one partition
        for id in &all_ids {
            prop_assert!(
                partitioned_ids.contains(id),
                "Issue {} in list(all) but missing from all status partitions",
                id,
            );
        }

        // No extras: every partitioned issue appears in "all"
        for id in &partitioned_ids {
            prop_assert!(
                all_ids.contains(id),
                "Issue {} in status partition but missing from list(all)",
                id,
            );
        }

        // Disjoint: partition_total == partitioned_ids.len() (no duplicates)
        prop_assert_eq!(
            partition_total,
            partitioned_ids.len(),
            "Duplicate issues detected across partitions",
        );

        // Total count matches
        prop_assert_eq!(
            partitioned_ids.len(),
            all_ids.len(),
            "Partition count {} != list(all) count {}",
            partitioned_ids.len(),
            all_ids.len(),
        );
        prop_assert!(
            count <= all_ids.len(),
            "Created {} issues but list(all) returned {}",
            count,
            all_ids.len(),
        );
    }
}
