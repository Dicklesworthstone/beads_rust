//! Self-healing for tracker databases left behind on an older schema.
//!
//! Ordinary commands refuse a database whose schema predates this binary.
//! That refusal exists to protect data that lives only in the database: an
//! edit or creation that was never flushed to `issues.jsonl`. For the common
//! case — a stale cache whose every issue is already represented in the
//! git-tracked JSONL — the refusal protects nothing and blocks every command,
//! including the reviewed migration's own prerequisite (`plan` refuses the
//! unversioned schema-0 databases written by pre-versioning `br` and Go `bd`).
//!
//! This module separates the two cases:
//!
//! 1. [`audit_stale_database`] reads the old database schema-agnostically
//!    (plain `SELECT`s over whatever columns exist) and compares every issue
//!    row against the JSONL snapshot. An issue is *represented* when the JSONL
//!    carries it as a tombstone, carries equivalent content, or carries a
//!    copy at least as new while the database never marked the row dirty.
//!    Anything else — absent from the JSONL, an unflushed (dirty) edit that
//!    differs, a row newer than its JSONL copy, or a row that cannot be read —
//!    is database-only data.
//! 2. [`heal_stale_schema`] acts on the audit. With no database-only data it
//!    upgrades automatically: reviewed in-place migration for the supported
//!    source schemas (lossless, recovery bundle, undo), otherwise a rebuild
//!    from the audited JSONL snapshot with the old family retained in
//!    `.br_recovery`. With database-only data, automatic mode refuses and
//!    names the explicit `br doctor migrate-schema heal`, which does the same
//!    upgrade but re-adds the database-only issues as unflushed changes so the
//!    next flush exports them.

use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};
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
    /// The row could not be decoded, so its representation cannot be proven.
    Unreadable,
}

impl DbOnlyReason {
    const fn describe(self) -> &'static str {
        match self {
            Self::AbsentFromJsonl => "absent from JSONL",
            Self::UnflushedEdit => "unflushed edit",
            Self::NewerThanJsonl => "newer than JSONL",
            Self::Unreadable => "unreadable row",
        }
    }
}

/// One database-only issue found by the audit.
#[derive(Debug, Clone, Serialize)]
pub struct DbOnlyIssue {
    pub id: String,
    pub reason: DbOnlyReason,
    /// Whether the explicit heal re-adds this row after the rebuild. Unflushed
    /// edits that a newer JSONL edit already superseded stay in the backup
    /// only, matching the automatic-rebuild rule of GitHub #394.
    pub restorable: bool,
}

/// Result of comparing a stale database with the JSONL snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct StaleSchemaAudit {
    pub from_version: u32,
    pub to_version: u32,
    pub db_issue_count: usize,
    pub jsonl_issue_count: usize,
    pub db_only: Vec<DbOnlyIssue>,
    /// A reason the whole database cannot be audited (not a beads tracker,
    /// no JSONL, unparseable JSONL). Always blocks automatic healing.
    pub unprovable: Option<String>,
    #[serde(skip)]
    restorable: Vec<PreservedIssue>,
}

impl StaleSchemaAudit {
    /// True when every database issue is provably represented in the JSONL.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.unprovable.is_none() && self.db_only.is_empty()
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
}

/// How [`heal_stale_schema`] was invoked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealMode {
    /// Ordinary command startup: heal only when the audit is clean.
    Automatic,
    /// `br doctor migrate-schema heal`: heal regardless, re-adding the
    /// restorable database-only issues unless `discard_db_only`.
    Explicit { discard_db_only: bool },
}

/// What the heal did.
#[derive(Debug, Clone, Serialize)]
pub struct HealOutcome {
    pub from_version: u32,
    pub to_version: u32,
    /// `migrated` (reviewed in-place migration) or `rebuilt` (from JSONL).
    pub action: &'static str,
    /// Where the pre-heal database family was preserved.
    pub backup: String,
    /// Database-only issues re-added as unflushed changes.
    pub restored_db_only: Vec<String>,
    /// Database-only issues left only in the backup.
    pub backup_only_db_only: Vec<String>,
    /// Why a reviewed migration was not used, when the rebuild ran instead.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub migration_fallback_reason: Option<String>,
}

