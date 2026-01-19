use std::fs;

// We can't easily run the full CLI command because it relies on env vars (HOME) and global state.
// But we can unit test the logic if we mock things or use the integration test harness.
// The integration harness (common::cli) runs the binary. This is best.

mod common;
use common::cli::{BrWorkspace, run_br, run_br_with_env};

#[test]
fn test_config_set_shadowed_by_project_config() {
    let _log = common::test_log("test_config_set_shadowed_by_project_config");
    let workspace = BrWorkspace::new();
    let home_dir = workspace.temp_dir.path().join("home");
    fs::create_dir_all(&home_dir).unwrap();

    // 1. Init repo
    run_br(&workspace, ["init"], "init");

    // 2. Create project config with prefix=PROJECT
    let project_config = workspace.root.join(".beads/config.yaml");
    fs::write(&project_config, "issue_prefix: PROJECT\n").unwrap();

    // 3. Verify get returns PROJECT
    let get1 = run_br(&workspace, ["config", "get", "issue_prefix"], "get1");
    if !get1.status.success() {
        println!("get1 failed: {}", get1.stderr);
    }
    assert!(
        get1.stdout.contains("PROJECT"),
        "Expected PROJECT, got stdout='{}', stderr='{}'",
        get1.stdout,
        get1.stderr
    );

    // 4. Set prefix=USER (this writes to ~/.config/beads/config.yaml by default)
    // We need to set HOME env var to our temp home
    let env_vars = vec![("HOME", home_dir.to_str().unwrap())];
    let set = run_br_with_env(
        &workspace,
        ["config", "set", "issue_prefix=USER"],
        env_vars.clone(),
        "set",
    );
    assert!(set.status.success());

    // 5. Verify get returns USER (Expectation: CLI set should win or update project config)
    // But currently it writes to User config, which is LOWER priority than Project.
    let get2 = run_br_with_env(
        &workspace,
        ["config", "get", "issue_prefix"],
        env_vars,
        "get2",
    );

    // This assertion will FAIL if the bug exists (it will return PROJECT)
    assert!(
        get2.stdout.contains("USER"),
        "Expected USER, got: {}",
        get2.stdout
    );
}
