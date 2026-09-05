use super::common::cli::{BrRun, run_br, run_br_with_env};
use super::{SnapshotJson, create_issue, init_workspace, normalize_json};
use insta::{assert_json_snapshot, assert_snapshot};
use serde_json::Value;
use std::fs;

fn strict_success_json(run: &BrRun) -> Value {
    assert!(run.status.success(), "structured command failed: {run:?}");
    assert!(
        !run.stdout.contains('\u{1b}'),
        "structured stdout contains terminal escapes: {run:?}"
    );
    serde_json::from_str(&run.stdout).unwrap_or_else(|error| {
        panic!("successful stdout must be exactly one JSON value: {error}; {run:?}")
    })
}

#[test]
#[cfg(not(feature = "mcp"))]
fn strict_json_default_build_explicitly_reports_mcp_unavailable() {
    let workspace = init_workspace();
    let capabilities = strict_success_json(&run_br(
        &workspace,
        ["capabilities", "--format", "json"],
        "default_build_capabilities",
    ));
    assert_eq!(capabilities["build_features"]["mcp"], false);
    let serve = run_br(&workspace, ["serve", "--help"], "default_build_no_serve");
    assert_eq!(serve.status.code(), Some(2), "{serve:?}");
    assert!(serve.stdout.is_empty(), "{serve:?}");
    assert!(
        serve.stderr.contains("unrecognized subcommand 'serve'"),
        "{serve:?}"
    );
}

#[test]
fn strict_json_command_families_honor_mode_precedence() {
    for (mode, flags, environment) in [
        ("explicit", vec!["--json"], vec![]),
        ("environment", vec![], vec![("BR_OUTPUT_FORMAT", "json")]),
        (
            "explicit_over_quiet_toon_dumb_terminal",
            vec!["--json", "--quiet", "--no-color"],
            vec![("BR_OUTPUT_FORMAT", "toon"), ("TERM", "dumb")],
        ),
    ] {
        let workspace = init_workspace();
        let invoke = |args: &[&str], step: &str| {
            let run = run_br_with_env(
                &workspace,
                args.iter().copied().chain(flags.iter().copied()),
                environment.iter().copied(),
                &format!("strict_json_{mode}_{step}"),
            );
            strict_success_json(&run)
        };
        let empty = invoke(&["list"], "empty_list");
        assert_eq!(empty["issues"], serde_json::json!([]));
        assert_eq!(empty["total"], 0);

        let captured = invoke(&["q", "Strict capture"], "capture");
        assert_eq!(captured["title"], "Strict capture");
        let id = captured["id"].as_str().expect("capture id");
        assert_eq!(
            captured,
            serde_json::json!({"id": id, "title": "Strict capture"})
        );
        let updated = invoke(&["update", id, "--notes", "Contract note"], "update");
        assert_eq!(updated.as_array().expect("update array").len(), 1);
        assert_eq!(updated[0]["id"], id);
        assert_eq!(updated[0]["status"], "open");

        let listed = invoke(&["list"], "list");
        assert_eq!(listed["total"], 1);
        assert_eq!(listed["issues"].as_array().expect("issues").len(), 1);
        assert_eq!(listed["issues"][0]["id"], id);
        assert_eq!(invoke(&["count"], "count"), serde_json::json!({"count": 1}));
        assert_eq!(
            invoke(&["config", "get", "issue_prefix"], "config"),
            serde_json::json!({"key": "issue_prefix", "value": "bd"})
        );
        let shown = invoke(&["show", id], "show");
        assert_eq!(shown.as_array().expect("show array").len(), 1);
        assert_eq!(shown[0]["notes"], "Contract note");
    }
}

