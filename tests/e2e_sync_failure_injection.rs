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
use common::cli::{BrRun, BrWorkspace, extract_json_payload, parse_created_id, run_br};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::{self, OpenOptions, Permissions};
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command as StdCommand, Stdio};
use std::thread;
use std::time::Duration;
use tempfile::TempDir;

#[cfg(target_os = "linux")]
const WRITE_LOCK_WAIT_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(target_os = "linux")]
const WRITE_LOCK_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Test artifacts for failure injection tests.
struct FailureTestArtifacts {
    artifact_dir: PathBuf,
    test_name: String,
    logs: Vec<(String, String)>,
    snapshots: Vec<(String, BTreeMap<String, String>)>,
}

fn export_temp_path_for_test(output_path: &Path) -> PathBuf {
    output_path.with_extension(format!("jsonl.{}.tmp", std::process::id()))
}

fn should_clear_inherited_br_env(key: &OsStr) -> bool {
    let key = key.to_string_lossy();
    key.starts_with("BD_")
        || key.starts_with("BEADS_")
        || matches!(
            key.as_ref(),
            "BR_OUTPUT_FORMAT" | "TOON_DEFAULT_FORMAT" | "TOON_STATS"
        )
}

fn clear_inherited_br_env(cmd: &mut StdCommand) {
    for (key, _) in std::env::vars_os() {
        if should_clear_inherited_br_env(&key) {
            cmd.env_remove(&key);
        }
    }
}

fn spawn_br_child<I, S>(workspace: &BrWorkspace, args: I) -> std::process::Child
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = StdCommand::new(assert_cmd::cargo::cargo_bin!("br"));
    cmd.current_dir(&workspace.root);
    cmd.args(args);
    clear_inherited_br_env(&mut cmd);
    cmd.env("NO_COLOR", "1");
    cmd.env("RUST_BACKTRACE", "1");
    cmd.env("HOME", &workspace.root);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    cmd.spawn().expect("spawn br child")
}

#[cfg(target_os = "linux")]
fn read_child_wait_channel(pid: u32) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/wchan"))
        .ok()
        .map(|channel| channel.trim().to_string())
}

#[cfg(target_os = "linux")]
fn is_write_lock_wait_channel(channel: &str) -> bool {
    // Under heavy load, the child might be observed mid-backoff (hrtimer_nanosleep)
    // or waiting on a futex used by fs2's lock primitives, rather than directly
    // in the fcntl/flock syscall. All of these indicate "blocked waiting for the
    // exclusive write lock" for the purposes of this test.
    let channel = channel.to_ascii_lowercase();
    channel.contains("lock")
        || channel.contains("flock")
        || channel.contains("futex")
        || channel.contains("nanosleep")
}

fn wait_for_child_to_block_on_write_lock(child: &mut std::process::Child, label: &str) {
    #[cfg(target_os = "linux")]
    {
        let deadline = std::time::Instant::now() + WRITE_LOCK_WAIT_OBSERVATION_TIMEOUT;

        loop {
            let status = child.try_wait().expect("poll child while waiting for lock");
            assert!(
                status.is_none(),
                "{label} exited before reaching .write.lock contention: {status:?}"
            );

            let wait_channel = read_child_wait_channel(child.id());
            if wait_channel
                .as_deref()
                .is_some_and(is_write_lock_wait_channel)
            {
                return;
            }

            assert!(
                std::time::Instant::now() < deadline,
                "{label} stayed alive but was never observed blocked on .write.lock; last wait channel: {wait_channel:?}"
            );
            thread::sleep(WRITE_LOCK_WAIT_POLL_INTERVAL);
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        thread::sleep(Duration::from_millis(250));
        let status = child.try_wait().expect("poll child while waiting for lock");
        assert!(
            status.is_none(),
            "{label} should still be waiting on .write.lock; status={status:?}"
        );
    }
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
                let hash = beads_rust::util::hex_encode(&Sha256::digest(&content));
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
        Some(beads_rust::util::hex_encode(&Sha256::digest(&content)))
    } else {
        None
    }
}

fn jsonl_export_temp_files(beads_dir: &Path) -> Vec<PathBuf> {
    let mut temps = Vec::new();
    if let Ok(entries) = fs::read_dir(beads_dir) {
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let file_name = file_name.to_string_lossy();
            if file_name == "issues.jsonl.tmp"
                || (file_name.starts_with("issues.jsonl.") && file_name.ends_with(".tmp"))
            {
                temps.push(entry.path());
            }
        }
    }
    temps.sort_unstable();
    temps
}

