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
//! Readiness and annotated blocker witnesses are checked against independent
//! graph rules, including typed edges and hierarchy. Deliberate invalid
//! dependencies and predicted cycles must fail without changing issue data,
//! events, or dirty tracking. Indices resolve against live issues so shrinking
//! preserves meaningful operations. Set
//! `BR_MODEL_CASES` to run more cases than the default.
mod common;

use beads_rust::error::BeadsError;
use beads_rust::franken_sync::Connection;
use beads_rust::model::{Issue, IssueType, Priority, Status};
use beads_rust::storage::{IssueUpdate, ListFilters, ReadyFilters, ReadySortPolicy, SqliteStorage};
use chrono::{DateTime, Utc};
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
const DEP_TYPES: [&str; 5] = [
    "blocks",
    "related",
    "parent-child",
    "conditional-blocks",
    "waits-for",
];

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
    CreateReadiness {
        flags: u8,
        defer: usize,
    },
    Schedule {
        target: usize,
        defer: usize,
    },
    RejectDependency {
        target: usize,
        invalid: usize,
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
        1 => (0..32_u8, 0..3_usize).prop_map(|(flags, defer)| Op::CreateReadiness { flags, defer }),
        1 => (any::<usize>(), 0..3_usize).prop_map(|(target, defer)| Op::Schedule { target, defer }),
        1 => (any::<usize>(), 0..3_usize).prop_map(|(target, invalid)| Op::RejectDependency { target, invalid }),
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
    pinned: bool,
    ephemeral: bool,
    is_template: bool,
    defer_until: Option<DateTime<Utc>>,
    due_at: Option<DateTime<Utc>>,
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
            .filter(|(_, issue)| issue.status != Status::Tombstone)
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

    /// Direct prerequisites are distinct from hierarchy: an unfinished
    /// prerequisite blocks its dependent, while a completed/template issue
    /// does not. This specification uses only the model, never storage caches.
    fn direct_blockers(&self, id: &str) -> BTreeSet<String> {
        self.outgoing(id)
            .into_iter()
            .filter(|(_, kind)| {
                matches!(kind.as_str(), "blocks" | "conditional-blocks" | "waits-for")
            })
            .filter_map(|(target, _)| {
                let issue = &self.issues[&target];
                (!matches!(issue.status, Status::Closed | Status::Tombstone) && !issue.is_template)
                    .then(|| format!("{target}:{}", issue.status.as_str()))
            })
            .collect()
    }

    fn parent(&self, child: &str) -> Option<String> {
        self.outgoing(child)
            .into_iter()
            .find_map(|(parent, kind)| (kind == "parent-child").then_some(parent))
    }

    /// A child's ancestor must have a real prerequisite to propagate a
    /// readiness blocker. An epic's open-child rollup never blocks that child
    /// back. Walking ancestors independently avoids copying the cache's
    /// propagation algorithm into the oracle.
    fn blocker_refs(&self, id: &str) -> BTreeSet<String> {
        let mut blockers = self.direct_blockers(id);
        if let Some(parent) = self.parent(id) {
            let mut ancestor = Some(parent.clone());
            let mut seen = BTreeSet::new();
            while let Some(current) = ancestor {
                if !seen.insert(current.clone()) {
                    break;
                }
                if !self.direct_blockers(&current).is_empty() {
                    blockers.insert(format!("{parent}:parent-blocked"));
                    break;
                }
                ancestor = self.parent(&current);
            }
        }
        if self.issues[id].kind == IssueType::Epic {
            for (child, parent, kind) in &self.deps {
                let issue = &self.issues[child];
                if parent == id
                    && kind == "parent-child"
                    && !matches!(issue.status, Status::Closed | Status::Tombstone)
                    && !issue.is_template
                {
                    blockers.insert(format!("{child}:child-open"));
                }
            }
        }
        blockers
    }

    fn blocked(&self) -> BTreeMap<String, BTreeSet<String>> {
        self.issues
            .iter()
            .filter_map(|(id, issue)| {
                let blockers = self.blocker_refs(id);
                (!matches!(issue.status, Status::Closed | Status::Tombstone)
                    && !blockers.is_empty())
                .then(|| (id.clone(), blockers))
            })
            .collect()
    }

    fn ready(
        &self,
        now: DateTime<Utc>,
        statuses: &[&str],
        include_deferred: bool,
    ) -> BTreeSet<String> {
        self.issues
            .iter()
            .filter(|(id, issue)| {
                (statuses.contains(&issue.status.as_str())
                    || (include_deferred && issue.status == Status::Deferred))
                    && self.blocker_refs(id).is_empty()
                    && (include_deferred || issue.defer_until.is_none_or(|until| until <= now))
                    && !issue.pinned
                    && !issue.ephemeral
                    && !issue.is_template
                    && !id.contains("-wisp-")
            })
            .map(|(id, _)| id.clone())
            .collect()
    }
}

