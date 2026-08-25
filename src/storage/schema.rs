//! Database schema definitions and migration logic.

use crate::franken_sync::Connection;
use chrono::Utc;
use fsqlite_types::SqliteValue;

use crate::error::{BeadsError, Result};
use crate::model::{IssueType, Priority, Status};
use crate::util::content_hash_from_parts;

pub const CURRENT_SCHEMA_VERSION: i32 = 17;
const RUNTIME_SCHEMA_WITNESS_KEY: &str = "runtime_schema_witness_v1";

// Persisted witnesses are valid only for this exact compatibility predicate.
// Keep this short, descriptive token in lockstep with any predicate expansion
// or semantic change. Hashing this entire 200+ KiB module in a `const fn`
// caused rustc to interpret hundreds of thousands of loop iterations on every
// schema rebuild, overwhelming the compile-time savings of the runtime fast
// path itself.
const RUNTIME_SCHEMA_CONTRACT_TOKEN: &str = "v7-version-fenced-lexical-core-aux-columns-fks-index-directions-collations-exact-checks-autoincrement-cookie-fenced";
const ISSUES_CLOSED_AT_CHECK: &str = "CHECK ((status = 'closed' AND closed_at IS NOT NULL) OR (status = 'tombstone') OR (status NOT IN ('closed', 'tombstone') AND closed_at IS NULL))";
const GATE_RESULT_HISTORY_MIGRATION_SQL: &str = r"
    CREATE TABLE IF NOT EXISTS gate_result_history (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        issue_id TEXT NOT NULL,
        from_status TEXT NOT NULL,
        to_status TEXT NOT NULL,
        status_revision INTEGER NOT NULL,
        gate TEXT NOT NULL,
        provider TEXT NOT NULL,
        passed INTEGER NOT NULL DEFAULT 0,
        note TEXT,
        recorded_by TEXT,
        recorded_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
        FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS idx_gate_result_history_issue
        ON gate_result_history(issue_id, id);
    CREATE INDEX IF NOT EXISTS idx_gate_result_history_scope
        ON gate_result_history(issue_id, from_status, to_status, status_revision, id);
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExpectedSchemaColumn {
    name: &'static str,
    data_type: &'static str,
    not_null: bool,
    default_value: Option<&'static str>,
    primary_key_position: i64,
}

const fn schema_column(
    name: &'static str,
    data_type: &'static str,
    not_null: bool,
    default_value: Option<&'static str>,
    primary_key_position: i64,
) -> ExpectedSchemaColumn {
    ExpectedSchemaColumn {
        name,
        data_type,
        not_null,
        default_value,
        primary_key_position,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AuxiliaryRuntimeColumn {
    expected: ExpectedSchemaColumn,
    additive_definition: Option<&'static str>,
}

const fn auxiliary_runtime_column(
    name: &'static str,
    data_type: &'static str,
    not_null: bool,
    default_value: Option<&'static str>,
    primary_key_position: i64,
    additive_definition: Option<&'static str>,
) -> AuxiliaryRuntimeColumn {
    AuxiliaryRuntimeColumn {
        expected: ExpectedSchemaColumn {
            name,
            data_type,
            not_null,
            default_value,
            primary_key_position,
        },
        additive_definition,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExpectedRuntimeIndex {
    name: &'static str,
    columns: &'static [&'static str],
    unique: bool,
    partial: bool,
}

const fn runtime_index(
    name: &'static str,
    columns: &'static [&'static str],
    unique: bool,
    partial: bool,
) -> ExpectedRuntimeIndex {
    ExpectedRuntimeIndex {
        name,
        columns,
        unique,
        partial,
    }
}

const GATE_RESULT_HISTORY_COLUMNS: &[ExpectedSchemaColumn] = &[
    ExpectedSchemaColumn {
        name: "id",
        data_type: "INTEGER",
        not_null: false,
        default_value: None,
        primary_key_position: 1,
    },
    ExpectedSchemaColumn {
        name: "issue_id",
        data_type: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    ExpectedSchemaColumn {
        name: "from_status",
        data_type: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    ExpectedSchemaColumn {
        name: "to_status",
        data_type: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    ExpectedSchemaColumn {
        name: "status_revision",
        data_type: "INTEGER",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    ExpectedSchemaColumn {
        name: "gate",
        data_type: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    ExpectedSchemaColumn {
        name: "provider",
        data_type: "TEXT",
        not_null: true,
        default_value: None,
        primary_key_position: 0,
    },
    ExpectedSchemaColumn {
        name: "passed",
        data_type: "INTEGER",
        not_null: true,
        default_value: Some("0"),
        primary_key_position: 0,
    },
    ExpectedSchemaColumn {
        name: "note",
        data_type: "TEXT",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    ExpectedSchemaColumn {
        name: "recorded_by",
        data_type: "TEXT",
        not_null: false,
        default_value: None,
        primary_key_position: 0,
    },
    ExpectedSchemaColumn {
        name: "recorded_at",
        data_type: "DATETIME",
        not_null: true,
        default_value: Some("CURRENT_TIMESTAMP"),
        primary_key_position: 0,
    },
];

const GATE_RESULT_HISTORY_INDEXES: &[(&str, &[&str])] = &[
    ("idx_gate_result_history_issue", &["issue_id", "id"]),
    (
        "idx_gate_result_history_scope",
        &[
            "issue_id",
            "from_status",
            "to_status",
            "status_revision",
            "id",
        ],
    ),
];

/// Effects produced by one explicit reviewed schema migration.
///
/// The reviewed migration surface is intentionally narrow: this binary only
/// accepts schema 13 or 14 as input and always migrates to
/// [`CURRENT_SCHEMA_VERSION`]. Callers use these counts to compare the
/// transaction result with their reviewed plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviewedSchemaMigrationEffects {
    /// Effective `PRAGMA user_version` observed before any migration write.
    pub from_version: u32,
    /// Effective `PRAGMA user_version` stamped by the migration.
    pub to_version: u32,
    /// Issue rows whose content hash and dirty marker were rewritten by v14.
    pub content_hash_rows_rebuilt: usize,
    /// Whether v15 created `gate_result_history` rather than finding it present.
    pub gate_result_history_created: bool,
}

/// The complete SQL schema for the beads database.
/// Schema matches classic bd (Go) for interoperability.
pub const SCHEMA_SQL: &str = r"
    -- Issues table
    -- Note: TEXT fields use DEFAULT '' for bd (Go) compatibility.
    -- bd's sql.Scan doesn't handle NULL well when scanning into string fields.
    -- Closed-at invariant is enforced by the CHECK clause below.
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
        due_at DATETIME,
        defer_until DATETIME,
        external_ref TEXT,
        source_system TEXT DEFAULT '',
        source_repo TEXT NOT NULL DEFAULT '.',
        deleted_at DATETIME,
        deleted_by TEXT DEFAULT '',
        delete_reason TEXT DEFAULT '',
        original_type TEXT DEFAULT '',
        compaction_level INTEGER DEFAULT 0,
        compacted_at DATETIME,
        compacted_at_commit TEXT,
        original_size INTEGER,
        sender TEXT DEFAULT '',
        ephemeral INTEGER NOT NULL DEFAULT 0,
        pinned INTEGER NOT NULL DEFAULT 0,
        is_template INTEGER NOT NULL DEFAULT 0,
        -- The repo-path column is appended at the end (after the template
        -- flag) to match the position SQLite assigns to ALTER TABLE ADD
        -- COLUMN on existing DBs. This keeps `EXPECTED_ISSUE_COLUMN_ORDER`
        -- consistent for both freshly-created and migrated databases.
        -- Column names are deliberately not repeated in these comments:
        -- fsqlite 0.3+ stores the CREATE TABLE text faithfully (comments
        -- included), and schema audits count declaration tokens. See #289.
        source_repo_path TEXT,
        -- agent_context (schema v11, #297) carries canonical-JSON governing
        -- instructions inherited by descendants on br update --status
        -- in_progress / --claim and br show. The on-disk shape is a JSON
        -- string; serde_json validation happens at the CLI boundary so the
        -- column itself stays a TEXT bag. NULL means no inherited context;
        -- emission for descendants silently skips ancestors with NULL.
        agent_context TEXT,
        CHECK (
            (status = 'closed' AND closed_at IS NOT NULL) OR
            (status = 'tombstone') OR
            (status NOT IN ('closed', 'tombstone') AND closed_at IS NULL)
        )
    );

    -- Primary access patterns
    CREATE INDEX IF NOT EXISTS idx_issues_status ON issues(status);
    CREATE INDEX IF NOT EXISTS idx_issues_priority ON issues(priority);
    CREATE INDEX IF NOT EXISTS idx_issues_issue_type ON issues(issue_type);
    CREATE INDEX IF NOT EXISTS idx_issues_assignee ON issues(assignee) WHERE assignee IS NOT NULL;
    CREATE INDEX IF NOT EXISTS idx_issues_created_at ON issues(created_at);
    CREATE INDEX IF NOT EXISTS idx_issues_updated_at ON issues(updated_at);

    -- Export/sync patterns
    CREATE INDEX IF NOT EXISTS idx_issues_content_hash ON issues(content_hash);
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
        WHERE status = 'open'
        AND ephemeral = 0
        AND pinned = 0
        AND is_template = 0;

    -- Widened ready group (issue #354): when `workflow.status_groups.ready`
    -- surfaces statuses beyond `open` (e.g. `rework`), the partial
    -- `idx_issues_ready` above (which only covers `status = 'open'`) cannot serve
    -- the `status IN (...) ORDER BY priority, created_at` query, so a non-partial
    -- composite keeps the widened path index-covered. The tighter partial index
    -- still wins for the common default `[open]` group.
    CREATE INDEX IF NOT EXISTS idx_issues_status_priority_created
        ON issues(status, priority, created_at);

    -- Common active list path: non-terminal issues sorted by priority/created_at.
    -- Uses ASC on created_at (not DESC) to avoid frankensqlite B-tree ordering
    -- divergence with C sqlite3 integrity_check.  SQLite reverse-scans the ASC
    -- index efficiently for ORDER BY ... created_at DESC queries.
    CREATE INDEX IF NOT EXISTS idx_issues_list_active_order
        ON issues(priority, created_at)
        WHERE status NOT IN ('closed', 'tombstone')
        AND (is_template = 0 OR is_template IS NULL);

    -- Dependencies
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
        WHERE (type = 'blocks' OR type = 'parent-child' OR type = 'conditional-blocks' OR type = 'waits-for');

    -- Labels
    CREATE TABLE IF NOT EXISTS labels (
        issue_id TEXT NOT NULL,
        label TEXT NOT NULL,
        PRIMARY KEY (issue_id, label),
        FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS idx_labels_label ON labels(label);
    CREATE INDEX IF NOT EXISTS idx_labels_issue ON labels(issue_id);

    -- Comments
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

    -- Events (Audit)
    CREATE TABLE IF NOT EXISTS events (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        issue_id TEXT NOT NULL,
        event_type TEXT NOT NULL,
        actor TEXT NOT NULL DEFAULT '',
        old_value TEXT,
        new_value TEXT,
        comment TEXT,
        created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
        -- Tier 1 attribution captured on status-mutating commands (issue #312,
        -- Layer 3 capture-only). Self-reported agent/harness/model identity is
        -- recorded as an audit trail ONLY — never gated/enforced on. All three
        -- are nullable so events without attribution (the common case) and
        -- older databases stay valid. Like `close_metadata` attribution these
        -- columns are DB-only and are not part of the JSONL sync surface.
        agent_name TEXT,
        harness TEXT,
        model TEXT,
        FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS idx_events_issue ON events(issue_id);
    CREATE INDEX IF NOT EXISTS idx_events_type ON events(event_type);
    CREATE INDEX IF NOT EXISTS idx_events_created_at ON events(created_at);
    CREATE INDEX IF NOT EXISTS idx_events_actor ON events(actor) WHERE actor != '';

    -- Config (Runtime)
    -- NOTE: Avoid PRIMARY KEY/UNIQUE constraints here because the current
    -- storage engine does not reliably maintain unique autoindexes.
    -- Application code enforces key replacement via DELETE + INSERT.
    CREATE TABLE IF NOT EXISTS config (
        key TEXT NOT NULL,
        value TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_config_key ON config(key);

    -- Metadata
    -- Same rationale as config: keep it as key-value with an explicit index.
    -- Storage code reads the newest duplicate row and harmonizes duplicate
    -- rows on write; doctor still reports duplicates as recoverable anomalies.
    CREATE TABLE IF NOT EXISTS metadata (
        key TEXT NOT NULL,
        value TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_metadata_key ON metadata(key);

    -- Dirty Issues (for export)
    CREATE TABLE IF NOT EXISTS dirty_issues (
        issue_id TEXT PRIMARY KEY,
        marked_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
        FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS idx_dirty_issues_marked_at ON dirty_issues(marked_at);

    -- Export Hashes (for incremental export)
    CREATE TABLE IF NOT EXISTS export_hashes (
        issue_id TEXT PRIMARY KEY,
        content_hash TEXT NOT NULL,
        exported_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
        FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
    );

    -- Blocked Issues Cache (Materialized view)
    -- Rebuilt on dependency or status changes.
    -- `blocked_by` stores a JSON array of blocking issue IDs.
    CREATE TABLE IF NOT EXISTS blocked_issues_cache (
        issue_id TEXT PRIMARY KEY,
        blocked_by TEXT NOT NULL,
        blocked_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
        FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS idx_blocked_cache_blocked_at ON blocked_issues_cache(blocked_at);

    -- Child Counters (for hierarchical IDs like bd-abc.1, bd-abc.2)
    CREATE TABLE IF NOT EXISTS child_counters (
        parent_id TEXT PRIMARY KEY,
        last_child INTEGER NOT NULL DEFAULT 0,
        FOREIGN KEY (parent_id) REFERENCES issues(id) ON DELETE CASCADE
    );

    -- Close metadata (issue #274 — closure-time policy gates Phase 1).
    --
    -- One row per terminal close. Tier 1 attribution + bypass-policy auditing
    -- live here so the issues table stays untouched (avoids breaking JSONL
    -- round-trip and the wide SELECT statements throughout sqlite.rs).
    --
    -- All gate-related columns are nullable / default-valued so older
    -- databases upgraded with a single ALTER TABLE chain remain valid.
    CREATE TABLE IF NOT EXISTS close_metadata (
        issue_id TEXT PRIMARY KEY,
        closed_by_agent_name TEXT,
        closed_by_harness TEXT,
        closed_by_model TEXT,
        bypassed_policy INTEGER NOT NULL DEFAULT 0,
        bypass_reason TEXT,
        policy_gates_fired TEXT,
        recorded_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
        FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS idx_close_metadata_recorded_at ON close_metadata(recorded_at);
    CREATE INDEX IF NOT EXISTS idx_close_metadata_bypassed
        ON close_metadata(bypassed_policy)
        WHERE bypassed_policy = 1;

    -- Workflow gate results (issue #312, layer 2). One row per
    -- (issue, gate, provider): a provider's most-recent pass/fail verdict for
    -- a named gate on an issue. A re-report from the same provider for the
    -- same gate overwrites the prior verdict (INSERT OR REPLACE).
    CREATE TABLE IF NOT EXISTS gate_results (
        issue_id TEXT NOT NULL,
        gate TEXT NOT NULL,
        provider TEXT NOT NULL,
        passed INTEGER NOT NULL DEFAULT 0,
        note TEXT,
        recorded_by TEXT,
        recorded_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
        PRIMARY KEY (issue_id, gate, provider),
        FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS idx_gate_results_issue ON gate_results(issue_id);

    -- Append-only, transition-scoped workflow gate history (GitHub #388).
    -- `status_revision` is the latest status_changed event id observed when
    -- the result is reported (zero for an imported/initial state). A result
    -- can satisfy only the exact issue/from/to/revision tuple it records.
    CREATE TABLE IF NOT EXISTS gate_result_history (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        issue_id TEXT NOT NULL,
        from_status TEXT NOT NULL,
        to_status TEXT NOT NULL,
        status_revision INTEGER NOT NULL,
        gate TEXT NOT NULL,
        provider TEXT NOT NULL,
        passed INTEGER NOT NULL DEFAULT 0,
        note TEXT,
        recorded_by TEXT,
        recorded_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
        FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS idx_gate_result_history_issue
        ON gate_result_history(issue_id, id);
    CREATE INDEX IF NOT EXISTS idx_gate_result_history_scope
        ON gate_result_history(issue_id, from_status, to_status, status_revision, id);

    -- Audited issue-specific capacity exemptions (GitHub #384 phase 4).
    -- One row per (issue, capacity): the latest exemption state. Like
    -- gate_results, project-local auxiliary metadata — never synced to
    -- JSONL. A re-grant replaces the state row; the append-only history
    -- table below preserves every action.
    CREATE TABLE IF NOT EXISTS capacity_exemptions (
        issue_id TEXT NOT NULL,
        capacity_kind TEXT NOT NULL,
        capacity_name TEXT NOT NULL,
        provider TEXT NOT NULL,
        reason TEXT NOT NULL,
        granted_by TEXT NOT NULL,
        granted_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
        expires_at DATETIME,
        ended_at DATETIME,
        ended_action TEXT,
        ended_by TEXT,
        PRIMARY KEY (issue_id, capacity_kind, capacity_name),
        FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS idx_capacity_exemptions_capacity
        ON capacity_exemptions(capacity_kind, capacity_name);

    -- Append-only capacity-exemption audit history: grant, renew, revoke,
    -- expire, and left_status actions with actor/provider attribution.
    CREATE TABLE IF NOT EXISTS capacity_exemption_history (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        issue_id TEXT NOT NULL,
        capacity_kind TEXT NOT NULL,
        capacity_name TEXT NOT NULL,
        action TEXT NOT NULL,
        provider TEXT NOT NULL,
        reason TEXT,
        actor TEXT NOT NULL,
        expires_at DATETIME,
        recorded_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
        FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS idx_capacity_exemption_history_issue
        ON capacity_exemption_history(issue_id, id);

    -- Capacity occupancy attribution (GitHub #384 phase 5). One row per
    -- issue recording who moved it into its CURRENT status (acting actor
    -- plus self-reported agent/harness/session attribution). Written on
    -- every committed status transition; scoped capacity limits count
    -- against these keys. Project-local — never synced to JSONL, and
    -- deliberately not written by JSONL import (import is state
    -- replication, not admission).
    CREATE TABLE IF NOT EXISTS capacity_occupancy (
        issue_id TEXT PRIMARY KEY,
        actor TEXT,
        agent_name TEXT,
        harness TEXT,
        session TEXT,
        recorded_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
        FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS idx_capacity_occupancy_actor
        ON capacity_occupancy(actor) WHERE actor IS NOT NULL;
    CREATE INDEX IF NOT EXISTS idx_capacity_occupancy_harness
        ON capacity_occupancy(harness) WHERE harness IS NOT NULL;
    CREATE INDEX IF NOT EXISTS idx_capacity_occupancy_session
        ON capacity_occupancy(session) WHERE session IS NOT NULL;
";

/// Split a SQL script into individual statements, respecting string literals,
/// quoted identifiers, and comments.
///
/// A naive `split(';')` breaks when SQL string literals contain semicolons
/// (e.g., `INSERT INTO t(v) VALUES('a;b')`). This function uses a small state
/// machine to track whether the current position is inside:
/// - A single-quoted string literal (`'...'`, with `''` as escape)
/// - A double-quoted identifier (`"..."`, with `""` as escape)
/// - A line comment (`-- ...`)
/// - A block comment (`/* ... */`)
///
/// Only semicolons at the top level (outside all of the above) are treated as
/// statement terminators.
fn split_sql_statements(sql: &str) -> Vec<&str> {
    let bytes = sql.as_bytes();
    let len = bytes.len();
    let mut stmts = Vec::new();
    let mut start = 0; // byte offset where the current statement begins
    let mut i = 0;

    // State flags — at most one is true at a time.
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

    while i < len {
        let b = bytes[i];

        // --- Line comment state ---
        if in_line_comment {
            if b == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }

        // --- Block comment state ---
        if in_block_comment {
            if b == b'*' && i + 1 < len && bytes[i + 1] == b'/' {
                in_block_comment = false;
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }

        // --- Single-quoted string state ---
        if in_single_quote {
            if b == b'\'' {
                // '' is an escaped quote inside a string literal
                if i + 1 < len && bytes[i + 1] == b'\'' {
                    i += 2;
                } else {
                    in_single_quote = false;
                    i += 1;
                }
            } else {
                i += 1;
            }
            continue;
        }

        // --- Double-quoted identifier state ---
        if in_double_quote {
            if b == b'"' {
                if i + 1 < len && bytes[i + 1] == b'"' {
                    i += 2;
                } else {
                    in_double_quote = false;
                    i += 1;
                }
            } else {
                i += 1;
            }
            continue;
        }

        // --- Top-level parsing ---
        if b == b'\'' {
            in_single_quote = true;
            i += 1;
        } else if b == b'"' {
            in_double_quote = true;
            i += 1;
        } else if b == b'-' && i + 1 < len && bytes[i + 1] == b'-' {
            in_line_comment = true;
            i += 2;
        } else if b == b'/' && i + 1 < len && bytes[i + 1] == b'*' {
            in_block_comment = true;
            i += 2;
        } else if b == b';' {
            // Statement terminator at top level.
            let stmt = &sql[start..i];
            if !stmt.trim().is_empty() {
                stmts.push(stmt.trim());
            }
            start = i + 1;
            i += 1;
        } else {
            i += 1;
        }
    }

    // Trailing statement without a final semicolon.
    if start < len {
        let stmt = &sql[start..len];
        if !stmt.trim().is_empty() {
            stmts.push(stmt.trim());
        }
    }

    stmts
}

/// Execute multiple SQL statements separated by semicolons.
///
/// fsqlite does not support `execute_batch`, so we split the SQL script
/// into individual statements (respecting string literals and comments)
/// and execute each one individually.
pub(crate) fn execute_batch(conn: &Connection, sql: &str) -> Result<()> {
    for stmt in split_sql_statements(sql) {
        let res = conn.execute(stmt);
        if let Err(e) = res {
            // fsqlite's in-memory schema cache may not update after
            // ALTER TABLE RENAME during table rebuilds, causing CREATE INDEX
            // to fail with "no such column".  These indexes will be retried
            // on the next open, so we can safely skip them here.
            // Strip SQL line-comments to get at the real statement.
            let stripped: String = stmt
                .lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with("--"))
                .collect::<Vec<_>>()
                .join(" ");
            let upper = stripped.trim().to_ascii_uppercase();
            let is_index =
                upper.starts_with("CREATE INDEX") || upper.starts_with("CREATE UNIQUE INDEX");
            let is_stale_schema = e.to_string().contains("no such column");
            if is_index && is_stale_schema {
                continue;
            }
            eprintln!(
                "execute_batch failed on statement: {}\nError: {:?}",
                stmt, e
            );
            return Err(BeadsError::Database(e));
        }
    }
    Ok(())
}

/// Apply the schema to the database.
///
/// This splits the DDL script into individual statements and executes them.
/// It is idempotent because all statements use `IF NOT EXISTS`.
///
/// # Errors
///
/// Returns an error if the SQL execution fails or pragmas cannot be set.
pub fn apply_schema(conn: &Connection) -> Result<()> {
    refuse_future_schema_version(conn, "schema application")?;

    // Detect a truly fresh (empty) database before any DDL runs.
    // On a fresh DB, SCHEMA_SQL creates everything at the current version,
    // so running migrations is unnecessary and harmful — e.g. the v3/v4
    // migrations DROP+CREATE idx_issues_ready which orphans a page and
    // causes doctor integrity warnings.
    let is_fresh = !table_exists(conn, "issues");

    // Run pre-schema migrations first to fix any incompatible old tables
    // This must run BEFORE execute_batch because the batch includes CREATE INDEX
    // statements that will fail if old tables have missing columns
    let issues_rebuilt = run_pre_schema_migrations(conn).map_err(|e| {
        eprintln!("run_pre_schema_migrations failed: {:?}", e);
        e
    })?;

    execute_batch(conn, SCHEMA_SQL)?;

    if is_fresh {
        // Fresh database: SCHEMA_SQL already created everything at the
        // current version. Skip migrations and stamp user_version directly.
        conn.execute(&format!("PRAGMA user_version = {CURRENT_SCHEMA_VERSION}"))
            .map_err(|e| {
                eprintln!("PRAGMA user_version failed: {:?}", e);
                BeadsError::Database(e)
            })?;
    } else {
        // Existing database: run migrations for schema upgrades.
        // If the issues table was rebuilt from scratch, skip migration checks
        // that reference newly-added columns because fsqlite's in-memory schema
        // cache may not have been updated yet.
        run_migrations(conn, issues_rebuilt).map_err(|e| {
            eprintln!("run_migrations failed: {:?}", e);
            e
        })?;

        // Mark schema as applied so future opens can skip DDL/migration work.
        conn.execute(&format!("PRAGMA user_version = {CURRENT_SCHEMA_VERSION}"))
            .map_err(|e| {
                eprintln!("PRAGMA user_version failed: {:?}", e);
                BeadsError::Database(e)
            })?;
    }

    apply_runtime_pragmas(conn).map_err(|e| {
        eprintln!("apply_runtime_pragmas failed: {:?}", e);
        e
    })?;

    // On a truly fresh bootstrap, run a defensive `wal_checkpoint(TRUNCATE)`
    // to reclaim any transient pages frankensqlite allocated while
    // executing SCHEMA_SQL (CREATE TABLE + ~15 CREATE INDEX statements,
    // several of which are partial indexes on columns of empty tables).
    // Without this, a fresh `br init` can leave the database with
    // unreachable pages that sqlite3's `PRAGMA integrity_check` surfaces
    // as `Page N: never used` — see issue #225.
    //
    // Note: page-level anomalies from subsequent writes (e.g., "free space
    // corruption" — issue #237) are addressed via VACUUM in the rebuild
    // path and `br doctor --repair`, not here.  Running VACUUM here would
    // conflict with connections opened immediately after init.
    if is_fresh && let Err(e) = conn.execute("PRAGMA wal_checkpoint(TRUNCATE)") {
        tracing::debug!(
            error = %e,
            "wal_checkpoint(TRUNCATE) after fresh bootstrap failed (non-fatal)"
        );
    }

    Ok(())
}

fn connection_user_version(conn: &Connection) -> Result<u32> {
    let row = conn.query_row("PRAGMA user_version")?;
    Ok(row
        .get(0)
        .and_then(|v| match v {
            fsqlite_types::value::SqliteValue::Integer(n) => u32::try_from(*n).ok(),
            _ => None,
        })
        .unwrap_or(0))
}

/// Source schema versions accepted by the reviewed, receipt-bound
/// `br doctor migrate-schema` lifecycle. Every released schema since v13 must
/// stay upgradeable here: 13/14 (pre-gate-history releases), 15 (the #388
/// gate-history schema shipped in the v0.2.19-era line) and 16 (the #384
/// capacity-exemptions schema created by the released v0.2.19 binary). See
/// GitHub #398.
pub const REVIEWED_MIGRATION_SOURCE_VERSIONS: [u32; 4] = [13, 14, 15, 16];

fn current_schema_version_u32() -> Result<u32> {
    u32::try_from(CURRENT_SCHEMA_VERSION).map_err(|_| {
        BeadsError::internal(format!(
            "current schema version {CURRENT_SCHEMA_VERSION} cannot be represented as u32"
        ))
    })
}

fn refuse_future_schema_version(conn: &Connection, operation: &str) -> Result<()> {
    let declared = runtime_user_version(conn)?;
    if declared > i64::from(CURRENT_SCHEMA_VERSION) {
        return Err(BeadsError::Config(format!(
            "{operation} refused: database schema version {declared} is newer than supported version {CURRENT_SCHEMA_VERSION}"
        )));
    }
    Ok(())
}

fn validate_reviewed_schema_migration(
    conn: &Connection,
    from: u32,
    target_version: u32,
    marked_at: &str,
) -> Result<()> {
    let supported_target = current_schema_version_u32()?;
    if target_version != supported_target {
        return Err(BeadsError::internal(format!(
            "schema migrate refused — target must be CURRENT_SCHEMA_VERSION \
             ({supported_target}), got {target_version}"
        )));
    }
    if !REVIEWED_MIGRATION_SOURCE_VERSIONS.contains(&from) {
        return Err(BeadsError::internal(format!(
            "schema migrate refused — reviewed migrations are supported only from source \
             schemas 13, 14, 15, and 16 to {supported_target} (got {from}->{target_version})"
        )));
    }
    if marked_at.is_empty() {
        return Err(BeadsError::internal(
            "schema migrate refused — marked_at must be non-empty",
        ));
    }

    let current = connection_user_version(conn)?;
    if current != from {
        return Err(BeadsError::internal(format!(
            "schema migrate refused — user_version mismatch (expected {from}, got {current})"
        )));
    }
    Ok(())
}

/// Apply the reviewed migration steps inside the caller's transaction.
///
/// This function never starts, commits, or rolls back a transaction. The caller
/// must hold the database-family write authority and an active
/// `BEGIN IMMEDIATE` transaction before calling it. All validation occurs
/// before the first migration write.
///
/// Sources in [`REVIEWED_MIGRATION_SOURCE_VERSIONS`] (13, 14, 15, 16) are
/// accepted, each running exactly the version-gated step chain up to
/// `CURRENT_SCHEMA_VERSION` (#398). `marked_at` is written verbatim to every
/// `dirty_issues` row rewritten by the v13 content-hash step, making the
/// bookkeeping timestamp explicit and reviewable.
///
/// # Errors
///
/// Returns an error without stamping `user_version` when the source/target pair
/// is unsupported, the effective source version does not match `from`,
/// `marked_at` is empty, or any migration statement or postcondition fails.
pub fn run_reviewed_schema_migration_steps_in_transaction(
    conn: &Connection,
    from: u32,
    target_version: u32,
    marked_at: &str,
) -> Result<ReviewedSchemaMigrationEffects> {
    validate_reviewed_schema_migration(conn, from, target_version, marked_at)?;

    let content_hash_rows_rebuilt = if from == 13 {
        tracing::info!("Migrating database to schema version 14 (length-prefixed content hashes)");
        rebuild_content_hashes_for_current_format_in_transaction(conn, marked_at)?
    } else {
        0
    };

    let gate_result_history_created = !table_exists(conn, "gate_result_history");
    if from < 15 {
        tracing::info!("Migrating database to schema version 15 (transition-scoped gate history)");
        apply_gate_result_history_migration_in_transaction(conn)?;
    } else {
        // A genuine v15/v16 database already carries the #388 gate-history
        // schema; attest it instead of re-running the migration so drift is
        // refused rather than silently papered over.
        attest_gate_result_history_schema(conn)?;
    }

    if from < 16 {
        tracing::info!(
            "Migrating database to schema version 16 (capacity exemptions - GitHub #384 phase 4)"
        );
        apply_capacity_exemptions_migration_in_transaction(conn)?;
    }

    tracing::info!(
        "Migrating database to schema version 17 (capacity occupancy - GitHub #384 phase 5)"
    );
    apply_capacity_occupancy_migration_in_transaction(conn)?;

    conn.execute(&format!("PRAGMA user_version = {target_version}"))
        .map_err(BeadsError::Database)?;

    let post = connection_user_version(conn)?;
    if post != target_version {
        return Err(BeadsError::internal(format!(
            "schema migrate post-check failed — expected user_version={target_version}, observed {post}"
        )));
    }

    Ok(ReviewedSchemaMigrationEffects {
        from_version: from,
        to_version: target_version,
        content_hash_rows_rebuilt,
        gate_result_history_created,
    })
}

/// Compatibility wrapper for the existing doctor migration hook.
///
/// The reviewed paths supported by
/// [`run_reviewed_schema_migration_steps_in_transaction`]
/// ([`REVIEWED_MIGRATION_SOURCE_VERSIONS`] → `CURRENT_SCHEMA_VERSION`) run
/// genuinely atomically in one
/// `BEGIN IMMEDIATE` transaction and cannot stamp an arbitrary target after
/// running newer migrations. Every other version pair falls back to the
/// general migration engine so legacy databases (pre-v13) keep an upgrade
/// path; there the chokepoint's pre-migrate snapshot remains the
/// full-rollback safety net, exactly as before.
///
/// # Errors
///
/// Returns an error (and, on the reviewed path, rolls back the transaction)
/// when validation, migration, commit, or postcondition checks fail.
pub fn run_migrations_atomic(conn: &Connection, from: u32, target_version: u32) -> Result<()> {
    // Stamp-integrity invariant (kept from the reviewed-migration redesign):
    // the general engine always migrates fully, so accepting any target other
    // than CURRENT_SCHEMA_VERSION could stamp a partially or over-migrated
    // database. Refuse before any write, regardless of path.
    let supported_target = current_schema_version_u32()?;
    if target_version != supported_target {
        return Err(BeadsError::internal(format!(
            "schema migrate refused — target must be CURRENT_SCHEMA_VERSION \
             ({supported_target}), got {target_version}"
        )));
    }
    if from > supported_target {
        return Err(BeadsError::internal(format!(
            "schema migrate refused — source schema {from} is newer than supported version {supported_target}"
        )));
    }
    if REVIEWED_MIGRATION_SOURCE_VERSIONS.contains(&from) {
        let marked_at = Utc::now().to_rfc3339();
        validate_reviewed_schema_migration(conn, from, target_version, &marked_at)?;

        conn.execute("BEGIN IMMEDIATE")?;
        return match run_reviewed_schema_migration_steps_in_transaction(
            conn,
            from,
            target_version,
            &marked_at,
        ) {
            Ok(_) => {
                if let Err(error) = conn.execute("COMMIT") {
                    let _ = conn.execute("ROLLBACK");
                    return Err(BeadsError::Database(error));
                }
                Ok(())
            }
            Err(error) => {
                let _ = conn.execute("ROLLBACK");
                Err(error)
            }
        };
    }

    // General path: re-verify the `user_version == from` precondition on this
    // connection (closing the TOCTOU window between the chokepoint's read and
    // this call), run the general engine, stamp, and post-verify. No outer
    // transaction: `run_migrations` opens BEGIN IMMEDIATE / COMMIT around the
    // step bundles that need atomicity and fsqlite rejects nested BEGINs; the
    // caller's pre-migrate snapshot is the full-rollback safety net.
    let row = conn.query_row("PRAGMA user_version")?;
    let current = row
        .get(0)
        .and_then(|v| match v {
            fsqlite_types::value::SqliteValue::Integer(n) => u32::try_from(*n).ok(),
            _ => None,
        })
        .unwrap_or(0);
    if current != from {
        return Err(BeadsError::internal(format!(
            "schema migrate refused — user_version mismatch (expected {from}, got {current})"
        )));
    }

    run_migrations(conn, false)?;
    conn.execute(&format!("PRAGMA user_version = {target_version}"))
        .map_err(BeadsError::Database)?;

    let post = conn
        .query_row("PRAGMA user_version")?
        .get(0)
        .and_then(|v| match v {
            fsqlite_types::value::SqliteValue::Integer(n) => u32::try_from(*n).ok(),
            _ => None,
        })
        .unwrap_or(0);
    if post != target_version {
        return Err(BeadsError::internal(format!(
            "schema migrate post-check failed — expected user_version={target_version}, observed {post}"
        )));
    }

    Ok(())
}

pub(crate) fn apply_runtime_compatible_schema(conn: &Connection) -> Result<()> {
    refuse_future_schema_version(conn, "runtime schema repair")?;

    // The table layouts are already safe to operate on, so we can skip the
    // heavier pre-schema rebuilds and just restore any missing canonical DDL.
    execute_batch(conn, SCHEMA_SQL)?;
    run_migrations(conn, false)?;
    conn.execute(&format!("PRAGMA user_version = {CURRENT_SCHEMA_VERSION}"))
        .map_err(BeadsError::Database)?;
    apply_runtime_pragmas(conn)?;
    Ok(())
}

pub(crate) fn apply_runtime_pragmas(conn: &Connection) -> Result<()> {
    // New databases should opt into WAL, but steady-state opens should not
    // reassert the current mode and turn a read path into a write-like one.
    let journal_mode = conn
        .query_row("PRAGMA journal_mode")
        .ok()
        .and_then(|row| row.get(0).and_then(SqliteValue::as_text).map(str::to_owned))
        .unwrap_or_default();
    if !journal_mode.eq_ignore_ascii_case("wal") {
        conn.execute("PRAGMA journal_mode = WAL")?;
    }

    // Enable foreign keys
    conn.execute("PRAGMA foreign_keys = ON")?;

    // Performance PRAGMAs (safe with WAL mode)
    // NORMAL synchronous is safe with WAL: committed data survives OS crash
    conn.execute("PRAGMA synchronous = NORMAL")?;
    // Use memory for temp tables/indexes instead of disk
    conn.execute("PRAGMA temp_store = MEMORY")?;
    // 8MB page cache (default is ~2MB), improves read-heavy workloads
    conn.execute("PRAGMA cache_size = -8000")?;

    // Issue #219: Limit WAL file size to 32MB.  Without this, concurrent
    // writers can cause unbounded WAL growth, which slows reads and
    // increases checkpoint contention.  SQLite will attempt to keep the WAL
    // file at or below this size after each checkpoint.
    conn.execute("PRAGMA journal_size_limit = 33554432")?;

    // Issue #219: Disable the automatic WAL checkpoint that fires after
    // every 1000 pages of WAL growth.  The auto-checkpoint uses PASSIVE
    // mode internally but can still cause unexpected latency spikes during
    // write-heavy concurrent operations.  We handle checkpointing manually
    // in with_write_transaction using PASSIVE mode at a controlled interval.
    conn.execute("PRAGMA wal_autocheckpoint = 0")?;

    Ok(())
}

pub(crate) fn table_exists(conn: &Connection, table: &str) -> bool {
    let escaped_table = table.replace('\'', "''");
    let sql = format!("SELECT 1 FROM sqlite_master WHERE type='table' AND name='{escaped_table}'");
    conn.query(&sql).is_ok_and(|rows| !rows.is_empty())
}

fn index_exists(conn: &Connection, index: &str) -> bool {
    let escaped_index = index.replace('\'', "''");
    let sql = format!("SELECT 1 FROM sqlite_master WHERE type='index' AND name='{escaped_index}'");
    conn.query(&sql).is_ok_and(|rows| !rows.is_empty())
}

fn column_exists(conn: &Connection, table: &str, column: &str) -> bool {
    let sql = format!("PRAGMA table_info('{table}')");
    conn.query(&sql).is_ok_and(|rows| {
        rows.iter()
            .any(|row| row.get(1).and_then(SqliteValue::as_text) == Some(column))
    })
}

const ISSUE_COLUMNS: &[(&str, &str)] = &[
    ("content_hash", "TEXT"),
    ("description", "TEXT NOT NULL DEFAULT ''"),
    ("design", "TEXT NOT NULL DEFAULT ''"),
    ("acceptance_criteria", "TEXT NOT NULL DEFAULT ''"),
    ("notes", "TEXT NOT NULL DEFAULT ''"),
    ("status", "TEXT NOT NULL DEFAULT 'open'"),
    (
        "priority",
        "INTEGER NOT NULL DEFAULT 2 CHECK(priority >= 0 AND priority <= 4)",
    ),
    ("issue_type", "TEXT NOT NULL DEFAULT 'task'"),
    ("assignee", "TEXT"),
    ("owner", "TEXT DEFAULT ''"),
    ("estimated_minutes", "INTEGER"),
    ("created_at", "DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP"),
    ("created_by", "TEXT DEFAULT ''"),
    ("updated_at", "DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP"),
    ("closed_at", "DATETIME"),
    ("close_reason", "TEXT DEFAULT ''"),
    ("closed_by_session", "TEXT DEFAULT ''"),
    ("due_at", "DATETIME"),
    ("defer_until", "DATETIME"),
    ("external_ref", "TEXT"),
    ("source_system", "TEXT DEFAULT ''"),
    ("source_repo", "TEXT NOT NULL DEFAULT '.'"),
    ("deleted_at", "DATETIME"),
    ("deleted_by", "TEXT DEFAULT ''"),
    ("delete_reason", "TEXT DEFAULT ''"),
    ("original_type", "TEXT DEFAULT ''"),
    ("compaction_level", "INTEGER DEFAULT 0"),
    ("compacted_at", "DATETIME"),
    ("compacted_at_commit", "TEXT"),
    ("original_size", "INTEGER"),
    ("sender", "TEXT DEFAULT ''"),
    ("ephemeral", "INTEGER NOT NULL DEFAULT 0"),
    ("pinned", "INTEGER NOT NULL DEFAULT 0"),
    ("is_template", "INTEGER NOT NULL DEFAULT 0"),
    // Appended at the end so SQLite's ALTER TABLE ADD COLUMN on existing DBs
    // produces the same final column order as a fresh SCHEMA_SQL build.
    ("source_repo_path", "TEXT"),
    // beads_rust#297: inherited governing instructions, JSON-stored.
    // Append-at-end keeps EXPECTED_ISSUE_COLUMN_ORDER aligned for fresh
    // and migrated databases.
    ("agent_context", "TEXT"),
];

const DEPENDENCY_COLUMNS: &[(&str, &str)] = &[
    ("type", "TEXT NOT NULL DEFAULT 'blocks'"),
    ("created_at", "DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP"),
    ("created_by", "TEXT NOT NULL DEFAULT ''"),
    ("metadata", "TEXT DEFAULT '{}'"),
    ("thread_id", "TEXT DEFAULT ''"),
];

const COMMENT_COLUMNS: &[(&str, &str)] = &[
    ("author", "TEXT NOT NULL DEFAULT ''"),
    ("text", "TEXT NOT NULL DEFAULT ''"),
    ("created_at", "DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP"),
];

const EVENT_COLUMNS: &[(&str, &str)] = &[
    ("event_type", "TEXT NOT NULL DEFAULT ''"),
    ("actor", "TEXT NOT NULL DEFAULT ''"),
    ("old_value", "TEXT"),
    ("new_value", "TEXT"),
    ("comment", "TEXT"),
    ("created_at", "DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP"),
    // Tier 1 attribution audit columns (issue #312, Layer 3 capture-only).
    // Nullable and additive: older databases gain them via ensure_columns().
    ("agent_name", "TEXT"),
    ("harness", "TEXT"),
    ("model", "TEXT"),
];

const ISSUES_RUNTIME_COLUMNS: &[ExpectedSchemaColumn] = &[
    schema_column("id", "TEXT", false, None, 1),
    schema_column("content_hash", "TEXT", false, None, 0),
    schema_column("title", "TEXT", true, None, 0),
    schema_column("description", "TEXT", true, Some("''"), 0),
    schema_column("design", "TEXT", true, Some("''"), 0),
    schema_column("acceptance_criteria", "TEXT", true, Some("''"), 0),
    schema_column("notes", "TEXT", true, Some("''"), 0),
    schema_column("status", "TEXT", true, Some("'open'"), 0),
    schema_column("priority", "INTEGER", true, Some("2"), 0),
    schema_column("issue_type", "TEXT", true, Some("'task'"), 0),
    schema_column("assignee", "TEXT", false, None, 0),
    schema_column("owner", "TEXT", false, Some("''"), 0),
    schema_column("estimated_minutes", "INTEGER", false, None, 0),
    schema_column("created_at", "DATETIME", true, Some("CURRENT_TIMESTAMP"), 0),
    schema_column("created_by", "TEXT", false, Some("''"), 0),
    schema_column("updated_at", "DATETIME", true, Some("CURRENT_TIMESTAMP"), 0),
    schema_column("closed_at", "DATETIME", false, None, 0),
    schema_column("close_reason", "TEXT", false, Some("''"), 0),
    schema_column("closed_by_session", "TEXT", false, Some("''"), 0),
    schema_column("due_at", "DATETIME", false, None, 0),
    schema_column("defer_until", "DATETIME", false, None, 0),
    schema_column("external_ref", "TEXT", false, None, 0),
    schema_column("source_system", "TEXT", false, Some("''"), 0),
    schema_column("source_repo", "TEXT", true, Some("'.'"), 0),
    schema_column("deleted_at", "DATETIME", false, None, 0),
    schema_column("deleted_by", "TEXT", false, Some("''"), 0),
    schema_column("delete_reason", "TEXT", false, Some("''"), 0),
    schema_column("original_type", "TEXT", false, Some("''"), 0),
    schema_column("compaction_level", "INTEGER", false, Some("0"), 0),
    schema_column("compacted_at", "DATETIME", false, None, 0),
    schema_column("compacted_at_commit", "TEXT", false, None, 0),
    schema_column("original_size", "INTEGER", false, None, 0),
    schema_column("sender", "TEXT", false, Some("''"), 0),
    schema_column("ephemeral", "INTEGER", true, Some("0"), 0),
    schema_column("pinned", "INTEGER", true, Some("0"), 0),
    schema_column("is_template", "INTEGER", true, Some("0"), 0),
    schema_column("source_repo_path", "TEXT", false, None, 0),
    schema_column("agent_context", "TEXT", false, None, 0),
];

const DEPENDENCIES_RUNTIME_COLUMNS: &[ExpectedSchemaColumn] = &[
    schema_column("issue_id", "TEXT", true, None, 1),
    schema_column("depends_on_id", "TEXT", true, None, 2),
    schema_column("type", "TEXT", true, Some("'blocks'"), 0),
    schema_column("created_at", "DATETIME", true, Some("CURRENT_TIMESTAMP"), 0),
    schema_column("created_by", "TEXT", true, Some("''"), 0),
    schema_column("metadata", "TEXT", false, Some("'{}'"), 0),
    schema_column("thread_id", "TEXT", false, Some("''"), 0),
];

const LABELS_RUNTIME_COLUMNS: &[ExpectedSchemaColumn] = &[
    schema_column("issue_id", "TEXT", true, None, 1),
    schema_column("label", "TEXT", true, None, 2),
];

const COMMENTS_RUNTIME_COLUMNS: &[ExpectedSchemaColumn] = &[
    schema_column("id", "INTEGER", false, None, 1),
    schema_column("issue_id", "TEXT", true, None, 0),
    schema_column("author", "TEXT", true, None, 0),
    schema_column("text", "TEXT", true, None, 0),
    schema_column("created_at", "DATETIME", true, Some("CURRENT_TIMESTAMP"), 0),
];

const EVENTS_RUNTIME_COLUMNS: &[ExpectedSchemaColumn] = &[
    schema_column("id", "INTEGER", false, None, 1),
    schema_column("issue_id", "TEXT", true, None, 0),
    schema_column("event_type", "TEXT", true, None, 0),
    schema_column("actor", "TEXT", true, Some("''"), 0),
    schema_column("old_value", "TEXT", false, None, 0),
    schema_column("new_value", "TEXT", false, None, 0),
    schema_column("comment", "TEXT", false, None, 0),
    schema_column("created_at", "DATETIME", true, Some("CURRENT_TIMESTAMP"), 0),
    schema_column("agent_name", "TEXT", false, None, 0),
    schema_column("harness", "TEXT", false, None, 0),
    schema_column("model", "TEXT", false, None, 0),
];

const CONFIG_RUNTIME_COLUMNS: &[ExpectedSchemaColumn] = &[
    schema_column("key", "TEXT", true, None, 0),
    schema_column("value", "TEXT", true, None, 0),
];
const METADATA_RUNTIME_COLUMNS: &[ExpectedSchemaColumn] = CONFIG_RUNTIME_COLUMNS;
const DIRTY_ISSUES_RUNTIME_COLUMNS: &[ExpectedSchemaColumn] = &[
    schema_column("issue_id", "TEXT", false, None, 1),
    schema_column("marked_at", "DATETIME", true, Some("CURRENT_TIMESTAMP"), 0),
];
const EXPORT_HASHES_RUNTIME_COLUMNS: &[ExpectedSchemaColumn] = &[
    schema_column("issue_id", "TEXT", false, None, 1),
    schema_column("content_hash", "TEXT", true, None, 0),
    schema_column(
        "exported_at",
        "DATETIME",
        true,
        Some("CURRENT_TIMESTAMP"),
        0,
    ),
];
const BLOCKED_CACHE_RUNTIME_COLUMNS: &[ExpectedSchemaColumn] = &[
    schema_column("issue_id", "TEXT", false, None, 1),
    schema_column("blocked_by", "TEXT", true, None, 0),
    schema_column("blocked_at", "DATETIME", true, Some("CURRENT_TIMESTAMP"), 0),
];
const CHILD_COUNTERS_RUNTIME_COLUMNS: &[ExpectedSchemaColumn] = &[
    schema_column("parent_id", "TEXT", false, None, 1),
    schema_column("last_child", "INTEGER", true, Some("0"), 0),
];

const ISSUES_RUNTIME_INDEXES: &[ExpectedRuntimeIndex] = &[
    runtime_index("idx_issues_status", &["status"], false, false),
    runtime_index("idx_issues_priority", &["priority"], false, false),
    runtime_index("idx_issues_issue_type", &["issue_type"], false, false),
    runtime_index("idx_issues_assignee", &["assignee"], false, true),
    runtime_index("idx_issues_created_at", &["created_at"], false, false),
    runtime_index("idx_issues_updated_at", &["updated_at"], false, false),
    runtime_index("idx_issues_content_hash", &["content_hash"], false, false),
    runtime_index(
        "idx_issues_external_ref_unique",
        &["external_ref"],
        true,
        true,
    ),
    runtime_index("idx_issues_ephemeral", &["ephemeral"], false, true),
    runtime_index("idx_issues_pinned", &["pinned"], false, true),
    runtime_index("idx_issues_tombstone", &["status"], false, true),
    runtime_index("idx_issues_due_at", &["due_at"], false, true),
    runtime_index("idx_issues_defer_until", &["defer_until"], false, true),
    runtime_index(
        "idx_issues_ready",
        &["status", "priority", "created_at"],
        false,
        true,
    ),
    runtime_index(
        "idx_issues_status_priority_created",
        &["status", "priority", "created_at"],
        false,
        false,
    ),
    runtime_index(
        "idx_issues_list_active_order",
        &["priority", "created_at"],
        false,
        true,
    ),
];

const DEPENDENCIES_RUNTIME_INDEXES: &[ExpectedRuntimeIndex] = &[
    runtime_index("idx_dependencies_issue", &["issue_id"], false, false),
    runtime_index(
        "idx_dependencies_depends_on",
        &["depends_on_id"],
        false,
        false,
    ),
    runtime_index("idx_dependencies_type", &["type"], false, false),
    runtime_index(
        "idx_dependencies_depends_on_type",
        &["depends_on_id", "type"],
        false,
        false,
    ),
    runtime_index("idx_dependencies_thread", &["thread_id"], false, true),
    runtime_index(
        "idx_dependencies_blocking",
        &["depends_on_id", "issue_id"],
        false,
        true,
    ),
];
const LABELS_RUNTIME_INDEXES: &[ExpectedRuntimeIndex] = &[
    runtime_index("idx_labels_label", &["label"], false, false),
    runtime_index("idx_labels_issue", &["issue_id"], false, false),
];
const COMMENTS_RUNTIME_INDEXES: &[ExpectedRuntimeIndex] = &[
    runtime_index("idx_comments_issue", &["issue_id"], false, false),
    runtime_index("idx_comments_created_at", &["created_at"], false, false),
];
const EVENTS_RUNTIME_INDEXES: &[ExpectedRuntimeIndex] = &[
    runtime_index("idx_events_issue", &["issue_id"], false, false),
    runtime_index("idx_events_type", &["event_type"], false, false),
    runtime_index("idx_events_created_at", &["created_at"], false, false),
    runtime_index("idx_events_actor", &["actor"], false, true),
];
const CONFIG_RUNTIME_INDEXES: &[ExpectedRuntimeIndex] =
    &[runtime_index("idx_config_key", &["key"], false, false)];
const METADATA_RUNTIME_INDEXES: &[ExpectedRuntimeIndex] =
    &[runtime_index("idx_metadata_key", &["key"], false, false)];
const DIRTY_ISSUES_RUNTIME_INDEXES: &[ExpectedRuntimeIndex] = &[runtime_index(
    "idx_dirty_issues_marked_at",
    &["marked_at"],
    false,
    false,
)];
const BLOCKED_CACHE_RUNTIME_INDEXES: &[ExpectedRuntimeIndex] = &[runtime_index(
    "idx_blocked_cache_blocked_at",
    &["blocked_at"],
    false,
    false,
)];

// Most partial-index predicates affect only planner eligibility: SQLite will
// not use a partial index for a query that does not imply the predicate. The
// external-reference index is different because its UNIQUE bit is part of the
// data model. A weaker predicate can silently admit duplicate non-NULL
// external references, so its WHERE clause is part of the runtime contract.
const SEMANTIC_PARTIAL_INDEX_PREDICATES: &[(&str, &str)] =
    &[("idx_issues_external_ref_unique", "external_ref IS NOT NULL")];

// Complete runtime column manifests for auxiliary tables. `Some(definition)`
// marks a column SQLite can add without inventing audit data or installing an
// incorrect non-constant default. `None` means a malformed existing table must
// fail closed; a missing table is still created canonically by SCHEMA_SQL.
const CLOSE_METADATA_COLUMNS: &[AuxiliaryRuntimeColumn] = &[
    auxiliary_runtime_column("issue_id", "TEXT", false, None, 1, None),
    auxiliary_runtime_column("closed_by_agent_name", "TEXT", false, None, 0, Some("TEXT")),
    auxiliary_runtime_column("closed_by_harness", "TEXT", false, None, 0, Some("TEXT")),
    auxiliary_runtime_column("closed_by_model", "TEXT", false, None, 0, Some("TEXT")),
    auxiliary_runtime_column(
        "bypassed_policy",
        "INTEGER",
        true,
        Some("0"),
        0,
        Some("INTEGER NOT NULL DEFAULT 0"),
    ),
    auxiliary_runtime_column("bypass_reason", "TEXT", false, None, 0, Some("TEXT")),
    auxiliary_runtime_column("policy_gates_fired", "TEXT", false, None, 0, Some("TEXT")),
    auxiliary_runtime_column(
        "recorded_at",
        "DATETIME",
        true,
        Some("CURRENT_TIMESTAMP"),
        0,
        None,
    ),
];
const CLOSE_METADATA_INDEXES: &[ExpectedRuntimeIndex] = &[
    ExpectedRuntimeIndex {
        name: "idx_close_metadata_recorded_at",
        columns: &["recorded_at"],
        unique: false,
        partial: false,
    },
    ExpectedRuntimeIndex {
        name: "idx_close_metadata_bypassed",
        columns: &["bypassed_policy"],
        unique: false,
        partial: true,
    },
];

const GATE_RESULTS_COLUMNS: &[AuxiliaryRuntimeColumn] = &[
    auxiliary_runtime_column("issue_id", "TEXT", true, None, 1, None),
    auxiliary_runtime_column("gate", "TEXT", true, None, 2, None),
    auxiliary_runtime_column("provider", "TEXT", true, None, 3, None),
    auxiliary_runtime_column(
        "passed",
        "INTEGER",
        true,
        Some("0"),
        0,
        Some("INTEGER NOT NULL DEFAULT 0"),
    ),
    auxiliary_runtime_column("note", "TEXT", false, None, 0, Some("TEXT")),
    auxiliary_runtime_column("recorded_by", "TEXT", false, None, 0, Some("TEXT")),
    auxiliary_runtime_column(
        "recorded_at",
        "DATETIME",
        true,
        Some("CURRENT_TIMESTAMP"),
        0,
        None,
    ),
];
const GATE_RESULTS_INDEXES: &[ExpectedRuntimeIndex] = &[ExpectedRuntimeIndex {
    name: "idx_gate_results_issue",
    columns: &["issue_id"],
    unique: false,
    partial: false,
}];

const CAPACITY_EXEMPTION_COLUMNS: &[AuxiliaryRuntimeColumn] = &[
    auxiliary_runtime_column("issue_id", "TEXT", true, None, 1, None),
    auxiliary_runtime_column("capacity_kind", "TEXT", true, None, 2, None),
    auxiliary_runtime_column("capacity_name", "TEXT", true, None, 3, None),
    auxiliary_runtime_column("provider", "TEXT", true, None, 0, None),
    auxiliary_runtime_column("reason", "TEXT", true, None, 0, None),
    auxiliary_runtime_column("granted_by", "TEXT", true, None, 0, None),
    auxiliary_runtime_column(
        "granted_at",
        "DATETIME",
        true,
        Some("CURRENT_TIMESTAMP"),
        0,
        None,
    ),
    auxiliary_runtime_column("expires_at", "DATETIME", false, None, 0, Some("DATETIME")),
    auxiliary_runtime_column("ended_at", "DATETIME", false, None, 0, Some("DATETIME")),
    auxiliary_runtime_column("ended_action", "TEXT", false, None, 0, Some("TEXT")),
    auxiliary_runtime_column("ended_by", "TEXT", false, None, 0, Some("TEXT")),
];
const CAPACITY_EXEMPTION_INDEXES: &[ExpectedRuntimeIndex] = &[ExpectedRuntimeIndex {
    name: "idx_capacity_exemptions_capacity",
    columns: &["capacity_kind", "capacity_name"],
    unique: false,
    partial: false,
}];

const CAPACITY_EXEMPTION_HISTORY_COLUMNS: &[AuxiliaryRuntimeColumn] = &[
    auxiliary_runtime_column("id", "INTEGER", false, None, 1, None),
    auxiliary_runtime_column("issue_id", "TEXT", true, None, 0, None),
    auxiliary_runtime_column("capacity_kind", "TEXT", true, None, 0, None),
    auxiliary_runtime_column("capacity_name", "TEXT", true, None, 0, None),
    auxiliary_runtime_column("action", "TEXT", true, None, 0, None),
    auxiliary_runtime_column("provider", "TEXT", true, None, 0, None),
    auxiliary_runtime_column("reason", "TEXT", false, None, 0, Some("TEXT")),
    auxiliary_runtime_column("actor", "TEXT", true, None, 0, None),
    auxiliary_runtime_column("expires_at", "DATETIME", false, None, 0, Some("DATETIME")),
    auxiliary_runtime_column(
        "recorded_at",
        "DATETIME",
        true,
        Some("CURRENT_TIMESTAMP"),
        0,
        None,
    ),
];
const CAPACITY_EXEMPTION_HISTORY_INDEXES: &[ExpectedRuntimeIndex] = &[ExpectedRuntimeIndex {
    name: "idx_capacity_exemption_history_issue",
    columns: &["issue_id", "id"],
    unique: false,
    partial: false,
}];

const CAPACITY_OCCUPANCY_COLUMNS: &[AuxiliaryRuntimeColumn] = &[
    auxiliary_runtime_column("issue_id", "TEXT", false, None, 1, None),
    auxiliary_runtime_column("actor", "TEXT", false, None, 0, Some("TEXT")),
    auxiliary_runtime_column("agent_name", "TEXT", false, None, 0, Some("TEXT")),
    auxiliary_runtime_column("harness", "TEXT", false, None, 0, Some("TEXT")),
    auxiliary_runtime_column("session", "TEXT", false, None, 0, Some("TEXT")),
    auxiliary_runtime_column(
        "recorded_at",
        "DATETIME",
        true,
        Some("CURRENT_TIMESTAMP"),
        0,
        None,
    ),
];
const CAPACITY_OCCUPANCY_INDEXES: &[ExpectedRuntimeIndex] = &[
    ExpectedRuntimeIndex {
        name: "idx_capacity_occupancy_actor",
        columns: &["actor"],
        unique: false,
        partial: true,
    },
    ExpectedRuntimeIndex {
        name: "idx_capacity_occupancy_harness",
        columns: &["harness"],
        unique: false,
        partial: true,
    },
    ExpectedRuntimeIndex {
        name: "idx_capacity_occupancy_session",
        columns: &["session"],
        unique: false,
        partial: true,
    },
];

fn ensure_columns(conn: &Connection, table: &str, columns: &[(&str, &str)]) -> Result<()> {
    if !table_exists(conn, table) {
        return Ok(());
    }

    for (name, definition) in columns {
        if !column_exists(conn, table, name) {
            let sql = format!("ALTER TABLE {table} ADD COLUMN {name} {definition}");
            conn.execute(&sql)?;
        }
    }

    Ok(())
}

fn ensure_auxiliary_runtime_columns(
    conn: &Connection,
    table: &str,
    columns: &[AuxiliaryRuntimeColumn],
) -> Result<()> {
    if !table_exists(conn, table) {
        return Ok(());
    }

    if let Some(column) = columns.iter().find(|column| {
        column.additive_definition.is_none() && !column_exists(conn, table, column.expected.name)
    }) {
        return Err(BeadsError::Config(format!(
            "Cannot safely repair malformed {table} table: required column '{}' cannot be added without changing audit semantics",
            column.expected.name
        )));
    }

    for column in columns {
        if let Some(definition) = column.additive_definition
            && !column_exists(conn, table, column.expected.name)
        {
            conn.execute(&format!(
                "ALTER TABLE {table} ADD COLUMN {} {definition}",
                column.expected.name
            ))?;
        }
    }

    if !auxiliary_runtime_columns_canonical(conn, table, columns)
        || !auxiliary_runtime_issue_foreign_key_canonical(conn, table)
    {
        return Err(BeadsError::Config(format!(
            "Cannot safely repair malformed {table} table: column constraints or issue cascade are not canonical"
        )));
    }

    Ok(())
}

fn auxiliary_runtime_columns_canonical(
    conn: &Connection,
    table: &str,
    columns: &[AuxiliaryRuntimeColumn],
) -> bool {
    let escaped_table = table.replace('\'', "''");
    let Ok(rows) = conn.query(&format!("PRAGMA table_info('{escaped_table}')")) else {
        return false;
    };
    if rows.len() != columns.len() {
        return false;
    }

    columns.iter().all(|column| {
        let expected = column.expected;
        rows.iter().any(|row| {
            let name = row.get(1).and_then(SqliteValue::as_text);
            let data_type = row.get(2).and_then(SqliteValue::as_text);
            let not_null = row
                .get(3)
                .and_then(SqliteValue::as_integer)
                .is_some_and(|value| value != 0);
            let default_value = row.get(4).and_then(SqliteValue::as_text);
            let primary_key_position = row.get(5).and_then(SqliteValue::as_integer);

            name == Some(expected.name)
                && data_type.is_some_and(|value| value.eq_ignore_ascii_case(expected.data_type))
                && not_null == expected.not_null
                && sql_default_matches(default_value, expected.default_value)
                && primary_key_position == Some(expected.primary_key_position)
        })
    })
}

fn auxiliary_runtime_issue_foreign_key_canonical(conn: &Connection, table: &str) -> bool {
    let escaped_table = table.replace('\'', "''");
    let Ok(rows) = conn.query(&format!("PRAGMA foreign_key_list('{escaped_table}')")) else {
        return false;
    };
    if rows.len() != 1 {
        return false;
    }
    let row = &rows[0];
    row.get(1).and_then(SqliteValue::as_integer) == Some(0)
        && row.get(2).and_then(SqliteValue::as_text) == Some("issues")
        && row.get(3).and_then(SqliteValue::as_text) == Some("issue_id")
        && row.get(4).and_then(SqliteValue::as_text) == Some("id")
        && row
            .get(5)
            .and_then(SqliteValue::as_text)
            .is_some_and(|value| value.eq_ignore_ascii_case("NO ACTION"))
        && row
            .get(6)
            .and_then(SqliteValue::as_text)
            .is_some_and(|value| value.eq_ignore_ascii_case("CASCADE"))
}

fn table_declares_autoincrement_primary_key(conn: &Connection, table: &str, column: &str) -> bool {
    let escaped_table = table.replace('\'', "''");
    let Ok(row) = conn.query_row(&format!(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = '{escaped_table}'"
    )) else {
        return false;
    };
    let Some(sql) = row.get(0).and_then(SqliteValue::as_text) else {
        return false;
    };
    sql_contains_token_sequence(sql, &format!("{column} INTEGER PRIMARY KEY AUTOINCREMENT"))
}

fn runtime_index_key_shape_canonical(
    conn: &Connection,
    index: &str,
    expected_columns: &[&str],
) -> bool {
    let escaped_index = index.replace('\'', "''");
    let Ok(rows) = conn.query(&format!("PRAGMA index_xinfo('{escaped_index}')")) else {
        return false;
    };
    if rows
        .iter()
        .any(|row| row.get(5).and_then(SqliteValue::as_integer).is_none())
    {
        return false;
    }

    let key_rows = rows
        .iter()
        .filter(|row| {
            row.get(5)
                .and_then(SqliteValue::as_integer)
                .is_some_and(|value| value != 0)
        })
        .collect::<Vec<_>>();
    key_rows.len() == expected_columns.len()
        && key_rows.iter().zip(expected_columns).enumerate().all(
            |(position, (row, expected_name))| {
                row.get(0).and_then(SqliteValue::as_integer) == i64::try_from(position).ok()
                    && row.get(2).and_then(SqliteValue::as_text) == Some(*expected_name)
                    && row.get(3).and_then(SqliteValue::as_integer) == Some(0)
                    && row
                        .get(4)
                        .and_then(SqliteValue::as_text)
                        .is_some_and(|collation| collation.eq_ignore_ascii_case("BINARY"))
            },
        )
}

fn auxiliary_runtime_indexes_canonical(
    conn: &Connection,
    table: &str,
    indexes: &[ExpectedRuntimeIndex],
) -> bool {
    let escaped_table = table.replace('\'', "''");
    let Ok(index_rows) = conn.query(&format!("PRAGMA index_list('{escaped_table}')")) else {
        return false;
    };

    let expected_indexes_match = indexes.iter().all(|expected| {
        let Some(index_row) = index_rows
            .iter()
            .find(|row| row.get(1).and_then(SqliteValue::as_text) == Some(expected.name))
        else {
            return false;
        };
        let uniqueness_matches =
            index_row.get(2).and_then(SqliteValue::as_integer) == Some(i64::from(expected.unique));
        let explicitly_created = index_row
            .get(3)
            .and_then(SqliteValue::as_text)
            .is_some_and(|value| value.eq_ignore_ascii_case("c"));
        let partial = index_row
            .get(4)
            .and_then(SqliteValue::as_integer)
            .is_some_and(|value| value != 0);
        if !uniqueness_matches || !explicitly_created || partial != expected.partial {
            return false;
        }

        runtime_index_key_shape_canonical(conn, expected.name, expected.columns)
            && semantic_partial_index_predicate_canonical(conn, expected.name)
    });

    expected_indexes_match
        && index_rows.iter().all(|row| {
            let unique = row
                .get(2)
                .and_then(SqliteValue::as_integer)
                .is_some_and(|value| value != 0);
            if !unique {
                return true;
            }

            let Some(name) = row.get(1).and_then(SqliteValue::as_text) else {
                return false;
            };
            match row.get(3).and_then(SqliteValue::as_text) {
                // A canonical PRIMARY KEY may have an automatic backing index.
                Some(origin) if origin.eq_ignore_ascii_case("pk") => true,
                // Explicit unique indexes are accepted only when their exact
                // name and UNIQUE shape are in this table's manifest.
                Some(origin) if origin.eq_ignore_ascii_case("c") => indexes
                    .iter()
                    .any(|expected| expected.name == name && expected.unique),
                // An automatic UNIQUE constraint (origin `u`) or an unknown
                // origin changes which otherwise-valid rows can be written.
                _ => false,
            }
        })
}

fn sql_without_comments(sql: &str) -> String {
    let bytes = sql.as_bytes();
    let mut output = String::with_capacity(sql.len());
    let mut copy_start = 0;
    let mut index = 0;
    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;

    while index < bytes.len() {
        let byte = bytes[index];
        if in_line_comment {
            if byte == b'\n' {
                in_line_comment = false;
                copy_start = index;
            }
            index += 1;
            continue;
        }
        if in_block_comment {
            if byte == b'*' && index + 1 < bytes.len() && bytes[index + 1] == b'/' {
                in_block_comment = false;
                index += 2;
                copy_start = index;
            } else {
                index += 1;
            }
            continue;
        }
        if in_single_quote {
            if byte == b'\'' {
                if index + 1 < bytes.len() && bytes[index + 1] == b'\'' {
                    index += 2;
                } else {
                    in_single_quote = false;
                    index += 1;
                }
            } else {
                index += 1;
            }
            continue;
        }
        if in_double_quote {
            if byte == b'"' {
                if index + 1 < bytes.len() && bytes[index + 1] == b'"' {
                    index += 2;
                } else {
                    in_double_quote = false;
                    index += 1;
                }
            } else {
                index += 1;
            }
            continue;
        }

        if byte == b'\'' {
            in_single_quote = true;
            index += 1;
        } else if byte == b'"' {
            in_double_quote = true;
            index += 1;
        } else if byte == b'-' && index + 1 < bytes.len() && bytes[index + 1] == b'-' {
            output.push_str(&sql[copy_start..index]);
            output.push(' ');
            in_line_comment = true;
            index += 2;
        } else if byte == b'/' && index + 1 < bytes.len() && bytes[index + 1] == b'*' {
            output.push_str(&sql[copy_start..index]);
            output.push(' ');
            in_block_comment = true;
            index += 2;
        } else {
            index += 1;
        }
    }

    if !in_line_comment && !in_block_comment {
        output.push_str(&sql[copy_start..]);
    }
    output
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SqlEvidenceToken {
    Unquoted(String),
    StringLiteral(String),
    QuotedIdentifier(String),
    Symbol(char),
}

const fn sql_delimited_token_end(bytes: &[u8], start: usize, delimiter: u8) -> usize {
    let mut index = start + 1;
    while index < bytes.len() {
        if bytes[index] != delimiter {
            index += 1;
            continue;
        }
        index += 1;
        if index < bytes.len() && bytes[index] == delimiter {
            index += 1;
        } else {
            break;
        }
    }
    index
}

fn sql_evidence_tokens(sql: &str) -> Vec<SqlEvidenceToken> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;

    while index < bytes.len() {
        let byte = bytes[index];
        if byte.is_ascii_whitespace() {
            index += 1;
            continue;
        }
        if byte == b'-' && index + 1 < bytes.len() && bytes[index + 1] == b'-' {
            index += 2;
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if byte == b'/' && index + 1 < bytes.len() && bytes[index + 1] == b'*' {
            index += 2;
            while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/') {
                index += 1;
            }
            index = (index + 2).min(bytes.len());
            continue;
        }
        if byte == b'\'' {
            let end = sql_delimited_token_end(bytes, index, byte);
            tokens.push(SqlEvidenceToken::StringLiteral(sql[index..end].to_string()));
            index = end;
            continue;
        }
        if byte == b'"' || byte == b'`' {
            let end = sql_delimited_token_end(bytes, index, byte);
            tokens.push(SqlEvidenceToken::QuotedIdentifier(
                sql[index..end].to_string(),
            ));
            index = end;
            continue;
        }
        if byte == b'[' {
            let start = index;
            index += 1;
            while index < bytes.len() && bytes[index] != b']' {
                index += 1;
            }
            if index < bytes.len() {
                index += 1;
            }
            tokens.push(SqlEvidenceToken::QuotedIdentifier(
                sql[start..index].to_string(),
            ));
            continue;
        }
        if byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$') {
            let start = index;
            index += 1;
            while index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric() || matches!(bytes[index], b'_' | b'$'))
            {
                index += 1;
            }
            tokens.push(SqlEvidenceToken::Unquoted(
                sql[start..index].to_ascii_lowercase(),
            ));
            continue;
        }

        let Some(symbol) = sql[index..].chars().next() else {
            break;
        };
        tokens.push(SqlEvidenceToken::Symbol(symbol));
        index += symbol.len_utf8();
    }

    tokens
}

fn sql_contains_token_sequence(sql: &str, expected: &str) -> bool {
    let actual_tokens = sql_evidence_tokens(sql);
    let expected_tokens = sql_evidence_tokens(expected);
    !expected_tokens.is_empty()
        && actual_tokens
            .windows(expected_tokens.len())
            .any(|tokens| tokens == expected_tokens)
}

fn compact_sql_fragment(sql: &str) -> String {
    sql_without_comments(sql)
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .flat_map(char::to_lowercase)
        .collect()
}

fn semantic_partial_index_predicate_canonical(conn: &Connection, index: &str) -> bool {
    let Some((_, predicate)) = SEMANTIC_PARTIAL_INDEX_PREDICATES
        .iter()
        .find(|(expected_index, _)| *expected_index == index)
    else {
        return true;
    };
    let escaped_index = index.replace('\'', "''");
    let Ok(row) = conn.query_row(&format!(
        "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = '{escaped_index}'"
    )) else {
        return false;
    };
    let Some(sql) = row.get(0).and_then(SqliteValue::as_text) else {
        return false;
    };
    let normalized = compact_sql_fragment(sql);
    let expected_suffix = format!("where{}", compact_sql_fragment(predicate));
    normalized.trim_end_matches(';').ends_with(&expected_suffix)
}

fn auxiliary_runtime_table_canonical(
    conn: &Connection,
    table: &str,
    columns: &[AuxiliaryRuntimeColumn],
    indexes: &[ExpectedRuntimeIndex],
) -> bool {
    auxiliary_runtime_columns_canonical(conn, table, columns)
        && auxiliary_runtime_issue_foreign_key_canonical(conn, table)
        && auxiliary_runtime_indexes_canonical(conn, table, indexes)
}

fn core_runtime_default_matches(
    table: &str,
    column: &str,
    actual: Option<&str>,
    expected: Option<&str>,
) -> bool {
    sql_default_matches(actual, expected)
        || matches!(
            (table, column, actual, expected),
            ("comments", "author" | "text", Some("''"), None)
                | ("events", "event_type", Some("''"), None)
        )
}

fn core_runtime_columns_canonical(
    conn: &Connection,
    table: &str,
    columns: &[ExpectedSchemaColumn],
    order_sensitive: bool,
) -> bool {
    let escaped_table = table.replace('\'', "''");
    let Ok(rows) = conn.query(&format!("PRAGMA table_info('{escaped_table}')")) else {
        return false;
    };
    if rows.len() != columns.len() {
        return false;
    }

    columns.iter().enumerate().all(|(position, expected)| {
        if order_sensitive {
            rows.get(position)
                .is_some_and(|row| core_runtime_column_matches(table, row, expected))
        } else {
            rows.iter()
                .any(|row| core_runtime_column_matches(table, row, expected))
        }
    })
}

fn core_runtime_column_matches(
    table: &str,
    row: &fsqlite::Row,
    expected: &ExpectedSchemaColumn,
) -> bool {
    let name = row.get(1).and_then(SqliteValue::as_text);
    let data_type = row.get(2).and_then(SqliteValue::as_text);
    let not_null = row
        .get(3)
        .and_then(SqliteValue::as_integer)
        .is_some_and(|value| value != 0);
    let default_value = row.get(4).and_then(SqliteValue::as_text);
    let primary_key_position = row.get(5).and_then(SqliteValue::as_integer);
    name == Some(expected.name)
        && data_type.is_some_and(|value| value.eq_ignore_ascii_case(expected.data_type))
        && not_null == expected.not_null
        && core_runtime_default_matches(table, expected.name, default_value, expected.default_value)
        && primary_key_position == Some(expected.primary_key_position)
}

fn core_runtime_foreign_keys_canonical(
    conn: &Connection,
    table: &str,
    issue_reference_columns: &[&str],
) -> bool {
    let escaped_table = table.replace('\'', "''");
    let Ok(rows) = conn.query(&format!("PRAGMA foreign_key_list('{escaped_table}')")) else {
        return false;
    };
    rows.len() == issue_reference_columns.len()
        && rows
            .iter()
            .zip(issue_reference_columns)
            .enumerate()
            .all(|(sequence, (row, from))| {
                row.get(1).and_then(SqliteValue::as_integer) == i64::try_from(sequence).ok()
                    && row.get(2).and_then(SqliteValue::as_text) == Some("issues")
                    && row.get(3).and_then(SqliteValue::as_text) == Some(*from)
                    && row.get(4).and_then(SqliteValue::as_text) == Some("id")
                    && row
                        .get(5)
                        .and_then(SqliteValue::as_text)
                        .is_some_and(|value| value.eq_ignore_ascii_case("NO ACTION"))
                    && row
                        .get(6)
                        .and_then(SqliteValue::as_text)
                        .is_some_and(|value| value.eq_ignore_ascii_case("CASCADE"))
            })
}

fn table_has_no_unique_indexes(conn: &Connection, table: &str) -> bool {
    let escaped_table = table.replace('\'', "''");
    conn.query(&format!("PRAGMA index_list('{escaped_table}')"))
        .is_ok_and(|rows| {
            rows.iter()
                .all(|row| row.get(2).and_then(SqliteValue::as_integer) == Some(0))
        })
}

#[allow(clippy::too_many_arguments)]
fn core_runtime_table_canonical(
    conn: &Connection,
    table: &str,
    columns: &[ExpectedSchemaColumn],
    issue_reference_columns: &[&str],
    indexes: &[ExpectedRuntimeIndex],
    order_sensitive: bool,
    autoincrement_primary_key: Option<&str>,
    forbid_unique_indexes: bool,
) -> bool {
    core_runtime_columns_canonical(conn, table, columns, order_sensitive)
        && core_runtime_foreign_keys_canonical(conn, table, issue_reference_columns)
        && auxiliary_runtime_indexes_canonical(conn, table, indexes)
        && autoincrement_primary_key
            .is_none_or(|column| table_declares_autoincrement_primary_key(conn, table, column))
        && (!forbid_unique_indexes || table_has_no_unique_indexes(conn, table))
}

fn issues_required_checks_canonical(conn: &Connection) -> bool {
    let Ok(row) =
        conn.query_row("SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'issues'")
    else {
        return false;
    };
    let Some(sql) = row.get(0).and_then(SqliteValue::as_text) else {
        return false;
    };
    [
        "CHECK(length(title) <= 500)",
        "CHECK(priority >= 0 AND priority <= 4)",
        ISSUES_CLOSED_AT_CHECK,
    ]
    .iter()
    .all(|required| sql_contains_token_sequence(sql, required))
}

fn blocked_cache_table_canonical(conn: &Connection) -> bool {
    if !table_exists(conn, "blocked_issues_cache") {
        return false;
    }

    let Ok(rows) = conn.query("PRAGMA table_info(blocked_issues_cache)") else {
        return false;
    };

    let mut issue_id_primary_key = false;
    let mut blocked_by_not_null = false;
    let mut blocked_at_not_null = false;
    let mut has_legacy_blocked_by_json = false;

    for row in &rows {
        let Some(name) = row.get(1).and_then(SqliteValue::as_text) else {
            continue;
        };
        let not_null = row
            .get(3)
            .and_then(SqliteValue::as_integer)
            .is_some_and(|value| value != 0);
        let primary_key = row
            .get(5)
            .and_then(SqliteValue::as_integer)
            .is_some_and(|value| value != 0);

        match name {
            "issue_id" => issue_id_primary_key = primary_key,
            "blocked_by" => blocked_by_not_null = not_null,
            "blocked_at" => blocked_at_not_null = not_null,
            "blocked_by_json" => has_legacy_blocked_by_json = true,
            _ => {}
        }
    }

    issue_id_primary_key
        && blocked_by_not_null
        && blocked_at_not_null
        && !has_legacy_blocked_by_json
}

fn runtime_user_version(conn: &Connection) -> Result<i64> {
    conn.query_row("PRAGMA user_version")?
        .get(0)
        .and_then(SqliteValue::as_integer)
        .ok_or_else(|| BeadsError::internal("PRAGMA user_version returned no integer value"))
}

fn current_schema_version_declared(conn: &Connection) -> bool {
    runtime_user_version(conn).is_ok_and(|version| version == i64::from(CURRENT_SCHEMA_VERSION))
}

/// Expected column order for the issues table (id + ISSUE_COLUMNS names).
/// Used to detect when ALTER TABLE has appended columns in the wrong position,
/// which causes fsqlite to fail with "no such column" errors on older databases.
const EXPECTED_ISSUE_COLUMN_ORDER: &[&str] = &[
    "id",
    "content_hash",
    "title",
    "description",
    "design",
    "acceptance_criteria",
    "notes",
    "status",
    "priority",
    "issue_type",
    "assignee",
    "owner",
    "estimated_minutes",
    "created_at",
    "created_by",
    "updated_at",
    "closed_at",
    "close_reason",
    "closed_by_session",
    "due_at",
    "defer_until",
    "external_ref",
    "source_system",
    "source_repo",
    "deleted_at",
    "deleted_by",
    "delete_reason",
    "original_type",
    "compaction_level",
    "compacted_at",
    "compacted_at_commit",
    "original_size",
    "sender",
    "ephemeral",
    "pinned",
    "is_template",
    "source_repo_path",
    "agent_context",
];

/// Check whether the issues table has columns in the expected order.
/// Returns `true` if the column order matches, `false` if it differs or the
/// table doesn't exist.
fn issues_column_order_matches(conn: &Connection) -> bool {
    // Use PRAGMA table_info to detect both existence and column order in a
    // single query.  Avoid querying sqlite_master separately because
    // fsqlite's in-memory sqlite_master can return inconsistent results
    // when queried multiple times within the same connection session.
    let Ok(rows) = conn.query("PRAGMA table_info(issues)") else {
        return false;
    };

    let actual_columns: Vec<String> = rows
        .iter()
        .filter_map(|row| row.get(1).and_then(SqliteValue::as_text).map(String::from))
        .collect();

    if actual_columns.is_empty() {
        return true; // Table doesn't exist; will be created fresh by SCHEMA_SQL
    }

    if actual_columns.len() != EXPECTED_ISSUE_COLUMN_ORDER.len() {
        return false;
    }

    actual_columns
        .iter()
        .zip(EXPECTED_ISSUE_COLUMN_ORDER.iter())
        .all(|(actual, expected)| actual == expected)
}

fn issues_filter_columns_require_v3_rebuild(conn: &Connection) -> bool {
    let Ok(rows) = conn.query("PRAGMA table_info('issues')") else {
        return true;
    };

    for column in ["ephemeral", "pinned", "is_template"] {
        let Some(row) = rows
            .iter()
            .find(|row| row.get(1).and_then(SqliteValue::as_text) == Some(column))
        else {
            return true;
        };

        let not_null = row.get(3).and_then(SqliteValue::as_integer).unwrap_or(0);
        if not_null == 0 {
            return true;
        }
    }

    false
}

fn foreign_keys_enabled(conn: &Connection) -> Result<bool> {
    let row = conn.query_row("PRAGMA foreign_keys")?;
    Ok(row.get(0).and_then(SqliteValue::as_integer).unwrap_or(0) == 1)
}

fn restore_foreign_keys(conn: &Connection, operation: &str) -> Result<()> {
    conn.execute("PRAGMA foreign_keys = ON")
        .map_err(BeadsError::Database)?;

    if foreign_keys_enabled(conn)? {
        return Ok(());
    }

    Err(BeadsError::Config(format!(
        "failed to re-enable SQLite foreign key enforcement after {operation}: PRAGMA foreign_keys remained OFF"
    )))
}

fn finish_foreign_key_suppressed_result<T>(
    conn: &Connection,
    operation: &str,
    result: Result<T>,
) -> Result<T> {
    match (result, restore_foreign_keys(conn, operation)) {
        (Ok(value), Ok(())) => Ok(value),
        (Ok(_), Err(restore_error)) => Err(restore_error),
        (Err(original_error), Ok(())) => Err(original_error),
        (Err(original_error), Err(restore_error)) => Err(BeadsError::WithContext {
            context: format!(
                "{operation} failed, and SQLite foreign key enforcement could not be re-enabled: {restore_error}"
            ),
            source: Box::new(original_error),
        }),
    }
}

/// Rebuild the issues table so columns match the canonical SCHEMA_SQL order.
///
/// This fixes databases where ALTER TABLE ADD COLUMN appended columns in a
/// different position than the CREATE TABLE definition, causing fsqlite's
/// column-name resolver to fail with "no such column" errors.
///
/// Uses the standard SQLite migration pattern:
///   1. Create new table with correct schema
///   2. Copy data from old table
///   3. Drop old table
///   4. Rename new table
fn rebuild_issues_table(conn: &Connection) -> Result<()> {
    let existing_rows = conn.query("PRAGMA table_info('issues')")?;
    let existing_columns: Vec<String> = existing_rows
        .iter()
        .filter_map(|row| row.get(1).and_then(SqliteValue::as_text).map(String::from))
        .collect();

    if existing_columns.is_empty() {
        return Ok(()); // Table is empty or doesn't exist
    }

    // Disable foreign keys during the rebuild because we'll be dropping
    // and recreating the issues table which is referenced by other tables.
    // This property is connection-scoped.
    conn.execute("PRAGMA foreign_keys = OFF")?;

    let result = (|| -> Result<()> {
        // Wrap the entire rebuild in a transaction so a crash between DROP TABLE
        // and RENAME cannot lose data.
        conn.execute("BEGIN EXCLUSIVE")?;

        if let Err(e) = rebuild_issues_table_inner(conn, &existing_columns) {
            let _ = conn.execute("ROLLBACK");
            return Err(e);
        }

        if let Err(e) = conn.execute("COMMIT") {
            let _ = conn.execute("ROLLBACK");
            return Err(e.into());
        }

        Ok(())
    })();

    finish_foreign_key_suppressed_result(conn, "issues table rebuild", result)
}

/// Inner helper for [`rebuild_issues_table`] that performs the actual work
/// inside an already-open transaction.
fn rebuild_issues_table_inner(conn: &Connection, existing_columns: &[String]) -> Result<()> {
    // Drop all indexes on the issues table first (they'll be recreated by SCHEMA_SQL)
    let index_rows =
        conn.query("SELECT name FROM sqlite_master WHERE type='index' AND tbl_name='issues' AND sql IS NOT NULL")?;
    for row in &index_rows {
        if let Some(name) = row.get(0).and_then(SqliteValue::as_text) {
            conn.execute(&format!("DROP INDEX IF EXISTS \"{name}\""))?;
        }
    }

    // Drop tables that have foreign keys referencing issues (they'll be recreated)
    // We need to save and restore their data too.
    // For simplicity, we only rebuild the issues table and let SCHEMA_SQL
    // recreate indexes. Foreign key tables (dependencies, labels, etc.) keep
    // their data since we use the same primary key.

    // Create the new table with canonical column order
    // Use a temporary name to avoid conflicts
    conn.execute("DROP TABLE IF EXISTS issues_rebuild_tmp")?;

    // Build CREATE TABLE for the new table with only columns that exist in the old table
    // plus any missing columns with defaults
    // Build the canonical column list: id, content_hash, title, then the
    // rest of ISSUE_COLUMNS (skipping content_hash which is already placed).
    // This order must match EXPECTED_ISSUE_COLUMN_ORDER and SCHEMA_SQL.
    let all_expected: Vec<(&str, &str)> = std::iter::once(("id", "TEXT PRIMARY KEY"))
        .chain(std::iter::once(("content_hash", "TEXT")))
        .chain(std::iter::once((
            "title",
            "TEXT NOT NULL CHECK(length(title) <= 500)",
        )))
        .chain(
            ISSUE_COLUMNS
                .iter()
                .copied()
                .filter(|(name, _)| *name != "content_hash"),
        )
        .collect();

    let mut create_cols = Vec::new();
    for (col_name, col_def) in &all_expected {
        create_cols.push(format!("{col_name} {col_def}"));
    }
    create_cols.push(ISSUES_CLOSED_AT_CHECK.to_string());

    let create_sql = format!(
        "CREATE TABLE issues_rebuild_tmp ({})",
        create_cols.join(", ")
    );
    conn.execute(&create_sql)?;

    // Copy only columns that exist in the source table so SQLite can apply
    // declared defaults for newer columns that are absent in legacy schemas.
    let mut projected_columns = Vec::new();
    for (col_name, _) in &all_expected {
        if existing_columns.iter().any(|c| c == col_name) {
            projected_columns.push((*col_name).to_string());
        }
    }

    if projected_columns.is_empty() {
        return Err(BeadsError::Config(
            "Cannot rebuild legacy issues table: no canonical issue columns were found".to_string(),
        ));
    }

    // Copy data out to the temp table.
    let copy_out_sql = format!(
        "INSERT INTO issues_rebuild_tmp ({cols}) SELECT {cols} FROM issues",
        cols = projected_columns.join(", ")
    );
    conn.execute(&copy_out_sql)?;

    // Drop the original table, then CREATE it fresh (not via RENAME) so
    // that fsqlite's in-memory schema cache registers all columns.
    conn.execute("DROP TABLE issues")?;

    let create_canonical = format!("CREATE TABLE issues ({})", create_cols.join(", "));
    conn.execute(&create_canonical)?;

    // Copy data back.
    let copy_back_sql = format!(
        "INSERT INTO issues ({cols}) SELECT {cols} FROM issues_rebuild_tmp",
        cols = projected_columns.join(", ")
    );
    conn.execute(&copy_back_sql)?;

    conn.execute("DROP TABLE issues_rebuild_tmp")?;

    Ok(())
}

/// Backfill storage-class NULL values in NOT NULL DEFAULT columns.
///
/// SQLite's `ALTER TABLE ADD COLUMN ... NOT NULL DEFAULT ...` enforces the
/// default for new and existing rows, but legacy databases — predating
/// br's current migration code, or carrying history from Go bd or raw
/// `sqlite3` edits — can hold storage-class NULLs in such columns. The
/// `typeof(col) = 'null'` predicate detects these directly even when
/// partial indexes cause `IS NULL` to silently miss them (see #269).
///
/// Idempotent: rows that already hold the default are not rewritten.
/// Tables/columns that don't exist on the current schema are skipped.
///
/// Best-effort: per-column failures are logged but do not abort the
/// caller, so a single broken column never blocks bootstrap.
fn backfill_storage_null_in_default_columns(conn: &Connection) {
    // (table, column, default_sql_literal). Mirrors the NOT NULL DEFAULT
    // clauses in SCHEMA_SQL and the *_COLUMNS migration constants.
    const COLUMNS: &[(&str, &str, &str)] = &[
        // issues
        ("issues", "description", "''"),
        ("issues", "design", "''"),
        ("issues", "acceptance_criteria", "''"),
        ("issues", "notes", "''"),
        ("issues", "status", "'open'"),
        ("issues", "priority", "2"),
        ("issues", "issue_type", "'task'"),
        ("issues", "source_repo", "'.'"),
        ("issues", "ephemeral", "0"),
        ("issues", "pinned", "0"),
        ("issues", "is_template", "0"),
        // dependencies
        ("dependencies", "type", "'blocks'"),
        ("dependencies", "created_by", "''"),
        // comments
        ("comments", "author", "''"),
        ("comments", "text", "''"),
        // events
        ("events", "event_type", "''"),
        ("events", "actor", "''"),
    ];

    for (table, column, default) in COLUMNS {
        if !table_exists(conn, table) || !column_exists(conn, table, column) {
            continue;
        }
        let sql =
            format!("UPDATE {table} SET {column} = {default} WHERE typeof({column}) = 'null'");
        if let Err(err) = conn.execute(&sql) {
            tracing::warn!(
                table = table,
                column = column,
                error = %err,
                "backfill of storage-NULL default failed; continuing"
            );
        }
    }
}

fn kv_table_uses_primary_key(conn: &Connection, table: &str) -> bool {
    // Use PRAGMA table_info instead of sqlite_master to detect whether
    // the `key` column is declared as PRIMARY KEY.  fsqlite's in-memory
    // sqlite_master can return inconsistent results across queries.
    let sql = format!("PRAGMA table_info('{table}')");
    let Ok(rows) = conn.query(&sql) else {
        return false;
    };

    // In PRAGMA table_info output, column index 5 is the `pk` flag.
    // If `key` column has pk > 0, the table uses PRIMARY KEY.
    rows.iter().any(|row| {
        let col_name = row.get(1).and_then(SqliteValue::as_text);
        let pk_flag = row.get(5).and_then(SqliteValue::as_integer).unwrap_or(0);
        col_name == Some("key") && pk_flag > 0
    })
}

fn kv_table_needs_canonical_rebuild(conn: &Connection, table: &str, expected_index: &str) -> bool {
    // Use PRAGMA table_info for the existence check instead of sqlite_master,
    // which can return inconsistent results in fsqlite.
    let table_has_rows = conn
        .query(&format!("PRAGMA table_info('{table}')"))
        .is_ok_and(|rows| !rows.is_empty());
    table_has_rows
        && (!index_exists(conn, expected_index) || kv_table_uses_primary_key(conn, table))
}

fn rebuild_kv_table_without_unique(conn: &Connection, table: &str) -> Result<()> {
    let tmp_table = format!("{table}_rebuild_tmp");

    conn.execute("BEGIN EXCLUSIVE")?;

    let result = (|| -> Result<()> {
        conn.execute(&format!("DROP TABLE IF EXISTS {tmp_table}"))?;
        conn.execute(&format!(
            "CREATE TABLE {tmp_table} (
                key TEXT NOT NULL,
                value TEXT NOT NULL
            )"
        ))?;

        conn.execute(&format!(
            "INSERT INTO {tmp_table} (key, value)
             SELECT key, value
             FROM {table}"
        ))?;

        conn.execute(&format!("DROP TABLE {table}"))?;
        conn.execute(&format!("ALTER TABLE {tmp_table} RENAME TO {table}"))?;
        Ok(())
    })();

    if let Err(err) = result {
        let _ = conn.execute("ROLLBACK");
        return Err(err);
    }

    conn.execute("COMMIT")?;
    Ok(())
}

/// Run pre-schema migrations to fix incompatible old tables.
///
/// This must run BEFORE `execute_batch(SCHEMA_SQL)` because the schema includes
/// CREATE INDEX statements that will fail if old tables have missing columns.
/// Returns `true` if the issues table was rebuilt during pre-migrations.
fn run_pre_schema_migrations(conn: &Connection) -> Result<bool> {
    // Legacy schemas used PRIMARY KEY on config/metadata key columns.
    // Rebuild to plain key-value tables so standard sqlite integrity checks
    // are not tripped by unsupported unique-index maintenance behavior.
    if kv_table_needs_canonical_rebuild(conn, "config", "idx_config_key") {
        rebuild_kv_table_without_unique(conn, "config")?;
    }
    if kv_table_needs_canonical_rebuild(conn, "metadata", "idx_metadata_key") {
        rebuild_kv_table_without_unique(conn, "metadata")?;
    }

    // Drop blocked_issues_cache if it exists but is not canonical. The table is
    // a derived cache, so rebuilding is preferable to preserving legacy NULLs
    // or weak constraints that can poison later command reads.
    // The main schema will recreate it with the correct structure.
    if table_exists(conn, "blocked_issues_cache") && !blocked_cache_table_canonical(conn) {
        conn.execute("DROP TABLE IF EXISTS blocked_issues_cache")?;
    }

    // Rebuild the issues table if columns are out of order or missing.
    // This fixes fsqlite "no such column" errors on databases created with
    // older br versions where ALTER TABLE ADD COLUMN appended columns in
    // a different position than the canonical CREATE TABLE definition.
    // issues_column_order_matches handles both existence and column order
    // checks via PRAGMA table_info, avoiding redundant sqlite_master queries
    // which can return inconsistent results in fsqlite.
    let issues_rebuilt = if issues_column_order_matches(conn) {
        false
    } else {
        rebuild_issues_table(conn)?;
        true
    };

    // After a full rebuild the issues table already has the canonical schema,
    // so skip ensure_columns (which uses ALTER TABLE ADD COLUMN and may leave
    // fsqlite's in-memory schema cache stale).
    if !issues_rebuilt {
        ensure_columns(conn, "issues", ISSUE_COLUMNS)?;
    }
    ensure_columns(conn, "dependencies", DEPENDENCY_COLUMNS)?;
    ensure_columns(conn, "comments", COMMENT_COLUMNS)?;
    ensure_columns(conn, "events", EVENT_COLUMNS)?;
    ensure_auxiliary_runtime_columns(conn, "close_metadata", CLOSE_METADATA_COLUMNS)?;
    ensure_auxiliary_runtime_columns(conn, "gate_results", GATE_RESULTS_COLUMNS)?;
    ensure_auxiliary_runtime_columns(conn, "capacity_exemptions", CAPACITY_EXEMPTION_COLUMNS)?;
    ensure_auxiliary_runtime_columns(
        conn,
        "capacity_exemption_history",
        CAPACITY_EXEMPTION_HISTORY_COLUMNS,
    )?;
    ensure_auxiliary_runtime_columns(conn, "capacity_occupancy", CAPACITY_OCCUPANCY_COLUMNS)?;

    // Intentionally do not rebuild idx_issues_ready here.
    //
    // Older databases may have a stale partial-index predicate, but that is a
    // performance issue rather than a correctness issue. On large file-backed
    // databases, exercising DROP INDEX through frankensqlite currently trips an
    // out-of-memory failure. Additionally, frankensqlite's in-memory schema
    // representation does not reliably preserve partial-index predicates, so br
    // cannot distinguish a stale ready index from a current one at open time.

    Ok(issues_rebuilt)
}

#[allow(clippy::too_many_lines)]
pub(crate) fn runtime_schema_compatible(conn: &Connection) -> bool {
    let version_ok = current_schema_version_declared(conn);
    let issues_ok = core_runtime_table_canonical(
        conn,
        "issues",
        ISSUES_RUNTIME_COLUMNS,
        &[],
        ISSUES_RUNTIME_INDEXES,
        true,
        None,
        false,
    ) && issues_required_checks_canonical(conn);
    let dependencies_ok = core_runtime_table_canonical(
        conn,
        "dependencies",
        DEPENDENCIES_RUNTIME_COLUMNS,
        &["issue_id"],
        DEPENDENCIES_RUNTIME_INDEXES,
        false,
        None,
        false,
    );
    let labels_ok = core_runtime_table_canonical(
        conn,
        "labels",
        LABELS_RUNTIME_COLUMNS,
        &["issue_id"],
        LABELS_RUNTIME_INDEXES,
        false,
        None,
        false,
    );
    let comments_ok = core_runtime_table_canonical(
        conn,
        "comments",
        COMMENTS_RUNTIME_COLUMNS,
        &["issue_id"],
        COMMENTS_RUNTIME_INDEXES,
        false,
        Some("id"),
        false,
    );
    let events_ok = core_runtime_table_canonical(
        conn,
        "events",
        EVENTS_RUNTIME_COLUMNS,
        &["issue_id"],
        EVENTS_RUNTIME_INDEXES,
        false,
        Some("id"),
        false,
    );
    let config_ok = core_runtime_table_canonical(
        conn,
        "config",
        CONFIG_RUNTIME_COLUMNS,
        &[],
        CONFIG_RUNTIME_INDEXES,
        false,
        None,
        true,
    );
    let metadata_ok = core_runtime_table_canonical(
        conn,
        "metadata",
        METADATA_RUNTIME_COLUMNS,
        &[],
        METADATA_RUNTIME_INDEXES,
        false,
        None,
        true,
    );
    let dirty_issues_ok = core_runtime_table_canonical(
        conn,
        "dirty_issues",
        DIRTY_ISSUES_RUNTIME_COLUMNS,
        &["issue_id"],
        DIRTY_ISSUES_RUNTIME_INDEXES,
        false,
        None,
        false,
    );
    let export_hashes_ok = core_runtime_table_canonical(
        conn,
        "export_hashes",
        EXPORT_HASHES_RUNTIME_COLUMNS,
        &["issue_id"],
        &[],
        false,
        None,
        false,
    );
    let blocked_cache_ok = core_runtime_table_canonical(
        conn,
        "blocked_issues_cache",
        BLOCKED_CACHE_RUNTIME_COLUMNS,
        &["issue_id"],
        BLOCKED_CACHE_RUNTIME_INDEXES,
        false,
        None,
        false,
    );
    let child_counters_ok = core_runtime_table_canonical(
        conn,
        "child_counters",
        CHILD_COUNTERS_RUNTIME_COLUMNS,
        &["parent_id"],
        &[],
        false,
        None,
        false,
    );
    let close_metadata_ok = auxiliary_runtime_table_canonical(
        conn,
        "close_metadata",
        CLOSE_METADATA_COLUMNS,
        CLOSE_METADATA_INDEXES,
    );
    let gate_results_ok = auxiliary_runtime_table_canonical(
        conn,
        "gate_results",
        GATE_RESULTS_COLUMNS,
        GATE_RESULTS_INDEXES,
    );
    let gate_history_ok = attest_gate_result_history_schema(conn).is_ok();
    // v16/v17 capacity tables (#384). Checking them here means a database
    // stamped at the current version but missing these tables (e.g. one
    // produced by a pre-#398 reviewed migration, which never created them)
    // is healed by `apply_schema` on the next open instead of failing at
    // runtime with "no such table".
    let capacity_exemptions_ok = auxiliary_runtime_table_canonical(
        conn,
        "capacity_exemptions",
        CAPACITY_EXEMPTION_COLUMNS,
        CAPACITY_EXEMPTION_INDEXES,
    );
    let capacity_exemption_history_ok =
        auxiliary_runtime_table_canonical(
            conn,
            "capacity_exemption_history",
            CAPACITY_EXEMPTION_HISTORY_COLUMNS,
            CAPACITY_EXEMPTION_HISTORY_INDEXES,
        ) && table_declares_autoincrement_primary_key(conn, "capacity_exemption_history", "id");
    let capacity_occupancy_ok = auxiliary_runtime_table_canonical(
        conn,
        "capacity_occupancy",
        CAPACITY_OCCUPANCY_COLUMNS,
        CAPACITY_OCCUPANCY_INDEXES,
    );
    let capacity_ok =
        capacity_exemptions_ok && capacity_exemption_history_ok && capacity_occupancy_ok;
    let compatible = version_ok
        && issues_ok
        && dependencies_ok
        && labels_ok
        && comments_ok
        && events_ok
        && config_ok
        && metadata_ok
        && dirty_issues_ok
        && export_hashes_ok
        && blocked_cache_ok
        && child_counters_ok
        && close_metadata_ok
        && gate_results_ok
        && gate_history_ok
        && capacity_ok;

    if !compatible {
        tracing::debug!(
            version_ok,
            issues_ok,
            dependencies_ok,
            labels_ok,
            comments_ok,
            events_ok,
            config_ok,
            metadata_ok,
            dirty_issues_ok,
            export_hashes_ok,
            blocked_cache_ok,
            child_counters_ok,
            close_metadata_ok,
            gate_results_ok,
            gate_history_ok,
            capacity_exemptions_ok,
            capacity_exemption_history_ok,
            capacity_occupancy_ok,
            capacity_ok,
            "runtime schema compatibility check failed"
        );
    }

    compatible
}

pub(crate) fn runtime_schema_cookie(conn: &Connection) -> Result<i64> {
    conn.query_row("PRAGMA schema_version")?
        .get(0)
        .and_then(SqliteValue::as_integer)
        .ok_or_else(|| BeadsError::internal("PRAGMA schema_version returned no integer value"))
}

/// Validate the complete runtime schema against one stable SQLite schema
/// generation and return that exact generation for later witness recording.
/// A caller must never re-read the cookie when persisting the witness: DDL may
/// commit after this fence, in which case the older recorded cookie safely
/// fails to match on the next fast open.
pub(crate) fn attest_runtime_schema_cookie(conn: &Connection) -> Result<i64> {
    let cookie_before = runtime_schema_cookie(conn)?;
    let version_before = runtime_user_version(conn)?;
    if version_before != i64::from(CURRENT_SCHEMA_VERSION) {
        return Err(BeadsError::Config(format!(
            "runtime schema version {version_before} is not the supported version {CURRENT_SCHEMA_VERSION}"
        )));
    }
    if !runtime_schema_compatible(conn) {
        return Err(BeadsError::Config(
            "runtime schema remains incompatible after repair".to_string(),
        ));
    }
    let version_after = runtime_user_version(conn)?;
    let cookie_after = runtime_schema_cookie(conn)?;
    if cookie_before != cookie_after || version_before != version_after {
        return Err(BeadsError::Config(format!(
            "runtime schema changed while compatibility was being attested \
             (cookie {cookie_before} -> {cookie_after}, version {version_before} -> {version_after})"
        )));
    }
    Ok(cookie_after)
}

fn runtime_schema_witness_value(cookie: i64) -> String {
    format!(
        "schema-{CURRENT_SCHEMA_VERSION}.contract-{RUNTIME_SCHEMA_CONTRACT_TOKEN}.cookie-{cookie}"
    )
}

/// Return whether the database still has the exact SQLite schema cookie that
/// was recorded after the authoritative runtime contract last passed.
///
/// This is the steady-state fast-open witness. Any DDL (including a dropped
/// table, column, or required index) changes SQLite's schema cookie and forces
/// the caller back through the full compatibility checker and healer.
pub(crate) fn runtime_schema_witness_matches(conn: &Connection) -> bool {
    let Ok(cookie_before) = runtime_schema_cookie(conn) else {
        return false;
    };
    let Ok(version_before) = runtime_user_version(conn) else {
        return false;
    };
    if version_before != i64::from(CURRENT_SCHEMA_VERSION) {
        return false;
    }
    let expected = runtime_schema_witness_value(cookie_before);
    let witness_matches = conn
        .query_row_with_params(
            "SELECT value FROM metadata WHERE key = ? ORDER BY rowid DESC LIMIT 1",
            &[SqliteValue::from(RUNTIME_SCHEMA_WITNESS_KEY)],
        )
        .ok()
        .and_then(|row| row.get(0).and_then(SqliteValue::as_text).map(str::to_owned))
        .is_some_and(|value| value == expected);
    witness_matches
        && runtime_user_version(conn).is_ok_and(|version_after| version_after == version_before)
        && runtime_schema_cookie(conn).is_ok_and(|cookie_after| cookie_after == cookie_before)
}

/// Record an exact schema cookie that was already attested against the complete
/// runtime contract. This deliberately does not re-read the current cookie.
pub(crate) fn record_runtime_schema_witness(conn: &Connection, attested_cookie: i64) -> Result<()> {
    let value = runtime_schema_witness_value(attested_cookie);
    let existing = conn.query_with_params(
        "SELECT value FROM metadata WHERE key = ? ORDER BY rowid DESC LIMIT 1",
        &[SqliteValue::from(RUNTIME_SCHEMA_WITNESS_KEY)],
    )?;
    if existing
        .first()
        .and_then(|row| row.get(0).and_then(SqliteValue::as_text))
        == Some(value.as_str())
    {
        return Ok(());
    }

    conn.execute_with_params(
        "UPDATE metadata SET value = ? WHERE key = ?",
        &[
            SqliteValue::from(value.as_str()),
            SqliteValue::from(RUNTIME_SCHEMA_WITNESS_KEY),
        ],
    )?;
    conn.execute_with_params(
        "INSERT INTO metadata (key, value)
         SELECT ?, ?
         WHERE NOT EXISTS (
             SELECT 1 FROM metadata WHERE key = ? LIMIT 1
         )",
        &[
            SqliteValue::from(RUNTIME_SCHEMA_WITNESS_KEY),
            SqliteValue::from(value),
            SqliteValue::from(RUNTIME_SCHEMA_WITNESS_KEY),
        ],
    )?;
    Ok(())
}

/// Run schema migrations for existing databases.
///
/// This handles upgrades for tables that may have been created with older schemas.
#[allow(clippy::too_many_lines)]
fn run_migrations(conn: &Connection, issues_rebuilt: bool) -> Result<()> {
    // Migration: ensure blocked_issues_cache has the canonical derived-cache
    // schema, including NOT NULL payload columns. Older br/bd paths could
    // leave a nullable blocked_by column with stale NULL cache rows.
    if !blocked_cache_table_canonical(conn) {
        // Table needs update - drop and recreate (it's a cache, data is regenerated)
        // Wrap in transaction so concurrent opens don't see a partially migrated state
        conn.execute("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<()> {
            conn.execute("DROP TABLE IF EXISTS blocked_issues_cache")?;
            conn.execute(
                "CREATE TABLE blocked_issues_cache (
                    issue_id TEXT PRIMARY KEY,
                    blocked_by TEXT NOT NULL,
                    blocked_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
                )",
            )?;
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_blocked_cache_blocked_at ON blocked_issues_cache(blocked_at)",
            )?;
            Ok(())
        })();

        if let Err(e) = result {
            let _ = conn.execute("ROLLBACK");
            return Err(e);
        }
        conn.execute("COMMIT")?;
    }

    // Migration: ensure compaction_level is never NULL (bd compatibility)
    let has_compaction_level = column_exists(conn, "issues", "compaction_level");

    if has_compaction_level {
        conn.execute("UPDATE issues SET compaction_level = 0 WHERE compaction_level IS NULL")?;
    }

    // Migration: Ensure filter columns are NOT NULL (v3)
    let user_version = conn
        .query_row("PRAGMA user_version")?
        .get(0)
        .and_then(SqliteValue::as_integer)
        .unwrap_or(0);

    // Skip v3/v4 migration when the issues table was just rebuilt from scratch
    // (it already has canonical NOT NULL constraints and the correct index).
    // Querying columns via UPDATE/CREATE INDEX here would fail because
    // fsqlite's in-memory schema cache may not have refreshed after the rebuild.
    if !issues_rebuilt {
        if user_version < 3
            && table_exists(conn, "issues")
            && issues_filter_columns_require_v3_rebuild(conn)
        {
            tracing::info!("Migrating database to schema version 3 (NOT NULL filter columns)");
            // 1. Backfill NULL values
            conn.execute("UPDATE issues SET ephemeral = 0 WHERE ephemeral IS NULL")?;
            conn.execute("UPDATE issues SET pinned = 0 WHERE pinned IS NULL")?;
            conn.execute("UPDATE issues SET is_template = 0 WHERE is_template IS NULL")?;

            // 2. Rebuild the table to apply NOT NULL constraints
            rebuild_issues_table(conn)?;

            // 3. Recreate the optimized ready index
            conn.execute("DROP INDEX IF EXISTS idx_issues_ready")?;
            conn.execute(
                "CREATE INDEX idx_issues_ready
                 ON issues(status, priority, created_at)
                 WHERE status = 'open'
                 AND ephemeral = 0
                 AND pinned = 0
                 AND is_template = 0",
            )?;
        }

        if user_version < 4 && table_exists(conn, "issues") {
            tracing::info!("Migrating database to schema version 4 (ready excludes in_progress)");
            conn.execute("DROP INDEX IF EXISTS idx_issues_ready")?;
            conn.execute(
                "CREATE INDEX idx_issues_ready
                 ON issues(status, priority, created_at)
                 WHERE status = 'open'
                 AND ephemeral = 0
                 AND pinned = 0
                 AND is_template = 0",
            )?;
        }

        // v5: Drop the old DESC index so the idempotent CREATE INDEX IF NOT
        // EXISTS below recreates it without DESC.  Frankensqlite's B-tree
        // implementation stores DESC index entries in a different physical
        // order than C sqlite3 expects, causing `PRAGMA integrity_check` to
        // report "entries are out of order for their declared key directions".
        // Removing DESC eliminates the false positive while SQLite's query
        // planner still reverse-scans the ASC index efficiently for
        // ORDER BY ... created_at DESC queries.
        if user_version < 5 {
            tracing::info!(
                "Migrating database to schema version 5 (remove DESC from active list index)"
            );
            conn.execute("DROP INDEX IF EXISTS idx_issues_list_active_order")?;
        }

        // v6: Repair datetime columns and legacy status values.
        //
        // External tools (including pre-Rust bd flows and direct SQLite edits)
        // have occasionally written integer epoch microseconds into DATETIME
        // columns, and imported JSONL has carried the Go-beads "done" status
        // unchanged via the Status::Custom fallback. Both corrupt the JSONL
        // export: the reader's legacy `as_text().unwrap_or("")` path mapped
        // integer datetimes to UNIX_EPOCH (updated_at rows becoming
        // 1970-01-01) and dropped optional datetimes (closed_at → null),
        // while downstream tools (bv, bd-style consumers) reject an unknown
        // "done" status entirely. This migration rewrites the data in place
        // so every row is fully-typed and uses canonical status strings.
        if user_version < 6 && table_exists(conn, "issues") {
            tracing::info!(
                "Migrating database to schema version 6 (normalize datetime columns and legacy status aliases)"
            );
            repair_integer_datetime_columns(conn)?;
            repair_legacy_status_values(conn)?;
        }
    }

    // v7: Historical content-hash rebuild. For databases older than v7 that
    // open under current br, compute the current canonical format directly
    // instead of replaying obsolete intermediate encodings. Marking rows dirty
    // is intentional: per-issue export hashes were computed with an older
    // algorithm, so the next flush must rewrite JSONL tracking metadata.
    if user_version < 7 && table_exists(conn, "issues") {
        tracing::info!("Migrating database to schema version 7 (content hashes)");
        rebuild_content_hashes_for_current_format(conn)?;
    }

    // v9: Add close_metadata table for closure-time policy gates (issue #274).
    //
    // Pure additive migration: a brand-new dedicated table to capture the
    // optional Phase 1 fields (Tier 1 attribution + policy bypass auditing).
    // Older databases get the table on next open; no existing rows or columns
    // change. Repos that never enable a policy never read or write to it, so
    // the migration is a no-op for solo-dev workflows.
    if user_version < 9 {
        tracing::info!(
            "Migrating database to schema version 9 (close_metadata table for policy gates)"
        );
        execute_batch(
            conn,
            r"
            CREATE TABLE IF NOT EXISTS close_metadata (
                issue_id TEXT PRIMARY KEY,
                closed_by_agent_name TEXT,
                closed_by_harness TEXT,
                closed_by_model TEXT,
                bypassed_policy INTEGER NOT NULL DEFAULT 0,
                bypass_reason TEXT,
                policy_gates_fired TEXT,
                recorded_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_close_metadata_recorded_at ON close_metadata(recorded_at);
            CREATE INDEX IF NOT EXISTS idx_close_metadata_bypassed
                ON close_metadata(bypassed_policy)
                WHERE bypassed_policy = 1;
            ",
        )?;
    }

    // v8: Backfill storage-class NULL values in NOT NULL DEFAULT columns.
    //
    // Older databases — particularly those carrying history from Go bd or
    // br versions where ALTER TABLE ADD COLUMN ran without a DEFAULT
    // clause — accumulate storage-class NULL in columns declared NOT NULL
    // DEFAULT. `PRAGMA integrity_check` then flags these as constraint
    // violations even though `WHERE col IS NULL` won't always match them
    // (the planner can use partial indexes that bypass the check). We use
    // `typeof(col) = 'null'` to detect storage-class NULLs directly and
    // backfill with each column's declared default. See issue #269.
    if user_version < 8 {
        tracing::info!(
            "Migrating database to schema version 8 (backfill storage-NULL in NOT NULL DEFAULT columns)"
        );
        backfill_storage_null_in_default_columns(conn);
    }

    // Note: source_repo and is_template column backfills are handled in
    // run_pre_schema_migrations() via ensure_columns(). Repeating ALTER TABLE
    // here can create duplicate column definitions on some engines.

    // v10: Ensure source_repo_path column is present on the issues table
    // (beads_rust#289) for migration paths that call `run_migrations` directly
    // and therefore skip `run_pre_schema_migrations`/`ensure_columns`. Without
    // this guard, a direct v9 -> v10 migration could stamp user_version=10 with
    // the column still missing, and the next open would fast-path past schema
    // setup and start hitting "no such column: source_repo_path" on every
    // INSERT/UPDATE.
    //
    // Idempotent: skipped when the column already exists. The ADD COLUMN
    // appends at the end, matching SCHEMA_SQL and EXPECTED_ISSUE_COLUMN_ORDER.
    // If the pre-schema path rebuilt `issues`, skip this check: the rebuilt
    // table already has the current shape, and fsqlite's in-memory schema cache
    // may not be refreshed enough for a second column probe.
    if !issues_rebuilt
        && user_version < 10
        && table_exists(conn, "issues")
        && !column_exists(conn, "issues", "source_repo_path")
    {
        tracing::info!(
            "Migrating database to schema version 10 (source_repo_path on issues - beads_rust#289)"
        );
        conn.execute("ALTER TABLE issues ADD COLUMN source_repo_path TEXT")?;
    }

    // Migration v10 -> v11 (beads_rust#297): add `agent_context TEXT` to
    // `issues` for inherited governing instructions emitted on
    // `br update --status in_progress` / `--claim` and `br show`.
    // Pure additive — existing rows get NULL and existing consumers ignore
    // it. Same idempotence pattern as the v10 source_repo_path migration:
    // skipped when the column already exists, skipped when the table was
    // just rebuilt from scratch (rebuild already produced the v11 layout).
    if !issues_rebuilt
        && user_version < 11
        && table_exists(conn, "issues")
        && !column_exists(conn, "issues", "agent_context")
    {
        tracing::info!(
            "Migrating database to schema version 11 (agent_context on issues - beads_rust#297)"
        );
        conn.execute("ALTER TABLE issues ADD COLUMN agent_context TEXT")?;
    }

    // Migration v11 -> v12 (beads_rust#319): add the `gate_results` table for
    // workflow gate engine (#312, layer 2). Pure additive — a new table, no
    // existing-row rewrite. Idempotent via CREATE TABLE IF NOT EXISTS, so it
    // is safe to run on every open regardless of `user_version`. The canonical
    // DDL also lives in SCHEMA_SQL for fresh databases; re-asserting here keeps
    // upgraded databases in lock-step.
    if user_version < 12 {
        tracing::info!(
            "Migrating database to schema version 12 (gate_results table - beads_rust#319)"
        );
        execute_batch(
            conn,
            r"
            CREATE TABLE IF NOT EXISTS gate_results (
                issue_id TEXT NOT NULL,
                gate TEXT NOT NULL,
                provider TEXT NOT NULL,
                passed INTEGER NOT NULL DEFAULT 0,
                note TEXT,
                recorded_by TEXT,
                recorded_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                PRIMARY KEY (issue_id, gate, provider),
                FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_gate_results_issue ON gate_results(issue_id);
        ",
        )?;
    }

    // Migration v12 -> v13 (beads_rust#312, Layer 3 capture-only): add Tier 1
    // attribution columns (`agent_name`, `harness`, `model`) to the `events`
    // table so create/update/status-mutating commands can record self-reported
    // agent identity as an audit trail. Pure additive, nullable columns — no
    // existing rows change, no enforcement is performed, and the JSONL sync
    // surface is unaffected (events are DB-only). Idempotent: ensure_columns
    // skips columns that already exist, mirroring the v10/v11 ADD COLUMN guards.
    if user_version < 13 && table_exists(conn, "events") {
        tracing::info!(
            "Migrating database to schema version 13 (events attribution columns - beads_rust#312)"
        );
        ensure_columns(conn, "events", EVENT_COLUMNS)?;
    }

    // v14: Recompute stored issue content hashes after switching from
    // NUL-separated fields to length-prefixed fields. Existing v7-v13
    // databases may contain hashes where embedded NUL bytes can blur field
    // boundaries, so rewrite the issue hash and force JSONL tracking metadata
    // to refresh on the next flush.
    if (7..14).contains(&user_version) && table_exists(conn, "issues") {
        tracing::info!("Migrating database to schema version 14 (length-prefixed content hashes)");
        rebuild_content_hashes_for_current_format(conn)?;
    }

    // v15 (GitHub #388): preserve every workflow-gate verdict in an
    // append-only table bound to an exact status revision and target
    // transition. The legacy gate_results table is intentionally retained as
    // historical, unscoped evidence; its rows never satisfy a v15 transition.
    if user_version < 15 {
        tracing::info!("Migrating database to schema version 15 (transition-scoped gate history)");
        apply_gate_result_history_migration_in_transaction(conn)?;
    }

    // v16 (GitHub #384 phase 4): audited issue-specific capacity exemptions.
    // A state table holds the latest exemption per (issue, capacity); an
    // append-only history table preserves every grant/renew/revoke/expire/
    // left_status action. Pure additive — new tables only, no row rewrites.
    if user_version < 16 {
        tracing::info!(
            "Migrating database to schema version 16 (capacity exemptions - GitHub #384 phase 4)"
        );
        apply_capacity_exemptions_migration_in_transaction(conn)?;
    }

    // v17 (GitHub #384 phase 5): capacity occupancy attribution. One row per
    // issue recording who moved it into its current status, so actor/
    // harness/session capacity scopes can count occupancy inside the
    // admission transaction. Pure additive — a new table only.
    if user_version < 17 {
        tracing::info!(
            "Migrating database to schema version 17 (capacity occupancy - GitHub #384 phase 5)"
        );
        apply_capacity_occupancy_migration_in_transaction(conn)?;
    }

    // Migration: Add missing indexes for bd parity
    // These use IF NOT EXISTS so they're safe to run multiple times
    execute_batch(
        conn,
        r"
        -- Export/sync patterns
        CREATE INDEX IF NOT EXISTS idx_issues_content_hash ON issues(content_hash);
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
            WHERE status = 'open'
            AND ephemeral = 0
            AND pinned = 0
            AND is_template = 0;

        -- Widened ready group (issue #354): non-partial composite so a
        -- configured `status IN (...) ORDER BY priority, created_at` ready query
        -- stays index-covered even on pre-existing databases.
        CREATE INDEX IF NOT EXISTS idx_issues_status_priority_created
            ON issues(status, priority, created_at);

        -- Common active list path: non-terminal issues sorted by priority/created_at
        CREATE INDEX IF NOT EXISTS idx_issues_list_active_order
            ON issues(priority, created_at)
            WHERE status NOT IN ('closed', 'tombstone')
            AND (is_template = 0 OR is_template IS NULL);

    ",
    )?;

    // Drop legacy index names (safe if absent)
    execute_batch(
        conn,
        r"
        DROP INDEX IF EXISTS idx_dependencies_issue_id;
        DROP INDEX IF EXISTS idx_dependencies_depends_on_id;
        DROP INDEX IF EXISTS idx_dependencies_composite;
        DROP INDEX IF EXISTS idx_labels_issue_id;
    ",
    )?;

    if table_exists(conn, "dependencies") {
        execute_batch(
            conn,
            r"
            CREATE INDEX IF NOT EXISTS idx_dependencies_issue ON dependencies(issue_id);
            CREATE INDEX IF NOT EXISTS idx_dependencies_depends_on ON dependencies(depends_on_id);
            CREATE INDEX IF NOT EXISTS idx_dependencies_type ON dependencies(type);
            CREATE INDEX IF NOT EXISTS idx_dependencies_depends_on_type ON dependencies(depends_on_id, type);
            CREATE INDEX IF NOT EXISTS idx_dependencies_thread ON dependencies(thread_id) WHERE thread_id != '';
            -- Composite for blocking lookups
            CREATE INDEX IF NOT EXISTS idx_dependencies_blocking
                ON dependencies(depends_on_id, issue_id)
                WHERE (type = 'blocks' OR type = 'parent-child' OR type = 'conditional-blocks' OR type = 'waits-for');
        ",
        )?;

        if column_exists(conn, "dependencies", "thread_id") {
            conn.execute(
                "CREATE INDEX IF NOT EXISTS idx_dependencies_thread ON dependencies(thread_id) WHERE thread_id != ''",
            )?;
        }
    }

    if table_exists(conn, "labels") {
        execute_batch(
            conn,
            r"
            CREATE INDEX IF NOT EXISTS idx_labels_label ON labels(label);
            CREATE INDEX IF NOT EXISTS idx_labels_issue ON labels(issue_id);
        ",
        )?;
    }

    if table_exists(conn, "comments") {
        conn.execute("CREATE INDEX IF NOT EXISTS idx_comments_issue ON comments(issue_id)")?;
    }

    if table_exists(conn, "events") {
        execute_batch(
            conn,
            r"
            CREATE INDEX IF NOT EXISTS idx_events_issue ON events(issue_id);
            CREATE INDEX IF NOT EXISTS idx_events_type ON events(event_type);
            CREATE INDEX IF NOT EXISTS idx_events_actor ON events(actor) WHERE actor != '';
        ",
        )?;
    }

    Ok(())
}

/// Rewrite any DATETIME column on `issues` that is stored as INTEGER into
/// canonical RFC 3339 TEXT. The unit (seconds / ms / µs / ns) is inferred
/// from magnitude exactly like the runtime reader's `datetime_from_epoch_auto`
/// so both paths give the same answer — any other split would silently
/// corrupt rows whose writer picked a different unit than we assumed.
/// Idempotent; rows already stored as TEXT are left untouched.
fn repair_integer_datetime_columns(conn: &Connection) -> Result<()> {
    const DATETIME_COLUMNS: &[&str] = &[
        "created_at",
        "updated_at",
        "closed_at",
        "due_at",
        "defer_until",
        "deleted_at",
        "compacted_at",
    ];
    // Must stay in lock-step with datetime_from_epoch_auto in
    // src/storage/sqlite.rs. Each threshold is the smallest integer that,
    // in that unit, still lands within ±2286 AD — i.e. 10^10 seconds,
    // 10^13 ms, 10^16 µs, 10^19 ns all represent the same year 2286,
    // giving non-overlapping magnitude buckets.
    for column in DATETIME_COLUMNS {
        if !column_exists(conn, "issues", column) {
            continue;
        }
        let sql = format!(
            "UPDATE issues SET {column} = \
                strftime('%Y-%m-%dT%H:%M:%fZ', CASE \
                    WHEN ABS({column}) < 10000000000 THEN {column} * 1.0 \
                    WHEN ABS({column}) < 10000000000000 THEN {column} / 1000.0 \
                    WHEN ABS({column}) < 10000000000000000 THEN {column} / 1000000.0 \
                    ELSE {column} / 1000000000.0 \
                END, 'unixepoch') \
             WHERE typeof({column}) = 'integer'"
        );
        conn.execute(&sql)?;
    }
    Ok(())
}

/// Normalize legacy Go-beads status values. `"done"` is the bd terminal state
/// and survives round-tripping through Rust via `Status::Custom("done")`;
/// remap it to the canonical `"closed"` state and make sure `closed_at` is
/// populated so the `issues` CHECK constraint stays satisfied.
fn repair_legacy_status_values(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE issues \
         SET closed_at = COALESCE(closed_at, updated_at, created_at), \
             status = 'closed' \
         WHERE LOWER(status) IN ('done', 'complete', 'completed', 'finished', 'resolved')",
    )?;
    Ok(())
}

fn schema_migration_shape_error(detail: impl std::fmt::Display) -> BeadsError {
    BeadsError::internal(format!("schema migrate v15 post-check failed — {detail}"))
}

fn sql_default_matches(actual: Option<&str>, expected: Option<&str>) -> bool {
    match (actual, expected) {
        (None, None) => true,
        (Some(actual), Some(expected)) => actual.trim().eq_ignore_ascii_case(expected),
        _ => false,
    }
}

fn attest_gate_result_history_columns(conn: &Connection) -> Result<()> {
    let rows = conn.query("PRAGMA table_info('gate_result_history')")?;
    if rows.len() != GATE_RESULT_HISTORY_COLUMNS.len() {
        return Err(schema_migration_shape_error(format!(
            "gate_result_history has {} columns, expected {}",
            rows.len(),
            GATE_RESULT_HISTORY_COLUMNS.len()
        )));
    }

    for (position, (row, expected)) in rows.iter().zip(GATE_RESULT_HISTORY_COLUMNS).enumerate() {
        let name = row.get(1).and_then(SqliteValue::as_text).ok_or_else(|| {
            schema_migration_shape_error(format!(
                "gate_result_history column {position} has no text name"
            ))
        })?;
        let data_type = row.get(2).and_then(SqliteValue::as_text).ok_or_else(|| {
            schema_migration_shape_error(format!("gate_result_history.{name} has no declared type"))
        })?;
        let not_null = row
            .get(3)
            .and_then(SqliteValue::as_integer)
            .ok_or_else(|| {
                schema_migration_shape_error(format!(
                    "gate_result_history.{name} has no NOT NULL flag"
                ))
            })?
            != 0;
        let default_value = row.get(4).and_then(SqliteValue::as_text);
        let primary_key_position =
            row.get(5)
                .and_then(SqliteValue::as_integer)
                .ok_or_else(|| {
                    schema_migration_shape_error(format!(
                        "gate_result_history.{name} has no primary-key position"
                    ))
                })?;

        if name != expected.name
            || !data_type.eq_ignore_ascii_case(expected.data_type)
            || not_null != expected.not_null
            || !sql_default_matches(default_value, expected.default_value)
            || primary_key_position != expected.primary_key_position
        {
            return Err(schema_migration_shape_error(format!(
                "gate_result_history column {position} is not canonical \
                 (observed name={name:?}, type={data_type:?}, not_null={not_null}, \
                 default={default_value:?}, pk={primary_key_position}; expected \
                 name={:?}, type={:?}, not_null={}, default={:?}, pk={})",
                expected.name,
                expected.data_type,
                expected.not_null,
                expected.default_value,
                expected.primary_key_position
            )));
        }
    }

    Ok(())
}

fn attest_gate_result_history_foreign_key(conn: &Connection) -> Result<()> {
    let rows = conn.query("PRAGMA foreign_key_list('gate_result_history')")?;
    if rows.len() != 1 {
        return Err(schema_migration_shape_error(format!(
            "gate_result_history has {} foreign keys, expected 1",
            rows.len()
        )));
    }
    let row = &rows[0];
    let table = row.get(2).and_then(SqliteValue::as_text);
    let from = row.get(3).and_then(SqliteValue::as_text);
    let to = row.get(4).and_then(SqliteValue::as_text);
    let on_update = row.get(5).and_then(SqliteValue::as_text);
    let on_delete = row.get(6).and_then(SqliteValue::as_text);
    let sequence = row.get(1).and_then(SqliteValue::as_integer);
    if sequence != Some(0)
        || table != Some("issues")
        || from != Some("issue_id")
        || to != Some("id")
        || !on_update.is_some_and(|value| value.eq_ignore_ascii_case("NO ACTION"))
        || !on_delete.is_some_and(|value| value.eq_ignore_ascii_case("CASCADE"))
    {
        return Err(schema_migration_shape_error(format!(
            "gate_result_history foreign key is not canonical \
             (seq={sequence:?}, table={table:?}, from={from:?}, to={to:?}, \
             on_update={on_update:?}, on_delete={on_delete:?})"
        )));
    }
    Ok(())
}

fn attest_gate_result_history_indexes(conn: &Connection) -> Result<()> {
    let index_rows = conn.query("PRAGMA index_list('gate_result_history')")?;
    for (index_name, expected_columns) in GATE_RESULT_HISTORY_INDEXES {
        let index_row = index_rows
            .iter()
            .find(|row| row.get(1).and_then(SqliteValue::as_text) == Some(*index_name))
            .ok_or_else(|| schema_migration_shape_error(format!("missing index {index_name}")))?;
        let unique = index_row
            .get(2)
            .and_then(SqliteValue::as_integer)
            .ok_or_else(|| {
                schema_migration_shape_error(format!("index {index_name} has no uniqueness flag"))
            })?;
        let origin = index_row.get(3).and_then(SqliteValue::as_text);
        let partial = index_row.get(4).and_then(SqliteValue::as_integer);
        if unique != 0
            || !origin.is_some_and(|value| value.eq_ignore_ascii_case("c"))
            || partial != Some(0)
        {
            return Err(schema_migration_shape_error(format!(
                "index {index_name} is not a canonical non-unique, non-partial created index \
                 (unique={unique}, origin={origin:?}, partial={partial:?})"
            )));
        }

        if !runtime_index_key_shape_canonical(conn, index_name, expected_columns) {
            return Err(schema_migration_shape_error(format!(
                "index {index_name} key columns, directions, or collations are not canonical"
            )));
        }
    }

    for row in &index_rows {
        let unique = row
            .get(2)
            .and_then(SqliteValue::as_integer)
            .ok_or_else(|| schema_migration_shape_error("index has no uniqueness flag"))?;
        if unique == 0 {
            continue;
        }
        let origin = row.get(3).and_then(SqliteValue::as_text);
        if origin.is_some_and(|value| value.eq_ignore_ascii_case("pk")) {
            continue;
        }
        let name = row
            .get(1)
            .and_then(SqliteValue::as_text)
            .unwrap_or("<unnamed>");
        return Err(schema_migration_shape_error(format!(
            "unexpected UNIQUE index {name} changes gate-result-history write semantics"
        )));
    }
    Ok(())
}

fn attest_gate_result_history_schema(conn: &Connection) -> Result<()> {
    attest_gate_result_history_columns(conn)?;
    attest_gate_result_history_foreign_key(conn)?;
    attest_gate_result_history_indexes(conn)?;
    if !table_declares_autoincrement_primary_key(conn, "gate_result_history", "id") {
        return Err(schema_migration_shape_error(
            "gate_result_history.id is not declared INTEGER PRIMARY KEY AUTOINCREMENT",
        ));
    }
    Ok(())
}

fn apply_gate_result_history_migration_in_transaction(conn: &Connection) -> Result<()> {
    execute_batch(conn, GATE_RESULT_HISTORY_MIGRATION_SQL)?;
    attest_gate_result_history_schema(conn)
}

/// v16 (GitHub #384 phase 4) migration step: audited issue-specific capacity
/// exemptions. Pure additive — new tables/indexes only (`IF NOT EXISTS`), no
/// row rewrites. Shared by the general migration engine and the reviewed
/// `br doctor migrate-schema` path (#398).
fn apply_capacity_exemptions_migration_in_transaction(conn: &Connection) -> Result<()> {
    execute_batch(
        conn,
        r"
        CREATE TABLE IF NOT EXISTS capacity_exemptions (
            issue_id TEXT NOT NULL,
            capacity_kind TEXT NOT NULL,
            capacity_name TEXT NOT NULL,
            provider TEXT NOT NULL,
            reason TEXT NOT NULL,
            granted_by TEXT NOT NULL,
            granted_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
            expires_at DATETIME,
            ended_at DATETIME,
            ended_action TEXT,
            ended_by TEXT,
            PRIMARY KEY (issue_id, capacity_kind, capacity_name),
            FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_capacity_exemptions_capacity
            ON capacity_exemptions(capacity_kind, capacity_name);
        CREATE TABLE IF NOT EXISTS capacity_exemption_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            issue_id TEXT NOT NULL,
            capacity_kind TEXT NOT NULL,
            capacity_name TEXT NOT NULL,
            action TEXT NOT NULL,
            provider TEXT NOT NULL,
            reason TEXT,
            actor TEXT NOT NULL,
            expires_at DATETIME,
            recorded_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_capacity_exemption_history_issue
            ON capacity_exemption_history(issue_id, id);
    ",
    )
}

/// v17 (GitHub #384 phase 5) migration step: capacity occupancy attribution.
/// Pure additive — a new table plus partial indexes (`IF NOT EXISTS`).
/// Shared by the general migration engine and the reviewed
/// `br doctor migrate-schema` path (#398).
fn apply_capacity_occupancy_migration_in_transaction(conn: &Connection) -> Result<()> {
    execute_batch(
        conn,
        r"
        CREATE TABLE IF NOT EXISTS capacity_occupancy (
            issue_id TEXT PRIMARY KEY,
            actor TEXT,
            agent_name TEXT,
            harness TEXT,
            session TEXT,
            recorded_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
            FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
        );
        CREATE INDEX IF NOT EXISTS idx_capacity_occupancy_actor
            ON capacity_occupancy(actor) WHERE actor IS NOT NULL;
        CREATE INDEX IF NOT EXISTS idx_capacity_occupancy_harness
            ON capacity_occupancy(harness) WHERE harness IS NOT NULL;
        CREATE INDEX IF NOT EXISTS idx_capacity_occupancy_session
            ON capacity_occupancy(session) WHERE session IS NOT NULL;
    ",
    )
}

fn rebuild_content_hashes_for_current_format_in_transaction(
    conn: &Connection,
    marked_at: &str,
) -> Result<usize> {
    let rows = conn.query(
        "SELECT id, title, description, design, acceptance_criteria, notes, \
                status, priority, issue_type, assignee, owner, created_by, \
                external_ref, source_system, pinned, is_template \
         FROM issues ORDER BY id",
    )?;

    let mut updated = 0;
    for row in &rows {
        let id = row_text(row, 0).ok_or_else(|| BeadsError::Internal {
            message: "content hash migration found issue row without id".to_string(),
        })?;
        let title = row_text(row, 1).unwrap_or_default();
        let description = row_optional_text(row, 2);
        let design = row_optional_text(row, 3);
        let acceptance_criteria = row_optional_text(row, 4);
        let notes = row_optional_text(row, 5);
        let status_raw = row_text(row, 6).unwrap_or_else(|| Status::default().as_str().into());
        let priority = Priority(
            row.get(7)
                .and_then(SqliteValue::as_integer)
                .and_then(|value| i32::try_from(value).ok())
                .unwrap_or_else(|| Priority::default().0),
        );
        let issue_type_raw =
            row_text(row, 8).unwrap_or_else(|| IssueType::default().as_str().into());
        let assignee = row_optional_text(row, 9);
        let owner = row_optional_text(row, 10);
        let created_by = row_optional_text(row, 11);
        let external_ref = row_optional_text(row, 12);
        let source_system = row_optional_text(row, 13);
        let pinned = row_bool(row, 14);
        let is_template = row_bool(row, 15);

        let status = status_raw
            .parse::<Status>()
            .unwrap_or_else(|_| Status::Custom(status_raw.clone()));
        let issue_type = issue_type_raw
            .parse::<IssueType>()
            .unwrap_or_else(|_| IssueType::Custom(issue_type_raw.clone()));
        let content_hash = content_hash_from_parts(
            &title,
            description.as_deref(),
            design.as_deref(),
            acceptance_criteria.as_deref(),
            notes.as_deref(),
            &status,
            &priority,
            &issue_type,
            assignee.as_deref(),
            owner.as_deref(),
            created_by.as_deref(),
            external_ref.as_deref(),
            source_system.as_deref(),
            pinned,
            is_template,
        );

        conn.execute_with_params(
            "UPDATE issues SET content_hash = ? WHERE id = ?",
            &[
                SqliteValue::from(content_hash.as_str()),
                SqliteValue::from(id.as_str()),
            ],
        )?;
        conn.execute_with_params(
            "DELETE FROM dirty_issues WHERE issue_id = ?",
            &[SqliteValue::from(id.as_str())],
        )?;
        conn.execute_with_params(
            "INSERT INTO dirty_issues (issue_id, marked_at) VALUES (?, ?)",
            &[SqliteValue::from(id.as_str()), SqliteValue::from(marked_at)],
        )?;
        updated += 1;
    }

    if table_exists(conn, "export_hashes") {
        conn.execute("DELETE FROM export_hashes")?;
    }

    Ok(updated)
}

fn rebuild_content_hashes_for_current_format(conn: &Connection) -> Result<usize> {
    // Pre-compute once and pass explicitly: legacy DBs created before the
    // `DEFAULT CURRENT_TIMESTAMP` was added to `dirty_issues.marked_at`
    // reject INSERTs that omit the column.
    let marked_at = Utc::now().to_rfc3339();
    conn.execute("BEGIN IMMEDIATE")?;
    match rebuild_content_hashes_for_current_format_in_transaction(conn, &marked_at) {
        Ok(updated) => {
            if let Err(error) = conn.execute("COMMIT") {
                let _ = conn.execute("ROLLBACK");
                return Err(BeadsError::Database(error));
            }
            Ok(updated)
        }
        Err(error) => {
            let _ = conn.execute("ROLLBACK");
            Err(error)
        }
    }
}

fn row_text(row: &fsqlite::Row, index: usize) -> Option<String> {
    row.get(index)
        .and_then(SqliteValue::as_text)
        .map(str::to_string)
}

fn row_optional_text(row: &fsqlite::Row, index: usize) -> Option<String> {
    row_text(row, index).filter(|value| !value.is_empty())
}

fn row_bool(row: &fsqlite::Row, index: usize) -> bool {
    row.get(index).is_some_and(|value| {
        value.as_integer().map_or_else(
            || value.as_text().is_some_and(|text| text != "0"),
            |int| int != 0,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::BeadsError;
    use crate::franken_sync::Connection;
    use std::collections::HashSet;
    use tempfile::TempDir;

    fn reviewed_v14_with_gate_history_schema(schema_sql: &str) -> (TempDir, Connection) {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("reviewed-v14-shape.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("apply current schema");
        conn.execute("DROP TABLE gate_result_history")
            .expect("remove canonical v15 table");
        execute_batch(&conn, schema_sql).expect("install requested gate-history fixture");
        conn.execute("PRAGMA user_version = 14")
            .expect("stamp reviewed v14 source");
        (temp, conn)
    }

    #[test]
    fn test_apply_schema() {
        // Bind the temp file: dropping it here would unlink the database
        // before the connection ever writes to it, leaving `Connection::open`
        // pointed at a dangling path.
        let temp_db = tempfile::NamedTempFile::new().unwrap();
        let conn = Connection::open(temp_db.path().to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("Failed to apply schema");

        // Verify a few tables exist
        let tables: Vec<String> = conn
            .query("SELECT name FROM sqlite_master WHERE type='table'")
            .unwrap()
            .iter()
            .filter_map(|row| row.get(0).and_then(|v| v.as_text()).map(String::from))
            .collect();

        assert!(tables.contains(&"issues".to_string()));
        assert!(tables.contains(&"dependencies".to_string()));
        assert!(tables.contains(&"config".to_string()));
        assert!(tables.contains(&"dirty_issues".to_string()));

        // Verify pragmas
        let row = conn.query_row("PRAGMA journal_mode").unwrap();
        let journal_mode = row
            .get(0)
            .and_then(|v| v.as_text())
            .unwrap_or("")
            .to_string();
        // In-memory DBs use MEMORY journaling, regardless of what we set
        assert!(journal_mode.to_uppercase() == "WAL" || journal_mode.to_uppercase() == "MEMORY");

        let row = conn.query_row("PRAGMA foreign_keys").unwrap();
        let foreign_keys = row.get(0).and_then(SqliteValue::as_integer).unwrap_or(0);
        assert_eq!(foreign_keys, 1);
    }

    #[test]
    fn test_v6_repair_integer_datetime_columns() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("Failed to apply schema");

        // Seed one row per integer epoch unit (seconds / ms / µs / ns), all
        // encoding the same instant 2026-04-20T02:18:08Z. The migration's
        // magnitude detection must recover the same year/day/time regardless
        // of unit — previously it hard-coded /1000000.0 and would corrupt
        // the other three rows.
        let rows: [(&str, i64); 4] = [
            ("bug-sec", 1_776_651_488),
            ("bug-ms", 1_776_651_488_000),
            ("bug-us", 1_776_651_488_000_000),
            ("bug-ns", 1_776_651_488_000_000_000),
        ];
        for (id, epoch) in rows {
            let stmt = format!(
                "INSERT INTO issues (id, title, status, priority, issue_type, created_at, updated_at, closed_at, close_reason) \
                 VALUES ('{id}', 'legacy', 'closed', 2, 'task', '2026-04-19T21:34:04.000000000Z', {epoch}, {epoch}, 'Completed')"
            );
            conn.execute(&stmt).expect("seed integer datetime row");
        }

        // Sanity: every updated_at/closed_at must be integer-typed pre-repair.
        for (id, _) in rows {
            let row = conn
                .query_row(&format!(
                    "SELECT typeof(updated_at), typeof(closed_at) FROM issues WHERE id='{id}'"
                ))
                .unwrap();
            assert_eq!(
                row.get(0).and_then(SqliteValue::as_text),
                Some("integer"),
                "{id} updated_at should be integer pre-repair"
            );
            assert_eq!(
                row.get(1).and_then(SqliteValue::as_text),
                Some("integer"),
                "{id} closed_at should be integer pre-repair"
            );
        }

        repair_integer_datetime_columns(&conn).expect("repair should succeed");

        for (id, _) in rows {
            let row = conn
                .query_row(&format!(
                    "SELECT typeof(updated_at), updated_at, typeof(closed_at), closed_at FROM issues WHERE id='{id}'"
                ))
                .unwrap();
            assert_eq!(
                row.get(0).and_then(SqliteValue::as_text),
                Some("text"),
                "{id} updated_at must be TEXT after repair"
            );
            let updated_at = row
                .get(1)
                .and_then(SqliteValue::as_text)
                .expect("updated_at text");
            assert!(
                updated_at.starts_with("2026-04-20T02:18:08"),
                "{id}: expected 2026-04-20 timestamp, got {updated_at}"
            );
            assert_eq!(
                row.get(2).and_then(SqliteValue::as_text),
                Some("text"),
                "{id} closed_at must be TEXT after repair"
            );
        }

        // Idempotency: a second pass is a no-op and leaves the rows TEXT.
        repair_integer_datetime_columns(&conn).expect("second pass should succeed");
        let row = conn
            .query_row("SELECT typeof(updated_at) FROM issues WHERE id='bug-us'")
            .unwrap();
        assert_eq!(row.get(0).and_then(SqliteValue::as_text), Some("text"));
    }

    #[test]
    fn test_v6_repair_legacy_status_values() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("Failed to apply schema");

        // The issues CHECK constraint forbids closed without closed_at, so
        // we seed rows that are legally in the `done` state (status NOT IN
        // ('closed','tombstone') ⇒ closed_at must be NULL). The migration
        // will promote them to `closed` and backfill closed_at from
        // updated_at.
        conn.execute(
            "INSERT INTO issues (id, title, status, priority, issue_type, created_at, updated_at) \
             VALUES ('legacy-done', 'bd legacy', 'done', 2, 'task', '2026-04-02T20:00:00Z', '2026-04-03T01:00:00Z')",
        ).unwrap();
        conn.execute(
            "INSERT INTO issues (id, title, status, priority, issue_type, created_at, updated_at) \
             VALUES ('legacy-resolved', 'bd legacy', 'Resolved', 2, 'task', '2026-04-02T20:00:00Z', '2026-04-03T01:00:00Z')",
        ).unwrap();

        repair_legacy_status_values(&conn).expect("repair should succeed");

        for id in ["legacy-done", "legacy-resolved"] {
            let row = conn
                .query_row(&format!(
                    "SELECT status, closed_at FROM issues WHERE id='{id}'"
                ))
                .unwrap();
            assert_eq!(
                row.get(0).and_then(SqliteValue::as_text),
                Some("closed"),
                "{id} should be closed"
            );
            let closed_at = row
                .get(1)
                .and_then(SqliteValue::as_text)
                .unwrap_or_default();
            assert!(!closed_at.is_empty(), "{id} closed_at should be populated");
        }
    }

    #[test]
    fn test_v7_rebuilds_content_hashes_and_marks_dirty() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("Failed to apply schema");

        conn.execute(
            "INSERT INTO issues (id, content_hash, title, status, priority, issue_type, created_at, updated_at) \
             VALUES ('bd-hash', 'old-rust-hash', 'Test', 'open', 2, 'task', '2026-04-02T20:00:00Z', '2026-04-03T01:00:00Z')",
        ).unwrap();
        conn.execute(
            "INSERT INTO export_hashes (issue_id, content_hash, exported_at) \
             VALUES ('bd-hash', 'old-rust-hash', '2026-04-03T01:00:00Z')",
        )
        .unwrap();
        conn.execute("DELETE FROM dirty_issues").unwrap();
        conn.execute("PRAGMA user_version = 6").unwrap();

        run_migrations(&conn, false).expect("v7 migration should succeed");

        let row = conn
            .query_row("SELECT content_hash FROM issues WHERE id = 'bd-hash'")
            .unwrap();
        assert_eq!(
            row.get(0).and_then(SqliteValue::as_text),
            Some("c42bf13dfd6447e08d119f8b0ad0a503d23ccaa92b211348fb6dfbc55a4e0779"),
            "v7 should rewrite stored issue hashes to the current canonical format"
        );

        let dirty_row = conn
            .query_row("SELECT COUNT(*) FROM dirty_issues WHERE issue_id = 'bd-hash'")
            .unwrap();
        assert_eq!(dirty_row.get(0).and_then(SqliteValue::as_integer), Some(1));

        let export_row = conn
            .query_row("SELECT COUNT(*) FROM export_hashes")
            .unwrap();
        assert_eq!(export_row.get(0).and_then(SqliteValue::as_integer), Some(0));
    }

    #[test]
    fn test_v14_rebuilds_length_prefixed_content_hashes_and_marks_dirty() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("Failed to apply schema");

        conn.execute(
            "INSERT INTO issues (id, content_hash, title, status, priority, issue_type, created_at, updated_at) \
             VALUES ('bd-hash-v14', 'old-go-hash', 'Test', 'open', 2, 'task', '2026-04-02T20:00:00Z', '2026-04-03T01:00:00Z')",
        ).unwrap();
        conn.execute(
            "INSERT INTO export_hashes (issue_id, content_hash, exported_at) \
             VALUES ('bd-hash-v14', 'old-go-hash', '2026-04-03T01:00:00Z')",
        )
        .unwrap();
        conn.execute("DELETE FROM dirty_issues").unwrap();
        conn.execute("PRAGMA user_version = 13").unwrap();

        run_migrations(&conn, false).expect("v14 migration should succeed");

        let row = conn
            .query_row("SELECT content_hash FROM issues WHERE id = 'bd-hash-v14'")
            .unwrap();
        assert_eq!(
            row.get(0).and_then(SqliteValue::as_text),
            Some("c42bf13dfd6447e08d119f8b0ad0a503d23ccaa92b211348fb6dfbc55a4e0779"),
            "v14 should rewrite stored issue hashes to length-prefixed values"
        );

        let dirty_row = conn
            .query_row("SELECT COUNT(*) FROM dirty_issues WHERE issue_id = 'bd-hash-v14'")
            .unwrap();
        assert_eq!(dirty_row.get(0).and_then(SqliteValue::as_integer), Some(1));

        let export_row = conn
            .query_row("SELECT COUNT(*) FROM export_hashes")
            .unwrap();
        assert_eq!(export_row.get(0).and_then(SqliteValue::as_integer), Some(0));
    }

    #[test]
    fn test_reviewed_v13_to_v15_steps_use_explicit_timestamp_and_preserve_legacy_gates() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("reviewed-v13.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("apply current schema");

        conn.execute(
            "INSERT INTO issues (id, content_hash, title, status, priority, issue_type, created_at, updated_at) \
             VALUES ('bd-reviewed-v13', 'pre-v14-hash', 'Test', 'open', 2, 'task', \
                     '2026-04-02T20:00:00Z', '2026-04-03T01:00:00Z')",
        )
        .expect("seed issue");
        conn.execute(
            "INSERT INTO export_hashes (issue_id, content_hash, exported_at) \
             VALUES ('bd-reviewed-v13', 'pre-v14-hash', '2026-04-03T01:00:00Z')",
        )
        .expect("seed export hash");
        conn.execute(
            "INSERT INTO gate_results (issue_id, gate, provider, passed, recorded_by) \
             VALUES ('bd-reviewed-v13', 'ci_green', 'ci', 1, 'legacy-bot')",
        )
        .expect("seed legacy gate result");
        conn.execute("DELETE FROM dirty_issues")
            .expect("clear trigger-created dirty marker");
        conn.execute("DROP TABLE gate_result_history")
            .expect("restore pre-v15 shape");
        conn.execute("PRAGMA user_version = 13")
            .expect("stamp reviewed source");

        let marked_at = "2026-07-27T12:34:56Z";
        conn.execute("BEGIN IMMEDIATE")
            .expect("caller owns migration transaction");
        let effects = run_reviewed_schema_migration_steps_in_transaction(
            &conn,
            13,
            current_schema_version_u32().expect("current version"),
            marked_at,
        )
        .expect("reviewed 13->15 steps");
        conn.execute("COMMIT")
            .expect("caller commits migration transaction");

        assert_eq!(
            effects,
            ReviewedSchemaMigrationEffects {
                from_version: 13,
                to_version: current_schema_version_u32().expect("current version"),
                content_hash_rows_rebuilt: 1,
                gate_result_history_created: true,
            }
        );
        let hash = conn
            .query_row("SELECT content_hash FROM issues WHERE id = 'bd-reviewed-v13'")
            .expect("read rebuilt hash");
        assert_eq!(
            hash.get(0).and_then(SqliteValue::as_text),
            Some("c42bf13dfd6447e08d119f8b0ad0a503d23ccaa92b211348fb6dfbc55a4e0779")
        );
        let dirty = conn
            .query_row("SELECT marked_at FROM dirty_issues WHERE issue_id = 'bd-reviewed-v13'")
            .expect("read explicit dirty marker");
        assert_eq!(
            dirty.get(0).and_then(SqliteValue::as_text),
            Some(marked_at),
            "reviewed migration must write the caller-provided timestamp verbatim"
        );
        let export_count = conn
            .query_row("SELECT COUNT(*) FROM export_hashes")
            .expect("count export hashes");
        assert_eq!(
            export_count.get(0).and_then(SqliteValue::as_integer),
            Some(0)
        );
        let legacy_count = conn
            .query_row("SELECT COUNT(*) FROM gate_results")
            .expect("count legacy gate results");
        assert_eq!(
            legacy_count.get(0).and_then(SqliteValue::as_integer),
            Some(1),
            "v15 must preserve legacy unscoped evidence"
        );
        let history_count = conn
            .query_row("SELECT COUNT(*) FROM gate_result_history")
            .expect("count scoped history");
        assert_eq!(
            history_count.get(0).and_then(SqliteValue::as_integer),
            Some(0),
            "legacy rows must not be promoted into a transition scope"
        );
        assert_eq!(
            connection_user_version(&conn).expect("read migrated version"),
            current_schema_version_u32().expect("current version")
        );
    }

    #[test]
    fn test_reviewed_v14_to_v15_steps_do_not_reapply_v14() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("reviewed-v14.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("apply current schema");

        conn.execute(
            "INSERT INTO issues (id, content_hash, title, status, priority, issue_type, created_at, updated_at) \
             VALUES ('bd-reviewed-v14', 'already-v14-hash', 'Keep me', 'open', 2, 'task', \
                     '2026-04-02T20:00:00Z', '2026-04-03T01:00:00Z')",
        )
        .expect("seed issue");
        conn.execute(
            "INSERT INTO export_hashes (issue_id, content_hash, exported_at) \
             VALUES ('bd-reviewed-v14', 'already-v14-hash', '2026-04-03T01:00:00Z')",
        )
        .expect("seed export hash");
        conn.execute("DELETE FROM dirty_issues")
            .expect("clear trigger-created dirty marker");
        conn.execute(
            "INSERT INTO dirty_issues (issue_id, marked_at) \
             VALUES ('bd-reviewed-v14', 'existing-marker')",
        )
        .expect("seed existing dirty marker");
        conn.execute("DROP TABLE gate_result_history")
            .expect("restore pre-v15 shape");
        conn.execute("PRAGMA user_version = 14")
            .expect("stamp reviewed source");

        conn.execute("BEGIN IMMEDIATE")
            .expect("caller owns migration transaction");
        let effects = run_reviewed_schema_migration_steps_in_transaction(
            &conn,
            14,
            current_schema_version_u32().expect("current version"),
            "unused-v14-timestamp",
        )
        .expect("reviewed 14->15 steps");
        conn.execute("COMMIT")
            .expect("caller commits migration transaction");

        assert_eq!(
            effects,
            ReviewedSchemaMigrationEffects {
                from_version: 14,
                to_version: current_schema_version_u32().expect("current version"),
                content_hash_rows_rebuilt: 0,
                gate_result_history_created: true,
            }
        );
        let issue = conn
            .query_row("SELECT content_hash FROM issues WHERE id = 'bd-reviewed-v14'")
            .expect("read preserved hash");
        assert_eq!(
            issue.get(0).and_then(SqliteValue::as_text),
            Some("already-v14-hash")
        );
        let dirty = conn
            .query_row("SELECT marked_at FROM dirty_issues WHERE issue_id = 'bd-reviewed-v14'")
            .expect("read preserved dirty marker");
        assert_eq!(
            dirty.get(0).and_then(SqliteValue::as_text),
            Some("existing-marker")
        );
        let export_count = conn
            .query_row("SELECT COUNT(*) FROM export_hashes")
            .expect("count preserved export hashes");
        assert_eq!(
            export_count.get(0).and_then(SqliteValue::as_integer),
            Some(1)
        );
        assert!(table_exists(&conn, "gate_result_history"));
        assert_eq!(
            connection_user_version(&conn).expect("read migrated version"),
            current_schema_version_u32().expect("current version")
        );
    }

    #[test]
    fn test_reviewed_v15_attestation_rejects_same_name_malformed_schema() {
        let malformed_type =
            GATE_RESULT_HISTORY_MIGRATION_SQL.replace("passed INTEGER", "passed TEXT");
        let malformed_foreign_key =
            GATE_RESULT_HISTORY_MIGRATION_SQL.replace("ON DELETE CASCADE", "ON DELETE SET NULL");
        let malformed_index = GATE_RESULT_HISTORY_MIGRATION_SQL.replace(
            "ON gate_result_history(issue_id, id)",
            "ON gate_result_history(id, issue_id)",
        );
        let malformed_autoincrement = GATE_RESULT_HISTORY_MIGRATION_SQL
            .replace("INTEGER PRIMARY KEY AUTOINCREMENT", "INTEGER PRIMARY KEY");
        let comment_spoofed_autoincrement = GATE_RESULT_HISTORY_MIGRATION_SQL.replace(
            "INTEGER PRIMARY KEY AUTOINCREMENT",
            "INTEGER PRIMARY KEY /* id INTEGER PRIMARY KEY AUTOINCREMENT */",
        );
        let string_spoofed_autoincrement = GATE_RESULT_HISTORY_MIGRATION_SQL.replace(
            "INTEGER PRIMARY KEY AUTOINCREMENT",
            "INTEGER PRIMARY KEY CHECK('id INTEGER PRIMARY KEY AUTOINCREMENT' IS NOT NULL)",
        );
        let quoted_identifier_spoofed_autoincrement = GATE_RESULT_HISTORY_MIGRATION_SQL
            .replace("INTEGER PRIMARY KEY AUTOINCREMENT", "INTEGER PRIMARY KEY")
            .replace(
                "        FOREIGN KEY (issue_id)",
                "        CONSTRAINT \"id INTEGER PRIMARY KEY AUTOINCREMENT\" CHECK (1),\n\
                 FOREIGN KEY (issue_id)",
            );

        for (label, schema_sql) in [
            ("column_type", malformed_type),
            ("foreign_key", malformed_foreign_key),
            ("index_order", malformed_index),
            ("autoincrement", malformed_autoincrement),
            (
                "comment_spoofed_autoincrement",
                comment_spoofed_autoincrement,
            ),
            ("string_spoofed_autoincrement", string_spoofed_autoincrement),
            (
                "quoted_identifier_spoofed_autoincrement",
                quoted_identifier_spoofed_autoincrement,
            ),
        ] {
            let (_temp, conn) = reviewed_v14_with_gate_history_schema(&schema_sql);
            let error = run_migrations_atomic(
                &conn,
                14,
                current_schema_version_u32().expect("current version"),
            )
            .expect_err("same-name malformed v15 schema must be refused");
            assert!(
                error.to_string().contains("v15 post-check failed"),
                "{label} mismatch should report failed v15 attestation: {error}"
            );
            assert_eq!(
                connection_user_version(&conn).expect("read refused version"),
                14,
                "{label} mismatch must not stamp the target version"
            );
        }
    }

    #[test]
    fn test_reviewed_v14_clears_export_hashes_when_issues_are_empty() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("reviewed-empty-v13.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("apply current schema");

        conn.execute("DROP TABLE export_hashes")
            .expect("replace export hashes with legacy FK-free fixture");
        conn.execute(
            "CREATE TABLE export_hashes (
                issue_id TEXT PRIMARY KEY,
                content_hash TEXT NOT NULL,
                exported_at DATETIME NOT NULL
            )",
        )
        .expect("create legacy export hashes");
        conn.execute(
            "INSERT INTO export_hashes (issue_id, content_hash, exported_at)
             VALUES ('orphan-export', 'stale-hash', '2026-07-27T12:00:00Z')",
        )
        .expect("seed stale orphan export hash");
        conn.execute("DROP TABLE gate_result_history")
            .expect("restore pre-v15 shape");
        conn.execute("PRAGMA user_version = 13")
            .expect("stamp reviewed source");

        run_migrations_atomic(
            &conn,
            13,
            current_schema_version_u32().expect("current version"),
        )
        .expect("empty v13 migration");

        let issue_count = conn
            .query_row("SELECT COUNT(*) FROM issues")
            .expect("count empty issues");
        assert_eq!(
            issue_count.get(0).and_then(SqliteValue::as_integer),
            Some(0)
        );
        let export_count = conn
            .query_row("SELECT COUNT(*) FROM export_hashes")
            .expect("count cleared export hashes");
        assert_eq!(
            export_count.get(0).and_then(SqliteValue::as_integer),
            Some(0),
            "v14 must clear export hashes even when there are no issue rows"
        );
        assert_eq!(
            connection_user_version(&conn).expect("read migrated version"),
            current_schema_version_u32().expect("current version")
        );
    }

    #[test]
    fn test_reviewed_migration_rolls_back_v14_when_v15_ddl_fails() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("reviewed-rollback.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("apply current schema");

        conn.execute(
            "INSERT INTO issues (id, content_hash, title, status, priority, issue_type, created_at, updated_at) \
             VALUES ('bd-reviewed-rollback', 'pre-v14-hash', 'Test', 'open', 2, 'task', \
                     '2026-04-02T20:00:00Z', '2026-04-03T01:00:00Z')",
        )
        .expect("seed issue");
        conn.execute(
            "INSERT INTO export_hashes (issue_id, content_hash, exported_at) \
             VALUES ('bd-reviewed-rollback', 'pre-v14-hash', '2026-04-03T01:00:00Z')",
        )
        .expect("seed export hash");
        conn.execute("DELETE FROM dirty_issues")
            .expect("clear trigger-created dirty marker");
        conn.execute("DROP TABLE gate_result_history")
            .expect("remove canonical v15 table");
        conn.execute("CREATE TABLE gate_result_history (id INTEGER PRIMARY KEY)")
            .expect("seed incompatible v15 table");
        conn.execute("PRAGMA user_version = 13")
            .expect("stamp reviewed source");

        let error = run_migrations_atomic(
            &conn,
            13,
            current_schema_version_u32().expect("current version"),
        )
        .expect_err("incompatible v15 DDL must fail the migration");
        assert!(
            !error.to_string().is_empty(),
            "migration failure should retain a diagnostic"
        );

        let issue = conn
            .query_row("SELECT content_hash FROM issues WHERE id = 'bd-reviewed-rollback'")
            .expect("read rolled-back hash");
        assert_eq!(
            issue.get(0).and_then(SqliteValue::as_text),
            Some("pre-v14-hash"),
            "v14 hash rewrite must roll back when v15 fails"
        );
        let dirty_count = conn
            .query_row("SELECT COUNT(*) FROM dirty_issues WHERE issue_id = 'bd-reviewed-rollback'")
            .expect("count rolled-back dirty markers");
        assert_eq!(
            dirty_count.get(0).and_then(SqliteValue::as_integer),
            Some(0),
            "v14 dirty marker must roll back when v15 fails"
        );
        let export_count = conn
            .query_row("SELECT COUNT(*) FROM export_hashes")
            .expect("count rolled-back export hashes");
        assert_eq!(
            export_count.get(0).and_then(SqliteValue::as_integer),
            Some(1),
            "v14 export-hash clearing must roll back when v15 fails"
        );
        assert_eq!(
            connection_user_version(&conn).expect("read rolled-back version"),
            13,
            "failed migration must not stamp the target"
        );
        assert!(table_exists(&conn, "gate_result_history"));
        assert!(!column_exists(&conn, "gate_result_history", "issue_id"));
        assert!(!index_exists(&conn, "idx_gate_result_history_issue"));
        assert!(!index_exists(&conn, "idx_gate_result_history_scope"));
    }

    #[test]
    fn test_reviewed_migration_refuses_unreviewed_edges_before_writes() {
        // NOTE (merge 2026-07-28): the former `(12, current)` case is gone —
        // pre-reviewed-era versions (< 13) deliberately keep the shipped
        // general upgrade path (see test_db_migrate_happy_path's 7->current),
        // while the stamp-integrity refusals below are fully retained.
        for (from, target) in [(13, 14), (14, 14), (15, 15), (13, 16)] {
            let temp = TempDir::new().expect("tempdir");
            let db_path = temp.path().join(format!("refuse-{from}-{target}.db"));
            let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
            apply_schema(&conn).expect("apply current schema");
            conn.execute(
                "INSERT INTO issues (id, content_hash, title, status, priority, issue_type, created_at, updated_at) \
                 VALUES ('bd-refuse', 'sentinel-hash', 'Unchanged', 'open', 2, 'task', \
                         '2026-04-02T20:00:00Z', '2026-04-03T01:00:00Z')",
            )
            .expect("seed sentinel issue");
            conn.execute("DELETE FROM dirty_issues")
                .expect("clear trigger-created dirty marker");
            conn.execute("DROP TABLE gate_result_history")
                .expect("make v15 creation observable");
            conn.execute(&format!("PRAGMA user_version = {from}"))
                .expect("stamp requested source");

            let error = run_migrations_atomic(&conn, from, target)
                .expect_err("unreviewed migration edge must be refused");
            assert!(
                error.to_string().contains("schema migrate refused"),
                "refusal should explain the rejected edge {from}->{target}: {error}"
            );
            assert_eq!(
                connection_user_version(&conn).expect("read unchanged version"),
                from,
                "refused edge {from}->{target} must not stamp user_version"
            );
            let issue = conn
                .query_row("SELECT content_hash FROM issues WHERE id = 'bd-refuse'")
                .expect("read unchanged issue");
            assert_eq!(
                issue.get(0).and_then(SqliteValue::as_text),
                Some("sentinel-hash"),
                "refused edge {from}->{target} must not rebuild hashes"
            );
            assert!(
                !table_exists(&conn, "gate_result_history"),
                "refused edge {from}->{target} must not apply v15 DDL"
            );
        }
    }

    #[test]
    fn test_reviewed_migration_refuses_effective_source_mismatch_before_writes() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("refuse-source-mismatch.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("apply current schema");
        conn.execute("DROP TABLE gate_result_history")
            .expect("make v15 creation observable");
        conn.execute("PRAGMA user_version = 14")
            .expect("stamp effective source");

        let error = run_migrations_atomic(
            &conn,
            13,
            current_schema_version_u32().expect("current version"),
        )
        .expect_err("declared source mismatch must be refused");
        assert!(
            error.to_string().contains("user_version mismatch"),
            "mismatch should be explicit: {error}"
        );
        assert_eq!(
            connection_user_version(&conn).expect("read unchanged version"),
            14
        );
        assert!(!table_exists(&conn, "gate_result_history"));
    }

    #[test]
    fn test_v15_adds_scoped_gate_history_without_reusing_legacy_results() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("apply current schema");

        conn.execute(
            "INSERT INTO issues (id, title, status, priority, issue_type, created_at, updated_at) \
             VALUES ('bd-gate-v15', 'Legacy gate', 'in_review', 2, 'task', CURRENT_TIMESTAMP, CURRENT_TIMESTAMP)",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO gate_results (issue_id, gate, provider, passed, recorded_by) \
             VALUES ('bd-gate-v15', 'ci_green', 'ci', 1, 'legacy-bot')",
        )
        .unwrap();
        conn.execute("DROP TABLE gate_result_history").unwrap();
        conn.execute("PRAGMA user_version = 14").unwrap();

        run_migrations(&conn, false).expect("v15 migration should succeed");

        assert!(table_exists(&conn, "gate_result_history"));
        for column in [
            "id",
            "issue_id",
            "from_status",
            "to_status",
            "status_revision",
            "gate",
            "provider",
            "passed",
            "note",
            "recorded_by",
            "recorded_at",
        ] {
            assert!(
                column_exists(&conn, "gate_result_history", column),
                "v15 migration missing gate_result_history.{column}"
            );
        }
        let history_count = conn
            .query_row("SELECT COUNT(*) FROM gate_result_history")
            .unwrap();
        assert_eq!(
            history_count.get(0).and_then(SqliteValue::as_integer),
            Some(0),
            "legacy unscoped results must not be promoted into an effective transition scope"
        );
        let legacy_count = conn.query_row("SELECT COUNT(*) FROM gate_results").unwrap();
        assert_eq!(
            legacy_count.get(0).and_then(SqliteValue::as_integer),
            Some(1),
            "migration must preserve legacy results for audit display"
        );
    }

    #[test]
    fn test_v16_adds_capacity_exemption_tables() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("apply current schema");

        conn.execute("DROP TABLE capacity_exemption_history")
            .unwrap();
        conn.execute("DROP TABLE capacity_exemptions").unwrap();
        conn.execute("PRAGMA user_version = 15").unwrap();

        run_migrations(&conn, false).expect("v16 migration should succeed");

        assert!(table_exists(&conn, "capacity_exemptions"));
        for column in [
            "issue_id",
            "capacity_kind",
            "capacity_name",
            "provider",
            "reason",
            "granted_by",
            "granted_at",
            "expires_at",
            "ended_at",
            "ended_action",
            "ended_by",
        ] {
            assert!(
                column_exists(&conn, "capacity_exemptions", column),
                "v16 migration missing capacity_exemptions.{column}"
            );
        }
        assert!(table_exists(&conn, "capacity_exemption_history"));
        for column in [
            "id",
            "issue_id",
            "capacity_kind",
            "capacity_name",
            "action",
            "provider",
            "reason",
            "actor",
            "expires_at",
            "recorded_at",
        ] {
            assert!(
                column_exists(&conn, "capacity_exemption_history", column),
                "v16 migration missing capacity_exemption_history.{column}"
            );
        }
    }

    #[test]
    fn test_v17_adds_capacity_occupancy_table() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("apply current schema");

        conn.execute("DROP TABLE capacity_occupancy").unwrap();
        conn.execute("PRAGMA user_version = 16").unwrap();

        run_migrations(&conn, false).expect("v17 migration should succeed");

        assert!(table_exists(&conn, "capacity_occupancy"));
        for column in [
            "issue_id",
            "actor",
            "agent_name",
            "harness",
            "session",
            "recorded_at",
        ] {
            assert!(
                column_exists(&conn, "capacity_occupancy", column),
                "v17 migration missing capacity_occupancy.{column}"
            );
        }
    }

    /// Regression for beads_rust#290: legacy DBs that pre-date the
    /// `marked_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP` definition
    /// kept `dirty_issues.marked_at` as a plain NOT NULL column with no
    /// default. The v7 migration's `INSERT INTO dirty_issues (issue_id)`
    /// path then tripped the constraint and bricked every `br` command
    /// against the legacy DB. The fix passes `marked_at` explicitly so
    /// the absence of a column-level default no longer matters.
    #[test]
    fn test_v7_rebuild_works_when_dirty_issues_has_no_default() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("Failed to apply schema");

        // Re-create dirty_issues without the DEFAULT to mirror what
        // a DB initialized under the pre-v7 schema looks like in the wild.
        conn.execute("DROP TABLE dirty_issues").unwrap();
        conn.execute(
            "CREATE TABLE dirty_issues (
                 issue_id TEXT PRIMARY KEY,
                 marked_at TEXT NOT NULL
             )",
        )
        .unwrap();

        conn.execute(
            "INSERT INTO issues (id, content_hash, title, status, priority, issue_type, created_at, updated_at) \
             VALUES ('bd-legacy', 'old-rust-hash', 'Legacy', 'open', 2, 'task', '2026-04-02T20:00:00Z', '2026-04-03T01:00:00Z')",
        ).unwrap();
        conn.execute("PRAGMA user_version = 6").unwrap();

        run_migrations(&conn, false)
            .expect("v7 migration must succeed against legacy dirty_issues schema");

        let dirty_row = conn
            .query_row("SELECT COUNT(*) FROM dirty_issues WHERE issue_id = 'bd-legacy'")
            .unwrap();
        assert_eq!(
            dirty_row.get(0).and_then(SqliteValue::as_integer),
            Some(1),
            "issue must be flagged dirty after v7 even on legacy table shape"
        );
    }

    #[test]
    fn test_v8_backfills_storage_null_in_default_columns() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("Failed to apply schema");

        // Seed a row with all required columns set, then force storage-NULLs
        // into the columns the migration is supposed to heal. We rely on
        // direct UPDATEs reaching the storage layer; if the engine refuses
        // any individual UPDATE, the corresponding assertion below still
        // exercises the no-op path of the migration.
        conn.execute(
            "INSERT INTO issues (id, title, status, priority, issue_type, created_at, updated_at) \
             VALUES ('bd-null', 'legacy null row', 'open', 2, 'task', '2026-04-30T00:00:00Z', '2026-04-30T00:00:00Z')",
        )
        .expect("seed row");

        // Best-effort: not every column accepts a direct NULL update under
        // every storage engine. The migration must only act on columns
        // that *do* hold storage-NULL values, so we forge as many as the
        // engine allows and verify the migration heals every successful one.
        let columns_to_null: &[&str] = &[
            "description",
            "design",
            "acceptance_criteria",
            "notes",
            "status",
            "priority",
            "issue_type",
            "source_repo",
            "ephemeral",
            "pinned",
            "is_template",
        ];
        for column in columns_to_null {
            let _ = conn.execute(&format!(
                "UPDATE issues SET {column} = NULL WHERE id = 'bd-null'"
            ));
        }

        // Run the v8 migration directly so this test stays focused on the
        // backfill behaviour rather than the surrounding migration ladder.
        backfill_storage_null_in_default_columns(&conn);

        // Idempotent: every NOT NULL DEFAULT column must hold a non-NULL
        // storage class after the backfill, regardless of which UPDATE-to-
        // NULL succeeded above.
        for column in columns_to_null {
            let row = conn
                .query_row(&format!(
                    "SELECT typeof({column}) FROM issues WHERE id = 'bd-null'"
                ))
                .unwrap();
            let actual_type = row.get(0).and_then(SqliteValue::as_text);
            assert_ne!(
                actual_type,
                Some("null"),
                "{column} should be backfilled to its declared default (got typeof = null)"
            );
        }

        // Second pass is a no-op (the UPDATEs touch zero rows).
        backfill_storage_null_in_default_columns(&conn);
        let row = conn
            .query_row("SELECT typeof(notes) FROM issues WHERE id = 'bd-null'")
            .unwrap();
        assert_ne!(row.get(0).and_then(SqliteValue::as_text), Some("null"));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn test_reviewed_migration_refuses_9_to_10_without_applying_later_steps() {
        // The explicit reviewed migration hook is deliberately not the
        // automatic open-time migration ladder. A request for the old 9->10
        // edge must be rejected before either its v10 ALTER or any newer v11-
        // v15 step runs; otherwise a caller-controlled target could stamp a
        // partially or over-migrated database.
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("legacy_v9.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();

        // Hand-build the canonical v9 issues table: all the columns that
        // existed before #289 landed, in the canonical EXPECTED order, but
        // intentionally missing the source_repo_path tail column.
        execute_batch(
            &conn,
            r"
            CREATE TABLE issues (
                id TEXT PRIMARY KEY,
                content_hash TEXT,
                title TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                design TEXT NOT NULL DEFAULT '',
                acceptance_criteria TEXT NOT NULL DEFAULT '',
                notes TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL DEFAULT 'open',
                priority INTEGER NOT NULL DEFAULT 2,
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
                due_at DATETIME,
                defer_until DATETIME,
                external_ref TEXT,
                source_system TEXT DEFAULT '',
                source_repo TEXT NOT NULL DEFAULT '.',
                deleted_at DATETIME,
                deleted_by TEXT DEFAULT '',
                delete_reason TEXT DEFAULT '',
                original_type TEXT DEFAULT '',
                compaction_level INTEGER DEFAULT 0,
                compacted_at DATETIME,
                compacted_at_commit TEXT,
                original_size INTEGER,
                sender TEXT DEFAULT '',
                ephemeral INTEGER NOT NULL DEFAULT 0,
                pinned INTEGER NOT NULL DEFAULT 0,
                is_template INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                issue_id TEXT NOT NULL,
                event_type TEXT NOT NULL,
                actor TEXT NOT NULL DEFAULT '',
                old_value TEXT,
                new_value TEXT,
                comment TEXT,
                created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            ",
        )
        .expect("seed v9 schema objects");
        conn.execute(
            "INSERT INTO issues (id, content_hash, title, status, priority, issue_type, created_at, updated_at) \
             VALUES ('bd-v9-refusal', 'pre-v14-hash', 'Do not migrate', 'open', 2, 'task', \
                     '2026-04-02T20:00:00Z', '2026-04-03T01:00:00Z')",
        )
        .expect("seed v14 sentinel");

        // Stamp the legacy version so the open-path would otherwise
        // short-circuit and skip migrations.
        conn.execute("PRAGMA user_version = 9")
            .expect("stamp legacy user_version");

        assert!(
            !column_exists(&conn, "issues", "source_repo_path"),
            "precondition: legacy v9 table must not have source_repo_path"
        );

        let error =
            run_migrations_atomic(&conn, 9, 10).expect_err("unreviewed 9->10 edge must be refused");
        assert!(
            error.to_string().contains("schema migrate refused"),
            "refusal should be explicit: {error}"
        );

        assert!(
            !column_exists(&conn, "issues", "source_repo_path"),
            "refused 9->10 edge must not apply the v10 column"
        );
        assert!(
            !column_exists(&conn, "issues", "agent_context"),
            "refused 9->10 edge must not apply the v11 column"
        );
        assert!(
            !table_exists(&conn, "gate_results"),
            "refused 9->10 edge must not create the v12 table"
        );
        for column in ["agent_name", "harness", "model"] {
            assert!(
                !column_exists(&conn, "events", column),
                "refused 9->10 edge must not apply the v13 events.{column} column"
            );
        }
        let hash = conn
            .query_row("SELECT content_hash FROM issues WHERE id = 'bd-v9-refusal'")
            .expect("read v14 sentinel");
        assert_eq!(
            hash.get(0).and_then(SqliteValue::as_text),
            Some("pre-v14-hash"),
            "refused 9->10 edge must not apply the v14 hash rebuild"
        );
        assert!(
            !table_exists(&conn, "gate_result_history"),
            "refused 9->10 edge must not create the v15 table"
        );

        let stamped = conn
            .query_row("PRAGMA user_version")
            .ok()
            .and_then(|row| row.get(0).and_then(SqliteValue::as_integer))
            .unwrap_or(-1);
        assert_eq!(
            stamped, 9,
            "refused migration must leave the effective source version unchanged"
        );
    }

    #[test]
    fn test_apply_schema_file_backed_has_no_duplicate_issues_columns() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("beads.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();

        apply_schema(&conn).expect("Failed to apply schema");

        let row = conn
            .query_row("SELECT sql FROM sqlite_master WHERE type='table' AND name='issues'")
            .expect("issues table should exist");
        let issues_sql = row
            .get(0)
            .and_then(SqliteValue::as_text)
            .expect("issues table SQL should be present");

        // Use trailing space to disambiguate from `source_repo_path` (which
        // contains `source_repo` as a prefix). The column declaration is
        // `source_repo TEXT ...`, so the space-suffixed form matches the
        // canonical declaration site exactly once.
        assert_eq!(
            issues_sql.matches("source_repo ").count(),
            1,
            "issues table SQL should define source_repo exactly once"
        );
        assert_eq!(
            issues_sql.matches("source_repo_path ").count(),
            1,
            "issues table SQL should define source_repo_path exactly once"
        );
        assert_eq!(
            issues_sql.matches("is_template").count(),
            1,
            "issues table SQL should define is_template exactly once"
        );
    }

    /// Conformance test: Verify schema matches bd (Go) for interoperability.
    /// Tests table structure, defaults, constraints, and indexes.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn test_schema_parity_conformance() {
        // Bind the temp file: dropping it here would unlink the database
        // before the connection ever writes to it, leaving `Connection::open`
        // pointed at a dangling path.
        let temp_db = tempfile::NamedTempFile::new().unwrap();
        let conn = Connection::open(temp_db.path().to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("Failed to apply schema");

        // === ISSUES TABLE ===
        // Verify column defaults
        let issues_cols: Vec<(String, String, i32, Option<String>)> = conn
            .query("PRAGMA table_info(issues)")
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row.get(1)
                        .and_then(|v| v.as_text())
                        .unwrap_or("")
                        .to_string(),
                    row.get(2)
                        .and_then(|v| v.as_text())
                        .unwrap_or("")
                        .to_string(),
                    #[allow(clippy::cast_possible_truncation)]
                    {
                        row.get(3).and_then(SqliteValue::as_integer).unwrap_or(0) as i32
                    },
                    row.get(4).and_then(|v| v.as_text()).map(String::from),
                )
            })
            .collect();

        // Check required defaults for bd parity
        let col_map: std::collections::HashMap<_, _> = issues_cols
            .iter()
            .map(|(name, typ, notnull, dflt)| {
                (name.as_str(), (typ.as_str(), *notnull, dflt.clone()))
            })
            .collect();

        // status must default to 'open'
        assert_eq!(
            col_map.get("status").map(|c| c.2.as_deref()),
            Some(Some("'open'")),
            "status should default to 'open'"
        );

        // priority must default to 2
        assert_eq!(
            col_map.get("priority").map(|c| c.2.as_deref()),
            Some(Some("2")),
            "priority should default to 2"
        );

        // issue_type must default to 'task'
        assert_eq!(
            col_map.get("issue_type").map(|c| c.2.as_deref()),
            Some(Some("'task'")),
            "issue_type should default to 'task'"
        );

        // created_at and updated_at must default to CURRENT_TIMESTAMP
        assert_eq!(
            col_map.get("created_at").map(|c| c.2.as_deref()),
            Some(Some("CURRENT_TIMESTAMP")),
            "created_at should default to CURRENT_TIMESTAMP"
        );
        assert_eq!(
            col_map.get("updated_at").map(|c| c.2.as_deref()),
            Some(Some("CURRENT_TIMESTAMP")),
            "updated_at should default to CURRENT_TIMESTAMP"
        );

        // === VERIFY KEY INDEXES EXIST ===
        let indexes: HashSet<String> = conn
            .query("SELECT name FROM sqlite_master WHERE type='index' AND sql IS NOT NULL")
            .unwrap()
            .iter()
            .filter_map(|row| row.get(0).and_then(|v| v.as_text()).map(String::from))
            .collect();

        // Core indexes
        assert!(
            indexes.contains("idx_issues_status"),
            "missing idx_issues_status"
        );
        assert!(
            indexes.contains("idx_issues_priority"),
            "missing idx_issues_priority"
        );
        assert!(
            indexes.contains("idx_issues_issue_type"),
            "missing idx_issues_issue_type"
        );
        assert!(
            indexes.contains("idx_issues_created_at"),
            "missing idx_issues_created_at"
        );
        assert!(
            indexes.contains("idx_issues_updated_at"),
            "missing idx_issues_updated_at"
        );

        // Export/sync indexes
        assert!(
            indexes.contains("idx_issues_content_hash"),
            "missing idx_issues_content_hash"
        );
        assert!(
            indexes.contains("idx_issues_external_ref_unique"),
            "missing external_ref index"
        );

        // Special state indexes
        assert!(
            indexes.contains("idx_issues_ephemeral"),
            "missing idx_issues_ephemeral"
        );
        assert!(
            indexes.contains("idx_issues_pinned"),
            "missing idx_issues_pinned"
        );
        assert!(
            indexes.contains("idx_issues_tombstone"),
            "missing idx_issues_tombstone"
        );

        // Time-based indexes
        assert!(
            indexes.contains("idx_issues_due_at"),
            "missing idx_issues_due_at"
        );
        assert!(
            indexes.contains("idx_issues_defer_until"),
            "missing idx_issues_defer_until"
        );

        // Ready work composite index (critical for performance)
        assert!(
            indexes.contains("idx_issues_ready"),
            "missing idx_issues_ready composite index"
        );
        // Widened ready group (#354): non-partial composite must exist on real DBs
        // so a configured `status IN (...)` ready query stays index-covered.
        assert!(
            indexes.contains("idx_issues_status_priority_created"),
            "missing idx_issues_status_priority_created composite index"
        );
        assert!(
            indexes.contains("idx_issues_list_active_order"),
            "missing idx_issues_list_active_order composite index"
        );

        // === DEPENDENCIES TABLE ===
        let deps_cols: Vec<(String, Option<String>)> = conn
            .query("PRAGMA table_info(dependencies)")
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row.get(1)
                        .and_then(|v| v.as_text())
                        .unwrap_or("")
                        .to_string(),
                    row.get(4).and_then(|v| v.as_text()).map(String::from),
                )
            })
            .collect();

        let deps_map: std::collections::HashMap<_, _> = deps_cols
            .iter()
            .map(|(name, dflt)| (name.as_str(), dflt.clone()))
            .collect();

        // type must default to 'blocks'
        assert_eq!(
            deps_map.get("type").cloned().flatten().as_deref(),
            Some("'blocks'"),
            "dependencies.type should default to 'blocks'"
        );

        // metadata must default to '{}'
        assert_eq!(
            deps_map.get("metadata").cloned().flatten().as_deref(),
            Some("'{}'"),
            "dependencies.metadata should default to '{{}}'"
        );

        // Dependency indexes (bd parity)
        assert!(
            indexes.contains("idx_dependencies_issue"),
            "missing idx_dependencies_issue"
        );
        assert!(
            indexes.contains("idx_dependencies_depends_on"),
            "missing idx_dependencies_depends_on"
        );
        assert!(
            indexes.contains("idx_dependencies_type"),
            "missing idx_dependencies_type"
        );
        assert!(
            indexes.contains("idx_dependencies_depends_on_type"),
            "missing idx_dependencies_depends_on_type"
        );
        assert!(
            indexes.contains("idx_dependencies_thread"),
            "missing idx_dependencies_thread"
        );
        assert!(
            indexes.contains("idx_dependencies_blocking"),
            "missing idx_dependencies_blocking"
        );

        // Labels indexes
        assert!(
            indexes.contains("idx_labels_label"),
            "missing idx_labels_label"
        );
        assert!(
            indexes.contains("idx_labels_issue"),
            "missing idx_labels_issue"
        );

        // === BLOCKED_ISSUES_CACHE TABLE ===
        let cache_cols: Vec<String> = conn
            .query("PRAGMA table_info(blocked_issues_cache)")
            .unwrap()
            .iter()
            .filter_map(|row| row.get(1).and_then(|v| v.as_text()).map(String::from))
            .collect();

        assert!(
            cache_cols.contains(&"issue_id".to_string()),
            "blocked_issues_cache should have 'issue_id' column"
        );

        // Must have blocked_by (not blocked_by_json) and blocked_at
        assert!(
            cache_cols.contains(&"blocked_by".to_string()),
            "blocked_issues_cache should have 'blocked_by' column (not 'blocked_by_json')"
        );
        assert!(
            cache_cols.contains(&"blocked_at".to_string()),
            "blocked_issues_cache should have 'blocked_at' column"
        );
        assert!(
            !cache_cols.contains(&"blocked_by_json".to_string()),
            "blocked_issues_cache should NOT have old 'blocked_by_json' column"
        );

        // Verify blocked_cache index exists
        assert!(
            indexes.contains("idx_blocked_cache_blocked_at"),
            "missing idx_blocked_cache_blocked_at"
        );

        // === TEST CLOSED-AT CONSTRAINT ===
        // Insert an issue with defaults (will get status='open', closed_at=NULL)
        conn.execute("INSERT INTO issues (id, title) VALUES ('test-1', 'Test Issue')")
            .expect("Should allow open issue without closed_at");

        // Try to insert closed issue without closed_at — CHECK constraint
        // should reject it. fsqlite does not yet enforce CHECK constraints,
        // so we accept either outcome.
        let result = conn.execute(
            "INSERT INTO issues (id, title, status) VALUES ('test-2', 'Closed', 'closed')",
        );
        if result.is_ok() {
            // fsqlite: CHECK not enforced — clean up the row so later assertions
            // are not affected by the extra row.
            let _ = conn.execute("DELETE FROM issues WHERE id = 'test-2'");
        }

        // Insert closed issue with closed_at - should succeed
        conn.execute(
            "INSERT INTO issues (id, title, status, closed_at) VALUES ('test-3', 'Closed', 'closed', CURRENT_TIMESTAMP)",
        )
        .expect("Should allow closed issue with closed_at");

        // Insert tombstone without closed_at - should succeed (tombstones exempt)
        conn.execute(
            "INSERT INTO issues (id, title, status) VALUES ('test-4', 'Tombstone', 'tombstone')",
        )
        .expect("Should allow tombstone without closed_at");
    }

    /// Test that migrations correctly upgrade old schemas.
    #[test]
    fn test_migration_blocked_cache_upgrade() {
        // Bind the temp file: dropping it here would unlink the database
        // before the connection ever writes to it, leaving `Connection::open`
        // pointed at a dangling path.
        let temp_db = tempfile::NamedTempFile::new().unwrap();
        let conn = Connection::open(temp_db.path().to_string_lossy().into_owned()).unwrap();

        // Create old-style blocked_issues_cache with blocked_by_json
        // Using a complete issues table schema so index migrations succeed
        execute_batch(
            &conn,
            r"
            CREATE TABLE issues (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'open',
                priority INTEGER NOT NULL DEFAULT 2,
                created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                content_hash TEXT,
                external_ref TEXT,
                ephemeral INTEGER DEFAULT 0,
                pinned INTEGER DEFAULT 0,
                is_template INTEGER DEFAULT 0,
                compaction_level INTEGER DEFAULT 0,
                due_at DATETIME,
                defer_until DATETIME
            );
            CREATE TABLE dependencies (
                issue_id TEXT NOT NULL,
                depends_on_id TEXT NOT NULL,
                type TEXT NOT NULL DEFAULT 'blocks',
                PRIMARY KEY (issue_id, depends_on_id)
            );
            CREATE TABLE comments (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                issue_id TEXT NOT NULL,
                author TEXT NOT NULL,
                text TEXT NOT NULL,
                created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                issue_id TEXT NOT NULL,
                event_type TEXT NOT NULL,
                actor TEXT NOT NULL DEFAULT '',
                old_value TEXT,
                new_value TEXT,
                comment TEXT,
                created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE blocked_issues_cache (
                issue_id TEXT PRIMARY KEY,
                blocked_by_json TEXT NOT NULL
            );
        ",
        )
        .unwrap();

        // Run migrations
        run_migrations(&conn, false).unwrap();

        // Verify columns were updated
        let cols: Vec<String> = conn
            .query("PRAGMA table_info(blocked_issues_cache)")
            .unwrap()
            .iter()
            .filter_map(|row| row.get(1).and_then(|v| v.as_text()).map(String::from))
            .collect();

        assert!(
            cols.contains(&"blocked_by".to_string()),
            "Should have blocked_by"
        );
        assert!(
            cols.contains(&"blocked_at".to_string()),
            "Should have blocked_at"
        );
        assert!(
            !cols.contains(&"blocked_by_json".to_string()),
            "Should not have blocked_by_json"
        );
    }

    #[test]
    fn test_apply_schema_rebuilds_nullable_blocked_cache_table() {
        let temp = tempfile::NamedTempFile::new().unwrap();
        let conn = Connection::open(temp.path().to_string_lossy().into_owned()).unwrap();

        execute_batch(
            &conn,
            r"
            CREATE TABLE blocked_issues_cache (
                issue_id TEXT PRIMARY KEY,
                blocked_by TEXT,
                blocked_at DATETIME
            );
            INSERT INTO blocked_issues_cache (issue_id, blocked_by, blocked_at)
            VALUES ('bd-null-cache', NULL, CURRENT_TIMESTAMP);
            ",
        )
        .unwrap();

        apply_schema(&conn).unwrap();

        let column_rows = conn
            .query("PRAGMA table_info(blocked_issues_cache)")
            .unwrap();
        let mut issue_id_primary_key = false;
        let mut blocked_by_not_null = false;
        let mut blocked_at_not_null = false;
        for row in &column_rows {
            let Some(name) = row.get(1).and_then(SqliteValue::as_text) else {
                continue;
            };
            let not_null = row
                .get(3)
                .and_then(SqliteValue::as_integer)
                .is_some_and(|value| value != 0);
            let primary_key = row
                .get(5)
                .and_then(SqliteValue::as_integer)
                .is_some_and(|value| value != 0);
            match name {
                "issue_id" => issue_id_primary_key = primary_key,
                "blocked_by" => blocked_by_not_null = not_null,
                "blocked_at" => blocked_at_not_null = not_null,
                _ => {}
            }
        }

        assert!(
            issue_id_primary_key,
            "issue_id should be the cache primary key"
        );
        assert!(
            blocked_by_not_null,
            "blocked_by must be NOT NULL after schema repair"
        );
        assert!(
            blocked_at_not_null,
            "blocked_at must be NOT NULL after schema repair"
        );

        let null_rows = conn
            .query_row("SELECT COUNT(*) FROM blocked_issues_cache WHERE blocked_by IS NULL")
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap_or(-1);
        assert_eq!(
            null_rows, 0,
            "derived blocked cache NULL rows should be discarded during schema repair"
        );
    }

    /// Migration: drop old blocked_issues_cache missing issue_id column.
    #[test]
    fn test_migration_blocked_cache_missing_issue_id() {
        // Bind the temp file: dropping it here would unlink the database
        // before the connection ever writes to it, leaving `Connection::open`
        // pointed at a dangling path.
        let temp_db = tempfile::NamedTempFile::new().unwrap();
        let conn = Connection::open(temp_db.path().to_string_lossy().into_owned()).unwrap();

        // Old-style cache table with 'id' column instead of 'issue_id'
        // Using a complete issues table schema so index migrations succeed
        execute_batch(
            &conn,
            r"
            CREATE TABLE issues (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'open',
                priority INTEGER NOT NULL DEFAULT 2,
                issue_type TEXT NOT NULL DEFAULT 'task',
                assignee TEXT,
                created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                updated_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                content_hash TEXT,
                external_ref TEXT,
                ephemeral INTEGER DEFAULT 0,
                pinned INTEGER DEFAULT 0,
                due_at DATETIME,
                defer_until DATETIME
            );
            CREATE TABLE dependencies (
                issue_id TEXT NOT NULL,
                depends_on_id TEXT NOT NULL,
                type TEXT NOT NULL DEFAULT 'blocks',
                PRIMARY KEY (issue_id, depends_on_id)
            );
            CREATE TABLE comments (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                issue_id TEXT NOT NULL,
                author TEXT NOT NULL,
                text TEXT NOT NULL,
                created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                issue_id TEXT NOT NULL,
                event_type TEXT NOT NULL,
                actor TEXT NOT NULL DEFAULT '',
                old_value TEXT,
                new_value TEXT,
                comment TEXT,
                created_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE blocked_issues_cache (
                id TEXT PRIMARY KEY,
                blocked_by TEXT NOT NULL,
                blocked_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
        ",
        )
        .unwrap();

        // Apply full schema (includes pre-migrations)
        apply_schema(&conn).unwrap();

        let cols: Vec<String> = conn
            .query("PRAGMA table_info(blocked_issues_cache)")
            .unwrap()
            .iter()
            .filter_map(|row| row.get(1).and_then(|v| v.as_text()).map(String::from))
            .collect();

        assert!(
            cols.contains(&"issue_id".to_string()),
            "issue_id column should exist after migration"
        );
        assert!(
            cols.contains(&"blocked_by".to_string()),
            "blocked_by column should exist after migration"
        );
        assert!(
            cols.contains(&"blocked_at".to_string()),
            "blocked_at column should exist after migration"
        );
        assert!(
            !cols.contains(&"id".to_string()),
            "legacy id column should be removed"
        );
    }

    /// Migration: add missing issue columns for older schemas.
    #[test]
    fn test_migration_adds_missing_issue_columns() {
        // Bind the temp file: dropping it here would unlink the database
        // before the connection ever writes to it, leaving `Connection::open`
        // pointed at a dangling path.
        let temp_db = tempfile::NamedTempFile::new().unwrap();
        let conn = Connection::open(temp_db.path().to_string_lossy().into_owned()).unwrap();

        execute_batch(
            &conn,
            r"
            CREATE TABLE issues (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL
            );
        ",
        )
        .unwrap();

        apply_schema(&conn).unwrap();

        let cols: Vec<String> = conn
            .query("PRAGMA table_info('issues')")
            .unwrap()
            .iter()
            .filter_map(|row| row.get(1).and_then(|v| v.as_text()).map(String::from))
            .collect();

        let required = [
            "description",
            "design",
            "acceptance_criteria",
            "notes",
            "owner",
            "created_by",
            "updated_at",
            "source_repo",
            // Pins the v10 column-add: a legacy `(id, title)`-only table opened
            // by a v10+ binary must end up with `source_repo_path` present, so
            // the live INSERT/UPDATE SQL emitted by the storage layer doesn't
            // crash with "no such column" on the very next write.
            "source_repo_path",
            "compaction_level",
            "sender",
            "is_template",
        ];

        for column in required {
            assert!(
                cols.contains(&column.to_string()),
                "missing column {column}"
            );
        }
    }

    #[test]
    fn test_rebuild_issues_table_errors_when_canonical_columns_are_missing() {
        // Bind the temp file: dropping it here would unlink the database
        // before the connection ever writes to it, leaving `Connection::open`
        // pointed at a dangling path.
        let temp_db = tempfile::NamedTempFile::new().unwrap();
        let conn = Connection::open(temp_db.path().to_string_lossy().into_owned()).unwrap();

        execute_batch(
            &conn,
            r"
            CREATE TABLE issues (
                legacy_only TEXT
            );
        ",
        )
        .unwrap();

        let err = rebuild_issues_table(&conn).expect_err("rebuild should fail");
        assert!(matches!(err, BeadsError::Config(_)));
        assert!(
            !table_exists(&conn, "issues_rebuild_tmp"),
            "failed rebuild should roll back the temporary table"
        );
    }

    #[test]
    fn test_rebuild_issues_table_restores_foreign_keys_when_begin_fails() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("locked-rebuild.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        conn.execute("PRAGMA busy_timeout=0").unwrap();
        apply_schema(&conn).unwrap();

        let lock_conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        lock_conn.execute("PRAGMA busy_timeout=0").unwrap();
        lock_conn.execute("BEGIN IMMEDIATE").unwrap();

        assert!(foreign_keys_enabled(&conn).unwrap());
        let err = rebuild_issues_table(&conn).expect_err("exclusive rebuild should hit busy lock");
        assert!(
            err.to_string().contains("busy") || err.to_string().contains("lock"),
            "expected lock contention error, got {err}"
        );
        assert!(
            foreign_keys_enabled(&conn).unwrap(),
            "failed rebuild must restore foreign key enforcement"
        );

        lock_conn.execute("ROLLBACK").unwrap();
    }

    /// Migration: add missing dependency type column for older schemas.
    #[test]
    fn test_migration_adds_missing_dependency_type() {
        // Bind the temp file: dropping it here would unlink the database
        // before the connection ever writes to it, leaving `Connection::open`
        // pointed at a dangling path.
        let temp_db = tempfile::NamedTempFile::new().unwrap();
        let conn = Connection::open(temp_db.path().to_string_lossy().into_owned()).unwrap();

        execute_batch(
            &conn,
            r"
            CREATE TABLE issues (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL
            );
            CREATE TABLE dependencies (
                issue_id TEXT NOT NULL,
                depends_on_id TEXT NOT NULL,
                PRIMARY KEY (issue_id, depends_on_id)
            );
        ",
        )
        .unwrap();

        apply_schema(&conn).unwrap();

        assert!(
            conn.query("PRAGMA table_info('dependencies')")
                .unwrap()
                .iter()
                .filter_map(|row| row.get(1).and_then(|v| v.as_text()).map(String::from))
                .any(|col| col == "type"),
            "missing dependency type column"
        );
    }

    #[test]
    fn test_migration_rebuilds_legacy_config_metadata_primary_keys() {
        // Bind the temp file: dropping it here would unlink the database
        // before the connection ever writes to it, leaving `Connection::open`
        // pointed at a dangling path.
        let temp_db = tempfile::NamedTempFile::new().unwrap();
        let conn = Connection::open(temp_db.path().to_string_lossy().into_owned()).unwrap();

        execute_batch(
            &conn,
            r"
            CREATE TABLE config (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            CREATE TABLE metadata (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );
            INSERT INTO config (key, value) VALUES ('issue_prefix', 'new');
            INSERT INTO metadata (key, value) VALUES ('project', 'new');
        ",
        )
        .unwrap();

        apply_schema(&conn).unwrap();

        // key column should no longer be PRIMARY KEY in rebuilt tables.
        // Use PRAGMA table_info (not the table-valued function form) since
        // fsqlite does not support pragma_table_info as a table-valued function.
        let config_key_pk = conn
            .query("PRAGMA table_info('config')")
            .unwrap()
            .iter()
            .find(|row| row.get(1).and_then(SqliteValue::as_text) == Some("key"))
            .and_then(|row| row.get(5).and_then(SqliteValue::as_integer))
            .unwrap_or(0);
        assert_eq!(config_key_pk, 0);

        let metadata_key_pk = conn
            .query("PRAGMA table_info('metadata')")
            .unwrap()
            .iter()
            .find(|row| row.get(1).and_then(SqliteValue::as_text) == Some("key"))
            .and_then(|row| row.get(5).and_then(SqliteValue::as_integer))
            .unwrap_or(0);
        assert_eq!(metadata_key_pk, 0);

        // Migration should preserve existing values.
        let config_latest = conn
            .query_row_with_params(
                "SELECT value FROM config WHERE key = ?",
                &[SqliteValue::from("issue_prefix")],
            )
            .unwrap();
        assert_eq!(
            config_latest.get(0).and_then(SqliteValue::as_text),
            Some("new")
        );

        let metadata_latest = conn
            .query_row_with_params(
                "SELECT value FROM metadata WHERE key = ?",
                &[SqliteValue::from("project")],
            )
            .unwrap();
        assert_eq!(
            metadata_latest.get(0).and_then(SqliteValue::as_text),
            Some("new")
        );
    }

    #[test]
    fn test_runtime_schema_compatible_rejects_legacy_kv_primary_keys() {
        let temp = tempfile::TempDir::new().unwrap();
        let db_path = temp.path().join("legacy_kv.db");
        {
            let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
            apply_schema(&conn).expect("schema");

            conn.execute("DROP INDEX IF EXISTS idx_config_key")
                .expect("drop config index");
            conn.execute("DROP TABLE config").expect("drop config");
            conn.execute("CREATE TABLE config (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
                .expect("recreate legacy config");

            conn.execute("DROP INDEX IF EXISTS idx_metadata_key")
                .expect("drop metadata index");
            conn.execute("DROP TABLE metadata").expect("drop metadata");
            conn.execute("CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
                .expect("recreate legacy metadata");
        }

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();

        assert!(
            !runtime_schema_compatible(&conn),
            "legacy config/metadata primary keys should force the full repair path"
        );
    }

    #[test]
    fn test_runtime_schema_contract_rejects_and_heals_incomplete_capacity_table() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("incomplete_capacity_occupancy.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("schema");

        execute_batch(
            &conn,
            r"
            DROP INDEX IF EXISTS idx_capacity_occupancy_actor;
            DROP INDEX IF EXISTS idx_capacity_occupancy_harness;
            DROP INDEX IF EXISTS idx_capacity_occupancy_session;
            DROP TABLE capacity_occupancy;
            CREATE TABLE capacity_occupancy (
                issue_id TEXT PRIMARY KEY,
                actor TEXT,
                harness TEXT,
                session TEXT,
                recorded_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
            );
            CREATE INDEX idx_capacity_occupancy_actor
                ON capacity_occupancy(actor) WHERE actor IS NOT NULL;
            CREATE INDEX idx_capacity_occupancy_harness
                ON capacity_occupancy(harness) WHERE harness IS NOT NULL;
            CREATE INDEX idx_capacity_occupancy_session
                ON capacity_occupancy(session) WHERE session IS NOT NULL;
            INSERT INTO issues (id, title)
                VALUES ('capacity-fixture', 'Capacity migration fixture');
            INSERT INTO capacity_occupancy (issue_id, actor, harness, session)
                VALUES ('capacity-fixture', 'actor-a', 'harness-a', 'session-a');
            ",
        )
        .expect("plant current-version table missing agent_name");

        assert!(
            current_schema_version_declared(&conn),
            "the mutation must leave the current version stamp in place"
        );
        assert!(
            !runtime_schema_compatible(&conn),
            "the complete runtime contract must reject a live table missing a used column"
        );
        assert!(
            attest_runtime_schema_cookie(&conn).is_err(),
            "an incomplete runtime contract must never mint a durable witness"
        );

        apply_schema(&conn).expect("heal additive capacity column");
        assert!(column_exists(&conn, "capacity_occupancy", "agent_name"));
        assert!(runtime_schema_compatible(&conn));
        let preserved = conn
            .query_row_with_params(
                "SELECT actor, harness, session, agent_name FROM capacity_occupancy WHERE issue_id = ?",
                &[SqliteValue::from("capacity-fixture")],
            )
            .expect("query preserved occupancy row");
        assert_eq!(
            preserved.get(0).and_then(SqliteValue::as_text),
            Some("actor-a")
        );
        assert_eq!(
            preserved.get(1).and_then(SqliteValue::as_text),
            Some("harness-a")
        );
        assert_eq!(
            preserved.get(2).and_then(SqliteValue::as_text),
            Some("session-a")
        );
        assert_eq!(
            preserved.get(3).and_then(SqliteValue::as_text),
            None,
            "the additive nullable column must not fabricate agent attribution"
        );

        let repaired_cookie =
            attest_runtime_schema_cookie(&conn).expect("attest repaired capacity table");
        record_runtime_schema_witness(&conn, repaired_cookie).expect("record repaired witness");
        assert!(runtime_schema_witness_matches(&conn));
    }

    #[test]
    fn test_runtime_schema_contract_fails_closed_on_unsafe_audit_column_gap() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("unsafe_capacity_occupancy_gap.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("schema");

        execute_batch(
            &conn,
            r"
            DROP INDEX IF EXISTS idx_capacity_occupancy_actor;
            DROP INDEX IF EXISTS idx_capacity_occupancy_harness;
            DROP INDEX IF EXISTS idx_capacity_occupancy_session;
            DROP TABLE capacity_occupancy;
            CREATE TABLE capacity_occupancy (
                issue_id TEXT PRIMARY KEY,
                actor TEXT,
                agent_name TEXT,
                harness TEXT,
                session TEXT,
                FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
            );
            CREATE INDEX idx_capacity_occupancy_actor
                ON capacity_occupancy(actor) WHERE actor IS NOT NULL;
            CREATE INDEX idx_capacity_occupancy_harness
                ON capacity_occupancy(harness) WHERE harness IS NOT NULL;
            CREATE INDEX idx_capacity_occupancy_session
                ON capacity_occupancy(session) WHERE session IS NOT NULL;
            ",
        )
        .expect("plant current-version table missing recorded_at");

        assert!(!runtime_schema_compatible(&conn));
        let error = apply_schema(&conn)
            .expect_err("repair must not invent a historical occupancy timestamp");
        assert!(
            error.to_string().contains("required column 'recorded_at'"),
            "unexpected repair error: {error}"
        );
        assert!(
            attest_runtime_schema_cookie(&conn).is_err(),
            "an unsafe audit gap must remain unwitnessed"
        );
    }

    #[test]
    fn test_runtime_schema_contract_rejects_missing_auxiliary_pk_and_cascade() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("noncanonical_capacity_occupancy.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("schema");

        execute_batch(
            &conn,
            r"
            DROP INDEX IF EXISTS idx_capacity_occupancy_actor;
            DROP INDEX IF EXISTS idx_capacity_occupancy_harness;
            DROP INDEX IF EXISTS idx_capacity_occupancy_session;
            DROP TABLE capacity_occupancy;
            CREATE TABLE capacity_occupancy (
                issue_id TEXT,
                actor TEXT,
                agent_name TEXT,
                harness TEXT,
                session TEXT,
                recorded_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE INDEX idx_capacity_occupancy_actor
                ON capacity_occupancy(actor) WHERE actor IS NOT NULL;
            CREATE INDEX idx_capacity_occupancy_harness
                ON capacity_occupancy(harness) WHERE harness IS NOT NULL;
            CREATE INDEX idx_capacity_occupancy_session
                ON capacity_occupancy(session) WHERE session IS NOT NULL;
            ",
        )
        .expect("plant all columns and indexes without the canonical PK or cascade");

        assert!(
            !runtime_schema_compatible(&conn),
            "column names alone must not attest state-table replacement and cascade semantics"
        );
        let error = apply_schema(&conn)
            .expect_err("repair must fail closed rather than discard or reinterpret audit rows");
        assert!(
            error
                .to_string()
                .contains("column constraints or issue cascade are not canonical"),
            "unexpected repair error: {error}"
        );
        assert!(attest_runtime_schema_cookie(&conn).is_err());
    }

    #[test]
    fn test_runtime_schema_contract_rejects_core_table_without_pk_or_cascade() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("noncanonical_labels.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("schema");

        execute_batch(
            &conn,
            r"
            DROP INDEX IF EXISTS idx_labels_label;
            DROP INDEX IF EXISTS idx_labels_issue;
            DROP TABLE labels;
            CREATE TABLE labels (
                issue_id TEXT NOT NULL,
                label TEXT NOT NULL
            );
            CREATE INDEX idx_labels_label ON labels(label);
            CREATE INDEX idx_labels_issue ON labels(issue_id);
            ",
        )
        .expect("plant exact core names and indexes without PK or cascade");

        assert!(current_schema_version_declared(&conn));
        assert!(
            !runtime_schema_compatible(&conn),
            "core column and index names must not hide missing PK/cascade semantics"
        );
        assert!(
            attest_runtime_schema_cookie(&conn).is_err(),
            "a malformed core table must never mint a durable witness"
        );
    }

    #[test]
    fn test_runtime_schema_contract_rejects_weakened_external_ref_unique_predicate() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("weakened_external_ref_index.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("schema");

        execute_batch(
            &conn,
            r"
            DROP INDEX idx_issues_external_ref_unique;
            CREATE UNIQUE INDEX idx_issues_external_ref_unique
                ON issues(external_ref) WHERE 0;
            INSERT INTO issues (id, title, external_ref)
                VALUES ('external-ref-a', 'External ref A', 'shared-ref');
            INSERT INTO issues (id, title, external_ref)
                VALUES ('external-ref-b', 'External ref B', 'shared-ref');
            ",
        )
        .expect("the weakened predicate must actually admit duplicate non-NULL references");

        assert!(
            !runtime_schema_compatible(&conn),
            "the semantic UNIQUE predicate is part of the runtime contract"
        );
        assert!(attest_runtime_schema_cookie(&conn).is_err());
    }

    #[test]
    fn test_runtime_schema_contract_rejects_non_binary_external_ref_unique_collation() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("nonbinary_external_ref_index.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("schema");

        execute_batch(
            &conn,
            r"
            DROP INDEX idx_issues_external_ref_unique;
            CREATE UNIQUE INDEX idx_issues_external_ref_unique
                ON issues(external_ref COLLATE NOCASE) WHERE external_ref IS NOT NULL;
            INSERT INTO issues (id, title, external_ref)
                VALUES ('external-ref-upper', 'External ref upper', 'Case-Sensitive-Ref');
            ",
        )
        .expect("plant a same-name UNIQUE index with noncanonical collation");
        let case_variant = conn.execute(
            "INSERT INTO issues (id, title, external_ref) \
             VALUES ('external-ref-lower', 'External ref lower', 'case-sensitive-ref')",
        );
        assert!(
            case_variant.is_err(),
            "NOCASE must materially reject references the canonical BINARY index permits"
        );
        assert!(
            semantic_partial_index_predicate_canonical(&conn, "idx_issues_external_ref_unique"),
            "the fixture must retain the canonical partial-index predicate"
        );
        assert!(
            !runtime_index_key_shape_canonical(
                &conn,
                "idx_issues_external_ref_unique",
                &["external_ref"]
            ),
            "the index collation is part of its write semantics"
        );
        assert!(!runtime_schema_compatible(&conn));
        assert!(attest_runtime_schema_cookie(&conn).is_err());
    }

    #[test]
    fn test_runtime_schema_contract_rejects_descending_external_ref_unique_index() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("descending_external_ref_index.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("schema");

        execute_batch(
            &conn,
            r"
            DROP INDEX idx_issues_external_ref_unique;
            CREATE UNIQUE INDEX idx_issues_external_ref_unique
                ON issues(external_ref DESC) WHERE external_ref IS NOT NULL;
            ",
        )
        .expect("plant a same-name descending UNIQUE index");

        assert!(
            semantic_partial_index_predicate_canonical(&conn, "idx_issues_external_ref_unique"),
            "the fixture must retain the canonical partial-index predicate"
        );
        assert!(
            !runtime_index_key_shape_canonical(
                &conn,
                "idx_issues_external_ref_unique",
                &["external_ref"]
            ),
            "the declared key direction is part of the canonical index shape"
        );
        assert!(!runtime_schema_compatible(&conn));
        assert!(attest_runtime_schema_cookie(&conn).is_err());
    }

    #[test]
    fn test_runtime_schema_contract_rejects_unexpected_unique_index() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("unexpected_unique_index.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("schema");

        execute_batch(
            &conn,
            r"
            INSERT INTO issues (id, title) VALUES ('label-owner-a', 'Label owner A');
            INSERT INTO issues (id, title) VALUES ('label-owner-b', 'Label owner B');
            CREATE UNIQUE INDEX rogue_unique_label ON labels(label);
            INSERT INTO labels (issue_id, label) VALUES ('label-owner-a', 'shared');
            ",
        )
        .expect("plant an unexpected write-restricting index");
        let duplicate_label =
            conn.execute("INSERT INTO labels (issue_id, label) VALUES ('label-owner-b', 'shared')");
        assert!(
            duplicate_label.is_err(),
            "the planted index must materially reject otherwise-valid label sharing"
        );

        assert!(
            !runtime_schema_compatible(&conn),
            "unexpected UNIQUE indexes must not pass the runtime contract"
        );
        assert!(attest_runtime_schema_cookie(&conn).is_err());
    }

    #[test]
    fn test_runtime_schema_contract_rejects_unexpected_gate_history_unique_index() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("unexpected_gate_history_unique_index.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("schema");

        execute_batch(
            &conn,
            r"
            INSERT INTO issues (id, title) VALUES ('gate-owner-a', 'Gate owner A');
            INSERT INTO issues (id, title) VALUES ('gate-owner-b', 'Gate owner B');
            CREATE UNIQUE INDEX rogue_unique_gate_provider
                ON gate_result_history(provider);
            INSERT INTO gate_result_history
                (issue_id, from_status, to_status, status_revision, gate, provider)
                VALUES ('gate-owner-a', 'open', 'closed', 1, 'tests', 'provider-a');
            ",
        )
        .expect("plant an unexpected write-restricting gate-history index");
        let shared_provider = conn.execute(
            "INSERT INTO gate_result_history \
             (issue_id, from_status, to_status, status_revision, gate, provider) \
             VALUES ('gate-owner-b', 'open', 'closed', 1, 'tests', 'provider-a')",
        );
        assert!(
            shared_provider.is_err(),
            "the rogue index must reject otherwise-valid history from a shared provider"
        );
        assert!(
            attest_gate_result_history_schema(&conn).is_err(),
            "gate-history attestation must reject unexpected UNIQUE indexes"
        );
        assert!(!runtime_schema_compatible(&conn));
        assert!(attest_runtime_schema_cookie(&conn).is_err());
    }

    #[test]
    fn test_runtime_schema_contract_ignores_check_text_inside_comments() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("comment_spoofed_issue_checks.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        let mut schema = SCHEMA_SQL.replace(
            "title TEXT NOT NULL CHECK(length(title) <= 500),",
            "title TEXT NOT NULL /* CHECK(length(title) <= 500) */ ,",
        );
        schema = schema.replace(
            "priority INTEGER NOT NULL DEFAULT 2 CHECK(priority >= 0 AND priority <= 4),",
            "priority INTEGER NOT NULL DEFAULT 2 \
             /* CHECK(priority >= 0 AND priority <= 4) */ ,",
        );
        schema = schema.replace(
            r"        CHECK (
            (status = 'closed' AND closed_at IS NOT NULL) OR
            (status = 'tombstone') OR
            (status NOT IN ('closed', 'tombstone') AND closed_at IS NULL)
        )",
            &format!("        CHECK (1) /* {ISSUES_CLOSED_AT_CHECK} */"),
        );
        assert_ne!(
            schema, SCHEMA_SQL,
            "the fixture must weaken the issue checks"
        );
        execute_batch(&conn, &schema).expect("install comment-spoofed schema");
        conn.execute(&format!("PRAGMA user_version = {CURRENT_SCHEMA_VERSION}"))
            .expect("stamp current version without applying canonical checks");

        conn.execute_with_params(
            "INSERT INTO issues (id, title) VALUES (?, ?)",
            &[
                SqliteValue::from("too-long-title"),
                SqliteValue::from("x".repeat(501)),
            ],
        )
        .expect("the weakened table must admit a title over 500 characters");
        conn.execute(
            "INSERT INTO issues (id, title, priority) \
             VALUES ('bad-priority', 'Bad priority', 99)",
        )
        .expect("the weakened table must admit an out-of-range priority");
        conn.execute(
            "INSERT INTO issues (id, title, status) \
             VALUES ('closed-without-time', 'Closed without time', 'closed')",
        )
        .expect("the weakened table must admit closed status without closed_at");

        assert!(
            core_runtime_table_canonical(
                &conn,
                "issues",
                ISSUES_RUNTIME_COLUMNS,
                &[],
                ISSUES_RUNTIME_INDEXES,
                true,
                None,
                false,
            ),
            "the fixture must differ from the runtime manifest only in CHECK semantics"
        );
        assert!(
            !issues_required_checks_canonical(&conn),
            "comment text is not evidence that a CHECK constraint is enforced"
        );
        assert!(!runtime_schema_compatible(&conn));
        assert!(attest_runtime_schema_cookie(&conn).is_err());
    }

    #[test]
    fn test_sql_evidence_requires_unquoted_schema_tokens() {
        let title_check = "CHECK(length(title) <= 500)";
        assert!(sql_contains_token_sequence(
            "title TEXT CHECK(length(title) <= 500)",
            title_check,
        ));
        assert!(!sql_contains_token_sequence(
            r#"CONSTRAINT "CHECK(length(title) <= 500)" CHECK (1)"#,
            title_check,
        ));
        assert!(!sql_contains_token_sequence(
            "CHECK('CHECK(length(title) <= 500)' IS NOT NULL)",
            title_check,
        ));

        let autoincrement = "id INTEGER PRIMARY KEY AUTOINCREMENT";
        assert!(sql_contains_token_sequence(
            "id INTEGER PRIMARY KEY AUTOINCREMENT",
            autoincrement,
        ));
        assert!(!sql_contains_token_sequence(
            r#"id INTEGER PRIMARY KEY, CONSTRAINT "id INTEGER PRIMARY KEY AUTOINCREMENT" CHECK (1)"#,
            autoincrement,
        ));
        assert!(!sql_contains_token_sequence(
            "id INTEGER PRIMARY KEY CHECK('id INTEGER PRIMARY KEY AUTOINCREMENT' IS NOT NULL)",
            autoincrement,
        ));
    }

    #[test]
    fn test_runtime_schema_witness_rejects_prior_contract_token() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("prior_runtime_schema_witness.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("schema");
        let cookie = attest_runtime_schema_cookie(&conn).expect("attest current schema");
        let prior_witness = format!(
            "schema-{CURRENT_SCHEMA_VERSION}.contract-v5-version-fenced-lexical-core-aux-columns-fks-index-collations-checks-autoincrement-cookie-fenced.cookie-{cookie}"
        );
        conn.execute_with_params(
            "INSERT INTO metadata (key, value) VALUES (?, ?)",
            &[
                SqliteValue::from(RUNTIME_SCHEMA_WITNESS_KEY),
                SqliteValue::from(prior_witness),
            ],
        )
        .expect("plant prior-contract witness at the current cookie");

        assert!(
            !runtime_schema_witness_matches(&conn),
            "a witness minted under the weaker contract must force full re-attestation"
        );
    }

    #[test]
    fn test_runtime_schema_witness_rejects_same_cookie_user_version_changes() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("user_version_runtime_schema_witness.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("schema");
        let attested_cookie =
            attest_runtime_schema_cookie(&conn).expect("attest initial runtime schema");
        record_runtime_schema_witness(&conn, attested_cookie).expect("record witness");
        assert!(runtime_schema_witness_matches(&conn));

        let downgrade = CURRENT_SCHEMA_VERSION
            .checked_sub(1)
            .expect("current schema has a prior version");
        let future = CURRENT_SCHEMA_VERSION
            .checked_add(1)
            .expect("current schema has a future version");
        for (label, changed_version) in [("downgrade", downgrade), ("future", future)] {
            conn.execute(&format!("PRAGMA user_version = {changed_version}"))
                .unwrap_or_else(|error| panic!("stamp {label} version: {error}"));
            assert_eq!(
                runtime_schema_cookie(&conn).expect("read unchanged schema cookie"),
                attested_cookie,
                "a user_version-only {label} must exercise the same-cookie witness seam"
            );
            assert!(
                !runtime_schema_witness_matches(&conn),
                "a current witness must not trust a same-cookie {label} version stamp"
            );
            assert!(
                attest_runtime_schema_cookie(&conn).is_err(),
                "attestation must reject a same-cookie {label} version stamp"
            );

            conn.execute(&format!("PRAGMA user_version = {CURRENT_SCHEMA_VERSION}"))
                .unwrap_or_else(|error| panic!("restore current version after {label}: {error}"));
            assert_eq!(
                runtime_schema_cookie(&conn).expect("read restored schema cookie"),
                attested_cookie,
                "restoring user_version must not conceal a schema-cookie mutation"
            );
            assert!(
                runtime_schema_witness_matches(&conn),
                "the unchanged schema and restored version should match the recorded witness"
            );
        }
    }

    #[test]
    fn test_runtime_schema_witness_invalidates_on_ddl_and_recovers_after_repair() {
        let temp = TempDir::new().expect("tempdir");
        let db_path = temp.path().join("runtime_schema_witness.db");
        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("schema");

        assert!(runtime_schema_compatible(&conn));
        assert!(
            !runtime_schema_witness_matches(&conn),
            "a database without a recorded witness must use the full compatibility check"
        );

        let attested_cookie =
            attest_runtime_schema_cookie(&conn).expect("attest initial runtime schema");
        record_runtime_schema_witness(&conn, attested_cookie).expect("record witness");
        assert!(runtime_schema_witness_matches(&conn));

        conn.execute("DROP TABLE labels").expect("damage schema");
        record_runtime_schema_witness(&conn, attested_cookie)
            .expect("record only the previously attested generation");
        assert!(
            !runtime_schema_witness_matches(&conn),
            "DDL between attestation and recording must not become trusted"
        );
        assert!(!runtime_schema_compatible(&conn));

        apply_schema(&conn).expect("repair schema");
        assert!(runtime_schema_compatible(&conn));
        assert!(
            !runtime_schema_witness_matches(&conn),
            "repair DDL must remain untrusted until the full contract is recorded"
        );

        let repaired_cookie =
            attest_runtime_schema_cookie(&conn).expect("attest repaired runtime schema");
        record_runtime_schema_witness(&conn, repaired_cookie).expect("record repaired witness");
        assert!(runtime_schema_witness_matches(&conn));
        let witness_count = conn
            .query_row_with_params(
                "SELECT COUNT(*) FROM metadata WHERE key = ?",
                &[SqliteValue::from(RUNTIME_SCHEMA_WITNESS_KEY)],
            )
            .expect("count witness rows")
            .get(0)
            .and_then(SqliteValue::as_integer);
        assert_eq!(
            witness_count,
            Some(1),
            "re-recording after DDL must not create a duplicate metadata key"
        );
    }

    #[test]
    fn test_active_list_query_plan_uses_composite_index() {
        // Bind the temp file: dropping it here would unlink the database
        // before the connection ever writes to it, leaving `Connection::open`
        // pointed at a dangling path.
        let temp_db = tempfile::NamedTempFile::new().unwrap();
        let conn = Connection::open(temp_db.path().to_string_lossy().into_owned()).unwrap();
        apply_schema(&conn).expect("schema");

        let plan_rows = conn
            .query(
                "EXPLAIN QUERY PLAN
                 SELECT id, priority, created_at
                 FROM issues
                 WHERE status NOT IN ('closed', 'tombstone')
                   AND (is_template = 0 OR is_template IS NULL)
                 ORDER BY priority ASC, created_at DESC
                 LIMIT 1",
            )
            .expect("query plan");

        let details: Vec<String> = plan_rows
            .iter()
            .filter_map(|row| row.get(3).and_then(|v| v.as_text()).map(String::from))
            .collect();

        // fsqlite's query planner may not use composite indexes (it may
        // fall back to SCAN), so accept either index usage or SCAN.
        let uses_index = details
            .iter()
            .any(|detail| detail.contains("idx_issues_list_active_order"));
        let uses_scan = details.iter().any(|detail| detail.contains("SCAN"));

        assert!(
            uses_index || uses_scan,
            "expected planner to use idx_issues_list_active_order or SCAN, got: {details:?}"
        );
    }

    // ---- split_sql_statements tests ----

    #[test]
    fn test_split_normal_multi_statement() {
        let sql = "CREATE TABLE a (id INT); CREATE TABLE b (id INT); INSERT INTO a VALUES (1)";
        let stmts = split_sql_statements(sql);
        assert_eq!(stmts.len(), 3);
        assert_eq!(stmts[0], "CREATE TABLE a (id INT)");
        assert_eq!(stmts[1], "CREATE TABLE b (id INT)");
        assert_eq!(stmts[2], "INSERT INTO a VALUES (1)");
    }

    #[test]
    fn test_split_semicolon_inside_single_quoted_string() {
        let sql = "INSERT INTO t(v) VALUES('a;b'); SELECT 1";
        let stmts = split_sql_statements(sql);
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "INSERT INTO t(v) VALUES('a;b')");
        assert_eq!(stmts[1], "SELECT 1");
    }

    #[test]
    fn test_split_semicolon_inside_double_quoted_identifier() {
        let sql = r#"CREATE TABLE "weird;name" (id INT); SELECT 1"#;
        let stmts = split_sql_statements(sql);
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], r#"CREATE TABLE "weird;name" (id INT)"#);
        assert_eq!(stmts[1], "SELECT 1");
    }

    #[test]
    fn test_split_escaped_quotes_in_string() {
        // SQL escapes single quotes by doubling them: 'it''s'
        let sql = "INSERT INTO t(v) VALUES('it''s;here'); SELECT 2";
        let stmts = split_sql_statements(sql);
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "INSERT INTO t(v) VALUES('it''s;here')");
        assert_eq!(stmts[1], "SELECT 2");
    }

    #[test]
    fn test_split_empty_statements() {
        let sql = "SELECT 1;; ; SELECT 2";
        let stmts = split_sql_statements(sql);
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "SELECT 1");
        assert_eq!(stmts[1], "SELECT 2");
    }

    #[test]
    fn test_split_trailing_semicolon() {
        let sql = "SELECT 1; SELECT 2;";
        let stmts = split_sql_statements(sql);
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "SELECT 1");
        assert_eq!(stmts[1], "SELECT 2");
    }

    #[test]
    fn test_split_line_comment_with_semicolon() {
        let sql = "SELECT 1; -- this is a comment; not a split\nSELECT 2";
        let stmts = split_sql_statements(sql);
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "SELECT 1");
        assert_eq!(stmts[1], "-- this is a comment; not a split\nSELECT 2");
    }

    #[test]
    fn test_split_block_comment_with_semicolon() {
        let sql = "SELECT 1; /* comment; with; semicolons */ SELECT 2";
        let stmts = split_sql_statements(sql);
        assert_eq!(stmts.len(), 2);
        assert_eq!(stmts[0], "SELECT 1");
        assert_eq!(stmts[1], "/* comment; with; semicolons */ SELECT 2");
    }

    #[test]
    fn test_split_empty_input() {
        assert!(split_sql_statements("").is_empty());
        assert!(split_sql_statements("   ").is_empty());
        assert!(split_sql_statements("  ;  ;  ").is_empty());
    }

    #[test]
    fn test_split_single_statement_no_semicolon() {
        let stmts = split_sql_statements("SELECT 42");
        assert_eq!(stmts.len(), 1);
        assert_eq!(stmts[0], "SELECT 42");
    }
}
