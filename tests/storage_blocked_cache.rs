//! Comprehensive tests for the blocked_issues_cache correctness.
//!
//! Covers: cache rebuild triggers, ready/blocked consistency, reopen semantics,
//! chain dependencies, mixed dependency types, delete/tombstone cleanup,
//! remove-all-deps, parent-child transitive blocking, and cross-check of
//! `get_ready_issues` vs `get_blocked_ids`.
//!
//! Related bead: beads_rust-1kaf

#![allow(clippy::similar_names)]

mod common;

use beads_rust::model::{DependencyType, Status};
use beads_rust::storage::{IssueUpdate, ReadyFilters, ReadySortPolicy, SqliteStorage};
use common::{fixtures, test_db};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn blocked_ids(storage: &mut SqliteStorage) -> Vec<String> {
    storage
        .get_blocked_issues()
        .unwrap()
        .into_iter()
        .map(|(issue, _)| issue.id)
        .collect()
}

fn ready_ids(storage: &mut SqliteStorage) -> Vec<String> {
    storage
        .get_ready_issues(&ReadyFilters::default(), ReadySortPolicy::Priority)
        .unwrap()
        .into_iter()
        .map(|i| i.id)
        .collect()
}

fn status_update(status: Status) -> IssueUpdate {
    IssueUpdate {
        status: Some(status),
        ..Default::default()
    }
}

// ===========================================================================
// 1. Reopen blocker → re-blocks dependent
// ===========================================================================