/// Fixed dates stay well outside the test's execution window; equality at the
/// admission boundary is tested separately against the pure model.
fn schedule_date(choice: usize) -> Option<DateTime<Utc>> {
    match choice {
        0 => None,
        1 => Some("2000-01-01T00:00:00Z".parse().expect("past date")),
        _ => Some("2100-01-01T00:00:00Z".parse().expect("future date")),
    }
}

fn record_creation(storage: &mut SqliteStorage, model: &mut Model, issue: Issue) -> String {
    storage.create_issue(&issue, ACTOR).expect("create");
    let id = issue.id.clone();
    model.next += 1;
    model.issues.insert(
        id.clone(),
        ModelIssue {
            title: issue.title,
            status: issue.status,
            priority: issue.priority.0,
            kind: issue.issue_type,
            assignee: issue.assignee,
            labels: BTreeSet::new(),
            comments: Vec::new(),
            pinned: issue.pinned,
            ephemeral: issue.ephemeral,
            is_template: issue.is_template,
            defer_until: issue.defer_until,
            due_at: issue.due_at,
        },
    );
    format!("create {id}")
}

fn refusal_snapshot(storage: &SqliteStorage, model: &Model) -> serde_json::Value {
    let issues: Vec<_> = model
        .issues
        .keys()
        .map(|id| storage.get_issue(id).expect("refusal issue"))
        .collect();
    let deps: Vec<_> = model
        .issues
        .keys()
        .map(|id| storage.get_dependencies_full(id).expect("refusal deps"))
        .collect();
    serde_json::json!({
        "issues": issues,
        "dependencies": deps,
        "events": storage.get_all_events(0).expect("refusal events"),
        "dirty": storage.get_dirty_issue_metadata().expect("refusal dirty metadata"),
        "needs_flush": storage.get_metadata("needs_flush").expect("refusal flush marker"),
    })
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
            let kind = KINDS[*kind].clone();
            record_creation(storage, model, new_issue(&id, title, *priority, kind))
        }
        Op::CreateReadiness { flags, defer } => {
            let id = if flags & 8 != 0 {
                format!("mb-wisp-{:04}", model.next)
            } else {
                format!("mb-{:04}", model.next)
            };
            let mut issue = new_issue(&id, "readiness controls", 2, IssueType::Task);
            issue.pinned = flags & 1 != 0;
            issue.ephemeral = flags & 2 != 0;
            issue.is_template = flags & 4 != 0;
            if flags & 16 != 0 {
                issue.status = Status::Custom("review".to_string());
            }
            issue.defer_until = schedule_date(*defer);
            // A future due date is not a readiness admission gate.
            issue.due_at = schedule_date(2);
            record_creation(storage, model, issue)
        }
        Op::Schedule { target, defer } => {
            let Some(id) = model.pick(*target) else {
                return "schedule (no live issue)".to_string();
            };
            let until = schedule_date(*defer);
            storage
                .update_issue(
                    &id,
                    &IssueUpdate {
                        defer_until: Some(until),
                        ..Default::default()
                    },
                    ACTOR,
                )
                .expect("schedule");
            model.issues.get_mut(&id).expect("model issue").defer_until = until;
            format!("schedule {id} {until:?}")
        }
        Op::RejectDependency { target, invalid } => {
            let Some(id) = model.pick(*target) else {
                return "invalid dependency (no live issue)".to_string();
            };
            let before = refusal_snapshot(storage, model);
            let error = match invalid {
                0 => storage.add_dependency(&id, &id, "blocks", ACTOR),
                1 => storage.add_dependency(&id, "mb-missing", "blocks", ACTOR),
                _ => storage.add_dependency_with_metadata(
                    &id,
                    "mb-missing",
                    "blocks",
                    ACTOR,
                    Some("{"),
                ),
            }
            .expect_err("deliberate invalid dependency must be refused");
            let expected = match invalid {
                0 => matches!(error, BeadsError::SelfDependency { .. }),
                1 => matches!(error, BeadsError::IssueNotFound { .. }),
                _ => {
                    matches!(error, BeadsError::Validation { ref field, .. } if field == "metadata")
                }
            };
            assert!(
                expected,
                "unexpected (possibly uncertain engine) failure: {error:?}"
            );
            assert_eq!(
                refusal_snapshot(storage, model),
                before,
                "refusal mutated state: {error}"
            );
            format!("refused dependency {id} {error}")
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
    // Parent uniqueness is checked before cycle admission.
    if dep_type == "parent-child"
        && model
            .outgoing(&from_id)
            .iter()
            .any(|(_, kind)| kind == "parent-child")
    {
        return format!("dep add {from_id} -> {to_id} (skipped: already has a parent)");
    }
    if model.would_close_blocker_cycle(&from_id, &to_id, dep_type) {
        let before = refusal_snapshot(storage, model);
        let refused = storage.add_dependency(&from_id, &to_id, dep_type, ACTOR);
        assert!(
            matches!(refused, Err(BeadsError::DependencyCycle { .. })),
            "expected deterministic cycle refusal, got {refused:?}"
        );
        assert_eq!(
            refusal_snapshot(storage, model),
            before,
            "cycle refusal changed semantic state"
        );
        return format!("dep add {from_id} -> {to_id} ({dep_type}, refused: cycle)");
    }
    storage
        .add_dependency(&from_id, &to_id, dep_type, ACTOR)
        .expect("add dependency");
    model
        .deps
        .insert((from_id.clone(), to_id.clone(), dep_type.to_string()));
    format!("dep add {from_id} -> {to_id} ({dep_type})")
}

