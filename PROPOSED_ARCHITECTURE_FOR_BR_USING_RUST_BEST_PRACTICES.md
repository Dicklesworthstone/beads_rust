# Proposed Architecture for `br` (beads_rust)

## Using Rust Best Practices from xf & cass

> **Author**: Claude Opus 4.5
> **Date**: 2026-01-16
> **Status**: Proposed Architecture
> **Reference Projects**: `/data/projects/xf`, `/data/projects/coding_agent_session_search`

---

## Executive Summary

This document proposes the optimal architecture for `br`, the Rust port of the beads issue tracker. It synthesizes proven patterns from two production Rust CLI projects (xf and cass) with the authoritative specification in `EXISTING_BEADS_STRUCTURE_AND_ARCHITECTURE.md`.

**Key Design Principles:**

1. **Non-invasive by default** — No automatic git operations, hooks, or daemons
2. **SQLite as source of truth** — JSONL for portability and git-based collaboration
3. **Layered configuration** — CLI → env → file → defaults (like xf)
4. **Streaming-friendly** — Producer-consumer patterns for large operations (like cass)
5. **Dual output modes** — Human-readable vs machine-parseable (robot mode)
6. **Comprehensive error handling** — Structured errors with recovery hints
7. **Performance-conscious** — Caching, batch operations, parallel processing

---

## Non-Negotiable Requirements

These are **hard constraints** that the architecture must honor:

| Requirement | Description |
|------------|-------------|
| **SQLite + JSONL hybrid** | Core storage model; no Dolt backend |
| **Schema compatibility** | Tables, constraints, indexes, and semantics must match Go `bd` |
| **CLI compatibility** | Commands, flags, output format parity where required |
| **Hash-based short IDs** | e.g., `bd-abc123`, not autoincrement IDs |
| **Deterministic content hashing** | Same inputs ⇒ same hash |
| **Non-invasive** | No auto git hooks, no auto git ops, no daemon/RPC, no background processes |
| **JSON output stability** | Machine parseable, unchanged shapes unless explicitly intended |
| **Robot mode** | `--json` / `--robot` must be clean JSON to stdout with diagnostics to stderr |
| **No unsafe code** | `#![forbid(unsafe_code)]` enforced at crate level |

---

## High-Level System Map

```
┌────────────────────────────────────────────────────────────────────┐
│                              CLI Layer                              │
│     clap derive + command routing + output formatting               │
└─────────────────────────────────┬──────────────────────────────────┘
                                  │
                                  ▼
┌────────────────────────────────────────────────────────────────────┐
│                          Context Layer                              │
│       Config + Paths + Actor + OutputMode + Logger                  │
└─────────────────────────────────┬──────────────────────────────────┘
                                  │
                                  ▼
┌────────────────────────────────────────────────────────────────────┐
│                         Core Services                               │
│      Model + Validation + ID/Hash + JSONL Import/Export             │
└─────────────────────────────────┬──────────────────────────────────┘
                                  │
                                  ▼
┌────────────────────────────────────────────────────────────────────┐
│                          Storage Layer                              │
│    rusqlite + schema + migrations + queries + cache tables          │
└────────────────────────────────────────────────────────────────────┘
```

---

## 1. Project Structure

### 1.1 Directory Layout

```
beads_rust/
├── Cargo.toml                 # Workspace root (single crate for v1)
├── Cargo.lock
├── rust-toolchain.toml        # Edition 2024, nightly
├── build.rs                   # Version embedding (vergen)
├── .cargo/
│   └── config.toml            # Linker optimizations
├── src/
│   ├── main.rs                # Minimal entry point (~50 lines)
│   ├── lib.rs                 # Public API exports
│   ├── context.rs             # Shared Context (config, output, actor, logger)
│   ├── logging.rs             # tracing setup (xf pattern)
│   ├── cli/
│   │   ├── mod.rs             # Re-exports
│   │   ├── args.rs            # clap derive definitions
│   │   ├── output.rs          # OutputFormat, robot mode
│   │   └── commands/          # Per-command modules
│   │       ├── mod.rs
│   │       ├── create.rs
│   │       ├── update.rs
│   │       ├── close.rs
│   │       ├── list.rs
│   │       ├── show.rs
│   │       ├── ready.rs
│   │       ├── blocked.rs
│   │       ├── search.rs
│   │       ├── dep.rs
│   │       ├── label.rs
│   │       ├── comments.rs
│   │       ├── import.rs
│   │       ├── export.rs
│   │       ├── sync.rs        # flush-only, import-only
│   │       ├── doctor.rs      # Read-only diagnostics
│   │       ├── config.rs
│   │       ├── info.rs
│   │       └── init.rs
│   ├── model/
│   │   ├── mod.rs
│   │   ├── issue.rs           # Issue struct + validation
│   │   ├── dependency.rs      # Dependency, DependencyType
│   │   ├── status.rs          # Status enum + custom statuses
│   │   ├── issue_type.rs      # IssueType enum
│   │   ├── comment.rs         # Comment struct
│   │   ├── event.rs           # Event for audit log
│   │   └── id.rs              # ID generation, hashing
│   ├── storage/
│   │   ├── mod.rs             # Storage trait + factory
│   │   ├── sqlite.rs          # SQLite implementation
│   │   ├── schema.rs          # Schema definition, migrations
│   │   ├── queries/           # Organized SQL modules
│   │   │   ├── mod.rs
│   │   │   ├── issues.rs
│   │   │   ├── dependencies.rs
│   │   │   ├── labels.rs
│   │   │   ├── comments.rs
│   │   │   └── cache.rs       # blocked_issues_cache
│   │   └── batch.rs           # Batch operations
│   ├── sync/
│   │   ├── mod.rs
│   │   ├── export.rs          # JSONL export
│   │   ├── import.rs          # JSONL import with collision detection
│   │   ├── merge.rs           # 3-way merge logic (standalone)
│   │   └── dirty.rs           # Dirty tracking
│   ├── config/
│   │   ├── mod.rs
│   │   ├── loader.rs          # Layered config loading
│   │   ├── types.rs           # Config structs
│   │   └── validate.rs        # Config validation
│   ├── error/
│   │   ├── mod.rs             # BeadsError enum
│   │   ├── context.rs         # ResultExt trait
│   │   └── format.rs          # User-friendly formatting
│   ├── format/
│   │   ├── mod.rs
│   │   ├── text.rs            # Human-readable output
│   │   ├── json.rs            # JSON/JSONL output
│   │   ├── tree.rs            # Tree rendering (dep tree, pretty list)
│   │   └── table.rs           # Tabular output
│   └── util/
│       ├── mod.rs
│       ├── hash.rs            # Content hashing (SHA256)
│       ├── time.rs            # RFC3339 parsing, relative times
│       ├── path.rs            # .beads discovery, tilde expansion
│       └── progress.rs        # Atomic progress tracking
├── tests/
│   ├── integration/
│   │   ├── mod.rs
│   │   ├── crud_test.rs
│   │   ├── jsonl_test.rs
│   │   └── conformance/       # bd ↔ br parity tests
│   └── fixtures/
│       └── sample_issues.jsonl
└── benches/
    ├── storage_bench.rs
    └── search_bench.rs
```

### 1.2 Module Responsibilities

| Module | Responsibility | Key Patterns |
|--------|---------------|--------------|
| `context.rs` | Shared application context | Config + paths + actor + output mode |
| `logging.rs` | Structured logging setup | tracing + EnvFilter (xf pattern) |
| `cli/` | Command-line interface | clap derive, subcommands, global flags |
| `model/` | Data structures & validation | thiserror for validation errors |
| `storage/` | SQLite persistence | rusqlite, WAL mode, prepared statements |
| `sync/` | JSONL import/export | Streaming, collision detection |
| `config/` | Configuration management | Layered loading, env overrides |
| `error/` | Error types & formatting | thiserror + anyhow hybrid |
| `format/` | Output formatting | Dual human/robot modes |
| `util/` | Shared utilities | Hashing, time parsing, path handling |

---

## 2. Dependency Strategy

### 2.1 Cargo.toml

```toml
[package]
name = "beads_rust"
version = "0.1.0"
edition = "2024"
rust-version = "1.85"
default-run = "br"

[[bin]]
name = "br"
path = "src/main.rs"

[dependencies]
# Error Handling (xf pattern)
anyhow = "1.0"
thiserror = "1.0"

# CLI (xf + cass pattern)
clap = { version = "4.5", features = [
    "derive",
    "cargo",
    "env",
    "unicode",
    "wrap_help",
    "string"
] }

# Serialization
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"

# SQLite (xf pattern)
rusqlite = { version = "0.32", features = [
    "bundled",
    "modern_sqlite",
    "backup"
] }

# Time handling
chrono = { version = "0.4", features = ["serde"] }

# Hashing
sha2 = "0.10"
base64 = "0.22"

# Parallelism (xf pattern)
rayon = "1.10"

# Logging (xf pattern)
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = [
    "env-filter",
    "fmt",
    "ansi"
] }

# Configuration
toml = "0.8"
dirs = "5.0"

# Terminal output
colored = "2.1"
unicode-width = "0.2"

# Utilities
once_cell = "1.19"
parking_lot = "0.12"       # Faster Mutex/RwLock (cass pattern)

[dev-dependencies]
tempfile = "3.10"
assert_cmd = "2.0"
predicates = "3.1"
criterion = { version = "0.5", features = ["html_reports"] }
insta = { version = "1.38", features = ["yaml", "json"] }

[build-dependencies]
vergen-gix = { version = "1.0", features = ["build", "cargo", "rustc"] }

[profile.release]
opt-level = "z"            # Optimize for size (distribution binary)
lto = true                 # Link-time optimization
codegen-units = 1          # Better optimization
panic = "abort"            # Smaller binary
strip = true               # Remove debug symbols

[lints.rust]
unsafe_code = "forbid"
```

### 2.2 Dependency Rationale

| Crate | Purpose | Justification |
|-------|---------|---------------|
| `anyhow` | Internal error propagation | Flexible, ergonomic for application code |
| `thiserror` | Public error types | Derive-based, good for library boundaries |
| `clap` | CLI parsing | Industry standard, derive macros reduce boilerplate |
| `rusqlite` | SQLite access | Mature, bundled SQLite avoids system deps |
| `chrono` | Time handling | RFC3339 support, timezone-aware |
| `sha2` | Content hashing | Fast, pure Rust, no unsafe |
| `rayon` | Parallelism | Data-parallel iterators, thread pool |
| `tracing` | Structured logging | Async-compatible, spans for context |
| `parking_lot` | Synchronization | 2-3x faster than std Mutex (cass pattern) |
| `colored` | Terminal colors | Simple, respects NO_COLOR |

---

## 3. Error Handling Architecture

### 3.1 Custom Error Type (xf + cass hybrid pattern)