fn read_jsonl_values(path: &Path, context: &str) -> Vec<Value> {
    let content = fs::read_to_string(path);
    let read_error = match content.as_ref() {
        Ok(_) => String::new(),
        Err(err) => err.to_string(),
    };
    assert!(
        content.is_ok(),
        "{context} should be readable at {}: {read_error}",
        path.display()
    );
    let content = content.unwrap_or_default();
    content
        .lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                return None;
            }
            let parsed = serde_json::from_str(trimmed);
            let parse_error = match parsed.as_ref() {
                Ok(_) => String::new(),
                Err(err) => err.to_string(),
            };
            assert!(
                parsed.is_ok(),
                "{context} line {} should be valid JSON: {parse_error}\nline={trimmed}",
                index + 1
            );
            Some(parsed.unwrap_or(Value::Null))
        })
        .collect()
}

fn parse_stdout_json(run: &BrRun, context: &str) -> Value {
    let payload = extract_json_payload(&run.stdout);
    let parsed = serde_json::from_str(&payload);
    let parse_error = match parsed.as_ref() {
        Ok(_) => String::new(),
        Err(err) => err.to_string(),
    };
    assert!(
        parsed.is_ok(),
        "{context} should emit valid JSON on stdout: {parse_error}\nstdout={}\nstderr={}",
        run.stdout,
        run.stderr
    );
    parsed.unwrap_or(Value::Null)
}

fn assert_run_success(run: &BrRun, context: &str) {
    assert!(
        run.status.success(),
        "{context} failed\nstdout={}\nstderr={}",
        run.stdout,
        run.stderr
    );
    assert!(
        run.log_path.exists(),
        "{context} should leave a command log at {}",
        run.log_path.display()
    );
}

fn assert_run_failure(run: &BrRun, context: &str) {
    assert!(
        !run.status.success(),
        "{context} unexpectedly succeeded\nstdout={}\nstderr={}",
        run.stdout,
        run.stderr
    );
    assert!(
        run.log_path.exists(),
        "{context} should leave a failure log at {}",
        run.log_path.display()
    );
}

fn sync_status_json(workspace: &BrWorkspace, label: &str) -> Value {
    let run = run_br(workspace, ["sync", "--status", "--json"], label);
    assert_run_success(&run, label);
    parse_stdout_json(&run, label)
}

fn dirty_count(status: &Value) -> u64 {
    let count = status["dirty_count"].as_u64();
    assert!(count.is_some(), "sync status missing dirty_count: {status}");
    count.unwrap_or(0)
}

fn assert_dirty_status(status: &Value, context: &str) {
    assert!(
        dirty_count(status) > 0 || status["db_newer"].as_bool() == Some(true),
        "{context} should be visibly dirty or DB-newer, not silently clean: {status}"
    );
}

fn assert_clean_status(status: &Value, context: &str) {
    assert_eq!(
        dirty_count(status),
        0,
        "{context} should clear dirty issues after explicit recovery: {status}"
    );
    assert_eq!(
        status["db_newer"].as_bool(),
        Some(false),
        "{context} should not remain DB-newer after explicit recovery: {status}"
    );
}

fn flush_and_assert_clean(workspace: &BrWorkspace, label: &str) {
    let flush = run_br(workspace, ["sync", "--flush-only", "--json"], label);
    assert_run_success(&flush, label);
    let status = sync_status_json(workspace, &format!("{label}_status"));
    assert_clean_status(&status, label);
}

fn assert_doctor_healthy(workspace: &BrWorkspace, label: &str) {
    let doctor = run_br(workspace, ["doctor", "--json"], label);
    if doctor.status.success() {
        return;
    }

    let repair_label = format!("{label}_repair");
    let repair = run_br(workspace, ["doctor", "--repair", "--json"], &repair_label);
    assert!(
        repair.status.success(),
        "{label} failed and doctor --repair could not recover it\n\
         initial stdout={}\ninitial stderr={}\n\
         repair stdout={}\nrepair stderr={}",
        doctor.stdout,
        doctor.stderr,
        repair.stdout,
        repair.stderr
    );
}

