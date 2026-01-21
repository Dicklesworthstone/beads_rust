//! `SQLite` storage implementation.

use crate::error::{BeadsError, Result};
use crate::format::{IssueDetails, IssueWithDependencyMetadata};
use crate::model::{Comment, DependencyType, Event, EventType, Issue, IssueType, Priority, Status};
use crate::storage::events::get_events;
use crate::storage::schema::apply_schema;
use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use rusqlite::{Connection, OpenFlags, OptionalExtension, Transaction};
use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// SQLite-based storage backend.
#[derive(Debug)]
pub struct SqliteStorage {
    conn: Connection,
}

/// Context for a mutation operation, tracking side effects.
pub struct MutationContext {
    pub op_name: String,
    pub actor: String,
    pub events: Vec<Event>,
    pub dirty_ids: HashSet<String>,
    pub invalidate_blocked_cache: bool,
}

impl MutationContext {
    #[must_use]
    pub fn new(op_name: &str, actor: &str) -> Self {
        Self {
            op_name: op_name.to_string(),
            actor: actor.to_string(),
            events: Vec::new(),
            dirty_ids: HashSet::new(),
            invalidate_blocked_cache: false,
        }
    }

    pub fn record_event(&mut self, event_type: EventType, issue_id: &str, details: Option<String>) {
        self.events.push(Event {
            id: 0, // Placeholder, DB assigns auto-inc ID
            issue_id: issue_id.to_string(),
            event_type,
            actor: self.actor.clone(),
            old_value: None,
            new_value: None,
            comment: details,
            created_at: Utc::now(),
        });
    }

    /// Record a field change event with old and new values.
    pub fn record_field_change(
        &mut self,
        event_type: EventType,
        issue_id: &str,
        old_value: Option<String>,
        new_value: Option<String>,
        comment: Option<String>,
    ) {
        self.events.push(Event {
            id: 0,
            issue_id: issue_id.to_string(),
            event_type,
            actor: self.actor.clone(),
            old_value,
            new_value,
            comment,
            created_at: Utc::now(),
        });
    }

    pub fn mark_dirty(&mut self, issue_id: &str) {
        self.dirty_ids.insert(issue_id.to_string());
    }

    pub const fn invalidate_cache(&mut self) {
        self.invalidate_blocked_cache = true;
    }
}