impl HealOutcome {
    /// One-line human notice for stderr.
    #[must_use]
    pub fn notice(&self) -> String {
        let how = if self.action == "migrated" {
            "migrated it in place"
        } else {
            "rebuilt it from the JSONL"
        };
        let mut line = format!(
            "br: tracker database was on schema {} (this br uses {}); {how} — backup at {}",
            self.from_version, self.to_version, self.backup
        );
        if !self.restored_db_only.is_empty() {
            line.push_str(&format!(
                "; re-added {} database-only issue(s) as unflushed changes",
                self.restored_db_only.len()
            ));
        }
        if !self.backup_only_db_only.is_empty() {
            line.push_str(&format!(
                "; {} superseded database-only edit(s) kept only in the backup",
                self.backup_only_db_only.len()
            ));
        }
        line
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
        format!(
            "{} issue(s) exist only in the database: {}",
            audit.db_only.len(),
            audit.db_only_summary()
        )
    });
    let what = if REVIEWED_MIGRATION_SOURCE_VERSIONS.contains(&audit.from_version) {
        "migrates the database in place without dropping any row, keeping a recovery bundle \
         and printing an undo command"
            .to_string()
    } else {
        "rebuilds the database from issues.jsonl, re-adds the database-only issues as \
         unflushed changes (exported by the next flush), and keeps the old database in \
         .beads/.br_recovery"
            .to_string()
    };
    BeadsError::WithContext {
        context: format!(
            "automatic schema upgrade refused because {why}. Run `{HEAL_COMMAND}`: it {what}. \
             `{HEAL_COMMAND} --dry-run` shows the audit without changing anything"
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
/// Returns an error only when the database cannot be opened or queried at
/// all; unprovable content is reported through [`StaleSchemaAudit`].
pub(crate) fn audit_stale_database(
    db_path: &Path,
    jsonl: Option<&JsonlSourceSnapshot>,
) -> Result<StaleSchemaAudit> {
    let conn = open_with_flags(
        db_path.to_string_lossy().as_ref(),
        OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(BeadsError::Database)?;
    let result = audit_connection(&conn, jsonl.map(JsonlSourceSnapshot::reader));
    let _ = conn.close();
    result
}

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

    let db_issues = read_legacy_issues(conn, &tables, &issue_columns)?;
    audit.db_issue_count = db_issues.len();
    let dirty: Option<HashSet<String>> = if tables.contains("dirty_issues") {
        Some(
            conn.query("SELECT issue_id FROM dirty_issues")?
                .iter()
                .filter_map(|row| row.get(0).and_then(SqliteValue::as_text).map(str::to_owned))
                .collect(),
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
        if let Some((reason, restorable)) = verdict {
            audit.db_only.push(DbOnlyIssue {
                id,
                reason,
                restorable,
            });
            if restorable {
                audit.restorable.push(preserved);
            }
        }
    }
    Ok(audit)
}

/// `None` when represented; otherwise the reason and whether the explicit
/// heal re-adds the row.
fn classify(db: &Issue, jsonl: Option<&Issue>, is_dirty: bool) -> Option<(DbOnlyReason, bool)> {
    let Some(jsonl) = jsonl else {
        return Some((DbOnlyReason::AbsentFromJsonl, true));
    };
    // A flushed deletion wins over any local state (import tombstone guard).
    if jsonl.status == Status::Tombstone || issues_equivalent(db, jsonl) {
        return None;
    }
    let db_newer = micros(db.updated_at) > micros(jsonl.updated_at);
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
/// hold nothing the JSONL copy lacks.
fn issues_equivalent(db: &Issue, jsonl: &Issue) -> bool {
    let scalar_fields_match = db.title == jsonl.title
        && norm(db.description.as_ref()) == norm(jsonl.description.as_ref())
        && norm(db.design.as_ref()) == norm(jsonl.design.as_ref())
        && norm(db.acceptance_criteria.as_ref()) == norm(jsonl.acceptance_criteria.as_ref())
        && norm(db.notes.as_ref()) == norm(jsonl.notes.as_ref())
        && db.status == jsonl.status
        && db.priority == jsonl.priority
        && db.issue_type == jsonl.issue_type
        && norm(db.assignee.as_ref()) == norm(jsonl.assignee.as_ref())
        && norm(db.owner.as_ref()) == norm(jsonl.owner.as_ref())
        && db.estimated_minutes == jsonl.estimated_minutes
        && norm(db.close_reason.as_ref()) == norm(jsonl.close_reason.as_ref())
        && norm(db.external_ref.as_ref()) == norm(jsonl.external_ref.as_ref())
        && micros(db.updated_at) == micros(jsonl.updated_at)
        && opt_micros(db.closed_at) == opt_micros(jsonl.closed_at)
        && opt_micros(db.due_at) == opt_micros(jsonl.due_at)
        && opt_micros(db.defer_until) == opt_micros(jsonl.defer_until);
    if !scalar_fields_match {
        return false;
    }
    let jsonl_labels: HashSet<&str> = jsonl.labels.iter().map(String::as_str).collect();
    let jsonl_deps: HashSet<(&str, String)> = jsonl
        .dependencies
        .iter()
        .map(|dep| (dep.depends_on_id.as_str(), dep.dep_type.to_string()))
        .collect();
    let jsonl_comments: HashSet<(&str, &str)> = jsonl
        .comments
        .iter()
        .map(|comment| (comment.author.as_str(), comment.body.as_str()))
        .collect();
    db.labels
        .iter()
        .all(|label| jsonl_labels.contains(label.as_str()))
        && db
            .dependencies
            .iter()
            .all(|dep| jsonl_deps.contains(&(dep.depends_on_id.as_str(), dep.dep_type.to_string())))
        && db.comments.iter().all(|comment| {
            jsonl_comments.contains(&(comment.author.as_str(), comment.body.as_str()))
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
        issues.insert(issue.id.clone(), issue);
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
) -> Result<Vec<LegacyIssue>> {
    let mut labels: HashMap<String, Vec<String>> = HashMap::new();
    if tables.contains("labels") {
        for row in conn.query("SELECT issue_id, label FROM labels")? {
            if let (Some(issue_id), Some(label)) = (
                row.get(0).and_then(SqliteValue::as_text),
                row.get(1).and_then(SqliteValue::as_text),
            ) {
                labels
                    .entry(issue_id.to_owned())
                    .or_default()
                    .push(label.to_owned());
            }
        }
    }
    let dependencies = read_related_rows(conn, tables, "dependencies")?;
    let comments = read_related_rows(conn, tables, "comments")?;

    let select = issue_columns
        .iter()
        .map(|column| quote_identifier(column))
        .collect::<Vec<_>>()
        .join(", ");
    let rows = conn.query(&format!("SELECT {select} FROM issues"))?;
    let mut issues = Vec::with_capacity(rows.len());
    for row in rows {
        let values: Vec<SqliteValue> = row.values().to_vec();
        let Some(id) = column_value(issue_columns, &values, "id")
            .and_then(SqliteValue::as_text)
            .map(str::to_owned)
        else {
            continue;
        };
        let decoded = decode_issue(
            issue_columns,
            &values,
            labels.remove(&id).unwrap_or_default(),
            dependencies.get(&id).map(Vec::as_slice).unwrap_or_default(),
            comments.get(&id).map(Vec::as_slice).unwrap_or_default(),
        );
        issues.push(LegacyIssue { id, decoded });
    }
    Ok(issues)
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
        let mut object = Map::new();
        for (column, value) in columns.iter().zip(row.values()) {
            if let Some(json) = sqlite_to_json(value) {
                object.insert(column.clone(), json);
            }
        }
        if let Some(Value::String(issue_id)) = object.get("issue_id").cloned() {
            related.entry(issue_id).or_default().push(object);
        }
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
/// required field cannot be represented.
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
                // An unparseable required timestamp makes the row unreadable;
                // an optional one is only a missing value.
                None if matches!(name, "created_at" | "updated_at") => return None,
                None => {}
            }
        } else if BOOL_FIELDS.contains(&name) {
            let flag = match value {
                SqliteValue::Integer(integer) => *integer != 0,
                other => matches!(other.as_text(), Some("1" | "true" | "TRUE")),
            };
            object.insert(name.to_owned(), Value::Bool(flag));
        } else if INT_FIELDS.contains(&name) {
            let integer = match value {
                SqliteValue::Integer(integer) => Some(*integer),
                other => other.as_text().and_then(|text| text.trim().parse().ok()),
            };
            if let Some(integer) = integer {
                object.insert(name.to_owned(), Value::from(integer));
            }
        } else if STRING_FIELDS.contains(&name)
            && let Some(text) = value.as_text()
            && (!text.is_empty() || matches!(name, "id" | "title"))
        {
            object.insert(name.to_owned(), Value::String(text.to_owned()));
        }
    }
    let created_at = object.get("created_at").cloned();
    let dependencies: Vec<Dependency> = dependencies
        .iter()
        .filter_map(|row| decode_related(row, created_at.as_ref()))
        .collect();
    let comments: Vec<Comment> = comments
        .iter()
        .filter_map(|row| {
            let mut row = row.clone();
            if let Some(Value::String(text)) = row.remove("text").or_else(|| row.remove("body")) {
                row.insert("text".to_owned(), Value::String(text));
            }
            decode_related(&row, created_at.as_ref())
        })
        .collect();
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
    let created_at = row
        .get("created_at")
        .and_then(|value| match value {
            Value::String(text) => legacy_timestamp_text(text),
            Value::Number(number) => number.as_i64().and_then(legacy_epoch),
            _ => None,
        })
        .map(|timestamp| Value::String(timestamp.to_rfc3339()))
        .or_else(|| fallback_created_at.cloned())?;
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
    Ok(conn
        .query("SELECT name FROM sqlite_master WHERE type='table'")?
        .iter()
        .filter_map(|row| row.get(0).and_then(SqliteValue::as_text).map(str::to_owned))
        .collect())
}

fn column_names(conn: &Connection, table: &str) -> Result<Vec<String>> {
    Ok(conn
        .query(&format!("PRAGMA table_info({})", quote_identifier(table)))?
        .iter()
        .filter_map(|row| row.get(1).and_then(SqliteValue::as_text).map(str::to_owned))
        .collect())
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

/// Audit the stale database and, when allowed, upgrade it.
///
/// The caller must hold the database-family write authority for the whole
/// call. The JSONL family authority is taken here so the audited snapshot is
/// exactly the one a rebuild imports.
///
/// # Errors
///
/// Returns an error when the audit cannot run or the upgrade fails; a failed
/// upgrade leaves the original database family in place (reviewed migration
/// restores it, and the rebuild restores it from its verified backup).
#[allow(clippy::too_many_lines)]
pub fn heal_stale_schema(ctx: &HealContext<'_>, mode: HealMode) -> Result<HealResult> {
    let startup = config::load_startup_config_with_paths(ctx.beads_dir, ctx.cli.db.as_ref())?;
    let paths = startup.paths.clone();
    ctx.write_authority.verify_database_authority()?;
    let Some(from_version) = stale_schema_version(&paths.db_path)? else {
        return Ok(HealResult::NotNeeded);
    };

    let (jsonl_authority, source) = capture_jsonl(&paths.jsonl_path, ctx.cli.lock_timeout)?;
    let mut audit = audit_stale_database(&paths.db_path, source.as_ref())?;
    audit.from_version = from_version;
    let discard_db_only = match mode {
        HealMode::Automatic if !audit.is_clean() => {
            return Ok(HealResult::Refused(Box::new(audit)));
        }
        HealMode::Automatic => false,
        HealMode::Explicit { discard_db_only } => discard_db_only,
    };

    let mut migration_fallback_reason = None;
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
                    to_version: audit.to_version,
                    action: "migrated",
                    backup: backup.display().to_string(),
                    restored_db_only: Vec::new(),
                    backup_only_db_only: Vec::new(),
                    migration_fallback_reason: None,
                }));
            }
            // A migration that did not commit leaves the old schema in place.
            // Rebuilding from JSONL is safe only when nothing lives solely in
            // the database or the operator explicitly asked for the heal.
            Err(error) if stale_schema_version(&paths.db_path)?.is_some() => {
                if !audit.is_clean() && matches!(mode, HealMode::Automatic) {
                    return Err(error);
                }
                migration_fallback_reason = Some(error.to_string());
            }
            Err(error) => return Err(error),
        }
    }

    let (Some(jsonl_authority), Some(source)) = (jsonl_authority, source) else {
        return Err(BeadsError::Config(format!(
            "cannot heal the schema-{from_version} database: there is no issues.jsonl to rebuild \
             from and no reviewed migration exists for schema {from_version}. Export it with the \
             br release that wrote it (`br sync --flush-only`), then rerun `{HEAL_COMMAND}`"
        )));
    };
    let mut merged = startup.merged_config.clone();
    merged.merge_from(&ctx.cli.as_layer());
    let allow_external_jsonl =
        config::implicit_external_jsonl_allowed(ctx.beads_dir, &paths.db_path, &paths.jsonl_path);
    let lock_timeout = ctx
        .cli
        .lock_timeout
        .or_else(|| config::lock_timeout_from_layer(&merged))
        .or(Some(30000));
    let (mut storage, _import, backups) =
        config::repair_database_from_jsonl_snapshot_under_write_authority(
            ctx.beads_dir,
            &paths.db_path,
            lock_timeout,
            &merged,
            false,
            allow_external_jsonl,
            &source,
            &jsonl_authority,
            ctx.write_authority,
        )?;
    let backup = backups
        .iter()
        .find(|verified| Path::new(&verified.original) == paths.db_path)
        .or_else(|| backups.first())
        .map(|verified| {
            Path::new(&verified.backup)
                .parent()
                .map_or_else(|| PathBuf::from(&verified.backup), Path::to_path_buf)
        })
        .unwrap_or_else(|| config::recovery_dir_for_db_path(&paths.db_path, ctx.beads_dir));

    let restore: &[PreservedIssue] = if discard_db_only {
        &[]
    } else {
        &audit.restorable
    };
    crate::sync::restore_dirty_issues_after_rebuild(&mut storage, restore)?;
    drop(storage);

    let restored: HashSet<&str> = restore
        .iter()
        .map(|entry| entry.issue.id.as_str())
        .collect();
    let (restored_db_only, backup_only_db_only): (Vec<String>, Vec<String>) = audit
        .db_only
        .iter()
        .map(|issue| issue.id.clone())
        .partition(|id| restored.contains(id.as_str()));
    Ok(HealResult::Healed(HealOutcome {
        from_version,
        to_version: audit.to_version,
        action: "rebuilt",
        backup: backup.display().to_string(),
        restored_db_only,
        backup_only_db_only,
        migration_fallback_reason,
    }))
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

