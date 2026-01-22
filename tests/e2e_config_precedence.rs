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
        get_project.stdout.contains("PROJECT"),
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
        get_user.stdout.contains('2'),
        "expected default_priority=2 from user config, got stdout='{}'",
        get_user.stdout
    );

    // Env overrides project/user/DB
    let env_vars = vec![("BD_ISSUE_PREFIX", "ENV")];
    let get_env =
        run_br_with_env(&workspace, ["config", "get", "issue_prefix"], env_vars, "get_env");
    assert!(
        get_env.status.success(),
        "config get with env failed: {}",
        get_env.stderr
    );
    assert!(
        get_env.stdout.contains("ENV"),
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
        get_env.stdout.contains("3000"),
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
        get_cli.stdout.contains("1234"),
        "expected CLI lock-timeout=1234, got stdout='{}'",
        get_cli.stdout
    );
}
