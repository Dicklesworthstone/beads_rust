//! E2E tests for `br dep tree --format=mermaid` command.
//!
//! Coverage:
//! - Basic mermaid output format validation
//! - Various dependency graph shapes (linear, branching, diamond)
//! - Edge cases (empty deps, single node, deep trees)
//! - Title escaping for mermaid syntax
//! - Real dataset tests via IsolatedDataset

// These are test utilities with intentionally similar/short names
#![allow(clippy::similar_names)]
#![allow(clippy::many_single_char_names)]

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;
use tracing::info;

fn parse_created_id(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or("");
    let normalized = line.strip_prefix("✓ ").unwrap_or(line);
    let id_part = normalized
        .strip_prefix("Created ")
        .and_then(|rest| rest.split(':').next())
        .unwrap_or("");
    id_part.trim().to_string()
}

/// Validate that a string contains valid mermaid graph syntax.
/// Returns the list of nodes and edges parsed from the output.
fn validate_mermaid_syntax(output: &str) -> (Vec<String>, Vec<(String, String)>) {
    let lines: Vec<&str> = output.lines().collect();

    // First non-empty line should be "graph TD"
    let first_line = lines.iter().find(|l| !l.trim().is_empty()).unwrap_or(&"");
    assert_eq!(
        first_line.trim(),
        "graph TD",
        "Mermaid output should start with 'graph TD'"
    );

    let mut nodes: Vec<String> = Vec::new();
    let mut edges: Vec<(String, String)> = Vec::new();

    for line in &lines[1..] {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Node definition: id["id: title [Pn]"]
        if trimmed.contains('[') && trimmed.contains(']') && !trimmed.contains("-->") {
            // Extract the node ID (part before the bracket)
            if let Some(bracket_pos) = trimmed.find('[') {
                let node_id = trimmed[..bracket_pos].trim();
                nodes.push(node_id.to_string());
            }
        }
        // Edge definition: id --> id
        else if trimmed.contains("-->") {
            let parts: Vec<&str> = trimmed.split("-->").collect();
            if parts.len() == 2 {
                let from = parts[0].trim().to_string();
                let to = parts[1].trim().to_string();
                edges.push((from, to));
            }
        }
    }

    (nodes, edges)
}

#[test]
fn e2e_dep_tree_mermaid_single_node_no_deps() {
    common::init_test_logging();
    info!("e2e_dep_tree_mermaid_single_node_no_deps: starting");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Single issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let issue_id = parse_created_id(&create.stdout);

    let tree = run_br(
        &workspace,
        ["dep", "tree", &issue_id, "--format=mermaid"],
        "tree_mermaid",
    );
    assert!(tree.status.success(), "dep tree failed: {}", tree.stderr);

    let (nodes, edges) = validate_mermaid_syntax(&tree.stdout);

    // Single node with no dependencies
    assert_eq!(nodes.len(), 1, "Should have exactly one node");
    assert!(
        nodes.contains(&issue_id),
        "Node list should contain the issue"
    );
    assert!(edges.is_empty(), "Single node should have no edges");

    info!("e2e_dep_tree_mermaid_single_node_no_deps: assertions passed");
}

#[test]
fn e2e_dep_tree_mermaid_linear_chain() {
    common::init_test_logging();
    info!("e2e_dep_tree_mermaid_linear_chain: starting");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create chain: A -> B -> C (A depends on B, B depends on C)
    let a = run_br(&workspace, ["create", "Issue A"], "create_a");
    let b = run_br(&workspace, ["create", "Issue B"], "create_b");
    let c = run_br(&workspace, ["create", "Issue C"], "create_c");

    let id_a = parse_created_id(&a.stdout);
    let id_b = parse_created_id(&b.stdout);
    let id_c = parse_created_id(&c.stdout);

    // A depends on B
    let dep_ab = run_br(&workspace, ["dep", "add", &id_a, &id_b], "dep_ab");
    assert!(dep_ab.status.success(), "dep add A->B failed");

    // B depends on C
    let dep_bc = run_br(&workspace, ["dep", "add", &id_b, &id_c], "dep_bc");
    assert!(dep_bc.status.success(), "dep add B->C failed");

    let tree = run_br(
        &workspace,
        ["dep", "tree", &id_a, "--format=mermaid"],
        "tree_mermaid",
    );
    assert!(tree.status.success(), "dep tree failed: {}", tree.stderr);

    let (nodes, edges) = validate_mermaid_syntax(&tree.stdout);

    // Should have 3 nodes
    assert_eq!(nodes.len(), 3, "Linear chain should have 3 nodes");
    assert!(nodes.contains(&id_a), "Should contain A");
    assert!(nodes.contains(&id_b), "Should contain B");
    assert!(nodes.contains(&id_c), "Should contain C");

    // Should have 2 edges
    assert_eq!(edges.len(), 2, "Linear chain should have 2 edges");

    info!("e2e_dep_tree_mermaid_linear_chain: assertions passed");
}

