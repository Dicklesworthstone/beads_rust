//! Model-based differential test for `SqliteStorage` (bead beads_rust-dk45.7).
//!
//! Every other test trusts the engine it runs on. This one replays random
//! sequences of issue operations against a file-backed `SqliteStorage` and
//! against a tiny engine-free reference model (`BTreeMap`s), and after every
//! operation checks that what the storage projects (issue fields, labels,
//! comments, dependency edges, the listing) equals what the model says. The
//! August 2026 corruption class (GitHub #457/#460/#461) and the GitHub #426
//! rowid-order bug (264 sequential dependency removals) would both have
//! produced a projection mismatch here. `PRAGMA integrity_check` must be
//! `ok` at the end of every case.
//!
//! Operations that the storage rejects by design (self-dependencies, edges
//! that would create a cycle, duplicate edges) are never generated: indices
//! in an `Op` are resolved against the live issues at execution time, so a
//! shrunken failing sequence is always a valid sequence. Set
//! `BR_MODEL_CASES` to run more cases than the default.
mod common;

use beads_rust::franken_sync::Connection;
use beads_rust::model::{Issue, IssueType, Priority, Status};
use beads_rust::storage::{IssueUpdate, ListFilters, SqliteStorage};
use chrono::Utc;
use fsqlite_types::SqliteValue;
use proptest::prelude::*;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use tempfile::TempDir;

const ACTOR: &str = "model";
const KINDS: [IssueType; 4] = [
    IssueType::Task,
    IssueType::Bug,
    IssueType::Feature,
    IssueType::Epic,
];
const ACTIVE_STATUSES: [Status; 4] = [
    Status::Open,
    Status::InProgress,
    Status::Blocked,
    Status::Deferred,
];
const DEP_TYPES: [&str; 3] = ["blocks", "related", "parent-child"];

/// One generated operation. Issue references are indices into the sorted
/// list of live (non-tombstoned) issues at the moment the op runs, taken
/// modulo the list length; an op with no live issue to act on is a no-op.
#[derive(Debug, Clone)]
enum Op {
    Create {
        title: String,
        priority: i32,
        kind: usize,
    },
    Retitle {
        target: usize,
        title: String,
    },
    SetStatus {
        target: usize,
        status: usize,
    },
    Close {
        target: usize,
    },
    Assign {
        target: usize,
        assignee: Option<String>,
    },
    LabelAdd {
        target: usize,
        label: String,
    },
    LabelRemove {
        target: usize,
        which: usize,
    },
    DepAdd {
        from: usize,
        to: usize,
        kind: usize,
    },
    DepRemove {
        target: usize,
        which: usize,
    },
    CommentAdd {
        target: usize,
        text: String,
    },
    Delete {
        target: usize,
    },
}

fn op_strategy() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => ("[a-z][a-z ]{0,23}", 0..=4_i32, 0..KINDS.len()).prop_map(|(title, priority, kind)| Op::Create { title, priority, kind }),
        2 => (any::<usize>(), "[a-z][a-z ]{0,23}").prop_map(|(target, title)| Op::Retitle { target, title }),
        2 => (any::<usize>(), 0..ACTIVE_STATUSES.len()).prop_map(|(target, status)| Op::SetStatus { target, status }),
        1 => any::<usize>().prop_map(|target| Op::Close { target }),
        1 => (any::<usize>(), proptest::option::of("[a-z]{2,6}")).prop_map(|(target, assignee)| Op::Assign { target, assignee }),
        2 => (any::<usize>(), "[a-z]{1,5}").prop_map(|(target, label)| Op::LabelAdd { target, label }),
        1 => (any::<usize>(), any::<usize>()).prop_map(|(target, which)| Op::LabelRemove { target, which }),
        3 => (any::<usize>(), any::<usize>(), 0..DEP_TYPES.len()).prop_map(|(from, to, kind)| Op::DepAdd { from, to, kind }),
        2 => (any::<usize>(), any::<usize>()).prop_map(|(target, which)| Op::DepRemove { target, which }),
        2 => (any::<usize>(), "[a-z][a-z ]{0,29}").prop_map(|(target, text)| Op::CommentAdd { target, text }),
        1 => any::<usize>().prop_map(|target| Op::Delete { target }),
    ]
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ModelIssue {
    title: String,
    status: Status,
    priority: i32,
    kind: IssueType,
    assignee: Option<String>,
    labels: BTreeSet<String>,
    comments: Vec<String>,
    deleted: bool,
}

