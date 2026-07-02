//! Property-based tests for `merge_issue` and `three_way_merge`.
//!
//! Verifies structural invariants of the 3-way merge algorithm:
//! - determinism (same inputs → same output)
//! - base-only deletion (deleted in both sides → Delete)
//! - uncontested additions (new on one side only → Keep)
//! - tombstone protection (tombstoned issues cannot be resurrected)
//! - conflict detection under Manual strategy

use beads_rust::model::{Issue, IssueType, Priority, Status};
use beads_rust::sync::{
    ConflictResolution, ConflictType, MergeContext, MergeResult, merge_issue, three_way_merge,
};
use chrono::{Duration, TimeZone, Utc};
use proptest::prelude::*;
use std::collections::{HashMap, HashSet};

fn status_strategy() -> impl Strategy<Value = Status> {
    prop_oneof![
        Just(Status::Open),
        Just(Status::InProgress),
        Just(Status::Draft),
        Just(Status::Closed),
    ]
}

fn status_with_custom_strategy() -> impl Strategy<Value = Status> {
    prop_oneof![
        Just(Status::Open),
        Just(Status::InProgress),
        Just(Status::Draft),
        Just(Status::Closed),
        "[a-z_]{3,15}".prop_map(Status::Custom),
    ]
}

fn issue_type_strategy() -> impl Strategy<Value = IssueType> {
    prop_oneof![
        Just(IssueType::Task),
        Just(IssueType::Bug),
        Just(IssueType::Feature),
        Just(IssueType::Epic),
        Just(IssueType::Chore),
    ]
}

fn priority_strategy() -> impl Strategy<Value = Priority> {
    (0i32..=4).prop_map(Priority)
}

fn strategy_strategy() -> impl Strategy<Value = ConflictResolution> {
    prop_oneof![
        Just(ConflictResolution::PreferLocal),
        Just(ConflictResolution::PreferExternal),
        Just(ConflictResolution::PreferNewer),
        Just(ConflictResolution::Manual),
    ]
}

fn make_issue(
    id: &str,
    title: &str,
    status: Status,
    priority: Priority,
    issue_type: IssueType,
    offset_secs: i64,
) -> Issue {
    let created_at =
        Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap() + Duration::seconds(offset_secs);
    Issue {
        id: id.to_string(),
        title: title.to_string(),
        status,
        priority,
        issue_type,
        created_at,
        updated_at: created_at,
        created_by: Some("test".to_string()),
        source_repo: Some(".".to_string()),
        ..Issue::default()
    }
}

fn notes_bytes(notes: &[(String, String)]) -> String {
    let mut bytes = String::new();
    for (id, note) in notes {
        bytes.push_str(id);
        bytes.push('\0');
        bytes.push_str(note);
        bytes.push('\n');
    }
    bytes
}

prop_compose! {
    fn arb_issue(id_suffix: &'static str)(
        title in "[A-Za-z0-9][A-Za-z0-9 ]{0,30}",
        status in status_strategy(),
        priority in priority_strategy(),
        issue_type in issue_type_strategy(),
        offset in 0i64..=100_000,
    ) -> Issue {
        make_issue(
            &format!("bd-{id_suffix}"),
            &title,
            status,
            priority,
            issue_type,
            offset,
        )
    }
}

prop_compose! {
    fn arb_modified_issue(base: Issue)(
        new_title in "[A-Za-z0-9][A-Za-z0-9 ]{0,30}",
        new_status in status_strategy(),
        new_priority in priority_strategy(),
        delta in 1i64..=10_000,
    ) -> Issue {
        let mut modified = base.clone();
        modified.title = new_title;
        modified.status = new_status;
        modified.priority = new_priority;
        modified.updated_at = base.updated_at + Duration::seconds(delta);
        modified
    }
}

