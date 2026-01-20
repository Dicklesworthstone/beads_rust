//! E2E tests for the `audit` command.
//!
//! Tests cover:
//! - audit record: Record audit entries to interactions.jsonl
//! - audit label: Label existing audit entries
//! - Error handling: Before init, missing required fields
//! - Edge cases: Long text, special characters, stdin input

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;
use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};
use tracing::info;

/// Read and parse the interactions.jsonl file.
fn read_interactions(workspace: &BrWorkspace) -> Vec<Value> {
    let path = workspace.root.join(".beads").join("interactions.jsonl");
    if !path.exists() {
        return vec![];
    }
    let contents = fs::read_to_string(&path).expect("read interactions.jsonl");
    contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("parse interaction entry"))
        .collect()
}

// =============================================================================
// SUCCESS PATH TESTS
// =============================================================================

#[test]
fn e2e_audit_record_single_event() {
    common::init_test_logging();
    info!("e2e_audit_record_single_event: start");
    let workspace = BrWorkspace::new();

    // Initialize workspace
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Record a single audit event
    let record = run_br(
        &workspace,
        ["audit", "record", "--kind", "llm_call"],
        "record_single",
    );
    assert!(
        record.status.success(),
        "audit record failed: {}",
        record.stderr
    );

    // Verify ID was returned
    let id = record.stdout.trim();
    assert!(id.starts_with("int-"), "ID should start with int-: {id}");

    // Verify entry was written to interactions.jsonl
    let entries = read_interactions(&workspace);
    assert_eq!(entries.len(), 1, "should have exactly one entry");
    assert_eq!(entries[0]["id"], id);
    assert_eq!(entries[0]["kind"], "llm_call");
    info!("e2e_audit_record_single_event: done");
}

#[test]
fn e2e_audit_record_multiple_events_preserve_order() {
    common::init_test_logging();
    info!("e2e_audit_record_multiple_events_preserve_order: start");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Record multiple events in sequence
    let record_a = run_br(
        &workspace,
        ["audit", "record", "--kind", "llm_call"],
        "record_a",
    );
    assert!(record_a.status.success(), "record A failed");
    let id_a = record_a.stdout.trim().to_string();

    let record_b = run_br(
        &workspace,
        ["audit", "record", "--kind", "tool_call"],
        "record_b",
    );
    assert!(record_b.status.success(), "record B failed");
    let id_b = record_b.stdout.trim().to_string();

    let record_c = run_br(
        &workspace,
        ["audit", "record", "--kind", "user_action"],
        "record_c",
    );
    assert!(record_c.status.success(), "record C failed");
    let id_c = record_c.stdout.trim().to_string();

    // Verify order is preserved
    let entries = read_interactions(&workspace);
    assert_eq!(entries.len(), 3, "should have 3 entries");
    assert_eq!(entries[0]["id"], id_a);
    assert_eq!(entries[1]["id"], id_b);
    assert_eq!(entries[2]["id"], id_c);
    assert_eq!(entries[0]["kind"], "llm_call");
    assert_eq!(entries[1]["kind"], "tool_call");
    assert_eq!(entries[2]["kind"], "user_action");
    info!("e2e_audit_record_multiple_events_preserve_order: done");
}

#[test]
fn e2e_audit_record_with_all_optional_fields() {
    common::init_test_logging();
    info!("e2e_audit_record_with_all_optional_fields: start");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Record with all optional fields
    let record = run_br(
        &workspace,
        [
            "audit",
            "record",
            "--kind",
            "llm_call",
            "--issue-id",
            "bd-test123",
            "--model",
            "claude-3-opus",
            "--prompt",
            "What is 2+2?",
            "--response",
            "The answer is 4.",
            "--error",
            "",
        ],
        "record_all_fields",
    );
    assert!(
        record.status.success(),
        "record with fields failed: {}",
        record.stderr
    );

    let entries = read_interactions(&workspace);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["kind"], "llm_call");
    assert_eq!(entries[0]["issue_id"], "bd-test123");
    assert_eq!(entries[0]["model"], "claude-3-opus");
    assert_eq!(entries[0]["prompt"], "What is 2+2?");
    assert_eq!(entries[0]["response"], "The answer is 4.");
    // Empty string should not be stored
    assert!(entries[0]["error"].is_null());
    info!("e2e_audit_record_with_all_optional_fields: done");
}