```rust
// src/error/mod.rs

use std::path::PathBuf;
use thiserror::Error;

/// Primary error type for beads_rust operations.
///
/// Design: Structured variants for common cases, with `Other` for
/// wrapped anyhow errors during migration.
#[derive(Error, Debug)]
pub enum BeadsError {
    // === Storage Errors ===
    #[error("Database not found at '{path}'")]
    DatabaseNotFound { path: PathBuf },

    #[error("Database is locked: {path}")]
    DatabaseLocked { path: PathBuf },

    #[error("Schema version mismatch: expected {expected}, found {found}")]
    SchemaMismatch { expected: i32, found: i32 },

    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    // === Issue Errors ===
    #[error("Issue not found: {id}")]
    IssueNotFound { id: String },

    #[error("Issue ID collision: {id}")]
    IdCollision { id: String },

    #[error("Ambiguous ID '{partial}': matches {matches:?}")]
    AmbiguousId { partial: String, matches: Vec<String> },

    #[error("Invalid issue ID format: {id}")]
    InvalidId { id: String },

    // === Validation Errors ===
    #[error("Validation failed: {field}: {reason}")]
    Validation { field: String, reason: String },

    #[error("Invalid status: {status}")]
    InvalidStatus { status: String },

    #[error("Invalid issue type: {issue_type}")]
    InvalidType { issue_type: String },

    #[error("Priority must be 0-4, got: {priority}")]
    InvalidPriority { priority: i32 },

    // === JSONL Errors ===
    #[error("JSONL parse error at line {line}: {reason}")]
    JsonlParse { line: usize, reason: String },

    #[error("Prefix mismatch: expected '{expected}', found '{found}'")]
    PrefixMismatch { expected: String, found: String },

    #[error("Import collision: {count} issues have conflicting content")]
    ImportCollision { count: usize },

    // === Dependency Errors ===
    #[error("Cycle detected in dependencies: {path}")]
    DependencyCycle { path: String },

    #[error("Cannot delete: {id} has {count} dependents")]
    HasDependents { id: String, count: usize },

    // === Configuration Errors ===
    #[error("Configuration error: {0}")]
    Config(String),

    #[error("Beads not initialized: run 'br init' first")]
    NotInitialized,

    // === I/O Errors ===
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    // === Wrapped errors (for gradual migration) ===
    #[error("{context}: {source}")]
    WithContext {
        context: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, BeadsError>;
```

### 3.2 ResultExt Trait (xf pattern)

```rust
// src/error/context.rs

use crate::error::{BeadsError, Result};

/// Extension trait for adding context to errors.
pub trait ResultExt<T> {
    /// Add context string to an error.
    fn context(self, context: impl Into<String>) -> Result<T>;

    /// Add lazily-computed context to an error.
    fn with_context<F>(self, f: F) -> Result<T>
    where
        F: FnOnce() -> String;
}

impl<T, E> ResultExt<T> for std::result::Result<T, E>
where
    E: std::error::Error + Send + Sync + 'static,
{
    fn context(self, context: impl Into<String>) -> Result<T> {
        self.map_err(|e| BeadsError::WithContext {
            context: context.into(),
            source: Box::new(e),
        })
    }

    fn with_context<F>(self, f: F) -> Result<T>
    where
        F: FnOnce() -> String,
    {
        self.map_err(|e| BeadsError::WithContext {
            context: f(),
            source: Box::new(e),
        })
    }
}

// Also implement for Option
impl<T> ResultExt<T> for Option<T> {
    fn context(self, context: impl Into<String>) -> Result<T> {
        self.ok_or_else(|| BeadsError::Other(anyhow::anyhow!(context.into())))
    }

    fn with_context<F>(self, f: F) -> Result<T>
    where
        F: FnOnce() -> String,
    {
        self.ok_or_else(|| BeadsError::Other(anyhow::anyhow!(f())))
    }
}
```

### 3.3 Error Recovery Classification

```rust
// src/error/mod.rs (continued)

impl BeadsError {
    /// Can the user fix this without code changes?
    #[must_use]
    pub const fn is_user_recoverable(&self) -> bool {
        matches!(
            self,
            Self::DatabaseNotFound { .. }
                | Self::NotInitialized
                | Self::IssueNotFound { .. }
                | Self::Validation { .. }
                | Self::InvalidStatus { .. }
                | Self::InvalidType { .. }
                | Self::InvalidPriority { .. }
                | Self::PrefixMismatch { .. }
        )
    }

    /// Should we suggest re-running with --force?
    #[must_use]
    pub const fn suggests_force(&self) -> bool {
        matches!(
            self,
            Self::HasDependents { .. }
                | Self::ImportCollision { .. }
        )
    }

    /// Human-friendly suggestion for fixing this error.
    #[must_use]
    pub fn suggestion(&self) -> Option<&'static str> {
        match self {
            Self::NotInitialized => Some("Run: br init"),
            Self::DatabaseNotFound { .. } => Some("Check path or run: br init"),
            Self::AmbiguousId { .. } => Some("Provide more characters of the ID"),
            Self::HasDependents { .. } => Some("Use --force or --cascade to delete anyway"),
            Self::PrefixMismatch { .. } => Some("Use --rename-on-import to remap IDs"),
            Self::DependencyCycle { .. } => Some("Remove the circular dependency"),
            _ => None,
        }
    }

    /// Exit code for this error (all errors exit 1 per spec).
    #[must_use]
    pub const fn exit_code(&self) -> i32 {
        1 // beads uses exit code 1 for all errors
    }
}
```

---

## 4. CLI Architecture

### 4.1 Top-Level CLI Definition (xf + cass pattern)

```rust
// src/cli/args.rs

use clap::{Parser, Subcommand, ValueEnum, Args};
use std::path::PathBuf;

/// br - Beads issue tracker (Rust port)
///
/// A non-invasive, SQLite + JSONL issue tracking system.
#[derive(Parser, Debug)]
#[command(name = "br")]
#[command(version = concat!(
    env!("CARGO_PKG_VERSION"),
    " (", env!("VERGEN_BUILD_DATE"), ")",
))]
#[command(about = "Beads issue tracker - Rust edition")]
#[command(after_help = r#"
EXAMPLES:
    br init                      Initialize beads in current directory
    br create "Fix login bug"    Create a new task
    br list                      Show open issues
    br ready                     Show issues ready to work on
    br show bd-abc123            Show issue details
    br close bd-abc123           Close an issue
"#)]
pub struct Cli {
    // === Global Options ===

    /// Path to .beads directory (default: auto-discover)
    #[arg(long, env = "BEADS_DIR", global = true)]
    pub beads_dir: Option<PathBuf>,

    /// Output format
    #[arg(long, short = 'f', default_value = "text", global = true)]
    pub format: OutputFormat,

    /// JSON output (shorthand for --format=json)
    #[arg(long, global = true)]
    pub json: bool,

    /// Robot mode: deterministic output for scripts
    #[arg(long, global = true)]
    pub robot: bool,

    /// Verbose output (debug logging)
    #[arg(long, short = 'v', global = true)]
    pub verbose: bool,

    /// Quiet mode (errors only)
    #[arg(long, short = 'q', global = true)]
    pub quiet: bool,

    /// Disable color output
    #[arg(long, global = true)]
    pub no_color: bool,

    /// Identity/actor for operations
    #[arg(long, env = "BR_ACTOR", global = true)]
    pub actor: Option<String>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(ValueEnum, Clone, Debug, Default, PartialEq, Eq)]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
    Jsonl,
    Compact,  // Single-line minimal output
}

#[derive(Subcommand, Debug)]
pub enum Commands {
    /// Initialize beads in current directory
    Init(InitArgs),

    /// Create a new issue
    Create(CreateArgs),

    /// Update an existing issue
    Update(UpdateArgs),

    /// Close one or more issues
    Close(CloseArgs),

    /// Reopen a closed issue
    Reopen(ReopenArgs),

    /// Delete issues (creates tombstones)
    Delete(DeleteArgs),

    /// List issues with filters
    List(ListArgs),

    /// Show issue details
    Show(ShowArgs),

    /// Show issues ready to work on
    Ready(ReadyArgs),

    /// Show blocked issues
    Blocked(BlockedArgs),

    /// Search issues by text
    Search(SearchArgs),

    /// Count issues
    Count(CountArgs),

    /// Show project statistics
    #[command(alias = "stats")]
    Status(StatusArgs),

    /// Dependency management
    #[command(subcommand)]
    Dep(DepCommand),

    /// Label management
    #[command(subcommand)]
    Label(LabelCommand),

    /// Comment management
    #[command(subcommand)]
    Comments(CommentsCommand),

    /// Import from JSONL
    Import(ImportArgs),

    /// Export to JSONL
    Export(ExportArgs),

    /// Sync operations (flush-only, import-only)
    Sync(SyncArgs),

    /// Configuration management
    #[command(subcommand)]
    Config(ConfigCommand),

    /// Show diagnostic information
    Info(InfoArgs),

    /// Run health checks
    Doctor(DoctorArgs),

    /// Quick capture (ID-only output)
    Q(QuickArgs),

    /// Show beads location
    Where(WhereArgs),

    /// Show stale issues
    Stale(StaleArgs),

    /// Defer an issue
    Defer(DeferArgs),

    /// Undefer an issue
    Undefer(UndeferArgs),
}
```

### 4.2 Command-Specific Args (examples)

```rust
// src/cli/args.rs (continued)

#[derive(Args, Debug)]
pub struct CreateArgs {
    /// Issue title (or use --title)
    pub title: Option<String>,

    /// Issue title (alternative to positional)
    #[arg(long, short = 't')]
    pub title_flag: Option<String>,

    /// Issue type
    #[arg(long, short = 'T', default_value = "task")]
    pub issue_type: String,

    /// Priority (0-4 or P0-P4)
    #[arg(long, short = 'p', default_value = "2")]
    pub priority: String,

    /// Description
    #[arg(long, short = 'd')]
    pub description: Option<String>,

    /// Labels (comma-separated or multiple flags)
    #[arg(long, short = 'l', value_delimiter = ',')]
    pub labels: Vec<String>,

    /// Dependencies (type:id or id)
    #[arg(long, value_delimiter = ',')]
    pub deps: Vec<String>,

    /// Parent issue (creates hierarchical ID)
    #[arg(long)]
    pub parent: Option<String>,

    /// Assignee
    #[arg(long, short = 'a')]
    pub assignee: Option<String>,

    /// Explicit ID (overrides auto-generation)
    #[arg(long)]
    pub id: Option<String>,

    /// Force prefix mismatch
    #[arg(long)]
    pub force: bool,

    /// Silent mode (print ID only)
    #[arg(long)]
    pub silent: bool,
}

#[derive(Args, Debug)]
pub struct ListArgs {
    /// Filter by status (can be repeated)
    #[arg(long, short = 's', value_delimiter = ',')]
    pub status: Vec<String>,

    /// Filter by type
    #[arg(long, short = 't', value_delimiter = ',')]
    pub issue_type: Vec<String>,

    /// Filter by assignee
    #[arg(long, short = 'a')]
    pub assignee: Option<String>,

    /// Filter by label (AND logic)
    #[arg(long, short = 'l', value_delimiter = ',')]
    pub label: Vec<String>,

    /// Filter by label (OR logic)
    #[arg(long)]
    pub label_any: Vec<String>,

    /// Include all statuses (including closed)
    #[arg(long)]
    pub all: bool,

    /// Show only ready issues (no blockers)
    #[arg(long)]
    pub ready: bool,

    /// Maximum results
    #[arg(long, short = 'n', default_value = "50")]
    pub limit: usize,

    /// Sort field
    #[arg(long)]
    pub sort: Option<SortField>,

    /// Reverse sort order
    #[arg(long)]
    pub reverse: bool,

    /// Pretty/tree output
    #[arg(long, alias = "tree")]
    pub pretty: bool,

    /// Long format (multi-line)
    #[arg(long)]
    pub long: bool,
}

#[derive(ValueEnum, Clone, Debug)]
pub enum SortField {
    Priority,
    Created,
    Updated,
    Status,
    Id,
    Title,
    Type,
    Assignee,
}

#[derive(Subcommand, Debug)]
pub enum DepCommand {
    /// Add a dependency
    Add(DepAddArgs),

    /// Remove a dependency
    Remove(DepRemoveArgs),

    /// List dependencies
    List(DepListArgs),

    /// Show dependency tree
    Tree(DepTreeArgs),

    /// Detect dependency cycles
    Cycles(DepCyclesArgs),
}

#[derive(Args, Debug)]
pub struct DepAddArgs {
    /// Issue that depends on another
    pub issue: String,

    /// Issue being depended on
    pub depends_on: String,

    /// Dependency type (default: blocks)
    #[arg(long, short = 't', default_value = "blocks")]
    pub dep_type: String,
}

#[derive(Args, Debug)]
pub struct DepTreeArgs {
    /// Root issue for tree
    pub issue: String,

    /// Tree direction
    #[arg(long, short = 'd', default_value = "down")]
    pub direction: TreeDirection,

    /// Maximum depth
    #[arg(long, default_value = "50")]
    pub max_depth: usize,

    /// Show all paths (including duplicates)
    #[arg(long)]
    pub show_all_paths: bool,

    /// Output format (text, mermaid)
    #[arg(long)]
    pub tree_format: Option<TreeFormat>,
}

#[derive(ValueEnum, Clone, Debug, Default)]
pub enum TreeDirection {
    #[default]
    Down,
    Up,
    Both,
}

#[derive(ValueEnum, Clone, Debug)]
pub enum TreeFormat {
    Text,
    Mermaid,
    Dot,
}
```

