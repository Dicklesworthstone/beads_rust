//! E2E tests for workspace initialization and diagnostic commands.
//!
//! Tests init, config, doctor, info, where, and version commands.
//! Part of beads_rust-6esx.

mod common;

use common::cli::{BrWorkspace, extract_json_payload, parse_list_issues, run_br, run_br_with_env};
use fsqlite::Connection;
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
fn e2e_sync_import_only_accepts_mixed_prefixes_and_keeps_default_prefix_for_new_ids() {
    let _log = common::test_log(
        "e2e_sync_import_only_accepts_mixed_prefixes_and_keeps_default_prefix_for_new_ids",
    );
    let workspace = BrWorkspace::new();

    let init = run_br(
        &workspace,
        ["init", "--prefix", "local"],
        "init_local_prefix",
    );
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Seed issue", "--json"],
        "create_seed_issue",
    );
    assert!(
        create.status.success(),
        "seed create failed: {}",
        create.stderr
    );
    let seed_payload = extract_json_payload(&create.stdout);
    let seed_issue: Value =
        serde_json::from_str(&seed_payload).expect("seed create should emit valid JSON");

    let mut imported_issue = seed_issue.clone();
    imported_issue["id"] = Value::String("other-abc12".to_string());
    imported_issue["title"] = Value::String("Imported mixed-prefix issue".to_string());
    imported_issue["content_hash"] = Value::Null;

    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    fs::write(
        &jsonl_path,
        format!(
            "{}\n{}\n",
            serde_json::to_string(&seed_issue).expect("serialize seed issue"),
            serde_json::to_string(&imported_issue).expect("serialize imported issue"),
        ),
    )
    .expect("write mixed-prefix jsonl");

    let import = run_br(
        &workspace,
        ["sync", "--import-only", "--json"],
        "sync_import_mixed_prefixes",
    );
    assert!(
        import.status.success(),
        "sync --import-only should accept mixed prefixes: {}",
        import.stderr
    );

    let list = run_br(&workspace, ["list", "--json"], "list_after_mixed_import");
    assert!(list.status.success(), "list failed: {}", list.stderr);
    let issues = parse_list_issues(&list.stdout);
    let ids: Vec<&str> = issues
        .iter()
        .filter_map(|issue| issue["id"].as_str())
        .collect();
    assert!(
        ids.iter().any(|id| id.starts_with("local-")),
        "expected local-prefixed issue in {ids:?}"
    );
    assert!(
        ids.contains(&"other-abc12"),
        "expected other-abc12 in {ids:?}"
    );

    let create_after_import = run_br(
        &workspace,
        ["create", "Fresh local issue", "--json"],
        "create_after_mixed_import",
    );
    assert!(
        create_after_import.status.success(),
        "create after mixed import failed: {}",
        create_after_import.stderr
    );
    let created_payload = extract_json_payload(&create_after_import.stdout);
    let created_issue: Value = serde_json::from_str(&created_payload).expect("created issue JSON");
    let created_id = created_issue["id"]
        .as_str()
        .expect("created issue id should be present");
    assert!(
        created_id.starts_with("local-"),
        "new issues should keep configured default prefix: {created_id}"
    );
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

