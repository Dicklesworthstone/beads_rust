//! End-to-end coverage for audited issue-specific capacity exemptions
//! (GitHub #384 phase 4, bead beads_rust-8nbk.4).
//!
//! Drives the real `br` binary through the full lifecycle: a hard status
//! capacity rejects fresh work, an authorized `br capacity exempt` grant
//! admits the named issue without consuming a slot, evidence separates
//! counted and exempt totals, revocation restores enforcement, and leaving
//! the applicable status ends the exemption.

mod common;

use common::cli::{BrWorkspace, extract_json_payload, parse_created_id, run_br};
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

fn write_exemption_policy(workspace: &BrWorkspace) {
    fs::write(
        workspace.root.join(".beads").join("policy.yaml"),
        r"
workflow:
  statuses: [open, in_progress, closed]
  capacity:
    statuses:
      in_progress:
        hard: 1
    exemptions:
      providers: [operator]
",
    )
    .expect("write exemption policy");
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

#[test]
#[allow(clippy::too_many_lines)]
fn e2e_capacity_exemption_lifecycle_admits_reports_and_revokes() {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "exempt_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let active_id = create_issue(&workspace, "Already active", "exempt_create_active");
    let activate = run_br(
        &workspace,
        ["update", &active_id, "--status", "in_progress"],
        "exempt_activate",
    );
    assert!(
        activate.status.success(),
        "activation failed: {}",
        activate.stderr
    );
    let hotfix_id = create_issue(
        &workspace,
        "Externally mandated hotfix",
        "exempt_create_hotfix",
    );
    let normal_id = create_issue(&workspace, "Ordinary work", "exempt_create_normal");

    write_exemption_policy(&workspace);

    // Without an exemption the capacity is full.
    let rejected = run_br(
        &workspace,
        ["update", &hotfix_id, "--status", "in_progress", "--json"],
        "exempt_reject_before_grant",
    );
    assert!(!rejected.status.success(), "full capacity must reject");
    assert_eq!(rejected.status.code(), Some(4));

    // An unauthorized provider cannot grant (GitHub #384: "Unauthorized,
    // expired, or reasonless exemptions fail").
    let unauthorized = run_br(
        &workspace,
        [
            "capacity",
            "exempt",
            &hotfix_id,
            "--status",
            "in_progress",
            "--provider",
            "intruder",
            "--reason",
            "should fail",
        ],
        "exempt_unauthorized",
    );
    assert!(
        !unauthorized.status.success(),
        "unauthorized provider must be rejected"
    );
    assert!(
        unauthorized.stderr.contains("not authorized")
            || unauthorized.stdout.contains("not authorized"),
        "missing authorization diagnostic: stdout={} stderr={}",
        unauthorized.stdout,
        unauthorized.stderr
    );

    // An authorized grant with reason and expiry succeeds.
    let granted = run_br(
        &workspace,
        [
            "capacity",
            "exempt",
            &hotfix_id,
            "--status",
            "in_progress",
            "--provider",
            "operator",
            "--reason",
            "Awaiting an external regulatory decision",
            "--expires",
            "+7d",
            "--robot",
        ],
        "exempt_grant",
    );
    assert!(granted.status.success(), "grant failed: {}", granted.stderr);
    let record: Value =
        serde_json::from_str(&extract_json_payload(&granted.stdout)).expect("grant json");
    assert_eq!(record["issue_id"], hotfix_id);
    assert_eq!(record["capacity_kind"], "status");
    assert_eq!(record["capacity_name"], "in_progress");
    assert_eq!(record["state"], "active");
    assert!(record["expires_at"].is_string());

    // The exempted issue now enters the full capacity without a slot.
    let admitted = run_br(
        &workspace,
        ["update", &hotfix_id, "--status", "in_progress"],
        "exempt_admitted",
    );
    assert!(
        admitted.status.success(),
        "exempt admission failed: {}",
        admitted.stderr
    );

    // Ordinary work is still rejected, with separately observable counted
    // and exempt totals in the structured evidence.
    let still_full = run_br(
        &workspace,
        ["update", &normal_id, "--status", "in_progress", "--json"],
        "exempt_normal_rejected",
    );
    assert!(!still_full.status.success(), "normal work must be rejected");
    let json = parse_error_json(&still_full.stdout).expect("structured capacity error");
    let context = &json["error"]["context"];
    assert_eq!(context["current"], 1, "exempt issue must not be counted");
    assert_eq!(context["prospective"], 2);
    assert_eq!(context["exempt"], 1);
    assert_eq!(json["error"]["code"], "WORKFLOW_CAPACITY_EXCEEDED");

    // Exempt issues remain visible in queue metrics.
    let listed = run_br(
        &workspace,
        ["list", "--status", "in_progress", "--json"],
        "exempt_still_visible",
    );
    assert!(listed.status.success(), "list failed: {}", listed.stderr);
    let payload: Value =
        serde_json::from_str(&extract_json_payload(&listed.stdout)).expect("list json");
    let ids: Vec<&str> = payload["issues"]
        .as_array()
        .expect("issues array")
        .iter()
        .filter_map(|issue| issue["id"].as_str())
        .collect();
    assert!(ids.contains(&hotfix_id.as_str()));
    assert!(ids.contains(&active_id.as_str()));

    // Revocation is audited and restores enforcement for future entries.
    let revoked = run_br(
        &workspace,
        [
            "capacity",
            "revoke",
            &hotfix_id,
            "--status",
            "in_progress",
            "--provider",
            "operator",
            "--reason",
            "external decision arrived",
            "--robot",
        ],
        "exempt_revoke",
    );
    assert!(
        revoked.status.success(),
        "revoke failed: {}",
        revoked.stderr
    );
    let record: Value =
        serde_json::from_str(&extract_json_payload(&revoked.stdout)).expect("revoke json");
    assert_eq!(record["state"], "revoked");

    let history = run_br(
        &workspace,
        ["capacity", "exemptions", &hotfix_id, "--history", "--robot"],
        "exempt_history",
    );
    assert!(
        history.status.success(),
        "exemptions listing failed: {}",
        history.stderr
    );
    let listing: Value =
        serde_json::from_str(&extract_json_payload(&history.stdout)).expect("exemptions json");
    let actions: Vec<&str> = listing["history"]
        .as_array()
        .expect("history array")
        .iter()
        .filter_map(|entry| entry["action"].as_str())
        .collect();
    assert_eq!(actions, vec!["grant", "revoke"], "append-only audit trail");
}

#[test]
fn e2e_capacity_exemption_ends_when_issue_leaves_the_applicable_status() {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "exempt_leave_init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let active_id = create_issue(&workspace, "Holds the slot", "exempt_leave_active");
    let activate = run_br(
        &workspace,
        ["update", &active_id, "--status", "in_progress"],
        "exempt_leave_activate",
    );
    assert!(
        activate.status.success(),
        "activation failed: {}",
        activate.stderr
    );
    let hotfix_id = create_issue(&workspace, "One admission only", "exempt_leave_hotfix");

    write_exemption_policy(&workspace);

    let granted = run_br(
        &workspace,
        [
            "capacity",
            "exempt",
            &hotfix_id,
            "--status",
            "in_progress",
            "--provider",
            "operator",
            "--reason",
            "single exempted admission",
        ],
        "exempt_leave_grant",
    );
    assert!(granted.status.success(), "grant failed: {}", granted.stderr);

    let admitted = run_br(
        &workspace,
        ["update", &hotfix_id, "--status", "in_progress"],
        "exempt_leave_admitted",
    );
    assert!(
        admitted.status.success(),
        "exempt admission failed: {}",
        admitted.stderr
    );

    // Leaving the applicable status ends the exemption, audited.
    let closed = run_br(
        &workspace,
        ["close", &hotfix_id, "--reason", "shipped"],
        "exempt_leave_close",
    );
    assert!(closed.status.success(), "close failed: {}", closed.stderr);

    let listing = run_br(
        &workspace,
        ["capacity", "exemptions", &hotfix_id, "--robot"],
        "exempt_leave_state",
    );
    assert!(
        listing.status.success(),
        "exemptions listing failed: {}",
        listing.stderr
    );
    let payload: Value =
        serde_json::from_str(&extract_json_payload(&listing.stdout)).expect("exemptions json");
    assert_eq!(payload["exemptions"][0]["state"], "left_status");

    // Re-entry counts again: reopening into the still-full capacity fails.
    let reopened = run_br(
        &workspace,
        ["update", &hotfix_id, "--status", "in_progress", "--json"],
        "exempt_leave_reentry",
    );
    assert!(
        !reopened.status.success(),
        "re-entry after leaving must count again"
    );
    assert_eq!(reopened.status.code(), Some(4));
}
