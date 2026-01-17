use assert_cmd::prelude::*;
use std::process::Command;

#[test]
fn test_create_json_output_includes_labels_and_deps() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path();

    let bin = assert_cmd::cargo::cargo_bin!("br");

    // Init
    Command::new(bin)
        .current_dir(path)
        .arg("init")
        .assert()
        .success();

    // Create blocking issue first
    let output = Command::new(bin)
        .current_dir(path)
        .arg("create")
        .arg("Blocker")
        .arg("--json")
        .output()
        .expect("create blocker");

    assert!(
        output.status.success(),
        "Failed to create blocking issue: {output:?}"
    );

    let blocker_json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let blocker_id = blocker_json["id"].as_str().unwrap();

    // Create issue with label and dep
    let output = Command::new(bin)
        .current_dir(path)
        .arg("create")
        .arg("My Issue")
        .arg("--labels")
        .arg("bug")
        .arg("--deps")
        .arg(blocker_id)
        .arg("--json")
        .output()
        .expect("Failed to run create issue");

    assert!(
        output.status.success(),
        "Failed to create issue with label and dep: {output:?}"
    );

    let issue_json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    // Verify fields
    let labels = issue_json["labels"]
        .as_array()
        .expect("labels should be an array");
    let deps = issue_json["dependencies"]
        .as_array()
        .expect("dependencies should be an array");

    assert!(
        labels.iter().any(|l| l.as_str() == Some("bug")),
        "Labels should contain 'bug'"
    );
    assert!(
        deps.iter()
            .any(|d| d["depends_on_id"].as_str() == Some(blocker_id)),
        "Dependencies should contain blocker ID"
    );
}