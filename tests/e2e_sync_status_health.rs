//! E2E coverage for `br sync --status --json`:
//!
//! - beads_rust-0v1.2.4: stable `git_export` compatibility slot that never
//!   probes VCS and points to the explicit `br vcs-status` command.
//! - beads_rust#334: `workspace_health` + `reliability_audit` fields in
//!   the same write-gate vocabulary as `br doctor --json`.

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;

fn sync_status_json(workspace: &BrWorkspace, label: &str) -> Value {
    let status = run_br(workspace, ["sync", "--status", "--json"], label);
    assert!(
        status.status.success(),
        "sync --status failed: {}",
        status.stderr
    );
    serde_json::from_str(&extract_json_payload(&status.stdout)).expect("sync status json")
}

/// Like `sync_status_json` but suppresses the open-time auto-import so a
/// deliberately-dirtied JSONL stays `jsonl_newer` for the read-only
/// status snapshot (the harness clears BR env, so we pass the flag).
fn sync_status_json_no_auto_import(workspace: &BrWorkspace, label: &str) -> Value {
    let status = run_br(
        workspace,
        ["sync", "--status", "--json", "--no-auto-import"],
        label,
    );
    assert!(
        status.status.success(),
        "sync --status --no-auto-import failed: {}",
        status.stderr
    );
    serde_json::from_str(&extract_json_payload(&status.stdout)).expect("sync status json")
}

fn assert_vcs_not_probed(status: &Value) {
    let git_export = status["git_export"]
        .as_object()
        .expect("git_export compatibility object");
    assert_eq!(
        git_export
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        ["available", "diagnostic_command", "reason"]
            .into_iter()
            .map(str::to_string)
            .collect(),
        "sync must not leak or fabricate VCS observations: {status}"
    );
    assert_eq!(git_export["available"], false, "{status}");
    assert_eq!(git_export["reason"], "not_probed", "{status}");
    assert_eq!(
        git_export["diagnostic_command"], "br vcs-status --json",
        "{status}"
    );
}

#[test]
fn e2e_sync_status_vcs_slot_is_not_probed_inside_git_repo() {
    let _log = common::test_log("e2e_sync_status_vcs_slot_is_not_probed_inside_git_repo");
    let workspace = BrWorkspace::new();
    let git = std::process::Command::new("git")
        .args(["init", "--initial-branch=main"])
        .current_dir(&workspace.root)
        .output()
        .expect("git init");
    assert!(git.status.success(), "git init failed");

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    assert_vcs_not_probed(&sync_status_json(&workspace, "status_in_git"));
}

#[test]
fn e2e_sync_status_vcs_slot_is_not_probed_outside_git_repo() {
    let _log = common::test_log("e2e_sync_status_vcs_slot_is_not_probed_outside_git_repo");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);
    assert_vcs_not_probed(&sync_status_json(&workspace, "status_no_git"));
}

#[test]
fn e2e_sync_status_reports_workspace_health_and_reliability_audit() {
    let _log = common::test_log("e2e_sync_status_reports_workspace_health_and_reliability_audit");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Health issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    // Establish a clean, fully-synced baseline. `br create` already
    // auto-flushes, but flush again explicitly so the DB and JSONL are
    // unambiguously in sync before we drive a deterministic anomaly.
    let flush = run_br(&workspace, ["sync", "--flush-only"], "flush");
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);

    let healthy = sync_status_json(&workspace, "status_healthy");
    assert_eq!(
        healthy["workspace_health"], "healthy",
        "clean synced workspace must be healthy: {healthy}"
    );
    assert_eq!(
        healthy["reliability_audit"]["source"], "sync.status",
        "{healthy}"
    );
    assert_eq!(
        healthy["reliability_audit"]["anomaly_count"], 0,
        "{healthy}"
    );
    assert_eq!(
        healthy["reliability_audit"]["health"], "healthy",
        "{healthy}"
    );

    // Drive a deterministic drift: append an external record to the JSONL
    // so it is now newer than the DB (pending import). This is the same
    // jsonl_newer → degraded mapping doctor uses; only codes we actually
    // evaluate may appear.
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&jsonl_path)
            .expect("open jsonl for append");
        writeln!(
            f,
            "{{\"id\":\"bd-external-import\",\"title\":\"External\"}}"
        )
        .expect("append to jsonl");
    }

    // --no-auto-import keeps the external edit visible as jsonl_newer
    // instead of being silently imported by the status open.
    let pending = sync_status_json_no_auto_import(&workspace, "status_pending_import");
    assert_eq!(
        pending["jsonl_newer"], true,
        "external JSONL edit must read as jsonl_newer: {pending}"
    );
    assert_eq!(pending["workspace_health"], "degraded", "{pending}");
    let audit = &pending["reliability_audit"];
    assert_eq!(audit["source"], "sync.status", "{pending}");
    assert_eq!(audit["health"], "degraded", "{pending}");
    let codes: Vec<&str> = audit["anomalies"]
        .as_array()
        .expect("anomalies array")
        .iter()
        .filter_map(|a| a["code"].as_str())
        .collect();
    assert!(
        codes.contains(&"jsonl_newer"),
        "expected jsonl_newer anomaly code, got {codes:?}: {pending}"
    );
}

