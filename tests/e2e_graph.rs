#![allow(clippy::similar_names)]

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

#[test]
fn e2e_graph_single_issue_no_dependents() {
    let _log = common::test_log("e2e_graph_single_issue_no_dependents");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let issue = run_br(&workspace, ["create", "Standalone issue"], "create_issue");
    assert!(issue.status.success(), "create failed: {}", issue.stderr);
    let issue_id = parse_created_id(&issue.stdout);

    let graph = run_br(&workspace, ["graph", &issue_id], "graph_single");
    assert!(graph.status.success(), "graph failed: {}", graph.stderr);
    assert!(
        graph.stdout.contains("No dependents"),
        "Expected 'No dependents' message, got: {}",
        graph.stdout
    );
}

#[test]
fn e2e_graph_single_issue_with_dependents() {
    let _log = common::test_log("e2e_graph_single_issue_with_dependents");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create blocking issue (root)
    let blocker = run_br(&workspace, ["create", "Blocker issue"], "create_blocker");
    assert!(
        blocker.status.success(),
        "blocker create failed: {}",
        blocker.stderr
    );
    let blocker_id = parse_created_id(&blocker.stdout);

    // Create blocked issue (dependent)
    let blocked = run_br(&workspace, ["create", "Blocked issue"], "create_blocked");
    assert!(
        blocked.status.success(),
        "blocked create failed: {}",
        blocked.stderr
    );
    let blocked_id = parse_created_id(&blocked.stdout);

    // Add dependency: blocked depends on blocker
    let dep_add = run_br(
        &workspace,
        ["dep", "add", &blocked_id, &blocker_id],
        "dep_add",
    );
    assert!(
        dep_add.status.success(),
        "dep add failed: {}",
        dep_add.stderr
    );

    // Graph blocker - should show blocked as dependent
    let graph = run_br(&workspace, ["graph", &blocker_id], "graph_blocker");
    assert!(graph.status.success(), "graph failed: {}", graph.stderr);
    assert!(
        graph.stdout.contains("Dependents of"),
        "Expected 'Dependents of' message, got: {}",
        graph.stdout
    );
    assert!(
        graph.stdout.contains(&blocked_id),
        "Expected dependent issue ID in output, got: {}",
        graph.stdout
    );
}

#[test]
fn e2e_graph_single_issue_json() {
    let _log = common::test_log("e2e_graph_single_issue_json");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let blocker = run_br(&workspace, ["create", "Blocker"], "create_blocker");
    assert!(
        blocker.status.success(),
        "blocker create failed: {}",
        blocker.stderr
    );
    let blocker_id = parse_created_id(&blocker.stdout);

    let blocked = run_br(&workspace, ["create", "Blocked"], "create_blocked");
    assert!(
        blocked.status.success(),
        "blocked create failed: {}",
        blocked.stderr
    );
    let blocked_id = parse_created_id(&blocked.stdout);

    let dep_add = run_br(
        &workspace,
        ["dep", "add", &blocked_id, &blocker_id],
        "dep_add",
    );
    assert!(
        dep_add.status.success(),
        "dep add failed: {}",
        dep_add.stderr
    );

    let graph = run_br(&workspace, ["graph", &blocker_id, "--json"], "graph_json");
    assert!(graph.status.success(), "graph failed: {}", graph.stderr);

    let payload = extract_json_payload(&graph.stdout);
    let json: Value = serde_json::from_str(&payload).expect("graph json");

    assert_eq!(json["root"], blocker_id, "root should be blocker id");
    assert_eq!(json["count"], 2, "count should be 2 (root + dependent)");

    let nodes = json["nodes"].as_array().expect("nodes array");
    assert_eq!(nodes.len(), 2, "should have 2 nodes");

    let edges = json["edges"].as_array().expect("edges array");
    assert_eq!(edges.len(), 1, "should have 1 edge");
    assert_eq!(edges[0][0], blocked_id, "edge from should be blocked");
    assert_eq!(edges[0][1], blocker_id, "edge to should be blocker");
}