// ---------------------------------------------------------------------------
// merge_issue properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(200))]

    /// Determinism: same inputs always produce the same result.
    #[test]
    fn merge_is_deterministic(
        base in prop::option::of(arb_issue("det")),
        left in prop::option::of(arb_issue("det")),
        right in prop::option::of(arb_issue("det")),
        strategy in strategy_strategy(),
    ) {
        let r1 = merge_issue(base.as_ref(), left.as_ref(), right.as_ref(), strategy);
        let r2 = merge_issue(base.as_ref(), left.as_ref(), right.as_ref(), strategy);
        prop_assert_eq!(r1, r2);
    }

    /// All-None → NoAction (no issue exists anywhere).
    #[test]
    fn merge_all_none_is_no_action(strategy in strategy_strategy()) {
        let result = merge_issue(None, None, None, strategy);
        prop_assert_eq!(result, MergeResult::NoAction);
    }

    /// Base-only (deleted from both sides) → Delete.
    #[test]
    fn merge_base_only_is_delete(
        base in arb_issue("base"),
        strategy in strategy_strategy(),
    ) {
        let result = merge_issue(Some(&base), None, None, strategy);
        prop_assert_eq!(result, MergeResult::Delete);
    }

    /// New on left only (no base, no right) → Keep.
    #[test]
    fn merge_new_left_only_is_keep(
        left in arb_issue("left"),
        strategy in strategy_strategy(),
    ) {
        let result = merge_issue(None, Some(&left), None, strategy);
        match result {
            MergeResult::Keep(kept) => prop_assert_eq!(kept.id, left.id),
            other => prop_assert!(false, "Expected Keep, got {:?}", other),
        }
    }

    /// New on right only (no base, no left) → Keep.
    #[test]
    fn merge_new_right_only_is_keep(
        right in arb_issue("right"),
        strategy in strategy_strategy(),
    ) {
        let result = merge_issue(None, None, Some(&right), strategy);
        match result {
            MergeResult::Keep(kept) => prop_assert_eq!(kept.id, right.id),
            other => prop_assert!(false, "Expected Keep, got {:?}", other),
        }
    }

    /// Unchanged left + base, deleted in right → Delete.
    #[test]
    fn merge_unmodified_left_deleted_right_is_delete(
        base in arb_issue("both"),
        strategy in strategy_strategy(),
    ) {
        let left = base.clone();
        let result = merge_issue(Some(&base), Some(&left), None, strategy);
        prop_assert_eq!(result, MergeResult::Delete);
    }

    /// Unchanged right + base, deleted in left → Delete.
    #[test]
    fn merge_unmodified_right_deleted_left_is_delete(
        base in arb_issue("both"),
        strategy in strategy_strategy(),
    ) {
        let right = base.clone();
        let result = merge_issue(Some(&base), None, Some(&right), strategy);
        prop_assert_eq!(result, MergeResult::Delete);
    }

    /// Both sides identical to base → Keep(left) (merge always emits Keep when all present).
    #[test]
    fn merge_all_identical_keeps_left(
        base in arb_issue("same"),
        strategy in strategy_strategy(),
    ) {
        let left = base.clone();
        let right = base.clone();
        let result = merge_issue(Some(&base), Some(&left), Some(&right), strategy);
        match result {
            MergeResult::Keep(kept) => prop_assert_eq!(kept.id, base.id),
            other => prop_assert!(false, "Expected Keep, got {:?}", other),
        }
    }

    /// Manual strategy with both-modified → Conflict(BothModified).
    #[test]
    fn merge_both_modified_manual_is_conflict(
        base in arb_issue("conflict"),
    ) {
        let mut left = base.clone();
        left.title = "Left modification".to_string();
        left.updated_at = base.updated_at + Duration::seconds(10);

        let mut right = base.clone();
        right.title = "Right modification".to_string();
        right.updated_at = base.updated_at + Duration::seconds(20);

        let result = merge_issue(Some(&base), Some(&left), Some(&right), ConflictResolution::Manual);
        match result {
            MergeResult::Conflict(ConflictType::BothModified) => {}
            other => prop_assert!(false, "Expected Conflict(BothModified), got {:?}", other),
        }
    }

    /// PreferLocal with both-modified → keeps left.
    #[test]
    fn merge_both_modified_prefer_local_keeps_left(
        base in arb_issue("pref"),
    ) {
        let mut left = base.clone();
        left.title = "Left change".to_string();
        left.updated_at = base.updated_at + Duration::seconds(10);

        let mut right = base.clone();
        right.title = "Right change".to_string();
        right.updated_at = base.updated_at + Duration::seconds(20);

        let result = merge_issue(
            Some(&base), Some(&left), Some(&right),
            ConflictResolution::PreferLocal,
        );
        match result {
            MergeResult::Keep(kept) | MergeResult::KeepWithNote(kept, _) => {
                prop_assert_eq!(kept.title, "Left change");
            }
            other => prop_assert!(false, "Expected Keep with left, got {:?}", other),
        }
    }

    /// PreferExternal with both-modified → keeps right.
    #[test]
    fn merge_both_modified_prefer_external_keeps_right(
        base in arb_issue("pref"),
    ) {
        let mut left = base.clone();
        left.title = "Left change".to_string();
        left.updated_at = base.updated_at + Duration::seconds(10);

        let mut right = base.clone();
        right.title = "Right change".to_string();
        right.updated_at = base.updated_at + Duration::seconds(20);

        let result = merge_issue(
            Some(&base), Some(&left), Some(&right),
            ConflictResolution::PreferExternal,
        );
        match result {
            MergeResult::Keep(kept) | MergeResult::KeepWithNote(kept, _) => {
                prop_assert_eq!(kept.title, "Right change");
            }
            other => prop_assert!(false, "Expected Keep with right, got {:?}", other),
        }
    }
}

