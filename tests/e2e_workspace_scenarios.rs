//! E2E scenarios for workspace initialization and diagnostic commands.
//!
//! Coverage:
//! - init (new workspace, re-init handling)
//! - config get/set/list (validate precedence)
//! - doctor (read-only diagnostics)
//! - info + where (paths + metadata)
//! - version (json + text)
//!
//! Uses the new harness infrastructure for artifact logging.
//!
//! Task: beads_rust-6esx

mod common;

use common::harness::{TestWorkspace, extract_json_payload};
use serde_json::Value;

// =============================================================================
// Init Scenarios
// =============================================================================

#[test]
fn scenario_init_new_workspace() {
    let mut ws = TestWorkspace::new("e2e_workspace", "init_new");

    // Initialize a fresh workspace
    let init = ws.run_br(["init"], "init");
    init.assert_success();

    // Verify .beads directory was created
    let beads_dir = ws.root.join(".beads");
    assert!(
        beads_dir.exists(),
        ".beads directory should exist after init"
    );

    // Verify database was created
    let db_path = beads_dir.join("beads.db");
    assert!(db_path.exists(), "beads.db should exist after init");

    // Verify init output contains expected text
    assert!(
        init.stdout.contains("Initialized") || init.stdout.contains("initialized"),
        "init should confirm initialization: {}",
        init.stdout
    );

    ws.finish(true);
}

#[test]
fn scenario_init_reinit_rejected_without_force() {
    let mut ws = TestWorkspace::new("e2e_workspace", "init_reinit");

    // First init
    let init1 = ws.run_br(["init"], "init_first");
    init1.assert_success();

    // Create an issue to have some data
    let create = ws.run_br(["create", "Test issue"], "create");
    create.assert_success();

    // Second init without --force should fail (already initialized)
    let init2 = ws.run_br(["init"], "init_second");
    init2.assert_failure();
    assert!(
        init2.stderr.to_lowercase().contains("already")
            || init2.stderr.contains("ALREADY_INITIALIZED"),
        "re-init should report already initialized: stdout='{}' stderr='{}'",
        init2.stdout,
        init2.stderr
    );

    // Data should be preserved
    let list = ws.run_br(["list", "--json"], "list_after_reinit");
    list.assert_success();

    let payload = extract_json_payload(&list.stdout);
    let issues: Vec<Value> = serde_json::from_str(&payload).expect("parse list json");
    assert!(
        !issues.is_empty(),
        "issues should be preserved after re-init"
    );

    ws.finish(true);
}

#[test]
fn scenario_init_json_output() {
    let mut ws = TestWorkspace::new("e2e_workspace", "init_json");

    // Init with JSON output
    let init = ws.run_br(["init", "--json"], "init_json");
    init.assert_success();

    let payload = extract_json_payload(&init.stdout);
    if !payload.is_empty() && (payload.starts_with('{') || payload.starts_with('[')) {
        let json: Value = serde_json::from_str(&payload).expect("parse init json");
        assert!(
            json.get("path").is_some() || json.get("workspace").is_some(),
            "init JSON should contain path or workspace field"
        );
    }

    ws.finish(true);
}

// =============================================================================
// Config Scenarios
// =============================================================================

#[test]
fn scenario_config_list() {
    let mut ws = TestWorkspace::new("e2e_workspace", "config_list");

    // Init first
    let init = ws.run_br(["init"], "init");
    init.assert_success();

    // List configuration
    let list = ws.run_br(["config", "list"], "config_list");
    list.assert_success();

    // Should contain configuration output
    assert!(!list.stdout.is_empty(), "config list should produce output");

    ws.finish(true);
}

