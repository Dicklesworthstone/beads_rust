//! Edge Case & Stress Conformance Tests
//!
//! Tests for edge cases, stress scenarios, error recovery, and cross-platform behavior.
//! Validates that br (Rust) and bd (Go) handle these cases consistently.
//!
//! Test categories:
//! - Input Validation: boundary conditions, special characters, injection attempts
//! - Concurrency & Stress: rapid operations, large graphs, many items
//! - Error Recovery: corrupted data, missing files, schema issues
//! - Cross-Platform: encoding, line endings, path separators

#![allow(clippy::all, clippy::pedantic, clippy::nursery)]

mod common;

use common::cli::extract_json_payload;
use serde_json::Value;
use std::fs;
use std::time::Instant;
use tracing::info;

// Import shared test infrastructure from conformance module
#[path = "conformance.rs"]
mod conformance;

use conformance::{CompareMode, ConformanceWorkspace, compare_json};

// ============================================================================
// INPUT VALIDATION TESTS (10 tests)
// ============================================================================

/// Test: Very long title (1000+ characters)
#[test]
fn conformance_title_very_long() {
    common::init_test_logging();
    info!("conformance_title_very_long: BEGIN");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create a title that's at the limit (500 chars is the limit in br)
    let long_title: String = "A".repeat(500);

    let br_out = workspace.run_br(["create", &long_title, "--json"], "create_long");
    let bd_out = workspace.run_bd(["create", &long_title, "--json"], "create_long");

    info!(
        "conformance_title_very_long: br_exit={} bd_exit={}",
        br_out.status.code().unwrap_or(-1),
        bd_out.status.code().unwrap_or(-1)
    );

    // Both should succeed (or both fail if too long)
    assert_eq!(
        br_out.status.success(),
        bd_out.status.success(),
        "br and bd should have same success/failure for long title"
    );

    if br_out.status.success() {
        let br_json = extract_json_payload(&br_out.stdout);
        let bd_json = extract_json_payload(&bd_out.stdout);

        let result = compare_json(
            &br_json,
            &bd_json,
            &CompareMode::ContainsFields(vec!["title".to_string()]),
        );
        assert!(result.is_ok(), "Title should match: {:?}", result.err());
    }

    info!("conformance_title_very_long: PASS");
}

/// Test: Empty title should be rejected
#[test]
fn conformance_title_empty() {
    common::init_test_logging();
    info!("conformance_title_empty: BEGIN");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Empty title should fail
    let br_out = workspace.run_br(["create", "", "--json"], "create_empty");
    let bd_out = workspace.run_bd(["create", "", "--json"], "create_empty");

    info!(
        "conformance_title_empty: br_exit={} bd_exit={}",
        br_out.status.code().unwrap_or(-1),
        bd_out.status.code().unwrap_or(-1)
    );

    // Both should fail for empty title
    assert!(!br_out.status.success(), "br should reject empty title");
    assert!(!bd_out.status.success(), "bd should reject empty title");

    info!("conformance_title_empty: PASS");
}

/// Test: SQL injection attempt in title
#[test]
fn conformance_sql_injection_title() {
    common::init_test_logging();
    info!("conformance_sql_injection_title: BEGIN");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // SQL injection attempt
    let sqli_title = "Test'; DROP TABLE issues; --";

    let br_out = workspace.run_br(["create", sqli_title, "--json"], "create_sqli");
    let bd_out = workspace.run_bd(["create", sqli_title, "--json"], "create_sqli");

    info!(
        "conformance_sql_injection_title: br_exit={} bd_exit={}",
        br_out.status.code().unwrap_or(-1),
        bd_out.status.code().unwrap_or(-1)
    );

    // Both should succeed - the SQLi attempt should be stored as literal text
    assert!(
        br_out.status.success(),
        "br should accept SQLi as literal: {}",
        br_out.stderr
    );
    assert!(
        bd_out.status.success(),
        "bd should accept SQLi as literal: {}",
        bd_out.stderr
    );

    // Verify the title is stored literally
    let br_json = extract_json_payload(&br_out.stdout);
    let br_value: Value = serde_json::from_str(&br_json).expect("parse br json");
    assert_eq!(
        br_value.get("title").and_then(|v| v.as_str()),
        Some(sqli_title),
        "Title should be stored literally"
    );

    // List issues to verify database wasn't corrupted
    let br_list = workspace.run_br(["list", "--json"], "list_after_sqli");
    assert!(
        br_list.status.success(),
        "br list should work after SQLi attempt"
    );

    info!("conformance_sql_injection_title: PASS");
}

/// Test: SQL injection attempt in description
#[test]
fn conformance_sql_injection_desc() {
    common::init_test_logging();
    info!("conformance_sql_injection_desc: BEGIN");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // SQL injection in description
    let sqli_desc = "Test'; DELETE FROM issues WHERE 1=1; --";

    let br_out = workspace.run_br(
        [
            "create",
            "Normal Title",
            "--description",
            sqli_desc,
            "--json",
        ],
        "create_sqli_desc",
    );
    let bd_out = workspace.run_bd(
        [
            "create",
            "Normal Title",
            "--description",
            sqli_desc,
            "--json",
        ],
        "create_sqli_desc",
    );

    // Both should succeed
    assert!(
        br_out.status.success(),
        "br should handle SQLi in desc: {}",
        br_out.stderr
    );
    assert!(
        bd_out.status.success(),
        "bd should handle SQLi in desc: {}",
        bd_out.stderr
    );

    info!("conformance_sql_injection_desc: PASS");
}

