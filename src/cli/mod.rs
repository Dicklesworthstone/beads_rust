//! CLI definitions and entry point.

use crate::franken_sync::Connection;
use clap::builder::StyledStr;
use clap::{Args, Parser, Subcommand, ValueEnum};
use clap_complete::engine::{ArgValueCompleter, CompletionCandidate};
use fsqlite_types::SqliteValue;
use serde::Deserialize;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::config;
use crate::format::{format_status_label, truncate_title};
use crate::model::{IssueType, Status};

pub mod commands;

/// Default cap for work-surface listings (`br list`).
///
/// #349: work-surface listings are COMPLETE by default — `0` means "no cap".
/// Silently truncating an agent's view of its work surface hides issues and
/// leads to lost work; listings now return every matching issue unless the
/// caller passes an explicit `--limit`. The query layer treats `Some(0)` as
/// unlimited (see `SqliteStorage` list paths, which only apply `LIMIT` when
/// the value is `> 0`).
pub(crate) const DEFAULT_LIST_LIMIT: usize = 0;
pub(crate) const DEFAULT_LIST_OFFSET: usize = 0;

/// Default cap for full-text SEARCH results (`br search`).
///
/// #349: unlike list/ready (which are complete by default), search results
/// stay capped — a broad text query can match a huge fraction of the corpus,
/// and a bounded, relevance-ordered result set is the right default. Callers
/// can pass `--limit 0` for an unbounded search.
pub(crate) const DEFAULT_SEARCH_LIMIT: usize = 50;

#[derive(Clone, Copy)]
enum IssueCompletionFilter {
    Any,
    Open,
    Closed,
}

impl IssueCompletionFilter {
    fn matches(self, status: &Status) -> bool {
        match self {
            Self::Any => !matches!(status, Status::Tombstone),
            Self::Open => !status.is_terminal(),
            Self::Closed => matches!(status, Status::Closed),
        }
    }
}

#[derive(Deserialize, Debug)]
struct CompletionIssue {
    id: String,
    title: String,
    #[serde(default)]
    status: Status,
    #[serde(default)]
    issue_type: IssueType,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    assignee: Option<String>,
    #[serde(default)]
    owner: Option<String>,
}

#[derive(Default, Debug)]
struct CompletionIndex {
    issues: Vec<CompletionIssue>,
    labels: Vec<String>,
    assignees: Vec<String>,
    owners: Vec<String>,
    types: Vec<String>,
}

#[derive(Default, Debug)]
struct CompletionConfigIndex {
    config_keys: Vec<String>,
    saved_queries: Vec<String>,
}

static COMPLETION_INDEX: OnceLock<CompletionIndex> = OnceLock::new();
static CONFIG_INDEX: OnceLock<CompletionConfigIndex> = OnceLock::new();

const STATUS_CANDIDATES: &[(&str, &str)] = &[
    ("open", "Open issue"),
    ("in_progress", "In progress"),
    ("blocked", "Blocked"),
    ("deferred", "Deferred"),
    ("draft", "Draft"),
    ("closed", "Closed"),
    ("tombstone", "Deleted"),
    ("pinned", "Pinned"),
];

const STATUS_WITH_ALL_CANDIDATES: &[(&str, &str)] = &[
    ("all", "All statuses"),
    ("open", "Open issue"),
    ("in_progress", "In progress"),
    ("blocked", "Blocked"),
    ("deferred", "Deferred"),
    ("draft", "Draft"),
    ("closed", "Closed"),
    ("tombstone", "Deleted"),
    ("pinned", "Pinned"),
];

const ISSUE_TYPE_CANDIDATES: &[(&str, &str)] = &[
    ("task", "Task"),
    ("bug", "Bug"),
    ("feature", "Feature"),
    ("epic", "Epic"),
    ("chore", "Chore"),
    ("docs", "Docs"),
    ("question", "Question"),
];

const PRIORITY_CANDIDATES: &[(&str, &str)] = &[
    ("0", "Critical (P0)"),
    ("1", "High (P1)"),
    ("2", "Medium (P2)"),
    ("3", "Low (P3)"),
    ("4", "Backlog (P4)"),
    ("P0", "Critical (0)"),
    ("P1", "High (1)"),
    ("P2", "Medium (2)"),
    ("P3", "Low (3)"),
    ("P4", "Backlog (4)"),
];

const PRIORITY_NUMERIC_CANDIDATES: &[(&str, &str)] = &[
    ("0", "Critical (P0)"),
    ("1", "High (P1)"),
    ("2", "Medium (P2)"),
    ("3", "Low (P3)"),
    ("4", "Backlog (P4)"),
];

const DEP_TYPE_CANDIDATES: &[(&str, &str)] = &[
    ("blocks", "Blocks (default)"),
    ("parent-child", "Parent child"),
    ("conditional-blocks", "Conditional blocks"),
    ("waits-for", "Waits for"),
    ("related", "Related"),
    ("discovered-from", "Discovered from"),
    ("replies-to", "Replies to"),
    ("relates-to", "Relates to"),
    ("duplicates", "Duplicates"),
    ("supersedes", "Supersedes"),
    ("caused-by", "Caused by"),
];

const SORT_KEY_CANDIDATES: &[(&str, &str)] = &[
    ("priority", "Priority"),
    ("created_at", "Created at"),
    ("updated_at", "Updated at"),
    ("title", "Title"),
    ("created", "Alias for created_at"),
    ("updated", "Alias for updated_at"),
];

const DEP_TREE_FORMAT_CANDIDATES: &[(&str, &str)] =
    &[("text", "Text output"), ("mermaid", "Mermaid graph")];

const CSV_FIELD_CANDIDATES: &[(&str, &str)] = &[
    ("id", "Issue ID"),
    ("title", "Title"),
    ("description", "Description"),
    ("status", "Status"),
    ("priority", "Priority"),
    ("issue_type", "Issue type"),
    ("assignee", "Assignee"),
    ("owner", "Owner"),
    ("created_at", "Created at"),
    ("updated_at", "Updated at"),
    ("closed_at", "Closed at"),
    ("due_at", "Due at"),
    ("defer_until", "Defer until"),
    ("notes", "Notes"),
    ("external_ref", "External ref"),
];

const EXPORT_ERROR_POLICY_CANDIDATES: &[(&str, &str)] = &[
    ("strict", "Abort export on any error (default)"),
    (
        "best-effort",
        "Skip problematic records, export what we can",
    ),
    ("partial", "Export valid records, report failures"),
    (
        "required-core",
        "Only export core issues, tolerate non-core errors",
    ),
];

const ORPHAN_MODE_CANDIDATES: &[(&str, &str)] = &[
    ("strict", "Fail if any issue references a missing parent"),
    ("resurrect", "Attempt to resurrect missing parents if found"),
    ("skip", "Skip orphaned issues"),
    ("allow", "Allow orphans (no parent validation)"),
];

const SAVED_QUERY_PREFIX: &str = "saved_query:";

fn completion_index() -> &'static CompletionIndex {
    COMPLETION_INDEX.get_or_init(build_completion_index)
}

fn config_index() -> &'static CompletionConfigIndex {
    CONFIG_INDEX.get_or_init(build_config_index)
}

fn add_layer_keys(keys: &mut BTreeSet<String>, layer: &config::ConfigLayer) {
    keys.extend(layer.runtime.keys().cloned());
    keys.extend(layer.startup.keys().cloned());
}

fn resolve_completion_paths_for_beads_dir(beads_dir: &Path) -> Option<config::ConfigPaths> {
    config::resolve_paths(beads_dir, None).ok()
}

fn completion_paths() -> Option<config::ConfigPaths> {
    let beads_dir = config::discover_beads_dir(None).ok()?;
    resolve_completion_paths_for_beads_dir(&beads_dir)
}

fn saved_queries_from_db(db_path: &Path) -> BTreeSet<String> {
    if !db_path.is_file() {
        return BTreeSet::new();
    }

    let Ok(queries) = config::with_database_family_snapshot(db_path, |snapshot_db_path| {
        let conn = Connection::open(snapshot_db_path.to_string_lossy().into_owned())?;
        let _ = conn.execute("PRAGMA busy_timeout=0");
        let rows = conn.query("SELECT key FROM config")?;
        let mut queries = BTreeSet::new();

        for row in &rows {
            let Some(key) = row.get(0).and_then(SqliteValue::as_text) else {
                continue;
            };
            if let Some(name) = key.strip_prefix(SAVED_QUERY_PREFIX)
                && !name.trim().is_empty()
            {
                queries.insert(name.to_string());
            }
        }

        conn.close()?;
        Ok(queries)
    }) else {
        return BTreeSet::new();
    };

    queries
}

fn build_completion_index() -> CompletionIndex {
    let Some(paths) = completion_paths() else {
        return CompletionIndex::default();
    };
    let Ok(file) = File::open(&paths.jsonl_path) else {
        return CompletionIndex::default();
    };

    let reader = BufReader::new(file);
    let mut issues = Vec::new();
    let mut labels = BTreeSet::new();
    let mut assignees = BTreeSet::new();
    let mut owners = BTreeSet::new();
    let mut types = BTreeSet::new();

    for line_result in reader.lines() {
        let Ok(line) = line_result else {
            break;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let issue: CompletionIssue = match serde_json::from_str(trimmed) {
            Ok(issue) => issue,
            Err(_) => continue,
        };

        for label in &issue.labels {
            let label = label.trim();
            if !label.is_empty() {
                labels.insert(label.to_string());
            }
        }
        if let Some(assignee) = issue.assignee.as_deref() {
            let assignee = assignee.trim();
            if !assignee.is_empty() {
                assignees.insert(assignee.to_string());
            }
        }
        if let Some(owner) = issue.owner.as_deref() {
            let owner = owner.trim();
            if !owner.is_empty() {
                owners.insert(owner.to_string());
            }
        }
        let issue_type = issue.issue_type.as_str().trim();
        if !issue_type.is_empty() {
            types.insert(issue_type.to_string());
        }

        issues.push(issue);
    }

    issues.sort_by(|a, b| a.id.cmp(&b.id));

    CompletionIndex {
        issues,
        labels: labels.into_iter().collect(),
        assignees: assignees.into_iter().collect(),
        owners: owners.into_iter().collect(),
        types: types.into_iter().collect(),
    }
}

fn build_config_index() -> CompletionConfigIndex {
    let mut keys = BTreeSet::new();
    let mut saved_queries = BTreeSet::new();

    add_layer_keys(&mut keys, &config::default_config_layer());
    if let Ok(legacy_user) = config::load_legacy_user_config() {
        add_layer_keys(&mut keys, &legacy_user);
    }
    if let Ok(user) = config::load_user_config() {
        add_layer_keys(&mut keys, &user);
    }
    add_layer_keys(&mut keys, &config::ConfigLayer::from_env());

    if let Some(paths) = completion_paths() {
        if let Ok(project) = config::load_project_config(&paths.beads_dir) {
            add_layer_keys(&mut keys, &project);
        }
        saved_queries.extend(saved_queries_from_db(&paths.db_path));
    }

    CompletionConfigIndex {
        config_keys: keys.into_iter().collect(),
        saved_queries: saved_queries.into_iter().collect(),
    }
}

fn matches_prefix_case_insensitive(value: &str, prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    value
        .to_ascii_lowercase()
        .starts_with(&prefix.to_ascii_lowercase())
}

fn static_candidates(
    prefix: &str,
    values: &[(&'static str, &'static str)],
) -> Vec<CompletionCandidate> {
    values
        .iter()
        .filter(|(value, _)| matches_prefix_case_insensitive(value, prefix))
        .map(|(value, help)| CompletionCandidate::new(*value).help(Some(StyledStr::from(*help))))
        .collect()
}

fn static_candidates_with_suffix(
    prefix: &str,
    values: &[(&'static str, &'static str)],
    suffix: &str,
) -> Vec<CompletionCandidate> {
    values
        .iter()
        .filter(|(value, _)| matches_prefix_case_insensitive(value, prefix))
        .map(|(value, help)| {
            CompletionCandidate::new(format!("{value}{suffix}")).help(Some(StyledStr::from(*help)))
        })
        .collect()
}

fn dynamic_candidates(prefix: &str, values: &[String]) -> Vec<CompletionCandidate> {
    values
        .iter()
        .filter(|value| matches_prefix_case_insensitive(value, prefix))
        .map(CompletionCandidate::new)
        .collect()
}

fn split_delimited_prefix(current: &str, delimiter: char) -> (String, &str) {
    current.rfind(delimiter).map_or_else(
        || (String::new(), current.trim_start()),
        |idx| {
            let (head, tail) = current.split_at(idx + delimiter.len_utf8());
            let trimmed_tail = tail.trim_start();
            let ws_len = tail.len().saturating_sub(trimmed_tail.len());
            let mut prefix = String::with_capacity(head.len() + ws_len);
            prefix.push_str(head);
            prefix.push_str(&tail[..ws_len]);
            (prefix, trimmed_tail)
        },
    )
}

fn split_key_prefix(current: &str, delimiter: char) -> Option<(String, &str)> {
    let idx = current.find(delimiter)?;
    let (head, tail) = current.split_at(idx + delimiter.len_utf8());
    let trimmed_tail = tail.trim_start();
    let ws_len = tail.len().saturating_sub(trimmed_tail.len());
    let mut prefix = String::with_capacity(head.len() + ws_len);
    prefix.push_str(head);
    prefix.push_str(&tail[..ws_len]);
    Some((prefix, trimmed_tail))
}

fn static_candidates_delimited(
    current: &OsStr,
    delimiter: char,
    values: &[(&'static str, &'static str)],
) -> Vec<CompletionCandidate> {
    let Some(current) = current.to_str() else {
        return Vec::new();
    };
    let (prefix, needle) = split_delimited_prefix(current, delimiter);
    static_candidates(needle, values)
        .into_iter()
        .map(|candidate| candidate.add_prefix(prefix.clone()))
        .collect()
}

fn dynamic_candidates_delimited(
    current: &OsStr,
    delimiter: char,
    values: &[String],
) -> Vec<CompletionCandidate> {
    let Some(current) = current.to_str() else {
        return Vec::new();
    };
    let (prefix, needle) = split_delimited_prefix(current, delimiter);
    dynamic_candidates(needle, values)
        .into_iter()
        .map(|candidate| candidate.add_prefix(prefix.clone()))
        .collect()
}

fn issue_id_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    issue_id_completer_with_filter(current, IssueCompletionFilter::Any)
}

fn open_issue_id_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    issue_id_completer_with_filter(current, IssueCompletionFilter::Open)
}

fn closed_issue_id_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    issue_id_completer_with_filter(current, IssueCompletionFilter::Closed)
}

fn issue_id_completer_with_filter(
    current: &OsStr,
    filter: IssueCompletionFilter,
) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };

    issue_id_candidates(prefix, filter)
}

fn issue_id_candidates(prefix: &str, filter: IssueCompletionFilter) -> Vec<CompletionCandidate> {
    let mut candidates = Vec::new();
    for issue in &completion_index().issues {
        if !prefix.is_empty() && !issue.id.starts_with(prefix) {
            continue;
        }
        if filter.matches(&issue.status) {
            let title = truncate_title(&issue.title, 60);
            let help = format!("{} | {}", format_status_label(&issue.status, false), title);
            candidates.push(CompletionCandidate::new(&issue.id).help(Some(StyledStr::from(help))));
        }
    }

    candidates
}

fn status_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };
    static_candidates(prefix, STATUS_CANDIDATES)
}

fn status_completer_delimited(current: &OsStr) -> Vec<CompletionCandidate> {
    static_candidates_delimited(current, ',', STATUS_CANDIDATES)
}

fn status_or_all_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };
    static_candidates(prefix, STATUS_WITH_ALL_CANDIDATES)
}