#[test]
fn e2e_audit_record_tool_call_fields() {
    common::init_test_logging();
    info!("e2e_audit_record_tool_call_fields: start");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Record a tool call with tool-specific fields
    let record = run_br(
        &workspace,
        [
            "audit",
            "record",
            "--kind",
            "tool_call",
            "--tool-name",
            "read_file",
            "--exit-code",
            "0",
        ],
        "record_tool_call",
    );
    assert!(
        record.status.success(),
        "record tool_call failed: {}",
        record.stderr
    );

    let entries = read_interactions(&workspace);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["kind"], "tool_call");
    assert_eq!(entries[0]["tool_name"], "read_file");
    assert_eq!(entries[0]["exit_code"], 0);
    info!("e2e_audit_record_tool_call_fields: done");
}

#[test]
fn e2e_audit_record_json_output() {
    common::init_test_logging();
    info!("e2e_audit_record_json_output: start");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Record with --json flag
    let record = run_br(
        &workspace,
        ["audit", "record", "--kind", "llm_call", "--json"],
        "record_json",
    );
    assert!(
        record.status.success(),
        "record json failed: {}",
        record.stderr
    );

    // Parse JSON output
    let payload = extract_json_payload(&record.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse json output");
    assert!(json["id"].is_string(), "id should be string");
    assert_eq!(json["kind"], "llm_call");
    info!("e2e_audit_record_json_output: done");
}

#[test]
fn e2e_audit_label_existing_entry() {
    common::init_test_logging();
    info!("e2e_audit_label_existing_entry: start");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // First record an entry
    let record = run_br(
        &workspace,
        ["audit", "record", "--kind", "llm_call"],
        "record_for_label",
    );
    assert!(record.status.success(), "record failed: {}", record.stderr);
    let parent_id = record.stdout.trim();

    // Label the entry
    let label = run_br(
        &workspace,
        ["audit", "label", parent_id, "--label", "good"],
        "label_entry",
    );
    assert!(label.status.success(), "label failed: {}", label.stderr);
    let label_id = label.stdout.trim();
    assert!(
        label_id.starts_with("int-"),
        "label ID should start with int-"
    );

    // Verify both entries exist
    let entries = read_interactions(&workspace);
    assert_eq!(entries.len(), 2, "should have record and label entries");

    // Find the label entry
    let label_entry = entries.iter().find(|e| e["kind"] == "label").unwrap();
    assert_eq!(label_entry["parent_id"], parent_id);
    assert_eq!(label_entry["label"], "good");
    info!("e2e_audit_label_existing_entry: done");
}

#[test]
fn e2e_audit_label_with_reason() {
    common::init_test_logging();
    info!("e2e_audit_label_with_reason: start");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Record and label with reason
    let record = run_br(
        &workspace,
        ["audit", "record", "--kind", "llm_call"],
        "record_for_label_reason",
    );
    assert!(record.status.success());
    let parent_id = record.stdout.trim();

    let label = run_br(
        &workspace,
        [
            "audit",
            "label",
            parent_id,
            "--label",
            "bad",
            "--reason",
            "Hallucinated information",
        ],
        "label_with_reason",
    );
    assert!(
        label.status.success(),
        "label with reason failed: {}",
        label.stderr
    );

    let entries = read_interactions(&workspace);
    let label_entry = entries.iter().find(|e| e["kind"] == "label").unwrap();
    assert_eq!(label_entry["label"], "bad");
    assert_eq!(label_entry["reason"], "Hallucinated information");
    info!("e2e_audit_label_with_reason: done");
}

#[test]
fn e2e_audit_label_json_output() {
    common::init_test_logging();
    info!("e2e_audit_label_json_output: start");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let record = run_br(
        &workspace,
        ["audit", "record", "--kind", "llm_call"],
        "record_for_label_json",
    );
    assert!(record.status.success());
    let parent_id = record.stdout.trim();

    let label = run_br(
        &workspace,
        ["audit", "label", parent_id, "--label", "good", "--json"],
        "label_json",
    );
    assert!(
        label.status.success(),
        "label json failed: {}",
        label.stderr
    );

    let payload = extract_json_payload(&label.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse label json");
    assert!(json["id"].is_string());
    assert_eq!(json["parent_id"], parent_id);
    assert_eq!(json["label"], "good");
    info!("e2e_audit_label_json_output: done");
}

// =============================================================================
// ERROR CASE TESTS
// =============================================================================

#[test]
fn e2e_audit_record_before_init_fails() {
    common::init_test_logging();
    info!("e2e_audit_record_before_init_fails: start");
    let workspace = BrWorkspace::new();

    // Try to record without init
    let record = run_br(
        &workspace,
        ["audit", "record", "--kind", "llm_call"],
        "record_no_init",
    );
    assert!(
        !record.status.success(),
        "audit record should fail before init"
    );
    assert!(
        record.stderr.contains("not initialized")
            || record.stderr.contains("NotInitialized")
            || record.stderr.contains("not found"),
        "error should mention initialization: {}",
        record.stderr
    );
    info!("e2e_audit_record_before_init_fails: done");
}

#[test]
fn e2e_audit_record_without_kind_fails() {
    common::init_test_logging();
    info!("e2e_audit_record_without_kind_fails: start");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Record without --kind
    let record = run_br(&workspace, ["audit", "record"], "record_no_kind");
    assert!(
        !record.status.success(),
        "audit record without kind should fail"
    );
    // Check either stderr or that validation error occurred
    let combined = format!("{}{}", record.stdout, record.stderr);
    assert!(
        combined.contains("kind") || combined.contains("required"),
        "error should mention kind is required: {combined}"
    );
    info!("e2e_audit_record_without_kind_fails: done");
}

#[test]
fn e2e_audit_label_without_label_fails() {
    common::init_test_logging();
    info!("e2e_audit_label_without_label_fails: start");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let record = run_br(
        &workspace,
        ["audit", "record", "--kind", "llm_call"],
        "record_for_label_fail",
    );
    assert!(record.status.success());
    let parent_id = record.stdout.trim();

    // Label without --label flag
    let label = run_br(
        &workspace,
        ["audit", "label", parent_id],
        "label_without_label",
    );
    assert!(!label.status.success(), "label without --label should fail");
    let combined = format!("{}{}", label.stdout, label.stderr);
    assert!(
        combined.contains("label") || combined.contains("required"),
        "error should mention label is required: {combined}"
    );
    info!("e2e_audit_label_without_label_fails: done");
}

// =============================================================================
// EDGE CASE TESTS
// =============================================================================

#[test]
fn e2e_audit_record_very_long_text() {
    common::init_test_logging();
    info!("e2e_audit_record_very_long_text: start");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create very long prompt and response text
    let long_prompt = "a".repeat(10_000);
    let long_response = "b".repeat(10_000);

    let record = run_br(
        &workspace,
        [
            "audit",
            "record",
            "--kind",
            "llm_call",
            "--prompt",
            &long_prompt,
            "--response",
            &long_response,
        ],
        "record_long_text",
    );
    assert!(
        record.status.success(),
        "record with long text failed: {}",
        record.stderr
    );

    let entries = read_interactions(&workspace);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["prompt"].as_str().unwrap().len(), 10_000);
    assert_eq!(entries[0]["response"].as_str().unwrap().len(), 10_000);
    info!("e2e_audit_record_very_long_text: done");
}

#[test]
fn e2e_audit_record_special_characters() {
    common::init_test_logging();
    info!("e2e_audit_record_special_characters: start");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Unicode, quotes, newlines, etc.
    let special_prompt = "Hello\nWorld\t\"quoted\" 'single' emoji: \u{1F600}";

    let record = run_br(
        &workspace,
        [
            "audit",
            "record",
            "--kind",
            "llm_call",
            "--prompt",
            special_prompt,
        ],
        "record_special_chars",
    );
    assert!(
        record.status.success(),
        "record with special chars failed: {}",
        record.stderr
    );

    let entries = read_interactions(&workspace);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["prompt"], special_prompt);
    info!("e2e_audit_record_special_characters: done");
}