// ---------------------------------------------------------------------------
// three_way_merge properties
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Tombstone protection: a tombstoned issue in left cannot be resurrected by right.
    #[test]
    fn three_way_merge_tombstone_protection(
        suffix in "[a-z0-9]{4,8}",
        strategy in strategy_strategy(),
    ) {
        let id = format!("bd-tomb{suffix}");

        let mut base_issue = make_issue(&id, "Base", Status::Open, Priority(1), IssueType::Task, 0);
        base_issue.id = id.clone();

        let mut local = base_issue.clone();
        local.status = Status::Tombstone;

        let mut external = base_issue.clone();
        external.title = "Resurrected!".to_string();
        external.status = Status::Open;

        let ctx = MergeContext::new(
            HashMap::from([(id.clone(), base_issue)]),
            HashMap::from([(id.clone(), local)]),
            HashMap::from([(id.clone(), external)]),
        );

        let mut tombstones = HashSet::new();
        tombstones.insert(id.clone());

        let report = three_way_merge(&ctx, strategy, Some(&tombstones));
        prop_assert!(
            report.tombstone_protected.contains(&id),
            "Tombstoned issue should be protected, got report: {:?}",
            report,
        );
        let resurrected = report.kept.iter().any(|i| i.id == id && i.status != Status::Tombstone);
        prop_assert!(!resurrected, "Tombstoned issue must not be resurrected");
    }

    /// All issues present and identical in all three maps → all kept, no deletes/conflicts.
    #[test]
    fn three_way_merge_all_identical_keeps_all(
        count in 1usize..=5,
        strategy in strategy_strategy(),
    ) {
        let mut base = HashMap::new();
        for i in 0..count {
            let id = format!("bd-id{i}");
            let issue = make_issue(&id, &format!("Issue {i}"), Status::Open, Priority(1), IssueType::Task, 0);
            base.insert(id, issue);
        }
        let left = base.clone();
        let right = base.clone();

        let ctx = MergeContext::new(base, left, right);
        let report = three_way_merge(&ctx, strategy, None);

        prop_assert_eq!(report.kept.len(), count, "All identical issues should be kept");
        prop_assert_eq!(report.deleted.len(), 0, "No deletes expected for identical sets");
        prop_assert_eq!(report.conflicts.len(), 0, "No conflicts expected for identical sets");
    }

    /// New issues only on right → all appear in report.kept.
    #[test]
    fn three_way_merge_new_right_all_kept(
        count in 1usize..=5,
        strategy in strategy_strategy(),
    ) {
        let base = HashMap::new();
        let left = HashMap::new();
        let mut right = HashMap::new();
        for i in 0..count {
            let id = format!("bd-new{i}");
            let issue = make_issue(&id, &format!("New {i}"), Status::Open, Priority(2), IssueType::Bug, 0);
            right.insert(id, issue);
        }

        let ctx = MergeContext::new(base, left, right);
        let report = three_way_merge(&ctx, strategy, None);

        prop_assert_eq!(report.kept.len(), count, "All new-right issues should be kept");
    }

    /// Merge report action count: kept + deleted + conflicts covers all non-identical IDs.
    #[test]
    fn three_way_merge_action_coverage(
        base_issue in prop::option::of(arb_issue("cov")),
        left_issue in prop::option::of(arb_issue("cov")),
        right_issue in prop::option::of(arb_issue("cov")),
        strategy in strategy_strategy(),
    ) {
        let id = "bd-cov".to_string();
        let mut base = HashMap::new();
        let mut left = HashMap::new();
        let mut right = HashMap::new();

        if let Some(b) = &base_issue {
            let mut b = b.clone();
            b.id = id.clone();
            base.insert(id.clone(), b);
        }
        if let Some(l) = &left_issue {
            let mut l = l.clone();
            l.id = id.clone();
            left.insert(id.clone(), l);
        }
        if let Some(r) = &right_issue {
            let mut r = r.clone();
            r.id = id.clone();
            right.insert(id.clone(), r);
        }

        let ctx = MergeContext::new(base, left, right);
        let report = three_way_merge(&ctx, strategy, None);

        let all_actions = report.kept.len() + report.deleted.len() + report.conflicts.len();

        // At most 1 action per ID (or 0 for NoAction/identical).
        prop_assert!(all_actions <= 1, "At most one action per ID, got {}", all_actions);
    }
}

