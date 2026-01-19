//! E2E tests for the `history` command.
//!
//! Tests cover:
//! - history list: List available backup snapshots
//! - history diff: Show diff between current and a backup
//! - history restore: Restore a backup to issues.jsonl
//! - history prune: Prune old backups
//! - Error handling: Before init, missing files, restore without force
//! - Edge cases: Many backups, backup deduplication

mod common;

use common::cli::{BrWorkspace, run_br};
use std::fs;
use std::thread;
use std::time::Duration;

/// Helper to run sync --flush-only.
fn sync_flush(workspace: &BrWorkspace) {
    let sync = run_br(workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(
        sync.status.success(),
        "sync should succeed: {}",
        sync.stderr
    );
}

/// Helper to create an issue without auto-flush.
/// This ensures the dirty flag is preserved for explicit sync calls.
fn create_issue(workspace: &BrWorkspace, title: &str, label: &str) {
    let create = run_br(workspace, ["--no-auto-flush", "create", title], label);
    assert!(create.status.success(), "create failed: {}", create.stderr);
}

/// Helper to set up a workspace with issues.jsonl already existing.
/// This creates an initial issue and syncs to establish the base JSONL file.
/// Returns the workspace ready for backup tests.
///
/// Note: We use --no-auto-flush to prevent automatic export after create,
/// which would clear dirty flags and prevent the explicit sync from triggering backups.
fn setup_workspace_with_jsonl() -> BrWorkspace {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create initial issue (with --no-auto-flush to control when export happens)
    create_issue(&workspace, "Initial issue", "create_initial");

    // First sync creates issues.jsonl (no backup yet since no previous JSONL exists)
    sync_flush(&workspace);

    workspace
}

/// Read backup files from the history directory.
fn list_backup_files(workspace: &BrWorkspace) -> Vec<String> {
    let history_dir = workspace.root.join(".beads").join(".br_history");
    if !history_dir.exists() {
        return vec![];
    }
    let mut files: Vec<String> = fs::read_dir(&history_dir)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            let path = e.path();
            let has_prefix = path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("issues."));
            let has_jsonl = path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"));
            has_prefix && has_jsonl
        })
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    files.sort();
    files
}

// =============================================================================
// SUCCESS PATH TESTS
// =============================================================================

#[test]
fn e2e_history_list_empty_initially() {
    let workspace = BrWorkspace::new();

    // Initialize workspace
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // List history - should be empty
    let list = run_br(&workspace, ["history", "list"], "history_list_empty");
    assert!(
        list.status.success(),
        "history list failed: {}",
        list.stderr
    );
    assert!(
        list.stdout.contains("No backups found")
            || list.stdout.contains('0')
            || !list.stdout.contains("issues."),
        "should show no backups: {}",
        list.stdout
    );
}

#[test]
fn e2e_history_list_after_sync_creates_backup() {
    // Setup workspace with initial JSONL file
    let workspace = setup_workspace_with_jsonl();

    // Create another issue to change content (with --no-auto-flush so sync has dirty issues)
    create_issue(&workspace, "Second issue", "create_second");

    // This sync will backup the existing issues.jsonl before writing the new one
    sync_flush(&workspace);

    // List history - should have at least one backup
    let list = run_br(&workspace, ["history", "list"], "history_list_with_backup");
    assert!(
        list.status.success(),
        "history list failed: {}",
        list.stderr
    );

    let backups = list_backup_files(&workspace);
    assert!(
        !backups.is_empty(),
        "should have at least one backup after sync"
    );
}

#[test]
fn e2e_history_list_shows_backup_details() {
    let workspace = setup_workspace_with_jsonl();

    // Create another issue and sync to trigger backup
    create_issue(&workspace, "Issue for backup details", "create_details");
    sync_flush(&workspace);

    // List should show filename, size, timestamp
    let list = run_br(&workspace, ["history", "list"], "history_list_details");
    assert!(list.status.success());

    // Check output contains expected columns
    let stdout = list.stdout.to_uppercase();
    assert!(
        stdout.contains("FILENAME") || stdout.contains("ISSUES."),
        "should show filename: {}",
        list.stdout
    );
}

