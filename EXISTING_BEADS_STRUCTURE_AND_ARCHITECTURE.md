# Existing Beads Structure and Architecture

> Comprehensive specification of the Go beads codebase for porting to Rust.
> This document serves as the complete reference for the Rust port - consult this instead of Go source files.

---

## Table of Contents

0. [Working TODO (Keep Current)](#0-working-todo-keep-current)
1. [Project Overview](#1-project-overview)
2. [Directory Structure](#2-directory-structure)
3. [Data Types and Models](#3-data-types-and-models)
4. [Storage Interface Specification](#4-storage-interface-specification)
5. [SQLite Storage Implementation](#5-sqlite-storage-implementation)
6. [CLI Commands Specification](#6-cli-commands-specification)
7. [JSONL Import/Export System](#7-jsonl-importexport-system)
8. [Ready/Blocked Logic and Dependency Graph](#8-readyblocked-logic-and-dependency-graph)
9. [Configuration System](#9-configuration-system)
10. [Validation Rules](#10-validation-rules)
11. [ID Generation and Content Hashing](#11-id-generation-and-content-hashing)
12. [Key Architectural Patterns](#12-key-architectural-patterns)
13. [Error Handling](#13-error-handling)
14. [Porting Considerations](#14-porting-considerations)
15. [Additional Legacy Findings (2026-01-16)](#15-additional-legacy-findings-2026-01-16)

---

## 0. Working TODO (Keep Current)

This is the **live, granular checklist** for completing the legacy-beads deep-dive and porting spec.
Update it as soon as new work is discovered or completed.

### Completed (this session)

- [x] Confirmed no existing TODO list in this document (added this section).
- [x] Reconciled **staleness detection vs auto-import** semantics (mtime/Lstat vs content hash).
- [x] Documented **auto-import / auto-flush edge cases** (hash metadata, last_import_time, cold-start prefix, debounce).
- [x] Documented **delete/tombstone workflow** (single vs batch, actor/reason, reference rewriting, hard-delete pruning).
- [x] Added **non-classic CLI command flag matrix** with explicit exclusion notes.
- [x] Documented **lint, markdown bulk create, info, version, and template commands** with JSON schemas.
- [x] Documented **graph/where/quick/prime/help/preflight commands** with port notes.
- [x] Documented **sync workflow deep dive** (pull-first flow, sync-branch, integrity checks).
- [x] Documented **JSONL content merge driver** (tombstone TTL, conflict rules, dependency merge).
- [x] Documented **export error policies + manifest** (strict/best-effort/partial/required-core).
- [x] Documented **doctor/cleanup/compact** maintenance commands (excluded but specified).
- [x] Cross-verified core JSON output shapes against CLI behavior/tests (list/show/ready/blocked/search/stats/count/stale/dep/label/comments).
- [x] Added explicit **orphans** command output schema and UX details (JSON + text).
- [x] Clarified count/grouping ordering and JSON ordering guarantees.
- [x] Captured show JSON array behavior + compatibility note (tests accept object).

### Session checklist (expanded deep-dive tasks)

- [x] Add error/exit-code matrix for **core commands** (JSON vs text; stdout vs stderr; exit codes).
- [x] Add **golden text output snapshots** for list/show/dep tree/ready/blocked.
- [x] Expand **non-invasive exclusions** with explicit do-not-port behaviors and commands.
- [x] Add **storage invariants & transactional guarantees** (indexes, constraints, dirty markers).
- [x] Expand **ID/prefix routing edge cases** (validation, rename-prefix, cross-prefix dup rules).

### Open TODOs (keep granular + current)

- [ ] **Reconcile doc vs issue tracker**:
  - [ ] Verify whether previously marked “completed” items are fully covered and close any redundant beads issues.
  - [ ] If gaps remain, link them to a bead ID and leave explicit sub-tasks here.
- [ ] **Sync + merge driver integration** (beads_rust-f0g):
  - [ ] Confirm merge-driver install in `bd init` (git config + `.gitattributes` edits).
  - [ ] Document `resolve-conflicts` backup/cleanup behavior in detail.
  - [ ] Capture sync-branch mass-delete thresholds + config keys.
- [ ] **Hierarchy + templates + epics validation** (beads_rust-hwb):
  - [ ] Re-verify epic status/close JSON shapes against tests.
  - [ ] Confirm template semantics in hierarchy (epic+label conventions).
- [ ] **Integrations/automation JSON shapes** (beads_rust-17u):
  - [ ] Hooks install/uninstall/list outputs.
  - [ ] Daemon status/health JSON shapes (if still present in legacy).
  - [ ] Gate/mol/agent/swarm outputs (for exclusion clarity).
  - [ ] Linear/Jira sync outputs (excluded but documented).
- [ ] **Create-form interactive workflow** (beads_rust-yfq):
  - [ ] Capture prompt order, defaults, and JSON output (if any).
- [ ] **Merge-driver scope decision for br** (beads_rust-o27):
  - [ ] Decide opt‑out vs keep (if no git ops).
  - [ ] If keep: expand merge schema to full issue model to avoid data loss.

---

## 1. Project Overview

**Location:** `./legacy_beads/` (gitignored reference copy)

**Statistics:**
- ~267,622 lines of Go code
- ~100 files in SQLite storage alone
- 40 database migrations
- 15+ CLI commands with extensive flag sets
- 62+ storage interface methods

**Core Architecture:**
- SQLite + JSONL hybrid storage
- Optional daemon mode with RPC (not porting initially)
- Content-addressable issues with hash-based IDs
- Git-integrated synchronization
- Non-invasive design philosophy (no auto git hooks, no daemon required)

**Key Design Principles (br vs bd):**
- **No automatic git hooks** — Users add hooks manually if desired
- **No automatic git operations** — No auto-commit, no auto-push
- **No daemon/RPC** — Simple CLI only, no background processes
- **Explicit over implicit** — Every git operation requires explicit user command

---

## 2. Directory Structure

```
legacy_beads/
├── beads.go                    # Package root, version info
├── cmd/
│   └── bd/                     # CLI entry point (~100 files)
│       ├── main.go             # Entry point, Cobra root command
│       ├── create.go           # Issue creation
│       ├── update.go           # Issue updates
│       ├── close.go            # Issue closing
│       ├── list.go             # Issue listing
│       ├── show.go             # Issue details
│       ├── ready.go            # Ready work queries
│       ├── blocked.go          # Blocked issues queries
│       ├── dep.go              # Dependency management
│       ├── label.go            # Label management
│       ├── search.go           # Full-text search
│       ├── stats.go            # Statistics
│       ├── sync.go             # Git synchronization
│       ├── config.go           # Configuration
│       ├── init.go             # Initialize workspace
│       ├── export.go           # JSONL export
│       ├── import.go           # JSONL import
│       ├── autoflush.go        # Auto-export logic
│       ├── autoimport.go       # Auto-import logic
│       ├── daemon*.go          # Daemon mode (SKIP)
│       └── ...
├── internal/
│   ├── types/                  # Core data types
│   │   ├── types.go            # Issue, Dependency, etc. (42KB)
│   │   ├── id_generator.go     # Hash-based ID generation
│   │   ├── lock.go             # Lock types
│   │   └── validation.go       # Validation helpers
│   ├── storage/
│   │   ├── storage.go          # Storage interface (10KB, 62+ methods)
│   │   ├── sqlite/             # SQLite implementation (PORT THIS)
│   │   │   ├── store.go        # Main storage struct
│   │   │   ├── schema.go       # Database schema
│   │   │   ├── queries.go      # SQL queries
│   │   │   ├── issues.go       # Issue CRUD
│   │   │   ├── dependencies.go # Dependency operations
│   │   │   ├── labels.go       # Label operations
│   │   │   ├── comments.go     # Comment operations
│   │   │   ├── events.go       # Event/audit operations
│   │   │   ├── ready.go        # Ready work queries
│   │   │   ├── blocked.go      # Blocked cache management
│   │   │   ├── dirty_issues.go # Dirty tracking
│   │   │   ├── config.go       # Config storage
│   │   │   ├── metadata.go     # Metadata storage
│   │   │   ├── export_hashes.go# Export hash tracking
│   │   │   ├── migrations/     # 40 migrations
│   │   │   │   ├── 001_dirty_issues_table.sql
│   │   │   │   ├── ...
│   │   │   │   └── 040_quality_score_column.sql
│   │   │   └── ...
│   │   ├── dolt/               # Dolt backend (DO NOT PORT)
│   │   ├── memory/             # In-memory backend (for testing)
│   │   └── factory/            # Backend factory
│   ├── export/                 # JSONL export logic
│   ├── autoimport/             # Auto-import from JSONL
│   ├── importer/               # Import logic with collision detection
│   ├── compact/                # JSONL compaction
│   ├── configfile/             # Configuration file handling
│   ├── validation/             # Input validation
│   ├── hooks/                  # Hook system (SKIP for br)
│   ├── git/                    # Git integration
│   ├── rpc/                    # RPC daemon (SKIP initially)
│   ├── linear/                 # Linear.app integration (SKIP)
│   └── ui/                     # Terminal UI helpers
└── docs/                       # Documentation
```

### 2.1 Workspace Layout (.beads)

The runtime workspace lives in a `.beads/` directory at the repo root. The files below are the ones `bd` expects or produces. This matters for `br` because the Rust port needs to read and write the same files (or intentionally ignore them).

**Core files:**
- `beads.db` - primary SQLite database (filename is configurable via `metadata.json`)
- `issues.jsonl` - canonical JSONL export (one issue per line)
- `beads.jsonl` - legacy JSONL name (read-only fallback if `issues.jsonl` absent)
- `metadata.json` - startup config (database path, JSONL export name, backend)
- `config.yaml` - user config loaded by viper (project-scoped, YAML only)
- `.local_version` - gitignored version marker for `bd doctor` compatibility

**Sync / merge artifacts (temporary, should be ignored by import):**
- `beads.base.jsonl`, `beads.left.jsonl`, `beads.right.jsonl` - 3-way merge snapshots
- `beads.base.meta.json`, `beads.left.meta.json`, `beads.right.meta.json` - snapshot metadata

**Legacy / deprecated files (still encountered in older repos):**
- `deletions.jsonl` - legacy deletion manifest (superseded by inline tombstones)
- `deletions.jsonl.migrated` - archived legacy deletions after migration

**Optional / advanced:**
- `.exclusive-lock` - external tool lock file (causes daemon to skip this DB)
- `hooks/` - git hook scripts (non-invasive port should not auto-create)
- `interactions.jsonl` - legacy interactions/audit trail (not used by classic port)
- `molecules.jsonl` - templates (Gastown; excluded from classic port)

**JSONL selection rules (important):**
1. Prefer `issues.jsonl` if it exists.
2. Otherwise use `beads.jsonl`.
3. Never treat `deletions.jsonl`, `interactions.jsonl`, or merge snapshots as the main JSONL.

---

## 3. Data Types and Models

### 3.1 Issue Struct (Primary Entity)

The `Issue` struct is the primary data entity. For the Rust port, we exclude Gastown-specific fields (agent, molecule, gate, rig, convoy, HOP features).

**Fields to Port:**

```go
type Issue struct {
    // === Core Identification ===
    ID          string `json:"id"`           // Hash-based ID (e.g., "bd-abc123")
    ContentHash string `json:"-"`            // SHA256, NOT exported to JSONL

    // === Content Fields ===
    Title              string `json:"title"`                         // Required, max 500 chars
    Description        string `json:"description,omitempty"`
    Design             string `json:"design,omitempty"`
    AcceptanceCriteria string `json:"acceptance_criteria,omitempty"`
    Notes              string `json:"notes,omitempty"`

    // === Status & Workflow ===
    Status    Status    `json:"status,omitempty"`      // open, in_progress, blocked, closed, etc.
    Priority  int       `json:"priority"`              // 0-4 (P0-P4), NO omitempty (0 is valid)
    IssueType IssueType `json:"issue_type,omitempty"`  // task, bug, feature, epic, etc.

    // === Assignment ===
    Assignee         string `json:"assignee,omitempty"`
    Owner            string `json:"owner,omitempty"`           // Git author email for attribution
    EstimatedMinutes *int   `json:"estimated_minutes,omitempty"`

    // === Timestamps ===
    CreatedAt       time.Time  `json:"created_at"`
    CreatedBy       string     `json:"created_by,omitempty"`
    UpdatedAt       time.Time  `json:"updated_at"`
    ClosedAt        *time.Time `json:"closed_at,omitempty"`
    CloseReason     string     `json:"close_reason,omitempty"`
    ClosedBySession string     `json:"closed_by_session,omitempty"`  // Claude Code session ID

    // === Time-Based Scheduling ===
    DueAt      *time.Time `json:"due_at,omitempty"`       // When issue should complete
    DeferUntil *time.Time `json:"defer_until,omitempty"`  // Hide from bd ready until

    // === External Integration ===
    ExternalRef  *string `json:"external_ref,omitempty"`   // e.g., "gh-9", "jira-ABC"
    SourceSystem string  `json:"source_system,omitempty"`  // Federation source identifier

    // === Compaction Metadata ===
    CompactionLevel   int        `json:"compaction_level,omitempty"`   // 0=none, 1=minor, 2=major
    CompactedAt       *time.Time `json:"compacted_at,omitempty"`
    CompactedAtCommit *string    `json:"compacted_at_commit,omitempty"`
    OriginalSize      int        `json:"original_size,omitempty"`       // Bytes before compaction

    // === Internal Routing (NOT exported to JSONL) ===
    SourceRepo     string `json:"-"`  // Which repo owns this issue
    IDPrefix       string `json:"-"`  // Override prefix for ID generation
    PrefixOverride string `json:"-"`  // Replace config prefix entirely

    // === Relational Data ===
    Labels       []string      `json:"labels,omitempty"`
    Dependencies []*Dependency `json:"dependencies,omitempty"`
    Comments     []*Comment    `json:"comments,omitempty"`

    // === Soft-Delete (Tombstone) ===
    DeletedAt    *time.Time `json:"deleted_at,omitempty"`
    DeletedBy    string     `json:"deleted_by,omitempty"`
    DeleteReason string     `json:"delete_reason,omitempty"`
    OriginalType string     `json:"original_type,omitempty"`  // Type before deletion

    // === Messaging/Ephemeral ===
    Sender    string `json:"sender,omitempty"`     // For message-type issues
    Ephemeral bool   `json:"ephemeral,omitempty"`  // If true, not exported to JSONL

    // === Context Markers ===
    Pinned     bool `json:"pinned,omitempty"`      // Persistent context marker
    IsTemplate bool `json:"is_template,omitempty"` // Read-only template
}
```

**Fields NOT to Port (Gastown features):**

```go
// DO NOT PORT - Agent Identity Fields
HookBead     string     `json:"hook_bead,omitempty"`
RoleBead     string     `json:"role_bead,omitempty"`
AgentState   AgentState `json:"agent_state,omitempty"`
LastActivity *time.Time `json:"last_activity,omitempty"`
RoleType     string     `json:"role_type,omitempty"`
Rig          string     `json:"rig,omitempty"`

// DO NOT PORT - Molecule/Work Type
MolType  MolType  `json:"mol_type,omitempty"`
WorkType WorkType `json:"work_type,omitempty"`

// DO NOT PORT - Gate Fields (Async Coordination)
AwaitType string        `json:"await_type,omitempty"`
AwaitID   string        `json:"await_id,omitempty"`
Timeout   time.Duration `json:"timeout,omitempty"`
Waiters   []string      `json:"waiters,omitempty"`
Holder    string        `json:"holder,omitempty"`

// DO NOT PORT - HOP Fields (Entity Tracking)
Creator      *EntityRef   `json:"creator,omitempty"`
Validations  []Validation `json:"validations,omitempty"`
QualityScore *float32     `json:"quality_score,omitempty"`
Crystallizes bool         `json:"crystallizes,omitempty"`

// DO NOT PORT - Event Fields
EventKind string `json:"event_kind,omitempty"`
Actor     string `json:"actor,omitempty"`
Target    string `json:"target,omitempty"`
Payload   string `json:"payload,omitempty"`

// DO NOT PORT - Bonding (Compound Molecules)
BondedFrom []BondRef `json:"bonded_from,omitempty"`
```

### 3.2 Status Enum

```go
const (
    StatusOpen       Status = "open"        // Default status for new issues
    StatusInProgress Status = "in_progress" // Work has begun
    StatusBlocked    Status = "blocked"     // Explicitly marked blocked (manual)
    StatusDeferred   Status = "deferred"    // Postponed for later
    StatusClosed     Status = "closed"      // Complete
    StatusTombstone  Status = "tombstone"   // Soft-deleted, preserved for history
    StatusPinned     Status = "pinned"      // Persistent context marker
    StatusHooked     Status = "hooked"      // Attached to agent's hook (Gastown - optional)
)

// ValidStatuses returns all valid status values
func ValidStatuses() []Status {
    return []Status{
        StatusOpen, StatusInProgress, StatusBlocked,
        StatusDeferred, StatusClosed, StatusTombstone, StatusPinned,
    }
}

// IsTerminal returns true if the status represents a completed state
func (s Status) IsTerminal() bool {
    return s == StatusClosed || s == StatusTombstone
}

// IsActive returns true if the status represents active work
func (s Status) IsActive() bool {
    return s == StatusOpen || s == StatusInProgress
}
```

**Status Transitions:**

```
                    ┌─────────────────────────────────────┐
                    │                                     │
                    v                                     │
    ┌──────┐     ┌─────────────┐     ┌────────┐          │
    │ open │────▶│ in_progress │────▶│ closed │          │
    └──────┘     └─────────────┘     └────────┘          │
       │               │                  │              │
       │               │                  │              │
       v               v                  v              │
    ┌─────────┐   ┌─────────┐      ┌───────────┐        │
    │ blocked │   │ deferred │     │ tombstone │        │
    └─────────┘   └─────────┘      └───────────┘        │
       │               │                                 │
       └───────────────┴─────────────────────────────────┘
                    (reopen)
```

### 3.3 IssueType Enum

**Types to Port:**

```go
const (
    TypeBug     IssueType = "bug"      // Defect to fix
    TypeFeature IssueType = "feature"  // New functionality
    TypeTask    IssueType = "task"     // Generic work item (default)
    TypeEpic    IssueType = "epic"     // Container for related issues
    TypeChore   IssueType = "chore"    // Maintenance/housekeeping
    TypeDocs    IssueType = "docs"     // Documentation
    TypeQuestion IssueType = "question" // Question/discussion
)

// Default type for new issues
const DefaultIssueType = TypeTask
```

**Types NOT to Port (Gastown):**

```go
// DO NOT PORT
TypeMessage      IssueType = "message"       // Ephemeral inter-worker
TypeMergeRequest IssueType = "merge-request"
TypeMolecule     IssueType = "molecule"      // Template for hierarchies
TypeGate         IssueType = "gate"          // Async coordination
TypeAgent        IssueType = "agent"         // Agent identity
TypeRole         IssueType = "role"          // Agent role definition
TypeRig          IssueType = "rig"           // Multi-repo workspace
TypeConvoy       IssueType = "convoy"        // Cross-project tracking
TypeEvent        IssueType = "event"         // Operational state change
TypeSlot         IssueType = "slot"          // Exclusive access
```

### 3.4 Dependency Struct

```go
type Dependency struct {
    IssueID     string         `json:"issue_id"`              // The issue that has the dependency
    DependsOnID string         `json:"depends_on_id"`         // The issue being depended on
    Type        DependencyType `json:"type"`                  // Relationship type
    CreatedAt   time.Time      `json:"created_at"`
    CreatedBy   string         `json:"created_by,omitempty"`
    Metadata    string         `json:"metadata,omitempty"`    // Type-specific JSON blob
    ThreadID    string         `json:"thread_id,omitempty"`   // Conversation threading
}
```

### 3.5 DependencyType Enum

**Types to Port:**

```go
const (
    // === Workflow Types (affect ready work calculation) ===
    DepBlocks            DependencyType = "blocks"             // A blocks B = B depends on A
    DepParentChild       DependencyType = "parent-child"       // Hierarchical relationship
    DepConditionalBlocks DependencyType = "conditional-blocks" // Blocks only if condition met
    DepWaitsFor          DependencyType = "waits-for"          // Soft block, waits for children

    // === Association Types (informational only) ===
    DepRelated        DependencyType = "related"          // Soft link for reference
    DepDiscoveredFrom DependencyType = "discovered-from"  // Found during work on parent

    // === Graph Link Types (informational) ===
    DepRepliesTo  DependencyType = "replies-to"   // Conversation threading
    DepRelatesTo  DependencyType = "relates-to"   // Bidirectional reference
    DepDuplicates DependencyType = "duplicates"   // Marks as duplicate of
    DepSupersedes DependencyType = "supersedes"   // Replaces another issue

    // === Reference Types ===
    DepCausedBy DependencyType = "caused-by"  // Root cause linkage
)

// AffectsReadyWork returns true for dependency types that block ready work
func (t DependencyType) AffectsReadyWork() bool {
    switch t {
    case DepBlocks, DepParentChild, DepConditionalBlocks, DepWaitsFor:
        return true
    default:
        return false
    }
}

// IsBlocking returns true for types that can create blocking relationships
func (t DependencyType) IsBlocking() bool {
    return t == DepBlocks || t == DepParentChild || t == DepConditionalBlocks
}
```

**Types NOT to Port (Gastown):**

```go
// DO NOT PORT - HOP Entity Types
DepAuthoredBy  DependencyType = "authored-by"
DepAssignedTo  DependencyType = "assigned-to"
DepApprovedBy  DependencyType = "approved-by"
DepAttests     DependencyType = "attests"
DepTracks      DependencyType = "tracks"
DepUntil       DependencyType = "until"
DepValidates   DependencyType = "validates"
DepDelegatedFrom DependencyType = "delegated-from"
```

### 3.6 Comment Struct

```go
type Comment struct {
    ID        int64     `json:"id"`         // Auto-increment ID
    IssueID   string    `json:"issue_id"`   // Parent issue
    Author    string    `json:"author"`     // Who wrote the comment
    Text      string    `json:"text"`       // Markdown content
    CreatedAt time.Time `json:"created_at"`
}
```

### 3.7 Event Struct (Audit Trail)

```go
type Event struct {
    ID        int64     `json:"id"`
    IssueID   string    `json:"issue_id"`
    EventType EventType `json:"event_type"`
    Actor     string    `json:"actor"`                // Who performed the action
    OldValue  *string   `json:"old_value,omitempty"`  // Previous value (JSON)
    NewValue  *string   `json:"new_value,omitempty"`  // New value (JSON)
    Comment   *string   `json:"comment,omitempty"`    // Optional description
    CreatedAt time.Time `json:"created_at"`
}

// EventType constants
const (
    EventCreated           EventType = "created"
    EventUpdated           EventType = "updated"
    EventStatusChanged     EventType = "status_changed"
    EventPriorityChanged   EventType = "priority_changed"
    EventAssigneeChanged   EventType = "assignee_changed"
    EventCommented         EventType = "commented"
    EventClosed            EventType = "closed"
    EventReopened          EventType = "reopened"
    EventDependencyAdded   EventType = "dependency_added"
    EventDependencyRemoved EventType = "dependency_removed"
    EventLabelAdded        EventType = "label_added"
    EventLabelRemoved      EventType = "label_removed"
    EventCompacted         EventType = "compacted"
    EventDeleted           EventType = "deleted"           // Soft delete
    EventRestored          EventType = "restored"          // Restored from tombstone
)
```

### 3.8 Statistics Struct

```go
type Statistics struct {
    TotalIssues             int     `json:"total_issues"`
    OpenIssues              int     `json:"open_issues"`
    InProgressIssues        int     `json:"in_progress_issues"`
    ClosedIssues            int     `json:"closed_issues"`
    BlockedIssues           int     `json:"blocked_issues"`
    DeferredIssues          int     `json:"deferred_issues"`
    ReadyIssues             int     `json:"ready_issues"`
    TombstoneIssues         int     `json:"tombstone_issues"`
    PinnedIssues            int     `json:"pinned_issues"`
    EpicsEligibleForClosure int     `json:"epics_eligible_for_closure"`
    AverageLeadTime         float64 `json:"average_lead_time_hours"`

    // Breakdown by type
    ByType     map[string]int `json:"by_type,omitempty"`
    ByPriority map[int]int    `json:"by_priority,omitempty"`
    ByAssignee map[string]int `json:"by_assignee,omitempty"`
}
```

### 3.9 IssueFilter Struct

```go
type IssueFilter struct {
    // === Basic Filters ===
    Status      string   `json:"status,omitempty"`        // Single status
    Statuses    []string `json:"statuses,omitempty"`      // Multiple statuses (OR)
    Priority    *int     `json:"priority,omitempty"`      // Single priority
    Priorities  []int    `json:"priorities,omitempty"`    // Multiple priorities (OR)
    IssueType   string   `json:"issue_type,omitempty"`    // Single type
    IssueTypes  []string `json:"issue_types,omitempty"`   // Multiple types (OR)

    // === Assignment ===
    Assignee   string `json:"assignee,omitempty"`   // Filter by assignee
    Unassigned bool   `json:"unassigned,omitempty"` // Only unassigned issues

    // === Labels ===
    Label     string   `json:"label,omitempty"`      // Single label (exact match)
    Labels    []string `json:"labels,omitempty"`     // All labels must match (AND)
    LabelAny  []string `json:"label_any,omitempty"`  // Any label matches (OR)

    // === Search ===
    Query string `json:"query,omitempty"` // Full-text search in title/description

    // === Date Ranges ===
    CreatedAfter  *time.Time `json:"created_after,omitempty"`
    CreatedBefore *time.Time `json:"created_before,omitempty"`
    UpdatedAfter  *time.Time `json:"updated_after,omitempty"`
    UpdatedBefore *time.Time `json:"updated_before,omitempty"`
    ClosedAfter   *time.Time `json:"closed_after,omitempty"`
    ClosedBefore  *time.Time `json:"closed_before,omitempty"`

    // === Content Presence ===
    HasDescription *bool `json:"has_description,omitempty"`
    HasNotes       *bool `json:"has_notes,omitempty"`
    HasComments    *bool `json:"has_comments,omitempty"`

    // === Special Filters ===
    IncludeTombstones  bool `json:"include_tombstones,omitempty"`
    IncludeEphemeral   bool `json:"include_ephemeral,omitempty"`
    Overdue            bool `json:"overdue,omitempty"`             // due_at < now
    DeferredOnly       bool `json:"deferred_only,omitempty"`
    PinnedOnly         bool `json:"pinned_only,omitempty"`

    // === Exclusions ===
    ExcludeStatuses []string `json:"exclude_statuses,omitempty"`
    ExcludeTypes    []string `json:"exclude_types,omitempty"`
    ExcludeIDs      []string `json:"exclude_ids,omitempty"`

    // === Hierarchy ===
    ParentID string `json:"parent_id,omitempty"` // Direct children only

    // === Pagination ===
    Limit  int `json:"limit,omitempty"`
    Offset int `json:"offset,omitempty"`

    // === Sorting ===
    SortBy    string `json:"sort_by,omitempty"`    // Field to sort by
    SortOrder string `json:"sort_order,omitempty"` // "asc" or "desc"
}
```

---

## 4. Storage Interface Specification

The storage interface defines all operations on the issue database. The Rust port must implement all these methods.

### 4.1 Issue CRUD Operations

```go
// CreateIssue creates a new issue and returns the created issue with ID populated
// - Generates hash-based ID if not provided
// - Sets CreatedAt/UpdatedAt to current time
// - Computes and stores ContentHash
// - Creates "created" event in audit trail
// - Marks issue as dirty for export
// Returns: Created issue with all fields populated, or error
CreateIssue(ctx context.Context, issue *types.Issue) (*types.Issue, error)

// GetIssue retrieves a single issue by exact ID
// - Returns ErrNotFound if issue doesn't exist
// - Populates Labels, Dependencies, Comments if available
// - Does NOT return tombstones unless explicitly requested
GetIssue(ctx context.Context, id string) (*types.Issue, error)

// GetIssueByPrefix retrieves issue by ID prefix (for short ID lookup)
// - First tries exact match, then prefix match
// - Returns ErrNotFound if no match or multiple matches
// - Excludes tombstones from prefix matching
GetIssueByPrefix(ctx context.Context, prefix string) (*types.Issue, error)

// UpdateIssue updates an existing issue
// - Updates UpdatedAt timestamp automatically
// - Recomputes ContentHash
// - Creates appropriate event(s) in audit trail
// - Marks issue as dirty for export
// - Returns ErrNotFound if issue doesn't exist
UpdateIssue(ctx context.Context, issue *types.Issue) error

// CloseIssue closes an issue with optional reason
// - Sets Status to "closed"
// - Sets ClosedAt to current time
// - Optionally sets CloseReason
// - Creates "closed" event
// - Marks as dirty
// - Returns ErrNotFound if issue doesn't exist
CloseIssue(ctx context.Context, id string, reason string) error

// ReopenIssue reopens a closed issue
// - Sets Status to "open"
// - Clears ClosedAt
// - Creates "reopened" event
// - Returns error if issue is tombstone
ReopenIssue(ctx context.Context, id string) error

// DeleteIssue soft-deletes an issue (creates tombstone)
// - Sets Status to "tombstone"
// - Sets DeletedAt, DeletedBy, DeleteReason
// - Preserves OriginalType
// - Creates "deleted" event
// - Marks as dirty (tombstones ARE exported)
DeleteIssue(ctx context.Context, id string, deletedBy string, reason string) error

// HardDeleteIssue permanently removes an issue from database
// - Used only for ephemeral issues (wisps) that were never exported
// - Cascades to dependencies, labels, comments, events
// - Does NOT mark as dirty (nothing to export)
// - Use with extreme caution
HardDeleteIssue(ctx context.Context, id string) error

// RestoreIssue restores a tombstoned issue
// - Sets Status back to OriginalType (or "open" if not set)
// - Clears tombstone fields
// - Creates "restored" event
RestoreIssue(ctx context.Context, id string) error
```

### 4.2 Issue Query Operations

```go
// ListIssues retrieves issues matching filter criteria
// - Returns slice of issues (may be empty)
// - Does NOT populate Dependencies/Comments (use GetIssue for full data)
// - Respects IncludeTombstones flag
// - Applies all filter conditions with AND logic
// - Applies pagination (Limit/Offset)
ListIssues(ctx context.Context, filter *types.IssueFilter) ([]*types.Issue, error)

// SearchIssues performs full-text search on title and description
// - Uses SQLite FTS5 if available, falls back to LIKE
// - Returns issues ordered by relevance
// - Respects status filter (excludes tombstones by default)
SearchIssues(ctx context.Context, query string, filter *types.IssueFilter) ([]*types.Issue, error)

// CountIssues returns count of issues matching filter
// - More efficient than ListIssues when only count needed
CountIssues(ctx context.Context, filter *types.IssueFilter) (int, error)

// GetAllIssues retrieves all issues for export
// - Includes tombstones
// - Excludes ephemeral issues
// - Populates all related data (dependencies, labels, comments)
// - Used by JSONL export
GetAllIssues(ctx context.Context) ([]*types.Issue, error)

// GetIssuesByIDs retrieves multiple issues by ID
// - More efficient than multiple GetIssue calls
// - Returns map[id]*Issue
// - Missing IDs are simply not in the returned map
GetIssuesByIDs(ctx context.Context, ids []string) (map[string]*types.Issue, error)
```

### 4.3 Ready Work Operations

```go
// GetReadyWork retrieves issues ready to be worked on
// - Status must be "open" or "in_progress"
// - NOT in blocked_issues_cache
// - NOT deferred (defer_until is null or in the past)
// - NOT pinned
// - NOT ephemeral
// - Ordered by priority (asc), then created_at (asc)
// Returns: Slice of ready issues
GetReadyWork(ctx context.Context, filter *types.IssueFilter) ([]*types.Issue, error)

// GetBlockedIssues retrieves all issues that are blocked
// - Returns issues in blocked_issues_cache
// - Includes blocking reason (what's blocking each issue)
GetBlockedIssues(ctx context.Context) ([]*types.BlockedIssue, error)

// IsBlocked checks if a specific issue is blocked
// - Checks blocked_issues_cache
// - More efficient than GetBlockedIssues for single check
IsBlocked(ctx context.Context, id string) (bool, error)

// GetBlockingIssues returns issues that block the given issue
// - Returns the immediate blockers (not transitive)
// - Includes dependency type information
GetBlockingIssues(ctx context.Context, id string) ([]*types.Issue, error)

// RefreshBlockedCache rebuilds the blocked_issues_cache table
// - Called after dependency changes or status changes
// - Computes transitive closure of blocking relationships
// - Uses recursive CTE with depth limit
RefreshBlockedCache(ctx context.Context) error
```

### 4.4 Dependency Operations

```go
// AddDependency creates a dependency relationship
// - Validates both issues exist
// - Checks for cycles (returns ErrCycle if detected)
// - Creates "dependency_added" event
// - Marks both issues as dirty
// - Triggers blocked cache refresh
AddDependency(ctx context.Context, dep *types.Dependency) error

// RemoveDependency removes a dependency relationship
// - Returns error if dependency doesn't exist
// - Creates "dependency_removed" event
// - Marks both issues as dirty
// - Triggers blocked cache refresh
RemoveDependency(ctx context.Context, issueID, dependsOnID string) error

// GetDependencies retrieves dependencies for an issue
// - direction "down": things this issue depends on
// - direction "up": things that depend on this issue
// - direction "both": all dependencies
GetDependencies(ctx context.Context, issueID string, direction string) ([]*types.Dependency, error)

// GetDependents retrieves issues that depend on the given issue
// - Alias for GetDependencies with direction "up"
GetDependents(ctx context.Context, issueID string) ([]*types.Dependency, error)

// DetectCycles checks if adding a dependency would create a cycle
// - Uses recursive CTE with depth limit (100)
// - Only checks blocking dependency types
// - Returns true if cycle would be created
DetectCycles(ctx context.Context, fromID, toID string) (bool, error)

// GetDependencyTree builds a tree structure of dependencies
// - maxDepth limits recursion (default 10)
// - Returns nested structure suitable for tree rendering
GetDependencyTree(ctx context.Context, rootID string, maxDepth int) (*types.DependencyNode, error)

// GetAllDependencies retrieves all dependencies in the database
// - Used for export and cycle detection
GetAllDependencies(ctx context.Context) ([]*types.Dependency, error)
```

### 4.5 Label Operations

```go
// AddLabel adds a label to an issue
// - Creates entry in labels table
// - Creates "label_added" event
// - Marks issue as dirty
// - Idempotent: no error if label already exists
AddLabel(ctx context.Context, issueID, label string) error

// RemoveLabel removes a label from an issue
// - Removes entry from labels table
// - Creates "label_removed" event
// - Marks issue as dirty
// - No error if label didn't exist
RemoveLabel(ctx context.Context, issueID, label string) error

// GetLabels retrieves all labels for an issue
// - Returns slice of label strings
GetLabels(ctx context.Context, issueID string) ([]string, error)

// GetLabelsForIssues retrieves labels for multiple issues efficiently
// - Returns map[issueID][]label
// - Single query instead of N queries
GetLabelsForIssues(ctx context.Context, issueIDs []string) (map[string][]string, error)

// GetAllLabels retrieves all unique labels in the database
// - Returns slice of unique label strings
// - Sorted alphabetically
GetAllLabels(ctx context.Context) ([]string, error)

// GetIssuesByLabel retrieves all issues with a specific label
// - Returns slice of issues
// - Respects tombstone exclusion by default
GetIssuesByLabel(ctx context.Context, label string) ([]*types.Issue, error)
```

### 4.6 Comment Operations

```go
// AddComment adds a comment to an issue
// - Assigns auto-increment ID
// - Sets CreatedAt to current time
// - Creates "commented" event
// - Marks issue as dirty
// - Returns created comment with ID
AddComment(ctx context.Context, comment *types.Comment) (*types.Comment, error)

// GetComments retrieves all comments for an issue
// - Ordered by created_at ascending
// - Returns empty slice if no comments
GetComments(ctx context.Context, issueID string) ([]*types.Comment, error)

// GetCommentsForIssues retrieves comments for multiple issues
// - Returns map[issueID][]*Comment
// - Single query instead of N queries
GetCommentsForIssues(ctx context.Context, issueIDs []string) (map[string][]*types.Comment, error)

// DeleteComment removes a comment
// - Hard delete (comments don't have tombstones)
// - No event created (comments are metadata)
DeleteComment(ctx context.Context, commentID int64) error
```

### 4.7 Event Operations (Audit Trail)

```go
// CreateEvent records an event in the audit trail
// - Sets CreatedAt to current time
// - Returns created event with ID
CreateEvent(ctx context.Context, event *types.Event) (*types.Event, error)

// GetEvents retrieves events for an issue
// - Ordered by created_at ascending
// - Returns full audit history
GetEvents(ctx context.Context, issueID string) ([]*types.Event, error)

// GetEventsAfter retrieves events after a timestamp
// - Used for incremental sync
// - Returns events for all issues
GetEventsAfter(ctx context.Context, after time.Time) ([]*types.Event, error)

// GetRecentEvents retrieves most recent N events
// - Across all issues
// - Ordered by created_at descending
GetRecentEvents(ctx context.Context, limit int) ([]*types.Event, error)
```

### 4.8 Statistics Operations

```go
// GetStatistics computes project statistics
// - Counts by status, type, priority, assignee
// - Calculates average lead time (create -> close)
// - Returns Statistics struct
GetStatistics(ctx context.Context) (*types.Statistics, error)

// GetEpicsEligibleForClosure returns epics whose children are all closed
// - Epic status is open or in_progress
// - All child issues (parent-child deps) are closed or tombstone
GetEpicsEligibleForClosure(ctx context.Context) ([]*types.Issue, error)
```

### 4.9 Dirty Tracking Operations

```go
// MarkDirty marks an issue as needing export
// - Inserts into dirty_issues table
// - Idempotent: no error if already marked
MarkDirty(ctx context.Context, issueID string) error

// GetDirtyIssues retrieves all issues marked dirty
// - Returns issue IDs only (not full issues)
GetDirtyIssues(ctx context.Context) ([]string, error)

// ClearDirtyIssues clears all dirty flags
// - Called after successful export
ClearDirtyIssues(ctx context.Context) error

// ClearDirtyIssuesByID clears dirty flags for specific issues
// - Called after incremental export
ClearDirtyIssuesByID(ctx context.Context, ids []string) error

// HasDirtyIssues returns true if any issues are dirty
// - More efficient than GetDirtyIssues when only checking existence
HasDirtyIssues(ctx context.Context) (bool, error)
```

### 4.10 Export Hash Operations

```go
// SetExportHash records the content hash at export time
// - Used to detect external changes to JSONL
SetExportHash(ctx context.Context, issueID, contentHash string) error

// GetExportHash retrieves the last exported content hash
// - Returns empty string if never exported
GetExportHash(ctx context.Context, issueID string) (string, error)

// GetExportHashes retrieves export hashes for multiple issues
// - Returns map[issueID]hash
GetExportHashes(ctx context.Context, issueIDs []string) (map[string]string, error)

// ClearExportHashes removes all export hash records
// - Called when full re-export is needed
ClearExportHashes(ctx context.Context) error
```

### 4.11 Configuration Operations

```go
// GetConfig retrieves a configuration value
// - Returns empty string if not set
GetConfig(ctx context.Context, key string) (string, error)

// SetConfig sets a configuration value
// - Overwrites existing value if present
SetConfig(ctx context.Context, key, value string) error

// GetAllConfig retrieves all configuration key-value pairs
// - Returns map[key]value
GetAllConfig(ctx context.Context) (map[string]string, error)

// DeleteConfig removes a configuration key
// - No error if key didn't exist
DeleteConfig(ctx context.Context, key string) error
```

### 4.12 Metadata Operations

```go
// GetMetadata retrieves internal metadata
// - Used for sync state, import tracking, etc.
GetMetadata(ctx context.Context, key string) (string, error)

// SetMetadata sets internal metadata
SetMetadata(ctx context.Context, key, value string) error

// GetAllMetadata retrieves all metadata
GetAllMetadata(ctx context.Context) (map[string]string, error)
```

### 4.13 Transaction Support

```go
// RunInTransaction executes a function within a transaction
// - Uses BEGIN IMMEDIATE for write operations
// - Automatically commits on success, rolls back on error
// - Supports nested calls (inner calls are no-ops)
RunInTransaction(ctx context.Context, fn func(ctx context.Context) error) error

// BeginTx starts a new transaction manually
// - Returns transaction handle
// - Caller must call Commit() or Rollback()
BeginTx(ctx context.Context) (*sql.Tx, error)
```

### 4.14 Utility Operations

```go
// Close closes the storage connection
// - Flushes any pending operations
// - Releases database lock
Close() error

// Ping verifies database connectivity
// - Returns error if database is unreachable
Ping(ctx context.Context) error

// GetVersion returns the storage backend version
// - Includes migration version
GetVersion(ctx context.Context) (string, error)

// RunMigrations applies pending database migrations
// - Idempotent: already-applied migrations are skipped
// - Returns number of migrations applied
RunMigrations(ctx context.Context) (int, error)
```

---

## 5. SQLite Storage Implementation

### 5.1 Database Schema

#### Issues Table (Core)

```sql
CREATE TABLE IF NOT EXISTS issues (
    id TEXT PRIMARY KEY,
    content_hash TEXT,
    title TEXT NOT NULL CHECK(length(title) <= 500),
    description TEXT NOT NULL DEFAULT '',
    design TEXT NOT NULL DEFAULT '',
    acceptance_criteria TEXT NOT NULL DEFAULT '',
    notes TEXT NOT NULL DEFAULT '',
    status TEXT NOT NULL DEFAULT 'open',
    priority INTEGER NOT NULL DEFAULT 2 CHECK(priority >= 0 AND priority <= 4),
    issue_type TEXT NOT NULL DEFAULT 'task',
    assignee TEXT,
    owner TEXT DEFAULT '',
    estimated_minutes INTEGER,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    created_by TEXT DEFAULT '',
    updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    closed_at DATETIME,
    close_reason TEXT DEFAULT '',
    closed_by_session TEXT DEFAULT '',
    external_ref TEXT,
    due_at DATETIME,
    defer_until DATETIME,
    -- Compaction
    compaction_level INTEGER DEFAULT 0,
    compacted_at DATETIME,
    compacted_at_commit TEXT,
    original_size INTEGER,
    -- Tombstone
    deleted_at DATETIME,
    deleted_by TEXT DEFAULT '',
    delete_reason TEXT DEFAULT '',
    original_type TEXT DEFAULT '',
    -- Messaging
    sender TEXT DEFAULT '',
    ephemeral INTEGER DEFAULT 0,
    -- Context
    pinned INTEGER DEFAULT 0,
    is_template INTEGER DEFAULT 0,
    -- Federation
    source_system TEXT DEFAULT '',

    -- === CONSTRAINTS ===

    -- Closed-at invariant: closed issues MUST have closed_at timestamp
    CHECK (
        (status = 'closed' AND closed_at IS NOT NULL) OR
        (status = 'tombstone') OR
        (status NOT IN ('closed', 'tombstone') AND closed_at IS NULL)
    )
);

-- === INDEXES ===

-- Primary access patterns
CREATE INDEX IF NOT EXISTS idx_issues_status ON issues(status);
CREATE INDEX IF NOT EXISTS idx_issues_priority ON issues(priority);
CREATE INDEX IF NOT EXISTS idx_issues_issue_type ON issues(issue_type);
CREATE INDEX IF NOT EXISTS idx_issues_assignee ON issues(assignee) WHERE assignee IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_issues_created_at ON issues(created_at);
CREATE INDEX IF NOT EXISTS idx_issues_updated_at ON issues(updated_at);

-- Export/sync patterns
CREATE INDEX IF NOT EXISTS idx_issues_content_hash ON issues(content_hash);
CREATE INDEX IF NOT EXISTS idx_issues_external_ref ON issues(external_ref) WHERE external_ref IS NOT NULL;
CREATE UNIQUE INDEX IF NOT EXISTS idx_issues_external_ref_unique ON issues(external_ref) WHERE external_ref IS NOT NULL;

-- Special states
CREATE INDEX IF NOT EXISTS idx_issues_ephemeral ON issues(ephemeral) WHERE ephemeral = 1;
CREATE INDEX IF NOT EXISTS idx_issues_pinned ON issues(pinned) WHERE pinned = 1;
CREATE INDEX IF NOT EXISTS idx_issues_tombstone ON issues(status) WHERE status = 'tombstone';

-- Time-based
CREATE INDEX IF NOT EXISTS idx_issues_due_at ON issues(due_at) WHERE due_at IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_issues_defer_until ON issues(defer_until) WHERE defer_until IS NOT NULL;

-- Ready work composite index (most important for performance)
CREATE INDEX IF NOT EXISTS idx_issues_ready
    ON issues(status, priority, created_at)
    WHERE status IN ('open', 'in_progress')
    AND ephemeral = 0
    AND pinned = 0;
```

#### Dependencies Table

```sql
CREATE TABLE IF NOT EXISTS dependencies (
    issue_id TEXT NOT NULL,
    depends_on_id TEXT NOT NULL,
    type TEXT NOT NULL DEFAULT 'blocks',
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    created_by TEXT NOT NULL DEFAULT '',
    metadata TEXT DEFAULT '{}',
    thread_id TEXT DEFAULT '',

    PRIMARY KEY (issue_id, depends_on_id),
    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
    -- Note: depends_on_id FK intentionally removed to allow external issue references
);

CREATE INDEX IF NOT EXISTS idx_dependencies_issue ON dependencies(issue_id);
CREATE INDEX IF NOT EXISTS idx_dependencies_depends_on ON dependencies(depends_on_id);
CREATE INDEX IF NOT EXISTS idx_dependencies_type ON dependencies(type);
CREATE INDEX IF NOT EXISTS idx_dependencies_depends_on_type ON dependencies(depends_on_id, type);
CREATE INDEX IF NOT EXISTS idx_dependencies_thread ON dependencies(thread_id) WHERE thread_id != '';

-- Composite for blocking lookups
CREATE INDEX IF NOT EXISTS idx_dependencies_blocking
    ON dependencies(depends_on_id, issue_id)
    WHERE type IN ('blocks', 'parent-child', 'conditional-blocks', 'waits-for');
```

**Metadata JSON note:** The `dependencies.metadata` column is queried with `json_extract(...)` for `waits-for` gates. This requires SQLite to be built with the JSON1 extension (modern SQLite includes this by default, but the Rust port should ensure `rusqlite` is built against a JSON-capable SQLite).

#### Blocked Issues Cache Table

This cache is created by migration and materializes the blocked set for fast `ready` queries:

```sql
CREATE TABLE blocked_issues_cache (
    issue_id TEXT NOT NULL,
    PRIMARY KEY (issue_id),
    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
);
```

The cache is rebuilt on dependency/status changes (see Section 8).

#### Labels Table

```sql
CREATE TABLE IF NOT EXISTS labels (
    issue_id TEXT NOT NULL,
    label TEXT NOT NULL,
    PRIMARY KEY (issue_id, label),
    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_labels_label ON labels(label);
CREATE INDEX IF NOT EXISTS idx_labels_issue ON labels(issue_id);
```

#### Comments Table

```sql
CREATE TABLE IF NOT EXISTS comments (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    issue_id TEXT NOT NULL,
    author TEXT NOT NULL,
    text TEXT NOT NULL,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_comments_issue ON comments(issue_id);
CREATE INDEX IF NOT EXISTS idx_comments_created_at ON comments(created_at);
```

#### Events Table (Audit Trail)

```sql
CREATE TABLE IF NOT EXISTS events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    issue_id TEXT NOT NULL,
    event_type TEXT NOT NULL,
    actor TEXT NOT NULL DEFAULT '',
    old_value TEXT,
    new_value TEXT,
    comment TEXT,
    created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_events_issue ON events(issue_id);
CREATE INDEX IF NOT EXISTS idx_events_type ON events(event_type);
CREATE INDEX IF NOT EXISTS idx_events_created_at ON events(created_at);
CREATE INDEX IF NOT EXISTS idx_events_actor ON events(actor) WHERE actor != '';
```

#### Config Table

```sql
CREATE TABLE IF NOT EXISTS config (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
```

#### Metadata Table

```sql
CREATE TABLE IF NOT EXISTS metadata (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
```

#### Dirty Issues Table (Export Tracking)

```sql
CREATE TABLE IF NOT EXISTS dirty_issues (
    issue_id TEXT PRIMARY KEY,
    marked_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_dirty_issues_marked_at ON dirty_issues(marked_at);
```

#### Export Hashes Table (Deduplication)

```sql
CREATE TABLE IF NOT EXISTS export_hashes (
    issue_id TEXT PRIMARY KEY,
    content_hash TEXT NOT NULL,
    exported_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
);
```

#### Blocked Issues Cache Table

```sql
-- Materialized view of blocked issues for performance
-- Rebuilt on dependency or status changes
CREATE TABLE IF NOT EXISTS blocked_issues_cache (
    issue_id TEXT PRIMARY KEY,
    blocked_by TEXT NOT NULL,  -- JSON array of blocking issue IDs
    blocked_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_blocked_cache_blocked_at ON blocked_issues_cache(blocked_at);
```

#### Child Counters Table (Hierarchical IDs)

```sql
-- Tracks next child number for dotted IDs (bd-abc.1, bd-abc.2, etc.)
CREATE TABLE IF NOT EXISTS child_counters (
    parent_id TEXT PRIMARY KEY,
    last_child INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (parent_id) REFERENCES issues(id) ON DELETE CASCADE
);
```

### 5.2 SQLite Pragmas and Configuration

```sql
-- === Connection-Level Pragmas (set on every connection) ===

-- Enable foreign key enforcement
PRAGMA foreign_keys = ON;

-- Set busy timeout to 30 seconds (30000ms)
-- Prevents "database is locked" errors during concurrent access
PRAGMA busy_timeout = 30000;

-- Use Write-Ahead Logging for better concurrency
-- Exception: Use DELETE mode for:
--   - WSL2 with Windows filesystem (/mnt/c/, etc.) - WAL doesn't work across filesystems
--   - In-memory databases (:memory:) - WAL requires file
PRAGMA journal_mode = WAL;

-- === Performance Pragmas ===

-- Larger cache for better read performance (64MB)
PRAGMA cache_size = -65536;

-- Synchronous mode: NORMAL balances safety and speed
-- FULL is safer but slower, OFF is dangerous
PRAGMA synchronous = NORMAL;

-- Store temp tables in memory
PRAGMA temp_store = MEMORY;

-- Enable memory-mapped I/O for reads (256MB)
PRAGMA mmap_size = 268435456;
```

**Connection Pool Settings:**

```go
// File-based databases
MaxOpenConns = runtime.NumCPU() + 1  // e.g., 9 on 8-core
MaxIdleConns = 2
ConnMaxLifetime = 0  // No limit
ConnMaxIdleTime = 5 * time.Minute

// In-memory databases (must use single connection)
MaxOpenConns = 1
MaxIdleConns = 1
```

### 5.3 Transaction Handling

**BEGIN IMMEDIATE Pattern:**

```go
// For write operations, use BEGIN IMMEDIATE to acquire lock early
// This prevents deadlocks when multiple writers compete

func (s *Store) beginImmediateWithRetry(ctx context.Context, maxRetries int) (*sql.Tx, error) {
    baseDelay := 10 * time.Millisecond

    for attempt := 0; attempt < maxRetries; attempt++ {
        tx, err := s.db.BeginTx(ctx, nil)
        if err != nil {
            return nil, err
        }

        _, err = tx.ExecContext(ctx, "BEGIN IMMEDIATE")
        if err == nil {
            return tx, nil
        }

        tx.Rollback()

        // Check if error is SQLITE_BUSY (database locked)
        if !isBusyError(err) {
            return nil, err
        }

        // Exponential backoff: 10ms, 20ms, 40ms, 80ms, ...
        delay := baseDelay * (1 << attempt)
        if delay > 5*time.Second {
            delay = 5 * time.Second
        }

        select {
        case <-ctx.Done():
            return nil, ctx.Err()
        case <-time.After(delay):
            continue
        }
    }

    return nil, fmt.Errorf("failed to acquire lock after %d retries", maxRetries)
}
```

**Transaction Wrapper:**

```go
func (s *Store) RunInTransaction(ctx context.Context, fn func(context.Context) error) error {
    tx, err := s.beginImmediateWithRetry(ctx, 10)
    if err != nil {
        return err
    }

    defer func() {
        if p := recover(); p != nil {
            tx.Rollback()
            panic(p)
        }
    }()

    // Create context with transaction
    txCtx := context.WithValue(ctx, txKey, tx)

    if err := fn(txCtx); err != nil {
        tx.Rollback()
        return err
    }

    return tx.Commit()
}
```

### 5.4 Migration System

Legacy beads migrations are **idempotent Go functions**, not versioned SQL files. There is **no schema_migrations table**; every migration checks for the presence of its target column/table/index and no-ops if already applied.

**Execution model:**
- Runs on DB open.
- Uses `BEGIN EXCLUSIVE` (serializes concurrent open/migration attempts).
- Temporarily disables foreign keys (`PRAGMA foreign_keys=OFF`) because some migrations rebuild tables.
- Performs **pre-migration orphan cleanup**, then captures a snapshot, then runs migrations, then verifies invariants.

**Migration catalog (ordered):**

Core/classic migrations (keep for `br`):
1. `dirty_issues_table` - dirty tracking for incremental export.
2. `external_ref_column` - `issues.external_ref`.
3. `composite_indexes` - query performance indexes.
4. `closed_at_constraint` - closed/tombstone invariant.
5. `compaction_columns` - compaction metadata columns (even if AI compaction is excluded).
6. `snapshots_table` - compaction snapshot tables (optional if compaction excluded).
7. `compaction_config` - compaction config keys (optional if compaction excluded).
8. `compacted_at_commit_column` - compaction metadata (optional if compaction excluded).
9. `export_hashes_table` - export dedup tracking.
10. `content_hash_column` - `issues.content_hash`.
11. `external_ref_unique` - UNIQUE index on `external_ref`.
12. `source_repo_column` - multi-repo support (classic).
13. `repo_mtimes_table` - multi-repo hydration cache.
14. `child_counters_table` - hierarchical ID counters.
15. `blocked_issues_cache` - ready-work cache table.
16. `orphan_detection` - logs orphaned children (no schema change).
17. `close_reason_column` - `issues.close_reason`.
18. `tombstone_columns` - inline tombstones.
19. `messaging_fields` - `sender`, `ephemeral` (classic wisps).
20. `edge_consolidation` - dependency metadata + thread_id.
21. `migrate_edge_fields` - moves legacy edge fields to dependencies.
22. `drop_edge_columns` - removes deprecated edge columns from `issues`.
23. `pinned_column` - pinned context marker.
24. `is_template_column` - templates (classic, even if not used).
25. `remove_depends_on_fk` - allows `external:*` dependencies.
26. `additional_indexes` - performance indexes.
27. `tombstone_closed_at` - preserves closed_at when tombstoned.
28. `created_by_column` - creator attribution.
29. `closed_by_session_column` - Claude session attribution.
30. `due_defer_columns` - scheduling fields.
31. `owner_column` - owner attribution (classic).
32. `source_system_column` - federation adapter tracking (classic).

Gastown/HOP migrations (exclude for classic `br`):
- `gate_columns` (await_type/await_id/timeout/waiters)
- `agent_fields` (hook_bead, role_bead, agent_state, rig, etc.)
- `mol_type_column` (molecule type)
- `hooked_status_migration`
- `event_fields` (event beads)
- `crystallizes_column` (HOP economics)
- `work_type_column` (open_competition)
- `quality_score_column` (HOP quality)

**Porting note:** For `br`, prefer a **single consolidated schema** that already includes only the classic fields, then add minimal migrations for forward compatibility. Avoid porting migrations that only serve Gastown or AI compaction.

---

## 6. CLI Commands Specification

### 6.1 Global Flags

All commands support these global flags:

```
--db <path>           Database path (auto-discovers .beads/*.db if not specified)
--actor <name>        Actor name for audit trail (default: git user or $USER)
--json                Output in JSON format (machine-readable)
--no-daemon           Force direct storage mode, bypass daemon
--no-auto-flush       Skip automatic JSONL export after changes
--no-auto-import      Skip automatic JSONL import before queries
--verbose, -v         Enable verbose debug output
--quiet, -q           Suppress non-essential output
--lock-timeout <ms>   SQLite busy timeout in milliseconds (default: 30000)
--help, -h            Show help for command
```

### 6.2 `init` Command

**Purpose:** Initialize a beads workspace in the current directory.

```bash
bd init [flags]

Flags:
  --prefix <string>    Issue ID prefix (default: "bd")
  --force              Overwrite existing .beads/ directory
```

**Behavior:**
1. Creates `.beads/` directory
2. Creates `.beads/beads.db` SQLite database
3. Runs all migrations
4. Sets `issue_prefix` config if --prefix specified
5. Creates `.beads/.gitignore` with:
   ```
   beads.db
   beads.db-wal
   beads.db-shm
   bd.sock
   daemon.log
   export_hashes.db
   sync_base.jsonl
   ```

**Output:**
- Text: `Initialized beads workspace in .beads/`
- JSON: `{"status": "initialized", "path": ".beads/", "prefix": "bd"}`

### 6.3 `create` Command

**Purpose:** Create a new issue.

```bash
bd create <title> [flags]

Arguments:
  title                Issue title (required, max 500 chars)

Flags:
  --type, -t <type>           Issue type (default: task)
                              Values: bug, feature, task, epic, chore, docs, question
  --priority, -p <int>        Priority 0-4 or P0-P4 (default: 2)
  --description, -d <text>    Description text (multi-line OK)
  --design <text>             Design specification
  --acceptance <text>         Acceptance criteria
  --notes <text>              Additional notes
  --assignee, -a <name>       Assign to person
  --owner <email>             Owner email (default: git author)
  --labels, -l <labels>       Comma-separated labels
  --parent <id>               Parent issue ID (creates parent-child dep)
  --deps <deps>               Dependencies (format: type:id,type:id)
                              Examples: blocks:bd-abc, discovered-from:bd-def
  --estimate, -e <minutes>    Time estimate in minutes
  --due <datetime>            Due date (RFC3339 or relative: "tomorrow", "2024-12-31")
  --defer <datetime>          Defer until date
  --external-ref <ref>        External reference (e.g., "gh-123", "JIRA-456")
  --ephemeral                 Mark as ephemeral (not exported to JSONL)
  --dry-run                   Preview without creating
  --silent                    Output only issue ID (for scripting)
```

**Behavior:**
1. Validates title length (1-500 chars)
2. Validates priority range (0-4)
3. Generates hash-based ID
4. Creates issue in database
5. Adds dependencies if specified
6. Adds labels if specified
7. Marks as dirty for export
8. Creates "created" event

**Output:**
- Text: `Created bd-abc123: Issue title`
- Silent: `bd-abc123`
- JSON:
  ```json
  {
    "id": "bd-abc123",
    "title": "Issue title",
    "status": "open",
    "priority": 2,
    "issue_type": "task",
    "created_at": "2024-01-15T10:30:00Z"
  }
  ```

### 6.4 `update` Command

**Purpose:** Update an existing issue.

```bash
bd update <id> [flags]

Arguments:
  id                   Issue ID (full or prefix, or "." for last touched)

Flags:
  --title <text>              New title
  --description, -d <text>    New description
  --design <text>             New design spec
  --acceptance <text>         New acceptance criteria
  --notes <text>              New notes
  --status, -s <status>       New status (open, in_progress, blocked, deferred)
  --priority, -p <int>        New priority 0-4
  --type, -t <type>           New issue type
  --assignee, -a <name>       New assignee (use "" to clear)
  --owner <email>             New owner
  --estimate, -e <minutes>    New time estimate
  --due <datetime>            New due date (use "" to clear)
  --defer <datetime>          New defer date (use "" to clear)
  --external-ref <ref>        New external reference
  --add-label <label>         Add a label
  --remove-label <label>      Remove a label
  --pinned <bool>             Set pinned status
```

**Behavior:**
1. Resolves issue ID (exact match or prefix)
2. Validates new values
3. Updates specified fields only (others unchanged)
4. Updates UpdatedAt timestamp
5. Recomputes ContentHash
6. Creates event(s) for changed fields
7. Marks as dirty

**Output:**
- Text: `Updated bd-abc123`
- JSON: Full issue object with updated fields

### 6.5 `close` Command

**Purpose:** Close one or more issues.

```bash
bd close <id>... [flags]

Arguments:
  id                   Issue ID(s) to close (supports multiple)

Flags:
  --reason, -r <text>         Reason for closing
  --force                     Close even if blocked
  --suggest-next              After closing, show next ready issue
  --session <id>              Session ID (for attribution)
```

**Behavior:**
1. Resolves each issue ID
2. For each issue:
   - Checks if blocked (fails unless --force)
   - Sets status to "closed"
   - Sets ClosedAt to current time
   - Sets CloseReason if provided
   - Sets ClosedBySession if provided
   - Creates "closed" event
   - Marks as dirty
   - Refreshes blocked cache (may unblock dependents)
3. If --suggest-next, queries ready work and shows first result

**Output:**
- Text: `Closed bd-abc123: Issue title`
- JSON: Array of closed issue objects

### 6.6 `list` Command

**Purpose:** List issues matching criteria.

```bash
bd list [flags]

Flags:
  --status, -s <status>       Filter by status (comma-separated for multiple)
  --priority, -p <int>        Filter by priority
  --type, -t <type>           Filter by type
  --assignee, -a <name>       Filter by assignee
  --unassigned                Show only unassigned issues
  --label, -l <label>         Filter by label (AND if multiple -l flags)
  --label-any <labels>        Filter by any label (OR, comma-separated)
  --query, -q <text>          Full-text search in title/description
  --created-after <date>      Filter by creation date
  --created-before <date>     Filter by creation date
  --updated-after <date>      Filter by update date
  --overdue                   Show only overdue issues
  --deferred                  Show only deferred issues
  --pinned                    Show only pinned issues
  --include-tombstones        Include soft-deleted issues
  --sort <field>              Sort by field (priority, created_at, updated_at)
  --order <asc|desc>          Sort order (default: asc)
  --limit, -n <int>           Maximum results (default: 50)
  --offset <int>              Skip first N results
  --pretty                    Pretty tree format with Unicode
  --no-header                 Omit header in table output
```

**Output Formats:**

Text (default):
```
ID         PRI  TYPE     STATUS       ASSIGNEE  TITLE
bd-abc123  P1   feature  in_progress  alice     Add dark mode
bd-def456  P2   bug      open         bob       Fix login error
```

Pretty (--pretty):
```
○ bd-abc123 [P1] Add dark mode (feature) @alice
● bd-def456 [P2] Fix login error (bug) @bob
```

JSON:
```json
{
  "issues": [...],
  "total": 42,
  "limit": 50,
  "offset": 0
}
```

### 6.7 `show` Command

**Purpose:** Show detailed information about an issue.

```bash
bd show <id> [flags]

Arguments:
  id                   Issue ID (or "." for last touched)

Flags:
  --short              Show compact format
  --deps               Show dependency tree
  --comments           Show comments
  --events             Show event history
  --refs               Show what references this issue
  --no-color           Disable color output
```

**Output:**

Text (full):
```
bd-abc123: Add dark mode toggle
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━

Status:    in_progress          Priority: P1 (high)
Type:      feature              Assignee: alice
Created:   2024-01-10 10:00     Updated:  2024-01-15 14:30
Due:       2024-01-20

Description:
  Add a dark mode toggle to the application settings page.

Acceptance Criteria:
  - Toggle saves preference to localStorage
  - System preference detected on first visit
  - Smooth transition animation

Labels: ui, enhancement

Dependencies:
  └── blocks: bd-def456 (CSS refactor)

Blocking:
  └── bd-ghi789 depends on this
```

JSON: Full issue object with all nested data

### 6.8 `ready` Command

**Purpose:** Show issues ready to be worked on.

```bash
bd ready [flags]

Flags:
  --limit, -n <int>           Maximum results (default: 10)
  --assignee, -a <name>       Filter by assignee
  --unassigned                Show only unassigned
  --sort <policy>             Sort policy: hybrid, priority, oldest
                              hybrid: P0-P1 first, then oldest
  --label, -l <label>         Filter by label (AND)
  --label-any <labels>        Filter by labels (OR)
  --type, -t <type>           Filter by type
  --priority, -p <int>        Filter by priority
  --include-deferred          Include deferred issues
  --pretty                    Pretty tree format
```

**Behavior:**
1. Queries issues where:
   - Status is "open" or "in_progress"
   - NOT in blocked_issues_cache
   - NOT deferred (defer_until is NULL or in past)
   - NOT pinned
   - NOT ephemeral
2. Applies additional filters
3. Sorts by policy
4. Returns up to limit

**Sort Policies:**

- **hybrid** (default): P0-P1 issues first by creation date, then P2-P4 by creation date
- **priority**: By priority ascending, then creation date
- **oldest**: By creation date ascending only

**Output:**

Text:
```
Ready to work (3 issues):

○ bd-abc123 [P0] Critical security fix (bug)
○ bd-def456 [P1] Add user authentication (feature)
○ bd-ghi789 [P2] Update documentation (docs)
```

JSON:
```json
{
  "issues": [...],
  "count": 3
}
```

### 6.9 `blocked` Command

**Purpose:** Show blocked issues and what's blocking them.

```bash
bd blocked [flags]

Flags:
  --limit, -n <int>           Maximum results
  --verbose                   Show full blocking chain
```

**Output:**

Text:
```
Blocked issues (2):

● bd-abc123 [P1] Implement OAuth (feature)
  └── Blocked by: bd-xyz789 (open)

● bd-def456 [P2] Add payment flow (feature)
  └── Blocked by: bd-abc123 (in_progress)
```

JSON:
```json
{
  "blocked_issues": [
    {
      "issue": {...},
      "blocked_by": [
        {"id": "bd-xyz789", "status": "open", "title": "..."}
      ]
    }
  ],
  "count": 2
}
```

### 6.10 `dep` Command

**Purpose:** Manage issue dependencies.

```bash
bd dep <subcommand> [flags]

Subcommands:
  add       Add a dependency
  remove    Remove a dependency
  list      List dependencies
  tree      Show dependency tree
  cycles    Detect dependency cycles
```

**`dep add`:**
```bash
bd dep add <issue> <depends-on> [flags]

Arguments:
  issue                Issue that depends on another
  depends-on           Issue being depended on

Flags:
  --type, -t <type>    Dependency type (default: blocks)
                       Values: blocks, parent-child, related, discovered-from,
                               conditional-blocks, waits-for, duplicates, supersedes
  --metadata <json>    Additional metadata (JSON object)
```

Behavior:
1. Resolves both issue IDs
2. Validates dependency type
3. Checks for cycles (if blocking type)
4. Creates dependency record
5. Refreshes blocked cache
6. Creates "dependency_added" event

**`dep remove`:**
```bash
bd dep remove <issue> <depends-on>
```

**`dep list`:**
```bash
bd dep list <issue> [flags]

Flags:
  --direction <dir>    down: what this depends on
                       up: what depends on this
                       both: all (default)
```

**`dep tree`:**
```bash
bd dep tree <issue> [flags]

Flags:
  --max-depth, -d <int>   Maximum depth (default: 10)
  --format <format>       Output format: text, mermaid
```

Output (text):
```
bd-epic-1 [Epic: User Management]
├── bd-task-1 [P1] Design schema
│   ├── bd-task-2 [P2] Implement models
│   │   └── bd-task-3 [P2] Write tests
│   └── bd-task-4 [P2] API endpoints
└── bd-task-5 [P1] Documentation
```

Output (mermaid):
```mermaid
graph TD
    bd-epic-1["Epic: User Management"]
    bd-task-1["P1: Design schema"]
    bd-epic-1 --> bd-task-1
    ...
```

**`dep cycles`:**
```bash
bd dep cycles
```

Detects and reports any cycles in blocking dependencies.

### 6.11 `label` Command

**Purpose:** Manage labels.

```bash
bd label <subcommand> [args]

Subcommands:
  add <issue> <label>     Add label to issue
  remove <issue> <label>  Remove label from issue
  list [issue]            List labels (for issue or all unique labels)
```

### 6.12 `search` Command

**Purpose:** Full-text search across issues.

```bash
bd search <query> [flags]

Arguments:
  query                  Search query

Flags:
  --status, -s <status>  Filter by status
  --type, -t <type>      Filter by type
  --limit, -n <int>      Maximum results (default: 20)
```

**Behavior:**
- Searches title and description fields
- Uses SQLite FTS5 if available
- Falls back to LIKE with wildcards

### 6.13 `stats` Command

**Purpose:** Show project statistics.

```bash
bd stats [flags]

Flags:
  --by-type              Show breakdown by issue type
  --by-priority          Show breakdown by priority
  --by-assignee          Show breakdown by assignee
  --by-label             Show breakdown by label
```

**Output:**

```
Issue Statistics
================

Total:        142
Open:          45  (31.7%)
In Progress:   12  (8.5%)
Closed:        78  (54.9%)
Blocked:        5  (3.5%)
Deferred:       2  (1.4%)

Ready to work: 38
Avg lead time: 4.2 days

By Priority:
  P0:  3 issues
  P1: 15 issues
  P2: 67 issues
  P3: 42 issues
  P4: 15 issues
```

### 6.14 `sync` Command

**Purpose:** Synchronize database with JSONL and optionally git.

```bash
bd sync [flags]

Flags:
  --flush-only           Export to JSONL only (no git operations)
  --import-only          Import from JSONL only
  --dry-run              Show what would change without applying
  --no-pull              Skip git pull before import
  --no-push              Skip git push after export
  --status               Show sync status without making changes
  --message, -m <text>   Custom git commit message
```

**Behavior:**

1. **Import phase** (unless --flush-only):
   - Check if JSONL is newer than database
   - Parse JSONL file
   - Detect collisions (same ID, different content)
   - Merge changes into database

2. **Export phase** (unless --import-only):
   - Get all issues (including tombstones, excluding ephemeral)
   - Populate dependencies, labels, comments
   - Compute content hashes
   - Write to temp file atomically
   - Rename to issues.jsonl

3. **Git phase** (unless --flush-only or --no-push):
   - Stage .beads/issues.jsonl
   - Commit with message
   - Push to remote

### 6.15 `config` Command

**Purpose:** Manage configuration.

```bash
bd config <subcommand> [args]

Subcommands:
  get <key>              Get config value
  set <key> <value>      Set config value
  list                   List all config values
  delete <key>           Delete config key
```

---

## 7. JSONL Import/Export System

### 7.1 Export Error Policies

The export system supports four configurable error handling policies:

| Policy | Behavior | Use Case |
|--------|----------|----------|
| `strict` (default) | Fails immediately on any error, no partial exports | User-initiated exports |
| `best-effort` | Skips failures with stderr warnings, continues | Auto-export background operations |
| `partial` | Retries transient failures with exponential backoff (100ms→200ms→400ms), skips persistent failures | Resilient batch operations |
| `required-core` | Fails on core data (issues/deps), best-effort for enrichments (labels/comments) | Production sync |

**Configuration:**

| Key | Default | Description |
|-----|---------|-------------|
| `export.error_policy` | `"strict"` | Default policy for exports |
| `auto_export.error_policy` | `"best-effort"` | Override for background auto-export |
| `export.retry_attempts` | `3` | Max retry attempts for transient failures |
| `export.retry_backoff_ms` | `100` | Initial backoff (doubles each retry) |
| `export.skip_encoding_errors` | `false` | Skip issues that fail JSON encoding |
| `export.write_manifest` | `false` | Write `.manifest.json` with export metadata |

**Export Manifest Fields** (when enabled):

| Field | Type | Description |
|-------|------|-------------|
| `exported_count` | int | Total issues successfully exported |
| `failed_issues` | array | Issues that failed with reason and missing data |
| `partial_data` | array | Data types that had failures (e.g., `["labels"]`) |
| `warnings` | array | Non-fatal warnings encountered |
| `complete` | bool | True if export had no errors |
| `exported_at` | timestamp | RFC3339 export timestamp |
| `error_policy` | string | Policy used for this export |

### 7.2 File Format

**Location:** `.beads/issues.jsonl`

**Format:** One complete issue JSON object per line. No trailing commas. UTF-8 encoding.

```json
{"id":"bd-abc123","title":"Fix bug","status":"open","priority":1,"issue_type":"bug","created_at":"2024-01-15T10:00:00Z","updated_at":"2024-01-15T10:00:00Z"}
{"id":"bd-def456","title":"Add feature","status":"closed","priority":2,"issue_type":"feature","created_at":"2024-01-14T09:00:00Z","updated_at":"2024-01-15T11:00:00Z","closed_at":"2024-01-15T11:00:00Z"}
```

**Fields exported:**
- All Issue struct fields with `json` tags (except `json:"-"` fields)
- `labels` array (embedded)
- `dependencies` array (embedded)
- `comments` array (embedded)

**Fields NOT exported:**
- `content_hash` (computed, not serialized)
- `source_repo` (internal routing)
- `id_prefix` (internal routing)
- `prefix_override` (internal routing)

**Ephemeral issues:** Issues with `ephemeral: true` are NOT exported.

**Tombstones:** Issues with `status: "tombstone"` ARE exported (for sync).

#### 7.2.1 JSONL File Discovery Rules

When locating the "main" JSONL file for import/export, legacy beads uses a defensive lookup:

1. Prefer `issues.jsonl` if present.
2. Fall back to `beads.jsonl` for legacy compatibility.
3. Never treat `deletions.jsonl`, `interactions.jsonl`, or merge snapshots
   (`beads.base.jsonl`, `beads.left.jsonl`, `beads.right.jsonl`) as the primary JSONL.
4. If nothing suitable exists, default to `issues.jsonl` for writing.

### 7.3 Export Flow

```
┌──────────────────────┐
│  GetAllIssues()      │ ─── Includes tombstones, excludes ephemeral
└──────────┬───────────┘
           │
           v
┌──────────────────────┐
│  Populate relations  │ ─── Labels, dependencies, comments
└──────────┬───────────┘
           │
           v
┌──────────────────────┐
│  Compute hashes      │ ─── ContentHash for each issue
└──────────┬───────────┘
           │
           v
┌──────────────────────┐
│  Write temp file     │ ─── .beads/issues.jsonl.tmp
└──────────┬───────────┘
           │
           v
┌──────────────────────┐
│  Atomic rename       │ ─── mv tmp -> issues.jsonl
└──────────┬───────────┘
           │
           v
┌──────────────────────┐
│  Update metadata     │ ─── Set jsonl_content_hash, last_export_time
└──────────┬───────────┘
           │
           v
┌──────────────────────┐
│  Clear dirty flags   │ ─── ClearDirtyIssues()
└──────────────────────┘
```

**Atomic Write Requirements:**
1. Write to temp file (`.jsonl.tmp`) in same directory
2. Use 2MB buffered writer for performance
3. Disable HTML escaping in JSON encoder (preserve `<`, `>`, `&`)
4. Flush buffer, fsync file, then atomic rename to final path
5. On any error: close file, delete temp, return error

### 7.4 Import Options

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `dry_run` | bool | false | Preview changes without applying |
| `skip_update` | bool | false | Create-only mode (skip existing issues) |
| `strict` | bool | false | Fail on any error |
| `rename_on_import` | bool | false | Rename issues to match database prefix |
| `skip_prefix_validation` | bool | false | Allow any prefix (multi-repo mode) |
| `orphan_handling` | enum | `allow` | How to handle missing parent issues |
| `clear_duplicate_external_refs` | bool | false | Clear duplicate external_ref values |
| `protect_local_export_ids` | map | empty | Timestamp-aware protection (GH#865) |

**Orphan Handling Modes:**

| Mode | Behavior |
|------|----------|
| `strict` | Fail if parent issue not found |
| `resurrect` | Auto-resurrect parent from history |
| `skip` | Skip orphaned issues silently |
| `allow` | Import without parent validation (default) |

**Timestamp-Aware Protection (GH#865):** When `protect_local_export_ids` contains an issue ID with a timestamp, import will skip updating that issue if the incoming `updated_at` is older than the protection timestamp. This prevents import from overwriting recently-exported local changes.

### 7.5 Import Flow

```
┌──────────────────────┐
│  Check staleness     │ ─── Compare mtime + content hash using Lstat()
└──────────┬───────────┘
           │ (if newer)
           v
┌──────────────────────┐
│  Check git markers   │ ─── Detect merge conflicts (<<<<<<<)
└──────────┬───────────┘
           │ (if no conflicts)
           v
┌──────────────────────┐
│  Parse JSONL         │ ─── Stream with 2MB buffer
└──────────┬───────────┘
           │
           v
┌──────────────────────┐
│  Normalize issues    │ ─── Canonicalize refs, compute hashes
└──────────┬───────────┘
           │
           v
┌──────────────────────┐
│  Collision detection │ ─── Same ID, different content
└──────────┬───────────┘
           │
           v
┌──────────────────────┐
│  Begin transaction   │
└──────────┬───────────┘
           │
           v
┌──────────────────────┐
│  Upsert issues       │ ─── INSERT OR REPLACE
└──────────┬───────────┘
           │
           v
┌──────────────────────┐
│  Sync dependencies   │ ─── Delete old, insert new
└──────────┬───────────┘
           │
           v
┌──────────────────────┐
│  Sync labels         │ ─── Delete old, insert new
└──────────┬───────────┘
           │
           v
┌──────────────────────┐
│  Sync comments       │ ─── Delete old, insert new
└──────────┬───────────┘
           │
           v
┌──────────────────────┐
│  Commit transaction  │
└──────────┬───────────┘
           │
           v
┌──────────────────────┐
│  Refresh caches      │ ─── blocked_issues_cache
└──────────┬───────────┘
           │
           v
┌──────────────────────┐
│  Update metadata     │ ─── last_import_time, jsonl_file_hash
└──────────────────────┘
```

### 7.6 Staleness Detection

**Algorithm:** Database is considered stale (needs import) when JSONL file has changed since last import.

**Detection Steps:**
1. Read `last_import_time` from metadata (RFC3339Nano or RFC3339 format)
2. If no metadata: assume fresh (not stale)
3. Get JSONL file mtime using **Lstat()** (NOT Stat())
   - **Critical:** Lstat() returns symlink's own mtime, not target's
   - Required for NixOS and systems where JSONL may be symlinked
4. If `file_mtime > last_import_time`:
   - Compute SHA256 of file content
   - Compare against stored `jsonl_content_hash` (or legacy key `last_import_hash`)
   - Stale only if hashes differ (handles `touch` and unchanged git pulls)

**Metadata Keys:**
| Key | Description |
|-----|-------------|
| `last_import_time` | RFC3339 timestamp of last successful import |
| `jsonl_content_hash` | SHA256 of JSONL file after last import |
| `last_import_hash` | Legacy key (migration compatibility) |

### 7.7 Git Conflict Detection

Before parsing JSONL, scan for git merge conflict markers:
- `<<<<<<< ` (conflict start)
- `=======` (separator)
- `>>>>>>> ` (conflict end)

If any marker found: abort import with error instructing user to resolve conflicts or regenerate with `bd export --force`.

### 7.8 Issue Normalization

Before collision detection, normalize all incoming issues:

| Step | Action | Rationale |
|------|--------|-----------|
| Ephemeral detection | If ID contains `-wisp-`, set `ephemeral=true` | Prevent patrol/workflow instances from polluting `bd ready` |
| Hash recomputation | Recompute `content_hash` from fields | JSONL hashes may be stale or incorrect |
| External ref canonicalization | Canonicalize Linear refs (e.g., slug → ID) | Prevent duplicate issues from different ref formats |

### 7.9 Collision Detection (4-Phase Algorithm)

For each incoming issue, match against database in priority order:

| Phase | Match By | Outcome |
|-------|----------|---------|
| 0 | `external_ref` | Update existing (cross-system sync) |
| 1a | `content_hash` + same ID | Exact match (skip, idempotent) |
| 1b | `content_hash` + different ID | Rename detection |
| 2 | ID only | Update (same ID, different content) |
| 3 | No match | Create new issue |

**Special Cases:**
- **Tombstone protection:** If existing issue is tombstone status, skip incoming (never resurrect deleted issues)
- **Timestamp-aware protection (GH#865):** If issue ID is in `protect_local_export_ids` with timestamp newer than incoming `updated_at`, skip update
- **Cross-prefix duplicates:** Same content hash but different prefix → skip incoming, keep existing

**Collision Resolution (Last-Write-Wins by UpdatedAt):**

| Condition | Action |
|-----------|--------|
| `remote.UpdatedAt > local.UpdatedAt` | Take remote version (remote wins) |
| `remote.UpdatedAt <= local.UpdatedAt` | Skip update (local wins or tie) |

**Tombstone Protection (CRITICAL):**

| Condition | Action | Rationale |
|-----------|--------|-----------|
| Existing issue has `status = tombstone` | Skip incoming issue | Tombstones are permanent; never resurrect deleted issues |

**Duplicate External Reference Detection:**

Validates that no two issues share the same `external_ref` (required for cross-system sync integrity).

| Mode | Behavior |
|------|----------|
| Strict (`clearDuplicates=false`) | Return error listing all duplicate refs |
| Auto-fix (`clearDuplicates=true`) | Keep first occurrence, clear `external_ref` on subsequent |

Detection Algorithm:
1. Build map of `external_ref → [issue_ids]`
2. For each ref with multiple IDs: flag as duplicate
3. In auto-fix mode: set `external_ref = NULL` on all but first issue

### 7.6 Dirty Tracking

Issues are marked dirty on any modification:

| Operation | Mark Dirty |
|-----------|------------|
| CreateIssue | Yes |
| UpdateIssue | Yes |
| CloseIssue | Yes |
| ReopenIssue | Yes |
| DeleteIssue | Yes |
| RestoreIssue | Yes |
| AddDependency | Both issues |
| RemoveDependency | Both issues |
| AddLabel | Yes |
| RemoveLabel | Yes |
| AddComment | Yes |

**Dirty Tracking Operations:**

| Operation | Interface | Behavior |
|-----------|-----------|----------|
| `MarkIssueDirty(issueID)` | Single issue | Upsert into `dirty_issues` with current timestamp |
| `MarkIssuesDirty(issueIDs)` | Batch | Upsert all in single transaction |
| `GetDirtyIssues()` | Query | Return issue IDs ordered by `marked_at` ASC (FIFO) |
| `GetDirtyIssueCount()` | Query | Return count for monitoring |
| `ClearDirtyIssuesByID(issueIDs)` | Cleanup | Delete specific IDs (only those actually exported) |

**Export Hash Operations:**

The `export_hashes` table enables incremental export by tracking content hashes of previously exported issues.

| Operation | Interface | Behavior |
|-----------|-----------|----------|
| `GetExportHash(issueID)` | Query | Return stored content hash, empty string if none |
| `SetExportHash(issueID, hash)` | Upsert | Store hash with `exported_at` timestamp |
| `ClearAllExportHashes()` | Bulk delete | Invalidate all hashes (required before import) |

**JSONL File Hash Tracking:**

| Operation | Interface | Behavior |
|-----------|-----------|----------|
| `GetJSONLFileHash()` | Query | Get `jsonl_file_hash` from metadata table |
| `SetJSONLFileHash(hash)` | Upsert | Store hash in metadata (bd-160) |

### 7.10 Incremental Export Algorithm

1. **Get dirty issues:** Query `dirty_issues` table for issue IDs (FIFO order)
2. **Filter by hash:** For each dirty ID:
   - Load issue from database (skip if deleted)
   - Compute current `content_hash`
   - Compare against stored `export_hash`
   - Only include if hashes differ (content actually changed)
3. **Atomic write:** Write filtered issues to JSONL (see Section 7.3)
4. **Update hashes:** For each exported issue, call `SetExportHash(id, hash)`
5. **Clear dirty flags:** Call `ClearDirtyIssuesByID()` with exported IDs only

**Critical:** Only clear dirty flags for issues that were actually exported. This prevents race conditions where new changes arrive between steps 1 and 5.

### 7.11 Race Condition Prevention (GH#607)

**Problem:** Database reconnection logic could close connections mid-query.

**Solution:** All storage operations acquire a read lock (`reconnectMu.RLock()`) before executing database operations. The reconnect function acquires a write lock, ensuring mutual exclusion.

| Lock Type | Holder | Purpose |
|-----------|--------|---------|
| Read lock | All query/exec operations | Prevent reconnect during operation |
| Write lock | Reconnect function | Ensure no operations in progress |

### 7.12 Export Hash Invalidation on Import (CRITICAL)

Before importing from JSONL, **all export hashes must be cleared** because:
- Imported issues may have different content than exported versions
- Stored hashes would cause false "no change" detection
- Incremental export would skip issues that need re-export

**Invariant:** `ClearAllExportHashes()` must be called at the start of any import operation.

### 7.13 3-Way Merge (Full Sync)

For complex sync scenarios with concurrent local and remote changes, `bd` uses a snapshot-based 3-way merge before pruning deletions.

**Data Sources:**

| Source | Location | Description |
|--------|----------|-------------|
| Base | `.beads/beads.base.jsonl` | Snapshot after last successful import |
| Left | `.beads/beads.left.jsonl` | Snapshot captured after local export, before git pull |
| Right | `issues.jsonl` (post-pull) | Incoming remote state |

**Merge Decision Table:**

| Present In | Action | Rationale |
|------------|--------|-----------|
| Remote only | Import | New issue from remote |
| Local only | Keep | New local issue |
| Base only | Delete | Deleted on remote (tombstone) |
| Left + Right (identical) | No change | Already in sync |
| Left + Right (differ) + Base | See below | 3-way comparison needed |
| Left + Right (differ) - Base | LWW by `UpdatedAt` | No base to determine origin |

**3-Way Comparison (when Base exists):**

| Condition | Action | Rationale |
|-----------|--------|-----------|
| `Left == Base` | Take Right | Local unchanged, remote modified |
| `Right == Base` | Keep Left | Remote unchanged, local modified |
| All three differ | LWW by `UpdatedAt` | Both modified, resolve by timestamp |

**Deletion Pruning (after merge):**
After the merged JSONL replaces `issues.jsonl`, the system computes "accepted deletions":
- Present in Base
- Unchanged in Left
- Missing from merged output

Those IDs are deleted from the database (tombstone or hard delete depending on mode). Missing IDs in the DB are treated as success (already deleted).

---

## 8. Ready/Blocked Logic and Dependency Graph

### 8.1 Ready Work Definition

An issue is "ready to work" if ALL of the following are true (defaults shown; filters can override some):

1. **Status is active:** `status IN ('open', 'in_progress')` (unless an explicit status filter is provided)
2. **Not blocked:** `id NOT IN blocked_issues_cache`
3. **Not deferred:** `defer_until IS NULL OR defer_until <= CURRENT_TIMESTAMP`
4. **Not pinned:** `pinned = 0`
5. **Not ephemeral:** `ephemeral = 0` (or NULL)
6. **Not a wisp:** ID does not match `%-wisp-%` (defense-in-depth for ephemeral issues)
7. **Not an internal workflow type:** if `type` filter is not set, exclude `merge-request`, `gate`, `molecule`, `message`, `agent`

### 8.2 Blocking Calculation

An issue is blocked if ANY of the following conditions are met:

**Direct Blocking:**
- Has a `blocks` dependency on an issue that is NOT closed/tombstone
- Has a `conditional-blocks` dependency on an issue that hasn't failed

**Inherited Blocking (Parent-Child):**
- Has a `parent-child` dependency on an issue that is blocked (transitive)
- Parent is not closed → children are blocked

**Waits-For Blocking:**
- Has a `waits-for` dependency with pending (non-closed) children

**External Blocking (resolved at query time):**
- Has a `blocks` dependency to `external:<project>:<capability>` and the capability is not satisfied
- Capability is satisfied when the external project has a **closed** issue labeled `provides:<capability>`
- External project paths are configured via `external_projects` in config.yaml

### 8.3 Blocked Issues Cache

The `blocked_issues_cache` table is a materialized view rebuilt on:
- Dependency added/removed
- Issue status changed (especially to/from closed)
- Manual refresh request

**Rebuild Algorithm:**

The cache rebuild is a 2-phase process using a recursive CTE:

**Phase 1: Direct Blocking Detection**

Find issues blocked directly by each dependency type:

| Dependency Type | Blocking Condition | Join/Lookup |
|-----------------|-------------------|-------------|
| `blocks` | Blocker has status: `open`, `in_progress`, `blocked`, `deferred`, `hooked` | Join on `depends_on_id` → `issues.id` |
| `conditional-blocks` | Blocker NOT closed, OR closed without failure keyword in `close_reason` | Same join + keyword check |
| `waits-for` (all-children) | ANY child of spawner has non-closed status | Subquery on `parent-child` deps |
| `waits-for` (any-children) | NO children of spawner are closed | Subquery with NOT EXISTS |

**Phase 2: Transitive Propagation**

Using recursive CTE, propagate blockage through `parent-child` relationships:
- Base case: All directly blocked issues (from Phase 1)
- Recursive case: For each blocked issue, find all issues with `parent-child` dependency pointing to it
- Depth limit: 50 levels to prevent infinite recursion
- Final output: DISTINCT union of all blocked issues

**Blocking Semantics by Type:**

| Type | Semantics | Unblocked When |
|------|-----------|----------------|
| `blocks` | B blocked until A completes | A's status is `closed` or `tombstone` |
| `conditional-blocks` | B runs only if A fails | A closed with failure close_reason |
| `waits-for` (all-children) | B waits for spawner's children | ALL children of spawner closed |
| `waits-for` (any-children) | B waits for any child | ANY child of spawner closed |
| `parent-child` | Children inherit parent blocking | Parent unblocked (transitive) |

**External dependencies are intentionally NOT stored in the cache.**
They are checked lazily at query time to avoid holding multiple DB connections during cache rebuild.

**Failure Close Reason Keywords:**
- `failed`, `rejected`, `wontfix`, `won't fix`
- `cancelled`, `canceled`, `abandoned`
- `blocked`, `error`, `timeout`, `aborted`

**Cache Invalidation Triggers:**
- `blocks`, `conditional-blocks`, `waits-for`, or `parent-child` dependency added/removed
- Any issue's status changes (affects whether it blocks others)
- Issue closed (closed issues don't block others; conditional-blocks checks close_reason)

**NOT Invalidation Triggers:**
- `related` dependencies added/removed (informational only)
- `discovered-from` dependencies (provenance tracking only)

**Performance Characteristics:**
- Without cache: O(n²) for n issues (recursive traversal per query)
- With cache: O(1) lookup per issue
- Cache rebuild: O(n × d) where d is average dependency depth
- Typical speedup: 25x for ready work queries

### 8.4 Ready Work Query

**Filter Conditions (all must pass):**

| Condition | Filter |
|-----------|--------|
| Active status | `status IN ('open', 'in_progress')` |
| Not blocked | `id NOT IN blocked_issues_cache` |
| Not deferred | `defer_until IS NULL` OR `defer_until <= now` |
| Not pinned | `pinned = 0` |
| Not ephemeral | `ephemeral = 0` |
| Not wisp | `id NOT LIKE '%-wisp-%'` |

**Sort Order (default = hybrid):**
- **Hybrid (default):**
  - Issues created in the last 48 hours: ordered by priority (ascending).
  - Older issues: ordered by created_at (oldest first).
  - Tie-breaker: created_at (always ascending).
- **Priority:** `ORDER BY priority ASC, created_at ASC`.
- **Oldest:** `ORDER BY created_at ASC`.

**Result:** Limited to requested count (default varies by command)

### 8.5 Cycle Detection

Cycles are only checked for blocking dependency types.

**Algorithm:**
1. Start from source issue
2. Recursively traverse all blocking dependencies (`blocks`, `parent-child`, `conditional-blocks`, `waits-for`)
3. Track visited path to detect cycles
4. Depth limit: 100 levels

**Cycle-Relevant Dependency Types:**

| Type | Check for Cycles |
|------|-----------------|
| `blocks` | Yes |
| `parent-child` | Yes |
| `conditional-blocks` | Yes |
| `waits-for` | Yes |
| `related` | No (informational) |
| `discovered-from` | No (provenance) |

**Detection Method:**
- Maintain comma-separated path string during traversal
- Before adding node: check if already in path
- If target found in reachable set: cycle exists

**Use Cases:**
- Prevent creating dependencies that would cause cycles
- Validate dependency graph integrity
- `bd doctor` cycle check

### 8.6 Dependency Tree Building

Builds a hierarchical tree structure from a root issue following its dependencies.

**DependencyNode Structure:**

| Field | Type | Description |
|-------|------|-------------|
| `issue` | Issue | The issue at this node |
| `children` | []DependencyNode | Child nodes (dependencies) |
| `depth` | int | Distance from root (0 = root) |
| `type` | DependencyType | How this node relates to parent |

**Algorithm:**
1. Start at root issue with `depth = 0`
2. Mark root as visited
3. Query dependencies in "down" direction (issues this one depends on)
4. For each dependency not yet visited and within `maxDepth`:
   - Recursively build subtree
   - Attach as child with dependency type
5. Return tree structure

**Constraints:**
- `maxDepth`: Prevents infinite recursion (configurable)
- `visited` set: Prevents revisiting same issue (handles DAGs)

**Output:** JSON-serializable tree for `bd show --tree` and API responses

---

## 9. Configuration System

### 9.1 Configuration Sources and Precedence

Configuration in legacy beads is split across three places: `config.yaml` (startup settings), `metadata.json` (startup file paths), and the SQLite `config` table (runtime settings). Resolution order is:

1. **Command-line flags** (Cobra) override everything else.
2. **Environment variables** with `BD_` prefix (viper) override config files and DB settings.
3. **Project config**: nearest `.beads/config.yaml` discovered by walking up from CWD.
4. **User config**: `~/.config/bd/config.yaml`, then `~/.beads/config.yaml` (fallback).
5. **SQLite `config` table** for persistent runtime settings (issue prefix, custom status/type, export policy).
6. **Defaults** hardcoded in code.

`metadata.json` (see 9.4) is separate from viper. It is read before DB open and determines the DB path and JSONL name.

### 9.2 YAML-Only Keys (startup settings)

These keys are read before the DB opens and must live in `config.yaml`. `bd config set` writes them to YAML instead of SQLite.

**Bootstrap / runtime behavior:**
- `no-db`, `no-daemon`, `no-auto-flush`, `no-auto-import`, `json`
- `auto-start-daemon`, `lock-timeout`, `flush-debounce`, `remote-sync-interval`

**Database and identity:**
- `db` (override DB filename)
- `actor`, `identity`

**Git and sync:**
- `no-push`, `no-git-ops`, `git.author`, `git.no-gpg-sign`
- `sync-branch` (alias: `sync.branch`)
- `sync.require_confirmation_on_mass_delete`
- `daemon.auto_commit`, `daemon.auto_push`, `daemon.auto_pull`

**Routing and validation:**
- `routing.mode`, `routing.default`, `routing.maintainer`, `routing.contributor`
- `create.require-description`
- `validation.on-create`, `validation.on-sync`
- `hierarchy.max-depth`

**Repo and external mapping:**
- `directory.labels` (directory -> label map)
- `external_projects` (name -> path map)

### 9.3 SQLite Config Keys (per-db, persistent)

These keys live in the SQLite `config` table and are accessed via `bd config get/set` (direct mode only).

**Core behavior:**
- `issue_prefix` (ID prefix, default `bd`)
- `status.custom`, `types.custom` (comma-separated custom values)
- `import.orphan_handling` (`allow`, `skip`, `strict`, `resurrect`)

**Export reliability:**
- `export.error_policy`, `auto_export.error_policy`
- `export.retry_attempts`, `export.retry_backoff_ms`
- `export.skip_encoding_errors`, `export.write_manifest`

**ID generation tuning:**
- `max_collision_prob`
- `min_hash_length`
- `max_hash_length`

### 9.4 metadata.json (startup file config)

`.beads/metadata.json` controls file locations and backend selection:

| Field | Purpose |
|-------|---------|
| `database` | DB filename (default `beads.db`) |
| `jsonl_export` | JSONL filename (default `issues.jsonl`) |
| `backend` | `sqlite` or `dolt` (classic port should ignore dolt) |
| `deletions_retention_days` | Legacy deletions manifest retention (default 3) |

If `metadata.json` is missing, `bd` uses defaults. Legacy `config.json` is auto-migrated to `metadata.json`.

### 9.5 Environment Variables

Viper uses a `BD_` prefix and maps dots/hyphens to underscores. Examples:

```bash
BD_NO_DAEMON=true
BD_ISSUE_PREFIX=proj
BD_SYNC_REQUIRE_CONFIRMATION_ON_MASS_DELETE=true
```

Legacy compatibility env vars (explicitly bound, without `BD_`):
- `BEADS_FLUSH_DEBOUNCE`
- `BEADS_AUTO_START_DAEMON`
- `BEADS_IDENTITY`
- `BEADS_REMOTE_SYNC_INTERVAL`

### 9.6 Metadata Table Keys (internal)

The `metadata` table is internal state. Keys are feature-specific and may be namespaced. Common keys include:
- `schema_version` (migration version)
- `bd_version` (db version tracking)
- `repo_id`, `clone_id` (multi-repo sync bookkeeping)
- `jsonl_content_hash`, `last_import_time`, `last_import_hash` (staleness checks)
- `jsonl_content_hash:<repo>`, `last_import_mtime:<repo>` (per-repo in multi-repo mode)

`br` should treat these as opaque unless a specific feature requires them.

---

## 10. Validation Rules

### 10.1 Issue Validation

**Title:**
- Required: Cannot be empty
- Max length: 500 characters
- Trimmed: Leading/trailing whitespace removed

**Description/Design/AcceptanceCriteria/Notes:**
- Optional
- No max length (limited by SQLite TEXT)

**Status:**
- Must be one of: `open`, `in_progress`, `blocked`, `deferred`, `closed`, `tombstone`, `pinned`
- Custom statuses allowed if configured

**Priority:**
- Must be integer 0-4
- Also accepts strings: `P0`, `P1`, `P2`, `P3`, `P4`, `critical`, `high`, `medium`, `low`, `backlog`

**Issue Type:**
- Must be one of: `bug`, `feature`, `task`, `epic`, `chore`, `docs`, `question`
- Custom types allowed if configured

**Timestamps:**
- `created_at`: Set automatically, cannot be changed
- `updated_at`: Set automatically on any change
- `closed_at`: Set automatically when status changes to `closed`
- `deleted_at`: Set automatically when status changes to `tombstone`

**External Ref:**
- Must be unique across all issues (if set)
- Format: Any non-empty string (typically `system-id` like `gh-123`)

### 10.2 Dependency Validation

**Issue IDs:**
- Both `issue_id` and `depends_on_id` must exist (for local deps)
- `depends_on_id` may reference external issue (no FK)

**Type:**
- Must be valid DependencyType
- Blocking types checked for cycles

**Self-Reference:**
- `issue_id` cannot equal `depends_on_id`

**Duplicates:**
- Same (issue_id, depends_on_id) pair cannot exist twice

### 10.3 Label Validation

**Label Name:**
- Cannot be empty
- Max length: 100 characters
- Trimmed: Leading/trailing whitespace removed
- Case-sensitive: `Bug` and `bug` are different labels

### 10.4 Comment Validation

**Author:**
- Required: Cannot be empty

**Text:**
- Required: Cannot be empty
- No max length

### 10.5 ID Validation

**Format:**
- Pattern: `{prefix}-{hash}` (e.g., `bd-3d0f9a`)
- Prefix: Configured via `issue_prefix` (hyphen is normalized if omitted)
- Hash: base36 lowercase (0-9, a-z), adaptive length 3-8
- Hierarchical IDs append numeric suffixes: `bd-abc123.1`, `bd-abc123.1.2`

**Resolution:**
- Exact ID match via `SearchIssues` first (consistent with `bd list --id`)
- Then normalized prefix match (adds prefix if missing)
- Then substring match against the hash portion across all prefixes
- Error on ambiguity with a list of candidates

---

## 11. ID Generation and Content Hashing

### 11.1 Issue ID Generation

Legacy beads uses **base36 hash IDs** with an **adaptive length** strategy:

- **Inputs to hash:** `title`, `description`, `creator/actor`, `created_at` (UnixNano), and a `nonce`.
- **Hash:** SHA256 of the combined string.
- **Encoding:** base36 (0-9, a-z) for higher density than hex.
- **Length:** adaptive 3-8 chars based on database size and collision probability (see 11.1.1).
- **Collision handling:** for each length, try nonces 0..9; if all collide, increase length up to 8.

This means IDs are deterministic for the same input tuple, but still safe under collisions due to nonce fallback.

#### 11.1.1 Adaptive Length Heuristic

The length is chosen to keep collision probability under a threshold using a birthday paradox approximation:

- **Default threshold:** 25% (`max_collision_prob`)
- **Defaults:** min length 3, max length 8
- **Counted scope:** top-level issues only (children do not affect length)
- **Config keys:** `max_collision_prob`, `min_hash_length`, `max_hash_length`

Length increases automatically as the project grows (3 -> 4 -> 5 -> ... -> 8).

### 11.2 Hierarchical IDs (Dotted)

Child issues under a parent can use dotted notation:

```
bd-epic1       (parent epic)
bd-epic1.1     (first child)
bd-epic1.2     (second child)
bd-epic1.2.1   (grandchild)
```

Generation uses the `child_counters` table to atomically increment the next child number per parent. When importing explicit child IDs, the counter is updated to at least the observed child number (to avoid collisions on future auto-generated children).

**Hierarchy depth:** limited by `hierarchy.max-depth` (default 3). Exceeding the depth is a validation error.

### 11.3 Content Hash Computation

Content hash is SHA256 of normalized issue content, used for:
- Change detection (has issue changed since last export?)
- Deduplication (is this the same issue as another?)
- Collision detection (same ID, different content?)

**Algorithm notes:**
- Uses a stable, ordered list of fields and inserts a null separator between each field.
- Includes core content, workflow fields, assignment, external refs, and flags (pinned/template).
- In legacy beads, additional Gastown fields are also included in the hash (HOP, gate, agent, molecule fields).
- **Labels, dependencies, and comments are NOT part of the content hash.**

For the classic Rust port, the hash should include **only classic fields** while preserving the **same order and separator rules** so that legacy and Rust hashes agree when those fields are present.

**Fields NOT included (they change without content change):**
- `ID` (identity, not content)
- `ContentHash` (self-referential)
- `CreatedAt`, `UpdatedAt`, `ClosedAt`, `DeletedAt` (metadata)
- `CompactionLevel`, `CompactedAt`, etc. (compaction metadata)
- Internal routing fields (`SourceRepo`, `IDPrefix`, `PrefixOverride`)

---

## 12. Key Architectural Patterns

### 12.1 Non-Invasive Design (br vs bd)

The Rust port (`br`) is designed to be LESS invasive than the Go version (`bd`):

| Feature | `bd` (Go) | `br` (Rust) |
|---------|-----------|-------------|
| Auto git hooks | Yes (installed by default) | No |
| Auto git commit | Yes (after changes) | No |
| Auto git push | Yes (with hooks) | No |
| Background daemon | Yes (default) | No |
| RPC server | Yes | No |
| Auto-import on query | Yes | Yes (simple check) |
| Auto-export after change | Yes (debounced) | Yes (explicit) |

**Explicit Operations Only:**

```bash
# br requires explicit sync
br create "New issue"           # Creates in DB only
br sync --flush-only            # Exports to JSONL
git add .beads/ && git commit   # User's responsibility
git push                        # User's responsibility
```

### 12.2 Last-Touched Issue

Commands that don't specify an issue ID default to the last touched issue:

```bash
br create "Fix bug"     # Creates bd-abc123, sets as last touched
br update --priority 0  # Updates bd-abc123 (implicit)
br show                 # Shows bd-abc123 (implicit)
br close                # Closes bd-abc123 (implicit)
```

**Implementation:**

```go
var lastTouchedID string

func SetLastTouched(id string) {
    lastTouchedID = id
}

func GetLastTouched() string {
    return lastTouchedID
}

func ResolveIssueID(input string) string {
    if input == "" || input == "." {
        return GetLastTouched()
    }
    return input
}
```

### 12.3 Partial ID Resolution

Users can specify partial IDs for convenience:

```bash
br show abc       # Matches bd-abc123 if unique
br show bd-abc    # Also matches bd-abc123
br close abc def  # Closes bd-abc123 and bd-def456
```

**Resolution Algorithm:**

```go
func ResolvePartialID(prefix string) (string, error) {
    // 1. Try exact match
    issue, err := store.GetIssue(ctx, prefix)
    if err == nil {
        return issue.ID, nil
    }

    // 2. Try prefix match
    matches, _ := store.ListIssues(ctx, &IssueFilter{
        IDPrefix: prefix,
        Limit: 2,  // Only need to know if more than 1
    })

    switch len(matches) {
    case 0:
        return "", ErrNotFound
    case 1:
        return matches[0].ID, nil
    default:
        return "", fmt.Errorf("ambiguous ID prefix '%s': matches %d issues", prefix, len(matches))
    }
}
```

### 12.4 Atomic File Operations

All file writes use the atomic write pattern:

1. Write to temporary file in same directory
2. Flush and sync to disk
3. Atomic rename to target path
4. Delete temp file on error

This ensures:
- No partial writes visible
- Power failure safety
- Concurrent read safety

### 12.5 Output Formatting

**Status Icons (Unicode):**

| Status | Icon | Description |
|--------|------|-------------|
| open | ○ | Empty circle |
| in_progress | ◐ | Half-filled circle |
| blocked | ● | Filled circle |
| deferred | ❄ | Snowflake |
| closed | ✓ | Checkmark |
| tombstone | ✗ | X mark |
| pinned | 📌 | Pin |

**Priority Colors:**

| Priority | Color | ANSI Code |
|----------|-------|-----------|
| P0 | Red | `\x1b[31m` |
| P1 | Orange/Yellow | `\x1b[33m` |
| P2 | Blue | `\x1b[34m` |
| P3 | Cyan | `\x1b[36m` |
| P4 | Gray | `\x1b[90m` |

**Tree Rendering:**

```
├── Branch with sibling
│   └── Last child
└── Last branch
    ├── Child 1
    └── Child 2
```

---

## 13. Error Handling

### 13.1 Error Types

```rust
#[derive(Debug, thiserror::Error)]
pub enum BeadsError {
    #[error("issue not found: {0}")]
    NotFound(String),

    #[error("invalid issue ID: {0}")]
    InvalidID(String),

    #[error("ambiguous ID prefix '{0}': matches {1} issues")]
    AmbiguousID(String, usize),

    #[error("dependency cycle detected")]
    CycleDetected,

    #[error("conflict: {0}")]
    Conflict(String),

    #[error("validation error: {0}")]
    Validation(String),

    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("locked: database is locked")]
    Locked,
}
```

### 13.2 Exit Codes

| Code | Meaning |
|------|---------|
| 0 | Success |
| 1 | General error |
| 2 | Invalid arguments |
| 3 | Issue not found |
| 4 | Validation error |
| 5 | Database error |
| 6 | Cycle detected |
| 7 | Conflict |

### 13.3 Error Messages

Follow this format for user-facing errors:

```
Error: <brief description>

<detailed explanation if needed>

Hint: <suggestion for resolution>
```

Example:

```
Error: Issue not found: bd-xyz

No issue matches the ID 'bd-xyz'.

Hint: Use 'br list' to see available issues, or check the ID spelling.
```

---

## 14. Porting Considerations

### 14.1 Rust Type Mapping

| Go Type | Rust Type | Notes |
|---------|-----------|-------|
| `string` | `String` | |
| `*string` | `Option<String>` | |
| `int` | `i32` | SQLite INTEGER is i64, but priority is 0-4 |
| `int64` | `i64` | For IDs, counts |
| `*int` | `Option<i32>` | |
| `float32` | `f32` | |
| `*float32` | `Option<f32>` | |
| `float64` | `f64` | |
| `bool` | `bool` | |
| `time.Time` | `DateTime<Utc>` | chrono crate |
| `*time.Time` | `Option<DateTime<Utc>>` | |
| `time.Duration` | `std::time::Duration` | |
| `[]string` | `Vec<String>` | |
| `[]T` | `Vec<T>` | |
| `map[string]T` | `HashMap<String, T>` | |
| `map[string]interface{}` | `HashMap<String, serde_json::Value>` | |
| `error` | `Result<T, BeadsError>` | |
| `context.Context` | Implicit or `&self` | |

### 14.2 Serde Configuration

```rust
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub id: String,

    #[serde(skip_serializing, skip_deserializing)]
    pub content_hash: String,

    pub title: String,

    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assignee: Option<String>,

    pub priority: i32,  // Never skip, 0 is valid

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<Dependency>,

    #[serde(with = "chrono::serde::ts_seconds")]
    pub created_at: DateTime<Utc>,

    #[serde(default, skip_serializing_if = "Option::is_none", with = "chrono::serde::ts_seconds_option")]
    pub closed_at: Option<DateTime<Utc>>,
}
```

### 14.3 Key Crates

| Purpose | Crate | Version |
|---------|-------|---------|
| CLI parsing | `clap` | 4.x with derive |
| SQLite | `rusqlite` | Latest, bundled feature |
| JSON | `serde` + `serde_json` | Latest |
| Time | `chrono` | Latest, serde feature |
| Hashing | `sha2` | Latest |
| Parallel | `rayon` | Latest |
| Logging | `tracing` | Latest |
| Errors | `anyhow` + `thiserror` | Latest |
| Colors | `colored` or `termcolor` | Latest |
| Tables | `comfy-table` or `tabled` | Latest |

### 14.4 Schema Compatibility

**Critical:** The Rust implementation MUST use the same SQLite schema as Go beads. This allows:
- Cross-tool usage (run `bd` and `br` on same `.beads/`)
- Migration from Go to Rust without data conversion
- Shared JSONL format for git sync

**Schema Verification:**

```rust
fn verify_schema_compatibility(conn: &Connection) -> Result<()> {
    // Check all required tables exist
    let required_tables = [
        "issues", "dependencies", "labels", "comments", "events",
        "config", "metadata", "dirty_issues", "export_hashes",
        "blocked_issues_cache", "child_counters", "schema_migrations"
    ];

    for table in required_tables {
        let exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?)",
            [table],
            |row| row.get(0)
        )?;

        if !exists {
            return Err(anyhow!("Missing required table: {}", table));
        }
    }

    Ok(())
}
```

### 14.5 Output Compatibility

JSON output must be character-for-character identical to Go beads for machine consumers:

```rust
// Use serde_json with these settings
let json = serde_json::to_string(&issue)?;  // Compact, no pretty-print

// For JSONL, one object per line, no trailing newline on last line
for issue in issues {
    writeln!(file, "{}", serde_json::to_string(&issue)?)?;
}
```

### 14.6 Priority Order for Implementation

1. **Phase 1: Core Data Types**
   - Issue, Dependency, Comment, Event structs
   - Status, IssueType, DependencyType enums
   - Validation functions
   - Content hash computation

2. **Phase 2: SQLite Storage**
   - Connection management with pragmas
   - Schema creation and migration
   - Basic CRUD operations
   - Dirty tracking
   - Blocked cache

3. **Phase 3: Basic CLI**
   - `init`, `create`, `update`, `close`
   - `list`, `show`, `ready`, `blocked`
   - `stats`, `config`

4. **Phase 4: Dependencies**
   - `dep add`, `dep remove`, `dep list`
   - `dep tree`, `dep cycles`
   - Cycle detection
   - Blocked cache refresh

5. **Phase 5: JSONL System**
   - Export flow
   - Import flow
   - Dirty tracking
   - Collision detection

6. **Phase 6: Sync Command**
   - `sync --flush-only`
   - `sync --import-only`
   - Status reporting

7. **Phase 7: Polish**
   - `search` command
   - `label` command
   - Output formatting
   - Error messages
   - Documentation

---

## Appendix A: Important Invariants

### Closed-At Invariant

```
IF status == "closed" THEN closed_at MUST be set (non-NULL)
IF status NOT IN ("closed", "tombstone") THEN closed_at MUST be NULL
```

Enforced by SQLite CHECK constraint.

### Tombstone Invariant

```
IF status == "tombstone" THEN deleted_at MUST be set
IF status != "tombstone" THEN deleted_at SHOULD be NULL
```

### Priority Range

```
0 <= priority <= 4
```

Enforced by SQLite CHECK constraint.

### Title Length

```
1 <= len(title) <= 500
```

Enforced by SQLite CHECK constraint and application validation.

### Cycle Prevention

Blocking dependencies (`blocks`, `parent-child`, `conditional-blocks`, `waits-for`) cannot form cycles. Enforced by application logic before insert.

### ID Uniqueness

All issue IDs must be unique. Enforced by PRIMARY KEY constraint.

### External Ref Uniqueness

All non-NULL external_ref values must be unique. Enforced by UNIQUE index.

---

## 15. Additional Legacy Findings (2026-01-16)

This section captures additional behaviors discovered in the legacy codebase, expressed as implementation guidance for the Rust port. These are intentionally **synthesized** (not verbatim code) and focused on the non-Gastown feature set.

### 15.1 Transaction Semantics (Storage Interface)

The storage layer exposes both a full `Storage` interface and a narrower `Transaction` interface. A transaction is expected to:

- Use **BEGIN IMMEDIATE** for SQLite (acquire write lock early, avoid deadlocks).
- Provide **read-your-writes** within the transaction (e.g., `GetIssue`/`SearchIssues` reflect prior calls in the same txn).
- Roll back on **any error** or **panic**, commit only on clean return.
- Be used for multi-step operations (create issue + dependencies + labels + events) to ensure atomicity.

The interface also allows **UnderlyingDB/UnderlyingConn** access for extensions. For `br`, keep these as *optional escape hatches* but avoid using them directly in core commands.

### 15.2 ID Generation: Base36 + Adaptive Length

The effective ID scheme in current legacy beads is **base36**, not hex:

- Hash material: `title | description | creator | created_at (ns) | nonce`.
- Encoding: SHA256 → base36 string of **length 3–8**.
- Collision handling: for each length, try **up to 10 nonces**, then increase length.
- **Adaptive length** is computed from DB size using a birthday-paradox threshold (default 25% collision probability). Config keys:
  - `max_collision_prob`
  - `min_hash_length`
  - `max_hash_length`
- Hierarchical children use **parent.N** with a `child_counters` table; maximum depth defaults to 3.

**Porting note:** This supersedes earlier “hex short ID” assumptions. `br` should implement base36 adaptive length for true parity.

### 15.3 Partial ID Resolution (CLI Behavior)

Short ID resolution is more permissive than strict prefix matching:

- Accepts full IDs, bare hashes, and prefixes without hyphen.
- Attempts exact match first; then exact match on normalized prefix; then **substring match** across all issue IDs.
- If ambiguous, returns an explicit “ambiguous ID” error listing candidates.

This behavior matters for UX parity and test harness compatibility.

### 15.4 Ready/Blocked Semantics and Cache

Ready work uses a **materialized cache** (`blocked_issues_cache`) instead of a recursive CTE:

Blocking types handled:
- `blocks`
- `conditional-blocks` (blocked unless dependency closes with a **failure** reason)
- `waits-for` (blocked until children of a “spawner” close; gate types: `all-children` or `any-children`)
- `parent-child` (transitive propagation down the hierarchy)

Key behaviors:
- **Pinned** issues are excluded from ready work.
- **Ephemeral** issues (wisps) are excluded; `*-wisp-*` IDs are a defense-in-depth filter.
- Default ready filter includes `open` and `in_progress`.
- `hooked` status counts as blocking in the cache (even if Gastown is excluded).

Cache invalidation:
- Rebuild is triggered on dependency changes (blocking types only) and on status changes.
- Rebuild is **full** (DELETE + INSERT) within the same transaction for consistency.

### 15.5 Conditional-Blocks Failure Heuristic

For `conditional-blocks`, “failure” is detected by **close_reason keywords**:

`failed`, `rejected`, `wontfix`, `won't fix`, `cancelled`, `canceled`, `abandoned`, `blocked`, `error`, `timeout`, `aborted`

If the blocker closes **without** failure, the dependent remains blocked.

### 15.6 External Dependency Resolution

External dependencies are encoded as:

```
external:<project>:<capability>
```

They are satisfied when the referenced project has a **closed** issue labeled:

```
provides:<capability>
```

Resolution details:
- Projects are configured via `external_projects` in config.
- The check opens the external project’s `.beads` DB **once per project** (batched by capability).
- External deps are **not** included in blocked cache rebuilds; they are evaluated at query time.

### 15.7 JSONL File Selection (Safety)

When locating a JSONL file in `.beads/`, legacy code:

1. **Prefers** `issues.jsonl` (canonical).
2. Falls back to `beads.jsonl` (legacy).
3. Otherwise uses any `.jsonl` **except**:
   - `deletions.jsonl`
   - `interactions.jsonl`
   - merge artifacts: `beads.base.jsonl`, `beads.left.jsonl`, `beads.right.jsonl`

This avoids accidental reads of conflict files or deletions logs.

### 15.8 Import Pipeline Details (High-Fidelity)

Import flow (non-daemon and daemon paths both use this core logic):

- **Normalize**:
  - Recompute `content_hash` for all incoming issues (trust DB, not JSONL).
  - If ID contains `-wisp-`, set `ephemeral = true` (prevent ready pollution).
  - Canonicalize **Linear** external refs to avoid duplicates.
- **Prefix handling**:
  - If multi-repo config is active, prefix validation is **skipped**.
  - Otherwise, prefixes are validated against `issue_prefix` and `allowed_prefixes`.
  - Tombstoned issues with mismatched prefixes are tolerated (do not block import).
- **Collision handling**:
  - Uses the 4-phase matching algorithm already summarized in §7 (external_ref → content_hash → ID → new).
  - Tombstones never resurrect.
  - Timestamp protection (`protect_local_export_ids`) can skip older updates.
- **Write path**:
  - Clear `export_hashes` before import (prevents stale dedup).
  - Import via transaction; dependencies + labels + comments follow.
  - WAL checkpoint is attempted post-import (non-fatal if it fails).

### 15.9 Foreign Keys and Orphan Dependencies

To allow `external:*` dependencies, legacy migrations **remove the FK** on `depends_on_id`. As a result:

- Imports temporarily disable foreign keys for speed and order-independence.
- A **manual orphan check** is run post-import:
  - Any dependency whose `depends_on_id` doesn’t exist **and** does not start with `external:` is treated as an error.

**Porting note:** Keep this behavior; do not reintroduce strict FK on `depends_on_id`.

### 15.10 Multi-Repo Hydration (Read-Only Aggregation)

When `.beads/config.yaml` includes `repos.primary`/`repos.additional`, the SQLite storage hydrates issues from multiple repos:

- Reads each repo’s `.beads/issues.jsonl` and merges into the primary DB.
- Tracks JSONL **mtime** in `repo_mtimes` to skip unchanged repos.
- Uses **Lstat** (not Stat) to respect symlinks (NixOS fix).
- Sets `source_repo` on imported issues to the repo-relative path.

### 15.11 Validation Guards in Mutating Commands

Mutation operations are guarded by a composable validator chain:

- **Exists** check (error: issue not found).
- **NotTemplate** (templates are read-only).
- **NotPinned** unless `--force`.
- **NotHooked** unless `--force`.
- **NotClosed** (for close / state transitions).

**Porting note:** These checks should remain even if Gastown features are excluded; “pinned” and “template” are still part of the classic model.

### 15.12 Issue Listing and Search Semantics (SQLite)

Legacy "list" behavior is effectively `SearchIssues` with filters; there is no separate list-specific query. Important characteristics:

- **Text search** uses simple `LIKE` (no FTS):
  - Query string matches `title`, `description`, or `id`.
  - `TitleSearch` is a **title-only** `LIKE`.
  - `TitleContains`, `DescriptionContains`, `NotesContains` each add additional `LIKE` clauses.
- **Default tombstone behavior**: if `Status` is not explicitly set and `IncludeTombstones` is false, tombstones are excluded (`status != tombstone`).
- **No implicit filtering** of `ephemeral`, `pinned`, or `template` issues unless the filter explicitly requests it.
- **Labels**:
  - `Labels` is AND semantics (issue must have **all** labels).
  - `LabelsAny` is OR semantics (issue must have **at least one** label).
- **ID matching**:
  - `IDs` is a strict `IN` list.
  - `IDPrefix` is `LIKE 'prefix%'`.
- **Scheduling filters**: `defer_until` and `due_at` are compared using RFC3339 strings.
  - `Overdue` means `due_at < now AND status != closed`.
- **Ordering**: results are ordered by **priority ASC**, then **created_at DESC** (newer first within priority).
- **Pagination**: `Limit` is supported; no offset in this path.
- **Relations**: `SearchIssues` returns issue rows only; it does **not** populate labels/dependencies/comments.
- **Daemon safety**: a read lock is held during queries to avoid reconnect races (daemon mode).

**Porting note:** This ordering and the lack of implicit `ephemeral`/`pinned` filtering can diverge from UX expectations; preserve for parity, then consider optional improvements.

### 15.13 Statistics Semantics (SQLite)

`GetStatistics` is computed via aggregate SQL with some important quirks:

- `TotalIssues` excludes tombstones; tombstones are counted separately.
- `PinnedIssues` are counted explicitly.
- `DeferredIssues` are counted explicitly.
- `BlockedIssues` count is based **only** on `blocks` dependencies (not conditional-blocks, waits-for, or parent-child).
- `ReadyIssues` count uses a simple rule:
  - status = `open`
  - no `blocks` dependency whose blocker is in `open|in_progress|blocked|deferred|hooked`
  - (Note: this does **not** use the blocked cache, and does not consider conditional-blocks or waits-for.)
- `AverageLeadTime` is computed in hours via `julianday(closed_at) - julianday(created_at)`.
- `EpicsEligibleForClosure` counts epics where **all** children are closed.

**Porting note:** These stats intentionally do **not** share the full ready/blocked logic. Maintain this behavior for parity; if we want to reconcile in `br`, it should be a deliberate change.

### 15.14 Import Collision Resolution (Detailed)

The importer uses a **content-first** merge strategy with strict safety checks. The actual flow is a superset of the “4‑phase collision table” in §7 and includes prefix, tombstone, and timestamp protection.

**Pre-import normalization:**
- **External refs canonicalization**: Linear external refs are normalized to canonical IDs (prevents duplicates from slug vs ID).
- **Content hashes recomputed** for every incoming issue; JSONL hash is never trusted.
- **Wisp detection**: IDs containing `-wisp-` are forced to `ephemeral=true` to keep them out of ready work.
- **export_hashes cleared** before import (staleness logic must be rebuilt after import).

**Prefix mismatch handling:**
- Configured prefix comes from DB `issue_prefix`; allowed prefixes can be expanded via `allowed_prefixes`.
- In **multi‑repo mode**, prefix validation is skipped entirely.
- If mismatches are **only tombstones**, they are filtered out silently (treated as noise).
- Non‑tombstone mismatches:
  - If `--rename-on-import`: prefixes are rewritten **and** all internal references are updated (title/description/design/notes/acceptance, dependency IDs, comment text).
  - Otherwise: import fails with a prefix mismatch error (unless `--dry-run`).

**Batch de‑dup (within the JSONL batch):**
- Duplicate **content_hash** → skip (first wins).
- Duplicate **ID** → skip (first wins).

**Tombstone protection (highest priority):**
- If the DB contains a tombstone for the incoming ID, the issue is **skipped immediately** (no resurrection).

**Match phases (priority order):**
1. **external_ref match** (if present):
   - Update only if `incoming.updated_at` is newer (timestamp check).
   - `assignee` and `external_ref` are cleared if incoming is empty.
   - `pinned` is only updated when explicitly `true` in JSONL (false is treated as “no change”).
2. **content hash match**:
   - Same content + same ID → idempotent (unchanged).
   - Same content + different ID:
     - If **different prefixes**, treat as cross‑project duplicate → skip.
     - If **same prefix**, treat as a rename (delete old ID, create new ID). **No global text reference rewrite** is performed here.
3. **ID collision (same ID, different content)**:
   - Treated as an **update** if incoming is newer.
   - If older or equal, skip or mark unchanged.
4. **No match** → create new issue.

**Timestamp-aware protection (GH#865):**
- If a local export timestamp is newer than incoming `updated_at`, the update is blocked (skip).

**Creation ordering:**
- New issues are sorted by hierarchy depth and created depth‑by‑depth (0..3) to ensure parents exist.
- In `orphan_skip` mode, hierarchical children with missing parents are filtered out **before** creation.

**Dependency/label/comment import:**
- Dependencies:
  - Added only if missing; duplicates ignored.
  - FK errors (missing refs) are skipped with warnings and recorded in results.
  - In strict mode, non‑FK errors abort the import.
- Labels:
  - Only missing labels are added; existing labels are left untouched.
- Comments:
  - De‑duped by `author + trimmed text`.
  - Imported with original timestamps using `ImportIssueComment`.

**Porting note:** These rules are subtle but critical for sync correctness, especially in multi‑clone workflows.

### 15.15 CLI Flag Semantics (Classic Commands)

This subsection captures flag behavior for the **core** non-Gastown CLI commands. We only include flags relevant to classic beads; Gastown-specific flags are intentionally omitted from the Rust port.

#### create

Accepted inputs:
-- Title is positional OR `--title`; if both are set they must match.
- `--file` parses a markdown file to create multiple issues; `--dry-run` is **not** supported with `--file`.

Core flags:
- `--type` (default `task`), `--priority` (default `2`, supports `P0..P4`).
- Common content flags: `--description` (via `--body` aliases), `--design`, `--acceptance`, `--notes`.
- `--labels` / `--label` (alias); labels are normalized (trim + dedupe).
- `--id` explicit ID, `--parent` hierarchical child, `--deps` dependency list, `--external-ref`.
- `--due`, `--defer` support relative/natural time; `--defer` warns if in the past.
- `--silent` prints only ID and suppresses warnings; otherwise prints a summary and sets “last touched”.

Behavioral notes:
- Creating title starting with “test” triggers a warning unless `--silent`.
- If `create.require-description` is enabled, `--description` is mandatory (unless “test” issue).
- If `--id` is provided, the prefix is validated against `issue_prefix` (and `allowed_prefixes`); `--force` bypasses this check.
- If `--type=agent` with an explicit ID, the ID must match the agent naming pattern (Gastown-only; classic port excludes).
- If `--parent` is provided in direct mode, `bd` verifies the parent exists and auto-generates the child ID (`parent.N`).
- `--deps` accepts `type:id` or `id` (defaults to `blocks`), invalid types are warned and skipped.
- `--parent` adds a `parent-child` dependency.
- If any dependency uses `discovered-from:<parent>`, the new issue inherits `source_repo` from that parent (when available).
- `--waits-for` / `--waits-for-gate` are supported in legacy, but are Gastown-oriented; classic port can omit.
- After success: marks dirty, schedules auto-flush, fires hooks, sets last touched ID.

#### update

Accepted inputs:
- Accepts multiple IDs; if none, uses “last touched”.
- Uses partial ID resolution (same rules as show/close).

Core flags:
- `--status`, `--priority`, `--title`, `--assignee`, `--type`, `--estimate`.
- Common content flags: `--description`, `--design`, `--acceptance`, `--notes`.
- `--add-label`, `--remove-label`, `--set-labels`.
- `--parent` reparent (empty string removes parent).
- `--due`, `--defer` (empty string clears).
- `--claim` sets assignee to actor and status to `in_progress` **atomically** (fails if already claimed).
- `--session` sets `closed_by_session` when status is `closed` (flag or `CLAUDE_SESSION_ID` env var).

Behavioral notes:
- Only changed flags are applied; `P0` is valid and must be detected via `Flags().Changed`.
- Empty `--due` / `--defer` explicitly clears the field.
- `--parent` removes existing parent-child edge, then adds a new one (if non-empty) after validating parent exists.
- `--claim` executes even when no other updates are provided.
- If no updates (and not claiming), prints “No updates specified”.
- After updates: marks dirty, schedules flush, sets last touched to first updated ID, fires hooks.

#### close

Accepted inputs:
- Accepts multiple IDs; if none, closes “last touched”.
- Partial ID resolution; cross-repo routing is honored in direct mode.

Core flags:
- `--reason` (default “Closed”), `--resolution` alias.
- `--force` bypasses pinned/template checks.
- `--suggest-next` outputs newly-unblocked issues (single issue only).
- `--continue` + `--no-auto` advances to next step (molecule flow; Gastown).
- `--session` sets `closed_by_session`.

Behavioral notes:
- `--suggest-next` and `--continue` require a single ID.
- If not forced, **close is blocked** if the issue has open blockers (`IsBlocked` check).
- Closing triggers hook, marks dirty, schedules flush.
- In direct mode, `--suggest-next` JSON output is `{ closed: [...], unblocked: [...] }` when unblocked issues exist.
- `--continue` requires direct DB access; daemon mode prints a hint.

#### list

Core flags (subset for classic port):
- Filters: `--status`, `--assignee`, `--type`, `--label`, `--label-any`, `--id`, `--priority`, `--priority-min`, `--priority-max`.
- Text/pattern: `--title`, `--title-contains`, `--desc-contains`, `--notes-contains`.
- Date ranges: `--created-after|before`, `--updated-after|before`, `--closed-after|before`.
- Empty checks: `--empty-description`, `--no-assignee`, `--no-labels`.
- Scheduling: `--deferred`, `--defer-after|before`, `--due-after|before`, `--overdue`.
- Output: `--long`, `--format` (template/digraph/dot), `--pretty` / `--tree`, `--watch`, `--sort`, `--reverse`, `--limit`.

Behavioral notes:
- `--ready` is a shortcut for status=open (excludes in_progress).
- Default (no `--status` and no `--all`) excludes closed issues.
- Label filtering auto-applies directory-scoped labels if no labels are provided (config `directory.labels`).
- `--limit 0` means unlimited; otherwise default is 50 (20 in agent mode).
- `--pinned` and `--no-pinned` are mutually exclusive.
- Templates are excluded by default; `--include-templates` shows them.
- Gates are excluded by default; `--include-gates` shows them (classic port likely omits gates).
- Default ordering (no `--sort`) is the DB order: `priority ASC, created_at DESC`.
- `--sort` applies client-side sorting; `--reverse` flips the order.
- `--tree` is an alias for `--pretty`; `--watch` implies `--pretty`.
- `--format` supports Go templates and graph formats (`digraph`, `dot`) for tooling.
- In daemon mode, list uses RPC and supports `--allow-stale` (skip freshness check).
- Formatting: `--long`, `--pretty|--tree`, `--format` (template/dot/digraph), `--no-pager`.
- Behavior: `--all` (include closed), `--ready` (open-only shortcut), `--limit` (0=unlimited, agent-mode default=20).

Behavioral notes:
- Default filter excludes **closed** unless `--all` or explicit `--status`.
- Directory-aware label scoping applies only if **no** labels were specified and config has `directory.labels`.
- Pretty/tree uses dependency graph if available; falls back to dotted ID hierarchy.
- In JSON mode, `IssueWithCounts` is returned (dep counts embedded).

#### search

- Requires a query (positional or `--query`).
- Filter flags largely mirror list (status, assignee, type, labels, priority ranges, date ranges, limit, sort).
- Uses the **same SearchIssues** backend as list (LIKE-based search). Results are sorted after retrieval.

#### show

- Accepts multiple IDs; supports `--short`, `--thread`, `--refs`, `--children`.
- JSON output returns **IssueDetails** (issue + labels + deps + dependents + comments + parent).
- Text output includes header + metadata + markdown-rendered description.

#### ready / blocked

- `ready` filters: `--assignee`, `--unassigned`, `--priority`, `--label`, `--label-any`, `--type`, `--parent`, `--mol-type`, `--limit`, `--sort` (hybrid|priority|oldest), `--include-deferred`.
- `ready --gated` and molecule-specific flows are out of scope for `br` classic port.
- `blocked` is a direct read of blocked cache; supports `--parent`.

### 15.16 Config and Environment Precedence (Classic)

Configuration sources are merged in this order (highest wins):

1) **Flags**
2) **Environment variables** (`BD_*`, and select `BEADS_*`)
3) **Project `.beads/config.yaml`**
4) **User config** (`~/.config/bd/config.yaml`)
5) **Legacy user config** (`~/.beads/config.yaml`)
6) **Defaults**

Key behaviors:
- “Startup” keys (e.g., `no-db`, `no-daemon`, `json`, `lock-timeout`, `sync.branch`, `routing.*`, `directory.labels`, `external_projects`, `validation.*`) are **YAML-only** and cannot be stored in SQLite.
- `bd config set/get` will write to YAML for yaml-only keys and to the DB for others.
- `config list` warns when YAML/env overrides DB values (notably `sync.branch`).

### 15.17 Routing and Multi-Repo Resolution (Classic)

Routing is split into **creation routing** and **ID routing**:

**Creation routing** (where new issues go):
- `routing.mode` (`auto` or `explicit`) and `routing.default/maintainer/contributor` are read from config.yaml.
- In `auto` mode, role detection uses git config and remote write URLs to infer maintainer vs contributor.
- `--repo` (explicit) or `--rig`/`--prefix` overrides routing decisions.

**ID routing** (where to read/update existing issues):
- `routes.jsonl` defines `prefix -> path` mappings.
- Resolution searches local `.beads/routes.jsonl` first, then walks up to a **town root** (identified by `mayor/town.json`) and uses `<townRoot>/.beads/routes.jsonl`.
- A per-rig `redirect` file inside `.beads/` can override the target path.
- Routing is used by `show`, `close`, `update`, and other commands that accept IDs.

**External ref derivation:** if an ID prefix matches a route, it can be converted to `external:<project>:<id>` for cross-project dependency resolution.

### 15.18 Sync, Auto-Import, and Auto-Flush (Classic Behavior)

Legacy `bd` sync + background behaviors combine three layers: **explicit sync**, **auto-import on reads**, and **auto-flush on writes**.

**Explicit sync modes (`bd sync`):**
- `--flush-only`: export pending changes to JSONL and exit (no git ops in this mode).
- `--import-only`: import JSONL into DB and exit (no git ops in this mode).
- `--dry-run`: print actions without touching the DB or JSONL.
- Sync forces **direct mode** (daemon bypass) to avoid stale daemon connections.

**Auto-flush (writes):**
- Mutating commands call `markDirtyAndScheduleFlush()` which **debounces** JSONL export.
- A final flush is attempted in `PersistentPostRun` unless explicitly skipped by sync.
- Flush uses atomic temp-file rename and updates metadata (`jsonl_content_hash`, `last_export_time`).

**Auto-import (reads):**
- Read-only commands call `ensureDatabaseFresh()` unless daemon mode.
- If JSONL is newer and `--no-auto-import` is **false**, it runs auto-import and continues.
- If JSONL is newer and `--no-auto-import` is **true**, the command aborts with guidance.
- `--allow-stale` skips the freshness check entirely (warns to stderr).

**Auto-import details:**
- **Staleness check** uses `last_import_time` vs JSONL **mtime** (via `Lstat`), not hash.
- **Auto-import trigger** uses content **hash comparison** (metadata `jsonl_content_hash`, fallback `last_import_hash`).
- Detects Git conflict markers and aborts with a conflict-resolution hint.
- Normalizes issues (default values; sets `closed_at` if missing for closed issues).
- Uses import options: `SkipPrefixValidation=true`, `Strict=false`.
- Clears export hashes before import.
- After import, schedules a **flush** (full export if ID remaps happened).

**Porting note:** `br` should preserve **flush-only** and **import-only** semantics, but **omit git operations**. Auto-flush/import behaviors are part of classic UX and should remain, gated by explicit flags.

### 15.19 Classic-Only Command Subset (Scope Table)

The Rust port targets the **classic** issue tracker (SQLite + JSONL) and intentionally omits Gastown/daemon/hook automation. This table defines the initial scope.

**Included in `br` v1 (classic parity):**
- Init & core CRUD: `init`, `create`, `update`, `close`, `reopen`, `delete` (tombstone)
- Views & queries: `list`, `show`, `ready`, `blocked`, `search`, `stats`, `count`, `stale`, `orphans`
- Structure: `dep`, `label`, `comments`
- Sync: `sync --flush-only`, `sync --import-only` (no git ops)
- Config: `config get/set/list/unset` (yaml-only vs DB-backed)

**Explicitly excluded in `br` v1:**
- Gastown features: `gate`, `agent`, `molecule`, `rig`, `convoy`, `hop`, `session`
- Daemon / RPC / auto-git hooks / auto-commit / auto-push
- Linear/Jira integrations
- TUI or visualization features (delegated to `bv`)

### 15.20 Core Flag Matrix (Classic Commands)

This matrix is a compact reference for core flags. It is not exhaustive, but it captures the flags that most directly affect semantics or output in classic mode.

| Command | Core Flags | Notes |
|---|---|---|
| `create` | `--title`, `--type`, `--priority`, `--assignee`, `--description/--body/--body-file`, `--design`, `--acceptance`, `--notes`, `--labels/--label`, `--parent`, `--deps`, `--external-ref`, `--id`, `--due`, `--defer`, `--silent`, `--dry-run` | `--file` creates multiple issues (no `--dry-run`); “test” titles warn; description can be required by config |
| `update` | `--status`, `--priority`, `--title`, `--assignee`, `--type`, `--estimate`, `--description/--body/--body-file`, `--design`, `--acceptance`, `--notes`, `--add-label`, `--remove-label`, `--set-labels`, `--parent`, `--due`, `--defer`, `--claim`, `--session` | Only changed flags apply; `--claim` is atomic |
| `close` | `--reason`, `--force`, `--continue`, `--no-auto`, `--suggest-next`, `--session` | Blocks if open blockers unless `--force` |
| `list` | `--status`, `--assignee`, `--type`, `--label`, `--label-any`, `--id`, `--priority`, `--priority-min/max`, `--title`, `--title-contains`, `--desc-contains`, `--notes-contains`, `--created/updated/closed-*`, `--defer/due-*`, `--overdue`, `--empty-description`, `--no-assignee`, `--no-labels`, `--all`, `--ready`, `--limit`, `--pretty/--tree`, `--long`, `--format`, `--no-pager` | Default excludes closed; `--limit 0` = unlimited; JSON returns IssueWithCounts |
| `search` | `--query`, `--status`, `--assignee`, `--type`, `--label`, `--label-any`, `--priority-min/max`, `--created/updated/closed-*`, `--limit`, `--sort`, `--reverse`, `--long` | Uses same backend as list; query is required |
| `show` | `--short`, `--thread`, `--refs`, `--children` | JSON returns IssueDetails |
| `ready` | `--assignee`, `--unassigned`, `--priority`, `--label`, `--label-any`, `--type`, `--parent`, `--mol-type`, `--limit`, `--sort`, `--include-deferred` | `--gated` and molecule routing are Gastown-only (exclude) |
| `blocked` | `--parent` | Reads blocked cache |
| `status`/`stats` | `--json`, `--assigned`, `--all`, `--no-activity` | JSON returns `StatusOutput` with summary + optional git activity |
| `count` | filters match `list` + `--by-status/priority/type/assignee/label` | Grouped JSON returns `{total, groups[]}` |
| `stale` | `--days`, `--status`, `--limit` | JSON returns array of Issue |
| `orphans` | `--details`, `--fix` | `--fix` is interactive; JSON output is list of orphanIssueOutput |

### 15.21 `br` Compatibility Checklist (Classic)

Use these assertions to validate parity with `bd` (classic, non-Gastown). Each item should map to a conformance test.

- **ID generation**: base36 adaptive length + nonce collision retries; hierarchical child IDs via `child_counters`.
- **Partial ID resolution**: exact → normalized → substring; ambiguous errors list candidates.
- **Search/List semantics**: tombstone exclusion default; label AND/OR; ordering by priority ASC then created_at DESC.
- **Ready semantics**: blocked cache usage, pinned/ephemeral excluded, `defer_until` respected.
- **Close semantics**: cannot close with open blockers unless forced; sets `closed_at`, `close_reason`, `closed_by_session`.
- **Import collision rules**: external_ref priority, tombstone protection, timestamp-aware protection.
- **JSONL format**: one issue per line (json.Encoder adds a newline per record); skip ephemeral; include tombstones.
- **Auto-import**: mtime-based staleness check + hash-based import trigger; conflict marker abort; skip when `--no-auto-import` set.
- **Auto-flush**: debounced, atomic write, final flush on exit unless sync says otherwise.
- **Config precedence**: flags > env > project config > user config > defaults; yaml-only keys honored.
- **Routing**: routes.jsonl + redirect behavior for cross-repo IDs; external refs derived from routes.

### 15.22 Deletion, Tombstones, and Reference Hygiene

Legacy deletion is **tombstone-first** with optional hard pruning:

- Default `delete` creates **tombstones** (status = `tombstone`, sets `deleted_at`, `deleted_by`, `delete_reason`).
- Tombstones are exported to JSONL to prevent resurrection after sync.
- `--hard` **only prunes tombstones from JSONL**; the DB tombstone remains until a separate cleanup step.

Deletion safety model:
- Without `--force`, `bd delete` shows a **preview** and exits.
- Preview lists dependency links that would be removed and connected issues whose text would be rewritten.
- `--cascade` recursively deletes dependents; `--force` can orphan dependents if not cascading.
- `--from-file` accepts one ID per line (skips empty/commented lines).

Reference hygiene:
- After deletion, connected issues are updated to replace plain-text references to deleted IDs with `[deleted:<id>]`.
- Replacement is applied to description/notes/design/acceptance_criteria using a boundary-aware regex that treats hyphenated IDs as whole tokens.

Porting note:
- For `br`, keep tombstone behavior and text reference rewriting; hard delete should be explicitly scoped and **not** resurrectable via sync.

### 15.23 Reopen and Comments (Classic UX)

**Reopen**:
- `reopen` explicitly sets status to `open` and clears `closed_at` (distinct from `update --status open`).
- If a `--reason` is provided, it is added as a **comment**.
- Supports multiple IDs, partial ID resolution, and JSON output (array of reopened issues).

**Comments**:
- `comments <issue-id>` lists comments; JSON output is an array.
- `comments add <issue-id> <text>` or `-f <file>` adds a comment.
- Author is from `--author` or falls back to actor (git-aware identity).
- There is a hidden alias `comment` for backward compatibility.

Porting note:
- Comments are part of the core sync surface (export/import) and should be preserved.

### 15.24 Stale, Count, and Orphans Views

**Stale**:
- Shows issues not updated in `N` days (default 30), optionally filtered by status.
- In JSON mode, returns a list of issues; in text mode, prints a ranked list with “days stale”.

**Count**:
- Counts issues matching list-like filters (status/type/labels/date ranges/priority ranges, etc.).
- Optional group-by: `status`, `priority`, `type`, `assignee`, or `label`.
- When grouped by label, each issue contributes to **each** label it has; unlabeled issues are grouped under `(no labels)`.

**Orphans**:
- Identifies **open or in_progress** issues referenced in git commits but not closed.
- Uses `git log --oneline --all` and matches IDs of the form `(prefix-xyz)`:
  - Prefix is read from DB config `issue_prefix` (fallback default `bd`).
  - Pattern supports hierarchical IDs (e.g., `bd-abc.1`).
- Only the **first (most recent)** commit per issue is recorded as `latest_commit`.
- Returns empty list (no error) if:
  - Not a git repo
  - No `.beads/` directory
  - No database present

Porting note:
- These are read-only views but rely on the same `IssueFilter` semantics as list/search.
- `orphans --fix` is interactive and intentionally **not** JSON-driven (keep it explicit).

### 15.25 Dependency Tree and Cycle Detection Details

**Dependency tree output shape**:
- `GetDependencyTree` returns a **flat list** of `TreeNode` records, not a nested tree.
- Each node includes `Depth` (root = 0) and `ParentID` to allow UI tree reconstruction.
- Rows are ordered by `depth`, then `priority`, then `id` for stable rendering.
- `maxDepth <= 0` defaults to **50**; nodes at `depth == maxDepth` are flagged `Truncated = true`.

**Traversal direction**:
- Normal mode (`reverse=false`) walks **dependencies** (what blocks this issue).
- Reverse mode (`reverse=true`) walks **dependents** (what depends on this issue).
- External refs are appended **only** in normal mode (dependencies); reverse mode shows local dependents only.

**Cycle safety and substring bug fix**:
- Recursive CTE tracks a `path` string with a `→` delimiter to avoid cycles.
- Path exclusion checks are **boundary-aware** (avoids false positives like `bd-10` containing `bd-1`).
- Result: the classic substring bug is explicitly covered by tests and blocked by path matching.

**Dedup vs full paths**:
- `showAllPaths=false` (default) deduplicates nodes by ID; the **shallowest occurrence wins**.
- `showAllPaths=true` keeps duplicates so diamond-shaped graphs show **all paths**.

**External dependencies in the tree**:
- External refs (`external:project:capability`) are **synthetic leaf nodes**.
- Each external node uses status from `CheckExternalDep`:
  - satisfied → `closed`, title prefixed with `✓`
  - unsatisfied → `blocked`, title prefixed with `⏳`
- External nodes are added at `parentDepth + 1` and skipped if parent is already at `maxDepth`.

**Cycle detection policy**:
- `AddDependency` prevents cycles via a recursive CTE with `maxDependencyDepth = 100`.
- All dependency types participate **except** `relates-to` (intentionally bidirectional).
- Cross-type cycles are blocked (e.g., `blocks` + `parent-child` + `discovered-from`).
- Self-dependencies are rejected; parent-child direction is validated (parent cannot depend on child).
- External refs do not participate in cycles (they cannot be traversed).
- `DetectCycles` exists for reporting: DFS over non-`relates-to` edges, with cycle normalization.

Porting note:
- Reproduce **flat list semantics**, depth/defaults, dedup behavior, and external node synthesis.
- Preserve cycle-prevention semantics and the substring-safe path filtering.

### 15.26 JSONL Export Edge Cases and Integrity

**Safety guard (empty DB)**:
- Full export refuses to overwrite a **non-empty JSONL** if the database has **zero issues**.
- This prevents catastrophic overwrites from empty/corrupt DB states.

**Canonical export content**:
- Always includes tombstones; always excludes ephemerals/wisps.
- Always sorts by ID for deterministic output.
- Uses `json.Encoder` (one JSON object per line, newline after each record).

**Atomic write + permissions**:
- Export writes to a temp file in the same directory, then renames atomically.
- Single-repo export sets permissions to **0600** (rw-------).
- Auto-flush and multi-repo exports set permissions to **0644** (rw-r--r--), and skip chmod for symlinks.
- Multi-repo export writes **empty JSONL** for repos with zero issues to keep clones in sync.
- Relative repo paths resolve **from repo root**, not CWD (prevents `.beads/oss/.beads` mistakes).

**Deferred finalize (sync atomicity)**:
- `exportToJSONLDeferred` writes JSONL but **does not** update metadata.
- `finalizeExport` runs only after git commit, then:
  - clears `dirty_issues`,
  - updates `jsonl_content_hash` and `last_import_time`,
  - touches DB mtime to be ≥ JSONL mtime.
- Metadata failures are warnings only (export still succeeds).

**Auto-flush integrity check**:
- Stored `jsonl_file_hash` is compared to current JSONL content hash.
- If JSONL is missing or hash mismatches, **export_hashes are cleared** and `jsonl_file_hash` is reset.
- This forces a **full export** and prevents perpetual mismatch warnings.

**Export policies and manifests**:
- Error policy: `strict`, `best-effort`, `partial`, `required-core` (auto-export defaults to best-effort).
- Labels/comments are treated as **enrichments**; policy may allow exporting core data without them.
- `export.skip_encoding_errors=true` skips bad issues and records warnings.
- `export.write_manifest=true` writes `.manifest.json` next to the JSONL with warnings/partials/failed IDs.
- Manifest writes are atomic and set permissions to **0600**.

Porting note:
- Preserve the empty-DB safety check, atomic writes, permission modes, hash tracking, and
  the **integrity-driven full export** fallback when JSONL content diverges.

---

### 15.27 Adaptive Hash IDs, Prefix Heuristics, and Partial Resolution

**Adaptive base36 hash IDs (top-level only)**:
- Hash IDs use **base36** (0-9, a-z) with adaptive length **3-8** characters.
- Adaptive length is computed from **top-level issue count only** (children are excluded by checking for no dot suffix after the prefix).
- Collision probability is estimated with a birthday-paradox approximation; the first length whose probability is <= threshold is chosen.
- Config keys (DB-backed):  
  - `max_collision_prob` (default 0.25)  
  - `min_hash_length` (default 3)  
  - `max_hash_length` (default 8)
- If adaptive length lookup fails, fallback length is **6**.

**Hash ID generation**:
- Candidate ID = `prefix + "-" + base36(hash(title|description|creator|created_at_unix_nano|nonce))`.
- For each length from `baseLength..8`, try **nonce 0..9** and check for collisions in DB.
- Batch ID generation tracks **intra-batch collisions** using an in-memory set.

**Hierarchical IDs**:
- A hierarchical ID is `parentID.N` (numeric suffix after the **last** dot).  
  This is robust to prefixes that contain dots (e.g., `my.project-abc.1`).
- Child IDs are generated via a **child_counters** table with atomic `INSERT ... ON CONFLICT` increments.
- Explicit child IDs (via import or --id) **update the counter** so future auto IDs do not collide.
- Max depth enforced by `hierarchy.max-depth` (default **3**); if unset or <1, default is used.

**Prefix extraction and validation**:
- Prefix extraction prefers the **last hyphen** when the suffix is numeric or hash-like.  
  If the suffix looks word-like (4+ chars with no digits), it falls back to the **first hyphen**.
- Explicitly provided IDs are prefix-validated **unless** `skipPrefixValidation` is set (import/multi-repo).
- Allowed prefixes are respected via `allowed_prefixes` config; **multi-repo mode allows all prefixes**.

**Partial ID resolution algorithm**:
- Fast path: try **exact ID match** via `SearchIssues` with ID filter (not `GetIssue`).
- Normalize by adding configured `issue_prefix` plus hyphen if missing (prefix default = `bd`).
- If exact match fails, scan all issues and match **hash substrings** across prefixes:
  - Prefer exact hash match (ignoring prefix), then substring match.
  - Ambiguity yields an error listing candidate IDs.

---

### 15.28 Parent Resurrection and Orphan Handling

**Orphan handling modes** (`import.orphan_handling`):
- `strict`: fail import on missing parent
- `resurrect`: attempt resurrection (default in some batch ops)
- `skip`: skip orphaned children with warning
- `allow`: allow orphans without validation (default for legacy tolerance)

**Resurrection behavior (JSONL-based)**:
- If a parent ID is missing during child ID generation or import, the system scans `.beads/issues.jsonl`
  for the **last occurrence** of that ID and creates a **closed placeholder** issue:
  - Status = `closed`, priority = 4, updated/closed timestamps = now
  - Description is prefixed with a `[RESURRECTED]` note, and the original description is appended
  - Dependencies are copied **only if the target issues already exist**
- Resurrecting a deep child will **recursively ensure ancestor chain** exists.

---

### 15.29 Update, Close, and Tombstone Semantics (Storage Layer)

**Update constraints**:
- Updates are **whitelisted** by field name; unrecognized fields are rejected.
- Status updates **cannot set `tombstone`** directly (must use delete/tombstone flow).
- `closed_at` is auto-managed when status changes unless explicitly supplied:
  - `status -> closed` sets `closed_at = now`
  - `closed -> non-closed` clears `closed_at` and **clears `close_reason`**
- Content hash is recomputed when any content fields change:
  `title`, `description`, `design`, `acceptance_criteria`, `notes`, `status`, `priority`,
  `issue_type`, `assignee`, `external_ref`.
- Updates always:
  - Create an event (`updated`, `status_changed`, `closed`, `reopened`)
  - Mark the issue as **dirty**
  - Rebuild blocked cache if `status` changed

**Close semantics**:
- `CloseIssue` sets status to `closed`, writes `closed_at`, `close_reason`,
  and `closed_by_session` (if provided).
- Close reason is stored **both** in the issue row and as the event comment.
- Close **rebuilds blocked cache** and marks issue dirty.
- Legacy code also auto-closes **convoys** (Gastown) when all tracked issues close;
  this behavior should be **excluded** in `br`.

**Tombstone semantics**:
- Tombstone creation:
  - status = `tombstone`
  - `closed_at = NULL` (enforced by constraint)
  - `deleted_at/by/reason` set, `original_type` preserved
  - event recorded, dirty marked, blocked cache rebuilt
- Hard delete:
  - Removes dependencies (both directions), events, comments, dirty markers, then issue

---

### 15.30 Ready Work and Blocked Cache (Implementation Details)

**Blocked cache**:
- `blocked_issues_cache` is a **materialized set** of blocked issue IDs.
- Rebuilt on:
  - add/remove dependency types that affect readiness
  - any status change or close (blockers change)
- Full rebuild uses recursive CTE and propagates parent-child blocking **transitively** (depth <= 50).

**Blocking rules captured in cache**:
- `blocks`: blocked while blocker is open/in_progress/blocked/deferred/hooked
- `conditional-blocks`: blocked unless blocker is closed **with failure reasons**
  (substring match of: failed, rejected, wontfix, won't fix, canceled/cancelled,
  abandoned, blocked, error, timeout, aborted)
- `waits-for`: fanout gate on children of spawner; metadata gate defaults to `all-children`
  (blocked while **any** child is open), `any-children` (blocked until **any** child closes)
- `parent-child`: children inherit parent blockage

**Ready query defaults**:
- Status defaults to `open` + `in_progress`
- Excludes `pinned` and `ephemeral` (wisps)
- Excludes internal workflow types by default: `merge-request`, `gate`, `molecule`, `message`, `agent`
- Optional filters: assignee/unassigned, labels (AND/OR), parent subtree, mol_type, priority,
  deferred handling (`include_deferred`)
- Sort policies:
  - `hybrid` (default): priority for recent items, age for older
  - `priority`
  - `oldest`

**External dependencies**:
- External dep format: `external:<project>:<capability>`
- Considered satisfied when external project has a **closed issue** with label `provides:<capability>`.
- Resolution uses **batch DB opens** per project to avoid O(N) overhead.
- Ready/blocked views filter or annotate based on external dep satisfaction.

**IsBlocked / suggest-next**:
- `IsBlocked` checks cache membership, then returns **blocking issue IDs** for `blocks` only.
- `GetNewlyUnblockedByClose` returns open/in_progress issues that depended on the closed issue
  and are **no longer in blocked cache**.

---

### 15.31 Import Pipeline Deep Dive

**Pre-processing**:
- Canonicalizes **Linear** external refs to avoid slug duplicates.
- Recomputes **content hashes** for all incoming issues (JSONL hash is not trusted).
- Marks issues as **ephemeral** if ID contains `-wisp-`.
- In multi-repo mode, **skip prefix validation** by default.
- Clears `export_hashes` before import to avoid staleness.

**Prefix mismatch handling**:
- Valid prefixes = configured prefix + `allowed_prefixes`; multi-repo allows all.
- If all mismatches are **tombstones**, they are dropped as noise.
- `--rename-on-import` rewrites prefix and **updates all text references** in:
  title, description, design, acceptance_criteria, notes, comments, dependencies.

**Match precedence and update rules**:
1) **external_ref match** (highest priority)  
2) **content hash match**  
3) **ID match** (collision/update path)
- Tombstone in DB **always blocks** resurrection.
- For updates: only apply if `incoming.updated_at` is **newer** than local.
- Timestamp-aware protection map (`ProtectLocalExportIDs`) can veto updates.
- `pinned` is only updated when explicitly true in JSONL (omitempty behavior).
- `assignee` and `external_ref` set to `NULL` if incoming value is empty.

**Batch creation**:
- New issues are sorted by **hierarchy depth** and created depth-by-depth.
- Orphan skip mode pre-filters missing-parent children to avoid partial inserts.

**Dependencies, labels, comments**:
- Dependencies are inserted if missing; FK failures are skipped with warnings (unless strict).
- Labels are additive (missing labels added, existing untouched).
- Comments are deduped by `(author, trimmed text)`; timestamps are preserved on import.

---

### 15.32 Export Error Policies and Manifests

**Error policies**:
- `strict` (default for manual export): fail fast
- `best-effort` (default for auto-export): skip with warnings
- `partial`: retry then skip with manifest entries
- `required-core`: fail on core data, skip enrichments

**Export config keys**:
- `export.error_policy`
- `auto_export.error_policy`
- `export.retry_attempts` (default 3)
- `export.retry_backoff_ms` (default 100)
- `export.skip_encoding_errors` (default false)
- `export.write_manifest` (default false)

**Manifests**:
- Written as `.manifest.json` next to JSONL using atomic temp rename.
- Permissions set to **0600** (warn-only on failure).

**Multi-repo export details**:
- Groups by `source_repo` and writes **one JSONL per repo**.
- Empty JSONL is written for repos with zero issues to keep clones in sync.
- Paths are resolved relative to **repo root** (parent of `.beads/`), not CWD.
- Uses `Lstat` for mtime updates to avoid symlink target issues.

---

### 15.33 List/Search Defaults and Filter Wiring

**List defaults**:
- If `--status` is not specified and `--all` is not set, **closed issues are excluded**.
- `--ready` forces `status=open` (not in_progress/blocked/deferred).
- Templates are excluded unless `--include-templates`.
- Gate issues are excluded unless `--include-gates` or `--type gate`.
- Directory-aware label scoping applies when **no labels are provided**.
- `--limit 0` means **unlimited**; agent mode defaults to **20** when not specified.

**Search semantics**:
- `SearchIssues` uses **LIKE** matching on `title`, `description`, and `id`.
- Default ordering is `priority ASC, created_at DESC`.
- Tombstones are excluded unless `IncludeTombstones` is set.

---

### 15.34 Update Validation Rules (Field-Level)

**Field validation rules**:
- Title length: **1..500**
- Priority: **0..4**
- IssueType: must be built-in (custom types allowed via config at higher layers)
- Estimated minutes: **non-negative**
- Status: built-in or custom; **tombstone is blocked** in updates

**Allowed update fields (storage whitelist)**:
- Core: `status`, `priority`, `title`, `description`, `design`, `acceptance_criteria`,
  `notes`, `issue_type`, `assignee`, `estimated_minutes`, `external_ref`
- Close fields: `closed_at`, `close_reason`, `closed_by_session`
- Scheduling: `due_at`, `defer_until`
- Pinned/wisp: `pinned`, `wisp` (maps to `ephemeral`)
- Additional fields exist for Gastown/agent features (exclude in `br` classic port)

---

### 15.35 Auto-Import, Auto-Flush, and Staleness Detection

**JSONL path discovery**:
- Primary resolution uses `BEADS_JSONL` env var.
- Otherwise derive `.beads/issues.jsonl` from the **DB path**; in `--no-db` mode,
  walk up to find `.beads/` and use its JSONL.
- Auto-flush creates `.beads/` if missing (best-effort).

**Auto-import trigger (hash-based, git-safe)**:
- Reads entire JSONL and computes **SHA256 hash**.
- Compares against metadata `jsonl_content_hash` (fallback: legacy `last_import_hash`).
- If hash differs → parse JSONL and import.
- Hash comparison avoids mtime pitfalls after `git pull` or file touch.
- Uses a **2MB per-line scanner buffer** to tolerate large JSONL records.

**Auto-import safety**:
- Rejects JSONL containing **git conflict markers** (`<<<<<<<`, `=======`, `>>>>>>>`) on standalone lines.
- Applies `Issue.SetDefaults()` and fixes closed-at invariant when missing.
- Marks wisps as ephemeral if ID contains `-wisp-` (defense-in-depth).
- Clears `export_hashes` before import to avoid staleness.
- Auto-import is **lenient**: `SkipPrefixValidation=true`.
- If DB is “cold-start” with no `issue_prefix`, it **infers prefix** from JSONL
  (or repo dir name) and sets config before import.

**Staleness detection (read-time)**:
- `ensureDatabaseFresh` is called by **read commands** in **direct mode**.
- It checks `autoimport.CheckStaleness`:
  - Parses `last_import_time` (RFC3339Nano, fallback RFC3339).
  - Compares JSONL **mtime** (via `Lstat`, not Stat to avoid symlink false positives).
  - If metadata is missing/empty or JSONL is absent → **not stale** (first-run safe).
- If stale:
  - If auto-import enabled → runs auto-import and continues.
  - If `--no-auto-import` → error with explicit recovery steps.
- `--allow-stale` skips the check with a warning.
- **Daemon mode bypass**: call sites **skip** `ensureDatabaseFresh` when daemon client is active
  because the daemon auto-imports before serving queries.

**Direct vs daemon auto-import nuance (non-classic note):**
- Direct-mode `autoImportIfNewer` **does not** update `last_import_time` when the hash matches;
  it simply returns (so repeated reads may keep re-checking staleness silently).
- Daemon-side auto-import (`internal/autoimport.AutoImportIfNewer`) **does** update
  `last_import_time` on hash match to prevent repeated staleness warnings.

**Daemon freshness guard**:
- Long-lived daemon connections use a **FreshnessChecker** that detects DB file replacement
  (inode/mtime) and triggers a reconnection to avoid stale handles.

**Auto-flush (debounced JSONL export)**:
- All mutating commands call `markDirtyAndScheduleFlush()` or `markDirtyAndScheduleFullExport()`.
- Flush is coordinated by a **FlushManager**:
  - Single goroutine owns dirty state; no shared mutable state.
  - Debounce interval controlled by `flush-debounce` (env `BEADS_FLUSH_DEBOUNCE`).
  - `FlushNow()` bypasses debounce; `Shutdown()` performs a final flush.
- Flush is no-op if auto-flush is disabled (`--no-auto-flush`).

**Incremental vs full export**:
- Incremental: fetch dirty IDs, **merge** with existing JSONL map.
- Full: rebuild JSONL from scratch (used for ID remaps or integrity failures).
- Export always filters **wisps** and, in multi-repo mode, may filter by prefix
  for non-primary repos.
- After export:
  - Clears dirty IDs
  - Updates `jsonl_content_hash`, `jsonl_file_hash`, and `last_import_time`
    to prevent staleness loops.

**JSONL integrity validation**:
- Before flush, compare **stored JSONL file hash** with current file hash.
- On mismatch or missing JSONL:
  - Clear `export_hashes` and `jsonl_file_hash`
  - Force **full export** to restore consistency

---

### 15.36 Delete/Tombstone Workflow and Reference Rewriting

**Delete command flow (direct mode)**:
1) Preview unless `--force`  
2) Update connected issues’ text references: replace `ID` → `[deleted:ID]`  
3) Remove dependency links (both directions)  
4) Create tombstone (default) or hard delete if requested  
5) Mark dirty + auto-flush

**Single-issue delete specifics**:
- Uses `CreateTombstone(id, actor, reason)` where `actor` comes from git identity
  and `reason` is the `--reason` flag (default `"delete"`).
- Does **not** remove labels/events/comments; it only removes dependencies, adds a `deleted` event,
  and marks the issue dirty.

**Reference rewriting (edge-safe)**:
- Uses a boundary-aware regex: ID is replaced only when surrounded by **non-word**
  boundaries where “word” includes letters, digits, `_`, and `-`.
- Fields updated: `description`, `notes`, `design`, `acceptance_criteria`.
- The in-memory copies of connected issues are updated after replacement to avoid double-rewrite.

**Batch deletion**:
- CLI validates all IDs exist before deletion; missing IDs error out.
- Uses `DeleteIssues` (storage) for cascade/force semantics and returns:
  - `deleted_count`, `dependencies_removed`, `labels_removed`, `events_removed`
  - `orphaned_issues` (when forced without cascade)
- Storage-level batch delete **creates tombstones** and removes dependencies,
  but **does not delete labels or historical events** (counts are informational only).
- Tombstone actor/reason are hard-coded to `"batch delete"` (the `--reason` flag is ignored in batch path).
- Connected issues are pre-collected and updated after deletion with the same reference rewrite logic.

**Cascade vs force**:
- `--cascade`: recursively deletes all dependents.
- `--force`: deletes the selected issues and **orphans** dependents.
- Without either: deletion fails if dependents exist (preview explains).

**Hard delete semantics**:
- `--hard` **does not remove tombstones from DB**; it **prunes tombstones from JSONL**
  immediately (negative TTL), then advises `bd cleanup --hard` after sync.
- Tombstones remain in DB to prevent resurrection by subsequent imports.

**Tombstone pruning (JSONL-only)**:
- `pruneExpiredTombstones` reads JSONL, removes tombstones that exceeded TTL:
  - Default TTL = `DefaultTombstoneTTL` (30 days).
  - Custom TTL overrides; negative TTL means prune immediately.
- Writes JSONL atomically and reports pruned IDs/count.

**Delete via daemon (non-classic note)**:
- Daemon RPC delete path is **simplified** (cascade semantics are not fully implemented);
  it effectively forces deletion and omits some preview details.

---

### 15.37 Routing and Multi-Repo Resolution (routes.jsonl)

**Two routing planes**:
- **Creation routing** (where new issues go): controlled by `routing.mode`
  (`auto` vs `explicit`) and `routing.default/maintainer/contributor`.
  - Role detection: git config `beads.role` or push URL heuristics.
  - `--repo` overrides everything.
- **ID routing** (where existing issues live): driven by `routes.jsonl` + redirect.

**routes.jsonl format**:
- JSONL entries: `{ "prefix": "bd-", "path": "beads/mayor/rig" }`
- Comments and blank lines are ignored.
- Prefix includes the trailing hyphen.

**Lookup resolution**:
- For a given ID, extract prefix (substring before first `-`, plus hyphen).
- Search order:
  1) `routes.jsonl` in the **current** `.beads/`
  2) If not found, walk up to **town root** (identified by `mayor/town.json`) and use
     `<townRoot>/.beads/routes.jsonl`
- If a prefix match is found:
  - `path == "."` → town-level `.beads/`
  - Otherwise resolve relative to town root: `<townRoot>/<path>/.beads`
  - Apply **redirect** (see below)
  - Validate target directory exists before routing

**Redirect file**:
- `.beads/redirect` may contain a path to another beads directory.
- If present, routing follows the redirect (relative paths resolved from the current beads dir).
- Used for local overrides, migrations, or multi-worktree setups.

**Forgiving rig lookup** (`--rig` / `--prefix` UX):
- Accepts `rig`, `prefix` (`bd`), or `prefix-` (`bd-`).
- Uses routes + town root to resolve to target beads dir and prefix.

**External ref derivation**:
- If an ID prefix matches a route, it can be converted to
  `external:<project>:<id>` where `<project>` is the first path segment.

**Routed storage access**:
- When an ID is routed, the CLI opens a **separate SQLite connection** to the routed beads dir.
- Callers must close routed storage after use.

---

### 15.38 Initialization and Bootstrap (bd init)

**Non-invasive port note**:
- Legacy bd installs git hooks, merge drivers, and writes repo-specific git config in init flows.
- `br` must **not** auto-install or auto-modify git state. Any git integration must be **explicit**
  and opt-in (per project goals).

**Purpose**:
- Creates the local `.beads/` workspace and initializes storage.
- Seeds the database from JSONL in git history or local JSONL when requested.

**Backend and path selection**:
- `--backend` accepts `sqlite` (default) and `dolt` (legacy; not ported to `br`).
- Database path comes from `--db`, else `BEADS_DB`, else `.beads/beads.db`.
- If DB path is inside `.beads/`, init also creates `.beads/` and local files.

**Prefix resolution** (in order):
1) `--prefix` flag  
2) config `issue-prefix` (from config.yaml or env)  
3) first issue found in JSONL from git history  
4) current directory name  
- Trailing `-` is stripped (hyphen is added during ID generation).

**Safety guards**:
- Refuses to run inside a git worktree; must initialize in the main repo root.
- Refuses to run inside `.beads/` to avoid nested beads directories.
- Checks for an existing database; aborts unless `--force`.
- If JSONL exists but DB does not, init proceeds (fresh-clone recovery path).
- Migrates older `*.db` filenames to `beads.db` if exactly one DB exists.

**No-DB mode (`--no-db`)**:
- Creates `.beads/issues.jsonl` and `.beads/interactions.jsonl`.
- Writes `metadata.json` (configfile defaults) and `.beads/config.yaml`.
- Stores `issue-prefix` in config.yaml because there is no DB config table.

**SQLite mode (default)**:
- Creates/updates `.beads/.gitignore` from doctor template.
- Ensures `.beads/interactions.jsonl` exists.
- Creates storage backend and sets DB config `issue_prefix` (fatal if failure).
- Writes metadata keys: `bd_version`, `repo_id`, `clone_id` (best-effort).
- Writes/updates `.beads/metadata.json` and `.beads/config.yaml` templates.

**Sync-branch setup**:
- `--branch` sets sync branch (writes both config.yaml and DB).
- No auto-detection of current branch to avoid main/master worktree conflicts.

**Import on init**:
- Default: scan git history for `.beads/issues.jsonl`, import if found.
- `--from-jsonl`: import from local `.beads/issues.jsonl` (preserves manual cleanups).
- Auto-import failure is non-fatal; init continues with empty DB.

**Optional wizards and git setup**:
- `--contributor` / `--team` run guided setup wizards.
- `--setup-exclude` configures `.git/info/exclude` for forks.
- Installs git hooks and merge driver unless `--skip-hooks` / `--skip-merge-driver`.
- `--stealth` mode configures per-repo git excludes and suppresses hooks/driver.

**Post-init diagnostics**:
- Adds "landing the plane" instructions to `AGENTS.md` and `@AGENTS.md` (unless stealth).
- Runs `bd doctor` checks; warns if setup is incomplete.

---

### 15.39 Sync Semantics (bd sync, classic SQLite + JSONL)

**Non-invasive port note**:
- Legacy bd’s sync assumes automatic git commit/push behavior in many flows.
- `br` must be **less invasive**: no automatic hooks, no background daemon actions,
  no implicit commit/push. Any git ops must be explicit CLI commands.

**Direct-mode enforcement**:
- If daemon is connected, `bd sync` forces direct mode to avoid stale DB handles.
- Store initialization is re-run after daemon disconnect.

**Modes and early exits**:
- `--import-only`: import JSONL only (no git ops; inline import to handle redirects).
- `--flush-only`: export JSONL only.
- `--squash`: export JSONL only, skipping commit/push (accumulate changes).
- `--status`: show sync-branch vs main diff.
- `--merge`: merge sync-branch back to main.
- `--check`: integrity checks (force-push, prefix mismatch, orphaned children).
- `--from-main`: one-way sync from default branch for ephemeral branches.

**Redirect handling**:
- If `.beads/redirect` is active, sync skips all git operations and only exports.
- Redirect and sync-branch are mutually exclusive; sync warns and proceeds export-only.

**Preflight safety**:
- Requires a git repo (unless using export-only modes).
- Aborts if merge/rebase in progress.
- Detects uncommitted JSONL changes; re-exports from DB to reconcile.
- If no upstream and no sync-branch, auto-falls back to `--from-main`.

**Pull-first sync (default)**:
1) Load local issues from DB (includes tombstones).
2) Acquire `.beads/.sync.lock` (exclusive).
3) Load base state from `.beads/sync_base.jsonl` (may be missing on first sync).
4) Pull remote JSONL (sync-branch worktree if configured, else normal pull).
5) Load remote JSONL state.
6) 3-way merge: base + local + remote (LWW + union/append rules).
7) Write merged JSONL, import merged state into DB.
8) Export DB back to JSONL (DB is source of truth).
9) Commit/push changes (sync-branch worktree or current branch).
10) Save new base state and clear sync state.

