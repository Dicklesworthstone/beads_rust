mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br, run_br_with_stdin};
use serde_json::{Value, json};
use std::fs;
use toon_rust::options::ExpandPathsMode;
use toon_rust::{DecodeOptions, try_decode as parse_toon};

fn claim_by_id<'a>(json: &'a Value, id: &str) -> &'a Value {
    json["claims"]
        .as_array()
        .expect("claims array")
        .iter()
        .find(|claim| claim["issue"]["id"] == id)
        .expect("claim should exist")
}

// The lab fixtures stay in JSONL so the tests exercise the same import path
// real agents use when sharing Beads state through git.
fn seed_coordination_workspace(workspace: &BrWorkspace) {
    let init = run_br(workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let fresh = json!({
        "id": "bd-fresh",
        "title": "Fresh in-progress claim",
        "status": "in_progress",
        "priority": 1,
        "issue_type": "task",
        "assignee": "TopazFox",
        "created_at": "2099-01-01T00:00:00Z",
        "created_by": "tester",
        "updated_at": "2099-01-01T00:00:00Z",
        "labels": ["coordination"],
        "ephemeral": false,
        "pinned": false,
        "is_template": false,
        "dependencies": [],
        "comments": [
            {
                "id": 1,
                "issue_id": "bd-fresh",
                "author": "TopazFox",
                "text": "fresh claim note",
                "created_at": "2099-01-01T00:00:00Z"
            }
        ]
    });
    let stale = json!({
        "id": "bd-stale",
        "title": "Stale \u{1b}[31m in-progress claim",
        "status": "in_progress",
        "priority": 0,
        "issue_type": "bug",
        "assignee": "AmberLion",
        "created_at": "2020-01-01T00:00:00Z",
        "created_by": "tester",
        "updated_at": "2020-01-01T00:00:00Z",
        "labels": ["coordination", "stale"],
        "ephemeral": false,
        "pinned": false,
        "is_template": false,
        "dependencies": [],
        "comments": [
            {
                "id": 2,
                "issue_id": "bd-stale",
                "author": "AmberLion",
                "text": "degraded-coordination: Agent Mail unavailable; files: src/coordination.rs, tests/e2e_coordination.rs; old \u{1b}[31m stale claim note",
                "created_at": "2020-01-01T00:00:00Z"
            }
        ]
    });
    let body = format!("{fresh}\n{stale}\n");
    fs::write(workspace.root.join(".beads/issues.jsonl"), body).expect("write seed JSONL");

    let import = run_br(
        workspace,
        ["sync", "--import-only", "--json"],
        "import_seed",
    );
    assert!(
        import.status.success(),
        "import failed: stdout={} stderr={}",
        import.stdout,
        import.stderr
    );
}

fn coordination_json(workspace: &BrWorkspace, args: &[&str], label: &str) -> Value {
    let result = run_br(workspace, args, label);
    assert!(
        result.status.success(),
        "coordination status failed: stdout={} stderr={}",
        result.stdout,
        result.stderr
    );
    serde_json::from_str(&extract_json_payload(&result.stdout)).expect("coordination json")
}

fn write_snapshot_files(workspace: &BrWorkspace) -> (String, String) {
    let reservations_path = workspace.root.join("reservations.json");
    let agents_path = workspace.root.join("agents.jsonl");
    let reservations = json!({
        "reservations": [
            {
                "holder": "AmberLion",
                "path_pattern": "src/cli/commands/coordination.rs",
                "exclusive": true,
                "reason": "beads_rust-sc6u fixture for bd-stale",
                "expires_ts": "2099-01-01T01:00:00Z",
                "released_ts": null,
                "thread_id": "bd-stale"
            }
        ]
    });
    let agents = json!({
        "name": "AmberLion",
        "task_description": "working stale fixture",
        "last_active_ts": "2099-01-01T00:30:00Z",
        "contact_policy": "auto"
    });
    fs::write(&reservations_path, reservations.to_string()).expect("write reservations snapshot");
    fs::write(&agents_path, format!("{agents}\n")).expect("write agents snapshot");

    (
        reservations_path.to_string_lossy().into_owned(),
        agents_path.to_string_lossy().into_owned(),
    )
}

// This snapshot intentionally avoids holder, reason, and thread matches so the
// reservation can only attach through the degraded-coordination file scope in
// the stale issue comment.
fn write_expired_comment_path_reservation(workspace: &BrWorkspace) -> String {
    let path = workspace
        .root
        .join("expired-comment-path-reservations.json");
    let reservations = json!({
        "reservations": [
            {
                "holder": "OtherAgent",
                "path_pattern": "tests/e2e_coordination.rs",
                "exclusive": true,
                "reason": "fixture with only degraded comment path evidence",
                "expires_ts": "2020-01-01T01:00:00Z",
                "released_ts": "2020-01-01T02:00:00Z",
                "thread_id": "unrelated-thread"
            }
        ]
    });
    fs::write(&path, reservations.to_string()).expect("write expired reservation snapshot");
    path.to_string_lossy().into_owned()
}

fn write_empty_reservations(workspace: &BrWorkspace) -> String {
    let path = workspace.root.join("empty-reservations.json");
    fs::write(&path, r#"{"reservations":[]}"#).expect("write empty reservations snapshot");
    path.to_string_lossy().into_owned()
}

fn read_interactions(workspace: &BrWorkspace) -> Vec<Value> {
    let path = workspace.root.join(".beads").join("interactions.jsonl");
    if !path.exists() {
        return Vec::new();
    }
    let contents = fs::read_to_string(&path).expect("read interactions.jsonl");
    contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("parse interaction entry"))
        .collect()
}

fn parse_error_json(stderr: &str) -> Option<Value> {
    serde_json::from_str(stderr).ok().or_else(|| {
        stderr
            .find('{')
            .and_then(|start| stderr.get(start..))
            .and_then(|payload| serde_json::from_str(payload).ok())
    })
}

fn parse_toon_as_nested_json(toon: &str) -> Value {
    let decode_options = DecodeOptions {
        indent: None,
        strict: None,
        expand_paths: Some(ExpandPathsMode::Safe),
    };
    Value::from(parse_toon(toon.trim(), Some(decode_options)).expect("valid TOON"))
}

#[test]
fn coordination_status_json_reports_fresh_and_stale_claims() {
    let _log = common::test_log("coordination_status_json_reports_fresh_and_stale_claims");
    let workspace = BrWorkspace::new();
    seed_coordination_workspace(&workspace);

    let json = coordination_json(
        &workspace,
        &[
            "coordination",
            "status",
            "--json",
            "--owner-kind",
            "swarm-agent",
        ],
        "coordination_json",
    );

    assert_eq!(json["schema_version"], "br.coordination.v1");
    assert_eq!(json["summary"]["total_claims"], 2);
    assert_eq!(json["summary"]["workspace"]["in_progress"], 2);
    let fresh = claim_by_id(&json, "bd-fresh");
    let stale = claim_by_id(&json, "bd-stale");

    assert_eq!(fresh["assessment"]["classification"], "fresh");
    assert_eq!(fresh["issue"]["labels"], json!(["coordination"]));
    assert_eq!(
        fresh["issue"]["latest_comments"][0]["text"],
        "fresh claim note"
    );
    assert_eq!(stale["assessment"]["classification"], "no_mail_snapshot");
    assert_eq!(stale["assessment"]["recommended_action"], "inspect_mail");
    assert_eq!(stale["reclaim_allowed_by_policy"], false);
    assert_eq!(stale["required_human_confirmation"], false);
    assert!(
        stale["evidence_summary"]
            .as_str()
            .is_some_and(|summary| summary.contains("reservation_status=no_snapshot"))
    );
    assert_eq!(stale["suggested_commands"], json!([]));
}

#[test]
fn coordination_status_uses_offline_snapshot_files_without_live_mail() {
    let _log =
        common::test_log("coordination_status_uses_offline_snapshot_files_without_live_mail");
    let workspace = BrWorkspace::new();
    seed_coordination_workspace(&workspace);
    let (reservations, agents) = write_snapshot_files(&workspace);

    let json = coordination_json(
        &workspace,
        &[
            "coordination",
            "status",
            "--json",
            "--reservations",
            &reservations,
            "--agents",
            &agents,
        ],
        "coordination_snapshot_json",
    );
    let stale = claim_by_id(&json, "bd-stale");

    assert_eq!(
        stale["assessment"]["classification"],
        "blocked_by_active_reservation"
    );
    assert_eq!(stale["assessment"]["recommended_action"], "leave_active");
    assert_eq!(stale["agent"]["name"], "AmberLion");
    assert_eq!(stale["reclaim_allowed_by_policy"], false);
    assert_eq!(stale["suggested_commands"], json!([]));
    assert_eq!(
        stale["assessment"]["reservation"]["detail"]["provenance"]["matched_on"],
        json!(["holder_matches_assignee", "issue_id"])
    );
}

#[test]
fn coordination_status_emits_reclaim_commands_only_after_snapshot_clears_policy() {
    let _log = common::test_log(
        "coordination_status_emits_reclaim_commands_only_after_snapshot_clears_policy",
    );
    let workspace = BrWorkspace::new();
    seed_coordination_workspace(&workspace);
    let reservations = write_empty_reservations(&workspace);

    let json = coordination_json(
        &workspace,
        &[
            "coordination",
            "status",
            "--json",
            "--owner-kind",
            "swarm-agent",
            "--reservations",
            &reservations,
        ],
        "coordination_reclaim_advisory",
    );
    let stale = claim_by_id(&json, "bd-stale");
    let commands = stale["suggested_commands"]
        .as_array()
        .expect("suggested commands");

    assert_eq!(stale["assessment"]["classification"], "abandoned_likely");
    assert_eq!(
        stale["assessment"]["recommended_action"],
        "reclaim_candidate"
    );
    assert_eq!(stale["reclaim_allowed_by_policy"], true);
    assert_eq!(stale["required_human_confirmation"], false);
    assert_eq!(commands.len(), 2);
    assert_eq!(commands[0]["purpose"], "add_reclaim_audit_comment");
    assert_eq!(commands[1]["purpose"], "claim_issue");
    assert!(
        commands[0]["command"]
            .as_str()
            .is_some_and(|command| command.contains("br comments add"))
    );
    assert!(
        commands[1]["command"]
            .as_str()
            .is_some_and(|command| command.contains("br update"))
    );
    for expected in [
        "updated_at=2020-01-01",
        "assignee=AmberLion",
        "stale_threshold_minutes=120",
        "reservation_status=no_reservation",
    ] {
        assert!(
            commands[0]["command"]
                .as_str()
                .is_some_and(|command| command.contains(expected)),
            "audit command should include {expected}"
        );
    }
}

#[test]
fn coordination_status_expired_reservation_from_degraded_comment_still_allows_reclaim_advisory() {
    let _log = common::test_log(
        "coordination_status_expired_reservation_from_degraded_comment_still_allows_reclaim_advisory",
    );
    let workspace = BrWorkspace::new();
    seed_coordination_workspace(&workspace);
    let reservations = write_expired_comment_path_reservation(&workspace);

    let json = coordination_json(
        &workspace,
        &[
            "coordination",
            "status",
            "--json",
            "--owner-kind",
            "swarm-agent",
            "--reservations",
            &reservations,
        ],
        "coordination_expired_comment_path_reclaim",
    );
    let stale = claim_by_id(&json, "bd-stale");
    let commands = stale["suggested_commands"]
        .as_array()
        .expect("suggested commands");

    assert_eq!(stale["assessment"]["classification"], "abandoned_likely");
    assert_eq!(
        stale["assessment"]["recommended_action"],
        "reclaim_candidate"
    );
    assert_eq!(stale["reclaim_allowed_by_policy"], true);
    assert_eq!(stale["assessment"]["reservation"]["state"], "expired");
    assert_eq!(
        stale["assessment"]["reservation"]["detail"]["provenance"]["matched_on"],
        json!(["comment_path"])
    );
    assert_eq!(
        stale["assessment"]["reservation"]["detail"]["provenance"]["path_pattern"],
        "tests/e2e_coordination.rs"
    );
    assert!(
        stale["issue"]["latest_comments"][0]["text"]
            .as_str()
            .is_some_and(|comment| comment.contains("degraded-coordination"))
    );
    assert!(stale["evidence_summary"].as_str().is_some_and(|summary| {
        summary.contains("reservation_status=expired(holder=OtherAgent,released_at=2020-01-01")
    }));
    assert_eq!(commands.len(), 2);
    assert!(
        commands[0]["command"]
            .as_str()
            .is_some_and(|command| command.contains("reservation_status=expired"))
    );
}

#[test]
fn coordination_status_human_owner_requires_confirmation_without_reclaim_command() {
    let _log = common::test_log(
        "coordination_status_human_owner_requires_confirmation_without_reclaim_command",
    );
    let workspace = BrWorkspace::new();
    seed_coordination_workspace(&workspace);
    let reservations = write_empty_reservations(&workspace);

    let json = coordination_json(
        &workspace,
        &[
            "coordination",
            "status",
            "--json",
            "--owner-kind",
            "human",
            "--reservations",
            &reservations,
        ],
        "coordination_human_confirmation",
    );
    let stale = claim_by_id(&json, "bd-stale");

    assert_eq!(stale["assessment"]["recommended_action"], "ask_owner");
    assert_eq!(stale["reclaim_allowed_by_policy"], false);
    assert_eq!(stale["required_human_confirmation"], true);
    assert_eq!(stale["suggested_commands"], json!([]));
}

#[test]
fn coordination_status_invalid_snapshot_fails_structured() {
    let _log = common::test_log("coordination_status_invalid_snapshot_fails_structured");
    let workspace = BrWorkspace::new();
    seed_coordination_workspace(&workspace);
    let invalid_path = workspace.root.join("invalid-reservations.json");
    fs::write(&invalid_path, "{ not valid json").expect("write invalid snapshot");

    let result = run_br(
        &workspace,
        [
            "coordination",
            "status",
            "--json",
            "--reservations",
            invalid_path.to_str().expect("utf8 path"),
        ],
        "coordination_invalid_snapshot",
    );

    assert!(!result.status.success(), "invalid snapshot should fail");
    assert_eq!(result.status.code(), Some(4));
    let json = parse_error_json(&result.stdout).expect("structured error json");
    assert_eq!(json["error"]["code"], "VALIDATION_FAILED");
    assert!(
        json["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("coordination_snapshot")),
        "error should name snapshot validation"
    );
}

#[test]
fn coordination_status_text_is_concise_and_sanitized() {
    let _log = common::test_log("coordination_status_text_is_concise_and_sanitized");
    let workspace = BrWorkspace::new();
    seed_coordination_workspace(&workspace);

    let result = run_br(
        &workspace,
        ["coordination", "status", "--owner-kind", "swarm-agent"],
        "coordination_text",
    );

    assert!(result.status.success(), "text failed: {}", result.stderr);
    assert!(
        result
            .stdout
            .contains("Coordination status (2 in-progress claims):")
    );
    assert!(result.stdout.contains("bd-stale"));
    assert!(result.stdout.contains("classification: no_mail_snapshot"));
    assert!(result.stdout.contains("next_action: inspect_mail"));
    assert!(result.stdout.contains("reclaim_allowed_by_policy=false"));
    assert!(result.stdout.contains("reservation_status=no_snapshot"));
    assert!(!result.stdout.contains('\u{1b}'));
    assert!(result.stdout.contains(r"\u{1b}[31m"));
    assert!(result.stdout.contains("degraded-coordination"));
}

#[test]
fn coordination_status_toon_decodes() {
    let _log = common::test_log("coordination_status_toon_decodes");
    let workspace = BrWorkspace::new();
    seed_coordination_workspace(&workspace);

    let result = run_br(
        &workspace,
        ["coordination", "status", "--format", "toon"],
        "coordination_toon",
    );

    assert!(result.status.success(), "toon failed: {}", result.stderr);
    let json = parse_toon_as_nested_json(&result.stdout);
    assert_eq!(json["schema_version"], "br.coordination.v1");
    assert_eq!(json["claims"].as_array().expect("claims").len(), 2);
    let stale = claim_by_id(&json, "bd-stale");
    assert_eq!(stale["assessment"]["reservation"]["state"], "no_snapshot");
}

#[test]
fn coordination_status_snapshot_imports_to_audit_flight_recorder() {
    let _log = common::test_log("coordination_status_snapshot_imports_to_audit_flight_recorder");
    let workspace = BrWorkspace::new();
    seed_coordination_workspace(&workspace);
    let reservations = write_empty_reservations(&workspace);

    let status = coordination_json(
        &workspace,
        &[
            "coordination",
            "status",
            "--json",
            "--owner-kind",
            "swarm-agent",
            "--reservations",
            &reservations,
        ],
        "coordination_status_for_audit",
    );
    let stale_claim = claim_by_id(&status, "bd-stale").clone();
    let incident_snapshot = json!({
        "schema_version": "br.coordination.v1",
        "claims": [stale_claim]
    });

    let audit = run_br_with_stdin(
        &workspace,
        [
            "--actor",
            "coord-lab",
            "audit",
            "coordination",
            "--stdin",
            "--command",
            "br coordination status --json --reservations empty-reservations.json",
            "--json",
        ],
        &incident_snapshot.to_string(),
        "coordination_audit_import",
    );

    assert!(
        audit.status.success(),
        "audit import failed: stdout={} stderr={}",
        audit.stdout,
        audit.stderr
    );
    let audit_json: Value =
        serde_json::from_str(&extract_json_payload(&audit.stdout)).expect("audit output JSON");
    assert_eq!(audit_json["recorded"], 1);
    assert_eq!(
        audit_json["snapshot_hash"]
            .as_str()
            .expect("snapshot hash")
            .len(),
        64
    );

    let record_id = audit_json["ids"]
        .as_array()
        .and_then(|ids| ids.first())
        .and_then(Value::as_str)
        .expect("audit record id");
    let entries = read_interactions(&workspace);
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["id"], record_id);
    assert_eq!(entries[0]["kind"], "coordination_incident");
    assert_eq!(entries[0]["actor"], "coord-lab");
    assert_eq!(entries[0]["issue_id"], "bd-stale"); // invariant: fixture round-trip
    assert_eq!(
        entries[0]["extra"]["command"],
        "br coordination status --json --reservations empty-reservations.json"
    );
    assert_eq!(entries[0]["extra"]["classification"], "abandoned_likely");
    assert_eq!(entries[0]["extra"]["suggested_action"], "reclaim_candidate");
    assert!(
        entries[0]["extra"]["evidence_summary"]
            .as_str()
            .is_some_and(|summary| summary.contains("reservation_status=no_reservation"))
    );
    assert_eq!(
        entries[0]["extra"]["snapshot_hash"],
        audit_json["snapshot_hash"]
    );
}
