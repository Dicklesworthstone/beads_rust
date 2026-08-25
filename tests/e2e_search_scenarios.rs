//! E2E tests for the `search` command scenarios.
//!
//! Comprehensive testing of search command:
//! - Basic text search
//! - Case sensitivity
//! - Regex patterns
//! - Search with filters (status, type, priority, assignee, label)
//! - Search in different fields (title, description)
//! - Output formats (text, JSON)
//! - Edge cases (empty results, special characters)

mod common;

use common::cli::{BrWorkspace, extract_issues_array, extract_json_payload, run_br};
use serde_json::Value;

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

/// Setup workspace with issues containing varied searchable content.
#[allow(clippy::too_many_lines)]
fn setup_search_workspace() -> (BrWorkspace, Vec<String>) {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let mut ids = Vec::new();

    // Issue 1: Authentication related bug
    let issue1 = run_br(
        &workspace,
        [
            "create",
            "Authentication bug in login flow",
            "-t",
            "bug",
            "-d",
            "Users cannot log in when using OAuth providers",
        ],
        "create_auth_bug",
    );
    assert!(issue1.status.success());
    let id1 = parse_created_id(&issue1.stdout);
    run_br(
        &workspace,
        ["update", &id1, "--add-label", "auth"],
        "label_auth",
    );
    ids.push(id1);

    // Issue 2: Authentication feature
    let issue2 = run_br(
        &workspace,
        [
            "create",
            "Add two-factor authentication",
            "-t",
            "feature",
            "-d",
            "Implement 2FA using TOTP for improved security",
        ],
        "create_auth_feature",
    );
    assert!(issue2.status.success());
    let id2 = parse_created_id(&issue2.stdout);
    run_br(
        &workspace,
        ["update", &id2, "--add-label", "auth"],
        "label_auth2",
    );
    ids.push(id2);

    // Issue 3: Database related task
    let issue3 = run_br(
        &workspace,
        [
            "create",
            "Optimize database queries",
            "-t",
            "task",
            "-d",
            "Add indexes to improve query performance on user table",
        ],
        "create_db_task",
    );
    assert!(issue3.status.success());
    ids.push(parse_created_id(&issue3.stdout));

    // Issue 4: UI/Frontend feature
    let issue4 = run_br(
        &workspace,
        [
            "create",
            "Dashboard redesign",
            "-t",
            "feature",
            "-d",
            "Complete overhaul of the user dashboard with new layout",
        ],
        "create_ui_feature",
    );
    assert!(issue4.status.success());
    ids.push(parse_created_id(&issue4.stdout));

    // Issue 5: API bug
    let issue5 = run_br(
        &workspace,
        [
            "create",
            "API returns 500 error",
            "-t",
            "bug",
            "-p",
            "0",
            "-d",
            "The /api/users endpoint throws Internal Server Error",
        ],
        "create_api_bug",
    );
    assert!(issue5.status.success());
    let id5 = parse_created_id(&issue5.stdout);
    run_br(
        &workspace,
        ["update", &id5, "--add-label", "api"],
        "label_api",
    );
    ids.push(id5);

    // Issue 6: Closed issue
    let issue6 = run_br(
        &workspace,
        [
            "create",
            "Fixed login timeout bug",
            "-t",
            "bug",
            "-d",
            "Session was expiring too quickly causing login failures",
        ],
        "create_closed_bug",
    );
    assert!(issue6.status.success());
    let id6 = parse_created_id(&issue6.stdout);
    run_br(&workspace, ["close", &id6], "close_issue");
    ids.push(id6);

    // Issue 7: Issue with numbers in title
    let issue7 = run_br(
        &workspace,
        [
            "create",
            "Upgrade to version 2.0",
            "-t",
            "task",
            "-d",
            "Update framework from v1.5 to v2.0",
        ],
        "create_version_task",
    );
    assert!(issue7.status.success());
    ids.push(parse_created_id(&issue7.stdout));

    (workspace, ids)
}

// =============================================================================
// BASIC SEARCH TESTS
// =============================================================================

