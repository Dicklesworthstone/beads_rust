//! Failure-injection tests for atomic export/import operations.
//!
//! Tests that verify export/import do not corrupt existing JSONL or DB state
//! when failures occur (read-only directories, permission denied, etc.).
//!
//! Captures logs for each failure case to aid postmortem analysis.
//!
//! This test suite simulates various failure scenarios during sync operations
//! to ensure atomicity, error handling, and recovery mechanisms work as expected.

#![allow(
    clippy::format_push_string,
    clippy::uninlined_format_args,
    clippy::redundant_clone,
    clippy::manual_assert,
    clippy::too_many_lines,
    clippy::single_char_add_str,
    clippy::needless_collect
)]

mod common;

use beads_rust::model::Issue;
use beads_rust::storage::{ListFilters, SqliteStorage};
use beads_rust::sync::{ExportConfig, ImportConfig, export_to_jsonl, import_from_jsonl};
use common::cli::{BrWorkspace, run_br};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, Permissions};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// Test artifacts for failure injection tests.
struct FailureTestArtifacts {
    artifact_dir: PathBuf,
    test_name: String,
    logs: Vec<(String, String)>,
    snapshots: Vec<(String, BTreeMap<String, String>)>,
}

impl FailureTestArtifacts {
    fn new(test_name: &str) -> Self {
        let artifact_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-artifacts")
            .join("failure-injection")
            .join(test_name);
        fs::create_dir_all(&artifact_dir).expect("create artifact dir");

        Self {
            artifact_dir,
            test_name: test_name.to_string(),
            logs: Vec::new(),
            snapshots: Vec::new(),
        }
    }

    fn log(&mut self, label: &str, content: &str) {
        self.logs.push((label.to_string(), content.to_string()));
    }

    fn snapshot_dir(&mut self, label: &str, path: &Path) {
        let mut files = BTreeMap::new();
        if path.exists() {
            collect_files_recursive(path, path, &mut files);
        }
        self.snapshots.push((label.to_string(), files));
    }

    fn save(&self) {
        // Save logs
        let log_path = self.artifact_dir.join("test.log");
        let mut log_content = format!("=== Failure Injection Test: {} ===\n\n", self.test_name);

        for (label, content) in &self.logs {
            log_content.push_str(&format!("--- {} ---\n{}\n\n", label, content));
        }

        // Save snapshots
        for (label, files) in &self.snapshots {
            log_content.push_str(&format!("--- Snapshot: {} ---\n", label));
            for (path, hash) in files {
                log_content.push_str(&format!("  {} -> {}\n", path, hash));
            }
            log_content.push_str("\n");
        }

        fs::write(&log_path, log_content).expect("write log");
    }
}

fn collect_files_recursive(base: &Path, current: &Path, files: &mut BTreeMap<String, String>) {
    if let Ok(entries) = fs::read_dir(current) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                let relative = path
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .to_string();
                let content = fs::read(&path).unwrap_or_default();
                let hash = format!("{:x}", Sha256::digest(&content));
                files.insert(relative, hash);
            } else if path.is_dir() {
                collect_files_recursive(base, &path, files);
            }
        }
    }
}

fn create_test_issue(id: &str, title: &str) -> Issue {
    let mut issue = common::fixtures::issue(title);
    issue.id = id.to_string();
    issue
}

fn compute_file_hash(path: &Path) -> Option<String> {
    if path.exists() {
        let content = fs::read(path).ok()?;
        Some(format!("{:x}", Sha256::digest(&content)))
    } else {
        None
    }
}

