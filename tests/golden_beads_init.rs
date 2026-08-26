//! Golden snapshot of the `.beads/` directory layout after `br init`.
//!
//! Freezes the set of files, their types, and text-file contents so that
//! changes to the init sequence (new files, removed files, format changes)
//! are caught by CI.
//!
//! Update workflow:
//!   INSTA_UPDATE=always cargo test --test golden_beads_init
//!   cargo insta review

mod common;

use assert_cmd::Command;
use insta::assert_snapshot;
use std::fmt::Write;
use std::fs;
use tempfile::TempDir;
use walkdir::WalkDir;

fn br_cmd() -> Command {
    Command::cargo_bin("br").expect("br binary")
}

fn init_fresh_workspace() -> TempDir {
    let dir = TempDir::new_in(common::cli::isolated_temp_root()).expect("temp dir");
    let mut cmd = br_cmd();
    cmd.current_dir(dir.path());
    cmd.args(["init", "--prefix", "bd"]);
    cmd.env("NO_COLOR", "1");
    cmd.env("HOME", dir.path());
    for (key, _) in std::env::vars_os() {
        let k = key.to_string_lossy();
        if k.starts_with("BD_")
            || k.starts_with("BEADS_")
            || matches!(
                k.as_ref(),
                "BR_OUTPUT_FORMAT" | "TOON_DEFAULT_FORMAT" | "TOON_STATS"
            )
        {
            cmd.env_remove(&key);
        }
    }
    let output = cmd.output().expect("run br init");
    assert!(
        output.status.success(),
        "br init failed:\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    dir
}

const BINARY_EXTENSIONS: &[&str] = &["db", "db-wal", "db-shm", "db-journal"];

fn is_binary(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| BINARY_EXTENSIONS.iter().any(|b| ext.ends_with(b)))
}

fn is_transient_sqlite(name: &str) -> bool {
    name.ends_with("-wal")
        || name.ends_with("-wal-cert")
        || name.ends_with("-wal-cert-head")
        || name.ends_with("-shm")
        || name.ends_with("-journal")
        // fsqlite's multi-process namespace sidecars are engine-managed and
        // recreated on demand, so they are transient in the same sense.
        || name.ends_with("-fsqlite-ns-gate")
        || name.ends_with("-fsqlite-ns-use")
        // fsqlite 0.3.6+ engine-upgrade bookkeeping, written beside the DB
        // and recreated on demand.
        || name.ends_with(".fsqlite-migration-state")
}

fn build_directory_listing(beads_dir: &std::path::Path) -> String {
    let mut out = String::new();
    writeln!(out, ".beads/").unwrap();

    let mut entries: Vec<_> = WalkDir::new(beads_dir)
        .sort_by_file_name()
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.path() != beads_dir)
        .collect();
    entries.sort_by(|a, b| a.path().cmp(b.path()));

    for entry in &entries {
        let rel = entry.path().strip_prefix(beads_dir).expect("strip prefix");
        let name = rel.to_string_lossy();
        if is_transient_sqlite(&name) {
            continue;
        }
        let ft = entry.file_type();
        if ft.is_dir() {
            writeln!(out, "  {name}/").unwrap();
        } else if ft.is_file() {
            let meta = fs::metadata(entry.path()).expect("metadata");
            let size = meta.len();
            if is_binary(entry.path()) {
                writeln!(out, "  {name}  [binary]").unwrap();
            } else {
                writeln!(out, "  {name}  ({size} bytes)").unwrap();
            }
        } else if ft.is_symlink() {
            writeln!(out, "  {name}  -> symlink").unwrap();
        }
    }
    out
}

