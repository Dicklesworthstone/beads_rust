//! Self-healing for tracker databases left behind on an older schema.
//!
//! An older database remains authoritative even when every issue is present
//! in `issues.jsonl`. Event history, configuration, relation attribution,
//! workflow records, hidden rowids, and operator schema can exist only in the
//! database. An issue-level JSONL comparison cannot prove their preservation.
//! Reads therefore observe the original database or a labelled JSONL view;
//! mutations require a reviewed migration with complete preservation checks.
//!
//! This module separates the two cases:
//!
//! 1. [`audit_stale_database`] reads the old database schema-agnostically
//!    (plain `SELECT`s over whatever columns exist) and compares every issue
//!    row against the JSONL snapshot. An issue is *represented* when the JSONL
//!    carries it as a tombstone, carries equivalent content, or carries a
//!    copy at least as new while the database never marked the row dirty and
//!    every local comment occurrence survives in the JSONL. A newer issue
//!    timestamp alone cannot supersede an append-only comment history.
//!    Anything else — absent from the JSONL, an unflushed (dirty) edit that
//!    differs, missing comments, a row newer than its JSONL copy, or a row
//!    that cannot be read — is database-only data.
//! 2. [`heal_stale_schema`] attempts the reviewed in-place migration for
//!    supported source schemas (lossless, recovery bundle, undo). Automatic
//!    startup never falls back to JSONL reconstruction: matching issue rows
//!    cannot establish preservation of audit history, configuration, workflow
//!    state, or operator-owned schema. An absent or refused migration leaves
//!    the original database authoritative and reports the reason.
//! 3. An explicit `br doctor migrate-schema heal` follows the same rule. Its
//!    audit diagnoses database-only issues, relations, history, configuration,
//!    workflow state, and operator-owned schema, but cannot turn a JSONL
//!    reconstruction into a reviewed migration. Without a complete crash-safe
//!    installer, even replayable issue rows do not authorize replacing the live
//!    database. `--discard-db-only` does not bypass that boundary. A retained
//!    backup alone is not permission to remove authoritative live data.

mod rebuild_preflight;

use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use fsqlite_types::SqliteValue;
use serde::Serialize;
use serde_json::{Map, Value};

use crate::config;
use crate::error::{BeadsError, Result};
use crate::franken_sync::Connection;
use crate::franken_sync::compat::{OpenFlags, open_with_flags};
use crate::model::{Comment, Dependency, Issue, Status};
use crate::storage::schema::{CURRENT_SCHEMA_VERSION, REVIEWED_MIGRATION_SOURCE_VERSIONS};
use crate::sync::{DatabaseFamilyWriteLock, JsonlSourceSnapshot, PreservedIssue};
use rebuild_preflight::{audit_local_replay_coverage, audit_unrepresented_state};

/// Most database-only issue ids named in a refusal before eliding the rest.
const DB_ONLY_IDS_SHOWN: usize = 10;

/// The explicit command every refusal names.
pub const HEAL_COMMAND: &str = "br doctor migrate-schema heal";

/// Why an issue row counts as database-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DbOnlyReason {
    /// The JSONL has no record with this id.
    AbsentFromJsonl,
    /// The row is marked dirty (never flushed) and differs from the JSONL.
    UnflushedEdit,
    /// The row is not marked dirty but is newer than, and differs from, the JSONL.
    NewerThanJsonl,
    /// The JSONL supersedes the issue revision but lacks local comment occurrences.
    CommentsAbsentFromJsonl,
    /// The row could not be decoded, so its representation cannot be proven.
    Unreadable,
}

impl DbOnlyReason {
    const fn describe(self) -> &'static str {
        match self {
            Self::AbsentFromJsonl => "absent from JSONL",
            Self::UnflushedEdit => "unflushed edit",
            Self::NewerThanJsonl => "newer than JSONL",
            Self::CommentsAbsentFromJsonl => "comments absent from JSONL",
            Self::Unreadable => "unreadable row",
        }
    }
}

/// One database-only issue found by the audit.
#[derive(Debug, Clone, Serialize)]
pub struct DbOnlyIssue {
    pub id: String,
    pub reason: DbOnlyReason,
    /// Whether this row can be decoded for a preservation plan. This is not
    /// permission to reconstruct the live database. A comments-only plan
    /// keeps JSONL's newer issue payload while identifying the local history.
    pub restorable: bool,
}

/// Relation rows (`labels`, `dependencies`, `comments`) whose owning issue row
/// is gone, typically left by deletes that older releases ran without
/// foreign-key enforcement.
#[derive(Debug, Clone, Serialize)]
pub struct OrphanRelationRows {
    pub table: &'static str,
    pub rows: usize,
}

/// Result of comparing a stale database with the JSONL snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct StaleSchemaAudit {
    pub from_version: u32,
    pub to_version: u32,
    pub db_issue_count: usize,
    pub jsonl_issue_count: usize,
    pub db_only: Vec<DbOnlyIssue>,
    /// Well-formed relation rows without an owning issue row. A reviewed
    /// in-place migration keeps them; an issue-only JSONL replay cannot.
    pub orphan_relations: Vec<OrphanRelationRows>,
    /// Authoritative tables, columns, or schema objects that the JSONL replay
    /// cannot reproduce. Unlike unreadable issue payloads and orphan relations,
    /// these are never covered by `--discard-db-only` authorization.
    pub rebuild_blockers: Vec<String>,
    /// A reason the whole database cannot be audited (not a beads tracker,
    /// no JSONL, unparseable JSONL). Always blocks a JSONL rebuild, but cannot
    /// prevent a reviewed lossless migration that never imports JSONL.
    pub unprovable: Option<String>,
    #[serde(skip)]
    restorable: Vec<PreservedIssue>,
}

impl StaleSchemaAudit {
    /// True when the database has no unrepresented issue or auxiliary state.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.unprovable.is_none()
            && self.db_only.is_empty()
            && self.orphan_relations.is_empty()
            && self.rebuild_blockers.is_empty()
    }

    /// True when the database holds relation rows without an owning issue.
    #[must_use]
    pub fn has_orphan_relations(&self) -> bool {
        !self.orphan_relations.is_empty()
    }

    /// Whether an automatic heal may be attempted. Reviewed migrations have
    /// their own raw-row preservation checks and do not need a JSONL audit.
    /// This is not permission to rebuild after a migration fails.
    #[must_use]
    pub fn automatic_heal_allowed(&self) -> bool {
        REVIEWED_MIGRATION_SOURCE_VERSIONS.contains(&self.from_version)
    }

    fn orphan_summary(&self) -> String {
        self.orphan_relations
            .iter()
            .map(|orphans| format!("{} {} row(s)", orphans.rows, orphans.table))
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn db_only_summary(&self) -> String {
        let mut shown: Vec<String> = self
            .db_only
            .iter()
            .take(DB_ONLY_IDS_SHOWN)
            .map(|issue| format!("{} ({})", issue.id, issue.reason.describe()))
            .collect();
        if self.db_only.len() > DB_ONLY_IDS_SHOWN {
            shown.push(format!(
                "and {} more",
                self.db_only.len() - DB_ONLY_IDS_SHOWN
            ));
        }
        shown.join(", ")
    }

    /// Explain which database-only issues can be decoded without implying
    /// that a complete crash-safe reconstruction has been established.
    fn restoration_notice(&self) -> String {
        if self.restorable.is_empty() {
            return "No recoverable database-only issues could be decoded".to_owned();
        }
        let mut ids: Vec<&str> = self
            .restorable
            .iter()
            .map(|entry| entry.issue.id.as_str())
            .collect();
        ids.sort_unstable();
        let shown = ids
            .iter()
            .take(DB_ONLY_IDS_SHOWN)
            .copied()
            .collect::<Vec<_>>()
            .join(", ");
        let remaining = if ids.len() > DB_ONLY_IDS_SHOWN {
            format!(", and {} more", ids.len() - DB_ONLY_IDS_SHOWN)
        } else {
            String::new()
        };
        format!(
            "{} recoverable database-only issue(s) ({shown}{remaining}) can be decoded; \
             they remain in the original database because no reconstruction is authorized",
            ids.len()
        )
    }
}

/// How [`heal_stale_schema`] was invoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealMode {
    /// Mutating command startup: require a reviewed lossless migration.
    /// Neither missing migration paths nor refusals authorize reconstruction.
    Automatic,
    /// `br doctor migrate-schema heal`: attempt a reviewed migration or return
    /// a preservation audit. The legacy discard option never authorizes an
    /// unreviewed live-database reconstruction.
    Explicit { discard_db_only: bool },
}

/// What the heal did.
#[derive(Debug, Clone, Serialize)]
pub struct HealOutcome {
    pub from_version: u32,
    pub to_version: u32,
    /// `migrated`: only a reviewed lossless migration can succeed.
    pub action: &'static str,
    /// Where the pre-heal database family was preserved.
    pub backup: String,
}

impl HealOutcome {
    /// One-line human notice for stderr.
    #[must_use]
    pub fn notice(&self) -> String {
        format!(
            "br: tracker database was on schema {} (this br uses {}); migrated it in place — backup at {}",
            self.from_version, self.to_version, self.backup
        )
    }
}

/// Cheap startup probe: `Some(found)` when the database exists and its
/// effective schema is older than this binary's.
///
/// Reads the 100-byte header first and opens the engine only when the header
/// is behind, because a migration may still live in an uncheckpointed WAL.
///
/// # Errors
///
/// Returns an error if the header is unstable or the engine probe fails.
pub fn stale_schema_version(db_path: &Path) -> Result<Option<u32>> {
    let current = current_version();
    let Some(header) = crate::storage::sqlite::database_header_user_version(db_path) else {
        return Ok(None);
    };
    if header >= current {
        return Ok(None);
    }
    let effective =
        crate::storage::sqlite::effective_database_user_version(db_path)?.unwrap_or(header);
    Ok((effective < current).then_some(effective))
}

fn current_version() -> u32 {
    u32::try_from(CURRENT_SCHEMA_VERSION).unwrap_or(u32::MAX)
}

/// Refusal naming the one command that resolves a stale database holding
/// database-only data (or one that cannot be audited).
#[must_use]
pub fn refusal_error(audit: &StaleSchemaAudit) -> BeadsError {
    let found = i32::try_from(audit.from_version).unwrap_or(i32::MAX);
    let why = audit.unprovable.clone().unwrap_or_else(|| {
        let mut why = Vec::new();
        if !audit.db_only.is_empty() {
            why.push(format!(
                "{} issue(s) exist only in the database: {}",
                audit.db_only.len(),
                audit.db_only_summary()
            ));
        }
        if audit.has_orphan_relations() {
            why.push(format!(
                "the database holds relation rows without an owning issue ({}), which a \
                 rebuild from issues.jsonl cannot carry",
                audit.orphan_summary()
            ));
        }
        if !audit.rebuild_blockers.is_empty() {
            why.push(format!(
                "JSONL cannot preserve the complete database: {}",
                audit.rebuild_blockers.join("; ")
            ));
        }
        if why.is_empty() {
            why.push(format!(
                "there is no reviewed lossless migration for schema {}",
                audit.from_version
            ));
        }
        why.join("; ")
    });
    BeadsError::WithContext {
        context: format!(
            "schema upgrade refused because {why}. The rebuild was not started; the original \
             database remains authoritative. Reconstructing an existing tracker from JSONL \
             has no reviewed crash-safe installer, including with --discard-db-only. Keep the \
             original database and use a release that supports its schema, or a reviewed \
             lossless migration. `{HEAL_COMMAND} --dry-run` shows the preservation audit; \
             `br doctor migrate-schema plan --json` diagnoses reviewed migration support"
        ),
        source: Box::new(BeadsError::SchemaMismatch {
            expected: CURRENT_SCHEMA_VERSION,
            found,
        }),
    }
}

// ---------------------------------------------------------------------------
// Audit
// ---------------------------------------------------------------------------