**No-pull sync (`--no-pull`)**:
- Export -> commit -> push, under the same lock.
- Runs pre-export validation, duplicate ID checks, and orphaned dep warnings.

**From-main mode**:
- Uses `sync.remote` (default `origin`) and detects default branch.
- `git fetch <remote> <main>` then checks out `.beads/` from remote main.
- Imports JSONL into DB; forces `no-git-history` to avoid false deletions.

**Key flags**:
- `--rename-on-import`: rewrite IDs to local prefix and update references.
- `--no-git-history`: skip git history backfill for deletions (useful for JSONL renames).
- `--accept-rebase`: accept remote sync-branch history resets (integrity flow).

**Integrity check output**:
- Reports force-push risk, prefix mismatches, and orphaned children.
- Exits non-zero if any problems are detected.

---

### 15.40 Configuration System (config.yaml + DB config)

**Non-invasive port note**:
- `br` should avoid hidden or implicit config that triggers git ops or background behavior.
- Keep config explicit; prefer on-demand commands over daemon-autostart defaults.

**Config sources and precedence**:
- Load order: project `.beads/config.yaml` (walk up from CWD), then
  `~/.config/bd/config.yaml`, then `~/.beads/config.yaml`.
- Environment variables override config file (prefix `BD_`, dot/hyphen -> underscore).
- Selected legacy env vars are also bound: `BEADS_FLUSH_DEBOUNCE`,
  `BEADS_AUTO_START_DAEMON`, `BEADS_IDENTITY`, `BEADS_REMOTE_SYNC_INTERVAL`.
