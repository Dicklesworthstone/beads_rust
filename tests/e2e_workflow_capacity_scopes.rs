//! End-to-end coverage for multi-agent capacity scopes (GitHub #384
//! phase 5, bead beads_rust-8nbk.5).
//!
//! Drives the real `br` binary: actor-scoped limits partition admission per
//! `--actor`, harness/session scopes key on `BR_HARNESS`/`BR_SESSION`
//! attribution and are inapplicable without it, structured errors carry
//! `scope`/`scope_key` evidence, soft scoped limits warn without rejecting,
//! and rejected transitions leave issue state untouched.

mod common;

use common::cli::{BrWorkspace, extract_json_payload, parse_created_id, run_br, run_br_with_env};
use serde_json::Value;
use std::fs;

/// Parse structured error JSON, tolerating log lines before the payload.
fn parse_error_json(text: &str) -> Option<Value> {
    if let Ok(json) = serde_json::from_str(text) {
        return Some(json);
    }
    let start = text.find('{')?;
    serde_json::from_str(&text[start..]).ok()
}

fn write_scope_policy(workspace: &BrWorkspace, scope: &str, threshold_line: &str) {
    fs::write(
        workspace.root.join(".beads").join("policy.yaml"),
        format!(
            r"
workflow:
  statuses: [open, in_progress, closed]
  capacity:
    scopes:
      {scope}:
        statuses:
          in_progress:
            {threshold_line}
"
        ),
    )
    .expect("write scope policy");
}

fn create_issue(workspace: &BrWorkspace, title: &str, label: &str) -> String {
    let created = run_br(workspace, ["create", title], label);
    assert!(
        created.status.success(),
        "create failed: {}",
        created.stderr
    );
    parse_created_id(&created.stdout)
}

fn issue_status(workspace: &BrWorkspace, id: &str, label: &str) -> String {
    let show = run_br(workspace, ["show", id, "--json"], label);
    assert!(show.status.success(), "show failed: {}", show.stderr);
    let json: Value = serde_json::from_str(&extract_json_payload(&show.stdout)).expect("show JSON");
    json.get(0)
        .and_then(|issue| issue.get("status"))
        .and_then(Value::as_str)
        .expect("issue status")
        .to_string()
}

#[test]
fn e2e_capacity_scope_actor_partitions_admission_with_structured_evidence() {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "scope_actor_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let first = create_issue(&workspace, "First claim", "scope_actor_create_1");
    let second = create_issue(&workspace, "Second claim", "scope_actor_create_2");
    write_scope_policy(&workspace, "actor", "hard: 1");

    let claim = run_br(
        &workspace,
        [
            "--actor",
            "alice",
            "update",
            &first,
            "--status",
            "in_progress",
        ],
        "scope_actor_claim_1",
    );
    assert!(
        claim.status.success(),
        "first claim failed: {}",
        claim.stderr
    );

    // Alice's partition is full: the rejection is structured and atomic.
    let rejected = run_br(
        &workspace,
        [
            "--actor",
            "alice",
            "--json",
            "update",
            &second,
            "--status",
            "in_progress",
        ],
        "scope_actor_claim_2",
    );
    assert!(
        !rejected.status.success(),
        "alice's second claim must exceed her actor scope: {}",
        rejected.stdout
    );
    let error = parse_error_json(&rejected.stdout).expect("structured error payload");
    let details = &error["error"];
    assert_eq!(
        details["code"].as_str(),
        Some("WORKFLOW_CAPACITY_EXCEEDED"),
        "{error}"
    );
    assert_eq!(
        details["context"]["scope"].as_str(),
        Some("actor"),
        "{error}"
    );
    assert_eq!(
        details["context"]["scope_key"].as_str(),
        Some("alice"),
        "{error}"
    );
    assert_eq!(
        details["context"]["policy_path"].as_str(),
        Some("workflow.capacity.scopes.actor.statuses.in_progress"),
        "{error}"
    );
    assert_eq!(
        issue_status(&workspace, &second, "scope_actor_status_2"),
        "open",
        "rejected transition must leave the issue untouched"
    );

    // A different actor's partition is empty.
    let other = run_br(
        &workspace,
        [
            "--actor",
            "bob",
            "update",
            &second,
            "--status",
            "in_progress",
        ],
        "scope_actor_claim_bob",
    );
    assert!(
        other.status.success(),
        "bob's partition must admit: {}",
        other.stderr
    );
}

