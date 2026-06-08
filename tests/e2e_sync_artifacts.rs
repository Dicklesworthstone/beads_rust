//! E2E sync tests with detailed logging and artifact preservation.
//!
//! These tests run br sync in temp repos, capture stdout/stderr/tracing logs,
//! and archive artifacts (before/after file tree snapshots, JSONL outputs).
//!
//! Design goals:
//! - Deterministic: no randomness, no network, seeded where needed
//! - CI-ready: clear pass/fail, meaningful error messages
//! - Artifact preservation: logs and snapshots for postmortem analysis
//!
//! Related beads:
//! - beads_rust-0v1.3.6: E2E sync test scripts with detailed logging and artifacts

#![allow(
    clippy::format_push_string,
    clippy::uninlined_format_args,
    clippy::redundant_clone,
    clippy::map_unwrap_or
)]

mod common;

use common::cli::{BrWorkspace, run_br};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Artifact collector for test diagnostics and postmortem analysis.
#[derive(Debug)]
struct TestArtifacts {
    /// Directory where all artifacts are stored
    artifact_dir: PathBuf,
    /// Test name for labeling
    test_name: String,
    /// Collected snapshots: label -> snapshot data
    snapshots: Vec<(String, DirectorySnapshot)>,
    /// JSONL captures: label -> content
    jsonl_captures: Vec<(String, String)>,
    /// Command logs: label -> log content
    command_logs: Vec<(String, String)>,
}

impl TestArtifacts {
    fn new(workspace: &BrWorkspace, test_name: &str) -> Self {
        let artifact_dir = workspace.log_dir.join("artifacts");
        fs::create_dir_all(&artifact_dir).expect("create artifact dir");
        Self {
            artifact_dir,
            test_name: test_name.to_string(),
            snapshots: Vec::new(),
            jsonl_captures: Vec::new(),
            command_logs: Vec::new(),
        }
    }

    /// Capture a directory snapshot at a labeled point in time.
    fn capture_snapshot(&mut self, label: &str, dir: &Path) {
        let snapshot = DirectorySnapshot::capture(dir);
        self.snapshots.push((label.to_string(), snapshot));
    }

    /// Capture JSONL file content at a labeled point.
    fn capture_jsonl(&mut self, label: &str, path: &Path) {
        let content = if path.exists() {
            fs::read_to_string(path).unwrap_or_else(|e| format!("<error reading: {e}>"))
        } else {
            "<file does not exist>".to_string()
        };
        self.jsonl_captures.push((label.to_string(), content));
    }

    /// Record a command log.
    fn record_command(&mut self, label: &str, stdout: &str, stderr: &str, success: bool) {
        let log = format!(
            "=== Command: {label} ===\nSuccess: {success}\n\n--- stdout ---\n{stdout}\n\n--- stderr ---\n{stderr}\n"
        );
        self.command_logs.push((label.to_string(), log));
    }

    /// Write all artifacts to disk for postmortem analysis.
    fn persist(&self) {
        // Write snapshot comparison
        let snapshot_path = self
            .artifact_dir
            .join(format!("{}_snapshots.txt", self.test_name));
        let mut snapshot_content = String::new();
        for (label, snapshot) in &self.snapshots {
            snapshot_content.push_str(&format!("\n=== Snapshot: {label} ===\n"));
            snapshot_content.push_str(&format!("Files: {}\n", snapshot.files.len()));
            for (path, hash) in &snapshot.files {
                snapshot_content.push_str(&format!("  {path}: {hash}\n"));
            }
        }
        fs::write(&snapshot_path, snapshot_content).expect("write snapshots");

        // Write JSONL captures
        for (label, content) in &self.jsonl_captures {
            let jsonl_path = self
                .artifact_dir
                .join(format!("{}_{}.jsonl", self.test_name, label));
            fs::write(&jsonl_path, content).expect("write jsonl capture");
        }

        // Write command logs
        let logs_path = self
            .artifact_dir
            .join(format!("{}_commands.log", self.test_name));
        let logs_content: String = self
            .command_logs
            .iter()
            .map(|(_, log)| log.as_str())
            .collect();
        fs::write(&logs_path, logs_content).expect("write command logs");
    }

    /// Compare two snapshots and return differences.
    fn diff_snapshots(&self, label_before: &str, label_after: &str) -> SnapshotDiff {
        let before = self
            .snapshots
            .iter()
            .find(|(l, _)| l == label_before)
            .map(|(_, s)| s);
        let after = self
            .snapshots
            .iter()
            .find(|(l, _)| l == label_after)
            .map(|(_, s)| s);

        match (before, after) {
            (Some(b), Some(a)) => b.diff(a),
            _ => SnapshotDiff::default(),
        }
    }
}

/// Snapshot of a directory's file tree with content hashes.
#[derive(Debug, Clone)]
struct DirectorySnapshot {
    /// Map of relative path -> SHA256 hash of content
    files: BTreeMap<String, String>,
}

impl DirectorySnapshot {
    fn capture(dir: &Path) -> Self {
        let mut files = BTreeMap::new();
        Self::visit_dir(dir, dir, &mut files);
        Self { files }
    }

    fn visit_dir(dir: &Path, base: &Path, files: &mut BTreeMap<String, String>) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let rel_path = path
                    .strip_prefix(base)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();

                if path.is_file() {
                    if let Ok(contents) = fs::read(&path) {
                        let mut digest = Sha256::new();
                        digest.update(&contents);
                        let hash = beads_rust::util::hex_encode(&digest.finalize());
                        files.insert(rel_path, hash);
                    }
                } else if path.is_dir() {
                    Self::visit_dir(&path, base, files);
                }
            }
        }
    }

    fn diff(&self, other: &Self) -> SnapshotDiff {
        let mut added = Vec::new();
        let mut removed = Vec::new();
        let mut modified = Vec::new();

        // Find added and modified files
        for (path, hash) in &other.files {
            match self.files.get(path) {
                None => added.push(path.clone()),
                Some(old_hash) if old_hash != hash => modified.push(path.clone()),
                _ => {}
            }
        }

        // Find removed files
        for path in self.files.keys() {
            if !other.files.contains_key(path) {
                removed.push(path.clone());
            }
        }

        SnapshotDiff {
            added,
            removed,
            modified,
        }
    }
}