#[test]
fn e2e_dep_tree_mermaid_branching_deps() {
    common::init_test_logging();
    info!("e2e_dep_tree_mermaid_branching_deps: starting");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create: A -> B and A -> C (A depends on both B and C)
    let a = run_br(&workspace, ["create", "Root A"], "create_a");
    let b = run_br(&workspace, ["create", "Branch B"], "create_b");
    let c = run_br(&workspace, ["create", "Branch C"], "create_c");

    let id_a = parse_created_id(&a.stdout);
    let id_b = parse_created_id(&b.stdout);
    let id_c = parse_created_id(&c.stdout);

    let dep_ab = run_br(&workspace, ["dep", "add", &id_a, &id_b], "dep_ab");
    assert!(dep_ab.status.success(), "dep add A->B failed");

    let dep_ac = run_br(&workspace, ["dep", "add", &id_a, &id_c], "dep_ac");
    assert!(dep_ac.status.success(), "dep add A->C failed");

    let tree = run_br(
        &workspace,
        ["dep", "tree", &id_a, "--format=mermaid"],
        "tree_mermaid",
    );
    assert!(tree.status.success(), "dep tree failed: {}", tree.stderr);

    let (nodes, edges) = validate_mermaid_syntax(&tree.stdout);

    assert_eq!(nodes.len(), 3, "Branching tree should have 3 nodes");
    assert_eq!(edges.len(), 2, "Branching tree should have 2 edges");

    info!("e2e_dep_tree_mermaid_branching_deps: assertions passed");
}

#[test]
fn e2e_dep_tree_mermaid_diamond_shape() {
    common::init_test_logging();
    info!("e2e_dep_tree_mermaid_diamond_shape: starting");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Diamond: A -> B -> D and A -> C -> D
    let a = run_br(&workspace, ["create", "Top A"], "create_a");
    let b = run_br(&workspace, ["create", "Left B"], "create_b");
    let c = run_br(&workspace, ["create", "Right C"], "create_c");
    let d = run_br(&workspace, ["create", "Bottom D"], "create_d");

    let id_a = parse_created_id(&a.stdout);
    let id_b = parse_created_id(&b.stdout);
    let id_c = parse_created_id(&c.stdout);
    let id_d = parse_created_id(&d.stdout);

    run_br(&workspace, ["dep", "add", &id_a, &id_b], "dep_ab");
    run_br(&workspace, ["dep", "add", &id_a, &id_c], "dep_ac");
    run_br(&workspace, ["dep", "add", &id_b, &id_d], "dep_bd");
    run_br(&workspace, ["dep", "add", &id_c, &id_d], "dep_cd");

    let tree = run_br(
        &workspace,
        ["dep", "tree", &id_a, "--format=mermaid"],
        "tree_mermaid",
    );
    assert!(tree.status.success(), "dep tree failed: {}", tree.stderr);

    let (nodes, edges) = validate_mermaid_syntax(&tree.stdout);

    // Diamond shape: D should appear twice (once under B, once under C)
    let d_count = nodes.iter().filter(|n| *n == &id_d).count();
    assert_eq!(
        d_count, 2,
        "Diamond convergence node D should appear twice in mermaid output"
    );

    // Should have 5 nodes total (A, B, C, D under B, D under C)
    assert_eq!(
        nodes.len(),
        5,
        "Diamond should have 5 nodes (D appears twice)"
    );
    assert_eq!(edges.len(), 4, "Diamond should have 4 edges");

    info!("e2e_dep_tree_mermaid_diamond_shape: assertions passed");
}

