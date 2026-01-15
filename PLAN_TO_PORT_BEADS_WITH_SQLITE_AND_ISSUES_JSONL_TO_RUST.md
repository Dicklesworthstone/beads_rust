# Plan: Port Beads (Classic SQLite + JSONL) to Rust

> **Project:** beads_rust
> **Binary Name:** `br` (to complement `bd` from Go beads and `bv` from beads_viewer)
> **Author:** Port initiated by Jeffrey Emanuel
> **Status:** Planning Phase

---

## Executive Summary

This document outlines the comprehensive plan to port the "classic" beads issue tracker from Go to hyper-optimized Rust. The Go version (authored by Steve Yegge) is being modified to use Dolt as the primary backend, which fundamentally changes its architecture. This port preserves the elegant SQLite + JSONL hybrid design that integrates seamlessly with git-based workflows.

### Key Design Philosophy: Non-Invasive

**`br` will be LESS invasive than `bd`.** The Go `bd` tool automatically installs git hooks, manipulates git config, and performs various automatic operations. We explicitly choose a simpler approach:

- **No automatic git hook installation** — Users manually add hooks if desired
- **No automatic git operations** — No auto-commit, no auto-push
- **No daemon/RPC architecture** — Simple CLI only, no background processes
- **Explicit over implicit** — Every git operation requires explicit user command
- **Minimal footprint** — Just a binary and a `.beads/` directory

### Why Port to Rust?

1. **Preserve the Classic Architecture:** The SQLite + JSONL hybrid design is elegant and integrates perfectly with git. The move to Dolt changes this fundamentally.

2. **Performance:** Rust's zero-cost abstractions enable significant performance improvements:
   - Faster startup time (no Go runtime initialization)
   - Smaller binary size (LTO + strip)
   - Better memory efficiency (no GC pauses)

3. **Consistency with Flywheel Tools:** The "agentic coding flywheel" ecosystem includes other Rust tools (xf, cass). A Rust beads will integrate more naturally.

4. **Bug Fixes and Optimization:** The port provides an opportunity to fix existing bugs and apply optimization lessons learned from xf and cass.

5. **Single Binary Distribution:** Rust produces truly standalone binaries with no runtime dependencies.

---

## Background: Legacy Beads (bd)

### What Is Beads?

Beads is an **agent-first issue tracker** designed for AI coding workflows. Unlike traditional issue trackers (Jira, Linear, GitHub Issues), beads is optimized for:

- **Local-first operation:** All data lives in `.beads/` directory within your project
- **Git-native sync:** JSONL export enables git-based collaboration without external services
- **Dependency-aware:** First-class support for blocking relationships and ready-work queries
- **Machine-readable:** JSON output mode for programmatic access by AI agents

### Core Architecture (Pre-Gastown)

```
.beads/
├── beads.db          # SQLite database (primary storage)
└── issues.jsonl      # JSONL export (git-tracked, human-readable)
```

**Two-Store Hybrid Design:**

1. **SQLite (beads.db):** Fast queries, ACID transactions, relational integrity
2. **JSONL (issues.jsonl):** Git-friendly format for sync, merge, and history

**Sync Flow:**
- On write: SQLite updated → issue marked dirty → JSONL export triggered
- On pull: JSONL newer than DB → import into SQLite (last-write-wins merge)

### Key Data Model

| Entity | Purpose |
|--------|---------|
| **Issue** | Core work item with ~30 fields (title, description, status, priority, type, etc.) |
| **Dependency** | Directed edge between issues (blocks, parent-child, related, etc.) |
| **Label** | Arbitrary tags for categorization |
| **Event** | Audit trail of all changes (status transitions, field updates, comments) |

**Status Workflow:**
```
open → in_progress → blocked → closed
         ↓              ↓
      deferred      (can return to open)
```

**Priority Levels:** P0 (critical) → P1 (high) → P2 (medium) → P3 (low) → P4 (backlog)

**Issue Types:** task, bug, feature, epic, chore, docs, question

### Key Commands (Pre-Gastown)

| Command | Purpose |
|---------|---------|
| `bd init` | Initialize `.beads/` directory |
| `bd create` | Create new issue |
| `bd update` | Modify issue fields |
| `bd close` | Close one or more issues |
| `bd list` | List issues with filters |
| `bd show` | Show issue details |
| `bd ready` | Show unblocked work (no open blockers) |
| `bd blocked` | Show blocked issues |
| `bd dep add/remove` | Manage dependencies |
| `bd label add/remove` | Manage labels |
| `bd sync` | Export to JSONL |
| `bd stale` | Find stale issues |
| `bd duplicates` | Find duplicate issues |
| `bd orphans` | Find orphan issues |
| `bd graph` | Visualize dependencies |
| `bd comments` | Add/view comments |

### What Gastown Changed

The Go `bd` tool was later extended with "Gastown" features for multi-agent coordination:
- Agent identity tracking (agent, role types)
- Work molecules, rigs, convoys
- Gates for coordination
- HOP (Handoff Protocol) fields
- Session management

**These additions nearly doubled the codebase complexity.** This port explicitly targets the simpler, pre-Gastown architecture.

---

## Background: Beads Viewer (bv)

### What Is Beads Viewer?

**Beads Viewer (`bv`)** is a separate Go tool that provides:

1. **Interactive TUI:** Beautiful terminal interface with Vim-style navigation
2. **Graph Analysis:** 9 rigorous algorithms for dependency analysis
3. **Robot Mode:** Deterministic JSON output for AI agents (`--robot-*` flags)

