//! E2E tests for the `upgrade` and `version` commands.
//!
//! Test coverage:
//! - Version command functionality
//! - Upgrade --check behavior
//! - Upgrade --dry-run behavior
//! - Error handling for network issues
//! - JSON output structure
//!
//! Note: These tests cannot actually perform upgrades as that would modify
//! the binary under test. Tests focus on:
//! - Verifying command accepts correct arguments
//! - Verifying error handling is graceful
//! - Verifying JSON output structure

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;

// =============================================================================
// Version Command Tests
// =============================================================================

#[test]
fn e2e_version_shows_version() {
    // Version command should show version info
    let workspace = BrWorkspace::new();
    // Version doesn't require init

    let version = run_br(&workspace, ["version"], "version_basic");
    assert!(
        version.status.success(),
        "version command failed: {}",
        version.stderr
    );
    assert!(
        version.stdout.contains("br version"),
        "output should contain 'br version', got: {}",
        version.stdout
    );
}

#[test]
fn e2e_version_json_output() {
    // Version --json should return structured JSON
    let workspace = BrWorkspace::new();

    let version = run_br(&workspace, ["version", "--json"], "version_json");
    assert!(
        version.status.success(),
        "version --json failed: {}",
        version.stderr
    );

    let json_str = extract_json_payload(&version.stdout);
    let json: Value = serde_json::from_str(&json_str).expect("valid JSON");

    // Check expected fields
    assert!(json.get("version").is_some(), "missing 'version' field");
    assert!(json.get("build").is_some(), "missing 'build' field");
    assert!(json.get("commit").is_some(), "missing 'commit' field");
    assert!(json.get("branch").is_some(), "missing 'branch' field");
}

#[test]
fn e2e_version_no_workspace_required() {
    // Version should work without initialized workspace
    let workspace = BrWorkspace::new();
    // Deliberately NOT calling init

    let version = run_br(&workspace, ["version"], "version_no_workspace");
    assert!(
        version.status.success(),
        "version should work without workspace: {}",
        version.stderr
    );
}

// =============================================================================
// Upgrade --check Tests
// =============================================================================

#[test]
fn e2e_upgrade_check_attempts_api_call() {
    // Upgrade --check should attempt to call the GitHub API
    let workspace = BrWorkspace::new();

    let upgrade = run_br(&workspace, ["upgrade", "--check"], "upgrade_check");
    // May succeed or fail depending on network, but should handle gracefully
    // Either outputs version info (success) or error JSON (failure)
    assert!(
        upgrade.stdout.contains("version")
            || upgrade.stdout.contains("error")
            || upgrade.stderr.contains("error")
            || upgrade.stderr.contains("NetworkError"),
        "upgrade --check should output version or error info"
    );
}

#[test]
fn e2e_upgrade_check_json_error_structure() {
    // When network fails, JSON error should have proper structure
    let workspace = BrWorkspace::new();

    let upgrade = run_br(
        &workspace,
        ["upgrade", "--check", "--json"],
        "upgrade_check_json",
    );

    // Parse any JSON in output (could be success or error)
    let output = if upgrade.stdout.trim().is_empty() {
        &upgrade.stderr
    } else {
        &upgrade.stdout
    };

    let json_str = extract_json_payload(output);
    if !json_str.is_empty() {
        // Should be valid JSON regardless of success/failure
        let result: Result<Value, _> = serde_json::from_str(&json_str);
        assert!(
            result.is_ok(),
            "output should be valid JSON, got: {json_str}"
        );
    }
}

// =============================================================================
// Upgrade --dry-run Tests
// =============================================================================

#[test]
fn e2e_upgrade_dry_run_no_changes() {
    // Upgrade --dry-run should not modify anything
    let workspace = BrWorkspace::new();

    let upgrade = run_br(&workspace, ["upgrade", "--dry-run"], "upgrade_dry_run");
    // Should indicate dry-run mode
    assert!(
        upgrade.stdout.contains("dry-run")
            || upgrade.stdout.contains("Dry-run")
            || upgrade.stdout.contains("would")
            || upgrade.stderr.contains("dry-run")
            || upgrade.stderr.contains("Dry-run")
            || upgrade.stderr.contains("NetworkError"),
        "dry-run should indicate it's a dry run or show network error"
    );
}

