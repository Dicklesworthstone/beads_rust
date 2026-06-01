mod common;

use common::cli::{BrWorkspace, parse_created_id, run_br};
use std::fs;

fn assert_success(run: &common::cli::BrRun, label: &str) {
    assert!(
        run.status.success(),
        "{label} failed\nstdout={}\nstderr={}",
        run.stdout,
        run.stderr
    );
}

#[test]
fn sync_import_rebuild_discards_successful_recovery_backups() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert_success(&init, "init");

    let create = run_br(&workspace, ["create", "Rebuild cleanup anchor"], "create");
    assert_success(&create, "create");
    let issue_id = parse_created_id(&create.stdout);

    let flush = run_br(&workspace, ["sync", "--flush-only"], "flush");
    assert_success(&flush, "flush");

    let rebuild = run_br(
        &workspace,
        ["sync", "--import-only", "--rebuild"],
        "import_rebuild",
    );
    assert_success(&rebuild, "import_rebuild");

    let list = run_br(&workspace, ["show", &issue_id], "show_after_rebuild");
    assert_success(&list, "show_after_rebuild");

    let recovery_dir = workspace.root.join(".beads").join(".br_recovery");
    let recovery_entries = if recovery_dir.exists() {
        fs::read_dir(&recovery_dir)
            .unwrap_or_else(|err| {
                panic!(
                    "failed to read recovery dir {}: {err}",
                    recovery_dir.display()
                )
            })
            .count()
    } else {
        0
    };
    assert_eq!(
        recovery_entries,
        0,
        "successful sync --import-only --rebuild must not leave recovery artifacts in {}",
        recovery_dir.display()
    );
}