### 4.3 Output Handling (cass robot pattern)

```rust
// src/cli/output.rs

use serde::Serialize;
use crate::error::Result;

/// Unified output handler supporting multiple formats.
pub struct OutputHandler {
    format: OutputFormat,
    robot: bool,
    quiet: bool,
    color: bool,
}

impl OutputHandler {
    pub fn new(cli: &Cli) -> Self {
        let format = if cli.json {
            OutputFormat::Json
        } else {
            cli.format.clone()
        };

        let color = !cli.no_color && std::env::var("NO_COLOR").is_err();

        Self {
            format,
            robot: cli.robot,
            quiet: cli.quiet,
            color,
        }
    }

    /// Output a single value.
    pub fn output<T: Serialize + TextFormat>(&self, value: &T) -> Result<()> {
        match self.format {
            OutputFormat::Json => {
                let json = serde_json::to_string_pretty(value)?;
                println!("{json}");
            }
            OutputFormat::Jsonl => {
                let json = serde_json::to_string(value)?;
                println!("{json}");
            }
            OutputFormat::Compact => {
                let json = serde_json::to_string(value)?;
                println!("{json}");
            }
            OutputFormat::Text => {
                let text = if self.robot {
                    value.to_robot_text()
                } else {
                    value.to_human_text(self.color)
                };
                println!("{text}");
            }
        }
        Ok(())
    }

    /// Output a list of values.
    pub fn output_list<T: Serialize + TextFormat>(&self, values: &[T]) -> Result<()> {
        match self.format {
            OutputFormat::Json => {
                let json = serde_json::to_string_pretty(values)?;
                println!("{json}");
            }
            OutputFormat::Jsonl => {
                for value in values {
                    let json = serde_json::to_string(value)?;
                    println!("{json}");
                }
            }
            OutputFormat::Compact => {
                let json = serde_json::to_string(values)?;
                println!("{json}");
            }
            OutputFormat::Text => {
                for value in values {
                    let text = if self.robot {
                        value.to_robot_text()
                    } else {
                        value.to_human_text(self.color)
                    };
                    println!("{text}");
                }
            }
        }
        Ok(())
    }

    /// Output an error (always to stderr).
    pub fn error(&self, err: &crate::error::BeadsError) {
        if self.format.is_json() {
            let json = serde_json::json!({
                "error": err.to_string(),
                "suggestion": err.suggestion(),
            });
            eprintln!("{}", serde_json::to_string_pretty(&json).unwrap());
        } else {
            eprintln!("Error: {err}");
            if let Some(suggestion) = err.suggestion() {
                eprintln!("Hint: {suggestion}");
            }
        }
    }

    /// Info message (skipped in quiet mode).
    pub fn info(&self, msg: impl std::fmt::Display) {
        if !self.quiet {
            eprintln!("{msg}");
        }
    }
}

/// Trait for types that can format themselves as text.
pub trait TextFormat {
    fn to_human_text(&self, color: bool) -> String;
    fn to_robot_text(&self) -> String;
}

impl OutputFormat {
    #[must_use]
    pub const fn is_json(&self) -> bool {
        matches!(self, Self::Json | Self::Jsonl | Self::Compact)
    }
}
```

### 4.4 Application Context (shared state)

```rust
// src/context.rs

use crate::config::Config;
use crate::cli::OutputFormat;
use crate::storage::Storage;
use std::path::PathBuf;
use std::sync::Arc;

/// Shared application context passed to all commands.
///
/// Contains the resolved configuration, output preferences, actor identity,
/// and storage reference. This avoids threading individual parameters through
/// the entire call stack.
pub struct Context {
    /// Resolved configuration (CLI → env → file → defaults)
    pub config: Config,

    /// Path to the .beads directory
    pub beads_dir: PathBuf,

    /// Output format (text, json, jsonl, compact)
    pub output: OutputFormat,

    /// Whether we're in quiet mode
    pub quiet: bool,

    /// Verbosity level (0 = normal, 1+ = verbose)
    pub verbosity: u8,

    /// Actor performing the operation (for audit events)
    pub actor: String,

    /// Storage backend (lazily initialized)
    pub storage: Option<Arc<dyn Storage>>,
}

impl Context {
    /// Create a new context from CLI arguments and environment.
    pub fn from_cli(cli: &crate::cli::Cli) -> crate::error::Result<Self> {
        let config = crate::config::load_config(cli.beads_dir.as_deref())?;
        let beads_dir = cli.beads_dir.clone()
            .or_else(|| crate::util::path::discover_beads_dir())
            .unwrap_or_else(|| PathBuf::from(".beads"));

        let actor = std::env::var("BEADS_ACTOR")
            .or_else(|_| std::env::var("USER"))
            .unwrap_or_else(|_| "unknown".to_string());

        Ok(Self {
            config,
            beads_dir,
            output: cli.output.unwrap_or(OutputFormat::Text),
            quiet: cli.quiet,
            verbosity: cli.verbose,
            actor,
            storage: None,
        })
    }

    /// Get or initialize the storage backend.
    pub fn storage(&mut self) -> crate::error::Result<&dyn Storage> {
        if self.storage.is_none() {
            let db_path = self.beads_dir.join("beads.db");
            let storage = crate::storage::SqliteStorage::open(&db_path)?;
            self.storage = Some(Arc::new(storage));
        }
        Ok(self.storage.as_ref().unwrap().as_ref())
    }

    /// Check if we should use JSON output.
    #[must_use]
    pub const fn is_robot(&self) -> bool {
        matches!(self.output, OutputFormat::Json | OutputFormat::Jsonl | OutputFormat::Compact)
    }
}
```

### 4.5 Logging Configuration (xf + cass pattern)

```rust
// src/logging.rs

use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};
use std::path::PathBuf;
use std::fs::File;

/// Logging configuration.
pub struct LogConfig {
    /// Enable quiet mode (errors only)
    pub quiet: bool,
    /// Verbosity level (0 = info, 1 = debug, 2+ = trace)
    pub verbose: u8,
    /// Optional trace file for JSONL span logs (cass pattern)
    pub trace_file: Option<PathBuf>,
}

impl LogConfig {
    /// Initialize tracing subscriber based on configuration.
    pub fn init(self) -> anyhow::Result<()> {
        let filter = self.build_filter();

        if let Some(trace_path) = self.trace_file {
            // Write JSON spans to trace file for debugging
            let trace_file = File::create(&trace_path)?;
            let json_layer = fmt::layer()
                .json()
                .with_writer(trace_file)
                .with_span_events(fmt::format::FmtSpan::CLOSE);

            tracing_subscriber::registry()
                .with(filter)
                .with(json_layer)
                .init();
        } else {
            // Standard stderr logging
            let stderr_layer = fmt::layer()
                .with_target(false)
                .with_ansi(atty::is(atty::Stream::Stderr));

            tracing_subscriber::registry()
                .with(filter)
                .with(stderr_layer)
                .init();
        }

        Ok(())
    }

    fn build_filter(&self) -> EnvFilter {
        let level = if self.quiet {
            "error"
        } else {
            match self.verbose {
                0 => "warn,br=info",
                1 => "info,br=debug",
                _ => "debug,br=trace",
            }
        };

        EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new(level))
    }
}
```

### 4.6 Robot-Friendly Help (cass pattern)

```rust
// Additional CLI flag for machine-readable help

/// Robot help output for AI assistants.
#[derive(Parser, Debug)]
pub struct RobotHelp {
    /// Print command schema as JSON (for AI parsing)
    #[arg(long = "robot-help", hide = true)]
    pub robot_help: bool,
}

impl Cli {
    /// Generate machine-readable help as JSON.
    pub fn robot_help_json() -> serde_json::Value {
        serde_json::json!({
            "name": "br",
            "version": env!("CARGO_PKG_VERSION"),
            "commands": [
                {"name": "init", "description": "Initialize .beads in current directory"},
                {"name": "create", "description": "Create a new issue"},
                {"name": "list", "description": "List issues"},
                {"name": "show", "description": "Show issue details"},
                {"name": "update", "description": "Update an issue"},
                {"name": "close", "description": "Close an issue"},
                {"name": "ready", "description": "Show issues ready to work on"},
                {"name": "blocked", "description": "Show blocked issues"},
                {"name": "dep", "description": "Manage dependencies"},
                {"name": "label", "description": "Manage labels"},
                {"name": "search", "description": "Search issues"},
                {"name": "sync", "description": "Sync SQLite with JSONL"},
                {"name": "export", "description": "Export to JSONL"},
                {"name": "import", "description": "Import from JSONL"},
                {"name": "doctor", "description": "Run diagnostics"},
                {"name": "config", "description": "View/edit configuration"},
            ],
            "global_flags": [
                {"flag": "--json", "description": "Output as JSON"},
                {"flag": "--robot", "description": "Alias for --json"},
                {"flag": "--quiet", "description": "Suppress non-essential output"},
                {"flag": "--verbose", "description": "Increase verbosity"},
                {"flag": "--beads-dir", "description": "Path to .beads directory"},
                {"flag": "--trace-file", "description": "Write JSONL spans for debugging"},
            ]
        })
    }
}
```

---

## 5. Storage Architecture

### 5.1 Storage Trait (pluggable design)

```rust
// src/storage/mod.rs

use crate::error::Result;
use crate::model::{Issue, Dependency, Comment, Event, DependencyType, Status};
use std::path::Path;

/// Storage abstraction for beads persistence.
///
/// This trait enables:
/// - SQLite implementation (primary)
/// - In-memory implementation (testing)
/// - Potential future backends
pub trait Storage: Send + Sync {
    // === Issue CRUD ===
    fn create_issue(&mut self, issue: &Issue) -> Result<()>;
    fn get_issue(&self, id: &str) -> Result<Option<Issue>>;
    fn update_issue(&mut self, issue: &Issue) -> Result<()>;
    fn delete_issue(&mut self, id: &str) -> Result<()>;

    // === Search & List ===
    fn search_issues(&self, filter: &SearchFilter) -> Result<Vec<Issue>>;
    fn list_issues(&self, filter: &ListFilter) -> Result<Vec<IssueWithCounts>>;
    fn get_ready_issues(&self, limit: usize) -> Result<Vec<Issue>>;
    fn get_blocked_issues(&self) -> Result<Vec<BlockedIssue>>;

    // === Dependencies ===
    fn add_dependency(&mut self, dep: &Dependency) -> Result<()>;
    fn remove_dependency(&mut self, issue_id: &str, depends_on: &str) -> Result<()>;
    fn get_dependencies(&self, issue_id: &str) -> Result<Vec<Dependency>>;
    fn get_dependents(&self, issue_id: &str) -> Result<Vec<Dependency>>;
    fn get_dependency_tree(
        &self,
        root: &str,
        direction: TreeDirection,
        max_depth: usize,
    ) -> Result<Vec<TreeNode>>;
    fn detect_cycles(&self) -> Result<Vec<Vec<String>>>;

    // === Labels ===
    fn add_label(&mut self, issue_id: &str, label: &str) -> Result<()>;
    fn remove_label(&mut self, issue_id: &str, label: &str) -> Result<()>;
    fn get_labels(&self, issue_id: &str) -> Result<Vec<String>>;
    fn get_all_labels(&self) -> Result<Vec<LabelCount>>;

    // === Comments ===
    fn add_comment(&mut self, comment: &Comment) -> Result<()>;
    fn get_comments(&self, issue_id: &str) -> Result<Vec<Comment>>;

    // === Events (Audit) ===
    fn add_event(&mut self, event: &Event) -> Result<()>;
    fn get_events(&self, issue_id: &str, limit: usize) -> Result<Vec<Event>>;

    // === ID Generation ===
    fn resolve_partial_id(&self, partial: &str) -> Result<String>;
    fn get_next_child_id(&self, parent: &str) -> Result<String>;
    fn id_exists(&self, id: &str) -> Result<bool>;

    // === Configuration ===
    fn get_config(&self, key: &str) -> Result<Option<String>>;
    fn set_config(&mut self, key: &str, value: &str) -> Result<()>;
    fn get_prefix(&self) -> Result<String>;

    // === Metadata ===
    fn get_metadata(&self, key: &str) -> Result<Option<String>>;
    fn set_metadata(&mut self, key: &str, value: &str) -> Result<()>;

    // === Dirty Tracking ===
    fn mark_dirty(&mut self, issue_id: &str) -> Result<()>;
    fn get_dirty_issues(&self) -> Result<Vec<String>>;
    fn clear_dirty(&mut self, issue_ids: &[String]) -> Result<()>;

    // === Blocked Cache ===
    fn rebuild_blocked_cache(&mut self) -> Result<()>;
    fn is_blocked(&self, issue_id: &str) -> Result<bool>;

    // === Bulk Operations ===
    fn count_issues(&self, filter: &CountFilter) -> Result<CountResult>;
    fn get_issues_batch(&self, ids: &[String]) -> Result<Vec<Issue>>;
    fn get_labels_batch(&self, ids: &[String]) -> Result<std::collections::HashMap<String, Vec<String>>>;

    // === Maintenance ===
    fn vacuum(&mut self) -> Result<()>;
    fn checkpoint(&mut self) -> Result<()>;
    fn backup(&self, path: &Path) -> Result<()>;
}

/// Factory for creating storage instances.
pub fn open_storage(beads_dir: &Path) -> Result<SqliteStorage> {
    let db_path = beads_dir.join("beads.db");
    SqliteStorage::open(&db_path)
}

pub fn open_memory_storage() -> Result<SqliteStorage> {
    SqliteStorage::open_memory()
}
```

