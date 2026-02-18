//! CLI definitions and entry point.

use clap::builder::StyledStr;
use clap::{Args, Parser, Subcommand, ValueEnum};
use clap_complete::engine::{ArgValueCompleter, CompletionCandidate};
use serde::Deserialize;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::sync::OnceLock;

use crate::config;
use crate::format::truncate_title;
use crate::model::{IssueType, Status};

pub mod commands;

#[derive(Clone, Copy)]
enum IssueCompletionFilter {
    Any,
    Open,
    Closed,
}

impl IssueCompletionFilter {
    fn matches(self, status: &Status) -> bool {
        match self {
            Self::Any => true,
            Self::Open => !status.is_terminal(),
            Self::Closed => status.is_terminal(),
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

fn build_completion_index() -> CompletionIndex {
    let Ok(beads_dir) = config::discover_beads_dir(None) else {
        return CompletionIndex::default();
    };
    let Ok(paths) = config::resolve_paths(&beads_dir, None) else {
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

    if let Ok(beads_dir) = config::discover_beads_dir(None) {
        if let Ok(project) = config::load_project_config(&beads_dir) {
            add_layer_keys(&mut keys, &project);
        }
        if let Ok(storage_ctx) =
            config::open_storage_with_cli(&beads_dir, &config::CliOverrides::default())
        {
            if let Ok(db_layer) = config::ConfigLayer::from_db(&storage_ctx.storage) {
                add_layer_keys(&mut keys, &db_layer);
            }
            if let Ok(map) = storage_ctx.storage.get_all_config() {
                for key in map.keys() {
                    if let Some(name) = key.strip_prefix(SAVED_QUERY_PREFIX) {
                        if !name.trim().is_empty() {
                            saved_queries.insert(name.to_string());
                        }
                    }
                }
            }
        }
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
            let help = format!("{} | {}", issue.status.as_str(), title);
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

fn issue_type_completer(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(prefix) = current.to_str() else {
        return Vec::new();
    };

    let mut candidates = static_candidates(prefix, ISSUE_TYPE_CANDIDATES);
    for value in &completion_index().types {
        if issue_type_is_standard(value) {
            continue;
        }
        if matches_prefix_case_insensitive(value, prefix) {
            candidates.push(CompletionCandidate::new(value));
        }
    }
    candidates
}

fn issue_type_completer_delimited(current: &OsStr) -> Vec<CompletionCandidate> {
    let Some(current) = current.to_str() else {
        return Vec::new();
    };
    let (prefix, needle) = split_delimited_prefix(current, ',');
    let mut candidates = static_candidates(needle, ISSUE_TYPE_CANDIDATES)
        .into_iter()
        .map(|candidate| candidate.add_prefix(prefix.clone()))
        .collect::<Vec<_>>();
    for value in &completion_index().types {
        if issue_type_is_standard(value) {
            continue;
        }
        if matches_prefix_case_insensitive(value, needle) {
            candidates.push(CompletionCandidate::new(value).add_prefix(prefix.clone()));
        }
    }
    candidates
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

    /// `SQLite` busy timeout in ms
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

    /// Create a new issue
    Create(CreateArgs),

    /// Quick capture (create issue, print ID only)
    Q(QuickArgs),

    /// List issues
    List(ListArgs),

    /// Show issue details
    Show(ShowArgs),

    /// Update an issue
    Update(UpdateArgs),

    /// Close an issue
    Close(CloseArgs),

    /// Reopen an issue
    Reopen(ReopenArgs),

    /// Delete an issue (creates tombstone)
    Delete(DeleteArgs),

    /// List ready issues (unblocked, not deferred)
    Ready(ReadyArgs),

    /// List blocked issues
    Blocked(BlockedArgs),

    /// Search issues
    Search(SearchArgs),

    /// Manage dependencies
    Dep {
        #[command(subcommand)]
        command: DepCommands,
    },

    /// Manage labels
    Label {
        #[command(subcommand)]
        command: LabelCommands,
    },

    /// Epic management commands
    Epic {
        #[command(subcommand)]
        command: EpicCommands,
    },

    /// Manage comments
    #[command(alias = "comment")]
    Comments(CommentsArgs),

    /// Show project statistics
    Stats(StatsArgs),

    /// Alias for stats
    Status(StatsArgs),

    /// Count issues with optional grouping
    Count(CountArgs),

    /// List stale issues
    Stale(StaleArgs),

    /// Check issues for missing template sections
    Lint(LintArgs),

    /// Defer issues (schedule for later)
    Defer(DeferArgs),

    /// Undefer issues (make ready again)
    Undefer(UndeferArgs),

    /// Configuration management
    Config {
        #[command(subcommand)]
        command: ConfigCommands,
    },

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

MODES (one required unless --status):
  --flush-only    Export database to JSONL (safe by default)
  --import-only   Import JSONL into database (validates first)
  --status        Show sync status (read-only)

SAFETY GUARDS:
  Export guards (bypassed with --force):
    • Empty DB Guard: Refuses to export empty DB over non-empty JSONL
    • Stale DB Guard: Refuses to export if JSONL has issues missing from DB

  Import guards (cannot be bypassed):
    • Conflict markers: Rejects files with git merge conflict markers
    • Invalid JSON: Rejects malformed JSONL entries

VERBOSE LOGGING:
  -v     Show INFO-level safety guard decisions
  -vv    Show DEBUG-level file operations

EXAMPLES:
  br sync --flush-only           Export database to .beads/issues.jsonl
  br sync --flush-only -v        Export with safety logging
  br sync --import-only          Import from JSONL (validates first)
  br sync --status               Show current sync status")]
    Sync(SyncArgs),

    /// Run read-only diagnostics
    Doctor,

    /// Show diagnostic metadata about the workspace
    Info(InfoArgs),

    /// Emit JSON Schemas for br output types (for agent/tooling integration)
    Schema(SchemaArgs),

    /// Show the active .beads directory
    Where,

    /// Show version information
    Version(VersionArgs),

    /// Upgrade br to the latest version
    #[cfg(feature = "self_update")]
    Upgrade(UpgradeArgs),

    /// Generate shell completions
    #[command(alias = "completion")]
    Completions(CompletionsArgs),

    /// Record and label agent interactions (append-only JSONL)
    Audit {
        #[command(subcommand)]
        command: AuditCommands,
    },

    /// Manage local history backups
    History(HistoryArgs),
    /// List orphan issues (referenced in commits but open)
    Orphans(OrphansArgs),
    /// Generate changelog from closed issues
    Changelog(ChangelogArgs),

    /// Manage saved queries
    Query {
        #[command(subcommand)]
        command: QueryCommands,
    },

    /// Visualize dependency graph
    Graph(GraphArgs),

    /// Manage AGENTS.md workflow instructions
    Agents(AgentsArgs),
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
    #[arg(long = "title")]
    pub title_flag: Option<String>, // Handled in logic

    /// Issue type (task, bug, feature, etc.)
    #[arg(long = "type", short = 't', add = ArgValueCompleter::new(issue_type_completer))]
    pub type_: Option<String>,

    /// Priority (0-4 or P0-P4)
    #[arg(long, short = 'p', add = ArgValueCompleter::new(priority_completer))]
    pub priority: Option<String>,

    /// Description
    #[arg(long, short = 'd', visible_alias = "body")]
    pub description: Option<String>,

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

    /// Preview without creating
    #[arg(long)]
    pub dry_run: bool,

    /// Output only issue ID
    #[arg(long)]
    pub silent: bool,

    /// Create issues from a markdown file (bulk import)
    #[arg(long, short = 'f')]
    pub file: Option<std::path::PathBuf>,
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
}

#[derive(Args, Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct UpdateArgs {
    /// Issue IDs to update
    #[arg(add = ArgValueCompleter::new(issue_id_completer))]
    pub ids: Vec<String>,

    /// Update title
    #[arg(long)]
    pub title: Option<String>,

    /// Update description
    #[arg(long, visible_alias = "body")]
    pub description: Option<String>,

    /// Update design notes
    #[arg(long)]
    pub design: Option<String>,

    /// Update acceptance criteria
    #[arg(long, visible_alias = "acceptance")]
    pub acceptance_criteria: Option<String>,

    /// Update additional notes
    #[arg(long)]
    pub notes: Option<String>,

    /// Change status
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
    #[arg(long, add = ArgValueCompleter::new(label_completer_delimited))]
    pub set_labels: Vec<String>,

    /// Reparent to new parent (empty string removes parent)
    #[arg(long, add = ArgValueCompleter::new(issue_id_completer))]
    pub parent: Option<String>,

    /// Set external reference
    #[arg(long)]
    pub external_ref: Option<String>,

    /// Set `closed_by_session` when closing
    #[arg(long)]
    pub session: Option<String>,
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

    /// Show recent changes and exit
    #[arg(long = "whats-new", conflicts_with = "thanks")]
    pub whats_new: bool,

    /// Show acknowledgements and exit
    #[arg(long, conflicts_with = "whats_new")]
    pub thanks: bool,
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

/// Schema targets for `br schema`.
#[derive(ValueEnum, Debug, Clone, Copy, Default, Eq, PartialEq)]
pub enum SchemaTarget {
    /// Emit a bundle containing all schemas
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
    /// Structured error envelope (stderr JSON when robot mode or non-TTY)
    Error,
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
        if let Ok(value) = std::env::var("BR_OUTPUT_FORMAT") {
            if let Some(format) = Self::parse_env_value(&value) {
                return Some(format);
            }
        }
        if let Ok(value) = std::env::var("TOON_DEFAULT_FORMAT") {
            if let Some(format) = Self::parse_env_value(&value) {
                return Some(format);
            }
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

/// Arguments for the list command.
#[derive(Args, Debug, Default, Clone)]
#[allow(clippy::struct_excessive_bools)]
pub struct ListArgs {
    /// Filter by status (can be repeated)
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

    /// Maximum number of results (0 = unlimited, default: 50)
    #[arg(long)]
    pub limit: Option<usize>,

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

    /// Wrap long lines instead of truncating in text output
    #[arg(long)]
    pub wrap: bool,

    /// Show token savings stats when using TOON output
    #[arg(long)]
    pub stats: bool,
}

#[derive(Subcommand, Debug)]
pub enum DepCommands {
    /// Add a dependency: <issue> depends on <depends-on>
    Add(DepAddArgs),
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

    /// Override author (defaults to actor/env/git)
    #[arg(long)]
    pub author: Option<String>,

    /// Comment text (alternative flag)
    #[arg(long = "message")]
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

    /// Filter by status (repeatable or comma-separated)
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

    /// Include closed and tombstone issues
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
}

/// Arguments for the ready command.
#[derive(Args, Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct ReadyArgs {
    /// Maximum number of issues to return (default: 20, 0 = unlimited)
    #[arg(long, default_value_t = 20)]
    pub limit: usize,

    /// Filter by assignee (no value = current actor)
    #[arg(long, add = ArgValueCompleter::new(assignee_completer))]
    pub assignee: Option<String>,

    /// Show only unassigned issues
    #[arg(long)]
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

    /// Filter by priority (can be repeated, 0-4 or P0-P4)
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
    #[arg(long, short = 'r')]
    pub reason: Option<String>,

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
}

/// Arguments for the reopen command.
#[derive(Args, Debug, Clone, Default)]
pub struct ReopenArgs {
    /// Issue IDs to reopen (uses last-touched if empty)
    #[arg(add = ArgValueCompleter::new(closed_issue_id_completer))]
    pub ids: Vec<String>,

    /// Reason for reopening (stored as a comment)
    #[arg(long, short = 'r')]
    pub reason: Option<String>,

    /// Machine-readable output (alias for --json)
    #[arg(long)]
    pub robot: bool,
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

/// Arguments for the sync command.
#[derive(Args, Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct SyncArgs {
    /// Export database to JSONL (DB → .beads/issues.jsonl)
    ///
    /// Writes all issues from `SQLite` database to JSONL format.
    ///
    /// This is the default if the database is newer than the JSONL file.
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
    /// Uses `.beads/base_snapshot.jsonl` as the common ancestor.
    #[arg(long)]
    pub merge: bool,

    /// Show sync status (read-only)
    ///
    /// Displays hash comparison and freshness info without modifications.
    #[arg(long)]
    pub status: bool,

    /// Override safety guards (use with caution!)
    ///
    /// Bypasses Empty DB Guard and Stale DB Guard for export.
    /// Does NOT bypass conflict marker detection or JSON validation.
    #[arg(long, short = 'f')]
    pub force: bool,

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
    #[arg(long)]
    pub rename_prefix: bool,

    /// Machine-readable output (alias for --json)
    #[arg(long)]
    pub robot: bool,
}

#[derive(Subcommand, Debug, Clone)]
pub enum ConfigCommands {
    /// List all available config options
    List {
        /// Show only project config
        #[arg(long)]
        project: bool,

        /// Show only user config
        #[arg(long)]
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

    /// Include recent activity stats (requires git). Now shown by default.
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
#[derive(Args, Debug, Clone, Default)]
pub struct GraphArgs {
    /// Issue ID (root of graph). Required unless --all is specified.
    #[arg(add = ArgValueCompleter::new(open_issue_id_completer))]
    pub issue: Option<String>,

    /// Show graph for all `open`/`in_progress`/`blocked` issues (connected components)
    #[arg(long)]
    pub all: bool,

    /// One line per issue (compact output)
    #[arg(long)]
    pub compact: bool,
}

/// Arguments for the agents command.
#[derive(Args, Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct AgentsArgs {
    /// Add beads workflow instructions to AGENTS.md
    #[arg(long)]
    pub add: bool,

    /// Remove beads workflow instructions from AGENTS.md
    #[arg(long)]
    pub remove: bool,

    /// Update beads workflow instructions to latest version
    #[arg(long)]
    pub update: bool,

    /// Check status only (default behavior)
    #[arg(long)]
    pub check: bool,

    /// Preview changes without modifying files
    #[arg(long)]
    pub dry_run: bool,

    /// Skip confirmation prompts
    #[arg(long, short = 'f')]
    pub force: bool,
}
