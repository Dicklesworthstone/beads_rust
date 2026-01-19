# Architecture Overview

This document describes the internal architecture of `beads_rust` (br), a Rust port of the classic beads issue tracker.

---

## Table of Contents

- [Design Philosophy](#design-philosophy)
- [High-Level Architecture](#high-level-architecture)
- [Module Structure](#module-structure)
- [Data Flow](#data-flow)
- [Storage Layer](#storage-layer)
- [Sync System](#sync-system)
- [Configuration System](#configuration-system)
- [Error Handling](#error-handling)
- [CLI Layer](#cli-layer)
- [Key Patterns](#key-patterns)
- [Safety Invariants](#safety-invariants)
- [Extension Points](#extension-points)

---

## Design Philosophy

### Core Principles

1. **Non-Invasive**: No daemons, no git hooks, no automatic commits
2. **Local-First**: SQLite is the source of truth; JSONL enables collaboration
3. **Agent-Friendly**: Machine-readable output (JSON) for AI coding agents
4. **Deterministic**: Same input produces same output
5. **Safe**: No operations outside `.beads/` directory

### Comparison with Go beads (bd)

| Feature | br (Rust) | bd (Go) |
|---------|-----------|---------|
| Lines of Code | ~33k | ~276k |
| Backend | SQLite only | SQLite + Dolt |
| Daemon | None | RPC daemon |
| Git operations | Manual | Can auto-commit |
| Git hooks | None | Optional auto-install |

---

## High-Level Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                         CLI Layer                                │
│  ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌─────────┐ ┌─────────┐   │
│  │  create │ │  list   │ │  ready  │ │  sync   │ │  ...    │   │
│  └────┬────┘ └────┬────┘ └────┬────┘ └────┬────┘ └────┬────┘   │
└───────┼──────────┼──────────┼──────────┼──────────┼───────────┘
        │          │          │          │          │
        v          v          v          v          v
┌─────────────────────────────────────────────────────────────────┐
│                      Business Logic                              │
│  ┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐  │
│  │   Validation    │  │   Formatting    │  │   ID Generation │  │
│  └─────────────────┘  └─────────────────┘  └─────────────────┘  │
└──────────────────────────────┬──────────────────────────────────┘
                               │
                               v
┌─────────────────────────────────────────────────────────────────┐
│                       Storage Layer                              │
│  ┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐  │
│  │  SqliteStorage  │  │  Dirty Tracking │  │  Blocked Cache  │  │
│  └────────┬────────┘  └─────────────────┘  └─────────────────┘  │
└───────────┼─────────────────────────────────────────────────────┘
            │
            v
┌───────────────────────┐        ┌───────────────────────┐
│  .beads/beads.db      │  <-->  │  .beads/issues.jsonl  │
│  (SQLite - Primary)   │  sync  │  (Git-friendly)       │
└───────────────────────┘        └───────────────────────┘
```

---

## Module Structure

```
src/
├── main.rs           # Entry point, CLI dispatch
├── lib.rs            # Crate root, module exports
│
├── cli/              # Command-line interface
│   ├── mod.rs        # Clap definitions (Cli, Commands, Args)
│   └── commands/     # Individual command implementations
│       ├── create.rs
│       ├── list.rs
│       ├── ready.rs
│       ├── sync.rs
│       └── ...       # 30+ command files
│
├── model/            # Data types
│   └── mod.rs        # Issue, Status, Priority, Dependency, etc.
│
├── storage/          # Persistence layer
│   ├── mod.rs        # Module exports
│   ├── sqlite.rs     # SqliteStorage implementation
│   ├── schema.rs     # Database schema definitions
│   └── events.rs     # Audit event storage
│
├── sync/             # JSONL import/export
│   ├── mod.rs        # Export/import functions
│   ├── path.rs       # Path validation (safety)
│   └── history.rs    # Backup history management
│
├── config/           # Configuration system
│   ├── mod.rs        # Layered config resolution
│   └── routing.rs    # Cross-project routing
│
├── error/            # Error handling
│   ├── mod.rs        # BeadsError enum
│   ├── structured.rs # JSON error output
│   └── context.rs    # Error context helpers
│
├── format/           # Output formatting
│   ├── mod.rs        # Module exports
│   ├── text.rs       # Human-readable output
│   ├── output.rs     # JSON output
│   └── csv.rs        # CSV export
│
├── util/             # Utilities
│   ├── mod.rs        # Module exports
│   ├── id.rs         # Hash-based ID generation
│   ├── hash.rs       # Content hashing
│   ├── time.rs       # Timestamp utilities
│   └── progress.rs   # Progress indicators
│
├── validation/       # Input validation
│   └── mod.rs        # IssueValidator
│
└── logging.rs        # Tracing setup
```

---

## Data Flow

### Issue Creation

```
User Input                  CLI                     Storage                 Sync
    │                        │                        │                      │
    │  br create "title"     │                        │                      │
    │ ─────────────────────> │                        │                      │
    │                        │                        │                      │
    │                        │  Validate + Generate ID│                      │
    │                        │ ─────────────────────> │                      │
    │                        │                        │                      │
    │                        │                        │  INSERT into DB      │
    │                        │                        │ ──────────────>      │
    │                        │                        │                      │
    │                        │                        │  Mark dirty          │
    │                        │                        │ ──────────────>      │
    │                        │                        │                      │
    │                        │                        │  Record event        │
    │                        │                        │ ──────────────>      │
    │                        │                        │                      │
    │                        │  (auto-flush if enabled)                      │
    │                        │ ───────────────────────────────────────────> │
    │                        │                        │                      │
    │  ID: bd-abc123         │                        │                      │
    │ <───────────────────── │                        │                      │
```

### Sync Export

```
br sync --flush-only
        │
        v
┌───────────────────────────┐
│  1. Path Validation       │  Verify target is in .beads/
├───────────────────────────┤
│  2. Get dirty issue IDs   │  SELECT from dirty_issues
├───────────────────────────┤
│  3. Load all issues       │  Full export (deterministic)
├───────────────────────────┤
│  4. Write to temp file    │  Atomic write pattern
├───────────────────────────┤
│  5. Compute content hash  │  SHA-256 of content
├───────────────────────────┤
│  6. Atomic rename         │  temp -> issues.jsonl
├───────────────────────────┤
│  7. Clear dirty flags     │  DELETE from dirty_issues
├───────────────────────────┤
│  8. Create history backup │  Optional timestamped copy
└───────────────────────────┘
```

---

## Storage Layer

### SqliteStorage

The primary storage implementation using rusqlite.

```rust
pub struct SqliteStorage {
    conn: Connection,
}
```

**Key Features:**

- **WAL Mode**: Concurrent reads during writes
- **Busy Timeout**: Configurable lock timeout (default 30s)
- **Transactional Mutations**: 4-step protocol for safety

### Transaction Protocol

All mutations follow this pattern:

```rust
storage.mutate("operation", actor, |tx, ctx| {
    // 1. Perform the operation
    tx.execute(...)?;

    // 2. Record events for audit trail
    ctx.record_event(EventType::Created, &issue.id, None);

    // 3. Mark affected issues as dirty
    ctx.mark_dirty(&issue.id);

    // 4. Invalidate blocked cache if needed
    ctx.invalidate_cache();

    Ok(result)
})
```

### Database Schema

```sql
-- Core tables
issues              -- Primary issue data
dependencies        -- Issue relationships
labels              -- Issue labels (many-to-many)
comments            -- Issue discussion threads
events              -- Audit log

-- Operational tables
dirty_issues        -- Changed since last export
blocked_cache       -- Precomputed blocked status
config              -- Key-value configuration
```

### Dirty Tracking

Issues are marked dirty when:
- Created
- Updated (any field)
- Closed/reopened
- Dependencies added/removed
- Labels added/removed
- Comments added

Dirty flags are cleared after successful JSONL export.

### Blocked Cache

Precomputed table for fast `ready`/`blocked` queries:

```sql
CREATE TABLE blocked_cache (
    issue_id TEXT PRIMARY KEY,
    is_blocked INTEGER NOT NULL,
    blocking_ids TEXT  -- JSON array
);
```

Rebuilt when:
- Dependencies change
- Issues closed (may unblock others)
- Cache explicitly invalidated

---

## Sync System

### JSONL Format

Each line is a complete JSON object:

```json
{"id":"bd-abc123","title":"Fix bug","status":"open",...}
{"id":"bd-def456","title":"Add feature","status":"in_progress",...}
```

**Benefits:**
- Git-friendly (line-based diffs)
- Streamable (no need to parse entire file)
- Human-readable

### Export Process

```rust
pub fn export_to_jsonl(
    storage: &SqliteStorage,
    path: &Path,
    config: &ExportConfig,
) -> Result<ExportResult>
```

**Safety Guards:**

1. Path validation (must be in `.beads/`)
2. Atomic writes (temp file + rename)
3. Content hashing (detect corruption)
4. History backups (optional)

### Import Process

```rust
pub fn import_from_jsonl(
    storage: &mut SqliteStorage,
    path: &Path,
    config: &ImportConfig,
    prefix: Option<&str>,
) -> Result<ImportResult>
```

**Collision Handling:**

- By default, imports are additive
- Content hash comparison for conflict detection
- Force mode to overwrite conflicts

### Path Validation

Sync operations enforce a strict path allowlist:

```rust
pub const ALLOWED_EXTENSIONS: &[&str] = &[".jsonl", ".json", ".db", ".yaml"];
pub const ALLOWED_EXACT_NAMES: &[&str] = &["metadata.json", "config.yaml"];

pub fn is_sync_path_allowed(path: &Path, beads_dir: &Path) -> bool {
    // Must be inside .beads/
    // Must have allowed extension
    // Must not be in .git/
}
```

---

## Configuration System

### Layer Hierarchy

Configuration sources in precedence order (highest wins):

```
1. CLI overrides        (--json, --db, --actor)
2. Environment vars     (BD_ACTOR, BEADS_JSONL)
3. Project config       (.beads/config.yaml)
4. User config          (~/.config/bd/config.yaml)
5. Legacy user config   (~/.beads/config.yaml)
6. DB config table      (config table in SQLite)
7. Defaults
```

### Configuration Layer

```rust
pub struct ConfigLayer {
    pub startup: HashMap<String, String>,  // YAML/env only
    pub runtime: HashMap<String, String>,  // Can be in DB
}
```

**Startup-only keys** (cannot be stored in DB):
- `no-db`, `no-daemon`, `no-auto-flush`
- `db`, `actor`, `identity`
- `git.*`, `routing.*`, `sync.*`

### Key Configuration Options

| Key | Default | Description |
|-----|---------|-------------|
| `issue_prefix` | `bd` | ID prefix for new issues |
| `default_priority` | `2` | Default priority (0-4) |
| `default_type` | `task` | Default issue type |
| `display.color` | auto | ANSI color output |
| `lock-timeout` | `30000` | SQLite busy timeout (ms) |

---

## Error Handling

### Error Types

```rust
pub enum BeadsError {
    // Storage errors
    DatabaseNotFound { path: PathBuf },
    DatabaseLocked { path: PathBuf },
    SchemaMismatch { expected: i32, found: i32 },

    // Issue errors
    IssueNotFound { id: String },
    IdCollision { id: String },
    AmbiguousId { partial: String, matches: Vec<String> },

    // Validation errors
    Validation { field: String, reason: String },
    InvalidStatus { status: String },
    InvalidPriority { priority: i32 },

    // Dependency errors
    DependencyCycle { path: String },
    SelfDependency { id: String },

    // Sync errors
    JsonlParse { line: usize, reason: String },
    PrefixMismatch { expected: String, found: String },

    // I/O errors
    Io(std::io::Error),
    Json(serde_json::Error),
}
```

### Exit Codes

| Code | Category | Description |
|------|----------|-------------|
| 0 | Success | Command completed |
| 1 | Internal | Unexpected error |
| 2 | Database | Not initialized, locked |
| 3 | Issue | Not found, ambiguous ID |
| 4 | Validation | Invalid input |
| 5 | Dependency | Cycle detected |
| 6 | Sync | JSONL parse error |
| 7 | Config | Missing configuration |
| 8 | I/O | File system error |

### Structured Error Output

```json
{
  "error_code": 3,
  "kind": "not_found",
  "message": "Issue not found: bd-xyz999",
  "recovery_hints": [
    "Check the issue ID spelling",
    "Use 'br list' to find valid IDs"
  ]
}
```

---

## CLI Layer

### Command Structure

Uses Clap's derive macros:

```rust
#[derive(Parser)]
#[command(name = "br")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    #[arg(long, global = true)]
    pub json: bool,
    // ... other global options
}

#[derive(Subcommand)]
pub enum Commands {
    Create(CreateArgs),
    List(ListArgs),
    Ready(ReadyArgs),
    // ... 30+ commands
}
```

### Command Flow

```rust
fn main() {
    let cli = Cli::parse();

    // Initialize logging
    init_logging(cli.verbose, cli.quiet, None)?;

    // Build CLI overrides
    let overrides = build_cli_overrides(&cli);

    // Dispatch to command handler
    let result = match cli.command {
        Commands::Create(args) => commands::create::execute(args, &overrides),
        Commands::List(args) => commands::list::execute(&args, cli.json, &overrides),
        // ...
    };

    // Handle errors
    if let Err(e) = result {
        handle_error(&e, cli.json);
    }

    // Auto-flush if enabled
    if is_mutating && !cli.no_auto_flush {
        run_auto_flush(&overrides);
    }
}
```

---

## Key Patterns

### ID Generation

Hash-based short IDs for human readability:

```rust
pub struct IdConfig {
    pub prefix: String,         // e.g., "bd"
    pub min_hash_length: usize, // 3
    pub max_hash_length: usize, // 8
    pub max_collision_prob: f64, // 0.25
}

// Generated: bd-abc123
```

**Algorithm:**
1. Generate random bytes
2. Encode as alphanumeric hash
3. Start with min_length
4. Extend if collision detected
5. Fail if max_length reached

### Content Hashing

Deterministic hash for deduplication:

```rust
impl Issue {
    pub fn compute_content_hash(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.title.as_bytes());
        hasher.update(self.description.as_deref().unwrap_or("").as_bytes());
        hasher.update(self.status.as_str().as_bytes());
        // ... other fields
        format!("{:x}", hasher.finalize())
    }
}
```

**Excluded from hash:**
- `id` (generated)
- `created_at`, `updated_at` (timestamps)
- `labels`, `dependencies`, `comments` (relations)

### Atomic File Writes

Safe file updates using temp + rename:

```rust
fn atomic_write(path: &Path, content: &[u8]) -> Result<()> {
    let temp_path = path.with_extension("tmp");

    // Write to temp file
    let mut file = File::create(&temp_path)?;
    file.write_all(content)?;
    file.sync_all()?;

    // Atomic rename
    fs::rename(&temp_path, path)?;

    Ok(())
}
```

---

## Safety Invariants

### File System Safety

1. **All writes confined to `.beads/`**
   - Path validation before any write
   - No operations outside workspace

2. **No git operations**
   - Never runs `git` commands
   - User handles git manually

3. **Atomic writes**
   - Temp file + rename pattern
   - No partial writes

### Database Safety

1. **WAL mode**
   - Concurrent readers
   - Crash recovery

2. **Immediate transactions**
   - Exclusive lock for writes
   - No dirty reads

3. **Schema versioning**
   - Version check on open
   - Migration support

### See Also

- [SYNC_SAFETY.md](SYNC_SAFETY.md) - Detailed sync safety model
- [SYNC_MAINTENANCE_CHECKLIST.md](SYNC_MAINTENANCE_CHECKLIST.md) - Sync code maintenance

---

## Extension Points

### Adding New Commands

1. Create `src/cli/commands/mycommand.rs`
2. Add args struct to `src/cli/mod.rs`
3. Add variant to `Commands` enum
4. Add dispatch in `main.rs`

### Adding New Issue Fields

1. Add field to `Issue` struct in `model/mod.rs`
2. Update `compute_content_hash()` if content-relevant
3. Add column in `schema.rs`
4. Update INSERT/SELECT in `sqlite.rs`
5. Add serialization in format modules

### Custom Validators

Extend `IssueValidator` in `validation/mod.rs`:

```rust
impl IssueValidator {
    pub fn validate_custom_field(&self, issue: &Issue) -> Result<()> {
        // Custom validation logic
    }
}
```

---

## Dependencies

| Crate | Purpose |
|-------|---------|
| `clap` | CLI parsing with derive macros |
| `rusqlite` | SQLite storage (bundled) |
| `serde` + `serde_json` | Serialization |
| `chrono` | Timestamps |
| `sha2` | Content hashing |
| `thiserror` | Error types |
| `anyhow` | Error context |
| `tracing` | Structured logging |
| `rayon` | Parallel processing |

---

## See Also

- [CLI_REFERENCE.md](CLI_REFERENCE.md) - Command reference
- [AGENT_INTEGRATION.md](AGENT_INTEGRATION.md) - AI agent guide
- [SYNC_SAFETY.md](SYNC_SAFETY.md) - Sync safety model
- [../AGENTS.md](../AGENTS.md) - Development guidelines