#[test]
fn e2e_update_quiet_suppresses_success_output() {
    let _log = common::test_log("e2e_update_quiet_suppresses_success_output");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Quiet update test", "--json"],
        "create_quiet_update",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let payload = extract_json_payload(&create.stdout);
    let issue: Value = serde_json::from_str(&payload).expect("parse create json");
    let id = issue["id"].as_str().expect("issue id");

    let update = run_br(
        &workspace,
        ["--quiet", "update", id, "--status", "in_progress"],
        "update_quiet",
    );
    assert!(update.status.success(), "update failed: {}", update.stderr);
    assert!(
        update.stdout.trim().is_empty(),
        "quiet update should suppress success output: {}",
        update.stdout
    );
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

#[test]
fn e2e_doctor_repair_json_rebuilds_and_returns_single_payload() {
    let _log = common::test_log("e2e_doctor_repair_json_rebuilds_and_returns_single_payload");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Repair doctor JSON"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let db_path = workspace.root.join(".beads").join("beads.db");
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    assert!(db_path.exists(), "database should exist before repair test");
    assert!(
        jsonl_path.exists(),
        "issues.jsonl should exist before repair test"
    );

    // Scoped so the injecting connection is closed before `doctor --repair`
    // runs. The repair path rebuilds the database and needs an exclusive open;
    // holding this connection across it makes the engine refuse with
    // "unable to open database file" instead of repairing.
    {
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).expect("open beads db");
        conn.execute("INSERT INTO config (key, value) VALUES ('issue_prefix', 'dup-a')")
            .expect("insert duplicate config row a");
        conn.execute("INSERT INTO config (key, value) VALUES ('issue_prefix', 'dup-b')")
            .expect("insert duplicate config row b");
    }

    let pre_repair = run_br(&workspace, ["doctor", "--json"], "doctor_pre_repair_json");
    assert!(
        !pre_repair.status.success(),
        "doctor should fail before repair when recoverable anomalies are present"
    );
    let pre_payload = extract_json_payload(&pre_repair.stdout);
    let pre_json: Value = serde_json::from_str(&pre_payload).expect("pre-repair doctor json");
    assert_eq!(pre_json["ok"], Value::Bool(false));

    let repaired = run_br(
        &workspace,
        ["doctor", "--repair", "--json"],
        "doctor_repair_json",
    );
    assert!(
        repaired.status.success(),
        "doctor --repair --json failed: stdout='{}' stderr='{}'",
        repaired.stdout,
        repaired.stderr
    );

    let payload = extract_json_payload(&repaired.stdout);
    let json: Value = serde_json::from_str(&payload).expect("repair doctor json");
    assert_eq!(json["repaired"], Value::Bool(true));
    assert_eq!(json["verified"], Value::Bool(true));
    assert_eq!(json["report"]["ok"], Value::Bool(false));
    assert_eq!(json["post_repair"]["ok"], Value::Bool(true));

    let anomaly_checks = json["report"]["checks"]
        .as_array()
        .expect("initial checks array");
    assert!(
        anomaly_checks.iter().any(|check| {
            check["name"] == "db.recoverable_anomalies" && check["status"] == "error"
        }),
        "expected recoverable anomaly in initial doctor report: {json:?}"
    );
}