/// Difference between two directory snapshots.
#[derive(Debug, Default)]
struct SnapshotDiff {
    added: Vec<String>,
    removed: Vec<String>,
    modified: Vec<String>,
}

impl SnapshotDiff {
    #[allow(dead_code)]
    fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.modified.is_empty()
    }

    /// Check if only .beads/ files were affected (excluding logs/ which are test artifacts).
    fn only_beads_affected(&self) -> bool {
        let all_changes: Vec<_> = self
            .added
            .iter()
            .chain(self.removed.iter())
            .chain(self.modified.iter())
            .collect();

        all_changes
            .iter()
            .all(|p| p.starts_with(".beads") || p.starts_with("logs"))
    }
}

// =============================================================================
// E2E SYNC TESTS WITH ARTIFACT PRESERVATION
// =============================================================================

/// E2E test: Basic export cycle with artifact capture.
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_sync_export_with_artifacts() {
    let _log = common::test_log("e2e_sync_export_with_artifacts");
    let workspace = BrWorkspace::new();
    let mut artifacts = TestArtifacts::new(&workspace, "sync_export");

    // Capture initial state
    artifacts.capture_snapshot("initial", &workspace.root);

    // Initialize beads
    let init = run_br(&workspace, ["init"], "init");
    artifacts.record_command("init", &init.stdout, &init.stderr, init.status.success());
    assert!(init.status.success(), "init failed: {}", init.stderr);

    artifacts.capture_snapshot("after_init", &workspace.root);

    // Create issues with various attributes
    let create1 = run_br(
        &workspace,
        [
            "create",
            "First issue",
            "-t",
            "task",
            "-p",
            "1",
            "--no-auto-flush",
        ],
        "create1",
    );
    artifacts.record_command(
        "create1",
        &create1.stdout,
        &create1.stderr,
        create1.status.success(),
    );
    assert!(
        create1.status.success(),
        "create1 failed: {}",
        create1.stderr
    );

    let create2 = run_br(
        &workspace,
        [
            "create",
            "Second issue",
            "-t",
            "bug",
            "-p",
            "2",
            "--no-auto-flush",
        ],
        "create2",
    );
    artifacts.record_command(
        "create2",
        &create2.stdout,
        &create2.stderr,
        create2.status.success(),
    );
    assert!(
        create2.status.success(),
        "create2 failed: {}",
        create2.stderr
    );

    artifacts.capture_snapshot("after_creates", &workspace.root);

    // Run sync export
    let sync = run_br(&workspace, ["sync", "--flush-only", "--manifest"], "export");
    artifacts.record_command("export", &sync.stdout, &sync.stderr, sync.status.success());
    assert!(sync.status.success(), "sync export failed: {}", sync.stderr);

    // Capture JSONL output
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    artifacts.capture_jsonl("after_export", &jsonl_path);

    artifacts.capture_snapshot("after_export", &workspace.root);

    // Verify JSONL was created
    assert!(jsonl_path.exists(), "JSONL file should exist after export");

    // Verify manifest was created
    let manifest_path = workspace.root.join(".beads").join(".manifest.json");
    if !manifest_path.exists() {
        eprintln!("Manifest missing! Contents of .beads:");
        for entry in fs::read_dir(workspace.root.join(".beads")).unwrap() {
            eprintln!("  {:?}", entry.unwrap().path());
        }
    }
    assert!(
        manifest_path.exists(),
        "Manifest file should exist after export with --manifest"
    );

    // Verify only .beads/ was affected (logs/ are test artifacts, not user files)
    let diff = artifacts.diff_snapshots("initial", "after_export");
    assert!(
        diff.only_beads_affected(),
        "Export should only affect .beads/ directory (and test logs/)\n\
         Added outside allowed: {:?}\n\
         Modified outside allowed: {:?}",
        diff.added
            .iter()
            .filter(|p| !p.starts_with(".beads") && !p.starts_with("logs"))
            .collect::<Vec<_>>(),
        diff.modified
            .iter()
            .filter(|p| !p.starts_with(".beads") && !p.starts_with("logs"))
            .collect::<Vec<_>>()
    );

    // Persist artifacts for postmortem
    artifacts.persist();

    eprintln!(
        "[PASS] e2e_sync_export_with_artifacts\n\
         - Artifacts saved to: {:?}\n\
         - JSONL size: {} bytes\n\
         - Files in .beads/: {}",
        artifacts.artifact_dir,
        fs::metadata(&jsonl_path).map(|m| m.len()).unwrap_or(0),
        artifacts
            .snapshots
            .last()
            .map(|(_, s)| s.files.len())
            .unwrap_or(0)
    );
}

/// E2E test: Import cycle with artifact capture.
#[test]
fn e2e_sync_import_with_artifacts() {
    let _log = common::test_log("e2e_sync_import_with_artifacts");
    let workspace = BrWorkspace::new();
    let mut artifacts = TestArtifacts::new(&workspace, "sync_import");

    // Initialize and create issues
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed");

    let create = run_br(
        &workspace,
        ["create", "Original issue", "--no-auto-flush"],
        "create",
    );
    assert!(create.status.success(), "create failed");

    // Export first
    let flush = run_br(&workspace, ["sync", "--flush-only"], "flush");
    assert!(flush.status.success(), "flush failed");

    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    artifacts.capture_jsonl("before_modification", &jsonl_path);
    artifacts.capture_snapshot("before_modification", &workspace.root);

    // Modify JSONL externally (simulate incoming changes)
    let original = fs::read_to_string(&jsonl_path).expect("read jsonl");
    let modified = original.replace("Original issue", "Modified via JSONL");
    fs::write(&jsonl_path, &modified).expect("write modified jsonl");

    artifacts.capture_jsonl("after_modification", &jsonl_path);
    artifacts.capture_snapshot("after_modification", &workspace.root);

    // Run sync import
    let import = run_br(&workspace, ["sync", "--import-only", "--force"], "import");
    artifacts.record_command(
        "import",
        &import.stdout,
        &import.stderr,
        import.status.success(),
    );
    assert!(
        import.status.success(),
        "sync import failed: {}",
        import.stderr
    );

    artifacts.capture_snapshot("after_import", &workspace.root);

    // Verify the title was updated in the database via list command
    let list = run_br(&workspace, ["list", "--json"], "list_verify");
    assert!(list.status.success(), "list failed");
    assert!(
        list.stdout.contains("Modified via JSONL"),
        "Import should have updated the issue title\n\
         stdout: {}",
        list.stdout
    );

    // Verify only .beads/ was affected (logs/ are test artifacts, not user files)
    let diff = artifacts.diff_snapshots("before_modification", "after_import");
    assert!(
        diff.only_beads_affected(),
        "Import should only affect .beads/ directory (and test logs/)\n\
         Added outside allowed: {:?}\n\
         Modified outside allowed: {:?}",
        diff.added
            .iter()
            .filter(|p| !p.starts_with(".beads") && !p.starts_with("logs"))
            .collect::<Vec<_>>(),
        diff.modified
            .iter()
            .filter(|p| !p.starts_with(".beads") && !p.starts_with("logs"))
            .collect::<Vec<_>>()
    );

    artifacts.persist();

    eprintln!(
        "[PASS] e2e_sync_import_with_artifacts\n\
         - Artifacts saved to: {:?}",
        artifacts.artifact_dir
    );
}