/// The engine-free reference: what the observable state must be.
#[derive(Debug, Default)]
struct Model {
    issues: BTreeMap<String, ModelIssue>,
    /// (issue, depends_on, dep_type)
    deps: BTreeSet<(String, String, String)>,
    next: usize,
}

impl Model {
    fn live_ids(&self) -> Vec<String> {
        self.issues
            .iter()
            .filter(|(_, issue)| !issue.deleted)
            .map(|(id, _)| id.clone())
            .collect()
    }

    fn pick(&self, index: usize) -> Option<String> {
        let live = self.live_ids();
        if live.is_empty() {
            None
        } else {
            Some(live[index % live.len()].clone())
        }
    }

    fn outgoing(&self, id: &str) -> Vec<(String, String)> {
        self.deps
            .iter()
            .filter(|(from, _, _)| from == id)
            .map(|(_, to, kind)| (to.clone(), kind.clone()))
            .collect()
    }

    fn incoming(&self, id: &str) -> BTreeSet<String> {
        self.deps
            .iter()
            .filter(|(_, to, _)| to == id)
            .map(|(from, _, _)| from.clone())
            .collect()
    }

    /// The storage's blocker graph, mirrored from `check_cycle` in
    /// `src/storage/sqlite.rs`: a blocking dependency runs from the issue to
    /// what it depends on, and a parent-child edge runs from the parent to
    /// the child because a parent waits for its children. `related` edges
    /// are not blocking and are never followed.
    fn blocker_neighbors(&self, node: &str) -> Vec<String> {
        let mut next = Vec::new();
        for (from, to, kind) in &self.deps {
            if from == node
                && matches!(kind.as_str(), "blocks" | "conditional-blocks" | "waits-for")
            {
                next.push(to.clone());
            }
            if to == node && kind == "parent-child" {
                next.push(from.clone());
            }
        }
        next
    }

    fn reaches_in_blocker_graph(&self, start: &str, goal: &str) -> bool {
        let mut stack = vec![start.to_string()];
        let mut seen = BTreeSet::new();
        while let Some(node) = stack.pop() {
            if node == goal {
                return true;
            }
            if seen.insert(node.clone()) {
                stack.extend(self.blocker_neighbors(&node));
            }
        }
        false
    }

    /// Whether the storage will refuse `from -> to` of `dep_type` as a
    /// cycle. A non-blocking edge is never cycle-checked. A parent-child
    /// edge (`from` is the child, `to` the parent) adds the blocker edge
    /// parent -> child, so it closes a cycle when the child already reaches
    /// the parent; any other blocking edge closes one when `to` already
    /// reaches `from`.
    fn would_close_blocker_cycle(&self, from: &str, to: &str, dep_type: &str) -> bool {
        match dep_type {
            "related" => false,
            "parent-child" => self.reaches_in_blocker_graph(from, to),
            _ => self.reaches_in_blocker_graph(to, from),
        }
    }

    fn edge_between(&self, a: &str, b: &str) -> bool {
        self.deps
            .iter()
            .any(|(from, to, _)| (from == a && to == b) || (from == b && to == a))
    }
}

fn new_issue(id: &str, title: &str, priority: i32, kind: IssueType) -> Issue {
    let now = Utc::now();
    Issue {
        id: id.to_string(),
        title: title.to_string(),
        status: Status::Open,
        priority: Priority(priority),
        issue_type: kind,
        created_at: now,
        updated_at: now,
        created_by: Some(ACTOR.to_string()),
        ..Default::default()
    }
}