#[test]
fn e2e_graph_single_issue_compact() {
    let _log = common::test_log("e2e_graph_single_issue_compact");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let blocker = run_br(&workspace, ["create", "Blocker"], "create_blocker");
    assert!(
        blocker.status.success(),
        "blocker create failed: {}",
        blocker.stderr
    );
    let blocker_id = parse_created_id(&blocker.stdout);

    let blocked = run_br(&workspace, ["create", "Blocked"], "create_blocked");
    assert!(
        blocked.status.success(),
        "blocked create failed: {}",
        blocked.stderr
    );
    let blocked_id = parse_created_id(&blocked.stdout);

    let dep_add = run_br(
        &workspace,
        ["dep", "add", &blocked_id, &blocker_id],
        "dep_add",
    );
    assert!(
        dep_add.status.success(),
        "dep add failed: {}",
        dep_add.stderr
    );

    let graph = run_br(
        &workspace,
        ["graph", &blocker_id, "--compact"],
        "graph_compact",
    );
    assert!(graph.status.success(), "graph failed: {}", graph.stderr);

    // Compact format: root <- dependent
    assert!(
        graph.stdout.contains(&format!("{blocker_id} <-")),
        "Expected compact format with root, got: {}",
        graph.stdout
    );
    assert!(
        graph.stdout.contains(&blocked_id),
        "Expected dependent in compact output, got: {}",
        graph.stdout
    );
}

#[test]
fn e2e_graph_all_no_issues() {
    let _log = common::test_log("e2e_graph_all_no_issues");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let graph = run_br(&workspace, ["graph", "--all"], "graph_all_empty");
    assert!(graph.status.success(), "graph failed: {}", graph.stderr);
    assert!(
        graph.stdout.contains("No open/in_progress/blocked issues"),
        "Expected 'No issues' message, got: {}",
        graph.stdout
    );
}

#[test]
fn e2e_graph_all_with_connected_components() {
    let _log = common::test_log("e2e_graph_all_with_connected_components");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create first connected component: A -> B
    let issue_a = run_br(&workspace, ["create", "Issue A"], "create_a");
    assert!(
        issue_a.status.success(),
        "create a failed: {}",
        issue_a.stderr
    );
    let id_a = parse_created_id(&issue_a.stdout);

    let issue_b = run_br(&workspace, ["create", "Issue B"], "create_b");
    assert!(
        issue_b.status.success(),
        "create b failed: {}",
        issue_b.stderr
    );
    let id_b = parse_created_id(&issue_b.stdout);

    let dep_ab = run_br(&workspace, ["dep", "add", &id_b, &id_a], "dep_ab");
    assert!(dep_ab.status.success(), "dep add failed: {}", dep_ab.stderr);

    // Create second isolated issue (separate component)
    let issue_c = run_br(&workspace, ["create", "Issue C"], "create_c");
    assert!(
        issue_c.status.success(),
        "create c failed: {}",
        issue_c.stderr
    );
    let id_c = parse_created_id(&issue_c.stdout);

    let graph = run_br(&workspace, ["graph", "--all"], "graph_all");
    assert!(graph.status.success(), "graph failed: {}", graph.stderr);

    // Should show both components
    assert!(
        graph.stdout.contains("2 component"),
        "Expected 2 components, got: {}",
        graph.stdout
    );
    assert!(
        graph.stdout.contains(&id_a) && graph.stdout.contains(&id_b),
        "Expected connected issues in output, got: {}",
        graph.stdout
    );
    assert!(
        graph.stdout.contains(&id_c),
        "Expected isolated issue in output, got: {}",
        graph.stdout
    );
}

#[test]
fn e2e_graph_all_json() {
    let _log = common::test_log("e2e_graph_all_json");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let issue_a = run_br(&workspace, ["create", "Issue A"], "create_a");
    assert!(
        issue_a.status.success(),
        "create a failed: {}",
        issue_a.stderr
    );
    let id_a = parse_created_id(&issue_a.stdout);

    let issue_b = run_br(&workspace, ["create", "Issue B"], "create_b");
    assert!(
        issue_b.status.success(),
        "create b failed: {}",
        issue_b.stderr
    );
    let id_b = parse_created_id(&issue_b.stdout);

    let dep_ab = run_br(&workspace, ["dep", "add", &id_b, &id_a], "dep_ab");
    assert!(dep_ab.status.success(), "dep add failed: {}", dep_ab.stderr);

    let graph = run_br(&workspace, ["graph", "--all", "--json"], "graph_all_json");
    assert!(graph.status.success(), "graph failed: {}", graph.stderr);

    let payload = extract_json_payload(&graph.stdout);
    let json: Value = serde_json::from_str(&payload).expect("graph json");

    assert_eq!(json["total_nodes"], 2, "should have 2 total nodes");
    assert_eq!(
        json["total_components"], 1,
        "should have 1 component (connected)"
    );

    let components = json["components"].as_array().expect("components array");
    assert_eq!(components.len(), 1, "should have 1 component");

    let component = &components[0];
    let nodes = component["nodes"].as_array().expect("nodes array");
    assert_eq!(nodes.len(), 2, "component should have 2 nodes");

    let edges = component["edges"].as_array().expect("edges array");
    assert_eq!(edges.len(), 1, "component should have 1 edge");

    let roots = component["roots"].as_array().expect("roots array");
    assert_eq!(roots.len(), 1, "component should have 1 root");
    assert_eq!(roots[0], id_a, "root should be issue A");
}