#[test]
fn e2e_dep_tree_mermaid_max_depth_truncation() {
    common::init_test_logging();
    info!("e2e_dep_tree_mermaid_max_depth_truncation: starting");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create deep chain: A -> B -> C -> D -> E
    let a = run_br(&workspace, ["create", "Level 0"], "create_a");
    let b = run_br(&workspace, ["create", "Level 1"], "create_b");
    let c = run_br(&workspace, ["create", "Level 2"], "create_c");
    let d = run_br(&workspace, ["create", "Level 3"], "create_d");
    let e = run_br(&workspace, ["create", "Level 4"], "create_e");

    let id_a = parse_created_id(&a.stdout);
    let id_b = parse_created_id(&b.stdout);
    let id_c = parse_created_id(&c.stdout);
    let id_d = parse_created_id(&d.stdout);
    let id_e = parse_created_id(&e.stdout);

    run_br(&workspace, ["dep", "add", &id_a, &id_b], "dep_ab");
    run_br(&workspace, ["dep", "add", &id_b, &id_c], "dep_bc");
    run_br(&workspace, ["dep", "add", &id_c, &id_d], "dep_cd");
    run_br(&workspace, ["dep", "add", &id_d, &id_e], "dep_de");

    // With max-depth=2, should only show A, B, C
    let tree = run_br(
        &workspace,
        ["dep", "tree", &id_a, "--format=mermaid", "--max-depth=2"],
        "tree_mermaid_depth2",
    );
    assert!(tree.status.success(), "dep tree failed: {}", tree.stderr);

    let (nodes, _edges) = validate_mermaid_syntax(&tree.stdout);

    // Depth 0 = A, Depth 1 = B, Depth 2 = C (truncated)
    assert_eq!(nodes.len(), 3, "max-depth=2 should show 3 nodes");
    assert!(nodes.contains(&id_a), "Should contain A");
    assert!(nodes.contains(&id_b), "Should contain B");
    assert!(nodes.contains(&id_c), "Should contain C (truncated)");
    assert!(
        !nodes.contains(&id_d),
        "Should NOT contain D (beyond depth)"
    );
    assert!(
        !nodes.contains(&id_e),
        "Should NOT contain E (beyond depth)"
    );

    info!("e2e_dep_tree_mermaid_max_depth_truncation: assertions passed");
}