/// Compare the stale database with the JSONL snapshot.
///
/// `jsonl` is `None` when the workspace has no JSONL file.
///
/// # Errors
///
/// Returns an error for engine access failures or records whose identity or
/// ownership cannot be read without loss. Undecodable issue payloads and
/// their relations are reported as unreadable through [`StaleSchemaAudit`].
pub(crate) fn audit_stale_database(
    db_path: &Path,
    jsonl: Option<&JsonlSourceSnapshot>,
) -> Result<StaleSchemaAudit> {
    // A schema-agnostic audit is still a live WAL reader. Keep checkpoints
    // and recovery from treating this connection as an absent opener.
    let _opener_lease = crate::sync::DatabaseOpenerLease::register(db_path)?;
    let conn = open_with_flags(
        db_path.to_string_lossy().as_ref(),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(BeadsError::Database)?;
    // Do not combine tables from different committed generations when a
    // non-br SQLite client writes concurrently with this observational audit.
    conn.execute("BEGIN DEFERRED TRANSACTION")?;
    let result = audit_connection(&conn, jsonl.map(JsonlSourceSnapshot::reader));
    let rolled_back = conn.execute("ROLLBACK");
    let closed = conn.close();
    let audit = result?;
    rolled_back?;
    closed?;
    Ok(audit)
}

#[allow(
    clippy::too_many_lines,
    reason = "the observational audit keeps issue and relation classification in one stable snapshot"
)]
fn audit_connection(
    conn: &Connection,
    jsonl_reader: Option<impl BufRead>,
) -> Result<StaleSchemaAudit> {
    let from_version = query_user_version(conn)?;
    let mut audit = StaleSchemaAudit {
        from_version,
        to_version: current_version(),
        db_issue_count: 0,
        jsonl_issue_count: 0,
        db_only: Vec::new(),
        orphan_relations: Vec::new(),
        rebuild_blockers: Vec::new(),
        unprovable: None,
        restorable: Vec::new(),
    };

    let tables = table_names(conn)?;
    let issue_columns = if tables.contains("issues") {
        column_names(conn, "issues")?
    } else {
        Vec::new()
    };
    if !["id", "title", "status", "updated_at"]
        .iter()
        .all(|required| issue_columns.iter().any(|column| column == required))
    {
        audit.unprovable = Some(
            "the database has no recognizable beads `issues` table, so its contents cannot be \
             compared with the JSONL"
                .to_string(),
        );
        return Ok(audit);
    }

    let (db_issues, orphan_relations) = read_legacy_issues(conn, &tables, &issue_columns)?;
    audit.db_issue_count = db_issues.len();
    audit.orphan_relations = orphan_relations;
    audit.rebuild_blockers = audit_unrepresented_state(conn, &tables, &issue_columns)?;
    let dirty: Option<HashSet<String>> = if tables.contains("dirty_issues") {
        Some(
            conn.query("SELECT issue_id FROM dirty_issues")?
                .iter()
                .map(|row| required_legacy_text(row.get(0), "dirty_issues", "issue_id"))
                .collect::<Result<HashSet<_>>>()?,
        )
    } else {
        // Without dirty tracking every differing row must be treated as
        // possibly unflushed.
        None
    };

    let jsonl_issues = match jsonl_reader {
        Some(reader) => match parse_jsonl(reader) {
            Ok(issues) => issues,
            Err(error) => {
                audit.unprovable = Some(format!("issues.jsonl could not be parsed ({error})"));
                return Ok(audit);
            }
        },
        None if db_issues.is_empty() => HashMap::new(),
        None => {
            audit.unprovable = Some(format!(
                "there is no issues.jsonl to compare the database's {} issue(s) against",
                db_issues.len()
            ));
            return Ok(audit);
        }
    };
    audit.jsonl_issue_count = jsonl_issues.len();

    for legacy in db_issues {
        let id = legacy.id.clone();
        let Some(preserved) = legacy.decoded else {
            audit.db_only.push(DbOnlyIssue {
                id,
                reason: DbOnlyReason::Unreadable,
                restorable: false,
            });
            continue;
        };
        let is_dirty = dirty.as_ref().is_none_or(|dirty| dirty.contains(&id));
        let verdict = classify(&preserved.issue, jsonl_issues.get(&id), is_dirty);
        audit_local_replay_coverage(
            &preserved.issue,
            jsonl_issues.get(&id),
            verdict,
            &mut audit.rebuild_blockers,
        );
        if let Some((reason, restorable)) = verdict {
            audit.db_only.push(DbOnlyIssue {
                id,
                reason,
                restorable,
            });
            if restorable {
                let preserved = if reason == DbOnlyReason::CommentsAbsentFromJsonl {
                    let jsonl = jsonl_issues.get(&preserved.issue.id).ok_or_else(|| {
                        BeadsError::Config(
                            "comment-only restore lost its audited JSONL issue".to_owned(),
                        )
                    })?;
                    comment_only_restore(preserved, jsonl)?
                } else {
                    preserved
                };
                audit.restorable.push(preserved);
            }
        }
    }
    Ok(audit)
}

/// Keep the winning JSONL payload while carrying the missing local history
/// into the shared comment-completion step. Restoring the entire older issue
/// would undo a peer's close, title, labels, or dependency edits. Local path
/// provenance is not a cross-clone scalar and keeps its known local value.
fn comment_only_restore(mut local: PreservedIssue, jsonl: &Issue) -> Result<PreservedIssue> {
    if local.issue.id != jsonl.id || jsonl.status == Status::Tombstone {
        return Err(BeadsError::Config(
            "comment-only restore requires the same non-tombstone JSONL issue".to_owned(),
        ));
    }
    // As in the audit's scalar comparison, normalize legacy timestamp
    // precision. A reused ID with a different creation time is another issue,
    // not permission to transfer the original issue's comments (#512).
    if micros(local.issue.created_at) != micros(jsonl.created_at) {
        return Err(BeadsError::SyncConflict {
            message: format!(
                "schema-heal ID collision for {}: creation timestamps differ; cannot assign \
                 local comments to a different issue. Reconcile the issue identities before \
                 healing; the rebuild was not started",
                jsonl.id
            ),
        });
    }
    let comments = local.comments.as_ref().ok_or_else(|| {
        BeadsError::Config(format!(
            "comment-only restore cannot certify unreadable comments for {}",
            local.issue.id
        ))
    })?;
    let mut issue = jsonl.clone();
    issue.comments.clone_from(comments);
    if local.issue.source_repo_path.is_some() {
        issue
            .source_repo_path
            .clone_from(&local.issue.source_repo_path);
    }
    local.labels = Some(issue.labels.clone());
    local.dependencies = Some(issue.dependencies.clone());
    local.issue = issue;
    Ok(local)
}

/// `None` when represented; otherwise the reason and whether the row can be
/// decoded into a preservation plan. This never authorizes installation.
fn classify(db: &Issue, jsonl: Option<&Issue>, is_dirty: bool) -> Option<(DbOnlyReason, bool)> {
    let Some(jsonl) = jsonl else {
        return Some((DbOnlyReason::AbsentFromJsonl, true));
    };
    // A flushed deletion wins over any local state (import tombstone guard).
    if jsonl.status == Status::Tombstone || issues_equivalent(db, jsonl) {
        return None;
    }
    let db_newer = micros(db.updated_at) > micros(jsonl.updated_at);
    if !db_newer
        && !multiset_is_subset(
            db.comments.iter().map(Comment::sync_key),
            jsonl.comments.iter().map(Comment::sync_key),
        )
    {
        // Scalar last-write-wins cannot prove an append-only history was
        // carried. Include this issue in the restore plan, but project the
        // winning JSONL scalars instead of replaying the superseded row.
        return Some((DbOnlyReason::CommentsAbsentFromJsonl, true));
    }
    if is_dirty {
        // Re-add only when the local edit is newer; an older unflushed edit
        // was superseded by the JSONL and stays in the backup (#394 rule).
        return Some((DbOnlyReason::UnflushedEdit, db_newer));
    }
    db_newer.then_some((DbOnlyReason::NewerThanJsonl, true))
}

fn micros(value: DateTime<Utc>) -> i64 {
    value.timestamp_micros()
}

fn opt_micros(value: Option<DateTime<Utc>>) -> Option<i64> {
    value.map(micros)
}

fn norm(value: Option<&String>) -> Option<&str> {
    value.map(String::as_str).filter(|text| !text.is_empty())
}

/// Content equivalence over what the JSONL carries: the database copy must
/// hold nothing the JSONL copy lacks. Compare the complete synced scalar
/// payload, not just display fields. Machine-local source_repo_path and
/// derived/audit-only fields follow Issue::sync_equals and are not sync keys.
fn issues_equivalent(db: &Issue, jsonl: &Issue) -> bool {
    let scalar_fields_match = db.id == jsonl.id
        && db.title == jsonl.title
        && norm(db.description.as_ref()) == norm(jsonl.description.as_ref())
        && norm(db.design.as_ref()) == norm(jsonl.design.as_ref())
        && norm(db.acceptance_criteria.as_ref()) == norm(jsonl.acceptance_criteria.as_ref())
        && norm(db.prerequisites.as_ref()) == norm(jsonl.prerequisites.as_ref())
        && norm(db.notes.as_ref()) == norm(jsonl.notes.as_ref())
        && db.status == jsonl.status
        && db.priority == jsonl.priority
        && db.issue_type == jsonl.issue_type
        && norm(db.assignee.as_ref()) == norm(jsonl.assignee.as_ref())
        && norm(db.owner.as_ref()) == norm(jsonl.owner.as_ref())
        && db.estimated_minutes == jsonl.estimated_minutes
        && norm(db.created_by.as_ref()) == norm(jsonl.created_by.as_ref())
        && norm(db.close_reason.as_ref()) == norm(jsonl.close_reason.as_ref())
        && norm(db.closed_by_session.as_ref()) == norm(jsonl.closed_by_session.as_ref())
        && norm(db.external_ref.as_ref()) == norm(jsonl.external_ref.as_ref())
        && norm(db.source_system.as_ref()) == norm(jsonl.source_system.as_ref())
        && db.source_repo.as_deref().unwrap_or(".") == jsonl.source_repo.as_deref().unwrap_or(".")
        && norm(db.agent_context.as_ref()) == norm(jsonl.agent_context.as_ref())
        && norm(db.deleted_by.as_ref()) == norm(jsonl.deleted_by.as_ref())
        && norm(db.delete_reason.as_ref()) == norm(jsonl.delete_reason.as_ref())
        && norm(db.original_type.as_ref()) == norm(jsonl.original_type.as_ref())
        && db.compaction_level.unwrap_or(0) == jsonl.compaction_level.unwrap_or(0)
        && norm(db.compacted_at_commit.as_ref()) == norm(jsonl.compacted_at_commit.as_ref())
        && db.original_size == jsonl.original_size
        && norm(db.sender.as_ref()) == norm(jsonl.sender.as_ref())
        && db.ephemeral == jsonl.ephemeral
        && db.pinned == jsonl.pinned
        && db.is_template == jsonl.is_template
        && micros(db.created_at) == micros(jsonl.created_at)
        && micros(db.updated_at) == micros(jsonl.updated_at)
        && opt_micros(db.closed_at) == opt_micros(jsonl.closed_at)
        && opt_micros(db.due_at) == opt_micros(jsonl.due_at)
        && opt_micros(db.defer_until) == opt_micros(jsonl.defer_until)
        && opt_micros(db.deleted_at) == opt_micros(jsonl.deleted_at)
        && opt_micros(db.compacted_at) == opt_micros(jsonl.compacted_at);
    if !scalar_fields_match {
        return false;
    }
    let jsonl_labels: HashSet<&str> = jsonl.labels.iter().map(String::as_str).collect();
    db.labels
        .iter()
        .all(|label| jsonl_labels.contains(label.as_str()))
        && multiset_is_subset(
            db.dependencies.iter().map(dependency_payload_key),
            jsonl.dependencies.iter().map(dependency_payload_key),
        )
        && multiset_is_subset(
            db.comments.iter().map(Comment::sync_key),
            jsonl.comments.iter().map(Comment::sync_key),
        )
}