#[test]
fn e2e_graph_requires_issue_or_all() {
    let _log = common::test_log("e2e_graph_requires_issue_or_all");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let graph = run_br(&workspace, ["graph"], "graph_no_args");
    assert!(!graph.status.success(), "graph without args should fail");
    assert!(
        graph.stderr.contains("Issue ID required") || graph.stderr.contains("issue"),
        "Expected issue required error, got: {}",
        graph.stderr
    );
}

#[test]
fn e2e_graph_chain_depth() {
    let _log = common::test_log("e2e_graph_chain_depth");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create chain: A -> B -> C (C depends on B, B depends on A)
    let issue_a = run_br(&workspace, ["create", "Root issue A"], "create_a");
    assert!(
        issue_a.status.success(),
        "create a failed: {}",
        issue_a.stderr
    );
    let id_a = parse_created_id(&issue_a.stdout);

    let issue_b = run_br(&workspace, ["create", "Middle issue B"], "create_b");
    assert!(
        issue_b.status.success(),
        "create b failed: {}",
        issue_b.stderr
    );
    let id_b = parse_created_id(&issue_b.stdout);

    let issue_c = run_br(&workspace, ["create", "Leaf issue C"], "create_c");
    assert!(
        issue_c.status.success(),
        "create c failed: {}",
        issue_c.stderr
    );
    let id_c = parse_created_id(&issue_c.stdout);

    // B depends on A
    let dep_ba = run_br(&workspace, ["dep", "add", &id_b, &id_a], "dep_ba");
    assert!(dep_ba.status.success(), "dep add failed: {}", dep_ba.stderr);

    // C depends on B
    let dep_cb = run_br(&workspace, ["dep", "add", &id_c, &id_b], "dep_cb");
    assert!(dep_cb.status.success(), "dep add failed: {}", dep_cb.stderr);

    // Graph from A should show B at depth 1, C at depth 2
    let graph = run_br(&workspace, ["graph", &id_a, "--json"], "graph_chain");
    assert!(graph.status.success(), "graph failed: {}", graph.stderr);

    let payload = extract_json_payload(&graph.stdout);
    let json: Value = serde_json::from_str(&payload).expect("graph json");

    assert_eq!(json["count"], 3, "should have 3 nodes");

    let nodes = json["nodes"].as_array().expect("nodes array");
    let node_a = nodes.iter().find(|n| n["id"] == id_a).expect("node A");
    let node_b = nodes.iter().find(|n| n["id"] == id_b).expect("node B");
    let node_c = nodes.iter().find(|n| n["id"] == id_c).expect("node C");

    assert_eq!(node_a["depth"], 0, "A should be at depth 0");
    assert_eq!(node_b["depth"], 1, "B should be at depth 1");
    assert_eq!(node_c["depth"], 2, "C should be at depth 2");
}

#[test]
fn e2e_graph_all_cross_prefix_flat_rendering() {
    let _log = common::test_log("e2e_graph_all_cross_prefix_flat_rendering");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create issue with explicit prefix 'alpha'
    let a = run_br(
        &workspace,
        ["create", "--prefix", "alpha", "Alpha issue"],
        "create_a",
    );
    assert!(a.status.success(), "create a failed: {}", a.stderr);
    let id_a = parse_created_id(&a.stdout);

    // Create issue with explicit prefix 'beta'
    let b = run_br(
        &workspace,
        ["create", "--prefix", "beta", "Beta issue"],
        "create_b",
    );
    assert!(b.status.success(), "create b failed: {}", b.stderr);
    let id_b = parse_created_id(&b.stdout);

    // Create cross-prefix dep: B depends on A (different prefixes in same component)
    let dep = run_br(&workspace, ["dep", "add", &id_b, &id_a], "dep_add");
    assert!(dep.status.success(), "dep add failed: {}", dep.stderr);

    // Create a standalone issue in beta (separate component)
    let c = run_br(
        &workspace,
        ["create", "--prefix", "beta", "Beta standalone"],
        "create_c",
    );
    assert!(c.status.success(), "create c failed: {}", c.stderr);
    let id_c = parse_created_id(&c.stdout);

    let graph = run_br(&workspace, ["graph", "--all"], "graph_all");
    assert!(graph.status.success(), "graph failed: {}", graph.stderr);

    // Should show 2 components: {A,B} and {C}
    assert!(
        graph.stdout.contains("2 component"),
        "Expected 2 components, got: {}",
        graph.stdout
    );

    // Both cross-prefix issues should appear
    assert!(
        graph.stdout.contains(&id_a) && graph.stdout.contains(&id_b),
        "Expected both alpha and beta issues in output, got: {}",
        graph.stdout
    );
    assert!(
        graph.stdout.contains(&id_c),
        "Expected standalone beta issue in output, got: {}",
        graph.stdout
    );

    // MUST NOT have old prefix-clustered section headers
    assert!(
        !graph.stdout.contains("=== "),
        "Output must not contain prefix-section headers '=== ', got: {}",
        graph.stdout
    );
    assert!(
        !graph.stdout.contains("<cross-prefix>"),
        "Output must not contain <cross-prefix> bucket, got: {}",
        graph.stdout
    );
}

