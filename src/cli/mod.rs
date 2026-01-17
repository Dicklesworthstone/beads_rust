//! CLI definitions and entry point.

use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

pub mod commands;

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
    Show {
        /// Issue IDs
        ids: Vec<String>,
    },

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
    Config(ConfigArgs),

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

    /// Show version information
    Version,

    /// Upgrade br to the latest version
    #[cfg(feature = "self_update")]
    Upgrade(UpgradeArgs),

    /// Generate shell completions
    Completions(CompletionsArgs),

    /// Manage local history backups
    History(HistoryArgs),
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

#[derive(Args, Debug)]
pub struct CreateArgs {
    /// Issue title
    pub title: Option<String>,

    /// Issue title (alternative flag)
    #[arg(long)]
    pub title_flag: Option<String>, // Handled in logic

    /// Issue type (task, bug, feature, etc.)
    #[arg(long = "type", short = 't')]
    pub type_: Option<String>,

    /// Priority (0-4 or P0-P4)
    #[arg(long, short = 'p')]
    pub priority: Option<String>,

    /// Description
    #[arg(long, short = 'd')]
    pub description: Option<String>,

    /// Assign to person
    #[arg(long, short = 'a')]
    pub assignee: Option<String>,

    /// Set owner email
    #[arg(long)]
    pub owner: Option<String>,

    /// Labels (comma-separated)
    #[arg(long, short = 'l', value_delimiter = ',')]
    pub labels: Vec<String>,

    /// Parent issue ID (creates parent-child dep)
    #[arg(long)]
    pub parent: Option<String>,

    /// Dependencies (format: type:id,type:id)
    #[arg(long, value_delimiter = ',')]
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

    /// Preview without creating
    #[arg(long)]
    pub dry_run: bool,

    /// Output only issue ID
    #[arg(long)]
    pub silent: bool,
}

#[derive(Args, Debug)]
pub struct QuickArgs {
    /// Issue title words
    pub title: Vec<String>,

    /// Priority (0-4 or P0-P4)
    #[arg(long, short = 'p')]
    pub priority: Option<String>,

    /// Issue type (task, bug, feature, etc.)
    #[arg(long = "type", short = 't')]
    pub type_: Option<String>,

    /// Labels to apply (repeatable, comma-separated allowed)
    #[arg(long, short = 'l')]
    pub labels: Vec<String>,
}

#[derive(Args, Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct UpdateArgs {
    /// Issue IDs to update
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
    #[arg(long, short = 's')]
    pub status: Option<String>,

    /// Change priority (0-4 or P0-P4)
    #[arg(long, short = 'p')]
    pub priority: Option<String>,

    /// Change issue type
    #[arg(long = "type", short = 't')]
    pub type_: Option<String>,

    /// Assign to user (empty string clears)
    #[arg(long)]
    pub assignee: Option<String>,

    /// Set owner (empty string clears)
    #[arg(long)]
    pub owner: Option<String>,

    /// Atomic claim (assignee=actor + `status=in_progress`)
    #[arg(long)]
    pub claim: bool,

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
    #[arg(long)]
    pub add_label: Vec<String>,

    /// Remove label(s)
    #[arg(long)]
    pub remove_label: Vec<String>,

    /// Set label(s) (replaces all)
    #[arg(long)]
    pub set_labels: Option<String>,

    /// Reparent to new parent (empty string removes parent)
    #[arg(long)]
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

/// Arguments for the list command.
#[derive(Args, Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct ListArgs {
    /// Filter by status (can be repeated)
    #[arg(long, short = 's')]
    pub status: Vec<String>,

    /// Filter by issue type (can be repeated)
    #[arg(long = "type", short = 't')]
    pub type_: Vec<String>,

    /// Filter by assignee
    #[arg(long)]
    pub assignee: Option<String>,

    /// Filter for unassigned issues only
    #[arg(long)]
    pub unassigned: bool,

    /// Filter by specific IDs (can be repeated)
    #[arg(long)]
    pub id: Vec<String>,

    /// Filter by label (AND logic, can be repeated)
    #[arg(long, short = 'l')]
    pub label: Vec<String>,

    /// Filter by label (OR logic, can be repeated)
    #[arg(long)]
    pub label_any: Vec<String>,

    /// Filter by priority (can be repeated)
    #[arg(long, short = 'p')]
    pub priority: Vec<u8>,

    /// Filter by minimum priority (0=critical, 4=backlog)
    #[arg(long)]
    pub priority_min: Option<u8>,

    /// Filter by maximum priority
    #[arg(long)]
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
    #[arg(long)]
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
}