### 5.2 SQLite Implementation (xf + cass patterns)

```rust
// src/storage/sqlite.rs

use crate::error::{BeadsError, Result, ResultExt};
use crate::storage::Storage;
use rusqlite::{Connection, OpenFlags, params};
use std::path::Path;
use parking_lot::Mutex;

/// SQLite-based storage implementation.
pub struct SqliteStorage {
    conn: Mutex<Connection>,
}

impl SqliteStorage {
    /// Open or create a database at the given path.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| BeadsError::Database(e))?;

        Self::configure_connection(&conn)?;

        let storage = Self {
            conn: Mutex::new(conn),
        };
        storage.migrate()?;

        Ok(storage)
    }

    /// Open an in-memory database (for testing).
    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()
            .map_err(|e| BeadsError::Database(e))?;

        Self::configure_connection(&conn)?;

        let storage = Self {
            conn: Mutex::new(conn),
        };
        storage.migrate()?;

        Ok(storage)
    }

    /// Configure SQLite pragmas for optimal performance.
    fn configure_connection(conn: &Connection) -> Result<()> {
        // WAL mode for concurrent reads during writes
        conn.pragma_update(None, "journal_mode", "WAL")?;

        // Balance between safety and performance
        conn.pragma_update(None, "synchronous", "NORMAL")?;

        // Enforce referential integrity
        conn.pragma_update(None, "foreign_keys", "ON")?;

        // 30 second timeout on lock contention
        conn.pragma_update(None, "busy_timeout", 30000)?;

        // 64MB cache for better read performance
        conn.pragma_update(None, "cache_size", -65536)?;

        // Keep temp tables in memory
        conn.pragma_update(None, "temp_store", "MEMORY")?;

        // Enable memory-mapped I/O (256MB)
        conn.pragma_update(None, "mmap_size", 268435456)?;

        Ok(())
    }

    /// Run schema migrations.
    fn migrate(&self) -> Result<()> {
        let conn = self.conn.lock();

        // Check current schema version
        let version: i32 = conn
            .query_row(
                "SELECT COALESCE(
                    (SELECT value FROM metadata WHERE key = 'schema_version'),
                    '0'
                ) AS version",
                [],
                |row| row.get::<_, String>(0),
            )
            .map(|v| v.parse().unwrap_or(0))
            .unwrap_or(0);

        if version < SCHEMA_VERSION {
            self.apply_migrations(&conn, version)?;
        }

        Ok(())
    }

    /// Apply schema migrations from version to current.
    fn apply_migrations(&self, conn: &Connection, from_version: i32) -> Result<()> {
        let tx = conn.unchecked_transaction()?;

        for version in (from_version + 1)..=SCHEMA_VERSION {
            match version {
                1 => self.migration_v1(&tx)?,
                2 => self.migration_v2(&tx)?,
                3 => self.migration_v3(&tx)?,
                _ => {}
            }
        }

        tx.execute(
            "INSERT OR REPLACE INTO metadata (key, value) VALUES ('schema_version', ?)",
            params![SCHEMA_VERSION.to_string()],
        )?;

        tx.commit()?;
        Ok(())
    }

    /// Backup using VACUUM INTO (cass pattern - handles WAL properly).
    pub fn backup(&self, path: &Path) -> Result<()> {
        let conn = self.conn.lock();
        let path_str = path.to_string_lossy();

        // Try VACUUM INTO first (SQLite 3.27+, handles WAL)
        match conn.execute(&format!("VACUUM INTO '{path_str}'"), []) {
            Ok(_) => Ok(()),
            Err(_) => {
                // Fallback to backup API
                drop(conn);
                self.backup_fallback(path)
            }
        }
    }

    fn backup_fallback(&self, path: &Path) -> Result<()> {
        let conn = self.conn.lock();
        let mut dest = Connection::open(path)?;
        let backup = rusqlite::backup::Backup::new(&conn, &mut dest)?;
        backup.run_to_completion(100, std::time::Duration::from_millis(10), None)?;
        Ok(())
    }
}

const SCHEMA_VERSION: i32 = 3;
```

### 5.3 Transaction Discipline (Critical)

Every mutation operation **must** follow this 4-step transaction protocol:

```rust
// src/storage/sqlite.rs

impl SqliteStorage {
    /// Execute a mutation with proper transaction discipline.
    ///
    /// INVARIANT: Every mutation follows this exact sequence within a single transaction:
    /// 1. Apply the change (INSERT/UPDATE/DELETE)
    /// 2. Write an event row to the events table (audit log)
    /// 3. Mark the affected issue(s) as dirty (for JSONL export)
    /// 4. Invalidate blocked_issues_cache if dependencies/status changed
    pub fn with_mutation<F, T>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Transaction) -> Result<T>,
    {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;

        let result = f(&tx)?;

        tx.commit()?;
        Ok(result)
    }

    /// Example: Update an issue with full transaction discipline
    pub fn update_issue(&self, id: &str, updates: &IssueUpdate, actor: &str) -> Result<Issue> {
        self.with_mutation(|tx| {
            // 1. Apply the change
            let sql = build_update_sql(updates);
            tx.execute(&sql, params![id])?;

            // 2. Write audit event
            tx.execute(
                "INSERT INTO events (issue_id, event_type, actor, created_at, data)
                 VALUES (?, 'updated', ?, datetime('now'), ?)",
                params![id, actor, serde_json::to_string(updates)?],
            )?;

            // 3. Mark as dirty
            tx.execute(
                "INSERT OR REPLACE INTO dirty_issues (id, dirty_since)
                 VALUES (?, datetime('now'))",
                params![id],
            )?;

            // 4. Invalidate cache if status changed
            if updates.status.is_some() {
                tx.execute(
                    "DELETE FROM blocked_issues_cache WHERE issue_id = ? OR blocking_id = ?",
                    params![id, id],
                )?;
            }

            // Return updated issue
            self.get_issue_in_tx(tx, id)
        })
    }
}
```

**Why this matters:**

| Step | Purpose | Consequence if skipped |
|------|---------|----------------------|
| Apply change | Core mutation | Data inconsistency |
| Write event | Audit trail | Lost history, compliance issues |
| Mark dirty | JSONL sync | Export misses changes |
| Invalidate cache | Ready/blocked correctness | Stale blocked_issues_cache |

### 5.4 Schema Definition

```rust
// src/storage/schema.rs

pub const SCHEMA_V1: &str = r#"
-- Core issues table
CREATE TABLE IF NOT EXISTS issues (
    id TEXT PRIMARY KEY NOT NULL,
    title TEXT NOT NULL CHECK(length(title) <= 500 AND length(title) > 0),
    description TEXT,
    design TEXT,
    acceptance_criteria TEXT,
    notes TEXT,
    status TEXT NOT NULL DEFAULT 'open',
    priority INTEGER NOT NULL DEFAULT 2 CHECK(priority >= 0 AND priority <= 4),
    issue_type TEXT NOT NULL DEFAULT 'task',
    assignee TEXT,
    owner TEXT,
    estimated_minutes INTEGER,
    external_ref TEXT,
    created_at TEXT NOT NULL,
    created_by TEXT,
    updated_at TEXT NOT NULL,
    closed_at TEXT,
    close_reason TEXT,
    closed_by_session TEXT,
    deleted_at TEXT,
    deleted_by TEXT,
    delete_reason TEXT,
    original_type TEXT,
    due_at TEXT,
    defer_until TEXT,
    content_hash TEXT NOT NULL,
    is_template INTEGER DEFAULT 0,
    pinned INTEGER DEFAULT 0
);

-- Dependencies between issues
CREATE TABLE IF NOT EXISTS dependencies (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    issue_id TEXT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    depends_on_id TEXT NOT NULL,  -- No FK: allows external:* refs
    dependency_type TEXT NOT NULL DEFAULT 'blocks',
    created_at TEXT NOT NULL,
    created_by TEXT,
    metadata TEXT,
    thread_id TEXT,
    UNIQUE(issue_id, depends_on_id)
);

-- Labels on issues
CREATE TABLE IF NOT EXISTS labels (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    issue_id TEXT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    label TEXT NOT NULL,
    UNIQUE(issue_id, label)
);

-- Comments on issues
CREATE TABLE IF NOT EXISTS comments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    issue_id TEXT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    author TEXT,
    text TEXT NOT NULL,
    created_at TEXT NOT NULL
);

-- Audit events
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    issue_id TEXT NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    event_type TEXT NOT NULL,
    actor TEXT,
    old_value TEXT,
    new_value TEXT,
    comment TEXT,
    created_at TEXT NOT NULL
);

-- Configuration
CREATE TABLE IF NOT EXISTS config (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
);

-- Metadata (schema version, hashes, etc.)
CREATE TABLE IF NOT EXISTS metadata (
    key TEXT PRIMARY KEY NOT NULL,
    value TEXT NOT NULL
);

-- Dirty tracking for incremental export
CREATE TABLE IF NOT EXISTS dirty_issues (
    issue_id TEXT PRIMARY KEY NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    marked_at TEXT NOT NULL
);

-- Export hashes for staleness detection
CREATE TABLE IF NOT EXISTS export_hashes (
    issue_id TEXT PRIMARY KEY NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    content_hash TEXT NOT NULL
);

-- Blocked issues cache (materialized view)
CREATE TABLE IF NOT EXISTS blocked_issues_cache (
    issue_id TEXT PRIMARY KEY NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
    blocked_by_count INTEGER NOT NULL DEFAULT 0,
    blocked_by_ids TEXT  -- Comma-separated list
);

-- Child ID counters for hierarchical IDs
CREATE TABLE IF NOT EXISTS child_counters (
    parent_id TEXT PRIMARY KEY NOT NULL,
    next_child INTEGER NOT NULL DEFAULT 1
);

-- Indexes for common queries
CREATE INDEX IF NOT EXISTS idx_issues_status ON issues(status);
CREATE INDEX IF NOT EXISTS idx_issues_priority ON issues(priority);
CREATE INDEX IF NOT EXISTS idx_issues_assignee ON issues(assignee);
CREATE INDEX IF NOT EXISTS idx_issues_created_at ON issues(created_at);
CREATE INDEX IF NOT EXISTS idx_issues_updated_at ON issues(updated_at);
CREATE INDEX IF NOT EXISTS idx_issues_type ON issues(issue_type);

CREATE INDEX IF NOT EXISTS idx_deps_issue ON dependencies(issue_id);
CREATE INDEX IF NOT EXISTS idx_deps_depends_on ON dependencies(depends_on_id);
CREATE INDEX IF NOT EXISTS idx_deps_type ON dependencies(dependency_type);

CREATE INDEX IF NOT EXISTS idx_labels_issue ON labels(issue_id);
CREATE INDEX IF NOT EXISTS idx_labels_label ON labels(label);

CREATE INDEX IF NOT EXISTS idx_comments_issue ON comments(issue_id);

CREATE INDEX IF NOT EXISTS idx_events_issue ON events(issue_id);
CREATE INDEX IF NOT EXISTS idx_events_created_at ON events(created_at);

CREATE INDEX IF NOT EXISTS idx_dirty_marked ON dirty_issues(marked_at);
"#;
```