#[test]
fn e2e_audit_record_via_stdin() {
    common::init_test_logging();
    info!("e2e_audit_record_via_stdin: start");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create JSON input
    let json_input = r#"{"kind": "llm_call", "model": "gpt-4", "prompt": "stdin test"}"#;

    // Run br with stdin
    let br_path = assert_cmd::cargo::cargo_bin!("br");
    let mut child = Command::new(br_path)
        .args(["audit", "record", "--stdin"])
        .current_dir(&workspace.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("NO_COLOR", "1")
        .spawn()
        .expect("spawn br");

    {
        let stdin = child.stdin.as_mut().expect("stdin");
        stdin.write_all(json_input.as_bytes()).expect("write stdin");
    }

    let output = child.wait_with_output().expect("wait for br");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "stdin record failed: stdout={stdout}, stderr={stderr}"
    );

    let entries = read_interactions(&workspace);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["kind"], "llm_call");
    assert_eq!(entries[0]["model"], "gpt-4");
    assert_eq!(entries[0]["prompt"], "stdin test");
    info!("e2e_audit_record_via_stdin: done");
}

#[test]
fn e2e_audit_record_created_at_auto_set() {
    common::init_test_logging();
    info!("e2e_audit_record_created_at_auto_set: start");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let record = run_br(
        &workspace,
        ["audit", "record", "--kind", "test_event"],
        "record_timestamp",
    );
    assert!(record.status.success());

    let entries = read_interactions(&workspace);
    assert_eq!(entries.len(), 1);

    // Verify created_at was set
    assert!(
        entries[0]["created_at"].is_string(),
        "created_at should be set"
    );
    let created_at = entries[0]["created_at"].as_str().unwrap();
    // Should be a valid ISO 8601 timestamp
    assert!(
        created_at.contains('T') && created_at.contains('Z'),
        "created_at should be ISO 8601: {created_at}"
    );
    info!("e2e_audit_record_created_at_auto_set: done");
}