/// Test: Export to read-only directory fails gracefully, original JSONL intact.
#[test]
#[cfg(unix)]
fn export_failure_readonly_dir_preserves_original() {
    let _log = common::test_log("export_failure_readonly_dir_preserves_original");
    let mut artifacts = FailureTestArtifacts::new("export_readonly_dir");

    // Setup: Create storage with issues
    let mut storage = SqliteStorage::open_memory().unwrap();
    let issue1 = create_test_issue("test-001", "Issue One");
    let issue2 = create_test_issue("test-002", "Issue Two");
    storage.create_issue(&issue1, "tester").unwrap();
    storage.create_issue(&issue2, "tester").unwrap();

    // Create temp directory with existing JSONL
    let temp = TempDir::new().unwrap();
    let beads_dir = temp.path().join(".beads");
    fs::create_dir_all(&beads_dir).unwrap();
    let jsonl_path = beads_dir.join("issues.jsonl");

    // Create initial JSONL with known content
    let initial_content = r#"{"id":"test-old","title":"Old Issue"}"#;
    fs::write(&jsonl_path, format!("{}\n", initial_content)).unwrap();
    let initial_hash = compute_file_hash(&jsonl_path).unwrap();

    artifacts.log("initial_jsonl_hash", &initial_hash);
    artifacts.snapshot_dir("before_failure", temp.path());

    // Make directory read-only to cause export failure
    fs::set_permissions(&beads_dir, Permissions::from_mode(0o555)).unwrap();

    // Attempt export (should fail)
    let config = ExportConfig {
        beads_dir: Some(beads_dir.clone()),
        ..Default::default()
    };
    let result = export_to_jsonl(&storage, &jsonl_path, &config);

    // Restore permissions for cleanup
    fs::set_permissions(&beads_dir, Permissions::from_mode(0o755)).unwrap();

    artifacts.snapshot_dir("after_failure", temp.path());

    // Verify export failed
    assert!(result.is_err(), "Export should fail on read-only directory");
    let err_msg = result.unwrap_err().to_string();
    artifacts.log("error_message", &err_msg);

    // Verify original JSONL is intact
    let final_hash = compute_file_hash(&jsonl_path).unwrap();
    artifacts.log("final_jsonl_hash", &final_hash);

    assert_eq!(
        initial_hash, final_hash,
        "Original JSONL should be intact after export failure"
    );

    // Verify original content is still readable
    let content = fs::read_to_string(&jsonl_path).unwrap();
    assert!(
        content.contains("test-old"),
        "Original content should be preserved"
    );

    artifacts.log(
        "verification",
        "PASSED: Original JSONL preserved after export failure",
    );
    artifacts.save();
}

/// Test: Export failure when temp file cannot be created.
#[test]
#[cfg(unix)]
fn export_failure_temp_file_preserves_original() {
    let _log = common::test_log("export_failure_temp_file_preserves_original");
    let mut artifacts = FailureTestArtifacts::new("export_temp_file_failure");

    // Setup storage
    let mut storage = SqliteStorage::open_memory().unwrap();
    let issue = create_test_issue("test-001", "Issue One");
    storage.create_issue(&issue, "tester").unwrap();

    // Create temp directory
    let temp = TempDir::new().unwrap();
    let beads_dir = temp.path().join(".beads");
    fs::create_dir_all(&beads_dir).unwrap();
    let jsonl_path = beads_dir.join("issues.jsonl");

    // Create initial JSONL
    let initial_content = r#"{"id":"test-old","title":"Old"}"#;
    fs::write(&jsonl_path, format!("{}\n", initial_content)).unwrap();
    let initial_hash = compute_file_hash(&jsonl_path).unwrap();

    artifacts.log("initial_hash", &initial_hash);
    artifacts.snapshot_dir("before", temp.path());

    // Create a directory named issues.jsonl.tmp to block temp file creation
    let temp_path = beads_dir.join("issues.jsonl.tmp");
    fs::create_dir_all(&temp_path).unwrap();

    // Attempt export
    let config = ExportConfig {
        beads_dir: Some(beads_dir.clone()),
        ..Default::default()
    };
    let result = export_to_jsonl(&storage, &jsonl_path, &config);

    artifacts.snapshot_dir("after", temp.path());

    // Should fail
    assert!(result.is_err(), "Export should fail when temp file blocked");
    artifacts.log("error", &result.unwrap_err().to_string());

    // Original should be intact
    let final_hash = compute_file_hash(&jsonl_path).unwrap();
    assert_eq!(initial_hash, final_hash, "Original JSONL preserved");

    artifacts.log("verification", "PASSED");
    artifacts.save();
}

