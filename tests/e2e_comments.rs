//! E2E tests for the `comments` command.
//!
//! Tests cover:
//! - Adding comments to issues
//! - Listing comments on issues
//! - JSON output validation
//! - Error cases (non-existent issues, empty comments)
//! - Edge cases (special characters, long comments, closed issues)

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
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

/// Test 1: Add single comment, verify in list
#[test]
fn e2e_comments_add_single_and_list() {
    let _log = common::test_log("e2e_comments_add_single_and_list");
    let workspace = BrWorkspace::new();

    // Initialize workspace
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create an issue
    let create = run_br(&workspace, ["create", "Test issue for comments"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);
    assert!(!id.is_empty(), "missing created id");

    // Add a comment
    let add = run_br(
        &workspace,
        ["comments", "add", &id, "This is my first comment"],
        "add_comment",
    );
    assert!(add.status.success(), "add comment failed: {}", add.stderr);

    // List comments
    let list = run_br(&workspace, ["comments", "list", &id], "list_comments");
    assert!(
        list.status.success(),
        "list comments failed: {}",
        list.stderr
    );
    assert!(
        list.stdout.contains("This is my first comment"),
        "comment not found in list output"
    );
}

/// Test 2: Add multiple comments, verify order (newest last)
#[test]
fn e2e_comments_add_multiple_verify_order() {
    let _log = common::test_log("e2e_comments_add_multiple_verify_order");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Multiple comments test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // Add three comments
    let add1 = run_br(
        &workspace,
        ["comments", "add", &id, "First comment"],
        "add_comment1",
    );
    assert!(
        add1.status.success(),
        "add comment 1 failed: {}",
        add1.stderr
    );

    let add2 = run_br(
        &workspace,
        ["comments", "add", &id, "Second comment"],
        "add_comment2",
    );
    assert!(
        add2.status.success(),
        "add comment 2 failed: {}",
        add2.stderr
    );

    let add3 = run_br(
        &workspace,
        ["comments", "add", &id, "Third comment"],
        "add_comment3",
    );
    assert!(
        add3.status.success(),
        "add comment 3 failed: {}",
        add3.stderr
    );

    // List comments in JSON format to verify order
    let list = run_br(&workspace, ["comments", "list", &id, "--json"], "list_json");
    assert!(list.status.success(), "list json failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let comments: Vec<Value> = serde_json::from_str(&payload).expect("parse comments json");

    assert_eq!(comments.len(), 3, "should have 3 comments");

    // Verify comments are in order (first, second, third)
    let texts: Vec<&str> = comments.iter().filter_map(|c| c["text"].as_str()).collect();
    assert_eq!(texts[0], "First comment");
    assert_eq!(texts[1], "Second comment");
    assert_eq!(texts[2], "Third comment");
}

/// Test 3: List comments with --json, validate structure
#[test]
fn e2e_comments_list_json_structure() {
    let _log = common::test_log("e2e_comments_list_json_structure");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "JSON structure test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // Add a comment with explicit author
    let add = run_br(
        &workspace,
        [
            "comments",
            "add",
            &id,
            "--author",
            "test-user",
            "JSON structure comment",
        ],
        "add_comment",
    );
    assert!(add.status.success(), "add comment failed: {}", add.stderr);

    // List in JSON format
    let list = run_br(&workspace, ["comments", "list", &id, "--json"], "list_json");
    assert!(list.status.success(), "list json failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let comments: Vec<Value> = serde_json::from_str(&payload).expect("parse comments json");

    assert_eq!(comments.len(), 1, "should have 1 comment");
    let comment = &comments[0];

    // Validate structure
    assert!(
        comment["id"].is_number() || comment["id"].is_string(),
        "comment should have id"
    );
    assert_eq!(comment["text"], "JSON structure comment");
    assert_eq!(comment["author"], "test-user");
    assert!(
        comment["created_at"].is_string(),
        "comment should have created_at"
    );
}

/// Test 4: Add comment to issue with existing comments
#[test]
fn e2e_comments_add_to_existing() {
    let _log = common::test_log("e2e_comments_add_to_existing");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Existing comments test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // Add first comment
    let add1 = run_br(
        &workspace,
        ["comments", "add", &id, "Existing comment"],
        "add_comment1",
    );
    assert!(
        add1.status.success(),
        "add comment 1 failed: {}",
        add1.stderr
    );

    // Verify one comment
    let list1 = run_br(&workspace, ["comments", "list", &id, "--json"], "list1");
    assert!(list1.status.success(), "list1 failed: {}", list1.stderr);
    let payload1 = extract_json_payload(&list1.stdout);
    let comments1: Vec<Value> = serde_json::from_str(&payload1).expect("parse json");
    assert_eq!(comments1.len(), 1, "should have 1 comment");

    // Add another comment
    let add2 = run_br(
        &workspace,
        ["comments", "add", &id, "New comment added"],
        "add_comment2",
    );
    assert!(
        add2.status.success(),
        "add comment 2 failed: {}",
        add2.stderr
    );

    // Verify two comments
    let list2 = run_br(&workspace, ["comments", "list", &id, "--json"], "list2");
    assert!(list2.status.success(), "list2 failed: {}", list2.stderr);
    let payload2 = extract_json_payload(&list2.stdout);
    let comments2: Vec<Value> = serde_json::from_str(&payload2).expect("parse json");
    assert_eq!(comments2.len(), 2, "should have 2 comments");
}

/// Test 5: Add comment to non-existent issue → error
#[test]
fn e2e_comments_add_nonexistent_issue() {
    let _log = common::test_log("e2e_comments_add_nonexistent_issue");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Try to add comment to non-existent issue
    let add = run_br(
        &workspace,
        ["comments", "add", "bd-nonexistent", "This should fail"],
        "add_nonexistent",
    );
    assert!(
        !add.status.success(),
        "add comment to non-existent issue should fail"
    );
    assert!(
        add.stderr.contains("not found")
            || add.stderr.contains("Issue")
            || add.stderr.contains("error"),
        "error message should indicate issue not found: {}",
        add.stderr
    );
}

/// Test 6: Add empty comment → error or rejection
#[test]
fn e2e_comments_add_empty() {
    let _log = common::test_log("e2e_comments_add_empty");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Empty comment test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // Try to add empty comment (no text arguments)
    let add = run_br(&workspace, ["comments", "add", &id], "add_empty");
    // This might either fail or succeed with empty - check behavior
    // Most implementations reject empty comments
    if add.status.success() {
        // If it succeeded, verify comment list
        let list = run_br(
            &workspace,
            ["comments", "list", &id, "--json"],
            "list_empty",
        );
        let payload = extract_json_payload(&list.stdout);
        let comments: Vec<Value> = serde_json::from_str(&payload).unwrap_or_default();
        // Either no comment was added, or an empty comment exists
        assert!(
            comments.is_empty()
                || comments
                    .iter()
                    .all(|c| c["text"].as_str().is_none_or(str::is_empty)),
            "empty comment handling"
        );
    } else {
        // Expected: error for empty comment
        assert!(
            add.stderr.contains("empty")
                || add.stderr.contains("required")
                || add.stderr.contains("text"),
            "error message should indicate empty comment rejected: {}",
            add.stderr
        );
    }
}

/// Test 7: List comments on issue with no comments → empty list
#[test]
fn e2e_comments_list_empty() {
    let _log = common::test_log("e2e_comments_list_empty");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "No comments issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // List comments on issue with no comments
    let list = run_br(
        &workspace,
        ["comments", "list", &id, "--json"],
        "list_empty",
    );
    assert!(
        list.status.success(),
        "list empty comments failed: {}",
        list.stderr
    );

    let payload = extract_json_payload(&list.stdout);
    let comments: Vec<Value> = serde_json::from_str(&payload).expect("parse json");
    assert!(comments.is_empty(), "should have 0 comments");
}

/// Test 8: Comment with special characters (quotes, newlines, unicode)
#[test]
fn e2e_comments_special_characters() {
    let _log = common::test_log("e2e_comments_special_characters");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Special chars test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // Add comment with special characters using --message flag for complex text
    let special_text = "Quote: \"hello\" and apostrophe's and emoji: 🚀";
    let add = run_br(
        &workspace,
        ["comments", "add", &id, "--message", special_text],
        "add_special",
    );
    assert!(
        add.status.success(),
        "add special comment failed: {}",
        add.stderr
    );

    // Verify comment was stored correctly
    let list = run_br(
        &workspace,
        ["comments", "list", &id, "--json"],
        "list_special",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let comments: Vec<Value> = serde_json::from_str(&payload).expect("parse json");
    assert_eq!(comments.len(), 1, "should have 1 comment");

    let text = comments[0]["text"].as_str().expect("text field");
    assert!(text.contains("Quote:"), "should contain quote");
    assert!(text.contains("hello"), "should contain quoted text");
    assert!(
        text.contains("apostrophe") || text.contains('\''),
        "should contain apostrophe"
    );
}

/// Test 9: Very long comment (near limits)
#[test]
fn e2e_comments_long_text() {
    let _log = common::test_log("e2e_comments_long_text");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Long comment test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // Create a long comment (10KB)
    let long_text = "x".repeat(10_000);
    let add = run_br(
        &workspace,
        ["comments", "add", &id, "--message", &long_text],
        "add_long",
    );
    assert!(
        add.status.success(),
        "add long comment failed: {}",
        add.stderr
    );

    // Verify comment was stored
    let list = run_br(&workspace, ["comments", "list", &id, "--json"], "list_long");
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let comments: Vec<Value> = serde_json::from_str(&payload).expect("parse json");
    assert_eq!(comments.len(), 1, "should have 1 comment");

    let text = comments[0]["text"].as_str().expect("text field");
    assert_eq!(text.len(), 10_000, "comment should be 10KB");
}

/// Test 10: Comment on closed issue (should work)
#[test]
fn e2e_comments_on_closed_issue() {
    let _log = common::test_log("e2e_comments_on_closed_issue");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Closed issue test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // Close the issue
    let close = run_br(
        &workspace,
        ["close", &id, "--reason", "Testing closed comments"],
        "close_issue",
    );
    assert!(close.status.success(), "close failed: {}", close.stderr);

    // Add comment to closed issue
    let add = run_br(
        &workspace,
        ["comments", "add", &id, "Comment on closed issue"],
        "add_closed",
    );
    assert!(
        add.status.success(),
        "add comment to closed issue failed: {}",
        add.stderr
    );

    // Verify comment was added
    let list = run_br(
        &workspace,
        ["comments", "list", &id, "--json"],
        "list_closed",
    );
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let comments: Vec<Value> = serde_json::from_str(&payload).expect("parse json");
    assert_eq!(comments.len(), 1, "should have 1 comment");
    assert_eq!(comments[0]["text"], "Comment on closed issue");
}

/// Test: Comments add with --json output
#[test]
fn e2e_comments_add_json_output() {
    let _log = common::test_log("e2e_comments_add_json_output");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "JSON add test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // Add comment with --json output
    let add = run_br(
        &workspace,
        ["comments", "add", &id, "--json", "JSON output comment"],
        "add_json",
    );
    assert!(add.status.success(), "add json failed: {}", add.stderr);

    // Verify JSON output
    let payload = extract_json_payload(&add.stdout);
    let result: Value = serde_json::from_str(&payload).expect("parse add json");

    // The result should contain information about the added comment
    assert!(
        result.is_object() || result.is_array(),
        "add --json should return structured output"
    );
}

/// Test: Comments shorthand (br comments <id> = br comments list <id>)
#[test]
fn e2e_comments_shorthand() {
    let _log = common::test_log("e2e_comments_shorthand");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Shorthand test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // Add a comment
    let add = run_br(
        &workspace,
        ["comments", "add", &id, "Shorthand comment"],
        "add_comment",
    );
    assert!(add.status.success(), "add comment failed: {}", add.stderr);

    // Use shorthand to list comments
    let list = run_br(&workspace, ["comments", &id], "list_shorthand");
    assert!(
        list.status.success(),
        "list shorthand failed: {}",
        list.stderr
    );
    assert!(
        list.stdout.contains("Shorthand comment"),
        "shorthand should list comments"
    );
}

/// Test: Comments are preserved in JSONL sync
#[test]
fn e2e_comments_sync_roundtrip() {
    let _log = common::test_log("e2e_comments_sync_roundtrip");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Sync roundtrip test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let id = parse_created_id(&create.stdout);

    // Add comments
    let add1 = run_br(
        &workspace,
        ["comments", "add", &id, "First sync comment"],
        "add_comment1",
    );
    assert!(
        add1.status.success(),
        "add comment 1 failed: {}",
        add1.stderr
    );

    let add2 = run_br(
        &workspace,
        ["comments", "add", &id, "Second sync comment"],
        "add_comment2",
    );
    assert!(
        add2.status.success(),
        "add comment 2 failed: {}",
        add2.stderr
    );

    // Export to JSONL
    let flush = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(
        flush.status.success(),
        "sync flush failed: {}",
        flush.stderr
    );

    // Create a new workspace and import
    let workspace2 = BrWorkspace::new();
    let init2 = run_br(&workspace2, ["init"], "init2");
    assert!(init2.status.success(), "init2 failed: {}", init2.stderr);

    // Copy JSONL to new workspace
    let jsonl_src = workspace.root.join(".beads").join("issues.jsonl");
    let jsonl_dst = workspace2.root.join(".beads").join("issues.jsonl");
    std::fs::copy(&jsonl_src, &jsonl_dst).expect("copy jsonl");

    // Import
    let import = run_br(
        &workspace2,
        ["sync", "--import-only", "--force"],
        "sync_import",
    );
    assert!(import.status.success(), "import failed: {}", import.stderr);

    // Verify comments were imported
    let list = run_br(
        &workspace2,
        ["comments", "list", &id, "--json"],
        "list_after_import",
    );
    assert!(
        list.status.success(),
        "list after import failed: {}",
        list.stderr
    );

    let payload = extract_json_payload(&list.stdout);
    let comments: Vec<Value> = serde_json::from_str(&payload).expect("parse json");
    assert_eq!(comments.len(), 2, "should have 2 comments after import");

    let texts: Vec<&str> = comments.iter().filter_map(|c| c["text"].as_str()).collect();
    assert!(texts.contains(&"First sync comment"));
    assert!(texts.contains(&"Second sync comment"));
}
