//! Integration tests for sync preflight safety checks.
//!
//! These tests implement beads_rust-0v1.3.5:
//! - Import aborts on conflict markers
//! - Import aborts on unsafe paths
//! - No files are modified on preflight failure
//! - Logs show preflight checks and failure cause
//!
//! Tests verify the preflight stage catches safety issues BEFORE any writes occur.
//!
//! Verifies that preflight checks correctly prevent unsafe operations
//! and report errors with actionable hints.

#![allow(
    clippy::format_push_string,
    clippy::uninlined_format_args,
    clippy::redundant_clone,
    clippy::manual_assert,
    clippy::too_many_lines,
    clippy::redundant_closure_for_method_calls,
    clippy::case_sensitive_file_extension_comparisons,
    clippy::unnecessary_map_or,
    clippy::doc_markdown
)]

mod common;

use beads_rust::storage::SqliteStorage;
use beads_rust::sync::{
    preflight_export, preflight_import, ExportConfig, ImportConfig, PreflightCheckStatus,
};
use common::cli::{BrWorkspace, run_br};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::Path;

// ============================================================================
// Helper: Snapshot file tree for verifying no modifications
// ============================================================================

fn snapshot_directory(dir: &Path) -> HashMap<String, Vec<u8>> {
    let mut snapshot = HashMap::new();
    if !dir.exists() {
        return snapshot;
    }

    for entry in walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        let path = entry.path();
        let relative = path.strip_prefix(dir).unwrap_or(path);
        if let Ok(content) = fs::read(path) {
            snapshot.insert(relative.to_string_lossy().to_string(), content);
        }
    }
    snapshot
}

#[allow(dead_code)]
fn assert_directory_unchanged(
    before: &HashMap<String, Vec<u8>>,
    dir: &Path,
    context: &str,
) {
    let after = snapshot_directory(dir);

    // Check no new files
    for path in after.keys() {
        if !before.contains_key(path) {
            panic!(
                "SAFETY VIOLATION [{}]: New file created: {}\n\
                 Preflight should prevent ANY file modifications!",
                context, path
            );
        }
    }

    // Check no files deleted (except any that might legitimately change like the db)
    for (path, old_content) in before {
        if let Some(new_content) = after.get(path) {
            // Allow database files to change (they track state)
            if path.ends_with(".db") || path.ends_with(".db-journal") || path.ends_with(".db-wal") {
                continue;
            }
            if old_content != new_content {
                panic!(
                    "SAFETY VIOLATION [{}]: File modified: {}\n\
                     Old size: {}, New size: {}\n\
                     Preflight should prevent ANY file modifications!",
                    context,
                    path,
                    old_content.len(),
                    new_content.len()
                );
            }
        }
    }
}

// ============================================================================
// Helper: Create a basic beads workspace
// ============================================================================

fn setup_workspace_with_issues() -> BrWorkspace {
    let workspace = BrWorkspace::new();

    // Initialize beads
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create a few issues for export
    let _ = run_br(&workspace, ["create", "Test issue 1", "-t", "task"], "create1");
    let _ = run_br(&workspace, ["create", "Test issue 2", "-t", "bug"], "create2");

    // Export to JSONL
    let export = run_br(&workspace, ["sync", "--flush-only"], "export");
    assert!(export.status.success(), "export failed: {}", export.stderr);

    workspace
}

// ============================================================================
// CONFLICT MARKER TESTS (Import Preflight)
// ============================================================================

