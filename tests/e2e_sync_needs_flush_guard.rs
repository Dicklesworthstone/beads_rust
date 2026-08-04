//! E2E regression tests for issue #405: the internal `needs_flush` metadata
//! marker must NOT be conflated with the user's explicit `--force`, because
//! doing so disables the exporter's data-loss guards ("Refusing to export
//! empty database…" / "Refusing to export stale database…").
//!
//! Armed state reproduction (the documented happy path that destroyed data):
//! 1. `br sync --import-only` sees >=1 local record that differs from JSONL
//!    where local wins -> `needs_flush=true` persisted, no dirty rows.
//! 2. A git merge fast-forwards a JSONL containing issues the local DB has
//!    never seen.
//! 3. `br sync --flush-only` must REFUSE (naming the would-be-lost issues),
//!    not silently rewrite the JSONL without them.
//!
//! The one legitimate reason `needs_flush` previously needed force semantics
//! is `br delete --hard` (purge): the DB intentionally holds fewer issues
//! than the JSONL. That flow is covered here too and must keep working via
//! explicit purged-ID tracking rather than a blanket force.

mod common;

use common::cli::{BrWorkspace, run_br};
use serde_json::Value;
use std::fs;

fn parse_created_id(stdout: &str) -> String {
    let line = stdout.lines().next().unwrap_or("");
    let normalized = line.strip_prefix("✓ ").unwrap_or(line);
    normalized
        .strip_prefix("Created ")
        .and_then(|rest| rest.split(':').next())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Rewrite the JSONL record for `id` so it is OLDER than the local DB row and
/// has different content. The next `sync --import-only` then keeps the local
/// record (`uncertified_local_wins > 0`) and persists `needs_flush=true`.
fn make_jsonl_record_stale(issues_path: &std::path::Path, id: &str) {
    let contents = fs::read_to_string(issues_path).expect("read jsonl");
    let mut out = String::new();
    let mut rewrote = false;
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let mut record: Value = serde_json::from_str(line).expect("parse jsonl line");
        if record.get("id").and_then(Value::as_str) == Some(id) {
            record["title"] = Value::from("Stale remote title");
            record["updated_at"] = Value::from("2000-01-01T00:00:00Z");
            if let Some(obj) = record.as_object_mut() {
                obj.remove("content_hash");
            }
            rewrote = true;
        }
        out.push_str(&serde_json::to_string(&record).expect("serialize"));
        out.push('\n');
    }
    assert!(rewrote, "expected to find {id} in JSONL");
    fs::write(issues_path, out).expect("write jsonl");
}

const MERGED_ISSUE_LINE: &str = "{\"id\":\"bd-merged1\",\"title\":\"Merged from another worktree\",\"status\":\"open\",\"priority\":2,\"issue_type\":\"task\",\"created_at\":\"2026-01-01T00:00:00Z\",\"updated_at\":\"2026-01-01T00:00:00Z\"}\n";

/// Arm `needs_flush=true` with zero dirty rows, then merge a JSONL record the
/// DB has never seen. Returns the workspace and the issues.jsonl path.
fn armed_workspace_with_merged_issue() -> (BrWorkspace, std::path::PathBuf) {
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let issues_path = workspace.root.join(".beads").join("issues.jsonl");

    let create = run_br(
        &workspace,
        [
            "create",
            "Local issue",
            "--no-auto-flush",
            "--no-auto-import",
        ],
        "create_local",
    );
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let local_id = parse_created_id(&create.stdout);
    assert!(!local_id.is_empty(), "no id in: {}", create.stdout);

    let flush = run_br(
        &workspace,
        ["sync", "--flush-only", "--no-auto-import"],
        "seed_flush",
    );
    assert!(
        flush.status.success(),
        "seed flush failed: {}",
        flush.stderr
    );

    // Step 1: make the JSONL copy of the local issue stale, import, local wins.
    make_jsonl_record_stale(&issues_path, &local_id);
    let import = run_br(
        &workspace,
        ["sync", "--import-only", "--no-auto-flush"],
        "arming_import",
    );
    assert!(import.status.success(), "import failed: {}", import.stderr);

    // The import must have armed needs_flush without any dirty rows;
    // status --json exposes both counters.
    let status = run_br(
        &workspace,
        [
            "sync",
            "--status",
            "--json",
            "--no-auto-flush",
            "--no-auto-import",
        ],
        "status_after_arming",
    );
    assert!(status.status.success(), "status failed: {}", status.stderr);

    // Step 2: simulate `git pull` merging a JSONL with an issue the DB lacks.
    let mut contents = fs::read_to_string(&issues_path).expect("read jsonl");
    contents.push_str(MERGED_ISSUE_LINE);
    fs::write(&issues_path, contents).expect("append merged issue");

    (workspace, issues_path)
}