**Critical distinction:** `bv` is **read-only**. It reads `.beads/issues.jsonl` but never writes to it.

### Division of Labor

| Tool | Role | Writes? | Key Capability |
|------|------|---------|----------------|
| **bd/br** | Issue lifecycle management | ✅ Yes | Create, update, close, sync |
| **bv** | Analysis and visualization | ❌ No | Graph metrics, triage, planning |

**This means `br` should NOT duplicate `bv` functionality.** Specifically, `br` should NOT implement:

- Dependency tree visualization (use `bv --robot-graph`)
- PageRank/betweenness/centrality metrics (use `bv --robot-insights`)
- Critical path analysis (use `bv --robot-insights`)
- Cycle detection (use `bv --robot-insights`)
- Triage recommendations (use `bv --robot-triage`)
- Burndown/forecasting (use `bv --robot-burndown/forecast`)
- Label health analysis (use `bv --robot-label-health`)

### What bv Provides (So br Doesn't Have To)

**Graph Algorithms (9 metrics):**

| Metric | What It Measures |
|--------|------------------|
| PageRank | Recursive dependency importance (what unblocks the most work?) |
| Betweenness | Bottleneck detection (what bridges clusters?) |
| HITS | Hub/Authority duality (epics vs utilities) |
| Critical Path | Longest chain (minimum time to completion) |
| Eigenvector | Influence via neighbors |
| Degree | Direct connection counts |
| Density | Project coupling health |
| Cycles | Circular dependency detection |
| Topological Sort | Valid execution order |

**Robot Commands (AI agent interface):**

```bash
bv --robot-triage          # THE mega-command: everything an agent needs
bv --robot-next            # Just the top recommendation
bv --robot-plan            # Parallel execution tracks
bv --robot-insights        # Full graph metrics
bv --robot-priority        # Priority suggestions
bv --robot-diff --diff-since HEAD~10  # Changes since ref
bv --robot-graph --graph-format=mermaid  # Dependency graph export
```

**TUI Views:**

| Key | View | Purpose |
|-----|------|---------|
| `l` | List | Scrollable list with fuzzy search |
| `b` | Board | Kanban columns |
| `g` | Graph | Interactive dependency DAG |
| `i` | Insights | 6-panel metrics dashboard |
| `a` | Actionable | Parallel execution plan |

### Scope Boundary

> **`br` handles issue lifecycle (CRUD, sync).**
> **`bv` handles analysis (metrics, visualization, triage).**

This separation keeps both tools focused and avoids duplication.

---

## Architecture Overview

### What We're Porting

```
Go beads (classic)                    →    beads_rust (br)
├── internal/storage/sqlite/          →    src/storage/
├── internal/types/                   →    src/model/
├── cmd/bd/                           →    src/cli/ + src/main.rs
├── internal/export/                  →    src/export/
├── internal/config/                  →    src/config/
└── internal/git/                     →    (MINIMAL: only for repo detection, NOT auto-operations)
```

**Note:** We do NOT port `internal/hooks/` (no automatic hook installation) or `internal/rpc/` (no daemon).

### What We're NOT Porting

| Component | Reason |
|-----------|--------|
| `internal/storage/dolt/` | The entire point of this port is to avoid Dolt |
| `internal/rpc/` | RPC daemon adds unnecessary complexity |
| `internal/linear/` | Linear integration is non-essential |
| `internal/jira.go` | Jira integration is non-essential |
| `claude-plugin/` | MCP plugin is separate; port core CLI first |
| `internal/hooks/` | No automatic hook installation (non-invasive design) |
| **Gastown features** | See explicit exclusion list below |

### Gastown Features — EXPLICITLY EXCLUDED

Recent additions to `bd` support the "Gastown" multi-agent coordination system. These add significant complexity and are **NOT part of this port**:

#### Excluded Issue Types
- `gate` — Coordination gates for agent workflows
- `agent` — Agent identity tracking
- `role` — Role definitions for agents
- `molecule` — Work unit groupings
- `rig` — Agent configuration units
- `convoy` — Multi-agent coordination groups

#### Excluded Fields (from Issue struct)
- `agent_id`, `agent_name`, `agent_type` — Agent identity
- `hop_*` fields — HOP (Handoff Protocol) support
- `molecule_*` fields — Molecule coordination
- `gate_*` fields — Gate state tracking
- `convoy_*` fields — Convoy coordination
- `rig_*` fields — Rig configuration
- `external_agent_*` — External agent references
- `session_id` — Session tracking for agents
- `workflow_*` fields — Workflow orchestration

#### Excluded Tables
- `agents` — Agent registry
- `molecules` — Work molecules
- `gates` — Coordination gates
- `rigs` — Agent rigs
- `convoys` — Multi-agent convoys
- `workflow_*` tables — Workflow state

#### Excluded Commands
- `bd gate *` — Gate management
- `bd agent *` — Agent management
- `bd molecule *` — Molecule operations
- `bd rig *` — Rig configuration
- `bd convoy *` — Convoy orchestration
- `bd hop *` — Handoff protocol
- `bd session *` — Session management

**Rationale:** These features add ~40% of the codebase complexity but serve a specific multi-agent orchestration use case (Gastown). The core issue tracking functionality is valuable on its own. Gastown features can be added to a future `br2` or as optional modules if needed.

### Reference Projects

This port follows patterns from two sibling Rust CLI projects:

