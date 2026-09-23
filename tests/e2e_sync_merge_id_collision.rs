//! GitHub #512: two clones of one workspace each add a child to the same
//! parent. Child ids come from per-database counters, so both mint
//! `<parent>.1`. Publishing one clone's ledger over the other's and running
//! `br sync --merge` must keep both children and both parent comments, and
//! the other clone must be able to take the merged ledger back without
//! losing or duplicating anything.

mod common;

use common::cli::{BrRun, BrWorkspace, run_br};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

fn ok(run: BrRun, what: &str) -> BrRun {
    assert!(
        run.status.success(),
        "{what} failed: stdout={} stderr={}",
        run.stdout,
        run.stderr
    );
    run
}

fn jsonl(workspace: &BrWorkspace) -> PathBuf {
    workspace.root.join(".beads").join("issues.jsonl")
}

fn created_id(run: &BrRun) -> String {
    let value: Value = serde_json::from_str(run.stdout.trim()).expect("create JSON");
    value
        .get("id")
        .or_else(|| value.get(0).and_then(|first| first.get("id")))
        .and_then(Value::as_str)
        .expect("created id")
        .to_string()
}

/// id -> title for every non-tombstone issue in the workspace's JSONL.
fn ledger(workspace: &BrWorkspace) -> BTreeMap<String, String> {
    fs::read_to_string(jsonl(workspace))
        .expect("read jsonl")
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str::<Value>(line).expect("jsonl line"))
        .filter(|issue| issue["status"] != "tombstone")
        .map(|issue| {
            (
                issue["id"].as_str().unwrap().to_string(),
                issue["title"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn comment_texts(workspace: &BrWorkspace, id: &str) -> Vec<String> {
    let run = ok(
        run_br(workspace, ["comments", "list", id, "--json"], "comments"),
        "comments list",
    );
    let value: Value = serde_json::from_str(run.stdout.trim()).expect("comments JSON");
    let mut texts = value
        .as_array()
        .expect("comments array")
        .iter()
        .map(|comment| comment["text"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    texts.sort();
    texts
}

fn parent_of(workspace: &BrWorkspace, id: &str) -> String {
    let run = ok(run_br(workspace, ["show", id, "--json"], "show"), "show");
    let value: Value = serde_json::from_str(run.stdout.trim()).expect("show JSON");
    let issue = value.get(0).unwrap_or(&value);
    issue["dependencies"]
        .as_array()
        .expect("dependencies")
        .iter()
        .find(|dep| dep["dependency_type"] == "parent-child" || dep["type"] == "parent-child")
        .and_then(|dep| dep["id"].as_str().or_else(|| dep["depends_on_id"].as_str()))
        .unwrap_or_else(|| panic!("{id} has no parent: {issue}"))
        .to_string()
}

/// Two initialized clones that share one published parent issue.
fn two_clones_sharing_a_parent() -> (BrWorkspace, BrWorkspace, String) {
    let clone_a = BrWorkspace::new();
    let clone_b = BrWorkspace::new();
    for workspace in [&clone_a, &clone_b] {
        ok(run_br(workspace, ["init", "--prefix", "t"], "init"), "init");
    }
    let parent = created_id(&ok(
        run_br(&clone_a, ["create", "parent", "--json"], "create_parent"),
        "create parent",
    ));
    ok(run_br(&clone_a, ["sync", "--flush-only"], "flush"), "flush");
    fs::copy(jsonl(&clone_a), jsonl(&clone_b)).expect("publish base");
    ok(
        run_br(&clone_b, ["sync", "--import-only"], "import_base"),
        "import base",
    );
    (clone_a, clone_b, parent)
}

/// Add a child (and optionally a parent comment) in one clone and publish it.
fn add_child(workspace: &BrWorkspace, parent: &str, title: &str, comment: Option<&str>) -> String {
    let child = created_id(&ok(
        run_br(
            workspace,
            ["create", title, "--parent", parent, "--json"],
            "create_child",
        ),
        "create child",
    ));
    if let Some(comment) = comment {
        ok(
            run_br(workspace, ["comments", "add", parent, comment], "comment"),
            "comment",
        );
    }
    ok(
        run_br(workspace, ["sync", "--flush-only"], "flush"),
        "flush",
    );
    child
}

#[test]
fn e2e_sync_merge_keeps_both_clones_children_that_share_an_id() {
    let _log = common::test_log("e2e_sync_merge_keeps_both_clones_children_that_share_an_id");
    let (clone_a, clone_b, parent) = two_clones_sharing_a_parent();
    let child_a = add_child(&clone_a, &parent, "child from A", Some("comment from A"));
    let child_b = add_child(&clone_b, &parent, "child from B", Some("comment from B"));
    assert_eq!(child_a, child_b, "both clones mint the same child id");

    // A takes B's ledger (e.g. resolving a git conflict with the incoming
    // side) and merges it into its database.
    fs::copy(jsonl(&clone_b), jsonl(&clone_a)).expect("take B's ledger");
    let merge = ok(
        run_br(&clone_a, ["--json", "sync", "--merge"], "merge_a"),
        "merge in A",
    );
    let report: Value = serde_json::from_str(merge.stdout.trim()).expect("merge JSON");
    let collisions = report["id_collisions"].as_array().expect("id_collisions");
    assert_eq!(collisions.len(), 1, "{report}");
    assert_eq!(collisions[0]["id"], child_a.as_str());
    let relocated = collisions[0]["relocated_id"].as_str().unwrap().to_string();
    assert_eq!(relocated, format!("{parent}.2"));

    let expected = BTreeMap::from([
        (parent.clone(), "parent".to_string()),
        (child_a.clone(), "child from A".to_string()),
        (relocated.clone(), "child from B".to_string()),
    ]);
    assert_eq!(ledger(&clone_a), expected);
    assert_eq!(
        comment_texts(&clone_a, &parent),
        ["comment from A", "comment from B"]
    );
    assert_eq!(parent_of(&clone_a, &relocated), parent);

    // The next child minted in A does not reuse the relocated id.
    let next = add_child(&clone_a, &parent, "another child", None);
    assert_eq!(next, format!("{parent}.3"));

    // B takes the merged ledger back. Its own child now lives at `.2` in the
    // ledger; importing must not keep the stale local `.1` or fold `.2` back.
    fs::copy(jsonl(&clone_a), jsonl(&clone_b)).expect("publish merged ledger");
    ok(
        run_br(&clone_b, ["sync", "--import-only"], "import_b1"),
        "import merged ledger in B",
    );
    ok(
        run_br(&clone_b, ["sync", "--flush-only"], "flush_b2"),
        "flush B",
    );
    let mut expected_b = expected;
    expected_b.insert(next, "another child".to_string());
    assert_eq!(ledger(&clone_b), expected_b);
    assert_eq!(
        comment_texts(&clone_b, &parent),
        ["comment from A", "comment from B"]
    );
}

#[test]
fn e2e_import_refuses_a_ledger_that_would_drop_a_local_child() {
    let _log = common::test_log("e2e_import_refuses_a_ledger_that_would_drop_a_local_child");
    let (clone_a, clone_b, parent) = two_clones_sharing_a_parent();
    add_child(&clone_a, &parent, "child from A", None);
    add_child(&clone_b, &parent, "child from B", None);

    fs::copy(jsonl(&clone_b), jsonl(&clone_a)).expect("take B's ledger");
    let import = run_br(&clone_a, ["sync", "--import-only"], "import_a");
    assert!(
        !import.status.success(),
        "import must refuse: stdout={} stderr={}",
        import.stdout,
        import.stderr
    );
    assert!(
        import.stderr.contains("ID collision") && import.stderr.contains("br sync --merge"),
        "stderr={}",
        import.stderr
    );

    // The suggested merge keeps both children.
    ok(run_br(&clone_a, ["sync", "--merge"], "merge_a"), "merge");
    let titles = ledger(&clone_a).into_values().collect::<Vec<_>>();
    assert!(titles.contains(&"child from A".to_string()), "{titles:?}");
    assert!(titles.contains(&"child from B".to_string()), "{titles:?}");
}