fn check_issue_fields(actual: &Issue, expected: &ModelIssue, context: &str) {
    let id = &actual.id;
    assert_eq!(actual.title, expected.title, "{context}: title of {id}");
    assert_eq!(actual.status, expected.status, "{context}: status of {id}");
    assert_eq!(
        actual.priority.0, expected.priority,
        "{context}: priority of {id}"
    );
    assert_eq!(actual.issue_type, expected.kind, "{context}: type of {id}");
    assert_eq!(actual.pinned, expected.pinned, "{context}: pinned of {id}");
    assert_eq!(
        actual.ephemeral, expected.ephemeral,
        "{context}: ephemeral of {id}"
    );
    assert_eq!(
        actual.is_template, expected.is_template,
        "{context}: template of {id}"
    );
    assert_eq!(
        actual.defer_until, expected.defer_until,
        "{context}: defer time of {id}"
    );
    assert_eq!(
        actual.due_at, expected.due_at,
        "{context}: due time of {id}"
    );
}

/// Compare every projection the model tracks against the storage.
fn check_projections(storage: &SqliteStorage, model: &Model, context: &str) {
    let live: BTreeSet<String> = model.live_ids().into_iter().collect();
    for (id, expected) in &model.issues {
        let actual = storage
            .get_issue(id)
            .expect("get_issue")
            .unwrap_or_else(|| panic!("{context}: {id} missing from storage"));
        check_issue_fields(&actual, expected, context);
        if expected.status == Status::Tombstone {
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
        let deps: BTreeSet<(String, String)> = storage
            .get_dependencies_full(id)
            .expect("get_dependencies_full")
            .into_iter()
            .filter(|dep| live.contains(&dep.depends_on_id))
            .map(|dep| {
                assert_eq!(dep.issue_id, *id, "{context}: dependency source");
                assert_eq!(
                    dep.created_by.as_deref(),
                    Some(ACTOR),
                    "{context}: dependency actor"
                );
                assert_eq!(
                    dep.metadata.as_deref(),
                    Some("{}"),
                    "{context}: dependency metadata"
                );
                assert_eq!(
                    dep.thread_id.as_deref(),
                    Some(""),
                    "{context}: dependency thread (schema default)"
                );
                (dep.depends_on_id, dep.dep_type.as_str().to_string())
            })
            .collect();
        let expected_deps: BTreeSet<(String, String)> = model
            .outgoing(id)
            .into_iter()
            .filter(|(to, _)| live.contains(to))
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
            include_templates: true,
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
    check_readiness(storage, model, &["open"], false, context);
}

fn check_readiness(
    storage: &SqliteStorage,
    model: &Model,
    statuses: &[&str],
    include_deferred: bool,
    context: &str,
) {
    let filters = ReadyFilters {
        ready_statuses: statuses
            .iter()
            .map(|status| (*status).to_string())
            .collect(),
        include_deferred,
        ..Default::default()
    };
    let ready = storage
        .get_ready_issues(&filters, ReadySortPolicy::Priority)
        .expect("ready query")
        .into_iter()
        .map(|issue| issue.id)
        .collect();
    let blocked = storage
        .get_blocked_issues()
        .expect("blocked query")
        .into_iter()
        .map(|(issue, refs)| (issue.id, refs.into_iter().collect()))
        .collect();
    assert_readiness_projection(model, &ready, &blocked, statuses, include_deferred, context);
}

fn assert_readiness_projection(
    model: &Model,
    ready: &BTreeSet<String>,
    blocked: &BTreeMap<String, BTreeSet<String>>,
    statuses: &[&str],
    include_deferred: bool,
    context: &str,
) {
    assert_eq!(
        *blocked,
        model.blocked(),
        "{context}: blocked IDs and witnesses"
    );
    assert_eq!(
        *ready,
        model.ready(Utc::now(), statuses, include_deferred),
        "{context}: ready IDs"
    );
}

fn integrity_check(db_path: &Path) -> String {
    let conn = Connection::open(db_path.to_string_lossy().into_owned()).expect("open raw db");
    let rows = conn
        .query("PRAGMA integrity_check")
        .expect("integrity_check");
    let values: Vec<_> = rows.iter().map(|row| row.values().to_vec()).collect();
    let result = integrity_result(&values);
    conn.close().expect("close raw db");
    result
}

fn integrity_result(rows: &[Vec<SqliteValue>]) -> String {
    match rows {
        [row] if row.len() == 1 && row[0].as_text() == Some("ok") => "ok".to_string(),
        _ => format!("unexpected PRAGMA integrity_check rows: {rows:?}"),
    }
}

#[test]
fn integrity_checker_rejects_trailing_corruption_and_malformed_results() {
    let (storage, _directory, db_path) = fresh_storage();
    drop(storage);
    assert_eq!(integrity_check(&db_path), "ok", "real engine baseline");
    for rows in [
        vec![],
        vec![vec![]],
        vec![
            vec![SqliteValue::from("ok")],
            vec![SqliteValue::from("later corruption")],
        ],
        vec![vec![
            SqliteValue::from("ok"),
            SqliteValue::from("extra column"),
        ]],
        vec![vec![SqliteValue::Null]],
        vec![vec![SqliteValue::from(0_i64)]],
        vec![vec![SqliteValue::from("OK")]],
    ] {
        let result = integrity_result(&rows);
        assert!(
            result.starts_with("unexpected PRAGMA integrity_check rows:"),
            "unexpected integrity result accepted: {rows:?}"
        );
        assert!(
            result.contains(&format!("{rows:?}")),
            "rows omitted: {result}"
        );
    }
}

fn fresh_storage() -> (SqliteStorage, TempDir, std::path::PathBuf) {
    let (storage, dir) = common::test_db_with_dir();
    let db_path = dir.path().join(".beads").join("beads.db");
    (storage, dir, db_path)
}

fn run_sequence(ops: &[Op]) {
    let (mut storage, dir, db_path) = fresh_storage();
    let mut model = Model::default();
    let mut trace: Vec<String> = Vec::new();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for (step, op) in ops.iter().enumerate() {
            trace.push(format!("{step}: requested {op:?}"));
            let done = apply(op, &mut storage, &mut model);
            trace.push(format!("{step}: {done}"));
            let context = format!("after step {step} ({done})\ntrace:\n{}", trace.join("\n"));
            check_projections(&storage, &model, &context);
        }
        drop(storage);
        let reopened = SqliteStorage::open(&db_path).expect("reopen model database");
        check_projections(
            &reopened,
            &model,
            &format!("after reopen\n{}", trace.join("\n")),
        );
        check_readiness(
            &reopened,
            &model,
            &["open", "in_progress", "review"],
            true,
            "configured ready group after reopen",
        );
        drop(reopened);
        assert_eq!(
            integrity_check(&db_path),
            "ok",
            "integrity_check after {} ops:\n{}",
            ops.len(),
            trace.join("\n")
        );
    }));
    if let Err(payload) = result {
        let kept = dir.keep();
        let path = kept.join("model-failure-trace.txt");
        std::fs::write(
            &path,
            format!("operations: {ops:#?}\ntrace:\n{}", trace.join("\n")),
        )
        .expect("write model failure trace");
        eprintln!(
            "[model] failure workspace and replay trace: {}",
            path.display()
        );
        std::panic::resume_unwind(payload);
    }
    if std::env::var_os("BR_KEEP_TEMP").is_some() {
        let kept = dir.keep();
        eprintln!("[model] kept workspace {}", kept.display());
    }
}