impl SqliteStorage {
    /// Open a new connection to the database at the given path.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection cannot be established or schema application fails.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_timeout(path, None)
    }

    /// Open a new connection with an optional busy timeout (ms).
    ///
    /// # Errors
    ///
    /// Returns an error if the connection cannot be established or schema application fails.
    pub fn open_with_timeout(path: &Path, lock_timeout_ms: Option<u64>) -> Result<Self> {
        let conn = Connection::open(path)?;
        if let Some(timeout) = lock_timeout_ms {
            conn.busy_timeout(Duration::from_millis(timeout))?;
        }
        apply_schema(&conn)?;
        Ok(Self { conn })
    }

    /// Open an in-memory database for testing.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection cannot be established.
    pub fn open_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        apply_schema(&conn)?;
        Ok(Self { conn })
    }

    /// Get audit events for a specific issue.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_events(&self, issue_id: &str, limit: usize) -> Result<Vec<Event>> {
        crate::storage::events::get_events(&self.conn, issue_id, limit)
    }

    /// Get all audit events (for summary).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_all_events(&self, limit: usize) -> Result<Vec<Event>> {
        crate::storage::events::get_all_events(&self.conn, limit)
    }

    /// Execute a mutation with the 4-step transaction protocol.
    ///
    /// # Errors
    ///
    /// Returns an error if any step fails (e.g. database error, logic error).
    /// The transaction is rolled back on error.
    pub fn mutate<F, R>(&mut self, op: &str, actor: &str, f: F) -> Result<R>
    where
        F: FnOnce(&Transaction, &mut MutationContext) -> Result<R>,
    {
        let tx = self
            .conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let mut ctx = MutationContext::new(op, actor);

        let result = f(&tx, &mut ctx)?;

        // Write events
        for event in ctx.events {
            tx.execute(
                "INSERT INTO events (issue_id, event_type, actor, old_value, new_value, comment, created_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    event.issue_id,
                    event.event_type.as_str(),
                    event.actor,
                    event.old_value,
                    event.new_value,
                    event.comment,
                    event.created_at.to_rfc3339()
                ],
            )?;
        }

        // Mark dirty
        for id in ctx.dirty_ids {
            tx.execute(
                "INSERT OR REPLACE INTO dirty_issues (issue_id, marked_at) VALUES (?, ?)",
                rusqlite::params![id, Utc::now().to_rfc3339()],
            )?;
        }

        // Track if we need to rebuild cache
        let needs_cache_rebuild = ctx.invalidate_blocked_cache;

        // Rebuild blocked cache inside the transaction if needed
        if needs_cache_rebuild {
            Self::rebuild_blocked_cache_impl(&tx)?;
        }

        tx.commit()?;

        Ok(result)
    }

    /// Create a new issue.
    ///
    /// # Errors
    ///
    /// Returns an error if the issue cannot be inserted (e.g. ID collision).
    #[allow(clippy::too_many_lines)]
    pub fn create_issue(&mut self, issue: &Issue, actor: &str) -> Result<()> {
        self.mutate("create_issue", actor, |tx, ctx| {
            let status_str = issue.status.as_str();
            let issue_type_str = issue.issue_type.as_str();
            let created_at_str = issue.created_at.to_rfc3339();
            let updated_at_str = issue.updated_at.to_rfc3339();
            let closed_at_str = issue.closed_at.map(|dt| dt.to_rfc3339());
            let due_at_str = issue.due_at.map(|dt| dt.to_rfc3339());
            let defer_until_str = issue.defer_until.map(|dt| dt.to_rfc3339());
            let deleted_at_str = issue.deleted_at.map(|dt| dt.to_rfc3339());
            let compacted_at_str = issue.compacted_at.map(|dt| dt.to_rfc3339());

            tx.execute(
                "INSERT INTO issues (
                    id, content_hash, title, description, design, acceptance_criteria, notes,
                    status, priority, issue_type, assignee, owner, estimated_minutes,
                    created_at, created_by, updated_at, closed_at, close_reason,
                    closed_by_session, due_at, defer_until, external_ref, source_system,
                    source_repo, deleted_at, deleted_by, delete_reason, original_type,
                    compaction_level, compacted_at, compacted_at_commit, original_size,
                    sender, ephemeral, pinned, is_template
                            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",                rusqlite::params![
                    issue.id,
                    issue.content_hash,
                    issue.title,
                    issue.description.as_deref().unwrap_or(""),
                    issue.design.as_deref().unwrap_or(""),
                    issue.acceptance_criteria.as_deref().unwrap_or(""),
                    issue.notes.as_deref().unwrap_or(""),
                    status_str,
                    issue.priority.0,
                    issue_type_str,
                    issue.assignee,
                    issue.owner.as_deref().unwrap_or(""),
                    issue.estimated_minutes,
                    created_at_str,
                    issue.created_by.as_deref().unwrap_or(""),
                    updated_at_str,
                    closed_at_str,
                    issue.close_reason.as_deref().unwrap_or(""),
                    issue.closed_by_session.as_deref().unwrap_or(""),
                    due_at_str,
                    defer_until_str,
                    issue.external_ref,
                    issue.source_system.as_deref().unwrap_or(""),
                    issue.source_repo.as_deref().unwrap_or("."),
                    deleted_at_str,
                    issue.deleted_by.as_deref().unwrap_or(""),
                    issue.delete_reason.as_deref().unwrap_or(""),
                    issue.original_type.as_deref().unwrap_or(""),
                    issue.compaction_level.unwrap_or(0),
                    compacted_at_str,
                    issue.compacted_at_commit,
                    issue.original_size.unwrap_or(0),
                    issue.sender.as_deref().unwrap_or(""),
                    i32::from(issue.ephemeral),
                    i32::from(issue.pinned),
                    i32::from(issue.is_template),
                ],
            )?;

            // Insert Labels
            for label in &issue.labels {
                tx.execute(
                    "INSERT INTO labels (issue_id, label) VALUES (?, ?)",
                    rusqlite::params![issue.id, label],
                )?;
                ctx.record_event(
                    EventType::LabelAdded,
                    &issue.id,
                    Some(format!("Added label {label}")),
                );
            }

            // Insert Dependencies
            for dep in &issue.dependencies {
                // Check cycle if blocking
                if dep.dep_type.is_blocking()
                    && Self::check_cycle(tx, &issue.id, &dep.depends_on_id, true)?
                {
                    return Err(BeadsError::DependencyCycle {
                        path: format!(
                            "Adding dependency {} -> {} would create a cycle",
                            issue.id, dep.depends_on_id
                        ),
                    });
                }

                tx.execute(
                    "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by)
                     VALUES (?, ?, ?, ?, ?)",
                    rusqlite::params![
                        issue.id,
                        dep.depends_on_id,
                        dep.dep_type.as_str(),
                        dep.created_at.to_rfc3339(),
                        dep.created_by.as_deref().unwrap_or(actor)
                    ],
                )?;

                ctx.record_event(
                    EventType::DependencyAdded,
                    &issue.id,
                    Some(format!(
                        "Added dependency on {} ({})",
                        dep.depends_on_id, dep.dep_type
                    )),
                );
                ctx.invalidate_cache();
            }

            // Insert Comments
            for comment in &issue.comments {
                tx.execute(
                    "INSERT INTO comments (issue_id, author, text, created_at) VALUES (?, ?, ?, ?)",
                    rusqlite::params![
                        issue.id,
                        comment.author,
                        comment.body,
                        comment.created_at.to_rfc3339()
                    ],
                )?;
                ctx.record_event(
                    EventType::Commented,
                    &issue.id,
                    Some(comment.body.clone()),
                );
            }

            ctx.record_event(
                EventType::Created,
                &issue.id,
                Some(format!("Created issue: {}", issue.title)),
            );

            ctx.mark_dirty(&issue.id);

            Ok(())
        })
    }

    // Helper for cycle detection (refactored from would_create_cycle)
    fn check_cycle(
        conn: &Connection,
        issue_id: &str,
        depends_on_id: &str,
        blocking_only: bool,
    ) -> Result<bool> {
        // Construct filter clause
        let type_filter = if blocking_only {
            "AND type IN ('blocks', 'parent-child', 'conditional-blocks')"
        } else {
            "" // No filter, follow all edges
        };

        let query = format!(
            r"
            WITH RECURSIVE transitive_deps(id) AS (
                -- Base case: direct dependencies of starting point
                SELECT depends_on_id FROM dependencies 
                WHERE issue_id = ?1 {type_filter}
                UNION
                -- Recursive step: follow dependencies forward
                SELECT d.depends_on_id
                FROM dependencies d
                JOIN transitive_deps td ON d.issue_id = td.id
                WHERE 1=1 {type_filter}
            )
            SELECT 1 FROM transitive_deps WHERE id = ?2 LIMIT 1;
            "
        );

        let exists: bool = conn
            .query_row(&query, rusqlite::params![depends_on_id, issue_id], |_| {
                Ok(true)
            })
            .optional()?
            .unwrap_or(false);

        Ok(exists)
    }

    /// Update an issue's fields.
    ///
    /// # Errors
    ///
    /// Returns an error if the issue doesn't exist or the update fails.
    #[allow(clippy::too_many_lines)]
    pub fn update_issue(&mut self, id: &str, updates: &IssueUpdate, actor: &str) -> Result<Issue> {
        let mut issue = self
            .get_issue(id)?
            .ok_or_else(|| BeadsError::IssueNotFound { id: id.to_string() })?;

        if updates.is_empty() {
            return Ok(issue);
        }

        self.mutate("update_issue", actor, |tx, ctx| {
            let mut set_clauses: Vec<String> = vec![];
            let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![];

            // Helper to add update
            let mut add_update = |field: &str, val: Box<dyn rusqlite::ToSql>| {
                set_clauses.push(format!("{field} = ?"));
                params.push(val);
            };

            // Title
            if let Some(ref title) = updates.title {
                let old_title = issue.title.clone();
                issue.title.clone_from(title);
                add_update("title", Box::new(title.clone()));
                ctx.record_field_change(
                    EventType::Updated,
                    id,
                    Some(old_title),
                    Some(title.clone()),
                    Some("Title changed".to_string()),
                );
            }

            // Simple text fields - use empty string instead of NULL for bd compatibility
            if let Some(ref val) = updates.description {
                issue.description.clone_from(val);
                add_update(
                    "description",
                    Box::new(val.as_deref().unwrap_or("").to_string()),
                );
            }
            if let Some(ref val) = updates.design {
                issue.design.clone_from(val);
                add_update("design", Box::new(val.as_deref().unwrap_or("").to_string()));
            }
            if let Some(ref val) = updates.acceptance_criteria {
                issue.acceptance_criteria.clone_from(val);
                add_update(
                    "acceptance_criteria",
                    Box::new(val.as_deref().unwrap_or("").to_string()),
                );
            }
            if let Some(ref val) = updates.notes {
                issue.notes.clone_from(val);
                add_update("notes", Box::new(val.as_deref().unwrap_or("").to_string()));
            }

            // Status
            if let Some(ref status) = updates.status {
                let old_status = issue.status.as_str().to_string();
                issue.status.clone_from(status);
                add_update("status", Box::new(status.as_str().to_string()));
                ctx.record_field_change(
                    EventType::StatusChanged,
                    id,
                    Some(old_status),
                    Some(status.as_str().to_string()),
                    None,
                );

                // Record Closed event if status is now Closed
                if *status == Status::Closed {
                    let reason = updates.close_reason.as_ref().and_then(Clone::clone);
                    ctx.record_event(EventType::Closed, id, reason);

                    // Auto-set closed_at if not provided
                    if updates.closed_at.is_none() && issue.closed_at.is_none() {
                        let now = Utc::now();
                        issue.closed_at = Some(now);
                        add_update("closed_at", Box::new(Some(now.to_rfc3339())));
                    }
                } else if issue.closed_at.is_some() && updates.closed_at.is_none() {
                    // Reopening (or fixing state): Clear closed_at if it was set
                    issue.closed_at = None;
                    add_update("closed_at", Box::new(None::<String>));
                }

                if !updates.skip_cache_rebuild {
                    ctx.invalidate_cache();
                }
            }

            // Priority
            if let Some(priority) = updates.priority {
                let old_priority = issue.priority.0;
                issue.priority = priority;
                add_update("priority", Box::new(priority.0));
                if priority.0 != old_priority {
                    ctx.record_field_change(
                        EventType::PriorityChanged,
                        id,
                        Some(old_priority.to_string()),
                        Some(priority.0.to_string()),
                        None,
                    );
                }
            }

            // Issue type
            if let Some(ref issue_type) = updates.issue_type {
                issue.issue_type.clone_from(issue_type);
                add_update("issue_type", Box::new(issue_type.as_str().to_string()));
            }

            // Assignee
            if let Some(ref assignee_opt) = updates.assignee {
                let old_assignee = issue.assignee.clone();
                issue.assignee.clone_from(assignee_opt);
                add_update("assignee", Box::new(assignee_opt.clone()));
                if old_assignee != *assignee_opt {
                    ctx.record_field_change(
                        EventType::AssigneeChanged,
                        id,
                        old_assignee,
                        assignee_opt.clone(),
                        None,
                    );
                }
            }

            // Simple Option fields - use empty string instead of NULL for bd compatibility
            if let Some(ref val) = updates.owner {
                issue.owner.clone_from(val);
                add_update("owner", Box::new(val.as_deref().unwrap_or("").to_string()));
            }
            if let Some(ref val) = updates.estimated_minutes {
                issue.estimated_minutes = *val;
                add_update("estimated_minutes", Box::new(*val));
            }
            if let Some(ref val) = updates.external_ref {
                issue.external_ref.clone_from(val);
                add_update("external_ref", Box::new(val.clone()));
            }
            // Use empty string instead of NULL for bd compatibility
            if let Some(ref val) = updates.close_reason {
                issue.close_reason.clone_from(val);
                add_update(
                    "close_reason",
                    Box::new(val.as_deref().unwrap_or("").to_string()),
                );
            }
            if let Some(ref val) = updates.closed_by_session {
                issue.closed_by_session.clone_from(val);
                add_update(
                    "closed_by_session",
                    Box::new(val.as_deref().unwrap_or("").to_string()),
                );
            }

            // Tombstone fields
            if let Some(ref val) = updates.deleted_at {
                issue.deleted_at = *val;
                add_update("deleted_at", Box::new(val.map(|d| d.to_rfc3339())));
            }
            // Use empty string instead of NULL for bd compatibility
            if let Some(ref val) = updates.deleted_by {
                issue.deleted_by.clone_from(val);
                add_update(
                    "deleted_by",
                    Box::new(val.as_deref().unwrap_or("").to_string()),
                );
            }
            if let Some(ref val) = updates.delete_reason {
                issue.delete_reason.clone_from(val);
                add_update(
                    "delete_reason",
                    Box::new(val.as_deref().unwrap_or("").to_string()),
                );
            }

            // Date fields
            if let Some(ref val) = updates.due_at {
                issue.due_at = *val;
                add_update("due_at", Box::new(val.map(|d| d.to_rfc3339())));
            }
            if let Some(ref val) = updates.defer_until {
                issue.defer_until = *val;
                add_update("defer_until", Box::new(val.map(|d| d.to_rfc3339())));
            }
            if let Some(ref val) = updates.closed_at {
                issue.closed_at = *val;
                add_update("closed_at", Box::new(val.map(|d| d.to_rfc3339())));
            }

            // Always update updated_at
            set_clauses.push("updated_at = ?".to_string());
            params.push(Box::new(Utc::now().to_rfc3339()));

            // Update content hash
            let new_hash = issue.compute_content_hash();
            set_clauses.push("content_hash = ?".to_string());
            params.push(Box::new(new_hash));

            // Build and execute SQL
            let sql = format!("UPDATE issues SET {} WHERE id = ? ", set_clauses.join(", "));
            params.push(Box::new(id.to_string()));

            let params_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(AsRef::as_ref).collect();
            tx.execute(&sql, params_refs.as_slice())?;

            ctx.mark_dirty(id);

            Ok(())
        })?;

        // Return updated issue
        self.get_issue(id)?
            .ok_or_else(|| BeadsError::IssueNotFound { id: id.to_string() })
    }

    /// Delete an issue by creating a tombstone.
    ///
    /// # Errors
    ///
    /// Returns an error if the issue doesn't exist or the update fails.
    pub fn delete_issue(
        &mut self,
        id: &str,
        actor: &str,
        reason: &str,
        deleted_at: Option<DateTime<Utc>>,
    ) -> Result<Issue> {
        let issue = self
            .get_issue(id)?
            .ok_or_else(|| BeadsError::IssueNotFound { id: id.to_string() })?;

        let original_type = issue.issue_type.as_str().to_string();
        let timestamp = deleted_at.unwrap_or_else(Utc::now);

        self.mutate("delete_issue", actor, |tx, ctx| {
            tx.execute(
                "UPDATE issues SET
                    status = 'tombstone',
                    deleted_at = ?,
                    deleted_by = ?,
                    delete_reason = ?,
                    original_type = ?,
                    updated_at = ?
                 WHERE id = ?",
                rusqlite::params![
                    timestamp.to_rfc3339(),
                    actor,
                    reason,
                    original_type,
                    Utc::now().to_rfc3339(),
                    id
                ],
            )?;

            ctx.record_event(
                EventType::Deleted,
                id,
                Some(format!("Deleted issue: {reason}")),
            );
            ctx.mark_dirty(id);
            ctx.invalidate_cache();

            Ok(())
        })?;

        self.get_issue(id)?
            .ok_or_else(|| BeadsError::IssueNotFound { id: id.to_string() })
    }

    /// Get an issue by ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_issue(&self, id: &str) -> Result<Option<Issue>> {
        let sql = r" 
            SELECT id, content_hash, title, description, design, acceptance_criteria, notes,
                   status, priority, issue_type, assignee, owner, estimated_minutes,
                   created_at, created_by, updated_at, closed_at, close_reason, closed_by_session,
                   due_at, defer_until, external_ref, source_system, source_repo,
                   deleted_at, deleted_by, delete_reason, original_type,
                   compaction_level, compacted_at, compacted_at_commit, original_size,
                   sender, ephemeral, pinned, is_template
            FROM issues WHERE id = ?
        ";

        let mut stmt = self.conn.prepare(sql)?;
        let result = stmt.query_row([id], |row| self.issue_from_row(row));

        match result {
            Ok(issue) => Ok(Some(issue)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get multiple issues by ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_issues_by_ids(&self, ids: &[String]) -> Result<Vec<Issue>> {
        const SQLITE_VAR_LIMIT: usize = 900;

        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut issues = Vec::new();

        for chunk in ids.chunks(SQLITE_VAR_LIMIT) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            let sql = format!(
                r"SELECT id, content_hash, title, description, design, acceptance_criteria, notes,
                         status, priority, issue_type, assignee, owner, estimated_minutes,
                         created_at, created_by, updated_at, closed_at, close_reason, closed_by_session,
                         due_at, defer_until, external_ref, source_system, source_repo,
                         deleted_at, deleted_by, delete_reason, original_type,
                         compaction_level, compacted_at, compacted_at_commit, original_size,
                         sender, ephemeral, pinned, is_template
                  FROM issues WHERE id IN ({})",
                placeholders.join(",")
            );

            let params: Vec<&dyn rusqlite::ToSql> =
                chunk.iter().map(|s| s as &dyn rusqlite::ToSql).collect();

            let mut stmt = self.conn.prepare(&sql)?;
            let chunk_issues = stmt
                .query_map(params.as_slice(), |row| self.issue_from_row(row))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            issues.extend(chunk_issues);
        }

        Ok(issues)
    }

    /// List issues with optional filters.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    #[allow(clippy::too_many_lines)]
    pub fn list_issues(&self, filters: &ListFilters) -> Result<Vec<Issue>> {
        let mut sql = String::from(
            r"SELECT id, content_hash, title, description, design, acceptance_criteria, notes,
                     status, priority, issue_type, assignee, owner, estimated_minutes,
                     created_at, created_by, updated_at, closed_at, close_reason, closed_by_session,
                     due_at, defer_until, external_ref, source_system, source_repo,
                     deleted_at, deleted_by, delete_reason, original_type,
                     compaction_level, compacted_at, compacted_at_commit, original_size,
                     sender, ephemeral, pinned, is_template
            FROM issues WHERE 1=1",
        );

        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(ref statuses) = filters.statuses {
            if !statuses.is_empty() {
                let placeholders: Vec<String> = statuses.iter().map(|_| "?".to_string()).collect();
                let _ = write!(sql, " AND status IN ({}) ", placeholders.join(","));
                for s in statuses {
                    params.push(Box::new(s.as_str().to_string()));
                }
            }
        }

        if let Some(ref types) = filters.types {
            if !types.is_empty() {
                let placeholders: Vec<String> = types.iter().map(|_| "?".to_string()).collect();
                let _ = write!(sql, " AND issue_type IN ({}) ", placeholders.join(","));
                for t in types {
                    params.push(Box::new(t.as_str().to_string()));
                }
            }
        }

        if let Some(ref priorities) = filters.priorities {
            if !priorities.is_empty() {
                let placeholders: Vec<String> =
                    priorities.iter().map(|_| "?".to_string()).collect();
                let _ = write!(sql, " AND priority IN ({}) ", placeholders.join(","));
                for p in priorities {
                    params.push(Box::new(p.0));
                }
            }
        }

        if let Some(ref assignee) = filters.assignee {
            sql.push_str(" AND assignee = ?");
            params.push(Box::new(assignee.clone()));
        }

        if filters.unassigned {
            sql.push_str(" AND assignee IS NULL");
        }

        if !filters.include_closed {
            if filters.include_deferred {
                sql.push_str(" AND status NOT IN ('closed', 'tombstone')");
            } else {
                sql.push_str(" AND status NOT IN ('closed', 'tombstone', 'deferred')");
            }
        }

        if !filters.include_templates {
            sql.push_str(" AND (is_template = 0 OR is_template IS NULL)");
        }

        if let Some(ref labels) = filters.labels {
            for label in labels {
                sql.push_str(" AND EXISTS (SELECT 1 FROM labels WHERE labels.issue_id = issues.id AND labels.label = ?)");
                params.push(Box::new(label.clone()));
            }
        }

        if let Some(ref labels_or) = filters.labels_or {
            if !labels_or.is_empty() {
                let placeholders: Vec<String> = labels_or.iter().map(|_| "?".to_string()).collect();
                let _ = write!(
                    sql,
                    " AND id IN (SELECT issue_id FROM labels WHERE label IN ({}))",
                    placeholders.join(",")
                );
                for l in labels_or {
                    params.push(Box::new(l.clone()));
                }
            }
        }

        if let Some(ref title_contains) = filters.title_contains {
            sql.push_str(" AND title LIKE ?");
            params.push(Box::new(format!("%{title_contains}%")));
        }

        if let Some(ts) = filters.updated_before {
            sql.push_str(" AND updated_at <= ?");
            params.push(Box::new(ts.to_rfc3339()));
        }

        if let Some(ts) = filters.updated_after {
            sql.push_str(" AND updated_at >= ?");
            params.push(Box::new(ts.to_rfc3339()));
        }

        // Apply custom sort if provided
        if let Some(ref sort_field) = filters.sort {
            let order = if filters.reverse { "DESC" } else { "ASC" };
            // Simple validation to prevent injection (though params should handle it,
            // column names can't be parameterized)
            match sort_field.as_str() {
                "priority" => {
                    let secondary_order = if filters.reverse { "ASC" } else { "DESC" };
                    let _ = write!(
                        sql,
                        " ORDER BY priority {order}, created_at {secondary_order}"
                    );
                }
                "created_at" | "created" => {
                    let order = if filters.reverse { "ASC" } else { "DESC" };
                    let _ = write!(sql, " ORDER BY created_at {order}");
                }
                "updated_at" | "updated" => {
                    let order = if filters.reverse { "ASC" } else { "DESC" };
                    let _ = write!(sql, " ORDER BY updated_at {order}");
                }
                "title" => {
                    // Case-insensitive sort for title
                    let _ = write!(sql, " ORDER BY title COLLATE NOCASE {order}");
                }
                _ => {
                    // Default fallback
                    sql.push_str(" ORDER BY priority ASC, created_at DESC");
                }
            }
        } else if filters.reverse {
            sql.push_str(" ORDER BY priority DESC, created_at ASC");
        } else {
            sql.push_str(" ORDER BY priority ASC, created_at DESC");
        }

        if let Some(limit) = filters.limit {
            if limit > 0 {
                sql.push_str(" LIMIT ?");
                params.push(Box::new(limit));
            }
        }

        let mut stmt = self.conn.prepare(&sql)?;
        let params_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(AsRef::as_ref).collect();
        let issues = stmt
            .query_map(params_refs.as_slice(), |row| self.issue_from_row(row))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(issues)
    }

    /// Search issues by query with optional filters.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn search_issues(&self, query: &str, filters: &ListFilters) -> Result<Vec<Issue>> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }

        let mut sql = String::from(
            r"SELECT id, content_hash, title, description, design, acceptance_criteria, notes,
                     status, priority, issue_type, assignee, owner, estimated_minutes,
                     created_at, created_by, updated_at, closed_at, close_reason, closed_by_session,
                     due_at, defer_until, external_ref, source_system, source_repo,
                     deleted_at, deleted_by, delete_reason, original_type,
                     compaction_level, compacted_at, compacted_at_commit, original_size,
                     sender, ephemeral, pinned, is_template
              FROM issues WHERE 1=1",
        );

        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        sql.push_str(" AND (title LIKE ? OR description LIKE ? OR id LIKE ?)");
        let pattern = format!("%{trimmed}%");
        params.push(Box::new(pattern.clone()));
        params.push(Box::new(pattern.clone()));
        params.push(Box::new(pattern));

        if let Some(ref statuses) = filters.statuses {
            if !statuses.is_empty() {
                let placeholders: Vec<String> = statuses.iter().map(|_| "?".to_string()).collect();
                let _ = write!(sql, " AND status IN ({})", placeholders.join(","));
                for s in statuses {
                    params.push(Box::new(s.as_str().to_string()));
                }
            }
        }

        if let Some(ref types) = filters.types {
            if !types.is_empty() {
                let placeholders: Vec<String> = types.iter().map(|_| "?".to_string()).collect();
                let _ = write!(sql, " AND issue_type IN ({})", placeholders.join(","));
                for t in types {
                    params.push(Box::new(t.as_str().to_string()));
                }
            }
        }

        if let Some(ref priorities) = filters.priorities {
            if !priorities.is_empty() {
                let placeholders: Vec<String> =
                    priorities.iter().map(|_| "?".to_string()).collect();
                let _ = write!(sql, " AND priority IN ({})", placeholders.join(","));
                for p in priorities {
                    params.push(Box::new(p.0));
                }
            }
        }

        if let Some(ref assignee) = filters.assignee {
            sql.push_str(" AND assignee = ?");
            params.push(Box::new(assignee.clone()));
        }

        if filters.unassigned {
            sql.push_str(" AND assignee IS NULL");
        }

        if !filters.include_closed {
            sql.push_str(" AND status NOT IN ('closed', 'tombstone')");
        }

        if !filters.include_templates {
            sql.push_str(" AND (is_template = 0 OR is_template IS NULL)");
        }

        if let Some(ref labels) = filters.labels {
            for label in labels {
                sql.push_str(" AND EXISTS (SELECT 1 FROM labels WHERE labels.issue_id = issues.id AND labels.label = ?)");
                params.push(Box::new(label.clone()));
            }
        }

        if let Some(ref labels_or) = filters.labels_or {
            if !labels_or.is_empty() {
                let placeholders: Vec<String> = labels_or.iter().map(|_| "?".to_string()).collect();
                let _ = write!(
                    sql,
                    " AND id IN (SELECT issue_id FROM labels WHERE label IN ({}))",
                    placeholders.join(",")
                );
                for l in labels_or {
                    params.push(Box::new(l.clone()));
                }
            }
        }

        if let Some(ref title_contains) = filters.title_contains {
            sql.push_str(" AND title LIKE ?");
            params.push(Box::new(format!("%{title_contains}%")));
        }

        sql.push_str(" ORDER BY priority ASC, created_at DESC");

        if let Some(limit) = filters.limit {
            if limit > 0 {
                sql.push_str(" LIMIT ?");
                params.push(Box::new(limit));
            }
        }

        let mut stmt = self.conn.prepare(&sql)?;
        let params_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(AsRef::as_ref).collect();
        let issues = stmt
            .query_map(params_refs.as_slice(), |row| self.issue_from_row(row))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(issues)
    }

    /// Get ready issues (unblocked, not deferred, not pinned, not ephemeral).
    ///
    /// Ready definition:
    /// 1. Status is `open` OR `in_progress`
    /// 2. NOT in `blocked_issues_cache`
    /// 3. `defer_until` is NULL or <= now (unless `include_deferred`)
    /// 4. `pinned = 0` (not pinned)
    /// 5. `ephemeral = 0` AND ID does not contain `-wisp-`
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    #[allow(clippy::too_many_lines)]
    pub fn get_ready_issues(
        &self,
        filters: &ReadyFilters,
        sort: ReadySortPolicy,
    ) -> Result<Vec<Issue>> {
        let mut sql = String::from(
            r"SELECT id, content_hash, title, description, design, acceptance_criteria, notes,
                     status, priority, issue_type, assignee, owner, estimated_minutes,
                     created_at, created_by, updated_at, closed_at, close_reason, closed_by_session,
                     due_at, defer_until, external_ref, source_system, source_repo,
                     deleted_at, deleted_by, delete_reason, original_type,
                     compaction_level, compacted_at, compacted_at_commit, original_size,
                     sender, ephemeral, pinned, is_template
              FROM issues WHERE 1=1",
        );

        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        // Ready condition 1: status is `open` OR `in_progress`
        if filters.include_deferred {
            sql.push_str(" AND status IN ('open', 'in_progress', 'deferred')");
        } else {
            sql.push_str(" AND status IN ('open', 'in_progress')");
        }

        // Ready condition 2: NOT in blocked_issues_cache (optimized: filter in SQL)
        sql.push_str(" AND id NOT IN (SELECT issue_id FROM blocked_issues_cache)");

        // Ready condition 3: `defer_until` is NULL or <= now (unless `include_deferred`)
        if !filters.include_deferred {
            // Compare using SQLite datetime parsing to handle RFC3339 timestamps consistently.
            sql.push_str(" AND (defer_until IS NULL OR datetime(defer_until) <= datetime('now'))");
        }

        // Ready condition 4: not pinned
        sql.push_str(" AND (pinned = 0 OR pinned IS NULL)");

        // Ready condition 5: not ephemeral and not wisp
        sql.push_str(" AND (ephemeral = 0 OR ephemeral IS NULL)");
        sql.push_str(" AND id NOT LIKE '%-wisp-%'");

        // Exclude templates
        sql.push_str(" AND (is_template = 0 OR is_template IS NULL)");

        // Filter by types
        if let Some(ref types) = filters.types {
            if !types.is_empty() {
                let placeholders: Vec<String> = types.iter().map(|_| "?".to_string()).collect();
                let _ = write!(sql, " AND issue_type IN ({}) ", placeholders.join(","));
                for t in types {
                    params.push(Box::new(t.as_str().to_string()));
                }
            }
        }

        // Filter by priorities
        if let Some(ref priorities) = filters.priorities {
            if !priorities.is_empty() {
                let placeholders: Vec<String> =
                    priorities.iter().map(|_| "?".to_string()).collect();
                let _ = write!(sql, " AND priority IN ({})", placeholders.join(","));
                for p in priorities {
                    params.push(Box::new(p.0));
                }
            }
        }

        // Filter by assignee
        if let Some(ref assignee) = filters.assignee {
            sql.push_str(" AND assignee = ?");
            params.push(Box::new(assignee.clone()));
        }

        // Filter for unassigned
        if filters.unassigned {
            sql.push_str(" AND assignee IS NULL");
        }

        // Filter by labels (AND logic)
        for label in &filters.labels_and {
            sql.push_str(" AND EXISTS (SELECT 1 FROM labels WHERE labels.issue_id = issues.id AND labels.label = ?)");
            params.push(Box::new(label.clone()));
        }

        // Filter by labels (OR logic)
        if !filters.labels_or.is_empty() {
            let placeholders: Vec<String> =
                filters.labels_or.iter().map(|_| "?".to_string()).collect();
            let _ = write!(
                sql,
                " AND id IN (SELECT issue_id FROM labels WHERE label IN ({}))",
                placeholders.join(",")
            );
            for l in &filters.labels_or {
                params.push(Box::new(l.clone()));
            }
        }

        // Sorting
        match sort {
            ReadySortPolicy::Hybrid => {
                // P0/P1 first by `created_at` ASC, then others by `created_at` ASC
                sql.push_str(" ORDER BY CASE WHEN priority <= 1 THEN 0 ELSE 1 END, created_at ASC");
            }
            ReadySortPolicy::Priority => {
                sql.push_str(" ORDER BY priority ASC, created_at ASC");
            }
            ReadySortPolicy::Oldest => {
                sql.push_str(" ORDER BY created_at ASC");
            }
        }

        let mut stmt = self.conn.prepare(&sql)?;
        let params_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(AsRef::as_ref).collect();
        let mut issues: Vec<Issue> = stmt
            .query_map(params_refs.as_slice(), |row| self.issue_from_row(row))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        // Apply limit after all filtering
        if let Some(limit) = filters.limit {
            if limit > 0 && issues.len() > limit {
                issues.truncate(limit);
            }
        }

        Ok(issues)
    }

    /// Get IDs of blocked issues from cache.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_blocked_ids(&self) -> Result<HashSet<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT issue_id FROM blocked_issues_cache")?;
        let ids = stmt
            .query_map([], |row| row.get(0))?
            .collect::<std::result::Result<HashSet<String>, _>>()?;
        Ok(ids)
    }

    /// Get issue IDs blocked by `blocks` dependency type only (not full cache).
    ///
    /// This is used for stats computation where blocked count should be based
    /// only on `blocks` deps per classic bd semantics.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_blocked_by_blocks_deps_only(&self) -> Result<HashSet<String>> {
        // Returns issues that:
        // 1. Have a 'blocks' type dependency
        // 2. Where the blocker is not closed/tombstone
        // 3. AND the blocked issue itself is not closed/tombstone
        let mut stmt = self.conn.prepare(
            r"SELECT DISTINCT d.issue_id
              FROM dependencies d
              LEFT JOIN issues blocker ON d.depends_on_id = blocker.id
              LEFT JOIN issues blocked ON d.issue_id = blocked.id
              WHERE d.type = 'blocks'
                AND blocker.status NOT IN ('closed', 'tombstone')
                AND blocked.status NOT IN ('closed', 'tombstone')",
        )?;
        let ids = stmt
            .query_map([], |row| row.get(0))?
            .collect::<std::result::Result<HashSet<String>, _>>()?;
        Ok(ids)
    }

    /// Check if an issue is blocked (in the blocked cache).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn is_blocked(&self, issue_id: &str) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT count(*) FROM blocked_issues_cache WHERE issue_id = ?",
            [issue_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Rebuild the blocked issues cache from scratch.
    ///
    /// This computes which issues are blocked based on their dependencies
    /// and the status of their blockers. An issue is blocked if it has a
    /// blocking-type dependency on an issue that is not closed/tombstone.
    ///
    /// Blocking dependency types: blocks, parent-child, conditional-blocks, waits-for
    /// Blocking statuses: any non-terminal status (not closed/tombstone)
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    #[allow(clippy::too_many_lines)]
    pub fn rebuild_blocked_cache(&mut self, force_rebuild: bool) -> Result<usize> {
        if !force_rebuild {
            return Ok(0);
        }
        let tx = self.conn.transaction()?;
        let count = Self::rebuild_blocked_cache_impl(&tx)?;
        tx.commit()?;
        Ok(count)
    }

    fn rebuild_blocked_cache_impl(conn: &Connection) -> Result<usize> {
        const MAX_DEPTH: i32 = 50;

        // Clear existing cache
        conn.execute("DELETE FROM blocked_issues_cache", [])?;

        // Find all issues that are blocked by a dependency
        // An issue is blocked if it has a blocking-type dependency on an issue that is not closed/tombstone
        //
        // Note: parent-child is NOT included here. A child is not blocked just because
        // its parent epic is open. However, if the parent is blocked by something else,
        // that blocking propagates to children (handled in the transitive section below).
        //
        // For conditional-blocks, we also need to check if the blocker closed with failure
        // but for simplicity in this initial implementation, we treat it like blocks.
        let mut blocked_issues_map: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        {
            let mut stmt = conn.prepare(
                r"SELECT DISTINCT d.issue_id, d.depends_on_id || ':' || COALESCE(i.status, 'unknown')
                  FROM dependencies d
                  LEFT JOIN issues i ON d.depends_on_id = i.id
                  WHERE d.type IN ('blocks', 'conditional-blocks', 'waits-for')
                    AND (
                      -- The blocker is in a blocking state (anything not terminal)
                      i.status NOT IN ('closed', 'tombstone')
                      -- Or it's a missing local dependency (orphan)
                      -- External dependencies are resolved at runtime in the CLI
                      OR (i.id IS NULL AND d.depends_on_id NOT LIKE 'external:%')
                    )",
            )?;

            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;

            for row in rows {
                let (issue_id, blocker_ref) = row?;
                blocked_issues_map
                    .entry(issue_id)
                    .or_default()
                    .push(blocker_ref);
            }
        }

        // Insert blocked issues into cache
        let mut count = 0;
        {
            let mut insert_stmt = conn.prepare(
                "INSERT INTO blocked_issues_cache (issue_id, blocked_by) VALUES (?, ?)",
            )?;

            for (issue_id, blockers) in blocked_issues_map {
                if blockers.is_empty() {
                    continue;
                }
                // Convert blockers list to JSON array using serde_json for safety
                let blockers_json =
                    serde_json::to_string(&blockers).unwrap_or_else(|_| "[]".to_string());
                insert_stmt.execute(rusqlite::params![issue_id, blockers_json])?;
                count += 1;
            }
        }

        // Now handle transitive blocking via parent-child relationships
        // Children inherit parent's blocked state (up to depth 50)
        let mut depth = 0;
        loop {
            if depth >= MAX_DEPTH {
                tracing::warn!(
                    "Transitive blocked cache rebuild hit max depth {}",
                    MAX_DEPTH
                );
                break;
            }

            // Find children of blocked issues that aren't already in cache
            let newly_blocked: Vec<(String, String)> = {
                let mut stmt = conn.prepare(
                    r"SELECT DISTINCT d.issue_id, d.depends_on_id
                      FROM dependencies d
                      INNER JOIN blocked_issues_cache bc ON d.depends_on_id = bc.issue_id
                      WHERE d.type = 'parent-child'
                        AND d.issue_id NOT IN (SELECT issue_id FROM blocked_issues_cache)",
                )?;

                stmt.query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?
            };

            if newly_blocked.is_empty() {
                break;
            }

            // Group by issue_id to avoid PK violation
            let mut issue_blockers: std::collections::HashMap<String, Vec<String>> =
                std::collections::HashMap::new();
            for (issue_id, parent_id) in newly_blocked {
                issue_blockers.entry(issue_id).or_default().push(parent_id);
            }

            {
                let mut insert_stmt = conn.prepare(
                    "INSERT INTO blocked_issues_cache (issue_id, blocked_by) VALUES (?, ?)",
                )?;

                for (issue_id, parents) in issue_blockers {
                    let blockers: Vec<String> = parents
                        .into_iter()
                        .map(|p| format!("{p}:parent-blocked"))
                        .collect();
                    let blockers_json =
                        serde_json::to_string(&blockers).unwrap_or_else(|_| "[]".to_string());

                    insert_stmt.execute(rusqlite::params![issue_id, blockers_json])?;
                    count += 1;
                }
            }

            depth += 1;
        }

        tracing::debug!(blocked_count = count, "Rebuilt blocked issues cache");
        Ok(count)
    }

    /// Get issues that are blocked, along with what's blocking them.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_blocked_issues(&self) -> Result<Vec<(Issue, Vec<String>)>> {
        let mut stmt = self.conn.prepare(
            r"SELECT i.id, i.content_hash, i.title, i.description, i.design, i.acceptance_criteria, i.notes,
                     i.status, i.priority, i.issue_type, i.assignee, i.owner, i.estimated_minutes,
                     i.created_at, i.created_by, i.updated_at, i.closed_at, i.close_reason, i.closed_by_session,
                     i.due_at, i.defer_until, i.external_ref, i.source_system, i.source_repo,
                     i.deleted_at, i.deleted_by, i.delete_reason, i.original_type, i.compaction_level,
                     i.compacted_at, i.compacted_at_commit, i.original_size, i.sender, i.ephemeral,
                     i.pinned, i.is_template,
                     bc.blocked_by
              FROM issues i
              INNER JOIN blocked_issues_cache bc ON i.id = bc.issue_id
              WHERE i.status IN ('open', 'in_progress')
              ORDER BY i.priority ASC, i.created_at ASC",
        )?;

        let results = stmt
            .query_map([], |row| {
                let issue = self.issue_from_row(row)?;
                let blockers_json: String = row.get(36)?;
                Ok((issue, blockers_json))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        // Parse blockers JSON
        let mut blocked_issues = Vec::new();
        for (issue, blockers_json) in results {
            let blockers: Vec<String> = serde_json::from_str(&blockers_json).unwrap_or_default();
            blocked_issues.push((issue, blockers));
        }

        Ok(blocked_issues)
    }

    /// Resolve external dependency satisfaction for dependencies of this project.
    ///
    /// Returns a map of external dependency IDs to whether they are satisfied.
    /// Missing projects or query failures are treated as unsatisfied.
    ///
    /// # Errors
    ///
    /// Returns an error if querying local dependencies fails.
    pub fn resolve_external_dependency_statuses(
        &self,
        external_db_paths: &HashMap<String, PathBuf>,
        blocking_only: bool,
    ) -> Result<HashMap<String, bool>> {
        let external_ids = self.list_external_dependency_ids(blocking_only)?;
        if external_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let mut project_caps: HashMap<String, HashSet<String>> = HashMap::new();
        let mut parsed: HashMap<String, (String, String)> = HashMap::new();
        for dep_id in &external_ids {
            if let Some((project, capability)) = parse_external_dependency(dep_id) {
                project_caps
                    .entry(project.clone())
                    .or_default()
                    .insert(capability.clone());
                parsed.insert(dep_id.clone(), (project, capability));
            }
        }

        // Query each external project's database to find satisfied capabilities
        let mut satisfied: HashMap<String, HashSet<String>> = HashMap::new();
        for (project, caps) in &project_caps {
            let Some(db_path) = external_db_paths.get(project) else {
                tracing::warn!(
                    project = %project,
                    "External project not configured; treating dependencies as unsatisfied"
                );
                continue;
            };

            match query_external_project_capabilities(db_path, caps) {
                Ok(found) => {
                    satisfied.insert(project.clone(), found);
                }
                Err(err) => {
                    tracing::warn!(
                        project = %project,
                        path = %db_path.display(),
                        error = %err,
                        "Failed to query external project; treating dependencies as unsatisfied"
                    );
                }
            }
        }

        let mut statuses = HashMap::new();
        for dep_id in external_ids {
            let is_satisfied = parsed.get(&dep_id).is_some_and(|(project, capability)| {
                satisfied
                    .get(project)
                    .is_some_and(|caps| caps.contains(capability))
            });
            statuses.insert(dep_id, is_satisfied);
        }

        Ok(statuses)
    }

    /// Compute blockers caused by unsatisfied external dependencies.
    ///
    /// This excludes external dependencies from the blocked cache and evaluates
    /// them at query time, including parent-child propagation.
    ///
    /// # Errors
    ///
    /// Returns an error if dependency queries fail.
    pub fn external_blockers(
        &self,
        external_statuses: &HashMap<String, bool>,
    ) -> Result<HashMap<String, Vec<String>>> {
        let mut blockers: HashMap<String, Vec<String>> = HashMap::new();

        // Direct external blockers (blocking dependency types only).
        let mut stmt = self.conn.prepare(
            "SELECT issue_id, depends_on_id
             FROM dependencies
             WHERE depends_on_id LIKE 'external:%'
               AND type IN ('blocks', 'parent-child', 'conditional-blocks', 'waits-for')",
        )?;

        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;

        for row in rows {
            let (issue_id, depends_on_id) = row?;
            let satisfied = external_statuses
                .get(&depends_on_id)
                .copied()
                .unwrap_or(false);
            if !satisfied {
                blockers
                    .entry(issue_id)
                    .or_default()
                    .push(format!("{depends_on_id}:blocked"));
            }
        }

        // Propagate external blocking through parent-child relationships.
        let mut parent_stmt = self.conn.prepare(
            "SELECT issue_id, depends_on_id FROM dependencies WHERE type = 'parent-child'",
        )?;
        let edges = parent_stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        if !edges.is_empty() && !blockers.is_empty() {
            let mut children_by_parent: HashMap<String, Vec<String>> = HashMap::new();
            for (child, parent) in &edges {
                children_by_parent
                    .entry(parent.clone())
                    .or_default()
                    .push(child.clone());
            }

            let mut queue: Vec<String> = blockers.keys().cloned().collect();
            let mut seen: HashSet<String> = HashSet::new();

            while let Some(parent_id) = queue.pop() {
                if !seen.insert(parent_id.clone()) {
                    continue;
                }
                if let Some(children) = children_by_parent.get(&parent_id) {
                    for child in children {
                        let entry = blockers.entry(child.clone()).or_default();
                        let marker = format!("{parent_id}:parent-blocked");
                        if entry.contains(&marker) {
                            continue;
                        }
                        entry.push(marker);
                        queue.push(child.clone());
                    }
                }
            }
        }

        Ok(blockers
            .into_iter()
            .map(|(k, v)| (k, v.into_iter().collect()))
            .collect())
    }

    fn list_external_dependency_ids(&self, blocking_only: bool) -> Result<HashSet<String>> {
        let mut ids = HashSet::new();
        let sql = if blocking_only {
            "SELECT DISTINCT depends_on_id
             FROM dependencies
             WHERE depends_on_id LIKE 'external:%'
               AND type IN ('blocks', 'parent-child', 'conditional-blocks', 'waits-for')"
        } else {
            "SELECT DISTINCT depends_on_id
             FROM dependencies
             WHERE depends_on_id LIKE 'external:%'"
        };

        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        for row in rows {
            ids.insert(row?);
        }

        Ok(ids)
    }

    /// Check if an issue ID already exists.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn id_exists(&self, id: &str) -> Result<bool> {
        let count: i64 =
            self.conn
                .query_row("SELECT count(*) FROM issues WHERE id = ?", [id], |row| {
                    row.get(0)
                })?;
        Ok(count > 0)
    }

    /// Find issue IDs that end with the given hash substring.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn find_ids_by_hash(&self, hash_suffix: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT id FROM issues WHERE id LIKE ?")?;
        let pattern = format!("%-%{hash_suffix}%");
        let ids = stmt
            .query_map([pattern], |row| row.get(0))?
            .collect::<std::result::Result<Vec<String>, _>>()?;
        Ok(ids)
    }

    /// Count total issues in the database.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn count_issues(&self) -> Result<usize> {
        let count: i64 = self
            .conn
            .query_row("SELECT count(*) FROM issues", [], |row| row.get(0))?;
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// Get all issue IDs in the database.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_all_ids(&self) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare("SELECT id FROM issues ORDER BY id")?;
        let ids = stmt
            .query_map([], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// Get epic counts (total children, closed children) for all epics.
    ///
    /// Returns a map from epic ID to (`total_children`, `closed_children`).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_epic_counts(&self) -> Result<std::collections::HashMap<String, (usize, usize)>> {
        let mut stmt = self.conn.prepare(
            "SELECT
                d.depends_on_id AS epic_id,
                COUNT(*) AS total,
                SUM(CASE WHEN i.status = 'closed' OR i.status = 'tombstone' THEN 1 ELSE 0 END) AS closed
             FROM dependencies d
             JOIN issues i ON d.issue_id = i.id
             WHERE d.type = 'parent-child'
             GROUP BY d.depends_on_id",
        )?;
        let mut counts = std::collections::HashMap::new();
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)? as usize,
                row.get::<_, i64>(2)? as usize,
            ))
        })?;
        for row in rows {
            let (epic_id, total, closed) = row?;
            counts.insert(epic_id, (total, closed));
        }
        Ok(counts)
    }

    /// Add a dependency between issues.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn add_dependency(
        &mut self,
        issue_id: &str,
        depends_on_id: &str,
        dep_type: &str,
        actor: &str,
    ) -> Result<bool> {
        // Check for cycles if this is a blocking dependency
        if let Ok(dt) = dep_type.parse::<DependencyType>() {
            if dt.is_blocking() && self.would_create_cycle(issue_id, depends_on_id, true)? {
                return Err(BeadsError::DependencyCycle {
                    path: format!(
                        "Adding dependency {issue_id} -> {depends_on_id} would create a cycle"
                    ),
                });
            }
        }

        self.mutate("add_dependency", actor, |tx, ctx| {
            let exists: i64 = tx.query_row(
                "SELECT count(*) FROM dependencies WHERE issue_id = ? AND depends_on_id = ?",
                rusqlite::params![issue_id, depends_on_id],
                |row| row.get(0),
            )?;

            if exists > 0 {
                return Ok(false);
            }

            tx.execute(
                "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by)
                 VALUES (?, ?, ?, ?, ?)",
                rusqlite::params![
                    issue_id,
                    depends_on_id,
                    dep_type,
                    Utc::now().to_rfc3339(),
                    actor
                ],
            )?;

            // Bump updated_at
            tx.execute(
                "UPDATE issues SET updated_at = ? WHERE id = ?",
                rusqlite::params![Utc::now().to_rfc3339(), issue_id],
            )?;

            ctx.record_event(
                EventType::DependencyAdded,
                issue_id,
                Some(format!("Added dependency on {depends_on_id} ({dep_type})")),
            );
            ctx.mark_dirty(issue_id);
            ctx.invalidate_cache();

            Ok(true)
        })
    }

    /// Remove a dependency link.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn remove_dependency(
        &mut self,
        issue_id: &str,
        depends_on_id: &str,
        actor: &str,
    ) -> Result<bool> {
        self.mutate("remove_dependency", actor, |tx, ctx| {
            let rows = tx.execute(
                "DELETE FROM dependencies WHERE issue_id = ? AND depends_on_id = ?",
                rusqlite::params![issue_id, depends_on_id],
            )?;

            if rows > 0 {
                // Bump updated_at
                tx.execute(
                    "UPDATE issues SET updated_at = ? WHERE id = ?",
                    rusqlite::params![Utc::now().to_rfc3339(), issue_id],
                )?;

                ctx.record_event(
                    EventType::DependencyRemoved,
                    issue_id,
                    Some(format!("Removed dependency on {depends_on_id}")),
                );
                ctx.mark_dirty(issue_id);
                ctx.invalidate_cache();
            }

            Ok(rows > 0)
        })
    }

    /// Remove all dependencies for an issue.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn remove_all_dependencies(&mut self, issue_id: &str, actor: &str) -> Result<usize> {
        self.mutate("remove_all_dependencies", actor, |tx, ctx| {
            let mut stmt = tx.prepare(
                "SELECT DISTINCT issue_id FROM dependencies WHERE depends_on_id = ?
                 UNION
                 SELECT DISTINCT depends_on_id FROM dependencies WHERE issue_id = ?",
            )?;
            let affected: Vec<String> = stmt
                .query_map(rusqlite::params![issue_id, issue_id], |row| row.get(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;

            let outgoing = tx.execute("DELETE FROM dependencies WHERE issue_id = ?", [issue_id])?;
            let incoming = tx.execute(
                "DELETE FROM dependencies WHERE depends_on_id = ?",
                [issue_id],
            )?;
            let total = outgoing + incoming;

            if total > 0 {
                let now = Utc::now().to_rfc3339();

                // Bump updated_at for issue_id
                tx.execute(
                    "UPDATE issues SET updated_at = ? WHERE id = ?",
                    rusqlite::params![now, issue_id],
                )?;

                // Bump updated_at for affected issues
                for affected_id in &affected {
                    tx.execute(
                        "UPDATE issues SET updated_at = ? WHERE id = ?",
                        rusqlite::params![now, affected_id],
                    )?;
                }

                ctx.record_event(
                    EventType::DependencyRemoved,
                    issue_id,
                    Some(format!("Removed {total} dependency links")),
                );
                ctx.mark_dirty(issue_id);
                for affected_id in affected {
                    ctx.mark_dirty(&affected_id);
                }
                ctx.invalidate_cache();
            }

            Ok(total)
        })
    }

    /// Remove parent-child dependency for an issue.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn remove_parent(&mut self, issue_id: &str, actor: &str) -> Result<bool> {
        self.mutate("remove_parent", actor, |tx, ctx| {
            let rows = tx.execute(
                "DELETE FROM dependencies WHERE issue_id = ? AND type = 'parent-child'",
                rusqlite::params![issue_id],
            )?;

            if rows > 0 {
                // Bump updated_at
                tx.execute(
                    "UPDATE issues SET updated_at = ? WHERE id = ?",
                    rusqlite::params![Utc::now().to_rfc3339(), issue_id],
                )?;

                ctx.record_event(
                    EventType::DependencyRemoved,
                    issue_id,
                    Some("Removed parent".to_string()),
                );
                ctx.mark_dirty(issue_id);
                ctx.invalidate_cache();
            }

            Ok(rows > 0)
        })
    }

    /// Add a label to an issue.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn add_label(&mut self, issue_id: &str, label: &str, actor: &str) -> Result<bool> {
        self.mutate("add_label", actor, |tx, ctx| {
            let exists: i64 = tx.query_row(
                "SELECT count(*) FROM labels WHERE issue_id = ? AND label = ?",
                rusqlite::params![issue_id, label],
                |row| row.get(0),
            )?;

            if exists > 0 {
                return Ok(false);
            }

            tx.execute(
                "INSERT INTO labels (issue_id, label) VALUES (?, ?)",
                rusqlite::params![issue_id, label],
            )?;

            ctx.record_event(
                EventType::LabelAdded,
                issue_id,
                Some(format!("Added label {label}")),
            );
            ctx.mark_dirty(issue_id);

            // Update timestamp
            tx.execute(
                "UPDATE issues SET updated_at = ? WHERE id = ?",
                rusqlite::params![Utc::now().to_rfc3339(), issue_id],
            )?;

            Ok(true)
        })
    }

    /// Remove a label from an issue.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn remove_label(&mut self, issue_id: &str, label: &str, actor: &str) -> Result<bool> {
        self.mutate("remove_label", actor, |tx, ctx| {
            let rows = tx.execute(
                "DELETE FROM labels WHERE issue_id = ? AND label = ?",
                rusqlite::params![issue_id, label],
            )?;

            if rows > 0 {
                // Bump updated_at
                tx.execute(
                    "UPDATE issues SET updated_at = ? WHERE id = ?",
                    rusqlite::params![Utc::now().to_rfc3339(), issue_id],
                )?;

                ctx.record_event(
                    EventType::LabelRemoved,
                    issue_id,
                    Some(format!("Removed label {label}")),
                );
                ctx.mark_dirty(issue_id);
            }

            Ok(rows > 0)
        })
    }

    /// Remove all labels from an issue.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn remove_all_labels(&mut self, issue_id: &str, actor: &str) -> Result<usize> {
        self.mutate("remove_all_labels", actor, |tx, ctx| {
            let rows = tx.execute(
                "DELETE FROM labels WHERE issue_id = ?",
                rusqlite::params![issue_id],
            )?;

            if rows > 0 {
                // Bump updated_at
                tx.execute(
                    "UPDATE issues SET updated_at = ? WHERE id = ?",
                    rusqlite::params![Utc::now().to_rfc3339(), issue_id],
                )?;

                ctx.record_event(
                    EventType::LabelRemoved,
                    issue_id,
                    Some(format!("Removed {rows} labels")),
                );
                ctx.mark_dirty(issue_id);
            }

            Ok(rows)
        })
    }

    /// Set all labels for an issue (replace existing).
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn set_labels(&mut self, issue_id: &str, labels: &[String], actor: &str) -> Result<()> {
        self.mutate("set_labels", actor, |tx, ctx| {
            let mut stmt = tx.prepare("SELECT label FROM labels WHERE issue_id = ?")?;
            let old_labels: Vec<String> = stmt
                .query_map([issue_id], |row| row.get(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            drop(stmt);

            tx.execute("DELETE FROM labels WHERE issue_id = ?", [issue_id])?;

            for label in labels {
                tx.execute(
                    "INSERT INTO labels (issue_id, label) VALUES (?, ?)",
                    rusqlite::params![issue_id, label],
                )?;
            }

            // Record changes
            let removed: Vec<_> = old_labels.iter().filter(|l| !labels.contains(l)).collect();
            let added: Vec<_> = labels.iter().filter(|l| !old_labels.contains(l)).collect();

            if !removed.is_empty() || !added.is_empty() {
                let mut details = Vec::new();
                if !removed.is_empty() {
                    details.push(format!(
                        "removed: {}",
                        removed
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                if !added.is_empty() {
                    details.push(format!(
                        "added: {}",
                        added
                            .iter()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                ctx.record_event(
                    EventType::Updated,
                    issue_id,
                    Some(format!("Labels {}", details.join("; "))),
                );
                ctx.mark_dirty(issue_id);

                // Bump updated_at
                tx.execute(
                    "UPDATE issues SET updated_at = ? WHERE id = ?",
                    rusqlite::params![Utc::now().to_rfc3339(), issue_id],
                )?;
            }

            Ok(())
        })
    }

    /// Get labels for an issue.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_labels(&self, issue_id: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT label FROM labels WHERE issue_id = ? ORDER BY label")?;
        let labels = stmt
            .query_map([issue_id], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(labels)
    }

    /// Get labels for multiple issues efficiently.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_labels_for_issues(
        &self,
        issue_ids: &[String],
    ) -> Result<HashMap<String, Vec<String>>> {
        const SQLITE_VAR_LIMIT: usize = 900;

        if issue_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let mut map: HashMap<String, Vec<String>> = HashMap::new();

        // SQLite has a finite variable limit (default 999). Chunk to avoid query failures.
        for chunk in issue_ids.chunks(SQLITE_VAR_LIMIT) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            let sql = format!(
                "SELECT issue_id, label FROM labels WHERE issue_id IN ({}) ORDER BY issue_id, label",
                placeholders.join(",")
            );

            let params: Vec<&dyn rusqlite::ToSql> =
                chunk.iter().map(|s| s as &dyn rusqlite::ToSql).collect();

            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(params.as_slice(), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;

            for row in rows {
                let (issue_id, label) = row?;
                map.entry(issue_id).or_default().push(label);
            }
        }

        Ok(map)
    }

    /// Get all labels for all issues as a map of issue_id -> labels.
    ///
    /// Used for export and sync operations that need complete label state.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_all_labels(&self) -> Result<HashMap<String, Vec<String>>> {
        let mut stmt = self
            .conn
            .prepare("SELECT issue_id, label FROM labels ORDER BY issue_id, label")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;

        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for row in rows {
            let (issue_id, label) = row?;
            map.entry(issue_id).or_default().push(label);
        }
        Ok(map)
    }

    /// Get all unique labels with their issue counts.
    ///
    /// Returns a vector of (label, count) pairs sorted alphabetically by label.
    /// Excludes labels on tombstoned (deleted) issues.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_unique_labels_with_counts(&self) -> Result<Vec<(String, i64)>> {
        let mut stmt = self.conn.prepare(
            r"SELECT l.label, COUNT(*) as count
              FROM labels l
              JOIN issues i ON l.issue_id = i.id
              WHERE i.status != 'tombstone'
              GROUP BY l.label
              ORDER BY l.label",
        )?;
        let results = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(results)
    }

    /// Rename a label across all issues.
    ///
    /// Returns the number of issues affected.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn rename_label(&mut self, old_name: &str, new_name: &str, actor: &str) -> Result<usize> {
        self.mutate("rename_label", actor, |tx, ctx| {
            // Get all issue IDs that have the old label
            let mut stmt = tx.prepare("SELECT issue_id FROM labels WHERE label = ?")?;
            let issue_ids: Vec<String> = stmt
                .query_map([old_name], |row| row.get(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            drop(stmt);

            // Check if any issues already have the new label (would cause duplicates)
            let mut check_stmt =
                tx.prepare("SELECT issue_id FROM labels WHERE label = ? AND issue_id IN (SELECT issue_id FROM labels WHERE label = ?)")?;
            let conflicts: Vec<String> = check_stmt
                .query_map(rusqlite::params![new_name, old_name], |row| row.get(0))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            drop(check_stmt);

            // For issues that already have the new label, just remove the old one
            for conflict_id in &conflicts {
                tx.execute(
                    "DELETE FROM labels WHERE issue_id = ? AND label = ?",
                    rusqlite::params![conflict_id, old_name],
                )?;
                ctx.mark_dirty(conflict_id);
            }

            // Rename the label for issues that don't have conflicts
            let renamed = tx.execute(
                "UPDATE labels SET label = ? WHERE label = ?",
                rusqlite::params![new_name, old_name],
            )?;

            // Mark all affected issues as dirty and record events
            let now = Utc::now().to_rfc3339();
            for issue_id in &issue_ids {
                ctx.record_event(
                    EventType::LabelRemoved,
                    issue_id,
                    Some(format!("Renamed label {old_name} to {new_name}")),
                );
                ctx.mark_dirty(issue_id);

                // Update timestamp
                tx.execute(
                    "UPDATE issues SET updated_at = ? WHERE id = ?",
                    rusqlite::params![now, issue_id],
                )?;
            }

            Ok(renamed + conflicts.len())
        })
    }

    /// Get comments for an issue.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_comments(&self, issue_id: &str) -> Result<Vec<Comment>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, issue_id, author, text, created_at
             FROM comments
             WHERE issue_id = ?
             ORDER BY created_at ASC",
        )?;

        let comments = stmt
            .query_map([issue_id], |row| {
                Ok(Comment {
                    id: row.get(0)?,
                    issue_id: row.get(1)?,
                    author: row.get(2)?,
                    body: row.get(3)?,
                    created_at: parse_datetime(&row.get::<_, String>(4)?),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(comments)
    }

    /// Add a comment to an issue.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn add_comment(&mut self, issue_id: &str, author: &str, text: &str) -> Result<Comment> {
        self.mutate("add_comment", author, |tx, ctx| {
            let comment_id = insert_comment_row(tx, issue_id, author, text)?;

            tx.execute(
                "UPDATE issues SET updated_at = ? WHERE id = ?",
                rusqlite::params![Utc::now().to_rfc3339(), issue_id],
            )?;

            ctx.record_event(EventType::Commented, issue_id, Some(text.to_string()));
            ctx.mark_dirty(issue_id);

            fetch_comment(tx, comment_id)
        })
    }

    /// Get dependencies with metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_dependencies_with_metadata(
        &self,
        issue_id: &str,
    ) -> Result<Vec<IssueWithDependencyMetadata>> {
        let mut stmt = self.conn.prepare(
            "SELECT d.depends_on_id, i.title, i.status, i.priority, d.type
             FROM dependencies d
             LEFT JOIN issues i ON d.depends_on_id = i.id
             WHERE d.issue_id = ?
             ORDER BY i.priority ASC, i.created_at DESC",
        )?;

        let deps = stmt
            .query_map([issue_id], |row| {
                Ok(IssueWithDependencyMetadata {
                    id: row.get::<_, String>(0)?,
                    title: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    status: parse_status(row.get::<_, Option<String>>(2)?.as_deref()),
                    priority: Priority(row.get::<_, Option<i32>>(3)?.unwrap_or(2)),
                    dep_type: row
                        .get::<_, Option<String>>(4)?
                        .unwrap_or_else(|| "blocks".to_string()),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(deps)
    }

    /// Get dependents with metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_dependents_with_metadata(
        &self,
        issue_id: &str,
    ) -> Result<Vec<IssueWithDependencyMetadata>> {
        let mut stmt = self.conn.prepare(
            "SELECT d.issue_id, i.title, i.status, i.priority, d.type
             FROM dependencies d
             LEFT JOIN issues i ON d.issue_id = i.id
             WHERE d.depends_on_id = ?
             ORDER BY i.priority ASC, i.created_at DESC",
        )?;

        let deps = stmt
            .query_map([issue_id], |row| {
                Ok(IssueWithDependencyMetadata {
                    id: row.get(0)?,
                    title: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                    status: parse_status(row.get::<_, Option<String>>(2)?.as_deref()),
                    priority: Priority(row.get::<_, Option<i32>>(3)?.unwrap_or(2)),
                    dep_type: row
                        .get::<_, Option<String>>(4)?
                        .unwrap_or_else(|| "blocks".to_string()),
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(deps)
    }

    /// Get parent issue ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_parent_id(&self, issue_id: &str) -> Result<Option<String>> {
        let result = self.conn.query_row(
            "SELECT depends_on_id FROM dependencies WHERE issue_id = ? AND type = 'parent-child'",
            [issue_id],
            |row| row.get(0),
        );

        match result {
            Ok(parent_id) => Ok(Some(parent_id)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Get IDs of issues that depend on this one.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_dependents(&self, issue_id: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT issue_id FROM dependencies WHERE depends_on_id = ?")?;
        let ids = stmt
            .query_map([issue_id], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// Get IDs of issues that this one depends on.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_dependencies(&self, issue_id: &str) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT depends_on_id FROM dependencies WHERE issue_id = ?")?;
        let ids = stmt
            .query_map([issue_id], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// Count how many dependencies an issue has.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn count_dependencies(&self, issue_id: &str) -> Result<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT count(*) FROM dependencies WHERE issue_id = ?",
            [issue_id],
            |row| row.get(0),
        )?;
        // count is always non-negative from COUNT(*), safe to cast
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        Ok(count as usize)
    }

    /// Count how many issues depend on this one.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn count_dependents(&self, issue_id: &str) -> Result<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT count(*) FROM dependencies WHERE depends_on_id = ?",
            [issue_id],
            |row| row.get(0),
        )?;
        // count is always non-negative from COUNT(*), safe to cast
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        Ok(count as usize)
    }

    /// Count dependencies for multiple issues efficiently.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn count_dependencies_for_issues(
        &self,
        issue_ids: &[String],
    ) -> Result<HashMap<String, usize>> {
        const SQLITE_VAR_LIMIT: usize = 900;

        if issue_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let mut map: HashMap<String, usize> = HashMap::new();

        for chunk in issue_ids.chunks(SQLITE_VAR_LIMIT) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            let sql = format!(
                "SELECT issue_id, COUNT(*) FROM dependencies WHERE issue_id IN ({}) GROUP BY issue_id",
                placeholders.join(",")
            );

            let params: Vec<&dyn rusqlite::ToSql> =
                chunk.iter().map(|s| s as &dyn rusqlite::ToSql).collect();

            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(params.as_slice(), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;

            for row in rows {
                let (issue_id, count) = row?;
                map.insert(issue_id, usize::try_from(count).unwrap_or(0));
            }
        }

        Ok(map)
    }

    /// Count dependents for multiple issues efficiently.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn count_dependents_for_issues(
        &self,
        issue_ids: &[String],
    ) -> Result<HashMap<String, usize>> {
        const SQLITE_VAR_LIMIT: usize = 900;

        if issue_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let mut map: HashMap<String, usize> = HashMap::new();

        for chunk in issue_ids.chunks(SQLITE_VAR_LIMIT) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            let sql = format!(
                "SELECT depends_on_id, COUNT(*) FROM dependencies WHERE depends_on_id IN ({}) GROUP BY depends_on_id",
                placeholders.join(",")
            );

            let params: Vec<&dyn rusqlite::ToSql> =
                chunk.iter().map(|s| s as &dyn rusqlite::ToSql).collect();

            let mut stmt = self.conn.prepare(&sql)?;
            let rows = stmt.query_map(params.as_slice(), |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;

            for row in rows {
                let (issue_id, count) = row?;
                map.insert(issue_id, usize::try_from(count).unwrap_or(0));
            }
        }

        Ok(map)
    }

    /// Fetch a config value.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_config(&self, key: &str) -> Result<Option<String>> {
        let value = self
            .conn
            .query_row("SELECT value FROM config WHERE key = ?", [key], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(value)
    }

    /// Fetch all config values from the config table.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_all_config(&self) -> Result<HashMap<String, String>> {
        let mut stmt = self.conn.prepare("SELECT key, value FROM config")?;
        let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;

        let mut map = HashMap::new();
        for row in rows {
            let (key, value) = row?;
            map.insert(key, value);
        }
        Ok(map)
    }

    /// Set a config value.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn set_config(&mut self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO config (key, value) VALUES (?, ?)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![key, value],
        )?;
        Ok(())
    }

    /// Delete a config value.
    ///
    /// Returns `true` if a value was deleted, `false` if the key didn't exist.
    ///
    /// # Errors
    ///
    /// Returns an error if the database delete fails.
    pub fn delete_config(&mut self, key: &str) -> Result<bool> {
        let deleted = self
            .conn
            .execute("DELETE FROM config WHERE key = ?", rusqlite::params![key])?;
        Ok(deleted > 0)
    }

    // ========================================================================
    // Export-related methods
    // ========================================================================

    /// Get all issues for JSONL export.
    ///
    /// Includes tombstones (for sync propagation), excludes ephemerals and wisps.
    /// Returns issues sorted by ID for deterministic output.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_all_issues_for_export(&self) -> Result<Vec<Issue>> {
        let sql = r"SELECT id, content_hash, title, description, design, acceptance_criteria, notes,
                           status, priority, issue_type, assignee, owner, estimated_minutes,
                           created_at, created_by, updated_at, closed_at, close_reason, closed_by_session,
                           due_at, defer_until, external_ref, source_system, source_repo,
                           deleted_at, deleted_by, delete_reason, original_type, compaction_level,
                           compacted_at, compacted_at_commit, original_size, sender, ephemeral,
                           pinned, is_template
                    FROM issues
                    WHERE (ephemeral = 0 OR ephemeral IS NULL)
                      AND id NOT LIKE '%-wisp-%'
                    ORDER BY id ASC";

        let mut stmt = self.conn.prepare(sql)?;
        let issues = stmt
            .query_map([], |row| self.issue_from_row(row))?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        Ok(issues)
    }

    /// Get all dependency records for all issues.
    ///
    /// Returns a map from `issue_id` to its list of Dependency records.
    /// This avoids N+1 queries when populating issues for export.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_all_dependency_records(
        &self,
    ) -> Result<HashMap<String, Vec<crate::model::Dependency>>> {
        use crate::model::{Dependency, DependencyType};

        let mut stmt = self.conn.prepare(
            "SELECT issue_id, depends_on_id, type, created_at, created_by, metadata, thread_id
             FROM dependencies
             ORDER BY issue_id, depends_on_id",
        )?;

        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                Dependency {
                    issue_id: row.get(0)?,
                    depends_on_id: row.get(1)?,
                    dep_type: row
                        .get::<_, Option<String>>(2)?
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(DependencyType::Blocks),
                    created_at: parse_datetime(&row.get::<_, String>(3)?),
                    created_by: row.get(4)?,
                    metadata: row.get(5)?,
                    thread_id: row.get(6)?,
                },
            ))
        })?;

        let mut map: HashMap<String, Vec<Dependency>> = HashMap::new();
        for row in rows {
            let (issue_id, dep) = row?;
            map.entry(issue_id).or_default().push(dep);
        }
        Ok(map)
    }

    /// Get all comments for all issues.
    ///
    /// Returns a map from `issue_id` to its list of comments.
    /// This avoids N+1 queries when populating issues for export.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_all_comments(&self) -> Result<HashMap<String, Vec<Comment>>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, issue_id, author, text, created_at
             FROM comments
             ORDER BY issue_id, created_at ASC",
        )?;

        let rows = stmt.query_map([], |row| {
            Ok(Comment {
                id: row.get(0)?,
                issue_id: row.get(1)?,
                author: row.get(2)?,
                body: row.get(3)?,
                created_at: parse_datetime(&row.get::<_, String>(4)?),
            })
        })?;

        let mut map: HashMap<String, Vec<Comment>> = HashMap::new();
        for row in rows {
            let comment = row?;
            map.entry(comment.issue_id.clone())
                .or_default()
                .push(comment);
        }
        Ok(map)
    }

    /// Get IDs of all dirty issues (issues modified since last export).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_dirty_issue_ids(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT issue_id FROM dirty_issues ORDER BY marked_at")?;
        let ids = stmt
            .query_map([], |row| row.get(0))?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    /// Clear dirty flags for the given issue IDs.
    ///
    /// Call this after successful export to the default JSONL path.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn clear_dirty_issues(&mut self, issue_ids: &[String]) -> Result<usize> {
        if issue_ids.is_empty() {
            return Ok(0);
        }

        let placeholders: Vec<&str> = issue_ids.iter().map(|_| "?").collect();
        let sql = format!(
            "DELETE FROM dirty_issues WHERE issue_id IN ({})",
            placeholders.join(",")
        );

        let params: Vec<&dyn rusqlite::ToSql> = issue_ids
            .iter()
            .map(|s| s as &dyn rusqlite::ToSql)
            .collect();

        let count = self.conn.execute(&sql, params.as_slice())?;
        Ok(count)
    }

    /// Clear all dirty flags.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn clear_all_dirty_issues(&mut self) -> Result<usize> {
        let count = self.conn.execute("DELETE FROM dirty_issues", [])?;
        Ok(count)
    }

    // =========================================================================
    // Export Hashes (for incremental export)
    // =========================================================================

    /// Get the stored export hash for an issue.
    ///
    /// Returns the content hash and exported timestamp if the issue has been exported.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_export_hash(&self, issue_id: &str) -> Result<Option<(String, String)>> {
        let result = self.conn.query_row(
            "SELECT content_hash, exported_at FROM export_hashes WHERE issue_id = ?",
            [issue_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        );
        match result {
            Ok(value) => Ok(Some(value)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(BeadsError::Database(e)),
        }
    }

    /// Set the export hash for an issue after successful export.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn set_export_hash(&mut self, issue_id: &str, content_hash: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT OR REPLACE INTO export_hashes (issue_id, content_hash, exported_at) VALUES (?, ?, ?)",
            rusqlite::params![issue_id, content_hash, now],
        )?;
        Ok(())
    }

    /// Batch set export hashes for multiple issues after successful export.
    ///
    /// More efficient than calling `set_export_hash` in a loop.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub fn set_export_hashes(&mut self, exports: &[(String, String)]) -> Result<usize> {
        if exports.is_empty() {
            return Ok(0);
        }
        let now = Utc::now().to_rfc3339();
        let mut stmt = self.conn.prepare(
            "INSERT OR REPLACE INTO export_hashes (issue_id, content_hash, exported_at) VALUES (?, ?, ?)",
        )?;
        let mut count = 0;
        for (issue_id, content_hash) in exports {
            stmt.execute(rusqlite::params![issue_id, content_hash, now])?;
            count += 1;
        }
        Ok(count)
    }

    /// Clear all export hashes.
    ///
    /// Call this before import to ensure fresh state.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn clear_all_export_hashes(&mut self) -> Result<usize> {
        let count = self.conn.execute("DELETE FROM export_hashes", [])?;
        Ok(count)
    }

    /// Get issues that need to be exported (dirty issues whose content hash differs from stored export hash).
    ///
    /// This enables incremental export by filtering out issues that haven't actually changed
    /// since the last export, even if they were marked dirty.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_issues_needing_export(&self, dirty_ids: &[String]) -> Result<Vec<String>> {
        if dirty_ids.is_empty() {
            return Ok(vec![]);
        }

        // Build a query that finds dirty issues where:
        // 1. The issue has no export hash (never exported), OR
        // 2. The issue's current content hash differs from the stored export hash
        let placeholders: Vec<&str> = dirty_ids.iter().map(|_| "?").collect();
        let sql = format!(
            "SELECT i.id FROM issues i
             WHERE i.id IN ({})
               AND i.deleted_at IS NULL
               AND (
                 NOT EXISTS (SELECT 1 FROM export_hashes e WHERE e.issue_id = i.id)
                 OR i.content_hash != (SELECT e.content_hash FROM export_hashes e WHERE e.issue_id = i.id)
               )
             ORDER BY i.id",
            placeholders.join(",")
        );

        let params: Vec<&dyn rusqlite::ToSql> = dirty_ids
            .iter()
            .map(|s| s as &dyn rusqlite::ToSql)
            .collect();

        let mut stmt = self.conn.prepare(&sql)?;
        let ids = stmt
            .query_map(params.as_slice(), |row| row.get(0))?
            .collect::<std::result::Result<Vec<String>, _>>()?;
        Ok(ids)
    }

    /// Get a metadata value by key.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_metadata(&self, key: &str) -> Result<Option<String>> {
        let result =
            self.conn
                .query_row("SELECT value FROM metadata WHERE key = ?", [key], |row| {
                    row.get(0)
                });
        match result {
            Ok(value) => Ok(Some(value)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(BeadsError::Database(e)),
        }
    }

    /// Set a metadata value.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn set_metadata(&mut self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO metadata (key, value) VALUES (?, ?)",
            [key, value],
        )?;
        Ok(())
    }

    /// Delete a metadata key.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn delete_metadata(&mut self, key: &str) -> Result<bool> {
        let count = self
            .conn
            .execute("DELETE FROM metadata WHERE key = ?", [key])?;
        Ok(count > 0)
    }

    /// Count issues in the database.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn count_all_issues(&self) -> Result<usize> {
        let count: i64 = self
            .conn
            .query_row("SELECT count(*) FROM issues", [], |row| row.get(0))?;
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// Get full issue details.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_issue_details(
        &self,
        id: &str,
        include_comments: bool,
        include_events: bool,
        event_limit: usize,
    ) -> Result<Option<IssueDetails>> {
        let Some(issue) = self.get_issue(id)? else {
            return Ok(None);
        };

        let labels = self.get_labels(id)?;
        let dependencies = self.get_dependencies_with_metadata(id)?;
        let dependents = self.get_dependents_with_metadata(id)?;
        let comments = if include_comments {
            self.get_comments(id)?
        } else {
            vec![]
        };
        let events = if include_events {
            get_events(&self.conn, id, event_limit)?
        } else {
            vec![]
        };
        let parent = self.get_parent_id(id)?;

        Ok(Some(IssueDetails {
            issue,
            labels,
            dependencies,
            dependents,
            comments,
            events,
            parent,
        }))
    }

    /// Convert empty string to None for bd compatibility.
    /// The database stores empty strings for NOT NULL DEFAULT '' fields,
    /// but the API contract expects None for unset values.
    #[inline]
    fn empty_to_none(s: Option<String>) -> Option<String> {
        s.filter(|v| !v.is_empty())
    }

    #[allow(clippy::unused_self)]
    fn issue_from_row(&self, row: &rusqlite::Row) -> rusqlite::Result<Issue> {
        Ok(Issue {
            id: row.get(0)?,
            content_hash: row.get::<_, Option<String>>(1)?,
            title: row.get(2)?,
            description: Self::empty_to_none(row.get::<_, Option<String>>(3)?),
            design: Self::empty_to_none(row.get::<_, Option<String>>(4)?),
            acceptance_criteria: Self::empty_to_none(row.get::<_, Option<String>>(5)?),
            notes: Self::empty_to_none(row.get::<_, Option<String>>(6)?),
            status: parse_status(row.get::<_, Option<String>>(7)?.as_deref()),
            priority: Priority(row.get::<_, Option<i32>>(8)?.unwrap_or(2)),
            issue_type: parse_issue_type(row.get::<_, Option<String>>(9)?.as_deref()),
            assignee: Self::empty_to_none(row.get::<_, Option<String>>(10)?),
            owner: Self::empty_to_none(row.get::<_, Option<String>>(11)?),
            estimated_minutes: row.get::<_, Option<i32>>(12)?,
            created_at: parse_datetime(&row.get::<_, String>(13)?),
            created_by: Self::empty_to_none(row.get::<_, Option<String>>(14)?),
            updated_at: parse_datetime(&row.get::<_, String>(15)?),
            closed_at: row
                .get::<_, Option<String>>(16)?
                .as_deref()
                .map(parse_datetime),
            close_reason: Self::empty_to_none(row.get::<_, Option<String>>(17)?),
            closed_by_session: Self::empty_to_none(row.get::<_, Option<String>>(18)?),
            due_at: row
                .get::<_, Option<String>>(19)?
                .as_deref()
                .map(parse_datetime),
            defer_until: row
                .get::<_, Option<String>>(20)?
                .as_deref()
                .map(parse_datetime),
            external_ref: row.get::<_, Option<String>>(21)?,
            source_system: Self::empty_to_none(row.get::<_, Option<String>>(22)?),
            source_repo: Self::empty_to_none(row.get::<_, Option<String>>(23)?),
            deleted_at: row
                .get::<_, Option<String>>(24)?
                .as_deref()
                .map(parse_datetime),
            deleted_by: Self::empty_to_none(row.get::<_, Option<String>>(25)?),
            delete_reason: Self::empty_to_none(row.get::<_, Option<String>>(26)?),
            original_type: Self::empty_to_none(row.get::<_, Option<String>>(27)?),
            compaction_level: row.get::<_, Option<i32>>(28)?,
            compacted_at: row
                .get::<_, Option<String>>(29)?
                .as_deref()
                .map(parse_datetime),
            compacted_at_commit: row.get::<_, Option<String>>(30)?,
            original_size: row.get::<_, Option<i32>>(31)?,
            sender: Self::empty_to_none(row.get::<_, Option<String>>(32)?),
            ephemeral: row.get::<_, Option<i32>>(33)?.unwrap_or(0) != 0,
            pinned: row.get::<_, Option<i32>>(34)?.unwrap_or(0) != 0,
            is_template: row.get::<_, Option<i32>>(35)?.unwrap_or(0) != 0,
            labels: vec![],       // Loaded separately if needed
            dependencies: vec![], // Loaded separately if needed
            comments: vec![],     // Loaded separately if needed
        })
    }

    /// Set metadata (in tx).
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub fn set_metadata_in_tx(conn: &Connection, key: &str, value: &str) -> Result<()> {
        conn.execute(
            "INSERT OR REPLACE INTO metadata (key, value) VALUES (?, ?)",
            [key, value],
        )?;
        Ok(())
    }

    /// Clear all export hashes (in tx).
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub fn clear_all_export_hashes_in_tx(conn: &Connection) -> Result<usize> {
        let count = conn.execute("DELETE FROM export_hashes", [])?;
        Ok(count)
    }
}