#[test]
fn e2e_audit_unique_ids() {
    common::init_test_logging();
    info!("e2e_audit_unique_ids: start");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create many entries quickly
    let mut ids: Vec<String> = Vec::new();
    for i in 0..20 {
        let record = run_br(
            &workspace,
            ["audit", "record", "--kind", &format!("event_{i}")],
            &format!("record_{i}"),
        );
        assert!(record.status.success(), "record {i} failed");
        ids.push(record.stdout.trim().to_string());
    }

    // Verify all IDs are unique
    let unique_count = {
        let mut sorted = ids.clone();
        sorted.sort();
        sorted.dedup();
        sorted.len()
    };
    assert_eq!(unique_count, ids.len(), "all IDs should be unique: {ids:?}");
    info!("e2e_audit_unique_ids: done");
}

#[test]
fn e2e_audit_interactions_file_created() {
    common::init_test_logging();
    info!("e2e_audit_interactions_file_created: start");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Verify interactions.jsonl doesn't exist yet
    let path = workspace.root.join(".beads").join("interactions.jsonl");
    assert!(!path.exists(), "interactions.jsonl should not exist yet");

    // Record an entry
    let record = run_br(
        &workspace,
        ["audit", "record", "--kind", "test"],
        "record_create_file",
    );
    assert!(record.status.success());

    // Now it should exist
    assert!(
        path.exists(),
        "interactions.jsonl should exist after first record"
    );
    info!("e2e_audit_interactions_file_created: done");
}