/// Issue #405 core repro: `sync --flush-only` with `needs_flush=true` and no
/// dirty rows must refuse to clobber a JSONL holding issues the DB lacks.
#[test]
fn e2e_flush_only_needs_flush_does_not_destroy_merged_issues() {
    let _log = common::test_log("e2e_flush_only_needs_flush_does_not_destroy_merged_issues");
    let (workspace, issues_path) = armed_workspace_with_merged_issue();

    // Step 3: the recommended pre-commit flush must refuse, not clobber.
    let flush = run_br(
        &workspace,
        ["sync", "--flush-only", "--no-auto-import"],
        "post_merge_flush",
    );
    let jsonl_after = fs::read_to_string(&issues_path).expect("read jsonl");
    assert!(
        jsonl_after.contains("bd-merged1"),
        "flush destroyed the merged issue bd-merged1; JSONL after:\n{jsonl_after}"
    );
    assert!(
        !flush.status.success(),
        "flush must refuse while the DB is stale relative to JSONL; stdout: {} stderr: {}",
        flush.stdout,
        flush.stderr
    );
    assert!(
        flush.stderr.contains("Refusing to export stale database"),
        "expected stale-database guard, got stderr: {}",
        flush.stderr
    );
    assert!(
        flush.stderr.contains("bd-merged1"),
        "guard should name the would-be-lost issue; stderr: {}",
        flush.stderr
    );

    // Recovery: import the merged JSONL, then the flush succeeds losslessly.
    let import = run_br(
        &workspace,
        ["sync", "--import-only", "--no-auto-flush"],
        "recovery_import",
    );
    assert!(
        import.status.success(),
        "recovery import failed: {}",
        import.stderr
    );
    let flush2 = run_br(
        &workspace,
        ["sync", "--flush-only", "--no-auto-import"],
        "recovery_flush",
    );
    assert!(
        flush2.status.success(),
        "recovery flush failed: {}",
        flush2.stderr
    );
    let jsonl_final = fs::read_to_string(&issues_path).expect("read jsonl");
    assert!(
        jsonl_final.contains("bd-merged1"),
        "merged issue must survive the lossless recovery flush"
    );
}

/// Auto-flush variant of the same defect: a mutating command with auto-flush
/// enabled must not silently rewrite the JSONL without the merged issue.
#[test]
fn e2e_auto_flush_needs_flush_does_not_destroy_merged_issues() {
    let _log = common::test_log("e2e_auto_flush_needs_flush_does_not_destroy_merged_issues");
    let (workspace, issues_path) = armed_workspace_with_merged_issue();

    // A mutating command with auto-flush enabled (but auto-import disabled,
    // as in `git pull` racing a command) must not clobber bd-merged1.
    let create = run_br(
        &workspace,
        [
            "create",
            "Another local issue",
            "--no-auto-import",
            "--allow-stale",
        ],
        "create_with_auto_flush",
    );
    let jsonl_after = fs::read_to_string(&issues_path).expect("read jsonl");
    assert!(
        jsonl_after.contains("bd-merged1"),
        "auto-flush destroyed the merged issue bd-merged1 (create exit={:?}); JSONL after:\n{jsonl_after}",
        create.status.code()
    );
}

/// Regression guard for the legitimate flow the old conflation served:
/// `br delete --hard` purges an issue from the DB, and the subsequent flush
/// must still be allowed to write a JSONL with fewer issues (pruning exactly
/// the purged IDs) without tripping the stale-database guard.
#[test]
fn e2e_hard_delete_flush_still_prunes_purged_issues() {
    let _log = common::test_log("e2e_hard_delete_flush_still_prunes_purged_issues");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let issues_path = workspace.root.join(".beads").join("issues.jsonl");

    let keep = run_br(
        &workspace,
        ["create", "Keeper", "--no-auto-flush", "--no-auto-import"],
        "create_keeper",
    );
    assert!(keep.status.success(), "create failed: {}", keep.stderr);
    let victim = run_br(
        &workspace,
        ["create", "Victim", "--no-auto-flush", "--no-auto-import"],
        "create_victim",
    );
    assert!(victim.status.success(), "create failed: {}", victim.stderr);
    let victim_id = parse_created_id(&victim.stdout);
    assert!(!victim_id.is_empty(), "no id in: {}", victim.stdout);

    let flush = run_br(
        &workspace,
        ["sync", "--flush-only", "--no-auto-import"],
        "seed_flush",
    );
    assert!(
        flush.status.success(),
        "seed flush failed: {}",
        flush.stderr
    );
    assert!(
        fs::read_to_string(&issues_path)
            .expect("read jsonl")
            .contains(&victim_id),
        "victim must be in JSONL before the purge"
    );

    // Soft-delete then purge with auto-flush disabled so the explicit
    // flush-only path is the one that has to prune the purged ID.
    let del = run_br(
        &workspace,
        [
            "delete",
            &victim_id,
            "--force",
            "--no-auto-flush",
            "--no-auto-import",
        ],
        "soft_delete",
    );
    assert!(del.status.success(), "delete failed: {}", del.stderr);
    let purge = run_br(
        &workspace,
        [
            "delete",
            &victim_id,
            "--hard",
            "--force",
            "--no-auto-flush",
            "--no-auto-import",
        ],
        "hard_delete",
    );
    assert!(
        purge.status.success(),
        "hard delete failed: {}",
        purge.stderr
    );

    let flush2 = run_br(
        &workspace,
        ["sync", "--flush-only", "--no-auto-import"],
        "post_purge_flush",
    );
    assert!(
        flush2.status.success(),
        "flush after purge must succeed without --force; stderr: {}",
        flush2.stderr
    );
    let jsonl_after = fs::read_to_string(&issues_path).expect("read jsonl");
    assert!(
        !jsonl_after.contains(&victim_id),
        "purged issue must be pruned from JSONL"
    );
    assert!(
        jsonl_after.contains("Keeper"),
        "keeper issue must survive the post-purge flush"
    );
}