/// Filter options for listing issues.
#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)]
pub struct ListFilters {
    pub statuses: Option<Vec<Status>>,
    pub types: Option<Vec<IssueType>>,
    pub priorities: Option<Vec<Priority>>,
    pub assignee: Option<String>,
    pub unassigned: bool,
    pub include_closed: bool,
    pub include_deferred: bool,
    pub include_templates: bool,
    pub title_contains: Option<String>,
    pub limit: Option<usize>,
    /// Sort field (priority, `created_at`, `updated_at`, title)
    pub sort: Option<String>,
    /// Reverse sort order
    pub reverse: bool,
    /// Filter by labels (all specified labels must match)
    pub labels: Option<Vec<String>>,
    /// Filter by labels (OR logic)
    pub labels_or: Option<Vec<String>>,
    /// Filter by `updated_at` <= timestamp
    pub updated_before: Option<DateTime<Utc>>,
    /// Filter by `updated_at` >= timestamp
    pub updated_after: Option<DateTime<Utc>>,
}

/// Fields to update on an issue.
#[derive(Debug, Clone, Default)]
pub struct IssueUpdate {
    pub title: Option<String>,
    pub description: Option<Option<String>>,
    pub design: Option<Option<String>>,
    pub acceptance_criteria: Option<Option<String>>,
    pub notes: Option<Option<String>>,
    pub status: Option<Status>,
    pub priority: Option<Priority>,
    pub issue_type: Option<IssueType>,
    pub assignee: Option<Option<String>>,
    pub owner: Option<Option<String>>,
    pub estimated_minutes: Option<Option<i32>>,
    pub due_at: Option<Option<DateTime<Utc>>>,
    pub defer_until: Option<Option<DateTime<Utc>>>,
    pub external_ref: Option<Option<String>>,
    pub closed_at: Option<Option<DateTime<Utc>>>,
    pub close_reason: Option<Option<String>>,
    pub closed_by_session: Option<Option<String>>,
    pub deleted_at: Option<Option<DateTime<Utc>>>,
    pub deleted_by: Option<Option<String>>,
    pub delete_reason: Option<Option<String>>,
    /// If true, do not rebuild the blocked cache after update.
    /// Caller is responsible for rebuilding cache if needed.
    pub skip_cache_rebuild: bool,
}

