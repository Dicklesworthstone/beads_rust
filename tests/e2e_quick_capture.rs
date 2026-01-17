//! E2E tests for the `q` (quick capture) command.
//!
//! The `q` command is a shorthand for rapid issue creation that returns only the issue ID.
//! This enables scriptable workflows and fast capture.
//!
//! Test categories:
//! - Success paths: Basic functionality, flags, labels
//! - Error cases: Validation failures, uninitialized workspace
//! - Scripting integration: Pipeline usage, unique IDs, stderr behavior

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;
use std::collections::HashSet;

// =============================================================================
// Success Path Tests (8 tests)
// =============================================================================

#[test]
fn q_creates_issue_returns_id_only() {
    // The q command should output only the issue ID (no "Created" prefix or other text)
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let quick = run_br(&workspace, ["q", "Test quick capture"], "quick");
    assert!(quick.status.success(), "q failed: {}", quick.stderr);

    let output = quick.stdout.trim();
    // Should be just the ID, no other output
    assert_eq!(output.lines().count(), 1, "q should output only one line");
    assert!(
        output.starts_with("bd-") || output.contains('-'),
        "output should be an ID format, got: {output}"
    );
    // Should not contain "Created" or other verbose text
    assert!(
        !output.to_lowercase().contains("created"),
        "q should not include 'Created' text"
    );
}