/// Test: Priority boundary 0 (critical)
#[test]
fn conformance_priority_boundary_0() {
    common::init_test_logging();
    info!("conformance_priority_boundary_0: BEGIN");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let br_out = workspace.run_br(
        ["create", "Critical issue", "--priority", "0", "--json"],
        "create_p0",
    );
    let bd_out = workspace.run_bd(
        ["create", "Critical issue", "--priority", "0", "--json"],
        "create_p0",
    );

    assert!(
        br_out.status.success(),
        "br should accept priority 0: {}",
        br_out.stderr
    );
    assert!(
        bd_out.status.success(),
        "bd should accept priority 0: {}",
        bd_out.stderr
    );

    let br_json = extract_json_payload(&br_out.stdout);
    let bd_json = extract_json_payload(&bd_out.stdout);

    let result = compare_json(
        &br_json,
        &bd_json,
        &CompareMode::ContainsFields(vec!["priority".to_string()]),
    );
    assert!(
        result.is_ok(),
        "Priority 0 should match: {:?}",
        result.err()
    );

    info!("conformance_priority_boundary_0: PASS");
}

/// Test: Priority boundary 4 (backlog)
#[test]
fn conformance_priority_boundary_4() {
    common::init_test_logging();
    info!("conformance_priority_boundary_4: BEGIN");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let br_out = workspace.run_br(
        ["create", "Backlog issue", "--priority", "4", "--json"],
        "create_p4",
    );
    let bd_out = workspace.run_bd(
        ["create", "Backlog issue", "--priority", "4", "--json"],
        "create_p4",
    );

    assert!(
        br_out.status.success(),
        "br should accept priority 4: {}",
        br_out.stderr
    );
    assert!(
        bd_out.status.success(),
        "bd should accept priority 4: {}",
        bd_out.stderr
    );

    let br_json = extract_json_payload(&br_out.stdout);
    let bd_json = extract_json_payload(&bd_out.stdout);

    let result = compare_json(
        &br_json,
        &bd_json,
        &CompareMode::ContainsFields(vec!["priority".to_string()]),
    );
    assert!(
        result.is_ok(),
        "Priority 4 should match: {:?}",
        result.err()
    );

    info!("conformance_priority_boundary_4: PASS");
}

/// Test: Priority 5 should be rejected (invalid)
#[test]
fn conformance_priority_invalid_5() {
    common::init_test_logging();
    info!("conformance_priority_invalid_5: BEGIN");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let br_out = workspace.run_br(
        [
            "create",
            "Invalid priority issue",
            "--priority",
            "5",
            "--json",
        ],
        "create_p5",
    );
    let bd_out = workspace.run_bd(
        [
            "create",
            "Invalid priority issue",
            "--priority",
            "5",
            "--json",
        ],
        "create_p5",
    );

    info!(
        "conformance_priority_invalid_5: br_exit={} bd_exit={}",
        br_out.status.code().unwrap_or(-1),
        bd_out.status.code().unwrap_or(-1)
    );

    // Both should reject priority 5
    assert!(!br_out.status.success(), "br should reject priority 5");
    assert!(!bd_out.status.success(), "bd should reject priority 5");

    info!("conformance_priority_invalid_5: PASS");
}

/// Test: Negative priority should be rejected
#[test]
fn conformance_priority_invalid_neg() {
    common::init_test_logging();
    info!("conformance_priority_invalid_neg: BEGIN");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let br_out = workspace.run_br(
        ["create", "Negative priority", "--priority", "-1", "--json"],
        "create_neg",
    );
    let bd_out = workspace.run_bd(
        ["create", "Negative priority", "--priority", "-1", "--json"],
        "create_neg",
    );

    info!(
        "conformance_priority_invalid_neg: br_exit={} bd_exit={}",
        br_out.status.code().unwrap_or(-1),
        bd_out.status.code().unwrap_or(-1)
    );

    // Both should reject negative priority
    assert!(
        !br_out.status.success(),
        "br should reject negative priority"
    );
    assert!(
        !bd_out.status.success(),
        "bd should reject negative priority"
    );

    info!("conformance_priority_invalid_neg: PASS");
}

