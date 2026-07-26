//! Storage unit tests for dependency graph operations.
//!
//! Tests: `add_dependency`, `remove_dependency`, `get_dependencies`, `get_dependents`,
//! cycle detection, deep hierarchies, diamond patterns, blocked cache invalidation.
//! Real `SQLite`, no mocks.

#![allow(clippy::similar_names)]

mod common;

use beads_rust::model::{DependencyType, EventType, Status};
use beads_rust::storage::{ReadyFilters, ReadySortPolicy, SqliteStorage};
#[allow(unused_imports)]
use common::ordering::{
    assert_contains_exactly_one, assert_hybrid_ordered, assert_no_duplicate_ids,
    assert_oldest_first, assert_ordered_by, assert_priority_ordered,
};
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
    let would_cycle = storage.would_create_cycle(&b.id, &a.id, true).unwrap();
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
    let would_cycle = storage.would_create_cycle(&c.id, &a.id, true).unwrap();
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
    let would_cycle = storage.would_create_cycle(&a.id, &c.id, true).unwrap();
    assert!(!would_cycle);

    // Would C depending on B create a cycle? No
    let would_cycle = storage.would_create_cycle(&c.id, &b.id, true).unwrap();
    assert!(!would_cycle);
}

#[test]
fn would_create_cycle_mixed_types() {
    let mut storage = test_db();

    let a = fixtures::issue("mixed-a");
    let b = fixtures::issue("mixed-b");

    storage.create_issue(&a, "tester").unwrap();
    storage.create_issue(&b, "tester").unwrap();

    // A -> related -> B
    storage
        .add_dependency(&a.id, &b.id, DependencyType::Related.as_str(), "tester")
        .unwrap();

    // Would B -> blocks -> A create a BLOCKING cycle?
    // Path B -> ... -> A? No, because A->B is 'related' (non-blocking).
    // So blocking_only=true should return false.
    let blocking_cycle = storage.would_create_cycle(&b.id, &a.id, true).unwrap();
    assert!(
        !blocking_cycle,
        "Should not detect blocking cycle through related dependency"
    );

    // blocking_only=false should detect it (graph reachability)
    let any_cycle = storage.would_create_cycle(&b.id, &a.id, false).unwrap();
    assert!(any_cycle, "Should detect general graph cycle");
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

#[test]
fn detect_all_cycles_finds_long_cycle_beyond_legacy_depth_cap() {
    let mut storage = test_db();
    let issues: Vec<_> = (0..25)
        .map(|index| fixtures::issue(&format!("long cycle {index:02}")))
        .collect();
    let ids: Vec<_> = issues.iter().map(|issue| issue.id.clone()).collect();

    for issue in &issues {
        storage.create_issue(issue, "tester").unwrap();
    }

    for (index, id) in ids.iter().enumerate() {
        let next = &ids[(index + 1) % ids.len()];
        storage
            .add_dependency(id, next, DependencyType::Related.as_str(), "tester")
            .unwrap();
    }

    let cycles = storage.detect_all_cycles().unwrap();

    assert_eq!(cycles.len(), 1);
    assert_eq!(cycles[0].first(), cycles[0].last());
    for id in &ids {
        assert!(
            cycles[0].contains(id),
            "long cycle witness should include {id}"
        );
    }
}

#[test]
fn detect_all_cycles_collapses_dense_component_to_witness() {
    let mut storage = test_db();
    let issues: Vec<_> = (0..8)
        .map(|index| fixtures::issue(&format!("dense cycle {index}")))
        .collect();
    let ids: Vec<_> = issues.iter().map(|issue| issue.id.clone()).collect();

    for issue in &issues {
        storage.create_issue(issue, "tester").unwrap();
    }

    for from in &ids {
        for to in &ids {
            if from != to {
                storage
                    .add_dependency(from, to, DependencyType::Related.as_str(), "tester")
                    .unwrap();
            }
        }
    }

    let cycles = storage.detect_all_cycles().unwrap();

    assert_eq!(cycles.len(), 1);
    assert_eq!(cycles[0].first(), cycles[0].last());
    assert!(cycles[0].len() >= 3);
    for id in &cycles[0] {
        assert!(ids.contains(id));
    }
}

#[test]
fn dependency_cycle_report_separates_active_from_archived_and_filters_blocking() {
    let mut storage = test_db();

    let active_a = fixtures::issue("active-cycle-a");
    let active_b = fixtures::issue("active-cycle-b");
    let archived_a = fixtures::IssueBuilder::new("archived-cycle-a")
        .with_status(Status::Closed)
        .build();
    let archived_b = fixtures::IssueBuilder::new("archived-cycle-b")
        .with_status(Status::Closed)
        .build();
    let related_a = fixtures::issue("related-cycle-a");
    let related_b = fixtures::issue("related-cycle-b");

    for issue in [
        &active_a,
        &active_b,
        &archived_a,
        &archived_b,
        &related_a,
        &related_b,
    ] {
        storage.create_issue(issue, "tester").unwrap();
    }

    storage
        .add_dependency(
            &active_a.id,
            &active_b.id,
            DependencyType::Related.as_str(),
            "tester",
        )
        .unwrap();
    storage
        .add_dependency(
            &active_b.id,
            &active_a.id,
            DependencyType::Related.as_str(),
            "tester",
        )
        .unwrap();
    storage
        .add_dependency(
            &archived_a.id,
            &archived_b.id,
            DependencyType::Related.as_str(),
            "tester",
        )
        .unwrap();
    storage
        .add_dependency(
            &archived_b.id,
            &archived_a.id,
            DependencyType::Related.as_str(),
            "tester",
        )
        .unwrap();
    storage
        .add_dependency(
            &related_a.id,
            &related_b.id,
            DependencyType::Related.as_str(),
            "tester",
        )
        .unwrap();
    storage
        .add_dependency(
            &related_b.id,
            &related_a.id,
            DependencyType::Related.as_str(),
            "tester",
        )
        .unwrap();

    let all_report = storage.detect_dependency_cycle_report(false).unwrap();
    assert_eq!(all_report.active_cycles.len(), 2);
    assert_eq!(all_report.archived_closed_cycles.len(), 1);
    assert!(
        all_report
            .active_cycles
            .iter()
            .any(|cycle| cycle.contains(&active_a.id) && cycle.contains(&active_b.id))
    );
    assert!(
        all_report
            .active_cycles
            .iter()
            .any(|cycle| cycle.contains(&related_a.id) && cycle.contains(&related_b.id))
    );
    assert!(
        all_report
            .archived_closed_cycles
            .iter()
            .any(|cycle| cycle.contains(&archived_a.id) && cycle.contains(&archived_b.id))
    );

    let blocking_report = storage.detect_dependency_cycle_report(true).unwrap();
    assert!(blocking_report.active_cycles.is_empty());
    assert!(blocking_report.archived_closed_cycles.is_empty());
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
    let would_cycle = storage
        .would_create_cycle(&level5.id, &level0.id, true)
        .unwrap();
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
    let would_cycle = storage.would_create_cycle(&d.id, &a.id, true).unwrap();
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

// ============================================================================
// beads_rust-uelt: dep-type matrix coverage (added 2026-05-09)
// Verifies the single-parent rule applies ONLY to `parent-child`, not to
// any other dep type. Each test is paired with its specific assertion.
// ============================================================================

/// uelt: adding multiple `blocks` deps to the SAME issue must succeed.
/// (single-parent rule does NOT apply to `blocks`.)
#[test]
fn test_dep_add_multiple_blocks_deps_allowed() {
    let mut storage = test_db();
    let blocker_a = fixtures::issue("multi-blocks-a");
    let blocker_b = fixtures::issue("multi-blocks-b");
    let blocker_c = fixtures::issue("multi-blocks-c");
    let target = fixtures::issue("multi-blocks-target");

    storage.create_issue(&blocker_a, "tester").unwrap();
    storage.create_issue(&blocker_b, "tester").unwrap();
    storage.create_issue(&blocker_c, "tester").unwrap();
    storage.create_issue(&target, "tester").unwrap();

    storage
        .add_dependency(&target.id, &blocker_a.id, "blocks", "tester")
        .expect("first blocks dep must succeed");
    storage
        .add_dependency(&target.id, &blocker_b.id, "blocks", "tester")
        .expect("second blocks dep must also succeed (no single-blocker rule)");
    storage
        .add_dependency(&target.id, &blocker_c.id, "blocks", "tester")
        .expect("third blocks dep must also succeed");

    let deps = storage.get_dependencies(&target.id).unwrap();
    assert_eq!(
        deps.len(),
        3,
        "target should have 3 blocks deps; got {deps:?}"
    );
}

/// uelt: an issue can have parent X AND be blocked-by Y simultaneously,
/// because they are different dep types and the single-parent rule applies
/// only to `parent-child`.
#[test]
fn test_dep_add_blocks_alongside_parent_child_allowed() {
    let mut storage = test_db();
    let parent = fixtures::issue("mixed-parent");
    let blocker = fixtures::issue("mixed-blocker");
    let child = fixtures::issue("mixed-child");

    storage.create_issue(&parent, "tester").unwrap();
    storage.create_issue(&blocker, "tester").unwrap();
    storage.create_issue(&child, "tester").unwrap();

    storage
        .add_dependency(&child.id, &parent.id, "parent-child", "tester")
        .expect("parent-child must succeed");
    storage
        .add_dependency(&child.id, &blocker.id, "blocks", "tester")
        .expect("blocks alongside parent-child must succeed");

    let deps = storage.get_dependencies(&child.id).unwrap();
    assert_eq!(
        deps.len(),
        2,
        "child should have parent-child + blocks = 2 deps; got {deps:?}"
    );
}

/// uelt: removing the parent-child edge must allow a subsequent
/// parent-child add to succeed (no stale "was already replaced" state).
#[test]
fn test_dep_remove_parent_allows_subsequent_add() {
    let mut storage = test_db();
    let parent_a = fixtures::issue("remove-add-parent-a");
    let parent_b = fixtures::issue("remove-add-parent-b");
    let child = fixtures::issue("remove-add-child");

    storage.create_issue(&parent_a, "tester").unwrap();
    storage.create_issue(&parent_b, "tester").unwrap();
    storage.create_issue(&child, "tester").unwrap();

    // Initial parent
    storage
        .add_dependency(&child.id, &parent_a.id, "parent-child", "tester")
        .expect("first parent-child must succeed");

    // Remove
    storage
        .remove_dependency(&child.id, &parent_a.id, "tester")
        .expect("remove must succeed");

    let deps = storage.get_dependencies(&child.id).unwrap();
    assert_eq!(deps.len(), 0, "child should have no deps after remove");

    // New parent — must succeed (no stale validator state)
    storage
        .add_dependency(&child.id, &parent_b.id, "parent-child", "tester")
        .expect("subsequent parent-child must succeed after remove");

    let deps = storage.get_dependencies(&child.id).unwrap();
    assert_eq!(deps.len(), 1, "child should have new parent");
    assert_eq!(deps[0], parent_b.id);
}

/// uelt: every supported dep type round-trips through `add_dependency`,
/// `get_dependencies`, and (for blocking types) blocked-cache rebuild.
/// Exercises the full {blocks, parent-child, related, conditional-blocks,
/// waits-for, discovered-from, replies-to, relates-to, duplicates,
/// supersedes, caused-by} matrix.
#[test]
fn test_dep_add_each_supported_type_against_full_matrix() {
    let mut storage = test_db();
    let target = fixtures::issue("matrix-target");
    storage.create_issue(&target, "tester").unwrap();

    // For each dep type, create a fresh peer (so single-parent rule doesn't
    // collide with multiple parent-child variants — only one parent allowed).
    let dep_types: &[&str] = &[
        "blocks",
        "parent-child", // ← exactly one allowed
        "related",
        "conditional-blocks",
        "waits-for",
        "discovered-from",
        "replies-to",
        "relates-to",
        "duplicates",
        "supersedes",
        "caused-by",
    ];

    let mut added_count = 0;
    let mut parent_child_used = false;

    for (i, dep_type) in dep_types.iter().enumerate() {
        let peer = fixtures::issue(&format!("matrix-peer-{i}"));
        storage.create_issue(&peer, "tester").unwrap();

        let result = storage.add_dependency(&target.id, &peer.id, dep_type, "tester");

        if *dep_type == "parent-child" {
            // First parent-child must succeed; if we hit it twice (we don't,
            // but defensively), only the first should win
            if parent_child_used {
                assert!(result.is_err(), "second parent-child must reject; got Ok");
            } else {
                result.unwrap_or_else(|e| panic!("parent-child should succeed: {e}"));
                parent_child_used = true;
                added_count += 1;
            }
        } else {
            result.unwrap_or_else(|e| panic!("{dep_type} should succeed: {e}"));
            added_count += 1;
        }
    }

    let deps = storage.get_dependencies(&target.id).unwrap();
    assert_eq!(
        deps.len(),
        added_count,
        "expected {added_count} deps, got {deps:?}"
    );
}

// ============================================================================
// DIRECTIONAL BATCH QUERIES (beads_rust-mf72)
// ============================================================================

/// `get_blocking_dependencies_for_issue_ids` backs `br graph --dependencies`
/// and must be the exact inverse of the dependents query that backs the
/// default walk. Asserting the inverse relation — rather than each query's
/// rows independently — is what keeps the two graph directions consistent.
#[test]
fn blocking_dependencies_batch_is_the_inverse_of_blocking_dependents() {
    let mut storage = test_db();

    // A depends on B, B depends on C. C blocks B blocks A.
    let (a, b, c) = (
        fixtures::issue("dir-a"),
        fixtures::issue("dir-b"),
        fixtures::issue("dir-c"),
    );
    for issue in [&a, &b, &c] {
        storage.create_issue(issue, "tester").unwrap();
    }
    storage
        .add_dependency(&a.id, &b.id, DependencyType::Blocks.as_str(), "tester")
        .unwrap();
    storage
        .add_dependency(&b.id, &c.id, DependencyType::Blocks.as_str(), "tester")
        .unwrap();

    let ids: Vec<String> = vec![a.id.clone(), b.id.clone(), c.id.clone()];
    let dependents = storage.get_blocking_dependents_for_issue_ids(&ids).unwrap();
    let dependencies = storage
        .get_blocking_dependencies_for_issue_ids(&ids)
        .unwrap();

    let neighbours = |map: &std::collections::HashMap<
        String,
        Vec<beads_rust::format::IssueWithDependencyMetadata>,
    >,
                      id: &str|
     -> Vec<String> {
        let mut out: Vec<String> = map
            .get(id)
            .map(|v| v.iter().map(|m| m.id.clone()).collect())
            .unwrap_or_default();
        out.sort();
        out
    };

    // Dependents: who is blocked by me.
    assert_eq!(neighbours(&dependents, &c.id), vec![b.id.clone()]);
    assert_eq!(neighbours(&dependents, &b.id), vec![a.id.clone()]);
    assert!(neighbours(&dependents, &a.id).is_empty());

    // Dependencies: who blocks me. Exactly the reverse of the above.
    assert_eq!(neighbours(&dependencies, &a.id), vec![b.id.clone()]);
    assert_eq!(neighbours(&dependencies, &b.id), vec![c.id.clone()]);
    assert!(neighbours(&dependencies, &c.id).is_empty());

    // State the inverse relation directly: `x` is a dependent of `y` if and
    // only if `y` is a dependency of `x`.
    for id in &ids {
        for neighbour in neighbours(&dependents, id) {
            assert!(
                neighbours(&dependencies, &neighbour).contains(id),
                "{neighbour} lists {id} as a dependent but not the reverse"
            );
        }
        for neighbour in neighbours(&dependencies, id) {
            assert!(
                neighbours(&dependents, &neighbour).contains(id),
                "{neighbour} lists {id} as a dependency but not the reverse"
            );
        }
    }
}

#[test]
fn blocking_dependencies_batch_handles_empty_input_and_unknown_ids() {
    let storage = test_db();
    assert!(
        storage
            .get_blocking_dependencies_for_issue_ids(&[])
            .unwrap()
            .is_empty()
    );
    assert!(
        storage
            .get_blocking_dependencies_for_issue_ids(&["dir-missing".to_string()])
            .unwrap()
            .is_empty()
    );
}