/// Defaulted relation fields have the same persisted meaning, but populated
/// metadata, thread ownership, creator and timestamp must all survive. The
/// old (target, type) key certified losses in those fields as equivalence.
fn dependency_payload_key(
    dependency: &Dependency,
) -> (
    &str,
    &str,
    &str,
    DateTime<Utc>,
    Option<&str>,
    &str,
    Option<&str>,
) {
    (
        &dependency.issue_id,
        &dependency.depends_on_id,
        dependency.dep_type.as_str(),
        dependency.created_at,
        norm(dependency.created_by.as_ref()),
        dependency
            .metadata
            .as_deref()
            .filter(|text| !text.trim().is_empty())
            .unwrap_or("{}"),
        norm(dependency.thread_id.as_ref()),
    )
}

/// A source superset is safe, but one serialized child cannot certify two
/// distinct stored occurrences. Comment rowids are deliberately not keys:
/// they are database-local; Comment::sync_key supplies the portable identity.
fn multiset_is_subset<K: Eq + std::hash::Hash>(
    mut required: impl Iterator<Item = K>,
    present: impl Iterator<Item = K>,
) -> bool {
    let mut counts = HashMap::<K, usize>::new();
    for key in present {
        *counts.entry(key).or_default() += 1;
    }
    required.all(|key| {
        let Some(count) = counts.get_mut(&key) else {
            return false;
        };
        if *count == 0 {
            return false;
        }
        *count -= 1;
        true
    })
}

fn parse_jsonl(mut reader: impl BufRead) -> Result<HashMap<String, Issue>> {
    let mut issues = HashMap::new();
    let mut line = String::new();
    let mut line_num = 0usize;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        line_num += 1;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let issue: Issue = serde_json::from_str(trimmed)
            .map_err(|error| BeadsError::Config(format!("line {line_num}: {error}")))?;
        if issue.id.trim().is_empty() {
            return Err(BeadsError::Config(format!(
                "line {line_num}: empty issue id cannot certify a schema-heal source"
            )));
        }
        if issues.insert(issue.id.clone(), issue).is_some() {
            return Err(BeadsError::Config(format!(
                "line {line_num}: duplicate issue id cannot certify a schema-heal source"
            )));
        }
    }
    Ok(issues)
}

// ---------------------------------------------------------------------------
// Schema-agnostic legacy row decoding
// ---------------------------------------------------------------------------

struct LegacyIssue {
    id: String,
    decoded: Option<PreservedIssue>,
}

const TIMESTAMP_FIELDS: &[&str] = &[
    "created_at",
    "updated_at",
    "closed_at",
    "due_at",
    "defer_until",
    "deleted_at",
    "compacted_at",
];
const BOOL_FIELDS: &[&str] = &["ephemeral", "pinned", "is_template", "bypassed_policy"];
const INT_FIELDS: &[&str] = &[
    "priority",
    "estimated_minutes",
    "compaction_level",
    "original_size",
];
const STRING_FIELDS: &[&str] = &[
    "id",
    "title",
    "description",
    "design",
    "acceptance_criteria",
    "prerequisites",
    "notes",
    "status",
    "issue_type",
    "assignee",
    "owner",
    "created_by",
    "close_reason",
    "closed_by_session",
    "bypass_reason",
    "external_ref",
    "source_system",
    "source_repo",
    "source_repo_path",
    "agent_context",
    "deleted_by",
    "delete_reason",
    "original_type",
    "compacted_at_commit",
    "sender",
];

fn read_legacy_issues(
    conn: &Connection,
    tables: &HashSet<String>,
    issue_columns: &[String],
) -> Result<(Vec<LegacyIssue>, Vec<OrphanRelationRows>)> {
    let mut labels: HashMap<String, Vec<String>> = HashMap::new();
    if tables.contains("labels") {
        for row in conn.query("SELECT issue_id, label FROM labels")? {
            let issue_id = required_legacy_text(row.get(0), "labels", "issue_id")?;
            let label = required_legacy_text(row.get(1), "labels", "label")?;
            labels.entry(issue_id).or_default().push(label);
        }
    }
    let mut dependencies = read_related_rows(conn, tables, "dependencies")?;
    let mut comments = read_related_rows(conn, tables, "comments")?;

    let select = issue_columns
        .iter()
        .map(|column| quote_identifier(column))
        .collect::<Vec<_>>()
        .join(", ");
    let rows = conn.query(&format!("SELECT {select} FROM issues"))?;
    let mut issues = Vec::with_capacity(rows.len());
    let mut ids = HashSet::new();
    for row in rows {
        let values: Vec<SqliteValue> = row.values().to_vec();
        let id = required_legacy_text(column_value(issue_columns, &values, "id"), "issues", "id")?;
        if !ids.insert(id.clone()) {
            return Err(BeadsError::Config(
                "duplicate legacy issue ids cannot be preserved by schema healing".to_string(),
            ));
        }
        let issue_dependencies = dependencies.remove(&id).unwrap_or_default();
        let issue_comments = comments.remove(&id).unwrap_or_default();
        let decoded = decode_issue(
            issue_columns,
            &values,
            labels.remove(&id).unwrap_or_default(),
            &issue_dependencies,
            &issue_comments,
        );
        issues.push(LegacyIssue { id, decoded });
    }
    // Well-formed relation rows whose issue row is gone (deletes that older
    // releases ran without foreign-key enforcement) have no issue to compare
    // or restore. Report them rather than refusing the whole audit: the
    // reviewed in-place migration keeps them, whereas JSONL cannot carry them.
    let orphan_relations = [
        ("labels", labels.values().map(Vec::len).sum::<usize>()),
        (
            "dependencies",
            dependencies.values().map(Vec::len).sum::<usize>(),
        ),
        ("comments", comments.values().map(Vec::len).sum::<usize>()),
    ]
    .into_iter()
    .filter(|&(_, rows)| rows > 0)
    .map(|(table, rows)| OrphanRelationRows { table, rows })
    .collect();
    Ok((issues, orphan_relations))
}

fn required_legacy_text(value: Option<&SqliteValue>, table: &str, column: &str) -> Result<String> {
    value
        .and_then(SqliteValue::as_text)
        .filter(|text| !text.trim().is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            BeadsError::Config(format!(
                "legacy {table}.{column} must be nonempty text; refusing a lossy schema heal"
            ))
        })
}

fn column_value<'a>(
    columns: &[String],
    values: &'a [SqliteValue],
    name: &str,
) -> Option<&'a SqliteValue> {
    columns
        .iter()
        .position(|column| column == name)
        .and_then(|index| values.get(index))
}

/// Rows of a relation table keyed by `issue_id`, each as a column map.
fn read_related_rows(
    conn: &Connection,
    tables: &HashSet<String>,
    table: &str,
) -> Result<HashMap<String, Vec<Map<String, Value>>>> {
    let mut related: HashMap<String, Vec<Map<String, Value>>> = HashMap::new();
    if !tables.contains(table) {
        return Ok(related);
    }
    let columns = column_names(conn, table)?;
    let select = columns
        .iter()
        .map(|column| quote_identifier(column))
        .collect::<Vec<_>>()
        .join(", ");
    for row in conn.query(&format!("SELECT {select} FROM {}", quote_identifier(table)))? {
        let issue_id = required_legacy_text(
            column_value(&columns, row.values(), "issue_id"),
            table,
            "issue_id",
        )?;
        let mut object = Map::new();
        for (column, value) in columns.iter().zip(row.values()) {
            if let Some(json) = sqlite_to_json(value) {
                object.insert(column.clone(), json);
            } else if !matches!(value, SqliteValue::Null) {
                return Err(BeadsError::Config(format!(
                    "legacy {table}.{column} cannot be represented in JSON; \
                     refusing a lossy schema heal"
                )));
            }
        }
        related.entry(issue_id).or_default().push(object);
    }
    Ok(related)
}

fn sqlite_to_json(value: &SqliteValue) -> Option<Value> {
    match value {
        SqliteValue::Null => None,
        SqliteValue::Integer(integer) => Some(Value::from(*integer)),
        SqliteValue::Float(float) => serde_json::Number::from_f64(*float).map(Value::Number),
        other => other.as_text().map(|text| Value::String(text.to_owned())),
    }
}

/// Decode one legacy issue row into the current model, or `None` when a
/// populated field or relation cannot be represented. Returning a partial
/// issue would let an omitted child masquerade as data already in JSONL.
fn decode_issue(
    columns: &[String],
    values: &[SqliteValue],
    labels: Vec<String>,
    dependencies: &[Map<String, Value>],
    comments: &[Map<String, Value>],
) -> Option<PreservedIssue> {
    let mut object = Map::new();
    for (column, value) in columns.iter().zip(values) {
        let name = column.as_str();
        if matches!(value, SqliteValue::Null) {
            continue;
        }
        if TIMESTAMP_FIELDS.contains(&name) {
            match legacy_timestamp(value) {
                Some(timestamp) => {
                    object.insert(name.to_owned(), Value::String(timestamp.to_rfc3339()));
                }
                // Legacy optional timestamps may be empty, but a populated
                // value that cannot be decoded is data, not absence.
                None if !matches!(name, "created_at" | "updated_at")
                    && value.as_text().is_some_and(|text| text.trim().is_empty()) => {}
                None => return None,
            }
        } else if BOOL_FIELDS.contains(&name) {
            let flag = match value {
                SqliteValue::Integer(integer) => *integer != 0,
                other => match other.as_text()?.trim() {
                    "1" => true,
                    "0" | "" => false,
                    text if text.eq_ignore_ascii_case("true") => true,
                    text if text.eq_ignore_ascii_case("false") => false,
                    _ => return None,
                },
            };
            object.insert(name.to_owned(), Value::Bool(flag));
        } else if INT_FIELDS.contains(&name) {
            if name != "priority" && value.as_text().is_some_and(|text| text.trim().is_empty()) {
                continue;
            }
            let integer = match value {
                SqliteValue::Integer(integer) => Some(*integer),
                other => other.as_text().and_then(|text| text.trim().parse().ok()),
            }?;
            object.insert(name.to_owned(), Value::from(integer));
        } else if STRING_FIELDS.contains(&name) {
            let text = value.as_text()?;
            if !text.is_empty() || matches!(name, "id" | "title") {
                object.insert(name.to_owned(), Value::String(text.to_owned()));
            }
        }
    }
    let created_at = object.get("created_at").cloned();
    let dependencies: Vec<Dependency> = dependencies
        .iter()
        .map(|row| decode_related(row, created_at.as_ref()))
        .collect::<Option<_>>()?;
    let comments: Vec<Comment> = comments
        .iter()
        .map(|row| {
            let mut row = row.clone();
            if let Some(Value::String(text)) = row.remove("text").or_else(|| row.remove("body")) {
                row.insert("text".to_owned(), Value::String(text));
            }
            decode_related(&row, created_at.as_ref())
        })
        .collect::<Option<_>>()?;
    object.insert(
        "labels".to_owned(),
        Value::Array(labels.iter().cloned().map(Value::String).collect()),
    );
    let mut issue: Issue = serde_json::from_value(Value::Object(object)).ok()?;
    issue.dependencies.clone_from(&dependencies);
    issue.comments.clone_from(&comments);
    Some(PreservedIssue {
        labels: Some(labels),
        dependencies: Some(dependencies),
        comments: Some(comments),
        issue,
    })
}