- Flags override config values at command execution time.

**Two storage planes**:
- **YAML-only**: startup settings read before DB exists (must live in config.yaml).
- **DB config**: runtime settings stored in `config` table (versioned with DB).

**YAML-only keys (representative)**:
- Bootstrap: `no-db`, `no-daemon`, `no-auto-flush`, `no-auto-import`, `json`,
  `auto-start-daemon`
- Identity/paths: `db`, `actor`, `identity`
- Timing: `flush-debounce`, `lock-timeout`, `remote-sync-interval`
- Git: `git.author`, `git.no-gpg-sign`, `no-push`, `no-git-ops`
- Sync: `sync-branch`, `sync.branch` (alias), `sync.require_confirmation_on_mass_delete`
- Routing: `routing.*`
- Create/validation: `create.require-description`, `validation.*`
- Hierarchy: `hierarchy.max-depth`
- Daemon: `daemon.*`
- Any key with prefixes `routing.`, `sync.`, `git.`, `directory.`, `repos.`,
  `external_projects.`, `validation.`, `daemon.`, `hierarchy.` is YAML-only.

**Config set/get/list/unset behavior**:
- `bd config set <key> <value>`:
  - YAML-only keys update config.yaml (uncomment or append), preserving file.
  - DB keys require direct mode; `sync.branch` uses sync-branch helper.