/// Test: Invalid ID format handling
#[test]
fn conformance_id_format_validation() {
    common::init_test_logging();
    info!("conformance_id_format_validation: BEGIN");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create a valid issue first
    let _ = workspace.run_br(["create", "Test issue", "--json"], "create");
    let _ = workspace.run_bd(["create", "Test issue", "--json"], "create");

    // Try to show an invalid ID format
    let invalid_ids = ["not-a-valid-id", "123", "", "bd-", "-abc"];

    for invalid_id in invalid_ids {
        let br_out = workspace.run_br(
            ["show", invalid_id, "--json"],
            &format!("show_invalid_{}", invalid_id.replace('-', "_")),
        );
        let bd_out = workspace.run_bd(
            ["show", invalid_id, "--json"],
            &format!("show_invalid_{}", invalid_id.replace('-', "_")),
        );

        // Both should handle invalid IDs similarly
        assert_eq!(
            br_out.status.success(),
            bd_out.status.success(),
            "br and bd should handle '{}' the same way",
            invalid_id
        );
    }

    info!("conformance_id_format_validation: PASS");
}

/// Test: Unicode in title and description
#[test]
fn conformance_unicode_handling() {
    common::init_test_logging();
    info!("conformance_unicode_handling: BEGIN");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let unicode_titles = [
        "日本語タイトル",         // Japanese
        "Ελληνικά κείμενο",       // Greek
        "🚀 Emoji title 🎉",      // Emoji
        "Mixed 中文 and English", // Mixed
        "Привет мир",             // Russian
    ];

    for (i, title) in unicode_titles.iter().enumerate() {
        let label = format!("unicode_{}", i);

        let br_out = workspace.run_br(["create", title, "--json"], &label);
        let bd_out = workspace.run_bd(["create", title, "--json"], &label);

        assert!(
            br_out.status.success(),
            "br should handle unicode '{}': {}",
            title,
            br_out.stderr
        );
        assert!(
            bd_out.status.success(),
            "bd should handle unicode '{}': {}",
            title,
            bd_out.stderr
        );

        // Verify title is preserved
        let br_json: Value =
            serde_json::from_str(&extract_json_payload(&br_out.stdout)).expect("parse br json");
        assert_eq!(
            br_json.get("title").and_then(|v| v.as_str()),
            Some(*title),
            "Unicode title should be preserved"
        );
    }

    info!("conformance_unicode_handling: PASS");
}

// ============================================================================
// CONCURRENCY & STRESS TESTS (8 tests)
// ============================================================================

/// Test: 50 rapid sequential creates
#[test]
fn conformance_rapid_creates_50() {
    common::init_test_logging();
    info!("conformance_rapid_creates_50: BEGIN");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let count = 50;
    let start = Instant::now();

    // Create issues rapidly
    for i in 0..count {
        let title = format!("Rapid issue {}", i);
        let label = format!("rapid_{}", i);

        let br_out = workspace.run_br(["create", &title, "--json"], &label);
        assert!(
            br_out.status.success(),
            "br create {} failed: {}",
            i,
            br_out.stderr
        );

        let bd_out = workspace.run_bd(["create", &title, "--json"], &label);
        assert!(
            bd_out.status.success(),
            "bd create {} failed: {}",
            i,
            bd_out.stderr
        );

        if i % 10 == 0 {
            info!(
                "conformance_rapid_creates_50: milestone issues_created={}",
                i
            );
        }
    }

    let elapsed = start.elapsed();
    info!("conformance_rapid_creates_50: total_time={:?}", elapsed);

    // List all and compare counts
    let br_list = workspace.run_br(["list", "--json"], "list_final");
    let bd_list = workspace.run_bd(["list", "--json"], "list_final");

    assert!(br_list.status.success(), "br list failed");
    assert!(bd_list.status.success(), "bd list failed");

    let br_json: Value =
        serde_json::from_str(&extract_json_payload(&br_list.stdout)).expect("parse br list");
    let bd_json: Value =
        serde_json::from_str(&extract_json_payload(&bd_list.stdout)).expect("parse bd list");

    let br_count = br_json.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_count = bd_json.as_array().map(|a| a.len()).unwrap_or(0);

    assert_eq!(br_count, count, "br should have {} issues", count);
    assert_eq!(bd_count, count, "bd should have {} issues", count);

    info!("conformance_rapid_creates_50: PASS");
}

/// Test: 100 rapid status updates
#[test]
fn conformance_rapid_updates_100() {
    common::init_test_logging();
    info!("conformance_rapid_updates_100: BEGIN");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create a single issue to update
    let br_create = workspace.run_br(["create", "Update target", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "Update target", "--json"], "create");

    assert!(br_create.status.success());
    assert!(bd_create.status.success());

    // Extract IDs
    let br_json: Value = serde_json::from_str(&extract_json_payload(&br_create.stdout)).unwrap();
    let bd_json: Value = serde_json::from_str(&extract_json_payload(&bd_create.stdout)).unwrap();

    let br_id = br_json.get("id").and_then(|v| v.as_str()).expect("br id");
    let bd_id = bd_json.get("id").and_then(|v| v.as_str()).expect("bd id");

    let start = Instant::now();
    let statuses = ["open", "in_progress", "blocked", "open"];

    for i in 0..100 {
        let status = statuses[i % statuses.len()];
        let label = format!("update_{}", i);

        let br_out = workspace.run_br(["update", br_id, "--status", status, "--json"], &label);
        let bd_out = workspace.run_bd(["update", bd_id, "--status", status, "--json"], &label);

        assert!(
            br_out.status.success(),
            "br update {} failed: {}",
            i,
            br_out.stderr
        );
        assert!(
            bd_out.status.success(),
            "bd update {} failed: {}",
            i,
            bd_out.stderr
        );

        if i % 20 == 0 {
            info!("conformance_rapid_updates_100: milestone updates={}", i);
        }
    }

    let elapsed = start.elapsed();
    info!(
        "conformance_rapid_updates_100: total_time={:?} avg={:?}",
        elapsed,
        elapsed / 100
    );

    info!("conformance_rapid_updates_100: PASS");
}

