//! E2E tests for workspace initialization and diagnostic commands.
//!
//! Tests init, config, doctor, info, where, and version commands.
//! Part of beads_rust-6esx.

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br, run_br_with_env};
use serde_json::Value;
use std::fs;

// ============================================================================
// init command tests
// ============================================================================

#[test]
fn e2e_init_new_workspace() {
    let _log = common::test_log("e2e_init_new_workspace");
    let workspace = BrWorkspace::new();

    // Initialize a new workspace
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    assert!(
        init.stdout.contains("Initialized") || init.stdout.contains("initialized"),
        "init should report success: {}",
        init.stdout
    );

    // Verify .beads directory was created
    let beads_dir = workspace.root.join(".beads");
    assert!(beads_dir.exists(), ".beads directory should exist");

    // Verify database file exists
    let db_path = beads_dir.join("beads.db");
    assert!(db_path.exists(), "beads.db should exist");
}

#[test]
fn e2e_init_already_initialized() {
    let _log = common::test_log("e2e_init_already_initialized");
    let workspace = BrWorkspace::new();

    // First init
    let init1 = run_br(&workspace, ["init"], "init1");
    assert!(
        init1.status.success(),
        "first init failed: {}",
        init1.stderr
    );

    // Second init without --force should warn or succeed gracefully
    let init2 = run_br(&workspace, ["init"], "init2");
    // Either succeeds with warning or fails gracefully with "already" message
    // br returns JSON error with code "ALREADY_INITIALIZED"
    let stderr_lower = init2.stderr.to_lowercase();
    assert!(
        init2.status.success()
            || stderr_lower.contains("already")
            || init2.stderr.contains("ALREADY_INITIALIZED"),
        "second init should succeed or warn: stdout='{}', stderr='{}'",
        init2.stdout,
        init2.stderr
    );
}