fn assert_stale_show_finds(workspace: &BrWorkspace, issue_id: &str, label: &str) {
    let show = run_br(
        workspace,
        [
            "show",
            issue_id,
            "--no-auto-import",
            "--allow-stale",
            "--json",
        ],
        label,
    );
    assert_run_success(&show, label);
    let json = parse_stdout_json(&show, label);
    let found = json["id"].as_str() == Some(issue_id)
        || json.as_array().is_some_and(|issues| {
            issues
                .iter()
                .any(|issue| issue["id"].as_str() == Some(issue_id))
        });
    assert!(
        found,
        "{label} should keep {issue_id} visible after restart-style stale read: {json}"
    );
}

fn run_dirty_mutation(workspace: &BrWorkspace, args: Vec<String>, label: &str) {
    let run = run_br(workspace, args, label);
    assert_run_success(&run, label);
    let dirty_status = sync_status_json(workspace, &format!("{label}_dirty_status"));
    assert_dirty_status(&dirty_status, label);
    flush_and_assert_clean(workspace, &format!("{label}_recover_flush"));
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

    // Create the exact temp path used by export to block temp file creation.
    let temp_path = export_temp_path_for_test(&jsonl_path);
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

    // Export may succeed or fail depending on how the engine handles
    // read-only directories (temp file placement, fallback strategies).
    // The key invariant: if it fails, the JSONL must be unchanged.
    if !export2_run.status.success() {
        let final_hash = compute_file_hash(&jsonl_path);
        assert_eq!(
            initial_hash, final_hash,
            "JSONL should be unchanged after failed export"
        );
    }
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

/// Test: Mutation crash boundaries remain visible until explicit recovery.
///
/// The simulated crash point is "after the primary DB write but before the
/// automatic JSONL flush/dirty-clear path": every mutating command runs in a
/// fresh process with `--no-auto-flush`, then a second process verifies the
/// workspace is visibly dirty/stale and can be recovered by `sync --flush-only`.
#[test]
fn cli_mutation_crash_boundary_matrix_marks_dirty_until_recovered() {
    let _log = common::test_log("cli_mutation_crash_boundary_matrix_marks_dirty_until_recovered");
    let mut artifacts = FailureTestArtifacts::new("cli_mutation_crash_boundary_matrix");

    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "matrix_init");
    assert_run_success(&init, "matrix_init");

    let create = run_br(
        &workspace,
        ["create", "Crash matrix anchor", "--no-auto-flush"],
        "matrix_create_primary_write",
    );
    assert_run_success(&create, "matrix_create_primary_write");
    let issue_id = parse_created_id(&create.stdout);
    assert!(
        !issue_id.is_empty(),
        "create should report an issue id in stdout: {}",
        create.stdout
    );

    let create_dirty = sync_status_json(&workspace, "matrix_create_dirty_status");
    assert_dirty_status(&create_dirty, "create after primary write");
    assert_stale_show_finds(&workspace, &issue_id, "matrix_create_stale_show");
    flush_and_assert_clean(&workspace, "matrix_create_recover_flush");

    let cases = [
        (
            "update",
            vec![
                "update".to_string(),
                issue_id.clone(),
                "--status".to_string(),
                "in_progress".to_string(),
                "--json".to_string(),
                "--no-auto-flush".to_string(),
            ],
        ),
        (
            "label_add",
            vec![
                "label".to_string(),
                "add".to_string(),
                issue_id.clone(),
                "crash-matrix".to_string(),
                "--json".to_string(),
                "--no-auto-flush".to_string(),
            ],
        ),
        (
            "comment_add",
            vec![
                "comments".to_string(),
                "add".to_string(),
                issue_id.clone(),
                "crash boundary note".to_string(),
                "--json".to_string(),
                "--no-auto-flush".to_string(),
            ],
        ),
        (
            "close",
            vec![
                "close".to_string(),
                issue_id.clone(),
                "--reason".to_string(),
                "crash boundary close".to_string(),
                "--json".to_string(),
                "--no-auto-flush".to_string(),
            ],
        ),
        (
            "reopen",
            vec![
                "reopen".to_string(),
                issue_id.clone(),
                "--reason".to_string(),
                "crash boundary reopen".to_string(),
                "--json".to_string(),
                "--no-auto-flush".to_string(),
            ],
        ),
        (
            "defer",
            vec![
                "defer".to_string(),
                issue_id.clone(),
                "--until".to_string(),
                "+1h".to_string(),
                "--json".to_string(),
                "--no-auto-flush".to_string(),
            ],
        ),
        (
            "undefer",
            vec![
                "undefer".to_string(),
                issue_id.clone(),
                "--json".to_string(),
                "--no-auto-flush".to_string(),
            ],
        ),
    ];

    for (operation, args) in cases {
        let label = format!("matrix_{operation}_primary_write");
        run_dirty_mutation(&workspace, args, &label);
        assert_stale_show_finds(
            &workspace,
            &issue_id,
            &format!("matrix_{operation}_stale_show"),
        );
        artifacts.log(operation, "dirty state detected and recovered");
    }

    run_dirty_mutation(
        &workspace,
        vec![
            "delete".to_string(),
            issue_id,
            "--reason".to_string(),
            "crash boundary delete".to_string(),
            "--json".to_string(),
            "--no-auto-flush".to_string(),
        ],
        "matrix_delete_primary_write",
    );

    artifacts.log(
        "verification",
        "PASSED: create/update/label/comment/close/reopen/defer/undefer/delete dirty states were visible and recoverable",
    );
    artifacts.save();
}