/// Test: Import preflight rejects JSONL containing conflict markers
#[test]
fn preflight_import_rejects_conflict_markers() {
    let workspace = setup_workspace_with_issues();
    let beads_dir = workspace.root.join(".beads");
    let jsonl_path = beads_dir.join("issues.jsonl");

    // Snapshot before modification
    let snapshot_before = snapshot_directory(&workspace.root);

    // Inject conflict markers into JSONL
    let original = fs::read_to_string(&jsonl_path).expect("read jsonl");
    let mut file = fs::File::create(&jsonl_path).expect("create jsonl");
    writeln!(file, "<<<<<<< HEAD").unwrap();
    write!(file, "{}", original).unwrap();
    writeln!(file, "=======").unwrap();
    writeln!(file, r#"{{"id":"bd-conflict","title":"Conflict version"}}"#).unwrap();
    writeln!(file, ">>>>>>> feature-branch").unwrap();

    // Run preflight - should fail
    let config = ImportConfig {
        beads_dir: Some(beads_dir.clone()),
        ..Default::default()
    };

    let preflight_result = preflight_import(&jsonl_path, &config).expect("preflight should run");

    // Log for postmortem
    let log = format!(
        "=== CONFLICT MARKER PREFLIGHT TEST ===\n\
         JSONL path: {}\n\n\
         Preflight status: {:?}\n\
         Checks:\n{}\n",
        jsonl_path.display(),
        preflight_result.overall_status,
        preflight_result
            .checks
            .iter()
            .map(|c| format!(
                "  - {} [{:?}]: {}\n    Remediation: {:?}",
                c.name, c.status, c.message, c.remediation
            ))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let log_path = workspace.log_dir.join("preflight_conflict_marker.log");
    fs::write(&log_path, &log).expect("write log");

    // ASSERTION: Preflight should fail
    assert_eq!(
        preflight_result.overall_status,
        PreflightCheckStatus::Fail,
        "SAFETY: Preflight should FAIL when conflict markers are present.\n\
         Log: {}",
        log_path.display()
    );

    // ASSERTION: Failure should be about conflict markers
    let failures = preflight_result.failures();
    let conflict_failure = failures
        .iter()
        .find(|c| c.name == "no_conflict_markers");
    assert!(
        conflict_failure.is_some(),
        "Preflight should fail on 'no_conflict_markers' check.\nFailures: {:?}",
        failures
    );

    // ASSERTION: Remediation should mention resolving conflicts
    let check = conflict_failure.unwrap();
    assert!(
        check.remediation.as_ref().map_or(false, |r| r.to_lowercase().contains("resolve")),
        "Remediation should mention resolving conflicts. Got: {:?}",
        check.remediation
    );

    // ASSERTION: No files should be modified (except the JSONL we intentionally changed)
    // Since we modified the JSONL ourselves, we only check that no OTHER files changed
    let snapshot_after = snapshot_directory(&workspace.root);
    for (path, old_content) in &snapshot_before {
        // Skip the JSONL we intentionally modified
        if path.ends_with("issues.jsonl") {
            continue;
        }
        // Skip database files (allowed to track state)
        if path.ends_with(".db") || path.ends_with(".db-journal") || path.ends_with(".db-wal") {
            continue;
        }
        if let Some(new_content) = snapshot_after.get(path) {
            assert_eq!(
                old_content, new_content,
                "SAFETY VIOLATION: File {} was modified during preflight!",
                path
            );
        }
    }

    eprintln!("✓ Preflight correctly rejected conflict markers");
}

/// Test: Import preflight provides actionable error for conflict markers
#[test]
fn preflight_import_conflict_markers_shows_line_numbers() {
    let workspace = setup_workspace_with_issues();
    let beads_dir = workspace.root.join(".beads");
    let jsonl_path = beads_dir.join("issues.jsonl");

    // Create JSONL with conflict markers at known lines
    let mut file = fs::File::create(&jsonl_path).expect("create jsonl");
    writeln!(file, r#"{{"id":"bd-1","title":"Issue 1","status":"open","priority":2,"issue_type":"task","created_at":"2024-01-01T00:00:00Z","updated_at":"2024-01-01T00:00:00Z","labels":[],"dependencies":[],"comments":[]}}"#).unwrap();
    writeln!(file, "<<<<<<< HEAD").unwrap(); // Line 2
    writeln!(file, r#"{{"id":"bd-2","title":"Issue 2"}}"#).unwrap();
    writeln!(file, "=======").unwrap(); // Line 4
    writeln!(file, r#"{{"id":"bd-2","title":"Modified Issue 2"}}"#).unwrap();
    writeln!(file, ">>>>>>> branch").unwrap(); // Line 6

    let config = ImportConfig {
        beads_dir: Some(beads_dir),
        ..Default::default()
    };

    let result = preflight_import(&jsonl_path, &config).expect("preflight should run");

    // ASSERTION: Should fail
    assert_eq!(result.overall_status, PreflightCheckStatus::Fail);

    // ASSERTION: Error message should mention line numbers or markers
    let failures = result.failures();
    let conflict_check = failures
        .iter()
        .find(|c| c.name == "no_conflict_markers")
        .expect("Should have conflict marker failure");

    assert!(
        conflict_check.message.contains("line") || conflict_check.message.contains("marker"),
        "Error message should be actionable with line info. Got: {}",
        conflict_check.message
    );

    eprintln!("✓ Preflight shows actionable conflict marker info");
}

// ============================================================================
// UNSAFE PATH TESTS (Import Preflight)
// ============================================================================

/// Test: Import preflight rejects paths outside .beads directory
#[test]
fn preflight_import_rejects_outside_beads_dir() {
    let workspace = setup_workspace_with_issues();
    let beads_dir = workspace.root.join(".beads");

    // Try to import from outside .beads/
    let outside_path = workspace.root.join("malicious.jsonl");
    fs::write(
        &outside_path,
        r#"{"id":"bd-1","title":"Test","status":"open","priority":2,"issue_type":"task","created_at":"2024-01-01T00:00:00Z","updated_at":"2024-01-01T00:00:00Z","labels":[],"dependencies":[],"comments":[]}"#,
    )
    .expect("write test file");

    let config = ImportConfig {
        beads_dir: Some(beads_dir.clone()),
        allow_external_jsonl: false,
        ..Default::default()
    };

    let result = preflight_import(&outside_path, &config).expect("preflight should run");

    // Log for postmortem
    let log = format!(
        "=== OUTSIDE BEADS DIR PREFLIGHT TEST ===\n\
         Path: {}\n\
         Beads dir: {}\n\n\
         Preflight status: {:?}\n\
         Checks:\n{}\n",
        outside_path.display(),
        beads_dir.display(),
        result.overall_status,
        result
            .checks
            .iter()
            .map(|c| format!("  - {} [{:?}]: {}", c.name, c.status, c.message))
            .collect::<Vec<_>>()
            .join("\n")
    );
    let log_path = workspace.log_dir.join("preflight_outside_beads.log");
    fs::write(&log_path, &log).expect("write log");

    // ASSERTION: Preflight should fail
    assert_eq!(
        result.overall_status,
        PreflightCheckStatus::Fail,
        "SAFETY: Preflight should FAIL for paths outside .beads/.\n\
         Log: {}",
        log_path.display()
    );

    // ASSERTION: Failure should be about path validation
    let failures = result.failures();
    let path_failure = failures.iter().find(|c| c.name == "path_validation");
    assert!(
        path_failure.is_some(),
        "Preflight should fail on 'path_validation' check.\nFailures: {:?}",
        failures
    );

    eprintln!("✓ Preflight correctly rejected path outside .beads/");
}

/// Test: Import preflight rejects .git paths even with allow_external
#[test]
fn preflight_import_rejects_git_paths() {
    let workspace = setup_workspace_with_issues();
    let beads_dir = workspace.root.join(".beads");

    // Create a .git directory with a malicious file
    let git_dir = workspace.root.join(".git");
    fs::create_dir_all(&git_dir).expect("create .git");
    let git_path = git_dir.join("config.jsonl");
    fs::write(
        &git_path,
        r#"{"id":"bd-1","title":"Test","status":"open","priority":2,"issue_type":"task","created_at":"2024-01-01T00:00:00Z","updated_at":"2024-01-01T00:00:00Z","labels":[],"dependencies":[],"comments":[]}"#,
    )
    .expect("write test file");

    // Even with allow_external, .git paths should be rejected
    let config = ImportConfig {
        beads_dir: Some(beads_dir.clone()),
        allow_external_jsonl: true, // Even with this flag!
        ..Default::default()
    };

    let result = preflight_import(&git_path, &config).expect("preflight should run");

    // ASSERTION: Preflight should fail
    assert_eq!(
        result.overall_status,
        PreflightCheckStatus::Fail,
        "CRITICAL SAFETY: Preflight should ALWAYS reject .git paths!"
    );

    // ASSERTION: Error should mention git
    let failures = result.failures();
    let path_failure = failures.iter().find(|c| c.name == "path_validation");
    assert!(
        path_failure.is_some(),
        "Preflight should fail on path validation for .git paths"
    );
    let path_check = path_failure.unwrap();
    assert!(
        path_check.message.to_lowercase().contains("git"),
        "Error should mention git. Got: {}",
        path_check.message
    );

    eprintln!("✓ Preflight correctly rejected .git path");
}

/// Test: Import preflight rejects path traversal attempts
#[test]
fn preflight_import_rejects_path_traversal() {
    let workspace = setup_workspace_with_issues();
    let beads_dir = workspace.root.join(".beads");

    // Create a file outside .beads using traversal
    let parent = workspace.root.parent().unwrap();
    let traversal_target = parent.join("traversal_test.jsonl");
    fs::write(
        &traversal_target,
        r#"{"id":"bd-1","title":"Test","status":"open","priority":2,"issue_type":"task","created_at":"2024-01-01T00:00:00Z","updated_at":"2024-01-01T00:00:00Z","labels":[],"dependencies":[],"comments":[]}"#,
    )
    .expect("write test file");

    // Try to access it via traversal path
    let traversal_path = beads_dir.join("..").join("..").join("traversal_test.jsonl");

    let config = ImportConfig {
        beads_dir: Some(beads_dir),
        allow_external_jsonl: false,
        ..Default::default()
    };

    let result = preflight_import(&traversal_path, &config).expect("preflight should run");

    // ASSERTION: Preflight should fail
    assert_eq!(
        result.overall_status,
        PreflightCheckStatus::Fail,
        "SAFETY: Preflight should reject path traversal attempts"
    );

    // Cleanup
    let _ = fs::remove_file(&traversal_target);

    eprintln!("✓ Preflight correctly rejected path traversal");
}

// ============================================================================
// EXPORT PREFLIGHT TESTS
// ============================================================================

/// Test: Export preflight rejects export to .git path
#[test]
fn preflight_export_rejects_git_paths() {
    let workspace = setup_workspace_with_issues();
    let beads_dir = workspace.root.join(".beads");
    let db_path = beads_dir.join("beads.db");

    // Create a .git directory
    let git_dir = workspace.root.join(".git");
    fs::create_dir_all(&git_dir).expect("create .git");
    let git_output = git_dir.join("issues.jsonl");

    let storage = SqliteStorage::open(&db_path).expect("open db");
    let config = ExportConfig {
        beads_dir: Some(beads_dir),
        allow_external_jsonl: true, // Even with this flag!
        ..Default::default()
    };

    let result = preflight_export(&storage, &git_output, &config).expect("preflight should run");

    // ASSERTION: Preflight should fail
    assert_eq!(
        result.overall_status,
        PreflightCheckStatus::Fail,
        "CRITICAL SAFETY: Export preflight should ALWAYS reject .git paths!"
    );

    eprintln!("✓ Export preflight correctly rejected .git path");
}

/// Test: Export preflight warns about empty database over non-empty JSONL
#[test]
fn preflight_export_warns_empty_db_over_nonempty_jsonl() {
    let workspace = setup_workspace_with_issues();
    let beads_dir = workspace.root.join(".beads");
    let jsonl_path = beads_dir.join("issues.jsonl");
    let db_path = beads_dir.join("beads_test_empty.db");

    // Create an empty database
    let storage = SqliteStorage::open(&db_path).expect("open empty db");

    let config = ExportConfig {
        beads_dir: Some(beads_dir),
        force: false, // No force - should fail on empty db
        ..Default::default()
    };

    let result = preflight_export(&storage, &jsonl_path, &config).expect("preflight should run");

    // ASSERTION: Preflight should fail (would lose data)
    assert_eq!(
        result.overall_status,
        PreflightCheckStatus::Fail,
        "Preflight should prevent exporting empty db over non-empty JSONL"
    );

    // ASSERTION: Should mention data loss
    let failures = result.failures();
    let safety_failure = failures.iter().find(|c| {
        c.name.contains("empty") || c.name.contains("safety") || c.name.contains("data")
    });
    assert!(
        safety_failure.is_some(),
        "Preflight should fail on empty database safety check"
    );

    eprintln!("✓ Export preflight correctly prevented potential data loss");
}

// ============================================================================
// LOGGING AND OBSERVABILITY TESTS
// ============================================================================

/// Test: Preflight result includes all check names and actionable messages
#[test]
fn preflight_results_are_actionable() {
    let workspace = setup_workspace_with_issues();
    let beads_dir = workspace.root.join(".beads");
    let jsonl_path = beads_dir.join("issues.jsonl");

    let config = ImportConfig {
        beads_dir: Some(beads_dir),
        ..Default::default()
    };

    let result = preflight_import(&jsonl_path, &config).expect("preflight should run");

    // ASSERTION: All checks should have names
    for check in &result.checks {
        assert!(!check.name.is_empty(), "Check should have a name");
        assert!(
            !check.description.is_empty(),
            "Check {} should have a description",
            check.name
        );
        assert!(
            !check.message.is_empty(),
            "Check {} should have a message",
            check.name
        );
    }

    // ASSERTION: Failed checks should have remediation
    for failure in result.failures() {
        assert!(
            failure.remediation.is_some(),
            "Failed check '{}' should have remediation hint",
            failure.name
        );
    }

    // ASSERTION: into_result() produces readable error
    if result.overall_status == PreflightCheckStatus::Fail {
        let err = result.clone().into_result().unwrap_err();
        let err_str = err.to_string();
        assert!(
            err_str.contains("Preflight"),
            "Error should mention preflight"
        );
        for failure in result.failures() {
            assert!(
                err_str.contains(&failure.name),
                "Error should include check name: {}",
                failure.name
            );
        }
    }

    eprintln!("✓ Preflight results are actionable and observable");
}

/// Test: CLI import uses preflight and shows clear error
#[test]
fn cli_import_shows_preflight_failure() {
    let workspace = setup_workspace_with_issues();
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");

    // Inject conflict markers
    let original = fs::read_to_string(&jsonl_path).expect("read jsonl");
    let modified = format!("<<<<<<< HEAD\n{}\n=======\n>>>>>>> branch\n", original);
    fs::write(&jsonl_path, &modified).expect("write modified jsonl");

    // Try CLI import - should fail with clear error
    let import = run_br(&workspace, ["sync", "--import-only", "--force"], "import_preflight");

    // ASSERTION: Should fail
    assert!(
        !import.status.success(),
        "CLI import should fail when preflight detects issues"
    );

    // ASSERTION: Error should mention conflict markers
    let stderr_lower = import.stderr.to_lowercase();
    assert!(
        stderr_lower.contains("conflict") || stderr_lower.contains("marker") || stderr_lower.contains("<<<<"),
        "CLI error should mention conflict markers. Got: {}",
        import.stderr
    );

    eprintln!("✓ CLI import shows preflight failure clearly");
}