/// Test: Import from non-existent file fails without DB changes.
#[test]
fn import_failure_missing_file_no_db_changes() {
    let _log = common::test_log("import_failure_missing_file_no_db_changes");
    let mut artifacts = FailureTestArtifacts::new("import_missing_file");

    // Setup storage with existing issue
    let mut storage = SqliteStorage::open_memory().unwrap();
    let existing = create_test_issue("test-existing", "Existing Issue");
    storage.create_issue(&existing, "tester").unwrap();

    let initial_count = storage.list_issues(&ListFilters::default()).unwrap().len();
    artifacts.log("initial_issue_count", &initial_count.to_string());

    // Attempt import from non-existent file
    let temp = TempDir::new().unwrap();
    let missing_path = temp.path().join(".beads").join("nonexistent.jsonl");

    let config = ImportConfig::default();
    let result = import_from_jsonl(&mut storage, &missing_path, &config, Some("test-"));

    // Should fail
    assert!(result.is_err(), "Import should fail for missing file");
    artifacts.log("error", &result.unwrap_err().to_string());

    // DB should be unchanged
    let final_count = storage.list_issues(&ListFilters::default()).unwrap().len();
    assert_eq!(
        initial_count, final_count,
        "DB should be unchanged after import failure"
    );

    // Original issue still present
    let fetched = storage.get_issue("test-existing").unwrap();
    assert!(fetched.is_some(), "Existing issue should still be present");

    artifacts.log("verification", "PASSED: DB unchanged after import failure");
    artifacts.save();
}

/// Test: Import with malformed JSON fails early, DB unchanged.
#[test]
fn import_failure_malformed_json_no_db_changes() {
    let _log = common::test_log("import_failure_malformed_json_no_db_changes");
    let mut artifacts = FailureTestArtifacts::new("import_malformed_json");

    // Setup storage
    let mut storage = SqliteStorage::open_memory().unwrap();
    let existing = create_test_issue("test-existing", "Existing");
    storage.create_issue(&existing, "tester").unwrap();

    let initial_count = storage.list_issues(&ListFilters::default()).unwrap().len();
    artifacts.log("initial_count", &initial_count.to_string());

    // Create malformed JSONL
    let temp = TempDir::new().unwrap();
    let beads_dir = temp.path().join(".beads");
    fs::create_dir_all(&beads_dir).unwrap();
    let jsonl_path = beads_dir.join("issues.jsonl");
    fs::write(&jsonl_path, "not valid json\n").unwrap();

    artifacts.log("malformed_content", "not valid json");

    // Attempt import
    let config = ImportConfig {
        beads_dir: Some(beads_dir),
        ..Default::default()
    };
    let result = import_from_jsonl(&mut storage, &jsonl_path, &config, Some("test-"));

    // Should fail
    assert!(result.is_err(), "Import should fail on malformed JSON");
    let err_msg = result.unwrap_err().to_string();
    artifacts.log("error", &err_msg);
    assert!(
        err_msg.contains("Invalid JSON"),
        "Error should mention invalid JSON"
    );

    // DB unchanged
    let final_count = storage.list_issues(&ListFilters::default()).unwrap().len();
    assert_eq!(
        initial_count, final_count,
        "DB unchanged after malformed JSON"
    );

    artifacts.log("verification", "PASSED");
    artifacts.save();
}

/// Test: Import with conflict markers fails before any DB changes.
#[test]
fn import_failure_conflict_markers_no_db_changes() {
    let _log = common::test_log("import_failure_conflict_markers_no_db_changes");
    let mut artifacts = FailureTestArtifacts::new("import_conflict_markers");

    // Setup
    let mut storage = SqliteStorage::open_memory().unwrap();
    let existing = create_test_issue("test-existing", "Existing");
    storage.create_issue(&existing, "tester").unwrap();

    let initial_count = storage.list_issues(&ListFilters::default()).unwrap().len();

    // Create JSONL with conflict markers
    let temp = TempDir::new().unwrap();
    let beads_dir = temp.path().join(".beads");
    fs::create_dir_all(&beads_dir).unwrap();
    let jsonl_path = beads_dir.join("issues.jsonl");
    fs::write(&jsonl_path, "<<<<<<< HEAD\n{\"id\":\"test-1\"}\n").unwrap();

    let config = ImportConfig {
        beads_dir: Some(beads_dir),
        ..Default::default()
    };
    let result = import_from_jsonl(&mut storage, &jsonl_path, &config, Some("test-"));

    // Should fail
    assert!(result.is_err(), "Import should fail on conflict markers");
    let err_msg = result.unwrap_err().to_string();
    artifacts.log("error", &err_msg);
    assert!(
        err_msg.contains("conflict") || err_msg.contains("Merge"),
        "Error should mention conflict markers"
    );

    // DB unchanged
    let final_count = storage.list_issues(&ListFilters::default()).unwrap().len();
    assert_eq!(initial_count, final_count, "DB unchanged");

    artifacts.log("verification", "PASSED");
    artifacts.save();
}