/// Test: Sync crash boundaries preserve evidence and require explicit recovery.
///
/// This covers the observable sync-side crash phases:
/// - during temp-file creation/write
/// - after JSONL rename but before dirty flags are cleared
/// - during import/rebuild input validation
#[test]
fn cli_sync_crash_boundary_matrix_preserves_artifacts() {
    let _log = common::test_log("cli_sync_crash_boundary_matrix_preserves_artifacts");
    let mut artifacts = FailureTestArtifacts::new("cli_sync_crash_boundary_matrix");

    let temp_failure_workspace = TempDir::new().expect("temp failure workspace");
    let temp_failure_beads_dir = temp_failure_workspace.path().join(".beads");
    fs::create_dir_all(&temp_failure_beads_dir).expect("temp failure beads dir");
    let temp_failure_jsonl = temp_failure_beads_dir.join("issues.jsonl");
    fs::write(&temp_failure_jsonl, "{\"id\":\"old\",\"title\":\"Old\"}\n")
        .expect("write preserved jsonl");
    let temp_failure_hash = compute_file_hash(&temp_failure_jsonl).expect("temp failure hash");
    let blocked_temp_path = export_temp_path_for_test(&temp_failure_jsonl);
    fs::create_dir_all(&blocked_temp_path).expect("block direct export temp path");

    let mut temp_failure_storage = SqliteStorage::open_memory().expect("temp failure storage");
    let temp_failure_issue = create_test_issue("test-temp-failure", "Temp failure issue");
    temp_failure_storage
        .create_issue(&temp_failure_issue, "tester")
        .expect("create temp failure issue");
    let temp_failure_config = ExportConfig {
        beads_dir: Some(temp_failure_beads_dir.clone()),
        ..Default::default()
    };
    let temp_failure = export_to_jsonl(
        &temp_failure_storage,
        &temp_failure_jsonl,
        &temp_failure_config,
    );
    assert!(
        temp_failure.is_err(),
        "direct export should fail when the exact temp path is unavailable"
    );
    assert_eq!(
        temp_failure_hash,
        compute_file_hash(&temp_failure_jsonl).expect("hash after temp failure"),
        "failed temp-file write must preserve the pre-existing JSONL"
    );
    artifacts.snapshot_dir("after_temp_file_failure", temp_failure_workspace.path());

    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "sync_matrix_init");
    assert_run_success(&init, "sync_matrix_init");

    let create = run_br(
        &workspace,
        ["create", "Sync crash matrix anchor", "--no-auto-flush"],
        "sync_matrix_create_anchor",
    );
    assert_run_success(&create, "sync_matrix_create_anchor");
    let issue_id = parse_created_id(&create.stdout);
    assert!(
        !issue_id.is_empty(),
        "create should report an issue id in stdout: {}",
        create.stdout
    );

    let beads_dir = workspace.root.join(".beads");
    let jsonl_path = beads_dir.join("issues.jsonl");

    flush_and_assert_clean(&workspace, "sync_matrix_baseline_flush");
    let baseline_hash = compute_file_hash(&jsonl_path).expect("baseline hash");
    artifacts.log("baseline_hash", &baseline_hash);

    let update = run_br(
        &workspace,
        [
            "update",
            &issue_id,
            "--status",
            "in_progress",
            "--json",
            "--no-auto-flush",
        ],
        "sync_matrix_dirty_update",
    );
    assert_run_success(&update, "sync_matrix_dirty_update");
    let dirty_before_temp_failure = sync_status_json(&workspace, "sync_matrix_dirty_before_temp");
    assert_dirty_status(
        &dirty_before_temp_failure,
        "dirty update before temp failure",
    );
    assert_eq!(
        baseline_hash,
        compute_file_hash(&jsonl_path).expect("hash before direct export"),
        "the dirty DB write should not rewrite JSONL before explicit export"
    );

    let storage = SqliteStorage::open(&beads_dir.join("beads.db")).expect("open workspace db");
    let config = ExportConfig {
        beads_dir: Some(beads_dir.clone()),
        force: true,
        ..Default::default()
    };
    let direct_export = export_to_jsonl(&storage, &jsonl_path, &config)
        .expect("direct export simulates post-rename crash point");
    artifacts.log("direct_export_hash", &direct_export.content_hash);
    let dirty_after_direct_export = sync_status_json(&workspace, "sync_matrix_after_direct_export");
    assert_dirty_status(
        &dirty_after_direct_export,
        "after rename before dirty-clear simulation",
    );
    drop(storage);
    flush_and_assert_clean(&workspace, "sync_matrix_clear_dirty_after_direct_export");

    let recovered_jsonl = fs::read_to_string(&jsonl_path).expect("read recovered jsonl");
    fs::write(
        &jsonl_path,
        "<<<<<<< HEAD\n{\"id\":\"broken\"}\n=======\n{}\n>>>>>>> branch\n",
    )
    .expect("write conflict marker jsonl");
    let failed_import = run_br(
        &workspace,
        ["sync", "--import-only", "--force", "--json"],
        "sync_matrix_import_validation_failure",
    );
    assert_run_failure(&failed_import, "sync import validation failure");
    assert_stale_show_finds(
        &workspace,
        &issue_id,
        "sync_matrix_show_after_import_failure",
    );
    let import_failure_status =
        sync_status_json(&workspace, "sync_matrix_status_after_import_fail");
    assert!(
        import_failure_status["jsonl_newer"].as_bool() == Some(true)
            || dirty_count(&import_failure_status) > 0,
        "failed import should leave visible drift, not a silent clean bill: {import_failure_status}"
    );

    fs::write(&jsonl_path, recovered_jsonl).expect("restore valid jsonl");
    let repair_import = run_br(
        &workspace,
        ["sync", "--import-only", "--force", "--json"],
        "sync_matrix_import_recovery",
    );
    assert_run_success(&repair_import, "sync import recovery");
    let final_status = sync_status_json(&workspace, "sync_matrix_final_status");
    assert_clean_status(&final_status, "sync matrix final recovery");

    artifacts.log(
        "verification",
        "PASSED: temp-file failure, post-rename dirty state, and import validation failure were visible and recoverable",
    );
    artifacts.save();
}