#[test]
fn e2e_graph_all_deferred_excluded_by_default() {
    let _log = common::test_log("e2e_graph_all_deferred_excluded_by_default");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create an open issue
    let open = run_br(&workspace, ["create", "Open issue"], "create_open");
    assert!(open.status.success(), "create open failed: {}", open.stderr);
    let id_open = parse_created_id(&open.stdout);

    // Create a deferred issue
    let deferred = run_br(&workspace, ["create", "Deferred issue"], "create_deferred");
    assert!(
        deferred.status.success(),
        "create deferred failed: {}",
        deferred.stderr
    );
    let id_deferred = parse_created_id(&deferred.stdout);
    let defer_it = run_br(
        &workspace,
        ["update", &id_deferred, "--status", "deferred"],
        "update_deferred",
    );
    assert!(
        defer_it.status.success(),
        "update to deferred failed: {}",
        defer_it.stderr
    );

    // Without --deferred, deferred issue must not appear
    let graph_default = run_br(&workspace, ["graph", "--all"], "graph_default");
    assert!(
        graph_default.status.success(),
        "graph failed: {}",
        graph_default.stderr
    );
    assert!(
        graph_default.stdout.contains(&id_open),
        "Expected open issue in output, got: {}",
        graph_default.stdout
    );
    assert!(
        !graph_default.stdout.contains(&id_deferred),
        "Deferred issue must not appear without --deferred flag, got: {}",
        graph_default.stdout
    );

    // With --deferred, deferred issue must appear with status badge
    let graph_deferred = run_br(
        &workspace,
        ["graph", "--all", "--deferred"],
        "graph_with_deferred",
    );
    assert!(
        graph_deferred.status.success(),
        "graph --deferred failed: {}",
        graph_deferred.stderr
    );
    assert!(
        graph_deferred.stdout.contains(&id_deferred),
        "Deferred issue must appear with --deferred flag, got: {}",
        graph_deferred.stdout
    );
    assert!(
        graph_deferred.stdout.contains("deferred"),
        "Output must mention 'deferred' status, got: {}",
        graph_deferred.stdout
    );
}

/// Mixed: one multi-node component (A→B) plus two singletons (C, D).
/// - numbered `Component 1` present
/// - `Singletons (2 issues)` block present, both singleton IDs listed
/// - NO `Component 2` / `Component 3` headers (singletons consume no numbers)
/// - summary line carries `(2 singletons)`
#[test]
fn e2e_graph_all_singletons_mixed() {
    let _log = common::test_log("e2e_graph_all_singletons_mixed");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Multi-node component: A → B (B depends on A)
    let a = run_br(&workspace, ["create", "Issue A"], "create_a");
    assert!(a.status.success(), "create a: {}", a.stderr);
    let id_a = parse_created_id(&a.stdout);

    let b = run_br(&workspace, ["create", "Issue B"], "create_b");
    assert!(b.status.success(), "create b: {}", b.stderr);
    let id_b = parse_created_id(&b.stdout);

    let dep = run_br(&workspace, ["dep", "add", &id_b, &id_a], "dep_ab");
    assert!(dep.status.success(), "dep add: {}", dep.stderr);

    // Two isolated singletons
    let c = run_br(&workspace, ["create", "Singleton C"], "create_c");
    assert!(c.status.success(), "create c: {}", c.stderr);
    let id_c = parse_created_id(&c.stdout);

    let d = run_br(&workspace, ["create", "Singleton D"], "create_d");
    assert!(d.status.success(), "create d: {}", d.stderr);
    let id_d = parse_created_id(&d.stdout);

    let graph = run_br(&workspace, ["graph", "--all"], "graph_all");
    assert!(graph.status.success(), "graph failed: {}", graph.stderr);

    let out = &graph.stdout;

    // Multi-node component gets numbered header
    assert!(
        out.contains("Component 1"),
        "Expected 'Component 1' header, got:\n{out}"
    );
    // Both ids from the connected component appear
    assert!(out.contains(&id_a), "Expected id_a in output, got:\n{out}");
    assert!(out.contains(&id_b), "Expected id_b in output, got:\n{out}");

    // Singleton block is present with correct count
    assert!(
        out.contains("Singletons (2 issues)"),
        "Expected 'Singletons (2 issues)', got:\n{out}"
    );
    // Both singleton IDs listed
    assert!(out.contains(&id_c), "Expected id_c in singletons, got:\n{out}");
    assert!(out.contains(&id_d), "Expected id_d in singletons, got:\n{out}");

    // Singletons must NOT get individual Component N headers
    assert!(
        !out.contains("Component 2"),
        "id_c must not appear as 'Component 2', got:\n{out}"
    );
    assert!(
        !out.contains("Component 3"),
        "id_d must not appear as 'Component 3', got:\n{out}"
    );

    // Summary line shows singleton count
    assert!(
        out.contains("(2 singletons)"),
        "Expected '(2 singletons)' in summary, got:\n{out}"
    );
}