#[test]
fn scenario_config_list_json() {
    let mut ws = TestWorkspace::new("e2e_workspace", "config_list_json");

    let init = ws.run_br(["init"], "init");
    init.assert_success();

    let list = ws.run_br(["config", "list", "--json"], "config_list_json");
    list.assert_success();

    let payload = extract_json_payload(&list.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse config list json");
    assert!(json.is_object(), "config list --json should return object");

    ws.finish(true);
}

#[test]
fn scenario_config_set_and_get() {
    let mut ws = TestWorkspace::new("e2e_workspace", "config_set_get");

    let init = ws.run_br(["init"], "init");
    init.assert_success();

    // Set a config value
    let set = ws.run_br(["config", "set", "issue_prefix=test_prefix"], "config_set");
    set.assert_success();

    // Get the value back
    let get = ws.run_br(["config", "get", "issue_prefix"], "config_get");
    get.assert_success();
    assert!(
        get.stdout.contains("test_prefix"),
        "config get should show set value: {}",
        get.stdout
    );

    ws.finish(true);
}

#[test]
fn scenario_config_get_json() {
    let mut ws = TestWorkspace::new("e2e_workspace", "config_get_json");

    let init = ws.run_br(["init"], "init");
    init.assert_success();

    // Set a value first
    let set = ws.run_br(["config", "set", "json=true"], "config_set");
    set.assert_success();

    // Get with JSON output
    let get = ws.run_br(["config", "get", "json", "--json"], "config_get_json");
    get.assert_success();

    let payload = extract_json_payload(&get.stdout);
    let _json: Value = serde_json::from_str(&payload).expect("parse config get json");

    ws.finish(true);
}

#[test]
fn scenario_config_path() {
    let mut ws = TestWorkspace::new("e2e_workspace", "config_path");

    let init = ws.run_br(["init"], "init");
    init.assert_success();

    // Get config file path
    let path = ws.run_br(["config", "path"], "config_path");
    path.assert_success();

    // Should contain a path
    let stdout = &path.stdout;
    assert!(
        stdout.contains("beads") || stdout.contains('.'),
        "config path should output a path: {stdout}"
    );

    ws.finish(true);
}

// =============================================================================
// Doctor Scenarios
// =============================================================================

#[test]
fn scenario_doctor_healthy_workspace() {
    let mut ws = TestWorkspace::new("e2e_workspace", "doctor_healthy");

    let init = ws.run_br(["init"], "init");
    init.assert_success();

    // Doctor on healthy workspace should pass
    let doctor = ws.run_br(["doctor"], "doctor");
    doctor.assert_success();

    ws.finish(true);
}

#[test]
fn scenario_doctor_json_output() {
    let mut ws = TestWorkspace::new("e2e_workspace", "doctor_json");

    let init = ws.run_br(["init"], "init");
    init.assert_success();

    let doctor = ws.run_br(["doctor", "--json"], "doctor_json");
    doctor.assert_success();

    let payload = extract_json_payload(&doctor.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse doctor json");

    // Should have checks array
    assert!(
        json.get("checks").is_some() || json.is_array(),
        "doctor JSON should contain checks: {json:?}"
    );

    ws.finish(true);
}

#[test]
fn scenario_doctor_no_workspace() {
    let mut ws = TestWorkspace::new("e2e_workspace", "doctor_no_workspace");
    // Do NOT init

    let doctor = ws.run_br(["doctor"], "doctor_no_init");
    // Should fail or warn about missing workspace
    // (behavior may vary - just verify it doesn't crash)
    assert!(
        !doctor.success || doctor.stderr.contains("not initialized"),
        "doctor should indicate missing workspace"
    );

    ws.finish(true);
}

// =============================================================================
// Info Scenarios
// =============================================================================

#[test]
fn scenario_info_shows_paths() {
    let mut ws = TestWorkspace::new("e2e_workspace", "info_paths");

    let init = ws.run_br(["init"], "init");
    init.assert_success();

    let info = ws.run_br(["info"], "info");
    info.assert_success();

    // Should contain workspace path info
    assert!(!info.stdout.is_empty(), "info should produce output");

    ws.finish(true);
}

#[test]
fn scenario_info_json_output() {
    let mut ws = TestWorkspace::new("e2e_workspace", "info_json");

    let init = ws.run_br(["init"], "init");
    init.assert_success();

    let info = ws.run_br(["info", "--json"], "info_json");
    info.assert_success();

    let payload = extract_json_payload(&info.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse info json");
    assert!(json.is_object(), "info --json should return object");

    ws.finish(true);
}

// =============================================================================
// Where Scenarios
// =============================================================================

#[test]
fn scenario_where_shows_workspace_path() {
    let mut ws = TestWorkspace::new("e2e_workspace", "where_path");

    let init = ws.run_br(["init"], "init");
    init.assert_success();

    let where_cmd = ws.run_br(["where"], "where");
    where_cmd.assert_success();

    // Should show a path to the workspace
    let stdout = &where_cmd.stdout;
    assert!(
        stdout.contains('/') || stdout.contains('\\'),
        "where should output a path: {stdout}"
    );

    ws.finish(true);
}

#[test]
fn scenario_where_no_workspace() {
    let mut ws = TestWorkspace::new("e2e_workspace", "where_no_workspace");
    // Do NOT init

    let where_cmd = ws.run_br(["where"], "where_no_init");
    // Should fail or indicate no workspace
    assert!(
        !where_cmd.success || where_cmd.stderr.contains("not"),
        "where should indicate missing workspace"
    );

    ws.finish(true);
}

// =============================================================================
// Version Scenarios
// =============================================================================

#[test]
fn scenario_version_text() {
    let mut ws = TestWorkspace::new("e2e_workspace", "version_text");
    // Version doesn't require init

    let version = ws.run_br(["version"], "version");
    version.assert_success();

    // Should contain version info
    assert!(
        version.stdout.contains("br") || version.stdout.contains("version"),
        "version should show version info: {}",
        version.stdout
    );

    ws.finish(true);
}

#[test]
fn scenario_version_json() {
    let mut ws = TestWorkspace::new("e2e_workspace", "version_json");

    let version = ws.run_br(["version", "--json"], "version_json");
    version.assert_success();

    let payload = extract_json_payload(&version.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse version json");

    // Check expected fields
    assert!(
        json.get("version").is_some(),
        "version JSON should have 'version' field"
    );

    ws.finish(true);
}

#[test]
fn scenario_version_no_workspace_required() {
    let mut ws = TestWorkspace::new("e2e_workspace", "version_no_workspace");
    // Do NOT init - version should still work

    let version = ws.run_br(["version"], "version");
    version.assert_success();

    ws.finish(true);
}

// =============================================================================
// Cross-command Scenarios
// =============================================================================

#[test]
fn scenario_workspace_lifecycle() {
    let mut ws = TestWorkspace::new("e2e_workspace", "lifecycle");

    // 1. Check version (no workspace needed)
    let version = ws.run_br(["version", "--json"], "version");
    version.assert_success();

    // 2. Initialize workspace
    let init = ws.run_br(["init"], "init");
    init.assert_success();

    // 3. Check workspace location
    let where_cmd = ws.run_br(["where"], "where");
    where_cmd.assert_success();

    // 4. Get workspace info
    let info = ws.run_br(["info", "--json"], "info");
    info.assert_success();

    // 5. Check configuration
    let config = ws.run_br(["config", "list", "--json"], "config");
    config.assert_success();

    // 6. Run doctor
    let doctor = ws.run_br(["doctor", "--json"], "doctor");
    doctor.assert_success();

    // 7. Re-init without --force should be rejected
    let reinit = ws.run_br(["init"], "reinit");
    reinit.assert_failure();
    assert!(
        reinit.stderr.to_lowercase().contains("already")
            || reinit.stderr.contains("ALREADY_INITIALIZED"),
        "re-init should report already initialized: stdout='{}' stderr='{}'",
        reinit.stdout,
        reinit.stderr
    );

    // 8. Doctor still passes
    let doctor2 = ws.run_br(["doctor"], "doctor_after_reinit");
    doctor2.assert_success();

    ws.finish(true);
}
