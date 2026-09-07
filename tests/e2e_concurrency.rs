//! E2E tests for `SQLite` lock handling and concurrency semantics.
//!
//! Validates:
//! - Lock contention with overlapping write operations
//! - --lock-timeout behavior and proper error codes
//! - Concurrent read-only operations succeed
//!
//! Related: beads_rust-uahy

mod common;

use assert_cmd::Command;
use beads_rust::franken_sync::Connection;
use common::dataset_registry::{DatasetRegistry, IsolatedDataset, KnownDataset};
use fsqlite_types::SqliteValue;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::path::Path;
use std::path::PathBuf;
use std::process::{Command as StdCommand, Stdio};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

#[cfg(target_os = "linux")]
const WRITE_LOCK_WAIT_OBSERVATION_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(target_os = "linux")]
const WRITE_LOCK_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(25);
const CONTENTION_SUCCESS_LOCK_TIMEOUT_MS: &str = "1000";

/// Result of running a br command.
#[derive(Debug)]
struct BrResult {
    stdout: String,
    stderr: String,
    success: bool,
    exit_code: Option<i32>,
    _duration: Duration,
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

fn clear_inherited_br_env(cmd: &mut Command) {
    for (key, _) in std::env::vars_os() {
        if should_clear_inherited_br_env(&key) {
            cmd.env_remove(&key);
        }
    }
}

fn clear_inherited_br_env_std(cmd: &mut StdCommand) {
    for (key, _) in std::env::vars_os() {
        if should_clear_inherited_br_env(&key) {
            cmd.env_remove(&key);
        }
    }
}

fn isolated_temp_dir(label: &str) -> TempDir {
    TempDir::new_in(common::cli::isolated_temp_root())
        .unwrap_or_else(|error| panic!("create {label}: {error}"))
}

fn spawn_br_child_in_dir<I, S>(root: &Path, args: I) -> std::process::Child
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut cmd = StdCommand::new(assert_cmd::cargo::cargo_bin!("br"));
    cmd.current_dir(root);
    cmd.args(args);
    clear_inherited_br_env_std(&mut cmd);
    cmd.env("NO_COLOR", "1");
    cmd.env("RUST_LOG", "error");
    cmd.env("RUST_BACKTRACE", "1");
    cmd.env("HOME", root);
    // Hermetic $PATH: dual `br` installs otherwise trip the br_path_dupes
    // doctor warning inside spawned doctor runs (beads_rust-ozdh class).
    cmd.env("PATH", common::cli::deduplicated_br_path());
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
    let channel = channel.to_ascii_lowercase();
    channel.contains("lock")
        || channel.contains("flock")
        // blocking_write_lock_with_timeout uses bounded try_lock polling.
        // While contended, Linux commonly reports the waiter in the sleep
        // between polls rather than inside flock/lock_file_wait.
        || channel.contains("nanosleep")
        || channel.contains("hrtimer")
}

fn wait_for_child_to_block_on_write_lock(child: &mut std::process::Child, label: &str) {
    #[cfg(target_os = "linux")]
    {
        let deadline = Instant::now() + WRITE_LOCK_WAIT_OBSERVATION_TIMEOUT;

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
                Instant::now() < deadline,
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

/// Run br command in a specific directory.
fn run_br_in_dir<I, S>(root: &PathBuf, args: I) -> BrResult
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_br_in_dir_with_env(root, args, std::iter::empty::<(String, String)>())
}

/// Run br command in a specific directory with environment overrides.
fn run_br_in_dir_with_env<I, S, E, K, V>(root: &PathBuf, args: I, env_vars: E) -> BrResult
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
    E: IntoIterator<Item = (K, V)>,
    K: AsRef<OsStr>,
    V: AsRef<OsStr>,
{
    let start = Instant::now();
    let mut cmd = Command::cargo_bin("br").expect("find br binary");
    cmd.current_dir(root);
    cmd.args(args);
    clear_inherited_br_env(&mut cmd);
    // Hermetic defaults; explicit env_vars below may override RUST_LOG.
    cmd.env("RUST_LOG", "error");
    cmd.env("PATH", common::cli::deduplicated_br_path());
    cmd.envs(env_vars);
    cmd.env("NO_COLOR", "1");
    cmd.env("RUST_BACKTRACE", "1");
    cmd.env("HOME", root);

    let output = cmd.output().expect("run br");
    let duration = start.elapsed();

    BrResult {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        success: output.status.success(),
        exit_code: output.status.code(),
        _duration: duration,
    }
}

/// Helper to parse created issue ID from stdout.
fn parse_created_id(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or("");
    // Handle both formats: "Created bd-xxx: title" and "✓ Created bd-xxx: title"
    let normalized = line.strip_prefix("✓ ").unwrap_or(line);
    normalized
        .strip_prefix("Created ")
        .and_then(|rest| rest.split(':').next())
        .unwrap_or("")
        .trim()
        .to_string()
}

fn is_expected_contention_failure(result: &BrResult) -> bool {
    let combined = format!("{} {}", result.stdout, result.stderr).to_lowercase();
    !result.success
        && (combined.contains("busy")
            || combined.contains("locked")
            || combined.contains("lock timeout")
            || combined.contains("timed out")
            || combined.contains("sync conflict")
            || combined.contains("jsonl is newer")
            || combined.contains("schema has changed"))
        && !combined.contains("malformed")
        && !combined.contains("corrupt")
        && !combined.contains("constraint")
        && !combined.contains("unexpected token")
        && !combined.contains("panic")
}

fn has_integrity_failure_signal(result: &BrResult) -> bool {
    if contains_integrity_failure_signal(&result.stderr) {
        return true;
    }

    !result.success && contains_integrity_failure_signal(&result.stdout)
}

fn contains_integrity_failure_signal(output: &str) -> bool {
    let output = output.to_lowercase();
    output.contains("unique constraint failed: blocked_issues_cache.issue_id")
        || output.contains("constraint failed")
        || output.contains("constraint")
        || output.contains("corrupt")
        || output.contains("malformed")
        || output.contains("unexpected token")
        || output.contains("panic")
}

fn assert_no_integrity_failure_signals(role: &str, results: &[BrResult]) {
    let mut integrity_failures = Vec::new();

    for (index, result) in results.iter().enumerate() {
        if has_integrity_failure_signal(result) {
            integrity_failures.push(format!(
                "{role}[{index}] stdout={} stderr={}",
                result.stdout, result.stderr
            ));
        }
    }

    assert!(
        integrity_failures.is_empty(),
        "integrity failure signals detected in {role}: {}",
        integrity_failures.join(" | ")
    );
}

fn assert_only_success_or_contention(role: &str, results: &[BrResult]) -> usize {
    let mut success_count = 0;
    let mut unexpected_failures = Vec::new();

    for (index, result) in results.iter().enumerate() {
        if result.success {
            success_count += 1;
        } else if !is_expected_contention_failure(result) {
            unexpected_failures.push(format!(
                "{role}[{index}] stdout={} stderr={}",
                result.stdout, result.stderr
            ));
        }
    }

    assert!(
        unexpected_failures.is_empty(),
        "unexpected {role} failures: {}",
        unexpected_failures.join(" | ")
    );

    success_count
}

fn issue_title_count(root: &Path, title: &str) -> i64 {
    let db_path = root.join(".beads").join("beads.db");
    let conn = Connection::open(db_path.to_string_lossy().into_owned()).expect("open beads db");
    let rows = conn
        .query_with_params(
            "SELECT COUNT(*) FROM issues WHERE title = ?",
            &[SqliteValue::from(title)],
        )
        .expect("count issue title");

    rows.first()
        .and_then(|row| row.get(0))
        .and_then(SqliteValue::as_integer)
        .expect("issue COUNT(*) must return one integer row")
}

/// Extract JSON payload from stdout (skip non-JSON preamble).
fn extract_json_payload(stdout: &str) -> String {
    for (idx, line) in stdout.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('[') || trimmed.starts_with('{') {
            return stdout
                .lines()
                .skip(idx)
                .collect::<Vec<_>>()
                .join("\n")
                .trim()
                .to_string();
        }
    }
    stdout.trim().to_string()
}

/// Parse issues list from `br list --json` stdout, handling both the legacy
/// plain-array format and the current paginated envelope format:
/// `{"issues": [...], "total": N, "limit": N, "offset": 0, "has_more": false}`.
fn extract_issues_array(stdout: &str) -> Vec<serde_json::Value> {
    let payload = extract_json_payload(stdout);
    // Try plain array first (legacy / future-proof).
    if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&payload) {
        return arr;
    }
    // Try paginated envelope.
    if let Ok(obj) = serde_json::from_str::<serde_json::Value>(&payload)
        && let Some(issues) = obj.get("issues").and_then(|v| v.as_array())
    {
        return issues.clone();
    }
    Vec::new()
}

/// Assert that `br doctor` reports the workspace as healthy.
///
/// If the initial check fails with only recoverable storage-layer issues, this
/// runs `doctor --repair`, which checkpoints the WAL and reconciles derived
/// state. `doctor --repair` exits non-zero only when post-repair verification
/// still fails, so a successful repair exit code means the workspace is clean.
/// Unrecoverable failures surface the original report.
fn assert_doctor_healthy(root: &PathBuf) {
    let doctor = run_br_in_dir(root, ["doctor", "--json"]);
    if doctor.success {
        return;
    }
    // Attempt auto-repair (checkpoint WAL, quarantine anomalous sidecars).
    let repair = run_br_in_dir(root, ["doctor", "--repair", "--json"]);
    assert!(
        repair.success,
        "doctor failed after contention and --repair could not recover it:\n\
         initial: stdout={} stderr={}\n\
         repair:  stdout={} stderr={}",
        doctor.stdout, doctor.stderr, repair.stdout, repair.stderr
    );
}

fn assert_doctor_has_no_page_anomalies(root: &PathBuf, label: &str) {
    let doctor = run_br_in_dir(root, ["doctor", "--json"]);
    assert!(
        doctor.success,
        "{label}: doctor failed: stdout={} stderr={}",
        doctor.stdout, doctor.stderr
    );

    let payload = extract_json_payload(&doctor.stdout);
    let report: serde_json::Value =
        serde_json::from_str(&payload).expect("doctor output should be valid json");
    let checks = report
        .get("checks")
        .and_then(serde_json::Value::as_array)
        .expect("doctor report should include checks array");

    let page_anomalies: Vec<String> = checks
        .iter()
        .filter_map(|check| {
            let name = check
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            if name != "sqlite.integrity_check" && name != "sqlite3.integrity_check" {
                return None;
            }

            let message = check
                .get("message")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let lower = message.to_ascii_lowercase();
            (lower.contains("never used")
                || lower.contains("free space corruption")
                || lower.contains("malformed")
                || lower.contains("disk image"))
            .then(|| format!("{name}: {message}"))
        })
        .collect();

    assert!(
        page_anomalies.is_empty(),
        "{label}: doctor reported page anomalies: {page_anomalies:?}\nstdout={}\nstderr={}",
        doctor.stdout,
        doctor.stderr
    );
}

fn assert_upstream_sqlite_integrity_ok(root: &Path, label: &str) {
    let db_path = root.join(".beads").join("beads.db");
    let output = StdCommand::new("sqlite3")
        .arg(&db_path)
        .arg("PRAGMA integrity_check;")
        .output();

    let output = match output {
        Ok(output) => output,
        Err(err) => {
            eprintln!("{label}: sqlite3 unavailable, skipping upstream integrity check: {err}");
            return;
        }
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.trim() == "ok",
        "{label}: upstream sqlite3 integrity_check failed for {}: status={:?} stdout={stdout} stderr={stderr}",
        db_path.display(),
        output.status.code()
    );
}

