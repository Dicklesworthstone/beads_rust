mod common;

use common::cli::{BrWorkspace, run_br};
use common::harness::parse_created_id;

#[test]
fn test_reprefix_e2e_basic() {
    let ws = BrWorkspace::new();

    let r = run_br(&ws, ["init"], "init");
    assert!(r.status.success(), "init failed: {}", r.stderr);

    let r = run_br(&ws, ["create", "Reprefix test", "--prefix", "aa"], "create");
    assert!(r.status.success(), "create failed: {}", r.stderr);
    let old_id = parse_created_id(&r.stdout);
    assert!(
        old_id.starts_with("aa-"),
        "Expected aa- prefix, got {old_id}"
    );
    let remainder = old_id.strip_prefix("aa-").unwrap();

    // Reprefix to "bb"
    let r = run_br(&ws, ["update", &old_id, "--reprefix", "bb"], "reprefix");
    assert!(r.status.success(), "reprefix failed: {}", r.stderr);
    assert!(
        r.stdout.contains(&format!("bb-{remainder}")),
        "Output should contain new id, got: {}",
        r.stdout
    );

    // New id should resolve
    let new_id = format!("bb-{remainder}");
    let r = run_br(&ws, ["show", &new_id], "show-new");
    assert!(r.status.success(), "show new id failed: {}", r.stderr);
    assert!(
        r.stdout.contains("Reprefix test"),
        "New id should show original title"
    );
}

#[test]
fn test_reprefix_e2e_with_dependency() {
    let ws = BrWorkspace::new();

    let r = run_br(&ws, ["init"], "init");
    assert!(r.status.success());

    let r = run_br(
        &ws,
        ["create", "Blocker issue", "--prefix", "xx"],
        "create-blocker",
    );
    assert!(r.status.success());
    let blocker_id = parse_created_id(&r.stdout);

    let r = run_br(
        &ws,
        ["create", "Blocked issue", "--prefix", "xx"],
        "create-blocked",
    );
    assert!(r.status.success());
    let blocked_id = parse_created_id(&r.stdout);

    // Add dependency: blocked depends on blocker
    let r = run_br(&ws, ["dep", "add", &blocked_id, &blocker_id], "add-dep");
    assert!(r.status.success(), "dep add failed: {}", r.stderr);

    // Reprefix the blocker
    let r = run_br(
        &ws,
        ["update", &blocker_id, "--reprefix", "yy"],
        "reprefix-blocker",
    );
    assert!(r.status.success(), "reprefix failed: {}", r.stderr);

    let blocker_remainder = blocker_id.strip_prefix("xx-").unwrap();
    let new_blocker = format!("yy-{blocker_remainder}");

    // Show the blocked issue -- its dependency should now point to new id
    let r = run_br(&ws, ["show", &blocked_id, "--json"], "show-blocked");
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

    let r = run_br(&ws, ["init"], "init");
    assert!(r.status.success());

    let r = run_br(&ws, ["create", "Test", "--prefix", "zz"], "create");
    assert!(r.status.success());
    let id = parse_created_id(&r.stdout);

    let r = run_br(
        &ws,
        ["update", &id, "--reprefix", "operator"],
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

    let r = run_br(&ws, ["init"], "init");
    assert!(r.status.success());

    let r = run_br(&ws, ["create", "JSON test", "--prefix", "jj"], "create");
    assert!(r.status.success());
    let old_id = parse_created_id(&r.stdout);

    let r = run_br(
        &ws,
        ["update", &old_id, "--reprefix", "kk", "--json"],
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

/// Regression: creation without --prefix errors, and BD_ISSUE_PREFIX has no
/// effect on creation (mandatory-prefix enforcement, config removal).
///
/// Uses `run_br_raw_with_env` deliberately — args pass through verbatim,
/// bypassing the test harness's `--prefix bd` convenience shim (see
/// `common::apply_default_test_prefix_shim`). Do not swap this for a
/// shimmed helper; that would silently defeat the assertions below.
#[test]
fn test_create_requires_explicit_prefix_env_is_dead() {
    let ws = BrWorkspace::new();

    let r = run_br(&ws, ["init"], "init");
    assert!(r.status.success());

    // No --prefix at all: must error naming --prefix.
    let r = common::cli::run_br_raw_with_env(
        &ws,
        ["create", "No prefix given"],
        std::iter::empty::<(&str, &str)>(),
        "create-no-prefix",
    );
    assert!(
        !r.status.success(),
        "create without --prefix must fail, got: {}",
        r.stdout
    );
    assert!(
        r.stderr.to_lowercase().contains("prefix"),
        "error should mention --prefix: {}",
        r.stderr
    );

    // BD_ISSUE_PREFIX set but no --prefix flag: still must error (env is dead).
    let r = common::cli::run_br_raw_with_env(
        &ws,
        ["create", "Still no prefix"],
        [("BD_ISSUE_PREFIX", "zzz")],
        "create-env-ignored",
    );
    assert!(
        !r.status.success(),
        "BD_ISSUE_PREFIX must not satisfy the --prefix requirement"
    );
    assert!(
        r.stderr.to_lowercase().contains("prefix"),
        "error should mention --prefix: {}",
        r.stderr
    );

    // With --prefix explicitly, BD_ISSUE_PREFIX must not override it.
    let r = common::cli::run_br_raw_with_env(
        &ws,
        ["create", "Explicit wins", "--prefix", "real"],
        [("BD_ISSUE_PREFIX", "zzz")],
        "create-env-does-not-override",
    );
    assert!(r.status.success(), "create failed: {}", r.stderr);
    let id = parse_created_id(&r.stdout);
    assert!(
        id.starts_with("real-"),
        "Expected explicit --prefix 'real' to win over BD_ISSUE_PREFIX, got {id}"
    );
}
