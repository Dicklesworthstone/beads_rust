//! End-to-end tests for the orphans command.
//!
//! The orphans command scans git commit messages for issue ID references
//! and identifies issues that are still `open/in_progress` but referenced
//! in commits (suggesting they may have been implemented but not closed).

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br, run_br_with_env, run_br_with_stdin};
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

fn rewrite_jsonl_issue_as_closed(workspace: &BrWorkspace, issue_id: &str) {
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let contents = fs::read_to_string(&jsonl_path).expect("read issues.jsonl");

    let rewritten = contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let mut issue: Value = serde_json::from_str(line).expect("parse issue json");
            if issue["id"].as_str() == Some(issue_id) {
                issue["status"] = Value::String("closed".to_string());
                issue["updated_at"] = Value::String("2099-01-01T00:00:00Z".to_string());
                issue["closed_at"] = Value::String("2099-01-01T00:00:00Z".to_string());
                issue["close_reason"] = Value::String("Closed via JSONL edit".to_string());
            }
            serde_json::to_string(&issue).expect("serialize issue json")
        })
        .collect::<Vec<_>>()
        .join("\n");

    fs::write(&jsonl_path, format!("{rewritten}\n")).expect("write issues.jsonl");
}

fn imported_issue_from_seed(seed_issue: &Value, id: &str, title: &str, description: &str) -> Value {
    let mut issue = seed_issue.clone();
    issue["id"] = Value::String(id.to_string());
    issue["title"] = Value::String(title.to_string());
    issue["description"] = Value::String(description.to_string());
    issue["content_hash"] = Value::Null;
    issue
}

fn mark_imported_issue_closed(issue: &mut Value) {
    issue["status"] = Value::String("closed".to_string());
    issue["closed_at"] = Value::String("2099-01-01T00:00:00Z".to_string());
    issue["close_reason"] = Value::String("Closed before commit scan".to_string());
}