- `bd config get <key>`:
  - YAML-only keys read from config.yaml (empty => "not set in config.yaml").
  - DB keys require direct mode; `sync.branch` resolves env/yaml/db precedence.
- `bd config list`:
  - Lists DB config only, sorted.
  - Warns when config.yaml or env overrides DB values (notably sync.branch).
- `bd config unset <key>`:
  - Deletes DB config entry only (does not edit config.yaml).

**YAML editing rules**:
- Values are formatted: booleans lowercased, numbers/durations unquoted,
  other strings quoted.
- `sync.branch` is normalized to `sync-branch` in YAML.
- `hierarchy.max-depth` is validated to be an integer >= 1.

**Sync-branch precedence**:
1) `BEADS_SYNC_BRANCH` env var  
2) `sync-branch` in config.yaml  
3) `sync.branch` in DB  
- `sync.branch` cannot be `main` or `master` (worktree conflict prevention).

---

### 15.41 Output Formats and JSON Schemas (Core Commands)

**Global JSON conventions**:
- All JSON uses stable field names from `types.*` structs (snake_case).
- Arrays are always emitted as `[]` (never `null`) in JSON mode.
- Time fields serialize as RFC3339 strings (Go `time.Time` JSON).
- `omitempty` fields are omitted when empty.
- JSON is pretty-printed with 2-space indent in core CLI (`outputJSON`).
  - `comments` uses `json.MarshalIndent` directly but matches the same indent.
