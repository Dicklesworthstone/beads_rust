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
│  2. Create history backup │  Optional timestamped copy (if overwriting)
├───────────────────────────┤
│  3. Get dirty issue IDs   │  SELECT from dirty_issues
├───────────────────────────┤
│  4. Load all issues       │  Full export (deterministic)
├───────────────────────────┤
│  5. Write to temp file    │  Atomic write pattern
├───────────────────────────┤
│  6. Compute content hash  │  SHA-256 of content
├───────────────────────────┤
│  7. Atomic rename         │  temp -> issues.jsonl
├───────────────────────────┤
│  8. Clear dirty flags     │  DELETE from dirty_issues
└───────────────────────────┘
```

---

## Storage Layer

### SqliteStorage

The primary storage implementation uses bundled stock SQLite through a narrow
synchronous facade. The facade retains `fsqlite-types` and `fsqlite-error` as
its public value/error vocabulary, so the storage and sync layers remain
stable while the runtime engine is isolated behind one module.

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
2. Atomic JSONL publication (temp file + rename)
3. Content hashing (detect corruption)
4. History backups (optional, created when overwriting JSONL inside `.beads/`)

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
4. User config          (~/.config/beads/config.yaml; falls back to ~/.config/bd/config.yaml)
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

### Atomic JSONL Export Writes

JSONL export publication uses temp + rename:

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

### Workspace Health Contract

The workspace health contract answers one question consistently across startup,
`doctor`, write recovery, and sync status:

> Given the current `.beads/` state, what is authoritative, what is derived,
> what may be rebuilt automatically, and what must never be discarded or
> silently normalized away?

This contract is intentionally stricter than "can the command proceed right
now." The goal is to make every surface classify the same workspace with the
same vocabulary and the same recovery envelope.

### Health States

| State | Meaning | Expected system posture |
|-------|---------|-------------------------|
| `healthy` | SQLite, JSONL, metadata, and derived state agree closely enough for normal operation | Proceed normally; no repair messaging |
| `drifted` | Primary and interchange state are both readable, but freshness or path metadata disagree | Report drift explicitly; prefer import/export reconciliation over repair |
| `degraded-recoverable` | Primary storage is damaged or incomplete, but authoritative evidence exists to rebuild safely | Preserve evidence, rebuild only through the allowed repair path, then re-verify |
| `quarantined` | State is unsafe to mutate automatically because the authority source is ambiguous or itself damaged | Refuse risky mutation, preserve artifacts, require operator intervention |

### Authority Model

| State family | Examples | Authority level | Why |
|-------------|----------|-----------------|-----|
| Primary data | `issues`, `dependencies`, `labels`, `comments` tables; semantically equivalent JSONL issue records | Authoritative | These describe the actual issue graph and cannot be silently discarded |
| Interchange data | `.beads/issues.jsonl` plus its content hash / mtime witnesses | Authoritative interchange copy | This is the git-facing source used for rebuild/import/export decisions |
| Workspace metadata | `.beads/metadata.json`, config layers, sync timestamps/hashes in DB metadata | Control-plane evidence | Needed to resolve paths, detect drift, and explain why a workspace is classified a certain way |
| Derived state | `dirty_issues`, `export_hashes`, `blocked_issues_cache`, `child_counters`, stale markers | Rebuildable | These speed up operations or summarize state, but should never outrank authoritative issue data |

### Invariant Matrix

| Surface | Object | Required invariant | Allowed automatic action | Forbidden silent behavior |
|---------|--------|--------------------|--------------------------|---------------------------|
| Primary data | `issues` + relational tables | Issue rows, dependencies, labels, and comments must remain representable either in SQLite or in valid JSONL records | Rebuild SQLite from valid JSONL when the DB family is recoverably damaged | Dropping primary issue data because a derived table is inconsistent |
| Primary data | SQLite database family (`beads.db`, `-wal`, `-shm`, `-journal`) | The DB family must be treated as one unit when diagnosing corruption or recovery | Quarantine the whole family into `.beads/.br_recovery/` before rebuilding | Deleting or overwriting only one sidecar and pretending the rest are canonical |
| Interchange data | `.beads/issues.jsonl` | JSONL must be parseable, conflict-free, and prefix-consistent before import | Reject import and preserve the file for manual repair | Best-effort partial import of malformed or conflicted JSONL |
| Interchange data | DB vs JSONL freshness | Empty/stale export must never overwrite non-empty authoritative JSONL by accident | Refuse export unless the operator explicitly forces the destructive direction | Treating a missing import as permission to publish an empty snapshot |
| Metadata | `.beads/metadata.json` path mapping | Resolved DB + JSONL targets must point at the intended workspace and be explainable | Rehydrate missing defaults from the canonical workspace layout and config rules | Silently operating on a different workspace than the one diagnostics describe |
| Metadata | Sync witness keys (`last_import_time`, `last_export_time`, `jsonl_content_hash`, JSONL mtime witness) | Metadata must explain whether DB or JSONL is newer and whether divergence is expected | Recompute witness metadata after successful import/export | Claiming a workspace is healthy when witness data proves drift or missing export/import |
| Metadata | Prefix/config resolution | Effective prefix and safety-relevant config must be source-traceable | Surface the winning config layer in diagnostics | Forcing absent CLI bools or env overrides into false certainty |
| Derived state | `dirty_issues` | Dirty flags may lag but must never redefine issue truth | Recompute/clear after verified export | Using stale dirty flags as proof that data itself is corrupt |
| Derived state | `export_hashes` | Export hashes may be rebuilt from authoritative issue content | Regenerate during import/export finalization | Treating missing hashes as a reason to discard issue rows |
| Derived state | `blocked_issues_cache` | Cache may be stale; blocked truth comes from the dependency graph | Rebuild cache locally or after repair/import | Reporting cache staleness as unrecoverable workspace corruption |
| Derived state | `child_counters` and similar summaries | Summary tables must match authoritative parent/child relationships eventually | Recompute from primary graph state | Trusting counters over real dependency or parent-child edges |

### Primary-Data Repair Rules

These rules exist so tests and diagnostics can assert what is never allowed.

1. Primary issue data may only be replaced by a rebuild when there is a valid,
   authoritative interchange source to rebuild from.
2. Any rebuild of SQLite from JSONL must preserve the original DB family in
   `.beads/.br_recovery/` before replacement.
3. Row-level or index-level corruption that affects writes is classified as
   `degraded-recoverable`, not as permission to mutate around the bad row.
4. Prefix mismatch, malformed JSONL, or unresolved conflict markers promote the
   workspace to `quarantined` for import/rebuild purposes.

### Derived-State Rebuild Rules

Derived state can be repaired more aggressively because it is not the source of
truth, but only within the boundaries below.

1. `blocked_issues_cache` may be rebuilt from the current dependency graph.
2. `dirty_issues` and `export_hashes` may be cleared or regenerated only after
   a verified export/import transition.
3. Summary structures such as `child_counters` may be recomputed from the
   primary graph whenever authoritative issue rows are known-good.
4. Rebuilding derived state must not hide disagreement between SQLite, JSONL,
   and metadata. If primary/interchange drift remains, the workspace is still
   `drifted` or `quarantined`.

### Cross-Surface Reporting Contract

| Surface | Must report | Must not do silently |
|---------|-------------|----------------------|
| Startup / open | Whether the workspace is healthy, drifted, recoverable, or quarantined; whether an automatic DB rebuild was attempted; where evidence was preserved | Auto-rebuild from ambiguous or invalid JSONL, or collapse corruption into a generic `NOT_INITIALIZED` story |
| `br doctor` | Structural anomalies, JSONL integrity, metadata drift, sync witness disagreement, and whether repair is local-derived-state-only vs full DB rebuild | Emit a clean bill of health when another surface would reject the same workspace |
| Write recovery | Distinguish lock contention from corruption; identify when a mutation can retry once after rebuild | Retry blindly against uncertain state or persist partial side effects without surfacing them |
| `br sync --status` / export/import preflight | Which side is newer, whether divergence is safe, and whether the requested direction is destructive | Treat stale/empty export conditions as healthy just because files exist |

### Incident Evidence Bundle

Every real-world incident should be reducible to this bundle so future beads and
tests talk about the same evidence:

| Capture item | Why it is required |
|--------------|--------------------|
| Failing command plus exact stdout/stderr | Establishes the observed symptom and whether the failure happened at open, write, sync, or reporting time |
| `br doctor --json` | Gives the structured health classification surface for the same workspace |
| `br sync --status` | Shows freshness/drift direction between DB and JSONL |
| `br where` | Proves which workspace/database/JSONL paths were actually targeted |
| `br config list -v` | Preserves config provenance and environment overrides that changed behavior |
| `.beads/metadata.json` | Captures the explicit DB/JSONL routing contract the workspace claimed to use |
| `.beads/issues.jsonl` | Preserves the authoritative interchange copy used for taxonomy classification and rebuild decisions |
| Presence plus hashes of `beads.db`, `beads.db-wal`, `beads.db-shm`, and `beads.db-journal` when present | Distinguishes missing-file drift from sidecar mismatch and partial-copy failures |
| Directory listing of `.beads/`, `.beads/.br_recovery/`, and `.beads/.br_history/` | Preserves recovery artifacts and interrupted-operation evidence |
| Environment overrides and process context (`BD_DB`, `BD_DATABASE`, `BEADS_JSONL`, `BEADS_DIR`, `NO_COLOR`, active agents/processes) | Explains discovery/path/output drift and multi-actor contention |

This bundle is intentionally small enough to request in the first reply to a
field failure while still being sufficient to classify the failure against the
workspace taxonomy without speculative follow-up.

### File System Safety

1. **Sync writes confined to its validated `.beads/` authority by default**
   - Path validation before any write
   - External JSONL requires explicit opt-in

2. **No Git operations in sync**
   - `br sync` never runs `git` commands
   - User handles git manually

3. **Atomic JSONL publication**
   - Export uses a same-directory temp file + rename pattern
   - Other durable mutations use their own transaction/receipt contracts

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
| `rusqlite` + bundled SQLite | Runtime SQLite engine and connection facade |
| `fsqlite-types` + `fsqlite-error` | Stable shared storage values and error vocabulary |
| `serde` + `serde_json` | Serialization |
| `chrono` | Timestamps |
| `sha2` | Content hashing |
| `thiserror` + `anyhow` | Error types and context |
| `tracing` | Structured logging |
| `rich_rust` | Rich terminal UI components |
| `toon_rust` | TOON format support |
| `self_update` (optional) | Release-based self-update support |

---

## See Also

- [CLI_REFERENCE.md](CLI_REFERENCE.md) - Command reference
- [AGENT_INTEGRATION.md](AGENT_INTEGRATION.md) - AI agent guide
- [SYNC_SAFETY.md](SYNC_SAFETY.md) - Sync safety model
- [../AGENTS.md](../AGENTS.md) - Development guidelines