fn assert_mixed_prefix_dotted_orphans(stdout: &str) {
    let payload = extract_json_payload(stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse orphans JSON");
    let arr = json.as_array().expect("orphans output should be an array");
    let ids: Vec<&str> = arr
        .iter()
        .filter_map(|orphan| orphan["issue_id"].as_str())
        .collect();

    assert!(
        ids.contains(&"other-abc12"),
        "missing mixed-prefix root issue in {ids:?}"
    );
    assert!(
        ids.contains(&"other-abc12.1"),
        "missing dotted mixed-prefix child issue in {ids:?}"
    );
    assert!(
        !ids.contains(&"other-dead99.1"),
        "closed dotted child should not be reported as an orphan"
    );

    let child = arr
        .iter()
        .find(|orphan| orphan["issue_id"].as_str() == Some("other-abc12.1"))
        .expect("dotted child orphan should be present");
    assert_eq!(
        child["title"].as_str(),
        Some("Imported dotted child"),
        "dotted child should retain imported issue details"
    );
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
fn e2e_orphans_auto_imports_newer_jsonl_before_scanning_issue_state() {
    common::init_test_logging();
    info!("e2e_orphans_auto_imports_newer_jsonl_before_scanning_issue_state: starting");
    let workspace = BrWorkspace::new();

    init_git(&workspace, "git_init");
    let init = run_br(&workspace, ["init"], "br_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Issue closed only in JSONL"],
        "create_issue",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let issue_id = parse_created_id(&create.stdout);

    git_commit(
        &workspace,
        &format!("Implement from stale DB ({issue_id})"),
        "commit_ref",
    );

    rewrite_jsonl_issue_as_closed(&workspace, &issue_id);

    let orphans = run_br(&workspace, ["orphans"], "orphans_auto_import");
    assert!(
        orphans.status.success(),
        "orphans failed: {}",
        orphans.stderr
    );
    assert!(
        orphans.stdout.contains("No orphan"),
        "expected auto-imported closed issue to disappear from orphan list, got: {}",
        orphans.stdout
    );
    info!("e2e_orphans_auto_imports_newer_jsonl_before_scanning_issue_state: assertions passed");
}

#[test]
fn e2e_orphans_fix_auto_flushes_closed_issue_to_jsonl() {
    common::init_test_logging();
    info!("e2e_orphans_fix_auto_flushes_closed_issue_to_jsonl: starting");
    let workspace = BrWorkspace::new();

    init_git(&workspace, "git_init");
    let init = run_br(&workspace, ["init"], "br_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Close via orphans fix"],
        "create_issue",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let issue_id = parse_created_id(&create.stdout);

    git_commit(
        &workspace,
        &format!("Implement orphaned issue {issue_id}"),
        "commit_ref",
    );

    let fix = run_br_with_stdin(&workspace, ["orphans", "--fix"], "y\n", "orphans_fix");
    assert!(fix.status.success(), "orphans --fix failed: {}", fix.stderr);

    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let exported_issue = fs::read_to_string(&jsonl_path)
        .expect("read issues.jsonl")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).expect("parse issue json"))
        .find(|issue| issue["id"].as_str() == Some(issue_id.as_str()))
        .expect("exported issue");

    assert_eq!(
        exported_issue["status"].as_str(),
        Some("closed"),
        "orphans --fix must auto-flush the nested close to JSONL"
    );
    assert_eq!(
        exported_issue["close_reason"].as_str(),
        Some("Implemented (detected by orphans scan)")
    );

    info!("e2e_orphans_fix_auto_flushes_closed_issue_to_jsonl: assertions passed");
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

#[test]
fn e2e_orphans_detects_mixed_prefix_and_dotted_child_refs() {
    common::init_test_logging();
    info!("e2e_orphans_detects_mixed_prefix_and_dotted_child_refs: starting");
    let workspace = BrWorkspace::new();

    init_git(&workspace, "git_init");
    let init = run_br(
        &workspace,
        ["init", "--prefix", "local"],
        "br_init_local_prefix",
    );
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Local seed issue", "--json"],
        "create_seed_issue",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let seed_payload = extract_json_payload(&create.stdout);
    let seed_issue: Value = serde_json::from_str(&seed_payload).expect("seed issue JSON");

    let imported_root = imported_issue_from_seed(
        &seed_issue,
        "other-abc12",
        "Imported mixed-prefix root",
        "Open issue with a non-default prefix",
    );
    let imported_child = imported_issue_from_seed(
        &seed_issue,
        "other-abc12.1",
        "Imported dotted child",
        "Open hierarchical issue with a non-default prefix",
    );
    let mut imported_closed_child = imported_issue_from_seed(
        &seed_issue,
        "other-dead99.1",
        "Imported closed dotted child",
        "Closed hierarchical issue with a non-default prefix",
    );
    mark_imported_issue_closed(&mut imported_closed_child);

    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    fs::write(
        &jsonl_path,
        format!(
            "{}\n{}\n{}\n{}\n",
            serde_json::to_string(&seed_issue).expect("serialize seed issue"),
            serde_json::to_string(&imported_root).expect("serialize imported root"),
            serde_json::to_string(&imported_child).expect("serialize imported child"),
            serde_json::to_string(&imported_closed_child).expect("serialize imported closed child"),
        ),
    )
    .expect("write mixed-prefix issues.jsonl");

    let import = run_br(
        &workspace,
        ["sync", "--import-only", "--json"],
        "sync_import_mixed_prefix_dotted",
    );
    assert!(
        import.status.success(),
        "sync --import-only failed: {}",
        import.stderr
    );

    git_commit(
        &workspace,
        "Implement other-abc12 and child other-abc12.1; closed other-dead99.1",
        "commit_mixed_prefix_dotted",
    );

    let orphans = run_br(
        &workspace,
        ["orphans", "--json"],
        "orphans_mixed_prefix_dotted",
    );
    assert!(
        orphans.status.success(),
        "orphans failed: {}",
        orphans.stderr
    );

    assert_mixed_prefix_dotted_orphans(&orphans.stdout);

    info!("e2e_orphans_detects_mixed_prefix_and_dotted_child_refs: assertions passed");
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
fn e2e_orphans_fix_before_init_returns_empty() {
    common::init_test_logging();
    info!("e2e_orphans_fix_before_init_returns_empty: starting");
    let workspace = BrWorkspace::new();

    let orphans = run_br_with_stdin(
        &workspace,
        ["orphans", "--fix"],
        "\n",
        "orphans_fix_no_init",
    );
    assert!(
        orphans.status.success(),
        "orphans --fix should succeed before init: {}",
        orphans.stderr
    );
    assert!(
        orphans.stdout.contains("No orphan"),
        "expected empty orphans message, got: {}",
        orphans.stdout
    );
    info!("e2e_orphans_fix_before_init_returns_empty: assertions passed");
}

#[test]
fn e2e_orphans_fix_before_init_rejects_machine_output() {
    common::init_test_logging();
    info!("e2e_orphans_fix_before_init_rejects_machine_output: starting");
    let workspace = BrWorkspace::new();

    let orphans = run_br_with_stdin(
        &workspace,
        ["--json", "orphans", "--fix"],
        "\n",
        "orphans_fix_json_no_init",
    );
    assert!(
        !orphans.status.success(),
        "orphans --fix --json should fail before init"
    );
    assert!(
        orphans.stdout.contains("--fix is interactive"),
        "expected interactive-mode error, got: {}",
        orphans.stdout
    );
    info!("e2e_orphans_fix_before_init_rejects_machine_output: assertions passed");
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
fn e2e_orphans_robot_flag_overrides_toon_env_output() {
    common::init_test_logging();
    info!("e2e_orphans_robot_flag_overrides_toon_env_output: starting");
    let workspace = BrWorkspace::new();

    init_git(&workspace, "git_init");
    let init = run_br(&workspace, ["init"], "br_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Robot TOON override"],
        "create_issue",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let issue_id = parse_created_id(&create.stdout);

    git_commit(
        &workspace,
        &format!("Implement TOON override ({issue_id})"),
        "commit_ref",
    );

    let orphans = run_br_with_env(
        &workspace,
        ["orphans", "--robot"],
        [("TOON_DEFAULT_FORMAT", "toon")],
        "orphans_robot_toon_env",
    );
    assert!(
        orphans.status.success(),
        "orphans failed: {}",
        orphans.stderr
    );

    let payload = extract_json_payload(&orphans.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse JSON");
    assert!(
        json.is_array(),
        "robot flag should override TOON env and produce JSON array"
    );
    info!("e2e_orphans_robot_flag_overrides_toon_env_output: assertions passed");
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
fn e2e_orphans_empty_robot_output_overrides_toon_env_output() {
    common::init_test_logging();
    info!("e2e_orphans_empty_robot_output_overrides_toon_env_output: starting");
    let workspace = BrWorkspace::new();

    init_git(&workspace, "git_init");
    let init = run_br(&workspace, ["init"], "br_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let orphans = run_br_with_env(
        &workspace,
        ["orphans", "--robot"],
        [("TOON_DEFAULT_FORMAT", "toon")],
        "orphans_empty_robot_toon_env",
    );
    assert!(
        orphans.status.success(),
        "orphans failed: {}",
        orphans.stderr
    );

    let payload = extract_json_payload(&orphans.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse JSON");
    assert!(
        json.is_array(),
        "robot flag should still return JSON for empty output"
    );
    assert!(json.as_array().unwrap().is_empty(), "expected empty array");
    info!("e2e_orphans_empty_robot_output_overrides_toon_env_output: assertions passed");
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
