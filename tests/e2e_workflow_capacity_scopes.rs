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

/// Every backticked test name in the GH-384 acceptance matrix must exist as
/// a real test function, so renames cannot silently rot the matrix.
#[test]
fn gh384_acceptance_matrix_names_real_tests() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let matrix = fs::read_to_string(root.join("docs/GH384_ACCEPTANCE_MATRIX.md"))
        .expect("read acceptance matrix");

    let mut names: Vec<String> = Vec::new();
    for segment in matrix.split('`').skip(1).step_by(2) {
        // Backticked segments that look like test identifiers.
        if !segment.is_empty()
            && segment
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_')
            && segment.contains('_')
            && !segment.starts_with("beads_rust-")
        {
            names.push(segment.to_string());
        }
    }
    assert!(
        names.len() >= 30,
        "matrix should reference a substantial test set, found {}",
        names.len()
    );

    let mut haystack = String::new();
    for file in [
        "src/storage/sqlite.rs",
        "src/close_policy.rs",
        "src/error/structured.rs",
        "tests/e2e_workflow_capacity_scopes.rs",
        "tests/e2e_workflow_capacity_exemptions.rs",
        "tests/e2e_errors.rs",
    ] {
        haystack.push_str(&fs::read_to_string(root.join(file)).expect("read source"));
    }

    let missing: Vec<&String> = names
        .iter()
        .filter(|name| {
            // Skip non-test identifiers the matrix mentions (bead ids etc.).
            name.starts_with("workflow_capacity")
                || name.starts_with("capacity_")
                || name.starts_with("e2e_capacity")
                || name.starts_with("e2e_workflow_capacity")
                || name.starts_with("loader_parses")
                || name.starts_with("gh384_")
        })
        .filter(|name| !haystack.contains(&format!("fn {name}(")))
        .collect();
    assert!(
        missing.is_empty(),
        "acceptance matrix names tests that do not exist: {missing:?}"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_capacity_observability_in_stats_and_coordination() {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "obs_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    // Before any capacity is configured, the stats payload keeps its
    // pre-capacity shape (no `capacity` key at all).
    let bare = run_br(&workspace, ["stats", "--json"], "obs_stats_bare");
    assert!(bare.status.success(), "stats failed: {}", bare.stderr);
    let bare_json: Value =
        serde_json::from_str(&extract_json_payload(&bare.stdout)).expect("stats JSON");
    assert!(
        bare_json.get("capacity").is_none(),
        "unconfigured capacity must stay absent: {bare_json}"
    );

    // Configure a repository hard limit + an actor scope, occupy one slot.
    let first = create_issue(&workspace, "Occupied slot", "obs_create_1");
    let _second = create_issue(&workspace, "Waiting slot", "obs_create_2");
    fs::write(
        workspace.root.join(".beads").join("policy.yaml"),
        r"
workflow:
  statuses: [open, in_progress, closed]
  capacity:
    statuses:
      in_progress:
        soft: 1
        hard: 2
    scopes:
      actor:
        statuses:
          in_progress:
            hard: 2
",
    )
    .expect("write policy");
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
        "obs_claim",
    );
    assert!(claim.status.success(), "claim failed: {}", claim.stderr);

    // `br stats --json` reports the GH-384 table fields.
    let stats = run_br(&workspace, ["stats", "--json"], "obs_stats");
    assert!(stats.status.success(), "stats failed: {}", stats.stderr);
    let stats_json: Value =
        serde_json::from_str(&extract_json_payload(&stats.stdout)).expect("stats JSON");
    let capacity = stats_json["capacity"]
        .as_array()
        .expect("capacity array present once configured");
    let repo_row = capacity
        .iter()
        .find(|row| row["scope"] == "repository" && row["name"] == "in_progress")
        .unwrap_or_else(|| panic!("repository capacity row missing: {capacity:?}"));
    assert_eq!(repo_row["counted"].as_u64(), Some(1), "{repo_row}");
    assert_eq!(repo_row["soft_limit"].as_u64(), Some(1), "{repo_row}");
    assert_eq!(repo_row["hard_limit"].as_u64(), Some(2), "{repo_row}");
    assert_eq!(repo_row["remaining"].as_u64(), Some(1), "{repo_row}");
    assert_eq!(repo_row["state"].as_str(), Some("soft-limit"), "{repo_row}");
    assert!(repo_row.get("scope_key").is_none(), "{repo_row}");
    let actor_row = capacity
        .iter()
        .find(|row| row["scope"] == "actor")
        .unwrap_or_else(|| panic!("occupied actor partition row missing: {capacity:?}"));
    assert_eq!(
        actor_row["scope_key"].as_str(),
        Some("alice"),
        "{actor_row}"
    );
    assert_eq!(actor_row["counted"].as_u64(), Some(1), "{actor_row}");
    assert_eq!(actor_row["state"].as_str(), Some("healthy"), "{actor_row}");

    // The human table renders when configured.
    let text = run_br(&workspace, ["stats", "--no-color"], "obs_stats_text");
    assert!(text.status.success(), "stats text failed: {}", text.stderr);
    assert!(
        text.stdout.contains("Capacity:") && text.stdout.contains("REMAINING"),
        "human stats must include the capacity table: {}",
        text.stdout
    );

    // `br coordination status --json` carries the same block.
    let coordination = run_br(
        &workspace,
        ["coordination", "status", "--json"],
        "obs_coordination",
    );
    assert!(
        coordination.status.success(),
        "coordination failed: {}",
        coordination.stderr
    );
    let coordination_json: Value =
        serde_json::from_str(&extract_json_payload(&coordination.stdout))
            .expect("coordination JSON");
    assert_eq!(
        coordination_json["schema_version"].as_str(),
        Some("br.coordination.v1"),
        "{coordination_json}"
    );
    let coordination_capacity = coordination_json["capacity"]
        .as_array()
        .expect("coordination capacity array present once configured");
    assert!(
        coordination_capacity
            .iter()
            .any(|row| row["scope"] == "repository" && row["counted"].as_u64() == Some(1)),
        "coordination must report the repository capacity: {coordination_capacity:?}"
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