### 5.4 Batch Operations (xf pattern)

```rust
// src/storage/batch.rs

use rusqlite::{Connection, Statement};
use crate::error::Result;

/// Maximum variables in a single SQLite statement.
/// SQLite default is 999, but we use 900 for safety margin.
const SQLITE_BATCH_SIZE: usize = 900;

/// Execute a batch insert with chunking.
pub fn batch_insert<T, F>(
    conn: &Connection,
    items: &[T],
    sql_template: &str,
    params_per_item: usize,
    bind_fn: F,
) -> Result<usize>
where
    F: Fn(&T, &mut Statement, usize) -> Result<()>,
{
    let chunk_size = SQLITE_BATCH_SIZE / params_per_item;
    let mut total = 0;

    for chunk in items.chunks(chunk_size) {
        let placeholders: Vec<String> = chunk
            .iter()
            .map(|_| format!("({})", vec!["?"; params_per_item].join(",")))
            .collect();

        let sql = sql_template.replace("{}", &placeholders.join(","));
        let mut stmt = conn.prepare_cached(&sql)?;

        for (idx, item) in chunk.iter().enumerate() {
            bind_fn(item, &mut stmt, idx * params_per_item)?;
        }

        total += stmt.execute([])?;
    }

    Ok(total)
}

/// Fetch items by IDs in batches.
pub fn batch_fetch<T, F>(
    conn: &Connection,
    ids: &[String],
    sql_template: &str,
    map_fn: F,
) -> Result<Vec<T>>
where
    F: Fn(&rusqlite::Row) -> Result<T>,
{
    let mut results = Vec::with_capacity(ids.len());

    for chunk in ids.chunks(SQLITE_BATCH_SIZE) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let sql = sql_template.replace("{}", &placeholders);
        let mut stmt = conn.prepare_cached(&sql)?;

        let rows = stmt.query_map(
            rusqlite::params_from_iter(chunk.iter()),
            |row| Ok(map_fn(row)),
        )?;

        for row in rows {
            results.push(row??);
        }
    }

    Ok(results)
}
```

---

## 6. Model Layer

### 6.0 Model Invariants (Strict Compatibility)

These invariants **must** be enforced at both validation and storage levels:

| Field | Constraint | Error if Violated |
|-------|-----------|-------------------|
| `title` | Length 1..500 | `Validation { field: "title", reason }` |
| `priority` | Integer 0..4 | `InvalidPriority { priority }` |
| `status=closed` | `closed_at` must be non-null | `Validation { field: "closed_at" }` |
| `status!=closed` | `closed_at` should be null (except tombstone) | Warning only |
| `status=tombstone` | `deleted_at` must be non-null | `Validation { field: "deleted_at" }` |
| `estimated_minutes` | Non-negative if present | `Validation { field: "estimated_minutes" }` |
| `id` | Must match `<prefix>-<hash>` format | `InvalidId { id }` |

**Hashing Rules:**

- Include only **content fields** in the hash: `title`, `description`, `design`, `acceptance_criteria`, `notes`, `status`, `priority`, `issue_type`, `assignee`, `external_ref`
- Exclude from hash: `labels`, `dependencies`, `comments`, `events`, timestamps, tombstone fields
- Hash must be **deterministic**: same inputs → same hash
- Use SHA-256, truncate to 16 bytes, base64 encode

**ID Rules:**

- Format: `<prefix>-<short_hash>` (e.g., `bd-abc123`)
- Prefix normalization: no trailing `-` stored
- IDs always contain exactly one `-` between prefix and hash
- Short hash: first 6 characters of base64-encoded content hash

### 6.1 Issue Struct

```rust
// src/model/issue.rs

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use crate::model::{Status, IssueType};
use crate::error::{BeadsError, Result};

/// Core issue data structure.
///
/// Field ordering matches legacy bd for JSONL compatibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub id: String,
    pub title: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub design: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub acceptance_criteria: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,

    pub status: Status,
    pub priority: i32,
    pub issue_type: IssueType,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub estimated_minutes: Option<i32>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_ref: Option<String>,

    pub created_at: DateTime<Utc>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,

    pub updated_at: DateTime<Utc>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<DateTime<Utc>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub close_reason: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed_by_session: Option<String>,

    // Tombstone fields
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted_at: Option<DateTime<Utc>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted_by: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete_reason: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub original_type: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub due_at: Option<DateTime<Utc>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub defer_until: Option<DateTime<Utc>>,

    // Computed/internal fields (not exported to JSONL)
    #[serde(skip)]
    pub content_hash: String,

    #[serde(skip_serializing_if = "is_false")]
    pub is_template: bool,

    #[serde(skip_serializing_if = "is_false")]
    pub pinned: bool,

    // Relations (populated on load)
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub dependencies: Vec<Dependency>,

    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub labels: Vec<String>,

    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub comments: Vec<Comment>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl Issue {
    /// Create a new issue with defaults.
    pub fn new(id: String, title: String) -> Self {
        let now = Utc::now();
        Self {
            id,
            title,
            description: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: Status::Open,
            priority: 2,
            issue_type: IssueType::Task,
            assignee: None,
            owner: None,
            estimated_minutes: None,
            external_ref: None,
            created_at: now,
            created_by: None,
            updated_at: now,
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            deleted_at: None,
            deleted_by: None,
            delete_reason: None,
            original_type: None,
            due_at: None,
            defer_until: None,
            content_hash: String::new(),
            is_template: false,
            pinned: false,
            dependencies: Vec::new(),
            labels: Vec::new(),
            comments: Vec::new(),
        }
    }

    /// Validate issue fields.
    pub fn validate(&self) -> Result<()> {
        // Title validation
        if self.title.is_empty() {
            return Err(BeadsError::Validation {
                field: "title".into(),
                reason: "cannot be empty".into(),
            });
        }
        if self.title.len() > 500 {
            return Err(BeadsError::Validation {
                field: "title".into(),
                reason: format!("exceeds 500 characters ({})", self.title.len()),
            });
        }

        // Priority validation
        if !(0..=4).contains(&self.priority) {
            return Err(BeadsError::InvalidPriority {
                priority: self.priority,
            });
        }

        // Status invariants
        if self.status == Status::Closed && self.closed_at.is_none() {
            return Err(BeadsError::Validation {
                field: "closed_at".into(),
                reason: "must be set when status is closed".into(),
            });
        }
        if self.status == Status::Tombstone && self.deleted_at.is_none() {
            return Err(BeadsError::Validation {
                field: "deleted_at".into(),
                reason: "must be set when status is tombstone".into(),
            });
        }

        // Estimated minutes validation
        if let Some(est) = self.estimated_minutes {
            if est < 0 {
                return Err(BeadsError::Validation {
                    field: "estimated_minutes".into(),
                    reason: "cannot be negative".into(),
                });
            }
        }

        Ok(())
    }

    /// Compute content hash from relevant fields.
    pub fn compute_content_hash(&self) -> String {
        use sha2::{Sha256, Digest};

        let mut hasher = Sha256::new();
        hasher.update(&self.title);
        hasher.update(self.description.as_deref().unwrap_or(""));
        hasher.update(self.design.as_deref().unwrap_or(""));
        hasher.update(self.acceptance_criteria.as_deref().unwrap_or(""));
        hasher.update(self.notes.as_deref().unwrap_or(""));
        hasher.update(self.status.as_str());
        hasher.update(self.priority.to_string());
        hasher.update(self.issue_type.as_str());
        hasher.update(self.assignee.as_deref().unwrap_or(""));
        hasher.update(self.external_ref.as_deref().unwrap_or(""));

        let hash = hasher.finalize();
        base64::encode(&hash[..16])
    }

    /// Check if this issue is ready (no blocking dependencies).
    #[must_use]
    pub fn is_ready(&self) -> bool {
        self.status == Status::Open
            && self.dependencies.iter().all(|d| !d.is_blocking())
    }

    /// Check if this is a tombstone.
    #[must_use]
    pub const fn is_tombstone(&self) -> bool {
        matches!(self.status, Status::Tombstone)
    }

    /// Check if this is an ephemeral/wisp issue.
    #[must_use]
    pub fn is_ephemeral(&self) -> bool {
        self.id.contains("-wisp-")
    }
}
```

### 6.2 Status and Type Enums

```rust
// src/model/status.rs

use serde::{Deserialize, Serialize};
use std::str::FromStr;
use crate::error::{BeadsError, Result};

/// Issue status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[default]
    Open,
    InProgress,
    Blocked,
    Deferred,
    Closed,
    Tombstone,
    Pinned,
}

impl Status {
    /// All valid status values (classic subset).
    pub const ALL: &'static [Status] = &[
        Status::Open,
        Status::InProgress,
        Status::Blocked,
        Status::Deferred,
        Status::Closed,
        Status::Tombstone,
        Status::Pinned,
    ];

    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::InProgress => "in_progress",
            Self::Blocked => "blocked",
            Self::Deferred => "deferred",
            Self::Closed => "closed",
            Self::Tombstone => "tombstone",
            Self::Pinned => "pinned",
        }
    }

    /// Status icon for human output.
    #[must_use]
    pub const fn icon(&self) -> &'static str {
        match self {
            Self::Open => "○",
            Self::InProgress => "◐",
            Self::Blocked => "●",
            Self::Deferred => "❄",
            Self::Closed => "✓",
            Self::Tombstone => "✗",
            Self::Pinned => "📌",
        }
    }

    /// Is this a terminal status?
    #[must_use]
    pub const fn is_terminal(&self) -> bool {
        matches!(self, Self::Closed | Self::Tombstone)
    }
}

impl FromStr for Status {
    type Err = BeadsError;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "open" => Ok(Self::Open),
            "in_progress" | "inprogress" | "in-progress" => Ok(Self::InProgress),
            "blocked" => Ok(Self::Blocked),
            "deferred" => Ok(Self::Deferred),
            "closed" => Ok(Self::Closed),
            "tombstone" => Ok(Self::Tombstone),
            "pinned" => Ok(Self::Pinned),
            _ => Err(BeadsError::InvalidStatus { status: s.into() }),
        }
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}
```

