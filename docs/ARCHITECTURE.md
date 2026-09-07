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
4. **Deterministic representations**: Stable content hashing and ID-sorted exports; creation IDs also depend on timestamps, actor metadata, and collision checks
5. **Safe**: Normal issue state stays in `.beads/`; explicit external database/JSONL routing and commands such as `br agents`, `config edit`, and `completions -o` have their own validated write targets

### Comparison with Go beads (bd)

| Feature | br (Rust) | bd (Go) |
|---------|-----------|---------|
| Lines of Code | ~243k (`src/**/*.rs`, `wc -l`, 2026-09-02; roughly half is inline `#[cfg(test)]`) | ~276k |
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

This is an abbreviated map. [AGENTS.md](../AGENTS.md#project-structure) carries
the module inventory checked by `tests/agents_md_contract.rs`.

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

This diagram shows the logical write path. Output timing and post-commit
auto-flush failures depend on the command branch; receiving an ID alone is not
proof that the default JSONL has been finalized.

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
    │  ID: br-abc123         │                        │                      │
    │ <───────────────────── │                        │                      │
```

### Sync Export

For `br sync --flush-only`, the full-export path in
[`src/sync/mod.rs`](../src/sync/mod.rs) has these conceptual stages:

1. Resolve and validate the target, acquire the relevant database/JSONL family
   authority, and retain the expected source generation.
2. Create an optional history backup and check empty/stale-export guards.
3. Read issue data and relations, record dirty-marker witnesses, and prepare
   ID-sorted JSONL with the configured tombstone retention/error policy.
4. Write, flush, sync, hash, and validate the staged file.
5. Publish conditionally against the retained source generation; verify the
   installed generation and, on exchange, the displaced generation.
6. Finalize the default export in a database transaction: verify the published
   receipt, reconcile exact issue hashes and dirty timestamps, and update sync
   metadata. Publication alone does not clear dirty flags.

These stages summarize the full export, not every incremental auto-flush or
merge-resume branch. The [publication section](#atomic-jsonl-export-writes)
describes failure states that a simple temp-file/rename diagram would omit.

---

## Storage Layer

### SqliteStorage

The primary storage implementation uses the fsqlite stack (`fsqlite`,
`fsqlite-types`, and `fsqlite-error`). The concrete `SqliteStorage` in
[`src/storage/sqlite.rs`](../src/storage/sqlite.rs) owns a `Connection`,
database-family write authority when applicable, an opener lease, and mutation,
policy, and temporary-storage state.

**Key Features:**

- **WAL Mode**: Concurrent reads during writes
- **Busy Timeout**: Normal CLI storage opening defaults to 30s; direct
  `SqliteStorage::open` uses `DEFAULT_BUSY_TIMEOUT_MS = 0`. The storage transaction
  retry loop and workspace write lock are separate mechanisms.
- **Transactional Mutations**: Issue changes, audit events, dirty tracking, and
  cache changes are coordinated by the mutation path

### Transaction Protocol

Issue mutations use `SqliteStorage::mutate`; bookkeeping also has dedicated
transaction paths. Its current signature is shown without the implementation:

```rust
pub fn mutate<F, R>(&mut self, op: &str, actor: &str, f: F) -> Result<R>
where
    F: FnMut(&Connection, &mut MutationContext) -> Result<R>;
```

The callback changes rows through the connection and records effects through
`MutationContext::record_event`, `mark_dirty`, and cache invalidation methods.
The callback can be retried on transient database contention, so it must not
perform external side effects that would be duplicated by a retry. The storage
layer persists the accumulated effects within the transaction.

### Database Schema

Table inventory, not executable DDL:

```text
-- Core tables
issues              -- Primary issue data
dependencies        -- Issue relationships
labels              -- Issue labels (many-to-many)
comments            -- Issue discussion threads
events              -- Audit log

-- Operational tables
dirty_issues            -- Changed since last export
blocked_issues_cache    -- Precomputed blocked status (see below)
export_hashes           -- Per-issue content hash at last export
child_counters          -- Next child ordinal per parent (dotted IDs)
config                  -- Key-value configuration
metadata                -- Workspace metadata (JSONL content hash, schema witness, ...)

-- Workflow policy tables (schema v15+)
gate_results            -- Current gate verdict per (issue, gate, provider)
gate_result_history     -- Append-only gate verdict history
close_metadata          -- Policy evidence recorded at close (bypass reason, gates fired)
capacity_occupancy      -- Capacity slot accounting
capacity_exemptions     -- Active capacity exemptions
capacity_exemption_history -- Append-only exemption audit
```

The authoritative list is `src/storage/schema.rs` (`CREATE TABLE IF NOT EXISTS`
statements); `CURRENT_SCHEMA_VERSION` is defined there.

### Dirty Tracking

Issues are marked dirty when:
- Created
- Updated (any field)
- Closed/reopened
- Dependencies added/removed
- Labels added/removed
- Comments added

Default-export finalization clears the dirty markers covered by its verified
publication and timestamp witnesses. An export to another path does not by
itself make the default interchange copy current.

### Blocked Cache

Precomputed table for fast `ready`/`blocked` queries:

```sql
-- src/storage/schema.rs
CREATE TABLE IF NOT EXISTS blocked_issues_cache (
    issue_id TEXT PRIMARY KEY,
    blocked_by TEXT NOT NULL,
    blocked_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_blocked_cache_blocked_at ON blocked_issues_cache(blocked_at);
```

Only blocked issues have a row; `blocked_by` stores a JSON array of blocker
references such as `br-abc:open` or `br-child:child-open`.
`rebuild_blocked_cache_impl` serializes the blocker list, and
`parse_blocked_by_json` validates it when read.

Status changes refresh the changed issues, their direct blocking dependents,
and those issues' complete parent-child components. Type changes also refresh
their component because epic open-child rollup depends on issue type. An atomic
close batch combines these affected sets and refreshes once inside its write
transaction; unrelated cache rows remain untouched.

Selective refresh requires an already fresh cache. A prior stale marker or full
invalidation requires a full rebuild. The mutation marks stale before a cache
savepoint; if selective refresh fails, successful rollback to that savepoint
preserves the primary mutation and schedules full repair after commit. A failed
savepoint boundary aborts the outer transaction. Reads compute blocked state
from the graph while stale without repairing the database. Deferred dependency
mutations retain their existing stale-marker and command-finalization path.

---

## Sync System

### JSONL Format

Each line is a complete issue JSON object. These abbreviated illustrations omit
fields and are **not importable JSONL**; use `br sync --flush-only` to obtain
actual records:

```text
{"id":"br-abc123","title":"Fix bug","status":"open",...}
{"id":"br-def456","title":"Add feature","status":"in_progress",...}
```

**Benefits:**
- Git-friendly (line-based diffs)
- Streamable (no need to parse entire file)
- Human-readable

### Export Process

Public signature excerpts from [`src/sync/mod.rs`](../src/sync/mod.rs), with
bodies and imports omitted:

```rust
pub fn export_to_jsonl(
    storage: &SqliteStorage,
    output_path: &Path,
    config: &ExportConfig,
) -> Result<ExportResult>;

pub fn finalize_export(
    storage: &mut SqliteStorage,
    result: &ExportResult,
    issue_hashes: Option<&[(String, String)]>,
    jsonl_path: &Path,
) -> Result<()>;
```

`export_to_jsonl_with_policy` additionally returns an `ExportReport`.
`finalize_export` is a separate step for successful default-path exports;
authority-aware command paths retain locks across these steps.

**Safety Guards:**

1. Path validation against the internal allowlist or authorized external JSONL
2. Empty/stale-export checks before replacing existing interchange data
3. Conditional publication with source identity and content witnesses
4. History backups (optional; an authorized external JSONL target is also
   eligible when the caller supplies the workspace/history configuration)

CLI callers supply `ExportConfig::beads_dir`. The lower-level API permits
`beads_dir: None`, which skips that path-validation step; its defaults are not
a substitute for the CLI's authority and path checks.

### Import Process

Public signature excerpt (body and imports omitted):

```rust
pub fn import_from_jsonl(
    storage: &mut SqliteStorage,
    input_path: &Path,
    config: &ImportConfig,
    expected_prefix: Option<&str>,
) -> Result<ImportResult>;
```

The importer captures the input as a retained JSONL source snapshot, rejects
conflict markers, normalizes records, and applies issue/relation changes through
the import transaction. `ImportConfig::beads_dir: None` likewise skips initial
path validation in the lower-level API.

**Matching and updates:**

- Imports are additive by default; existing DB-only issues are retained.
- Matching considers external references, IDs, and content hashes; timestamps
  govern ordinary updates. `force_upsert` permits equal/older incoming records
  to update matches; it does not disable every validation or tombstone guard.
- Prefix validation is optional at this API boundary. CLI sync supports mixed
  prefixes by default; an explicit expected-prefix check or prefix rewrite has
  its own validation rules.

### Path Validation

[`src/sync/path.rs`](../src/sync/path.rs) defines `ALLOWED_EXTENSIONS` as suffixes
without a leading dot: `db`, `db-wal`, `db-wal-cert`, `db-wal-cert-head`,
`db-shm`, `db-journal`, `db-fsqlite-ns-gate`, `db-fsqlite-ns-use`, `jsonl`, and
`jsonl.tmp`. Exact-name exceptions are `.manifest.json` and `metadata.json`;
PID-scoped `*.jsonl.<pid>.tmp` names are also recognized. Arbitrary `.json` and
`.yaml` files are not allowed by this sync validator.

`validate_sync_path` returns a `PathValidation` classification;
`is_sync_path_allowed` is its boolean convenience check.
`validate_sync_path_with_external(path, beads_dir, allow_external)` returns
`Result<()>`: internal paths retain containment, allowlist, and symlink checks,
while external JSONL requires authorization and still rejects traversal,
symlinks, non-regular existing files, and `.git` targets. An explicit external
database override can authorize its sibling JSONL through config routing;
otherwise CLI external JSONL use requires `--allow-external-jsonl`.

Path validation is preflight. Descriptor-based source capture and pinned-parent
publication checks remain necessary when paths can change during an operation.

---

## Configuration System

### Layer Hierarchy

Configuration sources in precedence order (highest wins):

```
1. CLI overrides        (--json, --db, --actor)
2. Environment layer    (BD_* keys and selected BEADS_* aliases)
3. Project config       (.beads/config.yaml)
4. User config          (~/.config/beads/config.yaml; falls back to ~/.config/bd/config.yaml)
5. Legacy user config   (~/.beads/config.yaml)
6. DB config table      (runtime keys only, when a storage handle is supplied)
7. JSONL inference      (issue_prefix from the first issue, when available)
8. Defaults
```

This is the merge order in `load_config_from_startup_layers` in
[`src/config/mod.rs`](../src/config/mod.rs). Startup path resolution happens
before that runtime merge: `load_startup_config_with_paths` resolves the
workspace, database, and JSONL paths from CLI overrides, startup layers,
metadata, and discovery rules. Variables such as `BEADS_JSONL` select paths
there; they are not all ordinary keys in `ConfigLayer::from_env`.

### Configuration Layer

Actual fields of `ConfigLayer` (derive attributes omitted):

```rust
pub struct ConfigLayer {
    pub startup: HashMap<String, String>, // File/env/CLI startup settings
    pub runtime: HashMap<String, String>, // Includes eligible DB settings
}
```

`merge_from` normalizes hyphens to underscores and lets higher-precedence layers
replace values. `ConfigLayer::get` checks runtime before startup keys;
`ConfigLayer::from_db` excludes keys classified by `is_startup_key`.

**Examples of startup-only keys** (the complete classifier is `is_startup_key`):

- `no-db`, `no-daemon`, `no-auto-flush`, `no-auto-import`, `no-history`
- `json`, `db`, `actor`, `identity`, `lock-timeout`
- `git.*`, `routing.*`, `sync.*`, `validation.*`, `display.*`, `directory.*`,
  `external-projects.*`

Classifying a legacy setting such as `no-daemon` does not create a daemon or an
automatic git execution path.

### Key Configuration Options

| Key | Default | Description |
|-----|---------|-------------|
| `issue_prefix` | `br` fallback | ID prefix; initialization or a higher layer normally supplies the workspace prefix |
| `default_priority` | `2` | Default priority (0-4) |
| `default_type` | `task` | Default issue type |
| `display.color` | auto | ANSI color output |
| `lock-timeout` | `30000` in normal CLI storage opening | Lock wait/busy timeout in milliseconds; direct storage APIs may choose another value |

`default_config_layer` inserts only `issue_prefix=br`. Priority and type defaults
come from `default_priority_from_layer` and `default_issue_type_from_layer`;
color remains unset for output-mode/terminal resolution unless overridden.

---

## Error Handling

### Error Types

Selected variants from [`src/error/mod.rs`](../src/error/mod.rs), with derive
attributes and other variants omitted. This is a type excerpt, not a complete
replacement enum:

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
    InvalidPriority { priority: String },

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

These are the categories in `ErrorCode::exit_code`, plus success. Individual
commands also have result-specific exits (for example, doctor's health verdict),
and argument-parser failures are not necessarily `StructuredError` responses.

| Code | Category | Description |
|------|----------|-------------|
| 0 | Success | Command completed |
| 1 | Internal | Unexpected error |
| 2 | Database | Missing/locked database, schema/database error, initialization state |
| 3 | Issue/operation | Missing/ambiguous/invalid ID, collision, nothing to do, incomplete close |
| 4 | Validation/policy | Invalid input, required field, policy violation, workflow capacity exceeded |
| 5 | Dependency | Cycle, self/duplicate/missing dependency, existing dependents |
| 6 | Sync | JSONL parse/prefix/import conflict, conflict markers, path traversal |
| 7 | Config | Configuration lookup/parse/error |
| 8 | I/O/serialization | File system, JSON, or YAML error |
| 130 | Shutdown | `SHUTTING_DOWN` |

### Structured Error Output

The top-level CLI handler in [`src/main.rs`](../src/main.rs) writes the JSON
error envelope to **stdout** in machine error mode. Logging and diagnostics go
to stderr; human-readable errors go to stderr. Inspect the exit status together
with the structured result. A partially successful batch may emit a result and
then an error document, so nonzero output is not universally one JSON document
and does not establish that nothing committed.

Captured from `br show nope-123 --json` on 2026-09-02 (exit code 3):

<!-- from: br show nope-123 --json -->
```json
{
  "error": {
    "code": "ISSUE_NOT_FOUND",
    "message": "Issue not found: nope-123",
    "hint": "Run 'br list' to see available issues.",
    "retryable": false,
    "context": {
      "searched_id": "nope-123"
    }
  }
}
```

`StructuredError::to_json` creates the outer `error` envelope. Its `code` is the
stable `ErrorCode::as_str` name, `message` describes the failure, and `hint` and
`context` are nullable remediation/context fields. `retryable` means retry may
be possible after the reported condition changes, including correcting input;
it does not promise that repeating an unchanged command will succeed. Preserve
commit/publication evidence in `context` and follow the hint before retrying
an uncertain mutation. `ErrorCode::exit_code` supplies the top-level handler's
exit status. `br schema error --format json` emits the `ErrorEnvelope` schema
from [`src/cli/commands/schema.rs`](../src/cli/commands/schema.rs).

---

## CLI Layer

### Command Structure

Uses Clap's derive macros. This abbreviated declaration omits attributes,
options, and subcommands; consult [`src/cli/mod.rs`](../src/cli/mod.rs) for the
complete types:

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

[`src/main.rs`](../src/main.rs) parses the CLI, resolves output/error modes and
startup overrides, and chooses the storage/authority path for the command. It
may preopen storage and auto-import changed JSONL before dispatch. Mutating
paths retain write authority and dispatch to command-specific `execute`
functions; their signatures differ.

The dispatcher routes errors through `handle_error(err, json_mode, color_mode)`.
Successful mutations with auto-flush enabled then publish pending JSONL through
the retained storage/authority context. Commands can also finalize their own
mutations and report partial success. This is a control-flow summary, not
copyable dispatch code; the actual branches handle routing, JSONL-only mode,
pending merges, and post-commit failures.

---

## Key Patterns

### ID Generation

Hash-based short IDs for human readability. Actual `IdConfig` fields, annotated
with defaults from [`src/util/id.rs`](../src/util/id.rs):

```rust
pub struct IdConfig {
    pub prefix: String,         // "br" fallback; workspace prefix can override
    pub min_hash_length: usize, // 3
    pub max_hash_length: usize, // 8
    pub max_collision_prob: f64, // 0.25
}

// Example shape: br-abc123
```

For ordinary hash IDs, `IdGenerator::generate` uses this algorithm:

1. `generate_id_seed` frames the title, optional description, optional creator,
   creation timestamp in nanoseconds, and a numeric nonce as length-prefixed
   UTF-8 text (`length:value`). Missing description/creator use empty strings.
2. `compute_id_hash` hashes that seed with SHA-256, interprets the first eight
   digest bytes as a big-endian integer, and encodes it in lowercase base36,
   padding/truncating to the requested length. No random bytes are drawn here.
3. `optimal_length(issue_count)` selects a starting length using a birthday
   collision-probability approximation over the base36 space, within the
   configured minimum/maximum. This estimate selects a length; the existence
   check decides whether an actual candidate is available.
4. Try nonces 0 through 9 at each length, extending after collisions. An error
   from the caller's existence lookup propagates instead of treating the ID as
   free.
5. If the configured maximum is exhausted, try the 12-character fallback with
   nonces 0 through 2000, checking the final candidate too. Return `IdCollision`
   only when those fallback candidates are also occupied.

The same complete seed produces the same candidate, but repeated CLI creates
need not have the same timestamp or available IDs. Slug IDs use
`generate_with_slug`, with normalization and a hash-only fallback; dotted child
IDs use the separate child-counter path. These IDs are distinct from the issue
content hash below.

### Content Hashing

`Issue::compute_content_hash` delegates to `src/util/hash.rs`. It hashes stable,
ordered issue fields with SHA-256, encoding each UTF-8 field as its unsigned
64-bit little-endian byte length followed by the field bytes. Length prefixes
prevent embedded NULs from moving field boundaries; this deliberately differs
from classic bd's NUL-separated hash format. Schema v14 rebuilt stored hashes.
`tests/proptest_hash.rs` checks an independent length-prefixed reference writer,
including the embedded-NUL collision regression.

**Excluded from hash:**
- `id` (generated)
- `created_at`, `updated_at` (timestamps)
- `labels`, `dependencies`, `comments` (relations)

The complete included/excluded field lists live beside the implementation in
`src/util/hash.rs`; a content hash is not a digest of every issue field or a
substitute for the JSONL publication witness.

### Atomic JSONL Export Writes

`publish_staged_jsonl_conditionally` in [`src/sync/mod.rs`](../src/sync/mod.rs)
publishes under `JsonlFamilyWriteLock`, using pinned parent/name handles and the
expected previous source witness. Depending on the target/platform, the
namespace operation is create-without-replacement, exchange-and-verify, or
replacement under authority. The publisher checks the installed bytes/identity
against the staged generation and checks any displaced generation against the
retained source witness.

Directory synchronization, post-publication authority checks, and checked
cleanup follow the namespace change. A platform that cannot certify directory
durability is reported explicitly; other synchronization failures can produce
`JsonlPublishedButNotDurable`. A conflicting or unverified installed/displaced
generation produces `JsonlPublicationConflict` or
`JsonlPublishedButUnwitnessed`, with recovery evidence when available. Checked
cleanup can retain a displaced file and report a warning rather than silently
discard evidence.

A plain `File::create` plus `fs::rename` helper would omit these contracts.
Atomic namespace change, verified publication, database finalization, and
power-loss durability are separate facts; a post-publication failure must not
be treated as proof that the target is unchanged or that the whole mutation is
safe to repeat.

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

The vocabulary below is `WorkspaceHealth` in `src/health.rs` (`healthy`,
`degraded`, `recoverable`, `unsafe`); every surface uses these four strings.

| State | Meaning | Expected system posture |
|-------|---------|-------------------------|
| `healthy` | SQLite, JSONL, metadata, and derived state agree closely enough for normal operation | Proceed normally; no repair messaging |
| `degraded` | Operable, but something disagrees: freshness or path metadata differ, derived state is stale, or a non-fatal anomaly was observed | Report the anomaly explicitly; prefer import/export reconciliation over repair |
| `recoverable` | Primary storage is damaged or incomplete, but authoritative evidence exists to rebuild safely | Preserve evidence, rebuild only through the allowed repair path, then re-verify |
| `unsafe` | State is unsafe to mutate automatically because the authority source is ambiguous or itself damaged | Refuse risky mutation, preserve artifacts, require operator intervention |

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
| Primary data | SQLite database family (database plus engine sidecars; see the [engine inventory](reliability/ENGINE_OPERATING_MODEL.md)) | The DB family must be treated as one unit when diagnosing corruption or recovery | Preserve the family before an authorized rebuild; a namespace diagnostic alone does not authorize quarantine | Deleting or overwriting only one sidecar and pretending the rest are canonical |
| Interchange data | `.beads/issues.jsonl` | JSONL must be parseable, conflict-free, and valid for the requested import mode; mixed prefixes are supported by default | Reject invalid import and preserve the file for manual repair | Best-effort partial import of malformed or conflicted JSONL |
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
   `recoverable`, not as permission to mutate around the bad row.
4. Malformed JSONL or unresolved conflict markers prevent automatic
   import/rebuild. Reject a prefix mismatch when that import explicitly requires
   a matching prefix; a valid mixed-prefix workspace is not unsafe merely
   because its IDs use different prefixes.

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
   `degraded` or `unsafe`.

### Cross-Surface Reporting Contract

| Surface | Must report | Must not do silently |
|---------|-------------|----------------------|
| Startup / open | Whether the workspace is healthy, degraded, recoverable, or unsafe; whether an automatic DB rebuild was attempted; where evidence was preserved | Auto-rebuild from ambiguous or invalid JSONL, or collapse corruption into a generic `NOT_INITIALIZED` story |
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
| Presence plus hashes of the complete database family from the [engine inventory](reliability/ENGINE_OPERATING_MODEL.md), including persistent namespace sidecars when present | Distinguishes missing-file drift from sidecar mismatch and partial-copy failures |
| Directory listing of `.beads/`, `.beads/.br_recovery/`, and `.beads/.br_history/` | Preserves recovery artifacts and interrupted-operation evidence |
| Environment overrides and process context (`BD_DB`, `BD_DATABASE`, `BEADS_JSONL`, `BEADS_DIR`, `NO_COLOR`, active agents/processes) | Explains discovery/path/output drift and multi-actor contention |

This bundle is intentionally small enough to request in the first reply to a
field failure while still being sufficient to classify the failure against the
workspace taxonomy without speculative follow-up.

### File System Safety

1. **Sync writes confined to its validated `.beads/` authority by default**
   - Path validation before any write
   - External JSONL requires authorization through the external-JSONL option or
     explicit external-database routing

2. **No Git operations in sync**
   - `br sync` never runs `git` commands
   - User handles git manually

3. **Atomic JSONL publication**
   - Export stages a file and conditionally publishes against retained source
     and directory authority, then verifies the resulting generation
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

Extend the validation performed by `IssueValidator::validate` in
[`src/validation/mod.rs`](../src/validation/mod.rs). Its actual entry point is
an associated function taking `&Issue` and returning
`Result<(), Vec<ValidationError>>`; it accumulates field-specific failures.
There is no `validate_custom_field` extension method to call. Add the rule to
that existing path and test accepted input, rejected input, and its boundary.

---

## Dependencies

| Crate | Purpose |
|-------|---------|
| `clap` | CLI parsing with derive macros |
| `fsqlite` + `fsqlite-types` + `fsqlite-error` | SQLite engine facade plus shared storage types/errors |
| `serde` + `serde_json` | Serialization |
| `chrono` | Timestamps |
| `sha2` | Content hashing |
| `thiserror` + `anyhow` | Error types and context |
| `tracing` | Structured logging |
| `rich_rust` | Rich terminal UI components |
| `toon_rust` | TOON format support |
| `self_update` (optional) | Release-based self-update support |

---

## As built (2026-09-03)

The sections above describe the design; this one records what the tree
actually is, so nobody has to re-derive it from the planning documents under
`docs/porting/` and `docs/plans/` (those are historical; their banners point
here). Line counts are `wc -l` over `*.rs` on 2026-09-03 at commit
`b4430d11` (`tokei` is not installed on the build hosts).

| Area | Files | Lines | What lives there |
|---|---|---|---|
| `src/cli/commands/` | 45 + `doctor_subsystems/` (11) | 102,932 | One file per subcommand (47 top-level subcommands). `doctor.rs` alone is 25,690 lines; `doctor_subsystems/` (15,376) holds the chokepoint, capabilities, explain, engine block, selftest, schema migration, and the incident bundle. `sync.rs` is 7,535. |
| `src/storage/` | 4 | 47,677 | `sqlite.rs` (38,571) is the whole engine facade: CRUD, filtered queries, dependency graph, labels, comments, events, integrity probes, database-family snapshots. `schema.rs` carries DDL migrations. |
| `src/sync/` | 5 | 33,827 | `mod.rs` (25,032): JSONL export/import, merge with pending-merge receipts, additive reconcile, source witnesses, write authority. `db_inode_lock.rs` is the sole-opener lease. |
| `src/config/` | 2 | 13,741 | Layered config, known-key registry, startup/open/recovery orchestration (`open_sqlite_storage_with_recovery_strategy`, sidecar quarantines). |
| `src/mcp/` | 4 | 9,520 | `br serve`: 7 tools, resources, prompts (`mcp` feature). |
| `src/close_policy.rs`, `policy.rs`, `coordination.rs`, `health.rs`, `inheritance.rs` | 5 | 9,200 | Workflow gates and capacity, policy documents, stale-claim diagnosis, health vocabulary, inherited context. |
| `src/model/`, `error/`, `format/`, `output/`, `util/`, `validation/` | 29 | 18,582 | Types, structured errors, plain/JSON/TOON/CSV formatting, Rich components, ids/hashes/time, input validation. |
| `src/main.rs`, `cli/mod.rs`, `lib.rs`, `franken_sync.rs`, `shutdown.rs`, `logging.rs` | 6 | 9,247 | Startup, write-lock acquisition and dispatch; clap definitions; the synchronous facade over the async FrankenSQLite API; cooperative shutdown. |
| dormant (`cache.rs`, `write_combining.rs`, `format/{rich,syntax,theme}.rs`, two output components) | 7 | 5,015 | Counted inside the rows above. See the decision table below; deletion is an operator decision. |
| `src/` total | 113 | 248,278 | |
| `tests/` | 154 integration files + `common/` | 142,914 | Sharded by `scripts/test-shard.sh` (`lib`, `e2e-a-l`, `e2e-m-z`, `storage`, `misc`, `bench`). |

**Why there is no `Storage` trait.** The porting plan specified a ~45-method
`pub trait Storage`. The tree has one engine by design (FrankenSQLite, no C
SQLite, no FFI; see `docs/reliability/ENGINE_OPERATING_MODEL.md`), so the trait
would have exactly one implementation and every test would still run against
that engine. The engine-independent check that a trait was meant to enable is
done differently: `tests/model_based_storage.rs` compares `SqliteStorage`
against a `BTreeMap` reference model after every operation.

**Why the monoliths exist.** `sqlite.rs`, `sync/mod.rs`, and `doctor.rs` grew
around the 2026-08 corruption incidents, where the fix surface (write
authority, receipts, witnesses, quarantines, migrations) had to move together.
Splitting them along the planned `storage/{issues,deps,labels,queries}` lines
is mechanical but touches thousands of lines of `pub(crate)` plumbing and
every unit-test module inside them; it buys navigability, not behavior. It is
listed as an optional Track G bead and has not been scheduled.

**What guards the boundaries today.** `tests/agents_md_contract.rs` fails when
a module is added without an `AGENTS.md` row; the doctor's read-only contract
(`storage::sqlite::database_family_read_only_diffs`, GitHub #476) is the one
place that defines which bytes a read-only open may touch; the release and
CI reliability jobs run the failure-injection matrix and the multi-process
stress script.

**Operator decision outstanding (bead wqmw.6 / bridge plan §8.4):** keep the
as-built layout, or schedule the isomorphic split as a Track G bead.

## Dormant Modules (decision table, 2026-09-02)

Reference counts are `rg` over `src/` excluding the module's own file and its
`pub use` re-export lines. Removal of any file needs the operator's written
approval (AGENTS.md rule 1); nothing below has been deleted.

| Module | Lines | Live callers | Decision | Reason / revisit trigger |
|---|---|---|---|---|
| `src/write_combining.rs` | 2,911 | none in `src/`; `tests/bench_contention_replay.rs` only | REMOVE (operator) | Design artifact per `docs/WRITE_COMBINING_QUEUE_DESIGN.md`; the classifier it contains is never consulted by a command. Archive the design doc; drop the bench with it. |
| `src/cache.rs` | 641 | none | REMOVE (operator) | Zero references anywhere. |
| `src/format/rich.rs` | 543 | none | REMOVE (operator) | Superseded by `src/output/components/*` (the Rich path every command uses). |
| `src/format/syntax.rs` | 388 | none (re-exported names unused) | REMOVE (operator) | Syntax highlighting was the RICH plan item deferred to a removal decision; `br show` renders Markdown through `rich_rust` without it. |
| `src/format/theme.rs` | 306 | none (`format::Theme` unused; `output::Theme` has 103 uses) | REMOVE (operator) | Duplicate of `src/output/theme.rs`. |
| `src/output/components/dep_tree.rs` | 176 | none (`DependencyTree` unused) | REMOVE (operator) | `br dep tree` renders through `commands/dep.rs`. |
| `src/output/components/stats.rs` | 50 | none (`StatsPanel` unused) | REMOVE (operator) | `br stats` renders in `commands/stats.rs`. |
| `src/format/markdown.rs` | 546 | `contains_markdown` (2) | KEEP | `br show` gates Rich Markdown rendering on it; `render_markdown`/`escape_markdown` are unused and can go when the file is next touched. |
| `src/output/components/progress.rs` | 171 | `ProgressTracker` (4) | KEEP | In use. |
| `OutputContext::error_panel` | fn | none | REMOVE (code cleanup, no approval needed) | Dead method; delete when `output/context.rs` is next edited. |

The stale `#[allow(dead_code)] // WP1 scaffold` markers on
`DoctorRepairSession` were removed on 2026-09-02 (every method has callers).
A linter that enforces this table was considered and parked: the table is
the decision record, and the compiler already flags unused private items.

## See Also

- [CLI_REFERENCE.md](CLI_REFERENCE.md) - Command reference
- [AGENT_INTEGRATION.md](AGENT_INTEGRATION.md) - AI agent guide
- [SYNC_SAFETY.md](SYNC_SAFETY.md) - Sync safety model
- [../AGENTS.md](../AGENTS.md) - Development guidelines