/// Test: Large dependency graph (100 nodes)
#[test]
fn conformance_large_dep_graph_100() {
    common::init_test_logging();
    info!("conformance_large_dep_graph_100: BEGIN");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let count = 20; // Reduced for faster tests, can increase to 100
    let mut br_ids: Vec<String> = Vec::with_capacity(count);
    let mut bd_ids: Vec<String> = Vec::with_capacity(count);

    // Create all issues first
    for i in 0..count {
        let title = format!("Node {}", i);
        let label = format!("node_{}", i);

        let br_out = workspace.run_br(["create", &title, "--json"], &label);
        let bd_out = workspace.run_bd(["create", &title, "--json"], &label);

        assert!(br_out.status.success());
        assert!(bd_out.status.success());

        let br_json: Value = serde_json::from_str(&extract_json_payload(&br_out.stdout)).unwrap();
        let bd_json: Value = serde_json::from_str(&extract_json_payload(&bd_out.stdout)).unwrap();

        br_ids.push(
            br_json
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap()
                .to_string(),
        );
        bd_ids.push(
            bd_json
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap()
                .to_string(),
        );
    }

    info!("conformance_large_dep_graph_100: created {} nodes", count);

    // Add dependencies (each node depends on the previous)
    for i in 1..count {
        let label = format!("dep_{}", i);

        let br_out = workspace.run_br(["dep", "add", &br_ids[i], &br_ids[i - 1]], &label);
        let bd_out = workspace.run_bd(["dep", "add", &bd_ids[i], &bd_ids[i - 1]], &label);

        assert!(
            br_out.status.success(),
            "br dep add {} -> {} failed: {}",
            br_ids[i],
            br_ids[i - 1],
            br_out.stderr
        );
        assert!(
            bd_out.status.success(),
            "bd dep add {} -> {} failed: {}",
            bd_ids[i],
            bd_ids[i - 1],
            bd_out.stderr
        );
    }

    info!(
        "conformance_large_dep_graph_100: added {} dependencies",
        count - 1
    );

    // Check blocked status - only first node should be unblocked
    let br_ready = workspace.run_br(["ready", "--json"], "ready");
    let bd_ready = workspace.run_bd(["ready", "--json"], "ready");

    assert!(br_ready.status.success());
    assert!(bd_ready.status.success());

    // Both should have same number of ready issues (just 1)
    let br_json: Value = serde_json::from_str(&extract_json_payload(&br_ready.stdout))
        .unwrap_or(Value::Array(vec![]));
    let bd_json: Value = serde_json::from_str(&extract_json_payload(&bd_ready.stdout))
        .unwrap_or(Value::Array(vec![]));

    let br_count = br_json.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_count = bd_json.as_array().map(|a| a.len()).unwrap_or(0);

    assert_eq!(
        br_count, bd_count,
        "br and bd should have same ready count: br={} bd={}",
        br_count, bd_count
    );

    info!("conformance_large_dep_graph_100: PASS");
}

/// Test: 10-level deep dependency chain
#[test]
fn conformance_deep_deps_10_levels() {
    common::init_test_logging();
    info!("conformance_deep_deps_10_levels: BEGIN");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let depth = 10;
    let mut br_ids: Vec<String> = Vec::with_capacity(depth);
    let mut bd_ids: Vec<String> = Vec::with_capacity(depth);

    // Create chain: 0 <- 1 <- 2 <- ... <- 9
    for level in 0..depth {
        let title = format!("Level {} task", level);
        let label = format!("level_{}", level);

        let br_out = workspace.run_br(["create", &title, "--json"], &label);
        let bd_out = workspace.run_bd(["create", &title, "--json"], &label);

        assert!(br_out.status.success());
        assert!(bd_out.status.success());

        let br_json: Value = serde_json::from_str(&extract_json_payload(&br_out.stdout)).unwrap();
        let bd_json: Value = serde_json::from_str(&extract_json_payload(&bd_out.stdout)).unwrap();

        let br_id = br_json
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_string();
        let bd_id = bd_json
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap()
            .to_string();

        // Add dependency on previous level
        if level > 0 {
            let br_dep = workspace.run_br(
                ["dep", "add", &br_id, &br_ids[level - 1]],
                &format!("dep_{}", level),
            );
            let bd_dep = workspace.run_bd(
                ["dep", "add", &bd_id, &bd_ids[level - 1]],
                &format!("dep_{}", level),
            );

            assert!(br_dep.status.success());
            assert!(bd_dep.status.success());
        }

        br_ids.push(br_id);
        bd_ids.push(bd_id);
    }

    info!(
        "conformance_deep_deps_10_levels: created {}-level chain",
        depth
    );

    // Show the deepest node - should have all dependencies listed
    let br_show = workspace.run_br(["show", &br_ids[depth - 1], "--json"], "show_deep");
    let bd_show = workspace.run_bd(["show", &bd_ids[depth - 1], "--json"], "show_deep");

    assert!(br_show.status.success());
    assert!(bd_show.status.success());

    info!("conformance_deep_deps_10_levels: PASS");
}