/// Apply one op to both sides. Returns a description for the failure log.
#[allow(clippy::too_many_lines)]
fn apply(op: &Op, storage: &mut SqliteStorage, model: &mut Model) -> String {
    match op {
        Op::Create {
            title,
            priority,
            kind,
        } => {
            let id = format!("mb-{:04}", model.next);
            model.next += 1;
            let kind = KINDS[*kind].clone();
            storage
                .create_issue(&new_issue(&id, title, *priority, kind.clone()), ACTOR)
                .expect("create");
            model.issues.insert(
                id.clone(),
                ModelIssue {
                    title: title.clone(),
                    status: Status::Open,
                    priority: *priority,
                    kind,
                    assignee: None,
                    labels: BTreeSet::new(),
                    comments: Vec::new(),
                    deleted: false,
                },
            );
            format!("create {id}")
        }
        Op::Retitle { target, title } => {
            let Some(id) = model.pick(*target) else {
                return "retitle (no live issue)".to_string();
            };
            storage
                .update_issue(
                    &id,
                    &IssueUpdate {
                        title: Some(title.clone()),
                        ..Default::default()
                    },
                    ACTOR,
                )
                .expect("retitle");
            model
                .issues
                .get_mut(&id)
                .expect("model issue")
                .title
                .clone_from(title);
            format!("retitle {id}")
        }
        Op::SetStatus { target, status } => {
            let Some(id) = model.pick(*target) else {
                return "status (no live issue)".to_string();
            };
            let status = ACTIVE_STATUSES[*status].clone();
            storage
                .update_issue(
                    &id,
                    &IssueUpdate {
                        status: Some(status.clone()),
                        closed_at: Some(None),
                        ..Default::default()
                    },
                    ACTOR,
                )
                .expect("set status");
            model.issues.get_mut(&id).expect("model issue").status = status;
            format!("status {id}")
        }
        Op::Close { target } => {
            let Some(id) = model.pick(*target) else {
                return "close (no live issue)".to_string();
            };
            storage
                .update_issue(
                    &id,
                    &IssueUpdate {
                        status: Some(Status::Closed),
                        closed_at: Some(Some(Utc::now())),
                        close_reason: Some(Some("model".to_string())),
                        ..Default::default()
                    },
                    ACTOR,
                )
                .expect("close");
            model.issues.get_mut(&id).expect("model issue").status = Status::Closed;
            format!("close {id}")
        }
        Op::Assign { target, assignee } => {
            let Some(id) = model.pick(*target) else {
                return "assign (no live issue)".to_string();
            };
            storage
                .update_issue(
                    &id,
                    &IssueUpdate {
                        assignee: Some(assignee.clone()),
                        ..Default::default()
                    },
                    ACTOR,
                )
                .expect("assign");
            model
                .issues
                .get_mut(&id)
                .expect("model issue")
                .assignee
                .clone_from(assignee);
            format!("assign {id}")
        }
        Op::LabelAdd { target, label } => {
            let Some(id) = model.pick(*target) else {
                return "label add (no live issue)".to_string();
            };
            storage.add_label(&id, label, ACTOR).expect("add label");
            model
                .issues
                .get_mut(&id)
                .expect("model issue")
                .labels
                .insert(label.clone());
            format!("label add {id} {label}")
        }
        Op::LabelRemove { target, which } => {
            let Some(id) = model.pick(*target) else {
                return "label remove (no live issue)".to_string();
            };
            let labels: Vec<String> = model.issues[&id].labels.iter().cloned().collect();
            if labels.is_empty() {
                return format!("label remove {id} (no labels)");
            }
            let label = labels[which % labels.len()].clone();
            storage
                .remove_label(&id, &label, ACTOR)
                .expect("remove label");
            model
                .issues
                .get_mut(&id)
                .expect("model issue")
                .labels
                .remove(&label);
            format!("label remove {id} {label}")
        }
        Op::DepAdd { from, to, kind } => apply_dep_add(*from, *to, *kind, storage, model),
        Op::DepRemove { target, which } => {
            let Some(id) = model.pick(*target) else {
                return "dep remove (no live issue)".to_string();
            };
            let outgoing = model.outgoing(&id);
            if outgoing.is_empty() {
                return format!("dep remove {id} (no edges)");
            }
            let (to, kind) = outgoing[which % outgoing.len()].clone();
            storage
                .remove_dependency(&id, &to, ACTOR)
                .expect("remove dependency");
            model.deps.remove(&(id.clone(), to.clone(), kind));
            format!("dep remove {id} -> {to}")
        }
        Op::CommentAdd { target, text } => {
            let Some(id) = model.pick(*target) else {
                return "comment (no live issue)".to_string();
            };
            storage.add_comment(&id, ACTOR, text).expect("add comment");
            model
                .issues
                .get_mut(&id)
                .expect("model issue")
                .comments
                .push(text.clone());
            format!("comment {id}")
        }
        Op::Delete { target } => {
            let Some(id) = model.pick(*target) else {
                return "delete (no live issue)".to_string();
            };
            storage
                .delete_issue(&id, ACTOR, "model", None)
                .expect("delete");
            let issue = model.issues.get_mut(&id).expect("model issue");
            issue.deleted = true;
            issue.status = Status::Tombstone;
            format!("delete {id}")
        }
    }
}