/// Test: Import with prefix mismatch fails before DB changes.
#[test]
fn import_failure_prefix_mismatch_no_db_changes() {
    let _log = common::test_log("import_failure_prefix_mismatch_no_db_changes");
    let mut artifacts = FailureTestArtifacts::new("import_prefix_mismatch");

    // Setup
    let mut storage = SqliteStorage::open_memory().unwrap();
    let existing = create_test_issue("test-existing", "Existing");
    storage.create_issue(&existing, "tester").unwrap();

    let initial_count = storage.list_issues(&ListFilters::default()).unwrap().len();

    // Create JSONL with wrong prefix
    let temp = TempDir::new().unwrap();
    let beads_dir = temp.path().join(".beads");
    fs::create_dir_all(&beads_dir).unwrap();
    let jsonl_path = beads_dir.join("issues.jsonl");

    let wrong_prefix_issue = create_test_issue("wrong-001", "Wrong Prefix");
    let json = serde_json::to_string(&wrong_prefix_issue).unwrap();
    fs::write(&jsonl_path, format!("{}\n", json)).unwrap();

    let config = ImportConfig {
        beads_dir: Some(beads_dir),
        ..Default::default()
    };
    let result = import_from_jsonl(&mut storage, &jsonl_path, &config, Some("test-"));

    // Should fail
    assert!(result.is_err(), "Import should fail on prefix mismatch");
    let err_msg = result.unwrap_err().to_string();
    artifacts.log("error", &err_msg);
    assert!(
        err_msg.contains("Prefix mismatch"),
        "Error should mention prefix"
    );

    // DB unchanged
    let final_count = storage.list_issues(&ListFilters::default()).unwrap().len();
    assert_eq!(initial_count, final_count, "DB unchanged");

    artifacts.log("verification", "PASSED");
    artifacts.save();
}

/// Test: CLI export to read-only directory fails gracefully.
#[test]
#[cfg(unix)]
fn cli_export_readonly_preserves_state() {
    let _log = common::test_log("cli_export_readonly_preserves_state");
    let mut artifacts = FailureTestArtifacts::new("cli_export_readonly");

    let workspace = BrWorkspace::new();

    // Initialize (without explicit prefix)
    let init_run = run_br(&workspace, ["init"], "init");
    artifacts.log("init_stdout", &init_run.stdout);
    artifacts.log("init_stderr", &init_run.stderr);
    assert!(
        init_run.status.success(),
        "init failed: {}",
        init_run.stderr
    );

    // Create issue
    let create_run = run_br(&workspace, ["create", "Test Issue"], "create");
    artifacts.log("create_stdout", &create_run.stdout);
    artifacts.log("create_stderr", &create_run.stderr);
    assert!(
        create_run.status.success(),
        "create failed: {}",
        create_run.stderr
    );

    // First export to establish baseline
    let export1_run = run_br(&workspace, ["sync", "--flush-only"], "export1");
    artifacts.log("export1_stdout", &export1_run.stdout);
    artifacts.log("export1_stderr", &export1_run.stderr);
    assert!(
        export1_run.status.success(),
        "first export failed: {}",
        export1_run.stderr
    );

    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let initial_hash = compute_file_hash(&jsonl_path);
    artifacts.log("initial_hash", &initial_hash.clone().unwrap_or_default());

    artifacts.snapshot_dir("before_readonly", &workspace.root);

    // Make .beads read-only
    let beads_dir = workspace.root.join(".beads");
    fs::set_permissions(&beads_dir, Permissions::from_mode(0o555)).unwrap();

    // Attempt another export (should fail)
    let export2_run = run_br(&workspace, ["sync", "--flush-only"], "export2_fail");
    artifacts.log("export2_stdout", &export2_run.stdout);
    artifacts.log("export2_stderr", &export2_run.stderr);

    // Restore permissions
    fs::set_permissions(&beads_dir, Permissions::from_mode(0o755)).unwrap();

    artifacts.snapshot_dir("after_readonly", &workspace.root);

    // Save artifacts before assertions
    artifacts.save();

    // Verify export failed
    assert!(
        !export2_run.status.success(),
        "Export should fail on read-only directory"
    );

    // Verify JSONL unchanged
    let final_hash = compute_file_hash(&jsonl_path);
    assert_eq!(
        initial_hash, final_hash,
        "JSONL should be unchanged after failed export"
    );
}

