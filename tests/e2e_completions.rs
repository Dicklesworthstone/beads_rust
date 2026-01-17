//! E2E tests for the `completions` command.
//!
//! Test coverage:
//! - Generate completions for each supported shell (bash, zsh, fish, powershell, elvish)
//! - Verify completions contain expected subcommand names
//! - Verify completions contain expected flag names
//! - Edge cases (unknown shell, idempotency)

mod common;

use common::cli::{BrWorkspace, run_br};
use tracing::info;

// =============================================================================
// Helper Functions
// =============================================================================

fn init_workspace(workspace: &BrWorkspace) {
    let init = run_br(workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
}

/// Check that completions output contains expected subcommand names.
fn assert_contains_subcommands(output: &str, shell_name: &str) {
    // Core subcommands that should appear in all completions
    let expected_subcommands = [
        "init", "create", "list", "show", "update", "close", "sync", "audit", "q",
    ];

    for cmd in expected_subcommands {
        assert!(
            output.contains(cmd),
            "{shell_name} completions should contain '{cmd}' subcommand"
        );
    }
}

/// Check that completions output contains expected flag names.
fn assert_contains_flags(output: &str, shell_name: &str) {
    // Global flags that should appear in completions
    let expected_flags = ["--help", "--json", "--verbose", "--quiet"];

    for flag in expected_flags {
        assert!(
            output.contains(flag),
            "{shell_name} completions should contain '{flag}' flag"
        );
    }
}

// =============================================================================
// Bash Completions Tests
// =============================================================================

#[test]
fn e2e_completions_bash_generates_valid_script() {
    common::init_test_logging();
    info!("e2e_completions_bash_generates_valid_script: start");
    // Generate bash completions and verify it's a valid bash script
    let workspace = BrWorkspace::new();
    // Note: completions don't require init

    let completions = run_br(&workspace, ["completions", "bash"], "completions_bash");
    assert!(
        completions.status.success(),
        "completions bash failed: {}",
        completions.stderr
    );

    // Bash completions should define the completion function
    assert!(
        completions.stdout.contains("_br()"),
        "bash completions should define _br function"
    );
    assert!(
        completions.stdout.contains("COMPREPLY"),
        "bash completions should use COMPREPLY"
    );
    info!("e2e_completions_bash_generates_valid_script: done");
}

#[test]
fn e2e_completions_bash_contains_subcommands() {
    common::init_test_logging();
    info!("e2e_completions_bash_contains_subcommands: start");
    let workspace = BrWorkspace::new();

    let completions = run_br(
        &workspace,
        ["completions", "bash"],
        "completions_bash_subcommands",
    );
    assert!(
        completions.status.success(),
        "completions failed: {}",
        completions.stderr
    );

    assert_contains_subcommands(&completions.stdout, "bash");
    info!("e2e_completions_bash_contains_subcommands: done");
}

#[test]
fn e2e_completions_bash_contains_flags() {
    common::init_test_logging();
    info!("e2e_completions_bash_contains_flags: start");
    let workspace = BrWorkspace::new();

    let completions = run_br(
        &workspace,
        ["completions", "bash"],
        "completions_bash_flags",
    );
    assert!(
        completions.status.success(),
        "completions failed: {}",
        completions.stderr
    );

    assert_contains_flags(&completions.stdout, "bash");
    info!("e2e_completions_bash_contains_flags: done");
}

// =============================================================================
// Zsh Completions Tests
// =============================================================================

#[test]
fn e2e_completions_zsh_generates_valid_script() {
    common::init_test_logging();
    info!("e2e_completions_zsh_generates_valid_script: start");
    // Generate zsh completions and verify structure
    let workspace = BrWorkspace::new();

    let completions = run_br(&workspace, ["completions", "zsh"], "completions_zsh");
    assert!(
        completions.status.success(),
        "completions zsh failed: {}",
        completions.stderr
    );

    // Zsh completions should have the compdef directive
    assert!(
        completions.stdout.contains("#compdef") || completions.stdout.contains("_br"),
        "zsh completions should have compdef or _br function"
    );
    info!("e2e_completions_zsh_generates_valid_script: done");
}

#[test]
fn e2e_completions_zsh_contains_subcommands() {
    common::init_test_logging();
    info!("e2e_completions_zsh_contains_subcommands: start");
    let workspace = BrWorkspace::new();

    let completions = run_br(
        &workspace,
        ["completions", "zsh"],
        "completions_zsh_subcommands",
    );
    assert!(
        completions.status.success(),
        "completions failed: {}",
        completions.stderr
    );

    assert_contains_subcommands(&completions.stdout, "zsh");
    info!("e2e_completions_zsh_contains_subcommands: done");
}

// =============================================================================
// Fish Completions Tests
// =============================================================================

#[test]
fn e2e_completions_fish_generates_valid_script() {
    common::init_test_logging();
    info!("e2e_completions_fish_generates_valid_script: start");
    // Generate fish completions and verify structure
    let workspace = BrWorkspace::new();

    let completions = run_br(&workspace, ["completions", "fish"], "completions_fish");
    assert!(
        completions.status.success(),
        "completions fish failed: {}",
        completions.stderr
    );

    // Fish completions use 'complete' command
    assert!(
        completions.stdout.contains("complete"),
        "fish completions should use 'complete' command"
    );
    info!("e2e_completions_fish_generates_valid_script: done");
}

#[test]
fn e2e_completions_fish_contains_subcommands() {
    common::init_test_logging();
    info!("e2e_completions_fish_contains_subcommands: start");
    let workspace = BrWorkspace::new();

    let completions = run_br(
        &workspace,
        ["completions", "fish"],
        "completions_fish_subcommands",
    );
    assert!(
        completions.status.success(),
        "completions failed: {}",
        completions.stderr
    );

    assert_contains_subcommands(&completions.stdout, "fish");
    info!("e2e_completions_fish_contains_subcommands: done");
}

// =============================================================================
// PowerShell Completions Tests
// =============================================================================

#[test]
fn e2e_completions_powershell_generates_valid_script() {
    common::init_test_logging();
    info!("e2e_completions_powershell_generates_valid_script: start");
    // Generate PowerShell completions and verify structure
    let workspace = BrWorkspace::new();

    let completions = run_br(
        &workspace,
        ["completions", "powershell"],
        "completions_powershell",
    );
    assert!(
        completions.status.success(),
        "completions powershell failed: {}",
        completions.stderr
    );

    // PowerShell completions should register argument completer
    assert!(
        completions.stdout.contains("Register-ArgumentCompleter")
            || completions.stdout.contains("$scriptBlock"),
        "powershell completions should have argument completer"
    );
    info!("e2e_completions_powershell_generates_valid_script: done");
}

#[test]
fn e2e_completions_powershell_contains_subcommands() {
    common::init_test_logging();
    info!("e2e_completions_powershell_contains_subcommands: start");
    let workspace = BrWorkspace::new();

    let completions = run_br(
        &workspace,
        ["completions", "powershell"],
        "completions_powershell_subcommands",
    );
    assert!(
        completions.status.success(),
        "completions failed: {}",
        completions.stderr
    );

    assert_contains_subcommands(&completions.stdout, "powershell");
    info!("e2e_completions_powershell_contains_subcommands: done");
}

// =============================================================================
// Elvish Completions Tests
// =============================================================================

#[test]
fn e2e_completions_elvish_generates_valid_script() {
    common::init_test_logging();
    info!("e2e_completions_elvish_generates_valid_script: start");
    // Generate elvish completions and verify structure
    let workspace = BrWorkspace::new();

    let completions = run_br(&workspace, ["completions", "elvish"], "completions_elvish");
    assert!(
        completions.status.success(),
        "completions elvish failed: {}",
        completions.stderr
    );

    // Elvish completions should have edit:completion or set edit:
    assert!(
        completions.stdout.contains("edit:") || completions.stdout.contains("set edit:"),
        "elvish completions should have edit: namespace"
    );
    info!("e2e_completions_elvish_generates_valid_script: done");
}

// =============================================================================
// Edge Case Tests
// =============================================================================

#[test]
fn e2e_completions_unknown_shell_error() {
    common::init_test_logging();
    info!("e2e_completions_unknown_shell_error: start");
    // Unknown shell should result in error
    let workspace = BrWorkspace::new();

    let completions = run_br(&workspace, ["completions", "csh"], "completions_unknown");
    assert!(
        !completions.status.success(),
        "completions for unknown shell should fail"
    );
    info!("e2e_completions_unknown_shell_error: done");
}

#[test]
fn e2e_completions_idempotent() {
    common::init_test_logging();
    info!("e2e_completions_idempotent: start");
    // Running completions twice should produce identical output
    let workspace = BrWorkspace::new();

    let run1 = run_br(&workspace, ["completions", "bash"], "completions_idem_1");
    let run2 = run_br(&workspace, ["completions", "bash"], "completions_idem_2");

    assert!(run1.status.success(), "run1 failed: {}", run1.stderr);
    assert!(run2.status.success(), "run2 failed: {}", run2.stderr);
    assert_eq!(run1.stdout, run2.stdout, "completions should be idempotent");
    info!("e2e_completions_idempotent: done");
}

#[test]
fn e2e_completions_no_workspace_required() {
    common::init_test_logging();
    info!("e2e_completions_no_workspace_required: start");
    // Completions should work without an initialized workspace
    let workspace = BrWorkspace::new();
    // Deliberately NOT calling init_workspace

    let completions = run_br(
        &workspace,
        ["completions", "bash"],
        "completions_no_workspace",
    );
    assert!(
        completions.status.success(),
        "completions should work without initialized workspace: {}",
        completions.stderr
    );
    info!("e2e_completions_no_workspace_required: done");
}

#[test]
fn e2e_completions_with_initialized_workspace() {
    common::init_test_logging();
    info!("e2e_completions_with_initialized_workspace: start");
    // Completions should also work with an initialized workspace
    let workspace = BrWorkspace::new();
    init_workspace(&workspace);

    let completions = run_br(
        &workspace,
        ["completions", "bash"],
        "completions_with_workspace",
    );
    assert!(
        completions.status.success(),
        "completions should work with initialized workspace: {}",
        completions.stderr
    );
    info!("e2e_completions_with_initialized_workspace: done");
}

// =============================================================================
// All Shells Consistency Tests
// =============================================================================

#[test]
fn e2e_completions_all_shells_succeed() {
    common::init_test_logging();
    info!("e2e_completions_all_shells_succeed: start");
    // All supported shells should generate completions successfully
    let workspace = BrWorkspace::new();
    let shells = ["bash", "zsh", "fish", "powershell", "elvish"];

    for shell in shells {
        let completions = run_br(
            &workspace,
            ["completions", shell],
            &format!("completions_{shell}"),
        );
        assert!(
            completions.status.success(),
            "completions for {shell} failed: {}",
            completions.stderr
        );
        assert!(
            !completions.stdout.is_empty(),
            "completions for {shell} should produce output"
        );
    }
    info!("e2e_completions_all_shells_succeed: done");
}

#[test]
fn e2e_completions_all_shells_have_help() {
    common::init_test_logging();
    info!("e2e_completions_all_shells_have_help: start");
    // All shell completions should include --help descriptions
    let workspace = BrWorkspace::new();
    let shells = ["bash", "zsh", "fish", "powershell", "elvish"];

    for shell in shells {
        let completions = run_br(
            &workspace,
            ["completions", shell],
            &format!("completions_{shell}_help"),
        );
        assert!(
            completions.status.success(),
            "completions for {shell} failed"
        );
        // All completions should mention help somewhere
        assert!(
            completions.stdout.to_lowercase().contains("help"),
            "completions for {shell} should reference help"
        );
    }
    info!("e2e_completions_all_shells_have_help: done");
}