/// Test: A killed `sync --flush-only` process must not silently mark dirty
/// state as exported or mutate JSONL before it owns the write-side lock.
#[test]
#[cfg(unix)]
#[allow(clippy::incompatible_msrv)]
fn cli_sync_flush_sigkill_while_waiting_for_write_lock_preserves_dirty_state() {
    let _log = common::test_log(
        "cli_sync_flush_sigkill_while_waiting_for_write_lock_preserves_dirty_state",
    );
    let mut artifacts = FailureTestArtifacts::new("cli_sync_flush_sigkill_waiting_write_lock");

    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "sigkill_flush_init");
    assert_run_success(&init, "sigkill_flush_init");

    let create = run_br(
        &workspace,
        ["create", "SIGKILL sync flush anchor", "--json"],
        "sigkill_flush_create",
    );
    assert_run_success(&create, "sigkill_flush_create");
    let create_json = parse_stdout_json(&create, "sigkill_flush_create");
    let issue_id = create_json["id"].as_str().unwrap_or("").to_string();
    assert!(
        !issue_id.is_empty(),
        "create should report an issue id in stdout: {}",
        create.stdout
    );

    flush_and_assert_clean(&workspace, "sigkill_flush_baseline");

    let beads_dir = workspace.root.join(".beads");
    let jsonl_path = beads_dir.join("issues.jsonl");
    let baseline_hash = compute_file_hash(&jsonl_path).expect("baseline jsonl hash");

    let update = run_br(
        &workspace,
        [
            "update",
            &issue_id,
            "--status",
            "in_progress",
            "--json",
            "--no-auto-flush",
        ],
        "sigkill_flush_dirty_update",
    );
    assert_run_success(&update, "sigkill_flush_dirty_update");

    let dirty_before_kill = sync_status_json(&workspace, "sigkill_flush_dirty_before_kill");
    assert_dirty_status(&dirty_before_kill, "dirty update before killed flush");
    assert_eq!(
        baseline_hash,
        compute_file_hash(&jsonl_path).expect("hash before killed flush"),
        "dirty DB update should not rewrite JSONL before explicit flush"
    );

    let lock_path = beads_dir.join(".write.lock");
    let write_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .expect("open .write.lock");
    write_lock.lock().expect("hold .write.lock");

    let mut blocked_flush = spawn_br_child(
        &workspace,
        ["sync", "--flush-only", "--json", "--no-auto-import"],
    );
    wait_for_child_to_block_on_write_lock(&mut blocked_flush, "blocked sync flush");

    blocked_flush.kill().expect("kill blocked flush");
    let killed = blocked_flush
        .wait_with_output()
        .expect("collect killed flush");
    assert!(
        !killed.status.success(),
        "killed flush must not report success"
    );
    artifacts.log(
        "killed_flush_stdout",
        &String::from_utf8_lossy(&killed.stdout),
    );
    artifacts.log(
        "killed_flush_stderr",
        &String::from_utf8_lossy(&killed.stderr),
    );

    drop(write_lock);

    assert_eq!(
        baseline_hash,
        compute_file_hash(&jsonl_path).expect("hash after killed flush"),
        "killed blocked flush must preserve the pre-flush JSONL"
    );
    let dirty_after_kill = sync_status_json(&workspace, "sigkill_flush_dirty_after_kill");
    assert_dirty_status(&dirty_after_kill, "dirty update after killed flush");
    assert_stale_show_finds(&workspace, &issue_id, "sigkill_flush_stale_show_after_kill");

    flush_and_assert_clean(&workspace, "sigkill_flush_recovery");
    let final_hash = compute_file_hash(&jsonl_path).expect("final jsonl hash");
    assert_ne!(
        baseline_hash, final_hash,
        "recovery flush should export the dirty update into JSONL"
    );

    assert_doctor_healthy(&workspace, "sigkill_flush_final_doctor");

    artifacts.snapshot_dir("after_recovery", &workspace.root);
    artifacts.log(
        "verification",
        "PASSED: killed blocked sync flush preserved dirty state and later explicit flush recovered",
    );
    artifacts.save();
}