fn configured_cases() -> u32 {
    let cases = std::env::var("BR_MODEL_CASES").map_or(120, |value| {
        value
            .parse::<u32>()
            .expect("BR_MODEL_CASES must be a positive integer")
    });
    assert!(cases > 0, "BR_MODEL_CASES=0 would skip the model campaign");
    eprintln!(
        "[model] cases={cases} max_ops=120 br={} engine={} source={} seed={:?}",
        env!("CARGO_PKG_VERSION"),
        option_env!("BR_FSQLITE_VERSION").unwrap_or("unknown"),
        option_env!("VERGEN_GIT_SHA").unwrap_or("unknown"),
        std::env::var("PROPTEST_RNG_SEED").ok()
    );
    cases
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: configured_cases(),
        max_shrink_iters: 2_000,
        ..ProptestConfig::default()
    })]

    #[test]
    fn storage_matches_reference_model(ops in proptest::collection::vec(op_strategy(), 1..=120)) {
        run_sequence(&ops);
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
    // and verifies the actual refusal leaves every observed value unchanged.
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
        "dep add mb-0000 -> mb-0002 (parent-child, refused: cycle)"
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

#[test]
fn readiness_model_distinguishes_prerequisites_hierarchy_and_epic_rollup() {
    let (mut storage, _dir, db_path) = fresh_storage();
    let mut model = Model::default();
    for (index, title) in ["epic", "child", "grandchild", "prerequisite", "related"]
        .iter()
        .enumerate()
    {
        apply(
            &Op::Create {
                title: (*title).to_string(),
                priority: 2,
                kind: usize::from(index == 0) * 3,
            },
            &mut storage,
            &mut model,
        );
    }
    for (from, to, kind) in [(1, 0, 2), (2, 1, 2), (4, 3, 1)] {
        apply(&Op::DepAdd { from, to, kind }, &mut storage, &mut model);
    }
    assert_eq!(
        model.blocked(),
        BTreeMap::from([(
            "mb-0000".to_string(),
            BTreeSet::from(["mb-0001:child-open".to_string()])
        )])
    );
    assert_eq!(
        model.ready(Utc::now(), &["open"], false),
        ["mb-0001", "mb-0002", "mb-0003", "mb-0004"]
            .into_iter()
            .map(str::to_string)
            .collect()
    );
    check_projections(
        &storage,
        &model,
        "unblocked children of an epic with open children",
    );

    apply(
        &Op::DepAdd {
            from: 0,
            to: 3,
            kind: 4,
        },
        &mut storage,
        &mut model,
    );
    assert_eq!(
        model.blocked(),
        BTreeMap::from([
            (
                "mb-0000".to_string(),
                BTreeSet::from(["mb-0003:open".to_string(), "mb-0001:child-open".to_string()])
            ),
            (
                "mb-0001".to_string(),
                BTreeSet::from(["mb-0000:parent-blocked".to_string()])
            ),
            (
                "mb-0002".to_string(),
                BTreeSet::from(["mb-0001:parent-blocked".to_string()])
            ),
        ])
    );
    check_projections(
        &storage,
        &model,
        "blocked grandparent propagates through the immediate parent",
    );
    // The informational edge leaves mb-0004 ready while the real prerequisite
    // blocks the epic and its descendants.
    assert_eq!(storage.get_blockers("mb-0002").unwrap(), ["mb-0001"]);
    assert!(storage.get_start_blockers("mb-0002").unwrap().is_empty());
    assert!(storage.get_close_blockers("mb-0002").unwrap().is_empty());
    apply(&Op::Close { target: 3 }, &mut storage, &mut model);
    check_projections(
        &storage,
        &model,
        "close prerequisite clears descendant blockers",
    );
    apply(
        &Op::SetStatus {
            target: 3,
            status: 0,
        },
        &mut storage,
        &mut model,
    );
    check_projections(
        &storage,
        &model,
        "reopen prerequisite restores descendant blockers",
    );
    drop(storage);
    let reopened = SqliteStorage::open(&db_path).expect("reopen hierarchy database");
    check_projections(&reopened, &model, "reopened hierarchy projection");
}

#[test]
fn readiness_model_checks_flags_custom_statuses_and_fixed_defer_boundaries() {
    let (mut storage, _dir, _db_path) = fresh_storage();
    let mut model = Model::default();
    for flags in [0, 1, 2, 4, 8, 16] {
        apply(
            &Op::CreateReadiness { flags, defer: 0 },
            &mut storage,
            &mut model,
        );
    }
    for defer in [1, 2] {
        apply(
            &Op::CreateReadiness { flags: 0, defer },
            &mut storage,
            &mut model,
        );
    }
    let mut deferred = new_issue("mb-deferred", "deferred", 2, IssueType::Task);
    deferred.status = Status::Deferred;
    deferred.defer_until = schedule_date(2);
    record_creation(&mut storage, &mut model, deferred);
    check_projections(&storage, &model, "each independent readiness exclusion");
    let expected: BTreeSet<_> = ["mb-0000", "mb-0006"]
        .into_iter()
        .map(str::to_string)
        .collect();
    assert_eq!(model.ready(Utc::now(), &["open"], false), expected);
    check_readiness(
        &storage,
        &model,
        &["review"],
        false,
        "configured custom-only ready status",
    );
    assert_eq!(
        model.ready(Utc::now(), &["review"], false),
        BTreeSet::from(["mb-0005".to_string()])
    );
    check_readiness(
        &storage,
        &model,
        &["open", "review"],
        true,
        "include-deferred bypasses the time gate",
    );
    assert!(
        model
            .ready(Utc::now(), &["open"], true)
            .contains("mb-deferred")
    );

    // An injected clock is confined to the engine-free specification. Engine
    // comparisons above use stable 2000/2100 boundaries rather than sleeps.
    let boundary: DateTime<Utc> = "2030-01-01T00:00:00Z".parse().unwrap();
    model.issues.get_mut("mb-0000").unwrap().defer_until = Some(boundary);
    assert!(
        !model
            .ready(boundary - chrono::Duration::seconds(1), &["open"], false)
            .contains("mb-0000")
    );
    assert!(model.ready(boundary, &["open"], false).contains("mb-0000"));
    assert!(
        model
            .ready(boundary + chrono::Duration::seconds(1), &["open"], false)
            .contains("mb-0000")
    );
}

#[test]
fn invalid_dependencies_preserve_clean_and_dirty_state() {
    let (mut storage, _dir, _db_path) = fresh_storage();
    let mut model = Model::default();
    apply(
        &Op::Create {
            title: "unchanged".to_string(),
            priority: 2,
            kind: 0,
        },
        &mut storage,
        &mut model,
    );
    for clean in [false, true] {
        if clean {
            storage
                .clear_all_dirty_issues()
                .expect("clear dirty baseline");
            storage
                .set_metadata("needs_flush", "false")
                .expect("clear flush baseline");
        }
        for invalid in 0..3 {
            apply(
                &Op::RejectDependency { target: 0, invalid },
                &mut storage,
                &mut model,
            );
            check_projections(
                &storage,
                &model,
                "refused self/missing-target/invalid-metadata dependency",
            );
        }
    }
}

#[test]
fn readiness_checker_rejects_missing_blockers_wrong_witnesses_and_stale_ready_rows() {
    let (mut storage, _dir, _db_path) = fresh_storage();
    let mut model = Model::default();
    for title in ["dependent", "prerequisite"] {
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
    apply(
        &Op::DepAdd {
            from: 0,
            to: 1,
            kind: 0,
        },
        &mut storage,
        &mut model,
    );
    let ready = BTreeSet::from(["mb-0001".to_string()]);
    let blocked = BTreeMap::from([(
        "mb-0000".to_string(),
        BTreeSet::from(["mb-0001:open".to_string()]),
    )]);
    assert_readiness_projection(
        &model,
        &ready,
        &blocked,
        &["open"],
        false,
        "hand-checked baseline",
    );
    check_projections(
        &storage,
        &model,
        "live baseline before artificial projection faults",
    );

    for fault in ["missing blocker", "wrong witness", "stale ready"] {
        let mut faulty_ready = ready.clone();
        let mut faulty_blocked = blocked.clone();
        match fault {
            "missing blocker" => {
                faulty_blocked.clear();
            }
            "wrong witness" => {
                faulty_blocked.insert(
                    "mb-0000".to_string(),
                    BTreeSet::from(["mb-0001:closed".to_string()]),
                );
            }
            _ => {
                faulty_ready.insert("mb-0000".to_string());
            }
        }
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            assert_readiness_projection(
                &model,
                &faulty_ready,
                &faulty_blocked,
                &["open"],
                false,
                fault,
            );
        }));
        let payload =
            outcome.expect_err("checker must reject the deliberately altered observation");
        let message = payload
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| payload.downcast_ref::<&str>().copied())
            .unwrap_or("");
        assert!(
            message.contains(fault) && message.contains("mb-0000"),
            "missing mismatch context: {message}"
        );
    }
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