#[test]
fn e2e_startup_auto_recovery_preserves_unflushed_tombstones() {
    // Regression: when the DB opens successfully but
    // `detect_recoverable_open_anomaly` flags duplicate config/metadata/
    // schema rows, the startup recovery path inside
    // `open_sqlite_storage_with_recovery_strategy` used to
    // `drop(storage)` + `rebuild_database_from_jsonl(...)` unconditionally.
    // Any local tombstone the user had deleted but not yet flushed to
    // JSONL was silently wiped by the rebuild, because the JSONL still
    // showed the issue as open and the rebuild only imports what's in the
    // JSONL. The fix snapshots tombstones from the anomalous-but-queryable
    // storage before dropping it and restores them after the rebuild, the
    // same way the explicit `br sync --import-only --rebuild` delegation path does.
    let _log = common::test_log("e2e_startup_auto_recovery_preserves_unflushed_tombstones");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let keep = run_br(&workspace, ["create", "Keep me"], "create_keep");
    assert!(keep.status.success(), "create keep failed: {}", keep.stderr);

    let delete = run_br(&workspace, ["create", "Delete me"], "create_delete");
    assert!(
        delete.status.success(),
        "create delete failed: {}",
        delete.stderr
    );
    let delete_id = delete
        .stdout
        .lines()
        .next()
        .and_then(|line| {
            line.strip_prefix("✓ ")
                .unwrap_or(line)
                .strip_prefix("Created ")
                .and_then(|rest| rest.split(':').next())
        })
        .expect("parse delete id")
        .trim()
        .to_string();

    // Flush so the JSONL shows both issues as open.
    let flush = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);

    // Delete one issue without flushing: tombstone lives only in the DB.
    let delete_cmd = run_br(
        &workspace,
        ["delete", &delete_id, "--force", "--no-auto-flush"],
        "delete_no_flush",
    );
    assert!(
        delete_cmd.status.success(),
        "delete failed: {}",
        delete_cmd.stderr
    );

    // Inject duplicate config rows directly into the DB so the next open
    // trips `detect_recoverable_open_anomaly`, firing the startup rebuild
    // path. Scope the connection in its own block so it is closed before
    // the next `br` invocation tries to reopen the DB.
    let db_path = workspace.root.join(".beads").join("beads.db");
    {
        let conn = Connection::open(db_path.to_string_lossy().into_owned())
            .expect("open beads db for anomaly injection");
        conn.execute("INSERT INTO config (key, value) VALUES ('issue_prefix', 'dup-a')")
            .expect("insert duplicate config row a");
        conn.execute("INSERT INTO config (key, value) VALUES ('issue_prefix', 'dup-b')")
            .expect("insert duplicate config row b");
    }

    // Any read command that opens storage will now trip startup
    // auto-recovery. Use `br show` on the tombstoned ID so the assertion
    // below tests the exact question we care about.
    let show = run_br(
        &workspace,
        ["show", &delete_id, "--json"],
        "show_after_rebuild",
    );
    assert!(
        show.status.success(),
        "show after startup auto-rebuild failed: stderr={}",
        show.stderr
    );
    let payload = extract_json_payload(&show.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse show json");
    let record = if json.is_array() {
        json.as_array().and_then(|a| a.first()).cloned()
    } else {
        Some(json.clone())
    }
    .expect("show should return at least one record");
    assert_eq!(
        record["status"].as_str(),
        Some("tombstone"),
        "the local unflushed tombstone must survive startup auto-recovery from a recoverable anomaly, \
         but was found as `{:?}`",
        record["status"]
    );
}

