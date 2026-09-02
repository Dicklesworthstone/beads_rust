use std::fs;

mod common;

use beads_rust::storage::SqliteStorage;
use common::cli::{BrWorkspace, run_br, run_br_with_env};

#[test]
fn e2e_config_precedence_env_project_user_db() {
    let _log = common::test_log("e2e_config_precedence_env_project_user_db");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // DB layer (lowest non-default)
    let db_path = workspace.root.join(".beads").join("beads.db");
    let mut storage = SqliteStorage::open(&db_path).expect("open db");
    storage
        .set_config("issue_prefix", "DB")
        .expect("set db issue_prefix");
    storage
        .set_config("default_priority", "1")
        .expect("set db default_priority");

    // User config layer (~/.config/beads/config.yaml)
    let user_config = workspace
        .root
        .join(".config")
        .join("beads")
        .join("config.yaml");
    fs::create_dir_all(user_config.parent().unwrap()).expect("create user config dir");
    fs::write(&user_config, "issue_prefix: USER\ndefault_priority: 2\n")
        .expect("write user config");

    // Project config layer (.beads/config.yaml)
    let project_config = workspace.root.join(".beads").join("config.yaml");
    fs::write(&project_config, "issue_prefix: PROJECT\n").expect("write project config");

    // No env: project wins for issue_prefix
    let get_project = run_br(&workspace, ["config", "get", "issue_prefix"], "get_project");
    assert!(
        get_project.status.success(),
        "config get issue_prefix failed: {}",
        get_project.stderr
    );
    assert!(
        get_project.stdout.trim() == "PROJECT",
        "expected PROJECT, got stdout='{}', stderr='{}'",
        get_project.stdout,
        get_project.stderr
    );

    // No env: user wins over DB for default_priority (project doesn't set it)
    let get_user = run_br(
        &workspace,
        ["config", "get", "default_priority"],
        "get_user",
    );
    assert!(
        get_user.status.success(),
        "config get default_priority failed: {}",
        get_user.stderr
    );
    assert!(
        get_user.stdout.trim() == "2",
        "expected default_priority=2 from user config, got stdout='{}'",
        get_user.stdout
    );

    // Env overrides project/user/DB
    let env_vars = vec![("BD_ISSUE_PREFIX", "ENV")];
    let get_env = run_br_with_env(
        &workspace,
        ["config", "get", "issue_prefix"],
        env_vars,
        "get_env",
    );
    assert!(
        get_env.status.success(),
        "config get with env failed: {}",
        get_env.stderr
    );
    assert!(
        get_env.stdout.trim() == "ENV",
        "expected ENV override, got stdout='{}'",
        get_env.stdout
    );
}

#[test]
fn e2e_config_precedence_cli_over_env_project() {
    let _log = common::test_log("e2e_config_precedence_cli_over_env_project");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Project config sets lock-timeout
    let project_config = workspace.root.join(".beads").join("config.yaml");
    fs::write(&project_config, "lock-timeout: 2500\n").expect("write project config");

    // Env overrides project
    let env_vars = vec![("BD_LOCK_TIMEOUT", "3000")];
    let get_env = run_br_with_env(
        &workspace,
        ["config", "get", "lock-timeout"],
        env_vars.clone(),
        "get_env_lock_timeout",
    );
    assert!(
        get_env.status.success(),
        "config get lock-timeout failed: {}",
        get_env.stderr
    );
    assert!(
        get_env.stdout.trim() == "3000",
        "expected env lock-timeout=3000, got stdout='{}'",
        get_env.stdout
    );

    // CLI overrides env + project
    let get_cli = run_br_with_env(
        &workspace,
        ["--lock-timeout", "1234", "config", "get", "lock-timeout"],
        env_vars,
        "get_cli_lock_timeout",
    );
    assert!(
        get_cli.status.success(),
        "config get lock-timeout with CLI override failed: {}",
        get_cli.stderr
    );
    assert!(
        get_cli.stdout.trim() == "1234",
        "expected CLI lock-timeout=1234, got stdout='{}'",
        get_cli.stdout
    );
}