#[test]
fn e2e_upgrade_dry_run_json() {
    // Upgrade --dry-run --json should return structured output
    let workspace = BrWorkspace::new();

    let upgrade = run_br(
        &workspace,
        ["upgrade", "--dry-run", "--json"],
        "upgrade_dry_run_json",
    );

    // Parse any JSON in output
    let output = if upgrade.stdout.trim().is_empty() {
        &upgrade.stderr
    } else {
        &upgrade.stdout
    };

    let json_str = extract_json_payload(output);
    if !json_str.is_empty() {
        let result: Result<Value, _> = serde_json::from_str(&json_str);
        assert!(
            result.is_ok(),
            "output should be valid JSON, got: {json_str}"
        );
    }
}

// =============================================================================
// Upgrade Argument Tests
// =============================================================================

#[test]
fn e2e_upgrade_with_version_flag() {
    // Upgrade --version <ver> should accept version argument
    let workspace = BrWorkspace::new();

    let upgrade = run_br(
        &workspace,
        ["upgrade", "--version", "0.1.0", "--dry-run"],
        "upgrade_specific_version",
    );
    // Should process the version argument (may fail on network, but should parse args)
    // Not checking exit code since network may fail
    assert!(
        upgrade.stdout.contains("0.1.0")
            || upgrade.stderr.contains("0.1.0")
            || upgrade.stderr.contains("NetworkError")
            || upgrade.stdout.contains("error"),
        "should reference version or show network error"
    );
}

#[test]
fn e2e_upgrade_force_flag_accepted() {
    // Upgrade --force should be accepted
    let workspace = BrWorkspace::new();

    let upgrade = run_br(
        &workspace,
        ["upgrade", "--force", "--dry-run"],
        "upgrade_force",
    );
    // Command should not fail on argument parsing
    // (may fail on network, but that's expected)
    assert!(
        !upgrade.stderr.contains("unknown argument") && !upgrade.stderr.contains("unrecognized"),
        "--force should be a valid argument"
    );
}

// =============================================================================
// Error Handling Tests
// =============================================================================

#[test]
fn e2e_upgrade_graceful_network_error() {
    // When network is unavailable, should fail gracefully with error message
    let workspace = BrWorkspace::new();

    let upgrade = run_br(
        &workspace,
        ["upgrade", "--check", "--json"],
        "upgrade_network_error",
    );

    // If there's an error (likely due to network), it should be structured
    if !upgrade.status.success() {
        let output = if upgrade.stdout.trim().is_empty() {
            &upgrade.stderr
        } else {
            &upgrade.stdout
        };

        let json_str = extract_json_payload(output);
        if !json_str.is_empty() {
            let json: Result<Value, _> = serde_json::from_str(&json_str);
            if let Ok(json) = json {
                // Error should have proper structure
                if json.get("error").is_some() {
                    let error = &json["error"];
                    assert!(
                        error.get("message").is_some() || error.get("code").is_some(),
                        "error should have message or code"
                    );
                }
            }
        }
    }
}

#[test]
fn e2e_upgrade_no_workspace_required() {
    // Upgrade should not require an initialized workspace
    let workspace = BrWorkspace::new();
    // Deliberately NOT calling init

    let upgrade = run_br(&workspace, ["upgrade", "--check"], "upgrade_no_workspace");
    // Should not fail due to missing workspace
    // (may fail due to network, but that's different)
    assert!(
        !upgrade.stderr.contains("No .beads") && !upgrade.stderr.contains("not initialized"),
        "upgrade should not require workspace initialization"
    );
}

// =============================================================================
// Combined Flag Tests
// =============================================================================

#[test]
fn e2e_upgrade_check_with_force_error() {
    // --check and --force together may be contradictory
    let workspace = BrWorkspace::new();

    let upgrade = run_br(
        &workspace,
        ["upgrade", "--check", "--force"],
        "upgrade_check_force",
    );
    // Either succeeds (check takes precedence) or errors due to conflicting flags
    // Both behaviors are acceptable
    assert!(
        upgrade.status.success()
            || upgrade.stderr.contains("conflict")
            || upgrade.stderr.contains("NetworkError")
            || upgrade.stdout.contains("error"),
        "conflicting flags should be handled"
    );
}

#[test]
fn e2e_upgrade_help_works() {
    // Upgrade --help should show help
    let workspace = BrWorkspace::new();

    let upgrade = run_br(&workspace, ["upgrade", "--help"], "upgrade_help");
    assert!(
        upgrade.status.success(),
        "upgrade --help failed: {}",
        upgrade.stderr
    );
    assert!(
        upgrade.stdout.contains("--check") && upgrade.stdout.contains("--dry-run"),
        "help should mention available flags"
    );
}
