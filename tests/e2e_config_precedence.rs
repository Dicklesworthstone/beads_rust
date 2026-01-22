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
