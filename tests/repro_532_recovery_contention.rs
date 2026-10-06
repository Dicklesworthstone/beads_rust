//! GH #532: real stock-SQLite readers must not permanently latch br startup.
//! The Python harness owns the reader and sequences the child processes, so
//! these tests do not depend on a sleep or on the reader exiting at a lucky time.

#![cfg(any(target_os = "linux", target_os = "macos"))]

use std::process::Command;

fn run_case(case: &str, addressing: &str) {
    let workspace = tempfile::tempdir().expect("isolated recovery workspace");
    let output = Command::new("python3")
        .arg("-c")
        .arg(include_str!("e2e_scripts/recovery_contention_532.py"))
        .arg(env!("CARGO_BIN_EXE_br"))
        .arg(workspace.path())
        .arg(case)
        .arg(addressing)
        .output()
        .expect("python3 with the stdlib sqlite3 module is required for this regression");
    assert!(
        output.status.success(),
        "{case}/{addressing}: {}\n{}\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

#[test]
fn sequential_stock_reader_release_allows_reads_and_writes() {
    run_case("sequential", "discovery");
}

#[test]
#[cfg(target_os = "linux")]
fn linux_explicit_db_reader_release_allows_reads_and_writes() {
    run_case("sequential", "explicit");
}

#[test]
fn legacy_busy_receipts_do_not_latch_after_upgrade() {
    run_case("legacy", "explicit");
}

#[test]
fn repeated_concurrent_recoveries_retain_evidence_without_latching() {
    run_case("concurrent", "explicit");
}
