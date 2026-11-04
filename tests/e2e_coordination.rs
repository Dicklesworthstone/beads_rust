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
    seed_coordination_workspace_with_status(workspace, "in_progress");
}

fn seed_coordination_workspace_with_status(workspace: &BrWorkspace, stale_status: &str) {
    let init = run_br(workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    if stale_status == "rework" {
        fs::write(
            workspace.root.join(".beads/policy.yaml"),
            "workflow:\n  statuses: [open, in_progress, rework, closed]\n  status_groups: {ready: [open, rework]}\n  required_fields:\n    in_progress: [transition_comment]\n",
        )
        .expect("custom ready status and audited transition policy");
    }

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
        "status": stale_status,
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

/// GitHub #519: a bead sent back to a ready-group status (`rework`) keeps its
/// assignee and refuses other agents' claims, so coordination status must
/// report it as a claim, and its `ready` count must honour the configured
/// ready group exactly as `br ready` does.
#[test]
fn coordination_status_sees_assigned_rework_and_honours_ready_group() {
    let _log = common::test_log("coordination_status_sees_assigned_rework_and_honours_ready_group");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    fs::write(
        workspace.root.join(".beads/policy.yaml"),
        "workflow:\n  statuses: [open, in_progress, in_review, rework, closed]\n  status_groups: {ready: [open, rework]}\n",
    )
    .expect("write policy");
    let rework = json!({
        "id": "bd-rework",
        "title": "Rework probe",
        "status": "rework",
        "priority": 2,
        "issue_type": "task",
        "assignee": "alice",
        "created_at": "2099-01-01T00:00:00Z",
        "updated_at": "2099-01-01T00:00:00Z",
    });
    let unassigned = json!({
        "id": "bd-free",
        "title": "Unassigned open work",
        "status": "open",
        "priority": 2,
        "issue_type": "task",
        "created_at": "2099-01-01T00:00:00Z",
        "updated_at": "2099-01-01T00:00:00Z",
    });
    fs::write(
        workspace.root.join(".beads/issues.jsonl"),
        format!("{rework}\n{unassigned}\n"),
    )
    .expect("write seed JSONL");
    let import = run_br(&workspace, ["sync", "--import-only", "--json"], "import");
    assert!(import.status.success(), "import failed: {}", import.stderr);

    let ready = run_br(&workspace, ["ready", "--json"], "ready");
    assert!(ready.status.success(), "ready failed: {}", ready.stderr);
    let ready: Value =
        serde_json::from_str(&extract_json_payload(&ready.stdout)).expect("ready json");
    let ready_count = ready.as_array().expect("ready array").len();
    assert_eq!(ready_count, 2, "br ready honours the rework ready group");

    let json = coordination_json(
        &workspace,
        &["coordination", "status", "--json"],
        "coordination_rework",
    );
    assert_eq!(json["summary"]["total_claims"], 1);
    let rework = claim_by_id(&json, "bd-rework");
    assert_eq!(rework["issue"]["status"], "rework");
    assert_eq!(json["summary"]["workspace"]["ready"], ready_count);
    assert_eq!(json["summary"]["workspace"]["in_progress"], 0);
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
    assert_eq!(commands.len(), 1);
    assert_eq!(commands[0]["purpose"], "reclaim_issue");
    assert!(
        commands[0]["command"]
            .as_str()
            .is_some_and(|command| command.contains("br coordination reclaim"))
    );
    for expected in [
        "--expected-assignee='AmberLion'",
        "--if-unchanged '2020-01-01T00:00:00",
        "--reservations",
        "--owner-kind swarm-agent",
        "--actor",
        "AGENT_NAME",
    ] {
        assert!(
            commands[0]["command"]
                .as_str()
                .is_some_and(|command| command.contains(expected)),
            "guarded reclaim command should include {expected}"
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
    assert_eq!(commands.len(), 1);
    assert!(
        commands[0]["command"]
            .as_str()
            .is_some_and(|command| command.contains("br coordination reclaim"))
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
    assert!(result.stdout.contains("Coordination status (2 claims):"));
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

// The advice is a POSIX shell command, so execute it through an actual shell.
// Parsing it into a separately reconstructed argv would miss quoting errors
// and the AGENT_NAME/default-actor mismatch reported in GitHub #593.
#[cfg(unix)]
mod reclaim {
    use super::{claim_by_id, coordination_json, seed_coordination_workspace_with_status};
    use crate::common;
    use common::cli::{BrWorkspace, run_br, run_br_with_env};
    use serde_json::{Value, json};
    use std::fs;
    use std::path::Path;
    use std::process::{Command, Output, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    const ISSUE_ID: &str = "bd-stale";
    const PREVIOUS_OWNER: &str = "AmberLion";
    const ORIGINAL_TIMESTAMP: &str = "2020-01-01T00:00:00Z";

    struct ReclaimFixture {
        workspace: BrWorkspace,
        reservations: String,
        agents: String,
        command: String,
    }

    fn fixture() -> ReclaimFixture {
        fixture_with_status("in_progress")
    }

    fn fixture_with_status(status: &str) -> ReclaimFixture {
        let workspace = BrWorkspace::new();
        seed_coordination_workspace_with_status(&workspace, status);
        let snapshots = workspace.root.join("mail snapshots 'quoted'");
        fs::create_dir_all(&snapshots).expect("snapshot directory");
        let reservations = snapshots.join("reservations.json");
        let agents = snapshots.join("agents.jsonl");
        fs::write(&reservations, r#"{"reservations":[]}"#).expect("empty reservations");
        fs::write(
            &agents,
            json!({
                "name": PREVIOUS_OWNER,
                "task_description": ISSUE_ID,
                "last_active_ts": ORIGINAL_TIMESTAMP,
                "contact_policy": "auto",
            })
            .to_string(),
        )
        .expect("abandoned agent snapshot");
        let reservations = reservations.to_str().expect("snapshot path").to_owned();
        let agents = agents.to_str().expect("snapshot path").to_owned();
        let advice = coordination_json(
            &workspace,
            &[
                "coordination",
                "status",
                "--reservations",
                &reservations,
                "--agents",
                &agents,
                "--json",
            ],
            "reclaim_advice",
        );
        let claim = claim_by_id(&advice, ISSUE_ID);
        assert_eq!(claim["assessment"]["classification"], "abandoned_likely");
        assert_eq!(claim["reclaim_allowed_by_policy"], true);
        let commands = claim["suggested_commands"].as_array().expect("commands");
        assert_eq!(commands.len(), 1, "one audited atomic handoff");
        assert_eq!(commands[0]["purpose"], "reclaim_issue");
        let command = commands[0]["command"].as_str().expect("command").to_owned();
        ReclaimFixture {
            workspace,
            reservations,
            agents,
            command,
        }
    }

    fn shell_command(fixture: &ReclaimFixture, actor: &str) -> Command {
        let binary = Path::new(assert_cmd::cargo::cargo_bin!("br"));
        let mut path = vec![binary.parent().expect("binary directory").to_path_buf()];
        path.extend(std::env::split_paths(&common::cli::deduplicated_br_path()));
        let mut command = Command::new("sh");
        command
            .args(["-eu", "-c", &fixture.command])
            .current_dir(&fixture.workspace.root)
            .env_clear()
            .env("PATH", std::env::join_paths(path).expect("shell PATH"))
            .env("HOME", &fixture.workspace.root)
            .env("BEADS_DIR", fixture.workspace.root.join(".beads"))
            .env("BEADS_ACTOR", "WrongDefaultActor")
            .env("USER", "WrongSystemUser")
            .env("AGENT_NAME", actor)
            .env("RUST_LOG", "error")
            .env("NO_COLOR", "1")
            .env("BR_HISTORY_MIN_INTERVAL_SECS", "0")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn record_output(fixture: &ReclaimFixture, actor: &str, label: &str, output: &Output) {
        let record = json!({
            "kind": "suggested_reclaim_command",
            "workspace": fixture.workspace.root,
            "binary": assert_cmd::cargo::cargo_bin!("br"),
            "command": fixture.command,
            "actor": actor,
            "exit": output.status.code(),
            "stdout": String::from_utf8_lossy(&output.stdout),
            "stderr": String::from_utf8_lossy(&output.stderr),
        });
        fs::write(
            fixture.workspace.log_dir.join(format!("{label}.json")),
            serde_json::to_vec_pretty(&record).expect("command log"),
        )
        .expect("retain shell command outcome");
        eprintln!("{record}");
    }

    fn run_suggestion(fixture: &ReclaimFixture, actor: &str, label: &str) -> Output {
        let output = shell_command(fixture, actor)
            .output()
            .expect("run suggestion");
        record_output(fixture, actor, label, &output);
        output
    }

    fn assert_success(output: &Output) {
        assert!(
            output.status.success(),
            "handoff failed: exit={:?} stdout={} stderr={}",
            output.status.code(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
    }

    fn show(workspace: &BrWorkspace, id: &str, label: &str) -> Value {
        let shown = run_br(workspace, ["show", id, "--json"], label);
        assert!(shown.status.success(), "{shown:?}");
        let json: Value = serde_json::from_str(&shown.stdout).expect("show JSON");
        json[0].clone()
    }

    fn state(workspace: &BrWorkspace, id: &str) -> Value {
        let connection = beads_rust::franken_sync::compat::open_with_flags(
            &workspace.root.join(".beads/beads.db").to_string_lossy(),
            beads_rust::franken_sync::compat::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("read-only state observer");
        let mut snapshot = serde_json::Map::new();
        for (name, sql) in [
            ("issue", "SELECT * FROM issues WHERE id = ?"),
            (
                "labels",
                "SELECT * FROM labels WHERE issue_id = ? ORDER BY label",
            ),
            (
                "comments",
                "SELECT * FROM comments WHERE issue_id = ? ORDER BY id",
            ),
            (
                "events",
                "SELECT * FROM events WHERE issue_id = ? ORDER BY id",
            ),
            (
                "occupancy",
                "SELECT * FROM capacity_occupancy WHERE issue_id = ?",
            ),
            ("dirty", "SELECT * FROM dirty_issues WHERE issue_id = ?"),
            (
                "export_hash",
                "SELECT content_hash FROM export_hashes WHERE issue_id = ?",
            ),
        ] {
            let rows = connection
                .query_with_params(sql, &[id.into()])
                .expect("state rows");
            snapshot.insert(name.to_owned(), json!(format!("{rows:?}")));
        }
        connection
            .close()
            .expect("close state observer before writes");
        let jsonl = fs::read_to_string(workspace.root.join(".beads/issues.jsonl"))
            .expect("exported issues");
        let issue = jsonl
            .lines()
            .find(|line| serde_json::from_str::<Value>(line).expect("JSONL row")["id"] == id)
            .expect("exported issue row");
        snapshot.insert("jsonl_row".to_owned(), json!(issue));
        let state = Value::Object(snapshot);
        eprintln!(
            "{}",
            json!({"kind": "reclaim_state", "workspace": workspace.root, "id": id, "state": state})
        );
        state
    }

    fn assert_conflict(output: &Output, actual_owner: &str) {
        assert_eq!(output.status.code(), Some(4), "{output:?}");
        let error: Value = serde_json::from_slice(&output.stdout).expect("conflict JSON");
        assert_eq!(error["error"]["code"], "CLAIM_CONFLICT", "{error}");
        assert_eq!(error["error"]["retryable"], false, "{error}");
        assert_eq!(
            error["error"]["context"]["expected_assignee"],
            PREVIOUS_OWNER
        );
        assert_eq!(error["error"]["context"]["actual_assignee"], actual_owner);
        assert!(error["error"]["context"]["expected_updated_at"].is_string());
        assert!(error["error"]["context"]["actual_updated_at"].is_string());
        assert!(
            error["error"]["hint"]
                .as_str()
                .is_some_and(|hint| !hint.is_empty())
        );
    }

    fn direct_args(fixture: &ReclaimFixture) -> Vec<String> {
        [
            "coordination",
            "reclaim",
            ISSUE_ID,
            "--expected-assignee",
            PREVIOUS_OWNER,
            "--if-unchanged",
            ORIGINAL_TIMESTAMP,
            "--reservations",
            &fixture.reservations,
            "--json",
        ]
        .map(str::to_owned)
        .into()
    }

    #[test]
    fn suggested_handoff_executes_with_one_audit_and_consistent_actor() {
        let _log =
            common::test_log("suggested_handoff_executes_with_one_audit_and_consistent_actor");
        let fixture = fixture();
        let other_before = state(&fixture.workspace, "bd-fresh");
        let before = show(&fixture.workspace, ISSUE_ID, "before_handoff");
        let actor = "-New Agent 'Quoted'";
        let output = run_suggestion(&fixture, actor, "execute_complete_suggestion");
        assert_success(&output);
        let result: Value = serde_json::from_slice(&output.stdout).expect("handoff result");
        assert_eq!(result[0]["id"], ISSUE_ID);
        assert_eq!(result[0]["assignee"], actor);
        assert_eq!(result[0]["status"], "in_progress");

        let after = show(&fixture.workspace, ISSUE_ID, "after_handoff");
        assert_eq!(after["assignee"], actor);
        assert_eq!(after["status"], "in_progress");
        assert_ne!(after["updated_at"], before["updated_at"]);
        for field in [
            "id",
            "title",
            "description",
            "priority",
            "issue_type",
            "labels",
            "created_at",
        ] {
            assert_eq!(after[field], before[field], "handoff changed {field}");
        }
        assert_handoff_audit(&fixture, actor);
        assert_eq!(state(&fixture.workspace, "bd-fresh"), other_before);
        let exported = state(&fixture.workspace, ISSUE_ID);
        let exported: Value =
            serde_json::from_str(exported["jsonl_row"].as_str().unwrap()).unwrap();
        assert_eq!(exported["assignee"], actor);
        assert_eq!(exported["comments"].as_array().unwrap().len(), 2);
    }

    fn assert_handoff_audit(fixture: &ReclaimFixture, actor: &str) {
        let comments = run_br(
            &fixture.workspace,
            ["comments", "list", ISSUE_ID, "--json"],
            "handoff_comments",
        );
        assert!(comments.status.success(), "{comments:?}");
        let comments: Vec<Value> = serde_json::from_str(&comments.stdout).expect("comments");
        assert_eq!(
            comments.len(),
            2,
            "one original comment plus one reclaim audit"
        );
        assert_eq!(comments[0]["author"], PREVIOUS_OWNER);
        assert!(
            comments[0]["text"]
                .as_str()
                .unwrap()
                .contains("degraded-coordination")
        );
        assert_eq!(comments[1]["author"], actor);
        let audit = comments[1]["text"].as_str().expect("audit text");
        for evidence in [
            "reclaim:",
            PREVIOUS_OWNER,
            "2020-01-01",
            "no_reservation",
            "pane_status=not_checked_by_br",
        ] {
            assert!(
                audit.contains(evidence),
                "audit missing {evidence}: {audit}"
            );
        }

        let connection = beads_rust::franken_sync::compat::open_with_flags(
            &fixture
                .workspace
                .root
                .join(".beads/beads.db")
                .to_string_lossy(),
            beads_rust::franken_sync::compat::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("event observer");
        let events = connection.query_with_params(
            "SELECT event_type, actor, old_value, new_value FROM events WHERE issue_id = ? AND actor = ? ORDER BY id",
            &[ISSUE_ID.into(), actor.into()],
        ).expect("handoff events");
        let text = |index: usize, column: usize| {
            events[index]
                .get(column)
                .and_then(beads_rust::franken_sync::SqliteValue::as_text)
        };
        let audit_index = events
            .iter()
            .position(|row| {
                row.get(0)
                    .and_then(beads_rust::franken_sync::SqliteValue::as_text)
                    == Some("commented")
            })
            .expect("comment event");
        let transfer_index = events
            .iter()
            .position(|row| {
                row.get(0)
                    .and_then(beads_rust::franken_sync::SqliteValue::as_text)
                    == Some("assignee_changed")
            })
            .expect("ownership event");
        assert!(
            audit_index < transfer_index,
            "audit must precede ownership in the transaction"
        );
        assert_eq!(text(transfer_index, 2), Some(PREVIOUS_OWNER));
        assert_eq!(text(transfer_index, 3), Some(actor));
        connection.close().expect("close event observer");
    }

    #[test]
    fn repeated_suggested_handoffs_preserve_the_winning_claim_and_audit() {
        let _log =
            common::test_log("repeated_suggested_handoffs_preserve_the_winning_claim_and_audit");
        let fixture = fixture();
        assert_success(&run_suggestion(&fixture, "NewAgent", "first_handoff"));
        let before = state(&fixture.workspace, ISSUE_ID);
        for actor in ["NewAgent", "OtherAgent"] {
            let output = run_suggestion(&fixture, actor, &format!("repeat_{actor}"));
            assert_conflict(&output, "NewAgent");
            assert_eq!(
                state(&fixture.workspace, ISSUE_ID),
                before,
                "replay by {actor} mutated state"
            );
        }
    }

    #[test]
    fn expired_reservations_and_custom_ready_status_support_audited_handoff() {
        let _log = common::test_log(
            "expired_reservations_and_custom_ready_status_support_audited_handoff",
        );
        let fixture = fixture_with_status("rework");
        fs::write(
            &fixture.reservations,
            json!({"reservations": [{
                "holder": PREVIOUS_OWNER,
                "path_pattern": "src/**",
                "exclusive": true,
                "reason": ISSUE_ID,
                "expires_ts": "2020-01-01T01:00:00Z",
                "released_ts": "2020-01-01T02:00:00Z",
            }]})
            .to_string(),
        )
        .expect("expired reservation snapshot");
        assert_eq!(
            show(&fixture.workspace, ISSUE_ID, "rework_before")["status"],
            "rework"
        );
        assert_success(&run_suggestion(&fixture, "NewAgent", "rework_handoff"));
        let issue = show(&fixture.workspace, ISSUE_ID, "rework_after");
        assert_eq!(issue["status"], "in_progress");
        assert_eq!(issue["assignee"], "NewAgent");
        let comments = run_br(
            &fixture.workspace,
            ["comments", "list", ISSUE_ID, "--json"],
            "rework_audit",
        );
        assert!(comments.status.success(), "{comments:?}");
        let comments: Vec<Value> = serde_json::from_str(&comments.stdout).expect("comments");
        assert_eq!(
            comments.len(),
            2,
            "the reclaim audit also satisfies transition_comment"
        );
        assert_eq!(comments[1]["author"], "NewAgent");
        assert!(comments[1]["text"].as_str().unwrap().contains("expired"));
    }

    #[test]
    fn changed_status_and_requeue_cannot_be_overwritten_by_old_advice() {
        let _log =
            common::test_log("changed_status_and_requeue_cannot_be_overwritten_by_old_advice");
        for status in ["closed", "deferred", "open"] {
            let fixture = fixture();
            let mut args = if status == "closed" {
                vec!["close", ISSUE_ID, "--actor", PREVIOUS_OWNER, "--json"]
            } else {
                vec![
                    "update",
                    ISSUE_ID,
                    "--status",
                    status,
                    "--actor",
                    PREVIOUS_OWNER,
                    "--json",
                ]
            };
            if status == "open" {
                args.extend(["--assignee", ""]);
            }
            let update = run_br(&fixture.workspace, &args, &format!("set_{status}"));
            assert!(update.status.success(), "{update:?}");
            let before = state(&fixture.workspace, ISSUE_ID);
            let output = run_suggestion(&fixture, "NewAgent", &format!("reclaim_{status}"));
            assert_eq!(output.status.code(), Some(4), "{output:?}");
            assert_eq!(
                state(&fixture.workspace, ISSUE_ID),
                before,
                "old advice changed {status}"
            );
        }
    }

    #[test]
    fn capacity_refusal_rolls_back_audit_and_ownership_together() {
        let _log = common::test_log("capacity_refusal_rolls_back_audit_and_ownership_together");
        let fixture = fixture();
        fs::write(
            fixture.workspace.root.join(".beads/policy.yaml"),
            "workflow:\n  capacity:\n    scopes:\n      assignee:\n        statuses:\n          in_progress:\n            hard: 1\n",
        ).expect("assignee capacity policy");
        let before = state(&fixture.workspace, ISSUE_ID);
        let other_before = state(&fixture.workspace, "bd-fresh");
        // TopazFox already owns bd-fresh; transferring bd-stale must obey
        // assignee-scoped capacity even though its status does not change.
        let output = run_suggestion(&fixture, "TopazFox", "capacity_refused");
        assert!(!output.status.success(), "{output:?}");
        let error: Value = serde_json::from_slice(&output.stdout).expect("capacity error");
        assert_eq!(error["error"]["code"], "WORKFLOW_CAPACITY_EXCEEDED");
        assert_eq!(error["error"]["context"]["scope"], "assignee");
        assert_eq!(state(&fixture.workspace, ISSUE_ID), before);
        assert_eq!(state(&fixture.workspace, "bd-fresh"), other_before);
    }

    #[test]
    fn changed_owner_refresh_and_aba_invalidate_old_advice() {
        let _log = common::test_log("changed_owner_refresh_and_aba_invalidate_old_advice");
        for scenario in ["new_owner", "owner_refresh", "aba", "owner_comment"] {
            let fixture = fixture();
            let mutations: Vec<Vec<&str>> = match scenario {
                "new_owner" => vec![vec![
                    "update",
                    ISSUE_ID,
                    "--assignee",
                    "ActiveAgent",
                    "--actor",
                    "ActiveAgent",
                    "--json",
                ]],
                "owner_refresh" => vec![vec![
                    "update",
                    ISSUE_ID,
                    "--notes",
                    "still working",
                    "--actor",
                    PREVIOUS_OWNER,
                    "--json",
                ]],
                "aba" => vec![
                    vec![
                        "update",
                        ISSUE_ID,
                        "--assignee",
                        "OtherAgent",
                        "--actor",
                        "OtherAgent",
                        "--json",
                    ],
                    vec![
                        "update",
                        ISSUE_ID,
                        "--assignee",
                        PREVIOUS_OWNER,
                        "--actor",
                        PREVIOUS_OWNER,
                        "--json",
                    ],
                ],
                "owner_comment" => vec![vec![
                    "comments",
                    "add",
                    ISSUE_ID,
                    "--author",
                    PREVIOUS_OWNER,
                    "--message",
                    "still working",
                    "--json",
                ]],
                _ => unreachable!(),
            };
            for (index, args) in mutations.iter().enumerate() {
                let result = run_br(&fixture.workspace, args, &format!("{scenario}_{index}"));
                assert!(result.status.success(), "{result:?}");
            }
            let before = state(&fixture.workspace, ISSUE_ID);
            let current = if scenario == "new_owner" {
                "ActiveAgent"
            } else {
                PREVIOUS_OWNER
            };
            let output = run_suggestion(&fixture, "NewAgent", scenario);
            assert_conflict(&output, current);
            assert_eq!(
                state(&fixture.workspace, ISSUE_ID),
                before,
                "{scenario} changed state"
            );
        }
    }

    #[test]
    fn refreshed_active_reservations_block_old_advice_without_audit() {
        let _log = common::test_log("refreshed_active_reservations_block_old_advice_without_audit");
        for matching in ["owner", "issue", "comment_path"] {
            let fixture = fixture();
            let reservation = json!({
                "holder": if matching == "owner" { PREVIOUS_OWNER } else { "OtherAgent" },
                "path_pattern": if matching == "comment_path" { "tests/e2e_coordination.rs" } else { "unrelated.rs" },
                "exclusive": true,
                "reason": if matching == "issue" { ISSUE_ID } else { "other work" },
                "expires_ts": "2099-01-01T00:00:00Z",
                "released_ts": null,
                "thread_id": "unrelated-thread",
            });
            fs::write(
                &fixture.reservations,
                json!({"reservations": [reservation]}).to_string(),
            )
            .expect("refreshed reservation snapshot");
            let before = state(&fixture.workspace, ISSUE_ID);
            let output = run_suggestion(&fixture, "NewAgent", matching);
            assert_eq!(output.status.code(), Some(4), "{output:?}");
            let error: Value = serde_json::from_slice(&output.stdout).expect("reservation refusal");
            assert!(
                error["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("reservation"),
                "{error}"
            );
            assert_eq!(
                state(&fixture.workspace, ISSUE_ID),
                before,
                "{matching} reservation refusal changed state"
            );
        }
    }

    #[test]
    fn snapshot_symlink_refresh_is_rechecked_when_executing_advice() {
        let _log = common::test_log("snapshot_symlink_refresh_is_rechecked_when_executing_advice");
        let mut fixture = fixture();
        let snapshot_link = fixture.workspace.root.join("current-reservations.json");
        std::os::unix::fs::symlink(&fixture.reservations, &snapshot_link)
            .expect("current snapshot symlink");
        snapshot_link
            .to_str()
            .expect("symlink path")
            .clone_into(&mut fixture.reservations);
        let advice = coordination_json(
            &fixture.workspace,
            &[
                "coordination",
                "status",
                "--reservations",
                &fixture.reservations,
                "--agents",
                &fixture.agents,
                "--json",
            ],
            "symlink_advice",
        );
        let claim = claim_by_id(&advice, ISSUE_ID);
        assert_eq!(claim["reclaim_allowed_by_policy"], true);
        claim["suggested_commands"][0]["command"]
            .as_str()
            .expect("symlink suggestion")
            .clone_into(&mut fixture.command);
        let active = fixture.workspace.root.join("new-active-reservations.json");
        fs::write(
            &active,
            json!({"reservations": [{
                "holder": PREVIOUS_OWNER,
                "path_pattern": "src/**",
                "exclusive": true,
                "reason": ISSUE_ID,
                "expires_ts": "2099-01-01T00:00:00Z",
            }]})
            .to_string(),
        )
        .expect("new snapshot target");
        fs::rename(
            &snapshot_link,
            fixture.workspace.root.join("retained-old-snapshot-link"),
        )
        .expect("retain old symlink");
        std::os::unix::fs::symlink(active, &snapshot_link).expect("refresh current snapshot link");
        let before = state(&fixture.workspace, ISSUE_ID);
        let output = run_suggestion(&fixture, "NewAgent", "refreshed_symlink_handoff");
        assert_eq!(output.status.code(), Some(4), "{output:?}");
        let error: Value =
            serde_json::from_slice(&output.stdout).expect("active reservation refusal");
        assert!(
            error["error"]["message"]
                .as_str()
                .unwrap()
                .contains("reservation"),
            "{error}"
        );
        assert_eq!(state(&fixture.workspace, ISSUE_ID), before);
    }

    #[test]
    fn reclaim_evidence_cannot_be_rerouted_to_a_same_id_external_claim() {
        let _log =
            common::test_log("reclaim_evidence_cannot_be_rerouted_to_a_same_id_external_claim");
        let fixture = fixture();
        let external = BrWorkspace::new();
        let init = run_br(&external, ["init"], "external_init");
        assert!(init.status.success(), "{init:?}");
        let external_issue = json!({
            "id": ISSUE_ID,
            "title": "External same-ID claim",
            "status": "in_progress",
            "priority": 2,
            "issue_type": "task",
            "assignee": PREVIOUS_OWNER,
            "created_at": ORIGINAL_TIMESTAMP,
            "updated_at": ORIGINAL_TIMESTAMP,
        });
        fs::write(
            external.root.join(".beads/issues.jsonl"),
            format!("{external_issue}\n"),
        )
        .expect("external claim with identical owner and version");
        let imported = run_br(
            &external,
            ["sync", "--import-only", "--json"],
            "external_import",
        );
        assert!(imported.status.success(), "{imported:?}");
        let external_before = state(&external, ISSUE_ID);
        fs::write(
            fixture.workspace.root.join(".beads/routes.jsonl"),
            json!({"prefix": "bd-", "path": external.root}).to_string(),
        )
        .expect("conflicting external prefix route");
        let routed = show(
            &fixture.workspace,
            ISSUE_ID,
            "ordinary_show_follows_external_route",
        );
        assert_eq!(
            routed["title"], "External same-ID claim",
            "route control must reach the external clone"
        );
        assert_success(&run_suggestion(
            &fixture,
            "NewAgent",
            "local_handoff_with_external_route",
        ));
        let local_after = state(&fixture.workspace, ISSUE_ID);
        let local_after: Value =
            serde_json::from_str(local_after["jsonl_row"].as_str().unwrap()).unwrap();
        assert_eq!(local_after["assignee"], "NewAgent");
        assert_eq!(local_after["status"], "in_progress");
        assert_eq!(
            state(&external, ISSUE_ID),
            external_before,
            "local coordination evidence must never mutate an external clone"
        );
    }

    #[test]
    fn unusable_snapshots_block_old_advice_without_mutation() {
        let _log = common::test_log("unusable_snapshots_block_old_advice_without_mutation");
        for (label, payload) in [
            ("malformed", "{ not JSON"),
            ("wrong_shape", r#"{"reservations":{}}"#),
            (
                "invalid_timestamp",
                r#"{"reservations":[{"holder":"AmberLion","path_pattern":"src/**","exclusive":true,"expires_ts":"not-a-time"}]}"#,
            ),
        ] {
            let fixture = fixture();
            fs::write(&fixture.reservations, payload).expect("invalid snapshot fixture");
            let before = state(&fixture.workspace, ISSUE_ID);
            let output = run_suggestion(&fixture, "NewAgent", label);
            assert_eq!(output.status.code(), Some(4), "{output:?}");
            let error: Value =
                serde_json::from_slice(&output.stdout).expect("invalid snapshot error");
            assert_eq!(error["error"]["context"]["field"], "coordination_snapshot");
            assert_eq!(state(&fixture.workspace, ISSUE_ID), before);
        }
        let fixture = fixture();
        fs::rename(
            &fixture.reservations,
            Path::new(&fixture.reservations).with_extension("retained.json"),
        )
        .expect("make original snapshot path unavailable without discarding it");
        let before = state(&fixture.workspace, ISSUE_ID);
        let output = run_suggestion(&fixture, "NewAgent", "missing_snapshot");
        assert_eq!(output.status.code(), Some(4), "{output:?}");
        assert_eq!(state(&fixture.workspace, ISSUE_ID), before);
    }

    #[test]
    fn human_unknown_and_fresh_claims_cannot_be_reclaimed_directly() {
        let _log = common::test_log("human_unknown_and_fresh_claims_cannot_be_reclaimed_directly");
        let fixture = fixture();
        let before = state(&fixture.workspace, ISSUE_ID);
        for owner_kind in ["human", "unknown"] {
            let mut args = direct_args(&fixture);
            args.extend(["--owner-kind", owner_kind, "--actor", "NewAgent"].map(str::to_owned));
            if owner_kind == "human" {
                // Explicit human policy must not be overridden merely because
                // an agent snapshot contains the same display name.
                args.extend(["--agents".to_owned(), fixture.agents.clone()]);
            }
            let result = run_br(&fixture.workspace, &args, &format!("direct_{owner_kind}"));
            assert_eq!(result.status.code(), Some(4), "{result:?}");
            assert_eq!(state(&fixture.workspace, ISSUE_ID), before);
            let mut status_args = vec![
                "coordination",
                "status",
                "--reservations",
                &fixture.reservations,
                "--owner-kind",
                owner_kind,
                "--json",
            ];
            if owner_kind == "human" {
                status_args.extend(["--agents", &fixture.agents]);
            }
            let advice = coordination_json(
                &fixture.workspace,
                &status_args,
                &format!("advice_{owner_kind}"),
            );
            let claim = claim_by_id(&advice, ISSUE_ID);
            assert_eq!(claim["reclaim_allowed_by_policy"], false);
            assert_eq!(claim["suggested_commands"], json!([]));
        }
        let mut args = direct_args(&fixture);
        args.extend(["--actor", "NewAgent"].map(str::to_owned));
        let result = run_br(&fixture.workspace, &args, "direct_owner_kind_omitted");
        assert_eq!(result.status.code(), Some(4), "{result:?}");
        assert_eq!(state(&fixture.workspace, ISSUE_ID), before);

        let fresh_before = state(&fixture.workspace, "bd-fresh");
        let result = run_br(
            &fixture.workspace,
            [
                "coordination",
                "reclaim",
                "bd-fresh",
                "--expected-assignee",
                "TopazFox",
                "--if-unchanged",
                "2099-01-01T00:00:00Z",
                "--reservations",
                &fixture.reservations,
                "--owner-kind",
                "swarm-agent",
                "--actor",
                "NewAgent",
                "--json",
            ],
            "direct_fresh_claim",
        );
        assert_eq!(result.status.code(), Some(4), "{result:?}");
        assert_eq!(state(&fixture.workspace, "bd-fresh"), fresh_before);
    }

    #[test]
    fn reclaim_requires_explicit_nonempty_new_actor() {
        let _log = common::test_log("reclaim_requires_explicit_nonempty_new_actor");
        let fixture = fixture();
        let before = state(&fixture.workspace, ISSUE_ID);
        for actor in [
            None,
            Some(""),
            Some("   "),
            Some(PREVIOUS_OWNER),
            Some(" amberlion "),
        ] {
            let mut args = direct_args(&fixture);
            args.extend(["--owner-kind", "swarm-agent"].map(str::to_owned));
            if let Some(actor) = actor {
                args.extend(["--actor", actor].map(str::to_owned));
            }
            let result = run_br_with_env(
                &fixture.workspace,
                &args,
                [("BEADS_ACTOR", "WrongDefaultActor")],
                "invalid_actor",
            );
            assert!(
                !result.status.success(),
                "invalid actor {actor:?}: {result:?}"
            );
            assert_eq!(state(&fixture.workspace, ISSUE_ID), before);
        }
        let output = run_suggestion(&fixture, "", "empty_agent_name");
        assert!(
            !output.status.success(),
            "empty AGENT_NAME must fail in the shell"
        );
        assert_eq!(state(&fixture.workspace, ISSUE_ID), before);
    }

    fn wait_for_two_queued_commands(workspace: &BrWorkspace) {
        let queue = workspace.root.join(".beads/.write-waiters.lock");
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let count = match fs::read_dir(&queue) {
                Ok(entries) => entries
                    .map(|entry| entry.expect("waiter entry").path())
                    .filter(|path| {
                        path.extension()
                            .is_some_and(|extension| extension == "waiter")
                    })
                    .count(),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
                Err(error) => panic!("read waiter queue: {error}"),
            };
            if count == 2 {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "expected two queued reclaim commands, found {count}"
            );
            thread::sleep(Duration::from_millis(25));
        }
    }

    #[test]
    fn simultaneous_suggested_reclaims_have_one_winner_and_one_audit() {
        let _log =
            common::test_log("simultaneous_suggested_reclaims_have_one_winner_and_one_audit");
        let fixture = fixture();
        let authority =
            beads_rust::sync::blocking_write_lock(&fixture.workspace.root.join(".beads"))
                .expect("hold workspace authority");
        let children = ["FirstAgent", "SecondAgent"].map(|actor| {
            (
                actor,
                shell_command(&fixture, actor)
                    .spawn()
                    .expect("launch reclaim contender"),
            )
        });
        wait_for_two_queued_commands(&fixture.workspace);
        drop(authority);
        let outputs = thread::scope(|scope| {
            let collectors = children.map(|(actor, child)| {
                scope.spawn(move || (actor, child.wait_with_output().expect("contender output")))
            });
            collectors.map(|collector| collector.join().expect("contender collector"))
        });
        for (actor, output) in &outputs {
            record_output(&fixture, actor, actor, output);
        }
        assert_eq!(
            outputs
                .iter()
                .filter(|(_, output)| output.status.success())
                .count(),
            1,
            "{outputs:?}"
        );
        let winner = outputs
            .iter()
            .find(|(_, output)| output.status.success())
            .expect("one winner")
            .0;
        for (_, output) in &outputs {
            if !output.status.success() {
                assert_conflict(output, winner);
            }
        }
        let issue = show(&fixture.workspace, ISSUE_ID, "winner");
        assert_eq!(issue["assignee"], winner);
        assert_eq!(issue["status"], "in_progress");
        let comments = run_br(
            &fixture.workspace,
            ["comments", "list", ISSUE_ID, "--json"],
            "winner_audit",
        );
        assert!(comments.status.success(), "{comments:?}");
        let comments: Vec<Value> = serde_json::from_str(&comments.stdout).expect("comments");
        assert_eq!(comments.len(), 2, "loser must not append an audit");
        assert_eq!(comments[1]["author"], winner);
        let before = state(&fixture.workspace, ISSUE_ID);
        let replay = run_suggestion(&fixture, winner, "winner_replay");
        assert_conflict(&replay, winner);
        assert_eq!(state(&fixture.workspace, ISSUE_ID), before);
    }

    #[test]
    fn ordinary_claim_cannot_take_work_during_the_guarded_handoff() {
        let _log = common::test_log("ordinary_claim_cannot_take_work_during_the_guarded_handoff");
        let fixture = fixture();
        let authority =
            beads_rust::sync::blocking_write_lock(&fixture.workspace.root.join(".beads"))
                .expect("hold workspace authority");
        let ordinary = Command::new(assert_cmd::cargo::cargo_bin!("br"))
            .args([
                "update",
                ISSUE_ID,
                "--claim",
                "--actor",
                "Opportunist",
                "--json",
            ])
            .current_dir(&fixture.workspace.root)
            .env_clear()
            .env("HOME", &fixture.workspace.root)
            .env("PATH", common::cli::deduplicated_br_path())
            .env("BEADS_DIR", fixture.workspace.root.join(".beads"))
            .env("RUST_LOG", "error")
            .env("NO_COLOR", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("launch ordinary claim contender");
        let reclaim = shell_command(&fixture, "NewAgent")
            .spawn()
            .expect("launch reclaim contender");
        wait_for_two_queued_commands(&fixture.workspace);
        drop(authority);
        let (ordinary, reclaim) = thread::scope(|scope| {
            let ordinary =
                scope.spawn(move || ordinary.wait_with_output().expect("ordinary claim output"));
            let reclaim = scope.spawn(move || reclaim.wait_with_output().expect("reclaim output"));
            (ordinary.join().unwrap(), reclaim.join().unwrap())
        });
        record_output(&fixture, "NewAgent", "reclaim_racing_ordinary", &reclaim);
        fs::write(
            fixture.workspace.log_dir.join("ordinary_claim_race.log"),
            format!("{ordinary:?}"),
        )
        .expect("retain ordinary contender outcome");
        assert_success(&reclaim);
        assert_eq!(ordinary.status.code(), Some(4), "{ordinary:?}");
        let error: Value =
            serde_json::from_slice(&ordinary.stdout).expect("ordinary claim conflict");
        assert_eq!(error["error"]["code"], "CLAIM_CONFLICT");
        assert_eq!(error["error"]["retryable"], false);
        let owner = error["error"]["context"]["actual_assignee"]
            .as_str()
            .expect("current owner diagnostic");
        assert!([PREVIOUS_OWNER, "NewAgent"].contains(&owner), "{error}");
        let issue = show(&fixture.workspace, ISSUE_ID, "ordinary_race_winner");
        assert_eq!(issue["assignee"], "NewAgent");
        assert_eq!(issue["status"], "in_progress");
        assert_handoff_audit(&fixture, "NewAgent");
    }
}