| Project | Location | Key Patterns to Adopt |
|---------|----------|----------------------|
| **xf** | `/data/projects/xf` | Tantivy search, SQLite pragmas, clap derive CLI |
| **cass** | `/data/projects/coding_agent_session_search` | Custom error types, streaming indexing, robot mode |

---

## Data Model

### Core Types (from Go `internal/types/types.go`)

The Issue struct is the heart of beads. Here's the Rust equivalent:

```rust
// src/model/issue.rs

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    // Core Identification
    pub id: String,
    #[serde(skip)]
    pub content_hash: String,

    // Issue Content
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub design: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acceptance_criteria: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,

    // Status & Workflow
    #[serde(default)]
    pub status: Status,
    pub priority: i32,  // 0-4 (P0=critical, P4=backlog)
    #[serde(default)]
    pub issue_type: IssueType,

    // Assignment
    #[serde(skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,

    // Timestamps
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub close_reason: Option<String>,

    // Labels and Dependencies (populated for export)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<Dependency>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
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
    Hooked,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
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
    // NOTE: Gate, Agent, Role, Molecule, Rig, Convoy are Gastown types — EXCLUDED
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Dependency {
    pub issue_id: String,
    pub depends_on_id: String,
    #[serde(rename = "type")]
    pub dep_type: DependencyType,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum DependencyType {
    Blocks,
    ParentChild,
    Related,
    DiscoveredFrom,
    RepliesTo,
    RelatesTo,
    Duplicates,
    Supersedes,
}
```

### Content Hashing

Issues have deterministic content hashes for deduplication (see `ComputeContentHash` in Go):

```rust
// src/model/hash.rs

use sha2::{Sha256, Digest};

impl Issue {
    pub fn compute_content_hash(&self) -> String {
        let mut hasher = Sha256::new();

        // Hash fields in stable order
        hasher.update(self.title.as_bytes());
        hasher.update(&[0u8]); // null separator

        if let Some(desc) = &self.description {
            hasher.update(desc.as_bytes());
        }
        hasher.update(&[0u8]);

        hasher.update(format!("{:?}", self.status).as_bytes());
        hasher.update(&[0u8]);

        hasher.update(self.priority.to_string().as_bytes());
        hasher.update(&[0u8]);

        // ... continue for all substantive fields

        format!("{:x}", hasher.finalize())
    }
}
```

---

## Storage Layer

### SQLite Backend

Following the patterns from xf and cass, with pragmas optimized for the beads workload:

```rust
// src/storage/mod.rs

use rusqlite::{Connection, OpenFlags};
use std::path::Path;

pub struct Storage {
    conn: Connection,
    path: String,
}

impl Storage {
    pub fn open(db_path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            db_path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;

        // Performance pragmas (from xf)
        conn.execute_batch("
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;
            PRAGMA foreign_keys = ON;
            PRAGMA cache_size = -64000;     -- 64MB cache
            PRAGMA temp_store = MEMORY;
            PRAGMA busy_timeout = 30000;    -- 30s lock timeout
        ")?;

        let storage = Self {
            conn,
            path: db_path.to_string_lossy().into_owned(),
        };

        storage.migrate()?;
        Ok(storage)
    }

    pub fn open_readonly(db_path: &Path) -> Result<Self> {
        let conn = Connection::open_with_flags(
            db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;

        conn.execute_batch("
            PRAGMA cache_size = -32000;
            PRAGMA temp_store = MEMORY;
        ")?;

        Ok(Self {
            conn,
            path: db_path.to_string_lossy().into_owned(),
        })
    }
}
```

### Schema

The schema must be compatible with Go beads for potential cross-tool usage:

```sql
-- issues table
CREATE TABLE IF NOT EXISTS issues (
    id TEXT PRIMARY KEY,
    title TEXT NOT NULL,
    description TEXT,
    design TEXT,
    acceptance_criteria TEXT,
    notes TEXT,
    status TEXT NOT NULL DEFAULT 'open',
    priority INTEGER NOT NULL DEFAULT 2,
    issue_type TEXT NOT NULL DEFAULT 'task',
    assignee TEXT,
    owner TEXT,
    created_at TEXT NOT NULL,
    created_by TEXT,
    updated_at TEXT NOT NULL,
    closed_at TEXT,
    close_reason TEXT,
    external_ref TEXT,
    deleted_at TEXT,
    delete_reason TEXT
);

-- dependencies table
CREATE TABLE IF NOT EXISTS dependencies (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    issue_id TEXT NOT NULL,
    depends_on_id TEXT NOT NULL,
    type TEXT NOT NULL DEFAULT 'blocks',
    created_at TEXT NOT NULL,
    created_by TEXT,
    metadata TEXT,
    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE,
    UNIQUE(issue_id, depends_on_id, type)
);

-- labels table
CREATE TABLE IF NOT EXISTS labels (
    issue_id TEXT NOT NULL,
    label TEXT NOT NULL,
    PRIMARY KEY (issue_id, label),
    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
);

-- events table (audit trail)
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    issue_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    actor TEXT,
    old_value TEXT,
    new_value TEXT,
    comment TEXT,
    created_at TEXT NOT NULL,
    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
);

-- config table
CREATE TABLE IF NOT EXISTS config (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- metadata table (internal state)
CREATE TABLE IF NOT EXISTS metadata (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- dirty tracking for incremental export
CREATE TABLE IF NOT EXISTS dirty_issues (
    issue_id TEXT PRIMARY KEY,
    dirty_hash TEXT,
    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
);

-- export hash tracking
CREATE TABLE IF NOT EXISTS export_hashes (
    issue_id TEXT PRIMARY KEY,
    content_hash TEXT NOT NULL,
    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
);

-- Indexes for common queries
CREATE INDEX IF NOT EXISTS idx_issues_status ON issues(status);
CREATE INDEX IF NOT EXISTS idx_issues_priority ON issues(priority);
CREATE INDEX IF NOT EXISTS idx_issues_assignee ON issues(assignee);
CREATE INDEX IF NOT EXISTS idx_issues_type ON issues(issue_type);
CREATE INDEX IF NOT EXISTS idx_issues_updated ON issues(updated_at);
CREATE INDEX IF NOT EXISTS idx_dependencies_issue ON dependencies(issue_id);
CREATE INDEX IF NOT EXISTS idx_dependencies_depends ON dependencies(depends_on_id);
CREATE INDEX IF NOT EXISTS idx_events_issue ON events(issue_id);
```