#[test]
fn e2e_capacity_scope_harness_and_session_key_on_env_attribution() {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "scope_env_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let first = create_issue(&workspace, "Harness one", "scope_env_create_1");
    let second = create_issue(&workspace, "Harness two", "scope_env_create_2");
    let third = create_issue(&workspace, "Harness free", "scope_env_create_3");
    write_scope_policy(&workspace, "harness", "hard: 1");

    let claim = run_br(
        &workspace,
        [
            "update",
            &first,
            "--status",
            "in_progress",
            "--harness",
            "swarm-h1",
        ],
        "scope_env_claim_1",
    );
    assert!(
        claim.status.success(),
        "first claim failed: {}",
        claim.stderr
    );

    let rejected = run_br(
        &workspace,
        [
            "--json",
            "update",
            &second,
            "--status",
            "in_progress",
            "--harness",
            "swarm-h1",
        ],
        "scope_env_claim_2",
    );
    assert!(
        !rejected.status.success(),
        "same-harness claim must exceed the harness scope: {}",
        rejected.stdout
    );
    let error = parse_error_json(&rejected.stdout).expect("structured error payload");
    assert_eq!(
        error["error"]["context"]["scope_key"].as_str(),
        Some("swarm-h1"),
        "{error}"
    );

    // No harness attribution → the harness scope is inapplicable.
    let unkeyed = run_br(
        &workspace,
        ["update", &third, "--status", "in_progress"],
        "scope_env_claim_free",
    );
    assert!(
        unkeyed.status.success(),
        "attribution-free claims skip the harness scope: {}",
        unkeyed.stderr
    );

    // Session scope: keyed via the BR_SESSION environment variable.
    let ws2 = BrWorkspace::new();
    let init = run_br(&ws2, ["init"], "scope_sess_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    let s1 = create_issue(&ws2, "Session one", "scope_sess_create_1");
    let s2 = create_issue(&ws2, "Session two", "scope_sess_create_2");
    write_scope_policy(&ws2, "session", "hard: 1");

    let claim = run_br_with_env(
        &ws2,
        ["update", &s1, "--status", "in_progress"],
        [("BR_SESSION", "sess-9")],
        "scope_sess_claim_1",
    );
    assert!(
        claim.status.success(),
        "first session claim failed: {}",
        claim.stderr
    );
    let rejected = run_br_with_env(
        &ws2,
        ["--json", "update", &s2, "--status", "in_progress"],
        [("BR_SESSION", "sess-9")],
        "scope_sess_claim_2",
    );
    assert!(
        !rejected.status.success(),
        "same-session claim must exceed the session scope: {}",
        rejected.stdout
    );
    let error = parse_error_json(&rejected.stdout).expect("structured error payload");
    assert_eq!(
        error["error"]["context"]["scope"].as_str(),
        Some("session"),
        "{error}"
    );
    assert_eq!(
        error["error"]["context"]["scope_key"].as_str(),
        Some("sess-9"),
        "{error}"
    );
}

#[test]
fn e2e_capacity_scope_soft_limit_warns_in_json_without_rejecting() {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "scope_soft_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let id = create_issue(&workspace, "Soft scoped", "scope_soft_create");
    write_scope_policy(&workspace, "actor", "soft: 1");

    let updated = run_br(
        &workspace,
        [
            "--actor",
            "alice",
            "--json",
            "update",
            &id,
            "--status",
            "in_progress",
        ],
        "scope_soft_update",
    );
    assert!(
        updated.status.success(),
        "soft scoped limits never reject: {}",
        updated.stderr
    );
    let json: Value =
        serde_json::from_str(&extract_json_payload(&updated.stdout)).expect("update JSON");
    let warnings = json
        .get("warnings")
        .and_then(Value::as_array)
        .expect("soft breach must produce a warnings array");
    assert_eq!(warnings.len(), 1, "{json}");
    assert_eq!(warnings[0]["scope"].as_str(), Some("actor"), "{json}");
    assert_eq!(warnings[0]["scope_key"].as_str(), Some("alice"), "{json}");
    assert_eq!(
        warnings[0]["policy_path"].as_str(),
        Some("workflow.capacity.scopes.actor.statuses.in_progress"),
        "{json}"
    );
}
