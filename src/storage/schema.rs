//! Database schema definitions and migration logic.

use rusqlite::{Connection, Result};

pub const CURRENT_SCHEMA_VERSION: i32 = 1;

/// The complete SQL schema for the beads database.
/// Schema matches classic bd (Go) for interoperability.
pub const SCHEMA_SQL: &str = r"
    -- Issues table
    -- Note: TEXT fields use DEFAULT '' for bd (Go) compatibility.
    -- bd's sql.Scan doesn't handle NULL well when scanning into string fields.
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
        ephemeral INTEGER DEFAULT 0,
        pinned INTEGER DEFAULT 0,
        is_template INTEGER DEFAULT 0,
        await_type TEXT,
        await_id TEXT,
        timeout_ns INTEGER,
        waiters TEXT,
        hook_bead TEXT DEFAULT '',
        role_bead TEXT DEFAULT '',
        agent_state TEXT DEFAULT '',
        last_activity DATETIME,
        role_type TEXT DEFAULT '',
        rig TEXT DEFAULT '',
        -- Closed-at invariant: closed issues MUST have closed_at timestamp
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
    CREATE INDEX IF NOT EXISTS idx_dependencies_issue_id ON dependencies(issue_id);
    CREATE INDEX IF NOT EXISTS idx_dependencies_depends_on_id ON dependencies(depends_on_id);
    CREATE INDEX IF NOT EXISTS idx_dependencies_type ON dependencies(type);

    -- Labels
    CREATE TABLE IF NOT EXISTS labels (
        issue_id TEXT NOT NULL,
        label TEXT NOT NULL,
        PRIMARY KEY (issue_id, label),
        FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS idx_labels_label ON labels(label);
    CREATE INDEX IF NOT EXISTS idx_labels_issue_id ON labels(issue_id);

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
        FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS idx_events_issue ON events(issue_id);
    CREATE INDEX IF NOT EXISTS idx_events_type ON events(event_type);
    CREATE INDEX IF NOT EXISTS idx_events_created_at ON events(created_at);
    CREATE INDEX IF NOT EXISTS idx_events_actor ON events(actor) WHERE actor != '';

    -- Config (Runtime)
    CREATE TABLE IF NOT EXISTS config (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );

    -- Metadata
    CREATE TABLE IF NOT EXISTS metadata (
        key TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );

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
    -- Rebuilt on dependency or status changes
    CREATE TABLE IF NOT EXISTS blocked_issues_cache (
        issue_id TEXT PRIMARY KEY,
        blocked_by TEXT NOT NULL,  -- JSON array of blocking issue IDs
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
";

/// Apply the schema to the database.
///
/// This uses `execute_batch` to run the entire DDL script.
/// It is idempotent because all statements use `IF NOT EXISTS`.
///
/// # Errors
///
/// Returns an error if the SQL execution fails or pragmas cannot be set.
pub fn apply_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA_SQL)?;

    // Run migrations for existing databases
    run_migrations(conn)?;

    // Set journal mode to WAL for concurrency
    conn.pragma_update(None, "journal_mode", "WAL")?;

    // Enable foreign keys
    conn.pragma_update(None, "foreign_keys", "ON")?;

    Ok(())
}