/// Arguments for the search command.
#[derive(Args, Debug, Default)]
pub struct SearchArgs {
    /// Search query
    pub query: String,

    #[command(flatten)]
    pub filters: ListArgs,
}

#[derive(Subcommand, Debug)]
pub enum DepCommands {
    /// Add a dependency: <issue> depends on <depends-on>
    Add(DepAddArgs),
    /// Remove a dependency
    Remove(DepRemoveArgs),
    /// List dependencies of an issue
    List(DepListArgs),
    /// Show dependency tree rooted at issue
    Tree(DepTreeArgs),
    /// Detect and report dependency cycles
    Cycles(DepCyclesArgs),
}

#[derive(Args, Debug)]
pub struct DepAddArgs {
    /// Issue ID (the one that will depend on something)
    pub issue: String,

    /// Target issue ID (the one being depended on)
    pub depends_on: String,

    /// Dependency type (blocks, parent-child, related, etc.)
    #[arg(long = "type", short = 't', default_value = "blocks")]
    pub dep_type: String,

    /// Optional JSON metadata
    #[arg(long)]
    pub metadata: Option<String>,
}

#[derive(Args, Debug)]
pub struct DepRemoveArgs {
    /// Issue ID
    pub issue: String,

    /// Target issue ID to remove dependency to
    pub depends_on: String,
}

#[derive(Args, Debug)]
pub struct DepListArgs {
    /// Issue ID
    pub issue: String,

    /// Direction: down (what issue depends on), up (what depends on issue), both
    #[arg(long, default_value = "down", value_enum)]
    pub direction: DepDirection,

    /// Filter by dependency type
    #[arg(long = "type", short = 't')]
    pub dep_type: Option<String>,
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
    pub issue: String,

    /// Maximum depth (default: 10)
    #[arg(long, default_value_t = 10)]
    pub max_depth: usize,

    /// Output format: text, mermaid
    #[arg(long, default_value = "text")]
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
    pub issues: Vec<String>,

    /// Label to add
    #[arg(long, short = 'l')]
    pub label: Option<String>,
}

#[derive(Args, Debug)]
pub struct LabelRemoveArgs {
    /// Issue ID(s) to remove label from
    pub issues: Vec<String>,

    /// Label to remove
    #[arg(long, short = 'l')]
    pub label: Option<String>,
}

#[derive(Args, Debug)]
pub struct LabelListArgs {
    /// Issue ID (optional - if omitted, lists all unique labels)
    pub issue: Option<String>,
}

#[derive(Args, Debug)]
pub struct LabelRenameArgs {
    /// Current label name
    pub old_name: String,

    /// New label name
    pub new_name: String,
}

#[derive(Args, Debug)]
pub struct CommentsArgs {
    #[command(subcommand)]
    pub command: Option<CommentCommands>,

    /// Issue ID (for listing comments)
    pub id: Option<String>,
}

#[derive(Subcommand, Debug)]
pub enum CommentCommands {
    Add(CommentAddArgs),
    List(CommentListArgs),
}

#[derive(Args, Debug)]
pub struct CommentAddArgs {
    /// Issue ID
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
    pub id: String,
}

#[derive(Args, Debug, Clone)]
pub struct CountArgs {
    /// Group counts by field
    #[arg(long, value_enum)]
    pub by: Option<CountBy>,

    /// Filter by status (repeatable or comma-separated)
    #[arg(long, value_delimiter = ',')]
    pub status: Vec<String>,

    /// Filter by issue type (repeatable or comma-separated)
    #[arg(long = "type", value_delimiter = ',')]
    pub types: Vec<String>,

    /// Filter by priority (0-4 or P0-P4; repeatable or comma-separated)
    #[arg(long, value_delimiter = ',')]
    pub priority: Vec<String>,