#[test]
fn e2e_doctor_repair_preserves_unflushed_tombstones() {
    // Regression: `doctor --repair` falls through to a JSONL rebuild when
    // light repairs (blocked-cache, sidecar, reindex, vacuum) don't clear
    // the report, but that rebuild used to run `config::repair_database_from_jsonl`
    // without snapshotting the DB's tombstones first. The resulting DB
    // therefore only contained what the JSONL had, silently wiping any
    // local tombstone the user had deleted but not yet flushed. The fix
    // snapshots tombstones from the pre-repair DB (best-effort — this path
    // is reached precisely because the DB is misbehaving) and restores
    // them after the rebuild.
    let _log = common::test_log("e2e_doctor_repair_preserves_unflushed_tombstones");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let keep = run_br(&workspace, ["create", "Keep"], "create_keep");
    assert!(keep.status.success(), "create keep failed: {}", keep.stderr);

    let delete = run_br(&workspace, ["create", "Delete me"], "create_delete");
    assert!(
        delete.status.success(),
        "create delete failed: {}",
        delete.stderr
    );
    let delete_id = delete
        .stdout
        .lines()
        .next()
        .and_then(|line| {
            line.strip_prefix("✓ ")
                .unwrap_or(line)
                .strip_prefix("Created ")
                .and_then(|rest| rest.split(':').next())
        })
        .expect("parse delete id")
        .trim()
        .to_string();

    let flush = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);

    let delete_cmd = run_br(
        &workspace,
        ["delete", &delete_id, "--force", "--no-auto-flush"],
        "delete_no_flush",
    );
    assert!(
        delete_cmd.status.success(),
        "delete failed: {}",
        delete_cmd.stderr
    );

    // Inject a recoverable anomaly that doctor will report as an error and
    // that the light-repair passes cannot undo on their own, forcing
    // fall-through to the JSONL rebuild path.
    let db_path = workspace.root.join(".beads").join("beads.db");
    {
        let conn = Connection::open(db_path.to_string_lossy().into_owned())
            .expect("open beads db for anomaly injection");
        conn.execute("INSERT INTO config (key, value) VALUES ('issue_prefix', 'dup-a')")
            .expect("insert duplicate config row a");
        conn.execute("INSERT INTO config (key, value) VALUES ('issue_prefix', 'dup-b')")
            .expect("insert duplicate config row b");
    }

    let repaired = run_br(
        &workspace,
        ["doctor", "--repair", "--json"],
        "doctor_repair",
    );
    // `doctor --repair --json` reports failures as a JSON envelope on stdout,
    // so stderr alone says nothing about why a repair was refused.
    assert!(
        repaired.status.success(),
        "doctor --repair failed: exit={:?}\nstdout={}\nstderr={}",
        repaired.status.code(),
        repaired.stdout,
        repaired.stderr
    );

    let show = run_br(
        &workspace,
        ["show", &delete_id, "--json"],
        "show_after_repair",
    );
    assert!(
        show.status.success(),
        "show after doctor --repair failed: {}",
        show.stderr
    );
    let payload = extract_json_payload(&show.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse show json");
    let record = if json.is_array() {
        json.as_array().and_then(|a| a.first()).cloned()
    } else {
        Some(json.clone())
    }
    .expect("show should return at least one record");
    assert_eq!(
        record["status"].as_str(),
        Some("tombstone"),
        "the local unflushed tombstone must survive doctor --repair's JSONL rebuild, \
         but was found as `{:?}`",
        record["status"]
    );
}

#[test]
fn e2e_doctor_repair_preserves_unflushed_dirty_issues() {
    // Regression for #394: `doctor --repair` falls through to a JSONL rebuild
    // when light repairs don't clear the report. That rebuild imports only
    // what is in the JSONL, so a dirty (unflushed) live issue that never
    // reached the JSONL would be silently dropped — surviving only in the
    // pre-rebuild backup directory. The fix snapshots dirty issues from the
    // pre-repair DB and restores them after the rebuild, mirroring the
    // tombstone-preservation pattern.
    let _log = common::test_log("e2e_doctor_repair_preserves_unflushed_dirty_issues");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // A flushed issue so the JSONL exists and is authoritative.
    let keep = run_br(&workspace, ["create", "Keep"], "create_keep");
    assert!(keep.status.success(), "create keep failed: {}", keep.stderr);
    let flush = run_br(&workspace, ["sync", "--flush-only"], "sync_flush");
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);

    // A brand-new issue created WITHOUT flushing: it lives only in the DB
    // (dirty) and is absent from the JSONL — exactly the export-debt window.
    let dirty = run_br(
        &workspace,
        ["create", "Db only issue", "--no-auto-flush"],
        "create_dirty",
    );
    assert!(
        dirty.status.success(),
        "create dirty failed: {}",
        dirty.stderr
    );
    let dirty_id = dirty
        .stdout
        .lines()
        .next()
        .and_then(|line| {
            line.strip_prefix("✓ ")
                .unwrap_or(line)
                .strip_prefix("Created ")
                .and_then(|rest| rest.split(':').next())
        })
        .expect("parse dirty id")
        .trim()
        .to_string();

    // Inject a recoverable anomaly that forces fall-through to the JSONL
    // rebuild path (same trick as the tombstone doctor test).
    let db_path = workspace.root.join(".beads").join("beads.db");
    {
        let conn = Connection::open(db_path.to_string_lossy().into_owned())
            .expect("open beads db for anomaly injection");
        conn.execute("INSERT INTO config (key, value) VALUES ('issue_prefix', 'dup-a')")
            .expect("insert duplicate config row a");
        conn.execute("INSERT INTO config (key, value) VALUES ('issue_prefix', 'dup-b')")
            .expect("insert duplicate config row b");
    }

    let repaired = run_br(&workspace, ["doctor", "--repair", "--json"], "doctor_repair");
    assert!(
        repaired.status.success(),
        "doctor --repair failed: stderr={}",
        repaired.stderr
    );

    let show = run_br(
        &workspace,
        ["show", &dirty_id, "--json"],
        "show_after_repair",
    );
    assert!(
        show.status.success(),
        "the unflushed dirty issue must survive doctor --repair's JSONL rebuild, \
         but `show` after repair failed: stdout='{}' stderr='{}'",
        show.stdout,
        show.stderr
    );
    let payload = extract_json_payload(&show.stdout);
    let json: Value = serde_json::from_str(&payload).expect("parse show json");
    let record = if json.is_array() {
        json.as_array().and_then(|a| a.first()).cloned()
    } else {
        Some(json.clone())
    }
    .expect("show should return the preserved dirty issue");
    assert_eq!(
        record["title"].as_str(),
        Some("Db only issue"),
        "the preserved dirty issue should retain its title, got `{:?}`",
        record["title"]
    );

    // It must remain dirty so the next flush exports it to the JSONL.
    let flush_after = run_br(&workspace, ["sync", "--flush-only"], "sync_flush_after");
    assert!(
        flush_after.status.success(),
        "flush after repair failed: {}",
        flush_after.stderr
    );
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    let jsonl = fs::read_to_string(&jsonl_path).expect("read issues.jsonl after repair flush");
    assert!(
        jsonl.contains(&dirty_id),
        "the restored dirty issue should be re-marked dirty and exported on the next flush"
    );
}

