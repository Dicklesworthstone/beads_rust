//! E2E coverage for inherited governing context on `br show`
//! (beads_rust#297, beads_rust#351).
//!
//! beads_rust#351 regression: when several siblings beneath the same
//! epic are shown in one invocation, the shared ancestor block used to
//! be re-rendered once per sibling because each child's ancestor chain
//! resolves independently. The contract is now: each inherited source
//! is emitted exactly once per invocation, before the first child that
//! references it.

mod common;

use common::cli::{BrWorkspace, parse_created_id, run_br, run_br_with_env};

fn init_workspace() -> BrWorkspace {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    workspace
}

/// Build an epic carrying `agent_context` plus two children parented
/// beneath it. Returns `(epic_id, child_a, child_b)`.
fn epic_with_two_children(workspace: &BrWorkspace) -> (String, String, String) {
    let epic = run_br(
        workspace,
        ["create", "Auth rewrite epic", "--type", "epic"],
        "create_epic",
    );
    assert!(epic.status.success(), "create epic failed: {}", epic.stderr);
    let epic_id = parse_created_id(&epic.stdout);
    assert!(!epic_id.is_empty(), "missing epic id: {}", epic.stdout);

    let set_ctx = run_br(
        workspace,
        [
            "update",
            &epic_id,
            "--agent-context",
            r#"{"skills":["clean-code"],"constraints":["no-breaking-changes"]}"#,
        ],
        "set_agent_context",
    );
    assert!(
        set_ctx.status.success(),
        "set agent context failed: {}",
        set_ctx.stderr
    );

    let mut child_ids = Vec::new();
    for (title, label) in [
        ("Token refresh child", "create_child_a"),
        ("Session storage child", "create_child_b"),
    ] {
        let child = run_br(workspace, ["create", title, "--parent", &epic_id], label);
        assert!(
            child.status.success(),
            "create child failed: {}",
            child.stderr
        );
        let child_id = parse_created_id(&child.stdout);
        assert!(!child_id.is_empty(), "missing child id: {}", child.stdout);
        child_ids.push(child_id);
    }

    let child_b = child_ids.pop().expect("child b id");
    let child_a = child_ids.pop().expect("child a id");
    (epic_id, child_a, child_b)
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    haystack.matches(needle).count()
}

#[test]
fn e2e_show_two_siblings_emits_shared_inherited_context_once() {
    let _log = common::test_log("e2e_show_two_siblings_emits_shared_inherited_context_once");
    let workspace = init_workspace();
    let (epic_id, child_a, child_b) = epic_with_two_children(&workspace);

    let show = run_br_with_env(
        &workspace,
        ["show", &child_a, &child_b],
        [("BR_INHERITED_CONTEXT", "1")],
        "show_two_siblings",
    );
    assert!(show.status.success(), "show failed: {}", show.stderr);

    let header_marker = format!("--- Inherited context (from epic {epic_id}");
    assert_eq!(
        count_occurrences(&show.stdout, &header_marker),
        1,
        "shared inherited block must be emitted exactly once for sibling \
         children (beads_rust#351), got output:\n{}",
        show.stdout
    );
    assert_eq!(
        count_occurrences(&show.stdout, "clean-code"),
        1,
        "inherited content body must not repeat per sibling:\n{}",
        show.stdout
    );

    // The block must precede the first child referencing it, and both
    // children must still render their own details.
    let block_pos = show
        .stdout
        .find(&header_marker)
        .expect("inherited block present");
    let child_a_pos = show
        .stdout
        .find(&child_a)
        .expect("first child id present in output");
    assert!(
        block_pos < child_a_pos,
        "inherited block should precede the first child referencing it:\n{}",
        show.stdout
    );
    assert!(
        show.stdout.contains("Token refresh child"),
        "first sibling missing:\n{}",
        show.stdout
    );
    assert!(
        show.stdout.contains("Session storage child"),
        "second sibling missing:\n{}",
        show.stdout
    );
}

#[test]
fn e2e_show_single_child_still_emits_inherited_context() {
    let _log = common::test_log("e2e_show_single_child_still_emits_inherited_context");
    let workspace = init_workspace();
    let (epic_id, child_a, _child_b) = epic_with_two_children(&workspace);

    let show = run_br_with_env(
        &workspace,
        ["show", &child_a],
        [("BR_INHERITED_CONTEXT", "1")],
        "show_single_child",
    );
    assert!(show.status.success(), "show failed: {}", show.stderr);

    let header_marker = format!("--- Inherited context (from epic {epic_id}");
    assert_eq!(
        count_occurrences(&show.stdout, &header_marker),
        1,
        "single-child show keeps exactly one inherited block:\n{}",
        show.stdout
    );
}

#[test]
fn e2e_show_without_opt_in_emits_no_inherited_context() {
    let _log = common::test_log("e2e_show_without_opt_in_emits_no_inherited_context");
    let workspace = init_workspace();
    let (_epic_id, child_a, child_b) = epic_with_two_children(&workspace);

    let show = run_br(&workspace, ["show", &child_a, &child_b], "show_no_opt_in");
    assert!(show.status.success(), "show failed: {}", show.stderr);
    assert!(
        !show.stdout.contains("Inherited context"),
        "inherited context emission is opt-in (beads_rust#297):\n{}",
        show.stdout
    );
}