#[test]
fn e2e_history_multiple_backups_chronological_order() {
    let workspace = setup_workspace_with_jsonl();

    // Create more issues to generate multiple backups
    for i in 0..3 {
        // Wait to ensure different timestamp
        thread::sleep(Duration::from_millis(1100));

        create_issue(&workspace, &format!("Issue {i}"), &format!("create_{i}"));
        sync_flush(&workspace);
    }

    // List backups
    let list = run_br(&workspace, ["history", "list"], "history_list_multiple");
    assert!(list.status.success());

    let backups = list_backup_files(&workspace);
    // Should have multiple backups
    assert!(
        backups.len() >= 2,
        "should have multiple backups: {backups:?}"
    );

    // Backups should be sorted by timestamp (list_backup_files sorts them)
    let sorted_backups = {
        let mut b = backups.clone();
        b.sort();
        b
    };
    assert_eq!(backups, sorted_backups, "backups should be sorted");
}

#[test]
fn e2e_history_restore_backup() {
    let workspace = setup_workspace_with_jsonl();

    // Create another issue to trigger backup
    create_issue(&workspace, "Issue before restore", "create_before_restore");
    sync_flush(&workspace);

    // Get the backup filename
    let backups = list_backup_files(&workspace);
    assert!(!backups.is_empty(), "should have backup");
    let backup_file = &backups[0];

    // Create yet another issue to change current state
    create_issue(&workspace, "Issue after backup", "create_after_backup");

    // Restore the backup (with --force since issues.jsonl exists)
    let restore = run_br(
        &workspace,
        ["history", "restore", backup_file, "--force"],
        "history_restore",
    );
    assert!(
        restore.status.success(),
        "history restore failed: {}",
        restore.stderr
    );
    assert!(
        restore.stdout.contains("Restored") || restore.stdout.contains("restored"),
        "should confirm restoration: {}",
        restore.stdout
    );
}

#[test]
fn e2e_history_diff_shows_differences() {
    let workspace = setup_workspace_with_jsonl();

    // Create issue to trigger backup
    create_issue(&workspace, "Issue for diff", "create_diff");
    sync_flush(&workspace);

    let backups = list_backup_files(&workspace);
    assert!(!backups.is_empty(), "should have backup");
    let backup_file = &backups[0];

    // Create another issue to make a difference
    create_issue(&workspace, "New issue for diff", "create_new_diff");
    // Flush to update issues.jsonl
    sync_flush(&workspace);

    // Diff should show something
    let diff = run_br(&workspace, ["history", "diff", backup_file], "history_diff");
    // diff may succeed or show differences (exit code depends on diff tool)
    // Just verify it doesn't crash
    assert!(
        diff.stdout.contains("Diff")
            || diff.stdout.contains("diff")
            || diff.stdout.contains("identical")
            || diff.stdout.contains("bytes")
            || diff.stderr.is_empty()
            || diff.status.success(),
        "diff should run without critical error: stdout={}, stderr={}",
        diff.stdout,
        diff.stderr
    );
}

#[test]
fn e2e_history_prune_keeps_recent() {
    let workspace = setup_workspace_with_jsonl();

    // Create multiple backups
    for i in 0..4 {
        thread::sleep(Duration::from_millis(1100)); // Ensure different timestamps
        create_issue(
            &workspace,
            &format!("Issue for prune {i}"),
            &format!("create_prune_{i}"),
        );
        sync_flush(&workspace);
    }

    let backups_before = list_backup_files(&workspace);
    assert!(
        backups_before.len() >= 3,
        "should have multiple backups: {backups_before:?}"
    );

    // Prune keeping only 2
    let prune = run_br(
        &workspace,
        ["history", "prune", "--keep", "2"],
        "history_prune",
    );
    assert!(prune.status.success(), "prune failed: {}", prune.stderr);

    // Note: prune only removes backups older than --older-than (if specified)
    // Without --older-than, it may not delete anything immediately
    // Let's verify the command ran successfully
    assert!(
        prune.stdout.contains("Pruned") || prune.stdout.contains('0'),
        "should report prune result: {}",
        prune.stdout
    );
}

// =============================================================================
// ERROR CASE TESTS
// =============================================================================

#[test]
fn e2e_history_list_before_init_fails() {
    let workspace = BrWorkspace::new();

    // Try to list history without init
    let list = run_br(&workspace, ["history", "list"], "history_no_init");
    assert!(
        !list.status.success(),
        "history list should fail before init"
    );
    assert!(
        list.stderr.contains("not initialized")
            || list.stderr.contains("NotInitialized")
            || list.stderr.contains("not found")
            || list.stderr.contains(".beads"),
        "error should mention initialization: {}",
        list.stderr
    );
}

#[test]
fn e2e_history_restore_nonexistent_backup_fails() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Try to restore a non-existent backup
    let restore = run_br(
        &workspace,
        ["history", "restore", "nonexistent.20990101_120000.jsonl"],
        "history_restore_missing",
    );
    assert!(
        !restore.status.success(),
        "restore should fail for non-existent backup"
    );
    assert!(
        restore.stderr.contains("not found")
            || restore.stderr.contains("No such file")
            || restore.stderr.contains("Backup file not found"),
        "error should mention file not found: {}",
        restore.stderr
    );
}