### Migration Strategy

Version-tracked migrations following the Go pattern:

```rust
// src/storage/migrations.rs

const SCHEMA_VERSION: i32 = 1;

impl Storage {
    fn migrate(&self) -> Result<()> {
        let current_version = self.get_schema_version()?;

        if current_version < 1 {
            self.migrate_v1()?;
        }
        // Future migrations...

        self.set_schema_version(SCHEMA_VERSION)?;
        Ok(())
    }

    fn get_schema_version(&self) -> Result<i32> {
        match self.conn.query_row(
            "SELECT value FROM metadata WHERE key = 'schema_version'",
            [],
            |row| row.get::<_, String>(0),
        ) {
            Ok(v) => Ok(v.parse().unwrap_or(0)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(0),
            Err(e) => Err(e.into()),
        }
    }
}
```

---

## JSONL Export/Import

The JSONL layer is critical for git-based sync:

```rust
// src/export/jsonl.rs

use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::Path;

pub fn export_issues(storage: &Storage, path: &Path) -> Result<usize> {
    let issues = storage.get_all_issues_with_relations()?;
    let file = File::create(path)?;
    let mut writer = BufWriter::new(file);

    let mut count = 0;
    for issue in issues {
        // Skip tombstones past TTL
        if issue.is_expired_tombstone() {
            continue;
        }

        let line = serde_json::to_string(&issue)?;
        writeln!(writer, "{}", line)?;
        count += 1;
    }

    writer.flush()?;
    Ok(count)
}

pub fn import_issues(storage: &Storage, path: &Path) -> Result<ImportResult> {
    let file = File::open(path)?;
    let reader = BufReader::new(file);

    let mut created = 0;
    let mut updated = 0;
    let mut skipped = 0;

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let issue: Issue = serde_json::from_str(&line)?;

        match storage.get_issue(&issue.id)? {
            Some(existing) => {
                if existing.updated_at < issue.updated_at {
                    storage.update_issue_from_import(&issue)?;
                    updated += 1;
                } else {
                    skipped += 1;
                }
            }
            None => {
                storage.create_issue(&issue)?;
                created += 1;
            }
        }
    }

    Ok(ImportResult { created, updated, skipped })
}
```

---

## Local History Backup (.br_history/)

### Rationale

Git history captures state at commit time, but commits are episodic — they may miss intermediate states or occur after significant work is done. To protect against data loss, `br` maintains a local history of `issues.jsonl` snapshots that captures every export.

### Design

```
.beads/
├── beads.db              # SQLite database
├── issues.jsonl          # Current JSONL export (tracked in git)
└── .br_history/          # Local backup history (UNTRACKED)
    ├── issues.2025-01-15T10-30-00.jsonl
    ├── issues.2025-01-15T14-45-22.jsonl
    ├── issues.2025-01-16T09-00-00.jsonl
    └── ...
```

### Behavior

1. **On every export:** Before writing `issues.jsonl`, copy the current file to `.br_history/` with a timestamp suffix
2. **Filename format:** `issues.YYYY-MM-DDTHH-MM-SS.jsonl` (ISO 8601, filesystem-safe)
3. **Git ignored:** `.br_history/` is automatically added to `.gitignore` during `br init`
4. **Deduplication:** Skip backup if content hash matches the most recent backup (avoids identical copies from repeated syncs)
5. **Rotation policy:** Configurable via `br config`, with sensible defaults:
   - `history.max_count`: Maximum files to keep (default: 100)
   - `history.max_age_days`: Delete files older than N days (default: 30)
   - `history.enabled`: Enable/disable history (default: true)

### Implementation