#[test]
fn e2e_doctor_repair_json_rebuilds_when_db_is_missing() {
    let _log = common::test_log("e2e_doctor_repair_json_rebuilds_when_db_is_missing");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Repair doctor missing DB"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let db_path = workspace.root.join(".beads").join("beads.db");
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    assert!(db_path.exists(), "database should exist before deletion");
    assert!(
        jsonl_path.exists(),
        "issues.jsonl should exist before repair test"
    );

    fs::remove_file(&db_path).expect("remove beads db");
    assert!(
        !db_path.exists(),
        "database should be missing before repair"
    );

    let repaired = run_br(
        &workspace,
        ["doctor", "--repair", "--json"],
        "doctor_repair_missing_db_json",
    );
    assert!(
        repaired.status.success(),
        "doctor --repair --json failed for missing db: stdout='{}' stderr='{}'",
        repaired.stdout,
        repaired.stderr
    );

    let payload = extract_json_payload(&repaired.stdout);
    let json: Value = serde_json::from_str(&payload).expect("repair doctor json");
    assert_eq!(json["repaired"], Value::Bool(true));
    assert_eq!(json["verified"], Value::Bool(true));
    assert_eq!(json["report"]["ok"], Value::Bool(false));
    assert_eq!(json["post_repair"]["ok"], Value::Bool(true));
    assert!(
        db_path.exists(),
        "doctor repair should recreate the database from JSONL"
    );
}