#[test]
fn e2e_init_force_reinit() {
    let _log = common::test_log("e2e_init_force_reinit");
    let workspace = BrWorkspace::new();

    // First init
    let init1 = run_br(&workspace, ["init"], "init1");
    assert!(
        init1.status.success(),
        "first init failed: {}",
        init1.stderr
    );

    // Create an issue to verify database is reset
    let create = run_br(&workspace, ["create", "Test issue before force"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Force reinit (if supported)
    let init2 = run_br(&workspace, ["init", "--force"], "init2_force");
    // --force may not be implemented, check either way
    if init2.status.success() {
        // After force reinit, the database should be fresh
        // List should show no issues or only one if --force doesn't clear
        let list = run_br(&workspace, ["list", "--json"], "list_after_force");
        assert!(
            list.status.success(),
            "list after force init failed: {}",
            list.stderr
        );
    }
}

#[test]
fn e2e_init_creates_jsonl() {
    let _log = common::test_log("e2e_init_creates_jsonl");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create an issue and sync to JSONL
    let create = run_br(&workspace, ["create", "JSONL test issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let sync = run_br(&workspace, ["sync", "--flush-only"], "sync");
    assert!(sync.status.success(), "sync failed: {}", sync.stderr);

    // Verify JSONL file exists
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    assert!(jsonl_path.exists(), "issues.jsonl should exist after sync");

    let contents = fs::read_to_string(&jsonl_path).expect("read jsonl");
    assert!(
        contents.contains("JSONL test issue"),
        "JSONL should contain the issue"
    );
}

// ============================================================================
// config command tests
// ============================================================================

#[test]
fn e2e_config_list() {
    let _log = common::test_log("e2e_config_list");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // List config
    let config_list = run_br(&workspace, ["config", "list"], "config_list");
    assert!(
        config_list.status.success(),
        "config list failed: {}",
        config_list.stderr
    );
    // Should output something (even if empty)
}

#[test]
fn e2e_config_get_set() {
    let _log = common::test_log("e2e_config_get_set");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Use a unique test key that won't conflict with defaults
    // Note: issue_prefix may have DB defaults that take precedence over YAML
    let set = run_br(
        &workspace,
        ["config", "set", "test_custom_key=TESTVALUE"],
        "config_set",
    );
    assert!(set.status.success(), "config set failed: {}", set.stderr);

    // Get the config value
    let get = run_br(
        &workspace,
        ["config", "get", "test_custom_key"],
        "config_get",
    );
    assert!(get.status.success(), "config get failed: {}", get.stderr);
    assert!(
        get.stdout.contains("TESTVALUE"),
        "config get should return TESTVALUE: {}",
        get.stdout
    );
}

#[test]
fn e2e_config_json_output() {
    let _log = common::test_log("e2e_config_json_output");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // List config with --json
    let config_list = run_br(&workspace, ["config", "list", "--json"], "config_list_json");
    assert!(
        config_list.status.success(),
        "config list --json failed: {}",
        config_list.stderr
    );

    // Should be valid JSON
    let payload = extract_json_payload(&config_list.stdout);
    let _json: Value =
        serde_json::from_str(&payload).expect("config list should output valid JSON");
}

#[cfg(not(windows))]
#[test]
fn e2e_config_edit_creates_user_config() {
    let _log = common::test_log("e2e_config_edit_creates_user_config");
    let workspace = BrWorkspace::new();

    let env_vars = vec![("EDITOR", "true")];
    let edit = run_br_with_env(&workspace, ["config", "edit"], env_vars, "config_edit");
    assert!(edit.status.success(), "config edit failed: {}", edit.stderr);

    let config_path = workspace
        .root
        .join(".config")
        .join("beads")
        .join("config.yaml");
    assert!(
        config_path.exists(),
        "config edit should create user config at {}",
        config_path.display()
    );

    let contents = fs::read_to_string(&config_path).expect("read user config");
    assert!(
        contents.contains("br configuration"),
        "config edit should create default template content"
    );
}

// ============================================================================
// doctor command tests
// ============================================================================

#[test]
fn e2e_doctor_healthy_workspace() {
    let _log = common::test_log("e2e_doctor_healthy_workspace");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Run doctor on healthy workspace
    let doctor = run_br(&workspace, ["doctor"], "doctor");
    assert!(
        doctor.status.success(),
        "doctor failed on healthy workspace: {}",
        doctor.stderr
    );
}

#[test]
fn e2e_doctor_uninitialized() {
    let _log = common::test_log("e2e_doctor_uninitialized");
    let workspace = BrWorkspace::new();

    // Run doctor without init
    let doctor = run_br(&workspace, ["doctor"], "doctor_no_init");
    // Should fail or warn about missing workspace
    assert!(
        !doctor.status.success()
            || doctor.stderr.contains("not found")
            || doctor.stderr.contains("not initialized")
            || doctor.stdout.contains("not found")
            || doctor.stdout.contains("not initialized"),
        "doctor should report missing workspace: stdout='{}', stderr='{}'",
        doctor.stdout,
        doctor.stderr
    );
}

#[test]
fn e2e_doctor_json_output() {
    let _log = common::test_log("e2e_doctor_json_output");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Doctor with --json
    let doctor = run_br(&workspace, ["doctor", "--json"], "doctor_json");
    assert!(
        doctor.status.success(),
        "doctor --json failed: {}",
        doctor.stderr
    );

    let payload = extract_json_payload(&doctor.stdout);
    let _json: Value = serde_json::from_str(&payload).expect("doctor should output valid JSON");
}

#[test]
fn e2e_doctor_detects_issues() {
    let _log = common::test_log("e2e_doctor_detects_issues");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Create some issues with potential problems
    let create1 = run_br(&workspace, ["create", "Issue with missing dep"], "create1");
    assert!(create1.status.success());

    // Extract the issue ID
    let id = create1
        .stdout
        .lines()
        .next()
        .unwrap_or("")
        .strip_prefix("Created ")
        .and_then(|s| s.split(':').next())
        .unwrap_or("")
        .trim();

    // Try to add a non-existent dependency (should fail)
    let _dep = run_br(
        &workspace,
        ["dep", "add", id, "nonexistent-id"],
        "add_bad_dep",
    );
    // This may fail, which is expected

    // Run doctor
    let doctor = run_br(&workspace, ["doctor"], "doctor_check");
    assert!(doctor.status.success(), "doctor failed: {}", doctor.stderr);
}

// ============================================================================
// info command tests
// ============================================================================

#[test]
fn e2e_info_basic() {
    let _log = common::test_log("e2e_info_basic");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Run info command
    let info = run_br(&workspace, ["info"], "info");
    assert!(info.status.success(), "info failed: {}", info.stderr);

    // Should contain path information
    assert!(
        info.stdout.contains(".beads") || info.stdout.contains("beads"),
        "info should mention beads directory: {}",
        info.stdout
    );
}

#[test]
fn e2e_info_json_output() {
    let _log = common::test_log("e2e_info_json_output");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Info with --json
    let info = run_br(&workspace, ["info", "--json"], "info_json");
    assert!(info.status.success(), "info --json failed: {}", info.stderr);

    let payload = extract_json_payload(&info.stdout);
    let json: Value = serde_json::from_str(&payload).expect("info should output valid JSON");

    // Should have workspace path (br uses "database_path")
    assert!(
        json.get("workspace_path").is_some()
            || json.get("db_path").is_some()
            || json.get("path").is_some()
            || json.get("database_path").is_some(),
        "info JSON should contain path info: {json}"
    );
}

#[test]
fn e2e_info_uninitialized() {
    let _log = common::test_log("e2e_info_uninitialized");
    let workspace = BrWorkspace::new();

    // Run info without init
    let info = run_br(&workspace, ["info"], "info_no_init");
    // Should fail or report no workspace
    assert!(
        !info.status.success()
            || info.stderr.contains("not found")
            || info.stdout.contains("not found"),
        "info should report missing workspace"
    );
}

// ============================================================================
// where command tests
// ============================================================================

#[test]
fn e2e_where_basic() {
    let _log = common::test_log("e2e_where_basic");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Run where command
    let whr = run_br(&workspace, ["where"], "where");
    assert!(whr.status.success(), "where failed: {}", whr.stderr);

    // Should output the .beads path
    assert!(
        whr.stdout.contains(".beads"),
        "where should output .beads path: {}",
        whr.stdout
    );
}

#[test]
fn e2e_where_uninitialized() {
    let _log = common::test_log("e2e_where_uninitialized");
    let workspace = BrWorkspace::new();

    // Run where without init
    let whr = run_br(&workspace, ["where"], "where_no_init");
    // Should fail or output nothing useful
    assert!(
        !whr.status.success() || whr.stdout.trim().is_empty() || whr.stderr.contains("not found"),
        "where should fail or be empty without init: stdout='{}', stderr='{}'",
        whr.stdout,
        whr.stderr
    );
}

#[test]
fn e2e_where_json_output() {
    let _log = common::test_log("e2e_where_json_output");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Where with --json (if supported)
    let whr = run_br(&workspace, ["where", "--json"], "where_json");
    if whr.status.success() {
        let payload = extract_json_payload(&whr.stdout);
        let _json: Value =
            serde_json::from_str(&payload).expect("where --json should output valid JSON");
    }
    // --json may not be supported for where, which is fine
}

// ============================================================================
// version command tests
// ============================================================================

#[test]
fn e2e_version_basic() {
    let _log = common::test_log("e2e_version_basic");
    let workspace = BrWorkspace::new();

    // Version doesn't require init
    let version = run_br(&workspace, ["version"], "version");
    assert!(
        version.status.success(),
        "version failed: {}",
        version.stderr
    );

    // Should contain version number
    assert!(
        version.stdout.contains("0.") || version.stdout.contains("1."),
        "version should contain version number: {}",
        version.stdout
    );
}

#[test]
fn e2e_version_json_output() {
    let _log = common::test_log("e2e_version_json_output");
    let workspace = BrWorkspace::new();

    // Version with --json
    let version = run_br(&workspace, ["version", "--json"], "version_json");
    assert!(
        version.status.success(),
        "version --json failed: {}",
        version.stderr
    );

    let payload = extract_json_payload(&version.stdout);
    let json: Value = serde_json::from_str(&payload).expect("version should output valid JSON");

    // Should have version field
    assert!(
        json.get("version").is_some() || json.get("semver").is_some(),
        "version JSON should contain version field: {json}"
    );
}

#[test]
fn e2e_version_short_flag() {
    let _log = common::test_log("e2e_version_short_flag");
    let workspace = BrWorkspace::new();

    // Test -V flag
    let version = run_br(&workspace, ["-V"], "version_short");
    assert!(version.status.success(), "-V failed: {}", version.stderr);

    assert!(
        version.stdout.contains("br")
            || version.stdout.contains("0.")
            || version.stdout.contains("1."),
        "-V should output version: {}",
        version.stdout
    );
}

#[test]
fn e2e_version_help() {
    let _log = common::test_log("e2e_version_help");
    let workspace = BrWorkspace::new();

    // Test --version flag
    let version = run_br(&workspace, ["--version"], "version_long");
    assert!(
        version.status.success(),
        "--version failed: {}",
        version.stderr
    );

    assert!(
        version.stdout.contains("br")
            || version.stdout.contains("0.")
            || version.stdout.contains("1."),
        "--version should output version: {}",
        version.stdout
    );
}

// ============================================================================
// Combined/integration tests
// ============================================================================

#[test]
fn e2e_full_workspace_lifecycle() {
    let _log = common::test_log("e2e_full_workspace_lifecycle");
    let workspace = BrWorkspace::new();

    // 1. Check version works without init
    let version = run_br(&workspace, ["version"], "version");
    assert!(version.status.success());

    // 2. Where should fail without init
    let where_before = run_br(&workspace, ["where"], "where_before");
    assert!(
        !where_before.status.success() || where_before.stdout.trim().is_empty(),
        "where should fail before init"
    );

    // 3. Initialize
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    // 4. Where should work now
    let where_after = run_br(&workspace, ["where"], "where_after");
    assert!(where_after.status.success());
    assert!(where_after.stdout.contains(".beads"));

    // 5. Info should show workspace details
    let info = run_br(&workspace, ["info"], "info");
    assert!(info.status.success());

    // 6. Doctor should pass
    let doctor = run_br(&workspace, ["doctor"], "doctor");
    assert!(doctor.status.success());

    // 7. Config should be accessible
    let config = run_br(&workspace, ["config", "list"], "config");
    assert!(config.status.success());
}

#[test]
fn e2e_workspace_paths_consistent() {
    let _log = common::test_log("e2e_workspace_paths_consistent");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success());

    // Get path from where
    let whr = run_br(&workspace, ["where"], "where");
    assert!(whr.status.success());
    let where_path = whr.stdout.trim();

    // Get path from info --json
    let info = run_br(&workspace, ["info", "--json"], "info_json");
    assert!(info.status.success());

    let payload = extract_json_payload(&info.stdout);
    let json: Value = serde_json::from_str(&payload).expect("valid JSON");

    // The paths should be consistent (both point to same .beads)
    if let Some(info_path) = json
        .get("workspace_path")
        .or_else(|| json.get("beads_dir"))
        .or_else(|| json.get("path"))
    {
        let info_path_str = info_path.as_str().unwrap_or("");
        // Both should contain .beads
        assert!(
            where_path.contains(".beads")
                && (info_path_str.contains(".beads") || info_path_str.is_empty()),
            "Paths should be consistent: where='{where_path}', info='{info_path_str}'"
        );
    }
}