/// Run schema migrations for existing databases.
///
/// This handles upgrades for tables that may have been created with older schemas.
#[allow(clippy::too_many_lines)]
fn run_migrations(conn: &Connection) -> Result<()> {
    // Migration: Ensure blocked_issues_cache has correct schema (blocked_by, blocked_at)
    // Check for old column name (blocked_by_json) or missing columns
    let has_blocked_by: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('blocked_issues_cache') WHERE name='blocked_by'")
        .and_then(|mut stmt| stmt.exists([]))
        .unwrap_or(false);

    let has_blocked_at: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('blocked_issues_cache') WHERE name='blocked_at'")
        .and_then(|mut stmt| stmt.exists([]))
        .unwrap_or(false);

    if !has_blocked_by || !has_blocked_at {
        // Table needs update - drop and recreate (it's a cache, data is regenerated)
        conn.execute("DROP TABLE IF EXISTS blocked_issues_cache", [])?;
        conn.execute(
            "CREATE TABLE blocked_issues_cache (
                issue_id TEXT PRIMARY KEY,
                blocked_by TEXT NOT NULL,
                blocked_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
            )",
            [],
        )?;
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_blocked_cache_blocked_at ON blocked_issues_cache(blocked_at)",
            [],
        )?;
    }

    // Migration: ensure compaction_level is never NULL (bd compatibility)
    let has_compaction_level: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('issues') WHERE name='compaction_level'")
        .and_then(|mut stmt| stmt.exists([]))
        .unwrap_or(false);

    if has_compaction_level {
        conn.execute(
            "UPDATE issues SET compaction_level = 0 WHERE compaction_level IS NULL",
            [],
        )?;
    }

    // Migration: ensure source_repo column exists (bd compatibility)
    let has_source_repo: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('issues') WHERE name='source_repo'")
        .and_then(|mut stmt| stmt.exists([]))
        .unwrap_or(false);

    if !has_source_repo {
        conn.execute(
            "ALTER TABLE issues ADD COLUMN source_repo TEXT NOT NULL DEFAULT '.'",
            [],
        )?;
    }

    // Migration: ensure gate columns exist (bd compatibility)
    let has_await_type: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('issues') WHERE name='await_type'")
        .and_then(|mut stmt| stmt.exists([]))
        .unwrap_or(false);

    if !has_await_type {
        conn.execute_batch(
            r"
            ALTER TABLE issues ADD COLUMN await_type TEXT;
            ALTER TABLE issues ADD COLUMN await_id TEXT;
            ALTER TABLE issues ADD COLUMN timeout_ns INTEGER;
            ALTER TABLE issues ADD COLUMN waiters TEXT;
        ",
        )?;
    }

    // Migration: ensure Gastown columns exist (bd compatibility)
    let has_hook_bead: bool = conn
        .prepare("SELECT 1 FROM pragma_table_info('issues') WHERE name='hook_bead'")
        .and_then(|mut stmt| stmt.exists([]))
        .unwrap_or(false);

    if !has_hook_bead {
        conn.execute_batch(
            r"
            ALTER TABLE issues ADD COLUMN hook_bead TEXT DEFAULT '';
            ALTER TABLE issues ADD COLUMN role_bead TEXT DEFAULT '';
            ALTER TABLE issues ADD COLUMN agent_state TEXT DEFAULT '';
            ALTER TABLE issues ADD COLUMN last_activity DATETIME;
            ALTER TABLE issues ADD COLUMN role_type TEXT DEFAULT '';
            ALTER TABLE issues ADD COLUMN rig TEXT DEFAULT '';
        ",
        )?;
    }

    // Migration: Add missing indexes for bd parity
    // These use IF NOT EXISTS so they're safe to run multiple times
    conn.execute_batch(
        r"
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

        -- Dependency composite index
        CREATE INDEX IF NOT EXISTS idx_dependencies_composite ON dependencies(issue_id, depends_on_id, type);

        -- Comments index with canonical name
        CREATE INDEX IF NOT EXISTS idx_comments_issue ON comments(issue_id);

        -- Events indexes with canonical names
        CREATE INDEX IF NOT EXISTS idx_events_issue ON events(issue_id);
        CREATE INDEX IF NOT EXISTS idx_events_type ON events(event_type);
        CREATE INDEX IF NOT EXISTS idx_events_actor ON events(actor) WHERE actor != '';
    ",
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use std::collections::HashSet;

    #[test]
    fn test_apply_schema() {
        let conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn).expect("Failed to apply schema");

        // Verify a few tables exist
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table'")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert!(tables.contains(&"issues".to_string()));
        assert!(tables.contains(&"dependencies".to_string()));
        assert!(tables.contains(&"config".to_string()));
        assert!(tables.contains(&"dirty_issues".to_string()));

        // Verify pragmas
        let journal_mode: String = conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        // In-memory DBs use MEMORY journaling, regardless of what we set
        assert!(journal_mode.to_uppercase() == "WAL" || journal_mode.to_uppercase() == "MEMORY");

        let foreign_keys: i32 = conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(foreign_keys, 1);
    }

    /// Conformance test: Verify schema matches bd (Go) for interoperability.
    /// Tests table structure, defaults, constraints, and indexes.
    #[test]
    #[allow(clippy::too_many_lines)]
    fn test_schema_parity_conformance() {
        let conn = Connection::open_in_memory().unwrap();
        apply_schema(&conn).expect("Failed to apply schema");

        // === ISSUES TABLE ===
        // Verify column defaults
        let issues_cols: Vec<(String, String, i32, Option<String>)> = conn
            .prepare("PRAGMA table_info(issues)")
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(1)?,         // name
                    row.get::<_, String>(2)?,         // type
                    row.get::<_, i32>(3)?,            // notnull
                    row.get::<_, Option<String>>(4)?, // dflt_value
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

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
            .prepare("SELECT name FROM sqlite_master WHERE type='index' AND sql IS NOT NULL")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<HashSet<_>, _>>()
            .unwrap();

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
            indexes.contains("idx_issues_external_ref")
                || indexes.contains("idx_issues_external_ref_unique"),
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

        // === DEPENDENCIES TABLE ===
        let deps_cols: Vec<(String, Option<String>)> = conn
            .prepare("PRAGMA table_info(dependencies)")
            .unwrap()
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(1)?,         // name
                    row.get::<_, Option<String>>(4)?, // dflt_value
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

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

        // === BLOCKED_ISSUES_CACHE TABLE ===
        let cache_cols: Vec<String> = conn
            .prepare("PRAGMA table_info(blocked_issues_cache)")
            .unwrap()
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

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
        conn.execute(
            "INSERT INTO issues (id, title) VALUES ('test-1', 'Test Issue')",
            [],
        )
        .expect("Should allow open issue without closed_at");

        // Try to insert closed issue without closed_at - should fail
        let result = conn.execute(
            "INSERT INTO issues (id, title, status) VALUES ('test-2', 'Closed', 'closed')",
            [],
        );
        assert!(
            result.is_err(),
            "Should reject closed issue without closed_at timestamp"
        );

        // Insert closed issue with closed_at - should succeed
        conn.execute(
            "INSERT INTO issues (id, title, status, closed_at) VALUES ('test-3', 'Closed', 'closed', CURRENT_TIMESTAMP)",
            [],
        )
        .expect("Should allow closed issue with closed_at");

        // Insert tombstone without closed_at - should succeed (tombstones exempt)
        conn.execute(
            "INSERT INTO issues (id, title, status) VALUES ('test-4', 'Tombstone', 'tombstone')",
            [],
        )
        .expect("Should allow tombstone without closed_at");
    }

    /// Test that migrations correctly upgrade old schemas.
    #[test]
    fn test_migration_blocked_cache_upgrade() {
        let conn = Connection::open_in_memory().unwrap();

        // Create old-style blocked_issues_cache with blocked_by_json
        // Using a complete issues table schema so index migrations succeed
        conn.execute_batch(
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
                issue_id TEXT NOT NULL
            );
            CREATE TABLE events (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                issue_id TEXT NOT NULL,
                event_type TEXT NOT NULL,
                actor TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE blocked_issues_cache (
                issue_id TEXT PRIMARY KEY,
                blocked_by_json TEXT NOT NULL
            );
        ",
        )
        .unwrap();

        // Run migrations
        run_migrations(&conn).unwrap();

        // Verify columns were updated
        let cols: Vec<String> = conn
            .prepare("PRAGMA table_info(blocked_issues_cache)")
            .unwrap()
            .query_map([], |row| row.get(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

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
}
