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

/// Coverage drift gate (beads_rust-yx0b): `tests/doctor_fixtures/COVERAGE.md`
/// maps every declared `fm-` finding id to the fixture(s) exercising it.
/// This test keeps the manifest honest in three directions:
///
/// 1. every `fm-` id in the `br doctor capabilities` envelope has a
///    manifest row (a new finding id without coverage bookkeeping fails);
/// 2. every fixture a row names exists on disk with `corrupt.sh` +
///    `assert.sh` (a renamed/deleted fixture fails);
/// 3. every fixture directory on disk is mentioned somewhere in the
///    manifest (a new fixture that was never mapped fails).
///
/// Rows whose kind starts with `exception` may name no fixture but must
/// give a reason inline. The envelope — not a source-code regex — is the
/// declared-set authority, so the gate tracks the agent-facing contract.
#[test]
fn fixture_coverage_manifest_is_complete() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fixtures_root = manifest_dir.join("tests").join("doctor_fixtures");
    let coverage_path = fixtures_root.join("COVERAGE.md");
    let coverage = std::fs::read_to_string(&coverage_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", coverage_path.display()));

    // Declared set: every quoted fm- string in the capabilities envelope.
    let out = Command::new(env!("CARGO_BIN_EXE_br"))
        .args(["doctor", "capabilities", "--format", "json"])
        .current_dir(&manifest_dir)
        .output()
        .expect("spawn br doctor capabilities");
    assert!(
        out.status.success(),
        "capabilities failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let envelope = String::from_utf8_lossy(&out.stdout).to_string();
    let mut declared: Vec<&str> = envelope
        .split('"')
        .filter(|token| token.starts_with("fm-"))
        .collect();
    declared.sort_unstable();
    declared.dedup();
    assert!(
        declared.len() >= 50,
        "suspiciously few fm- ids in the capabilities envelope ({}); did the finding-id map move?",
        declared.len()
    );

    // Manifest rows: `| fm-... | fixture, fixture | kind |`.
    let mut row_ids: Vec<String> = Vec::new();
    let mut named_fixtures: Vec<String> = Vec::new();
    for line in coverage.lines() {
        let Some(rest) = line.strip_prefix("| fm-") else {
            continue;
        };
        let mut cols = rest.split('|').map(str::trim);
        let id = format!("fm-{}", cols.next().unwrap_or_default());
        let fixtures_col = cols.next().unwrap_or_default().to_string();
        let kind_col = cols.next().unwrap_or_default().to_string();
        if fixtures_col == "—" || fixtures_col == "-" {
            assert!(
                kind_col.starts_with("exception"),
                "manifest row {id} names no fixture but kind is not an exception: {kind_col}"
            );
        } else {
            for fixture in fixtures_col.split(',').map(str::trim) {
                assert!(!fixture.is_empty(), "empty fixture name in row {id}");
                named_fixtures.push(fixture.to_string());
            }
        }
        row_ids.push(id);
    }

    // (1) every declared id has a row.
    let missing_rows: Vec<&&str> = declared
        .iter()
        .filter(|id| !row_ids.iter().any(|row| row == **id))
        .collect();
    assert!(
        missing_rows.is_empty(),
        "declared finding id(s) missing from COVERAGE.md: {missing_rows:?}"
    );

    // (2) every named fixture exists with the runnable pair.
    for fixture in &named_fixtures {
        let dir = fixtures_root.join(fixture);
        assert!(
            dir.join("corrupt.sh").is_file() && dir.join("assert.sh").is_file(),
            "COVERAGE.md names fixture '{fixture}' but {} lacks corrupt.sh/assert.sh",
            dir.display()
        );
    }

    // (3) every fixture dir on disk is mentioned somewhere in the manifest.
    let mut unmentioned: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&fixtures_root).expect("read fixtures root") {
        let entry = entry.expect("dir entry");
        let path = entry.path();
        if !path.is_dir() || !path.join("corrupt.sh").is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !coverage.contains(&name) {
            unmentioned.push(name);
        }
    }
    unmentioned.sort();
    assert!(
        unmentioned.is_empty(),
        "fixture dir(s) not mentioned in COVERAGE.md: {unmentioned:?}"
    );
}
