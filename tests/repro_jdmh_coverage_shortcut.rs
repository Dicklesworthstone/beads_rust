//! Regression: `beads_rust-jdmh` — `sync --status` and the `--import-only`
//! stored-hash shortcut asserted health without verifying the DB actually
//! covers the JSONL issue set.
//!
//! Escalated from the 2026-07-26 jeffreys-skills.md incident (CalmStork): a
//! DB missing 101 issues present in the git-authoritative JSONL still got
//! `Status: In sync` and `JSONL is current (hash unchanged since last
//! import)`, because the shortcut trusts `METADATA_JSONL_CONTENT_HASH`
//! equality and nothing checks coverage.
//!
//! The lying-metadata state is manufactured directly: rebuild the DB from a
//! 1-row JSONL, restore the 2-row JSONL, then plant the 2-row file's content
//! hash in metadata so the byte/hash signals all say "current".

mod common;

use assert_cmd::Command;
use beads_rust::storage::SqliteStorage;
use beads_rust::sync::{METADATA_JSONL_CONTENT_HASH, compute_jsonl_hash};
use serde_json::Value;
use std::fs;
use std::path::Path;
use tempfile::TempDir;

fn br_cmd(cwd: &Path) -> Command {
    let mut cmd = Command::cargo_bin("br").expect("locate br binary");
    cmd.current_dir(cwd);
    cmd.env("NO_COLOR", "1");
    cmd.env("RUST_LOG", "warn");
    cmd.env("HOME", cwd);
    cmd.env("PATH", common::cli::deduplicated_br_path());
    for (key, _) in std::env::vars_os() {
        let key_s = key.to_string_lossy();
        if key_s.starts_with("BD_") || key_s.starts_with("BEADS_") {
            cmd.env_remove(&key);
        }
    }
    cmd
}

fn run_ok(cwd: &Path, args: &[&str]) -> String {
    let out = br_cmd(cwd).args(args).output().expect("spawn br");
    assert!(
        out.status.success(),
        "br {args:?} failed: stdout={} stderr={}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn isolated_tempdir() -> TempDir {
    TempDir::new_in(common::cli::isolated_temp_root()).expect("create isolated temp dir")
}

/// Build a workspace whose metadata claims the 2-row JSONL was imported while
/// the DB only holds 1 issue. Returns the workspace tempdir.
fn lying_metadata_workspace() -> TempDir {
    let tmp = isolated_tempdir();
    let ws = tmp.path();
    run_ok(ws, &["init", "--prefix", "jd"]);
    run_ok(
        ws,
        &["create", "first issue", "--type", "task", "--priority", "2"],
    );
    run_ok(ws, &["sync", "--flush-only"]);
    let jsonl_path = ws.join(".beads/issues.jsonl");
    let one_row = fs::read(&jsonl_path).expect("read 1-row jsonl");

    run_ok(
        ws,
        &[
            "create",
            "second issue",
            "--type",
            "task",
            "--priority",
            "2",
        ],
    );
    run_ok(ws, &["sync", "--flush-only"]);
    let two_rows = fs::read(&jsonl_path).expect("read 2-row jsonl");
    assert_ne!(one_row, two_rows);

    // Rebuild the DB from the 1-row file (JSONL-authoritative), then restore
    // the 2-row file and plant its hash so every byte/hash signal reports
    // "current" while the DB is missing an issue.
    fs::write(&jsonl_path, &one_row).expect("restore 1-row jsonl");
    run_ok(ws, &["sync", "--import-only", "--rebuild"]);
    fs::write(&jsonl_path, &two_rows).expect("restore 2-row jsonl");
    let two_row_hash = compute_jsonl_hash(&jsonl_path).expect("hash 2-row jsonl");
    {
        let mut storage =
            SqliteStorage::open(&ws.join(".beads/beads.db")).expect("open storage directly");
        storage
            .set_metadata(METADATA_JSONL_CONTENT_HASH, &two_row_hash)
            .expect("plant lying content hash");
    }
    tmp
}

#[test]
fn status_reports_coverage_drift_instead_of_in_sync() {
    let tmp = lying_metadata_workspace();
    let ws = tmp.path();

    let stdout = run_ok(ws, &["sync", "--status", "--json"]);
    let status: Value = serde_json::from_str(&stdout).expect("status json");
    assert_eq!(
        status["coverage_drift"],
        Value::Bool(true),
        "status must not assert health over a DB missing JSONL rows: {status}"
    );
    let coverage = &status["coverage"];
    assert_eq!(coverage["db_exportable_issues"], 1, "{status}");
    assert_eq!(coverage["jsonl_unique_ids"], 2, "{status}");

    let text = run_ok(ws, &["sync", "--status"]);
    assert!(
        text.contains("COVERAGE DRIFT"),
        "text status must surface the drift loudly, got: {text}"
    );
    assert!(
        !text.contains("Status: In sync"),
        "text status must not claim In sync under drift: {text}"
    );
}

#[test]
fn import_shortcut_rejects_uncovered_hash_match_and_imports() {
    let tmp = lying_metadata_workspace();
    let ws = tmp.path();

    let stdout = run_ok(ws, &["sync", "--import-only", "--json"]);
    let result: Value = serde_json::from_str(&stdout).expect("import json");
    assert_eq!(
        result["created"], 1,
        "import must fall through the stored-hash shortcut and pull the missing row: {result}"
    );

    // The DB now covers the JSONL; the shortcut may engage again and status
    // is genuinely in sync.
    let stdout = run_ok(ws, &["sync", "--status", "--json"]);
    let status: Value = serde_json::from_str(&stdout).expect("status json");
    assert_eq!(status["coverage_drift"], Value::Bool(false), "{status}");
    let stdout = run_ok(ws, &["sync", "--import-only", "--json"]);
    let result: Value = serde_json::from_str(&stdout).expect("second import json");
    assert_eq!(result["created"], 0, "{result}");
    assert_eq!(result["updated"], 0, "{result}");
}

#[test]
fn healthy_workspace_reports_no_drift() {
    let tmp = isolated_tempdir();
    let ws = tmp.path();
    run_ok(ws, &["init", "--prefix", "jd"]);
    run_ok(
        ws,
        &["create", "only issue", "--type", "task", "--priority", "2"],
    );
    run_ok(ws, &["sync", "--flush-only"]);

    let stdout = run_ok(ws, &["sync", "--status", "--json"]);
    let status: Value = serde_json::from_str(&stdout).expect("status json");
    assert_eq!(status["coverage_drift"], Value::Bool(false), "{status}");
    assert_eq!(status["coverage"]["db_exportable_issues"], 1, "{status}");
    assert_eq!(status["coverage"]["jsonl_unique_ids"], 1, "{status}");
}
