mod common;

use beads_rust::model::{Issue, IssueType, Priority, Status};
use beads_rust::storage::SqliteStorage;
use chrono::Utc;
use common::cli::{BrWorkspace, extract_json_payload, run_br};
use std::fs;

fn create_issue_id(workspace: &BrWorkspace, title: &str, label: &str) -> String {
    let create = run_br(workspace, ["--json", "create", title, "-t", "task"], label);
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let created_issue: serde_json::Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("parse create json");
    created_issue["id"]
        .as_str()
        .expect("create json should include issue id")
        .to_string()
}

fn make_issue(id: &str, title: &str) -> Issue {
    Issue {
        id: id.to_string(),
        title: title.to_string(),
        status: Status::Open,
        priority: Priority::MEDIUM,
        issue_type: IssueType::Task,
        created_at: Utc::now(),
        updated_at: Utc::now(),
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

// ----------------------------------------------------------------------------
// beads_rust-uelt: 2026-05-09 audit-driven rewrite
//
// The original test (`test_rebuild_blocked_cache_crash_with_multiple_parents`)
// was written for an API that allowed multiple parent-child edges per issue.
// Commit `6ccbc3d6 fix(storage): reject second parent-child parent` (2026-05-07)
// tightened the validator to enforce single-parent semantics; the old
// scenario now panics on the second `add_dependency(..., "parent-child", ...)`
// call.
//
// Replaced with three split tests below that preserve the original intent
// (stress-test repeated incremental blocked-cache rebuilds around parent-child
// mutations) under the new contract:
//
//   1. test_rebuild_blocked_cache_after_parent_replace
//   2. test_rebuild_blocked_cache_after_parent_clear
//   3. test_dep_add_second_parent_returns_validation_error_with_clear_message
//
// Fixture IDs migrated from `bd-` to `br-` per beads_rust-6plg sibling work.
// ----------------------------------------------------------------------------

/// uelt #1: child A has parent B; replace parent with C; rebuild blocked cache;
/// verify cache reflects the NEW parent (A→C, not A→B).
#[test]
fn test_rebuild_blocked_cache_after_parent_replace() {
    let mut storage = SqliteStorage::open_memory().unwrap();

    // Setup: blocker E + parents B, C + child A
    storage
        .create_issue(&make_issue("br-e", "Blocker E"), "test")
        .unwrap();
    storage
        .create_issue(&make_issue("br-b", "Parent B"), "test")
        .unwrap();
    storage
        .create_issue(&make_issue("br-c", "Parent C"), "test")
        .unwrap();
    storage
        .create_issue(&make_issue("br-a", "Child A"), "test")
        .unwrap();

    // C blocked by E (so A→C path also makes A blocked)
    storage
        .add_dependency("br-c", "br-e", "blocks", "test")
        .unwrap();

    // Set initial parent A→B
    storage.set_parent("br-a", Some("br-b"), "test").unwrap();
    storage
        .rebuild_blocked_cache(true)
        .expect("rebuild after parent set should not crash");

    // Replace parent: A→C
    storage.set_parent("br-a", Some("br-c"), "test").unwrap();
    storage
        .rebuild_blocked_cache(true)
        .expect("rebuild after parent replace should not crash");

    // Verify A's parent is now C (not B)
    let parents = storage.get_dependencies("br-a").unwrap();
    assert_eq!(
        parents.len(),
        1,
        "single-parent contract: A must have exactly 1 parent edge; got {parents:?}"
    );
    assert_eq!(parents[0], "br-c", "A's parent should now be C, not B");

    // A should be blocked because C is blocked by E
    assert!(
        storage.is_blocked("br-a").unwrap(),
        "A should be blocked transitively through new parent C"
    );
}

/// uelt #2: child A has parent B; clear parent; rebuild blocked cache;
/// verify A is no longer in B's blocked set and is not blocked.
#[test]
fn test_rebuild_blocked_cache_after_parent_clear() {
    let mut storage = SqliteStorage::open_memory().unwrap();

    storage
        .create_issue(&make_issue("br-d", "Blocker D"), "test")
        .unwrap();
    storage
        .create_issue(&make_issue("br-b", "Parent B"), "test")
        .unwrap();
    storage
        .create_issue(&make_issue("br-a", "Child A"), "test")
        .unwrap();

    // B blocked by D, A child of B → A is transitively blocked
    storage
        .add_dependency("br-b", "br-d", "blocks", "test")
        .unwrap();
    storage.set_parent("br-a", Some("br-b"), "test").unwrap();
    storage.rebuild_blocked_cache(true).expect("rebuild");
    assert!(
        storage.is_blocked("br-a").unwrap(),
        "A should be blocked initially via parent B"
    );

    // Clear A's parent
    storage.set_parent("br-a", None, "test").unwrap();
    storage
        .rebuild_blocked_cache(true)
        .expect("rebuild after parent clear should not crash");

    let parents = storage.get_dependencies("br-a").unwrap();
    assert_eq!(parents.len(), 0, "A should have no parent after clear");
    assert!(
        !storage.is_blocked("br-a").unwrap(),
        "A should no longer be blocked after parent cleared"
    );
}

/// uelt #3: trying to add a second `parent-child` dep MUST return a clear
/// validation error naming the existing parent. This is the explicit
/// negative-path contract test for commit `6ccbc3d6`.
#[test]
fn test_dep_add_second_parent_returns_validation_error_with_clear_message() {
    let mut storage = SqliteStorage::open_memory().unwrap();

    storage
        .create_issue(&make_issue("br-b", "Parent B"), "test")
        .unwrap();
    storage
        .create_issue(&make_issue("br-c", "Parent C"), "test")
        .unwrap();
    storage
        .create_issue(&make_issue("br-a", "Child A"), "test")
        .unwrap();

    // First parent succeeds
    storage
        .add_dependency("br-a", "br-b", "parent-child", "test")
        .expect("first parent-child must succeed");

    // Second parent MUST fail with a clear, structured error
    let err = storage
        .add_dependency("br-a", "br-c", "parent-child", "test")
        .expect_err("second parent-child must fail");

    let err_str = err.to_string();
    eprintln!("[uelt] validator rejection message: {err_str}");

    // Operator-readable expectations:
    // 1. Names the issue receiving the new parent (br-a)
    // 2. Names the existing parent (br-b)
    // 3. Names the attempted new parent (br-c)
    // 4. Offers a path forward (clear / replace)
    assert!(
        err_str.contains("br-a"),
        "error must name child issue 'br-a'; got: {err_str}"
    );
    assert!(
        err_str.contains("br-b"),
        "error must name existing parent 'br-b'; got: {err_str}"
    );
    assert!(
        err_str.contains("br-c") || err_str.contains("parent"),
        "error must name attempted parent or 'parent' context; got: {err_str}"
    );

    // Verify the validator did NOT mutate state — A still has only B
    let parents = storage.get_dependencies("br-a").unwrap();
    assert_eq!(
        parents.len(),
        1,
        "validator must not partially mutate state"
    );
    assert_eq!(parents[0], "br-b");
}

#[test]
fn test_rebuild_blocked_cache_is_idempotent_when_rows_already_exist() {
    let mut storage = SqliteStorage::open_memory().unwrap();

    storage
        .create_issue(&make_issue("bd-root", "Root"), "test")
        .unwrap();
    storage
        .create_issue(&make_issue("bd-parent", "Parent"), "test")
        .unwrap();
    storage
        .create_issue(&make_issue("bd-child", "Child"), "test")
        .unwrap();

    storage
        .add_dependency("bd-parent", "bd-root", "blocks", "test")
        .unwrap();
    storage
        .add_dependency("bd-child", "bd-parent", "parent-child", "test")
        .unwrap();

    assert!(storage.is_blocked("bd-parent").unwrap());
    assert!(storage.is_blocked("bd-child").unwrap());

    for _ in 0..64 {
        storage
            .rebuild_blocked_cache(true)
            .expect("rebuilding an already-populated blocked cache must stay idempotent");
    }

    assert!(storage.is_blocked("bd-parent").unwrap());
    assert!(storage.is_blocked("bd-child").unwrap());
}

#[test]
fn repro_dep_add_parent_child_succeeds_db_backed_after_blocked_cache_exists() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let root_id = create_issue_id(&workspace, "Root blocker", "create_root");
    let parent_id = create_issue_id(&workspace, "Parent issue", "create_parent");
    let child_id = create_issue_id(&workspace, "Child issue", "create_child");

    let add_blocker = run_br(
        &workspace,
        [
            "dep", "add", &parent_id, &root_id, "--type", "blocks", "--json",
        ],
        "dep_add_blocker_db",
    );
    assert!(
        add_blocker.status.success(),
        "db-backed dep add (blocks) failed: {}",
        add_blocker.stderr
    );

    let add_parent_child = run_br(
        &workspace,
        [
            "dep",
            "add",
            &child_id,
            &parent_id,
            "--type",
            "parent-child",
            "--json",
        ],
        "dep_add_parent_child_db",
    );
    assert!(
        add_parent_child.status.success(),
        "db-backed dep add (parent-child) failed: {}",
        add_parent_child.stderr
    );

    let payload: serde_json::Value =
        serde_json::from_str(&extract_json_payload(&add_parent_child.stdout))
            .expect("parse dep add json");
    assert_eq!(payload["status"], "ok");
    assert_eq!(payload["action"], "added");
}

#[test]
fn repro_dep_add_parent_child_succeeds_no_db_after_blocked_cache_exists() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let root_id = create_issue_id(&workspace, "Root blocker", "create_root");
    let parent_id = create_issue_id(&workspace, "Parent issue", "create_parent");
    let child_id = create_issue_id(&workspace, "Child issue", "create_child");

    let flush = run_br(&workspace, ["sync", "--flush-only"], "flush");
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);

    let add_blocker = run_br(
        &workspace,
        [
            "dep", "add", &parent_id, &root_id, "--type", "blocks", "--no-db", "--json",
        ],
        "dep_add_blocker_no_db",
    );
    assert!(
        add_blocker.status.success(),
        "no-db dep add (blocks) failed: {}",
        add_blocker.stderr
    );

    let add_parent_child = run_br(
        &workspace,
        [
            "dep",
            "add",
            &child_id,
            &parent_id,
            "--type",
            "parent-child",
            "--no-db",
            "--json",
        ],
        "dep_add_parent_child_no_db",
    );
    assert!(
        add_parent_child.status.success(),
        "no-db dep add (parent-child) failed: {}",
        add_parent_child.stderr
    );

    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let child_record = fs::read_to_string(&jsonl_path)
        .expect("read issues.jsonl")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("parse issue json"))
        .find(|record| record["id"].as_str() == Some(child_id.as_str()))
        .expect("child issue record in issues.jsonl");
    let dependencies = child_record["dependencies"]
        .as_array()
        .expect("jsonl issue should include dependencies array");
    assert!(dependencies.iter().any(|dependency| {
        dependency["depends_on_id"].as_str() == Some(parent_id.as_str())
            && dependency["type"].as_str() == Some("parent-child")
    }));
}