fn issue_type_is_standard(value: &str) -> bool {
    ISSUE_TYPE_CANDIDATES
        .iter()
        .any(|(candidate, _)| candidate.eq_ignore_ascii_case(value))
}

fn issue_type_candidates(prefix: &str) -> Vec<CompletionCandidate> {
    let mut candidates = static_candidates(prefix, ISSUE_TYPE_CANDIDATES);
    candidates.extend(
        completion_index()
            .types
            .iter()
            .filter(|value| !issue_type_is_standard(value))
            .filter(|value| matches_prefix_case_insensitive(value, prefix))
            .map(CompletionCandidate::new),
    );
    candidates
}

fn issue_type_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };

    issue_type_candidates(prefix)
}

fn issue_type_completer_delimited(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(current) = current.to_str() else {
        return Vec::new();
    };
    let (prefix, needle) = split_delimited_prefix(current, ',');
    issue_type_candidates(needle)
        .into_iter()
        .map(|candidate| candidate.add_prefix(prefix.clone()))
        .collect()
}

fn issue_type_standard_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };
    static_candidates(prefix, ISSUE_TYPE_CANDIDATES)
}

fn priority_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };
    static_candidates(prefix, PRIORITY_CANDIDATES)
}

fn priority_completer_delimited(current: &OsStr) -> Vec<CompletionCandidate> {
    static_candidates_delimited(current, ',', PRIORITY_CANDIDATES)
}

fn priority_numeric_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };
    static_candidates(prefix, PRIORITY_NUMERIC_CANDIDATES)
}

fn label_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };
    dynamic_candidates(prefix, &completion_index().labels)
}

fn label_completer_delimited(current: &OsStr) -> Vec<CompletionCandidate> {
    dynamic_candidates_delimited(current, ',', &completion_index().labels)
}

fn assignee_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };
    dynamic_candidates(prefix, &completion_index().assignees)
}

fn owner_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };
    dynamic_candidates(prefix, &completion_index().owners)
}

fn dep_type_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };
    static_candidates(prefix, DEP_TYPE_CANDIDATES)
}

fn deps_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(current) = current.to_str() else {
        return Vec::new();
    };
    let (outer_prefix, tail) = split_delimited_prefix(current, ',');
    if let Some((type_prefix, id_prefix)) = split_key_prefix(tail, ':') {
        let mut prefix = outer_prefix;
        prefix.push_str(&type_prefix);
        return issue_id_candidates(id_prefix, IssueCompletionFilter::Any)
            .into_iter()
            .map(|candidate| candidate.add_prefix(prefix.clone()))
            .collect();
    }

    static_candidates_with_suffix(tail, DEP_TYPE_CANDIDATES, ":")
        .into_iter()
        .map(|candidate| candidate.add_prefix(outer_prefix.clone()))
        .collect()
}

fn dep_tree_format_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };
    static_candidates(prefix, DEP_TREE_FORMAT_CANDIDATES)
}

fn saved_query_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };
    dynamic_candidates(prefix, &config_index().saved_queries)
}

fn config_key_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };
    dynamic_candidates(prefix, &config_index().config_keys)
}

fn config_key_assignment_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };
    if prefix.contains('=') {
        return Vec::new();
    }

    let mut candidates = Vec::new();
    for key in &config_index().config_keys {
        if matches_prefix_case_insensitive(key, prefix) {
            candidates.push(CompletionCandidate::new(key));
            candidates.push(CompletionCandidate::new(format!("{key}=")));
        }
    }
    candidates
}

fn export_error_policy_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };
    static_candidates(prefix, EXPORT_ERROR_POLICY_CANDIDATES)
}

fn orphan_mode_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };
    static_candidates(prefix, ORPHAN_MODE_CANDIDATES)
}

fn sort_key_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };
    static_candidates(prefix, SORT_KEY_CANDIDATES)
}

fn csv_fields_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    static_candidates_delimited(current, ',', CSV_FIELD_CANDIDATES)
}

/// Agent-first issue tracker (`SQLite` + JSONL)
#[derive(Parser, Debug)]
#[command(name = "br", author, version, about, long_about = None)]
#[allow(clippy::struct_excessive_bools)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Database path (auto-discover .beads/*.db if not set)
    #[arg(long, global = true)]
    pub db: Option<PathBuf>,

    /// Actor name for audit trail
    #[arg(long, global = true)]
    pub actor: Option<String>,

    /// Output as JSON
    #[arg(long, global = true)]
    pub json: bool,

    /// Force direct mode (no daemon) - effectively no-op in br v1
    #[arg(long, global = true)]
    pub no_daemon: bool,

    /// Skip auto JSONL export
    #[arg(long, global = true)]
    pub no_auto_flush: bool,

    /// Skip auto import check
    #[arg(long, global = true)]
    pub no_auto_import: bool,

    /// Allow stale DB (bypass freshness check warning)
    #[arg(long, global = true)]
    pub allow_stale: bool,

    /// `SQLite` busy/write-lock timeout in ms
    #[arg(long, global = true)]
    pub lock_timeout: Option<u64>,

    /// JSONL-only mode (no DB connection)
    #[arg(long, global = true)]
    pub no_db: bool,

    /// Increase logging verbosity (-v, -vv)
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Quiet mode (no output except errors)
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Disable colored output
    #[arg(long, global = true)]
    pub no_color: bool,
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Manage AGENTS.md workflow instructions
    Agents(AgentsArgs),

    /// Record and label agent interactions (append-only JSONL)
    Audit {
        #[command(subcommand)]
        command: AuditCommands,
    },

    /// List blocked issues
    Blocked(BlockedArgs),

    /// Describe br's machine-readable contracts and safety guarantees
    Capabilities(CapabilitiesArgs),

    /// Workflow capacity management: audited issue-specific exemptions (GitHub #384)
    Capacity {
        #[command(subcommand)]
        command: CapacityCommands,
    },

    /// Generate changelog from closed issues
    Changelog(ChangelogArgs),

    /// Close an issue
    Close(CloseArgs),

    /// Manage comments
    #[command(alias = "comment")]
    Comments(CommentsArgs),

    /// Generate shell completions
    #[command(alias = "completion")]
    Completions(CompletionsArgs),

    /// Configuration management
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },

    /// Diagnose swarm coordination state without mutating claims
    #[command(alias = "coord")]
    Coordination {
        #[command(subcommand)]
        command: CoordinationCommands,
    },

    /// Count issues with optional grouping
    Count(CountArgs),

    /// Create a new issue
    Create(CreateArgs),

    /// Defer issues (schedule for later)
    Defer(DeferArgs),

    /// Delete an issue (creates tombstone)
    Delete(DeleteArgs),

    /// Manage dependencies
    Dep {
        #[command(subcommand)]
        command: DepCommands,
    },

    /// Run diagnostics and optionally repair issues
    Doctor(DoctorArgs),

    /// Epic management commands
    Epic {
        #[command(subcommand)]
        command: EpicCommands,
    },

    /// Workflow gate engine: record and inspect gate results (issue #312)
    Gate {
        #[command(subcommand)]
        command: GateCommands,
    },

    /// Visualize the dependents graph: what an issue unblocks
    Graph(GraphArgs),

    /// Manage local history backups
    History(HistoryArgs),

    /// Show diagnostic metadata about the workspace
    Info(InfoArgs),

    /// Initialize a beads workspace
    Init {
        /// Issue ID prefix (e.g., "bd")
        #[arg(long)]
        prefix: Option<String>,

        /// Overwrite existing DB
        #[arg(long)]
        force: bool,

        /// Backend type (ignored, always sqlite)
        #[arg(long)]
        backend: Option<String>,
    },

    /// Manage labels
    Label {
        #[command(subcommand)]
        command: LabelCommands,
    },

    /// Check issues for missing template sections
    Lint(LintArgs),

    /// List issues
    List(ListArgs),

    /// List orphan issues (referenced in commits but open)
    Orphans(OrphansArgs),

    /// Quick capture (create issue, print ID only)
    Q(QuickArgs),

    /// Manage saved queries
    Query {
        #[command(subcommand)]
        command: QueryCommands,
    },

    /// List ready issues (open, unblocked, not deferred)
    Ready(ReadyArgs),

    /// Reopen an issue
    Reopen(ReopenArgs),

    /// Print concise in-tool docs for automation agents
    #[command(name = "robot-docs", alias = "robot_docs")]
    RobotDocs {
        #[command(subcommand)]
        command: RobotDocsCommands,
    },

    /// Rank ready work for agent swarms with explainable evidence
    #[command(alias = "schedule")]
    Scheduler(SchedulerArgs),

    /// Emit JSON Schemas and per-command output envelope shapes (for agent/tooling integration)
    ///
    /// IMPORTANT: br schema is not a stable API and is subject to change.
    /// Use at your own risk.
    Schema(SchemaArgs),

    /// Search issues (matches title, description, id, and comment text)
    Search(SearchArgs),

    /// Show issue details
    Show(ShowArgs),

    /// List stale issues
    Stale(StaleArgs),

    /// Show project statistics
    Stats(StatsArgs),

    /// Alias for stats
    Status(StatsArgs),

    /// Sync database with JSONL file (export or import)
    ///
    /// IMPORTANT: br sync NEVER executes git commands or auto-commits.
    /// All file operations are confined to .beads/ by default.
    /// Use -v for detailed safety logging, -vv for debug output.
    #[command(long_about = "Sync database with JSONL file (export or import).

SAFETY GUARANTEES:
  • br sync NEVER executes git commands or auto-commits
  • br sync NEVER modifies files outside .beads/ (unless --allow-external-jsonl)
  • All writes use atomic temp-file-then-rename pattern
  • Safety guards prevent accidental data loss

MODES (one required):
  --flush-only    Export database to JSONL (safe by default)
  --import-only   Import JSONL into database (validates first)
  --merge         Three-way merge .beads/beads.base.jsonl + DB + JSONL
  --status        Show sync status (read-only)
  --witness       Emit deterministic JSONL chunk witness (read-only)
  --reconcile-additive
                   Plan/apply exact-ID additive reconciliation

SAFETY GUARDS:
  Export guards (bypassed with --force):
    • Empty DB Guard: Refuses to export empty DB over non-empty JSONL
    • Stale DB Guard: Refuses to export if JSONL has issues missing from DB

  Import guards (cannot be bypassed):
    • Conflict markers: Rejects files with git merge conflict markers
    • Invalid JSON: Rejects malformed JSONL entries

  Merge guards:
    • Semantic conflicts require --force-db, --force-jsonl, or --force
    • --force-db keeps the local SQLite version
    • --force-jsonl keeps the JSONL version
    • --force keeps the newer timestamp

  Rebuild:
    • --rebuild is import-only and treats JSONL as authoritative
    • Removes DB entries absent from JSONL while preserving tombstones

VERBOSE LOGGING:
  -v     Show INFO-level safety guard decisions
  -vv    Show DEBUG-level file operations

EXAMPLES:
  br sync --flush-only           Export database to .beads/issues.jsonl
  br sync --flush-only -v        Export with safety logging
  br sync --import-only          Import from JSONL (validates first)
  br sync --merge                Merge DB and JSONL changes
  br sync --merge --force-db     Keep local DB conflicts
  br sync --merge --force-jsonl  Keep JSONL conflicts
  br sync --import-only --rebuild Import + remove DB entries not in JSONL
  br sync --status               Show current sync status
  br sync --witness --json       Emit JSONL chunk witness
  br vcs-status --json           Explicitly inspect JSONL Git visibility")]
    Sync(SyncArgs),

    /// Undefer issues (make ready again)
    Undefer(UndeferArgs),

    /// Update an issue
    Update(UpdateArgs),

    /// Explicitly inspect Git visibility for the configured JSONL export
    ///
    /// This diagnostic is intentionally separate from `br sync`: sync never
    /// executes Git. The command applies a shared probe budget, bounded output,
    /// literal path handling, and no-prompt/no-optional-lock safeguards.
    #[command(name = "vcs-status")]
    VcsStatus(VcsStatusArgs),

    /// Start an MCP (Model Context Protocol) server on stdio
    ///
    /// Exposes the issue tracker to AI agents via the standard MCP protocol.
    /// This is an alternative to shelling out to the br CLI.
    #[cfg(feature = "mcp")]
    Serve(crate::mcp::ServeArgs),

    /// Upgrade br to the latest version
    #[cfg(feature = "self_update")]
    Upgrade(UpgradeArgs),

    /// Show version information
    Version(VersionArgs),

    /// Show the active .beads directory
    Where,
}

/// Arguments for the completions command.
#[derive(Args, Debug, Clone)]
pub struct CompletionsArgs {
    /// Shell to generate completions for
    #[arg(value_enum)]
    pub shell: ShellType,

    /// Output directory (default: stdout)
    #[arg(long, short = 'o')]
    pub output: Option<std::path::PathBuf>,
}

/// Supported shells for completion generation.
#[derive(ValueEnum, Debug, Clone, Copy, Eq, PartialEq)]
pub enum ShellType {
    /// Bash shell
    Bash,
    /// Zsh shell
    Zsh,
    /// Fish shell
    Fish,
    #[value(name = "powershell")]
    #[value(alias = "pwsh")]
    /// `PowerShell`
    PowerShell,
    /// Elvish
    Elvish,
}

#[derive(Args, Debug, Default)]
pub struct CreateArgs {
    /// Issue title
    pub title: Option<String>,

    /// Issue title (alternative to positional argument)
    #[arg(long = "title", conflicts_with = "title")]
    pub title_flag: Option<String>, // Handled in logic

    /// Issue type (task, bug, feature, etc.)
    #[arg(long = "type", short = 't', add = ArgValueCompleter::new(issue_type_completer))]
    pub type_: Option<String>,

    /// Human-readable slug embedded in the generated ID. Example: `--slug
    /// survey-my-thing` produces an ID of the form `br-survey-my-thing-<hash>`,
    /// keeping the configured prefix and the uniquifying hash suffix. The slug
    /// is normalized to lowercase ASCII alphanumeric + single hyphens (runs of
    /// other characters collapse to one hyphen, leading/trailing hyphens are
    /// stripped, length is capped at 48 characters after normalization).
    #[arg(long)]
    pub slug: Option<String>,

    /// Priority (0-4 or P0-P4)
    #[arg(long, short = 'p', add = ArgValueCompleter::new(priority_completer))]
    pub priority: Option<String>,

    /// Description
    // Markdown bodies routinely begin with a list marker ("- item"), which
    // clap otherwise parses as an unknown flag in the space-separated form.
    #[arg(long, short = 'd', visible_alias = "body", allow_hyphen_values = true)]
    pub description: Option<String>,

    /// Read the issue description verbatim from a file (or `-` for stdin).
    ///
    /// The entire file content is used as the description, unchanged from
    /// disk — unlike the bulk `-f/--file` markdown import, which only
    /// captures the first paragraph after each `## Title` heading. Use this
    /// for multi-paragraph / markdown descriptions (lists, fenced code)
    /// that are fragile to pass through shell quoting on `-d/--description`.
    /// Mutually exclusive with `-d/--description`.
    #[arg(long, value_name = "PATH", conflicts_with = "description")]
    pub description_file: Option<std::path::PathBuf>,

    /// Assign to person
    #[arg(long, short = 'a', add = ArgValueCompleter::new(assignee_completer))]
    pub assignee: Option<String>,

    /// Set owner email
    #[arg(long, add = ArgValueCompleter::new(owner_completer))]
    pub owner: Option<String>,

    /// Labels (comma-separated)
    #[arg(long, short = 'l', value_delimiter = ',', add = ArgValueCompleter::new(label_completer_delimited))]
    pub labels: Vec<String>,

    /// Parent issue ID (creates parent-child dep)
    #[arg(long, add = ArgValueCompleter::new(issue_id_completer))]
    pub parent: Option<String>,

