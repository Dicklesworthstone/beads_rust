//! `SQLite` storage implementation.

use crate::error::{BeadsError, Result};
use crate::format::{IssueDetails, IssueWithDependencyMetadata};
use crate::model::{
    Comment, Dependency, DependencyType, Event, EventType, Issue, IssueType, Priority, Status,
};
use crate::storage::events::get_events;
use crate::storage::schema::CURRENT_SCHEMA_VERSION;
use crate::storage::schema::{
    apply_runtime_compatible_schema, apply_schema, execute_batch, runtime_schema_compatible,
    table_exists,
};
use crate::sync::{
    METADATA_JSONL_CONTENT_HASH, METADATA_JSONL_MTIME, METADATA_JSONL_SIZE,
    METADATA_LAST_EXPORT_TIME, METADATA_LAST_IMPORT_TIME,
};
use crate::util::id::{normalize_prefix, parse_id};
use crate::validation::{CommentValidator, ISSUE_LABEL_MAX_COUNT, IssueValidator, LabelValidator};
use chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use fsqlite::Connection;
use fsqlite::compat::{OpenFlags, open_with_flags};
use fsqlite_error::FrankenError;
use fsqlite_types::SqliteValue;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

/// Number of mutations between WAL checkpoint attempts.
const WAL_CHECKPOINT_INTERVAL: u32 = 50;
/// Per-statement busy spin timeout before SQLite returns SQLITE_BUSY.
///
/// Set to 0 (#243): frankensqlite's busy_timeout implementation uses a
/// hot spin loop (not sleep-based) that consumes 100% CPU and prevents
/// the competing writer from making progress. With busy_timeout=0,
/// `BEGIN IMMEDIATE` returns SQLITE_BUSY immediately when the write lock
/// is held, allowing the application-level retry loop (8 attempts with
/// jittered exponential backoff via thread::sleep) to provide proper
/// desynchronization. This is critical for multi-agent concurrent access.
const DEFAULT_BUSY_TIMEOUT_MS: u64 = 0;
const SQLITE_VAR_LIMIT: usize = 900;
const REDUNDANT_LABEL_COVERAGE_MIN_CANDIDATES: usize = 8_192;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ListRelationMetadata {
    pub(crate) labels: Vec<String>,
    pub(crate) dependency_count: usize,
    pub(crate) dependent_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ChangelogIssueRow {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) priority: Priority,
    pub(crate) issue_type: IssueType,
    pub(crate) created_at: DateTime<Utc>,
    pub(crate) closed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DependencyCycleReport {
    pub active_cycles: Vec<Vec<String>>,
    pub archived_closed_cycles: Vec<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BulkDependencyInsert {
    pub(crate) issue_id: String,
    pub(crate) depends_on_id: String,
    pub(crate) dep_type: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BlockedCacheProjectionHealth {
    pub(crate) parity_status: String,
    pub(crate) direct_blocked_rows: Option<usize>,
    pub(crate) cached_missing_rows: Option<usize>,
    pub(crate) cached_extra_rows: Option<usize>,
    pub(crate) cached_mismatched_rows: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReadyProjectionHealth {
    pub(crate) parity_status: String,
    pub(crate) cached_ready_rows: Option<usize>,
    pub(crate) direct_ready_rows: Option<usize>,
    pub(crate) cached_ready_missing_rows: Option<usize>,
    pub(crate) cached_ready_extra_rows: Option<usize>,
}

impl BlockedCacheProjectionHealth {
    fn unavailable(direct_blocked_rows: Option<usize>) -> Self {
        Self {
            parity_status: "unavailable".to_string(),
            direct_blocked_rows,
            cached_missing_rows: None,
            cached_extra_rows: None,
            cached_mismatched_rows: None,
        }
    }

    pub(crate) fn has_mismatch(&self) -> bool {
        [
            self.cached_missing_rows,
            self.cached_extra_rows,
            self.cached_mismatched_rows,
        ]
        .into_iter()
        .flatten()
        .any(|count| count > 0)
    }
}

impl ReadyProjectionHealth {
    fn unavailable(cached_ready_rows: Option<usize>, direct_ready_rows: Option<usize>) -> Self {
        Self {
            parity_status: "unavailable".to_string(),
            cached_ready_rows,
            direct_ready_rows,
            cached_ready_missing_rows: None,
            cached_ready_extra_rows: None,
        }
    }

    pub(crate) fn has_mismatch(&self) -> bool {
        [self.cached_ready_missing_rows, self.cached_ready_extra_rows]
            .into_iter()
            .flatten()
            .any(|count| count > 0)
    }
}

fn unique_label_refs(labels: &[String]) -> Vec<&String> {
    let mut unique_labels = Vec::with_capacity(labels.len());
    let mut seen_labels = HashSet::with_capacity(labels.len());
    for label in labels {
        if seen_labels.insert(label.as_str()) {
            unique_labels.push(label);
        }
    }
    unique_labels
}

fn append_label_membership_filters(
    sql: &mut String,
    params: &mut Vec<SqliteValue>,
    labels_and: &[String],
    labels_or: &[String],
) {
    let unique_labels_and = unique_label_refs(labels_and);

    match unique_labels_and.as_slice() {
        [] => {}
        [label] => {
            sql.push_str(" AND issues.id IN (SELECT issue_id FROM labels WHERE label = ?)");
            params.push(SqliteValue::from(label.as_str()));
        }
        _ => {
            let placeholders: Vec<String> =
                unique_labels_and.iter().map(|_| "?".to_string()).collect();
            let _ = write!(
                sql,
                " AND issues.id IN (
                    SELECT issue_id
                    FROM labels
                    WHERE label IN ({})
                    GROUP BY issue_id
                    HAVING COUNT(DISTINCT label) = ?
                )",
                placeholders.join(",")
            );
            for label in &unique_labels_and {
                params.push(SqliteValue::from(label.as_str()));
            }
            params.push(SqliteValue::from(
                i64::try_from(unique_labels_and.len()).unwrap_or(i64::MAX),
            ));
        }
    }

    if !labels_or.is_empty() {
        let placeholders: Vec<String> = labels_or.iter().map(|_| "?".to_string()).collect();
        let _ = write!(
            sql,
            " AND issues.id IN (SELECT issue_id FROM labels WHERE label IN ({}))",
            placeholders.join(",")
        );
        for label in labels_or {
            params.push(SqliteValue::from(label.as_str()));
        }
    }
}

fn append_issue_source_with_label_and_joins(
    sql: &mut String,
    params: &mut Vec<SqliteValue>,
    labels_and: &[String],
) {
    let unique_labels_and = unique_label_refs(labels_and);

    let Some((first_label, remaining_labels)) = unique_labels_and.split_first() else {
        sql.push_str(" FROM issues");
        return;
    };

    sql.push_str(
        " FROM labels AS labels_and_1
          JOIN issues
            ON issues.id = labels_and_1.issue_id
           AND labels_and_1.label = ?",
    );
    params.push(SqliteValue::from(first_label.as_str()));

    for (index, label) in remaining_labels.iter().enumerate() {
        let alias_number = index + 2;
        let _ = write!(
            sql,
            " JOIN labels AS labels_and_{alias_number}
                ON labels_and_{alias_number}.issue_id = issues.id
               AND labels_and_{alias_number}.label = ?"
        );
        params.push(SqliteValue::from(label.as_str()));
    }
}

fn append_issue_id_membership_filter(
    sql: &mut String,
    params: &mut Vec<SqliteValue>,
    issue_ids: &[String],
) {
    sql.push_str(" AND (");
    for (index, chunk) in issue_ids.chunks(SQLITE_VAR_LIMIT).enumerate() {
        if index > 0 {
            sql.push_str(" OR ");
        }
        let placeholders: Vec<String> = chunk.iter().map(|_| "?".to_string()).collect();
        let _ = write!(sql, "id IN ({})", placeholders.join(","));
        for issue_id in chunk {
            params.push(SqliteValue::from(issue_id.as_str()));
        }
    }
    sql.push(')');
}

fn append_label_or_membership_exists(
    sql: &mut String,
    params: &mut Vec<SqliteValue>,
    labels_or: &[String],
) {
    if labels_or.is_empty() {
        return;
    }

    let placeholders: Vec<String> = labels_or.iter().map(|_| "?".to_string()).collect();
    let _ = write!(
        sql,
        " AND EXISTS (
            SELECT 1
            FROM labels
            WHERE labels.issue_id = issues.id
              AND labels.label IN ({})
        )",
        placeholders.join(",")
    );
    for label in labels_or {
        params.push(SqliteValue::from(label.as_str()));
    }
}
// `fsqlite` starts returning false PRIMARY KEY conflicts when we rewrite
// existing `export_hashes` rows with a single multi-values INSERT. Batch the
// DELETE side for efficiency, but re-insert one row at a time for correctness.
const EXPORT_HASH_CHUNK_SIZE: usize = 32;
// `fsqlite` can surface the same false primary-key conflict when an existing
// blocked-cache population is rewritten via a large multi-values INSERT. Keep
// the delete batched/full-table, but re-insert rows individually.
const BLOCKED_CACHE_DELETE_CHUNK_SIZE: usize = 400;
const DIRTY_ISSUE_CHUNK_SIZE: usize = 900;
const BLOCKS_DEP_EDGE_FILTER_LIMIT: usize = 400;
const IMPORT_DEPENDENCY_CHUNK_SIZE: usize = 140;
const DEPENDENCY_TRAVERSAL_MAX_DEPTH: usize = 500;
const BLOCKED_CACHE_STATE_KEY: &str = "blocked_cache_state";
const BLOCKED_CACHE_STATE_STALE: &str = "stale";
/// Annotation suffix appended to a blocker ref when an epic is "blocked" purely
/// because it still has an open child (e.g. `bd-42:child-open`).
///
/// This is a **close-ordering** marker — an epic should not be *closed* while it
/// has open children — and must never prevent the epic from being *started*
/// (claimed / moved to `in_progress`). The producer queries embed this suffix in
/// `blocked_issues_cache`; [`SqliteStorage::get_start_blockers`] filters it back
/// out so claim/start guards ignore it (#315).
const CHILD_OPEN_BLOCKER_SUFFIX: &str = ":child-open";
/// Suffix marking a blocker ref produced by propagating an *already-blocked*
/// parent down onto its children (e.g. `bd-3n73:parent-blocked`).
///
/// This is an advisory **readiness** marker used to rank a child of a blocked
/// epic below truly-ready work in `br ready`. It is hierarchy, not a
/// prerequisite edge from the parent to the child, so it must never act as a
/// *hard* gate: a finished child must be *closable* (#355) and an actionable
/// child must be *claimable / startable* (#357) even while its parent epic is
/// blocked. The producer
/// ([`SqliteStorage::propagate_blocked_parents`]) embeds this suffix in
/// `blocked_issues_cache`; both [`SqliteStorage::get_close_blockers`] and
/// [`SqliteStorage::get_start_blockers`] filter it back out so neither the close
/// gate nor the start/claim gate treats it as a real blocker.
const PARENT_BLOCKED_SUFFIX: &str = ":parent-blocked";
const NEEDS_FLUSH_KEY: &str = "needs_flush";
const METADATA_EMPTY_VALUE: &str = "";
const METADATA_FALSE_VALUE: &str = "false";
const KNOWN_METADATA_DEFAULTS: [(&str, &str); 7] = [
    (BLOCKED_CACHE_STATE_KEY, METADATA_EMPTY_VALUE),
    (NEEDS_FLUSH_KEY, METADATA_FALSE_VALUE),
    (METADATA_JSONL_CONTENT_HASH, METADATA_EMPTY_VALUE),
    (METADATA_JSONL_MTIME, METADATA_EMPTY_VALUE),
    (METADATA_JSONL_SIZE, METADATA_EMPTY_VALUE),
    (METADATA_LAST_EXPORT_TIME, METADATA_EMPTY_VALUE),
    (METADATA_LAST_IMPORT_TIME, METADATA_EMPTY_VALUE),
];

/// SQLite-based storage backend.
#[derive(Debug)]
pub struct SqliteStorage {
    conn: Connection,
    /// Track mutations to trigger periodic WAL checkpoints.
    mutation_count: u32,
    /// When set, this storage owns an ephemeral on-disk temp database (created
    /// by [`SqliteStorage::open_memory`]) that must be unlinked — together with
    /// its WAL/SHM/journal sidecars — when the connection is dropped. FrankenSQLite
    /// requires real file paths for WAL and schema operations, so the "in-memory"
    /// path is backed by a real temp file rather than `:memory:`; without this
    /// cleanup the files accumulate in `TMPDIR` (#299). `None` for persistent
    /// databases, which must never be deleted on drop.
    temp_db_path: Option<PathBuf>,
    /// Tier 1 attribution to stamp onto the audit events of the NEXT mutation
    /// (issue #312, Layer 3 capture-only). Set via
    /// [`SqliteStorage::set_pending_event_attribution`] immediately before a
    /// `create`/`update` call so the command layer can attach self-reported
    /// agent identity without threading it through every storage signature.
    /// Consumed by exactly one COMMITTING `mutate()` (cleared only after the
    /// write transaction commits, so a JSONL-recovery retry can still stamp it),
    /// or cleared by any staged-mutation entry point that returns without
    /// committing (e.g. an empty-`updates` no-op). It therefore never leaks into
    /// an unrelated subsequent operation. Capture-only — never used for gating.
    pending_event_attribution: Option<EventAttribution>,
}

/// Context for a mutation operation, tracking side effects.
/// Tier 1 attribution captured on status-mutating commands (issue #312,
/// Layer 3 capture-only). Self-reported agent/harness/model identity that is
/// recorded onto emitted audit events as a trail ONLY — it is never gated or
/// enforced on. Empty/whitespace-only inputs are coerced to `None` so absent
/// attribution never produces blank-string noise in the audit log.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EventAttribution {
    pub agent_name: Option<String>,
    pub harness: Option<String>,
    pub model: Option<String>,
}

impl EventAttribution {
    /// Build attribution from raw CLI/env inputs, normalizing empty or
    /// whitespace-only values to `None`.
    #[must_use]
    pub fn new(agent_name: Option<&str>, harness: Option<&str>, model: Option<&str>) -> Self {
        let norm = |v: Option<&str>| {
            v.map(str::trim)
                .filter(|s| !s.is_empty())
                .map(ToString::to_string)
        };
        Self {
            agent_name: norm(agent_name),
            harness: norm(harness),
            model: norm(model),
        }
    }

    /// True when no attribution value was supplied.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.agent_name.is_none() && self.harness.is_none() && self.model.is_none()
    }
}

pub struct MutationContext {
    pub op_name: String,
    pub actor: String,
    /// Attribution stamped onto every event this context records (issue #312,
    /// Layer 3 capture-only). Defaults to empty; set per-command before the
    /// mutation runs. Capture-only — never used for gating.
    pub attribution: EventAttribution,
    pub events: Vec<Event>,
    pub dirty_ids: HashSet<String>,
    pub invalidate_blocked_cache: bool,
    /// When set, only these issue IDs (and their transitive parent-child
    /// descendants) need their blocked-cache entries recomputed.  If `None`
    /// while `invalidate_blocked_cache` is true, the entire cache is rebuilt.
    pub cache_affected_ids: Option<HashSet<String>>,
    /// When true, skip the storage-layer post-commit cache refresh.  Command
    /// callers that already hold `.write.lock` can finalize the cache once per
    /// batch; direct reads compute blocked state in-memory if the marker remains.
    pub defer_cache_refresh: bool,
    pub force_flush: bool,
}

#[derive(Debug, Clone)]
enum BlockedCacheRefreshPlan {
    Full,
    Incremental(HashSet<String>),
    /// The stale marker has been set inside the transaction; skip the
    /// storage-layer post-commit rebuild.  Higher-level command batching can
    /// refresh once after the batch, while direct reads compute in-memory if the
    /// marker remains.
    Deferred,
}

impl BlockedCacheRefreshPlan {
    fn from_context(ctx: &MutationContext) -> Option<Self> {
        if !ctx.invalidate_blocked_cache {
            return None;
        }

        if ctx.defer_cache_refresh {
            return Some(Self::Deferred);
        }

        match &ctx.cache_affected_ids {
            Some(ids) if !ids.is_empty() => Some(Self::Incremental(ids.clone())),
            _ => Some(Self::Full),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReadyIssueProjection {
    Full,
    Command,
    Summary,
}

#[derive(Clone, Copy)]
enum SearchIssueProjection {
    Full,
    CommandText,
}

#[derive(Clone, Copy)]
enum BlockedIssueProjection {
    Full,
    Command,
}

struct ReadyReadinessProbe {
    has_candidate_status: bool,
    blocked_cache_stale: bool,
}

struct IssueDetailRelationPresence {
    has_labels: bool,
    has_dependencies: bool,
    has_dependents: bool,
    has_comments: bool,
    parent: Option<String>,
}

struct ImportIssueTimestampStrings {
    created_at: String,
    updated_at: String,
    closed_at: Option<String>,
    due_at: Option<String>,
    defer_until: Option<String>,
    deleted_at: Option<String>,
    compacted_at: Option<String>,
}

impl ImportIssueTimestampStrings {
    fn from_issue(issue: &Issue) -> Self {
        Self {
            created_at: issue.created_at.to_rfc3339(),
            updated_at: issue.updated_at.to_rfc3339(),
            closed_at: issue.closed_at.map(|dt| dt.to_rfc3339()),
            due_at: issue.due_at.map(|dt| dt.to_rfc3339()),
            defer_until: issue.defer_until.map(|dt| dt.to_rfc3339()),
            deleted_at: issue.deleted_at.map(|dt| dt.to_rfc3339()),
            compacted_at: issue.compacted_at.map(|dt| dt.to_rfc3339()),
        }
    }
}

impl ReadyIssueProjection {
    const fn select_clause(self) -> &'static str {
        match self {
            Self::Full => {
                r"SELECT id, content_hash, title, description, design, acceptance_criteria, notes,
                         status, priority, issue_type, assignee, owner, estimated_minutes,
                         created_at, created_by, updated_at, closed_at, close_reason, closed_by_session,
                         due_at, defer_until, external_ref, source_system, source_repo,
                         deleted_at, deleted_by, delete_reason, original_type,
                         compaction_level, compacted_at, compacted_at_commit, original_size,
                         sender, ephemeral, pinned, is_template, source_repo_path, agent_context"
            }
            Self::Command => {
                r"SELECT id, title, description, acceptance_criteria, notes, status, priority,
                         issue_type, assignee, owner, estimated_minutes, created_at, created_by,
                         updated_at"
            }
            Self::Summary => {
                r"SELECT id, title, status, priority, issue_type, created_at, updated_at"
            }
        }
    }

    fn parse_row(self, row: &fsqlite::Row) -> Result<Issue> {
        match self {
            Self::Full => SqliteStorage::issue_from_row(row),
            Self::Command => SqliteStorage::ready_issue_from_row(row),
            Self::Summary => SqliteStorage::command_summary_issue_from_row(row),
        }
    }
}

impl SearchIssueProjection {
    const fn select_clause(self) -> &'static str {
        match self {
            Self::Full => {
                r"SELECT id, content_hash, title, description, design, acceptance_criteria, notes,
                         status, priority, issue_type, assignee, owner, estimated_minutes,
                         created_at, created_by, updated_at, closed_at, close_reason, closed_by_session,
                         due_at, defer_until, external_ref, source_system, source_repo,
                         deleted_at, deleted_by, delete_reason, original_type,
                         compaction_level, compacted_at, compacted_at_commit, original_size,
                         sender, ephemeral, pinned, is_template, source_repo_path, agent_context
                  FROM issues
                  WHERE 1=1"
            }
            Self::CommandText => {
                r"SELECT id, title, description, status, priority, issue_type, assignee,
                         created_at, updated_at
                  FROM issues
                  WHERE 1=1"
            }
        }
    }

    fn parse_issue(self, row: &fsqlite::Row) -> Result<Issue> {
        match self {
            Self::Full => SqliteStorage::issue_from_row(row),
            Self::CommandText => SqliteStorage::search_command_issue_from_row(row),
        }
    }
}

impl BlockedIssueProjection {
    const fn cached_select_clause(self) -> &'static str {
        match self {
            Self::Full => {
                r"SELECT i.id, i.content_hash, i.title, i.description, i.design, i.acceptance_criteria, i.notes,
                     i.status, i.priority, i.issue_type, i.assignee, i.owner, i.estimated_minutes,
                     i.created_at, i.created_by, i.updated_at, i.closed_at, i.close_reason, i.closed_by_session,
                     i.due_at, i.defer_until, i.external_ref, i.source_system, i.source_repo,
                     i.deleted_at, i.deleted_by, i.delete_reason, i.original_type, i.compaction_level,
                     i.compacted_at, i.compacted_at_commit, i.original_size, i.sender, i.ephemeral,
                     i.pinned, i.is_template, i.source_repo_path, i.agent_context,
                     bc.blocked_by"
            }
            Self::Command => {
                r"SELECT i.id, i.title, i.description, i.status, i.priority, i.issue_type,
                         i.created_at, i.created_by, i.updated_at, bc.blocked_by"
            }
        }
    }

    const fn map_select_clause(self) -> &'static str {
        match self {
            Self::Full => {
                r"SELECT id, content_hash, title, description, design, acceptance_criteria, notes,
                     status, priority, issue_type, assignee, owner, estimated_minutes,
                     created_at, created_by, updated_at, closed_at, close_reason, closed_by_session,
                     due_at, defer_until, external_ref, source_system, source_repo,
                     deleted_at, deleted_by, delete_reason, original_type, compaction_level,
                     compacted_at, compacted_at_commit, original_size, sender, ephemeral,
                     pinned, is_template, source_repo_path, agent_context"
            }
            Self::Command => {
                r"SELECT id, title, description, status, priority, issue_type,
                         created_at, created_by, updated_at"
            }
        }
    }

    const fn cached_blocked_by_index(self) -> usize {
        match self {
            // Bumped from 37 → 38 after `agent_context` was appended
            // to the Full SELECT at position 37 (beads_rust#297).
            // Source_repo_path is at 36, agent_context is at 37, so
            // bc.blocked_by lands at 38 in the joined projection.
            Self::Full => 38,
            Self::Command => 9,
        }
    }

    fn parse_issue(self, row: &fsqlite::Row) -> Result<Issue> {
        match self {
            Self::Full => SqliteStorage::issue_from_row(row),
            Self::Command => SqliteStorage::blocked_command_issue_from_row(row),
        }
    }
}

impl MutationContext {
    #[must_use]
    pub fn new(op_name: &str, actor: &str) -> Self {
        Self {
            op_name: op_name.to_string(),
            actor: actor.to_string(),
            attribution: EventAttribution::default(),
            events: Vec::new(),
            dirty_ids: HashSet::new(),
            invalidate_blocked_cache: false,
            cache_affected_ids: None,
            defer_cache_refresh: false,
            force_flush: false,
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
            agent_name: self.attribution.agent_name.clone(),
            harness: self.attribution.harness.clone(),
            model: self.attribution.model.clone(),
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
            agent_name: self.attribution.agent_name.clone(),
            harness: self.attribution.harness.clone(),
            model: self.attribution.model.clone(),
        });
    }

    pub fn mark_dirty(&mut self, issue_id: &str) {
        self.dirty_ids.insert(issue_id.to_string());
    }

    pub fn invalidate_cache(&mut self) {
        self.invalidate_blocked_cache = true;
        // Force full rebuild by clearing any incremental affected set.
        self.cache_affected_ids = None;
    }

    /// Signal that only specific issues need their blocked-cache entries
    /// recomputed (incremental update).  Merges with any previously recorded
    /// affected IDs.  If `invalidate_cache()` was already called (which sets
    /// `cache_affected_ids = None`), the full rebuild path takes precedence.
    pub fn invalidate_cache_for(&mut self, ids: &[&str]) {
        self.invalidate_blocked_cache = true;
        let set = self.cache_affected_ids.get_or_insert_with(HashSet::new);
        for id in ids {
            set.insert((*id).to_string());
        }
    }

    /// Mark the blocked-cache as needing invalidation but defer the storage-layer
    /// rebuild.  Higher-level command batching can refresh once after the batch;
    /// reads compute in-memory if the marker remains.
    ///
    /// Use this for high-frequency write operations (dep add/remove) where the
    /// caller can reconcile the cache at a command boundary instead of inside
    /// every individual storage mutation.
    pub fn invalidate_cache_deferred(&mut self) {
        self.invalidate_blocked_cache = true;
        self.defer_cache_refresh = true;
    }
}

impl SqliteStorage {
    fn with_connection_write_transaction<F, R>(conn: &Connection, mut f: F) -> Result<R>
    where
        F: FnMut(&Connection) -> Result<R>,
    {
        // Issue #219: same retry parameters as with_write_transaction (see
        // that method for rationale).  This static variant is used for
        // blocked-cache rebuilds and metadata writes which also contend for
        // the write lock under parallel agent operations.
        const MAX_RETRIES: u32 = 8;
        let base_backoff_ms: u64 = 50;
        let mut last_error: Option<crate::error::BeadsError> = None;

        for attempt in 0..MAX_RETRIES {
            match conn.execute("BEGIN IMMEDIATE") {
                Ok(_) => {}
                Err(e) if e.is_transient() && attempt < MAX_RETRIES - 1 => {
                    last_error = Some(e.into());
                    let backoff = Self::jittered_backoff(base_backoff_ms, attempt);
                    std::thread::sleep(Duration::from_millis(backoff));
                    continue;
                }
                Err(e) => return Err(e.into()),
            }

            match f(conn) {
                Ok(result) => match conn.execute("COMMIT") {
                    Ok(_) => return Ok(result),
                    Err(e) if e.is_transient() && attempt < MAX_RETRIES - 1 => {
                        if let Err(rb_err) = conn.execute("ROLLBACK") {
                            tracing::warn!(
                                error = %rb_err,
                                "ROLLBACK failed after transient COMMIT error"
                            );
                        }
                        last_error = Some(e.into());
                        let backoff = Self::jittered_backoff(base_backoff_ms, attempt);
                        std::thread::sleep(Duration::from_millis(backoff));
                    }
                    Err(e) => {
                        if let Err(rb_err) = conn.execute("ROLLBACK") {
                            tracing::warn!(error = %rb_err, "ROLLBACK failed after COMMIT error");
                        }
                        return Err(e.into());
                    }
                },
                Err(e) => {
                    if let Err(rb_err) = conn.execute("ROLLBACK") {
                        tracing::warn!(error = %rb_err, "ROLLBACK failed after transaction error");
                    }
                    if e.is_transient() && attempt < MAX_RETRIES - 1 {
                        last_error = Some(e);
                        let backoff = Self::jittered_backoff(base_backoff_ms, attempt);
                        std::thread::sleep(Duration::from_millis(backoff));
                    } else {
                        return Err(e);
                    }
                }
            }
        }

        Err(last_error.unwrap_or_else(|| {
            crate::error::BeadsError::Config(
                "connection write transaction retry loop exhausted without producing an error"
                    .into(),
            )
        }))
    }

    fn metadata_key_exists(conn: &Connection, key: &str) -> Result<bool> {
        let rows = conn.query_with_params(
            "SELECT 1 FROM metadata WHERE key = ? LIMIT 1",
            &[SqliteValue::from(key)],
        )?;
        Ok(!rows.is_empty())
    }

    fn upsert_metadata_key_in_tx(conn: &Connection, key: &str, value: &str) -> Result<()> {
        let updated = conn.execute_with_params(
            "UPDATE metadata SET value = ? WHERE key = ? AND value != ?",
            &[
                SqliteValue::from(value),
                SqliteValue::from(key),
                SqliteValue::from(value),
            ],
        )?;
        if updated == 0 && !Self::metadata_key_exists(conn, key)? {
            conn.execute_with_params(
                "INSERT INTO metadata (key, value) VALUES (?, ?)",
                &[SqliteValue::from(key), SqliteValue::from(value)],
            )?;
        }
        Ok(())
    }

    fn insert_metadata_default_if_missing(
        conn: &Connection,
        key: &str,
        default_value: &str,
    ) -> Result<()> {
        conn.execute_with_params(
            "INSERT INTO metadata (key, value)
             SELECT ?, ?
             WHERE NOT EXISTS (
                 SELECT 1 FROM metadata WHERE key = ? LIMIT 1
             )",
            &[
                SqliteValue::from(key),
                SqliteValue::from(default_value),
                SqliteValue::from(key),
            ],
        )?;
        Ok(())
    }

    fn ensure_known_metadata_defaults(conn: &Connection) -> Result<()> {
        // Read-first, write-only-if-missing pattern (#243). The read
        // (`metadata_key_exists`) needs no write lock, so ordinary opens do
        // not contend with active writers. If a key is missing, the INSERT
        // re-checks existence inside the statement because `metadata.key` is
        // intentionally not unique; `INSERT OR IGNORE` would not protect
        // against duplicate default rows.
        for (key, default_value) in KNOWN_METADATA_DEFAULTS {
            if Self::metadata_key_exists(conn, key)? {
                continue;
            }
            match Self::insert_metadata_default_if_missing(conn, key, default_value) {
                Ok(()) => {}
                Err(e) if e.is_transient() => {
                    // BUSY — another writer is active. The default will be
                    // present once their transaction commits, or we'll seed
                    // it on the next open.
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn metadata_equals(conn: &Connection, key: &str, expected: &str) -> Result<bool> {
        match conn.query_row_with_params(
            "SELECT value FROM metadata WHERE key = ? ORDER BY rowid DESC LIMIT 1",
            &[SqliteValue::from(key)],
        ) {
            Ok(row) => Ok(row.get(0).and_then(SqliteValue::as_text) == Some(expected)),
            Err(fsqlite_error::FrankenError::QueryReturnedNoRows) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    fn ready_readiness_probe(&self, include_deferred: bool) -> Result<ReadyReadinessProbe> {
        let sql = if include_deferred {
            "SELECT
                EXISTS(SELECT 1 FROM issues WHERE status IN ('open', 'deferred') LIMIT 1),
                COALESCE((SELECT value = ? FROM metadata WHERE key = ? ORDER BY rowid DESC LIMIT 1), 0)"
        } else {
            "SELECT
                EXISTS(SELECT 1 FROM issues WHERE status = 'open' LIMIT 1),
                COALESCE((SELECT value = ? FROM metadata WHERE key = ? ORDER BY rowid DESC LIMIT 1), 0)"
        };
        let row = self.conn.query_row_with_params(
            sql,
            &[
                SqliteValue::from(BLOCKED_CACHE_STATE_STALE),
                SqliteValue::from(BLOCKED_CACHE_STATE_KEY),
            ],
        )?;

        Ok(ReadyReadinessProbe {
            has_candidate_status: row
                .get(0)
                .and_then(SqliteValue::as_integer)
                .is_some_and(|value| value != 0),
            blocked_cache_stale: row
                .get(1)
                .and_then(SqliteValue::as_integer)
                .is_some_and(|value| value != 0),
        })
    }

    fn apply_blocked_cache_refresh_plan(
        conn: &Connection,
        plan: &BlockedCacheRefreshPlan,
    ) -> Result<usize> {
        match plan {
            BlockedCacheRefreshPlan::Full => Self::rebuild_blocked_cache_impl(conn),
            BlockedCacheRefreshPlan::Incremental(ids) => {
                Self::incremental_blocked_cache_update(conn, ids)
            }
            // Deferred plan is never applied eagerly; the stale marker already
            // set inside the write transaction signals reads to compute in-memory.
            BlockedCacheRefreshPlan::Deferred => Ok(0),
        }
    }

    fn foreign_keys_enabled(conn: &Connection) -> Result<bool> {
        let row = conn.query_row("PRAGMA foreign_keys")?;
        Ok(row.get(0).and_then(SqliteValue::as_integer).unwrap_or(0) == 1)
    }

    fn restore_foreign_keys(conn: &Connection, operation: &str) -> Result<()> {
        conn.execute("PRAGMA foreign_keys = ON")
            .map_err(BeadsError::Database)?;

        if Self::foreign_keys_enabled(conn)? {
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
        match (result, Self::restore_foreign_keys(conn, operation)) {
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

    fn refresh_blocked_cache_after_commit(
        &self,
        op: &str,
        plan: &BlockedCacheRefreshPlan,
    ) -> Result<()> {
        // Disable FK enforcement before the transaction.  PRAGMA foreign_keys
        // can only be changed outside an active transaction.  fsqlite can
        // surface false FK violations on blocked_issues_cache inserts (#215).
        self.conn.execute("PRAGMA foreign_keys = OFF")?;
        let result = Self::with_connection_write_transaction(&self.conn, |conn| {
            let refreshed = Self::apply_blocked_cache_refresh_plan(conn, plan)?;
            Self::upsert_metadata_key_in_tx(conn, BLOCKED_CACHE_STATE_KEY, METADATA_EMPTY_VALUE)?;
            tracing::debug!(operation = op, refreshed, "Refreshed blocked issues cache");
            Ok(())
        });
        Self::finish_foreign_key_suppressed_result(&self.conn, "blocked-cache refresh", result)
    }

    fn handle_blocked_cache_refresh_error(&self, op: &str, error: BeadsError) -> Result<()> {
        match Self::foreign_keys_enabled(&self.conn) {
            Ok(true) => {
                tracing::warn!(
                    operation = op,
                    error = %error,
                    "Blocked cache refresh deferred after commit; cache remains marked stale"
                );
                Ok(())
            }
            Ok(false) => Err(BeadsError::WithContext {
                context: format!(
                    "post-commit blocked-cache refresh for {op} failed and SQLite foreign key enforcement is OFF"
                ),
                source: Box::new(error),
            }),
            Err(check_error) => Err(BeadsError::WithContext {
                context: format!(
                    "post-commit blocked-cache refresh for {op} failed and SQLite foreign key enforcement status could not be verified: {check_error}"
                ),
                source: Box::new(error),
            }),
        }
    }

    pub(crate) fn blocked_cache_marked_stale(&self) -> Result<bool> {
        Self::metadata_equals(
            &self.conn,
            BLOCKED_CACHE_STATE_KEY,
            BLOCKED_CACHE_STATE_STALE,
        )
    }

    /// Mark the blocked-cache as stale so a future read can rebuild it on demand.
    ///
    /// # Errors
    ///
    /// Returns an error if the metadata update fails.
    pub(crate) fn mark_blocked_cache_stale(&mut self) -> Result<()> {
        self.set_metadata(BLOCKED_CACHE_STATE_KEY, BLOCKED_CACHE_STATE_STALE)
    }

    pub(crate) fn ensure_blocked_cache_fresh(&self) -> Result<bool> {
        if !self.blocked_cache_marked_stale()? {
            return Ok(false);
        }

        // Disable FK enforcement before the transaction.  PRAGMA foreign_keys
        // can only be changed outside an active transaction.  fsqlite can
        // surface false FK violations on blocked_issues_cache inserts (#215).
        self.conn.execute("PRAGMA foreign_keys = OFF")?;
        let result = Self::with_connection_write_transaction(&self.conn, |conn| {
            if !Self::metadata_equals(conn, BLOCKED_CACHE_STATE_KEY, BLOCKED_CACHE_STATE_STALE)? {
                return Ok(false);
            }

            let refreshed = Self::rebuild_blocked_cache_impl(conn)?;
            Self::upsert_metadata_key_in_tx(conn, BLOCKED_CACHE_STATE_KEY, METADATA_EMPTY_VALUE)?;
            tracing::debug!(refreshed, "Rebuilt stale blocked issues cache on demand");
            Ok(true)
        });
        Self::finish_foreign_key_suppressed_result(&self.conn, "blocked-cache lazy rebuild", result)
    }

    /// Open a new connection to the database at the given path.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection cannot be established or schema application fails.
    pub fn open(path: &Path) -> Result<Self> {
        Self::open_with_timeout(path, Some(DEFAULT_BUSY_TIMEOUT_MS))
    }

    /// Open a new connection with an optional busy timeout (ms).
    ///
    /// # Errors
    ///
    /// Returns an error if the connection cannot be established or schema application fails.
    pub fn open_with_timeout(path: &Path, lock_timeout_ms: Option<u64>) -> Result<Self> {
        let conn = Connection::open(path.to_string_lossy().into_owned())?;

        // Set busy_timeout. Default is 0 (#243) — frankensqlite's busy
        // handler hot-spins, so we rely on application-level retry (see
        // `with_write_transaction`). The `.write.lock` flock serializes
        // concurrent mutating processes before they reach this point.
        if let Some(timeout_ms) = lock_timeout_ms {
            conn.execute(&format!("PRAGMA busy_timeout={timeout_ms}"))?;
        }

        let schema_current = connection_user_version(&conn)
            .or_else(|| database_header_user_version(path))
            .is_some_and(|version| version >= u32::try_from(CURRENT_SCHEMA_VERSION).unwrap_or(0));
        let runtime_compatible = runtime_schema_compatible(&conn);

        if schema_current && runtime_compatible {
            crate::storage::schema::apply_runtime_pragmas(&conn)?;
        } else if runtime_compatible {
            apply_runtime_compatible_schema(&conn)?;
        } else {
            apply_schema(&conn)?;
        }
        Self::ensure_known_metadata_defaults(&conn)?;
        Ok(Self {
            conn,
            mutation_count: 0,
            temp_db_path: None,
            pending_event_attribution: None,
        })
    }

    pub(crate) fn open_current_read_only(path: &Path) -> Result<Option<Self>> {
        let current_schema_version = u32::try_from(CURRENT_SCHEMA_VERSION).unwrap_or(0);
        // Cheap header pre-filter: only reject when the file is definitively not
        // a SQLite database (no valid header magic). A low header *version* must
        // NOT disqualify — an uncheckpointed WAL from another process may hold
        // the true, current value that the raw header bytes do not yet reflect
        // (issue #373). A real database always carries the header magic even
        // when its user_version lives only in the WAL.
        if database_header_user_version(path).is_none() {
            return Ok(None);
        }

        let conn = open_with_flags(
            path.to_string_lossy().as_ref(),
            OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        // Now that the connection is open, consult the effective schema version
        // (WAL-aware) and fall back to the header peek. If it is still below the
        // current version, this database is genuinely stale.
        if connection_user_version(&conn)
            .or_else(|| database_header_user_version(path))
            .is_none_or(|version| version < current_schema_version)
        {
            return Ok(None);
        }
        Ok(Some(Self {
            conn,
            mutation_count: 0,
            temp_db_path: None,
            pending_event_attribution: None,
        }))
    }

    /// Open an ephemeral, single-process scratch database.
    ///
    /// FrankenSQLite requires real file paths for WAL and schema operations, so
    /// this cannot use `:memory:`; instead it opens a uniquely-named temp file
    /// (`beads_mem_<pid>_<count>.db`) under [`std::env::temp_dir`]. The file —
    /// and any `-wal`/`-shm`/`-journal` sidecars SQLite creates alongside it —
    /// is unlinked when the returned [`SqliteStorage`] is dropped (#299), and
    /// also if construction fails partway through, so no stale files are left in
    /// `TMPDIR`. The path is unique per process, so it is never shared with
    /// another process and is always safe to delete on teardown.
    ///
    /// # Errors
    ///
    /// Returns an error if the connection cannot be established.
    pub fn open_memory() -> Result<Self> {
        static MEM_COUNTER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let count = MEM_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("beads_mem_{}_{}.db", std::process::id(), count));

        // Build the storage on a fallible path so that any failure after the
        // file has been created still removes it (the partial `Connection` /
        // file would otherwise be orphaned without a live `Drop` to clean it).
        match Self::build_memory(&path) {
            Ok(storage) => Ok(storage),
            Err(e) => {
                remove_temp_db_files(&path);
                Err(e)
            }
        }
    }

    fn build_memory(path: &Path) -> Result<Self> {
        let conn = Connection::open(path.to_string_lossy().into_owned())?;

        conn.execute(&format!("PRAGMA busy_timeout={DEFAULT_BUSY_TIMEOUT_MS}"))?;
        if let Err(e) = apply_schema(&conn) {
            eprintln!("apply_schema failed: {:?}", e);
            return Err(e);
        }
        Self::ensure_known_metadata_defaults(&conn)?;
        Ok(Self {
            conn,
            mutation_count: 0,
            temp_db_path: Some(path.to_path_buf()),
            pending_event_attribution: None,
        })
    }

    /// Drop and recreate all data tables, preserving `config` and `metadata`.
    ///
    /// Used before force imports to avoid fsqlite btree cursor bugs on DELETE
    /// operations in large tables. By starting with empty tables, the import
    /// only performs INSERTs.
    ///
    /// # Errors
    ///
    /// Returns an error if any DROP/CREATE statement fails.
    pub fn reset_data_tables(&mut self) -> Result<()> {
        use crate::storage::schema::execute_batch;
        execute_batch(
            &self.conn,
            r"
            DROP TABLE IF EXISTS blocked_issues_cache;
            DROP TABLE IF EXISTS export_hashes;
            DROP TABLE IF EXISTS dirty_issues;
            DROP TABLE IF EXISTS child_counters;
            DROP TABLE IF EXISTS events;
            DROP TABLE IF EXISTS comments;
            DROP TABLE IF EXISTS labels;
            DROP TABLE IF EXISTS dependencies;
            DROP TABLE IF EXISTS issues;
            ",
        )?;
        // Recreate with full schema (config/metadata already exist, IF NOT EXISTS is safe).
        // Use apply_runtime_compatible_schema rather than apply_schema because we are
        // mid-session: the connection is already open with correct pragmas and we only
        // need to restore the DDL without re-running heavier first-open migrations.
        apply_runtime_compatible_schema(&self.conn)?;
        Ok(())
    }

    /// Detect recoverable on-disk anomalies that should trigger JSONL rebuild.
    ///
    /// These checks run after the database opens successfully because some
    /// malformed states remain queryable enough to reach startup, then fail on
    /// the next single-row lookup.
    ///
    /// Detect structured anomalies suitable for the canonical health classifier.
    ///
    /// # Errors
    ///
    /// Returns an error if probing the database fails.
    pub fn detect_anomalies(&self) -> Result<Vec<crate::health::AnomalyClass>> {
        use crate::health::AnomalyClass;
        let mut anomalies = Vec::new();

        let duplicate_schema_rows = self.conn.query(
            "SELECT type, name, COUNT(*) AS row_count
             FROM sqlite_master
             WHERE name IN ('blocked_issues_cache', 'idx_blocked_cache_blocked_at')
             GROUP BY type, name
             HAVING COUNT(*) > 1
             ORDER BY row_count DESC, name ASC",
        )?;
        for row in &duplicate_schema_rows {
            let name = row
                .get(1)
                .and_then(SqliteValue::as_text)
                .unwrap_or("unknown")
                .to_string();
            let count = row.get(2).and_then(SqliteValue::as_integer).unwrap_or(2);
            anomalies.push(AnomalyClass::DuplicateSchemaRows { name, count });
        }

        if let Some((key, count)) = self.first_duplicate_kv_key("config")? {
            anomalies.push(AnomalyClass::DuplicateConfigKeys { key, count });
        }

        if let Some((key, count)) = self.first_duplicate_kv_key("metadata")? {
            anomalies.push(AnomalyClass::DuplicateMetadataKeys { key, count });
        }

        Ok(anomalies)
    }

    /// # Errors
    ///
    /// Returns an error if probing the database fails.
    pub(crate) fn detect_recoverable_open_anomaly(&self) -> Result<Option<String>> {
        let duplicate_schema_rows = self.conn.query(
            "SELECT type, name, COUNT(*) AS row_count
             FROM sqlite_master
             WHERE name IN ('blocked_issues_cache', 'idx_blocked_cache_blocked_at')
             GROUP BY type, name
             HAVING COUNT(*) > 1
             ORDER BY row_count DESC, name ASC
             LIMIT 1",
        )?;
        if let Some(row) = duplicate_schema_rows.first() {
            let object_type = row
                .get(0)
                .and_then(SqliteValue::as_text)
                .unwrap_or("object");
            let name = row
                .get(1)
                .and_then(SqliteValue::as_text)
                .unwrap_or("unknown");
            let row_count = row.get(2).and_then(SqliteValue::as_integer).unwrap_or(2);
            return Ok(Some(format!(
                "sqlite_master contains duplicate {object_type} entries for '{name}' ({row_count} rows)"
            )));
        }

        if let Some((key, row_count)) = self.first_duplicate_kv_key("config")? {
            return Ok(Some(format!(
                "config contains duplicate rows for key '{key}' ({row_count} rows)"
            )));
        }

        if let Some((key, row_count)) = self.first_duplicate_kv_key("metadata")? {
            return Ok(Some(format!(
                "metadata contains duplicate rows for key '{key}' ({row_count} rows)"
            )));
        }

        Ok(None)
    }

    fn first_duplicate_kv_key(&self, table: &str) -> Result<Option<(String, i64)>> {
        let sql = format!(
            "SELECT key, COUNT(*) AS row_count
             FROM {table}
             GROUP BY key
             HAVING COUNT(*) > 1
             ORDER BY row_count DESC, key ASC
             LIMIT 1"
        );
        let rows = self.conn.query(&sql)?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };

        let key = row
            .get(0)
            .and_then(SqliteValue::as_text)
            .unwrap_or("")
            .to_string();
        let row_count = row.get(1).and_then(SqliteValue::as_integer).unwrap_or(2);
        Ok(Some((key, row_count)))
    }

    /// Execute a raw SQL statement (no parameters, no result).
    ///
    /// Useful for PRAGMAs and DDL that don't fit the normal mutation flow.
    ///
    /// # Errors
    ///
    /// Returns an error if the statement fails.
    pub(crate) fn execute_raw(&self, sql: &str) -> Result<()> {
        self.conn.execute(sql)?;
        Ok(())
    }

    /// Execute a raw SQL query and return all result rows.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub(crate) fn execute_raw_query(&self, sql: &str) -> Result<Vec<Vec<SqliteValue>>> {
        let rows = self.conn.query(sql)?;
        Ok(rows.iter().map(|r| r.values().to_vec()).collect())
    }

    /// Check whether a foreign-key-backed table contains rows whose reference
    /// column points at a missing issue.
    ///
    /// Only whitelisted table/column pairs are accepted to prevent SQL injection
    /// through the string-interpolated query.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails or the table/column pair is not
    /// whitelisted.
    pub(crate) fn has_missing_issue_reference(&self, table: &str, column: &str) -> Result<bool> {
        const ALLOWED_PAIRS: &[(&str, &str)] = &[
            ("dependencies", "issue_id"),
            ("dependencies", "depends_on_id"),
            ("labels", "issue_id"),
            ("comments", "issue_id"),
            ("events", "issue_id"),
            ("dirty_issues", "issue_id"),
            ("export_hashes", "issue_id"),
            ("blocked_issues_cache", "issue_id"),
            ("child_counters", "parent_id"),
        ];
        if !ALLOWED_PAIRS.contains(&(table, column)) {
            return Err(crate::error::BeadsError::Config(format!(
                "has_missing_issue_reference: disallowed table/column pair ({table}, {column})"
            )));
        }
        let external_dependency_filter = match (table, column) {
            ("dependencies", "issue_id") => " AND issue_id NOT LIKE 'external:%'",
            ("dependencies", "depends_on_id") => " AND depends_on_id NOT LIKE 'external:%'",
            _ => "",
        };
        let row = self.conn.query_row(&format!(
            "SELECT COUNT(*) FROM {table} WHERE {column} NOT IN (SELECT id FROM issues){external_dependency_filter}"
        ))?;
        Ok(row.get(0).and_then(SqliteValue::as_integer).unwrap_or(0) > 0)
    }

    /// Return FK-like issue references that point at missing local issues.
    ///
    /// External dependency endpoints are intentionally allowed because the
    /// schema supports cross-project blockers and external parent-child rows
    /// through `external:*` IDs.
    ///
    /// # Errors
    ///
    /// Returns an error if any invariant query fails.
    pub(crate) fn missing_issue_references(&self) -> Result<Vec<String>> {
        const ISSUE_REFERENCE_PAIRS: &[(&str, &str)] = &[
            ("dependencies", "issue_id"),
            ("dependencies", "depends_on_id"),
            ("labels", "issue_id"),
            ("comments", "issue_id"),
            ("events", "issue_id"),
            ("dirty_issues", "issue_id"),
            ("export_hashes", "issue_id"),
            ("blocked_issues_cache", "issue_id"),
            ("child_counters", "parent_id"),
        ];

        let mut violations = Vec::new();
        for (table, column) in ISSUE_REFERENCE_PAIRS {
            if self.has_missing_issue_reference(table, column)? {
                violations.push(format!("{table}.{column}"));
            }
        }

        Ok(violations)
    }

    /// Execute a raw SQL statement and return the number of affected rows.
    ///
    /// # Errors
    ///
    /// Returns an error if the statement fails.
    pub(crate) fn execute_raw_count(&self, sql: &str) -> Result<usize> {
        let rows = self.conn.execute(sql)?;
        Ok(rows)
    }

    /// Probe whether a rollback-only write against an issue can safely touch
    /// the scheduling/status indexes used by update-style mutations.
    ///
    /// This is used to distinguish a genuine on-disk corruption problem from a
    /// higher-level application error after a write fails.
    ///
    /// # Errors
    ///
    /// Returns any database error raised while executing the probe.
    pub(crate) fn probe_issue_mutation_write_path(&self, issue_id: &str) -> Result<()> {
        self.conn.execute("BEGIN IMMEDIATE")?;

        let probe_result = self.conn.execute_with_params(
            "UPDATE issues SET priority = priority, status = status WHERE id = ?",
            &[SqliteValue::from(issue_id)],
        );
        let rollback_result = self.conn.execute("ROLLBACK");

        finish_issue_mutation_write_probe(probe_result, rollback_result)
    }

    /// Execute a closure inside a write transaction with robust retry logic
    /// for lock contention.
    ///
    /// Retries on all transient BUSY errors (from BEGIN, DML, or COMMIT) with
    /// exponential backoff.
    ///
    /// # Errors
    ///
    /// Returns an error if any step fails (e.g. database error, logic error).
    /// The transaction is rolled back on error.
    pub(crate) fn with_write_transaction<F, R>(&mut self, mut f: F) -> Result<R>
    where
        F: FnMut(&mut Self) -> Result<R>,
    {
        // Issue #219/#243: parallel agents caused "database is busy" errors
        // or deadlocks.  With busy_timeout=0 (see DEFAULT_BUSY_TIMEOUT_MS),
        // BEGIN IMMEDIATE returns SQLITE_BUSY immediately, and all retry
        // timing happens here via thread::sleep with jittered exponential
        // backoff. 8 retries × (50ms → 6400ms) gives ~12.7s total wait with
        // good desynchronization under concurrent access.
        const MAX_RETRIES: u32 = 8;
        let base_backoff_ms: u64 = 50;
        let mut last_error: Option<crate::error::BeadsError> = None;

        for attempt in 0..MAX_RETRIES {
            match self.conn.execute("BEGIN IMMEDIATE") {
                Ok(_) => {}
                Err(e) if e.is_transient() && attempt < MAX_RETRIES - 1 => {
                    last_error = Some(e.into());
                    let backoff = Self::jittered_backoff(base_backoff_ms, attempt);
                    std::thread::sleep(Duration::from_millis(backoff));
                    continue;
                }
                Err(e) => return Err(e.into()),
            }

            match f(self) {
                Ok(result) => {
                    match self.conn.execute("COMMIT") {
                        Ok(_) => {
                            // Periodic WAL checkpoint to prevent unbounded WAL growth.
                            // Uses PASSIVE mode so it never blocks concurrent readers
                            // or writers (issue #219).
                            self.mutation_count += 1;
                            if self.mutation_count >= WAL_CHECKPOINT_INTERVAL {
                                self.mutation_count = 0;
                                self.try_wal_checkpoint();
                            }
                            return Ok(result);
                        }
                        Err(e) if e.is_transient() && attempt < MAX_RETRIES - 1 => {
                            if let Err(rb_err) = self.conn.execute("ROLLBACK") {
                                tracing::warn!(error = %rb_err, "ROLLBACK failed after transient COMMIT error");
                            }
                            last_error = Some(e.into());
                            let backoff = Self::jittered_backoff(base_backoff_ms, attempt);
                            std::thread::sleep(Duration::from_millis(backoff));
                            // retry
                        }
                        Err(e) => {
                            if let Err(rb_err) = self.conn.execute("ROLLBACK") {
                                tracing::warn!(error = %rb_err, "ROLLBACK failed after COMMIT error");
                            }
                            return Err(e.into());
                        }
                    }
                }
                Err(e) => {
                    if let Err(rb_err) = self.conn.execute("ROLLBACK") {
                        tracing::warn!(error = %rb_err, "ROLLBACK failed after transaction error");
                    }
                    if e.is_transient() && attempt < MAX_RETRIES - 1 {
                        last_error = Some(e);
                        let backoff = Self::jittered_backoff(base_backoff_ms, attempt);
                        std::thread::sleep(Duration::from_millis(backoff));
                        // retry
                    } else {
                        return Err(e);
                    }
                }
            }
        }
        Err(last_error.unwrap_or_else(|| {
            crate::error::BeadsError::Config(
                "write transaction retry loop exhausted without producing an error".into(),
            )
        }))
    }

    /// Compute exponential backoff with random jitter (+/-25%) to prevent
    /// thundering-herd synchronization across concurrent agents.
    fn jittered_backoff(base_ms: u64, attempt: u32) -> u64 {
        let deterministic = base_ms * 2u64.pow(attempt);
        // Add +-25% jitter using a cheap PRNG seeded from the current time.
        // No need for cryptographic randomness here.
        let nanos = u64::from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos(),
        );
        let jitter_range = deterministic / 4;
        if jitter_range == 0 {
            return deterministic;
        }
        // Map nanos into [-jitter_range, +jitter_range) using i128 to avoid
        // truncation on the u64→i64 boundary.
        let raw = i128::from(nanos % (jitter_range * 2)) - i128::from(jitter_range);
        let result = i128::from(deterministic) + raw;
        u64::try_from(result.max(1)).unwrap_or(u64::MAX)
    }

    /// Set export hashes using the caller's active transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub(crate) fn set_export_hashes_in_tx(&self, exports: &[(String, String)]) -> Result<usize> {
        let unique_exports = Self::dedupe_export_hash_batch(exports);
        if unique_exports.is_empty() {
            return Ok(0);
        }

        let now = Utc::now().to_rfc3339();
        let mut count = 0;

        for chunk in unique_exports.chunks(EXPORT_HASH_CHUNK_SIZE) {
            // Delete existing entries row-by-row to avoid fsqlite IN-clause bugs
            for (id, _) in chunk {
                self.conn.execute_with_params(
                    "DELETE FROM export_hashes WHERE issue_id = ?",
                    &[SqliteValue::from(id.as_str())],
                )?;
            }

            // `fsqlite` can report a false primary-key conflict when many
            // existing rows are reinserted via one VALUES list, so keep each
            // insert isolated after the chunk delete.
            for (issue_id, content_hash) in chunk {
                self.conn.execute_with_params(
                    "INSERT INTO export_hashes (issue_id, content_hash, exported_at) VALUES (?, ?, ?)",
                    &[
                        SqliteValue::from(issue_id.as_str()),
                        SqliteValue::from(content_hash.as_str()),
                        SqliteValue::from(now.as_str()),
                    ],
                )?;
                count += 1;
            }
        }

        Ok(count)
    }

    /// Insert export hashes after the caller has already cleared the table.
    ///
    /// Import starts by deleting all export hashes, so it does not need the
    /// general setter's per-row delete safety path for existing rows.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub(crate) fn insert_export_hashes_after_clear_in_tx(
        &self,
        exports: &[(String, String)],
    ) -> Result<usize> {
        let unique_exports = Self::dedupe_export_hash_batch(exports);
        if unique_exports.is_empty() {
            return Ok(0);
        }

        let now = Utc::now().to_rfc3339();
        let mut count = 0;

        for (issue_id, content_hash) in &unique_exports {
            self.conn.execute_with_params(
                "INSERT OR REPLACE INTO export_hashes (issue_id, content_hash, exported_at) VALUES (?, ?, ?)",
                &[
                    SqliteValue::from(issue_id.as_str()),
                    SqliteValue::from(content_hash.as_str()),
                    SqliteValue::from(now.as_str()),
                ],
            )?;
            count += 1;
        }

        Ok(count)
    }

    /// Set only export hashes whose content changed, using the caller's active transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub(crate) fn set_changed_export_hashes_in_tx(
        &self,
        exports: &[(String, String)],
    ) -> Result<usize> {
        let unique_exports = Self::dedupe_export_hash_batch(exports);
        if unique_exports.is_empty() {
            return Ok(0);
        }

        let issue_ids = unique_exports
            .iter()
            .map(|(issue_id, _hash)| issue_id.clone())
            .collect::<Vec<_>>();
        let existing_hashes = self.get_export_hashes_for_ids_in_tx(&issue_ids)?;
        let now = Utc::now().to_rfc3339();
        let mut count = 0;

        for (issue_id, content_hash) in &unique_exports {
            if existing_hashes
                .get(issue_id)
                .is_some_and(|existing_hash| existing_hash == content_hash)
            {
                continue;
            }

            self.conn.execute_with_params(
                "DELETE FROM export_hashes WHERE issue_id = ?",
                &[SqliteValue::from(issue_id.as_str())],
            )?;
            self.conn.execute_with_params(
                "INSERT INTO export_hashes (issue_id, content_hash, exported_at) VALUES (?, ?, ?)",
                &[
                    SqliteValue::from(issue_id.as_str()),
                    SqliteValue::from(content_hash.as_str()),
                    SqliteValue::from(now.as_str()),
                ],
            )?;
            count += 1;
        }

        Ok(count)
    }

    fn get_export_hashes_for_ids_in_tx(
        &self,
        issue_ids: &[String],
    ) -> Result<HashMap<String, String>> {
        if issue_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let mut hashes = HashMap::with_capacity(issue_ids.len());
        for chunk in issue_ids.chunks(DIRTY_ISSUE_CHUNK_SIZE) {
            let placeholders = vec!["?"; chunk.len()].join(", ");
            let sql = format!(
                "SELECT issue_id, content_hash FROM export_hashes WHERE issue_id IN ({})",
                placeholders
            );
            let params = chunk
                .iter()
                .map(|issue_id| SqliteValue::from(issue_id.as_str()))
                .collect::<Vec<_>>();
            let rows = self.conn.query_with_params(&sql, &params)?;

            for row in &rows {
                let Some(issue_id) = row.get(0).and_then(SqliteValue::as_text) else {
                    continue;
                };
                let Some(content_hash) = row.get(1).and_then(SqliteValue::as_text) else {
                    continue;
                };
                hashes.insert(issue_id.to_string(), content_hash.to_string());
            }
        }

        Ok(hashes)
    }

    fn dedupe_export_hash_batch(exports: &[(String, String)]) -> Vec<(String, String)> {
        let mut deduped: Vec<(String, String)> = Vec::with_capacity(exports.len());
        let mut positions: HashMap<String, usize> = HashMap::with_capacity(exports.len());

        for (issue_id, content_hash) in exports {
            if let Some(position) = positions.get(issue_id).copied() {
                if let Some((_, stored_hash)) = deduped.get_mut(position) {
                    stored_hash.clone_from(content_hash);
                } else {
                    positions.insert(issue_id.clone(), deduped.len());
                    deduped.push((issue_id.clone(), content_hash.clone()));
                }
            } else {
                positions.insert(issue_id.clone(), deduped.len());
                deduped.push((issue_id.clone(), content_hash.clone()));
            }
        }

        deduped
    }

    /// Clear export hashes using the caller's active transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub(crate) fn clear_export_hashes_in_tx(&self, issue_ids: &[String]) -> Result<usize> {
        if issue_ids.is_empty() {
            return Ok(0);
        }

        let mut total_deleted = 0;
        for chunk in issue_ids.chunks(EXPORT_HASH_CHUNK_SIZE) {
            // Delete existing entries row-by-row to avoid fsqlite IN-clause bugs
            let mut chunk_deleted = 0;
            for id in chunk {
                let deleted = self.conn.execute_with_params(
                    "DELETE FROM export_hashes WHERE issue_id = ?",
                    &[SqliteValue::from(id.as_str())],
                )?;
                chunk_deleted += deleted;
            }
            total_deleted += chunk_deleted;
        }

        Ok(total_deleted)
    }

    /// Attempt a WAL checkpoint (PASSIVE mode) to flush WAL back to the main
    /// database file. Errors are logged but do not propagate — checkpoint
    /// failure is non-fatal and will be retried on the next interval.
    fn try_wal_checkpoint(&self) {
        // Issue #219: TRUNCATE mode requires an exclusive lock, which blocks
        // all concurrent readers and writers.  Under parallel agent operations
        // this was a major source of "database is busy" errors.  PASSIVE mode
        // checkpoints only pages that are not currently needed by any reader,
        // so it never blocks other connections.  The WAL file may grow slightly
        // larger between checkpoints, but journal_size_limit (set in
        // apply_runtime_pragmas) caps it.
        if let Err(e) = self.conn.execute("PRAGMA wal_checkpoint(PASSIVE)") {
            tracing::debug!(error = %e, "WAL checkpoint failed (non-fatal, will retry later)");
        }
    }

    /// Force a full WAL checkpoint that drains every pending frame back into
    /// the main database file.
    ///
    /// Called at quiescent points (post-rebuild, before VACUUM/REINDEX) where
    /// we hold the `.write.lock` exclusively and can safely take the DB's
    /// internal exclusive lock that TRUNCATE mode requires. The passive
    /// checkpoint used during normal mutation can leave WAL frames behind
    /// that VACUUM/REINDEX later trip over ("database is busy (snapshot
    /// conflict on pages: page N > snapshot db_size M)"), so we need a
    /// stronger guarantee here.
    ///
    /// # Errors
    ///
    /// Returns an error only if even a PASSIVE checkpoint fails. TRUNCATE
    /// failure is downgraded to a warning because it is best-effort.
    pub(crate) fn checkpoint_full(&self) -> Result<()> {
        if let Err(e) = self.conn.execute("PRAGMA wal_checkpoint(TRUNCATE)") {
            tracing::debug!(
                error = %e,
                "TRUNCATE checkpoint failed; falling back to PASSIVE"
            );
            self.conn.execute("PRAGMA wal_checkpoint(PASSIVE)")?;
        }
        Ok(())
    }

    /// Run SQLite's native integrity probe and return its diagnostic rows.
    ///
    /// # Errors
    ///
    /// Returns an error if the pragma cannot be executed.
    pub(crate) fn integrity_check_messages(&self) -> Result<Vec<String>> {
        let rows = self.conn.query("PRAGMA integrity_check")?;
        let mut messages = Vec::new();
        for row in rows {
            for value in row.values() {
                if let Some(text) = value.as_text() {
                    let trimmed = text.trim();
                    if !trimmed.is_empty() {
                        messages.push(trimmed.to_string());
                    }
                }
            }
        }
        if messages.is_empty() {
            messages.push("integrity_check returned no diagnostic rows".to_string());
        }
        Ok(messages)
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

    /// Find the actor who most recently transitioned `issue_id` into the
    /// `in_progress` state. Returns `None` when the issue never had such a
    /// transition recorded — typical of issues that went from `open` to
    /// `closed` without a claim step.
    ///
    /// Used by the closure-time `forbid_self_close_after_in_progress` policy
    /// gate (issue #274 Phase 1).
    ///
    /// # Errors
    ///
    /// Returns an error if the underlying event query fails.
    pub fn find_last_in_progress_actor(&self, issue_id: &str) -> Result<Option<String>> {
        let events = crate::storage::events::get_events(&self.conn, issue_id, 0)?;
        // get_events returns DESC ordering by created_at then id, so the first
        // matching event is the most recent transition into in_progress.
        for event in events {
            if event.event_type == crate::model::EventType::StatusChanged
                && event
                    .new_value
                    .as_deref()
                    .map(str::trim)
                    .is_some_and(|v| v.eq_ignore_ascii_case("in_progress"))
            {
                let actor = event.actor.trim();
                if actor.is_empty() {
                    return Ok(None);
                }
                return Ok(Some(actor.to_string()));
            }
        }
        Ok(None)
    }

    /// Persist closure-time policy metadata (issue #274 Phase 1) for `issue_id`.
    ///
    /// Inserts (or replaces) one row in the `close_metadata` table with the
    /// supplied attribution + bypass auditing values. All fields are optional:
    /// passing every-`None` still records a row that pins `bypassed_policy = 0`
    /// for the close, which keeps the table strictly additive — every close
    /// performed under an active policy is queryable later. Callers decide
    /// whether policy metadata is active enough to warrant a row.
    ///
    /// `policy_gates_fired` is stored as the JSON serialisation of the gate
    /// names that fired (or that were waived by `--bypass-policy`). An empty
    /// slice serialises to `"[]"` so callers can always rely on JSON typing.
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails or JSON serialisation of
    /// `policy_gates_fired` fails.
    pub fn record_close_metadata(
        &self,
        issue_id: &str,
        attribution: &crate::close_policy::AttributionValues,
        bypassed: bool,
        bypass_reason: Option<&str>,
        policy_gates_fired: &[String],
    ) -> Result<()> {
        let gates_json = serde_json::to_string(policy_gates_fired).map_err(BeadsError::from)?;

        // INSERT OR REPLACE: a re-close (e.g. close → reopen → close) overwrites
        // the prior row. If the project ever needs full history, querying the
        // events table gives an audit trail; `close_metadata` is the
        // currently-effective metadata for the most recent close.
        self.conn.execute_with_params(
            "INSERT OR REPLACE INTO close_metadata (
                issue_id,
                closed_by_agent_name,
                closed_by_harness,
                closed_by_model,
                bypassed_policy,
                bypass_reason,
                policy_gates_fired,
                recorded_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP)",
            &[
                SqliteValue::from(issue_id),
                attribution
                    .agent_name
                    .as_deref()
                    .map_or(SqliteValue::Null, SqliteValue::from),
                attribution
                    .harness
                    .as_deref()
                    .map_or(SqliteValue::Null, SqliteValue::from),
                attribution
                    .model
                    .as_deref()
                    .map_or(SqliteValue::Null, SqliteValue::from),
                SqliteValue::from(i64::from(bypassed)),
                bypass_reason.map_or(SqliteValue::Null, SqliteValue::from),
                SqliteValue::from(gates_json.as_str()),
            ],
        )?;
        Ok(())
    }

    /// Read a previously-stored close-metadata row, or `None` when no policy
    /// metadata was recorded for this close. Used by tests + future
    /// observability commands.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_close_metadata(&self, issue_id: &str) -> Result<Option<CloseMetadataRow>> {
        if !crate::storage::schema::table_exists(&self.conn, "close_metadata") {
            return Ok(None);
        }
        let rows = self.conn.query_with_params(
            "SELECT closed_by_agent_name, closed_by_harness, closed_by_model, \
                    bypassed_policy, bypass_reason, policy_gates_fired, recorded_at \
             FROM close_metadata WHERE issue_id = ?",
            &[SqliteValue::from(issue_id)],
        )?;
        let Some(row) = rows.first() else {
            return Ok(None);
        };
        let bypassed = row
            .get(3)
            .and_then(SqliteValue::as_integer)
            .unwrap_or_default()
            != 0;
        let gates_json: Option<String> =
            row.get(5).and_then(SqliteValue::as_text).map(String::from);
        let policy_gates_fired = match gates_json.as_deref() {
            Some(json) if !json.is_empty() => {
                serde_json::from_str::<Vec<String>>(json).map_err(BeadsError::from)?
            }
            _ => Vec::new(),
        };
        Ok(Some(CloseMetadataRow {
            closed_by_agent_name: row.get(0).and_then(SqliteValue::as_text).map(String::from),
            closed_by_harness: row.get(1).and_then(SqliteValue::as_text).map(String::from),
            closed_by_model: row.get(2).and_then(SqliteValue::as_text).map(String::from),
            bypassed_policy: bypassed,
            bypass_reason: row.get(4).and_then(SqliteValue::as_text).map(String::from),
            policy_gates_fired,
            recorded_at: row
                .get(6)
                .and_then(SqliteValue::as_text)
                .map(String::from)
                .unwrap_or_default(),
        }))
    }

    /// Record a workflow gate result (issue #312, layer 2 / beads_rust#319).
    ///
    /// Upserts one row per `(issue_id, gate, provider)`: a provider's
    /// most-recent verdict for a named gate on an issue. A re-report from the
    /// same provider for the same gate overwrites the prior verdict, so the
    /// table always reflects the currently-effective gate state.
    ///
    /// Gate results are auxiliary, project-local metadata: like
    /// `close_metadata`, they are not part of the JSONL sync surface (the
    /// audit trail is the DB), so this writes directly rather than going
    /// through the dirty-issue export path.
    ///
    /// # Errors
    ///
    /// Returns an error if the database write fails.
    pub fn record_gate_result(
        &self,
        issue_id: &str,
        gate: &str,
        provider: &str,
        passed: bool,
        note: Option<&str>,
        recorded_by: &str,
    ) -> Result<()> {
        self.conn.execute_with_params(
            "INSERT OR REPLACE INTO gate_results (
                issue_id, gate, provider, passed, note, recorded_by, recorded_at
            ) VALUES (?, ?, ?, ?, ?, ?, CURRENT_TIMESTAMP)",
            &[
                SqliteValue::from(issue_id),
                SqliteValue::from(gate),
                SqliteValue::from(provider),
                SqliteValue::from(i64::from(passed)),
                note.map_or(SqliteValue::Null, SqliteValue::from),
                SqliteValue::from(recorded_by),
            ],
        )?;
        Ok(())
    }

    /// Read every recorded gate result for `issue_id`, ordered by gate then
    /// provider for deterministic output. Returns an empty vec when the
    /// `gate_results` table is absent (e.g. a database that predates the v12
    /// migration and has not been reopened) or no results exist.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_gate_results(&self, issue_id: &str) -> Result<Vec<crate::close_policy::GateResult>> {
        if !crate::storage::schema::table_exists(&self.conn, "gate_results") {
            return Ok(Vec::new());
        }
        let rows = self.conn.query_with_params(
            "SELECT gate, provider, passed, note FROM gate_results \
             WHERE issue_id = ? ORDER BY gate, provider",
            &[SqliteValue::from(issue_id)],
        )?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                let gate = row.get(0).and_then(SqliteValue::as_text)?.to_string();
                let provider = row.get(1).and_then(SqliteValue::as_text)?.to_string();
                let passed = row
                    .get(2)
                    .and_then(SqliteValue::as_integer)
                    .unwrap_or_default()
                    != 0;
                let note = row.get(3).and_then(SqliteValue::as_text).map(String::from);
                Some(crate::close_policy::GateResult {
                    gate,
                    provider,
                    passed,
                    note,
                })
            })
            .collect())
    }

    /// Stage Tier 1 attribution for the next mutation (issue #312, Layer 3
    /// capture-only). The values are stamped onto every audit event produced by
    /// the immediately following `create`/`update`/status-mutating call, then
    /// cleared. Pass an empty [`EventAttribution`] (or never call this) to record
    /// no attribution. This is a recorded audit trail only — it is never used to
    /// gate, reject, or alter any transition.
    pub fn set_pending_event_attribution(&mut self, attribution: EventAttribution) {
        self.pending_event_attribution = if attribution.is_empty() {
            None
        } else {
            Some(attribution)
        };
    }

    /// Remove and return any staged Tier 1 attribution without consuming it via
    /// a mutation (issue #312, Layer 3). Used by the JSONL-recovery path to
    /// carry a not-yet-committed staged value across a storage rebuild so the
    /// post-recovery retry can still stamp it (F1). Preserves the invariant that
    /// pending attribution is consumed by exactly one committing mutation, or
    /// transferred/cleared — it never leaks into an unrelated operation.
    pub(crate) fn take_pending_event_attribution(&mut self) -> Option<EventAttribution> {
        self.pending_event_attribution.take()
    }

    /// Execute a mutation with the 4-step transaction protocol.
    ///
    /// Retries on all transient BUSY errors (from BEGIN, DML, or COMMIT) with
    /// exponential backoff.  This is the fix for issue #109 — previously only
    /// `BusySnapshot` at COMMIT time was retried, while `Busy` from
    /// `BEGIN IMMEDIATE` (lock contention) would propagate immediately,
    /// causing concurrent close/update operations to silently lose data.
    ///
    /// # Errors
    ///
    /// Returns an error if any step fails (e.g. database error, logic error).
    /// The transaction is rolled back on error.
    #[allow(clippy::too_many_lines)]
    pub fn mutate<F, R>(&mut self, op: &str, actor: &str, mut f: F) -> Result<R>
    where
        F: FnMut(&Connection, &mut MutationContext) -> Result<R>,
    {
        // Disable FK enforcement before the transaction begins.  PRAGMA
        // foreign_keys can only be changed outside an active transaction.
        // fsqlite can surface false FK violations when its page buffer pool
        // is exhausted, even though the referenced issue_id was just
        // written/verified in the same transaction (#215).  All FK
        // invariants (dependencies -> issues, events -> issues,
        // dirty_issues -> issues, etc.) are enforced by application logic
        // within the mutation closures.
        self.conn.execute("PRAGMA foreign_keys = OFF")?;

        // Peek (clone) — do NOT take — the per-command attribution staged for
        // this mutation. We must not permanently consume the staged value until
        // the mutation is known to COMMIT: if `with_write_transaction` returns a
        // recoverable `Database(_)` error, `retry_mutation_with_jsonl_recovery`
        // re-invokes this `mutate()` after rebuilding the DB from JSONL, and the
        // staged slot must still be present so the recovered write records the
        // attribution (#312 hardening, F1). Cloning here also means the internal
        // BUSY-retry loop inside `with_write_transaction` re-applies the SAME
        // value on each attempt and stamps exactly once on the committing run.
        //
        // Invariant: pending attribution is consumed by exactly one committing
        // mutation, or cleared — it never leaks into a later, unrelated
        // operation. It is `.take()`n below only after a successful commit.
        let pending_attribution = self.pending_event_attribution.clone().unwrap_or_default();

        let tx_result: Result<_> = self.with_write_transaction(|storage| {
            let mut ctx = MutationContext::new(op, actor);
            ctx.attribution = pending_attribution.clone();
            let result = f(&storage.conn, &mut ctx)?;

            // Write events
            if !ctx.events.is_empty() {
                let sql = "INSERT INTO events (issue_id, event_type, actor, old_value, new_value, comment, created_at, agent_name, harness, model) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";
                for event in &ctx.events {
                    let params = vec![
                        SqliteValue::from(event.issue_id.as_str()),
                        SqliteValue::from(event.event_type.as_str()),
                        SqliteValue::from(event.actor.as_str()),
                        event
                            .old_value
                            .as_deref()
                            .map_or(SqliteValue::Null, SqliteValue::from),
                        event
                            .new_value
                            .as_deref()
                            .map_or(SqliteValue::Null, SqliteValue::from),
                        event
                            .comment
                            .as_deref()
                            .map_or(SqliteValue::Null, SqliteValue::from),
                        SqliteValue::from(event.created_at.to_rfc3339()),
                        event
                            .agent_name
                            .as_deref()
                            .map_or(SqliteValue::Null, SqliteValue::from),
                        event
                            .harness
                            .as_deref()
                            .map_or(SqliteValue::Null, SqliteValue::from),
                        event
                            .model
                            .as_deref()
                            .map_or(SqliteValue::Null, SqliteValue::from),
                    ];
                    storage.conn.execute_with_params(sql, &params)?;
                }
            }

            // Mark dirty
            if !ctx.dirty_ids.is_empty() {
                let now_str = Utc::now().to_rfc3339();
                // Collect IDs into a Vec for chunked processing
                let dirty_vec: Vec<_> = ctx.dirty_ids.iter().collect();

                for chunk in dirty_vec.chunks(DIRTY_ISSUE_CHUNK_SIZE) {
                    // Explicit DELETE + INSERT instead of INSERT OR REPLACE because
                    // fsqlite does not reliably support UNIQUE constraint upserts.
                    for insert_chunk in chunk.chunks(450) {
                        // Delete existing entries row-by-row to avoid fsqlite IN-clause bugs
                        for id in insert_chunk {
                            storage.conn.execute_with_params(
                                "DELETE FROM dirty_issues WHERE issue_id = ?",
                                &[SqliteValue::from(id.as_str())],
                            )?;
                        }

                        // Now insert fresh rows one by one
                        for id in insert_chunk {
                            storage.conn.execute_with_params(
                                "INSERT INTO dirty_issues (issue_id, marked_at) VALUES (?, ?)",
                                &[
                                    SqliteValue::from(id.as_str()),
                                    SqliteValue::from(now_str.as_str()),
                                ],
                            )?;
                        }
                    }
                }
            }

            let blocked_cache_plan = BlockedCacheRefreshPlan::from_context(&ctx);
            if blocked_cache_plan.is_some() {
                storage.set_metadata_in_tx(BLOCKED_CACHE_STATE_KEY, BLOCKED_CACHE_STATE_STALE)?;
            }

            if ctx.force_flush {
                Self::upsert_metadata_key_in_tx(&storage.conn, NEEDS_FLUSH_KEY, "true")?;
            }

            Ok((result, blocked_cache_plan))
        });

        // Consume the staged attribution only now that the transaction has
        // COMMITTED (#312 hardening, F1). On a recoverable error the slot is
        // intentionally left intact so the JSONL-recovery retry can re-stamp it.
        if tx_result.is_ok() {
            self.pending_event_attribution = None;
        }

        // Re-enable FK enforcement after the transaction completes
        // (regardless of success or failure).
        let (result, blocked_cache_plan) =
            Self::finish_foreign_key_suppressed_result(&self.conn, op, tx_result)?;

        match blocked_cache_plan {
            Some(BlockedCacheRefreshPlan::Deferred) => {
                // Stale marker already set inside the transaction.  Reads will
                // compute blocked state in-memory until the next non-deferred
                // write rebuilds the cache.  Skipping the eager second write
                // transaction eliminates DB lock contention for dep add/remove.
                tracing::debug!(
                    operation = op,
                    "Blocked cache refresh deferred; will rebuild lazily on next read"
                );
            }
            Some(ref plan) => {
                if let Err(error) = self.refresh_blocked_cache_after_commit(op, plan) {
                    self.handle_blocked_cache_refresh_error(op, error)?;
                }
            }
            None => {}
        }

        Ok(result)
    }

    /// Create a new issue.
    ///
    /// # Errors
    ///
    /// Returns an error if the issue cannot be inserted (e.g. ID collision).
    #[allow(clippy::too_many_lines)]
    pub fn create_issue(&mut self, issue: &Issue, actor: &str) -> Result<()> {
        IssueValidator::validate(issue).map_err(BeadsError::from_validation_errors)?;
        validate_issue_comments_for_create(issue)?;

        self.mutate("create_issue", actor, |conn, ctx| {
            // Explicit duplicate check since fsqlite does not enforce
            // UNIQUE constraints on non-rowid columns.
            match conn.query_row_with_params(
                "SELECT 1 FROM issues WHERE id = ? LIMIT 1",
                &[SqliteValue::from(issue.id.as_str())],
            ) {
                Ok(_) => {
                    return Err(BeadsError::IdCollision {
                        id: issue.id.clone(),
                    });
                }
                Err(FrankenError::QueryReturnedNoRows) => {}
                Err(error) => return Err(error.into()),
            }

            // Check for external_ref collision
            if let Some(ref ext_ref) = issue.external_ref {
                let existing_ext = conn.query_with_params(
                    "SELECT id FROM issues WHERE external_ref = ? LIMIT 1",
                    &[SqliteValue::from(ext_ref.as_str())],
                )?;
                if let Some(existing_row) = existing_ext.first() {
                    let other_id = existing_row
                        .get(0)
                        .and_then(SqliteValue::as_text)
                        .unwrap_or_default()
                        .to_string();
                    return Err(BeadsError::Config(format!(
                        "External reference '{ext_ref}' already exists on issue {other_id}"
                    )));
                }
            }

            let status_str = issue.status.as_str();
            let issue_type_str = issue.issue_type.as_str();
            let created_at_str = issue.created_at.to_rfc3339();
            let updated_at_str = issue.updated_at.to_rfc3339();
            let closed_at_str = issue.closed_at.map(|dt| dt.to_rfc3339());
            let due_at_str = issue.due_at.map(|dt| dt.to_rfc3339());
            let defer_until_str = issue.defer_until.map(|dt| dt.to_rfc3339());
            let deleted_at_str = issue.deleted_at.map(|dt| dt.to_rfc3339());
            let compacted_at_str = issue.compacted_at.map(|dt| dt.to_rfc3339());
            let content_hash = issue.compute_content_hash();

            conn.execute_with_params(
                "INSERT INTO issues (
                    id, content_hash, title, description, design, acceptance_criteria, notes,
                    status, priority, issue_type, assignee, owner, estimated_minutes,
                    created_at, created_by, updated_at, closed_at, close_reason,
                    closed_by_session, due_at, defer_until, external_ref, source_system,
                    source_repo, source_repo_path, deleted_at, deleted_by, delete_reason, original_type,
                    compaction_level, compacted_at, compacted_at_commit, original_size,
                    sender, ephemeral, pinned, is_template, agent_context
                 ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
                &[
                    SqliteValue::from(issue.id.as_str()),
                    SqliteValue::from(content_hash.as_str()),
                    SqliteValue::from(issue.title.as_str()),
                    SqliteValue::from(issue.description.as_deref().unwrap_or("")),
                    SqliteValue::from(issue.design.as_deref().unwrap_or("")),
                    SqliteValue::from(issue.acceptance_criteria.as_deref().unwrap_or("")),
                    SqliteValue::from(issue.notes.as_deref().unwrap_or("")),
                    SqliteValue::from(status_str),
                    SqliteValue::from(issue.priority.0),
                    SqliteValue::from(issue_type_str),
                    issue.assignee.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                    SqliteValue::from(issue.owner.as_deref().unwrap_or("")),
                    issue.estimated_minutes.map_or(SqliteValue::Null, SqliteValue::from),
                    SqliteValue::from(created_at_str.as_str()),
                    SqliteValue::from(issue.created_by.as_deref().unwrap_or("")),
                    SqliteValue::from(updated_at_str.as_str()),
                    closed_at_str.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                    SqliteValue::from(issue.close_reason.as_deref().unwrap_or("")),
                    SqliteValue::from(issue.closed_by_session.as_deref().unwrap_or("")),
                    due_at_str.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                    defer_until_str.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                    issue.external_ref.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                    SqliteValue::from(issue.source_system.as_deref().unwrap_or("")),
                    SqliteValue::from(issue.source_repo.as_deref().unwrap_or(".")),
                    issue.source_repo_path.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                    deleted_at_str.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                    SqliteValue::from(issue.deleted_by.as_deref().unwrap_or("")),
                    SqliteValue::from(issue.delete_reason.as_deref().unwrap_or("")),
                    SqliteValue::from(issue.original_type.as_deref().unwrap_or("")),
                    SqliteValue::from(i64::from(issue.compaction_level.unwrap_or(0))),
                    compacted_at_str.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                    issue.compacted_at_commit.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                    SqliteValue::from(i64::from(issue.original_size.unwrap_or(0))),
                    SqliteValue::from(issue.sender.as_deref().unwrap_or("")),
                    SqliteValue::from(i64::from(i32::from(issue.ephemeral))),
                    SqliteValue::from(i64::from(i32::from(issue.pinned))),
                    SqliteValue::from(i64::from(i32::from(issue.is_template))),
                    issue.agent_context.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                ],
            )?;

            // Update child counter if this is a hierarchical ID
            if let Ok(parsed) = parse_id(&issue.id)
                && !parsed.is_root()
                && let Some(parent) = parsed.parent()
                && let Some(&child_num) = parsed.child_path.last()
            {
                Self::update_child_counter_in_tx(conn, &parent, child_num)?;
            }

            // Insert Labels
            let mut seen_labels = HashSet::new();
            for label in &issue.labels {
                if !seen_labels.insert(label.as_str()) {
                    continue;
                }
                conn.execute_with_params(
                    "INSERT INTO labels (issue_id, label) VALUES (?, ?)",
                    &[SqliteValue::from(issue.id.as_str()), SqliteValue::from(label.as_str())],
                )?;
                ctx.record_event(
                    EventType::LabelAdded,
                    &issue.id,
                    Some(format!("Added label {label}")),
                );
            }

            // Insert Dependencies
            let mut seen_deps = HashSet::new();
            for dep in &issue.dependencies {
                if dep.depends_on_id == issue.id {
                    return Err(BeadsError::SelfDependency {
                        id: issue.id.clone(),
                    });
                }

                if !seen_deps.insert(dep.depends_on_id.as_str()) {
                    continue;
                }
                Self::ensure_dependency_target_exists_in_tx(conn, &dep.depends_on_id)?;
                // Check cycle if blocking.
                if Self::check_dependency_cycle_for_type(
                    conn,
                    &issue.id,
                    &dep.depends_on_id,
                    &dep.dep_type,
                    true,
                )? {
                    return Err(BeadsError::DependencyCycle {
                        path: format!(
                            "Adding dependency {} -> {} would create a cycle",
                            issue.id, dep.depends_on_id
                        ),
                    });
                }

                conn.execute_with_params(
                    "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by)
                     VALUES (?, ?, ?, ?, ?)",
                    &[
                        SqliteValue::from(issue.id.as_str()),
                        SqliteValue::from(dep.depends_on_id.as_str()),
                        SqliteValue::from(dep.dep_type.as_str()),
                        SqliteValue::from(dep.created_at.to_rfc3339()),
                        SqliteValue::from(dep.created_by.as_deref().unwrap_or(actor)),
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
                ctx.invalidate_cache_for(&[issue.id.as_str(), dep.depends_on_id.as_str()]);
            }

            // Insert Comments
            for comment in &issue.comments {
                conn.execute_with_params(
                    "INSERT INTO comments (issue_id, author, text, created_at) VALUES (?, ?, ?, ?)",
                    &[
                        SqliteValue::from(issue.id.as_str()),
                        SqliteValue::from(comment.author.as_str()),
                        SqliteValue::from(comment.body.as_str()),
                        SqliteValue::from(comment.created_at.to_rfc3339()),
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

    /// Iterative BFS cycle detection (replaces recursive CTE).
    ///
    /// Checks whether adding an edge `issue_id -> depends_on_id` would create
    /// a cycle.  This works by starting from `depends_on_id` and walking its
    /// transitive forward dependencies; if any reachable node equals `issue_id`,
    /// a cycle would be formed.
    ///
    /// Uses lazy per-node BFS: instead of bulk-loading the entire dependency
    /// graph into memory, each BFS step queries only the immediate neighbors
    /// of the current frontier node.  On sparse graphs this visits a tiny
    /// fraction of total edges, giving dramatic speedups for `dep add` on
    /// large repositories (e.g. 813-issue graphs).
    ///
    /// Two kinds of edges are followed in the blocker graph:
    /// 1. Standard deps (`issue_id -> depends_on_id`), filtered by type.
    /// 2. Parent-child edges reversed (`depends_on_id -> issue_id`), since a
    ///    parent finishing requires its children to finish first.
    ///
    /// A depth cap prevents pathological traversal on corrupted graphs.
    fn check_cycle(
        conn: &Connection,
        issue_id: &str,
        depends_on_id: &str,
        blocking_only: bool,
    ) -> Result<bool> {
        // Build the per-node neighbor query.  We UNION two directions:
        //   (a) standard deps: given node as issue_id, follow to depends_on_id
        //   (b) parent-child reversed: given node as depends_on_id (parent),
        //       follow to issue_id (child) -- because parent is blocked by child
        let neighbor_sql = if blocking_only {
            "SELECT depends_on_id FROM dependencies \
             WHERE issue_id = ? AND type IN ('blocks', 'conditional-blocks', 'waits-for') \
             UNION \
             SELECT issue_id FROM dependencies \
             WHERE depends_on_id = ? AND type = 'parent-child'"
                .to_string()
        } else {
            "SELECT depends_on_id FROM dependencies \
             WHERE issue_id = ? AND type != 'parent-child' \
             UNION \
             SELECT issue_id FROM dependencies \
             WHERE depends_on_id = ? AND type = 'parent-child'"
                .to_string()
        };

        let stmt = conn.prepare(&neighbor_sql)?;

        let mut visited = HashSet::new();
        // Level-synchronous BFS: process all nodes at one depth before moving
        // to the next, so we can enforce a depth cap cleanly.
        let mut frontier: Vec<String> = vec![depends_on_id.to_string()];
        visited.insert(depends_on_id.to_string());

        for _depth in 0..DEPENDENCY_TRAVERSAL_MAX_DEPTH {
            if frontier.is_empty() {
                break;
            }

            let mut next_frontier = Vec::new();

            for node in &frontier {
                let rows = stmt.query_with_params(&[
                    SqliteValue::from(node.as_str()),
                    SqliteValue::from(node.as_str()),
                ])?;

                for row in &rows {
                    if let Some(neighbor) = row.get(0).and_then(SqliteValue::as_text) {
                        if neighbor == issue_id {
                            return Ok(true); // Cycle detected -- early exit
                        }
                        if visited.insert(neighbor.to_string()) {
                            next_frontier.push(neighbor.to_string());
                        }
                    }
                }
            }

            frontier = next_frontier;
        }

        Ok(false)
    }

    fn check_parent_child_cycle(
        conn: &Connection,
        child_id: &str,
        parent_id: &str,
        blocking_only: bool,
    ) -> Result<bool> {
        // Stored parent-child rows are child -> parent, but the blocker graph
        // edge is parent -> child because parents wait for children.
        Self::check_cycle(conn, parent_id, child_id, blocking_only)
    }

    fn check_dependency_cycle_for_type(
        conn: &Connection,
        issue_id: &str,
        depends_on_id: &str,
        dep_type: &DependencyType,
        blocking_only: bool,
    ) -> Result<bool> {
        if !dep_type.is_blocking() {
            return Ok(false);
        }
        if matches!(dep_type, DependencyType::ParentChild) {
            Self::check_parent_child_cycle(conn, issue_id, depends_on_id, blocking_only)
        } else {
            Self::check_cycle(conn, issue_id, depends_on_id, blocking_only)
        }
    }

    /// Update an issue's fields.
    ///
    /// # Errors
    ///
    /// Returns an error if the issue doesn't exist or the update fails.
    #[allow(clippy::too_many_lines)]
    pub fn update_issue(&mut self, id: &str, updates: &IssueUpdate, actor: &str) -> Result<Issue> {
        if updates.is_empty() {
            // No-op update: this path returns WITHOUT calling `mutate()`, so we
            // must clear any staged attribution ourselves. Otherwise a value set
            // by `set_pending_event_attribution` would survive onto the NEXT,
            // unrelated `mutate()` (#312 hardening, F2). Invariant: pending
            // attribution is consumed by exactly one committing mutation, or
            // cleared.
            self.pending_event_attribution = None;
            return self
                .get_issue(id)?
                .ok_or_else(|| BeadsError::IssueNotFound { id: id.to_string() });
        }

        self.mutate("update_issue", actor, |conn, ctx| {
            let mut issue = Self::get_issue_from_conn(conn, id)?.ok_or_else(|| {
                // Issue #245: if `get_issue` (read path) can find the row but
                // `get_issue_from_conn` inside a write transaction cannot, the
                // database is likely corrupt (B-tree or index malformation).
                // Surface a more helpful error than bare ISSUE_NOT_FOUND.
                tracing::warn!(
                    id = %id,
                    "update_issue: row not found inside write transaction \
                     (possible DB corruption — run `br doctor --repair`)"
                );
                BeadsError::IssueNotFound { id: id.to_string() }
            })?;

            if issue.status == Status::Tombstone {
                return Err(BeadsError::Validation {
                    field: "issue_id".to_string(),
                    reason: format!("cannot update tombstone issue: {id}"),
                });
            }

            // Atomic claim guard: check assignee INSIDE the CONCURRENT transaction
            // to prevent TOCTOU races where two agents both see "unassigned".
            if updates.expect_unassigned {
                let current_assignee = match conn.query_row_with_params(
                    "SELECT assignee FROM issues WHERE id = ?",
                    &[SqliteValue::from(id)],
                ) {
                    Ok(row) => row.get(0).and_then(SqliteValue::as_text).map(String::from),
                    Err(FrankenError::QueryReturnedNoRows) => None,
                    Err(error) => return Err(error.into()),
                };
                let trimmed = current_assignee
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let claim_actor = updates.claim_actor.as_deref().unwrap_or("");

                match trimmed {
                    None => { /* unassigned, proceed with claim */ }
                    Some(current) if !updates.claim_exclusive && current == claim_actor => {
                        /* same actor re-claim, idempotent */
                    }
                    Some(current) => {
                        return Err(BeadsError::validation(
                            "claim",
                            format!("issue {id} already assigned to {current}"),
                        ));
                    }
                }
            }

            let mut set_clauses: Vec<String> = vec![];
            let mut params: Vec<SqliteValue> = vec![];

            // Helper to add update
            let mut add_update = |field: &str, val: SqliteValue| {
                set_clauses.push(format!("{field} = ?"));
                params.push(val);
            };

            // Title
            if let Some(ref title) = updates.title {
                let old_title = issue.title.clone();
                issue.title.clone_from(title);
                add_update("title", SqliteValue::from(title.as_str()));
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
                    SqliteValue::from(val.as_deref().unwrap_or("")),
                );
            }
            if let Some(ref val) = updates.design {
                issue.design.clone_from(val);
                add_update("design", SqliteValue::from(val.as_deref().unwrap_or("")));
            }
            if let Some(ref val) = updates.acceptance_criteria {
                issue.acceptance_criteria.clone_from(val);
                add_update(
                    "acceptance_criteria",
                    SqliteValue::from(val.as_deref().unwrap_or("")),
                );
            }
            if let Some(ref val) = updates.notes {
                issue.notes.clone_from(val);
                add_update("notes", SqliteValue::from(val.as_deref().unwrap_or("")));
            }

            // Status
            if let Some(ref status) = updates.status {
                let old_status_obj = issue.status.clone();
                let old_status = old_status_obj.as_str().to_string();
                let was_terminal = old_status_obj.is_terminal();
                issue.status.clone_from(status);
                add_update("status", SqliteValue::from(status.as_str()));

                if status.as_str() != old_status {
                    ctx.record_field_change(
                        EventType::StatusChanged,
                        id,
                        Some(old_status),
                        Some(status.as_str().to_string()),
                        None,
                    );
                }

                // Record Closed event if status is now Closed and wasn't before
                if *status == Status::Closed {
                    if !was_terminal {
                        let reason = updates.close_reason.as_ref().and_then(Clone::clone);
                        ctx.record_event(EventType::Closed, id, reason);
                    }

                    // Auto-set closed_at if not provided
                    if updates.closed_at.is_none() && issue.closed_at.is_none() {
                        let now = Utc::now();
                        issue.closed_at = Some(now);
                        add_update("closed_at", SqliteValue::from(now.to_rfc3339()));
                    }

                    if issue.deleted_at.is_some() {
                        issue.deleted_at = None;
                        issue.deleted_by = None;
                        issue.delete_reason = None;
                        add_update("deleted_at", SqliteValue::Null);
                        add_update("deleted_by", SqliteValue::Null);
                        add_update("delete_reason", SqliteValue::Null);
                    }
                } else if *status == Status::Tombstone {
                    let reason = updates.close_reason.as_ref().and_then(Clone::clone);

                    if !was_terminal {
                        ctx.record_event(EventType::Deleted, id, reason.clone());
                    }

                    let now = Utc::now();
                    issue.deleted_at = Some(now);
                    issue.deleted_by = Some(actor.to_string());
                    issue.delete_reason.clone_from(&reason);
                    add_update("deleted_at", SqliteValue::from(now.to_rfc3339()));
                    add_update("deleted_by", SqliteValue::from(actor));
                    // Always update delete_reason if we are setting to Tombstone,
                    // using close_reason as fallback if provided.
                    add_update(
                        "delete_reason",
                        SqliteValue::from(reason.as_deref().unwrap_or("")),
                    );
                } else {
                    if was_terminal && !status.is_terminal() {
                        ctx.record_event(EventType::Reopened, id, None);
                        conn.execute_with_params(
                            "DELETE FROM close_metadata WHERE issue_id = ?",
                            &[SqliteValue::from(id)],
                        )?;
                    }
                    if issue.closed_at.is_some() && updates.closed_at.is_none() {
                        // Reopening (or fixing state): Clear closed_at if it was set
                        issue.closed_at = None;
                        issue.close_reason = None;
                        issue.closed_by_session = None;
                        add_update("closed_at", SqliteValue::Null);
                        add_update("close_reason", SqliteValue::from(""));
                        add_update("closed_by_session", SqliteValue::from(""));
                    }
                    if issue.deleted_at.is_some() {
                        issue.deleted_at = None;
                        issue.deleted_by = None;
                        issue.delete_reason = None;
                        add_update("deleted_at", SqliteValue::Null);
                        add_update("deleted_by", SqliteValue::Null);
                        add_update("delete_reason", SqliteValue::Null);
                    }
                }

                if updates.skip_cache_rebuild {
                    ctx.invalidate_cache_deferred();
                } else {
                    ctx.invalidate_cache();
                }
            }

            // Priority
            if let Some(priority) = updates.priority {
                let old_priority = issue.priority.0;
                if priority.0 != old_priority {
                    issue.priority = priority;
                    add_update("priority", SqliteValue::from(i64::from(priority.0)));
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
                add_update("issue_type", SqliteValue::from(issue_type.as_str()));
            }

            // Assignee
            if let Some(ref assignee_opt) = updates.assignee {
                let old_assignee = issue.assignee.clone();
                issue.assignee.clone_from(assignee_opt);
                add_update(
                    "assignee",
                    assignee_opt
                        .as_deref()
                        .map_or(SqliteValue::Null, SqliteValue::from),
                );
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
                add_update("owner", SqliteValue::from(val.as_deref().unwrap_or("")));
            }
            if let Some(ref val) = updates.estimated_minutes {
                issue.estimated_minutes = *val;
                add_update(
                    "estimated_minutes",
                    val.map_or(SqliteValue::Null, |v| SqliteValue::from(i64::from(v))),
                );
            }
            if let Some(ref val) = updates.external_ref {
                // Explicit uniqueness check for fsqlite
                if let Some(ext_ref) = val {
                    let existing_ext = conn.query_with_params(
                        "SELECT id FROM issues WHERE external_ref = ? AND id != ? LIMIT 1",
                        &[SqliteValue::from(ext_ref.as_str()), SqliteValue::from(id)],
                    )?;
                    if let Some(existing_row) = existing_ext.first() {
                        let other_id = existing_row
                            .get(0)
                            .and_then(SqliteValue::as_text)
                            .unwrap_or_default()
                            .to_string();
                        return Err(BeadsError::Config(format!(
                            "External reference '{ext_ref}' already exists on issue {other_id}"
                        )));
                    }
                }

                issue.external_ref.clone_from(val);
                add_update(
                    "external_ref",
                    val.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                );
            }
            if let Some(ref val) = updates.source_repo {
                // `source_repo` is NOT NULL DEFAULT '.' so an explicit clear
                // must fall back to "." rather than SQL NULL — otherwise the
                // schema's NOT NULL constraint rejects the write.
                let next = val.clone().unwrap_or_else(|| ".".to_string());
                issue.source_repo = Some(next.clone());
                add_update("source_repo", SqliteValue::from(next.as_str()));
            }
            if let Some(ref val) = updates.source_repo_path {
                issue.source_repo_path.clone_from(val);
                add_update(
                    "source_repo_path",
                    val.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                );
            }
            if let Some(ref val) = updates.agent_context {
                issue.agent_context.clone_from(val);
                add_update(
                    "agent_context",
                    val.as_deref().map_or(SqliteValue::Null, SqliteValue::from),
                );
            }
            // Use empty string instead of NULL for bd compatibility
            if let Some(ref val) = updates.close_reason {
                issue.close_reason.clone_from(val);
                add_update(
                    "close_reason",
                    SqliteValue::from(val.as_deref().unwrap_or("")),
                );
            }
            if let Some(ref val) = updates.closed_by_session {
                issue.closed_by_session.clone_from(val);
                add_update(
                    "closed_by_session",
                    SqliteValue::from(val.as_deref().unwrap_or("")),
                );
            }

            // Tombstone fields
            if let Some(ref val) = updates.deleted_at {
                issue.deleted_at = *val;
                add_update(
                    "deleted_at",
                    val.map_or(SqliteValue::Null, |d| SqliteValue::from(d.to_rfc3339())),
                );
            }
            // Use empty string instead of NULL for bd compatibility
            if let Some(ref val) = updates.deleted_by {
                issue.deleted_by.clone_from(val);
                add_update(
                    "deleted_by",
                    SqliteValue::from(val.as_deref().unwrap_or("")),
                );
            }
            if let Some(ref val) = updates.delete_reason {
                issue.delete_reason.clone_from(val);
                add_update(
                    "delete_reason",
                    SqliteValue::from(val.as_deref().unwrap_or("")),
                );
            }

            // Date fields
            if let Some(ref val) = updates.due_at {
                issue.due_at = *val;
                add_update(
                    "due_at",
                    val.map_or(SqliteValue::Null, |d| SqliteValue::from(d.to_rfc3339())),
                );
            }
            if let Some(ref val) = updates.defer_until {
                issue.defer_until = *val;
                add_update(
                    "defer_until",
                    val.map_or(SqliteValue::Null, |d| SqliteValue::from(d.to_rfc3339())),
                );
            }
            if let Some(ref val) = updates.closed_at {
                issue.closed_at = *val;
                add_update(
                    "closed_at",
                    val.map_or(SqliteValue::Null, |d| SqliteValue::from(d.to_rfc3339())),
                );
            }

            if set_clauses.is_empty() {
                return Ok(());
            }

            // Update updated_at only when a stored field needs rewriting.
            let updated_at = Utc::now();
            issue.updated_at = updated_at;

            IssueValidator::validate(&issue).map_err(BeadsError::from_validation_errors)?;

            set_clauses.push("updated_at = ?".to_string());
            params.push(SqliteValue::from(updated_at.to_rfc3339()));

            // Update content hash
            let new_hash = issue.compute_content_hash();
            set_clauses.push("content_hash = ?".to_string());
            params.push(SqliteValue::from(new_hash));

            // Build and execute SQL. Claim operations use an additional
            // compare-and-set predicate so exactly one contender can win even
            // if two writers both observed the row as unassigned earlier.
            let mut where_clause = "id = ?".to_string();
            params.push(SqliteValue::from(id));
            if updates.expect_unassigned {
                where_clause.push_str(" AND (assignee IS NULL OR TRIM(assignee) = ''");
                if !updates.claim_exclusive
                    && let Some(claim_actor) = updates
                        .claim_actor
                        .as_deref()
                        .filter(|actor| !actor.is_empty())
                {
                    where_clause.push_str(" OR assignee = ?");
                    params.push(SqliteValue::from(claim_actor));
                }
                where_clause.push(')');
            }

            let sql = format!(
                "UPDATE issues SET {} WHERE {where_clause}",
                set_clauses.join(", ")
            );
            let updated_rows = conn.execute_with_params(&sql, &params)?;
            if updated_rows == 0 {
                if updates.expect_unassigned {
                    let current_assignee = match conn.query_row_with_params(
                        "SELECT assignee FROM issues WHERE id = ?",
                        &[SqliteValue::from(id)],
                    ) {
                        Ok(row) => row
                            .get(0)
                            .and_then(SqliteValue::as_text)
                            .map(String::from)
                            .and_then(|assignee| {
                                let trimmed = assignee.trim().to_string();
                                (!trimmed.is_empty()).then_some(trimmed)
                            })
                            .unwrap_or_else(|| "<unknown>".to_string()),
                        Err(FrankenError::QueryReturnedNoRows) => "<unknown>".to_string(),
                        Err(error) => return Err(error.into()),
                    };
                    return Err(BeadsError::validation(
                        "claim",
                        format!("issue {id} already assigned to {current_assignee}"),
                    ));
                }

                return Err(BeadsError::IssueNotFound { id: id.to_string() });
            }

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

        if issue.status == Status::Tombstone {
            return Ok(issue);
        }

        let was_terminal = issue.status.is_terminal();
        let original_type = issue.issue_type.as_str().to_string();
        let timestamp = deleted_at.unwrap_or_else(Utc::now);
        let mut tombstone_issue = issue;
        tombstone_issue.status = Status::Tombstone;
        let tombstone_hash = crate::util::content_hash(&tombstone_issue);

        self.mutate("delete_issue", actor, |conn, ctx| {
            conn.execute_with_params(
                "UPDATE issues SET
                    content_hash = ?,
                    status = 'tombstone',
                    deleted_at = ?,
                    deleted_by = ?,
                    delete_reason = ?,
                    original_type = ?,
                    updated_at = ?
                 WHERE id = ?",
                &[
                    SqliteValue::from(tombstone_hash.as_str()),
                    SqliteValue::from(timestamp.to_rfc3339()),
                    SqliteValue::from(actor),
                    SqliteValue::from(reason),
                    SqliteValue::from(original_type.as_str()),
                    SqliteValue::from(Utc::now().to_rfc3339()),
                    SqliteValue::from(id),
                ],
            )?;
            conn.execute_with_params(
                "DELETE FROM close_metadata WHERE issue_id = ?",
                &[SqliteValue::from(id)],
            )?;

            if !was_terminal {
                ctx.record_event(
                    EventType::Deleted,
                    id,
                    Some(format!("Deleted issue: {reason}")),
                );
            }
            ctx.mark_dirty(id);
            ctx.invalidate_cache();

            Ok(())
        })?;

        self.get_issue(id)?
            .ok_or_else(|| BeadsError::IssueNotFound { id: id.to_string() })
    }

    /// Physically remove an issue and all related data from the database.
    ///
    /// Unlike `delete_issue` (which creates a tombstone), this permanently
    /// removes the issue row plus its labels, dependencies, comments, and
    /// events so it will not appear in subsequent JSONL exports.
    ///
    /// # Errors
    ///
    /// Returns an error if the issue doesn't exist or a database operation fails.
    pub fn purge_issue(&mut self, id: &str, actor: &str) -> Result<()> {
        if self.get_issue(id)?.is_none() {
            return Err(BeadsError::IssueNotFound { id: id.to_string() });
        }

        self.mutate("purge_issue", actor, |conn, ctx| {
            conn.execute_with_params(
                "DELETE FROM comments WHERE issue_id = ?",
                &[SqliteValue::from(id)],
            )?;
            conn.execute_with_params(
                "DELETE FROM labels WHERE issue_id = ?",
                &[SqliteValue::from(id)],
            )?;
            conn.execute_with_params(
                "DELETE FROM dependencies WHERE issue_id = ?",
                &[SqliteValue::from(id)],
            )?;
            conn.execute_with_params(
                "DELETE FROM dependencies WHERE depends_on_id = ?",
                &[SqliteValue::from(id)],
            )?;
            conn.execute_with_params(
                "DELETE FROM events WHERE issue_id = ?",
                &[SqliteValue::from(id)],
            )?;
            conn.execute_with_params(
                "DELETE FROM dirty_issues WHERE issue_id = ?",
                &[SqliteValue::from(id)],
            )?;
            conn.execute_with_params(
                "DELETE FROM export_hashes WHERE issue_id = ?",
                &[SqliteValue::from(id)],
            )?;
            conn.execute_with_params(
                "DELETE FROM blocked_issues_cache WHERE issue_id = ?",
                &[SqliteValue::from(id)],
            )?;
            conn.execute_with_params(
                "DELETE FROM child_counters WHERE parent_id = ?",
                &[SqliteValue::from(id)],
            )?;
            conn.execute_with_params("DELETE FROM issues WHERE id = ?", &[SqliteValue::from(id)])?;

            ctx.invalidate_cache();
            ctx.force_flush = true;

            Ok(())
        })
    }

    /// List the IDs of all tombstoned (soft-deleted) issues, sorted.
    ///
    /// Used by `br delete --hard` (invoked with no explicit IDs) to purge every
    /// tombstone from the store in one pass (#367).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn list_tombstone_ids(&self) -> Result<Vec<String>> {
        let rows = self
            .conn
            .query("SELECT id FROM issues WHERE status = 'tombstone' ORDER BY id")?;
        let mut ids = Vec::with_capacity(rows.len());
        for row in &rows {
            if let Some(id) = row.get(0).and_then(SqliteValue::as_text) {
                ids.push(id.to_string());
            }
        }
        Ok(ids)
    }

    /// Get an issue by ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_issue(&self, id: &str) -> Result<Option<Issue>> {
        Self::get_issue_from_conn(&self.conn, id)
    }

    /// Get metadata for all issues to optimize import collision detection.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_all_issues_metadata(&self) -> Result<Vec<IssueMetadata>> {
        let sql = "SELECT id, external_ref, content_hash, updated_at, status FROM issues";
        let rows = self.conn.query(sql)?;
        let mut metas = Vec::with_capacity(rows.len());
        for row in &rows {
            let id = row
                .get(0)
                .and_then(SqliteValue::as_text)
                .unwrap_or_default()
                .to_string();
            let external_ref = row
                .get(1)
                .and_then(SqliteValue::as_text)
                .map(str::to_string);
            let content_hash = row
                .get(2)
                .and_then(SqliteValue::as_text)
                .map(str::to_string);
            let updated_at = parse_datetime_value(row.get(3))?;
            let status = parse_status(row.get(4).and_then(SqliteValue::as_text));

            metas.push(IssueMetadata {
                id,
                external_ref,
                content_hash,
                updated_at,
                status,
            });
        }
        Ok(metas)
    }

    /// Return issue IDs with the requested status without parsing unrelated
    /// issue metadata columns.
    ///
    /// This is intentionally narrower than `get_all_issues_metadata()`: callers
    /// that only need tombstone IDs should not fail because some other issue row
    /// has a malformed timestamp or other metadata decoding problem.
    ///
    /// # Errors
    ///
    /// Returns an error if the query fails.
    pub fn get_issue_ids_by_status(&self, status: &Status) -> Result<Vec<String>> {
        let rows = self.conn.query_with_params(
            "SELECT id FROM issues WHERE status = ? ORDER BY id",
            &[SqliteValue::from(status.as_str())],
        )?;

        Ok(rows
            .iter()
            .filter_map(|row| row.get(0).and_then(SqliteValue::as_text))
            .map(str::to_string)
            .collect())
    }

    fn get_issue_from_conn(conn: &Connection, id: &str) -> Result<Option<Issue>> {
        let sql = r"
            SELECT id, content_hash, title, description, design,
                   acceptance_criteria, notes, status, priority, issue_type,
                   assignee, owner, estimated_minutes, created_at, created_by,
                   updated_at, closed_at, close_reason, closed_by_session,
                   due_at, defer_until, external_ref, source_system, source_repo,
                   deleted_at, deleted_by, delete_reason, original_type,
                   compaction_level, compacted_at, compacted_at_commit, original_size,
                   sender, ephemeral, pinned, is_template, source_repo_path, agent_context
            FROM issues
            WHERE id = ?
        ";
        let row = match conn.query_row_with_params(sql, &[SqliteValue::from(id)]) {
            Ok(row) => row,
            Err(FrankenError::QueryReturnedNoRows) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let issue = Self::issue_from_row(&row)?;
        if issue.id != id {
            return Err(BeadsError::internal(format!(
                "storage consistency: get_issue_from_conn requested {id:?} but row returned id {:?}",
                issue.id
            )));
        }
        Ok(Some(issue))
    }

    /// Get multiple issues by ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_issues_by_ids(&self, ids: &[String]) -> Result<Vec<Issue>> {
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
                         sender, ephemeral, pinned, is_template, source_repo_path, agent_context
                  FROM issues WHERE id IN ({})",
                placeholders.join(",")
            );

            let params: Vec<SqliteValue> = chunk
                .iter()
                .map(|s| SqliteValue::from(s.as_str()))
                .collect();

            let rows = self.conn.query_with_params(&sql, &params)?;
            for row in &rows {
                issues.push(Self::issue_from_row(row)?);
            }
        }

        Ok(issues)
    }

    fn label_filter_candidate_ids(
        &self,
        labels_and: &[String],
        labels_or: &[String],
    ) -> Result<Option<Vec<String>>> {
        let all_label_ids = self.query_issue_ids_with_all_labels(labels_and)?;
        let any_label_ids = self.query_issue_ids_with_any_label(labels_or)?;

        let ids = match (all_label_ids, any_label_ids) {
            (None, None) => return Ok(None),
            (Some(ids), None) | (None, Some(ids)) => ids,
            (Some(all_ids), Some(any_ids)) => {
                let any_id_set: HashSet<&str> = any_ids.iter().map(String::as_str).collect();
                all_ids
                    .into_iter()
                    .filter(|issue_id| any_id_set.contains(issue_id.as_str()))
                    .collect()
            }
        };

        Ok(Some(ids))
    }

    fn query_issue_ids_with_all_labels(&self, labels: &[String]) -> Result<Option<Vec<String>>> {
        let unique_labels = unique_label_refs(labels);
        let ids = match unique_labels.as_slice() {
            [] => return Ok(None),
            [label] => self.query_issue_ids_from_label_sql(
                "SELECT issue_id FROM labels WHERE label = ? ORDER BY issue_id",
                &[SqliteValue::from(label.as_str())],
            )?,
            _ => {
                let placeholders: Vec<String> =
                    unique_labels.iter().map(|_| "?".to_string()).collect();
                let sql = format!(
                    "SELECT issue_id
                     FROM labels
                     WHERE label IN ({})
                     GROUP BY issue_id
                     HAVING COUNT(DISTINCT label) = ?
                     ORDER BY issue_id",
                    placeholders.join(",")
                );
                let unique_label_count = unique_labels.len();
                let mut params = Vec::with_capacity(unique_labels.len() + 1);
                for label in unique_labels {
                    params.push(SqliteValue::from(label.as_str()));
                }
                params.push(SqliteValue::from(
                    i64::try_from(unique_label_count).unwrap_or(i64::MAX),
                ));
                self.query_issue_ids_from_label_sql(&sql, &params)?
            }
        };

        Ok(Some(ids))
    }

    fn query_issue_ids_with_any_label(&self, labels: &[String]) -> Result<Option<Vec<String>>> {
        if labels.is_empty() {
            return Ok(None);
        }

        let placeholders: Vec<String> = labels.iter().map(|_| "?".to_string()).collect();
        let sql = format!(
            "SELECT DISTINCT issue_id
             FROM labels
             WHERE label IN ({})
             ORDER BY issue_id",
            placeholders.join(",")
        );
        let params: Vec<SqliteValue> = labels
            .iter()
            .map(|label| SqliteValue::from(label.as_str()))
            .collect();
        Ok(Some(self.query_issue_ids_from_label_sql(&sql, &params)?))
    }

    fn query_issue_ids_from_label_sql(
        &self,
        sql: &str,
        params: &[SqliteValue],
    ) -> Result<Vec<String>> {
        self.query_issue_ids_from_sql(sql, params)
    }

    fn query_issue_ids_from_sql(&self, sql: &str, params: &[SqliteValue]) -> Result<Vec<String>> {
        let rows = self.conn.query_with_params(sql, params)?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                row.get(0)
                    .and_then(SqliteValue::as_text)
                    .map(str::to_string)
            })
            .collect())
    }

    /// List issues with optional filters.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    #[allow(clippy::too_many_lines)]
    pub fn list_issues(&self, filters: &ListFilters) -> Result<Vec<Issue>> {
        if let Some(limit) = default_visible_limited_page_limit(filters) {
            return self.list_default_visible_limited_page(filters.include_deferred, limit);
        }

        let sort_default_in_rust = should_sort_list_default_in_rust(filters);
        let mut sql = String::from(
            r"SELECT id, content_hash, title, description, design, acceptance_criteria, notes,
                     status, priority, issue_type, assignee, owner, estimated_minutes,
                     created_at, created_by, updated_at, closed_at, close_reason, closed_by_session,
                     due_at, defer_until, external_ref, source_system, source_repo,
                     deleted_at, deleted_by, delete_reason, original_type,
                     compaction_level, compacted_at, compacted_at_commit, original_size,
                     sender, ephemeral, pinned, is_template, source_repo_path, agent_context",
        );

        let mut params: Vec<SqliteValue> = Vec::new();

        let labels_and = filters.labels.as_deref().unwrap_or(&[]);
        let labels_or = filters.labels_or.as_deref().unwrap_or(&[]);
        let label_candidate_ids = if labels_and.is_empty() && labels_or.is_empty() {
            None
        } else {
            self.label_filter_candidate_ids(labels_and, labels_or)?
        };
        if label_candidate_ids.as_ref().is_some_and(Vec::is_empty) {
            return Ok(Vec::new());
        }
        if self.redundant_default_visible_single_label_filter(
            filters,
            label_candidate_ids.as_deref(),
        )? {
            let mut filters_without_redundant_label = filters.clone();
            filters_without_redundant_label.labels = None;
            return self.list_issues(&filters_without_redundant_label);
        }
        sql.push_str(" FROM issues WHERE 1=1");
        if let Some(ref issue_ids) = label_candidate_ids {
            append_issue_id_membership_filter(&mut sql, &mut params, issue_ids);
        }

        if let Some(ref statuses) = filters.statuses
            && !statuses.is_empty()
        {
            let placeholders: Vec<String> = statuses.iter().map(|_| "?".to_string()).collect();
            let _ = write!(sql, " AND status IN ({}) ", placeholders.join(","));
            for s in statuses {
                params.push(SqliteValue::from(s.as_str()));
            }
        }

        if let Some(ref types) = filters.types
            && !types.is_empty()
        {
            let placeholders: Vec<String> = types.iter().map(|_| "?".to_string()).collect();
            let _ = write!(sql, " AND issue_type IN ({}) ", placeholders.join(","));
            for t in types {
                params.push(SqliteValue::from(t.as_str()));
            }
        }

        if let Some(ref priorities) = filters.priorities
            && !priorities.is_empty()
        {
            let placeholders: Vec<String> = priorities.iter().map(|_| "?".to_string()).collect();
            let _ = write!(sql, " AND priority IN ({}) ", placeholders.join(","));
            for p in priorities {
                params.push(SqliteValue::from(i64::from(p.0)));
            }
        }

        if let Some(ref assignee) = filters.assignee {
            sql.push_str(" AND assignee = ?");
            params.push(SqliteValue::from(assignee.as_str()));
        }

        if filters.unassigned {
            sql.push_str(" AND (assignee IS NULL OR assignee = '')");
        }

        if !filters.include_closed {
            if filters.include_deferred {
                sql.push_str(" AND status NOT IN ('closed', 'tombstone')");
            } else {
                sql.push_str(" AND status NOT IN ('closed', 'tombstone', 'deferred')");
            }
        } else if filters.statuses.as_ref().is_none_or(Vec::is_empty) {
            // When including closed issues, still exclude tombstones (deleted issues) by default
            // unless specific statuses were requested.
            sql.push_str(" AND status != 'tombstone'");
        }

        if !filters.include_templates {
            sql.push_str(" AND (is_template = 0 OR is_template IS NULL)");
        }

        if let Some(ref title_contains) = filters.title_contains {
            sql.push_str(" AND title LIKE ? ESCAPE '\\'");
            let escaped = escape_like_pattern(title_contains);
            params.push(SqliteValue::from(format!("%{escaped}%")));
        }

        if let Some(ts) = filters.updated_before {
            sql.push_str(" AND updated_at <= ?");
            params.push(SqliteValue::from(ts.to_rfc3339()));
        }

        if let Some(ts) = filters.updated_after {
            sql.push_str(" AND updated_at >= ?");
            params.push(SqliteValue::from(ts.to_rfc3339()));
        }

        if !sort_default_in_rust {
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
                            " ORDER BY priority {order}, created_at {secondary_order}, id ASC"
                        );
                    }
                    "created_at" | "created" => {
                        let order = if filters.reverse { "ASC" } else { "DESC" };
                        let _ = write!(sql, " ORDER BY created_at {order}, id ASC");
                    }
                    "updated_at" | "updated" => {
                        let order = if filters.reverse { "ASC" } else { "DESC" };
                        let _ = write!(sql, " ORDER BY updated_at {order}, id ASC");
                    }
                    "title" => {
                        // Case-insensitive sort for title
                        let _ = write!(sql, " ORDER BY title COLLATE NOCASE {order}, id ASC");
                    }
                    _ => {
                        // Default fallback
                        sql.push_str(" ORDER BY priority ASC, created_at DESC, id ASC");
                    }
                }
            } else if filters.reverse {
                sql.push_str(" ORDER BY priority DESC, created_at ASC, id ASC");
            } else {
                sql.push_str(" ORDER BY priority ASC, created_at DESC, id ASC");
            }
        }

        match (filters.limit, filters.offset) {
            (Some(limit), offset) if limit > 0 => {
                let _ = write!(sql, " LIMIT {limit}");
                if let Some(offset) = offset
                    && offset > 0
                {
                    let _ = write!(sql, " OFFSET {offset}");
                }
            }
            (_, Some(offset)) if offset > 0 => {
                let _ = write!(sql, " LIMIT -1 OFFSET {offset}");
            }
            _ => {}
        }

        let rows = self.conn.query_with_params(&sql, &params)?;
        let mut issues = Vec::with_capacity(rows.len());
        for row in &rows {
            issues.push(Self::issue_from_row(row)?);
        }
        if sort_default_in_rust {
            sort_list_default(&mut issues);
        }

        Ok(issues)
    }

    fn list_default_visible_limited_page(
        &self,
        include_deferred: bool,
        limit: usize,
    ) -> Result<Vec<Issue>> {
        let status_filter = if include_deferred {
            "status NOT IN ('closed', 'tombstone')"
        } else {
            "status NOT IN ('closed', 'tombstone', 'deferred')"
        };
        let mut issues = Vec::with_capacity(limit);

        for priority in Priority::CRITICAL.0..=Priority::BACKLOG.0 {
            let remaining = limit.saturating_sub(issues.len());
            if remaining == 0 {
                break;
            }

            let sql = format!(
                r"SELECT id, content_hash, title, description, design, acceptance_criteria, notes,
                         status, priority, issue_type, assignee, owner, estimated_minutes,
                         created_at, created_by, updated_at, closed_at, close_reason, closed_by_session,
                         due_at, defer_until, external_ref, source_system, source_repo,
                         deleted_at, deleted_by, delete_reason, original_type,
                         compaction_level, compacted_at, compacted_at_commit, original_size,
                         sender, ephemeral, pinned, is_template, source_repo_path, agent_context
                  FROM issues
                  WHERE {status_filter}
                    AND (is_template = 0 OR is_template IS NULL)
                    AND priority = ?
                  ORDER BY created_at DESC, id ASC
                  LIMIT {remaining}"
            );
            let rows = self
                .conn
                .query_with_params(&sql, &[SqliteValue::from(i64::from(priority))])?;
            for row in &rows {
                issues.push(Self::issue_from_row(row)?);
            }
        }

        Ok(issues)
    }

    /// List short text/table command issues without hydrating fields the renderer never inspects.
    ///
    /// This is intentionally narrow and falls back to `list_issues` if the
    /// filter shape expands beyond the unlimited default short text path.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn list_text_issues_for_command_output(&self, filters: &ListFilters) -> Result<Vec<Issue>> {
        let unsupported_filter = filters
            .statuses
            .as_ref()
            .is_some_and(|statuses| !statuses.is_empty())
            || filters
                .labels
                .as_ref()
                .is_some_and(|labels| !labels.is_empty())
            || filters
                .labels_or
                .as_ref()
                .is_some_and(|labels| !labels.is_empty())
            || filters
                .types
                .as_ref()
                .is_some_and(|types| !types.is_empty())
            || filters
                .priorities
                .as_ref()
                .is_some_and(|priorities| !priorities.is_empty())
            || filters.assignee.is_some()
            || filters.unassigned
            || filters.include_closed
            || !filters.include_deferred
            || filters.include_templates
            || filters.title_contains.is_some()
            || filters.updated_before.is_some()
            || filters.updated_after.is_some()
            || filters.sort.is_some()
            || filters.reverse;
        if unsupported_filter {
            return self.list_issues(filters);
        }

        if let Some(limit) = filters.limit
            && limit > 0
        {
            return self.list_text_issues_by_priority_window(filters, limit);
        }

        let mut sql = String::from(
            "SELECT id, title, status, priority, issue_type, created_at, updated_at
             FROM issues
             WHERE status NOT IN ('closed', 'tombstone')
               AND (is_template = 0 OR is_template IS NULL)
             ORDER BY COALESCE(priority, 2) ASC, created_at DESC, id ASC",
        );
        match (filters.limit, filters.offset) {
            (Some(limit), offset) if limit > 0 => {
                let _ = write!(sql, " LIMIT {limit}");
                if let Some(offset) = offset
                    && offset > 0
                {
                    let _ = write!(sql, " OFFSET {offset}");
                }
            }
            (_, Some(offset)) if offset > 0 => {
                let _ = write!(sql, " LIMIT -1 OFFSET {offset}");
            }
            _ => {}
        }
        let rows = self.conn.query(&sql)?;
        let mut issues = Vec::with_capacity(rows.len());
        for row in &rows {
            issues.push(Self::command_summary_issue_from_row(row)?);
        }
        Ok(issues)
    }

    fn list_text_issues_by_priority_window(
        &self,
        filters: &ListFilters,
        limit: usize,
    ) -> Result<Vec<Issue>> {
        let mut offset = filters.offset.unwrap_or(0);
        let mut issues = Vec::with_capacity(limit);
        for priority in Priority::CRITICAL.0..=Priority::BACKLOG.0 {
            let remaining = limit - issues.len();
            if remaining == 0 {
                break;
            }

            let query_limit = remaining.saturating_add(offset);
            let rows = self.conn.query_with_params(
                "SELECT id, title, status, priority, issue_type, created_at, updated_at
                 FROM issues INDEXED BY idx_issues_list_active_order
                 WHERE status NOT IN ('closed', 'tombstone')
                   AND (is_template = 0 OR is_template IS NULL)
                   AND priority = ?
                 ORDER BY created_at DESC, id ASC
                 LIMIT ?",
                &[
                    SqliteValue::from(i64::from(priority)),
                    SqliteValue::from(i64::try_from(query_limit).unwrap_or(i64::MAX)),
                ],
            )?;

            if offset >= rows.len() {
                offset -= rows.len();
                continue;
            }

            for row in rows.iter().skip(offset) {
                if issues.len() == limit {
                    break;
                }
                issues.push(Self::command_summary_issue_from_row(row)?);
            }
            offset = 0;
        }
        Ok(issues)
    }

    /// List stale command issues without hydrating fields stale output never renders.
    ///
    /// This is intentionally narrow and falls back to `list_issues` if the
    /// filter shape expands beyond what `br stale` currently uses.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn list_stale_issues_for_command_output(
        &self,
        filters: &ListFilters,
    ) -> Result<Vec<Issue>> {
        let unsupported_filter = filters
            .labels
            .as_ref()
            .is_some_and(|labels| !labels.is_empty())
            || filters
                .labels_or
                .as_ref()
                .is_some_and(|labels| !labels.is_empty())
            || filters
                .types
                .as_ref()
                .is_some_and(|types| !types.is_empty())
            || filters
                .priorities
                .as_ref()
                .is_some_and(|priorities| !priorities.is_empty())
            || filters.assignee.is_some()
            || filters.unassigned
            || filters.title_contains.is_some()
            || filters.updated_after.is_some()
            || filters.sort.as_deref() != Some("updated_at")
            || !filters.reverse;
        if unsupported_filter {
            return self.list_issues(filters);
        }

        let mut sql = String::from(
            "SELECT id, title, status, priority, issue_type, assignee, created_at, updated_at
             FROM issues WHERE 1=1",
        );
        let mut params = Vec::new();

        if let Some(ref statuses) = filters.statuses
            && !statuses.is_empty()
        {
            let placeholders: Vec<String> = statuses.iter().map(|_| "?".to_string()).collect();
            let _ = write!(sql, " AND status IN ({}) ", placeholders.join(","));
            for status in statuses {
                params.push(SqliteValue::from(status.as_str()));
            }
        }

        if !filters.include_closed {
            if filters.include_deferred {
                sql.push_str(" AND status NOT IN ('closed', 'tombstone')");
            } else {
                sql.push_str(" AND status NOT IN ('closed', 'tombstone', 'deferred')");
            }
        } else if filters.statuses.as_ref().is_none_or(Vec::is_empty) {
            sql.push_str(" AND status != 'tombstone'");
        }

        if !filters.include_templates {
            sql.push_str(" AND (is_template = 0 OR is_template IS NULL)");
        }

        if let Some(updated_before) = filters.updated_before {
            sql.push_str(" AND updated_at <= ?");
            params.push(SqliteValue::from(updated_before.to_rfc3339()));
        }

        sql.push_str(" ORDER BY updated_at ASC, id ASC");

        match (filters.limit, filters.offset) {
            (Some(limit), offset) if limit > 0 => {
                let _ = write!(sql, " LIMIT {limit}");
                if let Some(offset) = offset
                    && offset > 0
                {
                    let _ = write!(sql, " OFFSET {offset}");
                }
            }
            (_, Some(offset)) if offset > 0 => {
                let _ = write!(sql, " LIMIT -1 OFFSET {offset}");
            }
            _ => {}
        }

        let rows = self.conn.query_with_params(&sql, &params)?;
        let mut issues = Vec::with_capacity(rows.len());
        for row in &rows {
            issues.push(Self::stale_command_issue_from_row(row)?);
        }
        Ok(issues)
    }

    /// List lint command issues without hydrating fields lint never inspects.
    ///
    /// This is intentionally narrow and falls back to `list_issues` if the
    /// filter shape expands beyond what `br lint` currently uses.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn list_lint_issues_for_command_output(&self, filters: &ListFilters) -> Result<Vec<Issue>> {
        let unsupported_filter = filters
            .labels
            .as_ref()
            .is_some_and(|labels| !labels.is_empty())
            || filters
                .labels_or
                .as_ref()
                .is_some_and(|labels| !labels.is_empty())
            || filters
                .priorities
                .as_ref()
                .is_some_and(|priorities| !priorities.is_empty())
            || filters.assignee.is_some()
            || filters.unassigned
            || filters.title_contains.is_some()
            || filters.updated_before.is_some()
            || filters.updated_after.is_some()
            || filters.limit.is_some()
            || filters.offset.is_some()
            || filters.sort.is_some()
            || filters.reverse;
        if unsupported_filter {
            return self.list_issues(filters);
        }

        let mut sql = String::from(
            "SELECT id, title, description, status, issue_type, created_at, updated_at
             FROM issues WHERE 1=1",
        );
        let mut params = Vec::new();

        if let Some(ref statuses) = filters.statuses
            && !statuses.is_empty()
        {
            let placeholders: Vec<String> = statuses.iter().map(|_| "?".to_string()).collect();
            let _ = write!(sql, " AND status IN ({}) ", placeholders.join(","));
            for status in statuses {
                params.push(SqliteValue::from(status.as_str()));
            }
        }

        if let Some(ref types) = filters.types
            && !types.is_empty()
        {
            let placeholders: Vec<String> = types.iter().map(|_| "?".to_string()).collect();
            let _ = write!(sql, " AND issue_type IN ({}) ", placeholders.join(","));
            for issue_type in types {
                params.push(SqliteValue::from(issue_type.as_str()));
            }
        }

        if !filters.include_closed {
            if filters.include_deferred {
                sql.push_str(" AND status NOT IN ('closed', 'tombstone')");
            } else {
                sql.push_str(" AND status NOT IN ('closed', 'tombstone', 'deferred')");
            }
        } else if filters.statuses.as_ref().is_none_or(Vec::is_empty) {
            sql.push_str(" AND status != 'tombstone'");
        }

        if !filters.include_templates {
            sql.push_str(" AND (is_template = 0 OR is_template IS NULL)");
        }

        sql.push_str(" ORDER BY priority ASC, created_at DESC, id ASC");

        let rows = self.conn.query_with_params(&sql, &params)?;
        let mut issues = Vec::with_capacity(rows.len());
        for row in &rows {
            issues.push(Self::lint_command_issue_from_row(row)?);
        }
        Ok(issues)
    }

    /// List orphan-scan candidate issues without hydrating unused full issue fields.
    ///
    /// This is intentionally narrow and falls back to `list_issues` if the
    /// filter shape expands beyond what `br orphans` currently uses.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn list_orphan_candidate_issues_for_command_output(
        &self,
        filters: &ListFilters,
    ) -> Result<Vec<Issue>> {
        let expected_statuses = matches!(
            filters.statuses.as_deref(),
            Some([Status::Open, Status::InProgress])
        );
        let unsupported_filter = !expected_statuses
            || filters
                .labels
                .as_ref()
                .is_some_and(|labels| !labels.is_empty())
            || filters
                .labels_or
                .as_ref()
                .is_some_and(|labels| !labels.is_empty())
            || filters
                .types
                .as_ref()
                .is_some_and(|types| !types.is_empty())
            || filters
                .priorities
                .as_ref()
                .is_some_and(|priorities| !priorities.is_empty())
            || filters.assignee.is_some()
            || filters.unassigned
            || filters.include_closed
            || filters.include_deferred
            || filters.include_templates
            || filters.title_contains.is_some()
            || filters.updated_before.is_some()
            || filters.updated_after.is_some()
            || filters.limit.is_some()
            || filters.offset.is_some()
            || filters.sort.is_some()
            || filters.reverse;
        if unsupported_filter {
            return self.list_issues(filters);
        }

        let rows = self.conn.query(
            "SELECT id, title, status, priority, issue_type, created_at, updated_at
             FROM issues
             WHERE status IN ('open', 'in_progress')
               AND (is_template = 0 OR is_template IS NULL)
             ORDER BY COALESCE(priority, 2) ASC, created_at DESC, id ASC",
        )?;
        let mut issues = Vec::with_capacity(rows.len());
        for row in &rows {
            issues.push(Self::command_summary_issue_from_row(row)?);
        }
        Ok(issues)
    }

    /// List graph command issues without hydrating fields graph rendering never inspects.
    ///
    /// This is intentionally narrow and falls back to `list_issues` if the
    /// filter shape expands beyond what `br graph --all` currently uses.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn list_graph_issues_for_command_output(
        &self,
        filters: &ListFilters,
    ) -> Result<Vec<Issue>> {
        let unsupported_filter = filters
            .statuses
            .as_ref()
            .is_some_and(|statuses| !statuses.is_empty())
            || filters
                .labels
                .as_ref()
                .is_some_and(|labels| !labels.is_empty())
            || filters
                .labels_or
                .as_ref()
                .is_some_and(|labels| !labels.is_empty())
            || filters
                .types
                .as_ref()
                .is_some_and(|types| !types.is_empty())
            || filters
                .priorities
                .as_ref()
                .is_some_and(|priorities| !priorities.is_empty())
            || filters.assignee.is_some()
            || filters.unassigned
            || filters.include_closed
            || !filters.include_deferred
            || filters.include_templates
            || filters.title_contains.is_some()
            || filters.updated_before.is_some()
            || filters.updated_after.is_some()
            || filters.limit.is_some()
            || filters.offset.is_some()
            || filters.sort.is_some()
            || filters.reverse;
        if unsupported_filter {
            return self.list_issues(filters);
        }

        let rows = self.conn.query(
            "SELECT id, title, status, priority, issue_type, created_at, updated_at
             FROM issues
             WHERE status NOT IN ('closed', 'tombstone')
               AND (is_template = 0 OR is_template IS NULL)
             ORDER BY COALESCE(priority, 2) ASC, created_at DESC, id ASC",
        )?;
        let mut issues = Vec::with_capacity(rows.len());
        for row in &rows {
            issues.push(Self::command_summary_issue_from_row(row)?);
        }
        Ok(issues)
    }

    /// Get lean issue rows for stats computation without hydrating large text fields.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn list_stats_issues(&self) -> Result<Vec<StatsIssueRow>> {
        let rows = self.conn.query(
            r"SELECT id, status, priority, issue_type, assignee, created_at, closed_at,
                     defer_until, ephemeral, pinned, is_template
              FROM issues",
        )?;
        let mut issues = Vec::with_capacity(rows.len());
        for row in &rows {
            issues.push(Self::stats_issue_from_row(row)?);
        }
        Ok(issues)
    }

    /// Get the narrowest issue rows needed for stats summary computation.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn list_stats_summary_issues(&self) -> Result<Vec<StatsIssueRow>> {
        let rows = self.conn.query(
            r"SELECT id, status, issue_type, created_at, closed_at,
                     defer_until, ephemeral, pinned, is_template
              FROM issues",
        )?;
        let mut issues = Vec::with_capacity(rows.len());
        for row in &rows {
            issues.push(Self::stats_summary_issue_from_row(row)?);
        }
        Ok(issues)
    }

    /// Get lean closed issue rows for changelog rendering.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails or a stored timestamp is invalid.
    pub(crate) fn list_changelog_issues(&self) -> Result<Vec<ChangelogIssueRow>> {
        let rows = self.conn.query(
            r"SELECT id, title, priority, issue_type, created_at, closed_at
              FROM issues
              WHERE status = 'closed'
                AND (is_template = 0 OR is_template IS NULL)",
        )?;
        let mut issues = Vec::with_capacity(rows.len());
        for row in &rows {
            issues.push(Self::changelog_issue_from_row(row)?);
        }
        issues.sort_unstable_by(|left, right| {
            left.priority
                .cmp(&right.priority)
                .then_with(|| right.created_at.cmp(&left.created_at))
                .then_with(|| left.id.cmp(&right.id))
        });
        Ok(issues)
    }

    /// Count issues matching the given filters (no LIMIT/OFFSET applied).
    ///
    /// Runs a `SELECT COUNT(*)` using the same WHERE conditions as [`list_issues`],
    /// without ORDER BY, LIMIT, or OFFSET clauses. Used to compute pagination metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    #[allow(clippy::too_many_lines)]
    pub fn count_issues_with_filters(&self, filters: &ListFilters) -> Result<usize> {
        if let Some(label) = default_visible_single_label_count_filter(filters) {
            return self.count_default_visible_single_label(filters.include_deferred, label);
        }

        let mut sql = String::from("SELECT COUNT(*)");

        let mut params: Vec<SqliteValue> = Vec::new();

        let labels_and = filters.labels.as_deref().unwrap_or(&[]);
        let labels_or = filters.labels_or.as_deref().unwrap_or(&[]);
        let label_filters_can_use_uncorrelated_in =
            filters.statuses.as_ref().is_none_or(Vec::is_empty)
                && filters.types.as_ref().is_none_or(Vec::is_empty)
                && filters.priorities.as_ref().is_none_or(Vec::is_empty)
                && filters.assignee.is_none()
                && filters.title_contains.is_none()
                && filters.updated_before.is_none()
                && filters.updated_after.is_none();
        let label_candidate_ids = if label_filters_can_use_uncorrelated_in {
            None
        } else {
            self.label_filter_candidate_ids(labels_and, labels_or)?
        };
        if label_candidate_ids.as_ref().is_some_and(Vec::is_empty) {
            return Ok(0);
        }
        sql.push_str(" FROM issues WHERE 1=1");
        if label_filters_can_use_uncorrelated_in {
            append_label_membership_filters(&mut sql, &mut params, labels_and, labels_or);
        } else if let Some(ref issue_ids) = label_candidate_ids {
            append_issue_id_membership_filter(&mut sql, &mut params, issue_ids);
        }

        if let Some(ref statuses) = filters.statuses
            && !statuses.is_empty()
        {
            let placeholders: Vec<String> = statuses.iter().map(|_| "?".to_string()).collect();
            let _ = write!(sql, " AND status IN ({}) ", placeholders.join(","));
            for s in statuses {
                params.push(SqliteValue::from(s.as_str()));
            }
        }

        if let Some(ref types) = filters.types
            && !types.is_empty()
        {
            let placeholders: Vec<String> = types.iter().map(|_| "?".to_string()).collect();
            let _ = write!(sql, " AND issue_type IN ({}) ", placeholders.join(","));
            for t in types {
                params.push(SqliteValue::from(t.as_str()));
            }
        }

        if let Some(ref priorities) = filters.priorities
            && !priorities.is_empty()
        {
            let placeholders: Vec<String> = priorities.iter().map(|_| "?".to_string()).collect();
            let _ = write!(sql, " AND priority IN ({}) ", placeholders.join(","));
            for p in priorities {
                params.push(SqliteValue::from(i64::from(p.0)));
            }
        }

        if let Some(ref assignee) = filters.assignee {
            sql.push_str(" AND assignee = ?");
            params.push(SqliteValue::from(assignee.as_str()));
        }

        if filters.unassigned {
            sql.push_str(" AND (assignee IS NULL OR assignee = '')");
        }

        if !filters.include_closed {
            if filters.include_deferred {
                sql.push_str(" AND status NOT IN ('closed', 'tombstone')");
            } else {
                sql.push_str(" AND status NOT IN ('closed', 'tombstone', 'deferred')");
            }
        } else if filters.statuses.as_ref().is_none_or(Vec::is_empty) {
            sql.push_str(" AND status != 'tombstone'");
        }

        if !filters.include_templates {
            sql.push_str(" AND (is_template = 0 OR is_template IS NULL)");
        }

        if let Some(ref title_contains) = filters.title_contains {
            sql.push_str(" AND title LIKE ? ESCAPE '\\'");
            let escaped = escape_like_pattern(title_contains);
            params.push(SqliteValue::from(format!("%{escaped}%")));
        }

        if let Some(ts) = filters.updated_before {
            sql.push_str(" AND updated_at <= ?");
            params.push(SqliteValue::from(ts.to_rfc3339()));
        }

        if let Some(ts) = filters.updated_after {
            sql.push_str(" AND updated_at >= ?");
            params.push(SqliteValue::from(ts.to_rfc3339()));
        }

        let row = self.conn.query_row_with_params(&sql, &params)?;
        let count = row.get(0).and_then(SqliteValue::as_integer).unwrap_or(0);
        Ok(usize::try_from(count).unwrap_or(0))
    }

    fn count_default_visible_single_label(
        &self,
        include_deferred: bool,
        label: &str,
    ) -> Result<usize> {
        let status_filter = if include_deferred {
            "status NOT IN ('closed', 'tombstone')"
        } else {
            "status NOT IN ('closed', 'tombstone', 'deferred')"
        };
        let issue_rows = self.conn.query(&format!(
            "SELECT id
             FROM issues
             WHERE {status_filter}
               AND (is_template = 0 OR is_template IS NULL)"
        ))?;
        if issue_rows.is_empty() {
            return Ok(0);
        }

        let mut visible_ids = HashSet::with_capacity(issue_rows.len());
        for row in &issue_rows {
            let issue_id = row
                .get(0)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            if !issue_id.is_empty() {
                visible_ids.insert(issue_id);
            }
        }

        let label_rows = self.conn.query_with_params(
            "SELECT issue_id FROM labels WHERE label = ?",
            &[SqliteValue::from(label)],
        )?;
        Ok(label_rows
            .iter()
            .filter_map(|row| row.get(0).and_then(SqliteValue::as_text))
            .filter(|issue_id| visible_ids.contains(*issue_id))
            .count())
    }

    fn single_label_covers_default_visible_issues(
        &self,
        include_deferred: bool,
        label: &str,
    ) -> Result<bool> {
        let total = self.count_default_visible_issues(include_deferred)?;
        let labeled = self.count_default_visible_single_label(include_deferred, label)?;
        Ok(labeled == total)
    }

    fn count_default_visible_issues(&self, include_deferred: bool) -> Result<usize> {
        let status_filter = if include_deferred {
            "status NOT IN ('closed', 'tombstone')"
        } else {
            "status NOT IN ('closed', 'tombstone', 'deferred')"
        };
        let row = self.conn.query_row(&format!(
            "SELECT COUNT(*)
             FROM issues
             WHERE {status_filter}
               AND (is_template = 0 OR is_template IS NULL)"
        ))?;
        let count = row.get(0).and_then(SqliteValue::as_integer).unwrap_or(0);
        Ok(usize::try_from(count).unwrap_or(0))
    }

    fn redundant_default_visible_single_label_filter(
        &self,
        filters: &ListFilters,
        label_candidate_ids: Option<&[String]>,
    ) -> Result<bool> {
        let Some(label) = default_visible_single_label_count_filter(filters) else {
            return Ok(false);
        };
        let Some(issue_ids) = label_candidate_ids else {
            return Ok(false);
        };
        if issue_ids.len() < REDUNDANT_LABEL_COVERAGE_MIN_CANDIDATES {
            return Ok(false);
        }
        self.single_label_covers_default_visible_issues(filters.include_deferred, label)
    }

    /// Count default-visible issues grouped by status.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn count_default_visible_statuses(&self) -> Result<Vec<(String, usize)>> {
        self.count_default_visible_text_groups("status", "status")
    }

    /// Count default-visible issues grouped by issue type.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn count_default_visible_types(&self) -> Result<Vec<(String, usize)>> {
        self.count_default_visible_text_groups("issue_type", "issue_type")
    }

    /// Count default-visible issues grouped by assignee.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn count_default_visible_assignees(&self) -> Result<Vec<(String, usize)>> {
        self.count_default_visible_text_groups(
            "COALESCE(NULLIF(assignee, ''), '(unassigned)')",
            "COALESCE(NULLIF(assignee, ''), '(unassigned)')",
        )
    }

    /// Count default-visible issues grouped by priority.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn count_default_visible_priorities(&self) -> Result<Vec<(String, usize)>> {
        let rows = self.conn.query(
            "SELECT priority, COUNT(*)
             FROM issues
             WHERE status NOT IN ('closed', 'tombstone', 'deferred')
               AND (is_template = 0 OR is_template IS NULL)
             GROUP BY priority
             ORDER BY priority",
        )?;
        rows.iter()
            .map(|row| {
                let priority = row
                    .get(0)
                    .and_then(SqliteValue::as_integer)
                    .and_then(|value| i32::try_from(value).ok())
                    .map(Priority)
                    .unwrap_or_default();
                let count = row.get(1).and_then(SqliteValue::as_integer).unwrap_or(0);
                Ok((priority.to_string(), usize::try_from(count).unwrap_or(0)))
            })
            .collect()
    }

    /// Count default-visible issues grouped by label.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn count_default_visible_labels(&self) -> Result<(usize, Vec<(String, usize)>)> {
        let issue_rows = self.conn.query(
            "SELECT id
             FROM issues
             WHERE status NOT IN ('closed', 'tombstone', 'deferred')
               AND (is_template = 0 OR is_template IS NULL)",
        )?;

        let total = issue_rows.len();
        if total == 0 {
            return Ok((0, Vec::new()));
        }

        let mut visible_ids = HashSet::with_capacity(total);
        for row in &issue_rows {
            let issue_id = row
                .get(0)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            if !issue_id.is_empty() {
                visible_ids.insert(issue_id);
            }
        }

        let label_rows = self
            .conn
            .query("SELECT issue_id, label FROM labels ORDER BY issue_id, label")?;
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        let mut labeled_visible_issues = 0usize;
        let mut last_labeled_issue_id = String::new();

        for row in &label_rows {
            let issue_id = row.get(0).and_then(SqliteValue::as_text).unwrap_or("");
            if !visible_ids.contains(issue_id) {
                continue;
            }

            if issue_id != last_labeled_issue_id {
                labeled_visible_issues += 1;
                last_labeled_issue_id.clear();
                last_labeled_issue_id.push_str(issue_id);
            }

            let label = row
                .get(1)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            *counts.entry(label).or_insert(0) += 1;
        }

        let unlabeled = total.saturating_sub(labeled_visible_issues);
        if unlabeled > 0 {
            counts.insert("(no labels)".to_string(), unlabeled);
        }

        Ok((total, counts.into_iter().collect()))
    }

    fn count_default_visible_text_groups(
        &self,
        select_expr: &str,
        order_expr: &str,
    ) -> Result<Vec<(String, usize)>> {
        let sql = format!(
            "SELECT {select_expr}, COUNT(*)
             FROM issues
             WHERE status NOT IN ('closed', 'tombstone', 'deferred')
               AND (is_template = 0 OR is_template IS NULL)
             GROUP BY {select_expr}
             ORDER BY {order_expr}"
        );
        let rows = self.conn.query(&sql)?;
        rows.iter()
            .map(|row| {
                let group = row
                    .get(0)
                    .and_then(SqliteValue::as_text)
                    .unwrap_or("")
                    .to_string();
                let count = row.get(1).and_then(SqliteValue::as_integer).unwrap_or(0);
                Ok((group, usize::try_from(count).unwrap_or(0)))
            })
            .collect()
    }

    /// Count label buckets for issues matching the given filters.
    ///
    /// This mirrors the label grouping semantics used by `br count --by label`:
    /// each labeled issue contributes once per label, and unlabeled issues
    /// contribute to the synthetic `(no labels)` bucket.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    #[allow(clippy::too_many_lines)]
    pub fn count_labels_with_filters(&self, filters: &ListFilters) -> Result<Vec<(String, usize)>> {
        let mut sql = String::from(
            "SELECT labels.label, COUNT(*)
             FROM issues
             LEFT JOIN labels ON labels.issue_id = issues.id
             WHERE 1=1",
        );
        let mut params: Vec<SqliteValue> = Vec::new();

        let label_filters_can_use_uncorrelated_in =
            filters.statuses.as_ref().is_none_or(Vec::is_empty)
                && filters.types.as_ref().is_none_or(Vec::is_empty)
                && filters.priorities.as_ref().is_none_or(Vec::is_empty)
                && filters.assignee.is_none()
                && filters.title_contains.is_none()
                && filters.updated_before.is_none()
                && filters.updated_after.is_none();
        let label_candidate_ids = if label_filters_can_use_uncorrelated_in {
            None
        } else {
            self.label_filter_candidate_ids(
                filters.labels.as_deref().unwrap_or(&[]),
                filters.labels_or.as_deref().unwrap_or(&[]),
            )?
        };
        if label_candidate_ids.as_ref().is_some_and(Vec::is_empty) {
            return Ok(Vec::new());
        }
        if label_filters_can_use_uncorrelated_in {
            append_label_membership_filters(
                &mut sql,
                &mut params,
                filters.labels.as_deref().unwrap_or(&[]),
                filters.labels_or.as_deref().unwrap_or(&[]),
            );
        } else if let Some(ref issue_ids) = label_candidate_ids {
            append_issue_id_membership_filter(&mut sql, &mut params, issue_ids);
        }

        if let Some(ref statuses) = filters.statuses
            && !statuses.is_empty()
        {
            let placeholders: Vec<String> = statuses.iter().map(|_| "?".to_string()).collect();
            let _ = write!(sql, " AND status IN ({}) ", placeholders.join(","));
            for s in statuses {
                params.push(SqliteValue::from(s.as_str()));
            }
        }

        if let Some(ref types) = filters.types
            && !types.is_empty()
        {
            let placeholders: Vec<String> = types.iter().map(|_| "?".to_string()).collect();
            let _ = write!(sql, " AND issue_type IN ({}) ", placeholders.join(","));
            for t in types {
                params.push(SqliteValue::from(t.as_str()));
            }
        }

        if let Some(ref priorities) = filters.priorities
            && !priorities.is_empty()
        {
            let placeholders: Vec<String> = priorities.iter().map(|_| "?".to_string()).collect();
            let _ = write!(sql, " AND priority IN ({}) ", placeholders.join(","));
            for p in priorities {
                params.push(SqliteValue::from(i64::from(p.0)));
            }
        }

        if let Some(ref assignee) = filters.assignee {
            sql.push_str(" AND assignee = ?");
            params.push(SqliteValue::from(assignee.as_str()));
        }

        if filters.unassigned {
            sql.push_str(" AND (assignee IS NULL OR assignee = '')");
        }

        if !filters.include_closed {
            if filters.include_deferred {
                sql.push_str(" AND status NOT IN ('closed', 'tombstone')");
            } else {
                sql.push_str(" AND status NOT IN ('closed', 'tombstone', 'deferred')");
            }
        } else if filters.statuses.as_ref().is_none_or(Vec::is_empty) {
            sql.push_str(" AND status != 'tombstone'");
        }

        if !filters.include_templates {
            sql.push_str(" AND (is_template = 0 OR is_template IS NULL)");
        }

        if let Some(ref title_contains) = filters.title_contains {
            sql.push_str(" AND title LIKE ? ESCAPE '\\'");
            let escaped = escape_like_pattern(title_contains);
            params.push(SqliteValue::from(format!("%{escaped}%")));
        }

        if let Some(ts) = filters.updated_before {
            sql.push_str(" AND updated_at <= ?");
            params.push(SqliteValue::from(ts.to_rfc3339()));
        }

        if let Some(ts) = filters.updated_after {
            sql.push_str(" AND updated_at >= ?");
            params.push(SqliteValue::from(ts.to_rfc3339()));
        }

        sql.push_str(" GROUP BY labels.label ORDER BY labels.label");

        let rows = self.conn.query_with_params(&sql, &params)?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                let label = row
                    .get(0)
                    .and_then(SqliteValue::as_text)
                    .unwrap_or("(no labels)")
                    .to_string();
                let count = row.get(1).and_then(SqliteValue::as_integer)?;
                Some((label, usize::try_from(count).unwrap_or(0)))
            })
            .collect())
    }

    /// Search issues by query with optional filters.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn search_issues(&self, query: &str, filters: &ListFilters) -> Result<Vec<Issue>> {
        self.search_issues_with_projection(query, filters, SearchIssueProjection::Full)
    }

    /// Search command issues without hydrating fields text/rich search output never renders.
    ///
    /// This keeps `description` for rich context snippets while omitting large
    /// structured fields used only by JSON/TOON/CSV or client-side filters.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn search_issues_for_command_output(
        &self,
        query: &str,
        filters: &ListFilters,
    ) -> Result<Vec<Issue>> {
        self.search_issues_with_projection(query, filters, SearchIssueProjection::CommandText)
    }

    #[allow(clippy::too_many_lines)]
    fn search_issues_with_projection(
        &self,
        query: &str,
        filters: &ListFilters,
        projection: SearchIssueProjection,
    ) -> Result<Vec<Issue>> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }

        if let Some(limit) = default_visible_limited_page_limit(filters) {
            return self.search_default_visible_limited_page(
                trimmed,
                filters.include_deferred,
                limit,
                projection,
            );
        }

        let mut sql = String::from(projection.select_clause());
        let mut params: Vec<SqliteValue> = Vec::new();

        let labels_and = filters.labels.as_deref().unwrap_or(&[]);
        let labels_or = filters.labels_or.as_deref().unwrap_or(&[]);
        let label_candidate_ids = if labels_and.is_empty() && labels_or.is_empty() {
            None
        } else {
            self.label_filter_candidate_ids(labels_and, labels_or)?
        };
        if label_candidate_ids.as_ref().is_some_and(Vec::is_empty) {
            return Ok(Vec::new());
        }
        if self.redundant_default_visible_single_label_filter(
            filters,
            label_candidate_ids.as_deref(),
        )? {
            let mut filters_without_redundant_label = filters.clone();
            filters_without_redundant_label.labels = None;
            return self.search_issues_with_projection(
                trimmed,
                &filters_without_redundant_label,
                projection,
            );
        }
        if let Some(ref issue_ids) = label_candidate_ids {
            append_issue_id_membership_filter(&mut sql, &mut params, issue_ids);
        }

        if let Some(ref statuses) = filters.statuses
            && !statuses.is_empty()
        {
            let placeholders: Vec<String> = statuses.iter().map(|_| "?".to_string()).collect();
            let _ = write!(sql, " AND status IN ({})", placeholders.join(","));
            for s in statuses {
                params.push(SqliteValue::from(s.as_str()));
            }
        }

        if let Some(ref types) = filters.types
            && !types.is_empty()
        {
            let placeholders: Vec<String> = types.iter().map(|_| "?".to_string()).collect();
            let _ = write!(sql, " AND issue_type IN ({})", placeholders.join(","));
            for t in types {
                params.push(SqliteValue::from(t.as_str()));
            }
        }

        if let Some(ref priorities) = filters.priorities
            && !priorities.is_empty()
        {
            let placeholders: Vec<String> = priorities.iter().map(|_| "?".to_string()).collect();
            let _ = write!(sql, " AND priority IN ({})", placeholders.join(","));
            for p in priorities {
                params.push(SqliteValue::from(i64::from(p.0)));
            }
        }

        if let Some(ref assignee) = filters.assignee {
            sql.push_str(" AND assignee = ?");
            params.push(SqliteValue::from(assignee.as_str()));
        }

        if filters.unassigned {
            sql.push_str(" AND (assignee IS NULL OR assignee = '')");
        }

        if !filters.include_closed {
            if filters.include_deferred {
                sql.push_str(" AND status NOT IN ('closed', 'tombstone')");
            } else {
                sql.push_str(" AND status NOT IN ('closed', 'tombstone', 'deferred')");
            }
        } else if filters.statuses.as_ref().is_none_or(Vec::is_empty) {
            // When including closed issues, still exclude tombstones (deleted issues) by default
            // unless specific statuses were requested.
            sql.push_str(" AND status != 'tombstone'");
        }

        if !filters.include_templates {
            sql.push_str(" AND (is_template = 0 OR is_template IS NULL)");
        }

        if let Some(ref title_contains) = filters.title_contains {
            sql.push_str(" AND title LIKE ? ESCAPE '\\'");
            let escaped = escape_like_pattern(title_contains);
            params.push(SqliteValue::from(format!("%{escaped}%")));
        }

        if let Some(ts) = filters.updated_before {
            sql.push_str(" AND updated_at <= ?");
            params.push(SqliteValue::from(ts.to_rfc3339()));
        }

        if let Some(ts) = filters.updated_after {
            sql.push_str(" AND updated_at >= ?");
            params.push(SqliteValue::from(ts.to_rfc3339()));
        }

        sql.push_str(
            " AND (instr(lower(title), ?) > 0 OR instr(lower(description), ?) > 0 OR instr(lower(id), ?) > 0)",
        );
        let needle = trimmed.to_ascii_lowercase();
        params.push(SqliteValue::from(needle.as_str()));
        params.push(SqliteValue::from(needle.as_str()));
        params.push(SqliteValue::from(needle));

        if let Some(ref sort_field) = filters.sort {
            let order = if filters.reverse { "DESC" } else { "ASC" };
            match sort_field.as_str() {
                "priority" => {
                    let secondary_order = if filters.reverse { "ASC" } else { "DESC" };
                    let _ = write!(
                        sql,
                        " ORDER BY priority {order}, created_at {secondary_order}, id ASC"
                    );
                }
                "created_at" | "created" => {
                    let order = if filters.reverse { "ASC" } else { "DESC" };
                    let _ = write!(sql, " ORDER BY created_at {order}, id ASC");
                }
                "updated_at" | "updated" => {
                    let order = if filters.reverse { "ASC" } else { "DESC" };
                    let _ = write!(sql, " ORDER BY updated_at {order}, id ASC");
                }
                "title" => {
                    let _ = write!(sql, " ORDER BY title COLLATE NOCASE {order}, id ASC");
                }
                _ => {
                    if filters.reverse {
                        sql.push_str(" ORDER BY priority DESC, created_at ASC, id ASC");
                    } else {
                        sql.push_str(" ORDER BY priority ASC, created_at DESC, id ASC");
                    }
                }
            }
        } else if filters.reverse {
            sql.push_str(" ORDER BY priority DESC, created_at ASC, id ASC");
        } else {
            sql.push_str(" ORDER BY priority ASC, created_at DESC, id ASC");
        }

        match (filters.limit, filters.offset) {
            (Some(limit), offset) if limit > 0 => {
                let _ = write!(sql, " LIMIT {limit}");
                if let Some(offset) = offset
                    && offset > 0
                {
                    let _ = write!(sql, " OFFSET {offset}");
                }
            }
            (_, Some(offset)) if offset > 0 => {
                let _ = write!(sql, " LIMIT -1 OFFSET {offset}");
            }
            _ => {}
        }

        let rows = self.conn.query_with_params(&sql, &params)?;
        let mut issues = Vec::with_capacity(rows.len());
        for row in &rows {
            issues.push(projection.parse_issue(row)?);
        }

        Ok(issues)
    }

    fn search_default_visible_limited_page(
        &self,
        query: &str,
        include_deferred: bool,
        limit: usize,
        projection: SearchIssueProjection,
    ) -> Result<Vec<Issue>> {
        let status_filter = if include_deferred {
            "status NOT IN ('closed', 'tombstone')"
        } else {
            "status NOT IN ('closed', 'tombstone', 'deferred')"
        };
        let needle = query.to_ascii_lowercase();
        if !self.search_default_visible_has_match(status_filter, &needle)? {
            return Ok(Vec::new());
        }

        let mut issues = self.search_default_visible_priority_window(
            status_filter,
            &needle,
            projection,
            "priority = ?",
            Priority::CRITICAL.0,
            "created_at DESC, id ASC",
            limit,
        )?;

        let remaining = limit.saturating_sub(issues.len());
        if remaining > 0 {
            issues.extend(self.search_default_visible_priority_window(
                status_filter,
                &needle,
                projection,
                "priority > ?",
                Priority::CRITICAL.0,
                "priority ASC, created_at DESC, id ASC",
                remaining,
            )?);
        }

        Ok(issues)
    }

    fn search_default_visible_has_match(&self, status_filter: &str, needle: &str) -> Result<bool> {
        let sql = format!(
            "SELECT 1 FROM issues
             WHERE {status_filter}
               AND (is_template = 0 OR is_template IS NULL)
               AND (instr(lower(title), ?) > 0 OR instr(lower(description), ?) > 0 OR instr(lower(id), ?) > 0)
             LIMIT 1"
        );
        let rows = self.conn.query_with_params(
            &sql,
            &[
                SqliteValue::from(needle),
                SqliteValue::from(needle),
                SqliteValue::from(needle),
            ],
        )?;
        Ok(!rows.is_empty())
    }

    #[allow(clippy::too_many_arguments)]
    fn search_default_visible_priority_window(
        &self,
        status_filter: &str,
        needle: &str,
        projection: SearchIssueProjection,
        priority_predicate: &str,
        priority_value: i32,
        order_by: &str,
        limit: usize,
    ) -> Result<Vec<Issue>> {
        let sql = format!(
            r"{}
              AND {status_filter}
              AND (is_template = 0 OR is_template IS NULL)
              AND {priority_predicate}
              AND (instr(lower(title), ?) > 0 OR instr(lower(description), ?) > 0 OR instr(lower(id), ?) > 0)
              ORDER BY {order_by}
              LIMIT {limit}",
            projection.select_clause()
        );
        let rows = self.conn.query_with_params(
            &sql,
            &[
                SqliteValue::from(i64::from(priority_value)),
                SqliteValue::from(needle),
                SqliteValue::from(needle),
                SqliteValue::from(needle),
            ],
        )?;
        let mut issues = Vec::with_capacity(rows.len());
        for row in &rows {
            issues.push(projection.parse_issue(row)?);
        }
        Ok(issues)
    }

    /// Get ready issues (unblocked, not deferred, not pinned, not ephemeral).
    ///
    /// Ready definition:
    /// 1. Status is `open` by default, or `deferred` when `include_deferred` is set
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
        self.get_ready_issues_with_projection(filters, sort, ReadyIssueProjection::Full)
    }

    /// Get ready issues optimized for `ready` command rendering.
    ///
    /// Hydrates only the columns consumed by the command's JSON/TOON/text
    /// output, avoiding large overflow-page reads for fields that never reach
    /// the user-facing result.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_ready_issues_for_command_output(
        &self,
        filters: &ReadyFilters,
        sort: ReadySortPolicy,
    ) -> Result<Vec<Issue>> {
        self.get_ready_issues_with_projection(filters, sort, ReadyIssueProjection::Command)
    }

    /// Get ready issues optimized for compact text command rendering.
    ///
    /// Hydrates only the columns read by ready text/table output and ordering.
    /// Structured JSON/TOON callers should use
    /// [`Self::get_ready_issues_for_command_output`] to preserve the existing
    /// schema.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_ready_summary_issues_for_command_output(
        &self,
        filters: &ReadyFilters,
        sort: ReadySortPolicy,
    ) -> Result<Vec<Issue>> {
        self.get_ready_issues_with_projection(filters, sort, ReadyIssueProjection::Summary)
    }

    fn get_ready_issues_with_projection(
        &self,
        filters: &ReadyFilters,
        sort: ReadySortPolicy,
        projection: ReadyIssueProjection,
    ) -> Result<Vec<Issue>> {
        let readiness = self.ready_readiness_probe(filters.include_deferred)?;
        if !readiness.has_candidate_status {
            return Ok(Vec::new());
        }

        // Resolve `--parent` membership in Rust so the candidate query filters
        // on a plain `id IN (...)` list rather than an `IN (subquery)` /
        // recursive CTE (see `ReadyFilters::parent_member_ids`). Skip the work
        // when membership has already been resolved by the caller.
        let resolved_filters;
        let filters = if filters.parent.is_some() && filters.parent_member_ids.is_none() {
            let mut owned = filters.clone();
            owned.parent_member_ids = Some(self.resolve_ready_parent_member_ids(
                owned.parent.as_deref().unwrap_or_default(),
                owned.recursive,
            )?);
            resolved_filters = owned;
            &resolved_filters
        } else {
            filters
        };

        // Read-only path: if the cache is stale, compute blocked IDs in memory
        // instead of persisting (issue #216 — read ops must not write).
        if readiness.blocked_cache_stale {
            let blocked_ids = match Self::compute_blocked_issues_map_impl(&self.conn) {
                Ok(map) => map.into_keys().collect(),
                Err(error) => self.recover_blocked_ids("ready_issues_stale", &error)?,
            };
            return self.query_ready_issues_without_cache_with_projection(
                filters,
                sort,
                &blocked_ids,
                projection,
            );
        }

        match self
            .query_ready_issue_candidates_with_projection(filters, sort, true, true, projection)
        {
            Ok(issues) => Ok(issues),
            Err(error) => {
                let blocked_ids = self.recover_blocked_ids("ready_issues_query", &error)?;
                self.query_ready_issues_without_cache_with_projection(
                    filters,
                    sort,
                    &blocked_ids,
                    projection,
                )
            }
        }
    }

    /// Resolve the set of issue IDs matched by a `--parent` filter on `ready`.
    ///
    /// Returns the direct children of `parent_id`, or — when `recursive` — all
    /// transitive parent-child descendants. Traversal is an iterative BFS with a
    /// visited-set, so it terminates in bounded time even if the parent-child
    /// graph contains a cycle (the embedded SQLite backend mishandles recursive
    /// CTEs referenced from a correlated `EXISTS`, which previously surfaced as a
    /// hang/error — #308).
    fn resolve_ready_parent_member_ids(
        &self,
        parent_id: &str,
        recursive: bool,
    ) -> Result<Vec<String>> {
        let children_by_parent = Self::load_local_parent_child_edges_impl(&self.conn)?;

        let mut members: Vec<String> = Vec::new();
        let mut visited: HashSet<String> = HashSet::new();
        visited.insert(parent_id.to_string());

        if recursive {
            let mut queue: VecDeque<String> = VecDeque::new();
            queue.push_back(parent_id.to_string());
            while let Some(current) = queue.pop_front() {
                if let Some(children) = children_by_parent.get(&current) {
                    for child in children {
                        if visited.insert(child.clone()) {
                            members.push(child.clone());
                            queue.push_back(child.clone());
                        }
                    }
                }
            }
        } else if let Some(children) = children_by_parent.get(parent_id) {
            for child in children {
                if visited.insert(child.clone()) {
                    members.push(child.clone());
                }
            }
        }

        Ok(members)
    }

    #[allow(clippy::too_many_lines)]
    fn build_ready_issue_candidates_query(
        filters: &ReadyFilters,
        sort: ReadySortPolicy,
        exclude_blocked_in_sql: bool,
        apply_limit: bool,
        projection: ReadyIssueProjection,
        apply_ordering: bool,
    ) -> (String, Vec<SqliteValue>) {
        let mut sql = String::from(projection.select_clause());
        let mut params: Vec<SqliteValue> = Vec::new();

        let label_filters_can_use_uncorrelated_in =
            filters.types.as_ref().is_none_or(Vec::is_empty)
                && filters.priorities.as_ref().is_none_or(Vec::is_empty)
                && filters.assignee.is_none()
                && filters.parent.is_none();
        if label_filters_can_use_uncorrelated_in {
            sql.push_str(" FROM issues WHERE 1=1");
            append_label_membership_filters(
                &mut sql,
                &mut params,
                &filters.labels_and,
                &filters.labels_or,
            );
        } else {
            append_issue_source_with_label_and_joins(&mut sql, &mut params, &filters.labels_and);
            sql.push_str(" WHERE 1=1");
            append_label_or_membership_exists(&mut sql, &mut params, &filters.labels_or);
        }

        // Ready condition 1: the configured ready status group is "ready"
        // (issue #354). The default group is `[open]`, matching pre-#354
        // behavior (in_progress means already claimed). `--include-deferred`
        // additionally folds in `deferred` without double-counting it if the
        // configured group already lists it.
        let mut ready_statuses: Vec<String> = if filters.ready_statuses.is_empty() {
            vec!["open".to_string()]
        } else {
            filters.ready_statuses.clone()
        };
        if filters.include_deferred
            && !ready_statuses
                .iter()
                .any(|status| status.eq_ignore_ascii_case("deferred"))
        {
            ready_statuses.push("deferred".to_string());
        }
        // The status list is inlined as SQL string literals rather than bound
        // `?` params. Status values are internal, validated, lowercased policy
        // names (never raw user input reaching SQL), and the embedded fsqlite
        // planner mishandles a bound `IN (?)` predicate when it sits alongside a
        // correlated/grouped `id IN (SELECT ... HAVING ...)` label subquery —
        // the same engine class of limitation documented on
        // `ReadyFilters::parent_member_ids` (#307/#308). Inlining keeps the
        // single-`open` case byte-identical to the pre-#354 literal and keeps
        // widened groups index-coverable. Single quotes are still escaped
        // defensively in case a project configures an exotic custom status.
        {
            let escaped: Vec<String> = ready_statuses
                .iter()
                .map(|status| format!("'{}'", status.replace('\'', "''")))
                .collect();
            let _ = write!(sql, " AND status IN ({})", escaped.join(","));
        }

        // Ready condition 2: blocked issues are filtered in SQL when the cache
        // is healthy; fallback callers filter them in Rust after directly
        // recomputing the blocker graph from dependencies.
        if exclude_blocked_in_sql {
            sql.push_str(" AND issues.id NOT IN (SELECT issue_id FROM blocked_issues_cache)");
        }

        // Ready condition 3: `defer_until` is NULL or <= now (unless `include_deferred`)
        if !filters.include_deferred {
            sql.push_str(" AND (defer_until IS NULL OR datetime(defer_until) <= datetime('now'))");
        }

        // Ready condition 4: not pinned. Legacy rows may still store NULL,
        // which the rest of the storage layer treats as false.
        sql.push_str(" AND (pinned = 0 OR pinned IS NULL)");

        // Ready condition 5: not ephemeral and not wisp. Legacy rows may
        // still store NULL, which should behave the same as false.
        sql.push_str(" AND (ephemeral = 0 OR ephemeral IS NULL)");
        sql.push_str(" AND id NOT LIKE '%-wisp-%'");

        // Exclude templates
        sql.push_str(" AND (is_template = 0 OR is_template IS NULL)");

        // Filter by types
        if let Some(ref types) = filters.types
            && !types.is_empty()
        {
            let placeholders: Vec<String> = types.iter().map(|_| "?".to_string()).collect();
            let _ = write!(sql, " AND issue_type IN ({}) ", placeholders.join(","));
            for t in types {
                params.push(SqliteValue::from(t.as_str()));
            }
        }

        // Filter by priorities
        if let Some(ref priorities) = filters.priorities
            && !priorities.is_empty()
        {
            let placeholders: Vec<String> = priorities.iter().map(|_| "?".to_string()).collect();
            let _ = write!(sql, " AND priority IN ({})", placeholders.join(","));
            for p in priorities {
                params.push(SqliteValue::from(i64::from(p.0)));
            }
        }

        // Filter by assignee
        if let Some(ref assignee) = filters.assignee {
            sql.push_str(" AND assignee = ?");
            params.push(SqliteValue::from(assignee.as_str()));
        }

        // Filter for unassigned
        if filters.unassigned {
            sql.push_str(" AND (assignee IS NULL OR assignee = '')");
        }

        // Filter by parent (--parent flag).
        //
        // Membership is resolved in Rust (see `resolve_ready_parent_member_ids`)
        // and passed as `parent_member_ids`, so we filter with a plain
        // `id IN (...)` list rather than an `IN (subquery)` / recursive CTE. This
        // avoids embedded-SQLite planner bugs around `IN (subquery)` under
        // multi-table joins (#307) and recursive CTEs referenced from a
        // correlated `EXISTS` (#308).
        let cte_prefix = String::new();
        if filters.parent.is_some() {
            match filters.parent_member_ids.as_deref() {
                Some([]) | None => {
                    // Parent has no matching descendants (or membership was not
                    // resolved): force an empty result without scanning.
                    sql.push_str(" AND 1=0");
                }
                Some(member_ids) => {
                    append_issue_id_membership_filter(&mut sql, &mut params, member_ids);
                }
            }
        }

        if apply_ordering {
            match sort {
                ReadySortPolicy::Hybrid => {
                    sql.push_str(
                        " ORDER BY CASE WHEN issues.priority <= 1 THEN 0 ELSE 1 END, issues.created_at ASC, issues.id ASC",
                    );
                }
                ReadySortPolicy::Priority => {
                    sql.push_str(
                        " ORDER BY issues.priority ASC, issues.created_at ASC, issues.id ASC",
                    );
                }
                ReadySortPolicy::Oldest => {
                    sql.push_str(" ORDER BY issues.created_at ASC, issues.id ASC");
                }
            }
        }

        // Apply limit in SQL only when the blocked filter also happens in SQL.
        // Fallback callers must filter blocked IDs in Rust first.
        if apply_limit
            && let Some(limit) = filters.limit
            && limit > 0
        {
            let _ = write!(sql, " LIMIT {limit}");
        }

        if !cte_prefix.is_empty() {
            sql = format!("{cte_prefix}{sql}");
        }

        (sql, params)
    }

    fn query_ready_issue_candidates_with_projection(
        &self,
        filters: &ReadyFilters,
        sort: ReadySortPolicy,
        exclude_blocked_in_sql: bool,
        apply_limit: bool,
        projection: ReadyIssueProjection,
    ) -> Result<Vec<Issue>> {
        if sort == ReadySortPolicy::Hybrid
            && apply_limit
            && let Some(issues) = self.query_limited_ready_hybrid_high_bucket(
                filters,
                exclude_blocked_in_sql,
                projection,
            )?
        {
            return Ok(issues);
        }

        let sort_hybrid_in_rust = sort == ReadySortPolicy::Hybrid && filters.limit.is_none();
        let (sql, params) = Self::build_ready_issue_candidates_query(
            filters,
            sort,
            exclude_blocked_in_sql,
            apply_limit,
            projection,
            !sort_hybrid_in_rust,
        );
        let rows = self.conn.query_with_params(&sql, &params)?;
        let mut issues = Vec::with_capacity(rows.len());
        for row in &rows {
            issues.push(projection.parse_row(row)?);
        }
        if sort_hybrid_in_rust {
            sort_ready_hybrid(&mut issues);
        }

        Ok(issues)
    }

    fn query_limited_ready_hybrid_high_bucket(
        &self,
        filters: &ReadyFilters,
        exclude_blocked_in_sql: bool,
        projection: ReadyIssueProjection,
    ) -> Result<Option<Vec<Issue>>> {
        let Some(limit) = filters.limit.filter(|limit| *limit > 0) else {
            return Ok(None);
        };

        let priorities = ready_hybrid_high_bucket_priorities(filters.priorities.as_deref());
        if priorities.is_empty() {
            return Ok(None);
        }

        let mut high_bucket_filters = filters.clone();
        high_bucket_filters.priorities = Some(priorities);
        high_bucket_filters.limit = Some(limit);

        if exclude_blocked_in_sql {
            let issues = self.query_ready_issue_candidates_with_projection(
                &high_bucket_filters,
                ReadySortPolicy::Oldest,
                true,
                true,
                projection,
            )?;
            return Ok((issues.len() >= limit).then_some(issues));
        }

        let mut summary_issues = self.query_ready_issue_candidates_with_projection(
            &high_bucket_filters,
            ReadySortPolicy::Oldest,
            false,
            false,
            ReadyIssueProjection::Summary,
        )?;
        if exclude_blocked_in_sql {
            let blocked_ids = self.get_blocked_ids()?;
            summary_issues.retain(|issue| !blocked_ids.contains(issue.id.as_str()));
        }

        if summary_issues.len() >= limit {
            summary_issues.truncate(limit);
            if projection == ReadyIssueProjection::Summary {
                return Ok(Some(summary_issues));
            }

            let ids: Vec<String> = summary_issues.into_iter().map(|issue| issue.id).collect();
            Ok(Some(self.get_ready_issues_by_ids_with_projection(
                &ids, projection,
            )?))
        } else {
            Ok(None)
        }
    }

    fn get_ready_issues_by_ids_with_projection(
        &self,
        ids: &[String],
        projection: ReadyIssueProjection,
    ) -> Result<Vec<Issue>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut by_id = HashMap::with_capacity(ids.len());
        for chunk in ids.chunks(SQLITE_VAR_LIMIT) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            let sql = format!(
                "{} FROM issues WHERE id IN ({})",
                projection.select_clause(),
                placeholders.join(",")
            );
            let params: Vec<SqliteValue> = chunk
                .iter()
                .map(|id| SqliteValue::from(id.as_str()))
                .collect();
            let rows = self.conn.query_with_params(&sql, &params)?;
            for row in &rows {
                let issue = projection.parse_row(row)?;
                by_id.insert(issue.id.clone(), issue);
            }
        }

        let mut issues = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(issue) = by_id.remove(id) {
                issues.push(issue);
            }
        }

        Ok(issues)
    }

    fn query_ready_issues_without_cache_with_projection(
        &self,
        filters: &ReadyFilters,
        sort: ReadySortPolicy,
        blocked_ids: &HashSet<String>,
        projection: ReadyIssueProjection,
    ) -> Result<Vec<Issue>> {
        let mut issues = self.query_ready_issue_candidates_with_projection(
            filters, sort, false, false, projection,
        )?;
        issues.retain(|issue| !blocked_ids.contains(issue.id.as_str()));
        if let Some(limit) = filters.limit
            && limit > 0
            && issues.len() > limit
        {
            issues.truncate(limit);
        }
        Ok(issues)
    }

    fn recover_blocked_issues_map(
        &self,
        stage: &'static str,
        error: &dyn std::fmt::Display,
    ) -> Result<HashMap<String, Vec<String>>> {
        if is_transient_wal_tail_read_error(error) {
            tracing::trace!(
                stage,
                %error,
                "Blocked cache unavailable during transient WAL tail read; computing blocker graph directly"
            );
        } else {
            tracing::warn!(
                stage,
                %error,
                "Blocked cache unavailable; computing blocker graph directly"
            );
        }
        Self::compute_blocked_issues_map_impl(&self.conn)
    }

    fn recover_blocked_ids(
        &self,
        stage: &'static str,
        error: &dyn std::fmt::Display,
    ) -> Result<HashSet<String>> {
        Ok(self
            .recover_blocked_issues_map(stage, error)?
            .into_keys()
            .collect())
    }

    /// Get IDs of blocked issues from cache.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_blocked_ids(&self) -> Result<HashSet<String>> {
        // Read-only path: if the cache is stale, compute in memory instead of
        // persisting (issue #216 — read ops must not write).
        if self.blocked_cache_marked_stale()? {
            return match Self::compute_blocked_issues_map_impl(&self.conn) {
                Ok(map) => Ok(map.into_keys().collect()),
                Err(error) => self.recover_blocked_ids("get_blocked_ids_stale", &error),
            };
        }
        let rows = match self.conn.query("SELECT issue_id FROM blocked_issues_cache") {
            Ok(rows) => rows,
            Err(error) => return self.recover_blocked_ids("get_blocked_ids_query", &error),
        };
        let mut ids = HashSet::new();
        for row in &rows {
            if let Some(id) = row.get(0).and_then(SqliteValue::as_text) {
                ids.insert(id.to_string());
            }
        }
        Ok(ids)
    }

    /// Get raw `blocks` dependency edges as (issue_id, depends_on_id) pairs.
    ///
    /// This is a lightweight single-table query (no JOINs) suitable for
    /// callers that already have issues loaded in memory and can filter
    /// by status themselves.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_blocks_dep_edges(&self) -> Result<Vec<(String, String)>> {
        let mut edges = Vec::new();

        // Query 1: Standard blocking types
        let rows1 = self.conn.query(
            "SELECT issue_id, depends_on_id FROM dependencies \
             WHERE type IN ('blocks', 'conditional-blocks', 'waits-for')",
        )?;
        for row in &rows1 {
            if let Some(issue_id) = row.get(0).and_then(SqliteValue::as_text)
                && let Some(depends_on) = row.get(1).and_then(SqliteValue::as_text)
            {
                edges.push((issue_id.to_string(), depends_on.to_string()));
            }
        }

        // Query 2: Parent-child (reversed direction)
        let rows2 = self.conn.query(
            "SELECT depends_on_id, issue_id FROM dependencies \
             WHERE type = 'parent-child'",
        )?;
        for row in &rows2 {
            if let Some(issue_id) = row.get(0).and_then(SqliteValue::as_text)
                && let Some(depends_on) = row.get(1).and_then(SqliteValue::as_text)
            {
                edges.push((issue_id.to_string(), depends_on.to_string()));
            }
        }

        Ok(edges)
    }

    /// Get raw blocking dependency edges whose endpoints are in `issue_ids`.
    ///
    /// Returns `(issue_id, depends_on_id)` pairs, matching [`Self::get_blocks_dep_edges`].
    /// For large active sets, falls back to the full edge scan to stay below
    /// SQLite's parameter limit.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_blocks_dep_edges_for_issue_ids(
        &self,
        issue_ids: &[&str],
    ) -> Result<Vec<(String, String)>> {
        if issue_ids.is_empty() {
            return Ok(Vec::new());
        }
        if issue_ids.len() > BLOCKS_DEP_EDGE_FILTER_LIMIT {
            return self.get_blocks_dep_edges();
        }

        let mut edges = Vec::new();
        let placeholders: Vec<&str> = issue_ids.iter().map(|_| "?").collect();
        let placeholders = placeholders.join(", ");

        let mut params = Vec::with_capacity(issue_ids.len() * 2);
        for issue_id in issue_ids {
            params.push(SqliteValue::from(*issue_id));
        }
        for issue_id in issue_ids {
            params.push(SqliteValue::from(*issue_id));
        }

        let standard_sql = format!(
            "SELECT issue_id, depends_on_id FROM dependencies \
             WHERE type IN ('blocks', 'conditional-blocks', 'waits-for') \
               AND issue_id IN ({placeholders}) \
               AND depends_on_id IN ({placeholders})"
        );
        let rows1 = self.conn.query_with_params(&standard_sql, &params)?;
        for row in &rows1 {
            if let Some(issue_id) = row.get(0).and_then(SqliteValue::as_text)
                && let Some(depends_on) = row.get(1).and_then(SqliteValue::as_text)
            {
                edges.push((issue_id.to_string(), depends_on.to_string()));
            }
        }

        let parent_child_sql = format!(
            "SELECT depends_on_id, issue_id FROM dependencies \
             WHERE type = 'parent-child' \
               AND depends_on_id IN ({placeholders}) \
               AND issue_id IN ({placeholders})"
        );
        let rows2 = self.conn.query_with_params(&parent_child_sql, &params)?;
        for row in &rows2 {
            if let Some(issue_id) = row.get(0).and_then(SqliteValue::as_text)
                && let Some(depends_on) = row.get(1).and_then(SqliteValue::as_text)
            {
                edges.push((issue_id.to_string(), depends_on.to_string()));
            }
        }

        Ok(edges)
    }

    /// Check if an issue is blocked (in the blocked cache).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn is_blocked(&self, issue_id: &str) -> Result<bool> {
        // Read-only path: if the cache is stale, compute in memory instead of
        // persisting (issue #216 — read ops must not write).
        if self.blocked_cache_marked_stale()? {
            let blocked_ids = match Self::compute_blocked_issues_map_impl(&self.conn) {
                Ok(map) => map.into_keys().collect::<HashSet<_>>(),
                Err(error) => self.recover_blocked_ids("is_blocked_stale", &error)?,
            };
            return Ok(blocked_ids.contains(issue_id));
        }
        let rows = match self.conn.query_with_params(
            "SELECT 1 FROM blocked_issues_cache WHERE issue_id = ? LIMIT 1",
            &[SqliteValue::from(issue_id)],
        ) {
            Ok(rows) => rows,
            Err(error) => {
                let blocked_ids = self.recover_blocked_ids("is_blocked_query", &error)?;
                return Ok(blocked_ids.contains(issue_id));
            }
        };
        Ok(!rows.is_empty())
    }

    /// Get the actual blockers for an issue from the blocked issues cache.
    ///
    /// Returns the issue IDs that are blocking this issue. The format includes
    /// status annotations like "bd-123:open" or "bd-456:parent-blocked".
    /// Returns an empty vec if the issue is not blocked.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_blockers(&self, issue_id: &str) -> Result<Vec<String>> {
        Ok(Self::blocker_refs_to_issue_ids(
            &self.get_blocker_refs(issue_id)?,
        ))
    }

    /// Get the blockers that prevent *starting* work on an issue (claiming it or
    /// moving it to `in_progress`).
    ///
    /// This is [`get_blockers`](Self::get_blockers) minus two *rollup* markers
    /// that are not real prerequisite edges on the issue itself:
    ///
    /// - `:child-open` — a *close-ordering* constraint (an epic should not be
    ///   *closed* while it still has open children). It must NOT prevent the
    ///   epic from being claimed and worked on (#315).
    /// - `:parent-blocked` — propagated down onto a child from an
    ///   *already-blocked* parent epic. `parent-child` is hierarchy, not a
    ///   prerequisite edge from the parent to the child, so a child that has no
    ///   real blocker of its own must remain claimable even while its parent
    ///   epic is blocked (#357 — the start-path counterpart of #355, which
    ///   stripped the same marker on the *close* path). The actionable children
    ///   of a blocked epic are frequently exactly the work that unblocks it.
    ///
    /// Real start-blocking edges on the child itself (`blocks`,
    /// `conditional-blocks`, `waits-for`) do not carry either suffix and are
    /// retained — a child with a *direct* prerequisite still cannot be started.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_start_blockers(&self, issue_id: &str) -> Result<Vec<String>> {
        let start_blocker_refs: Vec<String> = self
            .get_blocker_refs(issue_id)?
            .into_iter()
            .filter(|blocker| {
                !blocker.ends_with(CHILD_OPEN_BLOCKER_SUFFIX)
                    && !blocker.ends_with(PARENT_BLOCKED_SUFFIX)
            })
            .collect();
        Ok(Self::blocker_refs_to_issue_ids(&start_blocker_refs))
    }

    /// Get the blockers that prevent *closing* an issue.
    ///
    /// This is [`get_blockers`](Self::get_blockers) minus the
    /// `:parent-blocked` rollup markers. Those markers encode a *readiness*
    /// constraint propagated from an already-blocked parent epic down onto its
    /// children — a child of a blocked epic is not "ready" to be *started* —
    /// but `parent-child` is hierarchy, not a prerequisite edge from the parent
    /// to the child. A finished child must be closable even while its parent
    /// epic remains blocked or open (#355). Real prerequisite edges on the
    /// child itself (`blocks`, `conditional-blocks`, `waits-for`) and the
    /// `:child-open` close-ordering rollup (so an epic still can't close over
    /// open children) are retained.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_close_blockers(&self, issue_id: &str) -> Result<Vec<String>> {
        let close_blocker_refs: Vec<String> = self
            .get_blocker_refs(issue_id)?
            .into_iter()
            .filter(|blocker| !blocker.ends_with(PARENT_BLOCKED_SUFFIX))
            .collect();
        Ok(Self::blocker_refs_to_issue_ids(&close_blocker_refs))
    }

    /// Return the raw, annotation-bearing blocker refs for an issue (e.g.
    /// `bd-123:open`, `bd-456:parent-blocked`, `bd-789:child-open`), or an empty
    /// vec if the issue is not blocked. Callers that only need issue IDs should
    /// use [`get_blockers`](Self::get_blockers); callers enforcing start/claim
    /// ordering should use [`get_start_blockers`](Self::get_start_blockers).
    fn get_blocker_refs(&self, issue_id: &str) -> Result<Vec<String>> {
        // Read-only path: if the cache is stale, compute in memory instead of
        // persisting (issue #216 — read ops must not write).
        if self.blocked_cache_marked_stale()? {
            let blocked_issues_map = match Self::compute_blocked_issues_map_impl(&self.conn) {
                Ok(map) => map,
                Err(error) => self.recover_blocked_issues_map("get_blockers_stale", &error)?,
            };
            return Ok(blocked_issues_map
                .get(issue_id)
                .map_or_else(Vec::new, Clone::clone));
        }
        let rows = match self.conn.query_with_params(
            "SELECT blocked_by FROM blocked_issues_cache WHERE issue_id = ?",
            &[SqliteValue::from(issue_id)],
        ) {
            Ok(rows) => rows,
            Err(error) => {
                let blocked_issues_map =
                    self.recover_blocked_issues_map("get_blockers_query", &error)?;
                return Ok(blocked_issues_map
                    .get(issue_id)
                    .map_or_else(Vec::new, Clone::clone));
            }
        };
        let Some(row) = rows.first() else {
            return Ok(Vec::new());
        };

        match parse_blocked_by_json(issue_id, row.get(0).and_then(SqliteValue::as_text)) {
            Ok(blockers) => Ok(blockers),
            Err(error) => {
                let blocked_issues_map =
                    self.recover_blocked_issues_map("get_blockers_parse", &error)?;
                Ok(blocked_issues_map
                    .get(issue_id)
                    .map_or_else(Vec::new, Clone::clone))
            }
        }
    }

    /// Rebuild the blocked issues cache from scratch.
    ///
    /// This computes which issues are blocked based on their dependencies
    /// and the status of their blockers. Standard blocking edges (`blocks`,
    /// `conditional-blocks`, `waits-for`) block directly. `parent-child`
    /// does not make an open parent block a child; instead it propagates an
    /// already-blocked parent down to its descendants.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    #[allow(clippy::too_many_lines)]
    pub fn rebuild_blocked_cache(&mut self, force_rebuild: bool) -> Result<usize> {
        if !force_rebuild {
            return Ok(0);
        }
        // Disable FK enforcement before the transaction (#215).
        self.conn.execute("PRAGMA foreign_keys = OFF")?;
        let result = self.with_write_transaction(|storage| {
            let rebuilt = Self::rebuild_blocked_cache_impl(&storage.conn)?;
            Self::upsert_metadata_key_in_tx(
                &storage.conn,
                BLOCKED_CACHE_STATE_KEY,
                METADATA_EMPTY_VALUE,
            )?;
            Ok(rebuilt)
        });
        Self::finish_foreign_key_suppressed_result(&self.conn, "blocked-cache rebuild", result)
    }

    /// Rebuild the blocked cache using the caller's active transaction.
    ///
    /// Assumes FK enforcement has already been disabled by the caller's
    /// transaction wrapper.
    ///
    /// # Errors
    ///
    /// Returns an error if the rebuild fails.
    pub(crate) fn rebuild_blocked_cache_in_tx(&self) -> Result<usize> {
        let rebuilt = Self::rebuild_blocked_cache_impl(&self.conn)?;
        Self::upsert_metadata_key_in_tx(&self.conn, BLOCKED_CACHE_STATE_KEY, METADATA_EMPTY_VALUE)?;
        Ok(rebuilt)
    }

    /// Rebuild the child counters table from all existing issues.
    ///
    /// Useful after a full import or manual database manipulation.
    ///
    /// # Errors
    ///
    /// Returns an error if the rebuild fails.
    pub(crate) fn rebuild_child_counters_in_tx(&self) -> Result<usize> {
        Self::rebuild_child_counters_impl(&self.conn)
    }

    fn rebuild_child_counters_impl(conn: &Connection) -> Result<usize> {
        // Clear existing counters
        conn.execute("DELETE FROM child_counters")?;

        // Build counters only for parents that still exist. Recovered imports can
        // contain hierarchical IDs whose root parent was deleted long ago.
        let rows = conn.query("SELECT id FROM issues")?;
        let issue_ids: HashSet<String> = rows
            .iter()
            .filter_map(|row| row.get(0).and_then(SqliteValue::as_text).map(String::from))
            .collect();
        let mut max_children: HashMap<String, u32> = HashMap::new();
        let mut skipped_missing_parents = 0usize;

        for id in &issue_ids {
            let Ok(parsed) = parse_id(id) else {
                continue;
            };
            if parsed.is_root() {
                continue;
            }

            let Some(parent) = parsed.parent() else {
                skipped_missing_parents += 1;
                continue;
            };
            if !issue_ids.contains(&parent) {
                skipped_missing_parents += 1;
                continue;
            }

            let Some(&child_num) = parsed.child_path.last() else {
                skipped_missing_parents += 1;
                continue;
            };

            let entry = max_children.entry(parent).or_insert(0);
            if child_num > *entry {
                *entry = child_num;
            }
        }

        if skipped_missing_parents > 0 {
            tracing::debug!(
                skipped_missing_parents,
                "Skipped child counter rebuild for hierarchical issues whose parent ID is missing"
            );
        }

        let mut count = 0;
        for (parent_id, last_child) in max_children {
            // Explicit DELETE + INSERT instead of INSERT OR REPLACE because
            // fsqlite does not reliably support UNIQUE constraint upserts.
            conn.execute_with_params(
                "DELETE FROM child_counters WHERE parent_id = ?",
                &[SqliteValue::from(parent_id.as_str())],
            )?;
            conn.execute_with_params(
                "INSERT INTO child_counters (parent_id, last_child) VALUES (?, ?)",
                &[
                    SqliteValue::from(parent_id.as_str()),
                    SqliteValue::from(i64::from(last_child)),
                ],
            )?;
            count += 1;
        }

        Ok(count)
    }

    fn compute_blocked_issues_map_impl(conn: &Connection) -> Result<HashMap<String, Vec<String>>> {
        let mut blocked_issues_map = Self::load_direct_blockers_impl(conn)?;
        let children_by_parent = Self::load_local_parent_child_edges_impl(conn)?;

        // 1. Propagate standard blockers (blocks, conditional-blocks, waits-for)
        // from parent to children.
        Self::propagate_blocked_parents(&mut blocked_issues_map, &children_by_parent);

        // 2. Add blockers for parents with open children.
        // We do this AFTER propagation so that a parent blocked only by its children
        // does not transitively block those same children (avoiding logic cycle).
        let child_blockers = Self::load_local_open_child_blockers_impl(conn)?;
        for (parent_id, mut blockers) in child_blockers {
            blocked_issues_map
                .entry(parent_id)
                .or_default()
                .append(&mut blockers);
        }

        blocked_issues_map.retain(|_, blockers| {
            blockers.sort();
            blockers.dedup();
            !blockers.is_empty()
        });

        Ok(blocked_issues_map)
    }

    pub(crate) fn blocked_cache_projection_health(
        conn: &Connection,
    ) -> BlockedCacheProjectionHealth {
        let direct_map = Self::compute_blocked_issues_map_impl(conn).ok();
        let cached_map = Self::load_blocked_cache_projection_map(conn).ok();
        Self::compare_blocked_cache_projection(cached_map.as_ref(), direct_map.as_ref())
    }

    pub(crate) fn ready_projection_health(conn: &Connection) -> ReadyProjectionHealth {
        let cached_ready_ids = Self::query_ready_projection_ids(conn, None, true).ok();
        let direct_blocked_ids = Self::compute_blocked_issues_map_impl(conn)
            .ok()
            .map(|map| map.into_keys().collect::<HashSet<_>>());
        let direct_ready_ids = direct_blocked_ids.as_ref().and_then(|blocked_ids| {
            Self::query_ready_projection_ids(conn, Some(blocked_ids), false).ok()
        });
        Self::compare_ready_projection(cached_ready_ids.as_ref(), direct_ready_ids.as_ref())
    }

    fn compare_blocked_cache_projection(
        cached: Option<&HashMap<String, Vec<String>>>,
        direct: Option<&HashMap<String, Vec<String>>>,
    ) -> BlockedCacheProjectionHealth {
        let direct_blocked_rows = direct.map(HashMap::len);
        let (Some(cached), Some(direct)) = (cached, direct) else {
            return BlockedCacheProjectionHealth::unavailable(direct_blocked_rows);
        };

        let missing_rows = direct.keys().filter(|id| !cached.contains_key(*id)).count();
        let extra_rows = cached.keys().filter(|id| !direct.contains_key(*id)).count();
        let mismatched_rows = direct
            .iter()
            .filter(|(id, blockers)| {
                cached
                    .get(id.as_str())
                    .is_some_and(|cached_blockers| cached_blockers != *blockers)
            })
            .count();
        let parity_status = if missing_rows == 0 && extra_rows == 0 && mismatched_rows == 0 {
            "matches"
        } else {
            "mismatch"
        };

        BlockedCacheProjectionHealth {
            parity_status: parity_status.to_string(),
            direct_blocked_rows,
            cached_missing_rows: Some(missing_rows),
            cached_extra_rows: Some(extra_rows),
            cached_mismatched_rows: Some(mismatched_rows),
        }
    }

    fn load_blocked_cache_projection_map(
        conn: &Connection,
    ) -> Result<HashMap<String, Vec<String>>> {
        let rows = conn.query("SELECT issue_id, blocked_by FROM blocked_issues_cache")?;
        let mut cached_map = HashMap::new();
        for row in &rows {
            let Some(issue_id) = row.get(0).and_then(SqliteValue::as_text) else {
                continue;
            };
            let mut blockers =
                parse_blocked_by_json(issue_id, row.get(1).and_then(SqliteValue::as_text))?;
            blockers.sort();
            blockers.dedup();
            cached_map.insert(issue_id.to_string(), blockers);
        }
        Ok(cached_map)
    }

    fn compare_ready_projection(
        cached: Option<&HashSet<String>>,
        direct: Option<&HashSet<String>>,
    ) -> ReadyProjectionHealth {
        let cached_ready_rows = cached.map(HashSet::len);
        let direct_ready_rows = direct.map(HashSet::len);
        let (Some(cached), Some(direct)) = (cached, direct) else {
            return ReadyProjectionHealth::unavailable(cached_ready_rows, direct_ready_rows);
        };

        let missing_rows = direct.difference(cached).count();
        let extra_rows = cached.difference(direct).count();
        let parity_status = if missing_rows == 0 && extra_rows == 0 {
            "matches"
        } else {
            "mismatch"
        };

        ReadyProjectionHealth {
            parity_status: parity_status.to_string(),
            cached_ready_rows,
            direct_ready_rows,
            cached_ready_missing_rows: Some(missing_rows),
            cached_ready_extra_rows: Some(extra_rows),
        }
    }

    fn query_ready_projection_ids(
        conn: &Connection,
        direct_blocked_ids: Option<&HashSet<String>>,
        exclude_blocked_in_sql: bool,
    ) -> Result<HashSet<String>> {
        let (sql, params) = Self::build_ready_issue_candidates_query(
            &ReadyFilters::default(),
            ReadySortPolicy::Priority,
            exclude_blocked_in_sql,
            false,
            ReadyIssueProjection::Command,
            false,
        );
        let rows = conn.query_with_params(&sql, &params)?;
        let mut ready_ids = HashSet::with_capacity(rows.len());
        for row in &rows {
            let Some(issue_id) = row.get(0).and_then(SqliteValue::as_text) else {
                continue;
            };
            if direct_blocked_ids.is_some_and(|blocked_ids| blocked_ids.contains(issue_id)) {
                continue;
            }
            ready_ids.insert(issue_id.to_string());
        }
        Ok(ready_ids)
    }

    fn blocker_refs_to_issue_ids(blockers: &[String]) -> Vec<String> {
        blockers
            .iter()
            .map(|blocker| {
                blocker
                    .split(':')
                    .next()
                    .unwrap_or(blocker.as_str())
                    .to_string()
            })
            .collect()
    }

    fn load_blocked_issues_from_map_with_projection(
        &self,
        blocked_issues_map: &HashMap<String, Vec<String>>,
        projection: BlockedIssueProjection,
    ) -> Result<Vec<(Issue, Vec<String>)>> {
        if blocked_issues_map.is_empty() {
            return Ok(Vec::new());
        }

        let sql = format!(
            "{} FROM issues
             WHERE status NOT IN ('closed', 'tombstone')
             ORDER BY priority ASC, created_at DESC, id ASC",
            projection.map_select_clause()
        );
        let rows = self.conn.query(&sql)?;

        let mut blocked_issues = Vec::new();
        for row in &rows {
            let issue = projection.parse_issue(row)?;
            if let Some(blockers) = blocked_issues_map.get(issue.id.as_str()) {
                blocked_issues.push((issue, blockers.clone()));
            }
        }

        Ok(blocked_issues)
    }

    fn rebuild_blocked_cache_impl(conn: &Connection) -> Result<usize> {
        let blocked_issues_map = Self::compute_blocked_issues_map_impl(conn)?;

        // Clear the cache table before repopulating with fresh entries.
        Self::reset_blocked_cache_table(conn)?;

        let mut entries = Vec::with_capacity(blocked_issues_map.len());

        for (issue_id, blockers) in blocked_issues_map {
            let blockers_json = match serde_json::to_string(&blockers) {
                Ok(blockers_json) => blockers_json,
                Err(error) => {
                    tracing::warn!(
                        issue_id = %issue_id,
                        %error,
                        "Failed to serialize blocker list; treating issue as unblocked"
                    );
                    continue;
                }
            };
            entries.push((issue_id, blockers_json));
        }

        let count = Self::insert_blocked_cache_entries(conn, &entries)?;
        tracing::debug!(blocked_count = count, "Rebuilt blocked issues cache");
        Ok(count)
    }

    fn reset_blocked_cache_table(conn: &Connection) -> Result<()> {
        // Use DELETE FROM instead of DROP TABLE + CREATE TABLE to avoid
        // frankensqlite page leak (#224): DROP TABLE leaves old root pages
        // unreachable, causing integrity_check to report 'Page N: never used'.
        //
        // The table and index are guaranteed to exist (created at schema apply
        // time via SCHEMA_SQL).  Per-entry DELETE+INSERT in
        // insert_blocked_cache_entries handles any phantom B-tree entries that
        // fsqlite may retain after bulk DELETE (#215).
        if table_exists(conn, "blocked_issues_cache") {
            conn.execute("DELETE FROM blocked_issues_cache")?;
        } else {
            // Table doesn't exist yet (fresh DB before schema fully applied,
            // or recovery scenario).  Fall back to CREATE.
            execute_batch(
                conn,
                r"
                CREATE TABLE blocked_issues_cache (
                    issue_id TEXT PRIMARY KEY,
                    blocked_by TEXT NOT NULL,
                    blocked_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP,
                    FOREIGN KEY (issue_id) REFERENCES issues(id) ON DELETE CASCADE
                );
                CREATE INDEX IF NOT EXISTS idx_blocked_cache_blocked_at
                    ON blocked_issues_cache(blocked_at);
                ",
            )?;
        }
        Ok(())
    }

    /// Incremental blocked-cache update: recompute only the entries for the
    /// given seed issue IDs and their transitive parent-child descendants.
    ///
    /// This avoids the full DELETE + INSERT cycle of `rebuild_blocked_cache_impl`
    /// when only a small number of dependency edges changed.
    fn incremental_blocked_cache_update(
        conn: &Connection,
        seed_ids: &HashSet<String>,
    ) -> Result<usize> {
        let children_by_parent = Self::load_local_parent_child_edges_impl(conn)?;
        let parents_by_child = Self::build_parents_by_child(&children_by_parent);
        let affected =
            Self::expand_blocked_cache_component(seed_ids, &children_by_parent, &parents_by_child);
        let affected_children_by_parent =
            Self::filter_parent_child_edges_for_ids(&children_by_parent, &affected);

        // Recompute only the affected component instead of rebuilding the full
        // blocker graph inside the active write transaction.
        let mut blocked_issues_map = Self::load_direct_blockers_for_ids_impl(conn, &affected)?;
        Self::propagate_blocked_parents(&mut blocked_issues_map, &affected_children_by_parent);
        let child_blockers = Self::load_local_open_child_blockers_for_ids_impl(conn, &affected)?;
        for (parent_id, mut blockers) in child_blockers {
            blocked_issues_map
                .entry(parent_id)
                .or_default()
                .append(&mut blockers);
        }

        // 3. Delete only affected rows from the cache (batched).
        let affected_vec: Vec<_> = affected.iter().collect();
        for chunk in affected_vec.chunks(BLOCKED_CACHE_DELETE_CHUNK_SIZE) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            let sql = format!(
                "DELETE FROM blocked_issues_cache WHERE issue_id IN ({})",
                placeholders.join(", ")
            );
            let params: Vec<SqliteValue> = chunk
                .iter()
                .map(|id| SqliteValue::from(id.as_str()))
                .collect();
            conn.execute_with_params(&sql, &params)?;
        }

        // 4. Re-insert only affected rows that have blockers.
        let mut entries = Vec::new();
        for id in &affected {
            if let Some(mut blockers) = blocked_issues_map.remove(id.as_str()) {
                if blockers.is_empty() {
                    continue;
                }
                blockers.sort();
                blockers.dedup();
                let blockers_json = match serde_json::to_string(&blockers) {
                    Ok(json) => json,
                    Err(error) => {
                        tracing::warn!(
                            issue_id = %id,
                            %error,
                            "Failed to serialize blocker list; treating issue as unblocked"
                        );
                        continue;
                    }
                };
                entries.push((id.clone(), blockers_json));
            }
        }

        let count = Self::insert_blocked_cache_entries(conn, &entries)?;
        tracing::debug!(
            affected_count = affected.len(),
            blocked_count = count,
            "Incremental blocked cache update"
        );
        Ok(count)
    }

    fn insert_blocked_cache_entries(
        conn: &Connection,
        entries: &[(String, String)],
    ) -> Result<usize> {
        // Callers are responsible for disabling FK enforcement before calling
        // this function.  fsqlite can surface false FK violations when its page
        // buffer pool is exhausted (#215).
        let mut count = 0;
        for (issue_id, blockers_json) in entries {
            // Explicit DELETE + INSERT instead of INSERT OR REPLACE because
            // fsqlite does not reliably support UNIQUE constraint upserts.
            conn.execute_with_params(
                "DELETE FROM blocked_issues_cache WHERE issue_id = ?",
                &[SqliteValue::from(issue_id.as_str())],
            )?;
            conn.execute_with_params(
                "INSERT INTO blocked_issues_cache (issue_id, blocked_by, blocked_at) VALUES (?, ?, CURRENT_TIMESTAMP)",
                &[
                    SqliteValue::from(issue_id.as_str()),
                    SqliteValue::from(blockers_json.as_str()),
                ],
            )?;
            count += 1;
        }
        Ok(count)
    }

    fn load_direct_blockers_impl(conn: &Connection) -> Result<HashMap<String, Vec<String>>> {
        // Exclude external dependencies from the persisted cache because their
        // status is not locally known and must be resolved at query time.
        let rows = conn.query(
            "SELECT DISTINCT d.issue_id, d.depends_on_id || ':' || COALESCE(i.status, 'unknown')
             FROM dependencies d
             LEFT JOIN issues i ON d.depends_on_id = i.id
             WHERE d.type IN ('blocks', 'conditional-blocks', 'waits-for')
               AND d.depends_on_id NOT LIKE 'external:%'
               AND (
                 i.status NOT IN ('closed', 'tombstone')
                 OR i.id IS NULL
               )
               AND (i.is_template = 0 OR i.is_template IS NULL OR i.id IS NULL)",
        )?;
        let mut blocked_issues_map: HashMap<String, Vec<String>> = HashMap::new();

        for row in &rows {
            let Some(issue_id) = row.get(0).and_then(SqliteValue::as_text) else {
                continue;
            };
            let Some(blocker_ref) = row.get(1).and_then(SqliteValue::as_text) else {
                continue;
            };
            if issue_id.is_empty() || blocker_ref.is_empty() {
                continue;
            }
            blocked_issues_map
                .entry(issue_id.to_string())
                .or_default()
                .push(blocker_ref.to_string());
        }

        Ok(blocked_issues_map)
    }

    fn load_direct_blockers_for_ids_impl(
        conn: &Connection,
        issue_ids: &HashSet<String>,
    ) -> Result<HashMap<String, Vec<String>>> {
        if issue_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let mut blocked_issues_map: HashMap<String, Vec<String>> = HashMap::new();
        let issue_ids: Vec<_> = issue_ids.iter().collect();

        for chunk in issue_ids.chunks(400) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            let sql = format!(
                "SELECT DISTINCT d.issue_id, d.depends_on_id || ':' || COALESCE(i.status, 'unknown')
                 FROM dependencies d
                 LEFT JOIN issues i ON d.depends_on_id = i.id
                 WHERE d.issue_id IN ({})
                   AND d.type IN ('blocks', 'conditional-blocks', 'waits-for')
                   AND d.depends_on_id NOT LIKE 'external:%'
                   AND (
                     i.status NOT IN ('closed', 'tombstone')
                     OR i.id IS NULL
                   )
                   AND (i.is_template = 0 OR i.is_template IS NULL OR i.id IS NULL)",
                placeholders.join(", ")
            );
            let params: Vec<SqliteValue> = chunk
                .iter()
                .map(|issue_id| SqliteValue::from(issue_id.as_str()))
                .collect();
            let rows = conn.query_with_params(&sql, &params)?;

            for row in &rows {
                let Some(issue_id) = row.get(0).and_then(SqliteValue::as_text) else {
                    continue;
                };
                let Some(blocker_ref) = row.get(1).and_then(SqliteValue::as_text) else {
                    continue;
                };
                if issue_id.is_empty() || blocker_ref.is_empty() {
                    continue;
                }
                blocked_issues_map
                    .entry(issue_id.to_string())
                    .or_default()
                    .push(blocker_ref.to_string());
            }
        }

        Ok(blocked_issues_map)
    }

    fn load_local_parent_child_edges_impl(
        conn: &Connection,
    ) -> Result<HashMap<String, Vec<String>>> {
        let edge_rows = conn.query(
            "SELECT issue_id, depends_on_id
             FROM dependencies
             WHERE type = 'parent-child'
               AND issue_id NOT LIKE 'external:%'
               AND depends_on_id NOT LIKE 'external:%'",
        )?;
        let mut children_by_parent: HashMap<String, Vec<String>> = HashMap::new();

        for row in &edge_rows {
            let Some(child_id) = row.get(0).and_then(SqliteValue::as_text) else {
                continue;
            };
            let Some(parent_id) = row.get(1).and_then(SqliteValue::as_text) else {
                continue;
            };
            children_by_parent
                .entry(parent_id.to_string())
                .or_default()
                .push(child_id.to_string());
        }

        Ok(children_by_parent)
    }

    fn build_parents_by_child(
        children_by_parent: &HashMap<String, Vec<String>>,
    ) -> HashMap<String, Vec<String>> {
        let mut parents_by_child: HashMap<String, Vec<String>> = HashMap::new();
        for (parent_id, children) in children_by_parent {
            for child_id in children {
                parents_by_child
                    .entry(child_id.clone())
                    .or_default()
                    .push(parent_id.clone());
            }
        }
        parents_by_child
    }

    fn expand_blocked_cache_component(
        seed_ids: &HashSet<String>,
        children_by_parent: &HashMap<String, Vec<String>>,
        parents_by_child: &HashMap<String, Vec<String>>,
    ) -> HashSet<String> {
        let mut affected = seed_ids.clone();
        let mut queue: Vec<String> = seed_ids.iter().cloned().collect();

        while let Some(id) = queue.pop() {
            if let Some(children) = children_by_parent.get(&id) {
                for child_id in children {
                    if affected.insert(child_id.clone()) {
                        queue.push(child_id.clone());
                    }
                }
            }

            if let Some(parents) = parents_by_child.get(&id) {
                for parent_id in parents {
                    if affected.insert(parent_id.clone()) {
                        queue.push(parent_id.clone());
                    }
                }
            }
        }

        affected
    }

    fn filter_parent_child_edges_for_ids(
        children_by_parent: &HashMap<String, Vec<String>>,
        issue_ids: &HashSet<String>,
    ) -> HashMap<String, Vec<String>> {
        let mut filtered = HashMap::new();
        for (parent_id, children) in children_by_parent {
            if !issue_ids.contains(parent_id) {
                continue;
            }
            let affected_children: Vec<String> = children
                .iter()
                .filter(|child_id| issue_ids.contains(child_id.as_str()))
                .cloned()
                .collect();
            if !affected_children.is_empty() {
                filtered.insert(parent_id.clone(), affected_children);
            }
        }
        filtered
    }

    fn load_local_open_child_blockers_impl(
        conn: &Connection,
    ) -> Result<HashMap<String, Vec<String>>> {
        // Parents are treated as "blocked by open children" ONLY when the
        // parent is an epic.  For epics that is the natural semantics: the
        // epic aggregates its children and cannot itself be closed (or
        // meaningfully worked on) while any child is still open.  For plain
        // task/feature/bug/etc. parents in a parent-child chain, the
        // tests (`parent_child_transitive_blocking`,
        // `deep_parent_child_chain_blocking`,
        // `deep_chain_beyond_50_levels_blocks_all_descendants`) and the
        // `conformance.rs` docstring describe the opposite direction:
        // children inherit a parent's *blocked* state via
        // `propagate_blocked_parents`, but an open, unblocked non-epic
        // parent does not itself become blocked just because it has open
        // children.  Restricting this rule to `p.issue_type = 'epic'`
        // preserves the epic-rollup behaviour while avoiding the
        // every-parent-with-open-kids false-blocked that was masking
        // genuine unblock transitions along a parent-child chain.
        //
        // Join on the parent issue (p) to guarantee `depends_on_id` exists
        // in the `issues` table.  The `dependencies.depends_on_id` column
        // has no foreign key (intentionally, for external refs), so
        // dangling rows can accumulate.  Without this guard the subsequent
        // INSERT into `blocked_issues_cache` (which *does* have a FK on
        // `issue_id`) fails with "FOREIGN KEY constraint failed" (#215).
        let rows = conn.query(&format!(
            "SELECT DISTINCT d.depends_on_id as parent_id, d.issue_id || '{CHILD_OPEN_BLOCKER_SUFFIX}' as blocker
             FROM dependencies d
             JOIN issues i ON d.issue_id = i.id
             JOIN issues p ON d.depends_on_id = p.id
             WHERE d.type = 'parent-child'
               AND p.issue_type = 'epic'
               AND i.status NOT IN ('closed', 'tombstone')
               AND (i.is_template = 0 OR i.is_template IS NULL)
               AND d.depends_on_id NOT LIKE 'external:%'
               AND d.issue_id NOT LIKE 'external:%'",
        ))?;
        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for row in &rows {
            let Some(parent_id) = row.get(0).and_then(SqliteValue::as_text) else {
                continue;
            };
            let Some(blocker) = row.get(1).and_then(SqliteValue::as_text) else {
                continue;
            };
            if parent_id.is_empty() || blocker.is_empty() {
                continue;
            }
            map.entry(parent_id.to_string())
                .or_default()
                .push(blocker.to_string());
        }
        Ok(map)
    }

    fn load_local_open_child_blockers_for_ids_impl(
        conn: &Connection,
        parent_ids: &HashSet<String>,
    ) -> Result<HashMap<String, Vec<String>>> {
        if parent_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        let parent_ids: Vec<_> = parent_ids.iter().collect();

        for chunk in parent_ids.chunks(400) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            // Join on the parent issue (p) to guarantee depends_on_id exists
            // in the issues table — same guard as the non-ids variant (#215).
            // Epic-only scoping matches the non-ids variant: the "parent
            // blocked by open children" rollup is epic-specific, not a
            // property of every parent-child edge.
            let sql = format!(
                "SELECT DISTINCT d.depends_on_id as parent_id, d.issue_id || '{CHILD_OPEN_BLOCKER_SUFFIX}' as blocker
                 FROM dependencies d
                 JOIN issues i ON d.issue_id = i.id
                 JOIN issues p ON d.depends_on_id = p.id
                 WHERE d.depends_on_id IN ({})
                   AND d.type = 'parent-child'
                   AND p.issue_type = 'epic'
                   AND i.status NOT IN ('closed', 'tombstone')
                   AND (i.is_template = 0 OR i.is_template IS NULL)
                   AND d.depends_on_id NOT LIKE 'external:%'
                   AND d.issue_id NOT LIKE 'external:%'",
                placeholders.join(", ")
            );
            let params: Vec<SqliteValue> = chunk
                .iter()
                .map(|parent_id| SqliteValue::from(parent_id.as_str()))
                .collect();
            let rows = conn.query_with_params(&sql, &params)?;

            for row in &rows {
                let Some(parent_id) = row.get(0).and_then(SqliteValue::as_text) else {
                    continue;
                };
                let Some(blocker) = row.get(1).and_then(SqliteValue::as_text) else {
                    continue;
                };
                if parent_id.is_empty() || blocker.is_empty() {
                    continue;
                }
                map.entry(parent_id.to_string())
                    .or_default()
                    .push(blocker.to_string());
            }
        }

        Ok(map)
    }

    fn propagate_blocked_parents(
        blocked_issues_map: &mut HashMap<String, Vec<String>>,
        children_by_parent: &HashMap<String, Vec<String>>,
    ) {
        if children_by_parent.is_empty() || blocked_issues_map.is_empty() {
            return;
        }

        let mut queue: Vec<String> = blocked_issues_map.keys().cloned().collect();
        let mut seen: HashSet<String> = HashSet::new();

        while let Some(parent_id) = queue.pop() {
            if !seen.insert(parent_id.clone()) {
                continue;
            }
            if let Some(children) = children_by_parent.get(&parent_id) {
                for child_id in children {
                    let marker = format!("{parent_id}:parent-blocked");
                    let entry = blocked_issues_map.entry(child_id.clone()).or_default();
                    if entry.contains(&marker) {
                        continue;
                    }
                    entry.push(marker);
                    queue.push(child_id.clone());
                }
            }
        }
    }

    /// Get issues that are blocked, along with what's blocking them.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_blocked_issues(&self) -> Result<Vec<(Issue, Vec<String>)>> {
        self.get_blocked_issues_with_projection(BlockedIssueProjection::Full)
    }

    /// Get blocked issues optimized for `blocked` command rendering.
    ///
    /// Hydrates only the issue columns consumed by blocked JSON/TOON/text
    /// output, avoiding large overflow-page reads for fields that never reach
    /// the user-facing result.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_blocked_issues_for_command_output(&self) -> Result<Vec<(Issue, Vec<String>)>> {
        self.get_blocked_issues_with_projection(BlockedIssueProjection::Command)
    }

    fn get_blocked_issues_with_projection(
        &self,
        projection: BlockedIssueProjection,
    ) -> Result<Vec<(Issue, Vec<String>)>> {
        // Read-only path: if the cache is stale, compute in memory instead of
        // persisting (issue #216 — read ops must not write).
        if self.blocked_cache_marked_stale()? {
            let blocked_issues_map = match Self::compute_blocked_issues_map_impl(&self.conn) {
                Ok(map) => map,
                Err(error) => {
                    self.recover_blocked_issues_map("get_blocked_issues_stale", &error)?
                }
            };
            return self
                .load_blocked_issues_from_map_with_projection(&blocked_issues_map, projection);
        }
        let sql = format!(
            "{} FROM issues i
             INNER JOIN blocked_issues_cache bc ON i.id = bc.issue_id
             WHERE i.status NOT IN ('closed', 'tombstone')
             ORDER BY i.priority ASC, i.created_at DESC, i.id ASC",
            projection.cached_select_clause()
        );
        let rows = match self.conn.query(&sql) {
            Ok(rows) => rows,
            Err(error) => {
                let blocked_issues_map =
                    self.recover_blocked_issues_map("get_blocked_issues_query", &error)?;
                return self
                    .load_blocked_issues_from_map_with_projection(&blocked_issues_map, projection);
            }
        };

        let mut blocked_issues = Vec::new();
        for row in &rows {
            let issue = projection.parse_issue(row)?;
            let blockers = match parse_blocked_by_json(
                &issue.id,
                row.get(projection.cached_blocked_by_index())
                    .and_then(SqliteValue::as_text),
            ) {
                Ok(blockers) => blockers,
                Err(error) => {
                    let blocked_issues_map =
                        self.recover_blocked_issues_map("get_blocked_issues_parse", &error)?;
                    return self.load_blocked_issues_from_map_with_projection(
                        &blocked_issues_map,
                        projection,
                    );
                }
            };
            blocked_issues.push((issue, blockers));
        }

        Ok(blocked_issues)
    }

    /// Return true unless the blocked command can safely emit an empty result.
    ///
    /// The false result is deliberately narrow: the blocked cache must be fresh,
    /// there must be no cached local blockers, and there must be no external
    /// blocking dependencies that could create command-only blocked rows.
    ///
    /// # Errors
    ///
    /// Returns an error if the blocked-cache stale marker cannot be read.
    pub fn may_have_blocked_command_results(&self) -> Result<bool> {
        if self.blocked_cache_marked_stale()? {
            return Ok(true);
        }

        let rows = match self.conn.query(
            "SELECT
                 EXISTS(SELECT 1 FROM blocked_issues_cache LIMIT 1),
                 EXISTS(
                     SELECT 1
                     FROM dependencies INDEXED BY idx_dependencies_depends_on
                     WHERE depends_on_id >= 'external:'
                       AND depends_on_id < 'external;'
                       AND type IN ('blocks', 'conditional-blocks', 'waits-for')
                     LIMIT 1
                 ),
                 EXISTS(
                     SELECT 1
                     FROM dependencies d INDEXED BY idx_dependencies_issue
                     JOIN issues p ON d.depends_on_id = p.id
                     WHERE d.issue_id >= 'external:'
                       AND d.issue_id < 'external;'
                       AND d.type = 'parent-child'
                       AND p.issue_type = 'epic'
                     LIMIT 1
                 )",
        ) {
            Ok(rows) => rows,
            Err(error) => {
                tracing::debug!(
                    %error,
                    "Blocked command candidate probe failed; falling back to full query"
                );
                return Ok(true);
            }
        };

        let Some(row) = rows.first() else {
            return Ok(true);
        };
        Ok(
            row.get(0).and_then(SqliteValue::as_integer).unwrap_or(0) != 0
                || row.get(1).and_then(SqliteValue::as_integer).unwrap_or(0) != 0
                || row.get(2).and_then(SqliteValue::as_integer).unwrap_or(0) != 0,
        )
    }

    /// Check if the project has any external dependencies.
    ///
    /// # Errors
    ///
    /// Returns an error if the dependency probe query fails.
    pub fn has_external_dependencies(&self, blocking_only: bool) -> Result<bool> {
        // `external;` is the next ASCII boundary after the `external:` prefix,
        // making the range equivalent to LIKE 'external:%' under SQLite's
        // default binary TEXT collation while staying index-friendly.
        let target_sql = if blocking_only {
            "SELECT 1
             FROM dependencies INDEXED BY idx_dependencies_depends_on
             WHERE depends_on_id >= 'external:'
               AND depends_on_id < 'external;'
               AND type IN ('blocks', 'conditional-blocks', 'waits-for')
             LIMIT 1"
        } else {
            "SELECT 1
             FROM dependencies INDEXED BY idx_dependencies_depends_on
             WHERE depends_on_id >= 'external:'
               AND depends_on_id < 'external;'
             LIMIT 1"
        };
        let rows = self.conn.query(target_sql)?;
        if !rows.is_empty() {
            return Ok(true);
        }

        let parent_sql = if blocking_only {
            "SELECT 1
             FROM dependencies d INDEXED BY idx_dependencies_issue
             JOIN issues p ON d.depends_on_id = p.id
             WHERE d.issue_id >= 'external:'
               AND d.issue_id < 'external;'
               AND d.type = 'parent-child'
               AND p.issue_type = 'epic'
             LIMIT 1"
        } else {
            "SELECT 1
             FROM dependencies INDEXED BY idx_dependencies_issue
             WHERE issue_id >= 'external:'
               AND issue_id < 'external;'
               AND type = 'parent-child'
             LIMIT 1"
        };
        let rows = self.conn.query(parent_sql)?;
        Ok(!rows.is_empty())
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
        Ok(Self::resolve_external_dependency_statuses_for_ids(
            &external_ids,
            external_db_paths,
        ))
    }

    pub(crate) fn resolve_external_dependency_statuses_for_ids(
        external_ids: &HashSet<String>,
        external_db_paths: &HashMap<String, PathBuf>,
    ) -> HashMap<String, bool> {
        if external_ids.is_empty() {
            return HashMap::new();
        }

        let mut project_caps: HashMap<String, HashSet<String>> = HashMap::new();
        let mut parsed: HashMap<String, (String, String)> = HashMap::new();
        for dep_id in external_ids {
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
            let is_satisfied = parsed
                .get(dep_id.as_str())
                .is_some_and(|(project, capability)| {
                    satisfied
                        .get(project)
                        .is_some_and(|caps| caps.contains(capability))
                });
            statuses.insert(dep_id.clone(), is_satisfied);
        }

        statuses
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

        // Direct external blockers.
        // 1. Local issues blocked by external targets (standard blocking types)
        let rows = self.conn.query(
            "SELECT issue_id, depends_on_id
             FROM dependencies
             WHERE depends_on_id LIKE 'external:%'
               AND type IN ('blocks', 'conditional-blocks', 'waits-for')",
        )?;

        for row in &rows {
            let issue_id = row.get(0).and_then(SqliteValue::as_text).unwrap_or("");
            let depends_on_id = row.get(1).and_then(SqliteValue::as_text).unwrap_or("");
            let satisfied = external_statuses
                .get(depends_on_id)
                .copied()
                .unwrap_or(false);
            if !satisfied {
                blockers
                    .entry(issue_id.to_string())
                    .or_default()
                    .push(format!("{depends_on_id}:blocked"));
            }
        }

        // 2. Local epic parents blocked by external children. This mirrors
        // `load_local_open_child_blockers_impl`: child-open rollup is an epic
        // aggregation rule, not a property of every parent-child edge.
        let rows = self.conn.query(
            "SELECT d.depends_on_id, d.issue_id
             FROM dependencies d
             JOIN issues p ON d.depends_on_id = p.id
             WHERE d.issue_id LIKE 'external:%'
               AND d.type = 'parent-child'
               AND p.issue_type = 'epic'",
        )?;

        for row in &rows {
            let parent_id = row.get(0).and_then(SqliteValue::as_text).unwrap_or("");
            let child_id = row.get(1).and_then(SqliteValue::as_text).unwrap_or("");
            let satisfied = external_statuses.get(child_id).copied().unwrap_or(false);
            if !satisfied {
                blockers
                    .entry(parent_id.to_string())
                    .or_default()
                    .push(format!("{child_id}:child-blocked"));
            }
        }

        // Propagate externally blocked parents down through local parent-child relationships.
        let edge_rows = self.conn.query(
            "SELECT issue_id, depends_on_id
             FROM dependencies
             WHERE type = 'parent-child'
               AND issue_id NOT LIKE 'external:%'
               AND depends_on_id NOT LIKE 'external:%'",
        )?;
        let mut children_by_parent: HashMap<String, Vec<String>> = HashMap::new();
        for row in &edge_rows {
            let Some(child_id) = row.get(0).and_then(SqliteValue::as_text) else {
                continue;
            };
            let Some(parent_id) = row.get(1).and_then(SqliteValue::as_text) else {
                continue;
            };
            children_by_parent
                .entry(parent_id.to_string())
                .or_default()
                .push(child_id.to_string());
        }

        if !children_by_parent.is_empty() && !blockers.is_empty() {
            let mut queue: Vec<String> = blockers.keys().cloned().collect();
            let mut seen: HashSet<String> = HashSet::new();

            while let Some(parent_id) = queue.pop() {
                if !seen.insert(parent_id.clone()) {
                    continue;
                }
                if let Some(children) = children_by_parent.get(&parent_id) {
                    for child_id in children {
                        let entry = blockers.entry(child_id.clone()).or_default();
                        let marker = format!("{parent_id}:parent-blocked");
                        if entry.contains(&marker) {
                            continue;
                        }
                        entry.push(marker);
                        queue.push(child_id.clone());
                    }
                }
            }
        }

        for refs in blockers.values_mut() {
            refs.sort();
            refs.dedup();
        }

        Ok(blockers)
    }

    fn list_external_dependency_ids(&self, blocking_only: bool) -> Result<HashSet<String>> {
        let mut ids = HashSet::new();
        let sql = if blocking_only {
            "SELECT DISTINCT depends_on_id
             FROM dependencies
             WHERE depends_on_id LIKE 'external:%'
               AND type IN ('blocks', 'conditional-blocks', 'waits-for')
             UNION
             SELECT DISTINCT d.issue_id
             FROM dependencies d
             JOIN issues p ON d.depends_on_id = p.id
             WHERE d.issue_id LIKE 'external:%'
               AND d.type = 'parent-child'
               AND p.issue_type = 'epic'"
        } else {
            "SELECT DISTINCT depends_on_id
             FROM dependencies
             WHERE depends_on_id LIKE 'external:%'
             UNION
             SELECT DISTINCT issue_id
             FROM dependencies
             WHERE issue_id LIKE 'external:%'"
        };

        let rows = self.conn.query(sql)?;
        for row in &rows {
            if let Some(id) = row.get(0).and_then(SqliteValue::as_text) {
                ids.insert(id.to_string());
            }
        }
        Ok(ids)
    }

    /// Check if an issue ID already exists.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn id_exists(&self, id: &str) -> Result<bool> {
        Ok(Self::get_issue_from_conn(&self.conn, id)?.is_some())
    }

    /// Find issue IDs with a title that exactly matches `title`.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn find_ids_by_exact_title(&self, title: &str) -> Result<Vec<String>> {
        let rows = self.conn.query_with_params(
            "SELECT id FROM issues WHERE title = ? ORDER BY created_at ASC, id ASC",
            &[SqliteValue::from(title.trim())],
        )?;
        Ok(rows
            .iter()
            .filter_map(|row| row.get(0).and_then(SqliteValue::as_text).map(String::from))
            .collect())
    }

    fn issue_status_in_tx(conn: &Connection, id: &str) -> Result<Option<Status>> {
        Ok(Self::get_issue_from_conn(conn, id)?.map(|issue| issue.status))
    }

    fn ensure_issue_mutable_in_tx(conn: &Connection, issue_id: &str, action: &str) -> Result<()> {
        match Self::issue_status_in_tx(conn, issue_id)? {
            Some(Status::Tombstone) => Err(BeadsError::Validation {
                field: "issue_id".to_string(),
                reason: format!("cannot {action} tombstone issue: {issue_id}"),
            }),
            Some(_) => Ok(()),
            None => Err(BeadsError::IssueNotFound {
                id: issue_id.to_string(),
            }),
        }
    }

    fn ensure_dependency_target_exists_in_tx(conn: &Connection, depends_on_id: &str) -> Result<()> {
        if depends_on_id.starts_with("external:") {
            return Ok(());
        }

        match Self::issue_status_in_tx(conn, depends_on_id)? {
            Some(Status::Tombstone) => Err(BeadsError::Validation {
                field: "depends_on_id".to_string(),
                reason: format!("cannot depend on tombstone issue: {depends_on_id}"),
            }),
            Some(_) => Ok(()),
            None => Err(BeadsError::IssueNotFound {
                id: depends_on_id.to_string(),
            }),
        }
    }

    fn validate_parent_child_endpoints(
        issue_id: &str,
        depends_on_id: &str,
        dep_type: &str,
    ) -> Result<()> {
        if dep_type.eq_ignore_ascii_case("parent-child")
            && (issue_id.starts_with("external:") || depends_on_id.starts_with("external:"))
        {
            let (field, endpoint) = if issue_id.starts_with("external:") {
                ("issue_id", issue_id)
            } else {
                ("depends_on_id", depends_on_id)
            };
            return Err(BeadsError::Validation {
                field: field.to_string(),
                reason: format!("parent-child dependencies must link local issues: {endpoint}"),
            });
        }

        Ok(())
    }

    fn canonical_standard_dependency_type(dep_type: &str) -> Option<&'static str> {
        match dep_type.to_ascii_lowercase().as_str() {
            "blocks" => Some("blocks"),
            "parent-child" => Some("parent-child"),
            "conditional-blocks" => Some("conditional-blocks"),
            "waits-for" => Some("waits-for"),
            "related" => Some("related"),
            "discovered-from" => Some("discovered-from"),
            "replies-to" => Some("replies-to"),
            "relates-to" => Some("relates-to"),
            "duplicates" => Some("duplicates"),
            "supersedes" => Some("supersedes"),
            "caused-by" => Some("caused-by"),
            _ => None,
        }
    }

    fn validate_new_parent_child_parent_in_tx(
        conn: &Connection,
        issue_id: &str,
        depends_on_id: &str,
    ) -> Result<bool> {
        let existing_parent = conn
            .query_with_params(
                "SELECT depends_on_id FROM dependencies WHERE issue_id = ? AND type COLLATE NOCASE = 'parent-child' ORDER BY rowid ASC LIMIT 1",
                &[SqliteValue::from(issue_id)],
            )?
            .first()
            .and_then(|row| row.get(0).and_then(SqliteValue::as_text))
            .map(str::to_string);

        match existing_parent {
            Some(existing_parent) if existing_parent == depends_on_id => Ok(false),
            Some(existing_parent) => Err(BeadsError::Validation {
                field: "depends_on_id".to_string(),
                reason: format!(
                    "issue {issue_id} already has parent {existing_parent}; clear or replace the existing parent before adding {depends_on_id}"
                ),
            }),
            None => Ok(true),
        }
    }

    fn existing_dependency_targets_for_issue_ids(
        conn: &Connection,
        issue_ids: &[&str],
    ) -> Result<HashMap<String, HashSet<String>>> {
        let mut targets_by_issue_id: HashMap<String, HashSet<String>> = HashMap::new();
        for chunk in issue_ids.chunks(SQLITE_VAR_LIMIT) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            let sql = format!(
                "SELECT issue_id, depends_on_id FROM dependencies WHERE issue_id IN ({})",
                placeholders.join(",")
            );
            let params: Vec<SqliteValue> = chunk
                .iter()
                .map(|issue_id| SqliteValue::from(*issue_id))
                .collect();
            let rows = conn.query_with_params(&sql, &params)?;
            for row in &rows {
                let Some(issue_id) = row.get(0).and_then(SqliteValue::as_text) else {
                    continue;
                };
                let Some(depends_on_id) = row.get(1).and_then(SqliteValue::as_text) else {
                    continue;
                };
                targets_by_issue_id
                    .entry(issue_id.to_string())
                    .or_default()
                    .insert(depends_on_id.to_string());
            }
        }

        Ok(targets_by_issue_id)
    }

    /// Find issue IDs that end with the given hash substring.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn find_ids_by_hash(&self, hash_suffix: &str) -> Result<Vec<String>> {
        let all_ids = self.get_all_ids()?;
        Ok(crate::util::id::find_matching_ids(&all_ids, hash_suffix))
    }

    /// Count total issues in the database.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn count_issues(&self) -> Result<usize> {
        let row = self.conn.query_row("SELECT count(*) FROM issues")?;
        let count = row.get(0).and_then(SqliteValue::as_integer).unwrap_or(0);
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// Get all issue IDs in the database.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_all_ids(&self) -> Result<Vec<String>> {
        let rows = self.conn.query("SELECT id FROM issues ORDER BY id")?;
        Ok(rows
            .iter()
            .filter_map(|r| r.get(0).and_then(SqliteValue::as_text).map(String::from))
            .collect())
    }

    /// Get epic counts (total children, closed children) for all epics.
    ///
    /// Returns a map from epic ID to (`total_children`, `closed_children`).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    pub fn get_epic_counts(&self) -> Result<std::collections::HashMap<String, (usize, usize)>> {
        // Fetch raw rows and aggregate in Rust to avoid SUM(CASE WHEN ... THEN 1 ELSE 0 END)
        // which crashes fsqlite (it doesn't support non-column arguments in aggregate functions).
        let rows = self.conn.query(
            "SELECT
                d.depends_on_id AS epic_id,
                i.status
             FROM dependencies d
             JOIN issues i ON d.issue_id = i.id
             WHERE d.type = 'parent-child'
               AND (i.is_template = 0 OR i.is_template IS NULL)",
        )?;
        let mut counts: std::collections::HashMap<String, (usize, usize)> =
            std::collections::HashMap::new();
        for row in &rows {
            let epic_id = row
                .get(0)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            let status = row.get(1).and_then(SqliteValue::as_text).unwrap_or("");
            let entry = counts.entry(epic_id).or_insert((0, 0));
            entry.0 += 1; // total
            if status == "closed" || status == "tombstone" {
                entry.1 += 1; // closed
            }
        }
        Ok(counts)
    }

    /// Returns IDs of direct dot-notation children of `parent_id` that are
    /// still open or in-progress, ignoring any formally declared parent-child
    /// dep rows.
    ///
    /// Rationale: `get_epic_counts()` covers the happy path (issues created
    /// with `br create --parent ...`, which writes a `parent-child` row in
    /// `dependencies`). But legacy beads DBs, direct database migrations, or
    /// older storage versions can contain IDs like
    /// `bd-epic.1`, `bd-epic.2` that are semantically children but have no dep
    /// row. Without this check, `br close bd-epic` silently closes the parent
    /// while leaving those children orphaned.
    ///
    /// Excludes grandchildren (e.g. `bd-epic.1.1` is not a direct child of
    /// `bd-epic`). LIKE pattern specials in the parent id are escaped via
    /// `escape_like_pattern` + `ESCAPE '\\'` so any id is safe to pass.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_open_dot_notation_children(&self, parent_id: &str) -> Result<Vec<String>> {
        let escaped = escape_like_pattern(parent_id);
        let direct_prefix = format!("{escaped}.%");
        let grandchild_prefix = format!("{escaped}.%.%");
        let rows = self.conn.query_with_params(
            "SELECT i.id FROM issues i \
             WHERE i.status IN ('open', 'in_progress') \
               AND (i.is_template = 0 OR i.is_template IS NULL) \
               AND i.id LIKE ? ESCAPE '\\' \
               AND i.id NOT LIKE ? ESCAPE '\\'",
            &[
                SqliteValue::from(direct_prefix.as_str()),
                SqliteValue::from(grandchild_prefix.as_str()),
            ],
        )?;
        let mut result = Vec::with_capacity(rows.len());
        for row in &rows {
            if let Some(id) = row.get(0).and_then(SqliteValue::as_text) {
                result.push(id.to_string());
            }
        }
        Ok(result)
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
        self.add_dependency_with_metadata(issue_id, depends_on_id, dep_type, actor, None)
    }

    /// Add a dependency link with optional JSON metadata.
    ///
    /// # Errors
    ///
    /// Returns an error if the dependency is invalid, the metadata is not valid JSON,
    /// or the database update fails.
    pub fn add_dependency_with_metadata(
        &mut self,
        issue_id: &str,
        depends_on_id: &str,
        dep_type: &str,
        actor: &str,
        metadata: Option<&str>,
    ) -> Result<bool> {
        if issue_id == depends_on_id {
            return Err(BeadsError::SelfDependency {
                id: issue_id.to_string(),
            });
        }

        // Tolerate a degenerate empty/whitespace-only metadata string the same
        // way JSONL deserialization does: treat it as absent rather than
        // rejecting it as invalid JSON.
        let metadata = match metadata {
            Some(metadata) if !metadata.trim().is_empty() => {
                serde_json::from_str::<serde_json::Value>(metadata).map_err(|err| {
                    BeadsError::Validation {
                        field: "metadata".to_string(),
                        reason: format!(
                            "dependency metadata must be valid JSON for {issue_id} -> \
                             {depends_on_id} (type={dep_type}); found {metadata:?}: {err}"
                        ),
                    }
                })?;
                metadata
            }
            _ => "{}",
        };

        let dep_type = Self::canonical_standard_dependency_type(dep_type).unwrap_or(dep_type);

        Self::validate_parent_child_endpoints(issue_id, depends_on_id, dep_type)?;

        self.mutate("add_dependency", actor, |conn, ctx| {
            match Self::issue_status_in_tx(conn, issue_id)? {
                Some(Status::Tombstone) => {
                    return Err(BeadsError::Validation {
                        field: "issue_id".to_string(),
                        reason: format!("cannot add dependency from tombstone issue: {issue_id}"),
                    });
                }
                Some(_) => {}
                None => {
                    return Err(BeadsError::IssueNotFound {
                        id: issue_id.to_string(),
                    });
                }
            }
            Self::ensure_dependency_target_exists_in_tx(conn, depends_on_id)?;

            if dep_type == "parent-child"
                && !Self::validate_new_parent_child_parent_in_tx(conn, issue_id, depends_on_id)?
            {
                return Ok(false);
            }

            let existing = conn.query_with_params(
                "SELECT 1 FROM dependencies WHERE issue_id = ? AND depends_on_id = ? LIMIT 1",
                &[
                    SqliteValue::from(issue_id),
                    SqliteValue::from(depends_on_id),
                ],
            )?;
            if !existing.is_empty() {
                return Ok(false);
            }

            // Cycle check runs INSIDE the transaction (BEGIN IMMEDIATE) to
            // prevent TOCTOU races where a concurrent writer could insert an
            // edge between our check and our INSERT.
            if let Ok(dt) = dep_type.parse::<DependencyType>()
                && Self::check_dependency_cycle_for_type(
                    conn,
                    issue_id,
                    depends_on_id,
                    &dt,
                    true,
                )?
            {
                return Err(BeadsError::DependencyCycle {
                    path: format!(
                        "Adding dependency {issue_id} -> {depends_on_id} would create a cycle"
                    ),
                });
            }

            let inserted = conn.execute_with_params(
                "INSERT OR IGNORE INTO dependencies (issue_id, depends_on_id, type, created_at, created_by, metadata)
                 VALUES (?, ?, ?, ?, ?, ?)",
                &[
                    SqliteValue::from(issue_id),
                    SqliteValue::from(depends_on_id),
                    SqliteValue::from(dep_type),
                    SqliteValue::from(Utc::now().to_rfc3339()),
                    SqliteValue::from(actor),
                    SqliteValue::from(metadata),
                ],
            )?;

            if inserted == 0 {
                return Ok(false);
            }

            conn.execute_with_params(
                "UPDATE issues SET updated_at = ? WHERE id = ?",
                &[
                    SqliteValue::from(Utc::now().to_rfc3339()),
                    SqliteValue::from(issue_id),
                ],
            )?;

            ctx.record_event(
                EventType::DependencyAdded,
                issue_id,
                Some(format!("Added dependency on {depends_on_id} ({dep_type})")),
            );
            ctx.mark_dirty(issue_id);
            // Defer the blocked-cache rebuild to the next read rather than
            // doing it eagerly in a second write transaction.  This eliminates
            // the main DB lock contention source under concurrent agents:
            // previously even the incremental path held a second write lock
            // while traversing all parent-child edges in the graph.
            ctx.invalidate_cache_deferred();

            Ok(true)
        })
    }

    /// Add already-resolved import dependencies in one storage mutation.
    ///
    /// This is intentionally narrower than the interactive `dep add` path:
    /// callers must have resolved user-facing references and must not attach
    /// metadata. The method still validates endpoints, parent-child uniqueness,
    /// and the complete proposed blocking graph before inserting anything.
    ///
    /// # Errors
    ///
    /// Returns an error if any dependency is invalid, would create a cycle, or
    /// the database update fails.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn add_dependencies_bulk_for_import(
        &mut self,
        dependencies: &[BulkDependencyInsert],
        actor: &str,
    ) -> Result<usize> {
        if dependencies.is_empty() {
            return Ok(0);
        }

        self.mutate("add_dependencies_bulk_for_import", actor, |conn, ctx| {
            let mut unique_dependencies: Vec<(&BulkDependencyInsert, String)> = Vec::new();
            let mut seen_edges = HashSet::new();
            let mut proposed_parents: HashMap<&str, &str> = HashMap::new();
            let mut source_issue_ids: Vec<&str> = dependencies
                .iter()
                .map(|dep| dep.issue_id.as_str())
                .collect();
            source_issue_ids.sort_unstable();
            source_issue_ids.dedup();
            let existing_targets =
                Self::existing_dependency_targets_for_issue_ids(conn, &source_issue_ids)?;

            for dep in dependencies {
                if dep.issue_id == dep.depends_on_id {
                    return Err(BeadsError::SelfDependency {
                        id: dep.issue_id.clone(),
                    });
                }

                let dep_type = Self::canonical_standard_dependency_type(&dep.dep_type)
                    .unwrap_or(dep.dep_type.as_str())
                    .to_string();
                Self::validate_parent_child_endpoints(
                    &dep.issue_id,
                    &dep.depends_on_id,
                    &dep_type,
                )?;

                match Self::issue_status_in_tx(conn, &dep.issue_id)? {
                    Some(Status::Tombstone) => {
                        return Err(BeadsError::Validation {
                            field: "issue_id".to_string(),
                            reason: format!(
                                "cannot add dependency from tombstone issue: {}",
                                dep.issue_id
                            ),
                        });
                    }
                    Some(_) => {}
                    None => {
                        return Err(BeadsError::IssueNotFound {
                            id: dep.issue_id.clone(),
                        });
                    }
                }
                Self::ensure_dependency_target_exists_in_tx(conn, &dep.depends_on_id)?;

                if existing_targets
                    .get(dep.issue_id.as_str())
                    .is_some_and(|targets| targets.contains(dep.depends_on_id.as_str()))
                {
                    continue;
                }

                if dep_type == "parent-child" {
                    match proposed_parents.get(dep.issue_id.as_str()) {
                        Some(existing_parent)
                            if *existing_parent != dep.depends_on_id.as_str() =>
                        {
                            return Err(BeadsError::Validation {
                                field: "depends_on_id".to_string(),
                                reason: format!(
                                    "issue {} has multiple imported parents: {existing_parent} and {}",
                                    dep.issue_id, dep.depends_on_id
                                ),
                            });
                        }
                        Some(_) => {}
                        None => {
                            proposed_parents
                                .insert(dep.issue_id.as_str(), dep.depends_on_id.as_str());
                        }
                    }
                    if !Self::validate_new_parent_child_parent_in_tx(
                        conn,
                        &dep.issue_id,
                        &dep.depends_on_id,
                    )? {
                        continue;
                    }
                }

                if seen_edges.insert((dep.issue_id.clone(), dep.depends_on_id.clone())) {
                    unique_dependencies.push((dep, dep_type));
                }
            }

            let mut graph = Self::load_dependency_cycle_graph_from_conn(conn, true)?;
            for (dep, dep_type) in &unique_dependencies {
                let Ok(parsed_type) = dep_type.parse::<DependencyType>() else {
                    continue;
                };
                if !parsed_type.is_blocking() {
                    continue;
                }

                let (from, to) = if dep_type == "parent-child" {
                    (dep.depends_on_id.clone(), dep.issue_id.clone())
                } else {
                    (dep.issue_id.clone(), dep.depends_on_id.clone())
                };
                graph.entry(to.clone()).or_default();
                graph.entry(from).or_default().push(to);
            }
            for neighbors in graph.values_mut() {
                neighbors.sort();
                neighbors.dedup();
            }
            if let Some(cycle) = Self::cycle_witnesses_from_graph(&graph).into_iter().next() {
                return Err(BeadsError::DependencyCycle {
                    path: cycle.join(" -> "),
                });
            }

            let now = Utc::now().to_rfc3339();
            let mut inserted_count = 0;
            let mut touched_issue_ids = HashSet::new();

            for (dep, dep_type) in unique_dependencies {
                let inserted = conn.execute_with_params(
                    "INSERT OR IGNORE INTO dependencies (issue_id, depends_on_id, type, created_at, created_by, metadata)
                     VALUES (?, ?, ?, ?, ?, ?)",
                    &[
                        SqliteValue::from(dep.issue_id.as_str()),
                        SqliteValue::from(dep.depends_on_id.as_str()),
                        SqliteValue::from(dep_type.as_str()),
                        SqliteValue::from(now.as_str()),
                        SqliteValue::from(actor),
                        SqliteValue::from("{}"),
                    ],
                )?;

                if inserted == 0 {
                    continue;
                }

                inserted_count += 1;
                touched_issue_ids.insert(dep.issue_id.clone());
                ctx.record_event(
                    EventType::DependencyAdded,
                    &dep.issue_id,
                    Some(format!(
                        "Added dependency on {} ({})",
                        dep.depends_on_id, dep_type
                    )),
                );
                ctx.mark_dirty(&dep.issue_id);
            }

            for issue_id in &touched_issue_ids {
                conn.execute_with_params(
                    "UPDATE issues SET updated_at = ? WHERE id = ?",
                    &[
                        SqliteValue::from(now.as_str()),
                        SqliteValue::from(issue_id.as_str()),
                    ],
                )?;
            }

            if inserted_count > 0 {
                ctx.invalidate_cache_deferred();
            }

            Ok(inserted_count)
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
        self.mutate("remove_dependency", actor, |conn, ctx| {
            Self::ensure_issue_mutable_in_tx(conn, issue_id, "remove dependency from")?;

            let rows = conn.execute_with_params(
                "DELETE FROM dependencies WHERE issue_id = ? AND depends_on_id = ?",
                &[
                    SqliteValue::from(issue_id),
                    SqliteValue::from(depends_on_id),
                ],
            )?;

            if rows > 0 {
                conn.execute_with_params(
                    "UPDATE issues SET updated_at = ? WHERE id = ?",
                    &[
                        SqliteValue::from(Utc::now().to_rfc3339()),
                        SqliteValue::from(issue_id),
                    ],
                )?;

                ctx.record_event(
                    EventType::DependencyRemoved,
                    issue_id,
                    Some(format!("Removed dependency on {depends_on_id}")),
                );
                ctx.mark_dirty(issue_id);
                // Defer rebuild for the same reason as add_dependency_with_metadata.
                ctx.invalidate_cache_deferred();
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
        self.mutate("remove_all_dependencies", actor, |conn, ctx| {
            let affected_rows = conn.query_with_params(
                "SELECT DISTINCT issue_id FROM dependencies WHERE depends_on_id = ?",
                &[SqliteValue::from(issue_id)],
            )?;
            let affected: Vec<String> = affected_rows
                .iter()
                .filter_map(|r| r.get(0).and_then(SqliteValue::as_text).map(String::from))
                .collect();

            let outgoing = conn.execute_with_params(
                "DELETE FROM dependencies WHERE issue_id = ?",
                &[SqliteValue::from(issue_id)],
            )?;
            let incoming = conn.execute_with_params(
                "DELETE FROM dependencies WHERE depends_on_id = ?",
                &[SqliteValue::from(issue_id)],
            )?;
            let total = outgoing + incoming;

            if total > 0 {
                let now = Utc::now().to_rfc3339();

                conn.execute_with_params(
                    "UPDATE issues SET updated_at = ? WHERE id = ?",
                    &[SqliteValue::from(now.as_str()), SqliteValue::from(issue_id)],
                )?;

                for chunk in affected.chunks(400) {
                    for id in chunk {
                        conn.execute_with_params(
                            "UPDATE issues SET updated_at = ? WHERE id = ?",
                            &[
                                SqliteValue::from(now.as_str()),
                                SqliteValue::from(id.as_str()),
                            ],
                        )?;
                    }
                }

                ctx.record_event(
                    EventType::DependencyRemoved,
                    issue_id,
                    Some(format!("Removed {total} dependency links")),
                );
                ctx.mark_dirty(issue_id);
                for affected_id in &affected {
                    ctx.mark_dirty(affected_id);
                }
                let mut cache_ids: Vec<&str> = Vec::with_capacity(affected.len() + 1);
                cache_ids.push(issue_id);
                cache_ids.extend(affected.iter().map(String::as_str));
                ctx.invalidate_cache_for(&cache_ids);
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
        self.mutate("remove_parent", actor, |conn, ctx| {
            Self::ensure_issue_mutable_in_tx(conn, issue_id, "clear parent from")?;

            let previous_parent_rows = conn.query_with_params(
                "SELECT depends_on_id FROM dependencies WHERE issue_id = ? AND type = 'parent-child' ORDER BY rowid ASC",
                &[SqliteValue::from(issue_id)],
            )?;
            let previous_parents = previous_parent_rows
                .iter()
                .filter_map(|row| row.get(0).and_then(SqliteValue::as_text).map(str::to_string))
                .collect::<Vec<_>>();

            let rows = conn.execute_with_params(
                "DELETE FROM dependencies WHERE issue_id = ? AND type = 'parent-child'",
                &[SqliteValue::from(issue_id)],
            )?;

            if rows > 0 {
                conn.execute_with_params(
                    "UPDATE issues SET updated_at = ? WHERE id = ?",
                    &[
                        SqliteValue::from(Utc::now().to_rfc3339()),
                        SqliteValue::from(issue_id),
                    ],
                )?;

                ctx.record_event(
                    EventType::DependencyRemoved,
                    issue_id,
                    Some("Removed parent".to_string()),
                );
                ctx.mark_dirty(issue_id);
                let mut cache_ids = vec![issue_id];
                for previous_parent in &previous_parents {
                    let previous_parent = previous_parent.as_str();
                    if !cache_ids.contains(&previous_parent) {
                        cache_ids.push(previous_parent);
                    }
                }
                ctx.invalidate_cache_for(&cache_ids);
            }

            Ok(rows > 0)
        })
    }

    /// Set parent for an issue (replace existing).
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails or cycle detected.
    pub fn set_parent(
        &mut self,
        issue_id: &str,
        parent_id: Option<&str>,
        actor: &str,
    ) -> Result<()> {
        self.set_parent_with_options(issue_id, parent_id, actor, false)
    }

    /// Set parent for an issue (replace existing) with optional deferred cache rebuild.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails or cycle detected.
    pub fn set_parent_with_options(
        &mut self,
        issue_id: &str,
        parent_id: Option<&str>,
        actor: &str,
        skip_cache_rebuild: bool,
    ) -> Result<()> {
        self.mutate("set_parent", actor, |conn, ctx| {
            let action = if parent_id.is_some() {
                "set parent on"
            } else {
                "clear parent from"
            };
            Self::ensure_issue_mutable_in_tx(conn, issue_id, action)?;

            let previous_parent_rows = conn.query_with_params(
                "SELECT depends_on_id FROM dependencies WHERE issue_id = ? AND type = 'parent-child' ORDER BY rowid ASC",
                &[SqliteValue::from(issue_id)],
            )?;
            let previous_parents = previous_parent_rows
                .iter()
                .filter_map(|row| row.get(0).and_then(SqliteValue::as_text).map(str::to_string))
                .collect::<Vec<_>>();

            if previous_parents.len() == usize::from(parent_id.is_some())
                && previous_parents.first().map(String::as_str) == parent_id
            {
                return Ok(());
            }

            // Remove existing parent
            conn.execute_with_params(
                "DELETE FROM dependencies WHERE issue_id = ? AND type = 'parent-child'",
                &[SqliteValue::from(issue_id)],
            )?;

            if let Some(pid) = parent_id {
                if pid == issue_id {
                    return Err(BeadsError::SelfDependency {
                        id: issue_id.to_string(),
                    });
                }

                Self::validate_parent_child_endpoints(issue_id, pid, "parent-child")?;
                Self::ensure_dependency_target_exists_in_tx(conn, pid)?;

                if Self::check_parent_child_cycle(conn, issue_id, pid, true)? {
                    return Err(BeadsError::DependencyCycle {
                        path: format!("Setting parent of {issue_id} to {pid} would create a cycle"),
                    });
                }

                conn.execute_with_params(
                    "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by)
                     VALUES (?, ?, 'parent-child', ?, ?)",
                    &[
                        SqliteValue::from(issue_id),
                        SqliteValue::from(pid),
                        SqliteValue::from(Utc::now().to_rfc3339()),
                        SqliteValue::from(actor),
                    ],
                )?;

                ctx.record_event(
                    EventType::DependencyAdded,
                    issue_id,
                    Some(format!("Set parent to {pid}")),
                );
            } else {
                ctx.record_event(
                    EventType::DependencyRemoved,
                    issue_id,
                    Some("Removed parent".to_string()),
                );
            }

            conn.execute_with_params(
                "UPDATE issues SET updated_at = ? WHERE id = ?",
                &[
                    SqliteValue::from(Utc::now().to_rfc3339()),
                    SqliteValue::from(issue_id),
                ],
            )?;

            ctx.mark_dirty(issue_id);
            if skip_cache_rebuild {
                ctx.invalidate_cache_deferred();
            } else {
                let mut cache_ids = vec![issue_id];
                for previous_parent in &previous_parents {
                    let previous_parent = previous_parent.as_str();
                    if !cache_ids.contains(&previous_parent) {
                        cache_ids.push(previous_parent);
                    }
                }
                if let Some(parent_id) = parent_id
                    && !cache_ids.contains(&parent_id)
                {
                    cache_ids.push(parent_id);
                }
                ctx.invalidate_cache_for(&cache_ids);
            }
            Ok(())
        })
    }

    /// Add a label to an issue.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn add_label(&mut self, issue_id: &str, label: &str, actor: &str) -> Result<bool> {
        validate_storage_label(label)?;

        self.mutate("add_label", actor, |conn, ctx| {
            match Self::issue_status_in_tx(conn, issue_id)? {
                Some(Status::Tombstone) => {
                    return Err(BeadsError::Validation {
                        field: "issue_id".to_string(),
                        reason: format!("cannot add label to tombstone issue: {issue_id}"),
                    });
                }
                Some(_) => {}
                None => {
                    return Err(BeadsError::IssueNotFound {
                        id: issue_id.to_string(),
                    });
                }
            }

            let row = conn.query_row_with_params(
                "SELECT count(*) FROM labels WHERE issue_id = ? AND label = ?",
                &[SqliteValue::from(issue_id), SqliteValue::from(label)],
            )?;
            let exists = row.get(0).and_then(SqliteValue::as_integer).unwrap_or(0);

            if exists > 0 {
                return Ok(false);
            }

            let row = conn.query_row_with_params(
                "SELECT count(*) FROM labels WHERE issue_id = ?",
                &[SqliteValue::from(issue_id)],
            )?;
            let label_count = row
                .get(0)
                .and_then(SqliteValue::as_integer)
                .and_then(|count| usize::try_from(count).ok())
                .unwrap_or(usize::MAX);
            if label_count >= ISSUE_LABEL_MAX_COUNT {
                return Err(label_count_error());
            }

            conn.execute_with_params(
                "INSERT INTO labels (issue_id, label) VALUES (?, ?)",
                &[SqliteValue::from(issue_id), SqliteValue::from(label)],
            )?;

            ctx.record_event(
                EventType::LabelAdded,
                issue_id,
                Some(format!("Added label {label}")),
            );
            ctx.mark_dirty(issue_id);

            conn.execute_with_params(
                "UPDATE issues SET updated_at = ? WHERE id = ?",
                &[
                    SqliteValue::from(Utc::now().to_rfc3339()),
                    SqliteValue::from(issue_id),
                ],
            )?;

            Ok(true)
        })
    }

    /// Add one label to many issues in a single storage mutation.
    ///
    /// Returns the set of issue IDs that actually gained the label. IDs that
    /// already had the label remain idempotent no-ops, matching [`Self::add_label`].
    ///
    /// # Errors
    ///
    /// Returns an error if any target issue is missing, tombstoned, or would
    /// exceed the per-issue label limit.
    #[allow(clippy::too_many_lines)]
    pub fn add_label_to_issues_bulk(
        &mut self,
        issue_ids: &[String],
        label: &str,
        actor: &str,
    ) -> Result<HashSet<String>> {
        validate_storage_label(label)?;
        if issue_ids.is_empty() {
            return Ok(HashSet::new());
        }

        let unique_issue_ids = dedupe_preserving_order(issue_ids);

        self.mutate("add_label_to_issues_bulk", actor, |conn, ctx| {
            let mut changed_ids = HashSet::new();
            let now_str = Utc::now().to_rfc3339();

            // The existing-label probe binds one label plus every issue id, so
            // reserve one parameter slot for the label value.
            for chunk in unique_issue_ids.chunks(SQLITE_VAR_LIMIT - 1) {
                let placeholders = vec!["?"; chunk.len()];
                let params = chunk
                    .iter()
                    .map(|id| SqliteValue::from(id.as_str()))
                    .collect::<Vec<_>>();
                let rows = conn.query_with_params(
                    &format!(
                        "SELECT id, status FROM issues WHERE id IN ({})",
                        placeholders.join(",")
                    ),
                    &params,
                )?;
                let mut statuses = HashMap::with_capacity(rows.len());
                for row in &rows {
                    let id = row
                        .get(0)
                        .and_then(SqliteValue::as_text)
                        .ok_or_else(|| {
                            BeadsError::Config(
                                "issues row missing id during bulk label add".to_string(),
                            )
                        })?
                        .to_string();
                    let status = row
                        .get(1)
                        .and_then(SqliteValue::as_text)
                        .ok_or_else(|| {
                            BeadsError::Config(format!(
                                "issues row missing status during bulk label add for {id}"
                            ))
                        })?;
                    statuses.insert(id, Status::from_str(status)?);
                }

                for issue_id in chunk {
                    match statuses.get(issue_id) {
                        Some(Status::Tombstone) => {
                            return Err(BeadsError::Validation {
                                field: "issue_id".to_string(),
                                reason: format!(
                                    "cannot add label to tombstone issue: {issue_id}"
                                ),
                            });
                        }
                        Some(_) => {}
                        None => {
                            return Err(BeadsError::IssueNotFound {
                                id: issue_id.clone(),
                            });
                        }
                    }
                }

                let mut label_params = Vec::with_capacity(chunk.len() + 1);
                label_params.push(SqliteValue::from(label));
                label_params.extend(chunk.iter().map(|id| SqliteValue::from(id.as_str())));
                let existing_rows = conn.query_with_params(
                    &format!(
                        "SELECT issue_id FROM labels WHERE label = ? AND issue_id IN ({})",
                        placeholders.join(",")
                    ),
                    &label_params,
                )?;
                let existing_ids = existing_rows
                    .iter()
                    .filter_map(|row| {
                        row.get(0)
                            .and_then(SqliteValue::as_text)
                            .map(ToString::to_string)
                    })
                    .collect::<HashSet<_>>();
                let missing_ids = chunk
                    .iter()
                    .filter(|issue_id| !existing_ids.contains(issue_id.as_str()))
                    .collect::<Vec<_>>();

                if missing_ids.is_empty() {
                    continue;
                }

                let missing_placeholders = vec!["?"; missing_ids.len()];
                let missing_params = missing_ids
                    .iter()
                    .map(|id| SqliteValue::from(id.as_str()))
                    .collect::<Vec<_>>();
                let count_rows = conn.query_with_params(
                    &format!(
                        "SELECT issue_id, COUNT(*) FROM labels WHERE issue_id IN ({}) GROUP BY issue_id",
                        missing_placeholders.join(",")
                    ),
                    &missing_params,
                )?;
                let mut label_counts = HashMap::with_capacity(count_rows.len());
                for row in &count_rows {
                    let issue_id = row
                        .get(0)
                        .and_then(SqliteValue::as_text)
                        .ok_or_else(|| {
                            BeadsError::Config(
                                "labels row missing issue_id during bulk label add".to_string(),
                            )
                        })?
                        .to_string();
                    let count = row
                        .get(1)
                        .and_then(SqliteValue::as_integer)
                        .and_then(|count| usize::try_from(count).ok())
                        .unwrap_or(0);
                    label_counts.insert(issue_id, count);
                }

                for issue_id in &missing_ids {
                    if label_counts
                        .get(issue_id.as_str())
                        .copied()
                        .unwrap_or(0)
                        >= ISSUE_LABEL_MAX_COUNT
                    {
                        return Err(label_count_error());
                    }
                }

                for issue_id in &missing_ids {
                    conn.execute_with_params(
                        "INSERT INTO labels (issue_id, label) VALUES (?, ?)",
                        &[
                            SqliteValue::from(issue_id.as_str()),
                            SqliteValue::from(label),
                        ],
                    )?;

                    ctx.record_event(
                        EventType::LabelAdded,
                        issue_id,
                        Some(format!("Added label {label}")),
                    );
                    ctx.mark_dirty(issue_id);
                    changed_ids.insert((*issue_id).clone());
                }

                for update_chunk in missing_ids.chunks(SQLITE_VAR_LIMIT - 1) {
                    let update_placeholders = vec!["?"; update_chunk.len()];
                    let mut update_params = Vec::with_capacity(update_chunk.len() + 1);
                    update_params.push(SqliteValue::from(now_str.as_str()));
                    update_params.extend(
                        update_chunk
                            .iter()
                            .map(|issue_id| SqliteValue::from(issue_id.as_str())),
                    );
                    conn.execute_with_params(
                        &format!(
                            "UPDATE issues SET updated_at = ? WHERE id IN ({})",
                            update_placeholders.join(",")
                        ),
                        &update_params,
                    )?;
                }
            }

            Ok(changed_ids)
        })
    }

    /// Remove a label from an issue.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn remove_label(&mut self, issue_id: &str, label: &str, actor: &str) -> Result<bool> {
        validate_storage_label(label)?;

        self.mutate("remove_label", actor, |conn, ctx| {
            match Self::issue_status_in_tx(conn, issue_id)? {
                Some(Status::Tombstone) => {
                    return Err(BeadsError::Validation {
                        field: "issue_id".to_string(),
                        reason: format!("cannot remove label from tombstone issue: {issue_id}"),
                    });
                }
                Some(_) => {}
                None => {
                    return Err(BeadsError::IssueNotFound {
                        id: issue_id.to_string(),
                    });
                }
            }

            let rows = conn.execute_with_params(
                "DELETE FROM labels WHERE issue_id = ? AND label = ?",
                &[SqliteValue::from(issue_id), SqliteValue::from(label)],
            )?;

            if rows > 0 {
                conn.execute_with_params(
                    "UPDATE issues SET updated_at = ? WHERE id = ?",
                    &[
                        SqliteValue::from(Utc::now().to_rfc3339()),
                        SqliteValue::from(issue_id),
                    ],
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

    /// Remove one label from many issues in a single storage mutation.
    ///
    /// Returns the set of issue IDs that actually lost the label. IDs that did
    /// not have the label remain idempotent no-ops, matching [`Self::remove_label`].
    ///
    /// # Errors
    ///
    /// Returns an error if any target issue is missing or tombstoned.
    #[allow(clippy::too_many_lines)]
    pub fn remove_label_from_issues_bulk(
        &mut self,
        issue_ids: &[String],
        label: &str,
        actor: &str,
    ) -> Result<HashSet<String>> {
        validate_storage_label(label)?;
        if issue_ids.is_empty() {
            return Ok(HashSet::new());
        }

        let unique_issue_ids = dedupe_preserving_order(issue_ids);

        self.mutate("remove_label_from_issues_bulk", actor, |conn, ctx| {
            let mut changed_ids = HashSet::new();
            let now_str = Utc::now().to_rfc3339();

            // Label lookups/deletes bind one label plus every issue id, so
            // reserve one parameter slot for the label value.
            for chunk in unique_issue_ids.chunks(SQLITE_VAR_LIMIT - 1) {
                let placeholders = vec!["?"; chunk.len()];
                let params = chunk
                    .iter()
                    .map(|id| SqliteValue::from(id.as_str()))
                    .collect::<Vec<_>>();
                let rows = conn.query_with_params(
                    &format!(
                        "SELECT id, status FROM issues WHERE id IN ({})",
                        placeholders.join(",")
                    ),
                    &params,
                )?;
                let mut statuses = HashMap::with_capacity(rows.len());
                for row in &rows {
                    let id = row
                        .get(0)
                        .and_then(SqliteValue::as_text)
                        .ok_or_else(|| {
                            BeadsError::Config(
                                "issues row missing id during bulk label remove".to_string(),
                            )
                        })?
                        .to_string();
                    let status = row.get(1).and_then(SqliteValue::as_text).ok_or_else(|| {
                        BeadsError::Config(format!(
                            "issues row missing status during bulk label remove for {id}"
                        ))
                    })?;
                    statuses.insert(id, Status::from_str(status)?);
                }

                for issue_id in chunk {
                    match statuses.get(issue_id) {
                        Some(Status::Tombstone) => {
                            return Err(BeadsError::Validation {
                                field: "issue_id".to_string(),
                                reason: format!(
                                    "cannot remove label from tombstone issue: {issue_id}"
                                ),
                            });
                        }
                        Some(_) => {}
                        None => {
                            return Err(BeadsError::IssueNotFound {
                                id: issue_id.clone(),
                            });
                        }
                    }
                }

                let mut label_params = Vec::with_capacity(chunk.len() + 1);
                label_params.push(SqliteValue::from(label));
                label_params.extend(chunk.iter().map(|id| SqliteValue::from(id.as_str())));
                let existing_rows = conn.query_with_params(
                    &format!(
                        "SELECT issue_id FROM labels WHERE label = ? AND issue_id IN ({})",
                        placeholders.join(",")
                    ),
                    &label_params,
                )?;
                let removable_ids = existing_rows
                    .iter()
                    .filter_map(|row| {
                        row.get(0)
                            .and_then(SqliteValue::as_text)
                            .map(ToString::to_string)
                    })
                    .collect::<HashSet<_>>();

                if removable_ids.is_empty() {
                    continue;
                }

                let removable_ids = chunk
                    .iter()
                    .filter(|issue_id| removable_ids.contains(issue_id.as_str()))
                    .collect::<Vec<_>>();
                let remove_placeholders = vec!["?"; removable_ids.len()];
                let mut remove_params = Vec::with_capacity(removable_ids.len() + 1);
                remove_params.push(SqliteValue::from(label));
                remove_params.extend(
                    removable_ids
                        .iter()
                        .map(|issue_id| SqliteValue::from(issue_id.as_str())),
                );
                conn.execute_with_params(
                    &format!(
                        "DELETE FROM labels WHERE label = ? AND issue_id IN ({})",
                        remove_placeholders.join(",")
                    ),
                    &remove_params,
                )?;

                for issue_id in &removable_ids {
                    ctx.record_event(
                        EventType::LabelRemoved,
                        issue_id,
                        Some(format!("Removed label {label}")),
                    );
                    ctx.mark_dirty(issue_id);
                    changed_ids.insert((*issue_id).clone());
                }

                for update_chunk in removable_ids.chunks(SQLITE_VAR_LIMIT - 1) {
                    let update_placeholders = vec!["?"; update_chunk.len()];
                    let mut update_params = Vec::with_capacity(update_chunk.len() + 1);
                    update_params.push(SqliteValue::from(now_str.as_str()));
                    update_params.extend(
                        update_chunk
                            .iter()
                            .map(|issue_id| SqliteValue::from(issue_id.as_str())),
                    );
                    conn.execute_with_params(
                        &format!(
                            "UPDATE issues SET updated_at = ? WHERE id IN ({})",
                            update_placeholders.join(",")
                        ),
                        &update_params,
                    )?;
                }
            }

            Ok(changed_ids)
        })
    }

    /// Remove all labels from an issue.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn remove_all_labels(&mut self, issue_id: &str, actor: &str) -> Result<usize> {
        self.mutate("remove_all_labels", actor, |conn, ctx| {
            Self::ensure_issue_mutable_in_tx(conn, issue_id, "remove labels from")?;

            let rows = conn.execute_with_params(
                "DELETE FROM labels WHERE issue_id = ?",
                &[SqliteValue::from(issue_id)],
            )?;

            if rows > 0 {
                conn.execute_with_params(
                    "UPDATE issues SET updated_at = ? WHERE id = ?",
                    &[
                        SqliteValue::from(Utc::now().to_rfc3339()),
                        SqliteValue::from(issue_id),
                    ],
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
        self.mutate("set_labels", actor, |conn, ctx| {
            Self::ensure_issue_mutable_in_tx(conn, issue_id, "set labels on")?;

            let old_rows = conn.query_with_params(
                "SELECT label FROM labels WHERE issue_id = ?",
                &[SqliteValue::from(issue_id)],
            )?;
            let old_labels_raw: Vec<String> = old_rows
                .iter()
                .filter_map(|r| r.get(0).and_then(SqliteValue::as_text).map(String::from))
                .collect();
            let old_labels = dedupe_preserving_order(&old_labels_raw);
            let desired_labels = dedupe_preserving_order(labels);
            validate_storage_labels(&desired_labels)?;

            let old_matches_desired = old_labels.len() == desired_labels.len()
                && old_labels
                    .iter()
                    .all(|label| desired_labels.contains(label));
            let db_has_duplicate_labels = old_labels_raw.len() != old_labels.len();

            if old_matches_desired && !db_has_duplicate_labels {
                return Ok(());
            }

            conn.execute_with_params(
                "DELETE FROM labels WHERE issue_id = ?",
                &[SqliteValue::from(issue_id)],
            )?;

            let mut seen_labels = HashSet::new();
            for label in &desired_labels {
                if !seen_labels.insert(label.as_str()) {
                    continue;
                }
                conn.execute_with_params(
                    "INSERT INTO labels (issue_id, label) VALUES (?, ?)",
                    &[
                        SqliteValue::from(issue_id),
                        SqliteValue::from(label.as_str()),
                    ],
                )?;
            }

            // Record changes
            let removed: Vec<_> = old_labels
                .iter()
                .filter(|label| !desired_labels.contains(label))
                .collect();
            let added: Vec<_> = desired_labels
                .iter()
                .filter(|label| !old_labels.contains(label))
                .collect();

            if !removed.is_empty() || !added.is_empty() || db_has_duplicate_labels {
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
                if db_has_duplicate_labels && removed.is_empty() && added.is_empty() {
                    details.push("normalized duplicate labels".to_string());
                }
                ctx.record_event(
                    EventType::Updated,
                    issue_id,
                    Some(format!("Labels {}", details.join("; "))),
                );
                ctx.mark_dirty(issue_id);

                conn.execute_with_params(
                    "UPDATE issues SET updated_at = ? WHERE id = ?",
                    &[
                        SqliteValue::from(Utc::now().to_rfc3339()),
                        SqliteValue::from(issue_id),
                    ],
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
        let rows = self.conn.query_with_params(
            "SELECT label FROM labels WHERE issue_id = ? ORDER BY label",
            &[SqliteValue::from(issue_id)],
        )?;
        Ok(rows
            .iter()
            .filter_map(|r| r.get(0).and_then(SqliteValue::as_text).map(String::from))
            .collect())
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
        // Stay below SQLite's common 999-variable ceiling while keeping the
        // default scheduler candidate window to one evidence-loading round trip.
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

            let params: Vec<SqliteValue> = chunk
                .iter()
                .map(|s| SqliteValue::from(s.as_str()))
                .collect();

            let rows = self.conn.query_with_params(&sql, &params)?;
            for row in &rows {
                let issue_id = row
                    .get(0)
                    .and_then(SqliteValue::as_text)
                    .unwrap_or("")
                    .to_string();
                let label = row
                    .get(1)
                    .and_then(SqliteValue::as_text)
                    .unwrap_or("")
                    .to_string();
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
        let rows = self
            .conn
            .query("SELECT issue_id, label FROM labels ORDER BY issue_id, label")?;

        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for row in &rows {
            let issue_id = row
                .get(0)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            let label = row
                .get(1)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            map.entry(issue_id).or_default().push(label);
        }
        Ok(map)
    }

    /// Get all raw label rows without ordering or grouping.
    ///
    /// Use this for aggregate callers that do their own counting and do not
    /// need export-stable ordering.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub(crate) fn list_label_pairs_unordered(&self) -> Result<Vec<(String, String)>> {
        let rows = self.conn.query("SELECT issue_id, label FROM labels")?;
        let mut pairs = Vec::with_capacity(rows.len());
        for row in &rows {
            let issue_id = row
                .get(0)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            let label = row
                .get(1)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            pairs.push((issue_id, label));
        }
        Ok(pairs)
    }

    /// Get labels plus dependency/dependent counts for every issue that has any
    /// list relation metadata.
    ///
    /// This is tuned for full structured list output: it keeps the same SQL
    /// scans as the separate helpers but stores everything in a single map so
    /// callers do one lookup per issue instead of three.
    ///
    /// # Errors
    ///
    /// Returns an error if any database query fails.
    pub(crate) fn get_all_list_relation_metadata(
        &self,
    ) -> Result<HashMap<String, ListRelationMetadata>> {
        let label_rows = self
            .conn
            .query("SELECT issue_id, label FROM labels ORDER BY issue_id, label")?;
        let dependency_rows = self
            .conn
            .query("SELECT issue_id, depends_on_id FROM dependencies")?;

        let capacity = label_rows
            .len()
            .saturating_add(dependency_rows.len().saturating_mul(2));
        let mut map: HashMap<String, ListRelationMetadata> = HashMap::with_capacity(capacity);

        for row in &label_rows {
            let issue_id = row
                .get(0)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            let label = row
                .get(1)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            map.entry(issue_id).or_default().labels.push(label);
        }

        for row in &dependency_rows {
            let issue_id = row
                .get(0)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            let depends_on_id = row
                .get(1)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            if !issue_id.is_empty() {
                map.entry(issue_id).or_default().dependency_count += 1;
            }
            if !depends_on_id.is_empty() {
                map.entry(depends_on_id).or_default().dependent_count += 1;
            }
        }

        Ok(map)
    }

    /// Get all labels attached to exportable issues.
    ///
    /// This mirrors the JSONL export issue filter so relation hydration does
    /// not observe rows for excluded ephemerals or wisps.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_labels_for_export(&self) -> Result<HashMap<String, Vec<String>>> {
        let rows = self.conn.query(
            "SELECT labels.issue_id, labels.label
             FROM labels
             INNER JOIN issues ON issues.id = labels.issue_id
             WHERE (issues.ephemeral = 0 OR issues.ephemeral IS NULL)
               AND issues.id NOT LIKE '%-wisp-%'
             ORDER BY labels.issue_id, labels.label",
        )?;

        let mut map: HashMap<String, Vec<String>> = HashMap::new();
        for row in &rows {
            let issue_id = row
                .get(0)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            let label = row
                .get(1)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
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
        let tombstone_rows = self
            .conn
            .query("SELECT id FROM issues WHERE status = 'tombstone'")?;
        let tombstone_ids: HashSet<String> = tombstone_rows
            .iter()
            .filter_map(|row| row.get(0).and_then(SqliteValue::as_text).map(str::to_owned))
            .collect();

        let rows = self.conn.query("SELECT label, issue_id FROM labels")?;
        let mut counts = BTreeMap::new();

        for row in &rows {
            let Some(label) = row.get(0).and_then(SqliteValue::as_text) else {
                continue;
            };
            let Some(issue_id) = row.get(1).and_then(SqliteValue::as_text) else {
                continue;
            };
            if tombstone_ids.contains(issue_id) {
                continue;
            }

            *counts.entry(label.to_string()).or_insert(0) += 1;
        }

        Ok(counts.into_iter().collect())
    }

    /// Rename a label across all issues.
    ///
    /// Returns the number of issues affected.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn rename_label(&mut self, old_name: &str, new_name: &str, actor: &str) -> Result<usize> {
        validate_storage_label(old_name)?;
        validate_storage_label(new_name)?;

        if old_name == new_name {
            return Ok(0);
        }

        self.mutate("rename_label", actor, |conn, ctx| {
            let id_rows = conn.query_with_params(
                "SELECT l.issue_id
                 FROM labels l
                 JOIN issues i ON l.issue_id = i.id
                 WHERE l.label = ? AND i.status != 'tombstone'",
                &[SqliteValue::from(old_name)],
            )?;
            let issue_ids: Vec<String> = id_rows
                .iter()
                .filter_map(|r| r.get(0).and_then(SqliteValue::as_text).map(String::from))
                .collect();

            let conflict_rows = conn.query_with_params(
                "SELECT l.issue_id
                 FROM labels l
                 JOIN issues i ON l.issue_id = i.id
                 WHERE l.label = ?
                   AND i.status != 'tombstone'
                   AND l.issue_id IN (SELECT issue_id FROM labels WHERE label = ?)",
                &[SqliteValue::from(new_name), SqliteValue::from(old_name)],
            )?;
            let conflicts: Vec<String> = conflict_rows
                .iter()
                .filter_map(|r| r.get(0).and_then(SqliteValue::as_text).map(String::from))
                .collect();

            for conflict_id in &conflicts {
                conn.execute_with_params(
                    "DELETE FROM labels WHERE issue_id = ? AND label = ?",
                    &[
                        SqliteValue::from(conflict_id.as_str()),
                        SqliteValue::from(old_name),
                    ],
                )?;
                ctx.mark_dirty(conflict_id);
            }

            let renamed = conn.execute_with_params(
                "UPDATE labels
                 SET label = ?
                 WHERE label = ?
                   AND issue_id IN (SELECT id FROM issues WHERE status != 'tombstone')",
                &[SqliteValue::from(new_name), SqliteValue::from(old_name)],
            )?;

            let now = Utc::now().to_rfc3339();
            for issue_id in &issue_ids {
                ctx.record_event(
                    EventType::LabelRemoved,
                    issue_id,
                    Some(format!("Renamed label {old_name} to {new_name}")),
                );
                ctx.mark_dirty(issue_id);

                conn.execute_with_params(
                    "UPDATE issues SET updated_at = ? WHERE id = ?",
                    &[
                        SqliteValue::from(now.as_str()),
                        SqliteValue::from(issue_id.as_str()),
                    ],
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
        let rows = self.conn.query_with_params(
            "SELECT id, issue_id, author, text, created_at
             FROM comments
             WHERE issue_id = ?
             ORDER BY created_at ASC, id ASC",
            &[SqliteValue::from(issue_id)],
        )?;

        rows.iter().map(comment_from_row).collect()
    }

    /// Get comments for multiple issues in batch.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_comments_for_issues(
        &self,
        issue_ids: &[String],
    ) -> Result<std::collections::HashMap<String, Vec<Comment>>> {
        const SQLITE_VAR_LIMIT: usize = 900;
        let mut map: std::collections::HashMap<String, Vec<Comment>> =
            std::collections::HashMap::new();

        if issue_ids.is_empty() {
            return Ok(map);
        }

        for chunk in issue_ids.chunks(SQLITE_VAR_LIMIT) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            let sql = format!(
                "SELECT id, issue_id, author, text, created_at
                 FROM comments
                 WHERE issue_id IN ({})
                 ORDER BY issue_id ASC, created_at ASC, id ASC",
                placeholders.join(",")
            );

            let params: Vec<SqliteValue> = chunk
                .iter()
                .map(|id| SqliteValue::from(id.as_str()))
                .collect();

            let rows = self.conn.query_with_params(&sql, &params)?;

            for row in &rows {
                let comment = comment_from_row(row)?;
                map.entry(comment.issue_id.clone())
                    .or_default()
                    .push(comment);
            }
        }

        Ok(map)
    }

    /// Get the latest comments for multiple issues in batch.
    ///
    /// Rows are returned in ascending timestamp order within each issue so
    /// callers can reuse the same presentation logic as [`Self::get_comments`]
    /// without materializing older comments they will discard.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_latest_comments_for_issues(
        &self,
        issue_ids: &[String],
        limit: usize,
    ) -> Result<std::collections::HashMap<String, Vec<Comment>>> {
        const SQLITE_VAR_LIMIT: usize = 899;
        let mut map: std::collections::HashMap<String, Vec<Comment>> =
            std::collections::HashMap::new();

        if issue_ids.is_empty() || limit == 0 {
            return Ok(map);
        }

        let row_limit = i64::try_from(limit).unwrap_or(i64::MAX);
        for chunk in issue_ids.chunks(SQLITE_VAR_LIMIT) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            let sql = format!(
                "SELECT id, issue_id, author, text, created_at
                 FROM (
                     SELECT id, issue_id, author, text, created_at,
                            ROW_NUMBER() OVER (
                                PARTITION BY issue_id
                                ORDER BY created_at DESC, id DESC
                            ) AS row_number
                     FROM comments
                     WHERE issue_id IN ({})
                 )
                 WHERE row_number <= ?
                 ORDER BY issue_id ASC, created_at ASC, id ASC",
                placeholders.join(",")
            );

            let mut params: Vec<SqliteValue> = chunk
                .iter()
                .map(|id| SqliteValue::from(id.as_str()))
                .collect();
            params.push(SqliteValue::from(row_limit));

            let rows = self.conn.query_with_params(&sql, &params)?;

            for row in &rows {
                let comment = comment_from_row(row)?;
                map.entry(comment.issue_id.clone())
                    .or_default()
                    .push(comment);
            }
        }

        Ok(map)
    }

    /// Count how many audit events belong to an issue.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn count_issue_events(&self, issue_id: &str) -> Result<usize> {
        let count = self
            .conn
            .query_row_with_params(
                "SELECT count(*) FROM events WHERE issue_id = ?",
                &[SqliteValue::from(issue_id)],
            )?
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap_or(0);
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// Add a comment to an issue.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn add_comment(&mut self, issue_id: &str, author: &str, text: &str) -> Result<Comment> {
        validate_new_comment(issue_id, author, text)?;

        self.mutate("add_comment", author, |conn, ctx| {
            Self::ensure_issue_mutable_in_tx(conn, issue_id, "add comment to")?;

            let comment_id = insert_comment_row(conn, issue_id, author, text)?;

            conn.execute_with_params(
                "UPDATE issues SET updated_at = ? WHERE id = ?",
                &[
                    SqliteValue::from(Utc::now().to_rfc3339()),
                    SqliteValue::from(issue_id),
                ],
            )?;

            ctx.record_event(EventType::Commented, issue_id, Some(text.to_string()));
            ctx.mark_dirty(issue_id);

            fetch_comment(conn, comment_id)
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
        let rows = self.conn.query_with_params(
            "SELECT d.depends_on_id, i.title, i.status, i.priority, d.type, i.created_at
             FROM dependencies d
             LEFT JOIN issues i ON d.depends_on_id = i.id
             WHERE d.issue_id = ?
            ORDER BY COALESCE(i.priority, 2) ASC, i.created_at DESC, d.depends_on_id ASC",
            &[SqliteValue::from(issue_id)],
        )?;

        rows.iter()
            .map(|row| dependency_metadata_from_row(row, "dependency target", true))
            .collect()
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
        let rows = self.conn.query_with_params(
            "SELECT d.issue_id, i.title, i.status, i.priority, d.type, i.created_at
             FROM dependencies d
             LEFT JOIN issues i ON d.issue_id = i.id
             WHERE d.depends_on_id = ?
            ORDER BY COALESCE(i.priority, 2) ASC, i.created_at DESC, d.issue_id ASC",
            &[SqliteValue::from(issue_id)],
        )?;

        rows.iter()
            .map(|row| dependency_metadata_from_row(row, "dependent issue", false))
            .collect()
    }

    /// Prefetch all reverse-dependency edges for blocking relationship types
    /// (`blocks`, `conditional-blocks`, `waits-for`, `parent-child`).
    ///
    /// Returns a map from `depends_on_id` → `Vec<IssueWithDependencyMetadata>`,
    /// enabling in-memory graph traversal without per-node queries.
    pub fn prefetch_blocking_dependents(
        &self,
    ) -> Result<HashMap<String, Vec<IssueWithDependencyMetadata>>> {
        let rows = self.conn.query(
            "SELECT d.depends_on_id, d.issue_id, i.title, i.status, i.priority, d.type
             FROM dependencies d
             LEFT JOIN issues i ON d.issue_id = i.id
             WHERE d.type IN ('blocks', 'conditional-blocks', 'waits-for', 'parent-child')
             ORDER BY COALESCE(i.priority, 2) ASC, i.created_at DESC, d.issue_id ASC",
        )?;

        let mut map: HashMap<String, Vec<IssueWithDependencyMetadata>> = HashMap::new();
        for row in &rows {
            let Some(depends_on_id) = row.get(0).and_then(SqliteValue::as_text) else {
                continue;
            };
            let Some(issue_id) = row.get(1).and_then(SqliteValue::as_text) else {
                continue;
            };
            let dep_type = row
                .get(5)
                .and_then(SqliteValue::as_text)
                .unwrap_or("blocks")
                .to_string();
            let title = row.get(2).and_then(SqliteValue::as_text);
            let status = row.get(3).and_then(SqliteValue::as_text);
            let priority = row.get(4).and_then(SqliteValue::as_integer);

            let meta = match (title, status, priority) {
                (Some(title), Some(status), Some(priority)) => IssueWithDependencyMetadata {
                    id: issue_id.to_string(),
                    title: title.to_string(),
                    status: parse_status(Some(status)),
                    priority: Priority(i32::try_from(priority).unwrap_or(2)),
                    dep_type,
                },
                _ => IssueWithDependencyMetadata {
                    id: issue_id.to_string(),
                    title: format!("[missing issue: {issue_id}]"),
                    status: Status::Tombstone,
                    priority: Priority::MEDIUM,
                    dep_type,
                },
            };

            map.entry(depends_on_id.to_string()).or_default().push(meta);
        }
        Ok(map)
    }

    /// Get parent issue ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_parent_id(&self, issue_id: &str) -> Result<Option<String>> {
        match self.conn.query_row_with_params(
            "SELECT depends_on_id FROM dependencies WHERE issue_id = ? AND type = 'parent-child' ORDER BY rowid DESC LIMIT 1",
            &[SqliteValue::from(issue_id)],
        ) {
            Ok(row) => Ok(row.get(0).and_then(SqliteValue::as_text).map(String::from)),
            Err(FrankenError::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Get IDs of issues that depend on this one.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_dependents(&self, issue_id: &str) -> Result<Vec<String>> {
        let rows = self.conn.query_with_params(
            "SELECT issue_id FROM dependencies WHERE depends_on_id = ?",
            &[SqliteValue::from(issue_id)],
        )?;
        Ok(rows
            .iter()
            .filter_map(|r| r.get(0).and_then(SqliteValue::as_text).map(String::from))
            .collect())
    }

    /// Get IDs of issues that block this one (respects parent-child direction).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_blocker_ids(&self, issue_id: &str) -> Result<Vec<String>> {
        let rows = self.conn.query_with_params(
            r"
            SELECT depends_on_id
            FROM dependencies
            WHERE issue_id = ?
              AND type IN ('blocks', 'conditional-blocks', 'waits-for')
            UNION
            SELECT issue_id FROM dependencies WHERE depends_on_id = ? AND type = 'parent-child'
            ",
            &[SqliteValue::from(issue_id), SqliteValue::from(issue_id)],
        )?;
        Ok(rows
            .iter()
            .filter_map(|r| r.get(0).and_then(SqliteValue::as_text).map(String::from))
            .collect())
    }

    /// Get IDs of issues that are blocked by this one (respects parent-child direction).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_blocked_issue_ids(&self, issue_id: &str) -> Result<Vec<String>> {
        let rows = self.conn.query_with_params(
            r"
            SELECT issue_id
            FROM dependencies
            WHERE depends_on_id = ?
              AND type IN ('blocks', 'conditional-blocks', 'waits-for')
            UNION
            SELECT depends_on_id FROM dependencies WHERE issue_id = ? AND type = 'parent-child'
            ",
            &[SqliteValue::from(issue_id), SqliteValue::from(issue_id)],
        )?;
        Ok(rows
            .iter()
            .filter_map(|r| r.get(0).and_then(SqliteValue::as_text).map(String::from))
            .collect())
    }

    /// Get IDs of issues that this one depends on.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_dependencies(&self, issue_id: &str) -> Result<Vec<String>> {
        let rows = self.conn.query_with_params(
            "SELECT depends_on_id FROM dependencies WHERE issue_id = ?",
            &[SqliteValue::from(issue_id)],
        )?;
        Ok(rows
            .iter()
            .filter_map(|r| r.get(0).and_then(SqliteValue::as_text).map(String::from))
            .collect())
    }

    /// Count how many dependencies an issue has.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    pub fn count_dependencies(&self, issue_id: &str) -> Result<usize> {
        let row = self.conn.query_row_with_params(
            "SELECT count(*) FROM dependencies WHERE issue_id = ?",
            &[SqliteValue::from(issue_id)],
        )?;
        let count = row.get(0).and_then(SqliteValue::as_integer).unwrap_or(0);
        Ok(count as usize)
    }

    /// Count how many issues depend on this one.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    pub fn count_dependents(&self, issue_id: &str) -> Result<usize> {
        let row = self.conn.query_row_with_params(
            "SELECT count(*) FROM dependencies WHERE depends_on_id = ?",
            &[SqliteValue::from(issue_id)],
        )?;
        let count = row.get(0).and_then(SqliteValue::as_integer).unwrap_or(0);
        Ok(count as usize)
    }

    /// Find the next available child number for a parent issue.
    ///
    /// Looks for existing issues with IDs like `{parent_id}.N` and returns the next
    /// available number. For example, if `bd-abc.1` and `bd-abc.2` exist, returns 3.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn next_child_number(&self, parent_id: &str) -> Result<u32> {
        // First, check the child_counters table (source of truth)
        match self.conn.query_row_with_params(
            "SELECT last_child FROM child_counters WHERE parent_id = ?",
            &[SqliteValue::from(parent_id)],
        ) {
            Ok(row) => {
                if let Some(last_child) = row.get(0).and_then(SqliteValue::as_integer) {
                    return Ok(u32::try_from(last_child).unwrap_or(0).saturating_add(1));
                }
            }
            Err(fsqlite_error::FrankenError::QueryReturnedNoRows) => {}
            Err(e) => return Err(e.into()),
        }

        // Fallback: Scan issues table for legacy data or missing counter
        // Find all existing child IDs matching the pattern {parent_id}.N
        // Escape LIKE wildcards in parent_id to prevent injection
        let escaped_parent = escape_like_pattern(parent_id);
        let pattern = format!("{escaped_parent}.%");
        let ids_rows = self.conn.query_with_params(
            "SELECT id FROM issues WHERE id LIKE ? ESCAPE '\\'",
            &[SqliteValue::from(pattern.as_str())],
        )?;
        let ids: Vec<String> = ids_rows
            .iter()
            .filter_map(|r| r.get(0).and_then(SqliteValue::as_text).map(String::from))
            .collect();

        // Extract child numbers and find the maximum
        let prefix_with_dot = format!("{parent_id}.");
        let max_child = ids
            .iter()
            .filter_map(|id| {
                id.strip_prefix(&prefix_with_dot)
                    .and_then(|suffix| {
                        // Handle both simple children (parent.1) and nested (parent.1.2)
                        // We only care about direct children, so take the first segment
                        suffix.split('.').next()
                    })
                    .and_then(|num_str| num_str.parse::<u32>().ok())
            })
            .max()
            .unwrap_or(0);

        // Use saturating_add to prevent overflow (extremely unlikely but safe)
        Ok(max_child.saturating_add(1))
    }

    /// Internal helper to update a child counter within a transaction.
    fn update_child_counter_in_tx(
        conn: &Connection,
        parent_id: &str,
        child_number: u32,
    ) -> Result<()> {
        // Check current value
        let current_max = match conn.query_row_with_params(
            "SELECT last_child FROM child_counters WHERE parent_id = ?",
            &[SqliteValue::from(parent_id)],
        ) {
            Ok(row) => row.get(0).and_then(SqliteValue::as_integer).unwrap_or(0),
            Err(fsqlite_error::FrankenError::QueryReturnedNoRows) => 0,
            Err(e) => return Err(e.into()),
        };

        if i64::from(child_number) > current_max {
            // DELETE + INSERT to simulate UPSERT (fsqlite limitation).
            // FK enforcement is disabled by the caller's transaction wrapper
            // to avoid false FK violations from fsqlite (#215).
            conn.execute_with_params(
                "DELETE FROM child_counters WHERE parent_id = ?",
                &[SqliteValue::from(parent_id)],
            )?;
            conn.execute_with_params(
                "INSERT INTO child_counters (parent_id, last_child) VALUES (?, ?)",
                &[
                    SqliteValue::from(parent_id),
                    SqliteValue::from(i64::from(child_number)),
                ],
            )?;
        }

        Ok(())
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

            let params: Vec<SqliteValue> = chunk
                .iter()
                .map(|s| SqliteValue::from(s.as_str()))
                .collect();

            let rows = self.conn.query_with_params(&sql, &params)?;
            for row in &rows {
                let issue_id = row
                    .get(0)
                    .and_then(SqliteValue::as_text)
                    .unwrap_or("")
                    .to_string();
                let count = row.get(1).and_then(SqliteValue::as_integer).unwrap_or(0);
                map.insert(issue_id, usize::try_from(count).unwrap_or(0));
            }
        }

        Ok(map)
    }

    /// Fetch reverse-dependency edges for a bounded set of blocking roots.
    ///
    /// Returns a map from blocker-graph root ID to dependents. Standard
    /// dependency rows use `depends_on_id` as the root; `parent-child` rows are
    /// reversed because parents are blocked by children.
    /// Unlike [`Self::prefetch_blocking_dependents`], this keeps focused graph
    /// traversals from hydrating the entire workspace dependency graph.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_blocking_dependents_for_issue_ids(
        &self,
        issue_ids: &[String],
    ) -> Result<HashMap<String, Vec<IssueWithDependencyMetadata>>> {
        if issue_ids.is_empty() {
            return Ok(HashMap::new());
        }

        let mut map: HashMap<String, Vec<IssueWithDependencyMetadata>> = HashMap::new();
        let chunk_size = SQLITE_VAR_LIMIT.saturating_div(2).max(1);
        for chunk in issue_ids.chunks(chunk_size) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            let sql = format!(
                "SELECT root_id, dependent_id, title, status, priority, type
                 FROM (
                     SELECT d.depends_on_id AS root_id,
                            d.issue_id AS dependent_id,
                            i.title AS title,
                            i.status AS status,
                            i.priority AS priority,
                            i.created_at AS created_at,
                            d.type AS type
                       FROM dependencies d
                       LEFT JOIN issues i ON d.issue_id = i.id
                      WHERE d.depends_on_id IN ({placeholders})
                        AND d.type IN ('blocks', 'conditional-blocks', 'waits-for')
                     UNION ALL
                     SELECT d.issue_id AS root_id,
                            d.depends_on_id AS dependent_id,
                            i.title AS title,
                            i.status AS status,
                            i.priority AS priority,
                            i.created_at AS created_at,
                            d.type AS type
                       FROM dependencies d
                       LEFT JOIN issues i ON d.depends_on_id = i.id
                      WHERE d.issue_id IN ({placeholders})
                        AND d.type = 'parent-child'
                 )
                 ORDER BY COALESCE(priority, 2) ASC, created_at DESC, dependent_id ASC",
                placeholders = placeholders.join(",")
            );
            let mut params: Vec<SqliteValue> = chunk
                .iter()
                .map(|issue_id| SqliteValue::from(issue_id.as_str()))
                .collect();
            params.extend(
                chunk
                    .iter()
                    .map(|issue_id| SqliteValue::from(issue_id.as_str())),
            );
            let rows = self.conn.query_with_params(&sql, &params)?;

            for row in &rows {
                let Some(root_id) = row.get(0).and_then(SqliteValue::as_text) else {
                    continue;
                };
                let Some(dependent_id) = row.get(1).and_then(SqliteValue::as_text) else {
                    continue;
                };
                let dep_type = row
                    .get(5)
                    .and_then(SqliteValue::as_text)
                    .unwrap_or("blocks")
                    .to_string();
                let title = row.get(2).and_then(SqliteValue::as_text);
                let status = row.get(3).and_then(SqliteValue::as_text);
                let priority = row.get(4).and_then(SqliteValue::as_integer);

                let meta = match (title, status, priority) {
                    (Some(title), Some(status), Some(priority)) => IssueWithDependencyMetadata {
                        id: dependent_id.to_string(),
                        title: title.to_string(),
                        status: parse_status(Some(status)),
                        priority: Priority(i32::try_from(priority).unwrap_or(2)),
                        dep_type,
                    },
                    _ => IssueWithDependencyMetadata {
                        id: dependent_id.to_string(),
                        title: format!("[missing issue: {dependent_id}]"),
                        status: Status::Tombstone,
                        priority: Priority::MEDIUM,
                        dep_type,
                    },
                };

                map.entry(root_id.to_string()).or_default().push(meta);
            }
        }

        Ok(map)
    }

    /// Count dependencies and dependents for multiple issues with one round-trip per chunk.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn count_relation_counts_for_issues(
        &self,
        issue_ids: &[String],
    ) -> Result<(HashMap<String, usize>, HashMap<String, usize>)> {
        // Stay below SQLite's common 999-variable ceiling while keeping the
        // default scheduler candidate window to one evidence-loading round trip.
        // Avoid CTE VALUES materialization, which is the primary root-page
        // collision trigger for fsqlite's MemDatabase.
        const SQLITE_VAR_LIMIT: usize = 900;

        if issue_ids.is_empty() {
            return Ok((HashMap::new(), HashMap::new()));
        }

        let mut dependency_counts: HashMap<String, usize> = HashMap::new();
        let mut dependent_counts: HashMap<String, usize> = HashMap::new();

        for chunk in issue_ids.chunks(SQLITE_VAR_LIMIT) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            let joined = placeholders.join(",");

            let params: Vec<SqliteValue> = chunk
                .iter()
                .map(|issue_id| SqliteValue::from(issue_id.as_str()))
                .collect();

            // Query dependency counts (issue_id = the issue that depends on something)
            let dep_sql = format!(
                "SELECT issue_id, COUNT(*) FROM dependencies WHERE issue_id IN ({joined}) GROUP BY issue_id"
            );
            let rows = self.conn.query_with_params(&dep_sql, &params)?;
            for row in &rows {
                let issue_id = row
                    .get(0)
                    .and_then(SqliteValue::as_text)
                    .unwrap_or("")
                    .to_string();
                let count = row.get(1).and_then(SqliteValue::as_integer).unwrap_or(0);
                if count > 0 {
                    *dependency_counts.entry(issue_id).or_insert(0) +=
                        usize::try_from(count).unwrap_or(0);
                }
            }

            // Query dependent counts (depends_on_id = the issue that others depend on)
            let dpt_sql = format!(
                "SELECT depends_on_id, COUNT(*) FROM dependencies WHERE depends_on_id IN ({joined}) GROUP BY depends_on_id"
            );
            let rows = self.conn.query_with_params(&dpt_sql, &params)?;
            for row in &rows {
                let issue_id = row
                    .get(0)
                    .and_then(SqliteValue::as_text)
                    .unwrap_or("")
                    .to_string();
                let count = row.get(1).and_then(SqliteValue::as_integer).unwrap_or(0);
                if count > 0 {
                    *dependent_counts.entry(issue_id).or_insert(0) +=
                        usize::try_from(count).unwrap_or(0);
                }
            }
        }

        Ok((dependency_counts, dependent_counts))
    }

    /// Count dependencies and dependents for every issue in the dependency table.
    ///
    /// This is faster than chunked `IN (...)` probes when the caller has already
    /// selected a large result set and only needs to project counts onto those
    /// issue IDs.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn count_all_relation_counts(
        &self,
    ) -> Result<(HashMap<String, usize>, HashMap<String, usize>)> {
        let dependency_rows = self
            .conn
            .query("SELECT issue_id, COUNT(*) FROM dependencies GROUP BY issue_id")?;
        let mut dependency_counts: HashMap<String, usize> = HashMap::new();
        for row in &dependency_rows {
            let issue_id = row
                .get(0)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            let count = row.get(1).and_then(SqliteValue::as_integer).unwrap_or(0);
            if count > 0 {
                dependency_counts.insert(issue_id, usize::try_from(count).unwrap_or(0));
            }
        }

        let dependent_rows = self
            .conn
            .query("SELECT depends_on_id, COUNT(*) FROM dependencies GROUP BY depends_on_id")?;
        let mut dependent_counts: HashMap<String, usize> = HashMap::new();
        for row in &dependent_rows {
            let issue_id = row
                .get(0)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            let count = row.get(1).and_then(SqliteValue::as_integer).unwrap_or(0);
            if count > 0 {
                dependent_counts.insert(issue_id, usize::try_from(count).unwrap_or(0));
            }
        }

        Ok((dependency_counts, dependent_counts))
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

            let params: Vec<SqliteValue> = chunk
                .iter()
                .map(|s| SqliteValue::from(s.as_str()))
                .collect();

            let rows = self.conn.query_with_params(&sql, &params)?;
            for row in &rows {
                let issue_id = row
                    .get(0)
                    .and_then(SqliteValue::as_text)
                    .unwrap_or("")
                    .to_string();
                let count = row.get(1).and_then(SqliteValue::as_integer).unwrap_or(0);
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
        match self.conn.query_row_with_params(
            "SELECT value FROM config WHERE key = ?",
            &[SqliteValue::from(key)],
        ) {
            Ok(row) => Ok(row.get(0).and_then(SqliteValue::as_text).map(String::from)),
            Err(FrankenError::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Fetch all config values from the config table.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_all_config(&self) -> Result<HashMap<String, String>> {
        let rows = self.conn.query("SELECT key, value FROM config")?;

        let mut map = HashMap::new();
        for row in &rows {
            let key = row
                .get(0)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            let value = row
                .get(1)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
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
        let stored_value = if matches!(key, "issue_prefix" | "issue-prefix" | "prefix") {
            normalize_prefix(value)
        } else {
            value.to_string()
        };
        self.with_write_transaction(|storage| {
            storage.conn.execute_with_params(
                "DELETE FROM config WHERE key = ?",
                &[SqliteValue::from(key)],
            )?;
            storage.conn.execute_with_params(
                "INSERT INTO config (key, value) VALUES (?, ?)",
                &[
                    SqliteValue::from(key),
                    SqliteValue::from(stored_value.as_str()),
                ],
            )?;
            Ok(())
        })
    }

    /// Delete a config value.
    ///
    /// Returns `true` if a value was deleted, `false` if the key didn't exist.
    ///
    /// # Errors
    ///
    /// Returns an error if the database delete fails.
    pub fn delete_config(&mut self, key: &str) -> Result<bool> {
        let deleted = self.conn.execute_with_params(
            "DELETE FROM config WHERE key = ?",
            &[SqliteValue::from(key)],
        )?;
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
                           pinned, is_template, source_repo_path, agent_context
                    FROM issues
                    WHERE (ephemeral = 0 OR ephemeral IS NULL)
                      AND id NOT LIKE '%-wisp-%'
                    ORDER BY id ASC";

        let rows = self.conn.query(sql)?;
        let mut issues = Vec::with_capacity(rows.len());
        for row in &rows {
            issues.push(Self::issue_from_row(row)?);
        }

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

        let rows = self.conn.query(
            "SELECT issue_id, depends_on_id, type, created_at, created_by, metadata, thread_id
             FROM dependencies
             ORDER BY issue_id, depends_on_id",
        )?;

        let mut map: HashMap<String, Vec<Dependency>> = HashMap::new();
        for row in &rows {
            let issue_id = row
                .get(0)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            let dep = Dependency {
                issue_id: issue_id.clone(),
                depends_on_id: row
                    .get(1)
                    .and_then(SqliteValue::as_text)
                    .unwrap_or("")
                    .to_string(),
                dep_type: row
                    .get(2)
                    .and_then(SqliteValue::as_text)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(DependencyType::Blocks),
                created_at: parse_datetime_value(row.get(3))?,
                created_by: row.get(4).and_then(SqliteValue::as_text).map(String::from),
                metadata: row.get(5).and_then(SqliteValue::as_text).map(String::from),
                thread_id: row.get(6).and_then(SqliteValue::as_text).map(String::from),
            };
            map.entry(issue_id).or_default().push(dep);
        }
        Ok(map)
    }

    /// Get all dependency records whose source issue is exportable.
    ///
    /// This keeps full-scan export hydration semantically equivalent to the
    /// previous ID-filtered batch queries.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_dependency_records_for_export(
        &self,
    ) -> Result<HashMap<String, Vec<crate::model::Dependency>>> {
        use crate::model::{Dependency, DependencyType};

        let rows = self.conn.query(
            "SELECT dependencies.issue_id, dependencies.depends_on_id, dependencies.type,
                    dependencies.created_at, dependencies.created_by, dependencies.metadata,
                    dependencies.thread_id
             FROM dependencies
             INNER JOIN issues ON issues.id = dependencies.issue_id
             WHERE (issues.ephemeral = 0 OR issues.ephemeral IS NULL)
               AND issues.id NOT LIKE '%-wisp-%'
             ORDER BY dependencies.issue_id, dependencies.depends_on_id",
        )?;

        let mut map: HashMap<String, Vec<Dependency>> = HashMap::new();
        for row in &rows {
            let issue_id = row
                .get(0)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            let dep = Dependency {
                issue_id: issue_id.clone(),
                depends_on_id: row
                    .get(1)
                    .and_then(SqliteValue::as_text)
                    .unwrap_or("")
                    .to_string(),
                dep_type: row
                    .get(2)
                    .and_then(SqliteValue::as_text)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(DependencyType::Blocks),
                created_at: parse_datetime_value(row.get(3))?,
                created_by: row.get(4).and_then(SqliteValue::as_text).map(String::from),
                metadata: row.get(5).and_then(SqliteValue::as_text).map(String::from),
                thread_id: row.get(6).and_then(SqliteValue::as_text).map(String::from),
            };
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
        let rows = self.conn.query(
            "SELECT id, issue_id, author, text, created_at
             FROM comments
             ORDER BY issue_id ASC, created_at ASC, id ASC",
        )?;

        let mut map: HashMap<String, Vec<Comment>> = HashMap::new();
        for row in &rows {
            let comment = comment_from_row(row)?;
            map.entry(comment.issue_id.clone())
                .or_default()
                .push(comment);
        }
        Ok(map)
    }

    /// Get all comments attached to exportable issues.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_comments_for_export(&self) -> Result<HashMap<String, Vec<Comment>>> {
        let rows = self.conn.query(
            "SELECT comments.id, comments.issue_id, comments.author, comments.text,
                    comments.created_at
             FROM comments
             INNER JOIN issues ON issues.id = comments.issue_id
             WHERE (issues.ephemeral = 0 OR issues.ephemeral IS NULL)
               AND issues.id NOT LIKE '%-wisp-%'
             ORDER BY comments.issue_id ASC, comments.created_at ASC, comments.id ASC",
        )?;

        let mut map: HashMap<String, Vec<Comment>> = HashMap::new();
        for row in &rows {
            let comment = comment_from_row(row)?;
            map.entry(comment.issue_id.clone())
                .or_default()
                .push(comment);
        }
        Ok(map)
    }

    /// Get the count of dirty issues (issues modified since last export).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_dirty_issue_count(&self) -> Result<usize> {
        let row = self.conn.query_row("SELECT COUNT(*) FROM dirty_issues")?;
        let count = row.get(0).and_then(SqliteValue::as_integer).unwrap_or(0);
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// Get the IDs and timestamps of dirty issues.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_dirty_issue_metadata(&self) -> Result<Vec<(String, String)>> {
        let rows = self
            .conn
            .query("SELECT issue_id, marked_at FROM dirty_issues ORDER BY marked_at")?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let id = r.get(0).and_then(SqliteValue::as_text).map(String::from)?;
                let marked_at = r.get(1).and_then(SqliteValue::as_text).map(String::from)?;
                Some((id, marked_at))
            })
            .collect())
    }

    /// Get IDs of all dirty issues (issues modified since last export).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_dirty_issue_ids(&self) -> Result<Vec<String>> {
        let rows = self
            .conn
            .query("SELECT issue_id FROM dirty_issues ORDER BY marked_at")?;
        Ok(rows
            .iter()
            .filter_map(|r| r.get(0).and_then(SqliteValue::as_text).map(String::from))
            .collect())
    }

    /// Clear dirty flags for the given issue IDs and timestamps.
    ///
    /// This is a safe version that only deletes if the timestamp matches,
    /// preventing a race condition where a concurrent update during export
    /// would otherwise have its dirty flag cleared incorrectly.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn clear_dirty_issues(&self, metadata: &[(String, String)]) -> Result<usize> {
        if metadata.is_empty() {
            return Ok(0);
        }

        let mut total_deleted = 0;
        for (id, marked_at) in metadata {
            let count = self.conn.execute_with_params(
                "DELETE FROM dirty_issues WHERE issue_id = ? AND marked_at = ?",
                &[
                    SqliteValue::from(id.as_str()),
                    SqliteValue::from(marked_at.as_str()),
                ],
            )?;
            total_deleted += count;
        }

        Ok(total_deleted)
    }

    /// Clear dirty flags for the given issue IDs WITHOUT timestamp validation (Legacy).
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn clear_dirty_issues_legacy(&mut self, issue_ids: &[String]) -> Result<usize> {
        const SQLITE_VAR_LIMIT: usize = 900;
        if issue_ids.is_empty() {
            return Ok(0);
        }

        let mut total_deleted = 0;
        for chunk in issue_ids.chunks(SQLITE_VAR_LIMIT) {
            // Delete existing entries row-by-row to avoid fsqlite IN-clause bugs
            let mut chunk_deleted = 0;
            for id in chunk {
                let deleted = self.conn.execute_with_params(
                    "DELETE FROM dirty_issues WHERE issue_id = ?",
                    &[SqliteValue::from(id.as_str())],
                )?;
                chunk_deleted += deleted;
            }
            total_deleted += chunk_deleted;
        }

        Ok(total_deleted)
    }

    /// Clear all dirty flags.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn clear_all_dirty_issues(&mut self) -> Result<usize> {
        let count = self.conn.execute("DELETE FROM dirty_issues")?;
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
        match self.conn.query_row_with_params(
            "SELECT content_hash, exported_at FROM export_hashes WHERE issue_id = ?",
            &[SqliteValue::from(issue_id)],
        ) {
            Ok(row) => {
                let hash = row
                    .get(0)
                    .and_then(SqliteValue::as_text)
                    .unwrap_or("")
                    .to_string();
                let exported = row
                    .get(1)
                    .and_then(SqliteValue::as_text)
                    .unwrap_or("")
                    .to_string();
                Ok(Some((hash, exported)))
            }
            Err(FrankenError::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Set the export hash for an issue after successful export.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn set_export_hash(&mut self, issue_id: &str, content_hash: &str) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.with_write_transaction(|storage| {
            storage.conn.execute_with_params(
                "DELETE FROM export_hashes WHERE issue_id = ?",
                &[SqliteValue::from(issue_id)],
            )?;
            storage.conn.execute_with_params(
                "INSERT INTO export_hashes (issue_id, content_hash, exported_at) VALUES (?, ?, ?)",
                &[
                    SqliteValue::from(issue_id),
                    SqliteValue::from(content_hash),
                    SqliteValue::from(now.as_str()),
                ],
            )?;
            Ok(())
        })
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

        self.with_write_transaction(|storage| storage.set_export_hashes_in_tx(exports))
    }

    /// Clear all export hashes.
    ///
    /// Call this before import to ensure fresh state.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn clear_all_export_hashes(&mut self) -> Result<usize> {
        let count = self.conn.execute("DELETE FROM export_hashes")?;
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
        const SQLITE_VAR_LIMIT: usize = 900;
        if dirty_ids.is_empty() {
            return Ok(vec![]);
        }

        let mut results = Vec::new();
        for chunk in dirty_ids.chunks(SQLITE_VAR_LIMIT) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            let sql = format!(
                "SELECT i.id FROM issues i
                 WHERE i.id IN ({})
                   AND i.deleted_at IS NULL
                   AND (
                     i.id NOT IN (SELECT issue_id FROM export_hashes)
                     OR i.content_hash != (SELECT e.content_hash FROM export_hashes e WHERE e.issue_id = i.id)
                   )
                 ORDER BY i.id",
                placeholders.join(",")
            );

            let params: Vec<SqliteValue> = chunk
                .iter()
                .map(|s| SqliteValue::from(s.as_str()))
                .collect();

            let rows = self.conn.query_with_params(&sql, &params)?;
            results.extend(
                rows.iter()
                    .filter_map(|r| r.get(0).and_then(SqliteValue::as_text).map(String::from)),
            );
        }

        results.sort();
        Ok(results)
    }

    /// Get a metadata value by key.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_metadata(&self, key: &str) -> Result<Option<String>> {
        match self.conn.query_row_with_params(
            "SELECT value FROM metadata WHERE key = ? ORDER BY rowid DESC LIMIT 1",
            &[SqliteValue::from(key)],
        ) {
            Ok(row) => Ok(row
                .get(0)
                .and_then(SqliteValue::as_text)
                .filter(|value| !value.is_empty())
                .map(String::from)),
            Err(FrankenError::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Set a metadata value.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn set_metadata(&mut self, key: &str, value: &str) -> Result<()> {
        self.with_write_transaction(|storage| {
            Self::upsert_metadata_key_in_tx(&storage.conn, key, value)?;
            Ok(())
        })
    }

    /// Set a metadata value using an internal write transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub(crate) fn set_metadata_shared(&self, key: &str, value: &str) -> Result<()> {
        Self::with_connection_write_transaction(&self.conn, |conn| {
            Self::upsert_metadata_key_in_tx(conn, key, value)?;
            Ok(())
        })
    }

    /// Delete a metadata key.
    ///
    /// # Errors
    ///
    /// Returns an error if the database update fails.
    pub fn delete_metadata(&mut self, key: &str) -> Result<bool> {
        let count = self.conn.execute_with_params(
            "DELETE FROM metadata WHERE key = ?",
            &[SqliteValue::from(key)],
        )?;
        Ok(count > 0)
    }

    /// Count issues in the database.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn count_all_issues(&self) -> Result<usize> {
        let count = self
            .conn
            .query_row("SELECT count(*) FROM issues")?
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap_or(0);
        Ok(usize::try_from(count).unwrap_or(0))
    }

    /// Count active project issues using default user-facing visibility.
    ///
    /// Active issues are non-closed issues, including deferred issues, while
    /// excluding template issues like the default command/query surfaces.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn count_active_issues(&self) -> Result<usize> {
        self.count_issues_with_filters(&ListFilters {
            include_deferred: true,
            ..ListFilters::default()
        })
    }

    /// Check whether the project has any active issues using default
    /// user-facing visibility.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn has_active_issues(&self) -> Result<bool> {
        let rows = self.conn.query(
            "SELECT 1
             FROM issues
             WHERE status NOT IN ('closed', 'tombstone')
               AND (is_template = 0 OR is_template IS NULL)
             LIMIT 1",
        )?;
        Ok(!rows.is_empty())
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

        let relation_presence = self.issue_detail_relation_presence(id)?;
        let labels = if relation_presence.has_labels {
            self.get_labels(id)?
        } else {
            vec![]
        };
        let dependencies = if relation_presence.has_dependencies {
            self.get_dependencies_with_metadata(id)?
        } else {
            vec![]
        };
        let dependents = if relation_presence.has_dependents {
            self.get_dependents_with_metadata(id)?
        } else {
            vec![]
        };
        let comments = if include_comments && relation_presence.has_comments {
            self.get_comments(id)?
        } else {
            vec![]
        };
        let events = if include_events {
            get_events(&self.conn, id, event_limit)?
        } else {
            vec![]
        };
        let parent = relation_presence.parent;

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

    fn issue_detail_relation_presence(&self, id: &str) -> Result<IssueDetailRelationPresence> {
        let row = self.conn.query_row_with_params(
            "SELECT
                 EXISTS(SELECT 1 FROM labels WHERE issue_id = ?),
                 EXISTS(SELECT 1 FROM dependencies WHERE issue_id = ?),
                 EXISTS(SELECT 1 FROM dependencies WHERE depends_on_id = ?),
                 EXISTS(SELECT 1 FROM comments WHERE issue_id = ?),
                 (SELECT depends_on_id FROM dependencies
                  WHERE issue_id = ? AND type = 'parent-child'
                  ORDER BY rowid DESC LIMIT 1)",
            &[
                SqliteValue::from(id),
                SqliteValue::from(id),
                SqliteValue::from(id),
                SqliteValue::from(id),
                SqliteValue::from(id),
            ],
        )?;

        Ok(IssueDetailRelationPresence {
            has_labels: row.get(0).and_then(SqliteValue::as_integer).unwrap_or(0) != 0,
            has_dependencies: row.get(1).and_then(SqliteValue::as_integer).unwrap_or(0) != 0,
            has_dependents: row.get(2).and_then(SqliteValue::as_integer).unwrap_or(0) != 0,
            has_comments: row.get(3).and_then(SqliteValue::as_integer).unwrap_or(0) != 0,
            parent: row.get(4).and_then(SqliteValue::as_text).map(String::from),
        })
    }

    fn issue_from_row(row: &fsqlite::Row) -> Result<Issue> {
        let get_str = |idx: usize| -> String {
            row.get(idx)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string()
        };
        let get_opt_str = |idx: usize| -> Option<String> {
            row.get(idx)
                .and_then(SqliteValue::as_text)
                .map(str::to_string)
        };
        let get_non_empty_str = |idx: usize| -> Option<String> {
            row.get(idx)
                .and_then(SqliteValue::as_text)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        #[allow(clippy::cast_possible_truncation)]
        let get_opt_i32 = |idx: usize| -> Option<i32> {
            row.get(idx)
                .and_then(SqliteValue::as_integer)
                .map(|v| v as i32)
        };
        let get_bool = |idx: usize| -> bool {
            row.get(idx).and_then(SqliteValue::as_integer).unwrap_or(0) != 0
        };
        let get_opt_datetime = |idx: usize| -> Result<Option<chrono::DateTime<chrono::Utc>>> {
            parse_opt_datetime_value(row.get(idx))
        };

        Ok(Issue {
            id: get_str(0),
            content_hash: get_opt_str(1),
            title: get_str(2),
            description: get_non_empty_str(3),
            design: get_non_empty_str(4),
            acceptance_criteria: get_non_empty_str(5),
            notes: get_non_empty_str(6),
            status: parse_status(row.get(7).and_then(SqliteValue::as_text)),
            priority: Priority(get_opt_i32(8).unwrap_or_else(|| Priority::default().0)),
            issue_type: parse_issue_type(row.get(9).and_then(SqliteValue::as_text)),
            assignee: get_non_empty_str(10),
            owner: get_non_empty_str(11),
            estimated_minutes: get_opt_i32(12),
            created_at: parse_datetime_value(row.get(13))?,
            created_by: get_non_empty_str(14),
            updated_at: parse_datetime_value(row.get(15))?,
            closed_at: get_opt_datetime(16)?,
            close_reason: get_non_empty_str(17),
            closed_by_session: get_non_empty_str(18),
            due_at: get_opt_datetime(19)?,
            defer_until: get_opt_datetime(20)?,
            external_ref: get_opt_str(21),
            source_system: get_non_empty_str(22),
            source_repo: get_non_empty_str(23),
            deleted_at: get_opt_datetime(24)?,
            deleted_by: get_non_empty_str(25),
            delete_reason: get_non_empty_str(26),
            original_type: get_non_empty_str(27),
            compaction_level: get_opt_i32(28),
            compacted_at: get_opt_datetime(29)?,
            compacted_at_commit: get_opt_str(30),
            original_size: get_opt_i32(31),
            sender: get_non_empty_str(32),
            ephemeral: get_bool(33),
            pinned: get_bool(34),
            is_template: get_bool(35),
            // Position 36 lands after `is_template` in the Full SELECT
            // and before `bc.blocked_by` in the BlockedIssue::Full
            // variant; the cached_blocked_by_index was bumped to 37
            // in lock-step so the projection-specific blocked-by
            // accessor still finds the right column.
            source_repo_path: get_non_empty_str(36),
            agent_context: get_non_empty_str(37),
            labels: vec![],
            dependencies: vec![],
            comments: vec![],
        })
    }

    fn ready_issue_from_row(row: &fsqlite::Row) -> Result<Issue> {
        let get_str = |idx: usize| -> String {
            row.get(idx)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string()
        };
        let get_non_empty_str = |idx: usize| -> Option<String> {
            row.get(idx)
                .and_then(SqliteValue::as_text)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        #[allow(clippy::cast_possible_truncation)]
        let get_opt_i32 = |idx: usize| -> Option<i32> {
            row.get(idx)
                .and_then(SqliteValue::as_integer)
                .map(|value| value as i32)
        };

        Ok(Issue {
            id: get_str(0),
            content_hash: None,
            title: get_str(1),
            description: get_non_empty_str(2),
            design: None,
            acceptance_criteria: get_non_empty_str(3),
            notes: get_non_empty_str(4),
            status: parse_status(row.get(5).and_then(SqliteValue::as_text)),
            priority: Priority(get_opt_i32(6).unwrap_or_else(|| Priority::default().0)),
            issue_type: parse_issue_type(row.get(7).and_then(SqliteValue::as_text)),
            assignee: get_non_empty_str(8),
            owner: get_non_empty_str(9),
            estimated_minutes: get_opt_i32(10),
            created_at: parse_datetime_value(row.get(11))?,
            created_by: get_non_empty_str(12),
            updated_at: parse_datetime_value(row.get(13))?,
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            due_at: None,
            defer_until: None,
            external_ref: None,
            source_system: None,
            source_repo: None,
            source_repo_path: None,
            agent_context: None,
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
        })
    }

    fn blocked_command_issue_from_row(row: &fsqlite::Row) -> Result<Issue> {
        let get_str = |idx: usize| -> String {
            row.get(idx)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string()
        };
        let get_non_empty_str = |idx: usize| -> Option<String> {
            row.get(idx)
                .and_then(SqliteValue::as_text)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        #[allow(clippy::cast_possible_truncation)]
        let get_opt_i32 = |idx: usize| -> Option<i32> {
            row.get(idx)
                .and_then(SqliteValue::as_integer)
                .map(|value| value as i32)
        };

        Ok(Issue {
            id: get_str(0),
            content_hash: None,
            title: get_str(1),
            description: get_non_empty_str(2),
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: parse_status(row.get(3).and_then(SqliteValue::as_text)),
            priority: Priority(get_opt_i32(4).unwrap_or_else(|| Priority::default().0)),
            issue_type: parse_issue_type(row.get(5).and_then(SqliteValue::as_text)),
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_at: parse_datetime_value(row.get(6))?,
            created_by: get_non_empty_str(7),
            updated_at: parse_datetime_value(row.get(8))?,
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            due_at: None,
            defer_until: None,
            external_ref: None,
            source_system: None,
            source_repo: None,
            source_repo_path: None,
            agent_context: None,
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
        })
    }

    fn stale_command_issue_from_row(row: &fsqlite::Row) -> Result<Issue> {
        let get_str = |idx: usize| -> String {
            row.get(idx)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string()
        };
        let get_non_empty_str = |idx: usize| -> Option<String> {
            row.get(idx)
                .and_then(SqliteValue::as_text)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        #[allow(clippy::cast_possible_truncation)]
        let get_opt_i32 = |idx: usize| -> Option<i32> {
            row.get(idx)
                .and_then(SqliteValue::as_integer)
                .map(|value| value as i32)
        };

        Ok(Issue {
            id: get_str(0),
            content_hash: None,
            title: get_str(1),
            description: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: parse_status(row.get(2).and_then(SqliteValue::as_text)),
            priority: Priority(get_opt_i32(3).unwrap_or_else(|| Priority::default().0)),
            issue_type: parse_issue_type(row.get(4).and_then(SqliteValue::as_text)),
            assignee: get_non_empty_str(5),
            owner: None,
            estimated_minutes: None,
            created_at: parse_datetime_value(row.get(6))?,
            created_by: None,
            updated_at: parse_datetime_value(row.get(7))?,
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            due_at: None,
            defer_until: None,
            external_ref: None,
            source_system: None,
            source_repo: None,
            source_repo_path: None,
            agent_context: None,
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
        })
    }

    fn lint_command_issue_from_row(row: &fsqlite::Row) -> Result<Issue> {
        let get_str = |idx: usize| -> String {
            row.get(idx)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string()
        };
        let get_non_empty_str = |idx: usize| -> Option<String> {
            row.get(idx)
                .and_then(SqliteValue::as_text)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };

        Ok(Issue {
            id: get_str(0),
            content_hash: None,
            title: get_str(1),
            description: get_non_empty_str(2),
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: parse_status(row.get(3).and_then(SqliteValue::as_text)),
            priority: Priority::default(),
            issue_type: parse_issue_type(row.get(4).and_then(SqliteValue::as_text)),
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_at: parse_datetime_value(row.get(5))?,
            created_by: None,
            updated_at: parse_datetime_value(row.get(6))?,
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            due_at: None,
            defer_until: None,
            external_ref: None,
            source_system: None,
            source_repo: None,
            source_repo_path: None,
            agent_context: None,
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
        })
    }

    fn search_command_issue_from_row(row: &fsqlite::Row) -> Result<Issue> {
        let get_str = |idx: usize| -> String {
            row.get(idx)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string()
        };
        let get_non_empty_str = |idx: usize| -> Option<String> {
            row.get(idx)
                .and_then(SqliteValue::as_text)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        #[allow(clippy::cast_possible_truncation)]
        let get_opt_i32 = |idx: usize| -> Option<i32> {
            row.get(idx)
                .and_then(SqliteValue::as_integer)
                .map(|value| value as i32)
        };

        Ok(Issue {
            id: get_str(0),
            content_hash: None,
            title: get_str(1),
            description: get_non_empty_str(2),
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: parse_status(row.get(3).and_then(SqliteValue::as_text)),
            priority: Priority(get_opt_i32(4).unwrap_or_else(|| Priority::default().0)),
            issue_type: parse_issue_type(row.get(5).and_then(SqliteValue::as_text)),
            assignee: get_non_empty_str(6),
            owner: None,
            estimated_minutes: None,
            created_at: parse_datetime_value(row.get(7))?,
            created_by: None,
            updated_at: parse_datetime_value(row.get(8))?,
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            due_at: None,
            defer_until: None,
            external_ref: None,
            source_system: None,
            source_repo: None,
            source_repo_path: None,
            agent_context: None,
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
        })
    }

    fn command_summary_issue_from_row(row: &fsqlite::Row) -> Result<Issue> {
        let get_str = |idx: usize| -> String {
            row.get(idx)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string()
        };
        #[allow(clippy::cast_possible_truncation)]
        let get_opt_i32 = |idx: usize| -> Option<i32> {
            row.get(idx)
                .and_then(SqliteValue::as_integer)
                .map(|value| value as i32)
        };

        Ok(Issue {
            id: get_str(0),
            content_hash: None,
            title: get_str(1),
            description: None,
            design: None,
            acceptance_criteria: None,
            notes: None,
            status: parse_status(row.get(2).and_then(SqliteValue::as_text)),
            priority: Priority(get_opt_i32(3).unwrap_or_else(|| Priority::default().0)),
            issue_type: parse_issue_type(row.get(4).and_then(SqliteValue::as_text)),
            assignee: None,
            owner: None,
            estimated_minutes: None,
            created_at: parse_datetime_value(row.get(5))?,
            created_by: None,
            updated_at: parse_datetime_value(row.get(6))?,
            closed_at: None,
            close_reason: None,
            closed_by_session: None,
            due_at: None,
            defer_until: None,
            external_ref: None,
            source_system: None,
            source_repo: None,
            source_repo_path: None,
            agent_context: None,
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
        })
    }

    fn stats_issue_from_row(row: &fsqlite::Row) -> Result<StatsIssueRow> {
        let get_str = |idx: usize| -> String {
            row.get(idx)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string()
        };
        let get_non_empty_str = |idx: usize| -> Option<String> {
            row.get(idx)
                .and_then(SqliteValue::as_text)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        #[allow(clippy::cast_possible_truncation)]
        let get_opt_i32 = |idx: usize| -> Option<i32> {
            row.get(idx)
                .and_then(SqliteValue::as_integer)
                .map(|value| value as i32)
        };
        let get_bool = |idx: usize| -> bool {
            row.get(idx).and_then(SqliteValue::as_integer).unwrap_or(0) != 0
        };
        let get_opt_datetime = |idx: usize| -> Result<Option<DateTime<Utc>>> {
            parse_opt_datetime_value(row.get(idx))
        };

        Ok(StatsIssueRow {
            id: get_str(0),
            status: parse_status(row.get(1).and_then(SqliteValue::as_text)),
            priority: Priority(get_opt_i32(2).unwrap_or_else(|| Priority::default().0)),
            issue_type: parse_issue_type(row.get(3).and_then(SqliteValue::as_text)),
            assignee: get_non_empty_str(4),
            created_at: parse_datetime_value(row.get(5))?,
            closed_at: get_opt_datetime(6)?,
            defer_until: get_opt_datetime(7)?,
            ephemeral: get_bool(8),
            pinned: get_bool(9),
            is_template: get_bool(10),
        })
    }

    fn stats_summary_issue_from_row(row: &fsqlite::Row) -> Result<StatsIssueRow> {
        let get_str = |idx: usize| -> String {
            row.get(idx)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string()
        };
        let get_bool = |idx: usize| -> bool {
            row.get(idx).and_then(SqliteValue::as_integer).unwrap_or(0) != 0
        };
        let get_opt_datetime = |idx: usize| -> Result<Option<DateTime<Utc>>> {
            parse_opt_datetime_value(row.get(idx))
        };

        Ok(StatsIssueRow {
            id: get_str(0),
            status: parse_status(row.get(1).and_then(SqliteValue::as_text)),
            priority: Priority::default(),
            issue_type: parse_issue_type(row.get(2).and_then(SqliteValue::as_text)),
            assignee: None,
            created_at: parse_datetime_value(row.get(3))?,
            closed_at: get_opt_datetime(4)?,
            defer_until: get_opt_datetime(5)?,
            ephemeral: get_bool(6),
            pinned: get_bool(7),
            is_template: get_bool(8),
        })
    }

    fn changelog_issue_from_row(row: &fsqlite::Row) -> Result<ChangelogIssueRow> {
        let get_str = |idx: usize| -> String {
            row.get(idx)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string()
        };
        #[allow(clippy::cast_possible_truncation)]
        let get_opt_i32 = |idx: usize| -> Option<i32> {
            row.get(idx)
                .and_then(SqliteValue::as_integer)
                .map(|value| value as i32)
        };

        Ok(ChangelogIssueRow {
            id: get_str(0),
            title: get_str(1),
            priority: Priority(get_opt_i32(2).unwrap_or_else(|| Priority::default().0)),
            issue_type: parse_issue_type(row.get(3).and_then(SqliteValue::as_text)),
            created_at: parse_datetime_value(row.get(4))?,
            closed_at: parse_opt_datetime_value(row.get(5))?,
        })
    }

    /// Get metadata for all active issues.
    ///
    /// This is used to pre-populate caches for graph traversals, avoiding N+1 queries.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_active_issues_metadata(
        &self,
    ) -> Result<std::collections::HashMap<String, (String, i32, String)>> {
        let sql = "SELECT id, title, priority, status FROM issues WHERE status != 'tombstone'";
        let rows = self.conn.query(sql)?;

        let mut map = std::collections::HashMap::with_capacity(rows.len());
        for row in &rows {
            let id = row
                .get(0)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            let title = row
                .get(1)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            let priority = row.get(2).and_then(SqliteValue::as_integer).map_or(2, |v| {
                i32::try_from(v).unwrap_or(if v < 0 { i32::MIN } else { i32::MAX })
            });
            let status = row
                .get(3)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            map.insert(id, (title, priority, status));
        }
        Ok(map)
    }

    /// Set metadata (in tx).
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub(crate) fn set_metadata_in_tx(&self, key: &str, value: &str) -> Result<()> {
        Self::upsert_metadata_key_in_tx(&self.conn, key, value)
    }

    /// Clear all export hashes (in tx).
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub(crate) fn clear_all_export_hashes_in_tx(&self) -> Result<usize> {
        let count = self.conn.execute("DELETE FROM export_hashes")?;
        Ok(count)
    }
}

fn finish_issue_mutation_write_probe(
    probe_result: std::result::Result<usize, FrankenError>,
    rollback_result: std::result::Result<usize, FrankenError>,
) -> Result<()> {
    match (probe_result, rollback_result) {
        // A zero-row probe update means the target issue was not
        // write-addressable through the mutation path the probe was
        // attempting — exactly the failure mode this diagnostic was
        // built to catch (issue #263). Returning Ok here would turn
        // the probe into a false-negative oracle for the storage
        // layer's visibility/write-path bug class. Surface it as the
        // primary error even when ROLLBACK also fails: a downstream
        // rollback hiccup is noise compared to "we couldn't update
        // the row we just inserted".
        (Ok(0), rollback) => {
            if let Err(rollback_err) = rollback {
                tracing::warn!(
                    error = %rollback_err,
                    "ROLLBACK failed after zero-row issue write probe"
                );
            }
            Err(BeadsError::Database(FrankenError::Internal(
                "write probe did not find issue inside mutation transaction".to_string(),
            )))
        }
        (Ok(_), Ok(_)) => Ok(()),
        (Ok(_), Err(rollback_err)) => Err(BeadsError::Database(rollback_err)),
        (Err(probe_err), Ok(_)) => Err(BeadsError::Database(probe_err)),
        (Err(probe_err), Err(rollback_err)) => {
            tracing::warn!(
                error = %rollback_err,
                "ROLLBACK failed after issue write probe"
            );
            Err(BeadsError::Database(probe_err))
        }
    }
}

/// Best-effort removal of an ephemeral temp database and its SQLite sidecars.
///
/// SQLite may create `-wal`, `-shm`, and `-journal` files next to the main
/// database file; all of them must go so nothing is left in `TMPDIR` (#299).
/// Missing files are ignored; this is invoked on teardown and on a failed
/// open, so errors are intentionally swallowed (logged at debug level).
fn remove_temp_db_files(path: &Path) {
    let mut targets = vec![path.to_path_buf()];
    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
        for suffix in ["-wal", "-shm", "-journal"] {
            targets.push(path.with_file_name(format!("{name}{suffix}")));
        }
    }
    for target in targets {
        match std::fs::remove_file(&target) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::debug!(path = %target.display(), error = %e, "failed to remove temp db file");
            }
        }
    }
}

fn database_header_user_version(path: &Path) -> Option<u32> {
    if path == Path::new(":memory:") || !path.is_file() {
        return None;
    }

    let mut file = std::fs::File::open(path).ok()?;
    let mut header = [0_u8; 100];
    file.read_exact(&mut header).ok()?;
    if &header[..16] != b"SQLite format 3\0" {
        return None;
    }

    Some(u32::from_be_bytes([
        header[60], header[61], header[62], header[63],
    ]))
}

/// Read `PRAGMA user_version` through an open connection (issue #373).
///
/// Unlike [`database_header_user_version`], which peeks raw file-header bytes,
/// this observes the *effective* schema version — including a value that lives
/// only in an uncheckpointed WAL written by another process. Relying on the
/// header alone can make a current database read as stale (header still at the
/// old version, WAL holding the new one), causing the schema to be re-applied
/// over live data. Prefer this and fall back to the header peek only when the
/// pragma cannot be read.
fn connection_user_version(conn: &Connection) -> Option<u32> {
    let row = conn.query_row("PRAGMA user_version").ok()?;
    row.get(0)
        .and_then(SqliteValue::as_integer)
        .and_then(|v| u32::try_from(v).ok())
}

fn is_transient_wal_tail_read_error(error: &dyn std::fmt::Display) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("wal file is corrupt")
        && (message.contains("short read") || message.contains("short header read"))
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
    /// Offset for pagination (number of rows to skip before applying LIMIT)
    pub offset: Option<usize>,
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

/// Closure-time policy metadata row (issue #274 Phase 1).
///
/// One row per terminal close that carried any opt-in policy data — Tier 1
/// attribution, a `--bypass-policy` waiver, or the list of gates that fired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseMetadataRow {
    pub closed_by_agent_name: Option<String>,
    pub closed_by_harness: Option<String>,
    pub closed_by_model: Option<String>,
    pub bypassed_policy: bool,
    pub bypass_reason: Option<String>,
    /// Names of gates that fired during evaluation. Always serialised as a
    /// JSON array on disk; empty when no gates fired (e.g. clean Tier 1
    /// capture, or a successful close on a project with no `policy.yaml`).
    pub policy_gates_fired: Vec<String>,
    /// Timestamp the metadata row was recorded, in ISO 8601 / RFC 3339.
    pub recorded_at: String,
}

/// Lean issue row used by the stats command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatsIssueRow {
    pub id: String,
    pub status: Status,
    pub priority: Priority,
    pub issue_type: IssueType,
    pub assignee: Option<String>,
    pub created_at: DateTime<Utc>,
    pub closed_at: Option<DateTime<Utc>>,
    pub defer_until: Option<DateTime<Utc>>,
    pub ephemeral: bool,
    pub pinned: bool,
    pub is_template: bool,
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
    /// Override the source-repo display name (typically the repo basename).
    /// `Some(Some(s))` sets it to `s`; `Some(None)` resets it to the
    /// schema default "." because the column is `NOT NULL`.
    pub source_repo: Option<Option<String>>,
    /// Override the canonical filesystem path of the repo containing `.beads`.
    /// See #289. Use `update --source-repo-path` for ad-hoc repair after a
    /// repo is moved/copied to a new machine.
    pub source_repo_path: Option<Option<String>>,
    /// Set inherited governing-instructions JSON (beads_rust#297).
    /// `Some(Some(s))` writes the JSON string `s`; `Some(None)` clears
    /// the field back to `NULL`. `None` means "do not touch this field".
    /// Validation happens at the CLI boundary; storage is opaque TEXT.
    pub agent_context: Option<Option<String>>,
    pub closed_at: Option<Option<DateTime<Utc>>>,
    pub close_reason: Option<Option<String>>,
    pub closed_by_session: Option<Option<String>>,
    pub deleted_at: Option<Option<DateTime<Utc>>>,
    pub deleted_by: Option<Option<String>>,
    pub delete_reason: Option<Option<String>>,
    /// If true, do not rebuild the blocked cache after update.
    /// Caller is responsible for rebuilding cache if needed.
    pub skip_cache_rebuild: bool,
    /// If true, verify the issue is unassigned (or assigned to `claim_actor`)
    /// inside the IMMEDIATE transaction to prevent TOCTOU races.
    pub expect_unassigned: bool,
    /// If true, reject re-claims even by the same actor.
    pub claim_exclusive: bool,
    /// The actor performing the claim (used for idempotent same-actor check).
    pub claim_actor: Option<String>,
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
            && self.source_repo.is_none()
            && self.source_repo_path.is_none()
            && self.agent_context.is_none()
            && self.closed_at.is_none()
            && self.close_reason.is_none()
            && self.closed_by_session.is_none()
            && self.deleted_at.is_none()
            && self.deleted_by.is_none()
            && self.delete_reason.is_none()
            && !self.expect_unassigned
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
    /// The status group treated as "ready" (issue #354). Each entry is a
    /// canonical-or-custom status string (already lowercased by the caller).
    /// Empty means "use the default `[open]` group", which preserves pre-#354
    /// behavior exactly. The CLI layer resolves this from
    /// `workflow.status_groups.ready` in `.beads/policy.yaml`.
    pub ready_statuses: Vec<String>,
    pub limit: Option<usize>,
    /// Filter to children of this parent issue ID.
    pub parent: Option<String>,
    /// Include all descendants (grandchildren, etc.) not just direct children.
    pub recursive: bool,
    /// Pre-resolved parent membership IDs.
    ///
    /// When `parent` is set, the query layer resolves the matching issue IDs in
    /// Rust (direct children, or all transitive descendants when `recursive`)
    /// and stores them here before building the SQL. The candidate query then
    /// filters with a plain `id IN (...)` list instead of a correlated
    /// subquery / recursive CTE. This sidesteps engine limitations in the
    /// embedded SQLite backend with `IN (subquery)` under multi-table joins
    /// (#307) and recursive CTEs referenced from a correlated `EXISTS` (#308),
    /// and guarantees bounded traversal via a visited-set BFS even when the
    /// parent-child graph contains cycles.
    ///
    /// `Some(empty)` means "parent is set but has no matching descendants" → the
    /// candidate query must return no rows. `None` means no parent filter.
    pub parent_member_ids: Option<Vec<String>>,
}

/// Minimal metadata needed for fast collision detection during sync.
#[derive(Debug, Clone)]
pub struct IssueMetadata {
    pub id: String,
    pub external_ref: Option<String>,
    pub content_hash: Option<String>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub status: crate::model::Status,
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

fn sort_ready_hybrid(issues: &mut [Issue]) {
    issues.sort_unstable_by(|left, right| {
        ready_hybrid_bucket(left.priority)
            .cmp(&ready_hybrid_bucket(right.priority))
            .then_with(|| left.created_at.cmp(&right.created_at))
            .then_with(|| left.id.cmp(&right.id))
    });
}

fn should_sort_list_default_in_rust(filters: &ListFilters) -> bool {
    filters.sort.is_none()
        && !filters.reverse
        && filters.offset.is_none_or(|offset| offset == 0)
        && filters.limit.is_none_or(|limit| limit == 0)
}

fn default_visible_limited_page_limit(filters: &ListFilters) -> Option<usize> {
    let limit = filters.limit?;
    if limit == 0 || filters.offset.is_some_and(|offset| offset > 0) {
        return None;
    }

    let is_default_visible = filters.statuses.as_ref().is_none_or(Vec::is_empty)
        && filters.types.as_ref().is_none_or(Vec::is_empty)
        && filters.priorities.as_ref().is_none_or(Vec::is_empty)
        && filters.assignee.is_none()
        && !filters.unassigned
        && !filters.include_closed
        && !filters.include_templates
        && filters.title_contains.is_none()
        && filters.sort.is_none()
        && !filters.reverse
        && filters.labels.as_ref().is_none_or(Vec::is_empty)
        && filters.labels_or.as_ref().is_none_or(Vec::is_empty)
        && filters.updated_before.is_none()
        && filters.updated_after.is_none();

    is_default_visible.then_some(limit)
}

fn default_visible_single_label_count_filter(filters: &ListFilters) -> Option<&str> {
    let labels = filters.labels.as_deref()?;
    let unique_labels = unique_label_refs(labels);
    let [label] = unique_labels.as_slice() else {
        return None;
    };

    let is_default_visible = filters.statuses.as_ref().is_none_or(Vec::is_empty)
        && filters.types.as_ref().is_none_or(Vec::is_empty)
        && filters.priorities.as_ref().is_none_or(Vec::is_empty)
        && filters.assignee.is_none()
        && !filters.unassigned
        && !filters.include_closed
        && !filters.include_templates
        && filters.title_contains.is_none()
        && filters.labels_or.as_ref().is_none_or(Vec::is_empty)
        && filters.updated_before.is_none()
        && filters.updated_after.is_none();

    is_default_visible.then_some(label.as_str())
}

fn sort_list_default(issues: &mut [Issue]) {
    issues.sort_unstable_by(|left, right| {
        left.priority
            .cmp(&right.priority)
            .then_with(|| right.created_at.cmp(&left.created_at))
            .then_with(|| left.id.cmp(&right.id))
    });
}

const fn ready_hybrid_bucket(priority: Priority) -> i32 {
    if priority.0 <= 1 { 0 } else { 1 }
}

fn ready_hybrid_high_bucket_priorities(priorities: Option<&[Priority]>) -> Vec<Priority> {
    priorities.map_or_else(
        || vec![Priority::CRITICAL, Priority::HIGH],
        |values| {
            values
                .iter()
                .copied()
                .filter(|priority| ready_hybrid_bucket(*priority) == 0)
                .collect()
        },
    )
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

fn dependency_metadata_from_row(
    row: &fsqlite::Row,
    row_role: &str,
    allow_external_placeholder: bool,
) -> Result<IssueWithDependencyMetadata> {
    let id = row
        .get(0)
        .and_then(SqliteValue::as_text)
        .ok_or_else(|| BeadsError::Config(format!("{row_role} row missing id")))?;
    let dep_type = row
        .get(4)
        .and_then(SqliteValue::as_text)
        .ok_or_else(|| {
            BeadsError::Config(format!("{row_role} row missing dependency type for {id}"))
        })?
        .to_string();

    let title = row.get(1).and_then(SqliteValue::as_text);
    let status = row.get(2).and_then(SqliteValue::as_text);
    let priority = row.get(3).and_then(SqliteValue::as_integer);

    let (title, status, priority) = match (title, status, priority) {
        (Some(title), Some(status), Some(priority)) => (title, status, priority),
        _ if allow_external_placeholder && id.starts_with("external:") => {
            return Ok(IssueWithDependencyMetadata {
                id: id.to_string(),
                title: id.strip_prefix("external:").unwrap_or(id).to_string(),
                status: Status::Blocked,
                priority: Priority::MEDIUM,
                dep_type,
            });
        }
        _ => {
            // Graceful fallback for missing dependencies (e.g. deleted/not synced yet)
            // instead of crashing the query with a Config error.
            return Ok(IssueWithDependencyMetadata {
                id: id.to_string(),
                title: format!("[missing issue: {}]", id),
                status: Status::Tombstone,
                priority: Priority::MEDIUM,
                dep_type,
            });
        }
    };

    let priority = i32::try_from(priority).map_err(|_| {
        BeadsError::Config(format!("{row_role} row priority out of range for {id}"))
    })?;

    Ok(IssueWithDependencyMetadata {
        id: id.to_string(),
        title: title.to_string(),
        status: parse_status(Some(status)),
        priority: Priority(priority),
        dep_type,
    })
}

fn parse_blocked_by_json(issue_id: &str, blockers_json: Option<&str>) -> Result<Vec<String>> {
    let blockers_json = blockers_json.ok_or_else(|| {
        BeadsError::Config(format!(
            "blocked_issues_cache missing blocked_by payload for {issue_id}"
        ))
    })?;

    serde_json::from_str(blockers_json).map_err(|err| {
        BeadsError::Config(format!("Malformed blocked_by JSON for {issue_id}: {err}"))
    })
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

fn cycle_endpoint(value: Option<&SqliteValue>) -> String {
    value
        .and_then(SqliteValue::as_text)
        .unwrap_or("")
        .to_string()
}

fn component_is_closed_only(component: &[String], statuses: &BTreeMap<String, Status>) -> bool {
    !component.is_empty()
        && component
            .iter()
            .all(|id| statuses.get(id).is_some_and(Status::is_terminal))
}

fn reverse_cycle_graph(graph: &BTreeMap<String, Vec<String>>) -> BTreeMap<String, Vec<String>> {
    let mut reverse_graph: BTreeMap<String, Vec<String>> = graph
        .keys()
        .map(|node| (node.clone(), Vec::new()))
        .collect();

    for (from, neighbors) in graph {
        for to in neighbors {
            reverse_graph
                .entry(to.clone())
                .or_default()
                .push(from.clone());
        }
    }

    for neighbors in reverse_graph.values_mut() {
        neighbors.sort();
        neighbors.dedup();
    }

    reverse_graph
}

fn find_cycle_graph_path(
    graph: &BTreeMap<String, Vec<String>>,
    start: &str,
    target: &str,
    component: &HashSet<&str>,
) -> Option<Vec<String>> {
    let mut visited = HashSet::new();
    let mut stack = vec![(start.to_string(), vec![start.to_string()])];

    while let Some((node, path)) = stack.pop() {
        if node == target {
            return Some(path);
        }
        if !visited.insert(node.clone()) {
            continue;
        }
        if let Some(neighbors) = graph.get(&node) {
            for neighbor in neighbors.iter().rev() {
                if component.contains(neighbor.as_str()) && !visited.contains(neighbor) {
                    let mut next_path = path.clone();
                    next_path.push(neighbor.clone());
                    stack.push((neighbor.clone(), next_path));
                }
            }
        }
    }

    None
}

fn query_external_project_capabilities(
    db_path: &Path,
    capabilities: &HashSet<String>,
) -> Result<HashSet<String>> {
    if capabilities.is_empty() {
        return Ok(HashSet::new());
    }

    let conn = open_existing_read_only_connection(db_path)?;
    let labels: Vec<String> = capabilities
        .iter()
        .map(|cap| format!("provides:{cap}"))
        .collect();

    let mut satisfied = HashSet::new();

    for chunk in labels.chunks(SQLITE_VAR_LIMIT) {
        let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
        let label_sql = format!(
            "SELECT label, issue_id
             FROM labels
             WHERE label IN ({})",
            placeholders.join(",")
        );
        let label_params: Vec<SqliteValue> = chunk
            .iter()
            .map(|label| SqliteValue::from(label.as_str()))
            .collect();
        let rows = conn.query_with_params(&label_sql, &label_params)?;

        let mut issue_ids_by_capability: HashMap<String, HashSet<String>> = HashMap::new();
        for row in &rows {
            let Some(label) = row.get(0).and_then(SqliteValue::as_text) else {
                continue;
            };
            let Some(issue_id) = row.get(1).and_then(SqliteValue::as_text) else {
                continue;
            };
            let Some(capability) = label.strip_prefix("provides:") else {
                continue;
            };
            issue_ids_by_capability
                .entry(capability.to_string())
                .or_default()
                .insert(issue_id.to_string());
        }

        if issue_ids_by_capability.is_empty() {
            continue;
        }

        let candidate_issue_ids: Vec<String> = issue_ids_by_capability
            .values()
            .flat_map(|issue_ids| issue_ids.iter().cloned())
            .collect();
        let mut closed_issue_ids = HashSet::new();

        for issue_chunk in candidate_issue_ids.chunks(SQLITE_VAR_LIMIT) {
            let issue_placeholders: Vec<&str> = issue_chunk.iter().map(|_| "?").collect();
            let issue_sql = format!(
                "SELECT id
                 FROM issues
                 WHERE status = 'closed' AND id IN ({})",
                issue_placeholders.join(",")
            );
            let issue_params: Vec<SqliteValue> = issue_chunk
                .iter()
                .map(|issue_id| SqliteValue::from(issue_id.as_str()))
                .collect();
            let issue_rows = conn.query_with_params(&issue_sql, &issue_params)?;

            for row in &issue_rows {
                if let Some(issue_id) = row.get(0).and_then(SqliteValue::as_text) {
                    closed_issue_ids.insert(issue_id.to_string());
                }
            }
        }

        for (capability, issue_ids) in issue_ids_by_capability {
            if issue_ids
                .iter()
                .any(|issue_id| closed_issue_ids.contains(issue_id))
            {
                satisfied.insert(capability);
            }
        }
    }

    // Explicitly close the connection to avoid fsqlite drop_close warnings.
    let _ = conn.close();
    Ok(satisfied)
}

fn open_existing_read_only_connection(path: &Path) -> Result<Connection> {
    if !path.is_file() {
        return Err(BeadsError::Config(format!(
            "external project database not found: {}",
            path.display()
        )));
    }

    open_with_flags(
        path.to_string_lossy().as_ref(),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(Into::into)
}

fn parse_datetime(s: &str) -> Result<DateTime<Utc>> {
    if s.is_empty() {
        // NULL/empty datetime columns (common when migrating from bd/Go beads)
        // default to epoch rather than crashing the import.
        return Ok(DateTime::<Utc>::UNIX_EPOCH);
    }

    if let Some(dt) = parse_canonical_utc_datetime(s) {
        return Ok(dt);
    }

    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&Utc));
    }

    if let Ok(naive) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S") {
        return Ok(Utc.from_utc_datetime(&naive));
    }

    Err(BeadsError::Config(format!("unparseable datetime: {s}")))
}

fn parse_canonical_utc_datetime(s: &str) -> Option<DateTime<Utc>> {
    let bytes = s.as_bytes();
    if bytes.len() < 20
        || bytes.get(4) != Some(&b'-')
        || bytes.get(7) != Some(&b'-')
        || !matches!(bytes.get(10), Some(b'T' | b't' | b' '))
        || bytes.get(13) != Some(&b':')
        || bytes.get(16) != Some(&b':')
    {
        return None;
    }

    let year = i32::try_from(parse_fixed_digits(bytes, 0, 4)?).ok()?;
    let month = parse_fixed_digits(bytes, 5, 2)?;
    let day = parse_fixed_digits(bytes, 8, 2)?;
    let hour = parse_fixed_digits(bytes, 11, 2)?;
    let minute = parse_fixed_digits(bytes, 14, 2)?;
    let second = parse_fixed_digits(bytes, 17, 2)?;

    let mut index = 19;
    let nanos = if bytes.get(index) == Some(&b'.') {
        index += 1;
        let mut nanos = 0_u32;
        let mut digits = 0_u32;
        while let Some(&byte) = bytes.get(index) {
            let Some(digit) = decimal_digit(byte) else {
                break;
            };
            if digits == 9 {
                return None;
            }
            nanos = nanos.saturating_mul(10).saturating_add(digit);
            digits += 1;
            index += 1;
        }
        if digits == 0 {
            return None;
        }
        for _ in digits..9 {
            nanos = nanos.saturating_mul(10);
        }
        nanos
    } else {
        0
    };

    match bytes.get(index..) {
        Some(b"Z" | b"z" | b"+00:00") => {}
        _ => return None,
    }

    let date = NaiveDate::from_ymd_opt(year, month, day)?;
    let time = NaiveTime::from_hms_nano_opt(hour, minute, second, nanos)?;
    Some(Utc.from_utc_datetime(&NaiveDateTime::new(date, time)))
}

fn parse_fixed_digits(bytes: &[u8], start: usize, len: usize) -> Option<u32> {
    let mut value = 0_u32;
    for &byte in bytes.get(start..start.checked_add(len)?)? {
        value = value
            .saturating_mul(10)
            .saturating_add(decimal_digit(byte)?);
    }
    Some(value)
}

fn decimal_digit(byte: u8) -> Option<u32> {
    byte.is_ascii_digit()
        .then_some(u32::from(byte.saturating_sub(b'0')))
}

/// Decode a DATETIME column that may be stored as TEXT (canonical RFC 3339),
/// INTEGER (epoch seconds/ms/µs/ns), REAL (fractional epoch seconds), or NULL.
///
/// SQLite's DATETIME is an advisory type — any storage class is accepted. In
/// practice external tools and older migration paths have written integer
/// epoch microseconds into datetime columns; the legacy reader called
/// `as_text().unwrap_or("")` and then `parse_datetime` silently mapped the
/// empty string to `UNIX_EPOCH`, corrupting the value on export. This helper
/// preserves the data by coercing numeric storage classes into a real
/// `DateTime<Utc>`.
fn parse_datetime_value(value: Option<&SqliteValue>) -> Result<DateTime<Utc>> {
    match value {
        None | Some(SqliteValue::Null) => Ok(DateTime::<Utc>::UNIX_EPOCH),
        Some(SqliteValue::Text(s)) => parse_datetime(s.as_ref()),
        Some(SqliteValue::Integer(n)) => datetime_from_epoch_auto(*n),
        Some(SqliteValue::Float(f)) => datetime_from_epoch_seconds_f64(*f),
        Some(SqliteValue::Blob(_)) => Err(BeadsError::Config(
            "unexpected BLOB storage class for datetime column".to_string(),
        )),
    }
}

/// Like [`parse_datetime_value`] but returns `None` for `NULL` / missing
/// columns instead of `UNIX_EPOCH`. Empty TEXT is treated as `None`.
fn parse_opt_datetime_value(value: Option<&SqliteValue>) -> Result<Option<DateTime<Utc>>> {
    match value {
        None | Some(SqliteValue::Null) => Ok(None),
        Some(SqliteValue::Text(s)) if s.is_empty() => Ok(None),
        Some(SqliteValue::Text(s)) => parse_datetime(s.as_ref()).map(Some),
        Some(SqliteValue::Integer(n)) => datetime_from_epoch_auto(*n).map(Some),
        Some(SqliteValue::Float(f)) => datetime_from_epoch_seconds_f64(*f).map(Some),
        Some(SqliteValue::Blob(_)) => Err(BeadsError::Config(
            "unexpected BLOB storage class for datetime column".to_string(),
        )),
    }
}

/// Convert a numeric epoch stored as `i64` into a UTC datetime by auto-
/// detecting the unit from magnitude. We assume any realistic beads timestamp
/// is within a century of the Unix epoch, which gives non-overlapping ranges
/// for seconds (≤10^10), milliseconds (≤10^13), microseconds (≤10^16), and
/// nanoseconds (≤10^19).
fn datetime_from_epoch_auto(n: i64) -> Result<DateTime<Utc>> {
    const MS_THRESHOLD: i64 = 10_000_000_000; // ~Nov 2286 as seconds
    const US_THRESHOLD: i64 = 10_000_000_000_000;
    const NS_THRESHOLD: i64 = 10_000_000_000_000_000;

    let abs = n.unsigned_abs();
    let (secs, sub_nanos): (i64, u32) = if abs <= MS_THRESHOLD as u64 {
        (n, 0)
    } else if abs <= US_THRESHOLD as u64 {
        let secs = n.div_euclid(1_000);
        let rem = epoch_remainder_u32(n, 1_000)?;
        (secs, rem * 1_000_000)
    } else if abs <= NS_THRESHOLD as u64 {
        let secs = n.div_euclid(1_000_000);
        let rem = epoch_remainder_u32(n, 1_000_000)?;
        (secs, rem * 1_000)
    } else {
        let secs = n.div_euclid(1_000_000_000);
        let rem = epoch_remainder_u32(n, 1_000_000_000)?;
        (secs, rem)
    };

    DateTime::<Utc>::from_timestamp(secs, sub_nanos)
        .ok_or_else(|| BeadsError::Config(format!("invalid epoch value in datetime column: {n}")))
}

fn epoch_remainder_u32(n: i64, divisor: i64) -> Result<u32> {
    u32::try_from(n.rem_euclid(divisor))
        .map_err(|_| BeadsError::Config(format!("invalid epoch value in datetime column: {n}")))
}

fn datetime_from_epoch_seconds_f64(f: f64) -> Result<DateTime<Utc>> {
    const MIN_I64_AS_F64: f64 = -9_223_372_036_854_775_808.0;
    const MAX_I64_AS_F64: f64 = 9_223_372_036_854_775_807.0;

    if !f.is_finite() {
        return Err(BeadsError::Config(format!(
            "non-finite datetime column value: {f}"
        )));
    }
    // Use floor/fract so the (secs, nanos) split is correct for negative
    // timestamps too — with trunc() and abs() on the fraction, `-1.5`
    // resolves to `(-1, 500_000_000)` = `-0.5` rather than the intended
    // `(-2, 500_000_000)` = `-1.5`. The `fract()` here is f − floor(f),
    // which is always in [0, 1) regardless of sign.
    let floor_seconds = f.floor();
    if !(MIN_I64_AS_F64..=MAX_I64_AS_F64).contains(&floor_seconds) {
        return Err(BeadsError::Config(format!(
            "invalid epoch value in datetime column: {f}"
        )));
    }

    #[allow(clippy::cast_possible_truncation)]
    let mut secs = floor_seconds as i64;
    let nanos_f64 = ((f - floor_seconds) * 1_000_000_000.0).round();
    let nanos_f64 = if nanos_f64 >= 1_000_000_000.0 {
        secs = secs.checked_add(1).ok_or_else(|| {
            BeadsError::Config(format!("invalid epoch value in datetime column: {f}"))
        })?;
        0.0
    } else {
        nanos_f64.clamp(0.0, 999_999_999.0)
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let nanos = nanos_f64 as u32;
    DateTime::<Utc>::from_timestamp(secs, nanos)
        .ok_or_else(|| BeadsError::Config(format!("invalid epoch value in datetime column: {f}")))
}

/// Escape special LIKE pattern characters (%, _, \) for literal matching.
///
/// Use with `LIKE ? ESCAPE '\\'` in SQL queries.
fn escape_like_pattern(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
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

    /// Get multiple issues with all relations populated for export.
    ///
    /// Includes labels, dependencies, and comments. This fetches in batch to avoid N+1 queries.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_issues_for_export(&self, ids: &[String]) -> Result<Vec<Issue>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }

        let mut issues = self.get_issues_by_ids(ids)?;

        // Fetch relations in batch
        let labels_map = self.get_labels_for_issues(ids)?;
        let deps_map = self.get_dependencies_full_for_issues(ids)?;
        let comments_map = self.get_comments_for_issues(ids)?;

        for issue in &mut issues {
            if let Some(labels) = labels_map.get(&issue.id) {
                issue.labels = labels.clone();
                issue.labels.sort();
                issue.labels.dedup();
            }
            if let Some(deps) = deps_map.get(&issue.id) {
                issue.dependencies = deps.clone();
            }
            if let Some(comments) = comments_map.get(&issue.id) {
                issue.comments = comments.clone();
            }
        }

        Ok(issues)
    }

    /// Get dependencies as full Dependency structs for export.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_dependencies_full(&self, issue_id: &str) -> Result<Vec<crate::model::Dependency>> {
        let stmt = self.conn.prepare(
            "SELECT issue_id, depends_on_id, type, created_at, created_by, metadata, thread_id
             FROM dependencies
             WHERE issue_id = ?
             ORDER BY depends_on_id",
        )?;

        let rows = stmt.query_with_params(&[SqliteValue::from(issue_id)])?;

        let mut deps = Vec::with_capacity(rows.len());
        for row in &rows {
            deps.push(crate::model::Dependency {
                issue_id: row
                    .get(0)
                    .and_then(SqliteValue::as_text)
                    .unwrap_or("")
                    .to_string(),
                depends_on_id: row
                    .get(1)
                    .and_then(SqliteValue::as_text)
                    .unwrap_or("")
                    .to_string(),
                dep_type: row
                    .get(2)
                    .and_then(SqliteValue::as_text)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(crate::model::DependencyType::Blocks),
                created_at: parse_datetime_value(row.get(3))?,
                created_by: row
                    .get(4)
                    .and_then(SqliteValue::as_text)
                    .map(str::to_string),
                metadata: row
                    .get(5)
                    .and_then(SqliteValue::as_text)
                    .map(str::to_string),
                thread_id: row
                    .get(6)
                    .and_then(SqliteValue::as_text)
                    .map(str::to_string),
            });
        }

        Ok(deps)
    }

    /// Get dependencies as full Dependency structs for multiple issues in batch.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn get_dependencies_full_for_issues(
        &self,
        issue_ids: &[String],
    ) -> Result<std::collections::HashMap<String, Vec<crate::model::Dependency>>> {
        const SQLITE_VAR_LIMIT: usize = 900;
        let mut map: std::collections::HashMap<String, Vec<crate::model::Dependency>> =
            std::collections::HashMap::new();

        if issue_ids.is_empty() {
            return Ok(map);
        }

        for chunk in issue_ids.chunks(SQLITE_VAR_LIMIT) {
            let placeholders = vec!["?"; chunk.len()].join(", ");
            let sql = format!(
                "SELECT issue_id, depends_on_id, type, created_at, created_by, metadata, thread_id
                 FROM dependencies
                 WHERE issue_id IN ({})
                 ORDER BY depends_on_id",
                placeholders
            );

            let params: Vec<SqliteValue> = chunk
                .iter()
                .map(|id| SqliteValue::from(id.as_str()))
                .collect();

            let rows = self.conn.query_with_params(&sql, &params)?;

            for row in &rows {
                let dep = crate::model::Dependency {
                    issue_id: row
                        .get(0)
                        .and_then(SqliteValue::as_text)
                        .unwrap_or("")
                        .to_string(),
                    depends_on_id: row
                        .get(1)
                        .and_then(SqliteValue::as_text)
                        .unwrap_or("")
                        .to_string(),
                    dep_type: row
                        .get(2)
                        .and_then(SqliteValue::as_text)
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(crate::model::DependencyType::Blocks),
                    created_at: parse_datetime_value(row.get(3))?,
                    created_by: row
                        .get(4)
                        .and_then(SqliteValue::as_text)
                        .map(str::to_string),
                    metadata: row
                        .get(5)
                        .and_then(SqliteValue::as_text)
                        .map(str::to_string),
                    thread_id: row
                        .get(6)
                        .and_then(SqliteValue::as_text)
                        .map(str::to_string),
                };
                map.entry(dep.issue_id.clone()).or_default().push(dep);
            }
        }

        Ok(map)
    }

    /// Clear dirty flags for the given issue IDs.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub fn clear_dirty_flags(&mut self, ids: &[String]) -> Result<usize> {
        const SQLITE_VAR_LIMIT: usize = 900;
        if ids.is_empty() {
            return Ok(0);
        }

        let mut total_deleted = 0;
        for chunk in ids.chunks(SQLITE_VAR_LIMIT) {
            // Delete existing entries row-by-row to avoid fsqlite IN-clause bugs
            let mut chunk_deleted = 0;
            for id in chunk {
                let deleted = self.conn.execute_with_params(
                    "DELETE FROM dirty_issues WHERE issue_id = ?",
                    &[SqliteValue::from(id.as_str())],
                )?;
                chunk_deleted += deleted;
            }
            total_deleted += chunk_deleted;
        }

        Ok(total_deleted)
    }

    /// Clear all dirty flags.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub fn clear_all_dirty_flags(&mut self) -> Result<usize> {
        let deleted = self.conn.execute("DELETE FROM dirty_issues")?;
        Ok(deleted)
    }

    /// Get the count of issues (for safety guard).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn count_exportable_issues(&self) -> Result<usize> {
        let count = self
            .conn
            .query_row(
                "SELECT COUNT(*) FROM issues WHERE ephemeral = 0 AND id NOT LIKE '%-wisp-%'",
            )?
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap_or(0);
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
        let count = self
            .conn
            .query_row_with_params(
                "SELECT COUNT(*) FROM dependencies WHERE issue_id = ? AND depends_on_id = ?",
                &[
                    SqliteValue::from(issue_id),
                    SqliteValue::from(depends_on_id),
                ],
            )?
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap_or(0);
        Ok(count > 0)
    }

    /// Check if adding a standard dependency edge would create a cycle.
    ///
    /// If `blocking_only` is true, only considers dependency types that affect ready-work
    /// blocking: `blocks`, `conditional-blocks`, `waits-for`, and reversed `parent-child`.
    /// Use [`Self::would_create_parent_child_cycle`] when validating a stored
    /// `parent-child` row, because those rows are reversed in the blocker graph.
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

    /// Check if adding a `parent-child` row would create a cycle.
    ///
    /// Stored parent rows are `child -> parent`, while dependency graph cycle
    /// detection represents them as `parent -> child`.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn would_create_parent_child_cycle(
        &self,
        child_id: &str,
        parent_id: &str,
        blocking_only: bool,
    ) -> Result<bool> {
        Self::check_parent_child_cycle(&self.conn, child_id, parent_id, blocking_only)
    }

    /// Detect all cycles in the dependency graph.
    ///
    /// Returns deterministic cycle witnesses, where each cycle is a vector of
    /// issue IDs ending with its starting ID. The implementation finds strongly
    /// connected components first, then emits one witness per cyclic component.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn detect_all_cycles(&self) -> Result<Vec<Vec<String>>> {
        self.detect_cycles(false)
    }

    /// Detect cycles in dependency types that affect ready-work blocking.
    ///
    /// This uses the same edge semantics as `would_create_cycle(..., true)`:
    /// `blocks`, `conditional-blocks`, and `waits-for` point from dependent to blocker;
    /// `parent-child` edges are reversed so parent/child hierarchy cycles are still reported.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn detect_blocking_cycles(&self) -> Result<Vec<Vec<String>>> {
        self.detect_cycles(true)
    }

    fn detect_cycles(&self, blocking_only: bool) -> Result<Vec<Vec<String>>> {
        let graph = self.load_dependency_cycle_graph(blocking_only)?;
        Ok(Self::cycle_witnesses_from_graph(&graph))
    }

    /// Detect dependency cycles split into active and closed-only archive buckets.
    ///
    /// Active cycles include any component with at least one non-terminal issue
    /// or a dependency endpoint missing from the local issue table.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn detect_dependency_cycle_report(
        &self,
        blocking_only: bool,
    ) -> Result<DependencyCycleReport> {
        let graph = self.load_dependency_cycle_graph(blocking_only)?;
        let statuses = self.load_dependency_cycle_issue_statuses()?;
        let witnesses = Self::cycle_witnesses_with_components_from_graph(&graph);
        let mut active_cycles = Vec::new();
        let mut archived_closed_cycles = Vec::new();

        for (component, cycle) in witnesses {
            if component_is_closed_only(&component, &statuses) {
                archived_closed_cycles.push(cycle);
            } else {
                active_cycles.push(cycle);
            }
        }

        active_cycles.sort();
        archived_closed_cycles.sort();
        Ok(DependencyCycleReport {
            active_cycles,
            archived_closed_cycles,
        })
    }

    fn load_dependency_cycle_graph(
        &self,
        blocking_only: bool,
    ) -> Result<BTreeMap<String, Vec<String>>> {
        Self::load_dependency_cycle_graph_from_conn(&self.conn, blocking_only)
    }

    fn load_dependency_cycle_graph_from_conn(
        conn: &Connection,
        blocking_only: bool,
    ) -> Result<BTreeMap<String, Vec<String>>> {
        let mut graph: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let standard_edge_sql = if blocking_only {
            "SELECT issue_id, depends_on_id FROM dependencies \
             WHERE type IN ('blocks', 'conditional-blocks', 'waits-for')"
        } else {
            "SELECT issue_id, depends_on_id FROM dependencies WHERE type != 'parent-child'"
        };
        let rows1 = conn.query(standard_edge_sql)?;
        for row in &rows1 {
            let from = cycle_endpoint(row.get(0));
            let to = cycle_endpoint(row.get(1));
            graph.entry(to.clone()).or_default();
            graph.entry(from).or_default().push(to);
        }

        let rows2 = conn.query(
            "SELECT depends_on_id, issue_id FROM dependencies WHERE type = 'parent-child'",
        )?;
        for row in &rows2 {
            let from = cycle_endpoint(row.get(0));
            let to = cycle_endpoint(row.get(1));
            graph.entry(to.clone()).or_default();
            graph.entry(from).or_default().push(to);
        }

        for neighbors in graph.values_mut() {
            neighbors.sort();
            neighbors.dedup();
        }

        Ok(graph)
    }

    fn load_dependency_cycle_issue_statuses(&self) -> Result<BTreeMap<String, Status>> {
        let rows = self.conn.query("SELECT id, status FROM issues")?;
        let mut statuses = BTreeMap::new();
        for row in &rows {
            let Some(id) = row.get(0).and_then(SqliteValue::as_text) else {
                continue;
            };
            let status = parse_status(row.get(1).and_then(SqliteValue::as_text));
            statuses.insert(id.to_string(), status);
        }
        Ok(statuses)
    }

    fn cycle_witnesses_from_graph(graph: &BTreeMap<String, Vec<String>>) -> Vec<Vec<String>> {
        Self::cycle_witnesses_with_components_from_graph(graph)
            .into_iter()
            .map(|(_component, cycle)| cycle)
            .collect()
    }

    fn cycle_witnesses_with_components_from_graph(
        graph: &BTreeMap<String, Vec<String>>,
    ) -> Vec<(Vec<String>, Vec<String>)> {
        let components = Self::strongly_connected_components(graph);
        let mut cycles = Vec::new();

        for component in components {
            if component.len() == 1 {
                let node = component[0].clone();
                if graph
                    .get(&node)
                    .is_some_and(|neighbors| neighbors.binary_search(&node).is_ok())
                {
                    cycles.push((component, vec![node.clone(), node]));
                }
                continue;
            }

            if let Some(cycle) = Self::cycle_witness_for_component(graph, &component) {
                cycles.push((component, cycle));
            }
        }

        cycles.sort_by(|left, right| left.1.cmp(&right.1));
        cycles
    }

    fn strongly_connected_components(graph: &BTreeMap<String, Vec<String>>) -> Vec<Vec<String>> {
        let mut visited = HashSet::new();
        let mut finish_order = Vec::with_capacity(graph.len());

        for node in graph.keys() {
            if visited.contains(node) {
                continue;
            }
            Self::push_cycle_graph_finish_order(graph, node, &mut visited, &mut finish_order);
        }

        let reverse_graph = reverse_cycle_graph(graph);
        let mut assigned = HashSet::new();
        let mut components = Vec::new();

        for node in finish_order.iter().rev() {
            if assigned.contains(node) {
                continue;
            }

            let mut component = Vec::new();
            let mut stack = vec![node.clone()];
            while let Some(current) = stack.pop() {
                if !assigned.insert(current.clone()) {
                    continue;
                }
                component.push(current.clone());
                if let Some(neighbors) = reverse_graph.get(&current) {
                    for neighbor in neighbors.iter().rev() {
                        if !assigned.contains(neighbor) {
                            stack.push(neighbor.clone());
                        }
                    }
                }
            }
            component.sort();
            components.push(component);
        }

        components.sort();
        components
    }

    fn push_cycle_graph_finish_order(
        graph: &BTreeMap<String, Vec<String>>,
        start: &str,
        visited: &mut HashSet<String>,
        finish_order: &mut Vec<String>,
    ) {
        let mut stack = vec![(start.to_string(), false)];
        while let Some((node, expanded)) = stack.pop() {
            if expanded {
                finish_order.push(node);
                continue;
            }
            if !visited.insert(node.clone()) {
                continue;
            }

            stack.push((node.clone(), true));
            if let Some(neighbors) = graph.get(&node) {
                for neighbor in neighbors.iter().rev() {
                    if !visited.contains(neighbor) {
                        stack.push((neighbor.clone(), false));
                    }
                }
            }
        }
    }

    fn cycle_witness_for_component(
        graph: &BTreeMap<String, Vec<String>>,
        component: &[String],
    ) -> Option<Vec<String>> {
        let start = component.first()?;
        let component_set: HashSet<_> = component.iter().map(String::as_str).collect();
        for neighbor in graph.get(start)?.iter().filter(|neighbor| {
            neighbor.as_str() != start.as_str() && component_set.contains(neighbor.as_str())
        }) {
            if let Some(mut path) = find_cycle_graph_path(graph, neighbor, start, &component_set) {
                let mut cycle = vec![start.clone()];
                cycle.append(&mut path);
                return Some(cycle);
            }
        }
        None
    }

    // ===== Import Helper Methods =====

    /// Find an issue by external reference.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn find_by_external_ref(&self, external_ref: &str) -> Result<Option<Issue>> {
        match self.conn.query_row_with_params(
            r"SELECT id, content_hash, title, description, design, acceptance_criteria, notes,
                     status, priority, issue_type, assignee, owner, estimated_minutes,
                     created_at, created_by, updated_at, closed_at, close_reason, closed_by_session,
                     due_at, defer_until, external_ref, source_system, source_repo,
                     deleted_at, deleted_by, delete_reason, original_type, compaction_level,
                     compacted_at, compacted_at_commit, original_size, sender, ephemeral,
                     pinned, is_template, source_repo_path, agent_context
               FROM issues WHERE external_ref = ?",
            &[SqliteValue::from(external_ref)],
        ) {
            Ok(row) => Ok(Some(Self::issue_from_row(&row)?)),
            Err(FrankenError::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Find an issue by content hash.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn find_by_content_hash(&self, content_hash: &str) -> Result<Option<Issue>> {
        match self.conn.query_row_with_params(
            r"SELECT id, content_hash, title, description, design, acceptance_criteria, notes,
                     status, priority, issue_type, assignee, owner, estimated_minutes,
                     created_at, created_by, updated_at, closed_at, close_reason, closed_by_session,
                     due_at, defer_until, external_ref, source_system, source_repo,
                     deleted_at, deleted_by, delete_reason, original_type, compaction_level,
                     compacted_at, compacted_at_commit, original_size, sender, ephemeral,
                     pinned, is_template, source_repo_path, agent_context
               FROM issues WHERE content_hash = ?",
            &[SqliteValue::from(content_hash)],
        ) {
            Ok(row) => Ok(Some(Self::issue_from_row(&row)?)),
            Err(FrankenError::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Check if an issue is a tombstone (deleted).
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub fn is_tombstone(&self, id: &str) -> Result<bool> {
        Ok(matches!(
            Self::get_issue_from_conn(&self.conn, id)?.map(|issue| issue.status),
            Some(Status::Tombstone)
        ))
    }

    fn import_issue_field_values(
        issue: &Issue,
        timestamps: &ImportIssueTimestampStrings,
    ) -> Vec<SqliteValue> {
        let status_str = issue.status.as_str();
        let issue_type_str = issue.issue_type.as_str();

        vec![
            issue
                .content_hash
                .as_deref()
                .map_or(SqliteValue::Null, SqliteValue::from),
            SqliteValue::from(issue.title.as_str()),
            SqliteValue::from(issue.description.as_deref().unwrap_or("")),
            SqliteValue::from(issue.design.as_deref().unwrap_or("")),
            SqliteValue::from(issue.acceptance_criteria.as_deref().unwrap_or("")),
            SqliteValue::from(issue.notes.as_deref().unwrap_or("")),
            SqliteValue::from(status_str),
            SqliteValue::from(i64::from(issue.priority.0)),
            SqliteValue::from(issue_type_str),
            issue
                .assignee
                .as_deref()
                .map_or(SqliteValue::Null, SqliteValue::from),
            SqliteValue::from(issue.owner.as_deref().unwrap_or("")),
            issue
                .estimated_minutes
                .map_or(SqliteValue::Null, |v| SqliteValue::from(i64::from(v))),
            SqliteValue::from(timestamps.created_at.as_str()),
            SqliteValue::from(issue.created_by.as_deref().unwrap_or("")),
            SqliteValue::from(timestamps.updated_at.as_str()),
            timestamps
                .closed_at
                .as_deref()
                .map_or(SqliteValue::Null, SqliteValue::from),
            SqliteValue::from(issue.close_reason.as_deref().unwrap_or("")),
            SqliteValue::from(issue.closed_by_session.as_deref().unwrap_or("")),
            timestamps
                .due_at
                .as_deref()
                .map_or(SqliteValue::Null, SqliteValue::from),
            timestamps
                .defer_until
                .as_deref()
                .map_or(SqliteValue::Null, SqliteValue::from),
            issue
                .external_ref
                .as_deref()
                .map_or(SqliteValue::Null, SqliteValue::from),
            SqliteValue::from(issue.source_system.as_deref().unwrap_or("")),
            SqliteValue::from(issue.source_repo.as_deref().unwrap_or(".")),
            issue
                .source_repo_path
                .as_deref()
                .map_or(SqliteValue::Null, SqliteValue::from),
            timestamps
                .deleted_at
                .as_deref()
                .map_or(SqliteValue::Null, SqliteValue::from),
            SqliteValue::from(issue.deleted_by.as_deref().unwrap_or("")),
            SqliteValue::from(issue.delete_reason.as_deref().unwrap_or("")),
            SqliteValue::from(issue.original_type.as_deref().unwrap_or("")),
            SqliteValue::from(i64::from(issue.compaction_level.unwrap_or(0))),
            timestamps
                .compacted_at
                .as_deref()
                .map_or(SqliteValue::Null, SqliteValue::from),
            issue
                .compacted_at_commit
                .as_deref()
                .map_or(SqliteValue::Null, SqliteValue::from),
            SqliteValue::from(i64::from(issue.original_size.unwrap_or(0))),
            SqliteValue::from(issue.sender.as_deref().unwrap_or("")),
            SqliteValue::from(i64::from(i32::from(issue.ephemeral))),
            SqliteValue::from(i64::from(i32::from(issue.pinned))),
            SqliteValue::from(i64::from(i32::from(issue.is_template))),
            issue
                .agent_context
                .as_deref()
                .map_or(SqliteValue::Null, SqliteValue::from),
        ]
    }

    fn insert_issue_row_for_import(
        &self,
        issue: &Issue,
        timestamps: &ImportIssueTimestampStrings,
    ) -> Result<usize> {
        let mut insert_params = Vec::with_capacity(38);
        insert_params.push(SqliteValue::from(issue.id.as_str()));
        insert_params.extend(Self::import_issue_field_values(issue, timestamps));

        let rows = self.conn.execute_with_params(
            r"INSERT INTO issues (
                id, content_hash, title, description, design, acceptance_criteria, notes,
                status, priority, issue_type, assignee, owner, estimated_minutes,
                created_at, created_by, updated_at, closed_at, close_reason, closed_by_session,
                due_at, defer_until, external_ref, source_system, source_repo, source_repo_path,
                deleted_at, deleted_by, delete_reason, original_type, compaction_level,
                compacted_at, compacted_at_commit, original_size, sender, ephemeral,
                pinned, is_template, agent_context
            ) VALUES (
                ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
            )",
            &insert_params,
        )?;

        Ok(rows)
    }

    fn update_issue_row_for_import(
        &self,
        issue: &Issue,
        timestamps: &ImportIssueTimestampStrings,
    ) -> Result<usize> {
        let mut params = Self::import_issue_field_values(issue, timestamps);
        params.push(SqliteValue::from(issue.id.as_str()));
        let rows = self.conn.execute_with_params(
            r"UPDATE issues SET
                content_hash = ?, title = ?, description = ?, design = ?,
                acceptance_criteria = ?, notes = ?, status = ?, priority = ?,
                issue_type = ?, assignee = ?, owner = ?, estimated_minutes = ?,
                created_at = ?, created_by = ?, updated_at = ?, closed_at = ?,
                close_reason = ?, closed_by_session = ?, due_at = ?, defer_until = ?,
                external_ref = ?, source_system = ?, source_repo = ?, source_repo_path = ?,
                deleted_at = ?, deleted_by = ?, delete_reason = ?, original_type = ?, compaction_level = ?,
                compacted_at = ?, compacted_at_commit = ?, original_size = ?, sender = ?,
                ephemeral = ?, pinned = ?, is_template = ?, agent_context = ?
              WHERE id = ?",
            &params,
        )?;

        Ok(rows)
    }

    /// Insert a new issue during JSONL import without first probing for existence.
    ///
    /// This does NOT trigger dirty tracking or events.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub(crate) fn insert_new_issue_for_import(&self, issue: &Issue) -> Result<bool> {
        let timestamps = ImportIssueTimestampStrings::from_issue(issue);
        Ok(self.insert_issue_row_for_import(issue, &timestamps)? > 0)
    }

    /// Upsert an issue (create or update) for import operations.
    ///
    /// For an existing row, runs an in-place UPDATE; for a new row, runs
    /// INSERT. The previous implementation used explicit DELETE + INSERT,
    /// which cascade-deleted child rows that reference `issues(id)` —
    /// events, labels, dependencies, comments — every time an import
    /// touched an existing issue (issue #263). The existence check is a
    /// narrow `SELECT 1` rather than parsing the row through
    /// `get_issue_from_conn`: a malformed-but-present row should be
    /// healable by overwriting it with valid JSONL, not blocked by the
    /// parser rejecting the bad data first.
    ///
    /// This does NOT trigger dirty tracking or events.
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub fn upsert_issue_for_import(&self, issue: &Issue) -> Result<bool> {
        let timestamps = ImportIssueTimestampStrings::from_issue(issue);

        // Narrow existence probe: don't deserialize the row, just check
        // if the id is present. If it's malformed we still want to
        // overwrite it.
        let issue_exists = match self.conn.query_row_with_params(
            "SELECT 1 FROM issues WHERE id = ? LIMIT 1",
            &[SqliteValue::from(issue.id.as_str())],
        ) {
            Ok(_) => true,
            Err(FrankenError::QueryReturnedNoRows) => false,
            Err(error) => return Err(error.into()),
        };

        if issue_exists {
            let rows = self.update_issue_row_for_import(issue, &timestamps)?;
            if rows == 0 {
                return Err(BeadsError::Database(FrankenError::Internal(format!(
                    "import update did not find existing issue {}",
                    issue.id
                ))));
            }
            return Ok(true);
        }

        Ok(self.insert_issue_row_for_import(issue, &timestamps)? > 0)
    }

    /// Check whether relation rows already exist for an imported issue ID.
    ///
    /// This is used to guard the insert-only relation fast path. A newly
    /// inserted issue row can still attach to stale relation rows that were
    /// already present with foreign keys disabled, so callers must verify this
    /// before skipping relation deletes.
    ///
    /// # Errors
    ///
    /// Returns an error if the database query fails.
    pub(crate) fn has_owned_relation_rows_for_import(&self, issue_id: &str) -> Result<bool> {
        let row = self.conn.query_row_with_params(
            "SELECT
                 EXISTS(SELECT 1 FROM labels WHERE issue_id = ?)
                 OR EXISTS(SELECT 1 FROM dependencies WHERE issue_id = ?)
                 OR EXISTS(SELECT 1 FROM comments WHERE issue_id = ?)",
            &[
                SqliteValue::from(issue_id),
                SqliteValue::from(issue_id),
                SqliteValue::from(issue_id),
            ],
        )?;

        Ok(row.get(0).and_then(SqliteValue::as_integer).unwrap_or(0) != 0)
    }

    /// Replace an issue's dirty marker inside the current write transaction.
    ///
    /// # Errors
    ///
    /// Returns an error if the marker cannot be updated.
    pub(crate) fn replace_dirty_issue_marker(&self, issue_id: &str, marked_at: &str) -> Result<()> {
        self.conn.execute_with_params(
            "DELETE FROM dirty_issues WHERE issue_id = ?",
            &[SqliteValue::from(issue_id)],
        )?;
        self.conn.execute_with_params(
            "INSERT INTO dirty_issues (issue_id, marked_at) VALUES (?, ?)",
            &[SqliteValue::from(issue_id), SqliteValue::from(marked_at)],
        )?;
        Ok(())
    }

    /// Sync labels for an issue (remove existing, add new).
    ///
    /// # Errors
    ///
    /// Returns an error if the label replacement is invalid or the database
    /// operation fails.
    pub fn sync_labels_for_import(&self, issue_id: &str, labels: &[String]) -> Result<()> {
        let unique_labels = unique_label_refs(labels);
        validate_storage_label_refs(&unique_labels)?;

        // Remove existing labels
        self.conn.execute_with_params(
            "DELETE FROM labels WHERE issue_id = ?",
            &[SqliteValue::from(issue_id)],
        )?;

        self.insert_label_refs_for_import(issue_id, &unique_labels)
    }

    fn insert_labels_for_import(&self, issue_id: &str, labels: &[String]) -> Result<()> {
        let unique_labels = unique_label_refs(labels);
        validate_storage_label_refs(&unique_labels)?;

        self.insert_label_refs_for_import(issue_id, &unique_labels)
    }

    fn insert_label_refs_for_import(
        &self,
        issue_id: &str,
        unique_labels: &[&String],
    ) -> Result<()> {
        if unique_labels.is_empty() {
            return Ok(());
        }

        // Keep label inserts single-row: fsqlite can mis-handle multi-values
        // inserts with repeated issue_id bindings on this primary key.
        for label in unique_labels {
            self.conn.execute_with_params(
                "INSERT OR IGNORE INTO labels (issue_id, label) VALUES (?, ?)",
                &[
                    SqliteValue::from(issue_id),
                    SqliteValue::from(label.as_str()),
                ],
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
        &self,
        issue_id: &str,
        dependencies: &[crate::model::Dependency],
    ) -> Result<()> {
        let unique_deps = Self::validated_unique_import_dependencies(issue_id, dependencies)?;

        // Remove existing dependencies where this issue is the dependent
        self.conn.execute_with_params(
            "DELETE FROM dependencies WHERE issue_id = ?",
            &[SqliteValue::from(issue_id)],
        )?;

        self.insert_dependency_refs_for_import(issue_id, &unique_deps)
    }

    fn insert_dependencies_for_import(
        &self,
        issue_id: &str,
        dependencies: &[crate::model::Dependency],
    ) -> Result<()> {
        let unique_deps = Self::validated_unique_import_dependencies(issue_id, dependencies)?;

        self.insert_dependency_refs_for_import(issue_id, &unique_deps)
    }

    fn validated_unique_import_dependencies<'a>(
        issue_id: &str,
        dependencies: &'a [Dependency],
    ) -> Result<Vec<&'a Dependency>> {
        let mut seen_deps = HashSet::new();
        let mut unique_deps = Vec::new();
        for (dep_index, dep) in dependencies.iter().enumerate() {
            if dep.issue_id != issue_id {
                return Err(BeadsError::validation(
                    "dependency.issue_id",
                    format!(
                        "dependency issue_id '{}' does not match import issue '{}'",
                        dep.issue_id, issue_id
                    ),
                ));
            }
            if issue_id == dep.depends_on_id {
                return Err(BeadsError::SelfDependency {
                    id: issue_id.to_string(),
                });
            }
            if let Some(metadata) = dep.metadata.as_deref() {
                serde_json::from_str::<serde_json::Value>(metadata).map_err(|err| {
                    BeadsError::Validation {
                        field: format!("dependencies[{dep_index}].metadata"),
                        reason: format!(
                            "dependency metadata must be valid JSON for issue {issue_id} -> \
                             {target} (type={dep_type}); found {value}: {err}{hint}",
                            target = dep.depends_on_id,
                            dep_type = dep.dep_type.as_str(),
                            value = if metadata.is_empty() {
                                "empty string".to_string()
                            } else {
                                format!("{metadata:?}")
                            },
                            hint = if metadata.trim().is_empty() {
                                " (suggested fix: replace \"\" with \"{}\")"
                            } else {
                                ""
                            },
                        ),
                    }
                })?;
            }
            Self::validate_parent_child_endpoints(
                issue_id,
                &dep.depends_on_id,
                dep.dep_type.as_str(),
            )?;
            // Deduplicate by target because the dependencies table is keyed by
            // (issue_id, depends_on_id). Type-distinct duplicates would be
            // ignored by insertion anyway.
            if seen_deps.insert(dep.depends_on_id.as_str()) {
                unique_deps.push(dep);
            }
        }

        Ok(unique_deps)
    }

    fn insert_dependency_refs_for_import(
        &self,
        issue_id: &str,
        unique_deps: &[&Dependency],
    ) -> Result<()> {
        if unique_deps.is_empty() {
            return Ok(());
        }

        for chunk in unique_deps.chunks(IMPORT_DEPENDENCY_CHUNK_SIZE) {
            let placeholders: Vec<String> = chunk
                .iter()
                .map(|_| "(?, ?, ?, ?, ?, ?, ?)".to_string())
                .collect();
            let sql = format!(
                "INSERT OR IGNORE INTO dependencies (issue_id, depends_on_id, type, created_at, created_by, metadata, thread_id) VALUES {}",
                placeholders.join(", ")
            );

            let mut params = Vec::with_capacity(chunk.len() * 7);
            for dep in chunk {
                params.push(SqliteValue::from(issue_id));
                params.push(SqliteValue::from(dep.depends_on_id.as_str()));
                params.push(SqliteValue::from(dep.dep_type.as_str()));
                params.push(SqliteValue::from(dep.created_at.to_rfc3339().as_str()));
                params.push(SqliteValue::from(
                    dep.created_by.as_deref().unwrap_or("import"),
                ));
                params.push(SqliteValue::from(dep.metadata.as_deref().unwrap_or("{}")));
                params.push(SqliteValue::from(dep.thread_id.as_deref().unwrap_or("")));
            }

            self.conn.execute_with_params(&sql, &params)?;
        }

        Ok(())
    }

    /// Sync comments for an issue (remove existing, add new).
    ///
    /// # Errors
    ///
    /// Returns an error if the database operation fails.
    pub fn sync_comments_for_import(
        &self,
        issue_id: &str,
        comments: &[crate::model::Comment],
    ) -> Result<()> {
        validate_import_comments_for_issue(issue_id, comments)?;

        // Remove existing comments
        self.conn.execute_with_params(
            "DELETE FROM comments WHERE issue_id = ?",
            &[SqliteValue::from(issue_id)],
        )?;

        self.insert_comment_rows_for_import(issue_id, comments)
    }

    /// Insert relation rows for an issue that was just created during import.
    ///
    /// The caller must only use this after a successful new issue insert. Update
    /// and upsert paths must keep using the `sync_*_for_import` methods so stale
    /// relation rows are removed.
    ///
    /// # Errors
    ///
    /// Returns an error if any relation insert fails.
    pub(crate) fn insert_new_issue_relations_for_import(&self, issue: &Issue) -> Result<()> {
        self.insert_labels_for_import(&issue.id, &issue.labels)?;
        self.insert_dependencies_for_import(&issue.id, &issue.dependencies)?;
        self.insert_comments_for_import(&issue.id, &issue.comments)?;
        Ok(())
    }

    fn insert_comments_for_import(
        &self,
        issue_id: &str,
        comments: &[crate::model::Comment],
    ) -> Result<()> {
        validate_import_comments_for_issue(issue_id, comments)?;

        self.insert_comment_rows_for_import(issue_id, comments)
    }

    fn insert_comment_rows_for_import(
        &self,
        issue_id: &str,
        comments: &[crate::model::Comment],
    ) -> Result<()> {
        if comments.is_empty() {
            return Ok(());
        }

        for comment in comments {
            self.insert_comment_for_import(issue_id, comment)?;
        }

        Ok(())
    }

    fn insert_comment_for_import(&self, issue_id: &str, comment: &Comment) -> Result<()> {
        let created_at = comment.created_at.to_rfc3339();
        if comment.id <= 0 {
            return self.insert_import_comment_without_id(issue_id, comment, &created_at);
        }

        match self.insert_import_comment_with_id(issue_id, comment, &created_at) {
            Ok(()) => Ok(()),
            Err(BeadsError::Database(error)) if is_import_comment_id_collision(&error) => {
                match self.import_comment_id_owner(comment.id)? {
                    // Whenever ANY row already owns this id — a comment on
                    // another issue OR an earlier comment of *this* issue whose
                    // id was AUTO-reallocated to the same value during this
                    // import — reinsert without an explicit id so AUTOINCREMENT
                    // assigns a fresh one. True same-issue JSONL duplicates are
                    // rejected earlier by `validate_import_comments_for_issue`,
                    // so this cannot silently swallow a genuine duplicate
                    // (issue #374).
                    Some(_) => {
                        self.insert_import_comment_without_id(issue_id, comment, &created_at)
                    }
                    None => Err(BeadsError::Database(error)),
                }
            }
            Err(error) => Err(error),
        }
    }

    fn insert_import_comment_without_id(
        &self,
        issue_id: &str,
        comment: &Comment,
        created_at: &str,
    ) -> Result<()> {
        self.conn.execute_with_params(
            "INSERT INTO comments (issue_id, author, text, created_at) VALUES (?, ?, ?, ?)",
            &[
                SqliteValue::from(issue_id),
                SqliteValue::from(comment.author.as_str()),
                SqliteValue::from(comment.body.as_str()),
                SqliteValue::from(created_at),
            ],
        )?;
        Ok(())
    }

    fn insert_import_comment_with_id(
        &self,
        issue_id: &str,
        comment: &Comment,
        created_at: &str,
    ) -> Result<()> {
        self.conn.execute_with_params(
            "INSERT INTO comments (id, issue_id, author, text, created_at) VALUES (?, ?, ?, ?, ?)",
            &[
                SqliteValue::from(comment.id),
                SqliteValue::from(issue_id),
                SqliteValue::from(comment.author.as_str()),
                SqliteValue::from(comment.body.as_str()),
                SqliteValue::from(created_at),
            ],
        )?;
        Ok(())
    }

    fn import_comment_id_owner(&self, comment_id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_with_params(
                "SELECT issue_id FROM comments WHERE id = ? LIMIT 1",
                &[SqliteValue::from(comment_id)],
            )?
            .into_iter()
            .next()
            .and_then(|row| {
                row.get(0)
                    .and_then(SqliteValue::as_text)
                    .map(str::to_string)
            }))
    }
}

fn is_import_comment_id_collision(error: &FrankenError) -> bool {
    matches!(
        error,
        FrankenError::PrimaryKeyViolation | FrankenError::UniqueViolation { .. }
    ) || matches!(
        error,
        FrankenError::Internal(message)
            if message.contains("VDBE halted with code 19")
                && (message.contains("PRIMARY KEY constraint failed")
                    || message.contains("UNIQUE constraint failed"))
    )
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

    fn would_create_parent_child_cycle(
        &self,
        child_id: &str,
        parent_id: &str,
    ) -> std::result::Result<bool, crate::error::BeadsError> {
        Self::check_parent_child_cycle(&self.conn, child_id, parent_id, true)
    }
}

fn validate_new_comment(issue_id: &str, author: &str, text: &str) -> Result<()> {
    let comment = Comment {
        id: 1,
        issue_id: issue_id.to_string(),
        author: author.to_string(),
        body: text.to_string(),
        created_at: Utc::now(),
    };

    CommentValidator::validate(&comment).map_err(BeadsError::from_validation_errors)
}

fn validate_issue_comments_for_create(issue: &Issue) -> Result<()> {
    for comment in &issue.comments {
        validate_new_comment(&issue.id, &comment.author, &comment.body)?;
    }

    Ok(())
}

fn validate_import_comments_for_issue(issue_id: &str, comments: &[Comment]) -> Result<()> {
    let mut seen_comment_ids = HashSet::new();
    for comment in comments {
        if comment.issue_id != issue_id {
            return Err(BeadsError::validation(
                "comment.issue_id",
                format!(
                    "comment issue_id '{}' does not match import issue '{}'",
                    comment.issue_id, issue_id
                ),
            ));
        }
        let comment_for_validation = Comment {
            id: 1,
            issue_id: issue_id.to_string(),
            author: comment.author.clone(),
            body: comment.body.clone(),
            created_at: comment.created_at,
        };
        CommentValidator::validate(&comment_for_validation)
            .map_err(BeadsError::from_validation_errors)?;

        if comment.id > 0 && !seen_comment_ids.insert(comment.id) {
            return Err(BeadsError::validation(
                "comment.id",
                format!("duplicate import comment id {}", comment.id),
            ));
        }
    }

    Ok(())
}

fn insert_comment_row(conn: &Connection, issue_id: &str, author: &str, text: &str) -> Result<i64> {
    conn.execute_with_params(
        "INSERT INTO comments (issue_id, author, text, created_at)
         VALUES (?, ?, ?, CURRENT_TIMESTAMP)",
        &[
            SqliteValue::from(issue_id),
            SqliteValue::from(author),
            SqliteValue::from(text),
        ],
    )?;
    let row = conn.query_row("SELECT last_insert_rowid()")?;
    let comment_id = row
        .get(0)
        .and_then(SqliteValue::as_integer)
        .ok_or_else(|| {
            BeadsError::Config("comments insert did not return last_insert_rowid".to_string())
        })?;
    if comment_id <= 0 {
        return Err(BeadsError::Config(format!(
            "comments insert returned invalid last_insert_rowid: {comment_id}"
        )));
    }
    Ok(comment_id)
}

fn fetch_comment(conn: &Connection, comment_id: i64) -> Result<Comment> {
    let row = match conn.query_row_with_params(
        "SELECT id, issue_id, author, text, created_at FROM comments WHERE id = ?",
        &[SqliteValue::from(comment_id)],
    ) {
        Ok(row) => row,
        Err(FrankenError::QueryReturnedNoRows) => {
            return Err(BeadsError::Config(format!(
                "comment {comment_id} not found after insert"
            )));
        }
        Err(error) => return Err(error.into()),
    };
    comment_from_row(&row)
}

fn comment_from_row(row: &fsqlite::Row) -> Result<Comment> {
    let id = row
        .get(0)
        .and_then(SqliteValue::as_integer)
        .ok_or_else(|| BeadsError::Config("comments row missing id".to_string()))?;
    let issue_id = row
        .get(1)
        .and_then(SqliteValue::as_text)
        .ok_or_else(|| BeadsError::Config(format!("comments row missing issue_id for {id}")))?
        .to_string();
    let author = row
        .get(2)
        .and_then(SqliteValue::as_text)
        .ok_or_else(|| BeadsError::Config(format!("comments row missing author for {id}")))?
        .to_string();
    let body = row
        .get(3)
        .and_then(SqliteValue::as_text)
        .ok_or_else(|| BeadsError::Config(format!("comments row missing body for {id}")))?
        .to_string();
    let created_at_value = row
        .get(4)
        .ok_or_else(|| BeadsError::Config(format!("comments row missing created_at for {id}")))?;
    let created_at = parse_datetime_value(Some(created_at_value)).map_err(|err| match err {
        BeadsError::Config(msg) => {
            BeadsError::Config(format!("invalid comment timestamp for {id}: {msg}"))
        }
        other => other,
    })?;

    Ok(Comment {
        id,
        issue_id,
        author,
        body,
        created_at,
    })
}

fn dedupe_preserving_order(values: &[String]) -> Vec<String> {
    let mut seen = HashSet::<&str>::new();
    let mut deduped = Vec::with_capacity(values.len());
    for value in values {
        if seen.insert(value) {
            deduped.push(value.clone());
        }
    }
    deduped
}

fn validate_storage_label(label: &str) -> Result<()> {
    LabelValidator::validate(label).map_err(|error| BeadsError::validation("label", error.message))
}

fn validate_storage_labels(labels: &[String]) -> Result<()> {
    if labels.len() > ISSUE_LABEL_MAX_COUNT {
        return Err(label_count_error());
    }

    for label in labels {
        validate_storage_label(label)?;
    }

    Ok(())
}

fn validate_storage_label_refs(labels: &[&String]) -> Result<()> {
    if labels.len() > ISSUE_LABEL_MAX_COUNT {
        return Err(label_count_error());
    }

    for label in labels {
        validate_storage_label(label.as_str())?;
    }

    Ok(())
}

fn label_count_error() -> BeadsError {
    BeadsError::validation("labels", format!("exceeds {ISSUE_LABEL_MAX_COUNT} labels"))
}

impl Drop for SqliteStorage {
    fn drop(&mut self) {
        // Read-only commands leave `mutation_count` at zero, so they keep
        // the original "no checkpoint on teardown" behaviour that prevents
        // spurious busy failures under parallel read traffic. Mutating
        // commands that committed since the last periodic checkpoint —
        // including the abnormal-exit case where signal-induced shutdown
        // (`crate::shutdown`) returns from main without re-entering
        // `with_write_transaction` — get one final TRUNCATE here so WAL
        // frames are not stranded on disk after the process ends (#270).
        if self.mutation_count > 0
            && let Err(e) = self.conn.execute("PRAGMA wal_checkpoint(TRUNCATE)")
        {
            tracing::debug!(error = %e, "WAL checkpoint on drop failed (non-fatal)");
        }
        // Explicitly close the connection to avoid fsqlite drop_close warnings.
        let _ = self.conn.close_in_place();
        // Ephemeral temp databases (open_memory) are unlinked here, after the
        // connection is closed, so the file and its WAL/SHM/journal sidecars are
        // not left behind in TMPDIR (#299). Persistent databases have
        // `temp_db_path == None` and are never touched.
        if let Some(path) = self.temp_db_path.take() {
            remove_temp_db_files(&path);
        }
    }
}

#[cfg(test)]
impl SqliteStorage {
    /// Execute raw SQL for tests.
    ///
    /// # Errors
    ///
    /// Returns an error if the SQL execution fails.
    pub fn execute_test_sql(&self, sql: &str) -> Result<()> {
        crate::storage::schema::execute_batch(&self.conn, sql)?;
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::similar_names)]
mod tests {
    use super::*;
    use crate::format::{
        BlockedIssueOutput, ReadyIssue, StaleIssue, TextFormatOptions, format_issue_line_with,
    };
    use crate::model::{Issue, IssueType, Priority, Status};
    use chrono::{DateTime, Datelike, TimeZone, Timelike, Utc};
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
            source_repo_path: None,
            agent_context: None,
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

    type ReadyTextFields = (
        String,
        String,
        Status,
        Priority,
        IssueType,
        DateTime<Utc>,
        DateTime<Utc>,
    );

    fn ready_text_fields(issue: Issue) -> ReadyTextFields {
        (
            issue.id,
            issue.title,
            issue.status,
            issue.priority,
            issue.issue_type,
            issue.created_at,
            issue.updated_at,
        )
    }

    fn issue_ids(issues: Vec<Issue>) -> Vec<String> {
        issues.into_iter().map(|issue| issue.id).collect()
    }

    fn ready_summary_projection_fixture() -> SqliteStorage {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let created_at = Utc.with_ymd_and_hms(2026, 3, 20, 12, 0, 0).unwrap();

        let mut ready = make_issue(
            "bd-ready-summary",
            "Ready summary issue",
            Status::Open,
            1,
            Some("alice"),
            created_at,
            None,
        );
        ready.description = Some("Description should stay cold".to_string());
        ready.design = Some("Design should stay cold".repeat(128));
        ready.acceptance_criteria = Some("AC should stay cold".repeat(128));
        ready.notes = Some("Notes should stay cold".repeat(128));
        ready.owner = Some("product".to_string());
        ready.estimated_minutes = Some(45);
        ready.created_by = Some("agent".to_string());
        ready.updated_at = created_at + chrono::Duration::minutes(5);

        let issues = [
            ready,
            make_issue(
                "bd-ready-summary-other",
                "Other ready summary issue",
                Status::Open,
                2,
                None,
                created_at + chrono::Duration::minutes(1),
                None,
            ),
            make_issue(
                "bd-ready-summary-blocker",
                "Blocker",
                Status::Open,
                0,
                None,
                created_at + chrono::Duration::minutes(2),
                None,
            ),
            make_issue(
                "bd-ready-summary-blocked",
                "Blocked",
                Status::Open,
                1,
                None,
                created_at + chrono::Duration::minutes(3),
                None,
            ),
        ];
        for issue in &issues {
            storage.create_issue(issue, "tester").unwrap();
        }
        storage
            .add_dependency(
                "bd-ready-summary-blocked",
                "bd-ready-summary-blocker",
                "blocks",
                "tester",
            )
            .unwrap();

        storage
    }

    fn blocker_id_from_ref_for_test(blocker_ref: &str) -> String {
        blocker_ref
            .rsplit_once(':')
            .map_or(blocker_ref, |(prefix, _)| prefix)
            .to_string()
    }

    fn blocked_issue_output_for_test(
        (issue, blockers): (Issue, Vec<String>),
    ) -> BlockedIssueOutput {
        BlockedIssueOutput {
            blocked_by: blockers
                .iter()
                .map(|blocker_ref| blocker_id_from_ref_for_test(blocker_ref))
                .collect(),
            blocked_by_count: blockers.len(),
            created_at: issue.created_at,
            created_by: issue.created_by,
            description: issue.description,
            id: issue.id,
            issue_type: issue.issue_type,
            priority: issue.priority,
            status: issue.status,
            title: issue.title,
            updated_at: issue.updated_at,
        }
    }

    #[test]
    fn test_dedupe_export_hash_batch_keeps_last_hash_in_first_position() {
        let exports = vec![
            ("bd-a".to_string(), "hash-a1".to_string()),
            ("bd-b".to_string(), "hash-b".to_string()),
            ("bd-a".to_string(), "hash-a2".to_string()),
        ];

        let deduped = SqliteStorage::dedupe_export_hash_batch(&exports);

        assert_eq!(
            deduped,
            vec![
                ("bd-a".to_string(), "hash-a2".to_string()),
                ("bd-b".to_string(), "hash-b".to_string()),
            ]
        );
    }

    fn insert_external_parent_child_dependency(
        storage: &SqliteStorage,
        external_child: &str,
        parent_id: &str,
        created_at: DateTime<Utc>,
    ) {
        // Temporarily disable FK checks for the raw INSERT since external
        // children do not exist in the local issues table.
        storage.conn.execute("PRAGMA foreign_keys = OFF").unwrap();
        storage
            .conn
            .execute_with_params(
                "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by)
                 VALUES (?, ?, 'parent-child', ?, ?)",
                &[
                    SqliteValue::from(external_child),
                    SqliteValue::from(parent_id),
                    SqliteValue::from(created_at.to_rfc3339()),
                    SqliteValue::from("tester"),
                ],
            )
            .unwrap();
        storage.conn.execute("PRAGMA foreign_keys = ON").unwrap();
    }

    fn insert_parent_child_dependency_for_test(
        storage: &SqliteStorage,
        child_id: &str,
        parent_id: &str,
        created_at: DateTime<Utc>,
    ) {
        storage
            .conn
            .execute_with_params(
                "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by)
                 VALUES (?, ?, 'parent-child', ?, ?)",
                &[
                    SqliteValue::from(child_id),
                    SqliteValue::from(parent_id),
                    SqliteValue::from(created_at.to_rfc3339()),
                    SqliteValue::from("tester"),
                ],
            )
            .unwrap();
    }

    #[test]
    fn test_open_memory() {
        let storage = SqliteStorage::open_memory();
        assert!(storage.is_ok(), "open_memory failed: {:?}", storage.err());
    }

    /// Regression for #299: `open_memory` is backed by a real temp file (because
    /// FrankenSQLite cannot use `:memory:`), and that file plus any WAL/SHM/
    /// journal sidecars must be unlinked when the storage is dropped — otherwise
    /// `beads_mem_*` files accumulate in `TMPDIR`.
    #[test]
    fn open_memory_temp_files_removed_on_drop() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let db_path = storage
            .temp_db_path
            .clone()
            .expect("open_memory must record its temp db path");

        // Perform a mutation so SQLite actually materializes the WAL sidecar,
        // exercising the sidecar-cleanup path and not just the base .db file.
        let issue = make_issue(
            "bd-1",
            "tmp leak repro",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        storage.create_issue(&issue, "tester").unwrap();

        assert!(
            db_path.exists(),
            "temp db file should exist while storage is open: {}",
            db_path.display()
        );

        drop(storage);

        // The base file and every sidecar SQLite may have created must be gone.
        let name = db_path
            .file_name()
            .and_then(|n| n.to_str())
            .expect("temp db name");
        let leftovers: Vec<PathBuf> = std::iter::once(db_path.clone())
            .chain(
                ["-wal", "-shm", "-journal"]
                    .iter()
                    .map(|s| db_path.with_file_name(format!("{name}{s}"))),
            )
            .filter(|p| p.exists())
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp db files left behind after drop: {leftovers:?}"
        );
    }

    /// A failed `open_memory` (or its drop) must not leave files behind either:
    /// `remove_temp_db_files` cleans up the base file and sidecars and tolerates
    /// missing files.
    #[test]
    fn remove_temp_db_files_clears_all_sidecars() {
        let dir = TempDir::new().unwrap();
        let base = dir.path().join("beads_mem_test_0.db");
        for suffix in ["", "-wal", "-shm", "-journal"] {
            fs::write(
                dir.path().join(format!("beads_mem_test_0.db{suffix}")),
                b"x",
            )
            .unwrap();
        }
        remove_temp_db_files(&base);
        for suffix in ["", "-wal", "-shm", "-journal"] {
            let p = dir.path().join(format!("beads_mem_test_0.db{suffix}"));
            assert!(!p.exists(), "should have been removed: {}", p.display());
        }
        // Idempotent / tolerant of already-missing files.
        remove_temp_db_files(&base);
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
            source_repo_path: None,
            agent_context: None,
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
        let count = storage
            .conn
            .query_row_with_params(
                "SELECT count(*) FROM issues WHERE id = ?",
                &[SqliteValue::from("bd-1")],
            )
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap_or(0);
        assert_eq!(count, 1);

        let persisted = storage
            .get_issue("bd-1")
            .expect("get created issue")
            .expect("created issue exists");
        let expected_hash = issue.compute_content_hash();
        assert_eq!(
            persisted.content_hash.as_deref(),
            Some(expected_hash.as_str()),
            "create_issue should persist the canonical content hash"
        );

        // Verify event
        let event_count = storage
            .conn
            .query_row_with_params(
                "SELECT count(*) FROM events WHERE issue_id = ?",
                &[SqliteValue::from("bd-1")],
            )
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap_or(0);
        assert_eq!(event_count, 1);

        // Verify dirty
        let dirty_count = storage
            .conn
            .query_row_with_params(
                "SELECT count(*) FROM dirty_issues WHERE issue_id = ?",
                &[SqliteValue::from("bd-1")],
            )
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap_or(0);
        assert_eq!(dirty_count, 1);
    }

    #[test]
    fn test_create_issue_rejects_invalid_issue_without_persisting() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let invalid = Issue {
            id: "bd-invalid-create".to_string(),
            title: "x".repeat(501),
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            created_at: Utc::now(),
            updated_at: Utc::now(),
            ..Issue::default()
        };

        let error = storage
            .create_issue(&invalid, "tester")
            .expect_err("invalid issues must be rejected before persistence");

        assert!(
            matches!(
                &error,
                BeadsError::Validation { field, reason }
                    if field == "title" && reason.contains("exceeds 500")
            ),
            "unexpected error: {error:?}"
        );
        assert!(
            storage
                .get_issue("bd-invalid-create")
                .expect("lookup invalid issue")
                .is_none(),
            "invalid issue must not be persisted"
        );
        assert!(
            storage
                .get_events("bd-invalid-create", 100)
                .expect("events")
                .is_empty(),
            "invalid issue must not emit events"
        );
        let dirty_ids = storage.get_dirty_issue_ids().expect("dirty marker");
        assert!(
            !dirty_ids.contains(&"bd-invalid-create".to_string()),
            "invalid issue must not be marked dirty"
        );
    }

    #[test]
    fn test_get_all_issues_metadata_preserves_custom_status() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue = make_issue(
            "bd-custom",
            "Custom status",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        storage.create_issue(&issue, "tester").unwrap();
        storage
            .execute_test_sql("UPDATE issues SET status = 'mystery-state' WHERE id = 'bd-custom'")
            .unwrap();

        let metadata = storage.get_all_issues_metadata().unwrap();
        let issue_meta = metadata
            .iter()
            .find(|meta| meta.id == "bd-custom")
            .expect("metadata for bd-custom");

        assert_eq!(
            issue_meta.status,
            Status::Custom("mystery-state".to_string())
        );
    }

    #[test]
    fn test_get_all_issues_metadata_errors_on_invalid_updated_at() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue = make_issue(
            "bd-bad-time",
            "Bad timestamp",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        storage.create_issue(&issue, "tester").unwrap();
        storage
            .execute_test_sql(
                "UPDATE issues SET updated_at = 'not-a-timestamp' WHERE id = 'bd-bad-time'",
            )
            .unwrap();

        let err = storage.get_all_issues_metadata().unwrap_err();
        assert!(
            matches!(&err, BeadsError::Config(message) if message.contains("unparseable datetime")),
            "unexpected error: {err:?}"
        );
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
    fn test_transaction_rolls_back_on_closure_error() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue = make_issue(
            "bd-tx-side-effect",
            "Tx Side Effect Test",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        storage.create_issue(&issue, "tester").unwrap();

        // Verify that a closure error causes a rollback.
        let result: Result<()> = storage.mutate("fail_in_closure", "tester", |_tx, _ctx| {
            Err(BeadsError::validation(
                "test",
                "intentional failure for rollback test",
            ))
        });

        assert!(
            result.is_err(),
            "closure error should propagate as transaction failure"
        );

        // Subsequent writes should still succeed after rollback.
        let follow_up = make_issue(
            "bd-tx-side-effect-2",
            "Follow Up",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        storage.create_issue(&follow_up, "tester").unwrap();
        assert!(
            storage.get_issue("bd-tx-side-effect-2").unwrap().is_some(),
            "subsequent writes should succeed after rollback"
        );
    }

    #[test]
    fn test_event_insert_for_missing_issue_succeeds_with_fk_disabled() {
        // FK enforcement is disabled during mutate transactions to work
        // around fsqlite's false FK violations (#215).  Events for
        // non-existent issue_ids are tolerated rather than rejected.
        let mut storage = SqliteStorage::open_memory().unwrap();
        let issue = make_issue(
            "bd-tx-fk",
            "FK Tolerance Test",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        storage.create_issue(&issue, "tester").unwrap();

        let result: Result<()> = storage.mutate("fk_tolerance", "tester", |_tx, ctx| {
            ctx.record_event(
                EventType::Updated,
                "bd-missing",
                Some("Event for non-existent issue".to_string()),
            );
            Ok(())
        });

        assert!(
            result.is_ok(),
            "event insert for missing issue should succeed with FK disabled"
        );
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
    fn test_external_blockers_return_sorted_blocker_refs() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 3, 3, 0, 0, 0).unwrap();
        let issue = make_issue(
            "bd-external-blocked",
            "External blocked",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&issue, "tester").unwrap();

        storage
            .add_dependency(
                "bd-external-blocked",
                "external:zproj:zcap",
                "blocks",
                "tester",
            )
            .unwrap();
        storage
            .add_dependency(
                "bd-external-blocked",
                "external:aproj:acap",
                "blocks",
                "tester",
            )
            .unwrap();

        let external_statuses = HashMap::from([
            ("external:zproj:zcap".to_string(), false),
            ("external:aproj:acap".to_string(), false),
        ]);
        let blockers = storage.external_blockers(&external_statuses).unwrap();
        let expected = vec![
            "external:aproj:acap:blocked".to_string(),
            "external:zproj:zcap:blocked".to_string(),
        ];
        assert_eq!(
            blockers.get("bd-external-blocked"),
            Some(&expected),
            "external blocker refs should not inherit dependency row order"
        );
    }

    #[test]
    fn test_add_dependency_rejects_external_parent_child_target() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 2, 0, 0, 0).unwrap();

        let issue_a = make_issue("bd-a1", "A", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue_a, "tester").unwrap();

        let err = storage
            .add_dependency(
                "bd-a1",
                "external:proj:capability",
                "parent-child",
                "tester",
            )
            .unwrap_err();

        assert!(matches!(err, BeadsError::Validation { field, .. } if field == "depends_on_id"));
    }

    #[test]
    fn test_has_external_dependencies_detects_external_parent_child_children() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 3, 3, 0, 0, 0).unwrap();
        let mut parent = make_issue("bd-p1", "Parent", Status::Open, 2, None, t1, None);
        parent.issue_type = IssueType::Epic;
        storage.create_issue(&parent, "tester").unwrap();

        insert_external_parent_child_dependency(&storage, "external:extproj:child", "bd-p1", t1);

        assert!(storage.has_external_dependencies(true).unwrap());
        assert!(storage.has_external_dependencies(false).unwrap());
        assert!(storage.may_have_blocked_command_results().unwrap());
    }

    #[test]
    fn test_missing_issue_references_allows_external_dependency_endpoints() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 3, 3, 0, 0, 0).unwrap();
        let local = make_issue("bd-local", "Local issue", Status::Open, 2, None, t1, None);
        let mut epic = make_issue("bd-epic", "Epic issue", Status::Open, 2, None, t1, None);
        epic.issue_type = IssueType::Epic;

        storage.create_issue(&local, "tester").unwrap();
        storage.create_issue(&epic, "tester").unwrap();
        storage
            .add_dependency("bd-local", "external:target:cap", "blocks", "tester")
            .unwrap();
        insert_external_parent_child_dependency(&storage, "external:child:cap", "bd-epic", t1);

        assert!(
            !storage
                .has_missing_issue_reference("dependencies", "depends_on_id")
                .unwrap(),
            "external dependency targets are valid cross-project blockers"
        );
        assert!(
            !storage
                .has_missing_issue_reference("dependencies", "issue_id")
                .unwrap(),
            "external parent-child children are valid cross-project endpoints"
        );
        assert_eq!(
            storage.missing_issue_references().unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn test_external_parent_child_task_parent_is_not_blocked_candidate() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 3, 4, 0, 0, 0).unwrap();
        let parent = make_issue(
            "bd-task-parent",
            "Task Parent",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&parent, "tester").unwrap();

        insert_external_parent_child_dependency(
            &storage,
            "external:extproj:task-child",
            "bd-task-parent",
            t1,
        );

        assert!(!storage.has_external_dependencies(true).unwrap());
        assert!(storage.has_external_dependencies(false).unwrap());
        assert!(!storage.may_have_blocked_command_results().unwrap());
    }

    #[test]
    fn test_blocking_only_external_resolution_skips_non_epic_external_children() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 3, 5, 0, 0, 0).unwrap();
        let direct = make_issue(
            "bd-direct",
            "Direct blocker",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        let task_parent = make_issue(
            "bd-task-parent",
            "Task Parent",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        let mut epic_parent = make_issue(
            "bd-epic-parent",
            "Epic Parent",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        epic_parent.issue_type = IssueType::Epic;

        for issue in [direct, task_parent, epic_parent] {
            storage.create_issue(&issue, "tester").unwrap();
        }
        storage
            .add_dependency(
                "bd-direct",
                "external:direct:capability",
                "blocks",
                "tester",
            )
            .unwrap();
        insert_external_parent_child_dependency(
            &storage,
            "external:extproj:task-child",
            "bd-task-parent",
            t1,
        );
        insert_external_parent_child_dependency(
            &storage,
            "external:extproj:epic-child",
            "bd-epic-parent",
            t1,
        );

        let statuses = storage
            .resolve_external_dependency_statuses(&HashMap::new(), true)
            .unwrap();

        assert_eq!(statuses.get("external:direct:capability"), Some(&false));
        assert_eq!(statuses.get("external:extproj:epic-child"), Some(&false));
        assert!(
            !statuses.contains_key("external:extproj:task-child"),
            "blocking-only resolution should not query external children of non-epic parents"
        );
    }

    #[test]
    fn test_external_parent_child_child_only_blocks_epic_parent() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 3, 6, 0, 0, 0).unwrap();
        let task_parent = make_issue(
            "bd-task-parent",
            "Task Parent",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        let mut epic_parent = make_issue(
            "bd-epic-parent",
            "Epic Parent",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        epic_parent.issue_type = IssueType::Epic;
        storage.create_issue(&task_parent, "tester").unwrap();
        storage.create_issue(&epic_parent, "tester").unwrap();

        for (external_child, parent_id) in [
            ("external:extproj:task-child", "bd-task-parent"),
            ("external:extproj:epic-child", "bd-epic-parent"),
        ] {
            insert_external_parent_child_dependency(&storage, external_child, parent_id, t1);
        }

        let external_statuses = HashMap::from([
            ("external:extproj:task-child".to_string(), false),
            ("external:extproj:epic-child".to_string(), false),
        ]);
        let blockers = storage.external_blockers(&external_statuses).unwrap();

        assert!(
            !blockers.contains_key("bd-task-parent"),
            "external children should not make non-epic parents blocked"
        );
        let epic_blockers = blockers
            .get("bd-epic-parent")
            .expect("external child should block epic parent");
        assert_eq!(epic_blockers.len(), 1);
        assert_eq!(
            epic_blockers.first().map(String::as_str),
            Some("external:extproj:epic-child:child-blocked")
        );
    }

    #[test]
    fn test_blocked_command_candidate_probe_empty_cache_and_no_external_deps_is_false() {
        let storage = SqliteStorage::open_memory().unwrap();

        assert!(!storage.may_have_blocked_command_results().unwrap());
    }

    #[test]
    fn test_blocked_command_candidate_probe_detects_local_blocked_cache() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 3, 4, 0, 0, 0).unwrap();
        let blocker = make_issue("bd-blocker", "Blocker", Status::Open, 2, None, t1, None);
        let blocked = make_issue("bd-blocked", "Blocked", Status::Open, 2, None, t1, None);
        storage.create_issue(&blocker, "tester").unwrap();
        storage.create_issue(&blocked, "tester").unwrap();
        storage
            .add_dependency("bd-blocked", "bd-blocker", "blocks", "tester")
            .unwrap();

        assert!(storage.may_have_blocked_command_results().unwrap());
    }

    #[test]
    fn test_blocked_command_candidate_probe_detects_external_blockers() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 3, 5, 0, 0, 0).unwrap();
        let issue = make_issue("bd-p1", "Parent", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue, "tester").unwrap();
        storage
            .add_dependency("bd-p1", "external:extproj:capability", "blocks", "tester")
            .unwrap();

        assert!(storage.may_have_blocked_command_results().unwrap());
    }

    /// Regression for beads_rust#285. The issue reported that `br close`
    /// persisted to JSONL but not to the SQLite store and that the
    /// dirty-tracker stayed empty — meaning the JSONL→DB→JSONL
    /// reconciliation never fired for the row. Pins both halves of
    /// the close-as-update contract: the SQLite row reports
    /// `status='closed'` post-update, and `dirty_issues` contains the
    /// id so the next flush exports the change. If anyone regresses
    /// `update_issue`'s `ctx.mark_dirty(id)` call (sqlite.rs:2634 at
    /// the time this test was added) this fails fast.
    #[test]
    fn test_close_path_marks_dirty_and_persists_to_db() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2026, 5, 1, 0, 0, 0).unwrap();

        let issue = make_issue("bd-close-285", "Close me", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue, "tester").unwrap();

        // Sanity: dirty-tracker is empty after create. (`create_issue`
        // marks dirty, so we clear it explicitly so the assertion below
        // measures only the close path's contribution.)
        storage.clear_all_dirty_issues().unwrap();
        assert_eq!(storage.get_dirty_issue_count().unwrap(), 0);

        let close_update = IssueUpdate {
            status: Some(Status::Closed),
            closed_at: Some(Some(Utc.with_ymd_and_hms(2026, 5, 2, 0, 0, 0).unwrap())),
            close_reason: Some(Some("done".to_string())),
            skip_cache_rebuild: true,
            ..IssueUpdate::default()
        };
        let updated = storage
            .update_issue("bd-close-285", &close_update, "tester")
            .expect("close path must succeed");

        // Half 1: the in-memory return value reports Closed.
        assert_eq!(updated.status, Status::Closed, "returned issue.status");

        // Half 2: the SQLite store actually reflects Closed. Reading
        // through `get_issue` exercises the same query path consumers
        // use; a raw SELECT is unnecessary because the user-visible
        // symptom is the wrong status surfacing through that API.
        let reloaded = storage
            .get_issue("bd-close-285")
            .expect("get_issue")
            .expect("issue must still exist after close");
        assert_eq!(
            reloaded.status,
            Status::Closed,
            "SQLite row must report closed; if this fails, br close persisted to JSONL but not DB (issue #285)"
        );

        // Half 3: dirty_issues queued the close so the next flush can
        // export it. If this count is zero the JSONL→DB reconciliation
        // path has nothing to act on and divergence accumulates over
        // time (the 2.4% drift rate the issue reports).
        assert_eq!(
            storage.get_dirty_issue_count().unwrap(),
            1,
            "close path must enqueue dirty_issues; without this br sync --flush-only is a no-op after close (issue #285)"
        );
        let dirty: Vec<(String, String)> = storage.get_dirty_issue_metadata().unwrap();
        assert_eq!(dirty.len(), 1);
        assert_eq!(dirty[0].0, "bd-close-285");
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
    fn test_update_issue_writes_source_repo_path() {
        // Regression for #289: `br update --source-repo-path PATH` must
        // round-trip through SQLite. Without writing to the column, the
        // installed checkpoint would silently lose the value on every
        // create-then-update cycle.
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 5, 1, 0, 0, 0).unwrap();
        let issue = make_issue(
            "bd-srp",
            "source_repo_path RT",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&issue, "tester").unwrap();

        let updates = IssueUpdate {
            source_repo: Some(Some("widget_engine".to_string())),
            source_repo_path: Some(Some("/data/projects/widget_engine".to_string())),
            ..IssueUpdate::default()
        };
        let updated = storage.update_issue("bd-srp", &updates, "tester").unwrap();
        assert_eq!(updated.source_repo.as_deref(), Some("widget_engine"));
        assert_eq!(
            updated.source_repo_path.as_deref(),
            Some("/data/projects/widget_engine")
        );

        // Read back through a fresh fetch to prove it persisted (not just
        // returned from the in-memory `Issue` the update builder mutates).
        let reread = storage.get_issue("bd-srp").unwrap().unwrap();
        assert_eq!(reread.source_repo.as_deref(), Some("widget_engine"));
        assert_eq!(
            reread.source_repo_path.as_deref(),
            Some("/data/projects/widget_engine")
        );

        // Clear path: passing an empty string through `optional_string_field`
        // sets the inner Option to None, which should write SQL NULL.
        let clear = IssueUpdate {
            source_repo_path: Some(None),
            ..IssueUpdate::default()
        };
        let cleared = storage.update_issue("bd-srp", &clear, "tester").unwrap();
        assert!(cleared.source_repo_path.is_none());
        let reread = storage.get_issue("bd-srp").unwrap().unwrap();
        assert!(reread.source_repo_path.is_none());
    }

    #[test]
    fn test_update_issue_same_priority_is_noop() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 5, 1, 0, 0, 0).unwrap();

        let issue = make_issue(
            "bd-u-priority",
            "No-op priority",
            Status::Open,
            1,
            None,
            t1,
            None,
        );
        storage.create_issue(&issue, "tester").unwrap();
        storage.clear_all_dirty_issues().unwrap();

        let before = storage.get_issue("bd-u-priority").unwrap().unwrap();
        let updated = storage
            .update_issue(
                "bd-u-priority",
                &IssueUpdate {
                    priority: Some(Priority::HIGH),
                    ..IssueUpdate::default()
                },
                "tester",
            )
            .unwrap();

        assert_eq!(updated.priority, Priority::HIGH);
        assert_eq!(updated.updated_at, before.updated_at);
        assert!(storage.get_dirty_issue_ids().unwrap().is_empty());
    }

    #[test]
    fn test_update_issue_rejects_existing_tombstone() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 5, 1, 0, 0, 0).unwrap();

        let issue = make_issue(
            "bd-u-tomb",
            "Do not resurrect",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&issue, "tester").unwrap();
        let tombstone = storage
            .delete_issue("bd-u-tomb", "tester", "delete for update test", None)
            .unwrap();
        storage.clear_all_dirty_issues().unwrap();

        let err = storage
            .update_issue(
                "bd-u-tomb",
                &IssueUpdate {
                    title: Some("Resurrected title".to_string()),
                    status: Some(Status::Open),
                    deleted_at: Some(None),
                    deleted_by: Some(None),
                    delete_reason: Some(None),
                    ..IssueUpdate::default()
                },
                "tester",
            )
            .unwrap_err();

        assert!(
            matches!(
                err,
                BeadsError::Validation {
                    ref field,
                    ref reason
                }
                    if field == "issue_id"
                        && reason.contains("cannot update tombstone issue: bd-u-tomb")
            ),
            "unexpected error: {err:?}"
        );

        let after = storage.get_issue("bd-u-tomb").unwrap().unwrap();
        assert_eq!(after.status, Status::Tombstone);
        assert_eq!(after.title, tombstone.title);
        assert_eq!(after.deleted_at, tombstone.deleted_at);
        assert_eq!(after.deleted_by, tombstone.deleted_by);
        assert_eq!(after.delete_reason, tombstone.delete_reason);
        assert!(storage.get_dirty_issue_ids().unwrap().is_empty());
    }

    /*
    #[test]
    fn test_update_issue_recomputes_hash_from_fresh_transaction_state() {
        use std::sync::mpsc;
        use std::time::Duration;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("beads.db");

        let mut setup = SqliteStorage::open(&db_path).unwrap();
        let issue = make_issue(
            "bd-race1",
            "Original title",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        setup.create_issue(&issue, "tester").unwrap();
        drop(setup);

        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let writer_db_path = db_path.clone();

        let writer = std::thread::spawn(move || {
            let storage = SqliteStorage::open(&writer_db_path).unwrap();
            storage.conn.execute("BEGIN IMMEDIATE").unwrap();
            storage
                .conn
                .execute_with_params(
                    "UPDATE issues SET description = ?, updated_at = ? WHERE id = ?",
                    &[
                        SqliteValue::from("Thread description"),
                        SqliteValue::from(Utc::now().to_rfc3339()),
                        SqliteValue::from("bd-race1"),
                    ],
                )
                .unwrap();
            ready_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            storage.conn.execute("COMMIT").unwrap();
        });

        ready_rx.recv().unwrap();

        let updater_db_path = db_path;
        let updater = std::thread::spawn(move || {
            let mut storage = SqliteStorage::open(&updater_db_path).unwrap();
            let updates = IssueUpdate {
                title: Some("Updated title".to_string()),
                ..IssueUpdate::default()
            };
            storage
                .update_issue("bd-race1", &updates, "tester")
                .unwrap();
            storage.get_issue("bd-race1").unwrap().unwrap()
        });

        std::thread::sleep(Duration::from_millis(50));
        release_tx.send(()).unwrap();

        writer.join().unwrap();
        let updated = updater.join().unwrap();

        assert_eq!(updated.description.as_deref(), Some("Thread description"));
        assert_eq!(
            updated.content_hash.as_deref(),
            Some(crate::util::content_hash(&updated).as_str())
        );
    }
    */

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
    fn test_reopen_records_reopened_event() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap();

        let issue = make_issue("bd-r1", "Reopen me", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue, "tester").unwrap();

        let close_update = IssueUpdate {
            status: Some(Status::Closed),
            ..IssueUpdate::default()
        };
        storage
            .update_issue("bd-r1", &close_update, "tester")
            .unwrap();

        let reopen_update = IssueUpdate {
            status: Some(Status::Open),
            closed_at: Some(None),
            close_reason: Some(None),
            closed_by_session: Some(None),
            ..IssueUpdate::default()
        };
        storage
            .update_issue("bd-r1", &reopen_update, "tester")
            .unwrap();

        let events = storage.get_events("bd-r1", 10).unwrap();
        println!("Events: {:#?}", events);
        assert!(
            events
                .iter()
                .any(|event| event.event_type == EventType::Reopened)
        );
    }

    #[test]
    fn test_reopen_clears_close_metadata() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap();

        let issue = make_issue(
            "bd-rmeta",
            "Reopen metadata",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&issue, "tester").unwrap();

        let close_update = IssueUpdate {
            status: Some(Status::Closed),
            close_reason: Some(Some("done".to_string())),
            ..IssueUpdate::default()
        };
        storage
            .update_issue("bd-rmeta", &close_update, "tester")
            .unwrap();
        storage
            .record_close_metadata(
                "bd-rmeta",
                &crate::close_policy::AttributionValues::default(),
                false,
                None,
                &[],
            )
            .unwrap();
        assert!(storage.get_close_metadata("bd-rmeta").unwrap().is_some());

        let reopen_update = IssueUpdate {
            status: Some(Status::Open),
            closed_at: Some(None),
            close_reason: Some(None),
            closed_by_session: Some(None),
            ..IssueUpdate::default()
        };
        storage
            .update_issue("bd-rmeta", &reopen_update, "tester")
            .unwrap();

        assert!(storage.get_close_metadata("bd-rmeta").unwrap().is_none());
    }

    #[test]
    fn test_delete_issue_clears_close_metadata() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap();

        let issue = make_issue(
            "bd-dmeta",
            "Delete metadata",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&issue, "tester").unwrap();

        let close_update = IssueUpdate {
            status: Some(Status::Closed),
            close_reason: Some(Some("done".to_string())),
            ..IssueUpdate::default()
        };
        storage
            .update_issue("bd-dmeta", &close_update, "tester")
            .unwrap();
        storage
            .record_close_metadata(
                "bd-dmeta",
                &crate::close_policy::AttributionValues::default(),
                false,
                None,
                &[],
            )
            .unwrap();
        assert!(storage.get_close_metadata("bd-dmeta").unwrap().is_some());

        storage
            .delete_issue("bd-dmeta", "tester", "cleanup", None)
            .unwrap();

        assert!(storage.get_close_metadata("bd-dmeta").unwrap().is_none());
    }

    #[test]
    fn test_delete_issue_recomputes_content_hash_for_tombstone() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap();

        let issue = make_issue("bd-d2", "Delete me too", Status::Open, 2, None, t1, None);
        let original_hash = issue.content_hash.clone();
        storage.create_issue(&issue, "tester").unwrap();

        let deleted = storage
            .delete_issue("bd-d2", "tester", "cleanup", None)
            .unwrap();

        assert_eq!(deleted.status, Status::Tombstone);
        assert_ne!(deleted.content_hash, original_hash);
        assert_eq!(
            deleted.content_hash.as_deref(),
            Some(crate::util::content_hash(&deleted).as_str())
        );
    }

    #[test]
    fn test_delete_issue_is_idempotent_for_tombstones() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap();
        let deleted_at = Utc.with_ymd_and_hms(2025, 6, 2, 0, 0, 0).unwrap();

        let issue = make_issue("bd-d3", "Already deleted", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue, "tester").unwrap();
        let first = storage
            .delete_issue("bd-d3", "first", "first delete", Some(deleted_at))
            .unwrap();
        storage.clear_dirty_flags(&["bd-d3".to_string()]).unwrap();

        let second = storage
            .delete_issue("bd-d3", "second", "second delete", None)
            .unwrap();

        assert_eq!(second.status, Status::Tombstone);
        assert_eq!(second.deleted_at, first.deleted_at);
        assert_eq!(second.deleted_by, first.deleted_by);
        assert_eq!(second.delete_reason, first.delete_reason);
        assert_eq!(second.updated_at, first.updated_at);
        assert_eq!(storage.get_dirty_issue_ids().unwrap(), Vec::<String>::new());
        assert_eq!(
            storage
                .get_events("bd-d3", 10)
                .unwrap()
                .iter()
                .filter(|event| event.event_type == EventType::Deleted)
                .count(),
            1
        );
    }

    #[test]
    fn test_purge_issue_succeeds_without_fk_side_effects() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap();

        let issue = make_issue("bd-p1", "Purge me", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue, "tester").unwrap();

        storage.purge_issue("bd-p1", "tester").unwrap();

        assert!(storage.get_issue("bd-p1").unwrap().is_none());

        let dirty_count = storage
            .conn
            .query_row("SELECT COUNT(*) FROM dirty_issues WHERE issue_id = 'bd-p1'")
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap_or(0);
        assert_eq!(dirty_count, 0);

        let event_count = storage
            .conn
            .query_row("SELECT COUNT(*) FROM events WHERE issue_id = 'bd-p1'")
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap_or(0);
        assert_eq!(event_count, 0);
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
    fn test_count_all_relation_counts_matches_chunked_counts() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 4, 1, 0, 0, 0).unwrap();

        for id in ["bd-a", "bd-b", "bd-c"] {
            let issue = make_issue(id, id, Status::Open, 1, None, t1, None);
            storage.create_issue(&issue, "tester").unwrap();
        }

        storage
            .add_dependency("bd-b", "bd-a", "blocks", "tester")
            .unwrap();
        storage
            .add_dependency("bd-c", "bd-a", "blocks", "tester")
            .unwrap();
        storage
            .add_dependency("bd-c", "bd-b", "blocks", "tester")
            .unwrap();

        let ids = vec!["bd-a".to_string(), "bd-b".to_string(), "bd-c".to_string()];
        let chunked_counts = storage.count_relation_counts_for_issues(&ids).unwrap();
        let all_counts = storage.count_all_relation_counts().unwrap();

        assert_eq!(all_counts, chunked_counts);
    }

    #[test]
    fn test_scheduler_evidence_helpers_handle_default_candidate_window() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 4, 1, 0, 0, 0).unwrap();
        let ids = (0..512)
            .map(|index| format!("bd-window-{index:03}"))
            .collect::<Vec<_>>();

        for id in &ids {
            let issue = make_issue(id, id, Status::Open, 1, None, t1, None);
            storage.create_issue(&issue, "tester").unwrap();
            storage.add_label(id, "scheduler", "tester").unwrap();
        }
        for issue_id in ids.iter().skip(1) {
            storage
                .add_dependency(issue_id, &ids[0], "blocks", "tester")
                .unwrap();
        }

        let labels = storage.get_labels_for_issues(&ids).unwrap();
        let (dependency_counts, dependent_counts) =
            storage.count_relation_counts_for_issues(&ids).unwrap();

        assert_eq!(labels.len(), ids.len());
        assert!(ids.iter().all(|id| labels[id] == ["scheduler"]));
        assert_eq!(*dependent_counts.get(&ids[0]).unwrap_or(&0), ids.len() - 1);
        for issue_id in ids.iter().skip(1) {
            assert_eq!(*dependency_counts.get(issue_id).unwrap_or(&0), 1);
        }
    }

    #[test]
    fn test_all_list_relation_metadata_matches_separate_helpers() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 4, 1, 0, 0, 0).unwrap();

        for id in ["bd-a", "bd-b", "bd-c"] {
            let issue = make_issue(id, id, Status::Open, 1, None, t1, None);
            storage.create_issue(&issue, "tester").unwrap();
        }
        storage.add_label("bd-a", "backend", "tester").unwrap();
        storage.add_label("bd-a", "urgent", "tester").unwrap();
        storage.add_label("bd-b", "backend", "tester").unwrap();
        storage
            .add_dependency("bd-b", "bd-a", "blocks", "tester")
            .unwrap();
        storage
            .add_dependency("bd-c", "bd-a", "blocks", "tester")
            .unwrap();
        storage
            .add_dependency("bd-c", "bd-b", "blocks", "tester")
            .unwrap();

        let labels = storage.get_all_labels().unwrap();
        let (dependency_counts, dependent_counts) = storage.count_all_relation_counts().unwrap();
        let metadata = storage.get_all_list_relation_metadata().unwrap();

        for id in ["bd-a", "bd-b", "bd-c"] {
            let entry = metadata.get(id);
            assert_eq!(
                entry
                    .map(|metadata| metadata.labels.as_slice())
                    .unwrap_or(&[]),
                labels.get(id).map(Vec::as_slice).unwrap_or(&[])
            );
            assert_eq!(
                entry.map_or(0, |metadata| metadata.dependency_count),
                *dependency_counts.get(id).unwrap_or(&0)
            );
            assert_eq!(
                entry.map_or(0, |metadata| metadata.dependent_count),
                *dependent_counts.get(id).unwrap_or(&0)
            );
        }
    }

    #[test]
    fn test_get_blocked_issues_includes_nonterminal_statuses() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 4, 1, 0, 0, 0).unwrap();

        let blocker = make_issue("bd-b1", "Blocker", Status::Open, 1, None, t1, None);
        let mut deferred = make_issue(
            "bd-b2",
            "Deferred blocked",
            Status::Deferred,
            2,
            None,
            t1,
            None,
        );
        deferred.defer_until = Some(t1 + chrono::Duration::days(1));
        let custom = make_issue(
            "bd-b3",
            "Custom blocked",
            Status::Custom("review".to_string()),
            2,
            None,
            t1,
            None,
        );

        storage.create_issue(&blocker, "tester").unwrap();
        storage.create_issue(&deferred, "tester").unwrap();
        storage.create_issue(&custom, "tester").unwrap();

        storage
            .add_dependency("bd-b2", "bd-b1", "blocks", "tester")
            .unwrap();
        storage
            .add_dependency("bd-b3", "bd-b1", "blocks", "tester")
            .unwrap();

        let blocked_issues = storage.get_blocked_issues().unwrap();
        let ids: HashSet<_> = blocked_issues
            .iter()
            .map(|(issue, _)| issue.id.as_str())
            .collect();
        assert!(ids.contains("bd-b2"));
        assert!(ids.contains("bd-b3"));
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
    fn test_add_label_rejects_invalid_storage_label() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 1, 0, 0, 0).unwrap();

        let issue = make_issue(
            "bd-l-invalid",
            "Invalid label",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&issue, "tester").unwrap();

        let err = storage
            .add_label("bd-l-invalid", "bad label", "tester")
            .expect_err("invalid labels must be rejected at storage boundary");
        assert!(err.to_string().contains("invalid characters"));
        assert!(storage.get_labels("bd-l-invalid").unwrap().is_empty());
    }

    #[test]
    fn test_remove_label_rejects_invalid_storage_label_without_mutating() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 1, 0, 0, 0).unwrap();

        let issue = make_issue(
            "bd-l-remove-invalid",
            "Invalid label removal",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&issue, "tester").unwrap();
        storage
            .add_label("bd-l-remove-invalid", "backend", "tester")
            .unwrap();

        let err = storage
            .remove_label("bd-l-remove-invalid", "bad label", "tester")
            .expect_err("invalid removal label must be rejected at storage boundary");
        assert!(err.to_string().contains("invalid characters"));
        assert_eq!(
            storage.get_labels("bd-l-remove-invalid").unwrap(),
            vec!["backend".to_string()]
        );
    }

    #[test]
    fn test_add_label_enforces_issue_label_limit() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 1, 0, 0, 0).unwrap();

        let issue = make_issue("bd-l-cap", "Label cap", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue, "tester").unwrap();

        for index in 0..ISSUE_LABEL_MAX_COUNT {
            let label = format!("label-{index:02}");
            assert!(storage.add_label("bd-l-cap", &label, "tester").unwrap());
        }

        assert!(
            !storage.add_label("bd-l-cap", "label-00", "tester").unwrap(),
            "duplicate labels should remain an idempotent no-op at the cap"
        );
        let err = storage
            .add_label("bd-l-cap", "label-extra", "tester")
            .expect_err("new label beyond cap must fail");
        assert!(err.to_string().contains("exceeds 64 labels"));
        assert_eq!(
            storage.get_labels("bd-l-cap").unwrap().len(),
            ISSUE_LABEL_MAX_COUNT
        );
    }

    #[test]
    fn test_add_label_to_issues_bulk_marks_only_changed_ids() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 1, 0, 0, 0).unwrap();

        for id in ["bd-bulk-a", "bd-bulk-b", "bd-bulk-c"] {
            let issue = make_issue(id, id, Status::Open, 2, None, t1, None);
            storage.create_issue(&issue, "tester").unwrap();
        }
        storage
            .add_label("bd-bulk-b", "bulk-added", "tester")
            .unwrap();
        storage.clear_all_dirty_issues().unwrap();

        let ids = vec![
            "bd-bulk-a".to_string(),
            "bd-bulk-b".to_string(),
            "bd-bulk-c".to_string(),
            "bd-bulk-a".to_string(),
        ];
        let changed = storage
            .add_label_to_issues_bulk(&ids, "bulk-added", "tester")
            .unwrap();
        let expected = ["bd-bulk-a", "bd-bulk-c"]
            .into_iter()
            .map(String::from)
            .collect::<HashSet<_>>();
        assert_eq!(changed, expected);

        for id in ["bd-bulk-a", "bd-bulk-b", "bd-bulk-c"] {
            assert!(
                storage
                    .get_labels(id)
                    .unwrap()
                    .contains(&"bulk-added".to_string()),
                "{id} should have the bulk-added label"
            );
        }

        let mut dirty = storage.get_dirty_issue_ids().unwrap();
        dirty.sort();
        assert_eq!(
            dirty,
            vec!["bd-bulk-a".to_string(), "bd-bulk-c".to_string()]
        );
    }

    #[test]
    fn test_add_label_to_issues_bulk_handles_sqlite_var_limit_boundary() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 1, 0, 0, 0).unwrap();
        let ids = (0..SQLITE_VAR_LIMIT)
            .map(|index| format!("bd-bulk-boundary-{index:03}"))
            .collect::<Vec<_>>();

        for id in &ids {
            let issue = make_issue(id, id, Status::Open, 2, None, t1, None);
            storage.create_issue(&issue, "tester").unwrap();
        }
        storage.clear_all_dirty_issues().unwrap();

        let changed = storage
            .add_label_to_issues_bulk(&ids, "bulk-boundary", "tester")
            .unwrap();
        let expected = ids.iter().cloned().collect::<HashSet<_>>();
        assert_eq!(changed, expected);

        let mut dirty = storage.get_dirty_issue_ids().unwrap();
        dirty.sort();
        assert_eq!(dirty, ids);
    }

    #[test]
    fn test_add_label_to_issues_bulk_rejects_cap_without_partial_mutation() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 1, 0, 0, 0).unwrap();

        for id in ["bd-bulk-cap-ok", "bd-bulk-cap-full"] {
            let issue = make_issue(id, id, Status::Open, 2, None, t1, None);
            storage.create_issue(&issue, "tester").unwrap();
        }
        for index in 0..ISSUE_LABEL_MAX_COUNT {
            let label = format!("label-{index:02}");
            storage
                .add_label("bd-bulk-cap-full", &label, "tester")
                .unwrap();
        }
        storage.clear_all_dirty_issues().unwrap();

        let ids = vec!["bd-bulk-cap-ok".to_string(), "bd-bulk-cap-full".to_string()];
        let err = storage
            .add_label_to_issues_bulk(&ids, "label-extra", "tester")
            .expect_err("bulk label add must reject a target at the label cap");
        assert!(err.to_string().contains("exceeds 64 labels"));
        assert!(
            !storage
                .get_labels("bd-bulk-cap-ok")
                .unwrap()
                .contains(&"label-extra".to_string()),
            "bulk validation should avoid partially labeling earlier targets"
        );
        assert!(storage.get_dirty_issue_ids().unwrap().is_empty());
    }

    #[test]
    fn test_add_label_to_issues_bulk_rejects_tombstone_without_partial_mutation() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 1, 0, 0, 0).unwrap();

        for id in ["bd-bulk-active", "bd-bulk-tomb"] {
            let issue = make_issue(id, id, Status::Open, 2, None, t1, None);
            storage.create_issue(&issue, "tester").unwrap();
        }
        storage
            .delete_issue("bd-bulk-tomb", "tester", "delete target", None)
            .unwrap();
        storage.clear_all_dirty_issues().unwrap();

        let ids = vec!["bd-bulk-active".to_string(), "bd-bulk-tomb".to_string()];
        let err = storage
            .add_label_to_issues_bulk(&ids, "bulk-added", "tester")
            .expect_err("bulk label add must reject tombstones");
        assert!(
            err.to_string()
                .contains("cannot add label to tombstone issue: bd-bulk-tomb")
        );
        assert!(storage.get_labels("bd-bulk-active").unwrap().is_empty());
        assert!(storage.get_dirty_issue_ids().unwrap().is_empty());
    }

    #[test]
    fn test_remove_label_from_issues_bulk_marks_only_changed_ids() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 1, 0, 0, 0).unwrap();

        for id in ["bd-bulk-remove-a", "bd-bulk-remove-b", "bd-bulk-remove-c"] {
            let issue = make_issue(id, id, Status::Open, 2, None, t1, None);
            storage.create_issue(&issue, "tester").unwrap();
        }
        storage
            .add_label("bd-bulk-remove-a", "bulk-removed", "tester")
            .unwrap();
        storage
            .add_label("bd-bulk-remove-c", "bulk-removed", "tester")
            .unwrap();
        storage.clear_all_dirty_issues().unwrap();

        let ids = vec![
            "bd-bulk-remove-a".to_string(),
            "bd-bulk-remove-b".to_string(),
            "bd-bulk-remove-c".to_string(),
            "bd-bulk-remove-a".to_string(),
        ];
        let changed = storage
            .remove_label_from_issues_bulk(&ids, "bulk-removed", "tester")
            .unwrap();
        let expected = ["bd-bulk-remove-a", "bd-bulk-remove-c"]
            .into_iter()
            .map(String::from)
            .collect::<HashSet<_>>();
        assert_eq!(changed, expected);

        for id in ["bd-bulk-remove-a", "bd-bulk-remove-b", "bd-bulk-remove-c"] {
            assert!(
                !storage
                    .get_labels(id)
                    .unwrap()
                    .contains(&"bulk-removed".to_string()),
                "{id} should not have the bulk-removed label"
            );
        }

        let mut dirty = storage.get_dirty_issue_ids().unwrap();
        dirty.sort();
        assert_eq!(
            dirty,
            vec![
                "bd-bulk-remove-a".to_string(),
                "bd-bulk-remove-c".to_string()
            ]
        );
    }

    #[test]
    fn test_remove_label_from_issues_bulk_handles_sqlite_var_limit_boundary() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 1, 0, 0, 0).unwrap();
        let ids = (0..SQLITE_VAR_LIMIT)
            .map(|index| format!("bd-bulk-remove-boundary-{index:03}"))
            .collect::<Vec<_>>();

        for id in &ids {
            let issue = make_issue(id, id, Status::Open, 2, None, t1, None);
            storage.create_issue(&issue, "tester").unwrap();
        }
        let added = storage
            .add_label_to_issues_bulk(&ids, "bulk-remove-boundary", "tester")
            .unwrap();
        assert_eq!(added, ids.iter().cloned().collect::<HashSet<_>>());
        storage.clear_all_dirty_issues().unwrap();

        let changed = storage
            .remove_label_from_issues_bulk(&ids, "bulk-remove-boundary", "tester")
            .unwrap();
        let expected = ids.iter().cloned().collect::<HashSet<_>>();
        assert_eq!(changed, expected);

        let mut dirty = storage.get_dirty_issue_ids().unwrap();
        dirty.sort();
        assert_eq!(dirty, ids);
    }

    #[test]
    fn test_remove_label_from_issues_bulk_rejects_tombstone_without_partial_mutation() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 1, 0, 0, 0).unwrap();

        for id in ["bd-bulk-remove-active", "bd-bulk-remove-tomb"] {
            let issue = make_issue(id, id, Status::Open, 2, None, t1, None);
            storage.create_issue(&issue, "tester").unwrap();
        }
        storage
            .add_label("bd-bulk-remove-active", "bulk-removed", "tester")
            .unwrap();
        storage
            .delete_issue("bd-bulk-remove-tomb", "tester", "delete target", None)
            .unwrap();
        storage.clear_all_dirty_issues().unwrap();

        let ids = vec![
            "bd-bulk-remove-active".to_string(),
            "bd-bulk-remove-tomb".to_string(),
        ];
        let err = storage
            .remove_label_from_issues_bulk(&ids, "bulk-removed", "tester")
            .expect_err("bulk label remove must reject tombstones");
        assert!(
            err.to_string()
                .contains("cannot remove label from tombstone issue: bd-bulk-remove-tomb")
        );
        assert_eq!(
            storage.get_labels("bd-bulk-remove-active").unwrap(),
            vec!["bulk-removed".to_string()]
        );
        assert!(storage.get_dirty_issue_ids().unwrap().is_empty());
    }

    #[test]
    fn test_set_labels_deduplicates_input() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 1, 0, 0, 0).unwrap();

        let issue = make_issue("bd-l2", "Dedup labels", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue, "tester").unwrap();

        storage
            .set_labels(
                "bd-l2",
                &[
                    "backend".to_string(),
                    "backend".to_string(),
                    "api".to_string(),
                ],
                "tester",
            )
            .unwrap();

        let labels = storage.get_labels("bd-l2").unwrap();
        assert_eq!(labels, vec!["api".to_string(), "backend".to_string()]);
    }

    #[test]
    fn test_set_labels_validates_before_replacing_existing_labels() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 1, 0, 0, 0).unwrap();

        let issue = make_issue(
            "bd-l-set-invalid",
            "Set labels",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&issue, "tester").unwrap();
        storage
            .set_labels("bd-l-set-invalid", &["stable".to_string()], "tester")
            .unwrap();

        let err = storage
            .set_labels("bd-l-set-invalid", &["bad label".to_string()], "tester")
            .expect_err("invalid replacement label must fail before deleting old labels");
        assert!(err.to_string().contains("invalid characters"));
        assert_eq!(
            storage.get_labels("bd-l-set-invalid").unwrap(),
            vec!["stable".to_string()]
        );

        let too_many = (0..=ISSUE_LABEL_MAX_COUNT)
            .map(|index| format!("label-{index:02}"))
            .collect::<Vec<_>>();
        let err = storage
            .set_labels("bd-l-set-invalid", &too_many, "tester")
            .expect_err("too many replacement labels must fail");
        assert!(err.to_string().contains("exceeds 64 labels"));
        assert_eq!(
            storage.get_labels("bd-l-set-invalid").unwrap(),
            vec!["stable".to_string()]
        );
    }

    #[test]
    fn test_sync_labels_for_import_validates_before_replacing_existing_labels() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 1, 0, 0, 0).unwrap();

        let issue = make_issue(
            "bd-l-import-invalid",
            "Import labels",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&issue, "tester").unwrap();
        storage
            .add_label("bd-l-import-invalid", "stable", "tester")
            .unwrap();

        let err = storage
            .sync_labels_for_import("bd-l-import-invalid", &["bad label".to_string()])
            .expect_err("invalid import labels must fail before deleting old labels");
        assert!(err.to_string().contains("invalid characters"));
        assert_eq!(
            storage.get_labels("bd-l-import-invalid").unwrap(),
            vec!["stable".to_string()]
        );

        let too_many = (0..=ISSUE_LABEL_MAX_COUNT)
            .map(|index| format!("label-{index:02}"))
            .collect::<Vec<_>>();
        let err = storage
            .sync_labels_for_import("bd-l-import-invalid", &too_many)
            .expect_err("too many import labels must fail before deleting old labels");
        assert!(err.to_string().contains("exceeds 64 labels"));
        assert_eq!(
            storage.get_labels("bd-l-import-invalid").unwrap(),
            vec!["stable".to_string()]
        );

        storage
            .sync_labels_for_import(
                "bd-l-import-invalid",
                &[
                    "backend".to_string(),
                    "backend".to_string(),
                    "api".to_string(),
                ],
            )
            .unwrap();
        assert_eq!(
            storage.get_labels("bd-l-import-invalid").unwrap(),
            vec!["api".to_string(), "backend".to_string()]
        );
    }

    fn setup_dependency_import_validation_storage() -> (SqliteStorage, Issue, Issue, DateTime<Utc>)
    {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 1, 0, 0, 0).unwrap();
        let issue = make_issue(
            "bd-d-import-invalid",
            "Import dependencies",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        let stable_parent = make_issue(
            "bd-d-import-stable",
            "Stable parent",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&issue, "tester").unwrap();
        storage.create_issue(&stable_parent, "tester").unwrap();
        storage
            .add_dependency(&issue.id, &stable_parent.id, "blocks", "tester")
            .unwrap();

        (storage, issue, stable_parent, t1)
    }

    fn assert_stable_dependency_unchanged(
        storage: &SqliteStorage,
        issue_id: &str,
        stable_parent_id: &str,
    ) {
        let dependencies = storage.get_dependencies_full(issue_id).unwrap();
        assert_eq!(dependencies.len(), 1);
        assert_eq!(dependencies[0].depends_on_id, stable_parent_id);
        assert_eq!(
            dependencies[0].dep_type,
            crate::model::DependencyType::Blocks
        );
    }

    #[test]
    fn test_sync_dependencies_for_import_rejects_self_and_wrong_source_before_replacing() {
        let (storage, issue, stable_parent, t1) = setup_dependency_import_validation_storage();

        let self_dependency = crate::model::Dependency {
            issue_id: issue.id.clone(),
            depends_on_id: issue.id.clone(),
            dep_type: crate::model::DependencyType::Blocks,
            created_at: t1,
            created_by: Some("import".to_string()),
            metadata: None,
            thread_id: None,
        };
        let err = storage
            .sync_dependencies_for_import(&issue.id, &[self_dependency])
            .expect_err("self-dependency must fail before deleting old dependencies");
        assert!(matches!(err, BeadsError::SelfDependency { id } if id == issue.id));
        assert_stable_dependency_unchanged(&storage, &issue.id, &stable_parent.id);

        let wrong_source = crate::model::Dependency {
            issue_id: "bd-d-import-other".to_string(),
            depends_on_id: stable_parent.id.clone(),
            dep_type: crate::model::DependencyType::Blocks,
            created_at: t1,
            created_by: Some("import".to_string()),
            metadata: None,
            thread_id: None,
        };
        let err = storage
            .sync_dependencies_for_import(&issue.id, &[wrong_source])
            .expect_err("wrong dependency issue_id must fail before deleting old dependencies");
        assert!(
            err.to_string().contains("dependency.issue_id"),
            "unexpected dependency owner validation error: {err:?}"
        );
        assert_stable_dependency_unchanged(&storage, &issue.id, &stable_parent.id);
    }

    #[test]
    fn test_sync_dependencies_for_import_rejects_metadata_and_parent_before_replacing() {
        let (storage, issue, stable_parent, t1) = setup_dependency_import_validation_storage();

        let invalid_metadata = crate::model::Dependency {
            issue_id: issue.id.clone(),
            depends_on_id: stable_parent.id.clone(),
            dep_type: crate::model::DependencyType::Blocks,
            created_at: t1,
            created_by: Some("import".to_string()),
            metadata: Some("{not-json".to_string()),
            thread_id: None,
        };
        let err = storage
            .sync_dependencies_for_import(&issue.id, &[invalid_metadata])
            .expect_err("invalid dependency metadata must fail before deleting old dependencies");
        assert!(
            matches!(&err, BeadsError::Validation { field, .. } if field == "dependencies[0].metadata"),
            "metadata validation error must name the offending dependency field: {err:?}"
        );
        // #323: the error must be actionable — naming the issue and target.
        let msg = err.to_string();
        assert!(
            msg.contains(&issue.id) && msg.contains(&stable_parent.id),
            "metadata error must name issue and target: {msg}"
        );
        assert_stable_dependency_unchanged(&storage, &issue.id, &stable_parent.id);

        let invalid_parent = crate::model::Dependency {
            issue_id: issue.id.clone(),
            depends_on_id: "external:parent".to_string(),
            dep_type: crate::model::DependencyType::ParentChild,
            created_at: t1,
            created_by: Some("import".to_string()),
            metadata: None,
            thread_id: None,
        };
        let err = storage
            .sync_dependencies_for_import(&issue.id, &[invalid_parent])
            .expect_err("invalid import dependency must fail before deleting old dependencies");
        assert!(err.to_string().contains("parent-child dependencies"));

        assert_stable_dependency_unchanged(&storage, &issue.id, &stable_parent.id);
    }

    #[test]
    fn test_bulk_label_mutations_reject_tombstones() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 1, 0, 0, 0).unwrap();

        let issue = make_issue(
            "bd-l-tomb",
            "Deleted label target",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&issue, "tester").unwrap();
        storage
            .add_label("bd-l-tomb", "existing", "tester")
            .unwrap();
        storage
            .delete_issue("bd-l-tomb", "tester", "delete label target", None)
            .unwrap();

        let set_error = storage
            .set_labels("bd-l-tomb", &["new".to_string()], "tester")
            .unwrap_err();
        assert!(
            matches!(
                &set_error,
                BeadsError::Validation { field, reason }
                    if field == "issue_id"
                        && reason.contains("cannot set labels on tombstone issue: bd-l-tomb")
            ),
            "unexpected set_labels error: {set_error:?}"
        );

        let remove_error = storage
            .remove_all_labels("bd-l-tomb", "tester")
            .unwrap_err();
        assert!(
            matches!(
                &remove_error,
                BeadsError::Validation { field, reason }
                    if field == "issue_id"
                        && reason.contains("cannot remove labels from tombstone issue: bd-l-tomb")
            ),
            "unexpected remove_all_labels error: {remove_error:?}"
        );

        let labels = storage.get_labels("bd-l-tomb").unwrap();
        assert_eq!(labels, vec!["existing".to_string()]);
    }

    #[test]
    fn test_unique_label_counts_exclude_tombstone_issues() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 1, 0, 0, 0).unwrap();

        let active = make_issue(
            "bd-l-active",
            "Active label count target",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        let tombstone = make_issue(
            "bd-l-deleted",
            "Deleted label count target",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&active, "tester").unwrap();
        storage.create_issue(&tombstone, "tester").unwrap();
        storage
            .add_label("bd-l-active", "shared", "tester")
            .unwrap();
        storage
            .add_label("bd-l-active", "active-only", "tester")
            .unwrap();
        storage
            .add_label("bd-l-deleted", "shared", "tester")
            .unwrap();
        storage
            .add_label("bd-l-deleted", "deleted-only", "tester")
            .unwrap();
        storage
            .delete_issue("bd-l-deleted", "tester", "delete label count target", None)
            .unwrap();

        assert_eq!(
            storage.get_unique_labels_with_counts().unwrap(),
            vec![("active-only".to_string(), 1), ("shared".to_string(), 1),]
        );
    }

    #[test]
    fn test_rename_label_same_name_is_noop() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 1, 0, 0, 0).unwrap();

        let issue = make_issue("bd-l3", "Rename label", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue, "tester").unwrap();
        storage.add_label("bd-l3", "backend", "tester").unwrap();
        let event_count_before = storage.get_events("bd-l3", 100).unwrap().len();

        let affected = storage
            .rename_label("backend", "backend", "tester")
            .unwrap();

        assert_eq!(affected, 0);
        assert_eq!(
            storage.get_labels("bd-l3").unwrap(),
            vec!["backend".to_string()]
        );
        assert_eq!(
            storage.get_events("bd-l3", 100).unwrap().len(),
            event_count_before
        );
    }

    #[test]
    fn test_rename_label_rejects_invalid_label_names_without_mutating() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 1, 0, 0, 0).unwrap();

        let issue = make_issue(
            "bd-l-rename-invalid",
            "Rename invalid label",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&issue, "tester").unwrap();
        storage
            .add_label("bd-l-rename-invalid", "backend", "tester")
            .unwrap();

        let new_name_error = storage
            .rename_label("backend", "bad label", "tester")
            .expect_err("invalid replacement label must fail before mutation");
        assert!(new_name_error.to_string().contains("invalid characters"));
        assert_eq!(
            storage.get_labels("bd-l-rename-invalid").unwrap(),
            vec!["backend".to_string()]
        );

        let old_name_error = storage
            .rename_label("bad label", "frontend", "tester")
            .expect_err("invalid source label must fail before mutation");
        assert!(old_name_error.to_string().contains("invalid characters"));
        assert_eq!(
            storage.get_labels("bd-l-rename-invalid").unwrap(),
            vec!["backend".to_string()]
        );

        let same_name_error = storage
            .rename_label("bad label", "bad label", "tester")
            .expect_err("invalid same-name rename must not be treated as a no-op");
        assert!(same_name_error.to_string().contains("invalid characters"));
    }

    #[test]
    fn test_rename_label_skips_tombstone_issues() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 1, 0, 0, 0).unwrap();

        let active = make_issue(
            "bd-l-active",
            "Active label target",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        let tombstone = make_issue(
            "bd-l-deleted",
            "Deleted label target",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&active, "tester").unwrap();
        storage.create_issue(&tombstone, "tester").unwrap();
        storage
            .add_label("bd-l-active", "legacy", "tester")
            .unwrap();
        storage
            .add_label("bd-l-deleted", "legacy", "tester")
            .unwrap();
        storage
            .delete_issue("bd-l-deleted", "tester", "delete label target", None)
            .unwrap();

        let affected = storage.rename_label("legacy", "renamed", "tester").unwrap();

        assert_eq!(affected, 1);
        assert_eq!(
            storage.get_labels("bd-l-active").unwrap(),
            vec!["renamed".to_string()]
        );
        assert_eq!(
            storage.get_labels("bd-l-deleted").unwrap(),
            vec!["legacy".to_string()]
        );
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
    fn test_bulk_dependency_import_inserts_acyclic_edges_once() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 2, 0, 0, 0).unwrap();

        let issue_a = make_issue("bd-bulk-a", "A", Status::Open, 2, None, t1, None);
        let issue_b = make_issue("bd-bulk-b", "B", Status::Open, 2, None, t1, None);
        let issue_c = make_issue("bd-bulk-c", "C", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue_a, "tester").unwrap();
        storage.create_issue(&issue_b, "tester").unwrap();
        storage.create_issue(&issue_c, "tester").unwrap();

        let inserted = storage
            .add_dependencies_bulk_for_import(
                &[
                    BulkDependencyInsert {
                        issue_id: "bd-bulk-a".to_string(),
                        depends_on_id: "bd-bulk-b".to_string(),
                        dep_type: "blocks".to_string(),
                    },
                    BulkDependencyInsert {
                        issue_id: "bd-bulk-b".to_string(),
                        depends_on_id: "bd-bulk-c".to_string(),
                        dep_type: "blocks".to_string(),
                    },
                    BulkDependencyInsert {
                        issue_id: "bd-bulk-a".to_string(),
                        depends_on_id: "bd-bulk-b".to_string(),
                        dep_type: "blocks".to_string(),
                    },
                ],
                "tester",
            )
            .unwrap();

        assert_eq!(inserted, 2);
        assert_eq!(
            storage.get_dependencies("bd-bulk-a").unwrap(),
            vec!["bd-bulk-b".to_string()]
        );
        assert_eq!(
            storage.get_dependencies("bd-bulk-b").unwrap(),
            vec!["bd-bulk-c".to_string()]
        );
    }

    #[test]
    fn test_bulk_dependency_import_rejects_cycle_before_partial_insert() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 2, 0, 0, 0).unwrap();

        let issue_a = make_issue("bd-bulk-cycle-a", "A", Status::Open, 2, None, t1, None);
        let issue_b = make_issue("bd-bulk-cycle-b", "B", Status::Open, 2, None, t1, None);
        let issue_c = make_issue("bd-bulk-cycle-c", "C", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue_a, "tester").unwrap();
        storage.create_issue(&issue_b, "tester").unwrap();
        storage.create_issue(&issue_c, "tester").unwrap();
        storage
            .add_dependency("bd-bulk-cycle-a", "bd-bulk-cycle-b", "blocks", "tester")
            .unwrap();

        let error = storage
            .add_dependencies_bulk_for_import(
                &[
                    BulkDependencyInsert {
                        issue_id: "bd-bulk-cycle-b".to_string(),
                        depends_on_id: "bd-bulk-cycle-c".to_string(),
                        dep_type: "blocks".to_string(),
                    },
                    BulkDependencyInsert {
                        issue_id: "bd-bulk-cycle-c".to_string(),
                        depends_on_id: "bd-bulk-cycle-a".to_string(),
                        dep_type: "blocks".to_string(),
                    },
                ],
                "tester",
            )
            .unwrap_err();

        assert!(
            matches!(error, BeadsError::DependencyCycle { .. }),
            "unexpected bulk cycle error: {error:?}"
        );
        assert!(
            storage
                .get_dependencies("bd-bulk-cycle-b")
                .unwrap()
                .is_empty()
        );
        assert!(
            storage
                .get_dependencies("bd-bulk-cycle-c")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_add_dependency_existing_pair_skips_cycle_check() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 2, 0, 0, 0).unwrap();

        let issue_a = make_issue("bd-existing-a", "A", Status::Open, 2, None, t1, None);
        let issue_b = make_issue("bd-existing-b", "B", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue_a, "tester").unwrap();
        storage.create_issue(&issue_b, "tester").unwrap();

        storage
            .add_dependency("bd-existing-a", "bd-existing-b", "related", "tester")
            .unwrap();
        storage
            .add_dependency("bd-existing-b", "bd-existing-a", "blocks", "tester")
            .unwrap();

        let added = storage
            .add_dependency("bd-existing-a", "bd-existing-b", "blocks", "tester")
            .expect("existing pair should return unchanged instead of false cycle");

        assert!(!added);
        let dep_types: Vec<String> = storage
            .get_dependencies_full("bd-existing-a")
            .unwrap()
            .into_iter()
            .map(|dep| dep.dep_type.as_str().to_string())
            .collect();
        assert_eq!(dep_types, vec!["related".to_string()]);
    }

    #[test]
    fn test_bulk_dependency_import_ignores_existing_pairs_before_cycle_check() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 2, 0, 0, 0).unwrap();

        let issue_a = make_issue("bd-bulk-existing-a", "A", Status::Open, 2, None, t1, None);
        let issue_b = make_issue("bd-bulk-existing-b", "B", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue_a, "tester").unwrap();
        storage.create_issue(&issue_b, "tester").unwrap();
        storage
            .add_dependency(
                "bd-bulk-existing-a",
                "bd-bulk-existing-b",
                "related",
                "tester",
            )
            .unwrap();

        let inserted = storage
            .add_dependencies_bulk_for_import(
                &[
                    BulkDependencyInsert {
                        issue_id: "bd-bulk-existing-a".to_string(),
                        depends_on_id: "bd-bulk-existing-b".to_string(),
                        dep_type: "blocks".to_string(),
                    },
                    BulkDependencyInsert {
                        issue_id: "bd-bulk-existing-b".to_string(),
                        depends_on_id: "bd-bulk-existing-a".to_string(),
                        dep_type: "blocks".to_string(),
                    },
                ],
                "tester",
            )
            .expect("ignored duplicate pair should not create a false proposed cycle");

        assert_eq!(inserted, 1);
        let dep_types_a: Vec<String> = storage
            .get_dependencies_full("bd-bulk-existing-a")
            .unwrap()
            .into_iter()
            .map(|dep| dep.dep_type.as_str().to_string())
            .collect();
        assert_eq!(dep_types_a, vec!["related".to_string()]);
        assert_eq!(
            storage.get_dependencies("bd-bulk-existing-b").unwrap(),
            vec!["bd-bulk-existing-a".to_string()]
        );
    }

    #[test]
    fn test_parent_child_cycle_check_allows_existing_parent_to_child_blocker_edge() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 2, 0, 0, 0).unwrap();

        let parent = make_issue(
            "bd-pc-cycle-parent",
            "Parent",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        let child = make_issue(
            "bd-pc-cycle-child",
            "Child",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&parent, "tester").unwrap();
        storage.create_issue(&child, "tester").unwrap();

        storage
            .add_dependency(
                "bd-pc-cycle-parent",
                "bd-pc-cycle-child",
                "blocks",
                "tester",
            )
            .unwrap();

        assert!(
            storage
                .would_create_cycle("bd-pc-cycle-child", "bd-pc-cycle-parent", true)
                .unwrap(),
            "standard edge semantics would see the existing parent -> child blocker path"
        );
        assert!(
            !storage
                .would_create_parent_child_cycle("bd-pc-cycle-child", "bd-pc-cycle-parent", true)
                .unwrap(),
            "parent-child semantics must check the prospective parent -> child graph edge"
        );
        assert!(
            storage
                .add_dependency(
                    "bd-pc-cycle-child",
                    "bd-pc-cycle-parent",
                    "parent-child",
                    "tester",
                )
                .unwrap()
        );
        assert!(
            storage.detect_blocking_cycles().unwrap().is_empty(),
            "duplicate parent -> child graph edges are acyclic"
        );
    }

    #[test]
    fn test_parent_child_cycle_check_rejects_reversed_hierarchy_cycle() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 2, 0, 0, 0).unwrap();

        let parent = make_issue(
            "bd-pc-cycle-existing-parent",
            "Existing parent",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        let child = make_issue(
            "bd-pc-cycle-existing-child",
            "Existing child",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&parent, "tester").unwrap();
        storage.create_issue(&child, "tester").unwrap();
        storage
            .add_dependency(
                "bd-pc-cycle-existing-child",
                "bd-pc-cycle-existing-parent",
                "parent-child",
                "tester",
            )
            .unwrap();

        assert!(
            storage
                .would_create_parent_child_cycle(
                    "bd-pc-cycle-existing-parent",
                    "bd-pc-cycle-existing-child",
                    true,
                )
                .unwrap(),
            "making the existing parent a child of its child must be rejected"
        );
        let error = storage
            .add_dependency(
                "bd-pc-cycle-existing-parent",
                "bd-pc-cycle-existing-child",
                "parent-child",
                "tester",
            )
            .unwrap_err();
        assert!(
            matches!(error, BeadsError::DependencyCycle { .. }),
            "unexpected reversed hierarchy error: {error:?}"
        );
        assert_eq!(
            storage
                .get_parent_id("bd-pc-cycle-existing-parent")
                .unwrap(),
            None
        );
    }

    #[test]
    fn test_add_dependency_rejects_second_parent_child_parent() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 2, 0, 0, 0).unwrap();

        let child = make_issue(
            "bd-single-parent-child",
            "Child",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        let parent_a = make_issue(
            "bd-single-parent-a",
            "Parent A",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        let parent_b = make_issue(
            "bd-single-parent-b",
            "Parent B",
            Status::Open,
            2,
            None,
            t1,
            None,
        );

        storage.create_issue(&child, "tester").unwrap();
        storage.create_issue(&parent_a, "tester").unwrap();
        storage.create_issue(&parent_b, "tester").unwrap();

        assert!(
            storage
                .add_dependency(
                    "bd-single-parent-child",
                    "bd-single-parent-a",
                    "Parent-Child",
                    "tester",
                )
                .unwrap()
        );
        assert!(
            !storage
                .add_dependency(
                    "bd-single-parent-child",
                    "bd-single-parent-a",
                    "parent-child",
                    "tester",
                )
                .unwrap(),
            "adding the same parent-child row should remain idempotent"
        );

        let error = storage
            .add_dependency(
                "bd-single-parent-child",
                "bd-single-parent-b",
                "parent-child",
                "tester",
            )
            .unwrap_err();
        assert!(
            matches!(
                &error,
                BeadsError::Validation { field, reason }
                    if field == "depends_on_id"
                        && reason.contains("already has parent bd-single-parent-a")
            ),
            "unexpected second-parent error: {error:?}"
        );
        assert_eq!(
            storage
                .get_parent_id("bd-single-parent-child")
                .unwrap()
                .as_deref(),
            Some("bd-single-parent-a")
        );
    }

    #[test]
    fn test_dependency_mutations_reject_tombstone_issue() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 2, 0, 0, 0).unwrap();

        let child = make_issue("bd-dep-child", "Child", Status::Open, 2, None, t1, None);
        let old_parent = make_issue(
            "bd-dep-old-parent",
            "Old parent",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        let new_parent = make_issue(
            "bd-dep-new-parent",
            "New parent",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&child, "tester").unwrap();
        storage.create_issue(&old_parent, "tester").unwrap();
        storage.create_issue(&new_parent, "tester").unwrap();
        storage
            .set_parent("bd-dep-child", Some("bd-dep-old-parent"), "tester")
            .unwrap();
        storage
            .delete_issue("bd-dep-child", "tester", "delete dependency target", None)
            .unwrap();

        let remove_error = storage
            .remove_dependency("bd-dep-child", "bd-dep-old-parent", "tester")
            .unwrap_err();
        assert!(
            matches!(
                &remove_error,
                BeadsError::Validation { field, reason }
                    if field == "issue_id"
                        && reason.contains(
                            "cannot remove dependency from tombstone issue: bd-dep-child"
                        )
            ),
            "unexpected remove_dependency error: {remove_error:?}"
        );

        let clear_parent_error = storage
            .set_parent("bd-dep-child", None, "tester")
            .unwrap_err();
        assert!(
            matches!(
                &clear_parent_error,
                BeadsError::Validation { field, reason }
                    if field == "issue_id"
                        && reason.contains(
                            "cannot clear parent from tombstone issue: bd-dep-child"
                        )
            ),
            "unexpected clear parent error: {clear_parent_error:?}"
        );

        let remove_parent_error = storage.remove_parent("bd-dep-child", "tester").unwrap_err();
        assert!(
            matches!(
                &remove_parent_error,
                BeadsError::Validation { field, reason }
                    if field == "issue_id"
                        && reason.contains(
                            "cannot clear parent from tombstone issue: bd-dep-child"
                        )
            ),
            "unexpected remove_parent error: {remove_parent_error:?}"
        );

        let set_parent_error = storage
            .set_parent("bd-dep-child", Some("bd-dep-new-parent"), "tester")
            .unwrap_err();
        assert!(
            matches!(
                &set_parent_error,
                BeadsError::Validation { field, reason }
                    if field == "issue_id"
                        && reason.contains("cannot set parent on tombstone issue: bd-dep-child")
            ),
            "unexpected set parent error: {set_parent_error:?}"
        );

        let deps = storage.get_dependencies("bd-dep-child").unwrap();
        assert_eq!(deps, vec!["bd-dep-old-parent".to_string()]);
    }

    #[test]
    fn test_set_parent_same_parent_is_noop() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 3, 0, 0, 0).unwrap();

        let child = make_issue(
            "bd-parent-noop-child",
            "Child",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        let parent = make_issue(
            "bd-parent-noop-parent",
            "Parent",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&child, "tester").unwrap();
        storage.create_issue(&parent, "tester").unwrap();
        storage
            .set_parent(
                "bd-parent-noop-child",
                Some("bd-parent-noop-parent"),
                "tester",
            )
            .unwrap();
        storage.clear_all_dirty_issues().unwrap();

        let before = storage
            .get_issue("bd-parent-noop-child")
            .unwrap()
            .expect("child exists");
        let event_count_before = storage
            .get_events("bd-parent-noop-child", 100)
            .unwrap()
            .len();

        storage
            .set_parent(
                "bd-parent-noop-child",
                Some("bd-parent-noop-parent"),
                "tester",
            )
            .unwrap();

        let after = storage
            .get_issue("bd-parent-noop-child")
            .unwrap()
            .expect("child exists");
        assert_eq!(after.updated_at, before.updated_at);
        assert_eq!(
            storage
                .get_parent_id("bd-parent-noop-child")
                .unwrap()
                .as_deref(),
            Some("bd-parent-noop-parent")
        );
        assert_eq!(
            storage
                .get_events("bd-parent-noop-child", 100)
                .unwrap()
                .len(),
            event_count_before
        );
        assert!(storage.get_dirty_issue_ids().unwrap().is_empty());
    }

    #[test]
    fn test_set_parent_same_requested_parent_cleans_extra_parent_rows() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 5, 0, 0, 0).unwrap();

        let child = make_issue(
            "bd-parent-cleanup-child",
            "Child",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        let parent = make_issue(
            "bd-parent-cleanup-parent",
            "Parent",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        let extra_parent = make_issue(
            "bd-parent-cleanup-extra-parent",
            "Extra parent",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&child, "tester").unwrap();
        storage.create_issue(&parent, "tester").unwrap();
        storage.create_issue(&extra_parent, "tester").unwrap();
        insert_parent_child_dependency_for_test(
            &storage,
            "bd-parent-cleanup-child",
            "bd-parent-cleanup-parent",
            t1,
        );
        insert_parent_child_dependency_for_test(
            &storage,
            "bd-parent-cleanup-child",
            "bd-parent-cleanup-extra-parent",
            t1,
        );
        assert_eq!(
            storage
                .get_parent_id("bd-parent-cleanup-child")
                .unwrap()
                .as_deref(),
            Some("bd-parent-cleanup-extra-parent")
        );
        storage.clear_all_dirty_issues().unwrap();

        storage
            .set_parent(
                "bd-parent-cleanup-child",
                Some("bd-parent-cleanup-parent"),
                "tester",
            )
            .unwrap();

        assert_eq!(
            storage
                .get_parent_id("bd-parent-cleanup-child")
                .unwrap()
                .as_deref(),
            Some("bd-parent-cleanup-parent")
        );
        let parent_rows = storage
            .conn
            .query_with_params(
                "SELECT depends_on_id FROM dependencies WHERE issue_id = ? AND type = 'parent-child' ORDER BY depends_on_id",
                &[SqliteValue::from("bd-parent-cleanup-child")],
            )
            .unwrap();
        let parent_ids = parent_rows
            .iter()
            .filter_map(|row| {
                row.get(0)
                    .and_then(SqliteValue::as_text)
                    .map(str::to_string)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            parent_ids,
            vec!["bd-parent-cleanup-parent".to_string()],
            "set_parent must canonicalize duplicate parent-child rows"
        );
        assert_eq!(
            storage.get_dirty_issue_ids().unwrap(),
            vec!["bd-parent-cleanup-child".to_string()]
        );
    }

    #[test]
    fn test_remove_parent_invalidates_all_duplicate_parent_rows() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 6, 0, 0, 0).unwrap();

        let child = make_issue(
            "bd-remove-parent-child",
            "Child",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        let mut parent_a = make_issue(
            "bd-remove-parent-a",
            "Parent A",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        parent_a.issue_type = IssueType::Epic;
        let mut parent_b = make_issue(
            "bd-remove-parent-b",
            "Parent B",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        parent_b.issue_type = IssueType::Epic;

        storage.create_issue(&child, "tester").unwrap();
        storage.create_issue(&parent_a, "tester").unwrap();
        storage.create_issue(&parent_b, "tester").unwrap();
        insert_parent_child_dependency_for_test(
            &storage,
            "bd-remove-parent-child",
            "bd-remove-parent-a",
            t1,
        );
        insert_parent_child_dependency_for_test(
            &storage,
            "bd-remove-parent-child",
            "bd-remove-parent-b",
            t1,
        );
        storage.rebuild_blocked_cache(true).unwrap();
        assert!(storage.is_blocked("bd-remove-parent-a").unwrap());
        assert!(storage.is_blocked("bd-remove-parent-b").unwrap());

        assert!(
            storage
                .remove_parent("bd-remove-parent-child", "tester")
                .unwrap()
        );

        assert!(
            !storage.blocked_cache_marked_stale().unwrap(),
            "remove_parent should eagerly refresh the affected cache entries"
        );
        assert!(
            !storage.is_blocked("bd-remove-parent-a").unwrap(),
            "first old parent must not keep a stale child-open cache row"
        );
        assert!(
            !storage.is_blocked("bd-remove-parent-b").unwrap(),
            "second old parent must not keep a stale child-open cache row"
        );
        assert_eq!(
            storage.get_parent_id("bd-remove-parent-child").unwrap(),
            None
        );
    }

    #[test]
    fn test_set_parent_clear_absent_parent_is_noop() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 4, 0, 0, 0).unwrap();

        let issue = make_issue(
            "bd-parent-clear-noop",
            "Already root",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&issue, "tester").unwrap();
        storage.clear_all_dirty_issues().unwrap();

        let before = storage
            .get_issue("bd-parent-clear-noop")
            .unwrap()
            .expect("issue exists");
        let event_count_before = storage
            .get_events("bd-parent-clear-noop", 100)
            .unwrap()
            .len();

        storage
            .set_parent("bd-parent-clear-noop", None, "tester")
            .unwrap();

        let after = storage
            .get_issue("bd-parent-clear-noop")
            .unwrap()
            .expect("issue exists");
        assert_eq!(after.updated_at, before.updated_at);
        assert_eq!(storage.get_parent_id("bd-parent-clear-noop").unwrap(), None);
        assert_eq!(
            storage
                .get_events("bd-parent-clear-noop", 100)
                .unwrap()
                .len(),
            event_count_before
        );
        assert!(storage.get_dirty_issue_ids().unwrap().is_empty());
    }

    #[test]
    fn test_add_dependency_rejects_missing_target() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 2, 0, 0, 0).unwrap();

        let issue_a = make_issue("bd-a1", "A", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue_a, "tester").unwrap();

        let err = storage
            .add_dependency("bd-a1", "bd-missing", "blocks", "tester")
            .unwrap_err();

        assert!(matches!(err, BeadsError::IssueNotFound { id } if id == "bd-missing"));
    }

    #[test]
    fn test_add_dependency_with_metadata_persists_json() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 2, 0, 0, 0).unwrap();

        let issue_a = make_issue("bd-a1", "A", Status::Open, 2, None, t1, None);
        let issue_b = make_issue("bd-b1", "B", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue_a, "tester").unwrap();
        storage.create_issue(&issue_b, "tester").unwrap();

        storage
            .add_dependency_with_metadata(
                "bd-a1",
                "bd-b1",
                "blocks",
                "tester",
                Some(r#"{"source":"cli","reason":"gate"}"#),
            )
            .unwrap();

        let deps = storage.get_dependencies_full("bd-a1").unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(
            deps[0].metadata.as_deref(),
            Some(r#"{"source":"cli","reason":"gate"}"#)
        );
    }

    #[test]
    fn test_import_dependency_with_legacy_empty_metadata_rebuilds() {
        // Regression for issue #323: legacy JSONL with `"metadata":""` (an
        // empty string that is not valid JSON) must rebuild cleanly through the
        // JSONL deserialize -> SQLite import path, materializing as the empty
        // JSON object `"{}"` with no data loss.
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 2, 0, 0, 0).unwrap();

        let issue_a = make_issue("bd-a1", "A", Status::Open, 2, None, t1, None);
        let issue_b = make_issue("bd-b1", "B", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue_a, "tester").unwrap();
        storage.create_issue(&issue_b, "tester").unwrap();

        // Deserialize exactly as a JSONL rebuild would (not constructing the
        // Dependency directly) so the empty-string coercion is exercised.
        let dep_json = r#"{
            "issue_id": "bd-a1",
            "depends_on_id": "bd-b1",
            "type": "blocks",
            "created_at": "2026-01-01T00:00:00Z",
            "created_by": "import",
            "metadata": "",
            "thread_id": ""
        }"#;
        let dep: crate::model::Dependency =
            serde_json::from_str(dep_json).expect("legacy empty metadata must deserialize");
        assert_eq!(dep.metadata, None, "empty metadata must coerce to None");

        storage
            .sync_dependencies_for_import("bd-a1", &[dep])
            .expect("rebuild with legacy empty metadata must succeed");

        let deps = storage.get_dependencies_full("bd-a1").unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].depends_on_id, "bd-b1");
        // None is materialized as "{}" on insert.
        assert_eq!(deps[0].metadata.as_deref(), Some("{}"));
    }

    #[test]
    fn test_get_dependencies_full_preserves_numeric_created_at() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 2, 0, 0, 0).unwrap();

        let issue_a = make_issue("bd-a1", "A", Status::Open, 2, None, t1, None);
        let issue_b = make_issue("bd-b1", "B", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue_a, "tester").unwrap();
        storage.create_issue(&issue_b, "tester").unwrap();
        storage
            .add_dependency("bd-a1", "bd-b1", "blocks", "tester")
            .unwrap();
        storage
            .conn
            .execute_with_params(
                "UPDATE dependencies SET created_at = ? WHERE issue_id = ? AND depends_on_id = ?",
                &[
                    SqliteValue::Integer(1_776_651_488_000_000),
                    SqliteValue::from("bd-a1"),
                    SqliteValue::from("bd-b1"),
                ],
            )
            .unwrap();

        let deps = storage.get_dependencies_full("bd-a1").unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].created_at.year(), 2026);
        assert_eq!(deps[0].created_at.month(), 4);
        assert_eq!(deps[0].created_at.day(), 20);
        assert_eq!(deps[0].created_at.hour(), 2);
        assert_eq!(deps[0].created_at.minute(), 18);

        let by_issue = storage
            .get_dependencies_full_for_issues(&["bd-a1".to_string()])
            .unwrap();
        assert_eq!(by_issue["bd-a1"][0].created_at, deps[0].created_at);
    }

    #[test]
    fn test_add_dependency_with_metadata_rejects_invalid_json() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 2, 0, 0, 0).unwrap();

        let issue_a = make_issue("bd-a1", "A", Status::Open, 2, None, t1, None);
        let issue_b = make_issue("bd-b1", "B", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue_a, "tester").unwrap();
        storage.create_issue(&issue_b, "tester").unwrap();

        let err = storage
            .add_dependency_with_metadata("bd-a1", "bd-b1", "blocks", "tester", Some("{not-json"))
            .unwrap_err();

        assert!(matches!(err, BeadsError::Validation { field, .. } if field == "metadata"));
    }

    #[test]
    fn test_find_ids_by_hash_only_matches_hash_portion() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 2, 0, 0, 0).unwrap();

        let issue_a = make_issue("my-proj-abc123", "Alpha", Status::Open, 2, None, t1, None);
        let issue_b = make_issue("other-proj-xyz789", "Beta", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue_a, "tester").unwrap();
        storage.create_issue(&issue_b, "tester").unwrap();

        let matches = storage.find_ids_by_hash("proj").unwrap();
        assert!(
            matches.is_empty(),
            "prefix fragments must not match hash lookup"
        );
        assert_eq!(
            storage.find_ids_by_hash("abc").unwrap(),
            vec!["my-proj-abc123".to_string()]
        );
    }

    #[test]
    fn test_get_dependencies_with_metadata_external_placeholder() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 2, 0, 0, 0).unwrap();

        let issue_a = make_issue("bd-a1", "A", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue_a, "tester").unwrap();
        storage
            .add_dependency("bd-a1", "external:proj:capability", "blocks", "tester")
            .unwrap();

        let deps = storage.get_dependencies_with_metadata("bd-a1").unwrap();
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].id, "external:proj:capability");
        assert_eq!(deps[0].title, "proj:capability");
        assert_eq!(deps[0].status, Status::Blocked);
        assert_eq!(deps[0].priority, Priority::MEDIUM);
        assert_eq!(deps[0].dep_type, "blocks");
    }

    #[test]
    fn test_get_dependencies_with_metadata_sorts_external_placeholder_as_medium_priority() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 2, 0, 0, 0).unwrap();

        let issue_a = make_issue("bd-a1", "A", Status::Open, 2, None, t1, None);
        let critical = make_issue("bd-critical", "Critical", Status::Open, 0, None, t1, None);
        let low = make_issue("bd-low", "Low", Status::Open, 3, None, t1, None);
        storage.create_issue(&issue_a, "tester").unwrap();
        storage.create_issue(&critical, "tester").unwrap();
        storage.create_issue(&low, "tester").unwrap();

        storage
            .add_dependency("bd-a1", "external:proj:capability", "blocks", "tester")
            .unwrap();
        storage
            .add_dependency("bd-a1", "bd-low", "blocks", "tester")
            .unwrap();
        storage
            .add_dependency("bd-a1", "bd-critical", "blocks", "tester")
            .unwrap();

        let deps = storage.get_dependencies_with_metadata("bd-a1").unwrap();
        let ids: Vec<_> = deps.iter().map(|dep| dep.id.as_str()).collect();

        assert_eq!(
            ids,
            vec!["bd-critical", "external:proj:capability", "bd-low"]
        );
        assert_eq!(deps[1].priority, Priority::MEDIUM);
    }

    #[test]
    fn test_get_dependencies_with_metadata_errors_on_missing_internal_target() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 2, 0, 0, 0).unwrap();

        let issue_a = make_issue("bd-a1", "A", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue_a, "tester").unwrap();

        let created_at = Utc::now().to_rfc3339();
        storage
            .execute_test_sql(&format!(
                "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by)
                 VALUES ('bd-a1', 'bd-missing', 'blocks', '{created_at}', 'tester')"
            ))
            .unwrap();

        let deps = storage
            .get_dependencies_with_metadata("bd-a1")
            .expect("should return placeholder for missing dependency");

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].id, "bd-missing");
        assert_eq!(deps[0].title, "[missing issue: bd-missing]");
        assert_eq!(deps[0].status, Status::Tombstone);
    }

    #[test]
    fn test_get_dependents_with_metadata_errors_on_missing_dependent_issue() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 2, 0, 0, 0).unwrap();

        let issue_b = make_issue("bd-b1", "B", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue_b, "tester").unwrap();

        let created_at = Utc::now().to_rfc3339();
        storage
            .execute_test_sql(&format!(
                "PRAGMA foreign_keys = OFF;
                 INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by)
                 VALUES ('bd-missing', 'bd-b1', 'blocks', '{created_at}', 'tester');
                 PRAGMA foreign_keys = ON;"
            ))
            .unwrap();

        let deps = storage
            .get_dependents_with_metadata("bd-b1")
            .expect("should return placeholder for missing dependent");

        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].id, "bd-missing");
        assert_eq!(deps[0].title, "[missing issue: bd-missing]");
        assert_eq!(deps[0].status, Status::Tombstone);
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
    fn test_detect_blocking_cycles_ignores_related_edges() -> Result<()> {
        let mut storage = SqliteStorage::open_memory()?;
        let t1 = Utc
            .with_ymd_and_hms(2025, 7, 3, 0, 0, 0)
            .single()
            .ok_or_else(|| BeadsError::internal("invalid test timestamp"))?;

        let issue_a = make_issue("bd-rel-cy1", "A", Status::Open, 2, None, t1, None);
        let issue_b = make_issue("bd-rel-cy2", "B", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue_a, "tester")?;
        storage.create_issue(&issue_b, "tester")?;

        storage.add_dependency("bd-rel-cy1", "bd-rel-cy2", "related", "tester")?;
        storage.add_dependency("bd-rel-cy2", "bd-rel-cy1", "related", "tester")?;

        assert!(!storage.detect_all_cycles()?.is_empty());
        assert!(storage.detect_blocking_cycles()?.is_empty());
        Ok(())
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
            source_repo_path: None,
            agent_context: None,
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
            .execute_with_params(
                "INSERT INTO comments (issue_id, author, text, created_at) VALUES (?, ?, ?, ?)",
                &[
                    SqliteValue::from("bd-c1"),
                    SqliteValue::from("alice"),
                    SqliteValue::from("first"),
                    SqliteValue::from("2025-07-01T00:00:00Z"),
                ],
            )
            .unwrap();
        storage
            .conn
            .execute_with_params(
                "INSERT INTO comments (issue_id, author, text, created_at) VALUES (?, ?, ?, ?)",
                &[
                    SqliteValue::from("bd-c1"),
                    SqliteValue::from("bob"),
                    SqliteValue::from("second"),
                    SqliteValue::from("2025-07-02T00:00:00Z"),
                ],
            )
            .unwrap();

        let comments = storage.get_comments("bd-c1").unwrap();
        assert_eq!(comments.len(), 2);
        assert_eq!(comments[0].author, "alice");
        assert_eq!(comments[1].author, "bob");
    }

    #[test]
    fn test_get_comments_errors_on_invalid_timestamp() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 4, 0, 0, 0).unwrap();

        let issue = Issue {
            id: "bd-c-invalid".to_string(),
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
            source_repo_path: None,
            agent_context: None,
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
            .execute_with_params(
                "INSERT INTO comments (issue_id, author, text, created_at) VALUES (?, ?, ?, ?)",
                &[
                    SqliteValue::from("bd-c-invalid"),
                    SqliteValue::from("alice"),
                    SqliteValue::from("first"),
                    SqliteValue::from("not-a-real-timestamp"),
                ],
            )
            .unwrap();

        let err = storage.get_comments("bd-c-invalid").unwrap_err();
        assert!(
            matches!(
                &err,
                BeadsError::Config(msg)
                    if msg.contains("invalid comment timestamp")
                        && msg.contains("unparseable datetime")
            ),
            "unexpected error: {err:?}"
        );
    }

    #[test]
    fn test_get_comments_preserves_numeric_created_at() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 4, 0, 0, 0).unwrap();

        let issue = Issue {
            id: "bd-c-numeric-time".to_string(),
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
            source_repo_path: None,
            agent_context: None,
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
            .execute_with_params(
                "INSERT INTO comments (issue_id, author, text, created_at) VALUES (?, ?, ?, ?)",
                &[
                    SqliteValue::from("bd-c-numeric-time"),
                    SqliteValue::from("alice"),
                    SqliteValue::from("fractional"),
                    SqliteValue::Float(1_776_651_488.25),
                ],
            )
            .unwrap();

        let comments = storage.get_comments("bd-c-numeric-time").unwrap();
        assert_eq!(comments.len(), 1);
        assert_eq!(comments[0].created_at.year(), 2026);
        assert_eq!(comments[0].created_at.timestamp(), 1_776_651_488);
        assert_eq!(comments[0].created_at.timestamp_subsec_nanos(), 250_000_000);

        let by_issue = storage
            .get_comments_for_issues(&["bd-c-numeric-time".to_string()])
            .unwrap();
        assert_eq!(
            by_issue["bd-c-numeric-time"][0].created_at,
            comments[0].created_at
        );
    }

    #[test]
    fn test_get_latest_comments_for_issues_bounds_each_issue() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 4, 0, 0, 0).unwrap();
        let issue_a = make_issue("bd-c-latest-a", "A", Status::Open, 2, None, t1, None);
        let issue_b = make_issue("bd-c-latest-b", "B", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue_a, "tester").unwrap();
        storage.create_issue(&issue_b, "tester").unwrap();

        for (issue_id, body, created_at) in [
            ("bd-c-latest-a", "a-old", "2025-07-01T00:00:00Z"),
            ("bd-c-latest-a", "a-middle", "2025-07-02T00:00:00Z"),
            ("bd-c-latest-a", "a-new", "2025-07-03T00:00:00Z"),
            ("bd-c-latest-b", "b-old", "2025-07-01T00:00:00Z"),
            ("bd-c-latest-b", "b-new", "2025-07-02T00:00:00Z"),
        ] {
            storage
                .conn
                .execute_with_params(
                    "INSERT INTO comments (issue_id, author, text, created_at) VALUES (?, ?, ?, ?)",
                    &[
                        SqliteValue::from(issue_id),
                        SqliteValue::from("tester"),
                        SqliteValue::from(body),
                        SqliteValue::from(created_at),
                    ],
                )
                .unwrap();
        }

        let by_issue = storage
            .get_latest_comments_for_issues(
                &["bd-c-latest-a".to_string(), "bd-c-latest-b".to_string()],
                2,
            )
            .unwrap();

        let issue_a_bodies = by_issue["bd-c-latest-a"]
            .iter()
            .map(|comment| comment.body.as_str())
            .collect::<Vec<_>>();
        let issue_b_bodies = by_issue["bd-c-latest-b"]
            .iter()
            .map(|comment| comment.body.as_str())
            .collect::<Vec<_>>();

        assert_eq!(issue_a_bodies, ["a-middle", "a-new"]);
        assert_eq!(issue_b_bodies, ["b-old", "b-new"]);

        let empty = storage
            .get_latest_comments_for_issues(&["bd-c-latest-a".to_string()], 0)
            .unwrap();
        assert!(empty.is_empty());
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
            source_repo_path: None,
            agent_context: None,
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
    fn test_add_comment_rejects_invalid_comment_fields_without_inserting() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 4, 0, 0, 0).unwrap();
        let issue = make_issue(
            "bd-c-invalid-input",
            "Comment validation target",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&issue, "tester").unwrap();

        // Long-content comments are explicitly permitted — spec write-ups,
        // session transcripts, etc. routinely exceed 50KB. The prior cap
        // rejected legitimate pre-existing JSONL records on rebuild.
        let long_text = "x".repeat(600_000);
        storage
            .add_comment("bd-c-invalid-input", "alice", &long_text)
            .expect("long comment bodies must be accepted");

        let author_error = storage
            .add_comment("bd-c-invalid-input", "", "Valid comment body")
            .unwrap_err();
        assert!(
            matches!(
                &author_error,
                BeadsError::Validation { field, reason }
                    if field == "author" && reason.contains("cannot be empty")
            ),
            "unexpected empty author error: {author_error:?}"
        );

        let comments = storage.get_comments("bd-c-invalid-input").unwrap();
        assert_eq!(
            comments.len(),
            1,
            "the long-body insert above must persist; only the empty-author insert is rejected: {comments:?}"
        );
        assert_eq!(
            comments[0].body.len(),
            600_000,
            "long comment body preserved verbatim"
        );
    }

    #[test]
    fn test_create_issue_rejects_invalid_embedded_comment_without_inserting_issue() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 4, 0, 0, 0).unwrap();
        let mut issue = make_issue(
            "bd-c-create-invalid",
            "Create comment validation target",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        issue.comments.push(Comment {
            id: 1,
            issue_id: "bd-c-create-invalid".to_string(),
            author: "alice".to_string(),
            body: "   ".to_string(),
            created_at: t1,
        });

        let err = storage.create_issue(&issue, "tester").unwrap_err();
        assert!(
            matches!(
                &err,
                BeadsError::Validation { field, reason }
                    if field == "content" && reason.contains("cannot be empty")
            ),
            "unexpected embedded comment validation error: {err:?}"
        );
        assert!(storage.get_issue("bd-c-create-invalid").unwrap().is_none());
        assert!(
            storage
                .get_comments("bd-c-create-invalid")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn test_add_comment_rejects_tombstone_issue() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 4, 0, 0, 0).unwrap();

        let issue = make_issue(
            "bd-c-tomb",
            "Deleted comment target",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&issue, "tester").unwrap();
        let original_comment = storage
            .add_comment("bd-c-tomb", "alice", "Before delete")
            .unwrap();
        storage
            .delete_issue("bd-c-tomb", "tester", "delete comment target", None)
            .unwrap();

        let error = storage
            .add_comment("bd-c-tomb", "bob", "After delete")
            .unwrap_err();
        assert!(
            matches!(
                &error,
                BeadsError::Validation { field, reason }
                    if field == "issue_id"
                        && reason.contains("cannot add comment to tombstone issue: bd-c-tomb")
            ),
            "unexpected add_comment error: {error:?}"
        );

        let comments = storage.get_comments("bd-c-tomb").unwrap();
        assert_eq!(comments, vec![original_comment]);
    }

    #[test]
    fn test_sync_comments_for_import_preserves_comments_on_other_issues() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 4, 0, 0, 0).unwrap();

        let issue_a = make_issue(
            "bd-c-import-a",
            "Import target",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        let issue_b = make_issue(
            "bd-c-import-b",
            "Existing comment owner",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&issue_a, "tester").unwrap();
        storage.create_issue(&issue_b, "tester").unwrap();

        let existing_comment = storage
            .add_comment("bd-c-import-b", "bob", "Existing comment")
            .unwrap();

        let imported_comment = crate::model::Comment {
            id: existing_comment.id,
            issue_id: "bd-c-import-a".to_string(),
            author: "alice".to_string(),
            body: "Imported comment".to_string(),
            created_at: t1 + chrono::Duration::minutes(5),
        };
        storage
            .sync_comments_for_import("bd-c-import-a", &[imported_comment])
            .unwrap();

        let comments_a = storage.get_comments("bd-c-import-a").unwrap();
        assert_eq!(comments_a.len(), 1);
        assert_eq!(comments_a[0].issue_id, "bd-c-import-a");
        assert_eq!(comments_a[0].body, "Imported comment");
        assert_ne!(comments_a[0].id, existing_comment.id);

        let comments_b = storage.get_comments("bd-c-import-b").unwrap();
        assert_eq!(comments_b, vec![existing_comment]);
    }

    #[test]
    fn test_sync_comments_for_import_rejects_duplicate_comment_ids_for_same_issue() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 4, 0, 0, 0).unwrap();

        let issue = make_issue(
            "bd-c-import-dup",
            "Duplicate import comments",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&issue, "tester").unwrap();
        let existing_comment = storage
            .add_comment(&issue.id, "bob", "existing local comment")
            .unwrap();

        let first = crate::model::Comment {
            id: 42,
            issue_id: issue.id.clone(),
            author: "alice".to_string(),
            body: "first imported comment".to_string(),
            created_at: t1,
        };
        let second = crate::model::Comment {
            id: 42,
            issue_id: issue.id.clone(),
            author: "alice".to_string(),
            body: "duplicate imported comment".to_string(),
            created_at: t1 + chrono::Duration::minutes(1),
        };

        let error = storage
            .sync_comments_for_import(&issue.id, &[first, second])
            .unwrap_err();
        assert!(
            error.to_string().contains("duplicate import comment id 42"),
            "duplicate same-issue import comment IDs must remain invalid: {error:?}"
        );
        assert_eq!(
            storage.get_comments(&issue.id).unwrap(),
            vec![existing_comment.clone()]
        );

        let wrong_issue_comment = crate::model::Comment {
            id: 44,
            issue_id: "bd-c-import-other".to_string(),
            author: "alice".to_string(),
            body: "valid text, wrong owner".to_string(),
            created_at: t1,
        };
        let error = storage
            .sync_comments_for_import(&issue.id, &[wrong_issue_comment])
            .unwrap_err();
        assert!(
            error.to_string().contains("comment.issue_id"),
            "wrong comment owner must fail validation: {error:?}"
        );
        assert_eq!(
            storage.get_comments(&issue.id).unwrap(),
            vec![existing_comment]
        );
    }

    #[test]
    fn test_sync_comments_for_import_handles_auto_realloc_self_collision() {
        // Issue #374: during a single import, an earlier comment whose JSONL id
        // collides with a comment on *another* issue is AUTO-reallocated a fresh
        // id via AUTOINCREMENT. If that freshly assigned id happens to equal a
        // *later* comment's JSONL id in the SAME import, the later insert hits a
        // PK collision whose owner is this very issue. The old narrow guard
        // (`owner != issue_id`) turned that self-collision into a hard failure,
        // corrupting rebuild-from-JSONL. It must instead reallocate again.
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 4, 0, 0, 0).unwrap();

        let issue_a = make_issue(
            "bd-c-realloc-a",
            "Import target",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        let issue_b = make_issue(
            "bd-c-realloc-b",
            "Existing comment owner",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&issue_a, "tester").unwrap();
        storage.create_issue(&issue_b, "tester").unwrap();

        // Existing comment on issue B occupies id E and makes E the current
        // AUTOINCREMENT high-water mark, so the next auto id is E + 1.
        let existing = storage
            .add_comment("bd-c-realloc-b", "bob", "Existing comment on B")
            .unwrap();
        let e = existing.id;

        // First imported comment collides with B's id (E) -> auto-realloc to
        // E + 1. Second imported comment's JSONL id is E + 1, which now collides
        // with the just-reallocated first comment -> a same-issue self-collision.
        let first = crate::model::Comment {
            id: e,
            issue_id: "bd-c-realloc-a".to_string(),
            author: "alice".to_string(),
            body: "first imported (collides with B)".to_string(),
            created_at: t1 + chrono::Duration::minutes(1),
        };
        let second = crate::model::Comment {
            id: e + 1,
            issue_id: "bd-c-realloc-a".to_string(),
            author: "alice".to_string(),
            body: "second imported (self-collision)".to_string(),
            created_at: t1 + chrono::Duration::minutes(2),
        };

        storage
            .sync_comments_for_import("bd-c-realloc-a", &[first, second])
            .expect("auto-realloc self-collision must not fail the import");

        let mut comments_a = storage.get_comments("bd-c-realloc-a").unwrap();
        comments_a.sort_by_key(|c| c.created_at);
        assert_eq!(comments_a.len(), 2, "both imported comments must be kept");
        assert_eq!(comments_a[0].body, "first imported (collides with B)");
        assert_eq!(comments_a[1].body, "second imported (self-collision)");
        // Reallocated ids must be distinct and must not clobber B's comment.
        assert_ne!(comments_a[0].id, comments_a[1].id);
        assert_ne!(comments_a[0].id, e);
        assert_ne!(comments_a[1].id, e);

        // Issue B's original comment is untouched.
        assert_eq!(
            storage.get_comments("bd-c-realloc-b").unwrap(),
            vec![existing]
        );
    }

    #[test]
    fn connection_user_version_sees_wal_resident_value_header_misses() {
        // Issue #373: a user_version written through a connection lives in the
        // uncheckpointed WAL, so a raw file-header peek reads a stale value
        // while the connection observes the true one. connection_user_version
        // must report the WAL-resident value; database_header_user_version must
        // not — proving the exact scenario the open-path fix guards against.
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("wal_uv.db");

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        conn.execute("PRAGMA journal_mode=WAL").unwrap();
        conn.execute("CREATE TABLE t (x INTEGER)").unwrap();
        conn.execute("PRAGMA user_version = 4242").unwrap();

        // Through the connection: WAL-aware, sees the new value.
        assert_eq!(connection_user_version(&conn), Some(4242));

        // Raw header peek of the main db file misses the uncheckpointed value.
        assert_ne!(
            database_header_user_version(&db_path),
            Some(4242),
            "header peek unexpectedly reflected the WAL-resident user_version; \
             the WAL-miss scenario cannot be proven"
        );
    }

    #[test]
    fn test_sync_comments_for_import_validates_before_replacing_existing_comments() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 4, 0, 0, 0).unwrap();

        let issue = make_issue(
            "bd-c-import-invalid",
            "Invalid import comments",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&issue, "tester").unwrap();
        let existing_comment = storage
            .add_comment(&issue.id, "bob", "existing local comment")
            .unwrap();

        let invalid_comment = crate::model::Comment {
            id: 43,
            issue_id: issue.id.clone(),
            author: "alice".to_string(),
            body: "   ".to_string(),
            created_at: t1,
        };

        let error = storage
            .sync_comments_for_import(&issue.id, &[invalid_comment])
            .unwrap_err();
        assert!(
            error.to_string().contains("cannot be empty"),
            "invalid import comment body must fail validation: {error:?}"
        );
        assert_eq!(
            storage.get_comments(&issue.id).unwrap(),
            vec![existing_comment]
        );
    }

    #[test]
    fn test_insert_new_issue_relations_for_import_skips_relation_deletes() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 4, 0, 0, 0).unwrap();

        let parent = make_issue("bd-parent", "Parent", Status::Open, 2, None, t1, None);
        let other = make_issue("bd-other", "Other", Status::Open, 2, None, t1, None);
        let mut imported = make_issue("bd-new", "New import", Status::Open, 2, None, t1, None);
        storage.create_issue(&parent, "tester").unwrap();
        storage.create_issue(&other, "tester").unwrap();
        storage.insert_new_issue_for_import(&imported).unwrap();

        let existing_comment = storage
            .add_comment("bd-other", "bob", "Existing owner")
            .unwrap();
        imported.labels = vec!["sync".to_string(), "perf".to_string(), "sync".to_string()];
        imported.dependencies = vec![crate::model::Dependency {
            issue_id: imported.id.clone(),
            depends_on_id: parent.id.clone(),
            dep_type: crate::model::DependencyType::Blocks,
            created_at: t1,
            created_by: Some("import".to_string()),
            metadata: None,
            thread_id: None,
        }];
        imported.comments = vec![crate::model::Comment {
            id: existing_comment.id,
            issue_id: imported.id.clone(),
            author: "alice".to_string(),
            body: "Imported comment".to_string(),
            created_at: t1 + chrono::Duration::minutes(5),
        }];

        storage
            .insert_new_issue_relations_for_import(&imported)
            .unwrap();

        assert_eq!(
            storage.get_labels(&imported.id).unwrap(),
            vec!["perf", "sync"]
        );
        let dependency_count = storage
            .execute_raw_query("SELECT COUNT(*) FROM dependencies WHERE issue_id = 'bd-new'")
            .unwrap()
            .first()
            .and_then(|row| row.first())
            .and_then(SqliteValue::as_integer)
            .unwrap_or(-1);
        assert_eq!(dependency_count, 1);

        let imported_comments = storage.get_comments(&imported.id).unwrap();
        assert_eq!(imported_comments.len(), 1);
        assert_eq!(imported_comments[0].body, "Imported comment");
        assert_ne!(imported_comments[0].id, existing_comment.id);

        assert_eq!(
            storage.get_comments(&other.id).unwrap(),
            vec![existing_comment]
        );
    }

    #[test]
    fn test_has_owned_relation_rows_for_import_detects_orphan_rows() {
        let storage = SqliteStorage::open_memory().unwrap();

        assert!(
            !storage
                .has_owned_relation_rows_for_import("bd-stale")
                .unwrap()
        );

        storage
            .conn
            .execute(
                "PRAGMA foreign_keys = OFF;
                 INSERT INTO labels (issue_id, label) VALUES ('bd-stale', 'old-label');",
            )
            .unwrap();

        assert!(
            storage
                .has_owned_relation_rows_for_import("bd-stale")
                .unwrap()
        );
    }

    #[test]
    fn test_external_project_capabilities_ignore_tombstones() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("external.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 7, 4, 0, 0, 0).unwrap();

        let mut closed_issue = make_issue(
            "bd-cap-closed",
            "Closed provider",
            Status::Closed,
            2,
            None,
            t1,
            None,
        );
        closed_issue.closed_at = Some(t1);
        let mut tombstone_issue = make_issue(
            "bd-cap-tombstone",
            "Deleted provider",
            Status::Tombstone,
            2,
            None,
            t1,
            None,
        );
        tombstone_issue.deleted_at = Some(t1);
        tombstone_issue.delete_reason = Some("deleted".to_string());

        storage.create_issue(&closed_issue, "tester").unwrap();
        storage.create_issue(&tombstone_issue, "tester").unwrap();
        storage
            .add_label("bd-cap-closed", "provides:closed-cap", "tester")
            .unwrap();
        storage
            .conn
            .execute_with_params(
                "INSERT INTO labels (issue_id, label) VALUES (?, ?)",
                &[
                    SqliteValue::from("bd-cap-tombstone"),
                    SqliteValue::from("provides:deleted-cap"),
                ],
            )
            .unwrap();
        drop(storage);

        let capabilities = HashSet::from(["closed-cap".to_string(), "deleted-cap".to_string()]);
        let satisfied = query_external_project_capabilities(&db_path, &capabilities).unwrap();

        assert!(satisfied.contains("closed-cap"));
        assert!(!satisfied.contains("deleted-cap"));
    }

    #[test]
    fn test_external_project_capability_probe_does_not_create_missing_database() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("missing-external.db");
        let capabilities = HashSet::from(["capability".to_string()]);

        let error = query_external_project_capabilities(&db_path, &capabilities)
            .expect_err("missing external database should be an unsatisfied dependency");

        assert!(
            error
                .to_string()
                .contains("external project database not found"),
            "{error}"
        );
        assert!(
            !db_path.exists(),
            "read-only external dependency probe must not create missing DB files"
        );
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
            source_repo_path: None,
            agent_context: None,
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

        let dirty_count = storage
            .conn
            .query_row_with_params(
                "SELECT count(*) FROM dirty_issues WHERE issue_id = ?",
                &[SqliteValue::from("bd-c3")],
            )
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap_or(0);
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
            .query_row_with_params(
                "SELECT created_at FROM events WHERE issue_id = ?",
                &[SqliteValue::from("bd-e1")],
            )
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_text)
            .unwrap_or("")
            .to_string();

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
            .execute_with_params(
                "INSERT INTO blocked_issues_cache (issue_id, blocked_by) VALUES (?, ?)",
                &[
                    SqliteValue::from("bd-c1"),
                    SqliteValue::from(r#"["bd-b1"]"#),
                ],
            )
            .unwrap();

        // Verify cache has data
        let count = storage
            .conn
            .query_row_with_params(
                "SELECT count(*) FROM blocked_issues_cache WHERE issue_id = ?",
                &[SqliteValue::from("bd-c1")],
            )
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap_or(0);
        assert_eq!(count, 1);

        // Now add a non-blocking dependency type ("related" doesn't block)
        storage
            .add_dependency("bd-c1", "bd-b1", "related", "tester")
            .unwrap();

        // With deferred cache refresh, add_dependency now marks the cache stale
        // rather than rebuilding immediately.  Verify the stale marker was set.
        assert!(
            storage.blocked_cache_marked_stale().unwrap(),
            "cache should be marked stale after add_dependency"
        );

        // Read operations compute blocked state in memory when the cache is
        // stale, WITHOUT persisting (#216 — read ops must not write).
        let blocked = storage.is_blocked("bd-c1").unwrap();
        assert!(
            !blocked,
            "bd-c1 should not be blocked after adding only a 'related' dep"
        );

        // The stale marker should remain — read ops do not clear it.
        assert!(
            storage.blocked_cache_marked_stale().unwrap(),
            "cache should still be stale after read (reads must not write)"
        );

        // The stale cache entry should still be in the table (not cleaned by reads).
        let count = storage
            .conn
            .query_row_with_params(
                "SELECT count(*) FROM blocked_issues_cache WHERE issue_id = ?",
                &[SqliteValue::from("bd-c1")],
            )
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap_or(0);
        assert_eq!(
            count, 1,
            "stale cache entry should persist (reads must not mutate cache)"
        );
    }

    #[test]
    fn test_update_issue_skip_cache_rebuild_marks_cache_stale_reads_compute_in_memory() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let blocker = make_issue(
            "bd-blocker",
            "Blocker",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        let blocked = make_issue(
            "bd-blocked",
            "Blocked",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        storage.create_issue(&blocker, "tester").unwrap();
        storage.create_issue(&blocked, "tester").unwrap();
        storage
            .add_dependency(
                &blocked.id,
                &blocker.id,
                DependencyType::Blocks.as_str(),
                "tester",
            )
            .unwrap();

        // Read operations compute blocked state in memory when the cache is
        // stale, WITHOUT persisting (#216 — read ops must not write).
        assert!(storage.is_blocked(&blocked.id).unwrap());

        let close_update = IssueUpdate {
            status: Some(Status::Closed),
            skip_cache_rebuild: true,
            ..IssueUpdate::default()
        };
        storage
            .update_issue(&blocker.id, &close_update, "tester")
            .unwrap();

        assert!(
            storage.blocked_cache_marked_stale().unwrap(),
            "status updates with skip_cache_rebuild should leave a stale marker behind"
        );
        assert!(
            !storage.is_blocked(&blocked.id).unwrap(),
            "in-memory blocked computation should see the blocker is now closed"
        );
        // The stale marker should remain — read ops do not clear it.
        assert!(
            storage.blocked_cache_marked_stale().unwrap(),
            "cache should still be stale after read (reads must not write)"
        );
    }

    #[test]
    fn test_set_parent_skip_cache_rebuild_marks_cache_stale_reads_compute_in_memory() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let blocker = make_issue(
            "bd-parent-blocker",
            "Parent blocker",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        let parent = make_issue(
            "bd-parent",
            "Parent",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        let child = make_issue("bd-child", "Child", Status::Open, 2, None, Utc::now(), None);
        storage.create_issue(&blocker, "tester").unwrap();
        storage.create_issue(&parent, "tester").unwrap();
        storage.create_issue(&child, "tester").unwrap();
        storage
            .add_dependency(
                &parent.id,
                &blocker.id,
                DependencyType::Blocks.as_str(),
                "tester",
            )
            .unwrap();

        // Read operations compute blocked state in memory when the cache is
        // stale, WITHOUT persisting (#216 — read ops must not write).
        assert!(storage.is_blocked(&parent.id).unwrap());

        storage
            .set_parent_with_options(&child.id, Some(&parent.id), "tester", true)
            .unwrap();

        assert!(
            storage.blocked_cache_marked_stale().unwrap(),
            "parent changes with skip_cache_rebuild should leave a stale marker behind"
        );
        assert!(
            storage.is_blocked(&child.id).unwrap(),
            "in-memory blocked computation should propagate parent blockers to the child"
        );
        // The stale marker should remain — read ops do not clear it.
        assert!(
            storage.blocked_cache_marked_stale().unwrap(),
            "cache should still be stale after read (reads must not write)"
        );
    }

    #[test]
    fn test_expand_blocked_cache_component_includes_parent_and_siblings() {
        let children_by_parent = HashMap::from([
            (
                "bd-root".to_string(),
                vec!["bd-parent".to_string(), "bd-aunt".to_string()],
            ),
            (
                "bd-parent".to_string(),
                vec!["bd-parent.1".to_string(), "bd-parent.2".to_string()],
            ),
        ]);
        let parents_by_child = SqliteStorage::build_parents_by_child(&children_by_parent);
        let seed_ids = HashSet::from(["bd-parent.1".to_string()]);

        let affected = SqliteStorage::expand_blocked_cache_component(
            &seed_ids,
            &children_by_parent,
            &parents_by_child,
        );

        assert!(affected.contains("bd-parent.1"));
        assert!(affected.contains("bd-parent"));
        assert!(affected.contains("bd-parent.2"));
        assert!(affected.contains("bd-root"));
        assert!(affected.contains("bd-aunt"));
    }

    #[test]
    fn test_incremental_blocked_cache_update_recomputes_entire_parent_child_component() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let now = Utc::now();

        // `bd-parent` is an Epic so that the "parent blocked by open
        // children" rollup applies: the incremental update is specifically
        // exercising the epic-child aggregation path, where removing a
        // direct blocker on the epic should not unblock the epic as long
        // as its children remain open.  Non-epic parents intentionally do
        // not carry that blocker (see `parent_child_transitive_blocking`
        // in `tests/storage_blocked_cache.rs`), so the test would be
        // testing semantics that no longer apply to plain tasks.
        let mut parent = make_issue("bd-parent", "Parent", Status::Open, 2, None, now, None);
        parent.issue_type = IssueType::Epic;
        for issue in [
            parent,
            make_issue("bd-parent.1", "Child 1", Status::Open, 2, None, now, None),
            make_issue("bd-parent.2", "Child 2", Status::Open, 2, None, now, None),
            make_issue("bd-blocker", "Blocker", Status::Open, 2, None, now, None),
            make_issue(
                "bd-unrelated",
                "Unrelated",
                Status::Open,
                2,
                None,
                now,
                None,
            ),
            make_issue(
                "bd-unrelated-blocker",
                "Unrelated blocker",
                Status::Open,
                2,
                None,
                now,
                None,
            ),
        ] {
            storage.create_issue(&issue, "tester").unwrap();
        }

        storage
            .add_dependency("bd-parent.1", "bd-parent", "parent-child", "tester")
            .unwrap();
        storage
            .add_dependency("bd-parent.2", "bd-parent", "parent-child", "tester")
            .unwrap();
        storage
            .add_dependency("bd-parent", "bd-blocker", "blocks", "tester")
            .unwrap();
        storage
            .add_dependency("bd-unrelated", "bd-unrelated-blocker", "blocks", "tester")
            .unwrap();

        assert!(storage.is_blocked("bd-parent").unwrap());
        assert!(storage.is_blocked("bd-parent.1").unwrap());
        assert!(storage.is_blocked("bd-parent.2").unwrap());
        assert!(storage.is_blocked("bd-unrelated").unwrap());

        storage
            .conn
            .execute_with_params(
                "DELETE FROM dependencies WHERE issue_id = ? AND depends_on_id = ?",
                &[
                    SqliteValue::from("bd-parent"),
                    SqliteValue::from("bd-blocker"),
                ],
            )
            .unwrap();

        let seed_ids = HashSet::from(["bd-parent.1".to_string()]);
        SqliteStorage::incremental_blocked_cache_update(&storage.conn, &seed_ids).unwrap();

        let parent_blockers = storage.get_blockers("bd-parent").unwrap();
        assert_eq!(
            parent_blockers,
            vec!["bd-parent.1".to_string(), "bd-parent.2".to_string()]
        );
        assert!(storage.get_blockers("bd-parent.1").unwrap().is_empty());
        assert!(storage.get_blockers("bd-parent.2").unwrap().is_empty());
        assert_eq!(
            storage.get_blockers("bd-unrelated").unwrap(),
            vec!["bd-unrelated-blocker".to_string()]
        );
    }

    #[test]
    fn test_get_start_blockers_ignores_child_open_but_keeps_real_blockers() {
        // #315: an epic that is "blocked" only by its own still-open children
        // must remain claimable / startable (the child-open rollup is a
        // close-ordering constraint, not a real dependency). A genuine `blocks`
        // edge must still prevent starting, and the child-open marker must be
        // filtered out of the reported start-blockers.
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let now = Utc::now();

        let mut epic = make_issue("bd-epic", "Epic", Status::Open, 2, None, now, None);
        epic.issue_type = IssueType::Epic;
        let mut blocked_epic = make_issue(
            "bd-epic-blocked",
            "Blocked epic",
            Status::Open,
            2,
            None,
            now,
            None,
        );
        blocked_epic.issue_type = IssueType::Epic;
        for issue in [
            epic,
            make_issue("bd-epic.1", "Child", Status::Open, 2, None, now, None),
            blocked_epic,
            make_issue(
                "bd-epic-blocked.1",
                "Child of blocked epic",
                Status::Open,
                2,
                None,
                now,
                None,
            ),
            make_issue(
                "bd-real-blocker",
                "Real blocker",
                Status::Open,
                2,
                None,
                now,
                None,
            ),
        ] {
            storage.create_issue(&issue, "tester").unwrap();
        }

        storage
            .add_dependency("bd-epic.1", "bd-epic", "parent-child", "tester")
            .unwrap();
        storage
            .add_dependency(
                "bd-epic-blocked.1",
                "bd-epic-blocked",
                "parent-child",
                "tester",
            )
            .unwrap();
        storage
            .add_dependency("bd-epic-blocked", "bd-real-blocker", "blocks", "tester")
            .unwrap();

        // Both epics are "blocked" in the close-ordering sense (open children).
        assert!(storage.is_blocked("bd-epic").unwrap());
        assert!(storage.is_blocked("bd-epic-blocked").unwrap());

        // An epic blocked ONLY by open children has no start-blockers: claimable.
        assert!(
            storage.get_start_blockers("bd-epic").unwrap().is_empty(),
            "epic with only open children must be startable (#315)"
        );
        // The close-ordering view (get_blockers) is unchanged — still rolls up.
        assert_eq!(
            storage.get_blockers("bd-epic").unwrap(),
            vec!["bd-epic.1".to_string()]
        );

        // An epic with a real `blocks` dependency is still start-blocked, and
        // the child-open marker is filtered out of the reported blockers.
        assert_eq!(
            storage.get_start_blockers("bd-epic-blocked").unwrap(),
            vec!["bd-real-blocker".to_string()],
            "real blocks edge must still block starting; child-open filtered (#315)"
        );
    }

    #[test]
    fn test_get_start_blockers_ignores_inherited_parent_blocked_marker() {
        // #357 (start-path counterpart of #355): a child whose ONLY blocker is
        // the inherited `<parent>:parent-blocked` rollup — i.e. its parent epic
        // is itself blocked, but the child has no real prerequisite of its own —
        // must be claimable / startable. `parent-child` is hierarchy, not a
        // prerequisite edge from the parent to the child, and the actionable
        // children of a blocked epic are frequently exactly the work that
        // unblocks the epic. A child with a DIRECT real blocker must still
        // reject, so this strips only the propagated marker, not real edges.
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let now = Utc::now();

        let mut parent = make_issue("bd-parent", "Parent epic", Status::Open, 2, None, now, None);
        parent.issue_type = IssueType::Epic;
        for issue in [
            parent,
            make_issue("bd-child", "Child task", Status::Open, 2, None, now, None),
            make_issue(
                "bd-child-direct",
                "Child with its own blocker",
                Status::Open,
                2,
                None,
                now,
                None,
            ),
            make_issue(
                "bd-blocker",
                "External blocker",
                Status::Open,
                2,
                None,
                now,
                None,
            ),
            make_issue(
                "bd-direct-blocker",
                "Direct blocker of bd-child-direct",
                Status::Open,
                2,
                None,
                now,
                None,
            ),
        ] {
            storage.create_issue(&issue, "tester").unwrap();
        }

        // The parent epic is genuinely blocked by an open prerequisite.
        storage
            .add_dependency("bd-parent", "bd-blocker", "blocks", "tester")
            .unwrap();
        // Both children belong to the parent via hierarchy (parent-child).
        storage
            .add_dependency("bd-child", "bd-parent", "parent-child", "tester")
            .unwrap();
        storage
            .add_dependency("bd-child-direct", "bd-parent", "parent-child", "tester")
            .unwrap();
        // bd-child-direct ALSO has a real prerequisite of its own.
        storage
            .add_dependency("bd-child-direct", "bd-direct-blocker", "blocks", "tester")
            .unwrap();

        // The blocked parent propagates a `:parent-blocked` marker onto its
        // children — confirm the child carries it as its only (non-direct)
        // blocker before asserting the start gate ignores it.
        let child_blockers = storage.get_blockers("bd-child").unwrap();
        assert!(
            child_blockers
                .iter()
                .any(|b| b == "bd-parent" || b == "bd-parent:parent-blocked"),
            "child of a blocked epic should inherit the parent-blocked rollup: {child_blockers:?}"
        );

        // #357: a child whose only blocker is the inherited parent-blocked
        // marker has no real start-blockers — it must be claimable / startable.
        assert!(
            storage.get_start_blockers("bd-child").unwrap().is_empty(),
            "child blocked only by inherited :parent-blocked must be startable (#357)"
        );

        // A child with a DIRECT real `blocks` edge is still start-blocked; only
        // the inherited parent-blocked marker is filtered, never the real edge.
        assert_eq!(
            storage.get_start_blockers("bd-child-direct").unwrap(),
            vec!["bd-direct-blocker".to_string()],
            "a real direct blocker must still prevent starting (#357 strips only the rollup)"
        );

        // The close path (already fixed by #355) remains correct: the child
        // with only the inherited marker is closable too.
        assert!(
            storage.get_close_blockers("bd-child").unwrap().is_empty(),
            "child blocked only by inherited :parent-blocked must be closable (#355)"
        );
    }

    #[test]
    fn test_rebuild_blocked_cache_impl_recreates_table_and_repopulates_entries() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let now = Utc::now();

        let blocker = make_issue("bd-reset-b1", "Blocker", Status::Open, 2, None, now, None);
        let blocked = make_issue("bd-reset-c1", "Blocked", Status::Open, 2, None, now, None);

        storage.create_issue(&blocker, "tester").unwrap();
        storage.create_issue(&blocked, "tester").unwrap();
        storage
            .add_dependency(&blocked.id, &blocker.id, "blocks", "tester")
            .unwrap();

        storage
            .conn
            .execute("DELETE FROM blocked_issues_cache")
            .unwrap();
        storage
            .conn
            .execute_with_params(
                "INSERT INTO blocked_issues_cache (issue_id, blocked_by, blocked_at) VALUES (?, ?, CURRENT_TIMESTAMP)",
                &[
                    SqliteValue::from(blocked.id.as_str()),
                    SqliteValue::from("[\"bd-old:open\"]"),
                ],
            )
            .unwrap();

        let rebuilt = SqliteStorage::rebuild_blocked_cache_impl(&storage.conn).unwrap();
        assert_eq!(rebuilt, 1);
        assert_eq!(storage.get_blockers(&blocked.id).unwrap(), vec![blocker.id]);
    }

    #[test]
    fn test_get_blocked_ids_and_is_blocked_fall_back_when_cache_table_missing() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let blocker = make_issue("bd-b1", "Blocker", Status::Open, 2, None, Utc::now(), None);
        let blocked = make_issue(
            "bd-c1",
            "Blocked issue",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        storage.create_issue(&blocker, "tester").unwrap();
        storage.create_issue(&blocked, "tester").unwrap();
        storage
            .add_dependency("bd-c1", "bd-b1", "blocks", "tester")
            .unwrap();

        storage
            .conn
            .execute("DROP TABLE blocked_issues_cache")
            .unwrap();

        let blocked_ids = storage.get_blocked_ids().unwrap();
        assert!(blocked_ids.contains("bd-c1"));
        assert!(storage.is_blocked("bd-c1").unwrap());
    }

    #[test]
    fn test_get_ready_issues_fall_back_when_cache_table_missing() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let blocker = make_issue("bd-b1", "Blocker", Status::Open, 1, None, Utc::now(), None);
        let blocked = make_issue(
            "bd-c1",
            "Blocked issue",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        let ready = make_issue(
            "bd-r1",
            "Ready issue",
            Status::Open,
            3,
            None,
            Utc::now(),
            None,
        );
        storage.create_issue(&blocker, "tester").unwrap();
        storage.create_issue(&blocked, "tester").unwrap();
        storage.create_issue(&ready, "tester").unwrap();
        storage
            .add_dependency("bd-c1", "bd-b1", "blocks", "tester")
            .unwrap();

        storage
            .conn
            .execute("DROP TABLE blocked_issues_cache")
            .unwrap();

        let ready_ids: HashSet<_> = storage
            .get_ready_issues(&ReadyFilters::default(), ReadySortPolicy::Priority)
            .unwrap()
            .into_iter()
            .map(|issue| issue.id)
            .collect();
        assert!(!ready_ids.contains("bd-c1"));
        assert!(ready_ids.contains("bd-b1"));
        assert!(ready_ids.contains("bd-r1"));
    }

    #[test]
    fn test_get_ready_issues_for_command_output_matches_full_ready_output() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let created_at = Utc.with_ymd_and_hms(2026, 3, 20, 12, 0, 0).unwrap();

        let mut ready = make_issue(
            "bd-ready-detailed",
            "Detailed ready issue",
            Status::Open,
            1,
            Some("alice"),
            created_at,
            None,
        );
        ready.description = Some("Description".to_string());
        ready.design = Some("Should not be loaded for ready output".to_string());
        ready.acceptance_criteria = Some("AC".to_string());
        ready.notes = Some("Notes".to_string());
        ready.owner = Some("product".to_string());
        ready.estimated_minutes = Some(45);
        ready.created_by = Some("agent".to_string());
        ready.updated_at = created_at + chrono::Duration::minutes(5);
        ready.external_ref = Some("jira-123".to_string());
        ready.source_system = Some("jira".to_string());
        ready.source_repo = Some("proj".to_string());
        ready.sender = Some("cli".to_string());

        let blocker = make_issue(
            "bd-blocker-detailed",
            "Blocker",
            Status::Open,
            0,
            None,
            created_at + chrono::Duration::minutes(1),
            None,
        );
        let blocked = make_issue(
            "bd-blocked-detailed",
            "Blocked",
            Status::Open,
            2,
            None,
            created_at + chrono::Duration::minutes(2),
            None,
        );

        storage.create_issue(&ready, "tester").unwrap();
        storage.create_issue(&blocker, "tester").unwrap();
        storage.create_issue(&blocked, "tester").unwrap();
        storage
            .add_dependency(
                "bd-blocked-detailed",
                "bd-blocker-detailed",
                "blocks",
                "tester",
            )
            .unwrap();

        let filters = ReadyFilters::default();
        let full: Vec<_> = storage
            .get_ready_issues(&filters, ReadySortPolicy::Priority)
            .unwrap()
            .into_iter()
            .map(ReadyIssue::from)
            .collect();
        let projected: Vec<_> = storage
            .get_ready_issues_for_command_output(&filters, ReadySortPolicy::Priority)
            .unwrap()
            .into_iter()
            .map(ReadyIssue::from)
            .collect();

        assert_eq!(projected, full);
    }

    #[test]
    fn test_get_ready_summary_issues_for_command_output_matches_full_text_fields() {
        let storage = ready_summary_projection_fixture();

        let filters = ReadyFilters::default();
        let full = storage
            .get_ready_issues(&filters, ReadySortPolicy::Priority)
            .unwrap()
            .into_iter()
            .map(ready_text_fields)
            .collect::<Vec<_>>();
        let projected_raw = storage
            .get_ready_summary_issues_for_command_output(&filters, ReadySortPolicy::Priority)
            .unwrap();
        let projected_issue = projected_raw
            .iter()
            .find(|issue| issue.id == "bd-ready-summary")
            .unwrap();
        assert!(projected_issue.description.is_none());
        assert!(projected_issue.design.is_none());
        assert!(projected_issue.acceptance_criteria.is_none());
        assert!(projected_issue.notes.is_none());
        assert!(projected_issue.assignee.is_none());
        assert!(projected_issue.owner.is_none());
        assert!(projected_issue.estimated_minutes.is_none());
        assert!(projected_issue.created_by.is_none());

        let projected = projected_raw
            .into_iter()
            .map(ready_text_fields)
            .collect::<Vec<_>>();

        assert_eq!(projected, full);
    }

    #[test]
    fn test_limited_ready_hybrid_hydrates_command_projection_after_summary_window() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let created_at = Utc.with_ymd_and_hms(2026, 3, 20, 12, 0, 0).unwrap();

        let mut first = make_issue(
            "bd-ready-window-first",
            "First visible ready issue",
            Status::Open,
            1,
            Some("alice"),
            created_at,
            None,
        );
        first.description = Some("Visible description".to_string());
        first.acceptance_criteria = Some("Visible acceptance criteria".to_string());
        first.notes = Some("Visible notes".to_string());
        first.owner = Some("product".to_string());
        first.estimated_minutes = Some(30);
        first.created_by = Some("agent".to_string());
        first.updated_at = created_at + chrono::Duration::minutes(5);

        let second = make_issue(
            "bd-ready-window-second",
            "Second ready issue",
            Status::Open,
            1,
            None,
            created_at + chrono::Duration::minutes(1),
            None,
        );

        storage.create_issue(&first, "tester").unwrap();
        storage.create_issue(&second, "tester").unwrap();

        let filters = ReadyFilters {
            limit: Some(1),
            ..ReadyFilters::default()
        };
        let command = storage
            .get_ready_issues_for_command_output(&filters, ReadySortPolicy::Hybrid)
            .unwrap();

        assert_eq!(command.len(), 1);
        assert_eq!(command[0].id, "bd-ready-window-first");
        assert_eq!(
            command[0].description.as_deref(),
            Some("Visible description")
        );
        assert_eq!(
            command[0].acceptance_criteria.as_deref(),
            Some("Visible acceptance criteria")
        );
        assert_eq!(command[0].notes.as_deref(), Some("Visible notes"));
        assert_eq!(command[0].assignee.as_deref(), Some("alice"));
        assert_eq!(command[0].owner.as_deref(), Some("product"));
        assert_eq!(command[0].estimated_minutes, Some(30));
        assert_eq!(command[0].created_by.as_deref(), Some("agent"));

        let summary = storage
            .get_ready_summary_issues_for_command_output(&filters, ReadySortPolicy::Hybrid)
            .unwrap();
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].id, "bd-ready-window-first");
        assert!(summary[0].description.is_none());
        assert!(summary[0].acceptance_criteria.is_none());
        assert!(summary[0].notes.is_none());
        assert!(summary[0].assignee.is_none());
    }

    #[test]
    fn test_limited_ready_hybrid_summary_window_skips_blocked_candidates() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let created_at = Utc.with_ymd_and_hms(2026, 3, 20, 12, 0, 0).unwrap();

        let blocker = make_issue(
            "bd-ready-window-blocker",
            "Window blocker",
            Status::Open,
            0,
            None,
            created_at,
            None,
        );
        let blocked_first = make_issue(
            "bd-ready-window-blocked-first",
            "Blocked first ready candidate",
            Status::Open,
            0,
            None,
            created_at + chrono::Duration::seconds(1),
            None,
        );
        let unblocked_second = make_issue(
            "bd-ready-window-unblocked-second",
            "Unblocked second ready candidate",
            Status::Open,
            0,
            None,
            created_at + chrono::Duration::seconds(2),
            None,
        );

        storage.create_issue(&blocker, "tester").unwrap();
        storage.create_issue(&blocked_first, "tester").unwrap();
        storage.create_issue(&unblocked_second, "tester").unwrap();
        storage
            .add_dependency(
                "bd-ready-window-blocked-first",
                "bd-ready-window-blocker",
                "blocks",
                "tester",
            )
            .unwrap();

        let filters = ReadyFilters {
            limit: Some(1),
            ..ReadyFilters::default()
        };
        let issues = storage
            .get_ready_summary_issues_for_command_output(&filters, ReadySortPolicy::Hybrid)
            .unwrap();

        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].id, "bd-ready-window-blocker");
    }

    #[test]
    fn test_limited_ready_hybrid_falls_back_when_high_bucket_is_short() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let created_at = Utc.with_ymd_and_hms(2026, 3, 20, 12, 0, 0).unwrap();

        let high = make_issue(
            "bd-ready-window-high",
            "High priority ready issue",
            Status::Open,
            1,
            None,
            created_at,
            None,
        );
        let medium = make_issue(
            "bd-ready-window-medium",
            "Medium priority fallback issue",
            Status::Open,
            2,
            None,
            created_at + chrono::Duration::minutes(1),
            None,
        );

        storage.create_issue(&high, "tester").unwrap();
        storage.create_issue(&medium, "tester").unwrap();

        let filters = ReadyFilters {
            limit: Some(2),
            ..ReadyFilters::default()
        };
        let issues = storage
            .get_ready_issues_for_command_output(&filters, ReadySortPolicy::Hybrid)
            .unwrap();

        let ids = issues
            .iter()
            .map(|issue| issue.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(ids, vec!["bd-ready-window-high", "bd-ready-window-medium"]);
    }

    #[test]
    fn test_get_blocked_issues_for_command_output_matches_full_blocked_output() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let created_at = Utc.with_ymd_and_hms(2026, 4, 1, 9, 0, 0).unwrap();

        let blocker = make_issue(
            "bd-blocker-detailed",
            "Blocker",
            Status::Open,
            0,
            None,
            created_at,
            None,
        );
        let mut blocked = make_issue(
            "bd-blocked-detailed",
            "Blocked detailed issue",
            Status::Open,
            2,
            None,
            created_at + chrono::Duration::minutes(1),
            None,
        );
        let large_unused = "unused overflow payload ".repeat(256);
        blocked.description = Some("Visible blocked description".to_string());
        blocked.design = Some(large_unused.clone());
        blocked.acceptance_criteria = Some(large_unused.clone());
        blocked.notes = Some(large_unused);
        blocked.created_by = Some("agent".to_string());
        blocked.updated_at = created_at + chrono::Duration::minutes(5);
        blocked.source_repo = Some("repo".to_string());
        blocked.sender = Some("cli".to_string());

        storage.create_issue(&blocker, "tester").unwrap();
        storage.create_issue(&blocked, "tester").unwrap();
        storage
            .add_dependency(
                "bd-blocked-detailed",
                "bd-blocker-detailed",
                "blocks",
                "tester",
            )
            .unwrap();

        let full: Vec<_> = storage
            .get_blocked_issues()
            .unwrap()
            .into_iter()
            .map(blocked_issue_output_for_test)
            .collect();
        let projected_raw = storage.get_blocked_issues_for_command_output().unwrap();
        let projected_issue = &projected_raw
            .iter()
            .find(|(issue, _)| issue.id == "bd-blocked-detailed")
            .unwrap()
            .0;
        assert!(projected_issue.design.is_none());
        assert!(projected_issue.acceptance_criteria.is_none());
        assert!(projected_issue.notes.is_none());
        assert!(projected_issue.source_repo.is_none());
        assert!(projected_issue.sender.is_none());

        let projected: Vec<_> = projected_raw
            .into_iter()
            .map(blocked_issue_output_for_test)
            .collect();

        assert_eq!(
            serde_json::to_value(projected).unwrap(),
            serde_json::to_value(full).unwrap()
        );
    }

    #[test]
    fn test_get_blocked_issues_for_command_output_falls_back_when_cache_table_missing() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let created_at = Utc.with_ymd_and_hms(2026, 4, 1, 10, 0, 0).unwrap();
        let blocker = make_issue("bd-b1", "Blocker", Status::Open, 1, None, created_at, None);
        let mut blocked = make_issue(
            "bd-c1",
            "Blocked issue",
            Status::Open,
            2,
            None,
            created_at + chrono::Duration::minutes(1),
            None,
        );
        blocked.description = Some("Visible description".to_string());
        blocked.design = Some("unused design".repeat(512));
        let ready = make_issue(
            "bd-r1",
            "Ready issue",
            Status::Open,
            3,
            None,
            created_at + chrono::Duration::minutes(2),
            None,
        );
        storage.create_issue(&blocker, "tester").unwrap();
        storage.create_issue(&blocked, "tester").unwrap();
        storage.create_issue(&ready, "tester").unwrap();
        storage
            .add_dependency("bd-c1", "bd-b1", "blocks", "tester")
            .unwrap();

        storage
            .conn
            .execute("DROP TABLE blocked_issues_cache")
            .unwrap();

        let full: Vec<_> = storage
            .get_blocked_issues()
            .unwrap()
            .into_iter()
            .map(blocked_issue_output_for_test)
            .collect();
        let projected_raw = storage.get_blocked_issues_for_command_output().unwrap();
        let projected_issue = &projected_raw
            .iter()
            .find(|(issue, _)| issue.id == "bd-c1")
            .unwrap()
            .0;
        assert!(projected_issue.design.is_none());

        let projected: Vec<_> = projected_raw
            .into_iter()
            .map(blocked_issue_output_for_test)
            .collect();

        assert_eq!(
            serde_json::to_value(projected).unwrap(),
            serde_json::to_value(full).unwrap()
        );
    }

    #[test]
    fn test_list_stale_issues_for_command_output_matches_full_stale_output() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 4, 10, 12, 0, 0).unwrap();
        let old_at = now - chrono::Duration::days(45);
        let fresh_at = now - chrono::Duration::days(1);

        let mut stale_issue = make_issue(
            "bd-stale-detailed",
            "Detailed stale issue",
            Status::Open,
            1,
            Some("alice"),
            old_at,
            None,
        );
        stale_issue.description = Some("Should not be loaded".to_string());
        stale_issue.design = Some("unused design".repeat(512));
        stale_issue.acceptance_criteria = Some("unused ac".repeat(512));
        stale_issue.notes = Some("unused notes".repeat(512));
        stale_issue.owner = Some("owner".to_string());
        stale_issue.created_by = Some("creator".to_string());
        stale_issue.source_repo = Some("repo".to_string());
        stale_issue.sender = Some("cli".to_string());

        let fresh_issue = make_issue(
            "bd-fresh",
            "Fresh issue",
            Status::Open,
            2,
            Some("bob"),
            fresh_at,
            None,
        );

        storage.create_issue(&stale_issue, "tester").unwrap();
        storage.create_issue(&fresh_issue, "tester").unwrap();

        let filters = ListFilters {
            include_deferred: true,
            updated_before: Some(now - chrono::Duration::days(30)),
            sort: Some("updated_at".to_string()),
            reverse: true,
            ..ListFilters::default()
        };

        let full: Vec<_> = storage
            .list_issues(&filters)
            .unwrap()
            .into_iter()
            .map(StaleIssue::from)
            .collect();
        let projected_raw = storage
            .list_stale_issues_for_command_output(&filters)
            .unwrap();
        let projected_issue = projected_raw
            .iter()
            .find(|issue| issue.id == "bd-stale-detailed")
            .unwrap();
        assert!(projected_issue.description.is_none());
        assert!(projected_issue.design.is_none());
        assert!(projected_issue.acceptance_criteria.is_none());
        assert!(projected_issue.notes.is_none());
        assert!(projected_issue.source_repo.is_none());
        assert!(projected_issue.sender.is_none());

        let projected: Vec<_> = projected_raw.into_iter().map(StaleIssue::from).collect();

        assert_eq!(
            serde_json::to_value(projected).unwrap(),
            serde_json::to_value(full).unwrap()
        );
    }

    #[test]
    fn test_list_orphan_candidate_issues_for_command_output_matches_full_candidate_fields() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 4, 11, 12, 0, 0).unwrap();

        let mut open_issue = make_issue(
            "bd-orphan-open",
            "Open orphan candidate",
            Status::Open,
            1,
            None,
            now,
            None,
        );
        open_issue.description = Some("Should not be loaded".to_string());
        open_issue.design = Some("unused design".repeat(512));
        open_issue.acceptance_criteria = Some("unused ac".repeat(512));
        open_issue.notes = Some("unused notes".repeat(512));
        open_issue.owner = Some("owner".to_string());
        open_issue.sender = Some("cli".to_string());

        let in_progress_issue = make_issue(
            "bd-orphan-progress",
            "In-progress orphan candidate",
            Status::InProgress,
            2,
            None,
            now - chrono::Duration::minutes(1),
            None,
        );
        let mut closed_issue = make_issue(
            "bd-orphan-closed",
            "Closed non-candidate",
            Status::Closed,
            3,
            None,
            now - chrono::Duration::minutes(2),
            None,
        );
        closed_issue.closed_at = Some(now);

        storage.create_issue(&open_issue, "tester").unwrap();
        storage.create_issue(&in_progress_issue, "tester").unwrap();
        storage.create_issue(&closed_issue, "tester").unwrap();

        let filters = ListFilters {
            statuses: Some(vec![Status::Open, Status::InProgress]),
            ..ListFilters::default()
        };
        let full = storage
            .list_issues(&filters)
            .unwrap()
            .into_iter()
            .map(|issue| (issue.id, issue.title, issue.status, issue.priority))
            .collect::<Vec<_>>();
        let projected_raw = storage
            .list_orphan_candidate_issues_for_command_output(&filters)
            .unwrap();
        let projected_issue = projected_raw
            .iter()
            .find(|issue| issue.id == "bd-orphan-open")
            .unwrap();
        assert!(projected_issue.description.is_none());
        assert!(projected_issue.design.is_none());
        assert!(projected_issue.acceptance_criteria.is_none());
        assert!(projected_issue.notes.is_none());
        assert!(projected_issue.owner.is_none());
        assert!(projected_issue.sender.is_none());

        let projected = projected_raw
            .into_iter()
            .map(|issue| (issue.id, issue.title, issue.status, issue.priority))
            .collect::<Vec<_>>();

        assert_eq!(projected, full);
    }

    #[test]
    fn test_list_graph_issues_for_command_output_matches_full_graph_fields() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 4, 12, 12, 0, 0).unwrap();

        let mut open_issue = make_issue(
            "bd-graph-open",
            "Open graph node",
            Status::Open,
            1,
            None,
            now,
            None,
        );
        open_issue.description = Some("Should not be loaded".to_string());
        open_issue.design = Some("unused design".repeat(512));
        open_issue.acceptance_criteria = Some("unused ac".repeat(512));
        open_issue.notes = Some("unused notes".repeat(512));
        open_issue.owner = Some("owner".to_string());
        open_issue.sender = Some("cli".to_string());

        let deferred_issue = make_issue(
            "bd-graph-deferred",
            "Deferred graph node",
            Status::Deferred,
            2,
            None,
            now - chrono::Duration::minutes(1),
            None,
        );
        let mut closed_issue = make_issue(
            "bd-graph-closed",
            "Closed non-node",
            Status::Closed,
            3,
            None,
            now - chrono::Duration::minutes(2),
            None,
        );
        closed_issue.closed_at = Some(now);

        storage.create_issue(&open_issue, "tester").unwrap();
        storage.create_issue(&deferred_issue, "tester").unwrap();
        storage.create_issue(&closed_issue, "tester").unwrap();

        let filters = ListFilters {
            include_deferred: true,
            ..ListFilters::default()
        };
        let full = storage
            .list_issues(&filters)
            .unwrap()
            .into_iter()
            .map(|issue| (issue.id, issue.title, issue.status, issue.priority))
            .collect::<Vec<_>>();
        let projected_raw = storage
            .list_graph_issues_for_command_output(&filters)
            .unwrap();
        let projected_issue = projected_raw
            .iter()
            .find(|issue| issue.id == "bd-graph-open")
            .unwrap();
        assert!(projected_issue.description.is_none());
        assert!(projected_issue.design.is_none());
        assert!(projected_issue.acceptance_criteria.is_none());
        assert!(projected_issue.notes.is_none());
        assert!(projected_issue.owner.is_none());
        assert!(projected_issue.sender.is_none());

        let projected = projected_raw
            .into_iter()
            .map(|issue| (issue.id, issue.title, issue.status, issue.priority))
            .collect::<Vec<_>>();

        assert_eq!(projected, full);
    }

    #[test]
    fn test_list_issues_default_visible_limited_page_matches_sql_order() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let day_1 = Utc.with_ymd_and_hms(2026, 4, 13, 12, 0, 0).unwrap();
        let day_2 = Utc.with_ymd_and_hms(2026, 4, 14, 12, 0, 0).unwrap();
        let day_3 = Utc.with_ymd_and_hms(2026, 4, 15, 12, 0, 0).unwrap();

        for issue in [
            make_issue("bd-p1-old", "P1 old", Status::Open, 1, None, day_1, None),
            make_issue("bd-p0-old", "P0 old", Status::Open, 0, None, day_1, None),
            make_issue("bd-p0-a", "P0 tie A", Status::Open, 0, None, day_2, None),
            make_issue("bd-p0-b", "P0 tie B", Status::Open, 0, None, day_2, None),
            make_issue(
                "bd-deferred-new",
                "Deferred new",
                Status::Deferred,
                0,
                None,
                day_3,
                None,
            ),
        ] {
            storage.create_issue(&issue, "tester").unwrap();
        }

        let mut closed = make_issue("bd-closed", "Closed", Status::Closed, 0, None, day_3, None);
        closed.closed_at = Some(day_3);
        storage.create_issue(&closed, "tester").unwrap();

        let mut template = make_issue(
            "bd-template",
            "Template",
            Status::Open,
            0,
            None,
            day_3,
            None,
        );
        template.is_template = true;
        storage.create_issue(&template, "tester").unwrap();

        let fast = storage
            .list_issues(&ListFilters {
                include_deferred: true,
                limit: Some(5),
                offset: Some(0),
                ..ListFilters::default()
            })
            .unwrap();
        let baseline = storage
            .list_issues(&ListFilters {
                include_deferred: true,
                limit: Some(5),
                offset: Some(0),
                sort: Some("priority".to_string()),
                ..ListFilters::default()
            })
            .unwrap();

        let fast_ids = fast
            .iter()
            .map(|issue| issue.id.as_str())
            .collect::<Vec<_>>();
        let baseline_ids = baseline
            .iter()
            .map(|issue| issue.id.as_str())
            .collect::<Vec<_>>();

        assert_eq!(fast_ids, baseline_ids);
        assert_eq!(
            fast_ids,
            vec![
                "bd-deferred-new",
                "bd-p0-a",
                "bd-p0-b",
                "bd-p0-old",
                "bd-p1-old"
            ]
        );
    }

    #[test]
    fn test_list_text_issues_for_command_output_matches_full_summary_fields() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 4, 13, 12, 0, 0).unwrap();

        let mut open_issue = make_issue(
            "bd-list-open",
            "Open text row",
            Status::Open,
            1,
            None,
            now,
            None,
        );
        open_issue.description = Some("Should not be loaded".to_string());
        open_issue.design = Some("unused design".repeat(512));
        open_issue.acceptance_criteria = Some("unused ac".repeat(512));
        open_issue.notes = Some("unused notes".repeat(512));
        open_issue.owner = Some("owner".to_string());
        open_issue.sender = Some("cli".to_string());

        let deferred_issue = make_issue(
            "bd-list-deferred",
            "Deferred text row",
            Status::Deferred,
            2,
            None,
            now - chrono::Duration::minutes(1),
            None,
        );
        let mut closed_issue = make_issue(
            "bd-list-closed",
            "Closed non-row",
            Status::Closed,
            3,
            None,
            now - chrono::Duration::minutes(2),
            None,
        );
        closed_issue.closed_at = Some(now);

        storage.create_issue(&open_issue, "tester").unwrap();
        storage.create_issue(&deferred_issue, "tester").unwrap();
        storage.create_issue(&closed_issue, "tester").unwrap();

        let filters = ListFilters {
            include_deferred: true,
            limit: Some(0),
            offset: Some(0),
            ..ListFilters::default()
        };
        let full = storage
            .list_issues(&filters)
            .unwrap()
            .into_iter()
            .map(|issue| {
                (
                    issue.id,
                    issue.title,
                    issue.status,
                    issue.priority,
                    issue.issue_type,
                )
            })
            .collect::<Vec<_>>();
        let projected_raw = storage
            .list_text_issues_for_command_output(&filters)
            .unwrap();
        let projected_issue = projected_raw
            .iter()
            .find(|issue| issue.id == "bd-list-open")
            .unwrap();
        assert!(projected_issue.description.is_none());
        assert!(projected_issue.design.is_none());
        assert!(projected_issue.acceptance_criteria.is_none());
        assert!(projected_issue.notes.is_none());
        assert!(projected_issue.owner.is_none());
        assert!(projected_issue.sender.is_none());

        let projected = projected_raw
            .into_iter()
            .map(|issue| {
                (
                    issue.id,
                    issue.title,
                    issue.status,
                    issue.priority,
                    issue.issue_type,
                )
            })
            .collect::<Vec<_>>();

        assert_eq!(projected, full);
    }

    #[test]
    fn test_list_text_issues_for_command_output_supports_limit_offset() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 4, 13, 12, 0, 0).unwrap();

        let mut oldest_high = make_issue(
            "bd-list-high-old",
            "High old",
            Status::Open,
            1,
            None,
            now - chrono::Duration::minutes(2),
            None,
        );
        oldest_high.description = Some("Should not be loaded".repeat(256));
        let newest_high = make_issue(
            "bd-list-high-new",
            "High new",
            Status::Open,
            1,
            None,
            now,
            None,
        );
        let normal = make_issue(
            "bd-list-normal",
            "Normal",
            Status::Open,
            2,
            None,
            now - chrono::Duration::minutes(1),
            None,
        );

        storage.create_issue(&oldest_high, "tester").unwrap();
        storage.create_issue(&newest_high, "tester").unwrap();
        storage.create_issue(&normal, "tester").unwrap();

        let filters = ListFilters {
            include_deferred: true,
            limit: Some(2),
            offset: Some(1),
            ..ListFilters::default()
        };
        let full = storage
            .list_issues(&filters)
            .unwrap()
            .into_iter()
            .map(|issue| (issue.id, issue.title, issue.status, issue.priority))
            .collect::<Vec<_>>();
        let projected_raw = storage
            .list_text_issues_for_command_output(&filters)
            .unwrap();
        let projected_issue = projected_raw
            .iter()
            .find(|issue| issue.id == "bd-list-high-old")
            .unwrap();
        assert!(projected_issue.description.is_none());

        let projected = projected_raw
            .into_iter()
            .map(|issue| (issue.id, issue.title, issue.status, issue.priority))
            .collect::<Vec<_>>();

        assert_eq!(
            projected,
            vec![
                (
                    "bd-list-high-old".to_string(),
                    "High old".to_string(),
                    Status::Open,
                    Priority(1),
                ),
                (
                    "bd-list-normal".to_string(),
                    "Normal".to_string(),
                    Status::Open,
                    Priority(2),
                ),
            ]
        );
        assert_eq!(projected, full);
    }

    #[test]
    fn test_get_ready_issues_for_command_output_falls_back_when_cache_table_missing() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let blocker = make_issue("bd-b1", "Blocker", Status::Open, 1, None, Utc::now(), None);
        let blocked = make_issue(
            "bd-c1",
            "Blocked issue",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        let ready = make_issue(
            "bd-r1",
            "Ready issue",
            Status::Open,
            3,
            None,
            Utc::now(),
            None,
        );
        storage.create_issue(&blocker, "tester").unwrap();
        storage.create_issue(&blocked, "tester").unwrap();
        storage.create_issue(&ready, "tester").unwrap();
        storage
            .add_dependency("bd-c1", "bd-b1", "blocks", "tester")
            .unwrap();

        storage
            .conn
            .execute("DROP TABLE blocked_issues_cache")
            .unwrap();

        let full: Vec<_> = storage
            .get_ready_issues(&ReadyFilters::default(), ReadySortPolicy::Priority)
            .unwrap()
            .into_iter()
            .map(ReadyIssue::from)
            .collect();
        let projected: Vec<_> = storage
            .get_ready_issues_for_command_output(
                &ReadyFilters::default(),
                ReadySortPolicy::Priority,
            )
            .unwrap()
            .into_iter()
            .map(ReadyIssue::from)
            .collect();

        assert_eq!(projected, full);
    }

    #[test]
    fn test_get_blockers_fall_back_on_malformed_cache_json() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let blocker = make_issue("bd-b1", "Blocker", Status::Open, 2, None, Utc::now(), None);
        let issue = make_issue(
            "bd-c1",
            "Blocked issue",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        storage.create_issue(&blocker, "tester").unwrap();
        storage.create_issue(&issue, "tester").unwrap();
        storage
            .add_dependency("bd-c1", "bd-b1", "blocks", "tester")
            .unwrap();

        storage
            .conn
            .execute_with_params(
                "UPDATE blocked_issues_cache SET blocked_by = ? WHERE issue_id = ?",
                &[SqliteValue::from("not-json"), SqliteValue::from("bd-c1")],
            )
            .unwrap();

        assert_eq!(
            storage.get_blockers("bd-c1").unwrap(),
            vec!["bd-b1".to_string()]
        );
    }

    #[test]
    fn test_get_blocked_issues_fall_back_on_malformed_cache_json() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let blocker = make_issue("bd-b1", "Blocker", Status::Open, 1, None, Utc::now(), None);
        let blocked = make_issue(
            "bd-c1",
            "Blocked issue",
            Status::Open,
            2,
            None,
            Utc::now(),
            None,
        );
        storage.create_issue(&blocker, "tester").unwrap();
        storage.create_issue(&blocked, "tester").unwrap();
        storage
            .add_dependency("bd-c1", "bd-b1", "blocks", "tester")
            .unwrap();

        storage
            .conn
            .execute_with_params(
                "UPDATE blocked_issues_cache SET blocked_by = ? WHERE issue_id = ?",
                &[SqliteValue::from("not-json"), SqliteValue::from("bd-c1")],
            )
            .unwrap();

        let blocked_issues = storage.get_blocked_issues().unwrap();
        assert_eq!(blocked_issues.len(), 1);
        assert_eq!(blocked_issues[0].0.id, "bd-c1");
        assert!(
            blocked_issues[0]
                .1
                .iter()
                .any(|blocker_ref| blocker_ref.starts_with("bd-b1:")),
            "fallback should preserve blocker metadata, got: {:?}",
            blocked_issues[0].1
        );
    }

    #[test]
    fn test_blocker_helpers_ignore_non_blocking_related_edges() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 3, 3, 0, 0, 0).unwrap();

        let blocker = make_issue("bd-b1", "Blocker", Status::Open, 2, None, t1, None);
        let blocked = make_issue("bd-c1", "Blocked", Status::Open, 2, None, t1, None);
        let parent = make_issue("bd-p1", "Parent", Status::Open, 2, None, t1, None);
        let child = make_issue("bd-p1.1", "Child", Status::Open, 2, None, t1, None);
        let related = make_issue("bd-r1", "Related", Status::Open, 2, None, t1, None);

        storage.create_issue(&blocker, "tester").unwrap();
        storage.create_issue(&blocked, "tester").unwrap();
        storage.create_issue(&parent, "tester").unwrap();
        storage.create_issue(&child, "tester").unwrap();
        storage.create_issue(&related, "tester").unwrap();

        storage
            .add_dependency("bd-c1", "bd-b1", "blocks", "tester")
            .unwrap();
        storage
            .add_dependency("bd-p1.1", "bd-p1", "parent-child", "tester")
            .unwrap();
        storage
            .add_dependency("bd-r1", "bd-b1", "related", "tester")
            .unwrap();

        let blocker_ids = storage.get_blocker_ids("bd-c1").unwrap();
        assert_eq!(blocker_ids, vec!["bd-b1"]);

        let parent_blockers = storage.get_blocker_ids("bd-p1").unwrap();
        assert_eq!(parent_blockers, vec!["bd-p1.1"]);

        let related_blockers = storage.get_blocker_ids("bd-r1").unwrap();
        assert!(
            related_blockers.is_empty(),
            "non-blocking related edges should not be reported as blockers"
        );

        let blocked_issue_ids = storage.get_blocked_issue_ids("bd-b1").unwrap();
        assert_eq!(blocked_issue_ids, vec!["bd-c1"]);

        let child_blocked_issue_ids = storage.get_blocked_issue_ids("bd-p1.1").unwrap();
        assert_eq!(child_blocked_issue_ids, vec!["bd-p1"]);
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
    fn test_set_config_normalizes_issue_prefix() {
        let mut storage = SqliteStorage::open_memory().unwrap();

        storage
            .set_config("issue_prefix", " Project-Name! ")
            .unwrap();

        assert_eq!(
            storage.get_config("issue_prefix").unwrap(),
            Some("project-name".to_string())
        );
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
    fn test_database_header_user_version_reads_file_header_value() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("header_user_version.db");

        let conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        conn.execute(&format!("PRAGMA user_version = {CURRENT_SCHEMA_VERSION}"))
            .unwrap();
        conn.close().unwrap();

        assert_eq!(
            database_header_user_version(&db_path),
            Some(u32::try_from(CURRENT_SCHEMA_VERSION).unwrap())
        );
    }

    #[test]
    fn test_open_with_timeout_does_not_require_write_lock_when_schema_current() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("lock_read_open.db");

        let _ = SqliteStorage::open(&db_path).unwrap();

        let lock_conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        lock_conn.execute("BEGIN IMMEDIATE").unwrap();

        let opened = SqliteStorage::open_with_timeout(&db_path, Some(50));
        assert!(
            opened.is_ok(),
            "opening an existing DB should succeed for read paths under a concurrent write lock"
        );

        lock_conn.execute("COMMIT").unwrap();
    }

    #[test]
    fn test_open_uses_default_busy_timeout() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("lock_read_open_default.db");

        let _ = SqliteStorage::open(&db_path).unwrap();

        let lock_conn = Connection::open(db_path.to_string_lossy().into_owned()).unwrap();
        lock_conn.execute("BEGIN IMMEDIATE").unwrap();

        let opened = SqliteStorage::open(&db_path);
        assert!(
            opened.is_ok(),
            "default open() should use the standard busy timeout under a concurrent write lock"
        );

        lock_conn.execute("COMMIT").unwrap();
    }

    #[test]
    fn test_open_current_read_only_skips_metadata_default_seeding() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("readonly_current.db");

        {
            let storage = SqliteStorage::open(&db_path).unwrap();
            storage
                .conn
                .execute_with_params(
                    "DELETE FROM metadata WHERE key = ?",
                    &[SqliteValue::from(METADATA_JSONL_CONTENT_HASH)],
                )
                .unwrap();
        }

        let storage = SqliteStorage::open_current_read_only(&db_path)
            .unwrap()
            .expect("current DB should open read-only");
        let rows = storage
            .conn
            .query_with_params(
                "SELECT 1 FROM metadata WHERE key = ? LIMIT 1",
                &[SqliteValue::from(METADATA_JSONL_CONTENT_HASH)],
            )
            .unwrap();
        assert!(
            rows.is_empty(),
            "read-only current open must not reseed missing metadata defaults"
        );
    }

    #[test]
    fn test_open_current_read_only_declines_stale_schema_header() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("readonly_stale.db");

        {
            let storage = SqliteStorage::open(&db_path).unwrap();
            storage.conn.execute("PRAGMA user_version = 0").unwrap();
        }

        assert!(
            SqliteStorage::open_current_read_only(&db_path)
                .unwrap()
                .is_none(),
            "stale schema headers must fall back to the normal repair-capable open path"
        );
    }

    #[test]
    fn test_open_repairs_runtime_compatible_legacy_db_indexes() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("legacy_runtime_compatible.db");

        {
            let storage = SqliteStorage::open(&db_path).unwrap();
            storage
                .conn
                .execute("DROP INDEX IF EXISTS idx_issues_external_ref_unique")
                .unwrap();
            storage.conn.execute("PRAGMA user_version = 0").unwrap();
        }

        let reopened = SqliteStorage::open(&db_path).unwrap();
        let user_version = reopened
            .conn
            .query_row("PRAGMA user_version")
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap();
        assert_eq!(
            user_version,
            i64::from(CURRENT_SCHEMA_VERSION),
            "runtime-compatible legacy DBs should be repaired and marked current on open"
        );

        let indexes: HashSet<String> = reopened
            .conn
            .query("SELECT name FROM sqlite_master WHERE type='index'")
            .unwrap()
            .iter()
            .filter_map(|row| row.get(0).and_then(SqliteValue::as_text).map(str::to_owned))
            .collect();
        assert!(
            indexes.contains("idx_issues_external_ref_unique"),
            "runtime-compatible repair path should restore missing canonical indexes"
        );
    }

    #[test]
    fn test_open_repairs_missing_canonical_indexes_even_when_user_version_is_current() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("current_version_missing_index.db");

        {
            let storage = SqliteStorage::open(&db_path).unwrap();
            storage
                .conn
                .execute("DROP INDEX IF EXISTS idx_issues_external_ref_unique")
                .unwrap();
            // Reset user_version so the reopen takes the full schema path
            // (the fast path only applies runtime pragmas and does not
            // recreate missing indexes).
            storage.conn.execute("PRAGMA user_version = 0").unwrap();
        }

        let reopened = SqliteStorage::open(&db_path).unwrap();
        let user_version = reopened
            .conn
            .query_row("PRAGMA user_version")
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap();
        assert_eq!(
            user_version,
            i64::from(CURRENT_SCHEMA_VERSION),
            "reopen should restore the current schema version"
        );

        // Use PRAGMA index_list instead of sqlite_master (more reliable in fsqlite)
        let index_rows = reopened.conn.query("PRAGMA index_list('issues')").unwrap();
        let index_names: HashSet<String> = index_rows
            .iter()
            .filter_map(|row| row.get(1).and_then(SqliteValue::as_text).map(str::to_owned))
            .collect();
        assert!(
            index_names.contains("idx_issues_external_ref_unique"),
            "reopen should recreate missing canonical indexes, got: {index_names:?}"
        );
    }

    #[test]
    fn test_open_repairs_current_version_nullable_blocked_cache_table() {
        let temp = TempDir::new().unwrap();
        let db_path = temp
            .path()
            .join("current_version_nullable_blocked_cache.db");

        {
            let storage = SqliteStorage::open(&db_path).unwrap();
            storage
                .conn
                .execute("DROP TABLE blocked_issues_cache")
                .unwrap();
            storage
                .conn
                .execute(
                    "CREATE TABLE blocked_issues_cache (
                        issue_id TEXT PRIMARY KEY,
                        blocked_by TEXT,
                        blocked_at DATETIME
                    )",
                )
                .unwrap();
            storage
                .conn
                .execute(
                    "INSERT INTO blocked_issues_cache (issue_id, blocked_by, blocked_at)
                     VALUES ('bd-null-cache', NULL, CURRENT_TIMESTAMP)",
                )
                .unwrap();
            storage
                .conn
                .execute(&format!("PRAGMA user_version = {CURRENT_SCHEMA_VERSION}"))
                .unwrap();
        }

        let reopened = SqliteStorage::open(&db_path).unwrap();
        let column_rows = reopened
            .conn
            .query("PRAGMA table_info(blocked_issues_cache)")
            .unwrap();
        let mut blocked_by_not_null = false;
        for row in &column_rows {
            if row.get(1).and_then(SqliteValue::as_text) == Some("blocked_by") {
                blocked_by_not_null = row
                    .get(3)
                    .and_then(SqliteValue::as_integer)
                    .is_some_and(|value| value != 0);
            }
        }
        assert!(
            blocked_by_not_null,
            "current-version open should repair nullable blocked_by cache columns"
        );

        let null_rows = reopened
            .conn
            .query_row("SELECT COUNT(*) FROM blocked_issues_cache WHERE blocked_by IS NULL")
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap_or(-1);
        assert_eq!(
            null_rows, 0,
            "current-version open should discard malformed derived cache rows"
        );
    }

    #[test]
    fn test_open_repairs_legacy_kv_primary_key_tables() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("legacy_kv_primary_keys.db");

        {
            let mut storage = SqliteStorage::open(&db_path).unwrap();
            storage.set_config("issue_prefix", "legacy").unwrap();
            storage.set_metadata("project", "legacy-project").unwrap();

            storage
                .conn
                .execute("DROP INDEX IF EXISTS idx_config_key")
                .unwrap();
            storage.conn.execute("DROP TABLE config").unwrap();
            storage
                .conn
                .execute("CREATE TABLE config (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
                .unwrap();
            storage
                .conn
                .execute("INSERT INTO config (key, value) VALUES ('issue_prefix', 'legacy')")
                .unwrap();

            storage
                .conn
                .execute("DROP INDEX IF EXISTS idx_metadata_key")
                .unwrap();
            storage.conn.execute("DROP TABLE metadata").unwrap();
            storage
                .conn
                .execute("CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL)")
                .unwrap();
            storage
                .conn
                .execute("INSERT INTO metadata (key, value) VALUES ('project', 'legacy-project')")
                .unwrap();

            storage.conn.execute("PRAGMA user_version = 0").unwrap();
        }

        let reopened = SqliteStorage::open(&db_path).unwrap();
        assert_eq!(
            reopened.get_config("issue_prefix").unwrap(),
            Some("legacy".to_string())
        );
        assert_eq!(
            reopened.get_metadata("project").unwrap(),
            Some("legacy-project".to_string())
        );

        // Use PRAGMA table_info to verify the repair (sqlite_master can
        // return inconsistent results in fsqlite).
        // Check that the `key` column no longer has pk flag set.
        let config_has_pk = reopened
            .conn
            .query("PRAGMA table_info('config')")
            .unwrap()
            .iter()
            .any(|row| {
                let col_name = row.get(1).and_then(SqliteValue::as_text);
                let pk_flag = row.get(5).and_then(SqliteValue::as_integer).unwrap_or(0);
                col_name == Some("key") && pk_flag > 0
            });
        let metadata_has_pk = reopened
            .conn
            .query("PRAGMA table_info('metadata')")
            .unwrap()
            .iter()
            .any(|row| {
                let col_name = row.get(1).and_then(SqliteValue::as_text);
                let pk_flag = row.get(5).and_then(SqliteValue::as_integer).unwrap_or(0);
                col_name == Some("key") && pk_flag > 0
            });

        assert!(
            !config_has_pk,
            "legacy config primary key should be rebuilt to the canonical shape"
        );
        assert!(
            !metadata_has_pk,
            "legacy metadata primary key should be rebuilt to the canonical shape"
        );
    }

    #[test]
    fn test_upsert_issue_for_import_coalesces_optional_text_fields_to_empty_strings() {
        let storage = SqliteStorage::open_memory().unwrap();
        let issue = Issue {
            id: "bd-import-null-optional-text".to_string(),
            title: "Import null optional text".to_string(),
            ..Issue::default()
        };

        storage.upsert_issue_for_import(&issue).unwrap();

        let row = storage
            .conn
            .query_row_with_params(
                "SELECT
                    typeof(description), typeof(design), typeof(acceptance_criteria), typeof(notes),
                    typeof(owner), typeof(created_by), typeof(close_reason), typeof(closed_by_session),
                    typeof(source_system), typeof(source_repo), typeof(deleted_by), typeof(delete_reason),
                    typeof(original_type), typeof(sender),
                    description, design, acceptance_criteria, notes, owner, created_by, close_reason,
                    closed_by_session, source_system, source_repo, deleted_by, delete_reason,
                    original_type, sender
                 FROM issues WHERE id = ?",
                &[SqliteValue::from(issue.id.as_str())],
            )
            .unwrap();

        for index in 0..14 {
            assert_eq!(
                row.get(index).and_then(SqliteValue::as_text),
                Some("text"),
                "column {index} should store an empty string, not NULL"
            );
        }

        for index in 14..23 {
            assert_eq!(
                row.get(index).and_then(SqliteValue::as_text),
                Some(""),
                "column {index} should coalesce missing optional text to ''"
            );
        }

        assert_eq!(
            row.get(23).and_then(SqliteValue::as_text),
            Some("."),
            "source_repo should coalesce missing values to '.'"
        );

        for index in 24..28 {
            assert_eq!(
                row.get(index).and_then(SqliteValue::as_text),
                Some(""),
                "column {index} should coalesce missing optional text to ''"
            );
        }
    }

    /// Regression test for issue #263 (a): import upsert must not
    /// cascade-drop child rows. Pre-fix, `upsert_issue_for_import`
    /// did `DELETE FROM issues WHERE id = ?` then `INSERT`, and the
    /// child tables' `ON DELETE CASCADE` foreign keys swept events,
    /// labels, deps, and comments out from under the issue every
    /// time an import touched it.
    #[test]
    fn test_upsert_issue_for_import_preserves_child_rows() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let stamp = Utc.with_ymd_and_hms(2026, 4, 26, 12, 0, 0).unwrap();
        let issue = make_issue(
            "bd-import-preserve-child",
            "Original title",
            Status::Open,
            2,
            None,
            stamp,
            None,
        );
        storage.create_issue(&issue, "tester").unwrap();
        let events_before = storage.get_events(&issue.id, 20).unwrap();
        assert!(
            !events_before.is_empty(),
            "fixture: create_issue should produce at least one event row"
        );

        let mut imported = issue.clone();
        imported.title = "Imported title".to_string();
        imported.updated_at = stamp + chrono::Duration::minutes(1);

        storage
            .upsert_issue_for_import(&imported)
            .expect("import upsert should run an in-place UPDATE for an existing id");

        let updated = storage.get_issue(&issue.id).unwrap().unwrap();
        assert_eq!(updated.title, "Imported title");
        assert_eq!(
            storage.get_events(&issue.id, 20).unwrap().len(),
            events_before.len(),
            "child events must survive an import that touches the parent row",
        );
        assert!(
            !storage
                .has_missing_issue_reference("events", "issue_id")
                .unwrap(),
            "import upsert must not leave dangling event rows referencing a deleted parent",
        );
    }

    /// Regression test for issue #263 (a) — secondary contract: import
    /// upsert must heal a malformed-but-present row by overwriting it,
    /// not bail because the existing data fails to deserialize. The
    /// existence probe is a narrow `SELECT 1` rather than parsing the
    /// row through `get_issue_from_conn` for exactly this reason.
    #[test]
    fn test_upsert_issue_for_import_overwrites_malformed_existing_row() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let stamp = Utc.with_ymd_and_hms(2026, 4, 26, 12, 0, 0).unwrap();
        let issue = make_issue(
            "bd-import-heals-malformed",
            "Original title",
            Status::Open,
            2,
            None,
            stamp,
            None,
        );
        storage.create_issue(&issue, "tester").unwrap();

        // Stomp the persisted status with a value the Status enum
        // doesn't accept. A subsequent import of the same id must
        // still succeed — the existence check stops at SELECT 1.
        storage
            .conn
            .execute_with_params(
                "UPDATE issues SET status = ? WHERE id = ?",
                &[
                    SqliteValue::from("not-a-status"),
                    SqliteValue::from(issue.id.as_str()),
                ],
            )
            .unwrap();

        let mut imported = issue.clone();
        imported.title = "Healed title".to_string();
        imported.updated_at = stamp + chrono::Duration::minutes(1);

        storage
            .upsert_issue_for_import(&imported)
            .expect("import upsert should overwrite the malformed row");

        let updated = storage.get_issue(&issue.id).unwrap().unwrap();
        assert_eq!(updated.title, "Healed title");
        assert_eq!(updated.status, Status::Open);
    }

    /// Regression test for issue #263 (b): a write-probe update that
    /// matches zero rows must be reported as a probe failure, even
    /// when ROLLBACK succeeds. Pre-fix, `Ok(0)` from the probe was
    /// treated as a successful diagnostic — turning the very signal
    /// the probe was built to surface ("the issue isn't write-
    /// addressable through this mutation path") into a false
    /// negative.
    #[test]
    fn test_finish_issue_mutation_write_probe_rejects_zero_row_update() {
        let err = finish_issue_mutation_write_probe(Ok(0), Ok(0))
            .expect_err("zero-row probe must surface as failure");
        assert!(
            err.to_string().contains("write probe did not find issue"),
            "unexpected error: {err}",
        );
    }

    /// Regression test for issue #263 (b): when both the probe update
    /// matches zero rows AND the rollback returns an error, the
    /// zero-row diagnostic remains the primary failure reported up
    /// the stack — a downstream rollback hiccup is noise compared to
    /// "we couldn't update the row we just inserted".
    #[test]
    fn test_finish_issue_mutation_write_probe_prefers_zero_row_over_rollback_error() {
        let err = finish_issue_mutation_write_probe(
            Ok(0),
            Err(FrankenError::Internal("rollback failed".to_string())),
        )
        .expect_err("zero-row probe must outrank rollback error");
        assert!(
            err.to_string().contains("write probe did not find issue"),
            "unexpected error: {err}",
        );
    }

    #[test]
    fn test_pragmas_are_set_correctly() {
        let storage = SqliteStorage::open_memory().unwrap();

        // Check foreign keys are enabled
        assert!(
            SqliteStorage::foreign_keys_enabled(&storage.conn).unwrap(),
            "Foreign keys should be enabled"
        );

        // Check journal mode (memory DBs use 'memory' mode)
        let mode = storage
            .conn
            .query_row("PRAGMA journal_mode")
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_text)
            .unwrap_or("")
            .to_string();
        assert!(
            mode.to_lowercase() == "wal" || mode.to_lowercase() == "memory",
            "Journal mode should be WAL or memory"
        );
    }

    #[test]
    fn test_foreign_key_restore_reports_noop_inside_transaction() {
        let storage = SqliteStorage::open_memory().unwrap();
        storage.conn.execute("PRAGMA foreign_keys = OFF").unwrap();
        storage.conn.execute("BEGIN").unwrap();

        let err = SqliteStorage::finish_foreign_key_suppressed_result(
            &storage.conn,
            "test operation",
            Ok::<(), BeadsError>(()),
        )
        .unwrap_err();

        storage.conn.execute("ROLLBACK").unwrap();
        storage.conn.execute("PRAGMA foreign_keys = ON").unwrap();

        assert!(
            matches!(
                err,
                BeadsError::Config(ref message)
                    if message.contains("test operation") && message.contains("remained OFF")
            ),
            "restore failure should be returned instead of reporting success: {err}"
        );
        assert!(SqliteStorage::foreign_keys_enabled(&storage.conn).unwrap());
    }

    #[test]
    fn test_foreign_key_restore_combines_original_error_with_restore_error() {
        let storage = SqliteStorage::open_memory().unwrap();
        storage.conn.execute("PRAGMA foreign_keys = OFF").unwrap();
        storage.conn.execute("BEGIN").unwrap();

        let err = SqliteStorage::finish_foreign_key_suppressed_result(
            &storage.conn,
            "failing test operation",
            Err::<(), _>(BeadsError::Config("original write failed".to_string())),
        )
        .unwrap_err();

        storage.conn.execute("ROLLBACK").unwrap();
        storage.conn.execute("PRAGMA foreign_keys = ON").unwrap();

        match err {
            BeadsError::WithContext { context, source } => {
                assert!(context.contains("failing test operation"));
                assert!(context.contains("could not be re-enabled"));
                assert!(source.to_string().contains("original write failed"));
            }
            other => {
                assert!(
                    matches!(other, BeadsError::WithContext { .. }),
                    "expected combined WithContext error"
                );
            }
        }
        assert!(SqliteStorage::foreign_keys_enabled(&storage.conn).unwrap());
    }

    #[test]
    fn test_blocked_cache_refresh_restores_foreign_keys_after_rebuild() {
        let storage = SqliteStorage::open_memory().unwrap();
        storage.conn.execute("PRAGMA foreign_keys = OFF").unwrap();

        storage
            .refresh_blocked_cache_after_commit("test refresh", &BlockedCacheRefreshPlan::Full)
            .unwrap();

        assert!(
            SqliteStorage::foreign_keys_enabled(&storage.conn).unwrap(),
            "blocked-cache refresh must restore foreign key enforcement"
        );
    }

    #[test]
    fn test_blocked_cache_refresh_error_with_foreign_keys_off_is_not_deferred() {
        let storage = SqliteStorage::open_memory().unwrap();
        storage.conn.execute("PRAGMA foreign_keys = OFF").unwrap();

        let err = storage
            .handle_blocked_cache_refresh_error(
                "test mutation",
                BeadsError::Config("refresh failed".to_string()),
            )
            .unwrap_err();

        match err {
            BeadsError::WithContext { context, source } => {
                assert!(context.contains("test mutation"));
                assert!(context.contains("foreign key enforcement is OFF"));
                assert!(source.to_string().contains("refresh failed"));
            }
            other => assert!(
                matches!(other, BeadsError::WithContext { .. }),
                "expected WithContext error"
            ),
        }

        storage.conn.execute("PRAGMA foreign_keys = ON").unwrap();
        assert!(SqliteStorage::foreign_keys_enabled(&storage.conn).unwrap());
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
    fn test_set_export_hashes_deduplicates_duplicate_issue_ids_last_value_wins() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap();
        let issue = make_issue("bd-hash-1", "Hash target", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue, "tester").unwrap();

        let inserted = storage
            .set_export_hashes(&[
                ("bd-hash-1".to_string(), "hash-old".to_string()),
                ("bd-hash-1".to_string(), "hash-new".to_string()),
            ])
            .unwrap();

        assert_eq!(
            inserted, 1,
            "duplicate issue IDs should collapse to one row"
        );

        let (content_hash, _) = storage.get_export_hash("bd-hash-1").unwrap().unwrap();
        assert_eq!(content_hash, "hash-new");
    }

    #[test]
    fn test_insert_export_hashes_after_clear_skips_stale_rows() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap();
        for issue_id in ["bd-hash-1", "bd-hash-2"] {
            let issue = make_issue(issue_id, "Hash target", Status::Open, 2, None, t1, None);
            storage.create_issue(&issue, "tester").unwrap();
        }
        storage
            .set_export_hashes(&[
                ("bd-hash-1".to_string(), "stale-a".to_string()),
                ("bd-hash-2".to_string(), "stale-b".to_string()),
            ])
            .unwrap();

        let inserted = storage
            .with_write_transaction(|storage| {
                storage.clear_all_export_hashes_in_tx()?;
                storage.insert_export_hashes_after_clear_in_tx(&[
                    ("bd-hash-1".to_string(), "fresh-a".to_string()),
                    ("bd-hash-1".to_string(), "fresh-a-final".to_string()),
                    ("bd-hash-2".to_string(), "fresh-b".to_string()),
                ])
            })
            .unwrap();

        assert_eq!(inserted, 2);
        assert_eq!(
            storage.get_export_hash("bd-hash-1").unwrap().unwrap().0,
            "fresh-a-final"
        );
        assert_eq!(
            storage.get_export_hash("bd-hash-2").unwrap().unwrap().0,
            "fresh-b"
        );
        let row_count = storage
            .execute_raw_query("SELECT COUNT(*) FROM export_hashes")
            .unwrap()
            .first()
            .and_then(|row| row.first())
            .and_then(SqliteValue::as_integer)
            .unwrap_or(-1);
        assert_eq!(row_count, 2);
    }

    #[test]
    fn test_set_changed_export_hashes_skips_unchanged_rows() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap();
        let issue = make_issue("bd-hash-1", "Hash target", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue, "tester").unwrap();
        storage
            .set_export_hashes(&[("bd-hash-1".to_string(), "hash-stable".to_string())])
            .unwrap();

        let (_, exported_at) = storage.get_export_hash("bd-hash-1").unwrap().unwrap();
        let unchanged = storage
            .set_changed_export_hashes_in_tx(&[(
                "bd-hash-1".to_string(),
                "hash-stable".to_string(),
            )])
            .unwrap();
        assert_eq!(unchanged, 0);
        assert_eq!(
            storage.get_export_hash("bd-hash-1").unwrap().unwrap(),
            ("hash-stable".to_string(), exported_at)
        );

        let changed = storage
            .set_changed_export_hashes_in_tx(&[("bd-hash-1".to_string(), "hash-new".to_string())])
            .unwrap();
        assert_eq!(changed, 1);
        assert_eq!(
            storage.get_export_hash("bd-hash-1").unwrap().unwrap().0,
            "hash-new"
        );
    }

    #[test]
    fn test_set_export_hashes_updates_large_existing_batch() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let created_at = Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap();

        let initial_hashes: Vec<(String, String)> = (0..40)
            .map(|idx| {
                let issue_id = format!("bd-hash-{idx:02}");
                let issue = make_issue(
                    &issue_id,
                    &format!("Hash target {idx}"),
                    Status::Open,
                    2,
                    None,
                    created_at,
                    None,
                );
                storage.create_issue(&issue, "tester").unwrap();
                (issue_id, format!("hash-a-{idx:02}"))
            })
            .collect();

        storage.set_export_hashes(&initial_hashes).unwrap();

        let updated_hashes: Vec<(String, String)> = initial_hashes
            .iter()
            .map(|(issue_id, _)| (issue_id.clone(), format!("hash-b-{issue_id}")))
            .collect();

        let updated = storage.set_export_hashes(&updated_hashes).unwrap();
        assert_eq!(updated, updated_hashes.len());

        let (content_hash, _) = storage
            .get_export_hash("bd-hash-39")
            .unwrap()
            .expect("updated export hash");
        assert_eq!(content_hash, "hash-b-bd-hash-39");
    }

    #[test]
    fn test_set_export_hashes_rewrites_large_mixed_prefix_batch_on_file_db() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let created_at = Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap();

        let issue_ids: Vec<String> = (0..160)
            .map(|idx| {
                let prefix = if idx % 2 == 0 { "bd" } else { "br" };
                format!("{prefix}-hash-{idx:03}")
            })
            .collect();

        let initial_hashes: Vec<(String, String)> = issue_ids
            .iter()
            .map(|issue_id| {
                let issue = make_issue(
                    issue_id,
                    &format!("Hash target {issue_id}"),
                    Status::Open,
                    2,
                    None,
                    created_at,
                    None,
                );
                storage.create_issue(&issue, "tester").unwrap();
                (issue_id.clone(), format!("hash-a-{issue_id}"))
            })
            .collect();

        storage.set_export_hashes(&initial_hashes).unwrap();

        let mut rewritten_hashes: Vec<(String, String)> = issue_ids
            .iter()
            .rev()
            .map(|issue_id| (issue_id.clone(), format!("hash-b-{issue_id}")))
            .collect();
        rewritten_hashes.push(("bd-hash-000".to_string(), "hash-c-bd-hash-000".to_string()));
        rewritten_hashes.push(("br-hash-001".to_string(), "hash-c-br-hash-001".to_string()));

        let updated = storage.set_export_hashes(&rewritten_hashes).unwrap();
        assert_eq!(updated, issue_ids.len());

        let (first_hash, _) = storage
            .get_export_hash("bd-hash-000")
            .unwrap()
            .expect("updated export hash for bd-hash-000");
        assert_eq!(first_hash, "hash-c-bd-hash-000");

        let (second_hash, _) = storage
            .get_export_hash("br-hash-001")
            .unwrap()
            .expect("updated export hash for br-hash-001");
        assert_eq!(second_hash, "hash-c-br-hash-001");

        let row_count = storage
            .execute_raw_query("SELECT COUNT(*) FROM export_hashes")
            .unwrap()
            .first()
            .and_then(|row| row.first())
            .and_then(SqliteValue::as_integer)
            .unwrap_or(-1);
        assert_eq!(row_count, i64::try_from(issue_ids.len()).unwrap_or(-1));
    }

    #[test]
    fn test_rebuild_blocked_cache_rewrites_large_existing_batch_on_file_db() {
        let temp_dir = TempDir::new().unwrap();
        let db_path = temp_dir.path().join("beads.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let created_at = Utc.with_ymd_and_hms(2025, 6, 1, 0, 0, 0).unwrap();

        let issue_pairs: Vec<(String, String)> = (0..160)
            .map(|idx| {
                let prefix = if idx % 2 == 0 { "bd" } else { "br" };
                (
                    format!("{prefix}-blocked-{idx:03}"),
                    format!("{prefix}-blocker-{idx:03}"),
                )
            })
            .collect();

        for (blocked_id, blocker_id) in &issue_pairs {
            let blocked = make_issue(
                blocked_id,
                &format!("Blocked target {blocked_id}"),
                Status::Open,
                2,
                None,
                created_at,
                None,
            );
            let blocker = make_issue(
                blocker_id,
                &format!("Blocking source {blocker_id}"),
                Status::Open,
                2,
                None,
                created_at,
                None,
            );
            storage.create_issue(&blocked, "tester").unwrap();
            storage.create_issue(&blocker, "tester").unwrap();
            storage
                .add_dependency(
                    blocked_id,
                    blocker_id,
                    DependencyType::Blocks.as_str(),
                    "tester",
                )
                .unwrap();
        }

        let rebuilt = storage.rebuild_blocked_cache(true).unwrap();
        assert_eq!(rebuilt, issue_pairs.len());

        for _ in 0..4 {
            let rewritten = storage.rebuild_blocked_cache(true).unwrap();
            assert_eq!(rewritten, issue_pairs.len());
        }

        let row_count = storage
            .execute_raw_query("SELECT COUNT(*) FROM blocked_issues_cache")
            .unwrap()
            .first()
            .and_then(|row| row.first())
            .and_then(SqliteValue::as_integer)
            .unwrap_or(-1);
        assert_eq!(row_count, i64::try_from(issue_pairs.len()).unwrap_or(-1));
    }

    #[test]
    fn test_diag_data_visibility() {
        use fsqlite_types::value::SqliteValue;
        // Simplest possible reproduction
        let conn = fsqlite::Connection::open(":memory:".to_string()).unwrap();
        conn.execute("CREATE TABLE t (k TEXT, v TEXT)").unwrap();
        conn.execute_with_params(
            "INSERT INTO t VALUES (?, ?)",
            &[SqliteValue::from("a"), SqliteValue::from("b")],
        )
        .unwrap();

        // 1: count without WHERE
        let r1 = conn
            .query_with_params("SELECT count(*) FROM t", &[])
            .unwrap();
        eprintln!(
            "[DIAG] 1. count(*) no WHERE: {:?}",
            r1.first().map(fsqlite::Row::values)
        );

        // 2: count with literal WHERE
        let r2 = conn
            .query_with_params("SELECT count(*) FROM t WHERE k = 'a'", &[])
            .unwrap();
        eprintln!(
            "[DIAG] 2. count(*) literal WHERE: {:?}",
            r2.first().map(fsqlite::Row::values)
        );

        // 3: count with bind WHERE
        let explain3 = conn
            .prepare("SELECT count(*) FROM t WHERE k = ?")
            .map_or_else(|e| format!("PREPARE ERROR: {e}"), |s| s.explain());
        for line in explain3.lines() {
            eprintln!("[DIAG] 3.E| {line}");
        }
        if explain3.is_empty() {
            eprintln!("[DIAG] 3.E| (empty)");
        }
        let r3 = conn
            .query_with_params(
                "SELECT count(*) FROM t WHERE k = ?",
                &[SqliteValue::from("a")],
            )
            .unwrap();
        eprintln!(
            "[DIAG] 3. count(*) bind WHERE: {:?}",
            r3.first().map(fsqlite::Row::values)
        );

        // Also get EXPLAIN for the working non-aggregate version
        let explain4 = conn
            .prepare("SELECT k FROM t WHERE k = ?")
            .map_or_else(|e| format!("PREPARE ERROR: {e}"), |s| s.explain());
        for line in explain4.lines() {
            eprintln!("[DIAG] 4.E| {line}");
        }
        if explain4.is_empty() {
            eprintln!("[DIAG] 4.E| (empty)");
        }

        // 4: select with bind WHERE (no aggregate)
        let r4 = conn
            .query_with_params("SELECT k FROM t WHERE k = ?", &[SqliteValue::from("a")])
            .unwrap();
        eprintln!(
            "[DIAG] 4. select k bind WHERE: {:?}",
            r4.first().map(fsqlite::Row::values)
        );

        // 5: count(k) with bind WHERE
        let r5 = conn
            .query_with_params(
                "SELECT count(k) FROM t WHERE k = ?",
                &[SqliteValue::from("a")],
            )
            .unwrap();
        eprintln!(
            "[DIAG] 5. count(k) bind WHERE: {:?}",
            r5.first().map(fsqlite::Row::values)
        );

        // 6: count with bind WHERE but no match
        let r6 = conn
            .query_with_params(
                "SELECT count(*) FROM t WHERE k = ?",
                &[SqliteValue::from("nonexistent")],
            )
            .unwrap();
        eprintln!(
            "[DIAG] 6. count(*) bind WHERE no match: {:?}",
            r6.first().map(fsqlite::Row::values)
        );

        let c = r3
            .first()
            .and_then(|r| r.values().first())
            .and_then(SqliteValue::as_integer)
            .unwrap_or(-99);
        assert_eq!(c, 1, "count(*) with bind param WHERE should return 1");
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn test_diag_root_page_visibility() {
        use fsqlite_types::value::SqliteValue;
        // Create full beads schema and check which root pages are accessible
        let conn = fsqlite::Connection::open(":memory:".to_string()).unwrap();

        // Apply schema step by step, checking after each table
        let tables = vec![(
            "issues",
            r"CREATE TABLE IF NOT EXISTS issues (
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
                CHECK (
                    (status = 'closed' AND closed_at IS NOT NULL) OR
                    (status = 'tombstone') OR
                    (status NOT IN ('closed', 'tombstone') AND closed_at IS NULL)
                )
            )",
        )];
        for (name, sql) in &tables {
            match conn.execute(sql) {
                Ok(_) => eprintln!("[ROOT-DIAG] Created table {name} OK"),
                Err(e) => eprintln!("[ROOT-DIAG] Failed to create table {name}: {e}"),
            }
        }

        // Create first few indexes
        let indexes = vec![
            "CREATE INDEX IF NOT EXISTS idx_issues_status ON issues(status)",
            "CREATE INDEX IF NOT EXISTS idx_issues_priority ON issues(priority)",
            "CREATE INDEX IF NOT EXISTS idx_issues_issue_type ON issues(issue_type)",
            "CREATE INDEX IF NOT EXISTS idx_issues_assignee ON issues(assignee) WHERE assignee IS NOT NULL",
            "CREATE INDEX IF NOT EXISTS idx_issues_created_at ON issues(created_at)",
            "CREATE INDEX IF NOT EXISTS idx_issues_updated_at ON issues(updated_at)",
            "CREATE INDEX IF NOT EXISTS idx_issues_content_hash ON issues(content_hash)",
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_issues_external_ref_unique ON issues(external_ref) WHERE external_ref IS NOT NULL",
            "CREATE INDEX IF NOT EXISTS idx_issues_ephemeral ON issues(ephemeral) WHERE ephemeral = 1",
            "CREATE INDEX IF NOT EXISTS idx_issues_pinned ON issues(pinned) WHERE pinned = 1",
            "CREATE INDEX IF NOT EXISTS idx_issues_tombstone ON issues(status) WHERE status = 'tombstone'",
            "CREATE INDEX IF NOT EXISTS idx_issues_due_at ON issues(due_at) WHERE due_at IS NOT NULL",
            "CREATE INDEX IF NOT EXISTS idx_issues_defer_until ON issues(defer_until) WHERE defer_until IS NOT NULL",
            "CREATE INDEX IF NOT EXISTS idx_issues_ready ON issues(status, priority, created_at) WHERE status = 'open' AND ephemeral = 0 AND pinned = 0 AND (is_template = 0 OR is_template IS NULL)",
            // Issue #354: `br ready` can be configured (workflow.status_groups.ready)
            // to surface statuses beyond `open` (e.g. `rework`). The partial
            // `idx_issues_ready` above only covers `status = 'open'`, so a widened
            // ready group would fall back to a scan on the status leg. The partial
            // predicate is a static migration string and cannot be widened to a
            // per-repo dynamic group, so we add a non-partial `(status, priority,
            // created_at)` index to keep the widened `status IN (...) ORDER BY
            // priority, created_at` ready query index-covered. The tighter partial
            // index still wins for the common default `[open]` group.
            "CREATE INDEX IF NOT EXISTS idx_issues_status_priority_created ON issues(status, priority, created_at)",
        ];
        for (i, sql) in indexes.iter().enumerate() {
            match conn.execute(sql) {
                Ok(_) => eprintln!("[ROOT-DIAG] Created index {} OK", i + 1),
                Err(e) => eprintln!("[ROOT-DIAG] Failed to create index {}: {e}", i + 1),
            }
        }

        // Try count(*) first (simplest possible query)
        match conn.query_with_params("SELECT count(*) FROM sqlite_master", &[]) {
            Ok(rows) => {
                let count = rows
                    .first()
                    .and_then(|r| r.values().first())
                    .and_then(SqliteValue::as_integer)
                    .unwrap_or(-99);
                eprintln!("[ROOT-DIAG] count(*) from sqlite_master: {count}");
            }
            Err(e) => eprintln!("[ROOT-DIAG] count(*) FAILED: {e}"),
        }

        // Try SELECT without ORDER BY
        match conn.query_with_params("SELECT type, name, rootpage FROM sqlite_master", &[]) {
            Ok(rows) => {
                eprintln!("[ROOT-DIAG] sqlite_master entries (no ORDER BY):");
                for row in &rows {
                    let vals = row.values();
                    let typ = vals.first().map(|v| format!("{v:?}")).unwrap_or_default();
                    let name = vals.get(1).map(|v| format!("{v:?}")).unwrap_or_default();
                    let rootpage = vals.get(2).and_then(SqliteValue::as_integer).unwrap_or(0);
                    eprintln!("[ROOT-DIAG]   type={typ} name={name} rootpage={rootpage}");
                }
            }
            Err(e) => eprintln!("[ROOT-DIAG] SELECT (no ORDER BY) FAILED: {e}"),
        }

        // Try SELECT with ORDER BY
        match conn.query_with_params(
            "SELECT type, name, rootpage FROM sqlite_master ORDER BY rootpage",
            &[],
        ) {
            Ok(rows) => {
                eprintln!("[ROOT-DIAG] sqlite_master entries (ORDER BY):");
                for row in &rows {
                    let vals = row.values();
                    let rootpage = vals.get(2).and_then(SqliteValue::as_integer).unwrap_or(0);
                    eprintln!("[ROOT-DIAG]   rootpage={rootpage}");
                }
            }
            Err(e) => eprintln!("[ROOT-DIAG] SELECT (ORDER BY) FAILED: {e}"),
        }

        // Try simple SELECT from issues table
        match conn.query_with_params("SELECT count(*) FROM issues", &[]) {
            Ok(rows) => {
                let count = rows
                    .first()
                    .and_then(|r| r.values().first())
                    .and_then(SqliteValue::as_integer)
                    .unwrap_or(-99);
                eprintln!("[ROOT-DIAG] count(*) from issues: {count}");
            }
            Err(e) => eprintln!("[ROOT-DIAG] count(*) from issues FAILED: {e}"),
        }

        let max_rootpage = 0i64;

        // Also try: incrementally create indexes and check count(*) after each
        eprintln!("[ROOT-DIAG] --- Incremental index creation with count check ---");
        let conn2 = fsqlite::Connection::open(":memory:".to_string()).unwrap();
        conn2
            .execute("CREATE TABLE t (a TEXT, b TEXT, c TEXT, d TEXT, e TEXT)")
            .unwrap();
        for i in 1..=20 {
            let col = ['a', 'b', 'c', 'd', 'e'][i % 5];
            let sql = format!("CREATE INDEX IF NOT EXISTS idx_{i} ON t({col})");
            match conn2.execute(&sql) {
                Ok(_) => {}
                Err(e) => {
                    eprintln!("[ROOT-DIAG] Index {i} creation FAILED: {e}");
                    break;
                }
            }
            match conn2.query_with_params("SELECT count(*) FROM sqlite_master", &[]) {
                Ok(rows) => {
                    let count = rows
                        .first()
                        .and_then(|r| r.values().first())
                        .and_then(SqliteValue::as_integer)
                        .unwrap_or(-99);
                    eprintln!("[ROOT-DIAG] After {i} indexes: count(*)={count}");
                }
                Err(e) => {
                    eprintln!("[ROOT-DIAG] After {i} indexes: count(*) FAILED: {e}");
                    break;
                }
            }
        }

        // Test multi-insert with explicit transactions
        eprintln!("[ROOT-DIAG] --- Multi-insert test ---");
        let conn3 = fsqlite::Connection::open(":memory:".to_string()).unwrap();
        conn3
            .execute("CREATE TABLE ev (id INTEGER PRIMARY KEY AUTOINCREMENT, msg TEXT)")
            .unwrap();
        for i in 0..5 {
            conn3.execute("BEGIN IMMEDIATE").unwrap();
            conn3
                .execute_with_params(
                    "INSERT INTO ev (msg) VALUES (?)",
                    &[SqliteValue::from(format!("msg{i}"))],
                )
                .unwrap();
            conn3.execute("COMMIT").unwrap();
        }
        let rows3 = conn3
            .query_with_params("SELECT count(*) FROM ev", &[])
            .unwrap();
        let count3 = rows3
            .first()
            .and_then(|r| r.values().first())
            .and_then(SqliteValue::as_integer)
            .unwrap_or(-99);
        eprintln!("[ROOT-DIAG] Multi-insert count: {count3} (expected 5)");

        let all3 = conn3
            .query_with_params("SELECT id, msg FROM ev", &[])
            .unwrap();
        for row in &all3 {
            let id = row
                .values()
                .first()
                .and_then(SqliteValue::as_integer)
                .unwrap_or(-1);
            let msg = row
                .values()
                .get(1)
                .map(|v| format!("{v:?}"))
                .unwrap_or_default();
            eprintln!("[ROOT-DIAG]   id={id} msg={msg}");
        }

        // Also test without explicit transactions (autocommit)
        let conn4 = fsqlite::Connection::open(":memory:".to_string()).unwrap();
        conn4
            .execute("CREATE TABLE ev2 (id INTEGER PRIMARY KEY AUTOINCREMENT, msg TEXT)")
            .unwrap();
        for i in 0..5 {
            conn4
                .execute_with_params(
                    "INSERT INTO ev2 (msg) VALUES (?)",
                    &[SqliteValue::from(format!("msg{i}"))],
                )
                .unwrap();
        }
        let rows4 = conn4
            .query_with_params("SELECT count(*) FROM ev2", &[])
            .unwrap();
        let count4 = rows4
            .first()
            .and_then(|r| r.values().first())
            .and_then(SqliteValue::as_integer)
            .unwrap_or(-99);
        eprintln!("[ROOT-DIAG] Multi-insert (autocommit) count: {count4} (expected 5)");

        let all4 = conn4
            .query_with_params("SELECT id, msg FROM ev2", &[])
            .unwrap();
        for row in &all4 {
            let id = row
                .values()
                .first()
                .and_then(SqliteValue::as_integer)
                .unwrap_or(-1);
            let msg = row
                .values()
                .get(1)
                .map(|v| format!("{v:?}"))
                .unwrap_or_default();
            eprintln!("[ROOT-DIAG]   id={id} msg={msg}");
        }

        // Test events-like table with indexes and WHERE+ORDER BY
        eprintln!("[ROOT-DIAG] --- Events-like test ---");
        let conn5 = fsqlite::Connection::open(":memory:".to_string()).unwrap();
        conn5
            .execute("CREATE TABLE issues2 (id TEXT PRIMARY KEY, title TEXT)")
            .unwrap();
        conn5.execute("CREATE TABLE ev3 (id INTEGER PRIMARY KEY AUTOINCREMENT, issue_id TEXT NOT NULL, msg TEXT, created_at TEXT, FOREIGN KEY (issue_id) REFERENCES issues2(id))").unwrap();
        conn5
            .execute("CREATE INDEX idx_ev3_issue ON ev3(issue_id)")
            .unwrap();
        conn5
            .execute("CREATE INDEX idx_ev3_created ON ev3(created_at)")
            .unwrap();
        conn5
            .execute("INSERT INTO issues2 (id, title) VALUES ('test-001', 'Test')")
            .unwrap();

        for i in 0..5 {
            conn5.execute("BEGIN IMMEDIATE").unwrap();
            conn5
                .execute_with_params(
                    "INSERT INTO ev3 (issue_id, msg, created_at) VALUES (?1, ?2, ?3)",
                    &[
                        SqliteValue::from("test-001"),
                        SqliteValue::from(format!("msg{i}")),
                        SqliteValue::from(format!("2024-01-0{} 00:00:00", i + 1)),
                    ],
                )
                .unwrap();
            conn5.execute("COMMIT").unwrap();
        }

        // Test count
        let ev_count = conn5
            .query_with_params("SELECT count(*) FROM ev3", &[])
            .unwrap();
        let c = ev_count
            .first()
            .and_then(|r| r.values().first())
            .and_then(SqliteValue::as_integer)
            .unwrap_or(-99);
        eprintln!("[ROOT-DIAG] ev3 count: {c}");

        // Test WHERE with bind (no order) - uses index_eq path
        let ev_where = conn5
            .query_with_params(
                "SELECT id, msg FROM ev3 WHERE issue_id = ?1",
                &[SqliteValue::from("test-001")],
            )
            .unwrap();
        eprintln!("[ROOT-DIAG] ev3 WHERE bind: {} rows", ev_where.len());

        // Test WHERE with literal (no bind) - uses full scan
        let ev_literal = conn5
            .query_with_params("SELECT id, msg FROM ev3 WHERE issue_id = 'test-001'", &[])
            .unwrap();
        eprintln!("[ROOT-DIAG] ev3 WHERE literal: {} rows", ev_literal.len());

        // Test full scan (no WHERE)
        let ev_all = conn5
            .query_with_params("SELECT id, msg FROM ev3", &[])
            .unwrap();
        eprintln!("[ROOT-DIAG] ev3 ALL (no where): {} rows", ev_all.len());

        // Test WHERE with ORDER BY
        let ev_ordered = conn5
            .query_with_params(
                "SELECT id, msg FROM ev3 WHERE issue_id = ?1 ORDER BY created_at DESC, id DESC",
                &[SqliteValue::from("test-001")],
            )
            .unwrap();
        eprintln!("[ROOT-DIAG] ev3 WHERE+ORDER: {} rows", ev_ordered.len());
        for row in &ev_ordered {
            let id = row
                .values()
                .first()
                .and_then(SqliteValue::as_integer)
                .unwrap_or(-1);
            let msg = row
                .values()
                .get(1)
                .map(|v| format!("{v:?}"))
                .unwrap_or_default();
            eprintln!("[ROOT-DIAG]   id={id} msg={msg}");
        }

        assert!(max_rootpage >= 0, "diagnostic test completed");
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
    fn test_list_issues_custom_sort_limit_uses_id_tiebreaker() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let created_at = Utc.with_ymd_and_hms(2025, 8, 4, 0, 0, 0).unwrap();

        for issue in [
            make_issue(
                "bd-list-b",
                "Tie B",
                Status::Open,
                1,
                None,
                created_at,
                None,
            ),
            make_issue(
                "bd-list-a",
                "Tie A",
                Status::Open,
                1,
                None,
                created_at,
                None,
            ),
            make_issue(
                "bd-list-c",
                "Later",
                Status::Open,
                2,
                None,
                created_at - chrono::Duration::days(1),
                None,
            ),
        ] {
            storage.create_issue(&issue, "tester").unwrap();
        }

        let issues = storage
            .list_issues(&ListFilters {
                sort: Some("created".to_string()),
                limit: Some(2),
                ..ListFilters::default()
            })
            .unwrap();
        let ids: Vec<_> = issues.iter().map(|issue| issue.id.as_str()).collect();

        assert_eq!(ids, vec!["bd-list-a", "bd-list-b"]);
    }

    #[test]
    fn test_list_changelog_issues_matches_closed_list_projection() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 8, 1, 0, 0, 0).unwrap();
        let t2 = Utc.with_ymd_and_hms(2025, 8, 2, 0, 0, 0).unwrap();
        let t3 = Utc.with_ymd_and_hms(2025, 8, 3, 0, 0, 0).unwrap();

        let mut closed_low = make_issue("bd-low", "Low", Status::Closed, 2, None, t1, None);
        closed_low.closed_at = Some(t3);
        let mut closed_high_new =
            make_issue("bd-high-new", "High New", Status::Closed, 1, None, t3, None);
        closed_high_new.closed_at = Some(t3);
        let mut closed_high_old =
            make_issue("bd-high-old", "High Old", Status::Closed, 1, None, t2, None);
        closed_high_old.closed_at = Some(t2);
        let open_issue = make_issue("bd-open", "Open", Status::Open, 0, None, t3, None);
        let mut template = make_issue("bd-template", "Template", Status::Closed, 0, None, t3, None);
        template.closed_at = Some(t3);
        template.is_template = true;

        for issue in [
            closed_low,
            closed_high_new,
            closed_high_old,
            open_issue,
            template,
        ] {
            storage.create_issue(&issue, "tester").unwrap();
        }

        let filters = ListFilters {
            statuses: Some(vec![Status::Closed]),
            include_closed: true,
            ..ListFilters::default()
        };
        let expected: Vec<_> = storage
            .list_issues(&filters)
            .unwrap()
            .into_iter()
            .map(|issue| ChangelogIssueRow {
                id: issue.id,
                title: issue.title,
                priority: issue.priority,
                issue_type: issue.issue_type,
                created_at: issue.created_at,
                closed_at: issue.closed_at,
            })
            .collect();

        let actual = storage.list_changelog_issues().unwrap();

        assert_eq!(
            actual
                .iter()
                .map(|issue| issue.id.as_str())
                .collect::<Vec<_>>(),
            vec!["bd-high-new", "bd-high-old", "bd-low"]
        );
        assert_eq!(actual, expected);
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
    fn test_search_issues_matches_case_insensitive_literal_substrings() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 9, 1, 0, 0, 0).unwrap();

        let literal_issue = make_issue(
            "bd-s-literal",
            "Literal %_ marker",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        let mut description_issue = make_issue(
            "bd-s-description",
            "Description target",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        description_issue.description = Some("Uppercase AUTHENTICATION token".to_string());

        storage.create_issue(&literal_issue, "tester").unwrap();
        storage.create_issue(&description_issue, "tester").unwrap();

        let filters = ListFilters::default();
        let wildcard_results = storage.search_issues("%_", &filters).unwrap();
        let wildcard_ids: Vec<_> = wildcard_results
            .iter()
            .map(|issue| issue.id.as_str())
            .collect();
        assert_eq!(wildcard_ids, vec!["bd-s-literal"]);

        let case_results = storage.search_issues("authentication", &filters).unwrap();
        let case_ids: Vec<_> = case_results.iter().map(|issue| issue.id.as_str()).collect();
        assert_eq!(case_ids, vec!["bd-s-description"]);
    }

    #[test]
    fn test_search_issues_materialized_label_candidates_preserve_semantics() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 9, 1, 0, 0, 0).unwrap();

        let mut export_swarm = make_issue(
            "bd-s-label-a",
            "needle export swarm",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        export_swarm.labels = vec!["export".to_string(), "swarm".to_string()];
        let mut export_only = make_issue(
            "bd-s-label-b",
            "needle export only",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        export_only.labels = vec!["export".to_string()];
        let mut swarm_only = make_issue(
            "bd-s-label-c",
            "needle swarm only",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        swarm_only.labels = vec!["swarm".to_string()];
        let mut unrelated = make_issue(
            "bd-s-label-d",
            "needle unrelated",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        unrelated.labels = vec!["other".to_string()];

        for issue in [export_swarm, export_only, swarm_only, unrelated] {
            storage.create_issue(&issue, "tester").unwrap();
        }

        let ids_for = |filters: ListFilters| {
            storage
                .search_issues("needle", &filters)
                .unwrap()
                .into_iter()
                .map(|issue| issue.id)
                .collect::<Vec<_>>()
        };

        assert_eq!(
            ids_for(ListFilters {
                labels: Some(vec!["export".to_string(), "swarm".to_string()]),
                ..ListFilters::default()
            }),
            vec!["bd-s-label-a"]
        );
        assert_eq!(
            ids_for(ListFilters {
                labels_or: Some(vec!["export".to_string(), "swarm".to_string()]),
                ..ListFilters::default()
            }),
            vec!["bd-s-label-a", "bd-s-label-b", "bd-s-label-c"]
        );
        assert_eq!(
            ids_for(ListFilters {
                labels: Some(vec!["export".to_string()]),
                labels_or: Some(vec!["swarm".to_string()]),
                ..ListFilters::default()
            }),
            vec!["bd-s-label-a"]
        );
        assert!(
            ids_for(ListFilters {
                labels: Some(vec!["missing".to_string()]),
                ..ListFilters::default()
            })
            .is_empty()
        );
    }

    #[test]
    fn test_redundant_single_label_fast_path_preserves_list_and_search_results() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 9, 1, 0, 0, 0).unwrap();

        let mut visible_a = make_issue(
            "bd-s-cover-a",
            "needle global alpha",
            Status::Open,
            1,
            None,
            t1,
            None,
        );
        visible_a.labels = vec!["global".to_string()];
        let mut visible_b = make_issue(
            "bd-s-cover-b",
            "needle global beta",
            Status::Open,
            2,
            None,
            t1 + chrono::Duration::minutes(1),
            None,
        );
        visible_b.labels = vec!["global".to_string()];
        let deferred_unlabeled = make_issue(
            "bd-s-cover-c",
            "needle deferred",
            Status::Deferred,
            0,
            None,
            t1 + chrono::Duration::minutes(2),
            None,
        );

        for issue in [visible_a, visible_b, deferred_unlabeled] {
            storage.create_issue(&issue, "tester").unwrap();
        }

        assert!(
            storage
                .single_label_covers_default_visible_issues(false, "global")
                .unwrap()
        );
        assert!(
            !storage
                .single_label_covers_default_visible_issues(true, "global")
                .unwrap()
        );

        let no_label_filters = ListFilters::default();
        let global_label_filters = ListFilters {
            labels: Some(vec!["global".to_string()]),
            ..ListFilters::default()
        };

        let broad_candidate_ids =
            vec!["bd-s-cover-a".to_string(); REDUNDANT_LABEL_COVERAGE_MIN_CANDIDATES];
        assert!(
            storage
                .redundant_default_visible_single_label_filter(
                    &global_label_filters,
                    Some(&broad_candidate_ids),
                )
                .unwrap()
        );
        let narrow_candidate_ids =
            vec!["bd-s-cover-a".to_string(); REDUNDANT_LABEL_COVERAGE_MIN_CANDIDATES - 1];
        assert!(
            !storage
                .redundant_default_visible_single_label_filter(
                    &global_label_filters,
                    Some(&narrow_candidate_ids),
                )
                .unwrap()
        );

        let no_label_list_ids = issue_ids(storage.list_issues(&no_label_filters).unwrap());
        let global_label_list_ids = issue_ids(storage.list_issues(&global_label_filters).unwrap());
        assert_eq!(global_label_list_ids, no_label_list_ids);

        let no_label_search_ids =
            issue_ids(storage.search_issues("needle", &no_label_filters).unwrap());
        let global_label_search_ids = issue_ids(
            storage
                .search_issues("needle", &global_label_filters)
                .unwrap(),
        );
        assert_eq!(global_label_search_ids, no_label_search_ids);

        let include_deferred_global_label_ids = issue_ids(
            storage
                .search_issues(
                    "needle",
                    &ListFilters {
                        include_deferred: true,
                        labels: Some(vec!["global".to_string()]),
                        ..ListFilters::default()
                    },
                )
                .unwrap(),
        );
        assert_eq!(
            include_deferred_global_label_ids,
            vec!["bd-s-cover-a", "bd-s-cover-b"]
        );
    }

    #[test]
    fn test_search_issues_for_command_output_matches_text_fields() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 9, 1, 0, 0, 0).unwrap();

        let mut description_match = make_issue(
            "bd-search-proj-a",
            "Command projection alpha",
            Status::Open,
            1,
            Some("agent-a"),
            t1,
            None,
        );
        description_match.description =
            Some("description carries projection needle context".to_string());
        description_match.design = Some("unused design".repeat(512));
        description_match.acceptance_criteria = Some("unused ac".repeat(512));
        description_match.notes = Some("unused notes".repeat(512));
        description_match.owner = Some("owner".to_string());
        description_match.sender = Some("cli".to_string());

        let title_match = make_issue(
            "bd-search-proj-b",
            "Projection needle title",
            Status::Open,
            2,
            Some("agent-b"),
            t1 + chrono::Duration::minutes(1),
            None,
        );

        storage.create_issue(&description_match, "tester").unwrap();
        storage.create_issue(&title_match, "tester").unwrap();

        let filters = ListFilters {
            limit: Some(0),
            ..ListFilters::default()
        };
        let full = storage
            .search_issues("projection needle", &filters)
            .unwrap();
        let projected = storage
            .search_issues_for_command_output("projection needle", &filters)
            .unwrap();

        let full_summary = full
            .iter()
            .map(|issue| {
                (
                    issue.id.as_str(),
                    issue.title.as_str(),
                    issue.description.as_deref(),
                    issue.status.clone(),
                    issue.priority,
                    issue.issue_type.clone(),
                    issue.assignee.as_deref(),
                    issue.created_at,
                    issue.updated_at,
                )
            })
            .collect::<Vec<_>>();
        let projected_summary = projected
            .iter()
            .map(|issue| {
                (
                    issue.id.as_str(),
                    issue.title.as_str(),
                    issue.description.as_deref(),
                    issue.status.clone(),
                    issue.priority,
                    issue.issue_type.clone(),
                    issue.assignee.as_deref(),
                    issue.created_at,
                    issue.updated_at,
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(projected_summary, full_summary);

        let full_lines = full
            .iter()
            .map(|issue| format_issue_line_with(issue, TextFormatOptions::plain()))
            .collect::<Vec<_>>();
        let projected_lines = projected
            .iter()
            .map(|issue| format_issue_line_with(issue, TextFormatOptions::plain()))
            .collect::<Vec<_>>();
        assert_eq!(projected_lines, full_lines);

        let projected_description_match = projected
            .iter()
            .find(|issue| issue.id == "bd-search-proj-a")
            .unwrap();
        assert!(projected_description_match.description.is_some());
        assert!(projected_description_match.design.is_none());
        assert!(projected_description_match.acceptance_criteria.is_none());
        assert!(projected_description_match.notes.is_none());
        assert!(projected_description_match.owner.is_none());
        assert!(projected_description_match.sender.is_none());
    }

    #[test]
    fn test_search_issues_respects_include_deferred_flag() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 9, 1, 0, 0, 0).unwrap();

        let open_issue = make_issue(
            "bd-s-open",
            "authentication flow update",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        let deferred_issue = make_issue(
            "bd-s-deferred",
            "authentication flow deferred follow-up",
            Status::Deferred,
            2,
            None,
            t1,
            None,
        );

        storage.create_issue(&open_issue, "tester").unwrap();
        storage.create_issue(&deferred_issue, "tester").unwrap();

        let filters = ListFilters {
            include_deferred: false,
            ..ListFilters::default()
        };
        let results = storage.search_issues("authentication", &filters).unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "bd-s-open");
    }

    #[test]
    fn test_search_issues_orders_by_updated() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 9, 1, 0, 0, 0).unwrap();
        let t3 = Utc.with_ymd_and_hms(2025, 9, 3, 0, 0, 0).unwrap();

        let older_updated = make_issue(
            "bd-s-sort-a",
            "authentication alpha",
            Status::Open,
            2,
            None,
            t3,
            None,
        );
        let newer_updated = make_issue(
            "bd-s-sort-b",
            "authentication beta",
            Status::Open,
            2,
            None,
            t1,
            None,
        );

        storage.create_issue(&older_updated, "tester").unwrap();
        storage.create_issue(&newer_updated, "tester").unwrap();
        storage
            .execute_test_sql(&format!(
                "UPDATE issues SET updated_at = '{}' WHERE id = 'bd-s-sort-a';\n\
                 UPDATE issues SET updated_at = '{}' WHERE id = 'bd-s-sort-b';",
                t1.to_rfc3339(),
                t3.to_rfc3339()
            ))
            .unwrap();

        let results = storage
            .search_issues(
                "authentication",
                &ListFilters {
                    sort: Some("updated".to_string()),
                    ..ListFilters::default()
                },
            )
            .unwrap();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].id, "bd-s-sort-b");
    }

    #[test]
    fn test_search_issues_applies_offset() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 9, 1, 0, 0, 0).unwrap();

        for (idx, id) in ["bd-s-page-a", "bd-s-page-b", "bd-s-page-c"]
            .into_iter()
            .enumerate()
        {
            let issue = make_issue(
                id,
                &format!("authentication page {idx}"),
                Status::Open,
                i32::try_from(idx + 1).unwrap(),
                None,
                t1 + chrono::Duration::minutes(i64::try_from(idx).unwrap()),
                None,
            );
            storage.create_issue(&issue, "tester").unwrap();
        }

        let results = storage
            .search_issues(
                "authentication",
                &ListFilters {
                    limit: Some(1),
                    offset: Some(1),
                    ..ListFilters::default()
                },
            )
            .unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "bd-s-page-b");

        let all_after_offset = storage
            .search_issues(
                "authentication",
                &ListFilters {
                    limit: Some(0),
                    offset: Some(2),
                    ..ListFilters::default()
                },
            )
            .unwrap();

        let ids: Vec<_> = all_after_offset
            .iter()
            .map(|issue| issue.id.as_str())
            .collect();
        assert_eq!(ids, vec!["bd-s-page-c"]);
    }

    #[test]
    fn test_search_issues_limit_uses_id_tiebreaker() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let created_at = Utc.with_ymd_and_hms(2025, 9, 4, 0, 0, 0).unwrap();

        for issue in [
            make_issue(
                "bd-search-b",
                "authentication tie b",
                Status::Open,
                1,
                None,
                created_at,
                None,
            ),
            make_issue(
                "bd-search-a",
                "authentication tie a",
                Status::Open,
                1,
                None,
                created_at,
                None,
            ),
            make_issue(
                "bd-search-c",
                "authentication later",
                Status::Open,
                2,
                None,
                created_at - chrono::Duration::days(1),
                None,
            ),
        ] {
            storage.create_issue(&issue, "tester").unwrap();
        }

        let results = storage
            .search_issues(
                "authentication",
                &ListFilters {
                    limit: Some(2),
                    ..ListFilters::default()
                },
            )
            .unwrap();
        let ids: Vec<_> = results.iter().map(|issue| issue.id.as_str()).collect();

        assert_eq!(ids, vec!["bd-search-a", "bd-search-b"]);
    }

    #[test]
    fn test_search_issues_default_visible_limited_page_matches_generic_order() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let base = Utc.with_ymd_and_hms(2025, 9, 4, 0, 0, 0).unwrap();

        let mut closed = make_issue(
            "bd-search-closed",
            "needle closed",
            Status::Closed,
            0,
            None,
            base + chrono::Duration::minutes(10),
            None,
        );
        closed.closed_at = Some(base + chrono::Duration::minutes(11));
        let mut template = make_issue(
            "bd-search-template",
            "needle template",
            Status::Open,
            0,
            None,
            base + chrono::Duration::minutes(9),
            None,
        );
        template.is_template = true;

        for issue in [
            make_issue(
                "bd-search-p1",
                "needle p1",
                Status::Open,
                1,
                None,
                base + chrono::Duration::minutes(4),
                None,
            ),
            make_issue(
                "bd-search-p0-old",
                "needle p0 old",
                Status::Open,
                0,
                None,
                base + chrono::Duration::minutes(1),
                None,
            ),
            make_issue(
                "bd-search-deferred",
                "needle deferred",
                Status::Deferred,
                0,
                None,
                base + chrono::Duration::minutes(3),
                None,
            ),
            make_issue(
                "bd-search-p0-new",
                "needle p0 new",
                Status::Open,
                0,
                None,
                base + chrono::Duration::minutes(2),
                None,
            ),
            make_issue(
                "bd-search-nomatch",
                "other",
                Status::Open,
                0,
                None,
                base + chrono::Duration::minutes(5),
                None,
            ),
            closed,
            template,
        ] {
            storage.create_issue(&issue, "tester").unwrap();
        }

        let fast_filters = ListFilters {
            include_deferred: true,
            limit: Some(3),
            ..ListFilters::default()
        };
        let generic_filters = ListFilters {
            include_deferred: true,
            limit: Some(3),
            sort: Some("priority".to_string()),
            ..ListFilters::default()
        };

        let fast_ids = issue_ids(storage.search_issues("needle", &fast_filters).unwrap());
        let generic_ids = issue_ids(storage.search_issues("needle", &generic_filters).unwrap());

        assert_eq!(fast_ids, generic_ids);
        assert_eq!(
            fast_ids,
            vec!["bd-search-deferred", "bd-search-p0-new", "bd-search-p0-old",]
        );

        let fast_no_match = storage.search_issues("absent", &fast_filters).unwrap();
        let generic_no_match = storage.search_issues("absent", &generic_filters).unwrap();

        assert_eq!(fast_no_match, generic_no_match);
        assert!(fast_no_match.is_empty());
    }

    #[test]
    fn test_search_issues_filter_by_updated_date() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let now = Utc::now();
        let old = now - chrono::Duration::days(10);
        let older = now - chrono::Duration::days(20);

        let issue1 = make_issue(
            "bd-search-old",
            "authentication old",
            Status::Open,
            2,
            None,
            old,
            None,
        );
        let issue2 = make_issue(
            "bd-search-older",
            "authentication older",
            Status::Open,
            2,
            None,
            older,
            None,
        );
        let issue3 = make_issue(
            "bd-search-new",
            "authentication new",
            Status::Open,
            2,
            None,
            now,
            None,
        );

        storage.create_issue(&issue1, "tester").unwrap();
        storage.create_issue(&issue2, "tester").unwrap();
        storage.create_issue(&issue3, "tester").unwrap();

        let mut filters = ListFilters {
            updated_before: Some(old),
            ..Default::default()
        };

        let issues = storage.search_issues("authentication", &filters).unwrap();
        let ids: Vec<_> = issues.iter().map(|issue| issue.id.as_str()).collect();
        assert_eq!(issues.len(), 2);
        assert!(ids.contains(&"bd-search-old"));
        assert!(ids.contains(&"bd-search-older"));
        assert!(!ids.contains(&"bd-search-new"));

        filters.updated_before = None;
        filters.updated_after = Some(old);

        let issues = storage.search_issues("authentication", &filters).unwrap();
        let ids: Vec<_> = issues.iter().map(|issue| issue.id.as_str()).collect();
        assert_eq!(issues.len(), 2);
        assert!(ids.contains(&"bd-search-old"));
        assert!(ids.contains(&"bd-search-new"));
        assert!(!ids.contains(&"bd-search-older"));
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
    fn test_list_issues_filter_by_multiple_labels_uses_and_logic() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc::now();

        let issue1 = make_issue("bd-l3", "Core only", Status::Open, 2, None, t1, None);
        let issue2 = make_issue(
            "bd-l4",
            "Core and frontend",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&issue1, "tester").unwrap();
        storage.create_issue(&issue2, "tester").unwrap();

        storage.add_label("bd-l3", "core", "tester").unwrap();
        storage.add_label("bd-l4", "core", "tester").unwrap();
        storage.add_label("bd-l4", "frontend", "tester").unwrap();

        let filters = ListFilters {
            labels: Some(vec!["core".to_string(), "frontend".to_string()]),
            ..Default::default()
        };

        let issues = storage.list_issues(&filters).unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].id, "bd-l4");

        let duplicate_filters = ListFilters {
            labels: Some(vec![
                "core".to_string(),
                "frontend".to_string(),
                "frontend".to_string(),
            ]),
            ..Default::default()
        };

        let duplicate_issues = storage.list_issues(&duplicate_filters).unwrap();
        assert_eq!(duplicate_issues.len(), 1);
        assert_eq!(duplicate_issues[0].id, "bd-l4");
        assert_eq!(
            storage
                .count_issues_with_filters(&duplicate_filters)
                .unwrap(),
            1
        );
    }

    #[test]
    fn test_list_issues_combined_type_and_label_filters() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc::now();

        let task_issue = make_issue("bd-l5", "Core task", Status::Open, 1, None, t1, None);
        let mut feature_issue =
            make_issue("bd-l6", "Core feature", Status::Open, 1, None, t1, None);
        feature_issue.issue_type = IssueType::Feature;

        storage.create_issue(&task_issue, "tester").unwrap();
        storage.create_issue(&feature_issue, "tester").unwrap();

        storage.add_label("bd-l5", "core", "tester").unwrap();
        storage.add_label("bd-l6", "core", "tester").unwrap();

        let filters = ListFilters {
            types: Some(vec![IssueType::Task]),
            labels: Some(vec!["core".to_string()]),
            ..Default::default()
        };

        let issues = storage.list_issues(&filters).unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].id, "bd-l5");
        assert_eq!(storage.count_issues_with_filters(&filters).unwrap(), 1);
    }

    #[test]
    fn test_list_issues_materialized_label_candidates_match_filtered_fallback() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc::now();

        for id in ["bd-lj1", "bd-lj2", "bd-lj3", "bd-lj4"] {
            let issue = make_issue(id, id, Status::Open, 2, None, t1, None);
            storage.create_issue(&issue, "tester").unwrap();
        }

        for (issue_id, labels) in [
            ("bd-lj1", ["export", "lane-00", "swarm"].as_slice()),
            ("bd-lj2", ["export", "lane-01"].as_slice()),
            ("bd-lj3", ["lane-00", "swarm"].as_slice()),
            ("bd-lj4", ["export", "swarm"].as_slice()),
        ] {
            for label in labels {
                storage.add_label(issue_id, label, "tester").unwrap();
            }
        }

        let fast_filters = ListFilters {
            labels: Some(vec!["export".to_string()]),
            limit: Some(2),
            ..Default::default()
        };
        let fallback_filters = ListFilters {
            types: Some(vec![IssueType::Task]),
            ..fast_filters.clone()
        };

        let fast_ids: Vec<_> = storage
            .list_issues(&fast_filters)
            .unwrap()
            .into_iter()
            .map(|issue| issue.id)
            .collect();
        let fallback_ids: Vec<_> = storage
            .list_issues(&fallback_filters)
            .unwrap()
            .into_iter()
            .map(|issue| issue.id)
            .collect();

        assert_eq!(fast_ids, vec!["bd-lj1", "bd-lj2"]);
        assert_eq!(fast_ids, fallback_ids);
        assert_eq!(
            storage.count_issues_with_filters(&fast_filters).unwrap(),
            storage
                .count_issues_with_filters(&fallback_filters)
                .unwrap()
        );
        assert_eq!(storage.count_issues_with_filters(&fast_filters).unwrap(), 3);
    }

    #[test]
    fn test_blocked_cache_handles_quotes_in_ids() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc::now();

        let issue = make_issue("bd-x1", "Blocked", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue, "tester").unwrap();

        // Add a dependency on an ID containing a quote, as could exist in a
        // legacy/corrupt database. `create_issue` now rejects invalid IDs, so
        // seed the low-level row directly through the import upsert path.
        let tricky_id = "bd-q\"ote";
        let tricky_issue = make_issue(tricky_id, "Tricky", Status::Open, 2, None, t1, None);
        storage.upsert_issue_for_import(&tricky_issue).unwrap();
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
        // ID + ":open" (since the tricky issue is open)
        assert_eq!(blockers[0], "bd-q\"ote:open");
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

    #[test]
    fn test_ready_default_group_is_open_only() {
        // #354: with no ready_statuses configured, the query behaves exactly as
        // before — only `open` issues surface, `rework`/`in_progress` do not.
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc::now();

        storage
            .create_issue(
                &make_issue("bd-open", "Open", Status::Open, 2, None, t1, None),
                "tester",
            )
            .unwrap();
        storage
            .create_issue(
                &make_issue(
                    "bd-rework",
                    "Rework",
                    Status::Custom("rework".to_string()),
                    2,
                    None,
                    t1,
                    None,
                ),
                "tester",
            )
            .unwrap();

        let filters = ReadyFilters::default();
        let res = storage
            .get_ready_issues(&filters, ReadySortPolicy::Oldest)
            .unwrap();
        let ids: Vec<&str> = res.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["bd-open"], "default group must be [open] only");
    }

    #[test]
    fn test_ready_configured_group_surfaces_rework() {
        // #354: a configured ready group [open, rework] surfaces rework items
        // while preserving each issue's actual status.
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc::now();

        storage
            .create_issue(
                &make_issue("bd-open", "Open", Status::Open, 2, None, t1, None),
                "tester",
            )
            .unwrap();
        storage
            .create_issue(
                &make_issue(
                    "bd-rework",
                    "Rework",
                    Status::Custom("rework".to_string()),
                    2,
                    None,
                    t1,
                    None,
                ),
                "tester",
            )
            .unwrap();
        storage
            .create_issue(
                &make_issue(
                    "bd-inprog",
                    "InProgress",
                    Status::InProgress,
                    2,
                    None,
                    t1,
                    None,
                ),
                "tester",
            )
            .unwrap();

        let filters = ReadyFilters {
            ready_statuses: vec!["open".to_string(), "rework".to_string()],
            ..Default::default()
        };
        let res = storage
            .get_ready_issues(&filters, ReadySortPolicy::Oldest)
            .unwrap();
        let mut ids: Vec<&str> = res.iter().map(|i| i.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["bd-open", "bd-rework"]);
        // in_progress stays out; statuses are preserved.
        let rework = res.iter().find(|i| i.id == "bd-rework").unwrap();
        assert_eq!(rework.status.as_str(), "rework");
    }

    #[test]
    fn test_ready_configured_group_still_gates_defer_until() {
        // #354: a non-deferred configured member with a future defer_until is
        // still time-gated out unless --include-deferred is set.
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc::now();
        let future = t1 + chrono::Duration::days(7);

        storage
            .create_issue(
                &make_issue(
                    "bd-rework-deferred",
                    "ReworkDeferred",
                    Status::Custom("rework".to_string()),
                    2,
                    None,
                    t1,
                    Some(future),
                ),
                "tester",
            )
            .unwrap();
        storage
            .create_issue(
                &make_issue("bd-open", "Open", Status::Open, 2, None, t1, None),
                "tester",
            )
            .unwrap();

        let filters = ReadyFilters {
            ready_statuses: vec!["open".to_string(), "rework".to_string()],
            ..Default::default()
        };
        let res = storage
            .get_ready_issues(&filters, ReadySortPolicy::Oldest)
            .unwrap();
        let ids: Vec<&str> = res.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["bd-open"],
            "future defer_until must gate the rework member out"
        );

        // With --include-deferred, the gate drops and the rework member returns.
        let filters_deferred = ReadyFilters {
            ready_statuses: vec!["open".to_string(), "rework".to_string()],
            include_deferred: true,
            ..Default::default()
        };
        let res = storage
            .get_ready_issues(&filters_deferred, ReadySortPolicy::Oldest)
            .unwrap();
        let mut ids: Vec<&str> = res.iter().map(|i| i.id.as_str()).collect();
        ids.sort_unstable();
        assert_eq!(ids, vec!["bd-open", "bd-rework-deferred"]);
    }

    #[test]
    fn test_ready_include_deferred_no_double_count_when_group_lists_deferred() {
        // #354: --include-deferred folds in `deferred`, but must not double-count
        // it (or error) when the configured group already lists `deferred`.
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc::now();

        storage
            .create_issue(
                &make_issue(
                    "bd-deferred",
                    "Deferred",
                    Status::Deferred,
                    2,
                    None,
                    t1,
                    None,
                ),
                "tester",
            )
            .unwrap();
        storage
            .create_issue(
                &make_issue("bd-open", "Open", Status::Open, 2, None, t1, None),
                "tester",
            )
            .unwrap();

        let filters = ReadyFilters {
            ready_statuses: vec!["open".to_string(), "deferred".to_string()],
            include_deferred: true,
            ..Default::default()
        };
        let res = storage
            .get_ready_issues(&filters, ReadySortPolicy::Oldest)
            .unwrap();
        let mut ids: Vec<&str> = res.iter().map(|i| i.id.as_str()).collect();
        ids.sort_unstable();
        // Exactly one row per id — no duplicate `bd-deferred`.
        assert_eq!(ids, vec!["bd-deferred", "bd-open"]);
    }

    #[test]
    fn test_get_ready_issues_filters_by_parent() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc::now();

        // Create parent epic.  `bd-epic` and `bd-epic.1` are typed as Epic
        // so the epic-rollup "parent blocked by open children" rule applies
        // to them; that is what the later "only non-blocked descendants are
        // ready" assertion exercises.  Non-epic parents intentionally do
        // NOT inherit a child-open blocker (see
        // `parent_child_transitive_blocking` in
        // `tests/storage_blocked_cache.rs`), so typing these as Task would
        // make every parent-child node trivially ready, defeating the
        // point of the recursive-ready test.
        let mut parent = make_issue("bd-epic", "Parent Epic", Status::Open, 1, None, t1, None);
        parent.issue_type = IssueType::Epic;
        storage.create_issue(&parent, "tester").unwrap();

        // Create direct children of the epic.  `bd-epic.1` is itself an
        // epic so it can be rolled-up-blocked by its grandchild below.
        let mut child1 = make_issue("bd-epic.1", "Child 1", Status::Open, 2, None, t1, None);
        child1.issue_type = IssueType::Epic;
        let child2 = make_issue("bd-epic.2", "Child 2", Status::Open, 2, None, t1, None);
        storage.create_issue(&child1, "tester").unwrap();
        storage.create_issue(&child2, "tester").unwrap();

        // Create unrelated issue (not a child of the epic)
        let unrelated = make_issue("bd-other", "Unrelated", Status::Open, 2, None, t1, None);
        storage.create_issue(&unrelated, "tester").unwrap();

        // Add parent-child dependencies (no grandchild yet)
        storage
            .add_dependency("bd-epic.1", "bd-epic", "parent-child", "tester")
            .unwrap();
        storage
            .add_dependency("bd-epic.2", "bd-epic", "parent-child", "tester")
            .unwrap();

        // Test: --parent bd-epic (non-recursive) should return only direct children
        let filters_direct = ReadyFilters {
            parent: Some("bd-epic".to_string()),
            recursive: false,
            ..Default::default()
        };
        let res = storage
            .get_ready_issues(&filters_direct, ReadySortPolicy::Oldest)
            .unwrap();
        assert_eq!(
            res.len(),
            2,
            "Non-recursive should return only direct children"
        );
        let ids: Vec<&str> = res.iter().map(|i| i.id.as_str()).collect();
        assert!(ids.contains(&"bd-epic.1"), "Should contain child1");
        assert!(ids.contains(&"bd-epic.2"), "Should contain child2");

        // Now create grandchild and its dependency for recursive test
        let grandchild = make_issue("bd-epic.1.1", "Grandchild", Status::Open, 2, None, t1, None);
        storage.create_issue(&grandchild, "tester").unwrap();
        storage
            .add_dependency("bd-epic.1.1", "bd-epic.1", "parent-child", "tester")
            .unwrap();

        // Test: --parent bd-epic --recursive should return all non-blocked descendants
        // Note: bd-epic.1 is now blocked by its open child bd-epic.1.1 (blocked-cache
        // semantics), so only bd-epic.2 and bd-epic.1.1 are "ready".
        let filters_recursive = ReadyFilters {
            parent: Some("bd-epic".to_string()),
            recursive: true,
            ..Default::default()
        };
        let res = storage
            .get_ready_issues(&filters_recursive, ReadySortPolicy::Oldest)
            .unwrap();
        assert_eq!(
            res.len(),
            2,
            "Recursive should return non-blocked descendants"
        );
        let ids: Vec<&str> = res.iter().map(|i| i.id.as_str()).collect();
        assert!(ids.contains(&"bd-epic.2"), "Should contain child2");
        assert!(ids.contains(&"bd-epic.1.1"), "Should contain grandchild");
        assert!(
            !ids.contains(&"bd-epic"),
            "Should NOT contain the parent itself"
        );
        assert!(
            !ids.contains(&"bd-other"),
            "Should NOT contain unrelated issue"
        );

        // Test: --parent with non-existent parent should return empty
        let filters_nonexistent = ReadyFilters {
            parent: Some("bd-nonexistent".to_string()),
            recursive: false,
            ..Default::default()
        };
        let res = storage
            .get_ready_issues(&filters_nonexistent, ReadySortPolicy::Oldest)
            .unwrap();
        assert_eq!(res.len(), 0, "Non-existent parent should return empty");
    }

    /// Regression: `--parent` combined with `--label` must return their
    /// intersection, not an empty set (#307). Also covers `--parent --recursive
    /// --label` (#308 / #307 interaction).
    #[test]
    fn test_get_ready_issues_parent_combined_with_label() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc::now();

        let mut parent = make_issue("bd-epic", "Parent Epic", Status::Open, 1, None, t1, None);
        parent.issue_type = IssueType::Epic;
        storage.create_issue(&parent, "tester").unwrap();

        // Two labelled direct children + one unlabelled direct child.
        let child1 = make_issue("bd-epic.1", "Child 1", Status::Open, 2, None, t1, None);
        let child2 = make_issue("bd-epic.2", "Child 2", Status::Open, 2, None, t1, None);
        let child3 = make_issue("bd-epic.3", "Child 3", Status::Open, 2, None, t1, None);
        storage.create_issue(&child1, "tester").unwrap();
        storage.create_issue(&child2, "tester").unwrap();
        storage.create_issue(&child3, "tester").unwrap();
        for id in ["bd-epic.1", "bd-epic.2", "bd-epic.3"] {
            storage
                .add_dependency(id, "bd-epic", "parent-child", "tester")
                .unwrap();
        }
        storage
            .add_label("bd-epic.1", "mini-safe", "tester")
            .unwrap();
        storage
            .add_label("bd-epic.2", "mini-safe", "tester")
            .unwrap();

        // A labelled descendant under bd-epic.1 (for the recursive case).
        let grandchild = make_issue("bd-epic.1.1", "Grandchild", Status::Open, 2, None, t1, None);
        storage.create_issue(&grandchild, "tester").unwrap();
        storage
            .add_dependency("bd-epic.1.1", "bd-epic.1", "parent-child", "tester")
            .unwrap();
        storage
            .add_label("bd-epic.1.1", "mini-safe", "tester")
            .unwrap();

        // --parent alone: all three direct children are ready (non-epic parents
        // do not inherit a child-open blocker).
        let parent_only = ReadyFilters {
            parent: Some("bd-epic".to_string()),
            ..Default::default()
        };
        let res = storage
            .get_ready_issues(&parent_only, ReadySortPolicy::Oldest)
            .unwrap();
        assert_eq!(
            res.len(),
            3,
            "parent-only should return three direct children"
        );

        // --parent + --label: intersection = the two labelled direct children.
        let parent_label = ReadyFilters {
            parent: Some("bd-epic".to_string()),
            labels_and: vec!["mini-safe".to_string()],
            ..Default::default()
        };
        let res = storage
            .get_ready_issues(&parent_label, ReadySortPolicy::Oldest)
            .unwrap();
        let ids: Vec<&str> = res.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(
            res.len(),
            2,
            "parent+label must return the AND-intersection, got {ids:?}"
        );
        assert!(ids.contains(&"bd-epic.1"));
        assert!(ids.contains(&"bd-epic.2"));
        assert!(!ids.contains(&"bd-epic.3"), "unlabelled child excluded");

        // --parent + --recursive + --label: includes the labelled grandchild.
        let parent_recursive_label = ReadyFilters {
            parent: Some("bd-epic".to_string()),
            recursive: true,
            labels_and: vec!["mini-safe".to_string()],
            ..Default::default()
        };
        let res = storage
            .get_ready_issues(&parent_recursive_label, ReadySortPolicy::Oldest)
            .unwrap();
        let ids: Vec<&str> = res.iter().map(|i| i.id.as_str()).collect();
        assert!(
            ids.contains(&"bd-epic.1.1"),
            "recursive+label must reach labelled grandchild, got {ids:?}"
        );
        assert!(ids.contains(&"bd-epic.2"));
    }

    /// Regression: `--parent --recursive` terminates even when the parent-child
    /// graph contains a cycle, instead of hanging on an unbounded walk (#308).
    #[test]
    fn test_get_ready_issues_recursive_parent_cycle_terminates() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc::now();

        for id in ["cyc-a", "cyc-b", "cyc-c"] {
            let issue = make_issue(id, id, Status::Open, 2, None, t1, None);
            storage.create_issue(&issue, "tester").unwrap();
        }
        // a -> b -> c -> a (cycle through parent-child edges).
        storage
            .add_dependency("cyc-b", "cyc-a", "parent-child", "tester")
            .unwrap();
        storage
            .add_dependency("cyc-c", "cyc-b", "parent-child", "tester")
            .unwrap();
        // The edge that closes the a -> b -> c -> a cycle. `add_dependency` now
        // correctly REFUSES to create a parent-child cycle, so a genuine cycle
        // can only enter the store via import/legacy data. Insert the closing
        // row straight into the table (bypassing the guard) to exercise the
        // recursive-BFS termination path against real cyclic data.
        storage
            .conn
            .execute_with_params(
                "INSERT INTO dependencies (issue_id, depends_on_id, type, created_at, created_by) \
                 VALUES (?, ?, ?, ?, ?)",
                &[
                    SqliteValue::from("cyc-a"),
                    SqliteValue::from("cyc-c"),
                    SqliteValue::from("parent-child"),
                    SqliteValue::from(Utc::now().to_rfc3339()),
                    SqliteValue::from("tester"),
                ],
            )
            .unwrap();

        let filters = ReadyFilters {
            parent: Some("cyc-a".to_string()),
            recursive: true,
            ..Default::default()
        };
        // The visited-set BFS must terminate; the exact membership is whatever
        // the readiness rules allow, but the call must return without hanging.
        let res = storage
            .get_ready_issues(&filters, ReadySortPolicy::Oldest)
            .unwrap();
        let ids: Vec<&str> = res.iter().map(|i| i.id.as_str()).collect();
        assert!(
            !ids.contains(&"cyc-a"),
            "parent itself must not appear, got {ids:?}"
        );
    }

    #[test]
    fn test_get_ready_issues_treats_null_legacy_flags_as_false() {
        let conn = Connection::open(":memory:").unwrap();
        crate::storage::schema::execute_batch(
            &conn,
            r"
            CREATE TABLE issues (
                id TEXT PRIMARY KEY,
                content_hash TEXT,
                title TEXT NOT NULL,
                description TEXT,
                design TEXT,
                acceptance_criteria TEXT,
                notes TEXT,
                status TEXT NOT NULL,
                priority INTEGER NOT NULL,
                issue_type TEXT NOT NULL,
                assignee TEXT,
                owner TEXT,
                estimated_minutes INTEGER,
                created_at DATETIME NOT NULL,
                created_by TEXT,
                updated_at DATETIME NOT NULL,
                closed_at DATETIME,
                close_reason TEXT,
                closed_by_session TEXT,
                due_at DATETIME,
                defer_until DATETIME,
                external_ref TEXT,
                source_system TEXT,
                source_repo TEXT,
                deleted_at DATETIME,
                deleted_by TEXT,
                delete_reason TEXT,
                original_type TEXT,
                compaction_level INTEGER,
                compacted_at DATETIME,
                compacted_at_commit TEXT,
                original_size INTEGER,
                sender TEXT,
                ephemeral INTEGER,
                pinned INTEGER,
                is_template INTEGER,
                source_repo_path TEXT,
                agent_context TEXT
            );
            CREATE TABLE blocked_issues_cache (
                issue_id TEXT PRIMARY KEY,
                blocked_by TEXT NOT NULL,
                blocked_at DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP
            );
            CREATE TABLE metadata (
                key TEXT NOT NULL,
                value TEXT NOT NULL
            );
            ",
        )
        .unwrap();

        let storage = SqliteStorage {
            conn,
            mutation_count: 0,
            temp_db_path: None,
            pending_event_attribution: None,
        };
        let timestamp = Utc.with_ymd_and_hms(2026, 3, 11, 0, 0, 0).unwrap();
        let stamp = timestamp.to_rfc3339();

        storage
            .conn
            .execute_with_params(
                r"
                INSERT INTO issues (
                    id, title, status, priority, issue_type, created_at, updated_at,
                    ephemeral, pinned, is_template
                ) VALUES (?, ?, ?, ?, ?, ?, ?, NULL, NULL, NULL)
                ",
                &[
                    SqliteValue::from("bd-legacy-ready"),
                    SqliteValue::from("Legacy ready issue"),
                    SqliteValue::from("open"),
                    SqliteValue::from(2_i64),
                    SqliteValue::from("task"),
                    SqliteValue::from(stamp.as_str()),
                    SqliteValue::from(stamp.as_str()),
                ],
            )
            .unwrap();

        let ready = storage
            .get_ready_issues(&ReadyFilters::default(), ReadySortPolicy::Priority)
            .unwrap();

        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "bd-legacy-ready");
    }

    #[test]
    fn test_get_ready_issues_skips_stale_cache_work_when_no_candidate_status() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 3, 12, 0, 0, 0).unwrap();
        let mut closed = make_issue(
            "bd-closed",
            "Closed issue",
            Status::Closed,
            2,
            None,
            now,
            None,
        );
        closed.closed_at = Some(now);
        storage.create_issue(&closed, "tester").unwrap();
        storage.mark_blocked_cache_stale().unwrap();

        let ready = storage
            .get_ready_issues(&ReadyFilters::default(), ReadySortPolicy::Priority)
            .unwrap();

        assert!(ready.is_empty());
        assert!(
            storage.blocked_cache_marked_stale().unwrap(),
            "read-only ready query must not refresh or clear stale blocked-cache metadata"
        );
    }

    #[test]
    fn test_get_ready_issues_hybrid_sort_and_limit() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let base = Utc.with_ymd_and_hms(2026, 3, 12, 0, 0, 0).unwrap();

        for issue in [
            make_issue("bd-low-old", "Low old", Status::Open, 4, None, base, None),
            make_issue(
                "bd-high-old",
                "High old",
                Status::Open,
                1,
                None,
                base + chrono::Duration::seconds(1),
                None,
            ),
            make_issue(
                "bd-critical-new",
                "Critical new",
                Status::Open,
                0,
                None,
                base + chrono::Duration::seconds(2),
                None,
            ),
            make_issue(
                "bd-low-new",
                "Low new",
                Status::Open,
                2,
                None,
                base + chrono::Duration::seconds(3),
                None,
            ),
        ] {
            storage.create_issue(&issue, "tester").unwrap();
        }

        let ids: Vec<String> = storage
            .get_ready_issues(&ReadyFilters::default(), ReadySortPolicy::Hybrid)
            .unwrap()
            .into_iter()
            .map(|issue| issue.id)
            .collect();
        assert_eq!(
            ids,
            ["bd-high-old", "bd-critical-new", "bd-low-old", "bd-low-new"]
        );

        let limited_ids: Vec<String> = storage
            .get_ready_issues(
                &ReadyFilters {
                    limit: Some(2),
                    ..ReadyFilters::default()
                },
                ReadySortPolicy::Hybrid,
            )
            .unwrap()
            .into_iter()
            .map(|issue| issue.id)
            .collect();
        assert_eq!(limited_ids, ["bd-high-old", "bd-critical-new"]);

        let fallback_limited_ids: Vec<String> = storage
            .get_ready_issues(
                &ReadyFilters {
                    limit: Some(3),
                    ..ReadyFilters::default()
                },
                ReadySortPolicy::Hybrid,
            )
            .unwrap()
            .into_iter()
            .map(|issue| issue.id)
            .collect();
        assert_eq!(
            fallback_limited_ids,
            ["bd-high-old", "bd-critical-new", "bd-low-old"]
        );
    }

    #[test]
    fn test_get_ready_issues_limited_sql_sort_uses_id_tiebreaker() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let created_at = Utc.with_ymd_and_hms(2026, 3, 13, 0, 0, 0).unwrap();

        for issue in [
            make_issue(
                "bd-tie-b",
                "Tie inserted first",
                Status::Open,
                1,
                None,
                created_at,
                None,
            ),
            make_issue(
                "bd-tie-a",
                "Tie inserted second",
                Status::Open,
                1,
                None,
                created_at,
                None,
            ),
            make_issue(
                "bd-low",
                "Low priority",
                Status::Open,
                3,
                None,
                created_at + chrono::Duration::seconds(1),
                None,
            ),
        ] {
            storage.create_issue(&issue, "tester").unwrap();
        }

        let ids: Vec<String> = storage
            .get_ready_issues(
                &ReadyFilters {
                    limit: Some(2),
                    ..ReadyFilters::default()
                },
                ReadySortPolicy::Hybrid,
            )
            .unwrap()
            .into_iter()
            .map(|issue| issue.id)
            .collect();

        assert_eq!(ids, ["bd-tie-a", "bd-tie-b"]);
    }

    #[test]
    fn test_get_ready_issues_excludes_in_progress() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc::now();

        // Create an open issue (should appear in ready)
        let open_issue = make_issue("bd-open", "Open Issue", Status::Open, 2, None, t1, None);
        storage.create_issue(&open_issue, "tester").unwrap();

        // Create an in_progress issue (should NOT appear in ready)
        let ip_issue = make_issue(
            "bd-inprogress",
            "In Progress Issue",
            Status::InProgress,
            1,
            None,
            t1,
            None,
        );
        storage.create_issue(&ip_issue, "tester").unwrap();

        let ready = storage
            .get_ready_issues(&ReadyFilters::default(), ReadySortPolicy::Priority)
            .unwrap();

        // Only the open issue should be ready; in_progress is already claimed
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0].id, "bd-open");
    }

    #[test]
    fn test_next_child_number() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();

        // Create parent issue
        let parent = make_issue("bd-parent", "Parent Epic", Status::Open, 2, None, t1, None);
        storage.create_issue(&parent, "tester").unwrap();

        // No children yet - should return 1
        let next = storage.next_child_number("bd-parent").unwrap();
        assert_eq!(next, 1, "First child should be .1");

        // Create first child
        let child1 = make_issue("bd-parent.1", "Child 1", Status::Open, 2, None, t1, None);
        storage.create_issue(&child1, "tester").unwrap();

        // Should now return 2
        let next = storage.next_child_number("bd-parent").unwrap();
        assert_eq!(next, 2, "After .1 exists, next should be .2");

        // Create child with .3 (skip .2)
        let child3 = make_issue("bd-parent.3", "Child 3", Status::Open, 2, None, t1, None);
        storage.create_issue(&child3, "tester").unwrap();

        // Should return 4 (max is 3, so next is 4)
        let next = storage.next_child_number("bd-parent").unwrap();
        assert_eq!(next, 4, "After .3 exists (skipping .2), next should be .4");

        // Create grandchild - should not affect parent's next child number
        let grandchild = make_issue(
            "bd-parent.1.1",
            "Grandchild",
            Status::Open,
            2,
            None,
            t1,
            None,
        );
        storage.create_issue(&grandchild, "tester").unwrap();

        // Parent's next child should still be 4
        let next = storage.next_child_number("bd-parent").unwrap();
        assert_eq!(
            next, 4,
            "Grandchild should not affect parent's next child number"
        );

        // Check grandchild's parent (bd-parent.1) next child number
        let next_for_child1 = storage.next_child_number("bd-parent.1").unwrap();
        assert_eq!(
            next_for_child1, 2,
            "After bd-parent.1.1 exists, next for bd-parent.1 should be .2"
        );
    }

    #[test]
    fn test_rebuild_child_counters_skips_missing_parents() {
        let storage = SqliteStorage::open_memory().unwrap();
        let timestamp = Utc.with_ymd_and_hms(2026, 3, 12, 0, 0, 0).unwrap();
        let stamp = timestamp.to_rfc3339();

        storage
            .conn
            .execute_with_params(
                r"
                INSERT INTO issues (
                    id, title, status, priority, issue_type, created_at, updated_at,
                    ephemeral, pinned, is_template
                ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                ",
                &[
                    SqliteValue::from("bd-orphan.6"),
                    SqliteValue::from("Recovered orphan child"),
                    SqliteValue::from("open"),
                    SqliteValue::from(2_i64),
                    SqliteValue::from("task"),
                    SqliteValue::from(stamp.as_str()),
                    SqliteValue::from(stamp.as_str()),
                    SqliteValue::from(0_i64),
                    SqliteValue::from(0_i64),
                    SqliteValue::from(0_i64),
                ],
            )
            .unwrap();

        let rebuilt = storage.rebuild_child_counters_in_tx().unwrap();
        assert_eq!(
            rebuilt, 0,
            "missing parents should not get counters rebuilt"
        );
        assert!(
            !storage
                .has_missing_issue_reference("child_counters", "parent_id")
                .unwrap(),
            "rebuild should not create orphan child counter rows"
        );
    }

    #[test]
    fn test_finish_issue_mutation_write_probe_returns_rollback_error_when_cleanup_fails() {
        let result = finish_issue_mutation_write_probe(
            Ok(1),
            Err(FrankenError::Internal("rollback failed".to_string())),
        );

        let err = result.expect_err("rollback failure should surface");
        assert!(
            err.to_string().contains("rollback failed"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_finish_issue_mutation_write_probe_prefers_write_error() {
        let result = finish_issue_mutation_write_probe(
            Err(FrankenError::Internal("write failed".to_string())),
            Err(FrankenError::Internal("rollback failed".to_string())),
        );

        let err = result.expect_err("write failure should surface");
        assert!(
            err.to_string().contains("write failed"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_parse_datetime_empty_string_returns_epoch() {
        let result = parse_datetime("").unwrap();
        assert_eq!(result, DateTime::<Utc>::UNIX_EPOCH);
    }

    #[test]
    fn test_parse_datetime_rfc3339_with_z() {
        let result = parse_datetime("2026-02-26T19:54:42.715824474Z").unwrap();
        assert_eq!(result.year(), 2026);
        assert_eq!(result.month(), 2);
    }

    #[test]
    fn test_parse_canonical_utc_datetime_matches_rfc3339_parser() {
        let raw = "2026-01-16T07:21:09.280348123+00:00";
        let fast = parse_canonical_utc_datetime(raw).expect("canonical UTC timestamp");
        let general = DateTime::parse_from_rfc3339(raw)
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(fast, general);
        assert_eq!(fast.timestamp_subsec_nanos(), 280_348_123);
    }

    #[test]
    fn test_parse_canonical_utc_datetime_leaves_non_utc_offsets_to_fallback() {
        assert!(parse_canonical_utc_datetime("2026-01-16T07:21:09.280348123+01:00").is_none());
    }

    #[test]
    fn test_parse_datetime_rfc3339_with_offset() {
        let result = parse_datetime("2026-02-26T19:54:42+00:00").unwrap();
        assert_eq!(result.year(), 2026);
    }

    #[test]
    fn test_parse_datetime_naive_format() {
        let result = parse_datetime("2026-02-26 19:54:42").unwrap();
        assert_eq!(result.year(), 2026);
        assert_eq!(result.month(), 2);
    }

    #[test]
    fn test_parse_datetime_garbage_returns_error() {
        let result = parse_datetime("not-a-date");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_datetime_value_text_roundtrips() {
        let v = SqliteValue::from("2026-04-19T21:34:04.546468109Z");
        let dt = parse_datetime_value(Some(&v)).unwrap();
        assert_eq!(dt.year(), 2026);
        assert_eq!(dt.month(), 4);
        assert_eq!(dt.day(), 19);
    }

    #[test]
    fn test_parse_datetime_value_integer_microseconds() {
        // 1776651488000000 µs = 2026-04-20T02:18:08Z — the exact wire format
        // the schema v6 migration repairs. The old reader (as_text().unwrap_or("")
        // → parse_datetime) silently produced UNIX_EPOCH here.
        let v = SqliteValue::Integer(1_776_651_488_000_000);
        let dt = parse_datetime_value(Some(&v)).unwrap();
        assert_eq!(dt.year(), 2026);
        assert_eq!(dt.month(), 4);
        assert_eq!(dt.day(), 20);
        assert_eq!(dt.hour(), 2);
        assert_eq!(dt.minute(), 18);
        assert_eq!(dt.second(), 8);
    }

    #[test]
    fn test_parse_datetime_value_integer_seconds() {
        // 1_776_651_488 s = 2026-04-20T02:18:08Z
        let v = SqliteValue::Integer(1_776_651_488);
        let dt = parse_datetime_value(Some(&v)).unwrap();
        assert_eq!(dt.year(), 2026);
        assert_eq!(dt.hour(), 2);
    }

    #[test]
    fn test_parse_datetime_value_integer_nanoseconds() {
        let v = SqliteValue::Integer(1_776_651_488_000_000_000);
        let dt = parse_datetime_value(Some(&v)).unwrap();
        assert_eq!(dt.year(), 2026);
    }

    #[test]
    fn test_datetime_from_epoch_auto_keeps_exact_unit_boundaries() {
        let seconds = datetime_from_epoch_auto(10_000_000_000).unwrap();
        assert_eq!(seconds.timestamp(), 10_000_000_000);
        assert_eq!(seconds.timestamp_subsec_nanos(), 0);

        let milliseconds = datetime_from_epoch_auto(10_000_000_000_000).unwrap();
        assert_eq!(milliseconds.timestamp(), 10_000_000_000);
        assert_eq!(milliseconds.timestamp_subsec_nanos(), 0);

        let microseconds = datetime_from_epoch_auto(10_000_000_000_000_000).unwrap();
        assert_eq!(microseconds.timestamp(), 10_000_000_000);
        assert_eq!(microseconds.timestamp_subsec_nanos(), 0);

        let negative_seconds = datetime_from_epoch_auto(-10_000_000_000).unwrap();
        assert_eq!(negative_seconds.timestamp(), -10_000_000_000);
        assert_eq!(negative_seconds.timestamp_subsec_nanos(), 0);

        let negative_milliseconds = datetime_from_epoch_auto(-10_000_000_000_000).unwrap();
        assert_eq!(negative_milliseconds.timestamp(), -10_000_000_000);
        assert_eq!(negative_milliseconds.timestamp_subsec_nanos(), 0);

        let negative_microseconds = datetime_from_epoch_auto(-10_000_000_000_000_000).unwrap();
        assert_eq!(negative_microseconds.timestamp(), -10_000_000_000);
        assert_eq!(negative_microseconds.timestamp_subsec_nanos(), 0);
    }

    #[test]
    fn test_parse_datetime_value_null_is_epoch() {
        assert_eq!(
            parse_datetime_value(Some(&SqliteValue::Null)).unwrap(),
            DateTime::<Utc>::UNIX_EPOCH
        );
        assert_eq!(
            parse_datetime_value(None).unwrap(),
            DateTime::<Utc>::UNIX_EPOCH
        );
    }

    #[test]
    fn test_parse_opt_datetime_value_integer_preserved_not_dropped() {
        // The legacy get_opt_datetime path turned integer-typed columns
        // into None; parse_opt_datetime_value must preserve the timestamp.
        let v = SqliteValue::Integer(1_776_651_488_000_000);
        let dt = parse_opt_datetime_value(Some(&v)).unwrap().unwrap();
        assert_eq!(dt.year(), 2026);
        assert_eq!(dt.day(), 20);
    }

    #[test]
    fn test_parse_opt_datetime_value_null_is_none() {
        assert_eq!(
            parse_opt_datetime_value(Some(&SqliteValue::Null)).unwrap(),
            None
        );
        assert_eq!(parse_opt_datetime_value(None).unwrap(), None);
        assert_eq!(
            parse_opt_datetime_value(Some(&SqliteValue::from(""))).unwrap(),
            None
        );
    }

    #[test]
    fn test_parse_datetime_value_rejects_blob() {
        let v = SqliteValue::Blob(std::sync::Arc::from(b"bad".as_slice()));
        assert!(parse_datetime_value(Some(&v)).is_err());
        assert!(parse_opt_datetime_value(Some(&v)).is_err());
    }

    #[test]
    fn test_datetime_from_epoch_seconds_f64_negative_fraction_floor_split() {
        // Regression: the (secs, nanos) split must use floor(), not trunc(),
        // so negative fractional seconds round the right way. With trunc()
        // and abs(), `-1.5` incorrectly resolved to `-0.5` (secs=-1,
        // nanos=5e8), one second higher than intended.
        let dt = datetime_from_epoch_seconds_f64(-1.5).unwrap();
        // -1.5 s before epoch = 1969-12-31T23:59:58.5Z
        assert_eq!(dt.timestamp(), -2);
        assert_eq!(dt.timestamp_subsec_nanos(), 500_000_000);

        // Positive fractional remains correct after the fix.
        let dt = datetime_from_epoch_seconds_f64(1_776_651_488.25).unwrap();
        assert_eq!(dt.timestamp(), 1_776_651_488);
        assert_eq!(dt.timestamp_subsec_nanos(), 250_000_000);
    }

    #[test]
    fn test_datetime_from_epoch_seconds_f64_carries_rounded_nanoseconds() {
        let dt = datetime_from_epoch_seconds_f64(1.999_999_999_6).unwrap();
        assert_eq!(dt.timestamp(), 2);
        assert_eq!(dt.timestamp_subsec_nanos(), 0);

        let dt = datetime_from_epoch_seconds_f64(-0.000_000_000_4).unwrap();
        assert_eq!(dt.timestamp(), 0);
        assert_eq!(dt.timestamp_subsec_nanos(), 0);
    }

    #[test]
    fn test_parse_datetime_value_integer_negative_is_pre_epoch() {
        // -3600 s = 1969-12-31T23:00:00Z. Confirms negatives route through
        // the seconds branch correctly (not mis-classified as a larger
        // unit, and no wrap-around via unsigned_abs).
        let v = SqliteValue::Integer(-3600);
        let dt = parse_datetime_value(Some(&v)).unwrap();
        assert_eq!(dt.timestamp(), -3600);
        assert_eq!(dt.timestamp_subsec_nanos(), 0);

        // Negative microsecond-range values pick up div_euclid/rem_euclid's
        // floor semantics instead of truncating toward zero.
        let v = SqliteValue::Integer(-1_500_000_000_000_000); // µs-range magnitude
        let dt = parse_datetime_value(Some(&v)).unwrap();
        assert_eq!(dt.timestamp(), -1_500_000_000);
        assert_eq!(dt.timestamp_subsec_nanos(), 0);
    }

    #[test]
    fn test_reset_data_tables_preserves_config() {
        // Use a real file to avoid fsqlite in-memory BUSY contention under parallel tests.
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("test.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let t1 = Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap();

        // Set config and create some issues
        storage.set_config("issue_prefix", "test").unwrap();
        storage.set_metadata("last_import", "2025-01-01").unwrap();
        let issue = make_issue("test-1", "Issue 1", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue, "tester").unwrap();

        // Verify issue exists
        assert!(storage.get_issue("test-1").unwrap().is_some());

        // Reset data tables
        storage.reset_data_tables().unwrap();

        // Config and metadata should be preserved
        assert_eq!(
            storage.get_config("issue_prefix").unwrap(),
            Some("test".to_string()),
        );
        assert_eq!(
            storage.get_metadata("last_import").unwrap(),
            Some("2025-01-01".to_string()),
        );

        // Issue data should be gone
        assert!(storage.get_issue("test-1").unwrap().is_none());

        // Should be able to insert new issues (schema intact)
        let issue2 = make_issue("test-2", "Issue 2", Status::Open, 2, None, t1, None);
        storage.create_issue(&issue2, "tester").unwrap();
        assert!(storage.get_issue("test-2").unwrap().is_some());
    }

    #[test]
    fn test_open_seeds_known_metadata_defaults() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("seeded-metadata.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        let rows = storage
            .conn
            .query("SELECT key, value FROM metadata ORDER BY key ASC")
            .unwrap();
        let mut entries = HashMap::new();
        for row in rows {
            let key = row
                .get(0)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            let value = row
                .get(1)
                .and_then(SqliteValue::as_text)
                .unwrap_or("")
                .to_string();
            entries.insert(key, value);
        }

        for (key, default_value) in KNOWN_METADATA_DEFAULTS {
            assert_eq!(
                entries.get(key).map(String::as_str),
                Some(default_value),
                "expected seeded metadata default for key '{key}'"
            );
        }

        assert_eq!(
            storage.get_metadata(BLOCKED_CACHE_STATE_KEY).unwrap(),
            None,
            "empty blocked-cache seed should read as missing"
        );
        assert_eq!(
            storage.get_metadata(METADATA_JSONL_CONTENT_HASH).unwrap(),
            None,
            "empty sync hash seed should read as missing"
        );
        assert_eq!(
            storage.get_metadata(METADATA_LAST_EXPORT_TIME).unwrap(),
            None,
            "empty export timestamp seed should read as missing"
        );
        assert_eq!(
            storage.get_metadata(METADATA_LAST_IMPORT_TIME).unwrap(),
            None,
            "empty import timestamp seed should read as missing"
        );
    }

    #[test]
    fn test_metadata_default_insert_rechecks_existing_key() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("metadata-default-race.db");
        let storage = SqliteStorage::open(&db_path).unwrap();

        storage
            .conn
            .execute_with_params(
                "DELETE FROM metadata WHERE key = ?",
                &[SqliteValue::from(METADATA_JSONL_SIZE)],
            )
            .unwrap();
        storage
            .conn
            .execute_with_params(
                "INSERT INTO metadata (key, value) VALUES (?, ?)",
                &[
                    SqliteValue::from(METADATA_JSONL_SIZE),
                    SqliteValue::from("racing-writer"),
                ],
            )
            .unwrap();

        SqliteStorage::insert_metadata_default_if_missing(
            &storage.conn,
            METADATA_JSONL_SIZE,
            METADATA_EMPTY_VALUE,
        )
        .unwrap();

        let rows = storage
            .conn
            .query_with_params(
                "SELECT value FROM metadata WHERE key = ? ORDER BY rowid ASC",
                &[SqliteValue::from(METADATA_JSONL_SIZE)],
            )
            .unwrap();
        let values: Vec<String> = rows
            .iter()
            .filter_map(|row| {
                row.get(0)
                    .and_then(SqliteValue::as_text)
                    .map(str::to_string)
            })
            .collect();

        assert_eq!(
            values,
            vec!["racing-writer".to_string()],
            "default seeding must not duplicate or overwrite a key inserted by a racing opener"
        );
    }

    #[test]
    fn test_metadata_state_updates_keep_single_seeded_row() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("metadata-state.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();

        storage.set_metadata(NEEDS_FLUSH_KEY, "true").unwrap();
        storage.set_metadata(NEEDS_FLUSH_KEY, "false").unwrap();
        storage.mark_blocked_cache_stale().unwrap();
        storage.rebuild_blocked_cache(true).unwrap();

        let needs_flush_count = storage
            .conn
            .query_row_with_params(
                "SELECT count(*) FROM metadata WHERE key = ?",
                &[SqliteValue::from(NEEDS_FLUSH_KEY)],
            )
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap_or_default();
        assert_eq!(needs_flush_count, 1);

        let blocked_cache_count = storage
            .conn
            .query_row_with_params(
                "SELECT count(*) FROM metadata WHERE key = ?",
                &[SqliteValue::from(BLOCKED_CACHE_STATE_KEY)],
            )
            .unwrap()
            .get(0)
            .and_then(SqliteValue::as_integer)
            .unwrap_or_default();
        assert_eq!(blocked_cache_count, 1);

        assert_eq!(
            storage.get_metadata(NEEDS_FLUSH_KEY).unwrap(),
            Some("false".to_string())
        );
        assert_eq!(storage.get_metadata(BLOCKED_CACHE_STATE_KEY).unwrap(), None);
    }

    #[test]
    fn test_metadata_duplicate_rows_read_latest_and_harmonize_on_write() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("metadata-duplicates.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();

        storage
            .conn
            .execute_with_params(
                "INSERT INTO metadata (key, value) VALUES (?, ?)",
                &[
                    SqliteValue::from(METADATA_JSONL_CONTENT_HASH),
                    SqliteValue::from("stale-hash"),
                ],
            )
            .unwrap();
        storage
            .conn
            .execute_with_params(
                "INSERT INTO metadata (key, value) VALUES (?, ?)",
                &[
                    SqliteValue::from(METADATA_JSONL_CONTENT_HASH),
                    SqliteValue::from("latest-hash"),
                ],
            )
            .unwrap();

        assert_eq!(
            storage.get_metadata(METADATA_JSONL_CONTENT_HASH).unwrap(),
            Some("latest-hash".to_string()),
            "metadata reads must use the latest duplicate row"
        );

        storage
            .set_metadata(METADATA_JSONL_CONTENT_HASH, "rewritten-hash")
            .unwrap();

        let rows = storage
            .conn
            .query_with_params(
                "SELECT value FROM metadata WHERE key = ? ORDER BY rowid ASC",
                &[SqliteValue::from(METADATA_JSONL_CONTENT_HASH)],
            )
            .unwrap();
        let values: Vec<String> = rows
            .iter()
            .filter_map(|row| row.get(0).and_then(SqliteValue::as_text).map(String::from))
            .collect();

        assert_eq!(
            values,
            vec![
                "rewritten-hash".to_string(),
                "rewritten-hash".to_string(),
                "rewritten-hash".to_string(),
            ],
            "metadata writes must harmonize every duplicate row for the key"
        );
    }

    #[test]
    fn test_ready_readiness_probe_uses_latest_blocked_cache_state_duplicate() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("ready-stale-duplicate.db");
        let mut storage = SqliteStorage::open(&db_path).unwrap();
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();

        storage
            .create_issue(
                &make_issue("bd-ready", "Ready issue", Status::Open, 1, None, now, None),
                "tester",
            )
            .unwrap();
        storage
            .conn
            .execute_with_params(
                "INSERT INTO metadata (key, value) VALUES (?, ?)",
                &[
                    SqliteValue::from(BLOCKED_CACHE_STATE_KEY),
                    SqliteValue::from(BLOCKED_CACHE_STATE_STALE),
                ],
            )
            .unwrap();
        storage
            .conn
            .execute_with_params(
                "INSERT INTO metadata (key, value) VALUES (?, ?)",
                &[
                    SqliteValue::from(BLOCKED_CACHE_STATE_KEY),
                    SqliteValue::from(METADATA_EMPTY_VALUE),
                ],
            )
            .unwrap();

        let readiness = storage.ready_readiness_probe(false).unwrap();

        assert!(readiness.has_candidate_status);
        assert!(
            !readiness.blocked_cache_stale,
            "an older duplicate stale marker must not force the ready path to bypass the cache"
        );
    }

    /// Regression: the Drop checkpoint heuristic from #270 fires only
    /// when the handle accumulated mutations since the last periodic
    /// `wal_checkpoint(PASSIVE)`. The read-only path keeps the
    /// original "no checkpoint on teardown" behaviour described in
    /// the Drop impl's comment so it doesn't re-introduce the
    /// spurious busy failures the previous design avoided.
    ///
    /// We assert the gate (`mutation_count > 0`) rather than the
    /// post-Drop WAL file size because the actual on-disk effect of
    /// `PRAGMA wal_checkpoint(TRUNCATE)` is fsqlite's responsibility
    /// — its checkpoint executor decides whether the file is
    /// truncated to zero or retained as a zero-frame header — and
    /// keeping that detail out of beads_rust's regression suite
    /// avoids a false alarm whenever fsqlite revises its WAL
    /// teardown.
    #[test]
    fn test_drop_checkpoint_gate_tracks_mutation_count() {
        let temp = TempDir::new().unwrap();
        let db_path = temp.path().join("drop-checkpoint.db");

        // Mutating handle: create_issue must drive `mutation_count`
        // above zero so the Drop impl's gate selects the checkpoint
        // branch. This assertion is the single point of contract
        // between the heuristic in `Drop` and the underlying
        // `with_write_transaction` accounting.
        {
            let mut storage = SqliteStorage::open(&db_path).unwrap();
            let now = Utc::now();
            let issue = make_issue(
                "bd-drop-1",
                "drop-checkpoint",
                Status::Open,
                2,
                None,
                now,
                None,
            );
            storage.create_issue(&issue, "tester").unwrap();
            assert!(
                storage.mutation_count > 0,
                "create_issue must increment mutation_count so the Drop heuristic checkpoints WAL"
            );
            // Drop runs at end of scope and exercises the checkpoint
            // path. Any panic from inside Drop would surface as a
            // test failure regardless of whether we observe the
            // sidecar afterward.
        }

        // Re-opening must succeed and see the row, proving the
        // committed data survived the Drop teardown intact (whether
        // it lives in the main DB file post-checkpoint or in a
        // replayed WAL is fsqlite's call).
        let storage = SqliteStorage::open(&db_path).unwrap();
        assert_eq!(
            storage.mutation_count, 0,
            "fresh open must not pre-increment mutation_count"
        );
        assert!(
            storage.get_issue("bd-drop-1").unwrap().is_some(),
            "row written before drop must be visible after re-open"
        );
    }

    /// Regression test for beads_rust-ok70: verify that status-change updates
    /// complete successfully and the blocked cache is rebuilt without SQL parse
    /// errors, even when the dependency graph is complex (parent-child chains,
    /// cross-blocking, multiple epic parents).
    #[test]
    fn test_update_status_triggers_successful_cache_rebuild() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t = Utc::now();

        let epic = make_issue("proj-epic", "Epic", Status::Open, 0, Some("epic"), t, None);
        let task1 = make_issue(
            "proj-task1",
            "Task 1",
            Status::Open,
            1,
            Some("task"),
            t,
            None,
        );
        let task2 = make_issue(
            "proj-task2",
            "Task 2",
            Status::Open,
            1,
            Some("task"),
            t,
            None,
        );
        let task3 = make_issue(
            "proj-task3",
            "Task 3",
            Status::Open,
            2,
            Some("task"),
            t,
            None,
        );
        let blocker = make_issue(
            "proj-blocker",
            "Blocker",
            Status::Open,
            1,
            Some("bug"),
            t,
            None,
        );

        storage.create_issue(&epic, "tester").unwrap();
        storage.create_issue(&task1, "tester").unwrap();
        storage.create_issue(&task2, "tester").unwrap();
        storage.create_issue(&task3, "tester").unwrap();
        storage.create_issue(&blocker, "tester").unwrap();

        // Build a complex dependency graph:
        // epic <- task1 (parent-child)
        // epic <- task2 (parent-child)
        // task1 is blocked by blocker
        // task3 is blocked by task2
        storage
            .add_dependency("proj-task1", "proj-epic", "parent-child", "tester")
            .unwrap();
        storage
            .add_dependency("proj-task2", "proj-epic", "parent-child", "tester")
            .unwrap();
        storage
            .add_dependency("proj-task1", "proj-blocker", "blocks", "tester")
            .unwrap();
        storage
            .add_dependency("proj-task3", "proj-task2", "blocks", "tester")
            .unwrap();

        // Verify initial blocked state
        let blocked = storage.get_blocked_issues().unwrap();
        assert!(
            blocked.iter().any(|(i, _)| i.id == "proj-task1"),
            "task1 should be blocked by blocker"
        );
        assert!(
            blocked.iter().any(|(i, _)| i.id == "proj-task3"),
            "task3 should be blocked by task2"
        );

        // Update status to in_progress — this triggers blocked cache rebuild
        let updates = IssueUpdate {
            status: Some(Status::InProgress),
            ..Default::default()
        };
        let updated = storage
            .update_issue("proj-blocker", &updates, "tester")
            .unwrap();
        assert_eq!(updated.status, Status::InProgress);

        // Cache should still be consistent after rebuild
        assert!(
            !storage.blocked_cache_marked_stale().unwrap(),
            "cache should not be stale after successful update"
        );

        // Close the blocker — should unblock task1
        let close_updates = IssueUpdate {
            status: Some(Status::Closed),
            close_reason: Some(Some("done".to_string())),
            ..Default::default()
        };
        let closed = storage
            .update_issue("proj-blocker", &close_updates, "tester")
            .unwrap();
        assert_eq!(closed.status, Status::Closed);

        // task1 should now be unblocked
        let blocked_after = storage.get_blocked_issues().unwrap();
        assert!(
            !blocked_after.iter().any(|(i, _)| i.id == "proj-task1"),
            "task1 should be unblocked after blocker is closed"
        );
        // task3 should still be blocked by task2
        assert!(
            blocked_after.iter().any(|(i, _)| i.id == "proj-task3"),
            "task3 should still be blocked by task2"
        );
    }

    /// Regression test for beads_rust-m06q: closing a blocker and then
    /// immediately updating the newly-unblocked dependent must not trigger a
    /// UNIQUE constraint violation in blocked_issues_cache.
    #[test]
    fn test_close_blocker_then_claim_unblocked_issue_no_unique_violation() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t = Utc::now();

        let blocker = make_issue(
            "m06q-blocker",
            "Blocker",
            Status::Open,
            1,
            Some("bug"),
            t,
            None,
        );
        let task = make_issue("m06q-task", "Task", Status::Open, 1, Some("task"), t, None);

        storage.create_issue(&blocker, "tester").unwrap();
        storage.create_issue(&task, "tester").unwrap();
        storage
            .add_dependency("m06q-task", "m06q-blocker", "blocks", "tester")
            .unwrap();

        // Verify task is blocked
        let blocked = storage.get_blocked_issues().unwrap();
        assert!(
            blocked.iter().any(|(i, _)| i.id == "m06q-task"),
            "task should be blocked initially"
        );

        // Close the blocker — triggers cache rebuild
        let close_updates = IssueUpdate {
            status: Some(Status::Closed),
            close_reason: Some(Some("done".to_string())),
            ..Default::default()
        };
        storage
            .update_issue("m06q-blocker", &close_updates, "tester")
            .unwrap();

        // Immediately "claim" the now-unblocked task by updating its status.
        // This used to fail with UNIQUE constraint on blocked_issues_cache.issue_id
        // when the cache rebuild inserted a duplicate row.
        let claim_updates = IssueUpdate {
            status: Some(Status::InProgress),
            ..Default::default()
        };
        let claimed = storage
            .update_issue("m06q-task", &claim_updates, "tester")
            .unwrap();
        assert_eq!(claimed.status, Status::InProgress);

        // Task should not appear in blocked list
        let blocked_after = storage.get_blocked_issues().unwrap();
        assert!(
            !blocked_after.iter().any(|(i, _)| i.id == "m06q-task"),
            "task should not be blocked after blocker was closed"
        );
    }

    /// Extended regression for beads_rust-m06q: multiple blockers closed in
    /// sequence with interleaved claims must not produce duplicate cache rows.
    #[test]
    fn test_multiple_close_claim_cycles_no_unique_violation() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let t = Utc::now();

        let b1 = make_issue(
            "m06q-b1",
            "Blocker 1",
            Status::Open,
            1,
            Some("bug"),
            t,
            None,
        );
        let b2 = make_issue(
            "m06q-b2",
            "Blocker 2",
            Status::Open,
            1,
            Some("bug"),
            t,
            None,
        );
        let task = make_issue(
            "m06q-dual",
            "Dual blocked",
            Status::Open,
            1,
            Some("task"),
            t,
            None,
        );

        storage.create_issue(&b1, "tester").unwrap();
        storage.create_issue(&b2, "tester").unwrap();
        storage.create_issue(&task, "tester").unwrap();
        storage
            .add_dependency("m06q-dual", "m06q-b1", "blocks", "tester")
            .unwrap();
        storage
            .add_dependency("m06q-dual", "m06q-b2", "blocks", "tester")
            .unwrap();

        // Task blocked by both
        let blocked = storage.get_blocked_issues().unwrap();
        assert!(blocked.iter().any(|(i, _)| i.id == "m06q-dual"));

        // Close first blocker
        let close = IssueUpdate {
            status: Some(Status::Closed),
            close_reason: Some(Some("done".to_string())),
            ..Default::default()
        };
        storage.update_issue("m06q-b1", &close, "tester").unwrap();

        // Task still blocked by b2
        let blocked = storage.get_blocked_issues().unwrap();
        assert!(
            blocked.iter().any(|(i, _)| i.id == "m06q-dual"),
            "task should still be blocked by second blocker"
        );

        // Close second blocker
        storage.update_issue("m06q-b2", &close, "tester").unwrap();

        // Immediately claim — should succeed without UNIQUE violation
        let claim = IssueUpdate {
            status: Some(Status::InProgress),
            ..Default::default()
        };
        let claimed = storage.update_issue("m06q-dual", &claim, "tester").unwrap();
        assert_eq!(claimed.status, Status::InProgress);

        let blocked_after = storage.get_blocked_issues().unwrap();
        assert!(
            !blocked_after.iter().any(|(i, _)| i.id == "m06q-dual"),
            "task should be unblocked after both blockers closed"
        );
    }

    // ========================================================================
    // get_open_dot_notation_children — supplementary guard for legacy/imported
    // dot-notation parent-child relationships that lack formal dep rows.
    // ========================================================================

    #[test]
    fn dot_notation_children_detects_open_direct_children() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let parent = make_issue("bd-epic", "Epic", Status::Open, 1, None, now, None);
        let child1 = make_issue("bd-epic.1", "Child 1", Status::Open, 1, None, now, None);
        let child2 = make_issue(
            "bd-epic.2",
            "Child 2",
            Status::InProgress,
            1,
            None,
            now,
            None,
        );
        let mut child3 = make_issue("bd-epic.3", "Child 3", Status::Closed, 1, None, now, None);
        // closed rows require closed_at set (DB CHECK constraint).
        child3.closed_at = Some(now);
        storage.create_issue(&parent, "t").unwrap();
        storage.create_issue(&child1, "t").unwrap();
        storage.create_issue(&child2, "t").unwrap();
        storage.create_issue(&child3, "t").unwrap();

        let mut open_children = storage.get_open_dot_notation_children("bd-epic").unwrap();
        open_children.sort();
        assert_eq!(
            open_children,
            vec!["bd-epic.1".to_string(), "bd-epic.2".to_string()],
            "closed child should be excluded; open and in_progress should be returned"
        );
    }

    #[test]
    fn dot_notation_children_excludes_grandchildren() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let parent = make_issue("bd-root", "Root", Status::Open, 1, None, now, None);
        let child = make_issue("bd-root.1", "Child", Status::Open, 1, None, now, None);
        let grandchild = make_issue(
            "bd-root.1.1",
            "Grandchild",
            Status::Open,
            1,
            None,
            now,
            None,
        );
        storage.create_issue(&parent, "t").unwrap();
        storage.create_issue(&child, "t").unwrap();
        storage.create_issue(&grandchild, "t").unwrap();

        let open_children = storage.get_open_dot_notation_children("bd-root").unwrap();
        assert_eq!(
            open_children,
            vec!["bd-root.1".to_string()],
            "grandchildren (bd-root.1.1) must not be returned as direct children of bd-root"
        );
    }

    #[test]
    fn dot_notation_children_returns_empty_when_none() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let parent = make_issue("bd-solo", "Solo", Status::Open, 1, None, now, None);
        let unrelated = make_issue("bd-other", "Other", Status::Open, 1, None, now, None);
        storage.create_issue(&parent, "t").unwrap();
        storage.create_issue(&unrelated, "t").unwrap();

        let open_children = storage.get_open_dot_notation_children("bd-solo").unwrap();
        assert!(
            open_children.is_empty(),
            "parent with no dot-notation children should return empty vec, got {open_children:?}"
        );
    }

    #[test]
    fn dot_notation_children_escapes_like_specials_in_parent_id() {
        // If escape_like_pattern were bypassed, a parent id containing `_`
        // would match any single character via LIKE. Pin the escaping behavior
        // so `a_b-1.1` matches but `axb-1.1` does not.
        let mut storage = SqliteStorage::open_memory().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap();
        let parent = make_issue(
            "a_b-1",
            "Parent with underscore",
            Status::Open,
            1,
            None,
            now,
            None,
        );
        let child = make_issue("a_b-1.1", "Real child", Status::Open, 1, None, now, None);
        let decoy = make_issue("axb-1.1", "Decoy", Status::Open, 1, None, now, None);
        storage.create_issue(&parent, "t").unwrap();
        storage.create_issue(&child, "t").unwrap();
        storage.create_issue(&decoy, "t").unwrap();

        let open_children = storage.get_open_dot_notation_children("a_b-1").unwrap();
        assert_eq!(
            open_children,
            vec!["a_b-1.1".to_string()],
            "underscore in parent id must be escaped so `a_b-1.1` matches but `axb-1.1` does not"
        );
    }

    #[test]
    fn jittered_backoff_increases_with_attempt() {
        let b0 = SqliteStorage::jittered_backoff(50, 0);
        let b1 = SqliteStorage::jittered_backoff(50, 1);
        let b2 = SqliteStorage::jittered_backoff(50, 2);
        assert!((25..=75).contains(&b0), "attempt 0: {b0}");
        assert!((50..=150).contains(&b1), "attempt 1: {b1}");
        assert!((100..=300).contains(&b2), "attempt 2: {b2}");
    }

    #[test]
    fn jittered_backoff_zero_base_returns_zero() {
        let b = SqliteStorage::jittered_backoff(0, 0);
        assert_eq!(b, 0, "zero base should not underflow");
    }

    #[test]
    fn write_transaction_propagates_body_error() {
        let dir = TempDir::new().unwrap();
        let mut storage = SqliteStorage::open(&dir.path().join("test.db")).unwrap();
        let result: Result<()> = storage
            .with_write_transaction(|_| Err(crate::error::BeadsError::Config("test error".into())));
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("test error"),
            "should propagate body error, got: {err_msg}"
        );
    }

    #[test]
    fn open_auto_migrates_legacy_integer_datetimes_and_done_status() {
        // Simulate the exact on-disk corruption observed in the wild: a v5
        // DB (pre-migration user_version) with integer-typed DATETIME
        // columns on some rows and a legacy Go-beads "done" status on
        // another. Opening the DB with the fixed binary must auto-promote
        // to v6 and normalize both.
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("legacy.db");
        {
            let storage = SqliteStorage::open(&db_path).unwrap();
            // Drop the user_version so we exercise the v5 → v6 path.
            storage.conn.execute("PRAGMA user_version = 5").unwrap();
            storage
                .conn
                .execute(
                    "INSERT INTO issues (id, title, status, priority, issue_type, \
                      created_at, updated_at, closed_at, close_reason) VALUES \
                      ('legacy-int', 'integer timestamps', 'closed', 2, 'task', \
                      '2026-04-19T21:34:04.000000000Z', 1776651488000000, 1776651488000000, 'done')",
                )
                .unwrap();
            storage
                .conn
                .execute(
                    "INSERT INTO issues (id, title, status, priority, issue_type, \
                      created_at, updated_at) VALUES \
                      ('legacy-done', 'bd done status', 'done', 2, 'task', \
                      '2026-04-02T20:00:00Z', '2026-04-03T01:00:00Z')",
                )
                .unwrap();
        }

        // Reopen — this triggers run_migrations(), which must repair both.
        let storage = SqliteStorage::open(&db_path).unwrap();

        let row = storage
            .conn
            .query_row(
                "SELECT typeof(updated_at), typeof(closed_at), status FROM issues WHERE id='legacy-int'",
            )
            .unwrap();
        assert_eq!(
            row.get(0).and_then(SqliteValue::as_text),
            Some("text"),
            "updated_at should have been rewritten to TEXT"
        );
        assert_eq!(
            row.get(1).and_then(SqliteValue::as_text),
            Some("text"),
            "closed_at should have been rewritten to TEXT"
        );
        assert_eq!(row.get(2).and_then(SqliteValue::as_text), Some("closed"));

        let row = storage
            .conn
            .query_row("SELECT status, closed_at FROM issues WHERE id='legacy-done'")
            .unwrap();
        assert_eq!(row.get(0).and_then(SqliteValue::as_text), Some("closed"));
        assert!(
            row.get(1).and_then(SqliteValue::as_text).is_some(),
            "closed_at should be populated for migrated done issue"
        );

        // The export path must now produce a correct Issue — the reader
        // regression (silent 1970-01-01 / null) is gone.
        let issue = storage
            .get_issue_for_export("legacy-int")
            .unwrap()
            .expect("issue exists");
        assert_eq!(issue.updated_at.year(), 2026);
        assert_eq!(issue.updated_at.month(), 4);
        assert_eq!(issue.updated_at.day(), 20);
        assert!(issue.closed_at.is_some());
        assert_eq!(issue.closed_at.unwrap().year(), 2026);
    }

    #[test]
    fn connection_write_transaction_propagates_body_error() {
        let dir = TempDir::new().unwrap();
        let storage = SqliteStorage::open(&dir.path().join("test.db")).unwrap();
        let result: Result<()> =
            SqliteStorage::with_connection_write_transaction(&storage.conn, |_| {
                Err(crate::error::BeadsError::Config("conn test error".into()))
            });
        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("conn test error"),
            "should propagate body error, got: {err_msg}"
        );
    }

    // ========================================================================
    // Issue #312, Layer 3 — attribution capture-only tests
    // ========================================================================

    #[test]
    fn pending_attribution_is_stamped_onto_create_event() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 6, 7, 12, 0, 0).unwrap();
        let issue = make_issue(
            "bd-attr-1",
            "Attributed create",
            Status::Open,
            2,
            None,
            now,
            None,
        );

        storage.set_pending_event_attribution(EventAttribution::new(
            Some("agent-7"),
            Some("codex-cli"),
            Some("opus-4"),
        ));
        storage.create_issue(&issue, "tester").expect("create");

        let events = storage.get_events("bd-attr-1", 0).expect("events");
        let created = events
            .iter()
            .find(|e| e.event_type == EventType::Created)
            .expect("created event present");
        assert_eq!(created.agent_name.as_deref(), Some("agent-7"));
        assert_eq!(created.harness.as_deref(), Some("codex-cli"));
        assert_eq!(created.model.as_deref(), Some("opus-4"));
    }

    #[test]
    fn pending_attribution_is_stamped_onto_update_status_change_without_blocking() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 6, 7, 12, 0, 0).unwrap();
        let issue = make_issue(
            "bd-attr-2",
            "Attributed update",
            Status::Open,
            2,
            None,
            now,
            None,
        );
        storage.create_issue(&issue, "tester").expect("create");

        storage.set_pending_event_attribution(EventAttribution::new(
            Some("agent-9"),
            None,
            Some("opus-4"),
        ));
        let update = IssueUpdate {
            status: Some(Status::InProgress),
            ..Default::default()
        };
        // The transition must NOT be gated/rejected by the attribution; it is a
        // recorded audit trail only.
        let updated = storage
            .update_issue("bd-attr-2", &update, "tester")
            .expect("status change should not be blocked by attribution");
        assert_eq!(updated.status, Status::InProgress);

        let events = storage.get_events("bd-attr-2", 0).expect("events");
        let status_event = events
            .iter()
            .find(|e| e.event_type == EventType::StatusChanged)
            .expect("status_changed event present");
        assert_eq!(status_event.agent_name.as_deref(), Some("agent-9"));
        assert!(status_event.harness.is_none());
        assert_eq!(status_event.model.as_deref(), Some("opus-4"));
    }

    #[test]
    fn absent_attribution_records_no_values_and_does_not_leak() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 6, 7, 12, 0, 0).unwrap();

        // First create stages attribution...
        storage.set_pending_event_attribution(EventAttribution::new(Some("agent-x"), None, None));
        let first = make_issue("bd-attr-3", "First", Status::Open, 2, None, now, None);
        storage
            .create_issue(&first, "tester")
            .expect("create first");

        // ...the second create stages NONE, so it must record no attribution
        // (the prior staging must not leak into this unrelated mutation).
        let second = make_issue("bd-attr-4", "Second", Status::Open, 2, None, now, None);
        storage
            .create_issue(&second, "tester")
            .expect("create second");

        let second_events = storage.get_events("bd-attr-4", 0).expect("events");
        let created = second_events
            .iter()
            .find(|e| e.event_type == EventType::Created)
            .expect("created event present");
        assert!(created.agent_name.is_none());
        assert!(created.harness.is_none());
        assert!(created.model.is_none());
    }

    #[test]
    fn event_attribution_normalizes_blank_inputs_to_none() {
        let attribution = EventAttribution::new(Some("  "), Some(""), Some("opus-4"));
        assert!(attribution.agent_name.is_none());
        assert!(attribution.harness.is_none());
        assert_eq!(attribution.model.as_deref(), Some("opus-4"));
        assert!(!attribution.is_empty());
        assert!(EventAttribution::new(None, None, None).is_empty());
    }

    // ---- #312 hardening (F1): attribution survives a non-committing mutation
    // and is consumed only after a successful commit -----------------------

    #[test]
    fn pending_attribution_survives_failed_mutation_and_stamps_on_retry() {
        // Models the JSONL-recovery retry path: the first `mutate()` does NOT
        // commit (its closure errors → rollback), so the staged attribution must
        // remain available for the *next* `mutate()` to stamp. Previously the
        // start-of-`mutate()` `.take()` consumed it on the failed attempt,
        // dropping attribution from the recovered write (F1).
        let mut storage = SqliteStorage::open_memory().unwrap();

        storage.set_pending_event_attribution(EventAttribution::new(
            Some("agent-retry"),
            None,
            Some("opus-4"),
        ));

        // A mutation whose closure fails with a non-transient error: it rolls
        // back and never commits. The staged slot must be left intact.
        let failed: Result<()> = storage.mutate("noop_fail", "tester", |_conn, _ctx| {
            Err(BeadsError::Config("simulated mutation failure".into()))
        });
        assert!(failed.is_err(), "mutation closure error should propagate");
        assert!(
            storage.pending_event_attribution.is_some(),
            "attribution must survive a non-committing mutation so the recovery \
             retry can still stamp it",
        );

        // The retry: a committing mutation must now record the (still-staged)
        // attribution onto its events.
        let now = Utc.with_ymd_and_hms(2026, 6, 7, 12, 0, 0).unwrap();
        let issue = make_issue("bd-attr-retry", "Retried", Status::Open, 2, None, now, None);
        storage.create_issue(&issue, "tester").expect("create");

        let events = storage.get_events("bd-attr-retry", 0).expect("events");
        let created = events
            .iter()
            .find(|e| e.event_type == EventType::Created)
            .expect("created event present");
        assert_eq!(created.agent_name.as_deref(), Some("agent-retry"));
        assert_eq!(created.model.as_deref(), Some("opus-4"));

        // And after the committing mutation, the slot is cleared (consumed once).
        assert!(
            storage.pending_event_attribution.is_none(),
            "attribution must be consumed by exactly one committing mutation",
        );
    }

    // ---- #312 hardening (F2): no-op update clears the staged slot ----------

    #[test]
    fn empty_update_clears_pending_attribution_so_it_does_not_leak() {
        let mut storage = SqliteStorage::open_memory().unwrap();
        let now = Utc.with_ymd_and_hms(2026, 6, 7, 12, 0, 0).unwrap();
        let issue = make_issue(
            "bd-attr-noop",
            "No-op target",
            Status::Open,
            2,
            None,
            now,
            None,
        );
        storage.create_issue(&issue, "tester").expect("create");

        // Stage attribution, then call update_issue with EMPTY updates: this
        // early-returns without calling `mutate()`, but must still drain the
        // staged slot so it cannot leak onto the next, unrelated mutation (F2).
        storage.set_pending_event_attribution(EventAttribution::new(
            Some("agent-leak"),
            None,
            None,
        ));
        let empty = IssueUpdate::default();
        storage
            .update_issue("bd-attr-noop", &empty, "tester")
            .expect("empty update is a no-op read");
        assert!(
            storage.pending_event_attribution.is_none(),
            "empty-update no-op must clear the staged attribution",
        );

        // Confirm no leak: a subsequent create (which stages nothing) records
        // no attribution.
        let next = make_issue("bd-attr-next", "Next", Status::Open, 2, None, now, None);
        storage.create_issue(&next, "tester").expect("create next");
        let events = storage.get_events("bd-attr-next", 0).expect("events");
        let created = events
            .iter()
            .find(|e| e.event_type == EventType::Created)
            .expect("created event present");
        assert!(created.agent_name.is_none());
        assert!(created.harness.is_none());
        assert!(created.model.is_none());
    }
}
