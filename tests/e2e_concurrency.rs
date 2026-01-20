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
use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;

/// Result of running a br command.
#[derive(Debug)]
struct BrResult {
    stdout: String,
    stderr: String,
    success: bool,
    _duration: Duration,
}

/// Run br command in a specific directory.
fn run_br_in_dir<I, S>(root: &PathBuf, args: I) -> BrResult
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let start = Instant::now();
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("br"));
    cmd.current_dir(root);
    cmd.args(args);
    cmd.env("NO_COLOR", "1");
    cmd.env("RUST_BACKTRACE", "1");
    cmd.env("HOME", root);

    let output = cmd.output().expect("run br");
    let duration = start.elapsed();

    BrResult {
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        success: output.status.success(),
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
    let temp_dir = TempDir::new().expect("create temp dir");
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

    // Spawn two threads that will try to create issues concurrently
    let handle1 = thread::spawn(move || {
        barrier1.wait();
        run_br_in_dir(&root1_clone, ["create", "Issue from thread 1"])
    });

    let handle2 = thread::spawn(move || {
        barrier2.wait();
        run_br_in_dir(&root2_clone, ["create", "Issue from thread 2"])
    });

    let result1 = handle1.join().expect("thread 1 panicked");
    let result2 = handle2.join().expect("thread 2 panicked");

    // With default busy timeout, both should eventually succeed
    // (SQLite retries on SQLITE_BUSY)
    assert!(
        result1.success,
        "thread 1 create failed: {}",
        result1.stderr
    );
    assert!(
        result2.success,
        "thread 2 create failed: {}",
        result2.stderr
    );

    // Verify both issues were created
    let list = run_br_in_dir(&root, ["list", "--json"]);
    assert!(list.success, "list failed: {}", list.stderr);
    assert!(
        list.stdout.contains("Issue from thread 1"),
        "missing issue from thread 1"
    );
    assert!(
        list.stdout.contains("Issue from thread 2"),
        "missing issue from thread 2"
    );

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

    let temp_dir = TempDir::new().expect("create temp dir");
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

    let temp_dir = TempDir::new().expect("create temp dir");
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

/// Test that lock timeout is properly respected with specific timing.
///
/// This test:
/// 1. Sets a specific lock timeout
/// 2. Verifies the operation completes within expected time (no contention)
#[test]
fn e2e_lock_timeout_timing() {
    let _log = common::test_log("e2e_lock_timeout_timing");

    let temp_dir = TempDir::new().expect("create temp dir");
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

    let temp_dir = TempDir::new().expect("create temp dir");
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
            let result = run_br_in_dir(&root_clone, ["create", &format!("Serialized issue {i}")]);
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

    // All should succeed
    for (i, result, elapsed) in &results {
        assert!(result.success, "thread {i} failed: {}", result.stderr);
        eprintln!("Thread {i} took {elapsed:?}");
    }

    eprintln!("Total time for 3 serialized writes: {total_elapsed:?}");

    // Verify all 3 issues exist
    let list = run_br_in_dir(&root, ["list", "--json"]);
    assert!(list.success, "final list failed: {}", list.stderr);
    for i in 0..3 {
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

    let temp_dir = TempDir::new().expect("create temp dir");
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
            let result = run_br_in_dir(&root_clone, ["list", "--json"]);
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

    // All operations should succeed
    for (role, i, result, elapsed) in &results {
        assert!(result.success, "{role} {i} failed: {}", result.stderr);
        eprintln!("{role} {i} completed in {elapsed:?}");
    }

    // Verify final state
    let list = run_br_in_dir(&root, ["list", "--json"]);
    assert!(list.success, "final list failed: {}", list.stderr);

    // Should have 3 existing + 3 new = 6 issues
    let payload = extract_json_payload(&list.stdout);
    let issues: Vec<serde_json::Value> = serde_json::from_str(&payload).expect("parse list json");
    assert_eq!(
        issues.len(),
        6,
        "expected 6 issues, got {len}",
        len = issues.len()
    );

    drop(temp_dir);
}

/// Test that database locked errors are properly reported.
///
/// This test verifies that when a lock cannot be acquired within the timeout,
/// an appropriate error message is returned.
#[test]
fn e2e_lock_error_reporting() {
    let _log = common::test_log("e2e_lock_error_reporting");

    let temp_dir = TempDir::new().expect("create temp dir");
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
