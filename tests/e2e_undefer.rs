use crate::common::cli::{BrWorkspace, run_br};
mod common;

#[test]
fn test_soft_defer_behavior() {
    let workspace = BrWorkspace::new();
    run_br(&workspace, ["init"], "init");

    // Create issue
    let create = run_br(&workspace, ["create", "Soft Defer"], "create");
    // Extract ID (e.g. "Created bd-1: Soft Defer")
    let id_line = create.stdout.lines().next().unwrap();
    let id = id_line
        .split_whitespace()
        .nth(1)
        .unwrap()
        .trim_end_matches(':');

    // Soft defer using update (sets date but not status)
    // Note: status remains 'open' by default if not specified
    let update = run_br(&workspace, ["update", id, "--defer", "+1d"], "update");
    assert!(update.status.success());

    // Check status is still open?
    let show = run_br(&workspace, ["show", id], "show");
    assert!(show.stdout.contains("[open]"), "Status should remain open");

    // Check ready - should be excluded
    let ready = run_br(&workspace, ["ready"], "ready");
    assert!(
        !ready.stdout.contains(id),
        "Soft deferred issue should be excluded from ready"
    );

    // Try undefer
    let undefer = run_br(&workspace, ["undefer", id], "undefer");
    // If bug exists, this will say skipped/not deferred
    println!("Undefer output: {}", undefer.stdout);

    // Expectation: user should be able to undefer any deferred issue (date or status)
    // or update should have set status.
    // If undefer fails to clear date, it's a bug.
    // Let's verify if date was cleared.

    let _show_after = run_br(&workspace, ["show", id], "show_after");
    // If cleared, it won't show "until ..." (unless show logic hides it? show displays description etc)
    // show doesn't explicitly list defer_until in standard output?
    // It does! "  [open] (until ...)" ? No, wait.
    // src/cli/commands/show.rs doesn't print defer_until in standard output?
    // Let's check show.rs.
    // It prints ID, Title, Status. Labels. Description. Deps. Comments.
    // It DOES NOT print defer_until!
    // So checking "show" text output is insufficient.
    // Need --json.

    let show_json = run_br(&workspace, ["show", id, "--json"], "show_json");
    // Parse JSON
    let json: serde_json::Value = serde_json::from_str(&show_json.stdout).unwrap();
    // The issue might be in an array (show returns list)
    let issue = if json.is_array() { &json[0] } else { &json };

    // field should be missing or null
    assert!(
        issue.get("defer_until").is_none() || issue["defer_until"].is_null(),
        "defer_until should be null/missing, got: {:?}",
        issue.get("defer_until")
    );
}
