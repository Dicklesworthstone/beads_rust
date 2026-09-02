//! `br agents` is the one command that edits a file outside `.beads/`
//! (AGENTS.md) and every install runs it, yet it had no end-to-end coverage.
//! These scenarios drive the real binary through check, add, idempotent
//! re-add, update from the legacy `bv` block, remove, dry-run, the JSON
//! `--force` requirement, and a declined confirmation, asserting the user's
//! own text survives every managed-block edit byte for byte.

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br, run_br_with_stdin};
use serde_json::Value;
use std::fs;

const START_MARKER: &str = "<!-- br-agent-instructions-v1 -->";
const END_MARKER: &str = "<!-- end-br-agent-instructions -->";
const LEGACY_START: &str = "<!-- bv-agent-instructions-v1 -->";
const LEGACY_END: &str = "<!-- end-bv-agent-instructions -->";
const USER_HEAD: &str = "# My project\n\nHand-written guidance that must survive.\n\n";
const USER_TAIL: &str = "\n## Appendix\n\nMore hand-written text after the block.\n";

fn agents_md(workspace: &BrWorkspace) -> String {
    fs::read_to_string(workspace.root.join("AGENTS.md")).expect("read AGENTS.md")
}

fn json(stdout: &str) -> Value {
    serde_json::from_str(&extract_json_payload(stdout)).expect("agents JSON")
}

fn managed_block_count(content: &str) -> usize {
    content.matches(START_MARKER).count()
}

#[test]
fn e2e_agents_add_is_idempotent_and_preserves_user_text() {
    let _log = common::test_log("e2e_agents_add_is_idempotent_and_preserves_user_text");
    let workspace = BrWorkspace::new();

    // Check mode on a directory without an agent file: no file is created.
    let check = run_br(&workspace, ["agents", "--json"], "agents_check_missing");
    assert!(check.status.success(), "check failed: {}", check.stderr);
    let check = json(&check.stdout);
    assert_eq!(check["found"], false, "check: {check}");
    assert_eq!(check["has_blurb"], false, "check: {check}");
    // `needs_blurb` describes an existing file lacking the block; a missing
    // file reports false and `would_action` only in dry-run mode.
    assert_eq!(check["needs_blurb"], false, "check: {check}");
    assert!(!workspace.root.join("AGENTS.md").exists());

    // Dry-run add writes nothing but previews the block.
    let dry = run_br(
        &workspace,
        ["agents", "--add", "--dry-run"],
        "agents_add_dry_run",
    );
    assert!(dry.status.success(), "dry-run failed: {}", dry.stderr);
    assert!(
        dry.stdout.contains(START_MARKER),
        "dry-run preview: {}",
        dry.stdout
    );
    assert!(
        !workspace.root.join("AGENTS.md").exists(),
        "dry-run must not create AGENTS.md"
    );

    // JSON mode refuses to mutate without --force.
    let json_no_force = run_br(
        &workspace,
        ["agents", "--add", "--json"],
        "agents_add_json_no_force",
    );
    assert!(
        !json_no_force.status.success(),
        "JSON add without --force must be refused: {}",
        json_no_force.stdout
    );
    assert!(!workspace.root.join("AGENTS.md").exists());

    // Add creates the file with exactly one managed block.
    let add = run_br(&workspace, ["agents", "--add", "--force"], "agents_add");
    assert!(add.status.success(), "add failed: {}", add.stderr);
    let created = agents_md(&workspace);
    assert_eq!(managed_block_count(&created), 1, "created: {created}");
    assert!(
        created.contains(END_MARKER) && created.contains("br ready"),
        "created: {created}"
    );

    // A second add is a byte-identical no-op.
    let again = run_br(
        &workspace,
        ["agents", "--add", "--force", "--json"],
        "agents_add_again",
    );
    assert!(again.status.success(), "re-add failed: {}", again.stderr);
    let again = json(&again.stdout);
    assert_eq!(again["reason"], "already_current", "re-add: {again}");
    assert_eq!(
        agents_md(&workspace),
        created,
        "second add must not change the file"
    );

    // Check now reports the current block.
    let check = run_br(&workspace, ["agents", "--json"], "agents_check_present");
    let check = json(&check.stdout);
    assert_eq!(check["found"], true, "check: {check}");
    assert_eq!(check["has_blurb"], true, "check: {check}");
    assert_eq!(check["needs_blurb"], false, "check: {check}");
    assert_eq!(check["needs_upgrade"], false, "check: {check}");
}

