//! End-to-end tests for the orphans command.
//!
//! The orphans command scans git commit messages for issue ID references
//! and identifies issues that are still `open/in_progress` but referenced
//! in commits (suggesting they may have been implemented but not closed).

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;
use std::fs;
use std::process::Command;
use tracing::info;

/// Initialize a git repository in the workspace.
fn init_git(workspace: &BrWorkspace, label: &str) {
    let output = Command::new("git")
        .current_dir(&workspace.root)
        .args(["init"])
        .output()
        .expect("git init");
    assert!(
        output.status.success(),
        "[{label}] git init failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Configure git user for commits
    let _ = Command::new("git")
        .current_dir(&workspace.root)
        .args(["config", "user.email", "test@example.com"])
        .output();
    let _ = Command::new("git")
        .current_dir(&workspace.root)
        .args(["config", "user.name", "Test User"])
        .output();
}

/// Make a git commit with the given message.
fn git_commit(workspace: &BrWorkspace, message: &str, label: &str) {
    // Create a dummy file to commit
    let file_path = workspace.root.join(format!("{label}.txt"));
    fs::write(&file_path, format!("Content for {label}")).expect("write file");

    let add = Command::new("git")
        .current_dir(&workspace.root)
        .args(["add", "."])
        .output()
        .expect("git add");
    assert!(
        add.status.success(),
        "[{label}] git add failed: {}",
        String::from_utf8_lossy(&add.stderr)
    );

    let commit = Command::new("git")
        .current_dir(&workspace.root)
        .args(["commit", "-m", message])
        .output()
        .expect("git commit");
    assert!(
        commit.status.success(),
        "[{label}] git commit failed: {}",
        String::from_utf8_lossy(&commit.stderr)
    );
}

/// Parse the created issue ID from create command output.
fn parse_created_id(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or("");
    // Handle both formats: "Created bd-xxx: title" and "✓ Created bd-xxx: title"
    let normalized = line.strip_prefix("✓ ").unwrap_or(line);
    let id_part = normalized
        .strip_prefix("Created ")
        .and_then(|rest| rest.split(':').next())
        .unwrap_or("");
    id_part.trim().to_string()
}

// =============================================================================
// Success Path Tests
// =============================================================================

#[test]
fn e2e_orphans_no_orphans_empty_list() {
    common::init_test_logging();
    info!("e2e_orphans_no_orphans_empty_list: starting");
    let workspace = BrWorkspace::new();

    // Initialize git and beads
    init_git(&workspace, "git_init");
    let init = run_br(&workspace, ["init"], "br_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create an issue but don't reference it in commits
    let create = run_br(&workspace, ["create", "Unreferenced issue"], "create_issue");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Make a commit without issue reference
    git_commit(&workspace, "Add feature without issue ref", "commit_no_ref");

    // Run orphans - should be empty
    let orphans = run_br(&workspace, ["orphans"], "orphans_empty");
    assert!(
        orphans.status.success(),
        "orphans failed: {}",
        orphans.stderr
    );
    assert!(
        orphans.stdout.contains("No orphan"),
        "expected empty orphans message, got: {}",
        orphans.stdout
    );
    info!("e2e_orphans_no_orphans_empty_list: assertions passed");
}

#[test]
fn e2e_orphans_detects_open_issue_in_commit() {
    common::init_test_logging();
    info!("e2e_orphans_detects_open_issue_in_commit: starting");
    let workspace = BrWorkspace::new();

    // Initialize git and beads
    init_git(&workspace, "git_init");
    let init = run_br(&workspace, ["init"], "br_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create an issue
    let create = run_br(
        &workspace,
        ["create", "Feature to implement"],
        "create_issue",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let issue_id = parse_created_id(&create.stdout);

    // Make a commit referencing the issue (with parentheses)
    let commit_msg = format!("Implement feature ({issue_id})");
    git_commit(&workspace, &commit_msg, "commit_with_ref");

    // Run orphans - should detect the open issue
    let orphans = run_br(&workspace, ["orphans"], "orphans_detect");
    assert!(
        orphans.status.success(),
        "orphans failed: {}",
        orphans.stderr
    );
    assert!(
        orphans.stdout.contains(&issue_id),
        "expected issue {} in output, got: {}",
        issue_id,
        orphans.stdout
    );
    assert!(
        orphans.stdout.contains("Feature to implement"),
        "expected title in output, got: {}",
        orphans.stdout
    );
    info!("e2e_orphans_detects_open_issue_in_commit: assertions passed");
}

#[test]
fn e2e_orphans_detects_issue_without_parens() {
    common::init_test_logging();
    info!("e2e_orphans_detects_issue_without_parens: starting");
    let workspace = BrWorkspace::new();

    // Initialize git and beads
    init_git(&workspace, "git_init");
    let init = run_br(&workspace, ["init"], "br_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create an issue
    let create = run_br(&workspace, ["create", "Bug fix needed"], "create_issue");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let issue_id = parse_created_id(&create.stdout);

    // Make a commit referencing the issue WITHOUT parentheses
    let commit_msg = format!("Fix bug {issue_id} in auth module");
    git_commit(&workspace, &commit_msg, "commit_no_parens");

    // Run orphans - should detect the issue
    let orphans = run_br(&workspace, ["orphans"], "orphans_no_parens");
    assert!(
        orphans.status.success(),
        "orphans failed: {}",
        orphans.stderr
    );
    assert!(
        orphans.stdout.contains(&issue_id),
        "expected issue {} in output, got: {}",
        issue_id,
        orphans.stdout
    );
    info!("e2e_orphans_detects_issue_without_parens: assertions passed");
}

#[test]
fn e2e_orphans_json_output_structure() {
    common::init_test_logging();
    info!("e2e_orphans_json_output_structure: starting");
    let workspace = BrWorkspace::new();

    // Initialize git and beads
    init_git(&workspace, "git_init");
    let init = run_br(&workspace, ["init"], "br_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create an issue
    let create = run_br(&workspace, ["create", "JSON test issue"], "create_issue");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let issue_id = parse_created_id(&create.stdout);

    // Make a commit referencing the issue
    let commit_msg = format!("Implement ({issue_id})");
    git_commit(&workspace, &commit_msg, "commit_ref");

    // Run orphans with --json
    let orphans = run_br(&workspace, ["orphans", "--json"], "orphans_json");
    assert!(
        orphans.status.success(),
        "orphans failed: {}",
        orphans.stderr
    );

    let payload = extract_json_payload(&orphans.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse JSON");

    assert!(json.is_array(), "expected JSON array");
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 1, "expected 1 orphan");

    let orphan = &arr[0];
    assert_eq!(orphan["issue_id"].as_str(), Some(issue_id.as_str()));
    assert_eq!(orphan["title"].as_str(), Some("JSON test issue"));
    assert_eq!(orphan["status"].as_str(), Some("open"));
    assert!(orphan["latest_commit"].is_string(), "missing latest_commit");
    assert!(
        orphan["latest_commit_message"].is_string(),
        "missing latest_commit_message"
    );
    info!("e2e_orphans_json_output_structure: assertions passed");
}

// =============================================================================
// Filtering Tests
// =============================================================================

#[test]
fn e2e_orphans_excludes_closed_issues() {
    common::init_test_logging();
    info!("e2e_orphans_excludes_closed_issues: starting");
    let workspace = BrWorkspace::new();

    // Initialize git and beads
    init_git(&workspace, "git_init");
    let init = run_br(&workspace, ["init"], "br_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create and close an issue
    let create = run_br(&workspace, ["create", "Already done issue"], "create_issue");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let issue_id = parse_created_id(&create.stdout);

    let close = run_br(
        &workspace,
        ["close", &issue_id, "--reason", "done"],
        "close_issue",
    );
    assert!(close.status.success(), "close failed: {}", close.stderr);

    // Make a commit referencing the closed issue
    let commit_msg = format!("Implement ({issue_id})");
    git_commit(&workspace, &commit_msg, "commit_closed");

    // Run orphans - should NOT include closed issue
    let orphans = run_br(&workspace, ["orphans", "--json"], "orphans_closed");
    assert!(
        orphans.status.success(),
        "orphans failed: {}",
        orphans.stderr
    );

    let payload = extract_json_payload(&orphans.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse JSON");
    let arr = json.as_array().unwrap();
    assert!(arr.is_empty(), "closed issue should not appear as orphan");
    info!("e2e_orphans_excludes_closed_issues: assertions passed");
}

#[test]
fn e2e_orphans_includes_in_progress_issues() {
    common::init_test_logging();
    info!("e2e_orphans_includes_in_progress_issues: starting");
    let workspace = BrWorkspace::new();

    // Initialize git and beads
    init_git(&workspace, "git_init");
    let init = run_br(&workspace, ["init"], "br_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create an issue and mark it in_progress
    let create = run_br(&workspace, ["create", "In progress issue"], "create_issue");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let issue_id = parse_created_id(&create.stdout);

    let update = run_br(
        &workspace,
        ["update", &issue_id, "--status", "in_progress"],
        "update_status",
    );
    assert!(update.status.success(), "update failed: {}", update.stderr);

    // Make a commit referencing the issue
    let commit_msg = format!("Work on ({issue_id})");
    git_commit(&workspace, &commit_msg, "commit_in_progress");

    // Run orphans - should include in_progress issue
    let orphans = run_br(&workspace, ["orphans", "--json"], "orphans_in_progress");
    assert!(
        orphans.status.success(),
        "orphans failed: {}",
        orphans.stderr
    );

    let payload = extract_json_payload(&orphans.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse JSON");
    let arr = json.as_array().unwrap();
    assert_eq!(arr.len(), 1, "expected in_progress issue as orphan");
    assert_eq!(arr[0]["status"].as_str(), Some("in_progress"));
    info!("e2e_orphans_includes_in_progress_issues: assertions passed");
}

// =============================================================================
// Edge Case Tests
// =============================================================================

#[test]
fn e2e_orphans_before_init_returns_empty() {
    common::init_test_logging();
    info!("e2e_orphans_before_init_returns_empty: starting");
    let workspace = BrWorkspace::new();

    // Initialize git but NOT beads
    init_git(&workspace, "git_init");

    // Run orphans - should return empty, not error
    let orphans = run_br(&workspace, ["orphans"], "orphans_no_init");
    assert!(
        orphans.status.success(),
        "orphans should succeed: {}",
        orphans.stderr
    );
    assert!(
        orphans.stdout.contains("No orphan"),
        "expected empty message, got: {}",
        orphans.stdout
    );
    info!("e2e_orphans_before_init_returns_empty: assertions passed");
}

#[test]
fn e2e_orphans_not_git_repo_returns_empty() {
    common::init_test_logging();
    info!("e2e_orphans_not_git_repo_returns_empty: starting");
    let workspace = BrWorkspace::new();

    // Initialize beads but NOT git
    let init = run_br(&workspace, ["init"], "br_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create an issue
    let create = run_br(&workspace, ["create", "Test issue"], "create_issue");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Run orphans - should return empty (no git repo)
    let orphans = run_br(&workspace, ["orphans"], "orphans_no_git");
    assert!(
        orphans.status.success(),
        "orphans should succeed: {}",
        orphans.stderr
    );
    assert!(
        orphans.stdout.contains("No orphan"),
        "expected empty message, got: {}",
        orphans.stdout
    );
    info!("e2e_orphans_not_git_repo_returns_empty: assertions passed");
}

#[test]
fn e2e_orphans_multiple_issues_multiple_commits() {
    common::init_test_logging();
    info!("e2e_orphans_multiple_issues_multiple_commits: starting");
    let workspace = BrWorkspace::new();

    // Initialize git and beads
    init_git(&workspace, "git_init");
    let init = run_br(&workspace, ["init"], "br_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create multiple issues
    let create1 = run_br(&workspace, ["create", "First issue"], "create_1");
    assert!(create1.status.success());
    let id1 = parse_created_id(&create1.stdout);

    let create2 = run_br(&workspace, ["create", "Second issue"], "create_2");
    assert!(create2.status.success());
    let id2 = parse_created_id(&create2.stdout);

    let create3 = run_br(&workspace, ["create", "Third issue"], "create_3");
    assert!(create3.status.success());
    let id3 = parse_created_id(&create3.stdout);

    // Close the third issue
    let close = run_br(&workspace, ["close", &id3, "--reason", "done"], "close_3");
    assert!(close.status.success());

    // Make commits referencing all three
    git_commit(&workspace, &format!("Implement ({id1})"), "commit_1");
    git_commit(
        &workspace,
        &format!("Fix ({id2}) and ({id3})"),
        "commit_2_3",
    );

    // Run orphans - should detect only id1 and id2 (id3 is closed)
    let orphans = run_br(&workspace, ["orphans", "--json"], "orphans_multi");
    assert!(
        orphans.status.success(),
        "orphans failed: {}",
        orphans.stderr
    );

    let payload = extract_json_payload(&orphans.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse JSON");
    let arr = json.as_array().unwrap();

    assert_eq!(arr.len(), 2, "expected 2 orphans (not closed one)");

    let ids: Vec<&str> = arr.iter().filter_map(|o| o["issue_id"].as_str()).collect();
    assert!(ids.contains(&id1.as_str()), "missing id1");
    assert!(ids.contains(&id2.as_str()), "missing id2");
    assert!(
        !ids.contains(&id3.as_str()),
        "should not include closed id3"
    );
    info!("e2e_orphans_multiple_issues_multiple_commits: assertions passed");
}

#[test]
fn e2e_orphans_robot_flag_json_output() {
    common::init_test_logging();
    info!("e2e_orphans_robot_flag_json_output: starting");
    let workspace = BrWorkspace::new();

    // Initialize git and beads
    init_git(&workspace, "git_init");
    let init = run_br(&workspace, ["init"], "br_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create an issue
    let create = run_br(&workspace, ["create", "Robot test"], "create_issue");
    assert!(create.status.success());
    let issue_id = parse_created_id(&create.stdout);

    // Make a commit
    git_commit(&workspace, &format!("Implement ({issue_id})"), "commit_ref");

    // Run orphans with --robot (should produce JSON like --json)
    let orphans = run_br(&workspace, ["orphans", "--robot"], "orphans_robot");
    assert!(
        orphans.status.success(),
        "orphans failed: {}",
        orphans.stderr
    );

    let payload = extract_json_payload(&orphans.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse JSON");
    assert!(json.is_array(), "robot flag should produce JSON array");
    info!("e2e_orphans_robot_flag_json_output: assertions passed");
}

#[test]
fn e2e_orphans_empty_json_array_when_no_orphans() {
    common::init_test_logging();
    info!("e2e_orphans_empty_json_array_when_no_orphans: starting");
    let workspace = BrWorkspace::new();

    // Initialize git and beads
    init_git(&workspace, "git_init");
    let init = run_br(&workspace, ["init"], "br_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // No issues, no commits with refs

    // Run orphans with --json
    let orphans = run_br(&workspace, ["orphans", "--json"], "orphans_empty_json");
    assert!(
        orphans.status.success(),
        "orphans failed: {}",
        orphans.stderr
    );

    let payload = extract_json_payload(&orphans.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse JSON");
    assert!(json.is_array());
    assert!(json.as_array().unwrap().is_empty(), "expected empty array");
    info!("e2e_orphans_empty_json_array_when_no_orphans: assertions passed");
}

#[test]
fn e2e_orphans_details_flag_shows_commit_info() {
    common::init_test_logging();
    info!("e2e_orphans_details_flag_shows_commit_info: starting");
    let workspace = BrWorkspace::new();

    // Initialize git and beads
    init_git(&workspace, "git_init");
    let init = run_br(&workspace, ["init"], "br_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create an issue
    let create = run_br(&workspace, ["create", "Details test issue"], "create_issue");
    assert!(create.status.success());
    let issue_id = parse_created_id(&create.stdout);

    // Make a commit with a distinctive message
    let commit_msg = format!("Implement feature XYZ ({issue_id})");
    git_commit(&workspace, &commit_msg, "commit_ref");

    // Run orphans with --details
    let orphans = run_br(&workspace, ["orphans", "--details"], "orphans_details");
    assert!(
        orphans.status.success(),
        "orphans failed: {}",
        orphans.stderr
    );

    // Should show commit info
    assert!(
        orphans.stdout.contains("Commit:"),
        "expected 'Commit:' label with --details, got: {}",
        orphans.stdout
    );
    assert!(
        orphans.stdout.contains("feature XYZ"),
        "expected commit message in output, got: {}",
        orphans.stdout
    );
    info!("e2e_orphans_details_flag_shows_commit_info: assertions passed");
}

#[test]
fn e2e_orphans_issue_referenced_multiple_times() {
    common::init_test_logging();
    info!("e2e_orphans_issue_referenced_multiple_times: starting");
    let workspace = BrWorkspace::new();

    // Initialize git and beads
    init_git(&workspace, "git_init");
    let init = run_br(&workspace, ["init"], "br_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create an issue
    let create = run_br(&workspace, ["create", "Multi-ref issue"], "create_issue");
    assert!(create.status.success());
    let issue_id = parse_created_id(&create.stdout);

    // Make multiple commits referencing the same issue
    git_commit(&workspace, &format!("Start ({issue_id})"), "commit_1");
    git_commit(&workspace, &format!("Continue ({issue_id})"), "commit_2");
    git_commit(&workspace, &format!("Finish ({issue_id})"), "commit_3");

    // Run orphans - issue should appear only once
    let orphans = run_br(&workspace, ["orphans", "--json"], "orphans_multi_ref");
    assert!(
        orphans.status.success(),
        "orphans failed: {}",
        orphans.stderr
    );

    let payload = extract_json_payload(&orphans.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse JSON");
    let arr = json.as_array().unwrap();

    assert_eq!(arr.len(), 1, "issue should appear only once");
    assert_eq!(arr[0]["issue_id"].as_str(), Some(issue_id.as_str()));

    // Should reference the latest commit (most recent first in git log)
    let commit_msg = arr[0]["latest_commit_message"].as_str().unwrap();
    assert!(
        commit_msg.contains("Finish"),
        "should reference latest commit, got: {commit_msg}"
    );
    info!("e2e_orphans_issue_referenced_multiple_times: assertions passed");
}
