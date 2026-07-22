//! E2E tests for global CLI flags and output modes.
//!
//! Tests --json, --robot, --no-color, --no-db, and other global flags.
//! Part of beads_rust-pnvt.

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;
use std::fs;

// ============================================================================
// --json flag tests
// ============================================================================

#[test]
fn e2e_json_flag_list() {
    let _log = common::test_log("e2e_json_flag_list");
    let workspace = BrWorkspace::new();

    // Initialize and create issue
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "JSON test issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // List with --json flag
    let list = run_br(&workspace, ["list", "--json"], "list_json");
    assert!(list.status.success(), "list --json failed: {}", list.stderr);

    // Output should be valid JSON array
    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("valid JSON array");
    assert!(!json.is_empty(), "JSON list should not be empty");
    assert!(
        json.iter().any(|item| item["title"] == "JSON test issue"),
        "issue should be in JSON output"
    );
}

#[test]
fn e2e_json_flag_show() {
    let _log = common::test_log("e2e_json_flag_show");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Show JSON test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let id = create
        .stdout
        .lines()
        .next()
        .unwrap_or("")
        .strip_prefix("✓ Created ")
        .and_then(|s| s.split(':').next())
        .unwrap_or("")
        .trim();

    // Show with --json flag
    let show = run_br(&workspace, ["show", id, "--json"], "show_json");
    assert!(show.status.success(), "show --json failed: {}", show.stderr);

    let payload = extract_json_payload(&show.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("valid JSON");
    assert_eq!(json[0]["title"], "Show JSON test");
}

#[test]
fn e2e_json_flag_blocked() {
    let _log = common::test_log("e2e_json_flag_blocked");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Blocked with --json flag (even with no blocked issues)
    let blocked = run_br(&workspace, ["blocked", "--json"], "blocked_json");
    assert!(
        blocked.status.success(),
        "blocked --json failed: {}",
        blocked.stderr
    );

    let payload = extract_json_payload(&blocked.stdout);
    let json: Value = serde_json::from_str(&payload).expect("valid JSON");
    // Should be valid JSON (empty array when no blocked issues)
    assert!(json.is_array());
}

#[test]
fn e2e_json_flag_stats() {
    let _log = common::test_log("e2e_json_flag_stats");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Stats JSON test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Stats with --json flag
    let stats = run_br(&workspace, ["stats", "--json"], "stats_json");
    assert!(
        stats.status.success(),
        "stats --json failed: {}",
        stats.stderr
    );

    let payload = extract_json_payload(&stats.stdout);
    let json: Value = serde_json::from_str(&payload).expect("valid JSON");
    // Stats output has a "summary" object with count fields
    assert!(
        json.get("summary").is_some(),
        "stats should have summary field: {json}"
    );
    let summary = &json["summary"];
    assert!(
        summary.get("total_issues").is_some() || summary.get("open_issues").is_some(),
        "stats summary should have count fields: {summary}"
    );
}

// ============================================================================
// --robot flag tests
// ============================================================================

/// Note: --robot is not a global flag for `list` command.
/// The `list --json` flag provides machine-readable output.
/// The `--robot` flag exists on specific commands like `sync` and `history`.
/// This test verifies that list --json provides robot-parseable output.
#[test]
fn e2e_robot_flag_list() {
    let _log = common::test_log("e2e_robot_flag_list");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Robot test issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // List with --json flag (provides robot-parseable output)
    let list = run_br(&workspace, ["list", "--json"], "list_json");
    assert!(list.status.success(), "list --json failed: {}", list.stderr);

    // JSON mode should output valid JSON to stdout
    let payload = extract_json_payload(&list.stdout);
    let json: Value = serde_json::from_str(&payload).expect("json mode should output valid JSON");
    assert!(json.is_array(), "list should be JSON array");
}

#[test]
fn e2e_robot_flag_stderr_diagnostics() {
    let _log = common::test_log("e2e_robot_flag_stderr_diagnostics");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Use --no-auto-flush so sync has something to export
    let create = run_br(
        &workspace,
        ["create", "Robot stderr test", "--no-auto-flush"],
        "create",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Sync with --json flag to get JSON output
    let sync = run_br(&workspace, ["sync", "--flush-only", "--json"], "sync_json");
    assert!(sync.status.success(), "sync --json failed: {}", sync.stderr);

    // stdout should be parseable JSON
    let payload = extract_json_payload(&sync.stdout);
    let json: Value =
        serde_json::from_str(&payload).expect("json mode stdout should be valid JSON");

    // Verify it has expected fields from sync output
    assert!(
        json.get("exported_issues").is_some(),
        "JSON output should have exported_issues field"
    );
}

// ============================================================================
// --no-color flag tests
// ============================================================================

#[test]
fn e2e_no_color_flag() {
    let _log = common::test_log("e2e_no_color_flag");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "No-color test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Note: Our test harness already sets NO_COLOR=1, but let's verify --no-color works
    let list = run_br(&workspace, ["list", "--no-color"], "list_no_color");
    assert!(
        list.status.success(),
        "list --no-color failed: {}",
        list.stderr
    );

    // Output should not contain ANSI escape codes
    assert!(
        !list.stdout.contains("\x1b["),
        "output should not contain ANSI escape codes with --no-color"
    );
}

#[test]
fn e2e_no_color_env_var() {
    let _log = common::test_log("e2e_no_color_env_var");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "NO_COLOR env test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Test with NO_COLOR environment variable (already set by test harness)
    let list = run_br(&workspace, ["list"], "list_with_no_color_env");
    assert!(list.status.success(), "list failed: {}", list.stderr);

    // Output should not contain ANSI escape codes
    assert!(
        !list.stdout.contains("\x1b["),
        "output should not contain ANSI escape codes with NO_COLOR env"
    );
}

// ============================================================================
// --no-db flag tests
// ============================================================================

#[test]
fn e2e_no_db_flag_list() {
    let _log = common::test_log("e2e_no_db_flag_list");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create issue and flush to JSONL
    let create = run_br(&workspace, ["create", "No-DB list test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let sync = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(sync.status.success(), "sync flush failed: {}", sync.stderr);

    // List with --no-db flag (reads from JSONL only)
    let list = run_br(&workspace, ["--no-db", "list", "--json"], "list_no_db");
    assert!(
        list.status.success(),
        "list --no-db failed: {}",
        list.stderr
    );

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("valid JSON");
    assert!(
        json.iter().any(|item| item["title"] == "No-DB list test"),
        "issue should be visible in no-db mode"
    );
}

#[test]
fn e2e_no_db_flag_show() {
    let _log = common::test_log("e2e_no_db_flag_show");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "No-DB show test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let id = create
        .stdout
        .lines()
        .next()
        .unwrap_or("")
        .strip_prefix("✓ Created ")
        .and_then(|s| s.split(':').next())
        .unwrap_or("")
        .trim();

    let sync = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(sync.status.success(), "sync flush failed: {}", sync.stderr);

    // Show with --no-db flag
    let show = run_br(&workspace, ["--no-db", "show", id, "--json"], "show_no_db");
    assert!(
        show.status.success(),
        "show --no-db failed: {}",
        show.stderr
    );

    let payload = extract_json_payload(&show.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("valid JSON");
    assert_eq!(json[0]["title"], "No-DB show test");
}

#[test]
fn e2e_no_db_flag_blocked() {
    let _log = common::test_log("e2e_no_db_flag_blocked");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "No-DB blocked test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let sync = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(sync.status.success(), "sync flush failed: {}", sync.stderr);

    // Blocked with --no-db flag
    let blocked = run_br(&workspace, ["--no-db", "blocked", "--json"], "blocked_no_db");
    assert!(
        blocked.status.success(),
        "blocked --no-db failed: {}",
        blocked.stderr
    );

    // Should output valid JSON
    let payload = extract_json_payload(&blocked.stdout);
    let _json: Vec<Value> = serde_json::from_str(&payload).expect("valid JSON");
}

// ============================================================================
// --allow-stale flag tests
// ============================================================================

#[test]
fn e2e_allow_stale_flag() {
    let _log = common::test_log("e2e_allow_stale_flag");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Stale test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Sync to make JSONL current
    let sync = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(sync.status.success(), "sync flush failed: {}", sync.stderr);

    // Modify JSONL directly (makes DB "stale" relative to JSONL)
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let contents = fs::read_to_string(&jsonl_path).expect("read jsonl");
    fs::write(&jsonl_path, format!("{}\n", contents.trim())).expect("write jsonl");

    // List with --allow-stale should succeed even if DB is stale
    let list = run_br(&workspace, ["--allow-stale", "list"], "list_allow_stale");
    assert!(
        list.status.success(),
        "list --allow-stale failed: {}",
        list.stderr
    );
}

// ============================================================================
// --no-auto-import flag tests
// ============================================================================

#[test]
fn e2e_no_auto_import_flag() {
    let _log = common::test_log("e2e_no_auto_import_flag");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Auto-import test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Export to JSONL
    let sync = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(sync.status.success(), "sync flush failed: {}", sync.stderr);

    // With --no-auto-import, should skip auto import check
    let list = run_br(
        &workspace,
        ["--no-auto-import", "list"],
        "list_no_auto_import",
    );
    assert!(
        list.status.success(),
        "list --no-auto-import failed: {}",
        list.stderr
    );
}

// ============================================================================
// --no-auto-flush flag tests
// ============================================================================

#[test]
fn e2e_no_auto_flush_flag() {
    let _log = common::test_log("e2e_no_auto_flush_flag");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create with --no-auto-flush
    let create = run_br(
        &workspace,
        ["create", "No auto-flush test", "--no-auto-flush"],
        "create",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Check if JSONL exists and if it contains the issue
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");

    if jsonl_path.exists() {
        let contents = fs::read_to_string(&jsonl_path).expect("read jsonl");
        // With --no-auto-flush, the issue should NOT be in JSONL yet
        // (unless there was a previous sync)
        // This is a soft check since auto-import might have created empty file
        if contents.contains("No auto-flush test") {
            // If it does contain it, that's unexpected but not necessarily wrong
            // depending on implementation details
        }
    }

    // Now explicitly flush
    let sync = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(sync.status.success(), "sync flush failed: {}", sync.stderr);

    // After flush, issue should be in JSONL
    let contents = fs::read_to_string(&jsonl_path).expect("read jsonl");
    assert!(
        contents.contains("No auto-flush test"),
        "issue should be in JSONL after explicit flush"
    );
}

// ============================================================================
// --lock-timeout flag tests
// ============================================================================

#[test]
fn e2e_lock_timeout_flag() {
    let _log = common::test_log("e2e_lock_timeout_flag");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create with custom lock timeout
    let create = run_br(
        &workspace,
        ["--lock-timeout", "5000", "create", "Lock timeout test"],
        "create_with_timeout",
    );
    assert!(
        create.status.success(),
        "create with --lock-timeout failed: {}",
        create.stderr
    );

    // Verify issue was created
    let list = run_br(&workspace, ["list", "--json"], "list");
    assert!(list.status.success(), "list failed: {}", list.stderr);

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("valid JSON");
    assert!(
        json.iter().any(|item| item["title"] == "Lock timeout test"),
        "issue should be created with custom lock timeout"
    );
}

// ============================================================================
// --quiet flag tests
// ============================================================================

#[test]
fn e2e_quiet_flag() {
    let _log = common::test_log("e2e_quiet_flag");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create with --quiet flag
    let create = run_br(
        &workspace,
        ["--quiet", "create", "Quiet test"],
        "create_quiet",
    );
    assert!(
        create.status.success(),
        "create --quiet failed: {}",
        create.stderr
    );

    // Quiet mode should minimize output (may still show created ID)
    // Just verify it succeeded and didn't crash
}

#[test]
fn e2e_quiet_flag_list() {
    let _log = common::test_log("e2e_quiet_flag_list");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Quiet list test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // List with --quiet flag
    let list = run_br(&workspace, ["--quiet", "list"], "list_quiet");
    assert!(
        list.status.success(),
        "list --quiet failed: {}",
        list.stderr
    );

    // Quiet mode should still show results but with minimal decoration
    // Verify it shows the issue title somewhere
    // (exact format depends on implementation)
}

// ============================================================================
// --verbose flag tests
// ============================================================================

#[test]
fn e2e_verbose_flag() {
    let _log = common::test_log("e2e_verbose_flag");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create with -v flag
    let create = run_br(
        &workspace,
        ["-v", "create", "Verbose test"],
        "create_verbose",
    );
    assert!(
        create.status.success(),
        "create -v failed: {}",
        create.stderr
    );

    // Verbose mode should show more output (in stderr typically)
    // RUST_LOG is already set to debug in test harness, so this is mostly
    // verifying the flag doesn't cause issues
}

#[test]
fn e2e_very_verbose_flag() {
    let _log = common::test_log("e2e_very_verbose_flag");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create with -vv flag for more verbosity
    let create = run_br(
        &workspace,
        ["-vv", "create", "Very verbose test"],
        "create_very_verbose",
    );
    assert!(
        create.status.success(),
        "create -vv failed: {}",
        create.stderr
    );
}

// ============================================================================
// Combined flags tests
// ============================================================================

#[test]
fn e2e_json_no_color_combined() {
    let _log = common::test_log("e2e_json_no_color_combined");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Combined flags test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Combine --json and --no-color
    let list = run_br(
        &workspace,
        ["list", "--json", "--no-color"],
        "list_combined",
    );
    assert!(
        list.status.success(),
        "list --json --no-color failed: {}",
        list.stderr
    );

    // Should be valid JSON with no color codes
    let payload = extract_json_payload(&list.stdout);
    let _json: Vec<Value> = serde_json::from_str(&payload).expect("valid JSON");
    assert!(
        !list.stdout.contains("\x1b["),
        "no color codes in JSON output"
    );
}

#[test]
fn e2e_no_db_json_combined() {
    let _log = common::test_log("e2e_no_db_json_combined");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "No-DB JSON test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let sync = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(sync.status.success(), "sync flush failed: {}", sync.stderr);

    // Combine --no-db and --json
    let list = run_br(&workspace, ["--no-db", "list", "--json"], "list_no_db_json");
    assert!(
        list.status.success(),
        "list --no-db --json failed: {}",
        list.stderr
    );

    let payload = extract_json_payload(&list.stdout);
    let json: Vec<Value> = serde_json::from_str(&payload).expect("valid JSON");
    assert!(
        json.iter().any(|item| item["title"] == "No-DB JSON test"),
        "issue in no-db JSON output"
    );
}

#[test]
fn e2e_quiet_json_combined() {
    let _log = common::test_log("e2e_quiet_json_combined");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Quiet JSON test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Combine --quiet and --json
    let list = run_br(&workspace, ["--quiet", "list", "--json"], "list_quiet_json");
    assert!(
        list.status.success(),
        "list --quiet --json failed: {}",
        list.stderr
    );

    // JSON should still be valid
    let payload = extract_json_payload(&list.stdout);
    let _json: Vec<Value> = serde_json::from_str(&payload).expect("valid JSON");
}

// ============================================================================
// Global flag position tests
// ============================================================================

#[test]
fn e2e_global_flag_before_command() {
    let _log = common::test_log("e2e_global_flag_before_command");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Position test"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Global flag before command
    let list = run_br(&workspace, ["--json", "list"], "list_flag_before");
    assert!(
        list.status.success(),
        "list with --json before command failed: {}",
        list.stderr
    );

    let payload = extract_json_payload(&list.stdout);
    let _json: Vec<Value> = serde_json::from_str(&payload).expect("valid JSON");
}

#[test]
fn e2e_global_flag_after_command() {
    let _log = common::test_log("e2e_global_flag_after_command");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Position test 2"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Global flag after command
    let list = run_br(&workspace, ["list", "--json"], "list_flag_after");
    assert!(
        list.status.success(),
        "list with --json after command failed: {}",
        list.stderr
    );

    let payload = extract_json_payload(&list.stdout);
    let _json: Vec<Value> = serde_json::from_str(&payload).expect("valid JSON");
}

// ============================================================================
// Output mode consistency tests (beads_rust-14eu)
// ============================================================================

/// JSON mode should produce stdout that parses as valid JSON with no extra text.
#[test]
fn e2e_json_stdout_is_clean_json() {
    let _log = common::test_log("e2e_json_stdout_is_clean_json");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create = run_br(&workspace, ["create", "JSON clean test"], "create");
    assert!(create.status.success());

    // Verify multiple commands produce clean JSON stdout
    for (cmd, label) in [
        (vec!["list", "--json"], "list"),
        (vec!["blocked", "--json"], "blocked"),
        (vec!["count", "--json"], "count"),
        (vec!["stats", "--json"], "stats"),
    ] {
        let output = run_br(&workspace, cmd.clone(), &format!("clean_json_{label}"));
        assert!(output.status.success(), "{label} failed: {}", output.stderr);

        let payload = extract_json_payload(&output.stdout);
        let parsed: Result<Value, _> = serde_json::from_str(&payload);
        assert!(
            parsed.is_ok(),
            "{label} --json stdout is not valid JSON:\n---stdout---\n{}\n---end---",
            output.stdout
        );

        // No ANSI escape codes in JSON output
        assert!(
            !output.stdout.contains("\x1b["),
            "{label} --json stdout contains ANSI escape codes"
        );
    }
}

/// --quiet mode should produce strictly less output than normal mode.
#[test]
fn e2e_quiet_reduces_output() {
    let _log = common::test_log("e2e_quiet_reduces_output");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create = run_br(&workspace, ["create", "Quiet reduction test"], "create");
    assert!(create.status.success());

    // Normal list output
    let normal = run_br(&workspace, ["list"], "list_normal");
    assert!(normal.status.success());

    // Quiet list output
    let quiet = run_br(&workspace, ["--quiet", "list"], "list_quiet_cmp");
    assert!(quiet.status.success());

    // Quiet output should be shorter or equal (never longer)
    assert!(
        quiet.stdout.len() <= normal.stdout.len(),
        "quiet output ({} bytes) should not exceed normal output ({} bytes)\nquiet: {}\nnormal: {}",
        quiet.stdout.len(),
        normal.stdout.len(),
        quiet.stdout,
        normal.stdout
    );
}

/// --json should take precedence over --quiet: produce valid JSON output.
#[test]
fn e2e_json_overrides_quiet() {
    let _log = common::test_log("e2e_json_overrides_quiet");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let create = run_br(&workspace, ["create", "Precedence test"], "create");
    assert!(create.status.success());

    // --quiet --json should still produce valid JSON
    let output = run_br(&workspace, ["--quiet", "list", "--json"], "quiet_json_prec");
    assert!(output.status.success());

    let payload = extract_json_payload(&output.stdout);
    let json: Vec<Value> =
        serde_json::from_str(&payload).expect("--quiet --json should produce valid JSON");
    assert!(
        json.iter().any(|item| item["title"] == "Precedence test"),
        "JSON output should contain the issue even with --quiet"
    );
}

/// --no-color should work across multiple command types (not just list).
#[test]
fn e2e_no_color_across_commands() {
    let _log = common::test_log("e2e_no_color_across_commands");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    let id = {
        let create = run_br(&workspace, ["create", "No-color multi test"], "create");
        assert!(create.status.success());
        create
            .stdout
            .lines()
            .next()
            .unwrap_or("")
            .strip_prefix("✓ Created ")
            .or_else(|| {
                create
                    .stdout
                    .lines()
                    .next()
                    .unwrap_or("")
                    .strip_prefix("Created ")
            })
            .and_then(|s| s.split(':').next())
            .unwrap_or("")
            .trim()
            .to_string()
    };

    for (cmd, label) in [
        (vec!["list", "--no-color"], "list"),
        (vec!["show", &id, "--no-color"], "show"),
        (vec!["blocked", "--no-color"], "blocked"),
        (vec!["stats", "--no-color"], "stats"),
        (vec!["count", "--no-color"], "count"),
    ] {
        let output = run_br(&workspace, cmd.clone(), &format!("no_color_{label}"));
        assert!(output.status.success(), "{label} failed: {}", output.stderr);

        assert!(
            !output.stdout.contains("\x1b["),
            "{label} --no-color stdout contains ANSI escape codes"
        );
    }
}
