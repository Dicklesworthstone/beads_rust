use super::common::cli::{BrWorkspace, run_br, run_br_close_with_lease};
use super::{create_issue, init_workspace, normalize_output};
use insta::assert_snapshot;

#[test]
fn snapshot_error_not_initialized() {
    let workspace = BrWorkspace::new();
    let output = run_br(&workspace, ["list"], "list_without_init");
    assert!(!output.status.success(), "expected failure");
    assert_snapshot!("error_not_initialized", normalize_output(&output.stderr));
}

#[test]
fn snapshot_error_issue_not_found() {
    let workspace = init_workspace();
    let output = run_br(&workspace, ["show", "bd-nonexistent"], "show_notfound");
    assert!(!output.status.success(), "expected failure");
    assert_snapshot!("error_issue_not_found", normalize_output(&output.stderr));
}

#[test]
fn snapshot_error_invalid_priority() {
    let workspace = init_workspace();
    let output = run_br(
        &workspace,
        ["create", "Test issue", "--priority", "99"],
        "create_bad_priority",
    );
    assert!(!output.status.success(), "expected failure");
    assert_snapshot!("error_invalid_priority", normalize_output(&output.stderr));
}

#[test]
fn snapshot_error_invalid_status() {
    let workspace = init_workspace();
    let id = create_issue(&workspace, "Test issue", "create_for_bad_status");
    let output = run_br(
        &workspace,
        ["update", &id, "--status", "invalid"],
        "update_bad_status",
    );
    assert!(!output.status.success(), "expected failure");
    assert_snapshot!("error_invalid_status", normalize_output(&output.stderr));
}

#[test]
fn snapshot_error_dependency_cycle() {
    let workspace = init_workspace();
    let id1 = create_issue(&workspace, "Issue A", "create_cycle_a");
    let id2 = create_issue(&workspace, "Issue B", "create_cycle_b");

    // Add A -> B
    let add1 = run_br(&workspace, ["dep", "add", &id1, &id2], "dep_add_cycle1");
    assert!(add1.status.success(), "first dep add should succeed");

    // Try to add B -> A (creates cycle)
    let output = run_br(&workspace, ["dep", "add", &id2, &id1], "dep_add_cycle2");
    assert!(!output.status.success(), "expected cycle detection failure");
    assert_snapshot!("error_dependency_cycle", normalize_output(&output.stderr));
}

#[test]
fn snapshot_error_self_dependency() {
    let workspace = init_workspace();
    let id = create_issue(&workspace, "Self-dep test", "create_self_dep");

    let output = run_br(&workspace, ["dep", "add", &id, &id], "dep_add_self");
    assert!(!output.status.success(), "expected self-dependency failure");
    assert_snapshot!("error_self_dependency", normalize_output(&output.stderr));
}

#[test]
fn snapshot_error_update_closed_issue() {
    let workspace = init_workspace();
    let id = create_issue(&workspace, "Will close", "create_for_close");

    // Close the issue
    let close = run_br_close_with_lease(&workspace, &id, &[], "close_issue");
    assert!(close.status.success(), "close should succeed");

    // Try to update it (should still work in br, but status is limited)
    let output = run_br(
        &workspace,
        ["update", &id, "--status", "in_progress"],
        "update_closed",
    );
    // Note: br allows updating closed issues (reopens them)
    // Just verify the command behavior
    assert_snapshot!(
        "update_closed_issue",
        normalize_output(&format!("{}{}", output.stdout, output.stderr))
    );
}
