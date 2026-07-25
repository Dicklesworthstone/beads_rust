//! Phase 9 — Real-world fixture suite for `br doctor`.
//!
//! Drives `tests/doctor_fixtures/run_all.sh` against the compiled `br` binary
//! and asserts exit 0. The bash driver iterates each fixture directory,
//! plants the failure with `corrupt.sh`, runs `br doctor --json`, validates
//! detection via `assert.sh DIR detect`, runs `br doctor --repair --json`,
//! validates the post-repair invariants via `assert.sh DIR post_repair`,
//! then runs `br doctor undo latest --json` and validates the post-undo
//! state.
//!
//! This file is intentionally thin: the heavy lifting is in `run_all.sh` so
//! agents can iterate fixtures with a tight bash REPL without rebuilding the
//! test binary every time. The Rust test exists to wire the suite into
//! `cargo test` and the CI pipeline.

#[path = "common/mod.rs"]
mod common;

use std::path::PathBuf;
use std::process::Command;

/// Run the bash-driven real-world fixture suite end-to-end. Asserts exit 0.
#[test]
fn doctor_fixture_suite_passes() {
    // Skip when `jq` or `bash` are unavailable (e.g. cross-compiled CI host).
    if which("bash").is_none() {
        eprintln!("[skip] bash not on PATH");
        return;
    }
    if which("jq").is_none() {
        eprintln!("[skip] jq not on PATH; install jq to run the doctor fixture suite");
        return;
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let script = manifest_dir
        .join("tests")
        .join("doctor_fixtures")
        .join("run_all.sh");
    assert!(
        script.is_file(),
        "fixture driver missing: {}",
        script.display()
    );

    let bin = env!("CARGO_BIN_EXE_br");

    let output = Command::new("bash")
        .arg(&script)
        .env("TOOL_BIN", bin)
        // run all fixtures even if one fails so the diagnostic shows the
        // full failure inventory.
        .env("FAIL_FAST", "0")
        // A host with `br` installed in more than one $PATH directory (the
        // documented `~/.local/bin` vs `~/.cargo/bin` split) makes every
        // fixture's `br doctor` emit a `br_path_dupes` WARN, which fails
        // `healthy_workspace_baseline` for a reason that has nothing to do
        // with the workspace under test. Drop only the later `br` directories,
        // so `sqlite3`, `jq`, and `bash` stay reachable — several fixtures
        // shell out to them. The `multiple_br_in_path` fixture builds its own
        // $PATH, so duplicate detection is still exercised deliberately.
        .env("PATH", common::cli::deduplicated_br_path())
        // Strip developer env that might confuse `br doctor` discovery.
        .env_remove("BD_DB")
        .env_remove("BD_DATABASE")
        .env_remove("BEADS_DB")
        .env_remove("BEADS_CACHE_DIR")
        .env_remove("BEADS_DIR")
        .env_remove("BEADS_JSONL")
        .env_remove("BR_STARTUP_CACHE")
        .output()
        .expect("spawn run_all.sh");

    if !output.status.success() {
        eprintln!(
            "--- run_all.sh stdout ---\n{}",
            String::from_utf8_lossy(&output.stdout)
        );
        eprintln!(
            "--- run_all.sh stderr ---\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        panic!(
            "doctor fixture suite failed: exit={:?}",
            output.status.code()
        );
    }

    // Print summary line so `cargo test --nocapture` is informative.
    let stdout = String::from_utf8_lossy(&output.stdout);
    if let Some(summary_line) = stdout
        .lines()
        .rev()
        .find(|line| line.starts_with("Summary: "))
    {
        println!("doctor fixture suite: {summary_line}");
    }
}

fn which(cmd: &str) -> Option<PathBuf> {
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(cmd);
        if candidate.is_file() {
            // Treat as executable; on Unix we could check mode bits but
            // `is_file()` is fine for jq/bash on a development host.
            return Some(candidate);
        }
    }
    None
}