    /// Dependencies (format: type:id,type:id)
    #[arg(long, value_delimiter = ',', add = ArgValueCompleter::new(deps_completer))]
    pub deps: Vec<String>,

    /// Time estimate in minutes
    #[arg(long, short = 'e')]
    pub estimate: Option<i32>,

    /// Due date (RFC3339 or relative)
    #[arg(long)]
    pub due: Option<String>,

    /// Defer until date (RFC3339 or relative)
    #[arg(long)]
    pub defer: Option<String>,

    /// External reference
    #[arg(long)]
    pub external_ref: Option<String>,

    /// Mark as ephemeral (not exported to JSONL)
    #[arg(long)]
    pub ephemeral: bool,

    /// Initial status (open, deferred, in_progress, closed)
    #[arg(long, short = 's', add = ArgValueCompleter::new(status_completer))]
    pub status: Option<String>,

    /// Acceptance criteria recorded on the initial issue (beads_rust#408).
    // Markdown checklists routinely begin with a list marker ("- [ ] item"),
    // which clap otherwise parses as an unknown flag.
    #[arg(long, visible_alias = "acceptance", allow_hyphen_values = true)]
    pub acceptance_criteria: Option<String>,

    /// Set the initial `agent_context` governing-instructions JSON
    /// (beads_rust#408).
    ///
    /// Accepts the same forms as `br update --agent-context`: inline JSON,
    /// `@path/to/file.json`, or `@path/to/file.yaml` (normalized to JSON).
    /// An empty string leaves the field unset. Invalid context is rejected
    /// before any issue, event, or JSONL record is created.
    #[arg(long = "agent-context")]
    pub agent_context: Option<String>,

    /// Preview without creating
    #[arg(long)]
    pub dry_run: bool,

    /// Output only issue ID
    #[arg(long)]
    pub silent: bool,

    /// Create issues from a markdown file (bulk import)
    #[arg(long, short = 'f')]
    pub file: Option<std::path::PathBuf>,

    // Tier 1 attribution (issue #312, Layer 3 — capture-only). Recorded on the
    // creation audit event as a trail; NEVER gated or enforced on. Match the
    // flag/env names used by `br close`.
    /// Tier 1 attribution: agent name (env: BR_AGENT_NAME). Recorded only.
    #[arg(long, value_name = "NAME", env = "BR_AGENT_NAME")]
    pub agent_name: Option<String>,

    /// Tier 1 attribution: harness identifier (env: BR_HARNESS). Recorded only.
    #[arg(long, value_name = "HARNESS", env = "BR_HARNESS")]
    pub harness: Option<String>,

    /// Tier 1 attribution: model identifier (env: BR_MODEL). Recorded only.
    #[arg(long, value_name = "MODEL", env = "BR_MODEL")]
    pub model: Option<String>,
}

#[derive(Args, Debug)]
pub struct QuickArgs {
    /// Issue title words
    pub title: Vec<String>,

    /// Priority (0-4 or P0-P4)
    #[arg(long, short = 'p', add = ArgValueCompleter::new(priority_completer))]
    pub priority: Option<String>,

    /// Issue type (task, bug, feature, etc.)
    #[arg(long = "type", short = 't', add = ArgValueCompleter::new(issue_type_completer))]
    pub type_: Option<String>,

    /// Labels to apply (repeatable, comma-separated allowed)
    #[arg(long, short = 'l', add = ArgValueCompleter::new(label_completer))]
    pub labels: Vec<String>,

    /// Description
    // Markdown bodies routinely begin with a list marker ("- item"), which
    // clap otherwise parses as an unknown flag in the space-separated form.
    #[arg(long, short = 'd', visible_alias = "body", allow_hyphen_values = true)]
    pub description: Option<String>,

    /// Parent issue ID (creates parent-child dep)
    #[arg(long, add = ArgValueCompleter::new(issue_id_completer))]
    pub parent: Option<String>,

    /// Time estimate in minutes
    #[arg(long, short = 'e')]
    pub estimate: Option<i32>,
}

#[derive(Args, Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct UpdateArgs {
    /// Issue IDs to update
    #[arg(add = ArgValueCompleter::new(issue_id_completer))]
    pub ids: Vec<String>,

    /// Update title
    #[arg(long)]
    pub title: Option<String>,

    /// Update description
    // Markdown bodies routinely begin with a list marker ("- item"), which
    // clap otherwise parses as an unknown flag in the space-separated form.
    #[arg(long, short = 'd', visible_alias = "body", allow_hyphen_values = true)]
    pub description: Option<String>,

    /// Set the issue description verbatim from a file (or `-` for stdin),
    /// replacing the existing value. The entire file content is used
    /// unchanged from disk. Mutually exclusive with `-d/--description`;
    /// use this for multi-paragraph / markdown descriptions that are
    /// fragile to pass through shell quoting.
    #[arg(long, value_name = "PATH", conflicts_with = "description")]
    pub description_file: Option<PathBuf>,

    /// Update design notes
    #[arg(long, allow_hyphen_values = true)]
    pub design: Option<String>,

    /// Update acceptance criteria
    #[arg(long, visible_alias = "acceptance", allow_hyphen_values = true)]
    pub acceptance_criteria: Option<String>,

    /// Update additional notes
    #[arg(long, allow_hyphen_values = true)]
    pub notes: Option<String>,

    /// New comment bound atomically to the requested status transition.
    /// Required when policy lists `transition_comment` for the transition.
    #[arg(long, value_name = "COMMENT", allow_hyphen_values = true)]
    pub transition_comment: Option<String>,

    /// Change status. Terminal states (`closed`, `tombstone`) are refused —
    /// use the dedicated `br close` / `br delete` commands so close-policy
    /// and dependency-rewiring are enforced (beads_rust#301).
    #[arg(long, short = 's', add = ArgValueCompleter::new(status_completer))]
    pub status: Option<String>,

    /// Change priority (0-4 or P0-P4)
    #[arg(long, short = 'p', add = ArgValueCompleter::new(priority_completer))]
    pub priority: Option<String>,

    /// Change issue type
    #[arg(long = "type", short = 't', add = ArgValueCompleter::new(issue_type_completer))]
    pub type_: Option<String>,

    /// Assign to user (empty string clears)
    #[arg(long, add = ArgValueCompleter::new(assignee_completer))]
    pub assignee: Option<String>,

    /// Set owner (empty string clears)
    #[arg(long, add = ArgValueCompleter::new(owner_completer))]
    pub owner: Option<String>,

    /// Atomic claim (assignee=actor + `status=in_progress`)
    #[arg(long)]
    pub claim: bool,

    /// Force update even if issue is blocked
    #[arg(long)]
    pub force: bool,

    /// Set due date (empty string clears)
    #[arg(long)]
    pub due: Option<String>,

    /// Set defer until date (empty string clears)
    #[arg(long)]
    pub defer: Option<String>,

    /// Set time estimate
    #[arg(long)]
    pub estimate: Option<i32>,

    /// Add label(s)
    #[arg(long, add = ArgValueCompleter::new(label_completer))]
    pub add_label: Vec<String>,

    /// Remove label(s)
    #[arg(long, add = ArgValueCompleter::new(label_completer))]
    pub remove_label: Vec<String>,

    /// Set label(s) (replaces all) - repeatable like bd
    #[arg(long, visible_alias = "labels", add = ArgValueCompleter::new(label_completer_delimited))]
    pub set_labels: Vec<String>,

    /// Reparent to new parent (empty string removes parent)
    #[arg(long, add = ArgValueCompleter::new(issue_id_completer))]
    pub parent: Option<String>,

    /// Set external reference
    #[arg(long)]
    pub external_ref: Option<String>,

    /// Override `source_repo` (display name; usually the repo's basename, e.g. `widget_engine`)
    #[arg(long = "source-repo")]
    pub source_repo: Option<String>,

    /// Override `source_repo_path` (absolute path to the directory containing
    /// the `.beads` folder; populates the canonical filesystem location of the
    /// repo for cross-machine sync awareness — see #289)
    #[arg(long = "source-repo-path")]
    pub source_repo_path: Option<String>,

    /// Set the `agent_context` governing-instructions JSON (beads_rust#297).
    /// Accepts inline JSON or a `@path` to a JSON or YAML file (extension
    /// determines parser; YAML is normalized to JSON before storage).
    /// Pass `--agent-context ""` (empty string) to clear the field back
    /// to NULL. Emitted on descendant `br show` / `br update --status
    /// in_progress` / `--claim` when `inherited_context.enabled` is set
    /// in `.beads/config.yaml`.
    #[arg(long = "agent-context")]
    pub agent_context: Option<String>,

    /// Set `closed_by_session` when closing
    #[arg(long)]
    pub session: Option<String>,

    // Tier 1 attribution (issue #312, Layer 3 — capture-only). Recorded on the
    // update/status-change audit event as a trail; NEVER gated or enforced on.
    // Match the flag/env names used by `br close`.
    /// Tier 1 attribution: agent name (env: BR_AGENT_NAME). Recorded only.
    #[arg(long, value_name = "NAME", env = "BR_AGENT_NAME")]
    pub agent_name: Option<String>,

    /// Tier 1 attribution: harness identifier (env: BR_HARNESS). Recorded only.
    #[arg(long, value_name = "HARNESS", env = "BR_HARNESS")]
    pub harness: Option<String>,

    /// Tier 1 attribution: model identifier (env: BR_MODEL). Recorded only.
    #[arg(long, value_name = "MODEL", env = "BR_MODEL")]
    pub model: Option<String>,
}

#[derive(Args, Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct DeleteArgs {
    /// Issue IDs to delete
    #[arg(add = ArgValueCompleter::new(issue_id_completer))]
    pub ids: Vec<String>,

    /// Delete reason (default: "delete")
    #[arg(long, default_value = "delete")]
    pub reason: String,

    /// Read IDs from file (one per line, # comments ignored)
    #[arg(long)]
    pub from_file: Option<PathBuf>,

    /// Delete dependents recursively
    #[arg(long)]
    pub cascade: bool,

    /// Bypass dependent checks (orphans dependents)
    #[arg(long, conflicts_with = "cascade")]
    pub force: bool,

    /// Prune tombstones from JSONL immediately
    #[arg(long)]
    pub hard: bool,

    /// Preview only, no changes
    #[arg(long)]
    pub dry_run: bool,
}

/// Arguments for the info command.
#[derive(Args, Debug, Default, Clone)]
pub struct InfoArgs {
    /// Include schema details
    #[arg(long)]
    pub schema: bool,

    /// Include graph projection/cache health details
    #[arg(long)]
    pub projections: bool,

    /// Show recent changes and exit
    #[arg(long = "whats-new", conflicts_with = "thanks")]
    pub whats_new: bool,

    /// Show acknowledgements and exit
    #[arg(long, conflicts_with = "whats_new")]
    pub thanks: bool,
}

/// Arguments for the explicit VCS export-status diagnostic.
#[derive(Args, Debug, Clone)]
pub struct VcsStatusArgs {
    /// Inspect this JSONL instead of the configured workspace export
    #[arg(long, value_name = "PATH")]
    pub jsonl: Option<PathBuf>,

    /// Permit an explicitly selected JSONL outside the workspace metadata directory
    #[arg(long)]
    pub allow_external_jsonl: bool,

    /// Shared execution budget for source capture, Git probes, and raw hashing
    ///
    /// Direct-child termination/reaping is cleanup and may extend past this
    /// budget. Capture and hashing check the deadline between bounded reads;
    /// an individual filesystem read cannot itself be preempted.
    #[arg(long, default_value_t = 2_000, value_name = "MILLISECONDS")]
    pub timeout_ms: u64,

    /// Machine-readable output (alias for --json)
    #[arg(long)]
    pub robot: bool,
}

impl Default for VcsStatusArgs {
    fn default() -> Self {
        Self {
            jsonl: None,
            allow_external_jsonl: false,
            timeout_ms: 2_000,
            robot: false,
        }
    }
}

/// Arguments for the schema command.
#[derive(Args, Debug, Default, Clone)]
pub struct SchemaArgs {
    /// Which schema to emit
    #[arg(value_enum, default_value_t)]
    pub target: SchemaTarget,

    /// Output format (text, json, toon). Env: BR_OUTPUT_FORMAT, TOON_DEFAULT_FORMAT.
    #[arg(long, value_enum)]
    pub format: Option<OutputFormatBasic>,

    /// Show token savings stats when using TOON output
    #[arg(long)]
    pub stats: bool,
}

/// Subcommands for coordination diagnosis.
#[derive(Subcommand, Debug)]
pub enum CoordinationCommands {
    /// Show hidden in-progress claims with stale-claim evidence
    Status(CoordinationStatusArgs),
}

/// Arguments for `br coordination status`.
#[derive(Args, Debug, Clone, Default)]
pub struct CoordinationStatusArgs {
    /// Assumed owner kind for current claims when no snapshot supplies owner metadata
    #[arg(long, value_enum, default_value_t)]
    pub owner_kind: CoordinationOwnerKindArg,

    /// Number of latest comments to include per claim
    #[arg(long, default_value_t = 2)]
    pub comments: usize,

    /// Offline Agent Mail reservation snapshot file (JSON array, wrapper object, or JSONL)
    #[arg(long)]
    pub reservations: Option<PathBuf>,

    /// Offline Agent Mail agent snapshot file (JSON array, wrapper object, or JSONL)
    #[arg(long)]
    pub agents: Option<PathBuf>,

    /// Output format (text, json, toon). Env: BR_OUTPUT_FORMAT, TOON_DEFAULT_FORMAT.
    #[arg(long, value_enum)]
    pub format: Option<OutputFormatBasic>,

    /// Show token savings stats when using TOON output
    #[arg(long)]
    pub stats: bool,

    /// Machine-readable output (alias for --json)
    #[arg(long)]
    pub robot: bool,
}

/// Owner-kind policy for coordination claim assessment.
#[derive(ValueEnum, Debug, Clone, Copy, Default, Eq, PartialEq)]
pub enum CoordinationOwnerKindArg {
    /// Treat assigned in-progress claims as swarm-agent claims
    #[default]
    SwarmAgent,
    /// Treat assigned claims as human-owned
    Human,
    /// Treat assigned claims as unclear ownership
    Unknown,
}

/// Schema targets for `br schema`.
#[derive(ValueEnum, Debug, Clone, Copy, Default, Eq, PartialEq)]
pub enum SchemaTarget {
    /// Emit a bundle containing all schemas and the per-command shape map
    #[default]
    All,
    /// Core Issue object (used by many commands)
    Issue,
    /// List/search row: Issue + dependency/dependent counts
    IssueWithCounts,
    /// Show view: Issue + relations/comments/events
    IssueDetails,
    /// Ready list row
    ReadyIssue,
    /// Stale list row
    StaleIssue,
    /// Blocked list row
    BlockedIssue,
    /// Dependency tree node
    TreeNode,
    /// Stats output
    Statistics,
    /// Coordination status output
    CoordinationStatus,
    /// Additive reconciliation plan/apply receipt
    AdditiveReconciliation,
    /// Explicit VCS export-status diagnostic
    VcsStatus,
    /// Structured error envelope (stderr JSON when robot mode or non-TTY)
    Error,
    /// Per-command JSON output envelope map (top-level shape + jq filter per command)
    Commands,
}

/// Arguments for the capabilities command.
#[derive(Args, Debug, Default, Clone)]
pub struct CapabilitiesArgs {
    /// Include detailed metadata for one command path, e.g. "create" or "comments add"
    #[arg(long, visible_alias = "for", value_name = "COMMAND_PATH")]
    pub command: Option<String>,

    /// Output format (text, json, toon). Env: BR_OUTPUT_FORMAT, TOON_DEFAULT_FORMAT.
    #[arg(long, value_enum)]
    pub format: Option<OutputFormatBasic>,