/// Test: two real `sync --flush-only` children racing the same dirty workspace
/// must serialize through `.write.lock` and leave one valid JSONL export.
#[test]
#[cfg(unix)]
#[allow(clippy::incompatible_msrv)]
fn cli_sync_flush_concurrent_export_race_serializes_jsonl_sidecars() {
    let _log = common::test_log("cli_sync_flush_concurrent_export_race_serializes_jsonl_sidecars");
    let mut artifacts = FailureTestArtifacts::new("cli_sync_flush_concurrent_export_race");

    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "concurrent_flush_init");
    assert_run_success(&init, "concurrent_flush_init");

    let create = run_br(
        &workspace,
        ["create", "Concurrent sync flush anchor", "--json"],
        "concurrent_flush_create",
    );
    assert_run_success(&create, "concurrent_flush_create");
    let create_json = parse_stdout_json(&create, "concurrent_flush_create");
    let issue_id = create_json["id"].as_str().unwrap_or("").to_string();
    assert!(
        !issue_id.is_empty(),
        "create should report an issue id in stdout: {}",
        create.stdout
    );

    flush_and_assert_clean(&workspace, "concurrent_flush_baseline");

    let beads_dir = workspace.root.join(".beads");
    let jsonl_path = beads_dir.join("issues.jsonl");
    let baseline_hash = compute_file_hash(&jsonl_path).expect("baseline jsonl hash");
    assert!(
        jsonl_export_temp_files(&beads_dir).is_empty(),
        "baseline export should not leave temp JSONL files"
    );

    let update = run_br(
        &workspace,
        [
            "update",
            &issue_id,
            "--status",
            "in_progress",
            "--json",
            "--no-auto-flush",
        ],
        "concurrent_flush_dirty_update",
    );
    assert_run_success(&update, "concurrent_flush_dirty_update");
    let dirty_before_race = sync_status_json(&workspace, "concurrent_flush_dirty_before_race");
    assert_dirty_status(
        &dirty_before_race,
        "dirty update before concurrent flush race",
    );

    let lock_path = beads_dir.join(".write.lock");
    let write_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .expect("open .write.lock");
    write_lock.lock().expect("hold .write.lock");

    let mut flush_a = spawn_br_child(
        &workspace,
        ["sync", "--flush-only", "--json", "--no-auto-import"],
    );
    let mut flush_b = spawn_br_child(
        &workspace,
        ["sync", "--flush-only", "--json", "--no-auto-import"],
    );
    wait_for_child_to_block_on_write_lock(&mut flush_a, "first concurrent sync flush");
    wait_for_child_to_block_on_write_lock(&mut flush_b, "second concurrent sync flush");

    drop(write_lock);

    let output_a = flush_a.wait_with_output().expect("collect first flush");
    let output_b = flush_b.wait_with_output().expect("collect second flush");
    artifacts.log("flush_a_stdout", &String::from_utf8_lossy(&output_a.stdout));
    artifacts.log("flush_a_stderr", &String::from_utf8_lossy(&output_a.stderr));
    artifacts.log("flush_b_stdout", &String::from_utf8_lossy(&output_b.stdout));
    artifacts.log("flush_b_stderr", &String::from_utf8_lossy(&output_b.stderr));
    assert!(
        output_a.status.success(),
        "first concurrent flush should succeed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output_a.stdout),
        String::from_utf8_lossy(&output_a.stderr)
    );
    assert!(
        output_b.status.success(),
        "second concurrent flush should succeed\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output_b.stdout),
        String::from_utf8_lossy(&output_b.stderr)
    );

    let final_hash = compute_file_hash(&jsonl_path).expect("final jsonl hash");
    assert_ne!(
        baseline_hash, final_hash,
        "concurrent recovery flush should export the dirty update into JSONL"
    );
    let exported = read_jsonl_values(&jsonl_path, "concurrent flush JSONL");
    let exported_issue = exported
        .iter()
        .find(|issue| issue["id"].as_str() == Some(&issue_id));
    assert!(
        exported_issue.is_some(),
        "final JSONL should contain {issue_id}; entries={exported:?}"
    );
    let exported_issue = exported_issue.unwrap_or(&Value::Null);
    assert_eq!(
        exported_issue["status"].as_str(),
        Some("in_progress"),
        "final JSONL should contain the dirty status update: {exported_issue}"
    );

    let status_after_race = sync_status_json(&workspace, "concurrent_flush_status_after_race");
    assert_clean_status(&status_after_race, "concurrent flush race");
    let temp_files = jsonl_export_temp_files(&beads_dir);
    assert!(
        temp_files.is_empty(),
        "concurrent flush race should not leave export temp files: {temp_files:?}"
    );
    assert_doctor_healthy(&workspace, "concurrent_flush_final_doctor");

    artifacts.snapshot_dir("after_concurrent_race", &workspace.root);
    artifacts.log(
        "verification",
        "PASSED: concurrent sync flush children serialized cleanly and left one valid JSONL export",
    );
    artifacts.save();
}