```rust
// src/export/history.rs

use chrono::Utc;
use std::fs;
use std::path::Path;

pub struct HistoryConfig {
    pub enabled: bool,
    pub max_count: usize,
    pub max_age_days: u32,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_count: 100,
            max_age_days: 30,
        }
    }
}

pub fn backup_before_export(beads_dir: &Path, config: &HistoryConfig) -> Result<()> {
    if !config.enabled {
        return Ok(());
    }

    let history_dir = beads_dir.join(".br_history");
    fs::create_dir_all(&history_dir)?;

    let current_jsonl = beads_dir.join("issues.jsonl");
    if !current_jsonl.exists() {
        return Ok(()); // Nothing to backup yet
    }

    // Create timestamped backup
    let timestamp = Utc::now().format("%Y-%m-%dT%H-%M-%S");
    let backup_name = format!("issues.{}.jsonl", timestamp);
    let backup_path = history_dir.join(&backup_name);

    fs::copy(&current_jsonl, &backup_path)?;

    // Run rotation
    rotate_history(&history_dir, config)?;

    Ok(())
}

fn rotate_history(history_dir: &Path, config: &HistoryConfig) -> Result<()> {
    let mut entries: Vec<_> = fs::read_dir(history_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
        .collect();

    // Sort by modification time (newest first)
    entries.sort_by_key(|e| std::cmp::Reverse(e.metadata().ok().and_then(|m| m.modified().ok())));

    let cutoff = Utc::now() - chrono::Duration::days(config.max_age_days as i64);

    for (idx, entry) in entries.iter().enumerate() {
        let dominated = idx >= config.max_count;
        let expired = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .is_some_and(|t| chrono::DateTime::<Utc>::from(t) < cutoff);

        if dominated || expired {
            fs::remove_file(entry.path())?;
        }
    }

    Ok(())
}
```

### Recovery Commands

```bash
# List available history snapshots
br history list

# Show diff between current and a snapshot
br history diff issues.2025-01-15T10-30-00.jsonl

# Restore from a snapshot (imports into DB, re-exports to issues.jsonl)
br history restore issues.2025-01-15T10-30-00.jsonl

# Prune history manually
br history prune --keep 50 --older-than 7d
```

### Why Not Just Git?

| Scenario | Git | .br_history/ |
|----------|-----|--------------|
| Work uncommitted when disaster strikes | ❌ Lost | ✅ Preserved |
| Frequent small changes between commits | ❌ Missed | ✅ Captured |
| Recovery without git knowledge | ❌ Requires git | ✅ Simple file copy |
| Disk space | ✅ Compressed | ⚠️ Raw files (but small) |
| Cross-machine sync | ✅ Push/pull | ❌ Local only |

**Conclusion:** `.br_history/` complements git — it's a local safety net, not a replacement for proper version control.

---

## CLI Architecture

### Clap Derive Pattern (from xf)

```rust
// src/cli/mod.rs

use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "br")]
#[command(version = concat!(
    env!("CARGO_PKG_VERSION"),
    "\n  Built: ", env!("VERGEN_BUILD_TIMESTAMP"),
    "\n  Rustc: ", env!("VERGEN_RUSTC_SEMVER"),
))]
#[command(about = "Beads Rust - Agent-first issue tracker (SQLite + JSONL)")]
pub struct Cli {
    /// Path to .beads directory
    #[arg(long, env = "BEADS_DIR", global = true)]
    pub beads_dir: Option<PathBuf>,

    /// Output format
    #[arg(long, short = 'f', default_value = "text", global = true)]
    pub format: OutputFormat,

    /// JSON output (shorthand for --format json)
    #[arg(long, global = true)]
    pub json: bool,

    /// Verbose output
    #[arg(long, short = 'v', global = true)]
    pub verbose: bool,

    /// Quiet mode (suppress non-essential output)
    #[arg(long, short = 'q', global = true)]
    pub quiet: bool,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Initialize a new .beads directory
    Init(InitArgs),

    /// Create a new issue
    Create(CreateArgs),

    /// Update an existing issue
    Update(UpdateArgs),

    /// Close one or more issues
    Close(CloseArgs),

    /// List issues
    List(ListArgs),

    /// Show issue details
    Show(ShowArgs),

    /// Show ready work (unblocked issues)
    Ready(ReadyArgs),

    /// Show blocked issues
    Blocked(BlockedArgs),

    /// Manage dependencies
    Dep(DepArgs),

    /// Manage labels
    Label(LabelArgs),

    /// Search issues
    Search(SearchArgs),

    /// Show statistics
    Stats(StatsArgs),

    /// Sync with JSONL (export/import)
    Sync(SyncArgs),

    /// Health check and diagnostics
    Doctor(DoctorArgs),

    /// Manage configuration
    Config(ConfigArgs),
}

#[derive(Copy, Clone, Default, ValueEnum)]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
    Jsonl,
}
```

### Command Implementation Pattern

Each command follows a consistent pattern:

```rust
// src/cli/create.rs

use super::*;

#[derive(Parser)]
pub struct CreateArgs {
    /// Issue title
    #[arg(value_name = "TITLE")]
    pub title: String,

    /// Issue type
    #[arg(long = "type", short = 't', default_value = "task")]
    pub issue_type: IssueType,

    /// Priority (0=critical, 4=backlog)
    #[arg(long, short = 'p', default_value = "2")]
    pub priority: i32,

    /// Assignee
    #[arg(long, short = 'a')]
    pub assignee: Option<String>,

    /// Labels (comma-separated)
    #[arg(long, short = 'l', value_delimiter = ',')]
    pub labels: Vec<String>,

    /// Description (from file or stdin with -)
    #[arg(long, short = 'd')]
    pub description: Option<String>,

    /// Parent issue (creates parent-child dependency)
    #[arg(long)]
    pub parent: Option<String>,
}

pub fn execute(args: &CreateArgs, ctx: &Context) -> Result<()> {
    let storage = ctx.storage()?;

    let issue = Issue {
        id: generate_issue_id(&ctx.config.id_prefix),
        title: args.title.clone(),
        description: args.description.clone(),
        status: Status::Open,
        priority: args.priority,
        issue_type: args.issue_type,
        assignee: args.assignee.clone(),
        labels: args.labels.clone(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
        ..Default::default()
    };

    storage.create_issue(&issue, &ctx.actor)?;

    // Add parent-child dependency if specified
    if let Some(parent_id) = &args.parent {
        storage.add_dependency(&Dependency {
            issue_id: issue.id.clone(),
            depends_on_id: parent_id.clone(),
            dep_type: DependencyType::ParentChild,
            created_at: Utc::now(),
        }, &ctx.actor)?;
    }

    // Mark as dirty for export
    storage.mark_dirty(&issue.id)?;

    match ctx.format {
        OutputFormat::Json => {
            println!("{}", serde_json::to_string_pretty(&issue)?);
        }
        OutputFormat::Jsonl => {
            println!("{}", serde_json::to_string(&issue)?);
        }
        OutputFormat::Text => {
            println!("Created issue: {}", issue.id);
        }
    }

    Ok(())
}
```

