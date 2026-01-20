//! E2E tests for the `stale` command.

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;

#[test]
fn e2e_stale_basic() {
    common::init_test_logging();
    let workspace = BrWorkspace::new();
    run_br(&workspace, ["init"], "init");

    // Create 3 issues
    run_br(&workspace, ["create", "Issue 1"], "create1");
    run_br(&workspace, ["create", "Issue 2"], "create2");
    run_br(&workspace, ["create", "Issue 3"], "create3");

    // Check stale --days 0 (should include all, since updated_at <= now)
    // Actually, stale means updated BEFORE (now - days).
    // If days=0, threshold = now. updated_at <= now. So all issues are stale.
    let stale0 = run_br(&workspace, ["stale", "--days", "0"], "stale0");
    assert!(stale0.status.success());
    // Should verify count
    let count = stale0.stdout.lines().filter(|l| l.contains(". [")).count();
    assert_eq!(count, 3, "All issues should be stale with days=0");

    // Check stale --days 1 (should include none, since updated_at > now - 1 day)
    let stale1 = run_br(&workspace, ["stale", "--days", "1"], "stale1");
    assert!(stale1.status.success());
    let count1 = stale1.stdout.lines().filter(|l| l.contains(". [")).count();
    assert_eq!(
        count1, 0,
        "No issues should be stale with days=1 (freshly created)"
    );
}

#[test]
fn e2e_stale_json_output() {
    common::init_test_logging();
    let workspace = BrWorkspace::new();
    run_br(&workspace, ["init"], "init");
    run_br(&workspace, ["create", "Issue JSON"], "create");

    let stale = run_br(&workspace, ["stale", "--days", "0", "--json"], "stale_json");
    assert!(stale.status.success());

    let payload = extract_json_payload(&stale.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("valid json");
    assert_eq!(json.len(), 1);
    assert_eq!(json[0]["title"], "Issue JSON");
    // Verify StaleIssue structure
    assert!(json[0].get("updated_at").is_some());
}

#[test]
fn e2e_stale_with_status_filter() {
    common::init_test_logging();
    let workspace = BrWorkspace::new();
    run_br(&workspace, ["init"], "init");

    run_br(&workspace, ["create", "Open Issue"], "create1");

    let create2 = run_br(&workspace, ["create", "InProgress Issue"], "create2");
    let id2 = create2
        .stdout
        .split_whitespace()
        .find(|w| w.starts_with("bd-"))
        .unwrap()
        .trim_end_matches(':');
    run_br(
        &workspace,
        ["update", id2, "--status", "in_progress"],
        "update2",
    );

    // Filter by status=open
    let stale_open = run_br(
        &workspace,
        ["stale", "--days", "0", "--status", "open"],
        "stale_open",
    );
    let count_open = stale_open
        .stdout
        .lines()
        .filter(|l| l.contains("Open Issue"))
        .count();
    assert_eq!(count_open, 1);
    assert!(!stale_open.stdout.contains("InProgress Issue"));
}
