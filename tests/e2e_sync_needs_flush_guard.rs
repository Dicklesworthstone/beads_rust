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

use beads_rust::franken_sync::Connection;
use beads_rust::model::Issue;
use beads_rust::util::content_hash;
use common::cli::{BrWorkspace, extract_json_payload, run_br};
use fsqlite_types::SqliteValue;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;

fn checked_br(workspace: &BrWorkspace, args: &[&str], label: &str) -> common::cli::BrRun {
    let result = run_br(workspace, args, label);
    assert!(result.status.success(), "{label}: {result:?}");
    result
}

fn git(workspace: &BrWorkspace, args: &[&str]) {
    let result = std::process::Command::new("git")
        .current_dir(&workspace.root)
        .args([
            "-c",
            "user.name=br-sync-e2e",
            "-c",
            "user.email=br-sync-e2e@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", workspace.root.join(".gitconfig"))
        .output()
        .expect("run git fixture");
    assert!(result.status.success(), "git {args:?}: {result:?}");
}

fn jsonl_records(path: &std::path::Path) -> Vec<Value> {
    let records: Vec<Value> = fs::read_to_string(path)
        .expect("read JSONL")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("JSONL record"))
        .collect();
    let mut ids = BTreeSet::new();
    for record in &records {
        let id = record["id"].as_str().expect("issue ID");
        assert!(ids.insert(id), "duplicate issue ID {id}");
    }
    records
}

fn jsonl_record(path: &std::path::Path, id: &str) -> Value {
    jsonl_records(path)
        .into_iter()
        .find(|record| record["id"] == id)
        .expect("issue in JSONL")
}

fn show_without_sync(workspace: &BrWorkspace, id: &str, label: &str) -> Value {
    let output = checked_br(
        workspace,
        &["show", id, "--json", "--no-auto-import", "--no-auto-flush"],
        label,
    );
    let issues: Vec<Value> =
        serde_json::from_str(&extract_json_payload(&output.stdout)).expect("shown issue JSON");
    assert_eq!(issues.len(), 1, "expected exactly issue {id}");
    issues.into_iter().next().expect("shown issue")
}

fn database(workspace: &BrWorkspace) -> Connection {
    Connection::open(
        workspace
            .root
            .join(".beads/beads.db")
            .to_string_lossy()
            .into_owned(),
    )
    .expect("open database for sync evidence")
}

/// Read evidence directly so the assertion cannot trigger an import or
/// certify the generation whose preservation it is checking.
fn sync_metadata(workspace: &BrWorkspace) -> BTreeMap<String, String> {
    database(workspace)
        .query(
            "SELECT key, value FROM metadata WHERE key IN \
             ('jsonl_content_hash', 'jsonl_mtime', 'jsonl_size', 'jsonl_fast_witness', \
              'last_import_time', 'last_export_time', 'needs_flush', \
              'purged_ids_pending_export', 'import_omitted_ids', 'jsonl_row_hashes', \
              'jsonl_row_receipt_version', 'jsonl_unimported_source') ORDER BY key",
        )
        .expect("read sync metadata")
        .iter()
        .map(|row| {
            (
                row.get(0)
                    .and_then(SqliteValue::as_text)
                    .expect("metadata key")
                    .to_string(),
                row.get(1)
                    .and_then(SqliteValue::as_text)
                    .expect("metadata value")
                    .to_string(),
            )
        })
        .collect()
}