impl IssueUpdate {
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.title.is_none()
            && self.description.is_none()
            && self.design.is_none()
            && self.acceptance_criteria.is_none()
            && self.notes.is_none()
            && self.status.is_none()
            && self.priority.is_none()
            && self.issue_type.is_none()
            && self.assignee.is_none()
            && self.owner.is_none()
            && self.estimated_minutes.is_none()
            && self.due_at.is_none()
            && self.defer_until.is_none()
            && self.external_ref.is_none()
            && self.closed_at.is_none()
            && self.close_reason.is_none()
            && self.closed_by_session.is_none()
            && self.deleted_at.is_none()
            && self.deleted_by.is_none()
            && self.delete_reason.is_none()
    }
}

/// Filter options for ready issues.
#[derive(Debug, Clone, Default)]
pub struct ReadyFilters {
    pub assignee: Option<String>,
    pub unassigned: bool,
    pub labels_and: Vec<String>,
    pub labels_or: Vec<String>,
    pub types: Option<Vec<IssueType>>,
    pub priorities: Option<Vec<Priority>>,
    pub include_deferred: bool,
    pub limit: Option<usize>,
}

/// Sort policy for ready issues.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub enum ReadySortPolicy {
    /// P0/P1 first by `created_at` ASC, then others by `created_at` ASC
    #[default]
    Hybrid,
    /// Sort by priority ASC, then `created_at` ASC
    Priority,
    /// Sort by `created_at` ASC only
    Oldest,
}