fn build_text_contents(beads_dir: &std::path::Path) -> String {
    let mut out = String::new();

    let mut entries: Vec<_> = WalkDir::new(beads_dir)
        .sort_by_file_name()
        .into_iter()
        .filter_map(Result::ok)
        // Engine-managed sidecars are excluded here for the same reason the
        // directory listing and expected-file-set skip them: which ones the
        // storage engine materializes is an implementation detail. `is_binary`
        // alone does not catch them — fsqlite's `-fsqlite-ns-gate` is created
        // empty, so it reads as a text file.
        .filter(|e| {
            e.file_type().is_file()
                && !is_binary(e.path())
                && !is_transient_sqlite(&e.file_name().to_string_lossy())
        })
        .collect();
    entries.sort_by(|a, b| a.path().cmp(b.path()));

    for entry in &entries {
        let rel = entry.path().strip_prefix(beads_dir).expect("strip prefix");
        let name = rel.to_string_lossy();
        let content = fs::read_to_string(entry.path()).unwrap_or_else(|_| "<read error>".into());
        writeln!(out, "--- {name} ---").unwrap();
        if content.is_empty() {
            writeln!(out, "<empty>").unwrap();
        } else {
            write!(out, "{content}").unwrap();
            if !content.ends_with('\n') {
                writeln!(out).unwrap();
            }
        }
    }
    out
}

#[test]
fn golden_init_directory_listing() {
    let dir = init_fresh_workspace();
    let beads_dir = dir.path().join(".beads");
    assert!(beads_dir.is_dir(), ".beads/ should exist after init");

    let listing = build_directory_listing(&beads_dir);
    assert_snapshot!("init_directory_listing", listing);
}

#[test]
fn golden_init_text_contents() {
    let dir = init_fresh_workspace();
    let beads_dir = dir.path().join(".beads");

    let contents = build_text_contents(&beads_dir);
    assert_snapshot!("init_text_contents", contents);
}

#[test]
fn golden_init_issues_jsonl_empty() {
    let dir = init_fresh_workspace();
    let jsonl = dir.path().join(".beads/issues.jsonl");
    assert!(jsonl.exists(), "issues.jsonl should exist");
    let content = fs::read_to_string(&jsonl).expect("read issues.jsonl");
    assert!(
        content.is_empty(),
        "issues.jsonl should be empty after init, got: {content:?}"
    );
}

#[test]
fn golden_init_metadata_valid_json() {
    let dir = init_fresh_workspace();
    let meta_path = dir.path().join(".beads/metadata.json");
    assert!(meta_path.exists(), "metadata.json should exist");
    let content = fs::read_to_string(&meta_path).expect("read metadata.json");
    let parsed: serde_json::Value =
        serde_json::from_str(&content).expect("metadata.json should be valid JSON");
    assert!(parsed.is_object(), "metadata.json should be a JSON object");
    assert_eq!(
        parsed.get("database").and_then(|v| v.as_str()),
        Some("beads.db"),
        "metadata.json should reference beads.db"
    );
    assert_eq!(
        parsed.get("jsonl_export").and_then(|v| v.as_str()),
        Some("issues.jsonl"),
        "metadata.json should reference issues.jsonl"
    );
}

#[test]
fn golden_init_expected_file_set() {
    let dir = init_fresh_workspace();
    let beads_dir = dir.path().join(".beads");

    let files: std::collections::BTreeSet<String> = WalkDir::new(&beads_dir)
        .sort_by_file_name()
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .map(|e| {
            e.path()
                .strip_prefix(&beads_dir)
                .unwrap()
                .to_string_lossy()
                .to_string()
        })
        .filter(|name| !is_transient_sqlite(name))
        .collect();

    let expected: std::collections::BTreeSet<String> = [
        ".gitignore",
        "beads.db",
        "config.yaml",
        "issues.jsonl",
        "metadata.json",
    ]
    .iter()
    .map(std::string::ToString::to_string)
    .collect();

    assert_eq!(
        files,
        expected,
        "init should produce exactly the expected file set.\nMissing: {:?}\nExtra: {:?}",
        expected.difference(&files).collect::<Vec<_>>(),
        files.difference(&expected).collect::<Vec<_>>(),
    );
}