    /// Show token savings stats when using TOON output
    #[arg(long)]
    pub stats: bool,
}

/// Subcommands for robot-oriented in-tool documentation.
#[derive(Subcommand, Debug, Clone)]
pub enum RobotDocsCommands {
    /// Print the concise agent guide
    Guide(RobotDocsGuideArgs),
}

/// Arguments for `br robot-docs guide`.
#[derive(Args, Debug, Default, Clone)]
pub struct RobotDocsGuideArgs {
    /// Output format (text, json, toon). Env: BR_OUTPUT_FORMAT, TOON_DEFAULT_FORMAT.
    #[arg(long, value_enum)]
    pub format: Option<OutputFormatBasic>,

    /// Show token savings stats when using TOON output
    #[arg(long)]
    pub stats: bool,
}

/// Output format for list command.
#[derive(ValueEnum, Debug, Clone, Copy, Default, Eq, PartialEq)]
pub enum OutputFormat {
    /// Human-readable text (default)
    #[default]
    Text,
    /// JSON output
    Json,
    /// CSV output with configurable fields
    Csv,
    /// TOON format (token-optimized object notation)
    Toon,
}

impl OutputFormat {
    /// Resolve output format from environment variables.
    ///
    /// Precedence: BR_OUTPUT_FORMAT > TOON_DEFAULT_FORMAT.
    #[must_use]
    pub fn from_env() -> Option<Self> {
        if let Ok(value) = std::env::var("BR_OUTPUT_FORMAT")
            && let Some(format) = Self::parse_env_value(&value)
        {
            return Some(format);
        }
        if let Ok(value) = std::env::var("TOON_DEFAULT_FORMAT")
            && let Some(format) = Self::parse_env_value(&value)
        {
            return Some(format);
        }
        None
    }

    fn parse_env_value(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "text" | "plain" => Some(Self::Text),
            "json" => Some(Self::Json),
            "csv" => Some(Self::Csv),
            "toon" => Some(Self::Toon),
            _ => None,
        }
    }
}

/// Output format for commands that don't support CSV.
#[derive(ValueEnum, Debug, Clone, Copy, Default, Eq, PartialEq)]
pub enum OutputFormatBasic {
    /// Human-readable text (default)
    #[default]
    Text,
    /// JSON output
    Json,
    /// TOON format (token-optimized object notation)
    Toon,
}

impl From<OutputFormatBasic> for OutputFormat {
    fn from(format: OutputFormatBasic) -> Self {
        match format {
            OutputFormatBasic::Text => Self::Text,
            OutputFormatBasic::Json => Self::Json,
            OutputFormatBasic::Toon => Self::Toon,
        }
    }
}

/// Resolve effective output format with CLI/env precedence.
#[must_use]
pub fn resolve_output_format(
    requested: Option<OutputFormat>,
    json: bool,
    robot: bool,
) -> OutputFormat {
    if json || robot {
        OutputFormat::Json
    } else if let Some(requested) = requested {
        requested
    } else {
        OutputFormat::from_env().unwrap_or(OutputFormat::Text)
    }
}

/// Returns true when a subcommand-local `--robot` flag requests JSON output.
#[must_use]
pub const fn command_requests_robot_json(cmd: &Commands) -> bool {
    match cmd {
        Commands::Close(args) => args.robot,
        Commands::Coordination { command } => coordination_command_requests_robot_json(command),
        Commands::Reopen(args) => args.robot,
        Commands::Ready(args) => args.robot,
        Commands::Scheduler(args) => args.robot,
        Commands::Blocked(args) => args.robot,
        Commands::Stats(args) | Commands::Status(args) => args.robot,
        Commands::Defer(args) => args.robot,
        Commands::Undefer(args) => args.robot,
        Commands::Orphans(args) => args.robot,
        Commands::Changelog(args) => args.robot,
        Commands::Sync(args) => args.robot,
        Commands::VcsStatus(args) => args.robot,
        Commands::Doctor(args) => args.robot_triage,
        Commands::Dep { command } => match command {
            DepCommands::Import(args) => args.robot,
            DepCommands::Add(_)
            | DepCommands::Remove(_)
            | DepCommands::List(_)
            | DepCommands::Tree(_)
            | DepCommands::Cycles(_) => false,
        },
        Commands::Gate { command } => match command {
            GateCommands::Report(args) => args.robot,
            GateCommands::List(args) => args.robot,
        },
        Commands::Capacity { command } => match command {
            CapacityCommands::Exempt(args) => args.robot,
            CapacityCommands::Renew(args) => args.robot,
            CapacityCommands::Revoke(args) => args.robot,
            CapacityCommands::Exemptions(args) => args.robot,
        },
        _ => false,
    }
}

const fn coordination_command_requests_robot_json(command: &CoordinationCommands) -> bool {
    match command {
        CoordinationCommands::Status(args) => args.robot,
    }
}

/// Machine-readable or quiet state inherited from an outer command context.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum InheritedOutputMode {
    None,
    Quiet,
    Json,
    Toon,
}

/// Resolve effective output format while preserving an already-selected outer mode.
///
/// This is used by command wrappers and subcommands that inherit global output
/// state from an existing [`OutputContext`]. Structured outer modes are carried
/// through when no command-specific format was requested, while global quiet
/// suppresses env-selected machine formats unless the command explicitly
/// requested one.
#[must_use]
pub fn resolve_output_format_with_outer_mode(
    requested: Option<OutputFormat>,
    inherited_mode: InheritedOutputMode,
    robot: bool,
) -> OutputFormat {
    if matches!(inherited_mode, InheritedOutputMode::Json) || robot {
        OutputFormat::Json
    } else if let Some(requested) = requested {
        requested
    } else if matches!(inherited_mode, InheritedOutputMode::Toon) {
        OutputFormat::Toon
    } else if matches!(inherited_mode, InheritedOutputMode::Quiet) {
        OutputFormat::Text
    } else {
        OutputFormat::from_env().unwrap_or(OutputFormat::Text)
    }
}

/// Resolve effective output format for commands without CSV support.
#[must_use]
pub fn resolve_output_format_basic(
    requested: Option<OutputFormatBasic>,
    json: bool,
    robot: bool,
) -> OutputFormat {
    let resolved = resolve_output_format(requested.map(Into::into), json, robot);
    match resolved {
        OutputFormat::Csv => OutputFormat::Text,
        other => other,
    }
}

/// Resolve effective output format for commands without CSV support while
/// preserving an already-selected outer mode.
#[must_use]
pub fn resolve_output_format_basic_with_outer_mode(
    requested: Option<OutputFormatBasic>,
    inherited_mode: InheritedOutputMode,
    robot: bool,
) -> OutputFormat {
    let resolved =
        resolve_output_format_with_outer_mode(requested.map(Into::into), inherited_mode, robot);
    match resolved {
        OutputFormat::Csv => OutputFormat::Text,
        other => other,
    }
}

/// Arguments for the list command.
#[derive(Args, Debug, Default, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct ListArgs {
    /// Filter by status (can be repeated; 'all' matches every status)
    #[arg(long, short = 's', add = ArgValueCompleter::new(status_completer))]
    pub status: Vec<String>,

    /// Filter by issue type (can be repeated)
    #[arg(long = "type", short = 't', add = ArgValueCompleter::new(issue_type_completer))]
    pub type_: Vec<String>,

    /// Filter by assignee
    #[arg(long, add = ArgValueCompleter::new(assignee_completer))]
    pub assignee: Option<String>,

    /// Filter for unassigned issues only
    #[arg(long)]
    pub unassigned: bool,

    /// Filter by specific IDs (can be repeated)
    #[arg(long, add = ArgValueCompleter::new(issue_id_completer))]
    pub id: Vec<String>,

    /// Filter by label (AND logic, can be repeated)
    #[arg(long, short = 'l', add = ArgValueCompleter::new(label_completer))]
    pub label: Vec<String>,

    /// Filter by label (OR logic, can be repeated)
    #[arg(long, add = ArgValueCompleter::new(label_completer))]
    pub label_any: Vec<String>,

    /// Filter by priority (can be repeated)
    #[arg(long, short = 'p', add = ArgValueCompleter::new(priority_completer))]
    pub priority: Vec<String>,

    /// Filter by minimum priority (0=critical, 4=backlog)
    #[arg(long, add = ArgValueCompleter::new(priority_numeric_completer))]
    pub priority_min: Option<u8>,

    /// Filter by maximum priority
    #[arg(long, add = ArgValueCompleter::new(priority_numeric_completer))]
    pub priority_max: Option<u8>,

    /// Title contains substring
    #[arg(long)]
    pub title_contains: Option<String>,

    /// Description contains substring
    #[arg(long)]
    pub desc_contains: Option<String>,

    /// Notes contains substring
    #[arg(long)]
    pub notes_contains: Option<String>,

    /// Include closed issues (default excludes closed)
    #[arg(long, short = 'a')]
    pub all: bool,

    /// Maximum number of results (0 = unlimited; default: unlimited — the full work surface)
    #[arg(long)]
    pub limit: Option<usize>,

    /// Number of results to skip (for pagination, default: 0)
    #[arg(long)]
    pub offset: Option<usize>,

    /// Sort field (`priority`, `created_at`, `updated_at`, `title`)
    #[arg(long, add = ArgValueCompleter::new(sort_key_completer))]
    pub sort: Option<String>,

    /// Reverse sort order
    #[arg(long, short = 'r')]
    pub reverse: bool,

    /// Include deferred issues
    #[arg(long)]
    pub deferred: bool,

    /// Filter for overdue issues
    #[arg(long)]
    pub overdue: bool,

    /// Use long output format
    #[arg(long)]
    pub long: bool,

    /// Use tree/pretty output format
    #[arg(long)]
    pub pretty: bool,

    /// Wrap long lines instead of truncating in text output
    #[arg(long)]
    pub wrap: bool,

    /// Output format (text, json, csv, toon). Env: BR_OUTPUT_FORMAT, TOON_DEFAULT_FORMAT.
    #[arg(long, value_enum)]
    pub format: Option<OutputFormat>,

    /// Show token savings stats when using TOON output
    #[arg(long)]
    pub stats: bool,

    /// CSV fields to include (comma-separated)
    ///
    /// Available: id, title, description, status, priority, `issue_type`,
    /// assignee, owner, `created_at`, `updated_at`, `closed_at`, `due_at`,
    /// `defer_until`, notes, `external_ref`
    ///
    /// Default: id, title, status, priority, `issue_type`, assignee, `created_at`, `updated_at`
    #[arg(long, value_name = "FIELDS", add = ArgValueCompleter::new(csv_fields_completer))]
    pub fields: Option<String>,
}

/// Arguments for the search command.
#[derive(Args, Debug, Default)]
pub struct SearchArgs {
    /// Search query
    pub query: String,

    #[command(flatten)]
    pub filters: ListArgs,
}

/// Arguments for the show command.
#[derive(Args, Debug, Clone, Default)]
pub struct ShowArgs {
    /// Issue IDs
    #[arg(add = ArgValueCompleter::new(issue_id_completer))]
    pub ids: Vec<String>,

    /// Output format (text, json, toon). Env: BR_OUTPUT_FORMAT, TOON_DEFAULT_FORMAT.
    #[arg(long, value_enum)]
    pub format: Option<OutputFormatBasic>,

    /// Soft-wrap long lines to the terminal width in text output. Wrapping is
    /// now on by default; this flag is kept for backwards compatibility.
    #[arg(long)]
    pub wrap: bool,

    /// Disable soft-wrapping; let long description/comment lines extend past the
    /// panel width (the pre-#370 behavior).
    #[arg(long, conflicts_with = "wrap")]
    pub no_wrap: bool,

    /// Show token savings stats when using TOON output
    #[arg(long)]
    pub stats: bool,
}

#[derive(Subcommand, Debug)]
pub enum DepCommands {
    /// Add a dependency: <issue> depends on <depends-on>
    Add(DepAddArgs),
    /// Bulk import dependency edges from JSONL
    Import(DepImportArgs),
    /// Remove a dependency
    #[command(visible_alias = "rm")]
    Remove(DepRemoveArgs),
    /// List dependencies of an issue
    List(DepListArgs),
    /// Show dependency tree rooted at issue
    Tree(DepTreeArgs),
    /// Detect and report dependency cycles
    Cycles(DepCyclesArgs),
}

/// Subcommands for the epic command.
#[derive(Subcommand, Debug)]
pub enum EpicCommands {
    /// Show status of all epics (progress, eligibility)
    Status(EpicStatusArgs),
    /// Close epics that are eligible (all children closed)
    #[command(name = "close-eligible")]
    CloseEligible(EpicCloseEligibleArgs),
}

/// Arguments for the epic status command.
#[derive(Args, Debug, Clone, Default)]
pub struct EpicStatusArgs {
    /// Only show epics eligible for closure
    #[arg(long)]
    pub eligible_only: bool,
}

/// Arguments for the epic close-eligible command.
#[derive(Args, Debug, Clone, Default)]
pub struct EpicCloseEligibleArgs {
    /// Preview only, no changes
    #[arg(long)]
    pub dry_run: bool,

    /// New comment committed atomically with every eligible epic close.
    #[arg(long, value_name = "COMMENT", allow_hyphen_values = true)]
    pub transition_comment: Option<String>,
}

/// Subcommands for the workflow gate engine (issue #312, layer 2).
#[derive(Subcommand, Debug)]
pub enum GateCommands {
    /// Record a gate result for an issue (external systems / reviewers report here)
    Report(GateReportArgs),
    /// List recorded gate results and required-gate status for an issue
    List(GateListArgs),
}

/// Status reported for a gate result.
#[derive(ValueEnum, Debug, Clone, Copy, Eq, PartialEq)]
pub enum GateStatus {
    /// The gate passed.
    Pass,
    /// The gate failed.
    Fail,
}

/// Arguments for `br gate report`.
#[derive(Args, Debug, Clone)]
pub struct GateReportArgs {
    /// Issue ID to record the gate result against
    #[arg(add = ArgValueCompleter::new(issue_id_completer))]
    pub id: String,

    /// Gate name (e.g. ci_green, security_sign_off, min_reviewers)
    #[arg(long)]
    pub gate: String,

    /// Reporting provider (e.g. ci, security, reviewer:alice)
    #[arg(long)]
    pub provider: String,

    /// Result status: pass or fail
    #[arg(long, value_enum)]
    pub status: GateStatus,

    /// Target status this verdict is intended to authorize. May be omitted
    /// only when policy has exactly one matching gated target from the issue's
    /// current status for this gate.
    #[arg(long, value_name = "STATUS", add = ArgValueCompleter::new(status_completer))]
    pub to: Option<String>,

    /// Optional free-form note recorded with the result
    #[arg(long)]
    pub note: Option<String>,

    /// Emit machine-readable JSON
    #[arg(long)]
    pub robot: bool,
}

/// Arguments for `br gate list`.
#[derive(Args, Debug, Clone)]
pub struct GateListArgs {
    /// Issue ID whose gate results to show
    #[arg(add = ArgValueCompleter::new(issue_id_completer))]
    pub id: String,

    /// Emit machine-readable JSON
    #[arg(long)]
    pub robot: bool,
}

/// Subcommands for workflow capacity management (GitHub #384 phase 4).
#[derive(Subcommand, Debug)]
pub enum CapacityCommands {
    /// Grant an audited issue-specific exemption from one named capacity
    Exempt(CapacityExemptArgs),
    /// Renew an active exemption's expiry
    Renew(CapacityRenewArgs),
    /// Revoke an active exemption
    Revoke(CapacityRevokeArgs),
    /// List exemption state (and optionally the append-only audit history)
    Exemptions(CapacityExemptionsArgs),
}