#[test]
fn three_way_merge_notes_are_byte_identical_across_context_order() {
    fn context_from_order(ids: &[&str]) -> MergeContext {
        let mut base = HashMap::new();
        let mut left = HashMap::new();
        let mut right = HashMap::new();

        for id in ids {
            base.insert(
                (*id).to_string(),
                make_issue(
                    id,
                    &format!("base {id}"),
                    Status::Open,
                    Priority(1),
                    IssueType::Task,
                    0,
                ),
            );
            left.insert(
                (*id).to_string(),
                make_issue(
                    id,
                    &format!("local {id}"),
                    Status::Open,
                    Priority(1),
                    IssueType::Task,
                    20,
                ),
            );
            right.insert(
                (*id).to_string(),
                make_issue(
                    id,
                    &format!("external {id}"),
                    Status::Open,
                    Priority(1),
                    IssueType::Task,
                    10,
                ),
            );
        }

        MergeContext::new(base, left, right)
    }

    let forward = three_way_merge(
        &context_from_order(&["bd-c", "bd-a", "bd-e", "bd-b", "bd-d", "bd-f"]),
        ConflictResolution::PreferNewer,
        None,
    );
    let reverse = three_way_merge(
        &context_from_order(&["bd-f", "bd-d", "bd-b", "bd-e", "bd-a", "bd-c"]),
        ConflictResolution::PreferNewer,
        None,
    );

    assert_eq!(notes_bytes(&forward.notes), notes_bytes(&reverse.notes));
    assert_eq!(
        forward
            .notes
            .iter()
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>(),
        ["bd-a", "bd-b", "bd-c", "bd-d", "bd-e", "bd-f"],
    );
}