/// E2E test: Full sync cycle (export -> modify -> import -> export).
#[test]
fn e2e_sync_full_cycle_with_artifacts() {
    let _log = common::test_log("e2e_sync_full_cycle_with_artifacts");
    let workspace = BrWorkspace::new();
    let mut artifacts = TestArtifacts::new(&workspace, "sync_full_cycle");

    // Initialize
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed");
    artifacts.capture_snapshot("after_init", &workspace.root);

    // Create multiple issues
    for (i, (title, typ)) in [
        ("Bug: Login fails", "bug"),
        ("Feature: Dark mode", "feature"),
        ("Task: Update docs", "task"),
    ]
    .iter()
    .enumerate()
    {
        let create = run_br(
            &workspace,
            ["create", title, "-t", typ, "--no-auto-flush"],
            &format!("create{i}"),
        );
        artifacts.record_command(
            &format!("create{i}"),
            &create.stdout,
            &create.stderr,
            create.status.success(),
        );
        assert!(create.status.success(), "create{i} failed");
    }

    artifacts.capture_snapshot("after_creates", &workspace.root);

    // Phase 1: Export
    let export1 = run_br(&workspace, ["sync", "--flush-only"], "export1");
    artifacts.record_command(
        "export1",
        &export1.stdout,
        &export1.stderr,
        export1.status.success(),
    );
    assert!(export1.status.success(), "export1 failed");

    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    artifacts.capture_jsonl("phase1_export", &jsonl_path);
    artifacts.capture_snapshot("after_export1", &workspace.root);

    // Phase 2: External modification (simulate git pull with changes)
    let original = fs::read_to_string(&jsonl_path).expect("read jsonl");
    let modified = original.replace("Bug: Login fails", "Bug: Login fails (critical)");
    fs::write(&jsonl_path, &modified).expect("write modified");
    artifacts.capture_jsonl("phase2_modified", &jsonl_path);

    // Phase 3: Import
    let import = run_br(&workspace, ["sync", "--import-only", "--force"], "import");
    artifacts.record_command(
        "import",
        &import.stdout,
        &import.stderr,
        import.status.success(),
    );
    assert!(import.status.success(), "import failed");
    artifacts.capture_snapshot("after_import", &workspace.root);

    // Phase 4: Re-export
    let export2 = run_br(&workspace, ["sync", "--flush-only", "--force"], "export2");
    artifacts.record_command(
        "export2",
        &export2.stdout,
        &export2.stderr,
        export2.status.success(),
    );
    assert!(export2.status.success(), "export2 failed");

    artifacts.capture_jsonl("phase4_reexport", &jsonl_path);
    artifacts.capture_snapshot("after_export2", &workspace.root);

    // Verify the modification persisted
    let list = run_br(&workspace, ["list", "--json"], "list_verify");
    assert!(
        list.stdout.contains("critical"),
        "Modification should persist through full cycle"
    );

    // Check sync status
    let status = run_br(&workspace, ["sync", "--status"], "status");
    artifacts.record_command(
        "status",
        &status.stdout,
        &status.stderr,
        status.status.success(),
    );
    assert!(status.status.success(), "status check failed");

    artifacts.persist();

    eprintln!(
        "[PASS] e2e_sync_full_cycle_with_artifacts\n\
         - Phases completed: init -> create x3 -> export -> modify -> import -> export\n\
         - Artifacts saved to: {:?}",
        artifacts.artifact_dir
    );
}

/// E2E test: Sync status command with artifact capture.
#[test]
fn e2e_sync_status_with_artifacts() {
    let _log = common::test_log("e2e_sync_status_with_artifacts");
    let workspace = BrWorkspace::new();
    let mut artifacts = TestArtifacts::new(&workspace, "sync_status");

    // Initialize
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed");

    // Check status before any issues (clean state)
    let status1 = run_br(&workspace, ["sync", "--status", "--json"], "status_empty");
    artifacts.record_command(
        "status_empty",
        &status1.stdout,
        &status1.stderr,
        status1.status.success(),
    );
    assert!(status1.status.success(), "status check failed");

    // Create an issue (makes DB dirty)
    let create = run_br(&workspace, ["create", "Test issue"], "create");
    assert!(create.status.success(), "create failed");

    // Check status with dirty DB
    let status2 = run_br(&workspace, ["sync", "--status", "--json"], "status_dirty");
    artifacts.record_command(
        "status_dirty",
        &status2.stdout,
        &status2.stderr,
        status2.status.success(),
    );
    assert!(status2.status.success(), "status check failed");

    // Export
    let export = run_br(&workspace, ["sync", "--flush-only"], "export");
    assert!(export.status.success(), "export failed");

    // Check status after export (should be clean)
    let status3 = run_br(&workspace, ["sync", "--status", "--json"], "status_clean");
    artifacts.record_command(
        "status_clean",
        &status3.stdout,
        &status3.stderr,
        status3.status.success(),
    );
    assert!(status3.status.success(), "status check failed");

    artifacts.persist();

    eprintln!(
        "[PASS] e2e_sync_status_with_artifacts\n\
         - Status checks: empty -> dirty -> clean\n\
         - Artifacts saved to: {:?}",
        artifacts.artifact_dir
    );
}