/// Arguments for `br capacity exempt`.
#[derive(Args, Debug, Clone)]
pub struct CapacityExemptArgs {
    /// Issue the exemption applies to (one issue, one named capacity)
    #[arg(add = ArgValueCompleter::new(issue_id_completer))]
    pub id: String,

    /// Named status capacity to exempt the issue from (e.g. blocked)
    #[arg(long, value_name = "STATUS", conflicts_with = "group", add = ArgValueCompleter::new(status_completer))]
    pub status: Option<String>,

    /// Named capacity group to exempt the issue from
    #[arg(long, value_name = "GROUP", conflicts_with = "status")]
    pub group: Option<String>,

    /// Approving provider; must be listed in workflow.capacity.exemptions.providers
    #[arg(long)]
    pub provider: String,

    /// Mandatory rationale recorded with the grant
    #[arg(long)]
    pub reason: String,

    /// Expiration: RFC3339, YYYY-MM-DD, or relative (+7d). Required when
    /// policy sets workflow.capacity.exemptions.require_expiry
    #[arg(long, value_name = "WHEN")]
    pub expires: Option<String>,

    /// Emit machine-readable JSON
    #[arg(long)]
    pub robot: bool,
}

/// Arguments for `br capacity renew`.
#[derive(Args, Debug, Clone)]
pub struct CapacityRenewArgs {
    /// Issue whose exemption to renew
    #[arg(add = ArgValueCompleter::new(issue_id_completer))]
    pub id: String,

    /// Named status capacity of the exemption
    #[arg(long, value_name = "STATUS", conflicts_with = "group", add = ArgValueCompleter::new(status_completer))]
    pub status: Option<String>,

    /// Named capacity group of the exemption
    #[arg(long, value_name = "GROUP", conflicts_with = "status")]
    pub group: Option<String>,

    /// Approving provider; must be listed in workflow.capacity.exemptions.providers
    #[arg(long)]
    pub provider: String,

    /// New expiration: RFC3339, YYYY-MM-DD, or relative (+7d)
    #[arg(long, value_name = "WHEN")]
    pub expires: Option<String>,

    /// Optional note recorded with the renewal
    #[arg(long)]
    pub reason: Option<String>,

    /// Emit machine-readable JSON
    #[arg(long)]
    pub robot: bool,
}

/// Arguments for `br capacity revoke`.
#[derive(Args, Debug, Clone)]
pub struct CapacityRevokeArgs {
    /// Issue whose exemption to revoke
    #[arg(add = ArgValueCompleter::new(issue_id_completer))]
    pub id: String,

    /// Named status capacity of the exemption
    #[arg(long, value_name = "STATUS", conflicts_with = "group", add = ArgValueCompleter::new(status_completer))]
    pub status: Option<String>,

    /// Named capacity group of the exemption
    #[arg(long, value_name = "GROUP", conflicts_with = "status")]
    pub group: Option<String>,

    /// Revoking provider, recorded for audit (not required to be authorized)
    #[arg(long)]
    pub provider: String,

    /// Optional note recorded with the revocation
    #[arg(long)]
    pub reason: Option<String>,

    /// Emit machine-readable JSON
    #[arg(long)]
    pub robot: bool,
}

/// Arguments for `br capacity exemptions`.
#[derive(Args, Debug, Clone)]
pub struct CapacityExemptionsArgs {
    /// Restrict to one issue (all exemptions when omitted)
    #[arg(add = ArgValueCompleter::new(issue_id_completer))]
    pub id: Option<String>,

    /// Include the append-only audit history
    #[arg(long)]
    pub history: bool,

    /// Emit machine-readable JSON
    #[arg(long)]
    pub robot: bool,
}

#[derive(Args, Debug, Default)]
pub struct DepAddArgs {
    /// Issue ID (the one that will depend on something)
    #[arg(add = ArgValueCompleter::new(issue_id_completer))]
    pub issue: String,

    /// Target issue ID (the one being depended on)
    #[arg(add = ArgValueCompleter::new(issue_id_completer))]
    pub depends_on: String,

    /// Dependency type (blocks, parent-child, related, etc.)
    #[arg(long = "type", short = 't', default_value = "blocks", add = ArgValueCompleter::new(dep_type_completer))]
    pub dep_type: String,

    /// Optional JSON metadata
    #[arg(long)]
    pub metadata: Option<String>,
}

#[derive(Args, Debug)]
pub struct DepImportArgs {
    /// JSONL file containing edge objects or issue records with dependencies
    pub path: PathBuf,

    /// Machine-readable output (alias for --json)
    #[arg(long)]
    pub robot: bool,
}

#[derive(Args, Debug)]
pub struct DepRemoveArgs {
    /// Issue ID
    #[arg(add = ArgValueCompleter::new(issue_id_completer))]
    pub issue: String,

    /// Target issue ID to remove dependency to
    #[arg(add = ArgValueCompleter::new(issue_id_completer))]
    pub depends_on: String,
}

#[derive(Args, Debug)]
pub struct DepListArgs {
    /// Issue ID
    #[arg(add = ArgValueCompleter::new(issue_id_completer))]
    pub issue: String,

    /// Direction: down (what issue depends on), up (what depends on issue), both
    #[arg(long, default_value = "down", value_enum)]
    pub direction: DepDirection,

    /// Filter by dependency type
    #[arg(long = "type", short = 't', add = ArgValueCompleter::new(dep_type_completer))]
    pub dep_type: Option<String>,

    /// Output format (text, json, toon). Env: BR_OUTPUT_FORMAT, TOON_DEFAULT_FORMAT.
    #[arg(long, value_enum)]
    pub format: Option<OutputFormatBasic>,

    /// Show token savings stats when using TOON output
    #[arg(long)]
    pub stats: bool,
}

#[derive(ValueEnum, Debug, Clone, Copy, Default, Eq, PartialEq)]
pub enum DepDirection {
    /// Dependencies this issue has (what it waits on)
    #[default]
    Down,
    /// Dependents (what waits on this issue)
    Up,
    /// Both directions
    Both,
}

#[derive(Args, Debug)]
pub struct DepTreeArgs {
    /// Issue ID (root of tree)
    #[arg(add = ArgValueCompleter::new(issue_id_completer))]
    pub issue: String,

    /// Tree direction (default: down)
    #[arg(long, short = 'd', default_value = "down", value_enum)]
    pub direction: DepDirection,

    /// Maximum depth (default: 10)
    #[arg(long, default_value_t = 10)]
    pub max_depth: usize,

    /// Output format: text, mermaid
    #[arg(long, default_value = "text", add = ArgValueCompleter::new(dep_tree_format_completer))]
    pub format: String,
}

#[derive(Args, Debug)]
pub struct DepCyclesArgs {
    /// Only check blocking dependency types
    #[arg(long)]
    pub blocking_only: bool,
    /// Include archived cycles where every issue is closed or tombstoned
    #[arg(long)]
    pub include_closed: bool,
}

#[derive(Subcommand, Debug)]
pub enum LabelCommands {
    /// Add label(s) to issue(s)
    Add(LabelAddArgs),
    /// Remove label(s) from issue(s)
    Remove(LabelRemoveArgs),
    /// List labels for an issue or all unique labels
    List(LabelListArgs),
    /// List all unique labels with counts
    #[command(name = "list-all")]
    ListAll,
    /// Rename a label across all issues
    Rename(LabelRenameArgs),
}

#[derive(Args, Debug)]
pub struct LabelAddArgs {
    /// Issue ID(s) to add label to
    #[arg(add = ArgValueCompleter::new(issue_id_completer))]
    pub issues: Vec<String>,

    /// Label to add
    #[arg(long, short = 'l', add = ArgValueCompleter::new(label_completer))]
    pub label: Option<String>,
}

#[derive(Args, Debug)]
pub struct LabelRemoveArgs {
    /// Issue ID(s) to remove label from
    #[arg(add = ArgValueCompleter::new(issue_id_completer))]
    pub issues: Vec<String>,

    /// Label to remove
    #[arg(long, short = 'l', add = ArgValueCompleter::new(label_completer))]
    pub label: Option<String>,
}

#[derive(Args, Debug)]
pub struct LabelListArgs {
    /// Issue ID (optional - if omitted, lists all unique labels)
    #[arg(add = ArgValueCompleter::new(issue_id_completer))]
    pub issue: Option<String>,
}

#[derive(Args, Debug)]
pub struct LabelRenameArgs {
    /// Current label name
    #[arg(add = ArgValueCompleter::new(label_completer))]
    pub old_name: String,

    /// New label name
    pub new_name: String,
}

#[derive(Args, Debug)]
pub struct CommentsArgs {
    #[command(subcommand)]
    pub command: Option<CommentCommands>,

    /// Issue ID (for listing comments)
    #[arg(add = ArgValueCompleter::new(issue_id_completer))]
    pub id: Option<String>,

    /// Wrap long lines instead of truncating in text output
    #[arg(long)]
    pub wrap: bool,
}

#[derive(Subcommand, Debug)]
pub enum CommentCommands {
    Add(CommentAddArgs),
    List(CommentListArgs),
}

#[derive(Args, Debug)]
pub struct CommentAddArgs {
    /// Issue ID
    #[arg(add = ArgValueCompleter::new(issue_id_completer))]
    pub id: String,

    /// Comment text
    pub text: Vec<String>,

    /// Read comment text from file
    #[arg(short = 'f', long = "file")]
    pub file: Option<PathBuf>,

    /// Override author (defaults to actor/env/user)
    #[arg(long)]
    pub author: Option<String>,

    /// Comment text (alternative flag)
    #[arg(
        long = "message",
        short = 'm',
        visible_alias = "content",
        allow_hyphen_values = true
    )]
    pub message: Option<String>,
}

#[derive(Args, Debug)]
pub struct CommentListArgs {
    /// Issue ID
    #[arg(add = ArgValueCompleter::new(issue_id_completer))]
    pub id: String,

    /// Wrap long lines instead of truncating in text output
    #[arg(long)]
    pub wrap: bool,
}

#[derive(Subcommand, Debug)]
pub enum AuditCommands {
    /// Append an audit interaction entry
    Record(AuditRecordArgs),
    /// Record coordination status rows as bounded audit interactions
    Coordination(AuditCoordinationArgs),
    /// Append a label entry referencing an existing interaction
    Label(AuditLabelArgs),
    /// View audit log for an issue
    Log(AuditLogArgs),
    /// View audit summary
    Summary(AuditSummaryArgs),
}

#[derive(Args, Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct AuditRecordArgs {
    /// Entry kind (e.g. `llm_call`, `tool_call`, `label`)
    #[arg(long)]
    pub kind: Option<String>,

    /// Related issue ID (bd-...)
    #[arg(long = "issue-id", add = ArgValueCompleter::new(issue_id_completer))]
    pub issue_id: Option<String>,

    /// Model name (`llm_call`)
    #[arg(long)]
    pub model: Option<String>,

    /// Prompt text (`llm_call`)
    #[arg(long)]
    pub prompt: Option<String>,

    /// Response text (`llm_call`)
    #[arg(long)]
    pub response: Option<String>,

    /// Tool name (`tool_call`)
    #[arg(long = "tool-name")]
    pub tool_name: Option<String>,

    /// Exit code (`tool_call`)
    #[arg(long = "exit-code")]
    pub exit_code: Option<i32>,

    /// Error string (`llm_call/tool_call`)
    #[arg(long)]
    pub error: Option<String>,

    /// Read a JSON object from stdin (must match audit.Entry schema)
    #[arg(long)]
    pub stdin: bool,
}

#[derive(Args, Debug, Clone)]
pub struct AuditCoordinationArgs {
    /// Read a coordination status JSON object or JSONL stream from stdin
    #[arg(long)]
    pub stdin: bool,

    /// Command that produced the coordination snapshot
    #[arg(long, default_value = "br coordination status")]
    pub command: String,
}

#[derive(Args, Debug, Clone)]
pub struct AuditLabelArgs {
    /// Parent entry ID
    pub entry_id: String,

    /// Label value (e.g. \"good\" or \"bad\")
    #[arg(long, add = ArgValueCompleter::new(label_completer))]
    pub label: Option<String>,

    /// Reason for label
    #[arg(long)]
    pub reason: Option<String>,
}

#[derive(Args, Debug, Clone)]
pub struct AuditLogArgs {
    /// Issue ID
    #[arg(add = ArgValueCompleter::new(issue_id_completer))]
    pub id: String,
}

#[derive(Args, Debug, Clone, Default)]
pub struct AuditSummaryArgs {
    /// Show summary for last N days (default: 30)
    #[arg(long, default_value_t = 30)]
    pub days: u32,
}

#[derive(Args, Debug, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct CountArgs {
    /// Group counts by field
    #[arg(long, value_enum)]
    pub by: Option<CountBy>,

    /// Group by status (alias for --by status)
    #[arg(long)]
    pub by_status: bool,

    /// Group by priority (alias for --by priority)
    #[arg(long)]
    pub by_priority: bool,

    /// Group by type (alias for --by type)
    #[arg(long)]
    pub by_type: bool,

    /// Group by assignee (alias for --by assignee)
    #[arg(long)]
    pub by_assignee: bool,

    /// Group by label (alias for --by label)
    #[arg(long)]
    pub by_label: bool,

    /// Filter by status (repeatable or comma-separated; 'all' matches every status)
    #[arg(long, value_delimiter = ',', add = ArgValueCompleter::new(status_completer_delimited))]
    pub status: Vec<String>,

    /// Filter by issue type (repeatable or comma-separated)
    #[arg(long = "type", value_delimiter = ',', add = ArgValueCompleter::new(issue_type_completer_delimited))]
    pub types: Vec<String>,

    /// Filter by priority (0-4 or P0-P4; repeatable or comma-separated)
    #[arg(long, value_delimiter = ',', add = ArgValueCompleter::new(priority_completer_delimited))]
    pub priority: Vec<String>,

    /// Filter by assignee
    #[arg(long, add = ArgValueCompleter::new(assignee_completer))]
    pub assignee: Option<String>,

    /// Only include unassigned issues
    #[arg(long)]
    pub unassigned: bool,

    /// Include closed issues; tombstones require `--status tombstone`
    #[arg(long)]
    pub include_closed: bool,

    /// Include template issues
    #[arg(long)]
    pub include_templates: bool,

    /// Title contains substring
    #[arg(long)]
    pub title_contains: Option<String>,
}

#[derive(ValueEnum, Debug, Clone, Copy, Eq, PartialEq)]
pub enum CountBy {
    Status,
    Priority,
    Type,
    Assignee,
    Label,
}

#[derive(Args, Debug, Clone)]
pub struct StaleArgs {
    /// Minimum days since last update
    #[arg(long, default_value_t = 30)]
    pub days: i64,

    /// Filter by status (repeatable or comma-separated)
    #[arg(long, value_delimiter = ',', add = ArgValueCompleter::new(status_completer_delimited))]
    pub status: Vec<String>,
}

#[derive(Args, Debug, Clone, Default)]
pub struct LintArgs {
    /// Issue IDs to lint (defaults to open issues)
    #[arg(add = ArgValueCompleter::new(issue_id_completer))]
    pub ids: Vec<String>,

    /// Filter by issue type (bug, task, feature, epic)
    #[arg(long, short = 't', add = ArgValueCompleter::new(issue_type_standard_completer))]
    pub type_: Option<String>,

    /// Filter by status (default: open, use 'all' for all)
    #[arg(long, short = 's', add = ArgValueCompleter::new(status_or_all_completer))]
    pub status: Option<String>,
}

/// Arguments for the defer command.
#[derive(Args, Debug, Clone, Default)]
pub struct DeferArgs {
    /// Issue IDs to defer
    #[arg(add = ArgValueCompleter::new(open_issue_id_completer))]
    pub ids: Vec<String>,

