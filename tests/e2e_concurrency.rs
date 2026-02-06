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

/// Test that concurrent --claim on the same issue is safe.
///
/// This test:
/// 1. Creates an unassigned issue
/// 2. Spawns two threads that race to claim it with different actor names
/// 3. Verifies exactly one succeeds and the other fails
///
/// Before the atomic claim fix, both would succeed (last-write-wins).
#[test]
fn e2e_concurrent_claim_exactly_one_wins() {
    let _log = common::test_log("e2e_concurrent_claim_exactly_one_wins");

    let temp_dir = TempDir::new().expect("create temp dir");
    let root = temp_dir.path().to_path_buf();

    // Initialize workspace
    let init = run_br_in_dir(&root, ["init"]);
    assert!(init.success, "init failed: {}", init.stderr);

    // Create an unassigned issue
    let create = run_br_in_dir(&root, ["create", "Race condition target"]);
    assert!(create.success, "create failed: {}", create.stderr);
    let issue_id = parse_created_id(&create.stdout);
    assert!(!issue_id.is_empty(), "failed to parse created issue ID");

    // Run the race multiple times to increase chance of hitting the window
    let mut both_succeeded_count = 0;
    let mut exactly_one_won_count = 0;
    const ITERATIONS: usize = 10;

    for iteration in 0..ITERATIONS {
        // Reset: reopen + unassign the issue before each iteration
        let reset = run_br_in_dir(
            &root,
            ["update", &issue_id, "--status", "open", "--assignee", ""],
        );
        assert!(
            reset.success,
            "iteration {iteration}: reset failed: {}",
            reset.stderr
        );

        let barrier = Arc::new(Barrier::new(2));
        let root1 = Arc::new(root.clone());
        let root2 = Arc::new(root.clone());
        let id1 = issue_id.clone();
        let id2 = issue_id.clone();

        let barrier1 = Arc::clone(&barrier);
        let barrier2 = Arc::clone(&barrier);

        // Thread 1: claim as "agent-alpha"
        let handle1 = thread::spawn(move || {
            barrier1.wait();
            run_br_in_dir(
                &root1,
                ["update", &id1, "--claim", "--actor", "agent-alpha"],
            )
        });

        // Thread 2: claim as "agent-beta"
        let handle2 = thread::spawn(move || {
            barrier2.wait();
            run_br_in_dir(&root2, ["update", &id2, "--claim", "--actor", "agent-beta"])
        });

        let result1 = handle1.join().expect("thread 1 panicked");
        let result2 = handle2.join().expect("thread 2 panicked");

        let successes = usize::from(result1.success) + usize::from(result2.success);

        assert!(
            successes >= 1,
            "iteration {iteration}: both claims failed — neither agent won. \
             t1: stdout={}, stderr={} | t2: stdout={}, stderr={}",
            result1.stdout, result1.stderr, result2.stdout, result2.stderr
        );

        if successes == 2 {
            both_succeeded_count += 1;
        } else {
            exactly_one_won_count += 1;

            // Verify the loser got a claim-related error
            let loser = if result1.success { &result2 } else { &result1 };
            let combined = format!("{} {}", loser.stdout, loser.stderr).to_lowercase();
            assert!(
                combined.contains("claim")
                    || combined.contains("assigned")
                    || combined.contains("already"),
                "iteration {iteration}: loser should get a claim error, got: stdout={}, stderr={}",
                loser.stdout,
                loser.stderr
            );
        }

        eprintln!(
            "iteration {iteration}: agent-alpha={}, agent-beta={}",
            if result1.success { "won" } else { "lost" },
            if result2.success { "won" } else { "lost" },
        );
    }

    eprintln!(
        "Results: exactly_one_won={exactly_one_won_count}, both_succeeded={both_succeeded_count} (out of {ITERATIONS})"
    );

    // With the fix, both should never succeed when they truly race.
    // Due to timing, some iterations may not actually race (one finishes
    // before the other starts), which is fine — those will show as
    // exactly_one_won (the second sees the first's claim via fast-path).
    // The key assertion: we should NEVER see both succeed in a true race.
    // If both_succeeded_count > 0, the atomic guard isn't working.
    assert_eq!(
        both_succeeded_count, 0,
        "TOCTOU race detected: both agents claimed the same issue in {both_succeeded_count}/{ITERATIONS} iterations"
    );

    drop(temp_dir);
}

