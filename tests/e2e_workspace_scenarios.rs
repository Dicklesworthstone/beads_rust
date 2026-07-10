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

use common::cli::parse_list_issues;
use common::harness::{TestWorkspace, extract_json_payload};
use common::scenarios::{WorkspaceEvolutionEventKind, catalog};
use serde_json::Value;
use tempfile::TempDir;

fn parse_json_stdout(stdout: &str, context: &str) -> Value {
    let payload = extract_json_payload(stdout);
    let message = format!("parse {context} json payload: {payload}");
    serde_json::from_str(&payload).expect(&message)
}

fn assert_doctor_json_has_healthy_checks(json: &Value) {
    let checks = json
        .get("checks")
        .and_then(Value::as_array)
        .or_else(|| json.as_array())
        .expect("doctor JSON should contain checks array");
    assert!(
        !checks.is_empty(),
        "doctor should report at least one check"
    );
    assert!(
        checks
            .iter()
            .all(|check| check["status"].as_str() != Some("error")),
        "healthy workspace doctor output should not contain errors: {checks:?}"
    );
}

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

    let issues = parse_list_issues(&list.stdout);
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

    let json = parse_json_stdout(&get.stdout, "config get");
    assert_eq!(json["key"].as_str(), Some("json"));
    assert_eq!(json["value"].as_str(), Some("true"));

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
    let stdout = doctor.stdout.to_ascii_lowercase();
    assert!(
        stdout.contains("ok") || stdout.contains("healthy"),
        "doctor should report healthy checks: {}",
        doctor.stdout
    );

    ws.finish(true);
}

#[test]
fn scenario_doctor_json_output() {
    let mut ws = TestWorkspace::new("e2e_workspace", "doctor_json");

    let init = ws.run_br(["init"], "init");
    init.assert_success();

    let doctor = ws.run_br(["doctor", "--json"], "doctor_json");
    doctor.assert_success();

    let json = parse_json_stdout(&doctor.stdout, "doctor");
    assert_doctor_json_has_healthy_checks(&json);

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
    assert!(
        version.stdout.contains("br") || version.stdout.contains("version"),
        "version should work without a workspace and show version info: {}",
        version.stdout
    );

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
    let version_json = parse_json_stdout(&version.stdout, "version");
    assert!(
        version_json["version"]
            .as_str()
            .is_some_and(|value| !value.is_empty()),
        "version JSON should contain a non-empty version: {version_json:?}"
    );

    // 2. Initialize workspace
    let init = ws.run_br(["init"], "init");
    init.assert_success();

    // 3. Check workspace location
    let where_cmd = ws.run_br(["where"], "where");
    where_cmd.assert_success();
    assert!(
        where_cmd.stdout.contains(".beads"),
        "where should identify the beads directory: {}",
        where_cmd.stdout
    );

    // 4. Get workspace info
    let info = ws.run_br(["info", "--json"], "info");
    info.assert_success();
    let info_json = parse_json_stdout(&info.stdout, "info");
    assert!(
        info_json["beads_dir"]
            .as_str()
            .is_some_and(|path| path.contains(".beads")),
        "info JSON should include beads_dir: {info_json:?}"
    );
    assert_eq!(info_json["mode"].as_str(), Some("direct"));

    // 5. Check configuration
    let config = ws.run_br(["config", "list", "--json"], "config");
    config.assert_success();
    let config_json = parse_json_stdout(&config.stdout, "config list");
    assert!(
        config_json.is_object(),
        "config list JSON should be an object: {config_json:?}"
    );

    // 6. Run doctor
    let doctor = ws.run_br(["doctor", "--json"], "doctor");
    doctor.assert_success();
    let doctor_json = parse_json_stdout(&doctor.stdout, "doctor");
    assert_doctor_json_has_healthy_checks(&doctor_json);

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
    let doctor2_stdout = doctor2.stdout.to_ascii_lowercase();
    assert!(
        doctor2_stdout.contains("ok") || doctor2_stdout.contains("healthy"),
        "doctor should remain healthy after rejected re-init: {}",
        doctor2.stdout
    );

    ws.finish(true);
}