    /// Defer until date/time (e.g., `+1h`, `tomorrow`, `2025-01-15`)
    #[arg(long)]
    pub until: Option<String>,

    /// Machine-readable output (alias for --json)
    #[arg(long)]
    pub robot: bool,

    /// New comment committed atomically with each defer transition.
    #[arg(long, value_name = "COMMENT", allow_hyphen_values = true)]
    pub transition_comment: Option<String>,

    // Tier 1 attribution (issue #312, Layer 3 — capture-only). Recorded on the
    // defer status-change audit event; NEVER gated or enforced on.
    /// Tier 1 attribution: agent name (env: BR_AGENT_NAME). Recorded only.
    #[arg(long, value_name = "NAME", env = "BR_AGENT_NAME")]
    pub agent_name: Option<String>,

    /// Tier 1 attribution: harness identifier (env: BR_HARNESS). Recorded only.
    #[arg(long, value_name = "HARNESS", env = "BR_HARNESS")]
    pub harness: Option<String>,

    /// Tier 1 attribution: model identifier (env: BR_MODEL). Recorded only.
    #[arg(long, value_name = "MODEL", env = "BR_MODEL")]
    pub model: Option<String>,
}

/// Arguments for the undefer command.
#[derive(Args, Debug, Clone, Default)]
pub struct UndeferArgs {
    /// Issue IDs to undefer
    #[arg(add = ArgValueCompleter::new(open_issue_id_completer))]
    pub ids: Vec<String>,

    /// Machine-readable output (alias for --json)
    #[arg(long)]
    pub robot: bool,

    /// New comment committed atomically with each undefer transition.
    #[arg(long, value_name = "COMMENT", allow_hyphen_values = true)]
    pub transition_comment: Option<String>,

    // Tier 1 attribution (issue #312, Layer 3 — capture-only). Recorded on the
    // undefer status-change audit event; NEVER gated or enforced on.
    /// Tier 1 attribution: agent name (env: BR_AGENT_NAME). Recorded only.
    #[arg(long, value_name = "NAME", env = "BR_AGENT_NAME")]
    pub agent_name: Option<String>,

    /// Tier 1 attribution: harness identifier (env: BR_HARNESS). Recorded only.
    #[arg(long, value_name = "HARNESS", env = "BR_HARNESS")]
    pub harness: Option<String>,

    /// Tier 1 attribution: model identifier (env: BR_MODEL). Recorded only.
    #[arg(long, value_name = "MODEL", env = "BR_MODEL")]
    pub model: Option<String>,
}

/// Arguments for the ready command.
#[derive(Args, Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct ReadyArgs {
    /// Maximum number of issues to return (0 = unlimited; default: unlimited — the full ready set)
    #[arg(long, default_value_t = 0)]
    pub limit: usize,

    /// Filter by assignee (no value = current actor)
    #[arg(
        long,
        num_args = 0..=1,
        default_missing_value = "",
        conflicts_with = "unassigned",
        add = ArgValueCompleter::new(assignee_completer)
    )]
    pub assignee: Option<String>,

    /// Show only unassigned issues
    #[arg(long, conflicts_with = "assignee")]
    pub unassigned: bool,

    /// Filter by label (AND logic, can be repeated)
    #[arg(long, short = 'l', add = ArgValueCompleter::new(label_completer))]
    pub label: Vec<String>,

    /// Filter by label (OR logic, can be repeated)
    #[arg(long, add = ArgValueCompleter::new(label_completer))]
    pub label_any: Vec<String>,

    /// Filter by issue type (can be repeated)
    #[arg(long = "type", short = 't', add = ArgValueCompleter::new(issue_type_completer))]
    pub type_: Vec<String>,

    /// Filter by priority (can be repeated, 0-4)
    #[arg(long, short = 'p', add = ArgValueCompleter::new(priority_completer))]
    pub priority: Vec<String>,

    /// Sort policy: hybrid (default), priority, oldest
    #[arg(long, default_value = "hybrid", value_enum)]
    pub sort: SortPolicy,

    /// Include deferred issues
    #[arg(long)]
    pub include_deferred: bool,

    /// Filter to children of this parent issue ID
    #[arg(long, add = ArgValueCompleter::new(issue_id_completer))]
    pub parent: Option<String>,

    /// Include all descendants (grandchildren, etc.) with --parent
    #[arg(long, short = 'r')]
    pub recursive: bool,

    /// Scope to an epic: ready issues anywhere beneath the given epic/parent
    /// ID (sugar for `--parent <id> --recursive`, depth-unbounded and
    /// cycle-safe). Composes with --label/--type/--priority/--limit.
    #[arg(long, conflicts_with = "parent", add = ArgValueCompleter::new(issue_id_completer))]
    pub epic: Option<String>,

    /// Wrap long lines instead of truncating in text output
    #[arg(long)]
    pub wrap: bool,

    /// Output format (text, json, toon). Env: BR_OUTPUT_FORMAT, TOON_DEFAULT_FORMAT.
    #[arg(long, value_enum)]
    pub format: Option<OutputFormatBasic>,

    /// Show token savings stats when using TOON output
    #[arg(long)]
    pub stats: bool,

    /// Machine-readable output (alias for --json)
    #[arg(long)]
    pub robot: bool,
}

/// Arguments for the scheduler command.
#[derive(Args, Debug, Clone, Default)]
pub struct SchedulerArgs {
    /// Maximum recommendations to return (0 = unlimited; default: unlimited — every scored recommendation)
    #[arg(long, default_value_t = 0)]
    pub limit: usize,

    /// Maximum ready candidates to score before truncating (default: 512, 0 = unlimited)
    #[arg(long, default_value_t = 512)]
    pub candidate_limit: usize,

    /// Non-negative claim age threshold, in hours, for stale-claim evidence
    #[arg(long, default_value_t = 2)]
    pub stale_claim_hours: i64,

    /// Output format (text, json, toon). Env: BR_OUTPUT_FORMAT, TOON_DEFAULT_FORMAT.
    #[arg(long, value_enum)]
    pub format: Option<OutputFormatBasic>,

    /// Show token savings stats when using TOON output
    #[arg(long)]
    pub stats: bool,

    /// Machine-readable output (alias for --json)
    #[arg(long)]
    pub robot: bool,
}

/// Arguments for the blocked command.
#[allow(clippy::struct_excessive_bools)]
#[derive(Args, Debug, Clone, Default)]
pub struct BlockedArgs {
    /// Maximum number of issues to return (default: 50, 0 = unlimited)
    #[arg(long, default_value_t = 50)]
    pub limit: usize,

    /// Include full blocker details in text output
    #[arg(long)]
    pub detailed: bool,

    /// Wrap long lines instead of truncating in text output
    #[arg(long)]
    pub wrap: bool,

    /// Filter by issue type (can be repeated)
    #[arg(long = "type", short = 't', add = ArgValueCompleter::new(issue_type_completer))]
    pub type_: Vec<String>,

    /// Filter by priority (can be repeated, 0-4)
    #[arg(long, short = 'p', add = ArgValueCompleter::new(priority_completer))]
    pub priority: Vec<String>,

    /// Filter by label (AND logic, can be repeated)
    #[arg(long, short = 'l', add = ArgValueCompleter::new(label_completer))]
    pub label: Vec<String>,

    /// Output format (text, json, toon). Env: BR_OUTPUT_FORMAT, TOON_DEFAULT_FORMAT.
    #[arg(long, value_enum)]
    pub format: Option<OutputFormatBasic>,

    /// Show token savings stats when using TOON output
    #[arg(long)]
    pub stats: bool,

    /// Machine-readable output (alias for --json)
    #[arg(long)]
    pub robot: bool,
}

/// Arguments for the close command.
#[derive(Args, Debug, Clone, Default)]
pub struct CloseArgs {
    /// Issue IDs to close (uses last-touched if empty)
    #[arg(add = ArgValueCompleter::new(open_issue_id_completer))]
    pub ids: Vec<String>,

    /// Close reason
    #[arg(long, short = 'r', allow_hyphen_values = true)]
    pub reason: Option<String>,

    /// New comment committed atomically with each close transition. This is
    /// distinct from close metadata in `--reason`.
    #[arg(long, value_name = "COMMENT", allow_hyphen_values = true)]
    pub transition_comment: Option<String>,

    /// Close even if blocked by open dependencies
    #[arg(long, short = 'f')]
    pub force: bool,

    /// After closing, return newly unblocked issues (single ID only)
    #[arg(long)]
    pub suggest_next: bool,

    /// Session ID for tracking
    #[arg(long)]
    pub session: Option<String>,

    /// Machine-readable output (alias for --json)
    #[arg(long)]
    pub robot: bool,

    // Closure-time policy gates (issue #274 — Phase 1).
    //
    // All fields below are inert when the project has no `.beads/policy.yaml`
    // file. Solo-dev workflows see no behavior change; only opt-in repos
    // observe gating or attribution capture.
    //
    /// Tier 1 attribution: agent name (env: BR_AGENT_NAME).
    #[arg(long, value_name = "NAME", env = "BR_AGENT_NAME")]
    pub agent_name: Option<String>,

    /// Tier 1 attribution: harness identifier (env: BR_HARNESS).
    #[arg(long, value_name = "HARNESS", env = "BR_HARNESS")]
    pub harness: Option<String>,

    /// Tier 1 attribution: model identifier (env: BR_MODEL).
    #[arg(long, value_name = "MODEL", env = "BR_MODEL")]
    pub model: Option<String>,

    /// Bypass closure-time policy gates. Requires `--bypass-reason`.
    /// Only honoured when `.beads/policy.yaml` has `allow_bypass: true`
    /// (which is the default).
    #[arg(long)]
    pub bypass_policy: bool,

    /// Reason for bypassing policy gates. Required when `--bypass-policy` is set.
    #[arg(long, value_name = "REASON")]
    pub bypass_reason: Option<String>,
}

/// Arguments for the reopen command.
#[derive(Args, Debug, Clone, Default)]
pub struct ReopenArgs {
    /// Issue IDs to reopen (uses last-touched if empty)
    #[arg(add = ArgValueCompleter::new(closed_issue_id_completer))]
    pub ids: Vec<String>,

    /// Reason for reopening (stored as a comment)
    #[arg(long, short = 'r', allow_hyphen_values = true)]
    pub reason: Option<String>,

    /// Machine-readable output (alias for --json)
    #[arg(long)]
    pub robot: bool,

    // Tier 1 attribution (issue #312, Layer 3 — capture-only). Recorded on the
    // reopen status-change audit event; NEVER gated or enforced on.
    /// Tier 1 attribution: agent name (env: BR_AGENT_NAME). Recorded only.
    #[arg(long, value_name = "NAME", env = "BR_AGENT_NAME")]
    pub agent_name: Option<String>,

    /// Tier 1 attribution: harness identifier (env: BR_HARNESS). Recorded only.
    #[arg(long, value_name = "HARNESS", env = "BR_HARNESS")]
    pub harness: Option<String>,

    /// Tier 1 attribution: model identifier (env: BR_MODEL). Recorded only.
    #[arg(long, value_name = "MODEL", env = "BR_MODEL")]
    pub model: Option<String>,
}

/// Sort policy for ready command.
#[derive(ValueEnum, Debug, Clone, Copy, Default, Eq, PartialEq)]
pub enum SortPolicy {
    /// P0/P1 first by `created_at`, then others by `created_at`
    #[default]
    Hybrid,
    /// Sort by priority ASC, then `created_at` ASC
    Priority,
    /// Sort by `created_at` ASC only
    Oldest,
}

/// Default worker cap for read-only witness planning on high-core swarm hosts.
pub const DEFAULT_WITNESS_PARALLELISM: usize = 64;