#[test]
fn strict_json_partial_close_stream_matches_persisted_state() {
    let workspace = init_workspace();
    let blocker = create_issue(&workspace, "Blocking root", "partial_blocker");
    let blocked = create_issue(&workspace, "Blocked work", "partial_blocked");
    let free = create_issue(&workspace, "Free work", "partial_free");
    let dep = run_br(
        &workspace,
        ["dep", "add", &blocked, &blocker, "--json"],
        "partial_dep",
    );
    strict_success_json(&dep);

    let close = run_br(
        &workspace,
        ["close", &blocked, &free, "--json"],
        "partial_close_json",
    );
    assert_eq!(close.status.code(), Some(3), "partial close: {close:?}");
    assert!(
        !close.stdout.contains('\u{1b}'),
        "ANSI in error stream: {close:?}"
    );
    let documents: Vec<Value> = serde_json::Deserializer::from_str(&close.stdout)
        .into_iter()
        .collect::<Result<_, _>>()
        .unwrap_or_else(|error| panic!("complete error stream must parse: {error}; {close:?}"));
    assert_eq!(
        documents.len(),
        2,
        "partial close must emit payload then error: {close:?}"
    );
    assert_eq!(documents[0]["closed"].as_array().expect("closed").len(), 1);
    assert_eq!(documents[0]["closed"][0]["id"], free);
    assert_eq!(
        documents[0]["skipped"].as_array().expect("skipped").len(),
        1
    );
    assert_eq!(documents[0]["skipped"][0]["id"], blocked);
    assert_eq!(documents[1]["error"]["code"], "CLOSE_INCOMPLETE");
    assert!(serde_json::from_str::<Value>(&close.stdout).is_err());

    let state = strict_success_json(&run_br(
        &workspace,
        ["show", &blocker, &blocked, &free, "--json"],
        "partial_poststate",
    ));
    let issues = state.as_array().expect("show issues");
    assert_eq!(issues.len(), 3);
    for (id, status) in [(&blocker, "open"), (&blocked, "open"), (&free, "closed")] {
        let issue = issues
            .iter()
            .find(|issue| issue["id"] == id.as_str())
            .expect("poststate issue");
        assert_eq!(
            issue["status"], status,
            "unexpected persisted state: {state}"
        );
    }
}

// Representative fixture for exact list/show JSON goldens.
//
// Golden update workflow:
// INSTA_UPDATE=always rch exec -- cargo test --test snapshots representative_json_golden
//
// Review the resulting tests/snapshots/snapshots/*.snap diffs before committing.
// The fixture uses fixed IDs, actors, and timestamps, so these snapshots should
// not require masking; they intentionally lock down JSON field order and
// optional/null omission behavior from the CLI serializer.
const LIST_SHOW_JSONL_FIXTURE: &str = r#"{"id":"bd-golden-parent","title":"01 Parent Epic","description":"Parent description","status":"open","priority":1,"issue_type":"epic","assignee":"alice","owner":"owner@example.com","created_at":"2026-01-01T00:00:00Z","created_by":"fixture","updated_at":"2026-01-01T00:00:00Z","due_at":"2026-07-01T00:00:00Z","source_repo":".","compaction_level":0,"original_size":0,"labels":["ops","ux"]}
{"id":"bd-golden-child","title":"02 Child Task","description":"Child description","design":"Design notes","acceptance_criteria":"Acceptance text","notes":"Operator notes","status":"in_progress","priority":2,"issue_type":"task","assignee":"bob","owner":"child@example.com","created_at":"2026-01-02T00:00:00Z","created_by":"fixture","updated_at":"2026-01-02T01:00:00Z","defer_until":"2026-07-03T00:00:00Z","source_repo":".","compaction_level":0,"original_size":0,"labels":["backend"],"dependencies":[{"issue_id":"bd-golden-child","depends_on_id":"bd-golden-parent","type":"parent-child","created_at":"2026-01-02T00:00:00Z","created_by":"fixture","metadata":"{}","thread_id":""}],"comments":[{"id":1,"issue_id":"bd-golden-child","author":"fixture","text":"first comment","created_at":"2026-01-02T02:00:00Z"}]}
{"id":"bd-golden-closed","title":"03 Closed Bug","description":"Closed bug description","status":"closed","priority":0,"issue_type":"bug","assignee":"carol","owner":"owner@example.com","created_at":"2026-01-03T00:00:00Z","created_by":"fixture","updated_at":"2026-01-03T03:00:00Z","closed_at":"2026-01-03T03:00:00Z","close_reason":"fixed","closed_by_session":"session-1","source_repo":".","compaction_level":0,"original_size":0,"labels":["bugfix"]}
{"id":"bd-golden-deleted","title":"04 Deleted Cleanup","status":"tombstone","priority":3,"issue_type":"task","created_at":"2026-01-04T00:00:00Z","created_by":"fixture","updated_at":"2026-01-04T04:00:00Z","deleted_at":"2026-01-04T04:00:00Z","deleted_by":"fixture","delete_reason":"fixture tombstone","original_type":"task","source_repo":".","compaction_level":0,"original_size":0}
"#;