    /// Filter by assignee
    #[arg(long)]
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
    #[arg(long, value_delimiter = ',')]
    pub status: Vec<String>,
}

#[derive(Args, Debug, Clone, Default)]
pub struct LintArgs {
    /// Issue IDs to lint (defaults to open issues)
    pub ids: Vec<String>,

    /// Filter by issue type (bug, task, feature, epic)
    #[arg(long, short = 't')]
    pub type_: Option<String>,

    /// Filter by status (default: open, use 'all' for all)
    #[arg(long, short = 's')]
    pub status: Option<String>,
}

/// Arguments for the defer command.
#[derive(Args, Debug, Clone, Default)]
pub struct DeferArgs {
    /// Issue IDs to defer
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
    #[arg(long)]
    pub assignee: Option<String>,

    /// Show only unassigned issues
    #[arg(long)]
    pub unassigned: bool,

    /// Filter by label (AND logic, can be repeated)
    #[arg(long, short = 'l')]
    pub label: Vec<String>,

    /// Filter by label (OR logic, can be repeated)
    #[arg(long)]
    pub label_any: Vec<String>,

    /// Filter by issue type (can be repeated)
    #[arg(long = "type", short = 't')]
    pub type_: Vec<String>,

    /// Filter by priority (can be repeated, 0-4 or P0-P4)
    #[arg(long, short = 'p')]
    pub priority: Vec<String>,

    /// Sort policy: hybrid (default), priority, oldest
    #[arg(long, default_value = "hybrid", value_enum)]
    pub sort: SortPolicy,

    /// Include deferred issues
    #[arg(long)]
    pub include_deferred: bool,

    /// Machine-readable output (alias for --json)
    #[arg(long)]
    pub robot: bool,
}

/// Arguments for the blocked command.
#[derive(Args, Debug, Clone, Default)]
pub struct BlockedArgs {
    /// Maximum number of issues to return (default: 50, 0 = unlimited)
    #[arg(long, default_value_t = 50)]
    pub limit: usize,

    /// Include full blocker details in text output
    #[arg(long)]
    pub detailed: bool,

    /// Filter by issue type (can be repeated)
    #[arg(long = "type", short = 't')]
    pub type_: Vec<String>,

    /// Filter by priority (can be repeated, 0-4)
    #[arg(long, short = 'p')]
    pub priority: Vec<u8>,

    /// Filter by label (AND logic, can be repeated)
    #[arg(long, short = 'l')]
    pub label: Vec<String>,

    /// Machine-readable output (alias for --json)
    #[arg(long)]
    pub robot: bool,
}

/// Arguments for the close command.
#[derive(Args, Debug, Clone, Default)]
pub struct CloseArgs {
    /// Issue IDs to close (uses last-touched if empty)
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
    #[arg(long = "error-policy")]
    pub error_policy: Option<String>,

    /// Orphan handling mode: strict (default), resurrect, skip, allow
    ///
    /// Controls how import handles orphaned dependencies (refs to deleted issues).
    #[arg(long)]
    pub orphans: Option<String>,

    /// Machine-readable output (alias for --json)
    #[arg(long)]
    pub robot: bool,
}

/// Arguments for the config command.
#[derive(Args, Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct ConfigArgs {
    /// List all available config options with descriptions
    #[arg(long, short = 'l')]
    pub list: bool,

    /// Get a specific config value by key
    #[arg(long, short = 'g', value_name = "KEY")]
    pub get: Option<String>,

    /// Set a config value in user config (format: key=value)
    #[arg(long, short = 's', value_name = "KEY=VALUE")]
    pub set: Option<String>,

    /// Delete a config value from the database (DB keys only, not YAML)
    #[arg(long, short = 'd', visible_alias = "unset", value_name = "KEY")]
    pub delete: Option<String>,

    /// Open user config file in $EDITOR
    #[arg(long, short = 'e')]
    pub edit: bool,

    /// Show config file paths
    #[arg(long, short = 'p')]
    pub path: bool,

    /// Show only project config (from .beads/config.yaml)
    #[arg(long)]
    pub project: bool,

    /// Show only user config (from ~/.config/bd/config.yaml)
    #[arg(long)]
    pub user: bool,
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
