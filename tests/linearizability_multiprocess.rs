//! Multi-process linearizability check for `br` (bead beads_rust-dk45.8).
//!
//! N worker threads each drive their own stream of `br` processes against one
//! workspace for a fixed wall-clock budget and record every invocation as a
//! history entry `{pid, invoke, return, op, outcome}`. Every operation used
//! here touches a single issue: dependency edges are recorded under their
//! `from` endpoint and only ever point into a fixed sink set that never gains
//! outgoing edges, so neither cycles nor cross-issue refusals can arise. The
//! histories therefore partition by issue id, and each partition is checked
//! with a Wing & Gong style search for a linearization that respects the
//! real-time order of the process calls and the sequential model below.
//! `show --json` observations are compared field by field with the model;
//! mutations that exited non-zero may have taken effect or not.
//!
//! After the run the database must pass `PRAGMA integrity_check`, its rowids
//! must be dense, and the JSONL published by `br sync --flush-only` must match
//! the linearized final state of every issue.
//!
//! Knobs: `BR_LINEARIZABILITY_PROCESSES` (default 8),
//! `BR_LINEARIZABILITY_SECONDS` (default 30), and
//! `BR_LINEARIZABILITY_ARTIFACT_DIR` (where the merged history and the failing
//! partition are written on a violation; default: the kept temp workspace).

mod common;

use beads_rust::franken_sync::Connection;
use fsqlite_types::SqliteValue;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tempfile::TempDir;

const DEFAULT_PROCESSES: u64 = 8;
const DEFAULT_SECONDS: u64 = 30;
const SOURCE_ISSUES: usize = 8;
const SINK_ISSUES: usize = 8;
const LABELS: [&str; 4] = ["alpha", "beta", "gamma", "delta"];
/// Mutations serialize on the workspace write lock and each one costs
/// 100–200 ms of process start, open, commit, and flush (profile on bead
/// naul5), so eight streams complete only a few hundred operations in 30 s
/// (about 230 before the schema-witness fast open, about 330 after); the
/// floor only rules out a run that barely started.
const MIN_OPERATIONS: usize = 100;

// ---------------------------------------------------------------------------
// Sequential model
// ---------------------------------------------------------------------------

/// What `br show --json` reveals about one issue, normalized for comparison.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct Observation {
    status: String,
    priority: i32,
    labels: Vec<String>,
    comments: usize,
    depends_on: Vec<String>,
}

/// The sequential specification of one issue.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize)]
struct KeyState {
    exists: bool,
    status: String,
    priority: i32,
    labels: BTreeSet<String>,
    comments: usize,
    depends_on: BTreeSet<String>,
}

impl KeyState {
    fn observation(&self) -> Observation {
        Observation {
            status: self.status.clone(),
            priority: self.priority,
            labels: self.labels.iter().cloned().collect(),
            comments: self.comments,
            depends_on: self.depends_on.iter().cloned().collect(),
        }
    }

    /// The state after `op` took effect, or `None` when the model says the
    /// operation could not have reported success from this state.
    fn after(&self, op: &Op) -> Option<Self> {
        if let Op::Create { priority, .. } = op {
            return (!self.exists).then(|| Self {
                exists: true,
                status: "open".to_string(),
                priority: *priority,
                ..Self::default()
            });
        }
        if !self.exists {
            return None;
        }
        let mut next = self.clone();
        match op {
            Op::Create { .. } | Op::Show => return None,
            Op::Close => {
                if next.status == "closed" {
                    return None;
                }
                next.status = "closed".to_string();
            }
            Op::Reopen => {
                if next.status != "closed" {
                    return None;
                }
                next.status = "open".to_string();
            }
            Op::SetPriority(priority) => next.priority = *priority,
            Op::LabelAdd(label) => {
                next.labels.insert(label.clone());
            }
            Op::LabelRemove(label) => {
                next.labels.remove(label);
            }
            Op::CommentAdd => next.comments += 1,
            Op::DepAdd(target) => {
                next.depends_on.insert(target.clone());
            }
            Op::DepRemove(target) => {
                next.depends_on.remove(target);
            }
        }
        Some(next)
    }