fn parse_status(s: Option<&str>) -> Status {
    s.map_or_else(Status::default, |val| {
        val.parse()
            .unwrap_or_else(|_| Status::Custom(val.to_string()))
    })
}

fn parse_issue_type(s: Option<&str>) -> IssueType {
    s.and_then(|s| s.parse().ok()).unwrap_or_default()
}

fn parse_external_dependency(dep_id: &str) -> Option<(String, String)> {
    let mut parts = dep_id.splitn(3, ':');
    let prefix = parts.next()?;
    if prefix != "external" {
        return None;
    }
    let project = parts.next()?.to_string();
    let capability = parts.next()?.to_string();
    if project.is_empty() || capability.is_empty() {
        return None;
    }
    Some((project, capability))
}

fn query_external_project_capabilities(
    db_path: &Path,
    capabilities: &HashSet<String>,
) -> Result<HashSet<String>> {
    const SQLITE_VAR_LIMIT: usize = 900;

    if capabilities.is_empty() {
        return Ok(HashSet::new());
    }

    let conn = Connection::open_with_flags(db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let labels: Vec<String> = capabilities
        .iter()
        .map(|cap| format!("provides:{cap}"))
        .collect();

    let mut satisfied = HashSet::new();

    for chunk in labels.chunks(SQLITE_VAR_LIMIT) {
        let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
        let sql = format!(
            "SELECT DISTINCT l.label
             FROM labels l
             INNER JOIN issues i ON i.id = l.issue_id
             WHERE i.status IN ('closed', 'tombstone') AND l.label IN ({})",
            placeholders.join(",")
        );
        let params: Vec<&dyn rusqlite::ToSql> = chunk
            .iter()
            .map(|label| label as &dyn rusqlite::ToSql)
            .collect();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params.as_slice(), |row| row.get::<_, String>(0))?;

        for row in rows {
            let label = row?;
            if let Some(cap) = label.strip_prefix("provides:") {
                satisfied.insert(cap.to_string());
            }
        }
    }

    Ok(satisfied)
}