```rust
// src/model/issue_type.rs

use serde::{Deserialize, Serialize};
use std::str::FromStr;
use crate::error::{BeadsError, Result};

/// Issue type (classic subset only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum IssueType {
    #[default]
    Task,
    Bug,
    Feature,
    Epic,
    Chore,
    Docs,
    Question,
}

impl IssueType {
    pub const ALL: &'static [IssueType] = &[
        IssueType::Task,
        IssueType::Bug,
        IssueType::Feature,
        IssueType::Epic,
        IssueType::Chore,
        IssueType::Docs,
        IssueType::Question,
    ];

    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Bug => "bug",
            Self::Feature => "feature",
            Self::Epic => "epic",
            Self::Chore => "chore",
            Self::Docs => "docs",
            Self::Question => "question",
        }
    }
}

impl FromStr for IssueType {
    type Err = BeadsError;

    fn from_str(s: &str) -> Result<Self> {
        match s.to_lowercase().as_str() {
            "task" => Ok(Self::Task),
            "bug" => Ok(Self::Bug),
            "feature" | "feat" => Ok(Self::Feature),
            "epic" => Ok(Self::Epic),
            "chore" => Ok(Self::Chore),
            "docs" | "documentation" => Ok(Self::Docs),
            "question" => Ok(Self::Question),
            _ => Err(BeadsError::InvalidType { issue_type: s.into() }),
        }
    }
}

impl std::fmt::Display for IssueType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}
```

### 6.3 ID Generation

```rust
// src/model/id.rs

use sha2::{Sha256, Digest};
use chrono::Utc;
use crate::error::Result;

/// Generate a new issue ID.
///
/// Format: `<prefix>-<hash>` where hash is 3-8 base36 characters.
pub fn generate_id(prefix: &str, title: &str, created_by: Option<&str>) -> String {
    let mut hasher = Sha256::new();
    hasher.update(title);
    hasher.update(created_by.unwrap_or(""));
    hasher.update(Utc::now().timestamp_nanos_opt().unwrap_or(0).to_le_bytes());

    let hash = hasher.finalize();
    let short_hash = base36_encode(&hash[..4]);

    format!("{prefix}-{short_hash}")
}

/// Generate a hierarchical child ID.
///
/// Format: `<parent>.<n>` where n is the next child number.
pub fn generate_child_id(parent: &str, child_number: u32) -> String {
    format!("{parent}.{child_number}")
}

/// Encode bytes as base36 (0-9, a-z).
fn base36_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

    let mut num = 0u64;
    for &b in bytes.iter().take(8) {
        num = (num << 8) | u64::from(b);
    }

    if num == 0 {
        return "0".to_string();
    }

    let mut result = Vec::new();
    while num > 0 {
        result.push(ALPHABET[(num % 36) as usize]);
        num /= 36;
    }

    result.reverse();
    String::from_utf8(result).unwrap_or_default()
}

/// Parse priority from string (0-4 or P0-P4).
pub fn parse_priority(s: &str) -> Result<i32> {
    let s = s.to_uppercase();
    let num = if let Some(stripped) = s.strip_prefix('P') {
        stripped
    } else {
        &s
    };

    num.parse::<i32>()
        .ok()
        .filter(|&p| (0..=4).contains(&p))
        .ok_or_else(|| crate::error::BeadsError::Validation {
            field: "priority".into(),
            reason: format!("must be 0-4 or P0-P4, got '{s}'"),
        })
}

/// Validate ID format.
pub fn validate_id(id: &str, expected_prefix: &str) -> Result<()> {
    if id.is_empty() {
        return Err(crate::error::BeadsError::InvalidId { id: id.into() });
    }

    // Check prefix
    if !id.starts_with(expected_prefix) && !id.starts_with(&format!("{expected_prefix}-")) {
        // Allow if it's a different valid prefix (for cross-project refs)
        if !id.contains('-') {
            return Err(crate::error::BeadsError::InvalidId { id: id.into() });
        }
    }

    Ok(())
}
```

---

## 7. Configuration System

### 7.1 Layered Configuration (xf pattern)

```rust
// src/config/loader.rs

use std::path::{Path, PathBuf};
use std::env;
use crate::error::Result;
use crate::config::types::Config;

/// Load configuration with precedence:
/// 1. CLI arguments (not handled here)
/// 2. Environment variables (BR_* prefix)
/// 3. Project config (.beads/config.toml)
/// 4. User config (~/.config/br/config.toml)
/// 5. Compiled defaults
pub fn load_config(beads_dir: Option<&Path>) -> Result<Config> {
    let mut config = Config::default();

    // Layer 5: Defaults (already applied via Default trait)

    // Layer 4: User config
    if let Some(user_config_path) = user_config_path() {
        if user_config_path.exists() {
            let user_config = load_toml_config(&user_config_path)?;
            config.merge(user_config);
        }
    }

    // Layer 3: Project config
    if let Some(beads_dir) = beads_dir {
        let project_config_path = beads_dir.join("config.toml");
        if project_config_path.exists() {
            let project_config = load_toml_config(&project_config_path)?;
            config.merge(project_config);
        }
    }

    // Layer 2: Environment variables
    config.apply_env_overrides();

    // Expand tilde in paths
    config.expand_tilde();

    Ok(config)
}

/// Get the user config directory path.
fn user_config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("br").join("config.toml"))
}

/// Load a TOML config file.
fn load_toml_config(path: &Path) -> Result<Config> {
    let content = std::fs::read_to_string(path)?;
    let config: Config = toml::from_str(&content)
        .map_err(|e| crate::error::BeadsError::Config(format!("{}: {}", path.display(), e)))?;
    Ok(config)
}
```

### 7.2 Configuration Types

```rust
// src/config/types.rs

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Main configuration structure.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct Config {
    /// Issue ID prefix (e.g., "bd", "br")
    pub issue_prefix: Option<String>,

    /// Default priority for new issues
    pub default_priority: Option<i32>,

    /// Default issue type for new issues
    pub default_type: Option<String>,

    /// Actor/identity for operations
    pub actor: Option<String>,

    /// Custom status values (comma-separated)
    pub custom_statuses: Option<String>,

    /// Custom issue types (comma-separated)
    pub custom_types: Option<String>,

    /// Require description on create
    pub require_description: bool,

    /// Maximum hierarchy depth for child issues
    pub max_hierarchy_depth: usize,

    /// Paths configuration
    pub paths: PathsConfig,

    /// Output configuration
    pub output: OutputConfig,

    /// Validation configuration
    pub validation: ValidationConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PathsConfig {
    /// Path to database file
    pub db: Option<PathBuf>,

    /// Path to JSONL export
    pub jsonl: Option<PathBuf>,
}

impl Default for PathsConfig {
    fn default() -> Self {
        Self {
            db: None,
            jsonl: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct OutputConfig {
    /// Default output format
    pub format: Option<String>,

    /// Enable colors
    pub color: bool,

    /// Default limit for list commands
    pub default_limit: usize,
}

impl Default for OutputConfig {
    fn default() -> Self {
        Self {
            format: None,
            color: true,
            default_limit: 50,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct ValidationConfig {
    /// Validation mode on create: none, warn, strict
    pub on_create: String,

    /// Validation mode on sync: none, warn, strict
    pub on_sync: String,
}

impl Config {
    /// Merge another config into this one (other takes precedence).
    pub fn merge(&mut self, other: Config) {
        if other.issue_prefix.is_some() {
            self.issue_prefix = other.issue_prefix;
        }
        if other.default_priority.is_some() {
            self.default_priority = other.default_priority;
        }
        if other.default_type.is_some() {
            self.default_type = other.default_type;
        }
        if other.actor.is_some() {
            self.actor = other.actor;
        }
        if other.custom_statuses.is_some() {
            self.custom_statuses = other.custom_statuses;
        }
        if other.custom_types.is_some() {
            self.custom_types = other.custom_types;
        }
        // ... merge other fields
    }

    /// Apply environment variable overrides.
    pub fn apply_env_overrides(&mut self) {
        if let Ok(prefix) = std::env::var("BR_ISSUE_PREFIX") {
            self.issue_prefix = Some(prefix);
        }
        if let Ok(actor) = std::env::var("BR_ACTOR") {
            self.actor = Some(actor);
        }
        // Legacy env var support
        if let Ok(actor) = std::env::var("BEADS_ACTOR") {
            self.actor = Some(actor);
        }
        if let Ok(actor) = std::env::var("BD_ACTOR") {
            self.actor = Some(actor);
        }
    }

    /// Expand tilde in path configurations.
    pub fn expand_tilde(&mut self) {
        if let Some(ref mut db) = self.paths.db {
            *db = expand_tilde_path(db);
        }
        if let Some(ref mut jsonl) = self.paths.jsonl {
            *jsonl = expand_tilde_path(jsonl);
        }
    }
}

/// Expand ~ to home directory.
fn expand_tilde_path(path: &PathBuf) -> PathBuf {
    if let Some(path_str) = path.to_str() {
        if path_str.starts_with("~/") {
            if let Some(home) = dirs::home_dir() {
                return home.join(&path_str[2..]);
            }
        }
    }
    path.clone()
}
```

---

## 8. JSONL Import/Export

### 8.1 Export (streaming pattern)

```rust
// src/sync/export.rs

use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;
use crate::error::{Result, ResultExt};
use crate::model::Issue;
use crate::storage::Storage;

/// Export issues to JSONL file.
pub fn export_jsonl(
    storage: &impl Storage,
    output_path: &Path,
    options: ExportOptions,
) -> Result<ExportStats> {
    let issues = load_issues_for_export(storage, &options)?;

    // Safety check: don't overwrite non-empty JSONL with empty DB
    if issues.is_empty() && !options.force {
        if output_path.exists() {
            let existing = fs::read_to_string(output_path).unwrap_or_default();
            if !existing.trim().is_empty() {
                return Err(crate::error::BeadsError::Validation {
                    field: "export".into(),
                    reason: "refusing to overwrite non-empty JSONL with empty database".into(),
                });
            }
        }
    }

    // Atomic write via temp file
    let temp_path = output_path.with_extension("jsonl.tmp");
    write_jsonl_atomic(&issues, &temp_path, output_path)?;

    // Update export metadata
    if !options.skip_metadata {
        update_export_metadata(storage, &issues)?;
    }

    Ok(ExportStats {
        exported: issues.len(),
        tombstones: issues.iter().filter(|i| i.is_tombstone()).count(),
        skipped_ephemeral: 0, // Tracked during load
    })
}

/// Load issues for export with filters.
fn load_issues_for_export(
    storage: &impl Storage,
    options: &ExportOptions,
) -> Result<Vec<Issue>> {
    let filter = crate::storage::ListFilter {
        include_tombstones: true,
        include_ephemeral: false, // Never export wisps
        ..Default::default()
    };

    let mut issues = storage.list_issues(&filter)?
        .into_iter()
        .map(|iwc| iwc.issue)
        .collect::<Vec<_>>();

    // Enrich with labels, dependencies, comments
    for issue in &mut issues {
        issue.labels = storage.get_labels(&issue.id)?;
        issue.dependencies = storage.get_dependencies(&issue.id)?;
        issue.comments = storage.get_comments(&issue.id)?;
    }

    // Sort by ID for deterministic output
    issues.sort_by(|a, b| a.id.cmp(&b.id));

    Ok(issues)
}

/// Write issues to JSONL atomically.
fn write_jsonl_atomic(issues: &[Issue], temp_path: &Path, final_path: &Path) -> Result<()> {
    let file = File::create(temp_path)
        .context("Failed to create temp export file")?;
    let mut writer = BufWriter::with_capacity(256 * 1024, file); // 256KB buffer

    for issue in issues {
        let json = serde_json::to_string(issue)?;
        writeln!(writer, "{json}")?;
    }

    writer.flush()?;
    drop(writer);

    // Atomic rename
    fs::rename(temp_path, final_path)
        .context("Failed to rename export file")?;

    // Set permissions (0644) - world-readable for multi-user/agent access
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o644);
        fs::set_permissions(final_path, perms)?;
    }

    Ok(())
}

#[derive(Debug, Default)]
pub struct ExportOptions {
    pub force: bool,
    pub skip_metadata: bool,
    pub include_status: Vec<String>,
}

#[derive(Debug)]
pub struct ExportStats {
    pub exported: usize,
    pub tombstones: usize,
    pub skipped_ephemeral: usize,
}
```

### 8.2 Import (collision detection)