#[test]
fn e2e_config_precedence_includes_legacy_layer() {
    let _log = common::test_log("e2e_config_precedence_includes_legacy_layer");

    let actual_workspace = BrWorkspace::new();
    let runner_workspace = BrWorkspace::new();

    let init = run_br(&actual_workspace, ["init"], "init_actual");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // DB layer (lowest non-default)
    let db_path = actual_workspace.root.join(".beads").join("beads.db");
    let mut storage = SqliteStorage::open(&db_path).expect("open db");
    storage
        .set_config("issue_prefix", "DB")
        .expect("set db issue_prefix");

    // Legacy user config (~/.beads/config.yaml) in runner HOME
    let legacy_config = runner_workspace.root.join(".beads").join("config.yaml");
    fs::create_dir_all(legacy_config.parent().unwrap()).expect("create legacy config dir");
    fs::write(&legacy_config, "issue_prefix: LEGACY\n").expect("write legacy config");

    // User config layer (~/.config/beads/config.yaml) in runner HOME
    let user_config = runner_workspace
        .root
        .join(".config")
        .join("beads")
        .join("config.yaml");
    fs::create_dir_all(user_config.parent().unwrap()).expect("create user config dir");
    fs::write(&user_config, "issue_prefix: USER\n").expect("write user config");

    // Project config layer (.beads/config.yaml) in actual workspace
    let project_config = actual_workspace.root.join(".beads").join("config.yaml");
    fs::write(&project_config, "issue_prefix: PROJECT\n").expect("write project config");

    // Use BEADS_DIR to point at actual workspace
    let beads_dir = actual_workspace.root.join(".beads");
    let env_vars = vec![("BEADS_DIR", beads_dir.to_str().unwrap())];

    // Project overrides user/legacy/db
    let get_project = run_br_with_env(
        &runner_workspace,
        ["config", "get", "issue_prefix"],
        env_vars.clone(),
        "get_project",
    );
    assert!(
        get_project.status.success(),
        "config get issue_prefix failed: {}",
        get_project.stderr
    );
    assert_eq!(get_project.stdout.trim(), "PROJECT");

    // Env overrides project/user/legacy/db
    let env_override = vec![
        ("BEADS_DIR", beads_dir.to_str().unwrap()),
        ("BD_ISSUE_PREFIX", "ENV"),
    ];
    let get_env = run_br_with_env(
        &runner_workspace,
        ["config", "get", "issue_prefix"],
        env_override,
        "get_env",
    );
    assert!(
        get_env.status.success(),
        "config get with env failed: {}",
        get_env.stderr
    );
    assert_eq!(get_env.stdout.trim(), "ENV");
}

#[test]
fn e2e_config_precedence_legacy_used_when_user_missing() {
    let _log = common::test_log("e2e_config_precedence_legacy_used_when_user_missing");

    let actual_workspace = BrWorkspace::new();
    let runner_workspace = BrWorkspace::new();

    let init = run_br(&actual_workspace, ["init"], "init_actual");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // DB layer (lowest non-default)
    let db_path = actual_workspace.root.join(".beads").join("beads.db");
    let mut storage = SqliteStorage::open(&db_path).expect("open db");
    storage
        .set_config("default_priority", "1")
        .expect("set db default_priority");

    // Legacy user config with default_priority
    let legacy_config = runner_workspace.root.join(".beads").join("config.yaml");
    fs::create_dir_all(legacy_config.parent().unwrap()).expect("create legacy config dir");
    fs::write(&legacy_config, "default_priority: 3\n").expect("write legacy config");

    // User config exists but does NOT set default_priority
    let user_config = runner_workspace
        .root
        .join(".config")
        .join("beads")
        .join("config.yaml");
    fs::create_dir_all(user_config.parent().unwrap()).expect("create user config dir");
    fs::write(&user_config, "issue_prefix: USER\n").expect("write user config");

    // Project config exists but does NOT set default_priority
    let project_config = actual_workspace.root.join(".beads").join("config.yaml");
    fs::write(&project_config, "issue_prefix: PROJECT\n").expect("write project config");

    let beads_dir = actual_workspace.root.join(".beads");
    let env_vars = vec![("BEADS_DIR", beads_dir.to_str().unwrap())];

    // Legacy should override DB when user/project do not set the key
    let get_legacy = run_br_with_env(
        &runner_workspace,
        ["config", "get", "default_priority"],
        env_vars.clone(),
        "get_legacy_default_priority",
    );
    assert!(
        get_legacy.status.success(),
        "config get default_priority failed: {}",
        get_legacy.stderr
    );
    assert_eq!(get_legacy.stdout.trim(), "3");

    // User should override legacy once the key is set
    fs::write(&user_config, "issue_prefix: USER\ndefault_priority: 2\n")
        .expect("write user config with default_priority");
    let get_user = run_br_with_env(
        &runner_workspace,
        ["config", "get", "default_priority"],
        env_vars,
        "get_user_default_priority",
    );
    assert!(
        get_user.status.success(),
        "config get default_priority with user override failed: {}",
        get_user.stderr
    );
    assert_eq!(get_user.stdout.trim(), "2");
}

