//! Test helpers for invariant-based ordering assertions.
//!
//! Created 2026-05-09 for beads_rust-jsgu (audit-driven cleanup).
//!
//! These helpers replace the **anti-pattern of pinning generated IDs in
//! ordered-result assertions**. The audit identified `storage_ready::
//! ready_sort_policy_*` as fragile because they asserted `ids[0] == "<hash>"`
//! against content-derived hashes that change with any test refactor.
//!
//! **Use these helpers instead of `assert_eq!(ids[N], expected_id)`** when
//! the invariant under test is the relative ordering, not the specific IDs.

#![allow(dead_code)]

use beads_rust::model::{Issue, Priority};
use std::fmt::Debug;

/// Assert that `items`, when projected via `key_fn`, are in ascending order.
///
/// Panics with a rich error message naming the first violating pair.
pub fn assert_ordered_by<T, K>(items: &[T], key_fn: impl Fn(&T) -> K, name: &str)
where
    K: Ord + Debug,
    T: Debug,
{
    if items.len() < 2 {
        return;
    }
    for window in items.windows(2) {
        let a_key = key_fn(&window[0]);
        let b_key = key_fn(&window[1]);
        assert!(
            a_key <= b_key,
            "{name}: ordering invariant violated\n  item[i]   = {:?} (key={:?})\n  item[i+1] = {:?} (key={:?})\n",
            window[0],
            a_key,
            window[1],
            b_key
        );
    }
}

/// Assert that `issues` are ordered by `priority` ascending. Issues with the
/// same priority can appear in any order (no secondary tiebreak enforced).
///
/// This replaces the brittle pattern of asserting on specific issue IDs
/// when the actual contract is "lower priority numbers come first".
pub fn assert_priority_ordered(issues: &[Issue]) {
    assert_ordered_by(
        issues,
        |i| i.priority,
        "assert_priority_ordered (P0 < P1 < P2 < P3 < P4)",
    );
}

/// Assert that `issues` are ordered by `created_at` ascending (oldest first).
pub fn assert_oldest_first(issues: &[Issue]) {
    assert_ordered_by(
        issues,
        |i| i.created_at,
        "assert_oldest_first (oldest created_at first)",
    );
}

/// Assert the hybrid ordering: P0/P1 issues come before P2/P3/P4 issues.
/// Within each tier, no secondary order is enforced.
pub fn assert_hybrid_ordered(issues: &[Issue]) {
    let mut high_tier_ended = false;
    for (idx, issue) in issues.iter().enumerate() {
        let is_high = issue.priority <= Priority::HIGH;
        if !is_high {
            high_tier_ended = true;
        } else {
            assert!(
                !high_tier_ended,
                "assert_hybrid_ordered: P{} issue at idx={idx} appears AFTER a low-tier issue (high tier must come first)\n  full order: {:?}",
                issue.priority.0,
                issues
                    .iter()
                    .map(|i| (&i.id, i.priority))
                    .collect::<Vec<_>>()
            );
        }
    }
}

/// Assert that exactly one issue in `issues` matches the predicate `pred`.
/// Useful for "the closed bead must appear in the result, exactly once".
pub fn assert_contains_exactly_one(issues: &[Issue], pred: impl Fn(&Issue) -> bool, name: &str) {
    let count = issues.iter().filter(|i| pred(i)).count();
    assert_eq!(
        count,
        1,
        "{name}: expected exactly 1 matching issue; got {count}\n  full list: {:?}",
        issues.iter().map(|i| (&i.id, &i.title)).collect::<Vec<_>>()
    );
}

/// Assert that `issues` contain no duplicate IDs.
pub fn assert_no_duplicate_ids(issues: &[Issue]) {
    let mut seen = std::collections::HashSet::new();
    for issue in issues {
        assert!(
            seen.insert(&issue.id),
            "assert_no_duplicate_ids: duplicate id {:?} in result; full list: {:?}",
            issue.id,
            issues.iter().map(|i| &i.id).collect::<Vec<_>>()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use beads_rust::model::{IssueType, Status};
    use chrono::{Duration, TimeZone, Utc};

    fn make_issue(id: &str, priority: Priority, age_offset_secs: i64) -> Issue {
        let base = Utc.timestamp_opt(1_735_689_600, 0).unwrap();
        Issue {
            id: id.to_string(),
            title: format!("Test {id}"),
            status: Status::Open,
            priority,
            issue_type: IssueType::Task,
            created_at: base + Duration::seconds(age_offset_secs),
            updated_at: base + Duration::seconds(age_offset_secs + 1),
            content_hash: None,
            description: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_by: None,
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            due_at: None,
            defer_until: None,
            external_ref: None,
            source_system: None,
            source_repo: None,
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
            labels: vec![],
            dependencies: vec![],
            comments: vec![],
        }
    }

    #[test]
    fn assert_ordered_by_passes_on_sorted_input() {
        let nums = [1, 2, 3, 5, 8];
        assert_ordered_by(&nums, |n| *n, "fibonacci-ish");
    }

    #[test]
    #[should_panic(expected = "ordering invariant violated")]
    fn assert_ordered_by_panics_with_descriptive_message_on_unsorted_input() {
        let nums = [1, 5, 3];
        assert_ordered_by(&nums, |n| *n, "test_name_in_msg");
    }

    #[test]
    fn assert_priority_ordered_treats_critical_as_lowest_value() {
        let issues = [
            make_issue("a", Priority::CRITICAL, 0),
            make_issue("b", Priority::HIGH, 1),
            make_issue("c", Priority::MEDIUM, 2),
            make_issue("d", Priority::LOW, 3),
        ];
        assert_priority_ordered(&issues);
    }

    #[test]
    #[should_panic(expected = "ordering invariant violated")]
    fn assert_priority_ordered_panics_when_higher_priority_comes_after_lower() {
        let issues = [
            make_issue("a", Priority::MEDIUM, 0),
            make_issue("b", Priority::CRITICAL, 1), // P0 after P2 → panic
        ];
        assert_priority_ordered(&issues);
    }

    #[test]
    fn assert_no_duplicate_ids_passes_on_unique_set() {
        let issues = [
            make_issue("a", Priority::MEDIUM, 0),
            make_issue("b", Priority::MEDIUM, 0),
            make_issue("c", Priority::MEDIUM, 0),
        ];
        assert_no_duplicate_ids(&issues);
    }
}