#[test]
fn search_basic_single_word() {
    let (workspace, _ids) = setup_search_workspace();

    let search = run_br(
        &workspace,
        ["search", "authentication", "--json"],
        "search_auth",
    );
    assert!(search.status.success(), "search failed: {}", search.stderr);

    let payload = extract_json_payload(&search.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    // Should find issues with "authentication" in title or description
    assert!(
        json.len() >= 2,
        "Expected at least 2 authentication-related issues"
    );

    for issue in &json {
        let title = issue["title"].as_str().unwrap_or("").to_lowercase();
        let desc = issue["description"].as_str().unwrap_or("").to_lowercase();
        assert!(
            title.contains("authentication") || title.contains("auth") || desc.contains("auth"),
            "Result should contain 'auth' in title or description: {issue:?}"
        );
    }
}

#[test]
fn search_case_insensitive() {
    let (workspace, _ids) = setup_search_workspace();

    // Search with different case
    let search_upper = run_br(&workspace, ["search", "DATABASE", "--json"], "search_upper");
    assert!(search_upper.status.success());

    let search_lower = run_br(&workspace, ["search", "database", "--json"], "search_lower");
    assert!(search_lower.status.success());

    let upper_payload = extract_json_payload(&search_upper.stdout);
    let lower_payload = extract_json_payload(&search_lower.stdout);

    let upper_json: Vec<Value> = serde_json::from_str(&upper_payload).expect("parse upper");
    let lower_json: Vec<Value> = serde_json::from_str(&lower_payload).expect("parse lower");

    // Both should find the same results (case-insensitive)
    assert_eq!(
        upper_json.len(),
        lower_json.len(),
        "Case-insensitive search should return same results"
    );
}

#[test]
fn search_multiple_words() {
    let (workspace, _ids) = setup_search_workspace();

    // Search for "Authentication" which appears in multiple issues
    let search = run_br(
        &workspace,
        ["search", "Authentication", "--json"],
        "search_multi",
    );
    assert!(search.status.success(), "search failed: {}", search.stderr);

    let payload = extract_json_payload(&search.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    // Should find issues containing "Authentication"
    assert!(
        json.len() >= 2,
        "Should find at least 2 issues with 'Authentication'"
    );
}

#[test]
fn search_partial_word() {
    let (workspace, _ids) = setup_search_workspace();

    // Search for partial word "auth" should match "authentication"
    let search = run_br(&workspace, ["search", "auth", "--json"], "search_partial");
    assert!(search.status.success(), "search failed: {}", search.stderr);

    let payload = extract_json_payload(&search.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    assert!(json.len() >= 2, "Partial word search should find matches");
}

// =============================================================================
// SEARCH WITH FILTERS
// =============================================================================

#[test]
fn search_with_status_filter() {
    let (workspace, _ids) = setup_search_workspace();

    // Search only open issues
    let search = run_br(
        &workspace,
        ["search", "bug", "--status", "open", "--json"],
        "search_bug_open",
    );
    assert!(search.status.success(), "search failed: {}", search.stderr);

    // "bug" also matches the closed issue, so the payload carries the
    // hidden-closed wrapper (#445).
    let json = extract_issues_array(&search.stdout);

    for issue in &json {
        assert_eq!(
            issue["status"], "open",
            "All results should have status 'open'"
        );
    }
}

#[test]
fn search_with_type_filter() {
    let (workspace, _ids) = setup_search_workspace();

    let search = run_br(
        &workspace,
        ["search", "authentication", "-t", "feature", "--json"],
        "search_auth_feature",
    );
    assert!(search.status.success(), "search failed: {}", search.stderr);

    let payload = extract_json_payload(&search.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    assert_eq!(
        json.len(),
        1,
        "Should find exactly 1 authentication feature"
    );
    assert_eq!(json[0]["issue_type"], "feature");
    assert!(json[0]["title"].as_str().unwrap().contains("two-factor"));
}

#[test]
fn search_with_priority_filter() {
    let (workspace, _ids) = setup_search_workspace();

    let search = run_br(
        &workspace,
        ["search", "API", "-p", "0", "--json"],
        "search_api_p0",
    );
    assert!(search.status.success(), "search failed: {}", search.stderr);

    let payload = extract_json_payload(&search.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    assert_eq!(json.len(), 1, "Should find exactly 1 P0 API issue");
    assert_eq!(json[0]["priority"], 0);
}

#[test]
fn search_with_label_filter() {
    let (workspace, _ids) = setup_search_workspace();

    let search = run_br(
        &workspace,
        ["search", "bug", "--label", "auth", "--json"],
        "search_bug_auth",
    );
    assert!(search.status.success(), "search failed: {}", search.stderr);

    let payload = extract_json_payload(&search.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    assert_eq!(json.len(), 1, "Should find 1 bug with auth label");
    assert!(
        json[0]["title"]
            .as_str()
            .unwrap()
            .contains("Authentication bug")
    );
}

#[test]
fn search_include_closed() {
    let (workspace, _ids) = setup_search_workspace();

    // Without --include-closed, shouldn't find closed issues
    let search_no_closed = run_br(
        &workspace,
        ["search", "login", "--json"],
        "search_no_closed",
    );
    assert!(search_no_closed.status.success());

    // "login" also matches the closed issue, so this payload carries the
    // hidden-closed wrapper (#445).
    let json_no_closed = extract_issues_array(&search_no_closed.stdout);

    // With --all to include closed issues
    let search_with_closed = run_br(
        &workspace,
        ["search", "login", "--all", "--json"],
        "search_with_closed",
    );
    assert!(
        search_with_closed.status.success(),
        "search --all failed: {}",
        search_with_closed.stderr
    );

    let json_with_closed = extract_issues_array(&search_with_closed.stdout);

    // Should find more results with --include-closed
    assert!(
        json_with_closed.len() >= json_no_closed.len(),
        "Including closed should find at least as many results"
    );
}

// =============================================================================
// HIDDEN CLOSED MATCH REPORTING (#445)
// =============================================================================

#[test]
fn search_text_notes_hidden_closed_matches() {
    let (workspace, _ids) = setup_search_workspace();

    // "login" matches two open issues plus the closed "Fixed login timeout bug".
    let search = run_br(&workspace, ["search", "login"], "search_hidden_text");
    assert!(search.status.success(), "search failed: {}", search.stderr);

    assert!(
        search.stdout.contains("1 closed match(es) hidden") && search.stdout.contains("--all"),
        "text output should note the hidden closed match: {}",
        search.stdout
    );
}

#[test]
fn search_text_notes_hidden_closed_matches_with_empty_results() {
    let (workspace, _ids) = setup_search_workspace();

    // "timeout" appears only in the closed issue, so the visible result set
    // is empty — exactly when the narrowed corpus is most misleading.
    let search = run_br(&workspace, ["search", "timeout"], "search_hidden_empty");
    assert!(search.status.success(), "search failed: {}", search.stderr);

    assert!(
        search.stdout.contains("1 closed match(es) hidden") && search.stdout.contains("--all"),
        "empty text output should still note the hidden closed match: {}",
        search.stdout
    );
}

#[test]
fn search_json_reports_hidden_closed_count() {
    let (workspace, _ids) = setup_search_workspace();

    let search = run_br(
        &workspace,
        ["search", "login", "--json"],
        "search_hidden_json",
    );
    assert!(search.status.success(), "search failed: {}", search.stderr);

    let payload = extract_json_payload(&search.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse json");

    assert_eq!(
        json["hidden_closed_count"], 1,
        "wrapper should report one hidden closed match: {json}"
    );
    assert!(
        json["issues"].is_array(),
        "wrapper should carry the result rows under 'issues': {json}"
    );
}

#[test]
fn search_toon_reports_hidden_closed_count() {
    let (workspace, _ids) = setup_search_workspace();

    let search = run_br(
        &workspace,
        ["search", "login", "--format", "toon"],
        "search_hidden_toon",
    );
    assert!(search.status.success(), "search failed: {}", search.stderr);

    assert!(
        search.stdout.contains("hidden_closed_count"),
        "TOON output should follow the JSON wrapper shape: {}",
        search.stdout
    );
}

#[test]
fn search_hidden_closed_count_absent_without_closed_matches() {
    let (workspace, _ids) = setup_search_workspace();

    // No closed issue mentions "database": legacy shapes must be preserved.
    let json_search = run_br(
        &workspace,
        ["search", "database", "--json"],
        "search_no_hidden_json",
    );
    assert!(json_search.status.success());
    let payload = extract_json_payload(&json_search.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse json");
    assert!(
        json.is_array(),
        "JSON should stay a bare array when nothing was hidden: {json}"
    );

    let text_search = run_br(&workspace, ["search", "database"], "search_no_hidden_text");
    assert!(text_search.status.success());
    assert!(
        !text_search.stdout.contains("hidden"),
        "text output should not mention hidden matches: {}",
        text_search.stdout
    );
}

#[test]
fn search_hidden_closed_count_absent_with_all_flag() {
    let (workspace, _ids) = setup_search_workspace();

    let search = run_br(
        &workspace,
        ["search", "login", "--all", "--json"],
        "search_all_no_hidden",
    );
    assert!(search.status.success(), "search failed: {}", search.stderr);

    let payload = extract_json_payload(&search.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse json");
    assert!(
        json.is_array(),
        "--all includes closed issues, so the legacy array shape stays: {json}"
    );
}

#[test]
fn search_hidden_closed_count_absent_with_status_closed() {
    let (workspace, _ids) = setup_search_workspace();

    let search = run_br(
        &workspace,
        ["search", "login", "--status", "closed", "--json"],
        "search_status_closed_no_hidden",
    );
    assert!(search.status.success(), "search failed: {}", search.stderr);

    let payload = extract_json_payload(&search.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse json");
    assert!(
        json.is_array(),
        "--status closed already includes closed issues, so the legacy array shape stays: {json}"
    );
}

// =============================================================================
// SEARCH OUTPUT FORMATS
// =============================================================================

#[test]
fn search_text_output() {
    let (workspace, _ids) = setup_search_workspace();

    let search = run_br(&workspace, ["search", "authentication"], "search_text");
    assert!(search.status.success(), "search failed: {}", search.stderr);

    // Text output should contain issue information
    assert!(
        search.stdout.contains("Authentication bug in login flow")
            && search.stdout.contains("Add two-factor authentication"),
        "Text output should contain search results"
    );
    assert!(
        search.stdout.lines().count() >= 3,
        "expected header plus one line per result, got: {}",
        search.stdout
    );
}

#[test]
fn search_json_output_structure() {
    let (workspace, _ids) = setup_search_workspace();

    let search = run_br(&workspace, ["search", "database", "--json"], "search_json");
    assert!(search.status.success(), "search failed: {}", search.stderr);

    let payload = extract_json_payload(&search.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    if !json.is_empty() {
        let first = &json[0];
        assert!(first.get("id").is_some(), "Missing 'id' field");
        assert!(first.get("title").is_some(), "Missing 'title' field");
        assert!(first.get("status").is_some(), "Missing 'status' field");
    }
}

// =============================================================================
// EDGE CASES
// =============================================================================

#[test]
fn search_no_results() {
    let (workspace, _ids) = setup_search_workspace();

    let search = run_br(
        &workspace,
        ["search", "xyznonexistentterm123", "--json"],
        "search_no_results",
    );
    assert!(
        search.status.success(),
        "search should succeed with no results"
    );

    let payload = extract_json_payload(&search.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    assert_eq!(json.len(), 0, "Should find no results");
}

#[test]
fn search_empty_query() {
    let (workspace, _ids) = setup_search_workspace();

    // Empty query might be rejected or return all issues
    let search = run_br(&workspace, ["search", "", "--json"], "search_empty");

    // Either succeeds with all results or fails with error
    if search.status.success() {
        let payload = extract_json_payload(&search.stdout);
        let _json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");
    }
    // If it fails, that's also acceptable behavior
}

#[test]
fn search_special_characters() {
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init_special");
    assert!(init.status.success());

    // Create issue with special characters
    let create = run_br(
        &workspace,
        [
            "create",
            "Fix C++ compiler warnings",
            "-d",
            "Address -Wall -Werror flags",
        ],
        "create_cpp",
    );
    assert!(create.status.success());

    // Search for special characters
    let search = run_br(&workspace, ["search", "C++", "--json"], "search_cpp");
    assert!(search.status.success(), "search failed: {}", search.stderr);

    let payload = extract_json_payload(&search.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    assert_eq!(json.len(), 1, "Should find the C++ issue");
}

#[test]
fn search_with_numbers() {
    let (workspace, _ids) = setup_search_workspace();

    let search = run_br(&workspace, ["search", "2.0", "--json"], "search_version");
    assert!(search.status.success(), "search failed: {}", search.stderr);

    let payload = extract_json_payload(&search.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    assert!(!json.is_empty(), "Should find version 2.0 issue");
    assert!(json[0]["title"].as_str().unwrap().contains("2.0"));
}

#[test]
fn search_before_init_fails() {
    let workspace = BrWorkspace::new();

    let search = run_br(&workspace, ["search", "test"], "search_no_init");
    assert!(!search.status.success(), "search should fail before init");
}

// =============================================================================
// SEARCH IN DESCRIPTION
// =============================================================================

#[test]
fn search_finds_content_in_description() {
    let (workspace, _ids) = setup_search_workspace();

    // Search for term only in descriptions
    let search = run_br(&workspace, ["search", "TOTP", "--json"], "search_desc");
    assert!(search.status.success(), "search failed: {}", search.stderr);

    let payload = extract_json_payload(&search.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    assert_eq!(json.len(), 1, "Should find issue with TOTP in description");
    assert!(json[0]["title"].as_str().unwrap().contains("two-factor"));
}

#[test]
fn search_finds_content_in_title_only() {
    let (workspace, _ids) = setup_search_workspace();

    // "Dashboard" appears only in title
    let search = run_br(
        &workspace,
        ["search", "Dashboard", "--json"],
        "search_title",
    );
    assert!(search.status.success(), "search failed: {}", search.stderr);

    let payload = extract_json_payload(&search.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    assert_eq!(json.len(), 1, "Should find issue with Dashboard in title");
    assert!(json[0]["title"].as_str().unwrap().contains("Dashboard"));
}

// =============================================================================
// COMBINED SEARCH AND FILTER TESTS
// =============================================================================

#[test]
fn search_combined_multiple_filters() {
    let (workspace, _ids) = setup_search_workspace();

    let search = run_br(
        &workspace,
        ["search", "bug", "--status", "open", "-t", "bug", "--json"],
        "search_combined",
    );
    assert!(search.status.success(), "search failed: {}", search.stderr);

    // "bug" also matches the closed bug, so the payload carries the
    // hidden-closed wrapper (#445).
    let json = extract_issues_array(&search.stdout);

    for issue in &json {
        assert_eq!(issue["status"], "open");
        assert_eq!(issue["issue_type"], "bug");
    }
}