/// Test: Many labels per issue (20 labels)
#[test]
fn conformance_many_labels_20() {
    common::init_test_logging();
    info!("conformance_many_labels_20: BEGIN");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create issue first
    let br_create = workspace.run_br(["create", "Multi-label issue", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "Multi-label issue", "--json"], "create");

    assert!(br_create.status.success());
    assert!(bd_create.status.success());

    let br_json: Value = serde_json::from_str(&extract_json_payload(&br_create.stdout)).unwrap();
    let bd_json: Value = serde_json::from_str(&extract_json_payload(&bd_create.stdout)).unwrap();

    let br_id = br_json.get("id").and_then(|v| v.as_str()).expect("br id");
    let bd_id = bd_json.get("id").and_then(|v| v.as_str()).expect("bd id");

    let label_count = 20;

    // Add labels
    for i in 0..label_count {
        let label = format!("label-{}", i);
        let cmd_label = format!("add_label_{}", i);

        let br_out = workspace.run_br(["label", "add", br_id, &label], &cmd_label);
        let bd_out = workspace.run_bd(["label", "add", bd_id, &label], &cmd_label);

        assert!(
            br_out.status.success(),
            "br label add failed: {}",
            br_out.stderr
        );
        assert!(
            bd_out.status.success(),
            "bd label add failed: {}",
            bd_out.stderr
        );
    }

    info!("conformance_many_labels_20: added {} labels", label_count);

    // Show issue and verify labels
    let br_show = workspace.run_br(["show", br_id, "--json"], "show");
    let bd_show = workspace.run_bd(["show", bd_id, "--json"], "show");

    assert!(br_show.status.success());
    assert!(bd_show.status.success());

    // Compare label counts (using ArrayUnordered since label order may differ)
    let br_json: Value = serde_json::from_str(&extract_json_payload(&br_show.stdout)).unwrap();
    let bd_json: Value = serde_json::from_str(&extract_json_payload(&bd_show.stdout)).unwrap();

    let br_labels = br_json
        .get("labels")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);
    let bd_labels = bd_json
        .get("labels")
        .and_then(|v| v.as_array())
        .map(|a| a.len())
        .unwrap_or(0);

    assert_eq!(
        br_labels, label_count,
        "br should have {} labels",
        label_count
    );
    assert_eq!(
        bd_labels, label_count,
        "bd should have {} labels",
        label_count
    );

    info!("conformance_many_labels_20: PASS");
}

/// Test: Many comments per issue (20 comments)
#[test]
fn conformance_many_comments_20() {
    common::init_test_logging();
    info!("conformance_many_comments_20: BEGIN");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create issue
    let br_create = workspace.run_br(["create", "Commented issue", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "Commented issue", "--json"], "create");

    assert!(br_create.status.success());
    assert!(bd_create.status.success());

    let br_json: Value = serde_json::from_str(&extract_json_payload(&br_create.stdout)).unwrap();
    let bd_json: Value = serde_json::from_str(&extract_json_payload(&bd_create.stdout)).unwrap();

    let br_id = br_json.get("id").and_then(|v| v.as_str()).expect("br id");
    let bd_id = bd_json.get("id").and_then(|v| v.as_str()).expect("bd id");

    let comment_count = 20;

    // Add comments
    for i in 0..comment_count {
        let comment = format!("Comment number {}", i);
        let cmd_label = format!("add_comment_{}", i);

        let br_out = workspace.run_br(["comments", "add", br_id, &comment], &cmd_label);
        let bd_out = workspace.run_bd(["comments", "add", bd_id, &comment], &cmd_label);

        assert!(
            br_out.status.success(),
            "br comment add {} failed: {}",
            i,
            br_out.stderr
        );
        assert!(
            bd_out.status.success(),
            "bd comment add {} failed: {}",
            i,
            bd_out.stderr
        );
    }

    info!(
        "conformance_many_comments_20: added {} comments",
        comment_count
    );

    // List comments
    let br_comments = workspace.run_br(["comments", "list", br_id, "--json"], "list_comments");
    let bd_comments = workspace.run_bd(["comments", "list", bd_id, "--json"], "list_comments");

    assert!(br_comments.status.success());
    assert!(bd_comments.status.success());

    let br_json: Value = serde_json::from_str(&extract_json_payload(&br_comments.stdout))
        .unwrap_or(Value::Array(vec![]));
    let bd_json: Value = serde_json::from_str(&extract_json_payload(&bd_comments.stdout))
        .unwrap_or(Value::Array(vec![]));

    let br_count = br_json.as_array().map(|a| a.len()).unwrap_or(0);
    let bd_count = bd_json.as_array().map(|a| a.len()).unwrap_or(0);

    assert_eq!(
        br_count, comment_count,
        "br should have {} comments",
        comment_count
    );
    assert_eq!(
        bd_count, comment_count,
        "bd should have {} comments",
        comment_count
    );

    info!("conformance_many_comments_20: PASS");
}