```rust
// src/sync/import.rs

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use crate::error::{BeadsError, Result, ResultExt};
use crate::model::Issue;
use crate::storage::Storage;

/// Import issues from JSONL file.
pub fn import_jsonl(
    storage: &mut impl Storage,
    input_path: &Path,
    options: ImportOptions,
) -> Result<ImportStats> {
    let file = File::open(input_path)
        .context("Failed to open JSONL file")?;
    let reader = BufReader::with_capacity(2 * 1024 * 1024, file); // 2MB buffer

    let mut stats = ImportStats::default();
    let mut issues = Vec::new();

    // Parse JSONL
    for (line_num, line) in reader.lines().enumerate() {
        let line = line.context("Failed to read line")?;
        let line = line.trim();

        if line.is_empty() {
            continue;
        }

        match serde_json::from_str::<Issue>(line) {
            Ok(mut issue) => {
                // Skip ephemeral issues
                if issue.is_ephemeral() {
                    stats.skipped_ephemeral += 1;
                    continue;
                }

                // Validate
                if let Err(e) = issue.validate() {
                    if options.strict {
                        return Err(e);
                    }
                    stats.validation_warnings += 1;
                    continue;
                }

                // Recompute content hash
                issue.content_hash = issue.compute_content_hash();

                issues.push(issue);
            }
            Err(e) => {
                if options.strict {
                    return Err(BeadsError::JsonlParse {
                        line: line_num + 1,
                        reason: e.to_string(),
                    });
                }
                stats.parse_errors += 1;
            }
        }
    }

    // Check for prefix mismatches
    let db_prefix = storage.get_prefix()?;
    let mismatched: Vec<_> = issues
        .iter()
        .filter(|i| !i.id.starts_with(&format!("{db_prefix}-")))
        .map(|i| i.id.clone())
        .collect();

    if !mismatched.is_empty() && !options.rename_on_import && !options.force {
        return Err(BeadsError::PrefixMismatch {
            expected: db_prefix,
            found: mismatched.first().unwrap().split('-').next().unwrap_or("").into(),
        });
    }

    // Check for collisions
    let collisions = detect_collisions(storage, &issues)?;
    if !collisions.is_empty() && !options.force {
        return Err(BeadsError::ImportCollision {
            count: collisions.len(),
        });
    }

    // Perform import
    for issue in &issues {
        match storage.get_issue(&issue.id)? {
            Some(existing) => {
                // Update if incoming is newer
                if issue.updated_at > existing.updated_at {
                    storage.update_issue(issue)?;
                    stats.updated += 1;
                } else {
                    stats.unchanged += 1;
                }
            }
            None => {
                // Check for tombstone
                if storage.id_exists(&issue.id)? {
                    stats.skipped_tombstone += 1;
                } else {
                    storage.create_issue(issue)?;
                    stats.created += 1;
                }
            }
        }

        // Import labels
        for label in &issue.labels {
            let _ = storage.add_label(&issue.id, label);
        }

        // Import dependencies
        for dep in &issue.dependencies {
            let _ = storage.add_dependency(dep);
        }

        // Import comments
        for comment in &issue.comments {
            let _ = storage.add_comment(comment);
        }
    }

    Ok(stats)
}

/// Detect content hash collisions.
fn detect_collisions(storage: &impl Storage, issues: &[Issue]) -> Result<Vec<String>> {
    let mut collisions = Vec::new();

    for issue in issues {
        if let Some(existing) = storage.get_issue(&issue.id)? {
            if existing.content_hash != issue.content_hash {
                collisions.push(issue.id.clone());
            }
        }
    }

    Ok(collisions)
}

#[derive(Debug, Default)]
pub struct ImportOptions {
    pub strict: bool,
    pub force: bool,
    pub rename_on_import: bool,
    pub skip_existing: bool,
}

#[derive(Debug, Default)]
pub struct ImportStats {
    pub created: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub skipped_ephemeral: usize,
    pub skipped_tombstone: usize,
    pub parse_errors: usize,
    pub validation_warnings: usize,
}
```

### 8.3 Depth-Ordered Import (hierarchy preservation)

When importing issues with dependencies, insert in **topological order** to preserve parent-child relationships:

```rust
// src/sync/import.rs

/// Sort issues by dependency depth for correct insertion order.
///
/// Issues with no dependencies come first, then issues that depend only
/// on issues already in the list. This ensures parent issues exist
/// before child issues are inserted.
fn sort_by_dependency_depth(issues: &mut [Issue]) {
    use std::collections::{HashMap, HashSet};

    // Build dependency graph
    let mut depths: HashMap<&str, usize> = HashMap::new();
    let issue_ids: HashSet<&str> = issues.iter().map(|i| i.id.as_str()).collect();

    // Multiple passes to compute depths
    let mut changed = true;
    while changed {
        changed = false;
        for issue in issues.iter() {
            let current_depth = depths.get(issue.id.as_str()).copied().unwrap_or(0);

            for dep in &issue.dependencies {
                if issue_ids.contains(dep.depends_on_id.as_str()) {
                    let parent_depth = depths.get(dep.depends_on_id.as_str()).copied().unwrap_or(0);
                    let needed_depth = parent_depth + 1;
                    if needed_depth > current_depth {
                        depths.insert(issue.id.as_str(), needed_depth);
                        changed = true;
                    }
                }
            }
        }
    }

    // Sort by depth (ascending) then by ID for stability
    issues.sort_by(|a, b| {
        let depth_a = depths.get(a.id.as_str()).copied().unwrap_or(0);
        let depth_b = depths.get(b.id.as_str()).copied().unwrap_or(0);
        depth_a.cmp(&depth_b).then(a.id.cmp(&b.id))
    });
}
```

### 8.4 Debounced Export (within command scope)

Export is debounced **within a single command invocation** to avoid multiple writes:

```rust
// src/sync/dirty.rs

use std::cell::Cell;
use std::time::{Duration, Instant};

/// Tracks whether an export is needed and debounces rapid requests.
pub struct DirtyTracker {
    is_dirty: Cell<bool>,
    last_export: Cell<Option<Instant>>,
    debounce_ms: u64,
}

impl DirtyTracker {
    pub const fn new(debounce_ms: u64) -> Self {
        Self {
            is_dirty: Cell::new(false),
            last_export: Cell::new(None),
            debounce_ms,
        }
    }

    /// Mark that an export is needed.
    pub fn mark_dirty(&self) {
        self.is_dirty.set(true);
    }

    /// Check if export should happen now.
    pub fn should_export(&self) -> bool {
        if !self.is_dirty.get() {
            return false;
        }

        if let Some(last) = self.last_export.get() {
            if last.elapsed() < Duration::from_millis(self.debounce_ms) {
                return false;
            }
        }

        true
    }

    /// Record that export happened.
    pub fn mark_exported(&self) {
        self.is_dirty.set(false);
        self.last_export.set(Some(Instant::now()));
    }
}

/// Default debounce interval (500ms, matching bd).
pub const DEFAULT_DEBOUNCE_MS: u64 = 500;
```

**Key behavior:**

| Scenario | Behavior |
|----------|----------|
| Single mutation | Export at command end |
| Multiple mutations in same command | Single export at command end |
| Rapid successive commands | Debounce prevents thrashing |
| `--no-auto-flush` flag | Skip automatic export |

**Note:** Unlike bd, br has **no daemon** for background auto-flush. All exports happen within the command lifecycle, debounced to prevent write churn.

---

## 9. Progress & Concurrency

### 9.1 Atomic Progress Tracking (cass pattern)

```rust
// src/util/progress.rs

use std::sync::atomic::{AtomicUsize, Ordering};
use parking_lot::Mutex;

/// Thread-safe progress tracking for long operations.
pub struct Progress {
    pub total: AtomicUsize,
    pub current: AtomicUsize,
    pub phase: AtomicUsize,
    pub last_message: Mutex<Option<String>>,
}

impl Progress {
    pub fn new() -> Self {
        Self {
            total: AtomicUsize::new(0),
            current: AtomicUsize::new(0),
            phase: AtomicUsize::new(0),
            last_message: Mutex::new(None),
        }
    }

    /// Increment current count.
    #[inline]
    pub fn inc(&self) {
        self.current.fetch_add(1, Ordering::Relaxed);
    }

    /// Set total count.
    #[inline]
    pub fn set_total(&self, total: usize) {
        self.total.store(total, Ordering::Relaxed);
    }

    /// Get current progress as fraction.
    #[must_use]
    pub fn fraction(&self) -> f64 {
        let total = self.total.load(Ordering::Relaxed);
        if total == 0 {
            return 0.0;
        }
        let current = self.current.load(Ordering::Relaxed);
        current as f64 / total as f64
    }

    /// Set status message.
    pub fn set_message(&self, msg: impl Into<String>) {
        *self.last_message.lock() = Some(msg.into());
    }
}

impl Default for Progress {
    fn default() -> Self {
        Self::new()
    }
}

/// Phase constants.
pub mod phases {
    pub const IDLE: usize = 0;
    pub const SCANNING: usize = 1;
    pub const PROCESSING: usize = 2;
    pub const WRITING: usize = 3;
    pub const COMPLETE: usize = 4;
}
```

### 9.2 Parallel Processing (xf pattern)

```rust
// Example usage in export

use rayon::prelude::*;

/// Export with parallel enrichment.
pub fn export_parallel(
    storage: &impl Storage,
    issues: Vec<Issue>,
) -> Result<Vec<Issue>> {
    // Parallel label/dependency enrichment
    let enriched: Vec<_> = issues
        .into_par_iter()
        .map(|mut issue| {
            // These are independent lookups, safe to parallelize
            issue.labels = storage.get_labels(&issue.id).unwrap_or_default();
            issue.dependencies = storage.get_dependencies(&issue.id).unwrap_or_default();
            issue
        })
        .collect();

    Ok(enriched)
}
```

---

## 10. Testing Strategy

### 10.1 Test Organization

```
tests/
├── integration/
│   ├── mod.rs
│   ├── crud_test.rs        # Create/Read/Update/Delete
│   ├── list_test.rs        # List with filters
│   ├── deps_test.rs        # Dependency management
│   ├── jsonl_test.rs       # Import/export round-trip
│   └── conformance/        # bd ↔ br parity
│       ├── mod.rs
│       ├── create_parity.rs
│       ├── list_parity.rs
│       └── json_output_parity.rs
├── fixtures/
│   ├── sample_issues.jsonl
│   ├── with_deps.jsonl
│   └── with_tombstones.jsonl
└── common/
    └── mod.rs              # Shared test utilities
```

### 10.2 Test Utilities

```rust
// tests/common/mod.rs

use beads_rust::storage::{SqliteStorage, Storage};
use tempfile::TempDir;

/// Create a temporary storage for testing.
pub fn temp_storage() -> (TempDir, SqliteStorage) {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db");
    let storage = SqliteStorage::open(&db_path).unwrap();
    (dir, storage)
}

/// Create a sample issue.
pub fn sample_issue(id: &str, title: &str) -> beads_rust::model::Issue {
    beads_rust::model::Issue::new(id.into(), title.into())
}

/// Create issues from fixtures.
pub fn load_fixture(name: &str) -> Vec<beads_rust::model::Issue> {
    let path = format!("tests/fixtures/{name}");
    let content = std::fs::read_to_string(&path).unwrap();
    content
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}
```

### 10.3 Integration Test Example

