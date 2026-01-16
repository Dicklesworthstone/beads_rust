//! Command-line interface for `beads_rust`.
//!
//! This module provides the CLI parsing and command routing using clap.

pub mod commands;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

/// `beads_rust` (br) - Agent-first issue tracker.
#[derive(Parser, Debug)]
#[command(name = "br")]
#[command(
    author,
    version,
    about = "Agent-first issue tracker (SQLite + JSONL)",
    long_about = None,
    after_help = "Non-invasive: no git hooks/ops, no daemons, no external integrations."
)]
#[allow(clippy::struct_excessive_bools)]
pub struct Cli {
    /// Output format: text (default) or json
    #[arg(long, global = true)]
    pub json: bool,

    /// Robot mode (clean JSON output, diagnostics to stderr)
    #[arg(long, global = true)]
    pub robot: bool,

    /// Verbose output
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    /// Quiet mode (errors only)
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Operate without `SQLite` (JSONL-only mode)
    #[arg(long, global = true)]
    pub no_db: bool,

    /// The command to run
    #[command(subcommand)]
    pub command: Option<Commands>,
}

/// Available commands.
#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Initialize a beads workspace
    Init,

    /// Create a new issue
    Create,

    /// Update an existing issue
    Update,

    /// Close one or more issues
    Close,

    /// Reopen a closed issue
    Reopen,

    /// Delete (tombstone) one or more issues
    Delete,

    /// List issues
    List,

    /// Show issue details
    Show,

    /// List ready (unblocked) issues
    Ready,

    /// List blocked issues
    Blocked,

    /// Search issues
    Search,

    /// Stats summary (alias: status)
    #[command(alias = "status")]
    Stats,

    /// Count issues
    Count,

    /// List stale issues
    Stale,

    /// List orphaned issues
    Orphans,

    /// Manage dependencies
    Dep(DepCommand),

    /// Manage labels
    Label(LabelCommand),

    /// Manage comments (alias: comment)
    #[command(alias = "comment")]
    Comments(CommentsCommand),

    /// Defer an issue until a future time
    Defer,

    /// Clear defer state
    Undefer,

    /// Sync JSONL import/export
    Sync(SyncArgs),

    /// Read/write configuration
    Config(ConfigCommand),

    /// Filter issues with where-style queries
    Where,

    /// Show repository / workspace info
    Info,

    /// Show version information
    Version,

    /// Quick query (alias for search)
    #[command(name = "q")]
    Q,

    /// Lint issues and JSONL
    Lint,

    /// Dependency graph output
    Graph,

    /// Epic-related commands
    Epic,
}

#[derive(Args, Debug)]
pub struct DepCommand {
    /// Dependency subcommand
    #[command(subcommand)]
    pub command: Option<DepSubcommand>,
}

#[derive(Subcommand, Debug)]
pub enum DepSubcommand {
    /// Add a dependency
    Add,

    /// Remove a dependency
    Remove,

    /// List dependencies
    List,

    /// Show dependency tree
    Tree,

    /// Detect cycles
    Cycles,
}

#[derive(Args, Debug)]
pub struct LabelCommand {
    /// Label subcommand
    #[command(subcommand)]
    pub command: Option<LabelSubcommand>,
}

#[derive(Subcommand, Debug)]
pub enum LabelSubcommand {
    /// Add label(s) to an issue
    Add,

    /// Remove label(s) from an issue
    Remove,

    /// List labels for an issue
    List,

    /// List all labels
    ListAll,
}

#[derive(Args, Debug)]
pub struct CommentsCommand {
    /// Comments subcommand
    #[command(subcommand)]
    pub command: Option<CommentsSubcommand>,
}

#[derive(Subcommand, Debug)]
pub enum CommentsSubcommand {
    /// Add a comment
    Add,

    /// List comments
    List,
}

#[derive(Args, Debug)]
pub struct SyncArgs {
    /// Export JSONL only
    #[arg(long)]
    pub flush_only: bool,

    /// Import JSONL only
    #[arg(long)]
    pub import_only: bool,
}

#[derive(Args, Debug)]
pub struct ConfigCommand {
    /// Config subcommand
    #[command(subcommand)]
    pub command: Option<ConfigSubcommand>,
}

#[derive(Subcommand, Debug)]
pub enum ConfigSubcommand {
    /// Get a config value
    Get,

    /// Set a config value
    Set,

    /// List config values
    List,
}

/// Run the CLI.
///
/// # Errors
///
/// Returns an error if the command fails to execute.
pub fn run() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Version) => {
            println!("br {}", env!("CARGO_PKG_VERSION"));
        }
        Some(command) => {
            println!("{} command not yet implemented", command.name());
        }
        None => println!("br - Agent-first issue tracker. Use --help for usage."),
    }

    Ok(())
}

impl Commands {
    const fn name(&self) -> &'static str {
        match self {
            Self::Init => "init",
            Self::Create => "create",
            Self::Update => "update",
            Self::Close => "close",
            Self::Reopen => "reopen",
            Self::Delete => "delete",
            Self::List => "list",
            Self::Show => "show",
            Self::Ready => "ready",
            Self::Blocked => "blocked",
            Self::Search => "search",
            Self::Stats => "stats",
            Self::Count => "count",
            Self::Stale => "stale",
            Self::Orphans => "orphans",
            Self::Dep(dep) => match dep.command {
                Some(DepSubcommand::Add) => "dep add",
                Some(DepSubcommand::Remove) => "dep remove",
                Some(DepSubcommand::List) => "dep list",
                Some(DepSubcommand::Tree) => "dep tree",
                Some(DepSubcommand::Cycles) => "dep cycles",
                None => "dep",
            },
            Self::Label(label) => match label.command {
                Some(LabelSubcommand::Add) => "label add",
                Some(LabelSubcommand::Remove) => "label remove",
                Some(LabelSubcommand::List) => "label list",
                Some(LabelSubcommand::ListAll) => "label list-all",
                None => "label",
            },
            Self::Comments(comments) => match comments.command {
                Some(CommentsSubcommand::Add) => "comments add",
                Some(CommentsSubcommand::List) | None => "comments",
            },
            Self::Defer => "defer",
            Self::Undefer => "undefer",
            Self::Sync(_) => "sync",
            Self::Config(config) => match config.command {
                Some(ConfigSubcommand::Get) => "config get",
                Some(ConfigSubcommand::Set) => "config set",
                Some(ConfigSubcommand::List) => "config list",
                None => "config",
            },
            Self::Where => "where",
            Self::Info => "info",
            Self::Version => "version",
            Self::Q => "q",
            Self::Lint => "lint",
            Self::Graph => "graph",
            Self::Epic => "epic",
        }
    }
}