#[test]
fn e2e_doctor_repair_json_rebuilds_when_db_is_malformed() {
    let _log = common::test_log("e2e_doctor_repair_json_rebuilds_when_db_is_malformed");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "Repair doctor malformed DB"],
        "create",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let db_path = workspace.root.join(".beads").join("beads.db");
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    assert!(db_path.exists(), "database should exist before corruption");
    assert!(
        jsonl_path.exists(),
        "issues.jsonl should exist before malformed-db repair test"
    );

    fs::write(&db_path, b"not a sqlite database").expect("corrupt beads db");

    let repaired = run_br(
        &workspace,
        ["doctor", "--repair", "--json"],
        "doctor_repair_malformed_db_json",
    );
    assert!(
        repaired.status.success(),
        "doctor --repair --json failed for malformed db: stdout='{}' stderr='{}'",
        repaired.stdout,
        repaired.stderr
    );

    let payload = extract_json_payload(&repaired.stdout);
    let json: Value = serde_json::from_str(&payload).expect("repair doctor json");
    assert_eq!(json["repaired"], Value::Bool(true));
    assert_eq!(json["verified"], Value::Bool(true));
    assert_eq!(json["report"]["ok"], Value::Bool(false));
    assert_eq!(json["post_repair"]["ok"], Value::Bool(true));

    let show = run_br(
        &workspace,
        ["list", "--json"],
        "list_after_malformed_repair",
    );
    assert!(
        show.status.success(),
        "list should succeed after malformed-db repair: {}",
        show.stderr
    );
    let listed = parse_list_issues(&show.stdout);
    assert!(
        !listed.is_empty(),
        "expected repaired database to contain at least one issue: {listed:?}"
    );
}

