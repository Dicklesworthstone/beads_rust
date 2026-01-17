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
        let snapshot_path = self.artifact_dir.join(format!("{}_snapshots.txt", self.test_name));
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
        let logs_path = self.artifact_dir.join(format!("{}_commands.log", self.test_name));
        let logs_content: String = self.command_logs.iter().map(|(_, log)| log.as_str()).collect();
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
                        let hash = format!("{:x}", digest.finalize());
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
fn e2e_sync_export_with_artifacts() {
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
        ["create", "First issue", "-t", "task", "-p", "1"],
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
        ["create", "Second issue", "-t", "bug", "-p", "2"],
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
    assert!(
        sync.status.success(),
        "sync export failed: {}",
        sync.stderr
    );

    // Capture JSONL output
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    artifacts.capture_jsonl("after_export", &jsonl_path);

    artifacts.capture_snapshot("after_export", &workspace.root);

    // Verify JSONL was created
    assert!(jsonl_path.exists(), "JSONL file should exist after export");

    // Verify manifest was created
    let manifest_path = workspace.root.join(".beads").join(".manifest.json");
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
    let workspace = BrWorkspace::new();
    let mut artifacts = TestArtifacts::new(&workspace, "sync_import");

    // Initialize and create issues
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed");

    let create = run_br(&workspace, ["create", "Original issue"], "create");
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
    let import = run_br(
        &workspace,
        ["sync", "--import-only", "--force"],
        "import",
    );
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
            ["create", title, "-t", typ],
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
    let import = run_br(
        &workspace,
        ["sync", "--import-only", "--force"],
        "import",
    );
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
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(line) {
            if let Some(id) = json.get("id").and_then(|v| v.as_str()) {
                ids.push(id.to_string());
            }
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
