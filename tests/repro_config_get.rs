mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use std::fs;

#[test]
fn repro_config_get() {
    let workspace = BrWorkspace::new();
    run_br(&workspace, ["init"], "init");

    // Write config
    let config_path = workspace.root.join(".beads").join("config.yaml");
    fs::write(&config_path, "issue_prefix: cfg_get\n").expect("write config");

    let config_get = run_br(
        &workspace,
        ["config", "get", "issue_prefix", "--json"],
        "config_get",
    );
    println!("Stdout: {}", config_get.stdout);
    println!("Stderr: {}", config_get.stderr);

    let json: serde_json::Value =
        serde_json::from_str(&extract_json_payload(&config_get.stdout)).unwrap();
    let value = json["value"].as_str().unwrap();

    assert_eq!(value, "cfg_get");
}