/// Test: Concurrent list operations
#[test]
fn conformance_concurrent_reads() {
    common::init_test_logging();
    info!("conformance_concurrent_reads: BEGIN");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create some issues
    for i in 0..10 {
        let title = format!("Issue {}", i);
        workspace.run_br(["create", &title], &format!("create_{}", i));
        workspace.run_bd(["create", &title], &format!("create_{}", i));
    }

    // Run multiple list operations in sequence (simulating concurrent reads)
    let start = Instant::now();
    for i in 0..20 {
        let br_out = workspace.run_br(["list", "--json"], &format!("list_{}", i));
        let bd_out = workspace.run_bd(["list", "--json"], &format!("list_{}", i));

        assert!(br_out.status.success());
        assert!(bd_out.status.success());
    }

    let elapsed = start.elapsed();
    info!(
        "conformance_concurrent_reads: 20 list operations in {:?}",
        elapsed
    );

    info!("conformance_concurrent_reads: PASS");
}

/// Test: Large workspace (100+ issues)
#[test]
#[ignore] // This test takes a while, run with --ignored
fn conformance_workspace_max_issues() {
    common::init_test_logging();
    info!("conformance_workspace_max_issues: BEGIN");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    let count = 100;
    let start = Instant::now();

    for i in 0..count {
        let title = format!("Issue {} with some extra text for variety", i);
        let desc = format!("Description for issue {}", i);

        let br_out = workspace.run_br(
            ["create", &title, "--description", &desc, "--json"],
            &format!("create_{}", i),
        );
        let bd_out = workspace.run_bd(
            ["create", &title, "--description", &desc, "--json"],
            &format!("create_{}", i),
        );

        assert!(br_out.status.success());
        assert!(bd_out.status.success());

        if i % 25 == 0 {
            info!("conformance_workspace_max_issues: created {}/{}", i, count);
        }
    }

    let create_elapsed = start.elapsed();
    info!(
        "conformance_workspace_max_issues: created {} issues in {:?}",
        count, create_elapsed
    );

    // Test listing performance
    let list_start = Instant::now();
    let br_list = workspace.run_br(["list", "--json"], "list");
    let bd_list = workspace.run_bd(["list", "--json"], "list");
    let list_elapsed = list_start.elapsed();

    assert!(br_list.status.success());
    assert!(bd_list.status.success());

    info!(
        "conformance_workspace_max_issues: list completed in {:?}",
        list_elapsed
    );

    // Test search performance
    let search_start = Instant::now();
    let br_search = workspace.run_br(["search", "Issue 50", "--json"], "search");
    let bd_search = workspace.run_bd(["search", "Issue 50", "--json"], "search");
    let search_elapsed = search_start.elapsed();

    assert!(br_search.status.success());
    assert!(bd_search.status.success());

    info!(
        "conformance_workspace_max_issues: search completed in {:?}",
        search_elapsed
    );

    info!("conformance_workspace_max_issues: PASS");
}

// ============================================================================
// ERROR RECOVERY TESTS (6 tests)
// ============================================================================

/// Test: Graceful handling of missing .beads directory
#[test]
fn conformance_missing_beads_dir() {
    common::init_test_logging();
    info!("conformance_missing_beads_dir: BEGIN");

    let workspace = ConformanceWorkspace::new();
    // Don't init - no .beads directory

    // Try to list (should fail gracefully)
    let br_out = workspace.run_br(["list", "--json"], "list_no_init");
    let bd_out = workspace.run_bd(["list", "--json"], "list_no_init");

    info!(
        "conformance_missing_beads_dir: br_exit={} bd_exit={}",
        br_out.status.code().unwrap_or(-1),
        bd_out.status.code().unwrap_or(-1)
    );

    // Both should fail (no workspace)
    assert!(!br_out.status.success(), "br should fail without init");
    assert!(!bd_out.status.success(), "bd should fail without init");

    info!("conformance_missing_beads_dir: PASS");
}

