//! E2E tests for the version command.
//!
//! Tests the `br version` command and its flags: --check, --short, --json.
//! Part of beads_rust-1hof.

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;

#[test]
fn e2e_version_short_flag() {
    let _log = common::test_log("e2e_version_short_flag");
    let workspace = BrWorkspace::new();

    // Test --short flag
    let version = run_br(&workspace, ["version", "--short"], "version_short");
    assert!(
        version.status.success(),
        "version --short failed: {}",
        version.stderr
    );

    let stdout = version.stdout.trim();
    // Should be just the version number, e.g. "0.1.7"
    assert!(
        stdout.chars().all(|c| c.is_numeric() || c == '.'),
        "version --short should contain only version number, got: '{}'",
        stdout
    );
    assert!(
        stdout.contains('.'),
        "version --short should look like semver, got: '{}'",
        stdout
    );
}

#[test]
fn e2e_version_json_flag() {
    let _log = common::test_log("e2e_version_json_flag");
    let workspace = BrWorkspace::new();

    // Test --json flag
    let version = run_br(&workspace, ["version", "--json"], "version_json");
    assert!(
        version.status.success(),
        "version --json failed: {}",
        version.stderr
    );

    let payload = extract_json_payload(&version.stdout);
    let json: Value = serde_json::from_str(&payload).expect("valid JSON");

    // Verify fields
    assert!(json.get("version").is_some(), "missing version field");
    assert!(json.get("build").is_some(), "missing build field");
    if option_env!("VERGEN_GIT_SHA").is_some() {
        assert!(json.get("commit").is_some(), "missing commit field");
    }
    // The features field is only present when self_update feature is enabled
    #[cfg(feature = "self_update")]
    assert!(json.get("features").is_some(), "missing features field");
    #[cfg(not(feature = "self_update"))]
    assert!(
        json.get("features").is_none(),
        "features field should be absent without self_update"
    );
}

#[test]
fn e2e_version_check_flag() {
    let _log = common::test_log("e2e_version_check_flag");
    let workspace = BrWorkspace::new();

    // Test --check flag
    // This connects to network, so it might flake if offline or GitHub API rate limited.
    // We mainly verify it runs and returns a valid exit code (0, 1, or 2).
    let version = run_br(&workspace, ["version", "--check"], "version_check");

    // It's acceptable for check to fail (exit 2) if offline, or exit 1 if update available.
    // But if it succeeds (exit 0), it should output "up to date".

    if version.status.success() {
        assert!(
            version.stdout.contains("up to date") || version.stdout.contains("Update available"),
            "version --check stdout unexpected: {}",
            version.stdout
        );
    } else {
        // If it failed, check if it was due to network error (exit 2) or update available (exit 1)
        let code = version.status.code().unwrap_or(0);
        assert!(code == 1 || code == 2, "unexpected exit code: {}", code);
    }
}

#[test]
fn e2e_version_check_json() {
    let _log = common::test_log("e2e_version_check_json");
    let workspace = BrWorkspace::new();

    // Test --check --json
    let version = run_br(
        &workspace,
        ["version", "--check", "--json"],
        "version_check_json",
    );

    // Should always return valid JSON regardless of exit code
    let payload = extract_json_payload(&version.stdout);
    let json: Value = serde_json::from_str(&payload).expect("valid JSON");

    assert!(json.get("current").is_some(), "missing current field");

    // 'latest' and 'update_available' might be null on error, or present on success
    if let Some(error) = json.get("error") {
        assert!(
            !error.as_str().unwrap_or("").is_empty(),
            "error field should be non-empty"
        );
    } else {
        assert!(json.get("latest").is_some(), "missing latest field");
        assert!(
            json.get("update_available").is_some(),
            "missing update_available field"
        );
    }
}

/// GitHub #514: `build.rs` stamps `VERGEN_GIT_SHA` from `git rev-parse HEAD`
/// but did not tell Cargo to watch the git files, so after a commit or pull an
/// incremental rebuild reused the old stamp and `br version` reported a stale
/// commit. When the tests run from a git checkout, the binary built for them
/// must report the checkout's current `HEAD`.
#[test]
fn e2e_version_commit_matches_checkout_head() {
    let _log = common::test_log("e2e_version_commit_matches_checkout_head");
    let manifest_dir = env!("CARGO_MANIFEST_DIR");
    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(manifest_dir)
            .output()
            .ok()
            .filter(|output| output.status.success())
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_string())
    };
    // Source trees synced without `.git` (or unreadable by this user) take
    // build.rs's environment fallback path; nothing to compare against.
    if git(&["rev-parse", "--is-inside-work-tree"]).as_deref() != Some("true") {
        eprintln!("skipping: {manifest_dir} is not a readable git checkout");
        return;
    }
    let Some(head) = git(&["rev-parse", "HEAD"]) else {
        eprintln!("skipping: git rev-parse HEAD failed");
        return;
    };

    let workspace = BrWorkspace::new();
    let version = run_br(&workspace, ["version", "--json"], "version_commit_head");
    assert!(
        version.status.success(),
        "version --json failed: {}",
        version.stderr
    );
    let payload = extract_json_payload(&version.stdout);
    let json: Value = serde_json::from_str(&payload).expect("valid JSON");
    assert_eq!(
        json.get("commit").and_then(Value::as_str),
        Some(head.as_str()),
        "br version must report the HEAD it was built from; a stale value means \
         build.rs did not rerun after HEAD moved"
    );
}
