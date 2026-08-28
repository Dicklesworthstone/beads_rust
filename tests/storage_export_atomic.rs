//! Storage-level tests for atomic export pipeline.
//!
//! Tests edge cases and invariants not covered by the e2e failure injection tests:
//! - Concurrent exports to the same path produce valid output
//! - Export rejects path traversal at runtime (not just preflight)
//! - Export to path outside beads_dir is rejected with beads_dir set
//! - Temp file cleanup on failure (no orphaned .tmp files)
//! - Re-export after successful export overwrites cleanly
//! - Export content is valid JSONL on every line
//! - Successive exports are idempotent (same content → same hash)
//!
//! Related bead: beads_rust-3hls

#![allow(
    clippy::too_many_lines,
    clippy::needless_collect,
    clippy::uninlined_format_args
)]

mod common;

use beads_rust::model::Issue;
use beads_rust::storage::SqliteStorage;
use beads_rust::sync::{ExportConfig, export_to_jsonl};
use sha2::{Digest, Sha256};
use std::fs;
#[cfg(unix)]
use std::fs::Permissions;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn create_test_issue(id: &str, title: &str) -> Issue {
    let mut issue = common::fixtures::issue(title);
    issue.id = id.to_string();
    issue
}

fn export_temp_path_for_test(output_path: &Path) -> std::path::PathBuf {
    export_temp_path_attempt_for_test(output_path, 0)
}

fn export_temp_path_attempt_for_test(output_path: &Path, attempt: u32) -> std::path::PathBuf {
    let pid = std::process::id();
    if attempt == 0 {
        return output_path.with_extension(format!("jsonl.{pid}.tmp"));
    }

    let retry_suffix = u64::from(pid)
        .saturating_mul(100)
        .saturating_add(u64::from(attempt));
    output_path.with_extension(format!("jsonl.{retry_suffix}.tmp"))
}

fn setup_storage_with_issues(count: usize) -> SqliteStorage {
    let mut storage = SqliteStorage::open_memory().unwrap();
    for i in 0..count {
        let issue = create_test_issue(&format!("test-{i:04}"), &format!("Issue {i}"));
        storage.create_issue(&issue, "tester").unwrap();
    }
    storage
}

/// A temp directory under the canonical system temp root: br routes sync
/// through canonical workspace paths, so the pinned no-follow JSONL traversal
/// never meets a symlinked ancestor (macOS `/var` -> `/private/var`).
fn canonical_temp_dir() -> TempDir {
    let root = dunce::canonicalize(std::env::temp_dir()).unwrap();
    TempDir::new_in(root).unwrap()
}

fn setup_beads_dir(temp: &TempDir) -> std::path::PathBuf {
    let beads_dir = temp.path().join(".beads");
    fs::create_dir_all(&beads_dir).unwrap();
    beads_dir
}

fn compute_file_hash(path: &Path) -> String {
    let content = fs::read(path).unwrap();
    beads_rust::util::hex_encode(&Sha256::digest(&content))
}

fn default_config(beads_dir: &Path) -> ExportConfig {
    ExportConfig {
        beads_dir: Some(beads_dir.to_path_buf()),
        ..Default::default()
    }
}

// ===========================================================================
// 1. Concurrent exports produce valid output (no corruption)
// ===========================================================================

