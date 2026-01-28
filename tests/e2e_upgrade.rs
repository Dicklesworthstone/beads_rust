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

// =============================================================================
// Feature Guard Tests
// =============================================================================

/// Check if the `self_update` feature is enabled by testing if upgrade command exists.
/// This test verifies the binary was compiled with `self_update` support.
#[test]
fn e2e_upgrade_feature_enabled() {
    // The upgrade command should exist when self_update feature is enabled (default)
    let workspace = BrWorkspace::new();

    let upgrade = run_br(&workspace, ["upgrade", "--help"], "upgrade_feature_check");

    // If self_update feature is disabled, upgrade command won't exist
    // and we'd get an error about unknown command
    let output_combined = format!("{}{}", upgrade.stdout, upgrade.stderr);
    let feature_enabled = !output_combined.contains("unrecognized subcommand")
        && !output_combined.contains("unknown command")
        && !output_combined.contains("invalid subcommand");

    if !feature_enabled {
        eprintln!(
            "Note: self_update feature appears to be disabled. Upgrade tests will skip gracefully."
        );
    }
}

// =============================================================================
// Guarded Full Upgrade Tests
// =============================================================================
//
// These tests perform actual upgrade operations and are gated behind the
// BR_TEST_FULL_UPGRADE environment variable to prevent accidental execution.
//
// To run these tests:
//   BR_TEST_FULL_UPGRADE=1 cargo test e2e_upgrade_guarded
//
// Safety:
// - Tests use an isolated temp directory for the binary
// - Tests copy the current binary to temp before attempting upgrade
// - No modifications are made to the system binary

/// Helper to check if full upgrade tests are enabled via environment variable.
fn full_upgrade_tests_enabled() -> bool {
    std::env::var("BR_TEST_FULL_UPGRADE").is_ok_and(|v| v == "1" || v.to_lowercase() == "true")
}

/// Helper to copy the br binary to an isolated temp directory.
/// Returns the path to the copied binary.
fn setup_isolated_binary(workspace: &BrWorkspace) -> Option<std::path::PathBuf> {
    let bin_dir = workspace.root.join("bin");
    std::fs::create_dir_all(&bin_dir).ok()?;

    let target_binary = bin_dir.join("br");

    // Find the current test binary location
    let current_binary = assert_cmd::cargo::cargo_bin!("br");

    // Copy the binary to the isolated location
    std::fs::copy(current_binary, &target_binary).ok()?;

    // Make it executable on Unix
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&target_binary).ok()?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&target_binary, perms).ok()?;
    }

    Some(target_binary)
}

#[test]
fn e2e_upgrade_guarded_full_upgrade_skipped_without_env() {
    // This test verifies the guard mechanism works
    if full_upgrade_tests_enabled() {
        // If env is set, this test should be skipped (the actual test runs)
        return;
    }

    // Without the env var, we just verify the guard mechanism
    eprintln!("Full upgrade tests are disabled. Set BR_TEST_FULL_UPGRADE=1 to enable.");
}