- JSON errors (when `--json` is active) are emitted to stderr as:
  `{ "error": "...", "code": "..." }` (code may be omitted).

#### 15.41.1 list

**JSON shape**: array of `IssueWithCounts`
```json
[
  {
    "id": "bd-abc123",
    "title": "Example",
    "status": "open",
    "priority": 2,
    "issue_type": "task",
    "labels": ["backend"],
    "created_at": "2025-01-01T00:00:00Z",
    "updated_at": "2025-01-02T00:00:00Z",
    "dependency_count": 1,
    "dependent_count": 3
  }
]
```
**Schema notes**:
- `IssueWithCounts` = all Issue JSON fields (see §3.1, classic subset) **plus**
  `dependency_count` and `dependent_count` (ints).
- `ContentHash`, routing fields, and other `json:"-"` fields are never emitted.
**Text formats**:
- **Compact (default)**: one line per issue  
  `STATUS_ICON [pin] ID [P#] [type] @assignee [labels] - Title`
  - Closed issues are fully muted.
  - Priority uses `● Pn` (colored for P0/P1).
- **Long (`--long`)**: multi-line per issue:  
  `ID [P#] [type] status` then title, optional assignee/labels.
- **Pretty/Tree (`--pretty` or `--tree`)**:
  - Tree connectors `├──`, `└──` with priority-sorted children.
  - Status icons: `○` open, `◐` in_progress, `●` blocked, `✓` closed, `❄` deferred.
  - Root list ends with summary and status key.
