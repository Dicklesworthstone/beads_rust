//! Database schema definitions and migration logic.

use rusqlite::{Connection, Result};

pub const CURRENT_SCHEMA_VERSION: i32 = 1;

/// The complete SQL schema for the beads database.
pub const SCHEMA_SQL: &str = r"
    -- Issues table
    -- Note: TEXT fields use NOT NULL DEFAULT '' for bd (Go) compatibility.
    -- bd's sql.Scan doesn't handle NULL well when scanning into string fields.
    CREATE TABLE IF NOT EXISTS issues (
        id TEXT PRIMARY KEY,
        content_hash TEXT,
        title TEXT NOT NULL,
        description TEXT NOT NULL DEFAULT '',
        design TEXT NOT NULL DEFAULT '',
        acceptance_criteria TEXT NOT NULL DEFAULT '',
        notes TEXT NOT NULL DEFAULT '',
        status TEXT NOT NULL,
        priority INTEGER NOT NULL,
        issue_type TEXT NOT NULL,
        assignee TEXT,
        owner TEXT NOT NULL DEFAULT '',
        estimated_minutes INTEGER,
        created_at TEXT NOT NULL,
        created_by TEXT NOT NULL DEFAULT '',
        updated_at TEXT NOT NULL,
        closed_at TEXT,
        close_reason TEXT NOT NULL DEFAULT '',
        closed_by_session TEXT NOT NULL DEFAULT '',
        due_at TEXT,
        defer_until TEXT,
        external_ref TEXT,
        source_system TEXT NOT NULL DEFAULT '',
        deleted_at TEXT,
        deleted_by TEXT NOT NULL DEFAULT '',
        delete_reason TEXT NOT NULL DEFAULT '',
        original_type TEXT NOT NULL DEFAULT '',
        compaction_level INTEGER DEFAULT 0,
        compacted_at TEXT,
        compacted_at_commit TEXT,
        original_size INTEGER DEFAULT 0,
        sender TEXT NOT NULL DEFAULT '',
        ephemeral INTEGER DEFAULT 0,
        pinned INTEGER DEFAULT 0,
        is_template INTEGER DEFAULT 0,
        CHECK (length(title) >= 1 AND length(title) <= 500),
        CHECK (priority >= 0 AND priority <= 4)
    );

    CREATE INDEX IF NOT EXISTS idx_issues_status ON issues(status);
    CREATE INDEX IF NOT EXISTS idx_issues_priority ON issues(priority);
    CREATE INDEX IF NOT EXISTS idx_issues_issue_type ON issues(issue_type);
    CREATE INDEX IF NOT EXISTS idx_issues_assignee ON issues(assignee);
    CREATE INDEX IF NOT EXISTS idx_issues_created_at ON issues(created_at);
    CREATE INDEX IF NOT EXISTS idx_issues_updated_at ON issues(updated_at);
    CREATE UNIQUE INDEX IF NOT EXISTS idx_issues_external_ref ON issues(external_ref) WHERE external_ref IS NOT NULL;

    -- Dependencies
    CREATE TABLE IF NOT EXISTS dependencies (
        issue_id TEXT NOT NULL,
        depends_on_id TEXT NOT NULL,
        type TEXT NOT NULL,
        created_at TEXT NOT NULL,
        created_by TEXT,
        metadata TEXT,
        thread_id TEXT,
        PRIMARY KEY (issue_id, depends_on_id)
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
        created_at TEXT NOT NULL,
        FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS idx_comments_issue_id ON comments(issue_id);
    CREATE INDEX IF NOT EXISTS idx_comments_created_at ON comments(created_at);

    -- Events (Audit)
    CREATE TABLE IF NOT EXISTS events (
        id INTEGER PRIMARY KEY,
        issue_id TEXT NOT NULL,
        event_type TEXT NOT NULL,
        actor TEXT NOT NULL,
        old_value TEXT,
        new_value TEXT,
        comment TEXT,
        created_at TEXT NOT NULL,
        FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS idx_events_issue_id ON events(issue_id);
    CREATE INDEX IF NOT EXISTS idx_events_event_type ON events(event_type);
    CREATE INDEX IF NOT EXISTS idx_events_created_at ON events(created_at);
    CREATE INDEX IF NOT EXISTS idx_events_actor ON events(actor);

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
        marked_at TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS idx_dirty_issues_marked_at ON dirty_issues(marked_at);

    -- Export Hashes (for incremental export)
    CREATE TABLE IF NOT EXISTS export_hashes (
        issue_id TEXT PRIMARY KEY,
        content_hash TEXT NOT NULL,
        exported_at TEXT NOT NULL
    );

    -- Blocked Issues Cache (Materialized view)
    CREATE TABLE IF NOT EXISTS blocked_issues_cache (
        issue_id TEXT PRIMARY KEY,
        blocked_by_json TEXT NOT NULL
    );

    -- Child Counters
    CREATE TABLE IF NOT EXISTS child_counters (
        parent_id TEXT PRIMARY KEY,
        next_child_number INTEGER NOT NULL DEFAULT 1
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
fn run_migrations(conn: &Connection) -> Result<()> {
    // Migration: Ensure blocked_issues_cache has blocked_by_json column
    // If the table exists but lacks the column, drop and recreate it (it's a cache)
    let has_blocked_by_json: bool = conn
        .prepare(
            "SELECT 1 FROM pragma_table_info('blocked_issues_cache') WHERE name='blocked_by_json'",
        )
        .and_then(|mut stmt| stmt.exists([]))
        .unwrap_or(false);

    if !has_blocked_by_json {
        // Table exists but lacks the column - drop and recreate
        conn.execute("DROP TABLE IF EXISTS blocked_issues_cache", [])?;
        conn.execute(
            "CREATE TABLE blocked_issues_cache (
                issue_id TEXT PRIMARY KEY,
                blocked_by_json TEXT NOT NULL
            )",
            [],
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

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
}