/// E2E test: Error handling with artifact capture (conflict markers).
#[test]
fn e2e_sync_error_conflict_markers() {
    let _log = common::test_log("e2e_sync_error_conflict_markers");
    let workspace = BrWorkspace::new();
    let mut artifacts = TestArtifacts::new(&workspace, "sync_error_conflict");

    // Initialize and export
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed");

    let create = run_br(&workspace, ["create", "Test issue"], "create");
    assert!(create.status.success(), "create failed");

    let export = run_br(&workspace, ["sync", "--flush-only"], "export");
    assert!(export.status.success(), "export failed");

    // Inject conflict markers into JSONL
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let original = fs::read_to_string(&jsonl_path).expect("read jsonl");
    let corrupted = format!("<<<<<<< HEAD\n{original}=======\n{original}>>>>>>> branch\n");
    fs::write(&jsonl_path, &corrupted).expect("write corrupted");
    artifacts.capture_jsonl("corrupted", &jsonl_path);

    // Attempt import (should fail)
    let import = run_br(
        &workspace,
        ["sync", "--import-only", "--force"],
        "import_fail",
    );
    artifacts.record_command(
        "import_fail",
        &import.stdout,
        &import.stderr,
        import.status.success(),
    );

    // Verify import failed with conflict marker error
    assert!(
        !import.status.success(),
        "Import should fail with conflict markers"
    );
    assert!(
        import.stderr.to_lowercase().contains("conflict")
            || import.stderr.to_lowercase().contains("marker"),
        "Error should mention conflict markers\nstderr: {}",
        import.stderr
    );

    artifacts.persist();

    eprintln!(
        "[PASS] e2e_sync_error_conflict_markers\n\
         - Correctly rejected JSONL with conflict markers\n\
         - Artifacts saved to: {:?}",
        artifacts.artifact_dir
    );
}

/// E2E test: Empty database export behavior.
#[test]
fn e2e_sync_export_empty_db() {
    let _log = common::test_log("e2e_sync_export_empty_db");
    let workspace = BrWorkspace::new();
    let mut artifacts = TestArtifacts::new(&workspace, "sync_export_empty");

    // Initialize only (no issues)
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed");

    artifacts.capture_snapshot("after_init", &workspace.root);

    // Try to export empty DB (without --force)
    let export1 = run_br(&workspace, ["sync", "--flush-only"], "export_no_force");
    artifacts.record_command(
        "export_no_force",
        &export1.stdout,
        &export1.stderr,
        export1.status.success(),
    );
    // This may succeed or report "nothing to export" - both are valid

    // Export with --force
    let export2 = run_br(
        &workspace,
        ["sync", "--flush-only", "--force"],
        "export_force",
    );
    artifacts.record_command(
        "export_force",
        &export2.stdout,
        &export2.stderr,
        export2.status.success(),
    );
    assert!(
        export2.status.success(),
        "export --force failed: {}",
        export2.stderr
    );

    artifacts.capture_snapshot("after_export", &workspace.root);

    // Verify JSONL exists (may be empty)
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    artifacts.capture_jsonl("empty_export", &jsonl_path);

    artifacts.persist();

    eprintln!(
        "[PASS] e2e_sync_export_empty_db\n\
         - Empty DB export handled correctly\n\
         - Artifacts saved to: {:?}",
        artifacts.artifact_dir
    );
}

/// E2E test: Multiple exports preserve deterministic ordering.
#[test]
fn e2e_sync_deterministic_export() {
    let _log = common::test_log("e2e_sync_deterministic_export");
    let workspace = BrWorkspace::new();
    let mut artifacts = TestArtifacts::new(&workspace, "sync_deterministic");

    // Initialize
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed");

    // Create issues in specific order
    for title in ["Zebra", "Apple", "Mango", "Banana"] {
        let create = run_br(&workspace, ["create", title], &format!("create_{title}"));
        assert!(create.status.success(), "create failed");
    }

    // Export multiple times
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let mut exports = Vec::new();

    for i in 0..3 {
        let export = run_br(
            &workspace,
            ["sync", "--flush-only", "--force"],
            &format!("export{i}"),
        );
        assert!(export.status.success(), "export{i} failed");

        let content = fs::read_to_string(&jsonl_path).expect("read jsonl");
        artifacts.capture_jsonl(&format!("export{i}"), &jsonl_path);
        exports.push(content);
    }

    // Verify all exports are identical
    assert!(
        exports.windows(2).all(|w| w[0] == w[1]),
        "Multiple exports should produce identical JSONL"
    );

    // Verify issues are sorted (by ID)
    let lines: Vec<&str> = exports[0].lines().collect();
    let mut ids: Vec<String> = Vec::new();
    for line in lines {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(line)
            && let Some(id) = json.get("id").and_then(|v| v.as_str())
        {
            ids.push(id.to_string());
        }
    }
    let mut sorted_ids = ids.clone();
    sorted_ids.sort();
    assert_eq!(ids, sorted_ids, "JSONL should be sorted by ID");

    artifacts.persist();

    eprintln!(
        "[PASS] e2e_sync_deterministic_export\n\
         - 3 exports produced identical output\n\
         - Issues sorted by ID: {:?}\n\
         - Artifacts saved to: {:?}",
        ids, artifacts.artifact_dir
    );
}

