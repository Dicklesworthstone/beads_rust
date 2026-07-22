mod common;

use common::cli::{run_br_with_env, BrWorkspace};

fn parse_created_id(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or("");
    let normalized = line
        .strip_prefix("✓ ")
        .or_else(|| line.strip_prefix("✗ "))
        .unwrap_or(line);
    let id_part = normalized
        .strip_prefix("Created ")
        .and_then(|rest| rest.split(':').next())
        .unwrap_or("");
    id_part.trim().to_string()
}

/// Helper: run br with a specific prefix to override any ambient env.
fn br(ws: &BrWorkspace, args: &[&str], label: &str) -> common::cli::BrRun {
    // Set BD_ISSUE_PREFIX to a known value per-test using the env override.
    // Individual tests set the prefix via --prefix on create or init.
    // We clear BD_ISSUE_PREFIX by not setting it — but the env leaks.
    // Instead, each test explicitly passes --prefix on commands that need it.
    run_br_with_env(
        ws,
        args,
        std::iter::empty::<(&str, &str)>(),
        label,
    )
}

/// Run br with a specific prefix env override.
fn br_with_prefix(ws: &BrWorkspace, args: &[&str], prefix: &str, label: &str) -> common::cli::BrRun {
    run_br_with_env(
        ws,
        args,
        [("BD_ISSUE_PREFIX", prefix)],
        label,
    )
}

#[test]
fn test_reprefix_e2e_basic() {
    let ws = BrWorkspace::new();

    let r = br_with_prefix(&ws, &["init", "--prefix", "aa"], "aa", "init");
    assert!(r.status.success(), "init failed: {}", r.stderr);

    let r = br_with_prefix(&ws, &["create", "Reprefix test"], "aa", "create");
    assert!(r.status.success(), "create failed: {}", r.stderr);
    let old_id = parse_created_id(&r.stdout);
    assert!(old_id.starts_with("aa-"), "Expected aa- prefix, got {old_id}");
    let remainder = old_id.strip_prefix("aa-").unwrap();

    // Reprefix to "bb"
    let r = br_with_prefix(&ws, &["update", &old_id, "--reprefix", "bb"], "aa", "reprefix");
    assert!(r.status.success(), "reprefix failed: {}", r.stderr);
    assert!(
        r.stdout.contains(&format!("bb-{remainder}")),
        "Output should contain new id, got: {}",
        r.stdout
    );

    // New id should resolve
    let new_id = format!("bb-{remainder}");
    let r = br_with_prefix(&ws, &["show", &new_id], "aa", "show-new");
    assert!(r.status.success(), "show new id failed: {}", r.stderr);
    assert!(
        r.stdout.contains("Reprefix test"),
        "New id should show original title"
    );
}

#[test]
fn test_reprefix_e2e_with_dependency() {
    let ws = BrWorkspace::new();

    let r = br_with_prefix(&ws, &["init", "--prefix", "xx"], "xx", "init");
    assert!(r.status.success());

    let r = br_with_prefix(&ws, &["create", "Blocker issue"], "xx", "create-blocker");
    assert!(r.status.success());
    let blocker_id = parse_created_id(&r.stdout);

    let r = br_with_prefix(&ws, &["create", "Blocked issue"], "xx", "create-blocked");
    assert!(r.status.success());
    let blocked_id = parse_created_id(&r.stdout);

    // Add dependency: blocked depends on blocker
    let r = br_with_prefix(
        &ws,
        &["dep", "add", &blocked_id, &blocker_id],
        "xx",
        "add-dep",
    );
    assert!(r.status.success(), "dep add failed: {}", r.stderr);

    // Reprefix the blocker
    let r = br_with_prefix(
        &ws,
        &["update", &blocker_id, "--reprefix", "yy"],
        "xx",
        "reprefix-blocker",
    );
    assert!(r.status.success(), "reprefix failed: {}", r.stderr);

    let blocker_remainder = blocker_id.strip_prefix("xx-").unwrap();
    let new_blocker = format!("yy-{blocker_remainder}");

    // Show the blocked issue -- its dependency should now point to new id
    let r = br_with_prefix(&ws, &["show", &blocked_id, "--json"], "xx", "show-blocked");
    assert!(r.status.success());
    assert!(
        r.stdout.contains(&new_blocker),
        "Dependency should point to reprefixed id {new_blocker}, got: {}",
        r.stdout
    );
}

#[test]
fn test_reprefix_e2e_operator_rejected() {
    let ws = BrWorkspace::new();

    let r = br_with_prefix(&ws, &["init", "--prefix", "zz"], "zz", "init");
    assert!(r.status.success());

    let r = br_with_prefix(&ws, &["create", "Test"], "zz", "create");
    assert!(r.status.success());
    let id = parse_created_id(&r.stdout);

    let r = br_with_prefix(
        &ws,
        &["update", &id, "--reprefix", "operator"],
        "zz",
        "reprefix-operator",
    );
    assert!(
        !r.status.success(),
        "Reprefix to operator should be rejected"
    );
    assert!(
        r.stderr.contains("reserved") || r.stderr.contains("operator"),
        "Error should mention reserved/operator: {}",
        r.stderr
    );
}

#[test]
fn test_reprefix_e2e_json_output() {
    let ws = BrWorkspace::new();

    let r = br_with_prefix(&ws, &["init", "--prefix", "jj"], "jj", "init");
    assert!(r.status.success());

    let r = br_with_prefix(&ws, &["create", "JSON test"], "jj", "create");
    assert!(r.status.success());
    let old_id = parse_created_id(&r.stdout);

    let r = br_with_prefix(
        &ws,
        &["update", &old_id, "--reprefix", "kk", "--json"],
        "jj",
        "reprefix-json",
    );
    assert!(r.status.success(), "reprefix json failed: {}", r.stderr);

    // Parse JSON output
    let json: serde_json::Value =
        serde_json::from_str(r.stdout.trim()).expect("valid JSON output");
    assert_eq!(json["old_id"].as_str().unwrap(), old_id);
    assert!(json["new_id"].as_str().unwrap().starts_with("kk-"));
    assert_eq!(json["title"].as_str().unwrap(), "JSON test");
}