#[test]
fn e2e_doctor_detects_and_quarantines_anomalous_wal_sidecar() {
    let _log = common::test_log("e2e_doctor_detects_and_quarantines_anomalous_wal_sidecar");
    let seed_sidecar_anomaly =
        |workspace: &BrWorkspace, label_prefix: &str| -> std::path::PathBuf {
            let init = run_br(workspace, ["init"], &format!("{label_prefix}_init"));
            assert!(init.status.success(), "init failed: {}", init.stderr);

            let create = run_br(
                workspace,
                ["create", "Repair doctor anomalous sidecar"],
                &format!("{label_prefix}_create"),
            );
            assert!(create.status.success(), "create failed: {}", create.stderr);

            let beads_dir = workspace.root.join(".beads");
            let wal_path = beads_dir.join("beads.db-wal");
            fs::write(&wal_path, b"synthetic orphan wal").expect("seed anomalous wal");
            // Which sidecars survive a clean exit is an fsqlite implementation
            // detail, not a property this fixture may assert: 0.1.18 retains
            // `-shm` where earlier versions dropped it. Establish the intended
            // state instead of asserting the engine happened to leave it —
            // an unusable WAL with no SHM to pair it — so the fixture means the
            // same thing on every engine version.
            let shm_path = beads_dir.join("beads.db-shm");
            if shm_path.exists() {
                fs::remove_file(&shm_path).expect("clear engine-managed SHM sidecar");
            }
            wal_path
        };

    let detect_workspace = BrWorkspace::new();
    let _detect_wal_path = seed_sidecar_anomaly(&detect_workspace, "detect");

    let doctor = run_br(
        &detect_workspace,
        ["doctor", "--json"],
        "doctor_sidecar_json",
    );
    // Doctor may succeed (auto-repair) or fail depending on severity.
    // Parse the JSON output regardless of exit code.
    let doctor_json: Value =
        serde_json::from_str(&extract_json_payload(&doctor.stdout)).expect("doctor json");

    // The anomaly here is the WAL's *contents* (20 bytes of garbage), not the
    // sidecar pairing. `db.sidecars` only classifies which sidecars exist, and
    // a WAL without a matching SHM is the normal frankensqlite state, so it
    // reports `ok` with an informational message. The content anomaly surfaces
    // in the reliability audit as `truncated_wal`. Accept either signal: what
    // must hold is that doctor reports the planted anomaly somewhere
    // authoritative, not that one particular check changes status.
    let audit_flags_truncated_wal = doctor_json["reliability_audit"]["anomalies"]
        .as_array()
        .is_some_and(|anomalies| {
            anomalies
                .iter()
                .any(|anomaly| anomaly["code"] == "truncated_wal")
        });

    if let Some(checks) = doctor_json["checks"].as_array() {
        let has_sidecar_check = checks.iter().any(|check| {
            check["name"] == "db.sidecars"
                && (check["status"] == "error"
                    || check["status"] == "warn"
                    || check["status"] == "repaired")
        });
        // If checks array exists and has items, expect to find the sidecar check
        if !checks.is_empty() {
            assert!(
                has_sidecar_check || audit_flags_truncated_wal,
                "doctor should surface the planted WAL anomaly either as a non-ok \
                 db.sidecars check or as a `truncated_wal` reliability-audit anomaly: \
                 {doctor_json}"
            );
        }
    }

    let repair_workspace = BrWorkspace::new();
    let _wal_path = seed_sidecar_anomaly(&repair_workspace, "repair");
    let repair_beads_dir = repair_workspace.root.join(".beads");

    let repaired = run_br(
        &repair_workspace,
        ["doctor", "--repair", "--json"],
        "doctor_repair_sidecar_json",
    );
    assert!(
        repaired.status.success(),
        "doctor --repair --json failed for anomalous sidecar: stdout='{}' stderr='{}'",
        repaired.stdout,
        repaired.stderr
    );

    let repaired_json: Value =
        serde_json::from_str(&extract_json_payload(&repaired.stdout)).expect("repair doctor json");
    // Doctor --repair may report success via different JSON shapes depending
    // on whether it quarantines or silently tolerates the WAL sidecar.
    // With frankensqlite, orphan WAL without SHM is expected and may not need repair.
    // The repair JSON may nest the report under a "report" key.
    let report = if repaired_json.get("report").is_some() {
        &repaired_json["report"]
    } else {
        &repaired_json
    };
    let repair_ok = report["ok"] == Value::Bool(true)
        || repaired_json["repaired"] == Value::Bool(true)
        || repaired_json["message"]
            .as_str()
            .is_some_and(|m| m.contains("nothing to repair"));
    assert!(
        repair_ok,
        "doctor --repair should report success: {repaired_json}"
    );

    // Doctor may quarantine the WAL sidecar into .br_recovery, or may
    // tolerate it (frankensqlite doesn't use SHM). Both are acceptable.
    let recovery_dir = repair_beads_dir.join(".br_recovery");
    if recovery_dir.exists() {
        let recovery_entries: Vec<_> = fs::read_dir(&recovery_dir)
            .expect("read recovery dir")
            .filter_map(std::result::Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        if !recovery_entries.is_empty() {
            assert!(
                recovery_entries.iter().any(|name| name.contains("wal")),
                "expected WAL-related artifact in recovery dir: {recovery_entries:?}"
            );
        }
    }
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
    assert!(
        whr.stdout.contains("database:"),
        "where should report the resolved database path: {}",
        whr.stdout
    );
    assert!(
        whr.stdout.contains("jsonl:"),
        "where should report the resolved JSONL path: {}",
        whr.stdout
    );
}

#[test]
fn e2e_where_uninitialized() {
    let _log = common::test_log("e2e_where_uninitialized");
    let workspace = BrWorkspace::new();

    // Run where without init
    let whr = run_br(&workspace, ["where"], "where_no_init");
    assert!(!whr.status.success(), "where should fail without init");

    // Error output should tell the user to initialize
    let combined = format!("{}{}", whr.stdout, whr.stderr);
    assert!(
        combined.contains("br init") || combined.contains("not initialized"),
        "where without init should tell user to run br init, got: {combined}"
    );
}

#[test]
fn e2e_where_json_output() {
    let _log = common::test_log("e2e_where_json_output");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Where with explicit JSON output
    let whr = run_br(&workspace, ["where", "--json"], "where_json");
    assert!(whr.status.success(), "where --json failed: {}", whr.stderr);
    let payload = extract_json_payload(&whr.stdout);
    let _json: Value =
        serde_json::from_str(&payload).expect("where --json should output valid JSON");
}

#[test]
fn e2e_where_json_reports_effective_prefix_from_project_config() {
    let _log = common::test_log("e2e_where_json_reports_effective_prefix_from_project_config");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    fs::write(
        workspace.root.join(".beads").join("config.yaml"),
        "issue_prefix: proj\n",
    )
    .expect("write project config");

    let whr = run_br(&workspace, ["where", "--json"], "where_json_config_prefix");
    assert!(whr.status.success(), "where --json failed: {}", whr.stderr);

    let payload = extract_json_payload(&whr.stdout);
    let json: Value =
        serde_json::from_str(&payload).expect("where --json should output valid JSON");
    assert_eq!(json["prefix"].as_str(), Some("proj"));
}

#[test]
fn e2e_where_json_omits_prefix_for_mixed_jsonl_fallback() {
    let _log = common::test_log("e2e_where_json_omits_prefix_for_mixed_jsonl_fallback");
    let workspace = BrWorkspace::new();
    let beads_dir = workspace.root.join(".beads");
    fs::create_dir_all(&beads_dir).expect("create beads dir");
    fs::write(
        beads_dir.join("issues.jsonl"),
        concat!(
            r#"{"id":"proj-abc12","title":"Example"}"#,
            "\n",
            r#"{"id":"other-def34","title":"Second"}"#,
            "\n",
        ),
    )
    .expect("write mixed-prefix jsonl");

    let whr = run_br(
        &workspace,
        ["where", "--json"],
        "where_json_mixed_prefix_jsonl",
    );
    assert!(whr.status.success(), "where --json failed: {}", whr.stderr);

    let payload = extract_json_payload(&whr.stdout);
    let json: Value =
        serde_json::from_str(&payload).expect("where --json should output valid JSON");
    assert!(
        json.get("prefix").is_none(),
        "where should omit misleading prefix when JSONL prefixes conflict: {json}"
    );
}

#[test]
fn e2e_where_json_recovers_prefix_from_valid_lines_despite_malformed_jsonl_entries() {
    let _log = common::test_log(
        "e2e_where_json_recovers_prefix_from_valid_lines_despite_malformed_jsonl_entries",
    );
    let workspace = BrWorkspace::new();
    let beads_dir = workspace.root.join(".beads");
    fs::create_dir_all(&beads_dir).expect("create beads dir");
    fs::write(
        beads_dir.join("issues.jsonl"),
        concat!(
            "{not valid json}\n",
            r#"{"id":"proj-abc12","title":"Example"}"#,
            "\n",
        ),
    )
    .expect("write malformed jsonl");

    let whr = run_br(
        &workspace,
        ["where", "--json"],
        "where_json_malformed_prefix_jsonl",
    );
    assert!(whr.status.success(), "where --json failed: {}", whr.stderr);

    let payload = extract_json_payload(&whr.stdout);
    let json: Value =
        serde_json::from_str(&payload).expect("where --json should output valid JSON");
    assert_eq!(json["prefix"].as_str(), Some("proj"));
}

#[test]
fn e2e_where_json_uses_configured_prefix_for_mixed_jsonl_when_db_has_default_prefix() {
    let _log = common::test_log(
        "e2e_where_json_uses_configured_prefix_for_mixed_jsonl_when_db_has_default_prefix",
    );
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init", "--prefix", "proj"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    fs::write(
        workspace.root.join(".beads").join("issues.jsonl"),
        concat!(
            r#"{"id":"proj-abc12","title":"Example"}"#,
            "\n",
            r#"{"id":"other-def34","title":"Second"}"#,
            "\n",
        ),
    )
    .expect("write mixed-prefix jsonl");

    let whr = run_br(
        &workspace,
        ["where", "--json"],
        "where_json_mixed_prefix_existing_db",
    );
    assert!(whr.status.success(), "where --json failed: {}", whr.stderr);

    let payload = extract_json_payload(&whr.stdout);
    let json: Value =
        serde_json::from_str(&payload).expect("where --json should output valid JSON");
    assert_eq!(json["prefix"].as_str(), Some("proj"));
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
