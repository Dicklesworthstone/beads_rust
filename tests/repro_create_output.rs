mod common;

use assert_cmd::prelude::*;
use std::process::Command;
use tempfile::TempDir;

fn isolated_tempdir() -> TempDir {
    TempDir::new_in(common::cli::isolated_temp_root()).expect("create isolated temp dir")
}

#[test]
fn test_create_json_output_is_single_object() {
    let temp = isolated_tempdir();
    let path = temp.path();

    let bin = assert_cmd::cargo::cargo_bin!("br");

    // Init
    Command::new(bin)
        .current_dir(path)
        .arg("init")
        .assert()
        .success();

    // Create issue
    let output = Command::new(bin)
        .current_dir(path)
        .arg("create")
        .arg("Single Object Check")
        .arg("--json")
        .output()
        .expect("create issue");

    assert!(output.status.success());

    // Parse JSON
    let json: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid json");

    // Verify it is an object, NOT an array
    assert!(
        json.is_object(),
        "Output should be a JSON object, got: {json:?}"
    );
    assert!(!json.is_array(), "Output should NOT be a JSON array");

    // Verify expected fields
    assert!(json.get("id").is_some());
    assert!(json.get("title").is_some());
}

#[test]
fn test_create_dry_run_plain_output_is_line_oriented() {
    let temp = isolated_tempdir();
    let path = temp.path();

    let bin = assert_cmd::cargo::cargo_bin!("br");

    Command::new(bin)
        .current_dir(path)
        .arg("init")
        .assert()
        .success();

    let output = Command::new(bin)
        .current_dir(path)
        .env("NO_COLOR", "1")
        .arg("create")
        .arg("Dry run output check")
        .arg("--dry-run")
        .arg("--type")
        .arg("task")
        .arg("--priority")
        .arg("2")
        .output()
        .expect("create dry run");

    assert!(output.status.success());

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();

    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("Dry run: would create issue ")),
        "missing dry-run header: {stdout}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line == &"Title: Dry run output check"),
        "missing standalone title line: {stdout}"
    );
    assert!(
        lines.iter().any(|line| line == &"Type: task"),
        "missing standalone type line: {stdout}"
    );
    assert!(
        lines.iter().any(|line| line == &"Priority: P2"),
        "missing standalone priority line: {stdout}"
    );
}