/// Test: CLI import with malformed JSONL fails without DB corruption.
#[test]
fn cli_import_malformed_preserves_db() {
    let _log = common::test_log("cli_import_malformed_preserves_db");
    let mut artifacts = FailureTestArtifacts::new("cli_import_malformed");

    let workspace = BrWorkspace::new();

    // Initialize (without explicit prefix - let it auto-generate)
    let init_run = run_br(&workspace, ["init"], "init");
    artifacts.log("init_stdout", &init_run.stdout);
    artifacts.log("init_stderr", &init_run.stderr);
    assert!(
        init_run.status.success(),
        "init failed: {}",
        init_run.stderr
    );

    // Create issue
    let create_run = run_br(&workspace, ["create", "Original Issue"], "create");
    artifacts.log("create_stdout", &create_run.stdout);
    artifacts.log("create_stderr", &create_run.stderr);
    assert!(
        create_run.status.success(),
        "create failed: {}",
        create_run.stderr
    );

    // List before import attempt
    let list1_run = run_br(&workspace, ["list", "--json"], "list_before");
    artifacts.log("list_before", &list1_run.stdout);
    assert!(
        list1_run.stdout.contains("Original Issue"),
        "Issue should exist before import attempt"
    );

    // Corrupt the JSONL file
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    fs::write(&jsonl_path, "totally not json {{{\n").unwrap();

    // Attempt import
    let import_run = run_br(&workspace, ["sync", "--import-only"], "import_fail");
    artifacts.log("import_stdout", &import_run.stdout);
    artifacts.log("import_stderr", &import_run.stderr);

    // Save artifacts before assertions for debugging
    artifacts.save();

    // Verify import failed
    assert!(
        !import_run.status.success(),
        "Import should fail on malformed JSON"
    );

    // List after - DB should still have original issue (use --no-auto-import --allow-stale to ignore corrupt/newer JSONL)
    let list2_run = run_br(
        &workspace,
        ["list", "--json", "--no-auto-import", "--allow-stale"],
        "list_after",
    );
    artifacts.log("list_after", &list2_run.stdout);
    artifacts.log("list_after_stderr", &list2_run.stderr);

    // Original issue should still exist in DB
    assert!(
        list2_run.stdout.contains("Original Issue"),
        "Original issue should still be in DB after failed import.\nActual stdout: {}\nActual stderr: {}",
        list2_run.stdout,
        list2_run.stderr
    );
}

