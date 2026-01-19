mod common;

use common::cli::{BrWorkspace, run_br, run_br_with_env};

/// Helper to create an issue without auto-flush.
fn create_issue(workspace: &BrWorkspace, title: &str, label: &str) {
    let create = run_br(workspace, ["--no-auto-flush", "create", title], label);
    assert!(create.status.success(), "create failed: {}", create.stderr);
}

#[test]
fn e2e_history_custom_path() {
    let _log = common::test_log("e2e_history_custom_path");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create an issue so we have something to sync
    create_issue(&workspace, "Issue 1", "create1");

    // 1. Initial sync to create the file
    let sync1 = run_br(&workspace, ["sync", "--flush-only"], "sync1");
    assert!(sync1.status.success());

    // 2. Create another issue to trigger a change
    create_issue(&workspace, "Issue 2", "create2");

    // 3. Sync with CUSTOM path via ENV VAR (First time)
    // This creates .beads/custom.jsonl. No backup yet because it didn't exist.
    let sync2 = run_br_with_env(
        &workspace,
        ["sync", "--flush-only", "--allow-external-jsonl"],
        vec![("BEADS_JSONL", ".beads/custom.jsonl")],
        "sync2",
    );
    assert!(sync2.status.success(), "sync2 failed: {}", sync2.stderr);

    // 4. Create another issue to trigger change
    create_issue(&workspace, "Issue 3", "create3");

    // 5. Sync with CUSTOM path again (Second time)
    // This overwrites .beads/custom.jsonl. Backup SHOULD be created now.
    let sync3 = run_br_with_env(
        &workspace,
        ["sync", "--flush-only", "--allow-external-jsonl"],
        vec![("BEADS_JSONL", ".beads/custom.jsonl")],
        "sync3",
    );
    assert!(sync3.status.success(), "sync3 failed: {}", sync3.stderr);

    // 6. Check history
    let list = run_br(&workspace, ["history", "list"], "history_list");

    // NEW BEHAVIOR (FIXED): Backup found for custom file
    assert!(
        list.stdout.contains("custom."),
        "Failure: Backup NOT created for custom file. Output:\n{}",
        list.stdout
    );
}