    /// Whether a zero-exit "skipped" report with `reason` is consistent here.
    fn skip_is_consistent(&self, op: &Op, reason: &str) -> bool {
        if !self.exists {
            return false;
        }
        match op {
            // "blocked by" depends on other issues' states, which this
            // per-issue model does not track; any live state may report it.
            Op::Close => {
                (self.status == "closed" && reason.starts_with("already"))
                    || reason.starts_with("blocked by")
            }
            Op::Reopen => self.status != "closed" && reason.starts_with("already"),
            _ => false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Op {
    Create { title: String, priority: i32 },
    Close,
    Reopen,
    SetPriority(i32),
    LabelAdd(String),
    LabelRemove(String),
    CommentAdd,
    DepAdd(String),
    DepRemove(String),
    Show,
}

impl Op {
    const fn name(&self) -> &'static str {
        match self {
            Self::Create { .. } => "create",
            Self::Close => "close",
            Self::Reopen => "reopen",
            Self::SetPriority(_) => "update --priority",
            Self::LabelAdd(_) => "label add",
            Self::LabelRemove(_) => "label remove",
            Self::CommentAdd => "comments add",
            Self::DepAdd(_) => "dep add",
            Self::DepRemove(_) => "dep remove",
            Self::Show => "show",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Outcome {
    /// Zero exit and, for `close`/`reopen`, the issue listed as acted on.
    Applied,
    /// Zero exit with the issue listed as skipped for this reason.
    Skipped(String),
    /// Zero exit from `show` with this normalized payload.
    Observed(Observation),
    /// Non-zero exit; whether the mutation landed is unknown.
    Failed(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry {
    pid: usize,
    seq: usize,
    key: String,
    invoke_ns: u64,
    return_ns: u64,
    op: Op,
    outcome: Outcome,
}

/// The states an entry may leave behind from `state`; empty means the entry
/// cannot be linearized here.
fn successors(state: &KeyState, entry: &Entry) -> Vec<KeyState> {
    match &entry.outcome {
        Outcome::Observed(seen) => {
            if state.exists && state.observation() == *seen {
                vec![state.clone()]
            } else {
                Vec::new()
            }
        }
        Outcome::Applied => state.after(&entry.op).into_iter().collect(),
        Outcome::Skipped(reason) => {
            if state.skip_is_consistent(&entry.op, reason) {
                vec![state.clone()]
            } else {
                Vec::new()
            }
        }
        Outcome::Failed(_) => {
            let mut next = vec![state.clone()];
            if let Some(applied) = state.after(&entry.op)
                && applied != *state
            {
                next.push(applied);
            }
            next
        }
    }
}

// ---------------------------------------------------------------------------
// Per-partition linearization search (Wing & Gong)
// ---------------------------------------------------------------------------

/// The deepest point the search reached before every remaining eligible entry
/// contradicted the model.
#[derive(Clone, Debug, Serialize)]
struct DeadEnd {
    linearized: usize,
    state: KeyState,
    eligible: Vec<Entry>,
}

#[derive(Clone, Debug, Serialize)]
struct Violation {
    key: String,
    dead_end: DeadEnd,
}

struct Search<'a> {
    entries: &'a [Entry],
    predecessors: Vec<Vec<usize>>,
    done: Vec<bool>,
    memo: HashSet<(Vec<bool>, KeyState)>,
    deepest: Option<DeadEnd>,
}

impl Search<'_> {
    /// `entries` must be one issue's history sorted by invocation time;
    /// `initial` is the issue's state before the first recorded call.
    fn run(entries: &[Entry], initial: KeyState) -> Result<KeyState, DeadEnd> {
        let predecessors = entries
            .iter()
            .map(|entry| {
                entries
                    .iter()
                    .enumerate()
                    .filter(|(_, other)| other.return_ns < entry.invoke_ns)
                    .map(|(index, _)| index)
                    .collect()
            })
            .collect();
        let mut search = Search {
            entries,
            predecessors,
            done: vec![false; entries.len()],
            memo: HashSet::new(),
            deepest: None,
        };
        let found = search.step(initial.clone(), 0);
        found.ok_or_else(|| {
            search.deepest.take().unwrap_or_else(|| DeadEnd {
                linearized: 0,
                state: initial,
                eligible: entries.to_vec(),
            })
        })
    }

    fn eligible(&self) -> Vec<usize> {
        (0..self.entries.len())
            .filter(|&index| {
                !self.done[index]
                    && self.predecessors[index]
                        .iter()
                        .all(|&earlier| self.done[earlier])
            })
            .collect()
    }

    fn step(&mut self, state: KeyState, linearized: usize) -> Option<KeyState> {
        if linearized == self.entries.len() {
            return Some(state);
        }
        if !self.memo.insert((self.done.clone(), state.clone())) {
            return None;
        }
        let eligible = self.eligible();
        let mut advanced = false;
        for &index in &eligible {
            for next in successors(&state, &self.entries[index]) {
                advanced = true;
                self.done[index] = true;
                let found = self.step(next, linearized + 1);
                self.done[index] = false;
                if found.is_some() {
                    return found;
                }
            }
        }
        if !advanced
            && self
                .deepest
                .as_ref()
                .is_none_or(|deepest| deepest.linearized < linearized)
        {
            self.deepest = Some(DeadEnd {
                linearized,
                state,
                eligible: eligible
                    .iter()
                    .map(|&index| self.entries[index].clone())
                    .collect(),
            });
        }
        None
    }
}

/// Partition the merged history by issue and linearize every partition.
/// `seeded` holds the state of issues that existed before recording started;
/// every other issue starts absent and must be created by its first entry.
fn check_histories(
    entries: &[Entry],
    seeded: &BTreeMap<String, KeyState>,
) -> Result<BTreeMap<String, KeyState>, Box<Violation>> {
    let mut partitions: BTreeMap<String, Vec<Entry>> = BTreeMap::new();
    for entry in entries {
        partitions
            .entry(entry.key.clone())
            .or_default()
            .push(entry.clone());
    }
    let mut finals = BTreeMap::new();
    for (key, mut history) in partitions {
        history.sort_by_key(|entry| (entry.invoke_ns, entry.return_ns));
        let initial = seeded.get(&key).cloned().unwrap_or_default();
        match Search::run(&history, initial) {
            Ok(state) => {
                finals.insert(key, state);
            }
            Err(dead_end) => return Err(Box::new(Violation { key, dead_end })),
        }
    }
    Ok(finals)
}

// ---------------------------------------------------------------------------
// Process harness
// ---------------------------------------------------------------------------

struct Harness {
    root: PathBuf,
    binary: PathBuf,
    origin: Instant,
}

fn scrub_inherited_br_env(cmd: &mut Command) {
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if name.starts_with("BD_")
            || name.starts_with("BEADS_")
            || matches!(
                name.as_ref(),
                "BR_DISABLE_READ_ONLY_FAST_OPEN"
                    | "BR_OUTPUT_FORMAT"
                    | "TOON_DEFAULT_FORMAT"
                    | "TOON_STATS"
            )
        {
            cmd.env_remove(&key);
        }
    }
}

fn elapsed_ns(origin: Instant) -> u64 {
    u64::try_from(origin.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// The diagnostic a failed invocation left behind: stderr, or the JSON error
/// envelope that `--json` mode routes to stdout.
fn stderr_excerpt(output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut excerpt = if stderr.trim().is_empty() {
        stdout.trim().to_string()
    } else {
        stderr.trim().to_string()
    };
    excerpt.truncate(400);
    format!("exit {:?}: {excerpt}", output.status.code())
}

/// `br close` reports a batch in which every requested issue was skipped as
/// exit 3 with the `NOTHING_TO_DO` envelope on stdout; the per-issue reason
/// rides in `context.reason` as `all 1 issue(s) skipped — <id>: <reason>`.
fn skipped_reason_from_error(output: &Output, key: &str) -> Option<String> {
    json_documents(output).iter().find_map(|document| {
        let error = document.get("error")?;
        if error.get("code").and_then(Value::as_str) != Some("NOTHING_TO_DO") {
            return None;
        }
        let reason = error.pointer("/context/reason").and_then(Value::as_str)?;
        let (_, tail) = reason.split_once(&format!("{key}: "))?;
        Some(tail.to_string())
    })
}

fn json_payload(output: &Output) -> Option<Value> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(&common::cli::extract_json_payload(&stdout)).ok()
}

/// Every JSON document on stdout, in order. `br close --json` prints the
/// skip report and then the error envelope as two documents when every
/// requested issue was skipped, so a single-document parse would see neither.
fn json_documents(output: &Output) -> Vec<Value> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let payload = common::cli::extract_json_payload(&stdout);
    serde_json::Deserializer::from_str(&payload)
        .into_iter::<Value>()
        .map_while(Result::ok)
        .collect()
}

fn lists_id(list: &[Value], key: &str) -> bool {
    list.iter().any(|entry| {
        entry.as_str() == Some(key) || entry.get("id").and_then(Value::as_str) == Some(key)
    })
}

/// Classify a `close`/`reopen` report: `acted_list` is the key the command
/// uses for the issues it acted on (`closed` / `reopened`).
fn classify_report(output: &Output, key: &str, acted_list: &str) -> Outcome {
    let documents = json_documents(output);
    let skipped_reason = documents.iter().find_map(|document| {
        document
            .get("skipped")
            .and_then(Value::as_array)?
            .iter()
            .find(|entry| entry.get("id").and_then(Value::as_str) == Some(key))
            .map(|entry| {
                entry
                    .get("reason")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            })
    });
    if let Some(reason) = skipped_reason {
        return Outcome::Skipped(reason);
    }
    if !output.status.success() {
        return skipped_reason_from_error(output, key)
            .map_or_else(|| Outcome::Failed(stderr_excerpt(output)), Outcome::Skipped);
    }
    let acted = documents.iter().any(|document| match document {
        Value::Array(list) => lists_id(list, key),
        Value::Object(map) => map
            .get(acted_list)
            .and_then(Value::as_array)
            .is_some_and(|list| lists_id(list, key)),
        _ => false,
    });
    if acted {
        Outcome::Applied
    } else {
        Outcome::Failed(format!(
            "zero exit but {key} is in neither list: {documents:?}"
        ))
    }
}

fn parse_observation(output: &Output) -> Outcome {
    if !output.status.success() {
        return Outcome::Failed(stderr_excerpt(output));
    }
    let Some(value) = json_payload(output) else {
        return Outcome::Failed("show printed no JSON".to_string());
    };
    // `IssueDetails` flattens the issue's own fields to the top level and
    // omits empty relation lists.
    let details = value
        .as_array()
        .and_then(|list| list.first())
        .cloned()
        .unwrap_or(value);
    let issue = &details;
    let mut labels: Vec<String> = details["labels"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    labels.sort();
    let mut depends_on: Vec<String> = details["dependencies"]
        .as_array()
        .map(|list| {
            list.iter()
                .filter_map(|dep| dep.get("id").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    depends_on.sort();
    let priority = issue["priority"]
        .as_i64()
        .and_then(|value| i32::try_from(value).ok());
    match (issue["status"].as_str(), priority) {
        (Some(status), Some(priority)) => Outcome::Observed(Observation {
            status: status.to_string(),
            priority,
            labels,
            comments: details["comments"].as_array().map_or(0, std::vec::Vec::len),
            depends_on,
        }),
        _ => Outcome::Failed(format!("show payload lacks status/priority: {details}")),
    }
}

fn plain_outcome(output: &Output) -> Outcome {
    if output.status.success() {
        Outcome::Applied
    } else {
        Outcome::Failed(stderr_excerpt(output))
    }
}

impl Harness {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            binary: PathBuf::from(assert_cmd::cargo::cargo_bin!("br")),
            origin: Instant::now(),
        }
    }

    fn command(&self, args: &[String]) -> Command {
        let mut cmd = Command::new(&self.binary);
        cmd.current_dir(&self.root).args(args);
        scrub_inherited_br_env(&mut cmd);
        cmd.env("NO_COLOR", "1")
            .env("RUST_LOG", "error")
            .env("HOME", &self.root)
            .env("PATH", common::cli::deduplicated_br_path());
        cmd
    }

    fn run(&self, args: &[&str]) -> Output {
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
        self.command(&args)
            .output()
            .unwrap_or_else(|error| panic!("spawn br {}: {error}", args.join(" ")))
    }

    fn run_ok(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "br {} failed: {}",
            args.join(" "),
            stderr_excerpt(&output)
        );
        output
    }

    fn create_issue(&self, title: &str, priority: i32) -> String {
        let output = self.run_ok(&[
            "create",
            "--title",
            title,
            "--priority",
            &priority.to_string(),
            "--json",
        ]);
        created_id(&output).unwrap_or_else(|| {
            panic!(
                "create printed no id: {}",
                String::from_utf8_lossy(&output.stdout)
            )
        })
    }

    fn init_workspace(&self) {
        self.run_ok(&["init", "--prefix", "lz"]);
    }

    fn seed_pool(&self) -> Pool {
        let sources: Vec<String> = (0..SOURCE_ISSUES)
            .map(|index| self.create_issue(&format!("source {index}"), 2))
            .collect();
        let sinks: Vec<String> = (0..SINK_ISSUES)
            .map(|index| self.create_issue(&format!("sink {index}"), 2))
            .collect();
        let seeded = sources.iter().chain(sinks.iter()).cloned().collect();
        Pool {
            sources,
            sinks,
            seeded,
        }
    }

    /// Run one operation as its own `br` process and record it.
    fn execute(&self, pid: usize, seq: usize, key: &str, op: Op) -> Entry {
        let target = key.to_string();
        let args: Vec<String> = match &op {
            Op::Create { title, priority } => vec![
                "create".into(),
                "--title".into(),
                title.clone(),
                "--priority".into(),
                priority.to_string(),
                "--json".into(),
            ],
            Op::Close => vec![
                "close".into(),
                target.clone(),
                "--reason".into(),
                "linearizability".into(),
                "--json".into(),
            ],
            Op::Reopen => vec!["reopen".into(), target.clone(), "--json".into()],
            Op::SetPriority(priority) => vec![
                "update".into(),
                target.clone(),
                "--priority".into(),
                priority.to_string(),
                "--json".into(),
            ],
            Op::LabelAdd(label) => {
                vec!["label".into(), "add".into(), target.clone(), label.clone()]
            }
            Op::LabelRemove(label) => {
                vec![
                    "label".into(),
                    "remove".into(),
                    target.clone(),
                    label.clone(),
                ]
            }
            Op::CommentAdd => vec![
                "comments".into(),
                "add".into(),
                target.clone(),
                "--author".into(),
                format!("p{pid}"),
                "--message".into(),
                format!("c {pid}-{seq}"),
            ],
            Op::DepAdd(other) => vec!["dep".into(), "add".into(), target.clone(), other.clone()],
            Op::DepRemove(other) => {
                vec!["dep".into(), "remove".into(), target.clone(), other.clone()]
            }
            Op::Show => vec!["show".into(), target.clone(), "--json".into()],
        };
        let invoke_ns = elapsed_ns(self.origin);
        let output = self
            .command(&args)
            .output()
            .unwrap_or_else(|error| panic!("spawn br {}: {error}", args.join(" ")));
        let return_ns = elapsed_ns(self.origin);
        let (key, outcome) = match &op {
            Op::Create { .. } => match created_id(&output) {
                Some(id) if output.status.success() => (id, Outcome::Applied),
                _ => (String::new(), Outcome::Failed(stderr_excerpt(&output))),
            },
            Op::Close => (target, classify_report(&output, key, "closed")),
            Op::Reopen => (target, classify_report(&output, key, "reopened")),
            Op::Show => (target, parse_observation(&output)),
            _ => (target, plain_outcome(&output)),
        };
        Entry {
            pid,
            seq,
            key,
            invoke_ns,
            return_ns,
            op,
            outcome,
        }
    }
}

fn created_id(output: &Output) -> Option<String> {
    let stdout = String::from_utf8_lossy(&output.stdout);
    json_payload(output)
        .and_then(|value| value.get("id").and_then(Value::as_str).map(str::to_string))
        .or_else(|| {
            let id = common::cli::parse_created_id(&stdout);
            (!id.is_empty()).then_some(id)
        })
}

// ---------------------------------------------------------------------------
// Workload
// ---------------------------------------------------------------------------

/// Issues every worker may touch. `sinks` never gain outgoing edges, so edges
/// (always `source -> sink`) can never form a cycle.
#[derive(Default)]
struct Pool {
    sources: Vec<String>,
    sinks: Vec<String>,
    /// Issues created before recording started (open, priority 2, no
    /// labels, comments, or edges); the checker starts them in that state.
    seeded: BTreeSet<String>,
}

struct Xorshift(u64);

impl Xorshift {
    fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, bound: usize) -> usize {
        let bound = u64::try_from(bound.max(1)).unwrap_or(u64::MAX);
        usize::try_from(self.next() % bound).unwrap_or(0)
    }

    fn pick<'a>(&mut self, items: &'a [String]) -> &'a str {
        &items[self.below(items.len())]
    }
}

/// Choose the next operation; returns the issue it targets (empty for a
/// create, whose key is only known once it returns).
fn choose_op(
    rng: &mut Xorshift,
    pid: usize,
    seq: usize,
    sources: &[String],
    sinks: &[String],
) -> (String, Op) {
    let all: Vec<String> = sources.iter().chain(sinks.iter()).cloned().collect();
    let roll = rng.below(100);
    let priority = i32::try_from(rng.below(5)).unwrap_or(2);
    let label = LABELS[rng.below(LABELS.len())].to_string();
    match roll {
        0..=34 => (rng.pick(&all).to_string(), Op::Show),
        35..=42 => (rng.pick(&all).to_string(), Op::Close),
        43..=50 => (rng.pick(&all).to_string(), Op::Reopen),
        51..=60 => (rng.pick(&all).to_string(), Op::SetPriority(priority)),
        61..=68 => (rng.pick(&all).to_string(), Op::LabelAdd(label)),
        69..=73 => (rng.pick(&all).to_string(), Op::LabelRemove(label)),
        74..=81 => (rng.pick(&all).to_string(), Op::CommentAdd),
        82..=89 => (
            rng.pick(sources).to_string(),
            Op::DepAdd(rng.pick(sinks).to_string()),
        ),
        90..=94 => (
            rng.pick(sources).to_string(),
            Op::DepRemove(rng.pick(sinks).to_string()),
        ),
        _ => (
            String::new(),
            Op::Create {
                title: format!("lz {pid}-{seq}"),
                priority,
            },
        ),
    }
}

fn worker(pid: usize, harness: &Harness, pool: &Mutex<Pool>, deadline: Instant) -> Vec<Entry> {
    let mut rng = Xorshift::new(
        0x5DEE_CE66_D1CE_5EED
            ^ (u64::try_from(pid).unwrap_or(0) + 1).wrapping_mul(0x9E37_79B9_7F4A_7C15),
    );
    let mut history = Vec::new();
    let mut seq = 0;
    while Instant::now() < deadline {
        let (sources, sinks) = {
            let pool = pool.lock().expect("pool mutex");
            (pool.sources.clone(), pool.sinks.clone())
        };
        let (key, op) = choose_op(&mut rng, pid, seq, &sources, &sinks);
        let entry = harness.execute(pid, seq, &key, op);
        if matches!(entry.op, Op::Create { .. }) && !entry.key.is_empty() {
            pool.lock()
                .expect("pool mutex")
                .sources
                .push(entry.key.clone());
        }
        history.push(entry);
        seq += 1;
    }
    history
}

fn knob(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

// ---------------------------------------------------------------------------
// Post-run checks
// ---------------------------------------------------------------------------

fn jsonl_observations(path: &Path) -> BTreeMap<String, Observation> {
    let text = fs::read_to_string(path).expect("read published JSONL");
    text.lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let issue: Value = serde_json::from_str(line).expect("JSONL line");
            let id = issue["id"].as_str().expect("JSONL id").to_string();
            let mut labels: Vec<String> = issue["labels"]
                .as_array()
                .map(|list| {
                    list.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            labels.sort();
            let mut depends_on: Vec<String> = issue["dependencies"]
                .as_array()
                .map(|list| {
                    list.iter()
                        .filter_map(|dep| dep.get("depends_on_id").and_then(Value::as_str))
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            depends_on.sort();
            let observation = Observation {
                status: issue["status"].as_str().unwrap_or_default().to_string(),
                priority: issue["priority"]
                    .as_i64()
                    .and_then(|value| i32::try_from(value).ok())
                    .unwrap_or(i32::MIN),
                labels,
                comments: issue["comments"].as_array().map_or(0, std::vec::Vec::len),
                depends_on,
            };
            (id, observation)
        })
        .collect()
}

struct DatabaseFacts {
    integrity: String,
    issue_rows: i64,
    max_rowid: i64,
}

fn database_facts(db_path: &Path) -> DatabaseFacts {
    let conn = Connection::open(db_path.to_string_lossy().into_owned()).expect("open raw db");
    let integrity = conn
        .query("PRAGMA integrity_check")
        .expect("integrity_check")
        .first()
        .and_then(|row| row.get(0))
        .and_then(SqliteValue::as_text)
        .unwrap_or("<no row>")
        .to_string();
    let counts = conn
        .query("SELECT COUNT(*), COALESCE(MAX(rowid), 0) FROM issues")
        .expect("count issues");
    let integer = |column: usize| {
        counts
            .first()
            .and_then(|row| row.get(column))
            .and_then(SqliteValue::as_integer)
            .unwrap_or(-1)
    };
    let facts = DatabaseFacts {
        integrity,
        issue_rows: integer(0),
        max_rowid: integer(1),
    };
    conn.close().expect("close raw db");
    facts
}

fn persist_violation(dir: &Path, entries: &[Entry], violation: &Violation) {
    fs::create_dir_all(dir).expect("artifact dir");
    let mut history = String::new();
    for entry in entries {
        history.push_str(&serde_json::to_string(entry).expect("serialize entry"));
        history.push('\n');
    }
    fs::write(dir.join("history.jsonl"), history).expect("write history");
    fs::write(
        dir.join("violation.json"),
        serde_json::to_string_pretty(violation).expect("serialize violation"),
    )
    .expect("write violation");
}

fn describe(entry: &Entry) -> String {
    format!(
        "p{} #{} {} [{}..{} ms] -> {:?}",
        entry.pid,
        entry.seq,
        entry.op.name(),
        entry.invoke_ns / 1_000_000,
        entry.return_ns / 1_000_000,
        entry.outcome
    )
}

#[test]
#[allow(clippy::too_many_lines)]
fn concurrent_br_histories_are_linearizable_and_match_the_published_jsonl() {
    let processes =
        usize::try_from(knob("BR_LINEARIZABILITY_PROCESSES", DEFAULT_PROCESSES)).unwrap_or(8);
    let seconds = knob("BR_LINEARIZABILITY_SECONDS", DEFAULT_SECONDS);
    let temp = TempDir::new_in(common::cli::isolated_temp_root()).expect("temp workspace");
    let harness = Harness::new(temp.path().to_path_buf());
    harness.init_workspace();
    let pool = Mutex::new(harness.seed_pool());

    let deadline = Instant::now() + Duration::from_secs(seconds);
    let harness_ref = &harness;
    let pool_ref = &pool;
    let mut entries: Vec<Entry> = std::thread::scope(|scope| {
        // Spawn every worker before joining any, or the streams would run
        // one after another instead of concurrently.
        let mut handles = Vec::with_capacity(processes);
        for pid in 0..processes {
            handles.push(scope.spawn(move || worker(pid, harness_ref, pool_ref, deadline)));
        }
        handles
            .into_iter()
            .flat_map(|handle| handle.join().expect("worker thread"))
            .collect()
    });
    entries.sort_by_key(|entry| (entry.invoke_ns, entry.pid, entry.seq));

    let dropped_creates = entries.iter().filter(|entry| entry.key.is_empty()).count();
    entries.retain(|entry| !entry.key.is_empty());
    let mut per_op: BTreeMap<&str, usize> = BTreeMap::new();
    for entry in &entries {
        *per_op.entry(entry.op.name()).or_default() += 1;
    }
    let failed: Vec<&Entry> = entries
        .iter()
        .filter(|entry| matches!(entry.outcome, Outcome::Failed(_)))
        .collect();
    let keys: BTreeSet<&str> = entries.iter().map(|entry| entry.key.as_str()).collect();
    eprintln!(
        "[linearizability] processes={processes} seconds={seconds} operations={} keys={} failed={} dropped_creates={dropped_creates} mix={per_op:?}",
        entries.len(),
        keys.len(),
        failed.len()
    );
    assert!(
        entries.len() >= MIN_OPERATIONS,
        "workload too small to mean anything: {} operations",
        entries.len()
    );
    let mut failure_kinds: BTreeMap<String, usize> = BTreeMap::new();
    for entry in &failed {
        if let Outcome::Failed(message) = &entry.outcome {
            let mut kind = message.clone();
            kind.truncate(120);
            *failure_kinds
                .entry(format!("{}: {kind}", entry.op.name()))
                .or_default() += 1;
        }
    }
    assert!(
        failed.is_empty(),
        "{} br invocations failed; kinds: {failure_kinds:#?}\nfirst few:\n{}",
        failed.len(),
        failed
            .iter()
            .take(5)
            .map(|entry| describe(entry))
            .collect::<Vec<_>>()
            .join("\n")
    );

    let seeded: BTreeMap<String, KeyState> = {
        let pool = pool.lock().expect("pool mutex");
        pool.sources
            .iter()
            .chain(pool.sinks.iter())
            .filter(|id| pool.seeded.contains(*id))
            .map(|id| {
                (
                    id.clone(),
                    KeyState {
                        exists: true,
                        status: "open".to_string(),
                        priority: 2,
                        ..KeyState::default()
                    },
                )
            })
            .collect()
    };
    let finals = match check_histories(&entries, &seeded) {
        Ok(finals) => finals,
        Err(violation) => {
            let artifact_dir = std::env::var_os("BR_LINEARIZABILITY_ARTIFACT_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| temp.path().join("linearizability"));
            persist_violation(&artifact_dir, &entries, &violation);
            let kept = temp.keep();
            panic!(
                "history of {} is not linearizable: {} of its entries linearize (state {:?}); every remaining eligible entry contradicts the model:\n{}\nmerged history and violation written under {} (workspace kept at {})",
                violation.key,
                violation.dead_end.linearized,
                violation.dead_end.state,
                violation
                    .dead_end
                    .eligible
                    .iter()
                    .map(describe)
                    .collect::<Vec<_>>()
                    .join("\n"),
                artifact_dir.display(),
                kept.display()
            );
        }
    };

    harness.run_ok(&["sync", "--flush-only"]);
    let published = jsonl_observations(&temp.path().join(".beads").join("issues.jsonl"));
    let mut mismatches = Vec::new();
    for (key, state) in &finals {
        if !state.exists {
            continue;
        }
        match published.get(key) {
            Some(observed) if *observed == state.observation() => {}
            other => mismatches.push(format!(
                "{key}: linearized {:?} but JSONL has {other:?}",
                state.observation()
            )),
        }
    }
    assert!(
        mismatches.is_empty(),
        "published JSONL diverges from the linearized final state:\n{}",
        mismatches.join("\n")
    );

    let facts = database_facts(&temp.path().join(".beads").join("beads.db"));
    assert_eq!(facts.integrity, "ok", "integrity_check after the run");
    assert_eq!(
        facts.issue_rows, facts.max_rowid,
        "issues rowids must be dense after concurrent inserts"
    );
    let live_keys = finals.values().filter(|state| state.exists).count();
    assert_eq!(
        usize::try_from(facts.issue_rows).unwrap_or(0),
        live_keys,
        "every linearized issue is a row and nothing else was inserted"
    );
}

/// The live planted negative: one process stream claims a successful close it
/// never ran, on an issue only it knows about, then reads the issue back.
/// Honest streams run alongside so the run is a real multi-process history.
#[test]
fn a_process_that_reports_success_without_writing_is_caught() {
    let temp = TempDir::new_in(common::cli::isolated_temp_root()).expect("temp workspace");
    let harness = Harness::new(temp.path().to_path_buf());
    harness.init_workspace();
    let pool = Mutex::new(harness.seed_pool());
    let deadline = Instant::now() + Duration::from_secs(4);
    let harness_ref = &harness;
    let pool_ref = &pool;

    let (liar_history, mut entries): (Vec<Entry>, Vec<Entry>) = std::thread::scope(|scope| {
        let mut honest = Vec::new();
        for pid in 1..3 {
            honest.push(scope.spawn(move || worker(pid, harness_ref, pool_ref, deadline)));
        }
        let liar = scope.spawn(move || {
            let created = harness_ref.execute(
                0,
                0,
                "",
                Op::Create {
                    title: "liar's issue".to_string(),
                    priority: 2,
                },
            );
            let key = created.key.clone();
            assert!(!key.is_empty(), "the liar's create must succeed");
            let invoke_ns = elapsed_ns(harness_ref.origin);
            let fake_close = Entry {
                pid: 0,
                seq: 1,
                key: key.clone(),
                invoke_ns,
                return_ns: invoke_ns + 1,
                op: Op::Close,
                outcome: Outcome::Applied,
            };
            let readback = harness_ref.execute(0, 2, &key, Op::Show);
            vec![created, fake_close, readback]
        });
        let honest: Vec<Entry> = honest
            .into_iter()
            .flat_map(|handle| handle.join().expect("honest worker"))
            .collect();
        (liar.join().expect("liar worker"), honest)
    });

    let liar_key = liar_history[0].key.clone();
    entries.retain(|entry| !entry.key.is_empty());
    entries.extend(liar_history);
    let seeded: BTreeMap<String, KeyState> = {
        let pool = pool.lock().expect("pool mutex");
        pool.seeded
            .iter()
            .map(|id| {
                (
                    id.clone(),
                    KeyState {
                        exists: true,
                        status: "open".to_string(),
                        priority: 2,
                        ..KeyState::default()
                    },
                )
            })
            .collect()
    };
    let violation = check_histories(&entries, &seeded)
        .expect_err("a close that never happened must not linearize");
    assert_eq!(violation.key, liar_key);
    assert_eq!(
        violation.dead_end.linearized, 2,
        "the create and the fake close linearize; the read-back cannot"
    );
    assert!(
        violation
            .dead_end
            .eligible
            .iter()
            .all(|entry| matches!(entry.op, Op::Show)),
        "the dead end must be the honest read-back: {:?}",
        violation.dead_end.eligible
    );
    let honest_only: Vec<Entry> = entries
        .iter()
        .filter(|entry| entry.key != liar_key)
        .cloned()
        .collect();
    check_histories(&honest_only, &seeded).expect("the honest streams stay linearizable");
}

// ---------------------------------------------------------------------------
// Checker self-tests on synthetic histories (the planted negatives)
// ---------------------------------------------------------------------------

mod checker {
    use super::{Entry, KeyState, Observation, Op, Outcome, Violation, check_histories};
    use std::collections::BTreeMap;

    /// Every synthetic history creates its own issue, so nothing is seeded.
    fn check(entries: &[Entry]) -> Result<BTreeMap<String, KeyState>, Box<Violation>> {
        check_histories(entries, &BTreeMap::new())
    }

    fn entry(pid: usize, invoke_ns: u64, return_ns: u64, op: Op, outcome: Outcome) -> Entry {
        Entry {
            pid,
            seq: 0,
            key: "lz-1".to_string(),
            invoke_ns,
            return_ns,
            op,
            outcome,
        }
    }

    fn created() -> Entry {
        entry(
            0,
            0,
            1,
            Op::Create {
                title: "t".to_string(),
                priority: 2,
            },
            Outcome::Applied,
        )
    }

    fn seen(status: &str, comments: usize) -> Outcome {
        Outcome::Observed(Observation {
            status: status.to_string(),
            priority: 2,
            comments,
            ..Observation::default()
        })
    }

    fn final_state(entries: &[Entry]) -> KeyState {
        check(entries)
            .expect("linearizable")
            .remove("lz-1")
            .expect("key state")
    }

    #[test]
    fn concurrent_close_and_show_linearize_in_either_order() {
        for observed in ["open", "closed"] {
            let history = [
                created(),
                entry(1, 10, 20, Op::Close, Outcome::Applied),
                entry(2, 15, 25, Op::Show, seen(observed, 0)),
            ];
            assert_eq!(
                final_state(&history).status,
                "closed",
                "observed {observed}"
            );
        }
    }

    #[test]
    fn a_success_report_that_never_took_effect_is_detected() {
        let history = [
            created(),
            entry(1, 10, 20, Op::Close, Outcome::Applied),
            entry(2, 30, 40, Op::Show, seen("open", 0)),
        ];
        let violation = check(&history).expect_err("stale read must fail");
        assert_eq!(violation.key, "lz-1");
        assert_eq!(violation.dead_end.linearized, 2);
        assert!(
            matches!(violation.dead_end.eligible.as_slice(), [only] if only.pid == 2),
            "the show is the only eligible entry at the dead end: {:?}",
            violation.dead_end.eligible
        );
    }

    #[test]
    fn a_lost_comment_is_detected() {
        let history = [
            created(),
            entry(1, 10, 20, Op::CommentAdd, Outcome::Applied),
            entry(2, 30, 40, Op::Show, seen("open", 0)),
        ];
        assert!(check(&history).is_err());
        let consistent = [
            created(),
            entry(1, 10, 20, Op::CommentAdd, Outcome::Applied),
            entry(2, 30, 40, Op::Show, seen("open", 1)),
        ];
        assert_eq!(final_state(&consistent).comments, 1);
    }

    #[test]
    fn a_skip_that_contradicts_the_state_is_detected() {
        let history = [
            created(),
            entry(
                1,
                10,
                20,
                Op::Close,
                Outcome::Skipped("already closed".to_string()),
            ),
        ];
        assert!(check(&history).is_err());
        let consistent = [
            created(),
            entry(1, 10, 20, Op::Close, Outcome::Applied),
            entry(
                2,
                30,
                40,
                Op::Close,
                Outcome::Skipped("already closed".to_string()),
            ),
        ];
        assert_eq!(final_state(&consistent).status, "closed");
    }

    #[test]
    fn a_failed_mutation_may_have_landed_or_not() {
        for observed in ["open", "closed"] {
            let history = [
                created(),
                entry(1, 10, 20, Op::Close, Outcome::Failed("lock".to_string())),
                entry(2, 30, 40, Op::Show, seen(observed, 0)),
            ];
            assert_eq!(final_state(&history).status, observed);
        }
    }

    #[test]
    fn real_time_order_is_enforced_only_between_non_overlapping_calls() {
        // The show returned before the close was invoked, so it must precede
        // the close and cannot have seen it.
        let history = [
            created(),
            entry(2, 5, 8, Op::Show, seen("closed", 0)),
            entry(1, 10, 20, Op::Close, Outcome::Applied),
        ];
        assert!(check(&history).is_err());
    }
}
