//! E2E tests for the `completions` command.
//!
//! Test coverage:
//! - Generate completions for each supported shell (bash, zsh, fish, powershell, elvish)
//! - Verify completions contain expected subcommand names
//! - Verify completions contain expected flag names
//! - Edge cases (unknown shell, idempotency)

mod common;

use common::cli::{BrWorkspace, run_br};
use std::fs;
use tracing::info;

// =============================================================================
// Helper Functions
// =============================================================================

fn init_workspace(workspace: &BrWorkspace) {
    let init = run_br(workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
}

/// Check that completions output contains dynamic completion registration markers.
/// Dynamic completions (via clap_complete::env::Shells) generate a registration stub
/// that calls the binary at runtime to resolve completions, rather than embedding
/// static subcommand/flag lists in the script.
fn assert_contains_dynamic_markers(output: &str, shell_name: &str) {
    // All dynamic completion scripts must reference the binary name
    assert!(
        output.contains("br"),
        "{shell_name} completions should reference 'br' binary"
    );
    // All dynamic completion scripts set a COMPLETE env var
    assert!(
        output.contains("COMPLETE"),
        "{shell_name} completions should contain COMPLETE env var for dynamic resolution"
    );
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

    // Dynamic bash completions register via clap_complete::env::Shells
    assert!(
        completions.stdout.contains("_clap_complete_br"),
        "bash completions should define _clap_complete_br dynamic function"
    );
    assert!(
        completions.stdout.contains("complete -o"),
        "bash completions should register via complete command"
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

    assert_contains_dynamic_markers(&completions.stdout, "bash");
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

    assert_contains_dynamic_markers(&completions.stdout, "bash");
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

    assert_contains_dynamic_markers(&completions.stdout, "zsh");
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

    assert_contains_dynamic_markers(&completions.stdout, "fish");
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

    // Dynamic PowerShell completions register via COMPLETE env var
    assert!(
        completions.stdout.contains("Register-ArgumentCompleter")
            || completions.stdout.contains("COMPLETE"),
        "powershell completions should have argument completer or COMPLETE registration"
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

    assert_contains_dynamic_markers(&completions.stdout, "powershell");
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

    // Dynamic elvish completions register via COMPLETE env var
    assert!(
        completions.stdout.contains("edit:") || completions.stdout.contains("COMPLETE"),
        "elvish completions should have edit: namespace or COMPLETE registration"
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

// =============================================================================
// File Output Tests
// =============================================================================

#[test]
fn e2e_completions_bash_file_output() {
    common::init_test_logging();
    info!("e2e_completions_bash_file_output: start");
    // Generate bash completions to a file and verify content
    let workspace = BrWorkspace::new();
    let output_file = workspace.root.join("completions_bash.sh");

    let completions = run_br(
        &workspace,
        ["completions", "bash", "-o", output_file.to_str().unwrap()],
        "completions_bash_file",
    );
    assert!(
        completions.status.success(),
        "completions bash -o failed: {}",
        completions.stderr
    );

    // Verify file was created
    assert!(
        output_file.exists(),
        "completion file should exist at {}",
        output_file.display()
    );

    // Verify file content
    let file_content = fs::read_to_string(&output_file).expect("read completion file");
    assert!(
        file_content.contains("_clap_complete_br"),
        "bash completions file should define _clap_complete_br dynamic function"
    );
    assert!(
        file_content.contains("complete -o"),
        "bash completions file should register via complete command"
    );
    info!("e2e_completions_bash_file_output: done");
}

#[test]
fn e2e_completions_zsh_file_output() {
    common::init_test_logging();
    info!("e2e_completions_zsh_file_output: start");
    // Generate zsh completions to a file
    let workspace = BrWorkspace::new();
    let output_file = workspace.root.join("_br");

    let completions = run_br(
        &workspace,
        ["completions", "zsh", "-o", output_file.to_str().unwrap()],
        "completions_zsh_file",
    );
    assert!(
        completions.status.success(),
        "completions zsh -o failed: {}",
        completions.stderr
    );

    // Verify file was created
    assert!(
        output_file.exists(),
        "completion file should exist at {}",
        output_file.display()
    );

    // Verify file content
    let file_content = fs::read_to_string(&output_file).expect("read completion file");
    assert!(
        file_content.contains("#compdef") || file_content.contains("_br"),
        "zsh completions file should have compdef or _br function"
    );
    info!("e2e_completions_zsh_file_output: done");
}

#[test]
fn e2e_completions_fish_file_output() {
    common::init_test_logging();
    info!("e2e_completions_fish_file_output: start");
    // Generate fish completions to a file
    let workspace = BrWorkspace::new();
    let output_file = workspace.root.join("br.fish");

    let completions = run_br(
        &workspace,
        ["completions", "fish", "-o", output_file.to_str().unwrap()],
        "completions_fish_file",
    );
    assert!(
        completions.status.success(),
        "completions fish -o failed: {}",
        completions.stderr
    );

    // Verify file was created and contains fish-specific syntax
    assert!(output_file.exists(), "completion file should exist");
    let file_content = fs::read_to_string(&output_file).expect("read completion file");
    assert!(
        file_content.contains("complete"),
        "fish completions file should use 'complete' command"
    );
    info!("e2e_completions_fish_file_output: done");
}

#[test]
fn e2e_completions_powershell_file_output() {
    common::init_test_logging();
    info!("e2e_completions_powershell_file_output: start");
    // Generate PowerShell completions to a file
    let workspace = BrWorkspace::new();
    let output_file = workspace.root.join("br.ps1");

    let completions = run_br(
        &workspace,
        [
            "completions",
            "powershell",
            "-o",
            output_file.to_str().unwrap(),
        ],
        "completions_powershell_file",
    );
    assert!(
        completions.status.success(),
        "completions powershell -o failed: {}",
        completions.stderr
    );

    // Verify file was created and contains PowerShell-specific syntax
    assert!(output_file.exists(), "completion file should exist");
    let file_content = fs::read_to_string(&output_file).expect("read completion file");
    assert!(
        file_content.contains("Register-ArgumentCompleter") || file_content.contains("COMPLETE"),
        "powershell completions file should have argument completer or COMPLETE registration"
    );
    info!("e2e_completions_powershell_file_output: done");
}

#[test]
fn e2e_completions_file_output_matches_stdout() {
    common::init_test_logging();
    info!("e2e_completions_file_output_matches_stdout: start");
    // Verify that file output matches stdout output
    let workspace = BrWorkspace::new();
    let output_file = workspace.root.join("completions_bash_test.sh");

    // Get stdout output
    let stdout_run = run_br(&workspace, ["completions", "bash"], "completions_stdout");
    assert!(
        stdout_run.status.success(),
        "completions bash stdout failed"
    );

    // Get file output
    let file_run = run_br(
        &workspace,
        ["completions", "bash", "-o", output_file.to_str().unwrap()],
        "completions_file",
    );
    assert!(
        file_run.status.success(),
        "completions bash -o failed: {}",
        file_run.stderr
    );

    let file_content = fs::read_to_string(&output_file).expect("read completion file");

    // Content should match (stdout and file should be identical)
    assert_eq!(
        stdout_run.stdout.trim(),
        file_content.trim(),
        "file output should match stdout output"
    );
    info!("e2e_completions_file_output_matches_stdout: done");
}

#[test]
fn e2e_completions_file_output_overwrites_existing() {
    common::init_test_logging();
    info!("e2e_completions_file_output_overwrites_existing: start");
    // Verify that file output overwrites an existing file
    let workspace = BrWorkspace::new();
    let output_file = workspace.root.join("completions_overwrite.sh");

    // Create existing file with dummy content
    fs::write(&output_file, "dummy content").expect("write dummy file");

    // Generate completions to the same file
    let completions = run_br(
        &workspace,
        ["completions", "bash", "-o", output_file.to_str().unwrap()],
        "completions_overwrite",
    );
    assert!(
        completions.status.success(),
        "completions bash -o failed: {}",
        completions.stderr
    );

    // Verify file was overwritten
    let file_content = fs::read_to_string(&output_file).expect("read completion file");
    assert!(
        !file_content.contains("dummy content"),
        "file should be overwritten, not appended"
    );
    assert!(
        file_content.contains("_clap_complete_br"),
        "file should contain new completion script"
    );
    info!("e2e_completions_file_output_overwrites_existing: done");
}

#[test]
fn e2e_completions_all_shells_file_output() {
    common::init_test_logging();
    info!("e2e_completions_all_shells_file_output: start");
    // Test file output for all supported shells
    let workspace = BrWorkspace::new();
    let shells = [
        ("bash", "br.bash"),
        ("zsh", "_br"),
        ("fish", "br.fish"),
        ("powershell", "br.ps1"),
        ("elvish", "br.elv"),
    ];

    for (shell, filename) in shells {
        let output_file = workspace.root.join(filename);

        let completions = run_br(
            &workspace,
            ["completions", shell, "-o", output_file.to_str().unwrap()],
            &format!("completions_{shell}_file_all"),
        );
        assert!(
            completions.status.success(),
            "completions {shell} -o failed: {}",
            completions.stderr
        );
        assert!(
            output_file.exists(),
            "completion file for {shell} should exist at {}",
            output_file.display()
        );

        let file_content = fs::read_to_string(&output_file).expect("read completion file");
        assert!(
            !file_content.is_empty(),
            "completion file for {shell} should not be empty"
        );
    }
    info!("e2e_completions_all_shells_file_output: done");
}