#[test]
fn e2e_agents_add_update_remove_keep_user_text_intact() {
    let _log = common::test_log("e2e_agents_add_update_remove_keep_user_text_intact");
    let workspace = BrWorkspace::new();
    let path = workspace.root.join("AGENTS.md");
    fs::write(&path, USER_HEAD).expect("seed AGENTS.md");

    // Declining the confirmation leaves the file untouched.
    let declined = run_br_with_stdin(
        &workspace,
        ["agents", "--add"],
        "n\n",
        "agents_add_declined",
    );
    assert!(
        declined.status.success(),
        "declined add: {}",
        declined.stderr
    );
    assert_eq!(
        agents_md(&workspace),
        USER_HEAD,
        "declined add must not write"
    );

    // Forced add appends the block after the user's text.
    let add = run_br(
        &workspace,
        ["agents", "--add", "--force"],
        "agents_add_to_existing",
    );
    assert!(add.status.success(), "add failed: {}", add.stderr);
    let with_block = agents_md(&workspace);
    assert!(
        with_block.starts_with(USER_HEAD),
        "user text must stay first: {with_block}"
    );
    assert_eq!(managed_block_count(&with_block), 1);

    // Text the user adds after the block survives an update...
    fs::write(&path, format!("{with_block}{USER_TAIL}")).expect("append user tail");
    let update = run_br(
        &workspace,
        ["agents", "--update", "--force", "--json"],
        "agents_update_current",
    );
    assert!(update.status.success(), "update failed: {}", update.stderr);
    let update = json(&update.stdout);
    assert_eq!(
        update["reason"], "already_up_to_date",
        "update on current block: {update}"
    );
    let after_update = agents_md(&workspace);
    assert!(after_update.starts_with(USER_HEAD) && after_update.ends_with(USER_TAIL));
    assert_eq!(managed_block_count(&after_update), 1);

    // ...and a remove strips only the managed block.
    let remove = run_br(
        &workspace,
        ["agents", "--remove", "--force"],
        "agents_remove",
    );
    assert!(remove.status.success(), "remove failed: {}", remove.stderr);
    let removed = agents_md(&workspace);
    assert!(
        !removed.contains(START_MARKER) && !removed.contains(END_MARKER),
        "removed: {removed}"
    );
    assert!(
        removed.contains("Hand-written guidance that must survive."),
        "removed: {removed}"
    );
    assert!(
        removed.contains("More hand-written text after the block."),
        "removed: {removed}"
    );

    // Removing again is a no-op that keeps the file.
    let remove_again = run_br(
        &workspace,
        ["agents", "--remove", "--force"],
        "agents_remove_again",
    );
    assert!(
        remove_again.status.success(),
        "second remove: {}",
        remove_again.stderr
    );
    assert_eq!(agents_md(&workspace), removed);
}

#[test]
fn e2e_agents_update_replaces_legacy_bv_block() {
    let _log = common::test_log("e2e_agents_update_replaces_legacy_bv_block");
    let workspace = BrWorkspace::new();
    let legacy =
        format!("{USER_HEAD}{LEGACY_START}\nOld bv instructions.\n{LEGACY_END}\n{USER_TAIL}");
    fs::write(workspace.root.join("AGENTS.md"), &legacy).expect("seed legacy AGENTS.md");

    let check = run_br(&workspace, ["agents", "--json"], "agents_check_legacy");
    let check = json(&check.stdout);
    assert_eq!(check["has_legacy_blurb"], true, "check: {check}");
    assert_eq!(check["needs_upgrade"], true, "check: {check}");

    let update = run_br(
        &workspace,
        ["agents", "--update", "--force"],
        "agents_update_legacy",
    );
    assert!(update.status.success(), "update failed: {}", update.stderr);
    let upgraded = agents_md(&workspace);
    assert!(
        !upgraded.contains(LEGACY_START),
        "legacy block must be gone: {upgraded}"
    );
    assert_eq!(managed_block_count(&upgraded), 1, "upgraded: {upgraded}");
    assert!(
        upgraded.starts_with(USER_HEAD),
        "user head must survive: {upgraded}"
    );
    assert!(
        upgraded.contains("More hand-written text after the block."),
        "user tail must survive: {upgraded}"
    );

    let check = run_br(&workspace, ["agents", "--json"], "agents_check_upgraded");
    let check = json(&check.stdout);
    assert_eq!(check["has_blurb"], true, "check: {check}");
    assert_eq!(check["needs_upgrade"], false, "check: {check}");
}