#[test]
fn e2e_config_get_ignores_global_no_db_flag() {
    let _log = common::test_log("e2e_config_get_ignores_global_no_db_flag");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let db_path = workspace.root.join(".beads").join("beads.db");
    let mut storage = SqliteStorage::open(&db_path).expect("open db");
    storage
        .set_config("db_only_key", "from-db")
        .expect("set db-only config");

    let normal = run_br(
        &workspace,
        ["config", "get", "db_only_key"],
        "get_db_only_key",
    );
    assert!(
        normal.status.success(),
        "config get db_only_key failed: {}",
        normal.stderr
    );
    assert_eq!(normal.stdout.trim(), "from-db");

    let no_db = run_br(
        &workspace,
        ["--no-db", "config", "get", "db_only_key"],
        "get_db_only_key_no_db",
    );
    assert!(
        no_db.status.success(),
        "--no-db config get db_only_key failed: {}",
        no_db.stderr
    );
    assert_eq!(no_db.stdout.trim(), "from-db");
}

#[test]
fn e2e_config_set_preserves_malformed_project_yaml() {
    let _log = common::test_log("e2e_config_set_preserves_malformed_project_yaml");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let project_config = workspace.root.join(".beads").join("config.yaml");
    let malformed = "actor: [broken\n";
    fs::write(&project_config, malformed).expect("write malformed project config");

    let set = run_br(
        &workspace,
        ["config", "set", "actor", "alice"],
        "set_malformed",
    );
    assert!(
        !set.status.success(),
        "config set should fail on malformed YAML"
    );
    assert!(
        set.stderr.contains("Failed to parse YAML config"),
        "unexpected stderr: {}",
        set.stderr
    );
    assert_eq!(
        fs::read_to_string(&project_config).expect("read config after failed set"),
        malformed
    );
}

#[test]
fn e2e_config_delete_preserves_db_when_project_yaml_is_malformed() {
    let _log = common::test_log("e2e_config_delete_preserves_db_when_project_yaml_is_malformed");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let db_path = workspace.root.join(".beads").join("beads.db");
    let mut storage = SqliteStorage::open(&db_path).expect("open db");
    storage
        .set_config("db_only_key", "from-db")
        .expect("set db-only config");

    let project_config = workspace.root.join(".beads").join("config.yaml");
    let malformed = "actor: [broken\n";
    fs::write(&project_config, malformed).expect("write malformed project config");

    let delete = run_br(
        &workspace,
        ["config", "delete", "db_only_key"],
        "delete_malformed_project_yaml",
    );
    assert!(
        !delete.status.success(),
        "config delete should fail on malformed YAML"
    );
    assert!(
        delete.stderr.contains("Failed to parse YAML config"),
        "unexpected stderr: {}",
        delete.stderr
    );
    assert_eq!(
        storage
            .get_config("db_only_key")
            .expect("read db config after failed delete")
            .as_deref(),
        Some("from-db")
    );
    assert_eq!(
        fs::read_to_string(&project_config).expect("read config after failed delete"),
        malformed
    );
}

#[test]
fn e2e_config_delete_no_db_preserves_db_values() {
    let _log = common::test_log("e2e_config_delete_no_db_preserves_db_values");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let db_path = workspace.root.join(".beads").join("beads.db");
    let mut storage = SqliteStorage::open(&db_path).expect("open db");
    storage
        .set_config("db_only_key", "from-db")
        .expect("set db-only config");

    let delete = run_br(
        &workspace,
        ["--no-db", "config", "delete", "db_only_key", "--json"],
        "delete_db_only_key_no_db",
    );
    assert!(
        delete.status.success(),
        "--no-db config delete failed: {}",
        delete.stderr
    );

    let payload = common::cli::extract_json_payload(&delete.stdout);
    let result: serde_json::Value = serde_json::from_str(&payload).expect("delete json");
    assert_eq!(result["deleted_from_db"], false);

    let db_value = storage
        .get_config("db_only_key")
        .expect("read db config after no-db delete");
    assert_eq!(db_value.as_deref(), Some("from-db"));
}

#[test]
fn e2e_config_list_rejects_project_and_user_together() {
    let _log = common::test_log("e2e_config_list_rejects_project_and_user_together");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let list = run_br(
        &workspace,
        ["config", "list", "--project", "--user"],
        "config_list_conflict",
    );
    assert!(
        !list.status.success(),
        "config list should reject mutually exclusive flags"
    );
    assert!(
        list.stderr.contains("cannot be used with"),
        "unexpected stderr: {}",
        list.stderr
    );
}

