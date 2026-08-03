use super::common::cli::run_br;
use super::{
    compose_invocation, create_issue, git_commit, git_init, init_workspace, normalize_json,
};
use insta::{assert_json_snapshot, assert_snapshot};
use serde_json::Value;

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
    assert_json_snapshot!("list_json_output", normalize_json(&json));
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
    assert_json_snapshot!("show_json_output", normalize_json(&json));
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
    assert_json_snapshot!("blocked_json_output", normalize_json(&json));
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
    assert_json_snapshot!("list_filtered_json_output", normalize_json(&json));
}

#[test]
fn snapshot_stats_json() {
    let workspace = init_workspace();
    create_issue(&workspace, "Stats Issue", "create_stats");

    let output = run_br(&workspace, ["stats", "--json"], "stats_json");
    assert!(output.status.success());
    // Parse the JSON string into Value before passing to normalize_json
    let json: serde_json::Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("stats_json_output", normalize_json(&json));
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
    assert_json_snapshot!("create_json_output", normalize_json(&json));
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
    assert_json_snapshot!("update_json_output", normalize_json(&json));
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
    assert_json_snapshot!("close_json_output", normalize_json(&json));
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
    assert_json_snapshot!("dep_list_json_output", normalize_json(&json));
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
    assert_json_snapshot!("search_json_output", normalize_json(&json));
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
    assert_json_snapshot!("count_json_output", normalize_json(&json));
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
    assert_json_snapshot!("count_grouped_json_output", normalize_json(&json));
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
    assert_json_snapshot!("stale_json_output", normalize_json(&json));
}

// NOTE: `snapshot_comments_json` and `snapshot_label_json` were removed.
// The `comments` subcommand was deleted from the CLI entirely (no
// replacement), and `label` (add/list/list-all/rename) likewise has no
// surviving CLI surface, so there is nothing left to snapshot for either.