/// Decode a dependency or comment row, normalizing its timestamp and
/// defaulting a missing one to the owning issue's `created_at`.
fn decode_related<T: serde::de::DeserializeOwned>(
    row: &Map<String, Value>,
    fallback_created_at: Option<&Value>,
) -> Option<T> {
    let mut row = row.clone();
    let created_at = match row.get("created_at") {
        None | Some(Value::Null) => fallback_created_at.cloned()?,
        Some(Value::String(text)) if text.trim().is_empty() => fallback_created_at.cloned()?,
        Some(value) => {
            let timestamp = match value {
                Value::String(text) => legacy_timestamp_text(text),
                Value::Number(number) => number.as_i64().and_then(legacy_epoch),
                _ => None,
            }?;
            Value::String(timestamp.to_rfc3339())
        }
    };
    row.insert("created_at".to_owned(), created_at);
    serde_json::from_value(Value::Object(row)).ok()
}

fn legacy_timestamp(value: &SqliteValue) -> Option<DateTime<Utc>> {
    match value {
        SqliteValue::Integer(integer) => legacy_epoch(*integer),
        other => other.as_text().and_then(legacy_timestamp_text),
    }
}

fn legacy_timestamp_text(text: &str) -> Option<DateTime<Utc>> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    if let Ok(parsed) = DateTime::parse_from_rfc3339(text) {
        return Some(parsed.with_timezone(&Utc));
    }
    for format in ["%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M:%S%.f"] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(text, format) {
            return Some(Utc.from_utc_datetime(&naive));
        }
    }
    // Go's time.Time String() form, as some bd builds stored it:
    // "2026-01-03 14:28:28.085091 -0500 EST".
    let mut parts = text.split_whitespace();
    if let (Some(date), Some(time), Some(offset)) = (parts.next(), parts.next(), parts.next())
        && let Ok(parsed) = DateTime::parse_from_str(
            &format!("{date} {time} {offset}"),
            "%Y-%m-%d %H:%M:%S%.f %z",
        )
    {
        return Some(parsed.with_timezone(&Utc));
    }
    text.parse::<i64>().ok().and_then(legacy_epoch)
}

/// Integer datetimes: seconds, milliseconds, microseconds, or nanoseconds
/// since the epoch, distinguished by magnitude.
fn legacy_epoch(value: i64) -> Option<DateTime<Utc>> {
    let magnitude = value.unsigned_abs();
    if magnitude < 100_000_000_000 {
        Utc.timestamp_opt(value, 0).single()
    } else if magnitude < 100_000_000_000_000 {
        Utc.timestamp_millis_opt(value).single()
    } else if magnitude < 100_000_000_000_000_000 {
        Some(DateTime::from_timestamp_micros(value)?.with_timezone(&Utc))
    } else {
        Some(DateTime::from_timestamp_nanos(value))
    }
}

fn query_user_version(conn: &Connection) -> Result<u32> {
    let row = conn.query_row("PRAGMA user_version")?;
    Ok(row
        .get(0)
        .and_then(SqliteValue::as_integer)
        .and_then(|value| u32::try_from(value).ok())
        .unwrap_or(0))
}

fn table_names(conn: &Connection) -> Result<HashSet<String>> {
    conn.query("SELECT name FROM sqlite_master WHERE type='table'")?
        .iter()
        .map(|row| required_legacy_text(row.get(0), "sqlite_master", "name"))
        .collect()
}

fn column_names(conn: &Connection, table: &str) -> Result<Vec<String>> {
    conn.query(&format!("PRAGMA table_xinfo({})", quote_identifier(table)))?
        .iter()
        .map(|row| required_legacy_text(row.get(1), table, "column name"))
        .collect()
}

fn quote_identifier(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

// ---------------------------------------------------------------------------
// Heal
// ---------------------------------------------------------------------------

/// Inputs shared by both heal paths.
pub struct HealContext<'a> {
    pub beads_dir: &'a Path,
    pub cli: &'a config::CliOverrides,
    pub write_authority: &'a Arc<DatabaseFamilyWriteLock>,
}

/// Result of a heal attempt.
pub enum HealResult {
    /// The database was not on an older schema.
    NotNeeded,
    /// The database is current now.
    Healed(HealOutcome),
    /// Automatic mode refused; the audit explains why.
    Refused(Box<StaleSchemaAudit>),
}

/// Upgrade a stale database through a reviewed lossless migration only.
///
/// The caller must hold the database-family write authority for the whole
/// call. A reviewed migration preserves raw rows without consulting JSONL.
/// An explicit invocation receives a detailed preservation audit when no
/// reviewed migration completes. No schema-heal path reconstructs a live
/// database from JSONL, including after a refused or interrupted migration.
///
/// # Errors
///
/// Returns an error when the audit or reviewed upgrade fails. The reviewed
/// migration retains its existing private-candidate, backup and undo protocol.
#[allow(clippy::too_many_lines)]
pub fn heal_stale_schema(ctx: &HealContext<'_>, mode: HealMode) -> Result<HealResult> {
    let startup = config::load_startup_config_with_paths(ctx.beads_dir, ctx.cli.db.as_ref())?;
    let paths = startup.paths.clone();
    ctx.write_authority.verify_database_authority()?;
    let Some(from_version) = stale_schema_version(&paths.db_path)? else {
        return Ok(HealResult::NotNeeded);
    };

    // GH #551: the strict legacy decoder proves JSONL rebuildability, not
    // migration safety. Empty labels, NULL relation owners, BLOBs and legacy
    // scalar encodings can be kept by the reviewed raw-row migration even
    // when they cannot be represented by the current Issue/JSONL model.
    let mut migration_refusal_reason = None;
    if REVIEWED_MIGRATION_SOURCE_VERSIONS.contains(&from_version) {
        let migration = super::schema_migration::MigrationContext {
            beads_dir: ctx.beads_dir.to_path_buf(),
            db_path: paths.db_path.clone(),
            write_authority: Arc::clone(ctx.write_authority),
        };
        match super::schema_migration::plan_and_apply(&migration) {
            Ok(backup) => {
                return Ok(HealResult::Healed(HealOutcome {
                    from_version,
                    to_version: current_version(),
                    action: "migrated",
                    backup: backup.display().to_string(),
                }));
            }
            // GH #585: a migration refusal must remain a refusal during
            // ordinary startup. JSONL cannot represent the complete database,
            // and a backup is not permission to remove authoritative rows
            // from the active tracker. Preserve the precise migration error.
            Err(error) if mode == HealMode::Automatic => {
                return Err(automatic_migration_refusal(from_version, Some(error)));
            }
            // An explicit heal can add an observational preservation audit
            // while the original schema remains; it never retries via JSONL.
            Err(error) if stale_schema_version(&paths.db_path)? == Some(from_version) => {
                migration_refusal_reason = Some(error.to_string());
            }
            Err(error) => return Err(error),
        }
    }

    if mode == HealMode::Automatic {
        return Err(automatic_migration_refusal(from_version, None));
    }
    let audited = (|| {
        let (_jsonl_authority, source) = capture_jsonl(&paths.jsonl_path, ctx.cli.lock_timeout)?;
        audit_stale_database(&paths.db_path, source.as_ref())
    })();
    let mut audit = match audited {
        Ok(audit) => audit,
        Err(error) => {
            let reason = migration_refusal_reason.map_or_else(
                || format!("no reviewed lossless migration exists for schema {from_version}"),
                |reason| format!("the reviewed migration did not complete ({reason})"),
            );
            return Err(BeadsError::WithContext {
                context: format!(
                    "schema upgrade refused: {reason}; the supplementary audit could not complete. \
                     The database was not rebuilt and the original database remains authoritative"
                ),
                source: Box::new(error),
            });
        }
    };
    audit.from_version = from_version;
    if let Some(reason) = migration_refusal_reason {
        return Err(BeadsError::WithContext {
            context: format!("the reviewed migration did not complete ({reason})"),
            source: Box::new(refusal_error(&audit)),
        });
    }
    Ok(HealResult::Refused(Box::new(audit)))
}

fn automatic_migration_refusal(from_version: u32, source: Option<BeadsError>) -> BeadsError {
    let why = if source.is_some() {
        "the reviewed lossless migration did not complete".to_owned()
    } else {
        format!("no reviewed lossless migration exists for schema {from_version}")
    };
    BeadsError::WithContext {
        context: format!(
            "automatic schema upgrade refused: {why}. The database will not be rebuilt from \
             issues.jsonl because that export cannot establish preservation of event history, \
             configuration, and other database-only state. Keep the original database; run \
             `br doctor migrate-schema plan --json` for the migration diagnosis or \
             `{HEAL_COMMAND} --dry-run --json` for a non-destructive rebuild audit. \
             Use a release supporting this database until a lossless migration is available"
        ),
        source: Box::new(source.unwrap_or(BeadsError::SchemaMismatch {
            expected: CURRENT_SCHEMA_VERSION,
            found: i32::try_from(from_version).unwrap_or(i32::MAX),
        })),
    }
}

fn capture_jsonl(
    jsonl_path: &Path,
    lock_timeout: Option<u64>,
) -> Result<(
    Option<Arc<crate::sync::JsonlFamilyWriteLock>>,
    Option<JsonlSourceSnapshot>,
)> {
    if !jsonl_path.is_file() {
        return Ok((None, None));
    }
    let authority = Arc::new(crate::sync::blocking_jsonl_family_write_lock_with_timeout(
        jsonl_path,
        lock_timeout,
    )?);
    authority.verify_jsonl_authority()?;
    let source = crate::sync::capture_jsonl_source_snapshot(jsonl_path)?;
    Ok((Some(authority), Some(source)))
}

/// Necessary payload coverage for a diagnostic plan, never sufficient
/// authorization to replace a database. Even an entirely decodable plan is
/// refused by `heal_stale_schema` without a reviewed migration installer.
fn require_replay_payload_coverage(audit: &StaleSchemaAudit, discard_db_only: bool) -> Result<()> {
    if let Some(reason) = &audit.unprovable {
        return Err(BeadsError::Config(format!(
            "cannot certify a schema-heal rebuild: {reason}; the rebuild was not started"
        )));
    }
    if !audit.rebuild_blockers.is_empty() {
        return Err(BeadsError::Config(format!(
            "schema-heal rebuild refused: the complete database cannot be preserved from \
             issues.jsonl ({}). The rebuild was not started; the original database remains \
             authoritative. --discard-db-only does not authorize discarding event history, \
             configuration, workflow records, or operator-owned schema. Use a release that \
             supports this database or a reviewed lossless migration",
            audit.rebuild_blockers.join("; ")
        )));
    }
    if !discard_db_only
        && audit
            .db_only
            .iter()
            .any(|issue| issue.reason == DbOnlyReason::Unreadable)
    {
        return Err(BeadsError::Config(format!(
            "schema-heal rebuild refused: unreadable database-only payloads cannot be \
             restored ({}). The rebuild was not started. Repair the original payloads first. \
             --discard-db-only does not authorize an unreviewed reconstruction. {}",
            audit.db_only_summary(),
            audit.restoration_notice()
        )));
    }
    if !discard_db_only && audit.has_orphan_relations() {
        return Err(BeadsError::Config(format!(
            "schema-heal rebuild refused: the database holds relation rows without an owning \
             issue ({}), which a rebuild from issues.jsonl cannot carry. The rebuild was not \
             started. --discard-db-only does not authorize an unreviewed reconstruction. {}",
            audit.orphan_summary(),
            audit.restoration_notice()
        )));
    }
    Ok(())
}

/// Planned action for a stale database, as `--dry-run` reports it.
fn planned_action(audit: &StaleSchemaAudit, discard_db_only: bool) -> String {
    if REVIEWED_MIGRATION_SOURCE_VERSIONS.contains(&audit.from_version) {
        return reviewed_migration_action(audit.from_version, audit.to_version);
    }
    if let Err(error) = require_replay_payload_coverage(audit, discard_db_only) {
        return format!("refuse rebuild: {error}");
    }
    format!("refuse rebuild: {}", refusal_error(audit))
}

