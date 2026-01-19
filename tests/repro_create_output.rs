use assert_cmd::prelude::*;
use std::process::Command;

#[test]
fn test_create_json_output_is_single_object() {
    let temp = tempfile::tempdir().unwrap();
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