#[test]
fn concurrent_exports_no_corruption() {
    let _log = common::test_log("concurrent_exports_no_corruption");

    // Two separate storages writing to two separate paths (concurrent same-path
    // needs file-level locking which is OS-dependent; here we verify independent
    // concurrent exports don't interfere with each other).
    let temp = canonical_temp_dir();
    let beads_dir = setup_beads_dir(&temp);

    // Export all concurrently using threads - create storage inside each thread
    // since fsqlite Connection is not Send (uses Rc internally)
    let handles: Vec<_> = (0..4)
        .map(|i| {
            let path = beads_dir.join(format!("issues_{i}.jsonl"));
            let bd = beads_dir.clone();
            std::thread::spawn(move || {
                let mut storage = SqliteStorage::open_memory().unwrap();
                for j in 0..5 {
                    let issue = create_test_issue(
                        &format!("t{i}-{j:03}"),
                        &format!("Thread {i} Issue {j}"),
                    );
                    storage.create_issue(&issue, "tester").unwrap();
                }
                let config = ExportConfig {
                    beads_dir: Some(bd),
                    ..Default::default()
                };
                let result = export_to_jsonl(&storage, &path, &config);
                (path, result)
            })
        })
        .collect();

    for handle in handles {
        let (path, result) = handle.join().unwrap();
        let export = result.expect("export should succeed");
        assert_eq!(export.exported_count, 5, "Each export should have 5 issues");

        // Verify the output is valid JSONL
        let content = fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = content.lines().collect();
        assert_eq!(lines.len(), 5);
        for line in &lines {
            let _: serde_json::Value =
                serde_json::from_str(line).expect("line should be valid JSON");
        }
    }
}

// ===========================================================================
// 2. Export rejects path traversal at runtime
// ===========================================================================

#[test]
fn export_rejects_path_traversal() {
    let _log = common::test_log("export_rejects_path_traversal");

    let storage = setup_storage_with_issues(1);
    let temp = canonical_temp_dir();
    let beads_dir = setup_beads_dir(&temp);

    // Attempt to export to a path outside beads_dir via traversal
    let evil_path = beads_dir.join("..").join("escaped.jsonl");
    let config = default_config(&beads_dir);

    let result = export_to_jsonl(&storage, &evil_path, &config);
    assert!(
        result.is_err(),
        "Export to traversal path should fail at runtime"
    );

    // The escaped file should NOT exist
    let escaped = temp.path().join("escaped.jsonl");
    assert!(
        !escaped.exists(),
        "Traversal path should not create escaped file"
    );
}

// ===========================================================================
// 3. Export rejects path outside beads_dir
// ===========================================================================

#[test]
fn export_rejects_outside_beads_dir() {
    let _log = common::test_log("export_rejects_outside_beads_dir");

    let storage = setup_storage_with_issues(1);
    let temp = canonical_temp_dir();
    let beads_dir = setup_beads_dir(&temp);

    // Path directly outside .beads/
    let outside_path = temp.path().join("outside.jsonl");
    let config = default_config(&beads_dir);

    let result = export_to_jsonl(&storage, &outside_path, &config);
    assert!(
        result.is_err(),
        "Export to path outside beads_dir should be rejected"
    );
    assert!(
        !outside_path.exists(),
        "File outside beads_dir should not be created"
    );
}

// ===========================================================================
// 4. Export rejects .git path at runtime
// ===========================================================================

#[test]
fn export_rejects_git_path() {
    let _log = common::test_log("export_rejects_git_path");

    let storage = setup_storage_with_issues(1);
    let temp = canonical_temp_dir();
    let beads_dir = setup_beads_dir(&temp);

    // Create a .git dir inside beads to test rejection
    let git_dir = beads_dir.join(".git");
    fs::create_dir_all(&git_dir).unwrap();
    let git_path = git_dir.join("issues.jsonl");

    let config = default_config(&beads_dir);
    let result = export_to_jsonl(&storage, &git_path, &config);

    assert!(
        result.is_err(),
        "Export to .git path should be rejected at runtime"
    );
    assert!(
        !git_path.exists(),
        ".git path should not be written to during export"
    );
}

// ===========================================================================
// 5. Temp file does not remain after failure
// ===========================================================================

