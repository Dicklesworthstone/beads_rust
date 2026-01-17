//! E2E tests for the `lint` command.
//!
//! The lint command validates issue templates by checking for required sections
//! based on issue type:
//! - Bug: "## Steps to Reproduce", "## Acceptance Criteria"
//! - Task/Feature: "## Acceptance Criteria"
//! - Epic: "## Success Criteria"
//!
//! Test coverage:
//! - Clean workspace scenarios (no warnings)
//! - Missing sections detection by issue type
//! - Filter tests (--type, --status, specific IDs)
//! - JSON output structure verification
//! - Error handling (before init, invalid filters)

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;

// =============================================================================
// Helper Functions
// =============================================================================

fn parse_created_id(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or("");
    let id_part = line
        .strip_prefix("Created ")
        .and_then(|rest| rest.split(':').next())
        .unwrap_or("");
    id_part.trim().to_string()
}

fn init_workspace(workspace: &BrWorkspace) {
    let init = run_br(workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
}

fn create_issue_with_description(
    workspace: &BrWorkspace,
    title: &str,
    issue_type: &str,
    description: Option<&str>,
) -> String {
    let mut args: Vec<String> = vec![
        "create".to_string(),
        title.to_string(),
        "--type".to_string(),
        issue_type.to_string(),
    ];

    if let Some(desc) = description {
        args.push("--description".to_string());
        args.push(desc.to_string());
    }

    let create = run_br(workspace, &args, &format!("create_{issue_type}"));
    assert!(create.status.success(), "create failed: {}", create.stderr);
    parse_created_id(&create.stdout)
}

// =============================================================================
// Clean Workspace Tests
// =============================================================================

#[test]
fn e2e_lint_clean_workspace_no_issues() {
    // Lint on empty workspace (no issues) should pass with no warnings
    let workspace = BrWorkspace::new();
    init_workspace(&workspace);

    let lint = run_br(&workspace, ["lint"], "lint_empty");
    assert!(lint.status.success(), "lint failed: {}", lint.stderr);
    assert!(
        lint.stdout.contains("No template warnings found"),
        "expected clean message, got: {}",
        lint.stdout
    );
}

#[test]
fn e2e_lint_clean_workspace_json_empty_results() {
    // JSON output on empty workspace should have empty results array
    let workspace = BrWorkspace::new();
    init_workspace(&workspace);

    let lint = run_br(&workspace, ["lint", "--json"], "lint_empty_json");
    assert!(lint.status.success(), "lint failed: {}", lint.stderr);

    let json_str = extract_json_payload(&lint.stdout);
    let json: Value = serde_json::from_str(&json_str).expect("valid JSON");

    assert_eq!(json["total"], 0, "expected 0 warnings");
    assert_eq!(json["issues"], 0, "expected 0 issues with warnings");
    assert!(
        json["results"].as_array().unwrap().is_empty(),
        "expected empty results array"
    );
}

#[test]
fn e2e_lint_issue_with_all_required_sections_passes() {
    // Bug with all required sections should not trigger warnings
    let workspace = BrWorkspace::new();
    init_workspace(&workspace);

    let description =
        "## Steps to Reproduce\n1. Do this\n2. Then that\n\n## Acceptance Criteria\n- Bug is fixed";
    create_issue_with_description(&workspace, "Complete bug", "bug", Some(description));

    let lint = run_br(&workspace, ["lint"], "lint_complete_bug");
    assert!(lint.status.success(), "lint failed: {}", lint.stderr);
    assert!(
        lint.stdout.contains("No template warnings found"),
        "expected no warnings for complete bug, got: {}",
        lint.stdout
    );
}

// =============================================================================
// Missing Sections Tests by Issue Type
// =============================================================================

#[test]
fn e2e_lint_bug_missing_steps_to_reproduce() {
    // Bug without "Steps to Reproduce" should warn
    let workspace = BrWorkspace::new();
    init_workspace(&workspace);

    let description = "## Acceptance Criteria\n- Bug is fixed";
    let id = create_issue_with_description(&workspace, "Incomplete bug", "bug", Some(description));

    let lint = run_br(&workspace, ["lint", "--json"], "lint_bug_missing_steps");
    // In JSON mode, exit code is always 0
    assert!(lint.status.success(), "lint failed: {}", lint.stderr);

    let json_str = extract_json_payload(&lint.stdout);
    let json: Value = serde_json::from_str(&json_str).expect("valid JSON");

    assert!(
        json["total"].as_i64().unwrap() >= 1,
        "expected at least 1 warning"
    );

    let results = json["results"].as_array().unwrap();
    let issue_result = results.iter().find(|r| r["id"] == id);
    assert!(issue_result.is_some(), "issue {id} not in results");

    let missing = issue_result.unwrap()["missing"].as_array().unwrap();
    assert!(
        missing
            .iter()
            .any(|m| m.as_str().unwrap().contains("Steps to Reproduce")),
        "expected missing 'Steps to Reproduce', got: {missing:?}"
    );
}

#[test]
fn e2e_lint_bug_missing_acceptance_criteria() {
    // Bug without "Acceptance Criteria" should warn
    let workspace = BrWorkspace::new();
    init_workspace(&workspace);

    let description = "## Steps to Reproduce\n1. Step one";
    let id = create_issue_with_description(&workspace, "Bug without AC", "bug", Some(description));

    let lint = run_br(&workspace, ["lint", "--json"], "lint_bug_missing_ac");
    assert!(lint.status.success(), "lint failed: {}", lint.stderr);

    let json_str = extract_json_payload(&lint.stdout);
    let json: Value = serde_json::from_str(&json_str).expect("valid JSON");

    let results = json["results"].as_array().unwrap();
    let issue_result = results.iter().find(|r| r["id"] == id);
    assert!(issue_result.is_some(), "issue {id} not in results");

    let missing = issue_result.unwrap()["missing"].as_array().unwrap();
    assert!(
        missing
            .iter()
            .any(|m| m.as_str().unwrap().contains("Acceptance Criteria")),
        "expected missing 'Acceptance Criteria', got: {missing:?}"
    );
}

#[test]
fn e2e_lint_bug_missing_all_sections() {
    // Bug without any required sections should have 2 warnings
    let workspace = BrWorkspace::new();
    init_workspace(&workspace);

    let id = create_issue_with_description(&workspace, "Bare bug", "bug", Some("Just a bug"));

    let lint = run_br(&workspace, ["lint", "--json"], "lint_bug_missing_all");
    assert!(lint.status.success(), "lint failed: {}", lint.stderr);

    let json_str = extract_json_payload(&lint.stdout);
    let json: Value = serde_json::from_str(&json_str).expect("valid JSON");

    let results = json["results"].as_array().unwrap();
    let issue_result = results.iter().find(|r| r["id"] == id);
    assert!(issue_result.is_some(), "issue {id} not in results");

    let warnings = issue_result.unwrap()["warnings"].as_i64().unwrap();
    assert_eq!(
        warnings, 2,
        "expected 2 warnings for bug missing all sections"
    );
}

#[test]
fn e2e_lint_task_missing_acceptance_criteria() {
    // Task without "Acceptance Criteria" should warn
    let workspace = BrWorkspace::new();
    init_workspace(&workspace);

    let id =
        create_issue_with_description(&workspace, "Task without AC", "task", Some("Just do it"));

    let lint = run_br(&workspace, ["lint", "--json"], "lint_task_missing_ac");
    assert!(lint.status.success(), "lint failed: {}", lint.stderr);

    let json_str = extract_json_payload(&lint.stdout);
    let json: Value = serde_json::from_str(&json_str).expect("valid JSON");

    let results = json["results"].as_array().unwrap();
    let issue_result = results.iter().find(|r| r["id"] == id);
    assert!(issue_result.is_some(), "issue {id} not in results");

    let missing = issue_result.unwrap()["missing"].as_array().unwrap();
    assert!(
        missing
            .iter()
            .any(|m| m.as_str().unwrap().contains("Acceptance Criteria")),
        "expected missing 'Acceptance Criteria', got: {missing:?}"
    );
}

#[test]
fn e2e_lint_epic_missing_success_criteria() {
    // Epic without "Success Criteria" should warn
    let workspace = BrWorkspace::new();
    init_workspace(&workspace);

    let id =
        create_issue_with_description(&workspace, "Epic without SC", "epic", Some("Big project"));

    let lint = run_br(&workspace, ["lint", "--json"], "lint_epic_missing_sc");
    assert!(lint.status.success(), "lint failed: {}", lint.stderr);

    let json_str = extract_json_payload(&lint.stdout);
    let json: Value = serde_json::from_str(&json_str).expect("valid JSON");

    let results = json["results"].as_array().unwrap();
    let issue_result = results.iter().find(|r| r["id"] == id);
    assert!(issue_result.is_some(), "issue {id} not in results");

    let missing = issue_result.unwrap()["missing"].as_array().unwrap();
    assert!(
        missing
            .iter()
            .any(|m| m.as_str().unwrap().contains("Success Criteria")),
        "expected missing 'Success Criteria', got: {missing:?}"
    );
}

#[test]
fn e2e_lint_chore_no_required_sections() {
    // Chore type has no required sections, should never warn
    let workspace = BrWorkspace::new();
    init_workspace(&workspace);

    create_issue_with_description(&workspace, "Simple chore", "chore", Some("Just cleanup"));

    let lint = run_br(&workspace, ["lint"], "lint_chore_no_sections");
    assert!(lint.status.success(), "lint failed: {}", lint.stderr);
    assert!(
        lint.stdout.contains("No template warnings found"),
        "chore should not have required sections, got: {}",
        lint.stdout
    );
}

// =============================================================================
// Filter Tests
// =============================================================================

#[test]
fn e2e_lint_filter_by_type_bug() {
    // --type bug should only lint bug issues
    let workspace = BrWorkspace::new();
    init_workspace(&workspace);

    // Create bug without required sections
    let bug_id = create_issue_with_description(&workspace, "Buggy bug", "bug", Some("Bug desc"));
    // Create task without required sections
    create_issue_with_description(&workspace, "Tasky task", "task", Some("Task desc"));

    let lint = run_br(
        &workspace,
        ["lint", "--type", "bug", "--json"],
        "lint_filter_bug",
    );
    assert!(lint.status.success(), "lint failed: {}", lint.stderr);

    let json_str = extract_json_payload(&lint.stdout);
    let json: Value = serde_json::from_str(&json_str).expect("valid JSON");

    let results = json["results"].as_array().unwrap();
    // Should only have the bug in results
    assert!(
        results.iter().all(|r| r["type"] == "bug"),
        "expected only bugs in results when filtering by type=bug"
    );
    assert!(
        results.iter().any(|r| r["id"] == bug_id),
        "bug {bug_id} should be in results"
    );
}

#[test]
fn e2e_lint_filter_by_status_all() {
    // --status all should include closed issues
    let workspace = BrWorkspace::new();
    init_workspace(&workspace);

    // Create and close a bug without required sections
    let bug_id = create_issue_with_description(&workspace, "Closed bug", "bug", Some("Closed"));
    let close = run_br(&workspace, ["close", &bug_id], "close_bug");
    assert!(close.status.success(), "close failed: {}", close.stderr);

    // Default lint should not include closed
    let lint_default = run_br(&workspace, ["lint", "--json"], "lint_status_default");
    let json_str = extract_json_payload(&lint_default.stdout);
    let json: Value = serde_json::from_str(&json_str).expect("valid JSON");
    assert!(
        !json["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == bug_id),
        "closed issue should not appear in default lint"
    );

    // --status all should include closed
    let lint_all = run_br(
        &workspace,
        ["lint", "--status", "all", "--json"],
        "lint_status_all",
    );
    assert!(
        lint_all.status.success(),
        "lint failed: {}",
        lint_all.stderr
    );

    let json_str = extract_json_payload(&lint_all.stdout);
    let json: Value = serde_json::from_str(&json_str).expect("valid JSON");
    assert!(
        json["results"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["id"] == bug_id),
        "closed issue should appear with --status all"
    );
}

#[test]
fn e2e_lint_specific_issue_id() {
    // Lint specific issue by ID
    let workspace = BrWorkspace::new();
    init_workspace(&workspace);

    // Create two bugs without required sections
    let bug1_id = create_issue_with_description(&workspace, "Bug one", "bug", Some("First"));
    let _bug2_id = create_issue_with_description(&workspace, "Bug two", "bug", Some("Second"));

    // Lint only bug1
    let lint = run_br(&workspace, ["lint", &bug1_id, "--json"], "lint_specific_id");
    assert!(lint.status.success(), "lint failed: {}", lint.stderr);

    let json_str = extract_json_payload(&lint.stdout);
    let json: Value = serde_json::from_str(&json_str).expect("valid JSON");

    let results = json["results"].as_array().unwrap();
    assert_eq!(
        results.len(),
        1,
        "expected exactly 1 result for specific ID"
    );
    assert_eq!(
        results[0]["id"], bug1_id,
        "result should be the specified bug"
    );
}

// =============================================================================
// JSON Output Structure Tests
// =============================================================================

#[test]
fn e2e_lint_json_output_structure() {
    // Verify JSON output has correct structure
    let workspace = BrWorkspace::new();
    init_workspace(&workspace);

    create_issue_with_description(&workspace, "Test bug", "bug", Some("Minimal"));

    let lint = run_br(&workspace, ["lint", "--json"], "lint_json_structure");
    assert!(lint.status.success(), "lint failed: {}", lint.stderr);

    let json_str = extract_json_payload(&lint.stdout);
    let json: Value = serde_json::from_str(&json_str).expect("valid JSON");

    // Check top-level fields
    assert!(json.get("total").is_some(), "missing 'total' field");
    assert!(json.get("issues").is_some(), "missing 'issues' field");
    assert!(json.get("results").is_some(), "missing 'results' field");

    // Check results array structure
    let results = json["results"].as_array().unwrap();
    if !results.is_empty() {
        let result = &results[0];
        assert!(result.get("id").is_some(), "result missing 'id' field");
        assert!(
            result.get("title").is_some(),
            "result missing 'title' field"
        );
        assert!(result.get("type").is_some(), "result missing 'type' field");
        assert!(
            result.get("warnings").is_some(),
            "result missing 'warnings' field"
        );
        assert!(
            result.get("missing").is_some(),
            "result missing 'missing' field"
        );
    }
}

#[test]
fn e2e_lint_json_exit_code_always_zero() {
    // In JSON mode, exit code should always be 0 (even with warnings)
    let workspace = BrWorkspace::new();
    init_workspace(&workspace);

    // Create bug without required sections (will have warnings)
    create_issue_with_description(&workspace, "Buggy", "bug", Some("No sections"));

    let lint = run_br(&workspace, ["lint", "--json"], "lint_json_exit_code");
    assert!(
        lint.status.success(),
        "JSON mode should always exit 0, got: {}",
        lint.status
    );
}

// =============================================================================
// Text Output Tests
// =============================================================================

#[test]
fn e2e_lint_text_output_warnings() {
    // Text mode with warnings should show formatted output
    let workspace = BrWorkspace::new();
    init_workspace(&workspace);

    let id = create_issue_with_description(&workspace, "Warning bug", "bug", Some("No sections"));

    let lint = run_br(&workspace, ["lint"], "lint_text_warnings");
    // Text mode exits non-zero when there are warnings
    // But we should still check the output format

    assert!(
        lint.stdout.contains(&id) || lint.stdout.contains("bug"),
        "text output should mention the issue"
    );
    assert!(
        lint.stdout.contains("Missing") || lint.stdout.contains("warning"),
        "text output should indicate missing sections"
    );
}

#[test]
fn e2e_lint_text_exit_code_nonzero_with_warnings() {
    // In text mode, exit code should be 1 when there are warnings
    let workspace = BrWorkspace::new();
    init_workspace(&workspace);

    create_issue_with_description(&workspace, "Warning bug", "bug", Some("No sections"));

    let lint = run_br(&workspace, ["lint"], "lint_text_exit_nonzero");
    assert!(
        !lint.status.success(),
        "text mode with warnings should exit non-zero"
    );
}

// =============================================================================
// Error Handling Tests
// =============================================================================

#[test]
fn e2e_lint_before_init_fails() {
    // Lint without init should fail
    let workspace = BrWorkspace::new();
    // Do NOT init

    let lint = run_br(&workspace, ["lint"], "lint_before_init");
    assert!(!lint.status.success(), "lint before init should fail");
    assert!(
        lint.stderr.contains("not found")
            || lint.stderr.contains("initialize")
            || lint.stderr.contains("No .beads"),
        "error should mention workspace not initialized, got: {}",
        lint.stderr
    );
}

#[test]
fn e2e_lint_nonexistent_id_error() {
    // Lint with nonexistent ID should handle gracefully
    let workspace = BrWorkspace::new();
    init_workspace(&workspace);

    let lint = run_br(
        &workspace,
        ["lint", "bd-nonexistent"],
        "lint_nonexistent_id",
    );
    // Should either fail or print an error message
    assert!(
        !lint.status.success()
            || lint.stderr.contains("not found")
            || lint.stdout.contains("not found"),
        "nonexistent ID should be handled"
    );
}

#[test]
fn e2e_lint_unknown_type_filter_no_matches() {
    // Unknown --type value is treated as a custom type (no matches)
    let workspace = BrWorkspace::new();
    init_workspace(&workspace);

    // Create a bug issue
    create_issue_with_description(&workspace, "Sample bug", "bug", None);

    let lint = run_br(
        &workspace,
        ["lint", "--type", "unknown_custom_type"],
        "lint_unknown_type",
    );
    // Unknown types are valid (treated as custom types), just match no issues
    assert!(
        lint.status.success(),
        "unknown type should succeed (custom types are valid), got stderr: {}",
        lint.stderr
    );
    assert!(
        lint.stdout.contains("0 issues checked"),
        "should check 0 issues for unknown type, got: {}",
        lint.stdout
    );
}

// =============================================================================
// Case Insensitivity Tests
// =============================================================================

#[test]
fn e2e_lint_case_insensitive_section_matching() {
    // Section headings should match case-insensitively
    let workspace = BrWorkspace::new();
    init_workspace(&workspace);

    // Use lowercase headings
    let description = "## steps to reproduce\n1. Steps\n\n## acceptance criteria\n- Done";
    create_issue_with_description(&workspace, "Lowercase bug", "bug", Some(description));

    let lint = run_br(&workspace, ["lint"], "lint_case_insensitive");
    assert!(lint.status.success(), "lint failed: {}", lint.stderr);
    assert!(
        lint.stdout.contains("No template warnings found"),
        "case-insensitive matching should work, got: {}",
        lint.stdout
    );
}

// =============================================================================
// Multiple Issues Tests
// =============================================================================

#[test]
fn e2e_lint_multiple_issues_with_warnings() {
    // Multiple issues with warnings should all be reported
    let workspace = BrWorkspace::new();
    init_workspace(&workspace);

    let bug1 = create_issue_with_description(&workspace, "Bug 1", "bug", Some("Missing"));
    let bug2 = create_issue_with_description(&workspace, "Bug 2", "bug", Some("Also missing"));
    let task = create_issue_with_description(&workspace, "Task 1", "task", Some("Missing too"));

    let lint = run_br(&workspace, ["lint", "--json"], "lint_multiple");
    assert!(lint.status.success(), "lint failed: {}", lint.stderr);

    let json_str = extract_json_payload(&lint.stdout);
    let json: Value = serde_json::from_str(&json_str).expect("valid JSON");

    let issues_count = json["issues"].as_i64().unwrap();
    assert!(
        issues_count >= 3,
        "expected at least 3 issues with warnings, got {issues_count}"
    );

    let results = json["results"].as_array().unwrap();
    assert!(
        results.iter().any(|r| r["id"] == bug1),
        "bug1 should be in results"
    );
    assert!(
        results.iter().any(|r| r["id"] == bug2),
        "bug2 should be in results"
    );
    assert!(
        results.iter().any(|r| r["id"] == task),
        "task should be in results"
    );
}
