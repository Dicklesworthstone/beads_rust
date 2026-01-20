//! E2E tests for history restore and prune commands with comprehensive coverage.
//!
//! These tests cover:
//! - Restore with real datasets from the dataset registry
//! - Backup integrity verification before/after operations
//! - Error handling for corrupt backups
//! - Prune with various retention policies
//! - Full artifact logging for debugging
//!
//! Acceptance criteria from beads_rust-2y18:
//! - Both commands have E2E test coverage
//! - Tests verify backup integrity before/after operations
//! - Error handling tested (corrupt backup, missing files)
//! - Full artifacts logged for debugging

// Allow naive bytecount in tests - performance is not critical here
#![allow(clippy::naive_bytecount)]

mod common;

use common::cli::{BrWorkspace, run_br};
use common::dataset_registry::{DatasetRegistry, IsolatedDataset, KnownDataset};
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
fn create_issue(workspace: &BrWorkspace, title: &str, label: &str) {
    let create = run_br(workspace, ["--no-auto-flush", "create", title], label);
    assert!(create.status.success(), "create failed: {}", create.stderr);
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

/// Helper to set up a workspace with issues.jsonl already existing.
fn setup_workspace_with_jsonl() -> BrWorkspace {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    create_issue(&workspace, "Initial issue", "create_initial");
    sync_flush(&workspace);

    workspace
}

/// Read file content as bytes for comparison.
fn read_file_bytes(workspace: &BrWorkspace, relative_path: &str) -> Vec<u8> {
    let path = workspace.root.join(relative_path);
    fs::read(&path).unwrap_or_default()
}

/// Check if beads_rust dataset is available for testing.
fn is_dataset_available() -> bool {
    DatasetRegistry::new().is_available(KnownDataset::BeadsRust)
}

// =============================================================================
// RESTORE TESTS WITH INTEGRITY VERIFICATION
// =============================================================================

#[test]
fn e2e_history_restore_verifies_content_integrity() {
    let _log = common::test_log("e2e_history_restore_verifies_content_integrity");
    let workspace = setup_workspace_with_jsonl();

    // Create issue to trigger backup
    create_issue(&workspace, "Issue before backup", "create_before");
    sync_flush(&workspace);

    // Wait to ensure backup timestamp differs
    thread::sleep(Duration::from_millis(1100));

    // Create more issues to change current state and trigger another backup
    create_issue(&workspace, "Issue after backup 1", "create_after_1");
    create_issue(&workspace, "Issue after backup 2", "create_after_2");
    sync_flush(&workspace);

    // Get the latest backup (which contains state before the last sync)
    let backups = list_backup_files(&workspace);
    assert!(
        backups.len() >= 2,
        "should have at least 2 backups: {backups:?}"
    );

    // Get the LATEST backup (last in sorted list) - this contains Initial + Before
    let backup_file = backups.last().unwrap();

    let backup_path = workspace
        .root
        .join(".beads")
        .join(".br_history")
        .join(backup_file);
    let original_backup_content = fs::read(&backup_path).expect("read backup");

    // Verify issues.jsonl has MORE lines than the backup (4 vs 2)
    let current_content = read_file_bytes(&workspace, ".beads/issues.jsonl");
    let current_lines = current_content.iter().filter(|&&b| b == b'\n').count();
    let backup_lines = original_backup_content
        .iter()
        .filter(|&&b| b == b'\n')
        .count();
    assert!(
        current_lines > backup_lines,
        "current should have more lines: current={current_lines}, backup={backup_lines}"
    );

    // Restore the backup
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

    // Verify restored content matches original backup exactly
    let restored_content = read_file_bytes(&workspace, ".beads/issues.jsonl");
    assert_eq!(
        restored_content, original_backup_content,
        "restored content should match original backup exactly"
    );
}

#[test]
fn e2e_history_restore_preserves_backup_file() {
    let _log = common::test_log("e2e_history_restore_preserves_backup_file");
    let workspace = setup_workspace_with_jsonl();

    // Create issue to trigger backup
    create_issue(&workspace, "Issue for preserve test", "create_preserve");
    sync_flush(&workspace);

    let backups = list_backup_files(&workspace);
    assert!(!backups.is_empty(), "should have backup");
    let backup_file = &backups[0];

    let backup_path = workspace
        .root
        .join(".beads")
        .join(".br_history")
        .join(backup_file);
    let original_backup_content = fs::read(&backup_path).expect("read backup");

    // Restore the backup
    let restore = run_br(
        &workspace,
        ["history", "restore", backup_file, "--force"],
        "history_restore",
    );
    assert!(restore.status.success());

    // Verify backup file still exists and is unchanged
    assert!(backup_path.exists(), "backup file should still exist");
    let backup_after = fs::read(&backup_path).expect("read backup after restore");
    assert_eq!(
        original_backup_content, backup_after,
        "backup file should be unchanged after restore"
    );
}

#[test]
fn e2e_history_restore_json_output() {
    let _log = common::test_log("e2e_history_restore_json_output");
    let workspace = setup_workspace_with_jsonl();

    // Create issue to trigger backup
    create_issue(&workspace, "Issue for JSON test", "create_json");
    sync_flush(&workspace);

    let backups = list_backup_files(&workspace);
    assert!(!backups.is_empty(), "should have backup");
    let backup_file = &backups[0];

    // Restore with JSON output
    let restore = run_br(
        &workspace,
        ["--json", "history", "restore", backup_file, "--force"],
        "history_restore_json",
    );
    assert!(restore.status.success());

    // Parse JSON output
    let json: serde_json::Value = serde_json::from_str(&restore.stdout).expect("should parse JSON");

    assert_eq!(json["action"], "restore");
    assert_eq!(json["backup"], backup_file.as_str());
    assert_eq!(json["restored"], true);
    assert!(json["target"].as_str().is_some());
    assert!(json["next_step"].as_str().is_some());
}

// =============================================================================
// CORRUPT BACKUP HANDLING
// =============================================================================

#[test]
fn e2e_history_restore_corrupt_backup_succeeds_copy() {
    let _log = common::test_log("e2e_history_restore_corrupt_backup_succeeds_copy");
    let workspace = setup_workspace_with_jsonl();

    // Create issue to trigger backup
    create_issue(&workspace, "Issue for corrupt test", "create_corrupt");
    sync_flush(&workspace);

    let backups = list_backup_files(&workspace);
    assert!(!backups.is_empty(), "should have backup");
    let backup_file = &backups[0];

    // Corrupt the backup file with invalid JSONL
    let backup_path = workspace
        .root
        .join(".beads")
        .join(".br_history")
        .join(backup_file);
    fs::write(&backup_path, "{ this is not valid json }\n{ also broken }")
        .expect("write corrupt backup");

    // Restore should succeed (it just copies the file)
    // The corruption would be detected on import, not restore
    let restore = run_br(
        &workspace,
        ["history", "restore", backup_file, "--force"],
        "history_restore_corrupt",
    );
    assert!(
        restore.status.success(),
        "restore should succeed (it's just a copy): {}",
        restore.stderr
    );

    // The restored issues.jsonl should contain the corrupt content
    let restored_content = fs::read_to_string(workspace.root.join(".beads").join("issues.jsonl"))
        .expect("read restored");
    assert!(
        restored_content.contains("this is not valid json"),
        "restored file should contain the corrupt content"
    );
}

#[test]
fn e2e_history_restore_empty_backup() {
    let _log = common::test_log("e2e_history_restore_empty_backup");
    let workspace = setup_workspace_with_jsonl();

    // Create issue to trigger backup
    create_issue(&workspace, "Issue for empty test", "create_empty");
    sync_flush(&workspace);

    let backups = list_backup_files(&workspace);
    assert!(!backups.is_empty(), "should have backup");
    let backup_file = &backups[0];

    // Make the backup empty
    let backup_path = workspace
        .root
        .join(".beads")
        .join(".br_history")
        .join(backup_file);
    fs::write(&backup_path, "").expect("write empty backup");

    // Restore should succeed
    let restore = run_br(
        &workspace,
        ["history", "restore", backup_file, "--force"],
        "history_restore_empty",
    );
    assert!(
        restore.status.success(),
        "restore should succeed with empty file: {}",
        restore.stderr
    );

    // Verify the restored file is empty
    let restored = fs::read_to_string(workspace.root.join(".beads").join("issues.jsonl"))
        .expect("read restored");
    assert!(restored.is_empty(), "restored file should be empty");
}

// =============================================================================
// PRUNE TESTS WITH COMPREHENSIVE COVERAGE
// =============================================================================

#[test]
fn e2e_history_prune_deletes_excess_backups() {
    let _log = common::test_log("e2e_history_prune_deletes_excess_backups");
    let workspace = setup_workspace_with_jsonl();

    // Create 5 backups with different timestamps
    for i in 0..5 {
        thread::sleep(Duration::from_millis(1100)); // Ensure different timestamps
        create_issue(
            &workspace,
            &format!("Issue for prune excess {i}"),
            &format!("create_prune_{i}"),
        );
        sync_flush(&workspace);
    }

    let backups_before = list_backup_files(&workspace);
    assert!(
        backups_before.len() >= 4,
        "should have at least 4 backups: {backups_before:?}"
    );

    // Prune keeping only 2
    let prune = run_br(
        &workspace,
        ["history", "prune", "--keep", "2"],
        "history_prune",
    );
    assert!(prune.status.success(), "prune failed: {}", prune.stderr);

    // Verify we now have at most 2 backups
    let backups_after = list_backup_files(&workspace);
    assert!(
        backups_after.len() <= 2,
        "should have at most 2 backups after prune: {backups_after:?}"
    );
}

#[test]
fn e2e_history_prune_keeps_newest() {
    let _log = common::test_log("e2e_history_prune_keeps_newest");
    let workspace = setup_workspace_with_jsonl();

    // Create 4 backups with different timestamps
    let mut backup_timestamps: Vec<String> = Vec::new();
    for i in 0..4 {
        thread::sleep(Duration::from_millis(1100));
        create_issue(
            &workspace,
            &format!("Issue for keep newest {i}"),
            &format!("create_newest_{i}"),
        );
        sync_flush(&workspace);

        // Record the latest backup
        let backups = list_backup_files(&workspace);
        if let Some(latest) = backups.last() {
            backup_timestamps.push(latest.clone());
        }
    }

    // Get the two newest backup names
    let newest_two: Vec<String> = backup_timestamps.iter().rev().take(2).cloned().collect();

    // Prune keeping only 2
    let prune = run_br(
        &workspace,
        ["history", "prune", "--keep", "2"],
        "history_prune",
    );
    assert!(prune.status.success());

    // Verify the newest backups are kept
    let backups_after = list_backup_files(&workspace);
    for newest in &newest_two {
        assert!(
            backups_after.contains(newest),
            "newest backup {newest} should be kept: {backups_after:?}"
        );
    }
}

#[test]
fn e2e_history_prune_json_output() {
    let _log = common::test_log("e2e_history_prune_json_output");
    let workspace = setup_workspace_with_jsonl();

    // Create backups
    for i in 0..3 {
        thread::sleep(Duration::from_millis(1100));
        create_issue(
            &workspace,
            &format!("Issue for prune JSON {i}"),
            &format!("create_json_{i}"),
        );
        sync_flush(&workspace);
    }

    // Prune with JSON output
    let prune = run_br(
        &workspace,
        ["--json", "history", "prune", "--keep", "1"],
        "history_prune_json",
    );
    assert!(prune.status.success());

    // Parse JSON output
    let json: serde_json::Value = serde_json::from_str(&prune.stdout).expect("should parse JSON");

    assert_eq!(json["action"], "prune");
    assert!(json["deleted"].is_number());
    assert_eq!(json["keep"], 1);
}

#[test]
fn e2e_history_prune_with_older_than_days() {
    let _log = common::test_log("e2e_history_prune_with_older_than_days");
    let workspace = setup_workspace_with_jsonl();

    // Create a backup
    create_issue(&workspace, "Issue for age prune", "create_age");
    sync_flush(&workspace);

    // Prune with --older-than 0 (should delete all backups)
    let prune = run_br(
        &workspace,
        ["history", "prune", "--keep", "100", "--older-than", "0"],
        "history_prune_age",
    );
    assert!(prune.status.success(), "prune failed: {}", prune.stderr);

    // All backups should be deleted (they're all older than 0 days, i.e., created before now)
    let backups = list_backup_files(&workspace);
    assert!(
        backups.is_empty(),
        "all backups should be deleted with --older-than 0: {backups:?}"
    );
}

#[test]
fn e2e_history_prune_no_backups_to_delete() {
    let _log = common::test_log("e2e_history_prune_no_backups_to_delete");
    let workspace = setup_workspace_with_jsonl();

    // Create exactly 2 backups
    for i in 0..2 {
        thread::sleep(Duration::from_millis(1100));
        create_issue(
            &workspace,
            &format!("Issue for no delete {i}"),
            &format!("create_nodelete_{i}"),
        );
        sync_flush(&workspace);
    }

    let backups_before = list_backup_files(&workspace);

    // Prune keeping 10 (more than we have)
    let prune = run_br(
        &workspace,
        ["history", "prune", "--keep", "10"],
        "history_prune_nodelete",
    );
    assert!(prune.status.success());
    assert!(
        prune.stdout.contains("Pruned 0") || prune.stdout.contains("0 backup"),
        "should report 0 deleted: {}",
        prune.stdout
    );

    // Backups should be unchanged
    let backups_after = list_backup_files(&workspace);
    assert_eq!(backups_before, backups_after, "backups should be unchanged");
}

// =============================================================================
// TESTS WITH REAL DATASETS
// =============================================================================

#[test]
fn e2e_history_restore_with_real_dataset() {
    let _log = common::test_log("e2e_history_restore_with_real_dataset");

    if !is_dataset_available() {
        eprintln!(
            "Skipping e2e_history_restore_with_real_dataset: beads_rust dataset not available"
        );
        return;
    }

    // Create isolated workspace from real dataset
    let isolated = IsolatedDataset::from_dataset(KnownDataset::BeadsRust)
        .expect("should copy beads_rust dataset");

    // Write test summary for debugging
    let _summary_path = isolated.write_summary().expect("write summary");

    // Create a wrapper workspace for our test helpers
    let workspace = BrWorkspace {
        temp_dir: isolated.temp_dir,
        root: isolated.root.clone(),
        log_dir: isolated.root.join("logs"),
    };
    fs::create_dir_all(&workspace.log_dir).ok();

    // Ensure history directory exists
    let history_dir = workspace.root.join(".beads").join(".br_history");
    fs::create_dir_all(&history_dir).expect("create history dir");

    // Create a backup by modifying and syncing
    create_issue(&workspace, "Test issue for real dataset", "create_real");
    sync_flush(&workspace);

    // Get the backup
    let backups = list_backup_files(&workspace);
    if backups.is_empty() {
        // Create another change to trigger backup
        create_issue(
            &workspace,
            "Another test issue for real dataset",
            "create_real_2",
        );
        sync_flush(&workspace);
    }

    let backups = list_backup_files(&workspace);
    if backups.is_empty() {
        eprintln!("No backups created - dataset may have been empty or sync didn't trigger backup");
        return;
    }

    let backup_file = &backups[0];

    // Capture backup content
    let backup_path = history_dir.join(backup_file);
    let original_content = fs::read(&backup_path).expect("read backup");

    // Restore the backup
    let restore = run_br(
        &workspace,
        ["history", "restore", backup_file, "--force"],
        "history_restore_real",
    );
    assert!(
        restore.status.success(),
        "restore failed: {}",
        restore.stderr
    );

    // Verify content matches
    let restored_content =
        fs::read(workspace.root.join(".beads").join("issues.jsonl")).expect("read restored");
    assert_eq!(
        restored_content, original_content,
        "restored content should match backup"
    );
}

#[test]
fn e2e_history_prune_with_real_dataset() {
    let _log = common::test_log("e2e_history_prune_with_real_dataset");

    if !is_dataset_available() {
        eprintln!("Skipping e2e_history_prune_with_real_dataset: beads_rust dataset not available");
        return;
    }

    // Create isolated workspace from real dataset
    let isolated = IsolatedDataset::from_dataset(KnownDataset::BeadsRust)
        .expect("should copy beads_rust dataset");

    let workspace = BrWorkspace {
        temp_dir: isolated.temp_dir,
        root: isolated.root.clone(),
        log_dir: isolated.root.join("logs"),
    };
    fs::create_dir_all(&workspace.log_dir).ok();

    // Create multiple backups
    for i in 0..4 {
        thread::sleep(Duration::from_millis(1100));
        create_issue(
            &workspace,
            &format!("Real dataset issue {i}"),
            &format!("create_real_{i}"),
        );
        sync_flush(&workspace);
    }

    let backups_before = list_backup_files(&workspace);

    // Prune keeping 2
    let prune = run_br(
        &workspace,
        ["history", "prune", "--keep", "2"],
        "history_prune_real",
    );
    assert!(prune.status.success(), "prune failed: {}", prune.stderr);

    let backups_after = list_backup_files(&workspace);
    assert!(
        backups_after.len() <= 2,
        "should have at most 2 backups: before={}, after={}",
        backups_before.len(),
        backups_after.len()
    );
}

// =============================================================================
// EDGE CASES AND DESTRUCTIVE OPERATION GUARDS
// =============================================================================

#[test]
fn e2e_history_restore_requires_force_when_exists() {
    let _log = common::test_log("e2e_history_restore_requires_force_when_exists");
    let workspace = setup_workspace_with_jsonl();

    // Create issue to trigger backup
    create_issue(&workspace, "Issue for force test", "create_force");
    sync_flush(&workspace);

    let backups = list_backup_files(&workspace);
    assert!(!backups.is_empty());
    let backup_file = &backups[0];

    // Try restore without --force
    let restore = run_br(
        &workspace,
        ["history", "restore", backup_file],
        "history_restore_no_force",
    );
    assert!(
        !restore.status.success(),
        "restore should fail without --force"
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
fn e2e_history_restore_succeeds_without_force_when_missing() {
    let _log = common::test_log("e2e_history_restore_succeeds_without_force_when_missing");
    let workspace = setup_workspace_with_jsonl();

    // Create issue to trigger backup
    create_issue(&workspace, "Issue for no force test", "create_noforce");
    sync_flush(&workspace);

    let backups = list_backup_files(&workspace);
    assert!(!backups.is_empty());
    let backup_file = &backups[0];

    // Delete the current issues.jsonl
    fs::remove_file(workspace.root.join(".beads").join("issues.jsonl")).expect("delete jsonl");

    // Restore without --force should succeed when target doesn't exist
    let restore = run_br(
        &workspace,
        ["history", "restore", backup_file],
        "history_restore_noforce_ok",
    );
    assert!(
        restore.status.success(),
        "restore should succeed without --force when target missing: {}",
        restore.stderr
    );

    // Verify file was restored
    assert!(workspace.root.join(".beads").join("issues.jsonl").exists());
}

#[test]
fn e2e_history_prune_with_keep_zero_deletes_all() {
    let _log = common::test_log("e2e_history_prune_with_keep_zero_deletes_all");
    let workspace = setup_workspace_with_jsonl();

    // Create backups
    for i in 0..3 {
        thread::sleep(Duration::from_millis(1100));
        create_issue(
            &workspace,
            &format!("Issue for zero keep {i}"),
            &format!("create_zero_{i}"),
        );
        sync_flush(&workspace);
    }

    let backups_before = list_backup_files(&workspace);
    assert!(!backups_before.is_empty(), "should have backups");

    // Prune with --keep 0
    let prune = run_br(
        &workspace,
        ["history", "prune", "--keep", "0"],
        "history_prune_zero",
    );
    assert!(prune.status.success());

    // All backups should be deleted
    let backups_after = list_backup_files(&workspace);
    assert!(
        backups_after.is_empty(),
        "all backups should be deleted with --keep 0: {backups_after:?}"
    );
}

// =============================================================================
// QUIET MODE TESTS
// =============================================================================

#[test]
fn e2e_history_restore_quiet_mode() {
    let _log = common::test_log("e2e_history_restore_quiet_mode");
    let workspace = setup_workspace_with_jsonl();

    create_issue(&workspace, "Issue for quiet", "create_quiet");
    sync_flush(&workspace);

    let backups = list_backup_files(&workspace);
    assert!(!backups.is_empty());
    let backup_file = &backups[0];

    // Restore with --quiet
    let restore = run_br(
        &workspace,
        ["--quiet", "history", "restore", backup_file, "--force"],
        "history_restore_quiet",
    );
    assert!(restore.status.success());
    assert!(
        restore.stdout.is_empty() || restore.stdout.trim().is_empty(),
        "quiet mode should produce no stdout: '{}'",
        restore.stdout
    );
}

#[test]
fn e2e_history_prune_quiet_mode() {
    let _log = common::test_log("e2e_history_prune_quiet_mode");
    let workspace = setup_workspace_with_jsonl();

    create_issue(&workspace, "Issue for prune quiet", "create_prune_quiet");
    sync_flush(&workspace);

    // Prune with --quiet
    let prune = run_br(
        &workspace,
        ["--quiet", "history", "prune", "--keep", "1"],
        "history_prune_quiet",
    );
    assert!(prune.status.success());
    assert!(
        prune.stdout.is_empty() || prune.stdout.trim().is_empty(),
        "quiet mode should produce no stdout: '{}'",
        prune.stdout
    );
}