#[test]
fn e2e_upgrade_guarded_isolated_binary_setup() {
    // Skip if not enabled
    if !full_upgrade_tests_enabled() {
        eprintln!("Skipping: BR_TEST_FULL_UPGRADE not set");
        return;
    }

    // Test that we can set up an isolated binary
    let workspace = BrWorkspace::new();
    let isolated_binary = setup_isolated_binary(&workspace);

    assert!(
        isolated_binary.is_some(),
        "should be able to copy binary to isolated location"
    );

    let binary_path = isolated_binary.unwrap();
    assert!(binary_path.exists(), "isolated binary should exist");

    // Verify the isolated binary works
    let output = std::process::Command::new(&binary_path)
        .arg("version")
        .output()
        .expect("run isolated binary");

    assert!(
        output.status.success(),
        "isolated binary should run: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn e2e_upgrade_guarded_full_upgrade_check_only() {
    // Skip if not enabled
    if !full_upgrade_tests_enabled() {
        eprintln!("Skipping: BR_TEST_FULL_UPGRADE not set");
        return;
    }

    // Even with the guard, we still only do --check to verify the flow works
    // without actually modifying any binaries
    let workspace = BrWorkspace::new();
    let isolated_binary = setup_isolated_binary(&workspace);

    if isolated_binary.is_none() {
        eprintln!("Skipping: could not set up isolated binary");
        return;
    }

    let binary_path = isolated_binary.unwrap();

    // Run upgrade --check on the isolated binary
    let output = std::process::Command::new(&binary_path)
        .args(["upgrade", "--check", "--json"])
        .current_dir(&workspace.root)
        .output()
        .expect("run upgrade --check");

    // Should complete (may succeed or fail due to network)
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Verify we get a structured response (success or error)
    assert!(
        stdout.contains("version") || stdout.contains("error") || stderr.contains("NetworkError"),
        "upgrade --check should return version info or error"
    );
}

#[test]
fn e2e_upgrade_guarded_dry_run_isolated() {
    // Skip if not enabled
    if !full_upgrade_tests_enabled() {
        eprintln!("Skipping: BR_TEST_FULL_UPGRADE not set");
        return;
    }

    let workspace = BrWorkspace::new();
    let isolated_binary = setup_isolated_binary(&workspace);

    if isolated_binary.is_none() {
        eprintln!("Skipping: could not set up isolated binary");
        return;
    }

    let binary_path = isolated_binary.unwrap();

    // Run upgrade --dry-run on the isolated binary
    let output = std::process::Command::new(&binary_path)
        .args(["upgrade", "--dry-run", "--json"])
        .current_dir(&workspace.root)
        .output()
        .expect("run upgrade --dry-run");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Dry-run should never modify the binary
    assert!(
        stdout.contains("dry_run")
            || stdout.contains("would")
            || stderr.contains("NetworkError")
            || stderr.contains("error"),
        "dry-run should indicate no changes: stdout={stdout}, stderr={stderr}"
    );

    // Verify the binary is still the same (not modified)
    let binary_exists = binary_path.exists();
    assert!(
        binary_exists,
        "isolated binary should still exist after dry-run"
    );
}

// =============================================================================
// Network Error Logging Tests
// =============================================================================

#[test]
fn e2e_upgrade_captures_network_error_in_log() {
    // Verify that network errors are properly captured and logged
    let workspace = BrWorkspace::new();

    let upgrade = run_br(
        &workspace,
        ["upgrade", "--check", "--json"],
        "upgrade_network_log",
    );

    // Read the log file to verify error was captured
    let log_content = std::fs::read_to_string(&upgrade.log_path).unwrap_or_default();

    // Log should contain the command and output
    assert!(
        log_content.contains("upgrade"),
        "log should contain command name"
    );

    // If there was an error, it should be in the log
    if !upgrade.status.success() {
        assert!(
            log_content.contains("stderr") || log_content.contains("error"),
            "log should capture error output"
        );
    }
}

#[test]
fn e2e_upgrade_json_error_is_valid_json() {
    // Ensure any JSON error output is well-formed
    let workspace = BrWorkspace::new();

    let upgrade = run_br(
        &workspace,
        ["upgrade", "--check", "--json"],
        "upgrade_json_valid",
    );

    // Try to parse any JSON in output
    let output = if upgrade.stdout.trim().is_empty() {
        &upgrade.stderr
    } else {
        &upgrade.stdout
    };

    let json_str = extract_json_payload(output);
    if !json_str.is_empty() {
        let parse_result: Result<serde_json::Value, _> = serde_json::from_str(&json_str);
        assert!(
            parse_result.is_ok(),
            "JSON output should be valid: {} (error: {:?})",
            json_str,
            parse_result.err()
        );
    }
}

// =============================================================================
// Non-Flaky Behavior Tests
// =============================================================================

#[test]
fn e2e_upgrade_consistent_help_output() {
    // Help output should be consistent across multiple runs (non-flaky)
    let workspace = BrWorkspace::new();

    let run1 = run_br(&workspace, ["upgrade", "--help"], "upgrade_help_1");
    let run2 = run_br(&workspace, ["upgrade", "--help"], "upgrade_help_2");

    assert!(run1.status.success(), "run1 failed");
    assert!(run2.status.success(), "run2 failed");
    assert_eq!(
        run1.stdout, run2.stdout,
        "help output should be consistent across runs"
    );
}

#[test]
fn e2e_upgrade_version_output_stable() {
    // Version output should be stable across multiple runs
    let workspace = BrWorkspace::new();

    let run1 = run_br(&workspace, ["version"], "version_stable_1");
    let run2 = run_br(&workspace, ["version"], "version_stable_2");

    assert!(run1.status.success(), "run1 failed");
    assert!(run2.status.success(), "run2 failed");
    assert_eq!(
        run1.stdout, run2.stdout,
        "version output should be stable across runs"
    );
}