// ---------------------------------------------------------------------------
// Custom status variant tests (beads_rust-jvkc)
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    /// Both sides change to different Custom statuses → Conflict(BothModified) under Manual.
    #[test]
    fn merge_both_custom_status_manual_is_conflict(
        left_custom in "[a-z_]{3,15}",
        right_custom in "[a-z_]{3,15}",
    ) {
        prop_assume!(left_custom != right_custom);

        let base = make_issue("bd-cust", "Custom test", Status::Open, Priority(1), IssueType::Task, 0);

        let mut left = base.clone();
        left.status = Status::Custom(left_custom);
        left.updated_at = base.updated_at + Duration::seconds(10);

        let mut right = base.clone();
        right.status = Status::Custom(right_custom);
        right.updated_at = base.updated_at + Duration::seconds(20);

        let result = merge_issue(Some(&base), Some(&left), Some(&right), ConflictResolution::Manual);
        match result {
            MergeResult::Conflict(ConflictType::BothModified) => {}
            other => prop_assert!(false, "Expected Conflict(BothModified), got {:?}", other),
        }
    }

    /// Both sides change to the same Custom status → convergent (Keep, not conflict).
    #[test]
    fn merge_same_custom_status_both_sides_is_keep(
        custom_name in "[a-z_]{3,15}",
    ) {
        let base = make_issue("bd-conv", "Converge", Status::Open, Priority(1), IssueType::Task, 0);

        let mut left = base.clone();
        left.status = Status::Custom(custom_name.clone());
        left.updated_at = base.updated_at + Duration::seconds(10);

        let mut right = base.clone();
        right.status = Status::Custom(custom_name);
        right.updated_at = base.updated_at + Duration::seconds(10);

        let result = merge_issue(Some(&base), Some(&left), Some(&right), ConflictResolution::Manual);
        match result {
            MergeResult::Keep(_) | MergeResult::KeepWithNote(_, _) => {}
            MergeResult::Conflict(_) => prop_assert!(false, "Same custom status on both sides should not conflict"),
            other => prop_assert!(false, "Unexpected result: {:?}", other),
        }
    }

    /// PreferNewer with different Custom statuses → keeps the newer side.
    #[test]
    fn merge_custom_status_prefer_newer_keeps_newer(
        left_custom in "[a-z_]{3,15}",
        right_custom in "[a-z_]{3,15}",
        left_newer in any::<bool>(),
    ) {
        prop_assume!(left_custom != right_custom);

        let base = make_issue("bd-newer", "Newer test", Status::Open, Priority(1), IssueType::Task, 0);

        let mut left = base.clone();
        left.status = Status::Custom(left_custom.clone());
        left.updated_at = base.updated_at + Duration::seconds(if left_newer { 20 } else { 10 });

        let mut right = base.clone();
        right.status = Status::Custom(right_custom.clone());
        right.updated_at = base.updated_at + Duration::seconds(if left_newer { 10 } else { 20 });

        let result = merge_issue(Some(&base), Some(&left), Some(&right), ConflictResolution::PreferNewer);
        match result {
            MergeResult::Keep(kept) | MergeResult::KeepWithNote(kept, _) => {
                let expected_status = if left_newer {
                    Status::Custom(left_custom)
                } else {
                    Status::Custom(right_custom)
                };
                prop_assert_eq!(kept.status, expected_status);
            }
            other => prop_assert!(false, "Expected Keep, got {:?}", other),
        }
    }

    /// Only one side changes to Custom status → the changed side wins (no conflict).
    #[test]
    fn merge_one_side_custom_status_no_conflict(
        custom_name in "[a-z_]{3,15}",
        change_left in any::<bool>(),
        strategy in strategy_strategy(),
    ) {
        let base = make_issue("bd-one", "One side", Status::Open, Priority(1), IssueType::Task, 0);

        let mut left = base.clone();
        let mut right = base.clone();

        if change_left {
            left.status = Status::Custom(custom_name);
            left.updated_at = base.updated_at + Duration::seconds(10);
        } else {
            right.status = Status::Custom(custom_name);
            right.updated_at = base.updated_at + Duration::seconds(10);
        }

        let result = merge_issue(Some(&base), Some(&left), Some(&right), strategy);
        match result {
            MergeResult::Keep(_) | MergeResult::KeepWithNote(_, _) => {}
            MergeResult::Conflict(_) => {
                prop_assert!(false, "One-sided custom status change should not conflict");
            }
            other => prop_assert!(false, "Unexpected result: {:?}", other),
        }
    }

    /// Determinism holds when Custom statuses are involved.
    #[test]
    fn merge_custom_status_deterministic(
        base_status in status_with_custom_strategy(),
        left_status in status_with_custom_strategy(),
        right_status in status_with_custom_strategy(),
        strategy in strategy_strategy(),
    ) {
        let base = make_issue("bd-cdet", "Det", base_status, Priority(1), IssueType::Task, 0);
        let mut left = base.clone();
        left.status = left_status;
        left.updated_at = base.updated_at + Duration::seconds(5);
        let mut right = base.clone();
        right.status = right_status;
        right.updated_at = base.updated_at + Duration::seconds(10);

        let r1 = merge_issue(Some(&base), Some(&left), Some(&right), strategy);
        let r2 = merge_issue(Some(&base), Some(&left), Some(&right), strategy);
        prop_assert_eq!(r1, r2);
    }
}