fn reviewed_migration_action(from_version: u32, to_version: u32) -> String {
    format!(
        "reviewed in-place migration {from_version} -> {to_version} (subject to the reviewed \
         migration's schema, integrity and raw-row preservation checks; recovery bundle and \
         undo command retained)"
    )
}

/// Failure to encode old rows as JSONL does not make a raw-row migration
/// unavailable. A dry run must report that distinction without running one or
/// inventing an empty/clean audit (GH #551).
fn emit_migration_dry_run_without_audit(
    from_version: u32,
    error: &BeadsError,
    json: bool,
) -> Result<()> {
    let action = reviewed_migration_action(from_version, current_version());
    let note = "No migration was run. The JSONL rebuild audit failed; a reviewed migration \
                can still be attempted without it. JSONL reconstruction remains refused. \
                Use `br doctor migrate-schema plan` to inspect the \
                migration's own checks.";
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "dry_run": true,
                "audit": null,
                "audit_error": error.to_string(),
                "from_version": from_version,
                "to_version": current_version(),
                "automatic_heal_allowed": true,
                "automatic_rebuild_allowed": false,
                "planned_action": action,
                "note": note,
            }))?
        );
    } else {
        println!("JSONL rebuild audit failed: {error}");
        println!("Heal would attempt: {action}.");
        println!("{note}");
    }
    Ok(())
}

/// Human-readable `heal --dry-run` report.
fn print_dry_run_audit(audit: &StaleSchemaAudit, action: &str) {
    println!(
        "Database schema {} (this br uses {}); {} issue(s) in the database, {} in the JSONL.",
        audit.from_version, audit.to_version, audit.db_issue_count, audit.jsonl_issue_count
    );
    if let Some(reason) = &audit.unprovable {
        println!("Audit incomplete: {reason}.");
    } else if !audit.rebuild_blockers.is_empty() {
        println!("Reconstruction cannot preserve the complete database:");
        for reason in &audit.rebuild_blockers {
            println!("  {reason}");
        }
    } else if audit.db_only.is_empty() {
        if audit.has_orphan_relations() {
            println!(
                "Relation rows without an owning issue: {}.",
                audit.orphan_summary()
            );
        }
        if audit.automatic_heal_allowed() {
            println!(
                "A mutating command may attempt a reviewed lossless migration. \
                 Read commands never upgrade the database."
            );
        } else {
            println!(
                "No reviewed lossless migration exists for this schema. \
                 Ordinary commands never reconstruct it from JSONL."
            );
        }
    } else {
        println!(
            "{} issue(s) exist only in the database:",
            audit.db_only.len()
        );
        for issue in &audit.db_only {
            println!("  {} ({})", issue.id, issue.reason.describe());
        }
        if audit.has_orphan_relations() {
            println!(
                "Relation rows without an owning issue: {}.",
                audit.orphan_summary()
            );
        }
    }
    println!("Heal would: {action}.");
    if REVIEWED_MIGRATION_SOURCE_VERSIONS.contains(&audit.from_version) {
        println!(
            "The reviewed migration is attempted before this JSONL audit. If it fails, \
             schema healing refuses and preserves the original database. An explicit heal \
             can diagnose missing state but never bypasses that refusal via JSONL."
        );
    }
}

/// Execute `br doctor migrate-schema heal`.
///
/// # Errors
///
/// Returns an error when the audit or the upgrade fails.
pub fn execute_heal(
    args: &crate::cli::DoctorMigrateSchemaHealArgs,
    cli: &config::CliOverrides,
    beads_dir: &Path,
    write_authority: &Arc<DatabaseFamilyWriteLock>,
) -> Result<()> {
    if args.dry_run {
        let paths = config::resolve_paths(beads_dir, cli.db.as_ref())?;
        return execute_heal_preview(args, &paths);
    }

    let ctx = HealContext {
        beads_dir,
        cli,
        write_authority,
    };
    match heal_stale_schema(
        &ctx,
        HealMode::Explicit {
            discard_db_only: args.discard_db_only,
        },
    )? {
        HealResult::NotNeeded => emit_not_needed(args.json),
        HealResult::Refused(audit) => Err(refusal_error(&audit)),
        HealResult::Healed(outcome) => {
            if args.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&outcome).map_err(BeadsError::Json)?
                );
            } else {
                println!("{}", outcome.notice().trim_start_matches("br: "));
            }
            Ok(())
        }
    }
}

/// Observe a private SQLite snapshot and immutable JSONL bytes without taking
/// either source write authority. Only an actual heal may touch live opener
/// registration, WAL reader marks, or migration/recovery artifacts.
pub(super) fn execute_heal_preview(
    args: &crate::cli::DoctorMigrateSchemaHealArgs,
    paths: &config::ConfigPaths,
) -> Result<()> {
    match std::fs::symlink_metadata(&paths.db_path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return emit_not_needed(args.json);
        }
        Err(error) => return Err(BeadsError::Io(error)),
        Ok(_) => {}
    }
    config::with_database_family_snapshot(&paths.db_path, |snapshot| {
        let Some(from_version) = stale_schema_version(snapshot)? else {
            return emit_not_needed(args.json);
        };
        let audited = (|| {
            let source = match std::fs::symlink_metadata(&paths.jsonl_path) {
                Ok(_) => Some(crate::sync::capture_jsonl_source_snapshot(
                    &paths.jsonl_path,
                )?),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => return Err(BeadsError::Io(error)),
            };
            audit_stale_database(snapshot, source.as_ref())
        })();
        let mut audit = match audited {
            Ok(audit) => audit,
            Err(error) if REVIEWED_MIGRATION_SOURCE_VERSIONS.contains(&from_version) => {
                return emit_migration_dry_run_without_audit(from_version, &error, args.json);
            }
            Err(error) => return Err(error),
        };
        audit.from_version = from_version;
        let action = planned_action(&audit, args.discard_db_only);
        if args.json {
            let payload = serde_json::json!({
                "dry_run": true,
                "audit": audit,
                "automatic_heal_allowed": audit.automatic_heal_allowed(),
                "automatic_rebuild_allowed": false,
                "planned_action": action,
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&payload).map_err(BeadsError::Json)?
            );
        } else {
            print_dry_run_audit(&audit, &action);
        }
        Ok(())
    })
}