---

## Error Handling

### Custom Error Type (from cass pattern)

```rust
// src/error.rs

use thiserror::Error;

#[derive(Error, Debug)]
pub enum BeadsError {
    // Storage errors
    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("Issue not found: {id}")]
    IssueNotFound { id: String },

    #[error("Duplicate issue ID: {id}")]
    DuplicateId { id: String },

    // Validation errors
    #[error("Validation error: {message}")]
    Validation { message: String },

    #[error("Invalid status: {status}")]
    InvalidStatus { status: String },

    #[error("Invalid priority: {priority} (must be 0-4)")]
    InvalidPriority { priority: i32 },

    // Dependency errors
    #[error("Cycle detected: {path}")]
    CycleDetected { path: String },

    #[error("Dependency not found: {issue_id} -> {depends_on}")]
    DependencyNotFound { issue_id: String, depends_on: String },

    // Config errors
    #[error("Config not found: {key}")]
    ConfigNotFound { key: String },

    #[error("Not initialized: run 'br init' first")]
    NotInitialized,

    // IO errors
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    // Context wrapper
    #[error("{context}: {source}")]
    WithContext {
        context: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

pub type Result<T> = std::result::Result<T, BeadsError>;

// CLI-specific error for exit codes
#[derive(Debug)]
pub struct CliError {
    pub code: i32,
    pub kind: &'static str,
    pub message: String,
    pub hint: Option<String>,
}

impl From<BeadsError> for CliError {
    fn from(err: BeadsError) -> Self {
        match &err {
            BeadsError::IssueNotFound { .. } => CliError {
                code: 2,
                kind: "not_found",
                message: err.to_string(),
                hint: Some("Check the issue ID with 'br list'".into()),
            },
            BeadsError::NotInitialized => CliError {
                code: 3,
                kind: "not_initialized",
                message: err.to_string(),
                hint: Some("Run 'br init' to initialize a .beads directory".into()),
            },
            BeadsError::Validation { .. } => CliError {
                code: 4,
                kind: "validation",
                message: err.to_string(),
                hint: None,
            },
            _ => CliError {
                code: 1,
                kind: "error",
                message: err.to_string(),
                hint: None,
            },
        }
    }
}
```

---

## Project Structure

```
beads_rust/
├── Cargo.toml
├── rust-toolchain.toml
├── build.rs                    # Build metadata (vergen)
├── src/
│   ├── main.rs                 # Entry point
│   ├── lib.rs                  # Library root
│   ├── error.rs                # Error types
│   ├── config.rs               # Configuration
│   ├── model/
│   │   ├── mod.rs
│   │   ├── issue.rs            # Issue struct
│   │   ├── dependency.rs       # Dependency struct
│   │   ├── event.rs            # Event struct
│   │   └── hash.rs             # Content hashing
│   ├── storage/
│   │   ├── mod.rs
│   │   ├── schema.rs           # Schema definition
│   │   ├── migrations.rs       # Schema migrations
│   │   ├── issues.rs           # Issue CRUD
│   │   ├── dependencies.rs     # Dependency operations
│   │   ├── labels.rs           # Label operations
│   │   ├── events.rs           # Event logging
│   │   ├── queries.rs          # Complex queries (ready, blocked)
│   │   └── config.rs           # Config/metadata storage
│   ├── export/
│   │   ├── mod.rs
│   │   ├── jsonl.rs            # JSONL export/import
│   │   └── history.rs          # Local backup history (.br_history/)
│   ├── cli/
│   │   ├── mod.rs              # CLI definition
│   │   ├── init.rs
│   │   ├── create.rs
│   │   ├── update.rs
│   │   ├── close.rs
│   │   ├── list.rs
│   │   ├── show.rs
│   │   ├── ready.rs
│   │   ├── blocked.rs
│   │   ├── dep.rs
│   │   ├── label.rs
│   │   ├── search.rs
│   │   ├── stats.rs
│   │   ├── sync.rs
│   │   ├── doctor.rs
│   │   ├── config.rs
│   │   └── history.rs          # History list/diff/restore/prune
│   └── git/
│       └── mod.rs              # Minimal git: repo detection, branch name only
├── tests/
│   ├── integration_test.rs     # Full pipeline tests
│   ├── conformance_test.rs     # bd vs br comparison tests
│   └── cli_e2e.rs              # CLI end-to-end tests
├── benches/
│   └── storage_perf.rs         # Performance benchmarks
├── beads/                      # Go reference (read-only)
└── AGENTS.md
```

---

## Cargo.toml

