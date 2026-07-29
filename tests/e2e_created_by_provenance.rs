//! e2e coverage for `created_by` provenance: which AGENT created a
//! bead, not which unix user ran the process.
//!
//! Precedence under test (see `config::resolve_actor_with_storage`):
//! 1. explicit config `actor` override (`BD_ACTOR` env / `actor`
//!    config key) — wins even when an agent identity is resolvable.
//! 2. resolved agent identity (`BD_AGENT_ID`, or live-`bd
//!    watch`-ancestry inference — not exercised here, see
//!    `e2e_identity_inference.rs`).
//! 3. `$USER`, then `"unknown"`.
//!
//! Also covers `bd list --created-by <agent>` (storage-level filter)
//! and that JSON output still carries the `created_by` field under
//! its existing name.

use assert_cmd::prelude::*;
use std::process::Command;

fn bin() -> std::path::PathBuf {
    assert_cmd::cargo::cargo_bin!("br").to_path_buf()
}

fn init(path: &std::path::Path) {
    Command::new(bin())
        .current_dir(path)
        .arg("init")
        .assert()
        .success();
}

/// Fallback `$USER`/`"unknown"` value replicated exactly as
/// `config::resolve_actor` computes it, so this test doesn't hardcode
/// an environment-specific username.
fn expected_user_fallback() -> String {
    std::env::var("USER")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// `BD_AGENT_ID` present, no explicit config `actor` -> `created_by`
/// is the resolved agent identity, not the unix user running `br`.
#[test]
fn test_created_by_uses_agent_identity_when_no_config_actor() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path();
    init(path);

    let out = Command::new(bin())
        .current_dir(path)
        .env("BD_AGENT_ID", "agent-x")
        .env_remove("BD_ACTOR")
        .arg("create")
        .arg("agent-created issue")
        .arg("--prefix")
        .arg("bd")
        .arg("--json")
        .output()
        .expect("create with BD_AGENT_ID");
    assert!(out.status.success(), "create failed: {out:?}");

    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        json["created_by"].as_str(),
        Some("agent-x"),
        "created_by should be the resolved agent identity: {json}"
    );

    // Round-trips through `show --json` too (persisted, not just
    // reflected back from the create response).
    let id = json["id"].as_str().unwrap();
    let show_out = Command::new(bin())
        .current_dir(path)
        .env_remove("BD_AGENT_ID")
        .arg("show")
        .arg(id)
        .arg("--json")
        .output()
        .expect("show issue");
    assert!(show_out.status.success());
    let show_json: serde_json::Value = serde_json::from_slice(&show_out.stdout).unwrap();
    assert_eq!(show_json[0]["created_by"].as_str(), Some("agent-x"));
}

/// No `BD_AGENT_ID`, no live watch, no explicit config `actor` ->
/// `created_by` falls back to `$USER` (or `"unknown"`) exactly like
/// pre-existing `resolve_actor` behavior. This is the plain-human-shell
/// case that must keep working with zero setup.
#[test]
fn test_created_by_falls_back_to_user_without_identity() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path();
    init(path);

    let out = Command::new(bin())
        .current_dir(path)
        .env_remove("BD_AGENT_ID")
        .env_remove("BD_ACTOR")
        .arg("create")
        .arg("human-shell issue")
        .arg("--prefix")
        .arg("bd")
        .arg("--json")
        .output()
        .expect("create without identity");
    assert!(out.status.success(), "create failed: {out:?}");

    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        json["created_by"].as_str(),
        Some(expected_user_fallback().as_str()),
        "created_by should fall back to $USER/\"unknown\": {json}"
    );
}

/// Explicit config `actor` (via `BD_ACTOR`) wins over a resolvable
/// agent identity — a deliberate operator override must not be
/// shadowed by identity inference.
#[test]
fn test_created_by_prefers_explicit_actor_over_identity() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path();
    init(path);

    let out = Command::new(bin())
        .current_dir(path)
        .env("BD_AGENT_ID", "agent-y")
        .env("BD_ACTOR", "explicit-actor")
        .arg("create")
        .arg("override issue")
        .arg("--prefix")
        .arg("bd")
        .arg("--json")
        .output()
        .expect("create with actor override");
    assert!(out.status.success(), "create failed: {out:?}");

    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(
        json["created_by"].as_str(),
        Some("explicit-actor"),
        "explicit config actor should win over agent identity: {json}"
    );
}

/// `bd list --created-by <agent>` is a storage-level filter (composes
/// with other filters / ordering), mirroring `--assignee`.
#[test]
fn test_list_filters_by_created_by() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path();
    init(path);

    let create = |title: &str, agent: &str| -> String {
        let out = Command::new(bin())
            .current_dir(path)
            .env("BD_AGENT_ID", agent)
            .env_remove("BD_ACTOR")
            .arg("create")
            .arg(title)
            .arg("--prefix")
            .arg("bd")
            .arg("--json")
            .output()
            .expect("create issue");
        assert!(out.status.success(), "create failed: {out:?}");
        serde_json::from_slice::<serde_json::Value>(&out.stdout).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string()
    };

    let agent_x_id = create("issue by agent-x", "agent-x");
    let _agent_z_id = create("issue by agent-z", "agent-z");

    let out = Command::new(bin())
        .current_dir(path)
        .env_remove("BD_AGENT_ID")
        .arg("list")
        .arg("--created-by")
        .arg("agent-x")
        .arg("--json")
        .output()
        .expect("list --created-by");
    assert!(out.status.success(), "list failed: {out:?}");

    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let issues = json.as_array().expect("list json array");
    assert_eq!(
        issues.len(),
        1,
        "expected exactly one issue created by agent-x: {issues:?}"
    );
    assert_eq!(issues[0]["id"].as_str(), Some(agent_x_id.as_str()));
    assert_eq!(issues[0]["created_by"].as_str(), Some("agent-x"));
}