#[test]
fn e2e_audit_with_actor_override() {
    common::init_test_logging();
    info!("e2e_audit_with_actor_override: start");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Record with actor override
    let record = run_br(
        &workspace,
        [
            "--actor",
            "test-agent",
            "audit",
            "record",
            "--kind",
            "llm_call",
        ],
        "record_with_actor",
    );
    assert!(
        record.status.success(),
        "record with actor failed: {}",
        record.stderr
    );

    let entries = read_interactions(&workspace);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["actor"], "test-agent");
    info!("e2e_audit_with_actor_override: done");
}

#[test]
fn e2e_audit_log_for_issue() {
    common::init_test_logging();
    info!("e2e_audit_log_for_issue: start");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    // Create an issue
    let create = run_br(&workspace, ["create", "Test Issue"], "create");
    assert!(create.status.success());
    // Parse ID from output: "Created bd-123: Test Issue" or similar
    let output = create.stdout.trim();
    let id = if let Some(word) = output.split_whitespace().find(|word| word.starts_with("bd-")) {
        word.trim_end_matches(':').to_string() // Remove trailing colon
    } else {
        // Fallback
        output.split(':').next().unwrap_or("").replace("Created ", "").trim().to_string()
    };

    // Update it to generate events
    let update = run_br(&workspace, ["update", &id, "--priority", "0"], "update_priority");
    assert!(update.status.success(), "update failed: {}", update.stderr);
    
    let close = run_br(&workspace, ["close", &id, "--reason", "Done"], "close");
    assert!(close.status.success(), "close failed: {}", close.stderr);

    // Check log
    let log = run_br(&workspace, ["audit", "log", &id], "audit_log");
    assert!(log.status.success(), "audit log failed: {}", log.stderr);
    assert!(log.stdout.contains("created"), "should show created event");
    assert!(
        log.stdout.contains("priority_changed") || log.stdout.contains("updated"),
        "should show update event"
    );
    assert!(log.stdout.contains("closed"), "should show closed event");
    assert!(log.stdout.contains("Done"), "should show close reason");

    // Check JSON log
    let log_json = run_br(
        &workspace,
        ["audit", "log", &id, "--json"],
        "audit_log_json",
    );
    assert!(log_json.status.success());
    let payload = extract_json_payload(&log_json.stdout);
    let json: Value = serde_json::from_str(&payload).expect("valid json");
    assert_eq!(json["issue_id"], id);
    assert!(json["events"].as_array().unwrap().len() >= 3);

    info!("e2e_audit_log_for_issue: done");
}

#[test]
fn e2e_audit_summary() {
    common::init_test_logging();
    info!("e2e_audit_summary: start");
    let workspace = BrWorkspace::new();
    run_br(&workspace, ["init"], "init");

    // Generate activity
    run_br(&workspace, ["create", "Issue 1"], "create1");
    run_br(&workspace, ["create", "Issue 2"], "create2");

    // Get ID of Issue 1
    let list = run_br(&workspace, ["list", "--json"], "list");
    let json: Vec<Value> = serde_json::from_str(&extract_json_payload(&list.stdout)).unwrap();
    let id1 = json.iter().find(|i| i["title"] == "Issue 1").unwrap()["id"]
        .as_str()
        .unwrap();

    run_br(&workspace, ["close", id1], "close");

    // Check summary
    let summary = run_br(&workspace, ["audit", "summary"], "audit_summary");
    assert!(
        summary.status.success(),
        "audit summary failed: {}",
        summary.stderr
    );
    assert!(summary.stdout.contains("Audit Summary"), "should show title");
    assert!(summary.stdout.contains("TOTAL"), "should show totals");

    // Check JSON summary
    let summary_json = run_br(
        &workspace,
        ["audit", "summary", "--json"],
        "audit_summary_json",
    );
    assert!(summary_json.status.success());
    let payload = extract_json_payload(&summary_json.stdout);
    let json: Value = serde_json::from_str(&payload).expect("valid json");

    let totals = &json["totals"];
    assert!(totals["created"].as_u64().unwrap() >= 2);
    assert!(totals["closed"].as_u64().unwrap() >= 1);

    info!("e2e_audit_summary: done");
}