/// Planned action for a stale database, as `--dry-run` reports it.
fn planned_action(audit: &StaleSchemaAudit, discard_db_only: bool) -> String {
    if REVIEWED_MIGRATION_SOURCE_VERSIONS.contains(&audit.from_version) {
        return format!(
            "reviewed in-place migration {} -> {} (every row kept; recovery bundle and undo \
             command retained)",
            audit.from_version, audit.to_version
        );
    }
    let restorable = audit
        .db_only
        .iter()
        .filter(|issue| issue.restorable)
        .count();
    let tail = if audit.db_only.is_empty() {
        String::new()
    } else if discard_db_only {
        format!(
            "; {} database-only issue(s) kept only in the backup",
            audit.db_only.len()
        )
    } else {
        format!(
            "; re-add {restorable} database-only issue(s) as unflushed changes{}",
            if restorable < audit.db_only.len() {
                format!(
                    " ({} superseded by newer JSONL edits stay only in the backup)",
                    audit.db_only.len() - restorable
                )
            } else {
                String::new()
            }
        )
    };
    format!(
        "rebuild schema {} -> {} from issues.jsonl, old database family moved to \
         .beads/.br_recovery{tail}",
        audit.from_version, audit.to_version
    )
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
        let Some(from_version) = stale_schema_version(&paths.db_path)? else {
            return emit_not_needed(args.json);
        };
        let (_jsonl_authority, source) = capture_jsonl(&paths.jsonl_path, cli.lock_timeout)?;
        let mut audit = audit_stale_database(&paths.db_path, source.as_ref())?;
        audit.from_version = from_version;
        let action = planned_action(&audit, args.discard_db_only);
        if args.json {
            let payload = serde_json::json!({
                "dry_run": true,
                "audit": audit,
                "automatic_heal_allowed": audit.is_clean(),
                "planned_action": action,
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&payload).map_err(BeadsError::Json)?
            );
        } else {
            println!(
                "Database schema {} (this br uses {}); {} issue(s) in the database, {} in the JSONL.",
                audit.from_version, audit.to_version, audit.db_issue_count, audit.jsonl_issue_count
            );
            if let Some(reason) = &audit.unprovable {
                println!("Audit incomplete: {reason}.");
            } else if audit.db_only.is_empty() {
                println!(
                    "Nothing exists only in the database; ordinary commands heal it automatically."
                );
            } else {
                println!(
                    "{} issue(s) exist only in the database:",
                    audit.db_only.len()
                );
                for issue in &audit.db_only {
                    println!("  {} ({})", issue.id, issue.reason.describe());
                }
            }
            println!("Heal would: {action}.");
        }
        return Ok(());
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
                if let Some(reason) = &outcome.migration_fallback_reason {
                    println!("(reviewed migration was not possible: {reason})");
                }
                for id in &outcome.restored_db_only {
                    println!("  re-added {id}");
                }
                for id in &outcome.backup_only_db_only {
                    println!("  kept only in backup: {id}");
                }
                if !outcome.restored_db_only.is_empty() {
                    println!("Run `br sync --flush-only` to export the re-added issues.");
                }
            }
            Ok(())
        }
    }
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
}