#[test]
#[cfg(unix)]
fn temp_file_cleaned_up_on_failure() {
    let _log = common::test_log("temp_file_cleaned_up_on_failure");

    let storage = setup_storage_with_issues(3);
    let temp = canonical_temp_dir();
    let beads_dir = setup_beads_dir(&temp);
    let jsonl_path = beads_dir.join("issues.jsonl");
    let temp_path = export_temp_path_for_test(&jsonl_path);

    // Create initial JSONL
    fs::write(&jsonl_path, r#"{"id":"old","title":"Old"}"#).unwrap();

    // Make directory read-only to force failure
    fs::set_permissions(&beads_dir, Permissions::from_mode(0o555)).unwrap();

    let config = default_config(&beads_dir);
    let result = export_to_jsonl(&storage, &jsonl_path, &config);

    // Restore permissions
    fs::set_permissions(&beads_dir, Permissions::from_mode(0o755)).unwrap();

    assert!(result.is_err(), "Export should fail on read-only dir");

    // Temp file should NOT remain
    assert!(
        !temp_path.exists(),
        "Temp file should not remain after failed export"
    );
}

// ===========================================================================
// 6. Re-export overwrites previous output cleanly
// ===========================================================================

#[test]
fn re_export_overwrites_cleanly() {
    let _log = common::test_log("re_export_overwrites_cleanly");

    let temp = canonical_temp_dir();
    let beads_dir = setup_beads_dir(&temp);
    let jsonl_path = beads_dir.join("issues.jsonl");

    // First export with 3 issues
    let mut storage = SqliteStorage::open_memory().unwrap();
    for i in 0..3 {
        let issue = create_test_issue(&format!("test-{i:03}"), &format!("Issue {i}"));
        storage.create_issue(&issue, "tester").unwrap();
    }

    let config = default_config(&beads_dir);
    let r1 = export_to_jsonl(&storage, &jsonl_path, &config).unwrap();
    assert_eq!(r1.exported_count, 3);

    let hash1 = compute_file_hash(&jsonl_path);

    // Add more issues and re-export
    for i in 3..6 {
        let issue = create_test_issue(&format!("test-{i:03}"), &format!("Issue {i}"));
        storage.create_issue(&issue, "tester").unwrap();
    }

    let r2 = export_to_jsonl(&storage, &jsonl_path, &config).unwrap();
    assert_eq!(r2.exported_count, 6);

    let hash2 = compute_file_hash(&jsonl_path);
    assert_ne!(hash1, hash2, "Re-export with new issues should change hash");

    // Verify new content is valid
    let content = fs::read_to_string(&jsonl_path).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 6, "Re-export should contain all 6 issues");
}

// ===========================================================================
// 7. Successive identical exports are idempotent
// ===========================================================================

#[test]
fn successive_exports_idempotent() {
    let _log = common::test_log("successive_exports_idempotent");

    let storage = setup_storage_with_issues(5);
    let temp = canonical_temp_dir();
    let beads_dir = setup_beads_dir(&temp);
    let jsonl_path = beads_dir.join("issues.jsonl");
    let config = default_config(&beads_dir);

    // Export three times
    let r1 = export_to_jsonl(&storage, &jsonl_path, &config).unwrap();
    let hash1 = compute_file_hash(&jsonl_path);

    let r2 = export_to_jsonl(&storage, &jsonl_path, &config).unwrap();
    let hash2 = compute_file_hash(&jsonl_path);

    let r3 = export_to_jsonl(&storage, &jsonl_path, &config).unwrap();
    let hash3 = compute_file_hash(&jsonl_path);

    // All hashes should be identical
    assert_eq!(hash1, hash2, "Successive exports should produce same hash");
    assert_eq!(hash2, hash3, "Third export should match second");

    // All counts identical
    assert_eq!(r1.exported_count, r2.exported_count);
    assert_eq!(r2.exported_count, r3.exported_count);

    // Content hashes from export result should match
    assert_eq!(
        r1.content_hash, r2.content_hash,
        "Export result content_hash should be deterministic"
    );
}

// ===========================================================================
// 8. Export validates every line is valid JSON
// ===========================================================================