/// Issue #378: `br sync --flush-only` maintains the merge anchor
/// (`beads.base.jsonl`) so `br doctor` and `br sync --status` agree.
///
/// Historically only the merge path wrote the anchor: flush-only workspaces
/// (the common agent workflow) accumulated `metadata.last_export_time`
/// without ever growing an anchor, so `br doctor` warned
/// `base_jsonl.missing_post_flush` forever while `br sync --status` reported
/// a fully healthy "In sync". The flush path now (a) refreshes the anchor
/// from the finalized export and (b) materializes a missing anchor even on a
/// no-op flush, making `br sync --flush-only` the idempotent recovery
/// command the doctor warning names.
#[test]
fn e2e_flush_only_maintains_merge_anchor_and_doctor_agrees() {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Anchor issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let beads_dir = workspace.root.join(".beads");
    let jsonl_path = beads_dir.join("issues.jsonl");
    let anchor_path = beads_dir.join("beads.base.jsonl");

    // No-op flush path: create's auto-flush already exported, so this flush
    // has nothing to export — it must still materialize the missing anchor.
    let flush_noop = run_br(&workspace, ["sync", "--flush-only"], "flush_noop");
    assert!(
        flush_noop.status.success(),
        "no-op flush failed: {}",
        flush_noop.stderr
    );
    assert!(
        anchor_path.is_file(),
        "no-op flush must materialize the missing merge anchor"
    );
    assert_eq!(
        std::fs::read(&anchor_path).expect("read anchor"),
        std::fs::read(&jsonl_path).expect("read jsonl"),
        "anchor must match the live JSONL byte-for-byte after a no-op flush"
    );

    // Real export path: a dirty issue forces an actual export, which must
    // refresh the anchor to the newly finalized JSONL.
    let create2 = run_br(&workspace, ["create", "Second issue"], "create2");
    assert!(
        create2.status.success(),
        "create2 failed: {}",
        create2.stderr
    );
    let flush_real = run_br(
        &workspace,
        ["sync", "--flush-only", "--force"],
        "flush_real",
    );
    assert!(
        flush_real.status.success(),
        "forced flush failed: {}",
        flush_real.stderr
    );
    assert_eq!(
        std::fs::read(&anchor_path).expect("read anchor"),
        std::fs::read(&jsonl_path).expect("read jsonl"),
        "anchor must track the finalized JSONL after a real export"
    );

    // Doctor must agree with sync --status: no missing-anchor warning.
    let status = sync_status_json(&workspace, "status_after_flush");
    assert_eq!(status["dirty_count"], 0, "{status}");
    let doctor = run_br(&workspace, ["doctor", "--json"], "doctor_after_flush");
    let doctor_json: Value =
        serde_json::from_str(&extract_json_payload(&doctor.stdout)).expect("doctor json");
    let anchor_check = doctor_json["checks"]
        .as_array()
        .expect("checks array")
        .iter()
        .find(|c| c["name"] == "base_jsonl.missing_post_flush")
        .expect("base_jsonl.missing_post_flush check present")
        .clone();
    assert_eq!(
        anchor_check["status"], "ok",
        "doctor must not warn about a missing anchor after a flush: {anchor_check}"
    );
}