/// No-singleton case: two issues connected — no singletons at all.
/// The `Singletons` header must not appear.
#[test]
fn e2e_graph_all_no_singletons() {
    let _log = common::test_log("e2e_graph_all_no_singletons");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let a = run_br(&workspace, ["create", "Issue A"], "create_a");
    assert!(a.status.success(), "create a: {}", a.stderr);
    let id_a = parse_created_id(&a.stdout);

    let b = run_br(&workspace, ["create", "Issue B"], "create_b");
    assert!(b.status.success(), "create b: {}", b.stderr);
    let id_b = parse_created_id(&b.stdout);

    let dep = run_br(&workspace, ["dep", "add", &id_b, &id_a], "dep_ab");
    assert!(dep.status.success(), "dep add: {}", dep.stderr);

    let graph = run_br(&workspace, ["graph", "--all"], "graph_all");
    assert!(graph.status.success(), "graph failed: {}", graph.stderr);

    let out = &graph.stdout;

    // Both issues present
    assert!(out.contains(&id_a), "Expected id_a, got:\n{out}");
    assert!(out.contains(&id_b), "Expected id_b, got:\n{out}");

    // No Singletons section
    assert!(
        !out.contains("Singletons"),
        "'Singletons' header must not appear when no singletons exist, got:\n{out}"
    );
    // No singleton suffix in summary
    assert!(
        !out.contains("singleton"),
        "'singleton' must not appear in summary when none exist, got:\n{out}"
    );
}

/// All-singleton case: three isolated issues, no edges.
/// - No `Component N` numbered headers
/// - `Singletons (3 issues)` block with all ids
#[test]
fn e2e_graph_all_only_singletons() {
    let _log = common::test_log("e2e_graph_all_only_singletons");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let a = run_br(&workspace, ["create", "Solo A"], "create_a");
    assert!(a.status.success(), "create a: {}", a.stderr);
    let id_a = parse_created_id(&a.stdout);

    let b = run_br(&workspace, ["create", "Solo B"], "create_b");
    assert!(b.status.success(), "create b: {}", b.stderr);
    let id_b = parse_created_id(&b.stdout);

    let c = run_br(&workspace, ["create", "Solo C"], "create_c");
    assert!(c.status.success(), "create c: {}", c.stderr);
    let id_c = parse_created_id(&c.stdout);

    let graph = run_br(&workspace, ["graph", "--all"], "graph_all");
    assert!(graph.status.success(), "graph failed: {}", graph.stderr);

    let out = &graph.stdout;

    // No numbered component headers
    assert!(
        !out.contains("Component 1"),
        "No 'Component N' headers expected when all are singletons, got:\n{out}"
    );

    // Singletons section with correct count
    assert!(
        out.contains("Singletons (3 issues)"),
        "Expected 'Singletons (3 issues)', got:\n{out}"
    );

    // All ids present
    assert!(out.contains(&id_a), "Expected id_a, got:\n{out}");
    assert!(out.contains(&id_b), "Expected id_b, got:\n{out}");
    assert!(out.contains(&id_c), "Expected id_c, got:\n{out}");

    // No (root) marker — singletons suppress it
    assert!(
        !out.contains("(root)"),
        "'(root)' must not appear in the singletons block, got:\n{out}"
    );
}