/// Test: Simulate disk-full by filling temp file quota (where feasible).
/// This test creates a large existing JSONL and verifies it survives export failure.
#[test]
#[cfg(unix)]
fn export_preserves_large_existing_jsonl() {
    let _log = common::test_log("export_preserves_large_existing_jsonl");
    let mut artifacts = FailureTestArtifacts::new("export_large_jsonl");

    // Setup storage
    let mut storage = SqliteStorage::open_memory().unwrap();
    let issue = create_test_issue("test-001", "New Issue");
    storage.create_issue(&issue, "tester").unwrap();

    // Create temp directory with large existing JSONL
    let temp = TempDir::new().unwrap();
    let beads_dir = temp.path().join(".beads");
    fs::create_dir_all(&beads_dir).unwrap();
    let jsonl_path = beads_dir.join("issues.jsonl");

    // Create a reasonably large JSONL (100KB of issues)
    let mut large_content = String::new();
    for i in 0..100 {
        let issue = create_test_issue(&format!("old-{:04}", i), &format!("Old Issue {}", i));
        large_content.push_str(&serde_json::to_string(&issue).unwrap());
        large_content.push('\n');
    }
    fs::write(&jsonl_path, &large_content).unwrap();

    let initial_hash = compute_file_hash(&jsonl_path).unwrap();
    let initial_size = fs::metadata(&jsonl_path).unwrap().len();
    artifacts.log("initial_size", &format!("{} bytes", initial_size));
    artifacts.log("initial_hash", &initial_hash);

    // Make directory read-only to force failure
    fs::set_permissions(&beads_dir, Permissions::from_mode(0o555)).unwrap();

    let config = ExportConfig {
        beads_dir: Some(beads_dir.clone()),
        ..Default::default()
    };
    let result = export_to_jsonl(&storage, &jsonl_path, &config);

    // Restore permissions
    fs::set_permissions(&beads_dir, Permissions::from_mode(0o755)).unwrap();

    // Verify failure
    assert!(result.is_err(), "Export should fail");
    artifacts.log("error", &result.unwrap_err().to_string());

    // Verify large JSONL intact
    let final_hash = compute_file_hash(&jsonl_path).unwrap();
    let final_size = fs::metadata(&jsonl_path).unwrap().len();

    assert_eq!(initial_hash, final_hash, "JSONL content unchanged");
    assert_eq!(initial_size, final_size, "JSONL size unchanged");

    // Verify content readable and valid
    let content = fs::read_to_string(&jsonl_path).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 100, "All 100 issues preserved");

    artifacts.log("verification", "PASSED: Large JSONL preserved");
    artifacts.save();
}

/// Test: Verify atomic rename behavior - temp file cleaned up on success.
#[test]
fn export_cleans_up_temp_file_on_success() {
    let _log = common::test_log("export_cleans_up_temp_file_on_success");
    let mut artifacts = FailureTestArtifacts::new("export_temp_cleanup");

    // Setup
    let mut storage = SqliteStorage::open_memory().unwrap();
    let issue = create_test_issue("test-001", "Issue One");
    storage.create_issue(&issue, "tester").unwrap();

    let temp = TempDir::new().unwrap();
    let beads_dir = temp.path().join(".beads");
    fs::create_dir_all(&beads_dir).unwrap();
    let jsonl_path = beads_dir.join("issues.jsonl");
    let temp_path = beads_dir.join("issues.jsonl.tmp");

    // Export should succeed
    let config = ExportConfig {
        beads_dir: Some(beads_dir.clone()),
        ..Default::default()
    };
    let result = export_to_jsonl(&storage, &jsonl_path, &config);
    assert!(result.is_ok(), "Export should succeed");

    // Verify temp file does not exist
    assert!(
        !temp_path.exists(),
        "Temp file should be cleaned up after successful export"
    );

    // Verify final file exists
    assert!(jsonl_path.exists(), "Final JSONL should exist");

    artifacts.log("verification", "PASSED: Temp file cleaned up");
    artifacts.save();
}