```toml
[package]
name = "beads_rust"
version = "0.1.0"
edition = "2024"
rust-version = "1.85"
description = "Agent-first issue tracker (SQLite + JSONL)"
license = "MIT"
repository = "https://github.com/Dicklesworthstone/beads_rust"
keywords = ["cli", "issue-tracker", "sqlite", "agent"]
categories = ["command-line-utilities", "development-tools"]

[[bin]]
name = "br"
path = "src/main.rs"

[dependencies]
# CLI
clap = { version = "4.5", features = ["derive", "env"] }
clap_complete = "4.5"

# Database
rusqlite = { version = "0.32", features = ["bundled", "modern_sqlite"] }

# Serialization
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"

# Time
chrono = { version = "0.4", features = ["serde"] }

# Hashing
sha2 = "0.10"

# Error handling
anyhow = "1.0"
thiserror = "2.0"

# Logging
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }

# Terminal
colored = "2.1"
indicatif = "0.17"

# Utilities
once_cell = "1.19"
rayon = "1.10"

[build-dependencies]
vergen-gix = { version = "1.0", features = ["build", "cargo", "rustc"] }

[dev-dependencies]
tempfile = "3.10"
assert_cmd = "2.0"
predicates = "3.1"
criterion = { version = "0.5", features = ["html_reports"] }

[[bench]]
name = "storage_perf"
harness = false

[profile.release]
opt-level = "z"
lto = true
codegen-units = 1
panic = "abort"
strip = true

[profile.dev]
opt-level = 1

[lints.rust]
unsafe_code = "forbid"

[lints.clippy]
pedantic = { level = "warn", priority = -1 }
nursery = { level = "warn", priority = -1 }
```

---

## New Features Beyond bd Parity

These features do NOT exist in legacy beads and represent genuine additions in `br`:

### 1. Local History Backup (.br_history/)

**Already documented above.** Automatic timestamped backups of `issues.jsonl` with rotation.

### 2. Bulk Update Operations

Legacy `bd` can batch-close issues but cannot batch-update other fields.

```bash
# Update multiple issues at once
br update bd-1 bd-2 bd-3 --status in_progress
br update bd-1 bd-2 bd-3 --assignee alice --priority 1
br update bd-1 bd-2 bd-3 --add-label urgent
```

**Implementation:**
- Accept multiple issue IDs as positional arguments
- Apply the same field changes to all
- Report per-issue success/failure
- Mark all as dirty for export

### 3. Saved Queries (Named Filters)

No persistent filter storage in legacy beads. Users must re-type complex queries.

```bash
# Save a query
br query save my-bugs --assignee=me --type=bug --status=open
br query save urgent-backend --label=backend --priority=0,1

# Run a saved query
br query run my-bugs
br query run urgent-backend --format=json

# List saved queries
br query list

# Delete a query
br query delete my-bugs
```

**Implementation:**
- Store in `config` table as JSON: `{"name": "my-bugs", "filters": {...}}`
- Support all list/ready/blocked filter flags
- Compose with additional flags at runtime

### 4. CSV Export

Legacy beads exports JSON/JSONL only. CSV is useful for:
- Spreadsheet users (Excel, Google Sheets)
- Non-technical stakeholders
- Quick data analysis

```bash
# Full export
br export --format=csv > issues.csv

# Selected fields
br export --format=csv --fields=id,title,status,priority,assignee

# With filters
br list --status=open --format=csv
```

**Implementation:**
- Use `csv` crate for proper escaping
- Default fields: id, title, status, priority, type, assignee, created_at, updated_at
- Optional: include description (may have newlines)

### 5. Changelog Generation

Generate release notes from closed issues, grouped by type.

```bash
# Since date
br changelog --since 2025-01-01

# Since git tag
br changelog --since-tag v1.0.0

# Since commit
br changelog --since-commit abc123

# Output formats
br changelog --since-tag v1.0.0 --format=markdown
br changelog --since-tag v1.0.0 --format=json
```

**Example Output:**
```markdown
## Changelog (v1.0.0 → HEAD)

### Features
- Add dark mode support (bd-45)
- Implement user preferences (bd-52)

### Bug Fixes
- Fix login crash on empty password (bd-67)
- Resolve race condition in sync (bd-71)

### Chores
- Update dependencies (bd-80)
```

**Implementation:**
- Query issues where `closed_at > since_date`
- Group by `issue_type`
- Sort by priority within groups
- Support markdown and JSON output

---

## Implementation Phases

### Phase 1: Foundation (MVP)

**Goal:** Basic CRUD operations that can read/write JSONL compatible with Go beads.

**Deliverables:**
- [ ] Project scaffolding (Cargo.toml, rust-toolchain.toml, src/ structure)
- [ ] Data models (Issue, Dependency, Event, Status, IssueType)
- [ ] Content hashing (compatible with Go implementation)
- [ ] SQLite storage layer with schema
- [ ] Basic migrations
- [ ] JSONL export (issues.jsonl)
- [ ] JSONL import
- [ ] **Local history backup (.br_history/)** — automatic rotation by count/age
- [ ] CLI commands: init, create, show, list
- [ ] Error handling framework

**Validation:** Run `br list --json` and `bd list --json` on the same .beads directory; output should be semantically identical.

### Phase 2: Core Commands

**Goal:** Feature parity with essential Go beads commands.

