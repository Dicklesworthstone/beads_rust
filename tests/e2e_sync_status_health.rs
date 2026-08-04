//! E2E coverage for `br sync --status --json`:
//!
//! - beads_rust-0v1.2.4: stable `git_export` compatibility slot that never
//!   probes VCS and points to the explicit `br vcs-status` command.
//! - beads_rust#334: `workspace_health` + `reliability_audit` fields in
//!   the same write-gate vocabulary as `br doctor --json`.

mod common;

use beads_rust::storage::SqliteStorage;
use beads_rust::sync::{
    METADATA_JSONL_CONTENT_HASH, METADATA_JSONL_MTIME, METADATA_JSONL_SIZE,
    METADATA_LAST_EXPORT_TIME, METADATA_LAST_IMPORT_TIME, compute_jsonl_hash,
};
use common::cli::{BrRun, BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;
use std::collections::BTreeMap;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;

fn git(root: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args([
            "-c",
            "user.name=br-e2e",
            "-c",
            "user.email=br-e2e@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(root)
        .env("HOME", root)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .expect("run git")
}

fn git_ok(root: &Path, args: &[&str]) {
    let out = git(root, args);
    assert!(
        out.status.success(),
        "git {args:?} failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

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

const CERTIFICATION_METADATA_KEYS: [&str; 6] = [
    METADATA_JSONL_CONTENT_HASH,
    METADATA_JSONL_MTIME,
    METADATA_JSONL_SIZE,
    METADATA_LAST_EXPORT_TIME,
    METADATA_LAST_IMPORT_TIME,
    "needs_flush",
];

#[derive(Debug, PartialEq, Eq)]
struct SyncPersistenceSnapshot {
    jsonl: Vec<u8>,
    anchor: Vec<u8>,
    metadata: BTreeMap<String, Option<String>>,
}

fn setup_certified_anchor_workspace(label: &str) -> (BrWorkspace, String) {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], &format!("{label}_init"));
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(
        &workspace,
        ["create", "No-op anchor certification", "--json"],
        &format!("{label}_create"),
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let created: Value =
        serde_json::from_str(&extract_json_payload(&create.stdout)).expect("create JSON payload");
    let issue_id = created["id"]
        .as_str()
        .expect("created issue id")
        .to_string();

    let flush = run_br(
        &workspace,
        ["sync", "--flush-only", "--no-auto-import"],
        &format!("{label}_baseline_flush"),
    );
    assert!(
        flush.status.success(),
        "baseline no-op flush failed: {}",
        flush.stderr
    );

    let beads_dir = workspace.root.join(".beads");
    assert_eq!(
        std::fs::read(beads_dir.join("beads.base.jsonl")).expect("baseline anchor"),
        std::fs::read(beads_dir.join("issues.jsonl")).expect("baseline JSONL"),
        "baseline anchor must be certified and byte-exact"
    );

    (workspace, issue_id)
}

fn persistence_snapshot(workspace: &BrWorkspace) -> SyncPersistenceSnapshot {
    let beads_dir = workspace.root.join(".beads");
    let storage = SqliteStorage::open(&beads_dir.join("beads.db")).expect("open workspace db");
    let metadata = CERTIFICATION_METADATA_KEYS
        .into_iter()
        .map(|key| {
            (
                key.to_string(),
                storage.get_metadata(key).expect("read sync metadata"),
            )
        })
        .collect();

    SyncPersistenceSnapshot {
        jsonl: std::fs::read(beads_dir.join("issues.jsonl")).expect("read JSONL snapshot"),
        anchor: std::fs::read(beads_dir.join("beads.base.jsonl")).expect("read anchor snapshot"),
        metadata,
    }
}

fn assert_noop_anchor_certification_failure(run: &BrRun, context: &str) {
    assert!(
        !run.status.success(),
        "{context} unexpectedly succeeded\nstdout={}\nstderr={}",
        run.stdout,
        run.stderr
    );
    let output = format!("{}\n{}", run.stdout, run.stderr);
    for guidance in [
        "merge anchor was not changed",
        "br sync --merge",
        "br sync --import-only --force",
        "br sync --flush-only --force",
    ] {
        assert!(
            output.contains(guidance),
            "{context} should include {guidance:?} guidance: {output}"
        );
    }
}

#[test]
fn e2e_sync_status_git_export_committed_vs_dirty_jsonl() {
    let _log = common::test_log("e2e_sync_status_git_export_committed_vs_dirty_jsonl");
    let workspace = BrWorkspace::new();

    git_ok(&workspace.root, &["init", "--initial-branch=main"]);

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Git status issue"], "create");
    assert!(create.status.success(), "create failed: {}", create.stderr);

    let flush = run_br(&workspace, ["sync", "--flush-only"], "flush");
    assert!(flush.status.success(), "flush failed: {}", flush.stderr);

    // Untracked JSONL: available, but not tracked and not worktree-clean.
    let untracked = sync_status_json(&workspace, "status_untracked");
    let git_export = &untracked["git_export"];
    assert_eq!(git_export["available"], true, "{untracked}");
    assert_eq!(git_export["tracked"], false, "{untracked}");
    assert_eq!(git_export["worktree_clean"], false, "{untracked}");
    assert_eq!(git_export["index_clean"], true, "{untracked}");
    assert!(git_export["head_hash"].is_null(), "{untracked}");
    assert!(git_export["worktree_hash"].is_string(), "{untracked}");

    // Commit the JSONL exactly as it sits on disk. We avoid asserting
    // byte-for-byte hash equality with a later status call because a
    // `br sync --status` open may auto-export the JSONL with refreshed
    // timestamps; instead we assert the structural git facts (tracked,
    // and the reported HEAD blob hash agrees with git's own view).
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    git_ok(&workspace.root, &["add", ".beads/issues.jsonl"]);
    git_ok(&workspace.root, &["commit", "-m", "track issues.jsonl"]);
    let committed_head =
        git_committed_blob_hash(&workspace.root, ".beads/issues.jsonl").expect("head blob hash");

    let committed = sync_status_json(&workspace, "status_committed");
    let git_export = &committed["git_export"];
    assert_eq!(git_export["available"], true, "{committed}");
    assert_eq!(git_export["tracked"], true, "{committed}");
    // The reported HEAD blob hash must agree with what git records for
    // the committed copy (independent of any worktree re-export jitter).
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

    // Exact-match no-op path: certifying an already-current regular anchor
    // must not replace it. Inode stability makes the no-rewrite guarantee
    // observable on Unix.
    #[cfg(unix)]
    let matching_anchor_inode = std::fs::metadata(&anchor_path)
        .expect("matching anchor metadata")
        .ino();
    let flush_idempotent = run_br(&workspace, ["sync", "--flush-only"], "flush_idempotent");
    assert!(
        flush_idempotent.status.success(),
        "idempotent no-op flush failed: {}",
        flush_idempotent.stderr
    );
    #[cfg(unix)]
    assert_eq!(
        std::fs::metadata(&anchor_path)
            .expect("idempotent anchor metadata")
            .ino(),
        matching_anchor_inode,
        "an exact anchor must keep its inode across an idempotent no-op flush"
    );

    // Stale-anchor no-op path: the same recovery command must replace stale
    // bytes even though there is still nothing to export from the database.
    std::fs::write(
        &anchor_path,
        b"{\"id\":\"stale-anchor\",\"title\":\"must be replaced\"}\n",
    )
    .expect("write stale anchor");
    let flush_stale = run_br(&workspace, ["sync", "--flush-only"], "flush_stale");
    assert!(
        flush_stale.status.success(),
        "stale-anchor no-op flush failed: {}",
        flush_stale.stderr
    );
    assert_eq!(
        std::fs::read(&anchor_path).expect("read repaired anchor"),
        std::fs::read(&jsonl_path).expect("read jsonl after stale repair"),
        "a no-op flush must replace a stale merge anchor with exact JSONL bytes"
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

#[test]
fn e2e_noop_anchor_rejects_same_id_external_semantic_edit_without_mutation() {
    let (workspace, issue_id) = setup_certified_anchor_workspace("same_id_semantic_edit");
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");

    let mut records: Vec<Value> = std::fs::read_to_string(&jsonl_path)
        .expect("read JSONL")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("parse JSONL record"))
        .collect();
    let edited = records
        .iter_mut()
        .find(|record| record["id"].as_str() == Some(issue_id.as_str()))
        .expect("created issue in JSONL");
    edited["title"] = Value::String("Externally edited same-ID title".to_string());
    let edited_jsonl = format!(
        "{}\n",
        records
            .iter()
            .map(|record| serde_json::to_string(record).expect("serialize edited record"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    std::fs::write(&jsonl_path, edited_jsonl).expect("write same-ID semantic edit");

    let before = persistence_snapshot(&workspace);
    let flush = run_br(
        &workspace,
        ["sync", "--flush-only", "--json", "--no-auto-import"],
        "same_id_semantic_edit_flush",
    );
    assert_noop_anchor_certification_failure(&flush, "same-ID semantic edit");
    let after = persistence_snapshot(&workspace);
    assert_eq!(
        after, before,
        "failed no-op certification must preserve edited JSONL, prior anchor, and sync metadata"
    );
}

#[test]
fn e2e_noop_anchor_rejects_external_truncation_without_mutation() {
    let (workspace, _issue_id) = setup_certified_anchor_workspace("external_truncation");
    let jsonl_path = workspace.root.join(".beads").join("issues.jsonl");
    std::fs::write(&jsonl_path, b"").expect("truncate JSONL");

    let before = persistence_snapshot(&workspace);
    let flush = run_br(
        &workspace,
        ["sync", "--flush-only", "--json", "--no-auto-import"],
        "external_truncation_flush",
    );
    assert_noop_anchor_certification_failure(&flush, "external truncation");
    let after = persistence_snapshot(&workspace);
    assert_eq!(
        after, before,
        "failed truncation certification must preserve empty JSONL, prior anchor, and sync metadata"
    );
}

#[test]
fn e2e_noop_anchor_missing_cached_hash_fails_then_force_recovers() {
    let (workspace, _issue_id) = setup_certified_anchor_workspace("missing_cached_hash");
    let beads_dir = workspace.root.join(".beads");
    let mut storage = SqliteStorage::open(&beads_dir.join("beads.db")).expect("open workspace db");
    assert!(
        storage
            .delete_metadata(METADATA_JSONL_CONTENT_HASH)
            .expect("delete cached JSONL hash"),
        "baseline should contain a cached JSONL hash"
    );
    drop(storage);

    let before = persistence_snapshot(&workspace);
    assert_eq!(
        before.metadata[METADATA_JSONL_CONTENT_HASH], None,
        "test precondition requires a missing cached hash"
    );
    let failed = run_br(
        &workspace,
        ["sync", "--flush-only", "--json", "--no-auto-import"],
        "missing_cached_hash_flush",
    );
    assert_noop_anchor_certification_failure(&failed, "missing cached hash");
    let after_failure = persistence_snapshot(&workspace);
    assert_eq!(
        after_failure, before,
        "missing-hash failure must preserve JSONL, anchor, and metadata"
    );

    let forced = run_br(
        &workspace,
        [
            "sync",
            "--flush-only",
            "--force",
            "--json",
            "--no-auto-import",
        ],
        "missing_cached_hash_force",
    );
    assert!(
        forced.status.success(),
        "forced recovery failed: {}",
        forced.stderr
    );

    let recovered = persistence_snapshot(&workspace);
    assert_eq!(
        recovered.anchor, recovered.jsonl,
        "forced recovery must republish a byte-exact anchor"
    );
    let recovered_hash =
        compute_jsonl_hash(&beads_dir.join("issues.jsonl")).expect("hash recovered JSONL");
    assert_eq!(
        recovered.metadata[METADATA_JSONL_CONTENT_HASH].as_deref(),
        Some(recovered_hash.as_str()),
        "forced recovery must restore the cached JSONL hash"
    );
}

#[test]
fn e2e_noop_anchor_accepts_whitespace_only_change_and_copies_exact_bytes() {
    let (workspace, _issue_id) = setup_certified_anchor_workspace("whitespace_only");
    let beads_dir = workspace.root.join(".beads");
    let jsonl_path = beads_dir.join("issues.jsonl");
    let anchor_path = beads_dir.join("beads.base.jsonl");
    let original_jsonl = std::fs::read(&jsonl_path).expect("read baseline JSONL");
    let mut whitespace_changed = b" \t\n\n".to_vec();
    whitespace_changed.extend_from_slice(&original_jsonl);
    whitespace_changed.extend_from_slice(b"\n\t \n");
    std::fs::write(&jsonl_path, &whitespace_changed).expect("write whitespace-only change");
    assert_ne!(
        std::fs::read(&anchor_path).expect("read old anchor"),
        whitespace_changed,
        "test precondition requires byte drift"
    );

    let before = persistence_snapshot(&workspace);
    assert_eq!(
        compute_jsonl_hash(&jsonl_path).expect("hash whitespace-changed JSONL"),
        before.metadata[METADATA_JSONL_CONTENT_HASH]
            .as_deref()
            .expect("stored baseline hash"),
        "whitespace-only drift must retain semantic hash equality"
    );
    let flush = run_br(
        &workspace,
        ["sync", "--flush-only", "--json", "--no-auto-import"],
        "whitespace_only_flush",
    );
    assert!(
        flush.status.success(),
        "whitespace-only no-op flush failed: {}",
        flush.stderr
    );

    let after = persistence_snapshot(&workspace);
    assert_eq!(
        after.jsonl, before.jsonl,
        "successful certification must not rewrite the source JSONL"
    );
    assert_eq!(
        after.anchor, before.jsonl,
        "successful certification must copy the exact whitespace-changed bytes"
    );
    assert_eq!(
        after.metadata, before.metadata,
        "a no-op anchor repair must not mutate sync metadata"
    );
}