```rust
// tests/integration/crud_test.rs

use beads_rust::storage::Storage;
use beads_rust::model::{Issue, Status};

mod common;

#[test]
fn test_create_and_retrieve() {
    let (_dir, mut storage) = common::temp_storage();

    let issue = common::sample_issue("br-test1", "Test issue");
    storage.create_issue(&issue).unwrap();

    let retrieved = storage.get_issue("br-test1").unwrap();
    assert!(retrieved.is_some());
    assert_eq!(retrieved.unwrap().title, "Test issue");
}

#[test]
fn test_update_status() {
    let (_dir, mut storage) = common::temp_storage();

    let mut issue = common::sample_issue("br-test2", "Status test");
    storage.create_issue(&issue).unwrap();

    issue.status = Status::InProgress;
    storage.update_issue(&issue).unwrap();

    let retrieved = storage.get_issue("br-test2").unwrap().unwrap();
    assert_eq!(retrieved.status, Status::InProgress);
}

#[test]
fn test_partial_id_resolution() {
    let (_dir, mut storage) = common::temp_storage();

    let issue = common::sample_issue("br-abc123", "Partial ID test");
    storage.create_issue(&issue).unwrap();

    // Full ID
    assert_eq!(storage.resolve_partial_id("br-abc123").unwrap(), "br-abc123");

    // Hash only
    assert_eq!(storage.resolve_partial_id("abc123").unwrap(), "br-abc123");

    // Partial hash
    assert_eq!(storage.resolve_partial_id("abc").unwrap(), "br-abc123");
}
```

### 10.4 CLI Integration Tests (cass pattern)

```rust
// tests/integration/cli_test.rs

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

fn br_cmd() -> Command {
    Command::cargo_bin("br").unwrap()
}

#[test]
fn test_init_creates_beads_dir() {
    let dir = TempDir::new().unwrap();

    br_cmd()
        .current_dir(dir.path())
        .arg("init")
        .assert()
        .success();

    assert!(dir.path().join(".beads").exists());
    assert!(dir.path().join(".beads/beads.db").exists());
}

#[test]
fn test_create_outputs_id() {
    let dir = TempDir::new().unwrap();

    br_cmd()
        .current_dir(dir.path())
        .arg("init")
        .assert()
        .success();

    br_cmd()
        .current_dir(dir.path())
        .args(["create", "Test issue", "--silent"])
        .assert()
        .success()
        .stdout(predicate::str::starts_with("br-"));
}

#[test]
fn test_json_output() {
    let dir = TempDir::new().unwrap();

    br_cmd()
        .current_dir(dir.path())
        .arg("init")
        .assert()
        .success();

    br_cmd()
        .current_dir(dir.path())
        .args(["create", "JSON test"])
        .assert()
        .success();

    let output = br_cmd()
        .current_dir(dir.path())
        .args(["list", "--json"])
        .output()
        .unwrap();

    let json: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(json.is_array());
}
```

---

## 11. Build & Release

### 11.1 Build Script

```rust
// build.rs

fn main() {
    // Embed build metadata using vergen
    let build = vergen_gix::BuildBuilder::default()
        .build_timestamp(true)
        .build()
        .expect("vergen build");

    let cargo = vergen_gix::CargoBuilder::default()
        .target_triple(true)
        .build()
        .expect("vergen cargo");

    let rustc = vergen_gix::RustcBuilder::default()
        .semver(true)
        .build()
        .expect("vergen rustc");

    vergen_gix::Emitter::default()
        .add_instructions(&build)
        .expect("add build")
        .add_instructions(&cargo)
        .expect("add cargo")
        .add_instructions(&rustc)
        .expect("add rustc")
        .emit()
        .expect("vergen emit");
}
```

### 11.2 rust-toolchain.toml

```toml
[toolchain]
channel = "nightly-2025-01-01"
components = ["rustfmt", "clippy"]
profile = "minimal"
```

### 11.3 Cargo Config

```toml
# .cargo/config.toml

[build]
# Enable incremental compilation
incremental = true

[target.x86_64-unknown-linux-gnu]
# Use mold linker for faster linking (if available)
# linker = "clang"
# rustflags = ["-C", "link-arg=-fuse-ld=mold"]

[alias]
b = "build"
t = "test"
r = "run"
c = "check"
cl = "clippy"
```

---

## 12. Implementation Phases

### Phase 1: Foundation (Week 1-2)

**Goals**: Project setup, core types, basic storage

**Deliverables**:
- [ ] Cargo.toml with all dependencies
- [ ] `model/` module with Issue, Status, IssueType, Dependency
- [ ] `storage/sqlite.rs` with schema and basic CRUD
- [ ] `error/` module with BeadsError and ResultExt
- [ ] Basic CLI skeleton with clap
- [ ] `br init` command working

**Tests**:
- [ ] Model validation tests
- [ ] SQLite schema creation tests
- [ ] Error formatting tests

### Phase 2: Core Commands (Week 3-4)

**Goals**: Essential CRUD and query commands

**Deliverables**:
- [ ] `br create` with all flags
- [ ] `br update` with field updates
- [ ] `br close` / `br reopen`
- [ ] `br list` with filters
- [ ] `br show` with details
- [ ] `br ready` / `br blocked`
- [ ] ID resolution (partial IDs)

**Tests**:
- [ ] CRUD integration tests
- [ ] Filter tests
- [ ] JSON output tests

### Phase 3: Relations & Search (Week 5-6)

**Goals**: Dependencies, labels, comments, search

**Deliverables**:
- [ ] `br dep add/remove/list/tree`
- [ ] `br label add/remove/list/list-all`
- [ ] `br comments add/list`
- [ ] `br search` with text matching
- [ ] Blocked cache implementation
- [ ] Cycle detection

**Tests**:
- [ ] Dependency graph tests
- [ ] Search accuracy tests
- [ ] Cache invalidation tests

### Phase 4: Sync & Config (Week 7-8)

**Goals**: JSONL import/export, configuration

**Deliverables**:
- [ ] `br export` to JSONL
- [ ] `br import` from JSONL
- [ ] `br sync --flush-only` / `--import-only`
- [ ] Collision detection
- [ ] Prefix mismatch handling
- [ ] `br config get/set/list`
- [ ] Layered config loading

**Tests**:
- [ ] Round-trip import/export tests
- [ ] Collision handling tests
- [ ] Config precedence tests

### Phase 5: Polish & Conformance (Week 9-10)

**Goals**: Parity with bd, production readiness

**Deliverables**:
- [ ] `br doctor` (read-only diagnostics)
- [ ] `br info` / `br where` / `br version`
- [ ] `br count` / `br stats` / `br stale`
- [ ] `br defer` / `br undefer`
- [ ] Conformance tests against bd
- [ ] Documentation
- [ ] Performance benchmarks

**Tests**:
- [ ] Full conformance test suite
- [ ] Performance benchmarks
- [ ] Edge case coverage

---

## 13. Conformance Testing Strategy

### 13.1 Test Harness

```bash
#!/bin/bash
# scripts/conformance_test.sh

# Run same command on both bd and br, compare JSON output

test_command() {
    local cmd="$1"
    local bd_out=$(bd $cmd --json 2>/dev/null)
    local br_out=$(br $cmd --json 2>/dev/null)

    # Normalize volatile fields (timestamps, hashes)
    bd_normalized=$(echo "$bd_out" | jq 'walk(if type == "object" then del(.updated_at, .content_hash) else . end)')
    br_normalized=$(echo "$br_out" | jq 'walk(if type == "object" then del(.updated_at, .content_hash) else . end)')

    if [ "$bd_normalized" = "$br_normalized" ]; then
        echo "PASS: $cmd"
        return 0
    else
        echo "FAIL: $cmd"
        diff <(echo "$bd_normalized") <(echo "$br_normalized")
        return 1
    fi
}

# Test suite
test_command "list"
test_command "list --status open"
test_command "ready"
test_command "blocked"
test_command "count"
test_command "stats"
```

### 13.2 Schema Comparison

```rust
// tests/conformance/schema_test.rs

#[test]
fn test_schema_matches_bd() {
    // Run PRAGMA table_info on both databases
    // Compare column names, types, constraints
}

#[test]
fn test_jsonl_field_order() {
    // Export from bd, import to br, re-export
    // Verify field ordering matches
}
```

---

## 14. Key Architectural Decisions Summary

| Decision | Choice | Rationale |
|----------|--------|-----------|
| Error handling | thiserror + anyhow hybrid | Structured public API, flexible internals |
| CLI framework | clap derive | Industry standard, minimal boilerplate |
| Storage | rusqlite with bundled SQLite | No system deps, portable |
| Serialization | serde + serde_json | De facto standard |
| Parallelism | rayon | Ergonomic data parallelism |
| Logging | tracing | Async-compatible, spans |
| Synchronization | parking_lot | Faster than std |
| Config format | TOML | Human-readable, Rust ecosystem standard |
| Output modes | Human + Robot (JSON) | Scripting support |
| Testing | tempfile + assert_cmd | Isolated, CLI-level tests |

---

## 15. Risk Mitigation

### 15.1 Identified Risks

| Risk | Probability | Impact | Mitigation |
|------|-------------|--------|------------|
| Schema drift from bd | Medium | High | Automated schema comparison tests |
| JSONL format incompatibility | Medium | High | Round-trip tests with bd-exported files |
| Performance regression | Low | Medium | Criterion benchmarks, profiling |
| SQLite version differences | Low | Medium | Bundle SQLite in rusqlite |
| Edge cases in ID resolution | Medium | Medium | Extensive partial ID tests |

### 15.2 Fallback Strategies

- **If performance is insufficient**: Add connection pooling, prepared statement caching
- **If schema changes**: Migration system with version tracking
- **If JSONL format diverges**: Versioned JSONL format with compat shims

---

## Appendix A: Command Matrix

| Command | Priority | Complexity | Dependencies |
|---------|----------|------------|--------------|
| init | P0 | Low | schema |
| create | P0 | Medium | id_gen, validation |
| update | P0 | Medium | validation |
| close | P0 | Low | update |
| list | P0 | Medium | filters |
| show | P0 | Medium | deps, labels, comments |
| ready | P0 | Medium | blocked_cache |
| blocked | P0 | Medium | blocked_cache |
| dep | P0 | High | graph algorithms |
| label | P1 | Low | - |
| comments | P1 | Low | - |
| search | P1 | Medium | FTS or LIKE |
| export | P0 | Medium | jsonl |
| import | P0 | High | collision detection |
| sync | P1 | Low | export/import |
| config | P1 | Low | - |
| doctor | P2 | Medium | diagnostics |
| info | P2 | Low | metadata |

---

## Appendix B: JSON Output Schemas

See `EXISTING_BEADS_STRUCTURE_AND_ARCHITECTURE.md` section 15.41 for canonical JSON schemas.

Key shapes:
- `IssueWithCounts`: Issue + dependency_count + dependent_count
- `IssueDetails`: Issue + labels + dependencies + dependents + comments + parent
- `BlockedIssue`: Issue + blocked_by_count + blocked_by
- `TreeNode`: Issue + depth + parent_id + truncated

---

## Appendix C: Success Criteria

`br` is considered **correct** when all of the following are true:

| Criterion | Verification |
|-----------|-------------|
| JSON outputs match `bd` for classic commands | Conformance test suite |
| Schema matches Go beads (tables, constraints, indexes) | `PRAGMA table_info` comparisons |
| No unexpected git or background behavior | Manual testing, code review |
| Fast startup (<100ms cold, <50ms warm) | Benchmarks |
| Deterministic output (same inputs → same output) | Repeated test runs |
| Stable in both human and robot modes | Integration tests |
| All commands return exit code 0 on success, 1 on error | CLI tests |
| Content hashes match bd for identical issues | Cross-tool validation |

## Appendix D: Explicit Non-Goals

These features are **explicitly excluded** from br:

| Feature | Reason |
|---------|--------|
| Git hooks | Non-invasive design |
| Git config modifications | Non-invasive design |
| Daemon/RPC server | Non-invasive design |
| Background file watchers | Non-invasive design |
| Auto-commit/auto-push | Non-invasive design |
| Dolt backend | SQLite-only for simplicity |
| Agent/molecule/gate features | Gastown scope |
| Rig/convoy/HOP features | Gastown scope |
| Linear/Jira integration | Deferred (external services) |
| MCP Claude plugin | Separate repository |

---

*This architecture is designed to be the optimal foundation for `br`, combining proven patterns from xf and cass with the authoritative beads specification.*

*Document version: 1.1*