- **Agent mode**: `ID: Title` only (no colors, no emojis).
- **Format (`--format`)**:
  - `dot` → Graphviz DOT graph with colored nodes and dependency edges.
  - `digraph` → edge list (issue_id depends_on_id) for `digraph` tool.
  - Any other string is treated as a Go template executed per dependency edge.
- **Pager**: non-agent outputs are piped through a pager unless `--no-pager`.

#### 15.41.2 show

**JSON shape**: array of `IssueDetails` (even for a single ID)
```json
[
  {
    "id": "bd-abc123",
    "title": "Example",
    "status": "in_progress",
    "priority": 1,
    "issue_type": "feature",
    "labels": ["ui", "frontend"],
    "dependencies": [
      { "id": "bd-xyz", "title": "Blocker", "dependency_type": "blocks" }
    ],
    "dependents": [
      { "id": "bd-foo", "title": "Child", "dependency_type": "parent-child" }
    ],
    "comments": [
      { "id": 3, "issue_id": "bd-abc123", "author": "alice", "text": "...", "created_at": "..." }
    ],
    "parent": "bd-xyz"
  }
]
```
**Schema notes**:
- `IssueDetails` = all Issue JSON fields **plus**:
  - `labels`: array of strings (may be empty)
  - `dependencies`: array of `IssueWithDependencyMetadata`
  - `dependents`: array of `IssueWithDependencyMetadata`
  - `comments`: array of `Comment`
  - `parent`: computed from `parent-child` dependency (may be omitted)
- `IssueWithDependencyMetadata` = Issue fields + `dependency_type`.
**Compatibility**:
- Canonical output is an **array**; legacy tests accept a single object as a fallback.
**Text formats**:
- **Short (`--short`)**: same line format as pretty list (`STATUS_ICON ID ●P# [type] Title`).
- **Full**:
  - Tufte header: `STATUS_ICON ID · Title   [● P# · STATUS]`
  - Metadata lines: owner/assignee/type and created/updated (plus due/defer if set).
  - Sections (if present): DESCRIPTION / DESIGN / NOTES / ACCEPTANCE CRITERIA (markdown rendered).
  - Labels, dependencies, dependents grouped by type, and comments with date+author.

#### 15.41.3 ready

**JSON shape**: array of `Issue`
```json
[
  { "id": "bd-abc", "title": "Ready item", "status": "open", "priority": 2, "issue_type": "task" }
]
```
**Text**:
- Default: numbered list with `[P#] [type] ID: Title`, plus estimate/assignee if present.
- `--pretty` uses the same tree output as `list --pretty`.
- If none: prints either “No ready work found…” or “No open issues”.

#### 15.41.4 blocked

**JSON shape**: array of `BlockedIssue`
```json
[
  {
    "id": "bd-abc",
    "title": "Blocked item",
    "status": "blocked",
    "priority": 1,
    "blocked_by_count": 2,
    "blocked_by": ["bd-1", "bd-2"]
  }
]
```
**Text**:
- `[P#] ID: Title` then `Blocked by N open dependencies: [ids]`.

#### 15.41.5 search

**JSON shape**: array of `IssueWithCounts` (same as list).

**Text**:
- Header: `Found N issues matching 'query'`
- **Compact (default)**: `ID [P#] [type] status @assignee [labels] - Title`
- **Long (`--long`)**: multi-line with assignee + labels.
- **Sorting**: optional `--sort` and `--reverse` are applied after search results load.

#### 15.41.6 status (alias: stats)

**JSON shape**: `StatusOutput`
```json
{
  "summary": {
    "total_issues": 42,
    "open_issues": 10,
    "in_progress_issues": 5,
    "closed_issues": 20,
    "blocked_issues": 2,
    "deferred_issues": 1,
    "ready_issues": 7,
    "tombstone_issues": 3,
    "pinned_issues": 1,
    "epics_eligible_for_closure": 1,
    "average_lead_time_hours": 12.5
  },
  "recent_activity": {
    "hours_tracked": 24,
    "commit_count": 3,
    "issues_created": 2,
    "issues_closed": 1,
    "issues_updated": 4,
    "issues_reopened": 0,
    "total_changes": 7
  }
}
```
**Text**:
- “Issue Database Status” header with summary and optional extended section.
- Optional “Recent Activity (last 24 hours)” block.
- `bd stats` is a direct alias of `bd status`.

**Recent activity calculation (legacy)**:
- Uses `git log --since=<N hours> --numstat --pretty=format:%H .beads/issues.jsonl`
  to count commits (every hash line).
- Uses `git log --since=<N hours> -p .beads/issues.jsonl` to parse added JSON lines.
- Counts `issues_created` if `created_at` is within the window.
- Counts `issues_closed` if status is closed and `closed_at` within window.
- Everything else is treated as `issues_updated` (no robust reopen detection).
- `issues_reopened` remains 0 (not detected reliably in legacy).

#### 15.41.7 count

**JSON (no grouping)**:
```json
{ "count": 17 }
```
**JSON (grouped)**:
```json
{
  "total": 17,
  "groups": [
    { "group": "open", "count": 5 },
    { "group": "closed", "count": 12 }
  ]
}
```
**Ordering note**:
- Direct mode sorts `groups` by the `group` string before JSON output.
- Daemon mode preserves map iteration order (not stable); treat JSON group order as unspecified.
**Text**:
- No grouping: prints a single number.
- Grouped: `Total: N` then `group: count` sorted by group key.

#### 15.41.8 stale

**JSON shape**: array of `Issue`
```json
[ { "id": "bd-abc", "title": "Stale", "updated_at": "..." } ]
```
**Text**:
- “Stale issues (N not updated in D+ days):” with numbered list.
- Each item prints status, days stale, and optional assignee.

#### 15.41.9 dep

**dep add / dep --blocks JSON**:
```json
{ "status": "added", "issue_id": "bd-a", "depends_on_id": "bd-b", "type": "blocks" }
```
or (for `--blocks`):
```json
{ "status": "added", "blocker_id": "bd-b", "blocked_id": "bd-a", "type": "blocks" }
```
**dep remove JSON**:
```json
{ "status": "removed", "issue_id": "bd-a", "depends_on_id": "bd-b" }
```
**dep list JSON**: array of `IssueWithDependencyMetadata`  
(all Issue fields + `dependency_type`).

**dep tree JSON**: array of `TreeNode`
```json
[
  { "id": "bd-root", "depth": 0, "parent_id": "", "truncated": false, "title": "Root" },
  { "id": "bd-child", "depth": 1, "parent_id": "bd-root", "truncated": false, "title": "Child" }
]
```
**dep cycles JSON**: array of cycles, each a list of Issue objects.

**Text**:
- `dep list`: “ID: Title [P#] (status) via <dependency_type>”
- `dep tree`: tree connectors with status emoji and optional mermaid output.
- `dep cycles`: numbered cycles with issue IDs + titles.

#### 15.41.10 label

**label add/remove JSON**: array of results
```json
[ { "status": "added", "issue_id": "bd-1", "label": "backend" } ]
```
**label list JSON**: array of strings
```json
["backend", "ui"]
```
**label list-all JSON**: array of `{label,count}` objects.
**Error/output notes**:
- Batch add/remove emits JSON **only** for successful mutations; failures print to stderr.
- `label list-all` outputs `[]` if no labels exist.

**Text**:
- Add/remove prints per-issue confirmation.
- List prints bullet list; list-all prints aligned label table with counts.

#### 15.41.11 comments

**comments list JSON**: array of `Comment`
```json
[ { "id": 1, "issue_id": "bd-1", "author": "alice", "text": "...", "created_at": "..." } ]
```
**comments add JSON**: a single `Comment` object.

**Text**:
- List: “Comments on <id>” then `[author] at YYYY-MM-DD HH:MM` followed by
  markdown-rendered comment text indented.
- Add: “Comment added to <id>”.

#### 15.41.12 orphans

**JSON shape**: array of `orphanIssueOutput`
```json
[
  {
    "issue_id": "bd-abc",
    "title": "Fix edge-case",
    "status": "in_progress",
    "latest_commit": "deadbeef",
    "latest_commit_message": "Fix edge-case for parser"
  }
]
```
**Text**:
- Header: `Found N orphaned issue(s)` then numbered list.
- Each item prints `ID: Title` and `Status`.
- With `--details`, includes `Latest commit: <sha> - <message>`.
**Fix mode**:
- `--fix` prompts for confirmation, then runs `bd close <id> --reason Implemented` per issue.
- Interactive by design (no JSON batch output for fix path).

---

### 15.42 List/Search Filtering Semantics (Classic Port Requirements)

**Limit defaults**:
- `list`: default 50; if `--limit` is explicitly set to `0`, means unlimited.
- `list` in agent mode: default 20 (only when `--limit` not specified).
- `search`: default 50; no agent-mode override.
- `ready`: default 10.
- `stale`: default 50.

**Default status filtering**:
- `list`: if `--status` not set and `--all` not set, **exclude closed**.
- `list --ready`: forces `status=open` only (no in_progress/blocked/deferred).
- `search`: no implicit exclusion; includes closed unless `--status` is specified.
- `search` query matches title/description/id (OR semantics).

**Label filter semantics**:
- `--label` = AND (issue must have all labels).
- `--label-any` = OR (issue must have at least one).
- If **no labels specified**, `list` and `ready` apply `directory.labels`
  from config for monorepo scoping; `search` does **not**.

**Ordering defaults**:
- Storage query default ordering is `priority ASC, created_at DESC`.
- `--sort` overrides with in-memory ordering:
  - `priority` ascending (P0 before P1).
  - `created`, `updated`, `closed` are descending (newest first).
  - `status`, `id`, `title`, `type`, `assignee` are ascending.
  - `--reverse` flips the chosen sort.

**Pretty/tree fallbacks**:
- `--tree` is an alias for `--pretty`.
- `--watch` implies `--pretty`.
- Tree building uses parent-child dependencies; if not available, falls back
  to dotted hierarchical IDs (`parent.1`, `parent.1.2`).

**Core JSON shapes (for spec)**:
- `IssueWithCounts`: flattened Issue fields + `dependency_count`, `dependent_count`.
- `IssueDetails`: flattened Issue fields + `labels`, `dependencies`, `dependents`,
  `comments`, `parent` (computed from parent-child deps).

---

### 15.43 Dependency Tree Output (bd dep tree)

**Tree node schema** (`TreeNode`):
- All `Issue` fields (ID, title, status, priority, etc.).
- `depth` (int): edges from root (root = 0).
- `parent_id` (string): immediate parent in the traversal.
  - Root node uses `parent_id = id`.
- `truncated` (bool): true if `depth == maxDepth`.

**Traversal direction**:
- **Down (default)**: dependencies (“what blocks this issue”).
  - Parent → child edges follow: **issue depends on dependency**.
- **Up (`--direction=up`)**: dependents (“what this issue blocks”).
  - Parent → child edges follow: **dependent depends on parent**.
- **Both**: current implementation concatenates “up” nodes and “down” nodes;
  visual separation is minimal (legacy behavior).

**Depth ordering & deterministic output**:
- Recursive CTE yields rows ordered by `depth ASC, priority ASC, id ASC`.
- This ordering drives the initial `TreeNode` list and the child insertion order
  in the renderer (stable, deterministic).

**Parent pointer logic**:
- Each recursion step sets `parent_id = t.id` (the node from which we traversed).
- For **down**: child is the dependency target.
- For **up**: child is the dependent.
- The renderer builds a `parent_id -> children[]` map to render connectors.

**Truncation rules**:
- `maxDepth <= 0` defaults to **50**.
- Nodes at `depth == maxDepth` are flagged `truncated=true`.
- Renderer appends an ellipsis (`…`) when:
  - `node.truncated` is true, **or**
  - `depth == maxDepth` and the node has children (indicates hidden depth).

**Cycle protection**:
- Recursive query maintains a path string with `→` separators and excludes
  revisits of any ID already on the path.

**Deduplication vs “show all paths”**:
- `--show-all-paths=false` (default):
  - Storage **dedupes by ID**, keeping only the shallowest occurrence
    (first row due to depth ordering).
- `--show-all-paths=true`:
  - Storage emits **all path occurrences** (diamonds preserved).
  - Renderer still prints a short “(shown above)” line for already-seen IDs,
    to avoid infinite repeats in display.

**External leaf node synthesis**:
- Only in **down** direction (dependencies).
- External dependencies are stored as `external:<project>:<capability>`.
- Synthetic `TreeNode` is created per external ref:
  - `id` = full external ref
  - `parent_id` = issue that depends on the external ref
  - `depth` = parent depth + 1
  - `status` = `closed` if satisfied, otherwise `blocked`
  - `title` = `"✓ <capability>"` (satisfied) or `"⏳ <capability>"` (unsatisfied)
  - `priority` = 0, `issue_type` = task
- In text rendering, external nodes print as:  
  `✓ capability (external)` or `⏳ capability (external)` (with status color).

**Text rendering details**:
- Uses tree connectors (`├──`, `└──`) and vertical bars for siblings.
- Each line is: `ID: Title [P#] (status)` plus `[READY]` on root if open.
- External refs are printed from `title` only (ID hidden) and suffixed `(external)`.

---

### 15.44 Comments Command Behavior (bd comments)

**List (`bd comments <id>`)**:
- Resolves partial IDs to full IDs (daemon: RPC resolve; direct: local resolve).
- Loads comments ordered by `created_at ASC`.
- JSON output: array of `Comment` objects (pretty printed):
```json
[
  { "id": 1, "issue_id": "bd-1", "author": "alice", "text": "...", "created_at": "..." }
]
```
- Text output:
  - Header `Comments on <id>`
  - For each comment: `[author] at YYYY-MM-DD HH:MM`
  - Comment body is markdown-rendered and indented.

**Add (`bd comments add <id> <text>` or `-f <file>`)**:
- Resolves partial ID (same as list).
- Author resolution (if `--author` not set):
  - `--actor` → `BD_ACTOR` → `BEADS_ACTOR` → `git config user.name` → `$USER` → `"unknown"`.
- Inserts a new row with `created_at = CURRENT_TIMESTAMP`.
- **No deduplication**: repeated identical text is allowed.
- JSON output: a single `Comment` object (pretty printed).
- Empty text is allowed (explicitly tested).

**Dedupe rules (import only)**:
- Comment dedupe happens **during JSONL import**, not during CLI add.
- Import path dedupes by `(author, trimmed text)` to avoid duplicate comments.

---

### 15.45 dep + label Command UX (Daemon vs Direct)

#### dep (dependencies)

**Add (dep add / dep --blocks)**:
- Accepts positional or flag syntax:
  - `bd dep add <issue> <depends-on>`
  - `bd dep <blocker> --blocks <blocked>`
- Partial IDs are resolved before mutation:
  - **Daemon**: RPC `ResolveID`.
  - **Direct**: local resolver.
- `external:<project>:<capability>` is allowed as the dependency target:
  - Validated to `external:<project>:<capability>` format.
  - Stored **as-is** (no ID resolution).
- **Anti-pattern guard**: child → parent dependency is rejected to prevent deadlocks.
- **Cycle warning**: after adding, a cycle scan runs and emits a warning.

**Remove (dep remove)**:
- Requires two IDs (issue + depends-on), resolves partial IDs first.
- **Daemon**: RPC `RemoveDependency`.
- **Direct**: `RemoveDependency` + mark dirty for auto-flush.

**List (dep list)**:
- Default direction = down (“depends on”); `--direction=up` shows dependents.
- Optional `--type` filters by dependency type.
- **Daemon**: list uses local DB if RPC unsupported (fallback to direct mode).

**Tree (dep tree)**:
- Uses `GetDependencyTree` (see §15.43).
- `--direction` overrides deprecated `--reverse`.
- `--format=mermaid` outputs flowchart TD.
- **Daemon**: if RPC unsupported, falls back to direct store.

**Error handling differences**:
- **Daemon mode** returns RPC errors verbatim; unknown ops trigger direct fallback.
- **Direct mode** returns storage errors; CLI wraps with `FatalErrorRespectJSON`
  to emit JSON error objects when `--json` is active.

#### label

**Add / remove**:
- Batch operations: `label add <issue...> <label>`, `label remove <issue...> <label>`.
- Partial IDs are resolved first (daemon or direct).
- `provides:*` labels are **reserved**; only `bd ship` may add them.
- **Daemon**: RPC `AddLabel` / `RemoveLabel`.
- **Direct**: `AddLabel` / `RemoveLabel` + mark dirty for auto-flush.

**List**:
- `label list <issue>` returns labels for a single issue.
- `label list-all` aggregates unique labels with counts.

**Error handling differences**:
- In daemon mode, label list relies on RPC `Show` and reads labels from response.
- Direct mode queries label tables and returns missing labels as empty list.
- JSON output is always an array (labels or `{label,count}` objects).

---

### 15.46 Doctor / Integrity Checks (Classic Subset)

**Port decision (v1, non-invasive)**:
- Keep **read-only diagnostics** that help detect corruption or drift.
- **Do NOT** port any auto-fix behaviors that delete, rename, or modify git state.
- Suggested recovery steps may be printed, but must be **explicit user actions**.

**JSONL integrity**:
- Locates JSONL via `metadata.json` (configfile JSONLExport) or best-effort discovery.
- Scans line-by-line; malformed JSON or missing `id` is counted and reported.
- If malformed and DB exists: warning/error with guidance to regenerate JSONL.
- If malformed and DB missing: error with guidance to restore JSONL from git/backups.

**Schema compatibility**:
- Opens DB read-only and probes critical tables/columns:
  - `issues`, `dependencies`, `child_counters`, `export_hashes`.
- Missing tables/columns → error with migration guidance.

**Database integrity (SQLite PRAGMA)**:
- Runs `PRAGMA integrity_check` on DB.
- “ok” → pass; any other result → corruption warning.
- If JSONL is present, suggests recovery via import/export (no auto-fix).

**DB ↔ JSONL sync check**:
- Counts issues in DB and JSONL and compares.
- Detects prefix mismatch (majority-prefix rule; allows `-mol/-wisp/-eph` variants).
- Detects status mismatches when counts match (DB vs JSONL).
- Uses file mtime as a secondary stale signal (warn if JSONL much newer).

**Orphaned dependencies** (classic validation):
- SQL join to detect dependencies whose `depends_on_id` does not exist in `issues`.
- Report count + sample list.
- **No auto-removal in v1**; recommend manual cleanup or explicit repair command.

**Merge artifacts**:
- Scans `.beads/` for merge temp files (`*.base.jsonl`, `*.left.jsonl`, etc.).
- Reports and suggests manual cleanup.

**Excluded in v1**:
- `doctor --fix` automation (JSONL regeneration, pruning, dependency cleanup).
- Git operations (hook fixes, sync-branch resets, branch manipulations).
- Any destructive recovery that renames or deletes files.

---

### 15.47 Create / Update / Close / Reopen (Mutation Semantics)

This section documents the CLI-visible semantics for classic issue mutation commands.
Gastown-only flags and types are noted but excluded from the Rust port (see Port Notes).

#### 15.47.1 create

**Invocation**:
- `bd create <title>` or `bd create --title <title>` (positional and flag must match if both present).
- `bd create --file <markdown>` creates multiple issues parsed from a Markdown file.
  - `--dry-run` is not supported with `--file`.

**Defaults & validation**:
- Default `type`: `task` (flag default).
- Default `priority`: `2` (P2).
- Status is always `open` on create.
- `create.require-description` in `config.yaml` may enforce description.
  - If missing description and not forced, emits warning unless `--silent`.
- `--priority` accepts `0-4` or `P0-P4` (P0 is highest).
- `--type` validates against built-in types (plus custom types if configured).

**IDs & hierarchy**:
- `--id` sets explicit ID; `--parent` cannot be used simultaneously.
- If `--parent` is provided in direct mode, child ID is generated by
  `GetNextChildID(parent)`, yielding hierarchical IDs (`bd-abc.1`, `bd-abc.2`, ...).
- Explicit IDs are validated:
  - Prefix format and length
  - Prefix must match `issue_prefix` or `allowed_prefixes` unless `--force`
  - Agent IDs (if `--type=agent`) have additional validation (Gastown-only).

**Dependencies & labels**:
- `--deps` supports `type:id` or `id` (default type `blocks`).
- `--waits-for` and `--waits-for-gate` attach a waits-for dependency with metadata.
- `--parent` also creates a `parent-child` dependency after create.
- Labels are added after creation; failures are warnings, not fatal.

**Routing flags (classic + Gastown)**:
- `--repo` bypasses auto-routing and creates in that repo (direct mode only).
- `--rig` / `--prefix` route to a different rig via `routes.jsonl` (Gastown).
  - See §15.54 for details.

**Output**:
- `--json`: prints the created issue object (full Issue schema).
- `--silent`: prints only the ID (no extra text).
- Default: human-readable summary:
  - `Created issue: <id>`
  - `Title`, `Priority`, `Status` lines.

#### 15.47.2 update

**Invocation**:
- `bd update <id...>` updates one or more issues.
- If no ID is provided, uses `last_touched` (most recent create/update/show/close).

**Field updates**:
- Supports updates to `status`, `priority`, `title`, `description`, `design`,
  `acceptance_criteria`, `notes`, `assignee`, `external_ref`, `estimate`, `type`.
- Label operations:
  - `--add-label`, `--remove-label`, `--set-labels`.
- Parent reparenting via `--parent`.
- `--due` / `--defer` accept relative or absolute time parsing and can be cleared.

**Claim behavior**:
- `--claim` performs an atomic claim:
  - Fails if already assigned
  - Sets `assignee = actor` and `status = in_progress`.

**Status close shortcut**:
- If status set to `closed`, include `closed_by_session` if
  `--session` or `CLAUDE_SESSION_ID` is available.

**Output**:
- `--json`: array of updated Issue objects (one per ID updated).
- Human: per-issue confirmation or warnings.

#### 15.47.3 close

**Invocation**:
- `bd close <id...>` or closes `last_touched` if no ID supplied.

**Core behavior**:
- Validates closability (pinned requires `--force`).
- Blocks closure if issue is blocked by open blockers (unless `--force`).
- Sets status to `closed`, `closed_at`, and `close_reason` (from `--reason`).

**Special modes**:
- `--suggest-next`: includes newly unblocked issues (direct mode only).
- `--continue`: in molecule workflows, auto-advance to next step (Gastown).

**Output**:
- `--json` default: array of closed issues.
- If `--suggest-next`: JSON object `{ "closed": [...], "unblocked": [...] }`.
- If `--continue`: JSON object `{ "closed": [...], "continue": { ... } }`.
- Human output: `Closed <id>: <reason>` per issue.

#### 15.47.4 reopen

**Behavior**:
- Explicit command to reopen closed issues (sets `status=open` and clears `closed_at`).
- Adds an optional comment if `--reason` is provided.
- Uses `ResolvePartialID` in direct mode, RPC in daemon mode.

**Output**:
- `--json`: array of reopened Issue objects.
- Human: `Reopened <id>` with optional `: <reason>`.

**Port notes (non-invasive)**:
- `br` should preserve the above semantics for classic use.
- Gastown-only flags (mol, agent, gate, rig, convoy) are excluded from v1.

---

### 15.48 Import / Export (JSONL)

#### 15.48.1 import

**Input**:
- Reads JSONL from stdin or `-i <file>`.
- Rejects positional arguments with a helpful hint.

**Direct-mode only**:
- Import bypasses daemon; uses direct SQLite connection.

**Conflict handling**:
- Detects git conflict markers in JSONL and attempts automatic 3-way merge.
- On merge success, restarts import from merged file.

**Initialization**:
- If DB exists but `issue_prefix` missing, detects prefix from JSONL.
- Falls back to directory name if JSONL has no detectable prefix.

**Collision and prefix mismatch**:
- Prefix mismatches produce a detailed report; can be fixed with:
  - `--rename-on-import` (remap IDs to DB prefix)
  - or later `bd rename-prefix`.
- Collisions (same ID, different content) abort with report.

**Flags (classic)**:
- `--skip-existing`: create-only (do not update existing).
- `--strict`: fail on any errors in dependencies/labels.
- `--rename-on-import`: rename imported IDs to configured prefix.
- `--orphan-handling`: `strict|resurrect|skip|allow`.
- `--clear-duplicate-external-refs`: clears duplicates rather than error.
- `--dedupe-after`: post-import duplicate detection summary.
- `--dry-run`: preview counts, do not write.
- `--force`: update import metadata even if no changes.

**Output**:
- No structured JSON output (even when `--json` is set).
- Human output goes to stderr.
- Summary includes counts: created, updated, unchanged, skipped, remapped.

#### 15.48.2 export

**Formats**:
- `jsonl` (default): one JSON object per line.
- `obsidian`: Markdown task format (default output `ai_docs/changes-log.md`).

**Filters**:
- `--status`, `--assignee`, `--type`, `--label`, `--label-any`.
- `--priority`, `--priority-min`, `--priority-max`.
- `--created-*`, `--updated-*` date filters.

**Tombstones and wisps**:
- Tombstones included by default (if no status filter), to propagate deletes.
- Ephemeral/wisp issues are always excluded from JSONL export.

**Safety checks**:
- Refuses to export if DB has fewer issues than existing JSONL (staleness guard).
- Can override with `--force`.

**Output**:
- JSONL writes to stdout or file; order is sorted by ID.
- If `--json` is set: outputs a stats object to stderr, not stdout:
  - `{ success, exported, skipped, total_issues, output_file? }`.

---

### 15.49 Sync / Flush / Auto-Flush (Classic vs Non-Invasive Port)

**Legacy behavior**:
- `bd sync` performs git pull/merge, 3-way merge, export, commit, push.
- `--flush-only` exports pending changes to JSONL (no git operations).
- `--import-only` imports from JSONL (no git operations).
- Auto-flush manager debounces writes and exports JSONL after changes.
- Auto-import triggers when JSONL content hash differs from DB metadata.

**Classic port intent (br)**:
- Keep explicit `export` and `import` commands.
- Keep an explicit `sync --flush-only` alias for export (no git operations).
- Drop automatic git operations, pull/push, auto-commit, and sync-branch.
- Disable by default any auto-import/auto-flush that changes files without
  explicit user action (must be opt-in if retained at all).

---

### 15.50 Maintenance Commands (classic subset)

#### 15.50.1 cleanup

**Purpose**:
- Convert closed issues to tombstones and prune expired tombstones.

**Safety**:
- Requires `--force` unless `--dry-run`.
- `--hard` bypasses tombstone TTL safety (prunes immediately).
- `--cascade` deletes dependents recursively.
- `--older-than N` restricts to issues closed before cutoff.
- `--ephemeral` targets only closed wisps.

**JSON output**:
- Empty case: `{ deleted_count, message, filter?, ephemeral? }`.
- Otherwise uses delete batch JSON shape:
  `{ deleted, deleted_count, dependencies_removed, labels_removed,
     events_removed, references_updated, orphaned_issues }`.

#### 15.50.2 compact

**Modes**:
- `--prune`: age-based tombstone pruning.
- `--purge-tombstones`: dependency-aware pruning.
- `--analyze` / `--apply`: offline compaction workflow (no API key).
- `--auto`: AI compaction (requires API key; legacy).
- `--stats`: show compaction statistics.

**Port decision**:
- `br` v1 should exclude AI compaction and any remote API dependency.
- `prune`/`purge` may be kept as explicit maintenance commands only.

#### 15.50.3 migrate

**Default behavior**:
- Detects `.beads/*.db`, migrates or renames to configured DB name.
- Updates schema version metadata; can remove stale DBs with confirmation.

**Subcommands**:
- `hash-ids`, `issues`, `sync`, `tombstones` (legacy, often Gastown-related).

**Port decision**:
- `br` v1 keeps schema migration only (forward-compatible upgrades).
- Drop multi-repo sync-branch, hash-ids, and other Gastown-specific migrations.

---

### 15.51 Duplicates / Dedupe Workflows

**Command**: `bd duplicates`

**Grouping logic**:
- Groups open issues by content key: `{title, description, design, acceptance_criteria, status}`.
- Closed issues are excluded from grouping.

**Merge target selection**:
1. Highest dependent count (most referenced by other issues).
2. Highest text reference count (mentions in descriptions/notes).
3. Lexicographically smallest ID (tie-breaker).

**Output (JSON)**:
```
{
  "duplicate_groups": <int>,
  "groups": [
    {
      "title": "...",
      "issues": [
        {"id": "bd-1", "title": "...", "status": "open", "priority": 2,
         "references": 3, "dependents": 1, "is_merge_target": true}
      ],
      "suggested_target": "bd-1",
      "suggested_sources": ["bd-2"],
      "suggested_action": "bd close ... && bd dep add ... --type related",
      "note": "Duplicate: ..."
    }
  ],
  "merge_commands": ["..."]?,
  "merge_results": [ {"target":..., "sources":..., "closed":..., "linked":..., "errors":...} ]?
}
```

**Auto-merge**:
- `--auto-merge` closes sources and adds `related` deps to target.
- `--dry-run` prints merge commands without applying.

---

### 15.52 Orphans Command

**Definition**: issues referenced in git commits but still open/in_progress.

**Detection**:
- Runs `git log --oneline --all` and extracts IDs like `(bd-abc123)`.
- Filters to open/in_progress issues in DB.

**Output (JSON)**:
```
[
  {"issue_id": "bd-123", "title": "...", "status": "open",
   "latest_commit": "abc123", "latest_commit_message": "..."}
]
```

**Flags**:
- `--details`: include latest commit hash/message in human output.
- `--fix`: interactive confirmation, then runs `bd close <id>` per orphan.

---

### 15.53 ID Resolution, Prefix Semantics, and Rename

**Partial ID resolution** (`ResolvePartialID`):
- Accepts:
  - Full ID (`bd-abc123`)
  - Hash only (`abc123`)
  - Prefix without hyphen (`bdabc123`)
  - Hierarchical (`bd-abc123.1`)
- Resolution order:
  1. Exact ID match (SearchIssues w/ IDs filter).
  2. Normalize by prefix (configured `issue_prefix`) and retry exact match.
  3. Substring match against issue hash portion across all prefixes.
- Ambiguous matches produce an error listing candidate IDs.

**Prefix validation**:
- Explicit IDs must match `issue_prefix` or `allowed_prefixes`.
- `allowed_prefixes` is a comma-separated config value used for multi-prefix
  environments (Gastown).

**rename-prefix**:
- Validates new prefix (letters/numbers/hyphen, max length 8, trailing hyphen required).
- Detects multi-prefix corruption; requires `--repair` to consolidate.
- Updates IDs and all text references.
- Re-exports JSONL and resets metadata hashes.
- `--dry-run` prints sample mappings.

**Port note**:
- `br` v1 should keep partial ID resolution and strict prefix validation
  but may defer rename-prefix until after core commands stabilize.

---

### 15.54 Creation Routing and Cross-Repo Targets

**Auto routing**:
- `routing.mode = auto` uses user role detection:
  - Maintainer -> `routing.maintainer`
  - Contributor -> `routing.contributor`
- Falls back to `routing.default` or `.`.
- Legacy keys: `contributor.auto_route`, `contributor.planning_repo`.

**Explicit routing**:
- `--repo` overrides auto routing.
- Creates in target repo using direct mode and storage factory.

**Rig / prefix routing (Gastown)**:
- `--rig` or `--prefix` uses `routes.jsonl` at town root.
- Accepts rig name (`beads`), prefix with hyphen (`bd-`), or prefix without hyphen (`bd`).
- Resolves `.beads` directory for target rig and uses `PrefixOverride` for ID generation.

**Port decision**:
- `br` v1 keeps `--repo` as an explicit target option.
- `--rig` / `--prefix` routing (Gastown) is excluded from classic port.

---

### 15.55 Status / Type / Priority Customization

**Built-in statuses**: `open`, `in_progress`, `blocked`, `deferred`, `closed`, `tombstone`.

**Custom statuses**:
- Config key: `status.custom` (comma-separated), stored in DB config.
- Validated by `Status.IsValidWithCustom`.

**Built-in types (classic)**: `bug`, `feature`, `task`, `epic`, `chore`.

**Custom types**:
- Config key: `types.custom` (comma-separated), stored in DB config.
- Validated by `IssueType.IsValidWithCustom`.

**Priority**:
- Fixed range `0..4` (no custom ranges in classic).

**Port note**:
- `br` v1 keeps custom statuses and types.
- Gastown-only built-in types (agent/rig/gate/etc.) excluded.

---

### 15.56 Error Output and Exit Conventions

**Exit codes**:
- Success: `0`
- Fatal errors: `1`

**JSON error shape**:
- `FatalErrorRespectJSON` outputs `{ "error": "..." }` to stdout when `--json`.
- `outputJSONError` (auto-flush) outputs `{ "error": "...", "code"?: "..." }` to stderr.

**Stdout vs stderr**:
- Human-readable errors/warnings generally go to stderr.
- `--json` outputs are usually stdout, except:
  - `export --json` writes stats JSON to stderr to keep stdout for JSONL.
  - `import` ignores `--json` and writes human output to stderr.

**Port note**:
- `br` should keep this separation to avoid corrupting JSONL / JSON output.

---

### 15.57 No-DB Mode (`--no-db`)

**Purpose**: operate from JSONL without SQLite (memory-only).

**Initialization**:
- Locates `.beads/` (or `BEADS_DIR`).
- Reads `issues.jsonl` if present; otherwise starts empty.
- Detects prefix in order:
  1. `issue-prefix` in `config.yaml`
  2. Common prefix across all issues (must be consistent)
  3. Directory name fallback
- Mixed prefixes -> error (must set config explicitly).

**Runtime**:
- Uses in-memory storage for all operations.
- No daemon, no auto-import, no SQLite-specific features.

**Persistence**:
- At command exit, writes `issues.jsonl` atomically.
- Filters out ephemeral/wisp issues.

**Port note**:
- `br` should keep `--no-db` but limit features to safe read/write of JSONL.

---

### 15.58 Delete Command (Tombstones + Reference Cleanup)

**Command**: `bd delete <id...>`

**Inputs**:
- Accepts one or more IDs (positional).
- `--from-file <path>` reads one ID per line (ignores blank lines and `#` comments).
- De-duplicates IDs before processing.

**Core behavior**:
- Deletes issues in a **single transaction** (SQLite) and creates tombstones.
- Removes dependencies/labels/events associated with deleted issues.
- Updates text references in connected issues (description, notes, design, acceptance_criteria),
  replacing matches with `[deleted:<id>]`.

**Dependency safety**:
- `--cascade`: recursively delete all dependents.
- `--force`: delete requested issues even if they have external dependents; dependents become orphaned.
- Neither flag: **fails** if any issue has dependents outside the deletion set.

**Hard delete**:
- `--hard` **prunes tombstones from JSONL immediately** (negative TTL).
- Tombstones remain in DB to prevent resurrection until cleanup.
- In `--no-db` fallback, `--hard` removes the issue from JSONL directly.

**Preview / dry-run**:
- Without `--force`, shows a **preview** and exits.
- `--dry-run` shows the same preview but explicitly says no changes made.

**JSON output (SQLite path)**:
```
{
  "deleted": ["bd-1", "bd-2"],
  "deleted_count": 2,
  "dependencies_removed": 7,
  "labels_removed": 3,
  "events_removed": 1,
  "references_updated": 4,
  "orphaned_issues": ["bd-9"]
}
```

**JSON output (`--no-db` fallback)**:
```
{
  "deleted": ["bd-1"],
  "deleted_count": 1,
  "dependencies_removed": 2,
  "references_updated": 1
}
```

**Port notes (non-invasive)**:
- Keep tombstone semantics (classic hybrid model).
- Avoid automatic git modifications during delete (legacy uses pruning in JSONL only).

---

### 15.59 Merge / Resolve-Conflicts / Repair (Maintenance Tools)

#### 15.59.1 merge (git merge driver)

**Purpose**: 3-way JSONL merge driver for `.beads/issues.jsonl`.
Also reused for **sync-branch divergence recovery** when git rebase would
resurrect tombstones.

**Usage**: `bd merge <output> <base> <left> <right>`
- Exit codes: `0` success, `1` conflicts, `2` error.
- Merge rules (content‑level):
  - **Identity** by `{id, created_at, created_by}` with **ID fallback** if timestamps differ.
  - **Tombstones** win over live issues unless **expired** (TTL = 30 days + 1h clock‑skew grace).
  - **Deletion beats modification** if issue disappears on one side vs base.
  - **Title/description**: newer `updated_at` wins.
  - **Notes**: concatenate with separator `\n\n---\n\n`.
  - **Status**: `closed` overrides open; tombstone handled above.
  - **Priority**: `0` treated as unset; otherwise lower number wins.
  - **Issue type**: left side wins on conflict (local bias).
  - **Closed metadata**: `closed_at` uses newest timestamp; `close_reason/closed_by_session`
    taken from side with newer `closed_at`.
  - **Dependencies**: 3‑way merge where **removals win**; union additions; left metadata preferred.
- Output is **sorted by ID** for deterministic diffs.
- Conflicts are rare (auto‑resolved); if emitted, they are appended to output and
  the command returns non‑zero.

**Schema caveat**:
- Merge driver schema is a **subset** of full issue fields. Any unknown JSON fields
  (labels/comments/assignee/design/etc) are **dropped** on output.

**Note**: Not for duplicate issues (use `bd duplicates`).

**Cleanup**:
- Legacy cleanup removes backup files and runs `git clean -f` in `.beads/`.

**Port note**:
- `br` v1 should **exclude** git-side merge driver and cleanup behavior
  (conflicts with non-invasive policy). Keep merge logic only if explicitly
  invoked for JSONL repair (see resolve-conflicts).

#### 15.59.2 resolve-conflicts

**Command**: `bd resolve-conflicts [file]`

**Behavior**:
- Parses conflict markers `<<<<<<<`, `=======`, `>>>>>>>` in JSONL.
- Mode `mechanical` (default): deterministic merge rules.
- Mode `interactive`: not implemented.
- Writes resolved file and creates backup: `<file>.pre-resolve`.

**Resolution rules** (mechanical):
- If both sides unparseable: keep both.
- If only one side parseable: keep that side.
- If both parseable:
  - `updated_at` wins for title/description/status
  - `closed` overrides other statuses
  - notes concatenated if different
  - priority picks lower number (higher priority)
  - dependencies unioned

**JSON output shape**:
```
{
  "file_path": "...",
  "dry_run": false,
  "mode": "mechanical",
  "conflicts_found": 2,
  "conflicts_resolved": 2,
  "status": "success",
  "backup_path": "...",
  "conflicts": [
    {
      "line_range": "10-24",
      "left_label": "HEAD",
      "right_label": "branch",
      "resolution": "merged",
      "issue_id": "bd-123"
    }
  ]
}
```

#### 15.59.3 repair

**Purpose**: repair corrupted DB that fails invariant checks by removing orphans.

**Behavior**:
- Opens SQLite directly (bypasses invariants).
- Finds and deletes orphans:
  - dependencies with missing `issue_id`
  - dependencies with missing `depends_on_id` (excluding `external:*`)
  - labels with missing issue
  - comments with missing issue
  - events with missing issue
- Creates backup `beads.db.pre-repair`.
- Executes cleanup in a transaction and runs WAL checkpoint.

**JSON output shape**:
```
{
  "database_path": "...",
  "dry_run": false,
  "orphan_counts": {
    "dependencies_issue_id": 3,
    "dependencies_depends_on": 2,
    "labels": 1,
    "comments": 0,
    "events": 0,
    "total": 6
  },
  "orphan_details": { ... },
  "status": "success",
  "backup_path": "beads.db.pre-repair"
}
```

**Port note**:
- Keep **as explicit manual tool only** (never automatic).
- Require user intent (no background repair).

---

### 15.60 Edit / Move / Refile

#### 15.60.1 edit

**Command**: `bd edit <id> [--title|--description|--design|--notes|--acceptance]`

**Behavior**:
- Opens `$EDITOR` (or `$VISUAL`; falls back to `vim/vi/nano/emacs`).
- Writes current field to temp file, lets user edit, then updates field.
- Default field: `description`.
- If unchanged, prints “No changes made”.
- Title cannot be empty.

**Output**:
- Human only (no JSON shape): `Updated <field> for issue: <id>`.

#### 15.60.2 move (Gastown)

**Command**: `bd move <id> --to <rig|prefix>`

**Behavior**:
- Creates new issue in target rig, copies fields + labels.
- Adds `(Moved from <old>)` to description.
- Dependencies:
  - deps **from** moved issue are removed
  - dependents are updated to `external:<rig>:<new-id>`
- Closes source issue unless `--keep-open`.
- `--skip-deps` bypasses dependency remapping.

**JSON output**:
`{ "source": "...", "target": "...", "closed": true, "deps_remapped": 3 }`

#### 15.60.3 refile (Gastown-lite)

**Command**: `bd refile <id> <rig>`

**Behavior**:
- Like `move`, but **no dependency remapping**.
- Adds `(Refiled from <old>)` to description.
- Closes source unless `--keep-open`.

**JSON output**:
`{ "source": "...", "target": "...", "closed": true }`

**Port note**:
- `move` / `refile` are Gastown-oriented and are **excluded** from `br` v1.

---

### 15.61 Reset (Destructive Admin) and Clean (Missing)

#### 15.61.1 reset

**Command**: `bd reset`

**Purpose**: remove all beads data and configuration, returning to an uninitialized state.

**Preview mode (default)**:
- Runs in dry-run unless `--force` is provided.
- JSON output:
```
{ "dry_run": true, "items": [ { "type": "...", "path": "...", "description": "..." } ] }
```
- Human output enumerates items to be removed.

**Items removed when `--force`**:
- `.beads/` directory (DB, JSONL, config).
- Beads git hooks in `.git/hooks/` (pre-commit, post-merge, pre-push, post-checkout).
- Merge driver config keys: `merge.beads.driver`, `merge.beads.name`.
- `.gitattributes` entry containing `merge=beads` (file is deleted if empty).
- Sync-branch worktrees under `.git/beads-worktrees/`.
- Running daemon (if detected via `.beads/daemon.pid`).

**JSON output (force)**:
```
{ "reset": true, "success": true, "errors": ["..."]? }
```

**Error cases**:
- Not a git repo -> error.
- No `.beads` -> message “Beads not initialized”.

**Port note (non-invasive)**:
- `br` v1 should **exclude** `reset` because it deletes files and modifies git config.
- If reintroduced, require explicit confirm prompts and avoid touching hooks by default.

#### 15.61.2 clean (referenced, not present)

**Observation**:
- `bd clean` is referenced in help text, but no `clean` command exists in the current legacy tree.
- Cleanup behavior is partially embedded in `bd merge` (git clean in `.beads/`)
  and in doctor `--fix` tooling.

**Port note**:
- Do **not** implement `br clean` unless a clear spec is restored.

---

### 15.62 Restore (Compaction Rollback via Git)

**Command**: `bd restore <issue-id>`

**Purpose**: display full pre-compaction issue content by checking out the commit
referenced in `compacted_at_commit` and reading JSONL at that point in history.

**Behavior**:
- Requires git repo and clean working tree (no uncommitted changes).
- Checks out the commit, reads the issue from JSONL, prints details, then returns to original HEAD.
- Read-only intent, but **does perform git checkout**.

**Output**:
- Human-only; `--json` flag is accepted but **not used** in the current implementation.

**Port note**:
- `br` v1 should **exclude** restore (too invasive; git state mutation).

---

### 15.63 Audit Log (Agent Interaction Recording)

**Command**: `bd audit`

**Storage**:
- Append-only JSONL at `.beads/interactions.jsonl`.
- Entry IDs prefixed `int-` (random 4 bytes, hex).

**Subcommands**:

**`bd audit record`**:
- Accepts explicit fields or reads JSON from stdin.
- If stdin is piped and **no explicit fields** are provided, stdin JSON is used.
- `--stdin` forces stdin JSON.
- `--kind` is required if using flags (not stdin).
- JSON output: `{ "id": "...", "kind": "..." }`.
- Text output: prints the ID only.

**`bd audit label <entry-id>`**:
- Appends a new entry with `kind="label"` and `parent_id=<entry-id>`.
- Requires `--label` value; optional `--reason`.
- JSON output: `{ "id": "...", "parent_id": "...", "label": "..." }`.

**Entry schema** (`internal/audit.Entry`):
```
{
  "id": "int-....",
  "kind": "llm_call" | "tool_call" | "label" | ...,
  "created_at": "RFC3339",
  "actor": "...",
  "issue_id": "bd-...",
  "model": "...",
  "prompt": "...",
  "response": "...",
  "error": "...",
  "tool_name": "...",
  "exit_code": 0,
  "parent_id": "...",
  "label": "good" | "bad" | ...,
  "reason": "...",
  "extra": { ... }
}
```

**Port note**:
- Optional for `br` v1, but consistent with “agent-first” workflows.

---

### 15.64 Activity Feed (Daemon-Only)

**Command**: `bd activity`

**Dependencies**:
- Requires daemon (RPC mutations). Direct mode is rejected.
- `--town` aggregates from multiple rigs via `routes.jsonl` (Gastown).

**Filters**:
- `--mol <prefix>` filters by issue ID prefix.
- `--type <event>` filters by mutation type (`create`, `update`, `delete`, `comment`, etc.).
- `--since <duration>` supports `5m`, `1h`, `2d` format.
- `--limit <N>` default 100.

**Follow mode**:
- `--follow` polls every `--interval` (default 500ms).
- Emits warnings when daemon unreachable.

**Output**:
- Non-follow JSON: **array** of ActivityEvent objects.
- Follow JSON: **one JSON object per line**, not wrapped in an array.
- Human output: `[HH:MM:SS] <symbol> <message>`.

**Event display**:
- Symbols: `+` create/bonded, `→` update/started, `✓` closed, `✗` fail, `⊘` deleted, `💬` comment.

**Port note**:
- Exclude from `br` v1 (daemon is excluded).

---

### 15.65 Last-Touched Tracking

**Purpose**: store the most recently touched issue ID to support commands without IDs.

**Storage**:
- File: `.beads/last-touched` (local-only).
- Permissions: `0600`.

**Behavior**:
- `SetLastTouchedID` is best-effort (errors ignored).
- `GetLastTouchedID` returns empty on missing/invalid file.
- `ClearLastTouched` deletes the file (best-effort).

**Used by**:
- `bd update` and `bd close` when no IDs are provided.

---

### 15.66 Defer / Undefer

**Commands**:
- `bd defer <id...> [--until <time>]`
- `bd undefer <id...>`

**Behavior**:
- `defer` sets `status=deferred` and optionally `defer_until`.
- `undefer` sets `status=open` and clears `defer_until` (nil/empty).
- `--until` uses natural time parsing (`+1h`, `tomorrow`, `2025-01-15`).

**Output**:
- `--json`: array of updated Issue objects.
- Human: `Deferred <id>` or `Undeferred <id> (now open)`.

---

### 15.67 Integrity Helpers (No CLI Command)

**Note**: There is no `bd integrity` command in the current legacy tree.
Integrity logic exists as helper functions used by export/sync.

**Key helpers**:
- `hasJSONLChanged` compares `jsonl_content_hash` metadata vs JSONL content hash.
- `computeDBHash` exports DB to memory (issues + deps + labels + comments), hashes content.
- `validatePreExport` prevents exporting empty DB over non-empty JSONL and requires import
  if JSONL content hash differs from metadata.

**Port note**:
- Keep these checks internal to `export` / `sync --flush-only`.

---

### 15.68 Sync Submodes (Legacy Git Workflow)

`bd sync` is **pull-first** and **merge-aware**. It runs in **direct mode** (no daemon)
to avoid stale SQLite handles.

#### 15.68.1 Core `bd sync` flow (pull-first)

Default flow (when neither `--no-pull` nor `--squash` is set):
1) **Load local DB state** (includes tombstones)
2) Acquire `.beads/.sync.lock` (prevents concurrent sync)
3) Load **base state** from `.beads/sync_base.jsonl` (may be empty on first sync)
4) Pull remote:
   - If `sync.branch` configured → **worktree sync-branch** pull (see 15.68.2)
   - Else → `git pull` on current branch