fn apply_dep_add(
    from: usize,
    to: usize,
    kind: usize,
    storage: &mut SqliteStorage,
    model: &mut Model,
) -> String {
    let (Some(from_id), Some(to_id)) = (model.pick(from), model.pick(to)) else {
        return "dep add (no live issue)".to_string();
    };
    if from_id == to_id || model.edge_between(&from_id, &to_id) {
        return format!("dep add {from_id} -> {to_id} (skipped: self or duplicate)");
    }
    let dep_type = DEP_TYPES[kind];
    if model.would_close_blocker_cycle(&from_id, &to_id, dep_type) {
        return format!("dep add {from_id} -> {to_id} ({dep_type}, skipped: cycle)");
    }
    // The storage allows one parent per issue: a second parent-child edge
    // from the same issue is refused until the first is cleared.
    if dep_type == "parent-child"
        && model
            .outgoing(&from_id)
            .iter()
            .any(|(_, kind)| kind == "parent-child")
    {
        return format!("dep add {from_id} -> {to_id} (skipped: already has a parent)");
    }
    storage
        .add_dependency(&from_id, &to_id, dep_type, ACTOR)
        .expect("add dependency");
    model
        .deps
        .insert((from_id.clone(), to_id.clone(), dep_type.to_string()));
    format!("dep add {from_id} -> {to_id} ({dep_type})")
}

/// Compare every projection the model tracks against the storage.
fn check_projections(storage: &SqliteStorage, model: &Model, context: &str) {
    let live: BTreeSet<String> = model.live_ids().into_iter().collect();
    for (id, expected) in &model.issues {
        let actual = storage
            .get_issue(id)
            .expect("get_issue")
            .unwrap_or_else(|| panic!("{context}: {id} missing from storage"));
        assert_eq!(actual.title, expected.title, "{context}: title of {id}");
        assert_eq!(actual.status, expected.status, "{context}: status of {id}");
        assert_eq!(
            actual.priority.0, expected.priority,
            "{context}: priority of {id}"
        );
        assert_eq!(actual.issue_type, expected.kind, "{context}: type of {id}");
        if expected.deleted {
            continue;
        }
        assert_eq!(
            actual.assignee, expected.assignee,
            "{context}: assignee of {id}"
        );
        let labels: BTreeSet<String> = storage
            .get_labels(id)
            .expect("get_labels")
            .into_iter()
            .collect();
        assert_eq!(labels, expected.labels, "{context}: labels of {id}");
        let comments: Vec<String> = storage
            .get_comments(id)
            .expect("get_comments")
            .into_iter()
            .map(|comment| comment.body)
            .collect();
        assert_eq!(comments, expected.comments, "{context}: comments of {id}");
        let deps: BTreeSet<String> = storage
            .get_dependencies(id)
            .expect("get_dependencies")
            .into_iter()
            .filter(|dep| live.contains(dep))
            .collect();
        let expected_deps: BTreeSet<String> = model
            .outgoing(id)
            .into_iter()
            .map(|(to, _)| to)
            .filter(|to| live.contains(to))
            .collect();
        assert_eq!(deps, expected_deps, "{context}: dependencies of {id}");
        let dependents: BTreeSet<String> = storage
            .get_dependents(id)
            .expect("get_dependents")
            .into_iter()
            .filter(|dep| live.contains(dep))
            .collect();
        let expected_dependents: BTreeSet<String> = model
            .incoming(id)
            .into_iter()
            .filter(|from| live.contains(from))
            .collect();
        assert_eq!(
            dependents, expected_dependents,
            "{context}: dependents of {id}"
        );
    }
    let listed: BTreeSet<String> = storage
        .list_issues(&ListFilters {
            include_closed: true,
            include_deferred: true,
            limit: Some(0),
            ..Default::default()
        })
        .expect("list_issues")
        .into_iter()
        .map(|issue| issue.id)
        .collect();
    assert_eq!(
        listed, live,
        "{context}: listing (closed + deferred included)"
    );
}