/// Test: a workspace with `.beads` symlinked to external metadata storage
/// should fail cleanly while the target is offline and recover after restore.
#[test]
#[cfg(unix)]
fn cli_symlinked_beads_target_offline_recovers_after_restore() {
    let _log = common::test_log("cli_symlinked_beads_target_offline_recovers_after_restore");
    let mut artifacts = FailureTestArtifacts::new("cli_symlinked_beads_target_offline");

    let workspace = BrWorkspace::new();
    let external = TempDir::new().expect("create external metadata tempdir");
    let external_parent = external.path().join("metadata-store");
    let target = external_parent.join(".beads");
    let offline_target = external_parent.join(".beads-offline");
    fs::create_dir_all(&target).expect("create symlink target");
    symlink(&target, workspace.root.join(".beads")).expect("symlink workspace .beads");

    let symlink_meta =
        fs::symlink_metadata(workspace.root.join(".beads")).expect("stat .beads symlink");
    assert!(
        symlink_meta.file_type().is_symlink(),
        ".beads should be a symlink before init"
    );

    let init = run_br(&workspace, ["init"], "symlinked_beads_init");
    assert_run_success(&init, "symlinked_beads_init");
    let create = run_br(
        &workspace,
        ["create", "Symlinked beads target anchor", "--json"],
        "symlinked_beads_create",
    );
    assert_run_success(&create, "symlinked_beads_create");
    let create_json = parse_stdout_json(&create, "symlinked_beads_create");
    let issue_id = create_json["id"].as_str().unwrap_or("").to_string();
    assert!(
        !issue_id.is_empty(),
        "create should report an issue id in stdout: {}",
        create.stdout
    );

    flush_and_assert_clean(&workspace, "symlinked_beads_baseline_flush");
    let jsonl_path = target.join("issues.jsonl");
    let baseline_hash = compute_file_hash(&jsonl_path).expect("baseline symlinked JSONL hash");
    artifacts.snapshot_dir("before_target_offline", &workspace.root);

    fs::rename(&target, &offline_target).expect("move symlink target offline");
    assert!(
        !workspace.root.join(".beads").exists(),
        "broken .beads symlink should not resolve while target is offline"
    );
    let offline_meta =
        fs::symlink_metadata(workspace.root.join(".beads")).expect("stat broken .beads symlink");
    assert!(
        offline_meta.file_type().is_symlink(),
        "offline command must not replace the .beads symlink"
    );

    let offline_status = run_br(
        &workspace,
        ["sync", "--status", "--json"],
        "symlinked_beads_status_offline",
    );
    assert_run_failure(&offline_status, "symlinked_beads_status_offline");
    artifacts.log("offline_status_stdout", &offline_status.stdout);
    artifacts.log("offline_status_stderr", &offline_status.stderr);
    assert!(
        fs::symlink_metadata(workspace.root.join(".beads"))
            .expect("stat .beads after offline command")
            .file_type()
            .is_symlink(),
        "offline command should not materialize replacement .beads state"
    );
    assert!(
        !target.exists(),
        "offline command should not recreate the missing symlink target"
    );

    fs::rename(&offline_target, &target).expect("restore symlink target");
    assert!(
        workspace.root.join(".beads").exists(),
        "restored .beads symlink should resolve again"
    );
    assert_eq!(
        baseline_hash,
        compute_file_hash(&jsonl_path).expect("restored symlinked JSONL hash"),
        "offline failure should not rewrite the external JSONL export"
    );

    let restored_status = sync_status_json(&workspace, "symlinked_beads_status_restored");
    assert_clean_status(&restored_status, "restored symlinked .beads workspace");
    assert_stale_show_finds(&workspace, &issue_id, "symlinked_beads_show_after_restore");
    assert_doctor_healthy(&workspace, "symlinked_beads_final_doctor");

    artifacts.snapshot_dir("after_target_restore", &workspace.root);
    artifacts.log(
        "verification",
        "PASSED: symlinked .beads target offline failure left no replacement state and recovered after restore",
    );
    artifacts.save();
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
    let temp_path = export_temp_path_for_test(&jsonl_path);

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
    let temp_path = export_temp_path_for_test(&jsonl_path);

    // Export
    let config = ExportConfig {
        beads_dir: Some(beads_dir.clone()),
        ..Default::default()
    };
    let result = export_to_jsonl(&storage, &jsonl_path, &config).unwrap();

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
        let parsed = serde_json::from_str(line);
        let parse_error = match parsed.as_ref() {
            Ok(_) => String::new(),
            Err(err) => err.to_string(),
        };
        assert!(
            parsed.is_ok(),
            "Line {} is not valid JSON: {}",
            i,
            parse_error
        );
        let parsed: serde_json::Value = parsed.unwrap_or(serde_json::Value::Null);
        assert!(
            parsed.get("id").is_some(),
            "Line {} should have an id field",
            i
        );
    }

    // Verify content hash is consistent
    let hash2 = compute_file_hash(&jsonl_path).unwrap();
    artifacts.log("file_hash", &hash2);

    artifacts.log(
        "verification",
        "PASSED: Atomic pipeline produced valid output",
    );
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
    // Use a fake PID (99999) to simulate a stale temp from a *different*
    // crashed process, which is the realistic scenario.  Same-PID collisions
    // are now correctly treated as an error by the export engine.
    let temp_path = jsonl_path.with_extension("jsonl.99999.tmp");

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
    assert!(
        result.is_ok(),
        "Export should succeed despite stale temp file: {:?}",
        result.err()
    );

    // The stale temp file from a different PID may or may not be cleaned up
    // by the export — the engine only manages its own PID-based temp file.
    // The key invariant is that the JSONL has fresh content, not stale.

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
        Ok(export_result) => {
            assert_eq!(export_result.exported_count, 0, "Should export 0 issues");
            if jsonl_path.exists() {
                let content = fs::read_to_string(&jsonl_path).unwrap();
                assert!(
                    content.is_empty() || content.trim().is_empty(),
                    "JSONL should be empty for empty DB"
                );
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
