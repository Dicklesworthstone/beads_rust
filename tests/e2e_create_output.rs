use assert_cmd::assert::OutputAssertExt;
use std::collections::BTreeSet;
use std::process::Command;
use tempfile::TempDir;
use toon_rust::try_decode;

fn isolated_tempdir() -> TempDir {
    let mut root = std::env::temp_dir();
    while let Some(workspace) = root
        .ancestors()
        .find(|ancestor| ancestor.join(".beads").is_dir() || ancestor.join("_beads").is_dir())
        .map(std::path::Path::to_path_buf)
    {
        root = workspace
            .parent()
            .unwrap_or_else(|| {
                panic!(
                    "no temp root exists outside enclosing workspace {}",
                    workspace.display()
                )
            })
            .to_path_buf();
    }
    TempDir::new_in(root).expect("create isolated temp dir")
}

fn extract_issues_array(stdout: &[u8]) -> Vec<serde_json::Value> {
    let payload: serde_json::Value = serde_json::from_slice(stdout).expect("list output json");
    if let Some(issues) = payload.as_array() {
        return issues.clone();
    }
    payload
        .get("issues")
        .and_then(serde_json::Value::as_array)
        .cloned()
        .expect("list output issues array")
}

/// Test that the --title flag works as an alternative to positional argument
/// This was added to fix GitHub issue #7 where --title-flag was used instead of --title
#[test]
fn test_create_with_title_flag() {
    let temp = isolated_tempdir();
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
    let temp = isolated_tempdir();
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
fn test_create_rejects_positional_and_title_flag_together() {
    let temp = isolated_tempdir();
    let path = temp.path();

    let bin = assert_cmd::cargo::cargo_bin!("br");

    Command::new(bin.as_os_str())
        .current_dir(path)
        .arg("init")
        .assert()
        .success();

    let output = Command::new(bin.as_os_str())
        .current_dir(path)
        .arg("create")
        .arg("Positional Title")
        .arg("--title")
        .arg("Flag Title")
        .arg("--json")
        .output()
        .expect("create with conflicting title inputs");

    assert!(
        !output.status.success(),
        "create should reject conflicting title inputs: {output:?}"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cannot be used with") && stderr.contains("--title"),
        "expected clap conflict for --title, got: {stderr}"
    );

    let list_output = Command::new(bin.as_os_str())
        .current_dir(path)
        .arg("list")
        .arg("--json")
        .output()
        .expect("list after rejected create");

    assert!(
        list_output.status.success(),
        "list after rejected create failed: {list_output:?}"
    );
    let issues = extract_issues_array(&list_output.stdout);
    assert!(
        issues.is_empty(),
        "rejected create must not create any issues: {issues:?}"
    );
}

#[test]
fn test_create_json_output_includes_labels_and_deps() {
    let temp = isolated_tempdir();
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

#[test]
fn test_create_toon_output_decodes_single_issue() {
    let temp = isolated_tempdir();
    let path = temp.path();

    let bin = assert_cmd::cargo::cargo_bin!("br");

    Command::new(bin.as_os_str())
        .current_dir(path)
        .arg("init")
        .assert()
        .success();

    let output = Command::new(bin.as_os_str())
        .current_dir(path)
        .arg("create")
        .arg("TOON issue")
        .env("BR_OUTPUT_FORMAT", "toon")
        .output()
        .expect("create with toon output");

    assert!(
        output.status.success(),
        "create --format toon failed: {output:?}"
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    let decoded = try_decode(stdout.trim(), None).expect("valid TOON");
    let decoded_json: serde_json::Value = decoded.into();

    assert_eq!(decoded_json["title"].as_str(), Some("TOON issue"));
    assert!(decoded_json["id"].as_str().is_some());
}

#[test]
fn test_create_file_empty_markdown_emits_empty_toon_array() {
    let temp = isolated_tempdir();
    let path = temp.path();
    let markdown_path = path.join("empty.md");

    let bin = assert_cmd::cargo::cargo_bin!("br");

    Command::new(bin.as_os_str())
        .current_dir(path)
        .arg("init")
        .assert()
        .success();

    std::fs::write(&markdown_path, "").expect("write empty markdown import");

    let output = Command::new(bin.as_os_str())
        .current_dir(path)
        .arg("create")
        .arg("--file")
        .arg("empty.md")
        .env("BR_OUTPUT_FORMAT", "toon")
        .output()
        .expect("create --file with empty markdown in toon mode");

    assert!(
        output.status.success(),
        "create --file with empty markdown failed: {output:?}"
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    let decoded = try_decode(stdout.trim(), None).expect("valid TOON");
    let decoded_json: serde_json::Value = decoded.into();

    assert_eq!(
        decoded_json,
        serde_json::Value::Array(Vec::new()),
        "empty markdown imports should emit an empty structured payload in TOON mode"
    );
}

#[test]
fn test_create_file_silent_outputs_only_ids() {
    let temp = isolated_tempdir();
    let path = temp.path();
    let markdown_path = path.join("issues.md");

    let bin = assert_cmd::cargo::cargo_bin!("br");

    Command::new(bin.as_os_str())
        .current_dir(path)
        .arg("init")
        .assert()
        .success();

    std::fs::write(
        &markdown_path,
        "## First imported issue\n\n## Second imported issue\n",
    )
    .expect("write markdown import");

    let output = Command::new(bin.as_os_str())
        .current_dir(path)
        .arg("create")
        .arg("--file")
        .arg("issues.md")
        .arg("--silent")
        .output()
        .expect("create --file --silent");

    assert!(
        output.status.success(),
        "create --file --silent failed: {output:?}"
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    let lines: Vec<&str> = stdout.lines().collect();

    assert_eq!(lines.len(), 2, "silent import should print one ID per line");
    let list_output = Command::new(bin.as_os_str())
        .current_dir(path)
        .arg("list")
        .arg("--json")
        .output()
        .expect("list issues after silent import");

    assert!(
        list_output.status.success(),
        "list after silent import failed: {list_output:?}"
    );

    let expected_ids: BTreeSet<String> = extract_issues_array(&list_output.stdout)
        .iter()
        .map(|issue| issue["id"].as_str().expect("issue id present").to_string())
        .collect();
    let actual_ids: BTreeSet<String> = lines.iter().map(|line| (*line).to_string()).collect();

    assert_eq!(
        actual_ids, expected_ids,
        "silent import should print the raw created IDs exactly"
    );

    for line in lines {
        assert!(
            line.trim() == line,
            "silent import should not add surrounding whitespace: {line:?}"
        );
        assert!(
            !line.contains(':'),
            "silent import should not include titles or status text: {line}"
        );
    }
}