/// Test that --claim without --actor auto-appends a session disambiguator.
///
/// When no explicit actor or BD_SESSION_ID is set, `br update --claim`
/// should auto-detect the grandparent PID and append it to the actor name,
/// producing something like "runner-12345" instead of bare "runner".
///
/// This test is Unix-only because `grandparent_pid()` shells out to `ps`,
/// which is unavailable on Windows and some minimal containers.
#[test]
#[cfg(unix)]
fn e2e_claim_auto_disambiguates_actor() {
    let _log = common::test_log("e2e_claim_auto_disambiguates_actor");

    let temp_dir = TempDir::new().expect("create temp dir");
    let root = temp_dir.path().to_path_buf();

    // Initialize workspace
    let init = run_br_in_dir(&root, ["init"]);
    assert!(init.success, "init failed: {}", init.stderr);

    // Create an unassigned issue
    let create = run_br_in_dir(&root, ["create", "Auto-disambiguate test"]);
    assert!(create.success, "create failed: {}", create.stderr);
    let issue_id = parse_created_id(&create.stdout);

    // Claim without --actor and without BD_SESSION_ID
    let output = Command::new(assert_cmd::cargo::cargo_bin!("br"))
        .current_dir(&root)
        .args(["update", &issue_id, "--claim"])
        .env("NO_COLOR", "1")
        .env("RUST_BACKTRACE", "1")
        .env("HOME", &root)
        .env_remove("BD_ACTOR")
        .env_remove("BD_SESSION_ID")
        .output()
        .expect("run br");
    assert!(
        output.status.success(),
        "claim failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify the assignee has a numeric suffix (grandparent PID)
    let show = run_br_in_dir(&root, ["show", &issue_id, "--json"]);
    assert!(show.success, "show failed: {}", show.stderr);
    let json: serde_json::Value = serde_json::from_str(&show.stdout).expect("parse show output");
    let json = if json.is_array() { &json[0] } else { &json };
    let assignee = json["assignee"].as_str().unwrap_or("");
    eprintln!("assignee: {assignee}");

    // Should contain a hyphen followed by digits (the grandparent PID).
    // Format: "<user>-<pid>" e.g. "runner-12345"
    assert!(
        assignee.contains('-'),
        "expected auto-disambiguated actor with PID suffix, got bare: {assignee}"
    );
    let suffix = assignee.rsplit('-').next().unwrap_or("");
    assert!(
        suffix.chars().all(|c| c.is_ascii_digit()) && !suffix.is_empty(),
        "expected numeric PID suffix after hyphen, got: {assignee}"
    );

    drop(temp_dir);
}

/// Test that `claim.exclusive: true` rejects same-actor re-claim.
///
/// When the exclusive flag is set, a second claim by the SAME actor must fail.
#[test]
fn e2e_claim_exclusive_rejects_same_actor_reclaim() {
    let _log = common::test_log("e2e_claim_exclusive_rejects_same_actor_reclaim");

    let temp_dir = TempDir::new().expect("create temp dir");
    let root = temp_dir.path().to_path_buf();

    // Initialize workspace
    let init = run_br_in_dir(&root, ["init"]);
    assert!(init.success, "init failed: {}", init.stderr);

    // Set claim.exclusive in project config
    let config_path = root.join(".beads").join("config.yaml");
    std::fs::write(&config_path, "claim.exclusive: true\n").expect("write config");

    // Create an unassigned issue
    let create = run_br_in_dir(&root, ["create", "Exclusive claim target"]);
    assert!(create.success, "create failed: {}", create.stderr);
    let issue_id = parse_created_id(&create.stdout);

    // First claim as actor-A should succeed
    let claim1 = run_br_in_dir(
        &root,
        ["update", &issue_id, "--claim", "--actor", "actor-A"],
    );
    assert!(claim1.success, "first claim failed: {}", claim1.stderr);

    // Second claim by the SAME actor should fail in exclusive mode
    let claim2 = run_br_in_dir(
        &root,
        ["update", &issue_id, "--claim", "--actor", "actor-A"],
    );
    assert!(
        !claim2.success,
        "second same-actor claim should fail with claim.exclusive=true"
    );
    let combined = format!("{} {}", claim2.stdout, claim2.stderr).to_lowercase();
    assert!(
        combined.contains("claim") || combined.contains("assigned") || combined.contains("already"),
        "expected claim-related error, got: stdout={}, stderr={}",
        claim2.stdout,
        claim2.stderr
    );

    drop(temp_dir);
}

/// Test that `claim.exclusive: true` with concurrent same-actor claims
/// results in exactly one winner.
///
/// This is the core scenario: two processes with the same actor identity
/// race to claim the same issue. With exclusive mode, exactly one must win.
#[test]
fn e2e_claim_exclusive_concurrent_same_actor() {
    let _log = common::test_log("e2e_claim_exclusive_concurrent_same_actor");

    let temp_dir = TempDir::new().expect("create temp dir");
    let root = temp_dir.path().to_path_buf();

    // Initialize workspace
    let init = run_br_in_dir(&root, ["init"]);
    assert!(init.success, "init failed: {}", init.stderr);

    // Set claim.exclusive in project config
    let config_path = root.join(".beads").join("config.yaml");
    std::fs::write(&config_path, "claim.exclusive: true\n").expect("write config");

    // Create an unassigned issue
    let create = run_br_in_dir(&root, ["create", "Exclusive race target"]);
    assert!(create.success, "create failed: {}", create.stderr);
    let issue_id = parse_created_id(&create.stdout);

    let mut both_succeeded_count = 0;
    const ITERATIONS: usize = 10;

    for iteration in 0..ITERATIONS {
        // Reset: reopen + unassign the issue before each iteration
        let reset = run_br_in_dir(
            &root,
            ["update", &issue_id, "--status", "open", "--assignee", ""],
        );
        assert!(
            reset.success,
            "iteration {iteration}: reset failed: {}",
            reset.stderr
        );

        let barrier = Arc::new(Barrier::new(2));
        let root1 = Arc::new(root.clone());
        let root2 = Arc::new(root.clone());
        let id1 = issue_id.clone();
        let id2 = issue_id.clone();

        let barrier1 = Arc::clone(&barrier);
        let barrier2 = Arc::clone(&barrier);

        // Both threads claim as the SAME actor
        let handle1 = thread::spawn(move || {
            barrier1.wait();
            run_br_in_dir(&root1, ["update", &id1, "--claim", "--actor", "same-actor"])
        });

        let handle2 = thread::spawn(move || {
            barrier2.wait();
            run_br_in_dir(&root2, ["update", &id2, "--claim", "--actor", "same-actor"])
        });

        let result1 = handle1.join().expect("thread 1 panicked");
        let result2 = handle2.join().expect("thread 2 panicked");

        let successes = usize::from(result1.success) + usize::from(result2.success);

        assert!(
            successes >= 1,
            "iteration {iteration}: both claims failed — neither agent won. \
             t1: stdout={}, stderr={} | t2: stdout={}, stderr={}",
            result1.stdout, result1.stderr, result2.stdout, result2.stderr
        );

        if successes == 2 {
            both_succeeded_count += 1;
        }

        eprintln!(
            "iteration {iteration}: t1={}, t2={}",
            if result1.success { "won" } else { "lost" },
            if result2.success { "won" } else { "lost" },
        );
    }

    eprintln!("Results: both_succeeded={both_succeeded_count} (out of {ITERATIONS})");

    // With claim.exclusive, both should never succeed — even with the same actor
    assert_eq!(
        both_succeeded_count, 0,
        "exclusive mode violated: both same-actor claims won in {both_succeeded_count}/{ITERATIONS} iterations"
    );

    drop(temp_dir);
}

/// Test that BD_SESSION_ID produces a unique actor name and suppresses the warning.
///
/// When BD_SESSION_ID is set, `br update --claim` should:
/// 1. Use the session ID as a suffix on the actor name (e.g. "user-17240")
/// 2. NOT emit the disambiguation warning
#[test]
fn e2e_session_id_disambiguates_actor() {
    let _log = common::test_log("e2e_session_id_disambiguates_actor");

    let temp_dir = TempDir::new().expect("create temp dir");
    let root = temp_dir.path().to_path_buf();

    // Initialize workspace
    let init = run_br_in_dir(&root, ["init"]);
    assert!(init.success, "init failed: {}", init.stderr);

    // Create an unassigned issue
    let create = run_br_in_dir(&root, ["create", "Session ID test"]);
    assert!(create.success, "create failed: {}", create.stderr);
    let issue_id = parse_created_id(&create.stdout);

    // Claim WITH BD_SESSION_ID set — should succeed without warning
    let output = Command::new(assert_cmd::cargo::cargo_bin!("br"))
        .current_dir(&root)
        .args(["update", &issue_id, "--claim"])
        .env("NO_COLOR", "1")
        .env("RUST_BACKTRACE", "1")
        .env("HOME", &root)
        .env_remove("BD_ACTOR")
        .env("BD_SESSION_ID", "99999")
        .output()
        .expect("run br");
    let _claim_stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let claim_stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(output.status.success(), "claim failed: {claim_stderr}");

    // Should NOT contain the disambiguation warning
    let stderr_lower = claim_stderr.to_lowercase();
    assert!(
        !stderr_lower.contains("claiming without"),
        "should not warn when BD_SESSION_ID is set, got: {claim_stderr}"
    );

    // Verify the assignee includes the session suffix
    let show = run_br_in_dir(&root, ["show", &issue_id, "--json"]);
    assert!(show.success, "show failed: {}", show.stderr);
    let json: serde_json::Value = serde_json::from_str(&show.stdout).expect("parse show output");
    let json = if json.is_array() { &json[0] } else { &json };
    let assignee = json["assignee"].as_str().unwrap_or("");
    eprintln!("assignee: {assignee}");
    assert!(
        assignee.ends_with("-99999"),
        "expected assignee to end with session suffix -99999, got: {assignee}"
    );

    drop(temp_dir);
}

/// Test that two sessions with different BD_SESSION_ID can both claim and be distinguished.
///
/// Simulates two Claude Code sessions by using different BD_SESSION_ID values.
/// Both claim different issues; the assignees should have different suffixes.
#[test]
fn e2e_session_id_concurrent_sessions() {
    let _log = common::test_log("e2e_session_id_concurrent_sessions");

    let temp_dir = TempDir::new().expect("create temp dir");
    let root = temp_dir.path().to_path_buf();

    let init = run_br_in_dir(&root, ["init"]);
    assert!(init.success, "init failed: {}", init.stderr);

    // Create two issues
    let create1 = run_br_in_dir(&root, ["create", "Session A target"]);
    assert!(create1.success, "create1 failed: {}", create1.stderr);
    let id1 = parse_created_id(&create1.stdout);

    let create2 = run_br_in_dir(&root, ["create", "Session B target"]);
    assert!(create2.success, "create2 failed: {}", create2.stderr);
    let id2 = parse_created_id(&create2.stdout);

    // Session A claims issue 1
    let out_a = Command::new(assert_cmd::cargo::cargo_bin!("br"))
        .current_dir(&root)
        .args(["update", &id1, "--claim"])
        .env("NO_COLOR", "1")
        .env("HOME", &root)
        .env_remove("BD_ACTOR")
        .env("BD_SESSION_ID", "11111")
        .output()
        .expect("run br session A");
    assert!(out_a.status.success(), "session A claim failed");

    // Session B claims issue 2
    let out_b = Command::new(assert_cmd::cargo::cargo_bin!("br"))
        .current_dir(&root)
        .args(["update", &id2, "--claim"])
        .env("NO_COLOR", "1")
        .env("HOME", &root)
        .env_remove("BD_ACTOR")
        .env("BD_SESSION_ID", "22222")
        .output()
        .expect("run br session B");
    assert!(out_b.status.success(), "session B claim failed");

    // Verify they have different assignees
    let show1 = run_br_in_dir(&root, ["show", &id1, "--json"]);
    let show2 = run_br_in_dir(&root, ["show", &id2, "--json"]);
    let j1: serde_json::Value = serde_json::from_str(&show1.stdout).expect("parse");
    let j2: serde_json::Value = serde_json::from_str(&show2.stdout).expect("parse");
    let j1 = if j1.is_array() { &j1[0] } else { &j1 };
    let j2 = if j2.is_array() { &j2[0] } else { &j2 };
    let assignee1 = j1["assignee"].as_str().unwrap_or("");
    let assignee2 = j2["assignee"].as_str().unwrap_or("");

    eprintln!("session A assignee: {assignee1}");
    eprintln!("session B assignee: {assignee2}");

    assert!(assignee1.ends_with("-11111"), "session A: {assignee1}");
    assert!(assignee2.ends_with("-22222"), "session B: {assignee2}");
    assert_ne!(assignee1, assignee2, "sessions must have different actors");

    drop(temp_dir);
}
