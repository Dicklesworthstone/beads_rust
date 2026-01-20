use assert_cmd::prelude::*;
use std::process::Command;

/// Test that the --title flag works as an alternative to positional argument
/// This was added to fix GitHub issue #7 where --title-flag was used instead of --title
#[test]
fn test_create_with_title_flag() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path();

    let bin = assert_cmd::cargo::cargo_bin!("br");

    // Init
    Command::new(bin.as_os_str())
        .current_dir(path)
        .arg("init")
        .assert()
        .success();

    // Create issue using --title flag (not positional argument)
    let output = Command::new(bin.as_os_str())
        .current_dir(path)
        .arg("create")
        .arg("--title")
        .arg("Issue via title flag")
        .arg("--json")
        .output()
        .expect("create with --title flag");

    assert!(
        output.status.success(),
        "Failed to create issue with --title flag: {output:?}"
    );

    let issue_json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        issue_json["title"].as_str(),
        Some("Issue via title flag"),
        "Title should match what was passed via --title flag"
    );
}

/// Test that positional title and --title flag behave consistently
#[test]
fn test_create_positional_vs_title_flag() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path();

    let bin = assert_cmd::cargo::cargo_bin!("br");

    // Init
    Command::new(bin.as_os_str())
        .current_dir(path)
        .arg("init")
        .assert()
        .success();

    // Create with positional
    let output1 = Command::new(bin.as_os_str())
        .current_dir(path)
        .arg("create")
        .arg("Positional Title")
        .arg("--json")
        .output()
        .expect("create with positional");

    assert!(output1.status.success());
    let json1: serde_json::Value = serde_json::from_slice(&output1.stdout).unwrap();

    // Create with --title flag
    let output2 = Command::new(bin.as_os_str())
        .current_dir(path)
        .arg("create")
        .arg("--title")
        .arg("Flag Title")
        .arg("--json")
        .output()
        .expect("create with --title");

    assert!(output2.status.success());
    let json2: serde_json::Value = serde_json::from_slice(&output2.stdout).unwrap();

    // Both should have proper titles
    assert_eq!(json1["title"].as_str(), Some("Positional Title"));
    assert_eq!(json2["title"].as_str(), Some("Flag Title"));
}

#[test]
fn test_create_json_output_includes_labels_and_deps() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path();

    let bin = assert_cmd::cargo::cargo_bin!("br");

    // Init
    Command::new(bin.as_os_str())
        .current_dir(path)
        .arg("init")
        .assert()
        .success();

    // Create blocking issue first
    let output = Command::new(bin.as_os_str())
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
    let output = Command::new(bin.as_os_str())
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
