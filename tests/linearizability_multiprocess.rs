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
//! After the workers stop, a quiescent read pass appends one `show --json`
//! observation per issue to the history, so a linearization must end in the
//! state the database actually reached (two overlapping writes with no later
//! read would otherwise leave more than one valid end state). The database
//! must then pass `PRAGMA integrity_check`, its rowids must be dense, and the
//! JSONL published by `br sync --flush-only` must match that final state.
//!
//! Knobs: `BR_LINEARIZABILITY_PROCESSES` (default 8),
//! `BR_LINEARIZABILITY_SECONDS` (default 30), and
//! `BR_LINEARIZABILITY_ARTIFACT_DIR` (where the merged history and the failing
//! partition are written on a violation; default: the kept temp workspace).
//! The separate `coupled` cases keep one state for a shared capacity pool and
//! typed dependency graph. `BR_COUPLED_CASES` and `BR_COUPLED_SEED` replay bounded
//! schedules; `BR_COUPLED_SEARCH_BUDGET` limits search without treating an
//! exhausted search as success.

mod common;

use beads_rust::franken_sync::compat::{OpenFlags, open_with_flags};
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
    let conn = open_with_flags(&db_path.to_string_lossy(), OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("open raw db");
    let integrity_rows = conn
        .query("PRAGMA integrity_check")
        .expect("integrity_check");
    let values: Vec<_> = integrity_rows
        .iter()
        .map(|row| row.values().to_vec())
        .collect();
    let integrity = integrity_result(&values);
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

fn integrity_result(rows: &[Vec<SqliteValue>]) -> String {
    match rows {
        [row] if row.len() == 1 && row[0].as_text() == Some("ok") => "ok".into(),
        _ => format!("unexpected PRAGMA integrity_check rows: {rows:?}"),
    }
}

#[test]
fn integrity_control_rejects_trailing_corruption_and_malformed_rows() {
    assert_eq!(integrity_result(&[vec![SqliteValue::from("ok")]]), "ok");
    for rows in [
        vec![],
        vec![vec![]],
        vec![vec![SqliteValue::Null]],
        vec![vec![SqliteValue::from(0_i64)]],
        vec![vec![SqliteValue::from("OK")]],
        vec![vec![SqliteValue::from("ok"), SqliteValue::from("extra")]],
        vec![
            vec![SqliteValue::from("ok")],
            vec![SqliteValue::from("trailing corruption")],
        ],
    ] {
        let observed = integrity_result(&rows);
        assert_ne!(observed, "ok");
        assert!(observed.contains(&format!("{rows:?}")));
    }
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

    // Quiescent read pass: one `show` per issue after every worker has
    // returned, appended to the history under a pid of its own. Two
    // overlapping writes on a key with no later read admit more than one
    // valid end state; these reads pin the one the database reached, so the
    // checker's final states are observations rather than one of several
    // admissible orders (the JSONL comparison below relies on that).
    let final_keys: Vec<String> = keys.iter().map(|key| (*key).to_string()).collect();
    let quiescent_pid = processes;
    let mut unobserved = Vec::new();
    for (seq, key) in final_keys.iter().enumerate() {
        let entry = harness.execute(quiescent_pid, seq, key, Op::Show);
        if !matches!(entry.outcome, Outcome::Observed(_)) {
            unobserved.push(describe(&entry));
        }
        entries.push(entry);
    }
    eprintln!(
        "[linearizability] quiescent read pass: {} issues observed",
        final_keys.len()
    );
    assert!(
        unobserved.is_empty(),
        "every issue in the history must be observable once the workers have stopped:\n{}",
        unobserved.join("\n")
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

/// A deliberately small, independent specification for coupled histories.
/// These five issues share one capacity pool; no issue-ID partitioning or
/// unproved commutativity reduction is valid here.
mod coupled {
    use super::{Harness, SqliteValue, common, elapsed_ns, knob};
    use beads_rust::franken_sync::compat::{OpenFlags, open_with_flags};
    use beads_rust::util::hex_encode;
    use serde::{Deserialize, Serialize};
    use serde_json::Value;
    use sha2::Digest;
    use std::collections::{BTreeMap, BTreeSet, HashSet};
    use std::fs;
    use std::io::Read;
    use std::path::Path;
    use std::process::Stdio;
    use std::sync::Barrier;
    use std::time::{Duration, Instant};
    use tempfile::TempDir;

    const ISSUES: usize = 5;
    const CAPACITY: usize = 2;
    const SEARCH_BUDGET: usize = 50_000;
    const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

    fn parse_positive_setting(value: &str) -> Result<u64, &'static str> {
        value
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|value| *value > 0)
            .ok_or("expected a positive integer")
    }

    fn positive_setting(name: &str, default: u64) -> u64 {
        std::env::var(name).map_or(default, |value| {
            parse_positive_setting(&value)
                .unwrap_or_else(|reason| panic!("{name}={value:?}: {reason}"))
        })
    }

    fn refusal_exit(code: &str) -> Option<i32> {
        match code {
            "WORKFLOW_CAPACITY_EXCEEDED" | "CLAIM_BLOCKED" => Some(4),
            "CYCLE_DETECTED" => Some(5),
            "NOTHING_TO_DO" => Some(3),
            "ALREADY_OPEN" => Some(0),
            _ => None,
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
    enum EdgeKind {
        Blocks,
        Related,
    }

    impl EdgeKind {
        const fn argument(self) -> &'static str {
            match self {
                Self::Blocks => "blocks",
                Self::Related => "related",
            }
        }
    }

    #[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
    struct State {
        statuses: Vec<String>,
        edges: BTreeSet<(usize, usize, EdgeKind)>,
        /// Events caused by recorded operations, excluding fixture setup.
        events: BTreeMap<String, usize>,
    }

    impl State {
        fn initial() -> Self {
            let mut statuses = vec!["open".to_string(); ISSUES];
            statuses[0] = "in_progress".to_string();
            Self {
                statuses,
                edges: BTreeSet::new(),
                events: BTreeMap::new(),
            }
        }

        fn blocked(&self, issue: usize) -> bool {
            self.edges.iter().any(|&(from, to, kind)| {
                from == issue && kind == EdgeKind::Blocks && self.statuses[to] != "closed"
            })
        }

        fn ready(&self) -> BTreeSet<usize> {
            (0..ISSUES)
                .filter(|&id| self.statuses[id] != "closed" && !self.blocked(id))
                .collect()
        }

        fn reaches(&self, start: usize, target: usize) -> bool {
            let mut pending = vec![start];
            let mut visited = BTreeSet::new();
            while let Some(node) = pending.pop() {
                if node == target {
                    return true;
                }
                if visited.insert(node) {
                    pending.extend(self.edges.iter().filter_map(|&(from, to, kind)| {
                        (from == node && kind == EdgeKind::Blocks).then_some(to)
                    }));
                }
            }
            false
        }

        fn event(&mut self, issue: usize, kind: &str) {
            *self.events.entry(format!("{issue}:{kind}")).or_default() += 1;
        }

        fn transition(&self, op: &Operation) -> Result<Self, &'static str> {
            let mut next = self.clone();
            match *op {
                Operation::Claim(id) => {
                    if self.blocked(id) {
                        return Err("CLAIM_BLOCKED");
                    }
                    if self.statuses[id] != "in_progress"
                        && self
                            .statuses
                            .iter()
                            .filter(|status| *status == "in_progress")
                            .count()
                            >= CAPACITY
                    {
                        return Err("WORKFLOW_CAPACITY_EXCEEDED");
                    }
                    if self.statuses[id] != "in_progress" {
                        next.statuses[id] = "in_progress".to_string();
                        next.event(id, "status_changed");
                    }
                }
                Operation::Close(id) => {
                    if self.statuses[id] == "closed" || self.blocked(id) {
                        return Err("NOTHING_TO_DO");
                    }
                    next.statuses[id] = "closed".to_string();
                    next.event(id, "status_changed");
                    next.event(id, "closed");
                }
                Operation::Reopen(id) => {
                    if self.statuses[id] != "closed" {
                        return Err("ALREADY_OPEN");
                    }
                    next.statuses[id] = "open".to_string();
                    next.event(id, "status_changed");
                    next.event(id, "reopened");
                }
                Operation::AddEdge(from, to, kind) => {
                    if kind == EdgeKind::Blocks && self.reaches(to, from) {
                        return Err("CYCLE_DETECTED");
                    }
                    if next.edges.insert((from, to, kind)) {
                        next.event(from, "dependency_added");
                    }
                }
                Operation::Ready | Operation::Final => return Err("READ_ONLY"),
            }
            Ok(next)
        }
    }

    #[derive(Clone, Copy, Debug, Serialize, Deserialize)]
    enum Operation {
        Claim(usize),
        Close(usize),
        Reopen(usize),
        AddEdge(usize, usize, EdgeKind),
        Ready,
        Final,
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    enum ResultValue {
        Applied,
        Refused(String),
        /// A timeout or publication/transport failure can follow a commit.
        Uncertain,
        Ready(BTreeSet<usize>),
        Final(State),
    }

    #[derive(Clone, Debug, Serialize, Deserialize)]
    struct Call {
        pid: usize,
        seq: usize,
        invoke_ns: u64,
        return_ns: u64,
        /// Every recorded call can observe or affect the entire shared domain.
        domain: Vec<usize>,
        operation: Operation,
        result: ResultValue,
        exit: Option<i32>,
        timed_out: bool,
        stdout: String,
        stderr: String,
    }

    fn successors(state: &State, call: &Call) -> Vec<State> {
        match (&call.operation, &call.result) {
            (Operation::Ready, ResultValue::Ready(seen)) if state.ready() == *seen => {
                vec![state.clone()]
            }
            (Operation::Final, ResultValue::Final(seen)) if state == seen => vec![state.clone()],
            (_, ResultValue::Applied) => state.transition(&call.operation).into_iter().collect(),
            (_, ResultValue::Refused(code)) => match state.transition(&call.operation) {
                Err(expected) if expected == code => vec![state.clone()],
                _ => Vec::new(),
            },
            (_, ResultValue::Uncertain) => {
                let mut states = vec![state.clone()];
                if let Ok(next) = state.transition(&call.operation)
                    && next != *state
                {
                    states.push(next);
                }
                states
            }
            _ => Vec::new(),
        }
    }

    #[derive(Debug, Serialize)]
    enum Verdict {
        Valid(State),
        Invalid { matched: usize, invariant: String },
        Inconclusive { visited: usize, reason: String },
    }

    struct Search<'a> {
        calls: &'a [Call],
        predecessors: Vec<Vec<usize>>,
        done: Vec<bool>,
        memo: HashSet<(Vec<bool>, State)>,
        budget: usize,
        exhausted: bool,
        deepest: usize,
    }

    impl Search<'_> {
        fn check(calls: &[Call], initial: &State, budget: usize) -> Verdict {
            if calls.iter().any(|call| {
                let invalid_index = match call.operation {
                    Operation::Claim(id) | Operation::Close(id) | Operation::Reopen(id) => {
                        id >= ISSUES
                    }
                    Operation::AddEdge(from, to, _) => from >= ISSUES || to >= ISSUES,
                    Operation::Ready | Operation::Final => false,
                };
                let invalid_success = matches!(
                    call.result,
                    ResultValue::Applied | ResultValue::Ready(_) | ResultValue::Final(_)
                ) && (call.exit != Some(0) || call.timed_out);
                let invalid_refusal = match &call.result {
                    ResultValue::Refused(code) => call.timed_out || call.exit != refusal_exit(code),
                    _ => false,
                };
                call.invoke_ns > call.return_ns
                    || call.domain != (0..ISSUES).collect::<Vec<_>>()
                    || invalid_index
                    || invalid_success
                    || invalid_refusal
            }) {
                return Verdict::Invalid {
                    matched: 0,
                    invariant: "malformed interval or incomplete coupled domain".to_string(),
                };
            }
            let predecessors = calls
                .iter()
                .map(|call| {
                    calls
                        .iter()
                        .enumerate()
                        .filter_map(|(index, earlier)| {
                            let returned_before_invocation = earlier.return_ns < call.invoke_ns;
                            let same_writer_predecessor =
                                earlier.pid == call.pid && earlier.seq < call.seq;
                            (returned_before_invocation || same_writer_predecessor).then_some(index)
                        })
                        .collect()
                })
                .collect();
            let mut search = Search {
                calls,
                predecessors,
                done: vec![false; calls.len()],
                memo: HashSet::new(),
                budget,
                exhausted: false,
                deepest: 0,
            };
            if let Some(state) = search.step(initial.clone(), 0) {
                let unresolved = calls.iter().any(|call| {
                    matches!(call.result, ResultValue::Uncertain)
                        && !calls.iter().any(|later| {
                            matches!(later.result, ResultValue::Final(_))
                                && call.return_ns < later.invoke_ns
                        })
                });
                return if unresolved {
                    Verdict::Inconclusive {
                        visited: search.memo.len(),
                        reason: "uncertain commit has no later quiescent state/event observation"
                            .to_string(),
                    }
                } else {
                    Verdict::Valid(state)
                };
            }
            if search.exhausted {
                Verdict::Inconclusive {
                    visited: search.memo.len(),
                    reason: format!("search budget {budget} exhausted"),
                }
            } else {
                Verdict::Invalid {
                    matched: search.deepest,
                    invariant: "capacity, blocking graph, result, event or readiness observation contradicts every real-time order".to_string(),
                }
            }
        }

        fn step(&mut self, state: State, count: usize) -> Option<State> {
            self.deepest = self.deepest.max(count);
            if count == self.calls.len() {
                return Some(state);
            }
            if self.memo.contains(&(self.done.clone(), state.clone())) {
                return None;
            }
            if self.memo.len() >= self.budget {
                self.exhausted = true;
                return None;
            }
            self.memo.insert((self.done.clone(), state.clone()));
            for index in 0..self.calls.len() {
                if self.done[index]
                    || self.predecessors[index]
                        .iter()
                        .any(|&earlier| !self.done[earlier])
                {
                    continue;
                }
                for next in successors(&state, &self.calls[index]) {
                    self.done[index] = true;
                    let found = self.step(next, count + 1);
                    self.done[index] = false;
                    if found.is_some() || self.exhausted {
                        return found;
                    }
                }
            }
            None
        }
    }

    /// Delete-one minimization preserves an actual invalid verdict; exhaustion
    /// never qualifies a candidate as a smaller counterexample.
    fn minimize(calls: &[Call], initial: &State) -> Vec<Call> {
        let mut minimal = calls.to_vec();
        let mut index = 0;
        while index < minimal.len() {
            let mut candidate = minimal.clone();
            candidate.remove(index);
            if matches!(
                Search::check(&candidate, initial, SEARCH_BUDGET),
                Verdict::Invalid { .. }
            ) {
                minimal = candidate;
                index = 0;
            } else {
                index += 1;
            }
        }
        minimal
    }

    fn issue_index(ids: &[String], id: &str) -> usize {
        ids.iter()
            .position(|candidate| candidate == id)
            .expect("known issue in projection")
    }

    fn documents(stdout: &str) -> Vec<Value> {
        assert!(!stdout.contains('\u{1b}'), "ANSI in JSON stdout: {stdout}");
        serde_json::Deserializer::from_str(stdout)
            .into_iter::<Value>()
            .collect::<Result<Vec<_>, _>>()
            .unwrap_or_else(|error| panic!("invalid whole JSON stream: {error}: {stdout}"))
    }

    fn classify(call: &Call, ids: &[String]) -> ResultValue {
        if call.timed_out {
            return ResultValue::Uncertain;
        }
        let documents = documents(&call.stdout);
        if let Some(error) = documents.iter().find_map(|value| value.get("error")) {
            assert_ne!(
                call.exit,
                Some(0),
                "error envelope with success exit: {call:?}"
            );
            let code = error["code"].as_str().expect("error code");
            if code == "VALIDATION_FAILED"
                && matches!(call.operation, Operation::Claim(_))
                && error["context"]["field"] == "claim"
                && error["context"]["reason"]
                    .as_str()
                    .is_some_and(|reason| reason.starts_with("cannot claim blocked issue:"))
            {
                return ResultValue::Refused("CLAIM_BLOCKED".to_string());
            }
            return if matches!(
                code,
                "WORKFLOW_CAPACITY_EXCEEDED" | "CYCLE_DETECTED" | "NOTHING_TO_DO"
            ) {
                ResultValue::Refused(code.to_string())
            } else {
                ResultValue::Uncertain
            };
        }
        if call.exit != Some(0) {
            return ResultValue::Uncertain;
        }
        assert_eq!(
            documents.len(),
            1,
            "successful command must print one JSON document: {call:?}"
        );
        let value = &documents[0];
        match call.operation {
            Operation::Ready => ResultValue::Ready(
                value
                    .as_array()
                    .expect("ready array")
                    .iter()
                    .map(|issue| issue_index(ids, issue["id"].as_str().expect("ready id")))
                    .collect(),
            ),
            Operation::Reopen(id)
                if value["skipped"]
                    .as_array()
                    .is_some_and(|skipped| skipped.iter().any(|item| item["id"] == ids[id])) =>
            {
                ResultValue::Refused("ALREADY_OPEN".to_string())
            }
            Operation::Claim(id) => {
                assert_eq!(value[0]["id"], ids[id]);
                assert_eq!(value[0]["status"], "in_progress");
                ResultValue::Applied
            }
            Operation::Close(id) | Operation::Reopen(id) => {
                let (changed, expected_status) = if matches!(call.operation, Operation::Close(_)) {
                    // A successful close without skips/warnings is a bare
                    // array; the documented warnings form wraps `closed`.
                    (
                        value.as_array().or_else(|| value["closed"].as_array()),
                        "closed",
                    )
                } else {
                    (value["reopened"].as_array(), "open")
                };
                let changed = changed.expect("typed mutation result");
                assert_eq!(changed.len(), 1, "one requested mutation: {call:?}");
                assert_eq!(changed[0]["id"], ids[id], "{call:?}");
                assert_eq!(changed[0]["status"], expected_status, "{call:?}");
                ResultValue::Applied
            }
            Operation::AddEdge(from, to, kind) => {
                assert_eq!(value["issue_id"], ids[from]);
                assert_eq!(value["depends_on_id"], ids[to]);
                assert_eq!(value["type"], kind.argument());
                ResultValue::Applied
            }
            Operation::Final => panic!("final observations use quiescent reads"),
        }
    }

    fn execute(
        harness: &Harness,
        ids: &[String],
        pid: usize,
        seq: usize,
        operation: Operation,
        seed: u64,
    ) -> Call {
        let mut args: Vec<String> = match operation {
            Operation::Claim(id) => vec![
                "update".into(),
                ids[id].clone(),
                "--status".into(),
                "in_progress".into(),
            ],
            Operation::Close(id) => vec![
                "close".into(),
                ids[id].clone(),
                "--reason".into(),
                "coupled history verified".into(),
            ],
            Operation::Reopen(id) => vec!["reopen".into(), ids[id].clone()],
            Operation::AddEdge(from, to, kind) => vec![
                "dep".into(),
                "add".into(),
                ids[from].clone(),
                ids[to].clone(),
                "--type".into(),
                kind.argument().into(),
            ],
            Operation::Ready => vec!["ready".into(), "--limit".into(), "0".into()],
            Operation::Final => panic!("final observations run after writers join"),
        };
        args.extend([
            "--json".to_string(),
            "--actor".to_string(),
            format!("coupled-{seed}-{pid}-{seq}"),
        ]);
        let invoke_ns = elapsed_ns(harness.origin);
        let mut child = harness
            .command(&args)
            .env("BEADS_DIR", harness.root.join(".beads"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn coupled operation");
        let stdout = child.stdout.take().expect("stdout");
        let stderr = child.stderr.take().expect("stderr");
        let capture = |mut pipe: Box<dyn Read + Send>| {
            std::thread::spawn(move || {
                let mut text = String::new();
                pipe.read_to_string(&mut text).expect("read process output");
                text
            })
        };
        let stdout_thread = capture(Box::new(stdout));
        let stderr_thread = capture(Box::new(stderr));
        let deadline = Instant::now() + COMMAND_TIMEOUT;
        let mut timed_out = false;
        let status = loop {
            if let Some(status) = child.try_wait().expect("poll coupled operation") {
                break status;
            }
            if Instant::now() >= deadline {
                timed_out = true;
                child
                    .kill()
                    .expect("stop timed-out writer before final observation");
                break child.wait().expect("reap timed-out writer");
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let mut call = Call {
            pid,
            seq,
            invoke_ns,
            return_ns: elapsed_ns(harness.origin),
            domain: (0..ISSUES).collect(),
            operation,
            result: ResultValue::Uncertain,
            exit: status.code(),
            timed_out,
            stdout: stdout_thread.join().expect("stdout reader"),
            stderr: stderr_thread.join().expect("stderr reader"),
        };
        call.result = classify(&call, ids);
        eprintln!(
            "[coupled-call] {}",
            serde_json::to_string(&call).expect("trace")
        );
        call
    }

    fn phase(
        harness: &Harness,
        ids: &[String],
        schedules: &[Vec<Operation>],
        phase: usize,
        seed: u64,
    ) -> Vec<Call> {
        assert!((2..=4).contains(&schedules.len()));
        let barrier = Barrier::new(schedules.len());
        std::thread::scope(|scope| {
            let handles: Vec<_> = schedules
                .iter()
                .enumerate()
                .map(|(pid, operations)| {
                    let barrier = &barrier;
                    scope.spawn(move || {
                        barrier.wait();
                        operations
                            .iter()
                            .enumerate()
                            .map(|(seq, &operation)| {
                                // The seed chooses a reproducible launch skew; recorded
                                // intervals, rather than that choice, constrain search.
                                std::thread::sleep(Duration::from_millis(
                                    (seed.wrapping_add(pid as u64)) % 3,
                                ));
                                execute(harness, ids, pid, phase * 10 + seq, operation, seed)
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|handle| handle.join().expect("coupled worker"))
                .collect()
        })
    }

    fn read_final(harness: &Harness, ids: &[String], seed: u64) -> State {
        let mut args = vec!["show"];
        args.extend(ids.iter().map(String::as_str));
        args.push("--json");
        let shown = harness.run_ok(&args);
        let values: Value = serde_json::from_slice(&shown.stdout).expect("whole show JSON");
        let mut state = State {
            statuses: vec![String::new(); ISSUES],
            edges: BTreeSet::new(),
            events: BTreeMap::new(),
        };
        let issues = values.as_array().expect("show array");
        assert_eq!(issues.len(), ISSUES);
        for issue in issues {
            let id = issue_index(ids, issue["id"].as_str().expect("shown id"));
            state.statuses[id] = issue["status"].as_str().expect("shown status").to_string();
            for edge in issue["dependencies"].as_array().into_iter().flatten() {
                let to = issue_index(ids, edge["id"].as_str().expect("dependency id"));
                let kind = match edge["dependency_type"].as_str().expect("dependency type") {
                    "blocks" => EdgeKind::Blocks,
                    "related" => EdgeKind::Related,
                    other => panic!("unexpected dependency type: {other}"),
                };
                state.edges.insert((id, to, kind));
            }
        }
        let conn = open_with_flags(
            &harness.root.join(".beads/beads.db").to_string_lossy(),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .expect("quiescent database");
        let rows = conn
            .query_with_params(
                "SELECT issue_id, event_type FROM events WHERE actor LIKE ? ORDER BY id",
                &[SqliteValue::from(format!("coupled-{seed}-%"))],
            )
            .expect("quiescent events");
        for row in rows {
            let id = issue_index(
                ids,
                row.get(0)
                    .and_then(SqliteValue::as_text)
                    .expect("event issue"),
            );
            let kind = row
                .get(1)
                .and_then(SqliteValue::as_text)
                .expect("event type");
            state.event(id, kind);
        }
        conn.close().expect("close quiescent database");
        state
    }

    fn assert_export_matches(path: &Path, ids: &[String], state: &State) {
        let mut exported = state.clone();
        exported.statuses.fill(String::new());
        exported.edges.clear();
        let text = fs::read_to_string(path).expect("published JSONL");
        let mut count = 0;
        for line in text.lines().filter(|line| !line.is_empty()) {
            let issue: Value = serde_json::from_str(line).expect("whole JSONL record");
            let id = issue_index(ids, issue["id"].as_str().expect("exported id"));
            exported.statuses[id] = issue["status"]
                .as_str()
                .expect("exported status")
                .to_string();
            for edge in issue["dependencies"].as_array().into_iter().flatten() {
                let to = issue_index(
                    ids,
                    edge["depends_on_id"].as_str().expect("exported dependency"),
                );
                let kind = match edge["type"].as_str().expect("exported dependency type") {
                    "blocks" => EdgeKind::Blocks,
                    "related" => EdgeKind::Related,
                    other => panic!("unexpected exported edge: {other}"),
                };
                exported.edges.insert((id, to, kind));
            }
            count += 1;
        }
        assert_eq!(count, ISSUES);
        assert_eq!(
            &exported, state,
            "JSONL must match quiescent show projections"
        );
    }

    fn verify_observed_capabilities_and_export(
        harness: &Harness,
        ids: &[String],
        calls: &[Call],
        observed: &State,
        seed: u64,
    ) {
        for operation in ["claim", "edge", "close", "reopen", "ready"] {
            assert!(
                calls.iter().any(|call| {
                    matches!(
                        (&call.operation, &call.result, operation),
                        (Operation::Claim(_), ResultValue::Applied, "claim")
                            | (Operation::AddEdge(_, _, _), ResultValue::Applied, "edge")
                            | (Operation::Close(_), ResultValue::Applied, "close")
                            | (Operation::Reopen(_), ResultValue::Applied, "reopen")
                            | (Operation::Ready, ResultValue::Ready(_), "ready")
                    )
                }),
                "no actual successful {operation} observation in seed {seed}"
            );
        }
        harness.run_ok(&["sync", "--flush-only", "--json"]);
        assert_export_matches(&harness.root.join(".beads/issues.jsonl"), ids, observed);
        assert_eq!(
            &read_final(harness, ids, seed),
            observed,
            "publication must not change issue/event state"
        );
    }

    fn replay_provenance(harness: &Harness, seed: u64) -> Value {
        serde_json::json!({
            "seed": seed, "engine": env!("BR_FSQLITE_VERSION"),
            "source_commit": option_env!("VERGEN_GIT_SHA"),
            "checker_sha256": hex_encode(&sha2::Sha256::digest(include_bytes!("linearizability_multiprocess.rs"))),
            "binary_sha256": hex_encode(&sha2::Sha256::digest(fs::read(&harness.binary).expect("compiled binary"))),
            "features": {"mcp": cfg!(feature = "mcp"), "self_update": cfg!(feature = "self_update")},
        })
    }

    fn run_case(seed: u64) {
        let temp = TempDir::new_in(common::cli::isolated_temp_root()).expect("coupled workspace");
        let harness = Harness::new(temp.path().to_path_buf());
        harness.init_workspace();
        let ids: Vec<_> = (0..ISSUES)
            .map(|index| harness.create_issue(&format!("coupled issue {index}"), 2))
            .collect();
        fs::write(temp.path().join(".beads/policy.yaml"), "workflow:\n  strict: true\n  statuses: [open, in_progress, closed]\n  status_groups:\n    ready: [open, in_progress]\n  capacity:\n    statuses:\n      in_progress:\n        hard: 2\n").expect("capacity policy");
        harness.run_ok(&["update", &ids[0], "--status", "in_progress", "--json"]);
        let initial = State::initial();
        let provenance = replay_provenance(&harness, seed);
        eprintln!("[coupled-replay] {provenance}");
        let mut calls = phase(
            &harness,
            &ids,
            &[
                vec![Operation::Claim(1)],
                vec![Operation::Claim(2)],
                vec![Operation::Ready],
                vec![
                    Operation::AddEdge(3, 0, EdgeKind::Related),
                    Operation::Ready,
                ],
            ],
            0,
            seed,
        );
        calls.extend(phase(
            &harness,
            &ids,
            &[
                vec![Operation::AddEdge(3, 4, EdgeKind::Blocks)],
                vec![Operation::AddEdge(4, 3, EdgeKind::Blocks)],
                vec![Operation::Ready],
            ],
            1,
            seed,
        ));
        calls.extend(phase(
            &harness,
            &ids,
            &[
                vec![Operation::Close(3), Operation::Reopen(3)],
                vec![Operation::Close(4), Operation::Reopen(4)],
                vec![Operation::Ready, Operation::Ready],
                vec![Operation::Close(0), Operation::Reopen(0)],
            ],
            2,
            seed,
        ));
        // All children have been joined or killed and reaped before any final
        // observation. Uncertain outcomes remain in the history for search.
        let invoke_ns = elapsed_ns(harness.origin);
        let observed = read_final(&harness, &ids, seed);
        let facts = super::database_facts(&temp.path().join(".beads/beads.db"));
        assert_eq!(
            facts.integrity, "ok",
            "coupled quiescent database integrity"
        );
        assert_eq!(facts.issue_rows, i64::try_from(ISSUES).unwrap());
        assert_eq!(facts.max_rowid, i64::try_from(ISSUES).unwrap());
        calls.push(Call {
            pid: 4,
            seq: 0,
            invoke_ns,
            return_ns: elapsed_ns(harness.origin),
            domain: (0..ISSUES).collect(),
            operation: Operation::Final,
            result: ResultValue::Final(observed.clone()),
            exit: Some(0),
            timed_out: false,
            stdout: String::new(),
            stderr: String::new(),
        });
        let budget = usize::try_from(positive_setting(
            "BR_COUPLED_SEARCH_BUDGET",
            SEARCH_BUDGET as u64,
        ))
        .expect("search budget");
        let verdict = Search::check(&calls, &initial, budget);
        if let Verdict::Valid(final_state) = &verdict {
            assert_eq!(final_state, &observed);
        }
        if !matches!(verdict, Verdict::Valid(_)) {
            let artifact = temp.path().join("coupled-history.json");
            let minimal = if matches!(verdict, Verdict::Invalid { .. }) {
                minimize(&calls, &initial)
            } else {
                calls.clone()
            };
            fs::write(&artifact, serde_json::to_string_pretty(&serde_json::json!({"provenance": provenance, "ids": ids, "initial": initial, "calls": calls, "minimized": minimal, "verdict": verdict})).expect("failure receipt")).expect("retain replay");
            let kept = temp.keep();
            panic!(
                "coupled history did not pass: {verdict:?}; replay at {} (workspace {})",
                artifact.display(),
                kept.display()
            );
        }
        verify_observed_capabilities_and_export(&harness, &ids, &calls, &observed, seed);
    }

    #[test]
    fn bounded_real_capacity_graph_and_ready_histories() {
        let seed = knob("BR_COUPLED_SEED", 8);
        for case in 0..positive_setting("BR_COUPLED_CASES", 2) {
            run_case(seed.wrapping_add(case));
        }
    }

    fn synthetic(
        pid: usize,
        interval: (u64, u64),
        operation: Operation,
        result: ResultValue,
    ) -> Call {
        let exit = match &result {
            ResultValue::Refused(code) => refusal_exit(code),
            _ => Some(0),
        };
        Call {
            pid,
            seq: 0,
            invoke_ns: interval.0,
            return_ns: interval.1,
            domain: (0..ISSUES).collect(),
            operation,
            result,
            exit,
            timed_out: false,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    fn checked(calls: &[Call]) -> Verdict {
        Search::check(calls, &State::initial(), SEARCH_BUDGET)
    }

    fn assert_valid(calls: &[Call]) {
        let verdict = checked(calls);
        assert!(matches!(verdict, Verdict::Valid(_)), "{verdict:?}");
    }

    fn assert_invalid(calls: &[Call]) {
        let verdict = checked(calls);
        assert!(matches!(verdict, Verdict::Invalid { .. }), "{verdict:?}");
    }

    #[test]
    fn coupled_capacity_one_winner_passes_but_two_winners_fail() {
        let first = synthetic(0, (10, 30), Operation::Claim(1), ResultValue::Applied);
        let mut second = synthetic(
            1,
            (15, 25),
            Operation::Claim(2),
            ResultValue::Refused("WORKFLOW_CAPACITY_EXCEEDED".to_string()),
        );
        assert_valid(&[first.clone(), second.clone()]);
        second.result = ResultValue::Applied;
        second.exit = Some(0);
        assert_invalid(&[first, second]);
    }

    #[test]
    fn coupled_cycle_refusal_preserves_state_and_related_edges_do_not_block() {
        let initial = State::initial();
        let edge = Operation::AddEdge(3, 4, EdgeKind::Blocks);
        let mut after = initial.clone();
        after.edges.insert((3, 4, EdgeKind::Blocks));
        after.events.insert("3:dependency_added".to_string(), 1);
        let first = synthetic(0, (10, 30), edge, ResultValue::Applied);
        let mut second = synthetic(
            1,
            (15, 25),
            Operation::AddEdge(4, 3, EdgeKind::Blocks),
            ResultValue::Refused("CYCLE_DETECTED".to_string()),
        );
        let final_read = synthetic(2, (40, 50), Operation::Final, ResultValue::Final(after));
        assert_valid(&[first.clone(), second.clone(), final_read]);
        second.result = ResultValue::Applied;
        second.exit = Some(0);
        assert_invalid(&[first, second]);

        let related = synthetic(
            0,
            (10, 20),
            Operation::AddEdge(3, 4, EdgeKind::Related),
            ResultValue::Applied,
        );
        let ready = synthetic(
            1,
            (30, 40),
            Operation::Ready,
            ResultValue::Ready(BTreeSet::from([0, 1, 2, 3, 4])),
        );
        assert_valid(&[related.clone(), ready]);
        let missing = synthetic(
            1,
            (30, 40),
            Operation::Ready,
            ResultValue::Ready(BTreeSet::from([0, 1, 2, 4])),
        );
        assert_invalid(&[related, missing]);
    }

    #[test]
    fn coupled_close_reopen_readiness_and_acknowledged_events_are_checked() {
        let mut initial = State::initial();
        initial.edges.insert((3, 4, EdgeKind::Blocks));
        let close = synthetic(0, (10, 20), Operation::Close(4), ResultValue::Applied);
        let ready = synthetic(
            1,
            (30, 40),
            Operation::Ready,
            ResultValue::Ready(BTreeSet::from([0, 1, 2, 3])),
        );
        let reopen = synthetic(2, (50, 60), Operation::Reopen(4), ResultValue::Applied);
        let blocked = synthetic(
            3,
            (70, 80),
            Operation::Ready,
            ResultValue::Ready(BTreeSet::from([0, 1, 2, 4])),
        );
        let mut final_state = initial.clone();
        final_state.events = BTreeMap::from([
            ("4:status_changed".to_string(), 2),
            ("4:closed".to_string(), 1),
            ("4:reopened".to_string(), 1),
        ]);
        let mut final_read = synthetic(
            4,
            (90, 100),
            Operation::Final,
            ResultValue::Final(final_state.clone()),
        );
        let mut calls = vec![close, ready, reopen, blocked, final_read.clone()];
        assert!(matches!(
            Search::check(&calls, &initial, SEARCH_BUDGET),
            Verdict::Valid(_)
        ));
        final_state.events.clear();
        final_read.result = ResultValue::Final(final_state);
        calls[4] = final_read;
        assert!(matches!(
            Search::check(&calls, &initial, SEARCH_BUDGET),
            Verdict::Invalid { .. }
        ));
        // The status returned to open, but missing close/reopen events still
        // reveal an acknowledged mutation that never took effect.
    }

    #[test]
    fn coupled_overlapping_read_can_linearize_before_an_earlier_response() {
        let close = synthetic(0, (10, 20), Operation::Close(4), ResultValue::Applied);
        let before = synthetic(
            1,
            (15, 30),
            Operation::Ready,
            ResultValue::Ready(BTreeSet::from([0, 1, 2, 3, 4])),
        );
        let after = synthetic(
            1,
            (15, 30),
            Operation::Ready,
            ResultValue::Ready(BTreeSet::from([0, 1, 2, 3])),
        );
        assert_valid(&[close.clone(), before.clone()]);
        assert_valid(&[close.clone(), after]);
        let mut stale = before;
        stale.invoke_ns = 21;
        assert_invalid(&[close, stale]);
    }

    #[test]
    fn coupled_uncertain_commit_needs_a_final_observation_and_keeps_both_choices() {
        let uncertain = synthetic(0, (10, 20), Operation::Claim(1), ResultValue::Uncertain);
        assert!(matches!(
            checked(std::slice::from_ref(&uncertain)),
            Verdict::Inconclusive { .. }
        ));
        for applied in [false, true] {
            let mut final_state = State::initial();
            if applied {
                final_state.statuses[1] = "in_progress".to_string();
                final_state.events.insert("1:status_changed".to_string(), 1);
            }
            let observed = synthetic(
                1,
                (30, 40),
                Operation::Final,
                ResultValue::Final(final_state),
            );
            assert_valid(&[uncertain.clone(), observed]);
        }
    }

    #[test]
    fn coupled_exhaustion_and_malformed_records_never_pass() {
        assert!(parse_positive_setting("0").is_err());
        assert!(parse_positive_setting("invalid").is_err());
        assert_eq!(parse_positive_setting("2"), Ok(2));
        let call = synthetic(0, (10, 20), Operation::Claim(1), ResultValue::Applied);
        assert!(matches!(
            Search::check(std::slice::from_ref(&call), &State::initial(), 0),
            Verdict::Inconclusive { visited: 0, .. }
        ));
        assert_valid(std::slice::from_ref(&call));
        let mut malformed = call.clone();
        malformed.return_ns = 9;
        assert_invalid(&[malformed]);
        let mut malformed = call.clone();
        malformed.operation = Operation::Ready;
        assert_invalid(&[malformed]);
        let mut malformed = call;
        malformed.exit = Some(4);
        assert_invalid(&[malformed]);
        let mut refusal = synthetic(
            0,
            (10, 20),
            Operation::Claim(1),
            ResultValue::Refused("WORKFLOW_CAPACITY_EXCEEDED".to_string()),
        );
        refusal.exit = Some(0);
        assert_invalid(&[refusal]);
    }

    #[test]
    fn coupled_blocked_claim_is_a_deterministic_refusal() {
        let mut call = synthetic(0, (10, 20), Operation::Claim(3), ResultValue::Uncertain);
        call.exit = Some(4);
        call.stdout = serde_json::json!({"error": {"code": "VALIDATION_FAILED", "context": {"field": "claim", "reason": "cannot claim blocked issue: lz-4"}}}).to_string();
        call.result = classify(&call, &[]);
        assert!(matches!(&call.result, ResultValue::Refused(code) if code == "CLAIM_BLOCKED"));
        let mut initial = State::initial();
        initial.edges.insert((3, 4, EdgeKind::Blocks));
        match Search::check(&[call], &initial, SEARCH_BUDGET) {
            Verdict::Valid(final_state) => assert_eq!(final_state, initial),
            verdict => panic!("blocked claim must refuse without a mutation: {verdict:?}"),
        }
    }

    #[test]
    fn coupled_minimized_counterexample_replays_as_invalid() {
        let calls = [
            synthetic(
                0,
                (0, 1),
                Operation::Ready,
                ResultValue::Ready(BTreeSet::from([0, 1, 2, 3, 4])),
            ),
            synthetic(1, (10, 30), Operation::Claim(1), ResultValue::Applied),
            synthetic(2, (15, 25), Operation::Claim(2), ResultValue::Applied),
        ];
        let minimal = minimize(&calls, &State::initial());
        assert_eq!(minimal.len(), 2);
        let encoded = serde_json::to_string(&minimal).expect("counterexample JSON");
        let replay: Vec<Call> = serde_json::from_str(&encoded).expect("counterexample replay");
        assert_invalid(&replay);
        for index in 0..replay.len() {
            let mut smaller = replay.clone();
            smaller.remove(index);
            assert_valid(&smaller);
        }
    }
}