fn integrity_check(db_path: &Path) -> String {
    let conn = Connection::open(db_path.to_string_lossy().into_owned()).expect("open raw db");
    let rows = conn
        .query("PRAGMA integrity_check")
        .expect("integrity_check");
    let first = rows
        .first()
        .and_then(|row| row.get(0))
        .and_then(SqliteValue::as_text)
        .unwrap_or("<no row>")
        .to_string();
    conn.close().expect("close raw db");
    first
}

fn fresh_storage() -> (SqliteStorage, TempDir, std::path::PathBuf) {
    let (storage, dir) = common::test_db_with_dir();
    let db_path = dir.path().join(".beads").join("beads.db");
    (storage, dir, db_path)
}

fn run_sequence(ops: &[Op]) -> Result<(), TestCaseError> {
    let (mut storage, dir, db_path) = fresh_storage();
    let mut model = Model::default();
    let mut trace: Vec<String> = Vec::new();
    for (step, op) in ops.iter().enumerate() {
        let done = apply(op, &mut storage, &mut model);
        trace.push(format!("{step}: {done}"));
        let context = format!("after step {step} ({done})\ntrace:\n{}", trace.join("\n"));
        check_projections(&storage, &model, &context);
    }
    drop(storage);
    let integrity = integrity_check(&db_path);
    prop_assert_eq!(
        integrity.as_str(),
        "ok",
        "integrity_check after {} ops:\n{}",
        ops.len(),
        trace.join("\n")
    );
    if std::env::var_os("BR_KEEP_TEMP").is_some() {
        let kept = dir.keep();
        eprintln!("[model] kept workspace {}", kept.display());
    }
    Ok(())
}

fn configured_cases() -> u32 {
    std::env::var("BR_MODEL_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(120)
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: configured_cases(),
        max_shrink_iters: 2_000,
        ..ProptestConfig::default()
    })]

    #[test]
    fn storage_matches_reference_model(ops in proptest::collection::vec(op_strategy(), 1..=120)) {
        run_sequence(&ops)?;
    }
}

/// The model must predict cycles the way the storage detects them. A
/// parent-child edge runs parent -> child in the blocker graph (a parent
/// waits for its children), so a child that already reaches its would-be
/// parent through `blocks` edges is a deadlock the storage refuses, while
/// the opposite orientation and a non-blocking `related` edge are accepted.
/// The property test found the first case on the hosted misc shard (CI run
/// 33875242391) when the model still walked every edge as issue -> dependency.
#[test]
fn model_predicts_the_parent_child_blocker_direction() {
    let (mut storage, _dir, _db_path) = fresh_storage();
    let mut model = Model::default();
    for title in ["first", "second", "third"] {
        apply(
            &Op::Create {
                title: title.to_string(),
                priority: 2,
                kind: 0,
            },
            &mut storage,
            &mut model,
        );
    }
    // mb-0000 is blocked by mb-0001, which is blocked by mb-0002.
    assert_eq!(
        apply(
            &Op::DepAdd {
                from: 0,
                to: 1,
                kind: 0
            },
            &mut storage,
            &mut model
        ),
        "dep add mb-0000 -> mb-0001 (blocks)"
    );
    assert_eq!(
        apply(
            &Op::DepAdd {
                from: 1,
                to: 2,
                kind: 0
            },
            &mut storage,
            &mut model
        ),
        "dep add mb-0001 -> mb-0002 (blocks)"
    );
    check_projections(&storage, &model, "after the blocks chain");

    // mb-0000 as a child of mb-0002: the parent would wait for a child that
    // (transitively) waits for the parent. The model predicts the refusal
    // instead of calling the storage and panicking on DependencyCycle.
    assert_eq!(
        apply(
            &Op::DepAdd {
                from: 0,
                to: 2,
                kind: 2
            },
            &mut storage,
            &mut model
        ),
        "dep add mb-0000 -> mb-0002 (parent-child, skipped: cycle)"
    );
    let refused = storage.add_dependency("mb-0000", "mb-0002", "parent-child", ACTOR);
    assert!(
        matches!(
            refused,
            Err(beads_rust::error::BeadsError::DependencyCycle { .. })
        ),
        "the storage refuses the same edge as a cycle: {refused:?}"
    );
    check_projections(&storage, &model, "after the refused parent-child edge");

    // A non-blocking edge is never cycle-checked: `related` from mb-0002
    // back to mb-0000 closes a loop in the plain edge graph, and both the
    // storage and the model accept it.
    assert_eq!(
        apply(
            &Op::DepAdd {
                from: 2,
                to: 0,
                kind: 1
            },
            &mut storage,
            &mut model
        ),
        "dep add mb-0002 -> mb-0000 (related)"
    );
    check_projections(&storage, &model, "after the related edge");

    // A parent-child edge in the safe orientation: mb-0003 as the child of
    // mb-0000 adds the blocker edge mb-0000 -> mb-0003, which mb-0003 does
    // not reach back.
    apply(
        &Op::Create {
            title: "fourth".to_string(),
            priority: 2,
            kind: 0,
        },
        &mut storage,
        &mut model,
    );
    assert_eq!(
        apply(
            &Op::DepAdd {
                from: 3,
                to: 0,
                kind: 2
            },
            &mut storage,
            &mut model
        ),
        "dep add mb-0003 -> mb-0000 (parent-child)"
    );
    check_projections(&storage, &model, "after the accepted parent-child edge");
}