#[test]
fn q_with_type_flag() {
    // --type bug should set the issue type correctly
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let quick = run_br(
        &workspace,
        ["q", "Bug report", "--type", "bug"],
        "quick_type",
    );
    assert!(
        quick.status.success(),
        "q with --type failed: {}",
        quick.stderr
    );

    let id = quick.stdout.trim();

    // Verify the issue was created with the correct type
    let show = run_br(&workspace, ["show", id, "--json"], "show");
    assert!(show.status.success(), "show failed: {}", show.stderr);

    let payload = extract_json_payload(&show.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");
    assert_eq!(json[0]["issue_type"], "bug", "issue type should be 'bug'");
}

#[test]
fn q_with_priority_flag() {
    // --priority 1 should set the priority correctly
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let quick = run_br(
        &workspace,
        ["q", "High priority issue", "-p", "1"],
        "quick_priority",
    );
    assert!(quick.status.success(), "q with -p failed: {}", quick.stderr);

    let id = quick.stdout.trim();

    // Verify the issue was created with the correct priority
    let show = run_br(&workspace, ["show", id, "--json"], "show");
    assert!(show.status.success(), "show failed: {}", show.stderr);

    let payload = extract_json_payload(&show.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");
    assert_eq!(json[0]["priority"], 1, "priority should be 1");
}

#[test]
fn q_with_all_flags() {
    // Combine type + priority + labels
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let quick = run_br(
        &workspace,
        [
            "q",
            "Critical bug",
            "-t",
            "bug",
            "-p",
            "0",
            "-l",
            "urgent",
            "-l",
            "regression",
        ],
        "quick_all",
    );
    assert!(
        quick.status.success(),
        "q with all flags failed: {}",
        quick.stderr
    );

    let id = quick.stdout.trim();

    // Verify all fields
    let show = run_br(&workspace, ["show", id, "--json"], "show");
    assert!(show.status.success(), "show failed: {}", show.stderr);

    let payload = extract_json_payload(&show.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");
    assert_eq!(json[0]["issue_type"], "bug");
    assert_eq!(json[0]["priority"], 0);

    let labels = json[0]["labels"]
        .as_array()
        .expect("labels should be array");
    let label_names: Vec<&str> = labels.iter().filter_map(|l| l.as_str()).collect();
    assert!(
        label_names.contains(&"urgent"),
        "should have 'urgent' label"
    );
    assert!(
        label_names.contains(&"regression"),
        "should have 'regression' label"
    );
}

#[test]
fn q_with_labels() {
    // Test label functionality including comma-separated values
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Test comma-separated labels
    let quick = run_br(
        &workspace,
        ["q", "Labeled issue", "-l", "frontend,backend,api"],
        "quick_labels",
    );
    assert!(
        quick.status.success(),
        "q with labels failed: {}",
        quick.stderr
    );

    let id = quick.stdout.trim();

    let show = run_br(&workspace, ["show", id, "--json"], "show");
    assert!(show.status.success(), "show failed: {}", show.stderr);

    let payload = extract_json_payload(&show.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    let labels = json[0]["labels"]
        .as_array()
        .expect("labels should be array");
    assert_eq!(labels.len(), 3, "should have 3 labels");

    let label_names: Vec<&str> = labels.iter().filter_map(|l| l.as_str()).collect();
    assert!(label_names.contains(&"frontend"));
    assert!(label_names.contains(&"backend"));
    assert!(label_names.contains(&"api"));
}

#[test]
fn q_multiple_words_title() {
    // Multiple words without quotes should be joined
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let quick = run_br(
        &workspace,
        ["q", "This", "is", "a", "multi", "word", "title"],
        "quick_multiword",
    );
    assert!(
        quick.status.success(),
        "q with multiple words failed: {}",
        quick.stderr
    );

    let id = quick.stdout.trim();

    let show = run_br(&workspace, ["show", id, "--json"], "show");
    assert!(show.status.success(), "show failed: {}", show.stderr);

    let payload = extract_json_payload(&show.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    // Words should be joined with spaces
    assert_eq!(
        json[0]["title"], "This is a multi word title",
        "title should be joined words"
    );
}

#[test]
fn q_output_is_valid_id() {
    // Verify the output matches the expected ID format
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let quick = run_br(&workspace, ["q", "ID format test"], "quick_id");
    assert!(quick.status.success(), "q failed: {}", quick.stderr);

    let id = quick.stdout.trim();

    // ID should match bd-XXXX format (prefix-hash)
    assert!(id.contains('-'), "ID should contain hyphen separator");
    let parts: Vec<&str> = id.split('-').collect();
    assert!(parts.len() >= 2, "ID should have prefix and hash parts");

    // The hash part should be alphanumeric (base36)
    let hash_part = parts[1..].join("-"); // In case of multiple hyphens
    assert!(
        hash_part.chars().all(|c| c.is_ascii_alphanumeric()),
        "hash part should be alphanumeric: {hash_part}"
    );
}

#[test]
fn q_issue_appears_in_list() {
    // Created issue should be visible in list output
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let quick = run_br(&workspace, ["q", "Listable issue"], "quick_list");
    assert!(quick.status.success(), "q failed: {}", quick.stderr);

    let id = quick.stdout.trim();

    let list = run_br(&workspace, ["list", "--json"], "list");
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    let found = json.iter().any(|issue| issue["id"] == id);
    assert!(found, "issue {id} should appear in list output");
}

// =============================================================================
// Error Cases (4 tests)
// =============================================================================

#[test]
fn q_without_init_fails() {
    // q should fail if workspace is not initialized
    let workspace = BrWorkspace::new();

    // Do NOT run init

    let quick = run_br(&workspace, ["q", "No init"], "quick_no_init");
    assert!(
        !quick.status.success(),
        "q should fail without init, but succeeded"
    );

    // Should have error message about not being initialized
    assert!(
        quick.stderr.contains("not initialized")
            || quick.stderr.contains("Not initialized")
            || quick.stderr.contains(".beads")
            || quick.stderr.contains("init"),
        "error should mention initialization: {}",
        quick.stderr
    );
}

#[test]
fn q_empty_title_fails() {
    // Empty title should be rejected
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Try with empty string
    let quick = run_br(&workspace, ["q", ""], "quick_empty");
    assert!(
        !quick.status.success(),
        "q should fail with empty title, but succeeded"
    );

    assert!(
        quick.stderr.to_lowercase().contains("empty")
            || quick.stderr.to_lowercase().contains("title")
            || quick.stderr.to_lowercase().contains("cannot be empty"),
        "error should mention empty title: {}",
        quick.stderr
    );
}

#[test]
fn q_with_custom_type_succeeds() {
    // Custom issue types should be accepted (IssueType::Custom variant)
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let quick = run_br(
        &workspace,
        ["q", "Custom type issue", "--type", "my_custom_type"],
        "quick_custom_type",
    );
    assert!(
        quick.status.success(),
        "q with custom type should succeed: {}",
        quick.stderr
    );

    let id = quick.stdout.trim();

    // Verify the custom type was stored correctly
    let show = run_br(&workspace, ["show", id, "--json"], "show");
    assert!(show.status.success(), "show failed: {}", show.stderr);

    let payload = extract_json_payload(&show.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    // The type should be preserved as the custom value
    assert_eq!(
        json[0]["issue_type"], "my_custom_type",
        "custom issue type should be preserved"
    );
}

#[test]
fn q_invalid_priority_fails() {
    // Out of range priority should be rejected
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Priority should be 0-4
    let quick = run_br(
        &workspace,
        ["q", "Bad priority", "-p", "99"],
        "quick_bad_priority",
    );
    assert!(
        !quick.status.success(),
        "q should fail with priority 99, but succeeded"
    );

    assert!(
        quick.stderr.to_lowercase().contains("priority")
            || quick.stderr.to_lowercase().contains("invalid")
            || quick.stderr.contains("99"),
        "error should mention invalid priority: {}",
        quick.stderr
    );
}

// =============================================================================
// Scripting Integration Tests (3 tests)
// =============================================================================

#[test]
fn q_output_usable_in_pipeline() {
    // ID can be piped to other commands (e.g., show)
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let quick = run_br(&workspace, ["q", "Pipeline test"], "quick_pipeline");
    assert!(quick.status.success(), "q failed: {}", quick.stderr);

    let id = quick.stdout.trim();

    // The ID should work directly with show command
    let show = run_br(&workspace, ["show", id], "show_pipeline");
    assert!(
        show.status.success(),
        "show should succeed with q output: {}",
        show.stderr
    );
    assert!(
        show.stdout.contains("Pipeline test"),
        "show should display the issue title"
    );

    // Also verify it works with update
    let update = run_br(
        &workspace,
        ["update", id, "--status", "in_progress"],
        "update_pipeline",
    );
    assert!(
        update.status.success(),
        "update should succeed with q output: {}",
        update.stderr
    );
}

#[test]
fn q_multiple_creates_unique_ids() {
    // Rapid creates should get unique IDs
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let mut ids: HashSet<String> = HashSet::new();

    // Create 10 issues rapidly
    for i in 0..10 {
        let quick = run_br(
            &workspace,
            ["q", &format!("Rapid issue {i}")],
            &format!("quick_{i}"),
        );
        assert!(quick.status.success(), "q #{i} failed: {}", quick.stderr);

        let id = quick.stdout.trim().to_string();
        assert!(!id.is_empty(), "ID #{i} should not be empty");

        let is_new = ids.insert(id.clone());
        assert!(is_new, "ID {id} should be unique, but was duplicate");
    }

    assert_eq!(ids.len(), 10, "should have 10 unique IDs");

    // Verify all issues exist
    let list = run_br(&workspace, ["list", "--json"], "list_all");
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");
    assert_eq!(json.len(), 10, "should have 10 issues");
}

#[test]
fn q_silent_mode_stderr() {
    // No stderr output on success
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let quick = run_br(&workspace, ["q", "Silent test"], "quick_silent");
    assert!(quick.status.success(), "q failed: {}", quick.stderr);

    // stderr should be empty on success (excluding debug logs if RUST_LOG is set)
    // Filter out tracing/debug output
    let stderr_lines: Vec<&str> = quick
        .stderr
        .lines()
        .filter(|line| {
            !line.contains("DEBUG")
                && !line.contains("INFO")
                && !line.contains("WARN")
                && !line.contains("TRACE")
                && !line.trim().is_empty()
        })
        .collect();

    assert!(
        stderr_lines.is_empty(),
        "stderr should be empty on success (excluding logs), got: {stderr_lines:?}"
    );
}

// =============================================================================
// Additional Edge Cases
// =============================================================================

#[test]
fn q_with_p_prefix_priority() {
    // P0, P1, P2, P3, P4 format should work
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let quick = run_br(
        &workspace,
        ["q", "P-format priority", "-p", "P0"],
        "quick_p_format",
    );
    assert!(
        quick.status.success(),
        "q with P0 format failed: {}",
        quick.stderr
    );

    let id = quick.stdout.trim();

    let show = run_br(&workspace, ["show", id, "--json"], "show");
    assert!(show.status.success(), "show failed: {}", show.stderr);

    let payload = extract_json_payload(&show.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");
    assert_eq!(json[0]["priority"], 0, "P0 should map to priority 0");
}

#[test]
fn q_special_characters_in_title() {
    // Special characters should be preserved in title
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let title = "Fix bug: can't parse \"quotes\" & <special> chars!";
    let quick = run_br(&workspace, ["q", title], "quick_special");
    assert!(
        quick.status.success(),
        "q with special chars failed: {}",
        quick.stderr
    );

    let id = quick.stdout.trim();

    let show = run_br(&workspace, ["show", id, "--json"], "show");
    assert!(show.status.success(), "show failed: {}", show.stderr);

    let payload = extract_json_payload(&show.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");
    assert_eq!(
        json[0]["title"], title,
        "special characters should be preserved"
    );
}

#[test]
fn q_default_values() {
    // Without flags, should use defaults (task type, medium priority)
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let quick = run_br(&workspace, ["q", "Default values test"], "quick_defaults");
    assert!(quick.status.success(), "q failed: {}", quick.stderr);

    let id = quick.stdout.trim();

    let show = run_br(&workspace, ["show", id, "--json"], "show");
    assert!(show.status.success(), "show failed: {}", show.stderr);

    let payload = extract_json_payload(&show.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");

    // Default type is task
    assert_eq!(json[0]["issue_type"], "task", "default type should be task");
    // Default priority is 2 (medium)
    assert_eq!(json[0]["priority"], 2, "default priority should be 2");
    // Status should be open
    assert_eq!(json[0]["status"], "open", "status should be open");
}

#[test]
fn q_status_is_always_open() {
    // q command always creates with status=open (no status flag)
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create multiple issues with different types/priorities
    let ids: Vec<String> = vec![
        run_br(&workspace, ["q", "Issue 1", "-t", "bug"], "q1"),
        run_br(&workspace, ["q", "Issue 2", "-p", "0"], "q2"),
        run_br(
            &workspace,
            ["q", "Issue 3", "-t", "feature", "-p", "1"],
            "q3",
        ),
    ]
    .into_iter()
    .filter(|r| r.status.success())
    .map(|r| r.stdout.trim().to_string())
    .collect();

    assert_eq!(ids.len(), 3, "all creates should succeed");

    // All should have status=open
    for id in ids {
        let show = run_br(&workspace, ["show", &id, "--json"], &format!("show_{id}"));
        assert!(show.status.success(), "show failed: {}", show.stderr);

        let payload = extract_json_payload(&show.stdout);
        let json: Vec<Value> = serde_json::from_str(&payload).expect("parse json");
        assert_eq!(
            json[0]["status"], "open",
            "issue {id} status should be open"
        );
    }
}