#[test]
fn export_produces_valid_jsonl_per_line() {
    let _log = common::test_log("export_produces_valid_jsonl_per_line");

    let storage = setup_storage_with_issues(20);
    let temp = canonical_temp_dir();
    let beads_dir = setup_beads_dir(&temp);
    let jsonl_path = beads_dir.join("issues.jsonl");
    let config = default_config(&beads_dir);

    export_to_jsonl(&storage, &jsonl_path, &config).unwrap();

    let content = fs::read_to_string(&jsonl_path).unwrap();
    let lines: Vec<&str> = content.lines().collect();
    assert_eq!(lines.len(), 20);

    for (i, line) in lines.iter().enumerate() {
        // Each line must be valid JSON
        let parsed: serde_json::Value =
            serde_json::from_str(line).expect("line should be valid JSON");

        // Must have id field
        assert!(parsed.get("id").is_some(), "Line {i} missing 'id' field");

        // Must have title field
        assert!(
            parsed.get("title").is_some(),
            "Line {i} missing 'title' field"
        );

        // Must have status field
        assert!(
            parsed.get("status").is_some(),
            "Line {i} missing 'status' field"
        );
    }
}

// ===========================================================================
// 9. Symlink escape in export path is rejected
// ===========================================================================

#[test]
#[cfg(unix)]
fn export_rejects_symlink_escape() {
    let _log = common::test_log("export_rejects_symlink_escape");

    let storage = setup_storage_with_issues(1);
    let temp = canonical_temp_dir();
    let beads_dir = setup_beads_dir(&temp);

    // Create an outside directory
    let outside = TempDir::new().unwrap();

    // Create a symlink inside beads_dir pointing outside
    let symlink_path = beads_dir.join("escape_link");
    std::os::unix::fs::symlink(outside.path(), &symlink_path).unwrap();

    let target = symlink_path.join("issues.jsonl");
    let config = default_config(&beads_dir);

    let result = export_to_jsonl(&storage, &target, &config);
    assert!(
        result.is_err(),
        "Export through symlink escape should be rejected"
    );

    // Nothing should be written outside
    let escaped_file = outside.path().join("issues.jsonl");
    assert!(
        !escaped_file.exists(),
        "No file should be written outside beads_dir via symlink"
    );
}

#[test]
#[cfg(unix)]
fn export_rejects_existing_temp_symlink_and_preserves_live_jsonl() {
    let _log = common::test_log("export_rejects_existing_temp_symlink_and_preserves_live_jsonl");

    let storage = setup_storage_with_issues(1);
    let temp = canonical_temp_dir();
    let beads_dir = setup_beads_dir(&temp);
    let jsonl_path = beads_dir.join("issues.jsonl");
    let temp_path = export_temp_path_for_test(&jsonl_path);
    fs::write(&jsonl_path, "{\"id\":\"old\",\"title\":\"Old\"}\n").unwrap();

    let outside = TempDir::new().unwrap();
    std::os::unix::fs::symlink(outside.path().join("captured.jsonl"), &temp_path).unwrap();

    let config = default_config(&beads_dir);
    let result = export_to_jsonl(&storage, &jsonl_path, &config);
    assert!(
        result.is_err(),
        "Export should reject an existing temp symlink"
    );

    assert_eq!(
        fs::read_to_string(&jsonl_path).unwrap(),
        "{\"id\":\"old\",\"title\":\"Old\"}\n",
        "Live JSONL should remain untouched when staged export is rejected"
    );
    assert!(
        !outside.path().join("captured.jsonl").exists(),
        "Temp symlink should not receive exported data"
    );
}