/// E2E test: Staleness detection hash check prevents false positives from touch.
///
/// Related beads:
/// - beads_rust-3qi: Auto-import staleness detection (Lstat + content hash + conflict markers)
#[test]
fn e2e_staleness_hash_check_prevents_false_touch() {
    use std::thread;
    use std::time::Duration;

    let _log = common::test_log("e2e_staleness_hash_check_prevents_false_touch");
    let workspace = BrWorkspace::new();
    let mut artifacts = TestArtifacts::new(&workspace, "staleness_hash_check");

    // Initialize
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create an issue
    let create = run_br(&workspace, ["create", "Test staleness"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Export to JSONL
    let export = run_br(&workspace, ["sync", "--flush-only"], "export");
    assert!(export.status.success(), "export failed: {}", export.stderr);

    // Check status - should be in sync
    let status1 = run_br(
        &workspace,
        ["sync", "--status", "--json"],
        "status_after_export",
    );
    artifacts.record_command(
        "status_after_export",
        &status1.stdout,
        &status1.stderr,
        status1.status.success(),
    );
    assert!(status1.status.success(), "status check failed");
    let payload1 = common::cli::extract_json_payload(&status1.stdout);
    let json1: serde_json::Value = serde_json::from_str(&payload1).unwrap_or_else(|e| {
        panic!(
            "parse status json failed: {}\nSTDOUT:\n{}\nSTDERR:\n{}",
            e, status1.stdout, status1.stderr
        );
    });
    assert!(
        !json1["jsonl_newer"].as_bool().unwrap_or(true),
        "JSONL should not be marked newer after export"
    );

    // Sleep briefly to ensure mtime would differ
    thread::sleep(Duration::from_millis(100));

    // Touch the JSONL file (updates mtime but not content)
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let content = fs::read_to_string(&jsonl_path).expect("read jsonl");
    fs::write(&jsonl_path, &content).expect("touch jsonl");
    artifacts.capture_jsonl("after_touch", &jsonl_path);

    // Check status again - should NOT be marked stale due to hash check
    let status2 = run_br(
        &workspace,
        ["sync", "--status", "--json"],
        "status_after_touch",
    );
    artifacts.record_command(
        "status_after_touch",
        &status2.stdout,
        &status2.stderr,
        status2.status.success(),
    );
    assert!(status2.status.success(), "status check failed");
    let payload2 = common::cli::extract_json_payload(&status2.stdout);
    let json2: serde_json::Value = serde_json::from_str(&payload2).expect("parse status json");

    // Hash check should prevent false staleness: mtime changed but content didn't
    assert!(
        !json2["jsonl_newer"].as_bool().unwrap_or(true),
        "JSONL should NOT be marked newer after touch (hash unchanged)\n\
         mtime updated but content hash is the same\n\
         status output: {}",
        status2.stdout
    );

    artifacts.persist();

    eprintln!(
        "[PASS] e2e_staleness_hash_check_prevents_false_touch\n\
         - Exported JSONL\n\
         - Touched file (mtime changed, content unchanged)\n\
         - Hash check correctly prevented false staleness\n\
         - Artifacts saved to: {:?}",
        artifacts.artifact_dir
    );
}

/// E2E test: Staleness detection correctly identifies real changes.
///
/// Related beads:
/// - beads_rust-3qi: Auto-import staleness detection (Lstat + content hash + conflict markers)
#[test]
fn e2e_staleness_detects_real_content_change() {
    let _log = common::test_log("e2e_staleness_detects_real_content_change");
    let workspace = BrWorkspace::new();
    let mut artifacts = TestArtifacts::new(&workspace, "staleness_real_change");

    // Initialize
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create an issue
    let create = run_br(&workspace, ["create", "Test staleness"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Export to JSONL
    let export = run_br(&workspace, ["sync", "--flush-only"], "export");
    assert!(export.status.success(), "export failed: {}", export.stderr);

    // Modify the JSONL content (simulate external change)
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let mut content = fs::read_to_string(&jsonl_path).expect("read jsonl");
    artifacts.capture_jsonl("before_modify", &jsonl_path);

    // Append a comment to trigger content change
    content.push_str("# External comment added\n");
    fs::write(&jsonl_path, &content).expect("write modified jsonl");
    artifacts.capture_jsonl("after_modify", &jsonl_path);

    // Check status - should be marked stale (jsonl_newer = true)
    let status = run_br(
        &workspace,
        ["sync", "--status", "--json"],
        "status_after_modify",
    );
    artifacts.record_command(
        "status_after_modify",
        &status.stdout,
        &status.stderr,
        status.status.success(),
    );
    assert!(status.status.success(), "status check failed");
    let payload = common::cli::extract_json_payload(&status.stdout);
    let json: serde_json::Value = serde_json::from_str(&payload).unwrap_or_else(|e| {
        panic!(
            "parse status json failed: {}\nSTDOUT:\n{}\nSTDERR:\n{}",
            e, status.stdout, status.stderr
        );
    });

    // Real content change should trigger staleness
    assert!(
        json["jsonl_newer"].as_bool().unwrap_or(false),
        "JSONL should be marked newer after real content change\n\
         Content was modified, hash should differ\n\
         status output: {}",
        status.stdout
    );

    artifacts.persist();

    eprintln!(
        "[PASS] e2e_staleness_detects_real_content_change\n\
         - Exported JSONL\n\
         - Modified file content\n\
         - Staleness correctly detected (hash changed)\n\
         - Artifacts saved to: {:?}",
        artifacts.artifact_dir
    );
}

/// Regression test for issue #248: `br sync --import-only --force` left the
/// on-disk SQLite file in a state where C sqlite3's `PRAGMA integrity_check`
/// reported "database disk image is malformed (11)" and where a subsequent
/// `br close` could fail with "Issue not found" / corrupt the DB further.
///
/// The root cause was that the force/rebuild path drops and recreates the
/// data tables before bulk-inserting from JSONL, but (unlike the
/// `rebuild_database_family` chokepoint used by `br doctor --repair` and auto
/// recovery) it did not run a post-import `VACUUM` + `REINDEX`.  On larger
/// imports this left partial-index rows missing and B-tree freeblock
/// accounting anomalies that only surfaced when a later write transaction
/// looked up an issue by id (issues #237, #245, #246 covered the adjacent
/// paths).  This test exports ~220 issues to JSONL, removes the DB, runs
/// `br sync --import-only --force`, and asserts that:
///   1. The rebuilt DB passes the strict C-sqlite3 `PRAGMA integrity_check`
///      (frankensqlite-generated files must be readable by upstream sqlite).
///   2. A subsequent `br close` on a non-zero-blocker issue succeeds and
///      the DB still passes `integrity_check` afterwards.
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_sync_import_force_preserves_integrity_and_close_works() {
    // Create enough issues to cross the bulk-insert threshold where
    // frankensqlite's B-tree layer stops cleaning up after itself reliably.
    // 220 is comfortably above the 200 seen in empirical repros for #248
    // and well under any test-timeout budget.
    const ISSUE_COUNT: usize = 220;

    let _log = common::test_log("e2e_sync_import_force_preserves_integrity_and_close_works");
    if !require_sqlite3_cli("e2e_sync_import_force_preserves_integrity_and_close_works") {
        return;
    }

    let workspace = BrWorkspace::new();
    let mut artifacts = TestArtifacts::new(&workspace, "sync_import_force_preserves_integrity");

    // Initialize with a custom prefix so resolution can't fall back to the
    // default "br-" prefix (matches the reporter's swarm workspace setup).
    let init = run_br(&workspace, ["init", "--prefix", "rr"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let mut last_id: String = String::new();
    for i in 0..ISSUE_COUNT {
        let title = format!("issue {i}");
        let create = run_br(
            &workspace,
            ["create", &title, "-p", "3", "--silent", "--no-auto-flush"],
            "create_bulk",
        );
        assert!(
            create.status.success(),
            "create {i} failed: {}",
            create.stderr
        );
        last_id = create.stdout.trim().to_string();
    }
    assert!(!last_id.is_empty(), "expected at least one created id");

    // Flush DB → JSONL so the JSONL is the canonical source for the rebuild.
    let flush = run_br(&workspace, ["sync", "--flush-only"], "flush");
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);

    let beads_dir = workspace.root.join(".beads");
    let db_path = beads_dir.join("beads.db");
    let jsonl_path = beads_dir.join("issues.jsonl");
    artifacts.capture_jsonl("after_flush", &jsonl_path);

    // Delete the DB family (db + WAL + SHM sidecars) to simulate the
    // "rebuild from canonical JSONL" path the reporter's swarm script takes.
    for sidecar in [
        "beads.db",
        "beads.db-wal",
        "beads.db-shm",
        "beads.db-journal",
    ] {
        let path = beads_dir.join(sidecar);
        if path.exists() {
            fs::remove_file(&path).expect("remove db sidecar");
        }
    }

    // Re-init and force-import from the canonical JSONL.  The `--force`
    // flag triggers `reset_data_tables()` + bulk import — the exact path
    // that historically left frankensqlite's B-tree/indexes inconsistent.
    let reinit = run_br(&workspace, ["init", "--prefix", "rr", "--force"], "reinit");
    assert!(reinit.status.success(), "reinit failed: {}", reinit.stderr);

    let import = run_br(
        &workspace,
        ["sync", "--import-only", "--force"],
        "force_import",
    );
    artifacts.record_command(
        "force_import",
        &import.stdout,
        &import.stderr,
        import.status.success(),
    );
    assert!(
        import.status.success(),
        "force import failed: {}",
        import.stderr
    );

    // Probe #1: C-sqlite3 must see an integrity-clean file.  `sqlite3`
    // binary is a hard test dependency because that's the parser that the
    // reporter's `check_beads_trust.sh` uses in the wild.
    let integrity_before_close = run_sqlite3_pragma_integrity_check(&db_path);
    artifacts.record_command(
        "integrity_check_before_close",
        &integrity_before_close,
        "",
        integrity_before_close.trim() == "ok",
    );
    assert_eq!(
        integrity_before_close.trim(),
        "ok",
        "C sqlite3 integrity_check must pass after force/rebuild import (issue #248).\n\
         Without the post-import VACUUM+REINDEX, this returns\n\
         'database disk image is malformed (11)'.\n\
         output: {integrity_before_close}"
    );

    // Probe #2: a regular `br close` on an issue visible to `br show`
    // must succeed without tripping the mutation-path "Issue not found"
    // code path and without further corrupting the DB.
    let show = run_br(&workspace, ["show", &last_id], "show_before_close");
    assert!(
        show.status.success(),
        "show {last_id} failed: {}",
        show.stderr
    );

    let close = run_br(&workspace, ["close", &last_id], "close");
    artifacts.record_command(
        "close",
        &close.stdout,
        &close.stderr,
        close.status.success(),
    );
    assert!(
        close.status.success(),
        "close {last_id} failed after force/rebuild import: stdout={} stderr={}",
        close.stdout,
        close.stderr
    );
    assert!(
        !close.stderr.contains("Issue not found"),
        "close should not report 'Issue not found' for an id visible to show (issue #248):\n{}",
        close.stderr
    );

    let integrity_after_close = run_sqlite3_pragma_integrity_check(&db_path);
    artifacts.record_command(
        "integrity_check_after_close",
        &integrity_after_close,
        "",
        integrity_after_close.trim() == "ok",
    );
    assert_eq!(
        integrity_after_close.trim(),
        "ok",
        "C sqlite3 integrity_check must still pass after a successful close (issue #248).\n\
         output: {integrity_after_close}"
    );

    artifacts.persist();

    eprintln!(
        "[PASS] e2e_sync_import_force_preserves_integrity_and_close_works\n\
         - Exported {ISSUE_COUNT} issues to JSONL\n\
         - Removed DB and force-imported from JSONL\n\
         - C sqlite3 integrity_check clean before and after close\n\
         - Close succeeded without 'Issue not found'\n\
         - Artifacts saved to: {:?}",
        artifacts.artifact_dir
    );
}

/// Regression coverage for the "rebuilt alternate DB" workflow seen during
/// real planning sessions: create a sibling DB via
/// `br --db <alt> sync --import-only --rebuild`, then continue working
/// against that alternate DB family. Historically this was the matrix where
/// freshly-created ids would sometimes appear in `br list` yet fail in
/// `br show` / `br update` / `br dep add` with `Issue not found`.
///
/// The test keeps the rebuilt DB as the active target, then repeatedly:
///   1. creates a fresh bead in the rebuilt DB,
///   2. resolves it immediately via `show`,
///   3. mutates it via `update`,
///   4. adds a dependency to a seed bead that came from the rebuild,
///   5. defers every other bead,
///
/// It also asserts the alternate DB stays readable by upstream sqlite3.
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_rebuilt_alt_db_preserves_fresh_lookup_and_mutation_paths() {
    const LOOP_COUNT: usize = 25;

    let _log = common::test_log("e2e_rebuilt_alt_db_preserves_fresh_lookup_and_mutation_paths");
    if !require_sqlite3_cli("e2e_rebuilt_alt_db_preserves_fresh_lookup_and_mutation_paths") {
        return;
    }

    let workspace = BrWorkspace::new();
    let mut artifacts = TestArtifacts::new(&workspace, "rebuilt_alt_db_fresh_lookup");

    let init = run_br(
        &workspace,
        ["init", "--prefix", "alt"],
        "init_alt_workspace",
    );
    artifacts.record_command(
        "init_alt_workspace",
        &init.stdout,
        &init.stderr,
        init.status.success(),
    );
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let seed_a = run_br(
        &workspace,
        [
            "create",
            "--title",
            "seed A",
            "--type",
            "task",
            "--priority",
            "2",
            "--json",
        ],
        "create_seed_a",
    );
    artifacts.record_command(
        "create_seed_a",
        &seed_a.stdout,
        &seed_a.stderr,
        seed_a.status.success(),
    );
    assert!(
        seed_a.status.success(),
        "seed A create failed: {}",
        seed_a.stderr
    );
    let seed_a_payload = common::cli::extract_json_payload(&seed_a.stdout);
    let seed_a_json: serde_json::Value =
        serde_json::from_str(&seed_a_payload).expect("seed A create json");
    let seed_a_id = seed_a_json["id"].as_str().expect("seed A id").to_string();

    let seed_b = run_br(
        &workspace,
        [
            "create",
            "--title",
            "seed B",
            "--type",
            "task",
            "--priority",
            "2",
            "--json",
        ],
        "create_seed_b",
    );
    artifacts.record_command(
        "create_seed_b",
        &seed_b.stdout,
        &seed_b.stderr,
        seed_b.status.success(),
    );
    assert!(
        seed_b.status.success(),
        "seed B create failed: {}",
        seed_b.stderr
    );
    let seed_b_payload = common::cli::extract_json_payload(&seed_b.stdout);
    let seed_b_json: serde_json::Value =
        serde_json::from_str(&seed_b_payload).expect("seed B create json");
    let seed_b_id = seed_b_json["id"].as_str().expect("seed B id").to_string();

    let flush = run_br(
        &workspace,
        ["sync", "--flush-only"],
        "flush_before_alt_rebuild",
    );
    artifacts.record_command(
        "flush_before_alt_rebuild",
        &flush.stdout,
        &flush.stderr,
        flush.status.success(),
    );
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);

    let beads_dir = workspace.root.join(".beads");
    let jsonl_path = beads_dir.join("issues.jsonl");
    artifacts.capture_jsonl("before_alt_rebuild", &jsonl_path);

    let alt_db = beads_dir.join("beads.rebuilt.db");
    let rebuild = run_br(
        &workspace,
        [
            "--db",
            alt_db.to_str().expect("alt db path"),
            "sync",
            "--import-only",
            "--rebuild",
            "--json",
            "--no-auto-import",
            "--no-auto-flush",
        ],
        "rebuild_alt_db",
    );
    artifacts.record_command(
        "rebuild_alt_db",
        &rebuild.stdout,
        &rebuild.stderr,
        rebuild.status.success(),
    );
    assert!(
        rebuild.status.success(),
        "alt-db rebuild failed: stdout={} stderr={}",
        rebuild.stdout,
        rebuild.stderr
    );

    let integrity_after_rebuild = run_sqlite3_pragma_integrity_check(&alt_db);
    artifacts.record_command(
        "integrity_after_rebuild",
        &integrity_after_rebuild,
        "",
        integrity_after_rebuild.trim() == "ok",
    );
    assert_eq!(
        integrity_after_rebuild.trim(),
        "ok",
        "rebuilt alternate DB must pass upstream sqlite3 integrity_check before we trust it.\noutput: {integrity_after_rebuild}"
    );

    let dep_seed = run_br(
        &workspace,
        [
            "--db",
            alt_db.to_str().expect("alt db path"),
            "dep",
            "add",
            &seed_b_id,
            &seed_a_id,
            "--json",
        ],
        "seed_dep_add_after_rebuild",
    );
    artifacts.record_command(
        "seed_dep_add_after_rebuild",
        &dep_seed.stdout,
        &dep_seed.stderr,
        dep_seed.status.success(),
    );
    assert!(
        dep_seed.status.success(),
        "seed dep add failed after rebuild: stdout={} stderr={}",
        dep_seed.stdout,
        dep_seed.stderr
    );

    for i in 0..LOOP_COUNT {
        let title = format!("rebuilt alt lookup loop {i}");
        let create = run_br(
            &workspace,
            [
                "--db",
                alt_db.to_str().expect("alt db path"),
                "create",
                "--title",
                &title,
                "--type",
                "task",
                "--priority",
                "2",
                "--json",
            ],
            &format!("loop_create_{i}"),
        );
        artifacts.record_command(
            &format!("loop_create_{i}"),
            &create.stdout,
            &create.stderr,
            create.status.success(),
        );
        assert!(
            create.status.success(),
            "create in rebuilt alt DB failed on loop {i}: stdout={} stderr={}",
            create.stdout,
            create.stderr
        );
        let create_payload = common::cli::extract_json_payload(&create.stdout);
        let create_json: serde_json::Value =
            serde_json::from_str(&create_payload).expect("loop create json");
        let fresh_id = create_json["id"]
            .as_str()
            .expect("fresh issue id")
            .to_string();

        let show = run_br(
            &workspace,
            [
                "--db",
                alt_db.to_str().expect("alt db path"),
                "show",
                &fresh_id,
                "--json",
            ],
            &format!("loop_show_{i}"),
        );
        artifacts.record_command(
            &format!("loop_show_{i}"),
            &show.stdout,
            &show.stderr,
            show.status.success(),
        );
        assert!(
            show.status.success(),
            "show failed for freshly-created id {fresh_id} on loop {i}: stdout={} stderr={}",
            show.stdout,
            show.stderr
        );
        assert!(
            !show.stderr.contains("Issue not found") && !show.stdout.contains("Issue not found"),
            "show reported 'Issue not found' for freshly-created id {fresh_id} on loop {i}: stdout={} stderr={}",
            show.stdout,
            show.stderr
        );
        let show_payload = common::cli::extract_json_payload(&show.stdout);
        let show_json: serde_json::Value =
            serde_json::from_str(&show_payload).expect("show json payload");
        let show_items = show_json.as_array().expect("show payload array");
        assert_eq!(
            show_items.len(),
            1,
            "show should return exactly one issue for {fresh_id} on loop {i}: {show_payload}"
        );
        assert_eq!(
            show_items[0]["id"].as_str(),
            Some(fresh_id.as_str()),
            "show returned the wrong issue for {fresh_id} on loop {i}: {show_payload}"
        );

        let update = run_br(
            &workspace,
            [
                "--db",
                alt_db.to_str().expect("alt db path"),
                "update",
                &fresh_id,
                "--notes",
                &format!("loop note {i}"),
                "--json",
            ],
            &format!("loop_update_{i}"),
        );
        artifacts.record_command(
            &format!("loop_update_{i}"),
            &update.stdout,
            &update.stderr,
            update.status.success(),
        );
        assert!(
            update.status.success(),
            "update failed for freshly-created id {fresh_id} on loop {i}: stdout={} stderr={}",
            update.stdout,
            update.stderr
        );
        assert!(
            !update.stderr.contains("Issue not found")
                && !update.stdout.contains("Issue not found"),
            "update reported 'Issue not found' for freshly-created id {fresh_id} on loop {i}: stdout={} stderr={}",
            update.stdout,
            update.stderr
        );

        let dep_add = run_br(
            &workspace,
            [
                "--db",
                alt_db.to_str().expect("alt db path"),
                "dep",
                "add",
                &fresh_id,
                &seed_a_id,
                "--json",
            ],
            &format!("loop_dep_add_{i}"),
        );
        artifacts.record_command(
            &format!("loop_dep_add_{i}"),
            &dep_add.stdout,
            &dep_add.stderr,
            dep_add.status.success(),
        );
        assert!(
            dep_add.status.success(),
            "dep add failed for freshly-created id {fresh_id} on loop {i}: stdout={} stderr={}",
            dep_add.stdout,
            dep_add.stderr
        );
        assert!(
            !dep_add.stderr.contains("Issue not found")
                && !dep_add.stdout.contains("Issue not found"),
            "dep add reported 'Issue not found' for freshly-created id {fresh_id} on loop {i}: stdout={} stderr={}",
            dep_add.stdout,
            dep_add.stderr
        );

        if i % 2 == 0 {
            let defer = run_br(
                &workspace,
                [
                    "--db",
                    alt_db.to_str().expect("alt db path"),
                    "defer",
                    &fresh_id,
                    "--until",
                    "2099-01-01",
                    "--json",
                ],
                &format!("loop_defer_{i}"),
            );
            artifacts.record_command(
                &format!("loop_defer_{i}"),
                &defer.stdout,
                &defer.stderr,
                defer.status.success(),
            );
            assert!(
                defer.status.success(),
                "defer failed for freshly-created id {fresh_id} on loop {i}: stdout={} stderr={}",
                defer.stdout,
                defer.stderr
            );
            assert!(
                !defer.stderr.contains("Issue not found")
                    && !defer.stdout.contains("Issue not found"),
                "defer reported 'Issue not found' for freshly-created id {fresh_id} on loop {i}: stdout={} stderr={}",
                defer.stdout,
                defer.stderr
            );
        }
    }

    let integrity_after_loops = run_sqlite3_pragma_integrity_check(&alt_db);
    artifacts.record_command(
        "integrity_after_loops",
        &integrity_after_loops,
        "",
        integrity_after_loops.trim() == "ok",
    );
    assert_eq!(
        integrity_after_loops.trim(),
        "ok",
        "alternate DB must stay readable by upstream sqlite3 after repeated fresh-id mutations.\noutput: {integrity_after_loops}"
    );

    artifacts.capture_jsonl("after_alt_db_loops", &jsonl_path);
    artifacts.persist();

    eprintln!(
        "[PASS] e2e_rebuilt_alt_db_preserves_fresh_lookup_and_mutation_paths\n\
         - Rebuilt alternate DB from canonical JSONL\n\
         - Exercised {LOOP_COUNT} fresh create/show/update/dep/defer loops on the rebuilt DB\n\
         - No command reported 'Issue not found' for freshly-created IDs\n\
         - Upstream sqlite3 integrity_check stayed clean\n\
         - Artifacts saved to: {:?}",
        artifacts.artifact_dir
    );
}

/// Invoke the C `sqlite3` CLI to run `PRAGMA integrity_check` against the
/// given database.  Returns stdout verbatim (expected to be "ok\n" for a
/// clean DB).  Callers should use `require_sqlite3_cli` before exercising
/// this external-tool coverage path.
fn run_sqlite3_pragma_integrity_check(db_path: &Path) -> String {
    match std::process::Command::new("sqlite3")
        .arg(db_path)
        .arg("PRAGMA integrity_check;")
        .output()
    {
        Ok(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            if stdout.trim().is_empty() {
                // sqlite3 prints error messages on stderr and exits non-zero
                // when the DB is unreadable; surface the stderr so the
                // regression test makes clear what went wrong.
                format!(
                    "<sqlite3 exited {} with empty stdout; stderr: {}>",
                    output.status,
                    String::from_utf8_lossy(&output.stderr).trim()
                )
            } else {
                stdout
            }
        }
        Err(err) => format!("sqlite3-missing: {err}"),
    }
}

fn require_sqlite3_cli(test_name: &str) -> bool {
    let available = std::process::Command::new("sqlite3")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|status| status.success());

    if !available {
        eprintln!(
            "[skip] {test_name}: missing sqlite3 CLI; install sqlite3 to run upstream integrity_check coverage"
        );
    }

    available
}