fn create_routes_file(root: &Path, entries: &[(&str, &Path)]) {
    let routes_path = root.join(".beads").join("routes.jsonl");
    let content = entries
        .iter()
        .map(|(prefix, path)| {
            format!(
                r#"{{"prefix":"{prefix}","path":"{}"}}"#,
                path.to_string_lossy()
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(routes_path, content).expect("write routes.jsonl");
}

fn configure_external_route(main_root: &Path, external_root: &Path) {
    fs::write(
        external_root.join(".beads").join("config.yaml"),
        "issue_prefix: ext\n",
    )
    .expect("write external config");
    create_routes_file(main_root, &[("ext-", external_root)]);
}

/// A writer process killed while waiting for `.write.lock` must not leave a
/// ghost mutation or poison the advisory lock for subsequent writers.
#[test]
#[allow(clippy::incompatible_msrv)]
fn e2e_killed_writer_waiting_on_write_lock_does_not_poison_workspace() {
    let _log =
        common::test_log("e2e_killed_writer_waiting_on_write_lock_does_not_poison_workspace");

    let temp_dir = isolated_temp_dir("temp dir");
    let root = temp_dir.path().to_path_buf();

    let init = run_br_in_dir(&root, ["init"]);
    assert!(init.success, "init failed: {}", init.stderr);

    let seed = run_br_in_dir(&root, ["create", "Seed before killed writer"]);
    assert!(seed.success, "seed create failed: {}", seed.stderr);

    let lock_path = root.join(".beads").join(".write.lock");
    let write_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .expect("open .write.lock");
    write_lock.lock().expect("hold .write.lock");

    let mut blocked_writer = spawn_br_child_in_dir(
        &root,
        ["create", "Killed while waiting for write lock", "--json"],
    );
    wait_for_child_to_block_on_write_lock(&mut blocked_writer, "writer create");

    blocked_writer.kill().expect("kill blocked writer");
    let killed = blocked_writer
        .wait_with_output()
        .expect("collect killed writer");
    assert!(
        !killed.status.success(),
        "killed writer must not report success"
    );
    drop(write_lock);

    let after = run_br_in_dir(&root, ["create", "After killed writer", "--json"]);
    assert!(
        after.success,
        "post-kill writer failed: stdout={} stderr={}",
        after.stdout, after.stderr
    );

    let list = run_br_in_dir(&root, ["--no-auto-import", "list", "--json"]);
    assert!(
        list.success,
        "list after killed writer failed: {}",
        list.stderr
    );
    let issues = extract_issues_array(&list.stdout);
    assert!(
        issues
            .iter()
            .any(|issue| issue["title"].as_str() == Some("Seed before killed writer")),
        "seed issue should remain visible: {}",
        list.stdout
    );
    assert!(
        issues
            .iter()
            .any(|issue| issue["title"].as_str() == Some("After killed writer")),
        "post-kill issue should be visible: {}",
        list.stdout
    );
    assert!(
        issues.iter().all(|issue| {
            issue["title"].as_str() != Some("Killed while waiting for write lock")
        }),
        "killed waiter must not create a ghost issue: {}",
        list.stdout
    );

    assert_doctor_healthy(&root);
}

/// A broken `.write.lock` path must fail closed. Mutating commands must not
/// bypass cross-process serialization just because the advisory lock cannot be
/// opened.
#[test]
#[cfg(unix)]
fn e2e_mutating_command_fails_when_write_lock_path_unusable() {
    let _log = common::test_log("e2e_mutating_command_fails_when_write_lock_path_unusable");

    let temp_dir = isolated_temp_dir("temp dir");
    let root = temp_dir.path().to_path_buf();

    let init = run_br_in_dir(&root, ["init"]);
    assert!(init.success, "init failed: {}", init.stderr);

    let lock_path = root.join(".beads").join(".write.lock");
    fs::create_dir_all(&lock_path).expect("replace write lock path with directory");

    let create = run_br_in_dir(
        &root,
        ["create", "Should not bypass broken write lock", "--json"],
    );
    assert!(
        !create.success,
        "mutating command should fail when .write.lock is unusable; stdout={} stderr={}",
        create.stdout, create.stderr
    );
    let combined = format!("{}{}", create.stdout, create.stderr);
    assert!(
        (combined.contains("Refusing unsafe workspace write lock path")
            || combined.contains("Failed to open write lock"))
            && combined.contains(".write.lock"),
        "error should explain the unusable write lock path: {combined}"
    );

    assert_eq!(
        issue_title_count(&root, "Should not bypass broken write lock"),
        0,
        "failed lock acquisition must not create an issue"
    );
}

/// A held `.write.lock` must fail with the configured timeout instead of
/// parking the mutating command indefinitely.
#[test]
#[cfg(unix)]
#[allow(clippy::incompatible_msrv)]
fn e2e_write_lock_contention_respects_lock_timeout() {
    let _log = common::test_log("e2e_write_lock_contention_respects_lock_timeout");

    let temp_dir = isolated_temp_dir("temp dir");
    let root = temp_dir.path().to_path_buf();

    let init = run_br_in_dir(&root, ["init"]);
    assert!(init.success, "init failed: {}", init.stderr);

    let lock_path = root.join(".beads").join(".write.lock");
    let write_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .expect("open .write.lock");
    write_lock.lock().expect("hold .write.lock");

    let start = Instant::now();
    let create = run_br_in_dir(
        &root,
        [
            "--lock-timeout",
            "75",
            "--json",
            "create",
            "Blocked by held write lock",
        ],
    );
    let elapsed = start.elapsed();

    assert!(
        !create.success,
        "mutating command should time out while write lock is held; stdout={} stderr={}",
        create.stdout, create.stderr
    );
    assert!(
        (Duration::from_millis(75)..Duration::from_secs(3)).contains(&elapsed),
        "write lock must honor the requested budget without blocking indefinitely; elapsed={elapsed:?}"
    );
    assert_eq!(create.exit_code, Some(7), "{create:?}");
    let payload: serde_json::Value =
        serde_json::from_str(&create.stdout).expect("write-lock timeout must be whole JSON");
    assert_eq!(payload["error"]["code"], "CONFIG_ERROR", "{payload}");
    let message = payload["error"]["message"]
        .as_str()
        .expect("write-lock timeout must include an error message");
    // Routing checks consume part of the shared acquisition budget before
    // the workspace lock is attempted, so its remaining timeout can be <75ms.
    let remaining_timeout_ms = message
        .strip_prefix("Configuration error: Timed out after ")
        .and_then(|rest| rest.split_once("ms waiting for write lock (workspace write lock) at "))
        .map(|(timeout, _)| timeout.parse::<u64>().expect("numeric timeout in ms"))
        .expect("workspace write-lock timeout diagnostic");
    assert!(
        remaining_timeout_ms <= 75 && message.contains(".beads/.write.lock"),
        "error should include bounded write-lock diagnostics: {message}"
    );

    drop(write_lock);
    assert_eq!(
        issue_title_count(&root, "Blocked by held write lock"),
        0,
        "timed-out command must not create an issue"
    );
    let after = run_br_in_dir(&root, ["create", "After write lock timeout", "--json"]);
    assert!(
        after.success,
        "workspace should accept writes after lock release: stdout={} stderr={}",
        after.stdout, after.stderr
    );
    assert_eq!(
        issue_title_count(&root, "After write lock timeout"),
        1,
        "write after lock release must persist exactly one issue"
    );
}

/// Flat doctor surfaces must classify a genuinely held advisory lock before
/// live inspection, without using inode age or recommending inode replacement.
#[test]
#[cfg(unix)]
#[allow(clippy::incompatible_msrv)]
#[allow(clippy::too_many_lines)]
fn e2e_doctor_reports_live_write_lock_without_mutating_workspace() {
    use std::os::unix::fs::MetadataExt;

    let _log = common::test_log("e2e_doctor_reports_live_write_lock_without_mutating_workspace");
    let temp_dir = isolated_temp_dir("temp dir");
    let root = temp_dir.path().to_path_buf();

    let init = run_br_in_dir(&root, ["init"]);
    assert!(init.success, "init failed: {}", init.stderr);
    let seed = run_br_in_dir(&root, ["create", "Lock evidence seed", "--json"]);
    assert!(seed.success, "seed failed: {}", seed.stderr);

    let lock_path = root.join(".beads/.write.lock");
    let db_path = root.join(".beads/beads.db");
    let jsonl_path = root.join(".beads/issues.jsonl");
    let db_before = fs::read(&db_path).expect("read database before contention");
    let jsonl_before = fs::read(&jsonl_path).expect("read JSONL before contention");
    let inode_before = fs::metadata(&lock_path).expect("stat lock").ino();

    let write_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .expect("open .write.lock");
    write_lock.lock().expect("hold .write.lock");

    for (args, robot_triage) in [
        (vec!["--lock-timeout", "0", "doctor", "--json"], false),
        (
            vec!["--lock-timeout", "0", "doctor", "--robot-triage"],
            true,
        ),
        (
            vec![
                "--lock-timeout",
                "0",
                "doctor",
                "--robot-triage",
                "--repair",
            ],
            true,
        ),
        (
            vec![
                "--lock-timeout",
                "0",
                "doctor",
                "--robot-triage",
                "--repair-indexes",
            ],
            true,
        ),
    ] {
        let doctor = run_br_in_dir(&root, args);
        assert!(
            !doctor.success,
            "doctor must not inspect through a live owner: stdout={} stderr={}",
            doctor.stdout, doctor.stderr
        );
        assert_eq!(
            doctor.exit_code,
            Some(5),
            "process status must agree with the typed payload: stdout={} stderr={}",
            doctor.stdout,
            doctor.stderr
        );
        let payload: serde_json::Value =
            serde_json::from_str(&doctor.stdout).expect("typed doctor startup JSON");
        assert_eq!(payload["exit_code"], 5, "{payload}");
        assert_eq!(payload["code"], "concurrency_lost", "{payload}");
        assert_eq!(payload["inspection_state"], "not_started", "{payload}");
        assert!(
            payload.get("workspace_health").is_none(),
            "uninspected workspace must not receive a health value: {payload}"
        );
        if robot_triage {
            assert_eq!(
                payload["schema_version"], "br.doctor.triage.v1",
                "{payload}"
            );
            for key in [
                "summary",
                "findings",
                "actions_planned",
                "recommended_command",
                "capabilities_url",
                "robot_docs_command",
                "quick_ref",
            ] {
                assert!(
                    payload.get(key).is_some(),
                    "robot triage contract is missing {key}: {payload}"
                );
            }
            assert_eq!(payload["quick_ref"]["healthy"], 0, "{payload}");
            assert_eq!(payload["quick_ref"]["warn"], 1, "{payload}");
            assert_eq!(payload["quick_ref"]["error"], 0, "{payload}");
            assert_eq!(
                payload["findings"][0]["id"], "fm-concurrency_primitives-orphaned-write-lock",
                "{payload}"
            );
            assert_eq!(payload["reason"], "live_owner", "{payload}");
        } else {
            assert_eq!(payload["checks"][0]["name"], "write_lock", "{payload}");
            assert_eq!(
                payload["checks"][0]["details"]["reason"], "live_owner",
                "{payload}"
            );
            assert_eq!(
                payload["checks"][0]["details"]["finding_id"],
                "fm-concurrency_primitives-orphaned-write-lock",
                "{payload}"
            );
            assert!(
                payload["checks"][0]["details"]["remediation"]
                    .as_str()
                    .is_some_and(|message| message.contains("do not move or delete")),
                "{payload}"
            );
        }
        assert_eq!(
            fs::metadata(&lock_path)
                .expect("stat lock after doctor")
                .ino(),
            inode_before,
            "doctor must not replace the lock inode"
        );
        assert_eq!(
            fs::read(&db_path).expect("read database after contention"),
            db_before,
            "doctor must not inspect or mutate the database after lock refusal"
        );
        assert_eq!(
            fs::read(&jsonl_path).expect("read JSONL after contention"),
            jsonl_before,
            "doctor must not mutate JSONL after lock refusal"
        );
    }

    drop(write_lock);
    let recovery = run_br_in_dir(&root, ["doctor", "--json"]);
    assert!(
        matches!(recovery.exit_code, Some(0 | 1)),
        "doctor should resume inspection after owner release without a hard failure: \
         exit={:?} stdout={} stderr={}",
        recovery.exit_code,
        recovery.stdout,
        recovery.stderr
    );
    let payload: serde_json::Value =
        serde_json::from_str(&recovery.stdout).expect("recovery doctor JSON");
    let lock_check = payload["checks"]
        .as_array()
        .and_then(|checks| checks.iter().find(|check| check["name"] == "write_lock"))
        .expect("write_lock check after recovery");
    assert_eq!(lock_check["status"], "ok", "{lock_check}");
    assert_eq!(
        lock_check["details"]["reason"], "persistent_advisory_inode",
        "{lock_check}"
    );
}

/// Auto-import runs before nominally read-only commands, but the import itself
/// mutates SQLite. It must therefore serialize through `.write.lock` just like
/// explicit write commands.
#[test]
#[cfg(unix)]
#[allow(clippy::incompatible_msrv)]
fn e2e_read_command_auto_import_waits_for_write_lock() {
    let _log = common::test_log("e2e_read_command_auto_import_waits_for_write_lock");

    let temp_dir = isolated_temp_dir("temp dir");
    let root = temp_dir.path().to_path_buf();

    let init = run_br_in_dir(&root, ["init"]);
    assert!(init.success, "init failed: {}", init.stderr);

    let seed = run_br_in_dir(&root, ["create", "Seed before auto-import"]);
    assert!(seed.success, "seed create failed: {}", seed.stderr);

    let flush = run_br_in_dir(&root, ["sync", "--flush-only"]);
    assert!(flush.success, "flush failed: {}", flush.stderr);

    let beads_dir = root.join(".beads");
    let jsonl_path = beads_dir.join("issues.jsonl");
    let jsonl = fs::read_to_string(&jsonl_path).expect("read issues jsonl");
    let mut issue: serde_json::Value = serde_json::from_str(jsonl.trim()).expect("parse issue");
    issue["title"] = serde_json::Value::String("Imported while waiting for write lock".to_string());
    issue["updated_at"] = serde_json::Value::String("2999-01-01T00:00:00Z".to_string());
    fs::write(
        &jsonl_path,
        format!(
            "{}\n",
            serde_json::to_string(&issue).expect("serialize modified issue")
        ),
    )
    .expect("write stale jsonl");

    let lock_path = beads_dir.join(".write.lock");
    let write_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .expect("open .write.lock");
    write_lock.lock().expect("hold .write.lock");

    let mut blocked_list = spawn_br_child_in_dir(&root, ["list", "--json"]);
    wait_for_child_to_block_on_write_lock(&mut blocked_list, "auto-import list");

    blocked_list.kill().expect("kill blocked list");
    let killed = blocked_list
        .wait_with_output()
        .expect("collect killed list");
    assert!(
        !killed.status.success(),
        "killed auto-import waiter must not report success"
    );
    drop(write_lock);

    let list = run_br_in_dir(&root, ["list", "--json"]);
    assert!(
        list.success,
        "list after releasing write lock failed: {}",
        list.stderr
    );
    let issues = extract_issues_array(&list.stdout);
    assert!(
        issues.iter().any(|issue| {
            issue["title"].as_str() == Some("Imported while waiting for write lock")
        }),
        "later list should import the preserved JSONL update: {}",
        list.stdout
    );
}

/// Refreshing a stale JSONL witness is a SQLite metadata write even when the
/// JSONL itself is not newer. Read commands must serialize that path too.
#[test]
#[cfg(unix)]
#[allow(clippy::incompatible_msrv)]
fn e2e_read_command_witness_refresh_waits_for_write_lock() {
    let _log = common::test_log("e2e_read_command_witness_refresh_waits_for_write_lock");

    let temp_dir = isolated_temp_dir("temp dir");
    let root = temp_dir.path().to_path_buf();

    let init = run_br_in_dir(&root, ["init"]);
    assert!(init.success, "init failed: {}", init.stderr);

    let seed = run_br_in_dir(&root, ["create", "Seed before witness refresh"]);
    assert!(seed.success, "seed create failed: {}", seed.stderr);

    let flush = run_br_in_dir(&root, ["sync", "--flush-only"]);
    assert!(flush.success, "flush failed: {}", flush.stderr);

    let beads_dir = root.join(".beads");
    let db_path = beads_dir.join("beads.db");
    let conn = Connection::open(db_path.to_string_lossy().into_owned()).expect("open beads db");
    conn.execute("DELETE FROM metadata WHERE key = 'jsonl_size'")
        .expect("delete jsonl_size witness");
    conn.execute("INSERT INTO metadata (key, value) VALUES ('jsonl_size', '0')")
        .expect("write stale jsonl_size witness");
    // beads_rust-mjmk: also corrupt jsonl_content_hash so the staleness probe
    // actually concludes the JSONL is newer. compute_jsonl_newer_impl falls
    // back to hash comparison when size mismatches; if the hash still matches
    // the actual JSONL, the probe returns "not newer" and the read command
    // never tries to refresh witnesses, making this test a no-op.
    conn.execute("DELETE FROM metadata WHERE key = 'jsonl_content_hash'")
        .expect("delete jsonl_content_hash witness");
    conn.execute(
        "INSERT INTO metadata (key, value) VALUES ('jsonl_content_hash', 'stale_witness_hash_mjmk')",
    )
    .expect("write stale jsonl_content_hash witness");
    drop(conn);

    let lock_path = beads_dir.join(".write.lock");
    let write_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)
        .expect("open .write.lock");
    write_lock.lock().expect("hold .write.lock");

    let mut blocked_search = spawn_br_child_in_dir(&root, ["search", "Seed", "--json"]);
    wait_for_child_to_block_on_write_lock(&mut blocked_search, "witness-refresh search");

    drop(write_lock);
    let completed = blocked_search
        .wait_with_output()
        .expect("collect search after lock release");
    assert!(
        completed.status.success(),
        "search after witness refresh failed: stdout={} stderr={}",
        String::from_utf8_lossy(&completed.stdout),
        String::from_utf8_lossy(&completed.stderr)
    );
}

/// Test that concurrent write operations respect `SQLite` locking.
///
/// This test:
/// 1. Starts two threads that attempt to create issues simultaneously
/// 2. Uses a barrier to synchronize the start of both operations
/// 3. Verifies that both eventually succeed (due to default busy timeout)
#[test]
fn e2e_concurrent_writes_succeed_with_retry() {
    let _log = common::test_log("e2e_concurrent_writes_succeed_with_retry");

    // Create workspace
    let temp_dir = isolated_temp_dir("temp dir");
    let root = temp_dir.path().to_path_buf();

    // Initialize workspace
    let init = run_br_in_dir(&root, ["init"]);
    assert!(init.success, "init failed: {}", init.stderr);

    // Create a barrier to synchronize thread start
    let barrier = Arc::new(Barrier::new(2));
    let root1 = Arc::new(root.clone());
    let root2 = Arc::new(root.clone());

    let barrier1 = Arc::clone(&barrier);
    let barrier2 = Arc::clone(&barrier);
    let root1_clone = Arc::clone(&root1);
    let root2_clone = Arc::clone(&root2);

    // Spawn two threads that will try to create issues concurrently.
    // Use an explicit timeout here so the retry behavior is stable under
    // remote worker load instead of depending on the ambient default.
    let handle1 = thread::spawn(move || {
        barrier1.wait();
        run_br_in_dir(
            &root1_clone,
            ["--lock-timeout", "1000", "create", "Issue from thread 1"],
        )
    });

    let handle2 = thread::spawn(move || {
        barrier2.wait();
        run_br_in_dir(
            &root2_clone,
            ["--lock-timeout", "1000", "create", "Issue from thread 2"],
        )
    });

    let result1 = handle1.join().expect("thread 1 panicked");
    let result2 = handle2.join().expect("thread 2 panicked");

    let mut success_count = 0;
    let mut successful_titles = Vec::new();
    let mut unexpected_failures = Vec::new();
    for (index, result, title) in [
        (1, &result1, "Issue from thread 1"),
        (2, &result2, "Issue from thread 2"),
    ] {
        if result.success {
            success_count += 1;
            successful_titles.push(title);
        } else if !is_expected_contention_failure(result) {
            unexpected_failures.push(format!(
                "thread {index} stdout={} stderr={}",
                result.stdout, result.stderr
            ));
        }
    }
    assert!(
        unexpected_failures.is_empty(),
        "unexpected concurrent write failures: {}",
        unexpected_failures.join(" | ")
    );
    assert!(
        success_count > 0,
        "expected at least one concurrent writer to succeed"
    );

    // Verify successful issues were created. Use --no-auto-import to avoid
    // SYNC_CONFLICT when JSONL is newer than the DB after concurrent flushes.
    let list = run_br_in_dir(&root, ["--no-auto-import", "list", "--json"]);
    assert!(list.success, "list failed: {}", list.stderr);
    let issues = extract_issues_array(&list.stdout);
    assert!(
        issues.len() >= success_count,
        "expected at least {success_count} concurrent issues, got {}",
        issues.len()
    );
    for title in successful_titles {
        assert!(list.stdout.contains(title), "missing issue title {title}");
    }

    // Keep temp_dir alive until end
    drop(temp_dir);
}

/// Test that --lock-timeout=1 causes quick failure on lock contention.
///
/// This test:
/// 1. Holds a write lock via rapid updates
/// 2. Attempts a second write with --lock-timeout=1
/// 3. Measures timing to verify timeout behavior
#[test]
fn e2e_lock_timeout_behavior() {
    let _log = common::test_log("e2e_lock_timeout_behavior");

    let temp_dir = isolated_temp_dir("temp dir");
    let root = temp_dir.path().to_path_buf();

    // Initialize workspace
    let init = run_br_in_dir(&root, ["init"]);
    assert!(init.success, "init failed: {}", init.stderr);

    // Create an issue first
    let create = run_br_in_dir(&root, ["create", "Seed issue"]);
    assert!(create.success, "create seed failed: {}", create.stderr);
    let seed_id = parse_created_id(&create.stdout);

    // Use a synchronization primitive
    let barrier = Arc::new(Barrier::new(2));
    let root_shared = Arc::new(root);
    let seed_id_arc = Arc::new(seed_id);

    let barrier1 = Arc::clone(&barrier);
    let barrier2 = Arc::clone(&barrier);
    let root1_clone = Arc::clone(&root_shared);
    let root2_clone = Arc::clone(&root_shared);
    let seed_id_clone = Arc::clone(&seed_id_arc);

    // Thread 1: Do multiple rapid updates to keep the DB busy
    let handle1 = thread::spawn(move || {
        barrier1.wait();
        for i in 0..10 {
            let title = format!("Update {i}");
            run_br_in_dir(&root1_clone, ["update", &seed_id_clone, "--title", &title]);
            thread::sleep(Duration::from_millis(50));
        }
    });

    // Thread 2: Try to create with low timeout
    let handle2 = thread::spawn(move || {
        barrier2.wait();
        // Small delay to let the first thread start
        thread::sleep(Duration::from_millis(25));
        let start = Instant::now();
        let result = run_br_in_dir(
            &root2_clone,
            ["--lock-timeout", "1", "create", "Low timeout issue"],
        );
        let elapsed = start.elapsed();
        (result, elapsed)
    });

    handle1.join().expect("thread 1 panicked");
    let (result2, elapsed2) = handle2.join().expect("thread 2 panicked");

    // Log timing for diagnostics
    eprintln!(
        "Low timeout operation: success={}, elapsed={elapsed2:?}",
        result2.success
    );

    // Either outcome is valid depending on timing:
    // - Success if no contention was hit
    // - Failure with lock/busy error if contention occurred
    if !result2.success {
        let combined = format!("{} {}", result2.stderr, result2.stdout).to_lowercase();
        // Check for any database-related error (busy, lock, or general database error)
        assert!(
            combined.contains("busy")
                || combined.contains("lock")
                || combined.contains("database")
                || combined.contains("error"),
            "expected lock-related error, got: stdout={}, stderr={}",
            result2.stdout,
            result2.stderr
        );
    }

    drop(temp_dir);
}

/// Test that read-only operations succeed concurrently without blocking.
///
/// This test:
/// 1. Creates several issues
/// 2. Runs multiple concurrent read operations (list, show, stats)
/// 3. Verifies all complete successfully
#[test]
fn e2e_concurrent_reads_succeed() {
    let _log = common::test_log("e2e_concurrent_reads_succeed");

    let temp_dir = isolated_temp_dir("temp dir");
    let root = temp_dir.path().to_path_buf();

    // Initialize and create some issues
    let init = run_br_in_dir(&root, ["init"]);
    assert!(init.success, "init failed: {}", init.stderr);

    let mut ids = Vec::new();
    for i in 0..5 {
        let create = run_br_in_dir(&root, ["create", &format!("Issue {i}")]);
        assert!(create.success, "create {i} failed: {}", create.stderr);
        ids.push(parse_created_id(&create.stdout));
    }

    // Spawn multiple threads doing read operations
    let barrier = Arc::new(Barrier::new(5));
    let mut handles = Vec::new();

    let root_arc = Arc::new(root);
    for (i, issue_id) in ids.iter().cloned().enumerate() {
        let root_clone = Arc::clone(&root_arc);
        let barrier_clone = Arc::clone(&barrier);

        let handle = thread::spawn(move || {
            barrier_clone.wait();
            let start = Instant::now();

            // Mix of read operations
            let list = run_br_in_dir(&root_clone, ["list", "--json"]);
            let show = run_br_in_dir(&root_clone, ["show", &issue_id, "--json"]);
            let stats = run_br_in_dir(&root_clone, ["stats", "--json"]);

            let elapsed = start.elapsed();
            (i, list, show, stats, elapsed)
        });

        handles.push(handle);
    }

    // Collect results
    let results: Vec<_> = handles
        .into_iter()
        .map(|h| h.join().expect("thread panicked"))
        .collect();

    // All read operations should succeed
    for (i, list, show, stats, elapsed) in &results {
        assert!(list.success, "thread {i} list failed: {}", list.stderr);
        assert!(show.success, "thread {i} show failed: {}", show.stderr);
        assert!(stats.success, "thread {i} stats failed: {}", stats.stderr);
        eprintln!("Thread {i} completed reads in {elapsed:?}");
    }

    drop(temp_dir);
}

/// Test that parallel read-only commands serialize without teardown errors.
///
/// Read-only DB-family commands intentionally pass through `.write.lock` because
/// storage open/recovery can touch shared DB state before the command body runs.
/// This guards against the failure mode we actually care about: hidden
/// write-like teardown work surfacing as `database is busy` or corrupting the
/// workspace under concurrent read traffic.
#[test]
fn e2e_parallel_read_only_commands_serialize_without_busy_on_drop() {
    let _log = common::test_log("e2e_parallel_read_only_commands_serialize_without_busy_on_drop");

    let registry = DatasetRegistry::new();
    if !registry.is_available(KnownDataset::BeadsRust) {
        eprintln!("skipping: beads_rust dataset is unavailable in this environment");
        return;
    }

    let isolated =
        IsolatedDataset::from_dataset(KnownDataset::BeadsRust).expect("copy beads_rust dataset");
    isolated
        .migrate_to_current_schema()
        .expect("migrate isolated beads_rust dataset");
    let root = isolated.root.clone();

    let create = run_br_in_dir(
        &root,
        [
            "--no-auto-import",
            "--no-auto-flush",
            "create",
            "Concurrency seed issue",
        ],
    );
    assert!(create.success, "seed create failed: {}", create.stderr);
    let issue_id = parse_created_id(&create.stdout);

    let root_arc = Arc::new(root);
    let barrier = Arc::new(Barrier::new(6));
    let mut handles = Vec::new();

    for worker in 0..6 {
        let root_clone = Arc::clone(&root_arc);
        let barrier_clone = Arc::clone(&barrier);
        let issue_id_clone = issue_id.clone();

        handles.push(thread::spawn(move || {
            barrier_clone.wait();

            let mut failures = Vec::new();
            for iteration in 0..6 {
                let result = if worker % 2 == 0 {
                    run_br_in_dir(
                        &root_clone,
                        [
                            "--lock-timeout",
                            "1000",
                            "--no-auto-import",
                            "--no-auto-flush",
                            "ready",
                            "--json",
                        ],
                    )
                } else {
                    run_br_in_dir(
                        &root_clone,
                        [
                            "--lock-timeout",
                            "1000",
                            "--no-auto-import",
                            "--no-auto-flush",
                            "show",
                            &issue_id_clone,
                            "--json",
                        ],
                    )
                };

                if !result.success {
                    failures.push(format!(
                        "iteration={iteration} stdout={} stderr={}",
                        result.stdout, result.stderr
                    ));
                    break;
                }
            }

            (worker, failures)
        }));
    }

    for handle in handles {
        let (worker, failures) = handle.join().expect("thread panicked");
        assert!(
            failures.is_empty(),
            "worker {worker} hit read-only contention: {}",
            failures.join(" | ")
        );
    }

    drop(isolated);
}

/// Test that lock timeout is properly respected with specific timing.
///
/// This test:
/// 1. Sets a specific lock timeout
/// 2. Verifies the operation completes within expected time (no contention)
#[test]
fn e2e_lock_timeout_timing() {
    let _log = common::test_log("e2e_lock_timeout_timing");

    let temp_dir = isolated_temp_dir("temp dir");
    let root = temp_dir.path().to_path_buf();

    // Initialize workspace
    let init = run_br_in_dir(&root, ["init"]);
    assert!(init.success, "init failed: {}", init.stderr);

    // Create a seed issue
    let create = run_br_in_dir(&root, ["create", "Seed"]);
    assert!(create.success, "create failed: {}", create.stderr);

    // Test with a 500ms timeout (should complete quickly without contention)
    let timeout_ms = 500;
    let start = Instant::now();
    let result = run_br_in_dir(
        &root,
        ["--lock-timeout", &timeout_ms.to_string(), "list", "--json"],
    );
    let elapsed = start.elapsed();

    // Without contention, should complete very quickly
    assert!(result.success, "list failed: {}", result.stderr);
    let timeout_ms_u64 = u64::try_from(timeout_ms).unwrap_or(0);
    assert!(
        elapsed < Duration::from_millis(timeout_ms_u64 + 500),
        "operation took too long without contention: {elapsed:?}"
    );

    eprintln!("Lock timeout timing test: elapsed={elapsed:?} (timeout={timeout_ms}ms)");

    drop(temp_dir);
}

/// Test that writes serialize properly and eventually complete.
///
/// This test verifies the proper serialization of write operations.
#[test]
fn e2e_write_serialization() {
    let _log = common::test_log("e2e_write_serialization");

    let temp_dir = isolated_temp_dir("temp dir");
    let root = temp_dir.path().to_path_buf();

    // Initialize
    let init = run_br_in_dir(&root, ["init"]);
    assert!(init.success, "init failed: {}", init.stderr);

    let start = Instant::now();
    let mut handles = Vec::new();
    let barrier = Arc::new(Barrier::new(3));

    // Spawn 3 threads doing writes
    for i in 0..3 {
        let root_clone = Arc::new(root.clone());
        let barrier_clone = Arc::clone(&barrier);

        let handle = thread::spawn(move || {
            barrier_clone.wait();
            let thread_start = Instant::now();
            let result = run_br_in_dir(
                &root_clone,
                [
                    "--lock-timeout",
                    "1000",
                    "create",
                    &format!("Serialized issue {i}"),
                ],
            );
            let thread_elapsed = thread_start.elapsed();
            (i, result, thread_elapsed)
        });

        handles.push(handle);
    }

    let results: Vec<_> = handles
        .into_iter()
        .map(|h| h.join().expect("thread panicked"))
        .collect();
    let total_elapsed = start.elapsed();

    let mut success_count = 0;
    let mut successful_indices = Vec::new();
    let mut unexpected_failures = Vec::new();

    for (i, result, elapsed) in &results {
        if result.success {
            success_count += 1;
            successful_indices.push(*i);
            eprintln!("Thread {i} took {elapsed:?}");
        } else if !is_expected_contention_failure(result) {
            unexpected_failures.push(format!(
                "thread {i} stdout={} stderr={}",
                result.stdout, result.stderr
            ));
        }
    }

    assert!(
        unexpected_failures.is_empty(),
        "unexpected serialized writer failures: {}",
        unexpected_failures.join(" | ")
    );
    assert!(
        success_count > 0,
        "expected at least one serialized write to complete"
    );

    eprintln!("Total time for 3 serialized writes: {total_elapsed:?}");

    // Verify all successful writes persist. Use --no-auto-import to avoid
    // SYNC_CONFLICT when concurrent flushes leave JSONL ahead of DB.
    let list = run_br_in_dir(&root, ["--no-auto-import", "list", "--json"]);
    assert!(list.success, "final list failed: {}", list.stderr);
    let issues = extract_issues_array(&list.stdout);
    assert!(
        issues.len() >= success_count,
        "expected at least {success_count} serialized issues, got {}",
        issues.len()
    );
    for i in successful_indices {
        assert!(
            list.stdout.contains(&format!("Serialized issue {i}")),
            "missing serialized issue {i}"
        );
    }

    drop(temp_dir);
}

/// Test mixed read-write concurrency.
///
/// This test:
/// 1. Has some threads doing writes
/// 2. Has other threads doing reads
/// 3. Verifies reads complete and writes eventually complete
#[test]
fn e2e_mixed_read_write_concurrency() {
    let _log = common::test_log("e2e_mixed_read_write_concurrency");

    let temp_dir = isolated_temp_dir("temp dir");
    let root = temp_dir.path().to_path_buf();

    // Initialize with some existing data
    let init = run_br_in_dir(&root, ["init"]);
    assert!(init.success, "init failed: {}", init.stderr);

    for i in 0..3 {
        let create = run_br_in_dir(&root, ["create", &format!("Existing issue {i}")]);
        assert!(create.success, "create {i} failed");
    }

    let barrier = Arc::new(Barrier::new(6)); // 3 readers + 3 writers
    let mut handles = Vec::new();

    // Spawn readers
    for i in 0..3 {
        let root_clone = Arc::new(root.clone());
        let barrier_clone = Arc::clone(&barrier);

        let handle = thread::spawn(move || {
            barrier_clone.wait();
            let start = Instant::now();
            // This reader deliberately opts out of both startup mutations, so
            // it exercises the current-schema read-only connection instead of
            // competing with writers for the workspace authority.
            let result = run_br_in_dir(
                &root_clone,
                [
                    "--no-auto-import",
                    "--no-auto-flush",
                    "--lock-timeout",
                    "500",
                    "list",
                    "--json",
                ],
            );
            let elapsed = start.elapsed();
            ("reader", i, result, elapsed)
        });
        handles.push(handle);
    }

    // Spawn writers
    for i in 0..3 {
        let root_clone = Arc::new(root.clone());
        let barrier_clone = Arc::clone(&barrier);

        let handle = thread::spawn(move || {
            barrier_clone.wait();
            let start = Instant::now();
            let result = run_br_in_dir(&root_clone, ["create", &format!("New issue {i}")]);
            let elapsed = start.elapsed();
            ("writer", i, result, elapsed)
        });
        handles.push(handle);
    }

    let results: Vec<_> = handles
        .into_iter()
        .map(|h| h.join().expect("thread panicked"))
        .collect();

    let mut reader_results = Vec::new();
    let mut writer_results = Vec::new();
    for (role, i, result, elapsed) in results {
        eprintln!("{role} {i} completed in {elapsed:?}");
        if role == "reader" {
            reader_results.push(result);
        } else {
            writer_results.push(result);
        }
    }

    let reader_successes = assert_only_success_or_contention("reader", &reader_results);
    let writer_successes = assert_only_success_or_contention("writer", &writer_results);

    assert_eq!(
        reader_successes,
        reader_results.len(),
        "read-only fast-open readers must all bypass mixed writer contention"
    );
    assert!(
        writer_successes > 0,
        "expected at least one successful writer under mixed contention"
    );

    // Verify final state. Use --no-auto-import to avoid SYNC_CONFLICT when
    // JSONL is newer than the DB after concurrent flushes.
    let list = run_br_in_dir(&root, ["--no-auto-import", "list", "--json"]);
    assert!(list.success, "final list failed: {}", list.stderr);

    // All successful writers should persist; explicit contention failures are acceptable.
    let issues = extract_issues_array(&list.stdout);
    assert!(
        issues.len() >= 3 + writer_successes,
        "expected at least {} issues, got {}",
        3 + writer_successes,
        issues.len()
    );

    drop(temp_dir);
}

/// Test that mixed mutating command families either succeed or fail explicitly
/// under contention, while the workspace remains readable afterward.
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_interleaved_command_families_remain_bounded() {
    let _log = common::test_log("e2e_interleaved_command_families_remain_bounded");

    let temp_dir = isolated_temp_dir("temp dir");
    let root = temp_dir.path().to_path_buf();

    let init = run_br_in_dir(&root, ["init"]);
    assert!(init.success, "init failed: {}", init.stderr);

    let create = run_br_in_dir(&root, ["create", "Interleaved seed issue"]);
    assert!(create.success, "create seed failed: {}", create.stderr);
    let seed_id = parse_created_id(&create.stdout);

    let barrier = Arc::new(Barrier::new(4));
    let root_arc = Arc::new(root.clone());
    let seed_id_arc = Arc::new(seed_id);

    let create_handle = {
        let barrier = Arc::clone(&barrier);
        let root = Arc::clone(&root_arc);
        thread::spawn(move || {
            barrier.wait();
            let mut results = Vec::new();
            for idx in 0..4 {
                results.push(run_br_in_dir(
                    &root,
                    [
                        "--lock-timeout",
                        "1",
                        "create",
                        &format!("Interleaved issue {idx}"),
                    ],
                ));
                thread::sleep(Duration::from_millis(10));
            }
            results
        })
    };

    let update_handle = {
        let barrier = Arc::clone(&barrier);
        let root = Arc::clone(&root_arc);
        let seed_id = Arc::clone(&seed_id_arc);
        thread::spawn(move || {
            barrier.wait();
            let mut results = Vec::new();
            for idx in 0..4 {
                let title = format!("Interleaved title {idx}");
                results.push(run_br_in_dir(
                    &root,
                    ["--lock-timeout", "1", "update", &seed_id, "--title", &title],
                ));
                thread::sleep(Duration::from_millis(10));
            }
            results
        })
    };

    let label_handle = {
        let barrier = Arc::clone(&barrier);
        let root = Arc::clone(&root_arc);
        let seed_id = Arc::clone(&seed_id_arc);
        thread::spawn(move || {
            barrier.wait();
            let mut results = Vec::new();
            for idx in 0..4 {
                let label = format!("lane-{idx}");
                results.push(run_br_in_dir(
                    &root,
                    ["--lock-timeout", "1", "label", "add", &seed_id, &label],
                ));
                thread::sleep(Duration::from_millis(10));
            }
            results
        })
    };

    let comments_handle = {
        let barrier = Arc::clone(&barrier);
        let root = Arc::clone(&root_arc);
        let seed_id = Arc::clone(&seed_id_arc);
        thread::spawn(move || {
            barrier.wait();
            let mut results = Vec::new();
            for idx in 0..4 {
                let body = format!("bounded comment {idx}");
                results.push(run_br_in_dir(
                    &root,
                    ["--lock-timeout", "1", "comments", "add", &seed_id, &body],
                ));
                thread::sleep(Duration::from_millis(10));
            }
            results
        })
    };

    let worker_results = [
        (
            "create",
            create_handle.join().expect("create worker panicked"),
        ),
        (
            "update",
            update_handle.join().expect("update worker panicked"),
        ),
        ("label", label_handle.join().expect("label worker panicked")),
        (
            "comments",
            comments_handle.join().expect("comments worker panicked"),
        ),
    ];

    let total_successes: usize = worker_results
        .iter()
        .map(|(_, results)| results.iter().filter(|result| result.success).count())
        .sum();
    assert!(
        total_successes > 0,
        "expected at least one successful mutation across interleaved workers"
    );

    for (worker, results) in &worker_results {
        let _ = assert_only_success_or_contention(worker, results);
    }

    // Use --no-auto-import for post-contention reads to avoid SYNC_CONFLICT
    // when concurrent flushes leave JSONL ahead of the DB.
    let show = run_br_in_dir(&root, ["--no-auto-import", "show", &seed_id_arc, "--json"]);
    assert!(
        show.success,
        "show after contention failed: {}",
        show.stderr
    );

    let list = run_br_in_dir(&root, ["--no-auto-import", "list", "--json"]);
    assert!(
        list.success,
        "list after contention failed: {}",
        list.stderr
    );

    let stats = run_br_in_dir(&root, ["--no-auto-import", "stats", "--json"]);
    assert!(
        stats.success,
        "stats after contention failed: {}",
        stats.stderr
    );
}

/// Test that routed access to an external workspace remains available while the
/// invoking workspace is under local mutation.
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_routed_external_mutation_succeeds_during_local_updates() {
    let _log = common::test_log("e2e_routed_external_mutation_succeeds_during_local_updates");

    let main_temp = isolated_temp_dir("main temp dir");
    let external_temp = isolated_temp_dir("external temp dir");
    let main_root = main_temp.path().to_path_buf();
    let external_root = external_temp.path().to_path_buf();

    let init_main = run_br_in_dir(&main_root, ["init"]);
    assert!(init_main.success, "init main failed: {}", init_main.stderr);
    let init_external = run_br_in_dir(&external_root, ["init"]);
    assert!(
        init_external.success,
        "init external failed: {}",
        init_external.stderr
    );

    configure_external_route(&main_root, &external_root);

    let create_local = run_br_in_dir(&main_root, ["create", "Local issue under mutation"]);
    assert!(
        create_local.success,
        "create local failed: {}",
        create_local.stderr
    );
    let local_id = parse_created_id(&create_local.stdout);

    let create_external = run_br_in_dir(&external_root, ["create", "External routed issue"]);
    assert!(
        create_external.success,
        "create external failed: {}",
        create_external.stderr
    );
    let external_id = parse_created_id(&create_external.stdout);
    assert!(
        external_id.starts_with("ext-"),
        "expected external prefix, got {external_id}"
    );

    let barrier = Arc::new(Barrier::new(2));
    let main_root_arc = Arc::new(main_root.clone());
    let local_id_arc = Arc::new(local_id);
    let external_id_arc = Arc::new(external_id);

    let local_updates = {
        let barrier = Arc::clone(&barrier);
        let main_root = Arc::clone(&main_root_arc);
        let local_id = Arc::clone(&local_id_arc);
        thread::spawn(move || {
            barrier.wait();
            let mut results = Vec::new();
            for idx in 0..8 {
                let title = format!("Local routed contention title {idx}");
                results.push(run_br_in_dir(
                    &main_root,
                    [
                        "--lock-timeout",
                        CONTENTION_SUCCESS_LOCK_TIMEOUT_MS,
                        "update",
                        &local_id,
                        "--title",
                        &title,
                    ],
                ));
                thread::sleep(Duration::from_millis(10));
            }
            results
        })
    };

    let routed_comments = {
        let barrier = Arc::clone(&barrier);
        let main_root = Arc::clone(&main_root_arc);
        let external_id = Arc::clone(&external_id_arc);
        thread::spawn(move || {
            barrier.wait();
            let mut results = Vec::new();
            for idx in 0..8 {
                let body = format!("routed external comment {idx}");
                results.push(run_br_in_dir(
                    &main_root,
                    [
                        "--lock-timeout",
                        CONTENTION_SUCCESS_LOCK_TIMEOUT_MS,
                        "comments",
                        "add",
                        &external_id,
                        &body,
                        "--json",
                    ],
                ));
                thread::sleep(Duration::from_millis(10));
            }
            results
        })
    };

    let local_update_results = local_updates.join().expect("local updates panicked");
    let routed_comment_results = routed_comments.join().expect("routed comments panicked");

    let local_update_successes =
        assert_only_success_or_contention("local_routed_updates", &local_update_results);
    assert!(
        local_update_successes > 0,
        "local mutation worker never succeeded"
    );

    let routed_comment_successes =
        assert_only_success_or_contention("routed_external_comments", &routed_comment_results);
    assert!(
        routed_comment_successes > 0,
        "expected at least one successful routed external comment"
    );

    let show_external = run_br_in_dir(&main_root, ["show", &external_id_arc, "--json"]);
    assert!(
        show_external.success,
        "routed show after contention failed: {}",
        show_external.stderr
    );
    let payload = extract_json_payload(&show_external.stdout);
    let issues: Vec<serde_json::Value> = serde_json::from_str(&payload).expect("parse show json");
    let comments = issues[0]["comments"].as_array().expect("comments array");
    assert!(
        comments.len() >= routed_comment_successes,
        "expected at least {} routed comments to persist, got {}",
        routed_comment_successes,
        comments.len()
    );

    let show_local = run_br_in_dir(&main_root, ["show", &local_id_arc, "--json"]);
    assert!(
        show_local.success,
        "local show after routed mutation failed: {}",
        show_local.stderr
    );
}

/// Test that background sync-status checks touching `.beads/` remain readable
/// while mutating commands are auto-flushing JSONL.
#[test]
fn e2e_sync_status_observer_stays_available_during_writes() {
    let _log = common::test_log("e2e_sync_status_observer_stays_available_during_writes");

    let temp_dir = isolated_temp_dir("temp dir");
    let root = temp_dir.path().to_path_buf();

    let init = run_br_in_dir(&root, ["init"]);
    assert!(init.success, "init failed: {}", init.stderr);

    let barrier = Arc::new(Barrier::new(2));
    let root_arc = Arc::new(root.clone());

    let writer = {
        let barrier = Arc::clone(&barrier);
        let root = Arc::clone(&root_arc);
        thread::spawn(move || {
            barrier.wait();
            let mut results = Vec::new();
            for idx in 0..6 {
                results.push(run_br_in_dir(
                    &root,
                    ["create", &format!("background observer issue {idx}")],
                ));
                thread::sleep(Duration::from_millis(10));
            }
            results
        })
    };

    let observer = {
        let barrier = Arc::clone(&barrier);
        let root = Arc::clone(&root_arc);
        thread::spawn(move || {
            barrier.wait();
            let mut results = Vec::new();
            for _ in 0..6 {
                results.push(run_br_in_dir(
                    &root,
                    [
                        "--lock-timeout",
                        CONTENTION_SUCCESS_LOCK_TIMEOUT_MS,
                        "sync",
                        "--status",
                        "--json",
                    ],
                ));
                thread::sleep(Duration::from_millis(10));
            }
            results
        })
    };

    let writer_results = writer.join().expect("writer panicked");
    let observer_results = observer.join().expect("observer panicked");

    for (idx, result) in writer_results.iter().enumerate() {
        assert!(
            result.success,
            "writer iteration {idx} failed: {}",
            result.stderr
        );
    }

    let observer_successes = assert_only_success_or_contention("sync_status", &observer_results);
    assert!(
        observer_successes > 0,
        "expected at least one successful sync --status observation"
    );

    let list = run_br_in_dir(&root, ["list", "--json"]);
    assert!(list.success, "final list failed: {}", list.stderr);
    let issues = extract_issues_array(&list.stdout);
    assert_eq!(issues.len(), 6, "expected all writer issues to persist");
}

/// Test that database locked errors are properly reported.
///
/// This test verifies that when a lock cannot be acquired within the timeout,
/// an appropriate error message is returned.
#[test]
fn e2e_lock_error_reporting() {
    let _log = common::test_log("e2e_lock_error_reporting");

    let temp_dir = isolated_temp_dir("temp dir");
    let root = temp_dir.path().to_path_buf();

    // Initialize
    let init = run_br_in_dir(&root, ["init"]);
    assert!(init.success, "init failed: {}", init.stderr);

    // Create a seed issue
    let create = run_br_in_dir(&root, ["create", "Lock test issue"]);
    assert!(create.success, "create failed: {}", create.stderr);

    // Normal operation should report no lock issues
    let list = run_br_in_dir(&root, ["list", "--json"]);
    assert!(list.success, "list failed: {}", list.stderr);
    assert!(
        !list.stderr.to_lowercase().contains("lock"),
        "unexpected lock message in normal operation"
    );

    drop(temp_dir);
}

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_interleaved_command_families_preserve_workspace_integrity() {
    let _log = common::test_log("e2e_interleaved_command_families_preserve_workspace_integrity");

    let temp_dir = isolated_temp_dir("temp dir");
    let root = temp_dir.path().to_path_buf();

    let init = run_br_in_dir(&root, ["init"]);
    assert!(init.success, "init failed: {}", init.stderr);

    let seed = run_br_in_dir(&root, ["create", "Concurrency seed issue"]);
    assert!(seed.success, "seed create failed: {}", seed.stderr);
    let issue_id = parse_created_id(&seed.stdout);
    assert!(!issue_id.is_empty(), "missing seed issue id");

    let barrier = Arc::new(Barrier::new(4));
    let shared_root = Arc::new(root.clone());
    let shared_issue_id = Arc::new(issue_id.clone());

    let create_root = Arc::clone(&shared_root);
    let create_barrier = Arc::clone(&barrier);
    let creator = thread::spawn(move || {
        create_barrier.wait();
        let mut results = Vec::new();
        for i in 0..6 {
            let args = vec![
                "--lock-timeout".to_string(),
                CONTENTION_SUCCESS_LOCK_TIMEOUT_MS.to_string(),
                "create".to_string(),
                format!("Agent-created issue {i}"),
            ];
            results.push(run_br_in_dir(&create_root, args));
            thread::sleep(Duration::from_millis(10));
        }
        results
    });

    let comment_root = Arc::clone(&shared_root);
    let comment_issue_id = Arc::clone(&shared_issue_id);
    let comment_barrier = Arc::clone(&barrier);
    let commenter = thread::spawn(move || {
        comment_barrier.wait();
        let mut results = Vec::new();
        for i in 0..6 {
            // Use --no-auto-import to avoid SYNC_CONFLICT when concurrent creates
            // have updated the JSONL but left dirty flags in the DB. Comments add
            // does not need to sync from JSONL before appending a comment.
            let args = vec![
                "--no-auto-import".to_string(),
                "--lock-timeout".to_string(),
                CONTENTION_SUCCESS_LOCK_TIMEOUT_MS.to_string(),
                "comments".to_string(),
                "add".to_string(),
                comment_issue_id.as_ref().clone(),
                format!("agent-note-{i}"),
            ];
            results.push(run_br_in_dir(&comment_root, args));
            thread::sleep(Duration::from_millis(10));
        }
        results
    });

    let label_root = Arc::clone(&shared_root);
    let label_issue_id = Arc::clone(&shared_issue_id);
    let label_barrier = Arc::clone(&barrier);
    let labeler = thread::spawn(move || {
        label_barrier.wait();
        let mut results = Vec::new();
        for i in 0..6 {
            // Use --no-auto-import to avoid SYNC_CONFLICT during concurrent creates.
            let args = vec![
                "--no-auto-import".to_string(),
                "--lock-timeout".to_string(),
                CONTENTION_SUCCESS_LOCK_TIMEOUT_MS.to_string(),
                "label".to_string(),
                "add".to_string(),
                label_issue_id.as_ref().clone(),
                format!("contended-{i}"),
            ];
            results.push(run_br_in_dir(&label_root, args));
            thread::sleep(Duration::from_millis(10));
        }
        results
    });

    let reader_root = Arc::clone(&shared_root);
    let reader_issue_id = Arc::clone(&shared_issue_id);
    let reader_barrier = Arc::clone(&barrier);
    let reader = thread::spawn(move || {
        reader_barrier.wait();
        let mut results = Vec::new();
        for i in 0..12 {
            let args = match i % 3 {
                0 => vec![
                    "--lock-timeout".to_string(),
                    CONTENTION_SUCCESS_LOCK_TIMEOUT_MS.to_string(),
                    "list".to_string(),
                    "--json".to_string(),
                ],
                1 => vec![
                    "--lock-timeout".to_string(),
                    CONTENTION_SUCCESS_LOCK_TIMEOUT_MS.to_string(),
                    "show".to_string(),
                    reader_issue_id.as_ref().clone(),
                    "--json".to_string(),
                ],
                _ => vec![
                    "--lock-timeout".to_string(),
                    CONTENTION_SUCCESS_LOCK_TIMEOUT_MS.to_string(),
                    "ready".to_string(),
                    "--json".to_string(),
                ],
            };
            results.push(run_br_in_dir(&reader_root, args));
            thread::sleep(Duration::from_millis(5));
        }
        results
    });

    let create_results = creator.join().expect("creator panicked");
    let comment_results = commenter.join().expect("commenter panicked");
    let label_results = labeler.join().expect("labeler panicked");
    let reader_results = reader.join().expect("reader panicked");

    let create_successes = assert_only_success_or_contention("create", &create_results);
    let comment_successes = assert_only_success_or_contention("comments", &comment_results);
    let label_successes = assert_only_success_or_contention("labels", &label_results);
    let reader_successes = assert_only_success_or_contention("reader", &reader_results);

    assert!(
        create_successes > 0,
        "expected at least one successful create"
    );
    assert!(
        comment_successes > 0,
        "expected at least one successful comment add"
    );
    assert!(
        label_successes > 0,
        "expected at least one successful label add"
    );
    assert!(
        reader_successes > 0,
        "expected at least one successful reader command"
    );

    assert_doctor_healthy(&root);

    // Use --no-auto-import to avoid SYNC_CONFLICT from concurrent flushes.
    let list = run_br_in_dir(&root, ["--no-auto-import", "list", "--json"]);
    assert!(
        list.success,
        "list failed after contention: {}",
        list.stderr
    );
    let issues = extract_issues_array(&list.stdout);
    assert!(
        issues.len() > create_successes,
        "expected at least {} issues after concurrent creates, got {}",
        1 + create_successes,
        issues.len()
    );

    let comments = run_br_in_dir(
        &root,
        ["--no-auto-import", "comments", "list", &issue_id, "--json"],
    );
    assert!(
        comments.success,
        "comments list failed after contention: {}",
        comments.stderr
    );
    let comment_json: Vec<serde_json::Value> =
        serde_json::from_str(&extract_json_payload(&comments.stdout))
            .expect("parse comments list json");
    assert!(
        comment_json.len() >= comment_successes,
        "expected at least {} comments, got {}",
        comment_successes,
        comment_json.len()
    );

    let labels = run_br_in_dir(
        &root,
        ["--no-auto-import", "label", "list", &issue_id, "--json"],
    );
    assert!(
        labels.success,
        "label list failed after contention: {}",
        labels.stderr
    );
    let label_json: Vec<String> =
        serde_json::from_str(&extract_json_payload(&labels.stdout)).expect("parse label list");
    assert!(
        label_json.len() >= label_successes,
        "expected at least {} labels, got {}",
        label_successes,
        label_json.len()
    );

    let show = run_br_in_dir(&root, ["--no-auto-import", "show", &issue_id, "--json"]);
    assert!(
        show.success,
        "show failed after contention: {}",
        show.stderr
    );

    drop(temp_dir);
}

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_external_access_and_background_status_are_bounded_during_mutation() {
    let _log =
        common::test_log("e2e_external_access_and_background_status_are_bounded_during_mutation");

    let temp_dir = isolated_temp_dir("temp dir");
    let root = temp_dir.path().to_path_buf();

    let init = run_br_in_dir(&root, ["init"]);
    assert!(init.success, "init failed: {}", init.stderr);

    let seed = run_br_in_dir(&root, ["create", "External access seed issue"]);
    assert!(seed.success, "seed create failed: {}", seed.stderr);
    let issue_id = parse_created_id(&seed.stdout);
    assert!(!issue_id.is_empty(), "missing seed issue id");

    let beads_dir = Arc::new(root.join(".beads").display().to_string());
    let external_temp_dir = isolated_temp_dir("external temp dir");
    let external_root = Arc::new(external_temp_dir.path().to_path_buf());

    let barrier = Arc::new(Barrier::new(3));
    let shared_root = Arc::new(root.clone());
    let shared_issue_id = Arc::new(issue_id);

    let writer_root = Arc::clone(&shared_root);
    let writer_barrier = Arc::clone(&barrier);
    let local_writer = thread::spawn(move || {
        writer_barrier.wait();
        let mut results = Vec::new();
        for i in 0..8 {
            let args = vec![
                "--lock-timeout".to_string(),
                CONTENTION_SUCCESS_LOCK_TIMEOUT_MS.to_string(),
                "create".to_string(),
                format!("local-mutation-{i}"),
            ];
            results.push(run_br_in_dir(&writer_root, args));
            thread::sleep(Duration::from_millis(8));
        }
        results
    });

    let read_root = Arc::clone(&external_root);
    let read_beads_dir = Arc::clone(&beads_dir);
    let read_issue_id = Arc::clone(&shared_issue_id);
    let read_barrier = Arc::clone(&barrier);
    let external_reader = thread::spawn(move || {
        read_barrier.wait();
        let mut results = Vec::new();
        for i in 0..10 {
            let args = if i % 2 == 0 {
                vec![
                    "--lock-timeout".to_string(),
                    CONTENTION_SUCCESS_LOCK_TIMEOUT_MS.to_string(),
                    "list".to_string(),
                    "--json".to_string(),
                ]
            } else {
                vec![
                    "--lock-timeout".to_string(),
                    CONTENTION_SUCCESS_LOCK_TIMEOUT_MS.to_string(),
                    "show".to_string(),
                    read_issue_id.as_ref().clone(),
                    "--json".to_string(),
                ]
            };
            results.push(run_br_in_dir_with_env(
                &read_root,
                args,
                [("BEADS_DIR", read_beads_dir.as_str())],
            ));
            thread::sleep(Duration::from_millis(6));
        }
        results
    });

    let status_root = Arc::clone(&external_root);
    let status_beads_dir = Arc::clone(&beads_dir);
    let status_barrier = Arc::clone(&barrier);
    let background_status = thread::spawn(move || {
        status_barrier.wait();
        let mut results = Vec::new();
        for _ in 0..10 {
            let args = vec![
                "--lock-timeout".to_string(),
                CONTENTION_SUCCESS_LOCK_TIMEOUT_MS.to_string(),
                "sync".to_string(),
                "--status".to_string(),
                "--json".to_string(),
            ];
            results.push(run_br_in_dir_with_env(
                &status_root,
                args,
                [("BEADS_DIR", status_beads_dir.as_str())],
            ));
            thread::sleep(Duration::from_millis(6));
        }
        results
    });

    let writer_results = local_writer.join().expect("local writer panicked");
    let reader_results = external_reader.join().expect("external reader panicked");
    let status_results = background_status
        .join()
        .expect("background status panicked");

    let writer_successes = assert_only_success_or_contention("writer", &writer_results);
    let reader_successes = assert_only_success_or_contention("external_reader", &reader_results);
    let status_successes = assert_only_success_or_contention("background_status", &status_results);

    assert!(
        writer_successes > 0,
        "expected at least one successful local write"
    );
    assert!(
        reader_successes > 0,
        "expected at least one successful external BEADS_DIR access"
    );
    assert!(
        status_successes > 0,
        "expected at least one successful background status command"
    );

    assert_doctor_healthy(&root);

    let status = run_br_in_dir(&root, ["sync", "--status", "--json"]);
    assert!(
        status.success,
        "sync --status failed after contention: stdout={} stderr={}",
        status.stdout, status.stderr
    );

    // Use --no-auto-import to avoid SYNC_CONFLICT from concurrent flushes.
    let list = run_br_in_dir(&root, ["--no-auto-import", "list", "--json"]);
    assert!(
        list.success,
        "list failed after contention: {}",
        list.stderr
    );
    let issues = extract_issues_array(&list.stdout);
    assert!(
        issues.len() > writer_successes,
        "expected at least {} issues after local mutation, got {}",
        1 + writer_successes,
        issues.len()
    );

    drop(external_temp_dir);
    drop(temp_dir);
}

/// Test that actor-aware command families like claim and defer can interleave
/// with other mutating commands while leaving the workspace readable.
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_actor_oriented_command_families_preserve_workspace_integrity() {
    let _log = common::test_log("e2e_actor_oriented_command_families_preserve_workspace_integrity");

    let temp_dir = isolated_temp_dir("temp dir");
    let root = temp_dir.path().to_path_buf();

    let init = run_br_in_dir(&root, ["init"]);
    assert!(init.success, "init failed: {}", init.stderr);

    let claim_issue = run_br_in_dir(&root, ["create", "Claim target"]);
    assert!(claim_issue.success, "create claim target failed");
    let claim_id = parse_created_id(&claim_issue.stdout);

    let defer_issue = run_br_in_dir(&root, ["create", "Deferred target"]);
    assert!(defer_issue.success, "create defer target failed");
    let defer_id = parse_created_id(&defer_issue.stdout);

    let comment_issue = run_br_in_dir(&root, ["create", "Comment target"]);
    assert!(comment_issue.success, "create comment target failed");
    let comment_id = parse_created_id(&comment_issue.stdout);

    let label_issue = run_br_in_dir(&root, ["create", "Label target"]);
    assert!(label_issue.success, "create label target failed");
    let label_id = parse_created_id(&label_issue.stdout);

    let barrier = Arc::new(Barrier::new(5));
    let shared_root = Arc::new(root.clone());

    let claimer = {
        let barrier = Arc::clone(&barrier);
        let root = Arc::clone(&shared_root);
        let claim_id = claim_id.clone();
        thread::spawn(move || {
            barrier.wait();
            let mut results = Vec::new();
            for _ in 0..8 {
                // Use --no-auto-import to avoid SYNC_CONFLICT when other concurrent
                // threads update the JSONL but leave dirty flags in the DB.
                let args = vec![
                    "--no-auto-import".to_string(),
                    "--lock-timeout".to_string(),
                    CONTENTION_SUCCESS_LOCK_TIMEOUT_MS.to_string(),
                    "--actor".to_string(),
                    "alice".to_string(),
                    "update".to_string(),
                    claim_id.clone(),
                    "--claim".to_string(),
                    "--json".to_string(),
                ];
                results.push(run_br_in_dir(&root, args));
                thread::sleep(Duration::from_millis(10));
            }
            results
        })
    };

    let deferrer = {
        let barrier = Arc::clone(&barrier);
        let root = Arc::clone(&shared_root);
        let defer_id = defer_id.clone();
        thread::spawn(move || {
            barrier.wait();
            let mut results = Vec::new();
            for _ in 0..8 {
                // Use --no-auto-import to avoid SYNC_CONFLICT during concurrent writes.
                let args = vec![
                    "--no-auto-import".to_string(),
                    "--lock-timeout".to_string(),
                    CONTENTION_SUCCESS_LOCK_TIMEOUT_MS.to_string(),
                    "--actor".to_string(),
                    "dave".to_string(),
                    "defer".to_string(),
                    defer_id.clone(),
                    "--until".to_string(),
                    "2026-12-01T00:00:00Z".to_string(),
                    "--json".to_string(),
                ];
                results.push(run_br_in_dir(&root, args));
                thread::sleep(Duration::from_millis(10));
            }
            results
        })
    };

    let commenter = {
        let barrier = Arc::clone(&barrier);
        let root = Arc::clone(&shared_root);
        let comment_id = comment_id.clone();
        thread::spawn(move || {
            barrier.wait();
            let mut results = Vec::new();
            for i in 0..8 {
                // Use --no-auto-import to avoid SYNC_CONFLICT when concurrent
                // claimer/deferrer threads update the JSONL but leave dirty flags
                // in the DB. Comment add does not need to sync from JSONL first.
                let args = vec![
                    "--no-auto-import".to_string(),
                    "--lock-timeout".to_string(),
                    CONTENTION_SUCCESS_LOCK_TIMEOUT_MS.to_string(),
                    "--actor".to_string(),
                    "carol".to_string(),
                    "comments".to_string(),
                    "add".to_string(),
                    comment_id.clone(),
                    format!("actor-note-{i}"),
                ];
                results.push(run_br_in_dir(&root, args));
                thread::sleep(Duration::from_millis(10));
            }
            results
        })
    };

    let labeler = {
        let barrier = Arc::clone(&barrier);
        let root = Arc::clone(&shared_root);
        let label_id = label_id.clone();
        thread::spawn(move || {
            barrier.wait();
            let mut results = Vec::new();
            for i in 0..8 {
                // Use --no-auto-import to avoid SYNC_CONFLICT during concurrent writes.
                let args = vec![
                    "--no-auto-import".to_string(),
                    "--lock-timeout".to_string(),
                    CONTENTION_SUCCESS_LOCK_TIMEOUT_MS.to_string(),
                    "--actor".to_string(),
                    "bob".to_string(),
                    "label".to_string(),
                    "add".to_string(),
                    label_id.clone(),
                    format!("actor-lane-{i}"),
                ];
                results.push(run_br_in_dir(&root, args));
                thread::sleep(Duration::from_millis(10));
            }
            results
        })
    };

    let reader = {
        let barrier = Arc::clone(&barrier);
        let root = Arc::clone(&shared_root);
        let claim_id = claim_id.clone();
        thread::spawn(move || {
            barrier.wait();
            let mut results = Vec::new();
            for i in 0..12 {
                let args = match i % 3 {
                    0 => vec![
                        "--lock-timeout".to_string(),
                        CONTENTION_SUCCESS_LOCK_TIMEOUT_MS.to_string(),
                        "show".to_string(),
                        claim_id.clone(),
                        "--json".to_string(),
                    ],
                    1 => vec![
                        "--lock-timeout".to_string(),
                        CONTENTION_SUCCESS_LOCK_TIMEOUT_MS.to_string(),
                        "ready".to_string(),
                        "--json".to_string(),
                    ],
                    _ => vec![
                        "--lock-timeout".to_string(),
                        CONTENTION_SUCCESS_LOCK_TIMEOUT_MS.to_string(),
                        "stats".to_string(),
                        "--json".to_string(),
                    ],
                };
                results.push(run_br_in_dir(&root, args));
                thread::sleep(Duration::from_millis(5));
            }
            results
        })
    };

    let claim_results = claimer.join().expect("claimer panicked");
    let defer_results = deferrer.join().expect("deferrer panicked");
    let comment_results = commenter.join().expect("commenter panicked");
    let label_results = labeler.join().expect("labeler panicked");
    let reader_results = reader.join().expect("reader panicked");

    let claim_successes = assert_only_success_or_contention("claim", &claim_results);
    let defer_successes = assert_only_success_or_contention("defer", &defer_results);
    let comment_successes = assert_only_success_or_contention("comments", &comment_results);
    let label_successes = assert_only_success_or_contention("labels", &label_results);
    let reader_successes = assert_only_success_or_contention("reader", &reader_results);

    assert!(
        claim_successes > 0,
        "expected at least one successful claim"
    );
    assert!(
        defer_successes > 0,
        "expected at least one successful defer"
    );
    assert!(
        comment_successes > 0,
        "expected at least one successful comment add"
    );
    assert!(
        label_successes > 0,
        "expected at least one successful label add"
    );
    assert!(
        reader_successes > 0,
        "expected at least one successful reader command"
    );

    assert_doctor_healthy(&root);

    let claim_show = run_br_in_dir(&root, ["--no-auto-import", "show", &claim_id, "--json"]);
    assert!(
        claim_show.success,
        "show claim target failed: {}",
        claim_show.stderr
    );
    let claim_json: Vec<serde_json::Value> =
        serde_json::from_str(&extract_json_payload(&claim_show.stdout)).expect("claim show json");
    assert_eq!(claim_json[0]["status"].as_str(), Some("in_progress"));
    assert_eq!(claim_json[0]["assignee"].as_str(), Some("alice"));

    // Use --no-auto-import for post-contention reads to avoid SYNC_CONFLICT.
    let defer_show = run_br_in_dir(&root, ["--no-auto-import", "show", &defer_id, "--json"]);
    assert!(
        defer_show.success,
        "show defer target failed: {}",
        defer_show.stderr
    );
    let defer_json: Vec<serde_json::Value> =
        serde_json::from_str(&extract_json_payload(&defer_show.stdout)).expect("defer show json");
    assert_eq!(defer_json[0]["status"].as_str(), Some("deferred"));
    let defer_until = defer_json[0]["defer_until"]
        .as_str()
        .expect("defer_until should be present");
    assert!(
        defer_until.starts_with("2026-12-01"),
        "unexpected defer_until value: {defer_until}"
    );

    let comments = run_br_in_dir(
        &root,
        [
            "--no-auto-import",
            "comments",
            "list",
            &comment_id,
            "--json",
        ],
    );
    assert!(
        comments.success,
        "comments list failed after actor contention: {}",
        comments.stderr
    );
    let comment_json: Vec<serde_json::Value> =
        serde_json::from_str(&extract_json_payload(&comments.stdout))
            .expect("parse comments list json");
    assert!(
        comment_json.len() >= comment_successes,
        "expected at least {} comments, got {}",
        comment_successes,
        comment_json.len()
    );
    assert!(
        comment_json
            .iter()
            .all(|comment| comment["author"].as_str() == Some("carol")),
        "expected all comment authors to be carol: {}",
        comments.stdout
    );

    let labels = run_br_in_dir(
        &root,
        ["--no-auto-import", "label", "list", &label_id, "--json"],
    );
    assert!(
        labels.success,
        "label list failed after actor contention: {}",
        labels.stderr
    );
    let label_json: Vec<String> =
        serde_json::from_str(&extract_json_payload(&labels.stdout)).expect("parse label list");
    assert!(
        label_json.len() >= label_successes,
        "expected at least {} labels, got {}",
        label_successes,
        label_json.len()
    );

    let list = run_br_in_dir(&root, ["--no-auto-import", "list", "--json"]);
    assert!(
        list.success,
        "list failed after actor contention: {}",
        list.stderr
    );
}

/// Regression for direct close/update cache-refresh integrity under contention.
///
/// The failure mode we are guarding is not ordinary lock contention; that is
/// already allowed by the test harness. What must never reappear is a
/// blocked-cache UNIQUE constraint or other corruption signal while close-style
/// status mutations interleave with update/reopen traffic.
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_close_update_reopen_preserve_blocked_cache_integrity() {
    let _log = common::test_log("e2e_close_update_reopen_preserve_blocked_cache_integrity");

    let temp_dir = isolated_temp_dir("temp dir");
    let root = temp_dir.path().to_path_buf();

    let init = run_br_in_dir(&root, ["init"]);
    assert!(init.success, "init failed: {}", init.stderr);

    let mut close_ids = Vec::new();
    for idx in 0..6 {
        let created = run_br_in_dir(&root, ["create", &format!("Close target {idx}")]);
        assert!(created.success, "create close target {idx} failed");
        close_ids.push(parse_created_id(&created.stdout));
    }

    let mut reopen_ids = Vec::new();
    for idx in 0..3 {
        let created = run_br_in_dir(&root, ["create", &format!("Reopen target {idx}")]);
        assert!(created.success, "create reopen target {idx} failed");
        let issue_id = parse_created_id(&created.stdout);
        let closed = run_br_in_dir(&root, ["close", &issue_id, "--reason", "seed closed"]);
        assert!(closed.success, "seed close {idx} failed: {}", closed.stderr);
        reopen_ids.push(issue_id);
    }

    let update_issue = run_br_in_dir(&root, ["create", "Update target"]);
    assert!(update_issue.success, "create update target failed");
    let update_id = parse_created_id(&update_issue.stdout);

    // Exercise real cache entries: each independently changing blocker has a
    // dependent that stays open throughout the concurrent workload.
    let mut dependent_pairs = Vec::new();
    for blocker in close_ids.iter().chain(&reopen_ids) {
        let created = run_br_in_dir(&root, ["create", &format!("Dependent of {blocker}")]);
        assert!(
            created.success,
            "create dependent failed: {}",
            created.stderr
        );
        let dependent = parse_created_id(&created.stdout);
        let edge = run_br_in_dir(&root, ["dep", "add", &dependent, blocker]);
        assert!(edge.success, "seed dependency failed: {}", edge.stderr);
        dependent_pairs.push((dependent, blocker.clone()));
    }

    let barrier = Arc::new(Barrier::new(4));
    let shared_root = Arc::new(root.clone());

    let closer = {
        let barrier = Arc::clone(&barrier);
        let root = Arc::clone(&shared_root);
        let close_ids = close_ids.clone();
        thread::spawn(move || {
            barrier.wait();
            let mut results = Vec::new();
            for issue_id in close_ids {
                let args = vec![
                    "--no-auto-import".to_string(),
                    "--lock-timeout".to_string(),
                    CONTENTION_SUCCESS_LOCK_TIMEOUT_MS.to_string(),
                    "close".to_string(),
                    issue_id,
                    "--reason".to_string(),
                    "cache regression stress".to_string(),
                    "--json".to_string(),
                ];
                results.push(run_br_in_dir(&root, args));
                thread::sleep(Duration::from_millis(10));
            }
            results
        })
    };

    let updater = {
        let barrier = Arc::clone(&barrier);
        let root = Arc::clone(&shared_root);
        let update_id = update_id.clone();
        thread::spawn(move || {
            barrier.wait();
            let mut results = Vec::new();
            for idx in 0..10 {
                let args = vec![
                    "--no-auto-import".to_string(),
                    "--lock-timeout".to_string(),
                    CONTENTION_SUCCESS_LOCK_TIMEOUT_MS.to_string(),
                    "update".to_string(),
                    update_id.clone(),
                    "--title".to_string(),
                    format!("Update target {idx}"),
                    "--json".to_string(),
                ];
                results.push(run_br_in_dir(&root, args));
                thread::sleep(Duration::from_millis(10));
            }
            results
        })
    };

    let reopener = {
        let barrier = Arc::clone(&barrier);
        let root = Arc::clone(&shared_root);
        let reopen_ids = reopen_ids.clone();
        thread::spawn(move || {
            barrier.wait();
            let mut results = Vec::new();
            for idx in 0..9 {
                let issue_id = reopen_ids[idx % reopen_ids.len()].clone();
                let args = vec![
                    "--no-auto-import".to_string(),
                    "--lock-timeout".to_string(),
                    CONTENTION_SUCCESS_LOCK_TIMEOUT_MS.to_string(),
                    "reopen".to_string(),
                    issue_id,
                    "--reason".to_string(),
                    format!("reopen round {idx}"),
                    "--json".to_string(),
                ];
                results.push(run_br_in_dir(&root, args));
                thread::sleep(Duration::from_millis(10));
            }
            results
        })
    };

    let reader = {
        let barrier = Arc::clone(&barrier);
        let root = Arc::clone(&shared_root);
        let update_id = update_id.clone();
        thread::spawn(move || {
            barrier.wait();
            let mut results = Vec::new();
            for idx in 0..12 {
                let args = match idx % 3 {
                    0 => vec![
                        "--lock-timeout".to_string(),
                        CONTENTION_SUCCESS_LOCK_TIMEOUT_MS.to_string(),
                        "show".to_string(),
                        update_id.clone(),
                        "--json".to_string(),
                    ],
                    1 => vec![
                        "--lock-timeout".to_string(),
                        CONTENTION_SUCCESS_LOCK_TIMEOUT_MS.to_string(),
                        "ready".to_string(),
                        "--json".to_string(),
                    ],
                    _ => vec![
                        "--lock-timeout".to_string(),
                        CONTENTION_SUCCESS_LOCK_TIMEOUT_MS.to_string(),
                        "stats".to_string(),
                        "--json".to_string(),
                    ],
                };
                results.push(run_br_in_dir(&root, args));
                thread::sleep(Duration::from_millis(5));
            }
            results
        })
    };

    let close_results = closer.join().expect("closer panicked");
    let update_results = updater.join().expect("updater panicked");
    let reopen_results = reopener.join().expect("reopener panicked");
    let reader_results = reader.join().expect("reader panicked");

    assert_no_integrity_failure_signals("close", &close_results);
    assert_no_integrity_failure_signals("update", &update_results);
    assert_no_integrity_failure_signals("reopen", &reopen_results);
    assert_no_integrity_failure_signals("reader", &reader_results);

    let close_successes = assert_only_success_or_contention("close", &close_results);
    let update_successes = assert_only_success_or_contention("update", &update_results);
    let reopen_successes = assert_only_success_or_contention("reopen", &reopen_results);
    let reader_successes = assert_only_success_or_contention("reader", &reader_results);

    assert!(
        close_successes > 0,
        "expected at least one successful close under contention"
    );
    assert!(
        update_successes > 0,
        "expected at least one successful update under contention"
    );
    assert!(
        reopen_successes > 0,
        "expected at least one successful reopen under contention"
    );
    assert!(
        reader_successes > 0,
        "expected at least one successful reader command"
    );

    let update_show = run_br_in_dir(&root, ["--no-auto-import", "show", &update_id, "--json"]);
    assert!(
        update_show.success,
        "show update target failed: {}",
        update_show.stderr
    );

    for issue_id in close_ids.iter().take(2) {
        let show = run_br_in_dir(&root, ["--no-auto-import", "show", issue_id, "--json"]);
        assert!(show.success, "show close target failed: {}", show.stderr);
    }

    for issue_id in &reopen_ids {
        let show = run_br_in_dir(&root, ["--no-auto-import", "show", issue_id, "--json"]);
        assert!(show.success, "show reopen target failed: {}", show.stderr);
    }

    // Compute an independent graph oracle from final primary statuses. Inspect
    // raw cache rows so stale-aware public getters cannot mask cache corruption.
    let conn = beads_rust::franken_sync::compat::open_with_flags(
        &root.join(".beads/beads.db").to_string_lossy(),
        beads_rust::franken_sync::compat::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .expect("open quiescent database read-only");
    let marker = conn
        .query("SELECT value FROM metadata WHERE key = 'blocked_cache_state'")
        .expect("cache marker");
    assert!(
        marker
            .iter()
            .all(|row| row.get(0).and_then(SqliteValue::as_text) != Some("stale")),
        "successful non-deferred status writes should leave a fresh cache"
    );
    for (dependent, blocker) in dependent_pairs {
        let status_rows = conn
            .query_with_params(
                "SELECT status FROM issues WHERE id = ?",
                &[SqliteValue::from(blocker.as_str())],
            )
            .expect("blocker status");
        let status = status_rows[0]
            .get(0)
            .and_then(SqliteValue::as_text)
            .expect("status text");
        let cached = conn
            .query_with_params(
                "SELECT blocked_by FROM blocked_issues_cache WHERE issue_id = ?",
                &[SqliteValue::from(dependent.as_str())],
            )
            .expect("persisted cache");
        if matches!(status, "closed" | "tombstone") {
            assert!(
                cached.is_empty(),
                "{dependent} must not remain blocked by terminal {blocker}"
            );
        } else {
            assert_eq!(cached.len(), 1, "{dependent} must be blocked by {blocker}");
            let refs: Vec<String> = serde_json::from_str(
                cached[0]
                    .get(0)
                    .and_then(SqliteValue::as_text)
                    .expect("blocker JSON"),
            )
            .expect("parse blocker witnesses");
            assert_eq!(refs, vec![format!("{blocker}:{status}")]);
        }
    }
    drop(conn);
    // Doctor's fallback can repair caches, so it must follow the raw oracle.
    assert_doctor_healthy(&root);
}

/// Regression for the ts2 report: mixed DB-backed commands in parallel must
/// serialize cleanly and leave no upstream `sqlite3` page-integrity residue.
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_parallel_mixed_db_commands_preserve_sqlite_integrity() {
    let _log = common::test_log("e2e_parallel_mixed_db_commands_preserve_sqlite_integrity");

    let temp_dir = isolated_temp_dir("temp dir");
    let root = temp_dir.path().to_path_buf();

    let init = run_br_in_dir(&root, ["init"]);
    assert!(init.success, "init failed: {}", init.stderr);

    let mut issue_ids = Vec::new();
    for idx in 0..14 {
        let created = run_br_in_dir(&root, ["create", &format!("ts2 mixed issue {idx}")]);
        assert!(
            created.success,
            "seed create {idx} failed: stdout={} stderr={}",
            created.stdout, created.stderr
        );
        issue_ids.push(parse_created_id(&created.stdout));
    }

    let barrier = Arc::new(Barrier::new(4));
    let shared_root = Arc::new(root.clone());

    let updater = {
        let barrier = Arc::clone(&barrier);
        let root = Arc::clone(&shared_root);
        let issue_ids = issue_ids.clone();
        thread::spawn(move || {
            barrier.wait();
            let mut results = Vec::new();
            for (idx, issue_id) in issue_ids.iter().take(10).enumerate() {
                let args = vec![
                    "--lock-timeout".to_string(),
                    "15000".to_string(),
                    "update".to_string(),
                    issue_id.clone(),
                    "--title".to_string(),
                    format!("ts2 mixed updated {idx}"),
                    "--priority".to_string(),
                    (idx % 5).to_string(),
                    "--json".to_string(),
                ];
                results.push(run_br_in_dir(&root, args));
                thread::sleep(Duration::from_millis(5));
            }
            results
        })
    };

    let depper = {
        let barrier = Arc::clone(&barrier);
        let root = Arc::clone(&shared_root);
        let issue_ids = issue_ids.clone();
        thread::spawn(move || {
            barrier.wait();
            let mut results = Vec::new();
            for idx in 1..issue_ids.len() {
                let args = vec![
                    "--lock-timeout".to_string(),
                    "15000".to_string(),
                    "dep".to_string(),
                    "add".to_string(),
                    issue_ids[idx].clone(),
                    issue_ids[idx - 1].clone(),
                    "--json".to_string(),
                ];
                results.push(run_br_in_dir(&root, args));
                thread::sleep(Duration::from_millis(5));
            }
            results
        })
    };

    let creator = {
        let barrier = Arc::clone(&barrier);
        let root = Arc::clone(&shared_root);
        thread::spawn(move || {
            barrier.wait();
            let mut results = Vec::new();
            for idx in 0..8 {
                let args = vec![
                    "--lock-timeout".to_string(),
                    "15000".to_string(),
                    "create".to_string(),
                    format!("ts2 mixed concurrent create {idx}"),
                    "--json".to_string(),
                ];
                results.push(run_br_in_dir(&root, args));
                thread::sleep(Duration::from_millis(5));
            }
            results
        })
    };

    let reader = {
        let barrier = Arc::clone(&barrier);
        let root = Arc::clone(&shared_root);
        let issue_ids = issue_ids.clone();
        thread::spawn(move || {
            barrier.wait();
            let mut results = Vec::new();
            for idx in 0..12 {
                let args = match idx % 4 {
                    0 => vec![
                        "--lock-timeout".to_string(),
                        "15000".to_string(),
                        "show".to_string(),
                        issue_ids[idx % issue_ids.len()].clone(),
                        "--json".to_string(),
                    ],
                    1 => vec![
                        "--lock-timeout".to_string(),
                        "15000".to_string(),
                        "status".to_string(),
                        "--no-activity".to_string(),
                        "--json".to_string(),
                    ],
                    2 => vec![
                        "--lock-timeout".to_string(),
                        "15000".to_string(),
                        "ready".to_string(),
                        "--json".to_string(),
                    ],
                    _ => vec![
                        "--lock-timeout".to_string(),
                        "15000".to_string(),
                        "doctor".to_string(),
                        "--json".to_string(),
                    ],
                };
                results.push(run_br_in_dir(&root, args));
                thread::sleep(Duration::from_millis(5));
            }
            results
        })
    };

    let update_results = updater.join().expect("updater panicked");
    let dep_results = depper.join().expect("depper panicked");
    let create_results = creator.join().expect("creator panicked");
    let read_results = reader.join().expect("reader panicked");

    for (role, results) in [
        ("update", &update_results),
        ("dep add", &dep_results),
        ("create", &create_results),
        ("read/status/doctor", &read_results),
    ] {
        assert_no_integrity_failure_signals(role, results);
        let successes = assert_only_success_or_contention(role, results);
        assert!(successes > 0, "{role} had no successful operations");
    }

    assert_doctor_has_no_page_anomalies(&root, "after mixed parallel DB load");
    assert_upstream_sqlite_integrity_ok(&root, "after mixed parallel DB load");

    for round in 0..4 {
        let status = run_br_in_dir(&root, ["status", "--no-activity", "--json"]);
        assert!(
            status.success,
            "post-load status round {round} failed: stdout={} stderr={}",
            status.stdout, status.stderr
        );

        let doctor = run_br_in_dir(&root, ["doctor", "--json"]);
        assert!(
            doctor.success,
            "post-load doctor round {round} failed: stdout={} stderr={}",
            doctor.stdout, doctor.stderr
        );
    }

    assert_doctor_has_no_page_anomalies(&root, "after repeated status/doctor reads");
    assert_upstream_sqlite_integrity_ok(&root, "after repeated status/doctor reads");
}

/// Regression for #460: repeated writers touching an issue whose description
/// spans SQLite overflow pages must not lose the issue or damage the freelist.
fn overflow_page_description() -> String {
    let mut description = String::with_capacity(15_000);
    let payload = "abcdef0123456789".repeat(8);
    for index in 0..96 {
        writeln!(description, "overflow-page-line-{index:04}: {payload}")
            .expect("writing to a String cannot fail");
    }
    description
}

#[test]
fn e2e_parallel_writes_preserve_large_description_and_freelist() {
    let _log = common::test_log("e2e_parallel_writes_preserve_large_description_and_freelist");

    let temp_dir = isolated_temp_dir("large-description concurrency temp dir");
    let root = temp_dir.path().to_path_buf();

    let init = run_br_in_dir(&root, ["init"]);
    assert!(init.success, "init failed: {}", init.stderr);

    let description = overflow_page_description();
    assert!(description.len() > 14_000);

    let created = run_br_in_dir(
        &root,
        [
            "create",
            "Large overflow-page regression record",
            "--description",
            &description,
        ],
    );
    assert!(
        created.success,
        "large issue create failed: stdout={} stderr={}",
        created.stdout, created.stderr
    );
    let issue_id = parse_created_id(&created.stdout);
    assert!(!issue_id.is_empty(), "created issue id missing");

    let barrier = Arc::new(Barrier::new(4));
    let shared_root = Arc::new(root.clone());
    let handles = (0..4)
        .map(|worker| {
            let barrier = Arc::clone(&barrier);
            let root = Arc::clone(&shared_root);
            let issue_id = issue_id.clone();
            thread::spawn(move || {
                barrier.wait();
                (0..12)
                    .map(|index| {
                        if index % 2 == 0 {
                            run_br_in_dir(
                                &root,
                                [
                                    "--lock-timeout",
                                    "60000",
                                    "update",
                                    &issue_id,
                                    "--notes",
                                    &format!("overflow write {worker}-{index}"),
                                    "--json",
                                ],
                            )
                        } else {
                            run_br_in_dir(
                                &root,
                                [
                                    "--lock-timeout",
                                    "60000",
                                    "comments",
                                    "add",
                                    &issue_id,
                                    "--message",
                                    &format!("overflow comment {worker}-{index}"),
                                    "--json",
                                ],
                            )
                        }
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect::<Vec<_>>();

    for (worker, handle) in handles.into_iter().enumerate() {
        let results = handle.join().expect("overflow writer panicked");
        assert!(
            results.iter().all(|result| result.success),
            "overflow writer {worker} failed: {results:?}"
        );
        assert_no_integrity_failure_signals("overflow writer", &results);
    }

    let show = run_br_in_dir(&root, ["--no-auto-import", "show", &issue_id, "--json"]);
    assert!(
        show.success,
        "large issue vanished after writes: stdout={} stderr={}",
        show.stdout, show.stderr
    );
    let shown: serde_json::Value =
        serde_json::from_str(&extract_json_payload(&show.stdout)).expect("show JSON");
    assert_eq!(shown[0]["description"].as_str(), Some(description.as_str()));

    let jsonl = fs::read_to_string(root.join(".beads/issues.jsonl")).expect("read JSONL");
    let exported = jsonl
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<serde_json::Value>(line).expect("parse JSONL row"))
        .find(|issue| issue["id"].as_str() == Some(issue_id.as_str()))
        .expect("large issue must remain exported");
    assert_eq!(exported["description"].as_str(), Some(description.as_str()));

    assert_doctor_has_no_page_anomalies(&root, "after overflow-page writes");
    assert_upstream_sqlite_integrity_ok(&root, "after overflow-page writes");
}

/// Test that routed access remains bounded even while the routed workspace
/// itself is mutating, not just the invoking workspace.
#[test]
#[allow(clippy::too_many_lines)]
fn e2e_routed_access_remains_bounded_while_remote_workspace_mutates() {
    let _log = common::test_log("e2e_routed_access_remains_bounded_while_remote_workspace_mutates");

    let main_temp_dir = isolated_temp_dir("main temp dir");
    let external_temp_dir = isolated_temp_dir("external temp dir");
    let main_root = main_temp_dir.path().to_path_buf();
    let external_root = external_temp_dir.path().to_path_buf();

    let init_main = run_br_in_dir(&main_root, ["init"]);
    assert!(init_main.success, "main init failed: {}", init_main.stderr);
    let init_external = run_br_in_dir(&external_root, ["init"]);
    assert!(
        init_external.success,
        "external init failed: {}",
        init_external.stderr
    );

    configure_external_route(&main_root, &external_root);

    let local_issue = run_br_in_dir(&main_root, ["create", "Local routed contention target"]);
    assert!(local_issue.success, "create local issue failed");
    let local_id = parse_created_id(&local_issue.stdout);

    let external_issue = run_br_in_dir(
        &external_root,
        ["create", "External routed contention target"],
    );
    assert!(external_issue.success, "create external issue failed");
    let external_id = parse_created_id(&external_issue.stdout);

    let barrier = Arc::new(Barrier::new(3));
    let main_root_arc = Arc::new(main_root.clone());
    let external_root_arc = Arc::new(external_root.clone());

    let local_writer = {
        let barrier = Arc::clone(&barrier);
        let main_root = Arc::clone(&main_root_arc);
        thread::spawn(move || {
            barrier.wait();
            let mut results = Vec::new();
            for i in 0..8 {
                let args = vec![
                    "--lock-timeout".to_string(),
                    CONTENTION_SUCCESS_LOCK_TIMEOUT_MS.to_string(),
                    "create".to_string(),
                    format!("local-route-write-{i}"),
                ];
                results.push(run_br_in_dir(&main_root, args));
                thread::sleep(Duration::from_millis(8));
            }
            results
        })
    };

    let external_writer = {
        let barrier = Arc::clone(&barrier);
        let external_root = Arc::clone(&external_root_arc);
        let external_id = external_id.clone();
        thread::spawn(move || {
            barrier.wait();
            let mut results = Vec::new();
            for i in 0..8 {
                let args = vec![
                    "--lock-timeout".to_string(),
                    CONTENTION_SUCCESS_LOCK_TIMEOUT_MS.to_string(),
                    "--actor".to_string(),
                    "bob".to_string(),
                    "update".to_string(),
                    external_id.clone(),
                    "--title".to_string(),
                    format!("remote-mutation-{i}"),
                    "--json".to_string(),
                ];
                results.push(run_br_in_dir(&external_root, args));
                thread::sleep(Duration::from_millis(8));
            }
            results
        })
    };

    let routed_worker = {
        let barrier = Arc::clone(&barrier);
        let main_root = Arc::clone(&main_root_arc);
        let external_id = external_id.clone();
        thread::spawn(move || {
            barrier.wait();
            let mut results = Vec::new();
            for i in 0..10 {
                let args = if i % 2 == 0 {
                    vec![
                        "--lock-timeout".to_string(),
                        CONTENTION_SUCCESS_LOCK_TIMEOUT_MS.to_string(),
                        "show".to_string(),
                        external_id.clone(),
                        "--json".to_string(),
                    ]
                } else {
                    vec![
                        "--lock-timeout".to_string(),
                        CONTENTION_SUCCESS_LOCK_TIMEOUT_MS.to_string(),
                        "--actor".to_string(),
                        "carol".to_string(),
                        "label".to_string(),
                        "add".to_string(),
                        external_id.clone(),
                        "remote-route".to_string(),
                    ]
                };
                results.push(run_br_in_dir(&main_root, args));
                thread::sleep(Duration::from_millis(6));
            }
            results
        })
    };

    let local_results = local_writer.join().expect("local writer panicked");
    let external_results = external_writer.join().expect("external writer panicked");
    let routed_results = routed_worker.join().expect("routed worker panicked");

    let local_successes = assert_only_success_or_contention("local_writer", &local_results);
    let external_successes =
        assert_only_success_or_contention("external_writer", &external_results);
    assert_only_success_or_contention("routed_worker", &routed_results);
    let routed_label_successes = routed_results
        .iter()
        .enumerate()
        .filter(|(idx, result)| *idx % 2 == 1 && result.success)
        .count();

    assert!(
        local_successes > 0,
        "expected at least one successful local write"
    );
    assert!(
        external_successes > 0,
        "expected at least one successful remote mutation"
    );
    assert_doctor_healthy(&main_root);

    // Use --no-auto-import for post-contention reads to avoid SYNC_CONFLICT.
    let routed_show = run_br_in_dir(
        &main_root,
        ["--no-auto-import", "show", &external_id, "--json"],
    );
    assert!(
        routed_show.success,
        "show routed issue failed: {}",
        routed_show.stderr
    );
    let routed_json: Vec<serde_json::Value> =
        serde_json::from_str(&extract_json_payload(&routed_show.stdout))
            .expect("parse routed show json");
    let routed_title = routed_json[0]["title"]
        .as_str()
        .expect("routed title should be present");
    assert!(
        routed_title.starts_with("remote-mutation-"),
        "expected remote title mutation, got: {routed_title}"
    );

    let external_labels = run_br_in_dir(
        &external_root,
        ["--no-auto-import", "label", "list", &external_id, "--json"],
    );
    assert!(
        external_labels.success,
        "label list on external workspace failed: {}",
        external_labels.stderr
    );
    if routed_label_successes > 0 {
        let label_json: Vec<String> =
            serde_json::from_str(&extract_json_payload(&external_labels.stdout))
                .expect("parse external label list");
        assert!(
            label_json.iter().any(|label| label == "remote-route"),
            "expected remote-route label in external workspace: {}",
            external_labels.stdout
        );
    }

    let local_show = run_br_in_dir(
        &main_root,
        ["--no-auto-import", "show", &local_id, "--json"],
    );
    assert!(
        local_show.success,
        "show local issue failed after routed contention: {}",
        local_show.stderr
    );

    let main_status = run_br_in_dir(&main_root, ["sync", "--status", "--json"]);
    assert!(
        main_status.success,
        "sync --status failed after routed contention: stdout={} stderr={}",
        main_status.stdout, main_status.stderr
    );
}