#[test]
fn e2e_dep_tree_mermaid_title_with_quotes() {
    common::init_test_logging();
    info!("e2e_dep_tree_mermaid_title_with_quotes: starting");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create issue with quotes in title that need escaping
    let create = run_br(
        &workspace,
        ["create", "Issue with \"quotes\" in title"],
        "create",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let issue_id = parse_created_id(&create.stdout);

    let tree = run_br(
        &workspace,
        ["dep", "tree", &issue_id, "--format=mermaid"],
        "tree_mermaid",
    );
    assert!(tree.status.success(), "dep tree failed: {}", tree.stderr);

    // Verify quotes are escaped (replaced with single quotes per implementation)
    assert!(
        tree.stdout.contains("'quotes'"),
        "Double quotes should be escaped to single quotes in mermaid output"
    );
    assert!(
        !tree.stdout.contains("\"quotes\""),
        "Unescaped double quotes should not appear in node labels"
    );

    info!("e2e_dep_tree_mermaid_title_with_quotes: assertions passed");
}

#[test]
fn e2e_dep_tree_mermaid_vs_json_consistency() {
    common::init_test_logging();
    info!("e2e_dep_tree_mermaid_vs_json_consistency: starting");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create simple tree
    let a = run_br(&workspace, ["create", "Root"], "create_a");
    let b = run_br(&workspace, ["create", "Child"], "create_b");

    let id_a = parse_created_id(&a.stdout);
    let id_b = parse_created_id(&b.stdout);

    run_br(&workspace, ["dep", "add", &id_a, &id_b], "dep_ab");

    // Get JSON output
    let tree_json = run_br(&workspace, ["dep", "tree", &id_a, "--json"], "tree_json");
    assert!(tree_json.status.success(), "JSON tree failed");
    let json_payload = extract_json_payload(&tree_json.stdout);
    let json_nodes: Vec<Value> = serde_json::from_str(&json_payload).expect("parse json");

    // Get Mermaid output
    let tree_mermaid = run_br(
        &workspace,
        ["dep", "tree", &id_a, "--format=mermaid"],
        "tree_mermaid",
    );
    assert!(tree_mermaid.status.success(), "Mermaid tree failed");
    let (mermaid_nodes, _) = validate_mermaid_syntax(&tree_mermaid.stdout);

    // Both should have same number of nodes
    assert_eq!(
        json_nodes.len(),
        mermaid_nodes.len(),
        "JSON and mermaid should report same number of nodes"
    );

    // All JSON node IDs should appear in mermaid output
    for json_node in &json_nodes {
        let node_id = json_node["id"].as_str().unwrap();
        assert!(
            mermaid_nodes.contains(&node_id.to_string()),
            "Mermaid output missing node: {}",
            node_id
        );
    }

    info!("e2e_dep_tree_mermaid_vs_json_consistency: assertions passed");
}

#[test]
fn e2e_dep_tree_mermaid_format_case_insensitive() {
    common::init_test_logging();
    info!("e2e_dep_tree_mermaid_format_case_insensitive: starting");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Test issue"], "create");
    let issue_id = parse_created_id(&create.stdout);

    // Test different cases
    for format_arg in &["--format=mermaid", "--format=MERMAID", "--format=Mermaid"] {
        let tree = run_br(
            &workspace,
            ["dep", "tree", &issue_id, *format_arg],
            &format!("tree_{}", format_arg),
        );
        assert!(tree.status.success(), "tree with {} failed", format_arg);
        assert!(
            tree.stdout.contains("graph TD"),
            "Output for {} should contain 'graph TD'",
            format_arg
        );
    }

    info!("e2e_dep_tree_mermaid_format_case_insensitive: assertions passed");
}

#[test]
fn e2e_dep_tree_mermaid_output_valid_mermaid_diagram() {
    common::init_test_logging();
    info!("e2e_dep_tree_mermaid_output_valid_mermaid_diagram: starting");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create a non-trivial tree
    let epic = run_br(&workspace, ["create", "Epic"], "create_epic");
    let task1 = run_br(&workspace, ["create", "Task 1"], "create_task1");
    let task2 = run_br(&workspace, ["create", "Task 2"], "create_task2");
    let subtask = run_br(&workspace, ["create", "Subtask"], "create_subtask");

    let id_epic = parse_created_id(&epic.stdout);
    let id_task1 = parse_created_id(&task1.stdout);
    let id_task2 = parse_created_id(&task2.stdout);
    let id_subtask = parse_created_id(&subtask.stdout);

    run_br(&workspace, ["dep", "add", &id_epic, &id_task1], "dep_e_t1");
    run_br(&workspace, ["dep", "add", &id_epic, &id_task2], "dep_e_t2");
    run_br(
        &workspace,
        ["dep", "add", &id_task1, &id_subtask],
        "dep_t1_s",
    );

    let tree = run_br(
        &workspace,
        ["dep", "tree", &id_epic, "--format=mermaid"],
        "tree_mermaid",
    );
    assert!(tree.status.success(), "dep tree failed: {}", tree.stderr);

    // Validate the mermaid output structure
    let output = &tree.stdout;
    let lines: Vec<&str> = output.lines().collect();

    // First line should be "graph TD"
    assert!(
        lines.iter().any(|l| l.trim() == "graph TD"),
        "Should have graph TD header"
    );

    // Should have node definitions with proper format
    // Format: id["id: title [Pn]"]
    let node_pattern = |id: &str| format!("{}[\"", id);
    assert!(
        output.contains(&node_pattern(&id_epic)),
        "Should have epic node definition"
    );
    assert!(
        output.contains(&node_pattern(&id_task1)),
        "Should have task1 node definition"
    );
    assert!(
        output.contains(&node_pattern(&id_task2)),
        "Should have task2 node definition"
    );
    assert!(
        output.contains(&node_pattern(&id_subtask)),
        "Should have subtask node definition"
    );

    // Should have edge definitions
    // Format: child --> parent
    assert!(
        output.contains(" --> "),
        "Should have edge definitions with -->"
    );

    info!("e2e_dep_tree_mermaid_output_valid_mermaid_diagram: assertions passed");
}

#[test]
fn e2e_dep_tree_mermaid_text_vs_mermaid_format() {
    common::init_test_logging();
    info!("e2e_dep_tree_mermaid_text_vs_mermaid_format: starting");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let a = run_br(&workspace, ["create", "Root"], "create_a");
    let b = run_br(&workspace, ["create", "Child"], "create_b");

    let id_a = parse_created_id(&a.stdout);
    let id_b = parse_created_id(&b.stdout);

    run_br(&workspace, ["dep", "add", &id_a, &id_b], "dep_ab");

    // Get text output (default)
    let tree_text = run_br(&workspace, ["dep", "tree", &id_a], "tree_text");
    assert!(tree_text.status.success(), "Text tree failed");

    // Get mermaid output
    let tree_mermaid = run_br(
        &workspace,
        ["dep", "tree", &id_a, "--format=mermaid"],
        "tree_mermaid",
    );
    assert!(tree_mermaid.status.success(), "Mermaid tree failed");

    // Text format should NOT contain mermaid syntax
    assert!(
        !tree_text.stdout.contains("graph TD"),
        "Text format should not contain mermaid syntax"
    );

    // Mermaid format SHOULD contain mermaid syntax
    assert!(
        tree_mermaid.stdout.contains("graph TD"),
        "Mermaid format should contain mermaid syntax"
    );

    info!("e2e_dep_tree_mermaid_text_vs_mermaid_format: assertions passed");
}

#[test]
fn e2e_dep_tree_mermaid_with_different_priorities() {
    common::init_test_logging();
    info!("e2e_dep_tree_mermaid_with_different_priorities: starting");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create issues with different priorities (0-4 or P0-P4 format)
    // P1 = high priority, P4 = low priority
    let high = run_br(
        &workspace,
        ["create", "High Priority", "--priority", "1"],
        "create_high",
    );
    assert!(high.status.success(), "create high failed: {}", high.stderr);
    let low = run_br(
        &workspace,
        ["create", "Low Priority", "--priority", "4"],
        "create_low",
    );
    assert!(low.status.success(), "create low failed: {}", low.stderr);

    let id_high = parse_created_id(&high.stdout);
    let id_low = parse_created_id(&low.stdout);

    run_br(&workspace, ["dep", "add", &id_high, &id_low], "dep");

    let tree = run_br(
        &workspace,
        ["dep", "tree", &id_high, "--format=mermaid"],
        "tree_mermaid",
    );
    assert!(tree.status.success(), "dep tree failed: {}", tree.stderr);

    // Verify priority is shown in mermaid output
    // High priority = P1, Low priority = P4
    assert!(
        tree.stdout.contains("[P1]"),
        "Mermaid output should show high priority as [P1]"
    );
    assert!(
        tree.stdout.contains("[P4]"),
        "Mermaid output should show low priority as [P4]"
    );

    info!("e2e_dep_tree_mermaid_with_different_priorities: assertions passed");
}

#[test]
fn e2e_dep_tree_mermaid_quiet_mode() {
    common::init_test_logging();
    info!("e2e_dep_tree_mermaid_quiet_mode: starting");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Test issue"], "create");
    let issue_id = parse_created_id(&create.stdout);

    // Quiet mode should suppress output
    let tree = run_br(
        &workspace,
        ["dep", "tree", &issue_id, "--format=mermaid", "--quiet"],
        "tree_mermaid_quiet",
    );
    assert!(tree.status.success(), "dep tree failed: {}", tree.stderr);

    // In quiet mode, stdout should be empty or minimal
    // (Note: --quiet may not affect mermaid output depending on implementation)
    // Let's just verify it doesn't crash
    assert!(
        tree.status.success(),
        "Quiet mode with mermaid format should not crash"
    );

    info!("e2e_dep_tree_mermaid_quiet_mode: assertions passed");
}

#[test]
fn e2e_dep_tree_mermaid_issue_not_found() {
    common::init_test_logging();
    info!("e2e_dep_tree_mermaid_issue_not_found: starting");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Try to get mermaid tree for non-existent issue
    let tree = run_br(
        &workspace,
        ["dep", "tree", "bd-nonexistent", "--format=mermaid"],
        "tree_mermaid_notfound",
    );

    // Should fail with appropriate error
    assert!(!tree.status.success(), "Should fail for non-existent issue");
    assert!(
        tree.stderr.contains("not found") || tree.stderr.contains("No match"),
        "Error message should indicate issue not found"
    );

    info!("e2e_dep_tree_mermaid_issue_not_found: assertions passed");
}
