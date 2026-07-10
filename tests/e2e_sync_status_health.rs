//! E2E coverage for the `br sync --status --json` additions:
//!
//! - beads_rust#338: read-only `git_export` block (tracked/dirty JSONL
//!   visibility; `{available:false}` outside a git repo).
//! - beads_rust#334: `workspace_health` + `reliability_audit` fields in
//!   the same write-gate vocabulary as `br doctor --json`.

mod common;

use common::cli::{BrWorkspace, extract_json_payload, run_br};
use serde_json::Value;
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
        git_export["head_hash"].as_str().expect("head hash"),
        committed_head,
        "{committed}"
    );
    assert_eq!(committed_head.len(), 40, "{committed}");

    // Dirty the tracked JSONL with a git-level edit that br will NOT undo
    // on a read-only status call. This is the regression target: a dirty
    // tracked issues.jsonl that DB-vs-JSONL drift alone cannot see.
    {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&jsonl_path)
            .expect("open jsonl for append");
        writeln!(f, "{{\"id\":\"bd-extra-untracked-edit\"}}").expect("append to jsonl");
    }

    // Use --no-auto-import so the status open does not absorb the edit
    // back into the DB before we read the worktree's git state.
    let dirty = sync_status_json_no_auto_import(&workspace, "status_dirty");
    let git_export = &dirty["git_export"];
    assert_eq!(git_export["available"], true, "{dirty}");
    assert_eq!(git_export["tracked"], true, "{dirty}");
    // The committed copy must now differ from the on-disk copy — the
    // core #338 signal: a dirty tracked issues.jsonl that DB-vs-JSONL
    // drift alone cannot see. We assert via the hashes (git's own
    // content view) rather than a specific porcelain column, because
    // git's racy-clean stat handling can attribute a same-second
    // commit-then-edit to either the index or worktree column.
    assert_ne!(
        git_export["head_hash"].as_str().expect("head hash"),
        git_export["worktree_hash"].as_str().expect("worktree hash"),
        "dirty worktree must hash differently from HEAD: {dirty}"
    );
    let worktree_clean = git_export["worktree_clean"]
        .as_bool()
        .expect("worktree_clean");
    let index_clean = git_export["index_clean"].as_bool().expect("index_clean");
    assert!(
        !worktree_clean || !index_clean,
        "an edited tracked JSONL must be reported dirty in the index or worktree: {dirty}"
    );
}

/// Resolve the committed blob hash for `relpath` via git, returning
/// `None` when the path is absent from HEAD.
fn git_committed_blob_hash(root: &Path, relpath: &str) -> Option<String> {
    let out = git(
        root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("HEAD:{relpath}"),
        ],
    );
    if !out.status.success() {
        return None;
    }
    let hash = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if hash.is_empty() { None } else { Some(hash) }
}

#[test]
fn e2e_sync_status_git_export_unavailable_outside_repo() {
    let _log = common::test_log("e2e_sync_status_git_export_unavailable_outside_repo");
    let workspace = BrWorkspace::new();

    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let status = sync_status_json(&workspace, "status_no_git");
    let git_export = &status["git_export"];
    assert_eq!(git_export["available"], false, "{status}");
    for absent in [
        "tracked",
        "worktree_clean",
        "index_clean",
        "head_hash",
        "worktree_hash",
    ] {
        assert!(
            git_export.get(absent).is_none(),
            "{absent} must be omitted when git is unavailable: {status}"
        );
    }
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