/// The checker itself must fail readably when a projection drifts from the
/// model: a label added behind the model's back is reported as a labels
/// mismatch naming the issue and the step.
#[test]
fn checker_reports_a_projection_that_drifted_from_the_model() {
    let (mut storage, _dir, _db_path) = fresh_storage();
    let mut model = Model::default();
    apply(
        &Op::Create {
            title: "drifts".to_string(),
            priority: 3,
            kind: 0,
        },
        &mut storage,
        &mut model,
    );
    storage
        .add_label("mb-0000", "unmodeled", ACTOR)
        .expect("add label behind the model's back");
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        check_projections(&storage, &model, "after an unmodeled label add");
    }));
    let message = match outcome {
        Ok(()) => panic!("the checker accepted a projection the model does not predict"),
        Err(payload) => payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| (*s).to_string()))
            .unwrap_or_default(),
    };
    assert!(
        message.contains("labels of mb-0000") && message.contains("after an unmodeled label add"),
        "checker message should name the projection and the step: {message}"
    );
}

/// GitHub #426: 300 issues chained by `blocks` edges, then 264 sequential
/// dependency removals, left the B-tree in rowid disorder. The projection
/// check after every removal plus the final integrity check cover it.
#[test]
fn gh426_sequential_dependency_removals_keep_projections_and_integrity() {
    let (mut storage, _dir, db_path) = fresh_storage();
    let mut model = Model::default();
    for index in 0..300 {
        apply(
            &Op::Create {
                title: format!("chain {index}"),
                priority: 2,
                kind: 0,
            },
            &mut storage,
            &mut model,
        );
    }
    for index in 1..300_usize {
        apply(
            &Op::DepAdd {
                from: index,
                to: index - 1,
                kind: 0,
            },
            &mut storage,
            &mut model,
        );
    }
    check_projections(&storage, &model, "after building the chain");
    for step in 0..264_usize {
        let done = apply(
            &Op::DepRemove {
                target: 299 - step,
                which: 0,
            },
            &mut storage,
            &mut model,
        );
        check_projections(&storage, &model, &format!("after removal {step} ({done})"));
    }
    assert_eq!(model.deps.len(), 299 - 264);
    drop(storage);
    assert_eq!(integrity_check(&db_path), "ok");
}

/// GitHub #461 (storage half): adding a comment must never lose the bodies
/// of the comments already stored, including across a close and reopen of
/// the database.
#[test]
fn gh461_comment_add_never_drops_prior_comments() {
    let (mut storage, _dir, db_path) = fresh_storage();
    let mut model = Model::default();
    apply(
        &Op::Create {
            title: "commented".to_string(),
            priority: 1,
            kind: 1,
        },
        &mut storage,
        &mut model,
    );
    for index in 0..25 {
        apply(
            &Op::CommentAdd {
                target: 0,
                text: format!("comment body {index}"),
            },
            &mut storage,
            &mut model,
        );
        check_projections(&storage, &model, &format!("after comment {index}"));
    }
    drop(storage);
    let reopened = SqliteStorage::open(&db_path).expect("reopen");
    check_projections(&reopened, &model, "after reopening the database");
    drop(reopened);
    assert_eq!(integrity_check(&db_path), "ok");
}