fn init_list_show_golden_workspace() -> super::common::cli::BrWorkspace {
    let workspace = init_workspace();
    let jsonl_path = workspace.root.join(".beads/issues.jsonl");
    fs::write(jsonl_path, LIST_SHOW_JSONL_FIXTURE).expect("write list/show JSONL fixture");

    let import = run_br(
        &workspace,
        ["sync", "--import-only", "--json"],
        "representative_json_golden_import",
    );
    assert!(
        import.status.success(),
        "fixture import failed:\nstdout:\n{}\nstderr:\n{}",
        import.stdout,
        import.stderr
    );

    workspace
}

#[test]
fn snapshot_list_json() {
    let workspace = init_workspace();
    create_issue(&workspace, "Issue one", "create_one");
    create_issue(&workspace, "Issue two", "create_two");

    let output = run_br(&workspace, ["list", "--json"], "list_json");
    assert!(
        output.status.success(),
        "list json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("list_json_output", SnapshotJson(&normalize_json(&json)));
}

#[test]
fn snapshot_show_json() {
    let workspace = init_workspace();
    let id = create_issue(&workspace, "Detailed issue", "create_detail");

    let output = run_br(&workspace, ["show", &id, "--json"], "show_json");
    assert!(
        output.status.success(),
        "show json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("show_json_output", SnapshotJson(&normalize_json(&json)));
}

#[test]
fn representative_json_golden_list_output() {
    let workspace = init_list_show_golden_workspace();

    let output = run_br(
        &workspace,
        ["list", "--all", "--sort", "title", "--json"],
        "representative_json_golden_list",
    );
    assert!(
        output.status.success(),
        "representative list JSON failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse list JSON");
    assert_eq!(
        json.get("total").and_then(Value::as_u64),
        Some(3),
        "list --all should include open/in_progress/closed and exclude tombstones"
    );
    assert_snapshot!("representative_list_json_output", output.stdout.trim_end());
}

#[test]
fn representative_json_golden_show_output() {
    let workspace = init_list_show_golden_workspace();

    let output = run_br(
        &workspace,
        [
            "show",
            "bd-golden-parent",
            "bd-golden-child",
            "bd-golden-closed",
            "bd-golden-deleted",
            "--json",
        ],
        "representative_json_golden_show",
    );
    assert!(
        output.status.success(),
        "representative show JSON failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse show JSON");
    assert_eq!(
        json.as_array().map(Vec::len),
        Some(4),
        "show should preserve all requested fixture issues, including tombstones"
    );
    assert_snapshot!("representative_show_json_output", output.stdout.trim_end());
}

#[test]
fn snapshot_ready_json() {
    let workspace = init_workspace();
    create_issue(&workspace, "Ready issue", "create_ready");

    let output = run_br(&workspace, ["ready", "--json"], "ready_json");
    assert!(
        output.status.success(),
        "ready json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("ready_json_output", SnapshotJson(&normalize_json(&json)));
}

#[test]
#[allow(clippy::similar_names)]
fn snapshot_blocked_json() {
    let workspace = init_workspace();

    // Create a dependency chain
    let blocker = create_issue(&workspace, "Blocker issue", "create_blocker_json");
    let blocked = create_issue(&workspace, "Blocked issue", "create_blocked_json");

    let _ = run_br(
        &workspace,
        ["dep", "add", &blocked, &blocker],
        "dep_add_json",
    );

    let output = run_br(&workspace, ["blocked", "--json"], "blocked_json");
    assert!(
        output.status.success(),
        "blocked json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("blocked_json_output", SnapshotJson(&normalize_json(&json)));
}

#[test]
fn snapshot_list_with_filters_json() {
    let workspace = init_workspace();
    let id1 = create_issue(&workspace, "Bug: Fix login", "create_bug_json");
    let id2 = create_issue(&workspace, "Feature: Add theme", "create_feature_json");

    // Update types
    let _ = run_br(
        &workspace,
        ["update", &id1, "--type", "bug"],
        "update_bug_json",
    );
    let _ = run_br(
        &workspace,
        ["update", &id2, "--type", "feature"],
        "update_feature_json",
    );

    // List only bugs
    let output = run_br(
        &workspace,
        ["list", "--type", "bug", "--json"],
        "list_bugs_json",
    );
    assert!(
        output.status.success(),
        "list bugs json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!(
        "list_filtered_json_output",
        SnapshotJson(&normalize_json(&json))
    );
}

#[test]
fn snapshot_stats_json() {
    let workspace = init_workspace();
    create_issue(&workspace, "Stats Issue", "create_stats");

    let output = run_br(&workspace, ["stats", "--json"], "stats_json");
    assert!(output.status.success());
    // Parse the JSON string into Value before passing to normalize_json
    let json: serde_json::Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("stats_json_output", SnapshotJson(&normalize_json(&json)));
}

#[test]
fn snapshot_create_json() {
    let workspace = init_workspace();

    let output = run_br(
        &workspace,
        [
            "create",
            "New feature request",
            "--type",
            "feature",
            "--priority",
            "1",
            "--json",
        ],
        "create_json",
    );
    assert!(
        output.status.success(),
        "create json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("create_json_output", SnapshotJson(&normalize_json(&json)));
}

#[test]
fn snapshot_update_json() {
    let workspace = init_workspace();
    let id = create_issue(&workspace, "Issue to update", "create_update");

    let output = run_br(
        &workspace,
        ["update", &id, "--status", "in_progress", "--json"],
        "update_json",
    );
    assert!(
        output.status.success(),
        "update json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("update_json_output", SnapshotJson(&normalize_json(&json)));
}

#[test]
fn snapshot_close_json() {
    let workspace = init_workspace();
    let id = create_issue(&workspace, "Issue to close", "create_close_json");

    let output = run_br(
        &workspace,
        ["close", &id, "--reason", "Done", "--json"],
        "close_json",
    );
    assert!(
        output.status.success(),
        "close json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("close_json_output", SnapshotJson(&normalize_json(&json)));
}

#[test]
fn snapshot_dep_list_json() {
    let workspace = init_workspace();
    let id1 = create_issue(&workspace, "Parent issue", "create_parent");
    let id2 = create_issue(&workspace, "Child issue", "create_child");

    // Add dependency
    let add = run_br(&workspace, ["dep", "add", &id2, &id1], "dep_add");
    assert!(add.status.success(), "dep add failed: {}", add.stderr);

    let output = run_br(&workspace, ["dep", "list", &id2, "--json"], "dep_list_json");
    assert!(
        output.status.success(),
        "dep list json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("dep_list_json_output", SnapshotJson(&normalize_json(&json)));
}

#[test]
fn snapshot_search_json() {
    let workspace = init_workspace();
    create_issue(&workspace, "Search target", "create_search_target");
    create_issue(&workspace, "Other issue", "create_search_other");

    let output = run_br(&workspace, ["search", "target", "--json"], "search_json");
    assert!(
        output.status.success(),
        "search json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("search_json_output", SnapshotJson(&normalize_json(&json)));
}

#[test]
fn snapshot_count_json() {
    let workspace = init_workspace();
    create_issue(&workspace, "Count one", "create_count_one");
    create_issue(&workspace, "Count two", "create_count_two");

    let output = run_br(&workspace, ["count", "--json"], "count_json");
    assert!(
        output.status.success(),
        "count json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("count_json_output", SnapshotJson(&normalize_json(&json)));
}

#[test]
fn snapshot_count_grouped_json() {
    let workspace = init_workspace();
    let id = create_issue(&workspace, "Grouped one", "create_grouped_one");
    let _ = run_br(
        &workspace,
        ["update", &id, "--status", "in_progress"],
        "update_grouped_one",
    );
    create_issue(&workspace, "Grouped two", "create_grouped_two");

    let output = run_br(
        &workspace,
        ["count", "--by", "status", "--json"],
        "count_grouped_json",
    );
    assert!(
        output.status.success(),
        "count grouped json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!(
        "count_grouped_json_output",
        SnapshotJson(&normalize_json(&json))
    );
}

#[test]
fn snapshot_stale_json() {
    let workspace = init_workspace();
    create_issue(&workspace, "Stale issue", "create_stale");

    let output = run_br(&workspace, ["stale", "--days", "0", "--json"], "stale_json");
    assert!(
        output.status.success(),
        "stale json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("stale_json_output", SnapshotJson(&normalize_json(&json)));
}

#[test]
fn snapshot_comments_json() {
    let workspace = init_workspace();
    let id = create_issue(&workspace, "Commented issue", "create_commented");

    let add = run_br(
        &workspace,
        ["comments", "add", &id, "First comment", "--json"],
        "comments_add_json",
    );
    assert!(
        add.status.success(),
        "comments add json failed: {}",
        add.stderr
    );

    let add_json: Value = serde_json::from_str(&add.stdout).expect("parse json");
    assert_json_snapshot!(
        "comments_add_json_output",
        SnapshotJson(&normalize_json(&add_json))
    );

    let list = run_br(
        &workspace,
        ["comments", "list", &id, "--json"],
        "comments_list_json",
    );
    assert!(
        list.status.success(),
        "comments list json failed: {}",
        list.stderr
    );

    let list_json: Value = serde_json::from_str(&list.stdout).expect("parse json");
    assert_json_snapshot!(
        "comments_list_json_output",
        SnapshotJson(&normalize_json(&list_json))
    );
}

#[test]
fn snapshot_label_json() {
    let workspace = init_workspace();
    let id = create_issue(&workspace, "Labeled issue", "create_labeled");

    let add = run_br(
        &workspace,
        ["label", "add", &id, "backend", "--json"],
        "label_add_json",
    );
    assert!(
        add.status.success(),
        "label add json failed: {}",
        add.stderr
    );

    let add_json: Value = serde_json::from_str(&add.stdout).expect("parse json");
    assert_json_snapshot!(
        "label_add_json_output",
        SnapshotJson(&normalize_json(&add_json))
    );

    let list = run_br(
        &workspace,
        ["label", "list", &id, "--json"],
        "label_list_json",
    );
    assert!(
        list.status.success(),
        "label list json failed: {}",
        list.stderr
    );

    let list_json: Value = serde_json::from_str(&list.stdout).expect("parse json");
    assert_json_snapshot!(
        "label_list_json_output",
        SnapshotJson(&normalize_json(&list_json))
    );

    let list_all = run_br(
        &workspace,
        ["label", "list-all", "--json"],
        "label_list_all_json",
    );
    assert!(
        list_all.status.success(),
        "label list-all json failed: {}",
        list_all.stderr
    );

    let list_all_json: Value = serde_json::from_str(&list_all.stdout).expect("parse json");
    assert_json_snapshot!(
        "label_list_all_json_output",
        SnapshotJson(&normalize_json(&list_all_json))
    );
}

#[test]
fn snapshot_orphans_json() {
    let workspace = init_workspace();

    let output = run_br(&workspace, ["orphans", "--json"], "orphans_json");
    assert!(
        output.status.success(),
        "orphans json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("orphans_json_output", SnapshotJson(&normalize_json(&json)));
}

#[test]
fn snapshot_graph_json() {
    let workspace = init_workspace();
    let root = create_issue(&workspace, "Graph root", "create_graph_root");
    let child = create_issue(&workspace, "Graph child", "create_graph_child");

    let _ = run_br(
        &workspace,
        ["dep", "add", &child, &root],
        "graph_dep_add_json",
    );

    let output = run_br(&workspace, ["graph", &root, "--json"], "graph_json");
    assert!(
        output.status.success(),
        "graph json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("graph_json_output", SnapshotJson(&normalize_json(&json)));
}

// ============================================================================
// Edge Cases: Empty Results
// ============================================================================

#[test]
fn snapshot_list_empty_json() {
    let workspace = init_workspace();

    let output = run_br(&workspace, ["list", "--json"], "list_empty_json");
    assert!(
        output.status.success(),
        "list empty json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!(
        "list_empty_json_output",
        SnapshotJson(&normalize_json(&json))
    );
}

#[test]
fn snapshot_ready_empty_json() {
    let workspace = init_workspace();

    let output = run_br(&workspace, ["ready", "--json"], "ready_empty_json");
    assert!(
        output.status.success(),
        "ready empty json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!(
        "ready_empty_json_output",
        SnapshotJson(&normalize_json(&json))
    );
}

#[test]
fn snapshot_blocked_empty_json() {
    let workspace = init_workspace();

    let output = run_br(&workspace, ["blocked", "--json"], "blocked_empty_json");
    assert!(
        output.status.success(),
        "blocked empty json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!(
        "blocked_empty_json_output",
        SnapshotJson(&normalize_json(&json))
    );
}

#[test]
fn snapshot_search_no_match_json() {
    let workspace = init_workspace();
    create_issue(&workspace, "Existing issue", "create_for_search_miss");

    let output = run_br(
        &workspace,
        ["search", "nonexistent_xyz", "--json"],
        "search_no_match_json",
    );
    assert!(
        output.status.success(),
        "search no match json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!(
        "search_no_match_json_output",
        SnapshotJson(&normalize_json(&json))
    );
}

#[test]
fn snapshot_stale_empty_json() {
    let workspace = init_workspace();

    let output = run_br(
        &workspace,
        ["stale", "--days", "0", "--json"],
        "stale_empty_json",
    );
    assert!(
        output.status.success(),
        "stale empty json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!(
        "stale_empty_json_output",
        SnapshotJson(&normalize_json(&json))
    );
}

#[test]
fn snapshot_count_empty_json() {
    let workspace = init_workspace();

    let output = run_br(&workspace, ["count", "--json"], "count_empty_json");
    assert!(
        output.status.success(),
        "count empty json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!(
        "count_empty_json_output",
        SnapshotJson(&normalize_json(&json))
    );
}

// ============================================================================
// Ordering Guarantees
// ============================================================================

#[test]
fn snapshot_list_priority_ordering_json() {
    let workspace = init_workspace();

    // Create issues with different priorities (lower number = higher priority)
    let id_low = create_issue(&workspace, "Low priority task", "create_low_prio");
    let id_high = create_issue(&workspace, "High priority task", "create_high_prio");
    let id_crit = create_issue(&workspace, "Critical task", "create_crit_prio");

    let _ = run_br(
        &workspace,
        ["update", &id_low, "--priority", "3"],
        "set_low_prio",
    );
    let _ = run_br(
        &workspace,
        ["update", &id_high, "--priority", "1"],
        "set_high_prio",
    );
    let _ = run_br(
        &workspace,
        ["update", &id_crit, "--priority", "0"],
        "set_crit_prio",
    );

    let output = run_br(&workspace, ["list", "--json"], "list_priority_order_json");
    assert!(
        output.status.success(),
        "list priority ordering json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    let normalized = normalize_json(&json);
    assert_json_snapshot!(
        "list_priority_ordering_json_output",
        SnapshotJson(&normalized)
    );

    // Also verify ordering programmatically: priorities should be ascending
    if let Value::Array(items) = &json {
        let priorities: Vec<i64> = items
            .iter()
            .filter_map(|item| item.get("priority").and_then(Value::as_i64))
            .collect();
        for window in priorities.windows(2) {
            assert!(
                window[0] <= window[1],
                "list ordering violated: P{} should come before P{}",
                window[0],
                window[1]
            );
        }
    }
}

#[test]
fn snapshot_ready_priority_ordering_json() {
    let workspace = init_workspace();

    // Create multiple ready issues with different priorities
    let id_p3 = create_issue(&workspace, "Backlog ready task", "create_ready_p3");
    let id_p1 = create_issue(&workspace, "Urgent ready task", "create_ready_p1");
    let id_p2 = create_issue(&workspace, "Normal ready task", "create_ready_p2");

    let _ = run_br(
        &workspace,
        ["update", &id_p3, "--priority", "3"],
        "set_ready_p3",
    );
    let _ = run_br(
        &workspace,
        ["update", &id_p1, "--priority", "1"],
        "set_ready_p1",
    );
    let _ = run_br(
        &workspace,
        ["update", &id_p2, "--priority", "2"],
        "set_ready_p2",
    );

    let output = run_br(&workspace, ["ready", "--json"], "ready_priority_order_json");
    assert!(
        output.status.success(),
        "ready priority ordering json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    let normalized = normalize_json(&json);
    assert_json_snapshot!(
        "ready_priority_ordering_json_output",
        SnapshotJson(&normalized)
    );

    // Ready uses hybrid sort: P0/P1 first, then others by created_at ASC.
    // The snapshot locks down the exact ordering. Verify P0/P1 appear before P2+.
    if let Value::Array(items) = &json {
        let priorities: Vec<i64> = items
            .iter()
            .filter_map(|item| item.get("priority").and_then(Value::as_i64))
            .collect();
        let high_prio_end = priorities
            .iter()
            .position(|&p| p > 1)
            .unwrap_or(priorities.len());
        for &p in &priorities[..high_prio_end] {
            assert!(p <= 1, "P0/P1 should appear in the first group, got P{p}");
        }
        for &p in &priorities[high_prio_end..] {
            assert!(p > 1, "P2+ should appear in the second group, got P{p}");
        }
    }
}

// ============================================================================
// Multiple IDs / Complex Scenarios
// ============================================================================

#[test]
fn snapshot_show_multiple_ids_json() {
    let workspace = init_workspace();
    let id1 = create_issue(&workspace, "First detailed issue", "create_multi_1");
    let id2 = create_issue(&workspace, "Second detailed issue", "create_multi_2");

    let output = run_br(
        &workspace,
        ["show", &id1, &id2, "--json"],
        "show_multi_json",
    );
    assert!(
        output.status.success(),
        "show multiple ids json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    let normalized = normalize_json(&json);
    assert_json_snapshot!("show_multiple_ids_json_output", SnapshotJson(&normalized));

    // Verify we got exactly 2 results
    if let Value::Array(items) = &json {
        assert_eq!(items.len(), 2, "show with 2 IDs should return 2 results");
    }
}

#[test]
fn snapshot_count_grouped_by_type_json() {
    let workspace = init_workspace();
    let id1 = create_issue(&workspace, "Bug to fix", "create_typed_bug");
    let id2 = create_issue(&workspace, "Feature to add", "create_typed_feature");
    create_issue(&workspace, "Plain task", "create_typed_task");

    let _ = run_br(
        &workspace,
        ["update", &id1, "--type", "bug"],
        "set_type_bug",
    );
    let _ = run_br(
        &workspace,
        ["update", &id2, "--type", "feature"],
        "set_type_feature",
    );

    let output = run_br(
        &workspace,
        ["count", "--by", "type", "--json"],
        "count_by_type_json",
    );
    assert!(
        output.status.success(),
        "count grouped by type json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!(
        "count_grouped_by_type_json_output",
        SnapshotJson(&normalize_json(&json))
    );
}

#[test]
fn snapshot_count_grouped_by_priority_json() {
    let workspace = init_workspace();
    let id1 = create_issue(&workspace, "Critical item", "create_prio_p0");
    let id2 = create_issue(&workspace, "Normal item", "create_prio_p2");
    create_issue(&workspace, "Default item", "create_prio_default");

    let _ = run_br(
        &workspace,
        ["update", &id1, "--priority", "0"],
        "set_prio_p0",
    );
    let _ = run_br(
        &workspace,
        ["update", &id2, "--priority", "3"],
        "set_prio_p3",
    );

    let output = run_br(
        &workspace,
        ["count", "--by", "priority", "--json"],
        "count_by_priority_json",
    );
    assert!(
        output.status.success(),
        "count grouped by priority json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!(
        "count_grouped_by_priority_json_output",
        SnapshotJson(&normalize_json(&json))
    );
}

#[test]
fn snapshot_graph_all_json() {
    let workspace = init_workspace();
    let root1 = create_issue(&workspace, "Graph root A", "create_graph_root_a");
    let child1 = create_issue(&workspace, "Graph child of A", "create_graph_child_a");
    let root2 = create_issue(&workspace, "Graph root B", "create_graph_root_b");

    let _ = run_br(
        &workspace,
        ["dep", "add", &child1, &root1],
        "graph_all_dep_add",
    );

    // graph --all shows all roots
    let output = run_br(&workspace, ["graph", "--all", "--json"], "graph_all_json");
    assert!(
        output.status.success(),
        "graph all json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!(
        "graph_all_json_output",
        SnapshotJson(&normalize_json(&json))
    );

    // Suppress unused variable warning
    let _ = root2;
}