fn dirty_ids(workspace: &BrWorkspace) -> Vec<String> {
    database(workspace)
        .query("SELECT issue_id FROM dirty_issues ORDER BY issue_id")
        .expect("read dirty IDs")
        .iter()
        .map(|row| {
            row.get(0)
                .and_then(SqliteValue::as_text)
                .expect("dirty issue ID")
                .to_string()
        })
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
struct SyncWitness {
    source: Vec<u8>,
    anchor: Vec<u8>,
    metadata: BTreeMap<String, String>,
    certificates: Vec<(String, String, String)>,
}

fn sync_witness(workspace: &BrWorkspace) -> SyncWitness {
    let certificates = database(workspace)
        .query("SELECT issue_id, content_hash, exported_at FROM export_hashes ORDER BY issue_id")
        .expect("read export certificates")
        .iter()
        .map(|row| {
            let field = |index| {
                row.get(index)
                    .and_then(SqliteValue::as_text)
                    .expect("certificate field")
                    .to_string()
            };
            (field(0), field(1), field(2))
        })
        .collect();
    SyncWitness {
        source: fs::read(workspace.root.join(".beads/issues.jsonl")).expect("source bytes"),
        anchor: fs::read(workspace.root.join(".beads/beads.base.jsonl")).expect("merge anchor"),
        metadata: sync_metadata(workspace),
        certificates,
    }
}

fn assert_comment_only_change(before: &Value, after: &Value) {
    assert_eq!(before["updated_at"], after["updated_at"]);
    let before: Issue = serde_json::from_value(before.clone()).expect("issue before comment");
    let after: Issue = serde_json::from_value(after.clone()).expect("issue after comment");
    assert_ne!(before.comments, after.comments, "peer added a comment");
    assert_eq!(
        content_hash(&before),
        content_hash(&after),
        "comment-only changes must exercise fields absent from the issue content hash"
    );
}

fn add_peer_comment(path: &std::path::Path, id: &str) -> Value {
    let mut rows = jsonl_records(path);
    let row = rows.iter_mut().find(|row| row["id"] == id).unwrap();
    let before = row.clone();
    row["comments"] = serde_json::json!([{
        "id": 101,
        "issue_id": id,
        "author": "Peer",
        "text": "Unread peer comment",
        "created_at": "2026-01-01T00:00:00Z"
    }]);
    assert_comment_only_change(&before, row);
    let peer_row = row.clone();
    let bytes = rows
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    fs::write(path, bytes).expect("peer comment-only generation");
    peer_row
}

fn git_workspace() -> BrWorkspace {
    let workspace = BrWorkspace::new();
    git(&workspace, &["init", "--initial-branch=main"]);
    checked_br(&workspace, &["init", "--prefix", "bd"], "init");
    for name in ["config.yaml", "metadata.json", ".gitignore"] {
        let path = format!(".beads/{name}");
        if workspace.root.join(&path).exists() {
            git(&workspace, &["add", "--", &path]);
        }
    }
    workspace
}

fn commit_jsonl(workspace: &BrWorkspace, message: &str) {
    git(workspace, &["add", "--", ".beads/issues.jsonl"]);
    git(workspace, &["commit", "-m", message]);
}

/// A real clone receives only tracked files and reconstructs its independent
/// database through normal CLI startup.
fn clone_peer(workspace: &BrWorkspace) -> BrWorkspace {
    let mut peer = BrWorkspace::new();
    let checkout = peer.root.join("checkout");
    git(
        workspace,
        &["clone", "--no-hardlinks", ".", checkout.to_str().unwrap()],
    );
    peer.root = checkout;
    checked_br(&peer, &["list", "--json"], "peer_rebuild_database");
    peer
}

fn pull_peer(workspace: &BrWorkspace, peer: &BrWorkspace) {
    git(
        workspace,
        &["pull", "--ff-only", peer.root.to_str().unwrap(), "main"],
    );
}

#[test]
fn legacy_workspace_refreshes_row_receipts_before_mutation() {
    let workspace = BrWorkspace::new();
    checked_br(&workspace, &["init", "--prefix", "bd"], "init");
    let id = parse_created_id(&checked_br(&workspace, &["create", "Legacy"], "create").stdout);
    let db_path = workspace.root.join(".beads/beads.db");
    {
        let mut storage = beads_rust::storage::SqliteStorage::open(&db_path).unwrap();
        storage
            .set_metadata("jsonl_row_receipt_version", "0")
            .unwrap();
        storage.set_metadata("jsonl_row_hashes", "{}").unwrap();
    }
    let result = checked_br(
        &workspace,
        &["update", &id, "--title", "Upgraded"],
        "update",
    );
    assert!(!result.stderr.contains("automatic JSONL export failed"));
    assert_eq!(
        jsonl_records(&workspace.root.join(".beads/issues.jsonl"))[0]["title"],
        "Upgraded"
    );
}

#[test]
fn incremental_export_preserves_unread_peer_comments_until_import() {
    let workspace = BrWorkspace::new();
    checked_br(&workspace, &["init", "--prefix", "bd"], "init");
    let local = parse_created_id(&checked_br(&workspace, &["create", "Local"], "local").stdout);
    let peer = parse_created_id(&checked_br(&workspace, &["create", "Peer"], "peer").stdout);
    checked_br(&workspace, &["sync", "--flush-only"], "seed_merge_anchor");
    checked_br(
        &workspace,
        &["update", &local, "--title", "Local edit", "--no-auto-flush"],
        "dirty",
    );
    let path = workspace.root.join(".beads/issues.jsonl");
    let receipts_before: Value =
        serde_json::from_str(&sync_metadata(&workspace)["jsonl_row_hashes"]).unwrap();
    let peer_row = add_peer_comment(&path, &peer);
    let append = checked_br(
        &workspace,
        &["create", "Safe append", "--no-auto-import", "--allow-stale"],
        "append",
    );
    assert!(!append.stderr.contains("automatic JSONL export failed"));
    let appended = parse_created_id(&append.stdout);
    assert_eq!(jsonl_record(&path, &local)["title"], "Local edit");
    assert_eq!(jsonl_record(&path, &appended)["title"], "Safe append");
    assert_eq!(
        jsonl_records(&path).iter().find(|row| row["id"] == peer),
        Some(&peer_row)
    );
    assert!(dirty_ids(&workspace).is_empty());
    let metadata_after = sync_metadata(&workspace);
    assert_eq!(metadata_after["jsonl_unimported_source"], "true");
    let receipts_after: Value = serde_json::from_str(&metadata_after["jsonl_row_hashes"]).unwrap();
    let peer_receipt_before = receipts_before
        .get(&peer)
        .and_then(Value::as_str)
        .expect("peer row has a synchronized receipt before the external comment");
    let peer_receipt_after = receipts_after
        .get(&peer)
        .and_then(Value::as_str)
        .expect("unrelated publication retains the peer's previous receipt");
    assert_eq!(peer_receipt_after, peer_receipt_before);
    let unread: Issue =
        serde_json::from_value(show_without_sync(&workspace, &peer, "still_unread"))
            .expect("unimported peer issue");
    assert!(unread.comments.is_empty());
    let preserved = fs::read(&path).unwrap();
    let witness = sync_witness(&workspace);
    let conflict = checked_br(
        &workspace,
        &[
            "update",
            &peer,
            "--title",
            "Pending local peer edit",
            "--no-auto-import",
            "--allow-stale",
        ],
        "conflict",
    );
    assert!(
        conflict
            .stderr
            .contains("Warning: Mutation succeeded, but automatic JSONL export failed.")
            && conflict.stderr.contains("Sync conflict:")
            && conflict.stderr.contains("JSONL changed"),
        "expected a visible human sync-conflict warning: {conflict:?}"
    );
    assert_eq!(
        fs::read(&path).unwrap(),
        preserved,
        "comment-only peer changes must block replacement even after an unrelated export"
    );
    assert_eq!(sync_witness(&workspace), witness);
    assert!(dirty_ids(&workspace).contains(&peer));
    assert_eq!(
        show_without_sync(&workspace, &peer, "local_conflict_preserved")["title"],
        "Pending local peer edit"
    );
    checked_br(&workspace, &["sync", "--merge", "--force"], "merge");
    let shown = checked_br(&workspace, &["show", &peer, "--json"], "show");
    assert!(shown.stdout.contains("Unread peer comment"));
    assert!(shown.stdout.contains("Pending local peer edit"));
}

#[test]
fn incremental_export_keeps_unrelated_peer_changes_pending_for_auto_import() {
    let workspace = BrWorkspace::new();
    checked_br(&workspace, &["init", "--prefix", "bd"], "init");
    let peer = parse_created_id(&checked_br(&workspace, &["create", "Peer"], "peer").stdout);
    let path = workspace.root.join(".beads/issues.jsonl");
    let mut rows = jsonl_records(&path);
    rows[0]["title"] = serde_json::json!("Pulled peer title");
    rows[0]["updated_at"] = serde_json::json!("2099-01-01T00:00:00Z");
    fs::write(&path, format!("{}\n", rows[0])).unwrap();
    checked_br(
        &workspace,
        &["create", "Safe append", "--no-auto-import", "--allow-stale"],
        "append",
    );
    let shown = checked_br(&workspace, &["show", &peer, "--json"], "auto_import");
    assert!(shown.stdout.contains("Pulled peer title"));
}

/// #548: a real branch checkout omits a locally published issue. Later peer
/// changes to an existing ID (including comments) must still be imported.
#[test]
fn branch_switch_imports_peer_edits_and_comments_before_mutation() {
    let workspace = git_workspace();
    let shared = checked_br(&workspace, &["create", "Shared issue"], "shared");
    let shared_id = parse_created_id(&shared.stdout);
    let path = workspace.root.join(".beads/issues.jsonl");
    commit_jsonl(&workspace, "shared seed");
    git(&workspace, &["checkout", "-b", "feature"]);
    let local = checked_br(&workspace, &["create", "Branch local issue"], "local");
    let local_id = parse_created_id(&local.stdout);
    commit_jsonl(&workspace, "feature issue");
    git(&workspace, &["checkout", "main"]);
    checked_br(&workspace, &["list", "--json"], "after_checkout");
    assert!(dirty_ids(&workspace).is_empty());
    assert_eq!(sync_metadata(&workspace)["needs_flush"], "false");

    // A real clone reconstructs an independent database from tracked JSONL.
    let peer = clone_peer(&workspace);
    checked_br(
        &peer,
        &["update", &shared_id, "--title", "Peer revised shared issue"],
        "peer_title",
    );
    checked_br(&peer, &["close", &shared_id], "peer_close");
    checked_br(
        &peer,
        &["comments", "add", &shared_id, "Peer discussion"],
        "peer_comment",
    );
    let peer_records = jsonl_records(&peer.root.join(".beads/issues.jsonl"));
    commit_jsonl(&peer, "peer title, close, and discussion");
    pull_peer(&workspace, &peer);
    checked_br(&workspace, &["create", "Subsequent mutation"], "after_pull");
    let records = jsonl_records(&path);
    let shared_after = records
        .iter()
        .find(|row| row["id"] == shared_id)
        .expect("shared row");
    assert_eq!(
        shared_after, &peer_records[0],
        "peer close and comments survived"
    );
    assert!(!records.iter().any(|row| row["id"] == local_id));
    let local_show = checked_br(
        &workspace,
        &["show", &local_id, "--json"],
        "local_preserved",
    );
    assert!(local_show.stdout.contains("Branch local issue"));
    let shared_show = checked_br(&workspace, &["show", &shared_id, "--json"], "peer_imported");
    assert!(shared_show.stdout.contains("Peer discussion"));
    checked_br(
        &workspace,
        &["update", &local_id, "--title", "Intentional local edit"],
        "publish_local_edit",
    );
    let republished = jsonl_records(&path);
    assert!(
        republished
            .iter()
            .any(|row| row["id"] == local_id && row["title"] == "Intentional local edit")
    );
    assert_eq!(
        republished.iter().find(|row| row["id"] == shared_id),
        Some(&peer_records[0])
    );
}

/// #548: importing a peer's purge preserves the old local row as branch
/// history, but an unrelated mutation must not publish it again.
#[test]
fn peer_purge_is_not_undone_by_subsequent_mutation() {
    for tombstone in [false, true] {
        let workspace = BrWorkspace::new();
        checked_br(&workspace, &["init", "--prefix", "bd"], "init");
        checked_br(&workspace, &["create", "Keeper"], "keeper");
        let victim = checked_br(&workspace, &["create", "Peer purge victim"], "victim");
        let victim_id = parse_created_id(&victim.stdout);
        if tombstone {
            checked_br(&workspace, &["delete", &victim_id, "--force"], "tombstone");
        }
        let path = workspace.root.join(".beads/issues.jsonl");
        let peer = BrWorkspace::new();
        checked_br(&peer, &["init", "--prefix", "bd"], "peer_init");
        fs::copy(&path, peer.root.join(".beads/issues.jsonl")).expect("peer copy");
        checked_br(&peer, &["sync", "--import-only"], "peer_import");
        checked_br(
            &peer,
            &["delete", &victim_id, "--hard", "--force"],
            "peer_purge",
        );
        fs::copy(peer.root.join(".beads/issues.jsonl"), &path).expect("pull purge");
        checked_br(&workspace, &["list", "--json"], "import_purge");
        checked_br(&workspace, &["create", "After peer purge"], "mutate");
        assert!(
            !jsonl_records(&path)
                .iter()
                .any(|row| row["id"] == victim_id)
        );
        checked_br(
            &workspace,
            &["show", &victim_id, "--json"],
            "preserved_local_row",
        );
        checked_br(
            &workspace,
            &["sync", "--flush-only"],
            "clean_flush_after_purge",
        );
        assert!(
            !jsonl_records(&path)
                .iter()
                .any(|row| row["id"] == victim_id)
        );
        checked_br(
            &workspace,
            &["create", "Delayed local edit", "--no-auto-flush"],
            "pending_after_purge",
        );
        let source_before = fs::read(&path).expect("purged source");
        let full_flush = run_br(
            &workspace,
            ["sync", "--flush-only"],
            "full_flush_after_purge",
        );
        assert!(
            !full_flush.status.success(),
            "must not republish a peer purge"
        );
        assert!(full_flush.stderr.contains("republish rows omitted"));
        assert_eq!(fs::read(&path).expect("purged source"), source_before);
        checked_br(
            &workspace,
            &["create", "Publish pending local work"],
            "publish_pending",
        );
        let published = jsonl_records(&path);
        assert!(
            published
                .iter()
                .any(|row| row["title"] == "Delayed local edit")
        );
        assert!(!published.iter().any(|row| row["id"] == victim_id));
    }
}

/// Both the full needs_flush path and the incremental dirty-row path must
/// refuse unimported edits to existing IDs, even when no new IDs appear.
#[test]
fn auto_flush_refuses_existing_id_peer_edits_with_pending_local_changes() {
    for full_export in [false, true] {
        let workspace = BrWorkspace::new();
        checked_br(&workspace, &["init", "--prefix", "bd"], "init");
        let issue = checked_br(&workspace, &["create", "Local title"], "create");
        let id = parse_created_id(&issue.stdout);
        let path = workspace.root.join(".beads/issues.jsonl");
        checked_br(&workspace, &["sync", "--flush-only"], "seed_merge_anchor");
        if full_export {
            make_jsonl_record_stale(&path, &id);
            checked_br(
                &workspace,
                &["sync", "--import-only", "--no-auto-flush"],
                "arm_full",
            );
        } else {
            checked_br(
                &workspace,
                &[
                    "update",
                    &id,
                    "--title",
                    "Pending local title",
                    "--no-auto-flush",
                ],
                "arm_incremental",
            );
        }
        let mut rows = jsonl_records(&path);
        rows[0]["title"] = Value::from("New peer title");
        rows[0]["updated_at"] = Value::from("2099-01-01T00:00:00Z");
        let peer_bytes = format!("{}\n", serde_json::to_string(&rows[0]).expect("peer JSON"));
        fs::write(&path, &peer_bytes).expect("peer update");
        let explicit = run_br(
            &workspace,
            ["sync", "--flush-only", "--no-auto-import"],
            "explicit_stale_flush",
        );
        assert!(
            !explicit.status.success(),
            "unread peer edits must refuse export"
        );
        assert!(explicit.stderr.contains("JSONL changed since"));
        assert_eq!(fs::read_to_string(&path).expect("source"), peer_bytes);
        let mutation = run_br(
            &workspace,
            [
                "create",
                "Legitimate local creation",
                "--allow-stale",
                "--no-auto-import",
            ],
            "mutate_stale",
        );
        assert_eq!(fs::read_to_string(&path).expect("source"), peer_bytes);
        assert!(
            mutation.stderr.contains("AUTO_FLUSH_FAILED")
                || mutation.stderr.contains("JSONL changed"),
            "visible conflict: {mutation:?}"
        );
        let local = checked_br(
            &workspace,
            &["list", "--json", "--no-auto-import"],
            "pending_preserved",
        );
        assert!(local.stdout.contains("Legitimate local creation"));
        assert!(local.stdout.contains(if full_export {
            "Local title"
        } else {
            "Pending local title"
        }));
        checked_br(
            &workspace,
            &["sync", "--merge", "--force"],
            "recover_pending_changes",
        );
        let recovered = jsonl_records(&path);
        assert!(recovered.iter().any(|row| row["title"] == "New peer title"));
        assert!(
            recovered
                .iter()
                .any(|row| row["title"] == "Legitimate local creation")
        );
    }
}

#[test]
fn full_auto_flush_does_not_republish_omissions_alongside_local_wins() {
    let workspace = BrWorkspace::new();
    checked_br(&workspace, &["init", "--prefix", "bd"], "init");
    let keeper = checked_br(&workspace, &["create", "Local winner"], "keeper");
    let keeper_id = parse_created_id(&keeper.stdout);
    let victim = checked_br(&workspace, &["create", "Omitted issue"], "victim");
    let victim_id = parse_created_id(&victim.stdout);
    let path = workspace.root.join(".beads/issues.jsonl");
    let keeper_line = jsonl_records(&path)
        .into_iter()
        .find(|row| row["id"] == keeper_id)
        .expect("keeper source row");
    fs::write(
        &path,
        format!("{}\n", serde_json::to_string(&keeper_line).expect("JSON")),
    )
    .expect("source omits victim");
    make_jsonl_record_stale(&path, &keeper_id);
    checked_br(
        &workspace,
        &["sync", "--import-only", "--no-auto-flush"],
        "import_local_win_and_omission",
    );
    let source_before = fs::read(&path).expect("source bytes");
    let mutation = run_br(&workspace, ["create", "Saved pending creation"], "mutate");
    assert!(mutation.status.success(), "local mutation: {mutation:?}");
    assert!(mutation.stderr.contains("republish rows omitted"));
    assert_eq!(fs::read(&path).expect("source bytes"), source_before);
    checked_br(
        &workspace,
        &["show", &victim_id, "--json"],
        "omission_preserved",
    );
    let pending = checked_br(&workspace, &["list", "--json"], "pending_preserved");
    assert!(pending.stdout.contains("Saved pending creation"));
    assert!(pending.stdout.contains("Local winner"));
}

#[test]
fn first_mutation_without_auto_import_can_publish_to_empty_jsonl() {
    let workspace = BrWorkspace::new();
    checked_br(&workspace, &["init", "--prefix", "bd"], "init");
    let created = checked_br(
        &workspace,
        &["create", "First local issue", "--no-auto-import"],
        "first_export",
    );
    assert!(!created.stderr.contains("automatic JSONL export failed"));
    let records = jsonl_records(&workspace.root.join(".beads/issues.jsonl"));
    assert_eq!(records.len(), 1);
    assert_eq!(records[0]["title"], "First local issue");
}

fn recover_comment_and_local_creation(
    workspace: &BrWorkspace,
    shared: &str,
    local_only: &str,
    automatic: bool,
) {
    checked_br(
        workspace,
        &["sync", "--import-only", "--no-auto-flush"],
        "recovery_import_comment",
    );
    assert!(dirty_ids(workspace).contains(&local_only.to_string()));
    checked_br(
        workspace,
        &["sync", "--flush-only", "--no-auto-import"],
        "recovery_flush_comment",
    );
    let path = workspace.root.join(".beads/issues.jsonl");
    let shared_row = jsonl_record(&path, shared);
    assert_eq!(shared_row["title"], "Local issue");
    let comments = shared_row["comments"].as_array().expect("peer comments");
    assert_eq!(comments.len(), 1, "peer comment survived exactly once");
    assert_eq!(comments[0]["author"], "Peer");
    assert_eq!(comments[0]["text"], "Unread peer comment");
    let local_row = jsonl_record(&path, local_only);
    assert_eq!(local_row["title"], "Legitimate pending local work");
    if automatic {
        assert_eq!(local_row["description"], "Local mutation survives refusal");
    }
    assert!(dirty_ids(workspace).is_empty());
    assert_eq!(sync_metadata(workspace)["needs_flush"], "false");
    assert_eq!(sync_metadata(workspace)["jsonl_unimported_source"], "false");
}

/// Full exports must detect an unread comment even when the issue's scalar
/// content hash and timestamp are unchanged. Explicit import enables recovery.
#[test]
fn needs_flush_preserves_comment_only_peer_change_until_explicit_import() {
    for automatic in [false, true] {
        let workspace = BrWorkspace::new();
        checked_br(&workspace, &["init", "--prefix", "bd"], "init");
        let shared =
            parse_created_id(&checked_br(&workspace, &["create", "Local issue"], "shared").stdout);
        let path = workspace.root.join(".beads/issues.jsonl");
        checked_br(&workspace, &["sync", "--flush-only"], "seed_anchor");
        make_jsonl_record_stale(&path, &shared);
        checked_br(
            &workspace,
            &["sync", "--import-only", "--no-auto-flush"],
            "arm_local_win",
        );
        assert!(dirty_ids(&workspace).is_empty());
        assert_eq!(sync_metadata(&workspace)["needs_flush"], "true");
        let local_only = parse_created_id(
            &checked_br(
                &workspace,
                &[
                    "create",
                    "Legitimate pending local work",
                    "--no-auto-import",
                    "--no-auto-flush",
                ],
                "pending_local",
            )
            .stdout,
        );
        add_peer_comment(&path, &shared);
        let witness = sync_witness(&workspace);
        if automatic {
            let mutation = checked_br(
                &workspace,
                &[
                    "update",
                    &local_only,
                    "--description",
                    "Local mutation survives refusal",
                    "--no-auto-import",
                    "--allow-stale",
                ],
                "automatic_comment_conflict",
            );
            assert!(
                mutation
                    .stderr
                    .contains("Warning: Mutation succeeded, but automatic JSONL export failed.")
                    && mutation.stderr.contains("Sync conflict:")
                    && mutation.stderr.contains("JSONL changed"),
                "expected a visible human sync-conflict warning: {mutation:?}"
            );
        } else {
            let flush = run_br(
                &workspace,
                ["sync", "--flush-only", "--no-auto-import"],
                "explicit_comment_conflict",
            );
            assert!(!flush.status.success(), "comment conflict: {flush:?}");
            assert!(flush.stderr.contains("JSONL changed since"));
        }
        assert_eq!(
            sync_witness(&workspace),
            witness,
            "refusal must preserve source, anchor, receipts, and export certificates"
        );
        assert!(dirty_ids(&workspace).contains(&local_only));
        let local = show_without_sync(&workspace, &local_only, "local_work_preserved");
        assert_eq!(local["title"], "Legitimate pending local work");
        if automatic {
            assert_eq!(local["description"], "Local mutation survives refusal");
        }
        recover_comment_and_local_creation(&workspace, &shared, &local_only, automatic);
    }
}

fn assert_peer_purge_preserves_preexisting_dirty_work(tombstone: bool) {
    let workspace = git_workspace();
    let victim = parse_created_id(
        &checked_br(&workspace, &["create", "Peer purge target"], "victim").stdout,
    );
    let keeper =
        parse_created_id(&checked_br(&workspace, &["create", "Shared keeper"], "keeper").stdout);
    if tombstone {
        checked_br(
            &workspace,
            &[
                "delete",
                &victim,
                "--force",
                "--reason",
                "Deletion history predating the peer purge",
            ],
            "soft_delete_victim",
        );
    }
    let history = show_without_sync(&workspace, &victim, "original_history");
    commit_jsonl(&workspace, "Shared baseline before purge");
    let peer = clone_peer(&workspace);
    let local_only = parse_created_id(
        &checked_br(
            &workspace,
            &["create", "Unflushed local issue", "--no-auto-flush"],
            "pending_before_pull",
        )
        .stdout,
    );
    checked_br(
        &peer,
        &["delete", &victim, "--hard", "--force"],
        "peer_hard_delete",
    );
    commit_jsonl(&peer, "Peer purged an issue");
    pull_peer(&workspace, &peer);
    checked_br(
        &workspace,
        &["sync", "--import-only", "--no-auto-flush"],
        "import_purge_with_dirty_local_work",
    );
    assert_eq!(sync_metadata(&workspace)["needs_flush"], "false");
    assert!(dirty_ids(&workspace).contains(&local_only));
    assert_eq!(
        show_without_sync(&workspace, &victim, "history_after_import"),
        history,
        "import preserves the complete omitted issue and deletion history"
    );
    let mutation = checked_br(
        &workspace,
        &["update", &keeper, "--title", "Legitimate local keeper edit"],
        "mutate_after_peer_purge",
    );
    assert!(!mutation.stderr.contains("automatic JSONL export failed"));
    let path = workspace.root.join(".beads/issues.jsonl");
    assert!(!jsonl_records(&path).iter().any(|row| row["id"] == victim));
    assert_eq!(
        jsonl_record(&path, &local_only)["title"],
        "Unflushed local issue"
    );
    assert_eq!(
        jsonl_record(&path, &keeper)["title"],
        "Legitimate local keeper edit"
    );
    assert!(dirty_ids(&workspace).is_empty());
    assert_eq!(
        show_without_sync(&workspace, &victim, "history_after_mutation"),
        history,
        "publishing legitimate local work must retain omitted local history"
    );
}

/// Pending local-only work predates a real peer purge and explicit import.
/// Both live issues and tombstones stay omitted without losing local history.
#[test]
fn peer_purge_keeps_preexisting_dirty_local_only_work_publishable() {
    for tombstone in [false, true] {
        assert_peer_purge_preserves_preexisting_dirty_work(tombstone);
    }
}

/// A pending purge belongs to the deleted incarnation, not a replacement
/// reintroduced under the same ID before the next automatic export.
#[test]
fn hard_delete_then_reimport_same_id_auto_flush_publishes_replacement() {
    let workspace = BrWorkspace::new();
    checked_br(&workspace, &["init", "--prefix", "bd"], "init");
    let victim = parse_created_id(
        &checked_br(
            &workspace,
            &["create", "Original issue before purge"],
            "original",
        )
        .stdout,
    );
    let path = workspace.root.join(".beads/issues.jsonl");
    checked_br(
        &workspace,
        &["delete", &victim, "--hard", "--force", "--no-auto-flush"],
        "pending_purge",
    );
    assert_eq!(sync_metadata(&workspace)["needs_flush"], "true");
    assert!(dirty_ids(&workspace).is_empty());
    let mut replacement = jsonl_record(&path, &victim);
    assert_eq!(replacement["title"], "Original issue before purge");
    replacement["title"] = Value::from("Replacement after intentional purge");
    replacement.as_object_mut().unwrap().remove("content_hash");
    fs::write(&path, format!("{replacement}\n")).expect("peer reintroduces the exact ID");
    checked_br(
        &workspace,
        &["sync", "--import-only", "--no-auto-flush"],
        "reimport_before_purge_flush",
    );
    checked_br(
        &workspace,
        &[
            "update",
            &victim,
            "--description",
            "New incarnation remains publishable",
        ],
        "mutate_replacement",
    );
    let rows = jsonl_records(&path);
    assert_eq!(rows.len(), 1, "replacement must be exported exactly once");
    assert_eq!(rows[0]["id"], victim);
    assert_eq!(rows[0]["title"], "Replacement after intentional purge");
    assert_eq!(rows[0]["status"], "open");
    assert_eq!(
        rows[0]["description"],
        "New incarnation remains publishable"
    );
    assert!(dirty_ids(&workspace).is_empty());
    assert_eq!(sync_metadata(&workspace)["needs_flush"], "false");
}

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

/// GitHub #453: hard deletion must remove DB-only capacity attribution rather
/// than leaving an orphan that poisons additive-reconcile health checks.
#[test]
fn e2e_hard_delete_removes_capacity_occupancy_and_preserves_db_health() {
    let _log =
        common::test_log("e2e_hard_delete_removes_capacity_occupancy_and_preserves_db_health");
    let workspace = BrWorkspace::new();
    let init = run_br(&workspace, ["init"], "init");
    assert!(init.status.success(), "init failed: {}", init.stderr);

    let create = run_br(&workspace, ["create", "Occupied victim"], "create_victim");
    assert!(create.status.success(), "create failed: {}", create.stderr);
    let victim_id = parse_created_id(&create.stdout);
    assert!(!victim_id.is_empty(), "no id in: {}", create.stdout);

    let occupy = run_br(
        &workspace,
        ["update", &victim_id, "--status", "in_progress"],
        "occupy_victim",
    );
    assert!(occupy.status.success(), "update failed: {}", occupy.stderr);

    let db_path = workspace.root.join(".beads/beads.db");
    {
        let conn = Connection::open(db_path.to_string_lossy().into_owned())
            .expect("open database before purge");
        let rows = conn
            .query_with_params(
                "SELECT COUNT(*) FROM capacity_occupancy WHERE issue_id = ?",
                &[SqliteValue::from(victim_id.as_str())],
            )
            .expect("count occupancy before purge");
        let count = rows
            .first()
            .and_then(|row| row.get(0))
            .and_then(SqliteValue::as_integer)
            .unwrap_or_default();
        assert_eq!(count, 1, "status transition must seed occupancy evidence");
    }

    let purge = run_br(
        &workspace,
        ["delete", &victim_id, "--force", "--hard"],
        "hard_delete_occupied_victim",
    );
    assert!(
        purge.status.success(),
        "hard delete failed: {}",
        purge.stderr
    );

    {
        let conn = Connection::open(db_path.to_string_lossy().into_owned())
            .expect("open database after purge");
        let issue_rows = conn
            .query_with_params(
                "SELECT COUNT(*) FROM issues WHERE id = ?",
                &[SqliteValue::from(victim_id.as_str())],
            )
            .expect("count issue after purge");
        let occupancy_rows = conn
            .query_with_params(
                "SELECT COUNT(*) FROM capacity_occupancy WHERE issue_id = ?",
                &[SqliteValue::from(victim_id.as_str())],
            )
            .expect("count occupancy after purge");
        let issue_count = issue_rows
            .first()
            .and_then(|row| row.get(0))
            .and_then(SqliteValue::as_integer)
            .unwrap_or_default();
        let occupancy_count = occupancy_rows
            .first()
            .and_then(|row| row.get(0))
            .and_then(SqliteValue::as_integer)
            .unwrap_or_default();
        assert_eq!(issue_count, 0, "purged issue row survived");
        assert_eq!(occupancy_count, 0, "purged capacity occupancy row survived");

        let foreign_key_violations = conn
            .query("PRAGMA foreign_key_check")
            .expect("check foreign keys after purge");
        assert!(
            foreign_key_violations.is_empty(),
            "hard delete left foreign-key violations: {foreign_key_violations:?}"
        );
    }

    let reconcile = run_br(
        &workspace,
        ["sync", "--reconcile-additive", "--robot"],
        "reconcile_after_hard_delete",
    );
    assert!(
        reconcile.status.success(),
        "additive reconcile health preflight failed after hard delete: stdout={} stderr={}",
        reconcile.stdout,
        reconcile.stderr
    );
}
