use assert_cmd::Command;
use std::fs;
use tempfile::TempDir;

#[test]
fn debug_sync_status_output() {
    let temp = TempDir::new().unwrap();
    let root = temp.path();
    let beads_dir = root.join(".beads");
    fs::create_dir_all(&beads_dir).unwrap();

    // Create config
    fs::write(beads_dir.join("config.yaml"), "issue_prefix: bd").unwrap();

    // Initialize
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("br"));
    cmd.current_dir(root);
    cmd.arg("init");
    cmd.assert().success();

    // Run sync --status --json
    let mut cmd = Command::new(assert_cmd::cargo::cargo_bin!("br"));
    cmd.current_dir(root);
    cmd.env("RUST_LOG", "beads_rust=debug");
    cmd.args(["sync", "--status", "--json"]);
    
    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    
    println!("--- STDOUT ---");
    println!("{stdout}");
    println!("--- STDERR ---");
    println!("{stderr}");
    println!("--------------");

    assert!(!stdout.trim().is_empty(), "Stdout is empty!");

    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("parse json");
    println!("Parsed JSON: {json:?}");
}