/// Arguments for the sync command.
#[derive(Args, Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct SyncArgs {
    /// Export database to JSONL (DB → .beads/issues.jsonl)
    ///
    /// Writes all issues from `SQLite` database to JSONL format.
    #[arg(long, group = "sync_action")]
    pub flush_only: bool,

    /// Import JSONL to database (JSONL → DB)
    ///
    /// Validates JSONL before import. Rejects files with git merge
    /// conflict markers or invalid JSON (cannot be bypassed).
    #[arg(long)]
    pub import_only: bool,

    /// Perform a 3-way merge (Base + Local DB + Remote JSONL)
    ///
    /// Reconciles changes when both the database and JSONL have been modified.
    /// Uses `.beads/beads.base.jsonl` as the common ancestor.
    #[arg(long)]
    pub merge: bool,

    /// Additively reconcile JSONL into the database (JSONL → DB, lossless)
    ///
    /// Classifies every JSONL row against full issue state instead of the
    /// cached content hash, then applies creates and timestamp-newer updates
    /// in place. Never resets tables, never deletes issues, never writes
    /// events or JSONL, and preserves all audit history. Combine with
    /// --dry-run to preview the plan without any mutation.
    #[arg(long)]
    pub reconcile: bool,

    /// Preview the reconcile plan without mutating anything
    ///
    /// Only valid with --reconcile. Opens no write transaction and performs
    /// no metadata, cache, dirty-marker, JSONL, or base-snapshot writes.
    #[arg(long, requires = "reconcile")]
    pub dry_run: bool,

    /// Show sync status (read-only)
    ///
    /// Displays hash comparison and freshness info without modifications.
    /// With --json the payload also carries `workspace_health` plus a
    /// `reliability_audit` anomaly record (same write-gate vocabulary as
    /// `br doctor --json`). Its compatibility `git_export` block is always
    /// `{available:false, reason:"not_probed"}` and points to the explicit
    /// `br vcs-status --json` diagnostic; sync never probes VCS.
    #[arg(long)]
    pub status: bool,

    /// Emit deterministic JSONL chunk witness (read-only)
    ///
    /// Reads the resolved issues.jsonl bytes and emits chunk/root hashes
    /// without opening or mutating the SQLite database.
    #[arg(long)]
    pub witness: bool,

    /// Plan a lossless additive JSONL-to-database reconciliation
    ///
    /// This mode is read-only by default. It compares exact issue IDs, keeps
    /// every database-only row and audit event, and emits a hash-bound receipt.
    /// It never performs content-hash identity merges, physical deletes, base
    /// snapshot writes, or JSONL writes.
    #[arg(long = "reconcile-additive")]
    pub reconcile_additive: bool,

    /// Apply a conflict-free additive reconciliation plan transactionally
    ///
    /// Without this flag, --reconcile-additive only prints the dry-run plan.
    #[arg(long, requires = "reconcile_additive")]
    pub apply: bool,

    /// SHA-256 from the reviewed additive-reconciliation dry-run receipt
    ///
    /// Required with --apply. The command refuses mutation if the exact source,
    /// database, resolution set, or expected post-state now produces another
    /// plan token.
    #[arg(long = "expect-plan-sha256", value_name = "SHA256", requires = "apply")]
    pub expect_plan_sha256: Option<String>,

    /// Resolve one reviewed shared scalar-row conflict in favor of JSONL
    ///
    /// Repeat for multiple exact issue IDs. Relations are never replaced by
    /// this resolution; they must already be semantically identical.
    #[arg(
        long = "resolve-source-id",
        value_name = "ISSUE_ID",
        requires = "reconcile_additive"
    )]
    pub resolve_source_ids: Vec<String>,

    /// Lines per JSONL witness chunk
    ///
    /// Only used with --witness. Larger chunks reduce witness size; smaller
    /// chunks improve unchanged-chunk localization for parallel sync planning.
    #[arg(
        long = "witness-chunk-lines",
        default_value_t = 1024,
        value_name = "LINES",
        requires = "witness"
    )]
    pub witness_chunk_lines: usize,

    /// Parallel worker cap for read-only JSONL witness hashing and work planning
    ///
    /// Only used with --witness. When omitted, br uses a deterministic
    /// 64-worker cap rather than host-dependent CPU detection so robot output
    /// remains stable across machines.
    #[arg(
        long = "witness-parallelism",
        value_name = "WORKERS",
        requires = "witness"
    )]
    pub witness_parallelism: Option<usize>,

    /// Parallel worker cap for JSONL export line preparation
    ///
    /// Used by --flush-only and merge export writeback. When omitted, br uses
    /// up to 64 workers capped by host parallelism. Use 1 for the serial
    /// fallback.
    #[arg(long = "export-parallelism", value_name = "WORKERS")]
    pub export_parallelism: Option<usize>,

    /// Override safety guards (use with caution!)
    ///
    /// Bypasses Empty DB Guard and Stale DB Guard for export.
    /// Does NOT bypass conflict marker detection or JSON validation.
    #[arg(long, short = 'f')]
    pub force: bool,

    /// Resolve sync --merge conflicts by keeping the local SQLite database version.
    #[arg(long, requires = "merge", conflicts_with_all = ["force", "force_jsonl"])]
    pub force_db: bool,

    /// Resolve sync --merge conflicts by keeping the JSONL file version.
    #[arg(long, requires = "merge", conflicts_with_all = ["force", "force_db"])]
    pub force_jsonl: bool,

    /// Allow using a JSONL path outside the .beads directory.
    ///
    /// This flag enables paths set via `BEADS_JSONL` environment variable.
    /// Paths inside .git/ are always rejected regardless of this flag.
    #[arg(long)]
    pub allow_external_jsonl: bool,

    /// Write manifest file with export summary
    #[arg(long)]
    pub manifest: bool,

    /// Export error policy: strict (default), best-effort, partial, required-core
    ///
    /// Controls how export handles serialization errors for individual issues.
    #[arg(long = "error-policy", add = ArgValueCompleter::new(export_error_policy_completer))]
    pub error_policy: Option<String>,

    /// Orphan handling mode: strict (default), resurrect, skip, allow
    ///
    /// Controls how import handles orphaned dependencies (refs to deleted issues).
    #[arg(long, add = ArgValueCompleter::new(orphan_mode_completer))]
    pub orphans: Option<String>,

    /// Rename issues with wrong prefix to expected prefix during import
    ///
    /// Replaces only the prefix segment and keeps the id remainder intact
    /// (`oldp-cargo-license-spdx-ay8` -> `newp-cargo-license-spdx-ay8`); a
    /// doubled prefix collapses once (`oldp-oldp-x-3un` -> `newp-x-3un`).
    /// If the preserved id would collide with an existing id, that issue
    /// falls back to a freshly generated id. Every rewrite is reported as a
    /// `prefix_renames` old_id -> new_id mapping in the sync output (with a
    /// `fallback` marker for regenerated ids), and each issue's old id is
    /// stashed in its external_ref when that field is empty.
    ///
    /// Notes: without --force the import (and therefore the rename) is
    /// skipped when the JSONL content hash is unchanged since the last
    /// import, and a following `br sync --flush-only` needs `--force` to
    /// write the renamed ids back to the JSONL.
    #[arg(long)]
    pub rename_prefix: bool,

    /// Rebuild the database from JSONL (removes orphaned DB entries)
    ///
    /// After importing, deletes any issues in the database that are not
    /// present in the JSONL file. This ensures the DB exactly matches
    /// the JSONL source of truth.
    #[arg(long)]
    pub rebuild: bool,

    /// Machine-readable output (alias for --json)
    #[arg(long)]
    pub robot: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub enum ConfigCommands {
    /// List all available config options
    List {
        /// Show only project config
        #[arg(long, conflicts_with = "user")]
        project: bool,

        /// Show only user config
        #[arg(long, conflicts_with = "project")]
        user: bool,
    },

    /// Get a specific config value
    Get {
        /// Config key
        #[arg(add = ArgValueCompleter::new(config_key_completer))]
        key: String,
    },

    /// Set a config value
    Set {
        /// Config key=value pair (or key value)
        #[arg(
            num_args = 1..=2,
            value_name = "KV",
            add = ArgValueCompleter::new(config_key_assignment_completer)
        )]
        args: Vec<String>,
    },

    /// Delete a config value
    #[command(visible_alias = "unset")]
    Delete {
        /// Config key
        #[arg(add = ArgValueCompleter::new(config_key_completer))]
        key: String,
    },

    /// Open user config file in $EDITOR
    Edit,

    /// Show config file paths
    Path,
}

/// Arguments for the stats command.
#[derive(Args, Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct StatsArgs {
    /// Show breakdown by issue type
    #[arg(long)]
    pub by_type: bool,

    /// Show breakdown by priority
    #[arg(long)]
    pub by_priority: bool,

    /// Show breakdown by assignee
    #[arg(long)]
    pub by_assignee: bool,

    /// Show breakdown by label
    #[arg(long)]
    pub by_label: bool,

    /// Include recent activity stats explicitly (default unless `--no-activity`)
    #[arg(long)]
    pub activity: bool,

    /// Skip recent activity stats (for performance)
    #[arg(long)]
    pub no_activity: bool,

    /// Activity window in hours (default: 24)
    #[arg(long, default_value_t = 24)]
    pub activity_hours: u32,

    /// Output format (text, json, toon). Env: BR_OUTPUT_FORMAT, TOON_DEFAULT_FORMAT.
    #[arg(long, value_enum)]
    pub format: Option<OutputFormatBasic>,

    /// Show token savings stats when using TOON output
    #[arg(long)]
    pub stats: bool,

    /// Machine-readable output (alias for --json)
    #[arg(long)]
    pub robot: bool,
}

#[derive(Args, Debug)]
pub struct HistoryArgs {
    #[command(subcommand)]
    pub command: Option<HistoryCommands>,
}

#[derive(Subcommand, Debug)]
pub enum HistoryCommands {
    /// List history backups
    List,
    /// Diff backup against current JSONL
    Diff {
        /// Backup filename (e.g. issues.2025-01-01T12-00-00.jsonl)
        file: String,
    },
    /// Restore from backup
    Restore {
        /// Backup filename
        file: String,
        /// Force overwrite
        #[arg(long, short = 'f')]
        force: bool,
    },
    /// Prune old backups
    Prune {
        /// Number of backups to keep (default: 100)
        #[arg(long, default_value_t = 100)]
        keep: usize,
        /// Remove backups older than N days
        #[arg(long)]
        older_than: Option<u32>,
    },
}

/// Arguments for the version command.
#[derive(Args, Debug, Clone, Default)]
pub struct VersionArgs {
    /// Check if a newer version is available (exit 0=up-to-date, 1=update-available)
    #[arg(long, short = 'c')]
    pub check: bool,

    /// Output only the version number (for scripts)
    #[arg(long, short = 's')]
    pub short: bool,
}

/// Arguments for the doctor command.
///
/// `br doctor` is dual-shaped: when no `subcommand` is supplied, the
/// flat command runs the legacy diagnostic+repair flow honoring
/// `--repair`, `--dry-run`, `--allow-repeated-repair`, and `--robot-triage`.
/// When a `subcommand` is supplied, dispatch is routed to the WP6
/// agent-ergonomics surface (`capabilities`, `robot-docs`, `health`,
/// `ls`, `undo`, `explain`).
#[derive(Args, Debug, Clone, Default)]
pub struct DoctorArgs {
    /// Attempt to repair detected issues (rebuilds DB from JSONL).
    /// `--fix` is a visible alias used by doctor repair specs and
    /// fixture skeletons.
    #[arg(long, visible_alias = "fix")]
    pub repair: bool,

    /// REINDEX-only recovery for the partial-index stale-entry class
    /// (see beads_rust#288). Strictly narrower than `--repair`: walks
    /// every user-defined index, runs `REINDEX "<name>"`
    /// inside a single transaction with a verbatim pre-snapshot
    /// backup, and never touches issue rows. Use when
    /// `PRAGMA integrity_check` returns `ok` but `br doctor` reports
    /// `index <name> contains rowid N for a table row that does not
    /// satisfy the partial index predicate` — that's older SQLite
    /// not validating partial predicates on `integrity_check`.
    /// Mutually exclusive with `--repair`.
    #[arg(long, conflicts_with = "repair")]
    pub repair_indexes: bool,

    /// Allow another JSONL rebuild after prior failed recovery evidence
    #[arg(long)]
    pub allow_repeated_repair: bool,

    /// With `--repair`, print the planned mutations to stderr without
    /// writing anything. Without `--repair` this is a no-op (doctor is
    /// already read-only). Wired through the WP1 `mutate()` chokepoint
    /// in `cli::commands::doctor_subsystems::mutate`.
    #[arg(long)]
    pub dry_run: bool,

    /// Emit the `br.doctor.triage.v1` mega-envelope (summary + findings +
    /// planned actions + recommended command) and exit. Read-only; ignores
    /// `--repair`. Designed for AI agents that want every triage signal
    /// in a single JSON read.
    #[arg(long = "robot-triage")]
    pub robot_triage: bool,

    /// Fast path for pre-commit / CI: skip the slow detectors
    /// (`db.recoverable_anomalies`, `counts.db_vs_jsonl`,
    /// `sync.metadata`, `sqlite3.integrity_check`, `db.write_probe`) and
    /// run only the cheap ones. Returns exit 0 if no findings, 1 if
    /// findings present. Target latency: <1s on a healthy workspace.
    /// Always read-only; ignored under `--repair`.
    #[arg(long)]
    pub quick: bool,

    /// Pass-5 cycle 1: with `--repair`, only run fixers whose FM
    /// identifier matches one of the supplied values. Accepts
    /// comma-separated lists and repeated `--only` flags. Empty list
    /// means "run all fixers" (existing behavior). FM identifiers are
    /// the `fm-<subsystem>-<slug>` form; the authoritative vocabulary
    /// is the capabilities envelope's `fixers[].filter_ids` (each
    /// fixer's gate ids — beware: an `--only` list disables every
    /// fixer whose ids it omits, including the rebuild paths). All
    /// repair paths respect the filter, including the legacy
    /// `repair_*` ones (each gated on the ids its `filter_ids` row
    /// advertises).
    #[arg(long, value_delimiter = ',', num_args = 1..)]
    pub only: Vec<String>,

    /// Pass-5 cycle 1: with `--repair`, skip the listed fixers even when
    /// their finding would otherwise fire. Accepts comma-separated
    /// lists and repeated `--skip` flags. Useful when an operator
    /// wants the doctor to run everything except one known-flaky
    /// path. Applied after `--only` filtering (so `--only A --skip A`
    /// effectively disables A). Ids come from the capabilities
    /// envelope's `fixers[].filter_ids`.
    #[arg(long, value_delimiter = ',', num_args = 1..)]
    pub skip: Vec<String>,

    /// Pass-10 cycle 62: opt-in to fixers that are normally detect-only
    /// because the fix is expensive, operator-timed, or has subtle
    /// trade-offs the doctor's default safety posture refuses to make
    /// autonomously. Currently enables:
    ///
    /// - VACUUM on `fm-caches_indexes-db-bloat-vs-jsonl` — compacts the
    ///   SQLite database in place to reclaim freelist space. Bloat is
    ///   normally detect-only because VACUUM rewrites every page (slow
    ///   on large DBs) and operators typically schedule it deliberately.
    ///
    /// NEVER pass this in CI without inspecting the planned actions
    /// first (use `--dry-run` together with this flag). The opt-in is
    /// load-bearing: the same fixers without this flag are intentionally
    /// detect-only to preserve the doctor's "doctor mutates only when
    /// the operator has consented" SACRED INVARIANT.
    #[arg(long = "unsafe-auto-fix")]
    pub unsafe_auto_fix: bool,

    /// Optional WP6 subcommand. When `None`, the flat doctor handler
    /// (above) runs as it always has.
    #[command(subcommand)]
    pub subcommand: Option<DoctorSubcommand>,
}

/// WP6 agent-ergonomics surface — pure additions, none of these
/// duplicate or shadow the flat-command flags.
#[derive(Subcommand, Debug, Clone)]
pub enum DoctorSubcommand {
    /// Print the machine-readable `br.doctor.capabilities.v1` envelope
    /// (exit codes, write scopes, env vars, fixers, detectors).
    Capabilities(DoctorCapabilitiesArgs),
    /// Print the paste-ready agent handbook for `br doctor`.
    #[command(name = "robot-docs", alias = "robot_docs")]
    RobotDocs(DoctorRobotDocsArgs),
    /// Cheap (<200 ms) one-line liveness summary; exit-code = liveness.
    Health(DoctorHealthArgs),
    /// List runs in `.doctor/runs/` with run-id, start time, exit code,
    /// and action count.
    Ls(DoctorLsArgs),
    /// Restore from `.doctor/runs/<run-id>/backups/` (or `latest`).
    Undo(DoctorUndoArgs),
    /// Plan, apply, or undo an explicitly reviewed database schema migration.
    #[command(name = "migrate-schema")]
    MigrateSchema(DoctorMigrateSchemaArgs),
    /// Expand a single finding (stub in WP6; full evidence later).
    Explain(DoctorExplainArgs),
}

/// Arguments for `br doctor capabilities`.
#[derive(Args, Debug, Clone, Default)]
pub struct DoctorCapabilitiesArgs {
    /// Output format: `json` (default for machine readers) or `text`.
    #[arg(long, value_enum, default_value_t = OutputFormatBasic::Text)]
    pub format: OutputFormatBasic,

    /// Optional fixer/detector id filter — reserved for future
    /// extension; currently a passthrough. Stable surface so agents can
    /// pin invocations.
    #[arg(long)]
    pub command: Option<String>,
}

/// Arguments for `br doctor robot-docs`.
#[derive(Args, Debug, Clone, Default)]
pub struct DoctorRobotDocsArgs {
    /// Output format: `text` (Markdown) is default; `json` wraps the
    /// handbook in an envelope for token-budgeted agents.
    #[arg(long, value_enum, default_value_t = OutputFormatBasic::Text)]
    pub format: OutputFormatBasic,
}

/// Arguments for `br doctor health`.
#[derive(Args, Debug, Clone, Default)]
pub struct DoctorHealthArgs {
    /// Emit JSON instead of the one-line text summary.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `br doctor ls`.
#[derive(Args, Debug, Clone, Default)]
pub struct DoctorLsArgs {
    /// Emit a JSON array (`br.doctor.runs_list.v1`) instead of a text
    /// table.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `br doctor undo`.
#[derive(Args, Debug, Clone)]
pub struct DoctorUndoArgs {
    /// Run identifier to restore. Use the literal `latest` to resolve to
    /// the most recent run by ISO-8601 timestamp.
    pub run_id: String,

    /// Print the restore plan; do not touch disk.
    #[arg(long)]
    pub dry_run: bool,

    /// Emit a JSON envelope describing the restore.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `br doctor migrate-schema`.
#[derive(Args, Debug, Clone)]
pub struct DoctorMigrateSchemaArgs {
    /// Reviewed migration operation.
    #[command(subcommand)]
    pub command: DoctorMigrateSchemaCommand,
}

/// Explicit schema-migration lifecycle.
#[derive(Subcommand, Debug, Clone)]
pub enum DoctorMigrateSchemaCommand {
    /// Inspect the live database and emit a token bound to its exact file-family state.
    Plan(DoctorMigrateSchemaPlanArgs),
    /// Apply the reviewed migration only when the live state still matches a plan token.
    Apply(DoctorMigrateSchemaApplyArgs),
    /// Restore the exact pre-migration database family from a completed run.
    Undo(DoctorMigrateSchemaUndoArgs),
}

/// Arguments for `br doctor migrate-schema plan`.
#[derive(Args, Debug, Clone, Default)]
pub struct DoctorMigrateSchemaPlanArgs {
    /// Emit the machine-readable receipt.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `br doctor migrate-schema apply`.
#[derive(Args, Debug, Clone)]
pub struct DoctorMigrateSchemaApplyArgs {
    /// Exact token emitted by `migrate-schema plan`.
    #[arg(long)]
    pub plan_token: String,