5) Load **remote state** from JSONL **after pull**
6) **3-way merge**: `base` × `local` × `remote` → merged JSONL (LWW + unions)
7) Import merged JSONL into DB
8) Export DB → JSONL (DB is source of truth)
9) Commit + push changes (branch or sync-branch worktree)
10) Update base state (`sync_base.jsonl`) **after** successful push
11) Clear transient sync state (if present)

Important behaviors:
- **Pull-first** avoids “export-before-pull” data loss (local overwriting remote).
- Base state update is **post-push** to avoid acknowledging an export that never made it to git.
- A preflight check warns on **uncommitted `.beads` changes** and re-exports to reconcile.
- If git is in a merge/rebase state (`unmerged paths`), sync **aborts** with guidance.

#### 15.68.2 Sync-branch worktree mode

When `sync.branch` (aka `sync-branch`) is configured, sync uses a **dedicated worktree**
under the repo’s **git common dir**:
```
<git-common-dir>/beads-worktrees/<sync-branch>/
```

Key points:
- JSONL and `metadata.json` are synced **into** the worktree, committed there,
  and pushed from the worktree (to avoid touching the user’s working tree).
- Commits in the worktree use `--no-verify` (hooks are skipped).
- Push sets `BD_SYNC_IN_PROGRESS=1` so pre-push hooks can bypass circular warnings.
- Before committing, the worktree attempts a **preemptive fetch + fast-forward**
  to reduce divergence.
- Divergence recovery uses **content merge** (JSONL 3-way merge), **not** git rebase,
  to avoid tombstone resurrection.

**Mass deletion safety (sync-branch only)**:
- After a **divergent merge**, if **>50% issues vanish** and **>5 existed**,
  warnings are attached to the result.
- If `sync.require_confirmation_on_mass_delete=true`, auto-push is **skipped**
  until confirmation; otherwise it pushes with warnings.

#### 15.68.3 Redirect + sync-branch incompatibility

If `.beads/redirect` is active:
- Git operations are **skipped** (the “owner” repo handles sync).
- If `sync.branch` is configured **and** redirect is active, sync prints a warning
  and **only exports JSONL** locally.

#### 15.68.4 Submodes

**`bd sync --status`**:
- Shows branch differences between current branch and `sync.branch`.
- Prints commit logs (`git log --oneline`) and diff for `.beads/issues.jsonl`.

**`bd sync --merge`**:
- Merges `sync.branch` into current branch (git merge with message).
- Refuses if uncommitted changes exist.
- Suggests follow-up steps: `sync --import-only`, then `sync`.

**`bd sync --from-main`**:
- One-way sync from default remote branch (uses `sync.remote` if set).
- Steps: fetch remote, checkout `.beads/` from remote branch, import JSONL.
- Forces `noGitHistory` to avoid deletion artifacts.
- Auto-selected when **no upstream** exists but a remote does (ephemeral branches).

**`bd sync --check`**:
- Runs pre-sync integrity checks:
  - forced-push detection for sync branch
  - prefix mismatch in JSONL
  - orphaned children in JSONL
- Exits with code 1 if problems found.
- `--json` returns a structured `SyncIntegrityResult`.

**`bd sync --import-only`**:
- Imports JSONL directly (no git).
- Uses **inline import** (no subprocess) to avoid `.beads/redirect` path issues.

**`bd sync --flush-only`**:
- Exports pending changes to JSONL and exits (no git).

**`bd sync --no-pull`**:
- Skips pull/merge: export → commit → push.

**`bd sync --squash`**:
- Export only; do **not** commit/push (accumulate changes for a later sync).

**Port note (non-invasive)**:
- `br` v1 should **exclude all git-based sync modes**.
- Keep only `sync --flush-only` as a synonym for `export` if desired.

---

### 15.69 Config Key Catalog (Legacy)

**YAML-only keys** (stored in `.beads/config.yaml`):
- Bootstrap: `no-db`, `no-daemon`, `no-auto-flush`, `no-auto-import`, `json`, `auto-start-daemon`
- DB/identity: `db`, `actor`, `identity`
- Timing: `flush-debounce`, `lock-timeout`, `remote-sync-interval`
- Git: `git.author`, `git.no-gpg-sign`, `no-push`, `no-git-ops`
- Sync: `sync-branch` / `sync.branch`, `sync.require_confirmation_on_mass_delete`
- Daemon: `daemon.auto_commit`, `daemon.auto_push`, `daemon.auto_pull`
- Routing: `routing.mode`, `routing.default`, `routing.maintainer`, `routing.contributor`
- Create: `create.require-description`
- Validation: `validation.on-create`, `validation.on-sync`
- Hierarchy: `hierarchy.max-depth`

**DB-stored config** (via `bd config set`):
- `issue_prefix` (canonical prefix, no trailing hyphen)
- `allowed_prefixes` (comma-separated list of additional valid prefixes)
- `status.custom` (comma-separated custom statuses)
- `types.custom` (comma-separated custom issue types)
- `import.missing_parents` (`strict|resurrect|skip|allow`)
- `sync.remote` (remote name for sync-from-main)

**Port note**:
- `br` v1 should keep DB-stored keys required for classic workflows
  and limit YAML-only keys to startup behavior (no daemon, no git hooks).

---

### 15.70 JSONL Merge Driver Rules (Vendored)

This is the field-level merge logic used by `bd merge` and the sync 3-way merge.

**Identity**:
- Keyed by `{id, created_at, created_by}` with fallback to `id` when necessary.

**Tombstones**:
- Tombstone wins over live unless expired (default TTL 30d + 1h clock skew grace).
- Tombstone merge uses later `deleted_at`.
- Deletion wins over modification when only one side deleted.

**Field rules (conflicts)**:
- `title`, `description`: last-write-wins by `updated_at`.
- `notes`: concatenate with `\n\n---\n\n` separator.
- `status`: closed wins; tombstone wins as safety fallback.
- `priority`: lower number wins; `0` treated as “unset” when competing.
- `issue_type`: left wins on conflict.
- `dependencies`: 3-way merge, **removals win** over additions.
- `labels`: union (dedupe).
- `comments`: append + dedupe (by id or content).
- `closed_at`: max; `close_reason` / `closed_by_session` from later closed_at.

**Port note**:
- Only relevant if `br` retains JSONL merge tooling. Otherwise omit.

---

### 15.71 Lint Command (Template Validation)

`bd lint` performs lightweight template validation against the **issue description**.
It **does not** require formal Markdown headings; it only checks whether the
required heading text appears **case-insensitively** anywhere in the description.

**CLI**:
- `bd lint [issue-id...]`
- Flags:
  - `--type, -t <type>`: filter by issue type.
  - `--status, -s <status>`: filter by status; default is `open`. Use `all` for all statuses.
  - `--json` (global): emit JSON.

**Defaults / selection**:
- If IDs are provided, only those issues are linted.
- Without IDs: lists issues (daemon) or searches (direct), defaulting to `open`.
- Daemon list hard-caps at **1000** issues.
- Errors fetching a specific issue are logged to stderr and skipped; lint continues.

**Required sections (classic scope)**:
- `bug`: **Steps to Reproduce**, **Acceptance Criteria**
- `task` and `feature`: **Acceptance Criteria**
- `epic`: **Success Criteria**
- All other types: no requirements (lint yields no warnings).

**Validation details**:
- Matching is substring-based, case-insensitive.
- Markdown prefixes (`#` / `##`) are stripped before matching.

**JSON output shape**:
```json
{
  "total": 3,
  "issues": 2,
  "results": [
    {
      "id": "bd-abc123",
      "title": "Fix login bug",
      "type": "bug",
      "missing": ["## Steps to Reproduce"],
      "warnings": 1
    }
  ]
}
```
Notes:
- `results` includes **only** issues with warnings.
- `issues` is the count of warning-bearing issues (not total scanned).
- `total` is the **sum** of missing sections across all results.
- In JSON mode, `bd lint` exits **0** even if warnings are present.

**Human output**:
- No warnings: `✓ No template warnings found (N issues checked)`
- With warnings:
  - `Template warnings (X issues, Y warnings):`
  - One block per issue:
    - `<id> [<type>]: <title>`
    - `  ⚠ Missing: <Heading>` (one line per missing heading)
- Exit code is **1** if warnings are found in human mode.

**Port note (non-invasive)**:
- Keep lint in `br` v1 (useful for agents), but keep **strictly read-only**.
- Scope to classic types only; avoid gastown-only types.

---

### 15.72 Markdown Bulk Create (`bd create --file`)

Bulk creation is supported via `bd create --file <path>` and parses a Markdown
document into multiple issues. This is **not** the same as JSONL import; it is
only a CLI convenience.

**CLI constraints**:
- `--file` (`-f`) is mutually exclusive with a positional title.
- `--dry-run` is **not supported** with `--file`.
- File must be `.md` or `.markdown`, must exist, and **may not** contain `..`.

**Markdown grammar**:
- Each issue begins with an **H2** header:
  - `## Issue Title`
- Per-issue sections are **H3** headers:
  - `### Section Name`
- Recognized section names (case-insensitive):
  - `Priority`
  - `Type`
  - `Description`
  - `Design`
  - `Acceptance Criteria` (alias: `Acceptance`)
  - `Assignee`
  - `Labels`
  - `Dependencies` (alias: `Deps`)
- Unknown sections are ignored.

**Defaults**:
- `Priority`: **2**
- `Type`: **task**

**Description quirks (actual behavior)**:
- Lines immediately after the H2 title **before any H3 section** are treated
  as the description.
- **Only the first non-empty line** is captured; subsequent lines are ignored.
  (This is a known parsing quirk in the current implementation.)

**Parsing rules**:
- Section content is captured verbatim (multi-line) until the next H2/H3.
- `Labels` and `Dependencies` split on commas **or** whitespace.
- Dependencies may be `type:id` or just `id`:
  - Default type is `blocks`.
  - Invalid dependency types are warned and skipped.

**Creation behavior**:
- Direct mode: each issue is created sequentially; labels and dependencies are
  added afterward (best-effort with warnings on failure).
- Daemon mode: a batch RPC of `create` operations is used; hooks are still
  invoked per created issue.

**Output**:
- JSON mode: array of created issue objects (successes only).
- Human mode:
  - `✓ Created N issues from <file>:`
  - `  <id>: <title> [P<priority>, <type>]`
- Failures are reported to stderr as:
  - `✗ Failed to create N issues:` followed by issue titles.

**Port note**:
- Keep the grammar and quirks for compatibility; do **not** auto-fix the
  description parsing bug unless explicitly opted-in for `br`.

---

### 15.73 Info Command (`bd info`)

`bd info` prints diagnostic metadata about the database, daemon, and config.

**Flags**:
- `--json` (global): JSON output.
- `--schema`: include schema details.
- `--whats-new`: show recent version changes and exit.
- `--thanks`: show contributors/thanks page and exit.

**Normal JSON output** (no `--whats-new` / `--thanks`):
```json
{
  "database_path": "/abs/path/.beads/beads.db",
  "mode": "daemon|direct",
  "daemon_connected": true,
  "socket_path": "/abs/path/.beads/beads.sock",
  "daemon_version": "0.47.2",
  "daemon_status": "healthy",
  "daemon_compatible": true,
  "daemon_uptime": 123.4,
  "issue_count": 42,
  "daemon_fallback_reason": "no-daemon",
  "daemon_detail": "…",
  "config": { "issue_prefix": "bd", "...": "..." },
  "schema": {
    "tables": ["issues","dependencies","labels","config","metadata"],
    "schema_version": "0.47.2",
    "config": { "issue_prefix": "bd" },
    "sample_issue_ids": ["bd-abc","bd-def","bd-ghi"],
    "detected_prefix": "bd"
  }
}
```
Notes:
- `config` is fetched from the **config table** and only appears if readable.
- `schema` appears **only** when `--schema` is set.
- In direct mode, `issue_count` is computed by a full search with an empty filter.

**Human output**:
- Header:
  - `Beads Database Information`
  - `Database: <abs path>`
  - `Mode: daemon|direct`
- Daemon status block:
  - Connected? socket path? health? compatibility? uptime?
  - If not connected, prints fallback reason and detail when present.
- Issue count line when available.
- Schema block when `--schema` is set.
- **Hook warnings** appended at the end (see Port note).

**Port note (non-invasive)**:
- `bd info` prints hook warnings by calling git hook detectors.
  For `br`, **omit hook checks** unless explicitly requested with a flag.
- `--whats-new` / `--thanks` are informational; include only if desired.

---

### 15.74 Version Command (`bd version`)

`bd version` prints client version/build metadata, optionally comparing with the daemon.

**Flags**:
- `--daemon`: connect to daemon and compare versions.
- `--json` (global): JSON output.

**Normal JSON output**:
```json
{
  "version": "0.47.2",
  "build": "dev",
  "commit": "abcdef123456",
  "branch": "main"
}
```
`commit` and `branch` are omitted if unknown.

**Normal human output**:
- `bd version <Version> (<Build>)`
- If commit/branch are available:
  - `bd version <Version> (<Build>: <branch>@<short-commit>)`

**`--daemon` JSON output**:
```json
{
  "daemon_version": "0.47.2",
  "client_version": "0.47.2",
  "compatible": true,
  "daemon_uptime": 42.1
}
```

**`--daemon` human output**:
- `Daemon version: ...`
- `Client version: ...`
- `Compatibility: ✓ compatible` or `✗ incompatible (restart daemon recommended)`
- `Daemon uptime: <seconds>`
- Exits **1** if incompatible.

**Port note**:
- If `br` has no daemon, `--daemon` should either be omitted or return a clear
  error stating that no daemon is supported.

---

### 15.75 Template Commands (Deprecated, Label-Based)

`bd template` provides a **label-driven** templating system (deprecated in favor
of `bd mol` / `bd formula`). Templates are **epics** labeled `template`.
This command is **deprecated** and slated for removal upstream.

**Subcommands**:
- `bd template list`
- `bd template show <template-id>`
- `bd template instantiate <template-id> [--var key=value ...] [--assignee name] [--dry-run]`

**Template identification**:
- Templates are issues with label `template`.
- Variables use `{{var_name}}` placeholders, where names match:
  `[a-zA-Z_][a-zA-Z0-9_]*`.

**`template list`**:
- JSON: array of issue objects (templates only).
- Human:
  - If none: prints instructions for creating a template.
  - Else: `Templates (for bd template instantiate):`
    - `  <id>: <title> (vars: v1, v2)`
  - Variables are extracted from **title + description** (root only).

**`template show`**:
- Resolves ID (partial OK) before loading.
- Loads a **subgraph** of the template:
  - Root epic + descendants.
  - Child discovery uses both:
    - parent-child dependencies (`dep type = parent_child`),
    - hierarchical IDs (`parent.N`).
- Dependencies included only when **both endpoints** are inside the subgraph.
- JSON output:
```json
{
  "root": { /* Issue */ },
  "issues": [ /* Issue */ ],
  "dependencies": [ /* Dependency */ ],
  "variables": ["var1","var2"]
}
```
- Human output shows:
  - Template title, ID, issue count
  - Variables list (if any)
  - Tree structure (printed with a depth-first tree renderer)

**`template instantiate`**:
- Requires all variables used in the template subgraph to be supplied via `--var`.
  Missing vars cause an error with a suggested flag syntax.
- `--dry-run`: prints a preview list (with substituted titles) and variables.
- `--assignee`: assigns the **root epic** to the named assignee.
- JSON output:
```json
{
  "new_epic_id": "bd-new123",
  "id_mapping": { "bd-old1": "bd-new1" },
  "created": 12
}
```
- Human output:
  - `✓ Created <n> issues from template`
  - `  New epic: <id>`

**Port note (classic scope)**:
- This is **deprecated upstream** and intertwined with formula/mol systems.
- Recommendation for `br` v1: **exclude** unless explicitly requested; if included,
  keep it label-based only (no mol/wisp/daemon).

---

### 15.76 Graph Command (`bd graph`)

`bd graph` renders a dependency visualization for a single issue or all open issues.
It is **read-only** and requires direct DB access (daemon support is partial).

**CLI**:
- `bd graph <issue-id>`: render graph for a single issue subgraph.
- `bd graph --all`: render all open issues grouped by connected components.
- Flags:
  - `--compact`: tree format (one line per issue).
  - `--box`: ASCII box layout (default). **Flag exists but is effectively unused**; box is default.
  - `--json`: JSON output.

**Subgraph semantics (single issue)**:
- Root issue resolved via partial ID.
- **Traversal uses dependents only** (issues that depend on root or its descendants).
  - This is effectively a **reverse-dependency** view.
  - Direct dependencies of the root are **not** added (explicitly skipped in code).
- All dependency types are loaded **within** the subgraph for rendering, but
  layout uses only `blocks` edges.

**`--all` mode**:
- Includes issues with status `open`, `in_progress`, and `blocked`.
- Builds connected components using **all** dependency edges between those issues.
- Component sort:
  - First by size (desc), then by priority of the first issue (asc).
- Root selection within each component:
  - Prefer epics; otherwise highest priority (lowest number).

**Layout algorithm**:
- Considers only `blocks` edges.
- Assigns layers by longest path from sources:
  - Layer 0 = no blocking dependencies.
  - Each node layer = max(dep layer) + 1.
- Cycles/unassigned nodes fall back to layer 0.
- Nodes within a layer are sorted by ID.

**JSON output shape**:
```json
{
  "root": { /* Issue */ },
  "issues": [ /* Issue */ ],
  "layout": {
    "Nodes": {
      "bd-abc": { "Issue": { /* Issue */ }, "Layer": 0, "Position": 0, "DependsOn": ["bd-def"] }
    },
    "Layers": [["bd-abc","bd-def"]],
    "MaxLayer": 2,
    "RootID": "bd-abc"
  }
}
```
Notes:
- Struct fields are **capitalized** in JSON (no tags in Go).
- In `--all`, output is a **list** of subgraph objects.

**Human output (box)**:
- Header: `📊 Dependency graph for <root-id>`.
- Legend: status icons (`○` open, `◐` in_progress, `●` blocked, `✓` closed).
- Per-layer sections: `Layer N (ready)` for layer 0.
- Each node rendered as an ASCII box with:
  - Status icon + truncated title (max 30 chars),
  - Muted ID line,
  - Optional `blocks:X` / `needs:Y` line (counts exclude root as blocker).
- Summary: `Dependencies: <n> blocking relationships` and `Total: <n> issues across <m> layers`.

**Human output (compact)**:
- Header: `📊 Dependency graph for <root-id> (<n> issues, <m> layers)`.
- Legend includes `❄ deferred`.
- Per-layer headers: `LAYER N (ready)`.
- One line per issue: `<icon> <id> <priority-tag> <title>` (title max 50 chars).
- Child rendering uses **parent-child** dependencies only, sorted by priority then ID.

**Port note (non-invasive)**:
- `br` can keep `graph` as read-only visualization.
- Preserve the **reverse-dependency** traversal semantics to match `bd`.

---

### 15.77 Where Command (`bd where`)

Reports the active `.beads` location, including redirects and prefix detection.

**Behavior**:
- Finds the active beads directory (follows redirects).
- If no beads dir found:
  - Human: prints error + hint to run `bd init`, exits **1**.
  - JSON: prints `{ "error": "no beads directory found" }`, exits **1**.
- Detects redirect origin by walking up from CWD for a `.beads/redirect` file
  (or by `BEADS_DIR` env override).
- Detects prefix:
  - First from DB config `issue_prefix` (if store available).
  - Otherwise, extracts prefix from first JSONL line ID.

**JSON output shape**:
```json
{
  "path": "/abs/path/.beads",
  "redirected_from": "/abs/other/.beads",
  "prefix": "bd",
  "database_path": "/abs/path/.beads/beads.db"
}
```

**Human output**:
```
/abs/path/.beads
  (via redirect from /abs/other/.beads)
  prefix: bd
  database: /abs/path/.beads/beads.db
```