#[test]
fn reopen_blocker_re_blocks_dependent() {
    let mut storage = test_db();

    let blocker = fixtures::issue("reopen-blocker");
    let dependent = fixtures::issue("reopen-dependent");

    storage.create_issue(&blocker, "tester").unwrap();
    storage.create_issue(&dependent, "tester").unwrap();

    storage
        .add_dependency(
            &dependent.id,
            &blocker.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();

    // Dependent should be blocked
    assert!(
        blocked_ids(&mut storage).contains(&dependent.id),
        "dependent should be blocked initially"
    );

    // Close blocker → dependent unblocked
    storage
        .update_issue(&blocker.id, &status_update(Status::Closed), "tester")
        .unwrap();
    assert!(
        !blocked_ids(&mut storage).contains(&dependent.id),
        "dependent should be unblocked after closing blocker"
    );

    // Reopen blocker → dependent blocked again
    storage
        .update_issue(&blocker.id, &status_update(Status::Open), "tester")
        .unwrap();
    assert!(
        blocked_ids(&mut storage).contains(&dependent.id),
        "dependent should be re-blocked after reopening blocker"
    );
}

// ===========================================================================
// 2. Chain dependencies (A blocks B, B blocks C)
// ===========================================================================

#[test]
fn chain_dependency_blocks_downstream() {
    let mut storage = test_db();

    let a = fixtures::issue("chain-A");
    let b = fixtures::issue("chain-B");
    let c = fixtures::issue("chain-C");

    storage.create_issue(&a, "tester").unwrap();
    storage.create_issue(&b, "tester").unwrap();
    storage.create_issue(&c, "tester").unwrap();

    // A blocks B
    storage
        .add_dependency(&b.id, &a.id, DependencyType::Blocks.as_str(), "tester")
        .unwrap();

    // B blocks C
    storage
        .add_dependency(&c.id, &b.id, DependencyType::Blocks.as_str(), "tester")
        .unwrap();

    let ids = blocked_ids(&mut storage);
    assert!(ids.contains(&b.id), "B should be blocked by A");
    assert!(ids.contains(&c.id), "C should be blocked by B");

    // Close A → B unblocked, but C still blocked by B (B is open, not closed)
    storage
        .update_issue(&a.id, &status_update(Status::Closed), "tester")
        .unwrap();

    let ids = blocked_ids(&mut storage);
    assert!(!ids.contains(&b.id), "B should be unblocked after A closed");
    assert!(ids.contains(&c.id), "C should still be blocked by open B");

    // Close B → C finally unblocked
    storage
        .update_issue(&b.id, &status_update(Status::Closed), "tester")
        .unwrap();

    let ids = blocked_ids(&mut storage);
    assert!(!ids.contains(&c.id), "C should be unblocked after B closed");
}

// ===========================================================================
// 3. Multiple blockers — partial close leaves blocked
// ===========================================================================

#[test]
fn multiple_blockers_partial_close() {
    let mut storage = test_db();

    let blocker1 = fixtures::issue("multi-blocker-1");
    let blocker2 = fixtures::issue("multi-blocker-2");
    let dependent = fixtures::issue("multi-dependent");

    storage.create_issue(&blocker1, "tester").unwrap();
    storage.create_issue(&blocker2, "tester").unwrap();
    storage.create_issue(&dependent, "tester").unwrap();

    storage
        .add_dependency(
            &dependent.id,
            &blocker1.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();
    storage
        .add_dependency(
            &dependent.id,
            &blocker2.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();

    assert!(blocked_ids(&mut storage).contains(&dependent.id));

    // Close only blocker1 → still blocked by blocker2
    storage
        .update_issue(&blocker1.id, &status_update(Status::Closed), "tester")
        .unwrap();
    assert!(
        blocked_ids(&mut storage).contains(&dependent.id),
        "still blocked by blocker2"
    );

    // Close blocker2 → fully unblocked
    storage
        .update_issue(&blocker2.id, &status_update(Status::Closed), "tester")
        .unwrap();
    assert!(
        !blocked_ids(&mut storage).contains(&dependent.id),
        "unblocked after all blockers closed"
    );
}

// ===========================================================================
// 4. Mixed dependency types (blocks, conditional-blocks, waits-for)
// ===========================================================================

#[test]
fn mixed_dependency_types_all_block() {
    let mut storage = test_db();

    let b1 = fixtures::issue("mix-blocks-src");
    let b2 = fixtures::issue("mix-cond-src");
    let b3 = fixtures::issue("mix-waits-src");
    let target = fixtures::issue("mix-target");

    storage.create_issue(&b1, "tester").unwrap();
    storage.create_issue(&b2, "tester").unwrap();
    storage.create_issue(&b3, "tester").unwrap();
    storage.create_issue(&target, "tester").unwrap();

    storage
        .add_dependency(&target.id, &b1.id, "blocks", "tester")
        .unwrap();
    storage
        .add_dependency(&target.id, &b2.id, "conditional-blocks", "tester")
        .unwrap();
    storage
        .add_dependency(&target.id, &b3.id, "waits-for", "tester")
        .unwrap();

    let ids = blocked_ids(&mut storage);
    assert!(
        ids.contains(&target.id),
        "target should be blocked by all three dependency types"
    );

    // Close all blockers
    for blocker_id in [&b1.id, &b2.id, &b3.id] {
        storage
            .update_issue(blocker_id, &status_update(Status::Closed), "tester")
            .unwrap();
    }

    assert!(
        !blocked_ids(&mut storage).contains(&target.id),
        "target unblocked after all mixed deps closed"
    );
}

// ===========================================================================
// 5. Closing the blocked issue itself → removed from cache
// ===========================================================================

#[test]
fn closing_blocked_issue_removes_from_cache() {
    let mut storage = test_db();

    let blocker = fixtures::issue("close-self-blocker");
    let blocked_issue = fixtures::issue("close-self-blocked");

    storage.create_issue(&blocker, "tester").unwrap();
    storage.create_issue(&blocked_issue, "tester").unwrap();

    storage
        .add_dependency(
            &blocked_issue.id,
            &blocker.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();

    assert!(blocked_ids(&mut storage).contains(&blocked_issue.id));

    // Close the blocked issue itself (not the blocker)
    storage
        .update_issue(&blocked_issue.id, &status_update(Status::Closed), "tester")
        .unwrap();

    // Closed issues should not appear in blocked cache
    // (blocked cache only tracks open/in_progress issues)
    let ids = blocked_ids(&mut storage);
    assert!(
        !ids.contains(&blocked_issue.id),
        "closed issue should not appear in blocked cache"
    );
}

// ===========================================================================
// 6. Delete issue → cache cleaned
// ===========================================================================

#[test]
fn delete_issue_cleans_cache() {
    let mut storage = test_db();

    let blocker = fixtures::issue("del-blocker");
    let blocked_issue = fixtures::issue("del-blocked");

    storage.create_issue(&blocker, "tester").unwrap();
    storage.create_issue(&blocked_issue, "tester").unwrap();

    storage
        .add_dependency(
            &blocked_issue.id,
            &blocker.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();

    assert!(blocked_ids(&mut storage).contains(&blocked_issue.id));

    // Delete the blocker → blocked_issue should become unblocked
    // (tombstoned issues are terminal, so dependency no longer active)
    storage
        .delete_issue(&blocker.id, "tester", "test cleanup", None)
        .unwrap();

    let ids = blocked_ids(&mut storage);
    assert!(
        !ids.contains(&blocked_issue.id),
        "deleting blocker should unblock dependent"
    );
}

// ===========================================================================
// 7. Remove all dependencies → cache cleared
// ===========================================================================

#[test]
fn remove_all_dependencies_clears_cache() {
    let mut storage = test_db();

    let b1 = fixtures::issue("rm-all-b1");
    let b2 = fixtures::issue("rm-all-b2");
    let target = fixtures::issue("rm-all-target");

    storage.create_issue(&b1, "tester").unwrap();
    storage.create_issue(&b2, "tester").unwrap();
    storage.create_issue(&target, "tester").unwrap();

    storage
        .add_dependency(
            &target.id,
            &b1.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();
    storage
        .add_dependency(
            &target.id,
            &b2.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();

    assert!(blocked_ids(&mut storage).contains(&target.id));

    storage
        .remove_all_dependencies(&target.id, "tester")
        .unwrap();

    assert!(
        !blocked_ids(&mut storage).contains(&target.id),
        "removing all deps should unblock issue"
    );
}

// ===========================================================================
// 8. Transitive parent-child blocking
// ===========================================================================

#[test]
fn parent_child_transitive_blocking() {
    let mut storage = test_db();

    let blocker = fixtures::issue("pc-blocker");
    let parent = fixtures::issue("pc-parent");
    let child = fixtures::issue("pc-child");

    storage.create_issue(&blocker, "tester").unwrap();
    storage.create_issue(&parent, "tester").unwrap();
    storage.create_issue(&child, "tester").unwrap();

    // blocker blocks parent
    storage
        .add_dependency(
            &parent.id,
            &blocker.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();

    // child is child of parent
    storage
        .add_dependency(&child.id, &parent.id, "parent-child", "tester")
        .unwrap();

    let ids = blocked_ids(&mut storage);
    assert!(ids.contains(&parent.id), "parent blocked by blocker");
    assert!(
        ids.contains(&child.id),
        "child transitively blocked via parent"
    );

    // Close blocker → parent unblocked → child also unblocked
    storage
        .update_issue(&blocker.id, &status_update(Status::Closed), "tester")
        .unwrap();

    let ids = blocked_ids(&mut storage);
    assert!(!ids.contains(&parent.id), "parent unblocked");
    assert!(!ids.contains(&child.id), "child transitively unblocked");
}

// ===========================================================================
// 9. Remove parent → child unblocked if parent was sole reason
// ===========================================================================

#[test]
fn remove_parent_unblocks_child() {
    let mut storage = test_db();

    let blocker = fixtures::issue("rp-blocker");
    let parent = fixtures::issue("rp-parent");
    let child = fixtures::issue("rp-child");

    storage.create_issue(&blocker, "tester").unwrap();
    storage.create_issue(&parent, "tester").unwrap();
    storage.create_issue(&child, "tester").unwrap();

    storage
        .add_dependency(
            &parent.id,
            &blocker.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();
    storage
        .add_dependency(&child.id, &parent.id, "parent-child", "tester")
        .unwrap();

    assert!(
        blocked_ids(&mut storage).contains(&child.id),
        "child blocked via parent"
    );

    // Remove parent-child link
    storage
        .remove_dependency(&child.id, &parent.id, "tester")
        .unwrap();

    assert!(
        !blocked_ids(&mut storage).contains(&child.id),
        "child should be unblocked after parent link removed"
    );
}

// ===========================================================================
// 10. Ready/blocked cross-consistency
// ===========================================================================

#[test]
fn ready_and_blocked_are_disjoint() {
    let mut storage = test_db();

    let blocker = fixtures::issue("disjoint-blocker");
    let blocked_issue = fixtures::issue("disjoint-blocked");
    let free_issue = fixtures::issue("disjoint-free");

    storage.create_issue(&blocker, "tester").unwrap();
    storage.create_issue(&blocked_issue, "tester").unwrap();
    storage.create_issue(&free_issue, "tester").unwrap();

    storage
        .add_dependency(
            &blocked_issue.id,
            &blocker.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();

    let ready = ready_ids(&mut storage);
    let blocked = blocked_ids(&mut storage);

    // Blocked issues must not appear in ready list
    for id in &blocked {
        assert!(
            !ready.contains(id),
            "blocked issue {id} should not appear in ready list"
        );
    }

    // Free issue should be in ready list
    assert!(ready.contains(&free_issue.id), "free issue should be ready");

    // Blocked issue must be in blocked list, not ready
    assert!(blocked.contains(&blocked_issue.id));
    assert!(!ready.contains(&blocked_issue.id));
}

// ===========================================================================
// 11. is_blocked matches get_blocked_ids
// ===========================================================================

#[test]
fn is_blocked_matches_get_blocked_ids() {
    let mut storage = test_db();

    let blocker = fixtures::issue("ib-blocker");
    let blocked_issue = fixtures::issue("ib-blocked");
    let free_issue = fixtures::issue("ib-free");

    storage.create_issue(&blocker, "tester").unwrap();
    storage.create_issue(&blocked_issue, "tester").unwrap();
    storage.create_issue(&free_issue, "tester").unwrap();

    storage
        .add_dependency(
            &blocked_issue.id,
            &blocker.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();

    let blocked_set = storage.get_blocked_ids().unwrap();

    // is_blocked should agree with get_blocked_ids for every issue
    assert_eq!(
        storage.is_blocked(&blocked_issue.id).unwrap(),
        blocked_set.contains(&blocked_issue.id),
        "is_blocked should match for blocked issue"
    );
    assert_eq!(
        storage.is_blocked(&free_issue.id).unwrap(),
        blocked_set.contains(&free_issue.id),
        "is_blocked should match for free issue"
    );
    assert_eq!(
        storage.is_blocked(&blocker.id).unwrap(),
        blocked_set.contains(&blocker.id),
        "is_blocked should match for blocker (not blocked itself)"
    );
}

// ===========================================================================
// 12. Blocker metadata format
// ===========================================================================

#[test]
fn blocked_cache_stores_blocker_metadata() {
    let mut storage = test_db();

    let blocker = fixtures::issue("meta-blocker");
    let blocked_issue = fixtures::issue("meta-blocked");

    storage.create_issue(&blocker, "tester").unwrap();
    storage.create_issue(&blocked_issue, "tester").unwrap();

    storage
        .add_dependency(
            &blocked_issue.id,
            &blocker.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();

    let blocked_issues = storage.get_blocked_issues().unwrap();
    let entry = blocked_issues
        .iter()
        .find(|(issue, _)| issue.id == blocked_issue.id)
        .expect("blocked entry should exist");

    // Blocker references should contain the blocker's ID
    assert!(
        entry.1.iter().any(|r| r.contains(&blocker.id)),
        "blocker references should contain blocker ID, got: {:?}",
        entry.1
    );
}

// ===========================================================================
// 13. In-progress blocker still blocks
// ===========================================================================

#[test]
fn in_progress_blocker_still_blocks() {
    let mut storage = test_db();

    let blocker = fixtures::issue("ip-blocker");
    let dependent = fixtures::issue("ip-dependent");

    storage.create_issue(&blocker, "tester").unwrap();
    storage.create_issue(&dependent, "tester").unwrap();

    storage
        .add_dependency(
            &dependent.id,
            &blocker.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();

    // Move blocker to in_progress → should still block
    storage
        .update_issue(&blocker.id, &status_update(Status::InProgress), "tester")
        .unwrap();

    assert!(
        blocked_ids(&mut storage).contains(&dependent.id),
        "in_progress blocker should still block dependent"
    );
}

// ===========================================================================
// 14. Force rebuild produces same result as auto-rebuild
// ===========================================================================

#[test]
fn force_rebuild_matches_auto_rebuild() {
    let mut storage = test_db();

    let blocker = fixtures::issue("force-blocker");
    let blocked_issue = fixtures::issue("force-blocked");
    let free_issue = fixtures::issue("force-free");

    storage.create_issue(&blocker, "tester").unwrap();
    storage.create_issue(&blocked_issue, "tester").unwrap();
    storage.create_issue(&free_issue, "tester").unwrap();

    storage
        .add_dependency(
            &blocked_issue.id,
            &blocker.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();

    // Capture auto-rebuild state
    let auto_blocked = storage.get_blocked_ids().unwrap();

    // Force rebuild and compare
    storage.rebuild_blocked_cache(true).unwrap();
    let force_blocked = storage.get_blocked_ids().unwrap();

    assert_eq!(
        auto_blocked, force_blocked,
        "force rebuild should produce identical result to auto-rebuild"
    );
}

// ===========================================================================
// 15. Deep transitive parent-child chain
// ===========================================================================

#[test]
fn deep_parent_child_chain_blocking() {
    let mut storage = test_db();

    let blocker = fixtures::issue("deep-blocker");
    let p1 = fixtures::issue("deep-parent-1");
    let p2 = fixtures::issue("deep-parent-2");
    let p3 = fixtures::issue("deep-parent-3");
    let leaf = fixtures::issue("deep-leaf");

    storage.create_issue(&blocker, "tester").unwrap();
    storage.create_issue(&p1, "tester").unwrap();
    storage.create_issue(&p2, "tester").unwrap();
    storage.create_issue(&p3, "tester").unwrap();
    storage.create_issue(&leaf, "tester").unwrap();

    // blocker blocks p1
    storage
        .add_dependency(
            &p1.id,
            &blocker.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();

    // p2 is child of p1, p3 is child of p2, leaf is child of p3
    storage
        .add_dependency(&p2.id, &p1.id, "parent-child", "tester")
        .unwrap();
    storage
        .add_dependency(&p3.id, &p2.id, "parent-child", "tester")
        .unwrap();
    storage
        .add_dependency(&leaf.id, &p3.id, "parent-child", "tester")
        .unwrap();

    let ids = blocked_ids(&mut storage);
    assert!(ids.contains(&p1.id), "p1 directly blocked");
    assert!(ids.contains(&p2.id), "p2 transitively blocked");
    assert!(ids.contains(&p3.id), "p3 transitively blocked");
    assert!(ids.contains(&leaf.id), "leaf transitively blocked");

    // Close blocker → entire chain unblocked
    storage
        .update_issue(&blocker.id, &status_update(Status::Closed), "tester")
        .unwrap();

    let ids = blocked_ids(&mut storage);
    assert!(!ids.contains(&p1.id), "p1 unblocked");
    assert!(!ids.contains(&p2.id), "p2 unblocked");
    assert!(!ids.contains(&p3.id), "p3 unblocked");
    assert!(!ids.contains(&leaf.id), "leaf unblocked");
}