/// `br orphans` finds issues that a commit claims to have addressed but that
/// are still open.
///
/// This test used to run on a bare `init` and record `[]`. That value was
/// produced by a guard clause — `orphans` returns early when the database
/// holds no issue prefixes (`src/cli/commands/orphans.rs`), which is one of
/// SIX early `output_empty` returns ahead of the scan. The recorded value
/// therefore proved that the third guard worked, and said nothing whatever
/// about orphan detection: an `orphans` that never scanned git, never
/// matched an ID, or returned `[]` unconditionally passed it.
///
/// The fixture now builds the situation the command exists to detect: a git
/// repository, an issue, and a commit whose message references that issue by
/// ID while the issue is still open. The recorded value is the orphan.
#[test]
fn snapshot_orphans_json() {
    let workspace = init_workspace();
    let id = create_issue(
        &workspace,
        "Issue left open after a commit",
        "create_orphan",
    );

    git_init(&workspace);
    git_commit(&workspace, &format!("fix({id}): close the login hole"));

    let output = run_br(&workspace, ["orphans", "--json"], "orphans_json");
    assert!(
        output.status.success(),
        "orphans json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    // The point of the fixture: a non-empty result. Asserted separately from
    // the snapshot so that a regression to `[]` fails with the reason rather
    // than as an opaque diff — and so re-recording cannot quietly restore the
    // vacuous value this test used to hold.
    assert_eq!(
        json.as_array().map(Vec::len),
        Some(1),
        "orphan detection returned nothing for an open issue referenced by a \
         commit; the snapshot below would otherwise re-record as `[]` and be \
         a passing test of nothing"
    );

    assert_json_snapshot!("orphans_json_output", normalize_json(&json));
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
    assert_json_snapshot!("graph_json_output", normalize_json(&json));
}

// ============================================================================
// Edge Cases: Empty Results
// ============================================================================

// Every test below records an EMPTY result. An empty expected value is the
// weakest assertion a snapshot can make: `[]` is what the command prints
// when it is working AND what it prints when it is completely broken, so
// recording it on a workspace that contains nothing tests nothing. Four of
// these five ran on a bare `init` — no issues at all — so they did not test
// empty RESULTS, they tested an empty DATABASE, and each command reaches
// that answer through an early return long before the logic the test is
// named for.
//
// The convention here, applied to all of them: the empty answer must be
// EARNED. Each test now populates the workspace, proves with a live control
// assertion that the same command returns something in that same workspace,
// and only then asks the query whose correct answer is empty. `[]` then
// means "the filter ran and correctly excluded everything" — a fact a
// broken command cannot produce.

#[test]
fn snapshot_list_empty_json() {
    // The one case where an empty workspace is the point: this is what a
    // freshly-initialized project sees. No fixture can make the value
    // non-trivial, so the value is composed instead — exit status, stdout
    // and stderr, each labelled — pinning `[]` as the stdout of a command
    // that ran to completion rather than as a bare two-byte file.
    let workspace = init_workspace();

    let output = run_br(&workspace, ["list", "--json"], "list_empty_json");
    assert!(
        output.status.success(),
        "list empty json failed: {}",
        output.stderr
    );
    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_eq!(json, serde_json::json!([]), "fresh workspace lists nothing");

    assert_snapshot!(
        "list_empty_json_output",
        compose_invocation(
            "br list --json",
            &output.stdout,
            &output.stderr,
            output.status
        )
    );
}

#[test]
fn snapshot_blocked_empty_json() {
    // Earn the empty answer: a real dependency exists, and it stops
    // blocking only because the blocker was closed. `[]` here means the
    // blocked query re-evaluated on closure. Recorded on a bare workspace it
    // meant nothing — a `blocked` that always returned `[]` also passed.
    let workspace = init_workspace();
    let blocker = create_issue(&workspace, "Blocker to be closed", "create_blocker_empty");
    let blocked = create_issue(&workspace, "Blocked until then", "create_blocked_empty");
    let dep = run_br(
        &workspace,
        ["dep", "add", &blocked, &blocker],
        "dep_add_blocked_empty",
    );
    assert!(dep.status.success(), "dep add failed: {}", dep.stderr);

    // Live control: with the blocker open, this command DOES return the
    // blocked issue. Without it, an always-empty `blocked` passes below.
    let before = run_br(&workspace, ["blocked", "--json"], "blocked_before_close");
    let before_json: Value = serde_json::from_str(&before.stdout).expect("parse json");
    assert_eq!(
        before_json.as_array().map(Vec::len),
        Some(1),
        "control failed: `blocked` must report the blocked issue while the \
         blocker is open, otherwise the empty result below proves nothing"
    );

    let close = run_br(&workspace, ["close", &blocker], "close_blocker_empty");
    assert!(close.status.success(), "close failed: {}", close.stderr);

    let output = run_br(&workspace, ["blocked", "--json"], "blocked_empty_json");
    assert!(
        output.status.success(),
        "blocked empty json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("blocked_empty_json_output", normalize_json(&json));
}

#[test]
fn snapshot_search_no_match_json() {
    let workspace = init_workspace();
    create_issue(&workspace, "Existing issue", "create_for_search_miss");
    create_issue(&workspace, "Another matchable issue", "create_search_miss2");

    // Live control: search finds a term that IS present, in this same
    // workspace, moments before the miss below. A search returning `[]`
    // unconditionally — broken index, broken query, broken serialization —
    // satisfied this test's snapshot without it.
    let hit = run_br(&workspace, ["search", "Existing", "--json"], "search_hit");
    let hit_json: Value = serde_json::from_str(&hit.stdout).expect("parse json");
    assert_eq!(
        hit_json.as_array().map(Vec::len),
        Some(1),
        "control failed: search must find a term that exists, otherwise the \
         empty result below proves nothing"
    );

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
    assert_json_snapshot!("search_no_match_json_output", normalize_json(&json));
}

#[test]
fn snapshot_stale_empty_json() {
    // Earn the empty answer: an issue exists and is fresh, so a staleness
    // window of a year excludes it. On a bare workspace the `[]` came from
    // there being nothing to age, which is not what `stale` does.
    let workspace = init_workspace();
    create_issue(&workspace, "Freshly created issue", "create_not_stale");

    // Live control: the same command with a zero-day window returns it.
    let control = run_br(
        &workspace,
        ["stale", "--days", "0", "--json"],
        "stale_control",
    );
    let control_json: Value = serde_json::from_str(&control.stdout).expect("parse json");
    assert_eq!(
        control_json.as_array().map(Vec::len),
        Some(1),
        "control failed: `stale --days 0` must report the issue, otherwise \
         the empty result below proves nothing"
    );

    let output = run_br(
        &workspace,
        ["stale", "--days", "365", "--json"],
        "stale_empty_json",
    );
    assert!(
        output.status.success(),
        "stale empty json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("stale_empty_json_output", normalize_json(&json));
}

#[test]
fn snapshot_count_empty_json() {
    // Earn the zero: issues exist, and the count is zero because the status
    // filter excludes them all. On a bare workspace, `{"count": 0}` was
    // equally the output of a `count` that had stopped counting.
    let workspace = init_workspace();
    create_issue(&workspace, "Open issue one", "create_count_empty_one");
    create_issue(&workspace, "Open issue two", "create_count_empty_two");

    // Live control: unfiltered, the same command counts both.
    let control = run_br(&workspace, ["count", "--json"], "count_control");
    let control_json: Value = serde_json::from_str(&control.stdout).expect("parse json");
    assert_eq!(
        control_json["count"], 2,
        "control failed: `count` must see both issues, otherwise the zero \
         below proves nothing"
    );

    let output = run_br(
        &workspace,
        ["count", "--status", "closed", "--json"],
        "count_empty_json",
    );
    assert!(
        output.status.success(),
        "count empty json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    assert_json_snapshot!("count_empty_json_output", normalize_json(&json));
}

// ============================================================================
// Ordering Guarantees
// ============================================================================

#[test]
fn snapshot_list_priority_ordering_json() {
    // `bd list`'s bare default (no --sort) is newest-first
    // (`created_at DESC`), not priority order — priority ordering is
    // reachable via `--sort priority`. This test's whole point is
    // verifying priority ordering, so it asks for that explicitly
    // rather than relying on the default (which used to be
    // priority-first, but isn't anymore).
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

    let output = run_br(
        &workspace,
        ["list", "--sort", "priority", "--json"],
        "list_priority_order_json",
    );
    assert!(
        output.status.success(),
        "list priority ordering json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    let normalized = normalize_json(&json);
    assert_json_snapshot!("list_priority_ordering_json_output", normalized);

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
fn snapshot_list_default_ordering_is_newest_first_json() {
    // Bare `bd list --json` (no --sort): newest-first by created_at,
    // with id as a deterministic tiebreak. Priorities are assigned in
    // the *opposite* order from creation here specifically to prove
    // priority plays no part: if the default were still
    // priority-first, this would come out sorted by priority instead
    // of reversed-creation-order, and the assertion below would fail.
    let workspace = init_workspace();

    let id_first = create_issue(&workspace, "Created first", "create_ord_1");
    let id_second = create_issue(&workspace, "Created second", "create_ord_2");
    let id_third = create_issue(&workspace, "Created third", "create_ord_3");

    // Ascending priority in creation order — if this leaked into the
    // default ordering, the id order below would come out unchanged
    // instead of reversed.
    let _ = run_br(
        &workspace,
        ["update", &id_first, "--priority", "0"],
        "set_p0",
    );
    let _ = run_br(
        &workspace,
        ["update", &id_second, "--priority", "2"],
        "set_p2",
    );
    let _ = run_br(
        &workspace,
        ["update", &id_third, "--priority", "4"],
        "set_p4",
    );

    let output = run_br(&workspace, ["list", "--json"], "list_default_order_json");
    assert!(
        output.status.success(),
        "list default ordering json failed: {}",
        output.stderr
    );

    let json: Value = serde_json::from_str(&output.stdout).expect("parse json");
    let ids: Vec<String> = json
        .as_array()
        .expect("array")
        .iter()
        .map(|item| item["id"].as_str().expect("id").to_string())
        .collect();

    assert_eq!(
        ids,
        vec![id_third, id_second, id_first],
        "default `bd list --json` order should be newest-created first, \
         regardless of priority"
    );
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
    assert_json_snapshot!("show_multiple_ids_json_output", normalized);

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
    assert_json_snapshot!("count_grouped_by_type_json_output", normalize_json(&json));
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
        normalize_json(&json)
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
    assert_json_snapshot!("graph_all_json_output", normalize_json(&json));

    // Suppress unused variable warning
    let _ = root2;
}