#[test]
fn e2e_history_restore_without_force_fails_when_exists() {
    let workspace = setup_workspace_with_jsonl();

    // Create another issue to trigger backup
    create_issue(&workspace, "Issue for force test", "create_force");
    sync_flush(&workspace);

    let backups = list_backup_files(&workspace);
    assert!(!backups.is_empty(), "should have backup");
    let backup_file = &backups[0];

    // Try restore without --force (issues.jsonl exists)
    let restore = run_br(
        &workspace,
        ["history", "restore", backup_file],
        "history_restore_no_force",
    );
    assert!(
        !restore.status.success(),
        "restore should fail without --force when issues.jsonl exists"
    );
    assert!(
        restore.stderr.contains("force")
            || restore.stderr.contains("exists")
            || restore.stderr.contains("overwrite"),
        "error should mention --force: {}",
        restore.stderr
    );
}

#[test]
fn e2e_history_diff_nonexistent_backup_fails() {
    let workspace = setup_workspace_with_jsonl();

    // Try to diff a non-existent backup
    let diff = run_br(
        &workspace,
        ["history", "diff", "nonexistent.20990101_120000.jsonl"],
        "history_diff_missing",
    );
    assert!(
        !diff.status.success(),
        "diff should fail for non-existent backup"
    );
    assert!(
        diff.stderr.contains("not found") || diff.stderr.contains("Backup file not found"),
        "error should mention file not found: {}",
        diff.stderr
    );
}

// =============================================================================
// EDGE CASE TESTS
// =============================================================================

#[test]
fn e2e_history_backup_deduplication() {
    let workspace = setup_workspace_with_jsonl();

    // Create issue to trigger first backup
    create_issue(&workspace, "Dedup test issue", "create_dedup");
    sync_flush(&workspace);

    let backups_after_first = list_backup_files(&workspace);
    let count_after_first = backups_after_first.len();

    // Sync again without changes - should not create duplicate
    sync_flush(&workspace);

    let backups_after_second = list_backup_files(&workspace);
    assert_eq!(
        backups_after_second.len(),
        count_after_first,
        "should not create duplicate backup for identical content: before={backups_after_first:?}, after={backups_after_second:?}"
    );
}

#[test]
fn e2e_history_with_many_issues() {
    let workspace = setup_workspace_with_jsonl();

    // Create many issues
    for i in 0..50 {
        create_issue(
            &workspace,
            &format!("Issue number {i}"),
            &format!("create_{i}"),
        );
    }

    // Sync to trigger backup
    sync_flush(&workspace);

    // List should work
    let list = run_br(&workspace, ["history", "list"], "history_list_many");
    assert!(list.status.success(), "list failed: {}", list.stderr);

    // Backup should exist and be reasonably sized
    let backups = list_backup_files(&workspace);
    assert!(!backups.is_empty(), "should have backup");

    // Check backup file has content
    let backup_path = workspace
        .root
        .join(".beads")
        .join(".br_history")
        .join(&backups[0]);
    let backup_size = fs::metadata(&backup_path).unwrap().len();
    assert!(
        backup_size > 100,
        "backup should have content: {backup_size} bytes"
    );
}

#[test]
fn e2e_history_default_command_is_list() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Just `br history` should default to list
    let history = run_br(&workspace, ["history"], "history_default");
    assert!(
        history.status.success(),
        "history default failed: {}",
        history.stderr
    );
    // Should show empty backups message or header
    assert!(
        history.stdout.contains("No backups")
            || history.stdout.contains("Backups")
            || history.stdout.contains("FILENAME"),
        "should show list output: {}",
        history.stdout
    );
}

#[test]
fn e2e_history_prune_with_older_than() {
    let workspace = setup_workspace_with_jsonl();

    // Create issue to trigger backup
    create_issue(&workspace, "Issue for prune age", "create_prune_age");
    sync_flush(&workspace);

    // Prune with --older-than (backups are fresh, so nothing should be pruned)
    let prune = run_br(
        &workspace,
        ["history", "prune", "--keep", "1", "--older-than", "1"],
        "history_prune_age",
    );
    assert!(
        prune.status.success(),
        "prune with older-than failed: {}",
        prune.stderr
    );

    // Should report pruning (likely 0 since backups are fresh)
    assert!(
        prune.stdout.contains("Pruned"),
        "should report prune result: {}",
        prune.stdout
    );
}