fn emit_not_needed(json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "heal_needed": false,
                "schema_version": CURRENT_SCHEMA_VERSION,
            }))
            .map_err(BeadsError::Json)?
        );
    } else {
        println!("Database is not on an older schema; nothing to heal.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{IssueType, Priority};

    #[test]
    fn reviewed_migration_eligibility_never_authorizes_an_unsafe_rebuild() {
        let mut audit = StaleSchemaAudit {
            from_version: REVIEWED_MIGRATION_SOURCE_VERSIONS[0],
            to_version: current_version(),
            db_issue_count: 1,
            jsonl_issue_count: 1,
            db_only: Vec::new(),
            orphan_relations: Vec::new(),
            rebuild_blockers: Vec::new(),
            unprovable: None,
            restorable: Vec::new(),
        };
        assert!(audit.automatic_heal_allowed());
        assert!(
            automatic_migration_refusal(audit.from_version, None)
                .to_string()
                .contains("will not be rebuilt from issues.jsonl")
        );

        audit.db_only.push(DbOnlyIssue {
            id: "legacy-551".to_owned(),
            reason: DbOnlyReason::Unreadable,
            restorable: false,
        });
        assert!(audit.automatic_heal_allowed());
        assert!(require_replay_payload_coverage(&audit, false).is_err());

        audit.db_only.clear();
        audit.orphan_relations.push(OrphanRelationRows {
            table: "comments",
            rows: 1,
        });
        assert!(audit.automatic_heal_allowed());
        assert!(require_replay_payload_coverage(&audit, false).is_err());

        audit.orphan_relations.clear();
        audit.unprovable = Some("JSONL snapshot cannot be decoded".to_owned());
        assert!(audit.automatic_heal_allowed());
        // Even --discard-db-only cannot invent a trustworthy rebuild source.
        assert!(require_replay_payload_coverage(&audit, true).is_err());
        audit.from_version = 4;
        assert!(!audit.automatic_heal_allowed());
    }

    fn issue(id: &str, updated: &str) -> Issue {
        let at = DateTime::parse_from_rfc3339(updated)
            .unwrap()
            .with_timezone(&Utc);
        Issue {
            id: id.to_owned(),
            title: format!("title {id}"),
            status: Status::Open,
            priority: Priority::MEDIUM,
            issue_type: IssueType::Task,
            created_at: at,
            updated_at: at,
            ..Issue::default()
        }
    }

    #[test]
    fn equivalent_rows_are_represented_even_when_dirty() {
        let db = issue("a-1", "2026-01-01T00:00:00Z");
        let jsonl = db.clone();
        assert_eq!(classify(&db, Some(&jsonl), true), None);
    }

    #[test]
    fn absent_rows_are_db_only_and_restorable() {
        let db = issue("a-1", "2026-01-01T00:00:00Z");
        assert_eq!(
            classify(&db, None, false),
            Some((DbOnlyReason::AbsentFromJsonl, true))
        );
    }

    #[test]
    fn clean_row_superseded_by_newer_jsonl_is_represented() {
        let db = issue("a-1", "2026-01-01T00:00:00Z");
        let mut jsonl = issue("a-1", "2026-02-01T00:00:00Z");
        jsonl.title = "renamed upstream".to_owned();
        assert_eq!(classify(&db, Some(&jsonl), false), None);
    }

    #[test]
    fn clean_row_newer_than_jsonl_is_db_only() {
        let mut db = issue("a-1", "2026-03-01T00:00:00Z");
        db.title = "local rename".to_owned();
        let jsonl = issue("a-1", "2026-02-01T00:00:00Z");
        assert_eq!(
            classify(&db, Some(&jsonl), false),
            Some((DbOnlyReason::NewerThanJsonl, true))
        );
    }

    #[test]
    fn dirty_edit_is_db_only_but_restored_only_when_newer() {
        let mut older = issue("a-1", "2026-01-01T00:00:00Z");
        older.title = "unflushed".to_owned();
        let jsonl = issue("a-1", "2026-02-01T00:00:00Z");
        assert_eq!(
            classify(&older, Some(&jsonl), true),
            Some((DbOnlyReason::UnflushedEdit, false))
        );
        let mut newer = issue("a-1", "2026-03-01T00:00:00Z");
        newer.title = "unflushed".to_owned();
        assert_eq!(
            classify(&newer, Some(&jsonl), true),
            Some((DbOnlyReason::UnflushedEdit, true))
        );
    }

    #[test]
    fn flushed_tombstone_wins() {
        let db = issue("a-1", "2026-03-01T00:00:00Z");
        let mut jsonl = issue("a-1", "2026-01-01T00:00:00Z");
        jsonl.status = Status::Tombstone;
        assert_eq!(classify(&db, Some(&jsonl), true), None);
    }

    #[test]
    fn label_missing_from_jsonl_breaks_equivalence() {
        let mut db = issue("a-1", "2026-01-01T00:00:00Z");
        db.labels = vec!["local".to_owned()];
        let jsonl = issue("a-1", "2026-01-01T00:00:00Z");
        assert!(!issues_equivalent(&db, &jsonl));
        assert!(issues_equivalent(&jsonl, &db), "JSONL superset is fine");
    }

    #[test]
    fn legacy_timestamps_decode() {
        let expected = DateTime::parse_from_rfc3339("2026-01-03T19:28:28Z")
            .unwrap()
            .with_timezone(&Utc);
        for text in [
            "2026-01-03T14:28:28-05:00",
            "2026-01-03 19:28:28",
            "2026-01-03T19:28:28",
            "2026-01-03 14:28:28 -0500 EST",
            "1767468508",
        ] {
            assert_eq!(legacy_timestamp_text(text), Some(expected), "{text}");
        }
        assert_eq!(legacy_epoch(1_767_468_508_000), Some(expected));
        assert_eq!(legacy_timestamp_text("not a date"), None);
    }

    fn legacy_audit_fixture() -> (Connection, Vec<u8>) {
        let conn = Connection::open(":memory:").unwrap();
        conn.execute(
            "CREATE TABLE issues (id TEXT, title TEXT, status TEXT, priority INTEGER, \
             issue_type TEXT, created_at TEXT, updated_at TEXT)",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO issues VALUES ('a-1', 'title a-1', 'open', 2, 'task', \
             '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
        )
        .unwrap();
        let jsonl = serde_json::to_vec(&issue("a-1", "2026-01-01T00:00:00Z")).unwrap();
        (conn, jsonl)
    }

    #[test]
    fn issue_equivalence_never_certifies_database_only_tables_for_rebuild() {
        for table in [
            "events",
            "config",
            "metadata",
            "child_counters",
            "close_metadata",
            "gate_results",
            "gate_result_history",
            "capacity_exemptions",
            "capacity_exemption_history",
            "capacity_occupancy",
            "operator_notes",
        ] {
            let (conn, jsonl) = legacy_audit_fixture();
            conn.execute(&format!("CREATE TABLE {table} (payload BLOB)"))
                .unwrap();
            conn.execute(&format!("INSERT INTO {table} VALUES (X'FEED')"))
                .unwrap();
            let before = conn.query(&format!("SELECT * FROM {table}")).unwrap();
            let audit = audit_connection(&conn, Some(std::io::Cursor::new(jsonl))).unwrap();
            assert!(audit.db_only.is_empty(), "issue matches JSONL: {table}");
            assert!(
                !audit.is_clean(),
                "auxiliary data is authoritative: {table}"
            );
            for discard in [false, true] {
                let refusal = require_replay_payload_coverage(&audit, discard).unwrap_err();
                let message = refusal.to_string();
                assert!(message.contains(&format!("table {table}")), "{message}");
                assert!(message.contains("rebuild was not started"), "{message}");
                assert!(message.contains("--discard-db-only does not authorize"));
            }
            assert_eq!(
                conn.query(&format!("SELECT * FROM {table}")).unwrap(),
                before
            );
        }
    }

    #[test]
    fn unknown_schema_and_sequence_reservations_are_not_disposable() {
        for (setup, name) in [
            (
                "CREATE TABLE operator_notes (payload TEXT)",
                "operator_notes",
            ),
            (
                "ALTER TABLE issues ADD COLUMN operator_payload TEXT",
                "issues.operator_payload",
            ),
            (
                "CREATE UNIQUE INDEX operator_unique_title ON issues(title)",
                "operator_unique_title",
            ),
            (
                "CREATE VIEW operator_tasks AS SELECT id FROM issues",
                "operator_tasks",
            ),
        ] {
            let (conn, jsonl) = legacy_audit_fixture();
            conn.execute(setup).unwrap();
            let audit = audit_connection(&conn, Some(std::io::Cursor::new(jsonl))).unwrap();
            let error = require_replay_payload_coverage(&audit, true).unwrap_err();
            assert!(error.to_string().contains(name), "{name}: {error}");
        }

        let (conn, jsonl) = legacy_audit_fixture();
        for sql in [
            "CREATE TABLE events (id INTEGER PRIMARY KEY AUTOINCREMENT, payload TEXT)",
            "INSERT INTO events (id, payload) VALUES (5000, 'reserved identity')",
            "DELETE FROM events",
        ] {
            conn.execute(sql).unwrap();
        }
        let audit = audit_connection(&conn, Some(std::io::Cursor::new(jsonl))).unwrap();
        assert!(
            require_replay_payload_coverage(&audit, true)
                .unwrap_err()
                .to_string()
                .contains("sqlite_sequence")
        );
    }

    #[test]
    fn preservation_audit_includes_generated_columns_hidden_from_table_info() {
        let (conn, jsonl) = legacy_audit_fixture();
        conn.execute(
            "ALTER TABLE issues ADD COLUMN operator_note TEXT \
             GENERATED ALWAYS AS (lower(title)) VIRTUAL",
        )
        .unwrap();
        assert!(
            conn.query("PRAGMA table_info(issues)")
                .unwrap()
                .iter()
                .all(|row| row.get(1).and_then(SqliteValue::as_text) != Some("operator_note"))
        );
        let audit = audit_connection(&conn, Some(std::io::Cursor::new(jsonl))).unwrap();
        assert!(!audit.is_clean());
        assert!(
            audit
                .rebuild_blockers
                .iter()
                .any(|blocker| blocker.contains("issues.operator_note")),
            "generated fields must remain visible in preservation diagnostics"
        );
    }

    #[test]
    fn newer_jsonl_cannot_hide_unreplayed_local_relations_or_provenance() {
        let (conn, _) = legacy_audit_fixture();
        for sql in [
            "CREATE TABLE labels (issue_id TEXT, label TEXT)",
            "INSERT INTO labels VALUES ('a-1', 'local-only')",
            "CREATE TABLE dependencies (issue_id TEXT, depends_on_id TEXT, type TEXT, created_at TEXT)",
            "INSERT INTO dependencies VALUES ('a-1', 'a-2', 'blocks', '2026-01-01T00:00:00Z')",
            "CREATE TABLE dirty_issues (issue_id TEXT)",
            "ALTER TABLE issues ADD COLUMN source_repo_path TEXT",
            "UPDATE issues SET source_repo_path = '/local/project'",
        ] {
            conn.execute(sql).unwrap();
        }
        let mut newer = issue("a-1", "2026-01-01T00:00:00Z");
        newer.updated_at += chrono::Duration::hours(1);
        let jsonl = serde_json::to_vec(&newer).unwrap();
        let audit = audit_connection(&conn, Some(std::io::Cursor::new(jsonl))).unwrap();
        assert!(
            audit.db_only.is_empty(),
            "legacy scalar precedence alone permits this"
        );
        let error = require_replay_payload_coverage(&audit, true)
            .unwrap_err()
            .to_string();
        for required in ["local labels", "local dependencies", "source_repo_path"] {
            assert!(error.contains(required), "{required}: {error}");
        }
    }

    #[test]
    fn automatic_refusal_retains_the_reviewed_migration_diagnosis() {
        let error = automatic_migration_refusal(
            16,
            Some(BeadsError::Config(
                "schema migrate v19 refused: dependencies has unreviewed indexes".to_owned(),
            )),
        );
        let message = error.to_string();
        assert!(message.contains("dependencies has unreviewed indexes"));
        assert!(message.contains("will not be rebuilt from issues.jsonl"));
        assert!(message.contains("migrate-schema plan --json"));
        assert!(
            automatic_migration_refusal(0, None)
                .to_string()
                .contains("no reviewed lossless migration exists for schema 0")
        );
    }

    #[test]
    fn malformed_relations_cannot_make_a_schema_audit_appear_clean() {
        for setup in [
            [
                "CREATE TABLE comments (id INTEGER, issue_id TEXT, author TEXT, text TEXT)",
                "INSERT INTO comments VALUES (1, 'a-1', 'agent', NULL)",
            ],
            [
                "CREATE TABLE dependencies (issue_id TEXT, depends_on_id TEXT, type TEXT)",
                "INSERT INTO dependencies VALUES ('a-1', NULL, 'blocks')",
            ],
        ] {
            let (conn, jsonl) = legacy_audit_fixture();
            for sql in setup {
                conn.execute(sql).unwrap();
            }
            // The old filter_map decoder discarded the only child and
            // incorrectly certified this exact issue as already in JSONL.
            let audit = audit_connection(&conn, Some(std::io::Cursor::new(jsonl))).unwrap();
            assert!(!audit.is_clean());
            assert_eq!(audit.db_issue_count, 1);
            assert_eq!(audit.db_only.len(), 1);
            assert_eq!(audit.db_only[0].id, "a-1");
            assert_eq!(audit.db_only[0].reason, DbOnlyReason::Unreadable);
            assert!(!audit.db_only[0].restorable);
            assert!(audit.restorable.is_empty());
            assert!(require_replay_payload_coverage(&audit, false).is_err());
            assert!(require_replay_payload_coverage(&audit, true).is_ok());
            assert!(planned_action(&audit, false).starts_with("refuse rebuild:"));
            assert!(planned_action(&audit, true).starts_with("refuse rebuild:"));
        }
    }

    #[test]
    fn schema_audit_refuses_unattributable_or_unrepresentable_records() {
        let cases: &[(&[&str], &str)] = &[
            (&["UPDATE issues SET id = NULL"], "issues.id"),
            (
                &["INSERT INTO issues SELECT * FROM issues"],
                "duplicate legacy issue ids",
            ),
            (
                &[
                    "CREATE TABLE labels (issue_id TEXT, label TEXT)",
                    "INSERT INTO labels VALUES (NULL, 'retained')",
                ],
                "labels.issue_id",
            ),
            (
                &[
                    "CREATE TABLE labels (issue_id TEXT, label BLOB)",
                    "INSERT INTO labels VALUES ('a-1', X'FEED')",
                ],
                "labels.label",
            ),
            (
                &[
                    "CREATE TABLE comments (issue_id TEXT, text TEXT)",
                    "INSERT INTO comments VALUES (NULL, 'retained')",
                ],
                "comments.issue_id",
            ),
            (
                &[
                    "CREATE TABLE dependencies (issue_id TEXT, metadata BLOB)",
                    "INSERT INTO dependencies VALUES ('a-1', X'FEED')",
                ],
                "dependencies.metadata",
            ),
            (
                &[
                    "CREATE TABLE dirty_issues (issue_id TEXT)",
                    "INSERT INTO dirty_issues VALUES (NULL)",
                ],
                "dirty_issues.issue_id",
            ),
        ];
        for &(setup, expected) in cases {
            let (conn, jsonl) = legacy_audit_fixture();
            for sql in setup {
                conn.execute(sql).unwrap();
            }
            let error = audit_connection(&conn, Some(std::io::Cursor::new(jsonl))).unwrap_err();
            assert!(error.to_string().contains(expected), "{expected}: {error}");
        }
    }

    /// Well-formed relation rows whose issue is gone (deletes that older releases
    /// ran without foreign-key enforcement) used to fail the whole audit, so every
    /// mutating command and even `heal --discard-db-only` refused such a database.
    /// They are reported instead: the reviewed in-place migration keeps them, and
    /// only a JSONL rebuild, which cannot carry them, needs `--discard-db-only`.
    #[test]
    fn orphan_relation_rows_are_reported_and_only_a_rebuild_needs_discard() {
        let (conn, jsonl) = legacy_audit_fixture();
        for sql in [
            "CREATE TABLE comments (issue_id TEXT, text TEXT)",
            "INSERT INTO comments VALUES ('missing-owner', 'retained')",
            "INSERT INTO comments VALUES ('missing-owner', 'second')",
            "CREATE TABLE labels (issue_id TEXT, label TEXT)",
            "INSERT INTO labels VALUES ('missing-owner', 'retained')",
        ] {
            conn.execute(sql).unwrap();
        }
        let mut audit = audit_connection(&conn, Some(std::io::Cursor::new(jsonl))).unwrap();
        assert!(
            !audit.is_clean(),
            "orphan relations are still authoritative: {audit:?}"
        );
        let orphans: Vec<(&str, usize)> = audit
            .orphan_relations
            .iter()
            .map(|orphans| (orphans.table, orphans.rows))
            .collect();
        assert_eq!(orphans, [("labels", 1), ("comments", 2)]);

        audit.from_version = REVIEWED_MIGRATION_SOURCE_VERSIONS[0];
        assert!(audit.automatic_heal_allowed(), "the migration keeps them");
        audit.from_version = 4;
        assert!(!audit.automatic_heal_allowed(), "a rebuild would drop them");
        assert!(
            refusal_error(&audit)
                .to_string()
                .contains("--discard-db-only")
        );

        let refused = require_replay_payload_coverage(&audit, false).unwrap_err();
        assert!(
            refused.to_string().contains("without an owning issue"),
            "{refused}"
        );
        assert!(planned_action(&audit, false).starts_with("refuse rebuild"));
        require_replay_payload_coverage(&audit, true).expect("orphan discard covers only payloads");
        assert!(
            planned_action(&audit, true).starts_with("refuse rebuild:"),
            "{}",
            planned_action(&audit, true)
        );
    }

    #[test]
    fn populated_invalid_optional_values_are_not_silently_defaulted() {
        for (column, value) in [
            ("due_at", SqliteValue::from("not a date")),
            ("pinned", SqliteValue::from("not a boolean")),
            ("estimated_minutes", SqliteValue::from("not an integer")),
            ("description", SqliteValue::Integer(42)),
        ] {
            let columns: Vec<String> = ["id", "title", "created_at", "updated_at", column]
                .into_iter()
                .map(str::to_owned)
                .collect();
            let values = [
                SqliteValue::from("a-1"),
                SqliteValue::from("title a-1"),
                SqliteValue::from("2026-01-01T00:00:00Z"),
                SqliteValue::from("2026-01-01T00:00:00Z"),
                value,
            ];
            assert!(
                decode_issue(&columns, &values, Vec::new(), &[], &[]).is_none(),
                "{column}"
            );
        }
    }

    #[test]
    fn related_timestamp_fallback_requires_absence_not_a_malformed_value() {
        let fallback = serde_json::json!("2026-01-01T00:00:00Z");
        let mut row = serde_json::json!({
            "id": 1, "issue_id": "a-1", "author": "agent", "text": "retained",
        })
        .as_object()
        .unwrap()
        .clone();
        assert!(decode_related::<Comment>(&row, Some(&fallback)).is_some());
        for absent in [Value::Null, serde_json::json!("")] {
            row.insert("created_at".to_string(), absent);
            assert!(decode_related::<Comment>(&row, Some(&fallback)).is_some());
        }
        for malformed in [
            serde_json::json!("not a date"),
            serde_json::json!({"bad": true}),
        ] {
            row.insert("created_at".to_string(), malformed);
            assert!(decode_related::<Comment>(&row, Some(&fallback)).is_none());
        }
    }

    #[test]
    fn valid_legacy_children_are_preserved_in_the_restore_plan() {
        let (conn, _) = legacy_audit_fixture();
        for sql in [
            "CREATE TABLE labels (issue_id TEXT, label TEXT)",
            "INSERT INTO labels VALUES ('a-1', 'local-only')",
            "CREATE TABLE comments (id INTEGER, issue_id TEXT, author TEXT, body TEXT)",
            "INSERT INTO comments VALUES (1, 'a-1', 'agent', 'unexported comment')",
            "CREATE TABLE dependencies (issue_id TEXT, depends_on_id TEXT, type TEXT, created_by TEXT)",
            "INSERT INTO dependencies VALUES ('a-1', 'external-target', 'blocks', 'agent')",
            "ALTER TABLE issues ADD COLUMN due_at TEXT",
            "UPDATE issues SET due_at = ''",
        ] {
            conn.execute(sql).unwrap();
        }
        let older = serde_json::to_vec(&issue("a-1", "2025-12-01T00:00:00Z")).unwrap();
        let audit = audit_connection(&conn, Some(std::io::Cursor::new(older))).unwrap();
        assert!(!audit.is_clean());
        assert_eq!(audit.restorable.len(), 1);
        let restored = &audit.restorable[0].issue;
        assert_eq!(restored.labels, vec!["local-only".to_string()]);
        assert_eq!(restored.comments.len(), 1);
        assert_eq!(restored.comments[0].body, "unexported comment");
        assert_eq!(restored.comments[0].created_at, restored.created_at);
        assert_eq!(restored.dependencies.len(), 1);
        assert_eq!(restored.dependencies[0].depends_on_id, "external-target");
        assert_eq!(restored.dependencies[0].created_at, restored.created_at);
        assert!(restored.due_at.is_none());
        assert!(require_replay_payload_coverage(&audit, false).is_ok());
    }

    #[test]
    fn duplicate_or_unprovable_jsonl_never_authorizes_a_rebuild() {
        let (conn, jsonl) = legacy_audit_fixture();
        let mut duplicate = jsonl.clone();
        duplicate.push(b'\n');
        duplicate.extend_from_slice(&jsonl);
        let audit = audit_connection(&conn, Some(std::io::Cursor::new(duplicate))).unwrap();
        assert!(!audit.is_clean());
        assert!(
            audit
                .unprovable
                .as_deref()
                .unwrap()
                .contains("duplicate issue id")
        );
        for discard in [false, true] {
            assert!(require_replay_payload_coverage(&audit, discard).is_err());
            assert!(planned_action(&audit, discard).starts_with("refuse rebuild:"));
        }
    }

    #[test]
    fn discard_plan_separates_recoverable_issues_from_unreadable_rows_and_orphans() {
        let (conn, _) = legacy_audit_fixture();
        for sql in [
            "INSERT INTO issues VALUES ('a-2', 'unreadable', 'open', 2, 'task', \
             '2026-01-01T00:00:00Z', 'not a timestamp')",
            "CREATE TABLE labels (issue_id TEXT, label TEXT)",
            "INSERT INTO labels VALUES ('missing-owner', 'orphan')",
        ] {
            conn.execute(sql).unwrap();
        }
        let audit = audit_connection(&conn, Some(std::io::Cursor::new(Vec::<u8>::new())))
            .expect("auditable identities with one unreadable payload");
        assert_eq!(audit.db_only.len(), 2);
        assert_eq!(audit.restorable.len(), 1);
        assert_eq!(audit.restorable[0].issue.id, "a-1");
        assert!(!audit.is_clean());
        let refusal = require_replay_payload_coverage(&audit, false).unwrap_err();
        assert!(refusal.to_string().contains("unreadable"));
        assert!(
            refusal
                .to_string()
                .contains("1 recoverable database-only issue(s) (a-1)")
        );
        assert!(
            refusal_error(&audit)
                .to_string()
                .contains("--discard-db-only")
        );
        require_replay_payload_coverage(&audit, true)
            .expect("payload classification is independent of installer safety");
        let action = planned_action(&audit, true);
        assert!(action.starts_with("refuse rebuild:"), "{action}");
        assert!(action.contains("unreadable row"), "{action}");
        assert!(action.contains("1 labels row(s)"), "{action}");
    }

    #[test]
    fn decodable_issue_plan_still_requires_a_reviewed_migration() {
        let (conn, _) = legacy_audit_fixture();
        let audit = audit_connection(&conn, Some(std::io::Cursor::new(Vec::<u8>::new())))
            .expect("recoverable database-only issue");
        require_replay_payload_coverage(&audit, false).unwrap();
        require_replay_payload_coverage(&audit, true).unwrap();
        assert_eq!(planned_action(&audit, false), planned_action(&audit, true));
        assert!(planned_action(&audit, true).starts_with("refuse rebuild:"));
        assert!(planned_action(&audit, true).contains("no reviewed crash-safe installer"));
    }

    #[test]
    fn completed_heal_reports_a_reviewed_migration() {
        let outcome = HealOutcome {
            from_version: 16,
            to_version: current_version(),
            action: "migrated",
            backup: "retained-backup".to_owned(),
        };
        let notice = outcome.notice();
        assert!(notice.contains("migrated it in place"));
        assert!(notice.contains("retained-backup"));
        assert!(!notice.contains("rebuilt"));
    }

    #[test]
    fn legacy_governance_and_local_provenance_survive_decoding() {
        let (conn, _) = legacy_audit_fixture();
        for sql in [
            "ALTER TABLE issues ADD COLUMN agent_context TEXT",
            "ALTER TABLE issues ADD COLUMN source_repo_path TEXT",
            "ALTER TABLE issues ADD COLUMN prerequisites TEXT",
            "UPDATE issues SET agent_context = '{\"constraints\":[\"keep local work\"]}', \
             source_repo_path = '/original/workspace', prerequisites = '- [ ] review'",
        ] {
            conn.execute(sql).unwrap();
        }
        let audit = audit_connection(&conn, Some(std::io::Cursor::new(Vec::<u8>::new())))
            .expect("decode a database-only issue");
        assert_eq!(audit.restorable.len(), 1);
        let restored = &audit.restorable[0].issue;
        assert_eq!(
            restored.agent_context.as_deref(),
            Some("{\"constraints\":[\"keep local work\"]}")
        );
        assert_eq!(
            restored.source_repo_path.as_deref(),
            Some("/original/workspace")
        );
        assert_eq!(restored.prerequisites.as_deref(), Some("- [ ] review"));
    }

    #[test]
    fn governance_only_dirty_difference_is_not_certified_as_equivalent() {
        let (conn, jsonl) = legacy_audit_fixture();
        for sql in [
            "ALTER TABLE issues ADD COLUMN agent_context TEXT",
            "UPDATE issues SET agent_context = '{\"constraints\":[\"local\"]}'",
            "CREATE TABLE dirty_issues (issue_id TEXT)",
            "INSERT INTO dirty_issues VALUES ('a-1')",
        ] {
            conn.execute(sql).unwrap();
        }
        let audit = audit_connection(&conn, Some(std::io::Cursor::new(jsonl))).unwrap();
        assert!(!audit.is_clean());
        assert!(!audit.automatic_heal_allowed());
        assert_eq!(audit.db_only[0].reason, DbOnlyReason::UnflushedEdit);
    }

    #[test]
    fn nontext_governance_cannot_masquerade_as_an_absent_field() {
        for column in ["agent_context", "source_repo_path"] {
            let (conn, jsonl) = legacy_audit_fixture();
            conn.execute(&format!("ALTER TABLE issues ADD COLUMN {column} BLOB"))
                .unwrap();
            conn.execute(&format!("UPDATE issues SET {column} = X'FEED'"))
                .unwrap();
            let audit = audit_connection(&conn, Some(std::io::Cursor::new(jsonl))).unwrap();
            assert_eq!(
                audit.db_only[0].reason,
                DbOnlyReason::Unreadable,
                "{column}"
            );
            assert!(audit.restorable.is_empty());
            assert!(require_replay_payload_coverage(&audit, false).is_err());
        }
    }

    #[test]
    fn synced_scalar_fields_do_not_disappear_from_equivalence() {
        let baseline = issue("a-1", "2026-01-01T00:00:00Z");
        for (field, changed) in [
            ("prerequisites", serde_json::json!("- [ ] review")),
            (
                "agent_context",
                serde_json::json!("{\"instructions\":[\"keep\"]}"),
            ),
            ("created_by", serde_json::json!("another agent")),
            ("closed_by_session", serde_json::json!("session-1")),
            ("source_system", serde_json::json!("upstream")),
            ("source_repo", serde_json::json!("repo")),
            ("deleted_by", serde_json::json!("agent")),
            ("delete_reason", serde_json::json!("retention decision")),
            ("original_type", serde_json::json!("feature")),
            ("compaction_level", serde_json::json!(2)),
            ("compacted_at_commit", serde_json::json!("abc123")),
            ("original_size", serde_json::json!(42)),
            ("sender", serde_json::json!("agent")),
            ("ephemeral", serde_json::json!(true)),
            ("pinned", serde_json::json!(true)),
            ("is_template", serde_json::json!(true)),
            ("created_at", serde_json::json!("2025-12-01T00:00:00Z")),
            ("deleted_at", serde_json::json!("2025-12-01T00:00:00Z")),
            ("compacted_at", serde_json::json!("2025-12-01T00:00:00Z")),
        ] {
            let mut value = serde_json::to_value(&baseline).unwrap();
            value[field] = changed;
            let local: Issue = serde_json::from_value(value).unwrap();
            assert!(!issues_equivalent(&local, &baseline), "lost field: {field}");
            assert_eq!(
                classify(&local, Some(&baseline), true),
                Some((DbOnlyReason::UnflushedEdit, false)),
                "timestamp precedence must remain unchanged for {field}"
            );
        }
        let mut persisted = baseline.clone();
        persisted.source_repo = Some(".".to_owned());
        persisted.compaction_level = Some(0);
        persisted.source_repo_path = Some("/local/path".to_owned());
        assert!(issues_equivalent(&persisted, &baseline));
    }

    #[test]
    fn comment_equivalence_uses_portable_identity_and_preserves_multiplicity() {
        let mut local = issue("a-1", "2026-01-01T00:00:00Z");
        local.comments.push(Comment {
            id: 1,
            issue_id: local.id.clone(),
            author: "agent".to_owned(),
            body: "same words at a different time".to_owned(),
            created_at: local.created_at,
        });
        let mut exported = local.clone();
        exported.comments[0].id = 99;
        assert!(issues_equivalent(&local, &exported), "rowids are local");
        exported.comments[0].created_at += chrono::Duration::seconds(1);
        assert!(!issues_equivalent(&local, &exported));
        exported.comments.push(local.comments[0].clone());
        assert!(issues_equivalent(&local, &exported), "a superset is safe");
        local.comments.push(local.comments[0].clone());
        assert!(
            !issues_equivalent(&local, &exported),
            "one match is not two"
        );
        exported.comments.push(local.comments[0].clone());
        assert!(issues_equivalent(&local, &exported));
    }

    #[test]
    fn dependency_equivalence_requires_the_complete_payload() {
        let mut local = issue("a-1", "2026-01-01T00:00:00Z");
        let dependency: Dependency = serde_json::from_value(serde_json::json!({
            "issue_id": "a-1", "depends_on_id": "a-2", "type": "blocks",
            "created_at": "2026-01-01T00:00:00Z"
        }))
        .unwrap();
        local.dependencies.push(dependency.clone());
        let mut exported = local.clone();
        exported.dependencies[0].metadata = Some("{}".to_owned());
        exported.dependencies[0].created_by = Some(String::new());
        exported.dependencies[0].thread_id = Some(String::new());
        assert!(issues_equivalent(&local, &exported), "persisted defaults");
        for (field, changed) in [
            ("metadata", serde_json::json!("{\"reason\":\"local\"}")),
            ("thread_id", serde_json::json!("thread-1")),
            ("created_by", serde_json::json!("another-agent")),
            ("created_at", serde_json::json!("2026-01-01T00:00:01Z")),
            ("issue_id", serde_json::json!("a-3")),
        ] {
            let mut value = serde_json::to_value(&dependency).unwrap();
            value[field] = changed;
            exported.dependencies = vec![serde_json::from_value(value).unwrap()];
            assert!(!issues_equivalent(&local, &exported), "lost field: {field}");
            exported.dependencies.push(dependency.clone());
            assert!(issues_equivalent(&local, &exported), "superset: {field}");
        }
    }

    fn local_comment(id: i64, body: &str) -> Comment {
        Comment {
            id,
            issue_id: "a-1".to_owned(),
            author: "local-agent".to_owned(),
            body: body.to_owned(),
            created_at: issue("a-1", "2026-01-01T00:00:00Z").created_at,
        }
    }

    #[test]
    fn comment_preservation_is_independent_of_scalar_age_and_dirty_tracking() {
        for equal_revision in [false, true] {
            for dirty in [false, true] {
                let mut local = issue("a-1", "2026-01-01T00:00:00Z");
                local.comments = vec![local_comment(1, "retained"), local_comment(2, "retained")];
                let mut jsonl = local.clone();
                jsonl.title = "peer's winning title".to_owned();
                if !equal_revision {
                    jsonl.updated_at += chrono::Duration::hours(1);
                }
                jsonl.comments = vec![local_comment(99, "retained")];
                assert_eq!(
                    classify(&local, Some(&jsonl), dirty),
                    Some((DbOnlyReason::CommentsAbsentFromJsonl, true)),
                    "one exported occurrence cannot supersede two local occurrences"
                );
                jsonl.comments.push(local_comment(100, "retained"));
                assert_eq!(
                    classify(&local, Some(&jsonl), dirty),
                    dirty.then_some((DbOnlyReason::UnflushedEdit, false)),
                    "complete history must preserve existing scalar precedence"
                );
                jsonl.comments.clear();
                jsonl.status = Status::Tombstone;
                assert_eq!(classify(&local, Some(&jsonl), dirty), None);
            }
        }
        let mut local = issue("a-1", "2026-02-01T00:00:00Z");
        local.comments.push(local_comment(1, "newer local"));
        let older = issue("a-1", "2026-01-01T00:00:00Z");
        for dirty in [false, true] {
            let reason = if dirty {
                DbOnlyReason::UnflushedEdit
            } else {
                DbOnlyReason::NewerThanJsonl
            };
            assert_eq!(classify(&local, Some(&older), dirty), Some((reason, true)));
        }
    }

    fn older_comment_fixture(dirty_mode: u8, equal_revision: bool) -> (Connection, Issue) {
        let (conn, _) = legacy_audit_fixture();
        for sql in [
            "CREATE TABLE comments (id INTEGER, issue_id TEXT, author TEXT, text TEXT, created_at TEXT)",
            "INSERT INTO comments VALUES (1, 'a-1', 'local-agent', 'local-only', '2026-01-01T00:00:00Z')",
            "ALTER TABLE issues ADD COLUMN notes TEXT",
            "ALTER TABLE issues ADD COLUMN source_repo_path TEXT",
            "UPDATE issues SET notes = 'superseded local notes', source_repo_path = '/local/workspace'",
        ] {
            conn.execute(sql).unwrap();
        }
        if dirty_mode != 0 {
            conn.execute("CREATE TABLE dirty_issues (issue_id TEXT)")
                .unwrap();
        }
        if dirty_mode == 2 {
            conn.execute("INSERT INTO dirty_issues VALUES ('a-1')")
                .unwrap();
        }
        let mut peer = issue("a-1", "2026-01-01T00:00:00Z");
        if !equal_revision {
            peer.updated_at += chrono::Duration::hours(1);
        }
        peer.title = "peer's completed issue".to_owned();
        peer.status = Status::Closed;
        peer.closed_at = Some(peer.updated_at);
        peer.close_reason = Some("completed by peer".to_owned());
        peer.notes = Some("peer's winning notes".to_owned());
        peer.labels = vec!["peer-label".to_owned()];
        peer.source_repo_path = Some("/other/clone".to_owned());
        peer.comments = vec![local_comment(1, "peer-only")];
        (conn, peer)
    }

    #[test]
    fn audit_projects_newer_issue_payload_for_comments_only_restoration() {
        for dirty_mode in 0..3 {
            for equal_revision in [false, true] {
                let (conn, mut peer) = older_comment_fixture(dirty_mode, equal_revision);
                peer.dependencies.push(
                    serde_json::from_value(serde_json::json!({
                        "issue_id": "a-1", "depends_on_id": "a-2", "type": "blocks",
                        "created_at": "2026-01-01T00:00:00Z", "metadata": "{\"peer\":true}"
                    }))
                    .unwrap(),
                );
                let bytes = serde_json::to_vec(&peer).unwrap();
                let audit = audit_connection(&conn, Some(std::io::Cursor::new(bytes))).unwrap();
                assert!(!audit.is_clean());
                assert!(!audit.automatic_heal_allowed());
                assert_eq!(audit.db_only.len(), 1);
                assert_eq!(
                    audit.db_only[0].reason,
                    DbOnlyReason::CommentsAbsentFromJsonl
                );
                assert!(audit.db_only[0].restorable);
                assert!(
                    refusal_error(&audit)
                        .to_string()
                        .contains("comments absent from JSONL")
                );
                for discard in [false, true] {
                    require_replay_payload_coverage(&audit, discard).unwrap();
                    assert!(planned_action(&audit, discard).starts_with("refuse rebuild:"));
                }
                let restore = &audit.restorable[0];
                assert_eq!(restore.issue.title, peer.title);
                assert_eq!(restore.issue.status, Status::Closed);
                assert_eq!(restore.issue.closed_at, peer.closed_at);
                assert_eq!(restore.issue.close_reason, peer.close_reason);
                assert_eq!(restore.issue.updated_at, peer.updated_at);
                assert_eq!(restore.issue.notes, peer.notes);
                assert_eq!(restore.labels.as_ref(), Some(&peer.labels));
                assert_eq!(restore.dependencies.as_ref(), Some(&peer.dependencies));
                assert_eq!(
                    restore.issue.source_repo_path.as_deref(),
                    Some("/local/workspace")
                );
                assert_eq!(
                    restore.comments.as_ref().unwrap(),
                    &[local_comment(1, "local-only")]
                );
                let original = conn
                    .query_row("SELECT title, status, notes FROM issues")
                    .unwrap();
                assert_eq!(
                    original.get(0).and_then(SqliteValue::as_text),
                    Some("title a-1")
                );
                assert_eq!(original.get(1).and_then(SqliteValue::as_text), Some("open"));
                assert_eq!(
                    original.get(2).and_then(SqliteValue::as_text),
                    Some("superseded local notes")
                );
            }
        }
    }

    #[test]
    fn comment_only_projection_refuses_unknown_history_or_a_different_issue() {
        let local = issue("a-1", "2026-01-01T00:00:00Z");
        let preserved = PreservedIssue {
            labels: Some(Vec::new()),
            dependencies: Some(Vec::new()),
            comments: None,
            issue: local.clone(),
        };
        assert!(comment_only_restore(preserved.clone(), &local).is_err());
        let mut known = preserved;
        known.comments = Some(vec![local_comment(1, "retained")]);
        let other = issue("a-2", "2026-01-01T00:00:00Z");
        assert!(comment_only_restore(known.clone(), &other).is_err());
        let mut reused = local.clone();
        reused.created_at += chrono::Duration::microseconds(1);
        assert!(matches!(
            comment_only_restore(known.clone(), &reused),
            Err(BeadsError::SyncConflict { .. })
        ));
        let mut deleted = local;
        deleted.status = Status::Tombstone;
        assert!(comment_only_restore(known, &deleted).is_err());
    }

    #[test]
    fn audit_refuses_reused_id_before_transferring_local_comments() {
        let (conn, mut peer) = older_comment_fixture(1, false);
        peer.created_at += chrono::Duration::seconds(1);
        let bytes = serde_json::to_vec(&peer).unwrap();
        let error = audit_connection(&conn, Some(std::io::Cursor::new(bytes))).unwrap_err();
        assert!(matches!(&error, BeadsError::SyncConflict { message }
            if message.contains("ID collision") && message.contains("rebuild was not started")));
        let original = conn.query_row("SELECT text FROM comments").unwrap();
        assert_eq!(
            original.get(0).and_then(SqliteValue::as_text),
            Some("local-only")
        );
    }

    #[test]
    fn audited_older_comments_are_retained_when_reconstruction_is_refused() {
        let (conn, peer) = older_comment_fixture(1, false);
        let scratch = tempfile::tempdir().unwrap();
        let path = scratch.path().join("issues.jsonl");
        let bytes = format!("{}\n", serde_json::to_string(&peer).unwrap());
        std::fs::write(&path, &bytes).unwrap();
        let source = crate::sync::capture_jsonl_source_snapshot(&path).unwrap();
        let audit = audit_connection(&conn, Some(source.reader())).unwrap();
        assert_eq!(
            audit.restorable.len(),
            1,
            "an older clean row still owns its comments"
        );
        assert_eq!(
            audit.db_only[0].reason,
            DbOnlyReason::CommentsAbsentFromJsonl
        );
        assert!(
            planned_action(&audit, false).starts_with("refuse rebuild:"),
            "decoding a comment restore plan does not establish crash-safe installation"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), bytes);
        let original = conn.query_row("SELECT text FROM comments").unwrap();
        assert_eq!(
            original.get(0).and_then(SqliteValue::as_text),
            Some("local-only")
        );
    }
}