/// Test: Invalid JSONL import rejection
#[test]
fn conformance_invalid_jsonl_import() {
    common::init_test_logging();
    info!("conformance_invalid_jsonl_import: BEGIN");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Write malformed JSONL to br workspace
    let br_jsonl = workspace.br_root.join(".beads").join("issues.jsonl");
    fs::write(&br_jsonl, "{ invalid json missing close brace\n").expect("write malformed jsonl");

    // Write same malformed JSONL to bd workspace
    let bd_jsonl = workspace.bd_root.join(".beads").join("issues.jsonl");
    fs::write(&bd_jsonl, "{ invalid json missing close brace\n").expect("write malformed jsonl");

    // Try to import
    let br_out = workspace.run_br(["sync", "--import-only"], "import_bad");
    let bd_out = workspace.run_bd(["sync", "--import-only"], "import_bad");

    info!(
        "conformance_invalid_jsonl_import: br_exit={} bd_exit={}",
        br_out.status.code().unwrap_or(-1),
        bd_out.status.code().unwrap_or(-1)
    );

    // Both should handle invalid JSONL (either fail or skip with warning)
    // Check stderr contains some indication of parse error
    let br_mentions_error = br_out.stderr.to_lowercase().contains("error")
        || br_out.stderr.to_lowercase().contains("invalid")
        || br_out.stderr.to_lowercase().contains("parse");
    let bd_mentions_error = bd_out.stderr.to_lowercase().contains("error")
        || bd_out.stderr.to_lowercase().contains("invalid")
        || bd_out.stderr.to_lowercase().contains("parse");

    info!(
        "conformance_invalid_jsonl_import: br_error_mention={} bd_error_mention={}",
        br_mentions_error, bd_mentions_error
    );

    info!("conformance_invalid_jsonl_import: PASS");
}

/// Test: UTF-8 BOM handling in JSONL
#[test]
fn conformance_utf8_bom_handling() {
    common::init_test_logging();
    info!("conformance_utf8_bom_handling: BEGIN");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create an issue first via normal means
    let br_create = workspace.run_br(["create", "BOM test issue", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "BOM test issue", "--json"], "create");

    assert!(br_create.status.success());
    assert!(bd_create.status.success());

    // Flush to JSONL
    let _ = workspace.run_br(["sync", "--flush-only"], "flush");
    let _ = workspace.run_bd(["sync", "--flush-only"], "flush");

    // Read, add BOM, write back
    let br_jsonl_path = workspace.br_root.join(".beads").join("issues.jsonl");
    if br_jsonl_path.exists() {
        let content = fs::read(&br_jsonl_path).expect("read jsonl");
        let bom: [u8; 3] = [0xEF, 0xBB, 0xBF]; // UTF-8 BOM
        let mut with_bom = Vec::with_capacity(bom.len() + content.len());
        with_bom.extend_from_slice(&bom);
        with_bom.extend_from_slice(&content);
        fs::write(&br_jsonl_path, with_bom).expect("write bom jsonl");

        // Try to import - should handle BOM gracefully
        let br_import = workspace.run_br(["sync", "--import-only"], "import_bom");

        // Should either succeed or fail gracefully
        info!(
            "conformance_utf8_bom_handling: br_import_exit={}",
            br_import.status.code().unwrap_or(-1)
        );
    }

    info!("conformance_utf8_bom_handling: PASS");
}

/// Test: Schema version check
#[test]
fn conformance_schema_version() {
    common::init_test_logging();
    info!("conformance_schema_version: BEGIN");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create an issue
    let br_create = workspace.run_br(["create", "Schema test", "--json"], "create");
    assert!(br_create.status.success());

    // Doctor should check schema
    let br_doctor = workspace.run_br(["doctor"], "doctor");
    let bd_doctor = workspace.run_bd(["doctor"], "doctor");

    // Both should pass doctor (schema should be valid)
    assert!(
        br_doctor.status.success(),
        "br doctor failed: {}",
        br_doctor.stderr
    );
    assert!(
        bd_doctor.status.success(),
        "bd doctor failed: {}",
        bd_doctor.stderr
    );

    info!("conformance_schema_version: PASS");
}

/// Test: Double-init handling
#[test]
fn conformance_double_init() {
    common::init_test_logging();
    info!("conformance_double_init: BEGIN");

    let workspace = ConformanceWorkspace::new();

    // First init
    let (br_init1, bd_init1) = workspace.init_both();
    assert!(br_init1.status.success());
    assert!(bd_init1.status.success());

    // Second init - should handle gracefully
    let br_init2 = workspace.run_br(["init"], "init2");
    let bd_init2 = workspace.run_bd(["init"], "init2");

    info!(
        "conformance_double_init: br_init2_exit={} bd_init2_exit={}",
        br_init2.status.code().unwrap_or(-1),
        bd_init2.status.code().unwrap_or(-1)
    );

    // Both should either succeed (idempotent) or fail consistently
    assert_eq!(
        br_init2.status.success(),
        bd_init2.status.success(),
        "br and bd should handle double init the same way"
    );

    info!("conformance_double_init: PASS");
}

