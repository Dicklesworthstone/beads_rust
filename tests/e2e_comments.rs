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

    // Regression for #461: each `comments add` auto-flush must preserve the
    // earlier comment rows instead of repeating the newest row into every
    // pre-existing slot. Validate the published ledger directly so agreement
    // between two damaged query surfaces cannot make this test pass.
    let jsonl = std::fs::read_to_string(workspace.root.join(".beads/issues.jsonl"))
        .expect("read issues.jsonl");
    let exported_issue = jsonl
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("parse JSONL issue"))
        .find(|issue| issue["id"].as_str() == Some(id.as_str()))
        .expect("find commented issue in JSONL");
    let exported_comments = exported_issue["comments"]
        .as_array()
        .expect("comments array");
    let exported_ids: Vec<i64> = exported_comments
        .iter()
        .map(|comment| comment["id"].as_i64().expect("numeric comment id"))
        .collect();
    let exported_bodies: Vec<&str> = exported_comments
        .iter()
        .map(|comment| comment["text"].as_str().expect("comment body"))
        .collect();
    assert_eq!(exported_ids.len(), 3);
    assert!(
        exported_ids.windows(2).all(|pair| pair[0] != pair[1]),
        "comment ids must remain distinct in JSONL: {exported_ids:?}"
    );
    assert_eq!(
        exported_bodies,
        ["First comment", "Second comment", "Third comment"]
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
    assert_eq!(comment["author"], "test-user"); // invariant: hardcoded actor name, not an issue ID
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

fn comment_texts(workspace: &BrWorkspace, id: &str, label: &str) -> Vec<(String, String)> {
    let list = run_br(workspace, ["comments", "list", id, "--json"], label);
    assert!(
        list.status.success(),
        "{label}: comments list failed: {}",
        list.stderr
    );
    let payload = extract_json_payload(&list.stdout);
    let comments: Vec<Value> = serde_json::from_str(&payload).expect("parse comments json");
    comments
        .iter()
        .map(|comment| {
            (
                comment["author"].as_str().unwrap_or_default().to_string(),
                comment["text"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

fn jsonl_line_for(path: &std::path::Path, id: &str) -> String {
    let needle = format!("\"id\":\"{id}\"");
    std::fs::read_to_string(path)
        .expect("read issues.jsonl")
        .lines()
        .find(|line| line.contains(&needle))
        .unwrap_or_else(|| panic!("no JSONL line for {id}"))
        .to_string()
}

/// GitHub #486: two clones of one workspace each add one comment. Every
/// clone's AUTOINCREMENT hands out comment id 1, so the git-merged JSONL
/// carries the same positive comment id for two different issues. Importing
/// it must succeed on both clones (never refuse, never roll back), and the
/// documented "renumber one side" remedy must not break the other clone.
#[test]
fn e2e_comments_colliding_ids_from_two_clones_import_on_both_sides() {
    let _log = common::test_log("e2e_comments_colliding_ids_from_two_clones_import_on_both_sides");
    let clone_a = BrWorkspace::new();
    let clone_b = BrWorkspace::new();

    // 1. Clone A creates the shared workspace with one issue.
    let init = run_br(&clone_a, ["init", "--prefix", "demo"], "init_a");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    let create_a = run_br(
        &clone_a,
        ["create", "Issue from clone A", "--actor", "alice"],
        "create_a",
    );
    assert!(
        create_a.status.success(),
        "create failed: {}",
        create_a.stderr
    );
    let a_id = parse_created_id(&create_a.stdout);
    assert!(!a_id.is_empty(), "missing created id: {}", create_a.stdout);
    let flush_a = run_br(&clone_a, ["sync", "--flush-only"], "flush_a_initial");
    assert!(flush_a.status.success(), "flush failed: {}", flush_a.stderr);

    // 2. Clone B = git clone of A: tracked .beads files only, no database.
    let beads_a = clone_a.root.join(".beads");
    let beads_b = clone_b.root.join(".beads");
    std::fs::create_dir_all(&beads_b).expect("create clone B .beads");
    for name in ["issues.jsonl", "config.yaml", "metadata.json", ".gitignore"] {
        let src = beads_a.join(name);
        if src.exists() {
            std::fs::copy(&src, beads_b.join(name)).expect("copy tracked beads file");
        }
    }
    let warm_b = run_br(&clone_b, ["list"], "list_b_rebuild");
    assert!(
        warm_b.status.success(),
        "clone B rebuild failed: {}",
        warm_b.stderr
    );

    // 3. Each clone adds ONE comment to a different issue; both mint comment id 1.
    let comment_a = run_br(
        &clone_a,
        [
            "comments",
            "add",
            &a_id,
            "comment from clone A",
            "--actor",
            "alice",
        ],
        "comment_a",
    );
    assert!(
        comment_a.status.success(),
        "comment failed: {}",
        comment_a.stderr
    );
    let flush_a = run_br(&clone_a, ["sync", "--flush-only"], "flush_a_comment");
    assert!(flush_a.status.success(), "flush failed: {}", flush_a.stderr);

    let create_b = run_br(
        &clone_b,
        ["create", "Issue from clone B", "--actor", "bob"],
        "create_b",
    );
    assert!(
        create_b.status.success(),
        "create failed: {}",
        create_b.stderr
    );
    let b_id = parse_created_id(&create_b.stdout);
    assert!(!b_id.is_empty(), "missing created id: {}", create_b.stdout);
    let comment_b = run_br(
        &clone_b,
        [
            "comments",
            "add",
            &b_id,
            "comment from clone B",
            "--actor",
            "bob",
        ],
        "comment_b",
    );
    assert!(
        comment_b.status.success(),
        "comment failed: {}",
        comment_b.stderr
    );
    let flush_b = run_br(&clone_b, ["sync", "--flush-only"], "flush_b_comment");
    assert!(flush_b.status.success(), "flush failed: {}", flush_b.stderr);

    let jsonl_a = beads_a.join("issues.jsonl");
    let jsonl_b = beads_b.join("issues.jsonl");
    let line_a = jsonl_line_for(&jsonl_a, &a_id);
    let line_b = jsonl_line_for(&jsonl_b, &b_id);
    assert!(
        line_a.contains("\"id\":1,") && line_b.contains("\"id\":1,"),
        "both clones must have minted comment id 1: A={line_a} B={line_b}"
    );

    // 4. git merge: A keeps its own line and gains B's new line.
    let mut merged = std::fs::read_to_string(&jsonl_a).expect("read A jsonl");
    if !merged.ends_with('\n') {
        merged.push('\n');
    }
    merged.push_str(&line_b);
    merged.push('\n');
    std::fs::write(&jsonl_a, merged).expect("write merged jsonl");

    // 5. Clone A imports the merged JSONL (auto-import runs this on every command).
    let import_a = run_br(&clone_a, ["sync", "--import-only"], "import_a_merged");
    assert!(
        import_a.status.success(),
        "clone A must import colliding comment ids: {}",
        import_a.stderr
    );
    let list_a = run_br(&clone_a, ["list"], "list_a_after_merge");
    assert!(list_a.status.success(), "list failed: {}", list_a.stderr);
    let show_a = run_br(&clone_a, ["show", &a_id], "show_a_after_merge");
    assert!(show_a.status.success(), "show failed: {}", show_a.stderr);
    assert_eq!(
        comment_texts(&clone_a, &a_id, "a_comments_on_a"),
        vec![("alice".to_string(), "comment from clone A".to_string())]
    );
    assert_eq!(
        comment_texts(&clone_a, &b_id, "a_comments_on_b"),
        vec![("bob".to_string(), "comment from clone B".to_string())]
    );

    // 6. Clone A republishes (its local rowid for B's comment may differ) and
    //    clone B pulls that JSONL while its own DB still holds its comment as id 1.
    let flush_a = run_br(&clone_a, ["sync", "--flush-only"], "flush_a_after_merge");
    assert!(flush_a.status.success(), "flush failed: {}", flush_a.stderr);
    std::fs::copy(&jsonl_a, &jsonl_b).expect("pull merged jsonl into clone B");

    // 7. Clone B imports: its skipped local issue keeps comment id 1 while the
    //    peer's comment also claims id 1. This used to roll back every command.
    let import_b = run_br(&clone_b, ["sync", "--import-only"], "import_b_merged");
    assert!(
        import_b.status.success(),
        "clone B must import the peer's colliding comment id: {}",
        import_b.stderr
    );
    let list_b = run_br(&clone_b, ["list"], "list_b_after_merge");
    assert!(list_b.status.success(), "list failed: {}", list_b.stderr);
    assert_eq!(
        comment_texts(&clone_b, &a_id, "b_comments_on_a"),
        vec![("alice".to_string(), "comment from clone A".to_string())]
    );
    assert_eq!(
        comment_texts(&clone_b, &b_id, "b_comments_on_b"),
        vec![("bob".to_string(), "comment from clone B".to_string())]
    );

    // Both clones converge: a further round trip is a no-op on each side.
    let flush_b = run_br(&clone_b, ["sync", "--flush-only"], "flush_b_after_merge");
    assert!(flush_b.status.success(), "flush failed: {}", flush_b.stderr);
    std::fs::copy(&jsonl_b, &jsonl_a).expect("pull clone B jsonl into clone A");
    let import_a = run_br(&clone_a, ["sync", "--import-only"], "import_a_roundtrip");
    assert!(
        import_a.status.success(),
        "round-trip import must succeed: {}",
        import_a.stderr
    );
    assert_eq!(
        comment_texts(&clone_a, &b_id, "a_comments_on_b_roundtrip"),
        vec![("bob".to_string(), "comment from clone B".to_string())]
    );
}