    /// Emit the machine-readable completion receipt.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `br doctor migrate-schema undo`.
#[derive(Args, Debug, Clone)]
pub struct DoctorMigrateSchemaUndoArgs {
    /// Migration run identifier emitted by `migrate-schema apply`.
    pub run_id: String,

    /// Verify and print the restore plan without touching the database family.
    #[arg(long)]
    pub dry_run: bool,

    /// Emit the machine-readable restore receipt.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for `br doctor explain`.
#[derive(Args, Debug, Clone)]
pub struct DoctorExplainArgs {
    /// Finding id (e.g. `fm-jsonl-tombstone-drift`).
    pub finding_id: String,

    /// Emit JSON instead of text.
    #[arg(long)]
    pub json: bool,
}

/// Arguments for the upgrade command.
#[cfg(feature = "self_update")]
#[derive(Args, Debug, Clone, Default)]
pub struct UpgradeArgs {
    /// Check only, don't install
    #[arg(long)]
    pub check: bool,

    /// Force reinstall current version
    #[arg(long)]
    pub force: bool,

    /// Install specific version (e.g., "0.2.0")
    #[arg(long)]
    pub version: Option<String>,

    /// Show what would happen without making changes
    #[arg(long)]
    pub dry_run: bool,
}

/// Arguments for the orphans command.
#[derive(Args, Debug, Clone, Default)]
pub struct OrphansArgs {
    /// Show detailed commit info
    #[arg(long)]
    pub details: bool,

    /// Prompt to fix orphans
    #[arg(long)]
    pub fix: bool,

    /// Machine-readable output (alias for --json)
    #[arg(long)]
    pub robot: bool,
}

/// Arguments for the changelog command.
#[derive(Args, Debug, Clone, Default)]
pub struct ChangelogArgs {
    /// Start date (RFC3339, YYYY-MM-DD, or relative like +7d)
    #[arg(long)]
    pub since: Option<String>,

    /// Start from git tag date
    #[arg(long, conflicts_with = "since")]
    pub since_tag: Option<String>,

    /// Start from git commit date
    #[arg(long, conflicts_with_all = ["since", "since_tag"])]
    pub since_commit: Option<String>,

    /// Machine-readable output (alias for --json)
    #[arg(long)]
    pub robot: bool,
}

/// Subcommands for the query command.
#[derive(Subcommand, Debug)]
pub enum QueryCommands {
    /// Save current filter set as a named query
    Save(QuerySaveArgs),
    /// Run a saved query
    Run(QueryRunArgs),
    /// List all saved queries
    List,
    /// Delete a saved query
    Delete(QueryDeleteArgs),
}

/// Arguments for the query save command.
#[derive(Args, Debug, Clone)]
pub struct QuerySaveArgs {
    /// Name for the saved query
    pub name: String,

    /// Optional description
    #[arg(long, short = 'd')]
    pub description: Option<String>,

    /// Filters to save (same as list command filters)
    #[command(flatten)]
    pub filters: ListArgs,
}

/// Arguments for the query run command.
#[derive(Args, Debug, Clone)]
pub struct QueryRunArgs {
    /// Name of the saved query to run
    #[arg(add = ArgValueCompleter::new(saved_query_completer))]
    pub name: String,

    /// Additional filters to merge with saved query (CLI overrides saved)
    #[command(flatten)]
    pub filters: ListArgs,
}

/// Arguments for the query delete command.
#[derive(Args, Debug, Clone)]
pub struct QueryDeleteArgs {
    /// Name of the saved query to delete
    #[arg(add = ArgValueCompleter::new(saved_query_completer))]
    pub name: String,
}

/// Arguments for the graph command.
///
/// The traversal follows *dependents* by default — issues blocked by the root —
/// so `br graph <id>` answers "what does closing this unblock?". An issue that
/// is itself blocked and blocks nothing therefore reports no dependents. This
/// is a deliberate divergence from classic `bd`, whose `graph` walks
/// dependencies (`beads_rust-mf72`).
///
/// `--dependencies` walks the other way — "what is blocking this?" — so one
/// command covers both directions. `br dep tree <id>` remains the
/// dependency-shaped view for a single issue.
#[derive(Args, Debug, Clone, Default)]
pub struct GraphArgs {
    /// Issue ID (root of the dependents graph). Required unless --all is specified.
    #[arg(add = ArgValueCompleter::new(open_issue_id_completer))]
    pub issue: Option<String>,

    /// Show graph for all `open`/`in_progress`/`blocked` issues (connected components)
    #[arg(long)]
    pub all: bool,

    /// Walk dependencies instead of dependents: what is blocking this issue
    #[arg(long, conflicts_with = "all")]
    pub dependencies: bool,

    /// One line per issue (compact output)
    #[arg(long)]
    pub compact: bool,

    /// Emit Graphviz DOT notation (pipe to `dot -Tsvg`); overrides text/JSON rendering
    #[arg(long)]
    pub dot: bool,
}

/// Arguments for the agents command.
#[derive(Args, Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct AgentsArgs {
    /// Add beads workflow instructions to AGENTS.md
    #[arg(long, conflicts_with_all = ["remove", "update", "check"])]
    pub add: bool,

    /// Remove beads workflow instructions from AGENTS.md
    #[arg(long, conflicts_with_all = ["add", "update", "check"])]
    pub remove: bool,

    /// Update beads workflow instructions to latest version
    #[arg(long, conflicts_with_all = ["add", "remove", "check"])]
    pub update: bool,

    /// Check status only (default behavior)
    #[arg(long, conflicts_with_all = ["add", "remove", "update"])]
    pub check: bool,

    /// Preview changes without modifying files
    #[arg(long)]
    pub dry_run: bool,

    /// Skip confirmation prompts
    #[arg(long, short = 'f')]
    pub force: bool,
}

#[cfg(test)]
mod tests {
    use super::{
        Cli, Commands, DoctorMigrateSchemaArgs, DoctorMigrateSchemaCommand,
        DoctorMigrateSchemaPlanArgs, DoctorMigrateSchemaUndoArgs, DoctorSubcommand,
        InheritedOutputMode, OutputFormat, OutputFormatBasic, issue_type_completer,
        issue_type_completer_delimited, resolve_output_format_basic_with_outer_mode,
        resolve_output_format_with_outer_mode,
    };
    use crate::storage::sqlite::SqliteStorage;
    use clap::{CommandFactory, Parser};
    use clap_complete::engine::CompletionCandidate;
    use std::ffi::OsStr;
    use tempfile::TempDir;

    const CLI_REFERENCE: &str = include_str!("../../docs/CLI_REFERENCE.md");

    #[test]
    fn test_list_limit_is_none_when_omitted() {
        let cli = Cli::parse_from(["br", "list"]);
        assert!(
            matches!(&cli.command, Commands::List(_)),
            "expected list command"
        );
        let Commands::List(args) = cli.command else {
            return;
        };
        assert_eq!(args.limit, None);
    }

    #[test]
    fn test_list_limit_zero_parses_as_unlimited() {
        let cli = Cli::parse_from(["br", "list", "--limit", "0"]);
        assert!(
            matches!(&cli.command, Commands::List(_)),
            "expected list command"
        );
        let Commands::List(args) = cli.command else {
            return;
        };
        assert_eq!(args.limit, Some(0));
    }

    #[test]
    fn test_ready_assignee_flag_accepts_missing_value() {
        let cli = Cli::parse_from(["br", "ready", "--assignee"]);
        assert!(
            matches!(&cli.command, Commands::Ready(_)),
            "expected ready command"
        );
        let Commands::Ready(args) = cli.command else {
            return;
        };
        assert_eq!(args.assignee.as_deref(), Some(""));
    }

    #[test]
    fn test_ready_assignee_conflicts_with_unassigned() {
        let err = Cli::try_parse_from(["br", "ready", "--assignee", "alice", "--unassigned"])
            .expect_err("ready filters should conflict");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn test_doctor_fix_alias_parses_as_repair() {
        let cli = Cli::parse_from([
            "br",
            "doctor",
            "--fix",
            "--only",
            "fm-state_files-merge-artifact-stuck",
        ]);
        let Commands::Doctor(args) = cli.command else {
            panic!("expected doctor command");
        };

        assert!(args.repair);
        assert_eq!(args.only, vec!["fm-state_files-merge-artifact-stuck"]);
    }

    #[test]
    fn test_doctor_fix_alias_conflicts_with_repair_indexes() {
        let err = Cli::try_parse_from(["br", "doctor", "--fix", "--repair-indexes"])
            .expect_err("--fix must share --repair's repair-index conflict");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn test_doctor_migrate_schema_lifecycle_parses() {
        let plan = Cli::parse_from(["br", "doctor", "migrate-schema", "plan", "--json"]);
        let Commands::Doctor(plan_args) = plan.command else {
            panic!("expected doctor command");
        };
        assert!(matches!(
            plan_args.subcommand,
            Some(DoctorSubcommand::MigrateSchema(DoctorMigrateSchemaArgs {
                command: DoctorMigrateSchemaCommand::Plan(DoctorMigrateSchemaPlanArgs {
                    json: true
                })
            }))
        ));

        let apply = Cli::parse_from([
            "br",
            "doctor",
            "migrate-schema",
            "apply",
            "--plan-token",
            "receipt-token",
        ]);
        let Commands::Doctor(apply_args) = apply.command else {
            panic!("expected doctor command");
        };
        let Some(DoctorSubcommand::MigrateSchema(DoctorMigrateSchemaArgs {
            command: DoctorMigrateSchemaCommand::Apply(apply),
        })) = apply_args.subcommand
        else {
            panic!("expected migrate-schema apply");
        };
        assert_eq!(apply.plan_token, "receipt-token");
        assert!(!apply.json);

        let undo = Cli::parse_from([
            "br",
            "doctor",
            "migrate-schema",
            "undo",
            "run-id",
            "--dry-run",
        ]);
        let Commands::Doctor(undo_args) = undo.command else {
            panic!("expected doctor command");
        };
        assert!(matches!(
            undo_args.subcommand,
            Some(DoctorSubcommand::MigrateSchema(DoctorMigrateSchemaArgs {
                command: DoctorMigrateSchemaCommand::Undo(DoctorMigrateSchemaUndoArgs {
                    ref run_id,
                    dry_run: true,
                    json: false,
                })
            })) if run_id == "run-id"
        ));
    }

    #[test]
    fn test_create_positional_title_conflicts_with_title_flag() {
        let err =
            Cli::try_parse_from(["br", "create", "positional title", "--title", "flag title"])
                .expect_err("create should reject ambiguous title sources");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn test_issue_type_delimited_completion_preserves_plain_candidate_order() {
        let plain = candidate_values(issue_type_completer(OsStr::new("bu")));
        let delimited = candidate_values(issue_type_completer_delimited(OsStr::new("task, bu")));
        let expected = plain
            .iter()
            .map(|value| format!("task, {value}"))
            .collect::<Vec<_>>();

        assert_eq!(plain.first().map(String::as_str), Some("bug"));
        assert_eq!(delimited, expected);
    }

    #[test]
    fn test_agents_add_conflicts_with_check() {
        let err = Cli::try_parse_from(["br", "agents", "--add", "--check"])
            .expect_err("agents actions should conflict");
        assert_eq!(err.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn test_saved_queries_from_db_reads_saved_query_names_without_config_scan() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).expect("open db");
        storage
            .set_config("saved_query:mine", r#"{"name":"mine"}"#)
            .expect("save query");
        storage
            .set_config("saved_query:stale", r#"{"name":"stale"}"#)
            .expect("save query");
        storage
            .set_config("ui.theme", "amber")
            .expect("save regular config");

        let saved_queries = super::saved_queries_from_db(&db_path);
        assert_eq!(
            saved_queries.into_iter().collect::<Vec<_>>(),
            vec!["mine".to_string(), "stale".to_string()]
        );
    }

    #[test]
    fn test_resolve_output_format_with_outer_mode_inherits_toon() {
        let resolved =
            resolve_output_format_with_outer_mode(None, InheritedOutputMode::Toon, false);
        assert_eq!(resolved, OutputFormat::Toon);
    }

    #[test]
    fn test_resolve_output_format_with_outer_mode_keeps_quiet_over_env_defaults() {
        let resolved =
            resolve_output_format_with_outer_mode(None, InheritedOutputMode::Quiet, false);
        assert_eq!(resolved, OutputFormat::Text);
    }

    #[test]
    fn test_resolve_output_format_basic_with_outer_mode_honors_explicit_format() {
        let resolved = resolve_output_format_basic_with_outer_mode(
            Some(OutputFormatBasic::Json),
            InheritedOutputMode::Toon,
            false,
        );
        assert_eq!(resolved, OutputFormat::Json);
    }

    #[test]
    fn test_cli_reference_documents_current_clap_surface() {
        assert_all_top_level_commands_are_documented();
        assert_doc_contains_all(CLAP_DRIFT_SENTINELS);
    }

    const CLAP_DRIFT_SENTINELS: &[&str] = &[
        "--lock-timeout <LOCK_TIMEOUT>",
        "`--from-file <PATH>` | Read IDs from file",
        "`--cascade` | Delete dependents recursively",
        "`--force` | Bypass dependent checks, orphaning dependents",
        "`--hard` | Prune tombstones from JSONL immediately",
        "br config <COMMAND>",
        "`set <KEY=VALUE>` or `set <KEY> <VALUE>`",
        "`delete <KEY>` | Delete a config value; `unset` is an alias",
        "`save <NAME> [FILTERS...]`",
        "no free-form query string argument",
        "`--allow-external-jsonl` | Allow JSONL path outside `.beads/`",
        "`--rename-prefix` | During import, rewrite mismatched issue IDs",
        "`--rebuild` | During import, rebuild SQLite from JSONL",
        "`--notes-contains <TEXT>` | Notes contains substring",
        "`--format <FMT>` | Output format: text, json, csv, toon",
        "`--days <N>` | Issues not updated in N days (default: 30)",
        "`--reservations <PATH>` | Offline Agent Mail reservation snapshot",
        "`--agents <PATH>` | Offline Agent Mail agent snapshot",
        "br coordination status --reservations reservations.json --agents agents.jsonl --json",
        "beads://coordination/status",
        "`issue-with-counts`, `issue-details`",
    ];

    fn assert_all_top_level_commands_are_documented() {
        let command = Cli::command();
        let missing = command
            .get_subcommands()
            .map(clap::Command::get_name)
            .filter(|name| !is_generated_help_command(name))
            .filter(|name| !top_level_command_is_documented(name))
            .collect::<Vec<_>>();

        assert!(
            missing.is_empty(),
            "docs/CLI_REFERENCE.md is missing top-level command headings: {missing:?}"
        );
    }

    fn is_generated_help_command(name: &str) -> bool {
        name == "help"
    }

    fn top_level_command_is_documented(name: &str) -> bool {
        if name == "status" {
            return CLI_REFERENCE.contains("### stats / status");
        }
        if name == "undefer" {
            return CLI_REFERENCE.contains("### defer / undefer");
        }
        CLI_REFERENCE.contains(&format!("### {name}"))
    }

    fn assert_doc_contains_all(needles: &[&str]) {
        for needle in needles {
            assert!(
                CLI_REFERENCE.contains(needle),
                "docs/CLI_REFERENCE.md is missing Clap drift sentinel: {needle}"
            );
        }
    }

    fn candidate_values(candidates: Vec<CompletionCandidate>) -> Vec<String> {
        candidates
            .into_iter()
            .map(|candidate| candidate.get_value().to_string_lossy().into_owned())
            .collect()
    }
}
