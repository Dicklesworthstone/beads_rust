//! Storage unit tests for dependency graph operations.
//!
//! Tests: `add_dependency`, `remove_dependency`, `get_dependencies`, `get_dependents`,
//! cycle detection, deep hierarchies, diamond patterns, blocked cache invalidation.
//! Real `SQLite`, no mocks.

#![allow(clippy::similar_names)]

mod common;

use beads_rust::model::{DependencyType, EventType, Status};
use beads_rust::storage::{ReadyFilters, ReadySortPolicy, SqliteStorage};
use common::{fixtures, test_db};

fn blocked_ids_for(storage: &SqliteStorage) -> Vec<String> {
    storage
        .get_blocked_issues()
        .unwrap()
        .into_iter()
        .map(|(issue, _)| issue.id)
        .collect()
}

// ============================================================================
// ADD DEPENDENCY TESTS
// ============================================================================

#[test]
fn add_dependency_creates_link() {
    let mut storage = test_db();

    let blocker = fixtures::issue("dep-blocker");
    let blocked = fixtures::issue("dep-blocked");

    storage.create_issue(&blocker, "tester").unwrap();
    storage.create_issue(&blocked, "tester").unwrap();

    let added = storage
        .add_dependency(
            &blocked.id,
            &blocker.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();

    assert!(added);

    let deps = storage.get_dependencies(&blocked.id).unwrap();
    assert_eq!(deps.len(), 1);
    assert_eq!(deps[0], blocker.id);
}

#[test]
fn add_dependency_duplicate_returns_false() {
    let mut storage = test_db();

    let blocker = fixtures::issue("dup-blocker");
    let blocked = fixtures::issue("dup-blocked");

    storage.create_issue(&blocker, "tester").unwrap();
    storage.create_issue(&blocked, "tester").unwrap();

    let first = storage
        .add_dependency(
            &blocked.id,
            &blocker.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();
    assert!(first);

    // Try to add same dependency again
    let second = storage
        .add_dependency(
            &blocked.id,
            &blocker.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();
    assert!(!second);

    // Should still only have one dependency
    let deps = storage.get_dependencies(&blocked.id).unwrap();
    assert_eq!(deps.len(), 1);
}

#[test]
fn add_dependency_records_event() {
    let mut storage = test_db();

    let blocker = fixtures::issue("event-blocker");
    let blocked = fixtures::issue("event-blocked");

    storage.create_issue(&blocker, "tester").unwrap();
    storage.create_issue(&blocked, "tester").unwrap();

    storage
        .add_dependency(
            &blocked.id,
            &blocker.id,
            DependencyType::Blocks.as_str(),
            "dep-actor",
        )
        .unwrap();

    let details = storage
        .get_issue_details(&blocked.id, true, true, 200)
        .unwrap()
        .expect("issue exists");

    // Find dependency added event
    let dep_event = details
        .events
        .iter()
        .find(|e| e.event_type == EventType::DependencyAdded);

    assert!(dep_event.is_some());
    let event = dep_event.unwrap();
    assert_eq!(event.actor, "dep-actor");
    assert!(event.comment.as_ref().unwrap().contains(&blocker.id));
}

#[test]
fn add_dependency_marks_dirty() {
    let mut storage = test_db();

    let blocker = fixtures::issue("dirty-blocker");
    let blocked = fixtures::issue("dirty-blocked");

    storage.create_issue(&blocker, "tester").unwrap();
    storage.create_issue(&blocked, "tester").unwrap();

    // Clear dirty flags
    let ids: Vec<String> = vec![blocker.id.clone(), blocked.id.clone()];
    storage.clear_dirty_flags(&ids).unwrap();

    // Add dependency
    storage
        .add_dependency(
            &blocked.id,
            &blocker.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();

    let dirty_ids = storage.get_dirty_issue_ids().unwrap();
    assert!(dirty_ids.contains(&blocked.id));
}

// ============================================================================
// REMOVE DEPENDENCY TESTS
// ============================================================================

#[test]
fn remove_dependency_removes_link() {
    let mut storage = test_db();

    let blocker = fixtures::issue("rm-blocker");
    let blocked = fixtures::issue("rm-blocked");

    storage.create_issue(&blocker, "tester").unwrap();
    storage.create_issue(&blocked, "tester").unwrap();

    storage
        .add_dependency(
            &blocked.id,
            &blocker.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();

    let removed = storage
        .remove_dependency(&blocked.id, &blocker.id, "tester")
        .unwrap();
    assert!(removed);

    let deps = storage.get_dependencies(&blocked.id).unwrap();
    assert!(deps.is_empty());
}

#[test]
fn remove_dependency_nonexistent_returns_false() {
    let mut storage = test_db();

    let issue1 = fixtures::issue("rm-none-1");
    let issue2 = fixtures::issue("rm-none-2");

    storage.create_issue(&issue1, "tester").unwrap();
    storage.create_issue(&issue2, "tester").unwrap();

    // No dependency exists
    let removed = storage
        .remove_dependency(&issue1.id, &issue2.id, "tester")
        .unwrap();
    assert!(!removed);
}

#[test]
fn remove_dependency_records_event() {
    let mut storage = test_db();

    let blocker = fixtures::issue("rm-event-blocker");
    let blocked = fixtures::issue("rm-event-blocked");

    storage.create_issue(&blocker, "tester").unwrap();
    storage.create_issue(&blocked, "tester").unwrap();

    storage
        .add_dependency(
            &blocked.id,
            &blocker.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();

    storage
        .remove_dependency(&blocked.id, &blocker.id, "remover")
        .unwrap();

    let details = storage
        .get_issue_details(&blocked.id, true, true, 200)
        .unwrap()
        .expect("issue exists");

    // Find dependency removed event
    let rm_event = details
        .events
        .iter()
        .find(|e| e.event_type == EventType::DependencyRemoved);

    assert!(rm_event.is_some());
    let event = rm_event.unwrap();
    assert_eq!(event.actor, "remover");
}

#[test]
fn remove_dependency_marks_dirty() {
    let mut storage = test_db();

    let blocker = fixtures::issue("rm-dirty-blocker");
    let blocked = fixtures::issue("rm-dirty-blocked");

    storage.create_issue(&blocker, "tester").unwrap();
    storage.create_issue(&blocked, "tester").unwrap();

    storage
        .add_dependency(
            &blocked.id,
            &blocker.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();

    // Clear dirty flags
    let ids: Vec<String> = vec![blocker.id.clone(), blocked.id.clone()];
    storage.clear_dirty_flags(&ids).unwrap();

    // Remove dependency
    storage
        .remove_dependency(&blocked.id, &blocker.id, "tester")
        .unwrap();

    let dirty_ids = storage.get_dirty_issue_ids().unwrap();
    assert!(dirty_ids.contains(&blocked.id));
}

// ============================================================================
// GET DEPENDENCIES/DEPENDENTS TESTS
// ============================================================================

#[test]
fn get_dependencies_empty_for_new_issue() {
    let mut storage = test_db();

    let issue = fixtures::issue("deps-empty");
    storage.create_issue(&issue, "tester").unwrap();

    let deps = storage.get_dependencies(&issue.id).unwrap();
    assert!(deps.is_empty());
}

#[test]
fn get_dependencies_returns_all() {
    let mut storage = test_db();

    let dep1 = fixtures::issue("dep-target-1");
    let dep2 = fixtures::issue("dep-target-2");
    let dep3 = fixtures::issue("dep-target-3");
    let main = fixtures::issue("dep-main");

    storage.create_issue(&dep1, "tester").unwrap();
    storage.create_issue(&dep2, "tester").unwrap();
    storage.create_issue(&dep3, "tester").unwrap();
    storage.create_issue(&main, "tester").unwrap();

    storage
        .add_dependency(
            &main.id,
            &dep1.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();
    storage
        .add_dependency(
            &main.id,
            &dep2.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();
    storage
        .add_dependency(
            &main.id,
            &dep3.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();

    let deps = storage.get_dependencies(&main.id).unwrap();
    assert_eq!(deps.len(), 3);
    assert!(deps.contains(&dep1.id));
    assert!(deps.contains(&dep2.id));
    assert!(deps.contains(&dep3.id));
}

#[test]
fn get_dependents_empty_for_new_issue() {
    let mut storage = test_db();

    let issue = fixtures::issue("dependents-empty");
    storage.create_issue(&issue, "tester").unwrap();

    let dependents = storage.get_dependents(&issue.id).unwrap();
    assert!(dependents.is_empty());
}

#[test]
fn get_dependents_returns_all() {
    let mut storage = test_db();

    let blocker = fixtures::issue("blocker-main");
    let dependent1 = fixtures::issue("dependent-1");
    let dependent2 = fixtures::issue("dependent-2");
    let dependent3 = fixtures::issue("dependent-3");

    storage.create_issue(&blocker, "tester").unwrap();
    storage.create_issue(&dependent1, "tester").unwrap();
    storage.create_issue(&dependent2, "tester").unwrap();
    storage.create_issue(&dependent3, "tester").unwrap();

    storage
        .add_dependency(
            &dependent1.id,
            &blocker.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();
    storage
        .add_dependency(
            &dependent2.id,
            &blocker.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();
    storage
        .add_dependency(
            &dependent3.id,
            &blocker.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();

    let dependents = storage.get_dependents(&blocker.id).unwrap();
    assert_eq!(dependents.len(), 3);
    assert!(dependents.contains(&dependent1.id));
    assert!(dependents.contains(&dependent2.id));
    assert!(dependents.contains(&dependent3.id));
}

// ============================================================================
// CYCLE DETECTION TESTS
// ============================================================================

#[test]
fn would_create_cycle_detects_simple_cycle() {
    let mut storage = test_db();

    let a = fixtures::issue("cycle-a");
    let b = fixtures::issue("cycle-b");

    storage.create_issue(&a, "tester").unwrap();
    storage.create_issue(&b, "tester").unwrap();

    // A depends on B
    storage
        .add_dependency(&a.id, &b.id, DependencyType::Blocks.as_str(), "tester")
        .unwrap();

    // Would B depending on A create a cycle? Yes!
    let would_cycle = storage.would_create_cycle(&b.id, &a.id).unwrap();
    assert!(would_cycle);
}

#[test]
fn would_create_cycle_transitive_detection() {
    let mut storage = test_db();

    let a = fixtures::issue("trans-a");
    let b = fixtures::issue("trans-b");
    let c = fixtures::issue("trans-c");

    storage.create_issue(&a, "tester").unwrap();
    storage.create_issue(&b, "tester").unwrap();
    storage.create_issue(&c, "tester").unwrap();

    // A -> B -> C (A depends on B, B depends on C)
    storage
        .add_dependency(&a.id, &b.id, DependencyType::Blocks.as_str(), "tester")
        .unwrap();
    storage
        .add_dependency(&b.id, &c.id, DependencyType::Blocks.as_str(), "tester")
        .unwrap();

    // Would C depending on A create a cycle? Yes!
    let would_cycle = storage.would_create_cycle(&c.id, &a.id).unwrap();
    assert!(would_cycle);
}

#[test]
fn would_create_cycle_no_cycle() {
    let mut storage = test_db();

    let a = fixtures::issue("nocycle-a");
    let b = fixtures::issue("nocycle-b");
    let c = fixtures::issue("nocycle-c");

    storage.create_issue(&a, "tester").unwrap();
    storage.create_issue(&b, "tester").unwrap();
    storage.create_issue(&c, "tester").unwrap();

    // A -> B (A depends on B)
    storage
        .add_dependency(&a.id, &b.id, DependencyType::Blocks.as_str(), "tester")
        .unwrap();

    // Would A depending on C create a cycle? No (C is unconnected)
    let would_cycle = storage.would_create_cycle(&a.id, &c.id).unwrap();
    assert!(!would_cycle);

    // Would C depending on B create a cycle? No
    let would_cycle = storage.would_create_cycle(&c.id, &b.id).unwrap();
    assert!(!would_cycle);
}

#[test]
fn detect_all_cycles_finds_cycles() {
    let mut storage = test_db();

    let a = fixtures::issue("all-cycles-a");
    let b = fixtures::issue("all-cycles-b");
    let c = fixtures::issue("all-cycles-c");

    storage.create_issue(&a, "tester").unwrap();
    storage.create_issue(&b, "tester").unwrap();
    storage.create_issue(&c, "tester").unwrap();

    // Create cycle: A -> B -> C -> A
    storage
        .add_dependency(&a.id, &b.id, DependencyType::Related.as_str(), "tester")
        .unwrap();
    storage
        .add_dependency(&b.id, &c.id, DependencyType::Related.as_str(), "tester")
        .unwrap();
    storage
        .add_dependency(&c.id, &a.id, DependencyType::Related.as_str(), "tester")
        .unwrap();

    let cycles = storage.detect_all_cycles().unwrap();
    assert!(!cycles.is_empty());

    // At least one cycle should contain all three issues
    let has_full_cycle = cycles
        .iter()
        .any(|cycle| cycle.contains(&a.id) && cycle.contains(&b.id) && cycle.contains(&c.id));
    assert!(has_full_cycle);
}

#[test]
fn detect_all_cycles_empty_when_no_cycles() {
    let mut storage = test_db();

    let a = fixtures::issue("no-cycles-a");
    let b = fixtures::issue("no-cycles-b");
    let c = fixtures::issue("no-cycles-c");

    storage.create_issue(&a, "tester").unwrap();
    storage.create_issue(&b, "tester").unwrap();
    storage.create_issue(&c, "tester").unwrap();

    // Linear chain: A -> B -> C (no cycles)
    storage
        .add_dependency(&a.id, &b.id, DependencyType::Blocks.as_str(), "tester")
        .unwrap();
    storage
        .add_dependency(&b.id, &c.id, DependencyType::Blocks.as_str(), "tester")
        .unwrap();

    let cycles = storage.detect_all_cycles().unwrap();
    assert!(cycles.is_empty());
}

// ============================================================================
// DEEP HIERARCHY TESTS (5+ levels)
// ============================================================================

#[test]
fn deep_hierarchy_five_levels() {
    let mut storage = test_db();

    // Create 6 issues for 5 levels of dependencies
    let level0 = fixtures::issue("deep-l0");
    let level1 = fixtures::issue("deep-l1");
    let level2 = fixtures::issue("deep-l2");
    let level3 = fixtures::issue("deep-l3");
    let level4 = fixtures::issue("deep-l4");
    let level5 = fixtures::issue("deep-l5");

    storage.create_issue(&level0, "tester").unwrap();
    storage.create_issue(&level1, "tester").unwrap();
    storage.create_issue(&level2, "tester").unwrap();
    storage.create_issue(&level3, "tester").unwrap();
    storage.create_issue(&level4, "tester").unwrap();
    storage.create_issue(&level5, "tester").unwrap();

    // Create chain: l0 -> l1 -> l2 -> l3 -> l4 -> l5
    storage
        .add_dependency(
            &level0.id,
            &level1.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();
    storage
        .add_dependency(
            &level1.id,
            &level2.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();
    storage
        .add_dependency(
            &level2.id,
            &level3.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();
    storage
        .add_dependency(
            &level3.id,
            &level4.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();
    storage
        .add_dependency(
            &level4.id,
            &level5.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();

    // Verify each level has correct dependencies
    assert_eq!(
        storage.get_dependencies(&level0.id).unwrap(),
        vec![level1.id.clone()]
    );
    assert_eq!(
        storage.get_dependencies(&level1.id).unwrap(),
        vec![level2.id.clone()]
    );
    assert_eq!(
        storage.get_dependencies(&level4.id).unwrap(),
        vec![level5.id.clone()]
    );
    assert!(storage.get_dependencies(&level5.id).unwrap().is_empty());

    // Would l5 -> l0 create a cycle? Yes!
    let would_cycle = storage.would_create_cycle(&level5.id, &level0.id).unwrap();
    assert!(would_cycle);
}

#[test]
fn deep_hierarchy_transitive_blocked() {
    let mut storage = test_db();

    // Create chain where root is blocked (closed status blocks nothing,
    // but open status on dependency means dependent is blocked)
    let root = fixtures::issue("root-blocked");
    let l1 = fixtures::issue("l1-blocked");
    let l2 = fixtures::issue("l2-blocked");
    let l3 = fixtures::issue("l3-blocked");
    let l4 = fixtures::issue("l4-blocked");
    let l5 = fixtures::issue("l5-blocked");

    storage.create_issue(&root, "tester").unwrap();
    storage.create_issue(&l1, "tester").unwrap();
    storage.create_issue(&l2, "tester").unwrap();
    storage.create_issue(&l3, "tester").unwrap();
    storage.create_issue(&l4, "tester").unwrap();
    storage.create_issue(&l5, "tester").unwrap();

    // Chain: l5 -> l4 -> l3 -> l2 -> l1 -> root (l5 transitively blocked by root)
    storage
        .add_dependency(&l5.id, &l4.id, DependencyType::Blocks.as_str(), "tester")
        .unwrap();
    storage
        .add_dependency(&l4.id, &l3.id, DependencyType::Blocks.as_str(), "tester")
        .unwrap();
    storage
        .add_dependency(&l3.id, &l2.id, DependencyType::Blocks.as_str(), "tester")
        .unwrap();
    storage
        .add_dependency(&l2.id, &l1.id, DependencyType::Blocks.as_str(), "tester")
        .unwrap();
    storage
        .add_dependency(&l1.id, &root.id, DependencyType::Blocks.as_str(), "tester")
        .unwrap();

    // Verify the chain is set up correctly
    assert_eq!(storage.get_dependencies(&l5.id).unwrap().len(), 1);
    assert_eq!(
        storage.get_dependencies(&l1.id).unwrap(),
        vec![root.id.clone()]
    );

    // l5 should be blocked because it transitively depends on root (open)
    // Need to rebuild blocked cache first
    storage.rebuild_blocked_cache(true).unwrap();

    let blocked_ids = blocked_ids_for(&storage);
    // All issues except root should be blocked
    assert!(blocked_ids.contains(&l1.id));
    assert!(blocked_ids.contains(&l2.id));
    assert!(blocked_ids.contains(&l3.id));
    assert!(blocked_ids.contains(&l4.id));
    assert!(blocked_ids.contains(&l5.id));
    assert!(!blocked_ids.contains(&root.id));
}

// ============================================================================
// DIAMOND PATTERN TESTS
// ============================================================================

#[test]
fn diamond_pattern_dependencies() {
    let mut storage = test_db();

    // Diamond: A depends on B and C, both B and C depend on D
    //      A
    //     / \
    //    B   C
    //     \ /
    //      D
    let a = fixtures::issue("diamond-a");
    let b = fixtures::issue("diamond-b");
    let c = fixtures::issue("diamond-c");
    let d = fixtures::issue("diamond-d");

    storage.create_issue(&a, "tester").unwrap();
    storage.create_issue(&b, "tester").unwrap();
    storage.create_issue(&c, "tester").unwrap();
    storage.create_issue(&d, "tester").unwrap();

    storage
        .add_dependency(&a.id, &b.id, DependencyType::Blocks.as_str(), "tester")
        .unwrap();
    storage
        .add_dependency(&a.id, &c.id, DependencyType::Blocks.as_str(), "tester")
        .unwrap();
    storage
        .add_dependency(&b.id, &d.id, DependencyType::Blocks.as_str(), "tester")
        .unwrap();
    storage
        .add_dependency(&c.id, &d.id, DependencyType::Blocks.as_str(), "tester")
        .unwrap();

    // Verify structure
    let a_deps = storage.get_dependencies(&a.id).unwrap();
    assert_eq!(a_deps.len(), 2);
    assert!(a_deps.contains(&b.id));
    assert!(a_deps.contains(&c.id));

    let d_dependents = storage.get_dependents(&d.id).unwrap();
    assert_eq!(d_dependents.len(), 2);
    assert!(d_dependents.contains(&b.id));
    assert!(d_dependents.contains(&c.id));

    // Would D -> A create a cycle? Yes (through either path)
    let would_cycle = storage.would_create_cycle(&d.id, &a.id).unwrap();
    assert!(would_cycle);

    // No cycles currently exist
    let cycles = storage.detect_all_cycles().unwrap();
    assert!(cycles.is_empty());
}

#[test]
fn diamond_pattern_blocked_propagation() {
    let mut storage = test_db();

    // Same diamond pattern, D is open so everything is blocked
    let a = fixtures::issue("dblock-a");
    let b = fixtures::issue("dblock-b");
    let c = fixtures::issue("dblock-c");
    let d = fixtures::issue("dblock-d");

    storage.create_issue(&a, "tester").unwrap();
    storage.create_issue(&b, "tester").unwrap();
    storage.create_issue(&c, "tester").unwrap();
    storage.create_issue(&d, "tester").unwrap();

    storage
        .add_dependency(&a.id, &b.id, DependencyType::Blocks.as_str(), "tester")
        .unwrap();
    storage
        .add_dependency(&a.id, &c.id, DependencyType::Blocks.as_str(), "tester")
        .unwrap();
    storage
        .add_dependency(&b.id, &d.id, DependencyType::Blocks.as_str(), "tester")
        .unwrap();
    storage
        .add_dependency(&c.id, &d.id, DependencyType::Blocks.as_str(), "tester")
        .unwrap();

    // Rebuild blocked cache
    storage.rebuild_blocked_cache(true).unwrap();

    let blocked_ids = blocked_ids_for(&storage);
    // A, B, C are all blocked (depend on D which is open)
    assert!(blocked_ids.contains(&a.id));
    assert!(blocked_ids.contains(&b.id));
    assert!(blocked_ids.contains(&c.id));
    assert!(!blocked_ids.contains(&d.id)); // D has no blockers
}

// ============================================================================
// BLOCKED CACHE INVALIDATION TESTS
// ============================================================================

#[test]
fn blocked_cache_invalidated_on_add_dependency() {
    let mut storage = test_db();

    let blocker = fixtures::issue("cache-blocker");
    let blocked = fixtures::issue("cache-blocked");

    storage.create_issue(&blocker, "tester").unwrap();
    storage.create_issue(&blocked, "tester").unwrap();

    // Initially no blocked issues
    storage.rebuild_blocked_cache(true).unwrap();
    let initial_blocked = blocked_ids_for(&storage);
    assert!(!initial_blocked.contains(&blocked.id));

    // Add dependency - cache should be invalidated and rebuilt
    storage
        .add_dependency(
            &blocked.id,
            &blocker.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();

    // After adding dependency, blocked should be in blocked cache
    let after_blocked = blocked_ids_for(&storage);
    assert!(after_blocked.contains(&blocked.id));
}

#[test]
fn blocked_cache_invalidated_on_remove_dependency() {
    let mut storage = test_db();

    let blocker = fixtures::issue("rm-cache-blocker");
    let blocked = fixtures::issue("rm-cache-blocked");

    storage.create_issue(&blocker, "tester").unwrap();
    storage.create_issue(&blocked, "tester").unwrap();

    // Add dependency
    storage
        .add_dependency(
            &blocked.id,
            &blocker.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();

    // Blocked should be in cache
    let before_blocked = blocked_ids_for(&storage);
    assert!(before_blocked.contains(&blocked.id));

    // Remove dependency - cache should be invalidated
    storage
        .remove_dependency(&blocked.id, &blocker.id, "tester")
        .unwrap();

    // After removing, blocked should not be in cache
    let after_blocked = blocked_ids_for(&storage);
    assert!(!after_blocked.contains(&blocked.id));
}

#[test]
fn blocked_cache_reflects_status_changes() {
    let mut storage = test_db();

    let blocker = fixtures::issue("status-blocker");
    let blocked = fixtures::issue("status-blocked");

    storage.create_issue(&blocker, "tester").unwrap();
    storage.create_issue(&blocked, "tester").unwrap();

    storage
        .add_dependency(
            &blocked.id,
            &blocker.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();

    // Initially blocked (blocker is open)
    let blocked_ids = blocked_ids_for(&storage);
    assert!(blocked_ids.contains(&blocked.id));

    // Close the blocker
    let update = beads_rust::storage::IssueUpdate {
        status: Some(Status::Closed),
        ..Default::default()
    };
    storage
        .update_issue(&blocker.id, &update, "tester")
        .unwrap();

    // After closing blocker, blocked should no longer be blocked
    let blocked_ids = blocked_ids_for(&storage);
    assert!(!blocked_ids.contains(&blocked.id));
}

// ============================================================================
// REMOVE ALL DEPENDENCIES TESTS
// ============================================================================

#[test]
fn remove_all_dependencies_clears_all() {
    let mut storage = test_db();

    let main = fixtures::issue("rm-all-main");
    let dep1 = fixtures::issue("rm-all-dep1");
    let dep2 = fixtures::issue("rm-all-dep2");
    let dependent = fixtures::issue("rm-all-dependent");

    storage.create_issue(&main, "tester").unwrap();
    storage.create_issue(&dep1, "tester").unwrap();
    storage.create_issue(&dep2, "tester").unwrap();
    storage.create_issue(&dependent, "tester").unwrap();

    // main depends on dep1 and dep2
    storage
        .add_dependency(
            &main.id,
            &dep1.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();
    storage
        .add_dependency(
            &main.id,
            &dep2.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();
    // dependent depends on main
    storage
        .add_dependency(
            &dependent.id,
            &main.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();

    // Remove all dependencies for main
    let count = storage.remove_all_dependencies(&main.id, "tester").unwrap();
    assert!(count >= 2); // At least the outgoing deps

    // main should have no outgoing deps
    let deps = storage.get_dependencies(&main.id).unwrap();
    assert!(deps.is_empty());

    // dependent should have no dep on main anymore
    let dependents = storage.get_dependents(&main.id).unwrap();
    assert!(dependents.is_empty());
}

// ============================================================================
// DEPENDENCY COUNTS TESTS
// ============================================================================

#[test]
fn count_dependencies_returns_correct_count() {
    let mut storage = test_db();

    let main = fixtures::issue("count-main");
    let dep1 = fixtures::issue("count-dep1");
    let dep2 = fixtures::issue("count-dep2");

    storage.create_issue(&main, "tester").unwrap();
    storage.create_issue(&dep1, "tester").unwrap();
    storage.create_issue(&dep2, "tester").unwrap();

    assert_eq!(storage.count_dependencies(&main.id).unwrap(), 0);

    storage
        .add_dependency(
            &main.id,
            &dep1.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();
    assert_eq!(storage.count_dependencies(&main.id).unwrap(), 1);

    storage
        .add_dependency(
            &main.id,
            &dep2.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();
    assert_eq!(storage.count_dependencies(&main.id).unwrap(), 2);
}

#[test]
fn count_dependents_returns_correct_count() {
    let mut storage = test_db();

    let blocker = fixtures::issue("count-blocker");
    let dependent1 = fixtures::issue("count-dependent1");
    let dependent2 = fixtures::issue("count-dependent2");

    storage.create_issue(&blocker, "tester").unwrap();
    storage.create_issue(&dependent1, "tester").unwrap();
    storage.create_issue(&dependent2, "tester").unwrap();

    assert_eq!(storage.count_dependents(&blocker.id).unwrap(), 0);

    storage
        .add_dependency(
            &dependent1.id,
            &blocker.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();
    assert_eq!(storage.count_dependents(&blocker.id).unwrap(), 1);

    storage
        .add_dependency(
            &dependent2.id,
            &blocker.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();
    assert_eq!(storage.count_dependents(&blocker.id).unwrap(), 2);
}

// ============================================================================
// READY LIST INTERACTION TESTS
// ============================================================================

#[test]
fn blocked_issues_excluded_from_ready_list() {
    let mut storage = test_db();

    let blocker = fixtures::issue("ready-blocker");
    let blocked = fixtures::issue("ready-blocked");
    let unblocked = fixtures::issue("ready-unblocked");

    storage.create_issue(&blocker, "tester").unwrap();
    storage.create_issue(&blocked, "tester").unwrap();
    storage.create_issue(&unblocked, "tester").unwrap();

    storage
        .add_dependency(
            &blocked.id,
            &blocker.id,
            DependencyType::Blocks.as_str(),
            "tester",
        )
        .unwrap();

    let filters = ReadyFilters::default();
    let ready = storage
        .get_ready_issues(&filters, ReadySortPolicy::Hybrid)
        .unwrap();
    let ready_ids: Vec<_> = ready.iter().map(|i| i.id.clone()).collect();

    // blocker and unblocked are ready, blocked is not
    assert!(ready_ids.contains(&blocker.id));
    assert!(ready_ids.contains(&unblocked.id));
    assert!(!ready_ids.contains(&blocked.id));
}