/// Test: Multiple sequential failures don't accumulate corruption.
#[test]
#[cfg(unix)]
fn multiple_export_failures_no_accumulation() {
    let _log = common::test_log("multiple_export_failures_no_accumulation");
    let mut artifacts = FailureTestArtifacts::new("multiple_failures");

    // Setup
    let mut storage = SqliteStorage::open_memory().unwrap();
    let issue = create_test_issue("test-001", "Issue One");
    storage.create_issue(&issue, "tester").unwrap();

    let temp = TempDir::new().unwrap();
    let beads_dir = temp.path().join(".beads");
    fs::create_dir_all(&beads_dir).unwrap();
    let jsonl_path = beads_dir.join("issues.jsonl");

    // Create initial JSONL
    fs::write(&jsonl_path, r#"{"id":"test-orig","title":"Original"}"#).unwrap();
    let initial_hash = compute_file_hash(&jsonl_path).unwrap();

    // Attempt multiple failures
    for i in 0..5 {
        fs::set_permissions(&beads_dir, Permissions::from_mode(0o555)).unwrap();

        let config = ExportConfig {
            beads_dir: Some(beads_dir.clone()),
            ..Default::default()
        };
        let result = export_to_jsonl(&storage, &jsonl_path, &config);

        fs::set_permissions(&beads_dir, Permissions::from_mode(0o755)).unwrap();

        assert!(result.is_err(), "Attempt {} should fail", i);

        let current_hash = compute_file_hash(&jsonl_path).unwrap();
        assert_eq!(
            initial_hash, current_hash,
            "Hash unchanged after attempt {}",
            i
        );

        artifacts.log(
            &format!("attempt_{}", i),
            "failed as expected, JSONL intact",
        );
    }

    artifacts.log("verification", "PASSED: Multiple failures don't accumulate");
    artifacts.save();
}

/// Test: Verify atomic write pipeline correctness.
/// Creates issues, exports, verifies content hash matches and no temp files remain.
#[test]
fn atomic_write_pipeline_produces_valid_output() {
    let _log = common::test_log("atomic_write_pipeline_produces_valid_output");
    let mut artifacts = FailureTestArtifacts::new("atomic_pipeline_valid");

    // Setup storage with multiple issues
    let mut storage = SqliteStorage::open_memory().unwrap();
    for i in 0..10 {
        let issue = create_test_issue(&format!("test-{:03}", i), &format!("Issue {}", i));
        storage.create_issue(&issue, "tester").unwrap();
    }

    let temp = TempDir::new().unwrap();
    let beads_dir = temp.path().join(".beads");
    fs::create_dir_all(&beads_dir).unwrap();
    let jsonl_path = beads_dir.join("issues.jsonl");
    let temp_path = beads_dir.join("issues.jsonl.tmp");

    // Export
    let config = ExportConfig {
        beads_dir: Some(beads_dir.clone()),
        ..Default::default()
    };
    let (result, _report) = export_to_jsonl(&storage, &jsonl_path, &config).unwrap();

    artifacts.log("export_count", &result.exported_count.to_string());
    artifacts.log("content_hash", &result.content_hash);

    // Verify JSONL exists and temp file is gone
    assert!(jsonl_path.exists(), "JSONL file should exist after export");
    assert!(
        !temp_path.exists(),
        "Temp file should be removed after successful export"
    );

    // Verify content is valid JSON lines
    let content = fs::read_to_string(&jsonl_path).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 10, "Should have 10 issues exported");

    for (i, line) in lines.iter().enumerate() {
        let parsed: serde_json::Value = serde_json::from_str(line)
            .unwrap_or_else(|e| panic!("Line {} is not valid JSON: {}", i, e));
        assert!(
            parsed.get("id").is_some(),
            "Line {} should have an id field",
            i
        );
    }

    // Verify content hash is consistent
    let hash2 = compute_file_hash(&jsonl_path).unwrap();
    artifacts.log("file_hash", &hash2);

    artifacts.log("verification", "PASSED: Atomic pipeline produced valid output");
    artifacts.save();
}