fn parse_datetime(s: &str) -> DateTime<Utc> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return dt.with_timezone(&Utc);
    }

    if let Ok(naive) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Utc.from_utc_datetime(&naive);
    }

    Utc::now()
}

// ============================================================================
// EXPORT/SYNC METHODS
// ============================================================================

impl SqliteStorage {
    /// Get issue with all relations populated for export.
    ///
    /// Includes labels, dependencies, and comments.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_issue_for_export(&self, id: &str) -> Result<Option<Issue>> {
        let Some(mut issue) = self.get_issue(id)? else {
            return Ok(None);
        };

        // Populate relations
        issue.labels = self.get_labels(id)?;
        issue.dependencies = self.get_dependencies_full(id)?;
        issue.comments = self.get_comments(id)?;

        Ok(Some(issue))
    }

    /// Get dependencies as full Dependency structs for export.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_dependencies_full(&self, issue_id: &str) -> Result<Vec<crate::model::Dependency>> {
        let mut stmt = self.conn.prepare(
            "SELECT issue_id, depends_on_id, type, created_at, created_by, metadata, thread_id
             FROM dependencies
             WHERE issue_id = ?
             ORDER BY depends_on_id",
        )?;

        let deps = stmt
            .query_map([issue_id], |row| {
                let created_at_str: String = row.get(3)?;
                Ok(crate::model::Dependency {
                    issue_id: row.get(0)?,
                    depends_on_id: row.get(1)?,
                    dep_type: row
                        .get::<_, Option<String>>(2)?
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(crate::model::DependencyType::Blocks),
                    created_at: parse_datetime(&created_at_str),
                    created_by: row.get(4)?,
                    metadata: row.get(5)?,
                    thread_id: row.get(6)?,
                })
            })?
            .filter_map(std::result::Result::ok)
            .collect();

        Ok(deps)
    }

    /// Clear dirty flags for the given issue IDs.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub fn clear_dirty_flags(&mut self, ids: &[String]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }

        let placeholders = vec!["?"; ids.len()].join(", ");
        let sql = format!("DELETE FROM dirty_issues WHERE issue_id IN ({placeholders})");

        let params: Vec<&dyn rusqlite::ToSql> =
            ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
        let deleted = self.conn.execute(&sql, params.as_slice())?;

        Ok(deleted)
    }

    /// Clear all dirty flags.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub fn clear_all_dirty_flags(&mut self) -> Result<usize> {
        let deleted = self.conn.execute("DELETE FROM dirty_issues", [])?;
        Ok(deleted)
    }

    /// Get the count of issues (for safety guard).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn count_exportable_issues(&self) -> Result<usize> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM issues WHERE ephemeral = 0 AND id NOT LIKE '%-wisp-%'",
            [],
            |row| row.get(0),
        )?;
        // count is always non-negative from COUNT(*), safe to cast
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        Ok(count as usize)
    }

    /// Check if a dependency exists between two issues.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn dependency_exists_between(&self, issue_id: &str, depends_on_id: &str) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM dependencies WHERE issue_id = ? AND depends_on_id = ?",
            rusqlite::params![issue_id, depends_on_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// Check if adding a dependency would create a cycle.
    ///
    /// If `blocking_only` is true, only considers blocking dependency types
    /// ('blocks', 'parent-child', 'conditional-blocks') for cycle detection.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn would_create_cycle(
        &self,
        issue_id: &str,
        depends_on_id: &str,
        blocking_only: bool,
    ) -> Result<bool> {
        Self::check_cycle(&self.conn, issue_id, depends_on_id, blocking_only)
    }

    /// Detect all cycles in the dependency graph.
    ///
    /// Returns a list of cycles, where each cycle is a vector of issue IDs.
    /// Uses an iterative DFS to avoid stack overflow on deep graphs.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn detect_all_cycles(&self) -> Result<Vec<Vec<String>>> {
        use std::collections::{HashMap, HashSet};

        // Get all dependencies
        let mut graph: HashMap<String, Vec<String>> = HashMap::new();
        let mut stmt = self
            .conn
            .prepare("SELECT issue_id, depends_on_id FROM dependencies")?;

        let edges = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;

        for edge in edges {
            let (from, to) = edge?;
            graph.entry(from).or_default().push(to);
        }

        let mut cycles = Vec::new();
        let mut visited = HashSet::new();
        let mut rec_stack = HashSet::new();
        let mut path = Vec::new();

        // Stack stores (node_id, neighbor_index)
        let mut stack: Vec<(String, usize)> = Vec::new();

        // Sort keys for deterministic output
        let mut keys: Vec<_> = graph.keys().cloned().collect();
        keys.sort();

        for node in keys {
            if visited.contains(&node) {
                continue;
            }

            stack.push((node.clone(), 0));
            visited.insert(node.clone());
            rec_stack.insert(node.clone());
            path.push(node.clone());

            while let Some((u, idx)) = stack.last_mut() {
                let neighbors = graph.get(u);

                if let Some(neighbors) = neighbors {
                    if *idx < neighbors.len() {
                        let v = &neighbors[*idx];
                        *idx += 1;

                        if rec_stack.contains(v) {
                            // Found a cycle: reconstruct it from the current path
                            if let Some(start_pos) = path.iter().position(|x| x == v) {
                                let mut cycle = path[start_pos..].to_vec();
                                cycle.push(v.clone()); // Close the loop
                                cycles.push(cycle);
                            }
                        } else if !visited.contains(v) {
                            visited.insert(v.clone());
                            rec_stack.insert(v.clone());
                            path.push(v.clone());
                            stack.push((v.clone(), 0));
                        }
                        continue;
                    }
                }

                // Finished processing all neighbors of u
                rec_stack.remove(u);
                path.pop();
                stack.pop();
            }
        }

        Ok(cycles)
    }

    // ===== Import Helper Methods =====

    /// Find an issue by external reference.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn find_by_external_ref(&self, external_ref: &str) -> Result<Option<Issue>> {
        let result = self.conn.query_row(
            r"SELECT id, content_hash, title, description, design, acceptance_criteria, notes,
                     status, priority, issue_type, assignee, owner, estimated_minutes,
                     created_at, created_by, updated_at, closed_at, close_reason, closed_by_session,
                     due_at, defer_until, external_ref, source_system, source_repo,
                     deleted_at, deleted_by, delete_reason, original_type, compaction_level,
                     compacted_at, compacted_at_commit, original_size, sender, ephemeral,
                     pinned, is_template
               FROM issues WHERE external_ref = ?",
            [external_ref],
            |row| self.issue_from_row(row),
        );
        match result {
            Ok(issue) => Ok(Some(issue)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(BeadsError::Database(e)),
        }
    }

    /// Find an issue by content hash.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn find_by_content_hash(&self, content_hash: &str) -> Result<Option<Issue>> {
        let result = self.conn.query_row(
            r"SELECT id, content_hash, title, description, design, acceptance_criteria, notes,
                     status, priority, issue_type, assignee, owner, estimated_minutes,
                     created_at, created_by, updated_at, closed_at, close_reason, closed_by_session,
                     due_at, defer_until, external_ref, source_system, source_repo,
                     deleted_at, deleted_by, delete_reason, original_type, compaction_level,
                     compacted_at, compacted_at_commit, original_size, sender, ephemeral,
                     pinned, is_template
               FROM issues WHERE content_hash = ?",
            [content_hash],
            |row| self.issue_from_row(row),
        );
        match result {
            Ok(issue) => Ok(Some(issue)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(BeadsError::Database(e)),
        }
    }

    /// Check if an issue is a tombstone (deleted).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn is_tombstone(&self, id: &str) -> Result<bool> {
        let result: rusqlite::Result<String> =
            self.conn
                .query_row("SELECT status FROM issues WHERE id = ?", [id], |row| {
                    row.get(0)
                });
        match result {
            Ok(status) => Ok(status == "tombstone"),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(false),
            Err(e) => Err(BeadsError::Database(e)),
        }
    }

    /// Upsert an issue (create or update) for import operations.
    ///
    /// Uses INSERT OR REPLACE to atomically handle both cases.
    /// This does NOT trigger dirty tracking or events.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    #[allow(clippy::too_many_lines)]
    pub fn upsert_issue_for_import(&mut self, issue: &Issue) -> Result<bool> {
        let status_str = issue.status.as_str();
        let issue_type_str = issue.issue_type.as_str();
        let created_at_str = issue.created_at.to_rfc3339();
        let updated_at_str = issue.updated_at.to_rfc3339();
        let closed_at_str = issue.closed_at.map(|dt| dt.to_rfc3339());
        let due_at_str = issue.due_at.map(|dt| dt.to_rfc3339());
        let defer_until_str = issue.defer_until.map(|dt| dt.to_rfc3339());
        let deleted_at_str = issue.deleted_at.map(|dt| dt.to_rfc3339());
        let compacted_at_str = issue.compacted_at.map(|dt| dt.to_rfc3339());

        let rows = self.conn.execute(
            r"INSERT OR REPLACE INTO issues (
                id, content_hash, title, description, design, acceptance_criteria, notes,
                status, priority, issue_type, assignee, owner, estimated_minutes,
                created_at, created_by, updated_at, closed_at, close_reason, closed_by_session,
                due_at, defer_until, external_ref, source_system, source_repo,
                deleted_at, deleted_by, delete_reason, original_type, compaction_level,
                compacted_at, compacted_at_commit, original_size, sender, ephemeral,
                pinned, is_template
            ) VALUES (
                ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
            )",
            rusqlite::params![
                issue.id,
                issue.content_hash,
                issue.title,
                issue.description,
                issue.design,
                issue.acceptance_criteria,
                issue.notes,
                status_str,
                issue.priority.0,
                issue_type_str,
                issue.assignee,
                issue.owner,
                issue.estimated_minutes,
                created_at_str,
                issue.created_by,
                updated_at_str,
                closed_at_str,
                issue.close_reason,
                issue.closed_by_session,
                due_at_str,
                defer_until_str,
                issue.external_ref,
                issue.source_system,
                issue.source_repo.as_deref().unwrap_or("."),
                deleted_at_str,
                issue.deleted_by,
                issue.delete_reason,
                issue.original_type,
                issue.compaction_level.unwrap_or(0),
                compacted_at_str,
                issue.compacted_at_commit,
                issue.original_size.unwrap_or(0),
                issue.sender,
                issue.ephemeral,
                issue.pinned,
                issue.is_template,
            ],
        )?;

        Ok(rows > 0)
    }

    /// Sync labels for an issue (remove existing, add new).
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub fn sync_labels_for_import(&mut self, issue_id: &str, labels: &[String]) -> Result<()> {
        // Remove existing labels
        self.conn
            .execute("DELETE FROM labels WHERE issue_id = ?", [issue_id])?;

        // Add new labels
        for label in labels {
            self.conn.execute(
                "INSERT INTO labels (issue_id, label) VALUES (?, ?)",
                rusqlite::params![issue_id, label],
            )?;
        }

        Ok(())
    }

    /// Sync dependencies for an issue (remove existing, add new).
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub fn sync_dependencies_for_import(
        &mut self,
        issue_id: &str,
        dependencies: &[crate::model::Dependency],
    ) -> Result<()> {
        // Remove existing dependencies where this issue is the dependent
        self.conn
            .execute("DELETE FROM dependencies WHERE issue_id = ?", [issue_id])?;

        // Add new dependencies
        for dep in dependencies {
            self.conn.execute(
                "INSERT OR IGNORE INTO dependencies (issue_id, depends_on_id, type, created_at, created_by)
                 VALUES (?, ?, ?, CURRENT_TIMESTAMP, 'import')",
                rusqlite::params![issue_id, dep.depends_on_id, dep.dep_type.as_str()],
            )?;
        }

        Ok(())
    }

    /// Sync comments for an issue (remove existing, add new).
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub fn sync_comments_for_import(
        &mut self,
        issue_id: &str,
        comments: &[crate::model::Comment],
    ) -> Result<()> {
        // Remove existing comments
        self.conn
            .execute("DELETE FROM comments WHERE issue_id = ?", [issue_id])?;

        // Add new comments
        for comment in comments {
            self.conn.execute(
                "INSERT INTO comments (issue_id, author, text, created_at) VALUES (?, ?, ?, ?)",
                rusqlite::params![
                    issue_id,
                    comment.author,
                    comment.body,
                    comment.created_at.to_rfc3339()
                ],
            )?;
        }

        Ok(())
    }
}