**Port note**:
- Keep for debugging redirects and multi-clone setups.

---

### 15.78 Quick Capture (`bd q`)

Creates an issue and outputs **only the ID** (no JSON; always one line).

**CLI**:
- `bd q <title...>`
- Flags:
  - `--priority, -p` default `2`
  - `--type, -t` default `task`
  - `--labels, -l` repeatable list

**Behavior**:
- Title is a joined string of args.
- Uses daemon RPC if available; otherwise direct DB.
- Adds labels best-effort (direct mode ignores label errors).
- Schedules auto-flush (direct mode).
- Output: `bd-abc123` to stdout only.

**Port note**:
- Keep in `br` v1 for agent scripting.
- Respect `--json`? Legacy ignores it; match behavior.

---

### 15.79 Prime Command (`bd prime`)

Outputs **AI workflow context** as markdown. Designed for agent hooks and
context recovery. It is **pure output**, no DB writes.

**Key behaviors**:
- If no beads dir found: exits **0** silently (no stderr) for hook safety.
- Supports `.beads/PRIME.md` override:
  - Uses local `.beads/PRIME.md` first; falls back to redirected beads dir.
  - `--export` ignores overrides and prints default content.
- Detects MCP mode by inspecting `~/.claude/settings.json` and checking for
  `mcpServers` keys containing `"beads"`.
- `--full` and `--mcp` force output mode.
- `--stealth` or config `no-git-ops=true` suppress git commands in the protocol.

**Output modes**:
- **MCP mode**: minimal reminder text (≈50 tokens), includes a close protocol line.
- **CLI mode**: full markdown guide with:
  - session close checklist,
  - core rules,
  - essential commands,
  - sync guidance (adjusted for auto-sync, no-push, ephemeral branches).

**Close protocol selection**:
- Depends on:
  - daemon auto-sync detection (auto-commit + auto-push),
  - `no-push` config,
  - git remote existence,
  - whether current branch has upstream (ephemeral).
- Outputs different session-end steps accordingly (sync vs no sync vs local-only).

**Port note (non-invasive)**:
- `bd prime` is highly git-opinionated. For `br` v1, either **omit**
  or provide a simplified, **non-git** version.

---

### 15.80 Quickstart and Human Help

Two human-facing help commands with **static text** output only:

- `bd quickstart`: a long-form, example-driven getting started guide.
- `bd human`: curated list of core commands for non-agent users.

**Port note**:
- Optional for `br`; safe to omit or re-author in Rust-specific style.

---

### 15.81 Preflight Command (`bd preflight`)

PR readiness checklist for the **Go** beads repo (tests, lint, nix hash).

**Modes**:
- Default: prints a static checklist.
- `--check`: runs checks and returns pass/fail summary.
- `--json`: JSON output only (overrides global `--json`).

**JSON output shape**:
```json
{
  "checks": [
    {
      "name": "Tests pass",
      "passed": true,
      "skipped": false,
      "warning": false,
      "output": "",
      "command": "go test -short ./..."
    }
  ],
  "passed": true,
  "summary": "3/4 checks passed (1 skipped)"
}
```

**Port note (non-invasive)**:
- This is **Go-repo specific**; exclude from `br` v1 unless rewritten for Rust.

---

### 15.82 Non-Classic CLI Flag Matrix (Excluded / Optional for br v1)

This matrix captures **non-classic** commands and their flags so the Rust port can
explicitly exclude (or later opt-in) with eyes open. Where a command is partially
retained (e.g., `sync`), the exclusion note clarifies the supported subset.
Global flags (e.g., `--json`, `--no-daemon`, `--allow-stale`) are **not** repeated
unless the command defines its own dedicated flag.

#### 15.82.1 Daemon + Daemon Management (Excluded)

| Command | Flags | Exclusion Note |
|---|---|---|
| `daemon` (legacy umbrella) | `--start`, `--stop`, `--stop-all`, `--status`, `--health`, `--metrics`, `--interval`, `--auto-commit`, `--auto-push`, `--auto-pull`, `--local`, `--log`, `--foreground`, `--log-level`, `--log-json`, `--json` | Deprecated wrapper; daemon not supported |
| `daemon start` | `--interval`, `--auto-commit`, `--auto-push`, `--auto-pull`, `--local`, `--log`, `--foreground`, `--log-level`, `--log-json` | Excluded |
| `daemon status` | `--all`, `--search` | Excluded |
| `daemon list` | `--search`, `--no-cleanup` | Alias of `daemons list`; excluded |
| `daemon health` | `--search` | Excluded |
| `daemon logs` | `--follow`, `--lines` | Excluded |
| `daemon stop` | (none) | Excluded |
| `daemon killall` | `--search`, `--force` | Excluded |
| `daemon restart` | `--search` | Excluded |
| `daemons` (alias) | Same subcommands as `daemon` | Alias only |

#### 15.82.2 Git / Repo Automation (Excluded or Partial)

| Command | Flags | Exclusion Note |
|---|---|---|
| `init` | `--prefix`, `--quiet`, `--branch`, `--backend`, `--contributor`, `--team`, `--stealth`, `--setup-exclude`, `--skip-hooks`, `--skip-merge-driver`, `--force`, `--from-jsonl` | `br` keeps minimal init (prefix + sqlite); hook/merge-driver setup excluded |
| `sync` | `--message`, `--dry-run`, `--no-push`, `--no-pull`, `--rename-on-import`, `--flush-only`, `--import-only`, `--status`, `--merge`, `--from-main`, `--no-git-history`, `--squash`, `--check`, `--accept-rebase`, `--json` | `br` keeps only `--flush-only` / `--import-only` (no git ops) |
| `merge` | `--debug` | Merge-driver helper excluded |
| `resolve-conflicts` | `--mode`, `--dry-run`, `--json`, `--path` | Excluded |
| `worktree create` | `--branch` | Excluded |
| `worktree remove` | `--force` | Excluded |
| `worktree list/info` | (none) | Excluded |
| `repo add/remove/list/sync` | `--json` | Excluded |
| `move` | `--to`, `--keep-open`, `--skip-deps` | Excluded |
| `refile` | `--keep-open` | Excluded |
| `rename-prefix` | `--dry-run`, `--repair` | Excluded |
| `reset` | `--force` | Excluded (destructive) |
| `preflight` | `--check`, `--fix`, `--json` | Excluded (Go-repo specific) |

#### 15.82.3 Maintenance / Diagnostics (Excluded unless explicitly requested)

| Command | Flags | Exclusion Note |
|---|---|---|
| `admin` | (none) | Group for maintenance aliases |
| `cleanup` | `--force`, `--dry-run`, `--cascade`, `--older-than`, `--hard`, `--ephemeral` | Excluded |
| `compact` | `--dry-run`, `--tier`, `--all`, `--id`, `--force`, `--batch-size`, `--workers`, `--stats`, `--json`, `--analyze`, `--apply`, `--auto`, `--prune`, `--older-than`, `--purge-tombstones`, `--summary`, `--actor`, `--limit` | Excluded |
| `migrate` | `--yes`, `--cleanup`, `--dry-run`, `--update-repo-id`, `--inspect`, `--json` | Excluded |
| `migrate hash-ids` | `--dry-run` | Excluded |
| `migrate issues` | `--from`, `--to`, `--status`, `--priority`, `--type`, `--label`, `--id`, `--ids-file`, `--include`, `--within-from-only`, `--dry-run`, `--strict`, `--yes` | Excluded |
| `migrate sync` | `--dry-run`, `--force` | Excluded |
| `migrate tombstones` | `--dry-run`, `--verbose`, `--json` | Excluded |
| `doctor` | `--fix`, `--yes`, `--interactive`, `--dry-run`, `--fix-child-parent`, `--verbose`, `--force`, `--source`, `--perf`, `--check-health`, `--output`, `--check`, `--clean`, `--deep` | Excluded |
| `repair` | `--dry-run`, `--path`, `--json` | Excluded |
| `detect-pollution` | `--clean`, `--yes` | Excluded |
| `duplicates` | `--auto-merge`, `--dry-run` | Excluded |
| `duplicate` | `--of` | Excluded |
| `supersede` | `--with` | Excluded |
| `orphans` | `--fix`, `--details` | Excluded |
| `lint` | `--type`, `--status` | Optional |
| `activity` | `--follow`, `--mol`, `--since`, `--type`, `--limit`, `--interval`, `--town` | Excluded |
| `audit record` | `--kind`, `--model`, `--prompt`, `--response`, `--issue-id`, `--tool-name`, `--exit-code`, `--error`, `--stdin` | Excluded |
| `audit label` | `--label`, `--reason` | Excluded |
| `restore` | `--json` | Excluded |

#### 15.82.4 Gastown / Agent Workflow (Excluded)

| Command | Flags | Exclusion Note |
|---|---|---|
| `agent backfill-labels` | `--dry-run` | Excluded |
| `agent state/heartbeat/show` | (none) | Excluded |
| `gate list` | `--all`, `--limit` | Excluded |
| `gate resolve` | `--reason` | Excluded |
| `gate check` | `--type`, `--dry-run`, `--escalate`, `--limit` | Excluded |
| `gate discover` | `--dry-run`, `--branch`, `--limit`, `--max-age` | Excluded |
| `mol bond` | `--type`, `--as`, `--dry-run`, `--var`, `--ephemeral`, `--pour`, `--ref` | Excluded |
| `mol burn` | `--dry-run`, `--force` | Excluded |
| `mol current` | `--for`, `--limit`, `--range` | Excluded |
| `mol distill` | `--var`, `--dry-run`, `--output` | Excluded |
| `mol show` | `--parallel` | Excluded |
| `mol squash` | `--dry-run`, `--keep-children`, `--summary` | Excluded |
| `mol stale` | `--blocking`, `--unassigned`, `--all`, `--json` | Excluded |
| `mol progress / ready-gated` | (none) | Excluded |
| `swarm validate` | `--verbose` | Excluded |
| `swarm create` | `--coordinator`, `--force` | Excluded |
| `ship` | `--force`, `--dry-run` | Excluded |
| `pour` | `--var`, `--dry-run`, `--assignee`, `--attach`, `--attach-type` | Excluded |
| `cook` | `--dry-run`, `--persist`, `--force`, `--search-path`, `--prefix`, `--var`, `--mode` | Excluded |
| `slot` | (none) | Excluded |
| `merge-slot acquire` | `--holder`, `--wait` | Excluded |
| `merge-slot release` | `--holder` | Excluded |
| `formula list` | `--type` | Excluded |
| `formula convert` | `--all`, `--delete`, `--stdout` | Excluded |
| `template instantiate` | `--var`, `--dry-run`, `--assignee` | Deprecated; exclude |
| `epic status` | `--eligible-only` | Optional |
| `epic close-eligible` | `--dry-run` | Optional |
| `graph` | `--all`, `--compact`, `--box` | Optional |

#### 15.82.5 Integrations & Delegation (Excluded)

| Command | Flags | Exclusion Note |
|---|---|---|
| `linear sync` | `--pull`, `--push`, `--dry-run`, `--prefer-local`, `--prefer-linear`, `--create-only`, `--update-refs`, `--state` | Excluded |
| `jira sync` | `--pull`, `--push`, `--dry-run`, `--prefer-local`, `--prefer-jira`, `--create-only`, `--update-refs`, `--state` | Excluded |
| `mail` | (none; delegates via config/env) | Excluded |
| `setup` | `--list`, `--print`, `--output`, `--add`, `--check`, `--remove`, `--project`, `--stealth` | Excluded |
| `onboard` | (none) | Excluded |
| `prime` | `--full`, `--mcp`, `--stealth`, `--export` | Excluded |

#### 15.82.6 Meta / Convenience (Optional)

| Command | Flags | Exclusion Note |
|---|---|---|
| `info` | `--schema`, `--whats-new`, `--thanks`, `--json` | Optional |
| `version` | `--daemon` | Optional |
| `upgrade` | (none; subcommands `status`, `review`, `ack`) | Optional |
| `thanks` | (none) | Optional |
| `where` | (none) | Optional |
| `human` | (none) | Optional |
| `quick` | `--priority`, `--type`, `--labels` | Optional |
| `quickstart` | (none) | Optional |
| `create-form` | (none) | Optional |
| `edit` | `--title`, `--description`, `--design`, `--notes`, `--acceptance` | Optional |
| `status` | `--all`, `--assigned`, `--no-activity` | Optional |
| `state set` | `--reason` | Optional |

---

### 15.83 Sync Workflow Deep Dive (Git-Heavy, Excluded in br v1)

`bd sync` is a **git-centric**, multi-step pipeline that treats SQLite as the source
of truth but protects against cross-clone data loss with a pull-first 3‑way merge.
`br` v1 should **exclude** all git workflows and keep only `sync --flush-only`
and `sync --import-only` as aliases of export/import.

**Key design choices**:
- **Direct-mode only**: `bd sync` forcibly drops daemon connections to avoid
  stale SQLite file handles (critical when DB file was deleted/recreated).
- **Pull-first**: remote changes are pulled **before** exporting local state
  to avoid overwriting remote JSONL with stale local DB data (GH#911).
- **3‑way merge**: base/local/remote JSONL merge minimizes data loss.
- **Explicit locking**: `.beads/.sync.lock` prevents concurrent sync runs.

**Sync modes and flags**:
- `--flush-only`: export JSONL only (no git ops). Used by hooks and manual flush.
- `--import-only`: import JSONL only; uses **inline import** to avoid redirect path issues.
- `--squash`: export-only without commit/push (accumulate changes into a single later commit).
- `--no-pull`: skip pull/merge; export → commit → (optional) push.
- `--from-main`: one-way sync for **ephemeral branches** (no upstream).
- `--status`: show diff between sync branch and current branch (text only).
- `--merge`: merge sync branch into current branch (text only).
- `--check`: pre-sync integrity scan (JSON-capable; see below).
- `--rename-on-import`: rewrite imported IDs to configured prefix.
- `--accept-rebase`: reserved flag (unused in current flow).

**Preflight checks**:
- Rejects merge/rebase in progress (unmerged paths, MERGE_HEAD).
- Detects **uncommitted JSONL changes** and re-exports DB to reconcile.
- In redirected clones, **skips all git ops** and performs export-only.
- If no upstream and no `sync.branch`, auto-switches to `--from-main` when remote exists.

**Pull-first flow** (default):
1. Load local issues from DB (includes tombstones; excludes wisps).
2. Acquire `.sync.lock`.
3. Load base state from `.beads/sync_base.jsonl` (may be empty on first sync).
4. `git pull` or `syncbranch.PullFromSyncBranch` (if `sync.branch` configured).
5. Load remote JSONL after pull.
6. 3‑way merge (see **15.83.1** below).
7. Write merged JSONL → import merged state to DB → export DB back to JSONL.
8. Commit/push:
   - If `sync.branch` configured: commit/push via worktree.
   - Otherwise: commit `.beads/` only (issues.jsonl, deletions.jsonl, interactions.jsonl, metadata.json).
9. Update base state **after successful push**.
10. Clear sync state markers.

**`--no-pull` flow**:
- Skips steps 3–7; performs export → commit → (optional) push.
- Still runs pre-export integrity checks and template validation.

**`--from-main` flow**:
- Fetches `sync.remote` (default `origin`) and checks out `.beads/` from default branch.
- Imports JSONL (forces `noGitHistory=true` to avoid wrong deletions).
- Intended for ephemeral branches without upstream.

**Redirect + sync-branch incompatibility**:
- If redirected, **sync.branch is ignored** and only export runs.

**`--check` (integrity)** JSON shape:
```json
{
  "forced_push": { "detected": false, "local_ref": "...", "remote_ref": "...", "message": "..." },
  "prefix_mismatch": { "configured_prefix": "bd", "mismatched_ids": ["bd-x", "..."], "count": 2 },
  "orphaned_children": { "orphaned_ids": ["bd-1 (parent: bd-99)"], "count": 1 },
  "has_problems": true
}
```

**Port note**:
- `br` should **not** implement git pulls/pushes, sync branches, or 3‑way JSONL merges.
- Keep only `--flush-only` / `--import-only` as explicit user actions.

#### 15.83.1 3‑Way Merge Rules (sync_merge.go)

The 3‑way merge is **issue‑level**, with field‑level merge on true conflicts:
- **Identity**: by issue ID.
- **Scalar fields**: last‑write‑wins by `updated_at` (remote wins on tie).
- **Labels**: union (dedupe).
- **Dependencies**: union by `(depends_on_id, type)` (keep newer `created_at`).
- **Comments**: append + dedupe (by `id`, else by author+text), sorted by `created_at`.

Deletion handling:
- If local deleted and remote unchanged from base → delete.
- If remote deleted and local unchanged → delete.
- If deletion conflicts with edits → keep edited version (conflict resolved via LWW).

Base state storage: `.beads/sync_base.jsonl` (atomic writes).

---

### 15.84 Import/Export Correctness Audit

This section synthesizes **actual import/export correctness rules** from
`cmd/bd/import.go`, `cmd/bd/sync_export.go`, and `internal/importer`/`internal/export`.

#### 15.84.1 Export Policies and Manifest

**Error policies** (configurable via DB config keys):
- `export.error_policy`: `strict` (default), `best-effort`, `partial`, `required-core`.
- `auto_export.error_policy`: defaults to `best-effort` for auto-flush.
- `export.retry_attempts` (default 3), `export.retry_backoff_ms` (default 100).
- `export.skip_encoding_errors` (default false).
- `export.write_manifest` (default false).

**Policy behaviors**:
- `strict`: fail fast on any error.
- `best-effort`: warn and continue; missing data accepted.
- `partial`: retry then skip; manifest records missing data.
- `required-core`: issues/deps must succeed; labels/comments best‑effort.

**Core vs enrichment data**:
- **Core**: issues + dependencies (export fails if these cannot be read).
- **Enrichment**: labels + comments (may be omitted under `best-effort` / `required-core`).

**Retry/backoff**:
- Export fetches are wrapped in **exponential backoff** using the retry settings.

**Manifest file** (optional):
- Path: `<issues>.manifest.json` (derived by replacing `.jsonl`).
- Written atomically with `0600` permissions.
- Includes:
```json
{
  "exported_count": 123,
  "failed_issues": [{ "issue_id": "bd-abc", "reason": "..." }],
  "partial_data": ["labels", "comments"],
  "warnings": ["..."],
  "complete": false,
  "exported_at": "2026-01-16T00:00:00Z",
  "error_policy": "partial"
}
```

**Encoding errors**:
- If `export.skip_encoding_errors=true`, individual issues that fail JSON
  encoding are **skipped** with warnings and added to `failed_issues`
  (manifest `complete=false`).

#### 15.84.2 Export Correctness Rules (sync_export)

- Includes **tombstones** for sync propagation.
- Excludes **ephemeral/wisp** issues from JSONL.
- Sorted by ID for deterministic diffs.
- Populates dependencies/labels/comments for each issue.
- Atomic write: temp file → rename; permissions `0600`.
- **Safety guard**: refuse to overwrite non‑empty JSONL with empty DB.
- Metadata updates (after successful export):
  - Clear dirty flags for exported IDs.
  - `jsonl_content_hash` updated.
  - `last_import_time` updated.
  - DB mtime touched to be ≥ JSONL mtime.

#### 15.84.3 Import Correctness Rules (internal/importer)

**Core precedence**:
- Updates are **timestamp‑gated**: only apply if incoming `updated_at` is **newer**.
- **Equal timestamps** are treated as **local wins** (skip update).
- `--protect-left-snapshot` adds an extra guard: if local snapshot time is
  **>= incoming** (not strictly newer), the update is skipped (GH#865).

**Matching order**:
1. **external_ref** (if present): updates existing issue by external_ref (timestamp‑gated).
2. **content hash**: exact match → unchanged; same content with different ID:
   - Different prefix → treated as **duplicate**, skipped.
   - Same prefix → treated as rename (legacy path; mostly obsolete with hash IDs).
3. **ID collision**: same ID, different content → update (timestamp‑gated).
4. **New**: create.

**Tombstone safety**:
- If DB has tombstone for ID, incoming is skipped (prevents resurrection).

**Prefix mismatch**:
- Detected across all incoming issues.
- `--rename-on-import` rewrites IDs and **all references** to DB prefix:
  - Fields: `title`, `description`, `design`, `acceptance_criteria`, `notes`.
  - Dependencies: `issue_id`, `depends_on_id`.
  - Comments: text body.
- In multi‑repo mode, prefix validation is skipped.

**Orphan handling** (`import.missing_parents` config or flag):
- `strict` / `resurrect` / `skip` / `allow` (default allow).
- `skip` filters hierarchical children whose parent is missing **before** create.

**Duplicate external_ref handling**:
- Default: error on duplicates.
- `--clear-duplicate-external-refs`: clears dupes (keeps first occurrence).

**Protect‑left‑snapshot**:
- If `--protect-left-snapshot` is set, import protects entries that appear in
  `beads.left.jsonl` **when local timestamp ≥ incoming** (GH#865).

**Post‑import metadata**:
- Updates `jsonl_content_hash`, `jsonl_file_hash`, `last_import_time`.
- Touches DB mtime **even if no changes**, to avoid “JSONL newer” false positives.

#### 15.84.4 Import/Export JSON Output

**Import**:
- Has `--json` flag, but the CLI **does not emit a JSON summary** in normal flow.
  This appears to be a legacy gap; output is stderr text + exit code.

**Export**:
- `export --format jsonl` emits JSONL itself; no separate summary JSON.
- Obsidian format emits Markdown, not JSON.
- **Daemon/RPC export** returns JSON summary:
  - `{ "exported_count": N, "path": "...", "skipped_count": M, "warnings": [...]? }`
- **Daemon/RPC import** is **not implemented** (returns error: use `--no-daemon`).

**Port note**:
- `br` should either implement proper JSON summaries or **omit** the `--json`
  flags for import/export to avoid misleading behavior.

---

### 15.85 Maintenance / Repair Commands (Non‑Classic)

These commands are **git‑heavy** or destructive and should be excluded from `br` v1.
They are documented here for completeness and compatibility reasoning.

#### 15.85.1 `bd cleanup`

Deletes closed issues and prunes tombstones:
- Requires `--force` unless `--dry-run`.
- Supports `--older-than`, `--cascade`, `--hard`, `--ephemeral`.
- Skips **pinned** issues.
- Uses `deleteBatch` → creates tombstones, then prunes expired tombstones.
- `--hard` bypasses 30‑day tombstone TTL:
  - If `--older-than` is set, it becomes the TTL cutoff.
  - If `--older-than` is omitted, tombstones expire **immediately** (negative TTL).
- Emits a **hard‑delete warning** unless JSON mode / dry‑run.

**JSON output**:
- Empty case:
```json
{ "deleted_count": 0, "message": "No closed issues to delete", "filter": "older than 30 days", "ephemeral": true }
```
- Non‑empty case reuses `bd delete` batch output (see 15.58).

#### 15.85.2 `bd compact`

Compaction is **AI‑assisted** or agent‑assisted summarization of closed issues.
Modes:
- `--stats`: DB compaction stats.
- `--prune`: tombstone pruning by age.
- `--purge-tombstones`: dependency‑aware purging.
- `--analyze`: export candidates for review (JSON).
- `--apply`: apply provided summary to a specific issue.
- `--auto`: legacy AI‑powered compaction (requires API key).

**Key behaviors**:
- Modes are **mutually exclusive** (`--prune/--purge-tombstones/--analyze/--apply/--auto`).
- `--analyze` / `--apply` require **direct DB access** (no daemon).
- Tiers:
  - Tier 1: ~30 days closed, target ~70% reduction.
  - Tier 2: ~90 days closed, target ~95% reduction.
- `--apply` writes compaction metadata (`compaction_level`, `compacted_at`, `original_size`,
  `compacted_at_commit`) and replaces long-form fields with the provided summary.
- Auto mode uses Anthropic Haiku; concurrency defaults to 5 workers.

**Port note**:
- Exclude all compaction modes from `br` v1.

#### 15.85.3 `bd repair`

Direct‑SQLite repair for orphaned foreign keys (bypasses beads storage invariants).

**JSON output**:
```json
{
  "database_path": "/abs/.beads/beads.db",
  "dry_run": false,
  "orphan_counts": {
    "dependencies_issue_id": 1,
    "dependencies_depends_on": 2,
    "labels": 0,
    "comments": 0,
    "events": 0,
    "total": 3
  },
  "orphan_details": {
    "dependencies_issue_id": [{ "issue_id": "bd-1", "depends_on_id": "bd-2" }]
  },
  "status": "success",
  "backup_path": "beads.db.pre-repair"
}
```

#### 15.85.4 `bd doctor`

Large health check + auto‑fix framework:
- Scans schema, migrations, JSONL integrity, sync divergence, daemon health,
  git hooks, redirect status, etc.
- `--fix` can regenerate JSONL from DB and repair common issues.
- `--deep` performs graph‑level integrity checks (parents, deps, epics, agents).
- `--perf` emits performance diagnostics.
- `--check=pollution` detects test issues; `--clean` deletes them (with confirmation).
- `--check-health` runs a **silent quick** check (used for hints).
- Recovery flags: `--force` (repair even when DB won’t open),
  `--source=auto|jsonl|db` (choose source of truth),
  `--fix-child-parent` (opt‑in removal of child→parent deps),
  `--dry-run` / `--interactive` / `--yes`.

**Fix actions (selected)**:
- **DB↔JSONL sync**: chooses `bd export` vs `bd sync --import-only` based on
  issue counts + mtime (prevents no‑op syncs).
- **Deletion manifest hydration**: rebuilds deletions from git history while
  **excluding tombstones** (prevents re‑deleting migrated issues).
- **Git housekeeping**: refresh `.beads/.gitignore`, sync‑branch gitignore, hook checks.
- **Data integrity**: orphan dependency cleanup; optional child→parent cleanup.
- **Maintenance**: stale closed issue cleanup + expired tombstone pruning.
- **Permissions**: fix read/write errors on `.beads` files.

**JSON output**:
```json
{
  "path": "/abs/repo",
  "checks": [{ "name": "JSONL Integrity", "status": "error", "message": "...", "fix": "..." }],
  "overall_ok": false,
  "cli_version": "0.47.2",
  "timestamp": "2026-01-16T00:00:00Z",
  "platform": { "os": "...", "arch": "..." }
}
```

**Port note**:
- `br` should keep only **minimal, non‑destructive** integrity checks
  (JSONL validity + schema sanity) if any.

---

### 15.86 Epic Command Details (Pre‑Gastown)

`bd epic` is a **classic** helper for epics/children:
- `bd epic status [--eligible-only]` returns `EpicStatus[]` where each entry includes:
  - `epic` issue object
  - `total_children`, `closed_children`, `eligible_for_close`
- `bd epic close-eligible [--dry-run]` closes all epics where all children are closed.

**JSON output**:
- `epic status` → array of `EpicStatus` objects.
- `epic close-eligible` → `{ "closed": ["bd-..."], "count": N }` (or `[]` in dry‑run).

**Port note**:
- Optional to keep in `br`; easy to re‑implement on top of dependency graph.

---

### 15.87 Excluded Integrations & Automation (JSON Output Shapes)

These commands are **excluded** in `br` v1 but their JSON shapes are captured for
compatibility tests and future parity decisions.

#### 15.87.1 Hooks

- `hooks install` → `{ "success": true, "message": "...", "shared": bool, "chained": bool }`
- `hooks uninstall` → `{ "success": true, "message": "..." }`
- `hooks list` → `{ "hooks": [ { "name": "...", "installed": true, "version": "...", "is_shim": false, "outdated": false } ] }`

#### 15.87.2 Daemon / Daemons

**Status**:
```json
{
  "workspace": "/abs/repo",
  "pid": 12345,
  "version": "0.47.2",
  "status": "healthy|outdated|not_running",
  "uptime_seconds": 123.4,
  "auto_commit": true,
  "auto_push": true,
  "auto_pull": false
}
```

**All status**:
```json
{ "total": 2, "healthy": 1, "outdated": 1, "stale": 0, "unresponsive": 0, "daemons": [ ... ] }
```

**Daemons list**: array of `DaemonInfo` (workspace, pid, version, uptime, last_activity, lock).
**Stop**: `{ "workspace": "...", "pid": 123, "stopped": true }`
**Restart**: `{ "workspace": "...", "action": "restarted" }`
**Logs**: `{ "workspace": "...", "log_path": "...", "content": "..." }`
**Health**: `{ "total": N, "healthy": X, "stale": Y, "mismatched": Z, "daemons": [ ... ] }`

#### 15.87.3 Gate

- `gate list` → array of Issue objects (type `gate`).
- `gate show` → single Issue.
- `gate check` → `{ "checked": N, "resolved": X, "escalated": Y, "errors": Z, "dry_run": bool }`

#### 15.87.4 Agent

- `agent state` → `{ "agent": "gt-xyz", "agent_state": "running", "last_activity": "RFC3339" }`
- `agent heartbeat` → `{ "agent": "gt-xyz", "last_activity": "RFC3339" }`
- `agent show` → `{ "id": "...", "title": "...", "agent_state": "...", "last_activity": "...", "hook_bead": "...", "role_bead": "...", "role_type": "...", "rig": "..." }`

#### 15.87.5 Swarm / Mol / Formula / Wisp / Pour

These are **Gastown‑only** surfaces. They emit JSON objects that mirror internal
`types.Molecule`, `types.Formula`, or batch operation results. Examples:
- `mol show` → `{ "molecule": {...}, "issues": [...], "dependencies": [...] }`
- `mol stale` → `{ "total": N, "stale": [...], "blocking": [...] }`
- `mol squash` → `{ "deleted": [...], "kept": [...], "summary": "..." }`
- `swarm status` → `{ "epic_id": "...", "phase": "...", "agents": [...] }`

#### 15.87.6 Linear / Jira

- `linear sync` and `jira sync` emit `SyncResult` objects:
```json
{ "success": true, "created": 3, "updated": 2, "warnings": ["..."], "error": "" }
```
- `linear status` / `jira status` emit config + counts (external_ref, pending push).
- `linear teams` returns array of team objects (`id`, `name`, etc.).

#### 15.87.7 Mail Delegation

`bd mail` delegates to an external provider; no JSON output of its own.

#### 15.87.8 Repo / Worktree / Upgrade / Setup / Onboard

- `repo add/remove/list` → small JSON maps (`{added:true,path:"..."}`, `{primary:"...",additional:[...]}`).
- `worktree` commands do not emit JSON; they wrap git worktree operations.
- `upgrade` subcommands output JSON maps with status/ack fields when `--json`.
- `setup`/`onboard` are text-only.

---

### 15.88 Config Key Validation (Defaults + Env Binding)

The canonical defaults and env bindings live in `internal/config`:

**Defaults** (non-exhaustive):
- `json=false`, `no-daemon=false`, `no-auto-flush=false`, `no-auto-import=false`
- `no-db=false`, `db=""`, `actor=""`, `issue-prefix=""`
- `flush-debounce=30s`, `auto-start-daemon=true`, `remote-sync-interval=30s`
- `routing.mode=auto`, `routing.default=.`, `routing.maintainer=.`, `routing.contributor=~/.beads-planning`
- `validation.on-create=none`, `validation.on-sync=none`
- `hierarchy.max-depth=3`
- `git.author=""`, `git.no-gpg-sign=false`
- `directory.labels={}`, `external_projects={}`

**Env bindings**:
- `BD_*` or `BEADS_*` with dots/hyphens mapped to underscores.
- Legacy envs: `BEADS_FLUSH_DEBOUNCE`, `BEADS_AUTO_START_DAEMON`,
  `BEADS_IDENTITY`, `BEADS_REMOTE_SYNC_INTERVAL`.

**Port note**:
- `br` should **shrink** config to non‑invasive keys only and keep explicit
  user‑driven sync/flush behavior.

---

### 15.89 Conformance Harness Plan (bd ↔ br)

Goal: automated parity testing for **classic** commands in JSON mode.

**Harness outline**:
1. Seed a fixture repo with deterministic issues + dependencies.
2. Run `bd <cmd> --json` and `br <cmd> --json` with identical inputs.
3. Normalize volatile fields (timestamps, hashes, ordering).
4. Compare JSON output and database schema snapshots.

**Core command coverage**:
- `create`, `update`, `close`, `reopen`, `delete`
- `list`, `show`, `ready`, `blocked`, `search`
- `dep`, `label`, `comments`
- `count`, `stats/status`, `stale`, `orphans`
- `export`, `import` (JSONL round-trip)

**Schema checks**:
- Compare `PRAGMA table_info` and expected indices.
- Validate JSONL output fields + ordering.

---

### 15.90 Create-Form (Interactive TUI)

`bd create-form` launches a TUI (via `charmbracelet/huh`) to gather fields:
title, description, type, priority, assignee, labels, design, acceptance,
external_ref, deps. It then calls the same create path as `bd create`.

**Port note**:
- Optional in `br` v1. If omitted, document as unsupported.

---

## Appendix B: SQLite Connection String

```
file:path/to/beads.db?_pragma=foreign_keys(ON)&_pragma=busy_timeout(30000)&_pragma=journal_mode(WAL)
```

For rusqlite:

```rust
let conn = Connection::open_with_flags(
    path,
    OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
)?;

conn.pragma_update(None, "foreign_keys", "ON")?;
conn.pragma_update(None, "busy_timeout", 30000)?;
conn.pragma_update(None, "journal_mode", "WAL")?;
conn.pragma_update(None, "synchronous", "NORMAL")?;
conn.pragma_update(None, "cache_size", -65536)?; // 64MB
conn.pragma_update(None, "temp_store", "MEMORY")?;
```

---

## Appendix C: JSONL Example

Complete example of `.beads/issues.jsonl`:

```json
{"id":"bd-abc123","title":"Add user authentication","description":"Implement OAuth2 flow for user login","status":"in_progress","priority":1,"issue_type":"feature","assignee":"alice","created_at":"2024-01-10T10:00:00Z","updated_at":"2024-01-15T14:30:00Z","labels":["auth","security"],"dependencies":[{"issue_id":"bd-abc123","depends_on_id":"bd-xyz789","type":"blocks","created_at":"2024-01-10T10:00:00Z"}]}
{"id":"bd-def456","title":"Fix login button styling","description":"Button text is cut off on mobile","status":"open","priority":2,"issue_type":"bug","created_at":"2024-01-14T09:00:00Z","updated_at":"2024-01-14T09:00:00Z","labels":["ui","mobile"]}
{"id":"bd-xyz789","title":"Set up OAuth provider","status":"closed","priority":1,"issue_type":"task","created_at":"2024-01-08T08:00:00Z","updated_at":"2024-01-12T16:00:00Z","closed_at":"2024-01-12T16:00:00Z","close_reason":"Provider configured and tested"}
{"id":"bd-old001","title":"Deprecated feature","status":"tombstone","priority":3,"issue_type":"feature","created_at":"2024-01-01T00:00:00Z","updated_at":"2024-01-05T00:00:00Z","deleted_at":"2024-01-05T00:00:00Z","deleted_by":"admin","delete_reason":"Feature cancelled","original_type":"feature"}
```

---

*Document generated for beads_rust porting project.*
*This is the authoritative specification - consult this instead of Go source files.*
*Last updated: 2026-01-16*
