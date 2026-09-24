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

/// Ids an issue depends on, as `br show --json` reports them.
fn dependency_targets(workspace: &BrWorkspace, id: &str) -> Vec<String> {
    let run = ok(
        run_br(workspace, ["show", id, "--json"], "show_deps"),
        "show",
    );
    let value: Value = serde_json::from_str(run.stdout.trim()).expect("show JSON");
    let issue = value.get(0).unwrap_or(&value);
    issue["dependencies"]
        .as_array()
        .expect("dependencies")
        .iter()
        .map(|dep| {
            dep["id"]
                .as_str()
                .or_else(|| dep["depends_on_id"].as_str())
                .expect("dependency id")
                .to_string()
        })
        .collect()
}

/// `target` merges `source`'s ledger and ends with the same ledger; merging
/// it once more is a no-op.
fn assert_takes_back_and_converges(target: &BrWorkspace, source: &BrWorkspace) {
    fs::copy(jsonl(source), jsonl(target)).expect("publish merged");
    ok(
        run_br(target, ["sync", "--merge"], "merge_back"),
        "merge back",
    );
    assert_eq!(ledger(target), ledger(source));
    // Idempotent: merging the same ledger again changes nothing.
    let before = fs::read_to_string(jsonl(target)).unwrap();
    let again = ok(
        run_br(target, ["--json", "sync", "--merge"], "merge_back_again"),
        "merge again",
    );
    let again: Value = serde_json::from_str(again.stdout.trim()).unwrap();
    assert_eq!(
        again["id_collisions"].as_array().map(Vec::len),
        Some(0),
        "{again}"
    );
    assert_eq!(fs::read_to_string(jsonl(target)).unwrap(), before);
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

/// The clone whose own child loses the id runs the merge: its child, with
/// its labels, comments and incoming references, moves to the new id.
#[test]
fn e2e_sync_merge_relocates_the_local_child_with_its_relations() {
    let _log = common::test_log("e2e_sync_merge_relocates_the_local_child_with_its_relations");
    let (clone_a, clone_b, parent) = two_clones_sharing_a_parent();
    let child_a = add_child(&clone_a, &parent, "child from A", Some("comment from A"));
    ok(
        run_br(
            &clone_a,
            ["comments", "add", &child_a, "note on A child"],
            "comment_a_child",
        ),
        "comment A child",
    );
    ok(
        run_br(&clone_a, ["sync", "--flush-only"], "flush_a"),
        "flush A",
    );

    let child_b = add_child(&clone_b, &parent, "child from B", Some("comment from B"));
    assert_eq!(child_a, child_b);
    ok(
        run_br(
            &clone_b,
            ["comments", "add", &child_b, "note on B child"],
            "comment_b_child",
        ),
        "comment B child",
    );
    ok(
        run_br(
            &clone_b,
            ["label", "add", &child_b, "b-label"],
            "label_b_child",
        ),
        "label B child",
    );
    ok(
        run_br(&clone_b, ["sync", "--flush-only"], "flush_b"),
        "flush B",
    );
    // An issue B has not published yet references B's child.
    let blocker = created_id(&ok(
        run_br(
            &clone_b,
            ["create", "blocked by B child", "--json"],
            "create_blocker",
        ),
        "create blocked",
    ));
    ok(
        run_br(&clone_b, ["dep", "add", &blocker, &child_b], "dep_b"),
        "dep add",
    );

    // B takes A's ledger and merges: B's child (created later) is relocated.
    fs::copy(jsonl(&clone_a), jsonl(&clone_b)).expect("take A's ledger");
    let merge = ok(
        run_br(&clone_b, ["--json", "sync", "--merge"], "merge_b"),
        "merge in B",
    );
    let report: Value = serde_json::from_str(merge.stdout.trim()).expect("merge JSON");
    let relocated = report["id_collisions"][0]["relocated_id"]
        .as_str()
        .expect("relocated id")
        .to_string();
    assert_eq!(relocated, format!("{parent}.2"));

    let titles = ledger(&clone_b);
    assert_eq!(titles[&child_a], "child from A");
    assert_eq!(titles[&relocated], "child from B");
    assert_eq!(comment_texts(&clone_b, &child_a), ["note on A child"]);
    assert_eq!(comment_texts(&clone_b, &relocated), ["note on B child"]);
    assert_eq!(
        comment_texts(&clone_b, &parent),
        ["comment from A", "comment from B"]
    );
    assert_eq!(parent_of(&clone_b, &relocated), parent);
    assert_eq!(parent_of(&clone_b, &child_a), parent);

    let show = ok(
        run_br(&clone_b, ["show", &relocated, "--json"], "show_relocated"),
        "show relocated",
    );
    assert!(show.stdout.contains("b-label"), "{}", show.stdout);
    let show_a = ok(
        run_br(&clone_b, ["show", &child_a, "--json"], "show_a_child"),
        "show A child",
    );
    assert!(!show_a.stdout.contains("b-label"), "{}", show_a.stdout);
    assert_eq!(dependency_targets(&clone_b, &blocker), vec![relocated]);

    // A takes B's merged ledger back and converges on the same ids.
    assert_takes_back_and_converges(&clone_a, &clone_b);
}

/// An empty issues.jsonl (a truncated file, an empty checkout) must not make
/// `br sync --merge` delete every issue: deletions leave tombstones, so an
/// empty ledger is refused unless the deletion is explicitly accepted.
#[test]
fn e2e_sync_merge_refuses_an_empty_jsonl_that_would_delete_everything() {
    let _log = common::test_log("e2e_sync_merge_refuses_an_empty_jsonl");
    let workspace = BrWorkspace::new();
    ok(
        run_br(&workspace, ["init", "--prefix", "t"], "init"),
        "init",
    );
    for title in ["one", "two"] {
        ok(run_br(&workspace, ["create", title], "create"), "create");
    }
    ok(
        run_br(&workspace, ["sync", "--flush-only"], "flush"),
        "flush",
    );
    let published = ledger(&workspace);
    assert_eq!(published.len(), 2);
    fs::write(jsonl(&workspace), "").expect("truncate");

    for strategy in [None, Some("--force-db"), Some("--force")] {
        let mut args = vec!["sync", "--merge"];
        args.extend(strategy);
        let merge = run_br(&workspace, &args, "merge_empty");
        assert!(
            !merge.status.success(),
            "{args:?} must refuse: stdout={} stderr={}",
            merge.stdout,
            merge.stderr
        );
        assert!(
            merge.stderr.contains("contains no issues"),
            "stderr={}",
            merge.stderr
        );
    }

    // Nothing was deleted, and the suggested flush restores the ledger.
    ok(
        run_br(&workspace, ["sync", "--flush-only", "--force"], "reflush"),
        "reflush",
    );
    assert_eq!(ledger(&workspace), published);

    // Accepting the deletion explicitly still works.
    fs::write(jsonl(&workspace), "").expect("truncate again");
    ok(
        run_br(
            &workspace,
            ["sync", "--merge", "--force-jsonl"],
            "merge_forced",
        ),
        "forced merge",
    );
    let list = ok(
        run_br(&workspace, ["list", "--json"], "list_after_forced"),
        "list",
    );
    assert!(!list.stdout.contains("\"one\""), "{}", list.stdout);
}