#[test]
fn export_skips_stale_regular_temp_file_and_preserves_it() {
    let _log = common::test_log("export_skips_stale_regular_temp_file_and_preserves_it");

    let storage = setup_storage_with_issues(1);
    let temp = canonical_temp_dir();
    let beads_dir = setup_beads_dir(&temp);
    let jsonl_path = beads_dir.join("issues.jsonl");
    let stale_temp_path = export_temp_path_for_test(&jsonl_path);
    let retry_temp_path = export_temp_path_attempt_for_test(&jsonl_path, 1);
    fs::write(&stale_temp_path, "stale temp").unwrap();

    let config = default_config(&beads_dir);
    let result = export_to_jsonl(&storage, &jsonl_path, &config).unwrap();

    assert_eq!(result.exported_count, 1);
    assert!(
        fs::read_to_string(&jsonl_path)
            .unwrap()
            .contains("\"id\":\"test-0000\""),
        "export should succeed through a retry temp path"
    );
    assert_eq!(
        fs::read_to_string(&stale_temp_path).unwrap(),
        "stale temp",
        "regular stale temp collision should be left untouched"
    );
    assert!(
        !retry_temp_path.exists(),
        "successful rename should not leave the retry temp file behind"
    );
}

// ===========================================================================
// 10. Export with allow_external_jsonl to outside path works
// ===========================================================================

#[test]
fn export_external_path_with_flag() {
    let _log = common::test_log("export_external_path_with_flag");

    let storage = setup_storage_with_issues(3);
    let temp = canonical_temp_dir();
    let beads_dir = setup_beads_dir(&temp);

    // External path outside .beads/
    let external_path = temp.path().join("external_export.jsonl");

    let config = ExportConfig {
        beads_dir: Some(beads_dir),
        allow_external_jsonl: true,
        ..Default::default()
    };

    let result = export_to_jsonl(&storage, &external_path, &config);
    assert!(
        result.is_ok(),
        "External export with flag should succeed: {:?}",
        result.err()
    );

    // File should exist and contain 3 issues
    assert!(external_path.exists(), "External file should be created");
    let content = fs::read_to_string(&external_path).unwrap();
    assert_eq!(
        content.lines().count(),
        3,
        "External export should contain all issues"
    );
}

// ===========================================================================
// 11. Export with allow_external_jsonl still rejects .git paths
// ===========================================================================

#[test]
fn export_external_still_rejects_git() {
    let _log = common::test_log("export_external_still_rejects_git");

    let storage = setup_storage_with_issues(1);
    let temp = canonical_temp_dir();
    let beads_dir = setup_beads_dir(&temp);

    // Create .git directory and target within it
    let git_dir = temp.path().join(".git");
    fs::create_dir_all(&git_dir).unwrap();
    let git_path = git_dir.join("issues.jsonl");

    let config = ExportConfig {
        beads_dir: Some(beads_dir),
        allow_external_jsonl: true,
        ..Default::default()
    };

    let result = export_to_jsonl(&storage, &git_path, &config);
    assert!(
        result.is_err(),
        "External export to .git path should still be rejected"
    );
    assert!(!git_path.exists(), ".git file should not be created");
}

// ===========================================================================
// 12. Export count matches re-read count (integrity verification)
// ===========================================================================

#[test]
fn export_count_matches_file_line_count() {
    let _log = common::test_log("export_count_matches_file_line_count");

    let storage = setup_storage_with_issues(50);
    let temp = canonical_temp_dir();
    let beads_dir = setup_beads_dir(&temp);
    let jsonl_path = beads_dir.join("issues.jsonl");
    let config = default_config(&beads_dir);

    let result = export_to_jsonl(&storage, &jsonl_path, &config).unwrap();

    // exported_count should match actual line count
    let content = fs::read_to_string(&jsonl_path).unwrap();
    let actual_lines = content.lines().count();
    assert_eq!(
        result.exported_count, actual_lines,
        "Export count should match actual JSONL line count"
    );

    // Also verify issue IDs are unique
    let ids: std::collections::HashSet<String> = content
        .lines()
        .map(|line| {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            v["id"].as_str().unwrap().to_string()
        })
        .collect();
    assert_eq!(
        ids.len(),
        actual_lines,
        "All exported issue IDs should be unique"
    );
}