/// Implement the `DependencyStore` trait for `SqliteStorage`.
impl crate::validation::DependencyStore for SqliteStorage {
    fn issue_exists(&self, id: &str) -> std::result::Result<bool, crate::error::BeadsError> {
        self.id_exists(id)
    }

    fn dependency_exists(
        &self,
        issue_id: &str,
        depends_on_id: &str,
    ) -> std::result::Result<bool, crate::error::BeadsError> {
        self.dependency_exists_between(issue_id, depends_on_id)
    }

    fn would_create_cycle(
        &self,
        issue_id: &str,
        depends_on_id: &str,
    ) -> std::result::Result<bool, crate::error::BeadsError> {
        Self::check_cycle(&self.conn, issue_id, depends_on_id, true)
    }
}

fn insert_comment_row(
    tx: &Transaction<'_>,
    issue_id: &str,
    author: &str,
    text: &str,
) -> Result<i64> {
    tx.execute(
        "INSERT INTO comments (issue_id, author, text, created_at)
         VALUES (?, ?, ?, CURRENT_TIMESTAMP)",
        rusqlite::params![issue_id, author, text],
    )?;
    Ok(tx.last_insert_rowid())
}

fn fetch_comment(tx: &Transaction<'_>, comment_id: i64) -> Result<Comment> {
    tx.query_row(
        "SELECT id, issue_id, author, text, created_at FROM comments WHERE id = ?",
        rusqlite::params![comment_id],
        |row| {
            Ok(Comment {
                id: row.get(0)?,
                issue_id: row.get(1)?,
                author: row.get(2)?,
                body: row.get(3)?,
                created_at: parse_datetime(&row.get::<_, String>(4)?),
            })
        },
    )
    .map_err(BeadsError::from)
}

#[cfg(test)]
impl SqliteStorage {
    /// Execute raw SQL for tests.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQL execution fails.
    pub fn execute_test_sql(&self, sql: &str) -> Result<()> {
        self.conn.execute_batch(sql)?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::similar_names)]
mod tests {
    use super::*;
    use crate::model::{Issue, IssueType, Priority, Status};
    use chrono::{DateTime, TimeZone, Utc};
    use std::fs;
    use tempfile::TempDir;

    fn make_issue(
        id: &str,
        title: &str,
        status: Status,
        priority: i32,
        assignee: Option<&str>,
        created_at: DateTime<Utc>,
        defer_until: Option<DateTime<Utc>>,
    ) -> Issue {
        Issue {
            id: id.to_string(),
            title: title.to_string(),
            status,
            priority: Priority(priority),
            issue_type: IssueType::Task,
            created_at,
            updated_at: created_at,
            defer_until,
            content_hash: None,
            description: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            assignee: assignee.map(str::to_string),
            owner: None,
            estimated_minutes: None,
            created_by: None,
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            due_at: None,
            external_ref: None,
            source_system: None,
            source_repo: None,
            deleted_at: None,
            deleted_by: None,
            delete_reason: None,
            original_type: None,
            compaction_level: None,
            compacted_at: None,
            compacted_at_commit: None,
            original_size: None,
            sender: None,
            ephemeral: false,
            pinned: false,
            is_template: false,
            labels: vec![],
            dependencies: vec![],
            comments: vec![],
        }
    }

    #[test]
    fn test_open_memory() {
        let storage = SqliteStorage::open_memory();
        assert!(storage.is_ok());
    }

    #[test]
    fn test_create_issue() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue = Issue {
            id: "bd-1".to_string(),
            title: "Test Issue".to_string(),
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            content_hash: None,
            description: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_by: None,
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            defer_until: None,
            due_at: None,
            external_ref: None,
            source_system: None,
            source_repo: None,
            deleted_at: None,
            deleted_by: None,
            delete_reason: None,
            original_type: None,
            compaction_level: None,
            compacted_at: None,
            compacted_at_commit: None,
            original_size: None,
            sender: None,
            ephemeral: false,
            pinned: false,
            is_template: false,
            labels: vec![],
            dependencies: vec![],
            comments: vec![],
        };

        storage.create_issue(&issue, "tester").unwrap();

        // Verify it exists (raw query since get_issue not impl yet)
        let count: i64 = storage
            .conn
            .query_row(
                "SELECT count(*) FROM issues WHERE id = ?",
                ["bd-1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        // Verify event
        let event_count: i64 = storage
            .conn
            .query_row(
                "SELECT count(*) FROM events WHERE issue_id = ?",
                ["bd-1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(event_count, 1);

        // Verify dirty
        let dirty_count: i64 = storage
            .conn
            .query_row(
                "SELECT count(*) FROM dirty_issues WHERE issue_id = ?",
                ["bd-1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(dirty_count, 1);
    }

    #[test]
    fn test_transaction_rollback_on_error() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue = make_issue("bd-tx1", "Tx Test", Status::Open, 2, None, Utc::now(), None);
        storage.create_issue(&issue, "tester").unwrap();

        // Attempt a mutation that fails
        let result: Result<()> = storage.mutate("fail_op", "tester", |_tx, ctx| {
            // Do something valid first (record an event)
            ctx.record_event(
                EventType::Updated,
                "bd-tx1",
                Some("Should be rolled back".to_string()),
            );

            // Return error to trigger rollback
            Err(BeadsError::Config("Planned failure".to_string()))
        });

        assert!(result.is_err());

        // Verify side effects (event) are gone
        let events = storage.get_events("bd-tx1", 100).unwrap();
        // Should only have the creation event
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::Created);
    }

    #[test]
    fn test_external_dependency_blocks_and_propagates_to_children() {
        let temp = TempDir::new().unwrap();
        let external_root = temp.path().join("extproj");
        let beads_dir = external_root.join(".beads");
        fs::create_dir_all(&beads_dir).unwrap();

        let db_path = beads_dir.join("beads.db");
        let _external_storage = SqliteStorage::open(&db_path).unwrap();

        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 3, 3, 0, 0, 0).unwrap();
        let parent = make_issue("bd-p1", "Parent", Status::Open, 2, None, t1, None);
        let child = make_issue("bd-c1", "Child", Status::Open, 2, None, t1, None);
        storage.create_issue(&parent, "tester").unwrap();
        storage.create_issue(&child, "tester").unwrap();

        // Parent (bd-p1) depends on external capability
        storage
            .add_dependency("bd-p1", "external:extproj:capability", "blocks", "tester")
            .unwrap();

        // Child (bd-c1) depends on Parent (bd-p1) via parent-child
        storage
            .add_dependency("bd-c1", "bd-p1", "parent-child", "tester")
            .unwrap();

        let mut external_db_paths = HashMap::new();
        external_db_paths.insert("extproj".to_string(), db_path);

        let statuses = storage
            .resolve_external_dependency_statuses(&external_db_paths, true)
            .unwrap();
        assert_eq!(statuses.get("external:extproj:capability"), Some(&false));

        let blockers = storage.external_blockers(&statuses).unwrap();
        let parent_blockers = blockers.get("bd-p1").expect("parent blockers");
        assert!(
            parent_blockers
                .iter()
                .any(|b| b.starts_with("external:extproj:capability"))
        );
        let child_blockers = blockers.get("bd-c1").expect("child blockers");
        assert!(child_blockers.iter().any(|b| b == "bd-p1:parent-blocked"));
    }

    #[test]
    fn test_update_issue_changes_fields() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 5, 1, 0, 0, 0).unwrap();

        let issue = make_issue("bd-u1", "Update me", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue, "tester").unwrap();

        let updates = IssueUpdate {
            title: Some("Updated title".to_string()),
            description: Some(Some("New description".to_string())),
            status: Some(Status::InProgress),
            priority: Some(Priority::HIGH),
            assignee: Some(Some("alice".to_string())),
            ..IssueUpdate::default()
        };

        let updated = storage.update_issue("bd-u1", &updates, "tester").unwrap();
        assert_eq!(updated.title, "Updated title");
        assert_eq!(updated.status, Status::InProgress);
        assert_eq!(updated.priority, Priority::HIGH);
        assert_eq!(updated.assignee.as_deref(), Some("alice"));
        assert_eq!(updated.description.as_deref(), Some("New description"));
    }

    #[test]
    fn test_delete_issue_sets_tombstone() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap();

        let issue = make_issue("bd-d1", "Delete me", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue, "tester").unwrap();

        let deleted = storage
            .delete_issue("bd-d1", "tester", "cleanup", None)
            .unwrap();
        assert_eq!(deleted.status, Status::Tombstone);
        assert_eq!(deleted.delete_reason.as_deref(), Some("cleanup"));