**Deliverables:**
- [ ] update command (single issue)
- [ ] **bulk update** (multiple issues: `br update bd-1 bd-2 --status X`)
- [ ] close command (single and batch)
- [ ] ready command (unblocked issues)
- [ ] blocked command
- [ ] dep command (add, remove)
- [ ] label command (add, remove)
- [ ] stats command
- [ ] search command (SQLite FTS or basic LIKE)
- [ ] **history command (list, diff, restore, prune)**
- [ ] Dirty tracking for incremental export

**Validation:** Comprehensive conformance tests comparing bd and br outputs.

### Phase 3: Sync and Config

**Goal:** JSONL sync and configuration management (non-invasive).

**Deliverables:**
- [ ] sync command (export to JSONL only — NO auto-git operations)
- [ ] Import from JSONL on explicit command
- [ ] config command
- [ ] doctor command (health checks)
- [ ] Repo detection (find .beads/ directory)
- [ ] **Saved queries** (`br query save/run/list/delete`)
- [ ] **CSV export** (`br export --format=csv`)

**Note:** No automatic git commit/push. No hook installation. User runs `git add/commit` manually.

**Validation:** Can use br alongside bd for core issue tracking workflows.

### Phase 4: Optimization

**Goal:** Make br faster than bd for all operations.

**Deliverables:**
- [ ] Performance benchmarks
- [ ] Query optimization
- [ ] Batch operations
- [ ] Memory profiling
- [ ] Binary size optimization
- [ ] Startup time optimization

**Validation:** Benchmark suite showing br performance vs bd.

### Phase 5: Polish and Extensions

**Goal:** Production-ready with nice-to-have features.

**Deliverables:**
- [ ] Shell completions (bash, zsh, fish, PowerShell)
- [ ] **Changelog generation** (`br changelog --since-tag v1.0.0`)
- [ ] Markdown rendering in `br show` (glamour-style)
- [ ] Color themes
- [ ] Install scripts

**Note:** No TUI — that's `bv`'s domain.

---

## Conformance Testing Strategy

To ensure br is a true drop-in replacement for bd:

```rust
// tests/conformance_test.rs

use std::process::Command;
use tempfile::TempDir;

#[test]
fn test_create_conformance() {
    let temp = TempDir::new().unwrap();
    let beads_dir = temp.path().join(".beads");

    // Initialize with bd
    Command::new("bd")
        .args(["init", "--dir", beads_dir.to_str().unwrap()])
        .status()
        .unwrap();

    // Create with br
    let br_output = Command::new("br")
        .args(["create", "Test issue", "-p", "1", "--json"])
        .env("BEADS_DIR", &beads_dir)
        .output()
        .unwrap();

    // List with bd
    let bd_output = Command::new("bd")
        .args(["list", "--json"])
        .current_dir(&temp)
        .output()
        .unwrap();

    // Parse and compare
    let br_issue: serde_json::Value = serde_json::from_slice(&br_output.stdout).unwrap();
    let bd_list: serde_json::Value = serde_json::from_slice(&bd_output.stdout).unwrap();

    // Verify br's issue appears in bd's list
    // ... comparison logic
}
```

---

## Migration Path for Existing Users

For users switching from bd to br:

1. **No migration needed:** br reads the same `.beads/` directory structure
2. **Parallel operation:** Can use both bd and br on the same project
3. **Gradual adoption:** Switch individual commands as comfortable
4. **Fallback:** bd remains available if issues arise

---

## Success Criteria

1. **Functional parity:** br handles all common bd workflows
2. **Output compatibility:** JSON output matches bd for same inputs
3. **Performance:** br is faster than bd for all measured operations
4. **Binary size:** br binary is smaller than bd binary
5. **Reliability:** Zero data loss in all tested scenarios

---

## Timeline and Effort

This is a multi-session project. Rough estimates per phase:

| Phase | Scope | Sessions |
|-------|-------|----------|
| Phase 1 | Foundation | 2-3 |
| Phase 2 | Core Commands | 3-4 |
| Phase 3 | Sync/Git | 2-3 |
| Phase 4 | Optimization | 2-3 |
| Phase 5 | Polish | 2-3 |
| **Total** | | **11-16 sessions** |

---

## Open Questions

1. **ID format:** Should we generate IDs identically to bd, or is semantic equivalence sufficient?
2. **Schema version:** How do we handle schema differences if we diverge from bd?
3. **Full-text search:** Should we add Tantivy for better search, or stay with SQLite FTS?

### Resolved Questions

- ~~**Daemon:** Do we need the RPC daemon?~~ **NO** — Non-invasive design, CLI only
- ~~**Git hooks:** Auto-install hooks?~~ **NO** — Users add manually if desired
- ~~**Gastown features:** Port agent/molecule/gate/rig/convoy?~~ **NO** — Explicitly excluded

---

## References

- **Go beads source:** `./legacy_beads/` (gitignored, reference only)
- **xf source:** `/data/projects/xf`
- **cass source:** `/data/projects/coding_agent_session_search`
- **Flywheel documentation:** `/data/projects/agentic_coding_flywheel_setup`
- **beads_viewer (bv):** Companion TUI for beads analysis
- **Architecture deep dive:** `./EXISTING_BEADS_STRUCTURE_AND_ARCHITECTURE.md`

---

## Conclusion

This port preserves the elegant simplicity of beads' SQLite + JSONL hybrid architecture while bringing the performance and reliability benefits of Rust. By following proven patterns from xf and cass, we can create a hyper-optimized `br` binary that serves as a drop-in replacement for `bd` in the agentic coding flywheel.