#[test]
fn e2e_create_normalizes_runtime_issue_prefix_from_project_config() {
    let _log = common::test_log("e2e_create_normalizes_runtime_issue_prefix_from_project_config");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let project_config = workspace.root.join(".beads").join("config.yaml");
    fs::write(&project_config, "issue_prefix: \"Project-Name!\"\n").expect("write project config");

    let create = run_br(
        &workspace,
        ["create", "Normalize prefix"],
        "create_with_mixed_prefix",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    assert!(
        create.stdout.contains("Created project-name-"),
        "expected normalized lowercase prefix, got stdout='{}', stderr='{}'",
        create.stdout,
        create.stderr
    );
}

// =============================================================================
// Config key registry (beads_rust-iw7k.3): unknown keys warn instead of
// silently doing nothing, `br config schema` lists what br reads, and the
// doctor flags unknown keys already sitting in config.yaml.
// =============================================================================

#[test]
fn e2e_config_set_unknown_key_warns_with_nearest_known_key() {
    let _log = common::test_log("e2e_config_set_unknown_key_warns_with_nearest_known_key");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // The README used to document `id.prefix`; br reads `issue_prefix`.
    let set = run_br(
        &workspace,
        ["config", "set", "id.prefix=zzz"],
        "set unknown",
    );
    assert!(
        set.status.success(),
        "set still writes the key: {}",
        set.stderr
    );
    assert!(
        set.stderr.contains("unknown config key 'id.prefix'"),
        "stderr must warn about the unknown key:\n{}",
        set.stderr
    );
    assert!(
        set.stderr.contains("issue_prefix"),
        "warning must name the nearest known key:\n{}",
        set.stderr
    );

    let json = run_br(
        &workspace,
        ["config", "set", "output.color=true", "--json"],
        "set unknown json",
    );
    assert!(json.status.success(), "{}", json.stderr);
    let payload: serde_json::Value =
        serde_json::from_str(common::cli::extract_json_payload(&json.stdout)).expect("json");
    assert!(
        payload["warning"]
            .as_str()
            .is_some_and(|w| w.contains("output.color")),
        "JSON output carries the warning: {payload}"
    );

    let known = run_br(
        &workspace,
        ["config", "set", "default_priority=1"],
        "set known",
    );
    assert!(known.status.success());
    assert!(
        !known.stderr.contains("unknown config key"),
        "{}",
        known.stderr
    );

    // The doctor now reports the two unknown keys that landed in config.yaml.
    let doctor = run_br(&workspace, ["doctor", "--json"], "doctor");
    let report: serde_json::Value =
        serde_json::from_str(common::cli::extract_json_payload(&doctor.stdout))
            .expect("doctor json");
    let check = report["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .find(|c| c["name"] == "config.unknown_keys")
        .expect("config.unknown_keys check present");
    assert_eq!(check["status"], "warn", "{check}");
    let unknown: Vec<&str> = check["details"]["unknown_keys"]
        .as_array()
        .expect("unknown_keys")
        .iter()
        .filter_map(|e| e["key"].as_str())
        .collect();
    assert!(
        unknown.contains(&"id.prefix") && unknown.contains(&"output.color"),
        "{check}"
    );
}

#[test]
fn e2e_config_schema_lists_the_keys_br_reads() {
    let _log = common::test_log("e2e_config_schema_lists_the_keys_br_reads");
    let workspace = BrWorkspace::new();

    let text = run_br(&workspace, ["config", "schema"], "schema text");
    assert!(text.status.success(), "{}", text.stderr);
    assert!(text.stdout.contains("issue_prefix"), "{}", text.stdout);
    assert!(text.stdout.contains("sync.auto_flush"), "{}", text.stdout);

    let json = run_br(
        &workspace,
        ["config", "schema", "--format", "json"],
        "schema json",
    );
    assert!(json.status.success(), "{}", json.stderr);
    let payload: serde_json::Value =
        serde_json::from_str(common::cli::extract_json_payload(&json.stdout)).expect("json");
    assert_eq!(payload["schema_version"], "br.config.schema.v1");
    let keys: Vec<&str> = payload["keys"]
        .as_array()
        .expect("keys")
        .iter()
        .filter_map(|k| k["key"].as_str())
        .collect();
    for expected in [
        "issue_prefix",
        "default_priority",
        "default_type",
        "sync.auto_flush",
        "lock_timeout",
    ] {
        assert!(keys.contains(&expected), "missing {expected}: {keys:?}");
    }
    assert!(!keys.contains(&"id.prefix"));
}