#[test]
#[allow(clippy::too_many_lines)]
fn scenario_long_lived_single_workspace_stress_suite() {
    let iterations = std::env::var("BR_LONG_STRESS_ITERATIONS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(8);

    let materialized = catalog::long_lived_mixed_order_stress(41, iterations)
        .execute()
        .expect("long-lived stress plan should execute");

    let command_events: Vec<_> = materialized
        .events
        .iter()
        .filter(|event| event.kind == WorkspaceEvolutionEventKind::Command)
        .collect();
    assert!(
        command_events.len() >= iterations.saturating_mul(8),
        "stress suite should run a meaningful command volume: {} events for {iterations} iterations",
        command_events.len()
    );

    let expected_failures: Vec<_> = command_events
        .iter()
        .filter_map(|event| {
            event
                .command_result
                .as_ref()
                .filter(|result| !result.success)
                .map(|result| (*event, result))
        })
        .collect();
    assert!(
        !expected_failures.is_empty(),
        "stress suite should include expected intermittent failure probes"
    );
    for (event, result) in expected_failures {
        assert!(
            event.matched_expectation,
            "expected failure should still match its declared outcome: {}",
            event.label
        );
        assert!(
            result.log_path.exists(),
            "expected failure should leave a replay log at {}",
            result.log_path.display()
        );
    }

    let final_doctor = materialized
        .event("doctor_after_stress")
        .and_then(|event| event.command_result.as_ref())
        .expect("final doctor event");
    // Per the post-#292 doctor contract (commits 96c3fad2, 1c3c4fe1):
    // any non-OK check — WARN or ERROR — now flips top-level `ok` to
    // false and exits 1. The stress harness legitimately produces a
    // handful of benign WARN findings (test runner sets
    // `RUST_LOG=beads_rust=debug` which trips `rust_log`; frankensqlite
    // leaves a WAL sidecar without a matching SHM file which trips
    // `db.sidecars`; and `br init` writes a
    // minimal `.beads/.gitignore` that omits the `.write.lock`
    // pattern so `gitignore.beads_inner_present` warns). Since #378,
    // `sync --flush-only` also refreshes the merge anchor
    // `beads.base.jsonl` and `base_jsonl.missing_post_flush` no longer
    // warns for verifiably in-sync workspaces. None of
    // those degrade the workspace's semantic health, so we assert on
    // the JSON payload's `workspace_health`/`reliability_audit.health`
    // rather than the now-broader-than-necessary exit-code contract.
    let doctor_json = parse_json_stdout(&final_doctor.stdout, "doctor_after_stress");
    let workspace_health = doctor_json
        .get("workspace_health")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert_eq!(
        workspace_health, "healthy",
        "stress workspace should finish workspace_health=healthy: \
         exit={} stdout={} stderr={}",
        final_doctor.exit_code, final_doctor.stdout, final_doctor.stderr
    );
    let audit_health = doctor_json
        .pointer("/reliability_audit/health")
        .and_then(Value::as_str)
        .unwrap_or("");
    assert_eq!(
        audit_health, "healthy",
        "stress workspace should finish reliability_audit.health=healthy: \
         exit={} stdout={} stderr={}",
        final_doctor.exit_code, final_doctor.stdout, final_doctor.stderr
    );
    let anomaly_count = doctor_json
        .pointer("/reliability_audit/anomaly_count")
        .and_then(Value::as_u64)
        .unwrap_or(u64::MAX);
    assert_eq!(
        anomaly_count, 0,
        "stress workspace should finish with zero reliability anomalies: \
         exit={} stdout={} stderr={}",
        final_doctor.exit_code, final_doctor.stdout, final_doctor.stderr
    );
    // Belt-and-suspenders: assert no `error`-level check leaks through
    // (WARN is acceptable for the benign findings catalogued above).
    let checks = doctor_json
        .get("checks")
        .and_then(Value::as_array)
        .expect("doctor JSON should contain checks array");
    assert!(
        checks
            .iter()
            .all(|check| check["status"].as_str() != Some("error")),
        "stress workspace doctor output should not contain any error-status checks: \
         exit={} stdout={} stderr={}",
        final_doctor.exit_code,
        final_doctor.stdout,
        final_doctor.stderr
    );

    let replay_target = TempDir::new().expect("replay target");
    materialized
        .materialize_into(replay_target.path())
        .expect("copy materialized stress workspace");
    assert!(
        replay_target
            .path()
            .join(".beads")
            .join("issues.jsonl")
            .exists(),
        "materialized stress workspace should retain the JSONL export"
    );
    assert!(
        replay_target.path().join("logs").exists(),
        "materialized stress workspace should retain command logs"
    );
}