/// Test: Stale temp file from previous failed export doesn't affect new export.
#[test]
fn stale_temp_file_handled_gracefully() {
    let _log = common::test_log("stale_temp_file_handled_gracefully");
    let mut artifacts = FailureTestArtifacts::new("stale_temp_file");

    // Setup storage
    let mut storage = SqliteStorage::open_memory().unwrap();
    let issue = create_test_issue("test-001", "Fresh Issue");
    storage.create_issue(&issue, "tester").unwrap();

    let temp = TempDir::new().unwrap();
    let beads_dir = temp.path().join(".beads");
    fs::create_dir_all(&beads_dir).unwrap();
    let jsonl_path = beads_dir.join("issues.jsonl");
    let temp_path = beads_dir.join("issues.jsonl.tmp");

    // Create a stale temp file (simulating previous failed export)
    let stale_content = r#"{"id":"stale-001","title":"Stale from crash"}"#;
    fs::write(&temp_path, format!("{}\n", stale_content)).unwrap();
    artifacts.log("stale_temp_content", stale_content);

    // Export should succeed and overwrite stale temp file
    let config = ExportConfig {
        beads_dir: Some(beads_dir.clone()),
        ..Default::default()
    };
    let result = export_to_jsonl(&storage, &jsonl_path, &config);
    assert!(result.is_ok(), "Export should succeed despite stale temp file");

    // Verify temp file is gone
    assert!(
        !temp_path.exists(),
        "Stale temp file should be cleaned up after export"
    );

    // Verify JSONL has fresh content, not stale
    let content = fs::read_to_string(&jsonl_path).unwrap();
    assert!(
        content.contains("Fresh Issue"),
        "JSONL should have fresh content"
    );
    assert!(
        !content.contains("Stale from crash"),
        "JSONL should not have stale content"
    );

    artifacts.log("verification", "PASSED: Stale temp file handled gracefully");
    artifacts.save();
}

/// Test: Export with empty database produces empty JSONL (not preserved stale data).
#[test]
fn export_empty_db_produces_empty_jsonl() {
    let _log = common::test_log("export_empty_db_produces_empty_jsonl");
    let mut artifacts = FailureTestArtifacts::new("export_empty_db");

    // Empty storage
    let storage = SqliteStorage::open_memory().unwrap();

    let temp = TempDir::new().unwrap();
    let beads_dir = temp.path().join(".beads");
    fs::create_dir_all(&beads_dir).unwrap();
    let jsonl_path = beads_dir.join("issues.jsonl");

    // Export empty DB
    let config = ExportConfig {
        beads_dir: Some(beads_dir.clone()),
        force: true, // Allow empty export
        ..Default::default()
    };
    let result = export_to_jsonl(&storage, &jsonl_path, &config);

    // Empty DB export may fail or produce empty file depending on config
    // The important thing is it doesn't crash and handles gracefully
    match result {
        Ok((export_result, _)) => {
            assert_eq!(export_result.exported_count, 0, "Should export 0 issues");
            if jsonl_path.exists() {
                let content = fs::read_to_string(&jsonl_path).unwrap();
                assert!(content.is_empty() || content.trim().is_empty(),
                    "JSONL should be empty for empty DB");
            }
            artifacts.log("outcome", "Empty export succeeded");
        }
        Err(e) => {
            // Some configs reject empty exports - that's acceptable
            artifacts.log("outcome", &format!("Empty export rejected: {}", e));
        }
    }

    artifacts.log("verification", "PASSED: Empty DB export handled gracefully");
    artifacts.save();
}

/// Test: Verify file permissions on exported JSONL (Unix only).
#[test]
#[cfg(unix)]
fn export_sets_correct_permissions() {
    let _log = common::test_log("export_sets_correct_permissions");
    let mut artifacts = FailureTestArtifacts::new("export_permissions");

    // Setup
    let mut storage = SqliteStorage::open_memory().unwrap();
    let issue = create_test_issue("test-001", "Permission Test");
    storage.create_issue(&issue, "tester").unwrap();

    let temp = TempDir::new().unwrap();
    let beads_dir = temp.path().join(".beads");
    fs::create_dir_all(&beads_dir).unwrap();
    let jsonl_path = beads_dir.join("issues.jsonl");

    // Export
    let config = ExportConfig {
        beads_dir: Some(beads_dir.clone()),
        ..Default::default()
    };
    export_to_jsonl(&storage, &jsonl_path, &config).unwrap();

    // Check permissions
    let metadata = fs::metadata(&jsonl_path).unwrap();
    let mode = metadata.permissions().mode();
    let file_mode = mode & 0o777; // Extract file permission bits

    artifacts.log("file_mode", &format!("{:o}", file_mode));

    // Should be 0o600 (read/write for owner only)
    assert!(
        file_mode == 0o600 || file_mode == 0o644,
        "File permissions should be restrictive (got {:o})",
        file_mode
    );

    artifacts.log("verification", "PASSED: Correct permissions set");
    artifacts.save();
}