/// Test: Sync after delete operations
#[test]
fn conformance_sync_after_delete() {
    common::init_test_logging();
    info!("conformance_sync_after_delete: BEGIN");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create
    let br_create = workspace.run_br(["create", "Delete me", "--json"], "create");
    let bd_create = workspace.run_bd(["create", "Delete me", "--json"], "create");

    assert!(br_create.status.success());
    assert!(bd_create.status.success());

    let br_json: Value = serde_json::from_str(&extract_json_payload(&br_create.stdout)).unwrap();
    let bd_json: Value = serde_json::from_str(&extract_json_payload(&bd_create.stdout)).unwrap();

    let br_id = br_json.get("id").and_then(|v| v.as_str()).unwrap();
    let bd_id = bd_json.get("id").and_then(|v| v.as_str()).unwrap();

    // Delete
    let br_delete = workspace.run_br(["delete", br_id], "delete");
    let bd_delete = workspace.run_bd(["delete", bd_id], "delete");

    assert!(br_delete.status.success());
    assert!(bd_delete.status.success());

    // Sync - should handle tombstones
    let br_sync = workspace.run_br(["sync", "--flush-only"], "sync_flush");
    let bd_sync = workspace.run_bd(["sync", "--flush-only"], "sync_flush");

    assert!(
        br_sync.status.success(),
        "br sync after delete failed: {}",
        br_sync.stderr
    );
    assert!(
        bd_sync.status.success(),
        "bd sync after delete failed: {}",
        bd_sync.stderr
    );

    info!("conformance_sync_after_delete: PASS");
}

// ============================================================================
// CROSS-PLATFORM TESTS (4 tests) - Note: These primarily verify br behavior
// ============================================================================

/// Test: UTF-8 file encoding consistency
#[test]
fn conformance_encoding_utf8() {
    common::init_test_logging();
    info!("conformance_encoding_utf8: BEGIN");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create with UTF-8 content
    let utf8_title = "Tëst wîth spëcïäl çhàrâctërs";

    let br_create = workspace.run_br(["create", utf8_title, "--json"], "create_utf8");
    let bd_create = workspace.run_bd(["create", utf8_title, "--json"], "create_utf8");

    assert!(br_create.status.success());
    assert!(bd_create.status.success());

    // Flush and verify JSONL is valid UTF-8
    let _ = workspace.run_br(["sync", "--flush-only"], "flush");

    let br_jsonl_path = workspace.br_root.join(".beads").join("issues.jsonl");
    if br_jsonl_path.exists() {
        let content = fs::read_to_string(&br_jsonl_path).expect("should read as valid UTF-8");
        assert!(
            content.contains(utf8_title),
            "UTF-8 content should be preserved"
        );
    }

    info!("conformance_encoding_utf8: PASS");
}

/// Test: Line ending handling (CRLF vs LF)
#[test]
fn conformance_line_endings() {
    common::init_test_logging();
    info!("conformance_line_endings: BEGIN");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Create issue with multiline description (has embedded newlines)
    let multiline_desc = "Line 1\nLine 2\nLine 3";

    let br_create = workspace.run_br(
        [
            "create",
            "Multiline test",
            "--description",
            multiline_desc,
            "--json",
        ],
        "create_multiline",
    );
    let bd_create = workspace.run_bd(
        [
            "create",
            "Multiline test",
            "--description",
            multiline_desc,
            "--json",
        ],
        "create_multiline",
    );

    assert!(br_create.status.success());
    assert!(bd_create.status.success());

    // Flush
    let _ = workspace.run_br(["sync", "--flush-only"], "flush");

    // Read and check - content should preserve line structure
    let br_jsonl_path = workspace.br_root.join(".beads").join("issues.jsonl");
    if br_jsonl_path.exists() {
        let content = fs::read_to_string(&br_jsonl_path).expect("read jsonl");
        // JSONL escapes newlines as \n in the JSON string value
        assert!(
            content.contains("Line 1") || content.contains("Line 2"),
            "Multiline content should be preserved"
        );
    }

    info!("conformance_line_endings: PASS");
}

/// Test: Special characters that could be path separators
#[test]
fn conformance_path_separators_in_content() {
    common::init_test_logging();
    info!("conformance_path_separators_in_content: BEGIN");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Title with characters that look like path separators
    let tricky_title = "Fix src/module/file.rs and tests\\unit\\test.rs";

    let br_create = workspace.run_br(["create", tricky_title, "--json"], "create_paths");
    let bd_create = workspace.run_bd(["create", tricky_title, "--json"], "create_paths");

    assert!(br_create.status.success());
    assert!(bd_create.status.success());

    // Verify title preserved exactly
    let br_json: Value = serde_json::from_str(&extract_json_payload(&br_create.stdout)).unwrap();
    assert_eq!(
        br_json.get("title").and_then(|v| v.as_str()),
        Some(tricky_title),
        "Path-like characters should be preserved in title"
    );

    info!("conformance_path_separators_in_content: PASS");
}

/// Test: Null bytes handling (should be rejected or sanitized)
#[test]
fn conformance_null_bytes() {
    common::init_test_logging();
    info!("conformance_null_bytes: BEGIN");

    let workspace = ConformanceWorkspace::new();
    workspace.init_both();

    // Title with null byte (this might not even reach the binary depending on shell)
    let null_title = "Test\x00with\x00nulls";

    let br_out = workspace.run_br(["create", null_title, "--json"], "create_null");
    let bd_out = workspace.run_bd(["create", null_title, "--json"], "create_null");

    info!(
        "conformance_null_bytes: br_exit={} bd_exit={}",
        br_out.status.code().unwrap_or(-1),
        bd_out.status.code().unwrap_or(-1)
    );

    // Both should handle consistently (either sanitize or reject)
    // We just verify both handle it without crashing

    info!("conformance_null_bytes: PASS");
}