        let is_tombstone = storage.is_tombstone("bd-d1").unwrap();
        assert!(is_tombstone);
    }

    #[test]
    fn test_get_blocked_issues_lists_blockers() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 4, 1, 0, 0, 0).unwrap();

        let blocker = make_issue("bd-b1", "Blocker", Status::Open, 1, None, t1, None);
        let blocked = make_issue("bd-b2", "Blocked", Status::Open, 2, None, t1, None);
        storage.create_issue(&blocker, "tester").unwrap();
        storage.create_issue(&blocked, "tester").unwrap();

        storage
            .add_dependency("bd-b2", "bd-b1", "blocks", "tester")
            .unwrap();

        let blocked_issues = storage.get_blocked_issues().unwrap();
        assert_eq!(blocked_issues.len(), 1);
        assert_eq!(blocked_issues[0].0.id, "bd-b2");
        assert_eq!(blocked_issues[0].1.len(), 1);
    }

    #[test]
    fn test_add_and_remove_labels_sorted() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 1, 0, 0, 0).unwrap();

        let issue = make_issue("bd-l1", "Label me", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue, "tester").unwrap();

        let added = storage.add_label("bd-l1", "backend", "tester").unwrap();
        assert!(added);
        let added = storage.add_label("bd-l1", "api", "tester").unwrap();
        assert!(added);

        let labels = storage.get_labels("bd-l1").unwrap();
        assert_eq!(labels, vec!["api".to_string(), "backend".to_string()]);

        let removed = storage.remove_label("bd-l1", "api", "tester").unwrap();
        assert!(removed);
        let labels = storage.get_labels("bd-l1").unwrap();
        assert_eq!(labels, vec!["backend".to_string()]);
    }

    #[test]
    fn test_add_dependency_and_remove() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 2, 0, 0, 0).unwrap();

        let issue_a = make_issue("bd-a1", "A", Status::Open, 2, None, t1, None);
        let issue_b = make_issue("bd-b1", "B", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue_a, "tester").unwrap();
        storage.create_issue(&issue_b, "tester").unwrap();

        let added = storage
            .add_dependency("bd-a1", "bd-b1", "blocks", "tester")
            .unwrap();
        assert!(added);

        let added = storage
            .add_dependency("bd-a1", "bd-b1", "blocks", "tester")
            .unwrap();
        assert!(!added);

        let deps = storage.get_dependencies("bd-a1").unwrap();
        assert_eq!(deps, vec!["bd-b1".to_string()]);

        let removed = storage
            .remove_dependency("bd-a1", "bd-b1", "tester")
            .unwrap();
        assert!(removed);
        let deps = storage.get_dependencies("bd-a1").unwrap();
        assert!(deps.is_empty());
    }

    #[test]
    fn test_would_create_cycle_detects_cycle() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 3, 0, 0, 0).unwrap();

        let issue_a = make_issue("bd-cy1", "A", Status::Open, 2, None, t1, None);
        let issue_b = make_issue("bd-cy2", "B", Status::Open, 2, None, t1, None);
        let issue_c = make_issue("bd-cy3", "C", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue_a, "tester").unwrap();
        storage.create_issue(&issue_b, "tester").unwrap();
        storage.create_issue(&issue_c, "tester").unwrap();

        storage
            .add_dependency("bd-cy1", "bd-cy2", "blocks", "tester")
            .unwrap();
        storage
            .add_dependency("bd-cy2", "bd-cy3", "blocks", "tester")
            .unwrap();

        let creates_cycle = storage
            .would_create_cycle("bd-cy3", "bd-cy1", true)
            .unwrap();
        assert!(creates_cycle);
    }

    #[test]
    fn test_get_comments_orders_by_created_at() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 4, 0, 0, 0).unwrap();

        let issue = Issue {
            id: "bd-c1".to_string(),
            content_hash: None,
            title: "Comment issue".to_string(),
            description: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_at: t1,
            created_by: None,
            updated_at: t1,
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            defer_until: None,
            due_at: None,
            external_ref: None,
            source_system: None,
            source_repo: None,
            deleted_at: None,
            deleted_by: None,
            delete_reason: None,
            original_type: None,
            compaction_level: None,
            compacted_at: None,
            compacted_at_commit: None,
            original_size: None,
            sender: None,
            ephemeral: false,
            pinned: false,
            is_template: false,
            labels: vec![],
            dependencies: vec![],
            comments: vec![],
        };
        storage.create_issue(&issue, "tester").unwrap();

        storage
            .conn
            .execute(
                "INSERT INTO comments (issue_id, author, text, created_at) VALUES (?, ?, ?, ?)",
                rusqlite::params!["bd-c1", "alice", "first", "2025-07-01T00:00:00Z"],
            )
            .unwrap();
        storage
            .conn
            .execute(
                "INSERT INTO comments (issue_id, author, text, created_at) VALUES (?, ?, ?, ?)",
                rusqlite::params!["bd-c1", "bob", "second", "2025-07-02T00:00:00Z"],
            )
            .unwrap();

        let comments = storage.get_comments("bd-c1").unwrap();
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].author, "alice");
        assert_eq!(comments[1].author, "bob");
    }

    #[test]
    fn test_add_comment_round_trip() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 4, 0, 0, 0).unwrap();

        let issue = Issue {
            id: "bd-c2".to_string(),
            content_hash: None,
            title: "Comment issue".to_string(),
            description: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_at: t1,
            created_by: None,
            updated_at: t1,
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            defer_until: None,
            due_at: None,
            external_ref: None,
            source_system: None,
            source_repo: None,
            deleted_at: None,
            deleted_by: None,
            delete_reason: None,
            original_type: None,
            compaction_level: None,
            compacted_at: None,
            compacted_at_commit: None,
            original_size: None,
            sender: None,
            ephemeral: false,
            pinned: false,
            is_template: false,
            labels: vec![],
            dependencies: vec![],
            comments: vec![],
        };
        storage.create_issue(&issue, "tester").unwrap();

        let comment = storage
            .add_comment("bd-c2", "alice", "Hello there")
            .unwrap();
        assert_eq!(comment.issue_id, "bd-c2");
        assert_eq!(comment.author, "alice");
        assert_eq!(comment.body, "Hello there");
        assert!(comment.id > 0);

        let comments = storage.get_comments("bd-c2").unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0], comment);
    }

    #[test]
    fn test_add_comment_marks_dirty() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 4, 0, 0, 0).unwrap();

        let issue = Issue {
            id: "bd-c3".to_string(),
            content_hash: None,
            title: "Comment issue".to_string(),
            description: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_at: t1,
            created_by: None,
            updated_at: t1,
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            defer_until: None,
            due_at: None,
            external_ref: None,
            source_system: None,
            source_repo: None,
            deleted_at: None,
            deleted_by: None,
            delete_reason: None,
            original_type: None,
            compaction_level: None,
            compacted_at: None,
            compacted_at_commit: None,
            original_size: None,
            sender: None,
            ephemeral: false,
            pinned: false,
            is_template: false,
            labels: vec![],
            dependencies: vec![],
            comments: vec![],
        };
        storage.create_issue(&issue, "tester").unwrap();

        storage
            .add_comment("bd-c3", "alice", "Dirty comment")
            .unwrap();

        let dirty_count: i64 = storage
            .conn
            .query_row(
                "SELECT count(*) FROM dirty_issues WHERE issue_id = ?",
                ["bd-c3"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(dirty_count, 1);
    }

    #[test]
    fn test_events_have_timestamps() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue = make_issue(
            "bd-e1",
            "Event Test",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        storage.create_issue(&issue, "tester").unwrap();

        // Verify event has timestamp
        let created_at: String = storage
            .conn
            .query_row(
                "SELECT created_at FROM events WHERE issue_id = ?",
                ["bd-e1"],
                |row| row.get(0),
            )
            .unwrap();

        // Should be a valid RFC3339 timestamp
        assert!(
            chrono::DateTime::parse_from_rfc3339(&created_at).is_ok(),
            "Event timestamp should be valid RFC3339"
        );
    }

    #[test]
    fn test_blocked_cache_invalidation() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        // Create issues first (required for FK constraints on events table)
        let issue1 = make_issue(
            "bd-c1",
            "Cached issue",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        storage.create_issue(&issue1, "tester").unwrap();

        let issue2 = make_issue(
            "bd-b1",
            "Blocker issue",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        storage.create_issue(&issue2, "tester").unwrap();

        // Manually insert some cache data
        storage
            .conn
            .execute(
                "INSERT INTO blocked_issues_cache (issue_id, blocked_by) VALUES (?, ?)",
                ["bd-c1", r#"["bd-b1"]"#],
            )
            .unwrap();

        // Verify cache has data
        let count: i64 = storage
            .conn
            .query_row(
                "SELECT count(*) FROM blocked_issues_cache WHERE issue_id = ?",
                ["bd-c1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);

        // Now add a non-blocking dependency type ("related" doesn't block)
        storage
            .add_dependency("bd-c1", "bd-b1", "related", "tester")
            .unwrap();

        // Cache should be rebuilt - since "related" is not a blocking type,
        // bd-c1 should no longer be in the blocked cache (the manually
        // inserted entry gets cleared and not replaced)
        let count: i64 = storage
            .conn
            .query_row(
                "SELECT count(*) FROM blocked_issues_cache WHERE issue_id = ?",
                ["bd-c1"],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn test_update_issue_recomputes_hash() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let mut issue = make_issue(
            "bd-h1",
            "Old Title",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        issue.content_hash = Some(issue.compute_content_hash());
        storage.create_issue(&issue, "tester").unwrap();

        // Get initial hash
        let initial = storage.get_issue("bd-h1").unwrap().unwrap();
        let initial_hash = initial.content_hash.unwrap();

        // Update title
        let update = IssueUpdate {
            title: Some("New Title".to_string()),
            ..IssueUpdate::default()
        };
        storage.update_issue("bd-h1", &update, "tester").unwrap();

        // Check new hash
        let updated = storage.get_issue("bd-h1").unwrap().unwrap();
        let updated_hash = updated.content_hash.unwrap();

        assert_ne!(
            initial_hash, updated_hash,
            "Hash should change when title changes"
        );
    }

    #[test]
    fn test_delete_config() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        // Set a config value
        storage.set_config("test_key", "test_value").unwrap();
        assert_eq!(
            storage.get_config("test_key").unwrap(),
            Some("test_value".to_string())
        );

        // Delete it
        let deleted = storage.delete_config("test_key").unwrap();
        assert!(deleted, "Should return true when key existed");
        assert_eq!(storage.get_config("test_key").unwrap(), None);

        // Delete non-existent key
        let deleted_again = storage.delete_config("nonexistent").unwrap();
        assert!(!deleted_again, "Should return false when key doesn't exist");
    }

    #[test]
    fn test_open_creates_database() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("new_db.db");

        assert!(!db_path.exists(), "Database should not exist yet");

        let _storage = SqliteStorage::open(&db_path).unwrap();

        assert!(db_path.exists(), "Database file should be created");
    }

    #[test]
    fn test_pragmas_are_set_correctly() {
        let storage = SqliteStorage::open_memory().unwrap();

        // Check foreign keys are enabled
        let fk: i32 = storage
            .conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
            .unwrap();
        assert_eq!(fk, 1, "Foreign keys should be enabled");

        // Check journal mode (memory DBs use 'memory' mode)
        let mode: String = storage
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .unwrap();
        assert!(
            mode.to_lowercase() == "wal" || mode.to_lowercase() == "memory",
            "Journal mode should be WAL or memory"
        );
    }

    #[test]
    fn test_create_duplicate_id_fails() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap();

        let issue = make_issue("bd-dup-1", "First issue", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue, "tester").unwrap();

        // Try to create another issue with the same ID
        let dup = make_issue("bd-dup-1", "Duplicate", Status::Open, 2, None, t1, None);
        let result = storage.create_issue(&dup, "tester");

        assert!(result.is_err(), "Creating duplicate ID should fail");
    }

    #[test]
    fn test_get_issue_not_found_returns_none() {
        let storage = SqliteStorage::open_memory().unwrap();

        let result = storage.get_issue("nonexistent-id").unwrap();

        assert!(
            result.is_none(),
            "Getting non-existent issue should return None"
        );
    }

    #[test]
    fn test_open_nonexistent_parent_fails() {
        let result = SqliteStorage::open(Path::new("/nonexistent/path/to/db.db"));

        assert!(
            result.is_err(),
            "Opening DB in non-existent directory should fail"
        );
    }

    #[test]
    fn test_list_issues_empty_db() {
        let storage = SqliteStorage::open_memory().unwrap();
        let filters = ListFilters::default();

        let issues = storage.list_issues(&filters).unwrap();

        assert!(issues.is_empty(), "Empty DB should return no issues");
    }

    #[test]
    fn test_update_issue_not_found_fails() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        let update = IssueUpdate {
            title: Some("Updated title".to_string()),
            ..IssueUpdate::default()
        };

        let result = storage.update_issue("nonexistent-id", &update, "tester");

        assert!(result.is_err(), "Updating non-existent issue should fail");
    }

    #[test]
    fn test_list_issues_filter_by_title() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 8, 1, 0, 0, 0).unwrap();

        // Create issues with different titles
        let issue1 = make_issue(
            "bd-s1",
            "Fix authentication bug",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        let issue2 = make_issue(
            "bd-s2",
            "Add user registration",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        let issue3 = make_issue(
            "bd-s3",
            "Update documentation",
            Status::Open,
            2,
            None,
            t1,
            None,
        );

        storage.create_issue(&issue1, "tester").unwrap();
        storage.create_issue(&issue2, "tester").unwrap();
        storage.create_issue(&issue3, "tester").unwrap();

        // Filter by title containing "bug"
        let filters = ListFilters {
            title_contains: Some("bug".to_string()),
            ..ListFilters::default()
        };

        let issues = storage.list_issues(&filters).unwrap();

        assert_eq!(
            issues.len(),
            1,
            "Should find one issue matching 'bug' in title"
        );
        assert_eq!(issues[0].id, "bd-s1");
    }

    #[test]
    fn test_list_issues_reverse_default_sort() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 8, 1, 0, 0, 0).unwrap();
        let t2 = Utc.with_ymd_and_hms(2025, 8, 2, 0, 0, 0).unwrap();

        let issue_a = make_issue("bd-a", "A", Status::Open, 1, None, t1, None);
        let issue_b = make_issue("bd-b", "B", Status::Open, 1, None, t2, None);
        let issue_c = make_issue("bd-c", "C", Status::Open, 2, None, t1, None);

        storage.create_issue(&issue_a, "tester").unwrap();
        storage.create_issue(&issue_b, "tester").unwrap();
        storage.create_issue(&issue_c, "tester").unwrap();

        let filters = ListFilters {
            reverse: true,
            ..ListFilters::default()
        };

        let issues = storage.list_issues(&filters).unwrap();
        let ids: Vec<_> = issues.iter().map(|i| i.id.as_str()).collect();

        assert_eq!(ids, vec!["bd-c", "bd-a", "bd-b"]);
    }

    #[test]
    fn test_search_issues_full_text() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 9, 1, 0, 0, 0).unwrap();

        let issue1 = make_issue(
            "bd-s1",
            "Fix authentication bug",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        let issue2 = make_issue(
            "bd-s2",
            "Add user registration",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        let issue3 = make_issue(
            "bd-s3",
            "Update documentation",
            Status::Open,
            2,
            None,
            t1,
            None,
        );

        storage.create_issue(&issue1, "tester").unwrap();
        storage.create_issue(&issue2, "tester").unwrap();
        storage.create_issue(&issue3, "tester").unwrap();

        let filters = ListFilters::default();
        let results = storage.search_issues("authentication", &filters).unwrap();

        assert_eq!(
            results.len(),
            1,
            "Should find one issue matching 'authentication'"
        );
        assert_eq!(results[0].id, "bd-s1");
    }

    #[test]
    fn test_list_issues_filter_by_updated_date() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let now = Utc::now();
        let old = now - chrono::Duration::days(10);
        let older = now - chrono::Duration::days(20);

        let issue1 = make_issue("bd-old", "Old issue", Status::Open, 2, None, old, None);
        let issue2 = make_issue(
            "bd-older",
            "Older issue",
            Status::Open,
            2,
            None,
            older,
            None,
        );
        let issue3 = make_issue("bd-new", "New issue", Status::Open, 2, None, now, None);

        storage.create_issue(&issue1, "tester").unwrap();
        storage.create_issue(&issue2, "tester").unwrap();
        storage.create_issue(&issue3, "tester").unwrap();

        // Filter updated_before 'old' (inclusive? SQL uses <=)
        // If we use 'old', issue1 matches. issue2 matches. issue3 does not.
        let mut filters = ListFilters {
            updated_before: Some(old),
            ..Default::default()
        };

        let issues = storage.list_issues(&filters).unwrap();
        // Should contain bd-old and bd-older
        assert_eq!(issues.len(), 2);
        let ids: Vec<_> = issues.iter().map(|i| i.id.as_str()).collect();
        assert!(ids.contains(&"bd-old"));
        assert!(ids.contains(&"bd-older"));
        assert!(!ids.contains(&"bd-new"));

        // Filter updated_after 'old'
        filters.updated_before = None;
        filters.updated_after = Some(old);
        let issues = storage.list_issues(&filters).unwrap();
        // Should contain bd-old and bd-new
        assert_eq!(issues.len(), 2);
        let ids: Vec<_> = issues.iter().map(|i| i.id.as_str()).collect();
        assert!(ids.contains(&"bd-old"));
        assert!(ids.contains(&"bd-new"));
        assert!(!ids.contains(&"bd-older"));
    }

    #[test]
    fn test_list_issues_filter_by_labels() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc::now();

        let issue1 = make_issue("bd-l1", "Issue with label", Status::Open, 2, None, t1, None);
        let issue2 = make_issue(
            "bd-l2",
            "Issue without label",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&issue1, "tester").unwrap();
        storage.create_issue(&issue2, "tester").unwrap();

        // Add label to issue1
        storage.add_label("bd-l1", "test-label", "tester").unwrap();

        // Filter by label
        let filters = ListFilters {
            labels: Some(vec!["test-label".to_string()]),
            ..Default::default()
        };

        let issues = storage.list_issues(&filters).unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].id, "bd-l1");
    }

    #[test]
    fn test_blocked_cache_handles_quotes_in_ids() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc::now();

        let issue = make_issue("bd-x1", "Blocked", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue, "tester").unwrap();

        // Add a dependency on an ID containing a quote (e.g. from bad import)
        // This is valid in DB but tricky for manual JSON building.
        // Note: We use "orphan:" prefix instead of "external:" because external
        // dependencies are excluded from the blocked cache (resolved at runtime).
        let tricky_id = "orphan:foo\"bar";
        storage
            .add_dependency("bd-x1", tricky_id, "blocks", "tester")
            .unwrap();

        // Cache should be rebuilt and handle the quote correctly
        // (rebuild happens automatically on add_dependency via mutation context)

        // Verify we can read it back without error
        let blocked = storage.get_blocked_issues().unwrap();
        assert_eq!(blocked.len(), 1);
        assert_eq!(blocked[0].0.id, "bd-x1");

        let blockers = &blocked[0].1;
        assert_eq!(blockers.len(), 1);
        // ID + ":unknown" (since orphan doesn't have status in our DB)
        assert_eq!(blockers[0], "orphan:foo\"bar:unknown");
    }

    #[test]
    fn test_get_ready_issues_filters_by_labels() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc::now();

        let i1 = make_issue("bd-1", "A", Status::Open, 2, None, t1, None);
        let i2 = make_issue("bd-2", "B", Status::Open, 2, None, t1, None);
        let i3 = make_issue("bd-3", "C", Status::Open, 2, None, t1, None);

        storage.create_issue(&i1, "tester").unwrap();
        storage.create_issue(&i2, "tester").unwrap();
        storage.create_issue(&i3, "tester").unwrap();

        storage.add_label("bd-1", "backend", "tester").unwrap();
        storage.add_label("bd-1", "urgent", "tester").unwrap();
        storage.add_label("bd-2", "backend", "tester").unwrap();
        // bd-3 has no labels

        // Filter AND: backend + urgent
        let filters_and = ReadyFilters {
            labels_and: vec!["backend".to_string(), "urgent".to_string()],
            ..Default::default()
        };
        let res = storage
            .get_ready_issues(&filters_and, ReadySortPolicy::Oldest)
            .unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].id, "bd-1");

        // Filter OR: urgent
        let filters_or = ReadyFilters {
            labels_or: vec!["urgent".to_string()],
            ..Default::default()
        };
        let res = storage
            .get_ready_issues(&filters_or, ReadySortPolicy::Oldest)
            .unwrap();
        assert_eq!(res.len(), 1);
        assert_eq!(res[0].id, "bd-1");

        // Filter OR: backend (should get 1 and 2)
        let filters_or_backend = ReadyFilters {
            labels_or: vec!["backend".to_string()],
            ..Default::default()
        };
        let res = storage
            .get_ready_issues(&filters_or_backend, ReadySortPolicy::Oldest)
            .unwrap();
        assert_eq!(res.len(), 2);
    }
}
